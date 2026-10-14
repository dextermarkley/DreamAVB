// SPDX-License-Identifier: MIT OR Apache-2.0

//! MAC Address Acquisition Protocol (IEEE 1722-2016 Annex B).
//!
//! Milan requires this, and not as a formality. The Milan Baseline
//! Interoperability Specification §6.5.1 says a PAAD "shall implement
//! Multicast Address Allocation Protocol (MAAP)" and "shall use destination
//! MAC addresses allocated with MAAP for its output streams" — and §6.3.1 then
//! makes allocation a precondition for a Talker attribute being usable at all:
//!
//! > This parameter is valid when both of the two following conditions are
//! > satisfied:
//! > 1) The Destination MAC Address has been allocated by the MAAP protocol and
//! >    no conflict is being reported, and
//! > 2) A PROBE_TX_COMMAND message targeted to this STREAM_OUTPUT has been
//! >    received during the last 15 seconds, or the PAAD is registering a
//! >    Listener attribute that matches the Stream ID for this STREAM_OUTPUT.
//!
//! So picking an address out of the pool — which is all we did before this
//! module — leaves every Talker attribute we declare formally invalid, however
//! correct the address looks. A Milan listener is then entitled to reserve the
//! path and decline to media-lock, which is exactly what the PreSonus 16R did:
//! probing COMPLETED, ACMP SUCCESS, `FRAMES_RX` climbing at 8000/s, every
//! error counter zero, `MEDIA_LOCKED` zero.
//!
//! # The wire layout is transcribed from the 16R, not from the spec
//!
//! A captured `MAAP_ANNOUNCE` from the live 16R (`00:0a:92:03:3b:a9`, Milan
//! certified, so its own stream addresses must have been allocated this way):
//!
//! ```text
//! dst 91:e0:f0:00:ff:00
//! fe 03 08 10  00 00 00 00 00 00 00 00  91 e0 f0 00 d5 48  00 09  00 00 00 00 00 00  00 00
//! |  |  |      |                        |                  |      |                  |
//! |  |  |      stream_id, sv=0 so 0     requested_start    count  conflict_start     conflict_count
//! |  |  status(5)=0 | control_data_length(11)=16
//! |  sv(1)=0 version(3)=0 message_type(4)=3 ANNOUNCE
//! subtype 0xFE
//! ```
//!
//! Two independent facts confirm the field offsets rather than assuming them:
//! `requested_count` reads 9, which is exactly the 16R's nine STREAM_OUTPUTs
//! (eight audio plus one CRF), and `control_data_length` reads 16, which is
//! exactly the four fields that follow the common header (6 + 2 + 6 + 2). A
//! layout read one byte out would make both of those nonsense.
//!
//! Note `sv = 0`: MAAP does not use the common header's `stream_id`, and the
//! eight bytes are transmitted as zero.
//!
//! # Sans-IO
//!
//! No sockets and no clock: [`Maap::tick`] is handed the time and returns what
//! to send. Same shape as the gPTP port, for the same reason — the state
//! machine is then testable without hardware, which is the only way to exercise
//! a conflict (the live network has no second claimant to produce one).

use avb_core::MacAddr;

use crate::common::subtype;

/// The MAAP multicast destination, from the captured frame's own `dst`
/// (IEEE 1722-2016 Annex B).
pub const MAAP_MULTICAST: MacAddr = MacAddr([0x91, 0xE0, 0xF0, 0x00, 0xFF, 0x00]);

/// MAAP `message_type` values. `ANNOUNCE` is confirmed as 3 by the captured
/// frame; `PROBE` and `DEFEND` are the two other Annex B values.
pub mod msg {
    pub const PROBE: u8 = 1;
    pub const DEFEND: u8 = 2;
    pub const ANNOUNCE: u8 = 3;
}

/// Common AVTP control header length, before MAAP's own fields.
const HEADER_LEN: usize = 12;
/// MAAP's four fields: start(6) + count(2) + conflict_start(6) +
/// conflict_count(2). Matches the captured `maap_data_length` of 16.
const DATA_LEN: usize = 16;
/// Total MAAP PDU length (the AVTP payload, after the EtherType).
pub const MAAP_PDU_LEN: usize = HEADER_LEN + DATA_LEN;

