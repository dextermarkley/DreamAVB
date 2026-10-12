// SPDX-License-Identifier: MIT OR Apache-2.0
//! DreamAVB companion — Tauri backend.
//!
//! The brains live here in Rust (so a future Linux/Windows build reuses them):
//! enumerate NICs, hand the chosen one to the DreamAVB driver or give it back
//! to the OS default driver, and poll live stats. All privileged actions shell
//! out to the existing, entitlement-carrying `avb-installer` / `avb-probe`
//! binaries rather than re-implementing the system-extension flow in the GUI.
//!
//! Default policy (RULE): the driver stays HANDS-OFF until the user explicitly
//! enables it. On macOS "hands-off" = the system extension is deactivated, so
//! Apple's generic AppleEthernetE1000 driver owns the NIC as a normal
//! interface. "Enable" = activate the dext, which then wins the PCI match.

use std::path::{Path, PathBuf};
use std::process::Command;

use avb_config::{ClockSource, Config, EffectiveClock};
use serde::{Deserialize, Serialize};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager,
};

/// A detected network interface the user might hand to DreamAVB.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Nic {
    /// Interface name, e.g. "en7".
    pub bsd_name: String,
    /// Human label, e.g. "Intel I210 (Thunderbolt)".
    pub display: String,
    /// PCI id string, e.g. "8086:1533 / 1ab6:0214".
    pub pci_id: String,
    /// True if the DreamAVB driver supports this NIC today (only the I210).
    pub supported: bool,
}

/// Whether the DreamAVB driver currently owns a NIC, or the OS default does.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DriverState {
    /// Hands-off: the OS default driver owns the NIC (dext deactivated).
    DefaultDriver,
    /// Activating / waiting on approval.
    Pending,
    /// DreamAVB owns the NIC and the stack is running.
    Active,
    /// A fault was reported by the dext.
    Fault,
}

/// The snapshot the UI renders.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub state: DriverState,
    pub nic: Option<Nic>,
    /// One-line human status, e.g. "Default driver in control".
    pub summary: String,
}

/// Live stats pulled from the shared ring + `avb-probe` (all read-only).
///
/// Field names say exactly what is measured. The old `ring_underruns` read the
/// ring's CONSUMER underrun counter, which only the CoreAudio plug-in touches,
/// so it never meant anything in this process (review H10); `as_capable` was
/// really the media clock's lock flag, not gPTP asCapable.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Stats {
    /// Media clock locked (the ring ClockBlock's `locked`).
    pub clock_locked: bool,
    /// Drift of the media clock against nominal, ppm.
    pub media_drift_ppm: f64,
    /// Committed frames per second at the ring's write frontier.
    pub stream_frames_per_s: u64,
    /// Frames lost because the consumer fell a full ring behind.
    pub overrun_frames: u64,
    /// Frames the producer had to zero-fill (missing/late packets).
    pub zero_fill_frames: u64,
    pub rx_frames: u64,
    pub hw_rev: String,
    pub abi_version: u32,
}

/// Resolve a bundled/sibling helper binary (avb-probe).
///
/// Looked up next to our own binary, then inside a sibling AVBInstaller.app,
/// then relative to this crate's source tree for a dev run. No absolute
/// machine-specific path (review M7) — a hardcoded /Users/<me> fallback broke
/// the app for every other checkout.
fn helper_path(name: &str) -> PathBuf {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join(name));
            // …/build/AVBInstaller.app/Contents/MacOS/<name>
            cands.push(
                dir.join("AVBInstaller.app")
                    .join("Contents/MacOS")
                    .join(name),
            );
        }
    }
    // Dev run: this crate is apps/dreamavb-companion/src-tauri, so the
    // workspace root is three levels up.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .to_path_buf();
    cands.push(root.join("target/release").join(name));
    cands.push(
        root.join("build/AVBInstaller.app/Contents/MacOS")
            .join(name),
    );
    cands
        .iter()
        .find(|p| p.exists())
        .cloned()
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Locate AVBInstaller.app, which must be launched AS ITSELF to activate the
/// system extension.
fn installer_app() -> Option<PathBuf> {
    let mut cands = vec![PathBuf::from("/Applications/AVBInstaller.app")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("AVBInstaller.app"));
        }
    }
    cands.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../build/AVBInstaller.app")
            .to_path_buf(),
    );
    cands.into_iter().find(|p| p.exists())
}

/// Run AVBInstaller.app with an argument, as its own app bundle.
///
/// `avb-installer` checks `NSBundle::mainBundle().bundleIdentifier()` against
/// com.avbaudio.installer and refuses to run otherwise, and macOS only honours
/// an OSSystemExtensionRequest from the app bundle that CONTAINS the dext. So
/// spawning the helper binary as a child of this app always failed (review H9);
/// `open -W -a <app> --args <action>` launches it as itself.
fn run_installer(action: &str) -> Result<(), String> {
    let app = installer_app().ok_or_else(|| {
        "AVBInstaller.app not found — install it to /Applications, then try again".to_string()
    })?;
    let out = Command::new("/usr/bin/open")
        .arg("-W")
        .arg("-a")
        .arg(&app)
        .arg("--args")
        .arg(action)
        .output()
        .map_err(|e| format!("could not launch AVBInstaller.app: {e}"))?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if msg.is_empty() {
            format!("AVBInstaller.app {action} failed")
        } else {
            msg
        });
    }
    Ok(())
}

