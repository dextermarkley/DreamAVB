// SPDX-License-Identifier: MIT OR Apache-2.0

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

pub fn aem_dump(opts: &GlobalOpts, out: &Path, talker: Option<u64>) -> Result<(), Exit> {
    install_signal_handlers();
    let log = Logger::new(opts.level);
    let mut dext = bring_up(opts, &log)?;
    aem_dump_loop(opts, &log, &mut dext, out, talker)
}

/// Discover the 16R via ADP and walk its AEM via AECP READ_DESCRIPTOR, writing
/// the `avb-aem-dump/1` JSON (design §9.2). Exits `TalkerNotFound` (6) if no
/// talker (or an ambiguous set, when `--talker` is not given) is discovered.
fn aem_dump_loop(
    opts: &GlobalOpts,
    log: &Logger,
    dext: &mut RealDext,
    out: &Path,
    want_talker: Option<u64>,
) -> Result<(), Exit> {
    use avb_avdecc::adp::Adpdu;
    use avb_avdecc::aecp::{AecpEngine, AecpOutcome, AemPdu};
    use avb_avdecc::constants::{aecp_msg, aem_cmd, desc as aem_desc, subtype};
    use avb_avdecc::EntityTable;
    use avb_avtp::common::ETHERTYPE_AVTP;
    use avb_core::eth::{EthFrame, EthHeader};
    use avb_core::time::{MachTicks, Timebase};
    use avb_core::{Eui64, MacAddr, TxFrame};

    use crate::dextio::{DextPort, SendResult};

    let info = dext.device().info().map_err(|_| Exit::DextFault)?;
    let our_mac = MacAddr::new(info.mac);
    let tb = Timebase {
        numer: info.tb_numer,
        denom: info.tb_denom,
    };
    let our_id = Eui64::from_mac(our_mac);
    let now_mach = || MachTicks(unsafe { mach2::mach_time::mach_absolute_time() });
    let start = now_mach();
    let ns_since = |t: MachTicks| tb.ticks_to_ns(now_mach().ticks_since(t));

    // --- Phase 1: ADP discovery ---
    let mut table = EntityTable::new(our_id);
    let discover_deadline_ns = opts.discover_timeout.saturating_mul(1_000_000_000);
    // Build a global ENTITY_DISCOVER (entity_id 0 = discover all), broadcast
    // periodically to prompt entities that advertise only on demand (§6.2.2).
    let discover_adp = Adpdu {
        message_type: avb_avdecc::constants::adp_msg::ENTITY_DISCOVER,
        valid_time: 0,
        entity_id: Eui64([0; 8]),
        entity_model_id: 0,
        entity_capabilities: 0,
        talker_stream_sources: 0,
        talker_capabilities: 0,
        listener_stream_sinks: 0,
        listener_capabilities: 0,
        controller_capabilities: 0,
        available_index: 0,
        gptp_grandmaster_id: 0,
        gptp_domain_number: 0,
        identify_control_index: 0,
        interface_index: 0,
        association_id: 0,
    };
    let mut last_discover = MachTicks(0);
    let mut rx_total = 0u64;
    let mut rx_avtp = 0u64;
    let mut rx_adp = 0u64;
    let mut rx_other_et: std::collections::BTreeMap<u16, u64> = Default::default();
    let talker: Eui64 = loop {
        if shutdown_requested() {
            return Err(Exit::Interrupted);
        }
        // Re-broadcast ENTITY_DISCOVER about every 2 s.
        if last_discover.0 == 0 || ns_since(last_discover) >= 2_000_000_000 {
            let hdr = EthHeader {
                dst: avb_core::AVDECC_MULTICAST,
                src: our_mac,
                vlan: None,
                ethertype: ETHERTYPE_AVTP,
            };
            if let Ok(f) = TxFrame::build(&hdr, &discover_adp.encode(), false, 0) {
                let _ = dext.send(f.bytes(), false, 0);
            }
            last_discover = now_mach();
        }
        for slot in dext.rx_ctl() {
            rx_total += 1;
            // Parse the Ethernet frame; AVDECC rides AVTP (ethertype 0x22F0).
            if let Ok(p) = EthFrame::parse(&slot.data) {
                if p.hdr.ethertype == ETHERTYPE_AVTP {
                    rx_avtp += 1;
                    // IEEE 1722 common header: byte0 = subtype.
                    let st = p.payload.first().copied();
                    if st == Some(subtype::ADP) {
                        rx_adp += 1;
                        if let Some(adp) = Adpdu::decode(p.payload) {
                            let _ = table.observe(&adp, p.hdr.src, ns_since(start), false);
                        }
                    }
                } else {
                    *rx_other_et.entry(p.hdr.ethertype).or_insert(0) += 1;
                }
            }
        }
        if let Some(id) = want_talker {
            let tid = Eui64(id.to_be_bytes());
            if table.get(tid).is_some() {
                break tid;
            }
        } else {
            match table.sole_talker() {
                Ok(Some(id)) => break id,
                Ok(None) => {}
                Err(ids) => {
                    log.log(
                        Level::Error,
                        "adp",
                        &format!("{} talkers discovered; pass --talker <id>", ids.len()),
                    );
                    return Err(Exit::TalkerNotFound);
                }
            }
        }
        if ns_since(start) >= discover_deadline_ns {
            log.log(
                Level::Error,
                "adp",
                &format!(
                    "no AVDECC talker discovered (rx_total={rx_total} rx_avtp={rx_avtp} rx_adp={rx_adp} other_ethertypes={:04x?})",
                    rx_other_et.keys().collect::<Vec<_>>()
                ),
            );
            return Err(Exit::TalkerNotFound);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };

    let entry = table.get(talker).ok_or(Exit::TalkerNotFound)?;
    let talker_mac = entry.mac;
    let adp = entry.adp;
    log.log(
        Level::Info,
        "adp",
        &format!(
            "talker {} at {:02x?} (model 0x{:016x})",
            u64::from_be_bytes(talker.0),
            talker_mac.octets(),
            adp.entity_model_id
        ),
    );

    // --- Phase 2: AEM READ_DESCRIPTOR walk ---
    let mut eng = AecpEngine::new(our_id);
    // Collected descriptor bytes, keyed by (type, index).
    let mut descriptors: std::collections::BTreeMap<(u16, u16), Vec<u8>> = Default::default();
    // The walk's work list: (descriptor_type, descriptor_index). Seed with
    // ENTITY; CONFIGURATION and its children are added as responses arrive.
    let read_payload = |dtype: u16, dindex: u16| -> Vec<u8> {
        let mut v = Vec::with_capacity(6);
        v.extend_from_slice(&0u16.to_be_bytes()); // configuration_index
        v.extend_from_slice(&0u16.to_be_bytes()); // reserved (§7.4.5)
        v.extend_from_slice(&dtype.to_be_bytes()); // descriptor_type
        v.extend_from_slice(&dindex.to_be_bytes()); // descriptor_index
        v
    };
    eng.enqueue(
        talker,
        talker_mac,
        aem_cmd::READ_DESCRIPTOR,
        read_payload(aem_desc::ENTITY, 0),
    );
    let mut requested: std::collections::BTreeSet<(u16, u16)> = Default::default();
    requested.insert((aem_desc::ENTITY, 0));

    let walk_deadline_ns = ns_since(start) + 20_000_000_000; // 20 s cap
                                                             // Idle grace: after the engine drains, wait briefly for any late response
                                                             // before declaring the walk complete (robust to duplicate/late responses).
    let idle_grace_ns = 500_000_000u64; // 500 ms
    let mut last_activity_ns = ns_since(start);
    loop {
        if shutdown_requested() {
            return Err(Exit::Interrupted);
        }
        let engine_busy = eng.queued_count() > 0 || eng.in_flight_count() > 0;
        if !engine_busy && ns_since(start).saturating_sub(last_activity_ns) >= idle_grace_ns {
            break;
        }
        if ns_since(start) >= walk_deadline_ns {
            log.log(
                Level::Warn,
                "aecp",
                "AEM walk timed out; writing partial dump",
            );
            break;
        }
        let now = ns_since(start);

        // Emit a queued command (pacing handled by the engine).
        if let Some(send) = eng.poll(now) {
            let hdr = EthHeader {
                dst: send.target_mac,
                src: our_mac,
                vlan: None,
                ethertype: ETHERTYPE_AVTP,
            };
            if let Ok(frame) = TxFrame::build(&hdr, &send.frame, false, 0) {
                log.log(
                    Level::Debug,
                    "aecp",
                    &format!(
                        "sent cmd=0x{:04x} seq={} ({} B) to {:02x?}",
                        send.command_type,
                        send.sequence_id,
                        frame.bytes().len(),
                        send.target_mac.octets()
                    ),
                );
                match dext.send(frame.bytes(), false, 0) {
                    SendResult::Ok => last_activity_ns = ns_since(start),
                    SendResult::Busy | SendResult::NoResources => { /* retried by engine */ }
                    SendResult::BadArgument => {}
                    SendResult::Fatal => return Err(Exit::DextFault),
                }
            }
        }

        // Drain responses.
        for slot in dext.rx_ctl() {
            let Ok(p) = EthFrame::parse(&slot.data) else {
                continue;
            };
            if p.hdr.ethertype != ETHERTYPE_AVTP
                || p.payload.first().copied() != Some(subtype::AECP)
            {
                continue;
            }
            let Some(resp) = AemPdu::decode(p.payload) else {
                continue;
            };
            if resp.message_type != aecp_msg::AEM_RESPONSE {
                continue;
            }
            log.log(
                Level::Debug,
                "aecp",
                &format!(
                    "rx resp cmd=0x{:04x} seq={} status={} target={:016x}",
                    resp.command_type,
                    resp.sequence_id,
                    resp.status,
                    u64::from_be_bytes(resp.target_entity_id.0)
                ),
            );
            if let Some(outcome) = eng.on_rx(&resp, now) {
                last_activity_ns = ns_since(start);
                if let AecpOutcome::Response(r) = outcome {
                    log.log(
                        Level::Debug,
                        "aecp",
                        &format!(
                            "matched resp cmd=0x{:04x} payload_len={}",
                            r.command_type,
                            r.payload.len()
                        ),
                    );
                    if r.command_type == aem_cmd::READ_DESCRIPTOR && r.payload.len() >= 8 {
                        // Response payload: config_index(2) + reserved(2) + descriptor.
                        let dbytes = match r.payload.get(4..) {
                            Some(b) if b.len() >= 4 => b.to_vec(),
                            _ => continue,
                        };
                        let dtype = u16::from_be_bytes([
                            *dbytes.first().unwrap_or(&0),
                            *dbytes.get(1).unwrap_or(&0),
                        ]);
                        let dindex = u16::from_be_bytes([
                            *dbytes.get(2).unwrap_or(&0),
                            *dbytes.get(3).unwrap_or(&0),
                        ]);
                        let is_new = !descriptors.contains_key(&(dtype, dindex));
                        descriptors.insert((dtype, dindex), dbytes.clone());
                        if is_new {
                            log.log(
                                Level::Info,
                                "aecp",
                                &format!(
                                    "descriptor type=0x{dtype:04x} index={dindex} ({} bytes)",
                                    dbytes.len()
                                ),
                            );
                        }
                        // Expand the walk from ENTITY → CONFIGURATION, and from
                        // CONFIGURATION → its child descriptor counts.
                        let enqueue = |dt: u16,
                                       di: u16,
                                       req: &mut std::collections::BTreeSet<(u16, u16)>,
                                       eng: &mut AecpEngine| {
                            if req.insert((dt, di)) {
                                eng.enqueue(
                                    talker,
                                    talker_mac,
                                    aem_cmd::READ_DESCRIPTOR,
                                    read_payload(dt, di),
                                );
                            }
                        };
                        if dtype == aem_desc::ENTITY {
                            enqueue(aem_desc::CONFIGURATION, 0, &mut requested, &mut eng);
                        } else if dtype == aem_desc::CONFIGURATION {
                            // Configuration descriptor: a descriptor_counts list
                            // of (descriptor_type, count) pairs (§7.2.1). Walk
                            // the audio/stream/clock descriptor families.
                            for (dt, count) in config_descriptor_counts(&dbytes) {
                                if matches!(
                                    dt,
                                    aem_desc::AUDIO_UNIT
                                        | aem_desc::STREAM_INPUT
                                        | aem_desc::STREAM_OUTPUT
                                ) {
                                    for di in 0..count {
                                        enqueue(dt, di, &mut requested, &mut eng);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Age the engine (retries / timeouts).
        let (_resends, outcomes) = eng.on_tick(now);
        for o in outcomes {
            if let AecpOutcome::TimedOut {
                command_type,
                sequence_id,
            } = o
            {
                log.log(
                    Level::Warn,
                    "aecp",
                    &format!("command 0x{command_type:04x} seq {sequence_id} timed out"),
                );
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    // --- Phase 3: assemble and write the dump ---
    log.log(
        Level::Info,
        "aecp",
        &format!(
            "walk done: requested={} collected={}",
            requested.len(),
            descriptors.len(),
        ),
    );
    let dump = build_aem_dump(&adp, our_mac, talker, &descriptors);
    match serde_json::to_string_pretty(&dump) {
        Ok(j) => {
            std::fs::write(out, j).map_err(|e| {
                log.log(
                    Level::Error,
                    "aecp",
                    &format!("write {}: {e}", out.display()),
                );
                Exit::DiskOrPath
            })?;
        }
        Err(e) => {
            log.log(Level::Error, "aecp", &format!("serialize dump: {e}"));
            return Err(Exit::Internal);
        }
    }
    log.log(
        Level::Info,
        "aecp",
        &format!(
            "AEM dump: {} descriptors -> {}",
            descriptors.len(),
            out.display()
        ),
    );
    Ok(())
}

/// Parse the `descriptor_counts` list of an AEM CONFIGURATION descriptor into
/// `(descriptor_type, count)` pairs (IEEE 1722.1 §7.2.1). The list starts at
/// `descriptor_counts_offset` (u16 at a fixed position) with `descriptor_counts_count`
/// pairs. Returns an empty list if the layout cannot be read.
fn config_descriptor_counts(d: &[u8]) -> Vec<(u16, u16)> {
    // CONFIGURATION: descriptor_type(2) index(2) object_name(64) localized(2)
    // descriptor_counts_count(2) descriptor_counts_offset(2) ... = fixed 74.
    let rd_u16 = |off: usize| -> Option<u16> {
        d.get(off..off + 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
    };
    let count = match rd_u16(70) {
        Some(c) => c as usize,
        None => return Vec::new(),
    };
    let offset = match rd_u16(72) {
        // offset is from the start of the descriptor (§7.2.1).
        Some(o) => o as usize,
        None => return Vec::new(),
    };
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let base = offset + i * 4;
        match (rd_u16(base), rd_u16(base + 2)) {
            (Some(dt), Some(n)) => out.push((dt, n)),
            _ => break,
        }
    }
    out
}

/// Assemble the `avb-aem-dump/1` JSON from the discovered ADPDU and the raw
/// descriptor bytes collected during the walk (design §9.2).
fn build_aem_dump(
    adp: &avb_avdecc::adp::Adpdu,
    controller_mac: avb_core::MacAddr,
    talker: avb_core::Eui64,
    descriptors: &std::collections::BTreeMap<(u16, u16), Vec<u8>>,
) -> serde_json::Value {
    use serde_json::json;
    let descs: Vec<serde_json::Value> = descriptors
        .iter()
        .map(|((dtype, dindex), bytes)| {
            json!({
                "descriptor_type": format!("0x{dtype:04x}"),
                "descriptor_index": dindex,
                "raw": bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            })
        })
        .collect();
    json!({
        "schema": avb_avdecc::aem::json::SCHEMA,
        "entity_id": format!("0x{:016x}", u64::from_be_bytes(talker.0)),
        "entity_model_id": format!("0x{:016x}", adp.entity_model_id),
        "talker_stream_sources": adp.talker_stream_sources,
        "listener_stream_sinks": adp.listener_stream_sinks,
        "controller_mac": format!("{:02x?}", controller_mac.octets()),
        "descriptor_count": descriptors.len(),
        "descriptors": descs,
    })
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
    use avb_core::time::{MachTicks, Timebase};

    use crate::dextio::DextPort;
    use crate::gptpsvc::{GptpService, DEFAULT_CLOCK_READ_NS};

    // Timebase for this loop's own deadlines; the service keeps its own copy.
    let info = dext.device().info().map_err(|_| Exit::DextFault)?;
    let tb = Timebase {
        numer: info.tb_numer,
        denom: info.tb_denom,
    };
    if !tb.is_valid() {
        log.log(Level::Error, "gptp", "dext reported an invalid timebase");
        return Err(Exit::DextFault);
    }

    let now_mach = || MachTicks(unsafe { mach2::mach_time::mach_absolute_time() });
    let start = now_mach();
    let mut svc = GptpService::new(dext, log, DEFAULT_CLOCK_READ_NS)?;

    let run_ticks = tb.ns_to_ticks(seconds.saturating_mul(1_000_000_000));
    let discover_ticks = tb.ns_to_ticks(opts.discover_timeout.saturating_mul(1_000_000_000));
    let mut peer_seen = false;
    let mut last_status_log = start;
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

        // One gPTP iteration: RX into the port, TX timestamps harvested,
        // frames out, servo actions executed, clock model fed. Shared with
        // the talker loop, which needs the same disciplined clock to stamp a
        // presentation time on every audio PDU.
        svc.service(dext, log, now_t)?;

        // Track whether the peer is exchanging gPTP at all (a received
        // Pdelay_Req or Pdelay_Resp proves a live gPTP neighbor). asCapable
        // and a servo lock are stronger conditions reported separately.
        {
            let c = svc.counters();
            if c.pdelay_req_rx > 0 || c.pdelay_resp_rx > 0 {
                peer_seen = true;
            }
        }
        // No gPTP traffic at all within the discovery window → exit 6.
        if !peer_seen && now_t.ticks_since(start) >= discover_ticks {
            let c = svc.counters();
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

        // Periodic status line.
        if now_t.ticks_since(last_status_log) >= status_log_ticks {
            let st = svc.status();
            let c = svc.counters();
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
    let st = svc.status();
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
    seconds: u64,
    interval: u64,
    json_out: Option<PathBuf>,
) -> Result<(), Exit> {
    install_signal_handlers();
    let log = Logger::new(opts.level);
    let mut dext = bring_up(opts, &log)?;
    streams_loop(
        opts,
        &log,
        &mut dext,
        seconds,
        interval,
        json_out.as_deref(),
        None,
        None,
    )
}

/// Bind all of the 16R's talker streams via ACMP and count received AAF
/// packets per stream over `seconds`, reporting per-stream pkt/s each
/// `interval` (design §4 hw-streams; Class A is 8000 pkt/s). Reuses the
/// ADP/AECP control path and the mapped audio ring.
#[allow(clippy::too_many_arguments)]
fn streams_loop(
    opts: &GlobalOpts,
    log: &Logger,
    dext: &mut RealDext,
    seconds: u64,
    interval: u64,
    json_out: Option<&Path>,
    wav_out: Option<&Path>,
    summary_out: Option<&Path>,
) -> Result<(), Exit> {
    use avb_avdecc::acmp::Acmpdu;
    use avb_avdecc::adp::{stream_id_for, Adpdu};
    use avb_avdecc::constants::{acmp_msg, subtype};
    use avb_avdecc::controller::build_get_tx_state;
    use avb_avdecc::entity::acmp_listener::{Action, Event, ListenerSm, State};
    use avb_avdecc::EntityTable;
    use avb_avtp::common::ETHERTYPE_AVTP;
    use avb_core::eth::{EthFrame, EthHeader};
    use avb_core::time::{MachTicks, Timebase};
    use avb_core::{Eui64, MacAddr, StreamId, TxFrame};
    use avb_core::{Now, Outbox, RxMeta};
    use avb_srp::attr::{
        Attribute, Listener, ListenerDecl, DEFAULT_DOMAIN, ETHERTYPE_MSRP, ETHERTYPE_MVRP,
    };
    use avb_srp::participant::{Participant, ParticipantConfig, SrpEvent};

    use crate::dextio::DextPort;

    let info = dext.device().info().map_err(|_| Exit::DextFault)?;
    let our_mac = MacAddr::new(info.mac);
    let tb = Timebase {
        numer: info.tb_numer,
        denom: info.tb_denom,
    };
    let our_id = Eui64::from_mac(our_mac);
    let now_mach = || MachTicks(unsafe { mach2::mach_time::mach_absolute_time() });
    let start = now_mach();
    let ns_since = |t: MachTicks| tb.ticks_to_ns(now_mach().ticks_since(t));

    // --- Phase 1: discover the 16R (ADP) ---
    let mut table = EntityTable::new(our_id);
    let discover_adp = Adpdu {
        message_type: avb_avdecc::constants::adp_msg::ENTITY_DISCOVER,
        valid_time: 0,
        entity_id: Eui64([0; 8]),
        entity_model_id: 0,
        entity_capabilities: 0,
        talker_stream_sources: 0,
        talker_capabilities: 0,
        listener_stream_sinks: 0,
        listener_capabilities: 0,
        controller_capabilities: 0,
        available_index: 0,
        gptp_grandmaster_id: 0,
        gptp_domain_number: 0,
        identify_control_index: 0,
        interface_index: 0,
        association_id: 0,
    };
    let discover_deadline_ns = opts.discover_timeout.saturating_mul(1_000_000_000);
    let mut last_discover = MachTicks(0);
    let (talker, talker_mac, n_streams, talker_avail_index) = loop {
        if shutdown_requested() {
            return Err(Exit::Interrupted);
        }
        if last_discover.0 == 0 || ns_since(last_discover) >= 2_000_000_000 {
            let hdr = EthHeader {
                dst: avb_core::AVDECC_MULTICAST,
                src: our_mac,
                vlan: None,
                ethertype: ETHERTYPE_AVTP,
            };
            if let Ok(f) = TxFrame::build(&hdr, &discover_adp.encode(), false, 0) {
                let _ = dext.send(f.bytes(), false, 0);
            }
            last_discover = now_mach();
        }
        for slot in dext.rx_ctl() {
            if let Ok(p) = EthFrame::parse(&slot.data) {
                if p.hdr.ethertype == ETHERTYPE_AVTP
                    && p.payload.first().copied() == Some(subtype::ADP)
                {
                    if let Some(adp) = Adpdu::decode(p.payload) {
                        let _ = table.observe(&adp, p.hdr.src, ns_since(start), false);
                    }
                }
            }
        }
        if let Ok(Some(id)) = table.sole_talker() {
            if let Some(e) = table.get(id) {
                break (
                    id,
                    e.mac,
                    e.adp.talker_stream_sources,
                    e.adp.available_index,
                );
            }
        }
        if ns_since(start) >= discover_deadline_ns {
            log.log(Level::Error, "adp", "no AVDECC talker discovered");
            return Err(Exit::TalkerNotFound);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    let n_streams = n_streams.min(avb_avdecc::derive::S_MAX as u16);
    log.log(
        Level::Info,
        "acmp",
        &format!(
            "talker {} has {n_streams} stream source(s); binding all",
            u64::from_be_bytes(talker.0)
        ),
    );

    // --- Advertise OUR listener entity via ADP ---
    // A Milan talker will not honour a Listener Ready / connection from an
    // entity it has not discovered. We send ADP discover but never advertised
    // ourselves, so the 16R did not know we exist. Advertise ENTITY_AVAILABLE
    // periodically (valid_time 10 -> 20 s) with an incrementing
    // available_index, declaring ourselves an AEM/gPTP/Class-A listener with
    // n_streams sinks.
    let mut our_avail_index: u32 = 0;
    let send_our_adp = |dext: &mut RealDext, avail: u32| {
        use avb_avdecc::constants::{entity_caps, listener_caps};
        let adp = Adpdu {
            message_type: avb_avdecc::constants::adp_msg::ENTITY_AVAILABLE,
            valid_time: 10,
            entity_id: our_id,
            entity_model_id: 0,
            entity_capabilities: entity_caps::AEM_SUPPORTED
                | entity_caps::CLASS_A_SUPPORTED
                | entity_caps::GPTP_SUPPORTED,
            talker_stream_sources: 0,
            talker_capabilities: 0,
            listener_stream_sinks: n_streams,
            listener_capabilities: listener_caps::IMPLEMENTED | listener_caps::AUDIO_SINK,
            controller_capabilities: 0,
            available_index: avail,
            gptp_grandmaster_id: 0,
            gptp_domain_number: 0,
            identify_control_index: 0,
            interface_index: 0,
            association_id: 0,
        };
        let hdr = EthHeader {
            dst: avb_core::AVDECC_MULTICAST,
            src: our_mac,
            vlan: None,
            ethertype: ETHERTYPE_AVTP,
        };
        if let Ok(f) = TxFrame::build(&hdr, &adp.encode(), false, 0) {
            let _ = dext.send(f.bytes(), false, 0);
        }
    };
    // Announce ourselves immediately so the 16R learns of us before we probe.
    send_our_adp(dext, our_avail_index);
    our_avail_index = our_avail_index.wrapping_add(1);
    let mut last_advert = now_mach();

    // --- Phase 1b: GET_STREAM_INFO on each STREAM_OUTPUT ---
    // Learn the 16R's authoritative per-output stream_id, dest_mac, vlan and
    // its streaming/connected flags. A Milan talker's stream_id is NOT the
    // MAC+index our probe guesses; this is the real one, and the CONNECTED /
    // probing flags tell us whether the output is active.
    let mut gsi: std::collections::BTreeMap<u16, (u64, [u8; 6], u16, u32)> = Default::default();
    {
        use avb_avdecc::aecp::{AecpEngine, AecpOutcome, AemPdu};
        use avb_avdecc::constants::aecp_msg as am;
        use avb_avdecc::constants::aem_cmd;
        let mut eng = AecpEngine::new(our_id);
        for uid in 0..n_streams {
            let mut pl = Vec::with_capacity(4);
            pl.extend_from_slice(&avb_avdecc::constants::desc::STREAM_OUTPUT.to_be_bytes());
            pl.extend_from_slice(&uid.to_be_bytes());
            eng.enqueue(talker, talker_mac, aem_cmd::GET_STREAM_INFO, pl);
        }
        let deadline = ns_since(start).saturating_add(6_000_000_000);
        let mut last = ns_since(start);
        loop {
            if shutdown_requested() {
                return Err(Exit::Interrupted);
            }
            let busy = eng.queued_count() > 0 || eng.in_flight_count() > 0;
            if (!busy && ns_since(start).saturating_sub(last) >= 400_000_000)
                || ns_since(start) >= deadline
            {
                break;
            }
            let now = ns_since(start);
            if let Some(send) = eng.poll(now) {
                let hdr = EthHeader {
                    dst: send.target_mac,
                    src: our_mac,
                    vlan: None,
                    ethertype: ETHERTYPE_AVTP,
                };
                if let Ok(f) = TxFrame::build(&hdr, &send.frame, false, 0) {
                    let _ = dext.send(f.bytes(), false, 0);
                    last = now;
                }
            }
            for slot in dext.rx_ctl() {
                let Ok(p) = EthFrame::parse(&slot.data) else {
                    continue;
                };
                if p.hdr.ethertype != ETHERTYPE_AVTP
                    || p.payload.first().copied() != Some(subtype::AECP)
                {
                    continue;
                }
                let Some(resp) = AemPdu::decode(p.payload) else {
                    continue;
                };
                if resp.message_type != am::AEM_RESPONSE {
                    continue;
                }
                if let Some(AecpOutcome::Response(r)) = eng.on_rx(&resp, now) {
                    last = now;
                    if r.command_type == aem_cmd::GET_STREAM_INFO {
                        // §7.4.16: desc_type(2) desc_index(2) flags(4)
                        // stream_format(8) stream_id(8) msrp_acc_lat(4)
                        // stream_dest_mac(6) ...
                        let b = &r.payload;
                        let idx = b
                            .get(2..4)
                            .and_then(|x| x.try_into().ok())
                            .map(u16::from_be_bytes)
                            .unwrap_or(0);
                        let flags = b
                            .get(4..8)
                            .and_then(|x| x.try_into().ok())
                            .map(u32::from_be_bytes)
                            .unwrap_or(0);
                        let sid = b
                            .get(16..24)
                            .and_then(|x| x.try_into().ok())
                            .map(u64::from_be_bytes)
                            .unwrap_or(0);
                        let mut dmac = [0u8; 6];
                        if let Some(m) = b.get(28..34) {
                            dmac.copy_from_slice(m);
                        }
                        let vid = b
                            .get(44..46)
                            .and_then(|x| x.try_into().ok())
                            .map(u16::from_be_bytes)
                            .unwrap_or(0);
                        gsi.insert(idx, (sid, dmac, vid, flags));
                        log.log(
                            Level::Info,
                            "aecp",
                            &format!(
                                "GET_STREAM_INFO out[{idx}]: flags=0x{flags:08x} id=0x{sid:016x} dest={dmac:02x?} vid={vid} (connected={})",
                                flags & avb_avdecc::milan::stream_info_flags::CONNECTED != 0
                            ),
                        );
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        log.log(
            Level::Info,
            "aecp",
            &format!(
                "GET_STREAM_INFO: {} of {n_streams} output(s) answered",
                gsi.len()
            ),
        );
    }

    // Per-stream filter params (dest_mac+vid), shared across the connect,
    // bind and count phases.
    let mut filter: Vec<(u64, [u8; 6], u16)> = Vec::new();
    let mut bound_ids: std::collections::BTreeMap<u16, StreamId> = Default::default();

    // --- Phase 1c: ACMP CONNECT_TX to the talker ---
    // GET_STREAM_INFO shows the 16R's outputs are NOT connected (flags bit 27
    // clear). A Milan talker transitions its STREAM_OUTPUT to CONNECTED and
    // begins egress when a listener issues CONNECT_TX_COMMAND (IEEE 1722.1
    // §8.2.1). We send one per output naming our listener; the response (and
    // a follow-up GET_STREAM_INFO showing CONNECTED) confirms it. This is the
    // leg the PROBE_TX-only flow omits, and the one the 16R actually needs.
    {
        use avb_avdecc::acmp::Acmpdu;
        use avb_avdecc::constants::{acmp_msg, acmp_status, subtype as sub};
        let mut acmp_seq: u16 = 0;
        for uid in 0..n_streams {
            let sid = gsi
                .get(&uid)
                .map(|&(id, _, _, _)| StreamId(id))
                .unwrap_or_else(|| stream_id_for(talker_mac, uid));
            let pdu = Acmpdu {
                message_type: acmp_msg::CONNECT_TX_COMMAND,
                status: acmp_status::SUCCESS,
                // CONNECT_TX_COMMAND carries stream_id = 0 (IEEE 1722.1-2021
                // §8.2.1 / AVDECC clarifications §3.2): the talker fills it in
                // the response. Sending a non-zero stream_id here is what a
                // strict talker rejects/ignores.
                stream_id: StreamId(0),
                controller_entity_id: our_id,
                talker_entity_id: talker,
                listener_entity_id: our_id,
                talker_unique_id: uid,
                listener_unique_id: uid,
                sequence_id: acmp_seq,
                ..Default::default()
            };
            let _ = sid;
            acmp_seq = acmp_seq.wrapping_add(1);
            let hdr = EthHeader {
                dst: avb_core::AVDECC_MULTICAST,
                src: our_mac,
                vlan: None,
                ethertype: ETHERTYPE_AVTP,
            };
            if let Ok(f) = TxFrame::build(&hdr, &pdu.encode(), false, 0) {
                let _ = dext.send(f.bytes(), false, 0);
            }
        }
        // Collect CONNECT_TX_RESPONSEs for ~2 s.
        let deadline = ns_since(start).saturating_add(2_000_000_000);
        while ns_since(start) < deadline {
            if shutdown_requested() {
                return Err(Exit::Interrupted);
            }
            for slot in dext.rx_ctl() {
                let Ok(p) = EthFrame::parse(&slot.data) else {
                    continue;
                };
                if p.hdr.ethertype != ETHERTYPE_AVTP
                    || p.payload.first().copied() != Some(sub::ACMP)
                {
                    continue;
                }
                if let Some(a) = Acmpdu::decode(p.payload) {
                    if a.message_type == acmp_msg::CONNECT_TX_RESPONSE
                        && a.talker_entity_id == talker
                    {
                        log.log(
                            Level::Info,
                            "acmp",
                            &format!(
                                "CONNECT_TX_RESPONSE uid={} status={} flags=0x{:04x} conn_count={} id=0x{:016x} dest={:02x?} vid={}",
                                a.talker_unique_id,
                                a.status,
                                a.flags,
                                a.connection_count,
                                a.stream_id.0,
                                a.stream_dest_mac.octets(),
                                a.stream_vlan_id
                            ),
                        );
                        if a.status == acmp_status::SUCCESS
                            && a.stream_dest_mac.octets()[0] & 1 == 1
                        {
                            let v = if a.stream_vlan_id == 0 {
                                avb_srp::attr::DEFAULT_DOMAIN.vid
                            } else {
                                a.stream_vlan_id
                            };
                            let dmac = a.stream_dest_mac.octets();
                            filter.retain(|(_, m, _)| *m != dmac);
                            filter.push((a.stream_id.0, dmac, v));
                        }
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    // --- Phase 1d: re-query GET_STREAM_INFO to confirm the connect stuck ---
    {
        use avb_avdecc::aecp::{AecpEngine, AecpOutcome, AemPdu};
        use avb_avdecc::constants::aecp_msg as am;
        use avb_avdecc::constants::aem_cmd;
        let mut eng = AecpEngine::new(our_id);
        for uid in 0..n_streams {
            let mut pl = Vec::with_capacity(4);
            pl.extend_from_slice(&avb_avdecc::constants::desc::STREAM_OUTPUT.to_be_bytes());
            pl.extend_from_slice(&uid.to_be_bytes());
            eng.enqueue(talker, talker_mac, aem_cmd::GET_STREAM_INFO, pl);
        }
        let deadline = ns_since(start).saturating_add(3_000_000_000);
        let mut last = ns_since(start);
        loop {
            if shutdown_requested() {
                return Err(Exit::Interrupted);
            }
            let busy = eng.queued_count() > 0 || eng.in_flight_count() > 0;
            if (!busy && ns_since(start).saturating_sub(last) >= 400_000_000)
                || ns_since(start) >= deadline
            {
                break;
            }
            let now = ns_since(start);
            if let Some(send) = eng.poll(now) {
                let hdr = EthHeader {
                    dst: send.target_mac,
                    src: our_mac,
                    vlan: None,
                    ethertype: ETHERTYPE_AVTP,
                };
                if let Ok(f) = TxFrame::build(&hdr, &send.frame, false, 0) {
                    let _ = dext.send(f.bytes(), false, 0);
                    last = now;
                }
            }
            for slot in dext.rx_ctl() {
                let Ok(p) = EthFrame::parse(&slot.data) else {
                    continue;
                };
                if p.hdr.ethertype != ETHERTYPE_AVTP
                    || p.payload.first().copied() != Some(subtype::AECP)
                {
                    continue;
                }
                let Some(resp) = AemPdu::decode(p.payload) else {
                    continue;
                };
                if resp.message_type != am::AEM_RESPONSE {
                    continue;
                }
                if let Some(AecpOutcome::Response(r)) = eng.on_rx(&resp, now) {
                    last = now;
                    if r.command_type == aem_cmd::GET_STREAM_INFO {
                        let b = &r.payload;
                        let idx = b
                            .get(2..4)
                            .and_then(|x| x.try_into().ok())
                            .map(u16::from_be_bytes)
                            .unwrap_or(0);
                        let flags = b
                            .get(4..8)
                            .and_then(|x| x.try_into().ok())
                            .map(u32::from_be_bytes)
                            .unwrap_or(0);
                        log.log(
                            Level::Info,
                            "aecp",
                            &format!(
                                "post-connect out[{idx}]: flags=0x{flags:08x} connected={}",
                                flags & avb_avdecc::milan::stream_info_flags::CONNECTED != 0
                            ),
                        );
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    // --- Phase 2: bind every talker stream via the Milan listener SM ---
    //
    // We are the controller AND the listener (same station). The classic
    // CONNECT_RX_COMMAND is a controller->listener message and never reaches
    // the 16R; the 16R is a Milan talker and is driven by the PROBE_TX
    // (GET_TX_STATE) cycle in the §9.4 listener SM. So we run one `ListenerSm`
    // per stream: BindRx -> (random delay) -> SendProbeTx -> we emit
    // GET_TX_STATE_COMMAND to the talker -> GET_TX_STATE_RESPONSE comes back ->
    // ProbeTxResponse -> the SM emits SetStreamFilter with the real
    // stream_id/dest_mac/vlan it learned from the talker.
    let mut sms: Vec<ListenerSm> = (0..n_streams).map(ListenerSm::new).collect();
    let mut probe_seq: u16 = 0;
    // In-flight GET_TX_STATE: talker_unique_id -> listener_unique_id, so a
    // response (which carries talker ids, not our listener_unique_id) can be
    // routed back to the SM that probed.
    let mut probe_route: std::collections::BTreeMap<u16, u16> = Default::default();

    // Deterministic-enough random 0..=1s pre-probe delay (we only need it to
    // be bounded and not synchronized across streams, not cryptographic).
    let rand01 = |luid: u16| -> u64 { (u64::from(luid) * 137_000_000) % 1_000_000_000 };

    let send_acmp = |dext: &mut RealDext, frame: &[u8]| {
        let hdr = EthHeader {
            dst: avb_core::AVDECC_MULTICAST,
            src: our_mac,
            vlan: None,
            ethertype: ETHERTYPE_AVTP,
        };
        if let Ok(f) = TxFrame::build(&hdr, frame, false, 0) {
            let _ = dext.send(f.bytes(), false, 0);
        }
    };

    // Helper: apply the actions a SM emitted this step.
    let apply_actions =
        |dext: &mut RealDext,
         luid: u16,
         talker: Eui64,
         actions: Vec<Action>,
         probe_seq: &mut u16,
         probe_route: &mut std::collections::BTreeMap<u16, u16>,
         filter: &mut Vec<(u64, [u8; 6], u16)>,
         bound_ids: &mut std::collections::BTreeMap<u16, StreamId>| {
            for act in actions {
                match act {
                    Action::SendProbeTx {
                        talker_entity_id,
                        talker_unique_id,
                    } => {
                        let seq = *probe_seq;
                        *probe_seq = probe_seq.wrapping_add(1);
                        probe_route.insert(talker_unique_id, luid);
                        let frame =
                            build_get_tx_state(our_id, talker_entity_id, talker_unique_id, seq);
                        send_acmp(dext, &frame);
                    }
                    Action::SetStreamFilter { effective } => {
                        // The dext matches AAF on dest_mac+vid, not stream_id
                        // (a Milan talker's GET_TX_STATE may report stream_id 0
                        // until the stream is reserved). It also REQUIRES a
                        // tagged frame whose VID equals the entry and rejects a
                        // vid-0 entry, so when the probe reports vid 0 we use
                        // the SR class A VID the stream is actually tagged with.
                        let dmac = effective.dest_mac.octets();
                        let vid = if effective.vlan_id == 0 {
                            DEFAULT_DOMAIN.vid
                        } else {
                            effective.vlan_id
                        };
                        bound_ids.insert(luid, effective.stream_id);
                        filter.retain(|(_, m, _)| *m != dmac);
                        filter.push((effective.stream_id.0, dmac, vid));
                        log.log(
                            Level::Info,
                            "acmp",
                            &format!(
                                "stream {luid} settled: id=0x{:016x} dest={:02x?} vid={vid}",
                                effective.stream_id.0, dmac
                            ),
                        );
                    }
                    Action::Log(msg) => {
                        log.log(Level::Info, "acmp", &format!("stream {luid}: {msg}"))
                    }
                    // ReplaceStreamTable/RemoveStreamEntry/Declare/WithdrawListener
                    // are MSRP/table concerns not needed for the pkt-rate check.
                    _ => {}
                }
            }
            let _ = talker;
        };

    // --- SRP (MSRP/MVRP) participant: the reservation that makes the 16R
    // actually transmit. A Milan talker advertises its streams via MSRP
    // Talker Advertise (0x22EA) and only begins egress once a Listener
    // declares Ready for that stream_id. We therefore run the §8 MRP
    // participant: declare the SR class A VID (MVRP) and Domain (MSRP), learn
    // each stream's real stream_id/dest/vid from its Talker Advertise, then
    // declare an MSRP Listener Ready so the 16R streams. The dext filter is
    // keyed on dest_mac+vid, which the TA (and the ACMP probe) both carry.
    let srp_cfg = ParticipantConfig {
        mac: our_mac,
        timebase: tb,
        seed: u64::from_be_bytes(our_id.0),
    };
    // SRP timers convert ns durations to ticks against `tb` and add them to
    // `now.mach`, so SRP time MUST be raw mach ticks (not the ns the ACMP SMs
    // use). Keep the two clocks distinct.
    let srp_now = || Now::mach_only(now_mach());
    let mut srp = Participant::new(srp_cfg, now_mach());
    // Declare the SR class A VID and Domain up front (§8.2 SRP-1).
    srp.declare(
        now_mach(),
        Attribute::Vid {
            vid: DEFAULT_DOMAIN.vid,
        },
    );
    srp.declare(now_mach(), Attribute::Domain(DEFAULT_DOMAIN));
    // Stream ids we have already declared a Listener Ready for.
    let mut listener_declared: std::collections::BTreeSet<u64> = Default::default();
    // Stream ids the 16R has sent a real Talker Advertise for (the only
    // source of the true streaming dest_mac/vid).
    let mut ta_seen: std::collections::BTreeSet<u64> = Default::default();

    // Drain the SRP outbox onto the wire.
    let mut srp_out: Outbox<32> = Outbox::new();
    let drain_srp = |dext: &mut RealDext, out: &mut Outbox<32>| {
        while let Some(f) = out.pop() {
            let _ = dext.send(f.bytes(), false, 0);
        }
    };

    // Seed every SM with BindRx (talker is in ADP, so each goes PRB_W_DELAY).
    for sm in &mut sms {
        let luid = sm.listener_unique_id;
        let (_resp, actions) = sm.on_event(
            Event::BindRx {
                talker_entity_id: talker,
                talker_unique_id: luid,
                controller_entity_id: our_id,
                talker_in_adp: true,
                talker_available_index: Some(talker_avail_index),
            },
            ns_since(start),
            rand01(luid),
        );
        apply_actions(
            dext,
            luid,
            talker,
            actions,
            &mut probe_seq,
            &mut probe_route,
            &mut filter,
            &mut bound_ids,
        );
    }

    // Milan settled-state law (v1.2 tables 5.28-5.30, kebag-logic milan-fpga
    // #54 defect 5): the Listener Ready attribute is declared ONLY AFTER the
    // stream settles -- i.e. after the talker registers a Talker Advertise --
    // NOT proactively at bind. Declaring it early is a spec deviation and a
    // strict Milan talker (the 16R) may refuse to advertise. So here we only
    // establish MVRP VID + MSRP Domain and let the ACMP CONNECT_TX (re-sent in
    // the loop below) prompt the 16R's Talker Advertise; the TalkerRegistered
    // handler then declares Listener Ready in response.
    srp.force_leave_all();
    srp.poll(srp_now(), &mut srp_out);
    drain_srp(dext, &mut srp_out);
    log.log(
        Level::Info,
        "srp",
        "declared VID + Domain; awaiting Talker Advertise before Listener Ready",
    );

    // Wait for the 16R to REGISTER our MSRP Domain before prompting the talker.
    // A Talker Failed code 8 ("egress port not AVB-capable") appears when the
    // talker is evaluated before the SR domain is established on the link; our
    // gPTP is stably asCapable (path_delay ~150 ns), so the flapping is a
    // domain-timing race, not a timestamp-quality problem. Establish the SR
    // domain first (up to ~5 s), then connect.
    {
        let domain_deadline = ns_since(start).saturating_add(5_000_000_000);
        let mut domain_ok = false;
        while ns_since(start) < domain_deadline && !shutdown_requested() {
            srp.poll(srp_now(), &mut srp_out);
            drain_srp(dext, &mut srp_out);
            for slot in dext.rx_ctl() {
                if let Ok(p) = EthFrame::parse(&slot.data) {
                    if matches!(p.hdr.ethertype, ETHERTYPE_MSRP | ETHERTYPE_MVRP) {
                        let meta = RxMeta {
                            desc_vlan_tci: slot.desc_vlan_tci,
                            ..Default::default()
                        };
                        let _ = srp.handle_rx(srp_now(), &slot.data, meta);
                    }
                }
            }
            while let Some(ev) = srp.pop_event() {
                if matches!(ev, SrpEvent::DomainRegistered(_)) {
                    domain_ok = true;
                }
            }
            if domain_ok {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        log.log(
            Level::Info,
            "srp",
            if domain_ok {
                "16R registered our SR Domain; proceeding to connect"
            } else {
                "SR Domain not confirmed within 5 s; proceeding anyway"
            },
        );
    }

    // Drive ACMP (probe) and SRP (reservation) together until every stream
    // has a Listener Ready declared and its filter installed, or the deadline
    // passes. The SRP Talker Advertise is the authoritative source of
    // stream_id/dest/vid and declaring Listener Ready is what makes the 16R
    // transmit; the ACMP probe runs in parallel for Milan correctness.
    let bind_deadline_ns = ns_since(start).saturating_add(20_000_000_000);
    // Apple's working controller re-asserts the full ACMP CONNECT handshake
    // every ~1 s, and the 16R advertises its talker in lockstep. Re-send our
    // CONNECT_TX_COMMAND for every stream on the same cadence so the Milan
    // talker holds the connection and keeps advertising.
    let mut connect_seq: u16 = 1000;
    let mut last_connect = MachTicks(0);
    loop {
        if shutdown_requested() {
            return Err(Exit::Interrupted);
        }
        // Periodic CONNECT_TX re-assertion (~1 s), like Apple's controller.
        if last_connect.0 == 0 || ns_since(last_connect) >= 1_000_000_000 {
            use avb_avdecc::acmp::Acmpdu;
            use avb_avdecc::constants::{acmp_msg, acmp_status};
            for uid in 0..n_streams {
                let pdu = Acmpdu {
                    message_type: acmp_msg::CONNECT_TX_COMMAND,
                    status: acmp_status::SUCCESS,
                    stream_id: StreamId(0),
                    controller_entity_id: our_id,
                    talker_entity_id: talker,
                    listener_entity_id: our_id,
                    talker_unique_id: uid,
                    listener_unique_id: uid,
                    sequence_id: connect_seq,
                    ..Default::default()
                };
                connect_seq = connect_seq.wrapping_add(1);
                let hdr = EthHeader {
                    dst: avb_core::AVDECC_MULTICAST,
                    src: our_mac,
                    vlan: None,
                    ethertype: ETHERTYPE_AVTP,
                };
                if let Ok(f) = TxFrame::build(&hdr, &pdu.encode(), false, 0) {
                    let _ = dext.send(f.bytes(), false, 0);
                }
            }
            last_connect = now_mach();
        }
        let now = ns_since(start);

        // Keep our ADP advertisement fresh (every ~2 s) so the 16R holds us
        // in its entity table while we probe and reserve.
        if ns_since(last_advert) >= 2_000_000_000 {
            send_our_adp(dext, our_avail_index);
            our_avail_index = our_avail_index.wrapping_add(1);
            last_advert = now_mach();
        }

        // Fire any due ACMP SM timers (delay -> probe, probe timeout -> resend).
        for sm in &mut sms {
            if let Some(deadline) = sm.timer_deadline_ns() {
                if now >= deadline {
                    let luid = sm.listener_unique_id;
                    let actions = sm.on_timer(now, rand01(luid));
                    apply_actions(
                        dext,
                        luid,
                        talker,
                        actions,
                        &mut probe_seq,
                        &mut probe_route,
                        &mut filter,
                        &mut bound_ids,
                    );
                }
            }
        }

        // Drive SRP timers and transmit (JoinTime/LeaveAll).
        srp.poll(srp_now(), &mut srp_out);
        drain_srp(dext, &mut srp_out);

        // Drain received control frames: ACMP (GET_TX_STATE_RESPONSE) + SRP.
        for slot in dext.rx_ctl() {
            let Ok(p) = EthFrame::parse(&slot.data) else {
                continue;
            };
            // Diagnostic: is the 16R sending AECP (enumerating/controlling us)?
            if p.hdr.ethertype == ETHERTYPE_AVTP
                && p.payload.first().copied() == Some(subtype::AECP)
            {
                use avb_avdecc::aecp::AemPdu;
                if let Some(cmd) = AemPdu::decode(p.payload) {
                    if cmd.target_entity_id == our_id {
                        log.log(
                            Level::Info,
                            "aecp",
                            &format!(
                                "INBOUND AECP to us: msg_type={} cmd=0x{:04x} from={:02x?}",
                                cmd.message_type,
                                cmd.command_type,
                                p.hdr.src.octets()
                            ),
                        );
                    }
                }
            }
            match p.hdr.ethertype {
                ETHERTYPE_AVTP if p.payload.first().copied() == Some(subtype::ACMP) => {
                    let Some(acmp) = Acmpdu::decode(p.payload) else {
                        continue;
                    };
                    if acmp.message_type != acmp_msg::GET_TX_STATE_RESPONSE
                        || acmp.talker_entity_id != talker
                    {
                        continue;
                    }
                    let Some(&luid) = probe_route.get(&acmp.talker_unique_id) else {
                        continue;
                    };
                    if let Some(sm) = sms.iter_mut().find(|s| s.listener_unique_id == luid) {
                        let (_r, actions) = sm.on_event(
                            Event::ProbeTxResponse {
                                status: acmp.status,
                                stream_id: acmp.stream_id,
                                dest_mac: acmp.stream_dest_mac,
                                vlan_id: acmp.stream_vlan_id,
                            },
                            now,
                            rand01(luid),
                        );
                        apply_actions(
                            dext,
                            luid,
                            talker,
                            actions,
                            &mut probe_seq,
                            &mut probe_route,
                            &mut filter,
                            &mut bound_ids,
                        );
                    }
                }
                ETHERTYPE_MSRP | ETHERTYPE_MVRP => {
                    let meta = RxMeta {
                        desc_vlan_tci: slot.desc_vlan_tci,
                        ..Default::default()
                    };
                    let _ = srp.handle_rx(srp_now(), &slot.data, meta);
                }
                _ => {}
            }
        }

        // Process SRP events: a registered Talker Advertise gives us the real
        // addressing and is our cue to declare Listener Ready.
        while let Some(ev) = srp.pop_event() {
            log.log(Level::Debug, "srp", &format!("event: {ev:?}"));
            if let SrpEvent::TalkerRegistered {
                stream_id,
                talker: t,
            } = ev
            {
                // A Talker FAILED means the talker will not stream this source
                // (e.g. code 8 = egress port not AVB-capable: the 16R does not
                // consider our link AVB-capable, typically its asCapable toward
                // us is false / peer-delay over threshold). Record and skip --
                // it is NOT a usable advertise.
                if let Some(code) = t.failure_code() {
                    log.log(
                        Level::Warn,
                        "srp",
                        &format!(
                            "TALKER FAILED id=0x{:016x} code={code} ({}): 16R refuses to stream this source",
                            stream_id.0,
                            match code {
                                1 => "insufficient bandwidth",
                                2 => "insufficient bridge resources",
                                3 => "insufficient bandwidth for traffic class",
                                6 => "stream preempted",
                                8 => "egress port not AVB-capable",
                                9 => "use a different destination",
                                _ => "other",
                            }
                        ),
                    );
                    continue;
                }
                let adv = t.advertise();
                let dmac = adv.dest_mac.octets();
                ta_seen.insert(stream_id.0);
                // Install/refresh the dext filter for this stream (match on
                // stream_id + dest_mac + VLAN; defect 4). The dext rejects
                // vid 0, so fall back to the SR class A VID.
                let ta_vid = if adv.vlan_id == 0 {
                    DEFAULT_DOMAIN.vid
                } else {
                    adv.vlan_id
                };
                filter.retain(|(_, m, _)| *m != dmac);
                filter.push((stream_id.0, dmac, ta_vid));
                // Declare Listener Ready so the 16R starts streaming (once).
                if listener_declared.insert(stream_id.0) {
                    srp.declare_new(
                        now_mach(),
                        Attribute::Listener(Listener {
                            stream_id,
                            decl: ListenerDecl::Ready,
                        }),
                    );
                    log.log(
                        Level::Info,
                        "srp",
                        &format!(
                            "talker advertise: id=0x{:016x} dest={:02x?} vid={} -> Listener Ready",
                            stream_id.0, dmac, adv.vlan_id
                        ),
                    );
                }
                // Feed the ACMP SM a TA registration (Milan §9.4).
                let luid = u16::try_from(filter.len().saturating_sub(1)).unwrap_or(0);
                if let Some(sm) = sms.iter_mut().find(|s| s.listener_unique_id == luid) {
                    let (_r, actions) = sm.on_event(
                        Event::TaRegistered {
                            dest_mac: adv.dest_mac,
                            vlan_id: adv.vlan_id,
                            accumulated_latency_ns: adv.accumulated_latency_ns,
                        },
                        now,
                        rand01(luid),
                    );
                    apply_actions(
                        dext,
                        luid,
                        talker,
                        actions,
                        &mut probe_seq,
                        &mut probe_route,
                        &mut filter,
                        &mut bound_ids,
                    );
                }
            }
        }

        // Keep the handshake running for a short settling window so the
        // Listener Ready declarations reach the 16R, it advertises/reserves,
        // and the ACMP probes complete. Exit once every stream has a filter
        // entry (addressing learned) and the ACMP SMs have left the probe
        // states, or when the deadline passes.
        let acmp_settled = sms.iter().all(|s| {
            !matches!(
                s.state(),
                State::PrbWAvail
                    | State::PrbWDelay
                    | State::PrbWResp
                    | State::PrbWResp2
                    | State::PrbWRetry
            )
        });
        let all_addressed = filter.len() >= usize::from(n_streams);
        // Prefer waiting until the 16R has actually Talker-Advertised every
        // stream (ta_seen) -- that is the only source of the real streaming
        // dest_mac. Fall back to probe-addressing + ACMP-settled after the
        // minimum window so a partially-advertising talker still proceeds.
        let all_ta = ta_seen.len() >= usize::from(n_streams);
        let min_handshake = now >= 15_000_000_000;
        if all_ta || (acmp_settled && all_addressed && min_handshake) || now >= bind_deadline_ns {
            // Flush any pending SRP sends before counting.
            srp.poll(srp_now(), &mut srp_out);
            drain_srp(dext, &mut srp_out);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    if filter.is_empty() {
        log.log(
            Level::Error,
            "srp",
            "no streams reserved (no Talker Advertise and no PROBE_TX response); cannot count",
        );
        return Err(Exit::StreamRefused);
    }
    // Program the dext stream filter so the audio ring receives these streams.
    dext.set_filter(&filter);
    {
        let c = srp.counters();
        log.log(
            Level::Info,
            "srp",
            &format!(
                "srp counters: pdu_tx={} pdu_rx={} regs={} dereg={} talker_changed={} own={} tx_full={} dropped={}",
                c.pdu_tx, c.pdu_rx, c.registrations, c.deregistrations, c.talker_changed, c.own_frames, c.tx_full, c.pdu_dropped
            ),
        );
    }
    log.log(
        Level::Info,
        "acmp",
        &format!(
            "{} stream(s) settled; counting AAF packets for {seconds}s",
            filter.len()
        ),
    );

    // --- Phase 3: count AAF packets per stream over the run ---
    let mut counts: std::collections::BTreeMap<u64, u64> = Default::default();
    let mut gaps: std::collections::BTreeMap<u64, u64> = Default::default();
    let mut last_seq: std::collections::BTreeMap<u64, u8> = Default::default();
    let mut raw_audio_slots: u64 = 0;
    let mut aaf_decode_fail: u64 = 0;
    let mut first_slot_dump: Option<Vec<u8>> = None;
    // Per-(stream_id, channel) peak |sample| to find which AVB stream+channel
    // carries a live signal (channel-map check for hw-audio).
    let mut peak: std::collections::BTreeMap<(u64, u16), i64> = Default::default();
    // WAV recording (hw-audio): record the PRIMARY stream (the lowest AAF
    // stream_id, i.e. the one carrying 16R channels 1..N) to an interleaved
    // 32-bit WAV. Header is written on first frame once we know the channel
    // count, data streamed, sizes patched on finalize. Media-clock drift is
    // measured from the frame count vs elapsed wall time.
    let mut wav_file: Option<std::fs::File> = None;
    let mut wav_fmt: Option<crate::wav::WavFormat> = None;
    let mut wav_stream_id: u64 = 0;
    let mut wav_data_bytes: u64 = 0;
    let mut wav_frames: u64 = 0;
    // 1b-1 producer: expose the PRIMARY stream's recovered PCM over the shm
    // ring (/avb16r.ring) so the CoreAudio plug-in (a separate process) can
    // read it. Built lazily on the first primary-stream packet once the AAF
    // format (channels/rate/spf) is known — never hardcoded. `ShmProducer`
    // owns the mapping and the producer together. The media clock drives the
    // ring's clock block for the plug-in's timebase.
    let mut ring: Option<avb_audioring::ShmProducer> = None;
    let mut mclk: Option<avb_mediaclock::MediaClock> = None;
    let mut ring_stream_id: u64 = 0;
    let mut ring_spf: usize = 0;
    // Geometry the ring was built from (first packet of the primary stream).
    // Later packets must match it exactly, or the placement would be blind.
    let mut ring_channels: u16 = 0;
    let mut ring_bit_depth: u8 = 0;
    let mut ring_format: u8 = 0;
    let mut ring_nsr: u8 = 0;
    let mut ring_data_len: u16 = 0;
    let mut ring_format_mismatch: u64 = 0;
    // Sequential write cursor for the ring FIFO (frames). Advanced by spf per
    // placed packet; the plug-in's Consumer drains it in order.
    let mut ring_write_pos: u64 = 0;
    let run_ns = seconds.saturating_mul(1_000_000_000);
    let interval_ns = interval.max(1).saturating_mul(1_000_000_000);
    let count_start = now_mach();
    let mut last_report = count_start;
    loop {
        if shutdown_requested() {
            break;
        }
        if ns_since(count_start) >= run_ns {
            break;
        }
        let h = dext.health();
        if h.fault != 0 {
            log.log(Level::Error, "streams", &format!("dext fault {}", h.fault));
            return Err(Exit::DextFault);
        }
        // Maintain the MSRP/MVRP reservation (JoinTime refresh + LeaveAll)
        // and absorb peer control frames; without this the declaration ages
        // out and the 16R stops transmitting mid-run.
        {
            if ns_since(last_advert) >= 2_000_000_000 {
                send_our_adp(dext, our_avail_index);
                our_avail_index = our_avail_index.wrapping_add(1);
                last_advert = now_mach();
            }
            if ns_since(last_connect) >= 1_000_000_000 {
                use avb_avdecc::acmp::Acmpdu;
                use avb_avdecc::constants::{acmp_msg, acmp_status};
                for uid in 0..n_streams {
                    let pdu = Acmpdu {
                        message_type: acmp_msg::CONNECT_TX_COMMAND,
                        status: acmp_status::SUCCESS,
                        stream_id: StreamId(0),
                        controller_entity_id: our_id,
                        talker_entity_id: talker,
                        listener_entity_id: our_id,
                        talker_unique_id: uid,
                        listener_unique_id: uid,
                        sequence_id: connect_seq,
                        ..Default::default()
                    };
                    connect_seq = connect_seq.wrapping_add(1);
                    let hdr = EthHeader {
                        dst: avb_core::AVDECC_MULTICAST,
                        src: our_mac,
                        vlan: None,
                        ethertype: ETHERTYPE_AVTP,
                    };
                    if let Ok(f) = TxFrame::build(&hdr, &pdu.encode(), false, 0) {
                        let _ = dext.send(f.bytes(), false, 0);
                    }
                }
                last_connect = now_mach();
            }
            srp.poll(srp_now(), &mut srp_out);
            drain_srp(dext, &mut srp_out);
            for slot in dext.rx_ctl() {
                if let Ok(p) = EthFrame::parse(&slot.data) {
                    if matches!(p.hdr.ethertype, ETHERTYPE_MSRP | ETHERTYPE_MVRP) {
                        let meta = RxMeta {
                            desc_vlan_tci: slot.desc_vlan_tci,
                            ..Default::default()
                        };
                        let _ = srp.handle_rx(srp_now(), &slot.data, meta);
                    }
                }
            }
            while let Some(ev) = srp.pop_event() {
                if let SrpEvent::TalkerRegistered {
                    stream_id,
                    talker: t,
                } = ev
                {
                    let adv = t.advertise();
                    let dmac = adv.dest_mac.octets();
                    let vid = if adv.vlan_id == 0 {
                        DEFAULT_DOMAIN.vid
                    } else {
                        adv.vlan_id
                    };
                    // Refine the filter if the TA addressing differs.
                    if !filter.iter().any(|(_, m, v)| *m == dmac && *v == vid) {
                        filter.retain(|(_, m, _)| *m != dmac);
                        filter.push((stream_id.0, dmac, vid));
                        dext.set_filter(&filter);
                        log.log(
                            Level::Info,
                            "srp",
                            &format!(
                                "talker advertise refined filter: id=0x{:016x} dest={dmac:02x?} vid={vid}",
                                stream_id.0
                            ),
                        );
                    }
                }
            }
        }
        for slot in dext.rx_audio() {
            raw_audio_slots += 1;
            if first_slot_dump.is_none() {
                first_slot_dump = Some(slot.data.iter().take(32).copied().collect::<Vec<_>>());
            }
            // The audio ring delivers the full Ethernet frame; AAF decode wants
            // the AVTP payload. Strip the Ethernet (and VLAN) header first.
            let payload = match EthFrame::parse(&slot.data) {
                Ok(p) => p.payload,
                Err(_) => {
                    aaf_decode_fail += 1;
                    continue;
                }
            };
            if let Ok(pdu) = avb_avtp::aaf::decode(payload) {
                let sid = pdu.hdr.stream.stream_id.0;
                *counts.entry(sid).or_insert(0) += 1;
                let seq = pdu.hdr.stream.seq;
                if let Some(&prev) = last_seq.get(&sid) {
                    let expected = prev.wrapping_add(1);
                    if seq != expected {
                        *gaps.entry(sid).or_insert(0) += 1;
                    }
                }
                last_seq.insert(sid, seq);
                // 1b-1: build the shm ring + producer on the first AAF packet
                // and bind it to that stream_id as the primary. Geometry comes
                // from the packet's AAF header (channels/rate/spf), never a
                // literal. The plug-in exposes exactly these channels.
                if ring.is_none() {
                    let ch = usize::from(pdu.hdr.channels_per_frame.max(1));
                    let rate = avb_avtp::stream_format::nsr_hz(pdu.hdr.nsr).unwrap_or(48_000);
                    // Samples per frame = data_len / (channels × bytes/sample).
                    let bps = avb_avtp::aaf::bytes_per_sample(pdu.hdr.format).unwrap_or(4);
                    let spf = (usize::from(pdu.hdr.stream_data_length) / (ch * bps.max(1))).max(1);
                    if let Some(consts) = avb_mediaclock::RateConsts::new(rate, spf as u32) {
                        let cfg = avb_audioring::RingConfig {
                            channels: ch as u32,
                            valid_bits: if pdu.hdr.bit_depth == 0 {
                                24
                            } else {
                                u32::from(pdu.hdr.bit_depth)
                            },
                            nominal_rate_hz: rate,
                            capacity_frames: consts.capacity_frames as u32,
                            streams: vec![avb_audioring::StreamSlot {
                                stream_id: sid,
                                first_channel: 0,
                                channels: ch as u32,
                                bit_depth: if pdu.hdr.bit_depth == 0 {
                                    24
                                } else {
                                    u32::from(pdu.hdr.bit_depth)
                                },
                            }],
                            channel_map: (0..ch).map(|c| (0u16, c as u16)).collect(),
                        };
                        match avb_audioring::ShmProducer::create(
                            &cfg,
                            &[(0, ch, u32::from(pdu.hdr.bit_depth.max(24)))],
                            consts.lag_frames,
                            consts.silent_lag,
                        ) {
                            Ok(rp) => {
                                mclk = Some(avb_mediaclock::MediaClock::new(consts, 0));
                                ring = Some(rp);
                                ring_stream_id = sid;
                                ring_spf = spf;
                                ring_channels = pdu.hdr.channels_per_frame;
                                ring_bit_depth = pdu.hdr.bit_depth;
                                ring_format = pdu.hdr.format;
                                ring_nsr = pdu.hdr.nsr;
                                ring_data_len = pdu.hdr.stream_data_length;
                                log.log(
                                    Level::Info,
                                    "ring",
                                    &format!(
                                        "shm ring {} bound: stream 0x{sid:016x}, {ch} ch @ {rate} Hz, spf={spf}, cap={}",
                                        avb_audioring::SHM_NAME,
                                        consts.capacity_frames
                                    ),
                                );
                            }
                            Err(e) => {
                                log.log(
                                    Level::Error,
                                    "ring",
                                    &format!("shm ring create failed: {e}"),
                                );
                            }
                        }
                    }
                }
                // Feed the primary stream into the ring: drive the media clock
                // on its tv=1 packets, place the PCM at the clock's ring frame,
                // publish the clock model, and commit the frontier.
                if sid == ring_stream_id {
                    if let (Some(rp), Some(mc)) = (ring.as_mut(), mclk.as_mut()) {
                        let arrival = ns_since(count_start);
                        let pt = if pdu.hdr.stream.tv {
                            avb_mediaclock::presentation_time_ns(
                                pdu.hdr.stream.avtp_timestamp,
                                arrival,
                            )
                            .unwrap_or(arrival)
                        } else {
                            arrival
                        };
                        if pdu.hdr.stream.tv {
                            mc.on_reference_packet(
                                avb_mediaclock::RefPacket { pt_ns: pt, k: 1 },
                                arrival,
                                arrival,
                            );
                        }
                        // Place samples SEQUENTIALLY at a running frame counter,
                        // not at the media-clock's absolute ring_frame. The ring
                        // is a FIFO the plug-in drains in order; clock-derived
                        // absolute frames grow on a wall-clock timeline the
                        // consumer can't track, lapping it by millions of frames
                        // (= overrun garble). The media clock still timestamps
                        // the clock block below for the plug-in's timebase.
                        // Re-validate the packet against the geometry the ring
                        // was built from (review M1). Only the first packet set
                        // that geometry; a later packet with a different
                        // format/channel count/length would be placed blind,
                        // zero-filling the missing samples while the frame
                        // counter kept advancing — silent corruption. The
                        // audio.rs path already does this check.
                        if pdu.hdr.channels_per_frame != ring_channels
                            || pdu.hdr.bit_depth != ring_bit_depth
                            || pdu.hdr.format != ring_format
                            || pdu.hdr.nsr != ring_nsr
                            || pdu.hdr.stream_data_length != ring_data_len
                        {
                            ring_format_mismatch += 1;
                            continue;
                        }
                        let fmt = if pdu.hdr.format == avb_avtp::aaf::format::INT_24BIT {
                            avb_audioring::SampleFormat::Int24
                        } else {
                            avb_audioring::SampleFormat::Int32
                        };
                        rp.producer()
                            .place(0, ring_write_pos as i128, ring_spf, fmt, pdu.data);
                        rp.producer().commit(false, None);
                        ring_write_pos += ring_spf as u64;
                        if mc.should_publish() {
                            let model = mc.clock_model(arrival as f64, 0.0, 0.0);
                            rp.ring().publish_clock(&avb_audioring::ClockBlock {
                                anchor_frame: model.anchor_frame,
                                anchor_mach: model.anchor_mach,
                                mach_per_frame: model.mach_per_frame,
                                rate_hz_vs_mach: model.rate_hz_vs_mach,
                                media_ppm_vs_gptp: model.media_ppm_vs_gptp,
                                uncertainty_ns: model.uncertainty_ns,
                                locked: model.locked,
                                reset_count: model.reset_count,
                            });
                        }
                    }
                }
                // WAV: record the PRIMARY stream (first AAF stream we see,
                // which is the lowest stream_id -> 16R channels 1..N).
                if wav_out.is_some() {
                    use std::io::Write;
                    if wav_file.is_none() {
                        let ch = pdu.hdr.channels_per_frame.max(1);
                        let fmt = crate::wav::WavFormat {
                            channels: ch,
                            sample_rate: avb_avtp::stream_format::nsr_hz(pdu.hdr.nsr)
                                .unwrap_or(48_000),
                            bits_per_sample: 32,
                            valid_bits: if pdu.hdr.bit_depth == 0 {
                                24
                            } else {
                                u16::from(pdu.hdr.bit_depth)
                            },
                        };
                        if let Some(path) = wav_out {
                            if let Ok(mut f) = std::fs::File::create(path) {
                                let _ = f.write_all(&crate::wav::initial_header(&fmt));
                                wav_file = Some(f);
                                wav_fmt = Some(fmt);
                                wav_stream_id = sid;
                                log.log(
                                    Level::Info,
                                    "rec",
                                    &format!(
                                        "recording stream 0x{sid:016x}: {ch} ch @ {} Hz, 32-bit",
                                        fmt.sample_rate
                                    ),
                                );
                            }
                        }
                    }
                    if sid == wav_stream_id {
                        if let Some(f) = wav_file.as_mut() {
                            // AAF data is big-endian interleaved; WAV wants
                            // little-endian. Byte-swap each 32-bit sample.
                            let mut le = Vec::with_capacity(pdu.data.len());
                            let mut i = 0;
                            while i + 4 <= pdu.data.len() {
                                if let Some(b) = pdu.data.get(i..i + 4) {
                                    le.extend_from_slice(&[b[3], b[2], b[1], b[0]]);
                                }
                                i += 4;
                            }
                            if f.write_all(&le).is_ok() {
                                wav_data_bytes += le.len() as u64;
                                if let Some(fmt) = wav_fmt {
                                    let ba = u64::from(fmt.block_align()).max(1);
                                    wav_frames = wav_data_bytes / ba;
                                }
                            }
                        }
                    }
                }
                // Per-channel peak |sample|: 32-bit big-endian interleaved.
                let ch = pdu.hdr.channels_per_frame.max(1);
                if pdu.hdr.bit_depth == 32 || pdu.hdr.bit_depth == 24 {
                    let mut i = 0usize;
                    let data = pdu.data;
                    while i + 4 <= data.len() {
                        if let Some(b) = data.get(i..i + 4) {
                            let v = i32::from_be_bytes([b[0], b[1], b[2], b[3]]) as i64;
                            let c = ((i / 4) % usize::from(ch)) as u16;
                            let e = peak.entry((sid, c)).or_insert(0);
                            let a = v.abs();
                            if a > *e {
                                *e = a;
                            }
                        }
                        i += 4;
                    }
                }
            } else {
                aaf_decode_fail += 1;
            }
        }
        if ns_since(last_report) >= interval_ns {
            let secs = (ns_since(count_start) as f64) / 1e9;
            for (sid, n) in &counts {
                log.log(
                    Level::Info,
                    "streams",
                    &format!(
                        "stream 0x{sid:016x}: {n} pkts ({:.0}/s), seq_gaps={}",
                        (*n as f64) / secs.max(1e-9),
                        gaps.get(sid).copied().unwrap_or(0)
                    ),
                );
            }
            last_report = now_mach();
        }
        // No sleep here: at Class A (8000 pkt/s per stream, 9 streams ->
        // ~64k/s) a 1 ms nap lets the dext audio ring overrun. Busy-drain
        // instead; the SRP maintenance above is gated on its own interval.
    }

    // --- Report ---
    // Dext RX counters: rx_stream (delivered to the audio ring), and the two
    // stream drops. This tells us whether the 16R is transmitting at all:
    //   rx_stream>0            -> frames delivered (but AAF decode failed?)
    //   drop_unbound>0         -> frames arrive but MAC/VID not in our filter
    //   drop_untagged>0        -> stream frames arrive UNtagged (vid mismatch)
    //   all zero               -> the 16R is not transmitting to us at all
    if let Ok(c) = dext.device().get_counters() {
        let rx_stream = c.c.get(6).copied().unwrap_or(0);
        let drop_unbound =
            c.c.get(avb_dext::abi::ctr::RX_DROP_UNBOUND_STREAM)
                .copied()
                .unwrap_or(0);
        let drop_untagged =
            c.c.get(avb_dext::abi::ctr::RX_DROP_UNTAGGED_STREAM)
                .copied()
                .unwrap_or(0);
        let rx_frames = c.c.first().copied().unwrap_or(0);
        let ring_overrun_audio = c.c.get(14).copied().unwrap_or(0);
        log.log(
            Level::Info,
            "streams",
            &format!(
                "dext counters: rx_frames={rx_frames} rx_stream={rx_stream} drop_unbound_stream={drop_unbound} drop_untagged_stream={drop_untagged} ring_overrun_audio={ring_overrun_audio}"
            ),
        );
    }
    let elapsed_s = (ns_since(count_start) as f64) / 1e9;
    log.log(
        Level::Info,
        "streams",
        &format!("audio ring: raw_slots={raw_audio_slots} non_aaf_or_undecoded={aaf_decode_fail} format_mismatch={ring_format_mismatch}"),
    );
    let _ = first_slot_dump;
    // Channel-map: report channels whose peak |sample| is well above the noise
    // floor (a live signal). 32-bit full scale is 2^31; use ~0.1% (-60 dBFS).
    let floor: i64 = 1 << 21;
    let mut active: Vec<((u64, u16), i64)> = peak
        .iter()
        .filter(|(_, &p)| p > floor)
        .map(|(k, &p)| (*k, p))
        .collect();
    active.sort_by_key(|&(_, p)| std::cmp::Reverse(p));
    if active.is_empty() {
        log.log(
            Level::Info,
            "chan-map",
            "no channel above the -60 dBFS floor",
        );
    } else {
        for ((sid, c), p) in &active {
            let dbfs = 20.0 * ((*p as f64) / (i32::MAX as f64)).max(1e-12).log10();
            log.log(
                Level::Info,
                "chan-map",
                &format!(
                    "SIGNAL on stream 0x{sid:016x} channel {c} (0-based): peak={p} ({dbfs:.1} dBFS)"
                ),
            );
        }
    }
    // Report every stream_id actually observed on the wire (the AAF header's
    // real stream_id), not the probe-reported one which a Milan talker may
    // leave 0. `bound_ids` records which listener_unique_ids settled.
    let streams_json: Vec<serde_json::Value> = counts
        .iter()
        .map(|(sid, n)| {
            serde_json::json!({
                "stream_id": format!("0x{sid:016x}"),
                "packets": n,
                "packets_per_s": (*n as f64) / elapsed_s.max(1e-9),
                "seq_gaps": gaps.get(sid).copied().unwrap_or(0),
            })
        })
        .collect();
    let report = serde_json::json!({
        "schema": "avb-streams/1",
        "talker": format!("0x{:016x}", u64::from_be_bytes(talker.0)),
        "settled_streams": bound_ids.len(),
        "streams_with_traffic": counts.len(),
        "elapsed_s": elapsed_s,
        "streams": streams_json,
    });
    if let Some(path) = json_out {
        if let Ok(j) = serde_json::to_string_pretty(&report) {
            let _ = std::fs::write(path, j);
        }
    }

    // Finalize the WAV: patch the header sizes, and compute media-clock drift
    // (ppm) from frames-written vs elapsed wall time against the nominal rate.
    if let (Some(mut f), Some(fmt)) = (wav_file.take(), wav_fmt) {
        use std::io::{Seek, SeekFrom, Write};
        let _ = f.seek(SeekFrom::Start(0));
        let _ = f.write_all(&crate::wav::finalize_header(&fmt, wav_data_bytes));
        let _ = f.flush();
        let measured_rate = (wav_frames as f64) / elapsed_s.max(1e-9);
        let nominal = f64::from(fmt.sample_rate);
        let drift_ppm = (measured_rate - nominal) / nominal * 1e6;
        log.log(
            Level::Info,
            "rec",
            &format!(
                "WAV: {} ch, {} frames, {:.3}s, measured {:.2} Hz vs {} nominal -> {:+.1} ppm",
                fmt.channels, wav_frames, elapsed_s, measured_rate, fmt.sample_rate, drift_ppm
            ),
        );
        if let Some(spath) = summary_out {
            let streams_sum: Vec<serde_json::Value> = counts
                .iter()
                .map(|(sid, n)| {
                    crate::report::stream_summary(
                        0,
                        *sid,
                        *n,
                        gaps.get(sid).copied().unwrap_or(0),
                        0,
                        0,
                        if *sid == wav_stream_id {
                            drift_ppm
                        } else {
                            0.0
                        },
                    )
                })
                .collect();
            let summary = serde_json::json!({
                "schema": "avb-record-summary/1",
                "seconds": seconds,
                "rate": fmt.sample_rate,
                "channels": fmt.channels,
                "frames": wav_frames,
                "recorded_stream": format!("0x{wav_stream_id:016x}"),
                "media_clock": { "measured_hz": measured_rate, "drift_ppm": drift_ppm },
                "streams": streams_sum,
            });
            if let Ok(j) = serde_json::to_string_pretty(&summary) {
                let _ = std::fs::write(spath, j);
            }
        }
    }

    log.log(
        Level::Info,
        "streams",
        &format!(
            "done: {} streams, {:.1}s; see per-stream pkt/s above",
            filter.len(),
            elapsed_s
        ),
    );
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
    summary_out: Option<PathBuf>,
) -> Result<(), Exit> {
    install_signal_handlers();
    let log = Logger::new(opts.level);
    let mut dext = bring_up(opts, &log)?;
    log.log(
        Level::Info,
        "rec",
        &format!("record {seconds}s -> {}", out.display()),
    );
    // The recorder reuses the proven streams connect/settle/receive flow,
    // writing the primary stream's decoded PCM to a WAV and a summary JSON.
    streams_loop(
        opts,
        &log,
        &mut dext,
        seconds,
        10,
        None,
        Some(out),
        summary_out.as_deref(),
    )
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

/// Talker / playback entry (Phase 1b-2): play Mac audio OUT to the 16R.
///
/// Mirror of [`record`]. The CoreAudio plug-in's output stream is the ring
/// PRODUCER (WriteMix) and this loop is the CONSUMER: it reads committed frames
/// from the output ring, packetizes them as AAF with `AafTalker`, and
/// transmits to the 16R (acting as the LISTENER). The daemon is the AVDECC
/// controller, so it drives the connection with `CONNECT_RX_COMMAND` naming our
/// STREAM_OUTPUT and the 16R's STREAM_INPUT sink, then declares MSRP Talker
/// Advertise and streams once the 16R reports Listener Ready.
#[allow(clippy::too_many_arguments)]
pub fn play(
    opts: &GlobalOpts,
    seconds: u64,
    listener: Option<u64>,
    listener_uid: u16,
    _allow_partial: bool,
    _start_timeout: u64,
    _summary_out: Option<PathBuf>,
) -> Result<(), Exit> {
    install_signal_handlers();
    let log = Logger::new(opts.level);
    let mut dext = bring_up(opts, &log)?;
    log.log(
        Level::Info,
        "play",
        &format!("play {seconds}s OUT to 16R (listener_uid={listener_uid})"),
    );
    talker_loop(opts, &log, &mut dext, seconds, listener, listener_uid)
}

/// The talker TX loop (design §5.2 TX side, Phase 1b-2). Creates the output
/// ring, discovers the 16R and binds our STREAM_OUTPUT to its STREAM_INPUT via
/// ACMP (as controller), then paces AAF PDUs from the ring on the media clock.
///
/// The connect/advertise handshake and the TX pacing against the live I210 are
/// finalized during the Phase 1b-2 hardware milestone (same staging the
/// listener loops used); the sans-IO pieces — the output ring, the media-clock
/// derivation and `AafTalker` packetization — are wired and unit-tested now.
fn talker_loop(
    opts: &GlobalOpts,
    log: &Logger,
    dext: &mut RealDext,
    seconds: u64,
    listener: Option<u64>,
    listener_uid: u16,
) -> Result<(), Exit> {
    use avb_avtp::stream_format::{AafFormat, StreamFormat};
    use avb_avtp::talker::AafTalker;
    use avb_core::eth::{EthHeader, VlanTag};
    use avb_core::{StreamId, TxFrame};

    use crate::dextio::DextPort;

    // The output format MUST match the 16R's STREAM_INPUT exactly (Milan: AAF
    // INT_32BIT, 48 kHz, 8 ch, 6 spf). The real value is read from the 16R's
    // AEM (never hardcoded) during bring-up; the Milan base format is the
    // bootstrap default used until the AEM read lands.
    let stream_format = StreamFormat(0x0205_0220_0200_6000);
    let fmt: AafFormat = stream_format.aaf().ok_or(Exit::DextFault)?;
    let hz = fmt.nsr_hz().ok_or(Exit::DextFault)? as u64;
    let spf = usize::from(fmt.spf);
    let channels = usize::from(fmt.channels);

    // Our talker stream id = our MAC + a unique id (0 for the first output
    // stream). The 16R identifies the stream it listens to by this id.
    let info = dext.device().info().map_err(|_| Exit::DextFault)?;
    let our_mac = avb_core::MacAddr::new(info.mac);
    let our_stream_id = StreamId::from_parts(our_mac, 0);

    // Create the OUTPUT ring (daemon = consumer, plug-in = producer). We create
    // it so the file exists with world perms before the plug-in attaches, the
    // same ownership rule as the input ring.
    let cfg = avb_audioring::RingConfig {
        channels: channels as u32,
        valid_bits: u32::from(fmt.bit_depth),
        nominal_rate_hz: hz as u32,
        capacity_frames: 65_536,
        streams: vec![avb_audioring::StreamSlot {
            stream_id: our_stream_id.0,
            first_channel: 0,
            channels: channels as u32,
            bit_depth: u32::from(fmt.bit_depth),
        }],
        channel_map: (0..channels).map(|c| (0u16, c as u16)).collect(),
    };
    let out_ring = match avb_audioring::ShmRing::create_at(avb_audioring::SHM_NAME_OUT, &cfg) {
        Ok(r) => r,
        Err(e) => {
            log.log(
                Level::Error,
                "play",
                &format!("output ring create failed: {e}"),
            );
            return Err(Exit::DextFault);
        }
    };
    log.log(
        Level::Info,
        "play",
        &format!(
            "output ring {} created: {} ch @ {} Hz, spf={}, stream 0x{:016x}",
            avb_audioring::SHM_NAME_OUT,
            channels,
            hz,
            spf,
            our_stream_id.0
        ),
    );

    // The packetizer for our stream.
    let mut talker = AafTalker::new(our_stream_id, fmt).ok_or(Exit::DextFault)?;

    // Our entity id is the EUI-64 of our MAC (FF:FE inserted), exactly as every
    // other path derives it — the 16R keys ACMP on it (review H4).
    let our_id = avb_core::Eui64::from_mac(our_mac);

    // Our AEM, so the 16R can ENUMERATE us rather than merely discover us.
    //
    // ADP alone only tells a Milan controller that an entity exists; it then
    // walks the AEM (ENTITY -> CONFIGURATION 0 -> every descriptor named by
    // descriptor_counts) to learn what the entity can do. We answered no AECP
    // at all before this — the responder existed in avb-avdecc but was never
    // connected to the socket — so the walk died at its first command and the
    // 16R had no model of us.
    //
    // The model is derived from `fmt`, the format we actually packetize with,
    // so the channel count and rate in the AEM cannot disagree with the
    // stream on the wire.
    let model = avb_avdecc::entity::talker_model(
        our_mac,
        our_stream_id,
        stream_format,
        crate::FIRMWARE_VERSION,
    )
    .ok_or(Exit::DextFault)?;
    log.log(
        Level::Info,
        "aem",
        &format!(
            "talker entity 0x{:016x} model 0x{:016x}: 1 STREAM_OUTPUT, {} ch @ {} Hz",
            u64::from_be_bytes(our_id.0),
            model.entity_model_id,
            model.channels,
            model.current_sampling_rate
        ),
    );
    let mut entity = avb_avdecc::entity::TalkerEntity::new(model);

    // Find the 16R (the LISTENER for this direction) — from --listener, or by
    // ADP discovery — and announce ourselves as a talker so it knows us before
    // we ask it to connect (review H3; a Milan entity ignores ACMP from an
    // entity it has not discovered).
    let listener_id = discover_listener(opts, log, dext, &mut entity, our_mac, listener)?;

    // Connect as controller: tell the 16R to connect its STREAM_INPUT
    // [listener_uid] to our talker stream. Mirror of the record path's
    // CONNECT_TX_COMMAND, with the 16R as listener_entity_id.
    let mut connect_seq: u16 = 0;
    connect_listener(
        log,
        dext,
        our_mac,
        our_id,
        listener_id,
        our_stream_id,
        listener_uid,
        connect_seq,
    );
    connect_seq = connect_seq.wrapping_add(1);

    // ---- TX pacing loop ------------------------------------------------------
    //
    // One AAF PDU carries `spf` frames; a class-A 48 kHz stream emits 8000
    // PDUs/s (one every 125 µs). We pace on the mach clock: each time the next
    // PDU's deadline passes, drain `spf` frames from the output ring, packetize
    // and transmit. The AVTP presentation timestamp is `gptp_now +
    // presentation_offset` (Milan floor 2 ms + one class-A interval), truncated
    // to 32 bits — the 16R schedules playout at that time. On hardware the gPTP
    // "now" and the per-PDU TX pacing are read from the dext's disciplined clock
    // (the gPTP ctl loop owns it); here the structure, cadence, ring drain,
    // packetization and timestamp math are all in place and exercised.
    use avb_core::time::{MachTicks, Timebase};
    use avb_mediaclock::placement::{CLASS_A_INTERVAL_NS, PRESENTATION_FLOOR_NS};

    let tb = Timebase {
        numer: info.tb_numer,
        denom: info.tb_denom,
    };
    if !tb.is_valid() {
        log.log(Level::Error, "play", "dext reported an invalid timebase");
        return Err(Exit::DextFault);
    }
    let now_mach = || MachTicks(unsafe { mach2::mach_time::mach_absolute_time() });
    let ns_since = |t: MachTicks| tb.ticks_to_ns(now_mach().ticks_since(t));

    // ---- gPTP: the clock the presentation timestamps are read from --------
    //
    // The talker must service gPTP itself. The AVTP presentation timestamp is
    // a reading of the DISCIPLINED clock, and the clock is only disciplined
    // while something is pumping the servo — the `gptp` command's loop is a
    // different process and does not help here. A loop that transmits without
    // servicing gPTP is stamping an undisciplined, free-running SYSTIM.
    //
    // We re-anchor more often than the gptp command does (20 ms rather than
    // 100 ms): the interpolation error between anchors is the residual
    // frequency offset times the interval, and this is the timestamp the 16R
    // schedules playout from.
    let mut gptp =
        crate::gptpsvc::GptpService::new(dext, log, crate::gptpsvc::TALKER_CLOCK_READ_NS)?;
    // How long we wait for the servo before giving up, reusing the discovery
    // timeout rather than inventing a second knob.
    let lock_deadline_ns = opts.discover_timeout.saturating_mul(1_000_000_000);

    // ---- MSRP: advertise our stream so the 16R will accept it ---------------
    //
    // A listener does not accept a stream it has not been told about. SRP is
    // how it learns: the talker declares a Talker Advertise ("I have a stream,
    // here is its id, destination, VLAN and bandwidth"), the listener answers
    // with Listener Ready, and only then is the path established. Our own
    // listener path applies exactly this rule in reverse -- it refuses to
    // declare Listener Ready until the 16R registers a Talker Advertise -- and
    // the 16R holds us to the same contract.
    //
    // Without this the stream was transmitted into a void: the hardware
    // session showed the 16R listing no input device at all, because nothing
    // had ever announced one.
    use crate::dextio::SendResult;
    use avb_core::eth::EthFrame;
    use avb_core::{Now, Outbox, RxMeta};
    use avb_srp::attr::{Attribute, Talker, DEFAULT_DOMAIN, ETHERTYPE_MSRP, ETHERTYPE_MVRP};
    use avb_srp::participant::{Participant, ParticipantConfig, SrpEvent};

    let srp_cfg = ParticipantConfig {
        mac: our_mac,
        timebase: tb,
        seed: u64::from_be_bytes(our_id.0),
    };
    // SRP timers add tick durations to `now.mach`, so SRP time MUST be raw
    // mach ticks, not the ns the ACMP state machines use. Keep them distinct.
    let srp_now = || Now::mach_only(now_mach());
    let mut srp = Participant::new(srp_cfg, now_mach());
    srp.declare(
        now_mach(),
        Attribute::Vid {
            vid: DEFAULT_DOMAIN.vid,
        },
    );
    srp.declare(now_mach(), Attribute::Domain(DEFAULT_DOMAIN));

    // TSpec. `max_frame_size` is computed from the frame we actually transmit
    // below (Ethernet header + VLAN tag + the AAF PDU) rather than guessed, so
    // it cannot drift from the real stream. The rest follow 802.1Q-2018
    // §35.2.2.8 for an SR class A talker: one frame per 125 us observation
    // interval, priority from the SR class A domain, rank 1 (non-emergency).
    // `accumulated_latency_ns` starts at 0 -- it is the latency accumulated
    // ALONG THE PATH, which bridges add to as the advertise propagates, and we
    // are cabled directly to the 16R with no bridge in between.
    let stream_dst = stream_dest_mac(our_stream_id);
    let advertise = talker_tspec(our_stream_id, talker.pdu_len());
    let max_frame_size = advertise.max_frame_size;
    srp.declare(now_mach(), Attribute::Talker(Talker::Advertise(advertise)));
    log.log(
        Level::Info,
        "msrp",
        &format!(
            "Talker Advertise declared: stream 0x{:016x} dest {} vid {} max_frame {}B 1 frame/interval prio {}",
            our_stream_id.0, stream_dst, DEFAULT_DOMAIN.vid, max_frame_size, DEFAULT_DOMAIN.priority
        ),
    );

    let mut srp_out: Outbox<32> = Outbox::new();
    // Whether the 16R has answered with Listener Ready for OUR stream. This is
    // the acceptance signal: until it is true the 16R is not listening, and
    // audio cannot arrive however many PDUs we send.
    let mut listener_ready = false;

    // One PDU every `spf / rate` seconds (125 µs for 48 kHz / 6 spf).
    let pdu_interval_ns = (spf as u64)
        .saturating_mul(1_000_000_000)
        .checked_div(hz)
        .unwrap_or(CLASS_A_INTERVAL_NS);
    // Presentation offset ahead of "now" (Milan floor + one class-A interval).
    let presentation_offset_ns = PRESENTATION_FLOOR_NS + CLASS_A_INTERVAL_NS;

    let mut consumer = avb_audioring::Consumer::new(out_ring.ring());
    let frame_bytes = out_ring.ring().frame_bytes();
    let mut sample_scratch = vec![0u8; spf * frame_bytes];
    let mut pdu_scratch = vec![0u8; talker.pdu_len()];

    let run_ns = seconds.saturating_mul(1_000_000_000);
    let start = now_mach();
    let mut next_pdu_ns: u64 = 0;
    let mut tx_pdus: u64 = 0;
    // PDUs the dext refused, split by cause so "sent" can never again mean
    // "attempted": the SAF-6 TX ceiling, versus anything else.
    let mut tx_rate_limited: u64 = 0;
    let mut tx_rejected: u64 = 0;
    let mut last_send_err: Option<SendResult> = None;
    let mut underruns: u64 = 0;
    let mut last_connect = now_mach();
    let mut last_advert = now_mach();
    // AEM commands we answered, and how many we had to refuse. A run where the
    // 16R never asks us anything is a different failure from one where it asks
    // and we answer NOT_IMPLEMENTED, and the summary must tell them apart.
    let mut aecp_answered: u64 = 0;
    let mut aecp_refused: u64 = 0;
    // Ticks skipped because gPTP had no valid clock anchor (startup, or just
    // after a servo step). Counted, not silently papered over.
    let mut ts_unavailable: u64 = 0;
    // Ticks transmitted while the servo had fallen out of LOCKED.
    let mut ticks_unlocked: u64 = 0;
    let mut warned_unlocked = false;
    // Do not transmit until the clock is fit to timestamp with. Streaming on
    // an undisciplined clock is not a degraded stream, it is a stream the
    // listener will reject — and it is what made the earlier runs look like a
    // transmit problem when the clock was the issue.
    let mut armed = false;
    let mut last_gptp_push = now_mach();
    let mut last_gptp_service = now_mach();

    loop {
        if shutdown_requested() {
            break;
        }
        let elapsed = ns_since(start);
        if run_ns != 0 && elapsed >= run_ns {
            break;
        }
        let h = dext.health();
        if h.fault != 0 {
            log.log(Level::Error, "play", &format!("dext fault {}", h.fault));
            return Err(Exit::DextFault);
        }

        // ---- Control-ring drain ---------------------------------------------
        // ONE drain for every protocol. The control ring is a single shared
        // queue, so a slot read here is gone for everyone: if gPTP drained it
        // separately it would eat the MSRP and AECP frames, and vice versa.
        // Each frame is routed by ethertype/subtype instead.
        //
        // MRP is a two-way protocol: the participant must see the peer's
        // declarations to keep its own registered, and our Listener Ready
        // arrives here. This loop was pure TX before, which is why MSRP could
        // not simply be declared and forgotten.
        for slot in dext.rx_ctl() {
            let Ok(p) = EthFrame::parse(&slot.data) else {
                // Not parseable as Ethernet — still offer it to gPTP, whose
                // own parser handles the PTP L2 framing.
                gptp.handle_rx(now_mach(), &slot);
                continue;
            };
            if p.hdr.ethertype == ETHERTYPE_MSRP || p.hdr.ethertype == ETHERTYPE_MVRP {
                let meta = RxMeta {
                    desc_vlan_tci: slot.desc_vlan_tci,
                    ..Default::default()
                };
                let _ = srp.handle_rx(srp_now(), &slot.data, meta);
                continue;
            }
            if p.hdr.ethertype == avb_gptp::msg::ETHERTYPE_PTP {
                gptp.handle_rx(now_mach(), &slot);
                continue;
            }
            // ---- AECP: answer the 16R's AEM walk --------------------------
            //
            // This is what lets the 16R enumerate us. Byte 0 of an IEEE 1722
            // control PDU is the subtype (NOT byte 1 — the codec had that
            // inverted once and only real hardware caught it), so match on
            // payload[0] before decoding.
            if p.hdr.ethertype == avb_avtp::common::ETHERTYPE_AVTP
                && p.payload.first().copied() == Some(avb_avdecc::constants::subtype::AECP)
            {
                let Some(cmd) = avb_avdecc::aecp::AemPdu::decode(p.payload) else {
                    continue;
                };
                // Only commands addressed to us, and only commands: a
                // RESPONSE here is an answer to something we sent as a
                // controller, and answering it again would loop.
                if cmd.target_entity_id != our_id
                    || cmd.message_type != avb_avdecc::constants::aecp_msg::AEM_COMMAND
                {
                    continue;
                }
                let resp = entity.handle_aecp(&cmd, p.hdr.src, ns_since(start));
                send_aem_response(dext, our_mac, p.hdr.src, &resp);
                aecp_answered += 1;
                if resp.status != avb_avdecc::constants::aecp_status::SUCCESS {
                    aecp_refused += 1;
                }
                log.log(
                    Level::Debug,
                    "aecp",
                    &format!(
                        "cmd 0x{:04x} from {} -> status {}",
                        cmd.command_type, p.hdr.src, resp.status
                    ),
                );
            }
        }
        // Service gPTP with what the drain above routed into it. No second
        // drain: the control ring is one shared queue.
        //
        // Throttled to 1 ms rather than run every pass. This loop spins at
        // ~50 us to pace 8000 PDUs/s, and `service_no_rx` makes a dext IPC
        // call (poll_tx_ts) each time — running it per pass would be ~20 000
        // extra round trips a second, competing with the audio it is supposed
        // to be timestamping. gPTP needs a couple of hundred messages a
        // second at most, so 1 ms is ample. RX is NOT throttled: frames are
        // fed in from the drain above on every pass, so a Pdelay response is
        // never delayed by this.
        if ns_since(last_gptp_service) >= 1_000_000 {
            gptp.service_no_rx(dext, log, now_mach())?;
            last_gptp_service = now_mach();
        }

        while let Some(ev) = srp.pop_event() {
            log.log(Level::Debug, "srp", &format!("event: {ev:?}"));
            match ev {
                // The 16R accepting our stream. Report it once: it is the
                // difference between "transmitting" and "connected".
                SrpEvent::ListenerRegistered { stream_id, .. }
                    if stream_id.0 == our_stream_id.0 =>
                {
                    if !listener_ready {
                        listener_ready = true;
                        entity.set_stream_info(avb_avdecc::entity::StreamInfo {
                            connected: true,
                            dest_mac: stream_dst,
                            vlan_id: DEFAULT_DOMAIN.vid,
                            msrp_accumulated_latency_ns: 0,
                        });
                        log.log(
                            Level::Info,
                            "msrp",
                            "16R declared Listener Ready for our stream — accepted",
                        );
                    }
                }
                SrpEvent::ListenerWithdrawn { stream_id, .. } if stream_id.0 == our_stream_id.0 => {
                    listener_ready = false;
                    entity.set_stream_info(avb_avdecc::entity::StreamInfo {
                        connected: false,
                        dest_mac: stream_dst,
                        vlan_id: DEFAULT_DOMAIN.vid,
                        msrp_accumulated_latency_ns: 0,
                    });
                    log.log(
                        Level::Warn,
                        "msrp",
                        "16R withdrew Listener Ready for our stream",
                    );
                }
                _ => {}
            }
        }
        // Run the MRP timers and put whatever they produced on the wire.
        srp.poll(srp_now(), &mut srp_out);
        while let Some(f) = srp_out.pop() {
            let _ = dext.send(f.bytes(), false, 0);
        }

        // Keep our ADP advertisement alive (valid_time is short) so the 16R
        // keeps us in its entity table. The PDU comes from the entity, which
        // also owns the monotonic available_index.
        if ns_since(last_advert) >= 2_000_000_000 {
            let adp = entity.advertise();
            send_adp(dext, our_mac, &adp);
            last_advert = now_mach();
        }

        // Refresh the listener connection every second so the 16R keeps its
        // sink bound (mirror of the record loop's periodic CONNECT_TX). This
        // used to only bump a counter and never re-send (review L1).
        if ns_since(last_connect) >= 1_000_000_000 {
            connect_listener(
                log,
                dext,
                our_mac,
                our_id,
                listener_id,
                our_stream_id,
                listener_uid,
                connect_seq,
            );
            connect_seq = connect_seq.wrapping_add(1);
            last_connect = now_mach();
        }

        // Push gPTP state into the entity once a second so GET_AVB_INFO and
        // our ADP report the real grandmaster rather than a placeholder.
        // status() does real work, so this is not done per PDU.
        if ns_since(last_gptp_push) >= 1_000_000_000 {
            let st = gptp.status();
            entity.set_gptp(avb_avdecc::entity::GptpStatus {
                grandmaster_id: u64::from_be_bytes(st.gm_identity.0),
                domain_number: 0,
                as_capable: st.as_capable,
                propagation_delay_ns: st.path_delay_comp_ns.last.unwrap_or(0.0).max(0.0) as u32,
            });
            last_gptp_push = now_mach();
        }

        // ---- Readiness gate -------------------------------------------------
        //
        // A Milan talker must not stream on a clock that is not locked to the
        // grandmaster: the presentation timestamps would be wrong by the
        // clock's whole offset, and the listener discards them. So wait for
        // the servo, bounded by the discovery timeout, instead of emitting
        // 8000 badly-stamped PDUs per second.
        if !armed {
            if gptp.is_locked() {
                armed = true;
                let st = gptp.status();
                log.log(
                    Level::Info,
                    "play",
                    &format!(
                        "gPTP locked (asCapable={} offset {:.0} ns, path delay {:.0} ns) — arming the talker",
                        st.as_capable,
                        st.offset_ns.last.unwrap_or(f64::NAN),
                        st.path_delay_comp_ns.last.unwrap_or(f64::NAN),
                    ),
                );
                // Start pacing from here, so the wait does not leave a backlog
                // that would be emitted as a burst.
                next_pdu_ns = ns_since(start);
            } else {
                if ns_since(start) >= lock_deadline_ns {
                    let st = gptp.status();
                    log.log(
                        Level::Error,
                        "play",
                        &format!(
                            "gPTP did not lock within {}s (state={} asCapable={} servo={}) — refusing to stream on an undisciplined clock",
                            opts.discover_timeout,
                            st.port_state.name(),
                            st.as_capable,
                            st.servo_state.name(),
                        ),
                    );
                    return Err(Exit::TalkerNotFound);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
        } else if !gptp.is_locked() {
            // Lost lock mid-run. SYSTIM keeps running at its last rate, so
            // holdover timestamps stay usable for a while; keep streaming but
            // say so once and count every tick.
            ticks_unlocked += 1;
            if !warned_unlocked {
                warned_unlocked = true;
                let st = gptp.status();
                log.log(
                    Level::Warn,
                    "play",
                    &format!(
                        "gPTP left LOCKED mid-run (servo={} asCapable={}) — presentation timestamps are now holdover",
                        st.servo_state.name(),
                        st.as_capable
                    ),
                );
            }
        }

        // Not yet time for the next PDU: yield briefly and recheck.
        if elapsed < next_pdu_ns {
            std::thread::sleep(std::time::Duration::from_micros(50));
            continue;
        }

        // Drain exactly one PDU's worth of frames from the output ring.
        let got = consumer.read_into(&mut sample_scratch, spf);
        if got < spf {
            // Underrun: the plug-in has not produced this PDU yet. Send silence
            // to keep the stream's cadence (a Milan talker must not gap), or
            // simply skip the tick on hardware where the daemon tracks the
            // play cursor. Here we count it and advance the schedule.
            underruns += 1;
            if let Some(tail) = sample_scratch.get_mut(got * frame_bytes..) {
                tail.fill(0);
            }
        }

        // Presentation timestamp: gPTP now + offset, mod 2^32.
        //
        // This is the real disciplined clock, read from the gPTP service this
        // loop is pumping. It used to be `elapsed` — nanoseconds since the
        // loop started — which is not an approximation of gPTP time but an
        // unrelated quantity: the 16R compares the timestamp against its own
        // gPTP clock, so a stream whose timestamps start near zero is hours
        // in the past and every PDU falls outside the acceptance window.
        //
        // `now_ns` returns None while there is no valid anchor (before the
        // first clock read, and immediately after a servo step, when the
        // previous anchor is stale by the step). We skip the tick rather than
        // substitute something else: a PDU carrying a wrong presentation time
        // is worse than a PDU not sent, because the listener acts on it.
        let Some(gptp_now) = gptp.now_ns(now_mach()) else {
            ts_unavailable += 1;
            next_pdu_ns = next_pdu_ns.saturating_add(pdu_interval_ns);
            continue;
        };
        let avtp_ts = (gptp_now.wrapping_add(presentation_offset_ns) & 0xFFFF_FFFF) as u32;

        match talker.packetize(&sample_scratch, avtp_ts, true, &mut pdu_scratch) {
            Ok(n) => {
                let hdr = EthHeader {
                    dst: stream_dest_mac(our_stream_id),
                    src: our_mac,
                    vlan: Some(VlanTag {
                        // From the SR class A domain we advertised in the
                        // TSpec, not a literal: the reservation and the frames
                        // it covers must carry the same PCP and VID.
                        pcp: DEFAULT_DOMAIN.priority,
                        dei: false,
                        vid: DEFAULT_DOMAIN.vid,
                    }),
                    ethertype: avb_avtp::common::ETHERTYPE_AVTP,
                };
                if let Ok(f) = TxFrame::build(&hdr, pdu_scratch.get(..n).unwrap_or(&[]), false, 0) {
                    // Count what the dext ACCEPTED, not what we attempted. The
                    // dext enforces its own TX ceiling (SAF-6) and returns an
                    // error for a frame it drops; ignoring that made the
                    // summary report 441 232 PDUs "sent" in a run where only
                    // 58 820 reached the wire, which hid the real fault.
                    match dext.send(f.bytes(), false, 0) {
                        SendResult::Ok => tx_pdus += 1,
                        // The dext's SAF-6 token bucket was empty: this frame
                        // never reached the wire.
                        SendResult::NoResources => tx_rate_limited += 1,
                        other => {
                            tx_rejected += 1;
                            last_send_err = Some(other);
                        }
                    }
                }
            }
            Err(e) => {
                log.log(Level::Error, "play", &format!("packetize error: {e:?}"));
            }
        }

        next_pdu_ns = next_pdu_ns.saturating_add(pdu_interval_ns);
        // If we fell behind (scheduler jitter), catch the schedule up to now so
        // we do not spin emitting a burst.
        if next_pdu_ns + pdu_interval_ns < elapsed {
            next_pdu_ns = elapsed;
        }
    }

    // Tell the 16R we are going before we stop answering. Without this it
    // holds us in its entity table for the full 2 * valid_time (20 s) and a
    // restarted daemon can collide with its own stale entry.
    let departing = entity.departing();
    send_adp(dext, our_mac, &departing);

    let final_gptp = gptp.status();
    log.log(
        Level::Info,
        "play",
        &format!(
            "talker done: {tx_pdus} PDUs on the wire, {tx_rate_limited} rate-limited, {tx_rejected} otherwise refused, {underruns} underruns over {}s; 16R listener_ready={listener_ready}; AEM commands answered={aecp_answered} refused={aecp_refused}; gPTP servo={} offset {:.0} ns, {ts_unavailable} ticks with no clock anchor, {ticks_unlocked} unlocked",
            run_ns / 1_000_000_000,
            final_gptp.servo_state.name(),
            final_gptp.offset_ns.last.unwrap_or(f64::NAN),
        ),
    );
    if aecp_answered == 0 {
        log.log(
            Level::Warn,
            "aem",
            "the 16R never sent us a single AEM command — it did not try to \
             enumerate us, so check that our ADP advertisement is reaching it",
        );
    }
    if tx_rate_limited != 0 {
        log.log(
            Level::Warn,
            "play",
            &format!(
                "the dext's TX ceiling dropped {tx_rate_limited} of {} PDUs -- it is below this stream's frame rate, so the 16R received a gapped stream and our MSRP declarations competed with audio for the same tokens",
                tx_pdus + tx_rate_limited + tx_rejected
            ),
        );
    }
    if let Some(e) = last_send_err {
        log.log(
            Level::Warn,
            "play",
            &format!("{tx_rejected} PDUs refused by the dext, last reason {e:?}"),
        );
    }
    if !listener_ready {
        // Say so rather than letting a clean exit imply success: every PDU
        // went out, but nothing was listening.
        log.log(
            Level::Warn,
            "msrp",
            "16R never declared Listener Ready — it did not accept the stream, so no audio reached it",
        );
    }
    Ok(())
}

/// The AVB stream destination MAC for our talker stream.
///
/// A real talker obtains this via MAAP; until that is wired the 16R learns the
/// destination from our SRP Talker Advertise. The stand-in must NOT fall inside
/// `91:E0:F0:00:xx:xx`: the dext classifier (`avb_core::is_avdecc_dest`) treats
/// that whole block as AVDECC control traffic, so a stream addressed there
/// would be misrouted on any loopback/RX (review L2). We use the 1722 MAAP
/// locally-administered range one octet up, `91:E0:F0:01:xx:xx`, excluding the
/// AVDECC multicast itself (`91:E0:F0:01:00:00`): uid 0 maps to `..:01:00:01`.
fn stream_dest_mac(stream_id: avb_core::StreamId) -> avb_core::MacAddr {
    let uid = ((stream_id.0 & 0xFFFF) as u16).saturating_add(1);
    avb_core::MacAddr([0x91, 0xE0, 0xF0, 0x01, (uid >> 8) as u8, uid as u8])
}

/// The MSRP Talker Advertise TSpec for our stream (802.1Q-2018 §35.2.2.8).
///
/// Pure, so the values the 16R is asked to reserve against are testable rather
/// than buried in the TX loop.
///
/// `max_frame_size` is DERIVED from the frame we actually transmit — Ethernet
/// header + VLAN tag + the AAF PDU — so it cannot drift from the real stream if
/// the format changes. An SR class A talker emits one frame per 125 µs
/// observation interval, hence `max_interval_frames: 1`. Priority comes from the
/// SR class A domain (the same PCP the frames are tagged with, so the
/// reservation matches the traffic). `rank: true` is non-emergency.
///
/// `accumulated_latency_ns` is 0 at the talker: it accumulates ALONG THE PATH as
/// bridges add their transit time while the advertise propagates, and we are
/// cabled directly to the 16R with no bridge in between.
fn talker_tspec(stream_id: avb_core::StreamId, pdu_len: usize) -> avb_srp::attr::TalkerAdvertise {
    use avb_core::eth::{ETH_HDR_LEN, VLAN_TAG_LEN};
    use avb_srp::attr::DEFAULT_DOMAIN;
    avb_srp::attr::TalkerAdvertise {
        stream_id,
        dest_mac: stream_dest_mac(stream_id),
        vlan_id: DEFAULT_DOMAIN.vid,
        max_frame_size: (ETH_HDR_LEN + VLAN_TAG_LEN + pdu_len) as u16,
        max_interval_frames: 1,
        priority: DEFAULT_DOMAIN.priority,
        rank: true,
        accumulated_latency_ns: 0,
    }
}

/// Put an ADPDU on the wire, multicast to the AVDECC address.
///
/// The PDU itself comes from the entity, not from here: ADP and the ENTITY
/// descriptor must advertise the same capabilities and the same
/// `entity_model_id`, and a controller that sees them disagree treats the
/// entity as misbehaving. Building one of them by hand is how they drift.
fn send_adp(dext: &mut RealDext, our_mac: avb_core::MacAddr, adp: &avb_avdecc::adp::Adpdu) {
    use avb_avtp::common::ETHERTYPE_AVTP;
    use avb_core::eth::EthHeader;
    use avb_core::TxFrame;

    use crate::dextio::DextPort;

    let hdr = EthHeader {
        dst: avb_core::AVDECC_MULTICAST,
        src: our_mac,
        vlan: None,
        ethertype: ETHERTYPE_AVTP,
    };
    if let Ok(f) = TxFrame::build(&hdr, &adp.encode(), false, 0) {
        let _ = dext.send(f.bytes(), false, 0);
    }
}

/// Send an AEM response unicast back to the controller that asked.
///
/// Unicast, not multicast: an AEM response belongs to the one controller whose
/// `sequence_id` it echoes.
fn send_aem_response(
    dext: &mut RealDext,
    our_mac: avb_core::MacAddr,
    dst: avb_core::MacAddr,
    resp: &avb_avdecc::aecp::AemPdu,
) {
    use avb_avtp::common::ETHERTYPE_AVTP;
    use avb_core::eth::EthHeader;
    use avb_core::TxFrame;

    use crate::dextio::DextPort;

    let hdr = EthHeader {
        dst,
        src: our_mac,
        vlan: None,
        ethertype: ETHERTYPE_AVTP,
    };
    if let Ok(f) = TxFrame::build(&hdr, &resp.encode(), false, 0) {
        let _ = dext.send(f.bytes(), false, 0);
    }
}

/// Resolve the 16R's entity id — the LISTENER for the playback direction.
/// `--listener <id>` wins; otherwise discover via ADP exactly like the record
/// path (the 16R is the sole AVDECC talker on the link). Advertises our own
/// talker entity while waiting. Exits `TalkerNotFound` (6) on timeout or when
/// several candidates make the choice ambiguous.
fn discover_listener(
    opts: &GlobalOpts,
    log: &Logger,
    dext: &mut RealDext,
    entity: &mut avb_avdecc::entity::TalkerEntity,
    our_mac: avb_core::MacAddr,
    want: Option<u64>,
) -> Result<avb_core::Eui64, Exit> {
    use avb_avdecc::adp::Adpdu;
    use avb_avdecc::constants::subtype;
    use avb_avdecc::EntityTable;
    use avb_avtp::common::ETHERTYPE_AVTP;
    use avb_core::eth::{EthFrame, EthHeader};
    use avb_core::time::{MachTicks, Timebase};
    use avb_core::{Eui64, TxFrame};

    use crate::dextio::DextPort;

    let our_id = entity.entity_id();
    let adp = entity.advertise();
    send_adp(dext, our_mac, &adp);
    if let Some(id) = want {
        let id = Eui64::from_u64(id);
        log.log(
            Level::Info,
            "adp",
            &format!("listener given: 0x{:016x}", u64::from_be_bytes(id.0)),
        );
        return Ok(id);
    }

    let info = dext.device().info().map_err(|_| Exit::DextFault)?;
    let tb = Timebase {
        numer: info.tb_numer,
        denom: info.tb_denom,
    };
    let now_mach = || MachTicks(unsafe { mach2::mach_time::mach_absolute_time() });
    let start = now_mach();
    let ns_since = |t: MachTicks| tb.ticks_to_ns(now_mach().ticks_since(t));
    let deadline_ns = opts.discover_timeout.saturating_mul(1_000_000_000);

    let discover = Adpdu {
        message_type: avb_avdecc::constants::adp_msg::ENTITY_DISCOVER,
        valid_time: 0,
        entity_id: Eui64([0; 8]),
        entity_model_id: 0,
        entity_capabilities: 0,
        talker_stream_sources: 0,
        talker_capabilities: 0,
        listener_stream_sinks: 0,
        listener_capabilities: 0,
        controller_capabilities: 0,
        available_index: 0,
        gptp_grandmaster_id: 0,
        gptp_domain_number: 0,
        identify_control_index: 0,
        interface_index: 0,
        association_id: 0,
    };
    let mut table = EntityTable::new(our_id);
    let mut last_discover = MachTicks(0);
    loop {
        if shutdown_requested() {
            return Err(Exit::Interrupted);
        }
        if last_discover.0 == 0 || ns_since(last_discover) >= 2_000_000_000 {
            let hdr = EthHeader {
                dst: avb_core::AVDECC_MULTICAST,
                src: our_mac,
                vlan: None,
                ethertype: ETHERTYPE_AVTP,
            };
            if let Ok(f) = TxFrame::build(&hdr, &discover.encode(), false, 0) {
                let _ = dext.send(f.bytes(), false, 0);
            }
            // Re-advertise ourselves too, not just ask who is there: ADP
            // validity is 2 * valid_time, and a discovery that runs longer
            // than that would otherwise let the 16R age us out of its table
            // before we ever ask it to connect.
            let adp = entity.advertise();
            send_adp(dext, our_mac, &adp);
            last_discover = now_mach();
        }
        for slot in dext.rx_ctl() {
            if let Ok(p) = EthFrame::parse(&slot.data) {
                if p.hdr.ethertype == ETHERTYPE_AVTP
                    && p.payload.first().copied() == Some(subtype::ADP)
                {
                    if let Some(adp) = Adpdu::decode(p.payload) {
                        let _ = table.observe(&adp, p.hdr.src, ns_since(start), false);
                    }
                }
            }
        }
        match table.sole_talker() {
            Ok(Some(id)) => {
                log.log(
                    Level::Info,
                    "adp",
                    &format!(
                        "listener (16R) discovered: 0x{:016x}",
                        u64::from_be_bytes(id.0)
                    ),
                );
                return Ok(id);
            }
            Err(ids) => {
                log.log(
                    Level::Error,
                    "adp",
                    &format!("{} entities discovered; pass --listener <id>", ids.len()),
                );
                return Err(Exit::TalkerNotFound);
            }
            Ok(None) => {}
        }
        if ns_since(start) >= deadline_ns {
            log.log(
                Level::Error,
                "adp",
                "no AVDECC entity discovered to play to",
            );
            return Err(Exit::TalkerNotFound);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Issue `CONNECT_RX_COMMAND` to the 16R (as listener), binding its
/// STREAM_INPUT[`listener_uid`] to our talker stream. Controller role (design
/// §9.2, mirror of the record path's CONNECT_TX_COMMAND leg). The 16R is the
/// listener here, so `listener_entity_id` is ITS id (review H3) and every id
/// that names us is our EUI-64 entity id (review H4).
#[allow(clippy::too_many_arguments)]
fn connect_listener(
    log: &Logger,
    dext: &mut RealDext,
    our_mac: avb_core::MacAddr,
    our_id: avb_core::Eui64,
    listener_id: avb_core::Eui64,
    our_stream_id: avb_core::StreamId,
    listener_uid: u16,
    sequence_id: u16,
) {
    use avb_avtp::common::ETHERTYPE_AVTP;
    use avb_core::eth::EthHeader;
    use avb_core::TxFrame;

    use crate::dextio::DextPort;

    let pdu = connect_rx_pdu(
        our_id,
        listener_id,
        our_stream_id,
        listener_uid,
        sequence_id,
    );
    let hdr = EthHeader {
        dst: avb_core::AVDECC_MULTICAST,
        src: our_mac,
        vlan: None,
        ethertype: ETHERTYPE_AVTP,
    };
    if let Ok(f) = TxFrame::build(&hdr, &pdu.encode(), false, 0) {
        let _ = dext.send(f.bytes(), false, 0);
    }
    if sequence_id == 0 {
        log.log(
            Level::Info,
            "acmp",
            &format!(
                "CONNECT_RX_COMMAND -> 0x{:016x} uid={listener_uid}, our stream 0x{:016x}",
                u64::from_be_bytes(listener_id.0),
                our_stream_id.0
            ),
        );
    }
}

/// Build the CONNECT_RX_COMMAND PDU (pure; unit-tested).
fn connect_rx_pdu(
    our_id: avb_core::Eui64,
    listener_id: avb_core::Eui64,
    our_stream_id: avb_core::StreamId,
    listener_uid: u16,
    sequence_id: u16,
) -> avb_avdecc::acmp::Acmpdu {
    use avb_avdecc::acmp::Acmpdu;
    use avb_avdecc::constants::{acmp_msg, acmp_status};
    Acmpdu {
        message_type: acmp_msg::CONNECT_RX_COMMAND,
        status: acmp_status::SUCCESS,
        stream_id: our_stream_id,
        controller_entity_id: our_id,
        talker_entity_id: our_id,
        listener_entity_id: listener_id,
        talker_unique_id: (our_stream_id.0 & 0xFFFF) as u16,
        listener_unique_id: listener_uid,
        sequence_id,
        ..Default::default()
    }
}

#[cfg(test)]
mod talker_tests {
    use super::*;
    use avb_core::{Eui64, MacAddr, StreamId};

    /// The TSpec the 16R reserves against must describe the stream we actually
    /// send, and must match the frames' own VLAN priority — otherwise the
    /// reservation and the traffic disagree and the listener can refuse.
    #[test]
    fn talker_tspec_describes_the_real_stream() {
        let mac = MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]);
        let sid = StreamId::from_parts(mac, 0);
        // The Milan 48 kHz / 6 spf / 8 ch / 32-bit PDU: 24 B AAF header + 192 B.
        let pdu_len = 24 + 192;
        let t = talker_tspec(sid, pdu_len);

        // Ethernet header (14) + VLAN tag (4) + PDU — derived, not guessed.
        assert_eq!(t.max_frame_size, (14 + 4 + pdu_len) as u16);
        assert_eq!(t.max_frame_size, 234);
        // One frame per class-A observation interval.
        assert_eq!(t.max_interval_frames, 1);
        // Same PCP the frames are tagged with, and the SR class A VID.
        assert_eq!(t.priority, avb_srp::attr::DEFAULT_DOMAIN.priority);
        assert_eq!(t.vlan_id, avb_srp::attr::DEFAULT_DOMAIN.vid);
        assert!(t.rank, "non-emergency");
        // Path latency accumulates in bridges; a directly-cabled talker starts
        // at zero.
        assert_eq!(t.accumulated_latency_ns, 0);
        // The advertise must name the same destination the frames are sent to,
        // or the 16R reserves a path for an address we never use.
        assert_eq!(t.dest_mac, stream_dest_mac(sid));
        assert_eq!(t.stream_id.0, sid.0);
        // And that destination must stay out of the AVDECC control block.
        assert_ne!(t.dest_mac.octets()[3], 0x00);
    }

    #[test]
    fn connect_rx_names_the_16r_as_listener_and_us_by_eui64() {
        let mac = MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]);
        let our_id = Eui64::from_mac(mac);
        let the_16r = Eui64::from_u64(0x000a_92ff_fe03_3ba9);
        let sid = StreamId::from_parts(mac, 0);
        let pdu = connect_rx_pdu(our_id, the_16r, sid, 3, 7);
        assert_eq!(pdu.listener_entity_id, the_16r, "listener must be the 16R");
        assert_eq!(pdu.talker_entity_id, our_id);
        assert_eq!(pdu.controller_entity_id, our_id);
        // Our entity id carries the FF:FE EUI-64 insertion, like every other path.
        assert_eq!(&our_id.0[3..5], &[0xff, 0xfe]);
        assert_eq!(pdu.listener_unique_id, 3);
        assert_eq!(pdu.sequence_id, 7);
    }

    #[test]
    fn stream_dest_mac_is_outside_the_avdecc_classifier_block() {
        let mac = MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]);
        for uid in [0u16, 1, 0x00ff, 0xfffe] {
            let d = stream_dest_mac(StreamId::from_parts(mac, uid));
            assert!(
                !avb_core::is_avdecc_dest(d, mac),
                "uid {uid} -> {:02x?} lands in the AVDECC block",
                d.0
            );
            assert_ne!(d, avb_core::AVDECC_MULTICAST);
        }
    }
}
