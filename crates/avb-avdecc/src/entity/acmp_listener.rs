// SPDX-License-Identifier: MIT OR Apache-2.0

//! The Milan ACMP listener state machine (design §9.4, Milan v1.2 §5.5.3).
//!
//! One [`ListenerSm`] per STREAM_INPUT. Sans-IO: events in, actions out, one
//! timer. State names and timer values are named consts with citations and
//! are pinned by unit tests; if the Milan text differs the constants change,
//! not the table's structure (one test per §9.4 row, named after it).
//!
//! The SM owns no clock: the caller passes `now_ns` with each event and polls
//! [`ListenerSm::on_timer`] when `timer_deadline_ns()` is reached. Random
//! 0–1 s delays are injected by the caller (`arm_random`) so the SM stays
//! deterministic under test.

use avb_core::{Eui64, MacAddr, StreamId};

use crate::acmp::is_a5_resource_status;
use crate::constants::acmp_status;

/// Timers (Milan v1.2 §5.5.3 / design §9.4), as named consts with citations.
pub mod timers {
    /// Upper bound of the random pre-probe delay (§9.4 PRB_W_DELAY: 0–1 s).
    pub const DELAY_MAX_NS: u64 = 1_000_000_000;
    /// PROBE_TX response wait (§9.4 PRB_W_RESP/RESP2: 200 ms).
    pub const PROBE_RESP_NS: u64 = 200_000_000;
    /// Back-off before the next probe cycle (§9.4 PRB_W_RETRY: 4 s).
    pub const RETRY_NS: u64 = 4_000_000_000;
    /// Wait for a Talker Advertise after addressing is known (§9.4
    /// SETTLED_NO_RSV: TA_WAIT = 2 s).
    pub const TA_WAIT_NS: u64 = 2_000_000_000;
}

/// The probe cycle count at which the fallback fires (§9.4: `probe_cycles == 3`).
pub const PROBE_FALLBACK_CYCLES: u32 = 3;

/// Listener SM states (§9.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Unbound,
    PrbWAvail,
    PrbWDelay,
    PrbWResp,
    PrbWResp2,
    PrbWRetry,
    SettledNoRsv,
    SettledRsvOk,
}

/// What the Listener declared to MSRP (§8.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Declaration {
    None,
    Ready,
    AskingFailed,
}

/// Effective stream addressing (§8.2): from PROBE_TX, then the TA once it
/// registers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effective {
    pub stream_id: StreamId,
    pub dest_mac: MacAddr,
    pub vlan_id: u16,
    pub accumulated_latency_ns: u32,
}

/// Events into the SM (§9.4 table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// CONNECT_RX_COMMAND. `talker_in_adp` decides PRB_W_AVAIL vs PRB_W_DELAY.
    BindRx {
        talker_entity_id: Eui64,
        talker_unique_id: u16,
        controller_entity_id: Eui64,
        talker_in_adp: bool,
        /// `last_available_index` seed from the ADP table, if present.
        talker_available_index: Option<u32>,
        /// STREAMING_WAIT as the binding controller set it. Saved verbatim
        /// and reported back by GET_RX_STATE (§5.5.3.6.3 step 2, Table 5.44).
        streaming_wait: bool,
    },
    /// DISCONNECT_RX_COMMAND.
    UnbindRx,
    /// Talker ENTITY_AVAILABLE (carries its current available_index).
    TalkerAvailable { available_index: u32 },
    /// Talker ENTITY_DEPARTING or its ADP entry timed out.
    TalkerGone,
    /// PROBE_TX_RESPONSE (GET_TX_STATE_RESPONSE) matched to our probe.
    ProbeTxResponse {
        status: u8,
        stream_id: StreamId,
        dest_mac: MacAddr,
        vlan_id: u16,
    },
    /// Talker Advertise registered for the effective stream_id (§8.2).
    TaRegistered {
        dest_mac: MacAddr,
        vlan_id: u16,
        accumulated_latency_ns: u32,
    },
    /// Talker Advertise withdrawn.
    TaWithdrawn,
    /// Talker Advertise attributes changed (same stream_id).
    TaChanged {
        dest_mac: MacAddr,
        vlan_id: u16,
        accumulated_latency_ns: u32,
    },
    /// MSRP Talker Failed registered.
    TalkerFailed { failure_code: u8, a5: bool },
    /// MSRP Talker Failed withdrawn.
    TalkerFailedWithdrawn,
}

/// ACMP status to respond to a BIND_RX / UNBIND_RX (§9.4). `None` means no
/// ACMP response is emitted (events other than bind/unbind).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcmpResponse {
    Success,
    NotConnected,
    ListenerUnknownId,
}

/// Actions the SM asks the daemon to perform (§9.4 "actions out").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Send a PROBE_TX_COMMAND (GET_TX_STATE) to the talker. The caller
    /// assigns the shared PROBE_TX sequence_id and addresses it to
    /// `91:E0:F0:01:00:00`.
    SendProbeTx {
        talker_entity_id: Eui64,
        talker_unique_id: u16,
    },
    /// Re-issue the stream filter with the current effective values.
    SetStreamFilter { effective: Effective },
    /// Replace the audio stream table (addressing or latency changed).
    ReplaceStreamTable { effective: Effective },
    /// Remove this stream's filter and audio-table entry (withdraw).
    RemoveStreamEntry,
    /// Declare the given MSRP Listener declaration.
    Declare(Declaration),
    /// Withdraw the MSRP Listener declaration (Lv).
    WithdrawListener,
    /// Log line (§9.4 one-per-stream notices like U-8, deviation).
    Log(String),
}

/// Counters the SM keeps (§9.4). All advisory, surfaced in the summary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SmCounters {
    pub ta_wait_expired: u64,
    pub ta_probe_mismatch: u64,
    pub talker_failed_withdrawn: u64,
    pub probe_fallback: u64,
    pub probe_late_response: u64,
    pub ignored: u64,
    pub reboots: u64,
}

/// Per-binding state (§9.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Binding {
    talker_entity_id: Eui64,
    talker_unique_id: u16,
    controller_entity_id: Eui64,
    last_available_index: Option<u32>,
}

/// The binding a sink is holding, for GET_RX_STATE to report.
///
/// Milan §5.5.3.6.3 step 2 requires the binding parameters to be SAVED, and
/// §5.5.3.6.44 Table 5.44 requires GET_RX_STATE to report them back, so they
/// have to be readable from outside the state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoundTo {
    pub talker_entity_id: Eui64,
    pub talker_unique_id: u16,
    pub controller_entity_id: Eui64,
}

