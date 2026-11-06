// SPDX-License-Identifier: MIT OR Apache-2.0

//! avb-audioring: the recovered-audio ring ABI v1 (design §10.6).
//!
//! A position-independent 16 KiB header plus an interleaved S32LE data region.
//! [`Producer`] (the daemon's `audio` thread) places AAF samples by absolute
//! frame index and advances a frontier; [`Consumer`] (the `rec` thread, and
//! the Phase 1b AudioServerPlugIn) reads committed frames. The clock block is
//! a seqlock carrying the media-clock model (§10.5).
//!
//! The only system call is `libc::mmap`/`munmap` in [`AudioRing::new_anonymous`]
//! (design §6.1); `from_raw`, `Producer` and `Consumer` make no syscalls and
//! allocate nothing after construction.

#![deny(clippy::indexing_slicing)]

pub mod consumer;
pub mod layout;
pub mod producer;
pub mod ring;
pub mod shm;

pub use consumer::Consumer;
pub use layout::{counter, RingConfig, StreamSlot};
pub use producer::{Producer, SampleFormat};
pub use ring::{AudioRing, RingError};
pub use shm::{ShmProducer, ShmRing, SHM_NAME, SHM_NAME_OUT};

/// The DreamAVB device's full channel width, in ONE place.
///
/// 128 each way: room for a 16R (64) plus a 24R or another 64-channel device,
/// mapped onto these channels by the user's channel map. Fixed, never grown at
/// runtime, so a DAW's channel list does not change when devices come and go.
///
/// This is shared rather than defined per crate because the two ends MUST
/// agree and cannot negotiate: a HAL device's channel count is fixed when the
/// device is published, long before any ring exists, while the daemon creates
/// its rings when it runs. The plug-in therefore refuses a ring whose width
/// differs — which is the right failure, but it is silent audio, so the width
/// must not be able to drift. The daemon always creates rings this wide
/// regardless of how many streams are actually bound; the channels belonging
/// to an unbound stream are simply never written and read as silence.
pub const DEVICE_CHANNELS: u32 = 128;

/// Channels carried by one AVB stream, and therefore by one ring slot.
pub const CHANNELS_PER_STREAM: u32 = 8;

/// The presentation offset our talker stamps: how far ahead of gPTP now each
/// outbound AVTP timestamp is. It is all of the output path's latency beyond
/// the IO buffer, so the CoreAudio plug-in reports exactly this to the HAL and
/// the daemon stamps exactly this. One constant, so the two cannot disagree.
///
/// 2 ms, the Milan maximum (Baseline Interoperability Spec §7.2.1: a
/// listener buffers up to 2 ms + 125 us + 1 us).
///
/// Smaller offsets are conformant but the 16R does not hold lock on them.
/// Measured on the live 16R over 20 s per setting: at 1 ms and at 1.5 ms all
/// eight sinks cycled MEDIA_LOCKED / MEDIA_UNLOCKED about once every 5 s with
/// no new LATE_TIMESTAMP, and at 2 ms the lock counters did not move. Do not
/// lower this to save latency without repeating that measurement.
///
/// This is the DEFAULT. The user can change it (the companion app's
/// "Headroom" setting, `presentation_offset_us` in config.txt) within
/// [`PRESENTATION_OFFSET_RANGE_NS`]; the daemon publishes the value in force
/// in the output ring header so the plug-in reports the matching latency.
pub const TALKER_PRESENTATION_OFFSET_NS: u64 = 2_000_000;

/// The presentation offsets the daemon accepts. Below 1 ms the 16R never
/// held lock; above 5 ms is far past a Milan listener's 2.126 ms buffer.
pub const PRESENTATION_OFFSET_RANGE_NS: std::ops::RangeInclusive<u64> = 1_000_000..=5_000_000;

/// Output-ring frames the talker keeps queued at the LOW point of each write
/// burst, i.e. the floor the queue is trimmed to. The rest of the queue is
/// the IO buffer the HAL already counts. Shared so the plug-in reports exactly
/// the queue the daemon maintains: 24 frames = 4 PDUs = 0.5 ms at 48 kHz.
///
/// Measured before the trim existed: a standing backlog of ~518 frames
/// (10.8 ms) that nothing ever drained, because the plug-in writes and the
/// talker reads at the same clock and keep whatever excess they start with.
pub const OUTPUT_QUEUE_KEEP_FRAMES: u64 = 24;