/// Enumerate candidate NICs. Read-only: `system_profiler SPEthernetDataType`
/// gives each port's real PCI vendor/device/subsystem ids and BSD name, so the
/// I210 is matched on identity rather than a formatted ioreg substring, and we
/// never invent a BSD name (review M5). Everything that is not the I210 is
/// listed UNSUPPORTED and, per project policy, never driven.
#[tauri::command]
fn list_nics() -> Vec<Nic> {
    let mut nics = Vec::new();
    for p in ethernet_ports() {
        let supported = p.is_i210();
        let pci_id = match (&p.vendor, &p.device) {
            (Some(v), Some(d)) => {
                let base = format!("{}:{}", strip0x(v), strip0x(d));
                match (&p.sub_vendor, &p.sub_device) {
                    (Some(sv), Some(sd)) => {
                        format!("{base} / {}:{}", strip0x(sv), strip0x(sd))
                    }
                    _ => base,
                }
            }
            _ => "—".into(),
        };
        nics.push(Nic {
            bsd_name: p.bsd.clone().unwrap_or_default(),
            display: if supported {
                "Intel I210 (Thunderbolt)".into()
            } else {
                p.name.clone()
            },
            pci_id,
            supported,
        });
    }
    // Our dext owning the I210 removes its enX interface, so system_profiler
    // may not list it. Surface it from ioreg so the UI still shows the device.
    if !nics.iter().any(|n| n.supported) && dext_owns_i210() {
        nics.insert(
            0,
            Nic {
                bsd_name: String::new(),
                display: "Intel I210 (Thunderbolt)".into(),
                pci_id: "8086:1533 / 1ab6:0214".into(),
                supported: true,
            },
        );
    }
    nics
}

fn strip0x(s: &str) -> &str {
    s.strip_prefix("0x").unwrap_or(s)
}

/// One Ethernet port as reported by `system_profiler`.
#[derive(Clone, Debug, Default)]
struct EthPort {
    name: String,
    bsd: Option<String>,
    vendor: Option<String>,
    device: Option<String>,
    sub_vendor: Option<String>,
    sub_device: Option<String>,
}

impl EthPort {
    /// The Intel I210 DreamAVB drives: 8086:1533, subsystem 1ab6:0214.
    fn is_i210(&self) -> bool {
        let eq = |o: &Option<String>, want: &str| {
            o.as_deref()
                .map(|v| strip0x(v).eq_ignore_ascii_case(want))
                .unwrap_or(false)
        };
        eq(&self.vendor, "8086")
            && eq(&self.device, "1533")
            && (self.sub_vendor.is_none() || eq(&self.sub_vendor, "1ab6"))
    }
}

/// Parse `system_profiler SPEthernetDataType -json` into ports.
fn ethernet_ports() -> Vec<EthPort> {
    let out = match Command::new("/usr/sbin/system_profiler")
        .args(["SPEthernetDataType", "-json"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return Vec::new();
    };
    let Some(items) = v.get("SPEthernetDataType").and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    let s = |e: &serde_json::Value, k: &str| {
        e.get(k)
            .and_then(|x| x.as_str())
            .map(std::string::ToString::to_string)
    };
    items
        .iter()
        .map(|e| EthPort {
            name: s(e, "_name").unwrap_or_else(|| "Ethernet".into()),
            bsd: s(e, "spethernet_BSD_Device_Name"),
            vendor: s(e, "spethernet_vendor-id"),
            device: s(e, "spethernet_device-id"),
            sub_vendor: s(e, "spethernet_subsystem-vendor-id"),
            sub_device: s(e, "spethernet_subsystem-id"),
        })
        .collect()
}

/// True if OUR dext is the registered driver for the I210 (so the device has
/// no enX interface). Narrow `ioreg` query, not a whole-registry dump.
fn dext_owns_i210() -> bool {
    // `-rn <name>` only. Combining `-c IOUserService` with `-n AVBI210Driver`
    // returns NOTHING even when the dext is attached and driving the device
    // (verified against a live I210 whose dext answered avb-probe): ioreg does
    // not intersect the two filters the way it reads like it should, so the
    // class filter silently discarded the name match and this reported that the
    // default driver owned the NIC while ours actually did.
    Command::new("/usr/sbin/ioreg")
        .args(["-rn", "AVBI210Driver"])
        .output()
        .ok()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            // Require the bundle id too, so a stray name match elsewhere in the
            // registry cannot be mistaken for our attached service.
            s.contains("AVBI210Driver") && s.contains("com.avbaudio.installer.i210")
        })
        .unwrap_or(false)
}

/// Enable DreamAVB on a NIC: activate the system extension so our dext claims
/// the device. Only the I210 is supported, so the selected NIC is validated
/// rather than silently ignored.
///
/// `async` so Tauri runs it off the main thread (review M3): it launches
/// AVBInstaller.app and waits for it.
#[tauri::command]
async fn enable_driver(bsd_name: String) -> Result<Status, String> {
    let supported = list_nics()
        .into_iter()
        .any(|n| n.supported && n.bsd_name == bsd_name);
    if !supported {
        return Err(format!(
            "{bsd_name} is not a DreamAVB-supported interface (only the Intel I210 is)"
        ));
    }
    run_installer("activate")?;
    Ok(current_status())
}

/// Disable DreamAVB: deactivate the dext so the OS default driver reclaims the
/// NIC. This is the "give control back" action.
#[tauri::command]
async fn disable_driver() -> Result<Status, String> {
    run_installer("deactivate")?;
    Ok(current_status())
}

/// Current driver ownership state (reads `systemextensionsctl` + a probe).
#[tauri::command]
async fn status() -> Status {
    current_status()
}

/// How the I210 dext appears in `systemextensionsctl list`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DextSysextState {
    /// Not installed / deactivated — the default driver owns the NIC.
    Absent,
    /// Activated but not yet enabled/matched — mid-approval.
    Activating,
    /// Activated and enabled — our dext is in control.
    Enabled,
}

/// Read the I210 dext's state from `systemextensionsctl list`.
fn dext_sysext_state() -> DextSysextState {
    let out = Command::new("/usr/bin/systemextensionsctl")
        .arg("list")
        .output();
    match out {
        Ok(o) => classify_sysext(&String::from_utf8_lossy(&o.stdout)),
        Err(_) => DextSysextState::Absent,
    }
}

