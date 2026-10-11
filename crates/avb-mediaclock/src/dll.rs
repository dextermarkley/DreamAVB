// SPDX-License-Identifier: MIT OR Apache-2.0

//! The 2nd-order media-clock DLL (design §10.5).
//!
//! Times are f64 ns relative to an epoch `E` (u64 ns) rebased every 60 s to
//! keep f64 precision. The loop runs in the gPTP domain, one update per
//! `tv = 1` packet, advancing `k` packets after the previous update (k > 1
//! across gaps or tv=0 runs).
//!
//! ```text
//! ω = 2π·B·Tu, B = 1 Hz, Tu = spf/rate  →  b = √2·ω, c = ω²
//! init:  P = spf·1e9/rate;  t0 = pt;  t1 = pt + P
//! per k: t1 += (k−1)·P;  e = pt − t1;  t0 = t1;  t1 += b·e + P;  P += c·e
//! ```

use std::f64::consts::PI;

use crate::constants::RateConsts;

/// Rebase the f64 epoch every this many ns of wall time (§10.5: 60 s).
pub const EPOCH_REBASE_NS: u64 = 60_000_000_000;

/// The DLL state. `t0`/`t1`/`P` are f64 ns relative to [`Dll::epoch`].
#[derive(Clone, Copy, Debug)]
pub struct Dll {
    rate: u32,
    spf: u32,
    b: f64,
    c: f64,
    /// Nominal packet period (ns).
    p_nominal: f64,
    epoch: u64,
    initialised: bool,
    t0: f64,
    t1: f64,
    p: f64,
    /// Last loop error (ns), for the lock SM and stats.
    last_e: f64,
    updates: u64,
}

impl Dll {
    pub fn new(consts: RateConsts) -> Self {
        let tu = consts.tu_s;
        let w = 2.0 * PI * 1.0 * tu; // B = 1 Hz
        Self {
            rate: consts.rate,
            spf: consts.spf,
            b: 2.0f64.sqrt() * w,
            c: w * w,
            p_nominal: consts.p_init_ns,
            epoch: 0,
            initialised: false,
            t0: 0.0,
            t1: 0.0,
            p: consts.p_init_ns,
            last_e: 0.0,
            updates: 0,
        }
    }

    pub fn is_initialised(&self) -> bool {
        self.initialised
    }

    pub fn last_error_ns(&self) -> f64 {
        self.last_e
    }

    pub fn updates(&self) -> u64 {
        self.updates
    }

    /// Current sample period `T = P / spf` (ns).
    pub fn sample_period_ns(&self) -> f64 {
        self.p / f64::from(self.spf)
    }

    /// `t0` as an absolute ns value (epoch + relative).
    pub fn t0_abs_ns(&self) -> f64 {
        self.epoch as f64 + self.t0
    }

    /// Reset to the uninitialised state (mr toggle; §10.5).
    pub fn reset(&mut self) {
        self.initialised = false;
        self.t0 = 0.0;
        self.t1 = 0.0;
        self.p = self.p_nominal;
        self.last_e = 0.0;
    }

    /// Feed one `tv = 1` packet with presentation time `pt_ns` (absolute gPTP
    /// ns), `k` packets after the previous update (k ≥ 1). Returns the loop
    /// error `e` (ns), or `None` on the init packet.
    pub fn update(&mut self, pt_ns: u64, k: u64) -> Option<f64> {
        self.updates += 1;
        if !self.initialised {
            self.epoch = pt_ns;
            self.t0 = 0.0;
            self.t1 = self.p; // pt + P in relative terms (pt == epoch)
            self.p = self.p_nominal;
            self.initialised = true;
            self.last_e = 0.0;
            return None;
        }
        self.maybe_rebase(pt_ns);
        let pt = (pt_ns as f64) - (self.epoch as f64);
        let k = k.max(1);
        self.t1 += ((k - 1) as f64) * self.p;
        let e = pt - self.t1;
        self.t0 = self.t1;
        self.t1 += self.b * e + self.p;
        self.p += self.c * e;
        self.last_e = e;
        Some(e)
    }

    fn maybe_rebase(&mut self, pt_ns: u64) {
        // Rebase when the relative t1 has grown beyond the rebase window, to
        // keep f64 ns additions precise.
        if self.t1 > EPOCH_REBASE_NS as f64 {
            let shift = self.t1.floor();
            self.epoch = self.epoch.wrapping_add(shift as u64);
            self.t0 -= shift;
            self.t1 -= shift;
        }
        let _ = pt_ns;
    }

