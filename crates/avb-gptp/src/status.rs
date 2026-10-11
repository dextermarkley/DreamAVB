// SPDX-License-Identifier: MIT OR Apache-2.0

//! gPTP status for logs and JSON (design §7.6, PTP-10).

use std::collections::VecDeque;

use serde::Serialize;

use crate::msg::{ClockIdentity, ClockQuality, PortIdentity};
use crate::port::PortState;
use crate::servo::ServoState;

/// Every counter in §7.6 (revision-4 and revision-5 ones included).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct GptpCounters {
    // Message counters.
    pub sync_rx: u64,
    pub follow_up_rx: u64,
    pub announce_rx: u64,
    pub signaling_rx: u64,
    pub signaling_tx: u64,
    pub pdelay_req_tx: u64,
    pub pdelay_req_rx: u64,
    pub pdelay_resp_tx: u64,
    pub pdelay_resp_rx: u64,
    pub pdelay_resp_fu_tx: u64,
    pub pdelay_resp_fu_rx: u64,
    /// Frames that failed decoding or validation (§13.2).
    pub rx_dropped: u64,
    // Timeouts and losses.
    pub announce_timeouts: u64,
    /// Sync whose Follow_Up did not arrive within 3·Ts.
    pub follow_up_timeouts: u64,
    /// Servo HOLDOVER entries because no usable Sync came for 3·Ts.
    pub sync_timeouts: u64,
    /// Syncs that produced an offset and a servo update.
    pub syncs_used: u64,
    pub lost_responses: u64,
    pub pdelay_tx_ts_fail: u64,
    pub pdelay_resp_ts_fail: u64,
    /// The TX path refused a gPTP frame (outbox full or send error).
    pub pdelay_tx_fail: u64,
    pub multiple_responders: u64,
    /// Event messages without an RX hardware timestamp.
    pub rx_no_ts: u64,
    /// Sync/Follow_Up from a port other than the master port.
    pub foreign_port: u64,
    // Servo.
    pub steps: u64,
    pub set_clocks: u64,
    pub saturations: u64,
    pub unlocks: u64,
    pub gm_changes: u64,
    pub ascapable_transitions: u64,
    // Revision 4.
    pub rx_ts_ambiguous_step: u64,
    pub sync_step_skip: u64,
    pub nrr_out_of_range: u64,
    pub ascapable_slave_loss: u64,
    pub announce_interval_invalid: u64,
    pub announce_rejected: u64,
    pub sync_while_listening: u64,
    pub sync_in_holdover: u64,
    pub sync_interval_invalid: u64,
    pub sync_interval_changes: u64,
    pub pdelay_resp_step_skip: u64,
    /// Our Pdelay exchanges discarded because t1/t4 straddle a step or t4
    /// was ambiguous (not counted as lost).
    pub pdelay_step_discard: u64,
    pub step_gen_changes: u64,
    pub port_state_changes: u64,
    // Revision 5.
    pub pdelay_resp_late_drop: u64,
    pub slot_gen_invalid: u64,
    // Signaling (§7.4).
    pub interval_requests_honored: u64,
    pub interval_requests_rejected: u64,
}

/// A logged edge with its mach time.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Transition {
    pub mach: u64,
    pub from: String,
    pub to: String,
    pub reason: &'static str,
}

/// Bounded transition history (the newest [`Transitions::CAP`] kept).
#[derive(Clone, Debug, Default, Serialize)]
#[serde(transparent)]
pub struct Transitions(VecDeque<Transition>);

impl Transitions {
    pub const CAP: usize = 64;

    pub fn push(&mut self, t: Transition) {
        if self.0.len() >= Self::CAP {
            self.0.pop_front();
        }
        self.0.push_back(t);
    }

    pub fn iter(&self) -> impl Iterator<Item = &Transition> {
        self.0.iter()
    }

