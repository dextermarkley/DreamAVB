//! The AEM dump JSON (design §9.2 ADC-6), schema `avb-aem-dump/1`.
//!
//! Builds a `serde_json::Value` with exactly the §9.2 top-level keys from a
//! [`TalkerAem`] plus the ADP record and the per-interface data the daemon
//! supplies. This module decides the SHAPE; the daemon fills `captured_at`,
//! `counters`, `milan_info`, `timeouts` and `quirks` from run-time state.

use serde_json::{json, Map, Value};

use avb_avtp::stream_format::{Decoded, StreamFormat};

use super::model::{StreamDesc, TalkerAem};
use crate::adp::Adpdu;

/// The dump schema identifier (§9.2).
pub const SCHEMA: &str = "avb-aem-dump/1";

/// Render a [`StreamFormat`] as the `current_format` object the dump uses
/// (§9.2: `raw`, `subtype`, `format`, `nsr_hz`, `bit_depth`, `channels`,
/// `samples_per_frame`).
pub fn format_json(f: StreamFormat) -> Value {
    let mut m = Map::new();
    m.insert("raw".into(), json!(format!("0x{:016x}", f.0)));
    m.insert("subtype".into(), json!(f.subtype_name()));
    match f.decode() {
        Decoded::Aaf(a) => {
            m.insert("format".into(), json!(aaf_format_name(a.format)));
            m.insert("nsr_hz".into(), json!(a.nsr_hz()));
            m.insert("bit_depth".into(), json!(a.bit_depth));
            m.insert("channels".into(), json!(a.channels));
            m.insert("samples_per_frame".into(), json!(a.spf));
            m.insert("decodable".into(), json!(f.decodable()));
        }
        Decoded::Crf(c) => {
            m.insert("crf_type".into(), json!(c.crf_type));
            m.insert("base_frequency".into(), json!(c.base_frequency));
            m.insert("timestamp_interval".into(), json!(c.timestamp_interval));
        }
        Decoded::Iec61883(i) => {
            m.insert("fmt".into(), json!(i.fmt));
            m.insert("fdf_sfc".into(), json!(i.fdf_sfc));
            m.insert("dbs".into(), json!(i.dbs));
        }
        Decoded::Other { subtype } => {
            m.insert("other_subtype".into(), json!(subtype));
        }
    }
    Value::Object(m)
}

fn aaf_format_name(f: u8) -> String {
    avb_avtp::aaf::format::name(f).map_or_else(|| format!("0x{f:02x}"), str::to_string)
}

fn stream_json(name: &str, d: &StreamDesc) -> Value {
    json!({
        "name": name,
        "current_format": format_json(d.current_format),
        "formats": d.formats.iter().map(|f| format_json(*f)).collect::<Vec<_>>(),
        "stream_flags": d.stream_flags,
        "clock_domain_index": d.clock_domain_index,
    })
}

/// Build the dump skeleton from the static AEM data (§9.2 top-level keys).
/// `captured_at`, run-time `counters`, `milan_info`, `timeouts` and `quirks`
/// are injected by the daemon via [`DumpBuilder`].
pub struct DumpBuilder {
    root: Map<String, Value>,
}

impl DumpBuilder {
    pub fn new(adp: &Adpdu, aem: &TalkerAem) -> Self {
        let mut root = Map::new();
        root.insert("schema".into(), json!(SCHEMA));
        root.insert(
            "talker_entity_id".into(),
            json!(aem.entity.entity_id.to_string()),
        );
        root.insert("adp".into(), adp_json(adp));
        root.insert("entity".into(), entity_json(aem));
        root.insert("configurations".into(), configurations_json(aem));
        root.insert("audio_units".into(), audio_units_json(aem));
        root.insert(
            "streams_out".into(),
            Value::Array(
                aem.stream_outputs
                    .iter()
                    .map(|d| stream_json(&d.object_name, d))
                    .collect(),
            ),
        );
        root.insert(
            "streams_in".into(),
            Value::Array(
                aem.stream_inputs
                    .iter()
                    .map(|d| stream_json(&d.object_name, d))
                    .collect(),
            ),
        );
        root.insert("clock".into(), clock_json(aem));
        root.insert("avb_interface".into(), avb_interface_json(aem));
        root.insert("channel_map".into(), channel_map_json(aem));
        root.insert("quirks".into(), json!(aem.quirks));
        Self { root }
    }

    /// Inject a run-time field (`captured_at`, `counters`, `milan_info`,
    /// `timeouts`, …).
    pub fn with(mut self, key: &str, value: Value) -> Self {
        self.root.insert(key.into(), value);
        self
    }

    pub fn build(self) -> Value {
        Value::Object(self.root)
    }
}

fn adp_json(a: &Adpdu) -> Value {
    json!({
        "entity_id": a.entity_id.to_string(),
        "entity_model_id": format!("0x{:016x}", a.entity_model_id),
        "entity_capabilities": a.entity_capabilities,
        "talker_stream_sources": a.talker_stream_sources,
        "listener_stream_sinks": a.listener_stream_sinks,
        "controller_capabilities": a.controller_capabilities,
        "available_index": a.available_index,
        "gptp_grandmaster_id": format!("0x{:016x}", a.gptp_grandmaster_id),
        "gptp_domain_number": a.gptp_domain_number,
        "valid_time": a.valid_time,
    })
}

