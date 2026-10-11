// SPDX-License-Identifier: MIT OR Apache-2.0

//! The 64-bit AEM stream format (IEEE 1722.1-2021 §7.3.2 *(verify)*;
//! design §10.1).
//!
//! AAF: byte 0 = `v(1)=0 | subtype(7)=0x02`; byte 1 = `ut(1) | rsv(3) |
//! nsr(4)`; byte 2 = format; byte 3 = bit_depth; then
//! `channels_per_frame(10) | samples_per_frame(10) | rsv(12)`.
//!
//! CRF (dump only): byte 0 = `v | subtype=0x04`; then `type(4) |
//! timestamp_interval(12) | timestamps_per_pdu(8) | pull(3) |
//! base_frequency(29)`.
//!
//! IEC 61883 (dump only): byte 0 = `v | subtype=0x00`; byte 1 = `sf(1) |
//! fmt(6) | r(1)`; for 61883-6 (`sf = 1`): byte 2 = `fdf_evt(5) |
//! fdf_sfc(3)`, byte 3 = dbs, byte 4 = `b nb ut sc rsv(4)`, bytes 5–7 =
//! label counts (IEC 60958, MBLA, MIDI(4)|SMPTE(4)).

use crate::aaf::format;
use crate::common::subtype;

/// AAF nominal sample rate codes (IEEE 1722-2016 Table 19 *(verify)*):
/// `(code, Hz)`. Code 0 is user-specified; 0xB–0xF are reserved.
pub const NSR_TABLE: [(u8, u32); 10] = [
    (0x1, 8_000),
    (0x2, 16_000),
    (0x3, 32_000),
    (0x4, 44_100),
    (0x5, 48_000),
    (0x6, 88_200),
    (0x7, 96_000),
    (0x8, 176_400),
    (0x9, 192_000),
    (0xA, 24_000),
];

/// nsr codes [`StreamFormat::decodable`] accepts: 44.1, 48, 88.2, 96, 176.4
/// and 192 kHz (design §10.1).
pub const BINDABLE_NSR: [u8; 6] = [0x4, 0x5, 0x6, 0x7, 0x8, 0x9];

/// Maximum channels / samples per frame `decodable()` accepts.
pub const MAX_BINDABLE_CHANNELS: u16 = 64;
pub const MAX_BINDABLE_SPF: u16 = 64;

/// Hz for an nsr code.
pub fn nsr_hz(code: u8) -> Option<u32> {
    NSR_TABLE
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, hz)| *hz)
}

/// nsr code for a rate in Hz.
pub fn nsr_code(hz: u32) -> Option<u8> {
    NSR_TABLE.iter().find(|(_, h)| *h == hz).map(|(c, _)| *c)
}

/// AAF fields of a stream format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct AafFormat {
    /// Upto ("up to") bit: channels/spf are maxima, not exact.
    pub ut: bool,
    pub nsr: u8,
    pub format: u8,
    pub bit_depth: u8,
    pub channels: u16,
    pub spf: u16,
}

impl AafFormat {
    pub fn nsr_hz(&self) -> Option<u32> {
        nsr_hz(self.nsr)
    }

    /// `channels × spf × bytes_per_sample`, the AAF `stream_data_length` of
    /// one packet (design §10.2 step 5).
    pub fn packet_bytes(&self) -> Option<usize> {
        let bps = crate::aaf::bytes_per_sample(self.format)?;
        usize::from(self.channels)
            .checked_mul(usize::from(self.spf))?
            .checked_mul(bps)
    }
}

/// CRF fields (decoded for the dump only).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrfFormat {
    pub crf_type: u8,
    pub timestamp_interval: u16,
    pub timestamps_per_pdu: u8,
    pub pull: u8,
    pub base_frequency: u32,
}

