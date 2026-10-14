// SPDX-License-Identifier: MIT OR Apache-2.0

//! MAC addresses, EUI-64 identifiers, stream IDs and the AVDECC destination
//! predicate.

use std::fmt;

/// A 48-bit IEEE 802 MAC address, transmission order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    pub const BROADCAST: MacAddr = MacAddr([0xff; 6]);

    pub const fn new(octets: [u8; 6]) -> Self {
        Self(octets)
    }

    pub const fn octets(self) -> [u8; 6] {
        self.0
    }

    /// I/G bit (bit 0 of the first octet).
    pub const fn is_multicast(self) -> bool {
        let [first, ..] = self.0;
        first & 1 != 0
    }

    /// The first six bytes of `bytes`, if present.
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        bytes.first_chunk::<6>().map(|b| Self(*b))
    }

    /// The address as a 48-bit big-endian integer.
    pub const fn to_u64(self) -> u64 {
        let [a, b, c, d, e, f] = self.0;
        u64::from_be_bytes([0, 0, a, b, c, d, e, f])
    }

    /// Inverse of [`MacAddr::to_u64`] (the upper 16 bits are ignored).
    pub const fn from_u64(v: u64) -> Self {
        let [_, _, a, b, c, d, e, f] = v.to_be_bytes();
        Self([a, b, c, d, e, f])
    }
}

fn fmt_colon(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    for (i, b) in bytes.iter().enumerate() {
        if i != 0 {
            f.write_str(":")?;
        }
        write!(f, "{b:02x}")?;
    }
    Ok(())
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_colon(f, &self.0)
    }
}

/// A 64-bit extended unique identifier (gPTP clockIdentity, AVDECC entity ID).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Eui64(pub [u8; 8]);

impl Eui64 {
    /// EUI-48 to EUI-64 by inserting `FF:FE` after the OUI (802.1AS
    /// clockIdentity rule; design §7.1).
    pub const fn from_mac(mac: MacAddr) -> Self {
        let [a, b, c, d, e, f] = mac.0;
        Self([a, b, c, 0xff, 0xfe, d, e, f])
    }

    pub const fn to_u64(self) -> u64 {
        u64::from_be_bytes(self.0)
    }

    pub const fn from_u64(v: u64) -> Self {
        Self(v.to_be_bytes())
    }
}

impl fmt::Display for Eui64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_colon(f, &self.0)
    }
}

/// A 64-bit stream ID: the talker's 48-bit MAC followed by a 16-bit unique ID.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StreamId(pub u64);

impl StreamId {
    pub const fn from_parts(mac: MacAddr, unique_id: u16) -> Self {
        Self((mac.to_u64() << 16) | unique_id as u64)
    }

    pub const fn mac(self) -> MacAddr {
        MacAddr::from_u64(self.0 >> 16)
    }

    pub const fn unique_id(self) -> u16 {
        (self.0 & 0xffff) as u16
    }

    pub const fn to_be_bytes(self) -> [u8; 8] {
        self.0.to_be_bytes()
    }

    pub const fn from_be_bytes(b: [u8; 8]) -> Self {
        Self(u64::from_be_bytes(b))
    }
}

impl fmt::Display for StreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:016x}", self.0)
    }
}

/// The AVDECC multicast address used by ADP, ACMP and AECP AEM unsolicited
/// notifications (IEEE 1722.1).
pub const AVDECC_MULTICAST: MacAddr = MacAddr([0x91, 0xE0, 0xF0, 0x01, 0x00, 0x00]);

