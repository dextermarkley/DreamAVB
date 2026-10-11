// SPDX-License-Identifier: MIT OR Apache-2.0

//! Injected time: mach ticks, the mach timebase, gPTP nanoseconds and 32-bit
//! timestamp extension. No clock is read here (design §6.1).

use std::fmt;

/// A `mach_absolute_time()` value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MachTicks(pub u64);

impl MachTicks {
    /// `self + ticks`, saturating at `u64::MAX`.
    pub const fn add_ticks(self, ticks: u64) -> Self {
        Self(self.0.saturating_add(ticks))
    }

    /// Ticks from `earlier` to `self`, 0 if `earlier` is later.
    pub const fn ticks_since(self, earlier: MachTicks) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// `mach_timebase_info`: ns = ticks × numer / denom. On Apple Silicon this is
/// 125/3 (24 MHz). avb-core never queries it; the daemon passes the value the
/// dext reports in `AvbInfo.tb_numer/tb_denom`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Timebase {
    pub numer: u32,
    pub denom: u32,
}

impl Timebase {
    pub const IDENTITY: Timebase = Timebase { numer: 1, denom: 1 };

    /// Both terms non-zero. Conversions with an invalid timebase return 0.
    pub const fn is_valid(self) -> bool {
        self.numer != 0 && self.denom != 0
    }

    /// ticks → ns, rounded down, saturating at `u64::MAX`.
    pub fn ticks_to_ns(self, ticks: u64) -> u64 {
        mul_div(ticks, self.numer, self.denom)
    }

    /// ns → ticks, rounded down, saturating at `u64::MAX`.
    pub fn ns_to_ticks(self, ns: u64) -> u64 {
        mul_div(ns, self.denom, self.numer)
    }

    /// Duration from `from` to `to` in ns, negative when `to` is earlier.
    pub fn delta_ns(self, from: MachTicks, to: MachTicks) -> i64 {
        let n = self.ticks_to_ns(to.0.abs_diff(from.0));
        let n = i64::try_from(n).unwrap_or(i64::MAX);
        if to.0 >= from.0 {
            n
        } else {
            -n
        }
    }

    /// ticks → ns as a float (for regressions and servo math).
    pub fn ticks_to_ns_f64(self, ticks: f64) -> f64 {
        if !self.is_valid() {
            return 0.0;
        }
        ticks * f64::from(self.numer) / f64::from(self.denom)
    }
}

fn mul_div(v: u64, mul: u32, div: u32) -> u64 {
    let r = (u128::from(v) * u128::from(mul))
        .checked_div(u128::from(div))
        .unwrap_or(0);
    u64::try_from(r).unwrap_or(u64::MAX)
}

/// gPTP time in ns since the PTP epoch (seconds × 1e9 + nanoseconds).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GptpNs(pub u64);

impl GptpNs {
    pub const NS_PER_SEC: u64 = 1_000_000_000;

    /// From a PTP timestamp (48-bit seconds, 32-bit nanoseconds < 1e9).
    /// `None` on overflow or nanoseconds ≥ 1e9.
    pub fn from_secs_nanos(secs: u64, nanos: u32) -> Option<Self> {
        if u64::from(nanos) >= Self::NS_PER_SEC {
            return None;
        }
        secs.checked_mul(Self::NS_PER_SEC)?
            .checked_add(u64::from(nanos))
            .map(Self)
    }

    pub const fn secs(self) -> u64 {
        self.0 / Self::NS_PER_SEC
    }

    pub const fn subsec_nanos(self) -> u32 {
        (self.0 % Self::NS_PER_SEC) as u32
    }

    /// `self - earlier` in ns (signed, saturating).
    pub fn delta_ns(self, earlier: GptpNs) -> i64 {
        let d = i128::from(self.0) - i128::from(earlier.0);
        i64::try_from(d).unwrap_or(if d < 0 { i64::MIN } else { i64::MAX })
    }
}

impl fmt::Display for GptpNs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:09}", self.secs(), self.subsec_nanos())
    }
}

