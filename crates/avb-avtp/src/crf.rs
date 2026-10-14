// SPDX-License-Identifier: MIT OR Apache-2.0

//! CRF (Clock Reference Format) AVTPDU — IEEE 1722-2016 Clause 10 — with the
//! Milan v1.3 §7.3.1 parameters, for distributing a media clock as a stream.
//!
//! # Where this layout comes from
//!
//! Milan only *references* `[AVTP, Clause 10]` and never restates the PDU, so
//! the octet layout below is not transcribed from the Milan text. This project
//! forbids originating a bit-packed wire layout from a single reading (a
//! guessed CoreAudio FourCC once passed its own round-trip tests and failed
//! only against hardware), so it is corroborated by three independent sources:
//!
//! 1. **libavtp** (`avtp_crf.h` / `crf.c`, Avnu/Intel) declares the PDU as
//!    `subtype_data(4) + stream_id(8) + packet_info(8) + crf_data[]`, and sets
//!    `crf_data_length` to `sizeof(uint64_t) * timestamps` — i.e. **octets**,
//!    not a timestamp count.
//! 2. **NVIDIA DriveOS** `nvavtp.h`'s `tagNvAvtpCRFHeader` bitfields decode
//!    MSB-first to the identical octet positions, and independently name byte
//!    1 bit 2 as reserved.
//! 3. **Milan's own bandwidth arithmetic**, which is pure arithmetic from an
//!    unrelated table and so corroborates the *size* without reference to
//!    either implementation: v1.3 Table 4.4 gives `CRF, 1 ts/pdu` a
//!    MaxFrameSize of 28, and [`milan::bandwidth_bps`] reproduces the
//!    published 5696 kbps from it. A 20-octet header plus one 8-octet
//!    timestamp is the only split that yields 28.
//!
//! The Milan parameters themselves need no guessing: v1.3 Table 7.1 gives the
//! ATDECC format string as `0x041060010000BB80`, which is byte-for-byte the
//! `current_format` the PreSonus 16R advertises on its own `STREAM_INPUT 0`
//! ("Return CRF"), captured in `avb-avdecc`'s descriptor tests. Spec and
//! device agree independently.
//!
//! # Layout
//!
//! ```text
//!   0      subtype = 0x04
//!   1      sv(1) | version(3) | mr(1) | r(1) | fs(1) | tu(1)
//!   2      sequence_num
//!   3      type                      (1 = CRF_AUDIO_SAMPLE)
//!   4-11   stream_id
//!   12-15  pull(3) | base_frequency(29)
//!   16-17  crf_data_length           OCTETS, = 8 × timestamps
//!   18-19  timestamp_interval
//!   20..   crf_data: N × big-endian u64 gPTP nanosecond timestamps
//! ```
//!
//! Bytes 12–15 use the **same** 32-bit `pull | base_frequency` packing as the
//! low word of the 64-bit AEM stream format, which
//! [`tests::milan_pdu_and_aem_format_agree`] exploits as a cross-layer check.
//!
//! # What this module deliberately does not decide
//!
//! A CRF timestamp is the gPTP time of a media-clock event, one per
//! `timestamp_interval` ticks of `base_frequency`. **The caller supplies it.**
//! Milan v1.3 imposes no presentation-offset formula on a CRF talker — it
//! contains no such equation — so any transit offset is the daemon's policy
//! and is documented there rather than being smuggled in here behind a
//! spec citation.

use crate::common::subtype;
use avb_core::{DropReason, StreamId};

/// Octets of the CRF header before `crf_data`.
pub const CRF_HEADER_LEN: usize = 20;

/// Octets of one CRF timestamp.
pub const TIMESTAMP_LEN: usize = 8;

