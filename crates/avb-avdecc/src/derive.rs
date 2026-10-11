// SPDX-License-Identifier: MIT OR Apache-2.0

//! `derive`: build our listener model from the 16R's AEM (design §9.1, §9.3).
//!
//! [`listener_model`] is the ONLY constructor of [`ListenerModel`]; nothing
//! else may fabricate one, which is how we keep rate/channel/stream counts
//! out of the rest of the code (acceptance criterion 3). It picks the audio
//! talker set A (STREAM_OUTPUTs whose current format is AAF), refuses the
//! undecodable and the overflow past `S_MAX`, maps STREAM_INPUT j to the j-th
//! surviving STREAM_OUTPUT (B[j]), copies the sampling rates from the 16R's
//! AUDIO_UNIT, and chooses the clock reference stream per §10.5.

use avb_avtp::stream_format::StreamFormat;
use avb_core::{Eui64, MacAddr};

use crate::aem::model::{AudioMapping, StreamDesc, TalkerAem};
use crate::constants::{clock_source, desc, stream_flags};

/// The maximum number of streams this build binds (S_max; design §9.2, the
/// single allowed stream-count literal outside tests). Mirrors
/// `AVB_MAX_STREAMS` in the dext ABI.
pub const S_MAX: usize = 16;

/// Our entity_model_id prefix bytes (§9.1): `0x02_41_56` ("AV", locally
/// administered, no OUI) ‖ `0x01` (model schema revision). The low 32 bits
/// are `fnv1a32(canonical model)`.
pub const MODEL_ID_PREFIX: u32 = 0x0241_5601;

/// Why a listener model could not be derived (§9.3). The daemon maps these to
/// exit codes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeriveError {
    /// No AUDIO_UNIT in the talker's configuration 0.
    NoAudioUnit,
    /// An A-set STREAM_OUTPUT's current format is not decodable (§9.3: the A
    /// elements we would bind must be decodable by construction of B).
    UnsupportedCurrentFormat { stream_output_index: usize },
    /// The AUDIO_UNIT's current rate disagrees with a bound stream's format
    /// nsr (§9.3 "AUDIO_UNIT rate vs format nsr mismatch -> error").
    RateMismatch { audio_unit_hz: u32, stream_hz: u32 },
    /// |A| exceeds S_MAX and `--allow-partial` was not given (the daemon
    /// decides; `listener_model` always refuses the overflow and records it).
    TooManyStreams { count: usize },
}

impl std::fmt::Display for DeriveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeriveError::NoAudioUnit => write!(f, "no AUDIO_UNIT in configuration 0"),
            DeriveError::UnsupportedCurrentFormat {
                stream_output_index,
            } => write!(
                f,
                "STREAM_OUTPUT {stream_output_index} current format is not decodable"
            ),
            DeriveError::RateMismatch {
                audio_unit_hz,
                stream_hz,
            } => write!(
                f,
                "AUDIO_UNIT rate {audio_unit_hz} Hz != stream format rate {stream_hz} Hz"
            ),
            DeriveError::TooManyStreams { count } => {
                write!(
                    f,
                    "16R exposes {count} AAF STREAM_OUTPUTs; this build supports S_max = {S_MAX}"
                )
            }
        }
    }
}

impl std::error::Error for DeriveError {}

/// One derived STREAM_INPUT (design §9.3): STREAM_INPUT j ↔ 16R STREAM_OUTPUT
/// `stream_output_index`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DerivedStream {
    /// Dense listener index j.
    pub listener_index: usize,
    /// The 16R STREAM_OUTPUT descriptor index this corresponds to (B[j]).
    pub stream_output_index: usize,
    pub object_name: String,
    pub current_format: StreamFormat,
    /// Every format of B[j] that `decodable()` accepts.
    pub formats: Vec<StreamFormat>,
    /// Channel names (AUDIO_MAP/cluster, else "<name> ch<k>").
    pub channel_names: Vec<String>,
    /// `channels` from the current format.
    pub channels: u16,
    /// CLOCK_SYNC_SOURCE present on B[j].
    pub is_clock_sync_source: bool,
}

