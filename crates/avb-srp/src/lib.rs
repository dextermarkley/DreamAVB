//! avb-srp: sans-IO IEEE 802.1Q-2018 MRP end-station participant for MSRP
//! and MVRP (design §8).
//!
//! - [`mrp`]: applicant / registrar / LeaveAll state tables (§10.7), timers.
//! - [`pdu`]: MRPDU vector codec (§10.8, §35.2.2).
//! - [`attr`]: MSRP (Talker Advertise / Failed, Listener, Domain) and MVRP
//!   (VID) attribute values, [`A5_FAILURE_CODES`].
//! - [`participant`]: [`Participant`] (`handle_rx` / `poll` /
//!   `next_deadline`, `declare` / `withdraw` / `withdraw_all`, [`SrpEvent`]s
//!   and the MSRP event log).
//!
//! No GPL/LGPL-derived code or tables: the state tables are written from the
//! 802.1Q text and pinned by the tests next to them.
#![deny(clippy::indexing_slicing)]

pub mod attr;
pub mod mrp;
pub mod participant;
pub mod pdu;
mod wire;

#[cfg(test)]
mod participant_tests;

pub use attr::{
    is_a5_failure_code, App, AttrKey, Attribute, Domain, Listener, ListenerDecl, Talker,
    TalkerAdvertise, TalkerFailed, A5_FAILURE_CODES, DEFAULT_DOMAIN,
};
pub use participant::{
    LogEntry, Participant, ParticipantConfig, Registration, SrpCounters, SrpEvent, SrpStatus,
};
