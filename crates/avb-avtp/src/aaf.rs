//! AVTP Audio Format, PCM encapsulation (IEEE 1722-2016 §7.2-§7.3
//! *(verify)*).
//!
//! Header (24 B): common stream header (16 B), then
//! `format(8) | nsr(4) rsv(2) channels_per_frame(10) | bit_depth(8) |
//! stream_data_length(16) | rsv(3) sp(1) evt(4) | rsv(8)`, then
//! `stream_data_length` bytes of interleaved big-endian samples.
//!
//! [`decode`] is what the audio path uses. [`encode`] builds test vectors
//! (and the daemon's simulated talker in tests); the daemon never transmits
//! AAF (design §12).

use avb_core::DropReason;

use crate::common::{subtype, StreamHeader, STREAM_HEADER_LEN};

/// AAF header length, through the second format-specific word.
pub const AAF_HEADER_LEN: usize = 24;

/// AAF `format` field values (IEEE 1722-2016 Table 18 *(verify)*).
pub mod format {
    pub const USER: u8 = 0x00;
    pub const FLOAT_32BIT: u8 = 0x01;
    pub const INT_32BIT: u8 = 0x02;
    pub const INT_24BIT: u8 = 0x03;
    pub const INT_16BIT: u8 = 0x04;
    pub const AES3_32BIT: u8 = 0x05;

    pub fn name(f: u8) -> Option<&'static str> {
        Some(match f {
            USER => "USER",
            FLOAT_32BIT => "FLOAT_32BIT",
            INT_32BIT => "INT_32BIT",
            INT_24BIT => "INT_24BIT",
            INT_16BIT => "INT_16BIT",
            AES3_32BIT => "AES3_32BIT",
            _ => return None,
        })
    }
}

/// Bytes per sample on the wire for an AAF `format`, `None` for USER and
/// reserved values.
pub fn bytes_per_sample(fmt: u8) -> Option<usize> {
    match fmt {
        format::FLOAT_32BIT | format::INT_32BIT | format::AES3_32BIT => Some(4),
        format::INT_24BIT => Some(3),
        format::INT_16BIT => Some(2),
        _ => None,
    }
}

/// Decoded AAF PCM header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AafHeader {
    pub stream: StreamHeader,
    pub format: u8,
    /// Nominal sample rate code ([`crate::stream_format::nsr_hz`]).
    pub nsr: u8,
    pub channels_per_frame: u16,
    pub bit_depth: u8,
    pub stream_data_length: u16,
    /// Sparse timestamp mode.
    pub sp: bool,
    pub evt: u8,
}

/// An AAF PDU borrowing its sample data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AafPdu<'a> {
    pub hdr: AafHeader,
    /// Exactly `stream_data_length` bytes.
    pub data: &'a [u8],
}

/// Decode an AAF PDU (the Ethernet payload after the EtherType).
///
/// Errors: shorter than the header or than `stream_data_length` →
/// `TooShort`; subtype other than AAF, `sv = 0` or version ≠ 0 →
/// `Unsupported` (design §10.2 step 2).
pub fn decode(pdu: &[u8]) -> Result<AafPdu<'_>, DropReason> {
    let stream = StreamHeader::parse(pdu)?;
    if stream.subtype != subtype::AAF || !stream.sv || stream.version != 0 {
        return Err(DropReason::Unsupported);
    }
    let rest = pdu.get(STREAM_HEADER_LEN..).ok_or(DropReason::TooShort)?;
    let (f, data) = rest
        .split_first_chunk::<8>()
        .ok_or(DropReason::TooShort)?;
    let [fmt, b17, b18, depth, l0, l1, b22, _] = *f;
    let sdl = u16::from_be_bytes([l0, l1]);
    let data = data
        .get(..usize::from(sdl))
        .ok_or(DropReason::TooShort)?;
    Ok(AafPdu {
        hdr: AafHeader {
            stream,
            format: fmt,
            nsr: b17 >> 4,
            channels_per_frame: (u16::from(b17 & 0x03) << 8) | u16::from(b18),
            bit_depth: depth,
            stream_data_length: sdl,
            sp: b22 & 0x10 != 0,
            evt: b22 & 0x0f,
        },
        data,
    })
}

