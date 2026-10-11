// SPDX-License-Identifier: MIT OR Apache-2.0

//! avb-mediaclock: sans-IO media-clock recovery for the listener (design
//! §10.3–§10.5).
//!
//! The daemon's `audio` thread drives a [`MediaClock`] one `tv = 1` reference
//! packet at a time; it owns the 2nd-order [`dll::Dll`], the [`recovery`] lock
//! SM, [`stats`] windows and the rate-derived [`constants::RateConsts`]. The
//! published [`ClockModel`] is what the ring header's clock block carries
//! (§10.6) and what Phase 1b reads.
//!
//! No I/O, no clock reads, no threads: the caller passes `pt_ns` (gPTP),
//! `now_ns` and the mach mapping. `avb-mediaclock` depends only on `avb-core`
//! and `serde` (design §6.1 — never on `avb-avtp` or `avb-avdecc`; the daemon
//! passes the parsed AAF fields and the reference index `r` from
//! `avb_avdecc::derive::reference_index`).

#![deny(clippy::indexing_slicing)]

pub mod constants;
pub mod dll;
pub mod placement;
pub mod recovery;
pub mod stats;

pub use constants::RateConsts;
pub use dll::Dll;
pub use placement::{classify, early_bound_ns, frame_index, presentation_time_ns, Timing};
pub use recovery::{LockEvent, LockSm, LockState};
pub use stats::{evaluate_mcr5, Mcr5, Window, WindowStats};

use serde::Serialize;

/// The published media-clock model (design §10.5 / §10.6 clock block). The
/// daemon copies these fields into the ring header seqlock; Phase 1b reads
/// them to map ring frames to mach time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct ClockModel {
    /// The sample-frame index the anchor refers to.
    pub anchor_frame: u64,
    /// mach ticks (f64) of `anchor_frame`.
    pub anchor_mach: f64,
    /// mach ticks per sample frame (f64).
    pub mach_per_frame: f64,
    /// Disciplined sample rate vs mach (Hz).
    pub rate_hz_vs_mach: f64,
    /// Media ppm vs gPTP.
    /// Media-clock drift in ppm: the DLL's ppm vs nominal, minus the supplied
    /// `gptp_ppm_vs_mach`. NOTE: the daemon currently passes 0.0 for that term
    /// (the gPTP↔mach mapping is not plumbed into the audio loop), so in
    /// practice this reads as **ppm vs nominal**, not vs gPTP. Readers should
    /// gate it on `locked` and treat it as drift against the nominal rate
    /// until the servo's term is supplied.
    pub media_ppm_vs_gptp: f64,
    /// Model uncertainty (ns).
    pub uncertainty_ns: f64,
    pub locked: bool,
    pub reset_count: u32,
}

/// A reference-stream `tv = 1` packet handed to the media clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefPacket {
    /// Presentation time (gPTP ns), already 32→64 extended by the caller.
    pub pt_ns: u64,
    /// Packets since the previous reference update (k ≥ 1, > 1 across gaps or
    /// tv=0 runs).
    pub k: u64,
}

/// The media clock for one bound set (design §10.5).
#[derive(Clone, Debug)]
pub struct MediaClock {
    consts: RateConsts,
    dll: Dll,
    lock: LockSm,
    stats: WindowStats,
    /// The reference STREAM_INPUT index `r` (from `derive::reference_index`).
    reference_index: usize,
    /// Absolute sample-frame counter of the reference stream (× spf, advanced
    /// by gaps).
    ref_frame_counter: i128,
    /// `n0`/`t0` captured at the last DLL update, for placement.
    n0: i128,
    t0_ns: f64,
    /// Fixed at the first reference packet (§10.6 `n_base`).
    n_base: Option<i128>,
    packets_since_publish: u64,
}

impl MediaClock {
    pub fn new(consts: RateConsts, reference_index: usize) -> Self {
        Self {
            dll: Dll::new(consts),
            lock: LockSm::new(consts.lock_packets),
            stats: WindowStats::new(consts.rate),
            consts,
            reference_index,
            ref_frame_counter: 0,
            n0: 0,
            t0_ns: 0.0,
            n_base: None,
            packets_since_publish: 0,
        }
    }

