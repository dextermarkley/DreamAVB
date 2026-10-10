//! IEEE 802.1AS-2020 message codec (layer-2 transport, design §7.1, §13.2).
//!
//! Decoded: Sync (two-step), Follow_Up with the Follow_Up information TLV,
//! Pdelay_Req, Pdelay_Resp, Pdelay_Resp_Follow_Up, Announce with the path
//! trace TLV, and Signaling with the message-interval-request and gPTP-capable
//! TLVs.
//!
//! Encoded for transmission: **only** Pdelay_Req, Pdelay_Resp,
//! Pdelay_Resp_Follow_Up and the gPTP-capable Signaling reply. There is
//! deliberately no encoder for Announce, Sync or Follow_Up: our port is never
//! master (design §7.1, §13.3), and that is structural, not a flag.
//!
//! Field layouts are from 802.1AS-2020 clause 10.6 / 11.4 (common header
//! Table 10-7 *(verify table number)*, message bodies §11.4.2–§11.4.6 and
//! §10.6.3, TLVs §11.4.4.3, §10.6.4.3, §10.6.4.4) and pinned by the vectors in
//! the tests below.

use std::fmt;

use avb_core::{EthFrame, EthHeader, Eui64, GptpNs, MacAddr, TxFrame};
use serde::{Serialize, Serializer};

use crate::wire::{Rd, Wr};
use avb_core::DropReason;

/// gPTP EtherType (802.1AS-2020 §11.3.4 *(verify)*).
pub const ETHERTYPE_PTP: u16 = 0x88F7;
/// Destination of every 802.1AS PDU on a full-duplex link
/// (802.1AS-2020 §11.3.4: the individual-LAN-scope group address).
pub const PTP_PEER_MCAST: MacAddr = MacAddr([0x01, 0x80, 0xC2, 0x00, 0x00, 0x0E]);

/// Common header length.
pub const HEADER_LEN: usize = 34;
/// majorSdoId (transportSpecific) for gPTP (802.1AS-2020 §10.6.2.2.1).
pub const MAJOR_SDO_ID: u8 = 1;
/// versionPTP (802.1AS-2020 §10.6.2.2.4).
pub const VERSION_PTP: u8 = 2;
/// The only domain we take part in (design §7.1).
pub const DOMAIN: u8 = 0;

/// flags: twoStepFlag (octet 0 bit 1), 802.1AS-2020 Table 10-9 *(verify)*.
pub const FLAG_TWO_STEP: u16 = 0x0200;
/// flags: ptpTimescale (octet 1 bit 3).
pub const FLAG_PTP_TIMESCALE: u16 = 0x0008;

/// logMessageInterval for Pdelay_Resp / Pdelay_Resp_Follow_Up / Signaling
/// (802.1AS-2020 §10.6.2.2.14 / §11.4.2.8 *(verify)*).
pub const LOG_INTERVAL_UNSPECIFIED: i8 = 0x7F;

/// controlField values (802.1AS-2020 Table 10-6 *(verify)*; the field is
/// informational in v2 but kept as 1588-2008 assigns it).
const CONTROL_OTHER: u8 = 5;

/// Body sizes (messageLength) per type.
pub const SYNC_LEN: usize = 44;
pub const FOLLOW_UP_LEN: usize = 76;
pub const PDELAY_LEN: usize = 54;
pub const ANNOUNCE_BASE_LEN: usize = 64;
pub const SIGNALING_BASE_LEN: usize = 44;
/// Signaling with one 16-octet organization-extension TLV.
pub const SIGNALING_GPTP_CAPABLE_LEN: usize = SIGNALING_BASE_LEN + 16;

/// TLV types (1588-2019 Table 52 / 802.1AS-2020 Table 10-11 *(verify)*).
pub const TLV_ORGANIZATION_EXTENSION: u16 = 0x0003;
pub const TLV_PATH_TRACE: u16 = 0x0008;
/// IEEE 802.1 organizationId 00-80-C2.
pub const ORG_ID_8021: [u8; 3] = [0x00, 0x80, 0xC2];
/// organizationSubType: Follow_Up information TLV (802.1AS-2020 §11.4.4.3).
pub const ORG_SUBTYPE_FOLLOW_UP_INFO: u32 = 1;
/// organizationSubType: message interval request TLV (802.1AS-2020 §10.6.4.3).
pub const ORG_SUBTYPE_INTERVAL_REQUEST: u32 = 2;
/// organizationSubType: gPTP-capable TLV (802.1AS-2020 §10.6.4.4 *(verify)*).
pub const ORG_SUBTYPE_GPTP_CAPABLE: u32 = 4;
/// lengthField of the Follow_Up information TLV.
const FOLLOW_UP_TLV_LEN: u16 = 28;
/// lengthField of the interval-request and gPTP-capable TLVs.
const SIGNALING_TLV_LEN: u16 = 12;

/// An EUI-64 clock identity.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ClockIdentity(pub [u8; 8]);

impl ClockIdentity {
    /// From the I210 MAC with FF:FE inserted (design §7.1).
    pub const fn from_mac(mac: MacAddr) -> Self {
        Self(Eui64::from_mac(mac).0)
    }
}

impl fmt::Display for ClockIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for b in self.0 {
            if !first {
                f.write_str(":")?;
            }
            first = false;
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ClockIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Serialize for ClockIdentity {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// clockIdentity + portNumber.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct PortIdentity {
    pub clock: ClockIdentity,
    pub port: u16,
}

impl PortIdentity {
    /// targetPortIdentity "all ports" (all ones).
    pub const WILDCARD: PortIdentity = PortIdentity {
        clock: ClockIdentity([0xFF; 8]),
        port: 0xFFFF,
    };

