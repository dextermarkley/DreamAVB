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
    )
}

/// Bind all of the 16R's talker streams via ACMP and count received AAF
/// packets per stream over `seconds`, reporting per-stream pkt/s each
/// `interval` (design §4 hw-streams; Class A is 8000 pkt/s). Reuses the
/// ADP/AECP control path and the mapped audio ring.
fn streams_loop(
    opts: &GlobalOpts,
    log: &Logger,
    dext: &mut RealDext,
    seconds: u64,
    interval: u64,
    json_out: Option<&Path>,
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
        &format!("audio ring: raw_slots={raw_audio_slots} non_aaf_or_undecoded={aaf_decode_fail}"),
    );
    let _ = first_slot_dump;
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