/// Extend a 32-bit value (e.g. an AVTP timestamp: gPTP ns mod 2^32) to the
/// 64-bit value nearest `reference`, i.e. within [reference − 2^31,
/// reference + 2^31). `None` if that value would be below 0 or above
/// `u64::MAX`.
pub fn ext32(reference: u64, low: u32) -> Option<u64> {
    // Signed distance from the reference's low word to `low`, in [-2^31, 2^31).
    let diff = low.wrapping_sub(reference as u32) as i32;
    reference.checked_add_signed(i64::from(diff))
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPLE: Timebase = Timebase {
        numer: 125,
        denom: 3,
    };

    #[test]
    fn timebase_125_3() {
        assert_eq!(APPLE.ticks_to_ns(24), 1000);
        assert_eq!(APPLE.ticks_to_ns(24_000), 1_000_000);
        assert_eq!(APPLE.ns_to_ticks(1_000_000), 24_000); // AVB_STEP_SETTLE_NS
                                                          // §5.1 audio thread policy: 250/60/200 us.
        assert_eq!(APPLE.ns_to_ticks(250_000), 6000);
        assert_eq!(APPLE.ns_to_ticks(60_000), 1440);
        assert_eq!(APPLE.ns_to_ticks(200_000), 4800);
        // 50 ms harvester watchdog.
        assert_eq!(APPLE.ns_to_ticks(50_000_000), 1_200_000);
        // Rounds down.
        assert_eq!(APPLE.ticks_to_ns(1), 41);
        assert_eq!(APPLE.ns_to_ticks(41), 0);
        assert_eq!(APPLE.ns_to_ticks(42), 1);
        // No intermediate overflow; saturates.
        assert_eq!(APPLE.ticks_to_ns(u64::MAX), u64::MAX);
        assert_eq!(
            u128::from(APPLE.ns_to_ticks(u64::MAX)),
            u128::from(u64::MAX) * 3 / 125
        );
        assert_eq!(APPLE.delta_ns(MachTicks(48), MachTicks(24)), -1000);
        assert_eq!(APPLE.delta_ns(MachTicks(24), MachTicks(48)), 1000);
        assert!((APPLE.ticks_to_ns_f64(3.0) - 125.0).abs() < 1e-9);
    }

    #[test]
    fn timebase_identity_and_invalid() {
        let tb = Timebase::IDENTITY;
        assert_eq!(tb.ticks_to_ns(123_456_789), 123_456_789);
        assert_eq!(tb.ns_to_ticks(123_456_789), 123_456_789);
        assert_eq!(tb.delta_ns(MachTicks(10), MachTicks(3)), -7);
        assert!(tb.is_valid());
        let bad = Timebase { numer: 1, denom: 0 };
        assert!(!bad.is_valid());
        assert_eq!(bad.ticks_to_ns(5), 0);
        assert_eq!(bad.ticks_to_ns_f64(5.0), 0.0);
    }

    #[test]
    fn mach_ticks_helpers() {
        assert_eq!(MachTicks(5).add_ticks(3), MachTicks(8));
        assert_eq!(MachTicks(u64::MAX).add_ticks(3), MachTicks(u64::MAX));
        assert_eq!(MachTicks(8).ticks_since(MachTicks(5)), 3);
        assert_eq!(MachTicks(5).ticks_since(MachTicks(8)), 0);
    }

    #[test]
    fn gptp_ns() {
        let t = GptpNs::from_secs_nanos(12, 345).unwrap();
        assert_eq!(t.0, 12_000_000_345);
        assert_eq!((t.secs(), t.subsec_nanos()), (12, 345));
        assert_eq!(t.to_string(), "12.000000345");
        assert_eq!(GptpNs::from_secs_nanos(1, 1_000_000_000), None);
        assert_eq!(GptpNs::from_secs_nanos(u64::MAX, 0), None);
        assert_eq!(GptpNs(5).delta_ns(GptpNs(8)), -3);
        assert_eq!(GptpNs(u64::MAX).delta_ns(GptpNs(0)), i64::MAX);
    }

    #[test]
    fn ext32_wraps() {
        const P32: u64 = 1 << 32;
        const H31: u64 = 1 << 31;
        // Same epoch.
        assert_eq!(ext32(5 * P32 + 100, 200), Some(5 * P32 + 200));
        assert_eq!(ext32(5 * P32 + 200, 100), Some(5 * P32 + 100));
        // Low word wrapped forward past the 2^32 boundary.
        assert_eq!(ext32(6 * P32 - 10, 5), Some(6 * P32 + 5));
        // Value just before the boundary, reference just after it.
        assert_eq!(ext32(6 * P32 + 5, 0xFFFF_FFF0), Some(6 * P32 - 16));
        // ±2^31 edges: +2^31 - 1 ahead and -2^31 behind are reachable,
        // +2^31 ahead maps to 2^31 behind.
        let r = 7 * P32;
        assert_eq!(ext32(r, (H31 - 1) as u32), Some(r + H31 - 1));
        assert_eq!(ext32(r, H31 as u32), Some(r - H31));
        assert_eq!(ext32(r, (P32 - H31 + 1) as u32), Some(r - H31 + 1));
        // Exact.
        assert_eq!(ext32(r + 0x1234, 0x1234), Some(r + 0x1234));
        // Below zero / above u64::MAX.
        assert_eq!(ext32(5, 0xFFFF_FFF0), None);
        assert_eq!(ext32(u64::MAX - 5, 10), None);
        assert_eq!(ext32(0, 0), Some(0));
    }
}
