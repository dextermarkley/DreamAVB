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
static_assert(AVB_ABI_VERSION == 5);
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

// The Apple-state snapshot covers exactly the listed windows, contiguously
// indexed, and nothing in the MAC statistics block (0x800+) that may clear
// on read.
static void testSnapshot() {
    CHECK(kSnapWords == (0x300u + 0x200u + 0x100u * 6u + 0x700u + 0x200u) / 4u);
    CHECK(snapIndex(0x0400) == 0);
    CHECK(snapIndex(0x06fc) == 0x300 / 4 - 1);
    CHECK(snapIndex(0x2000) == (0x300u + 0x200u) / 4u);  // 0x0c00-0x0e00 now precedes
    CHECK(snapIndex(0x0d20) >= 0);  // realtime-queue ring base
    CHECK(snapIndex(0x3c08) >= 0);  // HOSTCC RX coalescing (AVB tuning)
    CHECK(snapIndex(0x06c8) >= 0);  // RX_PTP_CTL
    CHECK(snapIndex(0x06b0) >= 0);  // RX timestamp LSB
    CHECK(snapIndex(0x05c0) >= 0);  // TX timestamp LSB
    CHECK(snapIndex(0x6800) >= 0);  // GRC_MODE
    CHECK(snapIndex(0x6928) >= 0);  // EAV correction
    CHECK(snapIndex(0x5494) >= 0);  // Apple RX-rule ethertype 0x88F7
    CHECK(snapIndex(0x5410) >= 0);  // Apple RX-rule element
    CHECK(snapIndex(0x4d00) == -1); // gap between windows still excluded
    CHECK(snapIndex(0x69fc) == static_cast<int32_t>(kSnapWords) - 1);
    CHECK(snapIndex(0x0800) == -1);  // MAC statistics: never captured
    CHECK(snapIndex(0x0700) == -1);
    CHECK(snapIndex(0x6a00) == -1);
    CHECK(snapIndex(0x06c9) == -1);  // unaligned
    CHECK(snapIndex(0x0000) == -1);
    // Indices are unique and dense.
    int32_t last = -1;
    for (const auto& w : kSnapWindows) {
        for (uint32_t off = w.start; off < w.end; off += 4) {
            CHECK(snapIndex(off) == last + 1);
            last = snapIndex(off);
        }
    }
    CHECK((kReadSnapshotFlag & 0xffffffffull) == 0);
    std::printf("pass: Apple-state snapshot windows\n");
}

// The AVB RX classifier replay is the exact Apple sequence: it includes the
// gPTP ethertype element, preserves the duplicated indexed-port writes, and
// stays within the snapshot's 0x5000-0x5700 window (so a capture sees it).
static void testAvbRxRules() {
    CHECK(kAvbRxRuleWriteCount == 22);
    // The gPTP ethertype match (0x88F7) must be present at 0x5494.
    bool ethertype = false;
    int dup5494 = 0;
    for (uint32_t i = 0; i < kAvbRxRuleWriteCount; i++) {
        const auto& w = kAvbRxRuleWrites[i];
        if (w.off == 0x5494 && w.val == 0x88f7ffff) {
            ethertype = true;
        }
        if (w.off == 0x5494) {
            dup5494++;
        }
        // Every offset is captured by the snapshot, so a dump shows the result.
        CHECK(snapIndex(w.off) >= 0);
    }
    CHECK(ethertype);
    CHECK(dup5494 == 2);  // indexed port written twice, order preserved
    // First four are the rule-bank header from updateAVBStreamFilters.
    CHECK(kAvbRxRuleWrites[0].off == 0x5400 && kAvbRxRuleWrites[0].val == 0x8400e000);
    std::printf("pass: AVB RX classifier replay sequence\n");
}

