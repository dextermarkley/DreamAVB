// SPDX-License-Identifier: MIT OR Apache-2.0

//! One serviceable gPTP port (design §5.2), so more than one loop can run it.
//!
//! The `gptp` command's loop body used to be the only place that pumped the
//! sans-IO [`avb_gptp::Port`]: drain RX into it, harvest TX timestamps, send
//! what it asks for, execute its servo actions, feed it clock reads. The
//! talker needs exactly the same body, because the AVTP presentation
//! timestamp it stamps on every PDU is a reading of the DISCIPLINED clock —
//! and a loop that does not service gPTP has no disciplined clock to read.
//!
//! Before this existed the talker used nanoseconds-since-loop-start as a
//! stand-in for gPTP now. That is not an approximation of gPTP time, it is an
//! unrelated quantity: the 16R compares the timestamp against its own gPTP
//! clock, so a stream starting near zero is hours in the past and every PDU
//! is outside the acceptance window.
//!
//! [`GptpService::now_ns`] answers "what is gPTP time right now" without an
//! IPC per call. A class-A talker stamps 8000 PDUs/s; asking the dext for
//! SYSTIM that often would be 8000 extra round trips per second. Instead the
//! service keeps an anchor (mach ticks, SYSTIM ns) from its last clock read
//! and interpolates on the mach clock between re-anchors.

use avb_core::rx::{Now, RxMeta};
use avb_core::time::{MachTicks, Timebase};
use avb_core::{MacAddr, Outbox};
use avb_gptp::{Action, Port, PortConfig, ServoState, TxTsResult as GTxTs};

use crate::dextio::{DextPort, SendResult, TxTsResult};
use crate::exit::Exit;
use crate::log::{Level, Logger};
use crate::realdext::RealDext;

/// How often the clock model is fed, and the anchor re-read, by default.
pub const DEFAULT_CLOCK_READ_NS: u64 = 100_000_000;
/// A tighter cadence for the talker: the interpolation error between
/// re-anchors is the residual frequency offset times this interval, so 20 ms
/// at a post-lock residual of a few ppm is tens of nanoseconds.
pub const TALKER_CLOCK_READ_NS: u64 = 20_000_000;
// The talker must re-anchor more often than the gptp command: its reading
// becomes a presentation timestamp, where the interpolation error between
// anchors shows up directly in the listener's playout schedule.
const _: () = assert!(TALKER_CLOCK_READ_NS < DEFAULT_CLOCK_READ_NS);

/// A reading of the disciplined clock, used to answer "gPTP time now" without
/// an IPC per call.
///
/// Pure arithmetic, kept separate from the service so it can be tested: this
/// is the kind of time math that silently yields plausible-but-wrong values.
/// The rule is that interpolation only ever moves FORWARD from the anchor by
/// elapsed mach time — it never recomputes the whole timeline, which is how a
/// rate change rewrites the past.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockAnchor {
    /// Mach ticks at the moment the clock was read.
    pub mach: MachTicks,
    /// SYSTIM (gPTP) nanoseconds at that same moment.
    pub systim_ns: u64,
}

impl ClockAnchor {
    /// gPTP ns at `now`, interpolated on the mach clock.
    ///
    /// `ticks_since` is saturating, so a `now` BEFORE the anchor yields the
    /// anchor's own time rather than a wrapped, far-future value. That matters
    /// because a presentation timestamp is truncated to 32 bits, where a wrap
    /// is indistinguishable from a valid time.
    pub fn interpolate(&self, now: MachTicks, tb: Timebase) -> u64 {
        self.systim_ns
            .saturating_add(tb.ticks_to_ns(now.ticks_since(self.mach)))
    }
}

/// A gPTP port plus the I/O plumbing one loop iteration needs.
pub struct GptpService {
    port: Port,
    tb: Timebase,
    out: Outbox<16>,
    /// A frame held because the dext returned Busy/NoResources this tick.
    held: Option<avb_core::TxFrame>,
    last_clock_read: MachTicks,
    clock_read_ticks: u64,
    /// The last successful clock reading. `None` until the first read, and
    /// cleared by a clock step — an anchor taken before a step is wrong by the
    /// step, and a wrong presentation timestamp is worse than none.
    anchor: Option<ClockAnchor>,
}

