// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-NIC data for the probe: which service to open, register names, the
//! read-only dump, frequency math and timestamp-point latencies. The NIC
//! specific parts are data plus two pure functions, so a static table is
//! enough (and testable without hardware).

use crate::abi;
use crate::freq;

pub struct NicProfile {
    /// `AvbInfo::nic_kind` this profile expects.
    pub kind: u32,
    pub label: &'static str,
    /// Registry name of the dext's service (its IOUserClass).
    pub service_name: &'static str,
    pub dext_bundle_id: &'static str,
    /// What the dext is expected to match (for error messages).
    pub match_desc: &'static str,
    /// Exclusive BAR0 offset limit the dext enforces.
    pub reg_read_limit: u32,
    pub max_ppb: i64,
    pub correction_from_ppb: fn(i64) -> u32,
    pub expected_rate_ppb: fn(u32) -> f64,
    pub reg_dump: &'static [(&'static str, u32)],
    pub ctrl_a_name: &'static str,
    pub ctrl_b_name: &'static str,
    pub clock_ctl_name: &'static str,
    /// Timestamp point to MDI latencies at 1000 Mb/s, ns. Added to t1 / taken
    /// off t4 for the compensated link delay.
    pub tx_latency_1000_ns: i64,
    pub rx_latency_1000_ns: i64,
}

pub const BCM57762: NicProfile = NicProfile {
    kind: abi::NIC_BCM57762,
    label: "Broadcom BCM57762",
    service_name: "AVBBcmDriver",
    dext_bundle_id: "com.avbaudio.installer.bcm57762",
    match_desc: "14e4:1682 / 106b:00f6",
    reg_read_limit: 0x10000,
    max_ppb: freq::bcm::MAX_PPB,
    correction_from_ppb: freq::bcm::correction_from_ppb,
    expected_rate_ppb: freq::bcm::expected_rate_ppb,
    reg_dump: &[
        ("MISC_HOST_CTRL", 0x0068),
        ("GRC_MODE", 0x6800),
        ("EAV_REF_CLCK_CTL", 0x6908),
        ("EAV_REF_CLCK_LSB", 0x6900),
        ("EAV_REF_CLCK_MSB", 0x6904),
        ("EAV_REF_CLK_CORRECT_CTL", 0x6928),
        ("RX_PTP_CTL", 0x06c8),
        ("TX_TSTAMP_LSB", 0x05c0),
        ("TX_TSTAMP_MSB", 0x05c4),
        ("RX_TSTAMP_LSB", 0x06b0),
        ("RX_TSTAMP_MSB", 0x06b4),
        ("MAC_MODE", 0x0400),
        ("MAC_STATUS", 0x0404),
    ],
    ctrl_a_name: "MISC_HOST_CTRL",
    ctrl_b_name: "GRC_MODE",
    clock_ctl_name: "EAV_REF_CLK_CORRECT_CTL",
    tx_latency_1000_ns: 0,
    rx_latency_1000_ns: 0,
};

pub const I210: NicProfile = NicProfile {
    kind: abi::NIC_I210,
    label: "Intel I210",
    service_name: "AVBI210Driver",
    dext_bundle_id: "com.avbaudio.installer.i210",
    match_desc: "8086:1533 / 1ab6:0214",
    reg_read_limit: 0x20000,
    max_ppb: freq::i210::MAX_PPB,
    correction_from_ppb: freq::i210::correction_from_ppb,
    expected_rate_ppb: freq::i210::expected_rate_ppb,
    // Every entry passes `i210_readable` (no read side effects).
    reg_dump: &[
        ("CTRL", 0x0000),
        ("STATUS", 0x0008),
        ("CTRL_EXT", 0x0018),
        ("PHPM", 0x0E14),
        ("RCTL", 0x0100),
        ("TCTL", 0x0400),
        ("RXPBS", 0x2404),
        ("SRRCTL0", 0xC00C),
        ("RXDCTL0", 0xC028),
        ("RDH0", 0xC010),
        ("RDT0", 0xC018),
        ("TXDCTL0", 0xE028),
        ("TDH0", 0xE010),
        ("TDT0", 0xE018),
        ("ETQF3", 0x5CBC),
        ("TSYNCRXCTL", 0xB620),
        ("TSYNCTXCTL", 0xB614),
        ("TIMINCA", 0xB608),
        ("TSAUXC", 0xB640),
        ("SYSTIML", 0xB600),
        ("SYSTIMH", 0xB604),
        ("EEMNGCTL", 0x12030),
    ],
    ctrl_a_name: "CTRL",
    ctrl_b_name: "STATUS",
    clock_ctl_name: "TIMINCA",
    tx_latency_1000_ns: 178,
    rx_latency_1000_ns: 448,
};

