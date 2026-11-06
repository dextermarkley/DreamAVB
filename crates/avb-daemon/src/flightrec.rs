// SPDX-License-Identifier: MIT OR Apache-2.0

//! Control-frame flight recorder.
//!
//! The dext owns the NIC, so nothing can capture the link while the daemon
//! runs. When a peer's control plane goes silent mid-run (the 16R stopped ADP,
//! AECP and Sync together on the Broadcom path), the question is what it
//! received from us, and what it last sent, just before. This keeps the last
//! [`FLIGHT_RECORDER_FRAMES`] control frames in each direction in memory and
//! writes them to a pcap on demand. The source MAC tells the directions apart.
//!
//! Audio and CRF stream frames are not recorded: at 64 000 frames/s they
//! would push every control frame out of the buffer in milliseconds.

use std::collections::VecDeque;
use std::io::Write;
use std::path::Path;

/// Frames kept (both directions together). At ~100 control frames/s each way
/// that is about 20 s of history, ~0.3 MB.
pub const FLIGHT_RECORDER_FRAMES: usize = 4096;

/// AVTP ethertype and the stream subtypes that are NOT control traffic.
const ETHERTYPE_AVTP: u16 = 0x22F0;
const SUBTYPE_AAF: u8 = 0x02;
const SUBTYPE_CRF: u8 = 0x04;

/// Whether `frame` is a stream (audio or media-clock) frame rather than
/// control traffic. Looks through one 802.1Q tag. Pure.
pub fn is_stream_frame(frame: &[u8]) -> bool {
    let et = |at: usize| {
        frame
            .get(at..at + 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
    };
    let (ethertype, payload) = match et(12) {
        Some(0x8100) => (et(16), 18),
        other => (other, 14),
    };
    ethertype == Some(ETHERTYPE_AVTP)
        && matches!(frame.get(payload), Some(&SUBTYPE_AAF) | Some(&SUBTYPE_CRF))
}

/// A ring of the most recent control frames, with their wall-clock time.
pub struct FlightRecorder {
    frames: VecDeque<(u64, Vec<u8>)>,
    cap: usize,
}

impl Default for FlightRecorder {
    fn default() -> Self {
        Self::new(FLIGHT_RECORDER_FRAMES)
    }
}

impl FlightRecorder {
    pub fn new(cap: usize) -> Self {
        Self {
            frames: VecDeque::with_capacity(cap.min(FLIGHT_RECORDER_FRAMES)),
            cap: cap.max(1),
        }
    }

    /// Record `frame` at wall-clock `unix_ns`, unless it is a stream frame.
    pub fn record(&mut self, unix_ns: u64, frame: &[u8]) {
        if is_stream_frame(frame) {
            return;
        }
        if self.frames.len() == self.cap {
            self.frames.pop_front();
        }
        self.frames.push_back((unix_ns, frame.to_vec()));
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// The buffer as a nanosecond pcap, oldest first. Pure.
    pub fn to_pcap(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            crate::pcap::GLOBAL_HEADER_LEN
                + self
                    .frames
                    .iter()
                    .map(|(_, f)| f.len() + crate::pcap::RECORD_HEADER_LEN)
                    .sum::<usize>(),
        );
        out.extend_from_slice(&crate::pcap::global_header(65535));
        for (ts, f) in &self.frames {
            out.extend_from_slice(&crate::pcap::record_header(*ts, f.len() as u32));
            out.extend_from_slice(f);
        }
        out
    }

    /// Write the buffer to `path` (replacing it).
    pub fn write_pcap(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::File::create(path)?.write_all(&self.to_pcap())
    }
}

/// Wall-clock now in ns since the Unix epoch (0 if the clock is before it).
pub fn unix_now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ethertype: u16, first_payload: u8, tagged: bool) -> Vec<u8> {
        let mut f = vec![0u8; 12];
        if tagged {
            f.extend_from_slice(&0x8100u16.to_be_bytes());
            f.extend_from_slice(&0x6002u16.to_be_bytes());
        }
        f.extend_from_slice(&ethertype.to_be_bytes());
        f.push(first_payload);
        f.resize(f.len() + 40, 0);
        f
    }

    #[test]
    fn stream_frames_are_skipped_and_control_frames_kept() {
        assert!(is_stream_frame(&frame(0x22F0, 0x02, true)));
        assert!(is_stream_frame(&frame(0x22F0, 0x04, false)));
        assert!(!is_stream_frame(&frame(0x22F0, 0xFA, false)), "ADP");
        assert!(!is_stream_frame(&frame(0x22F0, 0xFB, false)), "AECP");
        assert!(!is_stream_frame(&frame(0x88F7, 0x02, false)), "gPTP");
        assert!(!is_stream_frame(&frame(0x22EA, 0x00, false)), "MSRP");
        assert!(!is_stream_frame(&[0u8; 10]), "runt");

        let mut r = FlightRecorder::new(3);
        r.record(1, &frame(0x22F0, 0x02, true));
        assert_eq!(r.len(), 0);
        for i in 0..5u8 {
            r.record(u64::from(i), &frame(0x88F7, i, false));
        }
        // Keeps the newest three.
        assert_eq!(r.len(), 3);
        let p = r.to_pcap();
        assert_eq!(&p[0..4], &crate::pcap::MAGIC_NS.to_le_bytes());
        // First record is the third frame sent (payload byte 2).
        let first_payload = crate::pcap::GLOBAL_HEADER_LEN + crate::pcap::RECORD_HEADER_LEN + 14;
        assert_eq!(p[first_payload], 2);
    }
}