/// Destinations the dext classifies as AVDECC (§3.4 rule 5): the AVDECC
/// multicast address, the `91:E0:F0:00:xx:xx` block, or our own unicast MAC.
/// Twin of `isAvdeccDest` in driver/AVBI210Driver/I210Logic.hpp; both are
/// tested against the same boundary table.
///
/// NOTE: the `91:E0:F0:00:xx:xx` arm is WIDER than the control plane really
/// is — that block is the MAAP dynamic pool, where every talker's stream
/// destination address comes from (the 16R's own streams are at
/// `91:E0:F0:00:CE:F4`..`FB`). It is harmless because this is only the
/// FALLBACK classification for a frame no per-stream filter matched, and an
/// established stream always has a filter entry keyed on dest_mac+vid. Do NOT
/// read this predicate as "stream addresses must avoid the MAAP pool": that
/// inference is what put our own talker in the 1722.1-reserved block.
pub fn is_avdecc_dest(mac: MacAddr, our_mac: MacAddr) -> bool {
    mac == AVDECC_MULTICAST || matches!(mac.0, [0x91, 0xE0, 0xF0, 0x00, _, _]) || mac == our_mac
}

/// First address of the MAAP dynamic allocation pool (IEEE 1722-2016 Annex B
/// Table B.2): the range a talker allocates its AVTP stream destination
/// addresses from.
pub const MAAP_DYNAMIC_FIRST: MacAddr = MacAddr([0x91, 0xE0, 0xF0, 0x00, 0x00, 0x00]);

/// Last address of the MAAP dynamic allocation pool. Above this sit the MAAP
/// locally-administered pool (`00:FE:00`..`00:FE:FF`) and then, one octet up,
/// the AVDECC multicast address and the rest of the 1722.1-reserved block.
pub const MAAP_DYNAMIC_LAST: MacAddr = MacAddr([0x91, 0xE0, 0xF0, 0x00, 0xFD, 0xFF]);

/// Addresses in the MAAP dynamic pool: `0x0000`..=`0xFDFF`.
pub const MAAP_DYNAMIC_COUNT: u32 = 0xFE00;

/// Whether `mac` is a valid AVTP stream destination address, i.e. inside the
/// MAAP dynamic allocation pool.
pub fn is_maap_dynamic(mac: MacAddr) -> bool {
    matches!(mac.0, [0x91, 0xE0, 0xF0, 0x00, hi, _] if hi <= 0xFD)
}

