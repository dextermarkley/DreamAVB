//! Clock statistics: mach timebase conversion, least-squares fit of EAV ns
//! against host time, monotonicity and bracket helpers.

use serde::Serialize;

use crate::abi::ClockSample;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Timebase {
    pub numer: u32,
    pub denom: u32,
}

impl Timebase {
    pub fn mach_to_ns(&self, ticks: u64) -> f64 {
        ticks as f64 * f64::from(self.numer) / f64::from(self.denom)
    }

    pub fn mach_delta_ns(&self, from: u64, to: u64) -> f64 {
        (to as f64 - from as f64) * f64::from(self.numer) / f64::from(self.denom)
    }
}

/// Midpoint of the host-time bracket around a register read.
pub fn midpoint(before: u64, after: u64) -> u64 {
    before + (after.saturating_sub(before)) / 2
}

pub fn is_strictly_monotonic(values: &[u64]) -> bool {
    values.windows(2).all(|w| w[1] > w[0])
}

/// Median of a slice (NaN when empty). Sorts a copy.
pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Fit {
    pub slope: f64,
    pub intercept: f64,
    pub residual_rms: f64,
    pub n: usize,
}

impl Fit {
    /// (slope - 1) in parts per million.
    pub fn ppm(&self) -> f64 {
        (self.slope - 1.0) * 1e6
    }

    /// (slope - 1) in parts per billion.
    pub fn ppb(&self) -> f64 {
        (self.slope - 1.0) * 1e9
    }
}

/// Ordinary least squares y = slope * x + intercept. The data is centred
/// before accumulation so large absolute ns values do not lose precision.
pub fn linear_fit(xs: &[f64], ys: &[f64]) -> Option<Fit> {
    let n = xs.len();
    if n < 2 || ys.len() != n {
        return None;
    }
    let x0 = xs[0];
    let y0 = ys[0];
    let mx = xs.iter().map(|x| x - x0).sum::<f64>() / n as f64;
    let my = ys.iter().map(|y| y - y0).sum::<f64>() / n as f64;
    let (mut sxx, mut sxy) = (0.0, 0.0);
    for (x, y) in xs.iter().zip(ys) {
        let dx = x - x0 - mx;
        let dy = y - y0 - my;
        sxx += dx * dx;
        sxy += dx * dy;
    }
    if sxx == 0.0 {
        return None;
    }
    let slope = sxy / sxx;
    // Intercept in the centred frame, shifted back to absolute coordinates.
    let intercept = (y0 + my) - slope * (x0 + mx);
    let mut ss = 0.0;
    for (x, y) in xs.iter().zip(ys) {
        let pred = (y0 + my) + slope * (x - x0 - mx);
        let r = y - pred;
        ss += r * r;
    }
    Some(Fit {
        slope,
        intercept,
        residual_rms: (ss / n as f64).sqrt(),
        n,
    })
}

/// On the BCM57762 the EAV MSB register reads 0 and the LSB wraps every
/// 2^32 ns (~4.29 s), measured on hardware. When every sample's high word is
/// zero, extend the counter in software (one wrap per decrease; valid while
/// samples are < 4.29 s apart). Returns the unwrapped values and wrap count.
pub fn unwrap32(values: &[u64]) -> (Vec<u64>, usize) {
    if values.iter().any(|v| v >> 32 != 0) {
        return (values.to_vec(), 0);
    }
    let mut wraps = 0u64;
    let mut out = Vec::with_capacity(values.len());
    for (i, v) in values.iter().enumerate() {
        if i > 0 && *v < values[i - 1] {
            wraps += 1;
        }
        out.push(v + (wraps << 32));
    }
    (out, wraps as usize)
}

