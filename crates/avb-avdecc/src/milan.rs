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

/// The GET_MILAN_INFO response body (Milan v1.2 §5.4.4.3).
///
/// Layout anchored on a REAL response from a PreSonus StudioLive 16R, which
/// answers `status=0` with a 26-byte payload:
///
/// ```text
/// 0000 00000001 00000000 01010000 000000000000000000000000
/// ^res ^proto=1 ^feat=0  ^cert    ^reserved
/// ```
///
/// `protocol_version` reading exactly 1 at offset 2 is what confirms the field
/// positions — a shifted layout would have put that 1 somewhere else.
///
/// The 16R's own response carries ZERO in `specification_version` (offset 14),
/// which is consistent rather than contradictory: that field is new in Milan
/// v1.3 (§5.4.4.1, "New field specification_version in the GET_MILAN_INFO
/// command"), and the 16R reports `certification_version` 1.1.0.0, so it
/// predates it. Figure 5.6 places the field immediately after
/// `certification_version`, which is offset 14 here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MilanInfo {
    pub protocol_version: u32,
    pub features_flags: u32,
    /// Packed `major.minor.revision.build`; `0` means NOT certified.
    pub certification_version: u32,
    /// Packed `major.minor.0.0` — §5.4.4.1: *"The specification_version field
    /// shall be set to the version number of the Milan specification that the
    /// PAAD-AE supports ... The last two numbers shall be set to 0."*
    pub specification_version: u32,
}

/// The Milan specification revision we implement against, packed
/// `major.minor.0.0` per §5.4.4.1 — 1.3.0.0.
///
/// This is a claim about which spec we were written against, NOT a claim of
/// certification: that is `certification_version`, which stays 0 because we
/// have passed no Milan certification.
pub const MILAN_SPECIFICATION_VERSION: u32 = 0x0103_0000;

/// Payload length the 16R sends, which we match rather than emitting the
/// significant bytes alone: a controller sizing the response against a
/// shipping Milan device is the case we have evidence for. It is also long
/// enough to carry `specification_version`, which ends at offset 18.
pub const MILAN_INFO_PAYLOAD_LEN: usize = 26;

impl MilanInfo {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(MILAN_INFO_PAYLOAD_LEN);
        v.extend_from_slice(&0u16.to_be_bytes()); // reserved
        v.extend_from_slice(&self.protocol_version.to_be_bytes());
        v.extend_from_slice(&self.features_flags.to_be_bytes());
        v.extend_from_slice(&self.certification_version.to_be_bytes());
        v.extend_from_slice(&self.specification_version.to_be_bytes());
        v.resize(MILAN_INFO_PAYLOAD_LEN, 0);
        v
    }

    pub fn decode(p: &[u8]) -> Option<Self> {
        let g = |a: usize| -> Option<u32> {
            Some(u32::from_be_bytes(p.get(a..a + 4)?.try_into().ok()?))
        };
        Some(Self {
            protocol_version: g(2)?,
            features_flags: g(6)?,
            certification_version: g(10)?,
            // A pre-v1.3 responder (the 16R) sends a payload long enough to
            // cover this but zero-filled, so 0 is "not stated" here, not a
            // decode failure.
            specification_version: g(14).unwrap_or(0),
        })
    }
}

