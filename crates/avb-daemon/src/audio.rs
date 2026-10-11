//! The audio per-packet pipeline (design §10.2–§10.6). The `audio` thread
//! calls [`AudioEngine::on_packet`] for each AAF slot; it parses, classifies
//! timing, drives the media clock on the reference stream, and places samples
//! into the ring. Nothing on this path allocates or logs (NFR-3): it only
//! bumps atomic counters. The pipeline test asserts 0 allocations here.

use std::sync::atomic::{AtomicU64, Ordering};

use avb_audioring::producer::{Producer, SampleFormat};
use avb_avtp::aaf;
use avb_avtp::stream_format::AafFormat;
use avb_mediaclock::{MediaClock, RefPacket, Timing};

/// Per-stream AAF counters (design §10.2 AAF-5). Atomics, read by `ctl` for
/// AECP GET_COUNTERS and status.
#[derive(Debug, Default)]
pub struct StreamCounters {
    pub packets: AtomicU64,
    pub seq_gap_packets: AtomicU64,
    pub seq_gap_events: AtomicU64,
    pub duplicates: AtomicU64,
    pub out_of_order: AtomicU64,
    pub late: AtomicU64,
    pub early: AtomicU64,
    pub tv0: AtomicU64,
    pub mr_toggles: AtomicU64,
    pub format_mismatch: AtomicU64,
    pub stream_id_mismatch: AtomicU64,
    pub binding_mismatch: AtomicU64,
    pub timestamp_uncertain: AtomicU64,
}

impl StreamCounters {
    fn inc(a: &AtomicU64, by: u64) {
        a.fetch_add(by, Ordering::Relaxed);
    }
}

/// The bound format and addressing for one stream (built by the daemon from
/// the derived model + the effective ACMP/TA values).
#[derive(Clone, Copy, Debug)]
pub struct BoundStream {
    pub stream_id: u64,
    pub fmt: AafFormat,
    pub sample_format: SampleFormat,
    /// `channels × spf × bytes_per_sample` — the expected AAF data length.
    pub packet_bytes: usize,
    /// Index into the producer's stream list / ring stream slot.
    pub ring_index: usize,
    pub is_reference: bool,
    pub early_bound_ns: u64,
    /// Last seen 8-bit sequence number +1 (expected next), None before first.
    pub expected_seq: Option<u8>,
    /// Last mr bit.
    pub last_mr: bool,
}

/// Outcome of placing one packet (for the pipeline test and status).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketOutcome {
    Placed,
    Dropped,
    FormatMismatch,
    Late,
    Early,
}

/// The audio engine: media clock + producer + per-stream state. One per bound
/// set. Driven entirely by `on_packet`; sans-IO.
pub struct AudioEngine<'a> {
    clock: MediaClock,
    producer: Producer<'a>,
    streams: Vec<BoundStream>,
    counters: &'a [StreamCounters],
}

impl<'a> AudioEngine<'a> {
    pub fn new(
        clock: MediaClock,
        producer: Producer<'a>,
        streams: Vec<BoundStream>,
        counters: &'a [StreamCounters],
    ) -> Self {
        Self {
            clock,
            producer,
            streams,
            counters,
        }
    }

    pub fn media_clock(&self) -> &MediaClock {
        &self.clock
    }

    /// Find the bound stream index by stream_id (linear scan ≤ 16; §10.2).
    fn find_stream(&self, stream_id: u64) -> Option<usize> {
        self.streams.iter().position(|s| s.stream_id == stream_id)
    }