/// `lo < t < hi`. When the bracket comes from 32-bit-only EAV reads (MSB
/// reads 0) but `t` is a full 64-bit latch, compare modulo 2^32.
pub fn in_bracket(lo: u64, t: u64, hi: u64) -> bool {
    if lo >> 32 == 0 && hi >> 32 == 0 && t >> 32 != 0 {
        let m = 0xffff_ffffu64;
        let span = hi.wrapping_sub(lo) & m;
        let off = t.wrapping_sub(lo) & m;
        off > 0 && off < span
    } else {
        lo < t && t < hi
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ClockStats {
    pub fit: Option<Fit>,
    pub ppm_vs_mach: Option<f64>,
    /// 32-bit wraps unwrapped in software (0 when the MSB is live).
    pub wraps32: usize,
    /// Monotonic after unwrapping.
    pub monotonic: bool,
    pub bracket_median_ns: f64,
    pub bracket_max_ns: f64,
    pub samples: usize,
}

/// Regress EAV ns against the host-time midpoint of each sample's bracket.
pub fn clock_stats(samples: &[ClockSample], tb: Timebase) -> ClockStats {
    let xs: Vec<f64> = samples
        .iter()
        .map(|s| tb.mach_to_ns(midpoint(s.mach_before, s.mach_after)))
        .collect();
    let raw: Vec<u64> = samples.iter().map(|s| s.eav_ns).collect();
    let (eav, wraps32) = unwrap32(&raw);
    let ys: Vec<f64> = eav.iter().map(|v| *v as f64).collect();
    let brackets: Vec<f64> = samples
        .iter()
        .map(|s| tb.mach_delta_ns(s.mach_before, s.mach_after))
        .collect();
    let fit = linear_fit(&xs, &ys);
    ClockStats {
        ppm_vs_mach: fit.map(|f| f.ppm()),
        fit,
        wraps32,
        monotonic: is_strictly_monotonic(&eav),
        bracket_median_ns: median(&brackets),
        bracket_max_ns: brackets.iter().copied().fold(f64::NAN, f64::max),
        samples: samples.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TB_APPLE_SILICON: Timebase = Timebase {
        numer: 125,
        denom: 3,
    };

    #[test]
    fn timebase_conversion() {
        assert_eq!(TB_APPLE_SILICON.mach_to_ns(24_000_000), 1e9);
        assert_eq!(Timebase { numer: 1, denom: 1 }.mach_delta_ns(10, 4), -6.0);
    }

    #[test]
    fn fit_recovers_known_ppm() {
        // EAV running 50 ppm fast relative to host, offset 1e18 ns, 100 ms steps.
        let slope = 1.0 + 50e-6;
        let xs: Vec<f64> = (0..100).map(|i| 1.0e12 + i as f64 * 1e8).collect();
        let ys: Vec<f64> = xs.iter().map(|x| 5.0e11 + slope * x).collect();
        let f = linear_fit(&xs, &ys).unwrap();
        assert!((f.ppm() - 50.0).abs() < 1e-3, "{}", f.ppm());
        assert!(f.residual_rms < 1.0);
    }

    #[test]
    fn clock_stats_on_synthetic_samples() {
        let tb = TB_APPLE_SILICON;
        // 24 ticks = 1000 ns. EAV slow by 20 ppm.
        let samples: Vec<ClockSample> = (0..50u64)
            .map(|i| {
                let before = 1_000_000 + i * 2_400_000; // 100 ms steps
                let after = before + 48; // 2 us bracket
                let mid_ns = tb.mach_to_ns(midpoint(before, after));
                ClockSample {
                    mach_before: before,
                    eav_ns: (mid_ns * (1.0 - 20e-6)) as u64 + 7,
                    mach_after: after,
                }
            })
            .collect();
        let s = clock_stats(&samples, tb);
        assert!(s.monotonic);
        assert!((s.ppm_vs_mach.unwrap() + 20.0).abs() < 0.01);
        assert!((s.bracket_median_ns - 2000.0).abs() < 1e-6);
        assert_eq!(s.samples, 50);
    }

    #[test]
    fn monotonic_and_median() {
        assert!(is_strictly_monotonic(&[1, 2, 3]));
        assert!(!is_strictly_monotonic(&[1, 2, 2]));
        assert!(!is_strictly_monotonic(&[3, 2]));
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
        assert!(median(&[]).is_nan());
        assert_eq!(midpoint(10, 20), 15);
        assert_eq!(midpoint(10, 5), 10);
    }

    #[test]
    fn unwrap32_extends_counter() {
        let (u, w) = unwrap32(&[0xffff_0000, 0x0000_1000, 0x0001_0000]);
        assert_eq!(w, 1);
        assert_eq!(u, vec![0xffff_0000, 0x1_0000_1000, 0x1_0001_0000]);
        // A live MSB disables unwrapping.
        let (u, w) = unwrap32(&[0x1_0000_0005, 0x1_0000_0001]);
        assert_eq!((u, w), (vec![0x1_0000_0005, 0x1_0000_0001], 0));
    }

    #[test]
    fn in_bracket_handles_32bit_reads() {
        // Values measured on the BCM57762 (seq 0x4000).
        assert!(in_bracket(318_873_137, 331_031_367_832, 318_916_864));
        assert!(!in_bracket(
            318_873_137,
            331_031_367_832 + 100_000,
            318_916_864
        ));
        // Bracket straddling a 32-bit wrap.
        assert!(in_bracket(0xffff_fff0, 0x5_0000_0002, 0x10));
        assert!(in_bracket(5, 6, 7));
        assert!(!in_bracket(5, 8, 7));
    }

    #[test]
    fn fit_rejects_degenerate_input() {
        assert!(linear_fit(&[1.0], &[1.0]).is_none());
        assert!(linear_fit(&[1.0, 1.0], &[1.0, 2.0]).is_none());
    }
}
