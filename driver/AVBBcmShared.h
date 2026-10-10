// AVBBcmShared.h - user-client ABI shared by the AVBBcmDriver dext and the
// avb-probe Rust CLI (mirrored by hand in crates/avb-probe/src/abi.rs; both
// sides assert the struct sizes below).
//
// Rules:
//   * fixed-width types, natural alignment, no implicit padding;
//   * every structure output stays <= 4096 bytes so it travels inline;
//   * bump AVB_ABI_VERSION on any layout or selector change.
//
// There is deliberately no generic register-write selector. Every write the
// dext performs goes through a named, sequence-checked operation.

#ifndef AVB_BCM_SHARED_H
#define AVB_BCM_SHARED_H

#include <stdint.h>

#define AVB_ABI_VERSION 1u

// External method selectors.
enum AvbSelector : uint32_t {
    kAvbSelGetInfo          = 0,  // in: -              out: struct AvbInfo
    kAvbSelReadClock        = 1,  // in: n (1..64)      out: struct n * AvbClockSample
    kAvbSelSetClock         = 2,  // in: ns             out: readback ns
    kAvbSelAdjFreq          = 3,  // in: ppb (i64)      out: raw CORRECT_CTL value
    kAvbSelInitDatapath     = 4,  // in: flags          out: stage
    kAvbSelPollRx           = 5,  // in: ethertype,max  out: struct AvbRxBatch
    kAvbSelSendFrame        = 6,  // in: struct frame + want_ts   out: tx_seq, mach_at_doorbell
    kAvbSelGetTxTimestamp   = 7,  // in: tx_seq         out: status, hw_ts, mach_latched
    kAvbSelReadReg32        = 8,  // in: BAR0 offset    out: value
    kAvbSelShutdownDatapath = 9,  // in: -              out: -
    kAvbSelCount            = 10,
    // 16..31 reserved for Phase 1 shared rings (CopyClientMemoryForType).
};

// Limits.
#define AVB_READ_CLOCK_MAX_SAMPLES 64u
#define AVB_ADJ_FREQ_MAX_PPB       1000000ll
#define AVB_RX_BATCH_MAX           24u
#define AVB_RX_COPY_BYTES          128u
#define AVB_FRAME_MIN_LEN          60u
#define AVB_FRAME_MAX_LEN          1514u
#define AVB_REG_READ_LIMIT         0x10000u

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
#define AVB_FLAG_APE     (1u << 0)
#define AVB_FLAG_ASF     (1u << 1)
#define AVB_FLAG_LINK_UP (1u << 2)
#define AVB_FLAG_1000FD  (1u << 3)
#define AVB_FLAG_SIG_OK  (1u << 4)  // NIC SRAM data signature present

// AvbInfo.fault (last fault code; once set, the dext refuses all MMIO until Stop).
enum AvbFault : uint32_t {
    kAvbFaultNone          = 0,
    kAvbFaultAllOnes       = 1,  // register read returned 0xFFFFFFFF (device gone/hung)
    kAvbFaultIdMismatch    = 2,  // config space IDs are not 14e4:1682 / 106b:00f6
    kAvbFaultFirmwarePoll  = 3,  // bootcode did not post ~MAGIC after core reset
    kAvbFaultBufmgr        = 4,  // BUFMGR enable poll timed out
    kAvbFaultHostcc        = 5,  // HOSTCC disable poll timed out
    kAvbFaultStopBlock     = 6,  // a block enable bit would not clear on shutdown
    kAvbFaultDma           = 7,  // DMA buffer / IODMACommand setup failed
    kAvbFaultMdio          = 8,  // MAC_MI_COM busy poll timed out
    kAvbFaultNoSignature   = 9,  // NIC SRAM DATA_SIG missing after reset
    kAvbFaultTxStall       = 10, // TX consumer did not advance
};

// kAvbSelGetTxTimestamp status
enum AvbTxTsStatus : uint32_t {
    kAvbTxTsPending      = 0,
    kAvbTxTsOk           = 1,
    kAvbTxTsTimeout      = 2,
    kAvbTxTsNotRequested = 3,
};

struct AvbInfo {
    uint32_t abi_version;
    uint32_t vendor_device;   // config 0x00, e.g. 0x168214e4
    uint32_t subsys;          // config 0x2c, e.g. 0x00f6106b
    uint32_t asic_rev;        // config 0xfc (GEN15 product id / asic rev)
    uint32_t misc_host_ctrl;
    uint32_t grc_mode;
    uint32_t stage;           // AvbStage
    uint32_t fault;           // AvbFault
    uint32_t flags;           // AVB_FLAG_*
    uint8_t  mac[6];
    uint8_t  pad[2];
    uint32_t tb_numer;        // dext-side mach_timebase_info
    uint32_t tb_denom;
};

struct AvbClockSample {
    uint64_t mach_before;     // mach_absolute_time() before the LSB read
    uint64_t eav_ns;          // (MSB << 32) | LSB
    uint64_t mach_after;      // mach_absolute_time() after the MSB read
};

struct AvbRxEntry {
    uint64_t harvest_mach;    // mach_absolute_time() when the dext harvested the BD
    uint64_t hw_ts;           // RX_TSTAMP (masked) if has_ts
    uint32_t len;             // frame length without FCS
    uint16_t type_flags;      // raw RX BD flags (low 16 bits of type_flags)
    uint8_t  has_ts;
    uint8_t  pad;
    uint8_t  data[AVB_RX_COPY_BYTES];
};

struct AvbRxBatch {
    uint32_t count;
    uint32_t dropped;         // software-ring overruns since last poll
    struct AvbRxEntry e[AVB_RX_BATCH_MAX];
};

#ifdef __cplusplus
static_assert(sizeof(AvbInfo) == 52, "AvbInfo ABI size");
static_assert(sizeof(AvbClockSample) == 24, "AvbClockSample ABI size");
static_assert(sizeof(AvbRxEntry) == 152, "AvbRxEntry ABI size");
static_assert(sizeof(AvbRxBatch) == 3656, "AvbRxBatch ABI size");
static_assert(sizeof(AvbRxBatch) <= 4096, "AvbRxBatch must travel inline");
static_assert(AVB_READ_CLOCK_MAX_SAMPLES * sizeof(AvbClockSample) <= 4096,
              "ReadClock output must travel inline");
#endif

#endif // AVB_BCM_SHARED_H
