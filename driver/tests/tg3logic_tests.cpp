// SPDX-License-Identifier: MIT OR Apache-2.0

// Host-side unit tests for Tg3Logic.hpp and the AVBShared.h ABI.
// Plain assert-style runner: prints one line per check group, exits non-zero
// on the first failure.

#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <cstring>

#include "../AVBShared.h"
#include "../AVBBcmDriver/Tg3Logic.hpp"

using namespace tg3logic;

static int gChecks = 0;

#define CHECK(cond)                                                              \
    do {                                                                         \
        ++gChecks;                                                               \
        if (!(cond)) {                                                           \
            std::fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond); \
            std::exit(1);                                                        \
        }                                                                        \
    } while (0)

// ABI sizes and offsets (mirrored by crates/avb-probe/src/abi.rs tests).
static_assert(sizeof(AvbInfo) == 60);
static_assert(offsetof(AvbInfo, nic_kind) == 52);
static_assert(offsetof(AvbInfo, clock_ctl) == 56);
static_assert(AVB_ABI_VERSION == 3);
static_assert(kAvbSelBaseCount == 10);  // the BCM dext implements selectors 0..9 only
static_assert(sizeof(AvbClockSample) == 24);
static_assert(sizeof(AvbRxEntry) == 152);
static_assert(sizeof(AvbRxBatch) == 3656);
static_assert(offsetof(AvbInfo, mac) == 36);
static_assert(offsetof(AvbInfo, tb_numer) == 44);
static_assert(offsetof(AvbRxEntry, len) == 16);
static_assert(offsetof(AvbRxEntry, type_flags) == 20);
static_assert(offsetof(AvbRxEntry, has_ts) == 22);
static_assert(offsetof(AvbRxEntry, data) == 24);
static_assert(offsetof(AvbRxBatch, e) == 8);
static_assert(kAvbSelShutdownDatapath == 9);

// Shared golden table (identical values in crates/avb-probe/src/freq.rs).
struct CorrCase {
    int64_t ppb;
    uint32_t reg;
};
static const CorrCase kCorrCases[] = {
    {0, 0x00000000},        {1, 0x00000000},        {-1, 0x00000000},
    {60, 0x80000001},       {-60, 0xC0000001},      {100000, 0x8000068E},
    {-100000, 0xC000068E},  {1000000, 0x80004189},  {-1000000, 0xC0004189},
};

static void testCorrection() {
    for (const auto& c : kCorrCases) {
        CHECK(eavCorrectionFromPpb(c.ppb) == c.reg);
    }
    // NEG only for negative inputs, EN whenever magnitude != 0.
    CHECK((eavCorrectionFromPpb(-100000) & bcm::kEavCorrectNegative) != 0);
    CHECK((eavCorrectionFromPpb(100000) & bcm::kEavCorrectNegative) == 0);
    CHECK((eavCorrectionFromPpb(100000) & bcm::kEavCorrectEnable) != 0);
    // Round trip within one LSB (1e9 / 2^24 ~= 59.6 ppb).
    const int64_t probes[] = {0, 1, -1, 100000, -100000, 1000000, -1000000, 12345, -54321};
    for (int64_t p : probes) {
        const int64_t back = eavPpbFromCorrection(eavCorrectionFromPpb(p));
        const int64_t err = back > p ? back - p : p - back;
        CHECK(err <= 30);
    }
    CHECK(eavPpbFromCorrection(0x80004189) == 999987);  // 16777 * 1e9 / 2^24 = 999,987.1
    std::printf("pass: EAV correction encoding (%zu golden cases)\n",
                sizeof(kCorrCases) / sizeof(kCorrCases[0]));
}

static void testTxBd() {
    CHECK(txLenFlags(60, false) == ((60u << 16) | 0x0004u));
    CHECK(txLenFlags(68, true) == ((68u << 16) | 0x0004u | 0x0020u));
    CHECK(txLenFlags(1514, false) == 0x05EA0004u);
    const TxBd bd = makeTxBd(0x0000001234567800ull, 68, true);
    CHECK(bd.addrHi == 0x12);
    CHECK(bd.addrLo == 0x34567800);
    CHECK(bd.lenFlags == ((68u << 16) | 0x24u));
    CHECK(bd.vlanTag == 0);
    std::printf("pass: TX BD encode\n");
}