/// MAAP's own version field, which occupies the five bits where the generic
/// AVTP control header carries `status`.
///
/// Read off the 16R, not assumed: it transmits bytes 2..4 as `08 10`. Decoding
/// that word as `status(5) | data_length(11)` gives status = 1, which is not a
/// defined status value; decoding it as `maap_version(5) | maap_data_length(11)`
/// gives version = 1 and length = 16, and 16 is exactly the four fields that
/// follow. Encoding it as a zero status produced `00 10` and failed the
/// byte-for-byte re-encode against the captured frame, which is how this was
/// caught.
const MAAP_VERSION: u8 = 1;

/// A decoded MAAP PDU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaapPdu {
    pub message_type: u8,
    pub requested_start: MacAddr,
    pub requested_count: u16,
    pub conflict_start: MacAddr,
    pub conflict_count: u16,
}

impl MaapPdu {
    /// Decode the AVTP payload (after the EtherType). `None` when the buffer is
    /// short or the subtype is not MAAP.
    ///
    /// A trailing-padded frame is accepted: the captured announce arrived as 50
    /// payload bytes for 28 bytes of PDU, because the Ethernet minimum pads it.
    /// Rejecting on length equality would discard every real frame.
    pub fn decode(pdu: &[u8]) -> Option<Self> {
        let b = pdu.get(..MAAP_PDU_LEN)?;
        if *b.first()? != subtype::MAAP {
            return None;
        }
        Some(Self {
            message_type: b.get(1)? & 0x0F,
            requested_start: MacAddr::from_slice(b.get(12..18)?)?,
            requested_count: u16::from_be_bytes([*b.get(18)?, *b.get(19)?]),
            conflict_start: MacAddr::from_slice(b.get(20..26)?)?,
            conflict_count: u16::from_be_bytes([*b.get(26)?, *b.get(27)?]),
        })
    }

    /// Encode into a fixed-size buffer, byte-for-byte in the captured layout.
    pub fn encode(&self) -> [u8; MAAP_PDU_LEN] {
        let mut b = [0u8; MAAP_PDU_LEN];
        b[0] = subtype::MAAP;
        // sv = 0 and version = 0, as the 16R transmits: MAAP does not use the
        // common header's stream_id, and bytes 4..12 stay zero.
        b[1] = self.message_type & 0x0F;
        // maap_version(5) = 1 | maap_data_length(11) = 16, exactly the `08 10`
        // the 16R transmits.
        let cdl = DATA_LEN as u16;
        b[2] = (MAAP_VERSION << 3) | ((cdl >> 8) & 0x07) as u8;
        b[3] = (cdl & 0xFF) as u8;
        b[12..18].copy_from_slice(&self.requested_start.octets());
        b[18..20].copy_from_slice(&self.requested_count.to_be_bytes());
        b[20..26].copy_from_slice(&self.conflict_start.octets());
        b[26..28].copy_from_slice(&self.conflict_count.to_be_bytes());
        b
    }

    /// Whether this PDU's requested block overlaps `[start, start+count)`.
    ///
    /// Annex B's own note warns against comparing only the low three bytes:
    /// other MAAP users need not share the `91:E0:F0` prefix, so the whole
    /// 48-bit range is compared.
    pub fn overlaps(&self, start: MacAddr, count: u16) -> bool {
        ranges_overlap(
            self.requested_start,
            self.requested_count,
            start,
            count.max(1),
        )
    }
}

/// Whether two 48-bit address ranges intersect.
fn ranges_overlap(a_start: MacAddr, a_count: u16, b_start: MacAddr, b_count: u16) -> bool {
    let a0 = a_start.to_u64();
    let a1 = a0.saturating_add(u64::from(a_count.max(1)));
    let b0 = b_start.to_u64();
    let b1 = b0.saturating_add(u64::from(b_count.max(1)));
    a0 < b1 && b0 < a1
}

/// What the state machine wants the caller to put on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Send this PDU to [`MAAP_MULTICAST`].
    Send(MaapPdu),
}

/// Where an allocation currently stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Nothing claimed yet.
    Idle,
    /// Probing a candidate block; `sent` probes so far.
    Probing { sent: u8 },
    /// The block is ours: announce periodically and defend it.
    Acquired,
}

/// Annex B timing. Probes go out a few times before a block is taken, and the
/// holder re-announces periodically so a later joiner learns the block is
/// occupied.
const PROBE_RETRANSMITS: u8 = 3;
const PROBE_INTERVAL_NS: u64 = 500_000_000;
/// The 16R emitted one announce in a 25 s observation window, consistent with
/// this interval.
const ANNOUNCE_INTERVAL_NS: u64 = 30_000_000_000;

