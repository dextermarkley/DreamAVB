// SPDX-License-Identifier: MIT OR Apache-2.0

//! AEM descriptor codecs (IEEE 1722.1-2021 §7.2). Decode is used by the
//! controller to enumerate the 16R; encode is used by our listener entity's
//! READ_DESCRIPTOR responder (design §9.2, §9.3).
//!
//! Each descriptor begins with `descriptor_type(16) | descriptor_index(16)`.
//! Fixed-length name fields are 64-byte NUL-padded UTF-8 (`object_name` /
//! `entity_name`). Offsets follow the §7.2 tables; the ones the listener
//! needs are decoded, the rest are kept raw by the caller.

use avb_avtp::stream_format::StreamFormat;
use avb_core::{Eui64, MacAddr};

use super::model::*;
use crate::wire::{Rd, Wr};

/// Fixed name-field length in an AEM descriptor (§7.2.1).
pub const NAME_LEN: usize = 64;

fn read_name(r: &mut Rd) -> Option<String> {
    let b = r.take(NAME_LEN)?;
    let end = b.iter().position(|&c| c == 0).unwrap_or(NAME_LEN);
    Some(String::from_utf8_lossy(b.get(..end).unwrap_or(b)).into_owned())
}

fn write_name(w: &mut Wr, s: &str) {
    let mut buf = [0u8; NAME_LEN];
    let bytes = s.as_bytes();
    let n = bytes.len().min(NAME_LEN);
    if let (Some(dst), Some(src)) = (buf.get_mut(..n), bytes.get(..n)) {
        dst.copy_from_slice(src);
    }
    w.put(&buf);
}

/// Decode a 32-bit `sampling_rate` field: top 3 bits are the pull field, low
/// 29 bits are the base frequency in Hz (§7.3.1). Pull 0 is 1:1 (the common
/// case); other pulls are rare and preserved as the raw base frequency.
fn decode_sampling_rate(raw: u32) -> u32 {
    raw & 0x1FFF_FFFF
}

fn encode_sampling_rate(hz: u32) -> u32 {
    hz & 0x1FFF_FFFF
}

// ---- ENTITY ----

pub fn encode_entity(d: &EntityDesc) -> Vec<u8> {
    let mut w = Wr::new();
    w.u16(crate::constants::desc::ENTITY);
    w.u16(0); // descriptor_index
    w.u64(d.entity_id.to_u64());
    w.u64(d.entity_model_id);
    w.u32(d.entity_capabilities);
    w.u16(d.talker_stream_sources);
    w.u16(d.talker_capabilities);
    w.u16(d.listener_stream_sinks);
    w.u16(d.listener_capabilities);
    w.u32(d.controller_capabilities);
    w.u32(d.available_index);
    w.u64(d.association_id);
    write_name(&mut w, &d.entity_name);
    w.u16(0); // vendor_name_string
    w.u16(1); // model_name_string
    write_name(&mut w, &d.firmware_version);
    write_name(&mut w, ""); // group_name
    write_name(&mut w, &d.serial_number);
    w.u16(d.configurations_count);
    w.u16(d.current_configuration);
    w.into_vec()
}

pub fn decode_entity(pdu: &[u8]) -> Option<EntityDesc> {
    let mut r = Rd::new(pdu);
    r.skip(4)?; // type + index
    let entity_id = Eui64::from_u64(r.u64()?);
    let entity_model_id = r.u64()?;
    let entity_capabilities = r.u32()?;
    let talker_stream_sources = r.u16()?;
    let talker_capabilities = r.u16()?;
    let listener_stream_sinks = r.u16()?;
    let listener_capabilities = r.u16()?;
    let controller_capabilities = r.u32()?;
    let available_index = r.u32()?;
    let association_id = r.u64()?;
    let entity_name = read_name(&mut r)?;
    r.skip(2 + 2)?; // vendor/model name string refs
    let firmware_version = read_name(&mut r)?;
    let _group_name = read_name(&mut r)?;
    let serial_number = read_name(&mut r)?;
    let configurations_count = r.u16()?;
    let current_configuration = r.u16()?;
    Some(EntityDesc {
        entity_id,
        entity_model_id,
        entity_capabilities,
        talker_stream_sources,
        talker_capabilities,
        listener_stream_sinks,
        listener_capabilities,
        controller_capabilities,
        available_index,
        association_id,
        entity_name,
        firmware_version,
        serial_number,
        configurations_count,
        current_configuration,
    })
}