/// Classify the dext's records in `systemextensionsctl list` output.
///
/// The list can hold SEVERAL records for one bundle id (an old version waiting
/// to uninstall beside a new one awaiting approval). Taking the first line
/// containing `[activated enabled]` could therefore report Active off a stale
/// version while the current one is still pending (review M4). A terminated /
/// uninstalling record is ignored entirely, and the pending state only wins
/// when NO live record is enabled. Pure, so it is unit-testable.
fn classify_sysext(text: &str) -> DextSysextState {
    let mut pending = false;
    for line in text
        .lines()
        .filter(|l| l.contains("com.avbaudio.installer.i210"))
    {
        let l = line.to_lowercase();
        // A record on its way out says nothing about who owns the NIC now.
        if l.contains("terminated") || l.contains("uninstall") {
            continue;
        }
        if l.contains("[activated enabled]") {
            return DextSysextState::Enabled;
        }
        if l.contains("activated") || l.contains("waiting") {
            pending = true;
        }
    }
    if pending {
        DextSysextState::Activating
    } else {
        DextSysextState::Absent
    }
}

fn current_status() -> Status {
    // Hands-off is the safe default: only a positive signal flips us to Active,
    // so a read failure never falsely claims the driver owns the NIC.
    let state = dext_sysext_state();
    let faulted = state == DextSysextState::Enabled && probe_fault();
    let nic = if state == DextSysextState::Enabled {
        list_nics().into_iter().find(|n| n.supported)
    } else {
        None
    };
    status_for(state, faulted, nic)
}

/// Pure mapping from (sysext state, fault, detected NIC) to the UI Status.
/// Separated from the system reads so the policy — Absent is always hands-off
/// — is unit-testable without depending on this machine's actual dext state.
/// The NIC is passed in from real detection; it used to be a hardcoded "en7"
/// (review M5), which is both machine-specific and wrong once our dext owns
/// the device (the enX interface is gone).
fn status_for(state: DextSysextState, faulted: bool, nic: Option<Nic>) -> Status {
    match state {
        DextSysextState::Enabled => {
            if faulted {
                Status {
                    state: DriverState::Fault,
                    nic,
                    summary: "DreamAVB driver reported a fault".into(),
                }
            } else {
                Status {
                    state: DriverState::Active,
                    nic,
                    summary: "DreamAVB active on the I210".into(),
                }
            }
        }
        DextSysextState::Activating => Status {
            state: DriverState::Pending,
            nic: None,
            summary: "Activating — waiting for approval".into(),
        },
        DextSysextState::Absent => Status {
            state: DriverState::DefaultDriver,
            nic: None,
            summary: "Default driver in control".into(),
        },
    }
}

/// Parse the dext fault CODE out of `avb-probe info` stdout.
///
/// The probe ALWAYS prints the word "fault", because `fault` is the field
/// LABEL and the code is its value — a healthy I210 prints
/// `stage          timesync   fault 0 (none)`. So a substring test for
/// "fault" is true for every successful probe, which is exactly what this
/// used to do: the UI reported a permanent fault on healthy hardware and
/// could never show Active. Compare the CODE to zero instead.
///
/// `None` means no `fault <code>` field was found, which is UNKNOWN rather
/// than healthy; the caller keeps the established policy of never asserting
/// a fault without positive evidence.
fn parse_fault_code(stdout: &str) -> Option<u32> {
    stdout.lines().find_map(|line| {
        line.split("fault ")
            .nth(1)?
            .split_whitespace()
            .next()?
            .parse::<u32>()
            .ok()
    })
}

/// True if `avb-probe info` reports a dext fault (non-zero exit, or a
/// non-zero fault code in its output). Read-only.
fn probe_fault() -> bool {
    match Command::new(helper_path("avb-probe")).arg("info").output() {
        Ok(o) => {
            if !o.status.success() {
                return true;
            }
            let s = String::from_utf8_lossy(&o.stdout);
            // An absent field is unknown, not faulted — same policy as a
            // probe we could not run at all.
            parse_fault_code(&s).is_some_and(|code| code != 0)
        }
        // Can't run the probe → don't assert a fault; treat as not-faulted.
        Err(_) => false,
    }
}

/// A private scratch path for this process's probe JSON.
///
/// NOT the shared system temp dir: that violates the project's no-/tmp rule
/// and a fixed name there races between overlapping refreshes and is
/// pre-creatable by any local user (review M6). Per-pid, under our own cache.
fn probe_json_path() -> PathBuf {
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library/Caches/com.avbaudio.companion");
    let _ = std::fs::create_dir_all(&base);
    base.join(format!("counters-{}.json", std::process::id()))
}

/// Live stats snapshot (read-only). Two live sources, no daemon IPC:
///   * the recovered-audio ring (`/Users/Shared/avb16r.ring`): its ClockBlock
///     gives media-clock lock + drift ppm, its counters give the ring's
///     producer-side overrun/zero-fill tallies, and its write frontier gives
///     the committed stream rate. Read directly via `avb-audioring`.
///   * `avb-probe counters --json`: the dext-side rx_frames + hardware rev.
///
/// `async` so Tauri runs it off the main thread (review M3): it samples the
/// ring over 100 ms and spawns the probe, which would otherwise stall the
/// webview once a second.
#[tauri::command]
async fn stats() -> Stats {
    let mut s = Stats::default();

    // Live ring read (media clock + ring counters + stream rate).
    if let Ok(shm) = avb_audioring::ShmRing::attach_ro() {
        let ring = shm.ring();
        use avb_audioring::layout::counter;
        // Producer-side tallies: frames lost because the consumer fell behind,
        // and frames the producer had to zero-fill. The old code read the
        // consumer's UNDERRUNS counter, which only the CoreAudio plug-in ever
        // increments — this process never reads the ring, so it was always
        // meaningless here (review H10).
        s.overrun_frames = ring.counter(counter::OVERRUN_FRAMES);
        s.zero_fill_frames = ring.counter(counter::ZERO_FILL_FRAMES);
        if let Some(cb) = ring.read_clock() {
            s.clock_locked = cb.locked;
            if cb.media_ppm_vs_gptp.is_finite() {
                s.media_drift_ppm = cb.media_ppm_vs_gptp;
            }
        }
        s.stream_frames_per_s = sample_stream_rate(ring);
    }

    // Dext-side counters (rx_frames, hardware) from the probe JSON.
    let tmp = probe_json_path();
    if let Ok(o) = Command::new(helper_path("avb-probe"))
        .arg("counters")
        .arg("--json")
        .arg(&tmp)
        .output()
    {
        if o.status.success() {
            if let Ok(txt) = std::fs::read_to_string(&tmp) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                    let probe = parse_stats(&v);
                    s.rx_frames = probe.rx_frames;
                    s.hw_rev = probe.hw_rev;
                    s.abi_version = probe.abi_version;
                    // Fall back to the probe's value if the ring was absent.
                    if s.media_drift_ppm == 0.0 {
                        s.media_drift_ppm = probe.media_drift_ppm;
                    }
                }
            }
        }
        let _ = std::fs::remove_file(&tmp);
    }
    s
}