/// MAAP for one contiguous block of stream destination addresses.
#[derive(Clone, Debug)]
pub struct Maap {
    state: State,
    start: MacAddr,
    count: u16,
    /// When the next probe or announce is due.
    next_due_ns: u64,
    /// Candidate index, bumped on every conflict so a retry lands elsewhere.
    attempt: u32,
    /// Conflicts observed, so a caller can report them rather than silently
    /// moving.
    pub conflicts: u32,
}

impl Maap {
    /// Start probing for `count` addresses beginning at `start`.
    pub fn new(start: MacAddr, count: u16, now_ns: u64) -> Self {
        Self {
            state: State::Probing { sent: 0 },
            start,
            count: count.max(1),
            next_due_ns: now_ns,
            attempt: 0,
            conflicts: 0,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// The block being probed or held. Only valid to transmit to once
    /// [`Maap::acquired`] is true.
    pub fn block(&self) -> (MacAddr, u16) {
        (self.start, self.count)
    }

    /// Whether the block is allocated and free of reported conflict — Milan
    /// §6.3.1 condition 1, and therefore whether a Talker attribute declared
    /// against this address is valid.
    pub fn acquired(&self) -> bool {
        matches!(self.state, State::Acquired)
    }

    /// Drive timers. Returns a PDU to send when one is due.
    pub fn tick(&mut self, now_ns: u64) -> Option<Action> {
        if now_ns < self.next_due_ns {
            return None;
        }
        match self.state {
            State::Idle => None,
            State::Probing { sent } => {
                if sent >= PROBE_RETRANSMITS {
                    // Nobody objected across every probe, so the block is ours.
                    self.state = State::Acquired;
                    self.next_due_ns = now_ns.saturating_add(ANNOUNCE_INTERVAL_NS);
                    Some(Action::Send(self.pdu(msg::ANNOUNCE)))
                } else {
                    self.state = State::Probing {
                        sent: sent.saturating_add(1),
                    };
                    self.next_due_ns = now_ns.saturating_add(PROBE_INTERVAL_NS);
                    Some(Action::Send(self.pdu(msg::PROBE)))
                }
            }
            State::Acquired => {
                self.next_due_ns = now_ns.saturating_add(ANNOUNCE_INTERVAL_NS);
                Some(Action::Send(self.pdu(msg::ANNOUNCE)))
            }
        }
    }

    /// Feed a received MAAP PDU. Returns a PDU to send when we must defend.
    ///
    /// `relocate` is called when we have to give up the block, and must return
    /// a fresh candidate start address; it is a callback rather than an
    /// internal computation so the address policy stays in one place with the
    /// code that derives our addresses.
    pub fn on_rx<F>(&mut self, pdu: &MaapPdu, now_ns: u64, relocate: F) -> Option<Action>
    where
        F: FnOnce(u32) -> MacAddr,
    {
        if !pdu.overlaps(self.start, self.count) {
            return None;
        }
        match (self.state, pdu.message_type) {
            // Someone else already holds or is defending this block. Whoever
            // is probing must move -- including us.
            (State::Probing { .. }, msg::ANNOUNCE | msg::DEFEND | msg::PROBE)
            | (State::Acquired, msg::ANNOUNCE | msg::DEFEND) => {
                self.conflicts = self.conflicts.saturating_add(1);
                self.attempt = self.attempt.saturating_add(1);
                self.start = relocate(self.attempt);
                self.state = State::Probing { sent: 0 };
                self.next_due_ns = now_ns;
                None
            }
            // We hold it and someone is probing it: say so.
            (State::Acquired, msg::PROBE) => {
                let mut d = self.pdu(msg::DEFEND);
                // Report the overlap back, which is what tells the prober
                // which range to avoid.
                d.conflict_start = self.start;
                d.conflict_count = self.count;
                Some(Action::Send(d))
            }
            _ => None,
        }
    }

    fn pdu(&self, message_type: u8) -> MaapPdu {
        MaapPdu {
            message_type,
            requested_start: self.start,
            requested_count: self.count,
            conflict_start: MacAddr([0; 6]),
            conflict_count: 0,
        }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// The real MAAP_ANNOUNCE captured from the live PreSonus 16R, verbatim,
    /// including the Ethernet padding it arrived with.
    fn captured_16r_announce() -> Vec<u8> {
        let mut v = vec![
            0xfe, 0x03, 0x08, 0x10, // subtype, msg_type=3, status|cdl=16
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // stream_id, sv=0
            0x91, 0xe0, 0xf0, 0x00, 0xd5, 0x48, // requested_start
            0x00, 0x09, // requested_count = its 9 streams
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // conflict_start
            0x00, 0x00, // conflict_count
        ];
        // Arrived as 50 payload bytes; the rest is padding to the Ethernet
        // minimum and must not stop the decode.
        v.resize(50, 0);
        v
    }

    #[test]
    fn decodes_the_16rs_real_announce() {
        let p = MaapPdu::decode(&captured_16r_announce()).expect("decodes");
        assert_eq!(p.message_type, msg::ANNOUNCE);
        assert_eq!(
            p.requested_start,
            MacAddr([0x91, 0xe0, 0xf0, 0x00, 0xd5, 0x48])
        );
        // Nine: eight audio STREAM_OUTPUTs plus the CRF one. This is the field
        // that proves the offset -- a layout read a byte out could not produce
        // the device's own stream count.
        assert_eq!(p.requested_count, 9);
        assert_eq!(p.conflict_count, 0);
        assert_eq!(p.conflict_start, MacAddr([0; 6]));
    }

    #[test]
    fn re_encodes_the_16rs_announce_byte_for_byte() {
        let raw = captured_16r_announce();
        let p = MaapPdu::decode(&raw).expect("decodes");
        // Only the PDU itself, not the frame padding.
        assert_eq!(&p.encode()[..], &raw[..MAAP_PDU_LEN]);
    }

    /// A round trip through our own codec cannot catch a shared offset error,
    /// so pin the header bytes we EMIT against the device's.
    #[test]
    fn our_probe_uses_the_devices_header_shape() {
        let p = MaapPdu {
            message_type: msg::PROBE,
            requested_start: MacAddr([0x91, 0xe0, 0xf0, 0x00, 0xc4, 0x8d]),
            requested_count: 8,
            ..Default::default()
        };
        let b = p.encode();
        assert_eq!(b[0], 0xfe, "subtype");
        assert_eq!(b[1], 0x01, "sv=0 version=0 message_type=PROBE");
        // The same maap_version|maap_data_length word the 16R sends, verbatim.
        assert_eq!((b[2], b[3]), (0x08, 0x10), "maap_version=1, length=16");
        assert_eq!(&b[4..12], &[0u8; 8], "MAAP leaves stream_id zero");
        assert_eq!(&b[12..18], &[0x91, 0xe0, 0xf0, 0x00, 0xc4, 0x8d]);
        assert_eq!(&b[18..20], &8u16.to_be_bytes());
    }

    #[test]
    fn probes_then_acquires_and_announces() {
        let start = MacAddr([0x91, 0xe0, 0xf0, 0x00, 0xc4, 0x8d]);
        let mut m = Maap::new(start, 8, 0);
        let mut t = 0u64;
        // Three probes.
        for i in 0..PROBE_RETRANSMITS {
            match m.tick(t) {
                Some(Action::Send(p)) => {
                    assert_eq!(p.message_type, msg::PROBE, "probe {i}");
                    assert_eq!(p.requested_count, 8);
                }
                None => panic!("probe {i} not emitted"),
            }
            assert!(!m.acquired(), "must not claim the block while probing");
            t += PROBE_INTERVAL_NS;
        }
        // Then it is ours, and the transition is an announce.
        match m.tick(t) {
            Some(Action::Send(p)) => assert_eq!(p.message_type, msg::ANNOUNCE),
            None => panic!("acquisition announce not emitted"),
        }
        assert!(m.acquired());
        assert_eq!(m.block(), (start, 8));
        // Quiet until the announce interval elapses.
        assert_eq!(m.tick(t + 1), None);
        assert!(matches!(
            m.tick(t + ANNOUNCE_INTERVAL_NS),
            Some(Action::Send(_))
        ));
    }

    /// The whole point of the module: until the block is acquired, a Talker
    /// attribute declared against it is invalid per Milan §6.3.1.
    #[test]
    fn not_acquired_until_probing_completes() {
        let m = Maap::new(MacAddr([0x91, 0xe0, 0xf0, 0x00, 0xc4, 0x8d]), 8, 0);
        assert!(!m.acquired(), "a fresh Maap has claimed nothing");
    }

    #[test]
    fn a_conflicting_announce_moves_us_and_restarts_probing() {
        let start = MacAddr([0x91, 0xe0, 0xf0, 0x00, 0xc4, 0x8d]);
        let mut m = Maap::new(start, 8, 0);
        let mut t = 0u64;
        for _ in 0..=PROBE_RETRANSMITS {
            m.tick(t);
            t += PROBE_INTERVAL_NS;
        }
        assert!(m.acquired());

        // The 16R announces a block that overlaps ours.
        let elsewhere = MacAddr([0x91, 0xe0, 0xf0, 0x00, 0x77, 0x00]);
        let intruder = MaapPdu {
            message_type: msg::ANNOUNCE,
            requested_start: MacAddr([0x91, 0xe0, 0xf0, 0x00, 0xc4, 0x90]),
            requested_count: 4,
            ..Default::default()
        };
        assert_eq!(m.on_rx(&intruder, t, |_| elsewhere), None);
        assert!(
            !m.acquired(),
            "a conflict must drop the claim, not keep transmitting to a contested block"
        );
        assert_eq!(m.block().0, elsewhere);
        assert_eq!(m.conflicts, 1);
    }

    #[test]
    fn we_defend_a_block_we_hold_against_a_probe() {
        let start = MacAddr([0x91, 0xe0, 0xf0, 0x00, 0xc4, 0x8d]);
        let mut m = Maap::new(start, 8, 0);
        let mut t = 0u64;
        for _ in 0..=PROBE_RETRANSMITS {
            m.tick(t);
            t += PROBE_INTERVAL_NS;
        }
        let prober = MaapPdu {
            message_type: msg::PROBE,
            requested_start: MacAddr([0x91, 0xe0, 0xf0, 0x00, 0xc4, 0x8f]),
            requested_count: 2,
            ..Default::default()
        };
        match m.on_rx(&prober, t, |_| panic!("must not relocate when defending")) {
            Some(Action::Send(d)) => {
                assert_eq!(d.message_type, msg::DEFEND);
                // The defend names the range to avoid.
                assert_eq!(d.conflict_start, start);
                assert_eq!(d.conflict_count, 8);
            }
            None => panic!("a probe into our block must be defended"),
        }
        assert!(m.acquired(), "defending keeps the block");
    }

    #[test]
    fn traffic_for_an_unrelated_block_is_ignored() {
        let mut m = Maap::new(MacAddr([0x91, 0xe0, 0xf0, 0x00, 0xc4, 0x8d]), 8, 0);
        let far = MaapPdu {
            message_type: msg::ANNOUNCE,
            requested_start: MacAddr([0x91, 0xe0, 0xf0, 0x00, 0xd5, 0x48]),
            requested_count: 9,
            ..Default::default()
        };
        assert_eq!(m.on_rx(&far, 0, |_| panic!("must not relocate")), None);
        assert_eq!(m.conflicts, 0);
    }

    /// Annex B warns against comparing only the low three bytes: another MAAP
    /// user need not share the 0x91E0F0 prefix.
    #[test]
    fn overlap_compares_the_whole_48_bit_address() {
        let ours = MacAddr([0x91, 0xe0, 0xf0, 0x00, 0xc4, 0x8d]);
        // Same low three bytes, different prefix -- must NOT be a conflict.
        let impostor = MaapPdu {
            message_type: msg::ANNOUNCE,
            requested_start: MacAddr([0x01, 0x02, 0x03, 0x00, 0xc4, 0x8d]),
            requested_count: 8,
            ..Default::default()
        };
        assert!(!impostor.overlaps(ours, 8));
        // And a genuine overlap on the full address is caught.
        let real = MaapPdu {
            requested_start: MacAddr([0x91, 0xe0, 0xf0, 0x00, 0xc4, 0x8d]),
            requested_count: 1,
            ..impostor
        };
        assert!(real.overlaps(ours, 8));
    }

    #[test]
    fn a_short_or_foreign_pdu_is_refused() {
        assert_eq!(MaapPdu::decode(&[]), None);
        assert_eq!(MaapPdu::decode(&[0xfe; MAAP_PDU_LEN - 1]), None);
        // ADP, not MAAP.
        let mut not_maap = captured_16r_announce();
        not_maap[0] = subtype::ADP;
        assert_eq!(MaapPdu::decode(&not_maap), None);
    }
}