    pub fn reference_index(&self) -> usize {
        self.reference_index
    }

    pub fn is_locked(&self) -> bool {
        self.lock.is_locked()
    }

    pub fn constants(&self) -> RateConsts {
        self.consts
    }

    pub fn stats(&self) -> &WindowStats {
        &self.stats
    }

    pub fn reset_count(&self) -> u64 {
        self.lock.reset_count
    }

    pub fn reference_switches(&self) -> u64 {
        self.lock.reference_switches
    }

    /// Feed one reference-stream `tv = 1` packet. `now_ns` is gPTP now (for
    /// window bookkeeping); `mach_ns` is the mach time of this packet's pt
    /// (for the ppm window endpoints). Returns a lock transition if any.
    pub fn on_reference_packet(
        &mut self,
        pkt: RefPacket,
        now_ns: u64,
        mach_ns: u64,
    ) -> Option<LockEvent> {
        self.ref_frame_counter += (pkt.k as i128) * i128::from(self.consts.spf);
        if self.n_base.is_none() {
            self.n_base = Some(self.ref_frame_counter - i128::from(self.consts.spf));
        }
        let event = match self.dll.update(pkt.pt_ns, pkt.k) {
            None => {
                // Init packet: anchor placement at this packet (DLL t0 == pt).
                self.n0 = self.ref_frame_counter;
                self.t0_ns = self.dll.t0_abs_ns();
                None
            }
            Some(e) => {
                // The DLL's t0 corresponds to this packet's frame index.
                self.n0 = self.ref_frame_counter;
                self.t0_ns = self.dll.t0_abs_ns();
                self.stats
                    .observe(self.ref_frame_counter, mach_ns, e, now_ns);
                self.lock.on_error(e)
            }
        };
        self.packets_since_publish += 1;
        event
    }

    /// mr toggle on the reference stream: reset the DLL + lock SM (§10.5).
    pub fn on_mr_reset(&mut self) -> LockEvent {
        self.dll.reset();
        self.n_base = None;
        self.lock.on_mr_reset()
    }

    /// A gap of `gap_ns` with no reference packets (§10.5).
    pub fn on_gap(&mut self, gap_ns: u64) -> Option<LockEvent> {
        self.lock.on_gap(gap_ns)
    }

    /// Record a reference-stream switch (no DLL reset, §10.5).
    pub fn record_reference_switch(&mut self) {
        self.lock.record_reference_switch();
    }

    /// Should the model be published this packet (§10.5 MODEL_PUB)?
    pub fn should_publish(&mut self) -> bool {
        if self.packets_since_publish >= self.consts.publish_packets {
            self.packets_since_publish = 0;
            true
        } else {
            false
        }
    }

    /// The absolute sample-frame index for a packet of ANY stream with
    /// presentation `pt_ns`, using the current DLL state (§10.6 placement).
    pub fn frame_index(&self, pt_ns: u64) -> i128 {
        frame_index(
            pt_ns as f64,
            self.t0_ns,
            self.n0,
            self.dll.sample_period_ns(),
        )
    }

    /// The ring frame (relative to `n_base`) for a packet of presentation
    /// `pt_ns`. `None` before the first reference packet fixes `n_base`.
    pub fn ring_frame(&self, pt_ns: u64) -> Option<i128> {
        let n_base = self.n_base?;
        Some(self.frame_index(pt_ns) - n_base)
    }