/// Estimate committed stream rate (frames/s) by sampling the ring's write
/// frontier over a 100 ms window. Read-only.
fn sample_stream_rate(ring: &avb_audioring::AudioRing) -> u64 {
    let c0 = avb_audioring::Consumer::new(ring).write_frame();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let c1 = avb_audioring::Consumer::new(ring).write_frame();
    c1.saturating_sub(c0).saturating_mul(10)
}

/// Map the probe's counters JSON into the UI Stats shape. Tolerant of missing
/// keys so a partial probe still renders.
/// Open the project page in the user's browser. Done in Rust rather than via
/// the shell plugin's JS API: the webview had no `shell:allow-open` permission
/// (so the footer link silently did nothing) and granting the webview
/// arbitrary `shell:allow-execute` just for a link was overbroad (review L8).
#[tauri::command]
async fn open_project_page() {
    let _ = Command::new("/usr/bin/open")
        .arg("https://github.com/dextermarkley/DreamAVB")
        .status();
}

/// Map the probe's counters JSON into the UI Stats shape. Tolerant of missing
/// keys so a partial probe still renders. Only the dext-side fields come from
/// here; the ring read in `stats()` owns the clock/ring fields.
fn parse_stats(v: &serde_json::Value) -> Stats {
    let g = |k: &str| v.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
    let gf = |k: &str| v.get(k).and_then(serde_json::Value::as_f64).unwrap_or(0.0);
    Stats {
        clock_locked: v
            .get("locked")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        media_drift_ppm: gf("media_drift_ppm"),
        stream_frames_per_s: g("stream_frames_per_s"),
        overrun_frames: g("ring_overrun_audio"),
        zero_fill_frames: g("zero_fill_frames"),
        rx_frames: g("rx_frames"),
        hw_rev: v
            .get("hw_rev")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        abi_version: g("abi_version") as u32,
    }
}
// ---------------------------------------------------------------------------
// Clock source

/// One selectable clock-source option, for the UI's `<select>`. Derived from
/// `ClockSource::ALL` so the UI never hardcodes the list — adding a variant to
/// the crate adds it here, and to the dropdown, with no UI change.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClockOption {
    /// The stable token (`ClockSource::as_str`), stored in the config file and
    /// passed back to `set_clock_source`.
    pub value: String,
    pub label: String,
    pub description: String,
}

/// The clock-source snapshot the UI renders: both what the user SELECTED and
/// what is actually IN FORCE, so the UI never claims a clock that is not
/// running the stream (see `avb_config::EffectiveClock`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClockSourceInfo {
    /// The selected source's stable token.
    pub selected: String,
    /// Every option in UI order, derived from `ClockSource::ALL`.
    pub options: Vec<ClockOption>,
    /// Human label of the clock actually in force.
    pub effective_label: String,
    /// Why that clock is in force.
    pub effective_reason: String,
    /// False when the effective clock is undisciplined (the drift state) — the
    /// UI warns on this.
    pub network_disciplined: bool,
    /// Complaints from `Config::load` (e.g. a malformed config line). Must not
    /// be swallowed.
    pub warnings: Vec<String>,
}

fn clock_options() -> Vec<ClockOption> {
    ClockSource::ALL
        .iter()
        .map(|s| ClockOption {
            value: s.as_str().to_string(),
            label: s.label().to_string(),
            description: s.description().to_string(),
        })
        .collect()
}

/// The effective clock the DAEMON published, read from a ring header.
///
/// This replaces an earlier attempt to re-derive the answer here with
/// `avb_config::resolve`. That could not work: a gPTP servo lock is not
/// observable from this process (the only read-only `avb-probe` facts,
/// `info` and `counters`, carry no servo state, and the subcommands that
/// touch gPTP actively drive the device and would disturb a live stream), so
/// the gPTP input was hardcoded `false` and every `Network time` selection
/// reported as the Mac clock even while gPTP was running the stream.
///
/// The daemon is the only process that sees the selection, the servo and a
/// recovered media clock at once, so it resolves them and publishes the
/// verdict. Reading one field it wrote cannot be wrong in that way.
///
/// Checks the OUTPUT ring first (the talker's, where the verdict is written),
/// then the input ring. `None` means no daemon is running, or one too old to
/// publish a verdict — which is NOT the same as "undisciplined" and must be
/// reported differently.
fn published_effective_clock() -> Option<EffectiveClock> {
    for name in [avb_audioring::SHM_NAME_OUT, avb_audioring::SHM_NAME] {
        if let Some(e) = avb_audioring::ShmRing::attach_ro_at(name)
            .ok()
            .and_then(|shm| EffectiveClock::from_code(shm.ring().effective_clock()))
        {
            return Some(e);
        }
    }
    None
}

