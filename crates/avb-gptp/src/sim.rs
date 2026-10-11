// SPDX-License-Identifier: MIT OR Apache-2.0

//! Test-only simulation of the 16R (GM + peer-delay responder/initiator), the
//! dext's timestamping, step generations and the §3.6 settle-window rule,
//! and the daemon's TX scheduler. Time is mach ns (identity timebase).
//!
//! The 16R side builds Sync/Follow_Up/Announce frames here, in test code
//! only: the library itself has no encoder for those messages.
#![allow(clippy::indexing_slicing)]

use avb_core::{MacAddr, MachTicks, Now, Outbox, RxMeta, Timebase};

use crate::msg::{self, Body, ClockIdentity, MessageType, PortIdentity, Timestamp};
use crate::port::{Action, Port, PortConfig, TxTsResult};
use crate::servo::ServoState;
use crate::LatencyProfile;

pub const OUR_MAC: MacAddr = MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]);
pub const PEER_MAC: MacAddr = MacAddr([0x00, 0x0a, 0x92, 0x03, 0x3b, 0xa9]);
pub const GM_ID: ClockIdentity = ClockIdentity([0x00, 0x0a, 0x92, 0xff, 0xfe, 0x03, 0x3b, 0xa9]);
pub const OTHER_GM: ClockIdentity = ClockIdentity([0x00, 0x1b, 0x21, 0xff, 0xfe, 0x00, 0x00, 0x01]);
pub const PEER_PORT: PortIdentity = PortIdentity {
    clock: GM_ID,
    port: 1,
};
pub const MS: u64 = 1_000_000;
pub const S: u64 = 1_000_000_000;
const GM_EPOCH_NS: u64 = 1_700_000_000_000_000_000;

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 1.0) / (1u64 << 53) as f64
    }

    pub fn gauss(&mut self) -> f64 {
        let (u1, u2) = (self.uniform(), self.uniform());
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

// --- 16R-side frame builders (test only) ---

fn header(
    t: MessageType,
    len: usize,
    source: PortIdentity,
    seq: u16,
    log: i8,
    flags: u16,
    corr: i64,
) -> Vec<u8> {
    let mut v = Vec::with_capacity(len + 14);
    v.extend_from_slice(&msg::PTP_PEER_MCAST.0);
    v.extend_from_slice(&PEER_MAC.0);
    v.extend_from_slice(&msg::ETHERTYPE_PTP.to_be_bytes());
    v.push(0x10 | t as u8);
    v.push(0x02);
    v.extend_from_slice(&(len as u16).to_be_bytes());
    v.extend_from_slice(&[0, 0]);
    v.extend_from_slice(&flags.to_be_bytes());
    v.extend_from_slice(&corr.to_be_bytes());
    v.extend_from_slice(&[0; 4]);
    v.extend_from_slice(&source.clock.0);
    v.extend_from_slice(&source.port.to_be_bytes());
    v.extend_from_slice(&seq.to_be_bytes());
    v.push(match t {
        MessageType::Sync => 0,
        MessageType::FollowUp => 2,
        _ => 5,
    });
    v.push(log as u8);
    v
}

fn ts(v: &mut Vec<u8>, ns: u64) {
    let t = Timestamp::from_ns(ns);
    v.extend_from_slice(&t.secs.to_be_bytes()[2..]);
    v.extend_from_slice(&t.nanos.to_be_bytes());
}

pub fn sync_frame(seq: u16, log: i8, corr: i64) -> Vec<u8> {
    let mut v = header(MessageType::Sync, 44, PEER_PORT, seq, log, 0x0208, corr);
    v.extend_from_slice(&[0; 10]);
    v
}

pub fn follow_up_frame(seq: u16, log: i8, pot: u64, corr: i64, csro: i32) -> Vec<u8> {
    let mut v = header(MessageType::FollowUp, 76, PEER_PORT, seq, log, 0x0008, corr);
    ts(&mut v, pot);
    v.extend_from_slice(&[0x00, 0x03, 0x00, 0x1C, 0x00, 0x80, 0xC2, 0x00, 0x00, 0x01]);
    v.extend_from_slice(&csro.to_be_bytes());
    v.extend_from_slice(&[0; 18]);
    v
}

pub fn announce_frame(
    seq: u16,
    log: i8,
    source: PortIdentity,
    gm: ClockIdentity,
    path: &[ClockIdentity],
) -> Vec<u8> {
    let len = 64 + 4 + 8 * path.len();
    let mut v = header(MessageType::Announce, len, source, seq, log, 0x0008, 0);
    v.extend_from_slice(&[0; 10]);
    v.extend_from_slice(&37i16.to_be_bytes());
    v.push(0);
    v.extend_from_slice(&[246, 248, 0xFE, 0x41, 0x00, 248]);
    v.extend_from_slice(&gm.0);
    v.extend_from_slice(&[0, 0, 0xA0]);
    v.extend_from_slice(&0x0008u16.to_be_bytes());
    v.extend_from_slice(&((8 * path.len()) as u16).to_be_bytes());
    for p in path {
        v.extend_from_slice(&p.0);
    }
    v
}

pub fn pdelay_req_frame(seq: u16) -> Vec<u8> {
    let mut v = header(MessageType::PdelayReq, 54, PEER_PORT, seq, 0, 0x0008, 0);
    v.extend_from_slice(&[0; 20]);
    v
}

pub fn pdelay_resp_frame(
    seq: u16,
    t2: u64,
    requesting: PortIdentity,
    source: PortIdentity,
) -> Vec<u8> {
    let mut v = header(MessageType::PdelayResp, 54, source, seq, 0x7F, 0x0208, 0);
    ts(&mut v, t2);
    v.extend_from_slice(&requesting.clock.0);
    v.extend_from_slice(&requesting.port.to_be_bytes());
    v
}

pub fn pdelay_resp_fu_frame(
    seq: u16,
    t3: u64,
    requesting: PortIdentity,
    source: PortIdentity,
) -> Vec<u8> {
    let mut v = header(
        MessageType::PdelayRespFollowUp,
        54,
        source,
        seq,
        0x7F,
        0x0008,
        0,
    );
    ts(&mut v, t3);
    v.extend_from_slice(&requesting.clock.0);
    v.extend_from_slice(&requesting.port.to_be_bytes());
    v
}

pub fn signaling_frame(subtype: u8, a: i8, b: i8, c: i8) -> Vec<u8> {
    let mut v = header(MessageType::Signaling, 60, PEER_PORT, 1, 0x7F, 0x0008, 0);
    v.extend_from_slice(&[0xFF; 10]);
    v.extend_from_slice(&[
        0x00, 0x03, 0x00, 0x0C, 0x00, 0x80, 0xC2, 0x00, 0x00, subtype,
    ]);
    v.extend_from_slice(&[a as u8, b as u8, c as u8, 0, 0, 0]);
    v
}

// --- the simulation ---

#[derive(Clone, Copy, Debug)]
pub struct SimCfg {
    pub sync_log: i8,
    pub announce_log: i8,
    /// GM frequency relative to our free-running oscillator, ppm.
    pub gm_ppm: f64,
    /// RMS noise on our (I210) RX/TX timestamps, ns.
    pub noise_ns: f64,
    /// RMS noise on the 16R's timestamps, ns.
    pub peer_noise_ns: f64,
    /// True one-way link delay (cable + PHY), ns.
    pub delay_ns: f64,
    pub tick_ns: u64,
    pub harvest_ns: u64,
    pub seed: u64,
    /// Error of each StepClock/SetClock write, ns.
    pub step_residual_ns: i64,
}

impl Default for SimCfg {
    fn default() -> Self {
        Self {
            sync_log: -3,
            announce_log: 0,
            gm_ppm: -83.0,
            noise_ns: 10.0,
            peer_noise_ns: 0.0,
            delay_ns: 142.6,
            tick_ns: 500_000,
            harvest_ns: 300_000,
            seed: 0x5EED,
            step_residual_ns: 500,
        }
    }
}

struct RxEv {
    at: u64,
    frame: Vec<u8>,
    hw: Option<u64>,
    /// Slot generation / ambiguity overrides (default: the dext rule).
    gen: Option<u32>,
    ambig: Option<bool>,
}

pub struct Sent {
    pub tau: u64,
    pub t: MessageType,
    pub seq: u16,
    pub bytes: Vec<u8>,
}

/// One accepted offset as the port computed it.
#[derive(Clone, Copy, Debug)]
pub struct OffsetRec {
    pub tau: u64,
    pub offset: f64,
}

pub struct Sim {
    pub cfg: SimCfg,
    pub port: Port,
    out: Box<Outbox<16>>,
    pub tau: u64,
    rng: Rng,
    // Our SYSTIM: s_ps at s_tau, rate (1 + f·1e-9).
    s_ps: i128,
    s_tau: u64,
    pub f_ppb: f64,
    // GM time: g_ps at g_tau, rate (1 + ppm·1e-6).
    g_ps: i128,
    g_tau: u64,
    pub gm_ppm: f64,
    pub gen: u32,
    step_tau: u64,
    ambig_pending: bool,
    /// Delay of the first tick after a step (simulated late dext tick).
    pub delay_tick_after_step: u64,
    extra_delay: u64,
    rx: Vec<RxEv>,
    next_sync_g: u64,
    sync_seq: u16,
    pub sync_log: i8,
    pub sync_corr: i64,
    next_ann: u64,
    ann_seq: u16,
    pub announce_log: i8,
    pub announces_on: bool,
    /// Announce emission interval if different from the advertised one.
    pub announce_emit_log: Option<i8>,
    pub last_ann_tau: u64,
    /// On the next step: inject a Sync+Follow_Up and a 16R Pdelay_Req
    /// captured just before the SYSTIM write (old timebase).
    pub inject_pre_step: bool,
    pub pre_step_req_seq: Option<u16>,
    /// Feed ReadClock samples every 125 ms.
    pub clock_reads: bool,
    next_read: u64,
    pub syncs_on: bool,
    pub gm_id: ClockIdentity,
    next_peer_req: u64,
    peer_req_seq: u16,
    pub peer_responds: bool,
    pub peer_requests: bool,
    pub peer_multi: bool,
    /// TX timestamps: false → GetTxTimestamp times out.
    pub tx_ts_ok: bool,
    /// Extra time before a TX timestamp resolves.
    pub tx_ts_delay: u64,
    ts: Option<(u64, u64, u64)>,
    pub sent: Vec<Sent>,
    pub offsets: Vec<OffsetRec>,
    last_used: u64,
    pub adp: Vec<ClockIdentity>,
    pub steps: Vec<(u64, i64)>,
    pub lock_tau: Option<u64>,
}

impl Sim {
    pub fn new(cfg: SimCfg) -> Self {
        let port = Port::new(
            PortConfig {
                mac: OUR_MAC,
                timebase: Timebase::IDENTITY,
                latency: LatencyProfile::I210_1000,
                initial_ppb: 0.0,
            },
            MachTicks(0),
            0,
        );
        Self {
            cfg,
            port,
            out: Box::new(Outbox::new()),
            tau: 0,
            rng: Rng::new(cfg.seed),
            s_ps: 5_000_000_000_000, // SYSTIM 5 s at start: far from the GM
            s_tau: 0,
            f_ppb: 0.0,
            g_ps: i128::from(GM_EPOCH_NS) * 1000,
            g_tau: 0,
            gm_ppm: cfg.gm_ppm,
            gen: 0,
            step_tau: 0,
            ambig_pending: false,
            delay_tick_after_step: 0,
            extra_delay: 0,
            rx: Vec::new(),
            next_sync_g: GM_EPOCH_NS + 50 * MS,
            sync_seq: 0,
            sync_log: cfg.sync_log,
            sync_corr: 0,
            next_ann: 20 * MS,
            ann_seq: 0,
            announce_log: cfg.announce_log,
            announces_on: true,
            announce_emit_log: None,
            last_ann_tau: 0,
            inject_pre_step: false,
            pre_step_req_seq: None,
            clock_reads: false,
            next_read: 0,
            syncs_on: true,
            gm_id: GM_ID,
            next_peer_req: 370 * MS,
            peer_req_seq: 100,
            peer_responds: true,
            peer_requests: true,
            peer_multi: false,
            tx_ts_ok: true,
            tx_ts_delay: 0,
            ts: None,
            sent: Vec::new(),
            offsets: Vec::new(),
            last_used: 0,
            adp: Vec::new(),
            steps: Vec::new(),
            lock_tau: None,
        }
    }

    /// Our SYSTIM (ns) at τ.
    pub fn systim_i(&self, tau: f64) -> i128 {
        let dt = tau - self.s_tau as f64;
        (self.s_ps + (dt * 1000.0 * (1.0 + self.f_ppb * 1e-9)).round() as i128) / 1000
    }

    /// GM time (ns) at τ.
    pub fn gm_time(&self, tau: f64) -> i128 {
        let dt = tau - self.g_tau as f64;
        (self.g_ps + (dt * 1000.0 * (1.0 + self.gm_ppm * 1e-6)).round() as i128) / 1000
    }

    /// True SYSTIM − GM at the current τ.
    pub fn true_offset(&self) -> f64 {
        let t = self.tau as f64;
        (self.systim_i(t) - self.gm_time(t)) as f64
    }

    fn rebase_s(&mut self) {
        let dt = self.tau as f64 - self.s_tau as f64;
        self.s_ps += (dt * 1000.0 * (1.0 + self.f_ppb * 1e-9)).round() as i128;
        self.s_tau = self.tau;
    }

    pub fn set_gm_ppm(&mut self, ppm: f64) {
        let dt = self.tau as f64 - self.g_tau as f64;
        self.g_ps += (dt * 1000.0 * (1.0 + self.gm_ppm * 1e-6)).round() as i128;
        self.g_tau = self.tau;
        self.gm_ppm = ppm;
    }

    fn noise(&mut self, rms: f64) -> i128 {
        (self.rng.gauss() * rms).round() as i128
    }

    fn our_rx_ts(&mut self, tau: f64) -> u64 {
        let n = self.noise(self.cfg.noise_ns);
        (self.systim_i(tau) + n + i128::from(LatencyProfile::I210_1000.rx_ns)) as u64
    }

    fn our_tx_ts(&mut self, tau: f64) -> u64 {
        let n = self.noise(self.cfg.noise_ns);
        (self.systim_i(tau) + n - i128::from(LatencyProfile::I210_1000.tx_ns)) as u64
    }

    fn peer_ts(&mut self, tau: f64) -> u64 {
        let n = self.noise(self.cfg.peer_noise_ns);
        (self.gm_time(tau) + n) as u64
    }

    pub fn inject(&mut self, at: u64, frame: Vec<u8>, hw: Option<u64>) {
        self.rx.push(RxEv {
            at,
            frame,
            hw,
            gen: None,
            ambig: None,
        });
    }

    /// Inject with an explicit slot generation and ambiguity flag.
    pub fn inject_slot(&mut self, at: u64, frame: Vec<u8>, hw: Option<u64>, gen: u32, ambig: bool) {
        self.rx.push(RxEv {
            at,
            frame,
            hw,
            gen: Some(gen),
            ambig: Some(ambig),
        });
    }

    /// Our RX timestamp for a frame arriving now (raw, with noise).
    pub fn rx_ts_now(&mut self) -> u64 {
        self.our_rx_ts(self.tau as f64)
    }

    /// Disturb SYSTIM by `delta_ns` without a generation change (the servo
    /// sees an offset excursion and decides a step).
    pub fn disturb(&mut self, delta_ns: i64) {
        self.rebase_s();
        self.s_ps += i128::from(delta_ns) * 1000;
    }

    fn tau_of_gm(&self, g: u64) -> f64 {
        let diff = (i128::from(g) * 1000 - self.g_ps) as f64 / 1000.0;
        self.g_tau as f64 + diff / (1.0 + self.gm_ppm * 1e-6)
    }

    fn gm_emit(&mut self) {
        let d = self.cfg.delay_ns;
        let h = self.cfg.harvest_ns as f64;
        // Sync + Follow_Up.
        loop {
            let ts_ns = (2f64.powi(i32::from(self.sync_log)) * 1e9) as u64;
            let tau_s = self.tau_of_gm(self.next_sync_g);
            if tau_s > self.tau as f64 {
                break;
            }
            if self.syncs_on {
                self.sync_seq = self.sync_seq.wrapping_add(1);
                let pot = self.next_sync_g;
                let ta = tau_s + d;
                let hw = self.our_rx_ts(ta);
                let corr = self.sync_corr;
                let sf = sync_frame(self.sync_seq, self.sync_log, corr);
                self.inject((ta + h) as u64, sf, Some(hw));
                let ff = follow_up_frame(self.sync_seq, self.sync_log, pot, -corr, 0);
                self.inject((ta + h + 1e6) as u64, ff, None);
            }
            self.next_sync_g += ts_ns;
        }
        // Announce.
        if self.tau >= self.next_ann {
            if self.announces_on {
                self.ann_seq = self.ann_seq.wrapping_add(1);
                let f = announce_frame(
                    self.ann_seq,
                    self.announce_log,
                    PEER_PORT,
                    self.gm_id,
                    &[self.gm_id],
                );
                self.last_ann_tau = self.tau + self.cfg.harvest_ns;
                self.inject(self.last_ann_tau, f, None);
            }
            let log = self
                .announce_emit_log
                .unwrap_or(self.announce_log)
                .clamp(-3, 4);
            self.next_ann += (2f64.powi(i32::from(log)) * 1e9) as u64;
        }
        // The 16R's own Pdelay_Req.
        if self.tau >= self.next_peer_req {
            if self.peer_requests {
                self.peer_req_seq = self.peer_req_seq.wrapping_add(1);
                let ta = self.tau as f64 + d;
                let hw = self.our_rx_ts(ta);
                let f = pdelay_req_frame(self.peer_req_seq);
                self.inject((ta + h) as u64, f, Some(hw));
            }
            self.next_peer_req += S;
        }
    }

    fn tick_ambig(&mut self) -> bool {
        if !self.ambig_pending {
            return false;
        }
        if self.tau >= self.step_tau + MS {
            self.ambig_pending = false;
        }
        true
    }

    pub fn tick(&mut self) {
        self.tau += self.cfg.tick_ns + std::mem::take(&mut self.extra_delay);
        let now = Now::mach_only(MachTicks(self.tau));
        self.gm_emit();
        let ambig = self.tick_ambig();
        let tau = self.tau;
        let mut due: Vec<RxEv> = Vec::new();
        let mut keep = Vec::new();
        for e in self.rx.drain(..) {
            if e.at <= tau {
                due.push(e);
            } else {
                keep.push(e);
            }
        }
        self.rx = keep;
        due.sort_by_key(|e| e.at);
        for e in due {
            let meta = RxMeta {
                harvest: MachTicks(tau),
                hw_ts: e.hw,
                clock_gen: e.gen.unwrap_or(self.gen),
                step_ambig: e.ambig.unwrap_or(ambig),
                desc_vlan_tci: None,
                loopback: false,
            };
            let _ = self.port.handle_rx(now, &e.frame, meta);
        }
        if let Some((tag, raw, at)) = self.ts {
            if at <= tau {
                self.ts = None;
                let r = if self.tx_ts_ok {
                    TxTsResult::Ok(raw)
                } else {
                    TxTsResult::Timeout
                };
                self.port.on_tx_timestamp(tag, r, MachTicks(tau));
            }
        }
        self.port.poll(now, &mut self.out);
        self.send();
        self.actions();
        if self.clock_reads && self.tau >= self.next_read {
            self.next_read = self.tau + 125 * MS;
            let mut reads = [crate::model::ClockRead::default(); 8];
            for (i, r) in reads.iter_mut().enumerate() {
                let mb = self.tau + i as u64 * 3000;
                let br = 1500 + (self.rng.next_u64() % 1000);
                let mid = mb as f64 + br as f64 / 2.0;
                *r = crate::model::ClockRead {
                    mach_before: mb,
                    systim_ns: self.systim_i(mid) as u64,
                    mach_after: mb + br,
                };
            }
            self.port.on_clock_read(MachTicks(self.tau), &reads);
        }
        self.record();
    }

    fn send(&mut self) {
        while let Some(f) = self.out.front().copied() {
            if f.want_ts && (self.ts.is_some() || !self.port.want_ts_allowed()) {
                break;
            }
            self.out.pop();
            let tau = self.tau;
            if !self.port.pre_send(f.tag, MachTicks(tau)) {
                continue;
            }
            self.port.on_frame_sent(f.tag, MachTicks(tau));
            let m = msg::decode_frame(f.bytes()).unwrap();
            let t = tau as f64;
            if f.want_ts {
                let raw = self.our_tx_ts(t);
                self.ts = Some((f.tag, raw, tau + self.tx_ts_delay));
            }
            if let Body::PdelayReq = m.body {
                if self.peer_responds {
                    let d = self.cfg.delay_ns;
                    let t2 = self.peer_ts(t + d);
                    let t3 = self.peer_ts(t + d + 24_000.0);
                    let t4tau = t + 2.0 * d + 24_000.0;
                    let t4 = self.our_rx_ts(t4tau);
                    let seq = m.header.sequence_id;
                    let me = self.port.identity();
                    let at = (t4tau + self.cfg.harvest_ns as f64) as u64;
                    self.inject(at, pdelay_resp_frame(seq, t2, me, PEER_PORT), Some(t4));
                    self.inject(
                        at + MS / 2,
                        pdelay_resp_fu_frame(seq, t3, me, PEER_PORT),
                        None,
                    );
                    if self.peer_multi {
                        let other = PortIdentity {
                            clock: OTHER_GM,
                            port: 1,
                        };
                        self.inject(at, pdelay_resp_frame(seq, t2, me, other), Some(t4 + 50));
                    }
                }
            }
            self.sent.push(Sent {
                tau,
                t: m.header.message_type,
                seq: m.header.sequence_id,
                bytes: f.bytes().to_vec(),
            });
        }
    }

    fn actions(&mut self) {
        while let Some(a) = self.port.take_action() {
            match a {
                Action::AdjFreq { ppb } => {
                    assert!(ppb.unsigned_abs() <= 488_000, "{ppb}");
                    self.rebase_s();
                    self.f_ppb = ppb as f64;
                }
                Action::StepClock { delta_ns } | Action::SetClock { delta_ns } => {
                    if matches!(a, Action::StepClock { .. }) {
                        assert!(delta_ns.unsigned_abs() <= 1_000_000_000);
                    }
                    assert!(
                        self.ts.is_none(),
                        "step issued with a TX timestamp outstanding"
                    );
                    if std::mem::take(&mut self.inject_pre_step) {
                        self.pre_step_frames();
                    }
                    self.rebase_s();
                    self.s_ps += i128::from(delta_ns + self.cfg.step_residual_ns) * 1000;
                    self.gen = self.gen.wrapping_add(1);
                    self.step_tau = self.tau;
                    self.ambig_pending = true;
                    self.extra_delay = self.delay_tick_after_step;
                    self.steps.push((self.tau, delta_ns));
                    self.port.on_clock_stepped(MachTicks(self.tau), self.gen);
                }
                Action::AdvertiseAdp { gm } => self.adp.push(gm),
            }
        }
    }

    /// A Sync/Follow_Up and a 16R Pdelay_Req received 100 us before the
    /// SYSTIM write: old-timebase timestamps, harvested by the next tick.
    fn pre_step_frames(&mut self) {
        let t = self.tau as f64 - 100_000.0;
        let d = self.cfg.delay_ns;
        let hw = self.our_rx_ts(t);
        let pot = self.gm_time(t - d) as u64;
        let at = self.tau + 1;
        self.inject(at, sync_frame(0xF001, self.sync_log, 0), Some(hw));
        self.inject(at, follow_up_frame(0xF001, self.sync_log, pot, 0, 0), None);
        let hw2 = self.our_rx_ts(t);
        self.peer_req_seq = self.peer_req_seq.wrapping_add(1);
        self.pre_step_req_seq = Some(self.peer_req_seq);
        self.inject(at, pdelay_req_frame(self.peer_req_seq), Some(hw2));
    }

    fn record(&mut self) {
        let used = self.port.counters().syncs_used;
        if used != self.last_used {
            self.last_used = used;
            if let Some(o) = self.port.last_offset() {
                self.offsets.push(OffsetRec {
                    tau: self.tau,
                    offset: o,
                });
            }
        }
        if self.lock_tau.is_none() && self.port.servo_state() == ServoState::Locked {
            self.lock_tau = Some(self.tau);
        }
    }

    pub fn run_for(&mut self, ns: u64) {
        let end = self.tau + ns;
        while self.tau < end {
            self.tick();
        }
    }

    /// Run until `f` holds or `ns` elapse; returns whether it held.
    pub fn run_until(&mut self, ns: u64, mut f: impl FnMut(&Sim) -> bool) -> bool {
        let end = self.tau + ns;
        while self.tau < end {
            self.tick();
            if f(self) {
                return true;
            }
        }
        false
    }

    pub fn sent_of(&self, t: MessageType) -> impl Iterator<Item = &Sent> {
        self.sent.iter().filter(move |s| s.t == t)
    }
}

/// mean, sd of offsets recorded at or after `from`.
pub fn stats(recs: &[OffsetRec], from: u64) -> (f64, f64, usize) {
    let v: Vec<f64> = recs
        .iter()
        .filter(|r| r.tau >= from)
        .map(|r| r.offset)
        .collect();
    let n = v.len() as f64;
    let m = v.iter().sum::<f64>() / n;
    let sd = (v.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / n).sqrt();
    (m, sd, v.len())
}
