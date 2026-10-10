//! MSRP (802.1Q-2018 §35.2.2) and MVRP (§11.2.3.1) attribute values and their
//! FirstValue encodings.

use std::fmt::Display;

use avb_core::{MacAddr, StreamId};
use serde::{Serialize, Serializer};

use crate::wire::{Rd, Wr};

/// MSRP EtherType (802.1Q-2018 Table 35-? / §35.2.2.1 *(verify)*).
pub const ETHERTYPE_MSRP: u16 = 0x22EA;
/// MVRP EtherType (802.1Q-2018 §11.2.3.1.? *(verify)*).
pub const ETHERTYPE_MVRP: u16 = 0x88F5;
/// MSRPDU destination: Nearest Bridge group address (802.1Q-2018 §35.2.2.1,
/// Table 8-1).
pub const MSRP_DEST: MacAddr = MacAddr([0x01, 0x80, 0xC2, 0x00, 0x00, 0x0E]);
/// MVRPDU destination: Customer Bridge MVRP address (802.1Q-2018 Table 10-1).
pub const MVRP_DEST: MacAddr = MacAddr([0x01, 0x80, 0xC2, 0x00, 0x00, 0x21]);

/// MSRP AttributeType values (802.1Q-2018 Table 35-1 *(verify)*).
pub const MSRP_TALKER_ADVERTISE: u8 = 1;
pub const MSRP_TALKER_FAILED: u8 = 2;
pub const MSRP_LISTENER: u8 = 3;
pub const MSRP_DOMAIN: u8 = 4;
/// MVRP VID attribute type (802.1Q-2018 §11.2.3.1.6 *(verify)*).
pub const MVRP_VID: u8 = 1;

/// FirstValue lengths (AttributeLength) per type.
pub const TALKER_ADVERTISE_LEN: usize = 25;
pub const TALKER_FAILED_LEN: usize = 34;
pub const LISTENER_LEN: usize = 8;
pub const DOMAIN_LEN: usize = 4;
pub const VID_LEN: usize = 2;

/// SR class A default (design §8.2, SRP-1; 802.1Q-2018 Table 35-7 /
/// §35.2.2.9 *(verify)*): SRclassID 6, priority 3, VID 2.
pub const DEFAULT_DOMAIN: Domain = Domain {
    class_id: 6,
    priority: 3,
    vid: 2,
};

/// Reservation failure codes that mean "not enough bandwidth / resources"
/// (802.1Q-2018 Table 35-6 *(verify codes)*); a Talker Failed with one of
/// these classifies the stream A-5 `not_simultaneously_streamable` (design
/// §9.2). Not listed on purpose: 4 StreamID in use, 5 destination address in
/// use, 7 latency changed, 8 port not AVB capable, 9 use a different
/// destination, 12 cannot store destination, 13 priority not an SR class, 14
/// MaxFrameSize too large, 16–19 configuration mismatches.
pub const A5_FAILURE_CODES: [(u8, &str); 7] = [
    (1, "insufficient bandwidth"),
    (2, "insufficient bridge resources"),
    (3, "insufficient bandwidth for traffic class"),
    (6, "stream pre-empted by higher rank"),
    (10, "out of MSRP resources"),
    (11, "out of MMRP resources"),
    (15, "maxFanInPorts limit reached"),
];

pub fn is_a5_failure_code(code: u8) -> bool {
    A5_FAILURE_CODES.iter().any(|(c, _)| *c == code)
}

fn ser_display<T: Display, S: Serializer>(v: &T, s: S) -> Result<S::Ok, S::Error> {
    s.collect_str(v)
}

/// Talker Advertise FirstValue (802.1Q-2018 §35.2.2.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct TalkerAdvertise {
    #[serde(serialize_with = "ser_display")]
    pub stream_id: StreamId,
    #[serde(serialize_with = "ser_display")]
    pub dest_mac: MacAddr,
    pub vlan_id: u16,
    pub max_frame_size: u16,
    pub max_interval_frames: u16,
    /// PriorityAndRank bits 7–5.
    pub priority: u8,
    /// PriorityAndRank bit 4 (1 = non-emergency).
    pub rank: bool,
    pub accumulated_latency_ns: u32,
}

/// Talker Failed: the advertise fields + FailureInformation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct TalkerFailed {
    #[serde(flatten)]
    pub advertise: TalkerAdvertise,
    pub failure_bridge_id: u64,
    pub failure_code: u8,
}

