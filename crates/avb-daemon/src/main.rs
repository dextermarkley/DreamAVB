// SPDX-License-Identifier: MIT OR Apache-2.0

//! avb-daemon: the Phase 1a AVB listener daemon (design §5, §11, §13).
//!
//! Subcommands (§11.1): `run`, `aem-dump`, `gptp`, `streams`, `record`,
//! `capture`. The heavy logic (TX scheduler, media path, WAV/RF64, pcap,
//! reports) is in sans-IO modules unit-tested with [`dextio::fake::FakeDext`];
//! `main` is the CLI shell and `realdext` + `stack` are the IOKit/thread glue
//! that runs on hardware at the milestones.
//!
//! `dead_code` is allowed at the crate root because the data-path building
//! blocks (`audio`, `txsched`, `wav`, `pcap`, `report`, the `DextPort` trait
//! and the extra exit codes) are connected by the ctl/audio/rec threads and
//! the record loop, which are the hardware-milestone step (§5.1 threads,
//! §14.3). They are fully exercised NOW by the unit tests and the 48 k/96 k
//! pipeline tests; the lint would otherwise fire on the test-only call sites
//! because `clippy --all-targets` type-checks the binary without `cfg(test)`.
#![allow(dead_code)]

mod audio;
mod dextio;
mod exit;
mod gptpsvc;
mod log;
mod pcap;
#[cfg(test)]
mod pipeline_tests;
mod realdext;
mod report;
mod stack;
mod txsched;
mod validate;
mod wav;

/// The `firmware_version` we report in our own ENTITY descriptor. A controller
/// shows this to the user and keys cache invalidation on it alongside
/// `entity_model_id`, so it must change when the build does.
pub const FIRMWARE_VERSION: &str = concat!("phase1b-", env!("CARGO_PKG_VERSION"));

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::exit::Exit;

/// The AVB listener daemon.
#[derive(Parser, Debug)]
#[command(name = "avb-daemon", version, about = "AVB listener daemon (Phase 1a)")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// ADP/gPTP discovery timeout in seconds (1–600).
    #[arg(long, global = true, default_value_t = 30)]
    discover_timeout: u64,

    /// Link-up timeout in seconds (1–600).
    #[arg(long, global = true, default_value_t = 10)]
    link_timeout: u64,

    /// Log level: error|warn|info|debug.
    #[arg(long, global = true, default_value = "info")]
    log_level: String,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Full stack until SIGINT or N seconds; JSON summary on exit.
    Run {
        #[arg(long)]
        seconds: Option<u64>,
        #[arg(long)]
        talker: Option<String>,
        #[arg(long)]
        allow_partial: bool,
        #[arg(long, default_value_t = 1)]
        status_interval: u64,
        #[arg(long)]
        json_out: Option<PathBuf>,
        #[arg(long)]
        capture: Option<PathBuf>,
    },
    /// Data path + ADP + AECP walk only; write the 16R AEM to JSON.
    AemDump {
        #[arg(long, default_value = "phase1a-aem-16r.json")]
        out: PathBuf,
        #[arg(long)]
        talker: Option<String>,
    },
    /// Data path + gPTP only; exit 6 if no gPTP peer within the timeout.
    Gptp {
        #[arg(long)]
        seconds: u64,
        #[arg(long)]
        json_out: Option<PathBuf>,
    },
    /// Full stack; per-stream counter table each interval.
    Streams {
        #[arg(long)]
        seconds: u64,
        #[arg(long, default_value_t = 1)]
        interval: u64,
        #[arg(long)]
        json_out: Option<PathBuf>,
    },
    /// Full stack + recorder; write a WAV plus sidecar JSONs.
    Record {
        #[arg(long)]
        seconds: u64,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        talker: Option<String>,
        #[arg(long)]
        allow_partial: bool,
        #[arg(long, default_value_t = 30)]
        start_timeout: u64,
        #[arg(long)]
        summary_out: Option<PathBuf>,
    },
    /// Data path only, no TX, no filter: control ring to a pcap.
    Capture {
        #[arg(long)]
        seconds: u64,
        #[arg(long)]
        out: PathBuf,
    },
    /// Full stack + TALKER: play audio OUT to the 16R from the output ring
    /// (Phase 1b-2). The CoreAudio plug-in's output stream fills the ring; this
    /// loop packetizes it as AAF and transmits to the 16R (as listener).
    Play {
        #[arg(long)]
        seconds: u64,
        /// The 16R entity id to send to (as the listener). Auto-discovered when
        /// omitted, like `record`'s talker discovery.
        #[arg(long)]
        listener: Option<String>,
        /// Which of the 16R's STREAM_INPUT sinks (listener_unique_id) to bind.
        #[arg(long, default_value_t = 0)]
        listener_uid: u16,
        #[arg(long)]
        allow_partial: bool,
        #[arg(long, default_value_t = 30)]
        start_timeout: u64,
        #[arg(long)]
        summary_out: Option<PathBuf>,
    },
}