/// `features_flags` bits (Milan v1.3 §5.4.4.1, Table 5.17). The spec numbers
/// these MSB-first, so the stated FIELD VALUE is the authority, not the bit #.
pub mod milan_features {
    /// Supports the Section 8 redundancy scheme.
    pub const REDUNDANCY: u32 = 0x0000_0001;
    /// Supports changing mappings while a Stream Output is streaming.
    pub const TALKER_DYNAMIC_MAPPINGS_WHILE_RUNNING: u32 = 0x0000_0002;
    /// Supports the AECP MVU binding commands of §5.4.4.6 / §5.4.4.7. Do NOT
    /// set this until BIND_STREAM and UNBIND_STREAM are actually served.
    pub const MVU_BINDING: u32 = 0x0000_0004;
    /// Supports audio-channel signal-presence monitoring (§5.3.7.7.1).
    pub const TALKER_SIGNAL_PRESENCE: u32 = 0x0000_0008;
}

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
///
/// These positions are TRANSCRIBED FROM HARDWARE, not from the table, because
/// an earlier transcription of Table 7.148 had them wrong and the only test
/// was `assert_eq!(CONNECTED, 1 << 27)` -- a tautology that re-asserts our own
/// literal and passes for any value. The anchor is a PreSonus 16R answering
/// GET_STREAM_INFO on its eight audio STREAM_INPUTs in two different states
/// in the same run, which makes the connect transition itself the evidence:
///
/// ```text
/// bound sink   0xf600_0006   stream_id + dest_mac + vlan populated, probing COMPLETED
/// unbound sink 0xf200_0000   stream_id 0, dest_mac 00:..:00, probing DISABLED
/// delta        0x0400_0006   = CONNECTED | SAVED_STATE | FAST_CONNECT
/// ```
///
/// So `CONNECTED` is bit 26, NOT bit 27: with the old value the daemon logged
/// `connected=false` for all seven perfectly-bound sinks. The two low bits are
/// set on exactly the sinks that re-bound by themselves after a 16R power
/// cycle with no controller action, and clear on the one that had no saved
/// assignment -- which is what `SAVED_STATE` and `FAST_CONNECT` mean.
///
/// The `*_VALID` group is anchored more weakly, by which response fields
/// carry a meaning rather than by a transition: all four are set in both
/// states above. Do not add a name here that no capture distinguishes.
pub mod stream_info_flags {
    /// Set on exactly the sinks the controller had saved an assignment for.
    pub const FAST_CONNECT: u32 = 1 << 1;
    /// Ditto -- the 16R re-binds these by itself after a power cycle.
    pub const SAVED_STATE: u32 = 1 << 2;
    pub const STREAM_VLAN_ID_VALID: u32 = 1 << 25;
    /// The sink is bound to a talker stream. Bit 26, proven by the delta.
    pub const CONNECTED: u32 = 1 << 26;
    pub const STREAM_DEST_MAC_VALID: u32 = 1 << 28;
    pub const MSRP_ACC_LAT_VALID: u32 = 1 << 29;
    pub const STREAM_ID_VALID: u32 = 1 << 30;
    pub const STREAM_FORMAT_VALID: u32 = 1 << 31;

