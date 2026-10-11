//! Every AVDECC command / status / descriptor / message-type code the design
//! relies on, as a named `const` with an IEEE 1722.1-2021 or Milan v1.2
//! citation. Pinned by [`tests`] so a typo cannot drift silently (design §9,
//! review checklist "no magic numbers").
//!
//! Codes come from IEEE Std 1722.1-2021 §7.4 (AEM commands), Table 7.126
//! (command types), §7.4.* (AEM status), §7.2 (descriptor types), §8
//! (ACMP) and §6 (ADP), plus Milan v1.2 §5. Vendor/MVU values are cited at
//! their definition.

/// AVTP control subtypes (IEEE 1722.1-2021 §4.4.1 / §6, §8, §9).
pub mod subtype {
    /// AVDECC Discovery Protocol (ADP), IEEE 1722.1-2021 §6.2.1.
    pub const ADP: u8 = 0xFA;
    /// AVDECC Enumeration and Control Protocol (AECP), §9.2.1.
    pub const AECP: u8 = 0xFB;
    /// AVDECC Connection Management Protocol (ACMP), §8.2.1.
    pub const ACMP: u8 = 0xFC;
}

/// ADP `message_type` values (IEEE 1722.1-2021 §6.2.1.5, Table 6.1).
pub mod adp_msg {
    pub const ENTITY_AVAILABLE: u8 = 0x00;
    pub const ENTITY_DEPARTING: u8 = 0x01;
    pub const ENTITY_DISCOVER: u8 = 0x02;
}

/// AECP `message_type` values (IEEE 1722.1-2021 §9.2.1.1.5, Table 9.1).
pub mod aecp_msg {
    pub const AEM_COMMAND: u8 = 0x00;
    pub const AEM_RESPONSE: u8 = 0x01;
    pub const VENDOR_UNIQUE_COMMAND: u8 = 0x06;
    pub const VENDOR_UNIQUE_RESPONSE: u8 = 0x07;
}

/// AECP status codes (IEEE 1722.1-2021 §9.2.1.1.6, Table 9.2; Milan v1.2 §5.4).
pub mod aecp_status {
    pub const SUCCESS: u8 = 0x00;
    pub const NOT_IMPLEMENTED: u8 = 0x01;
    pub const NO_SUCH_DESCRIPTOR: u8 = 0x02;
    pub const ENTITY_LOCKED: u8 = 0x03;
    pub const ENTITY_ACQUIRED: u8 = 0x04;
    pub const NOT_AUTHENTICATED: u8 = 0x05;
    pub const AUTHENTICATION_DISABLED: u8 = 0x06;
    pub const BAD_ARGUMENTS: u8 = 0x07;
    pub const NO_RESOURCES: u8 = 0x08;
    pub const IN_PROGRESS: u8 = 0x09;
    pub const ENTITY_MISBEHAVING: u8 = 0x0A;
    pub const NOT_SUPPORTED: u8 = 0x0B;
    /// AEM status: a stream is running, so the setter is refused (§9.3; the
    /// design cites "STREAM_IS_RUNNING 12").
    pub const STREAM_IS_RUNNING: u8 = 0x0C;
}

/// AEM `command_type` values (IEEE 1722.1-2021 §7.4, Table 7.126).
pub mod aem_cmd {
    pub const ACQUIRE_ENTITY: u16 = 0x0000;
    pub const LOCK_ENTITY: u16 = 0x0001;
    pub const ENTITY_AVAILABLE: u16 = 0x0002;
    pub const CONTROLLER_AVAILABLE: u16 = 0x0003;
    pub const READ_DESCRIPTOR: u16 = 0x0004;
    pub const SET_CONFIGURATION: u16 = 0x0006;
    pub const GET_CONFIGURATION: u16 = 0x0007;
    pub const SET_STREAM_FORMAT: u16 = 0x0008;
    pub const GET_STREAM_FORMAT: u16 = 0x0009;
    pub const SET_STREAM_INFO: u16 = 0x000E;
    pub const GET_STREAM_INFO: u16 = 0x000F;
    pub const SET_NAME: u16 = 0x0010;
    pub const GET_NAME: u16 = 0x0011;
    pub const SET_SAMPLING_RATE: u16 = 0x0012;
    pub const GET_SAMPLING_RATE: u16 = 0x0013;
    pub const SET_CLOCK_SOURCE: u16 = 0x0016;
    pub const GET_CLOCK_SOURCE: u16 = 0x0017;
    pub const REGISTER_UNSOLICITED_NOTIFICATION: u16 = 0x0024;
    pub const DEREGISTER_UNSOLICITED_NOTIFICATION: u16 = 0x0025;
    pub const GET_AVB_INFO: u16 = 0x0027;
    pub const GET_AS_PATH: u16 = 0x0028;
    pub const GET_COUNTERS: u16 = 0x0029;
    pub const GET_AUDIO_MAP: u16 = 0x002B;
}