/// IEC 61883 fields (decoded for the dump only).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Iec61883Format {
    pub sf: bool,
    pub fmt: u8,
    pub fdf_evt: u8,
    pub fdf_sfc: u8,
    pub dbs: u8,
    pub label_iec_60958_cnt: u8,
    pub label_mbla_cnt: u8,
    pub label_midi_cnt: u8,
    pub label_smpte_cnt: u8,
}

/// A stream format, decoded by subtype.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decoded {
    Aaf(AafFormat),
    Crf(CrfFormat),
    Iec61883(Iec61883Format),
    /// Any other subtype (vendor-specific, video, …): raw only.
    Other {
        subtype: u8,
    },
}

/// The AEM 64-bit stream format value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamFormat(pub u64);

impl StreamFormat {
    /// The 7-bit subtype in byte 0.
    pub const fn subtype(self) -> u8 {
        ((self.0 >> 56) as u8) & 0x7F
    }

    /// The `v` bit (byte 0 bit 7): vendor-defined format when set.
    pub const fn vendor(self) -> bool {
        (self.0 >> 63) != 0
    }

    pub fn decode(self) -> Decoded {
        let [_, b1, b2, b3, b4, b5, b6, b7] = self.0.to_be_bytes();
        let lo = u32::from_be_bytes([b4, b5, b6, b7]);
        if self.vendor() {
            return Decoded::Other {
                subtype: self.subtype(),
            };
        }
        match self.subtype() {
            subtype::AAF => Decoded::Aaf(AafFormat {
                ut: b1 & 0x80 != 0,
                nsr: b1 & 0x0F,
                format: b2,
                bit_depth: b3,
                channels: (lo >> 22) as u16 & 0x3FF,
                spf: (lo >> 12) as u16 & 0x3FF,
            }),
            subtype::CRF => Decoded::Crf(CrfFormat {
                crf_type: b1 >> 4,
                timestamp_interval: (u16::from(b1 & 0x0F) << 8) | u16::from(b2),
                timestamps_per_pdu: b3,
                pull: (lo >> 29) as u8,
                base_frequency: lo & 0x1FFF_FFFF,
            }),
            subtype::IEC61883_IIDC => Decoded::Iec61883(Iec61883Format {
                sf: b1 & 0x80 != 0,
                fmt: (b1 >> 1) & 0x3F,
                fdf_evt: b2 >> 3,
                fdf_sfc: b2 & 0x07,
                dbs: b3,
                label_iec_60958_cnt: b5,
                label_mbla_cnt: b6,
                label_midi_cnt: b7 >> 4,
                label_smpte_cnt: b7 & 0x0F,
            }),
            st => Decoded::Other { subtype: st },
        }
    }

    /// The AAF fields, if this is a (non-vendor) AAF format.
    pub fn aaf(self) -> Option<AafFormat> {
        match self.decode() {
            Decoded::Aaf(a) => Some(a),
            _ => None,
        }
    }

    pub fn is_aaf(self) -> bool {
        self.aaf().is_some()
    }

    /// Encode an AAF format (reserved bits zero).
    pub fn from_aaf(a: AafFormat) -> Self {
        let lo = (u32::from(a.channels & 0x3FF) << 22) | (u32::from(a.spf & 0x3FF) << 12);
        let [b4, b5, b6, b7] = lo.to_be_bytes();
        Self(u64::from_be_bytes([
            subtype::AAF,
            (u8::from(a.ut) << 7) | (a.nsr & 0x0F),
            a.format,
            a.bit_depth,
            b4,
            b5,
            b6,
            b7,
        ]))
    }

