// SPDX-License-Identifier: MIT OR Apache-2.0

//! PI servo with ACQUIRE / TRACKING / LOCKED / HOLDOVER (design §7.3 table).
//!
//! One update per accepted Sync. The caller (the port) only feeds Syncs that
//! are usable: port Slave, asCapable true, current step generation, not
//! ambiguous, no step pending. Frequencies are ppb (positive = SYSTIM runs
//! faster), offsets ns (positive = our SYSTIM is ahead of the GM).

use serde::Serialize;

use crate::stepgen::StepKind;

/// Above this |offset| the step rule fires (StepClock).
pub const STEP_THRESHOLD_NS: f64 = 100_000.0;
/// Above this |offset| the step uses SetClock (StepClock's dext bound).
pub const SET_THRESHOLD_NS: f64 = 1_000_000_000.0;
/// The I210 TIMINCA range, ±488 000 ppb (Phase 0b, avb-dext NicProfile).
pub const MAX_PPB: f64 = 488_000.0;
/// TRACKING → LOCKED after this many consecutive |offset| < [`LOCK_NS`].
pub const LOCK_COUNT: u32 = 16;
pub const LOCK_NS: f64 = 1_000.0;
/// LOCKED → TRACKING after this many consecutive |offset| > [`UNLOCK_NS`].
pub const UNLOCK_COUNT: u32 = 4;
pub const UNLOCK_NS: f64 = 10_000.0;
/// Kp = KP_TS / Ts ppb/ns, Ki = KI_TS / Ts ppb/ns per update (design §7.3).
pub const KP_TS: f64 = 0.2;
pub const KI_TS: f64 = 0.02;
/// Syncs ACQUIRE waits for before seeding the frequency.
pub const ACQUIRE_SYNCS: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ServoState {
    Acquire,
    Tracking,
    Locked,
    Holdover,
}

impl ServoState {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Acquire => "ACQUIRE",
            Self::Tracking => "TRACKING",
            Self::Locked => "LOCKED",
            Self::Holdover => "HOLDOVER",
        }
    }
}

/// What one update asks the clock to do. A step (if any) is issued before
/// the frequency change in ACQUIRE; TRACKING/LOCKED steps carry no change.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ServoOutput {
    pub step: Option<(StepKind, i64)>,
    pub adj_ppb: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ServoCounters {
    pub steps: u64,
    pub set_clocks: u64,
    pub saturations: u64,
    pub unlocks: u64,
    pub acquires: u64,
}

#[derive(Clone, Debug)]
pub struct Servo {
    state: ServoState,
    /// Last frequency commanded with AdjFreq.
    f_applied: f64,
    /// Frequency applied when leaving ACQUIRE; fixed until the next ACQUIRE.
    f_base: f64,
    /// PI integrator, ppb.
    i: f64,
    ts_s: f64,
    kp: f64,
    ki: f64,
    acquire_syncs: u32,
    lock_run: u32,
    unlock_run: u32,
    pub counters: ServoCounters,
}

impl Servo {
    /// `f_applied`: the frequency currently in the NIC (0 after InitDatapath).
    /// `ts_s`: the Sync interval placeholder until the first valid Sync.
    pub fn new(f_applied: f64, ts_s: f64) -> Self {
        let mut s = Self {
            state: ServoState::Acquire,
            f_applied,
            f_base: f_applied,
            i: 0.0,
            ts_s,
            kp: 0.0,
            ki: 0.0,
            acquire_syncs: 0,
            lock_run: 0,
            unlock_run: 0,
            counters: ServoCounters::default(),
        };
        s.set_interval(ts_s);
        s
    }

    pub fn state(&self) -> ServoState {
        self.state
    }

    pub fn f_applied(&self) -> f64 {
        self.f_applied
    }

    pub fn f_base(&self) -> f64 {
        self.f_base
    }

    pub fn integrator(&self) -> f64 {
        self.i
    }

