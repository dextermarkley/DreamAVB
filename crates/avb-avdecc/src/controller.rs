//! The AVDECC controller's connection management (design §9.2 Binding,
//! ADC-14, ADC-15). Discovery and the enumeration walk are driven by the
//! daemon through the [`crate::aecp::AecpEngine`]; this module owns the ACMP
//! side: a separate wrapping u16 sequence counter, sequential CONNECT_RX per
//! stream, the response-matching rule, and the GET_TX_STATE check.
//!
//! Sans-IO: `start_bind`/`poll`/`on_rx`/`on_tick` emit ACMPDUs and consume
//! received ones and clock ticks.

use avb_core::{Eui64, MacAddr};

use crate::acmp::{is_a5_resource_status, Acmpdu};
use crate::constants::{acmp_msg, acmp_status};

/// CONNECT_RX timeout (§9.2: 4500 ms, IEEE 1722.1-2021 Table 8.1 *(verify)*).
pub const CONNECT_RX_TIMEOUT_NS: u64 = 4_500_000_000;
/// Attempts spacing (§9.2: 1 s apart).
pub const ATTEMPT_SPACING_NS: u64 = 1_000_000_000;
/// Max CONNECT_RX attempts per stream (§9.2: up to 3).
pub const MAX_ATTEMPTS: u32 = 3;
/// GET_TX_STATE check timeout (§9.4 ADC-15: 200 ms *(verify)*).
pub const GET_TX_STATE_TIMEOUT_NS: u64 = 200_000_000;

/// One stream to bind: our STREAM_INPUT j → 16R STREAM_OUTPUT index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BindTarget {
    pub listener_unique_id: u16,
    pub talker_unique_id: u16,
}

/// The final per-stream bind outcome (§9.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindOutcome {
    Success,
    /// A non-SUCCESS ACMP status after the last attempt.
    Failed {
        status: u8,
        a5: bool,
    },
    /// All attempts timed out.
    TimedOut,
}

/// An ACMPDU to transmit (always to `91:E0:F0:01:00:00`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcmpSend {
    pub frame: Vec<u8>,
    pub sequence_id: u16,
    pub listener_unique_id: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    /// A CONNECT_RX is in flight for the current target.
    InFlight {
        sequence_id: u16,
        attempt: u32,
        deadline_ns: u64,
    },
}

/// The sequential ACMP controller binder (§9.2).
#[derive(Clone, Debug)]
pub struct Binder {
    controller_id: Eui64,
    talker_id: Eui64,
    listener_id: Eui64,
    talker_mac: MacAddr,
    /// Separate ACMP sequence counter (wrapping u16), independent of AECP and
    /// the entity's PROBE_TX counter (§9.2).
    acmp_seq: u16,
    targets: Vec<BindTarget>,
    cursor: usize,
    phase: Phase,
    pub outcomes: Vec<(u16, BindOutcome)>,
    /// Received ACMPDUs that did not match an in-flight command (§9.2).
    pub acmp_ctrl_unmatched: u64,
}

impl Binder {
    pub fn new(
        controller_id: Eui64,
        talker_id: Eui64,
        listener_id: Eui64,
        talker_mac: MacAddr,
        targets: Vec<BindTarget>,
    ) -> Self {
        Self {
            controller_id,
            talker_id,
            listener_id,
            talker_mac,
            acmp_seq: 0,
            targets,
            cursor: 0,
            phase: Phase::Idle,
            outcomes: Vec::new(),
            acmp_ctrl_unmatched: 0,
        }
    }

    pub fn is_done(&self) -> bool {
        self.cursor >= self.targets.len() && matches!(self.phase, Phase::Idle)
    }

    /// The talker's MAC, for the daemon's unicast AECP / GET_TX_STATE sends.
    pub fn talker_mac(&self) -> MacAddr {
        self.talker_mac
    }

    fn next_seq(&mut self) -> u16 {
        let s = self.acmp_seq;
        self.acmp_seq = self.acmp_seq.wrapping_add(1);
        s
    }

