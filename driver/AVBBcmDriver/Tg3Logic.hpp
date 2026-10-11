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

}  // namespace tg3logic

#endif  // TG3_LOGIC_HPP
