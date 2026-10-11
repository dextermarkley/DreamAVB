// SPDX-License-Identifier: MIT OR Apache-2.0

//! The sans-IO receive interface shared by the protocol crates:
//! `handle_rx(now, frame, meta) -> Result<(), DropReason>`.

use crate::time::{GptpNs, MachTicks};

/// The current time as the caller knows it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Now {
    pub mach: MachTicks,
    /// gPTP time, once the clock model has one.
    pub gptp: Option<GptpNs>,
}

impl Now {
    pub const fn mach_only(mach: MachTicks) -> Self {
        Self { mach, gptp: None }
    }
}

/// Per-frame receive metadata (from the dext's RX slot header, or synthesized
/// for software loopback).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RxMeta {
    /// mach time the dext harvested the descriptor.
    pub harvest: MachTicks,
    /// NIC SYSTIM ns of reception, when the NIC timestamped the frame.
    pub hw_ts: Option<u64>,
    /// Dext clock generation at harvest (`AvbRxSlot.clock_gen`).
    pub clock_gen: u32,
    /// Harvested inside a SYSTIM step's settling window: `hw_ts` may belong
    /// to either timebase and must not be used for timing.
    pub step_ambig: bool,
    /// TCI the NIC stripped into the descriptor (frame bytes carry no tag).
    pub desc_vlan_tci: Option<u16>,
    /// Our own transmitted frame, delivered locally (design §5.2).
    pub loopback: bool,
}

/// Why a received frame was not used. Each protocol crate counts these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DropReason {
    /// Shorter than the fixed part of the header being parsed.
    TooShort,
    /// A field holds a value the encoding forbids (e.g. a double VLAN tag).
    Malformed,
    /// EtherType not handled by this receiver.
    WrongEtherType,
    /// Destination MAC not handled by this receiver.
    WrongDestination,
    /// Message/subtype/version this implementation does not handle.
    Unsupported,
    /// Well-formed but addressed to another entity/port/stream.
    NotForUs,
    /// Our own frame (e.g. a looped-back advertisement).
    OwnFrame,
    /// Out of sequence or older than state already held.
    Stale,
    /// Timestamp unusable because of a SYSTIM step (`step_ambig`).
    StepAmbiguous,
    /// A value outside the accepted range.
    OutOfRange,
}

impl DropReason {
    /// Stable snake_case name for counters and logs.
    pub const fn name(self) -> &'static str {
        match self {
            DropReason::TooShort => "too_short",
            DropReason::Malformed => "malformed",
            DropReason::WrongEtherType => "wrong_ethertype",
            DropReason::WrongDestination => "wrong_destination",
            DropReason::Unsupported => "unsupported",
            DropReason::NotForUs => "not_for_us",
            DropReason::OwnFrame => "own_frame",
            DropReason::Stale => "stale",
            DropReason::StepAmbiguous => "step_ambiguous",
            DropReason::OutOfRange => "out_of_range",
        }
    }
}

impl std::fmt::Display for DropReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}