/// CRF `type` values (IEEE 1722-2016 Table 26 *(verify)*). Only the ones this
/// project encodes or classifies are named.
pub mod crf_type {
    /// User-specified.
    pub const USER: u8 = 0x00;
    /// Audio sample clock — the only type Milan uses.
    pub const AUDIO_SAMPLE: u8 = 0x01;
    /// Video frame sync.
    pub const VIDEO_FRAME: u8 = 0x02;
    /// Video line sync.
    pub const VIDEO_LINE: u8 = 0x03;
    /// Machine cycle.
    pub const MACHINE_CYCLE: u8 = 0x04;

    /// Stable name for logs and the AEM dump.
    pub fn name(t: u8) -> Option<&'static str> {
        Some(match t {
            USER => "USER",
            AUDIO_SAMPLE => "AUDIO_SAMPLE",
            VIDEO_FRAME => "VIDEO_FRAME",
            VIDEO_LINE => "VIDEO_LINE",
            MACHINE_CYCLE => "MACHINE_CYCLE",
            _ => return None,
        })
    }
}

/// The Milan Clock Reference Format parameters (Milan v1.3 §7.3.1, Table 7.1)
/// and the Optional format (§7.3.2, Table 7.2).
pub mod milan {
    use super::{CRF_HEADER_LEN, TIMESTAMP_LEN};

    /// ATDECC format string of the mandatory Milan Clock Reference Format
    /// (Table 7.1). Identical to the 16R's own `STREAM_INPUT 0`
    /// `current_format`.
    pub const FORMAT: u64 = 0x0410_6001_0000_BB80;

    /// ATDECC format string of the Optional Milan Clock Reference Format
    /// (Table 7.2), for the 44.1 kHz family.
    ///
    /// Note the 16R advertises `0x0410_9301_0000_AC44` for its optional
    /// format — `timestamp_interval` 147 where Table 7.2 requires 96. We
    /// transmit only [`FORMAT`], so the deviation does not affect us; it is
    /// recorded because it shows the device's own optional format is
    /// non-conformant and must not be used as an anchor.
    pub const OPTIONAL_FORMAT: u64 = 0x0410_6001_0000_AC44;

    /// `type` = CRF_AUDIO_SAMPLE.
    pub const TYPE: u8 = super::crf_type::AUDIO_SAMPLE;
    /// Media-clock events between consecutive timestamps.
    pub const TIMESTAMP_INTERVAL: u16 = 96;
    /// Timestamps carried in one PDU.
    pub const TIMESTAMPS_PER_PDU: u16 = 1;
    /// `pull` = 0, i.e. `base_frequency` is used as-is.
    pub const PULL: u8 = 0;
    /// Nominal media clock frequency in Hz.
    pub const BASE_FREQUENCY: u32 = 48_000;
    /// `base_frequency` of the optional 44.1 kHz family.
    pub const OPTIONAL_BASE_FREQUENCY: u32 = 44_100;

    /// `crf_data_length` for a Milan PDU, in OCTETS.
    pub const CRF_DATA_LENGTH: u16 = TIMESTAMPS_PER_PDU * TIMESTAMP_LEN as u16;

    /// Total AVTPDU octets: 20-octet header + one 8-octet timestamp. Equal to
    /// the MaxFrameSize of 28 in v1.3 Table 4.4, which is what corroborates
    /// the header length.
    pub const PDU_LEN: usize = CRF_HEADER_LEN + TIMESTAMP_LEN * TIMESTAMPS_PER_PDU as usize;

    /// MSRP `MaxIntervalFrames` for a class A CRF stream (Table 4.4).
    pub const MAX_INTERVAL_FRAMES: u16 = 1;

    /// PDUs per second = `base_frequency / timestamp_interval / ts_per_pdu`,
    /// i.e. 500 for Milan — one every 2 ms.
    pub const fn pdus_per_sec() -> u32 {
        BASE_FREQUENCY / TIMESTAMP_INTERVAL as u32 / TIMESTAMPS_PER_PDU as u32
    }

