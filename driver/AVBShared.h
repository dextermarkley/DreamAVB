// AVBShared.h - user-client ABI shared by the AVB dexts (AVBBcmDriver,
// AVBI210Driver) and the Rust user-space tools (mirrored by hand in
// crates/avb-probe/src/abi.rs, moving to crates/avb-dext/src/abi.rs; both
// sides assert the struct sizes and offsets below).
//
// Rules:
//   * fixed-width types, natural alignment, no implicit padding;
//   * every structure output stays <= 4096 bytes so it travels inline;
//   * bump AVB_ABI_VERSION on any layout or selector change.
//
// There is deliberately no generic register-write selector. Every write a
// dext performs goes through a named, sequence-checked operation.

#ifndef AVB_SHARED_H
#define AVB_SHARED_H

#include <stddef.h>
#include <stdint.h>

#define AVB_ABI_VERSION 3u

// AvbInfo.nic_kind
#define AVB_NIC_BCM57762 1u
#define AVB_NIC_I210     2u

// External method selectors.
enum AvbSelector : uint32_t {
    kAvbSelGetInfo          = 0,  // in: -              out: struct AvbInfo
    kAvbSelReadClock        = 1,  // in: n (1..64)      out: struct n * AvbClockSample
    kAvbSelSetClock         = 2,  // in: ns             out: readback ns
    kAvbSelAdjFreq          = 3,  // in: ppb (i64)      out: raw frequency register value
    kAvbSelInitDatapath     = 4,  // in: flags          out: stage
    kAvbSelPollRx           = 5,  // in: ethertype,max  out: struct AvbRxBatch
    kAvbSelSendFrame        = 6,  // in: struct frame + want_ts   out: tx_seq, mach_at_doorbell
    kAvbSelGetTxTimestamp   = 7,  // in: tx_seq         out: status, hw_ts, mach_latched
    kAvbSelReadReg32        = 8,  // in: BAR0 offset    out: value
    kAvbSelShutdownDatapath = 9,  // in: -              out: -
    kAvbSelStepClock        = 10, // in: delta_ns (i64)        out: before_ns, after_ns, mach_mid
    kAvbSelGetCounters      = 11, // in: -                     out: struct AvbCounters
    kAvbSelSetStreamFilter  = 12, // in: struct AvbStreamFilter out: -
    kAvbSelRingAttach       = 13, // in: flags (must be 0)     out: generation, rx_mem_size, consumer_mem_size
    kAvbSelRingDetach       = 14, // in: -                     out: -
    kAvbSelCount            = 15,
    kAvbSelBaseCount        = 10, // implemented by every NIC dext (BCM: only these)
    // 32..47 reserved: talker TX ring / Qav (not built in Phase 1a)
};

// CopyClientMemoryForType types
enum AvbMemType : uint64_t {
    kAvbMemRx       = 1,  // AvbShmHeader + control slots + audio slots; client read-only
    kAvbMemConsumer = 2,  // AvbShmConsumer; client read-write
    // 3 reserved: talker audio TX ring
};

// Limits. Each dext may enforce tighter per-NIC limits (BCM register window
// 0x10000; I210 |ppb| <= 488281).
#define AVB_READ_CLOCK_MAX_SAMPLES 64u
#define AVB_ADJ_FREQ_MAX_PPB       1000000ll
#define AVB_RX_BATCH_MAX           24u
#define AVB_RX_COPY_BYTES          128u
#define AVB_FRAME_MIN_LEN          60u
#define AVB_FRAME_MAX_LEN          1514u
#define AVB_REG_READ_LIMIT         0x20000u  // max over all NICs

// kAvbSelInitDatapath flags.
#define AVB_INIT_FLAG_PROMISC      (1u << 0)
#define AVB_INIT_FLAG_RX_PTP_V2_L2 (1u << 1)

// AvbInfo.stage
enum AvbStage : uint32_t {
    kAvbStageNone     = 0,
    kAvbStageTimeSync = 1,
    kAvbStageDatapath = 2,
};

