// SPDX-License-Identifier: MIT OR Apache-2.0

//! Our TALKER entity (design §9.3 in the playback direction): the ADP
//! advertiser and the AECP AEM responder for the stream the Mac sources.
//!
//! Why this exists: a Milan controller enumerates an entity by walking its
//! AEM — ENTITY, then CONFIGURATION 0 to learn `descriptor_counts`, then every
//! (type, index) that names. Phase 1a only ever answered as a LISTENER, with
//! `talker_stream_sources = 0` and no CONFIGURATION descriptor at all, so the
//! 16R could discover us over ADP and then learn nothing: the walk stopped at
//! the first READ_DESCRIPTOR. On hardware that showed up as the 16R listing no
//! input device for us.
//!
//! Sans-IO, like the listener entity: `advertise` produces an ADPDU and
//! `handle_aecp` turns a received AEM command into the response to send. The
//! daemon owns the socket, the clock and the counters.
//!
//! THE INVARIANT THAT MATTERS: every (type, index) pair implied by
//! `descriptor_counts` must be answerable by [`TalkerEntity::encode_descriptor`].
//! Advertising a count we cannot serve is worse than advertising none — the
//! controller asks for it and gets NO_SUCH_DESCRIPTOR mid-walk. The test
//! `every_advertised_descriptor_is_answerable` enforces it.

use avb_avtp::stream_format::StreamFormat;
use avb_core::{Eui64, MacAddr, StreamId};

use crate::adp::Adpdu;
use crate::aecp::AemPdu;
use crate::aem::desc;
use crate::aem::model::*;
use crate::constants::{
    adp_msg, aecp_msg, aecp_status, aem_cmd, clock_source, desc as dt, entity_caps, if_flags,
    stream_flags, talker_caps,
};
use crate::derive::MODEL_ID_PREFIX;
use crate::entity::control::ControlState;
use crate::entity::GptpStatus;

/// Advertisement `valid_time` (§9.3: 10 → 20 s validity).
pub const VALID_TIME: u8 = 10;

/// Our talker model: the single AAF stream the Mac sources, described in the
/// terms the AEM needs. Built by [`talker_model`], which is the only
/// constructor — the channel count, rate and spf come from the stream format
/// the talker actually transmits, never from a literal (acceptance 3, and the
/// project rule against hardcoded rates/channels).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TalkerModel {
    pub entity_id: Eui64,
    pub entity_model_id: u64,
    pub mac: MacAddr,
    pub stream_id: StreamId,
    pub object_name: String,
    pub current_format: StreamFormat,
    /// Every format we can source. Today exactly one: what we transmit.
    pub formats: Vec<StreamFormat>,
    pub channels: u16,
    /// Per-channel names, used for the AUDIO_CLUSTER descriptors.
    pub channel_names: Vec<String>,
    pub sampling_rates: Vec<u32>,
    pub current_sampling_rate: u32,
    pub firmware_version: String,
}

impl TalkerModel {
    /// Channel count as the descriptor counts need it.
    pub fn cluster_count(&self) -> u16 {
        self.channels
    }
}

/// Build our talker model from the format we actually transmit.
///
/// `fmt` must be a decodable AAF format — the caller already has one, since it
/// is packetizing with it. Returns `None` for a format whose AAF fields cannot
/// be read, rather than inventing a channel count.
pub fn talker_model(
    our_mac: MacAddr,
    stream_id: StreamId,
    fmt: StreamFormat,
    firmware_version: &str,
) -> Option<TalkerModel> {
    let aaf = fmt.aaf()?;
    let hz = aaf.nsr_hz()?;
    let channels = aaf.channels;
    // Channel names are positional: we are the source, so there is no upstream
    // cluster name to inherit the way the listener inherits the 16R's.
    let channel_names = (0..channels).map(|k| format!("Mac ch{}", k + 1)).collect();
    let mut m = TalkerModel {
        entity_id: Eui64::from_mac(our_mac),
        entity_model_id: 0, // filled below
        mac: our_mac,
        stream_id,
        object_name: "Mac→16R".to_string(),
        current_format: fmt,
        formats: vec![fmt],
        channels,
        channel_names,
        // We source at exactly the rate we transmit; a rate change is out of
        // scope (SET_SAMPLING_RATE answers BAD_ARGUMENTS for anything else).
        sampling_rates: vec![hz],
        current_sampling_rate: hz,
        firmware_version: firmware_version.to_string(),
    };
    m.entity_model_id = compute_talker_model_id(&m);
    Some(m)
}

/// `0x02415601 || fnv1a32(canonical model)`, the same scheme as the listener
/// model id (§9.1) so both directions key a controller's cache the same way.
/// The canonical form is the stream format, the channel count and the rates —
/// so the id changes iff the model a controller would read changes.
pub fn compute_talker_model_id(m: &TalkerModel) -> u64 {
    let mut canon = Vec::new();
    canon.extend_from_slice(b"talker");
    canon.extend_from_slice(&m.current_format.0.to_be_bytes());
    canon.extend_from_slice(&m.channels.to_be_bytes());
    for f in &m.formats {
        canon.extend_from_slice(&f.0.to_be_bytes());
    }
    for &hz in &m.sampling_rates {
        canon.extend_from_slice(&hz.to_be_bytes());
    }
    let lo = crate::derive::fnv1a32(&canon);
    (u64::from(MODEL_ID_PREFIX) << 32) | u64::from(lo)
}

/// Live stream state the daemon owns, reported through GET_STREAM_INFO. The
/// entity does not discover any of this itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamInfo {
    /// The 16R has answered our Talker Advertise with Listener Ready.
    pub connected: bool,
    pub dest_mac: MacAddr,
    pub vlan_id: u16,
    /// MSRP accumulated latency we advertised, in ns.
    pub msrp_accumulated_latency_ns: u32,
}

