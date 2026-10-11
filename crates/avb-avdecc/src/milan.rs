// SPDX-License-Identifier: MIT OR Apache-2.0

//! Milan v1.2 specifics: the Vendor-Unique MVU protocol id, GET_MILAN_INFO,
//! and the probing-status enum used in GET_STREAM_INFO (Milan v1.2 §5.4.2.10,
//! §5.4.4). Named constants with citations, pinned by tests.

/// MVU (Milan Vendor Unique) protocol id, Milan v1.2 §5.4.4.1: the 6-byte
/// `protocol_id` carried in a VENDOR_UNIQUE AECPDU, `00-1B-C5-0A-C1-00`.
pub const MVU_PROTOCOL_ID: [u8; 6] = [0x00, 0x1B, 0xC5, 0x0A, 0xC1, 0x00];

/// MVU command types (Milan v1.2 §5.4.4.2). GET_MILAN_INFO is command 0x0000.
pub mod mvu_cmd {
    pub const GET_MILAN_INFO: u16 = 0x0000;
}

/// Milan `protocol_version` reported by GET_MILAN_INFO (Milan v1.2 §5.4.4.3).
pub const MILAN_PROTOCOL_VERSION: u32 = 1;

/// `probing_status` in the Milan GET_STREAM_INFO response (Milan v1.2
/// §5.4.2.10, Table 15).
pub mod probing_status {
    pub const DISABLED: u8 = 0;
    pub const PASSIVE: u8 = 1;
    pub const ACTIVE: u8 = 2;
    pub const COMPLETED: u8 = 3;
}

/// Milan GET_STREAM_INFO `flags_ex` bits (Milan v1.2 §5.4.2.10).
pub mod stream_info_flags_ex {
    /// The listener has declared (is registering) the stream.
    pub const REGISTERING: u32 = 1 << 0;
}

/// `flags` in the (Milan-extended) GET_STREAM_INFO response (IEEE 1722.1-2021
/// §7.4.16, Table 7.148; Milan v1.2 §5.4.2.10).
pub mod stream_info_flags {
    pub const CLASS_B: u32 = 1 << 0;
    pub const CLASS_A: u32 = 1 << 1;
    pub const SUPPORTS_ENCRYPTED: u32 = 1 << 2;
    pub const STREAM_ID_VALID: u32 = 1 << 3;
    pub const STREAM_FORMAT_VALID: u32 = 1 << 4;
    pub const STREAM_DEST_MAC_VALID: u32 = 1 << 5;
    pub const MSRP_FAILURE_VALID: u32 = 1 << 6;
    pub const STREAM_VLAN_ID_VALID: u32 = 1 << 7;
    pub const CONNECTED: u32 = 1 << 27;
    pub const MSRP_ACC_LAT_VALID: u32 = 1 << 28;
    pub const FAST_CONNECT: u32 = 1 << 30;
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn mvu_protocol_id_pinned() {
        assert_eq!(MVU_PROTOCOL_ID, [0x00, 0x1B, 0xC5, 0x0A, 0xC1, 0x00]);
        assert_eq!(mvu_cmd::GET_MILAN_INFO, 0x0000);
        assert_eq!(MILAN_PROTOCOL_VERSION, 1);
    }

    #[test]
    fn probing_and_flags_pinned() {
        assert_eq!(probing_status::DISABLED, 0);
        assert_eq!(probing_status::COMPLETED, 3);
        assert_eq!(stream_info_flags_ex::REGISTERING, 1);
        assert_eq!(stream_info_flags::CONNECTED, 1 << 27);
        assert_eq!(stream_info_flags::CLASS_A, 1 << 1);
    }
}