/// A 16R STREAM_OUTPUT that was not bound and why (design §9.3, §9.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefusedStream {
    /// Not an AAF current format: `unbound_by_design` (never a failure).
    UnboundByDesign {
        stream_output_index: usize,
        subtype: String,
    },
    /// AAF but not decodable (depth/nsr/channels/spf out of range).
    Undecodable { stream_output_index: usize },
    /// Dropped because |A| > S_MAX.
    OverflowSMax { stream_output_index: usize },
}

/// Our derived listener model (design §9.3). Constructed ONLY by
/// [`listener_model`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenerModel {
    pub entity_id: Eui64,
    pub entity_model_id: u64,
    pub mac: MacAddr,
    /// STREAM_INPUTs, dense in j.
    pub streams: Vec<DerivedStream>,
    /// Sampling rates copied from the 16R AUDIO_UNIT.
    pub sampling_rates: Vec<u32>,
    pub current_sampling_rate: u32,
    /// The reference STREAM_INPUT index r (§10.5): lowest j with
    /// CLOCK_SYNC_SOURCE, else 0.
    pub reference_index: usize,
    /// STREAM_OUTPUTs we did not bind, with the reason.
    pub refused: Vec<RefusedStream>,
    pub firmware_version: String,
}

impl ListenerModel {
    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }

    /// Total audio channel count across all bound streams.
    pub fn total_channels(&self) -> usize {
        self.streams.iter().map(|s| usize::from(s.channels)).sum()
    }
}

