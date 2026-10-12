// SPDX-License-Identifier: MIT OR Apache-2.0

// Host-side unit tests for I210Logic.hpp and the AVBShared.h ABI (v3).
// Plain assert-style runner: prints one line per check group, exits non-zero
// on the first failure.

#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <cstring>

#include "../AVBShared.h"
#include "../AVBI210Driver/I210Logic.hpp"

using namespace i210logic;

static int gChecks = 0;

#define CHECK(cond)                                                              \
    do {                                                                         \
        ++gChecks;                                                               \
        if (!(cond)) {                                                           \
            std::fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond); \
            std::exit(1);                                                        \
        }                                                                        \
    } while (0)

// ABI sizes and offsets (mirrored by the Rust abi.rs offset_of!/size_of tests).
static_assert(AVB_ABI_VERSION == 3);
static_assert(sizeof(AvbInfo) == 60);
static_assert(sizeof(AvbClockSample) == 24);
static_assert(sizeof(AvbRxEntry) == 152);
static_assert(sizeof(AvbRxBatch) == 3656);
static_assert(offsetof(AvbInfo, hw_rev) == 12);
static_assert(offsetof(AvbInfo, ctrl_a) == 16);
static_assert(offsetof(AvbInfo, ctrl_b) == 20);
static_assert(offsetof(AvbInfo, mac) == 36);
static_assert(offsetof(AvbInfo, tb_numer) == 44);
static_assert(offsetof(AvbInfo, nic_kind) == 52);
static_assert(offsetof(AvbInfo, clock_ctl) == 56);
static_assert(AVB_NIC_BCM57762 == 1 && AVB_NIC_I210 == 2);
static_assert(kAvbFaultMasterDisable == 11 && kAvbFaultQueueEnable == 14);
static_assert(AVB_REG_READ_LIMIT == 0x20000);

// ABI v3 literals (design §3.1). The Rust mirror asserts the same values.
static_assert(kAvbSelStepClock == 10 && kAvbSelGetCounters == 11 && kAvbSelSetStreamFilter == 12 &&
              kAvbSelRingAttach == 13 && kAvbSelRingDetach == 14);
static_assert(kAvbSelCount == 15 && kAvbSelBaseCount == 10);
static_assert(kAvbMemRx == 1 && kAvbMemConsumer == 2);
static_assert(AVB_FLAG_RINGS_ATTACHED == 0x20 && AVB_FLAG_OWNED == 0x40);
static_assert(AVB_SHM_MAGIC == 0x33425641u);
static_assert(AVB_SHM_HEADER_SIZE == 16384 && AVB_RX_SLOT_SIZE == 2048 && AVB_RX_SLOT_HDR == 64);
static_assert(AVB_RX_SLOT_DATA_MAX == 1984 && AVB_RX_CTL_SLOTS == 512 && AVB_RX_AUDIO_SLOTS == 8192);
static_assert(AVB_RX_CTL_OFFSET == 16384 && AVB_RX_AUDIO_OFFSET == 16384 + 512 * 2048);
static_assert(AVB_RX_MEM_SIZE == 17842176u && AVB_CONSUMER_MEM_SIZE == 16384);
static_assert(AVB_MAX_STREAMS == 16 && AVB_STEP_CLOCK_MAX_NS == 1000000000ll);
static_assert(AVB_TX_RATE_PER_SEC == 80000 && AVB_TX_BURST == 64);
static_assert(AVB_TX_MAX_CLASS_A_STREAMS == 8);
// The ceiling must clear EVERY SR class A stream we talk at once (8000
// frames/s each, one per 125 us) with headroom for control traffic, since the
// bucket is shared across all TX. The 16R has 8 AAF streams per direction, so
// a full-width talker needs 64 000 frames/s.
static_assert(AVB_TX_RATE_PER_SEC > AVB_TX_MAX_CLASS_A_STREAMS * 8000u,
              "a full-width class A talker must fit under SAF-6");
static_assert(AVB_TX_RATE_PER_SEC - AVB_TX_MAX_CLASS_A_STREAMS * 8000u >= 8000u,
              "control traffic needs headroom a stream can never take");
static_assert(kAvbRxNone == 0 && kAvbRxGptp == 1 && kAvbRxMsrp == 2 && kAvbRxMvrp == 3 && kAvbRxAvdecc == 4 &&
              kAvbRxMaap == 5 && kAvbRxStream == 6 && AVB_RX_CLASS_COUNT == 7);
static_assert(AVB_RXF_TS_HW == 1 && AVB_RXF_VLAN_FRAME == 2 && AVB_RXF_VLAN_DESC == 4 &&
              AVB_RXF_TS_STEP_AMBIG == 8);
static_assert(AVB_STEP_SETTLE_NS == 1000000u);
static_assert(sizeof(AvbRxSlot) == 2048);
static_assert(offsetof(AvbRxSlot, hw_ts_ns) == 0 && offsetof(AvbRxSlot, harvest_mach) == 8 &&
              offsetof(AvbRxSlot, index) == 16 && offsetof(AvbRxSlot, status_error) == 24 &&
              offsetof(AvbRxSlot, len) == 28 && offsetof(AvbRxSlot, vlan_tci) == 30 &&
              offsetof(AvbRxSlot, flags) == 32 && offsetof(AvbRxSlot, cls) == 34 &&
              offsetof(AvbRxSlot, pad0) == 35 && offsetof(AvbRxSlot, desc_len) == 36 &&
              offsetof(AvbRxSlot, clock_gen) == 40 && offsetof(AvbRxSlot, pad1) == 44 &&
              offsetof(AvbRxSlot, data) == 64);