pub const ALL: [&NicProfile; 2] = [&I210, &BCM57762];

pub fn by_kind(kind: u32) -> Option<&'static NicProfile> {
    ALL.into_iter().find(|p| p.kind == kind)
}

/// Rust mirror of `isReadableRegOffsetI210` in
/// driver/AVBI210Driver/I210Logic.hpp (the dext is authoritative).
pub fn i210_readable(off: u32) -> bool {
    if off >= 0x20000 || off & 3 != 0 {
        return false;
    }
    if (0x4000..0x5000).contains(&off) || (0xB65C..=0xB668).contains(&off) {
        return false;
    }
    !matches!(
        off,
        0x1500
            | 0x00C0
            | 0x1580
            | 0x5B50
            | 0xB66C
            | 0xB6F8
            | 0xB618
            | 0xB61C
            | 0xB624
            | 0xB628
            | 0xB62C
            | 0xB630
            | 0x1084
            | 0x5BF4
            | 0x8FDC
            | 0x8FE0
            | 0x8FE4
            | 0xC030 // RQDPC(0): owned by the dext's statistics accumulator (ABI v3)
    )
}

// Per-NIC limits stay within the shared ABI limits, and the default adjfreq
// step (100 000 ppb) is valid for both NICs.
const _: () =
    assert!(I210.max_ppb <= abi::ADJ_FREQ_MAX_PPB && BCM57762.max_ppb <= abi::ADJ_FREQ_MAX_PPB);
const _: () = assert!(I210.reg_read_limit <= abi::REG_READ_LIMIT);
const _: () = assert!(100_000 <= I210.max_ppb && 100_000 <= BCM57762.max_ppb);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i210_dump_is_side_effect_free() {
        for (name, off) in I210.reg_dump {
            assert!(i210_readable(*off), "{name} 0x{off:x}");
            assert!(*off < I210.reg_read_limit);
        }
        assert!(!i210_readable(0xB618));
        assert!(!i210_readable(0x00C0));
        assert!(!i210_readable(0x4010));
        assert!(!i210_readable(0xC030));
        assert!(!i210_readable(0x20000));
    }

    #[test]
    fn bcm_dump_within_window() {
        for (name, off) in BCM57762.reg_dump {
            assert!(*off < BCM57762.reg_read_limit && off & 3 == 0, "{name}");
        }
    }

    #[test]
    fn lookup_and_limits() {
        assert_eq!(
            by_kind(abi::NIC_I210).unwrap().service_name,
            "AVBI210Driver"
        );
        assert_eq!(
            by_kind(abi::NIC_BCM57762).unwrap().service_name,
            "AVBBcmDriver"
        );
        assert!(by_kind(0).is_none());
        assert_eq!((I210.correction_from_ppb)(100_000), 0x0034_6DC6);
        assert_eq!((BCM57762.correction_from_ppb)(100_000), 0x8000_068E);
        // Net compensation on the I210: raw - (178 + 448) / 2 = raw - 313.
        assert_eq!((I210.tx_latency_1000_ns + I210.rx_latency_1000_ns) / 2, 313);
    }
}
