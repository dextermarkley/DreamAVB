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

use crate::acmp::Acmpdu;
use crate::adp::Adpdu;
use crate::aecp::AemPdu;
use crate::aecp::MvuPdu;
use crate::aem::desc;
use crate::aem::model::*;
use crate::constants::{acmp_flags, acmp_status};
use crate::constants::{
    adp_msg, aecp_msg, aecp_status, aem_cmd, clock_source, control_type, control_value_type,
    controller_caps, desc as dt, entity_caps, if_flags, listener_caps, stream_flags, talker_caps,
    NO_STRING,
};
use crate::derive::MODEL_ID_PREFIX;
use crate::entity::acmp_listener;
use crate::entity::control::ControlState;
use crate::entity::GptpStatus;
use crate::milan::{mvu_cmd, MilanInfo, MILAN_PROTOCOL_VERSION};

/// Advertisement `valid_time` (§9.3: 10 → 20 s validity).
pub const VALID_TIME: u8 = 10;

/// Our talker model: the AAF streams the Mac sources, described in the terms
/// the AEM needs. Built by [`talker_model`], which is the only constructor —
/// the channel count, rate and spf come from the stream format the talker
/// actually transmits, never from a literal (acceptance 3, and the project
/// rule against hardcoded rates/channels).
///
/// MULTI-STREAM. The 16R is 64 channels wide in each direction, carried as
/// eight 8-channel AAF streams, so the entity has to DESCRIBE eight
/// STREAM_OUTPUTs: a Milan listener will not bind a stream the entity does not
/// describe, which is what blocked the outbound side at one stream regardless
/// of how many the daemon was willing to packetize.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TalkerModel {
    pub entity_id: Eui64,
    pub entity_model_id: u64,
    pub mac: MacAddr,
    /// Our STREAM_OUTPUTs in descriptor-index order. `streams[i]` owns ring
    /// channels `i * channels ..`, which is the same layout the output ring
    /// and the daemon's gather use — one ordering, so the AEM a controller
    /// reads cannot disagree with what goes on the wire.
    pub streams: Vec<TalkerStream>,
    /// Our STREAM_INPUTs in descriptor-index order — the sinks a controller
    /// can bind a remote talker to.
    ///
    /// This entity used to advertise `listener_stream_sinks = 0` while the
    /// daemon was really receiving the 16R's audio, so a controller saw a
    /// transmit-only device and had nowhere to route anything TO us. The
    /// receive path existed; only the model denied it.
    pub sinks: Vec<TalkerSink>,
    /// Per-channel names for the INPUT side, length `sinks.len() * channels`.
    pub sink_channel_names: Vec<String>,
    pub current_format: StreamFormat,
    /// Every format we can source. Today exactly one: what we transmit.
    pub formats: Vec<StreamFormat>,
    /// Channels PER STREAM.
    pub channels: u16,
    /// Per-channel names across every stream, used for the AUDIO_CLUSTER
    /// descriptors. Length is `streams.len() * channels`.
    pub channel_names: Vec<String>,
    pub sampling_rates: Vec<u32>,
    pub current_sampling_rate: u32,
    pub firmware_version: String,
}

/// Fixed descriptor names.
///
/// These are constants rather than inline literals because GET_NAME must
/// answer exactly what READ_DESCRIPTOR reports. Two copies of the same string
/// in two encoders is precisely how that drifts apart -- the AVB_INTERFACE
/// clock quality had already drifted that way.
pub const ENTITY_NAME: &str = "AVB-Audio Mac";
pub const CONFIGURATION_NAME: &str = "Playback";
pub const AUDIO_UNIT_NAME: &str = "Mac audio";
pub const AVB_INTERFACE_NAME: &str = "I210";
pub const CLOCK_SOURCE_NAME: &str = "Mac internal";
pub const CLOCK_SOURCE_INPUT_STREAM_NAME: &str = "16R stream";
pub const CLOCK_SOURCE_GPTP_NAME: &str = "gPTP";
pub const CLOCK_DOMAIN_NAME: &str = "Domain";
pub const TIMING_NAME: &str = "Timing";
pub const PTP_INSTANCE_NAME: &str = "PTP Instance";
pub const PTP_PORT_NAME: &str = "PTP Port";
pub const IDENTIFY_CONTROL_NAME: &str = "Identify";
/// Name of the CRF STREAM_OUTPUT. Mirrors the 16R's own "Return CRF" sink
/// naming so a controller can see which end of the pair it is looking at.
pub const MEDIA_CLOCK_STREAM_NAME: &str = "Send CRF";

/// One STREAM_OUTPUT we source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TalkerStream {
    pub stream_id: StreamId,
    pub object_name: String,
    /// The format THIS output carries.
    ///
    /// Per-stream rather than taken from `TalkerModel::current_format`,
    /// because a media-clock (CRF) output and an audio (AAF) output are both
    /// STREAM_OUTPUTs in one index space. Describing every output with one
    /// shared format would report the CRF stream as AAF — and a listener that
    /// binds a media-clock stream expecting audio gets neither.
    pub format: StreamFormat,
}

impl TalkerStream {
    /// True when this output carries a media clock (CRF) rather than audio.
    ///
    /// Derived from the format rather than stored as a separate flag, so the
    /// two cannot contradict each other.
    pub fn is_media_clock(&self) -> bool {
        self.format.subtype() == avb_avtp::common::subtype::CRF
    }
}

/// One STREAM_INPUT we accept.
///
/// There is no `stream_id` here on purpose: a STREAM_INPUT descriptor does
/// not carry one. Which talker feeds a sink is ACMP binding state, not part
/// of the static model, so putting an id in the descriptor would publish a
/// binding the controller is supposed to own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TalkerSink {
    pub object_name: String,
}

impl TalkerModel {
    /// How many STREAM_OUTPUTs carry AUDIO.
    ///
    /// A media-clock (CRF) output has no audio channels, so it owns no
    /// AUDIO_CLUSTERs, no AUDIO_MAP and no STREAM_PORT_OUTPUT. Counting it as
    /// an audio stream would advertise `channels` phantom clusters per CRF
    /// stream and shift every input cluster's index.
    pub fn audio_stream_count(&self) -> u16 {
        u16::try_from(self.streams.iter().filter(|s| !s.is_media_clock()).count()).unwrap_or(0)
    }

    /// How many STREAM_OUTPUTs carry a media clock (CRF).
    pub fn media_clock_stream_count(&self) -> u16 {
        self.stream_count()
            .saturating_sub(self.audio_stream_count())
    }

    /// AUDIO_CLUSTERs belonging to the OUTPUT side, which occupy the low end
    /// of the shared cluster index space.
    pub fn out_cluster_count(&self) -> u16 {
        self.audio_stream_count().saturating_mul(self.channels)
    }

    /// AUDIO_CLUSTERs belonging to the INPUT side.
    pub fn in_cluster_count(&self) -> u16 {
        u16::try_from(self.sinks.len())
            .unwrap_or(0)
            .saturating_mul(self.channels)
    }

    /// TOTAL AUDIO_CLUSTERs across both directions.
    ///
    /// AUDIO_CLUSTER and AUDIO_MAP share ONE index space per configuration in
    /// 1722.1 — they are not namespaced by direction. So the input clusters
    /// have to sit ABOVE the output ones and the input port's `base_cluster`
    /// has to point past them. Numbering both from 0 would make cluster 0
    /// simultaneously our first output channel and our first input channel,
    /// and a controller reading the tree would silently get the output one.
    pub fn cluster_count(&self) -> u16 {
        self.out_cluster_count()
            .saturating_add(self.in_cluster_count())
    }

    /// How many STREAM_OUTPUTs we describe.
    pub fn stream_count(&self) -> u16 {
        u16::try_from(self.streams.len()).unwrap_or(0)
    }

    /// How many STREAM_INPUTs we describe.
    pub fn sink_count(&self) -> u16 {
        u16::try_from(self.sinks.len()).unwrap_or(0)
    }

    /// Total AUDIO_MAPs: one per AUDIO stream in each direction, sharing one
    /// index space the same way clusters do. A media-clock output has no map.
    pub fn map_count(&self) -> u16 {
        self.audio_stream_count().saturating_add(self.sink_count())
    }

    /// The first stream's id, for the places that still need a single handle
    /// (the ADP's entity identity is the entity id, not a stream).
    pub fn first_stream_id(&self) -> StreamId {
        self.streams
            .first()
            .map(|s| s.stream_id)
            .unwrap_or(StreamId(0))
    }

    /// The media-clock (CRF) STREAM_OUTPUT's descriptor index and stream id,
    /// if we source one.
    pub fn media_clock_stream(&self) -> Option<(u16, StreamId)> {
        let i = self.streams.iter().position(|s| s.is_media_clock())?;
        Some((u16::try_from(i).ok()?, self.streams.get(i)?.stream_id))
    }

    /// Append a Milan Clock Reference Format STREAM_OUTPUT (Milan v1.3 §7.3.1)
    /// and recompute the model id.
    ///
    /// Appended LAST so every audio stream keeps its descriptor index, which
    /// the output ports' `base_cluster = index * channels` and the output
    /// maps' `stream_index` both depend on. `audio_streams_come_first` pins
    /// that ordering.
    pub fn with_media_clock_stream(mut self, stream_id: StreamId) -> Self {
        self.streams.push(TalkerStream {
            stream_id,
            object_name: MEDIA_CLOCK_STREAM_NAME.to_string(),
            format: StreamFormat(avb_avtp::crf::milan::FORMAT),
        });
        self.entity_model_id = compute_talker_model_id(&self);
        self
    }

    /// Every audio stream sits below every media-clock stream in the
    /// STREAM_OUTPUT index space.
    pub fn audio_streams_come_first(&self) -> bool {
        let first_mc = self
            .streams
            .iter()
            .position(|s| s.is_media_clock())
            .unwrap_or(self.streams.len());
        self.streams
            .iter()
            .skip(first_mc)
            .all(TalkerStream::is_media_clock)
    }
}