static void testRxBd() {
    const RxBd post = makeStdRxBd(0xABCD00001000ull, 7, bcm::kRxStdDmaSize);
    CHECK(post.addrHi == 0xABCD);
    CHECK(post.addrLo == 0x1000);
    CHECK(post.idxLen == 1536);
    CHECK(post.typeFlags == 0x0004);
    CHECK(post.opaque == (0x00010000u | 7u));

    RxBd ret{};
    ret.idxLen = (3u << 16) | 72u;      // 68-byte frame + FCS
    ret.typeFlags = 0x0004u | 0x0200u;  // END | PTPSTAT_V2
    ret.opaque = 0x00010000u | 42u;
    RxReturn r = decodeRxReturn(ret);
    CHECK(r.len == 68);
    CHECK(r.stdIndex == 42);
    CHECK(r.fromStdRing);
    CHECK(!r.error);
    CHECK(r.hasTimestamp);
    CHECK(r.ptp == PtpStat::V2);
    CHECK(r.flags == 0x0204);

    ret.typeFlags = 0x0004u | 0x0010u;  // PTPSTAT_V1
    CHECK(decodeRxReturn(ret).ptp == PtpStat::V1);
    ret.typeFlags = 0x0004u | 0x0210u;  // both bits: not a valid latch state
    CHECK(decodeRxReturn(ret).ptp == PtpStat::Unknown);
    CHECK(!decodeRxReturn(ret).hasTimestamp);
    ret.typeFlags = 0x0004u;
    CHECK(!decodeRxReturn(ret).hasTimestamp);

    ret.errVlan = 0x00010000u;  // BAD_CRC
    CHECK(decodeRxReturn(ret).error);
    ret.errVlan = 0x00100000u;  // odd nibble alone is not an error
    CHECK(!decodeRxReturn(ret).error);
    ret.errVlan = 0x00000002u;  // VLAN tag bits only
    CHECK(!decodeRxReturn(ret).error);

    ret.idxLen = 2;  // runt shorter than FCS
    CHECK(decodeRxReturn(ret).len == 0);
    ret.opaque = 0x00020000u | 1u;  // jumbo ring
    CHECK(!decodeRxReturn(ret).fromStdRing);
    std::printf("pass: RX BD encode/decode\n");
}

static void testRing() {
    CHECK(isPow2(bcm::kStdRingSize) && isPow2(bcm::kRxRetRingSize) && isPow2(bcm::kTxRingSize));
    CHECK(!isPow2(0) && !isPow2(100));
    CHECK(ringNext(0, 512) == 1);
    CHECK(ringNext(511, 512) == 0);
    CHECK(ringNext(1023, 1024) == 0);
    CHECK(ringAdd(510, 4, 512) == 2);
    CHECK(ringDistance(5, 5, 512) == 0);
    CHECK(ringDistance(510, 2, 512) == 4);
    CHECK(ringDistance(2, 510, 512) == 508);
    std::printf("pass: ring index wrap\n");
}

static void testMisc() {
    CHECK(miReadFrame(1, 0x01) == (0x00200000u | 0x00010000u | 0x08000000u | 0x20000000u));
    CHECK(miWriteFrame(1, 0x00, 0x1200) ==
          (0x00200000u | 0x1200u | 0x04000000u | 0x20000000u));
    CHECK(linkFromAuxStat(0x0700) == LinkSpeed::G1Full);
    CHECK(linkFromAuxStat(0x0500) == LinkSpeed::M100Full);
    CHECK(linkFromAuxStat(0x0100) == LinkSpeed::Unknown);
    CHECK(macModeForLink(0x00e0480c, LinkSpeed::G1Full) == 0x00e04808);
    CHECK(macModeForLink(0x00e04808, LinkSpeed::M100Half) == 0x00e04806);
    CHECK(isOurDevice(0x168214e4, 0x00f6106b));
    CHECK(!isOurDevice(0x15338086, 0x02141ab6));  // Intel I210 on en7: never ours
    CHECK(!isOurDevice(0x168214e4, 0x00000000));
    CHECK(isAsic57766(0x57766001));
    CHECK(!isAsic57766(0x57785000));
    CHECK(isReadableRegOffset(0x6900));
    CHECK(!isReadableRegOffset(0x6901));
    CHECK(!isReadableRegOffset(0x10000));
    CHECK(!isReadableRegOffset(0x1fffc));  // inside the shared limit, outside the BCM window
    const uint8_t sb[0x14] = {1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x34, 0x12, 0, 0, 0, 0, 0x05, 0x00, 0xff, 0x01};
    CHECK(le16At(sb, bcm::kSbRxStdConsumer) == 0x1234);
    CHECK(le16At(sb, bcm::kSbRxProducer0) == 5);
    CHECK(le16At(sb, bcm::kSbTxConsumer0) == 0x01ff);
    CHECK(stdReplenishThreshold(64) == 8);
    CHECK(stdReplenishThreshold(4) == 1);
    CHECK(stdReplenishThreshold(1000) == 64);
    CHECK(join64(0x1, 0x2) == 0x0000000100000002ull);
    std::printf("pass: MDIO / link / identity / status-block helpers\n");
}

static void testFrameAndComposites() {
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
    CHECK(frameEthertype(f, 17) == 0);

    CHECK(stdRingMaxLenFlags() == ((512u << 16) | (1536u << 2)));
    CHECK(sendRcbMaxLenFlags() == (512u << 16));
    CHECK(rxRetRcbMaxLenFlags() == (1024u << 16));
    CHECK(rdmacModeValue() == 0x188303FEu);
    CHECK(wdmacModeValue() == 0x200003FEu);
    CHECK(grcModeDatapath() == 0x041B0034u);
    CHECK((grcModeDatapath() & bcm::kGrcNoRxPhdrCsum) == 0);
    CHECK(macModeDatapath() == 0x00E04800u);
    CHECK(jumboReplenishThreshold(bcm::kJumboPendingDefault) == 12);
    CHECK(jumboReplenishThreshold(1000) == 32);
    CHECK(bcm::kMacRxMtuValue == 1522);
    std::printf("pass: ethertype / Stage B composite register values\n");
}

int main() {
    testCorrection();
    testTxBd();
    testRxBd();
    testRing();
    testMisc();
    testFrameAndComposites();
    std::printf("all tests passed (%d checks)\n", gChecks);
    return 0;
}