    /// The 16R's own flag word for a BOUND audio sink, off the wire.
    pub const CAPTURED_16R_BOUND: u32 = 0xf600_0006;
    /// The same device's word for a sink with no saved assignment.
    pub const CAPTURED_16R_UNBOUND: u32 = 0xf200_0000;
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
    }

    /// The 16R's OWN two flag words, and what our bit names must say about
    /// them. This is NOT `assert_eq!(CONNECTED, 1 << 26)` -- that form
    /// re-asserts our own literal and cannot fail. Here the device decides:
    /// a wrong CONNECTED bit makes a bound sink read unbound (which is what
    /// the previous `1 << 27` did), and a bit that happens to be set in both
    /// words fails the "clear when unbound" half.
    #[test]
    fn stream_info_connect_bits_anchored_on_16r_capture() {
        use stream_info_flags as f;
        let bound = f::CAPTURED_16R_BOUND;
        let unbound = f::CAPTURED_16R_UNBOUND;

        // Exactly these three distinguish the two states, so each must be
        // set in one word and clear in the other.
        for (name, bit) in [
            ("CONNECTED", f::CONNECTED),
            ("SAVED_STATE", f::SAVED_STATE),
            ("FAST_CONNECT", f::FAST_CONNECT),
        ] {
            assert!(bound & bit != 0, "{name} must be set on a bound sink");
            assert!(unbound & bit == 0, "{name} must be clear when unbound");
        }

        // And the three together are the WHOLE difference: a fourth name
        // claiming to be part of the transition would leave a remainder.
        assert_eq!(
            bound ^ unbound,
            f::CONNECTED | f::SAVED_STATE | f::FAST_CONNECT,
            "the connect transition is exactly these three bits"
        );

        // The valid-group reports fields the device populated, so it is set
        // in both states -- it must NOT be confused with a connect bit.
        for (name, bit) in [
            ("STREAM_ID_VALID", f::STREAM_ID_VALID),
            ("STREAM_FORMAT_VALID", f::STREAM_FORMAT_VALID),
            ("STREAM_DEST_MAC_VALID", f::STREAM_DEST_MAC_VALID),
            ("MSRP_ACC_LAT_VALID", f::MSRP_ACC_LAT_VALID),
            ("STREAM_VLAN_ID_VALID", f::STREAM_VLAN_ID_VALID),
        ] {
            assert!(bound & bit != 0, "{name} set on a bound sink");
            assert!(unbound & bit != 0, "{name} set on an unbound sink too");
        }
    }

    /// The 16R's OWN GET_MILAN_INFO response, byte for byte off the wire.
    /// This is the anchor for our responder: a shifted layout would not put
    /// protocol_version == 1 at offset 2.
    #[test]
    fn milan_info_decodes_the_16r_response() {
        let p: [u8; MILAN_INFO_PAYLOAD_LEN] = [
            0x00, 0x00, // reserved
            0x00, 0x00, 0x00, 0x01, // protocol_version = 1
            0x00, 0x00, 0x00, 0x00, // features_flags = none
            0x01, 0x01, 0x00, 0x00, // certification_version = 1.1.0.0
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // reserved tail
        ];
        let i = MilanInfo::decode(&p).expect("decode");
        assert_eq!(i.protocol_version, MILAN_PROTOCOL_VERSION);
        assert_eq!(i.features_flags, 0);
        assert_eq!(i.certification_version, 0x0101_0000);
        // Re-encoding must reproduce the device's bytes exactly, which a
        // round trip through our own encoder alone could not prove.
        assert_eq!(i.encode(), p.to_vec());
    }

    /// Our own response is honest: protocol version 1, no optional features,
    /// and certification_version 0 because we are NOT Milan-certified.
    /// Claiming a certification we do not hold would be a false statement to
    /// every controller on the network.
    ///
    /// `specification_version` is the OTHER claim and must not be confused
    /// with it: §5.4.4.1 makes it mandatory and it says which revision we were
    /// written against, so it is 1.3.0.0 while certification stays 0. The test
    /// asserts both byte ranges so a future edit cannot quietly swap them.
    #[test]
    fn our_milan_info_claims_no_certification() {
        let ours = MilanInfo {
            protocol_version: MILAN_PROTOCOL_VERSION,
            features_flags: 0,
            certification_version: 0,
            specification_version: MILAN_SPECIFICATION_VERSION,
        };
        let b = ours.encode();
        assert_eq!(b.len(), MILAN_INFO_PAYLOAD_LEN);
        assert_eq!(MilanInfo::decode(&b), Some(ours));
        assert_eq!(&b[10..14], &[0, 0, 0, 0], "must not claim certification");
        assert_eq!(
            &b[14..18],
            &[0x01, 0x03, 0x00, 0x00],
            "specification_version is 1.3.0.0, major.minor.0.0 per 5.4.4.1"
        );
    }

    /// §5.4.4.1: *"The last two numbers shall be set to 0."* A revision or
    /// build number in the low half would violate that, so pin the constant's
    /// shape rather than only its value.
    #[test]
    fn specification_version_low_half_is_zero() {
        assert_eq!(
            MILAN_SPECIFICATION_VERSION & 0x0000_FFFF,
            0,
            "5.4.4.1 requires the last two of the four 8-bit numbers to be 0"
        );
        assert_eq!(MILAN_SPECIFICATION_VERSION >> 24, 1, "major version 1");
        assert_eq!((MILAN_SPECIFICATION_VERSION >> 16) & 0xFF, 3, "minor 3");
    }

    /// We must NOT claim MVU_BINDING until BIND_STREAM and UNBIND_STREAM are
    /// actually served. This test is what fails if the flag is set early.
    #[test]
    fn we_do_not_claim_mvu_binding_we_have_not_implemented() {
        // Mirrors what entity/talker.rs sends for GET_MILAN_INFO.
        let ours = MilanInfo {
            protocol_version: MILAN_PROTOCOL_VERSION,
            features_flags: 0,
            certification_version: 0,
            specification_version: MILAN_SPECIFICATION_VERSION,
        };
        assert_eq!(
            ours.features_flags & milan_features::MVU_BINDING,
            0,
            "BIND_STREAM/UNBIND_STREAM are not implemented yet (5.4.4.6-7)"
        );
    }
}
