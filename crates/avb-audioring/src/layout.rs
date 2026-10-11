// SPDX-License-Identifier: MIT OR Apache-2.0

//! The recovered-audio ring ABI v1 layout (design §10.6).
//!
//! The header is a position-independent 16 384-byte region: fields live at
//! fixed byte offsets and are accessed only atomically, so a C/C++ reader in
//! Phase 1b uses `__atomic_load_n` on the same offsets. We do NOT model it as
//! a `#[repr(C)]` struct with mixed field types; instead [`field`] gives the
//! offset of every field and the offsets are pinned by tests (RING-2).

/// Header magic: "AVAR" little-endian (§10.6).
pub const MAGIC: u32 = 0x5241_5641;
/// Ring ABI version (§10.6).
pub const VERSION: u32 = 1;
/// Header region size and the data offset (§10.6).
pub const HEADER_SIZE: usize = 16_384;
/// S32LE container format code (§10.6).
pub const SAMPLE_FORMAT_S32LE: u32 = 1;
/// Max channels (16 streams × 64 ch, §10.6).
pub const MAX_CHANNELS: u32 = 1024;
/// Max streams (S_max, §10.6).
pub const MAX_STREAMS: usize = 16;
/// Bytes per sample in the S32LE container.
pub const BYTES_PER_SAMPLE: usize = 4;
/// Per-stream record size (§10.6: 64 B).
pub const STREAM_RECORD_SIZE: usize = 64;
/// channel_map entry size (§10.6: 8 B).
pub const CHANNEL_MAP_ENTRY_SIZE: usize = 8;
/// gap-log entry size (§10.6: 32 B).
pub const GAP_LOG_ENTRY_SIZE: usize = 32;
/// gap-log capacity (§10.6: 128 entries).
pub const GAP_LOG_ENTRIES: usize = 128;

/// Fixed field offsets into the header (§10.6 table).
pub mod field {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 4;
    pub const HEADER_SIZE: usize = 8;
    pub const CHANNELS: usize = 12;
    pub const SAMPLE_FORMAT: usize = 16;
    pub const VALID_BITS: usize = 20;
    pub const NOMINAL_RATE_HZ: usize = 24;
    pub const CAPACITY_FRAMES: usize = 28;
    pub const FRAME_BYTES: usize = 32;
    pub const STREAM_COUNT: usize = 36;
    pub const DATA_OFFSET: usize = 40;
    pub const RESERVED_44: usize = 44;
    pub const CONFIG_GENERATION: usize = 48;
    pub const WRITE_FRAME: usize = 128;
    pub const READ_FRAME: usize = 256;
    /// Clock block (128 B): seq, pad, anchor_frame, anchor_mach,
    /// mach_per_frame, rate_hz_vs_mach, media_ppm_vs_gptp, uncertainty_ns,
    /// locked, reset_count.
    pub const CLOCK_BLOCK: usize = 384;
    pub const COUNTERS: usize = 512;
    pub const STREAMS: usize = 1024;
    pub const CHANNEL_MAP: usize = 2048;
    pub const GAP_LOG: usize = 10_240;
}

/// Clock-block sub-field offsets, relative to [`field::CLOCK_BLOCK`] (§10.6).
pub mod clock {
    pub const SEQ: usize = 0; // u32
    pub const PAD: usize = 4; // u32
    pub const ANCHOR_FRAME: usize = 8; // u64
    pub const ANCHOR_MACH: usize = 16; // f64
    pub const MACH_PER_FRAME: usize = 24; // f64
    pub const RATE_HZ_VS_MACH: usize = 32; // f64
    pub const MEDIA_PPM_VS_GPTP: usize = 40; // f64
    pub const UNCERTAINTY_NS: usize = 48; // f64
    pub const LOCKED: usize = 56; // u32
    pub const RESET_COUNT: usize = 60; // u32
}

/// Counter indices into the counters[64] block at [`field::COUNTERS`] (§10.6).
pub mod counter {
    pub const OVERRUN_FRAMES: usize = 0;
    pub const OVERRUN_EVENTS: usize = 1;
    pub const CONSUMER_SKIPS: usize = 2;
    pub const GAP_FRAMES_TOTAL: usize = 3;
    pub const LATE_DROPS: usize = 4;
    pub const PLACEMENT_REJECTS: usize = 5;
    pub const ZERO_FILL_FRAMES: usize = 6;
    pub const PLACEMENT_SLIP: usize = 7;
    pub const UNDERRUNS: usize = 8;
    pub const COUNT: usize = 64;
}

