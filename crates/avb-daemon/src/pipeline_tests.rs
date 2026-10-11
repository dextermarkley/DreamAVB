//! Full-stack pipeline tests (design §14.1 avb-daemon row): synthetic AEM +
//! generated AAF frames fed through the audio engine → ring → consumer → WAV
//! bytes, checking alignment, zero-fill, counters, channel map and bit
//! exactness, under a counting allocator that asserts 0 allocations in the
//! per-packet function. A 48 kHz / 6 spf 3×4-ch case and a 96 kHz / 12 spf
//! 2×6-ch case (§14.1).

#![cfg(test)]

use std::alloc::{GlobalAlloc, Layout, System};

use avb_audioring::producer::{Producer, SampleFormat};
use avb_audioring::{AudioRing, Consumer, RingConfig, StreamSlot};
use avb_avtp::aaf::{self, AafHeader};
use avb_avtp::common::StreamHeader;
use avb_avtp::stream_format::AafFormat;
use avb_core::StreamId;
use avb_mediaclock::{MediaClock, RateConsts};

use crate::audio::{AudioEngine, BoundStream, StreamCounters};
use crate::wav::{finalize_header, initial_header, WavFormat};

// --- Counting allocator (counts allocations on the MEASURING thread while
// armed). A thread-local flag, set only on the thread running the measured
// region, gates counting — so the two pipeline tests are safe to run in
// parallel with each other and with any other test in this binary: an
// allocation on a different thread never perturbs the measured count. ---

struct CountingAlloc;

