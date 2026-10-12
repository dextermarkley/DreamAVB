// SPDX-License-Identifier: MIT OR Apache-2.0

//! Rust mirror of `driver/AVBShared.h` (user-client ABI version 4).
//!
//! Keep in lock-step with the C header: the tests at the bottom assert the
//! same sizes and field offsets the C++ side `static_assert`s (design §3.1).

/// The ABI this build is written against.
///
/// A client MUST NOT refuse an older dext: v4 only adds [`sel::SEND_FRAMES`],
/// an optimisation of a path that already works per frame. A PCI-matched dext
/// does not terminate while it owns its device, so between `avb-installer
/// activate` and the reboot that loads the replacement the running dext is
/// still the old one — a client that demanded v4 would be dead in that window.
/// Compare against [`ABI_VERSION_SEND_FRAMES`] to pick the path instead.
pub const ABI_VERSION: u32 = 4;

/// First ABI version with [`sel::SEND_FRAMES`]; below this, batch by looping.
pub const ABI_VERSION_SEND_FRAMES: u32 = 4;

/// Oldest dext ABI this client can drive.
///
/// v3 and v4 differ only by the addition of [`sel::SEND_FRAMES`], and every
/// v3 layout is unchanged, so a v4 client drives a v3 dext correctly as long
/// as it does not call the new selector. Accepting the range is not laxity: a
/// PCI-matched dext does not terminate while it owns its device, so a dext
/// upgrade only takes effect at the next boot. Between `avb-installer
/// activate` and that boot the kernel is still running the old build, and a
/// client that insisted on an exact match would refuse to start at all.
pub const ABI_VERSION_MIN: u32 = 3;

/// Whether a dext reporting ABI `v` can be driven by this build.
pub const fn abi_supported(v: u32) -> bool {
    v >= ABI_VERSION_MIN && v <= ABI_VERSION
}

/// Whether a dext reporting ABI `v` implements [`sel::SEND_FRAMES`].
pub const fn abi_has_send_frames(v: u32) -> bool {
    v >= ABI_VERSION_SEND_FRAMES
}

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
    /// in: delta_ns (i64 as u64); out: before_ns, after_ns, mach_mid.
    pub const STEP_CLOCK: u32 = 10;
    /// out: struct Counters (512 B).
    pub const GET_COUNTERS: u32 = 11;
    /// in: struct StreamFilter (136 B).
    pub const SET_STREAM_FILTER: u32 = 12;
    /// in: flags (must be 0); out: generation, rx_mem_size, consumer_mem_size.
    pub const RING_ATTACH: u32 = 13;
    pub const RING_DETACH: u32 = 14;
    /// in: packed `TxBatch` (see `super::tx_batch`); out: accepted, mach.
    pub const SEND_FRAMES: u32 = 15;
    pub const COUNT: u32 = 16;
    /// Selectors every NIC dext implements (the BCM dext: only these).
    /// 32..47 are reserved for the talker path.
    pub const BASE_COUNT: u32 = 10;
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
pub const FLAG_RINGS_ATTACHED: u32 = 1 << 5;
/// Some client owns the datapath.
pub const FLAG_OWNED: u32 = 1 << 6;

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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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

// ---------------------------------------------------------------------------
// ABI v3: shared-memory RX rings (RingAttach + CopyClientMemoryForType).

/// `CopyClientMemoryForType` types (`IOConnectMapMemory64` memoryType).
pub mod mem {
    /// ShmHeader + control slots + audio slots; the client maps it read-only.
    pub const RX: u32 = 1;
    /// ShmConsumer; the client maps it read-write.
    pub const CONSUMER: u32 = 2;
}