/// Encode an AAF PDU into `out`; returns its length. The header's
/// `subtype` is forced to AAF and `stream_data_length` to `data.len()`.
/// For test vectors only (design §12).
pub fn encode(hdr: &AafHeader, data: &[u8], out: &mut [u8]) -> Option<usize> {
    let sdl = u16::try_from(data.len()).ok()?;
    let total = AAF_HEADER_LEN.checked_add(data.len())?;
    let out = out.get_mut(..total)?;
    let mut sh = hdr.stream;
    sh.subtype = subtype::AAF;
    sh.encode(out)?;
    let [l0, l1] = sdl.to_be_bytes();
    let f: &mut [u8; 8] = out.get_mut(STREAM_HEADER_LEN..AAF_HEADER_LEN)?.try_into().ok()?;
    *f = [
        hdr.format,
        ((hdr.nsr & 0x0f) << 4) | ((hdr.channels_per_frame >> 8) as u8 & 0x03),
        hdr.channels_per_frame as u8,
        hdr.bit_depth,
        l0,
        l1,
        (u8::from(hdr.sp) << 4) | (hdr.evt & 0x0f),
        0,
    ];
    out.get_mut(AAF_HEADER_LEN..)?.copy_from_slice(data);
    Some(total)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use avb_core::StreamId;

    /// Hand-assembled from the 1722-2016 field layout: 48 kHz INT_32BIT
    /// depth 24, 8 channels, 6 frames (192 B), seq 7, tv=1, sp=0.
    fn vector() -> Vec<u8> {
        let mut v = vec![
            0x02, 0x81, 0x07, 0x00, // subtype, sv|tv, seq, tu=0
            0x00, 0x0A, 0x92, 0x03, 0x3B, 0xA9, 0x00, 0x00, // stream_id
            0x12, 0x34, 0x56, 0x78, // avtp_timestamp
            0x02, 0x50, 0x08, 0x18, // INT_32BIT, nsr 5, ch 8, depth 24
            0x00, 0xC0, 0x00, 0x00, // sdl 192, sp 0 evt 0
        ];
        v.extend((0..192u32).map(|i| i as u8));
        v
    }

    #[test]
    fn aaf_header_decode_vector() {
        let v = vector();
        let p = decode(&v).unwrap();
        assert_eq!(p.hdr.stream.stream_id, StreamId(0x000A_9203_3BA9_0000));
        assert_eq!(p.hdr.stream.seq, 7);
        assert!(p.hdr.stream.tv && !p.hdr.stream.mr && !p.hdr.stream.tu);
        assert_eq!(p.hdr.stream.avtp_timestamp, 0x1234_5678);
        assert_eq!(p.hdr.format, format::INT_32BIT);
        assert_eq!(p.hdr.nsr, 5);
        assert_eq!(p.hdr.channels_per_frame, 8);
        assert_eq!(p.hdr.bit_depth, 24);
        assert_eq!(p.hdr.stream_data_length, 192);
        assert!(!p.hdr.sp);
        assert_eq!(p.data.len(), 192);
        assert_eq!(p.data[191], 191);
    }

    #[test]
    fn aaf_encode_round_trip_and_bits() {
        let mut hdr = decode(&vector()).unwrap().hdr;
        hdr.channels_per_frame = 0x3FF; // all 10 bits
        hdr.sp = true;
        hdr.evt = 0x0A;
        hdr.stream.mr = true;
        hdr.stream.tu = true;
        let data = [0xAAu8; 12];
        let mut out = [0u8; 64];
        let n = encode(&hdr, &data, &mut out).unwrap();
        assert_eq!(n, 36);
        assert_eq!(out[17], 0x53);
        assert_eq!(out[18], 0xFF);
        assert_eq!(out[22], 0x1A);
        let p = decode(&out[..n]).unwrap();
        assert_eq!(p.hdr.channels_per_frame, 0x3FF);
        assert!(p.hdr.sp && p.hdr.stream.mr && p.hdr.stream.tu);
        assert_eq!(p.hdr.evt, 0x0A);
        assert_eq!(p.hdr.stream_data_length, 12);
        assert_eq!(p.data, &data);
        assert!(encode(&hdr, &data, &mut out[..35]).is_none());
    }

    #[test]
    fn aaf_rejects() {
        let v = vector();
        assert_eq!(decode(&v[..23]), Err(DropReason::TooShort));
        assert_eq!(decode(&v[..24 + 191]), Err(DropReason::TooShort));
        let mut w = v.clone();
        w[1] &= 0x7F; // sv = 0
        assert_eq!(decode(&w), Err(DropReason::Unsupported));
        let mut w = v.clone();
        w[1] |= 0x10; // version 1
        assert_eq!(decode(&w), Err(DropReason::Unsupported));
        let mut w = v;
        w[0] = 0x04; // CRF
        assert_eq!(decode(&w), Err(DropReason::Unsupported));
    }

    #[test]
    fn format_codes_pinned() {
        assert_eq!(format::USER, 0);
        assert_eq!(format::FLOAT_32BIT, 1);
        assert_eq!(format::INT_32BIT, 2);
        assert_eq!(format::INT_24BIT, 3);
        assert_eq!(format::INT_16BIT, 4);
        assert_eq!(format::AES3_32BIT, 5);
        assert_eq!(bytes_per_sample(format::INT_24BIT), Some(3));
        assert_eq!(bytes_per_sample(format::INT_32BIT), Some(4));
        assert_eq!(bytes_per_sample(format::USER), None);
        assert_eq!(format::name(2), Some("INT_32BIT"));
    }
}