static_assert(kAvbCtrRxFrames == 0 && kAvbCtrRxClassBase == 1 && kAvbCtrRxDropOther == 8 &&
              kAvbCtrRxDropUnboundStream == 9 && kAvbCtrRxDropUntaggedStream == 10 && kAvbCtrRxErrors == 11 &&
              kAvbCtrRxNonEop == 12 && kAvbCtrRingOverrunCtl == 13 && kAvbCtrRingOverrunAudio == 14 &&
              kAvbCtrConsumerIndexInvalid == 15 && kAvbCtrHwMpc == 16 && kAvbCtrHwRnbc == 17 &&
              kAvbCtrHwRqdpc0 == 18 && kAvbCtrHwCrcErrs == 19 && kAvbCtrTxFrames == 20 &&
              kAvbCtrTxRateLimited == 21 && kAvbCtrTxTsRequested == 22 && kAvbCtrTxTsOk == 23 &&
              kAvbCtrTxTsTimeout == 24 && kAvbCtrTxTsBusyRejected == 25 && kAvbCtrTxStaleLatch == 26 &&
              kAvbCtrLinkUpEvents == 27 && kAvbCtrLinkDownEvents == 28 && kAvbCtrHarvestTicks == 29 &&
              kAvbCtrHarvestMaxBatch == 30 && kAvbCtrHarvestLateTicks == 31 && kAvbCtrLegacySwOverrun == 32 &&
              kAvbCtrRxOversize == 33 && kAvbCtrCount == 64);
static_assert(sizeof(AvbCounters) == 512);
static_assert(sizeof(AvbShmHeader) == 1024);
static_assert(offsetof(AvbShmHeader, magic) == 0 && offsetof(AvbShmHeader, abi_version) == 4 &&
              offsetof(AvbShmHeader, header_size) == 8 && offsetof(AvbShmHeader, slot_size) == 12 &&
              offsetof(AvbShmHeader, ctl_slots) == 16 && offsetof(AvbShmHeader, audio_slots) == 20 &&
              offsetof(AvbShmHeader, ctl_offset) == 24 && offsetof(AvbShmHeader, audio_offset) == 28 &&
              offsetof(AvbShmHeader, total_size) == 32 && offsetof(AvbShmHeader, generation) == 36 &&
              offsetof(AvbShmHeader, stage) == 40 && offsetof(AvbShmHeader, fault) == 44 &&
              offsetof(AvbShmHeader, flags) == 48 && offsetof(AvbShmHeader, producer_running) == 52 &&
              offsetof(AvbShmHeader, last_tick_mach) == 56 && offsetof(AvbShmHeader, mac) == 64 &&
              offsetof(AvbShmHeader, pad0) == 70 && offsetof(AvbShmHeader, ctl_head) == 128 &&
              offsetof(AvbShmHeader, pad1) == 136 && offsetof(AvbShmHeader, audio_head) == 256 &&
              offsetof(AvbShmHeader, pad2) == 264 && offsetof(AvbShmHeader, counters) == 384 &&
              offsetof(AvbShmHeader, clock_gen) == 896 && offsetof(AvbShmHeader, reserved) == 900);
static_assert(sizeof(AvbShmConsumer) == 256 && offsetof(AvbShmConsumer, ctl_tail) == 0 &&
              offsetof(AvbShmConsumer, audio_tail) == 128);
static_assert(sizeof(AvbStreamFilterEntry) == 8 && offsetof(AvbStreamFilterEntry, vid) == 6);
static_assert(sizeof(AvbStreamFilter) == 136 && offsetof(AvbStreamFilter, count) == 0 &&
              offsetof(AvbStreamFilter, e) == 8);
// Per-class counter index == cls (Gptp -> 1, Stream -> 6).
static_assert(rxVerdictCounter(RxVerdict::Gptp) == 1 && rxVerdictCounter(RxVerdict::Stream) == 6);
static_assert(i210::kRqdpc0 == 0x0C030 && i210::kCrcerrs == 0x04000 && i210::kMpc == 0x04010 &&
              i210::kRnbc == 0x040A0 && i210::kRxdStaVp == 0x8);

static void testIdentity() {
    CHECK(isOurDevice(0x15338086, 0x02141ab6));
    CHECK(!isOurDevice(0x168214e4, 0x00f6106b));  // BCM57762 (Apple TB adapter)
    CHECK(!isOurDevice(0x15338086, 0x00008086));  // I210 with another subsystem
    CHECK(!isOurDevice(0x15398086, 0x02141ab6));  // I211
    CHECK(!isOurDevice(0x15338086, 0x00000000));
    CHECK(!isOurDevice(0xffffffff, 0xffffffff));
    std::printf("pass: identity (ours only; BCM, I211, other subsystem rejected)\n");
}

// Shared golden table (identical values in crates/avb-probe/src/freq.rs).
struct IncCase {
    int64_t ppb;
    uint32_t reg;
};
static const IncCase kIncCases[] = {
    {0, 0x00000000},       {1, 0x00000022},       {-1, 0x80000022},
    {29, 0x000003E4},      {100000, 0x00346DC6},  {-100000, 0x80346DC6},
    {488281, 0x00FFFFF7},  {-488281, 0x80FFFFF7},
};

static void testTiminca() {
    for (const auto& c : kIncCases) {
        CHECK(timincaFromPpb(c.ppb) == c.reg);
    }
    CHECK((timincaFromPpb(-5) & i210::kTimincaIsgn) != 0);
    CHECK((timincaFromPpb(5) & i210::kTimincaIsgn) == 0);
    const int64_t probes[] = {0, 1, -1, 29, 100000, -100000, 488281, -488281, 12345, -54321};
    for (int64_t p : probes) {
        const int64_t back = ppbFromTiminca(timincaFromPpb(p));
        const int64_t err = back > p ? back - p : p - back;
        CHECK(err <= 1);  // one LSB is 0.029 ppb
    }
    CHECK(ppbFromTiminca(0x00346DC6) == 100000);
    CHECK((timincaFromPpb(i210::kMaxPpb) & ~i210::kTimincaIsgn) <= i210::kTimincaUsedMask);
    std::printf("pass: TIMINCA encoding (%zu golden cases)\n", sizeof(kIncCases) / sizeof(kIncCases[0]));
}

static void testSystim() {
    CHECK(systimToNs(0, 0) == 0);
    CHECK(systimToNs(1, 999999999) == 1999999999ull);
    CHECK(systimToNs(0xffffffffu, 999999999) == 4294967295ull * 1000000000ull + 999999999ull);
    const uint64_t vals[] = {0, 1, 999999999, 1000000000, 1234567890123456789ull, 4294967295999999999ull};
    for (uint64_t v : vals) {
        const Systim s = nsToSystim(v);
        CHECK(s.ns < 1000000000u);
        CHECK(systimToNs(s.sec, s.ns) == v);
    }
    const Systim s = nsToSystim(5999999999ull);
    CHECK(s.sec == 5 && s.ns == 999999999u);
    std::printf("pass: SYSTIM split/join\n");
}

