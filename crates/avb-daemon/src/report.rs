//! JSON report builders (design §11.3 summary.json, §11.1 subcommand
//! summaries). Pure serde_json; the stack fills them from run-time state.

use serde_json::{json, Value};

/// CPU usage from `getrusage` deltas (design §10.4 / §11.3).
pub fn cpu_json(user_us: u64, sys_us: u64) -> Value {
    json!({ "user_us": user_us, "system_us": sys_us })
}

/// A per-stream summary entry (§11.3). The caller passes the AAF-5 counters
/// and the measured rate/ppm.
#[allow(clippy::too_many_arguments)]
pub fn stream_summary(
    wav_channel_base: u32,
    stream_id: u64,
    packets: u64,
    seq_gap_packets: u64,
    late: u64,
    early: u64,
    measured_ppm: f64,
) -> Value {
    json!({
        "wav_channel_base": wav_channel_base,
        "stream_id": format!("0x{stream_id:016x}"),
        "packets": packets,
        "seq_gap_packets": seq_gap_packets,
        "late": late,
        "early": early,
        "measured_ppm": measured_ppm,
    })
}

/// The record summary skeleton (§11.3). `streams`, `media_clock`, `gptp`,
/// `dext_counters` and `ring_counters` are injected by the stack.
pub fn record_summary(
    seconds: u64,
    rate: u32,
    channels: u32,
    frames: u64,
    exit_code: i32,
) -> Value {
    json!({
        "schema": "avb-record-summary/1",
        "seconds": seconds,
        "rate_hz": rate,
        "channels": channels,
        "frames": frames,
        "wav_duration_s": if rate > 0 { frames as f64 / f64::from(rate) } else { 0.0 },
        "exit_status": exit_code,
    })
}

/// The gptp summary (§11.1): includes `locked` and `lock_time_s`.
pub fn gptp_summary(
    locked: bool,
    lock_time_s: Option<f64>,
    mean_offset_ns: f64,
    mean_delay_ns: f64,
) -> Value {
    json!({
        "schema": "avb-gptp-summary/1",
        "locked": locked,
        "lock_time_s": lock_time_s,
        "mean_offset_ns": mean_offset_ns,
        "mean_delay_ns": mean_delay_ns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_summary_shape() {
        let v = record_summary(600, 48_000, 32, 600 * 48_000, 0);
        assert_eq!(v["schema"], json!("avb-record-summary/1"));
        assert_eq!(v["rate_hz"], json!(48_000));
        assert_eq!(v["frames"], json!(600u64 * 48_000));
        assert_eq!(v["wav_duration_s"], json!(600.0));
        assert_eq!(v["exit_status"], json!(0));
    }

    #[test]
    fn gptp_summary_carries_lock_fields() {
        let v = gptp_summary(false, None, 0.0, 142.6);
        assert_eq!(v["locked"], json!(false));
        assert_eq!(v["lock_time_s"], Value::Null);
        assert_eq!(v["mean_delay_ns"], json!(142.6));
    }

    #[test]
    fn stream_summary_hex_id() {
        let v = stream_summary(0, 0x000a_9203_3ba9_0001, 100, 2, 1, 0, 0.05);
        assert_eq!(v["stream_id"], json!("0x000a92033ba90001"));
        assert_eq!(v["packets"], json!(100));
    }
}
