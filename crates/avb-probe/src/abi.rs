//! Rust mirror of `driver/AVBShared.h` (user-client ABI version 2).
//!
//! Keep in lock-step with the C header: the tests at the bottom assert the
//! same sizes and field offsets the C++ side `static_assert`s.

pub const ABI_VERSION: u32 = 2;

/// `AvbInfo::nic_kind` values.
pub const NIC_BCM57762: u32 = 1;
pub const NIC_I210: u32 = 2;

/// External method selectors.
pub mod sel {
    pub const GET_INFO: u32 = 0;
    pub const READ_CLOCK: u32 = 1;
    pub const SET_CLOCK: u32 = 2;
    pub const ADJ_FREQ: u32 = 3;
    pub const INIT_DATAPATH: u32 = 4;
    pub const POLL_RX: u32 = 5;
    pub const SEND_FRAME: u32 = 6;
    pub const GET_TX_TIMESTAMP: u32 = 7;
    pub const READ_REG32: u32 = 8;
    pub const SHUTDOWN_DATAPATH: u32 = 9;
    pub const COUNT: u32 = 10;
}

pub const READ_CLOCK_MAX_SAMPLES: usize = 64;
pub const ADJ_FREQ_MAX_PPB: i64 = 1_000_000;
pub const RX_BATCH_MAX: usize = 24;
pub const RX_COPY_BYTES: usize = 128;
pub const FRAME_MIN_LEN: usize = 60;
pub const FRAME_MAX_LEN: usize = 1514;
/// Maximum over all NICs; each NIC profile carries its own tighter limit.
pub const REG_READ_LIMIT: u32 = 0x20000;

pub const INIT_FLAG_PROMISC: u64 = 1 << 0;
pub const INIT_FLAG_RX_PTP_V2_L2: u64 = 1 << 1;

pub const STAGE_NONE: u32 = 0;
pub const STAGE_TIMESYNC: u32 = 1;
pub const STAGE_DATAPATH: u32 = 2;

pub const FLAG_APE: u32 = 1 << 0;
pub const FLAG_ASF: u32 = 1 << 1;
pub const FLAG_LINK_UP: u32 = 1 << 2;
pub const FLAG_1000FD: u32 = 1 << 3;
pub const FLAG_SIG_OK: u32 = 1 << 4;

pub const TX_TS_PENDING: u64 = 0;
pub const TX_TS_OK: u64 = 1;
pub const TX_TS_TIMEOUT: u64 = 2;
pub const TX_TS_NOT_REQUESTED: u64 = 3;

pub fn stage_name(stage: u32) -> &'static str {
    match stage {
        STAGE_NONE => "none",
        STAGE_TIMESYNC => "timesync",
        STAGE_DATAPATH => "datapath",
        _ => "unknown",
    }
}

pub fn fault_name(fault: u32) -> &'static str {
    match fault {
        0 => "none",
        1 => "all-ones read (device gone/hung)",
        2 => "PCI ID mismatch",
        3 => "firmware mailbox poll timeout",
        4 => "buffer manager enable timeout",
        5 => "host coalescing disable timeout",
        6 => "block stop timeout",
        7 => "DMA setup failed",
        8 => "MDIO busy timeout",
        9 => "NIC SRAM signature missing",
        10 => "TX stall",
        11 => "GIO master-disable timeout",
        12 => "reset / config-done timeout",
        13 => "SW/FW semaphore timeout",
        14 => "queue enable/disable timeout",
        _ => "unknown",
    }
}

