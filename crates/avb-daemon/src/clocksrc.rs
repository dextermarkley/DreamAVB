// SPDX-License-Identifier: MIT OR Apache-2.0

//! Resolving the user's clock-source selection and publishing the result.
//!
//! The Mac and the 16R do not share an oscillator. CoreAudio produces samples
//! on the host crystal while the 16R consumes them on its own media clock, and
//! a nominal 48 000 Hz on each side is not the same 48 000 Hz — the two drift
//! until the stream underruns or overruns. The fix is to source on a clock the
//! 16R also follows, and [`avb_config::ClockSource`] lets the user say which.
//!
//! THE DAEMON IS THE AUTHORITY. It is the only process that can see all three
//! inputs at once (the selection on disk, the gPTP servo, a recovered media
//! clock), so it resolves them and publishes BOTH the basis (the ring's clock
//! block) and the verdict (the ring's effective-clock code). Everything else
//! reads that. A plug-in or app that re-derived the answer would have to guess
//! at facts it cannot see, and a wrong guess reports one clock while another
//! runs the stream.

use avb_audioring::ClockBlock;
use avb_config::{ClockSource, Config, EffectiveClock};

/// How often the config file is re-read, so flipping the selector in the
/// companion app takes effect on a running daemon instead of at next start.
pub const CONFIG_RELOAD_NS: u64 = 1_000_000_000;

/// What to publish for one resolution.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Decision {
    pub effective: EffectiveClock,
    /// The basis to publish, or `None` when the sample clock is undisciplined.
    ///
    /// `None` is NOT "no information": paired with the effective code it means
    /// "run at nominal", which is a decision a reader must respect rather than
    /// going looking for another clock.
    pub basis: Option<ClockBlock>,
}

/// Resolve the selection and build what to publish. Pure, so the policy is
/// testable without a device.
///
/// `recovered` is the media clock recovered from an incoming 16R stream, when
/// one is locked. `gptp_ppm_vs_mach` is `None` until the servo has a valid
/// model.
pub fn decide(
    sel: ClockSource,
    nominal_hz: f64,
    recovered: Option<ClockBlock>,
    gptp_ppm_vs_mach: Option<f64>,
    gptp_locked: bool,
) -> Decision {
    let stream_locked = recovered.is_some_and(|c| c.locked);
    // gPTP is only usable as a sample-clock reference when the servo is locked
    // AND the mach model is valid — a locked servo with no model cannot say
    // how gPTP runs against the host clock.
    let gptp_rate = gptp_ppm_vs_mach
        .filter(|_| gptp_locked)
        .and_then(|ppm| avb_mediaclock::gptp_derived_rate_hz(nominal_hz, ppm));
    let effective = avb_config::resolve(sel, stream_locked, gptp_rate.is_some());
    let basis = match effective {
        // Mirror the recovered block onto the ring we publish on, so one
        // reader sees one basis wherever it looks.
        EffectiveClock::SixteenRStream => recovered,
        EffectiveClock::NetworkGptp => gptp_rate.map(|rate| ClockBlock {
            // Only `rate_hz_vs_mach` and `locked` are consumed today (see
            // `avb_coreaudio::plugin::ring_disciplined_rate_hz`). The anchor
            // fields describe a recovered stream's frame timeline, which a
            // gPTP-derived clock has no equivalent of, so they stay zero
            // rather than carrying a fabricated anchor.
            anchor_frame: 0,
            anchor_mach: 0.0,
            mach_per_frame: 0.0,
            rate_hz_vs_mach: rate,
            // Zero BY CONSTRUCTION: this clock is derived from gPTP, so it
            // cannot be offset from it. It is not a measurement.
            media_ppm_vs_gptp: 0.0,
            uncertainty_ns: 0.0,
            locked: true,
            reset_count: 0,
        }),
        EffectiveClock::MacInternal(_) => None,
    };
    Decision { effective, basis }
}

/// Tracks the selection for a running loop, re-reading the file so a change in
/// the companion app applies live.
pub struct ClockSelector {
    sel: ClockSource,
    nominal_hz: f64,
    /// Warnings from the last load, drained once so they are logged but not
    /// repeated every second.
    pending_warnings: Vec<String>,
    config_path: Option<String>,
}

impl ClockSelector {
    /// Load the initial selection. The returned warnings and path should be
    /// LOGGED: the daemon and the app agree on `$HOME`, so a daemon started
    /// under `sudo` would silently read a different file, and printing the
    /// path it actually used is what makes that visible.
    pub fn load(nominal_hz: f64) -> Self {
        let (cfg, warnings) = Config::load();
        Self {
            sel: cfg.clock_source,
            nominal_hz,
            pending_warnings: warnings,
            config_path: avb_config::config_path().map(|p| p.display().to_string()),
        }
    }

    pub fn selected(&self) -> ClockSource {
        self.sel
    }

    pub fn config_path(&self) -> Option<&str> {
        self.config_path.as_deref()
    }

