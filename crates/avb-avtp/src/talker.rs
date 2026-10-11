//! AAF talker packetizer (Phase 1b-2, the output/playback direction).
//!
//! The mirror of the listener decode path: given a negotiated [`AafFormat`] and
//! a stream id, [`AafTalker::packetize`] turns one PDU's worth of interleaved
//! **S32LE** container samples (as the output ring stores them) into a complete
//! AAF AVTPDU — common stream header + AAF header + big-endian sample payload —
//! ready to prepend an Ethernet/VLAN header and transmit.
//!
//! Clocking is the caller's job: the daemon's talker loop calls `packetize`
//! once per `spf`-frame tick of the media clock, passing the `avtp_timestamp`
//! (gPTP presentation time mod 2^32 ns) for that PDU. This module holds only
//! the per-stream sequence counter and the immutable format, and allocates
//! nothing after construction.
//!
//! Wire byte order: AAF carries big-endian samples (IEEE 1722-2016); the ring
//! stores little-endian S32. `packetize` byte-swaps LE→BE per sample, the exact
//! inverse of [`crate::aaf::decode`]'s BE→LE on the listener side.

use crate::aaf::{self, AafHeader};
use crate::common::StreamHeader;
use crate::stream_format::AafFormat;
use avb_core::StreamId;

/// Per-stream AAF talker state: immutable format + stream id, plus the running
/// sequence number. One per transmitted stream.
#[derive(Clone, Debug)]
pub struct AafTalker {
    stream_id: StreamId,
    fmt: AafFormat,
    seq: u8,
    /// Container bytes per sample on the wire (4 for INT_32BIT).
    wire_bps: usize,
    /// Interleaved samples per PDU = channels × spf.
    samples_per_pdu: usize,
    /// AAF `stream_data_length` for a full PDU = samples_per_pdu × wire_bps.
    pdu_data_len: usize,
}

/// Why a packetize call produced no PDU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TalkerError {
    /// The format is not a transmittable AAF format (bad bytes-per-sample).
    UnsupportedFormat,
    /// `samples` is not exactly `channels × spf` S32 samples (4 B each).
    BadSampleCount,
    /// `out` is too small for the full PDU.
    OutTooSmall,
}

impl AafTalker {
    /// Build a talker for a negotiated AAF format. Returns `None` if the
    /// format's wire sample size is unknown (e.g. USER / reserved).
    pub fn new(stream_id: StreamId, fmt: AafFormat) -> Option<Self> {
        let wire_bps = aaf::bytes_per_sample(fmt.format)?;
        let samples_per_pdu = usize::from(fmt.channels).checked_mul(usize::from(fmt.spf))?;
        let pdu_data_len = samples_per_pdu.checked_mul(wire_bps)?;
        Some(Self {
            stream_id,
            fmt,
            seq: 0,
            wire_bps,
            samples_per_pdu,
            pdu_data_len,
        })
    }

    /// Total AVTPDU length a full PDU occupies (header + payload).
    pub fn pdu_len(&self) -> usize {
        aaf::AAF_HEADER_LEN + self.pdu_data_len
    }

    /// The AAF `stream_data_length` of a full PDU.
    pub fn data_len(&self) -> usize {
        self.pdu_data_len
    }

    /// The current sequence number (next PDU's `sequence_num`).
    pub fn seq(&self) -> u8 {
        self.seq
    }

