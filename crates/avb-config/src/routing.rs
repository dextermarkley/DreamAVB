// SPDX-License-Identifier: MIT OR Apache-2.0

//! The user's channel map: which AVB device stream channels land on which
//! DreamAVB Core Audio channels.
//!
//! Two rules from the product, enforced here rather than in callers:
//!
//! 1. **Nothing moves without the user.** No function in this module changes
//!    or removes an existing route as a side effect. Adding a route that would
//!    overlap one already there is refused, never resolved by moving either.
//! 2. **A departed device keeps its channels.** Routes are keyed by the
//!    device's AVDECC entity id, which is stable, and say nothing about
//!    whether the device is present. An absent device's channels are simply
//!    silent; when it returns it lands in the same place.
//!
//! Persisted as `routing.txt` next to `config.txt`, one route per line:
//!
//! ```text
//! # direction=first:entity_id:stream_index:stream_channel:count
//! input=0:0x000a92fffe033ba9:1:0:8
//! ```
//!
//! `first` and `stream_channel` are 0-based. Dependency-free, like the rest of
//! this crate (see Cargo.toml).

use std::fmt;
use std::io;
use std::path::PathBuf;

/// Which side of the DreamAVB device a route is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Direction {
    /// AVB device stream -> DreamAVB input channel (what a DAW records).
    Input,
    /// DreamAVB output channel -> AVB device stream (what a DAW plays).
    Output,
}

impl Direction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Direction::Input => "input",
            Direction::Output => "output",
        }
    }
}

/// One contiguous run of channels between a device stream and DreamAVB.
///
/// `count` channels starting at the stream's `stream_channel` map onto
/// DreamAVB channels starting at `first`. A whole 8-channel stream is
/// `stream_channel = 0, count = 8`; a single channel is `count = 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    pub direction: Direction,
    /// First DreamAVB channel, 0-based.
    pub first: u16,
    /// The device's AVDECC entity id (EUI-64).
    pub entity_id: u64,
    /// The device's STREAM_OUTPUT index (input routes) or STREAM_INPUT index
    /// (output routes), as its AEM numbers them.
    pub stream_index: u16,
    /// First channel within that stream, 0-based.
    pub stream_channel: u16,
    pub count: u16,
}

impl Route {
    /// DreamAVB channels this route occupies, as a half-open range.
    pub fn channels(&self) -> std::ops::Range<u32> {
        let f = u32::from(self.first);
        f..f + u32::from(self.count)
    }

    fn overlaps(&self, other: &Route) -> bool {
        if self.direction != other.direction {
            return false;
        }
        let (a, b) = (self.channels(), other.channels());
        a.start < b.end && b.start < a.end
    }

    /// Whether this route claims any of the same device-stream channels as
    /// `other` (the same source cannot feed two places).
    fn same_source(&self, other: &Route) -> bool {
        if self.direction != other.direction
            || self.entity_id != other.entity_id
            || self.stream_index != other.stream_index
        {
            return false;
        }
        let a =
            u32::from(self.stream_channel)..u32::from(self.stream_channel) + u32::from(self.count);
        let b = u32::from(other.stream_channel)
            ..u32::from(other.stream_channel) + u32::from(other.count);
        a.start < b.end && b.start < a.end
    }

    fn to_line(self) -> String {
        format!(
            "{}={}:0x{:016x}:{}:{}:{}",
            self.direction.as_str(),
            self.first,
            self.entity_id,
            self.stream_index,
            self.stream_channel,
            self.count
        )
    }

    fn parse_value(direction: Direction, v: &str) -> Option<Route> {
        let mut it = v.trim().split(':');
        let first = it.next()?.trim().parse().ok()?;
        let e = it.next()?.trim();
        let entity_id = u64::from_str_radix(e.strip_prefix("0x").unwrap_or(e), 16).ok()?;
        let stream_index = it.next()?.trim().parse().ok()?;
        let stream_channel = it.next()?.trim().parse().ok()?;
        let count: u16 = it.next()?.trim().parse().ok()?;
        if it.next().is_some() || count == 0 {
            return None;
        }
        Some(Route {
            direction,
            first,
            entity_id,
            stream_index,
            stream_channel,
            count,
        })
    }
}

