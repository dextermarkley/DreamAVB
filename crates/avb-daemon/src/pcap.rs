// SPDX-License-Identifier: MIT OR Apache-2.0

//! pcap writer (design §11.1 capture / §4): nanosecond-resolution classic
//! pcap (magic `0xA1B23C4D`). Pure byte-builders; the daemon writes the global
//! header once, then a 16-byte record header + bytes per captured slot.

/// Nanosecond-resolution pcap magic (§11.1).
pub const MAGIC_NS: u32 = 0xA1B2_3C4D;
/// Ethernet link type.
pub const LINKTYPE_ETHERNET: u32 = 1;
/// pcap global header length.
pub const GLOBAL_HEADER_LEN: usize = 24;
/// pcap per-record header length.
pub const RECORD_HEADER_LEN: usize = 16;

/// The 24-byte global header (little-endian, ns magic).
pub fn global_header(snaplen: u32) -> [u8; GLOBAL_HEADER_LEN] {
    let mut h = [0u8; GLOBAL_HEADER_LEN];
    h[0..4].copy_from_slice(&MAGIC_NS.to_le_bytes());
    h[4..6].copy_from_slice(&2u16.to_le_bytes()); // version_major
    h[6..8].copy_from_slice(&4u16.to_le_bytes()); // version_minor
                                                  // thiszone(4)=0, sigfigs(4)=0 already zero.
    h[16..20].copy_from_slice(&snaplen.to_le_bytes());
    h[20..24].copy_from_slice(&LINKTYPE_ETHERNET.to_le_bytes());
    h
}

/// A 16-byte record header: `ts_sec`, `ts_nsec`, `incl_len`, `orig_len`.
pub fn record_header(ts_ns: u64, len: u32) -> [u8; RECORD_HEADER_LEN] {
    let mut h = [0u8; RECORD_HEADER_LEN];
    let secs = (ts_ns / 1_000_000_000) as u32;
    let nsec = (ts_ns % 1_000_000_000) as u32;
    h[0..4].copy_from_slice(&secs.to_le_bytes());
    h[4..8].copy_from_slice(&nsec.to_le_bytes());
    h[8..12].copy_from_slice(&len.to_le_bytes());
    h[12..16].copy_from_slice(&len.to_le_bytes());
    h
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn global_header_ns_magic() {
        let h = global_header(2048);
        assert_eq!(u32::from_le_bytes([h[0], h[1], h[2], h[3]]), 0xA1B2_3C4D);
        assert_eq!(u16::from_le_bytes([h[4], h[5]]), 2);
        assert_eq!(u16::from_le_bytes([h[6], h[7]]), 4);
        assert_eq!(u32::from_le_bytes([h[16], h[17], h[18], h[19]]), 2048);
        assert_eq!(u32::from_le_bytes([h[20], h[21], h[22], h[23]]), 1);
    }

    #[test]
    fn record_header_splits_ns() {
        let h = record_header(12_000_000_345, 100);
        assert_eq!(u32::from_le_bytes([h[0], h[1], h[2], h[3]]), 12);
        assert_eq!(u32::from_le_bytes([h[4], h[5], h[6], h[7]]), 345);
        assert_eq!(u32::from_le_bytes([h[8], h[9], h[10], h[11]]), 100);
        assert_eq!(u32::from_le_bytes([h[12], h[13], h[14], h[15]]), 100);
    }
}
