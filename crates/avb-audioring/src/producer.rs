//! The ring producer (design §10.6): placement writes, zero-fill, frontier
//! commit with the lagging-stream and all-silent rules, gap logging, overrun
//! counting. Never blocks (RING-1). Single producer (the `audio` thread).

use crate::layout::{self, counter, field};
use crate::ring::AudioRing;

/// AAF sample formats the producer converts (design §10.6). Mirrors the
/// `avb_avtp::aaf::format` codes the daemon passes in; kept local so this
/// crate does not depend on `avb-avtp`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    /// INT_32BIT: big-endian i32, MSB-aligned (depth < 32 stays MSB-aligned).
    Int32,
    /// INT_24BIT: 3-byte big-endian, stored as `i32 << 8`.
    Int24,
}

impl SampleFormat {
    pub fn wire_bytes(self) -> usize {
        match self {
            SampleFormat::Int32 => 4,
            SampleFormat::Int24 => 3,
        }
    }
}

/// A reason recorded in the gap log (§10.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GapReason {
    LaggingStream = 1,
    AllSilent = 2,
}

/// Per-stream producer state (the frontier bookkeeping, §10.6).
#[derive(Clone, Copy, Debug)]
struct StreamState {
    first_channel: usize,
    channels: usize,
    /// Highest absolute frame written +1 for this stream.
    written_upto: u64,
    active: bool,
}

/// The ring producer. Holds `&AudioRing` plus per-stream frontier state and
/// `zeroed_upto` (everything below is zero-filled). Never allocates after
/// construction.
pub struct Producer<'a> {
    ring: &'a AudioRing,
    streams: Vec<StreamState>,
    /// Everything in `[0, zeroed_upto)` has been zero-filled or written.
    zeroed_upto: u64,
    lag_frames: u64,
    silent_lag: u64,
    /// Fixed at construction: the committed frontier never regresses.
    committed: u64,
}

impl<'a> Producer<'a> {
    /// Build a producer over `ring`. `lag_frames` and `silent_lag` come from
    /// `avb_mediaclock::RateConsts` (§10.5). `streams[i]` = (first_channel,
    /// channels, bit_depth).
    pub fn new(
        ring: &'a AudioRing,
        streams: &[(usize, usize, u32)],
        lag_frames: u64,
        silent_lag: u64,
    ) -> Self {
        let streams = streams
            .iter()
            .map(|&(first_channel, channels, _bit_depth)| StreamState {
                first_channel,
                channels,
                written_upto: 0,
                active: true,
            })
            .collect();
        Self {
            ring,
            streams,
            zeroed_upto: 0,
            lag_frames,
            silent_lag,
            committed: 0,
        }
    }

    pub fn committed(&self) -> u64 {
        self.committed
    }

    /// Zero-fill `[zeroed_upto, up_to)` so missing data reads as zero (§10.6).
    fn zero_fill_to(&mut self, up_to: u64) {
        if up_to <= self.zeroed_upto {
            return;
        }
        let mut f = self.zeroed_upto;
        let cap = self.ring.capacity_frames();
        while f < up_to {
            // Zero up to the ring-wrap boundary in one span.
            let slot = f % cap;
            let span = (cap - slot).min(up_to - f);
            // SAFETY: single producer; span does not wrap.
            let buf = unsafe { self.ring.frames_mut(f, span as usize) };
            buf.fill(0);
            f += span;
        }
        self.ring
            .add_counter(counter::ZERO_FILL_FRAMES, up_to - self.zeroed_upto);
        self.zeroed_upto = up_to;
    }

    /// Place one packet of stream `stream_idx` at absolute ring frame `f`
    /// (from the media clock), `spf` frames of interleaved samples in `data`
    /// (big-endian wire bytes). Returns false if the packet was dropped
    /// (late / placement reject). Never blocks (§10.6 RING-1).
    pub fn place(
        &mut self,
        stream_idx: usize,
        f: i128,
        spf: usize,
        fmt: SampleFormat,
        data: &[u8],
    ) -> bool {
        let st = match self.streams.get(stream_idx) {
            Some(s) => *s,
            None => return false,
        };
        let cap = self.ring.capacity_frames();
        // Late: already behind the committed frontier.
        if f < i128::from(self.committed) {
            self.ring.add_counter(counter::LATE_DROPS, 1);
            return false;
        }
        // Too far ahead: placement reject.
        if f > i128::from(self.committed) + i128::from(cap / 2) {
            self.ring.add_counter(counter::PLACEMENT_REJECTS, 1);
            return false;
        }
        let f = f as u64;
        let end = f + spf as u64;
        // Zero-fill the gap up to this packet before writing it.
        self.zero_fill_to(f);
        self.write_samples(&st, f, spf, fmt, data);
        if let Some(s) = self.streams.get_mut(stream_idx) {
            s.written_upto = s.written_upto.max(end);
            s.active = true;
        }
        if end > self.zeroed_upto {
            self.zeroed_upto = end;
        }
        self.ring.store_u64(
            field::STREAMS + stream_idx * layout::STREAM_RECORD_SIZE + layout::stream::LAST_FRAME,
            end,
        );
        true
    }

    fn write_samples(&self, st: &StreamState, f: u64, spf: usize, fmt: SampleFormat, data: &[u8]) {
        let wire = fmt.wire_bytes();
        let ch = st.channels;
        let base_ch = st.first_channel * layout::BYTES_PER_SAMPLE;
        for frame in 0..spf as u64 {
            let abs = f + frame;
            // SAFETY: single producer; one frame never wraps.
            let out = unsafe { self.ring.frames_mut(abs, 1) };
            for c in 0..ch {
                let src = (frame as usize * ch + c) * wire;
                let dstb = base_ch + c * layout::BYTES_PER_SAMPLE;
                let sample = match fmt {
                    SampleFormat::Int32 => read_be_i32(data, src),
                    SampleFormat::Int24 => read_be_i24_shifted(data, src),
                };
                if let Some(slot) = out.get_mut(dstb..dstb + 4) {
                    slot.copy_from_slice(&sample.to_le_bytes());
                }
            }
        }
    }