/// Build our talker model from the format we actually transmit.
///
/// `fmt` must be a decodable AAF format — the caller already has one, since it
/// is packetizing with it. Returns `None` for a format whose AAF fields cannot
/// be read, rather than inventing a channel count.
///
/// `sink_count` is how many STREAM_INPUTs to describe, which the caller takes
/// from the inbound sources it actually bound (never a constant — the count
/// has to match what the receive path can really absorb, or we advertise a
/// sink that silently drops everything routed to it). Pass 0 for a
/// transmit-only configuration.
pub fn talker_model(
    our_mac: MacAddr,
    stream_ids: &[StreamId],
    fmt: StreamFormat,
    firmware_version: &str,
    sink_count: usize,
) -> Option<TalkerModel> {
    let aaf = fmt.aaf()?;
    let hz = aaf.nsr_hz()?;
    let channels = aaf.channels;
    if stream_ids.is_empty() {
        return None;
    }
    let streams: Vec<TalkerStream> = stream_ids
        .iter()
        .enumerate()
        .map(|(i, &stream_id)| TalkerStream {
            stream_id,
            // Named by the channel range it carries, so a controller's view
            // lines up with the 16R's own "Send 1-8" style labels.
            object_name: format!(
                "Mac→16R {}-{}",
                i * usize::from(channels) + 1,
                (i + 1) * usize::from(channels)
            ),
            format: fmt,
        })
        .collect();
    // Channel names are positional and span every stream: we are the source,
    // so there is no upstream cluster name to inherit the way the listener
    // inherits the 16R's.
    let total = streams.len() * usize::from(channels);
    let channel_names = (0..total).map(|k| format!("Mac ch{}", k + 1)).collect();
    // The input side, named from the 16R's perspective since that is what a
    // controller sees arriving: these are its sends landing on our sinks.
    let sinks: Vec<TalkerSink> = (0..sink_count)
        .map(|i| TalkerSink {
            object_name: format!(
                "16R→Mac {}-{}",
                i * usize::from(channels) + 1,
                (i + 1) * usize::from(channels)
            ),
        })
        .collect();
    let sink_total = sinks.len() * usize::from(channels);
    let sink_channel_names = (0..sink_total)
        .map(|k| format!("16R ch{}", k + 1))
        .collect();
    let mut m = TalkerModel {
        entity_id: Eui64::from_mac(our_mac),
        entity_model_id: 0, // filled below
        mac: our_mac,
        streams,
        sinks,
        sink_channel_names,
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

/// `MODEL_ID_PREFIX || fnv1a32(canonical model)`, the same scheme as the listener
/// model id (§9.1) so both directions key a controller's cache the same way.
/// The canonical form is the stream format, the channel count and the rates —
/// so the id changes iff the model a controller would read changes.
pub fn compute_talker_model_id(m: &TalkerModel) -> u64 {
    let mut canon = Vec::new();
    canon.extend_from_slice(b"talker");
    canon.extend_from_slice(&m.current_format.0.to_be_bytes());
    canon.extend_from_slice(&m.channels.to_be_bytes());
    // The STREAM COUNT is part of the identity: a controller caches the model
    // by this id, so a one-stream and an eight-stream entity must not share
    // one — it would read a stale 8-channel model for a 64-channel device.
    canon.extend_from_slice(&m.stream_count().to_be_bytes());
    // And every stream's OWN format, so adding a media-clock (CRF) output
    // changes the id. The AEM tree a controller caches gains a STREAM_OUTPUT
    // whose format is not AAF; a controller holding the cached audio-only
    // tree would otherwise keep describing that output as AAF forever.
    for s in &m.streams {
        canon.extend_from_slice(&s.format.0.to_be_bytes());
    }
    // And so is the SINK count, for exactly the same reason. Gaining
    // STREAM_INPUT and STREAM_PORT_INPUT descriptors changes the AEM tree, so
    // a transmit-only configuration and a duplex one must not share a model
    // id — a controller holding the cached transmit-only tree would never
    // learn we have sinks, and the ADP's listener_stream_sinks would
    // contradict the model it believes it already has.
    canon.extend_from_slice(&m.sink_count().to_be_bytes());
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

/// Descriptor index of our Milan IDENTIFY control.
///
/// `descriptor_counts` advertises exactly one CONTROL, so the only valid index
/// is 0. Named rather than written as a bare literal because it appears in two
/// unrelated places — the ADPDU's `identify_control_index` and the descriptor
/// walk — and they must not be able to drift apart.
pub const IDENTIFY_CONTROL_INDEX: u16 = 0;

/// Our talker entity.
#[derive(Clone, Debug)]
pub struct TalkerEntity {
    model: TalkerModel,
    gptp: GptpStatus,
    /// Run-time counters, pushed by the daemon. The entity never counts
    /// anything itself: the events live in the dext, the gPTP port and the
    /// receive path.
    counters: crate::counters::EntityCounters,
    /// Controller-set names, keyed by (descriptor_type, index, name_index).
    /// An override here wins over the model's own name in EVERY descriptor
    /// read, so SET_NAME is observable the way a controller expects.
    names: std::collections::BTreeMap<(u16, u16, u16), String>,
    /// The IDENTIFY control's current value: 0 off, 255 on.
    identify: u8,
    /// One Milan ACMP listener state machine per STREAM_INPUT (§5.5.3.3).
    /// Built from the model's sinks, so a transmit-only entity has none and
    /// every listener-side command it receives is LISTENER_UNKNOWN_ID.
    sink_sms: Vec<acmp_listener::ListenerSm>,
    available_index: u32,
    control: ControlState,
    stream: StreamInfo,
}

impl TalkerEntity {
    pub fn new(model: TalkerModel) -> Self {
        let sink_sms = (0..model.sink_count())
            .map(acmp_listener::ListenerSm::new)
            .collect();
        Self {
            model,
            gptp: GptpStatus::default(),
            counters: crate::counters::EntityCounters::default(),
            names: std::collections::BTreeMap::new(),
            identify: 0,
            sink_sms,
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

    /// Publish a fresh counter snapshot for GET_COUNTERS to answer from.
    pub fn set_counters(&mut self, c: crate::counters::EntityCounters) {
        self.counters = c;
    }

    pub fn counters(&self) -> &crate::counters::EntityCounters {
        &self.counters
    }

    pub fn set_gptp(&mut self, gptp: GptpStatus) {
        self.gptp = gptp;
    }

    /// The grandmaster id an advertisement WOULD carry, without building or
    /// counting one.
    ///
    /// This exists because reading it via `advertise()` is a trap: that method
    /// increments `available_index`, which IEEE 1722.1 requires to advance by
    /// exactly one per ENTITY_AVAILABLE actually sent. A caller polling the
    /// grandmaster on a hot loop to detect a change bumped the counter
    /// thousands of times per advertisement, so every ADPDU we sent carried a
    /// huge jump — which a controller must read as "this entity restarted" and
    /// answer by discarding its whole model of us. Mirrors `build_adp`'s own
    /// fallback to our clockIdentity before the port is Slave.
    pub fn advertised_grandmaster_id(&self) -> u64 {
        if self.gptp.grandmaster_id != 0 {
            self.gptp.grandmaster_id
        } else {
            self.model.entity_id.to_u64()
        }
    }

    /// The current `available_index`, for tests and diagnostics. Reading it
    /// does not advance it.
    pub fn available_index(&self) -> u32 {
        self.available_index
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
            // We answer the Milan MVU GET_MILAN_INFO, which is how a Milan
            // controller decides an entity is a Milan device at all. Without
            // this bit a controller has no reason to ask, and a talker it
            // cannot identify as Milan is one it will not offer for routing.
            // Only claimed because `handle_mvu` actually answers.
            | entity_caps::VENDOR_UNIQUE_SUPPORTED
            // Our CONTROL descriptor IS a Milan IDENTIFY control, and this is
            // the bit that says `identify_control_index` means anything. Both
            // are needed: without the flag a controller must ignore the index,
            // so an entity carrying a perfectly good IDENTIFY control looks
            // like one that has none.
            //
            // Found by diffing our ADPDU against the 16R's own on the wire.
            // The 16R sets exactly this bit, and once we set it too both
            // words read 0xc588 (ours was 0x8588 before this bit existed);
            // it carries CONTROL x1 at index 0, the same shape we
            // do — so this is transcribed from a device whose own controller
            // lists it as online, not reasoned from the spec.
            //
            // Only claimed because `descriptor_counts` advertises
            // (CONTROL, 1) and `encode_descriptor` answers it; the test below
            // ties the two together so the bit cannot outlive the descriptor.
            | entity_caps::AEM_IDENTIFY_CONTROL_INDEX_VALID
    }

    /// Talker capabilities, derived from what we actually transmit.
    ///
    /// MEDIA_CLOCK_SOURCE is claimed only when the model really describes a
    /// CRF STREAM_OUTPUT, so the bit cannot outlive the stream that justifies
    /// it — the same rule the IDENTIFY flag above follows.
    ///
    /// Found by diffing our ADPDU against the 16R's own on the wire while
    /// transmitting CRF:
    ///
    /// ```text
    /// 16R  talker_caps = 0x4801  IMPLEMENTED | MEDIA_CLOCK_SOURCE | AUDIO_SOURCE
    /// ours talker_caps = 0x4001  IMPLEMENTED |                      AUDIO_SOURCE
    /// ```
    ///
    /// A single bit, 0x0800, was the whole difference between the two words.
    /// We had built the CRF talker and bound it to the device's Return CRF
    /// sink, and the device media-locked to it — while our own ADP still said
    /// we were not a media clock source. A controller filtering discovered
    /// entities for a clock source has no reason to offer one that denies
    /// being one, so this is a capability we implemented and then withheld.
    pub fn talker_capabilities(&self) -> u16 {
        let mut caps = talker_caps::IMPLEMENTED | talker_caps::AUDIO_SOURCE;
        if self.model.media_clock_stream_count() > 0 {
            caps |= talker_caps::MEDIA_CLOCK_SOURCE;
        }
        caps
    }

    /// CLOCK_SOURCE descriptors: INTERNAL and gPTP always, plus INPUT_STREAM
    /// once there is a sink from which the daemon can recover media clock.
    /// Milan v1.3 §7.5 requires the gPTP source to point through
    /// TIMING → PTP_INSTANCE → PTP_PORT, all of which we now encode from the
    /// 16R's captured descriptor layouts.
    pub fn clock_source_count(&self) -> u16 {
        if self.model.sink_count() > 0 {
            3
        } else {
            2
        }
    }

    fn gptp_clock_source_index(&self) -> u16 {
        if self.model.sink_count() > 0 {
            2
        } else {
            1
        }
    }

    /// Listener capabilities, derived from whether we actually have sinks.
    ///
    /// Zero when `sinks` is empty. IMPLEMENTED is a claim that this entity
    /// implements the listener role, so asserting it with nowhere to route to
    /// would leave a controller offering a connection that must then fail.
    pub fn listener_capabilities(&self) -> u16 {
        if self.model.sink_count() == 0 {
            return 0;
        }
        listener_caps::IMPLEMENTED | listener_caps::AUDIO_SINK
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
            talker_stream_sources: self.model.stream_count(),
            talker_capabilities: self.talker_capabilities(),
            listener_stream_sinks: self.model.sink_count(),
            listener_capabilities: self.listener_capabilities(),
            controller_capabilities: controller_caps::IMPLEMENTED,
            available_index: self.available_index,
            gptp_grandmaster_id: gm,
            gptp_domain_number: self.gptp.domain_number,
            identify_control_index: IDENTIFY_CONTROL_INDEX,
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
        let mut v = vec![
            (dt::AUDIO_UNIT, 1),
            // One STREAM_OUTPUT per stream we source — audio and the
            // media clock alike. One STREAM_PORT_OUTPUT per AUDIO stream
            // only: a CRF output carries no audio channels, so it has no
            // port, no clusters and no map.
            (dt::STREAM_OUTPUT, self.model.stream_count()),
            (dt::STREAM_PORT_OUTPUT, self.model.audio_stream_count()),
        ];
        // The input side is omitted ENTIRELY rather than declared as zero when
        // we have no sinks: a count of 0 is a claim that the descriptor type
        // exists and is empty, which invites a controller to read index 0.
        if self.model.sink_count() > 0 {
            v.push((dt::STREAM_INPUT, self.model.sink_count()));
            v.push((dt::STREAM_PORT_INPUT, self.model.sink_count()));
        }
        v.extend([
            // Clusters and maps span BOTH directions in one index space.
            (dt::AUDIO_CLUSTER, self.model.cluster_count()),
            (dt::AUDIO_MAP, self.model.map_count()),
            // Milan's IDENTIFY control. Advertised because the 16R stalls its
            // enumeration without it, and answerable because the descriptor is
            // transcribed from the 16R's own.
            (dt::CONTROL, 1),
            (dt::AVB_INTERFACE, 1),
            (dt::CLOCK_SOURCE, self.clock_source_count()),
            (dt::CLOCK_DOMAIN, 1),
            (dt::TIMING, 1),
            (dt::PTP_INSTANCE, 1),
            (dt::PTP_PORT, 1),
        ]);
        v
    }

    /// Answer a Milan Vendor-Unique (MVU) command.
    ///
    /// This is how a Milan controller decides we are a Milan device: it sends
    /// GET_MILAN_INFO and reads the protocol version back. Before this existed
    /// the daemon dropped every VENDOR_UNIQUE AECPDU without replying, so a
    /// controller got silence — indistinguishable from a device that is not
    /// there, and no reason to offer our streams for routing.
    ///
    /// An unknown MVU command gets NOT_IMPLEMENTED rather than silence, so a
    /// controller learns we speak MVU and simply lack that one command.
    pub fn handle_mvu(&self, cmd: &MvuPdu) -> MvuPdu {
        let mut resp = cmd.clone();
        resp.message_type = aecp_msg::VENDOR_UNIQUE_RESPONSE;
        if cmd.target_entity_id != self.model.entity_id
            || cmd.message_type != aecp_msg::VENDOR_UNIQUE_COMMAND
        {
            resp.status = aecp_status::NOT_IMPLEMENTED;
            return resp;
        }
        match cmd.command_type {
            mvu_cmd::GET_MILAN_INFO => {
                resp.status = aecp_status::SUCCESS;
                resp.payload = MilanInfo {
                    protocol_version: MILAN_PROTOCOL_VERSION,
                    // No optional Milan features claimed. The 16R claims none
                    // either, so this is the shape a working peer sends.
                    features_flags: 0,
                    // 0 = NOT certified, which we are not. The 16R reports
                    // 1.1.0.0 here; claiming a certification we do not hold
                    // would be a false statement to every controller.
                    certification_version: 0,
                    // Which spec revision we are WRITTEN AGAINST -- a
                    // different claim from certification, and §5.4.4.1 makes
                    // it mandatory. Leaving it 0 (as the zero-padded payload
                    // did) tells a controller nothing about which Milan
                    // revision to speak to us.
                    specification_version: crate::milan::MILAN_SPECIFICATION_VERSION,
                }
                .encode();
            }
            _ => resp.status = aecp_status::NOT_IMPLEMENTED,
        }
        resp
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
            aem_cmd::GET_NAME => self.get_name(cmd),
            aem_cmd::SET_NAME => {
                // Milan v1.3 §5.4.2.11: refuse from a controller that is not
                // the lock holder, and do not change a user name by any
                // non-ATDECC means either -- which we satisfy trivially, as
                // there is no front panel or vendor app in this stack.
                if self.control.locked_by_other(ctrl) {
                    (aecp_status::ENTITY_LOCKED, echo())
                } else {
                    self.set_name(cmd)
                }
            }
            aem_cmd::GET_CONTROL => self.get_control(cmd),
            aem_cmd::SET_CONTROL => {
                if self.control.locked_by_other(ctrl) {
                    (aecp_status::ENTITY_LOCKED, echo())
                } else {
                    self.set_control(cmd)
                }
            }
            aem_cmd::START_STREAMING | aem_cmd::STOP_STREAMING => {
                if self.control.locked_by_other(ctrl) {
                    (aecp_status::ENTITY_LOCKED, echo())
                } else {
                    self.streaming_cmd(cmd, cmd.command_type == aem_cmd::START_STREAMING)
                }
            }
            aem_cmd::GET_AVB_INFO => self.get_avb_info(cmd),
            aem_cmd::GET_PTP_INSTANCE_GRANDMASTER_INFO => {
                self.get_ptp_instance_grandmaster_info(cmd)
            }
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
                // Per-descriptor counters, Milan v1.3 §5.4.2.25: supported for
                // each AVB Interface, Clock Domain, Stream Input and Stream
                // Output of the current configuration.
                //
                // This used to answer SUCCESS with an all-zero 128-byte block
                // and `counters_valid = 0` for ANY descriptor type — which
                // told a controller "I know nothing" about counters the spec
                // makes mandatory, and told it so even for descriptor types
                // that do not exist on this entity.
                let (ty, index) = match (be_u16(&cmd.payload, 0), be_u16(&cmd.payload, 2)) {
                    (Some(t), Some(i)) => (t, i),
                    _ => return (aecp_status::BAD_ARGUMENTS, echo()),
                };
                let block = match ty {
                    dt::AVB_INTERFACE if index == 0 => Some(self.counters.avb_interface_block()),
                    dt::CLOCK_DOMAIN if index == 0 => Some(self.counters.clock_domain_block()),
                    dt::STREAM_INPUT if index < self.model.sink_count() => {
                        self.counters.stream_input_block(index)
                    }
                    dt::STREAM_OUTPUT if index < self.model.stream_count() => {
                        self.counters.stream_output_block(index)
                    }
                    // ENTITY GET_COUNTERS is deliberately not implemented:
                    // Milan mandates the four types above and not this one
                    // (see ENTITY_GET_COUNTERS_IMPLEMENTED).
                    dt::ENTITY => return (aecp_status::NOT_IMPLEMENTED, echo()),
                    _ => None,
                };
                match block {
                    Some(b) => (aecp_status::SUCCESS, b.encode_payload(ty, index)),
                    // A descriptor we do not have is NO_SUCH_DESCRIPTOR, not
                    // an empty success — a controller must be able to tell a
                    // missing descriptor from one with nothing to report.
                    None => (aecp_status::NO_SUCH_DESCRIPTOR, echo()),
                }
            }
            // Never silence: an unimplemented command still gets a response
            // (ADC-10), or the controller retries until it times out.
            _ => (aecp_status::NOT_IMPLEMENTED, echo()),
        }
    }

    /// How many names a descriptor type has, and whether we have it.
    ///
    /// Milan requires GET_NAME/SET_NAME for *all* the names in the descriptors
    /// we implement, so this is derived from the descriptor tree rather than
    /// listed by hand: a type answers names exactly when it answers
    /// READ_DESCRIPTOR. ENTITY is the only one with two names
    /// (`entity_name` and `group_name`); every other named descriptor has
    /// just `object_name`.
    fn name_count(&self, ty: u16, index: u16) -> Option<u16> {
        self.encode_descriptor(ty, index)?;
        match ty {
            dt::ENTITY => Some(2),
            // These carry no name field at all, so a GET_NAME against them is
            // BAD_ARGUMENTS rather than an empty string.
            dt::STREAM_PORT_INPUT | dt::STREAM_PORT_OUTPUT | dt::AUDIO_MAP => None,
            _ => Some(1),
        }
    }

    /// An AUDIO_CLUSTER's default name. Output clusters occupy the low half
    /// of the shared index space and are named from our side; input clusters
    /// sit above them and are named from the 16R's.
    fn cluster_name(&self, index: u16) -> Option<String> {
        let m = &self.model;
        if index >= m.cluster_count() {
            return None;
        }
        let out_n = m.out_cluster_count();
        Some(if index < out_n {
            m.channel_names
                .get(usize::from(index))
                .cloned()
                .unwrap_or_else(|| format!("Mac ch{}", index + 1))
        } else {
            let j = usize::from(index.saturating_sub(out_n));
            m.sink_channel_names
                .get(j)
                .cloned()
                .unwrap_or_else(|| format!("16R ch{}", j + 1))
        })
    }

    /// A descriptor's name: the controller's SET_NAME override when present,
    /// otherwise the given default.
    fn name_of(&self, ty: u16, index: u16, default: &str) -> String {
        self.names
            .get(&(ty, index, 0))
            .cloned()
            .unwrap_or_else(|| default.to_string())
    }

    /// The name the MODEL would report, with no override applied. Kept beside
    /// the descriptor encoders so the two cannot drift: a GET_NAME that
    /// answered a different string than READ_DESCRIPTOR would be worse than
    /// not answering.
    fn default_name(&self, ty: u16, index: u16, name_index: u16) -> Option<String> {
        let m = &self.model;
        Some(match (ty, name_index) {
            (dt::ENTITY, 0) => ENTITY_NAME.to_string(),
            // group_name is deliberately empty: we belong to no group, and
            // inventing one would put a label in a controller's UI that the
            // user never chose.
            (dt::ENTITY, 1) => String::new(),
            (dt::CONFIGURATION, _) => CONFIGURATION_NAME.to_string(),
            (dt::AUDIO_UNIT, _) => AUDIO_UNIT_NAME.to_string(),
            (dt::AVB_INTERFACE, _) => AVB_INTERFACE_NAME.to_string(),
            (dt::CLOCK_SOURCE, _) => {
                if index == 0 {
                    CLOCK_SOURCE_NAME.to_string()
                } else if index == self.gptp_clock_source_index() {
                    CLOCK_SOURCE_GPTP_NAME.to_string()
                } else {
                    CLOCK_SOURCE_INPUT_STREAM_NAME.to_string()
                }
            }
            (dt::CLOCK_DOMAIN, _) => CLOCK_DOMAIN_NAME.to_string(),
            (dt::TIMING, _) => TIMING_NAME.to_string(),
            (dt::PTP_INSTANCE, _) => PTP_INSTANCE_NAME.to_string(),
            (dt::PTP_PORT, _) => PTP_PORT_NAME.to_string(),
            (dt::CONTROL, _) => IDENTIFY_CONTROL_NAME.to_string(),
            (dt::STREAM_OUTPUT, _) => m.streams.get(usize::from(index))?.object_name.clone(),
            (dt::STREAM_INPUT, _) => m.sinks.get(usize::from(index))?.object_name.clone(),
            (dt::AUDIO_CLUSTER, _) => self.cluster_name(index)?,
            _ => return None,
        })
    }

    /// The name a descriptor currently reports: the controller's override if
    /// one was set, otherwise the model's own.
    fn current_name(&self, ty: u16, index: u16, name_index: u16) -> Option<String> {
        if let Some(s) = self.names.get(&(ty, index, name_index)) {
            return Some(s.clone());
        }
        self.default_name(ty, index, name_index)
    }

    fn get_name(&self, cmd: &AemPdu) -> (u8, Vec<u8>) {
        // descriptor_type(2) descriptor_index(2) name_index(2) config_index(2)
        let (Some(ty), Some(index), Some(ni), Some(cfg)) = (
            be_u16(&cmd.payload, 0),
            be_u16(&cmd.payload, 2),
            be_u16(&cmd.payload, 4),
            be_u16(&cmd.payload, 6),
        ) else {
            return (aecp_status::BAD_ARGUMENTS, cmd.payload.clone());
        };
        if cfg != 0 {
            return (aecp_status::NO_SUCH_DESCRIPTOR, cmd.payload.clone());
        }
        let Some(n) = self.name_count(ty, index) else {
            return (aecp_status::NO_SUCH_DESCRIPTOR, cmd.payload.clone());
        };
        if ni >= n {
            return (aecp_status::BAD_ARGUMENTS, cmd.payload.clone());
        }
        let name = self.current_name(ty, index, ni).unwrap_or_default();
        let mut p = cmd.payload.get(0..8).unwrap_or(&[0; 8]).to_vec();
        p.resize(8, 0);
        p.extend_from_slice(&desc::name_to_bytes(&name));
        (aecp_status::SUCCESS, p)
    }

    fn set_name(&mut self, cmd: &AemPdu) -> (u8, Vec<u8>) {
        let (Some(ty), Some(index), Some(ni), Some(cfg)) = (
            be_u16(&cmd.payload, 0),
            be_u16(&cmd.payload, 2),
            be_u16(&cmd.payload, 4),
            be_u16(&cmd.payload, 6),
        ) else {
            return (aecp_status::BAD_ARGUMENTS, cmd.payload.clone());
        };
        if cfg != 0 {
            return (aecp_status::NO_SUCH_DESCRIPTOR, cmd.payload.clone());
        }
        let Some(n) = self.name_count(ty, index) else {
            return (aecp_status::NO_SUCH_DESCRIPTOR, cmd.payload.clone());
        };
        if ni >= n {
            return (aecp_status::BAD_ARGUMENTS, cmd.payload.clone());
        }
        let Some(field) = cmd.payload.get(8..8 + desc::NAME_LEN) else {
            return (aecp_status::BAD_ARGUMENTS, cmd.payload.clone());
        };
        let name = desc::name_from_bytes(field);
        // Stored as an override so READ_DESCRIPTOR reports it too. A SET_NAME
        // that only echoed the new name back, while the descriptor kept
        // reporting the old one, would be a lie a controller cannot detect
        // until it re-reads the tree.
        self.names.insert((ty, index, ni), name);
        (aecp_status::SUCCESS, cmd.payload.clone())
    }

    /// Our only CONTROL is the Milan IDENTIFY control at index 0.
    fn get_control(&self, cmd: &AemPdu) -> (u8, Vec<u8>) {
        let (Some(ty), Some(index)) = (be_u16(&cmd.payload, 0), be_u16(&cmd.payload, 2)) else {
            return (aecp_status::BAD_ARGUMENTS, cmd.payload.clone());
        };
        if ty != dt::CONTROL || index != 0 {
            return (aecp_status::NO_SUCH_DESCRIPTOR, cmd.payload.clone());
        }
        let mut p = cmd.payload.get(0..4).unwrap_or(&[0; 4]).to_vec();
        p.resize(4, 0);
        // LINEAR_UINT8 with number_of_values = 1, so the values block is the
        // single current value -- matching the CONTROL descriptor we publish.
        p.push(self.identify);
        (aecp_status::SUCCESS, p)
    }

    fn set_control(&mut self, cmd: &AemPdu) -> (u8, Vec<u8>) {
        let (Some(ty), Some(index)) = (be_u16(&cmd.payload, 0), be_u16(&cmd.payload, 2)) else {
            return (aecp_status::BAD_ARGUMENTS, cmd.payload.clone());
        };
        if ty != dt::CONTROL || index != 0 {
            return (aecp_status::NO_SUCH_DESCRIPTOR, cmd.payload.clone());
        }
        let Some(&v) = cmd.payload.get(4) else {
            return (aecp_status::BAD_ARGUMENTS, cmd.payload.clone());
        };
        // 1722.1 §7.3.5.2: the IDENTIFY control is off at 0 and on at 255.
        // Anything between is not a dimmer -- it is a controller that has
        // misunderstood the control -- so it is refused rather than rounded.
        if v != 0 && v != 0xFF {
            return (aecp_status::BAD_ARGUMENTS, cmd.payload.clone());
        }
        self.identify = v;
        (aecp_status::SUCCESS, cmd.payload.clone())
    }

    /// Answer a listener-side ACMP command addressed to us (Milan v1.3
    /// §5.5.3.1), and return whatever the sink's state machine wants done.
    ///
    /// We advertise `listener_stream_sinks` and `listener_capabilities`, which
    /// obliges us to answer these — a controller that binds a talker to one of
    /// our sinks and hears nothing waits out its command timeout and concludes
    /// the entity is broken, which is a worse answer than a refusal.
    ///
    /// `None` means "not ours, stay silent": §5.5.3.1 ends with *"The listener
    /// shall ignore all other ACMP messages"*, and a response to a command
    /// addressed elsewhere would be noise on a shared multicast address.
    pub fn handle_acmp_listener(
        &mut self,
        cmd: &Acmpdu,
        now_ns: u64,
        rand01_ns: u64,
    ) -> Option<(Acmpdu, Vec<acmp_listener::Action>)> {
        use crate::constants::acmp_msg as m;

        let reply_type = match cmd.message_type {
            m::CONNECT_RX_COMMAND => m::CONNECT_RX_RESPONSE,
            m::DISCONNECT_RX_COMMAND => m::DISCONNECT_RX_RESPONSE,
            m::GET_RX_STATE_COMMAND => m::GET_RX_STATE_RESPONSE,
            _ => return None,
        };
        // Listener-side commands route by listener_entity_id.
        if cmd.listener_entity_id != self.model.entity_id {
            return None;
        }

        // Echo every routing field: a controller matches a response to its
        // command by (sequence_id, ids), so inventing any of them makes the
        // response unmatchable. Individual tables below override what they
        // must.
        let mut r = Acmpdu {
            message_type: reply_type,
            status: acmp_status::SUCCESS,
            ..*cmd
        };

        // §5.5.3.1 step 1 / Table 5.27: an invalid listener_unique_id is
        // LISTENER_UNKNOWN_ID, and is checked BEFORE the lock and before the
        // state machine — a sink that does not exist cannot have a state.
        let Some(slot) = usize::from(cmd.listener_unique_id)
            .checked_sub(0)
            .filter(|&i| i < self.sink_sms.len())
        else {
            r.status = acmp_status::LISTENER_UNKNOWN_ID;
            return Some((r, Vec::new()));
        };

        // §5.5.3.6.3 / §5.5.3.6.45 step 1, Table 5.32: a bind or unbind from
        // anyone but the lock holder is CONTROLLER_NOT_AUTHORIZED. Note this
        // is NOT the AECP ENTITY_LOCKED — that is the status for the MVU
        // BIND_STREAM form (Table 5.33), a different message.
        //
        // GET_RX_STATE is deliberately NOT gated: it is a read, and no clause
        // restricts it to the lock holder.
        let mutating = reply_type != m::GET_RX_STATE_RESPONSE;
        if mutating && self.control.locked_by_other(cmd.controller_entity_id) {
            r.status = acmp_status::CONTROLLER_NOT_AUTHORIZED;
            return Some((r, Vec::new()));
        }

        let sm = self.sink_sms.get_mut(slot)?;
        match reply_type {
            m::GET_RX_STATE_RESPONSE => {
                match sm.bound_to() {
                    // Table 5.44: report the binding and the SRP parameters.
                    Some(b) => {
                        let eff = sm.effective();
                        r.talker_entity_id = b.talker_entity_id;
                        r.talker_unique_id = b.talker_unique_id;
                        r.connection_count = 1;
                        r.stream_id = eff.stream_id;
                        r.stream_dest_mac = eff.dest_mac;
                        r.stream_vlan_id = eff.vlan_id;
                        let mut f = acmp_flags::FAST_CONNECT;
                        if sm.streaming_wait() {
                            f |= acmp_flags::STREAMING_WAIT;
                        }
                        // REGISTERING_FAILED per Table 5.23: set while the
                        // sink's MSRP declaration is AskingFailed.
                        if sm.declaration() == acmp_listener::Declaration::AskingFailed {
                            f |= acmp_flags::REGISTERING_FAILED;
                        }
                        r.flags = f;
                    }
                    // Table 5.37: an UNBOUND sink ZEROES the talker fields
                    // rather than echoing them. Echoing would report a
                    // binding to whatever talker the controller happened to
                    // name in its query.
                    None => {
                        r.talker_entity_id = Eui64([0; 8]);
                        r.talker_unique_id = 0;
                        r.connection_count = 0;
                        r.stream_id = StreamId(0);
                        r.stream_dest_mac = MacAddr([0; 6]);
                        r.stream_vlan_id = 0;
                        r.flags = 0;
                    }
                }
                Some((r, Vec::new()))
            }
            _ => {
                let ev = if reply_type == m::CONNECT_RX_RESPONSE {
                    acmp_listener::Event::BindRx {
                        talker_entity_id: cmd.talker_entity_id,
                        talker_unique_id: cmd.talker_unique_id,
                        controller_entity_id: cmd.controller_entity_id,
                        // The caller knows the ADP table; the entity does
                        // not. Reported as not-yet-discovered, which sends
                        // the sink to PRB_W_AVAIL to wait for the talker's
                        // ENTITY_AVAILABLE — the safe order, since probing a
                        // talker we have not heard from cannot succeed.
                        talker_in_adp: false,
                        talker_available_index: None,
                        streaming_wait: cmd.flags & acmp_flags::STREAMING_WAIT != 0,
                    }
                } else {
                    acmp_listener::Event::UnbindRx
                };
                let (resp, actions) = sm.on_event(ev, now_ns, rand01_ns);
                r.status = match resp {
                    Some(acmp_listener::AcmpResponse::Success) => acmp_status::SUCCESS,
                    Some(acmp_listener::AcmpResponse::NotConnected) => acmp_status::NOT_CONNECTED,
                    Some(acmp_listener::AcmpResponse::ListenerUnknownId) => {
                        acmp_status::LISTENER_UNKNOWN_ID
                    }
                    // The SM emits no ACMP response only for events that are
                    // not bind/unbind, which this arm never passes it.
                    None => acmp_status::SUCCESS,
                };
                Some((r, actions))
            }
        }
    }

    /// START_STREAMING / STOP_STREAMING (Milan §5.4.2.19-20).
    ///
    /// Implemented for each Stream Input. A Stream Output gets NOT_SUPPORTED,
    /// which the spec names explicitly rather than leaving to the usual
    /// NO_SUCH_DESCRIPTOR: the descriptor DOES exist, so "you do not have
    /// that" would be the wrong refusal — the command is simply not
    /// applicable to a talker's own stream.
    fn streaming_cmd(&mut self, cmd: &AemPdu, start: bool) -> (u8, Vec<u8>) {
        let (Some(ty), Some(index)) = (be_u16(&cmd.payload, 0), be_u16(&cmd.payload, 2)) else {
            return (aecp_status::BAD_ARGUMENTS, cmd.payload.clone());
        };
        if ty == dt::STREAM_OUTPUT {
            if index >= self.model.stream_count() {
                return (aecp_status::NO_SUCH_DESCRIPTOR, cmd.payload.clone());
            }
            return (aecp_status::NOT_SUPPORTED, cmd.payload.clone());
        }
        if ty != dt::STREAM_INPUT {
            return (aecp_status::NO_SUCH_DESCRIPTOR, cmd.payload.clone());
        }
        let Some(sm) = self.sink_sms.get_mut(usize::from(index)) else {
            return (aecp_status::NO_SUCH_DESCRIPTOR, cmd.payload.clone());
        };
        // "This command has no effect on a Stream Input that is not already
        // bound or already started" — a no-op, explicitly NOT an error, so an
        // unbound or already-started sink still answers SUCCESS.
        if start {
            sm.start_streaming();
        } else {
            sm.stop_streaming();
        }
        (aecp_status::SUCCESS, cmd.payload.clone())
    }

    /// The sink state machines, one per STREAM_INPUT.
    pub fn sink_sms(&self) -> &[acmp_listener::ListenerSm] {
        &self.sink_sms
    }

    pub fn sink_sm_mut(&mut self, index: u16) -> Option<&mut acmp_listener::ListenerSm> {
        self.sink_sms.get_mut(usize::from(index))
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
        let encoded = match ty {
            dt::ENTITY if index == 0 => Some(desc::encode_entity(&EntityDesc {
                entity_id: m.entity_id,
                entity_model_id: m.entity_model_id,
                entity_capabilities: self.entity_capabilities(),
                talker_stream_sources: self.model.stream_count(),
                talker_capabilities: self.talker_capabilities(),
                listener_stream_sinks: self.model.sink_count(),
                listener_capabilities: self.listener_capabilities(),
                controller_capabilities: controller_caps::IMPLEMENTED,
                available_index: self.available_index,
                entity_name: self.name_of(dt::ENTITY, 0, ENTITY_NAME),
                firmware_version: m.firmware_version.clone(),
                serial_number: hex_mac(m.mac),
                configurations_count: 1,
                current_configuration: 0,
                ..Default::default()
            })),
            dt::CONFIGURATION if index == 0 => {
                Some(desc::encode_configuration(&ConfigurationDesc {
                    object_name: self.name_of(dt::CONFIGURATION, 0, CONFIGURATION_NAME),
                    descriptor_counts: self.descriptor_counts(),
                }))
            }
            dt::AUDIO_UNIT if index == 0 => Some(desc::encode_audio_unit(&AudioUnitDesc {
                object_name: self.name_of(dt::AUDIO_UNIT, 0, AUDIO_UNIT_NAME),
                clock_domain_index: 0,
                number_of_stream_input_ports: m.sink_count(),
                number_of_stream_output_ports: m.stream_count(),
                current_sampling_rate: m.current_sampling_rate,
                sampling_rates: m.sampling_rates.clone(),
            })),
            // One STREAM_INPUT per sink we accept.
            //
            // CLOCK_SYNC_SOURCE is set: an input stream genuinely CAN be our
            // media-clock reference, and in duplex the daemon already
            // recovers the sample clock from the 16R's arriving stream. This
            // is the opposite of the output side, where offering the flag
            // would invite the 16R to slave to a clock we do not discipline.
            dt::STREAM_INPUT if usize::from(index) < m.sinks.len() => {
                let sk = m.sinks.get(usize::from(index))?;
                Some(desc::encode_stream(
                    &StreamDesc {
                        object_name: self.name_of(dt::STREAM_INPUT, index, &sk.object_name),
                        clock_domain_index: 0,
                        stream_flags: stream_flags::CLASS_A | stream_flags::CLOCK_SYNC_SOURCE,
                        current_format: m.current_format,
                        formats: m.formats.clone(),
                        avb_interface_index: 0,
                        // Milan v1.3 §5.3.3.4 floor. An input buffer is a real
                        // commitment to absorb jitter, so this is the one
                        // descriptor field the spec sets a minimum on.
                        buffer_length: super::MILAN_MIN_STREAM_INPUT_BUFFER_NS,
                    },
                    true,
                ))
            }
            // One input port per sink, its clusters and map placed ABOVE the
            // output side's in the shared index space.
            dt::STREAM_PORT_INPUT if usize::from(index) < m.sinks.len() => {
                Some(desc::encode_stream_port(
                    &StreamPortDesc {
                        clock_domain_index: 0,
                        port_flags: 0,
                        number_of_clusters: m.channels,
                        base_cluster: m
                            .out_cluster_count()
                            .saturating_add(index.saturating_mul(m.channels)),
                        number_of_maps: 1,
                        base_map: m.stream_count().saturating_add(index),
                    },
                    true,
                ))
            }
            dt::STREAM_OUTPUT if usize::from(index) < m.streams.len() => {
                let st = m.streams.get(usize::from(index))?;
                Some(desc::encode_stream(
                    &StreamDesc {
                        object_name: self.name_of(dt::STREAM_OUTPUT, index, &st.object_name),
                        clock_domain_index: 0,
                        // CLASS_A always. CLOCK_SYNC_SOURCE offers this stream
                        // as a media-clock reference for the peer, which is
                        // true of the CRF stream and ONLY of the CRF stream:
                        // our audio streams run on the Mac's own clock, so
                        // offering one of those would invite the 16R to slave
                        // to a clock we do not discipline.
                        stream_flags: if st.is_media_clock() {
                            stream_flags::CLASS_A | stream_flags::CLOCK_SYNC_SOURCE
                        } else {
                            stream_flags::CLASS_A
                        },
                        current_format: st.format,
                        formats: if st.is_media_clock() {
                            vec![st.format]
                        } else {
                            m.formats.clone()
                        },
                        avb_interface_index: 0,
                        buffer_length: 2_000_000,
                    },
                    false,
                ))
            }
            // One port per AUDIO stream, each owning that stream's own block of
            // clusters and its own map — so cluster 9 belongs to stream 1 and
            // a controller can tell which eight channels are which. The
            // media-clock stream carries no audio channels, so it has no port.
            dt::STREAM_PORT_OUTPUT if index < m.audio_stream_count() => {
                Some(desc::encode_stream_port(
                    &StreamPortDesc {
                        clock_domain_index: 0,
                        port_flags: 0,
                        number_of_clusters: m.channels,
                        base_cluster: index.saturating_mul(m.channels),
                        number_of_maps: 1,
                        base_map: index,
                    },
                    false,
                ))
            }
            // One index space, output clusters first then input clusters, so
            // the name a controller reads tells it which direction it is in.
            dt::AUDIO_CLUSTER if index < m.cluster_count() => {
                let default = self.cluster_name(index)?;
                Some(desc::encode_audio_cluster(&AudioClusterDesc {
                    object_name: self.name_of(dt::AUDIO_CLUSTER, index, &default),
                    channel_count: 1,
                    // 0x40 = AAF sample format, matching the listener side.
                    format: 0x40,
                }))
            }
            // The map for stream `index`: its eight stream channels onto its
            // own eight clusters. `cluster_offset` is relative to the port's
            // base_cluster, so each map is identical in shape and it is the
            // port that places it in the 64-channel layout — which is also
            // why the SAME shape serves an input port sitting higher up.
            dt::AUDIO_MAP if index < m.map_count() => {
                // Maps below the AUDIO stream count belong to the output
                // ports; the rest to the input ports, renumbered from 0 so
                // `stream_index` is the index WITHIN that direction, as the
                // port expects.
                let si = if index < m.audio_stream_count() {
                    index
                } else {
                    index.saturating_sub(m.audio_stream_count())
                };
                Some(desc::encode_audio_map(&AudioMapDesc {
                    mappings: (0..m.channels)
                        .map(|k| AudioMapping {
                            stream_index: si,
                            stream_channel: k,
                            cluster_offset: k,
                            cluster_channel: 0,
                        })
                        .collect(),
                }))
            }
            // Milan requires a PAAD-AE to expose an IDENTIFY control, and the
            // 16R will not finish enumerating a peer without one: it asked us
            // for this descriptor repeatedly and we answered
            // NO_SUCH_DESCRIPTOR, which left its model of us incomplete, so it
            // never presented us as a routable device even though every stream
            // reservation succeeded. Values match the 16R's own IDENTIFY
            // control (see the `control_identify_*` tests): LINEAR_UINT8, one
            // value, 0 = off and 255 = on.
            dt::CONTROL if index == 0 => Some(desc::encode_control(&ControlDesc {
                object_name: self.name_of(dt::CONTROL, 0, IDENTIFY_CONTROL_NAME),
                localized_description: NO_STRING,
                block_latency: 0,
                control_latency: 0,
                control_domain: 0,
                control_value_type: control_value_type::LINEAR_UINT8,
                control_type: control_type::IDENTIFY,
                // 0 = the control does not self-clear. We have no LED to
                // blink, so there is nothing to time out.
                reset_time: 0,
                number_of_values: 1,
                // Not fed by a signal source; 0xffff is the 16R's own value
                // for this, not a guess at "unused".
                signal_type: NO_STRING,
                signal_index: 0,
                signal_output: 0,
                // minimum, maximum, step, default, current, unit, string.
                // `current` is the LIVE value, so a SET_CONTROL is visible in
                // the descriptor tree too and not only through GET_CONTROL.
                values: vec![
                    0x00,
                    0xff,
                    0xff,
                    0x00,
                    self.identify,
                    0x00,
                    0x00,
                    0xff,
                    0xff,
                ],
            })),
            dt::AVB_INTERFACE if index == 0 => {
                Some(desc::encode_avb_interface(&AvbInterfaceDesc {
                    object_name: self.name_of(dt::AVB_INTERFACE, 0, AVB_INTERFACE_NAME),
                    mac_address: m.mac,
                    // GPTP_GRANDMASTER_SUPPORTED is set only when the gPTP
                    // layer really can take the role. Milan v1.3 §4.3.1.1
                    // requires a Talker PAAD to be grandmaster-capable, but
                    // advertising it while the port cannot transmit
                    // Announce/Sync would be a false claim, so the daemon
                    // reports the live capability rather than a constant.
                    interface_flags: if_flags::GPTP_SUPPORTED
                        | if_flags::SRP_SUPPORTED
                        | if self.gptp.grandmaster_capable {
                            if_flags::GPTP_GRANDMASTER_SUPPORTED
                        } else {
                            0
                        },
                    clock_identity: m.entity_id,
                    // All five from the gPTP layer, so the descriptor and the
                    // wire cannot disagree about which clock we are.
                    priority1: self.gptp.priority1,
                    clock_class: self.gptp.clock_class,
                    offset_scaled_log_variance: self.gptp.offset_scaled_log_variance,
                    clock_accuracy: self.gptp.clock_accuracy,
                    priority2: self.gptp.priority2,
                    domain_number: self.gptp.domain_number,
                    log_sync_interval: -3,
                    log_announce_interval: 0,
                    log_pdelay_interval: 0,
                    port_number: 1,
                }))
            }
            // Two clock sources once we have sinks, and the comment that used
            // to sit here was left over from when this entity was
            // transmit-only: it said "the listener side offers a second
            // INPUT_STREAM source ... in this direction there is nothing to
            // slave to", but after the entity merge THIS IS the listener side,
            // and the daemon really does recover the media clock from the
            // 16R's arriving stream. Milan §5.3.3.6 wants that advertised.
            //
            // Index 0 INTERNAL: the Mac's own clock, which is what CoreAudio
            // produces our outbound stream on.
            dt::CLOCK_SOURCE if index == 0 => Some(desc::encode_clock_source(&ClockSourceDesc {
                object_name: self.name_of(dt::CLOCK_SOURCE, index, CLOCK_SOURCE_NAME),
                clock_source_type: clock_source::INTERNAL,
                ..Default::default()
            })),
            // Index 1 INPUT_STREAM, present only when a sink exists to
            // recover from. Located on STREAM_INPUT 0, which is the sink
            // whose arriving stream the media clock is actually taken from.
            dt::CLOCK_SOURCE if index == 1 && m.sink_count() > 0 => {
                Some(desc::encode_clock_source(&ClockSourceDesc {
                    object_name: self.name_of(
                        dt::CLOCK_SOURCE,
                        index,
                        CLOCK_SOURCE_INPUT_STREAM_NAME,
                    ),
                    clock_source_type: clock_source::INPUT_STREAM,
                    clock_source_location_type: dt::STREAM_INPUT,
                    clock_source_location_index: 0,
                    ..Default::default()
                }))
            }
            // gPTP as a media-clock source, Milan v1.3 §7.5.2: EXTERNAL and
            // located on TIMING 0. The source identifier is not imposed by
            // Milan; zero avoids inventing a vendor-specific identifier.
            dt::CLOCK_SOURCE if index == self.gptp_clock_source_index() => {
                Some(desc::encode_clock_source(&ClockSourceDesc {
                    object_name: self.name_of(dt::CLOCK_SOURCE, index, CLOCK_SOURCE_GPTP_NAME),
                    clock_source_type: clock_source::EXTERNAL,
                    clock_source_location_type: dt::TIMING,
                    clock_source_location_index: 0,
                    ..Default::default()
                }))
            }
            dt::CLOCK_DOMAIN if index == 0 => Some(desc::encode_clock_domain(&ClockDomainDesc {
                object_name: self.name_of(dt::CLOCK_DOMAIN, 0, CLOCK_DOMAIN_NAME),
                clock_source_index: 0,
                clock_sources: (0..self.clock_source_count()).collect(),
            })),
            dt::TIMING if index == 0 => Some(desc::encode_timing(&TimingDesc {
                object_name: self.name_of(dt::TIMING, 0, TIMING_NAME),
                algorithm: 0, // SINGLE, Milan v1.3 §7.5.3
                ptp_instances: vec![0],
            })),
            dt::PTP_INSTANCE if index == 0 => {
                Some(desc::encode_ptp_instance(&PtpInstanceDesc {
                    object_name: self.name_of(dt::PTP_INSTANCE, 0, PTP_INSTANCE_NAME),
                    clock_identity: m.entity_id,
                    flags: 1, // GRANDMASTER_CAPABLE, 16R hardware vector
                    number_of_controls: 0,
                    base_control: 0,
                    number_of_ptp_ports: 1,
                    base_ptp_port: 0,
                }))
            }
            dt::PTP_PORT if index == 0 => Some(desc::encode_ptp_port(&PtpPortDesc {
                object_name: self.name_of(dt::PTP_PORT, 0, PTP_PORT_NAME),
                port_number: 0,
                port_type: 0, // P2P link-layer, IEEE 1722.1-2021 §7.2.36
                flags: 0,
                avb_interface_index: 0,
                profile_identifier: MacAddr([0; 6]),
            })),
            _ => None,
        };
        encoded.and_then(|mut bytes| {
            bytes.get_mut(2..4)?.copy_from_slice(&index.to_be_bytes());
            Some(bytes)
        })
    }

    fn get_stream_format(&self, cmd: &AemPdu) -> (u8, Vec<u8>) {
        match (be_u16(&cmd.payload, 0), be_u16(&cmd.payload, 2)) {
            // ANY stream we describe, not just index 0: a controller asks
            // per stream before binding it, and refusing seven of eight stops
            // the walk for those streams.
            //
            // Each output's OWN format, not the model's shared
            // `current_format`: the media-clock output is CRF, and reporting
            // it as AAF would have a controller bind it as audio.
            (Some(dt::STREAM_OUTPUT), Some(i)) => {
                let Some(st) = self.model.streams.get(usize::from(i)) else {
                    return (aecp_status::NO_SUCH_DESCRIPTOR, cmd.payload.clone());
                };
                let mut p = cmd.payload.get(0..4).unwrap_or(&[0; 4]).to_vec();
                p.extend_from_slice(&st.format.0.to_be_bytes());
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

    /// GET_PTP_INSTANCE_GRANDMASTER_INFO (IEEE 1722.1-2021, command 0x0053).
    ///
    /// The 37-byte response layout is anchored by four Apple responses captured
    /// by the 16R across two different grandmasters. Fields after the quality
    /// dataset are the captured valid byte (1), reserved u32 (0), master port
    /// identity and steps_removed; no guessed bitfield is originated.
    fn get_ptp_instance_grandmaster_info(&self, cmd: &AemPdu) -> (u8, Vec<u8>) {
        let (Some(ty), Some(index)) = (be_u16(&cmd.payload, 0), be_u16(&cmd.payload, 2)) else {
            return (aecp_status::BAD_ARGUMENTS, cmd.payload.clone());
        };
        if ty != dt::PTP_INSTANCE || index != 0 {
            return (aecp_status::NO_SUCH_DESCRIPTOR, cmd.payload.clone());
        }
        let Some(gm) = self.gptp.grandmaster else {
            return (aecp_status::NOT_SUPPORTED, cmd.payload.clone());
        };
        let mut p = Vec::with_capacity(37);
        p.extend_from_slice(&ty.to_be_bytes());
        p.extend_from_slice(&index.to_be_bytes());
        p.extend_from_slice(&gm.identity.to_be_bytes());
        p.push(gm.priority1);
        p.push(gm.clock_class);
        p.push(gm.clock_accuracy);
        p.extend_from_slice(&gm.offset_scaled_log_variance.to_be_bytes());
        p.push(gm.priority2);
        p.push(gm.domain_number);
        p.push(gm.time_source);
        p.push(1); // present/valid in every captured Apple response
        p.extend_from_slice(&0u32.to_be_bytes()); // captured reserved field
        p.extend_from_slice(&gm.master_port_identity.to_be_bytes());
        p.extend_from_slice(&gm.master_port_number.to_be_bytes());
        p.extend_from_slice(&gm.steps_removed.to_be_bytes());
        debug_assert_eq!(p.len(), 37);
        (aecp_status::SUCCESS, p)
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

    /// Build one UNSOLICITED AEM response per registered controller,
    /// reporting the current answer to `command_type`.
    ///
    /// A controller that has sent REGISTER_UNSOLICITED_NOTIFICATION is asking
    /// to be told when our state changes, instead of polling us. We accepted
    /// those registrations and then never sent anything, which is worse than
    /// refusing them: the controller is waiting on a promise.
    ///
    /// The 16R does exactly this. It enumerates us fully, registers for
    /// unsolicited notifications, and then lists our streams as "(offline)"
    /// with no format information — while Apple's entity, which DOES push
    /// notifications after the same registration, is listed as selectable.
    ///
    /// The payload is produced by our OWN already-implemented handler for
    /// that command, via `aem_payload_for`, so a notification can never
    /// describe state we cannot also answer a direct query about. Commands we
    /// do not implement produce no notification rather than an invented one.
    ///
    /// `sequence_id` is 0: 1722.1 pairs a sequence id with a command, and an
    /// unsolicited response answers none, so echoing a borrowed id would
    /// invite a controller to match it against a request it never sent.
    pub fn unsolicited(&self, command_type: u16) -> Vec<(MacAddr, AemPdu)> {
        self.unsolicited_desc(command_type, dt::STREAM_OUTPUT, 0)
    }

    /// As `unsolicited`, for a specific descriptor.
    ///
    /// The 16R's reference peer pushes a GET_COUNTERS notification for EVERY
    /// one of its STREAM_OUTPUTs, not just index 0 — captured off the wire:
    /// Apple's entity sent 128 unsolicited GET_COUNTERS over 40 s, 16 each
    /// for STREAM_OUTPUT 0..7, `counters_valid = 0x000000ff`. So the index
    /// has to be a parameter, not a constant.
    pub fn unsolicited_desc(
        &self,
        command_type: u16,
        descriptor_type: u16,
        descriptor_index: u16,
    ) -> Vec<(MacAddr, AemPdu)> {
        let Some((status, payload)) =
            self.aem_payload_for(command_type, descriptor_type, descriptor_index)
        else {
            return Vec::new();
        };
        // Never notify a failure. An unsolicited response is an unprompted
        // claim about our state; sending NO_SUCH_DESCRIPTOR unbidden tells a
        // controller nothing it asked for and nothing it can use.
        if status != aecp_status::SUCCESS {
            return Vec::new();
        }
        self.control
            .unsol_controllers()
            .iter()
            .map(|c| {
                (
                    c.mac,
                    AemPdu {
                        message_type: aecp_msg::AEM_RESPONSE,
                        status,
                        target_entity_id: self.model.entity_id,
                        controller_entity_id: c.entity_id,
                        sequence_id: 0,
                        unsolicited: true,
                        command_type,
                        payload: payload.clone(),
                    },
                )
            })
            .collect()
    }

    /// The current payload our own handler would return for `command_type`
    /// against one descriptor. `None` for anything else, so the notification
    /// path cannot originate a payload shape no direct query would produce.
    fn aem_payload_for(
        &self,
        command_type: u16,
        descriptor_type: u16,
        descriptor_index: u16,
    ) -> Option<(u8, Vec<u8>)> {
        // A synthetic command carrying the descriptor fields the handlers
        // read from offsets 0 and 2. Built here rather than threaded in,
        // because an unsolicited notification has no originating command.
        let mut payload = Vec::with_capacity(4);
        payload.extend_from_slice(&descriptor_type.to_be_bytes());
        payload.extend_from_slice(&descriptor_index.to_be_bytes());
        let probe = AemPdu {
            message_type: aecp_msg::AEM_COMMAND,
            status: aecp_status::SUCCESS,
            target_entity_id: self.model.entity_id,
            controller_entity_id: self.model.entity_id,
            sequence_id: 0,
            unsolicited: false,
            command_type,
            payload,
        };
        match command_type {
            aem_cmd::GET_STREAM_INFO => Some(self.get_stream_info(&probe)),
            aem_cmd::GET_STREAM_FORMAT => Some(self.get_stream_format(&probe)),
            aem_cmd::GET_PTP_INSTANCE_GRANDMASTER_INFO => {
                Some(self.get_ptp_instance_grandmaster_info(&probe))
            }
            // Counters change continuously, so a periodic counter
            // notification is a state change being reported, not a
            // fabricated heartbeat. Reuses the same block builders the
            // GET_COUNTERS handler uses, so the two cannot diverge.
            aem_cmd::GET_COUNTERS => {
                let block = match descriptor_type {
                    dt::AVB_INTERFACE if descriptor_index == 0 => {
                        Some(self.counters.avb_interface_block())
                    }
                    dt::CLOCK_DOMAIN if descriptor_index == 0 => {
                        Some(self.counters.clock_domain_block())
                    }
                    dt::STREAM_INPUT if descriptor_index < self.model.sink_count() => {
                        self.counters.stream_input_block(descriptor_index)
                    }
                    dt::STREAM_OUTPUT if descriptor_index < self.model.stream_count() => {
                        self.counters.stream_output_block(descriptor_index)
                    }
                    _ => None,
                };
                block.map(|b| {
                    (
                        aecp_status::SUCCESS,
                        b.encode_payload(descriptor_type, descriptor_index),
                    )
                })
            }
            _ => None,
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
    use crate::constants::acmp_msg;

    const OUR_MAC: MacAddr = MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]);
    /// The Milan base AAF format the talker transmits: INT_32BIT, 48 kHz,
    /// 8 channels, 6 spf.
    const FMT: StreamFormat = StreamFormat(0x0205_0220_0200_6000);

    /// The eight streams the 16R is wide, as the daemon builds them.
    fn stream_ids(n: u16) -> Vec<StreamId> {
        (0..n).map(|i| StreamId::from_parts(OUR_MAC, i)).collect()
    }

    fn entity() -> TalkerEntity {
        let m = talker_model(OUR_MAC, &stream_ids(1), FMT, "phase1b-test", 0)
            .expect("decodable format");
        TalkerEntity::new(m)
    }

    /// A duplex entity: two STREAM_OUTPUTs and two STREAM_INPUTs, so the
    /// shared cluster/map index space has a non-trivial split to verify.
    fn duplex_entity() -> TalkerEntity {
        let m = talker_model(OUR_MAC, &stream_ids(2), FMT, "phase1b-test", 2)
            .expect("decodable format");
        TalkerEntity::new(m)
    }

    /// A media-clock entity: the eight audio streams plus a CRF output.
    fn crf_entity() -> TalkerEntity {
        let m = talker_model(OUR_MAC, &stream_ids(8), FMT, "phase1b-test", 1)
            .expect("decodable format")
            .with_media_clock_stream(StreamId::from_parts(OUR_MAC, 8));
        TalkerEntity::new(m)
    }

    /// MEDIA_CLOCK_SOURCE must appear in the ADP exactly when we really
    /// describe a CRF STREAM_OUTPUT, and never otherwise.
    ///
    /// The expected word is the 16R's own, captured off the wire while it was
    /// advertising its `Send CRF` output: `talker_caps = 0x4801`. Our entity
    /// transmitting CRF has the same role, so it must produce the same word.
    /// Asserting the bit against our own named constant alone would pass with
    /// any value, which is the tautology class this project has been bitten
    /// by before — so the literal 0x4801 from the device is the anchor.
    #[test]
    fn media_clock_source_is_claimed_only_with_a_crf_stream() {
        // Audio only: the bit must be absent.
        let audio_only = entity();
        assert_eq!(audio_only.model().media_clock_stream_count(), 0);
        assert_eq!(
            audio_only.talker_capabilities() & talker_caps::MEDIA_CLOCK_SOURCE,
            0,
            "an entity with no CRF stream must not claim to be a media clock source"
        );
        assert_eq!(audio_only.talker_capabilities(), 0x4001);

        // With a CRF output: the bit must be present, and the whole word must
        // equal the value the 16R itself advertises.
        let crf = crf_entity();
        assert_eq!(crf.model().media_clock_stream_count(), 1);
        assert_ne!(
            crf.talker_capabilities() & talker_caps::MEDIA_CLOCK_SOURCE,
            0,
            "an entity transmitting CRF must claim MEDIA_CLOCK_SOURCE"
        );
        assert_eq!(
            crf.talker_capabilities(),
            0x4801,
            "must match the 16R's own captured talker_capabilities"
        );

        // And the ADP on the wire must carry it, not just the accessor.
        let mut e = crf_entity();
        let adp = e.advertise();
        assert_eq!(adp.talker_capabilities, 0x4801);
    }

    /// The unsolicited registry must actually EMIT. It existed since Phase 1a
    /// accepting registrations and sending nothing, which is worse than
    /// refusing them: a controller that registered is waiting to be told.
    #[test]
    fn unsolicited_notifications_are_pushed_to_registered_controllers() {
        let mut e = crf_entity();
        let me = e.model().entity_id;
        // The `cmd` helper's third argument is the TARGET; it already sets
        // controller_entity_id to this id, which is the registering party.
        let ctrl = Eui64([9, 9, 9, 9, 9, 9, 9, 9]);
        let ctrl_mac = MacAddr([0x00, 0x0a, 0x92, 0x03, 0x3b, 0xa9]);

        // Nobody registered yet: emitting must produce NOTHING. Without this
        // half the test would pass on an implementation that broadcasts.
        assert!(
            e.unsolicited(aem_cmd::GET_STREAM_INFO).is_empty(),
            "must not notify a controller that never registered"
        );

        let reg = cmd(aem_cmd::REGISTER_UNSOLICITED_NOTIFICATION, vec![], me);
        let r = e.handle_aecp(&reg, ctrl_mac, 0);
        assert_eq!(r.status, aecp_status::SUCCESS);

        // Registered, but with no counters fed yet there is no block to
        // report. A failure must NOT be notified: an unsolicited response is
        // an unprompted claim about our state, and pushing
        // NO_SUCH_DESCRIPTOR unbidden tells a controller nothing it asked
        // for and nothing it can use.
        assert!(
            e.unsolicited_desc(aem_cmd::GET_COUNTERS, dt::STREAM_OUTPUT, 0)
                .is_empty(),
            "must not notify a descriptor we have no state for"
        );

        // Feed counters the way the daemon does.
        let n = usize::from(e.model().stream_count());
        e.set_counters(crate::counters::EntityCounters {
            outputs: (0..n)
                .map(|_| crate::counters::StreamOutputCounters {
                    frames_tx: Some(8000),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        });

        let out = e.unsolicited_desc(aem_cmd::GET_COUNTERS, dt::STREAM_OUTPUT, 0);
        assert_eq!(out.len(), 1, "exactly the one registered controller");
        let (dst, pdu) = &out[0];
        assert_eq!(*dst, ctrl_mac, "must go to the MAC it registered from");
        assert!(pdu.unsolicited, "the u bit is what marks it unsolicited");
        assert_eq!(pdu.message_type, aecp_msg::AEM_RESPONSE);
        assert_eq!(pdu.command_type, aem_cmd::GET_COUNTERS);
        assert_eq!(pdu.status, aecp_status::SUCCESS);
        assert_eq!(pdu.controller_entity_id, ctrl);
        assert_eq!(pdu.target_entity_id, me);
        // Sequence ids pair with commands; this answers none, so echoing a
        // borrowed id would invite the controller to match a request it never
        // sent.
        assert_eq!(pdu.sequence_id, 0);
        // The payload must be what a DIRECT query returns, not a shape
        // invented for the notification path.
        let direct = e.handle_aecp(
            &cmd(aem_cmd::GET_COUNTERS, vec![0, 6, 0, 0], me),
            ctrl_mac,
            0,
        );
        assert_eq!(direct.status, aecp_status::SUCCESS);
        assert_eq!(pdu.payload, direct.payload);

        // Every STREAM_OUTPUT must be notifiable, not just index 0 — the
        // reference peer pushes one per stream.
        for idx in 0..e.model().stream_count() {
            assert_eq!(
                e.unsolicited_desc(aem_cmd::GET_COUNTERS, dt::STREAM_OUTPUT, idx)
                    .len(),
                1,
                "STREAM_OUTPUT {idx} must produce a notification"
            );
        }
        // And an index past the end must produce none rather than an
        // invented block.
        assert!(
            e.unsolicited_desc(
                aem_cmd::GET_COUNTERS,
                dt::STREAM_OUTPUT,
                e.model().stream_count()
            )
            .is_empty(),
            "a descriptor we do not have must not be notified"
        );

        // A command we do not implement must produce NO notification.
        assert!(
            e.unsolicited(aem_cmd::GET_NAME).is_empty(),
            "only commands with a real handler may be notified"
        );

        // Deregistering stops the pushes.
        let dereg = cmd(aem_cmd::DEREGISTER_UNSOLICITED_NOTIFICATION, vec![], me);
        e.handle_aecp(&dereg, ctrl_mac, 0);
        assert!(
            e.unsolicited_desc(aem_cmd::GET_COUNTERS, dt::STREAM_OUTPUT, 0)
                .is_empty(),
            "a deregistered controller must stop receiving notifications"
        );
    }

    /// The CRF output is a STREAM_OUTPUT but carries no audio, so it must own
    /// no AUDIO_CLUSTER, no AUDIO_MAP and no STREAM_PORT_OUTPUT. Counting it
    /// as an audio stream would advertise eight phantom clusters and shift
    /// every input cluster's index.
    #[test]
    fn the_crf_output_owns_no_clusters_port_or_map() {
        let e = crf_entity();
        let m = e.model();
        assert_eq!(m.stream_count(), 9, "eight audio plus one media clock");
        assert_eq!(m.audio_stream_count(), 8);
        assert_eq!(m.media_clock_stream_count(), 1);

        // Clusters and maps are sized by the AUDIO streams only.
        assert_eq!(m.out_cluster_count(), 64, "8 audio streams x 8 channels");
        assert_eq!(m.in_cluster_count(), 8, "1 sink x 8 channels");
        assert_eq!(m.cluster_count(), 72);
        assert_eq!(m.map_count(), 8 + 1);

        let counts = e.descriptor_counts();
        let get = |t: u16| counts.iter().find(|(k, _)| *k == t).map(|(_, n)| *n);
        assert_eq!(get(dt::STREAM_OUTPUT), Some(9));
        assert_eq!(
            get(dt::STREAM_PORT_OUTPUT),
            Some(8),
            "one port per AUDIO stream, not per stream"
        );
        assert_eq!(get(dt::AUDIO_CLUSTER), Some(72));
        assert_eq!(get(dt::AUDIO_MAP), Some(9));

        // And the port for the CRF index must not be answerable.
        assert!(
            e.encode_descriptor(dt::STREAM_PORT_OUTPUT, 8).is_none(),
            "a port at the media-clock index would claim it has channels"
        );
        assert!(
            e.encode_descriptor(dt::STREAM_OUTPUT, 8).is_some(),
            "the CRF STREAM_OUTPUT itself must be answerable"
        );
    }

    /// Every descriptor the counts advertise must be answerable — the module
    /// invariant — re-checked with a media-clock stream present, since that is
    /// what changes the per-type counts.
    #[test]
    fn every_advertised_descriptor_is_answerable_with_a_crf_stream() {
        let e = crf_entity();
        for (ty, n) in e.descriptor_counts() {
            for i in 0..n {
                assert!(
                    e.encode_descriptor(ty, i).is_some(),
                    "advertised {} index {i} is unanswerable",
                    dt::name(ty).unwrap_or("?")
                );
            }
        }
    }

    /// The CRF output must describe itself as CRF, through READ_DESCRIPTOR and
    /// through GET_STREAM_FORMAT alike. Reporting the model's shared AAF
    /// format would have a controller bind a media clock as audio.
    #[test]
    fn the_crf_output_describes_itself_as_crf_not_aaf() {
        let e = crf_entity();
        let crf = StreamFormat(avb_avtp::crf::milan::FORMAT);

        let bytes = e.encode_descriptor(dt::STREAM_OUTPUT, 8).unwrap();
        let d = dec::decode_stream(&bytes).expect("stream descriptor decodes");
        assert_eq!(d.current_format, crf, "READ_DESCRIPTOR");
        assert_eq!(d.formats, vec![crf], "only CRF is offered on this output");
        assert_eq!(d.object_name, MEDIA_CLOCK_STREAM_NAME);

        // An audio output is unaffected.
        let bytes = e.encode_descriptor(dt::STREAM_OUTPUT, 0).unwrap();
        let d = dec::decode_stream(&bytes).expect("stream descriptor decodes");
        assert_eq!(d.current_format, FMT, "audio output stays AAF");
    }

    /// CLOCK_SYNC_SOURCE offers a stream as a media-clock reference. True of
    /// the CRF stream and ONLY of it: our audio runs on the Mac's own clock,
    /// so offering one of those invites the 16R to slave to a clock we do not
    /// discipline.
    #[test]
    fn only_the_crf_output_offers_itself_as_a_clock_sync_source() {
        let e = crf_entity();
        for i in 0..e.model().stream_count() {
            let bytes = e.encode_descriptor(dt::STREAM_OUTPUT, i).unwrap();
            let d = dec::decode_stream(&bytes).unwrap();
            assert_ne!(
                d.stream_flags & stream_flags::CLASS_A,
                0,
                "stream {i} must be class A"
            );
            let sync = d.stream_flags & stream_flags::CLOCK_SYNC_SOURCE != 0;
            assert_eq!(
                sync,
                i == 8,
                "stream {i}: CLOCK_SYNC_SOURCE must be set iff it is the CRF stream"
            );
        }
    }

    /// Gaining a CRF output changes the AEM tree, so it must change the model
    /// id — otherwise a controller holding the cached audio-only tree keeps
    /// describing output 8 as AAF forever.
    #[test]
    fn adding_the_crf_output_changes_the_model_id() {
        let audio_only = talker_model(OUR_MAC, &stream_ids(8), FMT, "phase1b-test", 1).unwrap();
        let with_crf = audio_only
            .clone()
            .with_media_clock_stream(StreamId::from_parts(OUR_MAC, 8));
        assert_ne!(
            audio_only.entity_model_id, with_crf.entity_model_id,
            "the model id must distinguish the two trees"
        );
    }

    /// Audio streams must keep the low indices: the output ports'
    /// `base_cluster = index * channels` and the output maps' `stream_index`
    /// both assume it.
    #[test]
    fn audio_streams_come_first_in_the_stream_output_index_space() {
        let e = crf_entity();
        assert!(e.model().audio_streams_come_first());
        assert_eq!(
            e.model().media_clock_stream(),
            Some((8, StreamId::from_parts(OUR_MAC, 8)))
        );
        for i in 0..8usize {
            assert!(!e.model().streams[i].is_media_clock(), "{i}");
        }
        assert!(e.model().streams[8].is_media_clock());
    }

    /// Every cluster is claimed exactly once across all ports, and the union
    /// is contiguous from 0 — the check that catches an index-space collision
    /// rather than spot-checking one index.
    #[test]
    fn clusters_are_claimed_exactly_once_with_a_crf_stream() {
        let e = crf_entity();
        let m = e.model();
        let mut seen = vec![0u32; usize::from(m.cluster_count())];
        for (ty, n) in [
            (dt::STREAM_PORT_OUTPUT, m.audio_stream_count()),
            (dt::STREAM_PORT_INPUT, m.sink_count()),
        ] {
            for i in 0..n {
                let bytes = e.encode_descriptor(ty, i).unwrap();
                let p = dec::decode_stream_port(&bytes).expect("port decodes");
                for k in 0..p.number_of_clusters {
                    let c = usize::from(p.base_cluster + k);
                    assert!(
                        c < seen.len(),
                        "cluster {c} is outside the advertised space"
                    );
                    seen[c] += 1;
                }
            }
        }
        assert!(
            seen.iter().all(|&n| n == 1),
            "every cluster must be claimed exactly once: {seen:?}"
        );
    }

    /// A duplex entity advertises sinks; a transmit-only one must not.
    #[test]
    fn duplex_entity_advertises_listener_sinks_and_transmit_only_does_not() {
        let mut tx_only = entity();
        let adp = tx_only.advertise();
        assert_eq!(adp.listener_stream_sinks, 0);
        assert_eq!(
            adp.listener_capabilities, 0,
            "claiming the listener role with no sinks offers a connection \
             that must then fail"
        );

        let mut dx = duplex_entity();
        let adp = dx.advertise();
        assert_eq!(adp.listener_stream_sinks, 2);
        assert_ne!(adp.listener_capabilities & listener_caps::IMPLEMENTED, 0);
        assert_ne!(adp.listener_capabilities & listener_caps::AUDIO_SINK, 0);
        // The talker side is unchanged by gaining sinks.
        assert_eq!(adp.talker_stream_sources, 2);
        assert_ne!(adp.talker_capabilities & talker_caps::AUDIO_SOURCE, 0);
    }

    /// The ENTITY descriptor and the ADP must agree about the sink count.
    /// They are built in two places, and a controller reads both.
    #[test]
    fn entity_descriptor_and_adp_agree_on_sinks() {
        let mut dx = duplex_entity();
        let adp = dx.advertise();
        let bytes = dx.encode_descriptor(dt::ENTITY, 0).unwrap();
        let d = dec::decode_entity(&bytes).expect("decodable");
        assert_eq!(d.listener_stream_sinks, adp.listener_stream_sinks);
        assert_eq!(d.listener_capabilities, adp.listener_capabilities);
        assert_eq!(d.talker_stream_sources, adp.talker_stream_sources);
    }

    /// STREAM_INPUT descriptors must exist, carry the Milan buffer floor, and
    /// offer themselves as a media-clock source -- which the OUTPUT side must
    /// not do.
    #[test]
    fn stream_input_descriptors_carry_the_milan_floor_and_clock_sync_source() {
        let dx = duplex_entity();
        for i in 0..2u16 {
            let bytes = dx.encode_descriptor(dt::STREAM_INPUT, i).unwrap();
            let d = dec::decode_stream(&bytes).expect("decodable");
            assert!(
                d.buffer_length >= super::super::MILAN_MIN_STREAM_INPUT_BUFFER_NS,
                "Milan v1.3 §5.3.3.4 floor"
            );
            assert_ne!(d.stream_flags & stream_flags::CLASS_A, 0);
            assert_ne!(
                d.stream_flags & stream_flags::CLOCK_SYNC_SOURCE,
                0,
                "an arriving stream can be our media-clock reference"
            );
        }
        // Past the end there is no sink.
        assert!(dx.encode_descriptor(dt::STREAM_INPUT, 2).is_none());

        // The output side must NOT offer itself as a clock source.
        let out = dx.encode_descriptor(dt::STREAM_OUTPUT, 0).unwrap();
        let od = dec::decode_stream(&out).expect("decodable");
        assert_eq!(
            od.stream_flags & stream_flags::CLOCK_SYNC_SOURCE,
            0,
            "offering this would invite the 16R to slave to a clock we do \
             not discipline"
        );
    }

    /// The one that actually bites: AUDIO_CLUSTER and AUDIO_MAP share ONE
    /// index space per configuration, so the input port must point PAST the
    /// output clusters. If both started at 0, cluster 0 would be our first
    /// output channel and our first input channel at once, and a controller
    /// walking the tree would silently get the output one.
    #[test]
    fn input_and_output_ports_do_not_overlap_in_the_shared_index_space() {
        let dx = duplex_entity();
        let m = dx.model();
        assert_eq!(m.out_cluster_count(), 16, "2 streams x 8 ch");
        assert_eq!(m.in_cluster_count(), 16);
        assert_eq!(m.cluster_count(), 32, "both directions in one space");
        assert_eq!(m.map_count(), 4);

        let mut seen_clusters: Vec<u16> = Vec::new();
        let mut seen_maps: Vec<u16> = Vec::new();
        for (ty, n, is_in) in [
            (dt::STREAM_PORT_OUTPUT, 2u16, false),
            (dt::STREAM_PORT_INPUT, 2u16, true),
        ] {
            for i in 0..n {
                let bytes = dx.encode_descriptor(ty, i).unwrap();
                let p = dec::decode_stream_port(&bytes).expect("decodable");
                assert_eq!(p.number_of_clusters, 8);
                assert_eq!(p.number_of_maps, 1);
                if is_in {
                    assert!(
                        p.base_cluster >= m.out_cluster_count(),
                        "input port {i} base_cluster {} overlaps the output side",
                        p.base_cluster
                    );
                    assert!(p.base_map >= m.stream_count());
                }
                for k in 0..p.number_of_clusters {
                    seen_clusters.push(p.base_cluster + k);
                }
                seen_maps.push(p.base_map);
            }
        }
        // Every cluster claimed exactly once, covering the whole space.
        seen_clusters.sort_unstable();
        assert_eq!(seen_clusters, (0..32).collect::<Vec<u16>>());
        seen_maps.sort_unstable();
        assert_eq!(seen_maps, vec![0, 1, 2, 3]);

        // And every cluster in the space answers, with the direction
        // readable from its name.
        for i in 0..32u16 {
            let bytes = dx.encode_descriptor(dt::AUDIO_CLUSTER, i).unwrap();
            let c = dec::decode_audio_cluster(&bytes).expect("decodable");
            if i < 16 {
                assert!(c.object_name.starts_with("Mac "), "{}", c.object_name);
            } else {
                assert!(c.object_name.starts_with("16R "), "{}", c.object_name);
            }
        }
        assert!(dx.encode_descriptor(dt::AUDIO_CLUSTER, 32).is_none());
    }

    /// An input map's `stream_index` is relative to its own direction, since
    /// the port places it in the shared space via base_map/base_cluster.
    #[test]
    fn input_maps_are_numbered_within_their_own_direction() {
        let dx = duplex_entity();
        for (idx, want_si) in [(0u16, 0u16), (1, 1), (2, 0), (3, 1)] {
            let bytes = dx.encode_descriptor(dt::AUDIO_MAP, idx).unwrap();
            let mp = dec::decode_audio_map(&bytes).expect("decodable");
            assert_eq!(mp.mappings.len(), 8);
            for (k, mapping) in mp.mappings.iter().enumerate() {
                assert_eq!(mapping.stream_index, want_si, "map {idx}");
                assert_eq!(mapping.stream_channel, k as u16);
                assert_eq!(mapping.cluster_offset, k as u16);
            }
        }
        assert!(dx.encode_descriptor(dt::AUDIO_MAP, 4).is_none());
    }

    /// The CONFIGURATION descriptor's counts must match what actually
    /// answers, in both directions. A controller sizes its walk from these.
    #[test]
    fn configuration_counts_match_the_descriptors_that_answer() {
        for mut e in [entity(), duplex_entity(), wide_entity()] {
            let counts = e.descriptor_counts();
            for (ty, n) in counts {
                assert!(n > 0, "a declared type must not be empty: {ty:#06x}");
                for i in 0..n {
                    assert!(
                        e.encode_descriptor(ty, i).is_some(),
                        "declared {ty:#06x}[{i}] of {n} does not answer"
                    );
                }
                assert!(
                    e.encode_descriptor(ty, n).is_none(),
                    "{ty:#06x} answers past its declared count {n}"
                );
            }
            // A transmit-only entity must not declare the input side at all.
            let declared: Vec<u16> = e.descriptor_counts().iter().map(|(t, _)| *t).collect();
            let has_sinks = e.model().sink_count() > 0;
            assert_eq!(declared.contains(&dt::STREAM_INPUT), has_sinks);
            assert_eq!(declared.contains(&dt::STREAM_PORT_INPUT), has_sinks);
            let _ = e.advertise();
        }
    }

    /// The AUDIO_UNIT's port counts are how a controller knows the unit has
    /// an input side at all.
    #[test]
    fn audio_unit_reports_both_port_directions() {
        let dx = duplex_entity();
        let bytes = dx.encode_descriptor(dt::AUDIO_UNIT, 0).unwrap();
        let u = dec::decode_audio_unit(&bytes).expect("decodable");
        assert_eq!(u.number_of_stream_input_ports, 2);
        assert_eq!(u.number_of_stream_output_ports, 2);

        let tx = entity();
        let bytes = tx.encode_descriptor(dt::AUDIO_UNIT, 0).unwrap();
        let u = dec::decode_audio_unit(&bytes).expect("decodable");
        assert_eq!(u.number_of_stream_input_ports, 0);
    }

    /// GET_COUNTERS over the real AECP path, per descriptor type.
    ///
    /// The old behaviour answered SUCCESS with an all-zero block and
    /// `counters_valid = 0` for ANY type, including ones this entity does not
    /// have — so a controller could not tell a missing descriptor from one
    /// with nothing to report, and the mandatory Milan counters read as
    /// unknown.
    #[test]
    fn get_counters_answers_per_descriptor_with_real_values() {
        let mut e = duplex_entity();
        let id = e.entity_id();
        e.set_counters(crate::counters::EntityCounters {
            link_up: Some(3),
            link_down: Some(2),
            gptp_gm_changed: Some(5),
            frames_rx: Some(99),
            frames_tx: None,
            rx_crc_error: None,
            clock_locked: Some(1),
            clock_unlocked: Some(0),
            inputs: vec![
                crate::counters::StreamInputCounters {
                    frames_rx: Some(4242),
                    ..Default::default()
                },
                crate::counters::StreamInputCounters {
                    frames_rx: Some(0),
                    ..Default::default()
                },
            ],
            outputs: vec![
                crate::counters::StreamOutputCounters {
                    frames_tx: Some(777),
                    stream_start: Some(1),
                    ..Default::default()
                },
                crate::counters::StreamOutputCounters::default(),
            ],
        });

        let ask = |e: &mut TalkerEntity, ty: u16, idx: u16| {
            let mut p = ty.to_be_bytes().to_vec();
            p.extend_from_slice(&idx.to_be_bytes());
            e.handle_aecp(&cmd(aem_cmd::GET_COUNTERS, p, id), OUR_MAC, 0)
        };
        let read = |r: &AemPdu, bit: u32| -> (u32, u32) {
            let valid =
                u32::from_be_bytes([r.payload[4], r.payload[5], r.payload[6], r.payload[7]]);
            let slot = crate::counters::slot_index(bit).unwrap();
            let a = 8 + slot * 4;
            let v = u32::from_be_bytes([
                r.payload[a],
                r.payload[a + 1],
                r.payload[a + 2],
                r.payload[a + 3],
            ]);
            (valid, v)
        };

        let r = ask(&mut e, dt::AVB_INTERFACE, 0);
        assert_eq!(r.status, aecp_status::SUCCESS);
        assert_eq!(r.payload.len(), 4 + 4 + 128);
        let (valid, v) = read(&r, crate::counters::avb_interface::LINK_UP);
        assert_eq!(v, 3);
        assert_ne!(valid & crate::counters::avb_interface::GPTP_GM_CHANGED, 0);
        assert_eq!(read(&r, crate::counters::avb_interface::FRAMES_RX).1, 99);

        let r = ask(&mut e, dt::CLOCK_DOMAIN, 0);
        assert_eq!(r.status, aecp_status::SUCCESS);
        assert_eq!(read(&r, crate::counters::clock_domain::LOCKED).1, 1);

        // Per-index, and the right index.
        let r = ask(&mut e, dt::STREAM_INPUT, 0);
        assert_eq!(r.status, aecp_status::SUCCESS);
        assert_eq!(read(&r, crate::counters::stream_input::FRAMES_RX).1, 4242);
        let r = ask(&mut e, dt::STREAM_INPUT, 1);
        assert_eq!(read(&r, crate::counters::stream_input::FRAMES_RX).1, 0);

        let r = ask(&mut e, dt::STREAM_OUTPUT, 0);
        assert_eq!(read(&r, crate::counters::stream_output::FRAMES_TX).1, 777);

        // A descriptor we do not have must NOT be an empty success.
        let r = ask(&mut e, dt::STREAM_INPUT, 9);
        assert_eq!(r.status, aecp_status::NO_SUCH_DESCRIPTOR);
        let r = ask(&mut e, dt::AVB_INTERFACE, 1);
        assert_eq!(r.status, aecp_status::NO_SUCH_DESCRIPTOR);
        let r = ask(&mut e, dt::AUDIO_UNIT, 0);
        assert_eq!(r.status, aecp_status::NO_SUCH_DESCRIPTOR);
        // ENTITY GET_COUNTERS is not mandated by Milan and not implemented.
        let r = ask(&mut e, dt::ENTITY, 0);
        assert_eq!(r.status, aecp_status::NOT_IMPLEMENTED);
    }

    /// A transmit-only entity has no STREAM_INPUT, so its counters must be a
    /// missing descriptor rather than a zeroed block.
    #[test]
    fn get_counters_has_no_stream_input_on_a_transmit_only_entity() {
        let mut e = entity();
        let id = e.entity_id();
        let mut p = dt::STREAM_INPUT.to_be_bytes().to_vec();
        p.extend_from_slice(&0u16.to_be_bytes());
        let r = e.handle_aecp(&cmd(aem_cmd::GET_COUNTERS, p, id), OUR_MAC, 0);
        assert_eq!(r.status, aecp_status::NO_SUCH_DESCRIPTOR);
    }

    /// GET_NAME must answer for EVERY name in EVERY descriptor we implement
    /// (Milan v1.3 §5.4.2.12), and must agree with READ_DESCRIPTOR.
    #[test]
    fn get_name_answers_for_every_named_descriptor_we_implement() {
        let mut e = duplex_entity();
        let id = e.entity_id();
        let ask = |e: &mut TalkerEntity, ty: u16, idx: u16, ni: u16| {
            let mut p = ty.to_be_bytes().to_vec();
            p.extend_from_slice(&idx.to_be_bytes());
            p.extend_from_slice(&ni.to_be_bytes());
            p.extend_from_slice(&0u16.to_be_bytes());
            e.handle_aecp(&cmd(aem_cmd::GET_NAME, p, id), OUR_MAC, 0)
        };
        let named = [
            (dt::ENTITY, 1u16),
            (dt::CONFIGURATION, 1),
            (dt::AUDIO_UNIT, 1),
            (dt::AVB_INTERFACE, 1),
            (dt::CLOCK_SOURCE, 1),
            (dt::CLOCK_DOMAIN, 1),
            (dt::CONTROL, 1),
            (dt::STREAM_OUTPUT, 2),
            (dt::STREAM_INPUT, 2),
            (dt::AUDIO_CLUSTER, 32),
        ];
        for (ty, count) in named {
            for i in 0..count {
                let r = ask(&mut e, ty, i, 0);
                assert_eq!(
                    r.status,
                    aecp_status::SUCCESS,
                    "GET_NAME {ty:#06x}[{i}] must answer"
                );
                assert_eq!(r.payload.len(), 8 + 64);
                let got = desc::name_from_bytes(&r.payload[8..]);
                assert!(!got.is_empty(), "{ty:#06x}[{i}] name is empty");
            }
        }
        // ENTITY has a second name; group_name is empty by design.
        let r = ask(&mut e, dt::ENTITY, 0, 1);
        assert_eq!(r.status, aecp_status::SUCCESS);
        assert_eq!(desc::name_from_bytes(&r.payload[8..]), "");
        // ...and no third.
        assert_eq!(
            ask(&mut e, dt::ENTITY, 0, 2).status,
            aecp_status::BAD_ARGUMENTS
        );
        // A descriptor with no name field at all.
        assert_eq!(
            ask(&mut e, dt::AUDIO_MAP, 0, 0).status,
            aecp_status::NO_SUCH_DESCRIPTOR
        );
        // A descriptor we do not have.
        assert_eq!(
            ask(&mut e, dt::STREAM_INPUT, 9, 0).status,
            aecp_status::NO_SUCH_DESCRIPTOR
        );
        // Only configuration 0 exists.
        let mut p = dt::ENTITY.to_be_bytes().to_vec();
        p.extend_from_slice(&0u16.to_be_bytes());
        p.extend_from_slice(&0u16.to_be_bytes());
        p.extend_from_slice(&1u16.to_be_bytes());
        let r = e.handle_aecp(&cmd(aem_cmd::GET_NAME, p, id), OUR_MAC, 0);
        assert_eq!(r.status, aecp_status::NO_SUCH_DESCRIPTOR);
    }

    /// SET_NAME must change what READ_DESCRIPTOR reports, not merely echo.
    /// A name that only came back from GET_NAME while the descriptor kept the
    /// old one is a lie a controller cannot detect until it re-reads.
    #[test]
    fn set_name_changes_the_descriptor_and_not_just_the_echo() {
        let mut e = duplex_entity();
        let id = e.entity_id();
        let set = |e: &mut TalkerEntity, ty: u16, idx: u16, s: &str| {
            let mut p = ty.to_be_bytes().to_vec();
            p.extend_from_slice(&idx.to_be_bytes());
            p.extend_from_slice(&0u16.to_be_bytes());
            p.extend_from_slice(&0u16.to_be_bytes());
            p.extend_from_slice(&desc::name_to_bytes(s));
            e.handle_aecp(&cmd(aem_cmd::SET_NAME, p, id), OUR_MAC, 0)
        };

        assert_eq!(
            set(&mut e, dt::ENTITY, 0, "Dexter's Mac").status,
            aecp_status::SUCCESS
        );
        let d = dec::decode_entity(&e.encode_descriptor(dt::ENTITY, 0).unwrap()).unwrap();
        assert_eq!(d.entity_name, "Dexter's Mac");

        assert_eq!(
            set(&mut e, dt::STREAM_INPUT, 1, "Drum bus").status,
            aecp_status::SUCCESS
        );
        let d = dec::decode_stream(&e.encode_descriptor(dt::STREAM_INPUT, 1).unwrap()).unwrap();
        assert_eq!(d.object_name, "Drum bus");
        // Sink 0 is untouched.
        let d0 = dec::decode_stream(&e.encode_descriptor(dt::STREAM_INPUT, 0).unwrap()).unwrap();
        assert_ne!(d0.object_name, "Drum bus");

        assert_eq!(
            set(&mut e, dt::AUDIO_CLUSTER, 20, "Kick in").status,
            aecp_status::SUCCESS
        );
        let c = dec::decode_audio_cluster(&e.encode_descriptor(dt::AUDIO_CLUSTER, 20).unwrap())
            .unwrap();
        assert_eq!(c.object_name, "Kick in");

        // And GET_NAME agrees with the descriptor.
        let mut p = dt::AUDIO_CLUSTER.to_be_bytes().to_vec();
        p.extend_from_slice(&20u16.to_be_bytes());
        p.extend_from_slice(&0u16.to_be_bytes());
        p.extend_from_slice(&0u16.to_be_bytes());
        let r = e.handle_aecp(&cmd(aem_cmd::GET_NAME, p, id), OUR_MAC, 0);
        assert_eq!(desc::name_from_bytes(&r.payload[8..]), "Kick in");

        // A 64-byte field truncates rather than overflowing.
        let long = "x".repeat(100);
        assert_eq!(
            set(&mut e, dt::CLOCK_DOMAIN, 0, &long).status,
            aecp_status::SUCCESS
        );
        let d =
            dec::decode_clock_domain(&e.encode_descriptor(dt::CLOCK_DOMAIN, 0).unwrap()).unwrap();
        assert_eq!(d.object_name.len(), 64);
    }

    /// Milan §5.4.2.11 / §5.4.2.17: a locked entity refuses both from any
    /// other controller, and still serves the lock holder.
    #[test]
    fn set_name_and_set_control_respect_the_entity_lock() {
        let mut e = duplex_entity();
        let id = e.entity_id();
        let holder = Eui64([1, 1, 1, 1, 1, 1, 1, 1]);
        let other = Eui64([2, 2, 2, 2, 2, 2, 2, 2]);
        // Lock once, as the holder.
        let mut c = cmd(aem_cmd::LOCK_ENTITY, vec![0; 16], id);
        c.controller_entity_id = holder;
        assert_eq!(e.handle_aecp(&c, OUR_MAC, 0).status, aecp_status::SUCCESS);

        let name_cmd = |ctrl: Eui64| {
            let mut p = dt::ENTITY.to_be_bytes().to_vec();
            p.extend_from_slice(&0u16.to_be_bytes());
            p.extend_from_slice(&0u16.to_be_bytes());
            p.extend_from_slice(&0u16.to_be_bytes());
            p.extend_from_slice(&desc::name_to_bytes("Taken"));
            let mut c = cmd(aem_cmd::SET_NAME, p, id);
            c.controller_entity_id = ctrl;
            c
        };
        assert_eq!(
            e.handle_aecp(&name_cmd(other), OUR_MAC, 0).status,
            aecp_status::ENTITY_LOCKED
        );
        assert_eq!(
            e.handle_aecp(&name_cmd(holder), OUR_MAC, 0).status,
            aecp_status::SUCCESS
        );

        let ctl_cmd = |ctrl: Eui64, v: u8| {
            let mut p = dt::CONTROL.to_be_bytes().to_vec();
            p.extend_from_slice(&0u16.to_be_bytes());
            p.push(v);
            let mut c = cmd(aem_cmd::SET_CONTROL, p, id);
            c.controller_entity_id = ctrl;
            c
        };
        assert_eq!(
            e.handle_aecp(&ctl_cmd(other, 0xFF), OUR_MAC, 0).status,
            aecp_status::ENTITY_LOCKED
        );
        assert_eq!(
            e.handle_aecp(&ctl_cmd(holder, 0xFF), OUR_MAC, 0).status,
            aecp_status::SUCCESS
        );
    }

    /// GET/SET_CONTROL on the IDENTIFY control, and the descriptor following
    /// along. 1722.1 §7.3.5.2: 0 is off and 255 is on; a value in between is
    /// a controller that misunderstood the control, not a dimmer.
    #[test]
    fn identify_control_round_trips_and_rejects_a_non_binary_value() {
        let mut e = duplex_entity();
        let id = e.entity_id();
        let get = |e: &mut TalkerEntity| {
            let mut p = dt::CONTROL.to_be_bytes().to_vec();
            p.extend_from_slice(&0u16.to_be_bytes());
            e.handle_aecp(&cmd(aem_cmd::GET_CONTROL, p, id), OUR_MAC, 0)
        };
        let set = |e: &mut TalkerEntity, v: u8| {
            let mut p = dt::CONTROL.to_be_bytes().to_vec();
            p.extend_from_slice(&0u16.to_be_bytes());
            p.push(v);
            e.handle_aecp(&cmd(aem_cmd::SET_CONTROL, p, id), OUR_MAC, 0)
        };

        let r = get(&mut e);
        assert_eq!(r.status, aecp_status::SUCCESS);
        assert_eq!(r.payload.get(4), Some(&0), "identify starts off");

        assert_eq!(set(&mut e, 0xFF).status, aecp_status::SUCCESS);
        assert_eq!(get(&mut e).payload.get(4), Some(&0xFF));
        // The descriptor's `current` value follows, so the state is visible in
        // the tree and not only through GET_CONTROL.
        let d = dec::decode_control(&e.encode_descriptor(dt::CONTROL, 0).unwrap()).unwrap();
        assert_eq!(d.values.get(4), Some(&0xFF));

        assert_eq!(set(&mut e, 0).status, aecp_status::SUCCESS);
        assert_eq!(get(&mut e).payload.get(4), Some(&0));

        assert_eq!(set(&mut e, 0x80).status, aecp_status::BAD_ARGUMENTS);
        assert_eq!(get(&mut e).payload.get(4), Some(&0), "refused, not applied");

        // Only CONTROL index 0 exists.
        let mut p = dt::CONTROL.to_be_bytes().to_vec();
        p.extend_from_slice(&1u16.to_be_bytes());
        let r = e.handle_aecp(&cmd(aem_cmd::GET_CONTROL, p, id), OUR_MAC, 0);
        assert_eq!(r.status, aecp_status::NO_SUCH_DESCRIPTOR);
    }

    fn rx_cmd(message_type: u8, listener: Eui64, luid: u16) -> Acmpdu {
        Acmpdu {
            message_type,
            status: acmp_status::SUCCESS,
            stream_id: StreamId(0xDEAD_BEEF),
            controller_entity_id: Eui64([9; 8]),
            talker_entity_id: Eui64([7; 8]),
            listener_entity_id: listener,
            talker_unique_id: 4,
            listener_unique_id: luid,
            stream_dest_mac: MacAddr([0xAA; 6]),
            connection_count: 77,
            sequence_id: 1234,
            flags: 0,
            stream_vlan_id: 9,
        }
    }

    /// A command addressed to another entity is IGNORED, not answered.
    /// §5.5.3.1: "The listener shall ignore all other ACMP messages" -- and
    /// ACMP is multicast, so answering someone else's command is noise every
    /// controller on the link has to filter.
    #[test]
    fn a_listener_command_for_another_entity_is_ignored() {
        let mut e = duplex_entity();
        let c = rx_cmd(acmp_msg::CONNECT_RX_COMMAND, Eui64([0xFE; 8]), 0);
        assert!(e.handle_acmp_listener(&c, 0, 0).is_none());
        // And a talker-side command is not ours to answer here either.
        let c = rx_cmd(acmp_msg::CONNECT_TX_COMMAND, e.entity_id(), 0);
        assert!(e.handle_acmp_listener(&c, 0, 0).is_none());
    }

    /// Table 5.27: an invalid listener_unique_id is LISTENER_UNKNOWN_ID with
    /// every routing field echoed, so the controller can match the response.
    #[test]
    fn an_invalid_sink_id_is_listener_unknown_id_with_routing_echoed() {
        let mut e = duplex_entity();
        let id = e.entity_id();
        for ty in [
            acmp_msg::CONNECT_RX_COMMAND,
            acmp_msg::DISCONNECT_RX_COMMAND,
            acmp_msg::GET_RX_STATE_COMMAND,
        ] {
            let c = rx_cmd(ty, id, 99);
            let (r, actions) = e.handle_acmp_listener(&c, 0, 0).expect("answered");
            assert_eq!(r.status, acmp_status::LISTENER_UNKNOWN_ID, "type {ty:#04x}");
            assert_eq!(r.sequence_id, c.sequence_id);
            assert_eq!(r.controller_entity_id, c.controller_entity_id);
            assert_eq!(r.listener_unique_id, c.listener_unique_id);
            assert_eq!(r.talker_unique_id, c.talker_unique_id);
            assert!(actions.is_empty());
        }
        // A transmit-only entity has no sinks at all, so even sink 0 is
        // unknown rather than silently accepted.
        let mut tx = entity();
        let tid = tx.entity_id();
        let c = rx_cmd(acmp_msg::CONNECT_RX_COMMAND, tid, 0);
        let (r, _) = tx.handle_acmp_listener(&c, 0, 0).expect("answered");
        assert_eq!(r.status, acmp_status::LISTENER_UNKNOWN_ID);
    }

    /// Table 5.37: an UNBOUND sink ZEROES the talker fields. Echoing them
    /// would report a binding to whatever talker the controller happened to
    /// name in its query -- a connection that does not exist.
    #[test]
    fn get_rx_state_on_an_unbound_sink_zeroes_the_talker_fields() {
        let mut e = duplex_entity();
        let id = e.entity_id();
        let c = rx_cmd(acmp_msg::GET_RX_STATE_COMMAND, id, 1);
        let (r, _) = e.handle_acmp_listener(&c, 0, 0).expect("answered");
        assert_eq!(r.status, acmp_status::SUCCESS);
        assert_eq!(r.message_type, acmp_msg::GET_RX_STATE_RESPONSE);
        assert_eq!(r.talker_entity_id, Eui64([0; 8]), "must not echo");
        assert_eq!(r.talker_unique_id, 0, "must not echo");
        assert_eq!(r.connection_count, 0);
        assert_eq!(r.stream_id, StreamId(0));
        assert_eq!(r.stream_dest_mac, MacAddr([0; 6]));
        assert_eq!(r.stream_vlan_id, 0);
        assert_eq!(r.flags, 0);
        // Routing fields still echo, or the response is unmatchable.
        assert_eq!(r.sequence_id, c.sequence_id);
        assert_eq!(r.controller_entity_id, c.controller_entity_id);
        assert_eq!(r.listener_unique_id, 1);
    }

    /// CONNECT_RX binds the sink, and GET_RX_STATE then reports the binding
    /// (Table 5.44) rather than the zeroes of Table 5.37.
    #[test]
    fn connect_rx_binds_a_sink_and_get_rx_state_then_reports_it() {
        let mut e = duplex_entity();
        let id = e.entity_id();
        let c = rx_cmd(acmp_msg::CONNECT_RX_COMMAND, id, 0);
        let (r, actions) = e.handle_acmp_listener(&c, 0, 0).expect("answered");
        assert_eq!(r.status, acmp_status::SUCCESS);
        assert_eq!(r.message_type, acmp_msg::CONNECT_RX_RESPONSE);
        // The sink is now bound and waiting for the talker's ADP. It emits no
        // action yet, and must not: PRB_W_AVAIL exists precisely because
        // probing a talker we have not heard from cannot succeed, so the only
        // correct thing to do here is wait.
        assert!(actions.is_empty(), "no probe before the talker is known");
        assert_eq!(e.sink_sms()[0].state(), acmp_listener::State::PrbWAvail);
        assert!(e.sink_sms()[0].bound_to().is_some());

        let q = rx_cmd(acmp_msg::GET_RX_STATE_COMMAND, id, 0);
        let (r, _) = e.handle_acmp_listener(&q, 0, 0).expect("answered");
        assert_eq!(r.status, acmp_status::SUCCESS);
        assert_eq!(r.talker_entity_id, c.talker_entity_id);
        assert_eq!(r.talker_unique_id, c.talker_unique_id);
        assert_eq!(r.connection_count, 1);
        assert_ne!(r.flags & acmp_flags::FAST_CONNECT, 0);

        // Sink 1 is untouched -- binding is per sink, not per entity.
        let q1 = rx_cmd(acmp_msg::GET_RX_STATE_COMMAND, id, 1);
        let (r1, _) = e.handle_acmp_listener(&q1, 0, 0).expect("answered");
        assert_eq!(r1.connection_count, 0);
        assert_eq!(r1.talker_entity_id, Eui64([0; 8]));

        // DISCONNECT_RX releases it again.
        let d = rx_cmd(acmp_msg::DISCONNECT_RX_COMMAND, id, 0);
        let (r, _) = e.handle_acmp_listener(&d, 0, 0).expect("answered");
        assert_eq!(r.status, acmp_status::SUCCESS);
        assert_eq!(r.message_type, acmp_msg::DISCONNECT_RX_RESPONSE);
        let (r, _) = e.handle_acmp_listener(&q, 0, 0).expect("answered");
        assert_eq!(r.connection_count, 0, "unbound again");
    }

    /// STREAMING_WAIT must be saved from the bind and reported back verbatim
    /// (§5.5.3.6.3 step 2, Table 5.44).
    #[test]
    fn streaming_wait_is_saved_from_the_bind_and_reported_back() {
        let mut e = duplex_entity();
        let id = e.entity_id();
        let mut c = rx_cmd(acmp_msg::CONNECT_RX_COMMAND, id, 0);
        c.flags = acmp_flags::STREAMING_WAIT;
        assert_eq!(
            e.handle_acmp_listener(&c, 0, 0).unwrap().0.status,
            acmp_status::SUCCESS
        );
        let q = rx_cmd(acmp_msg::GET_RX_STATE_COMMAND, id, 0);
        let (r, _) = e.handle_acmp_listener(&q, 0, 0).expect("answered");
        assert_ne!(r.flags & acmp_flags::STREAMING_WAIT, 0);

        // A bind without it reports it clear.
        let mut e2 = duplex_entity();
        let id2 = e2.entity_id();
        let c2 = rx_cmd(acmp_msg::CONNECT_RX_COMMAND, id2, 0);
        e2.handle_acmp_listener(&c2, 0, 0);
        let q2 = rx_cmd(acmp_msg::GET_RX_STATE_COMMAND, id2, 0);
        let (r2, _) = e2.handle_acmp_listener(&q2, 0, 0).expect("answered");
        assert_eq!(r2.flags & acmp_flags::STREAMING_WAIT, 0);
    }

    /// Table 5.32: bind and unbind from a non-holder are
    /// CONTROLLER_NOT_AUTHORIZED -- which is NOT the AECP ENTITY_LOCKED, that
    /// being the status for the MVU BIND_STREAM form (Table 5.33).
    /// GET_RX_STATE is a read and is deliberately not gated.
    #[test]
    fn bind_and_unbind_respect_the_lock_but_get_rx_state_does_not() {
        let mut e = duplex_entity();
        let id = e.entity_id();
        let holder = Eui64([1; 8]);
        let mut lk = cmd(aem_cmd::LOCK_ENTITY, vec![0; 16], id);
        lk.controller_entity_id = holder;
        assert_eq!(e.handle_aecp(&lk, OUR_MAC, 0).status, aecp_status::SUCCESS);

        for ty in [
            acmp_msg::CONNECT_RX_COMMAND,
            acmp_msg::DISCONNECT_RX_COMMAND,
        ] {
            let c = rx_cmd(ty, id, 0); // controller [9;8], not the holder
            let (r, actions) = e.handle_acmp_listener(&c, 0, 0).expect("answered");
            assert_eq!(
                r.status,
                acmp_status::CONTROLLER_NOT_AUTHORIZED,
                "type {ty:#04x}"
            );
            assert!(actions.is_empty(), "a refused command changes nothing");
        }
        // The read is still served to anyone.
        let q = rx_cmd(acmp_msg::GET_RX_STATE_COMMAND, id, 0);
        let (r, _) = e.handle_acmp_listener(&q, 0, 0).expect("answered");
        assert_eq!(r.status, acmp_status::SUCCESS);
        // And the holder can bind.
        let mut c = rx_cmd(acmp_msg::CONNECT_RX_COMMAND, id, 0);
        c.controller_entity_id = holder;
        let (r, _) = e.handle_acmp_listener(&c, 0, 0).expect("answered");
        assert_eq!(r.status, acmp_status::SUCCESS);
    }

    /// START/STOP_STREAMING on a Stream Input, and the explicit
    /// NOT_SUPPORTED a Stream Output must get (Milan §5.4.2.19-20).
    #[test]
    fn start_and_stop_streaming_work_on_inputs_and_are_refused_on_outputs() {
        let mut e = duplex_entity();
        let id = e.entity_id();
        let go = |e: &mut TalkerEntity, c: u16, ty: u16, idx: u16| {
            let mut p = ty.to_be_bytes().to_vec();
            p.extend_from_slice(&idx.to_be_bytes());
            e.handle_aecp(&cmd(c, p, id), OUR_MAC, 0)
        };

        // A Stream Output is NOT_SUPPORTED -- not NO_SUCH_DESCRIPTOR, since
        // the descriptor does exist; the command is just not applicable.
        for c in [aem_cmd::START_STREAMING, aem_cmd::STOP_STREAMING] {
            let r = go(&mut e, c, dt::STREAM_OUTPUT, 0);
            assert_eq!(r.status, aecp_status::NOT_SUPPORTED);
            // ...but an output index that does not exist is still missing.
            let r = go(&mut e, c, dt::STREAM_OUTPUT, 99);
            assert_eq!(r.status, aecp_status::NO_SUCH_DESCRIPTOR);
            // And a descriptor type the command does not apply to.
            let r = go(&mut e, c, dt::AUDIO_UNIT, 0);
            assert_eq!(r.status, aecp_status::NO_SUCH_DESCRIPTOR);
        }

        // Unbound: SUCCESS with no effect, explicitly not an error.
        let r = go(&mut e, aem_cmd::START_STREAMING, dt::STREAM_INPUT, 0);
        assert_eq!(r.status, aecp_status::SUCCESS);
        assert!(!e.sink_sms()[0].started(), "an unbound sink cannot start");

        // Bind with STREAMING_WAIT: bound but held stopped, which is the
        // whole point of the flag.
        let mut b = rx_cmd(acmp_msg::CONNECT_RX_COMMAND, id, 0);
        b.flags = acmp_flags::STREAMING_WAIT;
        e.handle_acmp_listener(&b, 0, 0);
        assert!(!e.sink_sms()[0].started());

        let r = go(&mut e, aem_cmd::START_STREAMING, dt::STREAM_INPUT, 0);
        assert_eq!(r.status, aecp_status::SUCCESS);
        assert!(e.sink_sms()[0].started());
        // Idempotent.
        assert_eq!(
            go(&mut e, aem_cmd::START_STREAMING, dt::STREAM_INPUT, 0).status,
            aecp_status::SUCCESS
        );
        assert!(e.sink_sms()[0].started());

        let r = go(&mut e, aem_cmd::STOP_STREAMING, dt::STREAM_INPUT, 0);
        assert_eq!(r.status, aecp_status::SUCCESS);
        assert!(!e.sink_sms()[0].started());

        // A sink past the end is missing.
        assert_eq!(
            go(&mut e, aem_cmd::START_STREAMING, dt::STREAM_INPUT, 9).status,
            aecp_status::NO_SUCH_DESCRIPTOR
        );
    }

    /// A bind WITHOUT STREAMING_WAIT starts immediately, and unbinding stops
    /// the stream -- a started sink with no talker would be a contradiction a
    /// later GET_RX_STATE would report.
    #[test]
    fn a_plain_bind_starts_streaming_and_unbind_stops_it() {
        let mut e = duplex_entity();
        let id = e.entity_id();
        let b = rx_cmd(acmp_msg::CONNECT_RX_COMMAND, id, 0);
        e.handle_acmp_listener(&b, 0, 0);
        assert!(e.sink_sms()[0].started(), "no STREAMING_WAIT means started");

        let d = rx_cmd(acmp_msg::DISCONNECT_RX_COMMAND, id, 0);
        e.handle_acmp_listener(&d, 0, 0);
        assert!(!e.sink_sms()[0].started());
        assert!(e.sink_sms()[0].bound_to().is_none());
    }

    /// Both are lock-gated (§5.4.2.19-20).
    #[test]
    fn streaming_commands_respect_the_entity_lock() {
        let mut e = duplex_entity();
        let id = e.entity_id();
        let holder = Eui64([1; 8]);
        let mut lk = cmd(aem_cmd::LOCK_ENTITY, vec![0; 16], id);
        lk.controller_entity_id = holder;
        assert_eq!(e.handle_aecp(&lk, OUR_MAC, 0).status, aecp_status::SUCCESS);

        let mut p = dt::STREAM_INPUT.to_be_bytes().to_vec();
        p.extend_from_slice(&0u16.to_be_bytes());
        let mut c = cmd(aem_cmd::START_STREAMING, p.clone(), id);
        c.controller_entity_id = Eui64([2; 8]);
        assert_eq!(
            e.handle_aecp(&c, OUR_MAC, 0).status,
            aecp_status::ENTITY_LOCKED
        );
        let mut c = cmd(aem_cmd::START_STREAMING, p, id);
        c.controller_entity_id = holder;
        assert_eq!(e.handle_aecp(&c, OUR_MAC, 0).status, aecp_status::SUCCESS);
    }

    /// Milan §5.3.3.6: a device that recovers its media clock from an
    /// arriving stream must advertise an INPUT_STREAM clock source. The
    /// daemon really does source on "16R media clock (recovered from the
    /// 16R's stream)", so advertising only INTERNAL understated what we do.
    #[test]
    fn a_sinking_entity_advertises_an_input_stream_clock_source() {
        let dx = duplex_entity();
        assert_eq!(dx.clock_source_count(), 3);
        let b = dx.encode_descriptor(dt::CLOCK_SOURCE, 1).unwrap();
        let cs = dec::decode_clock_source(&b).expect("decodable");
        assert_eq!(cs.clock_source_type, clock_source::INPUT_STREAM);
        assert_eq!(cs.clock_source_location_type, dt::STREAM_INPUT);
        assert_eq!(cs.clock_source_location_index, 0);

        let gptp = dec::decode_clock_source(&dx.encode_descriptor(dt::CLOCK_SOURCE, 2).unwrap())
            .expect("gPTP clock source");
        assert_eq!(gptp.clock_source_type, clock_source::EXTERNAL);
        assert_eq!(gptp.clock_source_location_type, dt::TIMING);
        assert_eq!(gptp.clock_source_location_index, 0);

        let timing =
            dec::decode_timing(&dx.encode_descriptor(dt::TIMING, 0).unwrap()).expect("TIMING");
        assert_eq!(timing.algorithm, 0);
        assert_eq!(timing.ptp_instances, vec![0]);
        let instance =
            dec::decode_ptp_instance(&dx.encode_descriptor(dt::PTP_INSTANCE, 0).unwrap())
                .expect("PTP_INSTANCE");
        assert_eq!(instance.clock_identity, dx.model().entity_id);
        assert_eq!(instance.flags, 1, "GRANDMASTER_CAPABLE");
        assert_eq!(instance.number_of_ptp_ports, 1);
        let port = dec::decode_ptp_port(&dx.encode_descriptor(dt::PTP_PORT, 0).unwrap())
            .expect("PTP_PORT");
        assert_eq!(port.port_type, 0);
        assert_eq!(port.avb_interface_index, 0);

        // The CLOCK_DOMAIN must list all three, or either network source is
        // unreachable to a controller.
        let b = dx.encode_descriptor(dt::CLOCK_DOMAIN, 0).unwrap();
        let cd = dec::decode_clock_domain(&b).expect("decodable");
        assert_eq!(cd.clock_sources, vec![0, 1, 2]);

        // Transmit-only has no stream to recover from, but gPTP remains a
        // valid source at index 1.
        let tx = entity();
        assert_eq!(tx.clock_source_count(), 2);
        let gptp = dec::decode_clock_source(&tx.encode_descriptor(dt::CLOCK_SOURCE, 1).unwrap())
            .expect("gPTP clock source");
        assert_eq!(gptp.clock_source_location_type, dt::TIMING);
        let cd = dec::decode_clock_domain(&tx.encode_descriptor(dt::CLOCK_DOMAIN, 0).unwrap())
            .expect("decodable");
        assert_eq!(cd.clock_sources, vec![0, 1]);
    }

    /// The full-width entity: eight streams, 64 channels.
    fn wide_entity() -> TalkerEntity {
        let m = talker_model(OUR_MAC, &stream_ids(8), FMT, "phase1b-test", 0)
            .expect("decodable format");
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
        let m2 = talker_model(OUR_MAC, &stream_ids(1), other, "x", 0).unwrap();
        assert_ne!(m2.channels, m.channels);
        assert_ne!(m2.entity_model_id, m.entity_model_id);
        // Gaining sinks changes the AEM tree, so it must change the id too.
        // Otherwise a controller holding the cached transmit-only tree never
        // learns we have STREAM_INPUTs, and the ADP's listener_stream_sinks
        // contradicts the model it believes it already has.
        let dx = talker_model(OUR_MAC, &stream_ids(1), FMT, "phase1b-test", 2).unwrap();
        assert_eq!(dx.stream_count(), m.stream_count());
        assert_ne!(
            dx.entity_model_id, m.entity_model_id,
            "transmit-only and duplex must not share a model id"
        );
        // And different sink counts differ from each other.
        let dx1 = talker_model(OUR_MAC, &stream_ids(1), FMT, "phase1b-test", 1).unwrap();
        assert_ne!(dx1.entity_model_id, dx.entity_model_id);
    }

    #[test]
    fn talker_model_refuses_a_non_aaf_format() {
        // A CRF format has no AAF channel count; we must not invent one.
        let crf = StreamFormat(0x0410_6001_0000_BB80);
        assert!(talker_model(OUR_MAC, &stream_ids(1), crf, "x", 0).is_none());
        // And no streams at all is refused rather than producing an entity
        // that describes nothing.
        assert!(talker_model(OUR_MAC, &[], FMT, "x", 0).is_none());
    }

    /// The whole point of this module: a controller walks ENTITY ->
    /// CONFIGURATION 0 -> every (type, index) in descriptor_counts. If any of
    /// those answers NO_SUCH_DESCRIPTOR the walk breaks, which is exactly how
    /// the 16R ended up listing no input device for us.
    #[test]
    fn every_advertised_descriptor_is_answerable() {
        for e in [entity(), wide_entity()] {
            check_walk(&e);
        }
    }

    /// The whole AEM walk against one entity, used for both the 1-stream and
    /// the 8-stream configuration: advertising a count we cannot serve is
    /// worse than advertising none, because the controller asks for it and
    /// gets NO_SUCH_DESCRIPTOR mid-walk and gives up on the entity.
    fn check_walk(e: &TalkerEntity) {
        // ENTITY and CONFIGURATION are the walk's roots and are not listed in
        // descriptor_counts, so check them explicitly.
        assert!(e.encode_descriptor(dt::ENTITY, 0).is_some());
        assert!(e.encode_descriptor(dt::CONFIGURATION, 0).is_some());
        let counts = e.descriptor_counts();
        assert!(!counts.is_empty());
        for (ty, n) in counts {
            assert!(n > 0, "advertised a zero count for type {ty:#06x}");
            for index in 0..n {
                let bytes = e.encode_descriptor(ty, index).unwrap_or_else(|| {
                    panic!(
                        "descriptor_counts advertises {ty:#06x}[{index}] but the responder cannot encode it"
                    )
                });
                assert_eq!(
                    bytes.get(2..4),
                    Some(index.to_be_bytes().as_slice()),
                    "descriptor {ty:#06x}[{index}] body carries the wrong index",
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

    /// Only `advertise()` may advance `available_index`.
    ///
    /// IEEE 1722.1 has a controller treat a non-sequential `available_index`
    /// as proof the entity restarted, and respond by discarding its model of
    /// it. So any OTHER call that bumps the counter corrupts every
    /// advertisement that follows.
    ///
    /// A real regression: the daemon polled the grandmaster by calling
    /// `advertise()` on a loop running at PDU rate, bumping the counter tens
    /// of thousands of times between the ADPDUs that actually went out. The
    /// 16R had to read each one as a restart. `advertised_grandmaster_id`
    /// exists so that read is free, and this test is what fails if a reader
    /// ever starts mutating again.
    #[test]
    fn reading_entity_state_never_advances_available_index() {
        let mut e = entity();
        e.advertise();
        let before = e.available_index();

        // Every read a caller might reasonably make on a hot loop.
        for _ in 0..1_000 {
            let _ = e.advertised_grandmaster_id();
            let _ = e.entity_id();
            let _ = e.departing();
            let _ = e.available_index();
        }
        assert_eq!(
            e.available_index(),
            before,
            "a read advanced available_index; a controller will see a restart"
        );

        // And the next real advertisement still steps by exactly one, which is
        // the property the spec actually requires.
        assert_eq!(e.advertise().available_index, before);
        assert_eq!(e.available_index(), before + 1);
    }

    /// The accessor must agree with what an advertisement carries, or the
    /// change detector watches a different value than the wire shows.
    #[test]
    fn advertised_grandmaster_accessor_matches_the_pdu() {
        let mut e = entity();
        // Before any gPTP status: build_adp falls back to our clockIdentity.
        assert_eq!(
            e.advertised_grandmaster_id(),
            e.advertise().gptp_grandmaster_id
        );
        // And after a real grandmaster is pushed in.
        e.set_gptp(GptpStatus {
            grandmaster_id: 0x000a_92ff_fe03_3ba9,
            ..Default::default()
        });
        assert_eq!(
            e.advertised_grandmaster_id(),
            e.advertise().gptp_grandmaster_id
        );
        assert_eq!(e.advertised_grandmaster_id(), 0x000a_92ff_fe03_3ba9);
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
    fn avb_interface_under_claims_grandmaster_until_told_otherwise() {
        let e = entity();
        let bytes = e.encode_descriptor(dt::AVB_INTERFACE, 0).unwrap();
        let d = dec::decode_avb_interface(&bytes).expect("decodable");
        assert_eq!(d.mac_address, OUR_MAC);
        assert_eq!(
            d.interface_flags,
            if_flags::GPTP_SUPPORTED | if_flags::SRP_SUPPORTED
        );
        // The default GptpStatus under-claims: a caller that has not told us
        // the gPTP layer can be grandmaster must not advertise that it can.
        assert_eq!(d.interface_flags & if_flags::GPTP_GRANDMASTER_SUPPORTED, 0);
        assert_eq!(d.priority1, 255);
    }

    /// The descriptor must report the dataset the gPTP port ANNOUNCES, not a
    /// second copy of it.
    ///
    /// These five values were hardcoded here and had drifted from the wire --
    /// the descriptor claimed clockAccuracy 0xFE while the port announces
    /// 0x20 -- so a controller reading our model saw a different clock than
    /// the one announcing. This test fails if they are ever decoupled again.
    #[test]
    fn avb_interface_reports_the_announced_dataset_and_grandmaster_capability() {
        let mut e = entity();
        e.set_gptp(GptpStatus {
            priority1: 248,
            priority2: 248,
            clock_class: 248,
            clock_accuracy: 0x20,
            offset_scaled_log_variance: 0x436A,
            grandmaster_capable: true,
            ..GptpStatus::default()
        });
        let bytes = e.encode_descriptor(dt::AVB_INTERFACE, 0).unwrap();
        let d = dec::decode_avb_interface(&bytes).expect("decodable");
        assert_ne!(
            d.interface_flags & if_flags::GPTP_GRANDMASTER_SUPPORTED,
            0,
            "Milan v1.3 §4.3.1.1: a Talker PAAD is grandmaster-capable"
        );
        assert_eq!(d.priority1, 248);
        assert_eq!(d.priority2, 248);
        assert_eq!(d.clock_class, 248);
        assert_eq!(d.clock_accuracy, 0x20, "must match what we announce");
        assert_eq!(d.offset_scaled_log_variance, 0x436A);
        // The gPTP clockIdentity is the EUI-64 of this interface's MAC, which
        // is also how the entity id is derived -- the same clock either way.
        assert_eq!(d.clock_identity, Eui64::from_mac(OUR_MAC));
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
            ..GptpStatus::default()
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
    fn ptp_grandmaster_info_reencodes_the_apple_capture() {
        let mut e = entity();
        let id = e.entity_id();
        e.set_gptp(GptpStatus {
            grandmaster_id: 0xAEBE_A0C5_58FA_0002,
            grandmaster: Some(crate::entity::GrandmasterStatus {
                identity: 0xAEBE_A0C5_58FA_0002,
                priority1: 0xF8,
                priority2: 0xF3,
                clock_class: 0x21,
                clock_accuracy: 0x43,
                offset_scaled_log_variance: 0x6AFA,
                domain_number: 0,
                time_source: 0xA0,
                master_port_identity: 0xAEBE_A0C5_58FA_0002,
                master_port_number: 0,
                steps_removed: 0,
            }),
            ..GptpStatus::default()
        });
        let r = e.handle_aecp(
            &cmd(
                aem_cmd::GET_PTP_INSTANCE_GRANDMASTER_INFO,
                vec![0x00, 0x27, 0x00, 0x00],
                id,
            ),
            OUR_MAC,
            0,
        );
        assert_eq!(r.status, aecp_status::SUCCESS);
        let captured = "00270000aebea0c558fa0002f821436afaf300a00100000000aebea0c558fa000200000000";
        let (pairs, remainder) = captured.as_bytes().as_chunks::<2>();
        assert!(remainder.is_empty());
        let expected: Vec<u8> = pairs
            .iter()
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).expect("hex"), 16).expect("byte"))
            .collect();
        assert_eq!(r.payload, expected);

        let controller = Eui64::from_u64(0x000A_92FF_FE03_3BA9);
        let register = cmd(aem_cmd::REGISTER_UNSOLICITED_NOTIFICATION, vec![], id);
        let mut register = register;
        register.controller_entity_id = controller;
        assert_eq!(
            e.handle_aecp(&register, MacAddr([0, 10, 146, 3, 59, 169]), 0)
                .status,
            aecp_status::SUCCESS
        );
        let pushed = e.unsolicited_desc(
            aem_cmd::GET_PTP_INSTANCE_GRANDMASTER_INFO,
            dt::PTP_INSTANCE,
            0,
        );
        assert_eq!(pushed.len(), 1);
        assert!(pushed[0].1.unsolicited);
        assert_eq!(pushed[0].1.payload, expected);
    }

    #[test]
    fn adp_advertises_the_identify_control_and_actually_has_one() {
        let mut e = entity();
        let adp = e.advertise();

        // The flag is what gives `identify_control_index` meaning. Without it
        // a controller must ignore the index, so an entity with a perfectly
        // good IDENTIFY control is indistinguishable from one with none --
        // which is how ours advertised itself while the 16R's controller
        // listed us as offline. The 16R sets this bit; we did not.
        assert_ne!(
            adp.entity_capabilities & entity_caps::AEM_IDENTIFY_CONTROL_INDEX_VALID,
            0,
            "ADP must mark identify_control_index valid"
        );

        // ... and the bit must not outlive the descriptor it points at. This
        // is the half that stops the pair from drifting: claiming an IDENTIFY
        // control we cannot return would break the walk at that index, which
        // is the same class of fault as advertising a descriptor count we
        // cannot answer.
        let counts = e.descriptor_counts();
        let controls = counts
            .iter()
            .find(|(t, _)| *t == dt::CONTROL)
            .map(|(_, n)| *n)
            .unwrap_or(0);
        assert!(
            controls > adp.identify_control_index,
            "identify_control_index {} is outside the {controls} CONTROL(s) we advertise",
            adp.identify_control_index,
        );

        // And it has to be answerable, not merely counted.
        let id = e.entity_id();
        let r = e.handle_aecp(
            &cmd(
                aem_cmd::READ_DESCRIPTOR,
                read_desc_payload(dt::CONTROL, adp.identify_control_index),
                id,
            ),
            OUR_MAC,
            0,
        );
        assert_eq!(
            r.status,
            aecp_status::SUCCESS,
            "the identify control we point at must be readable"
        );
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
        let sid = e.model().first_stream_id();
        assert_eq!(sid.mac(), OUR_MAC);
        assert_eq!(sid.unique_id(), 0);
    }

    /// The eight-stream entity must describe a 64-channel device: eight
    /// STREAM_OUTPUTs, eight ports, eight maps and sixty-four clusters. These
    /// are asserted as LITERALS, not as `model.cluster_count()`, so a change
    /// to the arithmetic fails here instead of silently redefining the device.
    #[test]
    fn the_wide_entity_describes_sixty_four_channels() {
        let e = wide_entity();
        let m = e.model();
        assert_eq!(m.stream_count(), 8);
        assert_eq!(m.channels, 8);
        assert_eq!(m.cluster_count(), 64);
        assert_eq!(m.channel_names.len(), 64);

        let counts = e.descriptor_counts();
        let get = |t: u16| {
            counts
                .iter()
                .copied()
                .find(|&(ty, _)| ty == t)
                .map(|(_, n)| n)
        };
        assert_eq!(get(dt::STREAM_OUTPUT), Some(8));
        assert_eq!(get(dt::STREAM_PORT_OUTPUT), Some(8));
        assert_eq!(get(dt::AUDIO_MAP), Some(8));
        assert_eq!(get(dt::AUDIO_CLUSTER), Some(64));
        assert_eq!(get(dt::AUDIO_UNIT), Some(1));

        // ADP and the ENTITY descriptor must agree on the source count, or a
        // controller treats the entity as misbehaving.
        let d = desc::decode_entity(&e.encode_descriptor(dt::ENTITY, 0).unwrap()).unwrap();
        assert_eq!(d.talker_stream_sources, 8);
    }

    /// Each port must own its OWN eight clusters, so cluster 9 belongs to
    /// stream 1 and a controller can tell which eight channels are which. A
    /// port that pointed at base_cluster 0 would map all eight streams onto
    /// the same eight channels.
    #[test]
    fn each_stream_port_owns_its_own_cluster_block() {
        let e = wide_entity();
        let mut seen = Vec::new();
        for i in 0..8u16 {
            let raw = e
                .encode_descriptor(dt::STREAM_PORT_OUTPUT, i)
                .expect("port exists");
            let p = desc::decode_stream_port(&raw).expect("decodes");
            assert_eq!(p.number_of_clusters, 8, "port {i}");
            assert_eq!(p.base_cluster, i * 8, "port {i}");
            assert_eq!(p.number_of_maps, 1, "port {i}");
            assert_eq!(p.base_map, i, "port {i}");
            seen.push(p.base_cluster);
        }
        // Every block distinct and together covering 0..64.
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 8, 16, 24, 32, 40, 48, 56]);
    }

    /// Each stream's descriptor must carry ITS OWN stream id. All eight
    /// sharing one id would have the 16R bind the same stream eight times.
    #[test]
    fn each_stream_output_carries_its_own_id() {
        let e = wide_entity();
        let ids: Vec<u64> = e.model().streams.iter().map(|s| s.stream_id.0).collect();
        let mut uniq = ids.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), 8, "stream ids must be distinct: {ids:#018x?}");
        for (i, s) in e.model().streams.iter().enumerate() {
            assert_eq!(s.stream_id.mac(), OUR_MAC);
            assert_eq!(s.stream_id.unique_id(), i as u16);
            // The name says which channels it carries.
            assert!(
                s.object_name
                    .contains(&format!("{}-{}", i * 8 + 1, (i + 1) * 8)),
                "stream {i} name {:?} should name its channel range",
                s.object_name
            );
        }
    }

    /// GET_STREAM_FORMAT must answer for every stream we describe, not just
    /// index 0 — a controller asks per stream before binding it.
    #[test]
    fn get_stream_format_answers_for_every_stream() {
        let mut e = wide_entity();
        for i in 0..8u16 {
            let mut pl = Vec::new();
            pl.extend_from_slice(&dt::STREAM_OUTPUT.to_be_bytes());
            pl.extend_from_slice(&i.to_be_bytes());
            let id = e.entity_id();
            let r = e.handle_aecp(&cmd(aem_cmd::GET_STREAM_FORMAT, pl, id), OUR_MAC, 0);
            assert_eq!(r.status, aecp_status::SUCCESS, "stream {i}");
            let fmt = r
                .payload
                .get(4..12)
                .and_then(|b| <[u8; 8]>::try_from(b).ok())
                .map(u64::from_be_bytes);
            assert_eq!(fmt, Some(FMT.0), "stream {i}");
        }
        // One past the end is refused.
        let mut pl = Vec::new();
        pl.extend_from_slice(&dt::STREAM_OUTPUT.to_be_bytes());
        pl.extend_from_slice(&8u16.to_be_bytes());
        let id = e.entity_id();
        assert_eq!(
            e.handle_aecp(&cmd(aem_cmd::GET_STREAM_FORMAT, pl, id), OUR_MAC, 0)
                .status,
            aecp_status::NO_SUCH_DESCRIPTOR
        );
    }

    /// A one-stream and an eight-stream entity must not share a model id: a
    /// controller caches the model by it and would serve a stale 8-channel
    /// model for a 64-channel device.
    #[test]
    fn the_stream_count_is_part_of_the_model_id() {
        let narrow = talker_model(OUR_MAC, &stream_ids(1), FMT, "x", 0).unwrap();
        let wide = talker_model(OUR_MAC, &stream_ids(8), FMT, "x", 0).unwrap();
        assert_ne!(narrow.entity_model_id, wide.entity_model_id);
    }
}