    /// Whether the listener can bind this format (design §10.1): AAF with
    /// INT_32BIT and depth 1–32, or INT_24BIT and depth 1–24; nsr in
    /// [`BINDABLE_NSR`]; 1–64 channels; 1–64 samples per frame; `ut = 0`.
    pub fn decodable(self) -> bool {
        let Some(a) = self.aaf() else {
            return false;
        };
        let depth_ok = match a.format {
            format::INT_32BIT => (1..=32).contains(&a.bit_depth),
            format::INT_24BIT => (1..=24).contains(&a.bit_depth),
            _ => false,
        };
        depth_ok
            && !a.ut
            && BINDABLE_NSR.contains(&a.nsr)
            && (1..=MAX_BINDABLE_CHANNELS).contains(&a.channels)
            && (1..=MAX_BINDABLE_SPF).contains(&a.spf)
    }

    /// Subtype name for summaries (`"AAF"`, `"CRF"`, …, or `"0xNN"`).
    pub fn subtype_name(self) -> String {
        if self.vendor() {
            return format!("VENDOR(0x{:02x})", self.subtype());
        }
        match subtype::name(self.subtype()) {
            Some(n) => n.to_string(),
            None => format!("0x{:02x}", self.subtype()),
        }
    }
}

impl std::fmt::Display for StreamFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "0x{:016x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q6_32: u64 = 0x0205_0220_0200_6000;
    const Q6_24: u64 = 0x0205_0218_0200_6000;

    #[test]
    fn format_vector_0x0205022002006000() {
        let f = StreamFormat(Q6_32);
        let a = f.aaf().unwrap();
        assert_eq!(
            a,
            AafFormat {
                ut: false,
                nsr: 5,
                format: format::INT_32BIT,
                bit_depth: 32,
                channels: 8,
                spf: 6
            }
        );
        assert_eq!(a.nsr_hz(), Some(48_000));
        assert_eq!(a.packet_bytes(), Some(192));
        assert!(f.decodable());
        assert_eq!(StreamFormat::from_aaf(a), f);
        assert_eq!(f.subtype_name(), "AAF");
        assert_eq!(f.to_string(), "0x0205022002006000");
    }

    #[test]
    fn format_vector_0x0205021802006000() {
        let f = StreamFormat(Q6_24);
        let a = f.aaf().unwrap();
        assert_eq!(a.bit_depth, 24);
        assert_eq!(a.format, format::INT_32BIT);
        assert_eq!((a.channels, a.spf, a.nsr), (8, 6, 5));
        assert!(f.decodable());
        assert_eq!(StreamFormat::from_aaf(a), f);
    }

    #[test]
    fn nsr_table_pinned() {
        assert_eq!(nsr_hz(0), None);
        assert_eq!(nsr_hz(4), Some(44_100));
        assert_eq!(nsr_hz(5), Some(48_000));
        assert_eq!(nsr_hz(6), Some(88_200));
        assert_eq!(nsr_hz(7), Some(96_000));
        assert_eq!(nsr_hz(8), Some(176_400));
        assert_eq!(nsr_hz(9), Some(192_000));
        assert_eq!(nsr_hz(0xA), Some(24_000));
        assert_eq!(nsr_hz(0xB), None);
        assert_eq!(nsr_code(96_000), Some(7));
        assert_eq!(nsr_code(12_345), None);
    }

    fn aaf(fmt: u8, depth: u8, nsr: u8, ch: u16, spf: u16, ut: bool) -> StreamFormat {
        StreamFormat::from_aaf(AafFormat {
            ut,
            nsr,
            format: fmt,
            bit_depth: depth,
            channels: ch,
            spf,
        })
    }

    #[test]
    fn decodable_matrix() {
        use format::*;
        // (format, depth, nsr, channels, spf, ut, expected)
        let cases: &[(u8, u8, u8, u16, u16, bool, bool)] = &[
            (INT_32BIT, 32, 5, 8, 6, false, true),
            (INT_32BIT, 24, 5, 8, 6, false, true),
            (INT_32BIT, 1, 5, 8, 6, false, true),
            (INT_32BIT, 0, 5, 8, 6, false, false),
            (INT_32BIT, 33, 5, 8, 6, false, false),
            (INT_24BIT, 24, 5, 8, 6, false, true),
            (INT_24BIT, 25, 5, 8, 6, false, false),
            (INT_24BIT, 0, 5, 8, 6, false, false),
            (INT_16BIT, 16, 5, 8, 6, false, false),
            (FLOAT_32BIT, 32, 5, 8, 6, false, false),
            (AES3_32BIT, 24, 5, 8, 6, false, false),
            (USER, 32, 5, 8, 6, false, false),
            (INT_32BIT, 32, 4, 2, 6, false, true),
            (INT_32BIT, 32, 6, 2, 12, false, true),
            (INT_32BIT, 32, 7, 2, 12, false, true),
            (INT_32BIT, 32, 8, 2, 24, false, true),
            (INT_32BIT, 32, 9, 2, 24, false, true),
            (INT_32BIT, 32, 0, 2, 6, false, false),
            (INT_32BIT, 32, 1, 2, 6, false, false),
            (INT_32BIT, 32, 3, 2, 6, false, false),
            (INT_32BIT, 32, 0xA, 2, 6, false, false),
            (INT_32BIT, 32, 5, 0, 6, false, false),
            (INT_32BIT, 32, 5, 1, 6, false, true),
            (INT_32BIT, 32, 5, 64, 6, false, true),
            (INT_32BIT, 32, 5, 65, 6, false, false),
            (INT_32BIT, 32, 5, 8, 0, false, false),
            (INT_32BIT, 32, 5, 8, 1, false, true),
            (INT_32BIT, 32, 5, 8, 64, false, true),
            (INT_32BIT, 32, 5, 8, 65, false, false),
            (INT_32BIT, 32, 5, 8, 6, true, false),
        ];
        for &(f, d, n, c, s, ut, want) in cases {
            let sf = aaf(f, d, n, c, s, ut);
            assert_eq!(
                sf.decodable(),
                want,
                "{sf} fmt {f} depth {d} nsr {n} ch {c} spf {s} ut {ut}"
            );
        }
        // Non-AAF formats never bind.
        assert!(!StreamFormat(0x0410_6001_0000_BB80).decodable());
        assert!(!StreamFormat(0x00A0_0208_4000_0800).decodable());
        // A vendor-defined format with an AAF-looking subtype never binds.
        assert!(!StreamFormat(Q6_32 | (1 << 63)).decodable());
    }

    #[test]
    fn crf_and_61883_decode_for_dump() {
        // Milan CRF: audio-sample type 1, interval 96, 1 timestamp/PDU, 48 kHz.
        match StreamFormat(0x0410_6001_0000_BB80).decode() {
            Decoded::Crf(c) => {
                assert_eq!(c.crf_type, 1);
                assert_eq!(c.timestamp_interval, 96);
                assert_eq!(c.timestamps_per_pdu, 1);
                assert_eq!(c.pull, 0);
                assert_eq!(c.base_frequency, 48_000);
            }
            d => panic!("{d:?}"),
        }
        assert_eq!(StreamFormat(0x0410_6001_0000_BB80).subtype_name(), "CRF");
        // IEC 61883-6 AM824, 48 kHz (sfc 2), 8 MBLA labels.
        match StreamFormat(0x00A0_0208_4000_0800).decode() {
            Decoded::Iec61883(i) => {
                assert!(i.sf);
                assert_eq!(i.fmt, 0x10);
                assert_eq!(i.fdf_evt, 0);
                assert_eq!(i.fdf_sfc, 2);
                assert_eq!(i.dbs, 8);
                assert_eq!(i.label_mbla_cnt, 8);
            }
            d => panic!("{d:?}"),
        }
        assert_eq!(
            StreamFormat(0x7F00_0000_0000_0000).decode(),
            Decoded::Other { subtype: 0x7F }
        );
        assert_eq!(StreamFormat(0x7F00_0000_0000_0000).subtype_name(), "0x7f");
    }
}
