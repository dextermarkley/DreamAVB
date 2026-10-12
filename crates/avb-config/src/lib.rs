// SPDX-License-Identifier: MIT OR Apache-2.0

//! User-selectable settings shared by the daemon and the companion app.
//!
//! Today that is one thing: which clock the Mac's audio is sourced on. It
//! matters because a talker and its listener do not share an oscillator. The
//! Mac's CoreAudio produces samples on the host crystal while the 16R consumes
//! them on its own media clock, and a nominal 48 000 Hz on each side is not the
//! same 48 000 Hz — the two drift apart until the stream underruns or overruns.
//! Sourcing on a clock the 16R also follows removes the drift at its origin,
//! rather than papering over it downstream.
//!
//! The file is a line-oriented `key=value` text file, not JSON, and this crate
//! has NO dependencies — see the note in `Cargo.toml`.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// Which clock the Mac sources audio on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ClockSource {
    /// Prefer the best clock actually available: the 16R's recovered media
    /// clock when a stream is locked, else network time, else the Mac. This is
    /// the default because the right answer changes with what is running —
    /// playback alone has no incoming stream to recover a media clock from.
    #[default]
    Auto,
    /// The 16R's media clock, recovered from the timestamps in the stream we
    /// receive from it. The most accurate option, because it tracks the very
    /// oscillator the 16R plays out on — but it needs an active input stream.
    SixteenRStream,
    /// Network time: the sample clock is derived from our gPTP-disciplined
    /// clock, which is slaved to the 16R as grandmaster. Works with playback
    /// alone, since it needs no incoming audio.
    NetworkGptp,
    /// The Mac's own crystal, free-running. No network discipline at all.
    /// Useful for testing without AVB, and as a fallback.
    MacInternal,
}

impl ClockSource {
    /// Every option, in the order a UI should present them.
    pub const ALL: [ClockSource; 4] = [
        ClockSource::Auto,
        ClockSource::SixteenRStream,
        ClockSource::NetworkGptp,
        ClockSource::MacInternal,
    ];

    /// The stable token written to the config file. Changing one of these is a
    /// config-format break.
    pub const fn as_str(self) -> &'static str {
        match self {
            ClockSource::Auto => "auto",
            ClockSource::SixteenRStream => "16r",
            ClockSource::NetworkGptp => "gptp",
            ClockSource::MacInternal => "mac",
        }
    }

    /// Parse a token, case-insensitively. `None` for anything unrecognised —
    /// callers substitute the default rather than guessing.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(ClockSource::Auto),
            "16r" | "stream" | "sixteenr" => Some(ClockSource::SixteenRStream),
            "gptp" | "network" => Some(ClockSource::NetworkGptp),
            "mac" | "internal" => Some(ClockSource::MacInternal),
            _ => None,
        }
    }

    /// Short label for a UI control.
    pub const fn label(self) -> &'static str {
        match self {
            ClockSource::Auto => "Automatic",
            ClockSource::SixteenRStream => "16R media clock",
            ClockSource::NetworkGptp => "Network time (gPTP)",
            ClockSource::MacInternal => "Mac internal",
        }
    }

    /// One line explaining the trade-off, for a UI to show beside the label.
    pub const fn description(self) -> &'static str {
        match self {
            ClockSource::Auto => {
                "Use the 16R's media clock when a stream is running, otherwise network time."
            }
            ClockSource::SixteenRStream => {
                "Recovered from the 16R's own stream. Most accurate, but needs an active input."
            }
            ClockSource::NetworkGptp => {
                "Derived from gPTP, with the 16R as grandmaster. Works for playback alone."
            }
            ClockSource::MacInternal => {
                "The Mac's own crystal, undisciplined. Will drift against the 16R."
            }
        }
    }
}

impl fmt::Display for ClockSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What is ACTUALLY disciplining the sample clock right now, as opposed to what
/// the user asked for. These differ whenever the chosen source is unavailable,
/// and a UI must show the effective value — telling the user the 16R's clock is
/// in use while the Mac's crystal is really running the stream is worse than
/// showing nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectiveClock {
    SixteenRStream,
    NetworkGptp,
    /// Falling back to the Mac's crystal, with the reason.
    MacInternal(FallbackReason),
}