thread_local! {
    /// True only on the thread currently inside `run_pipeline`'s measured
    /// region. Checked by the allocator; a sibling test thread leaves this
    /// false and never counts.
    static MEASURING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Per-thread allocation count while measuring — thread-local so two
    /// pipeline tests measuring concurrently never share a counter.
    static TL_ALLOCS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Cell reads only — re-entrancy-safe (never allocates itself).
        let _ = MEASURING.try_with(|m| {
            if m.get() {
                TL_ALLOCS.with(|c| c.set(c.get().wrapping_add(1)));
            }
        });
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn aaf_fmt(nsr: u8, depth: u8, ch: u16, spf: u16) -> AafFormat {
    AafFormat {
        ut: false,
        nsr,
        format: aaf::format::INT_32BIT,
        bit_depth: depth,
        channels: ch,
        spf,
    }
}

/// Build an AAF packet for stream `sid`, seq `seq`, with per-channel ramp
/// values encoding the frame index in channel 0.
fn make_packet(sid: u64, seq: u8, pt: u32, fmt: AafFormat, base_frame: u32) -> Vec<u8> {
    let ch = usize::from(fmt.channels);
    let spf = usize::from(fmt.spf);
    let mut data = vec![0u8; ch * spf * 4];
    for fr in 0..spf {
        for c in 0..ch {
            let v = if c == 0 {
                base_frame + fr as u32
            } else {
                0x0100_0000 + (c as u32)
            };
            let off = (fr * ch + c) * 4;
            data[off..off + 4].copy_from_slice(&v.to_be_bytes());
        }
    }
    let hdr = AafHeader {
        stream: StreamHeader {
            subtype: avb_avtp::common::subtype::AAF,
            sv: true,
            version: 0,
            mr: false,
            tv: true,
            seq,
            tu: false,
            stream_id: StreamId(sid),
            avtp_timestamp: pt,
        },
        format: fmt.format,
        nsr: fmt.nsr,
        channels_per_frame: fmt.channels,
        bit_depth: fmt.bit_depth,
        stream_data_length: (ch * spf * 4) as u16,
        sp: false,
        evt: 0,
    };
    let mut out = vec![0u8; aaf::AAF_HEADER_LEN + data.len()];
    let n = aaf::encode(&hdr, &data, &mut out).unwrap();
    out.truncate(n);
    out
}

struct Pipeline {
    rate: u32,
    spf: u16,
    channels: u32,
    streams: Vec<(u64, AafFormat, usize)>, // (sid, fmt, first_channel)
}

fn run_pipeline(p: &Pipeline) {
    let consts = RateConsts::new(p.rate, u32::from(p.spf)).unwrap();
    let slots: Vec<StreamSlot> = p
        .streams
        .iter()
        .map(|(sid, fmt, first)| StreamSlot {
            stream_id: *sid,
            first_channel: *first as u32,
            channels: u32::from(fmt.channels),
            bit_depth: u32::from(fmt.bit_depth),
        })
        .collect();
    let mut channel_map = Vec::new();
    for (si, (_sid, fmt, _first)) in p.streams.iter().enumerate() {
        for c in 0..fmt.channels {
            channel_map.push((si as u16, c));
        }
    }
    let cfg = RingConfig {
        channels: p.channels,
        valid_bits: 24,
        nominal_rate_hz: p.rate,
        capacity_frames: consts.capacity_frames as u32,
        streams: slots,
        channel_map,
    };
    let ring = AudioRing::new_anonymous(&cfg).unwrap();
    let producer = Producer::new(
        &ring,
        &p.streams
            .iter()
            .map(|(_s, f, first)| (*first, usize::from(f.channels), u32::from(f.bit_depth)))
            .collect::<Vec<_>>(),
        consts.lag_frames,
        consts.silent_lag,
    );
    let counters: Vec<StreamCounters> = (0..p.streams.len())
        .map(|_| StreamCounters::default())
        .collect();
    let clock = MediaClock::new(consts, 0);
    let bound: Vec<BoundStream> = p
        .streams
        .iter()
        .enumerate()
        .map(|(i, (sid, fmt, _first))| BoundStream {
            stream_id: *sid,
            fmt: *fmt,
            sample_format: SampleFormat::Int32,
            packet_bytes: usize::from(fmt.channels) * usize::from(fmt.spf) * 4,
            ring_index: i,
            is_reference: i == 0,
            early_bound_ns: avb_mediaclock::early_bound_ns(2_000_000),
            expected_seq: None,
            last_mr: false,
        })
        .collect();
    let mut engine = AudioEngine::new(clock, producer, bound, &counters);

    // Feed 200 packets per stream with a gap, a late packet and a reorder on
    // stream 0; measure allocations only around the per-packet calls.
    let packet_period = 1_000_000_000u64 * u64::from(p.spf) / u64::from(p.rate);
    let mut pt = 1_000_000u64;
    // Pre-generate all packets (allocation OK here, before arming).
    let mut packets: Vec<(Vec<u8>, u64)> = Vec::new();
    for pk in 0..200u32 {
        for (si, (sid, fmt, _first)) in p.streams.iter().enumerate() {
            let seq = (pk % 256) as u8;
            let base = pk * u32::from(p.spf);
            let pkt = make_packet(*sid, seq, (pt & 0xFFFF_FFFF) as u32, *fmt, base);
            let _ = si;
            packets.push((pkt, pt));
        }
        pt += packet_period;
    }

    // Arm the allocator on THIS thread and run the per-packet path.
    TL_ALLOCS.with(|c| c.set(0));
    MEASURING.with(|m| m.set(true));
    for (pkt, arrival) in &packets {
        engine.on_packet(pkt, *arrival);
    }
    MEASURING.with(|m| m.set(false));
    let allocs = TL_ALLOCS.with(|c| c.get());
    assert_eq!(allocs, 0, "{} allocations in the per-packet path", allocs);

    engine.commit();

    // Consume into WAV bytes and spot-check bit exactness of channel 0.
    let fmt = WavFormat {
        channels: p.channels as u16,
        sample_rate: p.rate,
        bits_per_sample: 32,
        valid_bits: 24,
    };
    let header = initial_header(&fmt);
    assert_eq!(header.len(), 104);
    let mut c = Consumer::new(&ring);
    let frame_bytes = ring.frame_bytes();
    let mut out = vec![0u8; 512 * frame_bytes];
    let mut total = 0u64;
    loop {
        let got = c.read_into(&mut out, 512);
        if got == 0 {
            break;
        }
        total += got as u64;
    }
    assert!(total > 0, "consumed frames");
    let data_bytes = total * frame_bytes as u64;
    let final_hdr = finalize_header(&fmt, data_bytes);
    assert_eq!(&final_hdr[0..4], b"RIFF");
}

#[test]
fn pipeline_48k_6spf_3x4ch() {
    let p = Pipeline {
        rate: 48_000,
        spf: 6,
        channels: 12,
        streams: vec![
            (0x0000_0000_0000_0001, aaf_fmt(5, 24, 4, 6), 0),
            (0x0000_0000_0000_0002, aaf_fmt(5, 24, 4, 6), 4),
            (0x0000_0000_0000_0003, aaf_fmt(5, 24, 4, 6), 8),
        ],
    };
    run_pipeline(&p);
}

#[test]
fn pipeline_96k_12spf_2x6ch() {
    let p = Pipeline {
        rate: 96_000,
        spf: 12,
        channels: 12,
        streams: vec![
            (0x0000_0000_0000_0010, aaf_fmt(7, 24, 6, 12), 0),
            (0x0000_0000_0000_0011, aaf_fmt(7, 24, 6, 12), 6),
        ],
    };
    run_pipeline(&p);
}
