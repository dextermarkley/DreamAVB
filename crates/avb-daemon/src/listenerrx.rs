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
    /// Not the stream the ring is bound to; the caller may still report it.
    NotPrimary,
    /// No ring yet (creation failed, or the rate/spf was not representable).
    NoRing,
    /// The packet's geometry disagrees with the ring's. NOT placed — the
    /// caller should skip it entirely rather than report it as audio.
    FormatMismatch,
}

/// The inbound audio path: AAF PDUs in, shm input ring out.
pub struct ListenerRx {
    ring: Option<avb_audioring::ShmProducer>,
    mclk: Option<avb_mediaclock::MediaClock>,
    /// The stream the ring is bound to (the first AAF stream seen).
    stream_id: u64,
    spf: usize,
    channels: u16,
    bit_depth: u8,
    format: u8,
    nsr: u8,
    data_len: u16,
    /// A running frame counter. Samples are placed SEQUENTIALLY here, NOT at
    /// the media clock's absolute ring frame: the ring is a FIFO the plug-in
    /// drains in order, and clock-derived absolute frames advance on a
    /// wall-clock timeline the consumer cannot track, lapping it by millions
    /// of frames (audible as overrun garble). The media clock still
    /// timestamps the published clock block for the plug-in's timebase.
    write_pos: u64,
    format_mismatch: u64,
    frames_placed: u64,
}

impl Default for ListenerRx {
    fn default() -> Self {
        Self::new()
    }
}

impl ListenerRx {
    pub fn new() -> Self {
        Self {
            ring: None,
            mclk: None,
            stream_id: 0,
            spf: 0,
            channels: 0,
            bit_depth: 0,
            format: 0,
            nsr: 0,
            data_len: 0,
            write_pos: 0,
            format_mismatch: 0,
            frames_placed: 0,
        }
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

        // Build the ring on the first AAF packet and bind it to that stream as
        // the primary. Geometry comes from the packet, never a literal.
        if self.ring.is_none() {
            self.create_ring(log, pdu, sid);
        }

        if self.ring.is_none() {
            return Placement::NoRing;
        }
        if sid != self.stream_id {
            return Placement::NotPrimary;
        }

        // Re-validate against the geometry the ring was built from. Only the
        // first packet set it; a later packet of a different shape must not be
        // placed.
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
        if pdu.hdr.stream.tv {
            mc.on_reference_packet(
                avb_mediaclock::RefPacket { pt_ns: pt, k: 1 },
                arrival_ns,
                arrival_ns,
            );
        }

        let fmt = if pdu.hdr.format == avb_avtp::aaf::format::INT_24BIT {
            avb_audioring::SampleFormat::Int24
        } else {
            avb_audioring::SampleFormat::Int32
        };
        rp.producer()
            .place(0, self.write_pos as i128, self.spf, fmt, pdu.data);
        rp.producer().commit(false, None);
        self.write_pos += self.spf as u64;
        self.frames_placed += self.spf as u64;

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
            channels: ch as u32,
            valid_bits: depth,
            nominal_rate_hz: rate,
            capacity_frames: consts.capacity_frames as u32,
            streams: vec![avb_audioring::StreamSlot {
                stream_id: sid,
                first_channel: 0,
                channels: ch as u32,
                bit_depth: depth,
            }],
            channel_map: (0..ch).map(|c| (0u16, c as u16)).collect(),
        };
        match avb_audioring::ShmProducer::create(
            &cfg,
            &[(0, ch, u32::from(pdu.hdr.bit_depth.max(24)))],
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
                log.log(
                    Level::Info,
                    "ring",
                    &format!(
                        "shm ring {} bound: stream 0x{sid:016x}, {ch} ch @ {rate} Hz, spf={spf}, cap={}",
                        avb_audioring::SHM_NAME,
                        consts.capacity_frames
                    ),
                );
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