/// Why a route was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteError {
    /// Zero channels.
    Empty,
    /// Runs past the DreamAVB device's channel count.
    OutOfRange { width: u16 },
    /// Would share DreamAVB channels with an existing route. Nothing is moved
    /// to make room: the user resolves it.
    Overlaps(Route),
    /// The same device-stream channels are already routed elsewhere.
    SourceAlreadyRouted(Route),
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RouteError::Empty => f.write_str("a route must cover at least one channel"),
            RouteError::OutOfRange { width } => {
                write!(f, "route runs past the device's {width} channels")
            }
            RouteError::Overlaps(r) => write!(
                f,
                "{} channels {}-{} are already used",
                r.direction.as_str(),
                r.first + 1,
                r.first + r.count
            ),
            RouteError::SourceAlreadyRouted(r) => write!(
                f,
                "that stream's channels already go to {} {}-{}",
                r.direction.as_str(),
                r.first + 1,
                r.first + r.count
            ),
        }
    }
}

/// The whole map for a DreamAVB device of a fixed width.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoutingMap {
    routes: Vec<Route>,
    /// Display names remembered for devices (`None` key) and their streams,
    /// so a mapped device that is not connected still shows by name. Labels
    /// only: nothing routes by them.
    names: std::collections::BTreeMap<(u64, Option<u16>), String>,
}

