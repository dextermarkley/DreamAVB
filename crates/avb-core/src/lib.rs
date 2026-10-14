// SPDX-License-Identifier: MIT OR Apache-2.0

//! avb-core: std-only primitives shared by the Phase 1a protocol crates
//! (design §6.1, §5.1).
//!
//! Nothing here does I/O, spawns threads or reads a clock: time is injected
//! as [`MachTicks`] / [`GptpNs`] values and converted with a [`Timebase`] the
//! caller supplies.

#![deny(clippy::indexing_slicing)]

pub mod counter;
pub mod eth;
pub mod mac;
pub mod outbox;
pub mod rx;
pub mod seqlock;
pub mod time;

pub use counter::Counter;
pub use eth::{EthFrame, EthHeader, VlanSource, VlanTag};
pub use mac::{
    is_avdecc_dest, is_maap_dynamic, maap_stream_dest, Eui64, MacAddr, StreamId, AVDECC_MULTICAST,
    MAAP_DYNAMIC_COUNT, MAAP_DYNAMIC_FIRST, MAAP_DYNAMIC_LAST,
};
pub use outbox::{FrameTooLong, Outbox, TxFrame, TxFull};
pub use rx::{DropReason, Now, RxMeta};
pub use seqlock::{Contended, SeqLock, SeqWords};
pub use time::{ext32, GptpNs, MachTicks, Timebase};
