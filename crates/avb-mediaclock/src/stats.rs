//! Media-clock statistics: 10 s windows, ppm vs nominal, sd across windows,
//! RMS of the loop error as residual jitter, and the MCR-5 verdict
//! (design §10.5).

use serde::Serialize;

/// Window length (§10.5: 10 s).
pub const WINDOW_NS: u64 = 10_000_000_000;
/// MCR-5 target: `|media_ppm_vs_gptp| ≤ 0.1 ppm` when the source is PTP.
pub const MCR5_LIMIT_PPM: f64 = 0.1;

/// One completed 10 s window (§10.5).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Window {
    /// Rate ppm vs the nominal AEM rate, from the window endpoints.
    pub ppm_vs_nominal: f64,
    /// RMS of the loop error over the window (ns), the residual jitter.
    pub rms_jitter_ns: f64,
    pub packets: u64,
}

/// Accumulates loop errors and endpoint pairs into 10 s windows (§10.5).
#[derive(Clone, Debug, Default)]
pub struct WindowStats {
    nominal_rate_hz: u32,
    window_start_ns: u64,
    start_sample_index: i128,
    start_mach_ns: u64,
    have_start: bool,
    sq_err_sum: f64,
    packets: u64,
    pub windows: Vec<Window>,
}

impl WindowStats {
    pub fn new(nominal_rate_hz: u32) -> Self {
        Self {
            nominal_rate_hz,
            ..Default::default()
        }
    }

    /// Feed one packet: its sample index, the mach ns of its pt, the loop
    /// error, and the current gPTP ns. Closes a window every 10 s.
    pub fn observe(&mut self, sample_index: i128, mach_ns: u64, e_ns: f64, now_ns: u64) {
        if !self.have_start {
            self.window_start_ns = now_ns;
            self.start_sample_index = sample_index;
            self.start_mach_ns = mach_ns;
            self.have_start = true;
        }
        self.sq_err_sum += e_ns * e_ns;
        self.packets += 1;
        if now_ns.saturating_sub(self.window_start_ns) >= WINDOW_NS {
            self.close_window(sample_index, mach_ns);
            self.window_start_ns = now_ns;
            self.start_sample_index = sample_index;
            self.start_mach_ns = mach_ns;
            self.sq_err_sum = 0.0;
            self.packets = 0;
        }
    }

    fn close_window(&mut self, end_sample_index: i128, end_mach_ns: u64) {
        let frames = (end_sample_index - self.start_sample_index) as f64;
        let mach_elapsed_ns = end_mach_ns.saturating_sub(self.start_mach_ns) as f64;
        let ppm = if mach_elapsed_ns > 0.0 && self.nominal_rate_hz > 0 {
            // measured rate (Hz) = frames / seconds; vs nominal.
            let measured_hz = frames / (mach_elapsed_ns / 1e9);
            (measured_hz / f64::from(self.nominal_rate_hz) - 1.0) * 1e6
        } else {
            0.0
        };
        let rms = if self.packets > 0 {
            (self.sq_err_sum / self.packets as f64).sqrt()
        } else {
            0.0
        };
        self.windows.push(Window {
            ppm_vs_nominal: ppm,
            rms_jitter_ns: rms,
            packets: self.packets,
        });
    }

    /// Standard deviation of per-window ppm across all closed windows (§10.5
    /// target ≤ 0.5 ppm).
    pub fn ppm_sd(&self) -> f64 {
        let n = self.windows.len();
        if n < 2 {
            return 0.0;
        }
        let mean = self.windows.iter().map(|w| w.ppm_vs_nominal).sum::<f64>() / n as f64;
        let var = self
            .windows
            .iter()
            .map(|w| {
                let d = w.ppm_vs_nominal - mean;
                d * d
            })
            .sum::<f64>()
            / (n as f64 - 1.0);
        var.sqrt()
    }
}

/// The MCR-5 verdict (§10.5).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Mcr5 {
    /// Source is PTP; `media_ppm_vs_gptp` within the limit.
    Pass { media_ppm_vs_gptp: f64 },
    /// Source is PTP; out of the ±0.1 ppm limit (a finding).
    Finding { media_ppm_vs_gptp: f64 },
    /// Source is not PTP; not applicable.
    NotApplicable { source_type: String },
}

/// Evaluate MCR-5 (§10.5): compares `|media_ppm_vs_gptp|` to 0.1 ppm when the
/// clock source is PTP, else "not applicable".
pub fn evaluate_mcr5(source_is_ptp: bool, source_type: &str, media_ppm_vs_gptp: f64) -> Mcr5 {
    if !source_is_ptp {
        return Mcr5::NotApplicable {
            source_type: source_type.to_string(),
        };
    }
    if media_ppm_vs_gptp.abs() <= MCR5_LIMIT_PPM {
        Mcr5::Pass { media_ppm_vs_gptp }
    } else {
        Mcr5::Finding { media_ppm_vs_gptp }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn window_closes_every_10s_and_computes_ppm() {
        let mut s = WindowStats::new(48_000);
        // Feed ~2 windows of perfectly nominal packets: 48000 frames/s.
        // One packet = 6 frames = 125 us. mach advances with sample index.
        let t_ns = 1e9 / 48_000.0;
        let mut now = 0u64;
        let mut idx: i128 = 0;
        let mut mach = 0u64;
        for _ in 0..(2 * 10 * 8000 + 10) {
            s.observe(idx, mach, 0.0, now);
            idx += 6;
            now += 125_000;
            mach = (idx as f64 * t_ns) as u64;
        }
        assert!(s.windows.len() >= 2, "closed {} windows", s.windows.len());
        for w in &s.windows {
            assert!(w.ppm_vs_nominal.abs() < 1.0, "ppm {}", w.ppm_vs_nominal);
            assert!(w.rms_jitter_ns < 1.0);
        }
        assert!(s.ppm_sd() < 0.5);
    }

    #[test]
    fn mcr5_verdicts() {
        assert_eq!(
            evaluate_mcr5(true, "ptp", 0.05),
            Mcr5::Pass {
                media_ppm_vs_gptp: 0.05
            }
        );
        assert_eq!(
            evaluate_mcr5(true, "ptp", 0.5),
            Mcr5::Finding {
                media_ppm_vs_gptp: 0.5
            }
        );
        assert_eq!(
            evaluate_mcr5(false, "internal", 0.0),
            Mcr5::NotApplicable {
                source_type: "internal".into()
            }
        );
    }
}