/// How many AVB streams make up the device's width in one direction.
pub const STREAMS_PER_DIRECTION: u32 = DEVICE_CHANNELS / CHANNELS_PER_STREAM;

const _: () = {
    assert!(
        DEVICE_CHANNELS.is_multiple_of(CHANNELS_PER_STREAM),
        "the device width must be a whole number of streams, or a slot's \
         channel range would run past the end of a frame"
    );
    assert!(STREAMS_PER_DIRECTION == 16);
    assert!(STREAMS_PER_DIRECTION as usize <= layout::MAX_STREAMS);
};

/// The media-clock model carried in the ring's clock block (§10.6). Mirrors
/// `avb_mediaclock::ClockModel`; kept here so the ring crate does not depend
/// on `avb-mediaclock` (the daemon copies the fields across).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ClockBlock {
    pub anchor_frame: u64,
    pub anchor_mach: f64,
    pub mach_per_frame: f64,
    pub rate_hz_vs_mach: f64,
    pub media_ppm_vs_gptp: f64,
    pub uncertainty_ns: f64,
    pub locked: bool,
    pub reset_count: u32,
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn test_config(capacity: u32) -> RingConfig {
        RingConfig {
            channels: 4,
            valid_bits: 24,
            nominal_rate_hz: 48_000,
            capacity_frames: capacity,
            streams: vec![StreamSlot {
                stream_id: 1,
                first_channel: 0,
                channels: 4,
                bit_depth: 24,
            }],
            channel_map: (0..4).map(|c| (0u16, c as u16)).collect(),
        }
    }

    #[test]
    fn header_initialised_and_reattach() {
        let r = AudioRing::new_anonymous(&test_config(1024)).unwrap();
        assert_eq!(r.load_u32(layout::field::MAGIC), layout::MAGIC);
        assert_eq!(r.load_u32(layout::field::VERSION), 1);
        assert_eq!(r.load_u32(layout::field::CHANNELS), 4);
        assert_eq!(r.load_u32(layout::field::CAPACITY_FRAMES), 1024);
        assert_eq!(r.frame_bytes(), 16);
    }

    #[test]
    fn presentation_offset_is_published_and_bounded() {
        let r = AudioRing::new_anonymous(&test_config(256)).unwrap();
        // Never written (an older daemon): no value, so readers use the default.
        assert_eq!(r.presentation_offset_ns(), None);
        r.set_presentation_offset_ns(3_000_000);
        assert_eq!(r.presentation_offset_ns(), Some(3_000_000));
        // Out of range is treated as not published.
        r.set_presentation_offset_ns(50_000_000);
        assert_eq!(r.presentation_offset_ns(), None);
        // The default sits inside the accepted range.
        assert!(PRESENTATION_OFFSET_RANGE_NS.contains(&TALKER_PRESENTATION_OFFSET_NS));
    }

    #[test]
    fn clock_block_seqlock_round_trip() {
        let r = AudioRing::new_anonymous(&test_config(256)).unwrap();
        let m = ClockBlock {
            anchor_frame: 12_345,
            anchor_mach: 9.87e6,
            mach_per_frame: 500.25,
            rate_hz_vs_mach: 48_000.1,
            media_ppm_vs_gptp: -0.03,
            uncertainty_ns: 170.0,
            locked: true,
            reset_count: 2,
        };
        r.publish_clock(&m);
        assert_eq!(r.read_clock(), Some(m));
    }

    #[test]
    fn clock_block_concurrent_reads_never_tear() {
        let r = Arc::new(AudioRing::new_anonymous(&test_config(256)).unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let r = Arc::clone(&r);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut k = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    k += 1;
                    r.publish_clock(&ClockBlock {
                        anchor_frame: k,
                        anchor_mach: k as f64,
                        mach_per_frame: k as f64 * 2.0,
                        rate_hz_vs_mach: 48_000.0,
                        media_ppm_vs_gptp: 0.0,
                        uncertainty_ns: 0.0,
                        locked: k.is_multiple_of(2),
                        reset_count: (k % 7) as u32,
                    });
                }
            })
        };
        let reader = {
            let r = Arc::clone(&r);
            std::thread::spawn(move || {
                let mut ok = 0u64;
                while ok < 100_000 {
                    if let Some(m) = r.read_clock() {
                        // Consistency: anchor_mach == anchor_frame, mach_per_frame == 2x.
                        assert_eq!(m.anchor_mach, m.anchor_frame as f64);
                        assert_eq!(m.mach_per_frame, m.anchor_frame as f64 * 2.0);
                        ok += 1;
                    }
                }
                ok
            })
        };
        assert_eq!(reader.join().unwrap(), 100_000);
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    }

    #[test]
    fn overrun_frames_counted_once_not_per_commit() {
        use crate::layout::counter;
        use crate::producer::{Producer, SampleFormat};
        let cfg = test_config(16); // tiny capacity
        let r = AudioRing::new_anonymous(&cfg).unwrap();
        let mut p = Producer::new(&r, &[(0, 4, 24)], 192, 192);
        let data = vec![0u8; 2 * 4 * 4];
        // Write well past capacity with a static consumer (read_frame stays 0),
        // committing each tick. Overrun loss must be counted once per lost
        // frame, not re-counted every commit.
        for i in 0..40u64 {
            p.place(0, (i * 2) as i128, 2, SampleFormat::Int32, &data);
            p.commit(false, None);
        }
        // Final write_frame = 80, capacity 16 -> 80 - 16 = 64 frames fell off
        // the back and must be counted exactly once each.
        assert_eq!(r.write_frame(), 80);
        assert_eq!(
            r.counter(counter::OVERRUN_FRAMES),
            80 - 16,
            "overrun frames must be counted once, not accumulated per commit"
        );
    }

    #[test]
    fn lagging_stream_gap_counted_once_not_per_commit() {
        use crate::layout::counter;
        use crate::producer::{Producer, SampleFormat};
        let r = AudioRing::new_anonymous(&test_config(256)).unwrap();
        // Two streams; stream 1 never delivers, so it lags stream 0.
        let mut p = Producer::new(&r, &[(0, 4, 24), (4, 4, 24)], 4, 192);
        let data = vec![0u8; 2 * 4 * 4];
        for i in 0..40u64 {
            p.place(0, (i * 2) as i128, 2, SampleFormat::Int32, &data);
            p.commit(false, None);
        }
        assert_eq!(r.write_frame(), 80);
        // Stream 1 missed frames 0..80: 80 gap frames, once each. Counting
        // its whole lag on every commit summed to ~1600 here (and 2.5e14 in a
        // 10-minute hardware run).
        assert_eq!(r.counter(counter::GAP_FRAMES_TOTAL), 80);
    }

    #[test]
    fn negative_frame_is_dropped_not_wrapped() {
        use crate::layout::counter;
        use crate::producer::{Producer, SampleFormat};
        let cfg = test_config(256);
        let r = AudioRing::new_anonymous(&cfg).unwrap();
        let mut p = Producer::new(&r, &[(0, 4, 24)], 192, 192);
        let data = vec![0u8; 2 * 4 * 4];
        // A negative frame index must be a late drop, never cast to a huge u64.
        assert!(!p.place(0, -5, 2, SampleFormat::Int32, &data));
        assert_eq!(r.counter(counter::LATE_DROPS), 1);
        assert_eq!(r.write_frame(), 0);
    }

    #[test]
    fn spsc_producer_consumer_stress() {
        // One producer writes an increasing ramp; one consumer verifies it in
        // order with no torn frames.
        //
        // The ring stays small (4096 frames) so the ramp wraps it ~73 times and
        // the modulo/wrap path is exercised, and the producer is held back by the
        // consumer's published progress so it can never lap it.
        //
        // It used to rely on a comment claiming 4096 was "large enough to avoid
        // overrun at this cadence" — a scheduling assumption, not a fact. On a
        // 2-core CI runner the producer ran unimpeded before the consumer was
        // first scheduled, lapped the ring, and the consumer's first read
        // correctly returned the oldest surviving frame (6516) instead of 0, so
        // the test failed with "frame out of order" on an overrun that is CORRECT
        // behaviour (covered deliberately by consumer::tests::overrun_skips).
        // Simply enlarging the ring to hold the whole ramp would also have been
        // deterministic, but would have stopped it wrapping at all. This test is
        // about ordering and tearing, so bound the producer instead of asserting
        // against the scheduler.
        use std::sync::atomic::{AtomicU64, Ordering};

        let capacity = 4096u32;
        let total_packets = 50_000u64;
        let spf = 6usize;
        let total_frames = total_packets * spf as u64;
        let r = Arc::new(AudioRing::new_anonymous(&test_config(capacity)).unwrap());
        // Frames the consumer has verified, published for flow control.
        let consumed = Arc::new(AtomicU64::new(0));
        // Stay a margin below capacity so a batch in flight cannot tip into
        // overrun.
        let high_water = u64::from(capacity) - 512;

        let producer = {
            let r = Arc::clone(&r);
            let consumed = Arc::clone(&consumed);
            std::thread::spawn(move || {
                let mut p = Producer::new(&r, &[(0, 4, 24)], 192, 192);
                for pkt in 0..total_packets {
                    let first = pkt * spf as u64;
                    // Backpressure: never get further ahead of the consumer than
                    // the ring can hold.
                    while first.saturating_sub(consumed.load(Ordering::Acquire)) > high_water {
                        std::hint::spin_loop();
                    }
                    // Each sample encodes its absolute frame index in ch 0.
                    let mut data = vec![0u8; spf * 4 * 4];
                    for fr in 0..spf {
                        let frame = first + fr as u64;
                        let v = (frame as u32).to_be_bytes();
                        let off = fr * 4 * 4;
                        data[off..off + 4].copy_from_slice(&v);
                    }
                    p.place(0, first as i128, spf, SampleFormat::Int32, &data);
                    p.commit(false, None);
                }
            })
        };
        let consumer = {
            let r = Arc::clone(&r);
            let consumed = Arc::clone(&consumed);
            std::thread::spawn(move || {
                let mut c = Consumer::new(&r);
                let mut next = 0u64;
                let frame_bytes = r.frame_bytes();
                let mut out = vec![0u8; 256 * frame_bytes];
                while next < total_frames {
                    let got = c.read_into(&mut out, 256);
                    if got == 0 {
                        std::hint::spin_loop();
                        continue;
                    }
                    for fr in 0..got {
                        let ch0 = &out[fr * frame_bytes..fr * frame_bytes + 4];
                        let v = u32::from_le_bytes([ch0[0], ch0[1], ch0[2], ch0[3]]);
                        assert_eq!(u64::from(v), next, "frame out of order");
                        next += 1;
                    }
                    consumed.store(next, Ordering::Release);
                }
                next
            })
        };
        producer.join().unwrap();
        assert_eq!(consumer.join().unwrap(), total_frames);
        // Prove the premise the ordering assertions rely on, instead of assuming
        // it: nothing was dropped or skipped, so in-order delivery was the only
        // correct outcome. Without the backpressure above these fire on a loaded
        // machine.
        assert_eq!(
            r.counter(crate::layout::counter::OVERRUN_EVENTS),
            0,
            "ring overran"
        );
        assert_eq!(
            r.counter(crate::layout::counter::CONSUMER_SKIPS),
            0,
            "consumer skipped"
        );
        // The ramp is far larger than the ring, so the wrap path was exercised.
        assert!(total_frames > u64::from(capacity) * 50);
    }

    /// Regression (review C1): a producer that attaches to a ring another
    /// process has already advanced must RESUME at the live frontier. A fresh
    /// `Producer::new` per call starts `committed` at 0, so it rejected every
    /// place beyond cap/2 and reset the shared frontier — silent playback.
    #[test]
    fn resumed_producer_advances_live_frontier() {
        let r = AudioRing::new_anonymous(&test_config(1024)).unwrap();
        // Another producer advances the ring well past cap/2.
        let mut first = Producer::new(&r, &[(0, 4, 24)], 0, 0);
        let data = vec![0u8; 4 * 4 * 64];
        for i in 0..20u64 {
            assert!(first.place(0, (i * 64) as i128, 64, SampleFormat::Int32, &data));
            first.commit(false, None);
        }
        let live = r.write_frame();
        assert_eq!(live, 20 * 64);
        assert!(
            live > 1024 / 2,
            "frontier must be beyond cap/2 for the test to bite"
        );

        // A per-call fresh producer (the old bug) cannot place at the frontier.
        let mut fresh = Producer::new(&r, &[(0, 4, 24)], 0, 0);
        assert!(!fresh.place(0, live as i128, 64, SampleFormat::Int32, &data));

        // A resumed producer places there and advances cumulatively.
        let mut p = Producer::resume(&r, &[(0, 4, 24)], 0, 0);
        let mut f = live;
        for _ in 0..3 {
            assert!(p.place(0, f as i128, 64, SampleFormat::Int32, &data));
            p.commit(false, None);
            f += 64;
            assert_eq!(r.write_frame(), f, "frontier advances, never resets");
        }
    }
}