    /// Build one AAF AVTPDU into `out` from `samples_le` (interleaved S32LE,
    /// exactly `channels × spf × 4` bytes). `avtp_timestamp` is the gPTP
    /// presentation time (ns mod 2^32) for this PDU; `tv` marks it valid.
    /// Returns the PDU length written. Advances the sequence number on success.
    ///
    /// `samples_le` comes straight from the output ring (LE); the bytes are
    /// swapped to the AAF wire's big-endian order per sample.
    pub fn packetize(
        &mut self,
        samples_le: &[u8],
        avtp_timestamp: u32,
        tv: bool,
        out: &mut [u8],
    ) -> Result<usize, TalkerError> {
        if self.wire_bps == 0 {
            return Err(TalkerError::UnsupportedFormat);
        }
        // The ring container is S32 (4 B); the wire sample size may be smaller
        // (INT_24BIT = 3 B wire). We take the top `wire_bps` bytes of each
        // big-endian sample, matching how the listener left-justifies on
        // decode. Require exactly `samples_per_pdu` S32 input samples.
        if samples_le.len() != self.samples_per_pdu.saturating_mul(4) {
            return Err(TalkerError::BadSampleCount);
        }
        let total = self.pdu_len();
        if out.len() < total {
            return Err(TalkerError::OutTooSmall);
        }

        let hdr = AafHeader {
            stream: StreamHeader {
                subtype: crate::common::subtype::AAF,
                sv: true,
                version: 0,
                mr: false,
                tv,
                seq: self.seq,
                tu: false,
                stream_id: self.stream_id,
                avtp_timestamp,
            },
            format: self.fmt.format,
            nsr: self.fmt.nsr,
            channels_per_frame: self.fmt.channels,
            bit_depth: self.fmt.bit_depth,
            stream_data_length: 0, // encode() overwrites with data.len()
            sp: false,
            evt: 0,
        };

        // Build the big-endian payload in a fixed on-stack scratch, then encode.
        // Max PDU payload is 64 ch × 64 spf × 4 B = 16384 B; cap the scratch at
        // the Milan-relevant max we actually emit (8 ch × 12 spf × 4 = 384 B is
        // typical, but allow up to the ABI bindable max).
        let mut scratch = [0u8; MAX_PDU_PAYLOAD];
        let payload = scratch
            .get_mut(..self.pdu_data_len)
            .ok_or(TalkerError::OutTooSmall)?;
        self.swap_le_to_be(samples_le, payload);

        let n = aaf::encode(&hdr, payload, out).ok_or(TalkerError::OutTooSmall)?;
        self.seq = self.seq.wrapping_add(1);
        Ok(n)
    }

    /// Convert interleaved S32LE container samples to the wire's big-endian
    /// order, taking the top `wire_bps` bytes of each sample (left-justified,
    /// the inverse of the listener's `read_be_iNN_*`).
    fn swap_le_to_be(&self, src_le: &[u8], dst_be: &mut [u8]) {
        let bps = self.wire_bps.min(4);
        let (chunks, _rem) = src_le.as_chunks::<4>();
        for (i, le) in chunks.iter().enumerate() {
            // le = [b0, b1, b2, b3] little-endian S32; big-endian is reversed.
            // Wire takes the HIGH `bps` bytes of the BE sample, i.e. the top
            // `bps` of [b3, b2, b1, b0].
            let be = [le[3], le[2], le[1], le[0]];
            let dst_off = i * bps;
            if let (Some(slot), Some(src)) = (dst_be.get_mut(dst_off..dst_off + bps), be.get(..bps))
            {
                slot.copy_from_slice(src);
            }
        }
    }
}

