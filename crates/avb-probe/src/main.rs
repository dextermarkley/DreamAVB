//! avb-probe: Phase 0 hardware gates for the AVBBcmDriver dext.
//!
//!   info / regs           decode AvbInfo, read-only register dump
//!   clock                 G2a: EAV clock ticks, rate vs mach_absolute_time
//!   adjfreq               G2b: frequency correction sign and magnitude
//!   datapath up|down      Stage B bring-up / shutdown
//!   rx-ptp                G3: RX PTP latch on the 16R's gPTP event frames
//!   pdelay                G4: TX latch + peer-delay sanity
//!   all                   the whole sequence, with a JSON report
//!
//! None of these claims "works" by itself: the printed numbers are the
//! evidence the hardware-test step judges against plan section 6.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

use avb_probe::abi::{self, AvbInfo, ClockSample};
use avb_probe::iokit::Device;
use avb_probe::ptp::{self, MsgType, PortIdentity};
use avb_probe::stats::{self, ClockStats, Timebase};
use avb_probe::{freq, report};

#[derive(Parser)]
#[command(
    name = "avb-probe",
    about = "Phase 0 probe for the BCM57762 AVB dext (EAV clock, RX/TX PTP latches)"
)]
struct Cli {
    /// Append machine-readable results to this JSON file.
    #[arg(long, global = true)]
    json: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the decoded AvbInfo.
    Info,
    /// Read-only dump of the Phase 0 registers.
    Regs,
    /// G2a: sample the EAV clock and regress it against mach_absolute_time.
    Clock {
        #[arg(long, default_value_t = 10.0)]
        seconds: f64,
        #[arg(long, default_value_t = 100)]
        interval_ms: u64,
    },
    /// G2b: baseline / +P / -P / baseline windows; always restores 0 ppb.
    Adjfreq {
        #[arg(long, default_value_t = 100_000, allow_hyphen_values = true)]
        ppb: i64,
        #[arg(long, default_value_t = 10.0)]
        seconds: f64,
        #[arg(long, default_value_t = 100)]
        interval_ms: u64,
    },
    /// Bring the Stage B datapath up or down.
    Datapath {
        #[arg(value_parser = ["up", "down"])]
        state: String,
    },
    /// G3: print PTP frames (ethertype 0x88F7) with their RX hardware timestamps.
    RxPtp {
        #[arg(long, default_value_t = 10.0)]
        seconds: f64,
    },
    /// G4: send Pdelay_Req with TX timestamps and compute the link delay.
    Pdelay {
        #[arg(long, default_value_t = 5)]
        count: u32,
    },
    /// info, clock 10 s, adjfreq +-100000 ppb, datapath up, rx-ptp 10 s, pdelay 5, datapath down.
    All,
}

// ---------------------------------------------------------------------------
// Ctrl-C handling without extra crates: a plain SIGINT handler that sets a flag.

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_sig: i32) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

extern "C" {
    fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
}

const SIGINT: i32 = 2;

fn install_sigint() {
    unsafe {
        signal(SIGINT, on_sigint);
    }
}

fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Sleep in short slices so Ctrl-C is noticed promptly.
fn nap(d: Duration) {
    let end = Instant::now() + d;
    while !interrupted() {
        let now = Instant::now();
        if now >= end {
            break;
        }
        sleep((end - now).min(Duration::from_millis(20)));
    }
}

fn emit<T: Serialize>(json: Option<&Path>, key: &str, rec: &T) -> Result<()> {
    if let Some(p) = json {
        report::append(p, key, rec)?;
    }
    Ok(())
}

fn timebase(info: &AvbInfo) -> Timebase {
    if info.tb_denom != 0 {
        return Timebase {
            numer: info.tb_numer,
            denom: info.tb_denom,
        };
    }
    let mut tb = mach2::mach_time::mach_timebase_info { numer: 0, denom: 0 };
    unsafe {
        mach2::mach_time::mach_timebase_info(&mut tb);
    }
    Timebase {
        numer: tb.numer,
        denom: tb.denom,
    }
}

