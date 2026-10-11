//! The decoded AEM model (design §9.2 enumeration, §9.3 derived model).
//!
//! These are plain data structs the controller fills from READ_DESCRIPTOR
//! responses and `derive` builds for our own entity. Descriptor *codecs* live
//! in [`super::desc`]; the AEM dump JSON in [`super::json`].

use avb_avtp::stream_format::StreamFormat;
use avb_core::{Eui64, MacAddr};

use crate::constants::desc;

/// Localized-string reference (configuration-local string index, or 0xFFFF
/// for "no string"). We resolve it against LOCALE/STRINGS when we can.
pub type LocalizedRef = u16;

/// Guards against a misbehaving peer (design §9.2 limits).
pub const MAX_CONFIGURATIONS: usize = 64;
pub const MAX_DESCRIPTORS_PER_TYPE: usize = 1024;
pub const MAX_FORMATS: usize = 64;
pub const MAX_SAMPLING_RATES: usize = 64;
pub const MAX_MAPPINGS: usize = 1024;

/// ENTITY descriptor (IEEE 1722.1-2021 §7.2.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntityDesc {
    pub entity_id: Eui64,
    pub entity_model_id: u64,
    pub entity_capabilities: u32,
    pub talker_stream_sources: u16,
    pub talker_capabilities: u16,
    pub listener_stream_sinks: u16,
    pub listener_capabilities: u16,
    pub controller_capabilities: u32,
    pub available_index: u32,
    pub association_id: u64,
    pub entity_name: String,
    pub firmware_version: String,
    pub serial_number: String,
    pub configurations_count: u16,
    pub current_configuration: u16,
}

/// CONFIGURATION descriptor (§7.2.2): the per-type descriptor counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigurationDesc {
    pub object_name: String,
    /// (descriptor_type, count) pairs.
    pub descriptor_counts: Vec<(u16, u16)>,
}

/// AUDIO_UNIT descriptor (§7.2.3), reduced to what the listener needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioUnitDesc {
    pub object_name: String,
    pub clock_domain_index: u16,
    pub number_of_stream_input_ports: u16,
    pub number_of_stream_output_ports: u16,
    pub current_sampling_rate: u32,
    /// Pull-field-decoded rates in Hz.
    pub sampling_rates: Vec<u32>,
}

/// STREAM_INPUT / STREAM_OUTPUT descriptor (§7.2.6).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamDesc {
    pub object_name: String,
    pub clock_domain_index: u16,
    pub stream_flags: u16,
    pub current_format: StreamFormat,
    pub formats: Vec<StreamFormat>,
    pub avb_interface_index: u16,
    pub buffer_length: u32,
}

/// AVB_INTERFACE descriptor (§7.2.8).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AvbInterfaceDesc {
    pub object_name: String,
    pub mac_address: MacAddr,
    pub interface_flags: u16,
    pub clock_identity: Eui64,
    pub priority1: u8,
    pub clock_class: u8,
    pub offset_scaled_log_variance: u16,
    pub clock_accuracy: u8,
    pub priority2: u8,
    pub domain_number: u8,
    pub log_sync_interval: i8,
    pub log_announce_interval: i8,
    pub log_pdelay_interval: i8,
    pub port_number: u16,
}

/// CLOCK_SOURCE descriptor (§7.2.9).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClockSourceDesc {
    pub object_name: String,
    pub clock_source_flags: u16,
    pub clock_source_type: u16,
    pub clock_source_identifier: u64,
    pub clock_source_location_type: u16,
    pub clock_source_location_index: u16,
}

/// CLOCK_DOMAIN descriptor (§7.2.32).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClockDomainDesc {
    pub object_name: String,
    pub clock_source_index: u16,
    /// CLOCK_SOURCE indices this domain may use.
    pub clock_sources: Vec<u16>,
}

