//! avb-probe library: the pure logic behind the Phase 0 hardware gates.
//!
//! Everything here except `iokit` is deterministic and unit-tested without
//! hardware. `iokit` is a thin wrapper over IOConnectCallMethod.

pub mod abi;
pub mod freq;
pub mod iokit;
pub mod nic;
pub mod ptp;
pub mod report;
pub mod stats;