impl RoutingMap {
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }

    /// Remember a display name for a device (`stream = None`) or one of its
    /// streams. Empty names are ignored.
    pub fn set_name(&mut self, entity_id: u64, stream: Option<u16>, name: &str) {
        let n = clean_name(name);
        if !n.is_empty() {
            self.names.insert((entity_id, stream), n);
        }
    }

    pub fn name(&self, entity_id: u64, stream: Option<u16>) -> Option<&str> {
        self.names.get(&(entity_id, stream)).map(String::as_str)
    }

    /// Add a route. Refused, with nothing changed, if it is empty, too wide,
    /// overlaps an existing route, or re-routes an already-routed source.
    pub fn add(&mut self, r: Route, width: u16) -> Result<(), RouteError> {
        if r.count == 0 {
            return Err(RouteError::Empty);
        }
        if r.channels().end > u32::from(width) {
            return Err(RouteError::OutOfRange { width });
        }
        if let Some(o) = self.routes.iter().find(|o| o.overlaps(&r)) {
            return Err(RouteError::Overlaps(*o));
        }
        if let Some(o) = self.routes.iter().find(|o| o.same_source(&r)) {
            return Err(RouteError::SourceAlreadyRouted(*o));
        }
        self.routes.push(r);
        Ok(())
    }

    /// Remove exactly this route (an explicit user action). Returns whether it
    /// was present. Never touches any other route.
    pub fn remove(&mut self, r: &Route) -> bool {
        let before = self.routes.len();
        self.routes.retain(|o| o != r);
        self.routes.len() != before
    }

    /// The DreamAVB channel a device stream channel lands on, if routed.
    pub fn channel_for(
        &self,
        direction: Direction,
        entity_id: u64,
        stream_index: u16,
        stream_channel: u16,
    ) -> Option<u16> {
        self.routes.iter().find_map(|r| {
            let in_range = r.direction == direction
                && r.entity_id == entity_id
                && r.stream_index == stream_index
                && stream_channel >= r.stream_channel
                && u32::from(stream_channel) < u32::from(r.stream_channel) + u32::from(r.count);
            in_range.then(|| r.first + (stream_channel - r.stream_channel))
        })
    }

    /// Whether any route mentions this device, i.e. it has been mapped before.
    pub fn knows(&self, entity_id: u64) -> bool {
        self.routes.iter().any(|r| r.entity_id == entity_id)
    }

    /// A PROPOSAL for placing a new device's streams in free channels, for the
    /// user to accept or edit. Pure: it does not modify the map.
    ///
    /// `streams` is `(stream_index, channels)` in the device's own order. Each
    /// stream is placed whole, at the lowest free run that fits it, after the
    /// existing routes are left exactly where they are. Streams that do not
    /// fit are omitted (the caller tells the user).
    pub fn propose(
        &self,
        direction: Direction,
        entity_id: u64,
        streams: &[(u16, u16)],
        width: u16,
    ) -> Vec<Route> {
        let mut used: Vec<bool> = vec![false; usize::from(width)];
        for r in self.routes.iter().filter(|r| r.direction == direction) {
            for c in r.channels() {
                if let Some(u) = used.get_mut(c as usize) {
                    *u = true;
                }
            }
        }
        let mut out = Vec::new();
        for &(stream_index, n) in streams {
            if n == 0 {
                continue;
            }
            let n = usize::from(n);
            let mut start = None;
            let mut run = 0usize;
            for (i, &u) in used.iter().enumerate() {
                if u {
                    run = 0;
                } else {
                    run += 1;
                    if run == n {
                        start = Some(i + 1 - n);
                        break;
                    }
                }
            }
            let Some(s) = start else { continue };
            for u in used.iter_mut().skip(s).take(n) {
                *u = true;
            }
            out.push(Route {
                direction,
                first: s as u16,
                entity_id,
                stream_index,
                stream_channel: 0,
                count: n as u16,
            });
        }
        out
    }

    /// Parse the file body. Malformed or conflicting lines are skipped with a
    /// complaint; nothing is renumbered to make a bad file "fit".
    pub fn parse(text: &str, width: u16) -> (Self, Vec<String>) {
        let mut map = Self::default();
        let mut warnings = Vec::new();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                warnings.push(format!("line {}: no '=' in {line:?}", i + 1));
                continue;
            };
            if k.trim() == "name" {
                // name=0xENTITY:-:text (device) or name=0xENTITY:INDEX:text
                let mut it = v.splitn(3, ':');
                let (Some(e), Some(s), Some(text)) = (it.next(), it.next(), it.next()) else {
                    warnings.push(format!("line {}: malformed name {:?}", i + 1, v.trim()));
                    continue;
                };
                let e = e.trim();
                let Ok(entity) = u64::from_str_radix(e.strip_prefix("0x").unwrap_or(e), 16) else {
                    warnings.push(format!("line {}: malformed name {:?}", i + 1, v.trim()));
                    continue;
                };
                let stream = match s.trim() {
                    "-" => None,
                    n => match n.parse() {
                        Ok(n) => Some(n),
                        Err(_) => {
                            warnings.push(format!("line {}: malformed name {:?}", i + 1, v.trim()));
                            continue;
                        }
                    },
                };
                map.set_name(entity, stream, text);
                continue;
            }
            let direction = match k.trim() {
                "input" => Direction::Input,
                "output" => Direction::Output,
                other => {
                    warnings.push(format!("line {}: unknown key {other:?}", i + 1));
                    continue;
                }
            };
            match Route::parse_value(direction, v) {
                Some(r) => {
                    if let Err(e) = map.add(r, width) {
                        warnings.push(format!("line {}: ignored: {e}", i + 1));
                    }
                }
                None => warnings.push(format!("line {}: malformed route {:?}", i + 1, v.trim())),
            }
        }
        (map, warnings)
    }

    /// Serialize, routes in a stable order (direction, then channel).
    pub fn to_text(&self) -> String {
        let mut rs = self.routes.clone();
        rs.sort_by_key(|r| (r.direction, r.first));
        let mut s = String::from(
            "# DreamAVB channel map. Written by the companion app; read by avb-daemon.\n\
             # direction=first:entity_id:stream_index:stream_channel:count (0-based)\n",
        );
        for r in rs {
            s.push_str(&r.to_line());
            s.push('\n');
        }
        for ((e, st), n) in &self.names {
            let st = st.map_or_else(|| "-".to_string(), |i| i.to_string());
            s.push_str(&format!("name=0x{e:016x}:{st}:{n}\n"));
        }
        s
    }
}

fn clean_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Channels in one block of the app's channel-map screen: one whole AVB
/// stream. The grid is `width / BLOCK_CHANNELS` blocks.
pub const BLOCK_CHANNELS: u16 = 8;

/// What one input block carries: a device's stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Assign {
    pub entity_id: u64,
    pub stream_index: u16,
}

impl RoutingMap {
    /// The input grid: entry `b` is what DreamAVB inputs
    /// `b*8+1 .. b*8+8` carry, from whole-stream input routes that sit on a
    /// block boundary. Routes the grid cannot show are left out of it and kept
    /// as they are by [`with_blocks`](Self::with_blocks).
    pub fn input_blocks(&self, width: u16) -> Vec<Option<Assign>> {
        let mut out = vec![None; usize::from(width / BLOCK_CHANNELS)];
        for r in self.routes.iter().filter(|r| is_block_route(r)) {
            if let Some(slot) = out.get_mut(usize::from(r.first / BLOCK_CHANNELS)) {
                *slot = Some(Assign {
                    entity_id: r.entity_id,
                    stream_index: r.stream_index,
                });
            }
        }
        out
    }