    pub fn last(&self) -> Option<&Transition> {
        self.0.back()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Running window of the last `cap` values with mean and (population) sd.
#[derive(Clone, Debug)]
pub struct Window {
    v: VecDeque<f64>,
    cap: usize,
}

impl Window {
    pub fn new(cap: usize) -> Self {
        Self {
            v: VecDeque::with_capacity(cap),
            cap: cap.max(1),
        }
    }

    pub fn push(&mut self, x: f64) {
        if self.v.len() >= self.cap {
            self.v.pop_front();
        }
        self.v.push_back(x);
    }

    pub fn clear(&mut self) {
        self.v.clear();
    }

    pub fn len(&self) -> usize {
        self.v.len()
    }

    pub fn is_empty(&self) -> bool {
        self.v.is_empty()
    }

    pub fn last(&self) -> Option<f64> {
        self.v.back().copied()
    }

    pub fn mean(&self) -> Option<f64> {
        if self.v.is_empty() {
            return None;
        }
        Some(self.v.iter().sum::<f64>() / self.v.len() as f64)
    }

    pub fn sd(&self) -> Option<f64> {
        let m = self.mean()?;
        let var = self.v.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / self.v.len() as f64;
        Some(var.sqrt())
    }

    pub fn max_abs(&self) -> Option<f64> {
        self.v.iter().map(|x| x.abs()).reduce(f64::max)
    }

    pub fn summary(&self) -> Stat {
        Stat {
            last: self.last(),
            mean: self.mean(),
            sd: self.sd(),
            n: self.v.len(),
        }
    }
}

/// last / mean / sd over a window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Stat {
    pub last: Option<f64>,
    pub mean: Option<f64>,
    pub sd: Option<f64>,
    pub n: usize,
}

/// Grandmaster data from the accepted Announce.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GmInfo {
    pub identity: ClockIdentity,
    pub priority1: u8,
    pub priority2: u8,
    pub clock_quality: ClockQuality,
    pub steps_removed: u16,
    pub time_source: u8,
    pub current_utc_offset: i16,
    pub path_trace: Vec<ClockIdentity>,
    pub master_port: PortIdentity,
}

/// The §7.6 snapshot.
#[derive(Clone, Debug, Serialize)]
pub struct GptpStatus {
    pub port_identity: PortIdentity,
    pub port_state: PortState,
    pub as_capable: bool,
    /// GM identity we advertise (our own clockIdentity while Listening).
    pub gm_identity: ClockIdentity,
    pub gm: Option<GmInfo>,
    pub servo_state: ServoState,
    pub step_gen: u32,
    /// ns, positive: our SYSTIM ahead of the GM.
    pub offset_ns: Stat,
    pub path_delay_raw_ns: Stat,
    pub path_delay_comp_ns: Stat,
    pub nrr_systim: Option<f64>,
    pub nrr_free: Option<f64>,
    pub cumulative_rate_ratio: Option<f64>,
    pub applied_ppb: f64,
    pub sync_interval_s: f64,
    pub announce_interval_s: f64,
    pub pdelay_log_interval: i8,
    /// Pdelay_Req → Pdelay_Resp turnaround of our responder (t3 − t2), ns.
    pub responder_turnaround_ns: Stat,
    pub responder_turnaround_max_ns: Option<f64>,
    pub gptp_ppm_vs_mach: Option<f64>,
    pub model_uncertainty_ns: Option<f64>,
    pub lost_responses_since_good: u32,
    pub counters: GptpCounters,
    pub ascapable_history: Transitions,
    pub port_state_history: Transitions,
    pub step_gen_history: Transitions,
    pub servo_history: Transitions,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_stats() {
        let mut w = Window::new(3);
        assert_eq!(w.summary(), Stat::default());
        for x in [1.0, 2.0, 3.0, 4.0] {
            w.push(x);
        }
        assert_eq!(w.len(), 3);
        assert_eq!(w.mean(), Some(3.0));
        assert!((w.sd().unwrap() - (2.0f64 / 3.0).sqrt()).abs() < 1e-12);
        assert_eq!(w.last(), Some(4.0));
        w.push(-9.0);
        assert_eq!(w.max_abs(), Some(9.0));
    }

    #[test]
    fn transitions_bounded() {
        let mut t = Transitions::default();
        for i in 0..100 {
            t.push(Transition {
                mach: i,
                from: "A".into(),
                to: "B".into(),
                reason: "r",
            });
        }
        assert_eq!(t.len(), Transitions::CAP);
        assert_eq!(t.iter().next().unwrap().mach, 100 - Transitions::CAP as u64);
        assert_eq!(t.last().unwrap().mach, 99);
    }
}