/// "AVB3" in little-endian byte order.
pub const SHM_MAGIC: u32 = 0x3342_5641;
/// One arm64 page; ShmHeader at offset 0.
pub const SHM_HEADER_SIZE: u32 = 16384;
pub const RX_SLOT_SIZE: u32 = 2048;
pub const RX_SLOT_HDR: u32 = 64;
/// >= 1522 (VLAN-tagged maximum frame).
pub const RX_SLOT_DATA_MAX: u32 = 1984;
pub const RX_CTL_SLOTS: u32 = 512;
pub const RX_AUDIO_SLOTS: u32 = 8192;
pub const RX_CTL_OFFSET: u32 = SHM_HEADER_SIZE;
pub const RX_AUDIO_OFFSET: u32 = RX_CTL_OFFSET + RX_CTL_SLOTS * RX_SLOT_SIZE;
/// 17 842 176 bytes.
pub const RX_MEM_SIZE: u32 = RX_AUDIO_OFFSET + RX_AUDIO_SLOTS * RX_SLOT_SIZE;
pub const CONSUMER_MEM_SIZE: u32 = 16384;
/// S_max.
pub const MAX_STREAMS: u32 = 16;
pub const STEP_CLOCK_MAX_NS: i64 = 1_000_000_000;
/// The 16R's AAF audio streams per direction (8 channels each, 64 total). The
/// TX ceiling has to clear all of them at once.
pub const TX_MAX_CLASS_A_STREAMS: u32 = 8;
/// Dext TX token bucket (SAF-6). Shared across ALL transmitted frames, audio
/// and control alike, so it must clear every SR class A stream we talk at once
/// (8000 frames/s each, one per 125 us observation interval) with headroom
/// left for gPTP, MSRP, MVRP and AVDECC. See the derivation in
/// `driver/AVBShared.h`.
///
/// Changing this requires REINSTALLING the dext: it is compiled into the
/// driver, not supplied by the client.
pub const TX_RATE_PER_SEC: u32 = 80000;
// Compile-time, so the bound holds even if the test below is ever removed.
// Mirrors the `static_assert`s in `driver/AVBShared.h` / `I210Logic.hpp`.
const _: () = {
    assert!(
        TX_RATE_PER_SEC > TX_MAX_CLASS_A_STREAMS * 8000,
        "the TX ceiling is shared across every frame we send, so it must clear \
         EVERY class A stream we talk at once (8000 frames/s each) with room \
         left for control traffic"
    );
    assert!(
        1_000_000_000 % TX_RATE_PER_SEC == 0,
        "the token period must be an integer number of nanoseconds"
    );
};
pub const TX_BURST: u32 = 64;

/// Packed encoder for [`sel::SEND_FRAMES`].
///
/// Mirrors `parseTxBatch` in `driver/AVBI210Driver/I210Logic.hpp`; the layout
/// is documented on `AvbTxBatchHeader` in `driver/AVBShared.h`. Encoding and
/// decoding live on opposite sides of the user-client boundary, so neither can
/// be checked by round-tripping against itself — the tests here assert the
/// byte layout against literals transcribed from the C header, which is the
/// only form that disagrees with a mistake made on one side.
pub mod tx_batch {
    use super::{FRAME_MAX_LEN, TX_BURST, TX_MAX_CLASS_A_STREAMS};

    /// Frames one call may carry.
    pub const MAX: usize = 16;
    /// Bytes of the fixed header: `count` then `total_bytes`.
    pub const HEADER_LEN: usize = 8;
    /// Bytes of each per-frame entry header: `len` then `reserved`.
    pub const ENTRY_LEN: usize = 4;

    const _: () = assert!(
        MAX >= TX_MAX_CLASS_A_STREAMS as usize,
        "a batch must hold one frame per class A stream, or the talker is back \
         to several user-client calls per 125 us interval"
    );
    const _: () = assert!(
        MAX <= TX_BURST as usize,
        "a full batch must fit the token burst, so a correctly paced talker is \
         never throttled part-way through an interval"
    );

    const fn round_up4(n: usize) -> usize {
        (n + 3) & !3
    }

    /// Worst case: every entry a maximum-length frame.
    pub const BYTES_MAX: usize = HEADER_LEN + MAX * (ENTRY_LEN + round_up4(FRAME_MAX_LEN));

    /// Bytes `count` frames of `len` each occupy.
    pub const fn bytes_for(count: usize, len: usize) -> usize {
        HEADER_LEN + count * (ENTRY_LEN + round_up4(len))
    }

    /// Why a batch could not be encoded. Each is a programming error on this
    /// side, not a device condition.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum EncodeError {
        /// No frames, or more than [`MAX`].
        Count(usize),
        /// A frame shorter than an Ethernet header or longer than the MTU.
        FrameLen { index: usize, len: usize },
    }

    impl core::fmt::Display for EncodeError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                Self::Count(n) => write!(f, "batch of {n} frames (must be 1..={MAX})"),
                Self::FrameLen { index, len } => {
                    write!(f, "frame {index} is {len} B (must be 14..={FRAME_MAX_LEN})")
                }
            }
        }
    }

    impl core::error::Error for EncodeError {}

    /// Encodes `frames` into `buf`, returning the used length.
    ///
    /// `buf` must be at least [`BYTES_MAX`]; the padding bytes between frames
    /// are zeroed rather than left as whatever the caller's buffer held, so
    /// the same frames always produce the same bytes.
    pub fn encode(buf: &mut [u8], frames: &[&[u8]]) -> Result<usize, EncodeError> {
        if frames.is_empty() || frames.len() > MAX {
            return Err(EncodeError::Count(frames.len()));
        }
        for (index, f) in frames.iter().enumerate() {
            if f.len() < 14 || f.len() > FRAME_MAX_LEN {
                return Err(EncodeError::FrameLen {
                    index,
                    len: f.len(),
                });
            }
        }
        let total: usize = HEADER_LEN
            + frames
                .iter()
                .map(|f| ENTRY_LEN + round_up4(f.len()))
                .sum::<usize>();
        assert!(
            buf.len() >= total,
            "batch buffer is {} B, need {total} B",
            buf.len()
        );
        buf[..total].fill(0);
        buf[0..4].copy_from_slice(&(frames.len() as u32).to_le_bytes());
        buf[4..8].copy_from_slice(&(total as u32).to_le_bytes());
        let mut at = HEADER_LEN;
        for f in frames {
            buf[at..at + 2].copy_from_slice(&(f.len() as u16).to_le_bytes());
            buf[at + 2..at + 4].copy_from_slice(&0u16.to_le_bytes());
            at += ENTRY_LEN;
            buf[at..at + f.len()].copy_from_slice(f);
            at += round_up4(f.len());
        }
        debug_assert_eq!(at, total);
        Ok(total)
    }
}