// ---- CONFIGURATION ----

pub fn encode_configuration(d: &ConfigurationDesc) -> Vec<u8> {
    let mut w = Wr::new();
    w.u16(crate::constants::desc::CONFIGURATION);
    w.u16(0);
    write_name(&mut w, &d.object_name);
    w.u16(0); // localized_description
    let counts = d.descriptor_counts.len().min(MAX_DESCRIPTORS_PER_TYPE) as u16;
    w.u16(counts);
    // descriptor_counts_offset = 2 (counts_count) + 2 (offset) from here = 4.
    w.u16(4);
    for &(ty, n) in d.descriptor_counts.iter().take(counts as usize) {
        w.u16(ty);
        w.u16(n);
    }
    w.into_vec()
}

pub fn decode_configuration(pdu: &[u8]) -> Option<ConfigurationDesc> {
    let mut r = Rd::new(pdu);
    r.skip(4)?;
    let object_name = read_name(&mut r)?;
    r.skip(2)?; // localized_description
    let counts_count = r.u16()?;
    r.skip(2)?; // descriptor_counts_offset (we read inline)
    let mut descriptor_counts = Vec::new();
    for _ in 0..counts_count.min(MAX_DESCRIPTORS_PER_TYPE as u16) {
        let ty = r.u16()?;
        let n = r.u16()?;
        descriptor_counts.push((ty, n));
    }
    Some(ConfigurationDesc {
        object_name,
        descriptor_counts,
    })
}

// ---- AUDIO_UNIT (reduced) ----

pub fn encode_audio_unit(d: &AudioUnitDesc) -> Vec<u8> {
    let mut w = Wr::new();
    w.u16(crate::constants::desc::AUDIO_UNIT);
    w.u16(0);
    write_name(&mut w, &d.object_name);
    w.u16(0); // localized_description
    w.u16(d.clock_domain_index);
    w.u16(d.number_of_stream_input_ports);
    w.u16(0); // base_stream_input_port
    w.u16(d.number_of_stream_output_ports);
    w.u16(0); // base_stream_output_port
              // ... other port counts omitted (0); then current/rates.
    w.u32(encode_sampling_rate(d.current_sampling_rate));
    let n = d.sampling_rates.len().min(MAX_SAMPLING_RATES) as u16;
    w.u16(n);
    w.u16(0); // sampling_rates_offset placeholder
    for &hz in d.sampling_rates.iter().take(n as usize) {
        w.u32(encode_sampling_rate(hz));
    }
    w.into_vec()
}

pub fn decode_audio_unit(pdu: &[u8]) -> Option<AudioUnitDesc> {
    let mut r = Rd::new(pdu);
    r.skip(4)?;
    let object_name = read_name(&mut r)?;
    r.skip(2)?; // localized_description
    let clock_domain_index = r.u16()?;
    let number_of_stream_input_ports = r.u16()?;
    r.skip(2)?; // base_stream_input_port
    let number_of_stream_output_ports = r.u16()?;
    r.skip(2)?; // base_stream_output_port
    let current_sampling_rate = decode_sampling_rate(r.u32()?);
    let n = r.u16()?;
    r.skip(2)?; // sampling_rates_offset
    let mut sampling_rates = Vec::new();
    for _ in 0..n.min(MAX_SAMPLING_RATES as u16) {
        sampling_rates.push(decode_sampling_rate(r.u32()?));
    }
    Some(AudioUnitDesc {
        object_name,
        clock_domain_index,
        number_of_stream_input_ports,
        number_of_stream_output_ports,
        current_sampling_rate,
        sampling_rates,
    })
}