    /// Advance the committed frontier to the minimum over active streams'
    /// `written_upto`, applying the lagging-stream rule. `all_silent_now` and
    /// `silent_frontier` drive the all-silent rule (§10.6). Returns the new
    /// committed frontier. Release-stores `write_frame`.
    pub fn commit(&mut self, all_silent_now: bool, silent_frontier: Option<u64>) -> u64 {
        if all_silent_now {
            if let Some(target) = silent_frontier {
                let target = target.saturating_sub(self.silent_lag);
                if target > self.committed {
                    self.zero_fill_to(target);
                    self.commit_to(target);
                }
            }
            return self.committed;
        }
        let furthest = self
            .streams
            .iter()
            .filter(|s| s.active)
            .map(|s| s.written_upto)
            .max()
            .unwrap_or(self.committed);
        // Minimum over active streams, treating a lagging stream as gapped.
        let mut frontier = furthest;
        for (i, s) in self.streams.iter().enumerate() {
            if !s.active {
                continue;
            }
            if furthest.saturating_sub(s.written_upto) > self.lag_frames {
                // Lagging: it is passed, its span logged as a gap.
                let gap = furthest.saturating_sub(s.written_upto);
                self.log_gap(i, s.written_upto, gap, GapReason::LaggingStream);
            } else {
                frontier = frontier.min(s.written_upto);
            }
        }
        if frontier > self.committed {
            self.zero_fill_to(frontier);
            self.commit_to(frontier);
        }
        self.committed
    }

    fn commit_to(&mut self, frontier: u64) {
        let read = self.ring.read_frame();
        let cap = self.ring.capacity_frames();
        if frontier.saturating_sub(read) > cap {
            // Overrun: still commit (never block), count the lost frames.
            self.ring
                .add_counter(counter::OVERRUN_FRAMES, frontier - read - cap);
            self.ring.add_counter(counter::OVERRUN_EVENTS, 1);
        }
        self.committed = frontier;
        self.ring.store_write_frame(frontier);
    }

    fn log_gap(&self, stream_idx: usize, start_frame: u64, frames: u64, reason: GapReason) {
        self.ring.add_counter(counter::GAP_FRAMES_TOTAL, frames);
        // Per-stream gap counters.
        let b = field::STREAMS + stream_idx * layout::STREAM_RECORD_SIZE;
        let gf = self.ring.load_u64(b + layout::stream::GAP_FRAMES);
        self.ring
            .store_u64(b + layout::stream::GAP_FRAMES, gf + frames);
        let ge = self.ring.load_u64(b + layout::stream::GAP_EVENTS);
        self.ring.store_u64(b + layout::stream::GAP_EVENTS, ge + 1);
        // Ring gap log (head + entries).
        let head_off = field::GAP_LOG;
        let head = self.ring.load_u64(head_off);
        let idx = (head as usize) % layout::GAP_LOG_ENTRIES;
        let eb = head_off + 8 + idx * layout::GAP_LOG_ENTRY_SIZE;
        self.ring.store_u64(eb, start_frame);
        self.ring.store_u32(eb + 8, frames as u32);
        self.ring.store_u32(
            eb + 12,
            u32::from(stream_idx as u16) | (u32::from(reason as u16) << 16),
        );
        self.ring.store_u64(head_off, head.wrapping_add(1));
    }

    pub fn set_active(&mut self, stream_idx: usize, active: bool) {
        if let Some(s) = self.streams.get_mut(stream_idx) {
            s.active = active;
        }
    }
}

/// Read a big-endian i32 at `off`, 0 if out of range (MSB-aligned, depth < 32
/// stays MSB-aligned — the wire value is used verbatim).
fn read_be_i32(data: &[u8], off: usize) -> i32 {
    match data
        .get(off..off + 4)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
    {
        Some(b) => i32::from_be_bytes(b),
        None => 0,
    }
}

/// Read a 3-byte big-endian sample and shift to `i32 << 8` (§10.6 INT_24BIT).
fn read_be_i24_shifted(data: &[u8], off: usize) -> i32 {
    match data
        .get(off..off + 3)
        .and_then(|b| <[u8; 3]>::try_from(b).ok())
    {
        Some([h, m, l]) => {
            let v = (i32::from(h as i8) << 16) | (i32::from(m) << 8) | i32::from(l);
            v << 8
        }
        None => 0,
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn be_sample_conversions() {
        // INT_32BIT: verbatim big-endian value.
        assert_eq!(read_be_i32(&[0x80, 0x00, 0x00, 0x00], 0), i32::MIN);
        assert_eq!(read_be_i32(&[0x7F, 0xFF, 0xFF, 0xFF], 0), i32::MAX);
        assert_eq!(read_be_i32(&[0x12, 0x34, 0x56, 0x78], 0), 0x1234_5678);
        // INT_24BIT: 0x800000 -> (i32)0xFFFF8000... << 8 keeps MSB alignment.
        assert_eq!(
            read_be_i24_shifted(&[0x80, 0x00, 0x00], 0),
            (-0x80_0000i32) << 8
        );
        assert_eq!(read_be_i24_shifted(&[0x00, 0x00, 0x01], 0), 1 << 8);
        assert_eq!(read_be_i24_shifted(&[0x7F, 0xFF, 0xFF], 0), 0x7F_FFFF << 8);
    }
}
