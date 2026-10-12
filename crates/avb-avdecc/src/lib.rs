// SPDX-License-Identifier: MIT OR Apache-2.0

//! avb-avdecc: sans-IO IEEE 1722.1-2021 / Milan v1.2 AVDECC for the Phase 1a
//! listener (design §9).
//!
//! - [`constants`], [`milan`]: every protocol code as a cited, pinned const.
//! - [`adp`]: ADPDU codec and the discovery entity table.
//! - [`acmp`]: ACMPDU codec; every emitted ACMPDU goes to
//!   `91:E0:F0:01:00:00`; the A-5 resource-status list.
//! - [`aecp`]: AEM + MVU codecs and the controller in-flight/retry engine.
//! - [`aem`]: descriptor codecs, the decoded model, and the AEM dump JSON.
//! - [`derive`]: the ONLY constructor of our listener model from the 16R AEM.
//! - [`controller`]: discovery, enumeration and binding.
//! - [`entity`]: our listener entity responder and the ACMP listener SM.
//!
//! No crate here does I/O, reads a clock or spawns a thread: time enters as
//! `now_ns` values and frames enter as byte slices (design §6.1).

#![deny(clippy::indexing_slicing)]

pub mod acmp;
pub mod adp;
pub mod aecp;
pub mod aem;
pub mod constants;
pub mod controller;
pub mod derive;
pub mod entity;
pub mod milan;
pub(crate) mod wire;

pub use acmp::{talker_response, Acmpdu, TalkerStream, A5_RESOURCE_STATUSES};
pub use adp::{Adpdu, EntityTable};
pub use aecp::{AecpEngine, AemPdu, MvuPdu};
pub use controller::{BindOutcome, BindTarget, Binder};
pub use derive::{
    first_audio_stream, listener_model, talker_output_format, DeriveError, ListenerModel,
    TalkerFormatError,
};
pub use entity::{ListenerEntity, ListenerSm};
