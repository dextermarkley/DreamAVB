//! Minimal IEEE 802.1AS / 1588v2 (layer 2) message building and parsing for
//! the Phase 0 peer-delay test. Only what G3/G4 need: build Pdelay_Req, parse
//! Pdelay_Req/Pdelay_Resp/Pdelay_Resp_Follow_Up, identify the other types.

use serde::Serialize;

pub const ETHERTYPE_PTP: u16 = 0x88F7;
pub const ETHERTYPE_VLAN: u16 = 0x8100;
/// 802.1AS peer-delay / link-local multicast destination.
pub const PDELAY_DST: [u8; 6] = [0x01, 0x80, 0xC2, 0x00, 0x00, 0x0E];
pub const ETH_HDR_LEN: usize = 14;
pub const PTP_HDR_LEN: usize = 34;
pub const PDELAY_MSG_LEN: usize = 54;
pub const PDELAY_REQ_FRAME_LEN: usize = ETH_HDR_LEN + PDELAY_MSG_LEN;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum MsgType {
    Sync,
    DelayReq,
    PdelayReq,
    PdelayResp,
    FollowUp,
    DelayResp,
    PdelayRespFollowUp,
    Announce,
    Signaling,
    Management,
    Other(u8),
}

impl MsgType {
    pub fn from_nibble(n: u8) -> Self {
        match n & 0x0f {
            0x0 => Self::Sync,
            0x1 => Self::DelayReq,
            0x2 => Self::PdelayReq,
            0x3 => Self::PdelayResp,
            0x8 => Self::FollowUp,
            0x9 => Self::DelayResp,
            0xA => Self::PdelayRespFollowUp,
            0xB => Self::Announce,
            0xC => Self::Signaling,
            0xD => Self::Management,
            x => Self::Other(x),
        }
    }

    pub fn is_event(&self) -> bool {
        matches!(
            self,
            Self::Sync | Self::DelayReq | Self::PdelayReq | Self::PdelayResp
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct PortIdentity {
    pub clock: [u8; 8],
    pub port: u16,
}

impl PortIdentity {
    /// EUI-64 clock identity from a MAC (insert FF:FE in the middle).
    pub fn from_mac(mac: [u8; 6], port: u16) -> Self {
        Self {
            clock: [mac[0], mac[1], mac[2], 0xFF, 0xFE, mac[3], mac[4], mac[5]],
            port,
        }
    }

    fn write(&self, out: &mut [u8]) {
        out[..8].copy_from_slice(&self.clock);
        out[8..10].copy_from_slice(&self.port.to_be_bytes());
    }

    fn read(b: &[u8]) -> Self {
        let mut clock = [0u8; 8];
        clock.copy_from_slice(&b[..8]);
        Self {
            clock,
            port: u16::from_be_bytes([b[8], b[9]]),
        }
    }
}

impl std::fmt::Display for PortIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, b) in self.clock.iter().enumerate() {
            if i > 0 {
                write!(f, ":")?;
            }
            write!(f, "{b:02x}")?;
        }
        write!(f, "/{}", self.port)
    }
}

/// 48-bit seconds + 32-bit nanoseconds -> ns.
pub fn ptp_ts_to_ns(b: &[u8]) -> u64 {
    let mut s = [0u8; 8];
    s[2..].copy_from_slice(&b[..6]);
    let secs = u64::from_be_bytes(s);
    let ns = u32::from_be_bytes([b[6], b[7], b[8], b[9]]);
    secs * 1_000_000_000 + u64::from(ns)
}

pub fn ns_to_ptp_ts(ns: u64) -> [u8; 10] {
    let secs = ns / 1_000_000_000;
    let sub = (ns % 1_000_000_000) as u32;
    let mut out = [0u8; 10];
    out[..6].copy_from_slice(&secs.to_be_bytes()[2..]);
    out[6..].copy_from_slice(&sub.to_be_bytes());
    out
}