/// Build the UI snapshot: the user's selection from disk, and what is actually
/// in force from the daemon.
fn clock_source_info(cfg: Config, warnings: Vec<String>) -> ClockSourceInfo {
    let (effective_label, effective_reason, network_disciplined) = match published_effective_clock()
    {
        Some(e) => (
            e.label().to_string(),
            e.reason().to_string(),
            e.is_network_disciplined(),
        ),
        // No daemon, so nothing is sourcing audio at all. Saying "Mac
        // internal" here would claim a clock is in use when no stream
        // exists; say plainly that it is not running.
        None => (
            "—".to_string(),
            "no daemon running; this takes effect when a stream starts".to_string(),
            false,
        ),
    };
    ClockSourceInfo {
        selected: cfg.clock_source.as_str().to_string(),
        options: clock_options(),
        effective_label,
        effective_reason,
        network_disciplined,
        warnings,
    }
}

/// Current clock-source selection + the clock actually in force.
///
/// `async` so Tauri runs it off the main thread (review M3): `ring_clock_locked`
/// touches the shared ring.
#[tauri::command]
async fn get_clock_source() -> ClockSourceInfo {
    let (cfg, warnings) = Config::load();
    clock_source_info(cfg, warnings)
}

/// Change the clock source. Unknown tokens are REJECTED, not silently
/// defaulted — a bad token means a UI bug, and hiding it would leave the user
/// on a clock they did not pick. On success the config is persisted and the
/// same snapshot is returned so the UI re-renders from one response.
#[tauri::command]
async fn set_clock_source(value: String) -> Result<ClockSourceInfo, String> {
    let source =
        ClockSource::parse(&value).ok_or_else(|| format!("unknown clock source {value:?}"))?;
    let cfg = Config {
        clock_source: source,
    };
    cfg.save()
        .map_err(|e| format!("could not save settings: {e}"))?;
    // Reload so any warnings about the on-disk file (and the canonical parse)
    // are reflected, not just this in-memory value.
    let (loaded, warnings) = Config::load();
    Ok(clock_source_info(loaded, warnings))
}

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Has the popover taken focus since it was last shown?
static POPOVER_FOCUSED: AtomicBool = AtomicBool::new(false);

/// When the popover was last shown, for the blur grace period below.
static POPOVER_SHOWN_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// How long after `show()` a blur is ignored.
///
/// A focus guard alone is NOT enough, and assuming otherwise is why the tray
/// icon still looked dead after the first fix: `toggle_window` asks for focus
/// explicitly, so `Focused(true)` arrives at once and arms the guard, the
/// status item then takes focus back, and the resulting `Focused(false)` hides
/// the window in the same breath as opening it. A short window of time is the
/// robust form, because it does not depend on the order macOS delivers focus
/// events in.
const BLUR_GRACE: Duration = Duration::from_millis(600);

/// Should a `Focused(false)` event actually hide the popover?
///
/// Pure so the grace period is testable without a window: `visible_for` is how
/// long the popover has been up, `None` when it was never shown.
fn should_hide_on_blur(ever_focused: bool, visible_for: Option<Duration>) -> bool {
    match visible_for {
        Some(d) => ever_focused && d >= BLUR_GRACE,
        None => false,
    }
}

/// Append a line to `~/Library/Logs/DreamAVB.log`.
///
/// The tray click path cannot be observed from a test — whether macOS delivers
/// the click as `Left`+`Up` at all, and what rectangle it reports, is only
/// knowable from the running app. Two fixes were made from theory before this
/// existed; this is the instrument.
fn diag(msg: &str) {
    use std::io::Write;
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let mut path = std::path::PathBuf::from(home);
    path.push("Library/Logs");
    let _ = std::fs::create_dir_all(&path);
    path.push("DreamAVB.log");
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = writeln!(f, "[{secs}] {msg}");
}

/// Show the popover under the tray icon, or hide it if it is already up.
///
/// `icon_rect` is the status item's screen rectangle from the click event.
/// Without it the window keeps whatever position the OS last gave it, which is
/// why this used to open nowhere near the menu bar — the function was
/// documented as positioning "near the tray" but never set a position at all.
fn toggle_window(app: &tauri::AppHandle, icon_rect: Option<tauri::Rect>) {
    let Some(w) = app.get_webview_window("main") else {
        diag("toggle: no window labelled 'main'");
        return;
    };
    let visible = w.is_visible().unwrap_or(false);
    diag(&format!(
        "toggle: visible_before={visible} rect={icon_rect:?}"
    ));
    if visible {
        let _ = w.hide();
        *POPOVER_SHOWN_AT.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return;
    }
    if let Some(rect) = icon_rect {
        position_under(&w, rect);
    }
    POPOVER_FOCUSED.store(false, Ordering::SeqCst);
    *POPOVER_SHOWN_AT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    let shown = w.show();
    // An Accessory app has no Dock icon and is not "active", so the window
    // will not come forward on its own: ask for the app AND the window.
    #[cfg(target_os = "macos")]
    let _ = w.set_focus();
    let _ = w.set_always_on_top(true);
    diag(&format!(
        "toggle: show={:?} visible_after={:?} pos={:?} size={:?}",
        shown.is_ok(),
        w.is_visible(),
        w.outer_position(),
        w.outer_size()
    ));
}

