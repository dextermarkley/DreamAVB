// SPDX-License-Identifier: MIT OR Apache-2.0

//! avb-dext: the user-space side of the AVB dext user-client ABI (v3).
//!
//! - `abi`: hand mirror of driver/AVBShared.h (sizes/offsets asserted in tests);
//! - `iokit`: `Device`, one typed method per selector, plus memory mapping;
//! - `shm`: shared-memory RX ring consumer (`RxRings`, `RxSlotRef`, `ConsumerTails`);
//! - `nic`, `freq`: per-NIC profiles and frequency-register math.
//!
//! Everything except `iokit` is pure and unit-tested without hardware.

pub mod abi;
pub mod freq;
pub mod iokit;
pub mod nic;
pub mod shm;
