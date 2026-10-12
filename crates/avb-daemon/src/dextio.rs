// SPDX-License-Identifier: MIT OR Apache-2.0

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

/// What one [`DextPort::send_batch`] call achieved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchResult {
    /// Frames that reached the TX ring. The dext queues in order, so this is
    /// a PREFIX of the batch; the rest were refused.
    pub accepted: usize,
    /// Why the prefix stopped, where the path can tell.
    ///
    /// The batched selector reports only a count — the dext splits the causes
    /// in its own counters — so a short batch there is `None`. The per-frame
    /// fallback knows the exact `SendResult` and reports it, which is what
    /// keeps a real error (a bad length, a device fault) distinguishable from
    /// ordinary back-pressure instead of all of it reading as "rate-limited".
    pub stopped_by: Option<SendResult>,
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

    /// Queue several untimestamped frames in ONE user-client call, returning
    /// how many reached the ring.
    ///
    /// This exists because the per-CALL cost dominates: one `SendFrame` round
    /// trip measures ~28 us, so eight class-A streams at 8000 frames/s each
    /// cannot be fed one call at a time. A short return is ordinary
    /// back-pressure — the dext queues in order and stops at the first frame
    /// the token bucket or TX ring refuses — so the caller re-paces the rest
    /// rather than treating it as an error.
    ///
    /// The default implementation loops over [`DextPort::send`], which is what
    /// an ABI v3 dext (no `SEND_FRAMES` selector) gets. It is correct but
    /// pays the per-call cost, so it cannot sustain the full width.
    fn send_batch(&mut self, frames: &[&[u8]]) -> BatchResult {
        let mut accepted = 0;
        for f in frames {
            // Stop at the first refusal so the caller sees a prefix, the same
            // shape the batched selector returns.
            match self.send(f, false, 0) {
                SendResult::Ok => accepted += 1,
                other => {
                    return BatchResult {
                        accepted,
                        stopped_by: Some(other),
                    }
                }
            }
        }
        BatchResult {
            accepted,
            stopped_by: None,
        }
    }

    /// Whether [`DextPort::send_batch`] is a single call rather than a loop.
    /// Reported so a summary can say which path a run actually used.
    fn batched_send(&self) -> bool {
        false
    }

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
        /// Per-send scripted results, consumed one per `send` while non-empty.
        ///
        /// An `Ok` entry falls through to the normal record-and-accept path,
        /// so a script like `[Ok, NoResources]` refuses the SECOND frame —
        /// which is the only way to exercise a batch that stops partway
        /// rather than at its first frame.
        pub send_script: VecDeque<SendResult>,
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
                send_script: VecDeque::new(),
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
            if let Some(r) = self.send_script.pop_front() {
                if r != SendResult::Ok {
                    return r;
                }
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

#[cfg(test)]
mod tests {
    use super::fake::FakeDext;
    use super::*;

    /// The default `send_batch` is the path an ABI v3 dext gets — the one
    /// currently running in the kernel — so its semantics have to match the
    /// batched selector's, or a fallback run would behave differently from a
    /// batched one in ways the summary could not show.
    #[test]
    fn fallback_batch_queues_every_frame_untimestamped() {
        let mut d = FakeDext::default();
        let a = [1u8; 64];
        let b = [2u8; 64];
        let c = [3u8; 64];

        let r = d.send_batch(&[&a[..], &b[..], &c[..]]);

        assert_eq!(
            r,
            BatchResult {
                accepted: 3,
                stopped_by: None
            }
        );
        assert_eq!(d.sent.len(), 3);
        // Order is the batch order: an AAF stream whose frames arrive out of
        // sequence is a listener-side error, not a reordering the dext fixes.
        assert_eq!(d.sent[0].0[0], 1);
        assert_eq!(d.sent[1].0[0], 2);
        assert_eq!(d.sent[2].0[0], 3);
        // No timestamps: a batch is audio, and the dext has one pending TS
        // slot, so requesting one per frame would serialise the whole batch.
        assert!(d
            .sent
            .iter()
            .all(|(_, want_ts, tag)| !*want_ts && *tag == 0));
    }

    /// A refusal must STOP the batch, not skip the refused frame and send the
    /// rest. The dext queues in order and stops at the first frame the token
    /// bucket refuses, so a fallback that carried on would put a later frame
    /// on the wire ahead of an earlier one and silently reorder the stream.
    #[test]
    fn fallback_batch_stops_at_a_refusal_rather_than_skipping_it() {
        let mut d = FakeDext::default();
        d.send_script
            .extend([SendResult::Ok, SendResult::NoResources, SendResult::Ok]);
        let a = [1u8; 64];
        let b = [2u8; 64];
        let c = [3u8; 64];

        let r = d.send_batch(&[&a[..], &b[..], &c[..]]);

        assert_eq!(
            r,
            BatchResult {
                accepted: 1,
                stopped_by: Some(SendResult::NoResources)
            }
        );
        // Frame 3 must NOT have been sent, even though its scripted result
        // was Ok — the caller re-paces the remainder in order.
        assert_eq!(d.sent.len(), 1);
        assert_eq!(d.sent[0].0[0], 1);
    }

    /// Back-pressure and a real error must stay distinguishable: folding
    /// every short batch into one "rate-limited" count is how a device fault
    /// or a bad frame length would read as ordinary pacing.
    #[test]
    fn fallback_batch_names_the_cause_it_stopped_on() {
        for cause in [
            SendResult::Busy,
            SendResult::NoResources,
            SendResult::BadArgument,
            SendResult::Fatal,
        ] {
            let mut d = FakeDext::default();
            d.next_send = Some(cause);
            let f = [0u8; 64];

            let r = d.send_batch(&[&f[..], &f[..]]);

            assert_eq!(
                r,
                BatchResult {
                    accepted: 0,
                    stopped_by: Some(cause)
                },
                "cause {cause:?} must be reported as itself"
            );
            assert!(d.sent.is_empty());
        }
    }

    /// An empty batch is a no-op, not a call. The talker stages frames per
    /// interval and an interval with nothing ready must not reach the dext.
    #[test]
    fn fallback_batch_of_nothing_makes_no_call() {
        let mut d = FakeDext::default();

        let r = d.send_batch(&[]);

        assert_eq!(
            r,
            BatchResult {
                accepted: 0,
                stopped_by: None
            }
        );
        assert!(d.sent.is_empty());
    }

    /// The default path must report itself as unbatched, so the run summary
    /// cannot show a batched run and a looping one identically — the whole
    /// point of the change is a rate only the real selector can reach.
    #[test]
    fn fallback_reports_that_it_is_not_batched() {
        assert!(!FakeDext::default().batched_send());
    }
}