fn run() -> Result<(), Exit> {
    let cli = Cli::parse();
    let level = crate::log::Level::parse(&cli.log_level).ok_or(Exit::Usage)?;
    validate::validate_timeout(cli.discover_timeout)?;
    validate::validate_timeout(cli.link_timeout)?;

    let opts = stack::GlobalOpts {
        discover_timeout: cli.discover_timeout,
        link_timeout: cli.link_timeout,
        level,
    };

    match cli.command {
        Command::Run {
            seconds,
            talker,
            allow_partial,
            status_interval,
            json_out,
            capture,
        } => {
            if let Some(s) = seconds {
                validate::validate_seconds(s)?;
            }
            let talker = talker.as_deref().map(validate::parse_talker).transpose()?;
            stack::run(
                &opts,
                seconds,
                talker,
                allow_partial,
                status_interval,
                json_out,
                capture,
            )
        }
        Command::AemDump { out, talker } => {
            validate::validate_out_path(&out)?;
            let talker = talker.as_deref().map(validate::parse_talker).transpose()?;
            stack::aem_dump(&opts, &out, talker)
        }
        Command::Gptp { seconds, json_out } => {
            validate::validate_seconds(seconds)?;
            stack::gptp(&opts, seconds, json_out)
        }
        Command::Streams {
            seconds,
            interval,
            json_out,
        } => {
            validate::validate_seconds(seconds)?;
            validate::validate_timeout(interval)?;
            stack::streams(&opts, seconds, interval, json_out)
        }
        Command::Record {
            seconds,
            out,
            talker,
            allow_partial,
            start_timeout,
            summary_out,
        } => {
            validate::validate_seconds(seconds)?;
            validate::validate_timeout(start_timeout)?;
            validate::validate_out_path(&out)?;
            let talker = talker.as_deref().map(validate::parse_talker).transpose()?;
            stack::record(
                &opts,
                seconds,
                &out,
                talker,
                allow_partial,
                start_timeout,
                summary_out,
            )
        }
        Command::Capture { seconds, out } => {
            validate::validate_seconds(seconds)?;
            validate::validate_out_path(&out)?;
            stack::capture(&opts, seconds, &out)
        }
        Command::Play {
            seconds,
            listener,
            listener_uid,
            allow_partial,
            start_timeout,
            summary_out,
        } => {
            validate::validate_seconds(seconds)?;
            validate::validate_timeout(start_timeout)?;
            let listener = listener
                .as_deref()
                .map(validate::parse_talker)
                .transpose()?;
            stack::play(
                &opts,
                seconds,
                listener,
                listener_uid,
                allow_partial,
                start_timeout,
                summary_out,
            )
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::from(Exit::Success.code() as u8),
        Err(e) => {
            eprintln!(
                "avb-daemon[main]: error: {} (exit {})",
                e.reason(),
                e.code()
            );
            ExitCode::from(e.code() as u8)
        }
    }
}
