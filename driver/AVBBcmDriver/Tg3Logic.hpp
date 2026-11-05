// SPDX-License-Identifier: MIT OR Apache-2.0

// Tg3Logic.hpp - pure, header-only helpers for the BCM57762 dext.
//
// No DriverKit includes: this file is compiled both into the dext and into the
// host-side unit tests (driver/tests). Everything here is deterministic integer
// math or bit packing, so it can be tested without hardware.

#ifndef TG3_LOGIC_HPP
#define TG3_LOGIC_HPP

#include <stdint.h>

#include "Bcm57762Regs.h"

namespace tg3logic {

// ---------------------------------------------------------------------------
// EAV reference-clock frequency correction.
//
// The correction register drives a 24-bit accumulator: each reference-clock
// tick adds the magnitude, and every overflow steps the counter by one extra
// (or one fewer, with NEG). The fractional rate change is therefore
// magnitude / 2^24, so magnitude = round(|ppb| * 2^24 / 1e9).

constexpr uint64_t kPpbScale = 1000000000ull;

constexpr uint32_t eavCorrectionMagnitude(int64_t ppb) {
    const uint64_t absPpb = ppb < 0 ? static_cast<uint64_t>(-ppb) : static_cast<uint64_t>(ppb);
    return static_cast<uint32_t>(((absPpb << 24) + kPpbScale / 2) / kPpbScale);
}

// Register value for EAV_REF_CLK_CORRECT_CTL. Caller must range-check ppb
// (|ppb| <= AVB_ADJ_FREQ_MAX_PPB) first; the magnitude is masked to 24 bits.
constexpr uint32_t eavCorrectionFromPpb(int64_t ppb) {
    const uint32_t mag = eavCorrectionMagnitude(ppb) & bcm::kEavCorrectMagMask;
    if (mag == 0) {
        return 0;
    }
    return bcm::kEavCorrectEnable | (ppb < 0 ? bcm::kEavCorrectNegative : 0u) | mag;
}

// Inverse: the ppb a register value actually applies (rounded to nearest).
constexpr int64_t eavPpbFromCorrection(uint32_t reg) {
    if ((reg & bcm::kEavCorrectEnable) == 0) {
        return 0;
    }
    const uint64_t mag = reg & bcm::kEavCorrectMagMask;
    const int64_t ppb = static_cast<int64_t>((mag * kPpbScale + (1ull << 23)) >> 24);
    return (reg & bcm::kEavCorrectNegative) ? -ppb : ppb;
}

constexpr uint64_t join64(uint32_t msb, uint32_t lsb) {
    return (static_cast<uint64_t>(msb) << 32) | lsb;
}

// ---------------------------------------------------------------------------
// Ring index helpers (all ring sizes are powers of two).

constexpr bool isPow2(uint32_t n) { return n != 0 && (n & (n - 1)) == 0; }

constexpr uint32_t ringNext(uint32_t idx, uint32_t size) { return (idx + 1) & (size - 1); }

constexpr uint32_t ringAdd(uint32_t idx, uint32_t n, uint32_t size) { return (idx + n) & (size - 1); }

// Number of entries from `from` (inclusive) up to `to` (exclusive).
constexpr uint32_t ringDistance(uint32_t from, uint32_t to, uint32_t size) {
    return (to - from) & (size - 1);
}

// ---------------------------------------------------------------------------
// Descriptors (host memory, little-endian 32-bit words).

struct TxBd {
    uint32_t addrHi;
    uint32_t addrLo;
    uint32_t lenFlags;  // len << 16 | flags
    uint32_t vlanTag;   // mss << 16 | vlan (both zero here)
};
static_assert(sizeof(TxBd) == 16, "TX BD is 16 bytes");

struct RxBd {
    uint32_t addrHi;
    uint32_t addrLo;
    uint32_t idxLen;     // idx << 16 | len
    uint32_t typeFlags;  // type << 16 | flags
    uint32_t ipTcpCsum;
    uint32_t errVlan;
    uint32_t reserved;
    uint32_t opaque;
};
static_assert(sizeof(RxBd) == 32, "RX BD is 32 bytes");

constexpr uint32_t txLenFlags(uint32_t len, bool wantTimestamp) {
    return ((len & 0xffffu) << bcm::kTxdLenShift) | bcm::kTxdFlagEnd |
           (wantTimestamp ? bcm::kTxdFlagHwTstamp : 0u);
}

constexpr TxBd makeTxBd(uint64_t iova, uint32_t len, bool wantTimestamp) {
    return TxBd{static_cast<uint32_t>(iova >> 32), static_cast<uint32_t>(iova),
                txLenFlags(len, wantTimestamp), 0};
}

// Producer-ring (standard RX) descriptor the host posts for buffer `index`.
constexpr RxBd makeStdRxBd(uint64_t iova, uint32_t index, uint32_t bufLen) {
    return RxBd{static_cast<uint32_t>(iova >> 32),
                static_cast<uint32_t>(iova),
                bufLen & 0xffffu,
                bcm::kRxdFlagEnd,
                0,
                0,
                0,
                bcm::kRxdOpaqueRingStd | (index & bcm::kRxdOpaqueIndexMask)};
}

enum class PtpStat : uint8_t { None = 0, V1 = 1, V2 = 2, Unknown = 3 };

constexpr PtpStat ptpStat(uint32_t typeFlags) {
    switch (typeFlags & bcm::kRxdPtpStatMask) {
        case 0: return PtpStat::None;
        case bcm::kRxdPtpStatV1: return PtpStat::V1;
        case bcm::kRxdPtpStatV2: return PtpStat::V2;
        default: return PtpStat::Unknown;
    }
}

struct RxReturn {
    uint32_t len;        // frame length without FCS (0 if shorter than FCS)
    uint32_t stdIndex;   // which std-ring buffer the frame landed in
    bool fromStdRing;
    bool error;
    bool hasTimestamp;   // PTPSTAT says the RX latch holds this frame's time
    PtpStat ptp;
    uint16_t flags;      // raw low 16 bits of type_flags
};

constexpr uint32_t kFcsLen = 4;

constexpr RxReturn decodeRxReturn(const RxBd& bd) {
    const uint32_t raw = bd.idxLen & 0xffffu;
    const PtpStat p = ptpStat(bd.typeFlags);
    return RxReturn{raw > kFcsLen ? raw - kFcsLen : 0,
                    bd.opaque & bcm::kRxdOpaqueIndexMask,
                    (bd.opaque & 0x00070000u) == bcm::kRxdOpaqueRingStd,
                    (bd.errVlan & bcm::kRxdErrMask) != 0,
                    p == PtpStat::V1 || p == PtpStat::V2,
                    p,
                    static_cast<uint16_t>(bd.typeFlags & 0xffffu)};
}

// ---------------------------------------------------------------------------
// MDIO frames for MAC_MI_COM.

constexpr uint32_t miReadFrame(uint32_t phy, uint32_t reg) {
    return ((phy << bcm::kMiComPhyShift) & bcm::kMiComPhyMask) |
           ((reg << bcm::kMiComRegShift) & bcm::kMiComRegMask) | bcm::kMiComCmdRead |
           bcm::kMiComStartBusy;
}

constexpr uint32_t miWriteFrame(uint32_t phy, uint32_t reg, uint16_t val) {
    return ((phy << bcm::kMiComPhyShift) & bcm::kMiComPhyMask) |
           ((reg << bcm::kMiComRegShift) & bcm::kMiComRegMask) | (val & bcm::kMiComDataMask) |
           bcm::kMiComCmdWrite | bcm::kMiComStartBusy;
}

// ---------------------------------------------------------------------------
// Link speed from the PHY auxiliary status register.

enum class LinkSpeed : uint8_t { Unknown = 0, M100Half, M100Full, G1Half, G1Full };

constexpr LinkSpeed linkFromAuxStat(uint16_t aux) {
    switch (aux & bcm::kAuxStatSpeedMask) {
        case bcm::kAuxStat100Half: return LinkSpeed::M100Half;
        case bcm::kAuxStat100Full: return LinkSpeed::M100Full;
        case bcm::kAuxStat1000Half: return LinkSpeed::G1Half;
        case bcm::kAuxStat1000Full: return LinkSpeed::G1Full;
        default: return LinkSpeed::Unknown;
    }
}

// MAC_MODE with the port-mode/duplex bits set for `speed` (other bits kept).
constexpr uint32_t macModeForLink(uint32_t macMode, LinkSpeed speed) {
    uint32_t m = macMode & ~(bcm::kMacModePortMask | bcm::kMacModeHalfDuplex);
    if (speed == LinkSpeed::M100Half || speed == LinkSpeed::M100Full) {
        m |= bcm::kMacModePortMii;
    } else {
        m |= bcm::kMacModePortGmii;
    }
    if (speed == LinkSpeed::M100Half || speed == LinkSpeed::G1Half) {
        m |= bcm::kMacModeHalfDuplex;
    }
    return m;
}

// ---------------------------------------------------------------------------
// Identity / register-access guards.

constexpr bool isOurDevice(uint32_t vendorDevice, uint32_t subsystem) {
    return vendorDevice == bcm::kVendorDevice && subsystem == bcm::kSubsystem;
}

constexpr bool isAsic57766(uint32_t asicRevReg) { return (asicRevReg >> 12) == bcm::kAsicRev57766; }

// BCM BAR0 register window. Deliberately local: AVB_REG_READ_LIMIT in
// AVBShared.h is the maximum over all NICs (0x20000 for the I210).
constexpr uint32_t kBcmRegReadLimit = 0x10000u;

constexpr bool isReadableRegOffset(uint64_t off) {
    return off < kBcmRegReadLimit && (off & 3u) == 0;
}

// ---------------------------------------------------------------------------
// Apple-state snapshot: register windows READ (never written) at Start,
// before Stage A's first write, to observe how Apple's BCM5701Enet left the
// chip. Apple's driver gets working RX PTP timestamps on this part; the
// tg3-documented RX path does not latch for us, so the configuration Apple
// leaves behind is the evidence. Windows are standard register blocks (MAC
// 0x400-0x6ff incl. TX/RX timestamp and RX_PTP_CTL, receive list placement,
// receive BD initiator, CPMU, read/write DMA, GRC + EAV); the MAC statistics
// block at 0x800 is excluded because its counters may clear on read.
struct SnapWindow {
    uint32_t start;
    uint32_t end;  // exclusive
};
// 0x5000-0x5700 added 2026-11-04: BCM5701Enet::startAVB programs an RX-rule /
// classifier block at 0x5410-0x561c (incl. ethertype 0x88F7 at 0x5494) that
// the tg3 path never touches and that is how Apple routes gPTP frames for RX
// timestamping. The earlier windows missed it.
constexpr SnapWindow kSnapWindows[] = {
    {0x0400u, 0x0700u}, {0x0c00u, 0x0e00u}, {0x2000u, 0x2100u}, {0x2400u, 0x2500u},
    {0x3600u, 0x3700u}, {0x3c00u, 0x3d00u}, {0x4800u, 0x4900u}, {0x4c00u, 0x4d00u},
    {0x5000u, 0x5700u}, {0x6800u, 0x6a00u},
};
constexpr uint32_t snapWindowWords(const SnapWindow& w) { return (w.end - w.start) / 4u; }
constexpr uint32_t kSnapWords = [] {
    uint32_t n = 0;
    for (const auto& w : kSnapWindows) {
        n += snapWindowWords(w);
    }
    return n;
}();

// Index of `off` in the snapshot array, or -1 if it is not a captured,
// aligned offset.
constexpr int32_t snapIndex(uint32_t off) {
    if ((off & 3u) != 0) {
        return -1;
    }
    uint32_t base = 0;
    for (const auto& w : kSnapWindows) {
        if (off >= w.start && off < w.end) {
            return static_cast<int32_t>(base + (off - w.start) / 4u);
        }
        base += snapWindowWords(w);
    }
    return -1;
}

// kAvbSelReadReg32 with this bit set in the offset argument reads the Start
// snapshot instead of the live register.
constexpr uint64_t kReadSnapshotFlag = 1ull << 32;

// ---------------------------------------------------------------------------
// Apple's AVB RX classifier / rule block (0x5400-0x561c), the exact ordered
// writes BCM5701Enet::updateAVBStreamFilters() + startAVB() make on this chip
// to route gPTP (ethertype 0x88F7) frames so their RX timestamps latch. The
// tg3-documented RX_PTP_CTL path (0x6c8) does NOT do this on 57766. Learned by
// read-only disassembly of AppleBCM5701Ethernet (register offsets and values
// only, never code); see docs/bcm-rx-timestamp-investigation.md.
//
// Replayed in order, duplicates preserved: 0x5494 and 0x5498 are written twice
// (they are indexed element/mask ports, so the second write is a second rule
// element, not a correction). The per-stream MAC/VLAN block (0x5480-0x548c)
// and the station-address match (setPerfectMatch) are intentionally omitted:
// they are needed only for unicast audio streams, not for gPTP multicast RX
// timestamps, and they require live stream state.
struct RegWrite {
    uint32_t off;
    uint32_t val;
};
constexpr RegWrite kAvbRxRuleWrites[] = {
    // updateAVBStreamFilters fixed rule-bank header (elements 0-3).
    {0x5400, 0x8400e000}, {0x5404, 0x8400e000}, {0x5408, 0x8400000c}, {0x540c, 0x8400000c},
    // startAVB classifier: ethertype 0x88F7 element + mask pairs.
    {0x5410, 0x8400000c},
    {0x5494, 0x88f7ffff},  // ethertype 0x88F7, low mask 0xffff
    {0x5414, 0x84006002},
    {0x5494, 0x013fffff},
    {0x5418, 0x84006002},
    {0x5498, 0x0140ffff},
    // Queue / timestamp config.
    {0x5500, 0x00000005}, {0x5504, 0x0000000a}, {0x5508, 0x00000010}, {0x550c, 0x00000020},
    {0x5510, 0x00000040}, {0x5608, 0x00000000}, {0x560c, 0x000000e0}, {0x5610, 0x00000000},
    {0x5614, 0x000000e1}, {0x5618, 0x00000025}, {0x561c, 0x00e4e3a2}, {0x0560, 0x0000000e},
};
constexpr uint32_t kAvbRxRuleWriteCount = sizeof(kAvbRxRuleWrites) / sizeof(kAvbRxRuleWrites[0]);

// Little-endian u16 from a byte buffer (status block fields).
constexpr uint16_t le16At(const uint8_t* p, uint32_t off) {
    return static_cast<uint16_t>(p[off] | (static_cast<uint16_t>(p[off + 1]) << 8));
}

// Ethertype of a frame, looking through one 802.1Q tag. 0 if too short.
constexpr uint16_t frameEthertype(const uint8_t* p, uint32_t len) {
    if (len < 14) {
        return 0;
    }
    const uint16_t et = static_cast<uint16_t>((p[12] << 8) | p[13]);
    if (et == 0x8100) {
        return len < 18 ? 0 : static_cast<uint16_t>((p[16] << 8) | p[17]);
    }
    return et;
}

// ---------------------------------------------------------------------------
// Composite register values for the Stage B bring-up (tg3 behaviour for an
// ASIC_REV_57766 / 57765_PLUS / PCIe / HW_TSO_3 part at MTU 1500). Kept here
// so the host tests pin them.

// RCVDBDI_STD_BD maxlen_flags on 57765_PLUS: ring size << 16 | dma size << 2.
constexpr uint32_t stdRingMaxLenFlags() {
    return (bcm::kStdRingSize << bcm::kBdInfoMaxLenShift) | (bcm::kRxStdDmaSize << 2);
}

constexpr uint32_t sendRcbMaxLenFlags() { return bcm::kTxRingSize << bcm::kBdInfoMaxLenShift; }

constexpr uint32_t rxRetRcbMaxLenFlags() { return bcm::kRxRetRingSize << bcm::kBdInfoMaxLenShift; }

constexpr uint32_t rdmacModeValue() {
    return bcm::kRdmacEnable | bcm::kRdmacErrEnables | bcm::kRdmacFifoLongBurst |
           bcm::kRdmacJmb2kMmrr | bcm::kRdmacIpv4LsoEn | bcm::kRdmacIpv6LsoEn;
}

constexpr uint32_t wdmacModeValue() {
    return bcm::kWdmacEnable | bcm::kWdmacErrEnables | bcm::kWdmacStatusTagFix;
}

// GRC_MODE for the datapath. tg3 clears NO_RX_PHDR_CSUM and sets
// NO_TX_PHDR_CSUM; TIME_SYNC_ENABLE is ORed in as tg3 does for PTP parts.
constexpr uint32_t grcModeDatapath() {
    return bcm::kGrcSwapLittleEndian | bcm::kGrcHostSendBds | bcm::kGrcNoTxPhdrCsum |
           bcm::kGrcIrqOnMacAttn | bcm::kGrcHostStackup | bcm::kGrcTimeSyncEnable;
}

constexpr uint32_t macModeDatapath() {
    return bcm::kMacModeTxStatEn | bcm::kMacModeRxStatEn | bcm::kMacModeTdeEn |
           bcm::kMacModeRdeEn | bcm::kMacModeFhdeEn;
}

// Jumbo-ring replenish threshold: min(jmb bdcache / 2, max(pending / 8, 1)).
constexpr uint32_t jumboReplenishThreshold(uint32_t pending) {
    const uint32_t host = pending / 8 > 1 ? pending / 8 : 1;
    return bcm::kJmbBdCacheSize / 2 < host ? bcm::kJmbBdCacheSize / 2 : host;
}

// Standard-ring replenish threshold, following tg3 behaviour:
// min(min(bdcache/2, ring size), max(posted/8, 1)).
constexpr uint32_t stdReplenishThreshold(uint32_t posted) {
    const uint32_t nic = bcm::kStdBdCacheSize / 2 < bcm::kStdRingSize ? bcm::kStdBdCacheSize / 2
                                                                       : bcm::kStdRingSize;
    const uint32_t host = posted / 8 > 1 ? posted / 8 : 1;
    return nic < host ? nic : host;
}

// --- RX-CPU stamp ring (driver/bcm-fw) -------------------------------------
//
// The stamping firmware records one entry per gPTP frame it services: the EAV
// reference-clock low 32 bits at service time, plus the frame's PTP
// messageType and sequenceId so the host can match a harvested frame to its
// timestamp. Ring layout (scratch 0x0800C000): slot = { word0 = eavLo,
// word1 = (messageType << 16) | sequenceId }, 256 slots, producer count at
// 0x0800BFF0. These helpers are pure (host-tested); the window I/O is in the
// dext.

struct RxCpuStamp {
    uint32_t eavLo;
    uint8_t  msgType;
    uint16_t seq;
};

// Decode one ring slot's two words.
constexpr RxCpuStamp decodeRxCpuStamp(uint32_t word0, uint32_t word1) {
    return RxCpuStamp{word0, static_cast<uint8_t>((word1 >> 16) & 0x0fu),
                      static_cast<uint16_t>(word1 & 0xffffu)};
}

// Parse the gPTP match key from a raw Ethernet frame: dst(6) src(6)
// ethertype(2), then the PTP common header. messageType is the low nibble of
// PTP byte 0 (frame[14]); sequenceId is the big-endian u16 at PTP offset 30
// (frame[44..46]). Returns false unless it is an 0x88F7 frame long enough.
inline bool parseGptpStampKey(const uint8_t * frame, uint32_t len, uint8_t * msgTypeOut,
                              uint16_t * seqOut) {
    if (frame == nullptr || len < 46) {
        return false;
    }
    if (frame[12] != 0x88 || frame[13] != 0xf7) {
        return false;
    }
    *msgTypeOut = static_cast<uint8_t>(frame[14] & 0x0fu);
    *seqOut = static_cast<uint16_t>((static_cast<uint16_t>(frame[44]) << 8) | frame[45]);
    return true;
}

// Combine a 32-bit EAV frame timestamp with the current software high word and
// a fresh low-word sample into a 64-bit value. EAV_REF_CLCK_MSB reads 0 on the
// 57766-class part, so the high word is tracked in software (advanced on wrap,
// re-aligned to the TX latch epoch). When the frame's low word is at or after
// the current low word, the frame was captured before the most recent wrap, so
// borrow one from the high word.
constexpr uint64_t reconstructEavTs(uint32_t hi, uint32_t curLo, uint32_t frameLsb) {
    if (curLo <= frameLsb) {
        hi--;
    }
    return (static_cast<uint64_t>(hi) << 32) | static_cast<uint64_t>(frameLsb);
}

// Only PTP event messages (messageType 0-7: Sync, Delay_Req, Pdelay_Req,
// Pdelay_Resp) carry a meaningful RX timestamp (IEEE 1588 / 802.1AS); general
// messages (Follow_Up, Pdelay_Resp_Follow_Up, Announce, ...) need none.
constexpr bool isPtpEventMsg(uint8_t msgType) { return msgType < 8; }

// Host mirror of the newest stamp-ring entries. Every scratch word costs a
// config-space window access (~145 us measured on the Thunderbolt adapter),
// on the same driver queue that serves SendFrames, so the dext reads each
// ring entry once, when it first appears, instead of re-reading a fixed window
// for every harvested gPTP frame.
constexpr uint32_t kRxCpuStampCacheSize = 16;
struct RxCpuStampCache {
    RxCpuStamp e[kRxCpuStampCacheSize];  // newest first
    uint32_t count;                      // valid entries in e
    uint32_t seen;                       // producer count already read
};

// First producer index to read when the firmware's count is `prod` and the
// cache has read up to `seen`: never older than the last `window` entries,
// and from the start of the window again if the count went backwards (the
// firmware was reloaded and restarted it).
constexpr uint32_t stampReadStart(uint32_t seen, uint32_t prod, uint32_t window) {
    const uint32_t lo = prod > window ? prod - window : 0u;
    if (prod < seen) {
        return lo;
    }
    return seen > lo ? seen : lo;
}

// Insert the newest entry at the front, dropping the oldest when full.
inline void stampCachePush(RxCpuStampCache & c, const RxCpuStamp & s) {
    const uint32_t keep = c.count < kRxCpuStampCacheSize ? c.count : kRxCpuStampCacheSize - 1u;
    for (uint32_t i = keep; i > 0; i--) {
        c.e[i] = c.e[i - 1u];
    }
    c.e[0] = s;
    c.count = keep + 1u;
}

// Scan a newest-first snapshot of ring entries for the first (msgType, seq)
// match and return its EAV low word. The dext builds `entries` by reading the
// slots at producer-1, producer-2, ... so index 0 is the most recent stamp.
inline bool matchRxCpuStamp(const RxCpuStamp * entries, uint32_t n, uint8_t msgType,
                            uint16_t seq, uint32_t * eavLoOut) {
    if (entries == nullptr) {
        return false;
    }
    for (uint32_t i = 0; i < n; i++) {
        if (entries[i].msgType == msgType && entries[i].seq == seq) {
            if (eavLoOut != nullptr) {
                *eavLoOut = entries[i].eavLo;
            }
            return true;
        }
    }
    return false;
}

}  // namespace tg3logic

#endif  // TG3_LOGIC_HPP