    /// A new map with the input grid replaced by `blocks`. Every route the
    /// grid does not show (outputs, partial or unaligned hand-edited routes)
    /// and every remembered name is carried over unchanged. Refused, with
    /// nothing returned, if the result would overlap or route one stream to
    /// two blocks.
    pub fn with_blocks(&self, blocks: &[Option<Assign>], width: u16) -> Result<Self, RouteError> {
        let mut next = RoutingMap {
            routes: Vec::new(),
            names: self.names.clone(),
        };
        for r in self.routes.iter().filter(|r| !is_block_route(r)) {
            next.add(*r, width)?;
        }
        for (b, a) in blocks.iter().enumerate() {
            let Some(a) = a else { continue };
            next.add(
                Route {
                    direction: Direction::Input,
                    first: (b as u16).saturating_mul(BLOCK_CHANNELS),
                    entity_id: a.entity_id,
                    stream_index: a.stream_index,
                    stream_channel: 0,
                    count: BLOCK_CHANNELS,
                },
                width,
            )?;
        }
        Ok(next)
    }
}

fn is_block_route(r: &Route) -> bool {
    r.direction == Direction::Input
        && r.stream_channel == 0
        && r.count == BLOCK_CHANNELS
        && r.first.is_multiple_of(BLOCK_CHANNELS)
}

/// `~/Library/Application Support/DreamAVB/routing.txt`.
pub fn routing_path() -> Option<PathBuf> {
    let mut p = crate::config_path()?;
    p.set_file_name("routing.txt");
    Some(p)
}

impl RoutingMap {
    /// Load the map; a missing file is an empty map (first run).
    pub fn load(width: u16) -> (Self, Vec<String>) {
        let Some(path) = routing_path() else {
            return (
                Self::default(),
                vec!["HOME is unset; no channel map".into()],
            );
        };
        match std::fs::read_to_string(&path) {
            Ok(t) => Self::parse(&t, width),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (Self::default(), Vec::new()),
            Err(e) => (
                Self::default(),
                vec![format!("could not read {}: {e}", path.display())],
            ),
        }
    }

