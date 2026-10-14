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
use crate::constants::NO_STRING;
use crate::wire::{Rd, Wr};

/// Fixed name-field length in an AEM descriptor (§7.2.1).
pub const NAME_LEN: usize = 64;

// ---- Hardware-anchored array offsets (§7.2) --------------------------------
//
// Every one of these is transcribed from a descriptor READ OFF A REAL PreSonus
// StudioLive 16R (see the `hw_16r` tests at the bottom of this file), not
// derived from our own encoder. They are absolute byte offsets from the first
// byte of the descriptor, which is how §7.2 defines every `*_offset` field.

/// `CONFIGURATION.descriptor_counts_offset` — the array follows the fixed part.
const CONFIGURATION_COUNTS_OFFSET: u16 = 74;

/// `AUDIO_UNIT.sampling_rates_offset`.
const AUDIO_UNIT_RATES_OFFSET: u16 = 144;

/// `STREAM_INPUT`/`STREAM_OUTPUT.formats_offset`.
const STREAM_FORMATS_OFFSET: u16 = 132;

/// `CLOCK_DOMAIN.clock_sources_offset`.
const CLOCK_DOMAIN_SOURCES_OFFSET: u16 = 76;

/// Bytes of port/count pairs between `base_stream_output_port` and
/// `current_sampling_rate` in an AUDIO_UNIT: 14 `(number_of_*, base_*)` pairs
/// (external in/out, internal in/out, controls, signal selectors, mixers,
/// matrices, splitters, combiners, demultiplexers, multiplexers, transcoders,
/// control blocks). Omitting these read the rate from byte 80 instead of 136,
/// which on the 16R is a run of zeros — so a healthy 48 kHz unit decoded as
/// "0 Hz" and the rate cross-check refused every real stream.
const AUDIO_UNIT_PORT_PAIRS_BYTES: usize = 56;

/// Bytes of backup/backed-up talker fields between `number_of_formats` and
/// `avb_interface_index` in a STREAM descriptor: three `(entity_id, unique_id)`
/// backup pairs plus one backed-up pair, 10 bytes each.
const STREAM_BACKUP_FIELDS_BYTES: usize = 40;

/// Read a 64-byte NUL-padded name out of a raw field, for the GET_NAME /
/// SET_NAME payloads which carry one outside any descriptor.
pub fn name_from_bytes(b: &[u8]) -> String {
    let b = b.get(..NAME_LEN).unwrap_or(b);
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(b.get(..end).unwrap_or(b)).into_owned()
}

/// The 64-byte NUL-padded encoding of a name, truncated to fit.
pub fn name_to_bytes(s: &str) -> [u8; NAME_LEN] {
    let mut buf = [0u8; NAME_LEN];
    let bytes = s.as_bytes();
    let n = bytes.len().min(NAME_LEN);
    if let (Some(dst), Some(src)) = (buf.get_mut(..n), bytes.get(..n)) {
        dst.copy_from_slice(src);
    }
    buf
}

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

/// Read `count` big-endian `u64`s starting at `offset` bytes from the START of
/// `pdu`, which is how §7.2 locates every variable-length descriptor array.
///
/// Descriptor offsets are absolute from the descriptor's first byte, NOT
/// relative to the field that holds them, and the array does NOT always begin
/// immediately after the fixed part. Reading these sequentially is the bug
/// class that made the 16R's `formats` list decode as all-zeros while every
/// round-trip unit test passed: our encoder wrote the array immediately after
/// the count, so encode and decode agreed with each other and not with the
/// hardware.
fn read_u64s_at(pdu: &[u8], offset: usize, count: usize) -> Vec<u64> {
    let mut out = Vec::new();
    for i in 0..count {
        let start = match offset.checked_add(i.saturating_mul(8)) {
            Some(s) => s,
            None => break,
        };
        let Some(end) = start.checked_add(8) else {
            break;
        };
        let Some(bytes) = pdu.get(start..end) else {
            break;
        };
        match <[u8; 8]>::try_from(bytes) {
            Ok(a) => out.push(u64::from_be_bytes(a)),
            Err(_) => break,
        }
    }
    out
}