    fn read(r: &mut Rd<'_>) -> Option<Self> {
        Some(Self {
            clock: ClockIdentity(r.arr()?),
            port: r.u16()?,
        })
    }

    fn write(&self, w: &mut Wr<'_>) {
        w.put(&self.clock.0);
        w.u16(self.port);
    }
}

impl fmt::Display for PortIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.clock, self.port)
    }
}

impl Serialize for PortIdentity {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// PTP Timestamp: 48-bit seconds + 32-bit nanoseconds.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Timestamp {
    pub secs: u64,
    pub nanos: u32,
}

impl Timestamp {
    /// `None` if nanoseconds ≥ 1e9 (malformed) or the value overflows u64 ns.
    pub fn to_ns(self) -> Option<u64> {
        GptpNs::from_secs_nanos(self.secs, self.nanos).map(|g| g.0)
    }

    pub fn from_ns(ns: u64) -> Self {
        let g = GptpNs(ns);
        Self {
            secs: g.secs(),
            nanos: g.subsec_nanos(),
        }
    }

    fn read(r: &mut Rd<'_>) -> Option<Self> {
        Some(Self {
            secs: r.u48()?,
            nanos: r.u32()?,
        })
    }

    fn write(&self, w: &mut Wr<'_>) {
        w.u48(self.secs);
        w.u32(self.nanos);
    }
}

/// messageType nibble (802.1AS-2020 Table 10-5 *(verify)*).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum MessageType {
    Sync = 0x0,
    PdelayReq = 0x2,
    PdelayResp = 0x3,
    FollowUp = 0x8,
    PdelayRespFollowUp = 0xA,
    Announce = 0xB,
    Signaling = 0xC,
}

impl MessageType {
    pub const fn from_nibble(n: u8) -> Option<Self> {
        Some(match n & 0x0F {
            0x0 => Self::Sync,
            0x2 => Self::PdelayReq,
            0x3 => Self::PdelayResp,
            0x8 => Self::FollowUp,
            0xA => Self::PdelayRespFollowUp,
            0xB => Self::Announce,
            0xC => Self::Signaling,
            _ => return None,
        })
    }

    /// Minimum messageLength for this type.
    pub const fn min_len(self) -> usize {
        match self {
            Self::Sync => SYNC_LEN,
            Self::FollowUp => FOLLOW_UP_LEN,
            Self::PdelayReq | Self::PdelayResp | Self::PdelayRespFollowUp => PDELAY_LEN,
            Self::Announce => ANNOUNCE_BASE_LEN,
            Self::Signaling => SIGNALING_BASE_LEN,
        }
    }

    const fn control(self) -> u8 {
        match self {
            Self::Sync => 0,
            Self::FollowUp => 2,
            _ => CONTROL_OTHER,
        }
    }
}

/// The 34-octet common header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    pub message_type: MessageType,
    pub major_sdo_id: u8,
    pub minor_version: u8,
    pub version: u8,
    pub length: u16,
    pub domain: u8,
    pub minor_sdo_id: u8,
    pub flags: u16,
    /// correctionField, scaled ns (2^-16 ns).
    pub correction: i64,
    pub type_specific: u32,
    pub source: PortIdentity,
    pub sequence_id: u16,
    pub control: u8,
    pub log_interval: i8,
}

impl Header {
    /// A header for one of the messages we transmit.
    fn ours(
        t: MessageType,
        len: usize,
        source: PortIdentity,
        seq: u16,
        log: i8,
        flags: u16,
    ) -> Self {
        Self {
            message_type: t,
            major_sdo_id: MAJOR_SDO_ID,
            minor_version: 0,
            version: VERSION_PTP,
            length: len as u16,
            domain: DOMAIN,
            minor_sdo_id: 0,
            flags,
            correction: 0,
            type_specific: 0,
            source,
            sequence_id: seq,
            control: t.control(),
            log_interval: log,
        }
    }

    fn write(&self, w: &mut Wr<'_>) {
        w.u8((self.major_sdo_id << 4) | self.message_type as u8);
        w.u8((self.minor_version << 4) | (self.version & 0x0F));
        w.u16(self.length);
        w.u8(self.domain);
        w.u8(self.minor_sdo_id);
        w.u16(self.flags);
        w.i64(self.correction);
        w.u32(self.type_specific);
        self.source.write(w);
        w.u16(self.sequence_id);
        w.u8(self.control);
        w.u8(self.log_interval as u8);
    }

    /// correctionField in ns.
    pub fn correction_ns(&self) -> f64 {
        scaled_ns_to_f64(self.correction)
    }
}

/// Scaled ns (2^-16 ns units) → ns.
pub fn scaled_ns_to_f64(v: i64) -> f64 {
    v as f64 / 65536.0
}

/// 802.1AS Follow_Up information TLV (802.1AS-2020 §11.4.4.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FollowUpInfo {
    /// (rateRatio − 1) × 2^41.
    pub cumulative_scaled_rate_offset: i32,
    pub gm_time_base_indicator: u16,
    /// ScaledNs (96 bits), kept raw.
    pub last_gm_phase_change: [u8; 12],
    pub scaled_last_gm_freq_change: i32,
}