/// `AvbRxSlot.cls`.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RxClass {
    None = 0,
    Gptp = 1,
    Msrp = 2,
    Mvrp = 3,
    Avdecc = 4,
    Maap = 5,
    Stream = 6,
}

/// kAvbRxNone..kAvbRxStream.
pub const RX_CLASS_COUNT: u32 = 7;

impl RxClass {
    pub const fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::None,
            1 => Self::Gptp,
            2 => Self::Msrp,
            3 => Self::Mvrp,
            4 => Self::Avdecc,
            5 => Self::Maap,
            6 => Self::Stream,
            _ => return None,
        })
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Gptp => "gptp",
            Self::Msrp => "msrp",
            Self::Mvrp => "mvrp",
            Self::Avdecc => "avdecc",
            Self::Maap => "maap",
            Self::Stream => "stream",
        }
    }
}

/// `AvbRxSlot.flags`: hw_ts_ns valid (SYSTIM ns, in-buffer timestamp).
pub const RXF_TS_HW: u16 = 1 << 0;
/// Set iff data[12..14] == 0x8100 (tag present in data[]).
pub const RXF_VLAN_FRAME: u16 = 1 << 1;
/// Descriptor VP set and no in-buffer tag; vlan_tci holds the tag. Never set
/// together with RXF_VLAN_FRAME.
pub const RXF_VLAN_DESC: u16 = 1 << 2;
/// Harvested inside a SYSTIM step's settling window: hw_ts_ns may belong to
/// either timebase; never use it for timing.
pub const RXF_TS_STEP_AMBIG: u16 = 1 << 3;
/// Settling window after a SYSTIM write.
pub const STEP_SETTLE_NS: u32 = 1_000_000;

/// One shared RX ring slot (2048 B).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RxSlot {
    /// SYSTIM ns of reception (if RXF_TS_HW).
    pub hw_ts_ns: u64,
    /// mach_absolute_time() at harvest.
    pub harvest_mach: u64,
    /// Ring record index (== producer head at write).
    pub index: u64,
    /// Advanced RX write-back status_error, verbatim.
    pub status_error: u32,
    /// Bytes in data[] (no FCS, no TS header).
    pub len: u16,
    /// Descriptor VLAN field (valid if RXF_VLAN_DESC).
    pub vlan_tci: u16,
    /// RXF_*.
    pub flags: u16,
    /// RxClass.
    pub cls: u8,
    pub pad0: u8,
    /// Raw PKT_LEN (includes the 16 B TS header if TSIP).
    pub desc_len: u32,
    /// Dext clock generation at harvest.
    pub clock_gen: u32,
    pub pad1: [u8; 20],
    pub data: [u8; RX_SLOT_DATA_MAX as usize],
}

impl Default for RxSlot {
    fn default() -> Self {
        Self {
            hw_ts_ns: 0,
            harvest_mach: 0,
            index: 0,
            status_error: 0,
            len: 0,
            vlan_tci: 0,
            flags: 0,
            cls: 0,
            pad0: 0,
            desc_len: 0,
            clock_gen: 0,
            pad1: [0; 20],
            data: [0; RX_SLOT_DATA_MAX as usize],
        }
    }
}