fn fmt_mac(m: &[u8; 6]) -> String {
    m.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

// ---------------------------------------------------------------------------
// info / regs

#[derive(Serialize)]
struct InfoRecord {
    vendor_device: String,
    subsys: String,
    asic_rev: String,
    misc_host_ctrl: String,
    grc_mode: String,
    time_sync_enable: bool,
    stage: String,
    fault: String,
    flags: u32,
    ape: bool,
    asf: bool,
    link_up: bool,
    gbe_full_duplex: bool,
    sram_signature_ok: bool,
    mac: String,
    timebase: Timebase,
}

fn cmd_info(dev: &Device, json: Option<&Path>) -> Result<AvbInfo> {
    let i = dev.info()?;
    let rec = InfoRecord {
        vendor_device: format!("0x{:08x}", i.vendor_device),
        subsys: format!("0x{:08x}", i.subsys),
        asic_rev: format!("0x{:08x}", i.asic_rev),
        misc_host_ctrl: format!("0x{:08x}", i.misc_host_ctrl),
        grc_mode: format!("0x{:08x}", i.grc_mode),
        time_sync_enable: i.grc_mode & 0x0008_0000 != 0,
        stage: abi::stage_name(i.stage).into(),
        fault: format!("{} ({})", i.fault, abi::fault_name(i.fault)),
        flags: i.flags,
        ape: i.flags & abi::FLAG_APE != 0,
        asf: i.flags & abi::FLAG_ASF != 0,
        link_up: i.flags & abi::FLAG_LINK_UP != 0,
        gbe_full_duplex: i.flags & abi::FLAG_1000FD != 0,
        sram_signature_ok: i.flags & abi::FLAG_SIG_OK != 0,
        mac: fmt_mac(&i.mac),
        timebase: timebase(&i),
    };
    println!(
        "vendor:device  {}   subsystem {}",
        rec.vendor_device, rec.subsys
    );
    println!(
        "asic_rev       {}   (57766-class: {})",
        rec.asic_rev,
        (i.asic_rev >> 12) == 0x57766
    );
    println!(
        "misc_host_ctrl {}   grc_mode {} (TIME_SYNC_ENABLE {})",
        rec.misc_host_ctrl, rec.grc_mode, rec.time_sync_enable
    );
    println!("stage          {}   fault {}", rec.stage, rec.fault);
    println!(
        "flags          0x{:x}  ape={} asf={} link_up={} 1000FD={} sram_sig={}",
        rec.flags, rec.ape, rec.asf, rec.link_up, rec.gbe_full_duplex, rec.sram_signature_ok
    );
    println!(
        "mac            {}   timebase {}/{}",
        rec.mac, rec.timebase.numer, rec.timebase.denom
    );
    emit(json, "info", &rec)?;
    Ok(i)
}

const REG_DUMP: &[(&str, u32)] = &[
    ("MISC_HOST_CTRL", 0x0068),
    ("GRC_MODE", 0x6800),
    ("EAV_REF_CLCK_CTL", 0x6908),
    ("EAV_REF_CLCK_LSB", 0x6900),
    ("EAV_REF_CLCK_MSB", 0x6904),
    ("EAV_REF_CLK_CORRECT_CTL", 0x6928),
    ("RX_PTP_CTL", 0x06c8),
    ("TX_TSTAMP_LSB", 0x05c0),
    ("TX_TSTAMP_MSB", 0x05c4),
    ("RX_TSTAMP_LSB", 0x06b0),
    ("RX_TSTAMP_MSB", 0x06b4),
    ("MAC_MODE", 0x0400),
    ("MAC_STATUS", 0x0404),
];

fn cmd_regs(dev: &Device, json: Option<&Path>) -> Result<()> {
    let mut rec = serde_json::Map::new();
    for (name, off) in REG_DUMP {
        let v = dev.read_reg32(*off)?;
        println!("{name:<24} 0x{off:04x} = 0x{v:08x}");
        rec.insert((*name).into(), format!("0x{v:08x}").into());
    }
    emit(json, "regs", &rec)
}

// ---------------------------------------------------------------------------
// G2a / G2b

/// Collect one sample per interval (the tightest of 4 back-to-back reads).
fn sample_clock(dev: &Device, seconds: f64, interval_ms: u64) -> Result<Vec<ClockSample>> {
    let end = Instant::now() + Duration::from_secs_f64(seconds);
    let mut out = Vec::new();
    while Instant::now() < end && !interrupted() {
        let batch = dev.read_clock(4)?;
        let best = batch
            .iter()
            .min_by_key(|s| s.mach_after.saturating_sub(s.mach_before))
            .copied()
            .expect("read_clock returns >= 1 sample");
        out.push(best);
        nap(Duration::from_millis(interval_ms));
    }
    Ok(out)
}

fn print_clock_stats(label: &str, s: &ClockStats) {
    match s.fit {
        Some(f) => println!(
            "{label}: n={} slope={:.9} ({:+.3} ppm vs mach) residual_rms={:.1} ns monotonic={} wraps32={} bracket median={:.0} ns max={:.0} ns",
            s.samples,
            f.slope,
            f.ppm(),
            f.residual_rms,
            s.monotonic,
            s.wraps32,
            s.bracket_median_ns,
            s.bracket_max_ns
        ),
        None => println!("{label}: n={} (not enough samples to fit) monotonic={}", s.samples, s.monotonic),
    }
}

#[derive(Serialize)]
struct ClockRecord {
    seconds: f64,
    interval_ms: u64,
    stats: ClockStats,
    first: Option<ClockSample>,
    last: Option<ClockSample>,
    /// Every raw sample, so hardware behaviour (e.g. counter width) can be
    /// analysed offline.
    raw: Vec<ClockSample>,
}

fn cmd_clock(
    dev: &Device,
    json: Option<&Path>,
    seconds: f64,
    interval_ms: u64,
) -> Result<ClockStats> {
    let info = dev.info()?;
    let tb = timebase(&info);
    let samples = sample_clock(dev, seconds, interval_ms)?;
    let s = stats::clock_stats(&samples, tb);
    print_clock_stats("clock", &s);
    if let (Some(a), Some(b)) = (samples.first(), samples.last()) {
        println!(
            "eav advanced {} ns over {:.0} ns of host time",
            b.eav_ns as i128 - a.eav_ns as i128,
            tb.mach_delta_ns(a.mach_before, b.mach_before)
        );
    }
    emit(
        json,
        "clock",
        &ClockRecord {
            seconds,
            interval_ms,
            stats: s.clone(),
            first: samples.first().copied(),
            last: samples.last().copied(),
            raw: samples.clone(),
        },
    )?;
    Ok(s)
}

#[derive(Serialize)]
struct WindowRecord {
    label: String,
    requested_ppb: i64,
    register: String,
    expected_ppb: f64,
    measured_ppb_vs_mach: Option<f64>,
    delta_vs_baseline_ppb: Option<f64>,
    scale_factor: Option<f64>,
    stats: ClockStats,
}

fn cmd_adjfreq(
    dev: &Device,
    json: Option<&Path>,
    ppb: i64,
    seconds: f64,
    interval_ms: u64,
) -> Result<()> {
    if ppb.abs() > abi::ADJ_FREQ_MAX_PPB {
        bail!("|ppb| must be <= {}", abi::ADJ_FREQ_MAX_PPB);
    }
    let info = dev.info()?;
    let tb = timebase(&info);
    let plan = [
        ("baseline-0", 0i64),
        ("plus", ppb),
        ("minus", -ppb),
        ("baseline-1", 0),
    ];
    let mut windows = Vec::new();
    let mut baseline: Option<f64> = None;
    let result = (|| -> Result<()> {
        for (label, p) in plan {
            if interrupted() {
                break;
            }
            let reg = dev.adj_freq(p)?;
            if reg != freq::correction_from_ppb(p) {
                println!(
                    "warning: dext wrote 0x{reg:08x}, probe expected 0x{:08x}",
                    freq::correction_from_ppb(p)
                );
            }
            let s = stats::clock_stats(&sample_clock(dev, seconds, interval_ms)?, tb);
            print_clock_stats(label, &s);
            let measured = s.fit.map(|f| f.ppb());
            if p == 0 && baseline.is_none() {
                baseline = measured;
            }
            let expected = freq::expected_rate_ppb(reg);
            let delta = match (measured, baseline) {
                (Some(m), Some(b)) => Some(m - b),
                _ => None,
            };
            let scale = delta
                .map(|d| freq::scale_factor(d, expected))
                .filter(|x| x.is_finite());
            if p != 0 {
                println!(
                    "  {label}: requested {p:+} ppb reg 0x{reg:08x} expected {expected:+.1} ppb measured delta {} scale {}",
                    delta.map_or("n/a".into(), |d| format!("{d:+.1} ppb")),
                    scale.map_or("n/a".into(), |x| format!("{x:.4}"))
                );
            } else if let (Some(d), true) = (delta, label == "baseline-1") {
                println!("  baseline drift between windows: {d:+.1} ppb");
            }
            windows.push(WindowRecord {
                label: label.into(),
                requested_ppb: p,
                register: format!("0x{reg:08x}"),
                expected_ppb: expected,
                measured_ppb_vs_mach: measured,
                delta_vs_baseline_ppb: delta,
                scale_factor: scale,
                stats: s,
            });
        }
        Ok(())
    })();
    // Always restore zero correction, whatever happened above (including Ctrl-C).
    let restore = dev.adj_freq(0);
    match &restore {
        Ok(r) => println!("restored frequency correction to 0 (register 0x{r:08x})"),
        Err(e) => eprintln!("FAILED to restore frequency correction: {e:#}"),
    }
    emit(json, "adjfreq", &windows)?;
    result?;
    restore?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Datapath

fn cmd_datapath(dev: &Device, json: Option<&Path>, up: bool) -> Result<()> {
    if up {
        let stage = dev.init_datapath(abi::INIT_FLAG_PROMISC | abi::INIT_FLAG_RX_PTP_V2_L2)?;
        let info = dev.info()?;
        println!(
            "datapath up: stage {} link_up={} 1000FD={}",
            abi::stage_name(stage),
            info.flags & abi::FLAG_LINK_UP != 0,
            info.flags & abi::FLAG_1000FD != 0
        );
        emit(
            json,
            "datapath",
            &serde_json::json!({"state": "up", "stage": stage, "flags": info.flags}),
        )
    } else {
        dev.shutdown_datapath()?;
        println!("datapath down");
        emit(json, "datapath", &serde_json::json!({"state": "down"}))
    }
}

// ---------------------------------------------------------------------------
// G3

#[derive(Serialize)]
struct RxPtpFrame {
    msg_type: String,
    seq: u16,
    source: String,
    has_ts: bool,
    hw_ts: u64,
    harvest_mach: u64,
    len: u32,
}

#[derive(Serialize)]
struct RxPtpRecord {
    seconds: f64,
    frames: Vec<RxPtpFrame>,
    dropped: u64,
    pdelay_req_count: usize,
    pdelay_req_with_ts: usize,
    repeated_hw_ts: usize,
    /// |delta(hw_ts) - delta(harvest)| for successive Pdelay_Req, ns.
    interval_mismatch_ns: Vec<f64>,
    max_interval_mismatch_ns: Option<f64>,
}

fn cmd_rx_ptp(dev: &Device, json: Option<&Path>, seconds: f64) -> Result<()> {
    let info = dev.info()?;
    let tb = timebase(&info);
    if info.stage != abi::STAGE_DATAPATH {
        bail!(
            "datapath is not up (stage {}); run `avb-probe datapath up` first",
            abi::stage_name(info.stage)
        );
    }
    let end = Instant::now() + Duration::from_secs_f64(seconds);
    let mut frames = Vec::new();
    let mut dropped = 0u64;
    let mut last_req: Option<(u64, u64)> = None;
    let mut mismatch = Vec::new();
    let mut last_ts: Option<u64> = None;
    let mut repeated = 0;
    let (mut req_count, mut req_ts) = (0, 0);
    while Instant::now() < end && !interrupted() {
        let batch = dev.poll_rx(ptp::ETHERTYPE_PTP, abi::RX_BATCH_MAX)?;
        dropped += u64::from(batch.dropped);
        for e in batch.entries() {
            let Some(m) = ptp::parse_frame(e.bytes()) else {
                continue;
            };
            let has_ts = e.has_ts != 0;
            println!(
                "{:<20} seq={:<5} src={} type_flags=0x{:04x} has_ts={} hw_ts={} harvest_mach={}",
                format!("{:?}", m.msg_type),
                m.seq,
                m.source,
                e.type_flags,
                u8::from(has_ts),
                e.hw_ts,
                e.harvest_mach
            );
            if has_ts {
                if last_ts == Some(e.hw_ts) {
                    repeated += 1;
                    println!("  WARNING: hw_ts identical to previous latched value (interlock not released?)");
                }
                last_ts = Some(e.hw_ts);
            }
            if m.msg_type == MsgType::PdelayReq {
                req_count += 1;
                if has_ts {
                    req_ts += 1;
                    if let Some((pts, pmach)) = last_req {
                        let d_hw = e.hw_ts as f64 - pts as f64;
                        let d_host = tb.mach_delta_ns(pmach, e.harvest_mach);
                        let mm = (d_hw - d_host).abs();
                        println!("  Pdelay_Req interval: hw {d_hw:.0} ns, harvest {d_host:.0} ns, |diff| {mm:.0} ns");
                        mismatch.push(mm);
                    }
                    last_req = Some((e.hw_ts, e.harvest_mach));
                }
            }
            frames.push(RxPtpFrame {
                msg_type: format!("{:?}", m.msg_type),
                seq: m.seq,
                source: m.source.to_string(),
                has_ts,
                hw_ts: e.hw_ts,
                harvest_mach: e.harvest_mach,
                len: e.len,
            });
        }
        nap(Duration::from_millis(5));
    }
    let max_mm = mismatch.iter().copied().reduce(f64::max);
    println!(
        "summary: {} PTP frames, {req_count} Pdelay_Req ({req_ts} with ts), repeated ts {repeated}, dropped {dropped}, max interval mismatch {}",
        frames.len(),
        max_mm.map_or("n/a".into(), |m| format!("{:.3} ms", m / 1e6))
    );
    emit(
        json,
        "rx_ptp",
        &RxPtpRecord {
            seconds,
            frames,
            dropped,
            pdelay_req_count: req_count,
            pdelay_req_with_ts: req_ts,
            repeated_hw_ts: repeated,
            interval_mismatch_ns: mismatch,
            max_interval_mismatch_ns: max_mm,
        },
    )
}

// ---------------------------------------------------------------------------
// G4

#[derive(Serialize, Default)]
struct PdelayExchange {
    seq: u16,
    tx_status: String,
    t1: Option<u64>,
    /// EAV estimate at the doorbell and at the next clock sample after the latch.
    eav_at_doorbell: Option<u64>,
    eav_next_sample: Option<u64>,
    t1_bracketed: Option<bool>,
    t2: Option<u64>,
    t3: Option<u64>,
    t4: Option<u64>,
    link_delay_ns: Option<f64>,
}

#[derive(Serialize)]
struct PdelayRecord {
    exchanges: Vec<PdelayExchange>,
    tx_ok: usize,
    median_link_delay_ns: Option<f64>,
}

/// EAV value at host time `mach`, extrapolated from a nearby sample at rate 1.
fn eav_at(sample: &ClockSample, mach: u64, tb: Timebase) -> u64 {
    let mid = stats::midpoint(sample.mach_before, sample.mach_after);
    (sample.eav_ns as f64 + tb.mach_delta_ns(mid, mach)).max(0.0) as u64
}

fn cmd_pdelay(dev: &Device, json: Option<&Path>, count: u32) -> Result<()> {
    let info = dev.info()?;
    let tb = timebase(&info);
    if info.stage != abi::STAGE_DATAPATH {
        bail!(
            "datapath is not up (stage {}); run `avb-probe datapath up` first",
            abi::stage_name(info.stage)
        );
    }
    let me = PortIdentity::from_mac(info.mac, 1);
    let mut exchanges = Vec::new();
    for i in 0..count {
        if interrupted() {
            break;
        }
        let seq = 0x4000u16.wrapping_add(i as u16);
        let mut x = PdelayExchange {
            seq,
            ..Default::default()
        };
        // Drain stale frames so we only match fresh responses.
        let _ = dev.poll_rx(ptp::ETHERTYPE_PTP, abi::RX_BATCH_MAX)?;
        let before = dev.read_clock(1)?[0];
        let (tx_seq, mach_doorbell) =
            dev.send_frame(&ptp::build_pdelay_req(info.mac, seq), true)?;
        // Poll the TX latch (the dext times out after 300 ms).
        let deadline = Instant::now() + Duration::from_millis(500);
        let (mut status, mut hw) = (abi::TX_TS_PENDING, 0);
        while Instant::now() < deadline {
            let (s, t, _) = dev.get_tx_timestamp(tx_seq)?;
            status = s;
            hw = t;
            if s != abi::TX_TS_PENDING {
                break;
            }
            sleep(Duration::from_millis(2));
        }
        x.tx_status = abi::tx_status_name(status).into();
        if status == abi::TX_TS_OK {
            let after = dev.read_clock(1)?[0];
            let lo = eav_at(&before, mach_doorbell, tb);
            let hi = after.eav_ns;
            x.t1 = Some(hw);
            x.eav_at_doorbell = Some(lo);
            x.eav_next_sample = Some(hi);
            x.t1_bracketed = Some(stats::in_bracket(lo, hw, hi));
        }
        // Collect Pdelay_Resp (t2, t4) and Pdelay_Resp_Follow_Up (t3).
        let mut corr = (0.0, 0.0);
        let deadline = Instant::now() + Duration::from_millis(1000);
        while Instant::now() < deadline && (x.t3.is_none() || x.t4.is_none()) && !interrupted() {
            let batch = dev.poll_rx(ptp::ETHERTYPE_PTP, abi::RX_BATCH_MAX)?;
            for e in batch.entries() {
                let Some(m) = ptp::parse_frame(e.bytes()) else {
                    continue;
                };
                if m.seq != seq || m.requesting != Some(me) {
                    continue;
                }
                match m.msg_type {
                    MsgType::PdelayResp => {
                        x.t2 = m.body_ts_ns;
                        corr.0 = m.correction_ns();
                        if e.has_ts != 0 {
                            x.t4 = Some(e.hw_ts);
                        }
                    }
                    MsgType::PdelayRespFollowUp => {
                        x.t3 = m.body_ts_ns;
                        corr.1 = m.correction_ns();
                    }
                    _ => {}
                }
            }
            sleep(Duration::from_millis(2));
        }
        if let (Some(t1), Some(t2), Some(t3), Some(t4)) = (x.t1, x.t2, x.t3, x.t4) {
            x.link_delay_ns = Some(ptp::link_delay_ns(t1, t2, t3, t4, corr.0, corr.1));
        }
        println!(
            "seq={seq:#06x} tx={} t1={} bracketed={} t2={} t3={} t4={} link_delay={}",
            x.tx_status,
            x.t1.map_or("-".into(), |v| v.to_string()),
            x.t1_bracketed.map_or("-".into(), |v| v.to_string()),
            x.t2.map_or("-".into(), |v| v.to_string()),
            x.t3.map_or("-".into(), |v| v.to_string()),
            x.t4.map_or("-".into(), |v| v.to_string()),
            x.link_delay_ns.map_or("-".into(), |v| format!("{v:.1} ns"))
        );
        exchanges.push(x);
        nap(Duration::from_millis(1000));
    }
    let delays: Vec<f64> = exchanges.iter().filter_map(|x| x.link_delay_ns).collect();
    let median = (!delays.is_empty()).then(|| stats::median(&delays));
    let tx_ok = exchanges.iter().filter(|x| x.t1.is_some()).count();
    println!(
        "summary: {tx_ok}/{} TX timestamps ok, median link delay {}",
        exchanges.len(),
        median.map_or("n/a (no complete exchanges)".into(), |m| format!(
            "{m:.1} ns"
        ))
    );
    emit(
        json,
        "pdelay",
        &PdelayRecord {
            exchanges,
            tx_ok,
            median_link_delay_ns: median,
        },
    )
}

// ---------------------------------------------------------------------------

fn cmd_all(dev: &Device, json: Option<&Path>) -> Result<()> {
    cmd_info(dev, json)?;
    cmd_clock(dev, json, 10.0, 100)?;
    cmd_adjfreq(dev, json, 100_000, 10.0, 100)?;
    cmd_datapath(dev, json, true)?;
    let r = cmd_rx_ptp(dev, json, 10.0).and_then(|_| cmd_pdelay(dev, json, 5));
    // Always try to take the datapath down again.
    let down = cmd_datapath(dev, json, false);
    r?;
    down
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    install_sigint();
    let dev = Device::open()?;
    let json = cli.json.as_deref();
    match cli.cmd {
        Cmd::Info => cmd_info(&dev, json).map(|_| ()),
        Cmd::Regs => cmd_regs(&dev, json),
        Cmd::Clock {
            seconds,
            interval_ms,
        } => cmd_clock(&dev, json, seconds, interval_ms).map(|_| ()),
        Cmd::Adjfreq {
            ppb,
            seconds,
            interval_ms,
        } => cmd_adjfreq(&dev, json, ppb, seconds, interval_ms),
        Cmd::Datapath { state } => cmd_datapath(&dev, json, state == "up"),
        Cmd::RxPtp { seconds } => cmd_rx_ptp(&dev, json, seconds),
        Cmd::Pdelay { count } => cmd_pdelay(&dev, json, count),
        Cmd::All => cmd_all(&dev, json),
    }
}