    /// Like [`load`](Self::load), but `None` when no map has ever been saved.
    ///
    /// The distinction matters: no file means the user has never mapped
    /// anything, so the daemon keeps the pre-map behaviour (see
    /// [`input_placement`]); an EMPTY saved map means the user cleared it, and
    /// nothing is bound.
    pub fn load_if_present(width: u16) -> (Option<Self>, Vec<String>) {
        let Some(path) = routing_path() else {
            return (None, vec!["HOME is unset; no channel map".into()]);
        };
        match std::fs::read_to_string(&path) {
            Ok(t) => {
                let (m, w) = Self::parse(&t, width);
                (Some(m), w)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (None, Vec::new()),
            Err(e) => (
                None,
                vec![format!("could not read {}: {e}", path.display())],
            ),
        }
    }

    /// Write atomically (temp file + rename), like `Config::save`.
    pub fn save(&self) -> io::Result<PathBuf> {
        let path = routing_path()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is unset"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("txt.tmp");
        std::fs::write(&tmp, self.to_text())?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }
}

/// Which of one device's streams to bind on the input side, and the first
/// DreamAVB input each lands on.
///
/// `sources` is the device's bindable streams as `(stream_index, channels)`
/// in its own AEM order. Returns `(stream_index, first_channel)` for the
/// streams to bind.
///
/// - `map` is `None` (the user has never saved a map): the pre-map layout,
///   stream `i` of the list on inputs `i * channels ..`, as far as it fits.
///   This keeps a single-device setup working unchanged.
/// - `map` is `Some`: exactly the streams that have a whole-stream input
///   route for this device. Anything unmapped is not bound, so a new device
///   appears without taking any inputs until the user maps it.
pub fn input_placement(
    map: Option<&RoutingMap>,
    entity_id: u64,
    sources: &[(u16, u16)],
    width: u16,
) -> Vec<(u16, u16)> {
    match map {
        None => {
            let mut out = Vec::new();
            let mut next: u32 = 0;
            for &(idx, ch) in sources {
                if next + u32::from(ch) > u32::from(width) {
                    break;
                }
                out.push((idx, next as u16));
                next += u32::from(ch);
            }
            out
        }
        Some(m) => sources
            .iter()
            .filter_map(|&(idx, ch)| {
                m.routes()
                    .iter()
                    .find(|r| {
                        r.direction == Direction::Input
                            && r.entity_id == entity_id
                            && r.stream_index == idx
                            && r.stream_channel == 0
                            && r.count == ch
                    })
                    .map(|r| (idx, r.first))
            })
            .collect(),
    }
}

/// Which of the devices on the link this run streams with, in order.
///
/// - No saved map: only `chosen` (the pre-map, one-device behaviour).
/// - A saved map: every present device that has an input route, ordered by
///   its lowest mapped input channel. If none of the present devices is
///   mapped, `chosen` alone, so audio still runs while the user maps them.
pub fn session_devices(map: Option<&RoutingMap>, chosen: u64, present: &[u64]) -> Vec<u64> {
    let Some(m) = map else {
        return vec![chosen];
    };
    let mut mapped: Vec<(u16, u64)> = present
        .iter()
        .filter_map(|&e| {
            m.routes()
                .iter()
                .filter(|r| r.direction == Direction::Input && r.entity_id == e)
                .map(|r| r.first)
                .min()
                .map(|f| (f, e))
        })
        .collect();
    if mapped.is_empty() {
        return vec![chosen];
    }
    mapped.sort_unstable();
    mapped.dedup_by_key(|x| x.1);
    mapped.into_iter().map(|(_, e)| e).collect()
}

// Outputs are not mapped here: the daemon offers every DreamAVB output to
// every device as fixed 8-channel streams, and each device picks (its own
// stream selection / routing). This map covers inputs only.

#[cfg(test)]
mod tests {
    use super::*;

    const R16: u64 = 0x000a_92ff_fe03_3ba9;

    fn two_device_map() -> RoutingMap {
        // The user's saved layout: 24R on inputs 1-64, 16R on 65-128.
        let mut m = RoutingMap::default();
        for i in 0..8u16 {
            m.add(whole(i * 8, R24, i + 1), 128).unwrap();
            m.add(whole(64 + i * 8, R16, i + 1), 128).unwrap();
        }
        m
    }

    #[test]
    fn sessions_follow_the_map_and_survive_an_absent_device() {
        let m = two_device_map();
        assert_eq!(session_devices(Some(&m), R16, &[R16, R24]), vec![R24, R16]);
        // 24R not on the link: the 16R streams alone, and keeps its place.
        assert_eq!(session_devices(Some(&m), R16, &[R16]), vec![R16]);
        // No map: only the chosen device.
        assert_eq!(session_devices(None, R16, &[R16, R24]), vec![R16]);
        // A map that names neither: still stream with the chosen one.
        assert_eq!(
            session_devices(Some(&RoutingMap::default()), R16, &[R16]),
            vec![R16]
        );
    }

    #[test]
    fn blocks_round_trip_and_keep_what_the_grid_cannot_show() {
        let mut m = RoutingMap::default();
        m.add(whole(64, R16, 1), 128).unwrap();
        // A hand-edited single-channel route and an output route.
        let odd = Route {
            direction: Direction::Input,
            first: 120,
            entity_id: R16,
            stream_index: 9,
            stream_channel: 3,
            count: 1,
        };
        m.add(odd, 128).unwrap();
        let mut out = whole(0, R16, 1);
        out.direction = Direction::Output;
        m.add(out, 128).unwrap();
        m.set_name(R16, None, "StudioLive 16R");

        let mut blocks = m.input_blocks(128);
        assert_eq!(blocks.len(), 16);
        assert_eq!(
            blocks[8],
            Some(Assign {
                entity_id: R16,
                stream_index: 1
            })
        );
        assert!(blocks[15].is_none(), "the 1-channel route is not a block");

        // The user moves stream 1 from block 8 to block 0 and adds stream 2.
        blocks[8] = None;
        blocks[0] = Some(Assign {
            entity_id: R16,
            stream_index: 1,
        });
        blocks[1] = Some(Assign {
            entity_id: R16,
            stream_index: 2,
        });
        let next = m.with_blocks(&blocks, 128).unwrap();
        assert_eq!(next.channel_for(Direction::Input, R16, 1, 0), Some(0));
        assert_eq!(next.channel_for(Direction::Input, R16, 2, 0), Some(8));
        assert!(next.routes().contains(&odd));
        assert!(next.routes().contains(&out));
        assert_eq!(next.name(R16, None), Some("StudioLive 16R"));
        // The original is untouched until the caller saves the new one.
        assert_eq!(m.channel_for(Direction::Input, R16, 1, 0), Some(64));
    }

