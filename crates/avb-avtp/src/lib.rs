//! avb-avtp: sans-IO IEEE 1722-2016 AVTP pieces the listener needs (design
//! §10.1, §10.2).
//!
//! - [`common`]: EtherType, subtype codes, the common stream header.
//! - [`aaf`]: AAF PCM header decode; [`aaf::encode`] exists for test vectors
//!   only and is not used by the daemon (design §12).
//! - [`stream_format`]: the 64-bit AEM stream format (IEEE 1722.1-2021
//!   §7.3.2): AAF fields, CRF and IEC 61883 decode for the AEM dump, the nsr
//!   code table and [`StreamFormat::decodable`].
//!
//! Encodings come from the IEEE texts and are pinned by our own vectors; no
//! GPL/LGPL-derived code or tables.
#![deny(clippy::indexing_slicing)]

pub mod aaf;
pub mod common;
pub mod stream_format;

pub use aaf::{AafHeader, AafPdu};
pub use common::{StreamHeader, ETHERTYPE_AVTP};
pub use stream_format::{AafFormat, CrfFormat, Decoded, Iec61883Format, StreamFormat};
