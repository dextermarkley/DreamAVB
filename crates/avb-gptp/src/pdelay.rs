//! Peer delay (design §7.2): NRR window, link delay, the initiator and
//! responder exchange state, and the asCapable conditions.
//!
//! Time units: our timestamps are SYSTIM ns (`u64`); the peer's t2/t3 are
//! kept in 2^-16 ns ("scaled ns", `i128`) so correctionFields add exactly.

use avb_core::MachTicks;

use crate::msg::PortIdentity;
use crate::status::{GptpCounters, Window};
use crate::stepgen::RxGen;
use crate::LatencyProfile;

/// NRR window length k (design §7.2).
pub const NRR_WINDOW: usize = 8;
/// |NRR − 1| above this is rejected (`nrr_out_of_range`).
pub const NRR_MAX_DEV: f64 = 1e-3;
/// neighborPropDelayThresh, ns (802.1AS-2020 §11.2.13.? default 800 ns *(verify)*).
pub const NEIGHBOR_PROP_DELAY_THRESH_NS: f64 = 800.0;
/// allowedLostResponses (802.1AS-2020 §11.5.3 default 3 *(verify)*).
pub const ALLOWED_LOST_RESPONSES: u32 = 3;
/// Exchanges looked back for the multiple-responder condition.
pub const MULTI_RESPONDER_LOOKBACK: usize = 3;
/// Compensated delays averaged for the delay the port uses.
pub const DELAY_AVG: usize = 8;
/// gPtpCapableReceiptTimeout (802.1AS-2020 §10.7.3.? default 9 *(verify)*).
pub const GPTP_CAPABLE_RECEIPT_TIMEOUT: u32 = 9;

const SCALE: f64 = 65536.0;

pub(crate) fn scaled(ns: u64) -> i128 {
    i128::from(ns) << 16
}

/// One complete exchange's NRR inputs: peer t3 (scaled ns), our t4 (ns).
#[derive(Clone, Copy, Debug, PartialEq)]
struct NrrPoint {
    t3: i128,
    t4: u64,
}

/// NRR window over complete exchanges of the current step generation, with
/// the previous generation's NRR held until 2 new exchanges exist.
#[derive(Clone, Debug, Default)]
pub struct NrrWindow {
    pts: std::collections::VecDeque<NrrPoint>,
    held: Option<f64>,
}

/// Result of offering an exchange to the window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NrrAdd {
    Added,
    /// The candidate NRR deviated by more than 1000 ppm; the exchange was not added.
    Rejected(f64),
}

impl NrrWindow {
    /// Offer a complete exchange.
    pub fn add(&mut self, t3_scaled: i128, t4: u64) -> NrrAdd {
        let p = NrrPoint { t3: t3_scaled, t4 };
        if let Some(old) = self.pts.front() {
            if let Some(r) = ratio(*old, p) {
                if (r - 1.0).abs() > NRR_MAX_DEV {
                    return NrrAdd::Rejected(r);
                }
            } else {
                return NrrAdd::Rejected(f64::NAN);
            }
        }
        if self.pts.len() >= NRR_WINDOW {
            self.pts.pop_front();
        }
        self.pts.push_back(p);
        if self.pts.len() >= 2 {
            self.held = None;
        }
        NrrAdd::Added
    }

    /// The window's own NRR (≥ 2 exchanges), else the held one.
    pub fn nrr(&self) -> Option<f64> {
        self.window_nrr().or(self.held)
    }

    fn window_nrr(&self) -> Option<f64> {
        if self.pts.len() < 2 {
            return None;
        }
        ratio(*self.pts.front()?, *self.pts.back()?)
    }

    pub fn valid(&self) -> bool {
        self.nrr().is_some()
    }

    pub fn held(&self) -> Option<f64> {
        self.held
    }

    pub fn len(&self) -> usize {
        self.pts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pts.is_empty()
    }