/// Our talker entity.
#[derive(Clone, Debug)]
pub struct TalkerEntity {
    model: TalkerModel,
    gptp: GptpStatus,
    available_index: u32,
    control: ControlState,
    stream: StreamInfo,
}

impl TalkerEntity {
    pub fn new(model: TalkerModel) -> Self {
        Self {
            model,
            gptp: GptpStatus::default(),
            available_index: 0,
            control: ControlState::new(),
            stream: StreamInfo::default(),
        }
    }

    pub fn model(&self) -> &TalkerModel {
        &self.model
    }

    pub fn entity_id(&self) -> Eui64 {
        self.model.entity_id
    }

    pub fn set_gptp(&mut self, gptp: GptpStatus) {
        self.gptp = gptp;
    }

    pub fn set_stream_info(&mut self, info: StreamInfo) {
        self.stream = info;
    }

    /// `entity_capabilities` for ADP and the ENTITY descriptor. One source, so
    /// the two can never disagree — a controller that sees different
    /// capabilities in ADP and in the descriptor treats the entity as
    /// misbehaving.
    pub fn entity_capabilities(&self) -> u32 {
        entity_caps::AEM_SUPPORTED
            | entity_caps::CLASS_A_SUPPORTED
            | entity_caps::GPTP_SUPPORTED
            | entity_caps::AEM_INTERFACE_INDEX_VALID
    }

    pub fn talker_capabilities(&self) -> u16 {
        talker_caps::IMPLEMENTED | talker_caps::AUDIO_SOURCE
    }

    /// Build an ENTITY_AVAILABLE ADPDU and bump `available_index`.
    ///
    /// `available_index` MUST increase monotonically across the life of the
    /// entity: a controller reads a repeat or a decrease as the entity having
    /// restarted, and re-enumerates (or drops) it.
    pub fn advertise(&mut self) -> Adpdu {
        let adp = self.build_adp(adp_msg::ENTITY_AVAILABLE);
        self.available_index = self.available_index.wrapping_add(1);
        adp
    }