/// FNV-1a 32-bit over `bytes` (§9.1 model-id low word). Deterministic, no
/// external crate.
pub fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in bytes {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// The reference STREAM_INPUT index (§10.5): the lowest derived index whose
/// B[j] carries CLOCK_SYNC_SOURCE, else 0. Shared with avb-mediaclock.
pub fn reference_index(model: &ListenerModel) -> usize {
    model
        .streams
        .iter()
        .find(|s| s.is_clock_sync_source)
        .map_or(0, |s| s.listener_index)
}

/// Resolve the channel names for stream `stream_output_index` from AUDIO_MAP /
/// AUDIO_CLUSTER, falling back to "<stream name> ch<k>" (§9.3).
fn channel_names(
    aem: &TalkerAem,
    out: &StreamDesc,
    out_index: usize,
    channels: u16,
) -> Vec<String> {
    let mut names = Vec::with_capacity(usize::from(channels));
    for k in 0..channels {
        let via_map = resolve_cluster_name(aem, out_index, k);
        names.push(via_map.unwrap_or_else(|| format!("{} ch{}", out.object_name, k + 1)));
    }
    names
}

fn resolve_cluster_name(aem: &TalkerAem, stream_index: usize, channel: u16) -> Option<String> {
    let stream_index = u16::try_from(stream_index).ok()?;
    let mapping: Option<&AudioMapping> = aem.audio_maps.iter().find_map(|m| {
        m.mappings
            .iter()
            .find(|x| x.stream_index == stream_index && x.stream_channel == channel)
    });
    let cluster_off = mapping?.cluster_offset;
    let cluster = aem.audio_clusters.get(usize::from(cluster_off))?;
    if cluster.object_name.is_empty() {
        None
    } else {
        Some(cluster.object_name.clone())
    }
}

/// Build our listener model from the 16R AEM (§9.1, §9.3). The ONLY
/// constructor of [`ListenerModel`].
///
/// `our_mac` is the I210 MAC; `firmware_version` is `phase1a-<git sha>`.
pub fn listener_model(
    aem: &TalkerAem,
    our_mac: MacAddr,
    firmware_version: &str,
) -> Result<ListenerModel, DeriveError> {
    let audio_unit = aem.audio_units.first().ok_or(DeriveError::NoAudioUnit)?;
    let au_hz = audio_unit.current_sampling_rate;

    // Audio talker set A: STREAM_OUTPUTs whose current format is AAF (§9.2).
    // Non-AAF are unbound_by_design. The A order is the descriptor-index order.
    let mut refused = Vec::new();
    let mut a_indices: Vec<usize> = Vec::new();
    for (i, out) in aem.stream_outputs.iter().enumerate() {
        if out.current_format.is_aaf() {
            a_indices.push(i);
        } else {
            refused.push(RefusedStream::UnboundByDesign {
                stream_output_index: i,
                subtype: out.current_format.subtype_name(),
            });
        }
    }

    // Overflow past S_MAX is refused before binding (§9.2). We record the
    // overflow and return TooManyStreams; the daemon decides --allow-partial.
    if a_indices.len() > S_MAX {
        for &i in a_indices.iter().skip(S_MAX) {
            refused.push(RefusedStream::OverflowSMax {
                stream_output_index: i,
            });
        }
        return Err(DeriveError::TooManyStreams {
            count: a_indices.len(),
        });
    }

    // B = A minus refused-undecodable; j is dense (§9.3).
    let mut streams = Vec::new();
    for &out_index in &a_indices {
        let out = match aem.stream_outputs.get(out_index) {
            Some(o) => o,
            None => continue,
        };
        // The current format must be decodable (A elements we bind), else the
        // whole derive fails per §9.3 (B is decodable by construction).
        if !out.current_format.decodable() {
            // An AAF-but-undecodable current format on an A member is a hard
            // error: it was selected into A (AAF) yet cannot be bound.
            return Err(DeriveError::UnsupportedCurrentFormat {
                stream_output_index: out_index,
            });
        }
        let aaf = match out.current_format.aaf() {
            Some(a) => a,
            None => continue,
        };
        // Rate check: the AUDIO_UNIT's current rate must match the stream nsr.
        if let Some(stream_hz) = aaf.nsr_hz() {
            if stream_hz != au_hz {
                return Err(DeriveError::RateMismatch {
                    audio_unit_hz: au_hz,
                    stream_hz,
                });
            }
        }
        let channels = aaf.channels;
        let formats: Vec<StreamFormat> = out
            .formats
            .iter()
            .copied()
            .filter(|f| f.decodable())
            .collect();
        let listener_index = streams.len();
        let channel_names = channel_names(aem, out, out_index, channels);
        streams.push(DerivedStream {
            listener_index,
            stream_output_index: out_index,
            object_name: truncate_utf8(&format!("→Mac {}", out.object_name), 64),
            current_format: out.current_format,
            formats,
            channel_names,
            channels,
            is_clock_sync_source: out.stream_flags & stream_flags::CLOCK_SYNC_SOURCE != 0,
        });
    }

    let reference = streams
        .iter()
        .find(|s| s.is_clock_sync_source)
        .map_or(0, |s| s.listener_index);

    let entity_id = Eui64::from_mac(our_mac);
    let model = {
        let mut m = ListenerModel {
            entity_id,
            entity_model_id: 0, // filled below
            mac: our_mac,
            streams,
            sampling_rates: audio_unit.sampling_rates.clone(),
            current_sampling_rate: au_hz,
            reference_index: reference,
            refused,
            firmware_version: firmware_version.to_string(),
        };
        m.entity_model_id = compute_model_id(&m);
        m
    };
    Ok(model)
}

/// `0x02415601 || fnv1a32(canonical model)` (§9.1). The canonical form is the
/// sorted (listener_index, formats[], current_format), the rates, and the
/// per-stream channel counts — so the id changes iff the derived model does
/// and is stable across runs against the same 16R (ADC-9).
pub fn compute_model_id(m: &ListenerModel) -> u64 {
    let mut canon = Vec::new();
    let mut streams: Vec<&DerivedStream> = m.streams.iter().collect();
    streams.sort_by_key(|s| s.listener_index);
    for s in streams {
        canon.extend_from_slice(&(s.listener_index as u32).to_be_bytes());
        canon.extend_from_slice(&(s.channels).to_be_bytes());
        canon.extend_from_slice(&s.current_format.0.to_be_bytes());
        for f in &s.formats {
            canon.extend_from_slice(&f.0.to_be_bytes());
        }
        canon.push(0xFF); // separator
    }
    for &hz in &m.sampling_rates {
        canon.extend_from_slice(&hz.to_be_bytes());
    }
    let lo = fnv1a32(&canon);
    (u64::from(MODEL_ID_PREFIX) << 32) | u64::from(lo)
}

/// Truncate a string to at most `max` bytes without splitting a char.
fn truncate_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or("").to_string()
}