/// Why the effective clock is not the selected one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackReason {
    /// The user asked for the Mac's clock.
    Selected,
    /// No locked input stream to recover a media clock from.
    NoStreamLock,
    /// gPTP is not locked to a grandmaster.
    NoGptpLock,
}

impl EffectiveClock {
    /// The code published in the ring header so other processes can read what
    /// is actually disciplining the stream.
    ///
    /// The DAEMON is the authority here, and this exists so nobody else has to
    /// re-derive the answer. The companion app cannot see a gPTP servo lock,
    /// so an app that called [`resolve`] itself would have to guess — and a
    /// guess of "not locked" reports the Mac's crystal while the 16R's clock
    /// is really running the stream. Reading one field the daemon wrote cannot
    /// be wrong in that way.
    ///
    /// 0 is deliberately "unknown", because a ring whose header was never
    /// written by a clock-aware daemon reads as zero.
    pub const fn code(self) -> u32 {
        match self {
            EffectiveClock::SixteenRStream => 1,
            EffectiveClock::NetworkGptp => 2,
            EffectiveClock::MacInternal(FallbackReason::Selected) => 3,
            EffectiveClock::MacInternal(FallbackReason::NoStreamLock) => 4,
            EffectiveClock::MacInternal(FallbackReason::NoGptpLock) => 5,
        }
    }

    /// Decode a published code. `None` for 0 (never written) or anything
    /// unrecognised — a reader must say "unknown", not invent a clock.
    pub const fn from_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(EffectiveClock::SixteenRStream),
            2 => Some(EffectiveClock::NetworkGptp),
            3 => Some(EffectiveClock::MacInternal(FallbackReason::Selected)),
            4 => Some(EffectiveClock::MacInternal(FallbackReason::NoStreamLock)),
            5 => Some(EffectiveClock::MacInternal(FallbackReason::NoGptpLock)),
            _ => None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            EffectiveClock::SixteenRStream => "16R media clock",
            EffectiveClock::NetworkGptp => "Network time (gPTP)",
            EffectiveClock::MacInternal(_) => "Mac internal",
        }
    }

    /// Is the sample clock disciplined to the network at all? A UI should warn
    /// when this is false, because drift against the 16R is then unavoidable.
    pub const fn is_network_disciplined(self) -> bool {
        matches!(
            self,
            EffectiveClock::SixteenRStream | EffectiveClock::NetworkGptp
        )
    }

    /// Why we are on this clock, for the UI's explanatory line.
    pub const fn reason(self) -> &'static str {
        match self {
            EffectiveClock::SixteenRStream => "recovered from the 16R's stream",
            EffectiveClock::NetworkGptp => "derived from gPTP",
            EffectiveClock::MacInternal(FallbackReason::Selected) => "selected",
            EffectiveClock::MacInternal(FallbackReason::NoStreamLock) => {
                "no locked input stream to recover the 16R's clock from"
            }
            EffectiveClock::MacInternal(FallbackReason::NoGptpLock) => "gPTP is not locked",
        }
    }
}

/// Resolve the selection against what is actually available.
///
/// Pure, so the policy is testable without a daemon: `stream_locked` is a
/// locked media clock recovered from an input stream, `gptp_locked` is a locked
/// gPTP servo. An explicit choice is NOT silently upgraded — asking for the Mac
/// clock gets the Mac clock — but it IS downgraded when unavailable, with the
/// reason, because the alternative is pretending.
pub fn resolve(sel: ClockSource, stream_locked: bool, gptp_locked: bool) -> EffectiveClock {
    match sel {
        ClockSource::MacInternal => EffectiveClock::MacInternal(FallbackReason::Selected),
        ClockSource::SixteenRStream => {
            if stream_locked {
                EffectiveClock::SixteenRStream
            } else {
                EffectiveClock::MacInternal(FallbackReason::NoStreamLock)
            }
        }
        ClockSource::NetworkGptp => {
            if gptp_locked {
                EffectiveClock::NetworkGptp
            } else {
                EffectiveClock::MacInternal(FallbackReason::NoGptpLock)
            }
        }
        // Auto walks down in accuracy order. The 16R's own media clock beats
        // gPTP even when both are available: gPTP carries TIME, and a media
        // clock derived from it is only as good as the assumption that the
        // 16R's audio oscillator follows gPTP too.
        ClockSource::Auto => {
            if stream_locked {
                EffectiveClock::SixteenRStream
            } else if gptp_locked {
                EffectiveClock::NetworkGptp
            } else {
                EffectiveClock::MacInternal(FallbackReason::NoGptpLock)
            }
        }
    }
}

