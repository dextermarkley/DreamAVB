//! The media-clock lock state machine and reference-stream management
//! (design §10.5). Sans-IO: packet events in, lock transitions and the
//! published model out.

/// Lock thresholds (§10.5). `|e| < 1 µs` to lock, `|e| > 10 µs` to drift.
pub const LOCK_ERR_NS: f64 = 1_000.0;
pub const UNLOCK_ERR_NS: f64 = 10_000.0;
/// Consecutive out-of-range packets that unlock (§10.5: 8).
pub const UNLOCK_RUN: u32 = 8;
/// No packets for this long unlocks (§10.5: 10 ms).
pub const NO_PACKET_UNLOCK_NS: u64 = 10_000_000;
/// A reference interruption longer than this switches reference (§10.5: 100 ms).
pub const REFERENCE_SWITCH_NS: u64 = 100_000_000;

/// Lock state (§10.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockState {
    Unlocked,
    Locked,
}

/// A lock transition the caller maps to the AEM counters (§9.3 CLOCK_DOMAIN
/// LOCKED/UNLOCKED and STREAM_INPUT MEDIA_LOCKED/MEDIA_UNLOCKED).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockEvent {
    Locked,
    Unlocked,
    /// mr toggle: DLL reset + STREAM_INTERRUPTED.
    Reset,
}

/// The lock SM (§10.5).
#[derive(Clone, Copy, Debug)]
pub struct LockSm {
    state: LockState,
    lock_packets_needed: u64,
    in_lock_run: u64,
    out_of_lock_run: u32,
    pub reset_count: u64,
    pub reference_switches: u64,
}

impl LockSm {
    pub fn new(lock_packets_needed: u64) -> Self {
        Self {
            state: LockState::Unlocked,
            lock_packets_needed,
            in_lock_run: 0,
            out_of_lock_run: 0,
            reset_count: 0,
            reference_switches: 0,
        }
    }

    pub fn state(&self) -> LockState {
        self.state
    }

    pub fn is_locked(&self) -> bool {
        self.state == LockState::Locked
    }

    /// A `tv = 1` packet with loop error `e_ns`. Returns a transition if the
    /// lock state changed.
    pub fn on_error(&mut self, e_ns: f64) -> Option<LockEvent> {
        let abs = e_ns.abs();
        match self.state {
            LockState::Unlocked => {
                if abs < LOCK_ERR_NS {
                    self.in_lock_run += 1;
                    if self.in_lock_run >= self.lock_packets_needed {
                        self.state = LockState::Locked;
                        self.out_of_lock_run = 0;
                        return Some(LockEvent::Locked);
                    }
                } else {
                    self.in_lock_run = 0;
                }
                None
            }
            LockState::Locked => {
                if abs > UNLOCK_ERR_NS {
                    self.out_of_lock_run += 1;
                    if self.out_of_lock_run >= UNLOCK_RUN {
                        self.state = LockState::Unlocked;
                        self.in_lock_run = 0;
                        return Some(LockEvent::Unlocked);
                    }
                } else {
                    self.out_of_lock_run = 0;
                }
                None
            }
        }
    }

    /// An mr toggle: unlock, reset the DLL, STREAM_INTERRUPTED (§10.5).
    pub fn on_mr_reset(&mut self) -> LockEvent {
        self.reset_count += 1;
        self.in_lock_run = 0;
        self.out_of_lock_run = 0;
        self.state = LockState::Unlocked;
        LockEvent::Reset
    }

    /// A gap of `gap_ns` with no packets. Unlocks if beyond
    /// [`NO_PACKET_UNLOCK_NS`] (§10.5).
    pub fn on_gap(&mut self, gap_ns: u64) -> Option<LockEvent> {
        if gap_ns > NO_PACKET_UNLOCK_NS && self.state == LockState::Locked {
            self.state = LockState::Unlocked;
            self.in_lock_run = 0;
            self.out_of_lock_run = 0;
            return Some(LockEvent::Unlocked);
        }
        None
    }

    /// Record a reference-stream switch (the reference was interrupted
    /// > [`REFERENCE_SWITCH_NS`]; the DLL is NOT reset, §10.5).
    pub fn record_reference_switch(&mut self) {
        self.reference_switches += 1;
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn locks_after_run_then_unlocks() {
        let mut sm = LockSm::new(5);
        for _ in 0..4 {
            assert_eq!(sm.on_error(500.0), None);
        }
        assert_eq!(sm.on_error(500.0), Some(LockEvent::Locked));
        assert!(sm.is_locked());
        // One excursion does not unlock.
        assert_eq!(sm.on_error(50_000.0), None);
        // A small error resets the out-of-lock run.
        assert_eq!(sm.on_error(100.0), None);
        // 8 consecutive large errors unlock.
        for _ in 0..7 {
            assert_eq!(sm.on_error(20_000.0), None);
        }
        assert_eq!(sm.on_error(20_000.0), Some(LockEvent::Unlocked));
        assert!(!sm.is_locked());
    }

    #[test]
    fn lock_run_resets_on_bad_packet() {
        let mut sm = LockSm::new(3);
        sm.on_error(100.0);
        sm.on_error(100.0);
        sm.on_error(5_000.0); // resets the in-lock run
        sm.on_error(100.0);
        sm.on_error(100.0);
        assert!(!sm.is_locked());
        assert_eq!(sm.on_error(100.0), Some(LockEvent::Locked));
    }

    #[test]
    fn mr_reset_unlocks_and_counts() {
        let mut sm = LockSm::new(1);
        sm.on_error(0.0); // locks
        assert!(sm.is_locked());
        assert_eq!(sm.on_mr_reset(), LockEvent::Reset);
        assert!(!sm.is_locked());
        assert_eq!(sm.reset_count, 1);
    }

    #[test]
    fn gap_unlocks() {
        let mut sm = LockSm::new(1);
        sm.on_error(0.0);
        assert_eq!(sm.on_gap(5_000_000), None); // 5 ms, under threshold
        assert_eq!(sm.on_gap(11_000_000), Some(LockEvent::Unlocked));
    }

    #[test]
    fn reference_switch_counts() {
        let mut sm = LockSm::new(1);
        sm.record_reference_switch();
        sm.record_reference_switch();
        assert_eq!(sm.reference_switches, 2);
    }
}