static void testTxDesc() {
    const AdvTxDesc a = makeAdvTxDesc(0x0000001234567800ull, 60, false);
    CHECK(a.addr == 0x0000001234567800ull);
    CHECK(a.cmdTypeLen == (60u | 0x00300000u | 0x01000000u | 0x02000000u | 0x08000000u | 0x20000000u));
    CHECK(a.cmdTypeLen == 0x2B30003Cu);
    CHECK(a.olinfo == (60u << 14));
    const AdvTxDesc b = makeAdvTxDesc(0x1000, 1514, true);
    CHECK(b.cmdTypeLen == (0x2B3805EAu));
    CHECK((b.cmdTypeLen & i210::kTxdMac2Step) != 0);
    CHECK(b.olinfo == (1514u << 14));
    CHECK(!txDescDone(b.olinfo));
    CHECK(txDescDone(0x1));
    std::printf("pass: advanced TX descriptor encode\n");
}

static void testRxDesc() {
    const AdvRxDesc r = makeAdvRxRead(0xABCD0000ull);
    CHECK(r.pktAddr == 0xABCD0000ull && r.hdrAddr == 0);
    // DD | EOP | TSIP, 16 B header + 68 B frame.
    RxWb w = decodeAdvRxWb(0x00008003u, 84);
    CHECK(w.dd && w.eop && w.tsip && !w.error && w.len == 84);
    w = decodeAdvRxWb(0x00000003u, 60);
    CHECK(w.dd && w.eop && !w.tsip);
    w = decodeAdvRxWb(0x80000003u, 60);  // RXE
    CHECK(w.error);
    w = decodeAdvRxWb(0x20000003u, 60);  // L4E alone is not a frame error
    CHECK(!w.error);
    w = decodeAdvRxWb(0x00000001u, 2048);  // non-last descriptor
    CHECK(w.dd && !w.eop);
    CHECK(!decodeAdvRxWb(0, 0).dd);
    std::printf("pass: advanced RX descriptor decode\n");
}

static void testTsHeaderAndFrame() {
    uint8_t buf[32] = {};
    // ns = 123456789 (0x075BCD15), sec = 0x12345678.
    buf[8] = 0x15;
    buf[9] = 0xCD;
    buf[10] = 0x5B;
    buf[11] = 0x07;
    buf[12] = 0x78;
    buf[13] = 0x56;
    buf[14] = 0x34;
    buf[15] = 0x12;
    CHECK(parseRxTsHeader(buf) == 0x12345678ull * 1000000000ull + 123456789ull);
    buf[0] = 0xff;  // reserved bytes are ignored
    CHECK(parseRxTsHeader(buf) == 0x12345678ull * 1000000000ull + 123456789ull);

    uint8_t f[20] = {};
    f[12] = 0x88;
    f[13] = 0xF7;
    CHECK(frameEthertype(f, 60) == 0x88F7);
    CHECK(frameEthertype(f, 13) == 0);
    f[12] = 0x81;
    f[13] = 0x00;
    f[16] = 0x22;
    f[17] = 0xF0;
    CHECK(frameEthertype(f, 60) == 0x22F0);

    const Mac m = macFromRal(0x33221100u, 0x80005544u);
    CHECK(m.b[0] == 0x00 && m.b[1] == 0x11 && m.b[2] == 0x22 && m.b[3] == 0x33 && m.b[4] == 0x44 &&
          m.b[5] == 0x55);
    std::printf("pass: timestamp header / ethertype / MAC\n");
}

static void testComposites() {
    CHECK(srrctlValue() == (2u | (4u << 8) | (1u << 25) | (1u << 30) | (1u << 31)));
    CHECK(srrctlValue() == 0xC2000402u);
    CHECK(rctlValue(false) == (0x2u | 0x10u | 0x8000u | 0x400000u | 0x4000000u));
    CHECK(rctlValue(true) == (rctlValue(false) | 0x8u));
    CHECK(tctlValue() == (0x2u | 0x8u | (0xFu << 4) | (0x40u << 12)));
    CHECK(tctlValue() == 0x000400FAu);
    CHECK(rxdctlEnable() == 0x02010A0Cu);
    CHECK(txdctlEnable() == 0x02010000u);
    CHECK(tsyncRxCtlValue(0) == 0x18u);
    CHECK(tsyncRxCtlValue(0xFFFFFFFFu) == 0xFFFFFFF8u);  // only TYPE/EN/VALID touched
    CHECK(etqf1588Value() == 0x440088F7u);
    CHECK(i210::kEtqf3 == 0x5CBCu);
    std::printf("pass: Stage B composite register values\n");
}

static void testMdicAndLink() {
    CHECK(mdicReadFrame(1) == 0x08010000u);
    CHECK(mdicWriteFrame(0, 0x1340) == 0x04001340u);
    CHECK((mdicReadFrame(31) & 0x03E00000u) == 0);  // PHYADD bits stay 0 (reserved)
    MdicResult r = mdicDecode(0x10007949u);
    CHECK(r.ready && !r.error && r.data == 0x7949);
    r = mdicDecode(0x48000000u);
    CHECK(!r.ready && r.error);
    CHECK(bmcrNeedsAutoneg(0x0800));   // power-down
    CHECK(bmcrNeedsAutoneg(0x0140));   // AN off
    CHECK(!bmcrNeedsAutoneg(0x1140));
    CHECK(bmcrAutoneg(0x0940) == 0x1340);
    CHECK((bmcrAutoneg(0x8000) & 0x8000) == 0);  // never PHY reset

    Link l = linkFromStatus(0x00080083u);  // LU | FD | speed 10b
    CHECK(l.up && l.fd && l.mbps == 1000);
    l = linkFromStatus(0x000000C3u);  // speed 11b also 1000
    CHECK(l.mbps == 1000);
    l = linkFromStatus(0x00000043u);
    CHECK(l.mbps == 100);
    l = linkFromStatus(0x00000000u);
    CHECK(!l.up && !l.fd && l.mbps == 10);
    std::printf("pass: MDIC frames / BMCR / STATUS link decode\n");
}