impl GptpService {
    /// Build the port from the dext's own identity and timebase.
    pub fn new(dext: &mut RealDext, log: &Logger, clock_read_ns: u64) -> Result<Self, Exit> {
        let info = dext.device().info().map_err(|_| Exit::DextFault)?;
        let mac = MacAddr::new(info.mac);
        let tb = Timebase {
            numer: info.tb_numer,
            denom: info.tb_denom,
        };
        if !tb.is_valid() {
            log.log(Level::Error, "gptp", "dext reported an invalid timebase");
            return Err(Exit::DextFault);
        }
        let now = MachTicks(unsafe { mach2::mach_time::mach_absolute_time() });
        // Seed the port with the dext's CURRENT clock generation. Left at 0
        // while the dext's is non-zero (it increments on each RingAttach and
        // SYSTIM step), every Pdelay exchange looks like it straddles a step
        // and is discarded.
        let clock_gen = dext.clock_gen();
        Ok(Self {
            port: Port::new(PortConfig::i210(mac, tb), now, clock_gen),
            tb,
            out: Outbox::default(),
            held: None,
            last_clock_read: now,
            clock_read_ticks: tb.ns_to_ticks(clock_read_ns),
            anchor: None,
        })
    }

    pub fn port(&self) -> &Port {
        &self.port
    }

    pub fn status(&self) -> avb_gptp::GptpStatus {
        self.port.status()
    }

    pub fn counters(&self) -> &avb_gptp::GptpCounters {
        self.port.counters()
    }

    /// Is the clock fit to timestamp a stream with? Both conditions matter:
    /// `as_capable` means the peer is exchanging gPTP with us, and a locked
    /// servo means SYSTIM actually tracks the grandmaster. A reading taken
    /// while the servo is still acquiring can be arbitrarily far out.
    pub fn is_locked(&self) -> bool {
        let st = self.port.status();
        st.as_capable && st.servo_state == ServoState::Locked
    }

    /// gPTP time now, in ns, interpolated from the last clock read.
    ///
    /// `None` when there is no valid anchor — before the first read, and
    /// immediately after a clock step until the next read lands. A caller that
    /// needs a presentation timestamp must skip rather than substitute
    /// something else.
    pub fn now_ns(&self, now_t: MachTicks) -> Option<u64> {
        Some(self.anchor?.interpolate(now_t, self.tb))
    }

    /// Run one iteration: RX in, TX timestamps harvested, frames out, servo
    /// actions executed, clock model fed. The caller owns the cadence.
    pub fn service(
        &mut self,
        dext: &mut RealDext,
        log: &Logger,
        now_t: MachTicks,
    ) -> Result<(), Exit> {
        let now = Now::mach_only(now_t);

        // 1) Drain RX control frames into the port.
        for slot in dext.rx_ctl() {
            let meta = RxMeta {
                harvest: now_t,
                hw_ts: slot.hw_ts_ns,
                clock_gen: slot.clock_gen,
                step_ambig: slot.step_ambig,
                desc_vlan_tci: slot.desc_vlan_tci,
                loopback: false,
            };
            let _ = self.port.handle_rx(now, &slot.data, meta);
        }

        self.service_no_rx(dext, log, now_t)
    }

