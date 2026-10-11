// SPDX-License-Identifier: MIT OR Apache-2.0

//! avb-probe library: the pure logic behind the Phase 0 hardware gates.
//!
//! `abi`, `freq`, `iokit` and `nic` now live in the avb-dext crate and are
//! re-exported here so the probe's paths are unchanged. Everything except
//! `iokit` is deterministic and unit-tested without hardware.

pub use avb_dext::{abi, freq, iokit, nic};

pub mod ptp;
pub mod report;
pub mod stats;