    pub fn gains(&self) -> (f64, f64) {
        (self.kp, self.ki)
    }

    /// Recompute the gains for a new Sync interval without touching the state.
    pub fn set_interval(&mut self, ts_s: f64) {
        if ts_s > 0.0 {
            self.ts_s = ts_s;
            self.kp = KP_TS / ts_s;
            self.ki = KI_TS / ts_s;
        }
    }

    pub fn interval(&self) -> f64 {
        self.ts_s
    }

    /// GM change (or daemon start): back to ACQUIRE.
    pub fn acquire(&mut self) {
        if self.state != ServoState::Acquire {
            self.counters.acquires += 1;
        }
        self.state = ServoState::Acquire;
        self.acquire_syncs = 0;
        self.i = 0.0;
        self.lock_run = 0;
        self.unlock_run = 0;
    }

    /// No Sync for 3·Ts, asCapable false, or port → Listening: freeze f and i.
    /// ACQUIRE stays ACQUIRE (it has nothing to hold).
    pub fn holdover(&mut self) -> bool {
        match self.state {
            ServoState::Tracking | ServoState::Locked => {
                self.state = ServoState::Holdover;
                self.lock_run = 0;
                self.unlock_run = 0;
                true
            }
            ServoState::Acquire | ServoState::Holdover => false,
        }
    }

    /// One usable Sync. `rate_ratio` = (1 + cSRO·2^-41)·NRR_systim.
    pub fn on_sync(&mut self, offset_ns: f64, rate_ratio: f64) -> ServoOutput {
        match self.state {
            ServoState::Acquire => self.acquire_sync(offset_ns, rate_ratio),
            ServoState::Holdover => {
                self.state = ServoState::Tracking;
                self.track(offset_ns)
            }
            ServoState::Tracking | ServoState::Locked => self.track(offset_ns),
        }
    }

    fn step_for(&mut self, offset_ns: f64) -> Option<(StepKind, i64)> {
        if offset_ns.abs() <= STEP_THRESHOLD_NS {
            return None;
        }
        self.counters.steps += 1;
        let kind = if offset_ns.abs() > SET_THRESHOLD_NS {
            self.counters.set_clocks += 1;
            StepKind::Set
        } else {
            StepKind::Step
        };
        Some((kind, (-offset_ns).round() as i64))
    }

    fn acquire_sync(&mut self, offset_ns: f64, rate_ratio: f64) -> ServoOutput {
        self.acquire_syncs += 1;
        if self.acquire_syncs < ACQUIRE_SYNCS {
            return ServoOutput::default();
        }
        let f = (self.f_applied + (rate_ratio - 1.0) * 1e9).clamp(-MAX_PPB, MAX_PPB);
        let step = self.step_for(offset_ns);
        self.f_applied = f;
        self.f_base = f;
        self.i = 0.0;
        self.lock_run = 0;
        self.unlock_run = 0;
        self.state = ServoState::Tracking;
        ServoOutput {
            step,
            adj_ppb: Some(f),
        }
    }

