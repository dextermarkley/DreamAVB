//! The ring consumer (design §10.6): reads committed frames into a caller
//! buffer, skipping past an overrun and counting underruns. Single consumer
//! (the `rec` thread in Phase 1a, the AudioServerPlugIn in Phase 1b).

use crate::layout::counter;
use crate::ring::AudioRing;

/// The ring consumer. `read_frame` is the next absolute frame to read.
pub struct Consumer<'a> {
    ring: &'a AudioRing,
    read_frame: u64,
}

impl<'a> Consumer<'a> {
    pub fn new(ring: &'a AudioRing) -> Self {
        let read_frame = ring.read_frame();
        Self { ring, read_frame }
    }

    /// Build a consumer at an explicit absolute read position (Phase 1b: the
    /// plug-in seeks to the live write frontier on StartIO so it plays current
    /// audio, not the whole committed backlog).
    pub fn with_read_frame(ring: &'a AudioRing, read_frame: u64) -> Self {
        Self { ring, read_frame }
    }

    /// The ring's current committed write frontier (absolute frame).
    pub fn write_frame(&self) -> u64 {
        self.ring.write_frame()
    }

    pub fn read_frame(&self) -> u64 {
        self.read_frame
    }

    /// Frames available to read right now (committed minus consumed).
    pub fn available(&self) -> u64 {
        self.ring.write_frame().saturating_sub(self.read_frame)
    }

    /// Copy up to `max_frames` committed frames into `out` (which must hold
    /// `max_frames × frame_bytes` bytes). Returns the number of frames copied.
    /// On overrun (`read_frame < write_frame − capacity`) the consumer first
    /// skips to `write_frame − capacity` and counts `consumer_skips` (§10.6).
    /// Zero returned means underrun (nothing committed yet), counted.
    pub fn read_into(&mut self, out: &mut [u8], max_frames: usize) -> usize {
        let write = self.ring.write_frame();
        let cap = self.ring.capacity_frames();
        // Overrun: we fell more than a full ring behind.
        if write.saturating_sub(self.read_frame) > cap {
            let skip_to = write - cap;
            self.ring.add_counter(counter::CONSUMER_SKIPS, 1);
            self.read_frame = skip_to;
        }
        let avail = write.saturating_sub(self.read_frame);
        if avail == 0 {
            self.ring.add_counter(counter::UNDERRUNS, 1);
            return 0;
        }
        let frame_bytes = self.ring.frame_bytes();
        let n = (avail as usize)
            .min(max_frames)
            .min(out.len() / frame_bytes.max(1));
        let mut copied = 0usize;
        let mut f = self.read_frame;
        while copied < n {
            // Copy up to the ring-wrap boundary in one span.
            let slot = f % cap;
            let span = ((cap - slot) as usize).min(n - copied);
            let src = self.ring.frames(f, span);
            let dst_off = copied * frame_bytes;
            if let Some(dst) = out.get_mut(dst_off..dst_off + span * frame_bytes) {
                dst.copy_from_slice(src);
            }
            f += span as u64;
            copied += span;
        }
        self.read_frame += copied as u64;
        self.ring.store_read_frame(self.read_frame);
        copied
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::layout::{RingConfig, StreamSlot};
    use crate::producer::{Producer, SampleFormat};
    use crate::AudioRing;

    fn ring() -> AudioRing {
        let cfg = RingConfig {
            channels: 2,
            valid_bits: 24,
            nominal_rate_hz: 48_000,
            capacity_frames: 16, // tiny for overrun testing
            streams: vec![StreamSlot {
                stream_id: 1,
                first_channel: 0,
                channels: 2,
                bit_depth: 24,
            }],
            channel_map: vec![(0, 0), (0, 1)],
        };
        AudioRing::new_anonymous(&cfg).unwrap()
    }

    #[test]
    fn produce_then_consume_roundtrip() {
        let r = ring();
        let mut p = Producer::new(&r, &[(0, 2, 24)], 192, 192);
        // One packet of 2 frames, 2 ch INT_32BIT, values f(frame,ch).
        let data: Vec<u8> = [0x11223344u32, 0x55667788, 0x0A0B0C0D, 0x7FFFFFFF]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect();
        assert!(p.place(0, 0, 2, SampleFormat::Int32, &data));
        p.commit(false, None);
        assert_eq!(r.write_frame(), 2);
        let mut c = Consumer::new(&r);
        assert_eq!(c.available(), 2);
        let mut out = vec![0u8; 2 * 8];
        assert_eq!(c.read_into(&mut out, 2), 2);
        // Little-endian of the big-endian wire values.
        assert_eq!(&out[0..4], &0x11223344u32.to_le_bytes());
        assert_eq!(&out[4..8], &0x55667788u32.to_le_bytes());
        assert_eq!(&out[12..16], &0x7FFFFFFFu32.to_le_bytes());
        // Nothing left -> underrun.
        assert_eq!(c.read_into(&mut out, 2), 0);
        assert_eq!(r.counter(counter::UNDERRUNS), 1);
    }

    #[test]
    fn overrun_skips() {
        let r = ring(); // capacity 16
        let mut p = Producer::new(&r, &[(0, 2, 24)], 192, 192);
        let data = vec![0u8; 2 * 2 * 4];
        // Write 20 frames without the consumer reading -> overrun past 16.
        for i in 0..10 {
            p.place(0, i * 2, 2, SampleFormat::Int32, &data);
            p.commit(false, None); // commit each tick, as the audio loop does
        }
        assert_eq!(r.write_frame(), 20);
        let mut c = Consumer::new(&r);
        let mut out = vec![0u8; 32 * 8];
        c.read_into(&mut out, 32);
        // Consumer skipped to write - capacity = 4.
        assert!(c.read_frame() >= 4);
        assert_eq!(r.counter(counter::CONSUMER_SKIPS), 1);
    }
}