/// A monitor's physical bounds, for [`popover_position`].
#[derive(Debug, Clone, Copy, PartialEq)]
struct MonitorBox {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// Where to put the popover for a status item at `icon_x`/`icon_y`.
///
/// Pure so the clamping is testable. Both axes are bounded: clamping only `x`
/// (as this first did) leaves a `y` from the click event free to put the window
/// wholly off-screen, which is indistinguishable from a tray icon that does
/// nothing. `mon` is `None` when the monitor cannot be read, in which case the
/// icon position is used unclamped.
fn popover_position(
    icon_x: f64,
    icon_y: f64,
    icon_w: f64,
    win_w: f64,
    win_h: f64,
    mon: Option<MonitorBox>,
    margin: f64,
) -> (i32, i32) {
    // Centre on the icon; just below the menu bar, where the icon's rect ends.
    let mut x = icon_x + icon_w / 2.0 - win_w / 2.0;
    let mut y = icon_y + margin / 2.0;
    if let Some(m) = mon {
        // max() after min() so a window wider/taller than the screen still
        // lands on it rather than being pushed off the near edge.
        x = x.min(m.x + m.w - win_w - margin).max(m.x + margin);
        y = y.min(m.y + m.h - win_h - margin).max(m.y + margin);
    }
    (x.round() as i32, y.round() as i32)
}

/// Place the popover centred under the status item, clamped onto the screen.
fn position_under(w: &tauri::WebviewWindow, rect: tauri::Rect) {
    use tauri::{PhysicalPosition, Position, Size};

    let scale = w.scale_factor().unwrap_or(1.0);
    let icon = match (rect.position, rect.size) {
        (Position::Physical(p), Size::Physical(s)) => (p.x as f64, p.y as f64, s.width as f64),
        (Position::Logical(p), Size::Logical(s)) => (p.x * scale, p.y * scale, s.width * scale),
        // Mixed units: fall back to the position alone, width unknown.
        (Position::Physical(p), _) => (p.x as f64, p.y as f64, 0.0),
        (Position::Logical(p), _) => (p.x * scale, p.y * scale, 0.0),
    };
    let (icon_x, icon_y, icon_w) = icon;
    let (win_w, win_h) = w
        .outer_size()
        .map(|s| (s.width as f64, s.height as f64))
        .unwrap_or((360.0 * scale, 520.0 * scale));
    let mon = w.current_monitor().ok().flatten().map(|m| {
        let p = m.position();
        let s = m.size();
        MonitorBox {
            x: f64::from(p.x),
            y: f64::from(p.y),
            w: f64::from(s.width),
            h: f64::from(s.height),
        }
    });
    let (x, y) = popover_position(icon_x, icon_y, icon_w, win_w, win_h, mon, 8.0 * scale);
    diag(&format!(
        "position: icon=({icon_x},{icon_y},w={icon_w}) win=({win_w}x{win_h}) mon={mon:?} -> ({x},{y})"
    ));
    let _ = w.set_position(Position::Physical(PhysicalPosition { x, y }));
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .invoke_handler(tauri::generate_handler![
            list_nics,
            enable_driver,
            disable_driver,
            status,
            stats,
            open_project_page,
            get_clock_source,
            set_clock_source
        ])
        .setup(|app| {
            diag("--- DreamAVB starting ---");
            // Menu-bar app: no dock icon.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            // "Open" as well as the icon click: a left-click toggle depends on
            // the tray event arriving as Left+Up, and if anything about that
            // differs the app looks dead with no other way in. A menu item is
            // a deterministic path — right-click the icon and pick it.
            let open = MenuItem::with_id(app, "open", "Open DreamAVB", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit DreamAVB", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &quit])?;

            // No unwrap on the icon (review L9): a missing bundle icon used to
            // panic during setup and take the whole app down at launch. A tray
            // without a custom icon is a degraded UI, not a crash.
            let mut tray = TrayIconBuilder::with_id("dreamavb-tray")
                .menu(&menu)
                .show_menu_on_left_click(false);
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
                diag("tray: icon loaded from bundle");
            } else {
                diag("tray: NO icon (not running from an .app bundle)");
            }
            let _tray = tray
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "quit" => app.exit(0),
                    // No icon rect from a menu item, so the popover keeps its
                    // last position rather than moving.
                    "open" => toggle_window(app, None),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    // Log EVERY event, not just the one we act on: whether
                    // macOS delivers the click as Left+Up at all is exactly
                    // what we could not previously tell.
                    diag(&format!("tray event: {event:?}"));
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        rect,
                        ..
                    } = event
                    {
                        toggle_window(tray.app_handle(), Some(rect));
                    }
                })
                .build(app)?;