/// Per-stream record sub-field offsets, relative to a stream's base at
/// `field::STREAMS + i*STREAM_RECORD_SIZE` (§10.6).
pub mod stream {
    pub const STREAM_ID: usize = 0; // u64
    pub const FIRST_CHANNEL: usize = 8; // u32
    pub const CHANNELS: usize = 12; // u32
    pub const GAP_FRAMES: usize = 16; // u64
    pub const GAP_EVENTS: usize = 24; // u64
    pub const LAST_FRAME: usize = 32; // u64
    pub const ACTIVE: usize = 40; // u32
    pub const BIT_DEPTH: usize = 44; // u32
}

/// One bound stream's placement in the ring (built by the daemon from
/// `ListenerModel`; §10.6 RING-4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamSlot {
    pub stream_id: u64,
    pub first_channel: u32,
    pub channels: u32,
    pub bit_depth: u32,
}

/// The ring configuration (built only from `ListenerModel`; §10.6, §11.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RingConfig {
    pub channels: u32,
    pub valid_bits: u32,
    pub nominal_rate_hz: u32,
    pub capacity_frames: u32,
    pub streams: Vec<StreamSlot>,
    /// channel_map[k] = (stream_index, stream_channel).
    pub channel_map: Vec<(u16, u16)>,
}

impl RingConfig {
    pub fn frame_bytes(&self) -> u32 {
        self.channels * BYTES_PER_SAMPLE as u32
    }

    pub fn data_bytes(&self) -> usize {
        self.capacity_frames as usize * self.frame_bytes() as usize
    }

    pub fn total_bytes(&self) -> usize {
        HEADER_SIZE + self.data_bytes()
    }

    /// Validate against the §10.6 limits.
    pub fn is_valid(&self) -> bool {
        self.channels >= 1
            && self.channels <= MAX_CHANNELS
            && self.capacity_frames.is_power_of_two()
            && self.streams.len() <= MAX_STREAMS
            && (self.valid_bits >= 1 && self.valid_bits <= 32)
            && self.nominal_rate_hz > 0
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::assertions_on_constants)]
mod tests {
    use super::*;

    #[test]
    fn offsets_match_design_table() {
        assert_eq!(field::MAGIC, 0);
        assert_eq!(field::VERSION, 4);
        assert_eq!(field::CHANNELS, 12);
        assert_eq!(field::CAPACITY_FRAMES, 28);
        assert_eq!(field::DATA_OFFSET, 40);
        assert_eq!(field::CONFIG_GENERATION, 48);
        assert_eq!(field::WRITE_FRAME, 128);
        assert_eq!(field::READ_FRAME, 256);
        assert_eq!(field::CLOCK_BLOCK, 384);
        assert_eq!(field::COUNTERS, 512);
        assert_eq!(field::STREAMS, 1024);
        assert_eq!(field::CHANNEL_MAP, 2048);
        assert_eq!(field::GAP_LOG, 10_240);
        // channel_map[1024] × 8 B ends at 2048 + 8192 = 10240.
        assert_eq!(
            field::CHANNEL_MAP + MAX_CHANNELS as usize * CHANNEL_MAP_ENTRY_SIZE,
            10_240
        );
        // streams[16] × 64 B ends at 1024 + 1024 = 2048.
        assert_eq!(field::STREAMS + MAX_STREAMS * STREAM_RECORD_SIZE, 2048);
        // gap log: head u64 + 128 × 32 B = 8 + 4096 ends at 10240 + 4104 = 14344.
        assert_eq!(
            field::GAP_LOG + 8 + GAP_LOG_ENTRIES * GAP_LOG_ENTRY_SIZE,
            14_344
        );
        assert!(14_344 <= HEADER_SIZE);
    }

    #[test]
    fn magic_and_version_pinned() {
        assert_eq!(MAGIC, 0x5241_5641);
        assert_eq!(&MAGIC.to_le_bytes(), b"AVAR");
        assert_eq!(VERSION, 1);
        assert_eq!(HEADER_SIZE, 16_384);
    }

    #[test]
    fn config_validation() {
        let c = RingConfig {
            channels: 8,
            valid_bits: 24,
            nominal_rate_hz: 48_000,
            capacity_frames: 65_536,
            streams: vec![StreamSlot {
                stream_id: 1,
                first_channel: 0,
                channels: 8,
                bit_depth: 24,
            }],
            channel_map: vec![(0, 0); 8],
        };
        assert!(c.is_valid());
        assert_eq!(c.frame_bytes(), 32);
        assert_eq!(c.total_bytes(), HEADER_SIZE + 65_536 * 32);
        // Non-power-of-two capacity is invalid.
        let mut bad = c.clone();
        bad.capacity_frames = 65_000;
        assert!(!bad.is_valid());
    }
}