/// Does CLOCK_SOURCE 1 point at the reference STREAM_INPUT? Helper the entity
/// uses so the AEM's CLOCK_SOURCE location follows the reference index (§9.3,
/// test "CLOCK_SOURCE 1 location following the reference index").
pub fn clock_source_location(model: &ListenerModel) -> (u16, u16) {
    (desc::STREAM_INPUT, model.reference_index as u16)
}

/// The clock-source type for CLOCK_SOURCE 1 (INPUT_STREAM, §9.3).
pub const REFERENCE_CLOCK_SOURCE_TYPE: u16 = clock_source::INPUT_STREAM;

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::aem::model::{AudioClusterDesc, AudioMapDesc, AudioUnitDesc, EntityDesc};

    const OUR_MAC: MacAddr = MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]);

    fn aaf(nsr: u8, depth: u8, ch: u16) -> StreamFormat {
        StreamFormat::from_aaf(avb_avtp::stream_format::AafFormat {
            ut: false,
            nsr,
            format: avb_avtp::aaf::format::INT_32BIT,
            bit_depth: depth,
            channels: ch,
            spf: 6,
        })
    }

    fn out_stream(name: &str, fmt: StreamFormat, flags: u16) -> StreamDesc {
        StreamDesc {
            object_name: name.into(),
            clock_domain_index: 0,
            stream_flags: flags,
            current_format: fmt,
            formats: vec![fmt],
            avb_interface_index: 0,
            buffer_length: 0,
        }
    }

    fn aem_with(outs: Vec<StreamDesc>, au_hz: u32) -> TalkerAem {
        TalkerAem {
            entity: EntityDesc {
                entity_id: Eui64::from_u64(0x000A_9203_3BA9_0001),
                talker_stream_sources: outs.len() as u16,
                ..Default::default()
            },
            configuration_index: 0,
            audio_units: vec![AudioUnitDesc {
                current_sampling_rate: au_hz,
                sampling_rates: vec![44_100, 48_000, 96_000],
                number_of_stream_input_ports: 0,
                ..Default::default()
            }],
            stream_outputs: outs,
            talker_mac: MacAddr([0, 0x0a, 0x92, 3, 0x3b, 0xa9]),
            ..Default::default()
        }
    }

    #[test]
    fn derive_three_streams_four_channels() {
        let f = aaf(5, 24, 4);
        let aem = aem_with(
            vec![
                out_stream("Out 1", f, stream_flags::CLASS_A),
                out_stream("Out 2", f, stream_flags::CLASS_A),
                out_stream("Out 3", f, stream_flags::CLASS_A),
            ],
            48_000,
        );
        let m = listener_model(&aem, OUR_MAC, "phase1a-test").unwrap();
        assert_eq!(m.stream_count(), 3);
        assert_eq!(m.total_channels(), 12);
        assert_eq!(m.streams[0].stream_output_index, 0);
        assert_eq!(m.streams[2].stream_output_index, 2);
        assert_eq!(m.current_sampling_rate, 48_000);
        assert_eq!(m.reference_index, 0); // none has CLOCK_SYNC_SOURCE -> 0
        assert!(m.streams[0].object_name.starts_with("→Mac Out 1"));
        assert!(m.refused.is_empty());
        assert_eq!(m.entity_id, Eui64::from_mac(OUR_MAC));
        assert_eq!((m.entity_model_id >> 32) as u32, MODEL_ID_PREFIX);
    }

    #[test]
    fn derive_refuses_seventeen() {
        let f = aaf(5, 24, 2);
        let outs: Vec<StreamDesc> = (0..17)
            .map(|i| out_stream(&format!("Out {i}"), f, stream_flags::CLASS_A))
            .collect();
        let aem = aem_with(outs, 48_000);
        let err = listener_model(&aem, OUR_MAC, "x").unwrap_err();
        assert_eq!(err, DeriveError::TooManyStreams { count: 17 });
    }

    #[test]
    fn derive_crf_between_two_aaf() {
        // Output 0 AAF, output 1 CRF (unbound_by_design), output 2 AAF.
        let f = aaf(5, 24, 8);
        let crf = StreamFormat(0x0410_6001_0000_BB80);
        let aem = aem_with(
            vec![
                out_stream("Out 0", f, stream_flags::CLASS_A),
                out_stream("CRF", crf, 0),
                out_stream("Out 2", f, stream_flags::CLASS_A),
            ],
            48_000,
        );
        let m = listener_model(&aem, OUR_MAC, "x").unwrap();
        // No exit 8; STREAM_INPUT 0 <-> output 0, STREAM_INPUT 1 <-> output 2.
        assert_eq!(m.stream_count(), 2);
        assert_eq!(m.streams[0].stream_output_index, 0);
        assert_eq!(m.streams[1].stream_output_index, 2);
        assert!(matches!(
            m.refused.first(),
            Some(RefusedStream::UnboundByDesign {
                stream_output_index: 1,
                ..
            })
        ));
    }

    #[test]
    fn reference_follows_clock_sync_source() {
        let f = aaf(5, 24, 8);
        let aem = aem_with(
            vec![
                out_stream("Out 0", f, stream_flags::CLASS_A),
                out_stream(
                    "Out 1",
                    f,
                    stream_flags::CLASS_A | stream_flags::CLOCK_SYNC_SOURCE,
                ),
            ],
            48_000,
        );
        let m = listener_model(&aem, OUR_MAC, "x").unwrap();
        assert_eq!(m.reference_index, 1);
        assert_eq!(reference_index(&m), 1);
        assert_eq!(clock_source_location(&m), (desc::STREAM_INPUT, 1));
    }

    #[test]
    fn rate_mismatch_is_error() {
        let f = aaf(5, 24, 8); // 48 kHz nsr
        let aem = aem_with(vec![out_stream("Out", f, 0)], 96_000); // AU says 96k
        assert_eq!(
            listener_model(&aem, OUR_MAC, "x"),
            Err(DeriveError::RateMismatch {
                audio_unit_hz: 96_000,
                stream_hz: 48_000
            })
        );
    }

    #[test]
    fn model_id_is_stable_and_changes_with_model() {
        let f = aaf(5, 24, 8);
        let a = aem_with(vec![out_stream("Out", f, 0)], 48_000);
        let m1 = listener_model(&a, OUR_MAC, "x").unwrap();
        let m2 = listener_model(&a, OUR_MAC, "y").unwrap(); // fw differs, model same
        assert_eq!(m1.entity_model_id, m2.entity_model_id);
        // A different channel count changes the id.
        let f2 = aaf(5, 24, 4);
        let b = aem_with(vec![out_stream("Out", f2, 0)], 48_000);
        let m3 = listener_model(&b, OUR_MAC, "x").unwrap();
        assert_ne!(m1.entity_model_id, m3.entity_model_id);
    }

    #[test]
    fn channel_names_from_map_else_fallback() {
        let f = aaf(5, 24, 2);
        let mut aem = aem_with(vec![out_stream("Out 1", f, 0)], 48_000);
        aem.audio_clusters = vec![
            AudioClusterDesc {
                object_name: "Kick".into(),
                channel_count: 1,
                format: 0x40,
            },
            AudioClusterDesc {
                object_name: "Snare".into(),
                channel_count: 1,
                format: 0x40,
            },
        ];
        aem.audio_maps = vec![AudioMapDesc {
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
        }];
        let m = listener_model(&aem, OUR_MAC, "x").unwrap();
        assert_eq!(m.streams[0].channel_names, vec!["Kick", "Snare"]);
        // Without a map, fall back to "<name> ch<k>".
        let aem2 = aem_with(vec![out_stream("Out 1", f, 0)], 48_000);
        let m2 = listener_model(&aem2, OUR_MAC, "x").unwrap();
        assert_eq!(m2.streams[0].channel_names, vec!["Out 1 ch1", "Out 1 ch2"]);
    }

    #[test]
    fn fnv1a32_known_vector() {
        // FNV-1a of the empty string is the offset basis.
        assert_eq!(fnv1a32(b""), 0x811c_9dc5);
        // "a" -> 0xe40c292c (reference FNV-1a 32).
        assert_eq!(fnv1a32(b"a"), 0xe40c_292c);
    }
}