/// AEM descriptor types (IEEE 1722.1-2021 §7.2, Table 7.1).
pub mod desc {
    pub const ENTITY: u16 = 0x0000;
    pub const CONFIGURATION: u16 = 0x0001;
    pub const AUDIO_UNIT: u16 = 0x0002;
    pub const STREAM_INPUT: u16 = 0x0005;
    pub const STREAM_OUTPUT: u16 = 0x0006;
    pub const JACK_INPUT: u16 = 0x0007;
    pub const JACK_OUTPUT: u16 = 0x0008;
    pub const AVB_INTERFACE: u16 = 0x0009;
    pub const CLOCK_SOURCE: u16 = 0x000A;
    pub const LOCALE: u16 = 0x000C;
    pub const STRINGS: u16 = 0x000D;
    pub const STREAM_PORT_INPUT: u16 = 0x000E;
    pub const STREAM_PORT_OUTPUT: u16 = 0x000F;
    pub const AUDIO_CLUSTER: u16 = 0x0014;
    pub const AUDIO_MAP: u16 = 0x0017;
    pub const CONTROL: u16 = 0x001A;
    pub const CLOCK_DOMAIN: u16 = 0x0024;

    /// Stable name for the AEM dump JSON and logs.
    pub fn name(t: u16) -> Option<&'static str> {
        Some(match t {
            ENTITY => "ENTITY",
            CONFIGURATION => "CONFIGURATION",
            AUDIO_UNIT => "AUDIO_UNIT",
            STREAM_INPUT => "STREAM_INPUT",
            STREAM_OUTPUT => "STREAM_OUTPUT",
            JACK_INPUT => "JACK_INPUT",
            JACK_OUTPUT => "JACK_OUTPUT",
            AVB_INTERFACE => "AVB_INTERFACE",
            CLOCK_SOURCE => "CLOCK_SOURCE",
            LOCALE => "LOCALE",
            STRINGS => "STRINGS",
            STREAM_PORT_INPUT => "STREAM_PORT_INPUT",
            STREAM_PORT_OUTPUT => "STREAM_PORT_OUTPUT",
            AUDIO_CLUSTER => "AUDIO_CLUSTER",
            AUDIO_MAP => "AUDIO_MAP",
            CONTROL => "CONTROL",
            CLOCK_DOMAIN => "CLOCK_DOMAIN",
            _ => return None,
        })
    }
}

/// ADP `entity_capabilities` flags (IEEE 1722.1-2021 §6.2.1.9, Table 6.3).
pub mod entity_caps {
    pub const EFU_MODE: u32 = 1 << 0;
    pub const ADDRESS_ACCESS_SUPPORTED: u32 = 1 << 1;
    pub const GATEWAY_ENTITY: u32 = 1 << 2;
    pub const AEM_SUPPORTED: u32 = 1 << 3;
    pub const LEGACY_AVC: u32 = 1 << 4;
    pub const ASSOCIATION_ID_SUPPORTED: u32 = 1 << 5;
    pub const ASSOCIATION_ID_VALID: u32 = 1 << 6;
    pub const VENDOR_UNIQUE_SUPPORTED: u32 = 1 << 7;
    pub const CLASS_A_SUPPORTED: u32 = 1 << 8;
    pub const CLASS_B_SUPPORTED: u32 = 1 << 9;
    pub const GPTP_SUPPORTED: u32 = 1 << 10;
    pub const AEM_AUTHENTICATION_SUPPORTED: u32 = 1 << 11;
    pub const AEM_AUTHENTICATION_REQUIRED: u32 = 1 << 12;
    pub const AEM_PERSISTENT_ACQUIRE_SUPPORTED: u32 = 1 << 13;
    pub const AEM_IDENTIFY_CONTROL_INDEX_VALID: u32 = 1 << 14;
    pub const AEM_INTERFACE_INDEX_VALID: u32 = 1 << 15;
    pub const GENERAL_CONTROLLER_IGNORE: u32 = 1 << 16;
    pub const ENTITY_NOT_READY: u32 = 1 << 17;
}

