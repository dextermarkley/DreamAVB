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
    if !avb_dext::abi::abi_supported(info.abi_version) {
        log.log(
            Level::Error,
            "dext",
            &format!(
                "dext ABI v{} outside the range v{}..=v{} this daemon drives; rebuild and reactivate (scripts/package.sh, avb-installer activate)",
                info.abi_version,
                avb_dext::abi::ABI_VERSION_MIN,
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
    use avb_avdecc::aecp::{AecpEngine, AecpOutcome, AemPdu, MvuPdu};
    use avb_avdecc::constants::aecp_status;
    use avb_avdecc::constants::{aecp_msg, aem_cmd, desc as aem_desc, subtype};
    use avb_avdecc::milan::mvu_cmd;
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
                            // EVERY type the device advertises, not a chosen
                            // family: the point of a dump is to capture the
                            // real bytes of whatever is there, and a filtered
                            // walk silently hides the one descriptor you came
                            // for. The 16R advertises CONTROL, LOCALE, TIMING
                            // and PTP_INSTANCE that an audio/stream/clock
                            // filter dropped, and a missing capture is what
                            // forces a wire layout to be guessed from a spec
                            // instead of transcribed from hardware.
                            for (dt, count) in config_descriptor_counts(&dbytes) {
                                for di in 0..count {
                                    enqueue(dt, di, &mut requested, &mut eng);
                                }
                            }
                        } else if dtype == aem_desc::PTP_INSTANCE {
                            // IEEE 1722.1-2021 §7.2.35: after the 70-byte
                            // common name/localized prefix and clockIdentity(8),
                            // flags(4), number/base controls(4), the descriptor
                            // carries number_of_ptp_ports and base_ptp_port.
                            // The 16R references PTP_PORT 0 here but omits that
                            // type from CONFIGURATION, so following the internal
                            // reference is the only way to capture it.
                            if let Some((base, number)) = ptp_port_range(&dbytes) {
                                for di in base..base.saturating_add(number) {
                                    enqueue(aem_desc::PTP_PORT, di, &mut requested, &mut eng);
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

    // --- Phase 2b: MVU GET_MILAN_INFO ---
    //
    // Ask the 16R, a shipping Milan device, for its own Milan info. The point
    // is to capture the RESPONSE BYTES so our own responder can be written
    // against hardware instead of against Milan v1.2 Table 18: a
    // GET_MILAN_INFO response is three packed u32s, exactly the kind of field
    // this project has already got wrong by originating it from a document.
    {
        let probe = MvuPdu {
            message_type: aecp_msg::VENDOR_UNIQUE_COMMAND,
            status: aecp_status::SUCCESS,
            target_entity_id: talker,
            controller_entity_id: our_id,
            sequence_id: 0xFFFF,
            command_type: mvu_cmd::GET_MILAN_INFO,
            // The command carries no arguments; the response appends the
            // payload. Milan v1.2 §5.4.4.3.
            payload: Vec::new(),
        };
        let hdr = EthHeader {
            dst: talker_mac,
            src: our_mac,
            vlan: None,
            ethertype: ETHERTYPE_AVTP,
        };
        let mut answered = false;
        if let Ok(frame) = TxFrame::build(&hdr, &probe.encode(), false, 0) {
            let _ = dext.send(frame.bytes(), false, 0);
            let probe_start = ns_since(start);
            // One second is many retries' worth at this layer; a Milan device
            // answers in milliseconds, and a silent miss is itself the finding.
            while ns_since(start) < probe_start + 1_000_000_000 {
                for slot in dext.rx_ctl() {
                    let Ok(p) = EthFrame::parse(&slot.data) else {
                        continue;
                    };
                    if p.hdr.ethertype != ETHERTYPE_AVTP {
                        continue;
                    }
                    let Some(r) = MvuPdu::decode(p.payload) else {
                        continue;
                    };
                    if r.message_type != aecp_msg::VENDOR_UNIQUE_RESPONSE {
                        continue;
                    }
                    log.log(
                        Level::Info,
                        "mvu",
                        &format!(
                            "16R GET_MILAN_INFO response: status={} cmd=0x{:04x} \
                             payload {} B = {}",
                            r.status,
                            r.command_type,
                            r.payload.len(),
                            r.payload
                                .iter()
                                .map(|b| format!("{b:02x}"))
                                .collect::<String>()
                        ),
                    );
                    answered = true;
                }
                if answered {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        if !answered {
            log.log(
                Level::Warn,
                "mvu",
                "the 16R did not answer GET_MILAN_INFO — it may not implement MVU, \
                 in which case Milan detection is not what gates routing",
            );
        }
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

/// Parse the `descriptor_counts` list with the one shared AEM decoder. Keeping
/// a second parser here drifted: the library learned the 16R underreports one
/// complete trailing PTP_INSTANCE pair, while this copy still stopped at the
/// declared count and silently omitted that descriptor from every dump.
fn config_descriptor_counts(d: &[u8]) -> Vec<(u16, u16)> {
    avb_avdecc::aem::desc::decode_configuration(d)
        .map(|c| c.descriptor_counts)
        .unwrap_or_default()
}

/// `(base_ptp_port, number_of_ptp_ports)` from a PTP_INSTANCE descriptor.
/// Offsets are IEEE 1722.1-2021 §7.2.35 and are pinned by the 16R descriptor
/// captured in `.agents/tmp/16r-aem-with-ptp2.json`.
fn ptp_port_range(d: &[u8]) -> Option<(u16, u16)> {
    let number = d.get(86..88).map(|b| u16::from_be_bytes([b[0], b[1]]))?;
    let base = d.get(88..90).map(|b| u16::from_be_bytes([b[0], b[1]]))?;
    Some((base, number))
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
                    "state={} asCapable={} servo={} gm={} path_delay_ns={:.0} nrr={:?} lost_since_good={} pdelay(rx_req={} rx_resp={} rx_fu={}) sync_rx={} announce_rx={} btca(won={} lost={}) mastertx(ann={} sync={} fu={} ts_lost={})",
                    st.port_state.name(),
                    st.as_capable,
                    st.servo_state.name(),
                    st.gm_identity,
                    st.path_delay_comp_ns.last.unwrap_or(f64::NAN),
                    st.nrr_systim,
                    st.lost_responses_since_good,
                    c.pdelay_req_rx,
                    c.pdelay_resp_rx,
                    c.pdelay_resp_fu_rx,
                    c.sync_rx,
                    c.announce_rx,
                    c.btca_we_won,
                    c.btca_we_lost,
                    c.announce_tx,
                    c.sync_tx,
                    c.follow_up_tx,
                    c.sync_tx_ts_lost,
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
    let mut peer_adp_dumped = false;
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
                        // The 16R's own advertisement, dumped once. It is the
                        // reference: its controller lists the 16R as online
                        // and us as offline while every stream-layer check
                        // passes, so what differs is how the two entities
                        // advertise themselves.
                        if !peer_adp_dumped
                            && adp.message_type == avb_avdecc::constants::adp_msg::ENTITY_AVAILABLE
                        {
                            peer_adp_dumped = true;
                            log.log(Level::Info, "wire", &dump_adp("ADP 16R's", &adp));
                        }
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
                // We are our own controller here and do not ask the talker
                // to hold off, so STREAMING_WAIT is clear.
                streaming_wait: false,
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
    //
    // The inbound path itself (ring creation from the first packet's geometry,
    // placement, media clock) lives in `listenerrx`, shared with the duplex
    // loop so there is one implementation of it.
    let mut lrx = crate::listenerrx::ListenerRx::new();
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
                // Feed the primary stream into the ring. The whole inbound
                // path — building the ring from the first packet's own
                // geometry, the media clock, placement and the published
                // clock block — lives in `listenerrx`, shared with the duplex
                // loop so there is one implementation of it.
                let placement = lrx.on_pdu(log, &pdu, ns_since(count_start));
                if placement == crate::listenerrx::Placement::FormatMismatch {
                    // Not audio we can trust: skip it entirely rather than
                    // recording or metering it.
                    continue;
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
        &format!("audio ring: raw_slots={raw_audio_slots} non_aaf_or_undecoded={aaf_decode_fail} format_mismatch={} frames_placed={}", lrx.format_mismatches(), lrx.frames_placed()),
    );
    report_discarded_streams(log, "streams", &lrx);
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
    listener_uid: Option<u16>,
    _allow_partial: bool,
    _start_timeout: u64,
    _summary_out: Option<PathBuf>,
    crf: bool,
) -> Result<(), Exit> {
    install_signal_handlers();
    let log = Logger::new(opts.level);
    let mut dext = bring_up(opts, &log)?;
    log.log(
        Level::Info,
        "play",
        &format!(
            "play {seconds}s OUT to 16R (sink {})",
            uid_label(listener_uid)
        ),
    );
    talker_loop(
        opts,
        &log,
        &mut dext,
        seconds,
        listener,
        listener_uid,
        false,
        false,
        // `play` is the standalone talker-drive mode: it exists to push a
        // stream at a listener with no controller in the picture, so acting
        // as that controller is the point of it. `duplex` -- the companion's
        // mode, on a network where the 16R has UC Surface -- defaults OFF.
        true,
        crf,
    )
}

/// Format one AAF AVTPDU as raw hex plus every decoded header field, for the
/// side-by-side wire comparison in [`duplex`].
///
/// `pdu` is the AVTP payload (after the EtherType), not the Ethernet frame.
/// `gptp_now` is our gPTP time at the moment the PDU was seen, which turns the
/// raw `avtp_timestamp` into the only form that means anything: how far ahead
/// of the clock the presentation time sits.
///
/// Both directions go through this ONE function so the two lines are directly
/// comparable: a dump that formats our PDU differently from the 16R's can hide
/// the very field difference it was written to find. The header bytes are
/// printed raw as well as decoded, because a decoder that misreads a field
/// misreads it identically in both dumps and the raw bytes are the only way to
/// see past it.
///
/// The presentation offset is the reason `gptp_now` is threaded in here. Our
/// own offset is a choice (Milan's 2 ms floor plus one class-A interval); the
/// 16R's is an OBSERVATION of what a Milan device that this network already
/// locks to actually uses. Printing both against the same clock is what makes
/// ours checkable against hardware rather than against my reading of the spec.
fn dump_aaf_pdu(label: &str, pdu: &[u8], gptp_now: Option<u64>) -> String {
    let hex: String = pdu
        .iter()
        .take(avb_avtp::aaf::AAF_HEADER_LEN)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    match avb_avtp::aaf::decode(pdu) {
        Ok(p) => {
            let s = &p.hdr.stream;
            // How far ahead of the clock the presentation time sits. Both are
            // ns mod 2^32 (a 4.295 s window), so a naive subtraction is wrong
            // across a wrap; fold the difference into the signed half-window,
            // which is the only reading that stays correct at the boundary.
            let pt = match gptp_now {
                Some(now) => {
                    let d = (i64::from(s.avtp_timestamp)) - ((now & 0xFFFF_FFFF) as i64);
                    let d = if d > 0x8000_0000 {
                        d - 0x1_0000_0000
                    } else if d < -0x8000_0000 {
                        d + 0x1_0000_0000
                    } else {
                        d
                    };
                    format!(" | presentation offset vs our gPTP now = {} us", d / 1000)
                }
                None => String::new(),
            };
            format!(
                "{label}: hdr[{hex}] | subtype=0x{:02x} sv={} ver={} mr={} tv={} seq={} tu={} \
                 stream_id=0x{:016x} avtp_ts={} ({:#010x}) | format={} nsr={} ch={} depth={} \
                 sdl={} sp={} evt={} | first8_payload={}{pt}",
                s.subtype,
                u8::from(s.sv),
                s.version,
                u8::from(s.mr),
                u8::from(s.tv),
                s.seq,
                u8::from(s.tu),
                s.stream_id.0,
                s.avtp_timestamp,
                s.avtp_timestamp,
                avb_avtp::aaf::format::name(p.hdr.format).unwrap_or("?"),
                p.hdr.nsr,
                p.hdr.channels_per_frame,
                p.hdr.bit_depth,
                p.hdr.stream_data_length,
                u8::from(p.hdr.sp),
                p.hdr.evt,
                p.data
                    .iter()
                    .take(8)
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        }
        Err(e) => format!("{label}: hdr[{hex}] | DOES NOT DECODE AS AAF: {e:?}"),
    }
}

/// Format one ADPDU field by field, for the side-by-side comparison in
/// [`duplex`].
///
/// Same reasoning as [`dump_aaf_pdu`], one layer up. The 16R appears ONLINE in
/// its own controller and we appear offline, while every stream-layer check
/// passes — so what differs is in how the two entities ADVERTISE themselves,
/// and the 16R's own ADPDU is the reference for what a controller on this
/// network accepts. `entity_capabilities` is printed as both the raw word and
/// the decoded flag names, because a missing capability bit is exactly the
/// kind of difference that is invisible in hex and decisive in behaviour.
fn dump_adp(label: &str, a: &avb_avdecc::adp::Adpdu) -> String {
    use avb_avdecc::constants::entity_caps as ec;
    let flags = [
        (ec::EFU_MODE, "EFU_MODE"),
        (ec::ADDRESS_ACCESS_SUPPORTED, "ADDRESS_ACCESS"),
        (ec::GATEWAY_ENTITY, "GATEWAY"),
        (ec::AEM_SUPPORTED, "AEM"),
        (ec::LEGACY_AVC, "LEGACY_AVC"),
        (ec::ASSOCIATION_ID_SUPPORTED, "ASSOC_ID_SUPPORTED"),
        (ec::ASSOCIATION_ID_VALID, "ASSOC_ID_VALID"),
        (ec::VENDOR_UNIQUE_SUPPORTED, "VENDOR_UNIQUE"),
        (ec::CLASS_A_SUPPORTED, "CLASS_A"),
        (ec::CLASS_B_SUPPORTED, "CLASS_B"),
        (ec::GPTP_SUPPORTED, "GPTP"),
        (ec::AEM_AUTHENTICATION_SUPPORTED, "AUTH_SUPPORTED"),
        (ec::AEM_AUTHENTICATION_REQUIRED, "AUTH_REQUIRED"),
        (ec::AEM_PERSISTENT_ACQUIRE_SUPPORTED, "PERSISTENT_ACQUIRE"),
        (ec::AEM_IDENTIFY_CONTROL_INDEX_VALID, "IDENTIFY_INDEX_VALID"),
        (ec::AEM_INTERFACE_INDEX_VALID, "INTERFACE_INDEX_VALID"),
        (ec::GENERAL_CONTROLLER_IGNORE, "CONTROLLER_IGNORE"),
        (ec::ENTITY_NOT_READY, "ENTITY_NOT_READY"),
    ];
    let named: Vec<&str> = flags
        .iter()
        .filter(|(bit, _)| a.entity_capabilities & bit != 0)
        .map(|(_, n)| *n)
        .collect();
    format!(
        "{label}: msg_type={} valid_time={} entity_id=0x{:016x} model_id=0x{:016x} \
         entity_caps=0x{:08x} [{}] | talker_sources={} talker_caps=0x{:04x} | \
         listener_sinks={} listener_caps=0x{:04x} | controller_caps=0x{:08x} \
         avail_index={} | gm_id=0x{:016x} domain={} identify_idx={} iface_idx={} \
         assoc_id=0x{:016x}",
        a.message_type,
        a.valid_time,
        a.entity_id.to_u64(),
        a.entity_model_id,
        a.entity_capabilities,
        named.join(" "),
        a.talker_stream_sources,
        a.talker_capabilities,
        a.listener_stream_sinks,
        a.listener_capabilities,
        a.controller_capabilities,
        a.available_index,
        a.gptp_grandmaster_id,
        a.gptp_domain_number,
        a.identify_control_index,
        a.interface_index,
        a.association_id,
    )
}

/// Both directions in ONE process (the mode the companion app launches).
///
/// This exists because the dext permits one `RingAttach` per connection: a
/// second `avb-daemon` attaching the same I210 is refused with
/// `kIOReturnExclusiveAccess` and exits `Owned` (9). So `play` and `record`
/// cannot be run side by side to get bidirectional audio — one loop has to do
/// both, sharing a single control-ring drain and a single gPTP service,
/// because the control ring is one queue and a slot read by one direction is
/// gone for the other.
///
/// `seconds == 0` means run until SIGINT, which is how the companion runs it.
#[allow(clippy::too_many_arguments)]
pub fn duplex(
    opts: &GlobalOpts,
    seconds: u64,
    listener: Option<u64>,
    listener_uid: Option<u16>,
    all_inbound_streams: bool,
    bind_sinks: bool,
    _start_timeout: u64,
    _summary_out: Option<PathBuf>,
    crf: bool,
) -> Result<(), Exit> {
    install_signal_handlers();
    let log = Logger::new(opts.level);
    let mut dext = bring_up(opts, &log)?;
    log.log(
        Level::Info,
        "duplex",
        &if seconds == 0 {
            format!(
                "duplex IN+OUT with the 16R until SIGINT (sink {})",
                uid_label(listener_uid)
            )
        } else {
            format!(
                "duplex {seconds}s IN+OUT with the 16R (sink {})",
                uid_label(listener_uid)
            )
        },
    );
    talker_loop(
        opts,
        &log,
        &mut dext,
        seconds,
        listener,
        listener_uid,
        true,
        all_inbound_streams,
        bind_sinks,
        crf,
    )
}

/// The talker TX loop (design §5.2 TX side, Phase 1b-2). Creates the output
/// ring, discovers the 16R and binds our STREAM_OUTPUT to its STREAM_INPUT via
/// ACMP (as controller), then paces AAF PDUs from the ring on the media clock.
///
/// With `duplex` set it ALSO runs the inbound direction in the same loop:
/// declaring MSRP Listener Ready when the 16R advertises its talker, installing
/// the dext RX filter, re-asserting ACMP CONNECT_TX, and feeding received AAF
/// into the input ring via [`crate::listenerrx`].
///
/// Both directions have to share one loop, not two processes: the dext allows
/// one `RingAttach` per connection, so a second daemon gets
/// `kIOReturnExclusiveAccess` and exits `Owned`. They also have to share one
/// control-ring drain and one gPTP service, because the control ring is a
/// single queue — a slot read by one direction is gone for the other.
#[allow(clippy::too_many_arguments)]
fn talker_loop(
    opts: &GlobalOpts,
    log: &Logger,
    dext: &mut RealDext,
    seconds: u64,
    listener: Option<u64>,
    listener_uid: Option<u16>,
    duplex: bool,
    all_inbound_streams: bool,
    bind_sinks: bool,
    crf: bool,
) -> Result<(), Exit> {
    use avb_avtp::crf::CrfTalker;
    use avb_avtp::stream_format::AafFormat;
    use avb_avtp::talker::AafTalker;
    use avb_core::eth::{EthHeader, VlanTag};
    use avb_core::{StreamId, TxFrame};

    use crate::dextio::DextPort;

    let dir = if duplex { "duplex" } else { "play" };
    // Our talker stream id = our MAC + a unique id (0 for the first output
    // stream). The 16R identifies the stream it listens to by this id.
    let info = dext.device().info().map_err(|_| Exit::DextFault)?;
    let our_mac = avb_core::MacAddr::new(info.mac);
    let our_id = avb_core::Eui64::from_mac(our_mac);

    // ---- The output format, read from the 16R ------------------------------
    //
    // AGENTS.md: rates, channel counts and stream counts are NEVER hardcoded.
    // This used to be a literal Milan base format (48 kHz / 8 ch / 6 spf),
    // which happens to match this 16R and would silently mismatch a
    // differently-configured one — the 16R would then discard every packet,
    // which looks exactly like a transmit fault.
    //
    // Ordering note: the format comes from the 16R, our entity model is built
    // FROM the format, and our ADP is built from the model. So discovery has to
    // run before the model exists, and we advertise ourselves as soon as it
    // does — still before any ACMP, which is what a Milan entity requires
    // (review H3).
    let listener_timeout_ns = opts.discover_timeout.saturating_mul(1_000_000_000);
    let (listener_id, listener_mac) =
        discover_listener(opts, log, dext, our_id, our_mac, listener)?;
    let peers = AecpPeers {
        our_mac,
        our_id,
        target: listener_id,
        target_mac: listener_mac,
    };
    let (stream_format, sink_uids) =
        derive_talker_format(log, dext, peers, listener_uid, listener_timeout_ns)?;
    let fmt: AafFormat = stream_format.aaf().ok_or(Exit::DextFault)?;
    let hz = fmt.nsr_hz().ok_or(Exit::DextFault)? as u64;
    let spf = usize::from(fmt.spf);
    let channels = usize::from(fmt.channels);

    // Which of the 16R's STREAM_OUTPUT sources we listen to, for duplex.
    //
    // ALL of its AAF audio sources, read from its AEM. Index 0 is `Send CRF`,
    // its media-clock source, so a hardcoded 0 asked for a stream with no
    // audio at all; binding only one of the eight audio sources kept 8 of the
    // device's 64 channels. The stream ids come from GET_STREAM_INFO in
    // descriptor-index order, which is what makes the inbound channel map
    // reproducible: ring channels 1-8 are the device's "Send 1-8" on every
    // run, not whichever stream's packet arrived first.
    let (inbound_uids, inbound_stream_ids) = if duplex {
        let (in_fmt, mut uids) = derive_inbound_sources(log, dext, peers, listener_timeout_ns)?;
        if !all_inbound_streams {
            // Default: ONE source, today's verified behaviour. Binding all
            // eight is behind --all-inbound-streams because the cross-stream
            // alignment still has a residual defect and the CoreAudio plug-in
            // refuses a ring wider than its own 8-channel device.
            uids.truncate(1);
            log.log(
                Level::Info,
                "duplex",
                "binding ONE inbound source (8 of the 16R's 64 channels); pass \
                 --all-inbound-streams for all eight",
            );
        }
        let ids_by_index = read_stream_ids(log, dext, peers, &uids, listener_timeout_ns)?;
        // Keep only the sources whose id the 16R actually told us: a slot
        // whose stream id we had to invent could never match an arriving
        // packet, and would sit in the channel map as permanent silence.
        let mut uid_kept: Vec<u16> = Vec::new();
        let mut ids: Vec<u64> = Vec::new();
        for u in &uids {
            match ids_by_index.get(u) {
                Some(&id) => {
                    uid_kept.push(*u);
                    ids.push(id);
                }
                None => log.log(
                    Level::Warn,
                    "duplex",
                    &format!(
                        "16R STREAM_OUTPUT {u} did not report a stream id, so it cannot be bound \
                         — its 8 channels will be absent rather than silent-but-mapped"
                    ),
                ),
            }
        }
        if ids.is_empty() {
            log.log(
                Level::Error,
                "duplex",
                "no 16R audio source reported a stream id, so nothing can be bound inbound",
            );
            return Err(Exit::TalkerNotFound);
        }
        log.log(
            Level::Info,
            "acmp",
            if bind_sinks {
                "connection management: acting as CONTROLLER — we bind the \
                 listener's sinks over ACMP (§5.5.2.4 Controller Bind). With \
                 the dext owning the NIC exclusively, no other controller on \
                 this machine can reach the AVB link: Apple's ATDECC needs \
                 Apple's AVB stack, and UC Surface is on a different \
                 interface. Pass --no-bind-sinks if another controller owns \
                 the assignments."
            } else {
                "connection management: pure Milan TALKER (--no-bind-sinks) — \
                 declaring our streams over SRP and answering PROBE_TX, \
                 binding nothing. Something else must bind the listener's \
                 sinks or no audio will be routed (§5.5.1.2, §5.5.2.7)."
            },
        );
        log.log(
            Level::Info,
            "duplex",
            &format!(
                "inbound: {} stream(s) from 16R STREAM_OUTPUT {uid_kept:?}, format 0x{:016x}, {} channels",
                ids.len(),
                in_fmt.0,
                ids.len() * usize::from(in_fmt.aaf().map(|a| a.channels).unwrap_or(0)),
            ),
        );
        (uid_kept, ids)
    } else {
        (Vec::new(), Vec::new())
    };

    // The outbound stream set: one of OUR STREAM_OUTPUTs per 16R audio sink,
    // in the sink's own descriptor-index order.
    //
    // `out_streams[i]` owns output-ring channels `i * channels ..`, the same
    // layout the AEM describes and the TX gather uses — one ordering, so what
    // a controller reads, what the ring holds and what goes on the wire cannot
    // disagree.
    struct OutStream {
        /// Our STREAM_OUTPUT unique id, which is also the ring slot index.
        uid: u16,
        stream_id: StreamId,
        /// The 16R STREAM_INPUT sink this stream is bound to.
        sink_uid: u16,
        /// The packetizer for this stream.
        talker: AafTalker,
        /// The 16R has answered this stream's Talker Advertise with Ready.
        ready: bool,
    }
    let mut out_streams: Vec<OutStream> = Vec::with_capacity(sink_uids.len());
    for (i, &sink_uid) in sink_uids.iter().enumerate() {
        let Ok(uid) = u16::try_from(i) else { break };
        let stream_id = StreamId::from_parts(our_mac, uid);
        let Some(talker) = AafTalker::new(stream_id, fmt) else {
            log.log(
                Level::Error,
                "play",
                &format!("could not build a packetizer for stream {uid}"),
            );
            return Err(Exit::DextFault);
        };
        out_streams.push(OutStream {
            uid,
            stream_id,
            sink_uid,
            talker,
            ready: false,
        });
    }
    if out_streams.is_empty() {
        log.log(Level::Error, "play", "no outbound stream to transmit");
        return Err(Exit::DextFault);
    }
    let our_stream_id = out_streams
        .first()
        .map(|o| o.stream_id)
        .unwrap_or(StreamId(0));
    let listener_uid = out_streams.first().map_or(0, |o| o.sink_uid);

    // ---- The media clock stream (CRF), optional ----------------------------
    //
    // A Milan Clock Reference Format stream (Milan v1.3 §7.3.1): 500 PDUs/s,
    // one 8-octet gPTP timestamp each, 96 media-clock events apart at 48 kHz.
    //
    // Its uid sits ABOVE every audio stream, so its destination address is
    // still inside the 64-address MAAP block claimed below and needs no second
    // allocation. The 16R's own matching sink is its STREAM_INPUT 0, which its
    // AEM names "Return CRF" — the mirror of its STREAM_OUTPUT 0 "Send CRF".
    //
    // Why gPTP and not our recovered media clock: in duplex we recover a media
    // clock FROM the 16R's arriving stream, so deriving CRF from it would feed
    // the 16R's own clock back to it — 16R → our recovery → our CRF → its
    // Return CRF, a loop. Milan §7.5 sanctions gPTP as a media clock source
    // instead, and since the 16R is our gPTP grandmaster, gPTP time IS its own
    // time: this hands it back a consistent restatement rather than a loop.
    let crf_uid = u16::try_from(out_streams.len()).unwrap_or(u16::MAX);
    let crf_stream_id = StreamId::from_parts(our_mac, crf_uid);
    let mut crf_talker = crf.then(|| CrfTalker::milan(crf_stream_id));
    /// The 16R's STREAM_INPUT index that accepts a media clock, from its own
    /// AEM: index 0 is "Return CRF". Not a guess — the descriptor name is
    /// captured in `avb-avdecc`'s tests.
    const CRF_SINK_UID: u16 = 0;
    if let Some(t) = &crf_talker {
        log.log(
            Level::Info,
            "crf",
            &format!(
                "media clock stream enabled: our STREAM_OUTPUT {crf_uid} 0x{:016x} -> 16R \
                 STREAM_INPUT {CRF_SINK_UID} (Return CRF), dest {}, {} PDU/s, {}B AVTPDU, \
                 interval {} events @ {} Hz, timestamps from gPTP",
                crf_stream_id.0,
                stream_dest_mac(crf_stream_id),
                avb_avtp::crf::milan::pdus_per_sec(),
                t.pdu_len(),
                t.timestamp_interval(),
                t.base_frequency(),
            ),
        );
    }

    // Create the OUTPUT ring (daemon = consumer, plug-in = producer). We create
    // it so the file exists with world perms before the plug-in attaches, the
    // same ownership rule as the input ring.
    // The output ring is the device's FULL width, like the input ring and for
    // the same reason: the CoreAudio device's channel count is fixed when the
    // device is published, so a ring of any other width is refused by the
    // plug-in — silently, as no audio. Channels belonging to a stream we are
    // not transmitting yet are produced by CoreAudio and simply not sent.
    let out_total_ch = avb_audioring::DEVICE_CHANNELS as usize;
    let out_slots = out_streams.len();
    let cfg = avb_audioring::RingConfig {
        channels: out_total_ch as u32,
        valid_bits: u32::from(fmt.bit_depth),
        nominal_rate_hz: hz as u32,
        capacity_frames: 65_536,
        streams: out_streams
            .iter()
            .enumerate()
            .map(|(i, s)| avb_audioring::StreamSlot {
                stream_id: s.stream_id.0,
                first_channel: (i * channels) as u32,
                channels: channels as u32,
                bit_depth: u32::from(fmt.bit_depth),
            })
            .collect(),
        channel_map: (0..out_total_ch)
            .map(|k| ((k / channels.max(1)) as u16, (k % channels.max(1)) as u16))
            .collect(),
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
            "output ring {} created: {out_total_ch}-channel ring @ {} Hz, spf={}, {} of {} stream slot(s) transmitted (stream 0x{:016x} -> 16R sink {})",
            avb_audioring::SHM_NAME_OUT,
            hz,
            spf,
            out_slots,
            avb_audioring::STREAMS_PER_DIRECTION,
            our_stream_id.0,
            listener_uid,
        ),
    );
    if out_slots < avb_audioring::STREAMS_PER_DIRECTION as usize {
        log.log(
            Level::Warn,
            "play",
            &format!(
                "output channels {}..{out_total_ch} are produced by CoreAudio and NOT transmitted: \
                 only {out_slots} of {} talker stream(s) is built, so audio played to them goes \
                 nowhere rather than to the 16R",
                out_slots * channels + 1,
                avb_audioring::STREAMS_PER_DIRECTION,
            ),
        );
    }

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
    let our_stream_ids: Vec<StreamId> = out_streams.iter().map(|o| o.stream_id).collect();
    // STREAM_INPUTs: exactly the inbound sources we really bound, so the
    // sinks we advertise are the ones the receive path can absorb. In
    // transmit-only mode this is 0 and the input side of the AEM is omitted
    // entirely rather than declared empty.
    let model = avb_avdecc::entity::talker_model(
        our_mac,
        &our_stream_ids,
        stream_format,
        crate::FIRMWARE_VERSION,
        inbound_stream_ids.len(),
    )
    .ok_or(Exit::DextFault)?;
    // The CRF output joins the model only when we really transmit it. An
    // advertised media-clock stream we never send is a stream a controller can
    // bind and then wait on forever.
    let model = if crf_talker.is_some() {
        model.with_media_clock_stream(crf_stream_id)
    } else {
        model
    };
    log.log(
        Level::Info,
        "aem",
        &format!(
            "entity 0x{:016x} model 0x{:016x}: {} STREAM_OUTPUT(s) ({} audio + {} media clock) + {} STREAM_INPUT(s) x {} ch = {} out channels @ {} Hz, {} AUDIO_CLUSTER(s) across both directions",
            u64::from_be_bytes(our_id.0),
            model.entity_model_id,
            model.stream_count(),
            model.audio_stream_count(),
            model.media_clock_stream_count(),
            model.sink_count(),
            model.channels,
            usize::from(model.audio_stream_count()) * usize::from(model.channels),
            model.current_sampling_rate,
            model.cluster_count(),
        ),
    );
    let mut entity = avb_avdecc::entity::TalkerEntity::new(model);

    // Announce ourselves now that the model is real — it carries the format we
    // just derived, so this ADP advertises an entity_model_id that will not
    // change underneath the 16R. This must happen BEFORE any ACMP: a Milan
    // entity ignores ACMP from an entity it has not discovered (review H3).
    let adp = entity.advertise();
    send_adp(dext, our_mac, &adp);

    // Connect as controller: tell the 16R to connect each of its STREAM_INPUT
    // sinks to the matching talker stream of ours. Mirror of the record path's
    // CONNECT_TX_COMMAND, with the 16R as listener_entity_id.
    //
    // ONE CONNECT PER STREAM. The 16R binds the sink a controller names, so
    // connecting one of eight leaves the other seven sinks unbound no matter
    // how many streams we advertise or transmit.
    let mut connect_seq: u16 = 0;
    // We deliberately do NOT tear the 16R's bindings down first.
    //
    // An earlier version of this sent DISCONNECT_RX for every sink before
    // connecting, reasoning that a binding left by a previous daemon would
    // make the 16R think it was already connected and skip the CONNECT_TX
    // handshake. That reasoning was wrong in a way that mattered: the 16R's
    // saved binding IS the routing. UC Surface renders it on the AVB Inputs
    // page as `0x<entity>:<n>`, which is how a user assigns our streams to
    // mixer channels — so disconnecting on every start silently wiped the
    // assignment the user had just made, and the page went from listing seven
    // of our streams to listing nothing at all.
    //
    // The evidence for the teardown was weak anyway: `ACMP talker commands
    // answered` varied between 0, 8 and 14 both before and after it, so the
    // one run that read 0 was noise rather than a stale binding. A
    // re-CONNECT_RX for a binding the 16R already holds is idempotent, which
    // is what makes the teardown unnecessary.
    //
    // `disconnect_rx_pdu` and `disconnect_listener` are kept and still tested:
    // a disconnect is the right response to a sink bound to a DIFFERENT
    // talker's stream, which needs the pre-connect GET_STREAM_INFO read to
    // detect. Doing it unconditionally is what was destructive.
    for o in &out_streams {
        connect_listener(
            log,
            dext,
            our_mac,
            our_id,
            listener_id,
            o.stream_id,
            o.sink_uid,
            connect_seq,
        );
        connect_seq = connect_seq.wrapping_add(1);
    }
    // And the media clock, onto the 16R's own "Return CRF" sink. Same
    // controller-bind rule as the audio sinks: nothing else on this machine
    // can reach the AVB link to do it.
    if crf_talker.is_some() {
        connect_listener(
            log,
            dext,
            our_mac,
            our_id,
            listener_id,
            crf_stream_id,
            CRF_SINK_UID,
            connect_seq,
        );
        connect_seq = connect_seq.wrapping_add(1);
    }

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

    // ---- Clock source ------------------------------------------------------
    //
    // Which clock the Mac sources audio on, as chosen in the companion app.
    // We and the 16R do not share an oscillator: a nominal 48 kHz here is not
    // the same 48 kHz there, so sourcing on the Mac's crystal drifts against
    // the listener until the stream breaks.
    //
    // The config path is LOGGED rather than assumed. The daemon and the app
    // agree on $HOME, so a daemon started under sudo would read root's file
    // instead and silently run on defaults.
    let mut clocksel = crate::clocksrc::ClockSelector::load(hz as f64);
    log.log(
        Level::Info,
        "clock",
        &format!(
            "clock source: {} ({}) [{}]",
            clocksel.selected().label(),
            clocksel.selected(),
            clocksel
                .config_path()
                .unwrap_or("HOME unset; using defaults")
        ),
    );
    for w in clocksel.take_warnings() {
        log.log(Level::Warn, "clock", &w);
    }
    // The recovered 16R media clock lives on the INPUT ring. In duplex our own
    // inbound path creates that ring once the first AAF packet arrives, so
    // there is nothing to attach yet and we must NOT conclude the clock is
    // unavailable. Attaching read-only here covers only the other case: a
    // `play` session running alongside a separate `record`.
    let recovered_ring = avb_audioring::ShmRing::attach().ok();
    log.log(
        Level::Info,
        "clock",
        match (duplex, recovered_ring.is_some()) {
            (true, _) => {
                "duplex — the 16R's recovered media clock will come from our own inbound \
                 stream once the first packet arrives"
            }
            (false, true) => "input ring present — the 16R's recovered media clock is available",
            (false, false) => {
                "no input ring — the 16R's recovered media clock is unavailable \
                 (use `duplex`, or run `record` alongside `play`)"
            }
        },
    );
    let mut last_clock_pub = now_mach();
    let mut last_effective: Option<avb_config::EffectiveClock> = None;

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
    use avb_srp::attr::{
        Attribute, Listener, ListenerDecl, Talker, DEFAULT_DOMAIN, ETHERTYPE_MSRP, ETHERTYPE_MVRP,
    };
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
    // `accumulated_latency_ns` is the talker's own source-to-wire latency;
    // bridges add to it as the attribute propagates. A zero claimed an
    // impossible zero-latency talker and made the 16R reject us as a live
    // source. `talker_tspec` derives the non-zero value from the class-A
    // packetizer cadence and pins it against captured reference endpoints.
    // ONE Talker Advertise PER STREAM. A Milan listener accepts no stream it
    // has not been advertised, so eight streams need eight declarations; one
    // declaration would have the 16R bind one and ignore the other seven
    // however many we transmitted. A brand-new talker attribute is declared
    // with New!: both Apple and the 16R do this on cold bring-up, whereas the
    // generic Join! path encoded JoinMt in our first on-wire declaration.
    for o in &out_streams {
        let advertise = talker_tspec(o.stream_id, o.talker.pdu_len());
        let max_frame_size = advertise.max_frame_size;
        srp.declare_new(now_mach(), Attribute::Talker(Talker::Advertise(advertise)));
        log.log(
            Level::Info,
            "msrp",
            &format!(
                "Talker Advertise declared: stream {} 0x{:016x} dest {} vid {} max_frame {}B 1 frame/interval prio {}",
                o.uid,
                o.stream_id.0,
                stream_dest_mac(o.stream_id),
                DEFAULT_DOMAIN.vid,
                max_frame_size,
                DEFAULT_DOMAIN.priority
            ),
        );
    }
    // The media clock needs its own reservation: it is a separate class A
    // stream with its own id and destination, and the 16R accepts no stream it
    // has not been advertised. `talker_tspec` derives max_frame_size from the
    // real AVTPDU, so this is 28 + 1 = 29 — Milan v1.3 Table 4.4's own value
    // for `CRF, 1 ts/pdu` plus the clock-tolerance byte the TSpec convention
    // already adds for the audio streams.
    if let Some(t) = &crf_talker {
        let advertise = talker_tspec(crf_stream_id, t.pdu_len());
        let max_frame_size = advertise.max_frame_size;
        srp.declare_new(now_mach(), Attribute::Talker(Talker::Advertise(advertise)));
        log.log(
            Level::Info,
            "msrp",
            &format!(
                "Talker Advertise declared: media clock {crf_uid} 0x{:016x} dest {} vid {} max_frame {}B 1 frame/interval prio {}",
                crf_stream_id.0,
                stream_dest_mac(crf_stream_id),
                DEFAULT_DOMAIN.vid,
                max_frame_size,
                DEFAULT_DOMAIN.priority
            ),
        );
    }

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
    // Presentation offset ahead of "now": the Milan MAXIMUM, 2 ms.
    //
    // NOT `PRESENTATION_FLOOR_NS + CLASS_A_INTERVAL_NS`. Those two constants
    // belong to the LISTENER's accept bound — `early_bound_ns` is
    // `max(ta_latency, 2 ms) + 125 us`, how far in the future an arriving PDU's
    // presentation time may sit and still be taken — and reusing them here
    // conflated the bound we TOLERATE with the offset we EMIT.
    //
    // Milan Baseline Interoperability Specification §7.2.1 fixes the emitted
    // one: a Listener PAAD must buffer received samples for up to 2.126 ms,
    // derived as "2ms (maximum presentation time offset) + 125us (timing
    // uncertainty) + 1us (gPTP accuracy)". So 2 ms is the MAXIMUM presentation
    // time offset a talker may use, and the 125 us is the listener's own
    // allowance on top of it — not ours to spend.
    //
    // At 2.125 ms we stamped 1 us inside a conforming listener's entire
    // buffer, leaving no margin for jitter; the 16R's own streams are observed
    // at +1.327 ms, comfortably under. The wire dump prints this offset for
    // both directions, so the change is checkable against the hardware.
    let presentation_offset_ns = PRESENTATION_FLOOR_NS;

    let mut consumer = avb_audioring::Consumer::new(out_ring.ring());
    let frame_bytes = out_ring.ring().frame_bytes();
    // One PDU's worth of FULL-WIDTH ring frames (64 channels), plus a
    // per-stream buffer the packetizer gets: it wants one stream's channels
    // interleaved, so each talker's eight are gathered out of the wide frame.
    let mut sample_scratch = vec![0u8; spf * frame_bytes];
    let stream_frame_bytes = channels * 4;
    let mut stream_scratch = vec![0u8; spf * stream_frame_bytes];
    let mut pdu_scratch = vec![0u8; out_streams.first().map_or(0, |o| o.talker.pdu_len())];
    // ---- Media clock (CRF) pacing state -----------------------------------
    //
    // 500 PDUs/s, one timestamp each, 2 ms apart. Paced on its OWN deadline
    // rather than every Nth audio tick: the audio interval is `spf / rate`
    // (125 us for this format) and 2 ms is a whole multiple of it only by
    // coincidence of these numbers, so deriving the CRF cadence from the audio
    // cadence would skew silently on a format whose interval does not divide
    // it.
    let crf_pdu_interval_ns = crf_talker.as_ref().map_or(0, |t| {
        let (num, den) = t.period_ns_exact();
        (num / den) as u64
    });
    let mut next_crf_ns: u64 = 0;
    // gPTP time of media-clock event 0, and the running event index.
    //
    // Timestamps are computed from the anchor and the event INDEX, never
    // accumulated, so rounding cannot drift (`CrfTalker::nth_timestamp`).
    // The anchor is dropped whenever gPTP loses its own anchor — a servo step
    // moves the clock, which makes every event time derived from the old
    // anchor wrong by the step.
    let mut crf_anchor_ns: Option<u64> = None;
    // Index of the NEXT timestamp, counting from the anchor.
    //
    // This is the TIMESTAMP index, not the media-clock event index:
    // `nth_timestamp` multiplies it by the time between consecutive
    // timestamps (`timestamp_interval / base_frequency` = 2 ms), so one PDU
    // advances it by one. The media-clock events covered are
    // `crf_ts_index * timestamp_interval`, which is what the summary reports.
    let mut crf_ts_index: u64 = 0;
    let mut crf_pdus: u64 = 0;
    // Times the timestamp basis was re-established mid-run. Each one is a
    // discontinuity in the series a listener is recovering a clock from, so a
    // nonzero count here is the first thing to look at if the 16R reports our
    // media clock as unstable.
    let mut crf_reanchors: u64 = 0;
    let mut crf_skipped_no_clock: u64 = 0;
    let mut crf_refused: u64 = 0;
    let mut crf_scratch = vec![0u8; crf_talker.as_ref().map_or(0, |t| t.pdu_len())];
    let mut crf_dumped = false;
    // One staged frame per outbound stream, so a whole observation interval
    // can go to the dext in ONE user-client call. Allocated once: the
    // per-packet path must not allocate (there is a test that arms an
    // allocation counter over it), and at 8000 intervals/s an allocation per
    // interval would be 8000 trips through the allocator per second.
    let mut tx_staged: Vec<TxFrame> = vec![TxFrame::EMPTY; out_streams.len()];
    // Which STREAM_OUTPUT each staged frame belongs to. A stream whose gather
    // or build failed is skipped, so the staged position is NOT the stream
    // index and FRAMES_TX per stream has to be attributed, not inferred from
    // a total divided by the stream count.
    let mut tx_staged_owner: Vec<usize> = vec![0; out_streams.len()];
    let mut frames_tx_per_stream: Vec<u64> = vec![0; out_streams.len()];
    let mut stream_starts: Vec<u64> = vec![0; out_streams.len()];
    // STREAM_STOP stays zero for the life of a run, and that is a real
    // reading rather than a missing one: `armed` is set once and never
    // cleared, so a stream that has started is never stopped again before
    // the process exits. If a disarm path is ever added, this is what it
    // must increment.
    let stream_stops: Vec<u64> = vec![0; out_streams.len()];

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
    // Peak |sample| we actually transmitted, and the tick counter that
    // throttles the measurement. 0 after a run that transmitted PDUs means the
    // protocol worked and the audio was silence.
    let mut tx_peak: i64 = 0;
    let mut tx_peak_tick: u64 = 0;
    // Observation intervals the loop could not service in time. Non-zero
    // means the per-tick work does not fit in 125 us, so the stream is gapped
    // — a different fault from the dext rate-limiting us, and it must not hide
    // behind the PDU count.
    let mut missed_intervals: u64 = 0;
    // When the talker armed, so the summary can report rates over the window
    // frames were actually being sent in rather than over the whole run, which
    // includes discovery, the AEM reads and the gPTP lock wait.
    let mut armed_at_ns: Option<u64> = None;
    // GET_COUNTERS + GET_STREAM_INFO reads of the 16R's own STREAM_INPUT
    // sinks, taken a few seconds AFTER we start transmitting. MEDIA_LOCKED
    // only means something while a stream is live, so this cannot be done in
    // the pre-stream descriptor phase. It answers the question
    // `listener_ready` cannot: that flag is the SRP layer saying the 16R
    // RESERVED the path, not that its audio engine locked to our packets.
    //
    // Sampled REPEATEDLY rather than once. A single sample cannot tell a
    // counter that is stuck at zero from one that has not been read at the
    // right moment, and it cannot see a lock that is gained and then lost —
    // which is what MEDIA_LOCKED and MEDIA_UNLOCKED both reading zero makes
    // ambiguous. Three samples spaced through the run resolve both.
    let mut sink_probes_done: u32 = 0;
    // Which of the 16R's STREAM_INPUT sinks to read GET_COUNTERS and
    // GET_STREAM_INFO from: every sink we bound an audio stream to, plus the
    // media-clock sink when we transmit CRF.
    //
    // The CRF sink has to be in here explicitly. It is not in `out_streams` —
    // that list is the audio pairing — so a probe loop over `out_streams`
    // alone would read every sink EXCEPT the one whose counters answer the
    // question this mode exists to ask: whether the 16R's MEDIA_LOCKED moves
    // when it receives our media clock.
    let probe_sinks: Vec<u16> = {
        let mut v: Vec<u16> = out_streams.iter().map(|o| o.sink_uid).collect();
        if crf_talker.is_some() && !v.contains(&CRF_SINK_UID) {
            v.push(CRF_SINK_UID);
        }
        v
    };
    // The stream each sink should report back once our bind has taken.
    //
    // This exists because the 16R's own CONNECTED flag is not sufficient
    // evidence of a binding. Its CRF sink 0 answers GET_STREAM_INFO with
    // CONNECTED set, `saved_assignment` set and `fast_connect` set while
    // reporting `stream_id = 0` and `dest_mac = 00:00:00:00:00:00` — the flag
    // says bound and the binding is empty. Trusting the flag made the connect
    // loop stop asking after one attempt, so the media clock was transmitted
    // for a whole run at exactly 500 PDU/s to a sink with no talker bound.
    let expected_stream: std::collections::BTreeMap<u16, u64> = {
        let mut m: std::collections::BTreeMap<u16, u64> = out_streams
            .iter()
            .map(|o| (o.sink_uid, o.stream_id.0))
            .collect();
        if crf_talker.is_some() {
            m.insert(CRF_SINK_UID, crf_stream_id.0);
        }
        m
    };
    // The media clock's uid when we actually source one, for labelling.
    let crf_uid_if_enabled: Option<u16> = crf_talker.as_ref().map(|_| crf_uid);
    // Our STREAM_OUTPUT unique id -> stream id, for the talker-side ACMP
    // responder.
    //
    // This MUST include the media clock. Its uid sits above every audio
    // stream and it is not in `out_streams`, so looking a probe up in that
    // list alone refuses the 16R's PROBE_TX for the CRF stream with
    // TALKER_UNKNOWN_ID — which is exactly what happened: the device reported
    // its CRF sink as probing ACTIVE with acmp_status 2 while we answered
    // every one of its probes with that refusal, so the bind could never
    // complete however correct the stream we were transmitting.
    let our_streams_by_uid: std::collections::BTreeMap<u16, StreamId> = {
        let mut m: std::collections::BTreeMap<u16, StreamId> =
            out_streams.iter().map(|o| (o.uid, o.stream_id)).collect();
        if crf_talker.is_some() {
            m.insert(crf_uid, crf_stream_id);
        }
        m
    };
    const SINK_PROBE_COUNT: u32 = 3;
    const SINK_PROBE_PERIOD_NS: u64 = 5_000_000_000;
    // Cadence of the connection watch once the diagnostic rounds are done.
    // Slower than the probes: this only has to notice an unbind, and the
    // daemon runs for hours.
    const SINK_WATCH_PERIOD_NS: u64 = 15_000_000_000;
    // Whether the current round logs every sink or only changes.
    let mut sink_watch_verbose = true;
    // ns_since(start) of the last probe/watch round, so the watch cadence is
    // measured from the previous round rather than from arming.
    let mut last_sink_round_ns: u64 = 0;
    // Per sink: (consecutive CONNECT_RX attempts, ns_since(start) of the next
    // permitted attempt). Reset when the device reports it connected.
    let mut sink_attempts: std::collections::BTreeMap<u16, (u32, u64)> =
        std::collections::BTreeMap::new();
    const SINK_CONNECT_BACKOFF_BASE_NS: u64 = 1_000_000_000;
    const SINK_CONNECT_BACKOFF_MAX_NS: u64 = 30_000_000_000;
    const SINK_CONNECT_COMPLAIN_AT: u32 = 4;
    // Sinks we have finished binding. Milan §5.5.1.4: after the Listener
    // acknowledges a binding, the controller "has no further involvement in
    // the connection process until an unbinding is desired". Entries are
    // never removed.
    let mut sink_bind_done: std::collections::BTreeSet<u16> = std::collections::BTreeSet::new();
    // ADP advertise state machine (Milan §5.6.3). `pending_advert_at` is the
    // DELAY state: Some(due) means TMR_DELAY is running and the
    // ENTITY_AVAILABLE goes out when it expires.
    let mut pending_advert_at: Option<u64> = None;
    // The grandmaster in our last advertisement, so GM_CHANGE (Table 5.55)
    // can be detected rather than waiting out the 5 s timer.
    let mut last_advertised_gm: Option<u64> = None;
    // How many ENTITY_DISCOVER probes we answered. Zero over a long run means
    // nothing is probing us; a climbing count means something is checking
    // whether we are alive, and before this existed we never replied.
    let mut adp_discovers_answered: u64 = 0;
    // PTP message types already hex-dumped, so each is captured once.
    let mut ptp_dumped: Vec<u8> = Vec::with_capacity(3);
    // What the 16R last told us about each of its sinks, keyed by sink index:
    // `Some(true)` = it reported CONNECTED, `Some(false)` = it reported not,
    // absent = we have not asked yet.
    //
    // This exists to stop CONNECT_RX being used as a keepalive. ACMP is a
    // connection-management protocol, not a heartbeat: a binding persists
    // until something unbinds it, so re-issuing CONNECT_RX for an already
    // bound sink every second is eight state-changing commands per second
    // that the device must process and report to its own controller. The user
    // watched banks "flash in and out" of UC Surface, and a controller
    // re-rendering mid-churn is the obvious way to produce that. So connect
    // until the device says it is connected, then stop asking and watch.
    let mut sink_connected: std::collections::BTreeMap<u16, bool> =
        std::collections::BTreeMap::new();
    // How many reconnects the watch triggered after an established binding
    // dropped. Nonzero means something really is unbinding us, which is worth
    // knowing separately from the initial connect.
    let mut sink_reconnects: u64 = 0;
    // One-shot wire dumps of a PDU in each direction, raw bytes plus decoded
    // fields.
    //
    // This exists because the 16R counts our frames (FRAMES_RX climbing at
    // exactly our PDU rate) while never media-locking, and reports zero on
    // every error counter it implements — no UNSUPPORTED_FORMAT, no
    // LATE_TIMESTAMP, no EARLY_TIMESTAMP, no SEQ_NUM_MISMATCH. No counter
    // names the fault, so the fault is in a header field the 16R reads
    // without counting a rejection.
    //
    // The 16R's OWN stream is the reference: identical format, and our
    // listener locks its media clock to it, so those bytes are known-good by
    // observation rather than by my reading of the spec. Dumping both and
    // diffing them field by field is the only check that can find a wrong
    // field nobody counts — a round trip through our own codec cannot, since
    // encode and decode would share the same mistake.
    let mut tx_pdu_dumped = false;
    let mut rx_pdu_dumped = false;
    let mut our_adp_dumped = false;
    // MAAP frames seen on the control ring, and how many we have dumped.
    // `maap_seen` matters even when it stays 0: that would mean MAAP is not
    // reaching us at all (the dext classifier, or a 16R that only announces
    // on allocation rather than periodically), which is a different problem
    // from a layout we cannot read.
    let mut maap_seen: u64 = 0;
    let mut maap_dumped: u32 = 0;
    let mut maap_tx: u64 = 0;
    let mut maap_acquired_logged = false;
    // Claim the whole 64-address block our per-stream destinations are derived
    // from, so one allocation covers every stream rather than 64 separate
    // claims. Starts probing immediately — a few hundred ms, well inside the
    // discovery window the talker already waits through.
    let mut maap = avb_avtp::maap::Maap::new(
        stream_dest_mac(avb_core::StreamId::from_parts(our_mac, 0)),
        64,
        0,
    );
    let mut last_connect = now_mach();
    let mut last_advert = now_mach();
    // AEM commands we answered, and how many we had to refuse. A run where the
    // 16R never asks us anything is a different failure from one where it asks
    // and we answer NOT_IMPLEMENTED, and the summary must tell them apart.
    let mut aecp_answered: u64 = 0;
    // EVERY command the 16R asks us, keyed by a readable label, counting
    // (asked, refused). Until this existed the log recorded only REFUSALS, so
    // "AEM commands answered=126" told us nothing about WHICH 126 — and the
    // question that actually matters is whether the 16R ever walks our
    // STREAM_OUTPUTs at all. It reports `avb_stream_fmt = []` for us and
    // `['None','Supported',...]` for Apple's entity, which means it has no
    // format information for our streams; this says whether that is because
    // it never asked or because we answered badly.
    let mut aecp_asked: std::collections::BTreeMap<String, (u64, u64)> =
        std::collections::BTreeMap::new();
    // Unsolicited notifications actually PUSHED. The registry has existed
    // since Phase 1a and was inert: registrations accepted, nothing ever
    // sent. A zero here while the 16R registered is the bug, not a quiet run.
    let mut unsol_sent: u64 = 0;
    let mut next_unsol_ns: u64 = UNSOL_COUNTERS_PERIOD_NS;
    // ACMP commands we answered AS THE TALKER. Zero while the 16R is
    // connecting means it never asked us to connect, which is a different
    // fault from asking and being refused.
    let mut acmp_listener_answered: u64 = 0;
    let mut acmp_talker_answered: u64 = 0;
    let mut aecp_refused: u64 = 0;
    // Milan Vendor-Unique commands answered. A Milan controller asks
    // GET_MILAN_INFO to decide we are a Milan device at all, so a run where
    // this stays 0 means no controller ever identified us as one.
    let mut mvu_answered: u64 = 0;
    // Refusals that are CORRECT: a controller walking a descriptor type past
    // its advertised count to find where the list ends. Counted separately so
    // a real gap in our model cannot hide inside a normal probe tally.
    let mut aecp_probes_past_end: u64 = 0;
    // Ticks skipped because gPTP had no valid clock anchor (startup, or just
    // after a servo step). Counted, not silently papered over.
    let mut ts_unavailable: u64 = 0;
    // Ticks transmitted while the servo had fallen out of LOCKED.
    let mut ticks_unlocked: u64 = 0;
    let mut warned_unlocked = false;
    /// How often the clock-role line is logged.
    const ROLE_LOG_PERIOD_NS: u64 = 10_000_000_000;
    let mut last_role_log_ns: u64 = 0;
    // CLOCK_DOMAIN LOCKED/UNLOCKED are EVENT counts, not a state: they count
    // transitions of the media clock acquiring and losing a disciplined
    // basis. `Decision.basis` is Some exactly when the sample clock is
    // disciplined, so its edges are the events.
    let mut clock_domain_locked: u32 = 0;
    let mut clock_domain_unlocked: u32 = 0;
    let mut clock_basis_was: Option<bool> = None;
    // Do not transmit until the clock is fit to timestamp with. Streaming on
    // an undisciplined clock is not a degraded stream, it is a stream the
    // listener will reject — and it is what made the earlier runs look like a
    // transmit problem when the clock was the issue.
    let mut armed = false;
    let mut last_gptp_push = now_mach();
    let mut last_unsolicited_ptp_gm: Option<u64> = None;
    let mut last_gptp_service = now_mach();

    // ---- Inbound direction (duplex only) -----------------------------------
    //
    // The 16R → Mac half. The data path itself is `listenerrx`, shared with
    // `streams_loop`. What is needed here is the declaration side: the 16R
    // only streams to a listener that has declared MSRP Listener Ready for its
    // stream, and the dext only delivers frames matching an installed filter.
    // Multi-slot in duplex (one slot per 16R audio source, in its own
    // descriptor-index order), single-stream otherwise.
    let mut lrx = crate::listenerrx::ListenerRx::with_streams(inbound_stream_ids.clone());
    // Dext RX filter entries: (stream_id, dest_mac, vlan).
    let mut in_filter: Vec<(u64, [u8; 6], u16)> = Vec::new();
    // Stream ids we have already declared Listener Ready for.
    let mut in_declared: std::collections::BTreeSet<u64> = Default::default();
    let mut in_connect_seq: u16 = 1000;
    let mut last_in_connect = MachTicks(0);
    let mut in_slots: u64 = 0;
    let mut in_decode_fail: u64 = 0;

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
            log.log(Level::Error, dir, &format!("dext fault {}", h.fault));
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
                // One-shot RAW capture of the 16R's own Announce, Sync and
                // Follow_Up, per message type.
                //
                // We need to ENCODE these three to become Grandmaster Capable
                // (Milan v1.3 §4.3.1.1), and `msg.rs` has encoders only for
                // Pdelay and Signaling -- Announce/Sync/Follow_Up are
                // decode-only by design, because this port was built
                // slave-only. Writing those encoders from the standard's field
                // tables is exactly how this project shipped a guessed
                // bit-packed constant before (the WriteMix FourCC, which
                // passed its own round-trip tests and failed only on real
                // hardware). The 16R is the grandmaster we already slave to,
                // so its messages are a correct-by-construction anchor.
                //
                // Dumped raw and undecoded on purpose: a decoder written to a
                // guessed layout would misread the very bytes being captured.
                let mt = p.payload.first().map(|b| b & 0x0F);
                let want = matches!(mt, Some(0x0B) | Some(0x00) | Some(0x08));
                if want && ptp_dumped.len() < 3 {
                    if let Some(m) = mt {
                        if !ptp_dumped.contains(&m) {
                            ptp_dumped.push(m);
                            let name = match m {
                                0x0B => "ANNOUNCE",
                                0x00 => "SYNC",
                                _ => "FOLLOW_UP",
                            };
                            let hex: String = p
                                .payload
                                .iter()
                                .map(|b| format!("{b:02x}"))
                                .collect::<Vec<_>>()
                                .join(" ");
                            log.log(
                                Level::Info,
                                "wire",
                                &format!("PTP {name} from 16R, {} B: [{hex}]", p.payload.len()),
                            );
                        }
                    }
                }
                gptp.handle_rx(now_mach(), &slot);
                continue;
            }
            // ---- ADP: answer a liveness probe ------------------------------
            //
            // The streaming loop did not decode ADP at all before this. The
            // three `table.observe` call sites are all in the DISCOVERY
            // helpers, and each discards the returned `AdpUpdate`, so a
            // received ENTITY_DISCOVER was never even looked at once streaming
            // began.
            //
            // Milan v1.3 §5.6.3.1: "Upon reception of an ADP ENTITY_DISCOVER
            // message, the PAAD-AE shall: 1) Extract the entity_id field of
            // the message. 2) If the entity_id is 0 or equal to the Entity ID
            // of the PAAD-AE, then continue processing the message according
            // to the state machine below (RCV_ADP_DISCOVER event). Otherwise,
            // discard the message." §5.6.3.5.4 then stops TMR_ADVERTISE,
            // starts TMR_DELAY and enters DELAY -- an ENTITY_AVAILABLE after a
            // random 0-4 s wait.
            //
            // Silence here is indistinguishable from an absent device to a
            // controller that probes one entity for liveness, which is what a
            // remembered-but-"offline" talker looks like from the other side.
            if p.hdr.ethertype == avb_avtp::common::ETHERTYPE_AVTP
                && p.payload.first().copied() == Some(avb_avtp::common::subtype::ADP)
                && p.hdr.src != our_mac
            {
                if let Some(adp_in) = avb_avdecc::adp::Adpdu::decode(p.payload) {
                    let for_us = adp_in.entity_id == avb_core::Eui64::from_u64(0)
                        || adp_in.entity_id == our_id;
                    if adp_in.is_discover() && for_us {
                        adp_discovers_answered = adp_discovers_answered.saturating_add(1);
                        // Step 1 STOPS TMR_ADVERTISE, so the discover replaces
                        // the periodic advertisement rather than adding to it.
                        last_advert = now_mach();
                        pending_advert_at =
                            Some(ns_since(start).saturating_add(adp_random_delay_ns()));
                    }
                }
            }
            // ---- MAAP: capture the 16R's own allocation traffic -----------
            //
            // Milan §6.5.1 requires a PAAD to implement MAAP, and §6.3.1 makes
            // MAAP allocation a precondition for a Talker attribute's
            // destination address being VALID. We do not implement it, which
            // is the open conformance violation.
            //
            // Before originating MAAP PDUs from the spec, capture the 16R's:
            // it is Milan-certified, so its own stream addresses
            // (91:E0:F0:00:CE:F4..FB) must have been allocated this way, and
            // its PDUs are a hardware anchor for the Annex B layout. This
            // project has shipped a guessed bit-packed field before -- the
            // WriteMix FourCC -- and the rule since is to transcribe a layout
            // from hardware rather than originate one.
            //
            // Dumped RAW and unconditionally, no decode: a decoder written to
            // a guessed layout would misread these bytes and hide the very
            // thing being captured.
            if p.hdr.ethertype == avb_avtp::common::ETHERTYPE_AVTP
                && p.payload.first().copied() == Some(avb_avtp::common::subtype::MAAP)
            {
                maap_seen = maap_seen.saturating_add(1);
                if maap_dumped < 4 {
                    maap_dumped += 1;
                    let hex: String = p
                        .payload
                        .iter()
                        .take(44)
                        .map(|b| format!("{b:02x}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    log.log(
                        Level::Info,
                        "wire",
                        &format!(
                            "MAAP from {:02x?} -> dst {:02x?}: [{hex}] ({} payload B)",
                            p.hdr.src.octets(),
                            p.hdr.dst.octets(),
                            p.payload.len()
                        ),
                    );
                }
                // Our own announces come back to us on the multicast; feeding
                // them in would read as a conflict with ourselves and move us
                // off a block we legitimately hold, forever.
                if p.hdr.src != our_mac {
                    if let Some(m) = avb_avtp::maap::MaapPdu::decode(p.payload) {
                        let before = maap.block().0;
                        let act = maap.on_rx(&m, ns_since(start), |attempt| {
                            // Relocate through the SAME derivation the stream
                            // addresses come from, offset by the attempt, so a
                            // moved block stays inside the MAAP pool and the
                            // per-stream addresses keep following the base.
                            avb_core::maap_stream_dest(
                                our_mac,
                                attempt.saturating_mul(64) as u16,
                                64,
                            )
                        });
                        if maap.block().0 != before {
                            log.log(
                                Level::Warn,
                                "maap",
                                &format!(
                                    "conflict on our block {:02x?} from {:02x?} — moved to \
                                     {:02x?} and re-probing",
                                    before.octets(),
                                    p.hdr.src.octets(),
                                    maap.block().0.octets()
                                ),
                            );
                        }
                        if let Some(avb_avtp::maap::Action::Send(d)) = act {
                            let hdr = EthHeader {
                                dst: avb_avtp::maap::MAAP_MULTICAST,
                                src: our_mac,
                                vlan: None,
                                ethertype: avb_avtp::common::ETHERTYPE_AVTP,
                            };
                            if let Ok(f) = TxFrame::build(&hdr, &d.encode(), false, 0) {
                                let _ = dext.send(f.bytes(), false, 0);
                            }
                            maap_tx = maap_tx.saturating_add(1);
                        }
                    }
                }
                continue;
            }
            // ---- ACMP: answer the listener binding our stream -------------
            //
            // Our CONNECT_RX_COMMAND tells the 16R to connect; the 16R then
            // sends US a CONNECT_TX_COMMAND as the talker, and only after our
            // CONNECT_TX_RESPONSE does it declare MSRP Listener Ready. This
            // branch did not exist, so the 16R waited on a response that never
            // came: `listener_ready` stayed false and every PDU we transmitted
            // was discarded — which on the wire looks exactly like a transmit
            // fault.
            if p.hdr.ethertype == avb_avtp::common::ETHERTYPE_AVTP
                && p.payload.first().copied() == Some(avb_avdecc::constants::subtype::ACMP)
            {
                let Some(acmp) = avb_avdecc::Acmpdu::decode(p.payload) else {
                    continue;
                };
                // The 16R's persisted routes may name a retired CRF output
                // rather than an audio stream. Log every incoming CONNECT_TX
                // command's complete routing tuple before resolving its UID,
                // so the next managed restart can distinguish that stale-route
                // case from an audio unique-ID convention mismatch without
                // changing a binding or guessing a new index scheme.
                if acmp.message_type == avb_avdecc::constants::acmp_msg::CONNECT_TX_COMMAND {
                    let key = format!(
                        "connect-tx-in-{:016x}-{}-{}",
                        u64::from_be_bytes(acmp.talker_entity_id.0),
                        acmp.talker_unique_id,
                        acmp.listener_unique_id,
                    );
                    log.log_rate_limited(
                        Level::Info,
                        "acmp",
                        &key,
                        &format!(
                            "inbound CONNECT_TX: controller=0x{:016x} talker=0x{:016x}/uid={} \
                             listener=0x{:016x}/uid={} stream=0x{:016x} dest={} vid={} \
                             count={} seq={} flags=0x{:04x}",
                            u64::from_be_bytes(acmp.controller_entity_id.0),
                            u64::from_be_bytes(acmp.talker_entity_id.0),
                            acmp.talker_unique_id,
                            u64::from_be_bytes(acmp.listener_entity_id.0),
                            acmp.listener_unique_id,
                            acmp.stream_id.0,
                            acmp.stream_dest_mac,
                            acmp.stream_vlan_id,
                            acmp.connection_count,
                            acmp.sequence_id,
                            acmp.flags,
                        ),
                    );
                }
                // Answer for the stream the listener NAMED, not always the
                // first: a CONNECT_TX carries `talker_unique_id`, and handing
                // back stream 0's addressing for a request about stream 5
                // would have the 16R reserve one stream and listen for
                // another. An id we do not have falls through to
                // `talker_response`, which refuses it with TALKER_UNKNOWN_ID
                // rather than substituting ours.
                //
                // Resolved against EVERY stream we source, media clock
                // included — `out_streams` is only the audio pairing.
                let named = our_streams_by_uid
                    .get(&acmp.talker_unique_id)
                    .map(|&sid| (acmp.talker_unique_id, sid));
                let ts = match named {
                    Some((uid, sid)) => avb_avdecc::TalkerStream {
                        our_id,
                        our_unique_id: uid,
                        stream_id: sid,
                        stream_dest_mac: stream_dest_mac(sid),
                        stream_vlan_id: DEFAULT_DOMAIN.vid,
                        connection_count: 1,
                    },
                    None => avb_avdecc::TalkerStream {
                        our_id,
                        // Deliberately an id we do not serve, so the refusal
                        // comes from the one place that decides it.
                        our_unique_id: u16::MAX,
                        stream_id: StreamId(0),
                        stream_dest_mac: avb_core::MacAddr([0; 6]),
                        stream_vlan_id: DEFAULT_DOMAIN.vid,
                        connection_count: 0,
                    },
                };
                if let Some(resp) = avb_avdecc::talker_response(&acmp, ts) {
                    let hdr = EthHeader {
                        dst: avb_core::AVDECC_MULTICAST,
                        src: our_mac,
                        vlan: None,
                        ethertype: avb_avtp::common::ETHERTYPE_AVTP,
                    };
                    if let Ok(f) = TxFrame::build(&hdr, &resp.encode(), false, 0) {
                        let _ = dext.send(f.bytes(), false, 0);
                    }
                    acmp_talker_answered += 1;
                    log.log(
                        Level::Info,
                        "acmp",
                        &format!(
                            "answered {} from the 16R: type 0x{:02x} -> 0x{:02x} \
                             talker_unique_id={} ({}) status {}",
                            if acmp.message_type
                                == avb_avdecc::constants::acmp_msg::CONNECT_TX_COMMAND
                            {
                                "CONNECT_TX_COMMAND"
                            } else {
                                "talker command"
                            },
                            acmp.message_type,
                            resp.message_type,
                            acmp.talker_unique_id,
                            // Which of OUR streams that id resolved to, or
                            // that it resolved to none. Without this the
                            // refusal read as an unexplained "status 2" and
                            // took a hardware run to attribute.
                            match named {
                                Some((uid, _)) if Some(uid) == crf_uid_if_enabled =>
                                    "our media clock".to_string(),
                                Some((uid, _)) => format!("our audio stream {uid}"),
                                None => "NOT ONE OF OURS".to_string(),
                            },
                            resp.status
                        ),
                    );
                    continue;
                }
                // ---- listener-side ACMP: we advertise sinks, so we answer --
                //
                // Milan §5.5.3.1. Reached only when `talker_response` declined
                // the PDU, so a command addressed to us as a TALKER is never
                // also fed to a sink.
                if let Some((resp, actions)) = entity.handle_acmp_listener(
                    &acmp,
                    ns_since(start),
                    // Same deterministic spread the listen path uses: the
                    // SM wants a per-sink delay so eight sinks do not
                    // probe in lockstep.
                    (u64::from(acmp.listener_unique_id) * 137_000_000) % 1_000_000_000,
                ) {
                    let hdr = EthHeader {
                        dst: avb_core::AVDECC_MULTICAST,
                        src: our_mac,
                        vlan: None,
                        ethertype: avb_avtp::common::ETHERTYPE_AVTP,
                    };
                    if let Ok(f) = TxFrame::build(&hdr, &resp.encode(), false, 0) {
                        let _ = dext.send(f.bytes(), false, 0);
                    }
                    acmp_listener_answered += 1;
                    log.log(
                        Level::Info,
                        "acmp",
                        &format!(
                            "answered listener command type 0x{:02x} -> 0x{:02x} status {} \
                             for sink {}; {} action(s) from the sink SM",
                            acmp.message_type,
                            resp.message_type,
                            resp.status,
                            acmp.listener_unique_id,
                            actions.len(),
                        ),
                    );
                    // The sink SM's actions are MSRP declarations and probe
                    // sends. Logged rather than performed: the duplex inbound
                    // path drives its own binding as its own controller, so
                    // acting on them here would have two owners issuing SRP
                    // for one sink. Answering the controller is the
                    // obligation; re-binding on its behalf is not.
                    for a in &actions {
                        log.log(
                            Level::Info,
                            "acmp",
                            &format!(
                                "sink {} SM action (not applied): {a:?}",
                                acmp.listener_unique_id
                            ),
                        );
                    }
                }
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
                // Milan Vendor-Unique first. An MVU PDU is an AECPDU with
                // message_type VENDOR_UNIQUE_COMMAND, so `AemPdu::decode`
                // rejects it and the old code `continue`d — the controller got
                // SILENCE, which is how a Milan controller concludes we are
                // not a Milan device and declines to offer our streams for
                // routing. GET_MILAN_INFO is the question it asks.
                if let Some(mvu) = avb_avdecc::aecp::MvuPdu::decode(p.payload) {
                    if mvu.target_entity_id == our_id
                        && mvu.message_type
                            == avb_avdecc::constants::aecp_msg::VENDOR_UNIQUE_COMMAND
                    {
                        let resp = entity.handle_mvu(&mvu);
                        let hdr = EthHeader {
                            dst: p.hdr.src,
                            src: our_mac,
                            vlan: None,
                            ethertype: avb_avtp::common::ETHERTYPE_AVTP,
                        };
                        if let Ok(frame) = TxFrame::build(&hdr, &resp.encode(), false, 0) {
                            let _ = dext.send(frame.bytes(), false, 0);
                        }
                        mvu_answered += 1;
                        log.log(
                            Level::Info,
                            "mvu",
                            &format!(
                                "answered MVU cmd=0x{:04x} from {} -> status {}",
                                mvu.command_type, p.hdr.src, resp.status
                            ),
                        );
                        continue;
                    }
                }
                let Some(cmd) = avb_avdecc::aecp::AemPdu::decode(p.payload) else {
                    continue;
                };
                // The 16R's answer to our GET_COUNTERS. Dump the raw
                // counters_valid bitmap and every NON-ZERO counter with its
                // bit index rather than decoding against assumed bit
                // positions: this project has been burned by originating a
                // bit-packed field, and a misread bit here would invent an
                // answer to the exact question we are trying to settle.
                if cmd.message_type == avb_avdecc::constants::aecp_msg::AEM_RESPONSE
                    && cmd.command_type == avb_avdecc::constants::aem_cmd::GET_COUNTERS
                {
                    let st = cmd.status;
                    // descriptor_type(2) descriptor_index(2) counters_valid(4)
                    // then 32 u32 counters (1722.1-2021 Table 7.165).
                    let dty = cmd
                        .payload
                        .get(0..2)
                        .and_then(|b| <[u8; 2]>::try_from(b).ok())
                        .map(u16::from_be_bytes);
                    let didx = cmd
                        .payload
                        .get(2..4)
                        .and_then(|b| <[u8; 2]>::try_from(b).ok())
                        .map(u16::from_be_bytes);
                    let valid = cmd
                        .payload
                        .get(4..8)
                        .and_then(|b| <[u8; 4]>::try_from(b).ok())
                        .map(u32::from_be_bytes);
                    let mut nz: Vec<String> = Vec::new();
                    for i in 0..32usize {
                        let a = 8 + i * 4;
                        let v = cmd
                            .payload
                            .get(a..a + 4)
                            .and_then(|b| <[u8; 4]>::try_from(b).ok())
                            .map(u32::from_be_bytes)
                            .unwrap_or(0);
                        if v != 0 {
                            nz.push(format!("bit{i}={v}"));
                        }
                    }
                    log.log(
                        Level::Info,
                        "aem",
                        &format!(
                            "16R GET_COUNTERS status={st} type={dty:?} index={didx:?} \
                             counters_valid={} non-zero: [{}] ({} payload B)",
                            valid.map_or("?".to_string(), |v| format!("0x{v:08x}")),
                            nz.join(" "),
                            cmd.payload.len()
                        ),
                    );
                    continue;
                }
                // The 16R's answer to our GET_STREAM_INFO: its OWN view of the
                // binding (1722.1-2021 §7.4.16.2). The byte-aligned fields are
                // what matter here, because they are directly comparable
                // against what we transmit — if the stream id and destination
                // MAC we read back are the ones we advertised, the decode is
                // self-validating, and if they are not, that mismatch IS the
                // bug. The flag words are dumped raw rather than decoded
                // against assumed bit positions.
                if cmd.message_type == avb_avdecc::constants::aecp_msg::AEM_RESPONSE
                    && cmd.command_type == avb_avdecc::constants::aem_cmd::GET_STREAM_INFO
                {
                    let p = &cmd.payload;
                    let u16at = |o: usize| {
                        p.get(o..o + 2)
                            .and_then(|b| <[u8; 2]>::try_from(b).ok())
                            .map(u16::from_be_bytes)
                    };
                    let u32at = |o: usize| {
                        p.get(o..o + 4)
                            .and_then(|b| <[u8; 4]>::try_from(b).ok())
                            .map(u32::from_be_bytes)
                    };
                    let u64at = |o: usize| {
                        p.get(o..o + 8)
                            .and_then(|b| <[u8; 8]>::try_from(b).ok())
                            .map(u64::from_be_bytes)
                    };
                    let didx = u16at(2);
                    let dmac = p
                        .get(28..34)
                        .map(|b| {
                            b.iter()
                                .map(|x| format!("{x:02x}"))
                                .collect::<Vec<_>>()
                                .join(":")
                        })
                        .unwrap_or_else(|| "?".into());
                    // What WE send to that sink, for side-by-side comparison.
                    let ours = out_streams
                        .iter()
                        .find(|o| Some(o.sink_uid) == didx)
                        .map(|o| (o.stream_id.0, stream_dest_mac(o.stream_id)));
                    log.log(
                        Level::Info,
                        "aem",
                        &format!(
                            "16R GET_STREAM_INFO status={} sink={didx:?} flags={} \
                             format={} stream_id={} dest_mac={dmac} vlan={:?} \
                             msrp_acc_lat={:?} msrp_failure_code={:?} flags_ex={} \
                             probing/acmp=0x{:02x} ({} payload B)",
                            cmd.status,
                            u32at(4).map_or("?".into(), |v| format!("0x{v:08x}")),
                            u64at(8).map_or("?".into(), |v| format!("0x{v:016x}")),
                            u64at(16).map_or("?".into(), |v| format!("0x{v:016x}")),
                            u16at(44),
                            u32at(24),
                            p.get(34),
                            u32at(48).map_or("?".into(), |v| format!("0x{v:08x}")),
                            p.get(52).copied().unwrap_or(0),
                            p.len()
                        ),
                    );
                    // Say the connect bits in words. `SAVED_STATE` clear is
                    // the 16R telling us the CONTROLLER never assigned that
                    // bank -- not a fault of ours, and not something we can
                    // fix by sending more. It is the one reading that tells
                    // "we failed to bind" apart from "nobody asked us to".
                    if let Some(fl) = u32at(4) {
                        use avb_avdecc::milan::stream_info_flags as sif;
                        let yn = |b: u32| if fl & b != 0 { "yes" } else { "no" };
                        // Remember it, so the connect loop can stop asking.
                        let mut changed = false;
                        if let Some(ix) = didx {
                            // A binding counts only when the device reports
                            // OUR stream id on that sink. The CONNECTED flag
                            // alone is not evidence: sink 0 sets it while
                            // reporting stream_id 0, so believing the flag
                            // makes an empty sink look bound and stops the
                            // retry that would have bound it.
                            let reported_sid = u64at(16).unwrap_or(0);
                            let sid_ok = match expected_stream.get(&ix) {
                                Some(&want) => reported_sid == want,
                                // A sink we never bound: nothing to compare
                                // against, so the flag is all we have.
                                None => true,
                            };
                            let flag_conn = fl & sif::CONNECTED != 0;
                            let now_conn = flag_conn && sid_ok;
                            if flag_conn && !sid_ok {
                                log.log(
                                    Level::Warn,
                                    "aem",
                                    &format!(
                                        "sink {ix} reports CONNECTED but carries stream \
                                         0x{reported_sid:016x}, not the 0x{:016x} we bound \
                                         — treating it as UNBOUND and retrying, because the \
                                         flag describes the sink's saved state rather than a \
                                         live binding",
                                        expected_stream.get(&ix).copied().unwrap_or(0),
                                    ),
                                );
                            }
                            let prev = sink_connected.insert(ix, now_conn);
                            changed = prev != Some(now_conn);
                            if now_conn {
                                // Clear the backoff: if this binding later
                                // drops, the retry starts fast again rather
                                // than inheriting a 30 s delay.
                                sink_attempts.remove(&ix);
                            }
                            if prev == Some(true) && !now_conn {
                                sink_reconnects += 1;
                                log.log(
                                    Level::Warn,
                                    "aem",
                                    &format!(
                                        "sink {ix} was bound and is now UNBOUND — \
                                         NOT re-binding: an unbind is a \
                                         controller's deliberate act, and Milan \
                                         §5.5.1.4 makes re-establishment the \
                                         listener's own Auto Connect duty"
                                    ),
                                );
                            }
                        }
                        if sink_watch_verbose || changed {
                            log.log(
                                Level::Info,
                                "aem",
                                &format!(
                                    "   sink={didx:?} connected={} saved_assignment={} \
                                     fast_connect={}{}",
                                    yn(sif::CONNECTED),
                                    yn(sif::SAVED_STATE),
                                    yn(sif::FAST_CONNECT),
                                    if fl & sif::SAVED_STATE == 0 {
                                        " -- assign this bank in the 16R's controller"
                                    } else {
                                        ""
                                    }
                                ),
                            );
                        }
                    }
                    if let Some((sid, dm)) = ours {
                        let id_ok = u64at(16) == Some(sid);
                        let mac_ok = p.get(28..34) == Some(&dm.octets()[..]);
                        log.log(
                            if id_ok && mac_ok {
                                Level::Info
                            } else {
                                Level::Warn
                            },
                            "aem",
                            &format!(
                                "   we transmit stream_id=0x{sid:016x} dest_mac={dm} \
                                 -> stream_id {} dest_mac {}",
                                if id_ok { "MATCHES" } else { "DIFFERS" },
                                if mac_ok { "MATCHES" } else { "DIFFERS" },
                            ),
                        );
                    }
                    continue;
                }
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
                {
                    // Label the command, naming the descriptor type for a
                    // READ_DESCRIPTOR so the tally shows the SHAPE of the
                    // walk rather than one opaque total.
                    let label =
                        if cmd.command_type == avb_avdecc::constants::aem_cmd::READ_DESCRIPTOR {
                            let dt = cmd
                                .payload
                                .get(4..6)
                                .and_then(|b| <[u8; 2]>::try_from(b).ok())
                                .map(u16::from_be_bytes);
                            match dt {
                                Some(t) => format!(
                                    "READ_DESCRIPTOR {}",
                                    avb_avdecc::constants::desc::name(t)
                                        .map_or_else(|| format!("0x{t:04x}"), str::to_string)
                                ),
                                None => "READ_DESCRIPTOR (unparseable)".to_string(),
                            }
                        } else {
                            format!("command 0x{:04x}", cmd.command_type)
                        };
                    let e = aecp_asked.entry(label).or_insert((0, 0));
                    e.0 += 1;
                    if resp.status != avb_avdecc::constants::aecp_status::SUCCESS {
                        e.1 += 1;
                    }
                }
                if resp.status != avb_avdecc::constants::aecp_status::SUCCESS {
                    aecp_refused += 1;
                    // Name WHAT we refused, at warn level. A refusal here is
                    // the 16R asking for part of our model we do not return,
                    // and a Milan listener that cannot finish enumerating a
                    // talker will not bind its stream — so this is the first
                    // thing to read when `listener_ready` stays false. For a
                    // READ_DESCRIPTOR the payload is
                    // configuration_index(2) reserved(2) type(2) index(2).
                    let what =
                        if cmd.command_type == avb_avdecc::constants::aem_cmd::READ_DESCRIPTOR {
                            let dt = cmd
                                .payload
                                .get(4..6)
                                .and_then(|b| <[u8; 2]>::try_from(b).ok())
                                .map(u16::from_be_bytes);
                            let di = cmd
                                .payload
                                .get(6..8)
                                .and_then(|b| <[u8; 2]>::try_from(b).ok())
                                .map(u16::from_be_bytes);
                            match (dt, di) {
                                (Some(t), Some(i)) => {
                                    format!("READ_DESCRIPTOR type=0x{t:04x} index={i}")
                                }
                                _ => "READ_DESCRIPTOR (unparseable payload)".to_string(),
                            }
                        } else {
                            format!("command 0x{:04x}", cmd.command_type)
                        };
                    // A refusal is not automatically a defect. A controller
                    // may walk a type until it is refused in order to find
                    // where the list ends, so a READ_DESCRIPTOR for an index
                    // AT OR BEYOND what we advertise is normal discovery and
                    // the correct answer is NO_SUCH_DESCRIPTOR. What matters
                    // is a refusal for something we DO advertise: that leaves
                    // the controller's model of us incomplete, and a Milan
                    // listener will not present a talker it could not fully
                    // enumerate. Conflating the two is how the real blocker
                    // (no CONTROL descriptor at all) sat in the log for a day
                    // looking the same as a harmless end-of-list probe.
                    let advertised =
                        if cmd.command_type == avb_avdecc::constants::aem_cmd::READ_DESCRIPTOR {
                            let dt = cmd
                                .payload
                                .get(4..6)
                                .and_then(|b| <[u8; 2]>::try_from(b).ok())
                                .map(u16::from_be_bytes);
                            let di = cmd
                                .payload
                                .get(6..8)
                                .and_then(|b| <[u8; 2]>::try_from(b).ok())
                                .map(u16::from_be_bytes);
                            match (dt, di) {
                                (Some(t), Some(i)) => entity
                                    .descriptor_counts()
                                    .iter()
                                    .any(|&(ty, n)| ty == t && i < n),
                                _ => false,
                            }
                        } else {
                            true
                        };
                    if advertised {
                        log.log(
                            Level::Warn,
                            "aem",
                            &format!(
                                "refused the 16R's {what} with status {} — we ADVERTISE \
                                 that descriptor, so its model of us stays incomplete \
                                 and it will not present us as routable",
                                resp.status
                            ),
                        );
                    } else {
                        log.log(
                            Level::Info,
                            "aem",
                            &format!(
                                "answered NO_SUCH_DESCRIPTOR to the 16R's {what} — we do \
                                 not advertise it, so this is the controller finding the \
                                 end of the list, not a gap in our model"
                            ),
                        );
                        aecp_refused -= 1;
                        aecp_probes_past_end += 1;
                    }
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
                // The 16R accepting one of our streams. Tracked PER STREAM:
                // with eight advertised it can accept some and not others,
                // and one flag would report the whole direction connected on
                // the strength of a single acceptance.
                SrpEvent::ListenerRegistered { stream_id, .. } => {
                    if let Some(o) = out_streams
                        .iter_mut()
                        .find(|o| o.stream_id.0 == stream_id.0)
                    {
                        if !o.ready {
                            o.ready = true;
                            log.log(
                                Level::Info,
                                "msrp",
                                &format!(
                                    "16R declared Listener Ready for our stream {} (0x{:016x}) — accepted",
                                    o.uid, o.stream_id.0
                                ),
                            );
                        }
                    }
                    // `StreamInfo` carries one stream, so the entity reports
                    // the first. The per-stream truth is in `out_streams`.
                    if let Some(first) = out_streams.first() {
                        if first.ready && !listener_ready {
                            listener_ready = true;
                            entity.set_stream_info(avb_avdecc::entity::StreamInfo {
                                connected: true,
                                dest_mac: stream_dest_mac(first.stream_id),
                                vlan_id: DEFAULT_DOMAIN.vid,
                                // MUST be the value we actually declare in the
                                // Talker Advertise. Reporting 0 here while
                                // MSRP declares our real transmit latency
                                // makes the entity contradict the wire, and a
                                // controller reading GET_STREAM_INFO sizes its
                                // buffer from THIS number.
                                msrp_accumulated_latency_ns:
                                    avb_srp::attr::TALKER_INITIAL_ACCUMULATED_LATENCY_NS,
                            });
                            unsol_sent += push_unsolicited(dext, our_mac, &entity, log);
                        }
                    }
                }
                SrpEvent::ListenerWithdrawn { stream_id, .. } => {
                    if let Some(o) = out_streams
                        .iter_mut()
                        .find(|o| o.stream_id.0 == stream_id.0)
                    {
                        if o.ready {
                            o.ready = false;
                            log.log(
                                Level::Warn,
                                "msrp",
                                &format!(
                                    "16R withdrew Listener Ready for our stream {} (0x{:016x})",
                                    o.uid, o.stream_id.0
                                ),
                            );
                        }
                    }
                    if let Some(first) = out_streams.first() {
                        if !first.ready && listener_ready {
                            listener_ready = false;
                            entity.set_stream_info(avb_avdecc::entity::StreamInfo {
                                connected: false,
                                dest_mac: stream_dest_mac(first.stream_id),
                                vlan_id: DEFAULT_DOMAIN.vid,
                                msrp_accumulated_latency_ns:
                                    avb_srp::attr::TALKER_INITIAL_ACCUMULATED_LATENCY_NS,
                            });
                            unsol_sent += push_unsolicited(dext, our_mac, &entity, log);
                        }
                    }
                }
                _ => {}
            }
            // Inbound: the 16R advertising ITS talker is our cue to declare
            // Listener Ready and install the dext filter. Without the
            // declaration it never streams to us; without the filter the dext
            // never delivers what it does send. This is the mirror of our own
            // Talker Advertise above, which is what makes the 16R accept us.
            if duplex {
                if let SrpEvent::TalkerRegistered {
                    stream_id,
                    talker: t,
                } = ev
                {
                    // Never our own stream reflected back at us.
                    if stream_id.0 != our_stream_id.0 {
                        if let Some(code) = t.failure_code() {
                            // A Talker FAILED is not a usable advertise: the
                            // 16R is saying it will not stream this source.
                            log.log(
                                Level::Warn,
                                "srp",
                                &format!(
                                    "16R TALKER FAILED id=0x{:016x} code={code} — it refuses to stream this source to us",
                                    stream_id.0
                                ),
                            );
                        } else {
                            let adv = t.advertise();
                            let dmac = adv.dest_mac.octets();
                            // The dext rejects vid 0, so fall back to the SR
                            // class A VID we declared.
                            let ta_vid = if adv.vlan_id == 0 {
                                DEFAULT_DOMAIN.vid
                            } else {
                                adv.vlan_id
                            };
                            in_filter.retain(|(_, m, _)| *m != dmac);
                            in_filter.push((stream_id.0, dmac, ta_vid));
                            dext.set_filter(&in_filter);
                            if in_declared.insert(stream_id.0) {
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
                                        "16R talker advertise: id=0x{:016x} dest={dmac:02x?} vid={ta_vid} -> declared Listener Ready",
                                        stream_id.0
                                    ),
                                );
                            }
                        }
                    }
                }
            }
        }
        // Run the MRP timers and put whatever they produced on the wire.
        srp.poll(srp_now(), &mut srp_out);
        while let Some(f) = srp_out.pop() {
            let _ = dext.send(f.bytes(), false, 0);
        }

        // Keep our ADP advertisement alive so the 16R keeps us in its entity
        // table, on the cadence Milan v1.3 Table 5.55 specifies: TMR_ADVERTISE
        // is "a timer with a fixed value of 5 seconds", and each expiry goes
        // through TMR_DELAY -- "a random value between 0 and 4 seconds"
        // (§5.6.3.5.5) -- before the ENTITY_AVAILABLE actually goes out. The
        // random delay is the anti-storm measure: without it, every PAAD on a
        // network that came up together advertises in lockstep forever.
        //
        // This used to be a bare 2 s loop with no delay state.
        if pending_advert_at.is_none() && ns_since(last_advert) >= ADP_TMR_ADVERTISE_NS {
            pending_advert_at = Some(ns_since(start).saturating_add(adp_random_delay_ns()));
            last_advert = now_mach();
        }
        // Periodic per-stream counter notifications to whoever registered.
        // The 16R registers and then expects to be TOLD; the reference peer
        // it accepts pushes these every 2.5 s per stream.
        if ns_since(start) >= next_unsol_ns {
            next_unsol_ns = ns_since(start).saturating_add(UNSOL_COUNTERS_PERIOD_NS);
            unsol_sent += push_unsolicited_counters(dext, our_mac, &entity);
        }
        if pending_advert_at.is_some_and(|due| ns_since(start) >= due) {
            pending_advert_at = None;
            let adp = entity.advertise();
            // Only once armed. Before the servo locks, `set_gptp` has not run
            // and `build_adp` falls back to our OWN clockIdentity as the
            // grandmaster, so an early dump shows gm_id == our entity id and
            // looks like a bug that is not there. Same trap as the inbound
            // AAF dump.
            if !our_adp_dumped && armed {
                our_adp_dumped = true;
                log.log(Level::Info, "wire", &dump_adp("ADP ours ", &adp));
            }
            send_adp(dext, our_mac, &adp);
            // Remember the grandmaster we just advertised: Table 5.55 makes
            // GM_CHANGE a trigger in its own right, because a controller uses
            // our advertised GM to decide whether we share its gPTP domain.
            last_advertised_gm = Some(adp.gptp_grandmaster_id);
        }
        // GM_CHANGE (Table 5.55): re-advertise rather than wait out the 5 s
        // timer, so a controller is not deciding domain compatibility from a
        // grandmaster we have already left.
        //
        // Reads the grandmaster through a NON-MUTATING accessor. Using
        // `entity.advertise()` here, as this first did, bumped
        // `available_index` on every pass of a loop that runs at PDU rate:
        // 1722.1 requires that counter to advance by exactly one per
        // ENTITY_AVAILABLE sent, so each ADPDU carried a jump of tens of
        // thousands and every controller had to read it as "this entity just
        // restarted" and throw away its model of us.
        if armed {
            let gm = entity.advertised_grandmaster_id();
            if last_advertised_gm.is_some_and(|prev| prev != gm) {
                log.log(
                    Level::Info,
                    "adp",
                    "gPTP grandmaster changed — re-advertising (Milan Table 5.55 GM_CHANGE)",
                );
                last_advertised_gm = Some(gm);
                pending_advert_at = Some(ns_since(start).saturating_add(adp_random_delay_ns()));
            }
        }

        // Connect each sink until the 16R says it IS connected, then stop.
        //
        // This used to re-send CONNECT_RX for all eight every second forever.
        // ACMP is connection MANAGEMENT, not a keepalive: a binding persists
        // until something unbinds it, so a blanket re-send is eight
        // state-changing commands a second that the device must process and
        // report onward to its own controller -- a plausible cause of banks
        // flickering in and out of UC Surface. A reviewer had asked for the
        // re-send because the previous code only bumped a counter and never
        // sent at all (review L1); the answer to that is to send while there
        // is evidence it is needed, which GET_STREAM_INFO now provides.
        // Bind each sink ONCE. Milan v1.3 §5.5.1.4 (Auto Connect): "Once a
        // controller has issued a binding request to a Listener and received
        // an acknowledgement, it has no further involvement in the connection
        // process until an unbinding is desired." §5.5.2.4 adds that the
        // Listener stores the binding in NON-VOLATILE memory and "If at any
        // future time the stream stops flowing or is otherwise disrupted, the
        // Listener again invokes Auto Connect to attempt to reestablish the
        // stream" -- across power cycles of talker, listener or any switch on
        // the path.
        //
        // So re-establishment is the LISTENER's duty, not ours. This used to
        // re-send every second forever; narrowing it to "until the device
        // reports connected, then watch and re-bind on unbind" was still more
        // involvement than the protocol has: an unbind is a controller's
        // deliberate act, so re-binding over it fights whichever controller
        // did it -- UC Surface, on this network. Now once a sink is bound we
        // never touch it again for the life of the run.
        //
        // Our ACMP CONNECT_RX_COMMAND *is* the Milan bind: §5.5.2.2 renames
        // CONNECT_RX_COMMAND/RESPONSE to BIND_RX_COMMAND/RESPONSE, same PDU.
        //
        // Gated on `--bind-sinks`, OFF by default, because a Milan Talker is
        // not supposed to do this at all. §5.5.2.7: "Talkers rely only on SRP
        // (not ACMP) to determine whether any Listeners are interested in
        // their Stream Outputs". §5.5.1.2: "This specification does not define
        // the concept of a binding from the perspective of a talker" -- it
        // belongs to the listener's sink and a CONTROLLER writes it.
        //
        // The 16R already has a controller (UC Surface), so binding its sinks
        // ourselves made us a second controller writing that device's
        // persistent routing state behind the first one's back. Observed: the
        // 16R reports sinks 2-8 connected with saved assignments and ZERO
        // unbind events, while UC Surface shows two of those banks empty, and
        // which banks disagree changes between views.
        if bind_sinks && ns_since(last_connect) >= 1_000_000_000 {
            // Over EVERY sink we bound, not just the audio pairing: the
            // media-clock sink is not in `out_streams`, so a loop over that
            // list alone would never retry the one bind the device had not
            // already saved from a previous run.
            for (&sink_uid, &sid) in &expected_stream {
                if sink_bind_done.contains(&sink_uid) {
                    continue;
                }
                // Skip only on positive evidence. An unasked sink (absent) and
                // one reported unbound both still need binding, so a probe
                // that never answers cannot leave a sink unbound forever.
                if sink_connected.get(&sink_uid) == Some(&true) {
                    // Bound -- by us or by the device's own saved state. Done
                    // with it, permanently.
                    sink_bind_done.insert(sink_uid);
                    continue;
                }
                // Back off per sink, but ONLY against a sink the device has
                // actually reported unbound. The first GET_STREAM_INFO answer
                // lands seconds after arming, so counting attempts before any
                // evidence exists made all eight sinks trip the complaint in
                // the opening seconds -- including the seven that were about
                // to confirm as connected. No evidence is not a refusal.
                //
                // Once the device does say unbound (its STREAM_INPUT 1, which
                // has no saved assignment), asking once a second forever is
                // the same misuse of ACMP as the blanket re-send, just
                // narrower. Doubling from 1 s to a 30 s ceiling lets a
                // transient failure recover promptly while a permanent one
                // settles into a slow poll.
                if sink_connected.get(&sink_uid) == Some(&false) {
                    let st = sink_attempts.entry(sink_uid).or_insert((0u32, 0u64));
                    if ns_since(start) < st.1 {
                        continue;
                    }
                    let backoff = SINK_CONNECT_BACKOFF_BASE_NS
                        .saturating_mul(1u64 << st.0.min(5))
                        .min(SINK_CONNECT_BACKOFF_MAX_NS);
                    st.0 = st.0.saturating_add(1);
                    st.1 = ns_since(start).saturating_add(backoff);
                    if st.0 == SINK_CONNECT_COMPLAIN_AT {
                        log.log(
                            Level::Warn,
                            "acmp",
                            &format!(
                                "sink {} still unbound after {} CONNECT_RX attempts \
                                 against its own 'not connected' report — backing \
                                 off to a slow retry",
                                sink_uid, st.0
                            ),
                        );
                    }
                }
                connect_listener(
                    log,
                    dext,
                    our_mac,
                    our_id,
                    listener_id,
                    avb_core::StreamId(sid),
                    sink_uid,
                    connect_seq,
                );
                connect_seq = connect_seq.wrapping_add(1);
            }
            last_connect = now_mach();
        }

        // Push gPTP state into the entity once a second so GET_AVB_INFO and
        // our ADP report the real grandmaster rather than a placeholder.
        // status() does real work, so this is not done per PDU.
        if ns_since(last_gptp_push) >= 1_000_000_000 {
            let st = gptp.status();
            entity.set_gptp(avb_avdecc::entity::GptpStatus {
                grandmaster_id: u64::from_be_bytes(st.gm_identity.0),
                grandmaster: st
                    .gm
                    .as_ref()
                    .map(|gm| avb_avdecc::entity::GrandmasterStatus {
                        identity: u64::from_be_bytes(gm.identity.0),
                        priority1: gm.priority1,
                        priority2: gm.priority2,
                        clock_class: gm.clock_quality.clock_class,
                        clock_accuracy: gm.clock_quality.clock_accuracy,
                        offset_scaled_log_variance: gm.clock_quality.offset_scaled_log_variance,
                        domain_number: 0,
                        time_source: gm.time_source,
                        master_port_identity: u64::from_be_bytes(gm.master_port.clock.0),
                        master_port_number: gm.master_port.port,
                        steps_removed: gm.steps_removed,
                    }),
                domain_number: 0,
                as_capable: st.as_capable,
                propagation_delay_ns: st.path_delay_comp_ns.last.unwrap_or(0.0).max(0.0) as u32,
                // The dataset the gPTP port actually announces, so the
                // AVB_INTERFACE descriptor and the wire cannot disagree.
                priority1: avb_gptp::PRIORITY1,
                priority2: avb_gptp::PRIORITY2,
                clock_class: avb_gptp::our_clock_quality().clock_class,
                clock_accuracy: avb_gptp::our_clock_quality().clock_accuracy,
                offset_scaled_log_variance: avb_gptp::our_clock_quality()
                    .offset_scaled_log_variance,
                // We claim the capability only because the master transmit
                // path and the time-source servo exist. Both are in this
                // build; the gate for performing the role at runtime is
                // asCapable, which the port enforces itself.
                grandmaster_capable: true,
            });
            if let Some(gm) = st.gm.as_ref() {
                let gm_id = u64::from_be_bytes(gm.identity.0);
                if last_unsolicited_ptp_gm != Some(gm_id) {
                    let mut sent = 0u64;
                    for (dst, resp) in entity.unsolicited_desc(
                        avb_avdecc::constants::aem_cmd::GET_PTP_INSTANCE_GRANDMASTER_INFO,
                        avb_avdecc::constants::desc::PTP_INSTANCE,
                        0,
                    ) {
                        send_aem_response(dext, our_mac, dst, &resp);
                        sent += 1;
                    }
                    if sent > 0 {
                        unsol_sent += sent;
                        last_unsolicited_ptp_gm = Some(gm_id);
                        log.log(
                            Level::Info,
                            "aem",
                            &format!("pushed unsolicited PTP grandmaster info for 0x{gm_id:016x}"),
                        );
                    }
                }
            }
            // Real GET_COUNTERS data. Every value here is COUNTED somewhere
            // (the dext's hardware tallies, the gPTP port, the receive path or
            // the TX attribution above) — nothing is derived or invented, and
            // a counter we do not maintain is left out of `counters_valid`
            // rather than reported as zero.
            let dc = dext.device().get_counters().ok();
            // Some(..) only for counters we genuinely maintain. A None is NOT
            // a zero: it keeps the counter out of counters_valid, so a
            // controller can tell "has not happened" from "not measured".
            // Reporting an untracked MEDIA_LOCKED or SEQ_NUM_MISMATCH as a
            // valid 0 would be a positive claim that the stream has never
            // glitched, made without having looked.
            let ctr = |i: usize| {
                dc.as_ref()
                    .map(|c| u32::try_from(c.get(i)).unwrap_or(u32::MAX))
            };
            let per_slot = lrx.per_slot_frames();
            let unsupported = u32::try_from(lrx.format_mismatches()).unwrap_or(u32::MAX);
            entity.set_counters(avb_avdecc::counters::EntityCounters {
                link_up: ctr(avb_dext::abi::ctr::LINK_UP_EVENTS),
                link_down: ctr(avb_dext::abi::ctr::LINK_DOWN_EVENTS),
                gptp_gm_changed: Some(
                    u32::try_from(gptp.counters().gm_changes).unwrap_or(u32::MAX),
                ),
                frames_tx: ctr(avb_dext::abi::ctr::TX_FRAMES),
                frames_rx: ctr(avb_dext::abi::ctr::RX_FRAMES),
                rx_crc_error: ctr(avb_dext::abi::ctr::HW_CRCERRS),
                clock_locked: Some(clock_domain_locked),
                clock_unlocked: Some(clock_domain_unlocked),
                inputs: (0..inbound_stream_ids.len())
                    .map(|i| avb_avdecc::counters::StreamInputCounters {
                        frames_rx: Some(
                            u32::try_from(per_slot.get(i).copied().unwrap_or(0))
                                .unwrap_or(u32::MAX),
                        ),
                        unsupported_format: Some(unsupported),
                        // The remaining Table 5.13 counters are MANDATORY and
                        // we do not yet measure them, so they are left None.
                        // That is visible non-compliance rather than a false
                        // zero, and it is the honest state until the receive
                        // path tracks media lock, interruption, sequence
                        // mismatch and timestamp earliness per stream.
                        ..Default::default()
                    })
                    .collect(),
                outputs: (0..out_streams.len())
                    .map(|i| avb_avdecc::counters::StreamOutputCounters {
                        stream_start: Some(
                            u32::try_from(stream_starts.get(i).copied().unwrap_or(0))
                                .unwrap_or(u32::MAX),
                        ),
                        stream_stop: Some(
                            u32::try_from(stream_stops.get(i).copied().unwrap_or(0))
                                .unwrap_or(u32::MAX),
                        ),
                        frames_tx: Some(
                            u32::try_from(frames_tx_per_stream.get(i).copied().unwrap_or(0))
                                .unwrap_or(u32::MAX),
                        ),
                        // MEDIA_RESET and TIMESTAMP_UNCERTAIN are not tracked.
                        ..Default::default()
                    })
                    .collect(),
            });
            last_gptp_push = now_mach();
        }

        // ---- Inbound audio (duplex) -----------------------------------------
        //
        // Deliberately BEFORE the TX readiness gate below. That gate waits for
        // OUR servo to lock before transmitting, and `continue`s while it
        // waits — but the inbound direction does not depend on our clock at
        // all: the 16R stamps its own PDUs and we recover its media clock from
        // them. Draining here means audio flows IN during the lock wait
        // instead of being blocked by an unrelated condition.
        if duplex {
            // Ask the 16R to connect EVERY one of its audio STREAM_OUTPUTs to
            // us, re-asserted on the same ~1 s cadence Apple's controller uses
            // so the Milan talker holds the connections and keeps advertising.
            //
            // One connect per source. The 16R only advertises a source a
            // controller has connected, so connecting one of eight is why a
            // run saw a single Talker Advertise and bound 8 of 64 channels —
            // the Listener Ready and dext-filter code below was already
            // multi-stream and had nothing to work with.
            if last_in_connect.0 == 0 || ns_since(last_in_connect) >= 1_000_000_000 {
                use avb_avdecc::acmp::Acmpdu;
                use avb_avdecc::constants::{acmp_msg, acmp_status};
                for (sink_idx, &uid) in inbound_uids.iter().enumerate() {
                    let pdu = Acmpdu {
                        message_type: acmp_msg::CONNECT_TX_COMMAND,
                        status: acmp_status::SUCCESS,
                        stream_id: avb_core::StreamId(0),
                        controller_entity_id: our_id,
                        talker_entity_id: listener_id,
                        listener_entity_id: our_id,
                        talker_unique_id: uid,
                        // OUR STREAM_INPUT index, which is the position in
                        // `inbound_uids`, NOT the 16R's STREAM_OUTPUT index.
                        //
                        // This used to echo `uid` back. The two are different
                        // numbering spaces and they do not line up: the 16R's
                        // audio sources start at descriptor index 1 because
                        // index 0 is its `Send CRF`, so echoing its index
                        // named a sink of ours that does not exist. It was
                        // unfalsifiable while we advertised no STREAM_INPUTs
                        // at all; now that the sinks are real, a listener id
                        // past `sink_count` is a connection to nowhere.
                        listener_unique_id: u16::try_from(sink_idx).unwrap_or(0),
                        sequence_id: in_connect_seq,
                        ..Default::default()
                    };
                    in_connect_seq = in_connect_seq.wrapping_add(1);
                    let hdr = EthHeader {
                        dst: avb_core::AVDECC_MULTICAST,
                        src: our_mac,
                        vlan: None,
                        ethertype: avb_avtp::common::ETHERTYPE_AVTP,
                    };
                    if let Ok(f) = TxFrame::build(&hdr, &pdu.encode(), false, 0) {
                        let _ = dext.send(f.bytes(), false, 0);
                    }
                }
                last_in_connect = now_mach();
            }
            for slot in dext.rx_audio() {
                in_slots += 1;
                // The audio ring carries whole Ethernet frames; AAF decode
                // wants the AVTP payload, so strip the Ethernet/VLAN header.
                let Ok(p) = avb_core::eth::EthFrame::parse(&slot.data) else {
                    in_decode_fail += 1;
                    continue;
                };
                match avb_avtp::aaf::decode(p.payload) {
                    Ok(pdu) => {
                        // Only once the talker is armed, which means the servo
                        // has LOCKED and its initial step is behind us.
                        //
                        // Dumping the FIRST inbound PDU instead reads a
                        // pre-step clock: the first one arrives during
                        // discovery, and the servo's opening step can be
                        // hundreds of ms. The offset printed then is the step
                        // we had not yet applied, not the 16R's presentation
                        // offset — it measured -475 ms once and that number
                        // was an artefact of this very bug. Both dumps must be
                        // read off the same locked clock to be comparable at
                        // all.
                        if !rx_pdu_dumped && armed {
                            rx_pdu_dumped = true;
                            log.log(
                                Level::Info,
                                "wire",
                                &dump_aaf_pdu("RX 16R's", p.payload, gptp.now_ns(now_mach())),
                            );
                        }
                        lrx.on_pdu(log, &pdu, elapsed);
                    }
                    Err(_) => in_decode_fail += 1,
                }
            }
        }

        // ---- Readiness gate -------------------------------------------------
        //
        // A Milan talker must not stream on a clock that is not locked to the
        // grandmaster: the presentation timestamps would be wrong by the
        // clock's whole offset, and the listener discards them. So wait for
        // the servo, bounded by the discovery timeout, instead of emitting
        // 8000 badly-stamped PDUs per second.
        // Five seconds of streaming is many thousands of packets: long enough
        // for a listener to lock, short enough to land inside a 25 s run.
        // Then twice more, so a counter that moves can be distinguished from
        // one that is genuinely stuck.
        //
        // After those diagnostic rounds this does NOT stop: it keeps asking
        // GET_STREAM_INFO on a slower cadence, because the connect loop above
        // now skips a sink the device reported connected, and a stale answer
        // would leave a sink that later unbound unconnected forever. The watch
        // rounds drop GET_COUNTERS (three samples already settled whether a
        // counter moves) and log per-sink lines only on a CHANGE, so a
        // long-running daemon does not fill its log with a steady state.
        //
        // The watch is spaced from the PREVIOUS round, not from `armed_at`
        // times a round count: the latter multiplies the watch period by an
        // ever-growing round number, which put the first watch round 60 s
        // after arming and left a 45 s hole behind the last diagnostic one.
        if armed
            && armed_at_ns.is_some_and(|a| {
                if sink_probes_done < SINK_PROBE_COUNT {
                    ns_since(start) > a + SINK_PROBE_PERIOD_NS * u64::from(sink_probes_done + 1)
                } else {
                    ns_since(start) >= last_sink_round_ns + SINK_WATCH_PERIOD_NS
                }
            })
        {
            let round = sink_probes_done + 1;
            let diagnostic_round = sink_probes_done < SINK_PROBE_COUNT;
            sink_watch_verbose = diagnostic_round;
            sink_probes_done += 1;
            last_sink_round_ns = ns_since(start);
            for &sink_uid in &probe_sinks {
                let mut pl = Vec::with_capacity(4);
                pl.extend_from_slice(&avb_avdecc::constants::desc::STREAM_INPUT.to_be_bytes());
                pl.extend_from_slice(&sink_uid.to_be_bytes());
                let hdr = EthHeader {
                    dst: peers.target_mac,
                    src: our_mac,
                    vlan: None,
                    ethertype: avb_avtp::common::ETHERTYPE_AVTP,
                };
                // Both reads carry the same payload shape
                // (descriptor_type, descriptor_index), so build one and ask
                // twice. GET_STREAM_INFO gives the 16R's OWN view of the
                // binding — the stream id, destination MAC, VLAN and MSRP
                // failure code it is actually waiting on — which is directly
                // comparable against what we transmit. GET_COUNTERS says
                // whether it locked.
                for ct in if diagnostic_round {
                    [
                        Some(avb_avdecc::constants::aem_cmd::GET_COUNTERS),
                        Some(avb_avdecc::constants::aem_cmd::GET_STREAM_INFO),
                    ]
                } else {
                    [None, Some(avb_avdecc::constants::aem_cmd::GET_STREAM_INFO)]
                }
                .into_iter()
                .flatten()
                {
                    let q = avb_avdecc::aecp::AemPdu {
                        message_type: avb_avdecc::constants::aecp_msg::AEM_COMMAND,
                        status: avb_avdecc::constants::aecp_status::SUCCESS,
                        target_entity_id: peers.target,
                        controller_entity_id: our_id,
                        sequence_id: 0xC000 | sink_uid,
                        command_type: ct,
                        unsolicited: false,
                        payload: pl.clone(),
                    };
                    if let Ok(frame) = TxFrame::build(&hdr, &q.encode(), false, 0) {
                        let _ = dext.send(frame.bytes(), false, 0);
                    }
                }
            }
            log.log(
                Level::Info,
                "aem",
                &format!(
                    "asked the 16R for {} on {} STREAM_INPUT sink(s) ({})",
                    if diagnostic_round {
                        "GET_COUNTERS + GET_STREAM_INFO"
                    } else {
                        "GET_STREAM_INFO"
                    },
                    probe_sinks.len(),
                    if diagnostic_round {
                        format!("diagnostic sample {round} of {SINK_PROBE_COUNT}")
                    } else {
                        // Was "sample 5 of 3": the round counter kept climbing
                        // past the diagnostic count once the watch took over,
                        // which reads like a broken loop rather than a
                        // deliberate steady-state poll.
                        format!(
                            "connection watch, every {}s",
                            SINK_WATCH_PERIOD_NS / 1_000_000_000
                        )
                    }
                ),
            );
        }
        // ---- MAAP: drive probe/announce, and gate streaming on it ----------
        //
        // Milan §6.3.1 condition 1: a Talker attribute's destination address is
        // only VALID once "allocated by the MAAP protocol and no conflict is
        // being reported". So the block must be claimed before we transmit to
        // it, not merely chosen — which is what we did before this existed.
        if let Some(avb_avtp::maap::Action::Send(p)) = maap.tick(ns_since(start)) {
            let hdr = EthHeader {
                dst: avb_avtp::maap::MAAP_MULTICAST,
                src: our_mac,
                vlan: None,
                ethertype: avb_avtp::common::ETHERTYPE_AVTP,
            };
            if let Ok(f) = TxFrame::build(&hdr, &p.encode(), false, 0) {
                let _ = dext.send(f.bytes(), false, 0);
            }
            maap_tx = maap_tx.saturating_add(1);
            if !maap_acquired_logged && maap.acquired() {
                maap_acquired_logged = true;
                let (base, n) = maap.block();
                log.log(
                    Level::Info,
                    "maap",
                    &format!(
                        "allocated {n} stream address(es) from {:02x?} after {} probe(s), \
                         {} conflict(s) — our Talker attributes are now valid (Milan §6.3.1)",
                        base.octets(),
                        maap_tx.saturating_sub(1),
                        maap.conflicts
                    ),
                );
            }
        }
        // ---- clock role: who is grandmaster, and how BTCA decided ---------
        //
        // Reported periodically because it is the only visible evidence of the
        // grandmaster path doing anything: with PRIORITY1 248 every merit rung
        // ties against the 16R and the contest is settled on clockIdentity, so
        // "we lost" is the EXPECTED reading here, not a fault. A climbing
        // `won` on this link, or any master-side TX, would mean we had taken
        // the clock from the device we stream to.
        if ns_since(start).saturating_sub(last_role_log_ns) >= ROLE_LOG_PERIOD_NS {
            last_role_log_ns = ns_since(start);
            let st = gptp.status();
            let c = gptp.counters();
            log.log(
                Level::Info,
                "clock",
                &format!(
                    "role={} servo={} gm={} (ours={}) btca(won={} lost={}) mastertx(ann={} sync={} fu={} ts_lost={}){}",
                    st.port_state.name(),
                    st.servo_state.name(),
                    st.gm_identity,
                    st.port_identity.clock,
                    c.btca_we_won,
                    c.btca_we_lost,
                    c.announce_tx,
                    c.sync_tx,
                    c.follow_up_tx,
                    c.sync_tx_ts_lost,
                    if crf_talker.is_some() {
                        format!(
                            " crf(pdus={crf_pdus} refused={crf_refused} no_anchor={crf_skipped_no_clock} reanchors={crf_reanchors})"
                        )
                    } else {
                        String::new()
                    },
                ),
            );
        }
        if !armed {
            // A conformant talker does not transmit to an unallocated address.
            if !maap.acquired() {
                // fall through to the rest of the loop; MAAP is still probing
            } else if gptp.is_locked() {
                armed = true;
                armed_at_ns = Some(ns_since(start));
                // STREAM_START, Table 5.14: the stream begins transmitting
                // here, when the clock became fit to timestamp with. Counted
                // per STREAM_OUTPUT because that is the descriptor a
                // controller asks about.
                for c in stream_starts.iter_mut() {
                    *c = c.saturating_add(1);
                }
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

        // ---- Clock source: resolve and publish ------------------------------
        //
        // Once a second, not per PDU: the inputs change at human speed (the
        // user picks a source, a lock comes or goes) and this re-reads a file.
        // The daemon is the authority — it publishes BOTH the basis and the
        // verdict onto the output ring, and the plug-in follows that rather
        // than deciding for itself.
        if ns_since(last_clock_pub) >= crate::clocksrc::CONFIG_RELOAD_NS {
            if let Some(newsel) = clocksel.reload() {
                log.log(
                    Level::Info,
                    "clock",
                    &format!("clock source changed to {} ({newsel})", newsel.label()),
                );
            }
            for w in clocksel.take_warnings() {
                log.log(Level::Warn, "clock", &w);
            }
            let st = gptp.status();
            // Where the recovered 16R media clock comes from.
            //
            // In DUPLEX our own inbound path owns the input ring, and it
            // creates that ring when the first AAF packet arrives — which is
            // long after this loop's start-up `ShmRing::attach()` ran. So that
            // attach always failed in duplex, `recovered` was always None, and
            // a session with the 16R's media clock arriving on the wire every
            // 125 us still reported it "unavailable" and ran the device
            // timeline on a gPTP-derived rate instead. Ask the live inbound
            // path first; the start-up attach remains the path for a `play`
            // session running alongside a separate `record`.
            let recovered = crate::clocksrc::recovered_for(
                lrx.recovered_clock(),
                recovered_ring.as_ref().and_then(|r| r.ring().read_clock()),
            );
            let decision = clocksel.decide(
                recovered,
                st.gptp_ppm_vs_mach,
                st.servo_state.is_usable_clock() && st.as_capable,
            );
            let basis_now = decision.basis.is_some();
            match clock_basis_was {
                None => {
                    // First reading is the initial state, not a transition.
                    if basis_now {
                        clock_domain_locked = clock_domain_locked.saturating_add(1);
                    }
                }
                Some(was) if was != basis_now => {
                    if basis_now {
                        clock_domain_locked = clock_domain_locked.saturating_add(1);
                    } else {
                        clock_domain_unlocked = clock_domain_unlocked.saturating_add(1);
                    }
                }
                Some(_) => {}
            }
            clock_basis_was = Some(basis_now);
            crate::clocksrc::publish(out_ring.ring(), &decision);
            // Report only transitions: a per-second line saying the same thing
            // buries the one that matters.
            if last_effective != Some(decision.effective) {
                let rate = decision
                    .basis
                    .map(|b| format!("{:.3} Hz", b.rate_hz_vs_mach))
                    .unwrap_or_else(|| format!("{hz} Hz nominal"));
                let level = if decision.effective.is_network_disciplined() {
                    Level::Info
                } else {
                    // Not an error — it can be exactly what the user asked
                    // for — but it is the state in which audio drifts against
                    // the 16R, so it must not look routine.
                    Level::Warn
                };
                log.log(
                    Level::Info,
                    "clock",
                    &format!(
                        "sourcing on {} ({}) at {rate}",
                        decision.effective.label(),
                        decision.effective.reason()
                    ),
                );
                if level == Level::Warn {
                    log.log(
                        Level::Warn,
                        "clock",
                        "the sample clock is not disciplined to the network — \
                         audio will drift against the 16R",
                    );
                }
                last_effective = Some(decision.effective);
            }
            last_clock_pub = now_mach();
        }

        // ---- Media clock (CRF) tick ----------------------------------------
        //
        // BEFORE the audio pacing gate below, which `continue`s on a tick that
        // is not yet due: at 125 us audio and 2 ms CRF that gate fires fifteen
        // times out of sixteen, so a CRF tick placed after it would be starved
        // to roughly nothing.
        if let Some(t) = &mut crf_talker {
            if armed && elapsed >= next_crf_ns {
                match gptp.now_ns(now_mach()) {
                    None => {
                        // No valid gPTP anchor: skip, and DROP ours. An event
                        // time derived from an anchor taken before a servo
                        // step is wrong by the step, and a media clock is the
                        // one thing where handing over a wrong time is worse
                        // than handing over nothing.
                        crf_skipped_no_clock += 1;
                        if crf_anchor_ns.take().is_some() {
                            crf_reanchors += 1;
                        }
                        next_crf_ns = next_crf_ns.saturating_add(crf_pdu_interval_ns);
                    }
                    Some(gptp_now) => {
                        // Establish or re-establish the basis. The anchor is
                        // the gPTP time of event 0; every later event time
                        // comes from the index, so the series is exactly on
                        // the nominal media-clock grid.
                        let anchor = match crf_anchor_ns {
                            Some(a) => a,
                            None => {
                                crf_ts_index = 0;
                                crf_anchor_ns = Some(gptp_now);
                                gptp_now
                            }
                        };
                        let ts = t.nth_timestamp(anchor, crf_ts_index);
                        // `tu` says the timestamps are not known good. 802.1AS
                        // asCapable plus a usable servo state is exactly that
                        // condition: without a completed Pdelay exchange the
                        // peer has no link delay for us, so it cannot correct
                        // what we send.
                        let st = gptp.status();
                        t.set_tu(!(st.as_capable && st.servo_state.is_usable_clock()));
                        match t.packetize(&[ts], &mut crf_scratch) {
                            Ok(n) => {
                                let hdr = EthHeader {
                                    dst: stream_dest_mac(crf_stream_id),
                                    src: our_mac,
                                    vlan: Some(VlanTag {
                                        pcp: DEFAULT_DOMAIN.priority,
                                        dei: false,
                                        vid: DEFAULT_DOMAIN.vid,
                                    }),
                                    ethertype: avb_avtp::common::ETHERTYPE_AVTP,
                                };
                                let body = crf_scratch.get(..n).unwrap_or(&[]);
                                if !crf_dumped {
                                    crf_dumped = true;
                                    let hex: String = body
                                        .iter()
                                        .map(|b| format!("{b:02x}"))
                                        .collect::<Vec<_>>()
                                        .join(" ");
                                    log.log(
                                        Level::Info,
                                        "wire",
                                        &format!(
                                            "TX CRF  {hex} | stream 0x{:016x} ts {ts} \
                                             (gptp_now {gptp_now}, ts_index {crf_ts_index}) tu={}",
                                            crf_stream_id.0,
                                            !(st.as_capable && st.servo_state.is_usable_clock()),
                                        ),
                                    );
                                }
                                if let Ok(f) = TxFrame::build(&hdr, body, false, 0) {
                                    // Sent on its own rather than joining the
                                    // audio batch: the audio batch is built
                                    // and submitted only on an audio tick, and
                                    // the two cadences do not line up.
                                    match dext.send(f.bytes(), false, 0) {
                                        SendResult::Ok => {
                                            crf_pdus += 1;
                                        }
                                        other => {
                                            crf_refused += 1;
                                            last_send_err = Some(other);
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                crf_refused += 1;
                                log.log(Level::Error, "crf", &format!("packetize error: {e:?}"));
                            }
                        }
                        // One timestamp per PDU, so the TIMESTAMP index
                        // advances by one. `nth_timestamp` turns it into a
                        // gPTP time by multiplying by the 2 ms between
                        // timestamps, so this must not advance by
                        // `timestamp_interval` — that would stamp the series
                        // 96x too fast.
                        crf_ts_index = crf_ts_index.saturating_add(1);
                        next_crf_ns = next_crf_ns.saturating_add(crf_pdu_interval_ns);
                        if next_crf_ns <= elapsed {
                            next_crf_ns = elapsed.saturating_add(crf_pdu_interval_ns);
                        }
                    }
                }
            } else if armed && next_crf_ns == 0 {
                next_crf_ns = elapsed;
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

        // Eight streams at 8000 frames/s each is 64 000 frames/s. That is
        // unreachable one user-client call at a time -- a SendFrame round
        // trip measures ~28 us, so the loop managed 35 380 frames/s with the
        // token bucket granting every one of them (0 rate-limited) and 30% of
        // intervals missed. So the interval is STAGED here and handed to the
        // dext in a single `send_batch` call below.
        let mut tx_staged_len = 0usize;
        for i in 0..out_streams.len() {
            let (uid, dst) = match out_streams.get(i) {
                Some(o) => (o.uid, stream_dest_mac(o.stream_id)),
                None => break,
            };
            // Gather this stream's channels out of the 64-channel ring frame.
            // Allocation-free: both buffers are sized once above.
            let first_ch = usize::from(uid) * channels;
            for f in 0..spf {
                let src = f * frame_bytes + first_ch * 4;
                let dst_off = f * stream_frame_bytes;
                if let (Some(s_in), Some(s_out)) = (
                    sample_scratch.get(src..src + stream_frame_bytes),
                    stream_scratch.get_mut(dst_off..dst_off + stream_frame_bytes),
                ) {
                    s_out.copy_from_slice(s_in);
                }
            }

            // Outbound peak |sample|, so "transmitting audio" is
            // distinguishable from "transmitting correctly-formed silence".
            // Without it a complete protocol success (listener_ready, zero
            // underruns, 8000 PDUs/s) reads as healthy while every sample is
            // zero — the shape of the dead WriteMix path that survived five
            // reviews. Sampled every 8th interval to keep the 50 us pacing
            // loop clean; a peak does not need every frame.
            if tx_peak_tick.is_multiple_of(8) {
                if let Some(bytes) = stream_scratch.get(..got.min(spf) * stream_frame_bytes) {
                    for sb in bytes.as_chunks::<4>().0 {
                        let a = i64::from(i32::from_ne_bytes(*sb)).abs();
                        if a > tx_peak {
                            tx_peak = a;
                        }
                    }
                }
            }

            let packed = match out_streams.get_mut(i) {
                Some(o) => o
                    .talker
                    .packetize(&stream_scratch, avtp_ts, true, &mut pdu_scratch),
                None => break,
            };
            match packed {
                Ok(n) => {
                    if !tx_pdu_dumped {
                        tx_pdu_dumped = true;
                        log.log(
                            Level::Info,
                            "wire",
                            &dump_aaf_pdu(
                                "TX ours ",
                                pdu_scratch.get(..n).unwrap_or(&[]),
                                Some(gptp_now),
                            ),
                        );
                    }
                    let hdr = EthHeader {
                        dst,
                        src: our_mac,
                        vlan: Some(VlanTag {
                            // From the SR class A domain we advertised in the
                            // TSpec, not a literal: the reservation and the
                            // frames it covers must carry the same PCP and
                            // VID.
                            pcp: DEFAULT_DOMAIN.priority,
                            dei: false,
                            vid: DEFAULT_DOMAIN.vid,
                        }),
                        ethertype: avb_avtp::common::ETHERTYPE_AVTP,
                    };
                    if let Ok(f) =
                        TxFrame::build(&hdr, pdu_scratch.get(..n).unwrap_or(&[]), false, 0)
                    {
                        if let Some(slot) = tx_staged.get_mut(tx_staged_len) {
                            *slot = f;
                            if let Some(o) = tx_staged_owner.get_mut(tx_staged_len) {
                                *o = i;
                            }
                            tx_staged_len += 1;
                        }
                    }
                }
                Err(e) => {
                    log.log(
                        Level::Error,
                        "play",
                        &format!("packetize error on stream {uid}: {e:?}"),
                    );
                }
            }
        }

        // One call for the whole interval.
        //
        // Count what the dext ACCEPTED, not what we attempted. The dext
        // enforces its own TX ceiling (SAF-6) and queues in order until a
        // frame is refused, so a short return is the prefix that reached the
        // wire. Ignoring that once made the summary report 441 232 PDUs
        // "sent" in a run where only 58 820 arrived, which hid the real
        // fault.
        if tx_staged_len > 0 {
            let mut batch: [&[u8]; avb_dext::abi::tx_batch::MAX] =
                [&[]; avb_dext::abi::tx_batch::MAX];
            let n = tx_staged_len.min(batch.len());
            for (slot, f) in batch.iter_mut().zip(tx_staged.iter()).take(n) {
                *slot = f.bytes();
            }
            let r = dext.send_batch(batch.get(..n).unwrap_or(&[]));
            tx_pdus += r.accepted as u64;
            // The dext queues in order until a frame is refused, so the
            // accepted PREFIX is what reached the wire — credit exactly those
            // streams.
            for k in 0..r.accepted.min(n) {
                if let Some(&owner) = tx_staged_owner.get(k) {
                    if let Some(c) = frames_tx_per_stream.get_mut(owner) {
                        *c += 1;
                    }
                }
            }
            let refused = n.saturating_sub(r.accepted);
            if refused > 0 {
                match r.stopped_by {
                    // Back-pressure: the dext's ceiling or a full TX ring.
                    // Both mean the frames never reached the wire, and the
                    // dext's own counters split the two.
                    None | Some(SendResult::NoResources) | Some(SendResult::Busy) => {
                        tx_rate_limited += refused as u64;
                    }
                    Some(other) => {
                        tx_rejected += refused as u64;
                        last_send_err = Some(other);
                    }
                }
            }
        }
        tx_peak_tick = tx_peak_tick.wrapping_add(1);

        // Schedule the next interval, and NEVER resynchronise to "now".
        //
        // This used to set `next_pdu_ns = elapsed` when it fell behind. With
        // one stream the per-tick work fitted inside 125 us and that never
        // fired. With eight it does not always fit, and resyncing to now made
        // the gate `elapsed < next_pdu_ns` immediately false again — so the
        // loop sent group after group with no pacing at all, drained the
        // dext's 64-token burst and had 415 140 of 935 952 frames denied while
        // the wire still only saw 14 880/s. A pacing loop that falls behind
        // must SLOW DOWN, not spin.
        //
        // Falling behind is now counted and the schedule placed on the next
        // boundary in the FUTURE, which guarantees a sleep before the next
        // group.
        next_pdu_ns = next_pdu_ns.saturating_add(pdu_interval_ns);
        if next_pdu_ns <= elapsed {
            missed_intervals += 1;
            next_pdu_ns = elapsed.saturating_add(pdu_interval_ns);
        }
    }

    // Tell the 16R we are going before we stop answering. Without this it
    // holds us in its entity table for the full 2 * valid_time (20 s) and a
    // restarted daemon can collide with its own stale entry.
    let departing = entity.departing();
    send_adp(dext, our_mac, &departing);

    // What the 16R actually asked us, one line per command shape. Read this
    // to answer "did it enumerate us, and how far did it get" — a question
    // the bare answered/refused totals cannot settle.
    if aecp_asked.is_empty() {
        log.log(
            Level::Warn,
            "aem",
            "the 16R asked us NOTHING over AECP this run — it never enumerated us",
        );
    } else {
        let mut parts: Vec<String> = Vec::new();
        for (label, (asked, refused)) in &aecp_asked {
            if *refused > 0 {
                parts.push(format!("{label} x{asked} ({refused} refused)"));
            } else {
                parts.push(format!("{label} x{asked}"));
            }
        }
        log.log(
            Level::Info,
            "aem",
            &format!(
                "the 16R asked us for: {} — {} distinct command shape(s); \
                 unsolicited notifications pushed: {unsol_sent}",
                parts.join(", "),
                aecp_asked.len()
            ),
        );
    }

    let final_gptp = gptp.status();
    // `run_ns` is the requested budget, not elapsed time: SIGTERM or a device
    // fault can end a run early. Reporting it as elapsed made an early-stopped
    // healthy run look as though its wire rate had collapsed.
    let actual_elapsed_s = ns_since(start) as f64 / 1e9;
    // Named rather than positional, so adding a field cannot silently shift
    // every later argument in a format string this long.
    let maap_acq = maap.acquired();
    let maap_conf = maap.conflicts;
    log.log(
        Level::Info,
        dir,
        &format!(
            "talker done: {tx_pdus} PDUs on the wire, {tx_rate_limited} rate-limited, {tx_rejected} otherwise refused, {underruns} underruns, {missed_intervals} intervals missed over {actual_elapsed_s:.1}s; outbound peak {tx_peak} ({}); 16R listener_ready={listener_ready}; AEM commands answered={aecp_answered} refused={aecp_refused} (plus {aecp_probes_past_end} correct end-of-list probes); Milan MVU answered={mvu_answered}; ACMP talker commands answered={acmp_talker_answered} listener commands answered={acmp_listener_answered}; MAAP seen={maap_seen} sent={maap_tx} acquired={maap_acq} conflicts={maap_conf}; sinks bound={}/{} unbind events={sink_reconnects}; ADP discovers answered={adp_discovers_answered} available_index={}; gPTP servo={} offset {:.0} ns, {ts_unavailable} ticks with no clock anchor, {ticks_unlocked} unlocked",
            peak_label(tx_peak),
            sink_connected.values().filter(|v| **v).count(),
            sink_connected.len(),
            entity.available_index(),
            final_gptp.servo_state.name(),
            final_gptp.offset_ns.last.unwrap_or(f64::NAN),
        ),
    );
    // The media clock gets its OWN line rather than being folded into the
    // audio totals: at 500 PDU/s against 64 000 it would round to nothing in a
    // combined count, and "the media clock stream stopped" is exactly the
    // failure this run exists to detect.
    if crf_talker.is_some() {
        let window_s = armed_at_ns
            .map(|a| (run_ns.saturating_sub(a) as f64) / 1e9)
            .filter(|s| *s > 0.0);
        log.log(
            Level::Info,
            "crf",
            &format!(
                "media clock done: {crf_pdus} CRF PDUs on the wire{}, {crf_refused} refused, \
                 {crf_skipped_no_clock} ticks skipped with no gPTP anchor, {crf_reanchors} \
                 timestamp-basis re-anchors, {crf_ts_index} timestamps covering {} \
                 media-clock events (stream 0x{:016x} -> 16R STREAM_INPUT {CRF_SINK_UID})",
                match window_s {
                    Some(s) => format!(" ({:.0}/s)", crf_pdus as f64 / s),
                    None => String::new(),
                },
                crf_ts_index.saturating_mul(u64::from(avb_avtp::crf::milan::TIMESTAMP_INTERVAL)),
                crf_stream_id.0,
            ),
        );
        if crf_pdus == 0 {
            log.log(
                Level::Error,
                "crf",
                "the media clock stream transmitted NOTHING — the 16R cannot have \
                 recovered a clock from it, so a blank Network Clock status says \
                 nothing about whether CRF would work",
            );
        } else if crf_reanchors > 0 {
            log.log(
                Level::Warn,
                "crf",
                &format!(
                    "the timestamp basis was re-established {crf_reanchors} time(s) mid-run; \
                     each one is a discontinuity in the series the 16R is recovering a clock \
                     from"
                ),
            );
        }
    }
    if duplex {
        // Report the inbound direction SEPARATELY and in terms of frames
        // actually placed. A direction that ran but moved nothing is the
        // failure mode that hid the dead output path for weeks — the summary
        // has to make "ran and moved audio" distinguishable from "ran".
        log.log(
            Level::Info,
            "duplex",
            &format!(
                "listener done: {in_slots} audio slots, {in_decode_fail} not decodable as AAF, {} frames placed into the input ring, {} format mismatches; Listener Ready declared for {} stream(s), dext filter entries {}",
                lrx.frames_placed(),
                lrx.format_mismatches(),
                in_declared.len(),
                in_filter.len(),
            ),
        );
        report_discarded_streams(log, "duplex", &lrx);
        // Per-slot frames and alignment health. A slot that bound and
        // delivered nothing is 8 silent channels in the middle of the device,
        // which the total frames_placed hides completely.
        let per = lrx.per_slot_frames();
        if per.len() > 1 {
            let silent: Vec<usize> = per
                .iter()
                .enumerate()
                .filter(|(_, &f)| f == 0)
                .map(|(i, _)| i)
                .collect();
            log.log(
                Level::Info,
                "duplex",
                &format!(
                    "inbound slots: {} bound, frames per slot {per:?}",
                    per.len()
                ),
            );
            if !silent.is_empty() {
                log.log(
                    Level::Warn,
                    "duplex",
                    &format!(
                        "slot(s) {silent:?} delivered NOT ONE frame — that is {} channels of the \
                         device reading as silence rather than as absent",
                        silent.len() * usize::from(lrx.channels_per_slot())
                    ),
                );
            }
            // Cross-stream health, MEASURED. A correctly-stepped stream set
            // stays within one 125 us observation interval of the shared
            // timeline basis; a large value means the device's streams are not
            // in step or one of them is gapping, and the ring's own late-drop
            // and placement-reject counters say what became of those packets.
            log.log(
                Level::Info,
                "duplex",
                &format!(
                    "cross-stream offsets: largest observed {} frame(s) ({:.2} ms); placed on \
                     assumption: {} with no timestamp, {} with an implausible forward jump",
                    lrx.max_offset_frames(),
                    lrx.max_offset_frames() as f64 * 1000.0 / 48_000.0,
                    lrx.assumed().0,
                    lrx.assumed().1,
                ),
            );
            if lrx.assumed().1 > 0 {
                log.log(
                    Level::Warn,
                    "duplex",
                    &format!(
                        "implausible forward jumps per slot: {:?} — a count concentrated in one \
                         slot means that stream is not timestamped on the same base as the rest",
                        lrx.suspect_per_slot()
                    ),
                );
            }
            // What the ring itself says became of every packet. A large
            // offset only matters if it cost frames.
            if let Some((late, rejects, zero_fill, gaps, overrun)) = lrx.ring_counters() {
                log.log(
                    Level::Info,
                    "duplex",
                    &format!(
                        "ring placement: {late} late drop(s), {rejects} placement reject(s), \
                         {zero_fill} zero-filled frame(s), {gaps} gap frame(s), {overrun} \
                         overrun frame(s)"
                    ),
                );
            }
        }
        if in_declared.is_empty() {
            log.log(
                Level::Warn,
                "duplex",
                "the 16R never advertised a talker stream to us, so we never \
                 declared Listener Ready and no audio could flow IN — check \
                 that its STREAM_OUTPUT is connected to this entity",
            );
        } else if lrx.frames_placed() == 0 {
            log.log(
                Level::Warn,
                "duplex",
                "Listener Ready was declared but NOT ONE audio frame reached \
                 the input ring — the reservation was made and the 16R still \
                 did not stream, or the dext filter did not match its frames",
            );
        }
    }
    if aecp_answered == 0 {
        log.log(
            Level::Warn,
            "aem",
            "the 16R never sent us a single AEM command — it did not try to \
             enumerate us, so check that our ADP advertisement is reaching it",
        );
    }
    // What TX ceiling the dext ACTUALLY enforced, measured.
    //
    // The ceiling is compiled into the driver and nothing reports it, so after
    // an upgrade that silently did not take there is no way to tell which one
    // is live — a blind instrument, and this project has been caught by those
    // before. Attempts, grants and the window are all in hand, so derive it
    // rather than reasoning about build timestamps.
    if let Some(armed_ns) = armed_at_ns {
        let window_s = (run_ns.saturating_sub(armed_ns)) as f64 / 1e9;
        let attempts = tx_pdus + tx_rate_limited + tx_rejected;
        if window_s > 1.0 && attempts > 0 {
            log.log(
                Level::Info,
                dir,
                &format!(
                    "TX over the {window_s:.1}s armed window: {:.0} frames/s attempted, {:.0}/s reached the wire via {}{}",
                    attempts as f64 / window_s,
                    tx_pdus as f64 / window_s,
                    // Which path, because the two have very different
                    // ceilings and a v3 dext falls back silently otherwise.
                    // A run that reports the per-frame path CANNOT reach the
                    // full width, so this is the first line to read when the
                    // rate is short.
                    if dext.batched_send() {
                        "one batched call per interval"
                    } else {
                        "one call PER FRAME (dext predates ABI v4; it cannot sustain 8 streams)"
                    },
                    if tx_rate_limited > 0 {
                        format!(
                            " — the dext granted about {:.0} frames/s, which is its compiled-in SAF-6 ceiling whatever the source now says",
                            tx_pdus as f64 / window_s
                        )
                    } else {
                        String::new()
                    }
                ),
            );
        }
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
    // Per-stream acceptance. With eight advertised the 16R can accept some
    // and not others, and `listener_ready` alone would report the direction
    // connected on the strength of the first.
    let ready_count = out_streams.iter().filter(|o| o.ready).count();
    log.log(
        Level::Info,
        dir,
        &format!(
            "outbound streams: {} of {} accepted by the 16R{}",
            ready_count,
            out_streams.len(),
            if ready_count == out_streams.len() {
                String::new()
            } else {
                format!(
                    "; NOT accepted: {:?}",
                    out_streams
                        .iter()
                        .filter(|o| !o.ready)
                        .map(|o| o.uid)
                        .collect::<Vec<_>>()
                )
            }
        ),
    );
    if ready_count > 0 && ready_count < out_streams.len() {
        log.log(
            Level::Warn,
            dir,
            &format!(
                "only {ready_count} of {} outbound streams were accepted, so {} of the device's \
                 {} output channels reached the 16R and the rest went nowhere",
                out_streams.len(),
                ready_count * channels,
                out_streams.len() * channels,
            ),
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
        // Distinguish the two reasons, which need different fixes. A Milan
        // listener only declares Ready after the TALKER answers its
        // CONNECT_TX_COMMAND; if it never sent us one, the problem is upstream
        // of the reservation (it was never told to connect, or it could not
        // finish enumerating us).
        if acmp_talker_answered == 0 {
            log.log(
                Level::Warn,
                "acmp",
                "the 16R never sent us a CONNECT_TX_COMMAND either, so it was never asked to \
                 connect or could not enumerate us — not a reservation problem",
            );
        } else {
            log.log(
                Level::Warn,
                "acmp",
                &format!(
                    "we answered {acmp_talker_answered} ACMP talker command(s), so the 16R did \
                     ask to connect and then still withheld Listener Ready — check the TSpec and \
                     the stream addressing against its own advertise"
                ),
            );
        }
    }
    Ok(())
}

/// The AVB stream destination MAC for our talker stream, from the MAAP dynamic
/// allocation pool (IEEE 1722-2016 Annex B).
///
/// We do not run the MAAP protocol itself yet — no probe, no defend — so the
/// block is derived deterministically from our own MAC by
/// [`avb_core::maap_stream_dest`]. The 16R learns the address from our SRP
/// Talker Advertise either way.
///
/// This previously used `91:E0:F0:01:xx:xx`, one octet above the pool, to keep
/// clear of the dext's AVDECC fallback classifier (which claims all of
/// `91:E0:F0:00:xx:xx`). That was the wrong trade and it cost a day: the
/// `01:xx:xx` block BEGINS at the AVDECC multicast address and is reserved for
/// 1722.1, so a stream addressed there is not a stream address at all. The
/// 16R reserved bandwidth for it, answered Listener Ready, counted the frames
/// in `FRAMES_RX` — and never raised `MEDIA_LOCKED`, because its audio engine
/// never took delivery. The classifier concern was unfounded: it is only the
/// fallback for a frame no per-stream filter matched, and the 16R's own
/// streams (`91:E0:F0:00:CE:F4`..`FB`) sit inside that same block and reach our
/// audio ring correctly.
///
/// `MAAP_BLOCK_LEN` is the room reserved for consecutive stream addresses, so
/// the base always leaves a whole device's worth inside the pool.
fn stream_dest_mac(stream_id: avb_core::StreamId) -> avb_core::MacAddr {
    const MAAP_BLOCK_LEN: u16 = 64;
    avb_core::maap_stream_dest(stream_id.mac(), stream_id.unique_id(), MAAP_BLOCK_LEN)
}

/// The MSRP Talker Advertise TSpec for our stream (802.1Q-2018 §35.2.2.8).
///
/// Pure, so the values the 16R is asked to reserve against are testable rather
/// than buried in the TX loop.
///
/// `max_frame_size` is the AVTPDU: the AAF header plus the payload, PLUS ONE
/// byte — and deliberately NOT the Ethernet header or the VLAN tag.
///
/// This is normative, from the Milan Baseline Interoperability Specification
/// §6.3.2, which tabulates the TSPEC a Talker PAAD "shall use":
///
/// ```text
///   AAF PCM32, 48 kHz, N channels    MaxFrameSize = 24*N + 24 + 1
/// ```
///
/// For our 8-channel 48 kHz 6-spf stream that is 24*8 + 24 + 1 = 217. The
/// spec's own note says MaxFrameSize "accounts for the size of the AVTPDU
/// header and the size of the payload", with "one more byte added to take the
/// fact into account, that the sampling clock of the PAAD may be a bit faster
/// than the nominal frequency" — and its bandwidth derivation then ADDS the
/// Ethernet and VLAN header and the FCS on top (`F = MaxFrameSize + 22`),
/// which is what proves they are not already inside it.
///
/// We previously declared 234 = 14 + 4 + 216, i.e. the whole Ethernet frame
/// and no drift byte. A Milan listener validates the TSPEC of a stream it is
/// asked to accept against the value for that format, so an over-declared
/// frame size is a reservation that does not describe the stream. That is a
/// candidate for the 16R reserving the path and counting our frames while
/// never media-locking.
///
/// Computed from `pdu_len` rather than from the channel count so it cannot
/// drift from the stream we really transmit if the format changes; the test
/// checks it against the spec's own 24*N + 24 + 1 arithmetic, so a wrong
/// `pdu_len` fails rather than being restated.
///
/// An SR class A talker emits one frame per 125 µs observation interval, hence
/// `max_interval_frames: 1` — which §6.3.2 also mandates. Priority comes from
/// the SR class A domain (the same PCP the frames are tagged with, so the
/// reservation matches the traffic). `rank: true` is non-emergency.
///
/// `accumulated_latency_ns` is our OWN transmit latency, not 0. It accumulates
/// along the path as bridges add transit time, but it STARTS at the talker's
/// own worst-case source-to-wire delay — see
/// `TALKER_INITIAL_ACCUMULATED_LATENCY_NS`, whose doc records the two
/// reference devices (Apple 12 836 ns, the 16R 150 000 ns) measured on this
/// same direct cable with no bridge in either path. We previously declared 0,
/// which claims the stream arrives with no latency at all.
fn talker_tspec(stream_id: avb_core::StreamId, pdu_len: usize) -> avb_srp::attr::TalkerAdvertise {
    use avb_srp::attr::{DEFAULT_DOMAIN, TALKER_INITIAL_ACCUMULATED_LATENCY_NS};
    avb_srp::attr::TalkerAdvertise {
        stream_id,
        dest_mac: stream_dest_mac(stream_id),
        vlan_id: DEFAULT_DOMAIN.vid,
        max_frame_size: pdu_len.saturating_add(1) as u16,
        max_interval_frames: 1,
        priority: DEFAULT_DOMAIN.priority,
        rank: true,
        accumulated_latency_ns: TALKER_INITIAL_ACCUMULATED_LATENCY_NS,
    }
}

/// Put an ADPDU on the wire, multicast to the AVDECC address.
///
/// The PDU itself comes from the entity, not from here: ADP and the ENTITY
/// descriptor must advertise the same capabilities and the same
/// `entity_model_id`, and a controller that sees them disagree treats the
/// entity as misbehaving. Building one of them by hand is how they drift.
/// `TMR_ADVERTISE` — Milan v1.3 Table 5.55: *"ADP advertise timer expired.
/// This is a timer with a fixed value of 5 seconds."*
const ADP_TMR_ADVERTISE_NS: u64 = 5_000_000_000;
/// Upper bound of `TMR_DELAY` — Table 5.55: *"ADP random delay timer expired.
/// This is a timer with a random value between 0 and 4 seconds."*
const ADP_TMR_DELAY_MAX_NS: u64 = 4_000_000_000;

/// A fresh `TMR_DELAY` value in `0..=ADP_TMR_DELAY_MAX_NS`.
///
/// The delay is an anti-storm measure, not a security primitive: its whole job
/// is to stop every PAAD that came up together from advertising in lockstep,
/// so a cheap decorrelated source is sufficient and a CSPRNG dependency is
/// not warranted. This mixes the monotonic clock's low bits, which differ per
/// call and per machine, rather than adding a crate to a workspace that pins
/// every dependency exactly.
fn adp_random_delay_ns() -> u64 {
    // Multiply by a large odd constant so the low-order clock bits, which
    // advance fastest, propagate upward instead of only affecting the bottom
    // of the range. `now_mach` is a per-function closure in this module, so
    // read the monotonic clock directly here.
    let ticks = unsafe { mach2::mach_time::mach_absolute_time() };
    let t = ticks.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (t >> 11) % (ADP_TMR_DELAY_MAX_NS + 1)
}

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
/// Push unsolicited AEM notifications to every controller that registered for
/// them, and return how many frames went out.
///
/// A controller sends REGISTER_UNSOLICITED_NOTIFICATION to be TOLD when our
/// state changes rather than poll us. We accepted those registrations and
/// then sent nothing — the controller sits waiting on a promise. The 16R
/// registers on us (one `command 0x0024` per enumeration) and then lists our
/// streams "(offline)" with no format info, while Apple's entity, which does
/// push after the same registration, is listed as selectable.
///
/// Notifies on GET_STREAM_INFO and GET_STREAM_FORMAT — the two that describe
/// the state a connection change actually alters. The payloads come from our
/// own handlers, so a notification can never describe state we could not also
/// answer a direct query about.
fn push_unsolicited(
    dext: &mut RealDext,
    our_mac: avb_core::MacAddr,
    entity: &avb_avdecc::entity::talker::TalkerEntity,
    log: &Logger,
) -> u64 {
    use avb_avdecc::constants::aem_cmd;
    let mut sent = 0u64;
    for cmd in [aem_cmd::GET_STREAM_INFO, aem_cmd::GET_STREAM_FORMAT] {
        for (dst, resp) in entity.unsolicited(cmd) {
            send_aem_response(dext, our_mac, dst, &resp);
            sent += 1;
            log.log(
                Level::Info,
                "aem",
                &format!(
                    "pushed unsolicited 0x{:04x} to {dst} (it registered for notifications)",
                    resp.command_type
                ),
            );
        }
    }
    sent
}

/// How often to push the per-stream counter notifications.
///
/// Apple's entity — the peer this 16R lists as selectable — pushes an
/// unsolicited GET_COUNTERS for EVERY one of its STREAM_OUTPUTs on a steady
/// cadence: captured off the wire, 128 pushes over 40 s, 16 each for
/// STREAM_OUTPUT 0..7, i.e. one per stream every 2.5 s. Counters change
/// continuously, so this is a state change being reported on a cadence, not
/// a fabricated heartbeat.
const UNSOL_COUNTERS_PERIOD_NS: u64 = 2_500_000_000;

/// Push an unsolicited GET_COUNTERS for each of our STREAM_OUTPUTs.
///
/// Quiet by design: at 2.5 s x N streams this would otherwise flood the log,
/// so it counts rather than narrates.
fn push_unsolicited_counters(
    dext: &mut RealDext,
    our_mac: avb_core::MacAddr,
    entity: &avb_avdecc::entity::talker::TalkerEntity,
) -> u64 {
    use avb_avdecc::constants::{aem_cmd, desc as dt};
    let mut sent = 0u64;
    for idx in 0..entity.model().stream_count() {
        for (dst, resp) in entity.unsolicited_desc(aem_cmd::GET_COUNTERS, dt::STREAM_OUTPUT, idx) {
            send_aem_response(dext, our_mac, dst, &resp);
            sent += 1;
        }
    }
    sent
}

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

/// The two ends of an AECP exchange: who we are, and the entity we are talking
/// to. Bundled because every AECP helper needs all four and they always travel
/// together.
#[derive(Clone, Copy)]
struct AecpPeers {
    our_mac: avb_core::MacAddr,
    our_id: avb_core::Eui64,
    target: avb_core::Eui64,
    target_mac: avb_core::MacAddr,
}

/// Read a set of AEM descriptors from the target over AECP `READ_DESCRIPTOR`,
/// returning the raw descriptor bytes keyed by the `(descriptor_type,
/// descriptor_index)` the response itself echoes.
///
/// Only SUCCESS responses are stored. A refusal (`NO_SUCH_DESCRIPTOR`,
/// `NOT_IMPLEMENTED`) is logged and left ABSENT, so the caller can tell "the
/// 16R said no" from "nothing came back" by which keys are missing rather than
/// by a sentinel value.
/// Read the 16R's `stream_id` for each of the given STREAM_OUTPUT indices via
/// AECP `GET_STREAM_INFO`.
///
/// This is how the inbound channel map becomes REPRODUCIBLE. The ring's slots
/// must be in the 16R's own STREAM_OUTPUT index order so that ring channels
/// 1-8 are its "Send 1-8" on every run; binding streams in the order their
/// packets happen to arrive would reshuffle 64 channels between runs. The
/// stream id is not in the STREAM_OUTPUT descriptor, and deriving it from the
/// entity id by pattern would be a guess about how this device forms ids —
/// `GET_STREAM_INFO` states it.
///
/// Returns only the indices the 16R actually answered, so a missing one is
/// absent rather than defaulted to a plausible-looking id.
fn read_stream_ids(
    log: &Logger,
    dext: &mut RealDext,
    peers: AecpPeers,
    indices: &[u16],
    timeout_ns: u64,
) -> Result<std::collections::BTreeMap<u16, u64>, Exit> {
    use avb_avdecc::aecp::{AecpEngine, AecpOutcome, AemPdu};
    use avb_avdecc::constants::{aecp_msg, aecp_status, aem_cmd, desc as aem_desc, subtype};
    use avb_avtp::common::ETHERTYPE_AVTP;
    use avb_core::eth::{EthFrame, EthHeader};
    use avb_core::time::{MachTicks, Timebase};
    use avb_core::TxFrame;

    use crate::dextio::{DextPort, SendResult};

    let info = dext.device().info().map_err(|_| Exit::DextFault)?;
    let tb = Timebase {
        numer: info.tb_numer,
        denom: info.tb_denom,
    };
    let now_mach = || MachTicks(unsafe { mach2::mach_time::mach_absolute_time() });
    let start = now_mach();
    let ns_since = |t: MachTicks| tb.ticks_to_ns(now_mach().ticks_since(t));

    let mut eng = AecpEngine::new(peers.our_id);
    for &i in indices {
        // §7.4.15 command payload: descriptor_type(2), descriptor_index(2).
        let mut pl = Vec::with_capacity(4);
        pl.extend_from_slice(&aem_desc::STREAM_OUTPUT.to_be_bytes());
        pl.extend_from_slice(&i.to_be_bytes());
        eng.enqueue(peers.target, peers.target_mac, aem_cmd::GET_STREAM_INFO, pl);
    }

    let mut got: std::collections::BTreeMap<u16, u64> = Default::default();
    while got.len() < indices.len() && ns_since(start) < timeout_ns {
        if shutdown_requested() {
            return Err(Exit::Interrupted);
        }
        let now = ns_since(start);
        if let Some(send) = eng.poll(now) {
            let hdr = EthHeader {
                dst: send.target_mac,
                src: peers.our_mac,
                vlan: None,
                ethertype: ETHERTYPE_AVTP,
            };
            if let Ok(f) = TxFrame::build(&hdr, &send.frame, false, 0) {
                if matches!(dext.send(f.bytes(), false, 0), SendResult::Fatal) {
                    return Err(Exit::DextFault);
                }
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
            if resp.message_type != aecp_msg::AEM_RESPONSE {
                continue;
            }
            let Some(AecpOutcome::Response(r)) = eng.on_rx(&resp, ns_since(start)) else {
                continue;
            };
            if r.command_type != aem_cmd::GET_STREAM_INFO || r.status != aecp_status::SUCCESS {
                continue;
            }
            // §7.4.16 response: descriptor_type(2) descriptor_index(2)
            // flags(4) stream_format(8) stream_id(8) ... — the same offsets
            // the record path reads, which answered 9 of 9 on this device.
            let b = &r.payload;
            let Some(idx) = b
                .get(2..4)
                .and_then(|x| <[u8; 2]>::try_from(x).ok())
                .map(u16::from_be_bytes)
            else {
                continue;
            };
            let Some(sid) = b
                .get(16..24)
                .and_then(|x| <[u8; 8]>::try_from(x).ok())
                .map(u64::from_be_bytes)
            else {
                continue;
            };
            if sid != 0 && indices.contains(&idx) {
                got.insert(idx, sid);
            }
        }
        let _ = eng.on_tick(ns_since(start));
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    log.log(
        Level::Info,
        "aecp",
        &format!(
            "16R stream ids: {} of {} STREAM_OUTPUT(s) answered GET_STREAM_INFO",
            got.len(),
            indices.len()
        ),
    );
    Ok(got)
}

fn read_descriptors(
    log: &Logger,
    dext: &mut RealDext,
    peers: AecpPeers,
    want: &[(u16, u16)],
    timeout_ns: u64,
) -> Result<std::collections::BTreeMap<(u16, u16), Vec<u8>>, Exit> {
    use avb_avdecc::aecp::{AecpEngine, AecpOutcome, AemPdu};
    use avb_avdecc::constants::{aecp_msg, aecp_status, aem_cmd, subtype};
    use avb_avtp::common::ETHERTYPE_AVTP;
    use avb_core::eth::{EthFrame, EthHeader};
    use avb_core::time::{MachTicks, Timebase};
    use avb_core::TxFrame;

    use crate::dextio::{DextPort, SendResult};

    let info = dext.device().info().map_err(|_| Exit::DextFault)?;
    let tb = Timebase {
        numer: info.tb_numer,
        denom: info.tb_denom,
    };
    let now_mach = || MachTicks(unsafe { mach2::mach_time::mach_absolute_time() });
    let start = now_mach();
    let ns_since = |t: MachTicks| tb.ticks_to_ns(now_mach().ticks_since(t));

    // READ_DESCRIPTOR command payload (§7.4.5): configuration_index(2),
    // reserved(2), descriptor_type(2), descriptor_index(2).
    let read_payload = |dtype: u16, dindex: u16| -> Vec<u8> {
        let mut v = Vec::with_capacity(8);
        v.extend_from_slice(&0u16.to_be_bytes());
        v.extend_from_slice(&0u16.to_be_bytes());
        v.extend_from_slice(&dtype.to_be_bytes());
        v.extend_from_slice(&dindex.to_be_bytes());
        v
    };

    let mut eng = AecpEngine::new(peers.our_id);
    for &(dt, di) in want {
        eng.enqueue(
            peers.target,
            peers.target_mac,
            aem_cmd::READ_DESCRIPTOR,
            read_payload(dt, di),
        );
    }

    let mut got: std::collections::BTreeMap<(u16, u16), Vec<u8>> = Default::default();
    loop {
        if shutdown_requested() {
            return Err(Exit::Interrupted);
        }
        if eng.queued_count() == 0 && eng.in_flight_count() == 0 {
            break;
        }
        if ns_since(start) >= timeout_ns {
            log.log(
                Level::Warn,
                "aecp",
                &format!(
                    "AEM read timed out with {} of {} descriptors in hand",
                    got.len(),
                    want.len()
                ),
            );
            break;
        }
        let now = ns_since(start);

        if let Some(send) = eng.poll(now) {
            let hdr = EthHeader {
                dst: send.target_mac,
                src: peers.our_mac,
                vlan: None,
                ethertype: ETHERTYPE_AVTP,
            };
            if let Ok(f) = TxFrame::build(&hdr, &send.frame, false, 0) {
                // Busy / NoResources are retried by the engine's own aging.
                if matches!(dext.send(f.bytes(), false, 0), SendResult::Fatal) {
                    return Err(Exit::DextFault);
                }
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
            if resp.message_type != aecp_msg::AEM_RESPONSE {
                continue;
            }
            let Some(AecpOutcome::Response(r)) = eng.on_rx(&resp, now) else {
                continue;
            };
            if r.command_type != aem_cmd::READ_DESCRIPTOR {
                continue;
            }
            // Response payload: configuration_index(2) reserved(2) descriptor.
            let Some(d) = r.payload.get(4..) else {
                continue;
            };
            if d.len() < 4 {
                continue;
            }
            let dt = u16::from_be_bytes([*d.first().unwrap_or(&0), *d.get(1).unwrap_or(&0)]);
            let di = u16::from_be_bytes([*d.get(2).unwrap_or(&0), *d.get(3).unwrap_or(&0)]);
            if r.status != aecp_status::SUCCESS {
                log.log(
                    Level::Warn,
                    "aecp",
                    &format!(
                        "16R refused descriptor type=0x{dt:04x} index={di} with status {}",
                        r.status
                    ),
                );
                continue;
            }
            got.insert((dt, di), d.to_vec());
        }

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
                    &format!("AEM read 0x{command_type:04x} seq {sequence_id} timed out"),
                );
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    Ok(got)
}

/// Derive our talker output format from the 16R's own AEM.
///
/// `AGENTS.md`: sample rates, channel counts and stream counts are never
/// hardcoded — they come from the 16R. This walks the part of its AEM that
/// decides our output format: CONFIGURATION 0 for how many STREAM_INPUT sinks
/// it has, each of those sinks, and AUDIO_UNIT 0 for the rate cross-check.
/// [`avb_avdecc::talker_output_format`] then picks the sink we are about to
/// bind.
///
/// `listener_uid` of `None` means "choose the sink", which is the default:
/// index 0 on the 16R is `Return CRF`, its media-clock stream, so a fixed
/// default of 0 reserved and connected a stream that carries no audio.
/// Returns the format AND the sink actually chosen, so the ACMP connect binds
/// the same sink the format came from.
///
/// Any failure is a refusal to transmit, never a fallback to a default. A
/// format that does not match the sink makes the 16R discard every packet,
/// which on the wire is indistinguishable from a transmit fault.
fn derive_talker_format(
    log: &Logger,
    dext: &mut RealDext,
    peers: AecpPeers,
    listener_uid: Option<u16>,
    timeout_ns: u64,
) -> Result<(avb_avtp::stream_format::StreamFormat, Vec<u16>), Exit> {
    use avb_avdecc::aem::desc::{decode_audio_unit, decode_stream};
    use avb_avdecc::constants::desc as aem_desc;

    // Pass 1: the CONFIGURATION (for the STREAM_INPUT count) and the
    // AUDIO_UNIT (for its current sampling rate).
    let first = read_descriptors(
        log,
        dext,
        peers,
        &[(aem_desc::CONFIGURATION, 0), (aem_desc::AUDIO_UNIT, 0)],
        timeout_ns,
    )?;

    let Some(config) = first.get(&(aem_desc::CONFIGURATION, 0)) else {
        log.log(
            Level::Error,
            "aecp",
            "the 16R did not return its CONFIGURATION descriptor, so the output format cannot be derived from its AEM (AGENTS.md forbids assuming one)",
        );
        return Err(Exit::TalkerNotFound);
    };
    // How many STREAM_INPUT sinks it exposes, from its own descriptor_counts.
    let sink_count = config_descriptor_counts(config)
        .into_iter()
        .find(|(dt, _)| *dt == aem_desc::STREAM_INPUT)
        .map(|(_, n)| n)
        .unwrap_or(0);
    if sink_count == 0 {
        log.log(
            Level::Error,
            "aecp",
            "the 16R's CONFIGURATION lists no STREAM_INPUT descriptors, so it has no sink to play to",
        );
        return Err(Exit::TalkerNotFound);
    }

    let audio_unit_hz = first
        .get(&(aem_desc::AUDIO_UNIT, 0))
        .and_then(|b| decode_audio_unit(b))
        .map(|au| au.current_sampling_rate);

    // Pass 2: every STREAM_INPUT, so the refusal can name how many exist and
    // the derived format is indexed by the real descriptor index.
    let want: Vec<(u16, u16)> = (0..sink_count)
        .map(|i| (aem_desc::STREAM_INPUT, i))
        .collect();
    let sinks_raw = read_descriptors(log, dext, peers, &want, timeout_ns)?;

    // Dense in descriptor index, so index i really is STREAM_INPUT i. A gap
    // (the 16R refused one) truncates rather than silently shifting the rest.
    let mut sinks = Vec::with_capacity(usize::from(sink_count));
    for i in 0..sink_count {
        match sinks_raw
            .get(&(aem_desc::STREAM_INPUT, i))
            .and_then(|b| decode_stream(b))
        {
            Some(s) => sinks.push(s),
            None => break,
        }
    }
    log.log(
        Level::Info,
        "aecp",
        &format!(
            "16R AEM: {} of {} STREAM_INPUT sinks read, AUDIO_UNIT rate {}",
            sinks.len(),
            sink_count,
            audio_unit_hz
                .map(|h| format!("{h} Hz"))
                .unwrap_or_else(|| "unknown".into())
        ),
    );

    // Choose the sinks. An explicit --listener-uid names exactly one;
    // otherwise EVERY AAF audio sink is bound, in descriptor-index order.
    //
    // The 16R's STREAM_INPUT 0 is `Return CRF`, its media-clock sink, so a
    // fixed default of 0 reserved and connected a stream that carries no
    // audio. Its audio sinks are 1..=8 (`Return 1-8` .. `Return 57-64`), 8
    // channels each — 64 in total, which is the device's full width.
    let uids: Vec<u16> = match listener_uid {
        Some(u) => vec![u],
        None => {
            let mut v: Vec<u16> = Vec::new();
            let mut chosen_fmt: Option<avb_avtp::stream_format::StreamFormat> = None;
            for (i, sink) in sinks.iter().enumerate() {
                let Ok(idx) = u16::try_from(i) else { break };
                let Some(a) = sink.current_format.aaf() else {
                    log.log(
                        Level::Info,
                        "aecp",
                        &format!(
                            "16R STREAM_INPUT {idx} ({}) is {} — not audio, skipping",
                            sink.object_name,
                            sink.current_format.subtype_name()
                        ),
                    );
                    continue;
                };
                if a.ut || !sink.current_format.decodable() {
                    continue;
                }
                // Every bound sink must share one format: the output ring has
                // a single geometry across its slots, so a sink configured
                // differently cannot be fed from a slot sized for the rest.
                match chosen_fmt {
                    None => chosen_fmt = Some(sink.current_format),
                    Some(f) if f == sink.current_format => {}
                    Some(f) => {
                        log.log(
                            Level::Warn,
                            "aecp",
                            &format!(
                                "16R STREAM_INPUT {idx} ({}) is 0x{:016x} but the others are \
                                 0x{:016x}; this build sends one geometry on every stream, so it \
                                 is skipped",
                                sink.object_name, sink.current_format.0, f.0
                            ),
                        );
                        continue;
                    }
                }
                v.push(idx);
            }
            if v.is_empty() {
                log.log(
                    Level::Error,
                    "aecp",
                    &format!(
                        "none of the 16R's {} STREAM_INPUT sinks is an AAF audio sink we can \
                         packetize (they are: {}); nothing to play to",
                        sinks.len(),
                        sinks
                            .iter()
                            .enumerate()
                            .map(|(i, s)| format!(
                                "{i}={} {}",
                                s.object_name,
                                s.current_format.subtype_name()
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
                return Err(Exit::TalkerNotFound);
            }
            let names: Vec<String> = v
                .iter()
                .filter_map(|&u| sinks.get(usize::from(u)))
                .map(|s| s.object_name.clone())
                .collect();
            log.log(
                Level::Info,
                "aecp",
                &format!(
                    "selected {} 16R audio sink(s) {v:?} ({}); pass --listener-uid for one",
                    v.len(),
                    names.join(", ")
                ),
            );
            v
        }
    };
    let uid = *uids.first().expect("uids is non-empty");

    match avb_avdecc::talker_output_format(&sinks, audio_unit_hz, uid) {
        Ok(fmt) => {
            let a = fmt.aaf();
            log.log(
                Level::Info,
                "aecp",
                &format!(
                    "output format derived from 16R STREAM_INPUT {uid}: 0x{:016x} ({})",
                    fmt.0,
                    a.map(|a| format!(
                        "{} Hz, {} ch, {} spf, {}-bit",
                        a.nsr_hz().unwrap_or(0),
                        a.channels,
                        a.spf,
                        a.bit_depth
                    ))
                    .unwrap_or_else(|| "not AAF".into())
                ),
            );
            Ok((fmt, uids))
        }
        Err(e) => {
            log.log(
                Level::Error,
                "aecp",
                &format!("cannot derive our output format from the 16R's AEM: {e}"),
            );
            Err(Exit::TalkerNotFound)
        }
    }
}

/// Report AAF audio the inbound path THREW AWAY because it binds one stream.
///
/// The 16R exposes eight 8-channel AAF streams per direction, and once a
/// controller connects them it talks all of them. This path keeps one, so a
/// run can take 64 channels off the wire and report a clean 8-channel capture
/// — 1 612 459 audio slots in, 1 199 976 frames placed, and no line saying the
/// other seven streams were discarded. Say it plainly instead.
fn report_discarded_streams(log: &Logger, dir: &str, lrx: &crate::listenerrx::ListenerRx) {
    let (pdus, streams) = lrx.non_primary();
    if pdus == 0 {
        return;
    }
    log.log(
        Level::Warn,
        dir,
        &format!(
            "DISCARDED {pdus} AAF packets from {streams} other stream(s): this path binds ONE \
             stream and the 16R is talking {} — that is {} of its channels being dropped, not a \
             fault but not the full device either",
            streams + 1,
            streams * 8
        ),
    );
}

/// How a `--listener-uid` choice reads in a log line: an explicit sink index,
/// or that we will choose one from the 16R's AEM.
fn uid_label(uid: Option<u16>) -> String {
    match uid {
        Some(u) => format!("{u} (explicit)"),
        None => "auto from the 16R's AEM".to_string(),
    }
}

/// A transmitted peak as dBFS against full-scale 32-bit, or the plain fact
/// that it was digital silence.
///
/// Silence is NOT a transmit fault — nothing playing on the Mac is the normal
/// idle state — but it must be stated, because a run that establishes the
/// stream perfectly and sends all zeros otherwise reads as a success.
fn peak_label(peak: i64) -> String {
    if peak <= 0 {
        return "digital silence — nothing is playing to the DreamAVB device".to_string();
    }
    let full = i64::from(i32::MAX) as f64;
    let dbfs = 20.0 * (peak as f64 / full).log10();
    format!("{dbfs:.1} dBFS")
}

/// Resolve which of the 16R's STREAM_OUTPUT sources our listener direction
/// should bind, and its format, from the 16R's own AEM.
///
/// The mirror of [`derive_talker_format`] for the inbound direction. Index 0 is
/// `Send CRF` — the 16R's media-clock source — so the inbound ACMP connect
/// previously asked for a stream that carries no audio. Returns the source's
/// `unique_id` and its current format.
fn derive_inbound_source(
    log: &Logger,
    dext: &mut RealDext,
    peers: AecpPeers,
    talker_uid: Option<u16>,
    timeout_ns: u64,
) -> Result<(avb_avtp::stream_format::StreamFormat, u16), Exit> {
    use avb_avdecc::aem::desc::decode_stream;
    use avb_avdecc::constants::desc as aem_desc;

    let base = read_descriptors(
        log,
        dext,
        peers,
        &[(aem_desc::CONFIGURATION, 0)],
        timeout_ns,
    )?;
    let Some(config) = base.get(&(aem_desc::CONFIGURATION, 0)) else {
        log.log(
            Level::Error,
            "aecp",
            "the 16R did not return its CONFIGURATION descriptor, so its STREAM_OUTPUT count is unknown and the inbound source cannot be chosen",
        );
        return Err(Exit::TalkerNotFound);
    };
    let source_count = config_descriptor_counts(config)
        .into_iter()
        .find(|(dt, _)| *dt == aem_desc::STREAM_OUTPUT)
        .map(|(_, n)| n)
        .unwrap_or(0);
    if source_count == 0 {
        log.log(
            Level::Error,
            "aecp",
            "the 16R's CONFIGURATION lists no STREAM_OUTPUT descriptors, so it has nothing to send us",
        );
        return Err(Exit::TalkerNotFound);
    }

    let want: Vec<(u16, u16)> = (0..source_count)
        .map(|i| (aem_desc::STREAM_OUTPUT, i))
        .collect();
    let raw = read_descriptors(log, dext, peers, &want, timeout_ns)?;
    let mut sources = Vec::with_capacity(usize::from(source_count));
    for i in 0..source_count {
        match raw
            .get(&(aem_desc::STREAM_OUTPUT, i))
            .and_then(|b| decode_stream(b))
        {
            Some(s) => sources.push(s),
            None => break,
        }
    }
    log.log(
        Level::Info,
        "aecp",
        &format!(
            "16R AEM: {} of {} STREAM_OUTPUT sources read",
            sources.len(),
            source_count
        ),
    );

    let uid = match talker_uid {
        Some(u) => u,
        None => match avb_avdecc::first_audio_stream(&sources) {
            Some(u) => u,
            None => {
                log.log(
                    Level::Error,
                    "aecp",
                    &format!(
                        "none of the 16R's {} STREAM_OUTPUT sources is an AAF audio source we can \
                         depacketize (they are: {}); nothing to record from",
                        sources.len(),
                        sources
                            .iter()
                            .enumerate()
                            .map(|(i, s)| format!(
                                "{i}={} {}",
                                s.object_name,
                                s.current_format.subtype_name()
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
                return Err(Exit::TalkerNotFound);
            }
        },
    };
    let Some(src) = sources.get(usize::from(uid)) else {
        log.log(
            Level::Error,
            "aecp",
            &format!(
                "the 16R has no STREAM_OUTPUT {uid} (it exposes {}); pass --talker-uid below that",
                sources.len()
            ),
        );
        return Err(Exit::TalkerNotFound);
    };
    let fmt = src.current_format;
    let Some(a) = fmt.aaf().filter(|a| !a.ut) else {
        log.log(
            Level::Error,
            "aecp",
            &format!(
                "16R STREAM_OUTPUT {uid} ({}) is configured for {}, not an exact AAF format; \
                 this build only depacketizes AAF",
                src.object_name,
                fmt.subtype_name()
            ),
        );
        return Err(Exit::TalkerNotFound);
    };
    log.log(
        Level::Info,
        "aecp",
        &format!(
            "selected 16R STREAM_OUTPUT {uid} ({}) as our audio source: 0x{:016x} ({} Hz, {} ch, \
             {} spf, {}-bit); pass --talker-uid to override",
            src.object_name,
            fmt.0,
            a.nsr_hz().unwrap_or(0),
            a.channels,
            a.spf,
            a.bit_depth
        ),
    );
    Ok((fmt, uid))
}

/// Resolve which of the 16R's STREAM_OUTPUT sources our listener direction
/// should bind, and their format, from the 16R's own AEM.
///
/// Returns EVERY AAF audio source in descriptor-index order, not just the
/// first. The 16R exposes nine STREAM_OUTPUTs — index 0 is `Send CRF`, its
/// media-clock source, and 1..=8 are `Send 1-8` .. `Send 57-64`, eight
/// channels each. Binding one of them keeps 8 of the device's 64 channels.
///
/// Every bound source must share one format, because the ring has a single
/// geometry across its slots. A source that differs is refused rather than
/// placed into a slot whose width does not match it.
fn derive_inbound_sources(
    log: &Logger,
    dext: &mut RealDext,
    peers: AecpPeers,
    timeout_ns: u64,
) -> Result<(avb_avtp::stream_format::StreamFormat, Vec<u16>), Exit> {
    use avb_avdecc::aem::desc::decode_stream;
    use avb_avdecc::constants::desc as aem_desc;

    let base = read_descriptors(
        log,
        dext,
        peers,
        &[(aem_desc::CONFIGURATION, 0)],
        timeout_ns,
    )?;
    let Some(config) = base.get(&(aem_desc::CONFIGURATION, 0)) else {
        log.log(
            Level::Error,
            "aecp",
            "the 16R did not return its CONFIGURATION descriptor, so its STREAM_OUTPUT count is unknown and the inbound sources cannot be chosen",
        );
        return Err(Exit::TalkerNotFound);
    };
    let source_count = config_descriptor_counts(config)
        .into_iter()
        .find(|(dt, _)| *dt == aem_desc::STREAM_OUTPUT)
        .map(|(_, n)| n)
        .unwrap_or(0);
    if source_count == 0 {
        log.log(
            Level::Error,
            "aecp",
            "the 16R's CONFIGURATION lists no STREAM_OUTPUT descriptors, so it has nothing to send us",
        );
        return Err(Exit::TalkerNotFound);
    }

    let want: Vec<(u16, u16)> = (0..source_count)
        .map(|i| (aem_desc::STREAM_OUTPUT, i))
        .collect();
    let raw = read_descriptors(log, dext, peers, &want, timeout_ns)?;
    let mut sources = Vec::with_capacity(usize::from(source_count));
    for i in 0..source_count {
        match raw
            .get(&(aem_desc::STREAM_OUTPUT, i))
            .and_then(|b| decode_stream(b))
        {
            Some(s) => sources.push(s),
            None => break,
        }
    }
    log.log(
        Level::Info,
        "aecp",
        &format!(
            "16R AEM: {} of {} STREAM_OUTPUT sources read",
            sources.len(),
            source_count
        ),
    );

    // Every AAF source we can depacketize, in descriptor-index order.
    let mut audio: Vec<u16> = Vec::new();
    let mut fmt: Option<avb_avtp::stream_format::StreamFormat> = None;
    for (i, s) in sources.iter().enumerate() {
        let Ok(idx) = u16::try_from(i) else { break };
        let Some(a) = s.current_format.aaf() else {
            log.log(
                Level::Info,
                "aecp",
                &format!(
                    "16R STREAM_OUTPUT {idx} ({}) is {} — not audio, skipping",
                    s.object_name,
                    s.current_format.subtype_name()
                ),
            );
            continue;
        };
        if a.ut || !s.current_format.decodable() {
            log.log(
                Level::Warn,
                "aecp",
                &format!(
                    "16R STREAM_OUTPUT {idx} ({}) is AAF but not an exact format this build can \
                     depacketize — skipping",
                    s.object_name
                ),
            );
            continue;
        }
        match fmt {
            None => fmt = Some(s.current_format),
            Some(f) if f == s.current_format => {}
            Some(f) => {
                // Mixed formats would need per-slot channel widths in the
                // ring. Refuse the odd one out rather than place it into a
                // slot sized for something else.
                log.log(
                    Level::Warn,
                    "aecp",
                    &format!(
                        "16R STREAM_OUTPUT {idx} ({}) is 0x{:016x} but the others are 0x{:016x}; \
                         this build binds one geometry across all slots, so it is skipped",
                        s.object_name, s.current_format.0, f.0
                    ),
                );
                continue;
            }
        }
        audio.push(idx);
    }

    let Some(fmt) = fmt else {
        log.log(
            Level::Error,
            "aecp",
            &format!(
                "none of the 16R's {} STREAM_OUTPUT sources is an AAF audio source we can \
                 depacketize (they are: {}); nothing to record from",
                sources.len(),
                sources
                    .iter()
                    .enumerate()
                    .map(|(i, s)| format!(
                        "{i}={} {}",
                        s.object_name,
                        s.current_format.subtype_name()
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
        return Err(Exit::TalkerNotFound);
    };
    let a = fmt.aaf();
    log.log(
        Level::Info,
        "aecp",
        &format!(
            "selected {} 16R audio source(s) {audio:?}: 0x{:016x} ({} Hz, {} ch each, {} spf, \
             {}-bit) = {} channels inbound",
            audio.len(),
            fmt.0,
            a.map(|a| a.nsr_hz().unwrap_or(0)).unwrap_or(0),
            a.map(|a| a.channels).unwrap_or(0),
            a.map(|a| a.spf).unwrap_or(0),
            a.map(|a| a.bit_depth).unwrap_or(0),
            audio.len() * usize::from(a.map(|a| a.channels).unwrap_or(0)),
        ),
    );
    Ok((fmt, audio))
}

/// Resolve the 16R's entity id AND MAC — the LISTENER for the playback
/// direction. `--listener <id>` selects which entity to wait for; otherwise the
/// sole AVDECC entity on the link is taken, exactly like the record path. The
/// MAC is required because the AEM read that derives our output format is a
/// unicast AECP command. Exits `TalkerNotFound` (6) on timeout or when several
/// candidates make the choice ambiguous.
fn discover_listener(
    opts: &GlobalOpts,
    log: &Logger,
    dext: &mut RealDext,
    our_id: avb_core::Eui64,
    our_mac: avb_core::MacAddr,
    want: Option<u64>,
) -> Result<(avb_core::Eui64, avb_core::MacAddr), Exit> {
    use avb_avdecc::adp::Adpdu;
    use avb_avdecc::constants::subtype;
    use avb_avdecc::EntityTable;
    use avb_avtp::common::ETHERTYPE_AVTP;
    use avb_core::eth::{EthFrame, EthHeader};
    use avb_core::time::{MachTicks, Timebase};
    use avb_core::{Eui64, TxFrame};

    use crate::dextio::DextPort;

    // We need the 16R's MAC, not just its entity id: the AEM read that derives
    // our output format is a UNICAST AECP command, and the only place its MAC
    // comes from is an observed ADPDU. So `--listener` selects WHICH entity to
    // wait for; it no longer skips discovery, because an id alone cannot be
    // addressed.
    let want_id = want.map(Eui64::from_u64);
    if let Some(id) = want_id {
        log.log(
            Level::Info,
            "adp",
            &format!(
                "listener given: 0x{:016x} (waiting for its ADP to learn its MAC)",
                u64::from_be_bytes(id.0)
            ),
        );
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
    let mut peer_adp_dumped = false;
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
            // We do NOT advertise ourselves here. Our ADP is built from our
            // entity model, and the model's stream format is exactly what this
            // discovery exists to go and read from the 16R. Advertising a
            // placeholder first would publish an entity_model_id we are about
            // to change. We advertise as soon as the model is real, which is
            // still before any ACMP (a Milan entity ignores ACMP from an
            // entity it has not discovered — review H3).
            last_discover = now_mach();
        }
        for slot in dext.rx_ctl() {
            if let Ok(p) = EthFrame::parse(&slot.data) {
                if p.hdr.ethertype == ETHERTYPE_AVTP
                    && p.payload.first().copied() == Some(subtype::ADP)
                {
                    if let Some(adp) = Adpdu::decode(p.payload) {
                        // The 16R's own advertisement, dumped once. Its
                        // controller lists the 16R as online and us as
                        // offline while every stream-layer check passes, so
                        // what differs is how the two entities advertise
                        // themselves — and the 16R's ADPDU is the reference
                        // for what this network's controller accepts.
                        if !peer_adp_dumped
                            && adp.message_type == avb_avdecc::constants::adp_msg::ENTITY_AVAILABLE
                        {
                            peer_adp_dumped = true;
                            log.log(Level::Info, "wire", &dump_adp("ADP 16R's", &adp));
                        }
                        let _ = table.observe(&adp, p.hdr.src, ns_since(start), false);
                    }
                }
            }
        }
        // With --listener we wait for THAT entity; otherwise the sole entity on
        // the wire. Either way we return its MAC alongside its id.
        let resolved = match want_id {
            Some(id) => table.get(id).map(|e| (id, e.mac)),
            None => match table.sole_talker() {
                Ok(Some(id)) => table.get(id).map(|e| (id, e.mac)),
                Err(ids) => {
                    log.log(
                        Level::Error,
                        "adp",
                        &format!("{} entities discovered; pass --listener <id>", ids.len()),
                    );
                    return Err(Exit::TalkerNotFound);
                }
                Ok(None) => None,
            },
        };
        if let Some((id, mac)) = resolved {
            log.log(
                Level::Info,
                "adp",
                &format!(
                    "listener (16R) discovered: 0x{:016x} at {:02x?}",
                    u64::from_be_bytes(id.0),
                    mac.octets()
                ),
            );
            return Ok((id, mac));
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
        // The controller leaves stream_id unset in CONNECT_RX_COMMAND. The
        // Listener obtains it from the subsequent CONNECT_TX_RESPONSE. Apple's
        // known-good transaction carries all-zero stream_id here; pre-filling
        // ours was a duplex-only divergence from both that capture and the
        // reusable AVDECC controller builder.
        stream_id: avb_core::StreamId(0),
        controller_entity_id: our_id,
        talker_entity_id: our_id,
        listener_entity_id: listener_id,
        talker_unique_id: (our_stream_id.0 & 0xFFFF) as u16,
        listener_unique_id: listener_uid,
        sequence_id,
        ..Default::default()
    }
}

/// Build the DISCONNECT_RX_COMMAND PDU (pure; unit-tested).
///
/// Identical addressing to [`connect_rx_pdu`] — only `message_type` differs —
/// because ACMP identifies the binding to tear down by the SAME five fields
/// that created it. A disconnect naming a different talker, stream or unique
/// id simply does not match, and the listener keeps the binding.
fn disconnect_rx_pdu(
    our_id: avb_core::Eui64,
    listener_id: avb_core::Eui64,
    our_stream_id: avb_core::StreamId,
    listener_uid: u16,
    sequence_id: u16,
) -> avb_avdecc::acmp::Acmpdu {
    let mut p = connect_rx_pdu(
        our_id,
        listener_id,
        our_stream_id,
        listener_uid,
        sequence_id,
    );
    p.message_type = avb_avdecc::constants::acmp_msg::DISCONNECT_RX_COMMAND;
    p
}

/// Tell the 16R to RELEASE its STREAM_INPUT\[`listener_uid`\] binding.
///
/// Needed because our stream ids are deterministic (our MAC plus the stream's
/// unique id), so they are identical on every run. Without a teardown the 16R
/// keeps yesterday's binding, decides it is already connected, and skips the
/// CONNECT_TX handshake entirely — which is exactly the run where
/// `ACMP talker commands answered` read 0 and every sink reported
/// MEDIA_LOCKED = 0. A binding to a stream that has stopped is not a
/// connection; it is a stale entry that prevents a fresh lock.
#[allow(clippy::too_many_arguments)]
fn disconnect_listener(
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

    let pdu = disconnect_rx_pdu(
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
                "DISCONNECT_RX_COMMAND -> 0x{:016x} uid={listener_uid}, clearing any \
                 stale binding to our stream 0x{:016x}",
                u64::from_be_bytes(listener_id.0),
                our_stream_id.0
            ),
        );
    }
}

#[cfg(test)]
mod talker_tests {
    use super::*;
    use avb_core::{Eui64, MacAddr, StreamId};

    /// The 16R's PTP_INSTANCE descriptor physically reports one PTP_PORT at
    /// base index 0 even though CONFIGURATION omits that descriptor type.
    #[test]
    fn ptp_instance_references_its_unlisted_port() {
        let mut descriptor = vec![0u8; 90];
        descriptor[86..88].copy_from_slice(&1u16.to_be_bytes());
        descriptor[88..90].copy_from_slice(&0u16.to_be_bytes());
        assert_eq!(ptp_port_range(&descriptor), Some((0, 1)));
        assert_eq!(ptp_port_range(&descriptor[..89]), None);
    }

    /// A run can establish the stream perfectly and transmit all zeros, so the
    /// summary must say which happened. These are the two readings the user
    /// acts on differently: silence means "play something", a dBFS figure
    /// means audio is really on the wire.
    #[test]
    fn peak_label_distinguishes_silence_from_signal() {
        assert!(peak_label(0).contains("digital silence"));
        assert!(peak_label(-1).contains("digital silence"));
        // Full scale is 0 dBFS, half scale about -6 dB, and a quiet signal
        // well below that — all reported as signal, never as silence.
        assert!(peak_label(i64::from(i32::MAX)).starts_with("0.0 dBFS"));
        assert!(peak_label(i64::from(i32::MAX) / 2).starts_with("-6.0 dBFS"));
        for p in [1, 1_000, 1_000_000] {
            let l = peak_label(p);
            assert!(l.ends_with("dBFS"), "{p} -> {l}");
            assert!(!l.contains("silence"), "{p} -> {l}");
        }
    }

    /// `--listener-uid` omitted must read as derived, not as a silent default
    /// of 0 — which is the 16R's CRF media-clock stream.
    #[test]
    fn uid_label_says_when_the_sink_is_derived() {
        assert!(uid_label(None).contains("auto"));
        assert!(!uid_label(None).contains('0'));
        assert!(uid_label(Some(1)).starts_with('1'));
        assert!(uid_label(Some(0)).contains("explicit"));
    }

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

        // Milan Baseline Interoperability Specification §6.3.2 tabulates the
        // TSPEC a Talker PAAD shall use. For AAF PCM32 at 48 kHz with N
        // channels: MaxFrameSize = 24*N + 24 + 1. Written as the spec's own
        // arithmetic rather than as a literal, so this checks our derivation
        // against the specification instead of restating whatever we produce.
        let channels = 8;
        let spec_max_frame_size = 24 * channels + 24 + 1;
        assert_eq!(
            t.max_frame_size, spec_max_frame_size as u16,
            "MaxFrameSize must be the Milan §6.3.2 value for AAF PCM32 48 kHz"
        );
        assert_eq!(t.max_frame_size, 217);

        // And specifically NOT the whole Ethernet frame. We declared
        // 14 + 4 + 216 = 234 for months; the spec's bandwidth derivation adds
        // the Ethernet header, VLAN tag and FCS ON TOP of MaxFrameSize
        // (F = MaxFrameSize + 22), which is what proves they are outside it.
        // A listener validating the TSPEC against the format sees an
        // over-declared frame size as a reservation that does not describe the
        // stream.
        assert_ne!(
            t.max_frame_size,
            (14 + 4 + pdu_len) as u16,
            "MaxFrameSize must exclude the Ethernet and VLAN header"
        );
        // One frame per class-A observation interval — also §6.3.2.
        assert_eq!(t.max_interval_frames, 1);
        // Same PCP the frames are tagged with, and the SR class A VID.
        assert_eq!(t.priority, avb_srp::attr::DEFAULT_DOMAIN.priority);
        assert_eq!(t.vlan_id, avb_srp::attr::DEFAULT_DOMAIN.vid);
        assert!(t.rank, "non-emergency");
        // `accumulated_latency` does NOT start at zero on a direct cable. It
        // starts at the talker's own source-to-wire delay and bridges add to
        // it. This assertion used to demand 0, which is how we shipped a
        // declaration claiming our stream arrives with no latency at all.
        //
        // The two reference talkers on this very cable, captured off the wire:
        // Apple's macOS AVB entity declares 12 836 ns and the 16R itself
        // declares 150 000 ns. Neither has a bridge in its path either, so
        // "no bridge" cannot be what makes the value zero.
        assert_ne!(
            t.accumulated_latency_ns, 0,
            "declaring 0 claims a physically impossible zero-latency stream"
        );
        assert_eq!(
            t.accumulated_latency_ns,
            avb_srp::attr::CLASS_A_INTERVAL_NS,
            "one class-A observation interval: a sample waits at most one \
             interval in the packetiser before its frame goes out"
        );
        // Sanity-bracket it against the measured reference values rather than
        // only against our own constant, so a future edit that makes the
        // constant absurd fails here.
        assert!(
            (12_836..=150_000).contains(&t.accumulated_latency_ns),
            "must be in the range the two captured reference devices bracket"
        );
        // The advertise must name the same destination the frames are sent to,
        // or the 16R reserves a path for an address we never use.
        assert_eq!(t.dest_mac, stream_dest_mac(sid));
        assert_eq!(t.stream_id.0, sid.0);
        // And that destination must be a real stream address: the MAAP
        // dynamic pool, which IS the `91:E0:F0:00:xx:xx` block. This assertion
        // used to demand the opposite (`octets()[3] != 0x00`), which is what
        // kept the talker in the 1722.1-reserved block.
        assert!(avb_core::is_maap_dynamic(t.dest_mac));
    }

    #[test]
    fn connect_rx_names_the_16r_as_listener_and_us_by_eui64() {
        let mac = MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]);
        let our_id = Eui64::from_mac(mac);
        let the_16r = Eui64::from_u64(0x000a_92ff_fe03_3ba9);
        let sid = StreamId::from_parts(mac, 0);
        let pdu = connect_rx_pdu(our_id, the_16r, sid, 3, 7);
        // Apple's successful ACMP capture has stream_id=0 in every initial
        // CONNECT_RX_COMMAND; the listener supplies the real stream id later
        // in CONNECT_TX_RESPONSE / CONNECT_RX_RESPONSE.
        assert_eq!(pdu.stream_id, StreamId(0));
        assert_eq!(pdu.listener_entity_id, the_16r, "listener must be the 16R");
        assert_eq!(pdu.talker_entity_id, our_id);
        assert_eq!(pdu.controller_entity_id, our_id);
        // Our entity id carries the FF:FE EUI-64 insertion, like every other path.
        assert_eq!(&our_id.0[3..5], &[0xff, 0xfe]);
        assert_eq!(pdu.listener_unique_id, 3);
        assert_eq!(pdu.sequence_id, 7);
    }

    /// Our stream destination addresses must come from the MAAP dynamic pool.
    ///
    /// The test this replaces asserted only that the address was not the
    /// AVDECC multicast address and not inside the dext's AVDECC fallback
    /// block — an invariant the OLD `91:E0:F0:01:00:01` addressing satisfied
    /// while being, in fact, inside the 1722.1-reserved block and not a legal
    /// stream address at all. It pinned the guess instead of the requirement,
    /// so it passed for as long as the bug existed.
    #[test]
    fn stream_dest_mac_comes_from_the_maap_dynamic_pool() {
        let mac = MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]);
        for uid in 0u16..64 {
            let d = stream_dest_mac(StreamId::from_parts(mac, uid));
            assert!(
                avb_core::is_maap_dynamic(d),
                "uid {uid} -> {d} is not a MAAP dynamic pool address"
            );
            assert_ne!(d, avb_core::AVDECC_MULTICAST);
            // Specifically: never back in the block this used to use.
            assert_ne!(
                d.0[3], 0x01,
                "uid {uid} -> {d} is in the 1722.1-reserved block again"
            );
        }
    }

    /// The addressing we advertise over SRP must be the addressing we
    /// transmit to. A reservation for one address and frames to another is
    /// exactly the shape of failure that reserves bandwidth and never locks.
    #[test]
    fn the_tspec_advertises_the_address_we_transmit_to() {
        let mac = MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]);
        for uid in [0u16, 1, 7] {
            let sid = StreamId::from_parts(mac, uid);
            assert_eq!(talker_tspec(sid, 234).dest_mac, stream_dest_mac(sid));
        }
    }
}