/// Parse a boolean config token, case-insensitively.
///
/// Accepts the obvious spellings a hand-editor might reach for. `None` for
/// anything unrecognised — the caller keeps the previous value and warns,
/// rather than guessing a boolean (which halves the odds of being right).
pub fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" | "enabled" => Some(true),
        "false" | "no" | "off" | "0" | "disabled" => Some(false),
        _ => None,
    }
}

/// The user-settable configuration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub clock_source: ClockSource,
    /// Start the companion app automatically at login. Default OFF: an audio
    /// utility that installs itself into the user's login items uninvited is a
    /// bad citizen. The companion app owns the system side of this (a macOS
    /// LaunchAgent); this crate only remembers the user's choice so both the
    /// app and the daemon can read it.
    pub launch_at_login: bool,
    /// Auto-select the supported NIC on app start instead of making the user
    /// pick. Default OFF: SELECTS ONLY — it never enables/activates the driver
    /// (that stays an explicit user action), and it only acts when the choice
    /// is unambiguous (see [`auto_select_nic`]).
    pub auto_select_nic: bool,
}

/// Why auto-select did not pick a NIC. Carries enough for the UI to explain
/// itself without guessing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoSelectOutcome {
    /// Exactly one supported NIC — selected, by this BSD name (which may be
    /// empty when our own dext owns the device and its enX interface is gone).
    Selected { bsd_name: String },
    /// No supported NIC to select.
    NoneSupported,
    /// More than one supported NIC: refuse to guess which the user wants.
    Ambiguous { count: usize },
}

/// Decide whether auto-select picks a NIC, as a PURE function over the
/// supported-NIC identifiers.
///
/// `supported` is the BSD name of every NIC the driver supports, in list
/// order. The rule, deliberately conservative because the wrong NIC is worse
/// than no NIC:
///   * exactly one  -> select it;
///   * zero         -> select nothing, say none is supported;
///   * two or more  -> select nothing, say it is ambiguous.
///
/// It SELECTS ONLY. Enabling/activating the driver is never part of this — the
/// caller takes the returned BSD name and sets the dropdown, nothing more.
pub fn auto_select_nic(supported: &[String]) -> AutoSelectOutcome {
    match supported {
        [one] => AutoSelectOutcome::Selected {
            bsd_name: one.clone(),
        },
        [] => AutoSelectOutcome::NoneSupported,
        many => AutoSelectOutcome::Ambiguous { count: many.len() },
    }
}

/// The config file's path: `~/Library/Application Support/DreamAVB/config.txt`.
///
/// `None` when `HOME` is unset. Both the daemon and the app run as the user —
/// the daemon carries its entitlement without root — so they agree on this
/// path. A daemon run under `sudo` would read root's HOME instead, which is
/// why every reader LOGS the path it used rather than assuming.
pub fn config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    let mut p = PathBuf::from(home);
    p.push("Library/Application Support/DreamAVB/config.txt");
    Some(p)
}

impl Config {
    /// Load the config, falling back to defaults.
    ///
    /// A missing file is the normal first-run case. A malformed one is NOT
    /// treated as fatal either: an audio daemon that refuses to start because a
    /// settings file has a typo is worse than one that runs on defaults, and
    /// the caller is told which keys were ignored so it can say so.
    pub fn load() -> (Self, Vec<String>) {
        let Some(path) = config_path() else {
            return (
                Self::default(),
                vec!["HOME is unset; using default settings".to_string()],
            );
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (Self::default(), Vec::new()),
            Err(e) => (
                Self::default(),
                vec![format!("could not read {}: {e}", path.display())],
            ),
        }
    }