impl FollowUpInfo {
    /// 1 + cumulativeScaledRateOffset × 2^-41.
    pub fn rate_ratio(&self) -> f64 {
        1.0 + f64::from(self.cumulative_scaled_rate_offset) / (1u64 << 41) as f64
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FollowUp {
    pub precise_origin: Timestamp,
    pub info: FollowUpInfo,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PdelayResp {
    pub request_receipt: Timestamp,
    pub requesting: PortIdentity,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PdelayRespFollowUp {
    pub response_origin: Timestamp,
    pub requesting: PortIdentity,
}

/// grandmasterClockQuality.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize)]
pub struct ClockQuality {
    pub clock_class: u8,
    pub clock_accuracy: u8,
    pub offset_scaled_log_variance: u16,
}

/// Announce body (802.1AS-2020 §10.6.3). `path_trace` holds the raw
/// pathSequence octets (a multiple of 8) of the path trace TLV, if present.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Announce<'a> {
    pub current_utc_offset: i16,
    pub gm_priority1: u8,
    pub gm_clock_quality: ClockQuality,
    pub gm_priority2: u8,
    pub gm_identity: ClockIdentity,
    pub steps_removed: u16,
    pub time_source: u8,
    path_trace: &'a [u8],
}

impl<'a> Announce<'a> {
    /// The path trace entries, in order.
    pub fn path_trace(&self) -> impl Iterator<Item = ClockIdentity> + 'a {
        self.path_trace
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| ClockIdentity(*c))
    }

    pub fn path_trace_contains(&self, id: ClockIdentity) -> bool {
        self.path_trace().any(|c| c == id)
    }
}

/// Message interval request TLV (802.1AS-2020 §10.6.4.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IntervalRequest {
    pub link_delay_interval: i8,
    pub time_sync_interval: i8,
    pub announce_interval: i8,
    pub flags: u8,
}

impl IntervalRequest {
    /// Special values (802.1AS-2020 §10.6.4.3.4 *(verify)*).
    pub const NO_CHANGE: i8 = -128;
    pub const SET_INITIAL: i8 = 126;
    pub const STOP: i8 = 127;
}

/// gPTP-capable TLV (802.1AS-2020 §10.6.4.4 *(verify)*).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GptpCapable {
    pub log_interval: i8,
    pub flags: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Signaling {
    pub target: PortIdentity,
    pub interval_request: Option<IntervalRequest>,
    pub gptp_capable: Option<GptpCapable>,
}

/// A decoded message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Body<'a> {
    Sync,
    FollowUp(FollowUp),
    PdelayReq,
    PdelayResp(PdelayResp),
    PdelayRespFollowUp(PdelayRespFollowUp),
    Announce(Announce<'a>),
    Signaling(Signaling),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Message<'a> {
    pub header: Header,
    pub body: Body<'a>,
}

/// Decode a gPTP message from an Ethernet frame (destination and EtherType
/// checked; one VLAN tag tolerated).
pub fn decode_frame(frame: &[u8]) -> Result<Message<'_>, DropReason> {
    let eth = EthFrame::parse(frame)?;
    if eth.hdr.ethertype != ETHERTYPE_PTP {
        return Err(DropReason::WrongEtherType);
    }
    if eth.hdr.dst != PTP_PEER_MCAST {
        return Err(DropReason::WrongDestination);
    }
    decode(eth.payload)
}

/// Decode a PTP message (the Ethernet payload). Validation per design §13.2:
/// majorSdoId 1, versionPTP 2, domain 0, messageLength ≥ the type's size and
/// ≤ the payload; the Follow_Up information TLV must be type 3 / 00-80-C2 /
/// subtype 1.
pub fn decode(p: &[u8]) -> Result<Message<'_>, DropReason> {
    let mut r = Rd::new(p);
    let b0 = r.u8().ok_or(DropReason::TooShort)?;
    let b1 = r.u8().ok_or(DropReason::TooShort)?;
    let length = r.u16().ok_or(DropReason::TooShort)?;
    if p.len() < HEADER_LEN {
        return Err(DropReason::TooShort);
    }
    if b0 >> 4 != MAJOR_SDO_ID || b1 & 0x0F != VERSION_PTP {
        return Err(DropReason::Unsupported);
    }
    let message_type = MessageType::from_nibble(b0).ok_or(DropReason::Unsupported)?;
    let len = usize::from(length);
    if len < message_type.min_len() {
        return Err(DropReason::Malformed);
    }
    // The dext pads short frames; bytes past messageLength are ignored.
    let msg = p.get(..len).ok_or(DropReason::TooShort)?;
    let mut r = Rd::new(msg.get(4..).ok_or(DropReason::TooShort)?);
    let short = DropReason::TooShort;
    let domain = r.u8().ok_or(short)?;
    let minor_sdo_id = r.u8().ok_or(short)?;
    let flags = r.u16().ok_or(short)?;
    let correction = r.i64().ok_or(short)?;
    let type_specific = r.u32().ok_or(short)?;
    let source = PortIdentity::read(&mut r).ok_or(short)?;
    let sequence_id = r.u16().ok_or(short)?;
    let control = r.u8().ok_or(short)?;
    let log_interval = r.i8().ok_or(short)?;
    if domain != DOMAIN {
        return Err(DropReason::NotForUs);
    }
    let header = Header {
        message_type,
        major_sdo_id: b0 >> 4,
        minor_version: b1 >> 4,
        version: b1 & 0x0F,
        length,
        domain,
        minor_sdo_id,
        flags,
        correction,
        type_specific,
        source,
        sequence_id,
        control,
        log_interval,
    };
    let body = match message_type {
        MessageType::Sync => Body::Sync,
        MessageType::PdelayReq => Body::PdelayReq,
        MessageType::FollowUp => Body::FollowUp(decode_follow_up(&mut r)?),
        MessageType::PdelayResp => {
            let request_receipt = Timestamp::read(&mut r).ok_or(short)?;
            let requesting = PortIdentity::read(&mut r).ok_or(short)?;
            Body::PdelayResp(PdelayResp {
                request_receipt,
                requesting,
            })
        }
        MessageType::PdelayRespFollowUp => {
            let response_origin = Timestamp::read(&mut r).ok_or(short)?;
            let requesting = PortIdentity::read(&mut r).ok_or(short)?;
            Body::PdelayRespFollowUp(PdelayRespFollowUp {
                response_origin,
                requesting,
            })
        }
        MessageType::Announce => Body::Announce(decode_announce(&mut r)?),
        MessageType::Signaling => Body::Signaling(decode_signaling(&mut r)?),
    };
    Ok(Message { header, body })
}