/// Build an 802.1AS Pdelay_Req frame (68 bytes, no FCS; the dext pads to 60).
pub fn build_pdelay_req(src_mac: [u8; 6], seq: u16) -> Vec<u8> {
    let mut f = vec![0u8; PDELAY_REQ_FRAME_LEN];
    f[0..6].copy_from_slice(&PDELAY_DST);
    f[6..12].copy_from_slice(&src_mac);
    f[12..14].copy_from_slice(&ETHERTYPE_PTP.to_be_bytes());
    let p = &mut f[ETH_HDR_LEN..];
    p[0] = 0x12; // transportSpecific 1 (802.1AS), messageType 2 (Pdelay_Req)
    p[1] = 0x02; // versionPTP 2
    p[2..4].copy_from_slice(&(PDELAY_MSG_LEN as u16).to_be_bytes());
    p[4] = 0; // domainNumber
    p[6..8].copy_from_slice(&0x0008u16.to_be_bytes()); // flags: ptpTimescale
                                                       // correctionField [8..16] and reserved [16..20] stay zero.
    PortIdentity::from_mac(src_mac, 1).write(&mut p[20..30]);
    p[30..32].copy_from_slice(&seq.to_be_bytes());
    p[32] = 5; // controlField (other)
    p[33] = 0; // logMessageInterval (logMinPdelayReqInterval = 0 -> 1 s)
               // Body: originTimestamp (10) + reserved (10), all zero.
    f
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PtpMsg {
    pub msg_type: MsgType,
    pub transport_specific: u8,
    pub version: u8,
    pub length: u16,
    pub domain: u8,
    pub flags: u16,
    /// correctionField in units of 2^-16 ns.
    pub correction_scaled: i64,
    pub source: PortIdentity,
    pub seq: u16,
    pub control: u8,
    pub log_interval: i8,
    /// Pdelay_Resp: requestReceiptTimestamp (t2); Pdelay_Resp_Follow_Up:
    /// responseOriginTimestamp (t3); others: the originTimestamp if present.
    pub body_ts_ns: Option<u64>,
    /// Pdelay_Resp / Pdelay_Resp_Follow_Up: requestingPortIdentity.
    pub requesting: Option<PortIdentity>,
}

impl PtpMsg {
    pub fn correction_ns(&self) -> f64 {
        self.correction_scaled as f64 / 65536.0
    }
}

/// Locate the PTP payload in an Ethernet frame (optionally one VLAN tag).
pub fn ptp_payload(frame: &[u8]) -> Option<&[u8]> {
    if frame.len() < ETH_HDR_LEN {
        return None;
    }
    let mut et = u16::from_be_bytes([frame[12], frame[13]]);
    let mut off = ETH_HDR_LEN;
    if et == ETHERTYPE_VLAN {
        if frame.len() < ETH_HDR_LEN + 4 {
            return None;
        }
        et = u16::from_be_bytes([frame[16], frame[17]]);
        off += 4;
    }
    if et != ETHERTYPE_PTP {
        return None;
    }
    Some(&frame[off..])
}

pub fn parse_frame(frame: &[u8]) -> Option<PtpMsg> {
    parse_ptp(ptp_payload(frame)?)
}

pub fn parse_ptp(p: &[u8]) -> Option<PtpMsg> {
    if p.len() < PTP_HDR_LEN {
        return None;
    }
    let msg_type = MsgType::from_nibble(p[0]);
    let length = u16::from_be_bytes([p[2], p[3]]);
    let body = &p[PTP_HDR_LEN..];
    let (body_ts_ns, requesting) = match msg_type {
        MsgType::PdelayResp | MsgType::PdelayRespFollowUp => {
            if body.len() < 20 {
                return None;
            }
            (
                Some(ptp_ts_to_ns(&body[..10])),
                Some(PortIdentity::read(&body[10..20])),
            )
        }
        MsgType::Sync | MsgType::FollowUp | MsgType::PdelayReq | MsgType::Announce
            if body.len() >= 10 =>
        {
            (Some(ptp_ts_to_ns(&body[..10])), None)
        }
        _ => (None, None),
    };
    Some(PtpMsg {
        msg_type,
        transport_specific: p[0] >> 4,
        version: p[1] & 0x0f,
        length,
        domain: p[4],
        flags: u16::from_be_bytes([p[6], p[7]]),
        correction_scaled: i64::from_be_bytes([
            p[8], p[9], p[10], p[11], p[12], p[13], p[14], p[15],
        ]),
        source: PortIdentity::read(&p[20..30]),
        seq: u16::from_be_bytes([p[30], p[31]]),
        control: p[32],
        log_interval: p[33] as i8,
        body_ts_ns,
        requesting,
    })
}

/// Two-step peer delay: ((t4 - t1) - (t3 - t2) - corrections) / 2, in ns.
/// t1 = our TX of Pdelay_Req, t2 = peer RX (Pdelay_Resp body),
/// t3 = peer TX of Pdelay_Resp (Follow_Up body), t4 = our RX of Pdelay_Resp.
pub fn link_delay_ns(
    t1: u64,
    t2: u64,
    t3: u64,
    t4: u64,
    corr_resp_ns: f64,
    corr_fu_ns: f64,
) -> f64 {
    let local = t4 as i128 - t1 as i128;
    let remote = t3 as i128 - t2 as i128;
    ((local - remote) as f64 - corr_resp_ns - corr_fu_ns) / 2.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];

    #[test]
    fn pdelay_req_golden_bytes() {
        let f = build_pdelay_req(MAC, 0x1234);
        #[rustfmt::skip]
        let expected: [u8; 68] = [
            // Ethernet
            0x01, 0x80, 0xC2, 0x00, 0x00, 0x0E, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x88, 0xF7,
            // PTP header
            0x12, 0x02, 0x00, 0x36, 0x00, 0x00, 0x00, 0x08,
            0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0,
            0x00, 0x11, 0x22, 0xFF, 0xFE, 0x33, 0x44, 0x55, 0x00, 0x01,
            0x12, 0x34, 0x05, 0x00,
            // originTimestamp + reserved
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(f.len(), 68);
        assert_eq!(f.as_slice(), &expected[..]);
    }

    #[test]
    fn pdelay_req_round_trip() {
        let m = parse_frame(&build_pdelay_req(MAC, 7)).unwrap();
        assert_eq!(m.msg_type, MsgType::PdelayReq);
        assert_eq!(m.transport_specific, 1);
        assert_eq!(m.version, 2);
        assert_eq!(m.length, 54);
        assert_eq!(m.seq, 7);
        assert_eq!(m.source, PortIdentity::from_mac(MAC, 1));
        assert_eq!(m.flags, 0x0008);
        assert!(m.msg_type.is_event());
    }

    #[test]
    fn timestamp_round_trip() {
        let ns = 1_700_000_000_123_456_789u64;
        assert_eq!(ptp_ts_to_ns(&ns_to_ptp_ts(ns)), ns);
        assert_eq!(ns_to_ptp_ts(1_000_000_001), [0, 0, 0, 0, 0, 1, 0, 0, 0, 1]);
    }

    fn resp_frame(
        msg_type: u8,
        seq: u16,
        ts_ns: u64,
        corr_ns: i64,
        requester: PortIdentity,
    ) -> Vec<u8> {
        let peer = [0x00, 0x0a, 0x92, 0x01, 0x02, 0x03];
        let mut f = build_pdelay_req(peer, seq);
        let p = &mut f[ETH_HDR_LEN..];
        p[0] = 0x10 | msg_type;
        p[8..16].copy_from_slice(&(corr_ns << 16).to_be_bytes());
        p[34..44].copy_from_slice(&ns_to_ptp_ts(ts_ns));
        requester.write(&mut p[44..54]);
        f
    }

    #[test]
    fn pdelay_exchange_gives_known_delay() {
        let me = PortIdentity::from_mac(MAC, 1);
        // Link delay 300 ns, peer turnaround 10 us, peer clock offset +5 s.
        let t1 = 1_000_000_000u64;
        let t2 = t1 + 300 + 5_000_000_000;
        let t3 = t2 + 10_000;
        let t4 = t1 + 300 + 10_000 + 300;
        let resp = parse_frame(&resp_frame(0x3, 42, t2, 0, me)).unwrap();
        let fu = parse_frame(&resp_frame(0xA, 42, t3, 0, me)).unwrap();
        assert_eq!(resp.msg_type, MsgType::PdelayResp);
        assert_eq!(fu.msg_type, MsgType::PdelayRespFollowUp);
        assert_eq!(resp.requesting, Some(me));
        assert_eq!(fu.seq, 42);
        let d = link_delay_ns(
            t1,
            resp.body_ts_ns.unwrap(),
            fu.body_ts_ns.unwrap(),
            t4,
            resp.correction_ns(),
            fu.correction_ns(),
        );
        assert_eq!(d, 300.0);
        // A 100 ns correction in the follow-up shortens the result by 50 ns.
        let fu2 = parse_frame(&resp_frame(0xA, 42, t3, 100, me)).unwrap();
        assert_eq!(fu2.correction_ns(), 100.0);
        let d2 = link_delay_ns(t1, t2, t3, t4, 0.0, fu2.correction_ns());
        assert_eq!(d2, 250.0);
    }

    #[test]
    fn vlan_tagged_and_non_ptp_frames() {
        let mut f = build_pdelay_req(MAC, 1);
        let mut tagged = f[..12].to_vec();
        tagged.extend_from_slice(&[0x81, 0x00, 0x60, 0x02]);
        tagged.extend_from_slice(&f[12..]);
        assert_eq!(parse_frame(&tagged).unwrap().seq, 1);
        f[12] = 0x08;
        f[13] = 0x00;
        assert!(parse_frame(&f).is_none());
        assert!(parse_frame(&[0u8; 10]).is_none());
    }

    #[test]
    fn msg_type_identification() {
        assert_eq!(MsgType::from_nibble(0x0), MsgType::Sync);
        assert_eq!(MsgType::from_nibble(0x8), MsgType::FollowUp);
        assert_eq!(MsgType::from_nibble(0xB), MsgType::Announce);
        assert_eq!(MsgType::from_nibble(0x1B), MsgType::Announce);
        assert_eq!(MsgType::from_nibble(0x5), MsgType::Other(5));
        assert!(!MsgType::Announce.is_event());
    }
}
