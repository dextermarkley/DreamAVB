// SPDX-License-Identifier: MIT OR Apache-2.0

//! Deriving a sample clock from gPTP, for the TALKER direction.
//!
//! The recovered media clock in [`crate::MediaClock`] needs an incoming stream
//! to measure: it reads the 16R's own timestamps. A playback-only session has
//! no incoming stream, yet still must not source audio on the Mac's crystal —
//! the Mac and the 16R do not share an oscillator, so a nominal 48 000 Hz on
//! each side is not the same 48 000 Hz and the stream drifts until it breaks.
//!
//! What IS available in that case is gPTP, which is slaved to the 16R as
//! grandmaster. Producing exactly `nominal_hz` samples per gPTP second gives a
//! sample clock referenced to the network's time base rather than to the Mac's
//! crystal.
//!
//! NOTE ON WHAT THIS DOES AND DOES NOT CLAIM: this removes the Mac crystal's
//! error relative to the network. Whether it removes ALL drift against the 16R
//! depends on whether the 16R's own audio oscillator follows gPTP, which is a
//! property of that device and is not knowable from this code. The residual is
//! measurable on hardware as `media_ppm_vs_gptp` while a stream is running;
//! the recovered-stream source ([`crate::MediaClock`]) is the one that tracks
//! the 16R's oscillator directly, and is preferred whenever it is available.

/// Samples per second measured against the MACH clock, for a sample clock that
/// produces `nominal_hz` samples per gPTP second.
///
/// `gptp_ppm_vs_mach` is `avb_gptp::GptpStatus::gptp_ppm_vs_mach`, defined as
/// `(slope - 1) * 1e6` where `slope = d(gptp) / d(mach)`.
///
/// DIRECTION, since getting it backwards is silent and doubles the error
/// instead of removing it: if gPTP runs FASTER than mach (`slope > 1`, ppm
/// positive), then more gPTP time passes per mach second, so more samples must
/// be produced per mach second to still hit `nominal_hz` per gPTP second.
/// Hence the rate scales UP with positive ppm:
///
/// ```text
/// samples/mach_s = nominal_hz · d(gptp)/d(mach) = nominal_hz · (1 + ppm/1e6)
/// ```
///
/// The result is in the mach domain because that is what consumes it: the
/// CoreAudio HAL's device timeline runs on the host clock, so
/// `GetZeroTimeStamp` needs a rate measured against mach, not against gPTP.
///
/// Returns `None` for a non-finite ppm or one so large it is not a clock
/// offset but a broken measurement — better no basis (the caller falls back
/// and says so) than a basis that is wrong.
pub fn gptp_derived_rate_hz(nominal_hz: f64, gptp_ppm_vs_mach: f64) -> Option<f64> {
    if !nominal_hz.is_finite() || nominal_hz <= 0.0 {
        return None;
    }
    if !gptp_ppm_vs_mach.is_finite() || gptp_ppm_vs_mach.abs() > MAX_PLAUSIBLE_PPM {
        return None;
    }
    let rate = nominal_hz * (1.0 + gptp_ppm_vs_mach / 1e6);
    rate.is_finite().then_some(rate)
}

/// Beyond this the reading is a broken measurement, not a crystal offset.
///
/// Commodity crystals are specified in tens of ppm; 1000 ppm is 0.1 %, far
/// outside anything a working oscillator pair shows, and well beyond the
/// roughly -100 ppm measured between this Mac and the 16R. A servo that has
/// not settled can report much larger values transiently, and those must not
/// be turned into a sample rate.
pub const MAX_PLAUSIBLE_PPM: f64 = 1000.0;

#[cfg(test)]
mod tests {
    use super::*;

    /// A worked example rather than a restatement of the formula: at +100 ppm
    /// gPTP-vs-mach, a 48 kHz sample clock must run 4.8 Hz FAST in the mach
    /// domain (48000 × 100/1e6 = 4.8).
    #[test]
    fn positive_ppm_raises_the_rate_by_the_worked_amount() {
        let r = gptp_derived_rate_hz(48_000.0, 100.0).expect("plausible");
        assert!((r - 48_004.8).abs() < 1e-6, "expected 48004.8 Hz, got {r}");
    }

    /// The other direction, because a sign error here is silent: gPTP slower
    /// than mach means fewer samples per mach second.
    #[test]
    fn negative_ppm_lowers_the_rate() {
        let r = gptp_derived_rate_hz(48_000.0, -100.0).expect("plausible");
        assert!((r - 47_995.2).abs() < 1e-6, "expected 47995.2 Hz, got {r}");
        assert!(r < 48_000.0);
    }

    #[test]
    fn zero_ppm_is_exactly_nominal() {
        assert_eq!(gptp_derived_rate_hz(48_000.0, 0.0), Some(48_000.0));
        assert_eq!(gptp_derived_rate_hz(96_000.0, 0.0), Some(96_000.0));
    }

    /// The rate is proportional to nominal, so the same offset scales: at
    /// 96 kHz, +100 ppm is 9.6 Hz rather than 4.8.
    #[test]
    fn the_offset_scales_with_the_nominal_rate() {
        let r = gptp_derived_rate_hz(96_000.0, 100.0).expect("plausible");
        assert!((r - 96_009.6).abs() < 1e-6, "got {r}");
    }

    #[test]
    fn implausible_or_broken_readings_yield_no_basis() {
        assert_eq!(gptp_derived_rate_hz(48_000.0, f64::NAN), None);
        assert_eq!(gptp_derived_rate_hz(48_000.0, f64::INFINITY), None);
        // An unsettled servo can report an enormous offset; it must not become
        // a sample rate.
        assert_eq!(gptp_derived_rate_hz(48_000.0, 50_000.0), None);
        assert_eq!(gptp_derived_rate_hz(48_000.0, -50_000.0), None);
        // A bad nominal rate is refused too.
        assert_eq!(gptp_derived_rate_hz(0.0, 0.0), None);
        assert_eq!(gptp_derived_rate_hz(-48_000.0, 0.0), None);
        assert_eq!(gptp_derived_rate_hz(f64::NAN, 0.0), None);
    }

    /// Right at the plausibility bound it still produces a rate, so the guard
    /// rejects only what it means to.
    #[test]
    fn the_plausibility_bound_is_inclusive() {
        assert!(gptp_derived_rate_hz(48_000.0, MAX_PLAUSIBLE_PPM).is_some());
        assert!(gptp_derived_rate_hz(48_000.0, -MAX_PLAUSIBLE_PPM).is_some());
        let just_over = MAX_PLAUSIBLE_PPM * 1.000_001;
        assert!(gptp_derived_rate_hz(48_000.0, just_over).is_none());
    }
}