/// Indices into `Counters::c` (AvbCounterIndex). All monotonic for the life
/// of the datapath (reset only by InitDatapath), except HARVEST_MAX_BATCH.
pub mod ctr {
    /// EOP descriptors harvested.
    pub const RX_FRAMES: usize = 0;
    /// Delivered per RxClass: index = cls (Gptp 1 .. Stream 6); 7 reserved.
    pub const RX_CLASS_BASE: usize = 1;
    pub const RX_DROP_OTHER: usize = 8;
    pub const RX_DROP_UNBOUND_STREAM: usize = 9;
    pub const RX_DROP_UNTAGGED_STREAM: usize = 10;
    pub const RX_ERRORS: usize = 11;
    pub const RX_NON_EOP: usize = 12;
    pub const RING_OVERRUN_CTL: usize = 13;
    pub const RING_OVERRUN_AUDIO: usize = 14;
    pub const CONSUMER_INDEX_INVALID: usize = 15;
    pub const HW_MPC: usize = 16;
    pub const HW_RNBC: usize = 17;
    pub const HW_RQDPC0: usize = 18;
    pub const HW_CRCERRS: usize = 19;
    pub const TX_FRAMES: usize = 20;
    pub const TX_RATE_LIMITED: usize = 21;
    pub const TX_TS_REQUESTED: usize = 22;
    pub const TX_TS_OK: usize = 23;
    pub const TX_TS_TIMEOUT: usize = 24;
    pub const TX_TS_BUSY_REJECTED: usize = 25;
    pub const TX_STALE_LATCH: usize = 26;
    pub const LINK_UP_EVENTS: usize = 27;
    pub const LINK_DOWN_EVENTS: usize = 28;
    pub const HARVEST_TICKS: usize = 29;
    /// GAUGE: max descriptors in one tick.
    pub const HARVEST_MAX_BATCH: usize = 30;
    /// Tick-to-tick gap > 2 ms.
    pub const HARVEST_LATE_TICKS: usize = 31;
    /// Phase 0 software ring (PollRx mode).
    pub const LEGACY_SW_OVERRUN: usize = 32;
    /// Payload after the TS header > RX_SLOT_DATA_MAX: discarded.
    pub const RX_OVERSIZE: usize = 33;
    /// `SEND_FRAMES` calls that queued at least one frame.
    pub const TX_BATCHES: usize = 34;
    /// Of those, calls where a later frame in the batch was refused.
    pub const TX_BATCH_PARTIAL: usize = 35;
    /// Calls refused whole: malformed batch, or a timestamp requested.
    pub const TX_BATCH_REJECTED: usize = 36;
    pub const COUNT: usize = 64;
}

/// Every defined counter index with its stable name, ascending. Index 7 is
/// reserved and has no name.
pub const COUNTER_NAMES: [(usize, &str); 36] = [
    (ctr::RX_FRAMES, "rx_frames"),
    (1, "rx_gptp"),
    (2, "rx_msrp"),
    (3, "rx_mvrp"),
    (4, "rx_avdecc"),
    (5, "rx_maap"),
    (6, "rx_stream"),
    (ctr::RX_DROP_OTHER, "rx_drop_other"),
    (ctr::RX_DROP_UNBOUND_STREAM, "rx_drop_unbound_stream"),
    (ctr::RX_DROP_UNTAGGED_STREAM, "rx_drop_untagged_stream"),
    (ctr::RX_ERRORS, "rx_errors"),
    (ctr::RX_NON_EOP, "rx_non_eop"),
    (ctr::RING_OVERRUN_CTL, "ring_overrun_ctl"),
    (ctr::RING_OVERRUN_AUDIO, "ring_overrun_audio"),
    (ctr::CONSUMER_INDEX_INVALID, "consumer_index_invalid"),
    (ctr::HW_MPC, "hw_mpc"),
    (ctr::HW_RNBC, "hw_rnbc"),
    (ctr::HW_RQDPC0, "hw_rqdpc0"),
    (ctr::HW_CRCERRS, "hw_crcerrs"),
    (ctr::TX_FRAMES, "tx_frames"),
    (ctr::TX_RATE_LIMITED, "tx_rate_limited"),
    (ctr::TX_TS_REQUESTED, "tx_ts_requested"),
    (ctr::TX_TS_OK, "tx_ts_ok"),
    (ctr::TX_TS_TIMEOUT, "tx_ts_timeout"),
    (ctr::TX_TS_BUSY_REJECTED, "tx_ts_busy_rejected"),
    (ctr::TX_STALE_LATCH, "tx_stale_latch"),
    (ctr::LINK_UP_EVENTS, "link_up_events"),
    (ctr::LINK_DOWN_EVENTS, "link_down_events"),
    (ctr::HARVEST_TICKS, "harvest_ticks"),
    (ctr::HARVEST_MAX_BATCH, "harvest_max_batch"),
    (ctr::HARVEST_LATE_TICKS, "harvest_late_ticks"),
    (ctr::LEGACY_SW_OVERRUN, "legacy_sw_overrun"),
    (ctr::RX_OVERSIZE, "rx_oversize"),
    (ctr::TX_BATCHES, "tx_batches"),
    (ctr::TX_BATCH_PARTIAL, "tx_batch_partial"),
    (ctr::TX_BATCH_REJECTED, "tx_batch_rejected"),
];

pub fn counter_name(index: usize) -> Option<&'static str> {
    COUNTER_NAMES
        .iter()
        .find(|(i, _)| *i == index)
        .map(|(_, n)| *n)
}

/// The per-class delivered counter for `cls` (the only class -> counter map).
pub const fn class_counter_index(cls: RxClass) -> usize {
    cls as usize
}

