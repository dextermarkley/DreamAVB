//! gPTP ↔ mach model (design §7.5, PTP-6).
//!
//! Every 125 ms `ctl` calls `ReadClock(8)` and hands the samples to
//! [`ModelEstimator::push_read`], which keeps the one with the smallest
//! bracket: x = bracket midpoint (mach), y = SYSTIM − last servo offset
//! (= gPTP time). A least-squares line over the last 64 kept samples,
//! rebased to the newest sample to keep f64 precision, gives the published
//! [`GptpMachModel`]. Samples with a bracket > 2 × the window median are
//! left out of the fit.

use std::collections::VecDeque;

use avb_core::{MachTicks, SeqWords, Timebase};

/// Samples in the regression window (8 s at 125 ms).
pub const MODEL_WINDOW: usize = 64;
/// Minimum kept samples before the model is valid.
pub const MODEL_MIN_SAMPLES: usize = 4;
/// Brackets above this multiple of the window median are outliers.
pub const OUTLIER_FACTOR: u64 = 2;

/// One `ReadClock` sample (avb-dext `ClockSample`: mach_before, eav_ns, mach_after).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClockRead {
    pub mach_before: u64,
    pub systim_ns: u64,
    pub mach_after: u64,
}

impl ClockRead {
    fn bracket(&self) -> u64 {
        self.mach_after.saturating_sub(self.mach_before)
    }
}

/// gPTP time as a line in mach time: `gptp(m) = epoch_gptp + slope·ns(m − epoch_mach)`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GptpMachModel {
    pub epoch_mach: u64,
    pub epoch_gptp: u64,
    /// gPTP ns per mach ns.
    pub slope: f64,
    pub valid: bool,
    pub uncertainty_ns: f64,
}

impl GptpMachModel {
    /// gPTP ns at mach `m` (`None` if invalid).
    pub fn gptp_at(&self, m: MachTicks, tb: Timebase) -> Option<u64> {
        if !self.valid {
            return None;
        }
        let dx = tb.ticks_to_ns_f64(m.0 as f64 - self.epoch_mach as f64);
        let y = self.epoch_gptp as i128 + (dx * self.slope).round() as i128;
        u64::try_from(y).ok()
    }

    /// `(slope − 1)·1e6`.
    pub fn ppm_vs_mach(&self) -> f64 {
        (self.slope - 1.0) * 1e6
    }
}

impl SeqWords<5> for GptpMachModel {
    fn to_words(self) -> [u64; 5] {
        [
            self.epoch_mach,
            self.epoch_gptp,
            self.slope.to_bits(),
            u64::from(self.valid),
            self.uncertainty_ns.to_bits(),
        ]
    }