// AvbInfo.flags
#define AVB_FLAG_APE     (1u << 0)  // BCM only
#define AVB_FLAG_ASF     (1u << 1)  // BCM only
#define AVB_FLAG_LINK_UP (1u << 2)
#define AVB_FLAG_1000FD  (1u << 3)
#define AVB_FLAG_SIG_OK  (1u << 4)  // BCM only: NIC SRAM data signature present
#define AVB_FLAG_RINGS_ATTACHED (1u << 5)
#define AVB_FLAG_OWNED          (1u << 6)   // some client owns the datapath

// AvbInfo.fault (last fault code; once set, the dext refuses all MMIO until Stop).
enum AvbFault : uint32_t {
    kAvbFaultNone          = 0,
    kAvbFaultAllOnes       = 1,  // register read returned 0xFFFFFFFF (device gone/hung)
    kAvbFaultIdMismatch    = 2,  // config space IDs are not the dext's one device
    kAvbFaultFirmwarePoll  = 3,  // BCM: bootcode did not post ~MAGIC after core reset
    kAvbFaultBufmgr        = 4,  // BCM: BUFMGR enable poll timed out
    kAvbFaultHostcc        = 5,  // BCM: HOSTCC disable poll timed out
    kAvbFaultStopBlock     = 6,  // BCM: a block enable bit would not clear on shutdown
    kAvbFaultDma           = 7,  // DMA buffer / IODMACommand setup failed
    kAvbFaultMdio          = 8,  // MDIO busy/ready poll timed out
    kAvbFaultNoSignature   = 9,  // BCM: NIC SRAM DATA_SIG missing after reset
    kAvbFaultTxStall       = 10, // TX consumer did not advance
    kAvbFaultMasterDisable = 11, // I210: GIO master-disable poll timed out
    kAvbFaultResetTimeout  = 12, // I210: reset / config-done poll timed out
    kAvbFaultSwFwSync      = 13, // I210: SW/FW semaphore not acquired
    kAvbFaultQueueEnable   = 14, // I210: RXDCTL/TXDCTL enable or disable poll timed out
};

// kAvbSelGetTxTimestamp status
enum AvbTxTsStatus : uint32_t {
    kAvbTxTsPending      = 0,
    kAvbTxTsOk           = 1,
    kAvbTxTsTimeout      = 2,
    kAvbTxTsNotRequested = 3,
};

//                          BCM57762                  I210
//   hw_rev                 cfg 0xfc ASIC rev         PCI revision ID (cfg 0x08 & 0xff)
//   ctrl_a                 MISC_HOST_CTRL            CTRL
//   ctrl_b                 GRC_MODE                  STATUS
//   clock_ctl              EAV_REF_CLK_CORRECT_CTL   TIMINCA
struct AvbInfo {
    uint32_t abi_version;
    uint32_t vendor_device;   // config 0x00, e.g. 0x15338086
    uint32_t subsys;          // config 0x2c, e.g. 0x02141ab6
    uint32_t hw_rev;
    uint32_t ctrl_a;
    uint32_t ctrl_b;
    uint32_t stage;           // AvbStage
    uint32_t fault;           // AvbFault
    uint32_t flags;           // AVB_FLAG_*
    uint8_t  mac[6];
    uint8_t  pad[2];
    uint32_t tb_numer;        // dext-side mach_timebase_info
    uint32_t tb_denom;
    uint32_t nic_kind;        // AVB_NIC_*
    uint32_t clock_ctl;
};

struct AvbClockSample {
    uint64_t mach_before;     // mach_absolute_time() before the first clock read
    uint64_t eav_ns;          // NIC clock ns (EAV on BCM, SYSTIM sec*1e9+ns on I210)
    uint64_t mach_after;      // mach_absolute_time() after the last clock read
};

struct AvbRxEntry {
    uint64_t harvest_mach;    // mach_absolute_time() when the dext harvested the descriptor
    uint64_t hw_ts;           // NIC clock ns of reception if has_ts
    uint32_t len;             // frame length without FCS (and without the I210 TS header)
    uint16_t type_flags;      // low 16 bits of the NIC's RX status (BCM BD flags; I210 status_error)
    uint8_t  has_ts;
    uint8_t  pad;
    uint8_t  data[AVB_RX_COPY_BYTES];
};

