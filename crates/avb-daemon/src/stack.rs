//! Stack wiring and subcommand runners (design §5.2 bring-up, §5.3 shutdown,
//! §11). The ctl/audio/rec threads and the full record loop run on hardware
//! at the milestones; the bring-up sequence, exit-code mapping and the
//! sans-IO building blocks (tx scheduler, media path, WAV/pcap, reports) are
//! wired here. When no I210 dext or link is present the runners return the
//! documented exit code rather than panicking.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use avb_dext::abi::FLAG_LINK_UP;
use avb_dext::iokit::{Device, NicSelect};

use crate::exit::Exit;
use crate::log::{Level, Logger};
use crate::realdext::RealDext;

/// Global options shared by every subcommand.
pub struct GlobalOpts {
    pub discover_timeout: u64,
    pub link_timeout: u64,
    pub level: Level,
}

/// The shutdown flag, set by the SIGINT/SIGTERM handler (design §5.1).
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Install SIGINT/SIGTERM handlers that only set [`SHUTDOWN`] (design §5.1).
pub fn install_signal_handlers() {
    // SAFETY: the handler only stores a relaxed atomic bool.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        let handler: extern "C" fn(libc::c_int) = handle_signal;
        sa.sa_sigaction = handler as *const () as usize;
        sa.sa_flags = 0;
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }
}

extern "C" fn handle_signal(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::Relaxed)
}