const _: () = assert!(
    class_counter_index(RxClass::Gptp) == 1 && class_counter_index(RxClass::Stream) == 6,
    "class counter index = cls"
);
const _: () = assert!(
    ctr::RX_CLASS_BASE + RX_CLASS_COUNT as usize == ctr::RX_DROP_OTHER,
    "class counters 1..7"
);
const _: () = assert!(ctr::TX_BATCH_REJECTED < ctr::COUNT, "counter space");
const _: () = assert!(RX_MEM_SIZE == 17_842_176, "rx mem size");
const _: () = assert!(
    RX_CTL_SLOTS.is_power_of_two() && RX_AUDIO_SLOTS.is_power_of_two(),
    "pow2"
);

/// AvbCounters (512 B).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counters {
    pub c: [u64; ctr::COUNT],
}

impl Default for Counters {
    fn default() -> Self {
        Self { c: [0; ctr::COUNT] }
    }
}

impl Counters {
    /// Counter `index`, 0 if out of range.
    pub fn get(&self, index: usize) -> u64 {
        self.c.get(index).copied().unwrap_or(0)
    }
}

/// AvbShmHeader (1024 B at offset 0 of mem::RX).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ShmHeader {
    pub magic: u32,
    pub abi_version: u32,
    pub header_size: u32,
    pub slot_size: u32,
    pub ctl_slots: u32,
    pub audio_slots: u32,
    pub ctl_offset: u32,
    pub audio_offset: u32,
    pub total_size: u32,
    /// RingAttach count since dext Start.
    pub generation: u32,
    /// Live AvbStage.
    pub stage: u32,
    /// Live AvbFault.
    pub fault: u32,
    /// Live AVB_FLAG_*.
    pub flags: u32,
    /// 1 while the harvester writes slots.
    pub producer_running: u32,
    /// mach of the last harvest tick.
    pub last_tick_mach: u64,
    pub mac: [u8; 6],
    pub pad0: [u8; 58],
    /// Records written (monotonic), release-stored.
    pub ctl_head: u64,
    pub pad1: [u8; 120],
    pub audio_head: u64,
    pub pad2: [u8; 120],
    pub counters: Counters,
    /// Incremented by every SYSTIM write; release-stored.
    pub clock_gen: u32,
    pub reserved: [u8; 124],
}

impl ShmHeader {
    /// A header with every constant field as the dext writes it at
    /// RingAttach, and everything else zero.
    pub fn expected(generation: u32) -> Self {
        Self {
            magic: SHM_MAGIC,
            abi_version: ABI_VERSION,
            header_size: SHM_HEADER_SIZE,
            slot_size: RX_SLOT_SIZE,
            ctl_slots: RX_CTL_SLOTS,
            audio_slots: RX_AUDIO_SLOTS,
            ctl_offset: RX_CTL_OFFSET,
            audio_offset: RX_AUDIO_OFFSET,
            total_size: RX_MEM_SIZE,
            generation,
            stage: 0,
            fault: 0,
            flags: 0,
            producer_running: 0,
            last_tick_mach: 0,
            mac: [0; 6],
            pad0: [0; 58],
            ctl_head: 0,
            pad1: [0; 120],
            audio_head: 0,
            pad2: [0; 120],
            counters: Counters::default(),
            clock_gen: 0,
            reserved: [0; 124],
        }
    }
}

/// AvbShmConsumer (256 B at offset 0 of mem::CONSUMER).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ShmConsumer {
    /// Records consumed (monotonic), release-stored by the daemon.
    pub ctl_tail: u64,
    pub pad0: [u8; 120],
    pub audio_tail: u64,
    pub pad1: [u8; 120],
}

/// AvbStreamFilterEntry (8 B).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamFilterEntry {
    /// Must have the multicast bit set.
    pub dest_mac: [u8; 6],
    /// 1..=4094; must match the frame's tag.
    pub vid: u16,
}

/// AvbStreamFilter (136 B), the SetStreamFilter structIn.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamFilter {
    /// 0..=MAX_STREAMS.
    pub count: u32,
    pub pad: u32,
    pub e: [StreamFilterEntry; MAX_STREAMS as usize],
}

impl StreamFilter {
    /// `None` if there are more than MAX_STREAMS entries.
    pub fn from_entries(entries: &[StreamFilterEntry]) -> Option<Self> {
        let mut f = Self::default();
        f.e.get_mut(..entries.len())?.copy_from_slice(entries);
        f.count = entries.len() as u32;
        Some(f)
    }

    pub fn entries(&self) -> &[StreamFilterEntry] {
        &self.e[..(self.count as usize).min(MAX_STREAMS as usize)]
    }