    /// Bandwidth in bit/s occupied by a CRF stream, by Milan v1.3 Table 4.4's
    /// own five-step rule, for `max_frame_size` AVTPDU octets.
    ///
    /// The step order is load-bearing and is reproduced as written: v1.3 adds
    /// the clock-tolerance byte in step 3, **after** the 68-octet minimum-frame
    /// clamp in step 2, so for a sub-minimum frame the byte escapes the clamp
    /// and lands in the result. That is why v1.3 publishes 5696 kbps for CRF
    /// while Milan v2.0a — which folds the same byte into MaxFrameSize, ahead
    /// of the clamp, and so has it absorbed — publishes 5632 kbps for a stream
    /// on the wire that is byte-for-byte identical.
    pub const fn bandwidth_bps(max_frame_size: u16, max_interval_frames: u16) -> u32 {
        // 1) Ethernet header (with VLAN tag) and FCS.
        let mut f = max_frame_size as u32 + 22;
        // 2) Not less than a minimum-sized Ethernet frame.
        if f < 68 {
            f = 68;
        }
        // 3) One more byte for a PAAD sampling clock slightly fast.
        f += 1;
        // 4) Preamble (8) and inter-packet gap (12).
        let w = f + 20;
        // 5) Per 125 µs interval, then per second, then to bits.
        w * max_interval_frames as u32 * 8000 * 8
    }
}

/// Why a CRF PDU could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrfError {
    /// `out` is too small for the header plus timestamps.
    OutTooSmall,
    /// The timestamp count does not match the header's `crf_data_length`.
    BadTimestampCount,
}

/// The CRF AVTPDU header (IEEE 1722-2016 Clause 10).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrfHeader {
    /// stream_id valid.
    pub sv: bool,
    pub version: u8,
    /// Media clock restart.
    pub mr: bool,
    /// Frame sync.
    pub fs: bool,
    /// Timestamp uncertain — set while our own clock is not usable.
    pub tu: bool,
    pub seq: u8,
    pub crf_type: u8,
    pub stream_id: StreamId,
    pub pull: u8,
    /// 29-bit nominal frequency in Hz.
    pub base_frequency: u32,
    /// Length of `crf_data` in OCTETS (8 × timestamps), not a count.
    pub crf_data_length: u16,
    pub timestamp_interval: u16,
}

impl CrfHeader {
    /// The Milan Clock Reference Format header for `stream_id`
    /// (Milan v1.3 Table 7.1), with `seq` 0 and no flags set.
    pub fn milan(stream_id: StreamId) -> Self {
        Self {
            sv: true,
            version: 0,
            mr: false,
            fs: false,
            tu: false,
            seq: 0,
            crf_type: milan::TYPE,
            stream_id,
            pull: milan::PULL,
            base_frequency: milan::BASE_FREQUENCY,
            crf_data_length: milan::CRF_DATA_LENGTH,
            timestamp_interval: milan::TIMESTAMP_INTERVAL,
        }
    }

    /// Timestamps `crf_data_length` describes. `None` when the length is not a
    /// whole number of 8-octet timestamps.
    pub const fn timestamps(&self) -> Option<usize> {
        if !(self.crf_data_length as usize).is_multiple_of(TIMESTAMP_LEN) {
            return None;
        }
        Some(self.crf_data_length as usize / TIMESTAMP_LEN)
    }

    /// Parse the fixed [`CRF_HEADER_LEN`] octets of a CRF AVTPDU.
    ///
    /// Byte 1 bit 2 is reserved and is ignored here, so a peer that sets it
    /// does not make an otherwise valid PDU undecodable.
    pub fn parse(pdu: &[u8]) -> Result<Self, DropReason> {
        let h: &[u8; CRF_HEADER_LEN] = pdu.first_chunk().ok_or(DropReason::TooShort)?;
        let [st, b1, seq, ty, s0, s1, s2, s3, s4, s5, s6, s7, p0, p1, p2, p3, d0, d1, i0, i1] = *h;
        if st != subtype::CRF {
            return Err(DropReason::Unsupported);
        }
        let pull_base = u32::from_be_bytes([p0, p1, p2, p3]);
        Ok(Self {
            sv: b1 & 0x80 != 0,
            version: (b1 >> 4) & 0x07,
            mr: b1 & 0x08 != 0,
            fs: b1 & 0x02 != 0,
            tu: b1 & 0x01 != 0,
            seq,
            crf_type: ty,
            stream_id: StreamId::from_be_bytes([s0, s1, s2, s3, s4, s5, s6, s7]),
            pull: (pull_base >> 29) as u8,
            base_frequency: pull_base & 0x1FFF_FFFF,
            crf_data_length: u16::from_be_bytes([d0, d1]),
            timestamp_interval: u16::from_be_bytes([i0, i1]),
        })
    }

