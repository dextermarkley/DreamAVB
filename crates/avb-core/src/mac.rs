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
pub fn is_avdecc_dest(mac: MacAddr, our_mac: MacAddr) -> bool {
    mac == AVDECC_MULTICAST || matches!(mac.0, [0x91, 0xE0, 0xF0, 0x00, _, _]) || mac == our_mac
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUR_MAC: MacAddr = MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]);

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
