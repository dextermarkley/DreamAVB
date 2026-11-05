// SPDX-License-Identifier: MIT OR Apache-2.0

// Shared RX frame classification and shared-ring index helpers, used by both
// the I210 and the BCM57762 dexts so the DP-4 classifier and the ABI v3 ring
// rules have one implementation. Pure constexpr; host-tested through
// i210logic_tests (moved verbatim from I210Logic.hpp).

#pragma once
#include <stdint.h>

#include "AVBShared.h"

namespace avbrx {

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

}  // namespace avbrx