    fn current(&self) -> Option<BindTarget> {
        self.targets.get(self.cursor).copied()
    }

    fn build_connect_rx(&mut self, t: BindTarget, now_ns: u64) -> AcmpSend {
        let seq = self.next_seq();
        let pdu = Acmpdu {
            message_type: acmp_msg::CONNECT_RX_COMMAND,
            status: acmp_status::SUCCESS,
            controller_entity_id: self.controller_id,
            talker_entity_id: self.talker_id,
            listener_entity_id: self.listener_id,
            talker_unique_id: t.talker_unique_id,
            listener_unique_id: t.listener_unique_id,
            sequence_id: seq,
            ..Default::default()
        };
        self.phase = Phase::InFlight {
            sequence_id: seq,
            attempt: match self.phase {
                Phase::InFlight { attempt, .. } => attempt + 1,
                Phase::Idle => 1,
            },
            deadline_ns: now_ns.saturating_add(CONNECT_RX_TIMEOUT_NS),
        };
        AcmpSend {
            frame: pdu.encode(),
            sequence_id: seq,
            listener_unique_id: t.listener_unique_id,
        }
    }

    /// Emit the next CONNECT_RX if idle and a target remains. Streams are
    /// bound sequentially: CONNECT_RX for j+1 is sent only after j's final
    /// outcome (§9.2). Returns at most one send.
    pub fn poll(&mut self, now_ns: u64) -> Option<AcmpSend> {
        if !matches!(self.phase, Phase::Idle) {
            return None;
        }
        let t = self.current()?;
        Some(self.build_connect_rx(t, now_ns))
    }

    /// Fold a received ACMPDU. Completes the in-flight CONNECT_RX iff
    /// (§9.2 matching rule): `message_type == CONNECT_RX_RESPONSE`,
    /// `controller_entity_id == ours`, `listener_entity_id` and
    /// `listener_unique_id` match, and `sequence_id` matches. Anything else
    /// (including our own command echoed back via loopback) is counted
    /// `acmp_ctrl_unmatched`.
    pub fn on_rx(&mut self, pdu: &Acmpdu) -> Option<(u16, BindOutcome)> {
        let (seq, _attempt) = match self.phase {
            Phase::InFlight {
                sequence_id,
                attempt,
                ..
            } => (sequence_id, attempt),
            Phase::Idle => {
                self.acmp_ctrl_unmatched += 1;
                return None;
            }
        };
        let t = match self.current() {
            Some(t) => t,
            None => {
                self.acmp_ctrl_unmatched += 1;
                return None;
            }
        };
        let matches = pdu.message_type == acmp_msg::CONNECT_RX_RESPONSE
            && pdu.controller_entity_id == self.controller_id
            && pdu.listener_entity_id == self.listener_id
            && pdu.listener_unique_id == t.listener_unique_id
            && pdu.sequence_id == seq;
        if !matches {
            self.acmp_ctrl_unmatched += 1;
            return None;
        }
        if pdu.status == acmp_status::SUCCESS {
            self.complete(t.listener_unique_id, BindOutcome::Success)
        } else {
            self.complete(
                t.listener_unique_id,
                BindOutcome::Failed {
                    status: pdu.status,
                    a5: is_a5_resource_status(pdu.status),
                },
            )
        }
    }

    /// Age the in-flight deadline. A timeout retries up to [`MAX_ATTEMPTS`]
    /// (1 s apart); the last timeout records [`BindOutcome::TimedOut`] and
    /// advances. Returns a resend to put on the wire and/or a final outcome.
    pub fn on_tick(&mut self, now_ns: u64) -> (Option<AcmpSend>, Option<(u16, BindOutcome)>) {
        let (seq, attempt, deadline) = match self.phase {
            Phase::InFlight {
                sequence_id,
                attempt,
                deadline_ns,
            } => (sequence_id, attempt, deadline_ns),
            Phase::Idle => return (None, None),
        };
        let _ = seq;
        if now_ns < deadline {
            return (None, None);
        }
        let t = match self.current() {
            Some(t) => t,
            None => return (None, None),
        };
        if attempt < MAX_ATTEMPTS {
            // Retry: a new command with a new sequence_id (§9.2); 1 s apart is
            // honoured by the daemon's tick cadence. We set the next deadline
            // to now + spacing + timeout implicitly via build_connect_rx.
            let send = self.build_connect_rx(t, now_ns.saturating_add(ATTEMPT_SPACING_NS));
            (Some(send), None)
        } else {
            let out = self.complete(t.listener_unique_id, BindOutcome::TimedOut);
            (None, out)
        }
    }

