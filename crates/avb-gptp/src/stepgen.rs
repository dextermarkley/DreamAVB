//! Step bookkeeping (design §7.3 "Step bookkeeping", rev 5).
//!
//! - RX event timestamps get their generation from the dext's slot alone
//!   ([`classify_rx_gen`]); comparisons with the current generation are
//!   equality only, so u32 wrap is irrelevant.
//! - TX timestamps belong to the generation current when `SendFrame` was
//!   called. [`TxGenHold`] makes that unambiguous: no StepClock/SetClock
//!   while a `want_ts` frame is outstanding, and no new `want_ts` frame while
//!   a step is pending.

use avb_core::MachTicks;

/// Generation of an RX event timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RxGen {
    Gen(u32),
    /// Harvested inside a step's settling window (`AVB_RXF_TS_STEP_AMBIG`):
    /// the timestamp may belong to either timebase.
    Ambiguous,
}

/// `flags & AVB_RXF_TS_STEP_AMBIG` → Ambiguous; otherwise `slot.clock_gen`.
pub const fn classify_rx_gen(slot_gen: u32, ambig: bool) -> RxGen {
    if ambig {
        RxGen::Ambiguous
    } else {
        RxGen::Gen(slot_gen)
    }
}

impl RxGen {
    /// Usable for timing in generation `current`.
    pub fn is_current(self, current: u32) -> bool {
        self == RxGen::Gen(current)
    }
}

/// A clock step the servo decided on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// `StepClock(delta_ns)`, |delta| ≤ 1 s.
    Step,
    /// `SetClock(ReadClock() + delta_ns)`, for |offset| > 1 s.
    Set,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingStep {
    pub kind: StepKind,
    pub delta_ns: i64,
    pub decided_at: MachTicks,
}

/// The TX-generation hold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxGenHold {
    /// Tag and send generation of the `want_ts` frame handed to `SendFrame`
    /// whose timestamp has not resolved yet (the scheduler keeps at most one).
    outstanding: Option<(u64, u32)>,
    pending: Option<PendingStep>,
    /// The step was handed to the caller and has not been confirmed yet.
    issued: bool,
}

impl TxGenHold {
    /// A new `want_ts` frame may be queued/sent: no step pending or in flight.
    pub fn want_ts_allowed(&self) -> bool {
        self.pending.is_none() && !self.issued
    }

    pub fn step_pending(&self) -> bool {
        self.pending.is_some() || self.issued
    }

    /// `SendFrame` accepted a `want_ts` frame in generation `gen`.
    pub fn on_want_ts_sent(&mut self, tag: u64, gen: u32) {
        self.outstanding = Some((tag, gen));
    }

    /// Generation the outstanding frame `tag` was sent in.
    pub fn sent_gen(&self, tag: u64) -> Option<u32> {
        match self.outstanding {
            Some((t, g)) if t == tag => Some(g),
            _ => None,
        }
    }

    /// The timestamp for `tag` resolved (OK or timeout).
    pub fn on_ts_resolved(&mut self, tag: u64) {
        if matches!(self.outstanding, Some((t, _)) if t == tag) {
            self.outstanding = None;
        }
    }

    pub fn outstanding(&self) -> bool {
        self.outstanding.is_some()
    }

    /// Record a decided step. A second decision before the first is issued
    /// replaces it (the newer offset is the better one).
    pub fn request(&mut self, step: PendingStep) {
        if !self.issued {
            self.pending = Some(step);
        }
    }

    /// The step, once no timestamp is outstanding. The caller must report
    /// completion with [`TxGenHold::on_step_done`].
    pub fn take(&mut self) -> Option<PendingStep> {
        if self.outstanding.is_some() || self.issued {
            return None;
        }
        let s = self.pending.take()?;
        self.issued = true;
        Some(s)
    }

    /// The issued step returned (applied or rejected).
    pub fn on_step_done(&mut self) {
        self.issued = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify() {
        assert_eq!(classify_rx_gen(7, false), RxGen::Gen(7));
        assert_eq!(classify_rx_gen(7, true), RxGen::Ambiguous);
        assert_eq!(classify_rx_gen(u32::MAX, true), RxGen::Ambiguous);
        assert!(classify_rx_gen(3, false).is_current(3));
        assert!(!classify_rx_gen(3, false).is_current(4));
        assert!(!classify_rx_gen(3, true).is_current(3));
        // Equality only: wrap is irrelevant.
        assert!(classify_rx_gen(0, false).is_current(0));
        assert!(!classify_rx_gen(u32::MAX, false).is_current(0));
    }

    fn step(d: i64) -> PendingStep {
        PendingStep {
            kind: StepKind::Step,
            delta_ns: d,
            decided_at: MachTicks(0),
        }
    }

    #[test]
    fn step_waits_for_outstanding_timestamp() {
        let mut h = TxGenHold::default();
        assert!(h.want_ts_allowed());
        h.on_want_ts_sent(5, 1);
        assert_eq!(h.sent_gen(5), Some(1));
        assert_eq!(h.sent_gen(6), None);
        h.request(step(-200_000));
        assert!(!h.want_ts_allowed());
        assert!(h.step_pending());
        assert_eq!(h.take(), None, "held while the TS is outstanding");
        h.on_ts_resolved(6); // unrelated tag
        assert_eq!(h.take(), None);
        h.on_ts_resolved(5);
        assert_eq!(h.take(), Some(step(-200_000)));
        assert!(!h.want_ts_allowed(), "still in flight until confirmed");
        assert_eq!(h.take(), None);
        h.on_step_done();
        assert!(h.want_ts_allowed());
        assert!(!h.step_pending());
    }

    #[test]
    fn newer_decision_replaces_pending() {
        let mut h = TxGenHold::default();
        h.on_want_ts_sent(1, 0);
        h.request(step(1));
        h.request(step(2));
        h.on_ts_resolved(1);
        assert_eq!(h.take().unwrap().delta_ns, 2);
        // A request while one is in flight is ignored (the Sync after the
        // step re-decides).
        h.request(step(3));
        h.on_step_done();
        assert_eq!(h.take(), None);
    }
}