    fn track(&mut self, offset_ns: f64) -> ServoOutput {
        if let Some(step) = self.step_for(offset_ns) {
            // Step rule: count, i := 0, skip the PI this Sync.
            self.i = 0.0;
            self.lock_run = 0;
            self.unlock_run = 0;
            if self.state == ServoState::Locked {
                self.counters.unlocks += 1;
                self.state = ServoState::Tracking;
            }
            return ServoOutput {
                step: Some(step),
                adj_ppb: None,
            };
        }
        let i_candidate = self.i + self.ki * offset_ns;
        let u = self.kp * offset_ns + i_candidate;
        let mut f = self.f_base - u;
        if f.abs() > MAX_PPB {
            // Anti-windup: clamp and keep the integrator frozen.
            f = f.clamp(-MAX_PPB, MAX_PPB);
            self.counters.saturations += 1;
        } else {
            self.i = i_candidate;
        }
        self.f_applied = f;
        match self.state {
            ServoState::Tracking => {
                if offset_ns.abs() < LOCK_NS {
                    self.lock_run += 1;
                    if self.lock_run >= LOCK_COUNT {
                        self.state = ServoState::Locked;
                        self.unlock_run = 0;
                    }
                } else {
                    self.lock_run = 0;
                }
            }
            ServoState::Locked => {
                if offset_ns.abs() > UNLOCK_NS {
                    self.unlock_run += 1;
                    if self.unlock_run >= UNLOCK_COUNT {
                        self.state = ServoState::Tracking;
                        self.counters.unlocks += 1;
                        self.lock_run = 0;
                        self.unlock_run = 0;
                    }
                } else {
                    self.unlock_run = 0;
                }
            }
            ServoState::Acquire | ServoState::Holdover => {}
        }
        ServoOutput {
            step: None,
            adj_ppb: Some(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracking(ts: f64) -> Servo {
        let mut s = Servo::new(0.0, ts);
        s.on_sync(50.0, 1.0);
        let o = s.on_sync(50.0, 1.0 - 83e-6);
        assert_eq!(o.step, None);
        assert!((o.adj_ppb.unwrap() + 83_000.0).abs() < 1e-6);
        assert_eq!(s.state(), ServoState::Tracking);
        s
    }

    #[test]
    fn gains_follow_ts() {
        let s = Servo::new(0.0, 0.125);
        assert_eq!(s.gains(), (1.6, 0.16));
        let mut s = Servo::new(0.0, 1.0);
        assert_eq!(s.gains(), (0.2, 0.02));
        s.set_interval(0.03125);
        assert_eq!(s.gains(), (6.4, 0.64));
        s.set_interval(0.0); // ignored
        assert_eq!(s.gains(), (6.4, 0.64));
    }

    #[test]
    fn acquire_waits_two_syncs_then_steps_and_seeds() {
        let mut s = Servo::new(1000.0, 0.125);
        assert_eq!(s.on_sync(5e6, 1.0), ServoOutput::default());
        assert_eq!(s.state(), ServoState::Acquire);
        let o = s.on_sync(5e6, 1.0 + 10e-6);
        assert_eq!(o.step, Some((StepKind::Step, -5_000_000)));
        assert!((o.adj_ppb.unwrap() - 11_000.0).abs() < 1e-6);
        assert_eq!(s.f_base(), o.adj_ppb.unwrap());
        assert_eq!(s.state(), ServoState::Tracking);
        assert_eq!(s.counters.steps, 1);
        // > 1 s → SetClock; ≤ 100 us → no step.
        let mut s = Servo::new(0.0, 0.125);
        s.on_sync(-2e9, 1.0);
        assert_eq!(
            s.on_sync(-2e9, 1.0).step,
            Some((StepKind::Set, 2_000_000_000))
        );
        assert_eq!(s.counters.set_clocks, 1);
        let mut s = Servo::new(0.0, 0.125);
        s.on_sync(100_000.0, 1.0);
        assert_eq!(s.on_sync(100_000.0, 1.0).step, None);
    }

    #[test]
    fn pi_update_and_anti_windup() {
        let mut s = tracking(0.125);
        let o = s.on_sync(100.0, 1.0);
        // u = 1.6·100 + 0.16·100 = 176; f = −83000 − 176.
        assert!((o.adj_ppb.unwrap() - (-83_176.0)).abs() < 1e-6);
        assert!((s.integrator() - 16.0).abs() < 1e-12);
        // At Ts = 1/32 s (Kp 6.4) a 99 us offset drives f past the clamp:
        // f is clamped and the integrator is not committed.
        let mut s = tracking(0.03125);
        s.on_sync(100.0, 1.0);
        let i0 = s.integrator();
        assert!((i0 - 64.0).abs() < 1e-9);
        let o = s.on_sync(99_000.0, 1.0);
        assert_eq!(o.adj_ppb, Some(-MAX_PPB));
        assert_eq!(s.integrator(), i0);
        assert_eq!(s.counters.saturations, 1);
        // The other direction clamps at +488 000, still frozen.
        let o = s.on_sync(-99_000.0, 1.0);
        assert_eq!(o.adj_ppb, Some(MAX_PPB));
        assert_eq!(s.integrator(), i0);
        assert_eq!(s.counters.saturations, 2);
        // An unclamped update commits again.
        s.on_sync(10.0, 1.0);
        assert!((s.integrator() - (i0 + 6.4)).abs() < 1e-9);
        assert_eq!(s.counters.saturations, 2);
    }

    #[test]
    fn step_rule_in_tracking_and_locked() {
        let mut s = tracking(0.125);
        for _ in 0..3 {
            s.on_sync(10.0, 1.0);
        }
        let o = s.on_sync(150_000.0, 1.0);
        assert_eq!(
            o,
            ServoOutput {
                step: Some((StepKind::Step, -150_000)),
                adj_ppb: None
            }
        );
        assert_eq!(s.integrator(), 0.0);
        assert_eq!(s.state(), ServoState::Tracking);
        for _ in 0..LOCK_COUNT - 1 {
            s.on_sync(999.0, 1.0);
            assert_eq!(s.state(), ServoState::Tracking);
        }
        s.on_sync(-999.0, 1.0);
        assert_eq!(s.state(), ServoState::Locked);
        // Step in LOCKED → TRACKING, counted unlock, i = 0.
        s.on_sync(-200_000.0, 1.0);
        assert_eq!(s.state(), ServoState::Tracking);
        assert_eq!(s.counters.unlocks, 1);
        assert_eq!(s.integrator(), 0.0);
        assert_eq!(s.counters.steps, 2);
    }

    #[test]
    fn lock_run_resets_and_unlock_after_four() {
        let mut s = tracking(0.125);
        for _ in 0..10 {
            s.on_sync(1.0, 1.0);
        }
        s.on_sync(1000.0, 1.0); // not < 1 us: run restarts
        for _ in 0..LOCK_COUNT - 1 {
            s.on_sync(1.0, 1.0);
        }
        assert_eq!(s.state(), ServoState::Tracking);
        s.on_sync(1.0, 1.0);
        assert_eq!(s.state(), ServoState::Locked);
        for _ in 0..3 {
            s.on_sync(20_000.0, 1.0);
        }
        s.on_sync(5.0, 1.0); // breaks the run
        for _ in 0..3 {
            s.on_sync(-20_000.0, 1.0);
        }
        assert_eq!(s.state(), ServoState::Locked);
        s.on_sync(-20_000.0, 1.0);
        assert_eq!(s.state(), ServoState::Tracking);
        assert_eq!(s.counters.unlocks, 1);
    }

    #[test]
    fn holdover_freezes_and_resumes_with_step_rule() {
        let mut s = tracking(0.125);
        s.on_sync(100.0, 1.0);
        let (f, i) = (s.f_applied(), s.integrator());
        assert!(s.holdover());
        assert_eq!(s.state(), ServoState::Holdover);
        assert!(!s.holdover());
        assert_eq!((s.f_applied(), s.integrator()), (f, i));
        // First Sync back: TRACKING, step rule applies.
        let o = s.on_sync(300_000.0, 1.0);
        assert_eq!(o.step, Some((StepKind::Step, -300_000)));
        assert_eq!(s.state(), ServoState::Tracking);
        // ACQUIRE has nothing to hold.
        let mut a = Servo::new(0.0, 0.125);
        assert!(!a.holdover());
        assert_eq!(a.state(), ServoState::Acquire);
        // GM change → ACQUIRE.
        s.acquire();
        assert_eq!(s.state(), ServoState::Acquire);
        assert_eq!(s.counters.acquires, 1);
    }
}