    #[test]
    fn a_grid_that_routes_one_stream_twice_is_refused() {
        let blocks = vec![
            Some(Assign {
                entity_id: R16,
                stream_index: 1,
            }),
            Some(Assign {
                entity_id: R16,
                stream_index: 1,
            }),
        ];
        assert!(matches!(
            RoutingMap::default().with_blocks(&blocks, 128),
            Err(RouteError::SourceAlreadyRouted(_))
        ));
    }

    #[test]
    fn names_round_trip_and_cannot_break_lines() {
        let mut m = RoutingMap::default();
        m.add(whole(0, R16, 1), 128).unwrap();
        m.set_name(R16, None, "Studio\nLive: 16R");
        m.set_name(R16, Some(1), "Send 1-8");
        let (back, w) = RoutingMap::parse(&m.to_text(), 128);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(back.name(R16, None), Some("Studio Live: 16R"));
        assert_eq!(back.name(R16, Some(1)), Some("Send 1-8"));
    }

    #[test]
    fn no_saved_map_keeps_the_single_device_layout() {
        let srcs: Vec<(u16, u16)> = (1..=8).map(|i| (i, 8)).collect();
        let p = input_placement(None, R16, &srcs, 128);
        assert_eq!(p.first(), Some(&(1, 0)));
        assert_eq!(p.last(), Some(&(8, 56)));
        assert_eq!(p.len(), 8);
        // Only what fits.
        assert_eq!(input_placement(None, R16, &srcs, 20).len(), 2);
    }

    #[test]
    fn a_saved_map_binds_only_mapped_streams_where_mapped() {
        let mut m = RoutingMap::default();
        m.add(whole(64, R16, 2), 128).unwrap();
        m.add(whole(0, R16, 5), 128).unwrap();
        let srcs: Vec<(u16, u16)> = (1..=8).map(|i| (i, 8)).collect();
        assert_eq!(
            input_placement(Some(&m), R16, &srcs, 128),
            vec![(2, 64), (5, 0)]
        );
        // Another device with no routes binds nothing: it shows up silently.
        assert!(input_placement(Some(&m), 0x99, &srcs, 128).is_empty());
        // An empty saved map (user cleared it) binds nothing either.
        assert!(input_placement(Some(&RoutingMap::default()), R16, &srcs, 128).is_empty());
    }

    #[test]
    fn a_route_whose_width_no_longer_matches_the_stream_is_not_used() {
        let mut m = RoutingMap::default();
        m.add(whole(0, R16, 1), 128).unwrap();
        // The device now reports stream 1 as 4 channels: do not bind it into
        // an 8-channel route.
        assert!(input_placement(Some(&m), R16, &[(1, 4)], 128).is_empty());
    }
    const R24: u64 = 0x000a_92ff_fe04_0001;
    const W: u16 = 128;

    fn whole(first: u16, entity_id: u64, stream_index: u16) -> Route {
        Route {
            direction: Direction::Input,
            first,
            entity_id,
            stream_index,
            stream_channel: 0,
            count: 8,
        }
    }

    #[test]
    fn overlap_is_refused_and_nothing_moves() {
        let mut m = RoutingMap::default();
        m.add(whole(0, R16, 1), W).unwrap();
        let before = m.clone();
        let err = m.add(whole(4, R24, 1), W).unwrap_err();
        assert_eq!(err, RouteError::Overlaps(whole(0, R16, 1)));
        assert_eq!(m, before, "a refused add must not change the map");
    }

    #[test]
    fn a_source_cannot_feed_two_places() {
        let mut m = RoutingMap::default();
        m.add(whole(0, R16, 1), W).unwrap();
        assert!(matches!(
            m.add(whole(8, R16, 1), W),
            Err(RouteError::SourceAlreadyRouted(_))
        ));
        // A different stream of the same device is fine.
        m.add(whole(8, R16, 2), W).unwrap();
    }

