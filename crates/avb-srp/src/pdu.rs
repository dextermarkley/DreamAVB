//! MRPDU vector codec (802.1Q-2018 §10.8, MSRP specifics §35.2.2).
//!
//! ```text
//! MRPDU     = ProtocolVersion(1) Message* EndMark(2)
//! Message   = AttributeType(1) AttributeLength(1) [AttributeListLength(2), MSRP only]
//!             VectorAttribute* EndMark(2)
//! Vector    = VectorHeader(2: LeaveAllEvent 3 bits | NumberOfValues 13 bits)
//!             FirstValue(AttributeLength) ThreePackedEvents(⌈n/3⌉)
//!             [FourPackedEvents(⌈n/4⌉), MSRP Listener only]
//! ```
//! We always encode NumberOfValues = 1 (design §8.1). The decoder accepts
//! NumberOfValues up to [`MAX_VALUES`] with the FirstValue + n increments
//! (see `attr::decode_value`). Validation (design §13.2): ProtocolVersion 0
//! accepted, higher versions parse the known types (forward compatibility,
//! 802.1Q-2018 §10.8.3.? *(verify)*); AttributeLength must equal the type's
//! defined length; NumberOfValues ≤ 512; the attribute-list EndMark is
//! required. A violation drops the whole PDU.

use avb_core::DropReason;

use crate::attr::{self, App, Attribute, ListenerDecl, MSRP_LISTENER};
use crate::wire::{Rd, Wr};

pub const PROTOCOL_VERSION: u8 = 0;
/// Per-vector NumberOfValues bound (design §13.2).
pub const MAX_VALUES: u16 = 512;
/// Largest MRPDU we build (Ethernet payload).
pub const MAX_PDU: usize = 1500;

/// AttributeEvent (802.1Q-2018 §10.8.2.10 *(verify)*): the ThreePackedEvents
/// digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AttrEvent {
    New = 0,
    JoinIn = 1,
    In = 2,
    JoinMt = 3,
    Mt = 4,
    Lv = 5,
}

impl AttrEvent {
    pub const fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::New,
            1 => Self::JoinIn,
            2 => Self::In,
            3 => Self::JoinMt,
            4 => Self::Mt,
            5 => Self::Lv,
            _ => return None,
        })
    }
}

/// One attribute event out of a received PDU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxEvent {
    pub attr: Attribute,
    pub event: AttrEvent,
}

/// A decoded MRPDU.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pdu {
    pub protocol_version: u8,
    /// Attribute types that carried a LeaveAll, in order.
    pub leave_all: Vec<u8>,
    pub events: Vec<RxEvent>,
    /// Messages of an unknown attribute type that were skipped.
    pub unknown_types: u32,
}

pub fn decode(app: App, p: &[u8]) -> Result<Pdu, DropReason> {
    let mut r = Rd::new(p);
    let mut pdu = Pdu {
        protocol_version: r.u8().ok_or(DropReason::TooShort)?,
        ..Pdu::default()
    };
    loop {
        match r.peek_u16() {
            // End of PDU: EndMark, or the end of the data.
            None | Some(0) => break,
            Some(_) => {}
        }
        let attr_type = r.u8().ok_or(DropReason::TooShort)?;
        let attr_len = usize::from(r.u8().ok_or(DropReason::TooShort)?);
        let known = attr::attr_len(app, attr_type);
        if let Some(l) = known {
            if l != attr_len {
                return Err(DropReason::Malformed);
            }
        }
        match app {
            App::Msrp => {
                let list_len = usize::from(r.u16().ok_or(DropReason::TooShort)?);
                let list = r.take(list_len).ok_or(DropReason::TooShort)?;
                if known.is_none() {
                    pdu.unknown_types += 1;
                    continue;
                }
                let mut lr = Rd::new(list);
                decode_vectors(app, attr_type, attr_len, &mut lr, &mut pdu)?;
            }
            App::Mvrp => {
                if known.is_none() {
                    // No list length to skip by: the rest is unparseable.
                    pdu.unknown_types += 1;
                    break;
                }
                decode_vectors(app, attr_type, attr_len, &mut r, &mut pdu)?;
            }
        }
    }
    Ok(pdu)
}