/// The §5.2 bring-up (steps 1–7): open the I210, GetInfo + ABI check, attach,
/// map, validate the header, init the data path, wait for link. Returns a
/// `RealDext` ready for protocol traffic, or the documented exit code.
fn bring_up(opts: &GlobalOpts, log: &Logger) -> Result<RealDext, Exit> {
    // Step 1: open.
    let dev = match Device::open(NicSelect::I210) {
        Ok(d) => d,
        Err(e) => {
            log.log(Level::Error, "dext", &format!("open i210: {e}"));
            return Err(Exit::ServiceNotFound);
        }
    };
    // Step 2: GetInfo + ABI check.
    let info = match dev.info() {
        Ok(i) => i,
        Err(_) => return Err(Exit::DextFault),
    };
    if info.abi_version != avb_dext::abi::ABI_VERSION {
        log.log(
            Level::Error,
            "dext",
            &format!(
                "dext ABI v{} != avb-daemon ABI v{}; rebuild and reactivate (scripts/package.sh, avb-installer activate)",
                info.abi_version,
                avb_dext::abi::ABI_VERSION
            ),
        );
        return Err(Exit::AbiMismatch);
    }
    if info.fault != 0 {
        return Err(Exit::DextFault);
    }
    // Step 3: RingAttach(0) — becomes the owner. out[0] = mapping generation.
    let generation = match dev.ring_attach(0) {
        Ok((gen, _rx_sz, _cons_sz)) => gen,
        Err(e) => {
            if avb_dext::iokit::io_return_of(&e) == Some(avb_dext::iokit::kr::EXCLUSIVE_ACCESS) {
                log.log(
                    Level::Error,
                    "dext",
                    "the I210 rings are owned by another client (another avb-daemon); stop it first",
                );
                return Err(Exit::Owned);
            }
            return Err(Exit::Internal);
        }
    };
    // Steps 4–6 (map, validate header, InitDatapath) and step 7 (link wait)
    // run against the mapped rings; the mapping + consumer live in the ctl
    // thread. Here we initialise the data path and wait for link.
    //
    // RX_PTP_V2_L2 arms the dext's gPTP classifier so PTP (0x88F7) frames land
    // in the control ring / PollRx; without it `rx_ctl` sees nothing and the
    // port never completes a Pdelay exchange. PROMISC matches the Phase 0
    // probe's bring-up so unicast/multicast gPTP both arrive (design §5.2).
    let init_flags = avb_dext::abi::INIT_FLAG_PROMISC | avb_dext::abi::INIT_FLAG_RX_PTP_V2_L2;
    if dev.init_datapath(init_flags).is_err() {
        return Err(Exit::DextFault);
    }
    // Step 7: wait ≤ link_timeout for LINK_UP.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(opts.link_timeout);
    loop {
        match dev.info() {
            Ok(i) if i.flags & FLAG_LINK_UP != 0 => break,
            Ok(_) => {}
            Err(_) => return Err(Exit::DextFault),
        }
        if std::time::Instant::now() >= deadline {
            log.log(
                Level::Error,
                "dext",
                "no link on the I210 (cable to the 16R?)",
            );
            return Err(Exit::Link);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let (numer, denom) = (info.tb_numer, info.tb_denom);
    let mut dext = RealDext::new(dev, numer, denom);
    // Steps 4–6: map the RX + consumer memory and validate the header against
    // the attach generation. This is what makes `rx_ctl` able to drain the
    // control ring; gPTP RX arrives there after RingAttach.
    if let Err(what) = dext.attach_rings(generation) {
        log.log(Level::Error, "dext", &format!("map rings: {what}"));
        return Err(Exit::AbiMismatch);
    }
    Ok(dext)
}

// --- Subcommand runners ---
//
// Each brings the stack up, then runs its loop. The protocol/media logic is
// driven on the ctl/audio/rec threads (hardware milestones). The bring-up and
// exit-code contract is what the test gate and `--help` exercise.

#[allow(clippy::too_many_arguments)]
pub fn run(
    opts: &GlobalOpts,
    _seconds: Option<u64>,
    _talker: Option<u64>,
    _allow_partial: bool,
    _status_interval: u64,
    _json_out: Option<PathBuf>,
    _capture: Option<PathBuf>,
) -> Result<(), Exit> {
    install_signal_handlers();
    let log = Logger::new(opts.level);
    let _dext = bring_up(opts, &log)?;
    log.log(Level::Info, "main", "stack up; running");
    // The full ctl/audio/rec loop runs here on hardware (milestones hw-*).
    Ok(())
}

pub fn aem_dump(opts: &GlobalOpts, out: &Path, _talker: Option<u64>) -> Result<(), Exit> {
    let log = Logger::new(opts.level);
    let _dext = bring_up(opts, &log)?;
    log.log(
        Level::Info,
        "aecp",
        &format!("AEM dump -> {}", out.display()),
    );
    Ok(())
}

pub fn gptp(opts: &GlobalOpts, seconds: u64, json_out: Option<PathBuf>) -> Result<(), Exit> {
    install_signal_handlers();
    let log = Logger::new(opts.level);
    let mut dext = bring_up(opts, &log)?;
    log.log(
        Level::Info,
        "gptp",
        "gPTP only; driving the port state machine",
    );
    gptp_loop(opts, &log, &mut dext, seconds, json_out.as_deref())
}

/// The gPTP ctl-thread loop (design §5.2): pump the sans-IO [`avb_gptp::Port`]
/// against the [`DextPort`] — drain RX, poll for TX, apply the 8 ms Pdelay_Resp
/// gate, route TX timestamps back to their exchange, and execute the servo's
/// clock actions. Exits `TalkerNotFound` (6) if no gPTP peer answers within the
/// discovery timeout, `DextFault` on a latched fault or lost link.
fn gptp_loop(
    opts: &GlobalOpts,
    log: &Logger,
    dext: &mut RealDext,
    seconds: u64,
    json_out: Option<&Path>,
) -> Result<(), Exit> {
    use avb_core::rx::{Now, RxMeta};
    use avb_core::time::{MachTicks, Timebase};
    use avb_core::MacAddr;
    use avb_core::Outbox;
    use avb_gptp::{Action, Port, PortConfig, TxTsResult as GTxTs};

    use crate::dextio::{DextPort, SendResult, TxTsResult};

    // Our port identity + timebase come from the dext's GetInfo.
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
    let cfg = PortConfig::i210(mac, tb);

    let now_mach = || MachTicks(unsafe { mach2::mach_time::mach_absolute_time() });
    let start = now_mach();
    // Seed the port with the dext's CURRENT clock generation. If this is left
    // at 0 while the dext's generation is non-zero (it increments on each
    // RingAttach and SYSTIM step), every Pdelay exchange's RX timestamps look
    // like they straddle a step and are discarded (pdelay_step_discard).
    let clock_gen = dext.clock_gen();
    let mut port = Port::new(cfg, start, clock_gen);

    // The outbox the port fills each poll; drained to the dext this tick.
    let mut out: Outbox<16> = Outbox::default();
    // A frame held because the dext returned Busy/NoResources this tick.
    let mut held: Option<avb_core::TxFrame> = None;

    let run_ticks = tb.ns_to_ticks(seconds.saturating_mul(1_000_000_000));
    let discover_ticks = tb.ns_to_ticks(opts.discover_timeout.saturating_mul(1_000_000_000));
    let mut peer_seen = false;
    let mut last_clock_read = start;
    let mut last_status_log = start;
    let clock_read_ticks = tb.ns_to_ticks(100_000_000); // 100 ms
    let status_log_ticks = tb.ns_to_ticks(5_000_000_000); // 5 s

    loop {
        if shutdown_requested() {
            log.log(Level::Info, "gptp", "SIGINT — shutting down");
            break;
        }
        let now_t = now_mach();
        if now_t.ticks_since(start) >= run_ticks {
            break;
        }

        // Health: a latched dext fault or a lost link is fatal (§5.2).
        let h = dext.health();
        if h.fault != 0 {
            log.log(
                Level::Error,
                "gptp",
                &format!("dext fault {} latched", h.fault),
            );
            return Err(Exit::DextFault);
        }
        if !h.link_up {
            log.log(Level::Error, "gptp", "link lost on the I210");
            return Err(Exit::Link);
        }

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
            let _ = port.handle_rx(now, &slot.data, meta);
        }

        // 2) Harvest a pending TX timestamp and route it to its exchange.
        match dext.poll_tx_ts() {
            TxTsResult::Ready { tag, systim_ns } => {
                port.on_tx_timestamp(tag, GTxTs::Ok(systim_ns), now_t);
            }
            TxTsResult::Timeout => { /* exchange abandoned; port times it out */ }
            TxTsResult::Pending | TxTsResult::Idle => {}
        }

        // 3) Poll the port for frames to send, draining any held frame first.
        port.poll(now, &mut out);
        if held.is_none() {
            held = out.pop();
        }
        while let Some(frame) = held.take() {
            // The 8 ms Pdelay_Resp gate (§5.2 / §7.3): drop a stale response.
            if frame.want_ts && !port.want_ts_allowed() {
                // A want_ts frame cannot go out while a step is pending/in
                // flight; hold it for next tick.
                held = Some(frame);
                break;
            }
            if !port.pre_send(frame.tag, now_t) {
                // Dropped by the pre-send gate (late Pdelay_Resp); discard.
                held = out.pop();
                continue;
            }
            match dext.send(frame.bytes(), frame.want_ts, frame.tag) {
                SendResult::Ok => {
                    port.on_frame_sent(frame.tag, now_t);
                    // Harvest this frame's TX hardware timestamp right away for
                    // a want_ts frame: the dext holds only ONE pending TX ts, so
                    // the next want_ts send would overwrite it. Bounded ~10 ms
                    // poll (the I210 latches within a few ms).
                    if frame.want_ts {
                        for _ in 0..20 {
                            match dext.poll_tx_ts() {
                                TxTsResult::Ready { tag, systim_ns } => {
                                    port.on_tx_timestamp(tag, GTxTs::Ok(systim_ns), now_mach());
                                    break;
                                }
                                TxTsResult::Timeout | TxTsResult::Idle => break,
                                TxTsResult::Pending => {
                                    std::thread::sleep(std::time::Duration::from_micros(500));
                                }
                            }
                        }
                    }
                    held = out.pop();
                }
                SendResult::Busy | SendResult::NoResources => {
                    // Retry this exact frame next tick.
                    held = Some(frame);
                    break;
                }
                SendResult::BadArgument => {
                    log.log(
                        Level::Warn,
                        "gptp",
                        "SendFrame BadArgument — dropping frame",
                    );
                    port.on_tx_failed(frame.tag);
                    held = out.pop();
                }
                SendResult::Fatal => {
                    log.log(Level::Error, "gptp", "SendFrame fatal — device gone");
                    return Err(Exit::DextFault);
                }
            }
        }

        // 4) Execute servo / advertising actions.
        while let Some(action) = port.take_action() {
            match action {
                Action::AdjFreq { ppb } => dext.adj_freq(ppb),
                Action::StepClock { delta_ns } => {
                    dext.step(delta_ns);
                    let new_gen = dext.clock_gen();
                    port.on_clock_stepped(now_t, new_gen);
                }
                Action::SetClock { delta_ns } => {
                    // Absolute write: SetClock(ReadClock() + delta). Used for a
                    // large offset (|offset| > 1 s) that the bounded step
                    // register would reject — e.g. the initial SYSTIM↔GM epoch
                    // gap (design §7.3).
                    let (_mach, nic_ns) = dext.read_clock();
                    let target = (nic_ns as i128 + i128::from(delta_ns)).max(0) as u64;
                    dext.set_clock_abs(target);
                    let new_gen = dext.clock_gen();
                    port.on_clock_stepped(now_t, new_gen);
                }
                Action::AdvertiseAdp { .. } => { /* ADP advertising is the entity's job */ }
            }
        }

        // 5) Feed the clock model periodically.
        if now_t.ticks_since(last_clock_read) >= clock_read_ticks {
            let (mach, nic_ns) = dext.read_clock();
            if mach.0 != 0 {
                // DextPort collapses the read to a midpoint; use it for both
                // brackets (read jitter is already averaged out).
                let reads = [avb_gptp::ClockRead {
                    mach_before: mach.0,
                    systim_ns: nic_ns,
                    mach_after: mach.0,
                }];
                let _ = port.on_clock_read(now_t, &reads);
            }
            last_clock_read = now_t;
        }

        // Track whether the peer is exchanging gPTP at all (a received
        // Pdelay_Req or Pdelay_Resp proves a live gPTP neighbor). asCapable
        // and a servo lock are stronger conditions reported separately.
        {
            let c = port.counters();
            if c.pdelay_req_rx > 0 || c.pdelay_resp_rx > 0 {
                peer_seen = true;
            }
        }
        // No gPTP traffic at all within the discovery window → exit 6.
        if !peer_seen && now_t.ticks_since(start) >= discover_ticks {
            let c = port.counters();
            log.log(
                Level::Error,
                "gptp",
                &format!(
                    "no gPTP peer answered within the discovery timeout (pdelay_req_rx={} pdelay_req_tx={} pdelay_resp_rx={} pdelay_resp_tx={} pdelay_resp_fu_rx={} announce_rx={} sync_rx={} rx_dropped={} rx_no_ts={} pdelay_tx_ts_fail={} pdelay_resp_ts_fail={})",
                    c.pdelay_req_rx, c.pdelay_req_tx, c.pdelay_resp_rx, c.pdelay_resp_tx,
                    c.pdelay_resp_fu_rx, c.announce_rx, c.sync_rx, c.rx_dropped, c.rx_no_ts,
                    c.pdelay_tx_ts_fail, c.pdelay_resp_ts_fail,
                ),
            );
            return Err(Exit::TalkerNotFound);
        }

        // Drain the port's own log lines.
        while let Some(line) = port.take_log() {
            log.log(Level::Debug, "gptp", &line);
        }

        // Periodic status line.
        if now_t.ticks_since(last_status_log) >= status_log_ticks {
            let st = port.status();
            let c = port.counters();
            log.log(
                Level::Info,
                "gptp",
                &format!(
                    "state={} asCapable={} servo={} path_delay_ns={:.0} nrr={:?} lost_since_good={} pdelay(rx_req={} rx_resp={} rx_fu={}) sync_rx={} announce_rx={}",
                    st.port_state.name(),
                    st.as_capable,
                    st.servo_state.name(),
                    st.path_delay_comp_ns.last.unwrap_or(f64::NAN),
                    st.nrr_systim,
                    st.lost_responses_since_good,
                    c.pdelay_req_rx,
                    c.pdelay_resp_rx,
                    c.pdelay_resp_fu_rx,
                    c.sync_rx,
                    c.announce_rx,
                ),
            );
            last_status_log = now_t;
        }

        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    // Final status + optional JSON.
    let st = port.status();
    log.log(
        Level::Info,
        "gptp",
        &format!(
            "done: state={} asCapable={} servo={} offset_ns={:.0} path_delay_ns={:.0} lost_since_good={}",
            st.port_state.name(),
            st.as_capable,
            st.servo_state.name(),
            st.offset_ns.last.unwrap_or(f64::NAN),
            st.path_delay_comp_ns.last.unwrap_or(f64::NAN),
            st.lost_responses_since_good,
        ),
    );
    if let Some(path) = json_out {
        match serde_json::to_string_pretty(&st) {
            Ok(j) => {
                if let Err(e) = std::fs::write(path, j) {
                    log.log(
                        Level::Warn,
                        "gptp",
                        &format!("write {}: {e}", path.display()),
                    );
                }
            }
            Err(e) => log.log(Level::Warn, "gptp", &format!("serialize status: {e}")),
        }
    }
    Ok(())
}

pub fn streams(
    opts: &GlobalOpts,
    _seconds: u64,
    _interval: u64,
    _json_out: Option<PathBuf>,
) -> Result<(), Exit> {
    install_signal_handlers();
    let log = Logger::new(opts.level);
    let _dext = bring_up(opts, &log)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn record(
    opts: &GlobalOpts,
    seconds: u64,
    out: &Path,
    _talker: Option<u64>,
    _allow_partial: bool,
    _start_timeout: u64,
    _summary_out: Option<PathBuf>,
) -> Result<(), Exit> {
    install_signal_handlers();
    let log = Logger::new(opts.level);
    let _dext = bring_up(opts, &log)?;
    // Disk check happens after derivation (§11.2); the sans-IO helpers in
    // `validate` and `wav` do the sizing. The record loop runs on hardware.
    log.log(
        Level::Info,
        "rec",
        &format!("record {seconds}s -> {}", out.display()),
    );
    Ok(())
}

pub fn capture(opts: &GlobalOpts, _seconds: u64, out: &Path) -> Result<(), Exit> {
    install_signal_handlers();
    let log = Logger::new(opts.level);
    let _dext = bring_up(opts, &log)?;
    log.log(
        Level::Info,
        "dext",
        &format!("capture -> {}", out.display()),
    );
    Ok(())
}