    #[test]
    fn width_is_enforced() {
        let mut m = RoutingMap::default();
        assert_eq!(
            m.add(whole(124, R16, 1), W),
            Err(RouteError::OutOfRange { width: W })
        );
        m.add(whole(120, R16, 1), W).unwrap();
    }

    #[test]
    fn inputs_and_outputs_do_not_collide() {
        let mut m = RoutingMap::default();
        m.add(whole(0, R16, 1), W).unwrap();
        let mut out = whole(0, R16, 1);
        out.direction = Direction::Output;
        m.add(out, W).unwrap();
    }

    #[test]
    fn channel_lookup_per_channel_and_whole_stream() {
        let mut m = RoutingMap::default();
        m.add(whole(16, R16, 1), W).unwrap();
        // 24R stream 3, channel 5 only, on DreamAVB channel 0.
        m.add(
            Route {
                direction: Direction::Input,
                first: 0,
                entity_id: R24,
                stream_index: 3,
                stream_channel: 5,
                count: 1,
            },
            W,
        )
        .unwrap();
        assert_eq!(m.channel_for(Direction::Input, R16, 1, 0), Some(16));
        assert_eq!(m.channel_for(Direction::Input, R16, 1, 7), Some(23));
        assert_eq!(m.channel_for(Direction::Input, R24, 3, 5), Some(0));
        assert_eq!(m.channel_for(Direction::Input, R24, 3, 4), None);
        assert_eq!(m.channel_for(Direction::Output, R16, 1, 0), None);
    }

    #[test]
    fn a_departed_device_keeps_its_channels() {
        // The map has no notion of presence: the 16R's routes are untouched
        // by anything that happens to the 24R, and vice versa.
        let mut m = RoutingMap::default();
        m.add(whole(0, R16, 1), W).unwrap();
        m.add(whole(8, R24, 1), W).unwrap();
        // "16R left": nothing in the map changes, and a proposal for a third
        // device must not take its channels.
        let p = m.propose(Direction::Input, 0x1234, &[(1, 8)], W);
        assert_eq!(
            p,
            vec![Route {
                entity_id: 0x1234,
                ..whole(16, 0x1234, 1)
            }]
        );
        assert_eq!(m.channel_for(Direction::Input, R16, 1, 0), Some(0));
    }

    #[test]
    fn proposal_fills_gaps_without_moving_anything() {
        let mut m = RoutingMap::default();
        m.add(whole(8, R16, 1), W).unwrap();
        let before = m.clone();
        let p = m.propose(Direction::Input, R24, &[(1, 8), (2, 8), (3, 4)], W);
        assert_eq!(
            p.iter()
                .map(|r| (r.stream_index, r.first, r.count))
                .collect::<Vec<_>>(),
            vec![(1, 0, 8), (2, 16, 8), (3, 24, 4)]
        );
        assert_eq!(m, before, "propose must be pure");
    }

    #[test]
    fn proposal_omits_what_does_not_fit() {
        let m = RoutingMap::default();
        let p = m.propose(Direction::Input, R24, &[(1, 8), (2, 8)], 12);
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn text_round_trips() {
        let mut m = RoutingMap::default();
        m.add(whole(8, R16, 2), W).unwrap();
        m.add(whole(0, R16, 1), W).unwrap();
        let text = m.to_text();
        assert!(text.contains("input=0:0x000a92fffe033ba9:1:0:8\n"));
        let (back, warnings) = RoutingMap::parse(&text, W);
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut a = m.routes().to_vec();
        let mut b = back.routes().to_vec();
        a.sort_by_key(|r| r.first);
        b.sort_by_key(|r| r.first);
        assert_eq!(a, b);
    }

    #[test]
    fn a_bad_file_is_not_renumbered_to_fit() {
        let text = "input=0:0x1:1:0:8\ninput=4:0x2:1:0:8\nbogus\ninput=1:x\n";
        let (m, w) = RoutingMap::parse(text, W);
        assert_eq!(
            m.routes().len(),
            1,
            "the overlapping line is dropped, not moved"
        );
        assert_eq!(m.routes()[0].first, 0);
        assert_eq!(w.len(), 3, "{w:?}");
    }
}
