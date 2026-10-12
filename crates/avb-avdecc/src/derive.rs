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

// --- Talker direction: our output format, derived from the 16R's AEM --------
//
// The mirror of `listener_model`. For the listener direction we read the 16R's
// STREAM_OUTPUTs (what it sends); for the talker direction we must read its
// STREAM_INPUTs (what it is configured to RECEIVE) and match one exactly.
//
// AGENTS.md: sample rates, channel counts and stream counts are never
// hardcoded. [`talker_output_format`] is the only thing allowed to choose our
// output format, so the rate/channel/spf our packetizer uses cannot disagree
// with the sink we are sending to.

/// Why our talker output format could not be derived from the 16R's AEM.
///
/// Every variant is a refusal to transmit. A format we guess wrong is worse
/// than not streaming: the 16R silently discards mismatched packets, which
/// looks exactly like a transmit fault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TalkerFormatError {
    /// The 16R has no STREAM_INPUT at `listener_uid` (we read the descriptor
    /// count from its CONFIGURATION, so this means the sink does not exist).
    NoSuchStreamInput { listener_uid: u16, available: usize },
    /// The sink's current format is not AAF (e.g. CRF or IEC 61883-6). We only
    /// packetize AAF, and converting is not a thing a talker may decide to do.
    NotAaf { listener_uid: u16, subtype: String },
    /// AAF, but outside what this build can packetize (depth/nsr/channels/spf).
    Undecodable { listener_uid: u16, raw: u64 },
    /// The `ut` ("up to") bit is set, so channels/spf are MAXIMA rather than
    /// exact values. A talker has to emit an exact channel count and an exact
    /// samples-per-frame, so there is nothing here to derive from.
    UpToFormat { listener_uid: u16 },
    /// The 16R's AUDIO_UNIT runs at a different rate than the sink's format
    /// claims. Trusting either one alone would be a guess, so we refuse.
    RateMismatch { audio_unit_hz: u32, stream_hz: u32 },
}

impl std::fmt::Display for TalkerFormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TalkerFormatError::NoSuchStreamInput {
                listener_uid,
                available,
            } => write!(
                f,
                "the 16R has no STREAM_INPUT {listener_uid} (it exposes {available}); pass --listener-uid below {available}"
            ),
            TalkerFormatError::NotAaf {
                listener_uid,
                subtype,
            } => write!(
                f,
                "16R STREAM_INPUT {listener_uid} is configured for {subtype}, not AAF; this build only packetizes AAF"
            ),
            TalkerFormatError::Undecodable { listener_uid, raw } => write!(
                f,
                "16R STREAM_INPUT {listener_uid} current format 0x{raw:016x} is AAF but outside this build's supported depth/rate/channels/spf"
            ),
            TalkerFormatError::UpToFormat { listener_uid } => write!(
                f,
                "16R STREAM_INPUT {listener_uid} advertises an 'up to' format, so its channel count and samples-per-frame are maxima, not exact values to transmit"
            ),
            TalkerFormatError::RateMismatch {
                audio_unit_hz,
                stream_hz,
            } => write!(
                f,
                "16R AUDIO_UNIT runs at {audio_unit_hz} Hz but its STREAM_INPUT format says {stream_hz} Hz"
            ),
        }
    }
}

impl std::error::Error for TalkerFormatError {}

/// The first STREAM descriptor whose current format is AAF audio we can
/// packetize, as a `unique_id` to bind over ACMP.
///
/// A Milan device's stream list is NOT all audio. On the PreSonus 16R index 0
/// is `Return CRF` / `Send CRF` — a Clock Reference Format stream carrying the
/// media clock — and the audio sinks are 1..=8 (`Return 1-8` .. `Return
/// 57-64`). So a `unique_id` default of 0 binds the media-clock stream: the
/// reservation and the ACMP connect both succeed, and not one audio sample
/// goes anywhere. That is indistinguishable from a transmit fault, which is
/// why the default has to be derived rather than assumed.
///
/// Returns `None` when the device exposes no AAF stream in this direction, so
/// the caller refuses rather than falling back to index 0.
pub fn first_audio_stream(streams: &[StreamDesc]) -> Option<u16> {
    streams.iter().enumerate().find_map(|(i, s)| {
        let aaf = s.current_format.aaf()?;
        // `ut` formats are maxima, not exact values; nothing to transmit.
        if aaf.ut || !s.current_format.decodable() {
            return None;
        }
        u16::try_from(i).ok()
    })
}

