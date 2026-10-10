// Host-side unit tests for I210Logic.hpp and the AVBShared.h ABI (v2).
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

// ABI v2 sizes and offsets (mirrored by crates/avb-probe/src/abi.rs tests).
static_assert(AVB_ABI_VERSION == 2);
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
    std::printf("all i210 tests passed (%d checks)\n", gChecks);
    return 0;
}