// ---- STREAM_INPUT / STREAM_OUTPUT ----

pub fn encode_stream(d: &StreamDesc, is_input: bool) -> Vec<u8> {
    let mut w = Wr::new();
    w.u16(if is_input {
        crate::constants::desc::STREAM_INPUT
    } else {
        crate::constants::desc::STREAM_OUTPUT
    });
    w.u16(0);
    write_name(&mut w, &d.object_name);
    w.u16(0); // localized_description
    w.u16(d.clock_domain_index);
    w.u16(d.stream_flags);
    w.u64(d.current_format.0);
    w.u16(0); // formats_offset placeholder
    let n = d.formats.len().min(MAX_FORMATS) as u16;
    w.u16(n);
    w.u64(0); // backup_talker_entity_id_0
    w.u16(0); // backup_talker_unique_id_0
    w.u16(d.avb_interface_index);
    w.u32(d.buffer_length);
    for f in d.formats.iter().take(n as usize) {
        w.u64(f.0);
    }
    w.into_vec()
}

pub fn decode_stream(pdu: &[u8]) -> Option<StreamDesc> {
    let mut r = Rd::new(pdu);
    r.skip(4)?;
    let object_name = read_name(&mut r)?;
    r.skip(2)?; // localized_description
    let clock_domain_index = r.u16()?;
    let stream_flags = r.u16()?;
    let current_format = StreamFormat(r.u64()?);
    r.skip(2)?; // formats_offset
    let n = r.u16()?;
    r.skip(8 + 2)?; // backup_talker_entity_id_0 + backup_talker_unique_id_0
    let avb_interface_index = r.u16()?;
    let buffer_length = r.u32()?;
    let mut formats = Vec::new();
    for _ in 0..n.min(MAX_FORMATS as u16) {
        formats.push(StreamFormat(r.u64()?));
    }
    Some(StreamDesc {
        object_name,
        clock_domain_index,
        stream_flags,
        current_format,
        formats,
        avb_interface_index,
        buffer_length,
    })
}

// ---- AVB_INTERFACE ----

pub fn encode_avb_interface(d: &AvbInterfaceDesc) -> Vec<u8> {
    let mut w = Wr::new();
    w.u16(crate::constants::desc::AVB_INTERFACE);
    w.u16(0);
    write_name(&mut w, &d.object_name);
    w.u16(0); // localized_description
    w.put(&d.mac_address.0);
    w.u16(d.interface_flags);
    w.u64(d.clock_identity.to_u64());
    w.u8(d.priority1);
    w.u8(d.clock_class);
    w.u16(d.offset_scaled_log_variance);
    w.u8(d.clock_accuracy);
    w.u8(d.priority2);
    w.u8(d.domain_number);
    w.u8(d.log_sync_interval as u8);
    w.u8(d.log_announce_interval as u8);
    w.u8(d.log_pdelay_interval as u8);
    w.u16(d.port_number);
    w.into_vec()
}

pub fn decode_avb_interface(pdu: &[u8]) -> Option<AvbInterfaceDesc> {
    let mut r = Rd::new(pdu);
    r.skip(4)?;
    let object_name = read_name(&mut r)?;
    r.skip(2)?;
    let mac_address = MacAddr(r.arr()?);
    let interface_flags = r.u16()?;
    let clock_identity = Eui64::from_u64(r.u64()?);
    let priority1 = r.u8()?;
    let clock_class = r.u8()?;
    let offset_scaled_log_variance = r.u16()?;
    let clock_accuracy = r.u8()?;
    let priority2 = r.u8()?;
    let domain_number = r.u8()?;
    let log_sync_interval = r.u8()? as i8;
    let log_announce_interval = r.u8()? as i8;
    let log_pdelay_interval = r.u8()? as i8;
    let port_number = r.u16()?;
    Some(AvbInterfaceDesc {
        object_name,
        mac_address,
        interface_flags,
        clock_identity,
        priority1,
        clock_class,
        offset_scaled_log_variance,
        clock_accuracy,
        priority2,
        domain_number,
        log_sync_interval,
        log_announce_interval,
        log_pdelay_interval,
        port_number,
    })
}

