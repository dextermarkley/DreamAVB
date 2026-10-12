// SPDX-License-Identifier: MIT OR Apache-2.0

//! Our listener entity (design §9.3, §9.4): the ADP advertiser, the AECP AEM
//! responder, and the per-STREAM_INPUT ACMP listener state machines.
//!
//! Sans-IO: `advertise`/`departing` produce ADPDUs, `handle_aecp` turns a
//! received AEM command into a response, and the [`acmp_listener`] SMs drive
//! connection management. The daemon owns the socket, the clock and the
//! counters.

pub mod acmp_listener;
pub mod control;
pub mod talker;

use avb_core::{Eui64, MacAddr};

use crate::adp::{Adpdu, ADP_CONTROL_DATA_LENGTH};
use crate::aecp::AemPdu;
use crate::aem::desc;
use crate::constants::{
    adp_msg, aecp_msg, aecp_status, aem_cmd, controller_caps, entity_caps, listener_caps,
};
use crate::derive::ListenerModel;
use crate::entity::control::ControlState;

pub use acmp_listener::{AcmpResponse, Action, Effective, Event, ListenerSm, State};
pub use talker::{talker_model, StreamInfo, TalkerEntity, TalkerModel};

/// Advertisement `valid_time` (§9.3: 10 → 20 s validity).
pub const VALID_TIME: u8 = 10;
/// Re-advertise interval (§9.3: every 5 s).
pub const ADVERTISE_INTERVAL_NS: u64 = 5_000_000_000;
/// LOCK_ENTITY auto-release (§9.3: 60 s). Re-exported from [`control`], which
/// holds the rule both directions share.
pub use control::LOCK_TIMEOUT_NS;
/// Max registered unsolicited-notification controllers (§9.3: 16).
pub use control::MAX_UNSOL_CONTROLLERS;

/// ACQUIRE_ENTITY decision (AS-7, §9.3). Milan v1.2 §5.4.2.1 states that a
/// PAAD-AE (Milan audio endpoint) does NOT implement ACQUIRE_ENTITY — Milan
/// uses only LOCK_ENTITY for exclusive control. We therefore answer
/// NOT_IMPLEMENTED. Recorded in phase1a-impl-verification.md.
pub const ACQUIRE_ENTITY_IMPLEMENTED: bool = false;

/// ENTITY GET_COUNTERS decision (AS-7, §9.3). Milan v1.2 §5.4.3 mandates
/// GET_COUNTERS for AVB_INTERFACE, CLOCK_DOMAIN and STREAM_INPUT, but NOT for
/// the ENTITY descriptor. We answer NOT_IMPLEMENTED for ENTITY GET_COUNTERS.
/// Recorded in phase1a-impl-verification.md.
pub const ENTITY_GET_COUNTERS_IMPLEMENTED: bool = false;

/// gPTP data the entity needs for ADP and GET_AVB_INFO (§9.3). Plain data; the
/// daemon fills it from `avb_gptp::status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GptpStatus {
    pub grandmaster_id: u64,
    pub domain_number: u8,
    pub as_capable: bool,
    pub propagation_delay_ns: u32,
}

/// The listener entity.
#[derive(Clone, Debug)]
pub struct ListenerEntity {
    model: ListenerModel,
    gptp: GptpStatus,
    available_index: u32,
    /// LOCK_ENTITY holder and the unsolicited-notification registrations,
    /// shared with the talker entity so the 60 s release and the 16-controller
    /// cap are written once.
    control: ControlState,
    /// One ACMP listener SM per STREAM_INPUT.
    pub streams: Vec<ListenerSm>,
    /// Shared PROBE_TX sequence counter across all SMs (§9.4).
    probe_tx_seq: u16,
}