/// A talker's stream destination address for stream `uid`, taken from the MAAP
/// dynamic pool (IEEE 1722-2016 Annex B).
///
/// A fully conformant talker CLAIMS its block by running the MAAP protocol —
/// probe the candidate range, defend it against other claimants, and move on a
/// conflict. We do not run MAAP yet, so this derives a stable pseudo-random
/// base from our own MAC instead: two different machines land on different
/// blocks with high probability, and the same machine always picks the same one
/// so a listener's saved connection stays valid across restarts.
///
/// The address MUST come from this pool. The 16R demonstrates it by example
/// (`91:E0:F0:00:CE:F4`..`FB`), and a Milan listener will reserve bandwidth for
/// a stream addressed outside it — SRP does not police the block — while never
/// media-locking to the audio.
pub fn maap_stream_dest(our_mac: MacAddr, uid: u16, block_len: u16) -> MacAddr {
    // FNV-1a over the MAC, so the base does not correlate with the OUI (every
    // Mac of one vendor would otherwise crowd into the same part of the pool).
    let mut h: u32 = 0x811c_9dc5;
    for b in our_mac.0 {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    let span = MAAP_DYNAMIC_COUNT.saturating_sub(u32::from(block_len).max(1));
    let base = if span == 0 { 0 } else { h % span };
    let off = (base + u32::from(uid)) & 0xFFFF;
    MacAddr([0x91, 0xE0, 0xF0, 0x00, (off >> 8) as u8, off as u8])
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUR_MAC: MacAddr = MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]);

    /// The destination addresses the PreSonus 16R allocates for its OWN eight
    /// stream outputs, read off the wire from its SRP Talker Advertise. They
    /// are the hardware anchor for which block a stream address belongs in.
    const SIXTEEN_R_STREAM_DESTS: [MacAddr; 8] = [
        MacAddr([0x91, 0xE0, 0xF0, 0x00, 0xCE, 0xF4]),
        MacAddr([0x91, 0xE0, 0xF0, 0x00, 0xCE, 0xF5]),
        MacAddr([0x91, 0xE0, 0xF0, 0x00, 0xCE, 0xF6]),
        MacAddr([0x91, 0xE0, 0xF0, 0x00, 0xCE, 0xF7]),
        MacAddr([0x91, 0xE0, 0xF0, 0x00, 0xCE, 0xF8]),
        MacAddr([0x91, 0xE0, 0xF0, 0x00, 0xCE, 0xF9]),
        MacAddr([0x91, 0xE0, 0xF0, 0x00, 0xCE, 0xFA]),
        MacAddr([0x91, 0xE0, 0xF0, 0x00, 0xCE, 0xFB]),
    ];

    #[test]
    fn the_16rs_own_stream_addresses_are_in_the_maap_dynamic_pool() {
        // If this fails, the pool bounds are wrong — the device is right.
        for d in SIXTEEN_R_STREAM_DESTS {
            assert!(is_maap_dynamic(d), "{d} is not in the MAAP dynamic pool");
        }
        assert!(is_maap_dynamic(MAAP_DYNAMIC_FIRST));
        assert!(is_maap_dynamic(MAAP_DYNAMIC_LAST));
        // One past the end is the MAAP locally-administered pool, not dynamic.
        assert!(!is_maap_dynamic(MacAddr([
            0x91, 0xE0, 0xF0, 0x00, 0xFE, 0x00
        ])));
        // The AVDECC multicast address and the rest of its block are reserved
        // for 1722.1 and are never stream destinations. This is exactly where
        // our talker used to transmit.
        assert!(!is_maap_dynamic(AVDECC_MULTICAST));
        for lo in [0x01u8, 0x08, 0xFF] {
            assert!(!is_maap_dynamic(MacAddr([
                0x91, 0xE0, 0xF0, 0x01, 0x00, lo
            ])));
        }
    }

    #[test]
    fn our_stream_dests_are_in_the_pool_and_distinct() {
        let mut seen = std::collections::BTreeSet::new();
        for uid in 0u16..64 {
            let d = maap_stream_dest(OUR_MAC, uid, 64);
            assert!(is_maap_dynamic(d), "uid {uid} -> {d} outside the pool");
            assert_ne!(d, AVDECC_MULTICAST);
            assert!(seen.insert(d), "uid {uid} -> {d} duplicated");
        }
        assert_eq!(seen.len(), 64);
    }

    #[test]
    fn our_block_does_not_collide_with_the_16rs() {
        // We do not run MAAP's probe/defend, so this is the one collision that
        // actually matters on this bench: our eight talker streams must not
        // land on the eight addresses the 16R already claims.
        let ours: Vec<MacAddr> = (0u16..64)
            .map(|u| maap_stream_dest(OUR_MAC, u, 64))
            .collect();
        for theirs in SIXTEEN_R_STREAM_DESTS {
            assert!(
                !ours.contains(&theirs),
                "our block collides with the 16R at {theirs}"
            );
        }
    }

    #[test]
    fn the_base_is_stable_per_mac_and_differs_between_macs() {
        // Stable: a listener's saved connection survives our restart.
        assert_eq!(
            maap_stream_dest(OUR_MAC, 3, 8),
            maap_stream_dest(OUR_MAC, 3, 8)
        );
        // And two machines do not crowd onto the same block despite sharing an
        // OUI — the hash covers every octet, not just the NIC-specific tail.
        let other = MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb9]);
        assert_ne!(
            maap_stream_dest(OUR_MAC, 0, 8),
            maap_stream_dest(other, 0, 8)
        );
        let same_oui_far = MacAddr([0x64, 0x4b, 0xf0, 0x99, 0x00, 0x01]);
        assert_ne!(
            maap_stream_dest(OUR_MAC, 0, 8),
            maap_stream_dest(same_oui_far, 0, 8)
        );
    }

    #[test]
    fn a_whole_block_fits_inside_the_pool() {
        // The base must leave room for block_len consecutive addresses, so the
        // last stream of a 64-channel device is still a legal destination.
        for mac in [
            OUR_MAC,
            MacAddr([0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
            MacAddr([0xff, 0xff, 0xff, 0xff, 0xff, 0xff]),
        ] {
            for block in [1u16, 8, 64] {
                for uid in 0..block {
                    let d = maap_stream_dest(mac, uid, block);
                    assert!(is_maap_dynamic(d), "{mac} block {block} uid {uid} -> {d}");
                }
            }
        }
    }

    #[test]
    fn eui64_inserts_fffe() {
        let e = Eui64::from_mac(OUR_MAC);
        assert_eq!(e.0, [0x64, 0x4b, 0xf0, 0xff, 0xfe, 0x12, 0x73, 0xb8]);
        assert_eq!(e.to_string(), "64:4b:f0:ff:fe:12:73:b8");
        assert_eq!(e.to_u64(), 0x644b_f0ff_fe12_73b8);
        assert_eq!(Eui64::from_u64(e.to_u64()), e);
    }

    #[test]
    fn mac_basics() {
        assert_eq!(OUR_MAC.to_string(), "64:4b:f0:12:73:b8");
        assert_eq!(OUR_MAC.to_u64(), 0x644b_f012_73b8);
        assert_eq!(MacAddr::from_u64(OUR_MAC.to_u64()), OUR_MAC);
        assert!(!OUR_MAC.is_multicast());
        assert!(AVDECC_MULTICAST.is_multicast());
        assert!(MacAddr::BROADCAST.is_multicast());
        assert_eq!(MacAddr::from_slice(&[1, 2, 3, 4, 5]), None);
        assert_eq!(
            MacAddr::from_slice(&[1, 2, 3, 4, 5, 6, 7]),
            Some(MacAddr([1, 2, 3, 4, 5, 6]))
        );
    }

    #[test]
    fn stream_id_parts() {
        let s = StreamId::from_parts(MacAddr([0x00, 0x0a, 0x92, 0x01, 0x02, 0x03]), 0x0005);
        assert_eq!(s.0, 0x000a_9201_0203_0005);
        assert_eq!(s.mac(), MacAddr([0x00, 0x0a, 0x92, 0x01, 0x02, 0x03]));
        assert_eq!(s.unique_id(), 5);
        assert_eq!(StreamId::from_be_bytes(s.to_be_bytes()), s);
        assert_eq!(s.to_string(), "0x000a920102030005");
    }

    /// Same boundary table as testAvdeccDest in driver/tests/i210logic_tests.cpp.
    #[test]
    fn avdecc_dest_boundary_table() {
        let accept = [
            [0x91, 0xE0, 0xF0, 0x00, 0x00, 0x00],
            [0x91, 0xE0, 0xF0, 0x00, 0xFF, 0xFF],
            [0x91, 0xE0, 0xF0, 0x01, 0x00, 0x00],
            [0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8], // our MAC
        ];
        let reject = [
            [0x91, 0xE0, 0xF0, 0x01, 0x00, 0x01],
            [0x91, 0xE0, 0xF1, 0x00, 0x00, 0x00],
            [0x91, 0xE0, 0xEF, 0xFF, 0xFF, 0xFF],
            [0x00, 0x0a, 0x92, 0x01, 0x02, 0x03], // other unicast
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff], // broadcast
        ];
        for m in accept {
            assert!(is_avdecc_dest(MacAddr(m), OUR_MAC), "{}", MacAddr(m));
        }
        for m in reject {
            assert!(!is_avdecc_dest(MacAddr(m), OUR_MAC), "{}", MacAddr(m));
        }
    }
}