struct AvbRxBatch {
    uint32_t count;
    uint32_t dropped;         // software-ring overruns since last poll
    struct AvbRxEntry e[AVB_RX_BATCH_MAX];
};

// ---------------------------------------------------------------------------
// ABI v3: shared-memory RX rings (kAvbSelRingAttach + CopyClientMemoryForType).

#define AVB_SHM_MAGIC           0x33425641u // "AVB3" in little-endian byte order
#define AVB_SHM_HEADER_SIZE     16384u      // one arm64 page; AvbShmHeader at offset 0
#define AVB_RX_SLOT_SIZE        2048u
#define AVB_RX_SLOT_HDR         64u
#define AVB_RX_SLOT_DATA_MAX    1984u       // >= 1522 (VLAN-tagged max frame)
#define AVB_RX_CTL_SLOTS        512u        // power of two
#define AVB_RX_AUDIO_SLOTS      8192u       // power of two
#define AVB_RX_CTL_OFFSET       AVB_SHM_HEADER_SIZE
#define AVB_RX_AUDIO_OFFSET     (AVB_RX_CTL_OFFSET + AVB_RX_CTL_SLOTS * AVB_RX_SLOT_SIZE)
#define AVB_RX_MEM_SIZE         (AVB_RX_AUDIO_OFFSET + AVB_RX_AUDIO_SLOTS * AVB_RX_SLOT_SIZE) // 17 842 176
#define AVB_CONSUMER_MEM_SIZE   16384u
#define AVB_MAX_STREAMS         16u         // S_max
#define AVB_STEP_CLOCK_MAX_NS   1000000000ll
#define AVB_TX_RATE_PER_SEC     1000u       // dext token bucket (SAF-6)
#define AVB_TX_BURST            64u

// AvbRxSlot.cls
enum AvbRxClass : uint8_t {
    kAvbRxNone = 0, kAvbRxGptp = 1, kAvbRxMsrp = 2, kAvbRxMvrp = 3,
    kAvbRxAvdecc = 4, kAvbRxMaap = 5, kAvbRxStream = 6,
};
#define AVB_RX_CLASS_COUNT 7u   // kAvbRxNone..kAvbRxStream

// AvbRxSlot.flags
#define AVB_RXF_TS_HW      (1u << 0)  // hw_ts_ns valid (SYSTIM ns, in-buffer timestamp)
#define AVB_RXF_VLAN_FRAME (1u << 1)  // set iff data[12..14] == 0x8100 (tag present in data[])
#define AVB_RXF_VLAN_DESC  (1u << 2)  // set iff descriptor VP set AND no in-buffer 0x8100 at offset 12;
                                      // vlan_tci then holds the tag. Never set together with VLAN_FRAME
#define AVB_RXF_TS_STEP_AMBIG (1u << 3) // harvested inside a SYSTIM step's settling window:
                                      // hw_ts_ns may belong to either timebase; never use it for timing
#define AVB_STEP_SETTLE_NS 1000000u   // settling window after a SYSTIM write

struct AvbRxSlot {              // 2048 B
    uint64_t hw_ts_ns;          //  0 SYSTIM ns of reception (if AVB_RXF_TS_HW)
    uint64_t harvest_mach;      //  8 mach_absolute_time() at harvest
    uint64_t index;             // 16 ring record index (== producer head at write)
    uint32_t status_error;      // 24 advanced RX write-back status_error, verbatim
    uint16_t len;               // 28 bytes in data[] (no FCS, no TS header)
    uint16_t vlan_tci;          // 30 descriptor VLAN field (valid if AVB_RXF_VLAN_DESC)
    uint16_t flags;             // 32 AVB_RXF_*
    uint8_t  cls;               // 34 AvbRxClass
    uint8_t  pad0;              // 35
    uint32_t desc_len;          // 36 raw PKT_LEN (includes 16 B TS header if TSIP)
    uint32_t clock_gen;         // 40 dext clock generation (ivars->clockGen) at harvest
    uint8_t  pad1[20];          // 44
    uint8_t  data[AVB_RX_SLOT_DATA_MAX]; // 64
};