fn decode_vectors(
    app: App,
    attr_type: u8,
    attr_len: usize,
    r: &mut Rd<'_>,
    pdu: &mut Pdu,
) -> Result<(), DropReason> {
    loop {
        let hdr = r.u16().ok_or(DropReason::Malformed)?; // EndMark required
        if hdr == 0 {
            return Ok(());
        }
        let leave_all = hdr >> 13;
        let n = hdr & 0x1FFF;
        if n > MAX_VALUES || leave_all > 1 {
            return Err(DropReason::Malformed);
        }
        if leave_all == 1 && !pdu.leave_all.contains(&attr_type) {
            pdu.leave_all.push(attr_type);
        }
        let fv = r.take(attr_len).ok_or(DropReason::TooShort)?;
        let three = r
            .take(usize::from(n).div_ceil(3))
            .ok_or(DropReason::TooShort)?;
        let four = if app == App::Msrp && attr_type == MSRP_LISTENER {
            Some(
                r.take(usize::from(n).div_ceil(4))
                    .ok_or(DropReason::TooShort)?,
            )
        } else {
            None
        };
        for i in 0..n {
            let byte = *three.get(usize::from(i / 3)).ok_or(DropReason::TooShort)?;
            if byte >= 216 {
                return Err(DropReason::Malformed);
            }
            let digit = match i % 3 {
                0 => byte / 36,
                1 => (byte / 6) % 6,
                _ => byte % 6,
            };
            let event = AttrEvent::from_u8(digit).ok_or(DropReason::Malformed)?;
            let decl = match four {
                Some(f) => {
                    let b = *f.get(usize::from(i / 4)).ok_or(DropReason::TooShort)?;
                    ListenerDecl::from_u8(b >> (6 - 2 * (i % 4)))
                }
                None => ListenerDecl::Ignore,
            };
            let attr =
                attr::decode_value(app, attr_type, fv, i, decl).ok_or(DropReason::Malformed)?;
            pdu.events.push(RxEvent { attr, event });
        }
    }
}

/// Builds one MRPDU of single-value vectors, grouped per attribute type in
/// ascending order. LeaveAll (if set) goes on the first vector of every
/// message.
#[derive(Clone, Debug)]
pub struct PduBuilder {
    app: App,
    leave_all: bool,
    /// (attr_type, encoded vectors) ascending by type.
    msgs: Vec<(u8, Vec<u8>)>,
}

impl PduBuilder {
    pub fn new(app: App, leave_all: bool) -> Self {
        Self {
            app,
            leave_all,
            msgs: Vec::new(),
        }
    }

    fn size(&self) -> usize {
        let per_msg = if self.app == App::Msrp { 4 } else { 2 };
        1 + 2
            + self
                .msgs
                .iter()
                .map(|(_, v)| per_msg + v.len() + 2)
                .sum::<usize>()
    }

    pub fn is_empty(&self) -> bool {
        self.msgs.is_empty()
    }

    /// Append one vector. `decl` is the Listener FourPackedEvent (Ignore for
    /// non-declaration sends). Returns false (nothing added) if the PDU would
    /// exceed [`MAX_PDU`].
    pub fn push(&mut self, attr: &Attribute, event: AttrEvent, decl: ListenerDecl) -> bool {
        let t = attr.attr_type();
        let Some(len) = attr::attr_len(self.app, t) else {
            return false;
        };
        let four = self.app == App::Msrp && t == MSRP_LISTENER;
        let vlen = 2 + len + 1 + usize::from(four);
        let new_msg = !self.msgs.iter().any(|(mt, _)| *mt == t);
        let extra = vlen
            + if new_msg {
                if self.app == App::Msrp {
                    6
                } else {
                    4
                }
            } else {
                0
            };
        if self.size() + extra > MAX_PDU {
            return false;
        }
        let first = new_msg;
        let mut buf = [0u8; 2 + attr::TALKER_FAILED_LEN + 2];
        let mut w = Wr::new(&mut buf);
        let la = u16::from(self.leave_all && first);
        w.u16((la << 13) | 1);
        attr.write_first_value(&mut w);
        w.u8(event as u8 * 36);
        if four {
            w.u8((decl as u8) << 6);
        }
        debug_assert!(!w.overflowed() && w.pos() == vlen);
        let n = w.pos();
        let Some(bytes) = buf.get(..n) else {
            return false;
        };
        match self.msgs.iter_mut().find(|(mt, _)| *mt == t) {
            Some((_, v)) => v.extend_from_slice(bytes),
            None => {
                self.msgs.push((t, bytes.to_vec()));
                self.msgs.sort_by_key(|(mt, _)| *mt);
            }
        }
        true
    }

