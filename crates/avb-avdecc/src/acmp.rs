// SPDX-License-Identifier: MIT OR Apache-2.0

//! ACMP: AVDECC Connection Management Protocol (IEEE 1722.1-2021 §8; design
//! §9.2 binding, §9.4 listener SM).
//!
//! Every ACMPDU this crate emits is addressed to the AVDECC multicast
//! `91:E0:F0:01:00:00` ([`avb_core::AVDECC_MULTICAST`]); the endpoints are
//! identified by the ids in the PDU, never by the destination MAC.

use avb_core::{Eui64, MacAddr, StreamId};

use crate::constants::{acmp_status, subtype};
use crate::wire::{Rd, Wr};

/// ACMPDU wire length (IEEE 1722.1-2021 §8.2.1): control header (12) + 44.
pub const ACMPDU_LEN: usize = 56;
/// `control_data_length` of an ACMPDU.
pub const ACMP_CONTROL_DATA_LENGTH: u16 = 44;

/// Talker ACMP statuses that mean the stream cannot be transmitted alongside
/// the others (A-5; design §9.2). Each is cited to IEEE 1722.1-2021 Table 8.2.
pub const A5_RESOURCE_STATUSES: [u8; 4] = [
    acmp_status::TALKER_NO_BANDWIDTH, // Table 8.2: TALKER_NO_BANDWIDTH (5)
    acmp_status::TALKER_EXCLUSIVE,    // Table 8.2: TALKER_EXCLUSIVE (6)
    acmp_status::TALKER_DEST_MAC_FAIL, // Table 8.2: TALKER_DEST_MAC_FAIL (3)
    acmp_status::TALKER_NO_STREAM_INDEX, // Table 8.2: TALKER_NO_STREAM_INDEX (4)
];

/// True iff `status` classifies a stream as not simultaneously streamable
/// (§9.2 A-5). `LISTENER_EXCLUSIVE` and the rest are deliberately excluded.
pub fn is_a5_resource_status(status: u8) -> bool {
    A5_RESOURCE_STATUSES.contains(&status)
}

/// A decoded ACMPDU (IEEE 1722.1-2021 §8.2.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Acmpdu {
    pub message_type: u8,
    pub status: u8,
    pub stream_id: StreamId,
    pub controller_entity_id: Eui64,
    pub talker_entity_id: Eui64,
    pub listener_entity_id: Eui64,
    pub talker_unique_id: u16,
    pub listener_unique_id: u16,
    pub stream_dest_mac: MacAddr,
    pub connection_count: u16,
    pub sequence_id: u16,
    pub flags: u16,
    pub stream_vlan_id: u16,
}

impl Acmpdu {
    /// Decode the AVTP payload (after the EtherType). `None` on a short
    /// buffer or a non-ACMP subtype.
    pub fn decode(pdu: &[u8]) -> Option<Self> {
        let mut r = Rd::new(pdu);
        let st = r.u8()?; // byte0 = subtype
        let b1 = r.u8()?; // byte1 = sv(1)|version(3)|message_type(4)
        if st != subtype::ACMP {
            return None;
        }
        let message_type = b1 & 0x0F;
        let b2 = r.u8()?;
        let status = (b2 >> 3) & 0x1F;
        r.skip(1)?; // low byte of control_data_length
        let stream_id = StreamId::from_be_bytes(r.arr()?);
        let controller_entity_id = Eui64(r.arr()?);
        let talker_entity_id = Eui64(r.arr()?);
        let listener_entity_id = Eui64(r.arr()?);
        let talker_unique_id = r.u16()?;
        let listener_unique_id = r.u16()?;
        let stream_dest_mac = MacAddr(r.arr()?);
        let connection_count = r.u16()?;
        let sequence_id = r.u16()?;
        let flags = r.u16()?;
        let stream_vlan_id = r.u16()?;
        Some(Self {
            message_type,
            status,
            stream_id,
            controller_entity_id,
            talker_entity_id,
            listener_entity_id,
            talker_unique_id,
            listener_unique_id,
            stream_dest_mac,
            connection_count,
            sequence_id,
            flags,
            stream_vlan_id,
        })
    }