    /// Encode into `out[..20]`. The reserved bit is written zero.
    pub fn encode(&self, out: &mut [u8]) -> Option<()> {
        let h: &mut [u8; CRF_HEADER_LEN] = out.first_chunk_mut()?;
        let [s0, s1, s2, s3, s4, s5, s6, s7] = self.stream_id.to_be_bytes();
        let pull_base = (u32::from(self.pull & 0x07) << 29) | (self.base_frequency & 0x1FFF_FFFF);
        let [p0, p1, p2, p3] = pull_base.to_be_bytes();
        let [d0, d1] = self.crf_data_length.to_be_bytes();
        let [i0, i1] = self.timestamp_interval.to_be_bytes();
        let b1 = (u8::from(self.sv) << 7)
            | ((self.version & 0x07) << 4)
            | (u8::from(self.mr) << 3)
            | (u8::from(self.fs) << 1)
            | u8::from(self.tu);
        *h = [
            subtype::CRF,
            b1,
            self.seq,
            self.crf_type,
            s0,
            s1,
            s2,
            s3,
            s4,
            s5,
            s6,
            s7,
            p0,
            p1,
            p2,
            p3,
            d0,
            d1,
            i0,
            i1,
        ];
        Some(())
    }
}

/// A decoded CRF PDU: its header and the raw `crf_data` octets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CrfPdu<'a> {
    pub header: CrfHeader,
    data: &'a [u8],
}

impl<'a> CrfPdu<'a> {
    /// Decode a complete CRF AVTPDU. `crf_data_length` must be a whole number
    /// of timestamps and must fit in `pdu`.
    pub fn decode(pdu: &'a [u8]) -> Result<Self, DropReason> {
        let header = CrfHeader::parse(pdu)?;
        let n = header.timestamps().ok_or(DropReason::Malformed)?;
        let want = n * TIMESTAMP_LEN;
        let data = pdu
            .get(CRF_HEADER_LEN..CRF_HEADER_LEN + want)
            .ok_or(DropReason::TooShort)?;
        Ok(Self { header, data })
    }