/// The talker attribute of one StreamID: Advertise or Failed (802.1Q-2018
/// §35.2.6 lets a declaration switch between them without a Leave).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type")]
pub enum Talker {
    Advertise(TalkerAdvertise),
    Failed(TalkerFailed),
}

impl Talker {
    pub fn advertise(&self) -> &TalkerAdvertise {
        match self {
            Talker::Advertise(a) => a,
            Talker::Failed(f) => &f.advertise,
        }
    }

    pub fn stream_id(&self) -> StreamId {
        self.advertise().stream_id
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, Talker::Failed(_))
    }

    pub fn failure_code(&self) -> Option<u8> {
        match self {
            Talker::Failed(f) => Some(f.failure_code),
            Talker::Advertise(_) => None,
        }
    }
}

/// Listener declaration type, the FourPackedEvent (802.1Q-2018 §35.2.2.7.2
/// *(verify)*).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum ListenerDecl {
    Ignore = 0,
    AskingFailed = 1,
    Ready = 2,
    ReadyFailed = 3,
}

impl ListenerDecl {
    pub const fn from_u8(v: u8) -> Self {
        match v & 3 {
            1 => Self::AskingFailed,
            2 => Self::Ready,
            3 => Self::ReadyFailed,
            _ => Self::Ignore,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Listener {
    #[serde(serialize_with = "ser_display")]
    pub stream_id: StreamId,
    pub decl: ListenerDecl,
}

/// MSRP Domain (802.1Q-2018 §35.2.2.8.? *(verify)*).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct Domain {
    pub class_id: u8,
    pub priority: u8,
    pub vid: u16,
}

/// MRP application.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum App {
    Msrp,
    Mvrp,
}

/// Any attribute value we declare or register.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "attribute", rename_all = "snake_case")]
pub enum Attribute {
    Talker(Talker),
    Listener(Listener),
    Domain(Domain),
    Vid { vid: u16 },
}

/// Attribute identity: talker and listener attributes are keyed by StreamID,
/// Domain and VID by their whole value (MRP value semantics).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AttrKey {
    Talker(StreamId),
    Listener(StreamId),
    Domain(Domain),
    Vid(u16),
}

impl Attribute {
    pub fn key(&self) -> AttrKey {
        match self {
            Attribute::Talker(t) => AttrKey::Talker(t.stream_id()),
            Attribute::Listener(l) => AttrKey::Listener(l.stream_id),
            Attribute::Domain(d) => AttrKey::Domain(*d),
            Attribute::Vid { vid } => AttrKey::Vid(*vid),
        }
    }

    pub fn app(&self) -> App {
        match self {
            Attribute::Vid { .. } => App::Mvrp,
            _ => App::Msrp,
        }
    }

    pub fn attr_type(&self) -> u8 {
        match self {
            Attribute::Talker(Talker::Advertise(_)) => MSRP_TALKER_ADVERTISE,
            Attribute::Talker(Talker::Failed(_)) => MSRP_TALKER_FAILED,
            Attribute::Listener(_) => MSRP_LISTENER,
            Attribute::Domain(_) => MSRP_DOMAIN,
            Attribute::Vid { .. } => MVRP_VID,
        }
    }

    /// The Listener FourPackedEvent for this value, if the type has one.
    pub fn four_packed(&self) -> Option<ListenerDecl> {
        match self {
            Attribute::Listener(l) => Some(l.decl),
            _ => None,
        }
    }

    /// Encode the FirstValue.
    pub(crate) fn write_first_value(&self, w: &mut Wr<'_>) {
        match self {
            Attribute::Talker(t) => {
                let a = t.advertise();
                w.put(&a.stream_id.to_be_bytes());
                w.put(&a.dest_mac.0);
                w.u16(a.vlan_id);
                w.u16(a.max_frame_size);
                w.u16(a.max_interval_frames);
                w.u8(((a.priority & 7) << 5) | (u8::from(a.rank) << 4));
                w.u32(a.accumulated_latency_ns);
                if let Talker::Failed(f) = t {
                    w.put(&f.failure_bridge_id.to_be_bytes());
                    w.u8(f.failure_code);
                }
            }
            Attribute::Listener(l) => w.put(&l.stream_id.to_be_bytes()),
            Attribute::Domain(d) => {
                w.u8(d.class_id);
                w.u8(d.priority);
                w.u16(d.vid);
            }
            Attribute::Vid { vid } => w.u16(*vid),
        }
    }
}

