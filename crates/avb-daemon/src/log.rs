// SPDX-License-Identifier: MIT OR Apache-2.0

//! Logging (design §11.1, NFR-4): stderr, prefixed `avb-daemon[<subsys>]:`,
//! rate-limited to one line per key per 10 s for repetitive conditions.
//! Nothing is logged on the audio per-packet path.

use std::collections::HashMap;
use std::io::Write;
use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Log levels (design §11.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

impl Level {
    pub fn parse(s: &str) -> Option<Level> {
        Some(match s {
            "error" => Level::Error,
            "warn" => Level::Warn,
            "info" => Level::Info,
            "debug" => Level::Debug,
            _ => return None,
        })
    }

    fn tag(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
        }
    }
}

/// Rate-limit window for keyed logs (§11.1: 10 s).
pub const RATE_LIMIT: Duration = Duration::from_secs(10);

/// The log subsystems (§11.1).
pub const SUBSYSTEMS: &[&str] = &[
    "dext", "gptp", "srp", "adp", "aecp", "acmp", "entity", "aaf", "mclk", "ring", "rec", "main",
];

/// A simple logger: a threshold level and a rate-limit map keyed by a string.
pub struct Logger {
    level: Level,
    last: Mutex<HashMap<String, Instant>>,
    /// Lines go to a writer thread, not straight to stderr. A stderr write is
    /// a syscall to the log file, and the talker loop logs from its real-time
    /// pass: 30-odd lines in one round stalled it ~1 ms, the talker retired
    /// 8-10 intervals, and every sink on both mixers dropped lock (an audible
    /// dropout). `None` only once dropped.
    tx: Option<Sender<String>>,
    writer: Option<JoinHandle<()>>,
}

impl Logger {
    pub fn new(level: Level) -> Self {
        let (tx, rx) = channel::<String>();
        let writer = std::thread::Builder::new()
            .name("avb-log".into())
            .spawn(move || {
                let stderr = std::io::stderr();
                for line in rx {
                    let _ = writeln!(stderr.lock(), "{line}");
                }
            })
            .ok();
        Self {
            level,
            last: Mutex::new(HashMap::new()),
            // Without a writer thread, fall back to writing inline.
            tx: writer.as_ref().map(|_| tx),
            writer,
        }
    }

    pub fn level(&self) -> Level {
        self.level
    }

    /// Log unconditionally (subject to the level threshold).
    pub fn log(&self, level: Level, subsys: &str, msg: &str) {
        if level <= self.level {
            let line = format!("avb-daemon[{subsys}]: {}: {msg}", level.tag());
            match &self.tx {
                Some(tx) => {
                    if let Err(e) = tx.send(line) {
                        eprintln!("{}", e.0);
                    }
                }
                None => eprintln!("{line}"),
            }
        }
    }

    /// Log at most once per [`RATE_LIMIT`] per `key` (for repetitive
    /// conditions like `harvest_stall`). Returns true if it logged.
    pub fn log_rate_limited(&self, level: Level, subsys: &str, key: &str, msg: &str) -> bool {
        if level > self.level {
            return false;
        }
        let now = Instant::now();
        let mut last = match self.last.lock() {
            Ok(l) => l,
            Err(p) => p.into_inner(),
        };
        if let Some(&t) = last.get(key) {
            if now.duration_since(t) < RATE_LIMIT {
                return false;
            }
        }
        last.insert(key.to_string(), now);
        drop(last);
        self.log(level, subsys, msg);
        true
    }
}

impl Drop for Logger {
    /// Flush: close the channel and wait for the writer to drain it, so the
    /// end-of-run summaries are never lost.
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_parse_and_threshold() {
        assert_eq!(Level::parse("warn"), Some(Level::Warn));
        assert_eq!(Level::parse("nope"), None);
        assert!(Level::Error < Level::Warn);
        assert!(Level::Warn < Level::Info);
    }

    #[test]
    fn rate_limit_suppresses_repeat() {
        let lg = Logger::new(Level::Info);
        assert!(lg.log_rate_limited(Level::Warn, "dext", "harvest_stall", "stalled"));
        // Immediate repeat is suppressed.
        assert!(!lg.log_rate_limited(Level::Warn, "dext", "harvest_stall", "stalled"));
        // A different key is allowed.
        assert!(lg.log_rate_limited(Level::Warn, "dext", "other", "x"));
    }

    #[test]
    fn level_threshold_blocks_below() {
        let lg = Logger::new(Level::Error);
        // Debug is below Error threshold -> not logged, returns false.
        assert!(!lg.log_rate_limited(Level::Debug, "mclk", "k", "m"));
    }

    #[test]
    fn subsystems_listed() {
        assert!(SUBSYSTEMS.contains(&"gptp"));
        assert!(SUBSYSTEMS.contains(&"rec"));
        assert_eq!(SUBSYSTEMS.len(), 12);
    }
}
