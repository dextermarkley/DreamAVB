//! The `DextPort` trait (design §14.2) abstracts the dext so the daemon logic
//! is testable without IOKit. `RealDext` (in `realdext.rs`) implements it over
//! `avb-dext`; `FakeDext` (here, test-only) uses heap rings, a scripted 16R
//! peer and a simulated clock.

use avb_core::MachTicks;

/// The result of a `SendFrame` attempt (design §5.2 / §13.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendResult {
    Ok,
    /// `kIOReturnBusy`: a timestamp is pending; retry next loop.
    Busy,
    /// `kIOReturnNoResources`: token bucket empty; retry.
    NoResources,
    /// `kIOReturnBadArgument`: bad length (a bug); drop the frame.
    BadArgument,
    /// NotReady / IOError / device gone: the caller checks fault and exits.
    Fatal,
}

/// A TX hardware-timestamp poll result (design §5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxTsResult {
    /// Timestamp ready: (tag, systim_ns).
    Ready { tag: u64, systim_ns: u64 },
    /// Still pending.
    Pending,
    /// Timed out; the exchange is abandoned.
    Timeout,
    /// No timestamp outstanding.
    Idle,
}

/// A received RX slot handed to the daemon (control or audio).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RxSlot {
    pub cls: u8,
    pub data: Vec<u8>,
    pub hw_ts_ns: Option<u64>,
    pub clock_gen: u32,
    pub step_ambig: bool,
    pub desc_vlan_tci: Option<u16>,
}

/// Health snapshot from the dext header (design §5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Health {
    pub fault: u32,
    pub link_up: bool,
    pub stage: u32,
    pub producer_running: bool,
    pub last_tick_mach: MachTicks,
}

/// The dext interface the daemon drives (design §14.2). Sans-IO callers get a
/// `FakeDext`; production gets `RealDext`.
pub trait DextPort {
    fn send(&mut self, frame: &[u8], want_ts: bool, tag: u64) -> SendResult;
    fn poll_tx_ts(&mut self) -> TxTsResult;
    /// Current clock reading (mach ticks and SYSTIM ns).
    fn read_clock(&mut self) -> (MachTicks, u64);
    /// Bounded frequency/offset step via the NIC's STEP register (small
    /// corrections; the dext rejects a step larger than STEP_CLOCK_MAX_NS).
    fn step(&mut self, delta_ns: i64);
    /// Absolute SYSTIM write (design §7.3 SetClock, used when |offset| > 1 s).
    fn set_clock_abs(&mut self, ns: u64);
    fn adj_freq(&mut self, ppb: i64);
    fn set_filter(&mut self, streams: &[(u64, [u8; 6], u16)]);
    /// Drain the control ring.
    fn rx_ctl(&mut self) -> Vec<RxSlot>;
    /// Drain the audio ring.
    fn rx_audio(&mut self) -> Vec<RxSlot>;
    fn health(&self) -> Health;
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::collections::VecDeque;

    /// A scripted peer action the `FakeDext` performs when the daemon sends a
    /// frame or on a tick (test-only).
    #[derive(Clone, Debug)]
    pub enum PeerScript {
        /// Deliver this control frame on the next `rx_ctl`.
        Ctl(RxSlot),
        /// Deliver this audio frame on the next `rx_audio`.
        Audio(RxSlot),
    }

    /// A heap-backed dext double with a scripted 16R peer and a simulated
    /// clock (design §14.2). No IOKit, no real time.
    pub struct FakeDext {
        pub now: MachTicks,
        pub systim_ns: u64,
        pub health: Health,
        pub sent: Vec<(Vec<u8>, bool, u64)>,
        /// Frames the daemon will receive, drained by rx_ctl/rx_audio.
        ctl_in: VecDeque<RxSlot>,
        audio_in: VecDeque<RxSlot>,
        /// If set, the next `send` returns this instead of Ok (then clears).
        pub next_send: Option<SendResult>,
        /// Pending TX timestamp to hand back (tag, systim_ns).
        pending_ts: Option<(u64, u64)>,
        /// When true, poll_tx_ts returns Pending instead of resolving.
        pub ts_stays_pending: bool,
        pub steps: Vec<i64>,
        pub set_clocks: Vec<u64>,
        pub freq_adjusts: Vec<i64>,
        pub filters_set: usize,
    }

    impl Default for FakeDext {
        fn default() -> Self {
            Self {
                now: MachTicks(0),
                systim_ns: 0,
                health: Health {
                    fault: 0,
                    link_up: true,
                    stage: 2,
                    producer_running: true,
                    last_tick_mach: MachTicks(0),
                },
                sent: Vec::new(),
                ctl_in: VecDeque::new(),
                audio_in: VecDeque::new(),
                next_send: None,
                pending_ts: None,
                ts_stays_pending: false,
                steps: Vec::new(),
                set_clocks: Vec::new(),
                freq_adjusts: Vec::new(),
                filters_set: 0,
            }
        }
    }

    impl FakeDext {
        pub fn push_ctl(&mut self, slot: RxSlot) {
            self.ctl_in.push_back(slot);
        }
        pub fn push_audio(&mut self, slot: RxSlot) {
            self.audio_in.push_back(slot);
        }
        pub fn apply(&mut self, script: PeerScript) {
            match script {
                PeerScript::Ctl(s) => self.ctl_in.push_back(s),
                PeerScript::Audio(s) => self.audio_in.push_back(s),
            }
        }
        pub fn advance_ns(&mut self, ns: u64) {
            // 125/3 timebase: ticks = ns * 3 / 125.
            self.now = MachTicks(self.now.0 + ns * 3 / 125);
            self.systim_ns += ns;
            self.health.last_tick_mach = self.now;
        }
    }

    impl DextPort for FakeDext {
        fn send(&mut self, frame: &[u8], want_ts: bool, tag: u64) -> SendResult {
            if let Some(r) = self.next_send.take() {
                return r;
            }
            self.sent.push((frame.to_vec(), want_ts, tag));
            if want_ts {
                self.pending_ts = Some((tag, self.systim_ns));
            }
            SendResult::Ok
        }
        fn poll_tx_ts(&mut self) -> TxTsResult {
            if self.ts_stays_pending && self.pending_ts.is_some() {
                return TxTsResult::Pending;
            }
            match self.pending_ts.take() {
                Some((tag, systim_ns)) => TxTsResult::Ready { tag, systim_ns },
                None => TxTsResult::Idle,
            }
        }
        fn read_clock(&mut self) -> (MachTicks, u64) {
            (self.now, self.systim_ns)
        }
        fn step(&mut self, delta_ns: i64) {
            self.steps.push(delta_ns);
            self.systim_ns = self.systim_ns.wrapping_add_signed(delta_ns);
        }
        fn set_clock_abs(&mut self, ns: u64) {
            self.set_clocks.push(ns);
            self.systim_ns = ns;
        }
        fn adj_freq(&mut self, ppb: i64) {
            self.freq_adjusts.push(ppb);
        }
        fn set_filter(&mut self, _streams: &[(u64, [u8; 6], u16)]) {
            self.filters_set += 1;
        }
        fn rx_ctl(&mut self) -> Vec<RxSlot> {
            self.ctl_in.drain(..).collect()
        }
        fn rx_audio(&mut self) -> Vec<RxSlot> {
            self.audio_in.drain(..).collect()
        }
        fn health(&self) -> Health {
            self.health
        }
    }
}