fn entity_json(aem: &TalkerAem) -> Value {
    let e = &aem.entity;
    json!({
        "entity_id": e.entity_id.to_string(),
        "entity_model_id": format!("0x{:016x}", e.entity_model_id),
        "entity_name": e.entity_name,
        "firmware_version": e.firmware_version,
        "serial_number": e.serial_number,
        "configurations_count": e.configurations_count,
        "current_configuration": e.current_configuration,
        "talker_stream_sources": e.talker_stream_sources,
        "listener_stream_sinks": e.listener_stream_sinks,
    })
}

fn configurations_json(aem: &TalkerAem) -> Value {
    json!([{
        "index": aem.configuration_index,
        "descriptor_counts": aem
            .configuration
            .descriptor_counts
            .iter()
            .map(|(t, n)| json!({"type": crate::constants::desc::name(*t).unwrap_or("UNKNOWN"), "count": n}))
            .collect::<Vec<_>>(),
    }])
}

fn audio_units_json(aem: &TalkerAem) -> Value {
    Value::Array(
        aem.audio_units
            .iter()
            .map(|u| {
                json!({
                    "name": u.object_name,
                    "current_sampling_rate": u.current_sampling_rate,
                    "sampling_rates": u.sampling_rates,
                    "number_of_stream_input_ports": u.number_of_stream_input_ports,
                    "number_of_stream_output_ports": u.number_of_stream_output_ports,
                })
            })
            .collect(),
    )
}

fn clock_json(aem: &TalkerAem) -> Value {
    let sources: Vec<Value> = aem
        .clock_sources
        .iter()
        .map(|s| {
            json!({
                "name": s.object_name,
                "type": s.clock_source_type,
                "location_type": crate::constants::desc::name(s.clock_source_location_type),
                "location_index": s.clock_source_location_index,
            })
        })
        .collect();
    let domain_current_source = aem.clock_domains.first().map(|d| d.clock_source_index);
    json!({ "sources": sources, "domain_current_source": domain_current_source })
}

fn avb_interface_json(aem: &TalkerAem) -> Value {
    match aem.avb_interfaces.first() {
        Some(i) => json!({
            "mac_address": i.mac_address.to_string(),
            "clock_identity": i.clock_identity.to_string(),
            "priority1": i.priority1,
            "clock_class": i.clock_class,
            "priority2": i.priority2,
            "domain_number": i.domain_number,
            "log_sync_interval": i.log_sync_interval,
        }),
        None => Value::Null,
    }
}

fn channel_map_json(aem: &TalkerAem) -> Value {
    let mut out = Vec::new();
    for map in &aem.audio_maps {
        for m in &map.mappings {
            let cluster_name = aem
                .audio_clusters
                .get(usize::from(m.cluster_offset))
                .map(|c| c.object_name.clone());
            out.push(json!({
                "stream": m.stream_index,
                "channel": m.stream_channel,
                "cluster_index": m.cluster_offset,
                "cluster_name": cluster_name,
            }));
        }
    }
    Value::Array(out)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::aem::model::{AudioUnitDesc, EntityDesc};
    use avb_core::{Eui64, MacAddr};

    fn tiny_aem() -> TalkerAem {
        TalkerAem {
            entity: EntityDesc {
                entity_id: Eui64::from_u64(0x000A_9203_3BA9_0001),
                entity_model_id: 0xABCD,
                entity_name: "StudioLive 16R".into(),
                configurations_count: 1,
                talker_stream_sources: 8,
                ..Default::default()
            },
            configuration_index: 0,
            audio_units: vec![AudioUnitDesc {
                object_name: "AU".into(),
                current_sampling_rate: 48_000,
                sampling_rates: vec![48_000, 96_000],
                ..Default::default()
            }],
            stream_outputs: vec![StreamDesc {
                object_name: "Out 1".into(),
                current_format: StreamFormat(0x0205_0220_0200_6000),
                formats: vec![StreamFormat(0x0205_0220_0200_6000)],
                ..Default::default()
            }],
            talker_mac: MacAddr([0, 0x0a, 0x92, 3, 0x3b, 0xa9]),
            ..Default::default()
        }
    }

    #[test]
    fn dump_has_schema_and_toplevel_keys() {
        let adp = Adpdu {
            entity_id: Eui64::from_u64(0x000A_9203_3BA9_0001),
            ..Default::default()
        };
        let v = DumpBuilder::new(&adp, &tiny_aem())
            .with("captured_at", json!("2026-10-10T23:00:00Z"))
            .build();
        assert_eq!(v["schema"], json!(SCHEMA));
        for key in [
            "talker_entity_id",
            "adp",
            "entity",
            "configurations",
            "audio_units",
            "streams_out",
            "streams_in",
            "clock",
            "avb_interface",
            "channel_map",
            "quirks",
            "captured_at",
        ] {
            assert!(v.get(key).is_some(), "missing key {key}");
        }
        assert_eq!(
            v["streams_out"][0]["current_format"]["nsr_hz"],
            json!(48_000)
        );
        assert_eq!(
            v["streams_out"][0]["current_format"]["subtype"],
            json!("AAF")
        );
        assert_eq!(
            v["streams_out"][0]["current_format"]["decodable"],
            json!(true)
        );
    }
}