impl ListenerEntity {
    /// Build from a derived model. The entity does not advertise until the
    /// daemon calls [`ListenerEntity::advertise`] (readiness is the daemon's
    /// to gate, §9.3).
    pub fn new(model: ListenerModel) -> Self {
        let streams = (0..model.stream_count())
            .map(|i| ListenerSm::new(i as u16))
            .collect();
        Self {
            model,
            gptp: GptpStatus::default(),
            available_index: 0,
            control: ControlState::new(),
            streams,
            probe_tx_seq: 0,
        }
    }

    pub fn model(&self) -> &ListenerModel {
        &self.model
    }

    pub fn entity_id(&self) -> Eui64 {
        self.model.entity_id
    }

    pub fn set_gptp(&mut self, gptp: GptpStatus) {
        self.gptp = gptp;
    }

    /// Next shared PROBE_TX sequence_id (wrapping u16, §9.4).
    pub fn next_probe_seq(&mut self) -> u16 {
        let s = self.probe_tx_seq;
        self.probe_tx_seq = self.probe_tx_seq.wrapping_add(1);
        s
    }

    /// `entity_capabilities` for ADP (§9.3).
    pub fn entity_capabilities(&self) -> u32 {
        entity_caps::AEM_SUPPORTED
            | entity_caps::CLASS_A_SUPPORTED
            | entity_caps::GPTP_SUPPORTED
            | entity_caps::AEM_IDENTIFY_CONTROL_INDEX_VALID
            | entity_caps::AEM_INTERFACE_INDEX_VALID
            | entity_caps::VENDOR_UNIQUE_SUPPORTED
    }

    /// Build an ENTITY_AVAILABLE ADPDU and bump `available_index` (§9.3).
    pub fn advertise(&mut self) -> Adpdu {
        let adp = self.build_adp(adp_msg::ENTITY_AVAILABLE);
        self.available_index = self.available_index.wrapping_add(1);
        adp
    }

    /// Build the ENTITY_DEPARTING ADPDU (§9.3 shutdown).
    pub fn departing(&self) -> Adpdu {
        self.build_adp(adp_msg::ENTITY_DEPARTING)
    }

    fn build_adp(&self, message_type: u8) -> Adpdu {
        // Until the port is Slave the GM id is our own clockIdentity (§9.3).
        let gm = if self.gptp.grandmaster_id != 0 {
            self.gptp.grandmaster_id
        } else {
            self.model.entity_id.to_u64()
        };
        Adpdu {
            message_type,
            valid_time: VALID_TIME,
            entity_id: self.model.entity_id,
            entity_model_id: self.model.entity_model_id,
            entity_capabilities: self.entity_capabilities(),
            talker_stream_sources: 0,
            talker_capabilities: 0,
            listener_stream_sinks: self.model.stream_count() as u16,
            listener_capabilities: listener_caps::IMPLEMENTED | listener_caps::AUDIO_SINK,
            controller_capabilities: controller_caps::IMPLEMENTED,
            available_index: self.available_index,
            gptp_grandmaster_id: gm,
            gptp_domain_number: self.gptp.domain_number,
            identify_control_index: 0,
            interface_index: 0,
            association_id: 0,
        }
    }

    /// Is a STREAM_INPUT bound (any SM past UNBOUND)? Used to refuse SET_*
    /// with STREAM_IS_RUNNING (§9.3 ADC-12).
    fn any_stream_bound(&self) -> bool {
        self.streams.iter().any(|s| s.state() != State::Unbound)
    }

    /// Handle an AEM command addressed to us. Returns the AEM response to
    /// send back (unicast to the controller). `now_ns` ages the lock.
    pub fn handle_aecp(&mut self, cmd: &AemPdu, src_mac: MacAddr, now_ns: u64) -> AemPdu {
        // Expire a stale lock first (§9.3: 60 s auto-release).
        self.control.expire_lock(now_ns);
        if cmd.message_type != aecp_msg::AEM_COMMAND || cmd.target_entity_id != self.model.entity_id
        {
            return self.respond(cmd, aecp_status::NOT_IMPLEMENTED, cmd.payload.clone());
        }

        let (status, payload) = self.dispatch(cmd, src_mac, now_ns);
        self.respond(cmd, status, payload)
    }