    /// Encode to the AVTP payload. Always transmitted to
    /// [`avb_core::AVDECC_MULTICAST`]; see [`dest_mac`].
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Wr::new();
        // IEEE 1722 common control header: byte0 = subtype; byte1 = message_type.
        w.u8(subtype::ACMP);
        w.u8(self.message_type & 0x0F);
        w.u8((self.status & 0x1F) << 3); // status(5) | cdl hi
        w.u8(ACMP_CONTROL_DATA_LENGTH as u8);
        w.put(&self.stream_id.to_be_bytes());
        w.put(&self.controller_entity_id.0);
        w.put(&self.talker_entity_id.0);
        w.put(&self.listener_entity_id.0);
        w.u16(self.talker_unique_id);
        w.u16(self.listener_unique_id);
        w.put(&self.stream_dest_mac.0);
        w.u16(self.connection_count);
        w.u16(self.sequence_id);
        w.u16(self.flags);
        w.u16(self.stream_vlan_id);
        w.u16(0); // reserved(16) (IEEE 1722.1-2021 §8.2.1)
        debug_assert_eq!(w.len(), ACMPDU_LEN);
        w.into_vec()
    }
}

/// The destination MAC for every ACMPDU this crate sends (§9).
pub fn dest_mac() -> MacAddr {
    avb_core::AVDECC_MULTICAST
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::constants::acmp_msg;

    fn sample() -> Acmpdu {
        Acmpdu {
            message_type: acmp_msg::CONNECT_RX_COMMAND,
            status: acmp_status::SUCCESS,
            stream_id: StreamId(0x000A_9203_3BA9_0001),
            controller_entity_id: Eui64::from_u64(0x644b_f0ff_fe12_73b8),
            talker_entity_id: Eui64::from_u64(0x000A_9203_3BA9_0000),
            listener_entity_id: Eui64::from_u64(0x644b_f0ff_fe12_73b8),
            talker_unique_id: 3,
            listener_unique_id: 1,
            stream_dest_mac: MacAddr([0x91, 0xE0, 0xF0, 0x00, 0x12, 0x34]),
            connection_count: 1,
            sequence_id: 0x1234,
            flags: 0,
            stream_vlan_id: 2,
        }
    }

    #[test]
    fn acmpdu_round_trip() {
        let a = sample();
        let bytes = a.encode();
        assert_eq!(bytes.len(), ACMPDU_LEN);
        assert_eq!(bytes.first(), Some(&subtype::ACMP)); // byte0 = subtype
        assert_eq!(bytes.get(1), Some(&acmp_msg::CONNECT_RX_COMMAND)); // byte1 = message_type
        assert_eq!(Acmpdu::decode(&bytes), Some(a));
    }

    #[test]
    fn every_acmpdu_targets_avdecc_multicast() {
        // The crate never varies the dest MAC; this pins §9's rule.
        assert_eq!(dest_mac(), MacAddr([0x91, 0xE0, 0xF0, 0x01, 0x00, 0x00]));
        assert_eq!(dest_mac(), avb_core::AVDECC_MULTICAST);
    }

    #[test]
    fn acmpdu_rejects_non_acmp() {
        let mut bytes = sample().encode();
        if let Some(b) = bytes.get_mut(0) {
            *b = subtype::ADP; // byte0 = subtype
        }
        assert_eq!(Acmpdu::decode(&bytes), None);
        assert_eq!(Acmpdu::decode(&[0u8; 4]), None);
    }

    #[test]
    fn a5_classification() {
        assert!(is_a5_resource_status(acmp_status::TALKER_NO_BANDWIDTH));
        assert!(is_a5_resource_status(acmp_status::TALKER_EXCLUSIVE));
        assert!(is_a5_resource_status(acmp_status::TALKER_DEST_MAC_FAIL));
        assert!(is_a5_resource_status(acmp_status::TALKER_NO_STREAM_INDEX));
        // A non-member such as LISTENER_EXCLUSIVE is not classified.
        assert!(!is_a5_resource_status(acmp_status::LISTENER_EXCLUSIVE));
        assert!(!is_a5_resource_status(acmp_status::SUCCESS));
        assert!(!is_a5_resource_status(acmp_status::NOT_CONNECTED));
    }
}
