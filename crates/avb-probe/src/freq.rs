//! EAV frequency-correction math (mirror of `eavCorrectionFromPpb` in
//! driver/AVBBcmDriver/Tg3Logic.hpp).
//!
//! EAV_REF_CLK_CORRECT_CTL: EN bit 31, NEG bit 30, 24-bit magnitude. The
//! magnitude is added to a 24-bit accumulator each tick, so the applied rate
//! change is magnitude / 2^24 = round(|ppb| * 2^24 / 1e9) / 2^24.

pub const CORRECT_ENABLE: u32 = 0x8000_0000;
pub const CORRECT_NEGATIVE: u32 = 0x4000_0000;
pub const CORRECT_MAG_MASK: u32 = 0x00ff_ffff;
const PPB_SCALE: u64 = 1_000_000_000;

/// Register value the dext writes for `ppb` (caller range-checks |ppb| <= 1e6).
pub fn correction_from_ppb(ppb: i64) -> u32 {
    let abs = ppb.unsigned_abs();
    let mag = (((abs << 24) + PPB_SCALE / 2) / PPB_SCALE) as u32 & CORRECT_MAG_MASK;
    if mag == 0 {
        return 0;
    }
    CORRECT_ENABLE | if ppb < 0 { CORRECT_NEGATIVE } else { 0 } | mag
}

/// The ppb a register value actually applies (rounded to nearest).
pub fn ppb_from_correction(reg: u32) -> i64 {
    if reg & CORRECT_ENABLE == 0 {
        return 0;
    }
    let mag = u64::from(reg & CORRECT_MAG_MASK);
    let ppb = ((mag * PPB_SCALE + (1 << 23)) >> 24) as i64;
    if reg & CORRECT_NEGATIVE != 0 {
        -ppb
    } else {
        ppb
    }
}

/// Exact (unrounded) rate change in ppb for a register value.
pub fn expected_rate_ppb(reg: u32) -> f64 {
    if reg & CORRECT_ENABLE == 0 {
        return 0.0;
    }
    let mag = f64::from(reg & CORRECT_MAG_MASK) * 1e9 / f64::from(1u32 << 24);
    if reg & CORRECT_NEGATIVE != 0 {
        -mag
    } else {
        mag
    }
}

/// measured / expected; NaN when nothing was expected.
pub fn scale_factor(measured_delta_ppb: f64, expected_ppb: f64) -> f64 {
    if expected_ppb == 0.0 {
        return f64::NAN;
    }
    measured_delta_ppb / expected_ppb
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Identical to kCorrCases in driver/tests/tg3logic_tests.cpp.
    const GOLDEN: &[(i64, u32)] = &[
        (0, 0x0000_0000),
        (1, 0x0000_0000),
        (-1, 0x0000_0000),
        (60, 0x8000_0001),
        (-60, 0xC000_0001),
        (100_000, 0x8000_068E),
        (-100_000, 0xC000_068E),
        (1_000_000, 0x8000_4189),
        (-1_000_000, 0xC000_4189),
    ];

    #[test]
    fn golden_table_matches_cpp() {
        for &(ppb, reg) in GOLDEN {
            assert_eq!(correction_from_ppb(ppb), reg, "ppb {ppb}");
        }
    }

    #[test]
    fn round_trip_within_half_lsb() {
        for p in [
            0i64, 1, -1, 100_000, -100_000, 1_000_000, -1_000_000, 12_345, -54_321,
        ] {
            let back = ppb_from_correction(correction_from_ppb(p));
            assert!((back - p).abs() <= 30, "{p} -> {back}");
        }
        assert_eq!(ppb_from_correction(0x8000_4189), 999_987);
    }

    #[test]
    fn expected_rate_and_scale() {
        let r = expected_rate_ppb(correction_from_ppb(100_000));
        // 0x68E = 1678; 1678 * 1e9 / 2^24 = 100,016.59 ppb.
        assert!((r - 100_016.59).abs() < 0.01, "{r}");
        assert!(expected_rate_ppb(correction_from_ppb(-100_000)) < 0.0);
        assert_eq!(expected_rate_ppb(0), 0.0);
        assert!((scale_factor(50_000.0, 100_000.0) - 0.5).abs() < 1e-12);
        assert!(scale_factor(1.0, 0.0).is_nan());
    }
}