    fn dispatch(&mut self, cmd: &AemPdu, src_mac: MacAddr, now_ns: u64) -> (u8, Vec<u8>) {
        let ctrl = cmd.controller_entity_id;
        match cmd.command_type {
            aem_cmd::ACQUIRE_ENTITY => {
                if ACQUIRE_ENTITY_IMPLEMENTED {
                    (aecp_status::SUCCESS, cmd.payload.clone())
                } else {
                    (aecp_status::NOT_IMPLEMENTED, cmd.payload.clone())
                }
            }
            aem_cmd::LOCK_ENTITY => (self.control.lock(ctrl, now_ns), cmd.payload.clone()),
            aem_cmd::ENTITY_AVAILABLE | aem_cmd::CONTROLLER_AVAILABLE => {
                (aecp_status::SUCCESS, cmd.payload.clone())
            }
            aem_cmd::GET_CONFIGURATION => (aecp_status::SUCCESS, vec![0, 0, 0, 0]),
            aem_cmd::SET_CONFIGURATION => {
                let cfg = cmd
                    .payload
                    .get(2..4)
                    .and_then(|b| b.try_into().ok())
                    .map(u16::from_be_bytes);
                if cfg == Some(0) {
                    (aecp_status::SUCCESS, cmd.payload.clone())
                } else {
                    (aecp_status::BAD_ARGUMENTS, cmd.payload.clone())
                }
            }
            aem_cmd::READ_DESCRIPTOR => self.read_descriptor(cmd),
            aem_cmd::REGISTER_UNSOLICITED_NOTIFICATION => (
                self.control.register_unsol(ctrl, src_mac),
                cmd.payload.clone(),
            ),
            aem_cmd::DEREGISTER_UNSOLICITED_NOTIFICATION => {
                (self.control.deregister_unsol(ctrl), cmd.payload.clone())
            }
            aem_cmd::SET_STREAM_FORMAT | aem_cmd::SET_SAMPLING_RATE => {
                // Rate/format change is out of scope; refuse while bound.
                if self.any_stream_bound() {
                    (aecp_status::STREAM_IS_RUNNING, cmd.payload.clone())
                } else if cmd.command_type == aem_cmd::SET_SAMPLING_RATE {
                    self.set_sampling_rate(cmd)
                } else {
                    self.set_stream_format(cmd)
                }
            }
            aem_cmd::GET_SAMPLING_RATE => {
                let mut p = vec![0, 0, 0, 0]; // descriptor_type + index
                p.extend_from_slice(
                    &(self.model.current_sampling_rate & 0x1FFF_FFFF).to_be_bytes(),
                );
                (aecp_status::SUCCESS, p)
            }
            aem_cmd::SET_STREAM_INFO => (aecp_status::NOT_IMPLEMENTED, cmd.payload.clone()),
            aem_cmd::GET_COUNTERS => self.get_counters(cmd),
            _ => (aecp_status::NOT_IMPLEMENTED, cmd.payload.clone()),
        }
    }

    fn set_sampling_rate(&mut self, cmd: &AemPdu) -> (u8, Vec<u8>) {
        let rate = cmd
            .payload
            .get(4..8)
            .and_then(|b| b.try_into().ok())
            .map(u32::from_be_bytes)
            .map(|r| r & 0x1FFF_FFFF);
        if rate == Some(self.model.current_sampling_rate) {
            (aecp_status::SUCCESS, cmd.payload.clone())
        } else {
            (aecp_status::BAD_ARGUMENTS, cmd.payload.clone())
        }
    }

