// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sample placement and arrival/late/early classification (design §10.3,
//! §10.6 placement). Pure arithmetic over DLL state; no I/O.

use avb_core::ext32;

/// The Milan default presentation offset floor (§10.3: 2 000 000 ns).
pub const PRESENTATION_FLOOR_NS: u64 = 2_000_000;
/// One class A observation interval added to `early_bound` (§10.3: 125 000 ns).
/// This is the one allowed timing literal (§14.4).
pub const CLASS_A_INTERVAL_NS: u64 = 125_000;

/// `early_bound = max(ta_latency, 2 ms) + 125 µs` (design §10.3, U-9).
pub fn early_bound_ns(ta_accumulated_latency_ns: u64) -> u64 {
    ta_accumulated_latency_ns
        .max(PRESENTATION_FLOOR_NS)
        .saturating_add(CLASS_A_INTERVAL_NS)
}

/// How a packet's presentation time relates to its arrival (§10.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timing {
    /// `pt >= arrival` and within `early_bound`.
    OnTime,
    /// `pt < arrival`.
    Late,
    /// `pt − arrival > early_bound`.
    Early,
}

/// Extend a 32-bit AVTP timestamp to 64-bit gPTP ns nearest `arrival_ns`
/// (§10.3; wraps via [`avb_core::ext32`]).
pub fn presentation_time_ns(avtp_timestamp: u32, arrival_ns: u64) -> Option<u64> {
    ext32(arrival_ns, avtp_timestamp)
}

/// Classify a packet given its presentation and arrival gPTP times (§10.3).
pub fn classify(pt_ns: u64, arrival_ns: u64, early_bound: u64) -> Timing {
    if pt_ns < arrival_ns {
        Timing::Late
    } else if pt_ns - arrival_ns > early_bound {
        Timing::Early
    } else {
        Timing::OnTime
    }
}

/// The absolute sample-frame index for a packet with presentation `pt_ns`,
/// given the DLL anchor `t0_ns`/`n0` and sample period `t_ns` (§10.6:
/// `n = n0 + round((pt − t0)/T)`).
pub fn frame_index(pt_ns: f64, t0_ns: f64, n0: i128, t_ns: f64) -> i128 {
    if t_ns <= 0.0 {
        return n0;
    }
    let delta = (pt_ns - t0_ns) / t_ns;
    n0 + delta.round() as i128
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn early_bound_floor_and_latency() {
        // Below the 2 ms floor -> floor + 125 us.
        assert_eq!(early_bound_ns(0), 2_000_000 + 125_000);
        assert_eq!(early_bound_ns(1_000_000), 2_000_000 + 125_000);
        // Above the floor -> latency + 125 us.
        assert_eq!(early_bound_ns(5_000_000), 5_000_000 + 125_000);
    }

    #[test]
    fn classification() {
        let eb = early_bound_ns(2_000_000); // 2.125 ms
        assert_eq!(classify(1_000, 2_000, eb), Timing::Late); // pt < arrival
        assert_eq!(classify(2_000, 2_000, eb), Timing::OnTime); // equal
        assert_eq!(classify(2_000 + eb, 2_000, eb), Timing::OnTime); // exactly eb
        assert_eq!(classify(2_000 + eb + 1, 2_000, eb), Timing::Early);
    }

    #[test]
    fn presentation_time_wraps() {
        // avtp_timestamp low word near arrival.
        let arrival = 5u64 << 32;
        assert_eq!(
            presentation_time_ns(200, arrival + 100),
            Some(arrival + 200)
        );
        // Low word just past a 2^32 boundary resolves to the next epoch.
        let near = (6u64 << 32) - 10;
        assert_eq!(presentation_time_ns(5, near), Some((6u64 << 32) + 5));
    }

    #[test]
    fn frame_index_rounds() {
        // t0 = 1000 ns, n0 = 48000, T = 20.833 ns/frame (48k).
        let t = 1e9 / 48_000.0;
        assert_eq!(frame_index(1000.0, 1000.0, 48_000, t), 48_000);
        // One frame later.
        assert_eq!(frame_index(1000.0 + t, 1000.0, 48_000, t), 48_001);
        // Half-frame rounds to nearest.
        assert_eq!(frame_index(1000.0 + 0.4 * t, 1000.0, 48_000, t), 48_000);
        assert_eq!(frame_index(1000.0 + 0.6 * t, 1000.0, 48_000, t), 48_001);
    }
}
