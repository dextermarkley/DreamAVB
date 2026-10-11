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

use std::path::PathBuf;
use std::process::Command;

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

/// Live stats pulled from `avb-probe` + the daemon (all read-only).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Stats {
    pub as_capable: bool,
    pub gptp_offset_ns: f64,
    pub stream_pkts_per_s: u64,
    pub media_drift_ppm: f64,
    pub ring_underruns: u64,
    pub ring_overruns: u64,
    pub rx_frames: u64,
    pub hw_rev: String,
    pub abi_version: u32,
}

/// Resolve a bundled/sibling helper binary (avb-installer / avb-probe).
fn helper_path(name: &str) -> PathBuf {
    // In a bundle the helpers sit next to the app binary; in dev, fall back to
    // the workspace release dir.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join(name);
            if cand.exists() {
                return cand;
            }
        }
    }
    PathBuf::from("/Users/dexter/AVB-Audio/target/release").join(name)
}

/// Enumerate candidate NICs by asking the system which network hardware ports
/// exist, then tagging the one DreamAVB supports (the Intel I210). Read-only:
/// `networksetup -listallhardwareports` + an `ioreg` check for the I210's PCI
/// identity. Everything that is not the I210 is listed UNSUPPORTED (and, per
/// project policy, never driven — the Broadcom included).
#[tauri::command]
fn list_nics() -> Vec<Nic> {
    let mut nics = Vec::new();

    // Is the I210 present at all? It is present either as a normal Ethernet
    // port (default driver owns it) or as our AVBI210Driver service (we own
    // it). `ioreg` sees both; the PCI id 8086:1533 is the ground truth.
    let i210_present = ioreg_has_i210();

    if i210_present {
        // BSD name only exists while the DEFAULT driver owns it (as enX); once
        // our dext claims it, the enX interface disappears. Report whatever
        // name networksetup still shows, else a placeholder.
        let bsd = ethernet_bsd_name().unwrap_or_else(|| "en0".into());
        nics.push(Nic {
            bsd_name: bsd,
            display: "Intel I210 (Thunderbolt)".into(),
            pci_id: "8086:1533 / 1ab6:0214".into(),
            supported: true,
        });
    }

    // List other Ethernet ports as unsupported context (never driven).
    for (port, dev) in list_hardware_ports() {
        if nics.iter().any(|n| n.bsd_name == dev) {
            continue;
        }
        if port.to_lowercase().contains("ethernet") || dev.starts_with("en") {
            nics.push(Nic {
                bsd_name: dev,
                display: port,
                pci_id: "—".into(),
                supported: false,
            });
        }
    }
    nics
}

/// True if an Intel I210 (PCI 8086:1533) is attached, per `ioreg`.
fn ioreg_has_i210() -> bool {
    Command::new("/usr/sbin/ioreg")
        .args(["-l", "-w", "0"])
        .output()
        .ok()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            // Our dext's service, or the raw PCI device id, either proves it.
            s.contains("AVBI210Driver") || s.contains("0x15338086")
        })
        .unwrap_or(false)
}

/// The BSD name (enX) of the first Ethernet hardware port, if any.
fn ethernet_bsd_name() -> Option<String> {
    list_hardware_ports()
        .into_iter()
        .find(|(port, dev)| dev.starts_with("en") && port.to_lowercase().contains("ethernet"))
        .map(|(_, dev)| dev)
}

/// Parse `networksetup -listallhardwareports` into (port name, device) pairs.
fn list_hardware_ports() -> Vec<(String, String)> {
    let out = match Command::new("/usr/sbin/networksetup")
        .arg("-listallhardwareports")
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut pairs = Vec::new();
    let mut cur_port: Option<String> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("Hardware Port: ") {
            cur_port = Some(p.trim().to_string());
        } else if let Some(d) = line.strip_prefix("Device: ") {
            if let Some(p) = cur_port.take() {
                pairs.push((p, d.trim().to_string()));
            }
        }
    }
    pairs
}