    /// The ENTITY_DEPARTING ADPDU, so the 16R drops us promptly at shutdown
    /// instead of waiting out `valid_time`.
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
            // Not 0: a controller keys its cached model on this, and a zero id
            // means "no AEM to cache", which defeats enumeration.
            entity_model_id: self.model.entity_model_id,
            entity_capabilities: self.entity_capabilities(),
            talker_stream_sources: 1,
            talker_capabilities: self.talker_capabilities(),
            listener_stream_sinks: 0,
            listener_capabilities: 0,
            controller_capabilities: 0,
            available_index: self.available_index,
            gptp_grandmaster_id: gm,
            gptp_domain_number: self.gptp.domain_number,
            identify_control_index: 0,
            interface_index: 0,
            association_id: 0,
        }
    }

    /// The per-type descriptor counts for configuration 0.
    ///
    /// Everything here must be answerable by `encode_descriptor`; see the
    /// module invariant. LOCALE/STRINGS are deliberately ABSENT rather than
    /// advertised-and-unserved — every descriptor carries its name inline, so
    /// omitting them costs a controller nothing, while claiming them and
    /// answering NO_SUCH_DESCRIPTOR would break the walk.
    pub fn descriptor_counts(&self) -> Vec<(u16, u16)> {
        vec![
            (dt::AUDIO_UNIT, 1),
            (dt::STREAM_OUTPUT, 1),
            (dt::STREAM_PORT_OUTPUT, 1),
            (dt::AUDIO_CLUSTER, self.model.cluster_count()),
            (dt::AUDIO_MAP, 1),
            (dt::AVB_INTERFACE, 1),
            (dt::CLOCK_SOURCE, 1),
            (dt::CLOCK_DOMAIN, 1),
        ]
    }

    /// Handle an AEM command addressed to us, returning the response to send
    /// back unicast to the controller. `now_ns` ages the lock.
    pub fn handle_aecp(&mut self, cmd: &AemPdu, src_mac: MacAddr, now_ns: u64) -> AemPdu {
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
        let echo = || cmd.payload.clone();
        match cmd.command_type {
            // Milan v1.2 §5.4.2.1: a PAAD-AE does not implement
            // ACQUIRE_ENTITY; exclusive control is LOCK_ENTITY only. Same
            // decision as the listener entity (AS-7).
            aem_cmd::ACQUIRE_ENTITY => (aecp_status::NOT_IMPLEMENTED, echo()),
            aem_cmd::LOCK_ENTITY => (self.control.lock(ctrl, now_ns), echo()),
            aem_cmd::ENTITY_AVAILABLE | aem_cmd::CONTROLLER_AVAILABLE => {
                (aecp_status::SUCCESS, echo())
            }
            aem_cmd::READ_DESCRIPTOR => self.read_descriptor(cmd),
            aem_cmd::GET_CONFIGURATION => (aecp_status::SUCCESS, vec![0, 0, 0, 0]),
            aem_cmd::SET_CONFIGURATION => {
                let cfg = be_u16(&cmd.payload, 2);
                if cfg == Some(0) {
                    (aecp_status::SUCCESS, echo())
                } else {
                    (aecp_status::BAD_ARGUMENTS, echo())
                }
            }
            aem_cmd::GET_STREAM_FORMAT => self.get_stream_format(cmd),
            aem_cmd::GET_STREAM_INFO => self.get_stream_info(cmd),
            aem_cmd::GET_SAMPLING_RATE => {
                let mut p = vec![0, 0, 0, 0]; // descriptor_type + index
                p.extend_from_slice(&encode_pull_rate(self.model.current_sampling_rate));
                (aecp_status::SUCCESS, p)
            }
            aem_cmd::GET_AVB_INFO => self.get_avb_info(cmd),
            aem_cmd::GET_AUDIO_MAP => self.get_audio_map(cmd),
            // A format or rate change while we are sourcing the stream is
            // refused, matching the listener's ADC-12 rule. Unlike the
            // listener we have exactly one format, so there is nothing to
            // accept even when idle.
            aem_cmd::SET_STREAM_FORMAT | aem_cmd::SET_SAMPLING_RATE => {
                if self.control.locked_by_other(ctrl) {
                    (aecp_status::ENTITY_LOCKED, echo())
                } else if self.stream.connected {
                    (aecp_status::STREAM_IS_RUNNING, echo())
                } else {
                    (aecp_status::BAD_ARGUMENTS, echo())
                }
            }
            aem_cmd::REGISTER_UNSOLICITED_NOTIFICATION => {
                (self.control.register_unsol(ctrl, src_mac), echo())
            }
            aem_cmd::DEREGISTER_UNSOLICITED_NOTIFICATION => {
                (self.control.deregister_unsol(ctrl), echo())
            }
            aem_cmd::GET_COUNTERS => {
                // SUCCESS with a zeroed block: the responder is complete
                // without the run-time counters, which the daemon owns.
                let mut p = cmd.payload.get(0..4).unwrap_or(&[0; 4]).to_vec();
                p.resize(4 + 4 + 128, 0);
                (aecp_status::SUCCESS, p)
            }
            // Never silence: an unimplemented command still gets a response
            // (ADC-10), or the controller retries until it times out.
            _ => (aecp_status::NOT_IMPLEMENTED, echo()),
        }
    }

    fn read_descriptor(&self, cmd: &AemPdu) -> (u8, Vec<u8>) {
        // payload: configuration_index(2) reserved(2) descriptor_type(2) descriptor_index(2)
        let (ty, index) = match (be_u16(&cmd.payload, 4), be_u16(&cmd.payload, 6)) {
            (Some(t), Some(i)) => (t, i),
            _ => return (aecp_status::BAD_ARGUMENTS, cmd.payload.clone()),
        };
        match self.encode_descriptor(ty, index) {
            Some(bytes) => {
                let mut p = Vec::new();
                p.extend_from_slice(cmd.payload.get(0..4).unwrap_or(&[0, 0, 0, 0]));
                p.extend_from_slice(&bytes);
                (aecp_status::SUCCESS, p)
            }
            None => (aecp_status::NO_SUCH_DESCRIPTOR, cmd.payload.clone()),
        }
    }

    /// Encode one of our descriptors. `None` is NO_SUCH_DESCRIPTOR.
    pub fn encode_descriptor(&self, ty: u16, index: u16) -> Option<Vec<u8>> {
        let m = &self.model;
        match ty {
            dt::ENTITY if index == 0 => Some(desc::encode_entity(&EntityDesc {
                entity_id: m.entity_id,
                entity_model_id: m.entity_model_id,
                entity_capabilities: self.entity_capabilities(),
                talker_stream_sources: 1,
                talker_capabilities: self.talker_capabilities(),
                listener_stream_sinks: 0,
                listener_capabilities: 0,
                controller_capabilities: 0,
                available_index: self.available_index,
                entity_name: "AVB-Audio Mac".into(),
                firmware_version: m.firmware_version.clone(),
                serial_number: hex_mac(m.mac),
                configurations_count: 1,
                current_configuration: 0,
                ..Default::default()
            })),
            dt::CONFIGURATION if index == 0 => {
                Some(desc::encode_configuration(&ConfigurationDesc {
                    object_name: "Playback".into(),
                    descriptor_counts: self.descriptor_counts(),
                }))
            }
            dt::AUDIO_UNIT if index == 0 => Some(desc::encode_audio_unit(&AudioUnitDesc {
                object_name: "Mac audio".into(),
                clock_domain_index: 0,
                number_of_stream_input_ports: 0,
                number_of_stream_output_ports: 1,
                current_sampling_rate: m.current_sampling_rate,
                sampling_rates: m.sampling_rates.clone(),
            })),
            dt::STREAM_OUTPUT if index == 0 => Some(desc::encode_stream(
                &StreamDesc {
                    object_name: m.object_name.clone(),
                    clock_domain_index: 0,
                    // CLASS_A only. NOT CLOCK_SYNC_SOURCE: that flag offers
                    // this stream as a media-clock reference for the peer, and
                    // our media clock is the Mac's, not disciplined to the
                    // 16R's — advertising it would invite the 16R to slave to
                    // a clock we do not control.
                    stream_flags: stream_flags::CLASS_A,
                    current_format: m.current_format,
                    formats: m.formats.clone(),
                    avb_interface_index: 0,
                    buffer_length: 2_000_000,
                },
                false,
            )),
            dt::STREAM_PORT_OUTPUT if index == 0 => Some(desc::encode_stream_port(
                &StreamPortDesc {
                    clock_domain_index: 0,
                    port_flags: 0,
                    number_of_clusters: m.cluster_count(),
                    base_cluster: 0,
                    number_of_maps: 1,
                    base_map: 0,
                },
                false,
            )),
            dt::AUDIO_CLUSTER if index < m.cluster_count() => {
                Some(desc::encode_audio_cluster(&AudioClusterDesc {
                    object_name: m
                        .channel_names
                        .get(usize::from(index))
                        .cloned()
                        .unwrap_or_else(|| format!("Mac ch{}", index + 1)),
                    channel_count: 1,
                    // 0x40 = AAF sample format, matching the listener side.
                    format: 0x40,
                }))
            }
            dt::AUDIO_MAP if index == 0 => Some(desc::encode_audio_map(&AudioMapDesc {
                mappings: (0..m.cluster_count())
                    .map(|k| AudioMapping {
                        stream_index: 0,
                        stream_channel: k,
                        cluster_offset: k,
                        cluster_channel: 0,
                    })
                    .collect(),
            })),
            dt::AVB_INTERFACE if index == 0 => {
                Some(desc::encode_avb_interface(&AvbInterfaceDesc {
                    object_name: "I210".into(),
                    mac_address: m.mac,
                    // We support gPTP and SRP but are never grandmaster: the
                    // port is slave-only and never sends Announce/Sync (§7).
                    interface_flags: if_flags::GPTP_SUPPORTED | if_flags::SRP_SUPPORTED,
                    clock_identity: m.entity_id,
                    // priority1 255 keeps us from ever winning BMCA.
                    priority1: 255,
                    clock_class: 248,
                    offset_scaled_log_variance: 0x436A,
                    clock_accuracy: 0xFE,
                    priority2: 248,
                    domain_number: self.gptp.domain_number,
                    log_sync_interval: -3,
                    log_announce_interval: 0,
                    log_pdelay_interval: 0,
                    port_number: 1,
                }))
            }
            // One source only, and it is INTERNAL: the stream is produced by
            // CoreAudio on the Mac's own clock. The listener side offers a
            // second INPUT_STREAM source because it slaves to the 16R; in this
            // direction there is nothing to slave to.
            dt::CLOCK_SOURCE if index == 0 => Some(desc::encode_clock_source(&ClockSourceDesc {
                object_name: "Mac internal".into(),
                clock_source_type: clock_source::INTERNAL,
                ..Default::default()
            })),
            dt::CLOCK_DOMAIN if index == 0 => Some(desc::encode_clock_domain(&ClockDomainDesc {
                object_name: "Domain".into(),
                clock_source_index: 0,
                clock_sources: vec![0],
            })),
            _ => None,
        }
    }

    fn get_stream_format(&self, cmd: &AemPdu) -> (u8, Vec<u8>) {
        match (be_u16(&cmd.payload, 0), be_u16(&cmd.payload, 2)) {
            (Some(dt::STREAM_OUTPUT), Some(0)) => {
                let mut p = cmd.payload.get(0..4).unwrap_or(&[0; 4]).to_vec();
                p.extend_from_slice(&self.model.current_format.0.to_be_bytes());
                (aecp_status::SUCCESS, p)
            }
            (Some(_), Some(_)) => (aecp_status::NO_SUCH_DESCRIPTOR, cmd.payload.clone()),
            _ => (aecp_status::BAD_ARGUMENTS, cmd.payload.clone()),
        }
    }

    /// GET_STREAM_INFO is deliberately NOT implemented, and this is a
    /// considered refusal rather than an oversight.
    ///
    /// The response carries a 32-bit `flags` word whose bit positions come
    /// from 1722.1-2021 Table 7.206. The field SEMANTICS are confirmed by the
    /// IEEE 1722.1 working group's own clarification document (Illouz,
    /// "AVDECC clarifications Part 2 - AEM commands", rev 4, §6.2), which
    /// names STREAM_FORMAT_VALID, STREAM_ID_VALID, MSRP_ACC_LAT_VALID,
    /// STREAM_DEST_MAC_VALID, STREAM_VLAN_ID_VALID, MSRP_FAILURE_VALID,
    /// CONNECTED and FAST_CONNECT and says when each is set. It does NOT give
    /// their numeric bit positions, and we have no other authoritative source
    /// for them.
    ///
    /// Every descriptor we DO encode is anchored: the same field layouts
    /// decoded 21 real descriptors off the 16R in the hw-avdecc milestone, so
    /// encoding with them is verified against hardware. A GET_STREAM_INFO
    /// response is the opposite case -- we would be originating a flags word we
    /// have never parsed from the 16R, so a wrong bit would assert something
    /// false about our stream (claiming CONNECTED, or marking the stream id
    /// invalid) with nothing to catch it. Pinning the guess in a unit test
    /// would only restate the guess; that is precisely the tautology that let
    /// a wrong WriteMix FourCC ship.
    ///
    /// NOT_IMPLEMENTED is a legal answer (ADC-10: respond, never go silent),
    /// and enumeration does not need this command. Implement it once the bit
    /// positions can be read from the standard.
    fn get_stream_info(&self, cmd: &AemPdu) -> (u8, Vec<u8>) {
        (aecp_status::NOT_IMPLEMENTED, cmd.payload.clone())
    }

    /// GET_AVB_INFO (§7.4.40): the gPTP identity and the one MSRP mapping we
    /// use. `propagation_delay` is the compensated path delay the servo
    /// measured, which the daemon supplies through [`TalkerEntity::set_gptp`].
    fn get_avb_info(&self, cmd: &AemPdu) -> (u8, Vec<u8>) {
        let mut p = Vec::with_capacity(32);
        p.extend_from_slice(&dt::AVB_INTERFACE.to_be_bytes());
        p.extend_from_slice(&0u16.to_be_bytes()); // descriptor_index
        p.extend_from_slice(&self.gptp.grandmaster_id.to_be_bytes());
        p.extend_from_slice(&self.gptp.propagation_delay_ns.to_be_bytes());
        p.extend_from_slice(&0u16.to_be_bytes()); // reserved
        p.push(self.gptp.domain_number);
        // flags: AS_CAPABLE | GPTP_ENABLED | SRP_ENABLED (bits 0..2).
        let mut flags = 0b110u8; // GPTP_ENABLED | SRP_ENABLED
        if self.gptp.as_capable {
            flags |= 0b001;
        }
        p.push(flags);
        p.extend_from_slice(&1u16.to_be_bytes()); // msrp_mappings_count
        p.extend_from_slice(&0u16.to_be_bytes()); // msrp_mappings_offset
        let d = avb_srp_domain();
        p.push(d.0); // SR class id
        p.push(d.1); // SR class priority
        p.extend_from_slice(&d.2.to_be_bytes()); // SR class VID
        let _ = cmd;
        (aecp_status::SUCCESS, p)
    }

    fn get_audio_map(&self, cmd: &AemPdu) -> (u8, Vec<u8>) {
        match be_u16(&cmd.payload, 2) {
            Some(0) => {}
            Some(_) => return (aecp_status::NO_SUCH_DESCRIPTOR, cmd.payload.clone()),
            None => return (aecp_status::BAD_ARGUMENTS, cmd.payload.clone()),
        }
        let n = self.model.cluster_count();
        let mut p = Vec::new();
        p.extend_from_slice(&dt::AUDIO_MAP.to_be_bytes());
        p.extend_from_slice(&0u16.to_be_bytes()); // map_index
        p.extend_from_slice(&n.to_be_bytes()); // number_of_mappings
        p.extend_from_slice(&0u16.to_be_bytes()); // reserved
        for k in 0..n {
            p.extend_from_slice(&0u16.to_be_bytes()); // stream_index
            p.extend_from_slice(&k.to_be_bytes()); // stream_channel
            p.extend_from_slice(&k.to_be_bytes()); // cluster_offset
            p.extend_from_slice(&0u16.to_be_bytes()); // cluster_channel
        }
        (aecp_status::SUCCESS, p)
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

/// The SR class A domain we declare in MSRP, as (class id, priority, vid).
/// Mirrors `avb_srp::attr::DEFAULT_DOMAIN` without taking a dependency on
/// that crate from here.
const fn avb_srp_domain() -> (u8, u8, u16) {
    (6, 3, 2)
}

fn be_u16(p: &[u8], at: usize) -> Option<u16> {
    p.get(at..at + 2)
        .and_then(|b| b.try_into().ok())
        .map(u16::from_be_bytes)
}

/// 1722.1 sampling-rate field: pull field (3 bits) ‖ base frequency (29 bits).
/// Pull 0 means the base frequency is exact.
fn encode_pull_rate(hz: u32) -> [u8; 4] {
    (hz & 0x1FFF_FFFF).to_be_bytes()
}

fn hex_mac(mac: MacAddr) -> String {
    mac.0.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::aem::desc as dec;

    const OUR_MAC: MacAddr = MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]);
    /// The Milan base AAF format the talker transmits: INT_32BIT, 48 kHz,
    /// 8 channels, 6 spf.
    const FMT: StreamFormat = StreamFormat(0x0205_0220_0200_6000);

    fn entity() -> TalkerEntity {
        let sid = StreamId::from_parts(OUR_MAC, 0);
        let m = talker_model(OUR_MAC, sid, FMT, "phase1b-test").expect("decodable format");
        TalkerEntity::new(m)
    }

    fn cmd(command_type: u16, payload: Vec<u8>, target: Eui64) -> AemPdu {
        AemPdu {
            message_type: aecp_msg::AEM_COMMAND,
            status: 0,
            target_entity_id: target,
            controller_entity_id: Eui64([9, 9, 9, 9, 9, 9, 9, 9]),
            sequence_id: 7,
            unsolicited: false,
            command_type,
            payload,
        }
    }

    fn read_desc_payload(ty: u16, index: u16) -> Vec<u8> {
        let mut p = vec![0, 0, 0, 0]; // configuration_index + reserved
        p.extend_from_slice(&ty.to_be_bytes());
        p.extend_from_slice(&index.to_be_bytes());
        p
    }

    #[test]
    fn model_is_derived_from_the_format_not_hardcoded() {
        let e = entity();
        let m = e.model();
        // 8 channels and 48 kHz come from FMT, not from a literal in the model.
        assert_eq!(m.channels, 8);
        assert_eq!(m.current_sampling_rate, 48_000);
        assert_eq!(m.channel_names.len(), 8);
        assert_eq!(m.entity_id, Eui64::from_mac(OUR_MAC));
        assert_eq!((m.entity_model_id >> 32) as u32, MODEL_ID_PREFIX);
        // A different channel count must produce a different model id, or a
        // controller would serve a stale cached model.
        let other = StreamFormat(0x0205_0220_0100_6000); // 4 ch
        let m2 = talker_model(OUR_MAC, m.stream_id, other, "x").unwrap();
        assert_ne!(m2.channels, m.channels);
        assert_ne!(m2.entity_model_id, m.entity_model_id);
    }

    #[test]
    fn talker_model_refuses_a_non_aaf_format() {
        // A CRF format has no AAF channel count; we must not invent one.
        let crf = StreamFormat(0x0410_6001_0000_BB80);
        assert!(talker_model(OUR_MAC, StreamId(0), crf, "x").is_none());
    }

    /// The whole point of this module: a controller walks ENTITY ->
    /// CONFIGURATION 0 -> every (type, index) in descriptor_counts. If any of
    /// those answers NO_SUCH_DESCRIPTOR the walk breaks, which is exactly how
    /// the 16R ended up listing no input device for us.
    #[test]
    fn every_advertised_descriptor_is_answerable() {
        let e = entity();
        // ENTITY and CONFIGURATION are the walk's roots and are not listed in
        // descriptor_counts, so check them explicitly.
        assert!(e.encode_descriptor(dt::ENTITY, 0).is_some());
        assert!(e.encode_descriptor(dt::CONFIGURATION, 0).is_some());
        let counts = e.descriptor_counts();
        assert!(!counts.is_empty());
        for (ty, n) in counts {
            assert!(n > 0, "advertised a zero count for type {ty:#06x}");
            for index in 0..n {
                assert!(
                    e.encode_descriptor(ty, index).is_some(),
                    "descriptor_counts advertises {ty:#06x}[{index}] but the \
                     responder cannot encode it"
                );
            }
            // One past the advertised count must NOT exist, or a controller
            // enumerating defensively reads a descriptor we never declared.
            assert!(
                e.encode_descriptor(ty, n).is_none(),
                "type {ty:#06x} answered index {n}, past its advertised count"
            );
        }
    }

    #[test]
    fn configuration_descriptor_round_trips_the_counts() {
        let e = entity();
        let bytes = e.encode_descriptor(dt::CONFIGURATION, 0).unwrap();
        let d = dec::decode_configuration(&bytes).expect("decodable");
        assert_eq!(d.descriptor_counts, e.descriptor_counts());
        // 8 clusters for 8 channels, derived from the format.
        assert!(d
            .descriptor_counts
            .iter()
            .any(|&(ty, n)| ty == dt::AUDIO_CLUSTER && n == 8));
    }

    #[test]
    fn entity_descriptor_advertises_talker_not_listener() {
        let e = entity();
        let bytes = e.encode_descriptor(dt::ENTITY, 0).unwrap();
        let d = dec::decode_entity(&bytes).expect("decodable");
        assert_eq!(d.talker_stream_sources, 1);
        assert_eq!(
            d.talker_capabilities,
            talker_caps::IMPLEMENTED | talker_caps::AUDIO_SOURCE
        );
        assert_eq!(d.listener_stream_sinks, 0);
        assert_eq!(d.entity_model_id, e.model().entity_model_id);
        assert_ne!(d.entity_model_id, 0, "a zero model id defeats caching");
        assert_eq!(d.configurations_count, 1);
    }

    /// ADP and the ENTITY descriptor must agree. A controller that sees
    /// different capabilities in the two treats the entity as misbehaving.
    #[test]
    fn adp_and_entity_descriptor_agree() {
        let mut e = entity();
        let adp = e.advertise();
        let d = dec::decode_entity(&e.encode_descriptor(dt::ENTITY, 0).unwrap()).unwrap();
        assert_eq!(adp.entity_id, d.entity_id);
        assert_eq!(adp.entity_model_id, d.entity_model_id);
        assert_eq!(adp.entity_capabilities, d.entity_capabilities);
        assert_eq!(adp.talker_stream_sources, d.talker_stream_sources);
        assert_eq!(adp.talker_capabilities, d.talker_capabilities);
        assert_eq!(adp.listener_stream_sinks, d.listener_stream_sinks);
    }

    #[test]
    fn available_index_increases_every_advertisement() {
        let mut e = entity();
        let a = e.advertise().available_index;
        let b = e.advertise().available_index;
        let c = e.advertise().available_index;
        assert_eq!((a, b, c), (0, 1, 2));
        // ENTITY_DEPARTING must not bump it (it is not an availability change).
        let d = e.departing().available_index;
        assert_eq!(d, 3);
        assert_eq!(e.advertise().available_index, 3);
    }

    #[test]
    fn stream_output_descriptor_carries_our_format_and_is_not_a_clock_source() {
        let e = entity();
        let bytes = e.encode_descriptor(dt::STREAM_OUTPUT, 0).unwrap();
        let d = dec::decode_stream(&bytes).expect("decodable");
        assert_eq!(d.current_format, FMT);
        assert_eq!(d.formats, vec![FMT]);
        assert_eq!(
            d.stream_flags & stream_flags::CLASS_A,
            stream_flags::CLASS_A
        );
        // Our media clock is the Mac's; offering it as a reference would
        // invite the 16R to slave to a clock we do not discipline.
        assert_eq!(d.stream_flags & stream_flags::CLOCK_SYNC_SOURCE, 0);
    }

    #[test]
    fn audio_unit_reports_an_output_port_and_our_rate() {
        let e = entity();
        let bytes = e.encode_descriptor(dt::AUDIO_UNIT, 0).unwrap();
        let d = dec::decode_audio_unit(&bytes).expect("decodable");
        assert_eq!(d.number_of_stream_output_ports, 1);
        assert_eq!(d.number_of_stream_input_ports, 0);
        assert_eq!(d.current_sampling_rate, 48_000);
        assert_eq!(d.sampling_rates, vec![48_000]);
    }

    #[test]
    fn avb_interface_is_gptp_and_srp_but_never_grandmaster() {
        let e = entity();
        let bytes = e.encode_descriptor(dt::AVB_INTERFACE, 0).unwrap();
        let d = dec::decode_avb_interface(&bytes).expect("decodable");
        assert_eq!(d.mac_address, OUR_MAC);
        assert_eq!(
            d.interface_flags,
            if_flags::GPTP_SUPPORTED | if_flags::SRP_SUPPORTED
        );
        assert_eq!(d.interface_flags & if_flags::GPTP_GRANDMASTER_SUPPORTED, 0);
        // priority1 255 means we never win BMCA; the port is slave-only.
        assert_eq!(d.priority1, 255);
    }

    #[test]
    fn audio_map_is_an_identity_map_over_the_channels() {
        let e = entity();
        let bytes = e.encode_descriptor(dt::AUDIO_MAP, 0).unwrap();
        let d = dec::decode_audio_map(&bytes).expect("decodable");
        assert_eq!(d.mappings.len(), 8);
        for (k, mp) in d.mappings.iter().enumerate() {
            assert_eq!(mp.stream_index, 0);
            assert_eq!(mp.stream_channel, k as u16);
            assert_eq!(mp.cluster_offset, k as u16);
        }
    }

    #[test]
    fn read_descriptor_echoes_the_configuration_index_then_the_bytes() {
        let mut e = entity();
        let id = e.entity_id();
        let r = e.handle_aecp(
            &cmd(
                aem_cmd::READ_DESCRIPTOR,
                read_desc_payload(dt::ENTITY, 0),
                id,
            ),
            OUR_MAC,
            0,
        );
        assert_eq!(r.status, aecp_status::SUCCESS);
        assert_eq!(r.message_type, aecp_msg::AEM_RESPONSE);
        assert_eq!(r.sequence_id, 7, "the response must echo the sequence id");
        assert_eq!(r.payload.get(0..4), Some(&[0, 0, 0, 0][..]));
        // The descriptor bytes follow, starting with the descriptor type.
        assert_eq!(r.payload.get(4..6), Some(&dt::ENTITY.to_be_bytes()[..]));
    }

    #[test]
    fn read_descriptor_for_a_missing_one_is_no_such_descriptor() {
        let mut e = entity();
        let id = e.entity_id();
        // We have no STREAM_INPUT at all in this direction.
        let r = e.handle_aecp(
            &cmd(
                aem_cmd::READ_DESCRIPTOR,
                read_desc_payload(dt::STREAM_INPUT, 0),
                id,
            ),
            OUR_MAC,
            0,
        );
        assert_eq!(r.status, aecp_status::NO_SUCH_DESCRIPTOR);
        // And a cluster past the channel count.
        let r = e.handle_aecp(
            &cmd(
                aem_cmd::READ_DESCRIPTOR,
                read_desc_payload(dt::AUDIO_CLUSTER, 8),
                id,
            ),
            OUR_MAC,
            0,
        );
        assert_eq!(r.status, aecp_status::NO_SUCH_DESCRIPTOR);
    }

    #[test]
    fn a_command_for_another_entity_is_not_answered_as_ours() {
        let mut e = entity();
        let r = e.handle_aecp(
            &cmd(
                aem_cmd::READ_DESCRIPTOR,
                read_desc_payload(dt::ENTITY, 0),
                Eui64([1, 2, 3, 4, 5, 6, 7, 8]),
            ),
            OUR_MAC,
            0,
        );
        assert_eq!(r.status, aecp_status::NOT_IMPLEMENTED);
    }

    #[test]
    fn unknown_command_still_gets_a_response() {
        let mut e = entity();
        let id = e.entity_id();
        // ADC-10: never silence, or the controller retries to its timeout.
        let r = e.handle_aecp(&cmd(0x7FFF, vec![], id), OUR_MAC, 0);
        assert_eq!(r.status, aecp_status::NOT_IMPLEMENTED);
        assert_eq!(r.message_type, aecp_msg::AEM_RESPONSE);
        assert_eq!(r.command_type, 0x7FFF);
    }

    #[test]
    fn acquire_is_not_implemented_but_lock_works() {
        let mut e = entity();
        let id = e.entity_id();
        // Milan v1.2 §5.4.2.1: a PAAD-AE uses LOCK_ENTITY, not ACQUIRE.
        let r = e.handle_aecp(&cmd(aem_cmd::ACQUIRE_ENTITY, vec![], id), OUR_MAC, 0);
        assert_eq!(r.status, aecp_status::NOT_IMPLEMENTED);
        let r = e.handle_aecp(&cmd(aem_cmd::LOCK_ENTITY, vec![], id), OUR_MAC, 0);
        assert_eq!(r.status, aecp_status::SUCCESS);
        // A different controller is refused while the lock is held.
        let mut other = cmd(aem_cmd::LOCK_ENTITY, vec![], id);
        other.controller_entity_id = Eui64([1, 1, 1, 1, 1, 1, 1, 1]);
        let r = e.handle_aecp(&other, OUR_MAC, 0);
        assert_eq!(r.status, aecp_status::ENTITY_LOCKED);
    }

    #[test]
    fn get_configuration_is_zero_and_set_rejects_anything_else() {
        let mut e = entity();
        let id = e.entity_id();
        let r = e.handle_aecp(&cmd(aem_cmd::GET_CONFIGURATION, vec![], id), OUR_MAC, 0);
        assert_eq!(r.status, aecp_status::SUCCESS);
        assert_eq!(r.payload, vec![0, 0, 0, 0]);
        let ok = e.handle_aecp(
            &cmd(aem_cmd::SET_CONFIGURATION, vec![0, 0, 0, 0], id),
            OUR_MAC,
            0,
        );
        assert_eq!(ok.status, aecp_status::SUCCESS);
        let bad = e.handle_aecp(
            &cmd(aem_cmd::SET_CONFIGURATION, vec![0, 0, 0, 1], id),
            OUR_MAC,
            0,
        );
        assert_eq!(bad.status, aecp_status::BAD_ARGUMENTS);
    }

    #[test]
    fn get_stream_format_answers_for_our_output_only() {
        let mut e = entity();
        let id = e.entity_id();
        let mut p = dt::STREAM_OUTPUT.to_be_bytes().to_vec();
        p.extend_from_slice(&0u16.to_be_bytes());
        let r = e.handle_aecp(&cmd(aem_cmd::GET_STREAM_FORMAT, p, id), OUR_MAC, 0);
        assert_eq!(r.status, aecp_status::SUCCESS);
        assert_eq!(r.payload.get(4..12), Some(&FMT.0.to_be_bytes()[..]));
        // A STREAM_INPUT query has no answer in this direction.
        let mut p = dt::STREAM_INPUT.to_be_bytes().to_vec();
        p.extend_from_slice(&0u16.to_be_bytes());
        let r = e.handle_aecp(&cmd(aem_cmd::GET_STREAM_FORMAT, p, id), OUR_MAC, 0);
        assert_eq!(r.status, aecp_status::NO_SUCH_DESCRIPTOR);
    }

    /// GET_STREAM_INFO must answer NOT_IMPLEMENTED rather than originate a
    /// flags word whose bit positions we cannot verify. See the doc comment on
    /// `get_stream_info` for why this is deliberate.
    #[test]
    fn get_stream_info_is_refused_not_guessed() {
        let mut e = entity();
        let id = e.entity_id();
        e.set_stream_info(StreamInfo {
            connected: true,
            dest_mac: MacAddr([0x91, 0xe0, 0xf0, 0x01, 0x00, 0x01]),
            vlan_id: 2,
            msrp_accumulated_latency_ns: 0,
        });
        let mut p = dt::STREAM_OUTPUT.to_be_bytes().to_vec();
        p.extend_from_slice(&0u16.to_be_bytes());
        let r = e.handle_aecp(&cmd(aem_cmd::GET_STREAM_INFO, p, id), OUR_MAC, 0);
        assert_eq!(r.status, aecp_status::NOT_IMPLEMENTED);
        // Still a response, never silence (ADC-10).
        assert_eq!(r.message_type, aecp_msg::AEM_RESPONSE);
        assert_eq!(r.command_type, aem_cmd::GET_STREAM_INFO);
    }

    #[test]
    fn get_avb_info_carries_the_grandmaster_and_one_msrp_mapping() {
        let mut e = entity();
        let id = e.entity_id();
        e.set_gptp(GptpStatus {
            grandmaster_id: 0x000A_92FF_FE03_3BA9,
            domain_number: 0,
            as_capable: true,
            propagation_delay_ns: 137,
        });
        let r = e.handle_aecp(&cmd(aem_cmd::GET_AVB_INFO, vec![], id), OUR_MAC, 0);
        assert_eq!(r.status, aecp_status::SUCCESS);
        let b = &r.payload;
        assert_eq!(&b[0..2], &dt::AVB_INTERFACE.to_be_bytes());
        assert_eq!(
            u64::from_be_bytes(b[4..12].try_into().unwrap()),
            0x000A_92FF_FE03_3BA9
        );
        assert_eq!(u32::from_be_bytes(b[12..16].try_into().unwrap()), 137);
        assert_ne!(b[19] & 0b001, 0, "as_capable must be reported");
        assert_eq!(u16::from_be_bytes(b[20..22].try_into().unwrap()), 1);
        // The SR class A domain we actually declare: class 6, prio 3, VID 2.
        assert_eq!((b[24], b[25]), (6, 3));
        assert_eq!(u16::from_be_bytes(b[26..28].try_into().unwrap()), 2);
    }

    #[test]
    fn adp_reports_our_own_identity_as_gm_until_gptp_locks() {
        let mut e = entity();
        // Before gPTP, the GM id is our own clockIdentity, never 0 (§9.3).
        let adp = e.advertise();
        assert_eq!(adp.gptp_grandmaster_id, e.entity_id().to_u64());
        e.set_gptp(GptpStatus {
            grandmaster_id: 0x000A_92FF_FE03_3BA9,
            ..Default::default()
        });
        assert_eq!(e.advertise().gptp_grandmaster_id, 0x000A_92FF_FE03_3BA9);
    }

    #[test]
    fn departing_differs_from_available_only_in_message_type() {
        let mut e = entity();
        let a = e.advertise();
        let d = e.departing();
        assert_eq!(a.message_type, adp_msg::ENTITY_AVAILABLE);
        assert_eq!(d.message_type, adp_msg::ENTITY_DEPARTING);
        assert_eq!(a.entity_id, d.entity_id);
        assert_eq!(a.entity_model_id, d.entity_model_id);
    }

    #[test]
    fn set_format_while_connected_is_stream_is_running() {
        let mut e = entity();
        let id = e.entity_id();
        e.set_stream_info(StreamInfo {
            connected: true,
            ..Default::default()
        });
        let r = e.handle_aecp(
            &cmd(aem_cmd::SET_STREAM_FORMAT, vec![0; 12], id),
            OUR_MAC,
            0,
        );
        assert_eq!(r.status, aecp_status::STREAM_IS_RUNNING);
    }

    #[test]
    fn unsolicited_registration_is_capped() {
        let mut e = entity();
        let id = e.entity_id();
        for i in 0..crate::entity::control::MAX_UNSOL_CONTROLLERS {
            let mut c = cmd(aem_cmd::REGISTER_UNSOLICITED_NOTIFICATION, vec![], id);
            c.controller_entity_id = Eui64([0, 0, 0, 0, 0, 0, 0, i as u8]);
            assert_eq!(e.handle_aecp(&c, OUR_MAC, 0).status, aecp_status::SUCCESS);
        }
        let mut c = cmd(aem_cmd::REGISTER_UNSOLICITED_NOTIFICATION, vec![], id);
        c.controller_entity_id = Eui64([7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(
            e.handle_aecp(&c, OUR_MAC, 0).status,
            aecp_status::NO_RESOURCES
        );
    }

    /// Stream-ID construction, confirmed against the IEEE 1722.1 working
    /// group's clarification document (Illouz, Part 2, §5.7): the first six
    /// bytes are the talker's MAC and the last two a Unique ID chosen by the
    /// talker. A controller may not ask for a stream id whose first six bytes
    /// are not our MAC, so this is ours to get right.
    #[test]
    fn stream_id_is_our_mac_plus_a_unique_id() {
        let e = entity();
        let sid = e.model().stream_id;
        assert_eq!(sid.mac(), OUR_MAC);
        assert_eq!(sid.unique_id(), 0);
    }
}