    /// Step generation changed: hold the last valid NRR, clear the window.
    pub fn on_gen_change(&mut self) {
        if let Some(r) = self.window_nrr() {
            self.held = Some(r);
        }
        self.pts.clear();
    }
}

fn ratio(old: NrrPoint, new: NrrPoint) -> Option<f64> {
    let d4 = i128::from(new.t4) - i128::from(old.t4);
    if d4 <= 0 {
        return None;
    }
    let d3 = (new.t3 - old.t3) as f64 / SCALE;
    Some(d3 / d4 as f64)
}

/// `((t4 − t1)·NRR − (t3 − t2)) / 2` in ns; t1/t4 ours (ns), t2/t3 the
/// peer's (scaled ns).
pub fn link_delay(t1: u64, t2_scaled: i128, t3_scaled: i128, t4: u64, nrr: f64) -> f64 {
    let local = (i128::from(t4) - i128::from(t1)) as f64;
    let remote = (t3_scaled - t2_scaled) as f64 / SCALE;
    (local * nrr - remote) / 2.0
}

/// Our side of one Pdelay exchange (initiator).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Exchange {
    pub seq: u16,
    pub tag: u64,
    /// Raw TX timestamp and the generation of its SendFrame call.
    pub t1: Option<(u64, u32)>,
    pub t2: Option<i128>,
    /// Pdelay_Resp correctionField (scaled ns), folded into t3.
    pub resp_corr: i64,
    pub t3: Option<i128>,
    /// Raw RX timestamp and its generation.
    pub t4: Option<(u64, RxGen)>,
    pub responder: Option<PortIdentity>,
    pub responses: u32,
    /// Ambiguous / straddling: dropped, not lost.
    pub discarded: bool,
    pub done: bool,
}

impl Exchange {
    pub fn new(seq: u16, tag: u64) -> Self {
        Self {
            seq,
            tag,
            t1: None,
            t2: None,
            resp_corr: 0,
            t3: None,
            t4: None,
            responder: None,
            responses: 0,
            discarded: false,
            done: false,
        }
    }

    pub fn multiple(&self) -> bool {
        self.responses > 1
    }
}

/// A complete exchange's results.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExchangeResult {
    pub delay_raw: f64,
    pub delay_comp: f64,
    pub nrr: Option<f64>,
}

/// Initiator state shared by the port: window, delays, losses.
#[derive(Clone, Debug)]
pub struct PdelayInitiator {
    pub(crate) cur: Option<Exchange>,
    pub nrr: NrrWindow,
    pub delay_raw: Window,
    pub delay_comp: Window,
    /// Compensated delays averaged for use (last [`DELAY_AVG`]).
    delay_use: Window,
    pub lost_since_good: u32,
    /// Multiple-responder flags of the last exchanges, newest last.
    multi: std::collections::VecDeque<bool>,
    pub seq: u16,
}

impl Default for PdelayInitiator {
    fn default() -> Self {
        Self {
            cur: None,
            nrr: NrrWindow::default(),
            delay_raw: Window::new(64),
            delay_comp: Window::new(64),
            delay_use: Window::new(DELAY_AVG),
            lost_since_good: 0,
            multi: std::collections::VecDeque::new(),
            seq: 0,
        }
    }
}

impl PdelayInitiator {
    /// Close the previous exchange before starting a new one: incomplete and
    /// not discarded → lost.
    pub(crate) fn finish_previous(&mut self, c: &mut GptpCounters) {
        if let Some(x) = self.cur.take() {
            if !x.done && !x.discarded {
                self.lost_since_good = self.lost_since_good.saturating_add(1);
                c.lost_responses += 1;
            }
            if self.multi.len() >= MULTI_RESPONDER_LOOKBACK {
                self.multi.pop_front();
            }
            self.multi.push_back(x.multiple());
        }
    }