    fn from_words(w: [u64; 5]) -> Self {
        let [epoch_mach, epoch_gptp, slope, valid, unc] = w;
        Self {
            epoch_mach,
            epoch_gptp,
            slope: f64::from_bits(slope),
            valid: valid != 0,
            uncertainty_ns: f64::from_bits(unc),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    /// Bracket midpoint, mach ticks.
    x: u64,
    /// Bracket width, mach ticks.
    bracket: u64,
    /// gPTP ns.
    y: u64,
}

/// The rolling estimator.
#[derive(Clone, Debug)]
pub struct ModelEstimator {
    tb: Timebase,
    samples: VecDeque<Sample>,
    model: GptpMachModel,
    pub rejected_outliers: u64,
}

impl ModelEstimator {
    pub fn new(tb: Timebase) -> Self {
        Self {
            tb,
            samples: VecDeque::with_capacity(MODEL_WINDOW),
            model: GptpMachModel::default(),
            rejected_outliers: 0,
        }
    }

    /// Drop every sample (step, SetClock, GM change).
    pub fn reset(&mut self) {
        self.samples.clear();
        self.model = GptpMachModel::default();
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn model(&self) -> GptpMachModel {
        self.model
    }

    /// Mark the published model invalid without dropping samples
    /// (Listening / HOLDOVER).
    pub fn invalidate(&mut self) {
        self.model.valid = false;
    }

    /// Add the min-bracket sample of one `ReadClock(n)` call. `offset_ns` is
    /// the servo's last offset (SYSTIM − gPTP); `usable` is true in TRACKING
    /// or LOCKED. Returns the refreshed model.
    pub fn push_read(
        &mut self,
        reads: &[ClockRead],
        offset_ns: f64,
        usable: bool,
    ) -> GptpMachModel {
        if !usable {
            self.model.valid = false;
            return self.model;
        }
        let Some(best) = reads
            .iter()
            .filter(|r| r.mach_after >= r.mach_before)
            .min_by_key(|r| r.bracket())
        else {
            return self.model;
        };
        let x = best.mach_before + best.bracket() / 2;
        let y = i128::from(best.systim_ns) - offset_ns.round() as i128;
        let Ok(y) = u64::try_from(y) else {
            return self.model;
        };
        if self.samples.len() >= MODEL_WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(Sample {
            x,
            bracket: best.bracket(),
            y,
        });
        self.fit();
        self.model
    }

    fn fit(&mut self) {
        let mut br: Vec<u64> = self.samples.iter().map(|s| s.bracket).collect();
        br.sort_unstable();
        let median = br.get(br.len() / 2).copied().unwrap_or(0);
        let limit = median.saturating_mul(OUTLIER_FACTOR);
        let kept: Vec<Sample> = self
            .samples
            .iter()
            .copied()
            .filter(|s| s.bracket <= limit)
            .collect();
        self.rejected_outliers = (self.samples.len() - kept.len()) as u64;
        let Some(last) = kept.last().copied() else {
            self.model.valid = false;
            return;
        };
        if kept.len() < MODEL_MIN_SAMPLES {
            self.model.valid = false;
            return;
        }
        // Rebased to the newest kept sample.
        let pts: Vec<(f64, f64)> = kept
            .iter()
            .map(|s| {
                let dx = self.tb.ticks_to_ns_f64(s.x as f64 - last.x as f64);
                let dy = (i128::from(s.y) - i128::from(last.y)) as f64;
                (dx, dy)
            })
            .collect();
        let n = pts.len() as f64;
        let mx = pts.iter().map(|p| p.0).sum::<f64>() / n;
        let my = pts.iter().map(|p| p.1).sum::<f64>() / n;
        let sxx: f64 = pts.iter().map(|p| (p.0 - mx) * (p.0 - mx)).sum();
        let sxy: f64 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
        if sxx <= 0.0 {
            self.model.valid = false;
            return;
        }
        let slope = sxy / sxx;
        let intercept = my - slope * mx;
        let rss: f64 = pts
            .iter()
            .map(|p| {
                let r = p.1 - (intercept + slope * p.0);
                r * r
            })
            .sum();
        let rms = (rss / n).sqrt();
        let half_bracket_ns = self.tb.ticks_to_ns_f64(median as f64) / 2.0;
        let uncertainty_ns = (rms * rms + half_bracket_ns * half_bracket_ns / 3.0).sqrt();
        let epoch_gptp = i128::from(last.y) + intercept.round() as i128;
        self.model = GptpMachModel {
            epoch_mach: last.x,
            epoch_gptp: u64::try_from(epoch_gptp).unwrap_or(last.y),
            slope,
            valid: true,
            uncertainty_ns,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TB: Timebase = Timebase {
        numer: 125,
        denom: 3,
    };

    /// Synthetic host: mach at 24 MHz, gPTP = g0 + (1 − 8e-6)·ns(mach).
    fn reads(k: u64, bracket_ticks: u64, noise: i64) -> [ClockRead; 8] {
        let base = 1_000_000 + k * 3_000_000; // 125 ms in ticks
        let mut out = [ClockRead::default(); 8];
        for (i, r) in out.iter_mut().enumerate() {
            let mb = base + i as u64 * 1000;
            // Only sample 3 has the narrow bracket; others are wide.
            let br = if i == 3 {
                bracket_ticks
            } else {
                bracket_ticks * 5
            };
            let mid = mb + br / 2;
            let ns = TB.ticks_to_ns_f64(mid as f64);
            let g = 1_700_000_000_000_000_000u64 + (ns * (1.0 - 8e-6)).round() as u64;
            *r = ClockRead {
                mach_before: mb,
                systim_ns: (i128::from(g) + i128::from(noise) + 500) as u64,
                mach_after: mb + br,
            };
        }
        out
    }

    #[test]
    fn regression_with_outliers() {
        let mut m = ModelEstimator::new(TB);
        assert!(!m.model().valid);
        let mut model = GptpMachModel::default();
        for k in 0..80u64 {
            // Every 10th call: all eight samples have a huge bracket (preempted).
            let br = if k % 10 == 7 { 2400 } else { 240 };
            let noise = if k % 2 == 0 { 20 } else { -20 };
            model = m.push_read(&reads(k, br, noise), 500.0, true);
        }
        assert!(model.valid);
        assert_eq!(m.len(), MODEL_WINDOW);
        assert!(m.rejected_outliers >= 5, "{}", m.rejected_outliers);
        assert!(
            (model.ppm_vs_mach() - (-8.0)).abs() < 0.01,
            "{}",
            model.ppm_vs_mach()
        );
        // Uncertainty ≈ sqrt(20² + (10000/2)²/3): bracket 240 ticks = 10 us.
        let want = (20.0f64 * 20.0 + 5000.0 * 5000.0 / 3.0).sqrt();
        assert!(
            (model.uncertainty_ns - want).abs() < 5.0,
            "{}",
            model.uncertainty_ns
        );
        // Prediction at the epoch: within the noise of the true value.
        let last = reads(79, 240, 0)[3];
        let mid = last.mach_before + 120;
        let truth = last.systim_ns - 500;
        let got = model.gptp_at(MachTicks(mid), TB).unwrap();
        assert!((got as i128 - truth as i128).abs() < 40, "{got} vs {truth}");
        // SeqWords round trip.
        assert_eq!(GptpMachModel::from_words(model.to_words()), model);
        // Not usable → invalid; reset clears.
        assert!(!m.push_read(&reads(81, 240, 0), 500.0, false).valid);
        m.reset();
        assert!(m.is_empty() && !m.model().valid);
        assert_eq!(m.model().gptp_at(MachTicks(0), TB), None);
    }
}