fn decode_follow_up(r: &mut Rd<'_>) -> Result<FollowUp, DropReason> {
    let short = DropReason::TooShort;
    let precise_origin = Timestamp::read(r).ok_or(short)?;
    let tlv_type = r.u16().ok_or(short)?;
    let tlv_len = r.u16().ok_or(short)?;
    let org: [u8; 3] = r.arr().ok_or(short)?;
    let sub: [u8; 3] = r.arr().ok_or(short)?;
    let subtype = u32::from_be_bytes([0, sub[0], sub[1], sub[2]]);
    if tlv_type != TLV_ORGANIZATION_EXTENSION
        || tlv_len != FOLLOW_UP_TLV_LEN
        || org != ORG_ID_8021
        || subtype != ORG_SUBTYPE_FOLLOW_UP_INFO
    {
        return Err(DropReason::Malformed);
    }
    let info = FollowUpInfo {
        cumulative_scaled_rate_offset: r.i32().ok_or(short)?,
        gm_time_base_indicator: r.u16().ok_or(short)?,
        last_gm_phase_change: r.arr().ok_or(short)?,
        scaled_last_gm_freq_change: r.i32().ok_or(short)?,
    };
    if precise_origin.to_ns().is_none() {
        return Err(DropReason::Malformed);
    }
    Ok(FollowUp {
        precise_origin,
        info,
    })
}

fn decode_announce<'a>(r: &mut Rd<'a>) -> Result<Announce<'a>, DropReason> {
    let short = DropReason::TooShort;
    r.take(10).ok_or(short)?; // originTimestamp (reserved in 802.1AS)
    let current_utc_offset = r.i16().ok_or(short)?;
    r.u8().ok_or(short)?; // reserved
    let gm_priority1 = r.u8().ok_or(short)?;
    let gm_clock_quality = ClockQuality {
        clock_class: r.u8().ok_or(short)?,
        clock_accuracy: r.u8().ok_or(short)?,
        offset_scaled_log_variance: r.u16().ok_or(short)?,
    };
    let gm_priority2 = r.u8().ok_or(short)?;
    let gm_identity = ClockIdentity(r.arr().ok_or(short)?);
    let steps_removed = r.u16().ok_or(short)?;
    let time_source = r.u8().ok_or(short)?;
    let mut path_trace: &[u8] = &[];
    while r.remaining() >= 4 {
        let t = r.u16().ok_or(short)?;
        let l = r.u16().ok_or(short)?;
        let v = r.take(usize::from(l)).ok_or(DropReason::Malformed)?;
        if t == TLV_PATH_TRACE {
            if v.len() % 8 != 0 {
                return Err(DropReason::Malformed);
            }
            path_trace = v;
        }
    }
    Ok(Announce {
        current_utc_offset,
        gm_priority1,
        gm_clock_quality,
        gm_priority2,
        gm_identity,
        steps_removed,
        time_source,
        path_trace,
    })
}

fn decode_signaling(r: &mut Rd<'_>) -> Result<Signaling, DropReason> {
    let short = DropReason::TooShort;
    let target = PortIdentity::read(r).ok_or(short)?;
    let mut s = Signaling {
        target,
        interval_request: None,
        gptp_capable: None,
    };
    while r.remaining() >= 4 {
        let t = r.u16().ok_or(short)?;
        let l = r.u16().ok_or(short)?;
        let v = r.take(usize::from(l)).ok_or(DropReason::Malformed)?;
        if t != TLV_ORGANIZATION_EXTENSION || l != SIGNALING_TLV_LEN {
            continue;
        }
        let mut tv = Rd::new(v);
        let org: [u8; 3] = tv.arr().ok_or(short)?;
        let sub: [u8; 3] = tv.arr().ok_or(short)?;
        if org != ORG_ID_8021 {
            continue;
        }
        match u32::from_be_bytes([0, sub[0], sub[1], sub[2]]) {
            ORG_SUBTYPE_INTERVAL_REQUEST => {
                s.interval_request = Some(IntervalRequest {
                    link_delay_interval: tv.i8().ok_or(short)?,
                    time_sync_interval: tv.i8().ok_or(short)?,
                    announce_interval: tv.i8().ok_or(short)?,
                    flags: tv.u8().ok_or(short)?,
                });
            }
            ORG_SUBTYPE_GPTP_CAPABLE => {
                s.gptp_capable = Some(GptpCapable {
                    log_interval: tv.i8().ok_or(short)?,
                    flags: tv.u8().ok_or(short)?,
                });
            }
            _ => {}
        }
    }
    Ok(s)
}