    /// No exchange in the last 3 had more than one Pdelay_Resp (the current
    /// one included once it has seen one).
    pub fn multi_responder(&self) -> bool {
        self.multi.iter().any(|&m| m) || self.cur.is_some_and(|x| x.multiple())
    }

    /// The compensated delay the port uses (mean of the last 8).
    pub fn delay(&self) -> Option<f64> {
        self.delay_use.mean()
    }

    /// Try to complete the current exchange in generation `gen`.
    pub(crate) fn try_complete(
        &mut self,
        gen: u32,
        lat: LatencyProfile,
        c: &mut GptpCounters,
    ) -> Option<ExchangeResult> {
        let x = self.cur.as_mut()?;
        if x.done || x.discarded {
            return None;
        }
        let ((t1, g1), t2, t3, (t4, g4)) = (x.t1?, x.t2?, x.t3?, x.t4?);
        // Straddling a step, ambiguous t4, or an old generation: dropped.
        if g4 != RxGen::Gen(g1) || g1 != gen {
            x.discarded = true;
            c.pdelay_step_discard += 1;
            return None;
        }
        x.done = true;
        let t1c = lat.tx_c(t1);
        let t4c = lat.rx_c(t4);
        match self.nrr.add(t3, t4c) {
            NrrAdd::Added => {}
            NrrAdd::Rejected(_) => c.nrr_out_of_range += 1,
        }
        let r = self.nrr.nrr();
        let n = r.unwrap_or(1.0);
        let delay_raw = link_delay(t1, t2, t3, t4, n);
        let delay_comp = link_delay(t1c, t2, t3, t4c, n);
        self.delay_raw.push(delay_raw);
        self.delay_comp.push(delay_comp);
        self.delay_use.push(delay_comp);
        self.lost_since_good = 0;
        Some(ExchangeResult {
            delay_raw,
            delay_comp,
            nrr: r,
        })
    }
}

/// Inputs of the asCapable rule (design §7.2, PTP-5).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AsCapableInputs {
    pub delay_comp: Option<f64>,
    pub lost_since_good: u32,
    pub multi_responder: bool,
    pub nrr_valid: bool,
    pub gptp_capable_expired: bool,
}

impl AsCapableInputs {
    /// The first failing condition, or `None` if asCapable.
    pub fn failing(&self) -> Option<&'static str> {
        match self.delay_comp {
            None => return Some("no_delay"),
            Some(d) if d > NEIGHBOR_PROP_DELAY_THRESH_NS => return Some("delay_above_thresh"),
            Some(_) => {}
        }
        if self.lost_since_good > ALLOWED_LOST_RESPONSES {
            return Some("lost_responses");
        }
        if self.multi_responder {
            return Some("multiple_responders");
        }
        if !self.nrr_valid {
            return Some("nrr_invalid");
        }
        if self.gptp_capable_expired {
            return Some("gptp_capable_timeout");
        }
        None
    }

    pub fn as_capable(&self) -> bool {
        self.failing().is_none()
    }
}