    /// Parse the file body. Returns the config and a list of complaints about
    /// anything ignored.
    pub fn parse(text: &str) -> (Self, Vec<String>) {
        let mut cfg = Self::default();
        let mut warnings = Vec::new();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                warnings.push(format!("line {}: no '=' in {line:?}", i + 1));
                continue;
            };
            match key.trim() {
                "clock_source" => match ClockSource::parse(value) {
                    Some(s) => cfg.clock_source = s,
                    None => warnings.push(format!(
                        "line {}: unknown clock_source {:?}; using {}",
                        i + 1,
                        value.trim(),
                        cfg.clock_source
                    )),
                },
                "launch_at_login" => match parse_bool(value) {
                    Some(b) => cfg.launch_at_login = b,
                    None => warnings.push(format!(
                        "line {}: unknown launch_at_login {:?}; using {}",
                        i + 1,
                        value.trim(),
                        cfg.launch_at_login
                    )),
                },
                "auto_select_nic" => match parse_bool(value) {
                    Some(b) => cfg.auto_select_nic = b,
                    None => warnings.push(format!(
                        "line {}: unknown auto_select_nic {:?}; using {}",
                        i + 1,
                        value.trim(),
                        cfg.auto_select_nic
                    )),
                },
                other => warnings.push(format!("line {}: unknown key {other:?}", i + 1)),
            }
        }
        (cfg, warnings)
    }

    /// Serialize to the file body.
    pub fn to_text(&self) -> String {
        format!(
            "# DreamAVB settings. Written by the companion app; read by avb-daemon.\n\
             clock_source={}\n\
             launch_at_login={}\n\
             auto_select_nic={}\n",
            self.clock_source, self.launch_at_login, self.auto_select_nic
        )
    }

    /// Write the config, creating the directory if needed.
    ///
    /// Atomic: written to a sibling temporary file and renamed, so a reader
    /// never sees a half-written file. The daemon re-reads this while running,
    /// so a torn read is a live possibility, not a theoretical one.
    pub fn save(&self) -> io::Result<PathBuf> {
        let path = config_path()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is unset"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("txt.tmp");
        std::fs::write(&tmp, self.to_text())?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_round_trip() {
        for s in ClockSource::ALL {
            assert_eq!(ClockSource::parse(s.as_str()), Some(s), "{s}");
            // Case and surrounding space must not matter: these come from a
            // hand-editable file.
            let shouty = format!("  {}  ", s.as_str().to_ascii_uppercase());
            assert_eq!(ClockSource::parse(&shouty), Some(s), "{s}");
        }
        assert_eq!(ClockSource::parse("nonsense"), None);
        assert_eq!(ClockSource::parse(""), None);
    }

    #[test]
    fn default_is_auto() {
        assert_eq!(ClockSource::default(), ClockSource::Auto);
        assert_eq!(Config::default().clock_source, ClockSource::Auto);
    }

    #[test]
    fn config_round_trips_through_text() {
        for s in ClockSource::ALL {
            let c = Config {
                clock_source: s,
                ..Default::default()
            };
            let (back, warnings) = Config::parse(&c.to_text());
            assert_eq!(back, c, "{s}");
            assert!(warnings.is_empty(), "{s}: {warnings:?}");
        }
    }

    /// Both boolean settings survive the round trip through the file, in every
    /// combination, so a serialize/parse mismatch on either cannot slip by.
    #[test]
    fn bool_settings_round_trip_through_text() {
        for launch in [true, false] {
            for auto in [true, false] {
                let c = Config {
                    clock_source: ClockSource::NetworkGptp,
                    launch_at_login: launch,
                    auto_select_nic: auto,
                };
                let (back, warnings) = Config::parse(&c.to_text());
                assert_eq!(back, c, "launch={launch} auto={auto}");
                assert!(
                    warnings.is_empty(),
                    "launch={launch} auto={auto}: {warnings:?}"
                );
            }
        }
    }

    /// The spellings a hand-editor might use all parse, and a bogus value is
    /// rejected (not guessed) so the previous value stands and a warning is
    /// raised.
    #[test]
    fn parse_bool_accepts_common_spellings_and_rejects_junk() {
        for t in ["true", "yes", "on", "1", "ENABLED", "  On  "] {
            assert_eq!(parse_bool(t), Some(true), "{t:?}");
        }
        for f in ["false", "no", "off", "0", "Disabled"] {
            assert_eq!(parse_bool(f), Some(false), "{f:?}");
        }
        assert_eq!(parse_bool("maybe"), None);
        assert_eq!(parse_bool(""), None);
    }

    /// A malformed boolean must warn and keep the default, never flip the
    /// setting to a guessed value. Also exercises the two new keys appearing
    /// alongside clock_source and an unknown key.
    #[test]
    fn malformed_bool_warns_but_keeps_default() {
        let (cfg, warnings) = Config::parse(
            "clock_source=mac\n\
             launch_at_login=yes\n\
             auto_select_nic=sometimes\n\
             bogus_key=1\n",
        );
        assert_eq!(cfg.clock_source, ClockSource::MacInternal);
        // The good bool applied.
        assert!(cfg.launch_at_login);
        // The malformed bool did NOT flip the default.
        assert!(!cfg.auto_select_nic);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings
            .iter()
            .any(|w| w.contains("unknown auto_select_nic")));
        assert!(warnings.iter().any(|w| w.contains("unknown key")));
    }

    /// Both new settings default OFF, as required.
    #[test]
    fn bool_settings_default_off() {
        let c = Config::default();
        assert!(!c.launch_at_login);
        assert!(!c.auto_select_nic);
        // And an absent file yields those defaults with no complaint.
        let (parsed, warnings) = Config::parse("");
        assert!(!parsed.launch_at_login);
        assert!(!parsed.auto_select_nic);
        assert!(warnings.is_empty());
    }

    /// The auto-select DECISION as a pure function: exactly one supported NIC
    /// selects it, zero selects nothing (none supported), two or more selects
    /// nothing (ambiguous). Never guesses.
    #[test]
    fn auto_select_picks_only_when_unambiguous() {
        // Exactly one -> select it, by its BSD name.
        assert_eq!(
            auto_select_nic(&["en7".to_string()]),
            AutoSelectOutcome::Selected {
                bsd_name: "en7".to_string()
            }
        );
        // One supported NIC whose enX is gone (our dext owns it): still a
        // selection, with an empty BSD name — the UI matches it by that.
        assert_eq!(
            auto_select_nic(&[String::new()]),
            AutoSelectOutcome::Selected {
                bsd_name: String::new()
            }
        );
        // Zero -> none supported.
        assert_eq!(auto_select_nic(&[]), AutoSelectOutcome::NoneSupported);
        // Two -> ambiguous, with the count, and NOTHING selected.
        assert_eq!(
            auto_select_nic(&["en7".to_string(), "en8".to_string()]),
            AutoSelectOutcome::Ambiguous { count: 2 }
        );
        // Three -> still ambiguous.
        assert_eq!(
            auto_select_nic(&["a".to_string(), "b".to_string(), "c".to_string()]),
            AutoSelectOutcome::Ambiguous { count: 3 }
        );
    }

    #[test]
    fn malformed_lines_warn_but_keep_defaults() {
        let (cfg, warnings) = Config::parse(
            "# a comment\n\
             \n\
             clock_source=gptp\n\
             garbage line\n\
             unknown_key=1\n\
             clock_source=not-a-source\n",
        );
        // The good line still applied; the bad one did not overwrite it.
        assert_eq!(cfg.clock_source, ClockSource::NetworkGptp);
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("no '='")));
        assert!(warnings.iter().any(|w| w.contains("unknown key")));
        assert!(warnings.iter().any(|w| w.contains("unknown clock_source")));
    }

    #[test]
    fn empty_file_is_defaults_without_complaint() {
        let (cfg, warnings) = Config::parse("");
        assert_eq!(cfg, Config::default());
        assert!(warnings.is_empty());
    }

    #[test]
    fn resolve_auto_prefers_the_stream_then_gptp_then_mac() {
        assert_eq!(
            resolve(ClockSource::Auto, true, true),
            EffectiveClock::SixteenRStream
        );
        assert_eq!(
            resolve(ClockSource::Auto, false, true),
            EffectiveClock::NetworkGptp
        );
        assert_eq!(
            resolve(ClockSource::Auto, false, false),
            EffectiveClock::MacInternal(FallbackReason::NoGptpLock)
        );
    }

    /// An explicit choice must not be silently upgraded: a user who picks the
    /// Mac's clock to take the network out of the picture (while debugging, or
    /// with no 16R attached) gets exactly that.
    #[test]
    fn an_explicit_mac_choice_is_never_upgraded() {
        for (stream, gptp) in [(true, true), (true, false), (false, true), (false, false)] {
            assert_eq!(
                resolve(ClockSource::MacInternal, stream, gptp),
                EffectiveClock::MacInternal(FallbackReason::Selected),
                "stream={stream} gptp={gptp}"
            );
        }
    }

    /// An unavailable choice IS downgraded, and says which one it was. The
    /// alternative is reporting a clock that is not running the stream.
    #[test]
    fn an_unavailable_choice_falls_back_with_its_reason() {
        assert_eq!(
            resolve(ClockSource::SixteenRStream, false, true),
            EffectiveClock::MacInternal(FallbackReason::NoStreamLock)
        );
        assert_eq!(
            resolve(ClockSource::NetworkGptp, true, false),
            EffectiveClock::MacInternal(FallbackReason::NoGptpLock)
        );
        // gPTP being locked does not rescue a 16R-stream choice: the user asked
        // for the media clock, and gPTP is a different clock.
        assert_eq!(
            resolve(ClockSource::SixteenRStream, false, false),
            EffectiveClock::MacInternal(FallbackReason::NoStreamLock)
        );
    }

    #[test]
    fn only_network_sources_count_as_disciplined() {
        assert!(EffectiveClock::SixteenRStream.is_network_disciplined());
        assert!(EffectiveClock::NetworkGptp.is_network_disciplined());
        assert!(
            !EffectiveClock::MacInternal(FallbackReason::Selected).is_network_disciplined(),
            "the Mac's crystal is not disciplined to anything"
        );
    }

    #[test]
    fn every_option_has_a_label_and_a_description() {
        for s in ClockSource::ALL {
            assert!(!s.label().is_empty(), "{s}");
            assert!(!s.description().is_empty(), "{s}");
        }
    }

    /// Every state `resolve` can produce must survive the round trip through a
    /// published code, and no two may collide. Driven from `resolve`'s own
    /// outputs rather than a hand-written list, so a new state cannot be added
    /// without either encoding it or failing here.
    #[test]
    fn every_resolvable_state_round_trips_through_its_code() {
        let mut seen = Vec::new();
        for sel in ClockSource::ALL {
            for stream in [true, false] {
                for gptp in [true, false] {
                    let e = resolve(sel, stream, gptp);
                    let code = e.code();
                    assert_ne!(code, 0, "{e:?} encodes as 0, which means 'never written'");
                    assert_eq!(
                        EffectiveClock::from_code(code),
                        Some(e),
                        "{e:?} did not survive its code"
                    );
                    if let Some((other, _)) = seen.iter().find(|(_, c)| *c == code) {
                        assert_eq!(*other, e, "two states share code {code}");
                    }
                    seen.push((e, code));
                }
            }
        }
        // All four effective states are reachable, so the encoding is exercised
        // rather than one arm being dead.
        let distinct: std::collections::BTreeSet<u32> = seen.iter().map(|(_, c)| *c).collect();
        assert!(
            distinct.len() >= 4,
            "only {} states reachable",
            distinct.len()
        );
    }

    #[test]
    fn an_unwritten_or_unknown_code_is_not_a_clock() {
        // 0 is what an untouched ring header reads as.
        assert_eq!(EffectiveClock::from_code(0), None);
        assert_eq!(EffectiveClock::from_code(u32::MAX), None);
    }
}