fn org_subtype(v: u32) -> [u8; 3] {
    let [_, a, b, c] = v.to_be_bytes();
    [a, b, c]
}

// --- Encoders: the four messages we are allowed to transmit (design §3.5) ---

fn eth_header(src: MacAddr) -> EthHeader {
    EthHeader {
        dst: PTP_PEER_MCAST,
        src,
        vlan: None,
        ethertype: ETHERTYPE_PTP,
    }
}

fn finish(src: MacAddr, payload: &[u8], complete: bool, want_ts: bool, tag: u64) -> TxFrame {
    debug_assert!(complete);
    // The payload is at most 60 octets, far below TX_FRAME_MAX.
    TxFrame::build(&eth_header(src), payload, want_ts, tag).unwrap_or(TxFrame::EMPTY)
}

/// Pdelay_Req (802.1AS-2020 §11.4.5): logMessageInterval = our current
/// logPdelayReqInterval, both 10-octet body fields reserved (zero).
pub fn encode_pdelay_req(
    src_mac: MacAddr,
    source: PortIdentity,
    seq: u16,
    log_interval: i8,
    tag: u64,
) -> TxFrame {
    let mut b = [0u8; PDELAY_LEN];
    let mut w = Wr::new(&mut b);
    Header::ours(
        MessageType::PdelayReq,
        PDELAY_LEN,
        source,
        seq,
        log_interval,
        FLAG_PTP_TIMESCALE,
    )
    .write(&mut w);
    w.zeros(20);
    let complete = !w.overflowed() && w.pos() == b.len();
    finish(src_mac, &b, complete, true, tag)
}

/// Pdelay_Resp (802.1AS-2020 §11.4.6): requestReceiptTimestamp = t2 (ours,
/// compensated), requestingPortIdentity copied, correction 0, sent with a TX
/// timestamp request.
pub fn encode_pdelay_resp(
    src_mac: MacAddr,
    source: PortIdentity,
    seq: u16,
    request_receipt: Timestamp,
    requesting: PortIdentity,
    tag: u64,
) -> TxFrame {
    let mut b = [0u8; PDELAY_LEN];
    let mut w = Wr::new(&mut b);
    Header::ours(
        MessageType::PdelayResp,
        PDELAY_LEN,
        source,
        seq,
        LOG_INTERVAL_UNSPECIFIED,
        FLAG_TWO_STEP | FLAG_PTP_TIMESCALE,
    )
    .write(&mut w);
    request_receipt.write(&mut w);
    requesting.write(&mut w);
    let complete = !w.overflowed() && w.pos() == b.len();
    finish(src_mac, &b, complete, true, tag)
}

/// Pdelay_Resp_Follow_Up (802.1AS-2020 §11.4.7 *(verify)*):
/// responseOriginTimestamp = t3 (ours, compensated).
pub fn encode_pdelay_resp_follow_up(
    src_mac: MacAddr,
    source: PortIdentity,
    seq: u16,
    response_origin: Timestamp,
    requesting: PortIdentity,
    tag: u64,
) -> TxFrame {
    let mut b = [0u8; PDELAY_LEN];
    let mut w = Wr::new(&mut b);
    Header::ours(
        MessageType::PdelayRespFollowUp,
        PDELAY_LEN,
        source,
        seq,
        LOG_INTERVAL_UNSPECIFIED,
        FLAG_PTP_TIMESCALE,
    )
    .write(&mut w);
    response_origin.write(&mut w);
    requesting.write(&mut w);
    let complete = !w.overflowed() && w.pos() == b.len();
    finish(src_mac, &b, complete, false, tag)
}