/// STREAM_PORT_INPUT / OUTPUT descriptor (§7.2.13).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamPortDesc {
    pub clock_domain_index: u16,
    pub port_flags: u16,
    pub number_of_clusters: u16,
    pub base_cluster: u16,
    pub number_of_maps: u16,
    pub base_map: u16,
}

/// AUDIO_CLUSTER descriptor (§7.2.16).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioClusterDesc {
    pub object_name: String,
    pub channel_count: u16,
    pub format: u8,
}

/// One AUDIO_MAP mapping (§7.2.19): stream channel k -> cluster/cluster-channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioMapping {
    pub stream_index: u16,
    pub stream_channel: u16,
    pub cluster_offset: u16,
    pub cluster_channel: u16,
}

/// AUDIO_MAP descriptor (§7.2.19).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioMapDesc {
    pub mappings: Vec<AudioMapping>,
}

/// A descriptor we decode, or raw bytes for a type we don't (§9.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Descriptor {
    Entity(EntityDesc),
    Configuration(ConfigurationDesc),
    AudioUnit(AudioUnitDesc),
    StreamInput(StreamDesc),
    StreamOutput(StreamDesc),
    AvbInterface(AvbInterfaceDesc),
    ClockSource(ClockSourceDesc),
    ClockDomain(ClockDomainDesc),
    StreamPortInput(StreamPortDesc),
    StreamPortOutput(StreamPortDesc),
    AudioCluster(AudioClusterDesc),
    AudioMap(AudioMapDesc),
    /// Any other type (LOCALE, STRINGS, CONTROL, JACK_*, unknown): raw bytes.
    Raw {
        descriptor_type: u16,
        bytes: Vec<u8>,
    },
}

impl Descriptor {
    pub fn descriptor_type(&self) -> u16 {
        match self {
            Descriptor::Entity(_) => desc::ENTITY,
            Descriptor::Configuration(_) => desc::CONFIGURATION,
            Descriptor::AudioUnit(_) => desc::AUDIO_UNIT,
            Descriptor::StreamInput(_) => desc::STREAM_INPUT,
            Descriptor::StreamOutput(_) => desc::STREAM_OUTPUT,
            Descriptor::AvbInterface(_) => desc::AVB_INTERFACE,
            Descriptor::ClockSource(_) => desc::CLOCK_SOURCE,
            Descriptor::ClockDomain(_) => desc::CLOCK_DOMAIN,
            Descriptor::StreamPortInput(_) => desc::STREAM_PORT_INPUT,
            Descriptor::StreamPortOutput(_) => desc::STREAM_PORT_OUTPUT,
            Descriptor::AudioCluster(_) => desc::AUDIO_CLUSTER,
            Descriptor::AudioMap(_) => desc::AUDIO_MAP,
            Descriptor::Raw {
                descriptor_type, ..
            } => *descriptor_type,
        }
    }
}

/// A talker's whole AEM for one configuration (what the controller builds and
/// `derive` consumes). Indexed by `(descriptor_type, descriptor_index)`.
#[derive(Clone, Debug, Default)]
pub struct TalkerAem {
    pub entity: EntityDesc,
    pub configuration_index: u16,
    pub configuration: ConfigurationDesc,
    pub audio_units: Vec<AudioUnitDesc>,
    /// STREAM_OUTPUT descriptors, in descriptor-index order.
    pub stream_outputs: Vec<StreamDesc>,
    pub stream_inputs: Vec<StreamDesc>,
    pub avb_interfaces: Vec<AvbInterfaceDesc>,
    pub clock_sources: Vec<ClockSourceDesc>,
    pub clock_domains: Vec<ClockDomainDesc>,
    pub stream_port_outputs: Vec<StreamPortDesc>,
    pub audio_clusters: Vec<AudioClusterDesc>,
    pub audio_maps: Vec<AudioMapDesc>,
    pub talker_mac: MacAddr,
    /// Quirks recorded during enumeration (§9.2 "limit exceeded").
    pub quirks: Vec<String>,
}
