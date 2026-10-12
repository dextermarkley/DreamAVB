// SPDX-License-Identifier: MIT OR Apache-2.0

//! `listenerrx`: the inbound audio path — AAF PDU in, shm input ring out.
//!
//! This is the LISTENER half of the data path (16R → Mac), factored out of
//! `stack::streams_loop` so the duplex loop runs the same code rather than a
//! second copy of it. Two implementations of a wire path is how this project
//! shipped a wrong `WriteMix` FourCC past five review passes: one of them was
//! never exercised.
//!
//! It owns the ring and the media clock, and nothing else. Sequence counting,
//! WAV capture and peak metering stay with their callers, because those are
//! reporting concerns rather than part of the data path.
//!
//! The ring's geometry comes from the first AAF packet's own header — channel
//! count, rate, samples per frame and bit depth — never from a literal
//! (`AGENTS.md`). Every later packet is re-validated against that geometry:
//! a packet with a different shape would otherwise be placed blind,
//! zero-filling the missing samples while the frame counter kept advancing,
//! which is silent corruption rather than a detectable fault.

use crate::log::{Level, Logger};

/// What happened to one AAF packet offered to [`ListenerRx::on_pdu`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Placed into the ring and committed.
    Placed,
    /// Not a stream this path is bound to; the caller may still report it.
    NotPrimary,
    /// No ring yet (creation failed, or the rate/spf was not representable).
    NoRing,
    /// The packet's geometry disagrees with the ring's. NOT placed — the
    /// caller should skip it entirely rather than report it as audio.
    FormatMismatch,
    /// A non-reference stream's packet arrived before the reference stream had
    /// established the shared timeline, so there is no frame to align it to.
    /// NOT placed: guessing a position would silently offset that stream's
    /// eight channels against the other fifty-six.
    BeforeReference,
}

/// The offset between a packet and the shared timeline basis, in sample
/// frames, from the RAW 32-bit AVTP timestamps.
///
/// This is the whole of the cross-stream arithmetic, a free function because
/// [`ListenerRx::on_pdu`] needs the real shm ring (a fixed system path) and so
/// cannot be exercised in a unit test without clobbering the live one.
///
/// Raw 32-bit, not two independently-unwrapped 64-bit times:
/// `presentation_time_ns` resolves each stream's wrap against its own arrival
/// time, so two streams either side of a boundary get unwrapped into different
/// epochs and their difference jumps by up to 2^31 ns. Measured on the live
/// 16R, the largest bogus "offset" that produced was 102 432 frames — 2.134 s,
/// half the 4.295 s wrap period, which is the signature of the mismatch rather
/// than of real skew. A wrapping 32-bit subtraction read as `i32` is
/// unambiguous for any true offset under 2.1 s, and the real one is well under
/// a millisecond.
pub fn delta_frames(ref_ts: u32, ts: u32, frame_period_ns: f64) -> i128 {
    if frame_period_ns <= 0.0 || !frame_period_ns.is_finite() {
        return 0;
    }
    // Wrapping difference read as signed: correct across the 2^32 ns boundary
    // in both directions.
    let d_ns = i64::from(ts.wrapping_sub(ref_ts) as i32);
    ((d_ns as f64) / frame_period_ns).round() as i128
}