    /// The dext's acceptance rule (`streamFilterValid`): count <= 16, every
    /// destination multicast, 1 <= vid <= 4094.
    pub fn is_valid(&self) -> bool {
        self.count <= MAX_STREAMS
            && self
                .entries()
                .iter()
                .all(|e| e.dest_mac[0] & 1 != 0 && (1..=4094).contains(&e.vid))
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
        assert_eq!(ABI_VERSION, 4);
        assert_eq!(ABI_VERSION_SEND_FRAMES, 4);
        assert_eq!(sel::SHUTDOWN_DATAPATH, 9);
        assert_eq!(sel::STEP_CLOCK, 10);
        assert_eq!(sel::GET_COUNTERS, 11);
        assert_eq!(sel::SET_STREAM_FILTER, 12);
        assert_eq!(sel::RING_ATTACH, 13);
        assert_eq!(sel::RING_DETACH, 14);
        assert_eq!(sel::SEND_FRAMES, 15);
        assert_eq!(sel::COUNT, 16);
        assert_eq!(sel::BASE_COUNT, 10);
        assert_eq!(mem::RX, 1);
        assert_eq!(mem::CONSUMER, 2);
        assert_eq!(FLAG_RINGS_ATTACHED, 0x20);
        assert_eq!(FLAG_OWNED, 0x40);
    }

    #[test]
    fn shm_constants_match_header() {
        assert_eq!(SHM_MAGIC.to_le_bytes(), *b"AVB3");
        assert_eq!(SHM_HEADER_SIZE, 16384);
        assert_eq!(RX_SLOT_SIZE, 2048);
        assert_eq!(RX_SLOT_HDR, 64);
        assert_eq!(RX_SLOT_DATA_MAX, 1984);
        assert_eq!(RX_SLOT_HDR + RX_SLOT_DATA_MAX, RX_SLOT_SIZE);
        assert_eq!(RX_CTL_SLOTS, 512);
        assert_eq!(RX_AUDIO_SLOTS, 8192);
        assert_eq!(RX_CTL_OFFSET, 16384);
        assert_eq!(RX_AUDIO_OFFSET, 1_064_960);
        assert_eq!(RX_MEM_SIZE, 17_842_176);
        assert_eq!(CONSUMER_MEM_SIZE, 16384);
        assert_eq!(MAX_STREAMS, 16);
        assert_eq!(STEP_CLOCK_MAX_NS, 1_000_000_000);
        assert_eq!(TX_RATE_PER_SEC, 80000);
        assert_eq!(TX_MAX_CLASS_A_STREAMS, 8);
        assert_eq!(TX_BURST, 64);
        assert_eq!(RX_CLASS_COUNT, 7);
        assert_eq!(RXF_TS_HW, 1);
        assert_eq!(RXF_VLAN_FRAME, 2);
        assert_eq!(RXF_VLAN_DESC, 4);
        assert_eq!(RXF_TS_STEP_AMBIG, 8);
        assert_eq!(STEP_SETTLE_NS, 1_000_000);
    }

    #[test]
    fn shm_sizes_and_offsets() {
        assert_eq!(size_of::<RxSlot>(), RX_SLOT_SIZE as usize);
        assert_eq!(offset_of!(RxSlot, hw_ts_ns), 0);
        assert_eq!(offset_of!(RxSlot, harvest_mach), 8);
        assert_eq!(offset_of!(RxSlot, index), 16);
        assert_eq!(offset_of!(RxSlot, status_error), 24);
        assert_eq!(offset_of!(RxSlot, len), 28);
        assert_eq!(offset_of!(RxSlot, vlan_tci), 30);
        assert_eq!(offset_of!(RxSlot, flags), 32);
        assert_eq!(offset_of!(RxSlot, cls), 34);
        assert_eq!(offset_of!(RxSlot, pad0), 35);
        assert_eq!(offset_of!(RxSlot, desc_len), 36);
        assert_eq!(offset_of!(RxSlot, clock_gen), 40);
        assert_eq!(offset_of!(RxSlot, pad1), 44);
        assert_eq!(offset_of!(RxSlot, data), RX_SLOT_HDR as usize);

        assert_eq!(size_of::<Counters>(), 512);
        assert!(size_of::<Counters>() <= 4096);

        assert_eq!(size_of::<ShmHeader>(), 1024);
        assert!(size_of::<ShmHeader>() <= SHM_HEADER_SIZE as usize);
        assert_eq!(std::mem::align_of::<ShmHeader>(), 8);
        assert_eq!(offset_of!(ShmHeader, magic), 0);
        assert_eq!(offset_of!(ShmHeader, abi_version), 4);
        assert_eq!(offset_of!(ShmHeader, header_size), 8);
        assert_eq!(offset_of!(ShmHeader, slot_size), 12);
        assert_eq!(offset_of!(ShmHeader, ctl_slots), 16);
        assert_eq!(offset_of!(ShmHeader, audio_slots), 20);
        assert_eq!(offset_of!(ShmHeader, ctl_offset), 24);
        assert_eq!(offset_of!(ShmHeader, audio_offset), 28);
        assert_eq!(offset_of!(ShmHeader, total_size), 32);
        assert_eq!(offset_of!(ShmHeader, generation), 36);
        assert_eq!(offset_of!(ShmHeader, stage), 40);
        assert_eq!(offset_of!(ShmHeader, fault), 44);
        assert_eq!(offset_of!(ShmHeader, flags), 48);
        assert_eq!(offset_of!(ShmHeader, producer_running), 52);
        assert_eq!(offset_of!(ShmHeader, last_tick_mach), 56);
        assert_eq!(offset_of!(ShmHeader, mac), 64);
        assert_eq!(offset_of!(ShmHeader, pad0), 70);
        assert_eq!(offset_of!(ShmHeader, ctl_head), 128);
        assert_eq!(offset_of!(ShmHeader, pad1), 136);
        assert_eq!(offset_of!(ShmHeader, audio_head), 256);
        assert_eq!(offset_of!(ShmHeader, pad2), 264);
        assert_eq!(offset_of!(ShmHeader, counters), 384);
        assert_eq!(offset_of!(ShmHeader, clock_gen), 896);
        assert_eq!(offset_of!(ShmHeader, reserved), 900);

        assert_eq!(size_of::<ShmConsumer>(), 256);
        assert_eq!(offset_of!(ShmConsumer, ctl_tail), 0);
        assert_eq!(offset_of!(ShmConsumer, audio_tail), 128);
        assert!(size_of::<ShmConsumer>() <= CONSUMER_MEM_SIZE as usize);

        assert_eq!(size_of::<StreamFilterEntry>(), 8);
        assert_eq!(offset_of!(StreamFilterEntry, dest_mac), 0);
        assert_eq!(offset_of!(StreamFilterEntry, vid), 6);
        assert_eq!(size_of::<StreamFilter>(), 136);
        assert_eq!(offset_of!(StreamFilter, count), 0);
        assert_eq!(offset_of!(StreamFilter, pad), 4);
        assert_eq!(offset_of!(StreamFilter, e), 8);
        assert!(size_of::<StreamFilter>() <= 4096);
    }

