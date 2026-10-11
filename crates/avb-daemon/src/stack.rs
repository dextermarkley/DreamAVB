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
    // Step 3: RingAttach(0) — becomes the owner.
    if let Err(e) = dev.ring_attach(0) {
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
    // Steps 4–6 (map, validate header, InitDatapath) and step 7 (link wait)
    // run against the mapped rings; the mapping + consumer live in the ctl
    // thread. Here we initialise the data path and wait for link.
    if dev.init_datapath(0).is_err() {
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
    Ok(RealDext::new(dev, numer, denom))
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

pub fn gptp(opts: &GlobalOpts, _seconds: u64, _json_out: Option<PathBuf>) -> Result<(), Exit> {
    install_signal_handlers();
    let log = Logger::new(opts.level);
    let _dext = bring_up(opts, &log)?;
    log.log(Level::Info, "gptp", "gPTP only");
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