/// The inbound audio path: AAF PDUs in, shm input ring out.
pub struct ListenerRx {
    ring: Option<avb_audioring::ShmProducer>,
    mclk: Option<avb_mediaclock::MediaClock>,
    /// The streams to bind, IN RING-CHANNEL ORDER: `expected[i]` occupies ring
    /// channels `i * channels .. (i + 1) * channels`.
    ///
    /// Supplied by the caller rather than learned from arrival order, because
    /// arrival order is not deterministic and the channel map has to be. The
    /// duplex loop fills it from the 16R's own STREAM_OUTPUT indices, so ring
    /// channels 1-8 really are the device's "Send 1-8".
    ///
    /// EMPTY means the legacy single-stream configuration: bind the first AAF
    /// stream seen and report every other as `NotPrimary`. `record` still uses
    /// that, so there is one implementation rather than two.
    expected: Vec<u64>,
    /// stream id -> slot index, built from `expected` once the ring exists.
    slot_of: std::collections::BTreeMap<u64, usize>,
    /// The slot whose timestamps drive the media clock DLL. One stream only:
    /// feeding eight streams' timestamps into one DLL would present the
    /// inter-stream offsets to it as jitter.
    mclk_slot: Option<usize>,
    /// The stream the ring is bound to in the single-stream configuration.
    stream_id: u64,
    spf: usize,
    channels: u16,
    bit_depth: u8,
    format: u8,
    nsr: u8,
    data_len: u16,
    /// The shared timeline basis: the RAW 32-bit AVTP timestamp of the
    /// furthest-ahead packet seen, and the ring frame it was placed at. Every
    /// packet is positioned at `basis_frame + offset_from(basis_ts)`, and the
    /// basis advances to whichever packet leads — so it tracks the leading
    /// edge across all bound streams rather than one nominated stream.
    ///
    /// The timestamp is kept RAW rather than unwrapped to 64 bits: AVTP
    /// timestamps wrap every 4.295 s, and differencing two independently
    /// unwrapped times put streams either side of a boundary in different
    /// epochs — the largest bogus "offset" measured that way was 102 432
    /// frames, 2.134 s, half the wrap period.
    basis_ts: Option<u32>,
    basis_frame: u64,
    /// Per-slot next sequential frame, used when a packet carries no usable
    /// timestamp so its stream stays continuous through the gap.
    next_frame: Vec<u64>,
    /// Nanoseconds per sample frame, from the stream's own rate.
    frame_period_ns: f64,
    format_mismatch: u64,
    frames_placed: u64,
    /// AAF packets DISCARDED because they belong to a stream other than the
    /// primary, and the distinct stream ids they came from.
    ///
    /// This matters in the single-stream configuration: the 16R advertises
    /// eight AAF streams of eight channels each, and once a controller
    /// connects them it sends all of them. A 25 s `record` run took 1 612 459
    /// audio slots off the wire and placed 1 199 976 frames — exactly one
    /// stream's worth — throwing the other seven away here. Nothing counted
    /// that, so the summary read as a clean 8-channel capture rather than a
    /// 64-channel capture with 56 channels dropped on the floor.
    non_primary_pdus: u64,
    non_primary_streams: std::collections::BTreeSet<u64>,
    /// Packets carrying no valid AVTP timestamp, placed at the current
    /// leading edge on assumption.
    untimed: u64,
    /// Packets whose timestamp would have advanced the shared basis by an
    /// implausible jump, so the basis was left alone and the packet placed at
    /// the current leading edge.
    suspect_jumps: u64,
    /// Suspect jumps per slot, so a single misbehaving stream is identifiable
    /// instead of showing up only as a fraction of the total.
    suspect_per_slot: Vec<u64>,
    /// How many raw-timestamp lines have been dumped (bounded diagnostic).
    dumped: u32,
    /// The largest forward advance the basis will accept from one packet.
    max_advance_frames: i128,
    /// The largest presentation-time offset from the reference actually
    /// OBSERVED, in frames, and the largest that was still inside the window.
    /// Measured rather than assumed: whether the window is too tight or the
    /// anchoring is wrong is not something to reason about from the code.
    max_abs_delta_frames: i128,
    /// Per-slot placed-frame counts, so a slot that binds and never delivers
    /// is visible rather than hidden in the total.
    per_slot_frames: Vec<u64>,
}

impl Default for ListenerRx {
    fn default() -> Self {
        Self::new()
    }
}

impl ListenerRx {
    /// Single-stream: bind the first AAF stream seen, report the rest as
    /// `NotPrimary`. What `record` uses.
    pub fn new() -> Self {
        Self::with_streams(Vec::new())
    }