// Counter indices into AvbCounters.c[]. All monotonic for the life of the
// datapath (reset only by InitDatapath), except those marked GAUGE.
enum AvbCounterIndex : uint32_t {
    kAvbCtrRxFrames = 0,            // EOP descriptors harvested (counted after the EOP check)
    kAvbCtrRxClassBase = 1,         // delivered per AvbRxClass: index = cls (== kAvbCtrRxClassBase + cls - 1),
                                    // so Gptp -> 1 ... Stream -> 6; index 7 reserved; cls 0 (None) is never delivered
    kAvbCtrRxDropOther = 8,         // not in the DP-4 set (IP, mDNS, broadcast, ...)
    kAvbCtrRxDropUnboundStream = 9, // AVTP stream subtype to a MAC/VID not in the filter
    kAvbCtrRxDropUntaggedStream = 10,
    kAvbCtrRxErrors = 11,           // RXE set: discarded
    kAvbCtrRxNonEop = 12,           // multi-buffer frame: discarded
    kAvbCtrRingOverrunCtl = 13,
    kAvbCtrRingOverrunAudio = 14,
    kAvbCtrConsumerIndexInvalid = 15,
    kAvbCtrHwMpc = 16,              // accumulated MPC  (missed packets)
    kAvbCtrHwRnbc = 17,             // accumulated RNBC (receive no buffers)
    kAvbCtrHwRqdpc0 = 18,           // accumulated RQDPC(0) (queue drops)
    kAvbCtrHwCrcErrs = 19,          // accumulated CRCERRS
    kAvbCtrTxFrames = 20,
    kAvbCtrTxRateLimited = 21,
    kAvbCtrTxTsRequested = 22,
    kAvbCtrTxTsOk = 23,
    kAvbCtrTxTsTimeout = 24,
    kAvbCtrTxTsBusyRejected = 25,
    kAvbCtrTxStaleLatch = 26,
    kAvbCtrLinkUpEvents = 27,
    kAvbCtrLinkDownEvents = 28,
    kAvbCtrHarvestTicks = 29,
    kAvbCtrHarvestMaxBatch = 30,    // GAUGE (max descriptors in one tick)
    kAvbCtrHarvestLateTicks = 31,   // tick-to-tick gap > 2 ms
    kAvbCtrLegacySwOverrun = 32,    // Phase 0 software ring (PollRx mode)
    kAvbCtrRxOversize = 33,         // payload after TS header > AVB_RX_SLOT_DATA_MAX: discarded (expected 0; confirms AS-6)
    kAvbCtrCount = 64,
};
struct AvbCounters { uint64_t c[kAvbCtrCount]; };   // 512 B

struct AvbShmHeader {           // 1024 B at offset 0 of kAvbMemRx
    uint32_t magic;             //   0 AVB_SHM_MAGIC
    uint32_t abi_version;       //   4 AVB_ABI_VERSION
    uint32_t header_size;       //   8 AVB_SHM_HEADER_SIZE
    uint32_t slot_size;         //  12 AVB_RX_SLOT_SIZE
    uint32_t ctl_slots;         //  16
    uint32_t audio_slots;       //  20
    uint32_t ctl_offset;        //  24
    uint32_t audio_offset;      //  28
    uint32_t total_size;        //  32 AVB_RX_MEM_SIZE
    uint32_t generation;        //  36 RingAttach count since dext Start
    uint32_t stage;             //  40 live AvbStage
    uint32_t fault;             //  44 live AvbFault
    uint32_t flags;             //  48 live AVB_FLAG_*
    uint32_t producer_running;  //  52 1 while the harvester writes slots
    uint64_t last_tick_mach;    //  56 mach of the last harvest tick
    uint8_t  mac[6];            //  64
    uint8_t  pad0[58];          //  70
    uint64_t ctl_head;          // 128 records written (monotonic), release-stored
    uint8_t  pad1[120];         // 136
    uint64_t audio_head;        // 256
    uint8_t  pad2[120];         // 264
    struct AvbCounters counters;// 384
    uint32_t clock_gen;         // 896 incremented by every SYSTIM write (SetClock/StepClock); release-stored
    uint8_t  reserved[124];     // 900
};