static void testReadable() {
    CHECK(isReadableRegOffsetI210(0x0000));
    CHECK(isReadableRegOffsetI210(0x0008));
    CHECK(isReadableRegOffsetI210(0xB600));
    CHECK(isReadableRegOffsetI210(0xB604));
    CHECK(isReadableRegOffsetI210(0xB608));
    CHECK(isReadableRegOffsetI210(0xB614));
    CHECK(isReadableRegOffsetI210(0xB620));
    CHECK(isReadableRegOffsetI210(0x12030));
    CHECK(isReadableRegOffsetI210(0x1fffc));
    CHECK(!isReadableRegOffsetI210(0x20000));
    CHECK(!isReadableRegOffsetI210(0x0002));
    CHECK(!isReadableRegOffsetI210(0x00C0));
    CHECK(!isReadableRegOffsetI210(0x1500));
    CHECK(!isReadableRegOffsetI210(0x1580));
    CHECK(!isReadableRegOffsetI210(0x4000));
    CHECK(!isReadableRegOffsetI210(0x4ffc));
    CHECK(isReadableRegOffsetI210(0x5000));
    CHECK(!isReadableRegOffsetI210(0x5B50));
    CHECK(!isReadableRegOffsetI210(0xB618));
    CHECK(!isReadableRegOffsetI210(0xB61C));
    CHECK(!isReadableRegOffsetI210(0xB624));
    CHECK(!isReadableRegOffsetI210(0xB628));
    CHECK(!isReadableRegOffsetI210(0xB62C));
    CHECK(!isReadableRegOffsetI210(0xB630));
    CHECK(!isReadableRegOffsetI210(0xB65C));
    CHECK(!isReadableRegOffsetI210(0xB668));
    CHECK(!isReadableRegOffsetI210(0xB66C));
    CHECK(!isReadableRegOffsetI210(0xB6F8));
    // Phase 1a statistics: only the dext's accumulator reads them.
    CHECK(!isReadableRegOffsetI210(0xC030));  // RQDPC(0)
    CHECK(!isReadableRegOffsetI210(0x4000));  // CRCERRS
    CHECK(!isReadableRegOffsetI210(0x4010));  // MPC
    CHECK(!isReadableRegOffsetI210(0x40A0));  // RNBC
    CHECK(isReadableRegOffsetI210(0xC028));   // RXDCTL0 next to it stays readable
    CHECK(isReadableRegOffsetI210(0xC034));
    std::printf("pass: readable-offset denylist\n");
}

static void testRing() {
    CHECK(isPow2(128) && isPow2(64) && !isPow2(0) && !isPow2(96));
    CHECK(ringNext(127, 128) == 0);
    CHECK(ringNext(5, 128) == 6);
    CHECK(ringPrev(0, 128) == 127);
    CHECK(ringDistance(60, 4, 64) == 8);
    CHECK(ringDistance(4, 4, 64) == 0);
    std::printf("pass: ring index wrap\n");
}

// ---------------------------------------------------------------------------
// ABI v3 / Phase 1a helpers (design §3.7)