    /// Bind `expected` streams in ring-channel order, so slot `i` lands on
    /// channels `i * channels_per_stream ..`.
    ///
    /// The order is the CALLER's, not arrival order, because the channel map
    /// has to be reproducible across runs. Slot 0 is the reference: its
    /// packets drive the media clock and the sequential frame counter, and the
    /// others are placed relative to it.
    pub fn with_streams(expected: Vec<u64>) -> Self {
        Self {
            ring: None,
            mclk: None,
            expected,
            slot_of: std::collections::BTreeMap::new(),
            mclk_slot: None,
            stream_id: 0,
            spf: 0,
            channels: 0,
            bit_depth: 0,
            format: 0,
            nsr: 0,
            data_len: 0,
            basis_ts: None,
            basis_frame: 0,
            next_frame: Vec::new(),
            frame_period_ns: 0.0,
            format_mismatch: 0,
            frames_placed: 0,
            non_primary_pdus: 0,
            non_primary_streams: std::collections::BTreeSet::new(),
            untimed: 0,
            suspect_jumps: 0,
            suspect_per_slot: Vec::new(),
            dumped: 0,
            max_advance_frames: 0,
            max_abs_delta_frames: 0,
            per_slot_frames: Vec::new(),
        }
    }

    /// How many streams this path is bound to (1 in the single-stream
    /// configuration, once a stream has been seen).
    pub fn slot_count(&self) -> usize {
        self.per_slot_frames.len()
    }

    /// Frames placed per slot, in ring-channel order. A zero entry is a stream
    /// that bound and delivered nothing — visible here rather than hidden in
    /// the total.
    pub fn per_slot_frames(&self) -> &[u64] {
        &self.per_slot_frames
    }

    /// Channels per bound stream, from the packets' own header.
    pub fn channels_per_slot(&self) -> u16 {
        self.channels
    }

    /// The ring's own placement counters: late drops, placement rejects,
    /// zero-filled frames, gap frames and overrun frames.
    ///
    /// These are the authoritative account of what became of every packet,
    /// maintained by the ring itself rather than by this path. A large
    /// cross-stream offset is only a problem if it actually cost frames, and
    /// these are what say so.
    pub fn ring_counters(&self) -> Option<(u64, u64, u64, u64, u64)> {
        use avb_audioring::counter;
        let r = self.ring.as_ref()?.ring();
        Some((
            r.counter(counter::LATE_DROPS),
            r.counter(counter::PLACEMENT_REJECTS),
            r.counter(counter::ZERO_FILL_FRAMES),
            r.counter(counter::GAP_FRAMES_TOTAL),
            r.counter(counter::OVERRUN_FRAMES),
        ))
    }

    /// The largest offset between a packet and the shared timeline basis that
    /// was actually OBSERVED, in sample frames. A correctly-stepped stream set
    /// stays within one observation interval; a large value means the device's
    /// streams are not in step, or one of them is gapping.
    pub fn max_offset_frames(&self) -> i128 {
        self.max_abs_delta_frames
    }

    /// Packets placed on an assumption rather than on their own timestamp:
    /// ones with no valid AVTP timestamp, and ones whose timestamp would have
    /// jumped the shared basis implausibly far forward.
    pub fn assumed(&self) -> (u64, u64) {
        (self.untimed, self.suspect_jumps)
    }

    /// Suspect forward jumps per slot, in ring-channel order.
    pub fn suspect_per_slot(&self) -> &[u64] {
        &self.suspect_per_slot
    }

    /// AAF packets discarded because they were not the primary stream, and how
    /// many distinct streams they came from.
    ///
    /// A non-zero count means audio the 16R sent us was thrown away: the
    /// single-stream configuration binds one 8-channel stream and the 16R
    /// talks eight of them. Reported so a capture that drops 56 of 64 channels
    /// cannot look like a clean one.
    pub fn non_primary(&self) -> (u64, usize) {
        (self.non_primary_pdus, self.non_primary_streams.len())
    }