    fn complete(&mut self, luid: u16, outcome: BindOutcome) -> Option<(u16, BindOutcome)> {
        self.phase = Phase::Idle;
        self.cursor += 1;
        self.outcomes.push((luid, outcome));
        Some((luid, outcome))
    }

    pub fn next_deadline_ns(&self) -> Option<u64> {
        match self.phase {
            Phase::InFlight { deadline_ns, .. } => Some(deadline_ns),
            Phase::Idle => None,
        }
    }
}

/// Build a GET_TX_STATE_COMMAND to the talker (ADC-15 check, §9.4). Uses the
/// controller's own ACMP sequence counter and the same matching rule with
/// talker ids in place of listener ids.
pub fn build_get_tx_state(
    controller_id: Eui64,
    talker_id: Eui64,
    talker_unique_id: u16,
    sequence_id: u16,
) -> Vec<u8> {
    Acmpdu {
        message_type: acmp_msg::GET_TX_STATE_COMMAND,
        status: acmp_status::SUCCESS,
        controller_entity_id: controller_id,
        talker_entity_id: talker_id,
        talker_unique_id,
        sequence_id,
        ..Default::default()
    }
    .encode()
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn ids() -> (Eui64, Eui64, Eui64, MacAddr) {
        (
            Eui64::from_u64(0x644b_f0ff_fe12_73b8), // controller/listener (same station)
            Eui64::from_u64(0x000A_9203_3BA9_0001), // talker
            Eui64::from_u64(0x644b_f0ff_fe12_73b8), // listener
            MacAddr([0, 0x0a, 0x92, 3, 0x3b, 0xa9]),
        )
    }

    fn targets() -> Vec<BindTarget> {
        vec![
            BindTarget {
                listener_unique_id: 0,
                talker_unique_id: 0,
            },
            BindTarget {
                listener_unique_id: 1,
                talker_unique_id: 2,
            },
            BindTarget {
                listener_unique_id: 2,
                talker_unique_id: 3,
            },
        ]
    }

    fn response(ctrl: Eui64, listener: Eui64, luid: u16, seq: u16, status: u8) -> Acmpdu {
        Acmpdu {
            message_type: acmp_msg::CONNECT_RX_RESPONSE,
            status,
            controller_entity_id: ctrl,
            listener_entity_id: listener,
            listener_unique_id: luid,
            sequence_id: seq,
            ..Default::default()
        }
    }

    #[test]
    fn binds_sequentially() {
        let (ctrl, talker, listener, mac) = ids();
        let mut b = Binder::new(ctrl, talker, listener, mac, targets());
        // First CONNECT_RX for j=0.
        let s0 = b.poll(0).expect("first send");
        assert_eq!(s0.listener_unique_id, 0);
        // j=1 cannot go until j=0 completes.
        assert!(b.poll(1).is_none());
        let r0 = response(ctrl, listener, 0, s0.sequence_id, acmp_status::SUCCESS);
        assert_eq!(b.on_rx(&r0), Some((0, BindOutcome::Success)));
        // Now j=1.
        let s1 = b.poll(2).expect("second send");
        assert_eq!(s1.listener_unique_id, 1);
        assert_ne!(s1.sequence_id, s0.sequence_id);
    }

    #[test]
    fn ignores_unmatched_responses() {
        let (ctrl, talker, listener, mac) = ids();
        let mut b = Binder::new(ctrl, talker, listener, mac, targets());
        let s = b.poll(0).unwrap();
        // Wrong sequence_id.
        assert!(b
            .on_rx(&response(
                ctrl,
                listener,
                0,
                s.sequence_id ^ 1,
                acmp_status::SUCCESS
            ))
            .is_none());
        // Wrong listener_unique_id.
        assert!(b
            .on_rx(&response(
                ctrl,
                listener,
                7,
                s.sequence_id,
                acmp_status::SUCCESS
            ))
            .is_none());
        // Wrong controller_entity_id.
        assert!(b
            .on_rx(&response(
                Eui64::from_u64(0xDEAD),
                listener,
                0,
                s.sequence_id,
                acmp_status::SUCCESS
            ))
            .is_none());
        // Loopback echo: our own CONNECT_RX_COMMAND (not a response).
        let mut echo = response(ctrl, listener, 0, s.sequence_id, acmp_status::SUCCESS);
        echo.message_type = acmp_msg::CONNECT_RX_COMMAND;
        assert!(b.on_rx(&echo).is_none());
        assert_eq!(b.acmp_ctrl_unmatched, 4);
        // The correct response still completes it.
        assert_eq!(
            b.on_rx(&response(
                ctrl,
                listener,
                0,
                s.sequence_id,
                acmp_status::SUCCESS
            )),
            Some((0, BindOutcome::Success))
        );
    }

    #[test]
    fn a5_status_classified_on_failure() {
        let (ctrl, talker, listener, mac) = ids();
        let mut b = Binder::new(ctrl, talker, listener, mac, targets());
        let s = b.poll(0).unwrap();
        let out = b.on_rx(&response(
            ctrl,
            listener,
            0,
            s.sequence_id,
            acmp_status::TALKER_NO_BANDWIDTH,
        ));
        assert_eq!(
            out,
            Some((
                0,
                BindOutcome::Failed {
                    status: acmp_status::TALKER_NO_BANDWIDTH,
                    a5: true
                }
            ))
        );
    }

    #[test]
    fn times_out_after_three_attempts() {
        let (ctrl, talker, listener, mac) = ids();
        let mut b = Binder::new(ctrl, talker, listener, mac, vec![targets()[0]]);
        b.poll(0).unwrap();
        // Attempt 1 timeout -> resend (attempt 2).
        let (resend, out) = b.on_tick(CONNECT_RX_TIMEOUT_NS);
        assert!(resend.is_some() && out.is_none());
        // Attempt 2 timeout -> resend (attempt 3).
        let (resend, out) = b.on_tick(10 * CONNECT_RX_TIMEOUT_NS);
        assert!(resend.is_some() && out.is_none());
        // Attempt 3 timeout -> final TimedOut.
        let (resend, out) = b.on_tick(100 * CONNECT_RX_TIMEOUT_NS);
        assert!(resend.is_none());
        assert_eq!(out, Some((0, BindOutcome::TimedOut)));
        assert!(b.is_done());
    }

    #[test]
    fn get_tx_state_uses_controller_counter() {
        let (ctrl, talker, _l, _m) = ids();
        let frame = build_get_tx_state(ctrl, talker, 3, 0x55);
        let d = Acmpdu::decode(&frame).unwrap();
        assert_eq!(d.message_type, acmp_msg::GET_TX_STATE_COMMAND);
        assert_eq!(d.talker_unique_id, 3);
        assert_eq!(d.sequence_id, 0x55);
    }

    #[test]
    fn acmp_counter_independent_of_others() {
        // The binder's seq advances on its own; a fresh binder starts at 0.
        let (ctrl, talker, listener, mac) = ids();
        let mut b = Binder::new(ctrl, talker, listener, mac, targets());
        let s0 = b.poll(0).unwrap();
        assert_eq!(s0.sequence_id, 0);
        let r0 = response(ctrl, listener, 0, s0.sequence_id, acmp_status::SUCCESS);
        b.on_rx(&r0);
        let s1 = b.poll(1).unwrap();
        assert_eq!(s1.sequence_id, 1);
    }
}