static const uint8_t kOurMac[6] = {0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8};
static const uint8_t kNearestBridge[6] = {0x01, 0x80, 0xC2, 0x00, 0x00, 0x0E};
static const uint8_t kMvrpDst[6] = {0x01, 0x80, 0xC2, 0x00, 0x00, 0x21};
static const uint8_t kAvdeccMc[6] = {0x91, 0xE0, 0xF0, 0x01, 0x00, 0x00};
static const uint8_t kStreamA[6] = {0x91, 0xE0, 0xF0, 0x00, 0xFE, 0x01};
static const uint8_t kStreamB[6] = {0x91, 0xE0, 0xF0, 0x00, 0xFE, 0x02};
static const uint8_t kStreamC[6] = {0x91, 0xE0, 0xF0, 0x00, 0xFE, 0x03};  // not in the filter
static const uint8_t kBroadcast[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
static const uint8_t kOtherUnicast[6] = {0x00, 0x0a, 0x92, 0x01, 0x02, 0x03};

// 60-byte test frame; returns its length. tagged: in-buffer 802.1Q tag, PCP 3.
static uint32_t mkFrame(uint8_t* f, const uint8_t* dst, bool tagged, uint16_t vid, uint16_t et, uint8_t sub) {
    std::memset(f, 0, 64);
    std::memcpy(f, dst, 6);
    std::memcpy(f + 6, kOtherUnicast, 6);
    uint32_t o = 12;
    if (tagged) {
        const uint16_t tci = static_cast<uint16_t>((3u << 13) | vid);
        f[12] = 0x81;
        f[13] = 0x00;
        f[14] = static_cast<uint8_t>(tci >> 8);
        f[15] = static_cast<uint8_t>(tci);
        o = 16;
    }
    f[o] = static_cast<uint8_t>(et >> 8);
    f[o + 1] = static_cast<uint8_t>(et);
    f[o + 2] = sub;
    return 60;
}

static AvbStreamFilter testFilter() {
    AvbStreamFilter flt{};
    flt.count = 2;
    std::memcpy(flt.e[0].dest_mac, kStreamA, 6);
    flt.e[0].vid = 2;
    std::memcpy(flt.e[1].dest_mac, kStreamB, 6);
    flt.e[1].vid = 2;
    return flt;
}

static RxVerdict cls(const uint8_t* dst, bool tagged, uint16_t vid, uint16_t et, uint8_t sub, bool vp = false,
                     uint16_t vlanField = 0) {
    uint8_t f[64];
    const uint32_t len = mkFrame(f, dst, tagged, vid, et, sub);
    const AvbStreamFilter flt = testFilter();
    return classifyRx(f, len, kOurMac, flt, vp, vlanField);
}

static void testClassify() {
    const AvbStreamFilter flt = testFilter();
    uint8_t f[64];
    // Rule 1: short frames.
    mkFrame(f, kNearestBridge, false, 0, 0x88F7, 0);
    CHECK(classifyRx(f, 13, kOurMac, flt, false, 0) == RxVerdict::DropOther);
    CHECK(classifyRx(f, 0, kOurMac, flt, false, 0) == RxVerdict::DropOther);
    mkFrame(f, kNearestBridge, true, 2, 0x88F7, 0);
    CHECK(classifyRx(f, 17, kOurMac, flt, false, 0) == RxVerdict::DropOther);  // truncated tag
    CHECK(classifyRx(f, 18, kOurMac, flt, false, 0) == RxVerdict::Gptp);
    // Rules 2-4: ethertype + destination.
    CHECK(cls(kNearestBridge, false, 0, 0x88F7, 0) == RxVerdict::Gptp);
    CHECK(cls(kNearestBridge, true, 0, 0x88F7, 0) == RxVerdict::Gptp);
    CHECK(cls(kAvdeccMc, false, 0, 0x88F7, 0) == RxVerdict::DropOther);
    CHECK(cls(kNearestBridge, false, 0, 0x22EA, 0) == RxVerdict::Msrp);
    CHECK(cls(kMvrpDst, false, 0, 0x22EA, 0) == RxVerdict::DropOther);
    CHECK(cls(kMvrpDst, false, 0, 0x88F5, 0) == RxVerdict::Mvrp);
    CHECK(cls(kNearestBridge, false, 0, 0x88F5, 0) == RxVerdict::DropOther);
    // Rule 5: AVTP control subtypes and MAAP.
    const uint8_t inBlock[6] = {0x91, 0xE0, 0xF0, 0x00, 0x12, 0x34};
    const uint8_t pastMc[6] = {0x91, 0xE0, 0xF0, 0x01, 0x00, 0x01};
    CHECK(cls(kAvdeccMc, false, 0, 0x22F0, 0xFA) == RxVerdict::Avdecc);
    CHECK(cls(kOurMac, false, 0, 0x22F0, 0xFB) == RxVerdict::Avdecc);
    CHECK(cls(inBlock, false, 0, 0x22F0, 0xFC) == RxVerdict::Avdecc);
    CHECK(cls(kAvdeccMc, true, 2, 0x22F0, 0xFC) == RxVerdict::Avdecc);
    CHECK(cls(pastMc, false, 0, 0x22F0, 0xFA) == RxVerdict::DropOther);
    CHECK(cls(kOtherUnicast, false, 0, 0x22F0, 0xFB) == RxVerdict::DropOther);
    const uint8_t maapDst[6] = {0x91, 0xE0, 0xF0, 0x00, 0xFF, 0x00};
    CHECK(cls(maapDst, false, 0, 0x22F0, 0xFE) == RxVerdict::Maap);
    CHECK(cls(kBroadcast, false, 0, 0x22F0, 0xFE) == RxVerdict::DropOther);
    // AVTP without a subtype byte.
    mkFrame(f, kAvdeccMc, false, 0, 0x22F0, 0xFA);
    CHECK(classifyRx(f, 14, kOurMac, flt, false, 0) == RxVerdict::DropOther);
    CHECK(classifyRx(f, 15, kOurMac, flt, false, 0) == RxVerdict::Avdecc);
    mkFrame(f, kAvdeccMc, true, 2, 0x22F0, 0xFA);
    CHECK(classifyRx(f, 18, kOurMac, flt, false, 0) == RxVerdict::DropOther);
    CHECK(classifyRx(f, 19, kOurMac, flt, false, 0) == RxVerdict::Avdecc);
    // Rule 5: streams (AAF subtype 0x02) against the filter.
    CHECK(cls(kStreamA, true, 2, 0x22F0, 0x02) == RxVerdict::Stream);
    CHECK(cls(kStreamB, true, 2, 0x22F0, 0x02) == RxVerdict::Stream);
    CHECK(cls(kStreamA, false, 0, 0x22F0, 0x02) == RxVerdict::DropUntaggedStream);
    CHECK(cls(kStreamA, true, 3, 0x22F0, 0x02) == RxVerdict::DropUnboundStream);
    CHECK(cls(kStreamC, true, 2, 0x22F0, 0x02) == RxVerdict::DropUnboundStream);
    CHECK(cls(kStreamC, false, 0, 0x22F0, 0x02) == RxVerdict::DropUnboundStream);
    // Descriptor VP: tag stripped into the descriptor (U-12).
    const uint16_t tci2 = (3u << 13) | 2u;
    const uint16_t tci3 = (3u << 13) | 3u;
    CHECK(cls(kStreamA, false, 0, 0x22F0, 0x02, true, tci2) == RxVerdict::Stream);
    CHECK(cls(kStreamA, false, 0, 0x22F0, 0x02, true, tci3) == RxVerdict::DropUnboundStream);
    CHECK(cls(kStreamA, false, 0, 0x22F0, 0x02, false, tci2) == RxVerdict::DropUntaggedStream);
    // The in-buffer tag wins over the descriptor field.
    CHECK(cls(kStreamA, true, 2, 0x22F0, 0x02, true, tci3) == RxVerdict::Stream);
    CHECK(cls(kStreamA, true, 3, 0x22F0, 0x02, true, tci2) == RxVerdict::DropUnboundStream);
    // Empty filter: every stream is unbound.
    AvbStreamFilter empty{};
    mkFrame(f, kStreamA, true, 2, 0x22F0, 0x02);
    CHECK(classifyRx(f, 60, kOurMac, empty, false, 0) == RxVerdict::DropUnboundStream);
    // Rule 6: everything else.
    CHECK(cls(kBroadcast, false, 0, 0x0806, 0) == RxVerdict::DropOther);      // ARP
    CHECK(cls(kOurMac, false, 0, 0x0800, 0x45) == RxVerdict::DropOther);      // IPv4
    CHECK(cls(kNearestBridge, false, 0, 0x88CC, 0) == RxVerdict::DropOther);  // LLDP
    // Delivered/drop mapping and counter indices.
    CHECK(rxDelivered(RxVerdict::Gptp) && rxDelivered(RxVerdict::Stream) && rxDelivered(RxVerdict::Maap));
    CHECK(!rxDelivered(RxVerdict::DropOther) && !rxDelivered(RxVerdict::DropUnboundStream) &&
          !rxDelivered(RxVerdict::DropUntaggedStream));
    CHECK(rxVerdictCounter(RxVerdict::Gptp) == 1 && rxVerdictCounter(RxVerdict::Msrp) == 2 &&
          rxVerdictCounter(RxVerdict::Mvrp) == 3 && rxVerdictCounter(RxVerdict::Avdecc) == 4 &&
          rxVerdictCounter(RxVerdict::Maap) == 5 && rxVerdictCounter(RxVerdict::Stream) == 6);
    CHECK(rxVerdictCounter(RxVerdict::DropOther) == kAvbCtrRxDropOther);
    CHECK(rxVerdictCounter(RxVerdict::DropUnboundStream) == kAvbCtrRxDropUnboundStream);
    CHECK(rxVerdictCounter(RxVerdict::DropUntaggedStream) == kAvbCtrRxDropUntaggedStream);
    std::printf("pass: classifyRx (rules 1-6, every drop reason, tagged/untagged, descriptor VP)\n");
}

static void testFrameBounds() {
    RxBounds b = rxFrameBounds(0, true);
    CHECK(b.kind == RxBoundsKind::ShortTs);
    b = rxFrameBounds(15, true);
    CHECK(b.kind == RxBoundsKind::ShortTs);
    b = rxFrameBounds(16, true);
    CHECK(b.kind == RxBoundsKind::Ok && b.flen == 0);
    b = rxFrameBounds(2000, true);
    CHECK(b.kind == RxBoundsKind::Ok && b.flen == 1984);
    b = rxFrameBounds(2001, true);
    CHECK(b.kind == RxBoundsKind::Oversize && b.flen == 1985);
    b = rxFrameBounds(2048, true);
    CHECK(b.kind == RxBoundsKind::Oversize && b.flen == 2032);
    b = rxFrameBounds(1984, false);
    CHECK(b.kind == RxBoundsKind::Ok && b.flen == 1984);
    b = rxFrameBounds(1985, false);
    CHECK(b.kind == RxBoundsKind::Oversize && b.flen == 1985);
    b = rxFrameBounds(0, false);
    CHECK(b.kind == RxBoundsKind::Ok && b.flen == 0);
    std::printf("pass: rxFrameBounds (TSIP short / oversize guards)\n");
}

struct HarvestSim {
    bool discarding = false;
    uint64_t frames = 0;
    uint64_t nonEop = 0;
    RxStep feed(bool eop) {
        const RxStep r = rxHarvestStep(&discarding, eop);
        if (r == RxStep::Frame) {
            frames++;
        } else if (r == RxStep::NonEop) {
            nonEop++;
        }
        return r;
    }
};

static void testHarvestStep() {
    HarvestSim a;
    CHECK(a.feed(true) == RxStep::Frame);
    CHECK(a.frames == 1 && a.nonEop == 0 && !a.discarding);

    HarvestSim b;  // !EOP, !EOP, EOP
    CHECK(b.feed(false) == RxStep::NonEop);
    CHECK(b.discarding);
    CHECK(b.feed(false) == RxStep::Discard);
    CHECK(b.discarding);
    CHECK(b.feed(true) == RxStep::Discard);
    CHECK(!b.discarding);
    CHECK(b.frames == 0 && b.nonEop == 1);

    HarvestSim c;  // !EOP, EOP, EOP
    CHECK(c.feed(false) == RxStep::NonEop);
    CHECK(c.feed(true) == RxStep::Discard);
    CHECK(c.feed(true) == RxStep::Frame);
    CHECK(c.frames == 1 && c.nonEop == 1 && !c.discarding);
    std::printf("pass: rxHarvestStep sequences and counter deltas\n");
}

static void testVlanFlags() {
    uint8_t untagged[64];
    uint8_t tagged[64];
    const uint32_t lu = mkFrame(untagged, kStreamA, false, 0, 0x22F0, 0x02);
    const uint32_t lt = mkFrame(tagged, kStreamA, true, 2, 0x22F0, 0x02);
    CHECK(rxVlanFlags(false, untagged, lu) == 0);
    CHECK(rxVlanFlags(false, tagged, lt) == AVB_RXF_VLAN_FRAME);
    CHECK(rxVlanFlags(true, tagged, lt) == AVB_RXF_VLAN_FRAME);
    CHECK(rxVlanFlags(true, untagged, lu) == AVB_RXF_VLAN_DESC);
    CHECK(rxVlanFlags(true, tagged, 13) == AVB_RXF_VLAN_DESC);  // too short to see the tag
    std::printf("pass: rxVlanFlags (4 VP x in-buffer tag combinations)\n");
}

static void testRingFree() {
    RingSpace s = ringFree(0, 0, AVB_RX_CTL_SLOTS);
    CHECK(s.valid && s.free == 512);
    s = ringFree(100, 0, AVB_RX_CTL_SLOTS);
    CHECK(s.valid && s.free == 412);
    s = ringFree(512, 0, AVB_RX_CTL_SLOTS);
    CHECK(s.valid && s.free == 0);
    s = ringFree(513, 0, AVB_RX_CTL_SLOTS);  // tail too far behind
    CHECK(!s.valid && s.free == 0);
    s = ringFree(5, 6, AVB_RX_CTL_SLOTS);  // tail ahead of head
    CHECK(!s.valid && s.free == 0);
    s = ringFree(5, 5000000, AVB_RX_AUDIO_SLOTS);
    CHECK(!s.valid);
    // Wrap at 2^64.
    s = ringFree(2, UINT64_MAX - 1, AVB_RX_CTL_SLOTS);  // used = 4
    CHECK(s.valid && s.free == 508);
    s = ringFree(0, 0ull - 512, AVB_RX_CTL_SLOTS);
    CHECK(s.valid && s.free == 0);
    s = ringFree(0, 0ull - 513, AVB_RX_CTL_SLOTS);
    CHECK(!s.valid);
    s = ringFree(8191, 0, AVB_RX_AUDIO_SLOTS);
    CHECK(s.valid && s.free == 1);
    std::printf("pass: ringFree (wrap at 2^64, invalid tails)\n");
}

static void testStepTarget() {
    StepResult r = stepTarget(1000000000000000000ull, 500000000, 1000);
    CHECK(r.ok && r.target == 1000000000500001000ull);
    r = stepTarget(5000000000ull, -1000000000, 0);
    CHECK(r.ok && r.target == 4000000000ull);
    r = stepTarget(5000000000ull, 1000000000, 0);
    CHECK(r.ok && r.target == 6000000000ull);
    CHECK(!stepTarget(5000000000ull, 1000000001, 0).ok);
    CHECK(!stepTarget(5000000000ull, -1000000001, 0).ok);
    CHECK(!stepTarget(5000000000ull, INT64_MIN, 0).ok);
    CHECK(!stepTarget(100000000ull, -200000000, 0).ok);  // result < 0
    r = stepTarget(100000000ull, -100000000, 0);
    CHECK(r.ok && r.target == 0);
    CHECK(!stepTarget(kSystimLimitNs - 10, 5, 5).ok);  // result == 2^32 s
    r = stepTarget(kSystimLimitNs - 10, 5, 4);
    CHECK(r.ok && r.target == kSystimLimitNs - 1);
    CHECK(!stepTarget(kSystimLimitNs, 0, 0).ok);
    CHECK(!stepTarget(0, 0, 1000000001ull).ok);  // implausible elapsed
    std::printf("pass: stepTarget\n");
}

static void testStepAmbig() {
    // 24 MHz mach: settle 1 ms = 24 000 ticks; 0.1 ms = 2 400 ticks.
    const uint64_t settle = 24000;
    const uint64_t T = 1000000;
    RxStepAmbig s{};
    CHECK(!rxStepAmbiguous(&s, T));  // no step: never ambiguous
    CHECK(!rxStepAmbiguous(&s, T + 100000));

    rxStepArm(&s, T, settle);
    CHECK(rxStepAmbiguous(&s, T + 4800) && s.pending);    // +0.2 ms
    CHECK(rxStepAmbiguous(&s, T + 16800) && s.pending);   // +0.7 ms
    CHECK(rxStepAmbiguous(&s, T + 28800) && !s.pending);  // +1.2 ms: ambiguous, settles
    CHECK(!rxStepAmbiguous(&s, T + 40800));

    RxStepAmbig d{};  // simulated 5 ms tick delay
    rxStepArm(&d, T, settle);
    CHECK(rxStepAmbiguous(&d, T + 120000) && !d.pending);
    CHECK(!rxStepAmbiguous(&d, T + 132000));

    RxStepAmbig two{};  // second step before settling moves the window
    rxStepArm(&two, T, settle);
    rxStepArm(&two, T + 12000, settle);  // +0.5 ms
    CHECK(two.stepMach == T + 12000);
    CHECK(rxStepAmbiguous(&two, T + 28800) && two.pending);   // +1.2 ms
    CHECK(rxStepAmbiguous(&two, T + 38400) && !two.pending);  // +1.6 ms settles
    CHECK(!rxStepAmbiguous(&two, T + 50400));

    RxStepAmbig ex{};  // tick start exactly at stepMach + settle settles
    rxStepArm(&ex, T, settle);
    CHECK(rxStepAmbiguous(&ex, T + settle) && !ex.pending);
    CHECK(!rxStepAmbiguous(&ex, T + settle + 12000));
    std::printf("pass: rxStepArm / rxStepAmbiguous (late tick, double step, exact boundary)\n");
}

static void testTokenBucket() {
    TokenBucket b{};
    const uint64_t t0 = 1000;
    int okCount = 0;
    for (int i = 0; i < 65; i++) {
        okCount += tokenBucketTake(&b, t0) ? 1 : 0;
    }
    CHECK(okCount == 64);  // burst 64, then empty
    // Refill is expressed in token PERIODS, not a hardcoded wall time, so these
    // describe the bucket's semantics at any AVB_TX_RATE_PER_SEC. Written as
    // "+1 ms = one token" they silently only ever described 1000/s.
    CHECK(tokenBucketTake(&b, t0 + kTxTokenNs));                      // one token
    CHECK(!tokenBucketTake(&b, t0 + kTxTokenNs));
    CHECK(!tokenBucketTake(&b, t0 + kTxTokenNs + kTxTokenNs / 2));    // no whole token yet
    CHECK(tokenBucketTake(&b, t0 + 2 * kTxTokenNs));                  // credit carried over
    CHECK(!tokenBucketTake(&b, t0));            // time going backwards: no refill
    // Long idle refills to the burst, never more.
    okCount = 0;
    for (int i = 0; i < 70; i++) {
        okCount += tokenBucketTake(&b, t0 + 10000000000ull) ? 1 : 0;
    }
    CHECK(okCount == 64);
    // Sustained: an attempt every half token period for 1 s -> burst + rate.
    TokenBucket s{};
    okCount = 0;
    const uint64_t attempts = 2ull * AVB_TX_RATE_PER_SEC;
    for (uint64_t i = 0; i < attempts; i++) {
        okCount += tokenBucketTake(&s, i * (kTxTokenNs / 2)) ? 1 : 0;
    }
    const int sustained = int(AVB_TX_RATE_PER_SEC) + int(AVB_TX_BURST);
    CHECK(okCount >= sustained - 4 && okCount <= sustained);
    // The ceiling must pass an SR class A talker: one frame per 125 us for a
    // full second, none rate-limited. At 1000/s this denied 7000 of 8000 and
    // nothing in the suite noticed -- the stream was silent on real hardware.
    TokenBucket a{};
    int denied = 0;
    for (uint64_t i = 0; i < 8000; i++) {
        denied += tokenBucketTake(&a, i * 125000ull) ? 0 : 1;
    }
    CHECK(denied == 0);
    // And it must pass a FULL-WIDTH talker: all AVB_TX_MAX_CLASS_A_STREAMS
    // streams emitting together, one frame each per 125 us observation
    // interval, for a full second. That is 64 000 frames/s on the 16R, over
    // three times the old 20 000/s ceiling -- so 8-stream playback was capped
    // at 2.5 streams' worth of frames and the rest were silently denied.
    TokenBucket w{};
    int deniedWide = 0;
    for (uint64_t iv = 0; iv < 8000; iv++) {
        for (uint32_t s2 = 0; s2 < AVB_TX_MAX_CLASS_A_STREAMS; s2++) {
            deniedWide += tokenBucketTake(&w, iv * 125000ull) ? 0 : 1;
        }
    }
    CHECK(deniedWide == 0);
    // Control traffic must still get through alongside a full-width talker:
    // gPTP, MSRP, MVRP and AVDECC share this one bucket, and starving them is
    // what stopped Listener Ready from ever arriving at 1000/s.
    TokenBucket c2{};
    int deniedCtl = 0;
    for (uint64_t iv = 0; iv < 8000; iv++) {
        for (uint32_t s2 = 0; s2 < AVB_TX_MAX_CLASS_A_STREAMS; s2++) {
            tokenBucketTake(&c2, iv * 125000ull);
        }
        // ~100 control frames/s: one every 80th interval.
        if (iv % 80 == 0) {
            deniedCtl += tokenBucketTake(&c2, iv * 125000ull) ? 0 : 1;
        }
    }
    CHECK(deniedCtl == 0);
    std::printf("pass: tokenBucketTake (%u/s, burst %u; %d in 1 s; class A 8000/8000; "
                "%u streams wide 0 denied; control 0 denied)\n",
                AVB_TX_RATE_PER_SEC, AVB_TX_BURST, okCount, AVB_TX_MAX_CLASS_A_STREAMS);
}

static void testStatsAccumulate() {
    AvbCounters c{};
    uint32_t last = 7;  // baseline from InitDatapath
    statsAccumulate(&c, &last, HwStatsRead{0, 0, 0, 7});
    CHECK(c.c[kAvbCtrHwCrcErrs] == 0 && c.c[kAvbCtrHwRqdpc0] == 0 && last == 7);
    statsAccumulate(&c, &last, HwStatsRead{3, 4, 5, 17});
    CHECK(c.c[kAvbCtrHwCrcErrs] == 3 && c.c[kAvbCtrHwMpc] == 4 && c.c[kAvbCtrHwRnbc] == 5 &&
          c.c[kAvbCtrHwRqdpc0] == 10 && last == 17);
    statsAccumulate(&c, &last, HwStatsRead{1, 0, 2, 17});  // clear-on-read: add; RQDPC unchanged
    CHECK(c.c[kAvbCtrHwCrcErrs] == 4 && c.c[kAvbCtrHwMpc] == 4 && c.c[kAvbCtrHwRnbc] == 7 &&
          c.c[kAvbCtrHwRqdpc0] == 10);
    last = 0xFFFFFFF0u;  // RQDPC wraps at 2^32
    statsAccumulate(&c, &last, HwStatsRead{0, 0, 0, 0x10});
    CHECK(c.c[kAvbCtrHwRqdpc0] == 10 + 0x20 && last == 0x10);
    for (uint32_t i = 0; i < kAvbCtrCount; i++) {  // nothing else touched
        if (i != kAvbCtrHwCrcErrs && i != kAvbCtrHwMpc && i != kAvbCtrHwRnbc && i != kAvbCtrHwRqdpc0) {
            CHECK(c.c[i] == 0);
        }
    }
    std::printf("pass: statistics accumulation (clear-on-read + RQDPC wrap)\n");
}

static void testStreamFilterValid() {
    AvbStreamFilter f = testFilter();
    CHECK(streamFilterValid(f));
    AvbStreamFilter empty{};
    CHECK(streamFilterValid(empty));
    AvbStreamFilter full{};
    full.count = AVB_MAX_STREAMS;
    for (uint32_t i = 0; i < AVB_MAX_STREAMS; i++) {
        std::memcpy(full.e[i].dest_mac, kStreamA, 6);
        full.e[i].dest_mac[5] = static_cast<uint8_t>(i);
        full.e[i].vid = static_cast<uint16_t>(1 + i);
    }
    CHECK(streamFilterValid(full));
    full.count = AVB_MAX_STREAMS + 1;
    CHECK(!streamFilterValid(full));
    AvbStreamFilter u = testFilter();
    std::memcpy(u.e[1].dest_mac, kOtherUnicast, 6);  // unicast
    CHECK(!streamFilterValid(u));
    AvbStreamFilter v0 = testFilter();
    v0.e[0].vid = 0;
    CHECK(!streamFilterValid(v0));
    AvbStreamFilter v4095 = testFilter();
    v4095.e[1].vid = 4095;
    CHECK(!streamFilterValid(v4095));
    AvbStreamFilter edge = testFilter();
    edge.e[0].vid = 1;
    edge.e[1].vid = 4094;
    CHECK(streamFilterValid(edge));
    AvbStreamFilter beyond = testFilter();  // entries past count are ignored
    beyond.e[5].vid = 0;
    CHECK(streamFilterValid(beyond));
    std::printf("pass: stream filter validation (unicast, VID 0/4095, count > 16 rejected)\n");
}

static void testAvdeccDest() {
    // Same boundary table as avb_core::is_avdecc_dest.
    const uint8_t acc[][6] = {
        {0x91, 0xE0, 0xF0, 0x00, 0x00, 0x00},
        {0x91, 0xE0, 0xF0, 0x00, 0xFF, 0xFF},
        {0x91, 0xE0, 0xF0, 0x01, 0x00, 0x00},
        {0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8},  // our MAC
    };
    const uint8_t rej[][6] = {
        {0x91, 0xE0, 0xF0, 0x01, 0x00, 0x01},
        {0x91, 0xE0, 0xF1, 0x00, 0x00, 0x00},
        {0x91, 0xE0, 0xEF, 0xFF, 0xFF, 0xFF},
    };
    for (const auto& m : acc) {
        CHECK(isAvdeccDest(m, kOurMac));
    }
    for (const auto& m : rej) {
        CHECK(!isAvdeccDest(m, kOurMac));
    }
    CHECK(!isAvdeccDest(kOtherUnicast, kOurMac));
    CHECK(!isAvdeccDest(kBroadcast, kOurMac));
    std::printf("pass: isAvdeccDest boundary table\n");
}

static void testProducerRunning() {
    const uint32_t stages[] = {kAvbStageNone, kAvbStageTimeSync, kAvbStageDatapath};
    const uint32_t faults[] = {kAvbFaultNone, kAvbFaultDma};
    for (int attached = 0; attached <= 1; attached++) {
        for (uint32_t st : stages) {
            for (uint32_t fl : faults) {
                const bool want = attached == 1 && st == kAvbStageDatapath && fl == kAvbFaultNone;
                CHECK(producerRunning(attached == 1, st, fl) == want);
            }
        }
    }
    std::printf("pass: producerRunning truth table\n");
}

int main() {
    testIdentity();
    testTiminca();
    testSystim();
    testTxDesc();
    testRxDesc();
    testTsHeaderAndFrame();
    testComposites();
    testMdicAndLink();
    testReadable();
    testRing();
    testClassify();
    testFrameBounds();
    testHarvestStep();
    testVlanFlags();
    testRingFree();
    testStepTarget();
    testStepAmbig();
    testTokenBucket();
    testStatsAccumulate();
    testStreamFilterValid();
    testAvdeccDest();
    testProducerRunning();
    std::printf("all i210 tests passed (%d checks)\n", gChecks);
    return 0;
}