/// ADP `talker_capabilities` flags (IEEE 1722.1-2021 §6.2.1.11, Table 6.4).
pub mod talker_caps {
    pub const IMPLEMENTED: u16 = 1 << 0;
    pub const OTHER_SOURCE: u16 = 1 << 9;
    pub const CONTROL_SOURCE: u16 = 1 << 10;
    pub const MEDIA_CLOCK_SOURCE: u16 = 1 << 11;
    pub const SMPTE_SOURCE: u16 = 1 << 12;
    pub const MIDI_SOURCE: u16 = 1 << 13;
    pub const AUDIO_SOURCE: u16 = 1 << 14;
    pub const VIDEO_SOURCE: u16 = 1 << 15;
}

/// ADP `listener_capabilities` flags (IEEE 1722.1-2021 §6.2.1.13, Table 6.5).
pub mod listener_caps {
    pub const IMPLEMENTED: u16 = 1 << 0;
    pub const OTHER_SINK: u16 = 1 << 9;
    pub const CONTROL_SINK: u16 = 1 << 10;
    pub const MEDIA_CLOCK_SINK: u16 = 1 << 11;
    pub const SMPTE_SINK: u16 = 1 << 12;
    pub const MIDI_SINK: u16 = 1 << 13;
    pub const AUDIO_SINK: u16 = 1 << 14;
    pub const VIDEO_SINK: u16 = 1 << 15;
}

/// ADP `controller_capabilities` flags (IEEE 1722.1-2021 §6.2.1.15, Table 6.6).
pub mod controller_caps {
    pub const IMPLEMENTED: u32 = 1 << 0;
}

/// ACMP `message_type` values (IEEE 1722.1-2021 §8.2.1.5, Table 8.1).
/// Each `*_RESPONSE` is `*_COMMAND + 1` (the response-matching rule, §9.2).
pub mod acmp_msg {
    pub const CONNECT_TX_COMMAND: u8 = 0x00;
    pub const CONNECT_TX_RESPONSE: u8 = 0x01;
    pub const DISCONNECT_TX_COMMAND: u8 = 0x02;
    pub const DISCONNECT_TX_RESPONSE: u8 = 0x03;
    pub const GET_TX_STATE_COMMAND: u8 = 0x04;
    pub const GET_TX_STATE_RESPONSE: u8 = 0x05;
    pub const CONNECT_RX_COMMAND: u8 = 0x06;
    pub const CONNECT_RX_RESPONSE: u8 = 0x07;
    pub const DISCONNECT_RX_COMMAND: u8 = 0x08;
    pub const DISCONNECT_RX_RESPONSE: u8 = 0x09;
    pub const GET_RX_STATE_COMMAND: u8 = 0x0A;
    pub const GET_RX_STATE_RESPONSE: u8 = 0x0B;
    pub const GET_TX_CONNECTION_COMMAND: u8 = 0x0C;
    pub const GET_TX_CONNECTION_RESPONSE: u8 = 0x0D;
    /// PROBE_TX_COMMAND is Milan v1.2 §5.5.3 (Milan uses GET_TX_STATE as the
    /// probe); kept distinct here for the listener SM's clarity, encoded as
    /// GET_TX_STATE on the wire (§9.4). The design's "PROBE_TX" maps to the
    /// GET_TX_STATE message type.
    pub const PROBE_TX_COMMAND: u8 = GET_TX_STATE_COMMAND;
    pub const PROBE_TX_RESPONSE: u8 = GET_TX_STATE_RESPONSE;
}