    /// The same iteration WITHOUT draining the control ring.
    ///
    /// The control ring is a single shared queue: a slot read here is gone for
    /// everyone. A loop that must also see MSRP and AECP therefore drains it
    /// once itself and feeds gPTP frames in with [`GptpService::handle_rx`],
    /// then calls this. Two readers of one ring silently steal each other's
    /// frames, which looks exactly like a peer that stopped responding.
    pub fn service_no_rx(
        &mut self,
        dext: &mut RealDext,
        log: &Logger,
        now_t: MachTicks,
    ) -> Result<(), Exit> {
        // 2) Harvest a pending TX timestamp and route it to its exchange.
        match dext.poll_tx_ts() {
            TxTsResult::Ready { tag, systim_ns } => {
                self.port.on_tx_timestamp(tag, GTxTs::Ok(systim_ns), now_t);
            }
            TxTsResult::Timeout => { /* exchange abandoned; port times it out */ }
            TxTsResult::Pending | TxTsResult::Idle => {}
        }

        // 3) Poll the port for frames to send, draining any held frame first.
        let now = Now::mach_only(now_t);
        self.port.poll(now, &mut self.out);
        if self.held.is_none() {
            self.held = self.out.pop();
        }
        while let Some(frame) = self.held.take() {
            // A want_ts frame cannot go out while a step is pending or in
            // flight; hold it for next tick.
            if frame.want_ts && !self.port.want_ts_allowed() {
                self.held = Some(frame);
                break;
            }
            if !self.port.pre_send(frame.tag, now_t) {
                // Dropped by the pre-send gate (late Pdelay_Resp); discard.
                self.held = self.out.pop();
                continue;
            }
            match dext.send(frame.bytes(), frame.want_ts, frame.tag) {
                SendResult::Ok => {
                    self.port.on_frame_sent(frame.tag, now_t);
                    // Harvest this frame's TX hardware timestamp right away:
                    // the dext holds only ONE pending TX timestamp, so the
                    // next want_ts send would overwrite it.
                    if frame.want_ts {
                        self.harvest_tx_ts(dext);
                    }
                    self.held = self.out.pop();
                }
                SendResult::Busy | SendResult::NoResources => {
                    // Retry this exact frame next tick.
                    self.held = Some(frame);
                    break;
                }
                SendResult::BadArgument => {
                    log.log(
                        Level::Warn,
                        "gptp",
                        "SendFrame BadArgument — dropping frame",
                    );
                    self.port.on_tx_failed(frame.tag);
                    self.held = self.out.pop();
                }
                SendResult::Fatal => {
                    log.log(Level::Error, "gptp", "SendFrame fatal — device gone");
                    return Err(Exit::DextFault);
                }
            }
        }

        // 4) Execute servo / advertising actions.
        while let Some(action) = self.port.take_action() {
            match action {
                Action::AdjFreq { ppb } => dext.adj_freq(ppb),
                Action::StepClock { delta_ns } => {
                    dext.step(delta_ns);
                    // The anchor predates the step, so it is now wrong by
                    // delta_ns. Drop it rather than hand out a skewed reading.
                    self.anchor = None;
                    let new_gen = dext.clock_gen();
                    self.port.on_clock_stepped(now_t, new_gen);
                }
                Action::SetClock { delta_ns } => {
                    // Absolute write: SetClock(ReadClock() + delta). Used for
                    // a large offset the bounded step register would reject —
                    // e.g. the initial SYSTIM↔GM epoch gap (§7.3).
                    let (_mach, nic_ns) = dext.read_clock();
                    let target = (nic_ns as i128 + i128::from(delta_ns)).max(0) as u64;
                    dext.set_clock_abs(target);
                    self.anchor = None;
                    let new_gen = dext.clock_gen();
                    self.port.on_clock_stepped(now_t, new_gen);
                }
                Action::AdvertiseAdp { .. } => { /* ADP advertising is the entity's job */ }
            }
        }

        // 5) Feed the clock model periodically, and re-anchor `now_ns`. Also
        //    read immediately whenever the anchor is missing, so a step is
        //    recovered from on the very next iteration instead of waiting out
        //    the interval with no usable clock.
        if self.anchor.is_none() || now_t.ticks_since(self.last_clock_read) >= self.clock_read_ticks
        {
            let (mach, nic_ns) = dext.read_clock();
            if mach.0 != 0 {
                // DextPort collapses the read to a midpoint; use it for both
                // brackets (read jitter is already averaged out).
                let reads = [avb_gptp::ClockRead {
                    mach_before: mach.0,
                    systim_ns: nic_ns,
                    mach_after: mach.0,
                }];
                let _ = self.port.on_clock_read(now_t, &reads);
                self.anchor = Some(ClockAnchor {
                    mach,
                    systim_ns: nic_ns,
                });
            }
            self.last_clock_read = now_t;
        }

        // Drain the port's own log lines.
        while let Some(line) = self.port.take_log() {
            log.log(Level::Debug, "gptp", &line);
        }
        Ok(())
    }