    /// Current packet period estimate (ns).
    pub fn packet_period_ns(&self) -> f64 {
        self.p
    }

    /// Rate ratio: nominal period / disciplined period. >1 means the source
    /// clock runs faster than nominal.
    pub fn rate_ratio(&self) -> f64 {
        if self.p == 0.0 {
            1.0
        } else {
            self.p_nominal / self.p
        }
    }

    /// ppm of the disciplined packet rate vs the nominal AEM rate.
    pub fn ppm_vs_nominal(&self) -> f64 {
        (self.rate_ratio() - 1.0) * 1e6
    }

    pub fn rate_hz(&self) -> u32 {
        self.rate
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn dll_48k() -> Dll {
        Dll::new(RateConsts::new(48_000, 6).unwrap())
    }

    #[test]
    fn init_then_tracks_nominal() {
        let mut d = dll_48k();
        let p = 125_000u64; // ns/packet at 48k/6spf
        assert_eq!(d.update(1_000_000, 1), None); // init
        assert!(d.is_initialised());
        // A perfectly nominal stream: pt advances exactly P each packet.
        let mut pt = 1_000_000u64;
        for _ in 0..100 {
            pt += p;
            let e = d.update(pt, 1).unwrap();
            assert!(e.abs() < 1.0, "error should stay tiny, got {e}");
        }
        assert!((d.packet_period_ns() - 125_000.0).abs() < 1.0);
        assert!(d.ppm_vs_nominal().abs() < 1.0);
    }

    #[test]
    fn converges_at_plus_100ppm() {
        let mut d = dll_48k();
        // Source runs +100 ppm: its real period is 125000 * (1 - 100e-6)
        // in gPTP ns (faster clock -> shorter period in gPTP time).
        let p_real = 125_000.0 * (1.0 - 100e-6);
        let mut pt = 10_000_000.0f64;
        d.update(pt as u64, 1);
        for _ in 0..20_000 {
            pt += p_real;
            d.update(pt as u64, 1);
        }
        // The DLL should have tracked to about +100 ppm.
        assert!(
            (d.ppm_vs_nominal() - 100.0).abs() < 1.0,
            "ppm {}",
            d.ppm_vs_nominal()
        );
        assert!(
            d.last_error_ns().abs() < 50.0,
            "residual {}",
            d.last_error_ns()
        );
    }

    #[test]
    fn k_advance_over_gap() {
        let mut d = dll_48k();
        let p = 125_000u64;
        let mut pt = 5_000_000u64;
        d.update(pt, 1);
        for _ in 0..50 {
            pt += p;
            d.update(pt, 1);
        }
        // A gap of 10 packets: pt jumps 11*P, k = 11.
        pt += 11 * p;
        let e = d.update(pt, 11).unwrap();
        assert!(e.abs() < 100.0, "gap handled, e = {e}");
    }

    #[test]
    fn ns_quantization_is_bounded() {
        let mut d = dll_48k();
        let p = 125_000.0f64;
        let mut pt = 2_000_000.0f64;
        d.update(pt as u64, 1);
        // Integer-ns pt (quantized) still converges within a few ns of error.
        for _ in 0..5000 {
            pt += p;
            d.update(pt.round() as u64, 1);
        }
        assert!(d.last_error_ns().abs() < 5.0);
    }

    #[test]
    fn reset_reinitialises() {
        let mut d = dll_48k();
        d.update(1_000_000, 1);
        d.update(1_125_000, 1);
        assert!(d.is_initialised());
        d.reset();
        assert!(!d.is_initialised());
        assert_eq!(d.update(9_000_000, 1), None); // first packet after reset
    }

    #[test]
    fn epoch_rebase_keeps_tracking() {
        let mut d = dll_48k();
        let p = 125_000u64;
        // Start at a pt already past the rebase window so t1 grows large.
        let mut pt = 10_000_000u64;
        d.update(pt, 1);
        for _ in 0..600_000 {
            // 600k packets ~ 75 s > 60 s rebase
            pt += p;
            d.update(pt, 1);
        }
        assert!(d.ppm_vs_nominal().abs() < 1.0, "still nominal after rebase");
        assert!(d.last_error_ns().abs() < 5.0);
    }
}
