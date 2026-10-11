// SPDX-License-Identifier: MIT OR Apache-2.0

//! The gPTP port (design §7.2–§7.4): port state machine, Sync/Follow_Up
//! offset, servo driving, peer delay scheduling, Signaling, step bookkeeping
//! and the gPTP ↔ mach model.
//!
//! The port has a role field ([`PortState`]) but **no** Announce, Sync or
//! Follow_Up transmit path: it can never become master (design §13.3). The
//! only frames it queues are Pdelay_Req, Pdelay_Resp, Pdelay_Resp_Follow_Up
//! and the gPTP-capable Signaling reply.
//!
//! Driving it (daemon `ctl` thread):
//! - every RX slot of class gPTP → [`Port::handle_rx`];
//! - every loop → [`Port::poll`] (queues frames into the gPTP outbox), then
//!   [`Port::take_action`] until `None` (AdjFreq / StepClock / SetClock /
//!   ADP re-advertise);
//! - TX scheduler: [`Port::want_ts_allowed`] before sending a `want_ts`
//!   frame, [`Port::pre_send`] right before `SendFrame` (8 ms late-drop),
//!   [`Port::on_frame_sent`] after it succeeded, [`Port::on_tx_timestamp`]
//!   when `GetTxTimestamp` resolves, [`Port::on_tx_failed`] if it was dropped;
//! - after StepClock/SetClock returns and `hdr.clock_gen` was re-read →
//!   [`Port::on_clock_stepped`] (or [`Port::on_step_failed`]);
//! - every 125 ms → [`Port::on_clock_read`] with the `ReadClock(8)` samples.

use std::collections::VecDeque;

use avb_core::{DropReason, MacAddr, MachTicks, Now, Outbox, RxMeta, Timebase, TxFrame};
use serde::Serialize;

use crate::model::{ClockRead, GptpMachModel, ModelEstimator};
use crate::msg::{
    self, Announce, Body, ClockIdentity, FollowUp, Header, IntervalRequest, PortIdentity,
    Signaling, Timestamp,
};
use crate::pdelay::{
    scaled, AsCapableInputs, Exchange, PdelayInitiator, RespStage, Response,
    GPTP_CAPABLE_RECEIPT_TIMEOUT,
};
use crate::servo::{Servo, ServoOutput, ServoState};
use crate::status::{GmInfo, GptpCounters, GptpStatus, Transition, Transitions, Window};
use crate::stepgen::{classify_rx_gen, PendingStep, RxGen, StepKind, TxGenHold};
use crate::{LatencyProfile, PORT_NUMBER};

/// Announce interval before the first accepted Announce (design §7.3).
pub const TA_PLACEHOLDER_S: f64 = 1.0;
/// Valid Announce logMessageInterval range.
pub const ANNOUNCE_LOG_RANGE: (i8, i8) = (-3, 4);
/// Sync interval placeholder before the first valid Sync.
pub const TS_PLACEHOLDER_S: f64 = 0.125;
/// Valid Sync logMessageInterval range.
pub const SYNC_LOG_RANGE: (i8, i8) = (-5, 1);
/// announceReceiptTimeout / Sync receipt multiplier.
pub const RECEIPT_TIMEOUT_MULT: f64 = 3.0;
/// Our initial logPdelayReqInterval (1 s).
pub const PDELAY_LOG_INITIAL: i8 = 0;
/// Range of message-interval-request linkDelayInterval values we honor.
pub const PDELAY_LOG_RANGE: (i8, i8) = (-3, 3);
/// Our Pdelay_Req waits at most this long behind a response in flight.
pub const PDELAY_REQ_DEFER_NS: u64 = 50_000_000;
/// A Pdelay_Resp later than this after its Pdelay_Req's harvest is dropped.
pub const PDELAY_RESP_LATE_NS: u64 = 8_000_000;

/// Port role. There is no Master: we never send Announce/Sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PortState {
    Listening,
    Slave,
}

impl PortState {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Listening => "LISTENING",
            Self::Slave => "SLAVE",
        }
    }
}

/// `GetTxTimestamp` outcome for a `want_ts` frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxTsResult {
    /// Raw SYSTIM ns.
    Ok(u64),
    Timeout,
}