    /// Take any warnings accumulated since the last call.
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_warnings)
    }

    /// Re-read the file. Returns the new selection when it CHANGED, so the
    /// caller logs a transition rather than the same line every second.
    pub fn reload(&mut self) -> Option<ClockSource> {
        let (cfg, warnings) = Config::load();
        self.pending_warnings.extend(warnings);
        (cfg.clock_source != self.sel).then(|| {
            self.sel = cfg.clock_source;
            self.sel
        })
    }

    pub fn decide(
        &self,
        recovered: Option<ClockBlock>,
        gptp_ppm_vs_mach: Option<f64>,
        gptp_locked: bool,
    ) -> Decision {
        decide(
            self.sel,
            self.nominal_hz,
            recovered,
            gptp_ppm_vs_mach,
            gptp_locked,
        )
    }
}

/// Which recovered 16R media clock to believe.
///
/// `live` is the clock block our OWN inbound path publishes into the input
/// ring it created; `attached` is one read from an input ring some other
/// process created, mapped read-only at start-up.
///
/// The order matters and is the fix for a real bug: in a duplex session the
/// input ring does not exist until the first AAF packet arrives, so the
/// start-up attach always found nothing. With only that source, `decide` was
/// handed `None` forever and demoted a session that had the 16R's media clock
/// arriving every 125 us down to a gPTP-derived rate — 47999.693 Hz against
/// the real 48000.001 Hz, a 6.4 ppm error the user hears as slow drift.
pub fn recovered_for(live: Option<ClockBlock>, attached: Option<ClockBlock>) -> Option<ClockBlock> {
    live.or(attached)
}