    /// Build the published [`ClockModel`] (§10.5). `mach_per_frame` and
    /// `anchor_mach` come from the caller's gPTP↔mach mapping of the DLL
    /// anchor; `media_ppm_vs_gptp` is the DLL ppm minus the gPTP ppm vs mach.
    pub fn clock_model(
        &self,
        anchor_mach: f64,
        mach_per_frame: f64,
        gptp_ppm_vs_mach: f64,
    ) -> ClockModel {
        let n_base = self.n_base.unwrap_or(0);
        let anchor_frame = (self.n0 - n_base).max(0) as u64;
        ClockModel {
            anchor_frame,
            anchor_mach,
            mach_per_frame,
            // Disciplined sample rate in Hz = nominal × the DLL's rate ratio.
            // This is the TRUE measured media-clock rate (e.g. 47994 Hz at
            // −118 ppm). The plug-in's GetZeroTimeStamp needs it to run the HAL
            // device timeline at data-arrival rate rather than nominal, which
            // is what otherwise warbles the audio. Always well-defined (the
            // DLL's rate_ratio starts at 1.0 ⇒ nominal before lock), and
            // reported independent of `mach_per_frame` (which the ring producer
            // does not currently supply). The consumer gates on `locked`.
            rate_hz_vs_mach: f64::from(self.consts.rate) * self.dll.rate_ratio(),
            media_ppm_vs_gptp: self.dll.ppm_vs_nominal() - gptp_ppm_vs_mach,
            uncertainty_ns: self.dll.last_error_ns().abs(),
            locked: self.lock.is_locked(),
            reset_count: self.lock.reset_count as u32,
        }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn mc_48k() -> MediaClock {
        MediaClock::new(RateConsts::new(48_000, 6).unwrap(), 0)
    }

    #[test]
    fn locks_on_nominal_stream() {
        let mut mc = mc_48k();
        let p = 125_000u64;
        let mut pt = 1_000_000u64;
        let mut now = 0u64;
        mc.on_reference_packet(RefPacket { pt_ns: pt, k: 1 }, now, pt);
        // lock_packets = 8000; feed 8001 in-lock packets.
        let mut locked_at = None;
        for i in 0..8_200u64 {
            pt += p;
            now += p;
            if let Some(LockEvent::Locked) =
                mc.on_reference_packet(RefPacket { pt_ns: pt, k: 1 }, now, pt)
            {
                locked_at = Some(i);
                break;
            }
        }
        assert!(mc.is_locked());
        assert_eq!(locked_at, Some(7999)); // 8000th in-lock packet
    }

    #[test]
    fn ring_frame_is_dense_from_base() {
        let mut mc = mc_48k();
        let p = 125_000u64;
        let pt0 = 2_000_000u64;
        mc.on_reference_packet(RefPacket { pt_ns: pt0, k: 1 }, 0, pt0);
        // First packet fixes n_base; its ring frame should be ~0.
        let f0 = mc.ring_frame(pt0).unwrap();
        mc.on_reference_packet(
            RefPacket {
                pt_ns: pt0 + p,
                k: 1,
            },
            p,
            pt0 + p,
        );
        let f1 = mc.ring_frame(pt0 + p).unwrap();
        assert_eq!(f1 - f0, 6, "one packet is spf=6 frames apart");
    }

    #[test]
    fn mr_reset_bumps_count_and_unlocks() {
        let mut mc = mc_48k();
        mc.on_reference_packet(
            RefPacket {
                pt_ns: 1_000_000,
                k: 1,
            },
            0,
            1_000_000,
        );
        let ev = mc.on_mr_reset();
        assert_eq!(ev, LockEvent::Reset);
        assert_eq!(mc.reset_count(), 1);
        assert!(!mc.is_locked());
    }

    #[test]
    fn publish_cadence() {
        let mut mc = mc_48k(); // publish_packets = 8
        let mut pt = 1_000_000u64;
        mc.on_reference_packet(RefPacket { pt_ns: pt, k: 1 }, 0, pt);
        let mut publishes = 0;
        for _ in 0..64 {
            pt += 125_000;
            mc.on_reference_packet(RefPacket { pt_ns: pt, k: 1 }, 0, pt);
            if mc.should_publish() {
                publishes += 1;
            }
        }
        assert_eq!(publishes, 64 / 8);
    }

    #[test]
    fn clock_model_reports_lock_and_reset() {
        let mut mc = mc_48k();
        mc.on_reference_packet(
            RefPacket {
                pt_ns: 1_000_000,
                k: 1,
            },
            0,
            1_000_000,
        );
        let m = mc.clock_model(1234.0, 500.0, 0.0);
        assert!(!m.locked);
        assert_eq!(m.reset_count, 0);
        assert!((m.rate_hz_vs_mach - 48_000.0).abs() < 100.0);
    }
}