// ---- CLOCK_SOURCE ----

pub fn encode_clock_source(d: &ClockSourceDesc) -> Vec<u8> {
    let mut w = Wr::new();
    w.u16(crate::constants::desc::CLOCK_SOURCE);
    w.u16(0);
    write_name(&mut w, &d.object_name);
    w.u16(0);
    w.u16(d.clock_source_flags);
    w.u16(d.clock_source_type);
    w.u64(d.clock_source_identifier);
    w.u16(d.clock_source_location_type);
    w.u16(d.clock_source_location_index);
    w.into_vec()
}

pub fn decode_clock_source(pdu: &[u8]) -> Option<ClockSourceDesc> {
    let mut r = Rd::new(pdu);
    r.skip(4)?;
    let object_name = read_name(&mut r)?;
    r.skip(2)?;
    let clock_source_flags = r.u16()?;
    let clock_source_type = r.u16()?;
    let clock_source_identifier = r.u64()?;
    let clock_source_location_type = r.u16()?;
    let clock_source_location_index = r.u16()?;
    Some(ClockSourceDesc {
        object_name,
        clock_source_flags,
        clock_source_type,
        clock_source_identifier,
        clock_source_location_type,
        clock_source_location_index,
    })
}

// ---- CLOCK_DOMAIN ----

pub fn encode_clock_domain(d: &ClockDomainDesc) -> Vec<u8> {
    let mut w = Wr::new();
    w.u16(crate::constants::desc::CLOCK_DOMAIN);
    w.u16(0);
    write_name(&mut w, &d.object_name);
    w.u16(0);
    w.u16(d.clock_source_index);
    w.u16(8); // clock_sources_offset (name 64 + loc 2 + index 2 + count 2 + off 2)
    let n = d.clock_sources.len() as u16;
    w.u16(n);
    for &s in &d.clock_sources {
        w.u16(s);
    }
    w.into_vec()
}

pub fn decode_clock_domain(pdu: &[u8]) -> Option<ClockDomainDesc> {
    let mut r = Rd::new(pdu);
    r.skip(4)?;
    let object_name = read_name(&mut r)?;
    r.skip(2)?;
    let clock_source_index = r.u16()?;
    r.skip(2)?; // clock_sources_offset
    let n = r.u16()?;
    let mut clock_sources = Vec::new();
    for _ in 0..n.min(MAX_DESCRIPTORS_PER_TYPE as u16) {
        clock_sources.push(r.u16()?);
    }
    Some(ClockDomainDesc {
        object_name,
        clock_source_index,
        clock_sources,
    })
}

// ---- STREAM_PORT_INPUT / OUTPUT ----

pub fn encode_stream_port(d: &StreamPortDesc, is_input: bool) -> Vec<u8> {
    let mut w = Wr::new();
    w.u16(if is_input {
        crate::constants::desc::STREAM_PORT_INPUT
    } else {
        crate::constants::desc::STREAM_PORT_OUTPUT
    });
    w.u16(0);
    w.u16(d.clock_domain_index);
    w.u16(d.port_flags);
    w.u16(d.number_of_clusters);
    w.u16(d.base_cluster);
    w.u16(d.number_of_maps);
    w.u16(d.base_map);
    w.into_vec()
}