    /// Packets refused because their geometry disagreed with the ring's.
    pub fn format_mismatches(&self) -> u64 {
        self.format_mismatch
    }

    /// Audio frames actually placed into the ring. This is the number that
    /// distinguishes "the inbound path ran and moved audio" from "it ran and
    /// moved nothing" — the failure mode that hid the dead output path until
    /// a hardware run showed PDUs equal to underruns.
    pub fn frames_placed(&self) -> u64 {
        self.frames_placed
    }

    /// The stream the ring is bound to, once one has been seen.
    pub fn bound_stream_id(&self) -> Option<u64> {
        self.ring.as_ref().map(|_| self.stream_id)
    }

    /// The 16R's media clock as recovered from the inbound stream, read back
    /// from the clock block this path publishes into the input ring.
    ///
    /// This is the ONLY place the recovered clock is available in a duplex
    /// session. The input ring is created here, on the first AAF packet — so
    /// anything that tried to `ShmRing::attach()` it during start-up found
    /// nothing and silently fell back to a gPTP-derived rate. The 16R's audio
    /// hardware is what the samples are actually paced by, and a gPTP-derived
    /// 48 kHz is a different 48 kHz.
    pub fn recovered_clock(&self) -> Option<avb_audioring::ClockBlock> {
        self.ring.as_ref()?.ring().read_clock()
    }

    pub fn has_ring(&self) -> bool {
        self.ring.is_some()
    }