    /// The encoded MRPDU (Ethernet payload).
    pub fn finish(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(self.size());
        v.push(PROTOCOL_VERSION);
        for (t, vecs) in &self.msgs {
            let len = attr::attr_len(self.app, *t).unwrap_or(0);
            v.push(*t);
            v.push(len as u8);
            if self.app == App::Msrp {
                // AttributeListLength counts the vectors and the list EndMark
                // (802.1Q-2018 §35.2.2.6 *(verify)*).
                v.extend_from_slice(&((vecs.len() + 2) as u16).to_be_bytes());
            }
            v.extend_from_slice(vecs);
            v.extend_from_slice(&[0, 0]);
        }
        v.extend_from_slice(&[0, 0]);
        v
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::attr::*;
    use avb_core::{MacAddr, StreamId};

    pub fn ta() -> TalkerAdvertise {
        TalkerAdvertise {
            stream_id: StreamId(0x000a_9203_3ba9_0000),
            dest_mac: MacAddr([0x91, 0xE0, 0xF0, 0x00, 0xFE, 0x00]),
            vlan_id: 2,
            max_frame_size: 224,
            max_interval_frames: 1,
            priority: 3,
            rank: true,
            accumulated_latency_ns: 125_000,
        }
    }

    fn one(app: App, la: bool, a: &Attribute, e: AttrEvent, d: ListenerDecl) -> Vec<u8> {
        let mut b = PduBuilder::new(app, la);
        assert!(b.push(a, e, d));
        b.finish()
    }

    #[test]
    fn domain_vector() {
        let a = Attribute::Domain(DEFAULT_DOMAIN);
        let p = one(
            App::Msrp,
            false,
            &a,
            AttrEvent::JoinMt,
            ListenerDecl::Ignore,
        );
        #[rustfmt::skip]
        let want = [
            0x00,                   // ProtocolVersion
            0x04, 0x04, 0x00, 0x09, // Domain, len 4, list 9
            0x00, 0x01,             // VectorHeader: no LA, 1 value
            0x06, 0x03, 0x00, 0x02, // SRclassID 6, priority 3, VID 2
            0x6C,                   // JoinMt (3) × 36
            0x00, 0x00,             // list EndMark
            0x00, 0x00,             // PDU EndMark
        ];
        assert_eq!(p, want);
        let d = decode(App::Msrp, &p).unwrap();
        assert_eq!(
            d.events,
            [RxEvent {
                attr: a,
                event: AttrEvent::JoinMt
            }]
        );
        assert!(d.leave_all.is_empty());
    }

    #[test]
    fn talker_advertise_vector() {
        let a = Attribute::Talker(Talker::Advertise(ta()));
        let p = one(App::Msrp, true, &a, AttrEvent::JoinIn, ListenerDecl::Ignore);
        #[rustfmt::skip]
        let want = [
            0x00,
            0x01, 0x19, 0x00, 0x1E,                         // TA, len 25, list 30
            0x20, 0x01,                                     // LeaveAll + 1 value
            0x00, 0x0a, 0x92, 0x03, 0x3b, 0xa9, 0x00, 0x00, // StreamID
            0x91, 0xE0, 0xF0, 0x00, 0xFE, 0x00,             // dest MAC
            0x00, 0x02,                                     // VLAN 2
            0x00, 0xE0, 0x00, 0x01,                         // MaxFrameSize 224, MaxIntervalFrames 1
            0x70,                                           // priority 3, rank 1
            0x00, 0x01, 0xE8, 0x48,                         // accumulated latency 125 000 ns
            0x24,                                           // JoinIn (1) × 36
            0x00, 0x00,
            0x00, 0x00,
        ];
        assert_eq!(p, want);
        let d = decode(App::Msrp, &p).unwrap();
        assert_eq!(d.events[0].attr, a);
        assert_eq!(d.leave_all, [MSRP_TALKER_ADVERTISE]);
    }

    #[test]
    fn talker_failed_vector() {
        let a = Attribute::Talker(Talker::Failed(TalkerFailed {
            advertise: ta(),
            failure_bridge_id: 0x8000_000a_9203_3ba9,
            failure_code: 1,
        }));
        let p = one(App::Msrp, false, &a, AttrEvent::New, ListenerDecl::Ignore);
        assert_eq!(&p[..5], &[0x00, 0x02, 0x22, 0x00, 39]);
        assert_eq!(
            &p[32..43],
            &[0x80, 0x00, 0x00, 0x0a, 0x92, 0x03, 0x3b, 0xa9, 0x01, 0x00, 0x00]
        );
        assert_eq!(p.len(), 1 + 4 + 2 + 34 + 1 + 2 + 2);
        let d = decode(App::Msrp, &p).unwrap();
        assert_eq!(
            d.events,
            [RxEvent {
                attr: a,
                event: AttrEvent::New
            }]
        );
    }

    #[test]
    fn listener_four_packed_vector() {
        let sid = StreamId(0x000a_9203_3ba9_0001);
        for (decl, fp) in [
            (ListenerDecl::Ready, 0x80u8),
            (ListenerDecl::AskingFailed, 0x40),
        ] {
            let a = Attribute::Listener(Listener {
                stream_id: sid,
                decl,
            });
            let p = one(App::Msrp, false, &a, AttrEvent::JoinIn, decl);
            #[rustfmt::skip]
            let want = [
                0x00,
                0x03, 0x08, 0x00, 0x0E,                         // Listener, len 8, list 14
                0x00, 0x01,
                0x00, 0x0a, 0x92, 0x03, 0x3b, 0xa9, 0x00, 0x01, // StreamID
                0x24,                                           // JoinIn
                fp,                                             // FourPacked: decl × 64
                0x00, 0x00,
                0x00, 0x00,
            ];
            assert_eq!(p, want);
            assert_eq!(decode(App::Msrp, &p).unwrap().events[0].attr, a);
        }
    }

    #[test]
    fn mvrp_vid_vector() {
        let a = Attribute::Vid { vid: 2 };
        let p = one(
            App::Mvrp,
            false,
            &a,
            AttrEvent::JoinIn,
            ListenerDecl::Ignore,
        );
        // No AttributeListLength in MVRP.
        assert_eq!(
            p,
            [0x00, 0x01, 0x02, 0x00, 0x01, 0x00, 0x02, 0x24, 0x00, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            decode(App::Mvrp, &p).unwrap().events,
            [RxEvent {
                attr: a,
                event: AttrEvent::JoinIn
            }]
        );
    }

    /// NumberOfValues > 1 with FirstValue + n, events three- and four-packed.
    #[test]
    fn decode_multiple_values() {
        // 5 Listener values from StreamID …:0010: JoinIn, JoinMt, Lv, New, In;
        // declarations Ready, AskingFailed, Ignore, ReadyFailed, Ready.
        let three0 = 36 + 3 * 6 + 5; // JoinIn, JoinMt, Lv
        let three1 = 12; // New (0·36), In (2·6), pad 0
        let four0 = 0b10_01_00_11; // Ready, AskingFailed, Ignore, ReadyFailed
        let four1 = 2 << 6;
        #[rustfmt::skip]
        let p = [
            0x00,
            0x03, 0x08, 0x00, 16,
            0x00, 0x05,
            0x00, 0x0a, 0x92, 0x03, 0x3b, 0xa9, 0x00, 0x10,
            three0, three1,
            four0, four1,
            0x00, 0x00,
            0x00, 0x00,
        ];
        let d = decode(App::Msrp, &p).unwrap();
        let got: Vec<_> = d
            .events
            .iter()
            .map(|e| match e.attr {
                Attribute::Listener(l) => (l.stream_id.0 & 0xFFFF, l.decl, e.event),
                _ => panic!(),
            })
            .collect();
        use AttrEvent::*;
        use ListenerDecl::*;
        assert_eq!(
            got,
            [
                (0x10, Ready, JoinIn),
                (0x11, AskingFailed, JoinMt),
                (0x12, Ignore, Lv),
                (0x13, ReadyFailed, New),
                (0x14, Ready, In)
            ]
        );
        // MVRP: 4 VIDs from 100, and a Talker Advertise pair (StreamID and
        // destination MAC both increment).
        let p = [
            0x00, 0x01, 0x02, 0x00, 0x04, 0x00, 100, 36, 36, 0x00, 0x00, 0x00, 0x00,
        ];
        let d = decode(App::Mvrp, &p).unwrap();
        let vids: Vec<_> = d.events.iter().map(|e| e.attr).collect();
        assert_eq!(vids, [100, 101, 102, 103].map(|vid| Attribute::Vid { vid }),);
        assert_eq!(d.events[3].event, AttrEvent::JoinIn);
        let mut b = PduBuilder::new(App::Msrp, false);
        b.push(
            &Attribute::Talker(Talker::Advertise(ta())),
            AttrEvent::JoinIn,
            ListenerDecl::Ignore,
        );
        let mut p = b.finish();
        p[5..7].copy_from_slice(&[0x00, 0x02]); // NumberOfValues 2 (both JoinIn: digit 0 = 1, digit 1 = 1)
        p[3..5].copy_from_slice(&[0x00, 0x1E]);
        p[32] = 36 + 6;
        let d = decode(App::Msrp, &p).unwrap();
        let Attribute::Talker(t) = d.events[1].attr else {
            panic!()
        };
        assert_eq!(t.stream_id(), StreamId(0x000a_9203_3ba9_0001));
        assert_eq!(
            t.advertise().dest_mac,
            MacAddr([0x91, 0xE0, 0xF0, 0x00, 0xFE, 0x01])
        );
        assert_eq!(t.advertise().vlan_id, 2);
        // Domain: SRclassID and priority increment.
        let p = [
            0x00,
            0x04,
            0x04,
            0x00,
            9,
            0x00,
            0x02,
            5,
            2,
            0x00,
            0x02,
            36 + 6,
            0,
            0,
            0,
            0,
        ];
        let d = decode(App::Msrp, &p).unwrap();
        assert_eq!(
            d.events[1].attr,
            Attribute::Domain(Domain {
                class_id: 6,
                priority: 3,
                vid: 2
            })
        );
    }

    #[test]
    fn decode_rejections() {
        let good = one(
            App::Msrp,
            false,
            &Attribute::Domain(DEFAULT_DOMAIN),
            AttrEvent::JoinIn,
            ListenerDecl::Ignore,
        );
        assert!(decode(App::Msrp, &good).is_ok());
        // AttributeLength wrong for the type.
        let mut p = good.clone();
        p[2] = 5;
        assert_eq!(decode(App::Msrp, &p), Err(DropReason::Malformed));
        // NumberOfValues 513.
        let mut p = good.clone();
        p[5..7].copy_from_slice(&513u16.to_be_bytes());
        assert_eq!(decode(App::Msrp, &p), Err(DropReason::Malformed));
        // List EndMark missing (list length cut before it).
        let mut p = good.clone();
        p[3..5].copy_from_slice(&7u16.to_be_bytes());
        assert_eq!(decode(App::Msrp, &p), Err(DropReason::Malformed));
        // ThreePacked byte ≥ 216 and event digit 6+ (≥ 6·36).
        let mut p = good.clone();
        p[11] = 216;
        assert_eq!(decode(App::Msrp, &p), Err(DropReason::Malformed));
        // Truncated.
        assert_eq!(decode(App::Msrp, &good[..9]), Err(DropReason::TooShort));
        assert_eq!(decode(App::Msrp, &[]), Err(DropReason::TooShort));
        // Unknown MSRP type skipped by its list length; ProtocolVersion 1 accepted.
        let mut p = vec![0x01, 0x09, 0x02, 0x00, 0x04, 0xAA, 0xBB, 0x00, 0x00];
        p.extend_from_slice(&good[1..]);
        let d = decode(App::Msrp, &p).unwrap();
        assert_eq!(
            (d.protocol_version, d.unknown_types, d.events.len()),
            (1, 1, 1)
        );
        // PDU ending without the final EndMark (end of data) is accepted.
        assert!(decode(App::Msrp, &good[..good.len() - 2]).is_ok());
    }

    #[test]
    fn builder_groups_types_and_marks_leave_all_once() {
        let mut b = PduBuilder::new(App::Msrp, true);
        let l = |n| {
            Attribute::Listener(Listener {
                stream_id: StreamId(n),
                decl: ListenerDecl::Ready,
            })
        };
        assert!(b.push(&l(1), AttrEvent::JoinIn, ListenerDecl::Ready));
        assert!(b.push(
            &Attribute::Domain(DEFAULT_DOMAIN),
            AttrEvent::JoinIn,
            ListenerDecl::Ignore
        ));
        assert!(b.push(&l(2), AttrEvent::Lv, ListenerDecl::Ignore));
        let p = b.finish();
        let d = decode(App::Msrp, &p).unwrap();
        assert_eq!(d.leave_all, [MSRP_LISTENER, MSRP_DOMAIN]);
        assert_eq!(d.events.len(), 3);
        // Domain (type 4) message after Listener (type 3).
        assert_eq!(p[1], MSRP_LISTENER);
        // Only the first vector of each message carries LeaveAll.
        assert_eq!(p[5] >> 5, 1);
        assert_eq!(p[5 + 12] >> 5, 0);
        // Size cap.
        let mut b = PduBuilder::new(App::Msrp, false);
        let mut n = 0;
        while b.push(&l(n), AttrEvent::JoinIn, ListenerDecl::Ready) {
            n += 1;
        }
        assert!(b.finish().len() <= MAX_PDU);
        assert_eq!(n as usize, (MAX_PDU - 1 - 2 - 6) / 12);
    }
}