/// ACMP status codes (IEEE 1722.1-2021 §8.2.1.16, Table 8.2).
pub mod acmp_status {
    pub const SUCCESS: u8 = 0x00;
    pub const LISTENER_UNKNOWN_ID: u8 = 0x01;
    pub const TALKER_UNKNOWN_ID: u8 = 0x02;
    pub const TALKER_DEST_MAC_FAIL: u8 = 0x03;
    pub const TALKER_NO_STREAM_INDEX: u8 = 0x04;
    pub const TALKER_NO_BANDWIDTH: u8 = 0x05;
    pub const TALKER_EXCLUSIVE: u8 = 0x06;
    pub const LISTENER_TALKER_TIMEOUT: u8 = 0x07;
    pub const LISTENER_EXCLUSIVE: u8 = 0x08;
    pub const STATE_UNAVAILABLE: u8 = 0x09;
    pub const NOT_CONNECTED: u8 = 0x0A;
    pub const NO_SUCH_CONNECTION: u8 = 0x0B;
    pub const COULD_NOT_SEND_MESSAGE: u8 = 0x0C;
    pub const TALKER_MISBEHAVING: u8 = 0x0D;
    pub const LISTENER_MISBEHAVING: u8 = 0x0E;
    pub const CONTROLLER_NOT_AUTHORIZED: u8 = 0x10;
    pub const INCOMPATIBLE_REQUEST: u8 = 0x11;
    pub const NOT_SUPPORTED: u8 = 0x1F;
}

/// ACMP connect flags (IEEE 1722.1-2021 §8.2.1.17, Table 8.3).
pub mod acmp_flags {
    pub const CLASS_B: u16 = 1 << 0;
    pub const FAST_CONNECT: u16 = 1 << 1;
    pub const SAVED_STATE: u16 = 1 << 2;
    pub const STREAMING_WAIT: u16 = 1 << 3;
    pub const SUPPORTS_ENCRYPTED: u16 = 1 << 4;
    pub const ENCRYPTED_PDU: u16 = 1 << 5;
    pub const TALKER_FAILED: u16 = 1 << 6;
}

/// Clock source types (IEEE 1722.1-2021 §7.2.9, Table 7.14).
pub mod clock_source {
    pub const INTERNAL: u16 = 0x0000;
    pub const EXTERNAL: u16 = 0x0001;
    pub const INPUT_STREAM: u16 = 0x0002;
}

/// Stream flags in STREAM_INPUT/OUTPUT descriptors (IEEE 1722.1-2021 §7.2.6,
/// Table 7.11).
pub mod stream_flags {
    pub const CLOCK_SYNC_SOURCE: u16 = 1 << 0;
    pub const CLASS_A: u16 = 1 << 1;
    pub const CLASS_B: u16 = 1 << 2;
    pub const SUPPORTS_ENCRYPTED: u16 = 1 << 3;
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn subtypes_pinned() {
        assert_eq!(subtype::ADP, 0xFA);
        assert_eq!(subtype::AECP, 0xFB);
        assert_eq!(subtype::ACMP, 0xFC);
    }

    #[test]
    fn adp_and_aecp_msg_pinned() {
        assert_eq!(adp_msg::ENTITY_AVAILABLE, 0);
        assert_eq!(adp_msg::ENTITY_DEPARTING, 1);
        assert_eq!(adp_msg::ENTITY_DISCOVER, 2);
        assert_eq!(aecp_msg::AEM_COMMAND, 0);
        assert_eq!(aecp_msg::AEM_RESPONSE, 1);
        assert_eq!(aecp_msg::VENDOR_UNIQUE_COMMAND, 6);
        assert_eq!(aecp_msg::VENDOR_UNIQUE_RESPONSE, 7);
    }

    #[test]
    fn aecp_status_pinned() {
        assert_eq!(aecp_status::SUCCESS, 0);
        assert_eq!(aecp_status::NOT_IMPLEMENTED, 1);
        assert_eq!(aecp_status::NO_SUCH_DESCRIPTOR, 2);
        assert_eq!(aecp_status::ENTITY_LOCKED, 3);
        assert_eq!(aecp_status::BAD_ARGUMENTS, 7);
        assert_eq!(aecp_status::NO_RESOURCES, 8);
        assert_eq!(aecp_status::IN_PROGRESS, 9);
        assert_eq!(aecp_status::NOT_SUPPORTED, 0x0B);
        assert_eq!(aecp_status::STREAM_IS_RUNNING, 12);
    }