pub fn decode_stream_port(pdu: &[u8]) -> Option<StreamPortDesc> {
    let mut r = Rd::new(pdu);
    r.skip(4)?;
    let clock_domain_index = r.u16()?;
    let port_flags = r.u16()?;
    let number_of_clusters = r.u16()?;
    let base_cluster = r.u16()?;
    let number_of_maps = r.u16()?;
    let base_map = r.u16()?;
    Some(StreamPortDesc {
        clock_domain_index,
        port_flags,
        number_of_clusters,
        base_cluster,
        number_of_maps,
        base_map,
    })
}

// ---- AUDIO_CLUSTER ----

pub fn encode_audio_cluster(d: &AudioClusterDesc) -> Vec<u8> {
    let mut w = Wr::new();
    w.u16(crate::constants::desc::AUDIO_CLUSTER);
    w.u16(0);
    write_name(&mut w, &d.object_name);
    w.u16(0); // localized_description
    w.u16(0); // signal_type
    w.u16(0); // signal_index
    w.u16(0); // signal_output
    w.u32(0); // path_latency
    w.u32(0); // block_latency
    w.u16(d.channel_count);
    w.u8(d.format);
    w.into_vec()
}

pub fn decode_audio_cluster(pdu: &[u8]) -> Option<AudioClusterDesc> {
    let mut r = Rd::new(pdu);
    r.skip(4)?;
    let object_name = read_name(&mut r)?;
    r.skip(2 + 2 + 2 + 2 + 4 + 4)?; // loc_desc, signal_type/index/output, path/block latency
    let channel_count = r.u16()?;
    let format = r.u8()?;
    Some(AudioClusterDesc {
        object_name,
        channel_count,
        format,
    })
}

// ---- AUDIO_MAP ----

pub fn encode_audio_map(d: &AudioMapDesc) -> Vec<u8> {
    let mut w = Wr::new();
    w.u16(crate::constants::desc::AUDIO_MAP);
    w.u16(0);
    w.u16(8); // mappings_offset
    let n = d.mappings.len().min(MAX_MAPPINGS) as u16;
    w.u16(n);
    for m in d.mappings.iter().take(n as usize) {
        w.u16(m.stream_index);
        w.u16(m.stream_channel);
        w.u16(m.cluster_offset);
        w.u16(m.cluster_channel);
    }
    w.into_vec()
}