    /// The gPTP nanosecond timestamps, in wire order.
    pub fn timestamps(&self) -> impl Iterator<Item = u64> + '_ {
        self.data
            .as_chunks::<TIMESTAMP_LEN>()
            .0
            .iter()
            .copied()
            .map(u64::from_be_bytes)
    }

    /// How many timestamps this PDU carries.
    pub fn len(&self) -> usize {
        self.data.len() / TIMESTAMP_LEN
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Per-stream CRF talker state: the immutable header template plus the running
/// sequence number. Allocates nothing after construction.
#[derive(Clone, Debug)]
pub struct CrfTalker {
    header: CrfHeader,
    timestamps_per_pdu: usize,
}

impl CrfTalker {
    /// A Milan Clock Reference Format talker for `stream_id`.
    pub fn milan(stream_id: StreamId) -> Self {
        Self {
            header: CrfHeader::milan(stream_id),
            timestamps_per_pdu: milan::TIMESTAMPS_PER_PDU as usize,
        }
    }

    /// Total AVTPDU octets one PDU occupies.
    pub const fn pdu_len(&self) -> usize {
        CRF_HEADER_LEN + self.timestamps_per_pdu * TIMESTAMP_LEN
    }

    /// Timestamps each PDU must carry.
    pub const fn timestamps_per_pdu(&self) -> usize {
        self.timestamps_per_pdu
    }

    /// The next PDU's `sequence_num`.
    pub const fn seq(&self) -> u8 {
        self.header.seq
    }

    /// Media-clock events between consecutive timestamps.
    pub const fn timestamp_interval(&self) -> u16 {
        self.header.timestamp_interval
    }

    pub const fn base_frequency(&self) -> u32 {
        self.header.base_frequency
    }

    /// Nanoseconds between consecutive timestamps, as an exact rational
    /// `(numerator, denominator)` — `interval × 1e9 / base_frequency`.
    ///
    /// Returned as a fraction rather than rounded because at 44.1 kHz the
    /// period is 2176870.748… ns: accumulating a rounded integer per event
    /// drifts hundreds of microseconds per hour on a stream whose entire
    /// purpose is carrying phase.
    pub const fn period_ns_exact(&self) -> (u128, u128) {
        (
            self.header.timestamp_interval as u128 * 1_000_000_000,
            self.header.base_frequency as u128,
        )
    }

    /// gPTP time of the `n`th media-clock event after `base_ns`, computed from
    /// `n` directly so rounding cannot accumulate.
    pub const fn nth_timestamp(&self, base_ns: u64, n: u64) -> u64 {
        let (num, den) = self.period_ns_exact();
        base_ns.wrapping_add((n as u128 * num / den) as u64)
    }

    /// Set `mr`, the media clock restart flag, on subsequent PDUs.
    pub fn set_mr(&mut self, mr: bool) {
        self.header.mr = mr;
    }

    /// Set `tu`. IEEE 1722 requires it while the timestamps are not known
    /// good — here, whenever our gPTP clock is not a usable time source.
    pub fn set_tu(&mut self, tu: bool) {
        self.header.tu = tu;
    }

    /// Write one complete CRF AVTPDU into `out`, returning its length and
    /// advancing the sequence number.
    ///
    /// `timestamps` must hold exactly [`Self::timestamps_per_pdu`] entries: a
    /// short write would leave the remainder of `crf_data` as whatever was in
    /// `out`, which a listener would read as real media-clock events.
    pub fn packetize(&mut self, timestamps: &[u64], out: &mut [u8]) -> Result<usize, CrfError> {
        if timestamps.len() != self.timestamps_per_pdu {
            return Err(CrfError::BadTimestampCount);
        }
        let len = self.pdu_len();
        let buf = out.get_mut(..len).ok_or(CrfError::OutTooSmall)?;
        self.header.encode(buf).ok_or(CrfError::OutTooSmall)?;
        let data = buf.get_mut(CRF_HEADER_LEN..).ok_or(CrfError::OutTooSmall)?;
        for (slot, ts) in data
            .as_chunks_mut::<TIMESTAMP_LEN>()
            .0
            .iter_mut()
            .zip(timestamps)
        {
            *slot = ts.to_be_bytes();
        }
        self.header.seq = self.header.seq.wrapping_add(1);
        Ok(len)
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::stream_format::{Decoded, StreamFormat};

    const SID: StreamId = StreamId(0x644B_F012_73B8_0009);

    /// The 64-bit AEM format string and the on-wire PDU are two DIFFERENT
    /// packings of the same stream parameters, so a mix-up between them cannot
    /// be caught by testing either one alone. Decode the spec's own format
    /// string and require every shared field to match the PDU we build.
    #[test]
    fn milan_pdu_and_aem_format_agree() {
        let Decoded::Crf(f) = StreamFormat(milan::FORMAT).decode() else {
            panic!("Milan CRF format string must decode as CRF");
        };
        assert_eq!(f.crf_type, milan::TYPE, "type");
        assert_eq!(f.timestamp_interval, milan::TIMESTAMP_INTERVAL, "interval");
        assert_eq!(
            u16::from(f.timestamps_per_pdu),
            milan::TIMESTAMPS_PER_PDU,
            "timestamps per pdu"
        );
        assert_eq!(f.pull, milan::PULL, "pull");
        assert_eq!(f.base_frequency, milan::BASE_FREQUENCY, "base frequency");

        let h = CrfHeader::milan(SID);
        assert_eq!(h.crf_type, f.crf_type);
        assert_eq!(h.timestamp_interval, f.timestamp_interval);
        assert_eq!(h.pull, f.pull);
        assert_eq!(h.base_frequency, f.base_frequency);
        // crf_data_length is OCTETS; the format string carries a COUNT.
        assert_eq!(
            h.timestamps(),
            Some(usize::from(f.timestamps_per_pdu)),
            "crf_data_length must describe the format's timestamp count"
        );
    }

    /// Bytes 12..16 of the PDU carry `pull | base_frequency` in the SAME 32-bit
    /// packing as the low word of the AEM format string. Equality of the raw
    /// octets is a layout check neither module could make alone.
    #[test]
    fn pull_and_base_frequency_share_the_aem_packing() {
        let mut pdu = [0u8; milan::PDU_LEN];
        CrfHeader::milan(SID).encode(&mut pdu).unwrap();
        let aem = milan::FORMAT.to_be_bytes();
        assert_eq!(&pdu[12..16], &aem[4..8], "pull|base_frequency word");
    }

    #[test]
    fn header_vector_round_trips() {
        // sv=1 version=0 mr=1 fs=1 tu=1, seq 0x2A, AUDIO_SAMPLE.
        let b = [
            0x04, 0x8B, 0x2A, 0x01, 0x64, 0x4B, 0xF0, 0x12, 0x73, 0xB8, 0x00, 0x09, 0x00, 0x00,
            0xBB, 0x80, 0x00, 0x08, 0x00, 0x60,
        ];
        let h = CrfHeader::parse(&b).unwrap();
        assert!(h.sv && h.mr && h.fs && h.tu);
        assert_eq!(h.version, 0);
        assert_eq!(h.seq, 0x2A);
        assert_eq!(h.crf_type, crf_type::AUDIO_SAMPLE);
        assert_eq!(h.stream_id, SID);
        assert_eq!(h.pull, 0);
        assert_eq!(h.base_frequency, 48_000);
        assert_eq!(h.crf_data_length, 8);
        assert_eq!(h.timestamp_interval, 96);
        let mut out = [0u8; CRF_HEADER_LEN];
        h.encode(&mut out).unwrap();
        assert_eq!(out, b, "re-encode must be octet-identical");
    }

    /// `pull` occupies the top 3 bits of the same word as the 29-bit frequency,
    /// so a non-zero pull must not corrupt the frequency and vice versa.
    #[test]
    fn pull_and_frequency_do_not_bleed() {
        let h = CrfHeader {
            pull: 5,
            base_frequency: 0x1FFF_FFFF,
            ..CrfHeader::milan(SID)
        };
        let mut out = [0u8; CRF_HEADER_LEN];
        h.encode(&mut out).unwrap();
        let back = CrfHeader::parse(&out).unwrap();
        assert_eq!(back.pull, 5);
        assert_eq!(back.base_frequency, 0x1FFF_FFFF);
    }

    #[test]
    fn reserved_bit_is_written_zero_and_ignored_on_parse() {
        let mut out = [0u8; CRF_HEADER_LEN];
        CrfHeader::milan(SID).encode(&mut out).unwrap();
        assert_eq!(out[1] & 0x04, 0, "reserved bit must be written zero");

        let mut with_reserved = out;
        with_reserved[1] |= 0x04;
        assert_eq!(
            CrfHeader::parse(&with_reserved).unwrap(),
            CrfHeader::parse(&out).unwrap(),
            "a peer setting the reserved bit must not change the decode"
        );
    }

    #[test]
    fn parse_rejects_short_and_non_crf() {
        let mut out = [0u8; CRF_HEADER_LEN];
        CrfHeader::milan(SID).encode(&mut out).unwrap();
        assert_eq!(
            CrfHeader::parse(&out[..CRF_HEADER_LEN - 1]),
            Err(DropReason::TooShort)
        );
        let mut aaf = out;
        aaf[0] = subtype::AAF;
        assert_eq!(CrfHeader::parse(&aaf), Err(DropReason::Unsupported));
    }

    #[test]
    fn packetize_builds_a_milan_pdu_that_decodes_back() {
        let mut t = CrfTalker::milan(SID);
        assert_eq!(t.pdu_len(), milan::PDU_LEN);
        assert_eq!(t.pdu_len(), 28, "Milan CRF AVTPDU is 28 octets");

        let mut out = [0u8; 64];
        let n = t.packetize(&[0x0123_4567_89AB_CDEF], &mut out).unwrap();
        assert_eq!(n, 28);

        let pdu = CrfPdu::decode(&out[..n]).unwrap();
        assert_eq!(pdu.len(), 1);
        assert_eq!(
            pdu.timestamps().collect::<Vec<_>>(),
            vec![0x0123_4567_89AB_CDEF]
        );
        assert_eq!(pdu.header.stream_id, SID);
        assert_eq!(pdu.header.seq, 0);
        assert_eq!(t.seq(), 1, "sequence advances after a PDU");
    }

    #[test]
    fn packetize_refuses_a_wrong_timestamp_count() {
        let mut t = CrfTalker::milan(SID);
        let mut out = [0u8; 64];
        assert_eq!(
            t.packetize(&[], &mut out),
            Err(CrfError::BadTimestampCount),
            "an empty slice would leave crf_data holding stale octets"
        );
        assert_eq!(
            t.packetize(&[1, 2], &mut out),
            Err(CrfError::BadTimestampCount)
        );
        assert_eq!(t.seq(), 0, "a refused PDU must not burn a sequence number");
    }

    #[test]
    fn packetize_refuses_a_short_buffer() {
        let mut t = CrfTalker::milan(SID);
        let mut out = [0u8; milan::PDU_LEN - 1];
        assert_eq!(t.packetize(&[1], &mut out), Err(CrfError::OutTooSmall));
    }

    #[test]
    fn decode_rejects_a_data_length_that_is_not_whole_timestamps() {
        let mut out = [0u8; 64];
        let h = CrfHeader {
            crf_data_length: 7,
            ..CrfHeader::milan(SID)
        };
        h.encode(&mut out).unwrap();
        assert_eq!(CrfPdu::decode(&out), Err(DropReason::Malformed));
    }

    #[test]
    fn decode_rejects_a_pdu_shorter_than_its_declared_data() {
        let mut out = [0u8; CRF_HEADER_LEN + 4];
        CrfHeader::milan(SID).encode(&mut out).unwrap();
        assert_eq!(CrfPdu::decode(&out), Err(DropReason::TooShort));
    }

    #[test]
    fn mr_and_tu_reach_the_wire() {
        let mut t = CrfTalker::milan(SID);
        t.set_mr(true);
        t.set_tu(true);
        let mut out = [0u8; 64];
        let n = t.packetize(&[1], &mut out).unwrap();
        let pdu = CrfPdu::decode(&out[..n]).unwrap();
        assert!(pdu.header.mr && pdu.header.tu);
    }

    /// Reproduce Milan v1.3 Table 4.4's own published bandwidth from the PDU
    /// length this module defines. Independent of libavtp and NVIDIA, so it
    /// corroborates the 20-octet header from the spec alone.
    #[test]
    fn milan_pdu_len_reproduces_the_published_bandwidth() {
        assert_eq!(milan::PDU_LEN, 28, "v1.3 Table 4.4 MaxFrameSize for CRF");
        assert_eq!(
            milan::bandwidth_bps(milan::PDU_LEN as u16, milan::MAX_INTERVAL_FRAMES),
            5_696_000,
            "v1.3 Table 4.4 publishes 5696 kbps"
        );
        // The same wire frame under v2.0a's ordering, where the tolerance byte
        // is folded into MaxFrameSize ahead of the minimum-frame clamp.
        let f = (milan::PDU_LEN as u32 + 1 + 22).max(68);
        assert_eq!((f + 20) * 8000 * 8, 5_632_000, "v2.0a publishes 5632 kbps");
    }

    #[test]
    fn milan_cadence_is_500_pdus_per_second() {
        assert_eq!(milan::pdus_per_sec(), 500);
        let t = CrfTalker::milan(SID);
        let (num, den) = t.period_ns_exact();
        assert_eq!(num / den, 2_000_000, "2 ms between timestamps at 48 kHz");
    }

    /// `nth_timestamp`'s `n` is the TIMESTAMP index, not the media-clock event
    /// index. Advancing it by `timestamp_interval` per PDU instead of by 1
    /// would stamp the series 96x too fast — a wrong media clock rather than a
    /// visibly broken one — so pin the relationship both ways.
    #[test]
    fn the_timestamp_index_advances_once_per_pdu_not_once_per_event() {
        let t = CrfTalker::milan(SID);
        let base = 1_000_000_000u64;

        // One PDU apart is one timestamp period: 2 ms.
        assert_eq!(t.nth_timestamp(base, 1) - base, 2_000_000);
        // 500 PDUs is exactly one second of stream.
        assert_eq!(
            t.nth_timestamp(base, u64::from(milan::pdus_per_sec())) - base,
            1_000_000_000,
            "500 timestamps must span exactly one second"
        );
        // And one second of stream covers base_frequency media-clock events.
        let events = u64::from(milan::pdus_per_sec())
            * u64::from(milan::TIMESTAMPS_PER_PDU)
            * u64::from(milan::TIMESTAMP_INTERVAL);
        assert_eq!(
            events,
            u64::from(milan::BASE_FREQUENCY),
            "one second of CRF must account for exactly base_frequency events"
        );
    }

    /// At 44.1 kHz the period is 2176870.748… ns. Accumulating a rounded
    /// integer per event drifts; computing from `n` does not. Assert against
    /// exact rational arithmetic over an hour of events, and assert the naive
    /// accumulation really does drift, so this test cannot pass vacuously.
    #[test]
    fn nth_timestamp_does_not_accumulate_rounding_error() {
        let t = CrfTalker {
            header: CrfHeader {
                base_frequency: milan::OPTIONAL_BASE_FREQUENCY,
                ..CrfHeader::milan(SID)
            },
            timestamps_per_pdu: 1,
        };
        let (num, den) = t.period_ns_exact();
        assert_ne!(num % den, 0, "44.1 kHz period must not be a whole ns");

        let events: u64 = 44_100 / 96 * 3600; // one hour of media-clock events
        let base = 1_000_000_000u64;

        let exact = base + (events as u128 * num / den) as u64;
        assert_eq!(t.nth_timestamp(base, events), exact);

        let naive_step = (num / den) as u64;
        let naive = base + naive_step * events;
        assert!(
            exact.abs_diff(naive) > 100_000,
            "naive accumulation must visibly drift, else this proves nothing \
             (exact {exact}, naive {naive})"
        );
    }

    #[test]
    fn crf_type_names_are_stable() {
        assert_eq!(crf_type::AUDIO_SAMPLE, 0x01);
        assert_eq!(crf_type::name(0x01), Some("AUDIO_SAMPLE"));
        assert_eq!(crf_type::name(0x7F), None);
    }

    #[test]
    fn optional_format_is_the_spec_value_not_the_device_deviation() {
        let Decoded::Crf(f) = StreamFormat(milan::OPTIONAL_FORMAT).decode() else {
            panic!("optional CRF format string must decode as CRF");
        };
        assert_eq!(f.base_frequency, milan::OPTIONAL_BASE_FREQUENCY);
        assert_eq!(
            f.timestamp_interval, 96,
            "Table 7.2 requires 96; the 16R's own 147 is non-conformant and \
             must not be used as the anchor"
        );
    }
}