    /// Process one AAF slot (the Ethernet payload after the EtherType), with
    /// its arrival gPTP ns. Allocation-free (NFR-3). `commit` is left to the
    /// caller between batches.
    pub fn on_packet(&mut self, payload: &[u8], arrival_ns: u64) -> PacketOutcome {
        let pdu = match aaf::decode(payload) {
            Ok(p) => p,
            Err(_) => return PacketOutcome::Dropped,
        };
        let sid = pdu.hdr.stream.stream_id.0;
        let Some(idx) = self.find_stream(sid) else {
            return PacketOutcome::Dropped; // stream_id_mismatch handled by caller table
        };
        let st = match self.streams.get(idx) {
            Some(s) => *s,
            None => return PacketOutcome::Dropped,
        };
        let ctr = match self.counters.get(idx) {
            Some(c) => c,
            None => return PacketOutcome::Dropped,
        };
        StreamCounters::inc(&ctr.packets, 1);

        // §10.2 step 5: header fields must match the bound format.
        if pdu.hdr.format != st.fmt.format
            || pdu.hdr.nsr != st.fmt.nsr
            || pdu.hdr.channels_per_frame != st.fmt.channels
            || pdu.hdr.bit_depth != st.fmt.bit_depth
            || usize::from(pdu.hdr.stream_data_length) != st.packet_bytes
        {
            StreamCounters::inc(&ctr.format_mismatch, 1);
            return PacketOutcome::FormatMismatch;
        }

        // §10.2 step 6: flags.
        if pdu.hdr.stream.mr != st.last_mr {
            StreamCounters::inc(&ctr.mr_toggles, 1);
            if st.is_reference {
                self.clock.on_mr_reset();
            }
            if let Some(s) = self.streams.get_mut(idx) {
                s.last_mr = pdu.hdr.stream.mr;
            }
        }
        if pdu.hdr.stream.tu {
            StreamCounters::inc(&ctr.timestamp_uncertain, 1);
        }
        if !pdu.hdr.stream.tv {
            StreamCounters::inc(&ctr.tv0, 1);
        }

        // §10.2 step 7: sequence check with d = (seq - expected) mod 256.
        let mut k: u64 = 1;
        if let Some(exp) = st.expected_seq {
            let d = pdu.hdr.stream.seq.wrapping_sub(exp);
            match d {
                0 => {}
                1..=127 => {
                    StreamCounters::inc(&ctr.seq_gap_packets, u64::from(d));
                    StreamCounters::inc(&ctr.seq_gap_events, 1);
                    k = u64::from(d) + 1;
                }
                255 => {
                    StreamCounters::inc(&ctr.duplicates, 1);
                    return PacketOutcome::Dropped;
                }
                _ => {
                    StreamCounters::inc(&ctr.out_of_order, 1);
                    return PacketOutcome::Dropped;
                }
            }
        }
        if let Some(s) = self.streams.get_mut(idx) {
            s.expected_seq = Some(pdu.hdr.stream.seq.wrapping_add(1));
        }

        // Presentation time (§10.3), only meaningful with tv set.
        let pt = if pdu.hdr.stream.tv {
            avb_mediaclock::presentation_time_ns(pdu.hdr.stream.avtp_timestamp, arrival_ns)
                .unwrap_or(arrival_ns)
        } else {
            arrival_ns
        };

        // Drive the media clock on the reference stream's tv packets.
        if st.is_reference && pdu.hdr.stream.tv {
            self.clock
                .on_reference_packet(RefPacket { pt_ns: pt, k }, arrival_ns, arrival_ns);
        }

        // Timing classification (§10.3).
        match avb_mediaclock::classify(pt, arrival_ns, st.early_bound_ns) {
            Timing::Late => {
                StreamCounters::inc(&ctr.late, 1);
            }
            Timing::Early => {
                StreamCounters::inc(&ctr.early, 1);
            }
            Timing::OnTime => {}
        }

        // Placement (§10.6).
        let Some(ring_frame) = self.clock.ring_frame(pt) else {
            return PacketOutcome::Dropped;
        };
        let spf = usize::from(st.fmt.spf);
        let placed =
            self.producer
                .place(st.ring_index, ring_frame, spf, st.sample_format, pdu.data);
        if placed {
            PacketOutcome::Placed
        } else {
            PacketOutcome::Dropped
        }
    }

    /// Advance the ring frontier (called between batches; may allocate nothing).
    pub fn commit(&mut self) {
        self.producer.commit(false, None);
    }

    pub fn is_locked(&self) -> bool {
        self.clock.is_locked()
    }
}