/// Max AAF PDU payload we build on-stack: ABI bindable max 64 ch × 64 spf × 4 B.
const MAX_PDU_PAYLOAD: usize = 64 * 64 * 4;

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::aaf;
    use crate::stream_format::StreamFormat;

    fn q6_48k_8ch_32() -> AafFormat {
        // 0x0205022002006000: nsr 5 (48k), INT_32BIT, depth 32, 8 ch, 6 spf.
        StreamFormat(0x0205_0220_0200_6000).aaf().unwrap()
    }

    #[test]
    fn packetize_roundtrips_through_decode() {
        let sid = StreamId::from_be_bytes([0x00, 0x0a, 0x92, 0x03, 0x3b, 0xa9, 0x00, 0x01]);
        let fmt = q6_48k_8ch_32();
        let mut t = AafTalker::new(sid, fmt).unwrap();
        assert_eq!(t.data_len(), 8 * 6 * 4); // 192
        assert_eq!(t.pdu_len(), aaf::AAF_HEADER_LEN + 192);

        // 8 ch × 6 spf S32LE samples: value = frame*100 + ch, as LE i32.
        let n = 8 * 6;
        let mut samples = vec![0u8; n * 4];
        for fr in 0..6 {
            for ch in 0..8 {
                let v = (fr as i32) * 100 + ch as i32;
                let off = (fr * 8 + ch) * 4;
                samples[off..off + 4].copy_from_slice(&v.to_le_bytes());
            }
        }

        let mut out = vec![0u8; t.pdu_len()];
        let len = t.packetize(&samples, 0x1234_5678, true, &mut out).unwrap();
        assert_eq!(len, t.pdu_len());
        assert_eq!(t.seq(), 1); // advanced

        // Decode it back and check the header + that the samples survive.
        let pdu = aaf::decode(&out[..len]).unwrap();
        assert_eq!(pdu.hdr.stream.subtype, crate::common::subtype::AAF);
        assert!(pdu.hdr.stream.sv);
        assert!(pdu.hdr.stream.tv);
        assert_eq!(pdu.hdr.stream.seq, 0);
        assert_eq!(pdu.hdr.stream.stream_id, sid);
        assert_eq!(pdu.hdr.stream.avtp_timestamp, 0x1234_5678);
        assert_eq!(pdu.hdr.format, aaf::format::INT_32BIT);
        assert_eq!(pdu.hdr.channels_per_frame, 8);
        assert_eq!(pdu.hdr.bit_depth, 32);
        assert_eq!(pdu.hdr.stream_data_length as usize, 192);

        // Wire payload is big-endian S32; reconstruct and compare to inputs.
        for fr in 0..6 {
            for ch in 0..8 {
                let want = (fr as i32) * 100 + ch as i32;
                let off = (fr * 8 + ch) * 4;
                let be = &pdu.data[off..off + 4];
                let got = i32::from_be_bytes([be[0], be[1], be[2], be[3]]);
                assert_eq!(got, want, "fr {fr} ch {ch}");
            }
        }
    }

    #[test]
    fn seq_wraps_at_256() {
        let sid = StreamId(0);
        let mut t = AafTalker::new(sid, q6_48k_8ch_32()).unwrap();
        let samples = vec![0u8; 8 * 6 * 4];
        let mut out = vec![0u8; t.pdu_len()];
        for i in 0..257u32 {
            t.packetize(&samples, i, true, &mut out).unwrap();
        }
        assert_eq!(t.seq(), 1); // 257 PDUs sent, 257 mod 256 = 1
    }

    #[test]
    fn bad_sample_count_rejected() {
        let sid = StreamId(0);
        let mut t = AafTalker::new(sid, q6_48k_8ch_32()).unwrap();
        let mut out = vec![0u8; t.pdu_len()];
        let short = vec![0u8; 8 * 6 * 4 - 4];
        assert_eq!(
            t.packetize(&short, 0, true, &mut out),
            Err(TalkerError::BadSampleCount)
        );
    }

    #[test]
    fn out_too_small_rejected() {
        let sid = StreamId(0);
        let mut t = AafTalker::new(sid, q6_48k_8ch_32()).unwrap();
        let samples = vec![0u8; 8 * 6 * 4];
        let mut tiny = vec![0u8; 10];
        assert_eq!(
            t.packetize(&samples, 0, true, &mut tiny),
            Err(TalkerError::OutTooSmall)
        );
    }

    #[test]
    fn int24_takes_top_three_bytes() {
        // INT_24BIT: wire is 3 B/sample, taken from the top of the BE sample.
        let sid = StreamId(0);
        let fmt = AafFormat {
            ut: false,
            nsr: 5,
            format: aaf::format::INT_24BIT,
            bit_depth: 24,
            channels: 1,
            spf: 1,
        };
        let mut t = AafTalker::new(sid, fmt).unwrap();
        assert_eq!(t.data_len(), 3);
        // S32LE 0x11223344 -> BE 0x11223344 -> top 3 bytes 0x11,0x22,0x33.
        let samples = 0x1122_3344i32.to_le_bytes().to_vec();
        let mut out = vec![0u8; t.pdu_len()];
        let len = t.packetize(&samples, 0, true, &mut out).unwrap();
        let pdu = aaf::decode(&out[..len]).unwrap();
        assert_eq!(pdu.data, &[0x11, 0x22, 0x33]);
    }
}