    fn set_stream_format(&mut self, cmd: &AemPdu) -> (u8, Vec<u8>) {
        // Unbound: a value present in that stream's `formats` -> SUCCESS.
        let idx = cmd
            .payload
            .get(2..4)
            .and_then(|b| b.try_into().ok())
            .map(u16::from_be_bytes);
        let fmt = cmd
            .payload
            .get(4..12)
            .and_then(|b| b.try_into().ok())
            .map(u64::from_be_bytes);
        match (idx, fmt) {
            (Some(i), Some(f)) => {
                let ok = self
                    .model
                    .streams
                    .get(usize::from(i))
                    .is_some_and(|s| s.formats.iter().any(|x| x.0 == f));
                if ok {
                    (aecp_status::SUCCESS, cmd.payload.clone())
                } else {
                    (aecp_status::BAD_ARGUMENTS, cmd.payload.clone())
                }
            }
            _ => (aecp_status::BAD_ARGUMENTS, cmd.payload.clone()),
        }
    }

    fn read_descriptor(&self, cmd: &AemPdu) -> (u8, Vec<u8>) {
        // payload: configuration_index(2) reserved(2) descriptor_type(2) descriptor_index(2)
        let ty = cmd
            .payload
            .get(4..6)
            .and_then(|b| b.try_into().ok())
            .map(u16::from_be_bytes);
        let index = cmd
            .payload
            .get(6..8)
            .and_then(|b| b.try_into().ok())
            .map(u16::from_be_bytes);
        let (ty, index) = match (ty, index) {
            (Some(t), Some(i)) => (t, i),
            _ => return (aecp_status::BAD_ARGUMENTS, cmd.payload.clone()),
        };
        match self.encode_descriptor(ty, index) {
            Some(bytes) => {
                // Response payload echoes configuration_index + reserved, then
                // the descriptor bytes.
                let mut p = Vec::new();
                p.extend_from_slice(cmd.payload.get(0..4).unwrap_or(&[0, 0, 0, 0]));
                p.extend_from_slice(&bytes);
                (aecp_status::SUCCESS, p)
            }
            None => (aecp_status::NO_SUCH_DESCRIPTOR, cmd.payload.clone()),
        }
    }

    /// Encode one of our descriptors (§9.3 derived AEM). Only index 0 exists
    /// for the single-instance descriptors; STREAM_INPUT/CLUSTER are indexed.
    fn encode_descriptor(&self, ty: u16, index: u16) -> Option<Vec<u8>> {
        use crate::aem::model::*;
        use crate::constants::{clock_source, desc as dt, stream_flags};
        match ty {
            dt::ENTITY if index == 0 => Some(desc::encode_entity(&EntityDesc {
                entity_id: self.model.entity_id,
                entity_model_id: self.model.entity_model_id,
                entity_capabilities: self.entity_capabilities(),
                listener_stream_sinks: self.model.stream_count() as u16,
                listener_capabilities: listener_caps::IMPLEMENTED | listener_caps::AUDIO_SINK,
                controller_capabilities: controller_caps::IMPLEMENTED,
                available_index: self.available_index,
                entity_name: "AVB-Audio Mac".into(),
                firmware_version: self.model.firmware_version.clone(),
                serial_number: hex_mac(self.model.mac),
                configurations_count: 1,
                current_configuration: 0,
                ..Default::default()
            })),
            dt::STREAM_INPUT => {
                let s = self.model.streams.get(usize::from(index))?;
                let mut flags = stream_flags::CLASS_A;
                if usize::from(index) == self.model.reference_index {
                    flags |= stream_flags::CLOCK_SYNC_SOURCE;
                }
                Some(desc::encode_stream(
                    &StreamDesc {
                        object_name: s.object_name.clone(),
                        clock_domain_index: 0,
                        stream_flags: flags,
                        current_format: s.current_format,
                        formats: s.formats.clone(),
                        avb_interface_index: 0,
                        buffer_length: 2_000_000,
                    },
                    true,
                ))
            }
            dt::CLOCK_SOURCE if index <= 1 => {
                let d = if index == 0 {
                    ClockSourceDesc {
                        object_name: "Internal".into(),
                        clock_source_type: clock_source::INTERNAL,
                        ..Default::default()
                    }
                } else {
                    ClockSourceDesc {
                        object_name: "Stream clock".into(),
                        clock_source_type: clock_source::INPUT_STREAM,
                        clock_source_location_type: dt::STREAM_INPUT,
                        clock_source_location_index: self.model.reference_index as u16,
                        ..Default::default()
                    }
                };
                Some(desc::encode_clock_source(&d))
            }
            dt::CLOCK_DOMAIN if index == 0 => Some(desc::encode_clock_domain(&ClockDomainDesc {
                object_name: "Domain".into(),
                clock_source_index: 1,
                clock_sources: vec![0, 1],
            })),
            _ => None,
        }
    }