struct AvbShmConsumer {         // 256 B at offset 0 of kAvbMemConsumer
    uint64_t ctl_tail;          //   0 records consumed (monotonic), release-stored by the daemon
    uint8_t  pad0[120];
    uint64_t audio_tail;        // 128
    uint8_t  pad1[120];
};

struct AvbStreamFilterEntry {   // 8 B
    uint8_t  dest_mac[6];       // must have the multicast bit set
    uint16_t vid;               // 1..4094; must match the frame's tag
};
struct AvbStreamFilter {        // 136 B
    uint32_t count;             // 0..AVB_MAX_STREAMS
    uint32_t pad;
    struct AvbStreamFilterEntry e[AVB_MAX_STREAMS];
};

#ifdef __cplusplus
static_assert(sizeof(AvbRxSlot) == AVB_RX_SLOT_SIZE, "AvbRxSlot ABI size");
static_assert(offsetof(AvbRxSlot, data) == AVB_RX_SLOT_HDR, "AvbRxSlot data offset");
static_assert(offsetof(AvbRxSlot, clock_gen) == 40 && offsetof(AvbRxSlot, pad1) == 44, "AvbRxSlot clock_gen");
static_assert(sizeof(AvbCounters) == 512, "AvbCounters ABI size");
static_assert(sizeof(AvbShmHeader) == 1024, "AvbShmHeader ABI size");
static_assert(offsetof(AvbShmHeader, ctl_head) == 128 && offsetof(AvbShmHeader, audio_head) == 256 &&
              offsetof(AvbShmHeader, counters) == 384, "AvbShmHeader offsets");
static_assert(offsetof(AvbShmHeader, clock_gen) == 896 && offsetof(AvbShmHeader, reserved) == 900,
              "AvbShmHeader clock_gen");
static_assert(sizeof(AvbShmConsumer) == 256 && offsetof(AvbShmConsumer, audio_tail) == 128, "consumer");
static_assert(sizeof(AvbStreamFilter) == 136, "AvbStreamFilter ABI size");
static_assert(sizeof(AvbStreamFilter) <= 4096 && sizeof(AvbCounters) <= 4096, "inline");
static_assert(AVB_RX_MEM_SIZE == 17842176u, "rx mem size");
static_assert((AVB_RX_CTL_SLOTS & (AVB_RX_CTL_SLOTS - 1)) == 0 &&
              (AVB_RX_AUDIO_SLOTS & (AVB_RX_AUDIO_SLOTS - 1)) == 0, "pow2");
static_assert(kAvbCtrRxOversize < kAvbCtrCount, "counter space");
static_assert(kAvbCtrRxClassBase + AVB_RX_CLASS_COUNT == kAvbCtrRxDropOther, "class counters 1..7");
// (uint32_t cast: C++20 deprecates arithmetic between two enumeration types.)
static_assert(uint32_t(kAvbCtrRxClassBase) + kAvbRxGptp - 1 == 1 &&
              uint32_t(kAvbCtrRxClassBase) + kAvbRxStream - 1 == 6,
              "class counter index = cls");
#endif

#ifdef __cplusplus
static_assert(sizeof(AvbInfo) == 60, "AvbInfo ABI size");
static_assert(sizeof(AvbClockSample) == 24, "AvbClockSample ABI size");
static_assert(sizeof(AvbRxEntry) == 152, "AvbRxEntry ABI size");
static_assert(sizeof(AvbRxBatch) == 3656, "AvbRxBatch ABI size");
static_assert(sizeof(AvbRxBatch) <= 4096, "AvbRxBatch must travel inline");
static_assert(AVB_READ_CLOCK_MAX_SAMPLES * sizeof(AvbClockSample) <= 4096,
              "ReadClock output must travel inline");
#endif

#endif // AVB_SHARED_H