/// Enable DreamAVB on a NIC: activate the system extension so our dext claims
/// the device. Shells out to the entitled `avb-installer`.
#[tauri::command]
fn enable_driver(_bsd_name: String) -> Result<Status, String> {
    let out = Command::new(helper_path("avb-installer"))
        .arg("activate")
        .output()
        .map_err(|e| format!("could not run avb-installer: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(current_status())
}

/// Disable DreamAVB: deactivate the dext so the OS default driver reclaims the
/// NIC. This is the "give control back" action.
#[tauri::command]
fn disable_driver() -> Result<Status, String> {
    let out = Command::new(helper_path("avb-installer"))
        .arg("deactivate")
        .output()
        .map_err(|e| format!("could not run avb-installer: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(current_status())
}

/// Current driver ownership state (reads `systemextensionsctl` + a probe).
#[tauri::command]
fn status() -> Status {
    current_status()
}

/// Classify the dext's `systemextensionsctl list` line.
#[derive(Clone, Copy, PartialEq, Eq)]
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
    let text = match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(_) => return DextSysextState::Absent,
    };
    for line in text
        .lines()
        .filter(|l| l.contains("com.avbaudio.installer.i210"))
    {
        if line.contains("[activated enabled]") {
            return DextSysextState::Enabled;
        }
        // "activated waiting for user", "terminated waiting to uninstall", etc.
        if line.contains("activated") || line.contains("waiting") {
            return DextSysextState::Activating;
        }
    }
    DextSysextState::Absent
}

fn current_status() -> Status {
    // Hands-off is the safe default: only a positive signal flips us to Active,
    // so a read failure never falsely claims the driver owns the NIC.
    let state = dext_sysext_state();
    let faulted = state == DextSysextState::Enabled && probe_fault();
    status_for(state, faulted)
}

/// Pure mapping from (sysext state, fault) to the UI Status. Separated from the
/// system reads so the policy — Absent is always hands-off — is unit-testable
/// without depending on this machine's actual dext state.
fn status_for(state: DextSysextState, faulted: bool) -> Status {
    match state {
        DextSysextState::Enabled => {
            let nic = Some(Nic {
                bsd_name: "en7".into(),
                display: "Intel I210 (Thunderbolt)".into(),
                pci_id: "8086:1533 / 1ab6:0214".into(),
                supported: true,
            });
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

/// True if `avb-probe info` reports a dext fault (non-zero exit or a fault
/// marker in its output). Read-only.
fn probe_fault() -> bool {
    match Command::new(helper_path("avb-probe")).arg("info").output() {
        Ok(o) => {
            if !o.status.success() {
                return true;
            }
            let s = String::from_utf8_lossy(&o.stdout);
            s.to_lowercase().contains("fault")
        }
        // Can't run the probe → don't assert a fault; treat as not-faulted.
        Err(_) => false,
    }
}

/// Live stats snapshot (read-only). Parses `avb-probe counters --json`.
#[tauri::command]
fn stats() -> Stats {
    let tmp = std::env::temp_dir().join("dreamavb-counters.json");
    let ran = Command::new(helper_path("avb-probe"))
        .arg("counters")
        .arg("--json")
        .arg(&tmp)
        .output();
    if let Ok(o) = ran {
        if o.status.success() {
            if let Ok(txt) = std::fs::read_to_string(&tmp) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                    return parse_stats(&v);
                }
            }
        }
    }
    Stats::default()
}

/// Map the probe's counters JSON into the UI Stats shape. Tolerant of missing
/// keys so a partial probe still renders.
fn parse_stats(v: &serde_json::Value) -> Stats {
    let g = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    let gf = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
    Stats {
        as_capable: v
            .get("as_capable")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
        gptp_offset_ns: gf("gptp_offset_ns"),
        stream_pkts_per_s: g("stream_pkts_per_s"),
        media_drift_ppm: gf("media_drift_ppm"),
        ring_underruns: g("ring_underruns"),
        ring_overruns: g("ring_overrun_audio"),
        rx_frames: g("rx_frames"),
        hw_rev: v
            .get("hw_rev")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        abi_version: g("abi_version") as u32,
    }
}

/// Toggle the main popover window near the tray.
fn toggle_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else {
            let _ = w.show();
            let _ = w.set_focus();
        }
    }
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
            stats
        ])
        .setup(|app| {
            // Menu-bar app: no dock icon.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let quit = MenuItem::with_id(app, "quit", "Quit DreamAVB", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&quit])?;

            let _tray = TrayIconBuilder::with_id("dreamavb-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| {
                    if event.id.as_ref() == "quit" {
                        app.exit(0);
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_window(tray.app_handle());
                    }
                })
                .build(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running DreamAVB companion");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_dext_maps_to_hands_off() {
        // The safety-critical invariant: when the dext is Absent (not
        // activated), the state is DefaultDriver — the OS default driver owns
        // the NIC. Tested on the pure classifier so it does not depend on this
        // machine's actual dext state.
        assert_eq!(
            status_for(DextSysextState::Absent, false).state,
            DriverState::DefaultDriver
        );
        // Activating -> Pending, enabled+ok -> Active, enabled+fault -> Fault.
        assert_eq!(
            status_for(DextSysextState::Activating, false).state,
            DriverState::Pending
        );
        assert_eq!(
            status_for(DextSysextState::Enabled, false).state,
            DriverState::Active
        );
        assert_eq!(
            status_for(DextSysextState::Enabled, true).state,
            DriverState::Fault
        );
    }

    #[test]
    fn only_i210_is_supported() {
        let nics = list_nics();
        assert!(nics
            .iter()
            .any(|n| n.supported && n.pci_id.contains("8086:1533")));
        // Nothing else is marked supported (never touch the Broadcom).
        assert!(nics.iter().filter(|n| n.supported).count() == 1);
    }

    #[test]
    fn parse_stats_tolerates_partial_json() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"rx_frames": 737751, "ring_overrun_audio": 0}"#).unwrap();
        let s = parse_stats(&v);
        assert_eq!(s.rx_frames, 737_751);
        assert_eq!(s.ring_overruns, 0);
        assert!(!s.as_capable); // missing -> false, not a panic
    }
}