/// Publish a decision onto a ring: the effective code always, the basis when
/// there is one.
///
/// When there is no basis the ring's clock block is marked UNLOCKED rather than
/// left as it was. A stale locked block would otherwise keep a reader on a
/// clock the user has just switched away from.
pub fn publish(ring: &avb_audioring::AudioRing, d: &Decision) {
    ring.set_effective_clock(d.effective.code());
    match d.basis {
        Some(b) => ring.publish_clock(&b),
        None => ring.publish_clock(&ClockBlock {
            locked: false,
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recovered(locked: bool, rate: f64) -> ClockBlock {
        ClockBlock {
            rate_hz_vs_mach: rate,
            locked,
            ..Default::default()
        }
    }

    #[test]
    fn auto_uses_the_recovered_stream_clock_when_one_is_locked() {
        let d = decide(
            ClockSource::Auto,
            48_000.0,
            Some(recovered(true, 47_994.0)),
            Some(-20.0),
            true,
        );
        assert_eq!(d.effective, EffectiveClock::SixteenRStream);
        // The recovered rate is passed through untouched, not recomputed.
        assert_eq!(d.basis.map(|b| b.rate_hz_vs_mach), Some(47_994.0));
    }

    /// The duplex shape, end to end: there is never an attached input ring
    /// (our own inbound path creates it after start-up), so the LIVE block has
    /// to be what reaches `decide`. Feeding only the attached source is what
    /// silently demoted duplex to a gPTP-derived rate.
    #[test]
    fn duplex_recovered_clock_comes_from_the_live_inbound_path() {
        let live = recovered(true, 48_000.001);
        let r = recovered_for(Some(live), None);
        assert_eq!(r.map(|b| b.rate_hz_vs_mach), Some(48_000.001));

        let d = decide(ClockSource::Auto, 48_000.0, r, Some(-6.4), true);
        assert_eq!(d.effective, EffectiveClock::SixteenRStream);
        assert_eq!(d.basis.map(|b| b.rate_hz_vs_mach), Some(48_000.001));

        // With the live source dropped — the bug — the same session lands on
        // gPTP instead, which is the regression this pins.
        let bug = decide(ClockSource::Auto, 48_000.0, None, Some(-6.4), true);
        assert_eq!(bug.effective, EffectiveClock::NetworkGptp);
        assert_ne!(bug.effective, d.effective);
    }

    /// A `play` session alongside a separate `record` has no live inbound path
    /// of its own, so the attached ring must still be used.
    #[test]
    fn a_play_only_session_still_uses_an_attached_input_ring() {
        let attached = recovered(true, 47_994.0);
        let r = recovered_for(None, Some(attached));
        assert_eq!(r.map(|b| b.rate_hz_vs_mach), Some(47_994.0));
        assert_eq!(recovered_for(None, None), None);
    }

    /// The live block wins over an attached one: it describes the stream this
    /// process is actually placing into the ring.
    #[test]
    fn the_live_block_wins_over_an_attached_one() {
        let live = recovered(true, 48_000.001);
        let attached = recovered(true, 47_994.0);
        assert_eq!(
            recovered_for(Some(live), Some(attached)).map(|b| b.rate_hz_vs_mach),
            Some(48_000.001)
        );
    }

    #[test]
    fn auto_falls_back_to_gptp_when_there_is_no_stream() {
        let d = decide(ClockSource::Auto, 48_000.0, None, Some(100.0), true);
        assert_eq!(d.effective, EffectiveClock::NetworkGptp);
        let b = d.basis.expect("a gPTP basis");
        assert!(b.locked);
        assert!((b.rate_hz_vs_mach - 48_004.8).abs() < 1e-6);
        // Derived from gPTP, so it cannot be offset from gPTP.
        assert_eq!(b.media_ppm_vs_gptp, 0.0);
    }

    /// The selection must win over availability, or the control is decorative.
    #[test]
    fn choosing_the_mac_clock_publishes_no_basis_even_with_both_available() {
        let d = decide(
            ClockSource::MacInternal,
            48_000.0,
            Some(recovered(true, 47_994.0)),
            Some(-20.0),
            true,
        );
        assert_eq!(
            d.effective,
            EffectiveClock::MacInternal(avb_config::FallbackReason::Selected)
        );
        assert!(
            d.basis.is_none(),
            "an explicit Mac selection must not publish a network basis"
        );
    }

    /// Asking for the 16R's clock with no stream running must not silently
    /// substitute gPTP: it is a different clock, and the user named one.
    #[test]
    fn choosing_the_stream_clock_without_a_stream_does_not_substitute_gptp() {
        let d = decide(
            ClockSource::SixteenRStream,
            48_000.0,
            None,
            Some(100.0),
            true,
        );
        assert_eq!(
            d.effective,
            EffectiveClock::MacInternal(avb_config::FallbackReason::NoStreamLock)
        );
        assert!(d.basis.is_none());
    }

    #[test]
    fn an_unlocked_recovered_block_does_not_count_as_a_stream() {
        let d = decide(
            ClockSource::SixteenRStream,
            48_000.0,
            Some(recovered(false, 47_994.0)),
            None,
            false,
        );
        assert_eq!(
            d.effective,
            EffectiveClock::MacInternal(avb_config::FallbackReason::NoStreamLock)
        );
        assert!(d.basis.is_none());
    }

    /// A locked servo with no valid mach model cannot say how gPTP runs
    /// against the host clock, so it is not a usable sample-clock reference.
    #[test]
    fn gptp_without_a_mach_model_is_not_usable() {
        let d = decide(ClockSource::NetworkGptp, 48_000.0, None, None, true);
        assert_eq!(
            d.effective,
            EffectiveClock::MacInternal(avb_config::FallbackReason::NoGptpLock)
        );
        assert!(d.basis.is_none());
    }

    #[test]
    fn an_unlocked_servo_is_not_usable_even_with_a_model() {
        let d = decide(ClockSource::NetworkGptp, 48_000.0, None, Some(10.0), false);
        assert_eq!(
            d.effective,
            EffectiveClock::MacInternal(avb_config::FallbackReason::NoGptpLock)
        );
    }

    /// An implausible ppm is refused upstream, so it must not become a basis.
    #[test]
    fn an_implausible_gptp_offset_yields_no_basis() {
        let d = decide(
            ClockSource::NetworkGptp,
            48_000.0,
            None,
            Some(50_000.0),
            true,
        );
        assert!(d.basis.is_none());
        assert!(!d.effective.is_network_disciplined());
    }

    /// Every decision's basis and verdict must agree: a network-disciplined
    /// verdict needs a locked basis, and a Mac verdict needs none. A mismatch
    /// is how a reader ends up on a clock nobody chose.
    #[test]
    fn the_verdict_and_the_basis_never_disagree() {
        for sel in ClockSource::ALL {
            for rec in [
                None,
                Some(recovered(false, 0.0)),
                Some(recovered(true, 47_994.0)),
            ] {
                for ppm in [None, Some(-100.0), Some(0.0)] {
                    for locked in [true, false] {
                        let d = decide(sel, 48_000.0, rec, ppm, locked);
                        match d.effective {
                            EffectiveClock::MacInternal(_) => assert!(
                                d.basis.is_none(),
                                "{sel:?}/{rec:?}/{ppm:?}/{locked}: Mac verdict with a basis"
                            ),
                            _ => {
                                let b = d.basis.unwrap_or_else(|| {
                                    panic!(
                                        "{sel:?}/{rec:?}/{ppm:?}/{locked}: {:?} with no basis",
                                        d.effective
                                    )
                                });
                                assert!(b.locked, "disciplined verdict with an unlocked basis");
                                assert!(
                                    b.rate_hz_vs_mach > 0.0,
                                    "disciplined verdict with a non-positive rate"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn reload_reports_only_a_real_change() {
        let mut s = ClockSelector {
            sel: ClockSource::Auto,
            nominal_hz: 48_000.0,
            pending_warnings: vec!["w".into()],
            config_path: None,
        };
        assert_eq!(s.take_warnings(), vec!["w".to_string()]);
        // Draining is idempotent, so a warning is logged once, not every tick.
        assert!(s.take_warnings().is_empty());
        assert_eq!(s.selected(), ClockSource::Auto);
    }
}