            // Menu-bar popover behaviour: close when it loses focus, the way
            // every other menu-bar app behaves (review L11) — but not in the
            // first moments after being shown. See `BLUR_GRACE`: we ask for
            // focus ourselves, so a focus-only guard is armed instantly and
            // the following blur closed the window as fast as it opened.
            if let Some(w) = app.get_webview_window("main") {
                let hide = w.clone();
                w.on_window_event(move |ev| match ev {
                    tauri::WindowEvent::Focused(true) => {
                        POPOVER_FOCUSED.store(true, Ordering::SeqCst);
                        diag("window: focused");
                    }
                    tauri::WindowEvent::Focused(false) => {
                        let visible_for = POPOVER_SHOWN_AT
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .map(|t| t.elapsed());
                        let ever = POPOVER_FOCUSED.load(Ordering::SeqCst);
                        let hiding = should_hide_on_blur(ever, visible_for);
                        diag(&format!(
                            "window: blur ever_focused={ever} visible_for={visible_for:?} hiding={hiding}"
                        ));
                        if hiding {
                            let _ = hide.hide();
                        }
                    }
                    _ => {}
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running DreamAVB companion");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nic_i210() -> Nic {
        Nic {
            bsd_name: "en7".into(),
            display: "Intel I210 (Thunderbolt)".into(),
            pci_id: "8086:1533 / 1ab6:0214".into(),
            supported: true,
        }
    }

    #[test]
    fn absent_dext_maps_to_hands_off() {
        // The safety-critical invariant: when the dext is Absent (not
        // activated), the state is DefaultDriver — the OS default driver owns
        // the NIC. Tested on the pure classifier so it does not depend on this
        // machine's actual dext state.
        assert_eq!(
            status_for(DextSysextState::Absent, false, None).state,
            DriverState::DefaultDriver
        );
        // Activating -> Pending, enabled+ok -> Active, enabled+fault -> Fault.
        assert_eq!(
            status_for(DextSysextState::Activating, false, None).state,
            DriverState::Pending
        );
        assert_eq!(
            status_for(DextSysextState::Enabled, false, Some(nic_i210())).state,
            DriverState::Active
        );
        assert_eq!(
            status_for(DextSysextState::Enabled, true, Some(nic_i210())).state,
            DriverState::Fault
        );
    }

    /// Verbatim `avb-probe info` output from the live I210 on 2026-10-12,
    /// with the dext attached and the link up. The device is HEALTHY: the
    /// fault code is 0.
    const HEALTHY_PROBE: &str = "\
nic            Intel I210 (nic_kind 2, service AVBI210Driver)
vendor:device  0x15338086   subsystem 0x02141ab6
hw_rev         0x00000003   (PCI revision ID)
CTRL           0x081c0241   STATUS 0x00380783   TIMINCA 0x00000000
stage          timesync   fault 0 (none)
flags          0x0  link_up=false 1000FD=false
STATUS link    up=true speed=1000 Mb/s full
mac            64:4b:f0:12:73:b8   timebase 125/3
";

    /// Regression: the probe always PRINTS the word "fault" because it is a
    /// field label, so the old `contains("fault")` test reported a fault on
    /// every healthy device and the UI could never leave the Fault state.
    /// This asserts against real captured output, not a constructed string,
    /// so it cannot pass by restating the parser's own assumption.
    #[test]
    fn healthy_probe_output_is_not_a_fault() {
        assert_eq!(parse_fault_code(HEALTHY_PROBE), Some(0));
        // The bug, stated directly: the label is present, the fault is not.
        assert!(HEALTHY_PROBE.contains("fault"));
        assert!(!parse_fault_code(HEALTHY_PROBE).is_some_and(|c| c != 0));
    }

    #[test]
    fn nonzero_fault_code_is_a_fault() {
        let faulted = HEALTHY_PROBE.replace("fault 0 (none)", "fault 3 (tx_hang)");
        assert_eq!(parse_fault_code(&faulted), Some(3));
        assert!(parse_fault_code(&faulted).is_some_and(|c| c != 0));
    }

    /// An absent field is UNKNOWN, not healthy and not faulted — the caller
    /// must not assert a fault without positive evidence.
    #[test]
    fn missing_fault_field_is_unknown() {
        assert_eq!(parse_fault_code("nic   Intel I210\nflags 0x0\n"), None);
        assert_eq!(parse_fault_code(""), None);
        // A label with no parseable code must not read as a fault either.
        assert_eq!(parse_fault_code("stage timesync   fault\n"), None);
    }

    /// Regression (review M4): a stale enabled record must not report Active
    /// while the current version is still awaiting approval.
    #[test]
    fn stale_terminated_record_does_not_report_enabled() {
        let listing = "\
--- com.avbaudio.installer.i210
*   *   8NG8NMGYVF  com.avbaudio.installer.i210 (282.7.31/282.7.31) [terminated waiting to uninstall on reboot]
    *   8NG8NMGYVF  com.avbaudio.installer.i210 (283.0.31/283.0.31) [activated waiting for user]
";
        assert_eq!(classify_sysext(listing), DextSysextState::Activating);

        // An old record that is still 'activated enabled' but terminated is
        // likewise ignored.
        let stale_enabled = "\
*   *   8NG8NMGYVF  com.avbaudio.installer.i210 (282.7.31/282.7.31) [terminated waiting to uninstall on reboot]
    *   8NG8NMGYVF  com.avbaudio.installer.i210 (283.0.31/283.0.31) [activated waiting for user]
";
        assert_eq!(classify_sysext(stale_enabled), DextSysextState::Activating);

        // A genuinely enabled live record wins.
        let live = "*   *   8NG8NMGYVF  com.avbaudio.installer.i210 (283.0.31/283.0.31) [activated enabled]";
        assert_eq!(classify_sysext(live), DextSysextState::Enabled);

        // Nothing for our bundle id at all -> hands off.
        assert_eq!(
            classify_sysext("*   *   TEAM  com.example.other (1/1) [activated enabled]"),
            DextSysextState::Absent
        );
        assert_eq!(classify_sysext(""), DextSysextState::Absent);
    }

    /// The I210 is matched on PCI identity, and nothing else ever is (the
    /// Broadcom in particular is never driven).
    #[test]
    fn only_the_i210_matches_by_pci_identity() {
        let i210 = EthPort {
            name: "Intel I210".into(),
            bsd: Some("en7".into()),
            vendor: Some("0x8086".into()),
            device: Some("0x1533".into()),
            sub_vendor: Some("0x1ab6".into()),
            sub_device: Some("0x0214".into()),
        };
        assert!(i210.is_i210());

        // The Broadcom BCM57762 in Apple's Thunderbolt adapter.
        let bcm = EthPort {
            name: "Apple 57762-A0".into(),
            bsd: Some("en8".into()),
            vendor: Some("0x14e4".into()),
            device: Some("0x1682".into()),
            sub_vendor: Some("0x106b".into()),
            sub_device: Some("0x00f6".into()),
        };
        assert!(!bcm.is_i210());

        // A USB NIC with no PCI ids at all.
        assert!(!EthPort {
            name: "USB LAN".into(),
            ..Default::default()
        }
        .is_i210());

        // Right vendor, wrong device.
        assert!(!EthPort {
            vendor: Some("0x8086".into()),
            device: Some("0x157b".into()),
            ..Default::default()
        }
        .is_i210());
    }

    #[test]
    fn parse_stats_tolerates_partial_json() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"rx_frames": 737751, "ring_overrun_audio": 0}"#).unwrap();
        let s = parse_stats(&v);
        assert_eq!(s.rx_frames, 737_751);
        assert_eq!(s.overrun_frames, 0);
        assert!(!s.clock_locked); // missing -> false, not a panic
    }

    /// The probe scratch file must not live in the shared system temp dir.
    #[test]
    fn probe_json_is_private_and_per_process() {
        let p = probe_json_path();
        let s = p.to_string_lossy();
        assert!(!s.starts_with("/tmp"), "{s} is in the shared temp dir");
        assert!(
            !s.starts_with("/private/tmp"),
            "{s} is in the shared temp dir"
        );
        assert!(s.contains(&std::process::id().to_string()));
    }