/// Our talker output format, taken from the 16R STREAM_INPUT we are about to
/// bind (`listener_uid`). This is the ONLY place the talker's rate, channel
/// count and samples-per-frame come from.
///
/// `audio_unit_hz` is the 16R's AUDIO_UNIT current sampling rate when we read
/// that descriptor; `None` skips the cross-check rather than inventing a rate.
///
/// We use `current_format` (what the sink is configured for right now), not
/// the `formats` list of everything it could accept: picking from the list
/// would mean choosing a configuration the 16R is not actually in.
pub fn talker_output_format(
    inputs: &[StreamDesc],
    audio_unit_hz: Option<u32>,
    listener_uid: u16,
) -> Result<StreamFormat, TalkerFormatError> {
    let sink =
        inputs
            .get(usize::from(listener_uid))
            .ok_or(TalkerFormatError::NoSuchStreamInput {
                listener_uid,
                available: inputs.len(),
            })?;
    let fmt = sink.current_format;

    let aaf = fmt.aaf().ok_or_else(|| TalkerFormatError::NotAaf {
        listener_uid,
        subtype: fmt.subtype_name(),
    })?;
    // `ut` makes channels/spf upper bounds, so there is no exact format to
    // derive. Checked BEFORE `decodable()`, which also folds in `!ut`: both
    // would refuse, but only this one tells the user why.
    if aaf.ut {
        return Err(TalkerFormatError::UpToFormat { listener_uid });
    }
    if !fmt.decodable() {
        return Err(TalkerFormatError::Undecodable {
            listener_uid,
            raw: fmt.0,
        });
    }
    // Both descriptors describe the same hardware, so a disagreement means we
    // do not understand its configuration; refuse rather than pick a side.
    if let (Some(au_hz), Some(stream_hz)) = (audio_unit_hz, aaf.nsr_hz()) {
        if au_hz != stream_hz {
            return Err(TalkerFormatError::RateMismatch {
                audio_unit_hz: au_hz,
                stream_hz,
            });
        }
    }
    Ok(fmt)
}

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

    // --- talker_output_format ---------------------------------------------
    //
    // The format the talker packetizes with must come from the 16R, not from a
    // constant. The daemon previously hardcoded the Milan base format
    // (0x0205_0220_0200_6000), which happens to match this 16R and would
    // silently mismatch a differently-configured one.

    /// The format the daemon used to hardcode. Named here ONLY so a test can
    /// assert the derivation does not return it for a sink configured
    /// otherwise.
    const OLD_HARDCODED: u64 = 0x0205_0220_0200_6000;

    fn aaf_spf(nsr: u8, depth: u8, ch: u16, spf: u16, ut: bool) -> StreamFormat {
        StreamFormat::from_aaf(avb_avtp::stream_format::AafFormat {
            ut,
            nsr,
            format: avb_avtp::aaf::format::INT_32BIT,
            bit_depth: depth,
            channels: ch,
            spf,
        })
    }

    #[test]
    fn derives_the_sinks_own_format_not_the_old_hardcoded_default() {
        // A 16R sink configured for 96 kHz / 4 ch / 12 spf — nothing like the
        // Milan base. The derivation must follow the hardware.
        let sink = aaf_spf(7, 32, 4, 12, false);
        let got = talker_output_format(&[out_stream("In 1", sink, 0)], Some(96_000), 0).unwrap();
        assert_eq!(got, sink);
        assert_ne!(
            got.0, OLD_HARDCODED,
            "derivation returned the old hardcoded Milan base format for a 96 kHz/4 ch sink"
        );
        let a = got.aaf().unwrap();
        assert_eq!((a.nsr_hz(), a.channels, a.spf), (Some(96_000), 4, 12));
    }

    #[test]
    fn picks_the_sink_named_by_listener_uid() {
        let first = aaf_spf(5, 32, 8, 6, false);
        let second = aaf_spf(5, 32, 2, 6, false);
        let inputs = [out_stream("In 1", first, 0), out_stream("In 2", second, 0)];
        assert_eq!(
            talker_output_format(&inputs, Some(48_000), 1).unwrap(),
            second
        );
        assert_eq!(
            talker_output_format(&inputs, Some(48_000), 0).unwrap(),
            first
        );
    }

    #[test]
    fn a_sink_that_does_not_exist_is_refused() {
        let inputs = [out_stream("In 1", aaf_spf(5, 32, 8, 6, false), 0)];
        assert_eq!(
            talker_output_format(&inputs, Some(48_000), 3),
            Err(TalkerFormatError::NoSuchStreamInput {
                listener_uid: 3,
                available: 1
            })
        );
        // No sinks at all is the same refusal, not a panic.
        assert!(matches!(
            talker_output_format(&[], Some(48_000), 0),
            Err(TalkerFormatError::NoSuchStreamInput { available: 0, .. })
        ));
    }

    #[test]
    fn a_non_aaf_sink_is_refused() {
        let crf = StreamFormat(0x0410_6001_0000_BB80);
        let err = talker_output_format(&[out_stream("In 1", crf, 0)], Some(48_000), 0).unwrap_err();
        assert!(
            matches!(
                err,
                TalkerFormatError::NotAaf {
                    listener_uid: 0,
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn an_up_to_format_is_refused_because_channels_and_spf_are_maxima() {
        // `ut` means "up to": 8 ch / 6 spf are upper bounds, so there is no
        // exact format to transmit.
        let upto = aaf_spf(5, 32, 8, 6, true);
        assert_eq!(
            talker_output_format(&[out_stream("In 1", upto, 0)], Some(48_000), 0),
            Err(TalkerFormatError::UpToFormat { listener_uid: 0 })
        );
        // The same format without `ut` is accepted, so the refusal is the
        // bit's doing and not something else about the format.
        let exact = aaf_spf(5, 32, 8, 6, false);
        assert!(talker_output_format(&[out_stream("In 1", exact, 0)], Some(48_000), 0).is_ok());
    }

    #[test]
    fn audio_unit_and_sink_disagreeing_on_rate_is_refused() {
        let sink = aaf_spf(5, 32, 8, 6, false); // 48 kHz
        assert_eq!(
            talker_output_format(&[out_stream("In 1", sink, 0)], Some(96_000), 0),
            Err(TalkerFormatError::RateMismatch {
                audio_unit_hz: 96_000,
                stream_hz: 48_000
            })
        );
    }

    #[test]
    fn an_unknown_audio_unit_rate_skips_the_cross_check_rather_than_inventing_one() {
        let sink = aaf_spf(5, 32, 8, 6, false);
        assert_eq!(
            talker_output_format(&[out_stream("In 1", sink, 0)], None, 0).unwrap(),
            sink
        );
    }

    // --- Stream selection: the real 16R puts CRF at index 0 ----------------

    /// The 16R's actual sink list: `Return CRF` at 0, `Return 1-8`..`Return
    /// 57-64` at 1..=8. Formats are the ones decoded off the hardware.
    fn sixteen_r_sinks() -> Vec<StreamDesc> {
        let crf = StreamFormat(0x0410_6001_0000_bb80);
        let audio = StreamFormat(0x0205_0220_0200_6000);
        let mut v = vec![out_stream("Return CRF", crf, 3)];
        for n in 0..8 {
            v.push(out_stream(
                &format!("Return {}-{}", n * 8 + 1, n * 8 + 8),
                audio,
                3,
            ));
        }
        v
    }

    /// A `unique_id` default of 0 bound the media-clock stream, so the
    /// reservation succeeded and no audio moved. The first AUDIO stream is 1.
    #[test]
    fn first_audio_stream_skips_the_crf_media_clock_at_index_0() {
        let sinks = sixteen_r_sinks();
        assert!(!sinks[0].current_format.is_aaf(), "index 0 must be the CRF");
        assert_eq!(first_audio_stream(&sinks), Some(1));
        assert_ne!(first_audio_stream(&sinks), Some(0));
    }

    /// Derivation against the auto-selected sink yields the 16R's real
    /// configured format, and against the CRF sink refuses outright.
    #[test]
    fn deriving_from_the_selected_sink_matches_the_hardware_format() {
        let sinks = sixteen_r_sinks();
        let uid = first_audio_stream(&sinks).expect("an audio sink");
        assert_eq!(
            talker_output_format(&sinks, Some(48_000), uid).unwrap().0,
            0x0205_0220_0200_6000
        );
        assert_eq!(
            talker_output_format(&sinks, Some(48_000), 0),
            Err(TalkerFormatError::NotAaf {
                listener_uid: 0,
                subtype: sinks[0].current_format.subtype_name(),
            })
        );
    }

    /// A device with no audio stream in this direction must yield nothing, so
    /// the caller refuses instead of falling back to index 0.
    #[test]
    fn a_device_with_only_crf_streams_selects_nothing() {
        let crf = StreamFormat(0x0410_6001_0000_bb80);
        assert_eq!(
            first_audio_stream(&[out_stream("Return CRF", crf, 3)]),
            None
        );
        assert_eq!(first_audio_stream(&[]), None);
    }

    /// An `ut` ("up to") stream is not selectable: its channel count and spf
    /// are maxima, so there is no exact format to transmit.
    #[test]
    fn an_up_to_stream_is_not_selectable() {
        let upto = aaf_spf(5, 32, 8, 6, true);
        let exact = aaf_spf(5, 32, 8, 6, false);
        let streams = vec![
            out_stream("In upto", upto, 3),
            out_stream("In exact", exact, 3),
        ];
        assert_eq!(first_audio_stream(&streams), Some(1));
    }
}