/// Our response to one of the peer's Pdelay_Req (responder).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Response {
    pub seq: u16,
    pub requesting: PortIdentity,
    pub harvest: MachTicks,
    /// t2 raw and its generation.
    pub t2: u64,
    pub t2_gen: RxGen,
    pub tag: u64,
    pub stage: RespStage,
    /// Generation of the Pdelay_Resp SendFrame call.
    pub sent_gen: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RespStage {
    /// Pdelay_Resp not pushed yet (waiting behind a pending step).
    Waiting,
    /// In the outbox or sent; waiting for t3.
    InFlight,
    /// t3 known: Pdelay_Resp_Follow_Up to push with this t3 (raw).
    FollowUp(u64),
    Done,
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAT: LatencyProfile = LatencyProfile::I210_1000;

    #[test]
    fn link_delay_formula() {
        // Peer clock offset +5 s, turnaround 10 us, 300 ns each way, NRR 1.
        let t1 = 1_000_000_000u64;
        let t2 = scaled(t1 + 300 + 5_000_000_000);
        let t3 = t2 + scaled(10_000);
        let t4 = t1 + 300 + 10_000 + 300;
        assert_eq!(link_delay(t1, t2, t3, t4, 1.0), 300.0);
        // A correctionField of +100 ns folded into t3 shortens it by 50 ns.
        assert_eq!(link_delay(t1, t2, t3 + (100 << 16), t4, 1.0), 250.0);
    }

    /// Phase 0b G4 as a vector: five exchanges with raw delays whose mean is
    /// 455.6 ns compensate (TX +178, RX −448) to a mean of 142.6 ns.
    #[test]
    fn phase0b_delay_vector() {
        let mut ini = PdelayInitiator::default();
        let mut c = GptpCounters::default();
        let raws = [450u64, 455, 455, 456, 462];
        for (i, raw) in raws.iter().enumerate() {
            // t4 and t3 advance by exactly 1 s, so NRR is exactly 1.
            let turn = 24_000;
            let t4 = 10_000_000_000 + i as u64 * 1_000_000_000 + turn;
            let t1 = t4 - 2 * raw - turn;
            let t3 = scaled(t4 + 7_000_000_000);
            let t2 = t3 - scaled(turn);
            let mut x = Exchange::new(i as u16, 0);
            x.t1 = Some((t1, 0));
            x.t2 = Some(t2);
            x.t3 = Some(t3);
            x.t4 = Some((t4, RxGen::Gen(0)));
            ini.cur = Some(x);
            let r = ini.try_complete(0, LAT, &mut c).unwrap();
            assert!((r.delay_raw - *raw as f64).abs() < 1e-6, "{r:?}");
            assert!((r.delay_comp - (*raw as f64 - 313.0)).abs() < 1e-6);
            ini.finish_previous(&mut c);
        }
        assert!((ini.delay_raw.mean().unwrap() - 455.6).abs() < 1e-9);
        assert!((ini.delay_comp.mean().unwrap() - 142.6).abs() < 1e-9);
        assert!((ini.delay().unwrap() - 142.6).abs() < 1e-9);
        assert_eq!(c.lost_responses, 0);
        assert!((ini.nrr.nrr().unwrap() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn nrr_window_hold_and_reject() {
        let mut w = NrrWindow::default();
        assert!(!w.valid());
        let r = 1.0 - 83.18e-6;
        let pt = |n: u64, off: i128| {
            (
                scaled(5_000_000_000) + ((n as f64 * 1e9 * r * SCALE) as i128) + off,
                1_000 + n * 1_000_000_000,
            )
        };
        let (a, b) = pt(0, 0);
        assert_eq!(w.add(a, b), NrrAdd::Added);
        assert!(!w.valid(), "one exchange is not enough");
        for n in 1..12 {
            let (a, b) = pt(n, 0);
            assert_eq!(w.add(a, b), NrrAdd::Added);
        }
        assert_eq!(w.len(), NRR_WINDOW);
        let full = w.nrr().unwrap();
        assert!((full - r).abs() < 1e-12);
        // 1 + 1.5e-3 candidate rejected, not added.
        let (_, b) = pt(12, 0);
        let bad = w.pts.front().unwrap().t3
            + ((b - w.pts.front().unwrap().t4) as f64 * (1.0 + 1.5e-3) * SCALE) as i128;
        assert!(matches!(w.add(bad, b), NrrAdd::Rejected(x) if (x - 1.0015).abs() < 1e-9));
        assert_eq!(w.len(), NRR_WINDOW);
        // Generation change: held until two new exchanges.
        w.on_gen_change();
        assert!(w.is_empty());
        assert_eq!(w.nrr(), Some(full));
        let (a, b) = pt(20, scaled(123_456));
        w.add(a, b + 100_000);
        assert_eq!(w.nrr(), Some(full));
        assert_eq!(w.held(), Some(full));
        let (a, b) = pt(21, scaled(123_456));
        w.add(a, b + 100_000);
        assert_eq!(w.held(), None);
        assert!((w.nrr().unwrap() - r).abs() < 1e-9);
        // A change with < 2 exchanges keeps the old hold.
        let mut w2 = NrrWindow::default();
        w2.add(0, 1);
        w2.on_gen_change();
        assert_eq!(w2.nrr(), None);
    }

    #[test]
    fn exchange_straddling_step_is_discarded_not_lost() {
        let mut ini = PdelayInitiator::default();
        let mut c = GptpCounters::default();
        let mut x = Exchange::new(1, 0);
        x.t1 = Some((1000, 0));
        x.t2 = Some(0);
        x.t3 = Some(scaled(10));
        x.t4 = Some((3000, RxGen::Gen(1)));
        ini.cur = Some(x);
        assert!(ini.try_complete(1, LAT, &mut c).is_none());
        assert_eq!(c.pdelay_step_discard, 1);
        ini.finish_previous(&mut c);
        assert_eq!(c.lost_responses, 0);
        // Ambiguous t4.
        let mut x = Exchange::new(2, 0);
        x.t1 = Some((1000, 1));
        x.t2 = Some(0);
        x.t3 = Some(scaled(10));
        x.t4 = Some((3000, RxGen::Ambiguous));
        ini.cur = Some(x);
        assert!(ini.try_complete(1, LAT, &mut c).is_none());
        ini.finish_previous(&mut c);
        assert_eq!((c.pdelay_step_discard, c.lost_responses), (2, 0));
        // Incomplete → lost.
        ini.cur = Some(Exchange::new(3, 0));
        ini.finish_previous(&mut c);
        assert_eq!((c.lost_responses, ini.lost_since_good), (1, 1));
    }

    #[test]
    fn as_capable_conditions() {
        let ok = AsCapableInputs {
            delay_comp: Some(142.6),
            lost_since_good: 3,
            multi_responder: false,
            nrr_valid: true,
            gptp_capable_expired: false,
        };
        assert!(ok.as_capable());
        let cases = [
            (
                AsCapableInputs {
                    delay_comp: None,
                    ..ok
                },
                "no_delay",
            ),
            (
                AsCapableInputs {
                    delay_comp: Some(800.1),
                    ..ok
                },
                "delay_above_thresh",
            ),
            (
                AsCapableInputs {
                    lost_since_good: 4,
                    ..ok
                },
                "lost_responses",
            ),
            (
                AsCapableInputs {
                    multi_responder: true,
                    ..ok
                },
                "multiple_responders",
            ),
            (
                AsCapableInputs {
                    nrr_valid: false,
                    ..ok
                },
                "nrr_invalid",
            ),
            (
                AsCapableInputs {
                    gptp_capable_expired: true,
                    ..ok
                },
                "gptp_capable_timeout",
            ),
        ];
        for (i, why) in cases {
            assert_eq!(i.failing(), Some(why));
        }
        assert!(AsCapableInputs {
            delay_comp: Some(800.0),
            ..ok
        }
        .as_capable());
    }

    #[test]
    fn multi_responder_lookback() {
        let mut ini = PdelayInitiator::default();
        let mut c = GptpCounters::default();
        let mut x = Exchange::new(1, 0);
        x.responses = 2;
        ini.cur = Some(x);
        assert!(ini.multi_responder());
        ini.finish_previous(&mut c);
        for s in 2..4 {
            let mut x = Exchange::new(s, 0);
            x.responses = 1;
            ini.cur = Some(x);
            ini.finish_previous(&mut c);
            assert!(ini.multi_responder(), "still within the last 3");
        }
        ini.cur = Some(Exchange::new(4, 0));
        ini.finish_previous(&mut c);
        assert!(!ini.multi_responder());
    }
}
