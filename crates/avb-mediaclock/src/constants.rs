//! Rate-derived media-clock constants (design §10.5). Every threshold is a
//! duration converted with the bound model's `rate` (AUDIO_UNIT current rate,
//! Hz) and `spf` (samples per frame of the reference stream), never a packet
//! or frame literal (acceptance criterion 14 / §14.4). The only numeric
//! literals here are the durations themselves.

/// The durations the thresholds come from (§10.5).
pub mod durations {
    /// Consecutive-in-lock hold before LOCKED.
    pub const LOCK_HOLD_S: f64 = 1.0;
    /// A stream this far behind the frontier is treated as gapped.
    pub const LAG_GAP_S: f64 = 0.004;
    /// All-silent frontier lag.
    pub const SILENT_FR_S: f64 = 0.004;
    /// Ring span (→ capacity, rounded up to a power of two).
    pub const RING_SPAN_S: f64 = 1.25;
    /// Model publish interval.
    pub const MODEL_PUB_S: f64 = 0.001;
}

/// The constants derived from one bound model's `rate` and `spf` (§10.5).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateConsts {
    pub rate: u32,
    pub spf: u32,
    /// `ceil(LOCK_HOLD · rate / spf)` packets.
    pub lock_packets: u64,
    /// `ceil(LAG_GAP · rate)` frames.
    pub lag_frames: u64,
    /// `ceil(SILENT_FR · rate)` frames.
    pub silent_lag: u64,
    /// `next_pow2(ceil(RING_SPAN · rate))` frames.
    pub capacity_frames: u64,
    /// `max(1, ceil(MODEL_PUB · rate / spf))` packets.
    pub publish_packets: u64,
    /// Packet period in ns: `spf · 1e9 / rate`.
    pub p_init_ns: f64,
    /// Packet period in s: `spf / rate` (the DLL's `Tu`).
    pub tu_s: f64,
}

fn ceil_div_f64(num: f64, den: f64) -> u64 {
    (num / den).ceil() as u64
}

fn next_pow2(v: u64) -> u64 {
    if v <= 1 {
        return 1;
    }
    let mut p = 1u64;
    while p < v {
        p <<= 1;
    }
    p
}

impl RateConsts {
    /// Derive the constants from `rate` (Hz) and `spf`. Both must be non-zero;
    /// returns `None` otherwise (the caller has already validated the AEM).
    pub fn new(rate: u32, spf: u32) -> Option<Self> {
        if rate == 0 || spf == 0 {
            return None;
        }
        let r = f64::from(rate);
        let s = f64::from(spf);
        use durations::*;
        Some(Self {
            rate,
            spf,
            lock_packets: ceil_div_f64(LOCK_HOLD_S * r, s),
            lag_frames: ceil_div_f64(LAG_GAP_S * r, 1.0),
            silent_lag: ceil_div_f64(SILENT_FR_S * r, 1.0),
            capacity_frames: next_pow2(ceil_div_f64(RING_SPAN_S * r, 1.0)),
            publish_packets: ceil_div_f64(MODEL_PUB_S * r, s).max(1),
            p_init_ns: s * 1e9 / r,
            tu_s: s / r,
        })
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn constants_48k_6spf() {
        // §10.5: 8000 packets, 192 frames, 192 frames, 65536 frames, 8 packets, 125 us.
        let c = RateConsts::new(48_000, 6).unwrap();
        assert_eq!(c.lock_packets, 8000);
        assert_eq!(c.lag_frames, 192);
        assert_eq!(c.silent_lag, 192);
        assert_eq!(c.capacity_frames, 65_536);
        assert_eq!(c.publish_packets, 8);
        assert!((c.p_init_ns - 125_000.0).abs() < 1e-6);
        assert!((c.tu_s - 125e-6).abs() < 1e-12);
    }

    #[test]
    fn constants_96k_12spf() {
        // §10.5: 8000, 384, 384, 131072, 8, 125 us.
        let c = RateConsts::new(96_000, 12).unwrap();
        assert_eq!(c.lock_packets, 8000);
        assert_eq!(c.lag_frames, 384);
        assert_eq!(c.silent_lag, 384);
        assert_eq!(c.capacity_frames, 131_072);
        assert_eq!(c.publish_packets, 8);
        assert!((c.p_init_ns - 125_000.0).abs() < 1e-6);
    }

    #[test]
    fn rejects_zero() {
        assert!(RateConsts::new(0, 6).is_none());
        assert!(RateConsts::new(48_000, 0).is_none());
    }

    #[test]
    fn next_pow2_cases() {
        assert_eq!(next_pow2(0), 1);
        assert_eq!(next_pow2(1), 1);
        assert_eq!(next_pow2(60_000), 65_536);
        assert_eq!(next_pow2(65_536), 65_536);
        assert_eq!(next_pow2(65_537), 131_072);
    }
}