    #[test]
    fn counter_indices_and_names() {
        assert_eq!(ctr::RX_FRAMES, 0);
        assert_eq!(ctr::RX_CLASS_BASE, 1);
        assert_eq!(ctr::RX_DROP_OTHER, 8);
        assert_eq!(ctr::RX_ERRORS, 11);
        assert_eq!(ctr::CONSUMER_INDEX_INVALID, 15);
        assert_eq!(ctr::HW_CRCERRS, 19);
        assert_eq!(ctr::TX_FRAMES, 20);
        assert_eq!(ctr::LINK_DOWN_EVENTS, 28);
        assert_eq!(ctr::LEGACY_SW_OVERRUN, 32);
        assert_eq!(ctr::RX_OVERSIZE, 33);
        assert_eq!(ctr::TX_BATCHES, 34);
        assert_eq!(ctr::TX_BATCH_PARTIAL, 35);
        assert_eq!(ctr::TX_BATCH_REJECTED, 36);
        assert_eq!(ctr::COUNT, 64);
        assert_eq!(class_counter_index(RxClass::Gptp), 1);
        assert_eq!(class_counter_index(RxClass::Msrp), 2);
        assert_eq!(class_counter_index(RxClass::Mvrp), 3);
        assert_eq!(class_counter_index(RxClass::Avdecc), 4);
        assert_eq!(class_counter_index(RxClass::Maap), 5);
        assert_eq!(class_counter_index(RxClass::Stream), 6);
        // Names: every index 0..=36 except the reserved 7, ascending, unique.
        let idx: Vec<usize> = COUNTER_NAMES.iter().map(|(i, _)| *i).collect();
        let want: Vec<usize> = (0..=ctr::TX_BATCH_REJECTED).filter(|i| *i != 7).collect();
        assert_eq!(idx, want);
        let mut names: Vec<&str> = COUNTER_NAMES.iter().map(|(_, n)| *n).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), COUNTER_NAMES.len());
        for c in 1..=6u8 {
            let cls = RxClass::from_u8(c).unwrap();
            assert_eq!(
                counter_name(class_counter_index(cls)),
                Some(format!("rx_{}", cls.name()).as_str())
            );
        }
        assert_eq!(counter_name(7), None);
        assert_eq!(counter_name(ctr::RX_OVERSIZE), Some("rx_oversize"));
        assert_eq!(RxClass::from_u8(7), None);
        assert_eq!(RxClass::from_u8(0), Some(RxClass::None));
    }

    /// The encoded bytes are checked against literals transcribed from
    /// `AvbTxBatchHeader` / `AvbTxBatchEntry` in `driver/AVBShared.h`, NOT
    /// against a decoder of our own. Encoder and decoder sit on opposite sides
    /// of the user-client boundary, so a shared misunderstanding of the layout
    /// would round-trip perfectly and still be refused by the dext — the same
    /// tautology that let an inverted AVTP header and a guessed FourCC through.
    #[test]
    fn tx_batch_encodes_the_documented_layout() {
        use tx_batch::{bytes_for, encode, EncodeError, BYTES_MAX, ENTRY_LEN, HEADER_LEN, MAX};

        let mut buf = vec![0xAAu8; BYTES_MAX];
        // Two frames, the first 15 B so its 4-byte padding is exercised.
        let a: Vec<u8> = (0..15u8).collect();
        let b: Vec<u8> = (100..116u8).collect();
        let n = encode(&mut buf, &[&a, &b]).unwrap();

        // header: count = 2, total_bytes = n, both little-endian u32.
        assert_eq!(&buf[0..4], &2u32.to_le_bytes());
        assert_eq!(&buf[4..8], &(n as u32).to_le_bytes());
        // entry 0: len = 15, reserved = 0, then 15 bytes + one zero pad byte.
        assert_eq!(&buf[8..10], &15u16.to_le_bytes());
        assert_eq!(&buf[10..12], &0u16.to_le_bytes());
        assert_eq!(&buf[12..27], &a[..]);
        assert_eq!(
            buf[27], 0,
            "padding must be zeroed, not left as caller bytes"
        );
        // entry 1 starts on the next 4-byte boundary.
        assert_eq!(&buf[28..30], &16u16.to_le_bytes());
        assert_eq!(&buf[30..32], &0u16.to_le_bytes());
        assert_eq!(&buf[32..48], &b[..]);
        assert_eq!(n, 48);
        assert_eq!(n, HEADER_LEN + (ENTRY_LEN + 16) + (ENTRY_LEN + 16));

        // The real shape: one class A interval of the 16R's eight streams.
        let frame = vec![0x5Au8; 234];
        let frames: Vec<&[u8]> = (0..8).map(|_| frame.as_slice()).collect();
        let n = encode(&mut buf, &frames).unwrap();
        assert_eq!(n, bytes_for(8, 234));
        assert_eq!(&buf[0..4], &8u32.to_le_bytes());
        // Frame i begins past i whole (entry + padded frame) blocks.
        for i in 0..8usize {
            let at = HEADER_LEN + i * (ENTRY_LEN + 236);
            assert_eq!(&buf[at..at + 2], &234u16.to_le_bytes());
            assert_eq!(buf[at + ENTRY_LEN], 0x5A);
            assert_eq!(buf[at + ENTRY_LEN + 234], 0, "frame {i} pad byte 0");
            assert_eq!(buf[at + ENTRY_LEN + 235], 0, "frame {i} pad byte 1");
        }

        // A full batch of maximum-length frames is exactly the worst case the
        // ABI reserves, so the dext's length guard and ours agree.
        let big = vec![0u8; FRAME_MAX_LEN];
        let frames: Vec<&[u8]> = (0..MAX).map(|_| big.as_slice()).collect();
        assert_eq!(encode(&mut buf, &frames).unwrap(), BYTES_MAX);

        // Refusals are our own programming errors, caught before any syscall.
        assert_eq!(encode(&mut buf, &[]), Err(EncodeError::Count(0)));
        let frames: Vec<&[u8]> = (0..MAX + 1).map(|_| frame.as_slice()).collect();
        assert_eq!(encode(&mut buf, &frames), Err(EncodeError::Count(MAX + 1)));
        let short = [0u8; 13];
        assert_eq!(
            encode(&mut buf, &[&short]),
            Err(EncodeError::FrameLen { index: 0, len: 13 })
        );
        let long = vec![0u8; FRAME_MAX_LEN + 1];
        assert_eq!(
            encode(&mut buf, &[&frame, &long]),
            Err(EncodeError::FrameLen {
                index: 1,
                len: FRAME_MAX_LEN + 1
            })
        );
    }

    #[test]
    fn stream_filter_rules() {
        let ok = StreamFilterEntry {
            dest_mac: [0x91, 0xE0, 0xF0, 0x00, 0x12, 0x34],
            vid: 2,
        };
        let f = StreamFilter::from_entries(&[ok, ok]).unwrap();
        assert_eq!(f.count, 2);
        assert_eq!(f.entries(), &[ok, ok]);
        assert!(f.is_valid());
        assert!(StreamFilter::default().is_valid());
        assert!(StreamFilter::from_entries(&[ok; 16]).unwrap().is_valid());
        assert!(StreamFilter::from_entries(&[ok; 17]).is_none());
        let unicast = StreamFilterEntry {
            dest_mac: [0x00, 0x0a, 0x92, 0x01, 0x02, 0x03],
            ..ok
        };
        assert!(!StreamFilter::from_entries(&[unicast]).unwrap().is_valid());
        for vid in [0, 4095] {
            let e = StreamFilterEntry { vid, ..ok };
            assert!(!StreamFilter::from_entries(&[e]).unwrap().is_valid());
        }
        let f = StreamFilter {
            count: 17,
            ..StreamFilter::default()
        };
        assert!(!f.is_valid());
    }
}
