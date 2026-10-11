// SPDX-License-Identifier: MIT OR Apache-2.0

//! External input validation (design §13.2). clap handles syntactic ranges
//! (exit 2); these are the semantic checks that map to specific exit codes
//! (talker id format, output path writability, disk space).

use std::path::Path;

use crate::exit::Exit;

/// Parse a `--talker` id: 16 hex digits, optional `0x` prefix and colons
/// (§13.2). Returns the EUI-64 as a u64 or an error (exit 2).
pub fn parse_talker(s: &str) -> Result<u64, Exit> {
    let cleaned: String = s
        .trim_start_matches("0x")
        .trim_start_matches("0X")
        .chars()
        .filter(|c| *c != ':')
        .collect();
    if cleaned.len() != 16 || !cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Exit::Usage);
    }
    u64::from_str_radix(&cleaned, 16).map_err(|_| Exit::Usage)
}

/// A timeout in seconds must be 1–600 (§13.2).
pub fn validate_timeout(s: u64) -> Result<u64, Exit> {
    if (1..=600).contains(&s) {
        Ok(s)
    } else {
        Err(Exit::Usage)
    }
}

/// `--seconds` must be 1–86 400 (§13.2).
pub fn validate_seconds(s: u64) -> Result<u64, Exit> {
    if (1..=86_400).contains(&s) {
        Ok(s)
    } else {
        Err(Exit::Usage)
    }
}

/// The parent directory of `--out` must exist and be writable (§13.2). The
/// file itself may exist (it is overwritten). Maps to exit 9.
pub fn validate_out_path(path: &Path) -> Result<(), Exit> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = parent.unwrap_or_else(|| Path::new("."));
    if !dir.is_dir() {
        return Err(Exit::DiskOrPath);
    }
    // Writability: probe with a metadata read; a real write check happens when
    // the file is opened (also exit 9).
    match std::fs::metadata(dir) {
        Ok(m) if m.permissions().readonly() => Err(Exit::DiskOrPath),
        Ok(_) => Ok(()),
        Err(_) => Err(Exit::DiskOrPath),
    }
}

/// Projected WAV size in bytes (§11.2): `104 + seconds·rate·channels·4`.
pub fn projected_wav_bytes(seconds: u64, rate: u32, channels: u32) -> u64 {
    104 + seconds * u64::from(rate) * u64::from(channels) * 4
}

/// Disk-space check (§11.2): free space must be ≥ projected + 64 MiB.
/// Returns the free bytes on success for logging; exit 9 on insufficient
/// space. Uses `statvfs` on the directory of `path`.
pub fn check_disk_space(path: &Path, projected: u64) -> Result<u64, Exit> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let free = statvfs_free_bytes(dir).ok_or(Exit::DiskOrPath)?;
    let need = projected.saturating_add(64 * 1024 * 1024);
    if free >= need {
        Ok(free)
    } else {
        Err(Exit::DiskOrPath)
    }
}

/// Free bytes on the filesystem containing `dir`, via `libc::statvfs`.
fn statvfs_free_bytes(dir: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: zeroed statvfs, valid path pointer.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    if r != 0 {
        return None;
    }
    Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn talker_id_formats() {
        assert_eq!(parse_talker("000a92033ba90001"), Ok(0x000a_9203_3ba9_0001));
        assert_eq!(
            parse_talker("0x000a92033ba90001"),
            Ok(0x000a_9203_3ba9_0001)
        );
        assert_eq!(
            parse_talker("00:0a:92:03:3b:a9:00:01"),
            Ok(0x000a_9203_3ba9_0001)
        );
        assert_eq!(parse_talker("short"), Err(Exit::Usage));
        assert_eq!(parse_talker("000a92033ba9000g"), Err(Exit::Usage)); // non-hex
    }

    #[test]
    fn timeout_and_seconds_ranges() {
        assert_eq!(validate_timeout(30), Ok(30));
        assert_eq!(validate_timeout(0), Err(Exit::Usage));
        assert_eq!(validate_timeout(601), Err(Exit::Usage));
        assert_eq!(validate_seconds(1), Ok(1));
        assert_eq!(validate_seconds(86_400), Ok(86_400));
        assert_eq!(validate_seconds(86_401), Err(Exit::Usage));
        assert_eq!(validate_seconds(0), Err(Exit::Usage));
    }

    #[test]
    fn projected_size() {
        // 10 s, 48 kHz, 8 ch, 4 B = 15_360_000 + 104.
        assert_eq!(projected_wav_bytes(10, 48_000, 8), 104 + 15_360_000);
    }

    /// A path whose parent always exists on any checkout (this crate's own
    /// directory) — never a machine-specific absolute path, so the test passes
    /// on CI runners and other contributors' machines.
    fn crate_dir_out() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("out.wav")
    }

    #[test]
    fn out_path_parent_must_exist() {
        let ok = crate_dir_out();
        assert_eq!(validate_out_path(&ok), Ok(()));
        let bad = Path::new("/no/such/dir/out.wav");
        assert_eq!(validate_out_path(bad), Err(Exit::DiskOrPath));
    }

    #[test]
    fn disk_space_check_runs() {
        // The checkout volume almost certainly has > 64 MiB + a few bytes free.
        let p = crate_dir_out();
        assert!(check_disk_space(&p, 1024).is_ok());
    }
}
