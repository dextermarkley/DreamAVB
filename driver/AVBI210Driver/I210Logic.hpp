// SPDX-License-Identifier: MIT OR Apache-2.0

// I210Logic.hpp - pure, header-only helpers for the Intel I210 dext.
//
// No DriverKit includes: this file is compiled both into the dext and into the
// host-side unit tests (driver/tests/i210logic_tests.cpp). Everything here is
// deterministic integer math or bit packing, so it can be tested without
// hardware. Register meanings: I210Regs.h (Intel datasheet 333016).

#ifndef I210_LOGIC_HPP
#define I210_LOGIC_HPP

#include <stdint.h>

#include "../AVBShared.h"
#include "I210Regs.h"

namespace i210logic {

// ---------------------------------------------------------------------------
// Identity / register-access guards.

constexpr bool isOurDevice(uint32_t vendorDevice, uint32_t subsystem) {
    return vendorDevice == i210::kVendorDevice && subsystem == i210::kSubsystem;
}

// Offsets kAvbSelReadReg32 may read. Excludes every register whose read has a
// side effect: read-to-clear causes, statistics, the read-to-set semaphore and
// the timestamp latches (a read releases or re-arms them).
constexpr bool isReadableRegOffsetI210(uint64_t off) {
    if (off >= i210::kMinBar0Size || (off & 3u) != 0) {
        return false;
    }
    if (off >= i210::kStatsBegin && off < i210::kStatsEnd) {
        return false;
    }
    if (off >= i210::kAuxStmpl0 && off <= i210::kAuxStmph1) {
        return false;
    }
    switch (off) {
        case i210::kIcr:
        case i210::kIcrAlias:
        case i210::kEicr:
        case i210::kSwsm:
        case i210::kTsicr:
        case i210::kSystimr:
        case i210::kTxStmpl:
        case i210::kTxStmph:
        case i210::kRxStmpl:
        case i210::kRxStmph:
        case i210::kRxSatrl:
        case i210::kRxSatrh:
        case i210::kRqdpc0:  // owned by the dext's statistics accumulator (DP-8)
        case 0x1084:  // PEIND (read-to-clear)
        case 0x5BF4:  // RR2DCDELAY (read-to-clear)
        case 0x8FDC:  // BMTPDC (read-to-clear)
        case 0x8FE0:  // B2OSPC (read-to-clear)
        case 0x8FE4:  // O2BGPTC (read-to-clear)
            return false;
        default:
            return true;
    }
}

// ---------------------------------------------------------------------------
// Clock: SYSTIMH = seconds, SYSTIML = nanoseconds (< 1e9). The ABI carries
// u64 ns = sec * 1e9 + ns.

constexpr uint64_t kNsPerSec = 1000000000ull;

constexpr uint64_t systimToNs(uint32_t sec, uint32_t ns) {
    return static_cast<uint64_t>(sec) * kNsPerSec + ns;
}

struct Systim {
    uint32_t sec;
    uint32_t ns;
};

constexpr Systim nsToSystim(uint64_t total) {
    return Systim{static_cast<uint32_t>(total / kNsPerSec), static_cast<uint32_t>(total % kNsPerSec)};
}

// TIMINCA for a frequency offset in ppb. Each 8 ns tick adds 8 ns +/-
// Incvalue * 2^-32 ns, so the fractional rate is Incvalue / 2^35 and
// Incvalue = round(|ppb| * 2^35 / 1e9). Caller range-checks |ppb| <= kMaxPpb.
constexpr uint32_t timincaFromPpb(int64_t ppb) {
    const uint64_t absPpb = ppb < 0 ? static_cast<uint64_t>(-ppb) : static_cast<uint64_t>(ppb);
    const uint32_t mag =
        static_cast<uint32_t>(((absPpb << 35) + kNsPerSec / 2) / kNsPerSec) & i210::kTimincaUsedMask;
    if (mag == 0) {
        return 0;
    }
    return mag | (ppb < 0 ? i210::kTimincaIsgn : 0u);
}

// Inverse (rounded to the nearest ppb).
constexpr int64_t ppbFromTiminca(uint32_t reg) {
    const uint64_t mag = reg & ~i210::kTimincaIsgn;
    const int64_t ppb = static_cast<int64_t>((mag * kNsPerSec + (1ull << 34)) >> 35);
    return (reg & i210::kTimincaIsgn) ? -ppb : ppb;
}

// ---------------------------------------------------------------------------
// Ring index helpers (ring sizes are powers of two). Our own copy, so the two
// dexts stay independent.

constexpr bool isPow2(uint32_t n) { return n != 0 && (n & (n - 1)) == 0; }

constexpr uint32_t ringNext(uint32_t idx, uint32_t size) { return (idx + 1) & (size - 1); }

constexpr uint32_t ringPrev(uint32_t idx, uint32_t size) { return (idx - 1) & (size - 1); }

// Entries from `from` (inclusive) up to `to` (exclusive).
constexpr uint32_t ringDistance(uint32_t from, uint32_t to, uint32_t size) {
    return (to - from) & (size - 1);
}

// ---------------------------------------------------------------------------
// Advanced descriptors (16 bytes, little-endian).

struct AdvTxDesc {
    uint64_t addr;
    uint32_t cmdTypeLen;
    uint32_t olinfo;  // PAYLEN 31:14 on read; STA (DD bit 0) on write-back
};
static_assert(sizeof(AdvTxDesc) == 16, "advanced TX descriptor is 16 bytes");

struct AdvRxDesc {
    uint64_t pktAddr;  // read format; write-back overlays this qword
    uint64_t hdrAddr;  // read format (0); write-back: status_error | length | vlan
};
static_assert(sizeof(AdvRxDesc) == 16, "advanced RX descriptor is 16 bytes");

// One-buffer data descriptor for a whole frame. RS on every descriptor so the
// DD write-back can be used to reclaim it.
constexpr AdvTxDesc makeAdvTxDesc(uint64_t iova, uint32_t len, bool wantTs) {
    const uint32_t cmd = (len & i210::kTxdDtalenMask) | i210::kTxdDtypData | i210::kTxdCmdEop |
                         i210::kTxdCmdIfcs | i210::kTxdCmdRs | i210::kTxdCmdDext |
                         (wantTs ? i210::kTxdMac2Step : 0u);
    return AdvTxDesc{iova, cmd, (len & 0x3ffffu) << i210::kTxdPaylenShift};
}

constexpr bool txDescDone(uint32_t olinfo) { return (olinfo & i210::kTxdStaDd) != 0; }

constexpr AdvRxDesc makeAdvRxRead(uint64_t iova) { return AdvRxDesc{iova, 0}; }

struct RxWb {
    uint32_t len;   // PKT_LEN as written (includes the 16 B timestamp header if tsip)
    bool dd;
    bool eop;
    bool tsip;
    bool error;     // RXE (frame error)
};

constexpr RxWb decodeAdvRxWb(uint32_t statusError, uint16_t length) {
    return RxWb{length,
                (statusError & i210::kRxdStaDd) != 0,
                (statusError & i210::kRxdStaEop) != 0,
                (statusError & i210::kRxdStaTsip) != 0,
                (statusError & i210::kRxdErrRxe) != 0};
}

// 7.1.6: bytes 0..7 reserved, 8..11 SYSTIML (ns, LE), 12..15 SYSTIMH (sec, LE).
constexpr uint32_t le32At(const uint8_t* p, uint32_t off) {
    return static_cast<uint32_t>(p[off]) | (static_cast<uint32_t>(p[off + 1]) << 8) |
           (static_cast<uint32_t>(p[off + 2]) << 16) | (static_cast<uint32_t>(p[off + 3]) << 24);
}

constexpr uint64_t parseRxTsHeader(const uint8_t* buf) {
    return systimToNs(le32At(buf, 12), le32At(buf, 8));
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

// Station address from RAL0/RAH0 (bytes in network order, low byte first).
struct Mac {
    uint8_t b[6];
};

constexpr Mac macFromRal(uint32_t ral, uint32_t rah) {
    return Mac{{static_cast<uint8_t>(ral), static_cast<uint8_t>(ral >> 8), static_cast<uint8_t>(ral >> 16),
                static_cast<uint8_t>(ral >> 24), static_cast<uint8_t>(rah), static_cast<uint8_t>(rah >> 8)}};
}

// ---------------------------------------------------------------------------
// Composite register values for Stage B (pinned by the host tests).

constexpr uint32_t kRxBufKiB = 2;

// One-buffer advanced descriptors, 2 KiB buffers, header size left at its
// 256 B default, timestamp in buffer, drop when no descriptors.
constexpr uint32_t srrctlValue() {
    return (kRxBufKiB << i210::kSrrctlBsizePktShift) | (4u << i210::kSrrctlBsizeHdrShift) |
           i210::kSrrctlDescTypeAdvOneBuf | i210::kSrrctlTimestamp | i210::kSrrctlDropEn;
}

// RX on, broadcast + all multicast (01:80:C2:00:00:0E), strip CRC, discard
// pause frames (power-on default); unicast promiscuous when asked.
constexpr uint32_t rctlValue(bool promisc) {
    return i210::kRctlRxEn | i210::kRctlBam | i210::kRctlMpe | i210::kRctlSecrc | i210::kRctlDpf |
           (promisc ? i210::kRctlUpe : 0u);
}

constexpr uint32_t tctlValue() {
    return i210::kTctlEn | i210::kTctlPsp | (i210::kTctlCtValue << i210::kTctlCtShift) |
           (i210::kTctlBstValue << i210::kTctlBstShift);
}

// RXDCTL power-on thresholds (PTHRESH 0xC, HTHRESH 0xA, WTHRESH 1) + ENABLE.
constexpr uint32_t rxdctlEnable() {
    return (0xCu << i210::kDctlPthreshShift) | (0xAu << i210::kDctlHthreshShift) |
           (1u << i210::kDctlWthreshShift) | i210::kDctlEnable;
}

// TXDCTL: WTHRESH 1, everything else 0 (4.5.10 suggestion) + ENABLE.
constexpr uint32_t txdctlEnable() { return (1u << i210::kDctlWthreshShift) | i210::kDctlEnable; }

// TSYNCRXCTL with timestamping on, TYPE = all packets; other bits kept.
constexpr uint32_t tsyncRxCtlValue(uint32_t current) {
    return (current & ~(i210::kTsyncRxTypeMask | i210::kTsyncRxEnabled | i210::kTsyncRxValid)) |
           i210::kTsyncRxTypeAll | i210::kTsyncRxEnabled;
}

// ETQF[3]: EtherType 0x88F7, filter enabled, 1588 timestamp. No queue steering.
constexpr uint32_t etqf1588Value() { return 0x88F7u | i210::kEtqfFilterEn | i210::kEtqf1588; }

// ---------------------------------------------------------------------------
// MDIC frames (internal PHY; PHYADD bits are reserved on the I210).

constexpr uint32_t mdicReadFrame(uint32_t reg) {
    return ((reg << i210::kMdicRegShift) & i210::kMdicRegMask) | i210::kMdicOpRead;
}

constexpr uint32_t mdicWriteFrame(uint32_t reg, uint16_t val) {
    return ((reg << i210::kMdicRegShift) & i210::kMdicRegMask) | i210::kMdicOpWrite |
           (val & i210::kMdicDataMask);
}

struct MdicResult {
    bool ready;
    bool error;
    uint16_t data;
};

constexpr MdicResult mdicDecode(uint32_t mdic) {
    return MdicResult{(mdic & i210::kMdicReady) != 0, (mdic & i210::kMdicError) != 0,
                      static_cast<uint16_t>(mdic & i210::kMdicDataMask)};
}

// BMCR value that restarts autonegotiation out of power-down. Clears
// PDOWN and never sets the PHY reset bit (333016 4.3.1.4 forbids it).
constexpr uint16_t bmcrAutoneg(uint16_t bmcr) {
    return static_cast<uint16_t>(((bmcr & ~(i210::kBmcrPowerDown | 0x8000u)) | i210::kBmcrAnEnable |
                                  i210::kBmcrAnRestart) &
                                 0xffffu);
}

constexpr bool bmcrNeedsAutoneg(uint16_t bmcr) {
    return (bmcr & i210::kBmcrPowerDown) != 0 || (bmcr & i210::kBmcrAnEnable) == 0;
}

// ---------------------------------------------------------------------------
// Link state from STATUS.

struct Link {
    bool up;
    uint32_t mbps;  // 10, 100 or 1000
    bool fd;
};

constexpr Link linkFromStatus(uint32_t status) {
    const uint32_t s = (status & i210::kStatusSpeedMask) >> i210::kStatusSpeedShift;
    return Link{(status & i210::kStatusLu) != 0, s == 0 ? 10u : (s == 1 ? 100u : 1000u),
                (status & i210::kStatusFd) != 0};
}

// ---------------------------------------------------------------------------
// ABI v3 RX path (design §3.4): length guards, multi-descriptor handling,
// VLAN flags and the DP-4 classifier.

// The two harvest length guards, in one place. They run before
// classification and before any copy, in ring and legacy mode alike.
enum class RxBoundsKind : uint8_t { Ok, ShortTs, Oversize };

struct RxBounds {
    RxBoundsKind kind;
    uint32_t flen;  // frame bytes after the TS header (valid for Ok and Oversize)
};

constexpr RxBounds rxFrameBounds(uint32_t pktLen, bool tsip) {
    if (tsip && pktLen < i210::kRxTsHeaderLen) {
        return RxBounds{RxBoundsKind::ShortTs, 0};
    }
    const uint32_t flen = tsip ? pktLen - i210::kRxTsHeaderLen : pktLen;
    if (flen > AVB_RX_SLOT_DATA_MAX) {
        return RxBounds{RxBoundsKind::Oversize, flen};
    }
    return RxBounds{RxBoundsKind::Ok, flen};
}

// Multi-descriptor frames: the first descriptor (no EOP) is counted once as
// NonEop, every following descriptor up to and including the EOP one is
// discarded without being counted as a frame.
enum class RxStep : uint8_t { Discard, NonEop, Frame };

constexpr RxStep rxHarvestStep(bool* discarding, bool eop) {
    if (*discarding) {
        if (eop) {
            *discarding = false;
        }
        return RxStep::Discard;
    }
    if (!eop) {
        *discarding = true;
        return RxStep::NonEop;
    }
    return RxStep::Frame;
}

constexpr bool frameHasVlanTag(const uint8_t* f, uint32_t len) {
    return len >= 14 && f[12] == 0x81 && f[13] == 0x00;
}

// Exactly one of: none, VLAN_FRAME (in-buffer tag; it always wins), VLAN_DESC
// (descriptor VP set and no in-buffer tag: the tag is in the descriptor).
constexpr uint16_t rxVlanFlags(bool vp, const uint8_t* f, uint32_t len) {
    if (frameHasVlanTag(f, len)) {
        return AVB_RXF_VLAN_FRAME;
    }
    return vp ? AVB_RXF_VLAN_DESC : 0;
}

constexpr bool macEq(const uint8_t* a, const uint8_t* b) {
    for (uint32_t i = 0; i < 6; i++) {
        if (a[i] != b[i]) {
            return false;
        }
    }
    return true;
}

// AVDECC/MAAP destinations: the AVDECC multicast 91:E0:F0:01:00:00, the
// 91:E0:F0:00:xx:xx block, or our own station address. Same table as
// avb_core::is_avdecc_dest.
constexpr bool isAvdeccDest(const uint8_t* dst, const uint8_t* ourMac) {
    const uint8_t kAvdeccMc[6] = {0x91, 0xE0, 0xF0, 0x01, 0x00, 0x00};
    if (macEq(dst, kAvdeccMc)) {
        return true;
    }
    if (dst[0] == 0x91 && dst[1] == 0xE0 && dst[2] == 0xF0 && dst[3] == 0x00) {
        return true;
    }
    return macEq(dst, ourMac);
}

// Classifier verdicts. Delivered verdicts carry their AvbRxClass value.
enum class RxVerdict : uint8_t {
    Gptp = kAvbRxGptp,
    Msrp = kAvbRxMsrp,
    Mvrp = kAvbRxMvrp,
    Avdecc = kAvbRxAvdecc,
    Maap = kAvbRxMaap,
    Stream = kAvbRxStream,
    DropOther = 0x80,
    DropUnboundStream = 0x81,
    DropUntaggedStream = 0x82,
};

constexpr bool rxDelivered(RxVerdict v) {
    return static_cast<uint8_t>(v) >= kAvbRxGptp && static_cast<uint8_t>(v) <= kAvbRxStream;
}

// AvbCounters index for a verdict (delivered: per-class counter = cls).
constexpr uint32_t rxVerdictCounter(RxVerdict v) {
    switch (v) {
        case RxVerdict::DropOther:
            return kAvbCtrRxDropOther;
        case RxVerdict::DropUnboundStream:
            return kAvbCtrRxDropUnboundStream;
        case RxVerdict::DropUntaggedStream:
            return kAvbCtrRxDropUntaggedStream;
        default:
            return kAvbCtrRxClassBase + static_cast<uint8_t>(v) - 1;
    }
}

constexpr uint16_t kEtherVlan = 0x8100;
constexpr uint16_t kEtherGptp = 0x88F7;
constexpr uint16_t kEtherMsrp = 0x22EA;
constexpr uint16_t kEtherMvrp = 0x88F5;
constexpr uint16_t kEtherAvtp = 0x22F0;
constexpr uint8_t kAvtpSubtypeAdp = 0xFA;
constexpr uint8_t kAvtpSubtypeAecp = 0xFB;
constexpr uint8_t kAvtpSubtypeAcmp = 0xFC;
constexpr uint8_t kAvtpSubtypeMaap = 0xFE;

// DP-4 classification of one received frame (data[] without TS header).
// vp/vlanField come from the advanced RX write-back descriptor.
constexpr RxVerdict classifyRx(const uint8_t* f, uint32_t len, const uint8_t* ourMac,
                               const AvbStreamFilter& filter, bool vp, uint16_t vlanField) {
    if (len < 14) {
        return RxVerdict::DropOther;
    }
    const uint8_t* dst = f;
    uint16_t et = static_cast<uint16_t>((f[12] << 8) | f[13]);
    bool tagged = false;
    uint16_t vid = 0;
    uint32_t payload = 14;
    if (et == kEtherVlan) {
        if (len < 18) {
            return RxVerdict::DropOther;
        }
        tagged = true;
        vid = static_cast<uint16_t>(((f[14] << 8) | f[15]) & 0x0FFF);
        et = static_cast<uint16_t>((f[16] << 8) | f[17]);
        payload = 18;
    } else if (vp) {
        tagged = true;  // tag stripped into the descriptor (U-12)
        vid = static_cast<uint16_t>(vlanField & 0x0FFF);
    }

    const uint8_t kNearestBridge[6] = {0x01, 0x80, 0xC2, 0x00, 0x00, 0x0E};
    const uint8_t kMvrpAddr[6] = {0x01, 0x80, 0xC2, 0x00, 0x00, 0x21};
    if (et == kEtherGptp && macEq(dst, kNearestBridge)) {
        return RxVerdict::Gptp;
    }
    if (et == kEtherMsrp && macEq(dst, kNearestBridge)) {
        return RxVerdict::Msrp;
    }
    if (et == kEtherMvrp && macEq(dst, kMvrpAddr)) {
        return RxVerdict::Mvrp;
    }
    if (et != kEtherAvtp || len < payload + 1) {
        return RxVerdict::DropOther;
    }
    const uint8_t subtype = f[payload];
    if (subtype == kAvtpSubtypeAdp || subtype == kAvtpSubtypeAecp || subtype == kAvtpSubtypeAcmp) {
        return isAvdeccDest(dst, ourMac) ? RxVerdict::Avdecc : RxVerdict::DropOther;
    }
    if (subtype == kAvtpSubtypeMaap) {
        return isAvdeccDest(dst, ourMac) ? RxVerdict::Maap : RxVerdict::DropOther;
    }
    const uint32_t n = filter.count < AVB_MAX_STREAMS ? filter.count : AVB_MAX_STREAMS;
    bool macKnown = false;
    for (uint32_t i = 0; i < n; i++) {
        if (macEq(dst, filter.e[i].dest_mac)) {
            macKnown = true;
            if (tagged && vid == filter.e[i].vid) {
                return RxVerdict::Stream;
            }
        }
    }
    if (macKnown && !tagged) {
        return RxVerdict::DropUntaggedStream;
    }
    return RxVerdict::DropUnboundStream;
}

// SetStreamFilter validation: count <= S_max, multicast destinations,
// 1 <= vid <= 4094.
constexpr bool streamFilterValid(const AvbStreamFilter& f) {
    if (f.count > AVB_MAX_STREAMS) {
        return false;
    }
    for (uint32_t i = 0; i < f.count; i++) {
        if ((f.e[i].dest_mac[0] & 1u) == 0 || f.e[i].vid < 1 || f.e[i].vid > 4094) {
            return false;
        }
    }
    return true;
}

// ---------------------------------------------------------------------------
// Shared ring indices (monotonic u64 records; slots is a power of two).
// The consumer tail is client-writable, so it is validated every tick:
// 0 <= head - tail <= slots (mod 2^64). Invalid -> treat the ring as full.

struct RingSpace {
    bool valid;
    uint64_t free;
};

constexpr RingSpace ringFree(uint64_t head, uint64_t tail, uint32_t slots) {
    const uint64_t used = head - tail;  // wraps at 2^64 by design
    if (used > slots) {
        return RingSpace{false, 0};
    }
    return RingSpace{true, slots - used};
}

// live header producer_running rule (§3.4)
constexpr bool producerRunning(bool attached, uint32_t stage, uint32_t fault) {
    return attached && stage == kAvbStageDatapath && fault == kAvbFaultNone;
}

// ---------------------------------------------------------------------------
// StepClock (§3.6): target = s0 + delta + elapsed, where elapsed is the time
// since the mid-point of the s0 read. |delta| <= AVB_STEP_CLOCK_MAX_NS and
// the result must lie in [0, 2^32 s) (the SYSTIMH range).

constexpr uint64_t kSystimLimitNs = 4294967296ull * kNsPerSec;

struct StepResult {
    bool ok;
    uint64_t target;
};

constexpr StepResult stepTarget(uint64_t s0, int64_t delta, uint64_t elapsedNs) {
    if (delta > AVB_STEP_CLOCK_MAX_NS || delta < -AVB_STEP_CLOCK_MAX_NS || s0 >= kSystimLimitNs ||
        elapsedNs > static_cast<uint64_t>(AVB_STEP_CLOCK_MAX_NS)) {
        return StepResult{false, 0};
    }
    // s0 < 2^32 s (~4.3e18) and both other terms are <= 1e9, so int64 holds it.
    const int64_t t = static_cast<int64_t>(s0) + delta + static_cast<int64_t>(elapsedNs);
    if (t < 0 || static_cast<uint64_t>(t) >= kSystimLimitNs) {
        return StepResult{false, 0};
    }
    return StepResult{true, static_cast<uint64_t>(t)};
}

// Step tagging (§3.6). Every harvest tick from a SYSTIM write up to and
// including the first tick that starts >= settleTicks after it is
// ambiguous. Times are mach ticks.
struct RxStepAmbig {
    bool pending;
    uint64_t stepMach;
    uint64_t settleTicks;
};

constexpr void rxStepArm(RxStepAmbig* s, uint64_t nowMach, uint64_t settle) {
    s->pending = true;
    s->stepMach = nowMach;  // a second step before settling moves the window forward
    s->settleTicks = settle;
}

// Called once per harvest tick with the tick's measured start time.
constexpr bool rxStepAmbiguous(RxStepAmbig* s, uint64_t tickStartMach) {
    if (!s->pending) {
        return false;
    }
    if (tickStartMach >= s->stepMach + s->settleTicks) {
        s->pending = false;
    }
    return true;
}

// ---------------------------------------------------------------------------
// TX token bucket (SAF-6): AVB_TX_RATE_PER_SEC refill, AVB_TX_BURST depth,
// starts full. Integer ns; credit below one token carries over.

constexpr uint64_t kTxTokenNs = kNsPerSec / AVB_TX_RATE_PER_SEC;
static_assert(kNsPerSec % AVB_TX_RATE_PER_SEC == 0, "token period is an integer ns");

struct TokenBucket {
    bool primed;
    uint32_t tokens;
    uint64_t lastNs;  // time up to which refill has been credited
};

constexpr bool tokenBucketTake(TokenBucket* b, uint64_t nowNs) {
    if (!b->primed) {
        b->primed = true;
        b->tokens = AVB_TX_BURST;
        b->lastNs = nowNs;
    } else if (nowNs > b->lastNs) {
        const uint64_t n = (nowNs - b->lastNs) / kTxTokenNs;
        if (b->tokens + n >= AVB_TX_BURST) {
            b->tokens = AVB_TX_BURST;
            b->lastNs = nowNs;
        } else {
            b->tokens += static_cast<uint32_t>(n);
            b->lastNs += n * kTxTokenNs;
        }
    }
    if (b->tokens == 0) {
        return false;
    }
    b->tokens--;
    return true;
}

// ---------------------------------------------------------------------------
// Hardware statistics accumulation (every 100 ms). CRCERRS, MPC and RNBC are
// clear-on-read: add the value. RQDPC(0) is a free-running 32-bit counter
// that wraps (333016 §8.10.10): add the modulo-2^32 delta since the last read.

struct HwStatsRead {
    uint32_t crcerrs;
    uint32_t mpc;
    uint32_t rnbc;
    uint32_t rqdpc0;
};

constexpr void statsAccumulate(AvbCounters* c, uint32_t* rqdpcLast, const HwStatsRead& r) {
    c->c[kAvbCtrHwCrcErrs] += r.crcerrs;
    c->c[kAvbCtrHwMpc] += r.mpc;
    c->c[kAvbCtrHwRnbc] += r.rnbc;
    c->c[kAvbCtrHwRqdpc0] += static_cast<uint32_t>(r.rqdpc0 - *rqdpcLast);
    *rqdpcLast = r.rqdpc0;
}

}  // namespace i210logic

#endif  // I210_LOGIC_HPP