/// Same as [`read_u64s_at`] for the `u32` arrays (`sampling_rates`).
fn read_u32s_at(pdu: &[u8], offset: usize, count: usize) -> Vec<u32> {
    let mut out = Vec::new();
    for i in 0..count {
        let start = match offset.checked_add(i.saturating_mul(4)) {
            Some(s) => s,
            None => break,
        };
        let Some(end) = start.checked_add(4) else {
            break;
        };
        let Some(bytes) = pdu.get(start..end) else {
            break;
        };
        match <[u8; 4]>::try_from(bytes) {
            Ok(a) => out.push(u32::from_be_bytes(a)),
            Err(_) => break,
        }
    }
    out
}

/// Same as [`read_u64s_at`] for the `(u16, u16)` pair arrays
/// (`CONFIGURATION.descriptor_counts`).
fn read_u16_pairs_at(pdu: &[u8], offset: usize, count: usize) -> Vec<(u16, u16)> {
    let mut out = Vec::new();
    for i in 0..count {
        let start = match offset.checked_add(i.saturating_mul(4)) {
            Some(s) => s,
            None => break,
        };
        let Some(end) = start.checked_add(4) else {
            break;
        };
        let Some(bytes) = pdu.get(start..end) else {
            break;
        };
        let Ok(a) = <[u8; 4]>::try_from(bytes) else {
            break;
        };
        let Some(hi) = a.get(..2).and_then(|s| <[u8; 2]>::try_from(s).ok()) else {
            break;
        };
        let Some(lo) = a.get(2..).and_then(|s| <[u8; 2]>::try_from(s).ok()) else {
            break;
        };
        out.push((u16::from_be_bytes(hi), u16::from_be_bytes(lo)));
    }
    out
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
    w.u16(NO_STRING); // vendor_name_string: no LOCALE/STRINGS advertised
    w.u16(NO_STRING); // model_name_string: no LOCALE/STRINGS advertised
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
    w.u16(NO_STRING); // localized_description: no LOCALE/STRINGS advertised
    let counts = d.descriptor_counts.len().min(MAX_DESCRIPTORS_PER_TYPE) as u16;
    w.u16(counts);
    // descriptor_counts_offset is ABSOLUTE from the descriptor's first byte
    // (§7.2.2). The array starts right after this field, at byte 74 — which is
    // exactly what the 16R writes. Writing a relative 4 here made our own
    // descriptors unreadable by any spec-conformant controller.
    w.u16(CONFIGURATION_COUNTS_OFFSET);
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
    let counts_offset = r.u16()?;
    let declared = usize::from(counts_count).min(MAX_DESCRIPTORS_PER_TYPE);
    // CONFIGURATION ends with descriptor_counts[], so complete pairs after the
    // advertised count cannot be a later field. The 16R firmware declares 10
    // pairs but sends 11; the trailing hardware-captured pair is
    // PTP_INSTANCE(0), which its unsolicited GET_DYNAMIC_INFO also references.
    // Consume complete trailing pairs for interoperability while keeping our
    // encoder strict and refusing any partial tail.
    let available = pdu
        .get(usize::from(counts_offset)..)
        .filter(|tail| tail.len() % 4 == 0)
        .map_or(declared, |tail| tail.len() / 4)
        .min(MAX_DESCRIPTORS_PER_TYPE);
    let descriptor_counts =
        read_u16_pairs_at(pdu, usize::from(counts_offset), declared.max(available));
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
    w.u16(NO_STRING); // localized_description: no LOCALE/STRINGS advertised
    w.u16(d.clock_domain_index);
    w.u16(d.number_of_stream_input_ports);
    w.u16(0); // base_stream_input_port
    w.u16(d.number_of_stream_output_ports);
    w.u16(0); // base_stream_output_port
              // The 14 remaining (number_of_*, base_*) port/count pairs. We
              // expose none of these objects, so they are all zero — but they
              // must be PRESENT, or current_sampling_rate lands 56 bytes early
              // and no conformant controller can read our rate.
    for _ in 0..AUDIO_UNIT_PORT_PAIRS_BYTES {
        w.u8(0);
    }
    w.u32(encode_sampling_rate(d.current_sampling_rate));
    let n = d.sampling_rates.len().min(MAX_SAMPLING_RATES) as u16;
    // Offset BEFORE count, and absolute from the descriptor start (§7.2.4).
    w.u16(AUDIO_UNIT_RATES_OFFSET);
    w.u16(n);
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
    r.skip(AUDIO_UNIT_PORT_PAIRS_BYTES)?; // the 14 other port/count pairs
    let current_sampling_rate = decode_sampling_rate(r.u32()?);
    let rates_offset = r.u16()?;
    let n = r.u16()?;
    // Read the array at its declared absolute offset rather than assuming it
    // follows the fixed part.
    let sampling_rates = read_u32s_at(
        pdu,
        usize::from(rates_offset),
        usize::from(n).min(MAX_SAMPLING_RATES),
    )
    .into_iter()
    .map(decode_sampling_rate)
    .collect();
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
    w.u16(NO_STRING); // localized_description: no LOCALE/STRINGS advertised
    w.u16(d.clock_domain_index);
    w.u16(d.stream_flags);
    w.u64(d.current_format.0);
    // formats_offset is ABSOLUTE from the descriptor start (§7.2.6); it was a
    // zero placeholder that was never patched, pointing a controller at our
    // descriptor_type field instead of the formats array.
    w.u16(STREAM_FORMATS_OFFSET);
    let n = d.formats.len().min(MAX_FORMATS) as u16;
    w.u16(n);
    // The three backup talker pairs and the backed-up pair. We support no
    // backup talker, so all zero — but they must be present or the formats
    // array lands 40 bytes early.
    for _ in 0..STREAM_BACKUP_FIELDS_BYTES {
        w.u8(0);
    }
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
    let formats_offset = r.u16()?;
    let n = r.u16()?;
    r.skip(STREAM_BACKUP_FIELDS_BYTES)?; // 3 backup talker pairs + backed-up
    let avb_interface_index = r.u16()?;
    let buffer_length = r.u32()?;
    let formats = read_u64s_at(
        pdu,
        usize::from(formats_offset),
        usize::from(n).min(MAX_FORMATS),
    )
    .into_iter()
    .map(StreamFormat)
    .collect();
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
    w.u16(NO_STRING); // localized_description: no LOCALE/STRINGS advertised
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
    w.u16(NO_STRING); // localized_description: no LOCALE/STRINGS advertised
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

// ---- CONTROL ----

/// Fixed-field length of a CONTROL descriptor, i.e. where `values` starts.
/// The 16R's own descriptor reports `values_offset = 104`, which is what
/// pins this constant to hardware rather than to arithmetic.
pub const CONTROL_VALUES_OFFSET: u16 = 104;

pub fn encode_control(d: &ControlDesc) -> Vec<u8> {
    let mut w = Wr::new();
    w.u16(crate::constants::desc::CONTROL);
    w.u16(0);
    write_name(&mut w, &d.object_name);
    w.u16(d.localized_description);
    w.u32(d.block_latency);
    w.u32(d.control_latency);
    w.u16(d.control_domain);
    w.u16(d.control_value_type);
    w.u64(d.control_type);
    w.u32(d.reset_time);
    // A real offset, not a placeholder: an unpatched 0 here is how our STREAM
    // encoder shipped a formats array no conformant controller could find.
    w.u16(CONTROL_VALUES_OFFSET);
    w.u16(d.number_of_values);
    w.u16(d.signal_type);
    w.u16(d.signal_index);
    w.u16(d.signal_output);
    let mut v = w.into_vec();
    debug_assert_eq!(v.len(), CONTROL_VALUES_OFFSET as usize);
    v.extend_from_slice(&d.values);
    v
}

pub fn decode_control(pdu: &[u8]) -> Option<ControlDesc> {
    let mut r = Rd::new(pdu);
    r.skip(4)?;
    let object_name = read_name(&mut r)?;
    let localized_description = r.u16()?;
    let block_latency = r.u32()?;
    let control_latency = r.u32()?;
    let control_domain = r.u16()?;
    let control_value_type = r.u16()?;
    let control_type = r.u64()?;
    let reset_time = r.u32()?;
    let values_offset = r.u16()?;
    let number_of_values = r.u16()?;
    let signal_type = r.u16()?;
    let signal_index = r.u16()?;
    let signal_output = r.u16()?;
    // Read `values` from the offset the DEVICE states, not from where our own
    // fixed fields happen to end. A device that lays the block out elsewhere
    // is conformant, and trusting our own arithmetic over its stated offset is
    // the mistake that made the 16R's AUDIO_UNIT sample rate decode as 0 Hz.
    let values = pdu.get(values_offset as usize..)?.to_vec();
    Some(ControlDesc {
        object_name,
        localized_description,
        block_latency,
        control_latency,
        control_domain,
        control_value_type,
        control_type,
        reset_time,
        number_of_values,
        signal_type,
        signal_index,
        signal_output,
        values,
    })
}

// ---- CLOCK_DOMAIN ----

pub fn encode_clock_domain(d: &ClockDomainDesc) -> Vec<u8> {
    let mut w = Wr::new();
    w.u16(crate::constants::desc::CLOCK_DOMAIN);
    w.u16(0);
    write_name(&mut w, &d.object_name);
    w.u16(NO_STRING); // localized_description: no LOCALE/STRINGS advertised
    w.u16(d.clock_source_index);
    w.u16(CLOCK_DOMAIN_SOURCES_OFFSET);
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
    let clock_sources_offset = r.u16()?;
    let n = r.u16()?;
    let mut sources = Rd::new(pdu.get(usize::from(clock_sources_offset)..)?);
    let mut clock_sources = Vec::new();
    for _ in 0..n.min(MAX_DESCRIPTORS_PER_TYPE as u16) {
        clock_sources.push(sources.u16()?);
    }
    Some(ClockDomainDesc {
        object_name,
        clock_source_index,
        clock_sources,
    })
}

// ---- TIMING / PTP_INSTANCE / PTP_PORT -------------------------------------

const TIMING_INSTANCES_OFFSET: u16 = 76;

pub fn encode_timing(d: &TimingDesc) -> Vec<u8> {
    let mut w = Wr::new();
    w.u16(crate::constants::desc::TIMING);
    w.u16(0);
    write_name(&mut w, &d.object_name);
    w.u16(NO_STRING);
    w.u16(d.algorithm);
    w.u16(TIMING_INSTANCES_OFFSET);
    let n = d.ptp_instances.len().min(MAX_DESCRIPTORS_PER_TYPE) as u16;
    w.u16(n);
    for &index in d.ptp_instances.iter().take(usize::from(n)) {
        w.u16(index);
    }
    w.into_vec()
}

pub fn decode_timing(pdu: &[u8]) -> Option<TimingDesc> {
    let mut r = Rd::new(pdu);
    r.skip(4)?;
    let object_name = read_name(&mut r)?;
    r.skip(2)?;
    let algorithm = r.u16()?;
    let offset = r.u16()?;
    let n = r.u16()?;
    let mut instances = Rd::new(pdu.get(usize::from(offset)..)?);
    let mut ptp_instances = Vec::new();
    for _ in 0..n.min(MAX_DESCRIPTORS_PER_TYPE as u16) {
        ptp_instances.push(instances.u16()?);
    }
    Some(TimingDesc {
        object_name,
        algorithm,
        ptp_instances,
    })
}

pub fn encode_ptp_instance(d: &PtpInstanceDesc) -> Vec<u8> {
    let mut w = Wr::new();
    w.u16(crate::constants::desc::PTP_INSTANCE);
    w.u16(0);
    write_name(&mut w, &d.object_name);
    w.u16(NO_STRING);
    w.u64(d.clock_identity.to_u64());
    w.u32(d.flags);
    w.u16(d.number_of_controls);
    w.u16(d.base_control);
    w.u16(d.number_of_ptp_ports);
    w.u16(d.base_ptp_port);
    w.into_vec()
}

pub fn decode_ptp_instance(pdu: &[u8]) -> Option<PtpInstanceDesc> {
    let mut r = Rd::new(pdu);
    r.skip(4)?;
    Some(PtpInstanceDesc {
        object_name: read_name(&mut r)?,
        clock_identity: Eui64::from_u64({
            r.skip(2)?;
            r.u64()?
        }),
        flags: r.u32()?,
        number_of_controls: r.u16()?,
        base_control: r.u16()?,
        number_of_ptp_ports: r.u16()?,
        base_ptp_port: r.u16()?,
    })
}

pub fn encode_ptp_port(d: &PtpPortDesc) -> Vec<u8> {
    let mut w = Wr::new();
    w.u16(crate::constants::desc::PTP_PORT);
    w.u16(0);
    write_name(&mut w, &d.object_name);
    w.u16(NO_STRING);
    w.u16(d.port_number);
    w.u16(d.port_type);
    w.u32(d.flags);
    w.u16(d.avb_interface_index);
    w.put(&d.profile_identifier.0);
    w.into_vec()
}

pub fn decode_ptp_port(pdu: &[u8]) -> Option<PtpPortDesc> {
    let mut r = Rd::new(pdu);
    r.skip(4)?;
    let object_name = read_name(&mut r)?;
    r.skip(2)?;
    Some(PtpPortDesc {
        object_name,
        port_number: r.u16()?,
        port_type: r.u16()?,
        flags: r.u32()?,
        avb_interface_index: r.u16()?,
        profile_identifier: MacAddr(r.arr()?),
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
    w.u16(NO_STRING); // localized_description: no LOCALE/STRINGS advertised
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

/// Decode tests pinned to descriptor bytes captured from a REAL PreSonus
/// StudioLive 16R (entity `0x000a92fffe033ba9`, firmware as shipped, read over
/// AECP by `avb-daemon aem-dump` on the I210).
///
/// These exist because every codec in this file was previously "tested" only
/// by `decode(encode(x)) == x`. That is a tautology: our encoder and decoder
/// shared the same wrong field offsets, so the round trip passed while the
/// 16R's own descriptors decoded to garbage — `formats` as all-zeros, a
/// healthy 48 kHz AUDIO_UNIT as "0 Hz". A hex string lifted off the wire
/// cannot agree with a bug in our own layout.
#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod hw_16r {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(s.len() / 2);
        let mut i = 0;
        while i + 1 < b.len() {
            let hi = (b[i] as char).to_digit(16).expect("hex");
            let lo = (b[i + 1] as char).to_digit(16).expect("hex");
            out.push((hi * 16 + lo) as u8);
            i += 2;
        }
        out
    }

    /// CONTROL 0 — "Identify LED Control", the Milan IDENTIFY control. This is
    /// the descriptor the 16R asks US for and we used to refuse, which stalled
    /// its enumeration of us.
    const CONTROL_0_IDENTIFY: &str = "001a00004964656e74696679204c454420436f6e74726f6c0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ffff00000000000000000000000190e0f000000000010000000000680001ffff0000000000ffff00000000ffff";

    /// CONFIGURATION 0 — "Configuration 0", 10 descriptor-count entries.
    const CONFIG_0: &str = "00010000436f6e66696775726174696f6e203000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ffff000a004a001a000100020001000500090006000900090001000a000b000c0001001a0001002400010026000100270001";

    /// AUDIO_UNIT 0 — "Audio Unit 0", 48 kHz current, {44.1 k, 48 k}.
    const AUDIO_UNIT_0: &str = "00020000417564696f20556e6974203000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ffff0000000100000001000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000bb80009000020000ac440000bb80";

    /// STREAM_INPUT 0 — "Return CRF". The 16R's media-clock sink, NOT audio.
    const STREAM_INPUT_0_CRF: &str = "0005000052657475726e20435246000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ffff00000003041060010000bb8000840002000000000000000000000000000000000000000000000000000000000000000000000000000000000000002070b0041060010000bb80041093010000ac44";

    /// STREAM_INPUT 1 — "Return 1-8", AAF 48 kHz INT32 8 ch 6 spf, 10 formats.
    const STREAM_INPUT_1_AAF: &str = "0005000152657475726e20312d38000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ffff0000000302050220020060000084000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000002070b00204022000406000020402200080600002040220010060000204022001806000020402200200600002050220004060000205022000806000020502200100600002050220018060000205022002006000";

    /// STREAM_OUTPUT 1 — "Send 1-8", the source our listener direction binds.
    const STREAM_OUTPUT_1_AAF: &str = "0006000153656e6420312d380000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ffff000000020205022002006000008400020000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002040220020060000205022002006000";

    /// TIMING 0, PTP_INSTANCE 0 and referenced PTP_PORT 0 from the live 16R.
    const TIMING_0: &str = "0026000054696d696e6700000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ffff0000004c00010000";
    const PTP_INSTANCE_0: &str = "0027000050545020496e7374616e636500000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ffff000a92fffe033ba9000000010000000000010000";
    const PTP_PORT_0: &str = "0028000050545020506f72740000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ffff00000000000000000000000000000000";

    #[test]
    fn timing_ptp_chain_reencodes_the_16r_byte_for_byte() {
        let timing_raw = hex(TIMING_0);
        let timing = decode_timing(&timing_raw).expect("TIMING");
        assert_eq!(timing.algorithm, 0);
        assert_eq!(timing.ptp_instances, vec![0]);
        assert_eq!(encode_timing(&timing), timing_raw);

        let instance_raw = hex(PTP_INSTANCE_0);
        let instance = decode_ptp_instance(&instance_raw).expect("PTP_INSTANCE");
        assert_eq!(instance.clock_identity.to_u64(), 0x000a_92ff_fe03_3ba9);
        assert_eq!(instance.flags, 1, "GRANDMASTER_CAPABLE");
        assert_eq!(instance.number_of_ptp_ports, 1);
        assert_eq!(instance.base_ptp_port, 0);
        assert_eq!(encode_ptp_instance(&instance), instance_raw);

        let port_raw = hex(PTP_PORT_0);
        let port = decode_ptp_port(&port_raw).expect("PTP_PORT");
        assert_eq!(port.port_type, 0, "P2P link-layer");
        assert_eq!(port.avb_interface_index, 0);
        assert_eq!(encode_ptp_port(&port), port_raw);
    }

    #[test]
    fn configuration_0_off_the_wire() {
        let d = decode_configuration(&hex(CONFIG_0)).expect("decode");
        assert_eq!(d.object_name, "Configuration 0");
        // 9 STREAM_INPUTs and 9 STREAM_OUTPUTs: one CRF plus eight audio.
        assert_eq!(
            d.descriptor_counts
                .iter()
                .copied()
                .find(|&(t, _)| t == crate::constants::desc::STREAM_INPUT),
            Some((crate::constants::desc::STREAM_INPUT, 9))
        );
        assert_eq!(
            d.descriptor_counts
                .iter()
                .copied()
                .find(|&(t, _)| t == crate::constants::desc::STREAM_OUTPUT),
            Some((crate::constants::desc::STREAM_OUTPUT, 9))
        );
        assert_eq!(d.descriptor_counts.len(), 11);
        assert_eq!(
            d.descriptor_counts.last().copied(),
            Some((0x0027, 1)),
            "the 16R underreports 10 pairs but physically appends PTP_INSTANCE",
        );
    }

    /// The bug that made `duplex` refuse to stream: the rate was read from
    /// byte 80 (a run of zeros on this device) instead of byte 136, so the
    /// AUDIO_UNIT-vs-stream rate cross-check compared 48000 against 0.
    #[test]
    fn audio_unit_0_current_rate_is_48k_not_zero() {
        let d = decode_audio_unit(&hex(AUDIO_UNIT_0)).expect("decode");
        assert_eq!(d.object_name, "Audio Unit 0");
        assert_eq!(d.current_sampling_rate, 48_000);
        assert_ne!(d.current_sampling_rate, 0);
        assert_eq!(d.sampling_rates, vec![44_100, 48_000]);
        assert_eq!(d.number_of_stream_input_ports, 1);
        assert_eq!(d.number_of_stream_output_ports, 1);
    }

    /// STREAM_INPUT 0 is CRF, so a talker must not bind it. This is the sink
    /// `--listener-uid`'s old default of 0 pointed at.
    #[test]
    fn stream_input_0_is_crf_media_clock() {
        let d = decode_stream(&hex(STREAM_INPUT_0_CRF)).expect("decode");
        assert_eq!(d.object_name, "Return CRF");
        assert!(!d.current_format.is_aaf());
        assert_eq!(d.current_format.0, 0x0410_6001_0000_bb80);
        // Its two formats are both CRF, decoded from the real array at 132.
        assert_eq!(d.formats.len(), 2);
        assert!(d.formats.iter().all(|f| !f.is_aaf()));
    }

    /// The formats array lives at byte 132, not immediately after the count at
    /// 86. Reading sequentially produced ten all-zero formats.
    #[test]
    fn stream_input_1_formats_are_real_not_zeros() {
        let d = decode_stream(&hex(STREAM_INPUT_1_AAF)).expect("decode");
        assert_eq!(d.object_name, "Return 1-8");
        assert_eq!(d.current_format.0, 0x0205_0220_0200_6000);
        assert_eq!(d.formats.len(), 10);
        assert!(
            d.formats.iter().all(|f| f.0 != 0),
            "formats decoded as zeros: {:#018x?}",
            d.formats.iter().map(|f| f.0).collect::<Vec<_>>()
        );
        // Five 44.1 kHz and five 48 kHz variants, 1/2/4/6/8 channels each.
        assert_eq!(d.formats[0].0, 0x0204_0220_0040_6000);
        assert_eq!(d.formats[9].0, 0x0205_0220_0200_6000);
        assert!(d.formats.iter().any(|f| f.0 == d.current_format.0));
        // avb_interface_index is at 126 and buffer_length at 128; the old
        // layout read both out of the backup-talker zeros.
        assert_eq!(d.avb_interface_index, 0);
        assert_eq!(d.buffer_length, 2_126_000);
    }

    #[test]
    fn stream_output_1_is_the_audio_source() {
        let d = decode_stream(&hex(STREAM_OUTPUT_1_AAF)).expect("decode");
        assert_eq!(d.object_name, "Send 1-8");
        assert!(d.current_format.is_aaf());
        assert_eq!(d.current_format.0, 0x0205_0220_0200_6000);
        assert_eq!(d.formats.len(), 2);
        assert_eq!(d.formats[0].0, 0x0204_0220_0200_6000);
        assert_eq!(d.formats[1].0, 0x0205_0220_0200_6000);
    }

    /// Our own encoder must place its arrays where the 16R places them, so a
    /// controller reading OUR descriptors uses the same offsets we just proved
    /// against the hardware. Asserted as literals, not as our own constants.
    #[test]
    fn our_encoders_use_the_hardware_offsets() {
        let s = encode_stream(
            &StreamDesc {
                object_name: "x".into(),
                clock_domain_index: 0,
                stream_flags: 3,
                current_format: StreamFormat(0x0205_0220_0200_6000),
                formats: vec![StreamFormat(0x0205_0220_0200_6000)],
                avb_interface_index: 0,
                buffer_length: 8,
            },
            true,
        );
        assert_eq!(u16::from_be_bytes([s[82], s[83]]), 132);
        assert_eq!(u16::from_be_bytes([s[84], s[85]]), 1);
        assert_eq!(
            u64::from_be_bytes(s[132..140].try_into().expect("8 bytes")),
            0x0205_0220_0200_6000
        );

        let a = encode_audio_unit(&AudioUnitDesc {
            object_name: "x".into(),
            clock_domain_index: 0,
            number_of_stream_input_ports: 1,
            number_of_stream_output_ports: 1,
            current_sampling_rate: 48_000,
            sampling_rates: vec![48_000],
        });
        assert_eq!(
            u32::from_be_bytes(a[136..140].try_into().expect("4 bytes")),
            48_000
        );
        assert_eq!(u16::from_be_bytes([a[140], a[141]]), 144);
        assert_eq!(u16::from_be_bytes([a[142], a[143]]), 1);

        let c = encode_configuration(&ConfigurationDesc {
            object_name: "x".into(),
            descriptor_counts: vec![(crate::constants::desc::STREAM_INPUT, 9)],
        });
        assert_eq!(u16::from_be_bytes([c[70], c[71]]), 1);
        assert_eq!(u16::from_be_bytes([c[72], c[73]]), 74);
        assert_eq!(u16::from_be_bytes([c[74], c[75]]), 5);

        let cd = encode_clock_domain(&ClockDomainDesc {
            object_name: "x".into(),
            clock_source_index: 0,
            clock_sources: vec![0, 1],
        });
        assert_eq!(u16::from_be_bytes([cd[68], cd[69]]), 0xffff);
        assert_eq!(u16::from_be_bytes([cd[72], cd[73]]), 76);
        assert_eq!(u16::from_be_bytes([cd[74], cd[75]]), 2);
        assert_eq!(decode_clock_domain(&cd).unwrap().clock_sources, vec![0, 1]);
    }

    /// The 16R's own IDENTIFY control, decoded field by field. This is the
    /// anchor for our encoder: every value below was read off the wire, so a
    /// wrong field offset here cannot be hidden by our encoder agreeing with
    /// our decoder.
    #[test]
    fn control_identify_decodes_the_16r_layout() {
        let b = hex(CONTROL_0_IDENTIFY);
        assert_eq!(b.len(), 113);
        let d = decode_control(&b).expect("decode CONTROL");

        assert_eq!(d.object_name, "Identify LED Control");
        assert_eq!(d.localized_description, 0xffff, "no STRINGS descriptor");
        assert_eq!(d.block_latency, 0);
        assert_eq!(d.control_latency, 0);
        assert_eq!(d.control_domain, 0);
        // CONTROL_LINEAR_UINT8 — one unsigned byte, which is what makes the
        // 9-byte values block below the right shape.
        assert_eq!(d.control_value_type, 0x0001);
        // IDENTIFY: AVB OUI-24 90:e0:f0 then the control index within it.
        assert_eq!(d.control_type, 0x90e0_f000_0000_0001);
        assert_eq!(d.reset_time, 0);
        assert_eq!(d.number_of_values, 1);
        assert_eq!(d.signal_type, 0xffff, "not fed by a signal source");
        assert_eq!(d.signal_index, 0);
        assert_eq!(d.signal_output, 0);

        // minimum, maximum, step, default, current, unit, string.
        assert_eq!(
            d.values,
            vec![0x00, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff]
        );

        // The device states 104, and our fixed fields must end exactly there.
        assert_eq!(u16::from_be_bytes([b[94], b[95]]), CONTROL_VALUES_OFFSET);
        assert_eq!(usize::from(CONTROL_VALUES_OFFSET) + d.values.len(), b.len());
    }

    /// Our encoder must reproduce the 16R's bytes EXACTLY from the decoded
    /// form. Byte equality against hardware is the check a round trip cannot
    /// give: it fails if we place any field even one byte off.
    #[test]
    fn control_identify_re_encodes_byte_for_byte() {
        let b = hex(CONTROL_0_IDENTIFY);
        let d = decode_control(&b).expect("decode CONTROL");
        assert_eq!(encode_control(&d), b);
    }
}