/// Something the daemon must do on the dext or in other subsystems.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    AdjFreq {
        ppb: i64,
    },
    /// `StepClock(delta_ns)`; report back with `on_clock_stepped`.
    StepClock {
        delta_ns: i64,
    },
    /// `SetClock(ReadClock() + delta_ns)` (|offset| > 1 s); report back with
    /// `on_clock_stepped`.
    SetClock {
        delta_ns: i64,
    },
    /// Re-advertise ADP with this GM identity (design §9.3).
    AdvertiseAdp {
        gm: ClockIdentity,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct PortConfig {
    pub mac: MacAddr,
    pub timebase: Timebase,
    pub latency: LatencyProfile,
    /// Frequency currently applied in the NIC, ppb (0 after InitDatapath).
    pub initial_ppb: f64,
}

impl PortConfig {
    pub fn i210(mac: MacAddr, timebase: Timebase) -> Self {
        Self {
            mac,
            timebase,
            latency: LatencyProfile::I210_1000,
            initial_ppb: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum TagKind {
    PdelayReq = 1,
    PdelayResp = 2,
    PdelayRespFu = 3,
    Signaling = 4,
}

fn tag_kind(tag: u64) -> Option<TagKind> {
    Some(match tag >> 56 {
        1 => TagKind::PdelayReq,
        2 => TagKind::PdelayResp,
        3 => TagKind::PdelayRespFu,
        4 => TagKind::Signaling,
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug)]
struct PendingSync {
    header: Header,
    hw: Option<u64>,
    gen: RxGen,
    at: MachTicks,
}

const LOG_CAP: usize = 256;

pub struct Port {
    mac: MacAddr,
    id: PortIdentity,
    tb: Timebase,
    lat: LatencyProfile,
    gen: u32,
    hold: TxGenHold,
    issued: Option<PendingStep>,
    state: PortState,
    slave_since: MachTicks,
    servo: Servo,
    ini: PdelayInitiator,
    resp: Option<Response>,
    tag_ctr: u32,
    pdelay_log: i8,
    next_req: MachTicks,
    gm: Option<GmInfo>,
    last_gm: Option<ClockIdentity>,
    ta_s: f64,
    announce_deadline: Option<MachTicks>,
    ts_s: f64,
    ts_valid: bool,
    pending_sync: Option<PendingSync>,
    last_sync_used: Option<MachTicks>,
    as_capable: bool,
    ascap_false_since: MachTicks,
    peer_gptp_capable: Option<(i8, MachTicks)>,
    next_sig: Option<MachTicks>,
    sig_seq: u16,
    last_offset: Option<f64>,
    last_rate_ratio: Option<f64>,
    offsets: Window,
    turnaround: Window,
    turnaround_max: Option<f64>,
    model: ModelEstimator,
    actions: VecDeque<Action>,
    log: VecDeque<String>,
    c: GptpCounters,
    h_ascap: Transitions,
    h_port: Transitions,
    h_gen: Transitions,
    h_servo: Transitions,
}

impl Port {
    /// `clock_gen`: `hdr.clock_gen` acquire-loaded after mapping (§5.2 step 5).
    pub fn new(cfg: PortConfig, now: MachTicks, clock_gen: u32) -> Self {
        let id = PortIdentity {
            clock: ClockIdentity::from_mac(cfg.mac),
            port: PORT_NUMBER,
        };
        Self {
            mac: cfg.mac,
            id,
            tb: cfg.timebase,
            lat: cfg.latency,
            gen: clock_gen,
            hold: TxGenHold::default(),
            issued: None,
            state: PortState::Listening,
            slave_since: now,
            servo: Servo::new(cfg.initial_ppb, TS_PLACEHOLDER_S),
            ini: PdelayInitiator::default(),
            resp: None,
            tag_ctr: 0,
            pdelay_log: PDELAY_LOG_INITIAL,
            next_req: now,
            gm: None,
            last_gm: None,
            ta_s: TA_PLACEHOLDER_S,
            announce_deadline: None,
            ts_s: TS_PLACEHOLDER_S,
            ts_valid: false,
            pending_sync: None,
            last_sync_used: None,
            as_capable: false,
            ascap_false_since: now,
            peer_gptp_capable: None,
            next_sig: None,
            sig_seq: 0,
            last_offset: None,
            last_rate_ratio: None,
            offsets: Window::new(64),
            turnaround: Window::new(64),
            turnaround_max: None,
            model: ModelEstimator::new(cfg.timebase),
            actions: VecDeque::new(),
            log: VecDeque::new(),
            c: GptpCounters::default(),
            h_ascap: Transitions::default(),
            h_port: Transitions::default(),
            h_gen: Transitions::default(),
            h_servo: Transitions::default(),
        }
    }

    // --- accessors ---

    pub fn identity(&self) -> PortIdentity {
        self.id
    }

    pub fn state(&self) -> PortState {
        self.state
    }

    pub fn servo_state(&self) -> ServoState {
        self.servo.state()
    }

    pub fn as_capable(&self) -> bool {
        self.as_capable
    }

    pub fn step_gen(&self) -> u32 {
        self.gen
    }

    pub fn counters(&self) -> &GptpCounters {
        &self.c
    }

    pub fn last_offset(&self) -> Option<f64> {
        self.last_offset
    }

    pub fn nrr_systim(&self) -> Option<f64> {
        self.ini.nrr.nrr()
    }

    pub fn delay_comp(&self) -> Option<f64> {
        self.ini.delay()
    }

    pub fn applied_ppb(&self) -> f64 {
        self.servo.f_applied()
    }

    pub fn sync_interval_s(&self) -> f64 {
        self.ts_s
    }

    pub fn announce_interval_s(&self) -> f64 {
        self.ta_s
    }

    pub fn pdelay_log_interval(&self) -> i8 {
        self.pdelay_log
    }

    pub fn model(&self) -> GptpMachModel {
        self.model.model()
    }

    /// GM identity to advertise: the GM in Slave, our own clockIdentity otherwise.
    pub fn gm_identity(&self) -> ClockIdentity {
        match (&self.gm, self.state) {
            (Some(g), PortState::Slave) => g.identity,
            _ => self.id.clock,
        }
    }

    pub fn gm(&self) -> Option<&GmInfo> {
        self.gm.as_ref()
    }

    /// One log line per edge/decision, oldest first.
    pub fn take_log(&mut self) -> Option<String> {
        self.log.pop_front()
    }

    /// A slot was dropped by the ring consumer for `clock_gen` ahead of the
    /// header (avb-dext `SlotError::GenAhead`).
    pub fn note_slot_gen_invalid(&mut self) {
        self.c.slot_gen_invalid += 1;
    }

    fn ns_to_ticks(&self, ns: u64) -> u64 {
        self.tb.ns_to_ticks(ns)
    }

    fn secs_ticks(&self, s: f64) -> u64 {
        self.ns_to_ticks((s * 1e9).max(0.0) as u64)
    }

    fn note(&mut self, now: MachTicks, line: String) {
        if self.log.len() >= LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(format!("[{}] {line}", now.0));
    }

    fn next_tag(&mut self, kind: TagKind, seq: u16) -> u64 {
        self.tag_ctr = self.tag_ctr.wrapping_add(1);
        ((kind as u64) << 56) | (u64::from(self.tag_ctr) << 16) | u64::from(seq)
    }

    fn servo_edge(&mut self, now: MachTicks, before: ServoState, reason: &'static str) {
        let after = self.servo.state();
        if before != after {
            self.h_servo.push(Transition {
                mach: now.0,
                from: before.name().into(),
                to: after.name().into(),
                reason,
            });
            self.note(
                now,
                format!("servo {} -> {} ({reason})", before.name(), after.name()),
            );
            if !matches!(after, ServoState::Tracking | ServoState::Locked) {
                self.model.invalidate();
            }
        }
    }

    fn servo_holdover(&mut self, now: MachTicks, reason: &'static str) {
        let before = self.servo.state();
        if self.servo.holdover() {
            self.servo_edge(now, before, reason);
        }
    }

    fn servo_acquire(&mut self, now: MachTicks, reason: &'static str) {
        let before = self.servo.state();
        self.servo.acquire();
        self.model.reset();
        self.servo_edge(now, before, reason);
    }

    // --- RX ---

    pub fn handle_rx(&mut self, now: Now, frame: &[u8], meta: RxMeta) -> Result<(), DropReason> {
        let m = match msg::decode_frame(frame) {
            Ok(m) => m,
            Err(e) => {
                self.c.rx_dropped += 1;
                return Err(e);
            }
        };
        let h = m.header;
        if h.source.clock == self.id.clock {
            if matches!(m.body, Body::Announce(_)) {
                self.c.announce_rejected += 1;
            } else {
                self.c.rx_dropped += 1;
            }
            return Err(DropReason::OwnFrame);
        }
        let now_m = now.mach;
        match m.body {
            Body::Announce(a) => self.rx_announce(now_m, &h, &a),
            Body::Sync => self.rx_sync(now_m, h, meta),
            Body::FollowUp(fu) => self.rx_follow_up(now_m, &h, &fu),
            Body::PdelayReq => self.rx_pdelay_req(&h, meta),
            Body::PdelayResp(r) => self.rx_pdelay_resp(now_m, &h, &r, meta),
            Body::PdelayRespFollowUp(r) => self.rx_pdelay_resp_fu(now_m, &h, &r),
            Body::Signaling(s) => self.rx_signaling(now_m, &h, &s),
        }
    }

    fn rx_announce(
        &mut self,
        now: MachTicks,
        h: &Header,
        a: &Announce<'_>,
    ) -> Result<(), DropReason> {
        if a.path_trace_contains(self.id.clock) {
            self.c.announce_rejected += 1;
            return Err(DropReason::NotForUs);
        }
        self.c.announce_rx += 1;
        let (lo, hi) = ANNOUNCE_LOG_RANGE;
        if (lo..=hi).contains(&h.log_interval) {
            self.ta_s = 2f64.powi(i32::from(h.log_interval));
        } else {
            self.c.announce_interval_invalid += 1;
        }
        self.announce_deadline =
            Some(now.add_ticks(self.secs_ticks(RECEIPT_TIMEOUT_MULT * self.ta_s)));
        let info = GmInfo {
            identity: a.gm_identity,
            priority1: a.gm_priority1,
            priority2: a.gm_priority2,
            clock_quality: a.gm_clock_quality,
            steps_removed: a.steps_removed,
            time_source: a.time_source,
            current_utc_offset: a.current_utc_offset,
            path_trace: a.path_trace().take(16).collect(),
            master_port: h.source,
        };
        match self.state {
            PortState::Listening => {
                self.set_port_state(now, PortState::Slave, "announce_accepted");
                self.slave_since = now;
                match self.last_gm {
                    None => self.servo_acquire(now, "first_gm"),
                    Some(g) if g == info.identity => {
                        // HOLDOVER → TRACKING on the next usable Sync.
                    }
                    Some(_) => {
                        self.c.gm_changes += 1;
                        self.servo_acquire(now, "gm_change");
                    }
                }
                self.last_gm = Some(info.identity);
                let gm = info.identity;
                self.gm = Some(info);
                self.actions.push_back(Action::AdvertiseAdp { gm });
            }
            PortState::Slave => {
                let changed = self
                    .gm
                    .as_ref()
                    .is_some_and(|g| g.identity != info.identity);
                let gm = info.identity;
                self.gm = Some(info);
                self.last_gm = Some(gm);
                if changed {
                    self.c.gm_changes += 1;
                    self.pending_sync = None;
                    self.note(now, format!("gm change to {gm} without timeout"));
                    self.servo_acquire(now, "gm_change");
                    self.actions.push_back(Action::AdvertiseAdp { gm });
                }
            }
        }
        Ok(())
    }

    fn set_port_state(&mut self, now: MachTicks, to: PortState, reason: &'static str) {
        let from = self.state;
        if from == to {
            return;
        }
        self.state = to;
        self.c.port_state_changes += 1;
        self.h_port.push(Transition {
            mach: now.0,
            from: from.name().into(),
            to: to.name().into(),
            reason,
        });
        self.note(
            now,
            format!("port {} -> {} ({reason})", from.name(), to.name()),
        );
    }

    fn enter_listening(&mut self, now: MachTicks, reason: &'static str) {
        self.set_port_state(now, PortState::Listening, reason);
        self.servo_holdover(now, reason);
        self.model.invalidate();
        self.pending_sync = None;
        self.announce_deadline = None;
        self.actions
            .push_back(Action::AdvertiseAdp { gm: self.id.clock });
    }

    fn master(&self) -> Option<PortIdentity> {
        match self.state {
            PortState::Slave => self.gm.as_ref().map(|g| g.master_port),
            PortState::Listening => None,
        }
    }

    fn rx_sync(&mut self, now: MachTicks, h: Header, meta: RxMeta) -> Result<(), DropReason> {
        self.c.sync_rx += 1;
        if self.state == PortState::Listening {
            self.c.sync_while_listening += 1;
            return Ok(());
        }
        if Some(h.source) != self.master() {
            self.c.foreign_port += 1;
            return Err(DropReason::NotForUs);
        }
        let (lo, hi) = SYNC_LOG_RANGE;
        if (lo..=hi).contains(&h.log_interval) {
            let ts = 2f64.powi(i32::from(h.log_interval));
            if self.ts_valid && ts != self.ts_s {
                self.c.sync_interval_changes += 1;
                self.note(now, format!("sync interval {} s -> {ts} s", self.ts_s));
                // The receipt timeout restarts in the new interval.
                if self.last_sync_used.is_some() {
                    self.last_sync_used = Some(now);
                }
            }
            self.ts_s = ts;
            self.ts_valid = true;
            self.servo.set_interval(ts);
        } else {
            self.c.sync_interval_invalid += 1;
        }
        self.pending_sync = Some(PendingSync {
            header: h,
            hw: meta.hw_ts,
            gen: classify_rx_gen(meta.clock_gen, meta.step_ambig),
            at: now,
        });
        Ok(())
    }

    fn rx_follow_up(
        &mut self,
        now: MachTicks,
        h: &Header,
        fu: &FollowUp,
    ) -> Result<(), DropReason> {
        self.c.follow_up_rx += 1;
        if self.state == PortState::Listening {
            return Ok(());
        }
        if Some(h.source) != self.master() {
            self.c.foreign_port += 1;
            return Err(DropReason::NotForUs);
        }
        let Some(s) = self.pending_sync else {
            return Err(DropReason::Stale);
        };
        if s.header.sequence_id != h.sequence_id || s.header.source != h.source {
            return Err(DropReason::Stale);
        }
        self.pending_sync = None;
        let window = self.secs_ticks(RECEIPT_TIMEOUT_MULT * self.ts_s);
        if now.ticks_since(s.at) > window {
            self.c.follow_up_timeouts += 1;
            return Err(DropReason::Stale);
        }
        // A complete Sync/Follow_Up from the master: the Sync receipt timer
        // restarts even if this one is not usable for the servo.
        if self.last_sync_used.is_some() {
            self.last_sync_used = Some(now);
        }
        self.process_sync(now, &s, h, fu);
        Ok(())
    }

    /// Offset per Sync (design §7.3):
    /// rateRatio = (1 + cSRO·2^-41)·NRR_systim;
    /// syncEvent = POT + (FU.corr + Sync.corr)·2^-16 + delay·rateRatio/NRR;
    /// offset = t_rx_c − syncEvent.
    fn process_sync(&mut self, now: MachTicks, s: &PendingSync, fu_h: &Header, fu: &FollowUp) {
        if !self.as_capable {
            if self.servo.state() == ServoState::Holdover {
                self.c.sync_in_holdover += 1;
            }
            return;
        }
        let Some(t_rx) = s.hw else {
            self.c.rx_no_ts += 1;
            return;
        };
        match s.gen {
            RxGen::Ambiguous => {
                self.c.rx_ts_ambiguous_step += 1;
                self.c.sync_step_skip += 1;
                return;
            }
            RxGen::Gen(g) if g != self.gen => {
                self.c.sync_step_skip += 1;
                return;
            }
            RxGen::Gen(_) => {}
        }
        if self.hold.step_pending() {
            self.c.sync_step_skip += 1;
            return;
        }
        let Some(pot) = fu.precise_origin.to_ns() else {
            return;
        };
        let nrr = self.ini.nrr.nrr().unwrap_or(1.0);
        let rr = fu.info.rate_ratio() * nrr;
        let delay = self.ini.delay().unwrap_or(0.0);
        let offset = sync_offset(
            self.lat.rx_c(t_rx),
            pot,
            fu_h.correction,
            s.header.correction,
            delay,
            rr,
            nrr,
        );
        self.last_offset = Some(offset);
        self.last_rate_ratio = Some(rr);
        self.offsets.push(offset);
        self.last_sync_used = Some(now);
        self.c.syncs_used += 1;
        let before = self.servo.state();
        let out = self.servo.on_sync(offset, rr);
        self.servo_edge(now, before, "sync");
        self.apply_servo(now, out);
    }

    fn apply_servo(&mut self, now: MachTicks, out: ServoOutput) {
        self.c.steps = self.servo.counters.steps;
        self.c.set_clocks = self.servo.counters.set_clocks;
        self.c.saturations = self.servo.counters.saturations;
        self.c.unlocks = self.servo.counters.unlocks;
        if let Some((kind, delta_ns)) = out.step {
            self.note(now, format!("step decided: {kind:?} {delta_ns} ns"));
            self.hold.request(PendingStep {
                kind,
                delta_ns,
                decided_at: now,
            });
        }
        if let Some(f) = out.adj_ppb {
            self.actions.push_back(Action::AdjFreq {
                ppb: f.round() as i64,
            });
        }
    }

    fn rx_pdelay_req(&mut self, h: &Header, meta: RxMeta) -> Result<(), DropReason> {
        self.c.pdelay_req_rx += 1;
        let Some(t2) = meta.hw_ts else {
            self.c.rx_no_ts += 1;
            return Err(DropReason::Unsupported);
        };
        let g = classify_rx_gen(meta.clock_gen, meta.step_ambig);
        if g == RxGen::Ambiguous {
            self.c.rx_ts_ambiguous_step += 1;
        }
        let tag = self.next_tag(TagKind::PdelayResp, h.sequence_id);
        self.resp = Some(Response {
            seq: h.sequence_id,
            requesting: h.source,
            harvest: meta.harvest,
            t2,
            t2_gen: g,
            tag,
            stage: RespStage::Waiting,
            sent_gen: None,
        });
        Ok(())
    }

    fn rx_pdelay_resp(
        &mut self,
        now: MachTicks,
        h: &Header,
        r: &msg::PdelayResp,
        meta: RxMeta,
    ) -> Result<(), DropReason> {
        self.c.pdelay_resp_rx += 1;
        if r.requesting != self.id {
            return Err(DropReason::NotForUs);
        }
        let Some(x) = self.ini.cur.as_mut().filter(|x| x.seq == h.sequence_id) else {
            return Err(DropReason::Stale);
        };
        x.responses += 1;
        if x.responses == 2 {
            self.c.multiple_responders += 1;
        }
        if x.responder.is_some() {
            return Ok(());
        }
        x.responder = Some(h.source);
        let Some(rrt) = r.request_receipt.to_ns() else {
            x.discarded = true;
            return Err(DropReason::Malformed);
        };
        x.t2 = Some(scaled(rrt));
        x.resp_corr = h.correction;
        match (meta.hw_ts, classify_rx_gen(meta.clock_gen, meta.step_ambig)) {
            (None, _) => self.c.rx_no_ts += 1,
            (Some(_), RxGen::Ambiguous) => {
                self.c.rx_ts_ambiguous_step += 1;
                x.discarded = true;
            }
            (Some(t4), g) => x.t4 = Some((t4, g)),
        }
        self.complete_exchange(now);
        Ok(())
    }

    fn rx_pdelay_resp_fu(
        &mut self,
        now: MachTicks,
        h: &Header,
        r: &msg::PdelayRespFollowUp,
    ) -> Result<(), DropReason> {
        self.c.pdelay_resp_fu_rx += 1;
        if r.requesting != self.id {
            return Err(DropReason::NotForUs);
        }
        let Some(x) = self
            .ini
            .cur
            .as_mut()
            .filter(|x| x.seq == h.sequence_id && x.responder == Some(h.source))
        else {
            return Err(DropReason::Stale);
        };
        let Some(t3) = r.response_origin.to_ns() else {
            return Err(DropReason::Malformed);
        };
        // t3 = responseOriginTimestamp + correctionFields (Resp_FU and Resp).
        x.t3 = Some(scaled(t3) + i128::from(h.correction) + i128::from(x.resp_corr));
        self.complete_exchange(now);
        Ok(())
    }

    fn complete_exchange(&mut self, now: MachTicks) {
        if self
            .ini
            .try_complete(self.gen, self.lat, &mut self.c)
            .is_some()
        {
            self.eval_as_capable(now);
        }
    }

    fn rx_signaling(
        &mut self,
        now: MachTicks,
        _h: &Header,
        s: &Signaling,
    ) -> Result<(), DropReason> {
        self.c.signaling_rx += 1;
        let t = s.target;
        let for_us = t == PortIdentity::WILDCARD
            || ((t.clock == self.id.clock || t.clock == PortIdentity::WILDCARD.clock)
                && (t.port == self.id.port || t.port == 0xFFFF));
        if !for_us {
            return Err(DropReason::NotForUs);
        }
        if let Some(ir) = s.interval_request {
            let v = ir.link_delay_interval;
            let (lo, hi) = PDELAY_LOG_RANGE;
            if v == IntervalRequest::NO_CHANGE {
            } else if v == IntervalRequest::SET_INITIAL || (lo..=hi).contains(&v) {
                let new = if v == IntervalRequest::SET_INITIAL {
                    PDELAY_LOG_INITIAL
                } else {
                    v
                };
                self.c.interval_requests_honored += 1;
                self.note(
                    now,
                    format!(
                        "pdelay interval log {} -> {new} (peer request)",
                        self.pdelay_log
                    ),
                );
                self.pdelay_log = new;
            } else {
                self.c.interval_requests_rejected += 1;
                self.note(now, format!("pdelay interval request {v} rejected"));
            }
        }
        if let Some(g) = s.gptp_capable {
            let first = self.peer_gptp_capable.is_none();
            self.peer_gptp_capable = Some((g.log_interval, now));
            if first || self.next_sig.is_none() {
                self.next_sig = Some(now);
            }
        }
        Ok(())
    }

    // --- asCapable ---

    fn gptp_capable_expired(&self, now: MachTicks) -> bool {
        match self.peer_gptp_capable {
            None => false,
            Some((log, at)) => {
                let s = f64::from(GPTP_CAPABLE_RECEIPT_TIMEOUT) * 2f64.powi(i32::from(log));
                now.ticks_since(at) > self.secs_ticks(s)
            }
        }
    }

    fn as_capable_inputs(&self, now: MachTicks) -> AsCapableInputs {
        AsCapableInputs {
            delay_comp: self.ini.delay(),
            lost_since_good: self.ini.lost_since_good,
            multi_responder: self.ini.multi_responder(),
            nrr_valid: self.ini.nrr.valid(),
            gptp_capable_expired: self.gptp_capable_expired(now),
        }
    }

    fn eval_as_capable(&mut self, now: MachTicks) {
        let inputs = self.as_capable_inputs(now);
        let v = inputs.as_capable();
        if v == self.as_capable {
            return;
        }
        self.as_capable = v;
        self.c.ascapable_transitions += 1;
        let reason = inputs.failing().unwrap_or("all_conditions_met");
        self.h_ascap.push(Transition {
            mach: now.0,
            from: (!v).to_string(),
            to: v.to_string(),
            reason,
        });
        self.note(now, format!("asCapable -> {v} ({reason})"));
        if !v {
            self.ascap_false_since = now;
            self.servo_holdover(now, "ascapable_false");
        }
    }

    // --- poll / timers ---

    pub fn poll<const N: usize>(&mut self, now: Now, out: &mut Outbox<N>) {
        let now = now.mach;
        self.timers(now);
        self.poll_responder(out);
        self.poll_initiator(now, out);
        self.poll_signaling(now, out);
    }

    fn timers(&mut self, now: MachTicks) {
        if self.state == PortState::Slave {
            if self.announce_deadline.is_some_and(|d| now >= d) {
                self.c.announce_timeouts += 1;
                self.enter_listening(now, "announce_timeout");
            } else if !self.as_capable {
                let since = self.ascap_false_since.max(self.slave_since);
                if now.ticks_since(since) > self.secs_ticks(RECEIPT_TIMEOUT_MULT * self.ta_s) {
                    self.c.ascapable_slave_loss += 1;
                    self.enter_listening(now, "ascapable_false");
                }
            }
        }
        let rt = self.secs_ticks(RECEIPT_TIMEOUT_MULT * self.ts_s);
        if self
            .pending_sync
            .is_some_and(|s| now.ticks_since(s.at) > rt)
        {
            self.pending_sync = None;
            self.c.follow_up_timeouts += 1;
        }
        if matches!(
            self.servo.state(),
            ServoState::Tracking | ServoState::Locked
        ) && self.last_sync_used.is_some_and(|t| now.ticks_since(t) > rt)
        {
            self.c.sync_timeouts += 1;
            self.servo_holdover(now, "sync_timeout");
        }
        self.eval_as_capable(now);
    }

    fn poll_responder<const N: usize>(&mut self, out: &mut Outbox<N>) {
        let Some(r) = self.resp else { return };
        match r.stage {
            RespStage::Waiting if self.hold.want_ts_allowed() => {
                let ts = Timestamp::from_ns(self.lat.rx_c(r.t2));
                let f = msg::encode_pdelay_resp(self.mac, self.id, r.seq, ts, r.requesting, r.tag);
                if self.push(out, f) {
                    self.set_resp_stage(RespStage::InFlight);
                } else {
                    self.resp = None;
                }
            }
            RespStage::FollowUp(t3) => {
                let ts = Timestamp::from_ns(self.lat.tx_c(t3));
                let tag = self.next_tag(TagKind::PdelayRespFu, r.seq);
                let f = msg::encode_pdelay_resp_follow_up(
                    self.mac,
                    self.id,
                    r.seq,
                    ts,
                    r.requesting,
                    tag,
                );
                self.push(out, f);
                self.set_resp_stage(RespStage::Done);
            }
            _ => {}
        }
    }

    fn set_resp_stage(&mut self, s: RespStage) {
        if let Some(r) = self.resp.as_mut() {
            r.stage = s;
        }
    }

    fn push<const N: usize>(&mut self, out: &mut Outbox<N>, f: TxFrame) -> bool {
        if out.push(f).is_err() {
            self.c.pdelay_tx_fail += 1;
            return false;
        }
        true
    }

    fn pdelay_interval_ticks(&self) -> u64 {
        self.secs_ticks(2f64.powi(i32::from(self.pdelay_log)))
    }

    fn poll_initiator<const N: usize>(&mut self, now: MachTicks, out: &mut Outbox<N>) {
        if now < self.next_req {
            return;
        }
        let resp_busy = self
            .resp
            .is_some_and(|r| !matches!(r.stage, RespStage::Done));
        if resp_busy && now.ticks_since(self.next_req) < self.ns_to_ticks(PDELAY_REQ_DEFER_NS) {
            return;
        }
        if !self.hold.want_ts_allowed() {
            return;
        }
        self.ini.finish_previous(&mut self.c);
        self.ini.seq = self.ini.seq.wrapping_add(1);
        let seq = self.ini.seq;
        let tag = self.next_tag(TagKind::PdelayReq, seq);
        self.ini.cur = Some(Exchange::new(seq, tag));
        let f = msg::encode_pdelay_req(self.mac, self.id, seq, self.pdelay_log, tag);
        self.push(out, f);
        let iv = self.pdelay_interval_ticks();
        let next = self.next_req.add_ticks(iv);
        self.next_req = if next <= now { now.add_ticks(iv) } else { next };
        self.eval_as_capable(now);
    }

    fn poll_signaling<const N: usize>(&mut self, now: MachTicks, out: &mut Outbox<N>) {
        let (Some((log, _)), Some(due)) = (self.peer_gptp_capable, self.next_sig) else {
            return;
        };
        if now < due {
            return;
        }
        if self.gptp_capable_expired(now) {
            self.next_sig = None;
            return;
        }
        self.sig_seq = self.sig_seq.wrapping_add(1);
        let tag = self.next_tag(TagKind::Signaling, self.sig_seq);
        let f = msg::encode_signaling_gptp_capable(self.mac, self.id, self.sig_seq, log, tag);
        self.push(out, f);
        self.next_sig = Some(now.add_ticks(self.secs_ticks(2f64.powi(i32::from(log)))));
    }

    /// Earliest time `poll` has work.
    pub fn next_deadline(&self) -> Option<MachTicks> {
        let rt = self.secs_ticks(RECEIPT_TIMEOUT_MULT * self.ts_s);
        let mut d = vec![Some(self.next_req), self.announce_deadline, self.next_sig];
        d.push(self.pending_sync.map(|s| s.at.add_ticks(rt)));
        if matches!(
            self.servo.state(),
            ServoState::Tracking | ServoState::Locked
        ) {
            d.push(self.last_sync_used.map(|t| t.add_ticks(rt)));
        }
        if self.state == PortState::Slave && !self.as_capable {
            let since = self.ascap_false_since.max(self.slave_since);
            d.push(Some(since.add_ticks(
                self.secs_ticks(RECEIPT_TIMEOUT_MULT * self.ta_s) + 1,
            )));
        }
        if let Some((log, at)) = self.peer_gptp_capable {
            let s = f64::from(GPTP_CAPABLE_RECEIPT_TIMEOUT) * 2f64.powi(i32::from(log));
            d.push(Some(at.add_ticks(self.secs_ticks(s) + 1)));
        }
        if self
            .resp
            .is_some_and(|r| matches!(r.stage, RespStage::Waiting | RespStage::FollowUp(_)))
        {
            d.push(Some(MachTicks(0)));
        }
        d.into_iter().flatten().min()
    }

    // --- TX feedback ---

    /// Whether a new `want_ts` frame may be sent (no step pending or in flight).
    pub fn want_ts_allowed(&self) -> bool {
        self.hold.want_ts_allowed()
    }

    /// Called right before `SendFrame` for a gPTP frame. `false` → drop it:
    /// a Pdelay_Resp more than 8 ms after its Pdelay_Req's harvest (design
    /// §7.2, rev 5), or a response that was superseded.
    pub fn pre_send(&mut self, tag: u64, now: MachTicks) -> bool {
        if tag_kind(tag) != Some(TagKind::PdelayResp) {
            return true;
        }
        let late = self.ns_to_ticks(PDELAY_RESP_LATE_NS);
        match self.resp {
            Some(r) if r.tag == tag => {
                if now.ticks_since(r.harvest) > late {
                    self.c.pdelay_resp_late_drop += 1;
                    self.set_resp_stage(RespStage::Done);
                    self.note(now, format!("pdelay_resp seq {} late, dropped", r.seq));
                    false
                } else {
                    true
                }
            }
            _ => false,
        }
    }

    /// `SendFrame` accepted the frame.
    pub fn on_frame_sent(&mut self, tag: u64, _now: MachTicks) {
        match tag_kind(tag) {
            Some(TagKind::PdelayReq) => {
                self.c.pdelay_req_tx += 1;
                self.hold.on_want_ts_sent(tag, self.gen);
            }
            Some(TagKind::PdelayResp) => {
                self.c.pdelay_resp_tx += 1;
                self.hold.on_want_ts_sent(tag, self.gen);
                let g = self.gen;
                if let Some(r) = self.resp.as_mut().filter(|r| r.tag == tag) {
                    r.sent_gen = Some(g);
                }
            }
            Some(TagKind::PdelayRespFu) => self.c.pdelay_resp_fu_tx += 1,
            Some(TagKind::Signaling) => self.c.signaling_tx += 1,
            None => {}
        }
    }

    /// The scheduler dropped a frame it could not send.
    pub fn on_tx_failed(&mut self, tag: u64) {
        self.hold.on_ts_resolved(tag);
        self.c.pdelay_tx_fail += 1;
        if tag_kind(tag) == Some(TagKind::PdelayResp) && self.resp.is_some_and(|r| r.tag == tag) {
            self.set_resp_stage(RespStage::Done);
        }
    }

    /// `GetTxTimestamp` resolved for a `want_ts` frame.
    pub fn on_tx_timestamp(&mut self, tag: u64, res: TxTsResult, now: MachTicks) {
        let sent_gen = self.hold.sent_gen(tag);
        self.hold.on_ts_resolved(tag);
        match tag_kind(tag) {
            Some(TagKind::PdelayReq) => {
                let gen = sent_gen.unwrap_or(self.gen);
                let Some(x) = self.ini.cur.as_mut().filter(|x| x.tag == tag) else {
                    return;
                };
                match res {
                    TxTsResult::Ok(t1) => {
                        x.t1 = Some((t1, gen));
                        self.complete_exchange(now);
                    }
                    TxTsResult::Timeout => self.c.pdelay_tx_ts_fail += 1,
                }
            }
            Some(TagKind::PdelayResp) => {
                let Some(r) = self.resp.filter(|r| r.tag == tag) else {
                    return;
                };
                match res {
                    TxTsResult::Timeout => {
                        self.c.pdelay_resp_ts_fail += 1;
                        self.set_resp_stage(RespStage::Done);
                    }
                    TxTsResult::Ok(t3) => {
                        let g = r.sent_gen.or(sent_gen).unwrap_or(self.gen);
                        if r.t2_gen != RxGen::Gen(g) {
                            self.c.pdelay_resp_step_skip += 1;
                            self.set_resp_stage(RespStage::Done);
                            return;
                        }
                        let turn = self.lat.tx_c(t3) as f64 - self.lat.rx_c(r.t2) as f64;
                        self.turnaround.push(turn);
                        self.turnaround_max =
                            Some(self.turnaround_max.map_or(turn, |m| m.max(turn)));
                        self.set_resp_stage(RespStage::FollowUp(t3));
                    }
                }
            }
            _ => {}
        }
    }

    // --- clock actions ---

    /// Next thing to do. Steps come out only when no timestamp is
    /// outstanding (TX-generation hold).
    pub fn take_action(&mut self) -> Option<Action> {
        if let Some(a) = self.actions.pop_front() {
            return Some(a);
        }
        let s = self.hold.take()?;
        self.issued = Some(s);
        Some(match s.kind {
            StepKind::Step => Action::StepClock {
                delta_ns: s.delta_ns,
            },
            StepKind::Set => Action::SetClock {
                delta_ns: s.delta_ns,
            },
        })
    }

    pub fn step_pending(&self) -> bool {
        self.hold.step_pending()
    }

    /// StepClock/SetClock returned; `new_gen` is `hdr.clock_gen` re-read
    /// right after it. Clears the NRR window (holding the last NRR) and
    /// resets the model.
    pub fn on_clock_stepped(&mut self, now: MachTicks, new_gen: u32) {
        self.hold.on_step_done();
        self.issued = None;
        if new_gen == self.gen {
            return;
        }
        self.h_gen.push(Transition {
            mach: now.0,
            from: self.gen.to_string(),
            to: new_gen.to_string(),
            reason: "step",
        });
        self.note(now, format!("step generation {} -> {new_gen}", self.gen));
        self.gen = new_gen;
        self.c.step_gen_changes += 1;
        self.ini.nrr.on_gen_change();
        self.model.reset();
    }

    /// The issued step was rejected (StepClock BadArgument → retried as SetClock).
    pub fn on_step_failed(&mut self, now: MachTicks) {
        self.hold.on_step_done();
        if let Some(s) = self.issued.take() {
            if s.kind == StepKind::Step {
                self.note(now, "StepClock rejected; retrying as SetClock".into());
                self.hold.request(PendingStep {
                    kind: StepKind::Set,
                    ..s
                });
            } else {
                self.note(now, "SetClock failed".into());
            }
        }
    }

    /// `ReadClock(8)` samples taken every 125 ms (design §7.5).
    pub fn on_clock_read(&mut self, _now: MachTicks, reads: &[ClockRead]) -> GptpMachModel {
        let usable = self.state == PortState::Slave
            && matches!(
                self.servo.state(),
                ServoState::Tracking | ServoState::Locked
            )
            && !self.hold.step_pending();
        self.model.push_read(
            reads,
            self.last_offset.unwrap_or(0.0),
            usable && self.last_offset.is_some(),
        )
    }

    // --- status ---

    pub fn status(&self) -> GptpStatus {
        let nrr = self.ini.nrr.nrr();
        let model = self.model.model();
        GptpStatus {
            port_identity: self.id,
            port_state: self.state,
            as_capable: self.as_capable,
            gm_identity: self.gm_identity(),
            gm: self.gm.clone(),
            servo_state: self.servo.state(),
            step_gen: self.gen,
            offset_ns: self.offsets.summary(),
            path_delay_raw_ns: self.ini.delay_raw.summary(),
            path_delay_comp_ns: self.ini.delay_comp.summary(),
            nrr_systim: nrr,
            nrr_free: nrr.map(|n| n * (1.0 + self.servo.f_applied() * 1e-9)),
            cumulative_rate_ratio: self.last_rate_ratio,
            applied_ppb: self.servo.f_applied(),
            sync_interval_s: self.ts_s,
            announce_interval_s: self.ta_s,
            pdelay_log_interval: self.pdelay_log,
            responder_turnaround_ns: self.turnaround.summary(),
            responder_turnaround_max_ns: self.turnaround_max,
            gptp_ppm_vs_mach: model.valid.then(|| model.ppm_vs_mach()),
            model_uncertainty_ns: model.valid.then_some(model.uncertainty_ns),
            lost_responses_since_good: self.ini.lost_since_good,
            counters: self.c,
            ascapable_history: self.h_ascap.clone(),
            port_state_history: self.h_port.clone(),
            step_gen_history: self.h_gen.clone(),
            servo_history: self.h_servo.clone(),
        }
    }
}

/// `t_rx_c − (POT + (fu_corr + sync_corr)·2^-16 + delay·rr/nrr)`, ns. Integer
/// parts are subtracted in i128 before converting, so f64 only carries the
/// difference.
pub fn sync_offset(
    t_rx_c: u64,
    precise_origin_ns: u64,
    fu_correction: i64,
    sync_correction: i64,
    delay_comp: f64,
    rate_ratio: f64,
    nrr: f64,
) -> f64 {
    let base = (i128::from(t_rx_c) - i128::from(precise_origin_ns)) as f64;
    let corr = (i128::from(fu_correction) + i128::from(sync_correction)) as f64 / 65536.0;
    let nrr = if nrr > 0.0 { nrr } else { 1.0 };
    base - corr - delay_comp * rate_ratio / nrr
}