    /// Offer one decoded AAF packet to the inbound path.
    ///
    /// `arrival_ns` is a monotonic reading taken when the packet was drained;
    /// it is used both as the media clock's arrival time and as the fallback
    /// presentation time for a packet without a valid timestamp (`tv = 0`).
    pub fn on_pdu(
        &mut self,
        log: &Logger,
        pdu: &avb_avtp::aaf::AafPdu<'_>,
        arrival_ns: u64,
    ) -> Placement {
        let sid = pdu.hdr.stream.stream_id.0;

        // Build the ring on the first AAF packet from a stream we expect.
        // Geometry comes from the packet, never a literal.
        if self.ring.is_none() {
            if !self.expected.is_empty() && !self.expected.contains(&sid) {
                self.non_primary_pdus += 1;
                self.non_primary_streams.insert(sid);
                return Placement::NotPrimary;
            }
            self.create_ring(log, pdu, sid);
        }

        if self.ring.is_none() {
            return Placement::NoRing;
        }
        let Some(&slot) = self.slot_of.get(&sid) else {
            self.non_primary_pdus += 1;
            self.non_primary_streams.insert(sid);
            return Placement::NotPrimary;
        };

        // Re-validate against the geometry the ring was built from. Only the
        // first packet set it; a later packet of a different shape must not be
        // placed. Every slot shares one geometry: the 16R's audio streams are
        // identically configured, and a slot that differed would need its own
        // channel width in the ring's layout.
        if pdu.hdr.channels_per_frame != self.channels
            || pdu.hdr.bit_depth != self.bit_depth
            || pdu.hdr.format != self.format
            || pdu.hdr.nsr != self.nsr
            || pdu.hdr.stream_data_length != self.data_len
        {
            self.format_mismatch += 1;
            return Placement::FormatMismatch;
        }

        let (Some(rp), Some(mc)) = (self.ring.as_mut(), self.mclk.as_mut()) else {
            return Placement::NoRing;
        };

        // Drive the media clock from the packets that carry a real timestamp.
        let pt = if pdu.hdr.stream.tv {
            avb_mediaclock::presentation_time_ns(pdu.hdr.stream.avtp_timestamp, arrival_ns)
                .unwrap_or(arrival_ns)
        } else {
            arrival_ns
        };

        // Where this packet goes on the SHARED timeline.
        //
        // One basis for all eight streams: `(basis_ts, basis_frame)`. A
        // packet's frame is the basis frame plus its own offset from the basis
        // timestamp, and the basis then advances to whichever packet is
        // furthest ahead. So the timeline tracks the LEADING EDGE across every
        // bound stream.
        //
        // Two failure modes this avoids, both measured on the live 16R rather
        // than reasoned about:
        //
        // 1. The media clock's ABSOLUTE ring frame advances on a gPTP
        //    wall-clock timeline the consumer cannot track and laps it by
        //    millions of frames (audible as overrun garble). This basis only
        //    ever moves when a packet actually arrives, and only by that
        //    packet's own measured offset, so it advances at exactly the rate
        //    audio is delivered and cannot run away from the consumer.
        //
        // 2. Anchoring to one nominated REFERENCE stream's last packet made
        //    alignment hostage to gaps in that one stream: when it stalled,
        //    every other stream's offset grew with the stall and the largest
        //    observed reached 13 677 frames (285 ms). Tracking the leading
        //    edge means no single stream's gap can strand the basis.
        //
        // A packet behind the basis gets a negative offset and lands in the
        // past, where `place` arbitrates: below the committed frontier it is a
        // late drop, and far ahead it is a placement reject. Both are counted
        // in the ring, so this code does not need a believability window of
        // its own invention on top.
        let ts = pdu.hdr.stream.avtp_timestamp;
        // Raw-timestamp dump for the first few packets of each slot, so the
        // real relationship between the eight streams' timestamps can be read
        // off the wire instead of inferred from aggregate counters.
        if self.dumped < 48 {
            self.dumped += 1;
            log.log(
                Level::Debug,
                "rxts",
                &format!(
                    "slot {slot} sid 0x{sid:016x} seq {} tv {} ts {} (0x{:08x}) arrival {arrival_ns}",
                    pdu.hdr.stream.seq, pdu.hdr.stream.tv, ts, ts
                ),
            );
        }
        // A timestamp of ZERO is not a timestamp, even with tv set.
        //
        // The 16R sends packets with `tv = true` and `avtp_timestamp = 0` —
        // read off the wire, not inferred: at seq 135 all eight streams were
        // in perfect lockstep on ts 3529219834 while one carried ts 0.
        // Differencing against zero yields a garbage offset of up to 2^31 ns,
        // which is exactly the 2147.46 ms "largest offset" and the 14.8% of
        // packets that looked like implausible forward jumps.
        let timed = pdu.hdr.stream.tv && ts != 0;

        let frame: i128 = if timed {
            match self.basis_ts {
                None => {
                    // First timestamped packet of the session fixes the
                    // origin. It also seeds the media clock.
                    self.basis_ts = Some(ts);
                    self.basis_frame = 0;
                    0
                }
                Some(basis_ts) => {
                    let d = delta_frames(basis_ts, ts, self.frame_period_ns);
                    if d.abs() > self.max_abs_delta_frames {
                        self.max_abs_delta_frames = d.abs();
                    }
                    let f = i128::from(self.basis_frame) + d;
                    if d > self.max_advance_frames {
                        // A forward jump this big is not a stream leading the
                        // others, it is a timestamp we should not believe: one
                        // bogus far-future value would drag the basis with it
                        // permanently, leaving a zero-filled hole across all
                        // 64 channels and making every later packet read as
                        // hundreds of milliseconds "behind". The largest
                        // offset observed on the live 16R grew run to run
                        // (494 ms, then 1473 ms), which is that signature.
                        //
                        // Asymmetric on purpose: a large BACKWARD offset is a
                        // genuinely lagging stream and `place` arbitrates it
                        // as a late drop. Only the forward direction can move
                        // the timeline, so only it needs guarding.
                        self.suspect_jumps += 1;
                        if let Some(c) = self.suspect_per_slot.get_mut(slot) {
                            *c += 1;
                        }
                        i128::from(self.basis_frame)
                    } else {
                        if d > 0 {
                            // This packet is the new leading edge.
                            self.basis_ts = Some(ts);
                            self.basis_frame = f.max(0) as u64;
                        }
                        f
                    }
                }
            }
        } else {
            // No usable timestamp. Continue this slot's OWN sequence from
            // where its last packet landed, which is what keeps the stream
            // continuous through a timestamp gap. Placing it at the shared
            // leading edge instead would stack consecutive packets of the
            // same stream on one frame.
            self.untimed += 1;
            let f = self
                .next_frame
                .get(slot)
                .copied()
                .unwrap_or(self.basis_frame);
            i128::from(f)
        };
        // Record where this slot's next packet continues from.
        if let Some(n) = self.next_frame.get_mut(slot) {
            *n = (frame.max(0) as u64).saturating_add(self.spf as u64);
        }
        // The media clock is driven by one stream's timestamps — mixing eight
        // into one DLL would feed it the inter-stream offsets as jitter. The
        // first stream to deliver a timestamped packet is it.
        if timed {
            if self.mclk_slot.is_none() {
                self.mclk_slot = Some(slot);
            }
            if self.mclk_slot == Some(slot) {
                mc.on_reference_packet(
                    avb_mediaclock::RefPacket { pt_ns: pt, k: 1 },
                    arrival_ns,
                    arrival_ns,
                );
            }
        }

        let fmt = if pdu.hdr.format == avb_avtp::aaf::format::INT_24BIT {
            avb_audioring::SampleFormat::Int24
        } else {
            avb_audioring::SampleFormat::Int32
        };
        rp.producer().place(slot, frame, self.spf, fmt, pdu.data);
        // The commit watermark is the MINIMUM over active slots, so the ring
        // only advances once every bound stream has contributed — which is
        // what makes a 64-channel frame whole rather than part-filled. A slot
        // that falls more than `lag_frames` behind is passed and its span
        // logged as a gap, so one stalled stream cannot wedge the other seven.
        rp.producer().commit(false, None);
        self.frames_placed += self.spf as u64;
        if let Some(c) = self.per_slot_frames.get_mut(slot) {
            *c += self.spf as u64;
        }

        if mc.should_publish() {
            let model = mc.clock_model(arrival_ns as f64, 0.0, 0.0);
            rp.ring().publish_clock(&avb_audioring::ClockBlock {
                anchor_frame: model.anchor_frame,
                anchor_mach: model.anchor_mach,
                mach_per_frame: model.mach_per_frame,
                rate_hz_vs_mach: model.rate_hz_vs_mach,
                media_ppm_vs_gptp: model.media_ppm_vs_gptp,
                uncertainty_ns: model.uncertainty_ns,
                locked: model.locked,
                reset_count: model.reset_count,
            });
        }
        Placement::Placed
    }