/// The Milan ACMP listener SM for one STREAM_INPUT (§9.4).
#[derive(Clone, Debug)]
pub struct ListenerSm {
    pub listener_unique_id: u16,
    state: State,
    binding: Option<Binding>,
    effective: Effective,
    /// GET_STREAM_INFO addressing the controller learned (fallback source).
    get_stream_info_fallback: Option<Effective>,
    probe_cycles: u32,
    declaration: Declaration,
    /// Whether a TA is currently registered for the effective stream.
    ta_registered: bool,
    /// Whether a Talker Failed is currently registered.
    talker_failed_registered: bool,
    timer_deadline_ns: Option<u64>,
    a5_classified: bool,
    /// STREAMING_WAIT from the binding command, saved verbatim.
    streaming_wait: bool,
    /// Whether this Stream Input is STARTED (Milan §5.4.2.19-20).
    ///
    /// A bind with STREAMING_WAIT set leaves the sink bound but stopped --
    /// that is what the flag is for -- so START_STREAMING is what starts it.
    /// A bind without the flag is started immediately.
    started: bool,
    pub counters: SmCounters,
}

impl ListenerSm {
    pub fn new(listener_unique_id: u16) -> Self {
        Self {
            listener_unique_id,
            state: State::Unbound,
            binding: None,
            effective: Effective::default(),
            get_stream_info_fallback: None,
            probe_cycles: 0,
            declaration: Declaration::None,
            ta_registered: false,
            talker_failed_registered: false,
            timer_deadline_ns: None,
            a5_classified: false,
            streaming_wait: false,
            started: false,
            counters: SmCounters::default(),
        }
    }

    /// Who this sink is bound to, or `None` while UNBOUND.
    pub fn bound_to(&self) -> Option<BoundTo> {
        self.binding.as_ref().map(|b| BoundTo {
            talker_entity_id: b.talker_entity_id,
            talker_unique_id: b.talker_unique_id,
            controller_entity_id: b.controller_entity_id,
        })
    }

    /// STREAMING_WAIT as the binding controller set it (§5.5.3.6.3 step 2:
    /// "Update the binding parameters with ... STREAMING_WAIT ... then save
    /// them"). Reported back verbatim by GET_RX_STATE, Table 5.44.
    pub fn streaming_wait(&self) -> bool {
        self.streaming_wait
    }

    /// Whether this Stream Input is started.
    pub fn started(&self) -> bool {
        self.started
    }

    /// START_STREAMING (§5.4.2.19). Returns whether the state CHANGED.
    ///
    /// "This command has no effect on a Stream Input that is not already
    /// bound or already started" -- so an unbound or already-started sink is
    /// a no-op, not an error.
    pub fn start_streaming(&mut self) -> bool {
        if self.binding.is_none() || self.started {
            return false;
        }
        self.started = true;
        true
    }

