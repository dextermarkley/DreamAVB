//! AVTP EtherType, subtype codes and the common stream header
//! (IEEE 1722-2016 §4.4).

use avb_core::{DropReason, StreamId};

/// AVTP EtherType (IEEE 1722-2016 §4.4.1 *(verify)*).
pub const ETHERTYPE_AVTP: u16 = 0x22F0;

/// AVTP subtype values (IEEE 1722-2016 Table 6 *(verify)*). Only the ones
/// this project decodes or classifies are listed.
pub mod subtype {
    /// IEC 61883 / IIDC stream.
    pub const IEC61883_IIDC: u8 = 0x00;
    /// AVTP Audio Format.
    pub const AAF: u8 = 0x02;
    /// Clock Reference Format.
    pub const CRF: u8 = 0x04;
    /// AVDECC Discovery Protocol (IEEE 1722.1-2021 §6).
    pub const ADP: u8 = 0xFA;
    /// AVDECC Enumeration and Control Protocol (IEEE 1722.1-2021 §9).
    pub const AECP: u8 = 0xFB;
    /// AVDECC Connection Management Protocol (IEEE 1722.1-2021 §8).
    pub const ACMP: u8 = 0xFC;
    /// MAC Address Acquisition Protocol (IEEE 1722-2016 Annex B).
    pub const MAAP: u8 = 0xFE;

    /// Stable name for logs and the AEM dump.
    pub fn name(st: u8) -> Option<&'static str> {
        Some(match st {
            IEC61883_IIDC => "IEC61883_IIDC",
            AAF => "AAF",
            CRF => "CRF",
            ADP => "ADP",
            AECP => "AECP",
            ACMP => "ACMP",
            MAAP => "MAAP",
            _ => return None,
        })
    }
}

/// Bytes of the common stream header through `avtp_timestamp`.
pub const STREAM_HEADER_LEN: usize = 16;

/// The common AVTP stream header (IEEE 1722-2016 §4.4.4 *(verify)*):
/// `subtype(8) | sv(1) version(3) mr(1) fsd(2) tv(1) | sequence_num(8) |
/// fsd(7) tu(1) | stream_id(64) | avtp_timestamp(32)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamHeader {
    pub subtype: u8,
    /// stream_id valid.
    pub sv: bool,
    pub version: u8,
    /// Media clock restart.
    pub mr: bool,
    /// avtp_timestamp valid.
    pub tv: bool,
    pub seq: u8,
    /// Timestamp uncertain.
    pub tu: bool,
    pub stream_id: StreamId,
    /// gPTP time mod 2^32 ns.
    pub avtp_timestamp: u32,
}

impl StreamHeader {
    /// Parse the first [`STREAM_HEADER_LEN`] bytes of an AVTPDU. Bits this
    /// header leaves format-specific are ignored here.
    pub fn parse(pdu: &[u8]) -> Result<Self, DropReason> {
        let h: &[u8; STREAM_HEADER_LEN] = pdu.first_chunk().ok_or(DropReason::TooShort)?;
        let [st, b1, seq, b3, s0, s1, s2, s3, s4, s5, s6, s7, t0, t1, t2, t3] = *h;
        Ok(Self {
            subtype: st,
            sv: b1 & 0x80 != 0,
            version: (b1 >> 4) & 0x07,
            mr: b1 & 0x08 != 0,
            tv: b1 & 0x01 != 0,
            seq,
            tu: b3 & 0x01 != 0,
            stream_id: StreamId::from_be_bytes([s0, s1, s2, s3, s4, s5, s6, s7]),
            avtp_timestamp: u32::from_be_bytes([t0, t1, t2, t3]),
        })
    }

    /// Encode into `out[..16]` with the format-specific bits zero.
    pub fn encode(&self, out: &mut [u8]) -> Option<()> {
        let h: &mut [u8; STREAM_HEADER_LEN] = out.first_chunk_mut()?;
        let [s0, s1, s2, s3, s4, s5, s6, s7] = self.stream_id.to_be_bytes();
        let [t0, t1, t2, t3] = self.avtp_timestamp.to_be_bytes();
        let b1 = (u8::from(self.sv) << 7)
            | ((self.version & 0x07) << 4)
            | (u8::from(self.mr) << 3)
            | u8::from(self.tv);
        *h = [
            self.subtype,
            b1,
            self.seq,
            u8::from(self.tu),
            s0,
            s1,
            s2,
            s3,
            s4,
            s5,
            s6,
            s7,
            t0,
            t1,
            t2,
            t3,
        ];
        Some(())
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn subtype_codes_pinned() {
        assert_eq!(ETHERTYPE_AVTP, 0x22F0);
        assert_eq!(subtype::IEC61883_IIDC, 0x00);
        assert_eq!(subtype::AAF, 0x02);
        assert_eq!(subtype::CRF, 0x04);
        assert_eq!(subtype::ADP, 0xFA);
        assert_eq!(subtype::AECP, 0xFB);
        assert_eq!(subtype::ACMP, 0xFC);
        assert_eq!(subtype::MAAP, 0xFE);
        assert_eq!(subtype::name(0x02), Some("AAF"));
        assert_eq!(subtype::name(0x7F), None);
    }

    #[test]
    fn stream_header_vector() {
        // sv=1 version 0 mr=1 tv=1, seq 0x2A, tu=1.
        let b = [
            0x02, 0x89, 0x2A, 0x01, 0x00, 0x0A, 0x92, 0x03, 0x3B, 0xA9, 0x00, 0x01, 0xDE, 0xAD,
            0xBE, 0xEF,
        ];
        let h = StreamHeader::parse(&b).unwrap();
        assert_eq!(h.subtype, 0x02);
        assert!(h.sv && h.mr && h.tv && h.tu);
        assert_eq!(h.version, 0);
        assert_eq!(h.seq, 0x2A);
        assert_eq!(h.stream_id, StreamId(0x000A_9203_3BA9_0001));
        assert_eq!(h.avtp_timestamp, 0xDEAD_BEEF);
        let mut out = [0u8; 16];
        h.encode(&mut out).unwrap();
        assert_eq!(out, b);
        assert_eq!(
            StreamHeader::parse(&b[..15]),
            Err(DropReason::TooShort),
            "short header"
        );
    }
}
