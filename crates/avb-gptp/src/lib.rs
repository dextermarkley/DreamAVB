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
/// priority1 we report in AVB_INTERFACE / GET_AVB_INFO. It never appears on
/// the wire: we send no Announce (design §7.1, user decision).
pub const PRIORITY1: u8 = 255;

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