    #[test]
    fn aem_cmd_pinned() {
        assert_eq!(aem_cmd::ACQUIRE_ENTITY, 0x0000);
        assert_eq!(aem_cmd::LOCK_ENTITY, 0x0001);
        assert_eq!(aem_cmd::READ_DESCRIPTOR, 0x0004);
        assert_eq!(aem_cmd::GET_STREAM_FORMAT, 0x0009);
        assert_eq!(aem_cmd::GET_STREAM_INFO, 0x000F);
        assert_eq!(aem_cmd::GET_SAMPLING_RATE, 0x0013);
        assert_eq!(aem_cmd::REGISTER_UNSOLICITED_NOTIFICATION, 0x0024);
        assert_eq!(aem_cmd::GET_AVB_INFO, 0x0027);
        assert_eq!(aem_cmd::GET_COUNTERS, 0x0029);
        assert_eq!(aem_cmd::GET_AUDIO_MAP, 0x002B);
    }

    #[test]
    fn descriptor_types_pinned() {
        assert_eq!(desc::ENTITY, 0x0000);
        assert_eq!(desc::CONFIGURATION, 0x0001);
        assert_eq!(desc::AUDIO_UNIT, 0x0002);
        assert_eq!(desc::STREAM_INPUT, 0x0005);
        assert_eq!(desc::STREAM_OUTPUT, 0x0006);
        assert_eq!(desc::AVB_INTERFACE, 0x0009);
        assert_eq!(desc::CLOCK_SOURCE, 0x000A);
        assert_eq!(desc::STREAM_PORT_INPUT, 0x000E);
        assert_eq!(desc::AUDIO_CLUSTER, 0x0014);
        assert_eq!(desc::AUDIO_MAP, 0x0017);
        assert_eq!(desc::CLOCK_DOMAIN, 0x0024);
        assert_eq!(desc::name(0x0006), Some("STREAM_OUTPUT"));
        assert_eq!(desc::name(0x1234), None);
    }

    #[test]
    fn acmp_msg_response_is_command_plus_one() {
        assert_eq!(
            acmp_msg::CONNECT_RX_RESPONSE,
            acmp_msg::CONNECT_RX_COMMAND + 1
        );
        assert_eq!(
            acmp_msg::DISCONNECT_RX_RESPONSE,
            acmp_msg::DISCONNECT_RX_COMMAND + 1
        );
        assert_eq!(
            acmp_msg::GET_TX_STATE_RESPONSE,
            acmp_msg::GET_TX_STATE_COMMAND + 1
        );
        assert_eq!(acmp_msg::PROBE_TX_COMMAND, acmp_msg::GET_TX_STATE_COMMAND);
        assert_eq!(acmp_msg::CONNECT_RX_COMMAND, 0x06);
        assert_eq!(acmp_msg::DISCONNECT_RX_COMMAND, 0x08);
    }

    #[test]
    fn acmp_status_pinned() {
        assert_eq!(acmp_status::SUCCESS, 0);
        assert_eq!(acmp_status::LISTENER_UNKNOWN_ID, 1);
        assert_eq!(acmp_status::TALKER_DEST_MAC_FAIL, 3);
        assert_eq!(acmp_status::TALKER_NO_STREAM_INDEX, 4);
        assert_eq!(acmp_status::TALKER_NO_BANDWIDTH, 5);
        assert_eq!(acmp_status::TALKER_EXCLUSIVE, 6);
        assert_eq!(acmp_status::LISTENER_EXCLUSIVE, 8);
        assert_eq!(acmp_status::NOT_CONNECTED, 0x0A);
        assert_eq!(acmp_status::NOT_SUPPORTED, 0x1F);
    }

    #[test]
    fn clock_and_stream_flags_pinned() {
        assert_eq!(clock_source::INTERNAL, 0);
        assert_eq!(clock_source::INPUT_STREAM, 2);
        assert_eq!(stream_flags::CLOCK_SYNC_SOURCE, 1);
        assert_eq!(stream_flags::CLASS_A, 2);
        assert_eq!(entity_caps::AEM_SUPPORTED, 1 << 3);
        assert_eq!(entity_caps::GPTP_SUPPORTED, 1 << 10);
        assert_eq!(listener_caps::AUDIO_SINK, 1 << 14);
    }
}