    fn create_ring(&mut self, log: &Logger, pdu: &avb_avtp::aaf::AafPdu<'_>, sid: u64) {
        let ch = usize::from(pdu.hdr.channels_per_frame.max(1));
        // The slots to build. In the multi-stream configuration this is the
        // caller's ordered list; in the single-stream one it is just the
        // stream that got us here.
        let slots: Vec<u64> = if self.expected.is_empty() {
            vec![sid]
        } else {
            self.expected.clone()
        };
        let rate = avb_avtp::stream_format::nsr_hz(pdu.hdr.nsr).unwrap_or(48_000);
        // Samples per frame = data_len / (channels × bytes per sample).
        let bps = avb_avtp::aaf::bytes_per_sample(pdu.hdr.format).unwrap_or(4);
        let spf = (usize::from(pdu.hdr.stream_data_length) / (ch * bps.max(1))).max(1);
        let Some(consts) = avb_mediaclock::RateConsts::new(rate, spf as u32) else {
            return;
        };
        // A 16R that reports bit_depth 0 means "the format's natural depth";
        // 24 is that for both AAF integer formats we accept.
        let depth = if pdu.hdr.bit_depth == 0 {
            24
        } else {
            u32::from(pdu.hdr.bit_depth)
        };
        let cfg = avb_audioring::RingConfig {
            // TOTAL ring width: every bound stream's channels side by side.
            channels: (slots.len() * ch) as u32,
            valid_bits: depth,
            nominal_rate_hz: rate,
            capacity_frames: consts.capacity_frames as u32,
            // One slot per expected stream, each occupying its own contiguous
            // block of ring channels. Slot order is `expected`'s order, which
            // is the 16R's STREAM_OUTPUT index order, so ring channels 1-8
            // really are the device's "Send 1-8".
            streams: slots
                .iter()
                .enumerate()
                .map(|(i, &id)| avb_audioring::StreamSlot {
                    stream_id: id,
                    first_channel: (i * ch) as u32,
                    channels: ch as u32,
                    bit_depth: depth,
                })
                .collect(),
            channel_map: (0..slots.len())
                .flat_map(|i| (0..ch).map(move |c| (i as u16, c as u16)))
                .collect(),
        };
        let prod_streams: Vec<(usize, usize, u32)> = (0..slots.len())
            .map(|i| (i * ch, ch, u32::from(pdu.hdr.bit_depth.max(24))))
            .collect();
        match avb_audioring::ShmProducer::create(
            &cfg,
            &prod_streams,
            consts.lag_frames,
            consts.silent_lag,
        ) {
            Ok(rp) => {
                self.mclk = Some(avb_mediaclock::MediaClock::new(consts, 0));
                self.ring = Some(rp);
                self.stream_id = sid;
                self.spf = spf;
                self.channels = pdu.hdr.channels_per_frame;
                self.bit_depth = pdu.hdr.bit_depth;
                self.format = pdu.hdr.format;
                self.nsr = pdu.hdr.nsr;
                self.data_len = pdu.hdr.stream_data_length;
                self.frame_period_ns = 1e9 / f64::from(rate.max(1));
                // A stream legitimately leading the others does so by at most
                // a few observation intervals. Eight packets' worth is
                // generous and still two orders of magnitude below the
                // hundreds of milliseconds a bogus timestamp produced.
                self.max_advance_frames = (spf as i128).saturating_mul(8);
                self.slot_of = slots.iter().enumerate().map(|(i, &id)| (id, i)).collect();
                self.per_slot_frames = vec![0; slots.len()];
                self.suspect_per_slot = vec![0; slots.len()];
                self.next_frame = vec![0; slots.len()];
                log.log(
                    Level::Info,
                    "ring",
                    &format!(
                        "shm ring {} bound: {} stream(s) x {ch} ch = {} channels @ {rate} Hz, spf={spf}, cap={}; reference stream 0x{:016x}",
                        avb_audioring::SHM_NAME,
                        slots.len(),
                        slots.len() * ch,
                        consts.capacity_frames,
                        slots.first().copied().unwrap_or(sid),
                    ),
                );
                for (i, id) in slots.iter().enumerate() {
                    log.log(
                        Level::Info,
                        "ring",
                        &format!(
                            "  slot {i}: stream 0x{id:016x} -> ring channels {}..{}",
                            i * ch + 1,
                            (i + 1) * ch
                        ),
                    );
                }
            }
            Err(e) => {
                log.log(
                    Level::Error,
                    "ring",
                    &format!("shm ring create failed: {e}"),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 48 kHz, 6 samples per frame: one AVB observation interval is 125 us.
    const PERIOD_NS: f64 = 1e9 / 48_000.0;
    const SPF: i128 = 6;
    const INTERVAL_NS: u32 = 125_000;

    /// The 16R emits its eight streams back to back inside one interval with
    /// the same presentation time, so corresponding packets share a frame.
    /// This case must be exact: an error here offsets eight channels against
    /// the other fifty-six.
    #[test]
    fn the_same_timestamp_is_a_zero_offset() {
        let ts = 1_000_000_000u32;
        assert_eq!(delta_frames(ts, ts, PERIOD_NS), 0);
    }

    /// A packet one observation interval ahead of the basis is exactly one
    /// packet's worth of frames ahead — which is how a stream that arrives
    /// before the current leading edge lands where that edge is about to be,
    /// making buffering unnecessary.
    #[test]
    fn one_interval_is_one_packet_of_frames() {
        let r = 1_000_000_000u32;
        assert_eq!(delta_frames(r, r.wrapping_add(INTERVAL_NS), PERIOD_NS), SPF);
        assert_eq!(
            delta_frames(r, r.wrapping_sub(INTERVAL_NS), PERIOD_NS),
            -SPF
        );
    }

    /// THE BUG THIS PINS. AVTP timestamps are 32-bit nanoseconds and wrap
    /// every 4.295 s. Differencing two independently-unwrapped 64-bit times
    /// put streams either side of a boundary into different epochs; the
    /// largest bogus offset measured on the live 16R was 102 432 frames
    /// (2.134 s), half the wrap period. On the raw values the same pair is one
    /// interval apart, in both directions.
    #[test]
    fn a_pair_straddling_the_wrap_is_one_interval_apart_not_two_seconds() {
        let r = u32::MAX - (INTERVAL_NS / 2);
        let next = r.wrapping_add(INTERVAL_NS);
        assert!(next < r, "the next packet must really have wrapped");
        assert_eq!(delta_frames(r, next, PERIOD_NS), SPF);
        assert_eq!(delta_frames(next, r, PERIOD_NS), -SPF);

        // What the naive 64-bit difference would have produced instead.
        let bogus = (i128::from(u64::from(next)) - i128::from(u64::from(r))) as f64 / PERIOD_NS;
        assert!(
            bogus.abs() > 100_000.0,
            "the naive difference really is a ~2 s jump: {bogus} frames"
        );
    }

    /// The sign convention must not flip anywhere in the usable range, or a
    /// stream would be placed ahead of the basis when it is behind it.
    #[test]
    fn the_sign_is_stable_across_the_usable_range() {
        let r = 12_345_678u32;
        for ms in [1u32, 10, 100, 1000, 2000] {
            let ns = ms * 1_000_000;
            assert!(
                delta_frames(r, r.wrapping_add(ns), PERIOD_NS) > 0,
                "+{ms} ms"
            );
            assert!(
                delta_frames(r, r.wrapping_sub(ns), PERIOD_NS) < 0,
                "-{ms} ms"
            );
        }
    }

    /// A nonsense period must not produce a nonsense frame offset.
    #[test]
    fn a_bad_frame_period_yields_no_offset() {
        let r = 1_000_000_000u32;
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(delta_frames(r, r.wrapping_add(INTERVAL_NS), bad), 0);
        }
    }

    /// The default configuration is still single-stream, so `record` keeps its
    /// existing behaviour and there is one implementation rather than two.
    #[test]
    fn the_default_configuration_is_single_stream() {
        let rx = ListenerRx::new();
        assert!(rx.expected.is_empty());
        assert_eq!(rx.slot_count(), 0);
        assert_eq!(rx.non_primary(), (0, 0));
        assert_eq!(rx.assumed(), (0, 0));
        assert_eq!(rx.max_offset_frames(), 0);
        assert!(rx.recovered_clock().is_none());
    }

    /// A multi-stream configuration keeps the caller's order: slot i is
    /// `expected[i]`, which is what makes ring channels 1-8 the device's
    /// "Send 1-8" reproducibly rather than by arrival race.
    #[test]
    fn a_multi_stream_configuration_keeps_the_callers_order() {
        let ids: Vec<u64> = (1..=8).map(|i| 0x000a_9203_3ba9_0000 | i).collect();
        let rx = ListenerRx::with_streams(ids.clone());
        assert_eq!(rx.expected, ids);
        // Slots are assigned on ring creation, not construction.
        assert_eq!(rx.slot_count(), 0);
    }
}
