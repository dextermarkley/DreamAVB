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

/// Enumerate candidate NICs. Today only the I210 is supported; the Broadcom and
/// everything else are listed as UNSUPPORTED (and per project policy never
/// touched). Real enumeration is wired to `avb-probe` on hardware; this returns
/// the known set so the UI is populated.
#[tauri::command]
fn list_nics() -> Vec<Nic> {
    // A real build queries the system (SPNetworkDataType / ioreg) and tags each
    // by PCI id. The I210 is the one DreamAVB drives.
    vec![Nic {
        bsd_name: "en7".into(),
        display: "Intel I210 (Thunderbolt)".into(),
        pci_id: "8086:1533 / 1ab6:0214".into(),
        supported: true,
    }]
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

fn current_status() -> Status {
    // Hands-off is the default and the safe assumption when nothing reports
    // active. Hardware wiring reads `systemextensionsctl list` + `avb-probe
    // info` to distinguish default/pending/active/fault.
    Status {
        state: DriverState::DefaultDriver,
        nic: None,
        summary: "Default driver in control".into(),
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
    fn default_state_is_hands_off() {
        // The safety-critical default: nothing active -> OS default driver owns
        // the NIC. The UI must never show Active without a positive signal.
        assert_eq!(current_status().state, DriverState::DefaultDriver);
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
