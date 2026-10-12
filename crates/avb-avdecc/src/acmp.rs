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

/// What our talker direction knows about the one stream it offers, so it can
/// answer a listener's ACMP commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TalkerStream {
    /// Our own entity id.
    pub our_id: Eui64,
    /// Our STREAM_OUTPUT's `unique_id` — the listener names this when it
    /// connects, so a command for any other id is TALKER_NO_STREAM_INDEX.
    pub our_unique_id: u16,
    pub stream_id: StreamId,
    pub stream_dest_mac: MacAddr,
    pub stream_vlan_id: u16,
    /// How many listeners are currently bound (reported, not enforced).
    pub connection_count: u16,
}

/// Answer an ACMP command addressed to us AS THE TALKER, or `None` when the
/// PDU is not ours to answer (a response, a different entity, a listener-side
/// command).
///
/// Without this a Milan listener cannot bind our stream at all. The sequence is
/// controller -> `CONNECT_RX_COMMAND` -> listener, then **listener ->
/// `CONNECT_TX_COMMAND` -> talker**, and only after the talker's
/// `CONNECT_TX_RESPONSE` does the listener declare MSRP Listener Ready. We are
/// both controller and talker here, so sending `CONNECT_RX_COMMAND` and never
/// answering `CONNECT_TX_COMMAND` left the 16R waiting on us: it never
/// declared Listener Ready and every PDU we sent was discarded, which on the
/// wire is indistinguishable from a transmit fault.
///
/// A command naming an unknown talker id or stream index is REFUSED with the
/// specific status rather than answered with our own stream, so a listener
/// that asks for something we do not have is told so instead of being handed
/// a stream it did not request.
pub fn talker_response(cmd: &Acmpdu, s: TalkerStream) -> Option<Acmpdu> {
    use crate::constants::acmp_msg as m;

    // Only the three talker-side commands. Responses are someone else's
    // answer and listener-side commands are not ours.
    let reply_type = match cmd.message_type {
        m::CONNECT_TX_COMMAND => m::CONNECT_TX_RESPONSE,
        m::DISCONNECT_TX_COMMAND => m::DISCONNECT_TX_RESPONSE,
        m::GET_TX_STATE_COMMAND => m::GET_TX_STATE_RESPONSE,
        _ => return None,
    };

    // ACMP routes talker-side commands by talker_entity_id.
    if cmd.talker_entity_id != s.our_id {
        return None;
    }

    // Echo the command's own routing fields back — a listener matches a
    // response to its command by (sequence_id, ids), so inventing any of them
    // makes the response unmatchable.
    let mut r = Acmpdu {
        message_type: reply_type,
        status: acmp_status::SUCCESS,
        ..*cmd
    };

    if cmd.talker_unique_id != s.our_unique_id {
        r.status = acmp_status::TALKER_NO_STREAM_INDEX;
        return Some(r);
    }

    // The addressing the listener needs in order to receive us: our stream id,
    // the destination MAC we actually transmit to, and the VLAN. These must be
    // the same values our MSRP Talker Advertise declares, or the listener
    // reserves one stream and listens for another.
    r.stream_id = s.stream_id;
    r.stream_dest_mac = s.stream_dest_mac;
    r.stream_vlan_id = s.stream_vlan_id;
    r.connection_count = s.connection_count;
    Some(r)
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

    // --- Talker-side responder --------------------------------------------

    const OUR_ID: u64 = 0x644b_f0ff_fe12_73b8;

    fn our_stream() -> TalkerStream {
        TalkerStream {
            our_id: Eui64::from_u64(OUR_ID),
            our_unique_id: 0,
            stream_id: StreamId(0x644b_f012_73b8_0000),
            stream_dest_mac: MacAddr([0x91, 0xE0, 0xF0, 0x01, 0x00, 0x01]),
            stream_vlan_id: 2,
            connection_count: 1,
        }
    }

    /// The 16R's CONNECT_TX_COMMAND, as it arrives after our
    /// CONNECT_RX_COMMAND: it names US as the talker and itself as listener.
    fn connect_tx_from_16r() -> Acmpdu {
        Acmpdu {
            message_type: acmp_msg::CONNECT_TX_COMMAND,
            status: acmp_status::SUCCESS,
            stream_id: StreamId(0),
            controller_entity_id: Eui64::from_u64(OUR_ID),
            talker_entity_id: Eui64::from_u64(OUR_ID),
            listener_entity_id: Eui64::from_u64(0x000a_92ff_fe03_3ba9),
            talker_unique_id: 0,
            listener_unique_id: 1,
            stream_dest_mac: MacAddr([0; 6]),
            connection_count: 0,
            sequence_id: 0x4321,
            flags: 0,
            stream_vlan_id: 0,
        }
    }

    /// The command this answers is the one the 16R was waiting on: without a
    /// CONNECT_TX_RESPONSE it never declares MSRP Listener Ready.
    #[test]
    fn connect_tx_is_answered_with_our_stream_addressing() {
        let r = talker_response(&connect_tx_from_16r(), our_stream()).expect("a response");
        assert_eq!(r.message_type, acmp_msg::CONNECT_TX_RESPONSE);
        assert_eq!(r.status, acmp_status::SUCCESS);
        // The addressing the listener needs, which the command did not carry.
        assert_eq!(r.stream_id, StreamId(0x644b_f012_73b8_0000));
        assert_eq!(
            r.stream_dest_mac,
            MacAddr([0x91, 0xE0, 0xF0, 0x01, 0x00, 0x01])
        );
        assert_eq!(r.stream_vlan_id, 2);
        // Routing fields echoed, or the listener cannot match the response.
        assert_eq!(r.sequence_id, 0x4321);
        assert_eq!(r.listener_unique_id, 1);
        assert_eq!(r.listener_entity_id, Eui64::from_u64(0x000a_92ff_fe03_3ba9));
        assert_eq!(r.talker_entity_id, Eui64::from_u64(OUR_ID));
    }

    #[test]
    fn disconnect_and_get_state_are_answered_too() {
        for (cmd_t, resp_t) in [
            (
                acmp_msg::DISCONNECT_TX_COMMAND,
                acmp_msg::DISCONNECT_TX_RESPONSE,
            ),
            (
                acmp_msg::GET_TX_STATE_COMMAND,
                acmp_msg::GET_TX_STATE_RESPONSE,
            ),
        ] {
            let mut c = connect_tx_from_16r();
            c.message_type = cmd_t;
            let r = talker_response(&c, our_stream()).expect("a response");
            assert_eq!(r.message_type, resp_t);
            assert_eq!(r.status, acmp_status::SUCCESS);
            assert_eq!(r.stream_id, StreamId(0x644b_f012_73b8_0000));
        }
    }

    /// A stream index we do not have is refused with the specific status, not
    /// answered with our own stream.
    #[test]
    fn an_unknown_stream_index_is_refused_not_substituted() {
        let mut c = connect_tx_from_16r();
        c.talker_unique_id = 7;
        let r = talker_response(&c, our_stream()).expect("a response");
        assert_eq!(r.status, acmp_status::TALKER_NO_STREAM_INDEX);
        assert_ne!(r.stream_id, StreamId(0x644b_f012_73b8_0000));
        assert_eq!(r.talker_unique_id, 7);
    }

    /// Not ours to answer: a command for another talker, our own responses
    /// coming back, and listener-side commands.
    #[test]
    fn pdus_that_are_not_ours_are_ignored() {
        let mut other = connect_tx_from_16r();
        other.talker_entity_id = Eui64::from_u64(0x000a_92ff_fe03_3ba9);
        assert_eq!(talker_response(&other, our_stream()), None);

        for t in [
            acmp_msg::CONNECT_TX_RESPONSE,
            acmp_msg::CONNECT_RX_COMMAND,
            acmp_msg::CONNECT_RX_RESPONSE,
            acmp_msg::GET_RX_STATE_COMMAND,
            acmp_msg::DISCONNECT_RX_COMMAND,
        ] {
            let mut c = connect_tx_from_16r();
            c.message_type = t;
            assert_eq!(talker_response(&c, our_stream()), None, "type {t:#04x}");
        }
    }

    /// The response must survive a wire round trip, since that is how the
    /// listener actually reads it.
    #[test]
    fn the_response_round_trips_on_the_wire() {
        let r = talker_response(&connect_tx_from_16r(), our_stream()).expect("a response");
        assert_eq!(Acmpdu::decode(&r.encode()), Some(r));
    }
}