pub fn decode_audio_map(pdu: &[u8]) -> Option<AudioMapDesc> {
    let mut r = Rd::new(pdu);
    r.skip(4)?;
    r.skip(2)?; // mappings_offset
    let n = r.u16()?;
    let mut mappings = Vec::new();
    for _ in 0..n.min(MAX_MAPPINGS as u16) {
        let stream_index = r.u16()?;
        let stream_channel = r.u16()?;
        let cluster_offset = r.u16()?;
        let cluster_channel = r.u16()?;
        mappings.push(AudioMapping {
            stream_index,
            stream_channel,
            cluster_offset,
            cluster_channel,
        });
    }
    Some(AudioMapDesc { mappings })
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::constants::{clock_source, stream_flags};

    #[test]
    fn entity_round_trip() {
        let d = EntityDesc {
            entity_id: Eui64::from_u64(0x644b_f0ff_fe12_73b8),
            entity_model_id: 0x0241_5601_dead_beef,
            entity_capabilities: 0x0000_0408,
            talker_stream_sources: 0,
            talker_capabilities: 0,
            listener_stream_sinks: 3,
            listener_capabilities: 0x4001,
            controller_capabilities: 1,
            available_index: 7,
            association_id: 0,
            entity_name: "AVB-Audio Mac".into(),
            firmware_version: "phase1a-abc1234".into(),
            serial_number: "644bf01273b8".into(),
            configurations_count: 1,
            current_configuration: 0,
        };
        assert_eq!(decode_entity(&encode_entity(&d)), Some(d));
    }

    #[test]
    fn configuration_round_trip() {
        let d = ConfigurationDesc {
            object_name: "Config".into(),
            descriptor_counts: vec![(0x0002, 1), (0x0005, 3), (0x0006, 3)],
        };
        assert_eq!(decode_configuration(&encode_configuration(&d)), Some(d));
    }

    #[test]
    fn audio_unit_round_trip() {
        let d = AudioUnitDesc {
            object_name: "Unit".into(),
            clock_domain_index: 0,
            number_of_stream_input_ports: 3,
            number_of_stream_output_ports: 0,
            current_sampling_rate: 48_000,
            sampling_rates: vec![44_100, 48_000, 96_000],
        };
        assert_eq!(decode_audio_unit(&encode_audio_unit(&d)), Some(d));
    }

    #[test]
    fn stream_round_trip() {
        let d = StreamDesc {
            object_name: "Out 1".into(),
            clock_domain_index: 0,
            stream_flags: stream_flags::CLASS_A | stream_flags::CLOCK_SYNC_SOURCE,
            current_format: StreamFormat(0x0205_0218_0200_6000),
            formats: vec![
                StreamFormat(0x0205_0220_0200_6000),
                StreamFormat(0x0205_0218_0200_6000),
            ],
            avb_interface_index: 0,
            buffer_length: 2_000_000,
        };
        let enc = encode_stream(&d, false);
        assert_eq!(enc.get(0..2), Some(&[0x00, 0x06][..])); // STREAM_OUTPUT
        assert_eq!(decode_stream(&enc), Some(d));
    }

    #[test]
    fn avb_interface_round_trip() {
        let d = AvbInterfaceDesc {
            object_name: "en".into(),
            mac_address: MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]),
            interface_flags: 0x0003,
            clock_identity: Eui64::from_u64(0x644b_f0ff_fe12_73b8),
            priority1: 255,
            clock_class: 248,
            offset_scaled_log_variance: 0x436A,
            clock_accuracy: 0xFE,
            priority2: 248,
            domain_number: 0,
            log_sync_interval: -3,
            log_announce_interval: 0,
            log_pdelay_interval: 0,
            port_number: 1,
        };
        assert_eq!(decode_avb_interface(&encode_avb_interface(&d)), Some(d));
    }

    #[test]
    fn clock_source_and_domain_round_trip() {
        let s = ClockSourceDesc {
            object_name: "Stream clock".into(),
            clock_source_flags: 0,
            clock_source_type: clock_source::INPUT_STREAM,
            clock_source_identifier: 0,
            clock_source_location_type: crate::constants::desc::STREAM_INPUT,
            clock_source_location_index: 2,
        };
        assert_eq!(decode_clock_source(&encode_clock_source(&s)), Some(s));
        let dom = ClockDomainDesc {
            object_name: "Domain".into(),
            clock_source_index: 1,
            clock_sources: vec![0, 1],
        };
        assert_eq!(decode_clock_domain(&encode_clock_domain(&dom)), Some(dom));
    }

    #[test]
    fn stream_port_cluster_map_round_trip() {
        let p = StreamPortDesc {
            clock_domain_index: 0,
            port_flags: 0,
            number_of_clusters: 4,
            base_cluster: 0,
            number_of_maps: 1,
            base_map: 0,
        };
        assert_eq!(decode_stream_port(&encode_stream_port(&p, true)), Some(p));
        let c = AudioClusterDesc {
            object_name: "In 1 ch 1".into(),
            channel_count: 1,
            format: 0x40,
        };
        assert_eq!(decode_audio_cluster(&encode_audio_cluster(&c)), Some(c));
        let m = AudioMapDesc {
            mappings: vec![
                AudioMapping {
                    stream_index: 0,
                    stream_channel: 0,
                    cluster_offset: 0,
                    cluster_channel: 0,
                },
                AudioMapping {
                    stream_index: 0,
                    stream_channel: 1,
                    cluster_offset: 1,
                    cluster_channel: 0,
                },
            ],
        };
        assert_eq!(decode_audio_map(&encode_audio_map(&m)), Some(m));
    }
}