pub fn tx_status_name(status: u64) -> &'static str {
    match status {
        TX_TS_PENDING => "pending",
        TX_TS_OK => "ok",
        TX_TS_TIMEOUT => "timeout",
        TX_TS_NOT_REQUESTED => "not-requested",
        _ => "unknown",
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AvbInfo {
    pub abi_version: u32,
    pub vendor_device: u32,
    pub subsys: u32,
    /// BCM: ASIC rev (cfg 0xfc). I210: PCI revision ID.
    pub hw_rev: u32,
    /// BCM: MISC_HOST_CTRL. I210: CTRL.
    pub ctrl_a: u32,
    /// BCM: GRC_MODE. I210: STATUS.
    pub ctrl_b: u32,
    pub stage: u32,
    pub fault: u32,
    pub flags: u32,
    pub mac: [u8; 6],
    pub pad: [u8; 2],
    pub tb_numer: u32,
    pub tb_denom: u32,
    pub nic_kind: u32,
    /// BCM: EAV_REF_CLK_CORRECT_CTL. I210: TIMINCA.
    pub clock_ctl: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct ClockSample {
    pub mach_before: u64,
    /// NIC clock ns (EAV on BCM, SYSTIM sec*1e9+ns on I210).
    pub eav_ns: u64,
    pub mach_after: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxEntry {
    pub harvest_mach: u64,
    pub hw_ts: u64,
    pub len: u32,
    pub type_flags: u16,
    pub has_ts: u8,
    pub pad: u8,
    pub data: [u8; RX_COPY_BYTES],
}

impl Default for RxEntry {
    fn default() -> Self {
        Self {
            harvest_mach: 0,
            hw_ts: 0,
            len: 0,
            type_flags: 0,
            has_ts: 0,
            pad: 0,
            data: [0; RX_COPY_BYTES],
        }
    }
}

impl RxEntry {
    /// The copied prefix of the frame (at most RX_COPY_BYTES).
    pub fn bytes(&self) -> &[u8] {
        let n = (self.len as usize).min(RX_COPY_BYTES);
        &self.data[..n]
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RxBatch {
    pub count: u32,
    pub dropped: u32,
    pub e: [RxEntry; RX_BATCH_MAX],
}

impl Default for RxBatch {
    fn default() -> Self {
        Self {
            count: 0,
            dropped: 0,
            e: [RxEntry::default(); RX_BATCH_MAX],
        }
    }
}

impl RxBatch {
    pub fn entries(&self) -> &[RxEntry] {
        &self.e[..(self.count as usize).min(RX_BATCH_MAX)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn sizes_match_c_static_asserts() {
        assert_eq!(size_of::<AvbInfo>(), 60);
        assert_eq!(size_of::<ClockSample>(), 24);
        assert_eq!(size_of::<RxEntry>(), 152);
        assert_eq!(size_of::<RxBatch>(), 3656);
        assert!(size_of::<RxBatch>() <= 4096);
        assert!(READ_CLOCK_MAX_SAMPLES * size_of::<ClockSample>() <= 4096);
    }

    #[test]
    fn offsets_match_c_static_asserts() {
        assert_eq!(offset_of!(AvbInfo, abi_version), 0);
        assert_eq!(offset_of!(AvbInfo, vendor_device), 4);
        assert_eq!(offset_of!(AvbInfo, subsys), 8);
        assert_eq!(offset_of!(AvbInfo, hw_rev), 12);
        assert_eq!(offset_of!(AvbInfo, ctrl_a), 16);
        assert_eq!(offset_of!(AvbInfo, ctrl_b), 20);
        assert_eq!(offset_of!(AvbInfo, stage), 24);
        assert_eq!(offset_of!(AvbInfo, fault), 28);
        assert_eq!(offset_of!(AvbInfo, flags), 32);
        assert_eq!(offset_of!(AvbInfo, mac), 36);
        assert_eq!(offset_of!(AvbInfo, tb_numer), 44);
        assert_eq!(offset_of!(AvbInfo, tb_denom), 48);
        assert_eq!(offset_of!(AvbInfo, nic_kind), 52);
        assert_eq!(offset_of!(AvbInfo, clock_ctl), 56);
        assert_eq!(offset_of!(ClockSample, mach_before), 0);
        assert_eq!(offset_of!(ClockSample, eav_ns), 8);
        assert_eq!(offset_of!(ClockSample, mach_after), 16);
        assert_eq!(offset_of!(RxEntry, harvest_mach), 0);
        assert_eq!(offset_of!(RxEntry, hw_ts), 8);
        assert_eq!(offset_of!(RxEntry, len), 16);
        assert_eq!(offset_of!(RxEntry, type_flags), 20);
        assert_eq!(offset_of!(RxEntry, has_ts), 22);
        assert_eq!(offset_of!(RxEntry, data), 24);
        assert_eq!(offset_of!(RxBatch, count), 0);
        assert_eq!(offset_of!(RxBatch, dropped), 4);
        assert_eq!(offset_of!(RxBatch, e), 8);
    }

    #[test]
    fn selectors_match_header() {
        assert_eq!(sel::SHUTDOWN_DATAPATH, 9);
        assert_eq!(sel::COUNT, 10);
    }
}