    /// The dropdown options are DERIVED from the crate's `ClockSource::ALL`,
    /// not hardcoded here or in the UI: same count, same order, and every
    /// emitted token parses back to the variant it came from (so a value the UI
    /// sends to `set_clock_source` is always one `ClockSource::parse` accepts).
    #[test]
    fn clock_options_are_derived_from_the_crate() {
        let opts = clock_options();
        assert_eq!(opts.len(), ClockSource::ALL.len());
        for (opt, src) in opts.iter().zip(ClockSource::ALL.iter()) {
            assert_eq!(opt.value, src.as_str());
            assert_eq!(ClockSource::parse(&opt.value), Some(*src));
            assert!(!opt.label.is_empty());
            assert!(!opt.description.is_empty());
        }
    }

    /// The snapshot reports the SELECTION from disk and the EFFECTIVE clock
    /// from the daemon — two different facts, deliberately not conflated. With
    /// no daemon running the selection is still shown (so the control reflects
    /// what is saved) while nothing is claimed to be in force.
    #[test]
    fn the_selection_is_reported_separately_from_what_is_in_force() {
        let cfg = Config {
            clock_source: ClockSource::MacInternal,
        };
        let info = clock_source_info(cfg, vec!["a warning".to_string()]);
        assert_eq!(info.selected, "mac");
        // No ring in a unit test, so no verdict is published.
        assert!(!info.network_disciplined);
        assert!(
            info.effective_reason.contains("no daemon"),
            "got {:?}",
            info.effective_reason
        );
        // Warnings must survive into the snapshot, not be swallowed.
        assert_eq!(info.warnings, vec!["a warning".to_string()]);
    }

    /// With no daemon running there is no published verdict, and the snapshot
    /// must say so rather than naming a clock. Reporting "Mac internal" here
    /// would claim a clock is sourcing audio when no stream exists at all.
    #[test]
    fn no_daemon_reports_no_effective_clock_rather_than_the_mac() {
        // No ring is attached in a unit test, so this exercises the None arm.
        assert!(published_effective_clock().is_none());
        let info = clock_source_info(
            Config {
                clock_source: ClockSource::NetworkGptp,
            },
            Vec::new(),
        );
        // The SELECTION is still reported, so the control shows what is saved.
        assert_eq!(info.selected, "gptp");
        // But nothing is claimed to be in force.
        assert!(!info.network_disciplined);
        assert!(
            info.effective_reason.contains("no daemon"),
            "reason should say why there is no effective clock, got {:?}",
            info.effective_reason
        );
        assert_ne!(
            info.effective_label, "Mac internal",
            "must not name a clock when none is sourcing audio"
        );
    }

    // --- popover show/hide behaviour ---------------------------------------

    #[test]
    fn blur_is_ignored_before_the_grace_period() {
        // The regression that made the tray icon look dead: we ask for focus
        // ourselves, so `Focused(true)` arrives at once and a focus-only guard
        // is armed. The blur that immediately follows must NOT hide the window.
        assert!(
            !should_hide_on_blur(true, Some(Duration::from_millis(5))),
            "a blur milliseconds after show() must not close the popover"
        );
    }

    #[test]
    fn blur_hides_once_the_popover_has_settled() {
        assert!(should_hide_on_blur(
            true,
            Some(BLUR_GRACE + Duration::from_millis(1))
        ));
    }

    #[test]
    fn blur_does_nothing_when_never_focused_or_never_shown() {
        assert!(!should_hide_on_blur(false, Some(Duration::from_secs(10))));
        assert!(!should_hide_on_blur(true, None));
    }

    const SCREEN: MonitorBox = MonitorBox {
        x: 0.0,
        y: 0.0,
        w: 1920.0,
        h: 1080.0,
    };

    #[test]
    fn popover_centres_under_the_icon() {
        let (x, y) = popover_position(900.0, 24.0, 24.0, 360.0, 520.0, Some(SCREEN), 8.0);
        assert_eq!(x, 732, "centred: 900 + 12 - 180");
        assert_eq!(y, 28, "just below the icon's rect");
    }

    #[test]
    fn popover_stays_on_screen_for_a_corner_icon() {
        // An icon at the far right would otherwise put most of the window past
        // the screen edge.
        let (x, _) = popover_position(1900.0, 24.0, 24.0, 360.0, 520.0, Some(SCREEN), 8.0);
        assert!(
            x + 360 <= 1920 - 8 + 1,
            "right edge must stay on screen, got x={x}"
        );
    }

    #[test]
    fn popover_y_is_clamped_too() {
        // Clamping only x (the first version of this) let a bogus or
        // flipped-coordinate y put the window wholly off-screen — which looks
        // exactly like a tray icon that does nothing.
        let (_, y) = popover_position(900.0, 5000.0, 24.0, 360.0, 520.0, Some(SCREEN), 8.0);
        assert!(
            y + 520 <= 1080 - 8 + 1,
            "bottom edge must stay on screen, got y={y}"
        );
    }

    #[test]
    fn popover_lands_on_screen_when_larger_than_it() {
        // A window taller than the display must still touch the near edge
        // rather than be pushed off it by the clamp order.
        let small = MonitorBox {
            x: 0.0,
            y: 0.0,
            w: 300.0,
            h: 400.0,
        };
        let (x, y) = popover_position(100.0, 24.0, 24.0, 360.0, 520.0, Some(small), 8.0);
        assert_eq!((x, y), (8, 8), "clamped to the top-left margin");
    }

    #[test]
    fn popover_without_a_monitor_uses_the_icon_position() {
        let (x, y) = popover_position(900.0, 24.0, 24.0, 360.0, 520.0, None, 8.0);
        assert_eq!((x, y), (732, 28));
    }
}
