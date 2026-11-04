// SPDX-License-Identifier: MIT OR Apache-2.0

//! The AVB devices the daemon found on the link, and the streams each one can
//! send us. Written by avb-daemon on every start, read by the companion app to
//! build the channel-map screen.
//!
//! This is a REPORT of what is on the network right now, never configuration:
//! nothing reads it to decide routing (that is `routing.txt`), so a device
//! disappearing from this file changes no mapping. A device that is mapped but
//! absent shows up in the app from `routing.txt` alone.
//!
//! `devices.txt` next to `config.txt`, TAB-separated (shown here with `→`),
//! one record per line:
//!
//! ```text
//! device→0x000a92fffe033ba9→0x000a92e001080501→StudioLive 16R
//! source→0x000a92fffe033ba9→1→8→Send 1-8
//! ```
//!
//! `device <entity_id> <model_id> <name>` and
//! `source <entity_id> <stream_output_index> <channels> <name>`. Names are
//! last so they may contain spaces; tabs and newlines in names are replaced.

use std::io;
use std::path::PathBuf;

/// One stream a device can send us (one of its STREAM_OUTPUTs carrying audio
/// we can depacketize).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub index: u16,
    pub channels: u16,
    pub name: String,
}

/// One AVB device seen on the link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub entity_id: u64,
    pub model_id: u64,
    pub name: String,
    pub sources: Vec<Source>,
}

fn clean(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '\t' || c == '\n' || c == '\r' {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}

fn hex(s: &str) -> Option<u64> {
    let s = s.trim();
    u64::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16).ok()
}

/// Serialize, devices in entity-id order so the file is stable.
pub fn to_text(devices: &[Device]) -> String {
    let mut ds: Vec<&Device> = devices.iter().collect();
    ds.sort_by_key(|d| d.entity_id);
    let mut s = String::from(
        "# AVB devices DreamAVB found on the link. Written by avb-daemon; a report, not settings.\n",
    );
    for d in ds {
        s.push_str(&format!(
            "device\t0x{:016x}\t0x{:016x}\t{}\n",
            d.entity_id,
            d.model_id,
            clean(&d.name)
        ));
        for src in &d.sources {
            s.push_str(&format!(
                "source\t0x{:016x}\t{}\t{}\t{}\n",
                d.entity_id,
                src.index,
                src.channels,
                clean(&src.name)
            ));
        }
    }
    s
}

/// Parse; unreadable lines are skipped (this is a report, best effort).
pub fn parse(text: &str) -> Vec<Device> {
    let mut out: Vec<Device> = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.splitn(5, '\t').collect();
        match f.as_slice() {
            ["device", e, m, name] => {
                if let (Some(entity_id), Some(model_id)) = (hex(e), hex(m)) {
                    out.push(Device {
                        entity_id,
                        model_id,
                        name: name.trim().to_string(),
                        sources: Vec::new(),
                    });
                }
            }
            ["source", e, i, c, name] => {
                let (Some(entity_id), Ok(index), Ok(channels)) =
                    (hex(e), i.trim().parse(), c.trim().parse())
                else {
                    continue;
                };
                if let Some(d) = out.iter_mut().find(|d| d.entity_id == entity_id) {
                    d.sources.push(Source {
                        index,
                        channels,
                        name: name.trim().to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// `~/Library/Application Support/DreamAVB/devices.txt`.
pub fn devices_path() -> Option<PathBuf> {
    let mut p = crate::config_path()?;
    p.set_file_name("devices.txt");
    Some(p)
}

/// Read the last report; empty if the daemon has never written one.
pub fn load() -> Vec<Device> {
    devices_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| parse(&t))
        .unwrap_or_default()
}

/// Write atomically (temp file + rename).
pub fn save(devices: &[Device]) -> io::Result<PathBuf> {
    let path =
        devices_path().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is unset"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("txt.tmp");
    std::fs::write(&tmp, to_text(devices))?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r16() -> Device {
        Device {
            entity_id: 0x000a_92ff_fe03_3ba9,
            model_id: 0x000a_92e0_0108_0501,
            name: "StudioLive 16R".into(),
            sources: (1..=8)
                .map(|i| Source {
                    index: i,
                    channels: 8,
                    name: format!("Send {}-{}", (i - 1) * 8 + 1, i * 8),
                })
                .collect(),
        }
    }

    #[test]
    fn round_trips_in_a_stable_order() {
        let b = Device {
            entity_id: 0x1,
            model_id: 0x2,
            name: "Other".into(),
            sources: vec![],
        };
        let text = to_text(&[r16(), b.clone()]);
        assert!(text.contains("device\t0x000a92fffe033ba9\t0x000a92e001080501\tStudioLive 16R\n"));
        assert!(text.contains("source\t0x000a92fffe033ba9\t1\t8\tSend 1-8\n"));
        assert_eq!(parse(&text), vec![b, r16()]);
    }

    #[test]
    fn hostile_names_cannot_break_the_format() {
        let mut d = r16();
        d.name = "Bad\tname\nwith device\t0x1".into();
        let back = parse(&to_text(&[d]));
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].name, "Bad name with device 0x1");
        assert_eq!(back[0].sources.len(), 8);
    }

    #[test]
    fn junk_is_skipped() {
        assert!(parse("garbage\nsource\t0x1\t1\t8\torphan\ndevice\tzz\t0\tx\n").is_empty());
    }
}