static void testRxCpuStamp() {
    // Slot decode: word1 = (messageType << 16) | sequenceId.
    const RxCpuStamp d = decodeRxCpuStamp(0x12345678u, (0x2u << 16) | 0xabcdu);
    CHECK(d.eavLo == 0x12345678u && d.msgType == 0x2 && d.seq == 0xabcd);
    // Only the low nibble of word1's high half is the messageType.
    CHECK(decodeRxCpuStamp(0, 0x00ff0001u).msgType == 0x0f);

    // gPTP key parse: a Pdelay_Req (messageType 2) with sequenceId 0x1234.
    uint8_t frame[64] = {0};
    frame[12] = 0x88;
    frame[13] = 0xf7;        // ethertype 0x88F7
    frame[14] = 0x12;        // transportSpecific=1, messageType=2
    frame[44] = 0x12;        // sequenceId high
    frame[45] = 0x34;        // sequenceId low
    uint8_t mt = 0xff;
    uint16_t seq = 0;
    CHECK(parseGptpStampKey(frame, sizeof(frame), &mt, &seq));
    CHECK(mt == 0x2 && seq == 0x1234);
    // Rejects: wrong ethertype, and too-short frames.
    frame[13] = 0x00;
    CHECK(!parseGptpStampKey(frame, sizeof(frame), &mt, &seq));
    frame[13] = 0xf7;
    CHECK(!parseGptpStampKey(frame, 45, &mt, &seq));
    CHECK(!parseGptpStampKey(nullptr, 64, &mt, &seq));

    // Match newest-first: entries[0] is the most recent stamp. Pick the first
    // (msgType, seq) match and return its EAV low word.
    const RxCpuStamp ring[4] = {
        {0x0000000a, 0x0, 0x0005},  // Sync, newest
        {0x0000000b, 0x2, 0x0004},  // Pdelay_Req
        {0x0000000c, 0x2, 0x0003},
        {0x0000000d, 0x0, 0x0002},
    };
    uint32_t eav = 0;
    CHECK(matchRxCpuStamp(ring, 4, 0x2, 0x0004, &eav) && eav == 0x0000000b);
    CHECK(matchRxCpuStamp(ring, 4, 0x0, 0x0002, &eav) && eav == 0x0000000d);
    // No match in the window, empty ring, and null are all false.
    CHECK(!matchRxCpuStamp(ring, 4, 0x2, 0x0099, &eav));
    CHECK(!matchRxCpuStamp(ring, 0, 0x0, 0x0005, &eav));
    CHECK(!matchRxCpuStamp(nullptr, 4, 0x0, 0x0005, &eav));
    // On a sequenceId collision across messageTypes, messageType disambiguates.
    const RxCpuStamp collide[2] = {{0x111, 0x2, 0x0007}, {0x222, 0x0, 0x0007}};
    CHECK(matchRxCpuStamp(collide, 2, 0x0, 0x0007, &eav) && eav == 0x222);
    std::printf("pass: RX-CPU stamp ring decode / key parse / match\n");

    // Event vs general messages: only 0-7 need an RX timestamp.
    CHECK(isPtpEventMsg(0x0) && isPtpEventMsg(0x2) && isPtpEventMsg(0x3) && isPtpEventMsg(0x7));
    CHECK(!isPtpEventMsg(0x8) && !isPtpEventMsg(0xa) && !isPtpEventMsg(0xb) && !isPtpEventMsg(0xf));

    // Incremental read start: only new entries, bounded to the window, and a
    // restarted (smaller) count re-reads the window.
    CHECK(stampReadStart(0, 0, 16) == 0);
    CHECK(stampReadStart(0, 5, 16) == 0);
    CHECK(stampReadStart(5, 7, 16) == 5);
    CHECK(stampReadStart(7, 7, 16) == 7);    // nothing new
    CHECK(stampReadStart(10, 100, 16) == 84); // fell behind: newest 16 only
    CHECK(stampReadStart(100, 3, 16) == 0);  // firmware reloaded
    CHECK(stampReadStart(100, 40, 16) == 24);

    // Cache push: newest first, capped at kRxCpuStampCacheSize.
    RxCpuStampCache c{};
    for (uint32_t i = 0; i < kRxCpuStampCacheSize + 5; i++) {
        stampCachePush(c, RxCpuStamp{i, 0x0, static_cast<uint16_t>(i)});
    }
    CHECK(c.count == kRxCpuStampCacheSize);
    CHECK(c.e[0].seq == kRxCpuStampCacheSize + 4);
    CHECK(c.e[kRxCpuStampCacheSize - 1].seq == 5);
    CHECK(matchRxCpuStamp(c.e, c.count, 0x0, 10, &eav) && eav == 10);
    CHECK(!matchRxCpuStamp(c.e, c.count, 0x0, 4, &eav));  // aged out
    RxCpuStampCache one{};
    stampCachePush(one, RxCpuStamp{0x77, 0x3, 0x0042});
    CHECK(one.count == 1 && matchRxCpuStamp(one.e, one.count, 0x3, 0x0042, &eav) && eav == 0x77);
    std::printf("pass: RX-CPU stamp cache (event filter, incremental read start, push/age-out)\n");
}