/// Defined AttributeLength for a type, `None` if the type is unknown.
pub fn attr_len(app: App, attr_type: u8) -> Option<usize> {
    match (app, attr_type) {
        (App::Msrp, MSRP_TALKER_ADVERTISE) => Some(TALKER_ADVERTISE_LEN),
        (App::Msrp, MSRP_TALKER_FAILED) => Some(TALKER_FAILED_LEN),
        (App::Msrp, MSRP_LISTENER) => Some(LISTENER_LEN),
        (App::Msrp, MSRP_DOMAIN) => Some(DOMAIN_LEN),
        (App::Mvrp, MVRP_VID) => Some(VID_LEN),
        _ => None,
    }
}

/// Decode the FirstValue of a known type and apply the "FirstValue + n"
/// increment (802.1Q-2018 §35.2.2.8.4–5 for MSRP *(verify)*, §11.2.3.1.? for
/// MVRP): StreamID +n and destination_address +n for talkers, StreamID +n for
/// listeners, SRclassID +n and SRclassPriority +n for Domain, VID +n.
pub(crate) fn decode_value(
    app: App,
    attr_type: u8,
    fv: &[u8],
    n: u16,
    decl: ListenerDecl,
) -> Option<Attribute> {
    let mut r = Rd::new(fv);
    let inc = u64::from(n);
    Some(match (app, attr_type) {
        (App::Msrp, MSRP_TALKER_ADVERTISE | MSRP_TALKER_FAILED) => {
            let sid = r.u64()?;
            let mac = MacAddr(r.arr()?);
            let vlan_id = r.u16()?;
            let max_frame_size = r.u16()?;
            let max_interval_frames = r.u16()?;
            let pr = r.u8()?;
            let accumulated_latency_ns = r.u32()?;
            let a = TalkerAdvertise {
                stream_id: StreamId(sid.wrapping_add(inc)),
                dest_mac: MacAddr::from_u64(mac.to_u64().wrapping_add(inc) & 0xFFFF_FFFF_FFFF),
                vlan_id,
                max_frame_size,
                max_interval_frames,
                priority: pr >> 5,
                rank: pr & 0x10 != 0,
                accumulated_latency_ns,
            };
            if attr_type == MSRP_TALKER_FAILED {
                Attribute::Talker(Talker::Failed(TalkerFailed {
                    advertise: a,
                    failure_bridge_id: r.u64()?,
                    failure_code: r.u8()?,
                }))
            } else {
                Attribute::Talker(Talker::Advertise(a))
            }
        }
        (App::Msrp, MSRP_LISTENER) => Attribute::Listener(Listener {
            stream_id: StreamId(r.u64()?.wrapping_add(inc)),
            decl,
        }),
        (App::Msrp, MSRP_DOMAIN) => Attribute::Domain(Domain {
            class_id: r.u8()?.wrapping_add(n as u8),
            priority: r.u8()?.wrapping_add(n as u8),
            vid: r.u16()?,
        }),
        (App::Mvrp, MVRP_VID) => Attribute::Vid {
            vid: r.u16()?.wrapping_add(n),
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a5_codes() {
        assert!(is_a5_failure_code(1) && is_a5_failure_code(2) && is_a5_failure_code(3));
        assert!(!is_a5_failure_code(4) && !is_a5_failure_code(8) && !is_a5_failure_code(0));
        assert_eq!(
            DEFAULT_DOMAIN,
            Domain {
                class_id: 6,
                priority: 3,
                vid: 2
            }
        );
    }

    #[test]
    fn keys_and_types() {
        let sid = StreamId(0x000a_9203_3ba9_0000);
        let l = Attribute::Listener(Listener {
            stream_id: sid,
            decl: ListenerDecl::Ready,
        });
        assert_eq!(l.key(), AttrKey::Listener(sid));
        assert_eq!(
            (l.app(), l.attr_type(), l.four_packed()),
            (App::Msrp, 3, Some(ListenerDecl::Ready))
        );
        let v = Attribute::Vid { vid: 2 };
        assert_eq!(
            (v.app(), v.attr_type(), v.key()),
            (App::Mvrp, 1, AttrKey::Vid(2))
        );
        assert_eq!(attr_len(App::Msrp, 5), None);
        assert_eq!(attr_len(App::Mvrp, 1), Some(2));
        for v in 0..4 {
            assert_eq!(ListenerDecl::from_u8(v) as u8, v);
        }
    }
}