    /// Feed one already-drained control frame to the port. Used by a loop that
    /// owns the control-ring drain itself (see [`GptpService::service_no_rx`]).
    pub fn handle_rx(&mut self, now_t: MachTicks, slot: &crate::dextio::RxSlot) {
        let meta = RxMeta {
            harvest: now_t,
            hw_ts: slot.hw_ts_ns,
            clock_gen: slot.clock_gen,
            step_ambig: slot.step_ambig,
            desc_vlan_tci: slot.desc_vlan_tci,
            loopback: false,
        };
        let _ = self.port.handle_rx(Now::mach_only(now_t), &slot.data, meta);
    }

    /// Poll briefly for the TX timestamp of the frame just sent. Bounded at
    /// ~10 ms; the I210 latches within a few ms.
    ///
    /// This must run immediately after the send, not on the next iteration:
    /// the dext holds exactly ONE pending TX timestamp, so a second want_ts
    /// send overwrites it and the first exchange silently loses its t1.
    fn harvest_tx_ts(&mut self, dext: &mut RealDext) {
        for _ in 0..20 {
            match dext.poll_tx_ts() {
                TxTsResult::Ready { tag, systim_ns } => {
                    let now = MachTicks(unsafe { mach2::mach_time::mach_absolute_time() });
                    self.port.on_tx_timestamp(tag, GTxTs::Ok(systim_ns), now);
                    return;
                }
                TxTsResult::Timeout | TxTsResult::Idle => return,
                TxTsResult::Pending => {
                    std::thread::sleep(std::time::Duration::from_micros(500));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Apple Silicon: 125/3 ns per tick (24 MHz).
    const TB: Timebase = Timebase {
        numer: 125,
        denom: 3,
    };

    #[test]
    fn interpolation_moves_forward_from_the_anchor() {
        let a = ClockAnchor {
            mach: MachTicks(1_000_000),
            systim_ns: 1_700_000_000_000_000_000,
        };
        // At the anchor itself, gPTP time is exactly the reading.
        assert_eq!(a.interpolate(MachTicks(1_000_000), TB), a.systim_ns);
        // One million ticks later is 1e6 * 125/3 ns.
        let later = a.interpolate(MachTicks(2_000_000), TB);
        assert_eq!(later, a.systim_ns + TB.ticks_to_ns(1_000_000));
        // Monotonic in `now`.
        assert!(a.interpolate(MachTicks(1_500_000), TB) < later);
    }

    /// A `now` before the anchor must not wrap into the far future. The
    /// presentation timestamp is truncated to 32 bits, where a wrapped value
    /// is indistinguishable from a valid one, so this would be silent.
    #[test]
    fn interpolation_does_not_go_backwards_past_the_anchor() {
        let a = ClockAnchor {
            mach: MachTicks(5_000_000),
            systim_ns: 1_000_000_000,
        };
        assert_eq!(a.interpolate(MachTicks(0), TB), a.systim_ns);
        assert_eq!(a.interpolate(MachTicks(4_999_999), TB), a.systim_ns);
    }

    /// The error between re-anchors is the residual frequency offset times
    /// the interval. At the talker's 20 ms cadence that must stay far below
    /// the 2 ms presentation offset, or a late PDU could be stamped early.
    #[test]
    fn talker_reanchor_interval_keeps_interpolation_error_small() {
        // 100 ppm is a generous bound for post-lock residual offset.
        let worst_ppm = 100.0_f64;
        let err_ns = TALKER_CLOCK_READ_NS as f64 * worst_ppm / 1e6;
        assert!(
            err_ns < 10_000.0,
            "interpolation error {err_ns} ns over the re-anchor interval is \
             not negligible against the presentation offset"
        );
    }
}