static void testReconstructEavTs() {
    // No wrap: frame low behind the current low -> same high word.
    CHECK(reconstructEavTs(25, 0x0000f000, 0x0000e000) == (25ull << 32 | 0x0000e000));
    // Frame low AT the current low counts as "before the latest wrap" (borrow),
    // matching the <= boundary the hardware path used.
    CHECK(reconstructEavTs(25, 0x0000e000, 0x0000e000) == (24ull << 32 | 0x0000e000));
    // Wrap: current low has wrapped past the frame low -> borrow one.
    CHECK(reconstructEavTs(26, 0x00001000, 0xfffff000) == (25ull << 32 | 0xfffff000));
    // t1/t4 alignment: a TX latch at hi=25 and an RX frame ~27 us later (no
    // wrap) land in the same epoch, so (t4 - t1) is a small positive delta.
    const uint64_t t1 = 25ull << 32 | 0x5f5e1000;             // TX low
    const uint64_t t4 = reconstructEavTs(25, 0x5f5e8000, 0x5f5e6cf0);  // RX, ~23 us later
    CHECK(t4 > t1 && (t4 - t1) < 100000);  // sub-100 us round-trip-ish
    std::printf("pass: EAV 64-bit timestamp reconstruction\n");
}

static void testTxPacePlan() {
    // The measured case: 8 streams -> 2 x 4, half an interval apart.
    constexpr TxPacePlan p8 = txPacePlan(8, kTxBurstMax, kClassAIntervalNs);
    CHECK(p8.parts == 2 && p8.chunk == 4 && p8.stepNs == 62500);
    // At or under the cap: one doorbell, no deferral.
    CHECK(txPacePlan(4, 4, 125000).parts == 1 && txPacePlan(4, 4, 125000).stepNs == 0);
    CHECK(txPacePlan(1, 4, 125000).parts == 1 && txPacePlan(1, 4, 125000).chunk == 1);
    // Uneven: 5 -> 3 + 2, never more than the cap per sub-burst.
    CHECK(txPacePlan(5, 4, 125000).parts == 2 && txPacePlan(5, 4, 125000).chunk == 3);
    // The batch maximum: 16 -> 4 x 4, 31.25 us apart.
    constexpr TxPacePlan p16 = txPacePlan(AVB_TX_BATCH_MAX, kTxBurstMax, kClassAIntervalNs);
    CHECK(p16.parts == 4 && p16.chunk == 4 && p16.stepNs == 31250);
    // Every frame is covered and the last sub-burst starts inside the interval.
    for (uint32_t n = 1; n <= AVB_TX_BATCH_MAX; n++) {
        const TxPacePlan p = txPacePlan(n, kTxBurstMax, kClassAIntervalNs);
        CHECK(p.chunk <= kTxBurstMax && p.parts * p.chunk >= n && (p.parts - 1) * p.chunk < n);
        CHECK((p.parts - 1) * p.stepNs < kClassAIntervalNs);
    }
    CHECK(txPacePlan(0, 4, 125000).parts == 0);
    CHECK(txPacePlan(7, 0, 125000).parts == 1);  // cap 0 = no pacing
    std::printf("pass: TX pacing plan\n");
}

int main() {
    testTxPacePlan();
    testSnapshot();
    testAvbRxRules();
    testRxCpuStamp();
    testReconstructEavTs();
    testCorrection();
    testTxBd();
    testRxBd();
    testRing();
    testMisc();
    testFrameAndComposites();
    std::printf("all tests passed (%d checks)\n", gChecks);
    return 0;
}