    fn get_counters(&self, cmd: &AemPdu) -> (u8, Vec<u8>) {
        let ty = cmd
            .payload
            .get(0..2)
            .and_then(|b| b.try_into().ok())
            .map(u16::from_be_bytes);
        match ty {
            Some(desc_ty) if desc_ty == crate::constants::desc::ENTITY => {
                if ENTITY_GET_COUNTERS_IMPLEMENTED {
                    // SUCCESS with valid mask 0 and zero values.
                    let mut p = cmd.payload.get(0..4).unwrap_or(&[0; 4]).to_vec();
                    p.resize(4 + 4 + 128, 0);
                    (aecp_status::SUCCESS, p)
                } else {
                    (aecp_status::NOT_IMPLEMENTED, cmd.payload.clone())
                }
            }
            // STREAM_INPUT / AVB_INTERFACE / CLOCK_DOMAIN: the daemon fills in
            // the atomic values; here we return SUCCESS with a zeroed block so
            // the responder is complete without the run-time counters.
            Some(_) => {
                let mut p = cmd.payload.get(0..4).unwrap_or(&[0; 4]).to_vec();
                p.resize(4 + 4 + 128, 0);
                (aecp_status::SUCCESS, p)
            }
            None => (aecp_status::BAD_ARGUMENTS, cmd.payload.clone()),
        }
    }

    fn respond(&self, cmd: &AemPdu, status: u8, payload: Vec<u8>) -> AemPdu {
        AemPdu {
            message_type: aecp_msg::AEM_RESPONSE,
            status,
            target_entity_id: self.model.entity_id,
            controller_entity_id: cmd.controller_entity_id,
            sequence_id: cmd.sequence_id,
            unsolicited: false,
            command_type: cmd.command_type,
            payload,
        }
    }
}

fn hex_mac(mac: MacAddr) -> String {
    mac.0.iter().map(|b| format!("{b:02x}")).collect()
}

/// Build the full ADPDU control-data-length check helper (used in tests).
pub const fn adp_control_data_length() -> u16 {
    ADP_CONTROL_DATA_LENGTH
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::assertions_on_constants)]
mod tests {
    use super::*;
    use crate::constants::stream_flags;
    use crate::derive::{listener_model, ListenerModel};
    use avb_avtp::stream_format::{AafFormat, StreamFormat};

