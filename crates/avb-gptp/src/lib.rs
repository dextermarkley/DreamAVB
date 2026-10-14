// SPDX-License-Identifier: MIT OR Apache-2.0

//! avb-gptp: sans-IO, slave-only IEEE 802.1AS time-aware end station
//! (design §7).
//!
//! - [`msg`]: message codec. Announce/Sync/Follow_Up are decode-only.
//! - [`pdelay`]: NRR window, link delay, initiator/responder state, asCapable.
//! - [`stepgen`]: RX-generation classification and the TX-generation hold.
//! - [`servo`]: the PI servo and its ACQUIRE/TRACKING/LOCKED/HOLDOVER states.
//! - [`port`]: [`Port`], which ties them together behind the crate shape
//!   `handle_rx` / `poll` / `next_deadline`, plus TX-timestamp feedback and
//!   clock [`Action`]s for the daemon to execute.
//! - [`model`]: the gPTP ↔ mach regression ([`GptpMachModel`]).
//! - [`status`]: [`GptpStatus`] (serde).
//!
//! Nothing here reads a clock, spawns a thread or does I/O: time, frames,
//! timestamps and clock reads are injected by the caller.
#![deny(clippy::indexing_slicing)]

pub mod btca;
pub mod model;
pub mod msg;
pub mod pdelay;
pub mod port;
pub mod servo;
pub mod status;
pub mod stepgen;
mod wire;

#[cfg(test)]
mod port_tests;
#[cfg(test)]
mod sim;

pub use model::{ClockRead, GptpMachModel, ModelEstimator};
pub use msg::{ClockIdentity, PortIdentity};
pub use port::{Action, Port, PortConfig, PortState, TxTsResult};
pub use servo::ServoState;
pub use status::{GptpCounters, GptpStatus};
pub use stepgen::{classify_rx_gen, RxGen};

/// The portNumber of our single gPTP port (design §7.1).
pub const PORT_NUMBER: u16 = 1;
/// priority1 we report in AVB_INTERFACE / GET_AVB_INFO and announce.
///
/// 248, as Milan v1.3 §4.2.6.2.1 requires of a Grandmaster-Capable PAAD, and
/// exactly what the 16R announces. This was 255 until the master transmit
/// path and the time-source servo existed; raising it before them would have
/// advertised a role we could not perform.
///
/// Against the 16R this makes every merit rung a TIE -- priority1 248,
/// clockClass 248, accuracy 0x20, variance 0x436a, priority2 248 are
/// identical on both sides -- so the comparison falls through to
/// grandmasterIdentity, where the lower value wins. Ours is
/// 644bf0.fffe.1273b8 (from the I210's MAC) against the 16R's
/// 000a92.fffe.033ba9, so the 16R wins and keeps the clock. That is the
/// correct outcome, not a workaround: we are now a real BTCA contender, and
/// against a device with a higher clockIdentity we would take the role.
pub const PRIORITY1: u8 = 248;
/// priority2, BTCA rung 5. 248 is the 802.1AS default for a time-aware
/// system and is what the 16R announces.
pub const PRIORITY2: u8 = 248;

/// The `grandmasterClockQuality` we would announce.
///
/// Values transcribed from the 16R's own Announce (see
/// `docs/gptp-master-capture.md`): `clockClass` 248, `clockAccuracy` 0x20,
/// `offsetScaledLogVariance` 0x436a. 248 is the 802.1AS class for a clock
/// that is not synchronised to a primary reference, which is exactly what a
/// Mac's oscillator is — claiming a lower class would assert a traceable
/// reference we do not have and would win BTCA on a false premise.
pub const fn our_clock_quality() -> msg::ClockQuality {
    msg::ClockQuality {
        clock_class: 248,
        clock_accuracy: 0x20,
        offset_scaled_log_variance: 0x436a,
    }
}

/// Timestamp-point latencies: the TX timestamp is taken `tx_ns` before the
/// frame reaches the wire, the RX timestamp `rx_ns` after it arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatencyProfile {
    pub tx_ns: i64,
    pub rx_ns: i64,
}

impl LatencyProfile {
    /// I210 at 1000 Mb/s, Phase 0b values (docs/phase0b-i210-results.md G4;
    /// U-13: not independently verified): `tx_c = tx + 178`, `rx_c = rx − 448`.
    pub const I210_1000: LatencyProfile = LatencyProfile {
        tx_ns: 178,
        rx_ns: 448,
    };

    pub const fn tx_c(self, raw: u64) -> u64 {
        raw.saturating_add_signed(self.tx_ns)
    }

    pub const fn rx_c(self, raw: u64) -> u64 {
        raw.saturating_add_signed(-self.rx_ns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_profile() {
        let l = LatencyProfile::I210_1000;
        assert_eq!(l.tx_c(1000), 1178);
        assert_eq!(l.rx_c(1000), 552);
        assert_eq!(l.rx_c(10), 0);
    }
}