/// Signaling carrying our gPTP-capable TLV (design §7.4; only sent in reply
/// to the peer's own gPTP-capable TLV). targetPortIdentity is the wildcard.
pub fn encode_signaling_gptp_capable(
    src_mac: MacAddr,
    source: PortIdentity,
    seq: u16,
    tlv_log_interval: i8,
    tag: u64,
) -> TxFrame {
    const LEN: usize = SIGNALING_GPTP_CAPABLE_LEN;
    let mut b = [0u8; LEN];
    let mut w = Wr::new(&mut b);
    Header::ours(
        MessageType::Signaling,
        LEN,
        source,
        seq,
        LOG_INTERVAL_UNSPECIFIED,
        FLAG_PTP_TIMESCALE,
    )
    .write(&mut w);
    PortIdentity::WILDCARD.write(&mut w);
    w.u16(TLV_ORGANIZATION_EXTENSION);
    w.u16(SIGNALING_TLV_LEN);
    w.put(&ORG_ID_8021);
    w.put(org_subtype(ORG_SUBTYPE_GPTP_CAPABLE).as_slice());
    w.u8(tlv_log_interval as u8);
    w.u8(0); // flags
    w.zeros(4); // reserved
    let complete = !w.overflowed() && w.pos() == b.len();
    finish(src_mac, &b, complete, false, tag)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
pub(crate) mod tests {
    use super::*;

    pub const OUR_MAC: MacAddr = MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]);

    pub fn ours() -> PortIdentity {
        PortIdentity {
            clock: ClockIdentity::from_mac(OUR_MAC),
            port: 1,
        }
    }

    pub fn peer() -> PortIdentity {
        PortIdentity {
            clock: ClockIdentity([0x00, 0x0a, 0x92, 0xff, 0xfe, 0x03, 0x3b, 0xa9]),
            port: 1,
        }
    }

    const ETH_PTP: [u8; 14] = [
        0x01, 0x80, 0xC2, 0x00, 0x00, 0x0E, 0x00, 0x0a, 0x92, 0x03, 0x3b, 0xa9, 0x88, 0xF7,
    ];

    fn with_eth(p: &[u8]) -> Vec<u8> {
        let mut v = ETH_PTP.to_vec();
        v.extend_from_slice(p);
        v
    }

    #[test]
    fn clock_identity_from_mac() {
        let c = ClockIdentity::from_mac(OUR_MAC);
        assert_eq!(c.0, [0x64, 0x4b, 0xf0, 0xff, 0xfe, 0x12, 0x73, 0xb8]);
        assert_eq!(c.to_string(), "64:4b:f0:ff:fe:12:73:b8");
        assert_eq!(ours().to_string(), "64:4b:f0:ff:fe:12:73:b8/1");
    }

    /// Pdelay_Req as we send it. Byte-identical to the Phase 0b probe frame
    /// that the 16R answered (docs/phase0b-i210-results.md G4).
    #[test]
    fn pdelay_req_vector() {
        let f = encode_pdelay_req(OUR_MAC, ours(), 0x1234, 0, 77);
        #[rustfmt::skip]
        let want: [u8; 68] = [
            0x01, 0x80, 0xC2, 0x00, 0x00, 0x0E, 0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8, 0x88, 0xF7,
            0x12, 0x02, 0x00, 0x36, 0x00, 0x00, 0x00, 0x08,
            0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0,
            0x64, 0x4b, 0xf0, 0xff, 0xfe, 0x12, 0x73, 0xb8, 0x00, 0x01,
            0x12, 0x34, 0x05, 0x00,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(f.bytes(), &want[..]);
        assert!(f.want_ts);
        assert_eq!(f.tag, 77);
        let m = decode_frame(f.bytes()).unwrap();
        assert_eq!(m.header.message_type, MessageType::PdelayReq);
        assert_eq!(m.header.source, ours());
        assert_eq!(m.header.sequence_id, 0x1234);
        assert_eq!(m.body, Body::PdelayReq);
    }

    #[test]
    fn pdelay_resp_vector() {
        let t2 = Timestamp {
            secs: 0x0000_0102_0304,
            nanos: 123_456_789,
        };
        let f = encode_pdelay_resp(OUR_MAC, ours(), 0xBEEF, t2, peer(), 5);
        #[rustfmt::skip]
        let want: [u8; 68] = [
            0x01, 0x80, 0xC2, 0x00, 0x00, 0x0E, 0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8, 0x88, 0xF7,
            0x13, 0x02, 0x00, 0x36, 0x00, 0x00, 0x02, 0x08,
            0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0,
            0x64, 0x4b, 0xf0, 0xff, 0xfe, 0x12, 0x73, 0xb8, 0x00, 0x01,
            0xBE, 0xEF, 0x05, 0x7F,
            0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x07, 0x5B, 0xCD, 0x15,
            0x00, 0x0a, 0x92, 0xff, 0xfe, 0x03, 0x3b, 0xa9, 0x00, 0x01,
        ];
        assert_eq!(f.bytes(), &want[..]);
        assert!(f.want_ts);
        let m = decode_frame(f.bytes()).unwrap();
        assert_eq!(
            m.body,
            Body::PdelayResp(PdelayResp {
                request_receipt: t2,
                requesting: peer()
            })
        );
        assert_eq!(m.header.flags & FLAG_TWO_STEP, FLAG_TWO_STEP);
    }

    #[test]
    fn pdelay_resp_follow_up_vector() {
        let t3 = Timestamp { secs: 1, nanos: 2 };
        let f = encode_pdelay_resp_follow_up(OUR_MAC, ours(), 1, t3, peer(), 6);
        #[rustfmt::skip]
        let want: [u8; 68] = [
            0x01, 0x80, 0xC2, 0x00, 0x00, 0x0E, 0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8, 0x88, 0xF7,
            0x1A, 0x02, 0x00, 0x36, 0x00, 0x00, 0x00, 0x08,
            0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0,
            0x64, 0x4b, 0xf0, 0xff, 0xfe, 0x12, 0x73, 0xb8, 0x00, 0x01,
            0x00, 0x01, 0x05, 0x7F,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02,
            0x00, 0x0a, 0x92, 0xff, 0xfe, 0x03, 0x3b, 0xa9, 0x00, 0x01,
        ];
        assert_eq!(f.bytes(), &want[..]);
        assert!(!f.want_ts);
    }

    #[test]
    fn signaling_gptp_capable_vector() {
        let f = encode_signaling_gptp_capable(OUR_MAC, ours(), 9, 3, 0);
        #[rustfmt::skip]
        let want: [u8; 74] = [
            0x01, 0x80, 0xC2, 0x00, 0x00, 0x0E, 0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8, 0x88, 0xF7,
            0x1C, 0x02, 0x00, 0x3C, 0x00, 0x00, 0x00, 0x08,
            0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0,
            0x64, 0x4b, 0xf0, 0xff, 0xfe, 0x12, 0x73, 0xb8, 0x00, 0x01,
            0x00, 0x09, 0x05, 0x7F,
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0x00, 0x03, 0x00, 0x0C, 0x00, 0x80, 0xC2, 0x00, 0x00, 0x04,
            0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert_eq!(f.bytes(), &want[..]);
        let m = decode_frame(f.bytes()).unwrap();
        let Body::Signaling(s) = m.body else { panic!() };
        assert_eq!(s.target, PortIdentity::WILDCARD);
        assert_eq!(
            s.gptp_capable,
            Some(GptpCapable {
                log_interval: 3,
                flags: 0
            })
        );
        assert_eq!(s.interval_request, None);
    }

    /// Two-step Sync from the GM (802.1AS-2020 §11.4.3), seq 0x0102,
    /// logMessageInterval −3, non-zero correctionField −1.5 ns.
    #[rustfmt::skip]
    pub const SYNC_VECTOR: [u8; 44] = [
        0x10, 0x02, 0x00, 0x2C, 0x00, 0x00, 0x02, 0x08,
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE, 0x80, 0x00,
        0, 0, 0, 0,
        0x00, 0x0a, 0x92, 0xff, 0xfe, 0x03, 0x3b, 0xa9, 0x00, 0x01,
        0x01, 0x02, 0x00, 0xFD,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    #[test]
    fn sync_vector() {
        let f = with_eth(&SYNC_VECTOR);
        let m = decode_frame(&f).unwrap();
        assert_eq!(m.body, Body::Sync);
        let h = m.header;
        assert_eq!(h.message_type, MessageType::Sync);
        assert_eq!(h.sequence_id, 0x0102);
        assert_eq!(h.log_interval, -3);
        assert_eq!(h.correction, -0x18000);
        assert_eq!(h.correction_ns(), -1.5);
        assert_eq!(h.source, peer());
        assert_eq!(h.flags & FLAG_TWO_STEP, FLAG_TWO_STEP);
        assert_eq!(h.control, 0);
        // Padded to the Ethernet minimum: trailing bytes past messageLength ignored.
        let mut padded = with_eth(&SYNC_VECTOR);
        padded.extend_from_slice(&[0xAA; 2]);
        assert_eq!(decode_frame(&padded).unwrap().header, h);
    }

    /// Follow_Up with the Follow_Up information TLV (802.1AS-2020 §11.4.4),
    /// preciseOriginTimestamp 1 700 000 000.000 000 123 s, correction +2.25 ns,
    /// cumulativeScaledRateOffset −182 958 735 (−83.2 ppm × 2^41).
    #[rustfmt::skip]
    pub const FOLLOW_UP_VECTOR: [u8; 76] = [
        0x18, 0x02, 0x00, 0x4C, 0x00, 0x00, 0x00, 0x08,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x40, 0x00,
        0, 0, 0, 0,
        0x00, 0x0a, 0x92, 0xff, 0xfe, 0x03, 0x3b, 0xa9, 0x00, 0x01,
        0x01, 0x02, 0x02, 0xFD,
        0x00, 0x00, 0x65, 0x53, 0xF1, 0x00, 0x00, 0x00, 0x00, 0x7B,
        0x00, 0x03, 0x00, 0x1C, 0x00, 0x80, 0xC2, 0x00, 0x00, 0x01,
        0xF5, 0x18, 0x45, 0x71,
        0x00, 0x00,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0,
    ];

    #[test]
    fn follow_up_vector() {
        let f = with_eth(&FOLLOW_UP_VECTOR);
        let m = decode_frame(&f).unwrap();
        let Body::FollowUp(fu) = m.body else { panic!() };
        assert_eq!(m.header.correction_ns(), 2.25);
        assert_eq!(m.header.sequence_id, 0x0102);
        assert_eq!(fu.precise_origin.secs, 1_700_000_000);
        assert_eq!(fu.precise_origin.nanos, 123);
        assert_eq!(fu.precise_origin.to_ns(), Some(1_700_000_000_000_000_123));
        assert_eq!(fu.info.cumulative_scaled_rate_offset, -182_958_735);
        let rr = fu.info.rate_ratio();
        assert!((rr - (1.0 - 83.2e-6)).abs() < 1e-12, "{rr}");
        // Wrong TLV subtype / org / length → Malformed.
        for (i, v) in [(53usize, 0x02u8), (50, 0x81), (47, 0x1B)] {
            let mut b = FOLLOW_UP_VECTOR;
            b[i] = v;
            assert_eq!(decode(&b), Err(DropReason::Malformed), "byte {i}");
        }
        // nanoseconds ≥ 1e9.
        let mut b = FOLLOW_UP_VECTOR;
        b[40..44].copy_from_slice(&1_000_000_000u32.to_be_bytes());
        assert_eq!(decode(&b), Err(DropReason::Malformed));
    }

    /// Announce with a one-entry path trace (802.1AS-2020 §10.6.3, §10.6.3.3.4):
    /// priority1 246, clockClass 248, accuracy 0xFE, variance 0x4100,
    /// priority2 248, stepsRemoved 0, timeSource 0xA0, logMessageInterval 0.
    #[rustfmt::skip]
    pub const ANNOUNCE_VECTOR: [u8; 76] = [
        0x1B, 0x02, 0x00, 0x4C, 0x00, 0x00, 0x00, 0x08,
        0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0,
        0x00, 0x0a, 0x92, 0xff, 0xfe, 0x03, 0x3b, 0xa9, 0x00, 0x01,
        0x00, 0x07, 0x05, 0x00,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0x00, 0x25, 0x00,
        0xF6, 0xF8, 0xFE, 0x41, 0x00, 0xF8,
        0x00, 0x0a, 0x92, 0xff, 0xfe, 0x03, 0x3b, 0xa9,
        0x00, 0x00, 0xA0,
        0x00, 0x08, 0x00, 0x08,
        0x00, 0x0a, 0x92, 0xff, 0xfe, 0x03, 0x3b, 0xa9,
    ];

    #[test]
    fn announce_vector() {
        let f = with_eth(&ANNOUNCE_VECTOR);
        let m = decode_frame(&f).unwrap();
        let Body::Announce(a) = m.body else { panic!() };
        assert_eq!(a.current_utc_offset, 37);
        assert_eq!(a.gm_priority1, 246);
        assert_eq!(
            a.gm_clock_quality,
            ClockQuality {
                clock_class: 248,
                clock_accuracy: 0xFE,
                offset_scaled_log_variance: 0x4100
            }
        );
        assert_eq!(a.gm_priority2, 248);
        assert_eq!(a.gm_identity, peer().clock);
        assert_eq!(a.steps_removed, 0);
        assert_eq!(a.time_source, 0xA0);
        assert_eq!(a.path_trace().collect::<Vec<_>>(), [peer().clock]);
        assert!(a.path_trace_contains(peer().clock));
        assert!(!a.path_trace_contains(ours().clock));
        // Path trace length not a multiple of 8.
        let mut b = ANNOUNCE_VECTOR;
        b[67] = 7;
        assert_eq!(decode(&b), Err(DropReason::Malformed));
        // No TLV at all is fine.
        let mut b = ANNOUNCE_VECTOR[..64].to_vec();
        b[3] = 64;
        let Body::Announce(a) = decode(&b).unwrap().body else {
            panic!()
        };
        assert_eq!(a.path_trace().count(), 0);
    }

    /// Signaling with a message-interval-request TLV (802.1AS-2020 §10.6.4.3):
    /// linkDelayInterval 2, timeSyncInterval −128 (no change), announce 127.
    #[rustfmt::skip]
    pub const SIGNALING_INTERVAL_VECTOR: [u8; 60] = [
        0x1C, 0x02, 0x00, 0x3C, 0x00, 0x00, 0x00, 0x08,
        0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0,
        0x00, 0x0a, 0x92, 0xff, 0xfe, 0x03, 0x3b, 0xa9, 0x00, 0x01,
        0x00, 0x03, 0x05, 0x7F,
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0x00, 0x03, 0x00, 0x0C, 0x00, 0x80, 0xC2, 0x00, 0x00, 0x02,
        0x02, 0x80, 0x7F, 0x03, 0x00, 0x00,
    ];

    #[test]
    fn signaling_interval_vector() {
        let f = with_eth(&SIGNALING_INTERVAL_VECTOR);
        let m = decode_frame(&f).unwrap();
        let Body::Signaling(s) = m.body else { panic!() };
        assert_eq!(
            s.interval_request,
            Some(IntervalRequest {
                link_delay_interval: 2,
                time_sync_interval: IntervalRequest::NO_CHANGE,
                announce_interval: IntervalRequest::STOP,
                flags: 3
            })
        );
        assert_eq!(s.gptp_capable, None);
    }

    #[test]
    fn header_validation() {
        // majorSdoId 0 (1588 default profile) and version 1.
        let mut b = SYNC_VECTOR;
        b[0] = 0x00;
        assert_eq!(decode(&b), Err(DropReason::Unsupported));
        let mut b = SYNC_VECTOR;
        b[1] = 0x01;
        assert_eq!(decode(&b), Err(DropReason::Unsupported));
        // minorVersionPTP 1 (802.1AS-2020) is accepted.
        let mut b = SYNC_VECTOR;
        b[1] = 0x12;
        assert!(decode(&b).is_ok());
        // Domain 1.
        let mut b = SYNC_VECTOR;
        b[4] = 1;
        assert_eq!(decode(&b), Err(DropReason::NotForUs));
        // messageLength below the type's size, and beyond the frame.
        let mut b = SYNC_VECTOR;
        b[3] = 43;
        assert_eq!(decode(&b), Err(DropReason::Malformed));
        let mut b = SYNC_VECTOR;
        b[3] = 45;
        assert_eq!(decode(&b), Err(DropReason::TooShort));
        // Unknown message types (Delay_Req 1, Management 0xD).
        let mut b = SYNC_VECTOR;
        b[0] = 0x11;
        assert_eq!(decode(&b), Err(DropReason::Unsupported));
        assert_eq!(decode(&SYNC_VECTOR[..20]), Err(DropReason::TooShort));
        // Wrong EtherType / destination.
        let mut f = with_eth(&SYNC_VECTOR);
        f[13] = 0xF8;
        assert_eq!(decode_frame(&f), Err(DropReason::WrongEtherType));
        let mut f = with_eth(&SYNC_VECTOR);
        f[5] = 0x0F;
        assert_eq!(decode_frame(&f), Err(DropReason::WrongDestination));
    }

    #[test]
    fn timestamp_round_trip() {
        let ns = 1_700_000_000_123_456_789u64;
        assert_eq!(Timestamp::from_ns(ns).to_ns(), Some(ns));
        assert_eq!(
            Timestamp::from_ns(1_000_000_001),
            Timestamp { secs: 1, nanos: 1 }
        );
    }
}