    fn model() -> ListenerModel {
        use crate::aem::model::*;
        let f = StreamFormat::from_aaf(AafFormat {
            ut: false,
            nsr: 5,
            format: avb_avtp::aaf::format::INT_32BIT,
            bit_depth: 24,
            channels: 8,
            spf: 6,
        });
        let out = |n: &str, flags: u16| StreamDesc {
            object_name: n.into(),
            stream_flags: flags,
            current_format: f,
            formats: vec![f],
            ..Default::default()
        };
        let aem = TalkerAem {
            entity: EntityDesc {
                entity_id: Eui64::from_u64(0x000A_9203_3BA9_0001),
                talker_stream_sources: 2,
                ..Default::default()
            },
            audio_units: vec![AudioUnitDesc {
                current_sampling_rate: 48_000,
                sampling_rates: vec![48_000],
                ..Default::default()
            }],
            stream_outputs: vec![
                out("Out 1", stream_flags::CLASS_A),
                out("Out 2", stream_flags::CLASS_A),
            ],
            talker_mac: MacAddr([0, 0x0a, 0x92, 3, 0x3b, 0xa9]),
            ..Default::default()
        };
        listener_model(
            &aem,
            MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]),
            "phase1a-test",
        )
        .unwrap()
    }

    fn cmd(command_type: u16, payload: Vec<u8>, entity_id: Eui64) -> AemPdu {
        AemPdu {
            message_type: aecp_msg::AEM_COMMAND,
            status: aecp_status::SUCCESS,
            target_entity_id: entity_id,
            controller_entity_id: Eui64::from_u64(0xC0FFEE),
            sequence_id: 1,
            unsolicited: false,
            command_type,
            payload,
        }
    }

    #[test]
    fn adp_advertise_fields_and_available_index() {
        let mut e = ListenerEntity::new(model());
        let a = e.advertise();
        assert_eq!(a.message_type, adp_msg::ENTITY_AVAILABLE);
        assert_eq!(a.valid_time, VALID_TIME);
        assert_eq!(a.listener_stream_sinks, 2);
        assert_eq!(a.talker_stream_sources, 0);
        assert_eq!(
            a.listener_capabilities,
            listener_caps::IMPLEMENTED | listener_caps::AUDIO_SINK
        );
        assert_eq!(a.available_index, 0);
        assert_eq!(e.advertise().available_index, 1); // bumped each send
        assert_eq!(e.departing().message_type, adp_msg::ENTITY_DEPARTING);
    }

    #[test]
    fn acquire_entity_not_implemented_milan() {
        // Milan v1.2 §5.4.2.1: PAAD-AE does not implement ACQUIRE_ENTITY.
        let mut e = ListenerEntity::new(model());
        let id = e.entity_id();
        let r = e.handle_aecp(
            &cmd(aem_cmd::ACQUIRE_ENTITY, vec![0; 16], id),
            MacAddr::BROADCAST,
            0,
        );
        assert_eq!(r.status, aecp_status::NOT_IMPLEMENTED);
        assert!(!ACQUIRE_ENTITY_IMPLEMENTED);
    }

    #[test]
    fn entity_get_counters_not_implemented_milan() {
        let mut e = ListenerEntity::new(model());
        let id = e.entity_id();
        let mut payload = Vec::new();
        payload.extend_from_slice(&crate::constants::desc::ENTITY.to_be_bytes());
        payload.extend_from_slice(&[0, 0]); // index
        let r = e.handle_aecp(
            &cmd(aem_cmd::GET_COUNTERS, payload, id),
            MacAddr::BROADCAST,
            0,
        );
        assert_eq!(r.status, aecp_status::NOT_IMPLEMENTED);
        assert!(!ENTITY_GET_COUNTERS_IMPLEMENTED);
    }

    #[test]
    fn lock_entity_and_contention() {
        let mut e = ListenerEntity::new(model());
        let id = e.entity_id();
        let mut c1 = cmd(aem_cmd::LOCK_ENTITY, vec![0; 16], id);
        c1.controller_entity_id = Eui64::from_u64(1);
        assert_eq!(
            e.handle_aecp(&c1, MacAddr::BROADCAST, 0).status,
            aecp_status::SUCCESS
        );
        let mut c2 = cmd(aem_cmd::LOCK_ENTITY, vec![0; 16], id);
        c2.controller_entity_id = Eui64::from_u64(2);
        assert_eq!(
            e.handle_aecp(&c2, MacAddr::BROADCAST, 0).status,
            aecp_status::ENTITY_LOCKED
        );
        // After 60 s the lock auto-releases.
        assert_eq!(
            e.handle_aecp(&c2, MacAddr::BROADCAST, LOCK_TIMEOUT_NS)
                .status,
            aecp_status::SUCCESS
        );
    }

    #[test]
    fn unknown_command_returns_not_implemented() {
        let mut e = ListenerEntity::new(model());
        let id = e.entity_id();
        let r = e.handle_aecp(&cmd(0x7000, vec![], id), MacAddr::BROADCAST, 0);
        assert_eq!(r.status, aecp_status::NOT_IMPLEMENTED);
        assert_eq!(r.message_type, aecp_msg::AEM_RESPONSE);
    }

    #[test]
    fn set_configuration_arguments() {
        let mut e = ListenerEntity::new(model());
        let id = e.entity_id();
        let ok = e.handle_aecp(
            &cmd(aem_cmd::SET_CONFIGURATION, vec![0, 0, 0, 0], id),
            MacAddr::BROADCAST,
            0,
        );
        assert_eq!(ok.status, aecp_status::SUCCESS);
        let bad = e.handle_aecp(
            &cmd(aem_cmd::SET_CONFIGURATION, vec![0, 0, 0, 1], id),
            MacAddr::BROADCAST,
            0,
        );
        assert_eq!(bad.status, aecp_status::BAD_ARGUMENTS);
    }

    #[test]
    fn get_sampling_rate_returns_current() {
        let mut e = ListenerEntity::new(model());
        let id = e.entity_id();
        let r = e.handle_aecp(
            &cmd(aem_cmd::GET_SAMPLING_RATE, vec![], id),
            MacAddr::BROADCAST,
            0,
        );
        assert_eq!(r.status, aecp_status::SUCCESS);
        let hz = r
            .payload
            .get(4..8)
            .and_then(|b| b.try_into().ok())
            .map(u32::from_be_bytes);
        assert_eq!(hz, Some(48_000));
    }

    #[test]
    fn read_descriptor_entity_and_stream_and_missing() {
        let mut e = ListenerEntity::new(model());
        let id = e.entity_id();
        let rd = |ty: u16, idx: u16| {
            let mut p = vec![0, 0, 0, 0];
            p.extend_from_slice(&ty.to_be_bytes());
            p.extend_from_slice(&idx.to_be_bytes());
            cmd(aem_cmd::READ_DESCRIPTOR, p, id)
        };
        let ent = e.handle_aecp(
            &rd(crate::constants::desc::ENTITY, 0),
            MacAddr::BROADCAST,
            0,
        );
        assert_eq!(ent.status, aecp_status::SUCCESS);
        let si = e.handle_aecp(
            &rd(crate::constants::desc::STREAM_INPUT, 1),
            MacAddr::BROADCAST,
            0,
        );
        assert_eq!(si.status, aecp_status::SUCCESS);
        let miss = e.handle_aecp(
            &rd(crate::constants::desc::STREAM_INPUT, 9),
            MacAddr::BROADCAST,
            0,
        );
        assert_eq!(miss.status, aecp_status::NO_SUCH_DESCRIPTOR);
    }

    #[test]
    fn register_unsolicited_limit() {
        let mut e = ListenerEntity::new(model());
        let id = e.entity_id();
        for i in 0..MAX_UNSOL_CONTROLLERS as u64 {
            let mut c = cmd(aem_cmd::REGISTER_UNSOLICITED_NOTIFICATION, vec![], id);
            c.controller_entity_id = Eui64::from_u64(i + 1);
            assert_eq!(
                e.handle_aecp(&c, MacAddr::BROADCAST, 0).status,
                aecp_status::SUCCESS
            );
        }
        let mut over = cmd(aem_cmd::REGISTER_UNSOLICITED_NOTIFICATION, vec![], id);
        over.controller_entity_id = Eui64::from_u64(999);
        assert_eq!(
            e.handle_aecp(&over, MacAddr::BROADCAST, 0).status,
            aecp_status::NO_RESOURCES
        );
    }
}