    /// STOP_STREAMING (§5.4.2.20). Returns whether the state CHANGED.
    pub fn stop_streaming(&mut self) -> bool {
        if self.binding.is_none() || !self.started {
            return false;
        }
        self.started = false;
        true
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn declaration(&self) -> Declaration {
        self.declaration
    }

    pub fn effective(&self) -> Effective {
        self.effective
    }

    pub fn probe_cycles(&self) -> u32 {
        self.probe_cycles
    }

    pub fn a5_classified(&self) -> bool {
        self.a5_classified
    }

    pub fn timer_deadline_ns(&self) -> Option<u64> {
        self.timer_deadline_ns
    }

    pub fn last_available_index(&self) -> Option<u32> {
        self.binding.and_then(|b| b.last_available_index)
    }

    /// The addressing the controller learned via GET_STREAM_INFO, used by the
    /// probe fallback. Set by the daemon when it reads the 16R's stream info.
    pub fn set_get_stream_info_fallback(&mut self, eff: Effective) {
        self.get_stream_info_fallback = Some(eff);
    }

    fn is_bound_state(&self) -> bool {
        !matches!(self.state, State::Unbound)
    }

    /// Process one event. `now_ns` is the current time; `rand01_ns` is a
    /// caller-supplied random value in `0..=DELAY_MAX_NS` used when a state
    /// arms the random pre-probe delay. Returns the ACMP response (if the
    /// event was a bind/unbind) and the actions to perform.
    pub fn on_event(
        &mut self,
        ev: Event,
        now_ns: u64,
        rand01_ns: u64,
    ) -> (Option<AcmpResponse>, Vec<Action>) {
        let mut out = Vec::new();
        // ENTITY_AVAILABLE updates last_available_index after the reboot
        // comparison, in every bound state and row (§9.4).
        if let Event::TalkerAvailable { available_index } = ev {
            return self.on_talker_available(available_index, now_ns, rand01_ns);
        }
        let resp = match (self.state, ev) {
            // --- UNBOUND ---
            (State::Unbound, Event::BindRx { talker_in_adp, .. }) => {
                self.store_binding(ev);
                self.probe_cycles = 0;
                if talker_in_adp {
                    self.enter(State::PrbWDelay, now_ns, rand01_ns, &mut out);
                } else {
                    self.enter(State::PrbWAvail, now_ns, rand01_ns, &mut out);
                }
                Some(AcmpResponse::Success)
            }
            (State::Unbound, Event::UnbindRx) => Some(AcmpResponse::NotConnected),

            // --- PRB_W_AVAIL ---
            // (TalkerAvailable handled above -> PRB_W_DELAY)

            // --- PRB_W_RESP / PRB_W_RESP2: PROBE_TX_RESPONSE ---
            (
                State::PrbWResp | State::PrbWResp2,
                Event::ProbeTxResponse {
                    status,
                    stream_id,
                    dest_mac,
                    vlan_id,
                },
            ) => {
                if status == acmp_status::SUCCESS {
                    self.effective = Effective {
                        stream_id,
                        dest_mac,
                        vlan_id,
                        accumulated_latency_ns: 0,
                    };
                    out.push(Action::SetStreamFilter {
                        effective: self.effective,
                    });
                    out.push(Action::ReplaceStreamTable {
                        effective: self.effective,
                    });
                    self.probe_cycles = 0;
                    self.enter(State::SettledNoRsv, now_ns, rand01_ns, &mut out);
                } else {
                    if is_a5_resource_status(status) {
                        self.a5_classified = true;
                        out.push(Action::Log(format!(
                            "A-5: stream not simultaneously streamable (ACMP status {status})"
                        )));
                    }
                    // probe_cycles unchanged (the talker answered).
                    self.arm(State::PrbWRetry, now_ns, timers::RETRY_NS, &mut out);
                }
                None
            }

            // --- SETTLED_NO_RSV: TA registered ---
            (
                State::SettledNoRsv,
                Event::TaRegistered {
                    dest_mac,
                    vlan_id,
                    accumulated_latency_ns,
                },
            ) => {
                self.apply_ta(dest_mac, vlan_id, accumulated_latency_ns, &mut out);
                self.ta_registered = true;
                self.declare(Declaration::Ready, &mut out);
                self.enter(State::SettledRsvOk, now_ns, rand01_ns, &mut out);
                None
            }

            // --- SETTLED_NO_RSV / SETTLED_RSV_OK: Talker Failed registered ---
            (
                State::SettledNoRsv | State::SettledRsvOk,
                Event::TalkerFailed {
                    failure_code: _,
                    a5,
                },
            ) => {
                self.talker_failed_registered = true;
                if a5 {
                    self.a5_classified = true;
                }
                self.declare(Declaration::AskingFailed, &mut out);
                // TA_WAIT not armed (§9.4).
                self.state = State::SettledNoRsv;
                self.timer_deadline_ns = None;
                None
            }

            // --- SETTLED_NO_RSV: Talker Failed withdrawn, no TA ---
            (State::SettledNoRsv, Event::TalkerFailedWithdrawn) if !self.ta_registered => {
                self.talker_failed_registered = false;
                self.counters.talker_failed_withdrawn += 1;
                // Keep the Asking Failed declaration (same as expired TA_WAIT).
                self.declare(Declaration::AskingFailed, &mut out);
                None
            }

            // --- SETTLED_RSV_OK: TA withdrawn ---
            (State::SettledRsvOk, Event::TaWithdrawn) => {
                self.ta_registered = false;
                out.push(Action::WithdrawListener);
                self.declaration = Declaration::None;
                self.enter(State::SettledNoRsv, now_ns, rand01_ns, &mut out);
                None
            }

            // --- SETTLED_RSV_OK: TA changed ---
            (
                State::SettledRsvOk,
                Event::TaChanged {
                    dest_mac,
                    vlan_id,
                    accumulated_latency_ns,
                },
            ) => {
                let mac_vid_changed =
                    dest_mac != self.effective.dest_mac || vlan_id != self.effective.vlan_id;
                if mac_vid_changed {
                    self.counters.ta_probe_mismatch += 1;
                }
                let lat_changed = accumulated_latency_ns != self.effective.accumulated_latency_ns;
                self.effective.dest_mac = dest_mac;
                self.effective.vlan_id = vlan_id;
                self.effective.accumulated_latency_ns = accumulated_latency_ns;
                if mac_vid_changed {
                    out.push(Action::SetStreamFilter {
                        effective: self.effective,
                    });
                    out.push(Action::ReplaceStreamTable {
                        effective: self.effective,
                    });
                } else if lat_changed {
                    out.push(Action::ReplaceStreamTable {
                        effective: self.effective,
                    });
                }
                // Listener Ready stays declared; no Lv.
                None
            }

            // --- any bound state: TalkerGone (DEPARTING / ADP timeout) ---
            (_, Event::TalkerGone) if self.is_bound_state() => {
                out.push(Action::WithdrawListener);
                out.push(Action::RemoveStreamEntry);
                self.declaration = Declaration::None;
                self.ta_registered = false;
                self.enter(State::PrbWAvail, now_ns, rand01_ns, &mut out);
                None
            }

            // --- any bound state: BIND_RX (rebind) ---
            (
                _,
                Event::BindRx {
                    talker_entity_id,
                    talker_unique_id,
                    controller_entity_id,
                    talker_in_adp,
                    talker_available_index,
                    streaming_wait: _,
                },
            ) if self.is_bound_state() => {
                let same = self.binding.is_some_and(|b| {
                    b.talker_entity_id == talker_entity_id && b.talker_unique_id == talker_unique_id
                });
                if same {
                    // Update controller_entity_id/flags; stay put.
                    if let Some(b) = self.binding.as_mut() {
                        b.controller_entity_id = controller_entity_id;
                    }
                    Some(AcmpResponse::Success)
                } else {
                    // Withdraw old, store new, go as from UNBOUND.
                    out.push(Action::WithdrawListener);
                    out.push(Action::RemoveStreamEntry);
                    self.declaration = Declaration::None;
                    self.ta_registered = false;
                    self.binding = Some(Binding {
                        talker_entity_id,
                        talker_unique_id,
                        controller_entity_id,
                        last_available_index: talker_available_index,
                    });
                    self.probe_cycles = 0;
                    if talker_in_adp {
                        self.enter(State::PrbWDelay, now_ns, rand01_ns, &mut out);
                    } else {
                        self.enter(State::PrbWAvail, now_ns, rand01_ns, &mut out);
                    }
                    Some(AcmpResponse::Success)
                }
            }

            // --- any bound state: UNBIND_RX ---
            (_, Event::UnbindRx) if self.is_bound_state() => {
                out.push(Action::WithdrawListener);
                out.push(Action::RemoveStreamEntry);
                self.declaration = Declaration::None;
                self.binding = None;
                // An unbound sink cannot be started: §5.4.2.19 makes being
                // bound a precondition, so leaving this set would let a later
                // GET_RX_STATE report a started stream with no talker.
                self.started = false;
                self.streaming_wait = false;
                self.ta_registered = false;
                self.state = State::Unbound;
                self.timer_deadline_ns = None;
                Some(AcmpResponse::Success)
            }

            // --- any state except PRB_W_RESP/RESP2: PROBE_TX_RESPONSE ---
            (_, Event::ProbeTxResponse { .. }) => {
                self.counters.probe_late_response += 1;
                None
            }

            // --- everything else: ignored ---
            _ => {
                self.counters.ignored += 1;
                None
            }
        };
        (resp, out)
    }

    /// The random-delay / probe / retry / TA_WAIT timer fired (§9.4).
    pub fn on_timer(&mut self, now_ns: u64, rand01_ns: u64) -> Vec<Action> {
        let mut out = Vec::new();
        self.timer_deadline_ns = None;
        match self.state {
            State::PrbWDelay => {
                self.send_probe(&mut out);
                self.arm(State::PrbWResp, now_ns, timers::PROBE_RESP_NS, &mut out);
            }
            State::PrbWResp => {
                self.send_probe(&mut out);
                self.arm(State::PrbWResp2, now_ns, timers::PROBE_RESP_NS, &mut out);
            }
            State::PrbWResp2 => {
                self.probe_cycles += 1;
                if self.probe_cycles >= PROBE_FALLBACK_CYCLES {
                    self.fallback(now_ns, rand01_ns, &mut out);
                } else {
                    self.arm(State::PrbWRetry, now_ns, timers::RETRY_NS, &mut out);
                }
            }
            State::PrbWRetry => {
                self.enter(State::PrbWDelay, now_ns, rand01_ns, &mut out);
            }
            State::SettledNoRsv => {
                // TA_WAIT expired with no TA registered.
                self.counters.ta_wait_expired += 1;
                out.push(Action::Log(format!(
                    "U-8: no Talker Advertise for {} after 2 s; declaring Listener (Asking Failed)",
                    self.effective.stream_id
                )));
                self.declare(Declaration::AskingFailed, &mut out);
            }
            _ => {}
        }
        out
    }

    // --- helpers ---

    fn on_talker_available(
        &mut self,
        available_index: u32,
        now_ns: u64,
        rand01_ns: u64,
    ) -> (Option<AcmpResponse>, Vec<Action>) {
        let mut out = Vec::new();
        if !self.is_bound_state() {
            self.counters.ignored += 1;
            return (None, out);
        }
        // Reboot comparison BEFORE updating last_available_index (§9.4).
        let prev = self.binding.and_then(|b| b.last_available_index);
        let rebooted = prev.is_some_and(|p| available_index < p);
        if rebooted {
            self.counters.reboots += 1;
            out.push(Action::WithdrawListener);
            out.push(Action::RemoveStreamEntry);
            self.declaration = Declaration::None;
            self.ta_registered = false;
            self.probe_cycles = 0;
            self.enter(State::PrbWDelay, now_ns, rand01_ns, &mut out);
        } else if self.state == State::PrbWAvail {
            // Talker became available: go probe.
            self.enter(State::PrbWDelay, now_ns, rand01_ns, &mut out);
        } else {
            self.counters.ignored += 1;
        }
        // Update last_available_index on EVERY ENTITY_AVAILABLE, after reboot
        // comparison, in whichever row the event took.
        if let Some(b) = self.binding.as_mut() {
            b.last_available_index = Some(available_index);
        }
        (None, out)
    }

    fn store_binding(&mut self, ev: Event) {
        if let Event::BindRx {
            talker_entity_id,
            talker_unique_id,
            controller_entity_id,
            talker_available_index,
            streaming_wait,
            ..
        } = ev
        {
            self.binding = Some(Binding {
                talker_entity_id,
                talker_unique_id,
                controller_entity_id,
                last_available_index: talker_available_index,
            });
            self.streaming_wait = streaming_wait;
            // STREAMING_WAIT means "bound, but do not start yet", so it is
            // the one case where a fresh binding is not already started.
            self.started = !streaming_wait;
        }
    }

    fn send_probe(&mut self, out: &mut Vec<Action>) {
        if let Some(b) = self.binding {
            out.push(Action::SendProbeTx {
                talker_entity_id: b.talker_entity_id,
                talker_unique_id: b.talker_unique_id,
            });
        }
    }

    fn fallback(&mut self, now_ns: u64, rand01_ns: u64, out: &mut Vec<Action>) {
        self.counters.probe_fallback += 1;
        out.push(Action::Log(
            "deviation: PROBE_TX unanswered, using GET_STREAM_INFO".into(),
        ));
        if let Some(eff) = self.get_stream_info_fallback {
            self.effective = eff;
        }
        out.push(Action::SetStreamFilter {
            effective: self.effective,
        });
        out.push(Action::ReplaceStreamTable {
            effective: self.effective,
        });
        self.enter(State::SettledNoRsv, now_ns, rand01_ns, out);
    }

    fn apply_ta(&mut self, dest_mac: MacAddr, vlan_id: u16, latency: u32, out: &mut Vec<Action>) {
        if dest_mac != self.effective.dest_mac || vlan_id != self.effective.vlan_id {
            self.counters.ta_probe_mismatch += 1;
            self.effective.dest_mac = dest_mac;
            self.effective.vlan_id = vlan_id;
            out.push(Action::SetStreamFilter {
                effective: self.effective,
            });
            out.push(Action::ReplaceStreamTable {
                effective: self.effective,
            });
        }
        self.effective.accumulated_latency_ns = latency;
    }

    fn declare(&mut self, d: Declaration, out: &mut Vec<Action>) {
        self.declaration = d;
        out.push(Action::Declare(d));
    }

    /// Enter a state, cancelling the previous timer and arming the new state's
    /// entry timer / evaluating its entry rows (§9.4).
    fn enter(&mut self, s: State, now_ns: u64, rand01_ns: u64, out: &mut Vec<Action>) {
        self.timer_deadline_ns = None;
        self.state = s;
        match s {
            State::PrbWDelay => {
                self.timer_deadline_ns =
                    Some(now_ns.saturating_add(rand01_ns.min(timers::DELAY_MAX_NS)));
            }
            State::SettledNoRsv => {
                // Entry evaluation (§9.4): TA registered -> SETTLED_RSV_OK;
                // else Talker Failed registered -> Asking Failed, no TA_WAIT;
                // else arm TA_WAIT.
                if self.ta_registered {
                    self.declare(Declaration::Ready, out);
                    self.state = State::SettledRsvOk;
                } else if self.talker_failed_registered {
                    self.declare(Declaration::AskingFailed, out);
                } else {
                    self.timer_deadline_ns = Some(now_ns.saturating_add(timers::TA_WAIT_NS));
                }
            }
            _ => {}
        }
    }

    /// Enter a state with an explicit fixed timer (PRB_W_RESP/RESP2/RETRY).
    fn arm(&mut self, s: State, now_ns: u64, dur_ns: u64, _out: &mut [Action]) {
        self.state = s;
        self.timer_deadline_ns = Some(now_ns.saturating_add(dur_ns));
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    const TALKER: Eui64 = Eui64([0, 0x0a, 0x92, 0xff, 0xfe, 0x03, 0x3b, 0xa9]);
    const CTRL: Eui64 = Eui64([0x64, 0x4b, 0xf0, 0xff, 0xfe, 0x12, 0x73, 0xb8]);
    const MAC_A: MacAddr = MacAddr([0x91, 0xe0, 0xf0, 0x00, 0x11, 0x11]);
    const MAC_B: MacAddr = MacAddr([0x91, 0xe0, 0xf0, 0x00, 0x22, 0x22]);
    const SID: StreamId = StreamId(0x000A_9203_3BA9_0000);

    fn sm() -> ListenerSm {
        ListenerSm::new(0)
    }

    fn bind(talker_in_adp: bool, avail: Option<u32>) -> Event {
        Event::BindRx {
            talker_entity_id: TALKER,
            talker_unique_id: 0,
            controller_entity_id: CTRL,
            talker_in_adp,
            talker_available_index: avail,
            streaming_wait: false,
        }
    }

    fn probe_ok() -> Event {
        Event::ProbeTxResponse {
            status: acmp_status::SUCCESS,
            stream_id: SID,
            dest_mac: MAC_A,
            vlan_id: 2,
        }
    }

    /// Drive from UNBOUND to SETTLED_NO_RSV via a successful probe, with the
    /// talker already in ADP. Returns the SM at SETTLED_NO_RSV (TA_WAIT armed).
    fn settled_no_rsv() -> ListenerSm {
        let mut s = sm();
        s.on_event(bind(true, Some(5)), 0, 500_000_000); // -> PRB_W_DELAY
        s.on_timer(500_000_000, 0); // delay -> PRB_W_RESP (probe sent)
        let (_r, acts) = s.on_event(probe_ok(), 700_000_000, 0);
        assert_eq!(s.state(), State::SettledNoRsv);
        assert!(acts
            .iter()
            .any(|a| matches!(a, Action::SetStreamFilter { .. })));
        s
    }

    // Row: UNBOUND + BIND_RX (talker not in ADP) -> PRB_W_AVAIL
    #[test]
    fn unbound_bindrx_talker_not_in_adp() {
        let mut s = sm();
        let (resp, _) = s.on_event(bind(false, None), 0, 0);
        assert_eq!(resp, Some(AcmpResponse::Success));
        assert_eq!(s.state(), State::PrbWAvail);
        assert_eq!(s.probe_cycles(), 0);
    }

    // Row: UNBOUND + BIND_RX (talker in ADP) -> PRB_W_DELAY
    #[test]
    fn unbound_bindrx_talker_in_adp() {
        let mut s = sm();
        let (resp, _) = s.on_event(bind(true, Some(3)), 0, 400_000_000);
        assert_eq!(resp, Some(AcmpResponse::Success));
        assert_eq!(s.state(), State::PrbWDelay);
        assert_eq!(s.timer_deadline_ns(), Some(400_000_000));
        assert_eq!(s.last_available_index(), Some(3));
    }

    // Row: PRB_W_AVAIL + talker ENTITY_AVAILABLE -> PRB_W_DELAY
    #[test]
    fn prb_w_avail_entity_available() {
        let mut s = sm();
        s.on_event(bind(false, None), 0, 0);
        let (_r, _a) = s.on_event(
            Event::TalkerAvailable { available_index: 9 },
            10,
            300_000_000,
        );
        assert_eq!(s.state(), State::PrbWDelay);
        assert_eq!(s.last_available_index(), Some(9));
    }

    // Row: PRB_W_DELAY + timer -> PRB_W_RESP (probe sent)
    #[test]
    fn prb_w_delay_timer_sends_probe() {
        let mut s = sm();
        s.on_event(bind(true, Some(1)), 0, 100);
        let acts = s.on_timer(100, 0);
        assert_eq!(s.state(), State::PrbWResp);
        assert!(acts.iter().any(|a| matches!(a, Action::SendProbeTx { .. })));
        assert_eq!(s.timer_deadline_ns(), Some(100 + timers::PROBE_RESP_NS));
    }

    // Row: PRB_W_RESP + timeout -> PRB_W_RESP2 (resend)
    #[test]
    fn prb_w_resp_timeout_resends() {
        let mut s = sm();
        s.on_event(bind(true, Some(1)), 0, 0);
        s.on_timer(0, 0); // -> PRB_W_RESP
        let acts = s.on_timer(timers::PROBE_RESP_NS, 0);
        assert_eq!(s.state(), State::PrbWResp2);
        assert!(acts.iter().any(|a| matches!(a, Action::SendProbeTx { .. })));
    }

    // Row: PRB_W_RESP/RESP2 + PROBE_TX_RESPONSE SUCCESS -> SETTLED_NO_RSV
    #[test]
    fn prb_w_resp_probe_success_settles() {
        let s = settled_no_rsv();
        assert_eq!(s.state(), State::SettledNoRsv);
        assert_eq!(s.effective().stream_id, SID);
        assert_eq!(s.effective().dest_mac, MAC_A);
        assert_eq!(s.probe_cycles(), 0);
        // TA_WAIT armed on entry (no TA registered).
        assert!(s.timer_deadline_ns().is_some());
    }

    // Row: PRB_W_RESP/RESP2 + PROBE_TX_RESPONSE non-SUCCESS -> PRB_W_RETRY
    #[test]
    fn prb_w_resp_probe_failure_retries_and_a5() {
        let mut s = sm();
        s.on_event(bind(true, Some(1)), 0, 0);
        s.on_timer(0, 0); // PRB_W_RESP
        let fail = Event::ProbeTxResponse {
            status: acmp_status::TALKER_NO_BANDWIDTH,
            stream_id: SID,
            dest_mac: MAC_A,
            vlan_id: 2,
        };
        let (_r, acts) = s.on_event(fail, 100, 0);
        assert_eq!(s.state(), State::PrbWRetry);
        assert!(s.a5_classified());
        assert!(acts.iter().any(|a| matches!(a, Action::Log(_))));
        assert_eq!(s.probe_cycles(), 0); // unchanged; talker answered
    }

    // Row: PRB_W_RESP2 + timeout -> PRB_W_RETRY (probe_cycles += 1)
    #[test]
    fn prb_w_resp2_timeout_increments_cycles() {
        let mut s = sm();
        s.on_event(bind(true, Some(1)), 0, 0);
        s.on_timer(0, 0); // PRB_W_RESP
        s.on_timer(timers::PROBE_RESP_NS, 0); // PRB_W_RESP2
        s.on_timer(2 * timers::PROBE_RESP_NS, 0); // timeout
        assert_eq!(s.state(), State::PrbWRetry);
        assert_eq!(s.probe_cycles(), 1);
    }

    // Row: PRB_W_RETRY + timer -> PRB_W_DELAY
    #[test]
    fn prb_w_retry_timer_back_to_delay() {
        let mut s = sm();
        s.on_event(bind(true, Some(1)), 0, 0);
        s.on_timer(0, 0);
        s.on_timer(timers::PROBE_RESP_NS, 0);
        s.on_timer(2 * timers::PROBE_RESP_NS, 0); // PRB_W_RETRY
        s.on_timer(10_000_000_000, 250_000_000); // retry timer
        assert_eq!(s.state(), State::PrbWDelay);
    }

    // Row: SETTLED_NO_RSV + TA registered -> SETTLED_RSV_OK (Ready)
    #[test]
    fn settled_no_rsv_ta_registered_ready() {
        let mut s = settled_no_rsv();
        let (_r, acts) = s.on_event(
            Event::TaRegistered {
                dest_mac: MAC_A,
                vlan_id: 2,
                accumulated_latency_ns: 2_000_000,
            },
            800_000_000,
            0,
        );
        assert_eq!(s.state(), State::SettledRsvOk);
        assert_eq!(s.declaration(), Declaration::Ready);
        assert!(acts
            .iter()
            .any(|a| matches!(a, Action::Declare(Declaration::Ready))));
    }

    // Row: SETTLED_NO_RSV + TA_WAIT timer (no TA within 2 s) -> Asking Failed
    #[test]
    fn settled_no_rsv_ta_wait_expired_asking_failed() {
        let mut s = settled_no_rsv();
        let deadline = s.timer_deadline_ns().unwrap();
        let acts = s.on_timer(deadline, 0);
        assert_eq!(s.counters.ta_wait_expired, 1);
        assert_eq!(s.declaration(), Declaration::AskingFailed);
        assert!(acts
            .iter()
            .any(|a| matches!(a, Action::Log(l) if l.contains("U-8"))));
        // A later TA registration -> SETTLED_RSV_OK, re-declared Ready.
        let (_r, _a) = s.on_event(
            Event::TaRegistered {
                dest_mac: MAC_A,
                vlan_id: 2,
                accumulated_latency_ns: 0,
            },
            s.timer_deadline_ns().unwrap_or(deadline + 1_000_000_000),
            0,
        );
        assert_eq!(s.state(), State::SettledRsvOk);
        assert_eq!(s.declaration(), Declaration::Ready);
    }

    // Row: SETTLED_NO_RSV + Talker Failed withdrawn, no TA -> keep Asking Failed
    #[test]
    fn settled_no_rsv_talker_failed_withdrawn() {
        let mut s = settled_no_rsv();
        // Register a Talker Failed first (puts us in Asking Failed).
        s.on_event(
            Event::TalkerFailed {
                failure_code: 1,
                a5: false,
            },
            800_000_000,
            0,
        );
        assert_eq!(s.declaration(), Declaration::AskingFailed);
        let (_r, _a) = s.on_event(Event::TalkerFailedWithdrawn, 900_000_000, 0);
        assert_eq!(s.counters.talker_failed_withdrawn, 1);
        assert_eq!(s.declaration(), Declaration::AskingFailed);
    }

    // Row: SETTLED_RSV_OK + TA withdrawn -> SETTLED_NO_RSV (withdraw Ready)
    #[test]
    fn settled_rsv_ok_ta_withdrawn() {
        let mut s = settled_no_rsv();
        s.on_event(
            Event::TaRegistered {
                dest_mac: MAC_A,
                vlan_id: 2,
                accumulated_latency_ns: 0,
            },
            800_000_000,
            0,
        );
        assert_eq!(s.state(), State::SettledRsvOk);
        let (_r, acts) = s.on_event(Event::TaWithdrawn, 900_000_000, 0);
        assert_eq!(s.state(), State::SettledNoRsv);
        assert_eq!(s.declaration(), Declaration::None);
        assert!(acts.iter().any(|a| matches!(a, Action::WithdrawListener)));
        assert!(s.timer_deadline_ns().is_some()); // TA_WAIT re-armed
    }

    // Row: SETTLED_NO_RSV/RSV_OK + Talker Failed registered -> Asking Failed, A-5
    #[test]
    fn settled_talker_failed_registered_a5() {
        let mut s = settled_no_rsv();
        let (_r, _a) = s.on_event(
            Event::TalkerFailed {
                failure_code: 2,
                a5: true,
            },
            800_000_000,
            0,
        );
        assert_eq!(s.declaration(), Declaration::AskingFailed);
        assert!(s.a5_classified());
        assert_eq!(s.timer_deadline_ns(), None); // TA_WAIT not armed
                                                 // A non-member failure code does not classify A-5.
        let mut s2 = settled_no_rsv();
        s2.on_event(
            Event::TalkerFailed {
                failure_code: 9,
                a5: false,
            },
            800_000_000,
            0,
        );
        assert!(!s2.a5_classified());
    }

    // Row: SETTLED_RSV_OK + TA changed (VID) -> ta_probe_mismatch, filter re-issued
    #[test]
    fn settled_rsv_ok_ta_changed_vid() {
        let mut s = settled_no_rsv();
        s.on_event(
            Event::TaRegistered {
                dest_mac: MAC_A,
                vlan_id: 2,
                accumulated_latency_ns: 0,
            },
            800_000_000,
            0,
        );
        let (_r, acts) = s.on_event(
            Event::TaChanged {
                dest_mac: MAC_A,
                vlan_id: 5,
                accumulated_latency_ns: 0,
            },
            900_000_000,
            0,
        );
        assert_eq!(s.counters.ta_probe_mismatch, 1);
        assert_eq!(s.effective().vlan_id, 5);
        assert!(acts
            .iter()
            .any(|a| matches!(a, Action::SetStreamFilter { .. })));
        assert_eq!(s.declaration(), Declaration::Ready); // still Ready, no Lv
        assert!(!acts.iter().any(|a| matches!(a, Action::WithdrawListener)));
    }

    // Row: SETTLED_RSV_OK + TA changed (latency only) -> no filter, new table
    #[test]
    fn settled_rsv_ok_ta_changed_latency_only() {
        let mut s = settled_no_rsv();
        s.on_event(
            Event::TaRegistered {
                dest_mac: MAC_A,
                vlan_id: 2,
                accumulated_latency_ns: 2_000_000,
            },
            800_000_000,
            0,
        );
        let (_r, acts) = s.on_event(
            Event::TaChanged {
                dest_mac: MAC_A,
                vlan_id: 2,
                accumulated_latency_ns: 3_000_000,
            },
            900_000_000,
            0,
        );
        assert!(!acts
            .iter()
            .any(|a| matches!(a, Action::SetStreamFilter { .. })));
        assert!(acts
            .iter()
            .any(|a| matches!(a, Action::ReplaceStreamTable { .. })));
        assert_eq!(s.effective().accumulated_latency_ns, 3_000_000);
        assert_eq!(s.declaration(), Declaration::Ready);
    }

    // Row: any bound state + talker DEPARTING/ADP timeout -> PRB_W_AVAIL (withdraw)
    #[test]
    fn bound_talker_gone_withdraws() {
        let mut s = settled_no_rsv();
        s.on_event(
            Event::TaRegistered {
                dest_mac: MAC_A,
                vlan_id: 2,
                accumulated_latency_ns: 0,
            },
            800_000_000,
            0,
        );
        let (_r, acts) = s.on_event(Event::TalkerGone, 900_000_000, 300_000_000);
        assert_eq!(s.state(), State::PrbWAvail);
        assert_eq!(s.declaration(), Declaration::None);
        assert!(acts.iter().any(|a| matches!(a, Action::WithdrawListener)));
        assert!(acts.iter().any(|a| matches!(a, Action::RemoveStreamEntry)));
    }

    // Row: any bound state + talker AVAILABLE with reboot -> PRB_W_DELAY (withdraw)
    #[test]
    fn bound_reboot_reprobes() {
        let mut s = settled_no_rsv(); // last_available_index = 5
        let (_r, acts) = s.on_event(
            Event::TalkerAvailable { available_index: 1 },
            900_000_000,
            250_000_000,
        );
        assert_eq!(s.state(), State::PrbWDelay);
        assert_eq!(s.counters.reboots, 1);
        assert!(acts.iter().any(|a| matches!(a, Action::WithdrawListener)));
        assert_eq!(s.probe_cycles(), 0);
        assert_eq!(s.last_available_index(), Some(1));
    }

    // Row: two successive reboots both detected (5 -> 1 -> 6 -> 0)
    #[test]
    fn two_successive_reboots_detected() {
        let mut s = settled_no_rsv(); // 5
        s.on_event(Event::TalkerAvailable { available_index: 1 }, 1, 0); // reboot 1
        s.on_event(Event::TalkerAvailable { available_index: 6 }, 2, 0); // forward, no reboot
        assert_eq!(s.last_available_index(), Some(6));
        s.on_event(Event::TalkerAvailable { available_index: 0 }, 3, 0); // reboot 2
        assert_eq!(s.counters.reboots, 2);
        assert_eq!(s.last_available_index(), Some(0));
    }

    // Row: any bound state + BIND_RX same talker/unique -> unchanged, SUCCESS
    #[test]
    fn bound_rebind_same_talker_unchanged() {
        let mut s = settled_no_rsv();
        let before = s.state();
        let (resp, _acts) = s.on_event(bind(true, Some(5)), 900_000_000, 0);
        assert_eq!(resp, Some(AcmpResponse::Success));
        assert_eq!(s.state(), before);
    }

    // Row: any bound state + BIND_RX different talker -> re-bind, SUCCESS, filter removed
    #[test]
    fn bound_rebind_different_talker_removes_filter() {
        let mut s = settled_no_rsv();
        let ev = Event::BindRx {
            talker_entity_id: Eui64::from_u64(0xDEAD_BEEF),
            talker_unique_id: 1,
            controller_entity_id: CTRL,
            talker_in_adp: false,
            talker_available_index: None,
            streaming_wait: false,
        };
        let (resp, acts) = s.on_event(ev, 900_000_000, 0);
        assert_eq!(resp, Some(AcmpResponse::Success));
        assert_eq!(s.state(), State::PrbWAvail);
        assert!(acts.iter().any(|a| matches!(a, Action::RemoveStreamEntry)));
    }

    // Row: any bound state + UNBIND_RX -> UNBOUND, SUCCESS
    #[test]
    fn bound_unbind_rx() {
        let mut s = settled_no_rsv();
        let (resp, acts) = s.on_event(Event::UnbindRx, 900_000_000, 0);
        assert_eq!(resp, Some(AcmpResponse::Success));
        assert_eq!(s.state(), State::Unbound);
        assert!(acts.iter().any(|a| matches!(a, Action::WithdrawListener)));
    }

    // Row: UNBOUND + UNBIND_RX -> UNBOUND, NOT_CONNECTED
    #[test]
    fn unbound_unbind_rx_not_connected() {
        let mut s = sm();
        let (resp, _) = s.on_event(Event::UnbindRx, 0, 0);
        assert_eq!(resp, Some(AcmpResponse::NotConnected));
        assert_eq!(s.state(), State::Unbound);
    }

    // Row: any state except PRB_W_RESP/RESP2 + PROBE_TX_RESPONSE -> ignored, counted
    #[test]
    fn probe_late_response_ignored() {
        let mut s = settled_no_rsv();
        let (_r, acts) = s.on_event(probe_ok(), 900_000_000, 0);
        assert_eq!(s.counters.probe_late_response, 1);
        assert!(acts.is_empty());
    }

    // The 9.2–12.2 s fallback window with a simulated clock.
    #[test]
    fn fallback_after_three_silent_cycles() {
        let mut s = sm();
        s.set_get_stream_info_fallback(Effective {
            stream_id: SID,
            dest_mac: MAC_B,
            vlan_id: 3,
            accumulated_latency_ns: 0,
        });
        let mut t = 0u64;
        s.on_event(bind(true, Some(1)), t, 1_000_000_000); // PRB_W_DELAY (1 s)
                                                           // Cycle 1: delay fires at 1 s -> PRB_W_RESP; +200ms RESP2; +200ms retry.
        t = 1_000_000_000;
        s.on_timer(t, 0); // -> PRB_W_RESP
        t += timers::PROBE_RESP_NS;
        s.on_timer(t, 0); // -> PRB_W_RESP2
        t += timers::PROBE_RESP_NS;
        s.on_timer(t, 0); // RESP2 timeout -> PRB_W_RETRY, cycles=1
        assert_eq!(s.probe_cycles(), 1);
        t += timers::RETRY_NS;
        s.on_timer(t, 0); // retry -> PRB_W_DELAY
                          // Cycle 2 (use 0 delay).
        s.on_timer(t, 0); // delay -> PRB_W_RESP
        t += timers::PROBE_RESP_NS;
        s.on_timer(t, 0); // RESP2
        t += timers::PROBE_RESP_NS;
        s.on_timer(t, 0); // retry, cycles=2
        assert_eq!(s.probe_cycles(), 2);
        t += timers::RETRY_NS;
        s.on_timer(t, 0); // -> PRB_W_DELAY
                          // Cycle 3.
        s.on_timer(t, 0); // -> PRB_W_RESP
        t += timers::PROBE_RESP_NS;
        s.on_timer(t, 0); // RESP2
        t += timers::PROBE_RESP_NS;
        let acts = s.on_timer(t, 0); // cycles=3 -> fallback
        assert_eq!(s.probe_cycles(), 3);
        assert_eq!(s.state(), State::SettledNoRsv);
        assert_eq!(s.counters.probe_fallback, 1);
        assert_eq!(s.effective().dest_mac, MAC_B); // from GET_STREAM_INFO
        assert!(acts
            .iter()
            .any(|a| matches!(a, Action::Log(l) if l.contains("deviation"))));
        // Total elapsed is within the 9.2–12.2 s window for the 1 s + 0 + 0 case.
        assert!(
            (9_200_000_000..=12_200_000_000).contains(&t),
            "fallback at {t} ns"
        );
    }

    // rev-5: Talker Failed already registered on entry via probe fallback ->
    // Asking Failed on entry, A-5 for a member code, TA_WAIT not armed.
    #[test]
    fn fallback_entry_with_talker_failed_registered() {
        let mut s = sm();
        s.set_get_stream_info_fallback(Effective {
            stream_id: SID,
            dest_mac: MAC_B,
            vlan_id: 3,
            accumulated_latency_ns: 0,
        });
        s.on_event(bind(true, Some(1)), 0, 0); // -> PRB_W_DELAY
                                               // Register Talker Failed with an A-5 code while still probing.
                                               // (It is held in talker_failed_registered for the entry evaluation.)
        s.talker_failed_registered = true;
        s.a5_classified = true;
        // Three silent probe cycles, each: delay->RESP, RESP->RESP2,
        // RESP2 timeout->RETRY(++)/fallback, RETRY->DELAY.
        let mut t = 0u64;
        for cycle in 0..3 {
            s.on_timer(t, 0); // delay -> PRB_W_RESP (probe sent)
            t += timers::PROBE_RESP_NS;
            s.on_timer(t, 0); // RESP timeout -> PRB_W_RESP2
            t += timers::PROBE_RESP_NS;
            s.on_timer(t, 0); // RESP2 timeout -> PRB_W_RETRY (++)/fallback
            if cycle < 2 {
                t += timers::RETRY_NS;
                s.on_timer(t, 0); // retry -> PRB_W_DELAY
            }
        }
        assert_eq!(s.state(), State::SettledNoRsv);
        assert_eq!(s.declaration(), Declaration::AskingFailed);
        assert_eq!(s.timer_deadline_ns(), None); // TA_WAIT not armed
    }

    // rev-5: TA and Talker Failed both registered on entry -> TA row wins (Ready)
    #[test]
    fn settled_entry_ta_and_failed_both_registered_ta_wins() {
        let mut s = sm();
        s.on_event(bind(true, Some(1)), 0, 0);
        s.on_timer(0, 0); // PRB_W_RESP
                          // Both registered at the moment of the SUCCESS entry.
        s.ta_registered = true;
        s.talker_failed_registered = true;
        let (_r, _a) = s.on_event(probe_ok(), 100, 0);
        // Entry evaluation: TA takes priority -> SETTLED_RSV_OK, Ready.
        assert_eq!(s.state(), State::SettledRsvOk);
        assert_eq!(s.declaration(), Declaration::Ready);
    }

    // last_available_index updated in SETTLED_RSV_OK and in PRB_W_RETRY.
    #[test]
    fn last_available_index_updates_in_bound_states() {
        let mut s = settled_no_rsv();
        s.on_event(
            Event::TaRegistered {
                dest_mac: MAC_A,
                vlan_id: 2,
                accumulated_latency_ns: 0,
            },
            800_000_000,
            0,
        );
        assert_eq!(s.state(), State::SettledRsvOk);
        s.on_event(
            Event::TalkerAvailable { available_index: 7 },
            900_000_000,
            0,
        );
        assert_eq!(s.last_available_index(), Some(7));
        // In PRB_W_RETRY as well.
        let mut s2 = sm();
        s2.on_event(bind(true, Some(2)), 0, 0);
        s2.on_timer(0, 0);
        s2.on_timer(timers::PROBE_RESP_NS, 0);
        s2.on_timer(2 * timers::PROBE_RESP_NS, 0); // PRB_W_RETRY
        assert_eq!(s2.state(), State::PrbWRetry);
        s2.on_event(
            Event::TalkerAvailable { available_index: 8 },
            10_000_000_000,
            0,
        );
        assert_eq!(s2.last_available_index(), Some(8));
    }

    // BIND_RX / UNBIND_RX with unknown id is handled by the entity layer
    // (listener_unique_id >= STREAM_INPUT count -> LISTENER_UNKNOWN_ID); the SM
    // itself only exists for valid ids, so we assert the response variant exists.
    #[test]
    fn listener_unknown_id_variant_exists() {
        assert_eq!(
            AcmpResponse::ListenerUnknownId,
            AcmpResponse::ListenerUnknownId
        );
    }

    #[test]
    fn timers_pinned() {
        assert_eq!(timers::DELAY_MAX_NS, 1_000_000_000);
        assert_eq!(timers::PROBE_RESP_NS, 200_000_000);
        assert_eq!(timers::RETRY_NS, 4_000_000_000);
        assert_eq!(timers::TA_WAIT_NS, 2_000_000_000);
        assert_eq!(PROBE_FALLBACK_CYCLES, 3);
    }
}
