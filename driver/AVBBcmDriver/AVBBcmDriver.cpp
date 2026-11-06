// SPDX-License-Identifier: MIT OR Apache-2.0

// AVBBcmDriver.cpp - Phase 0 PCIDriverKit driver for the BCM57762 (57766-class)
// in the Apple Thunderbolt Gigabit Ethernet adapter.
//
// Register sequences follow the observable behaviour of the Linux tg3 driver
// for an ASIC_REV_57766 / 57765_PLUS / 57765_CLASS / PCIe / CPMU part. No tg3
// code is copied; see Bcm57762Regs.h for the constants and Tg3Logic.hpp for the
// pure helpers (both host-tested in driver/tests).
//
// Safety rules (plan section 0):
//   * only 14e4:1682 with subsystem 106b:00f6 is ever touched (personality,
//     entitlement and the Start() check below);
//   * every poll is bounded; a timeout or an all-ones read latches ivars->fault
//     and all further MMIO is refused until Stop;
//   * there is no generic register-write path: every write belongs to a named
//     sequence (Stage A, SetClock, AdjFreq, Stage B init/shutdown, TX doorbell).
//
// Bring-up has two stages:
//   Stage A (Start): host control + MEMARB enable + GRC_MODE |= TIME_SYNC_ENABLE.
//                    No chip reset, no DMA.
//   Stage B (kAvbSelInitDatapath): core reset, one std RX ring, one RX return
//                    ring, one TX ring, RX PTP filter, polled by a 1 ms timer.

#include <os/log.h>

#include <DriverKit/IOLib.h>
#include <DriverKit/IOUserServer.h>
#include <DriverKit/IOUserClient.h>
#include <DriverKit/IOBufferMemoryDescriptor.h>
#include <DriverKit/IODMACommand.h>
#include <DriverKit/IODispatchQueue.h>
#include <DriverKit/IOTimerDispatchSource.h>
#include <DriverKit/OSAction.h>
#include <DriverKit/OSData.h>
#include <PCIDriverKit/PCIDriverKit.h>

#include "AVBBcmDriver.h"
#include "AVBBcmUserClient.h"
#include "../AVBShared.h"
#include "../AVBRxCommon.hpp"
#include "Bcm57762Regs.h"
#include "Tg3Logic.hpp"

#define LOG(fmt, ...) os_log(OS_LOG_DEFAULT, "AVBBcm: " fmt, ##__VA_ARGS__)

using namespace tg3logic;
using namespace avbrx;

namespace {

// 256 posted buffers hold ~16 ms of one 8 kpkt/s AAF stream, or ~2 ms of all
// eight 16R streams, against a 1 ms harvest tick. The std ring holds 512 and
// the return ring 1024, so both still exceed what is posted.
constexpr uint32_t kRxPosted      = 256;    // std RX buffers posted
constexpr uint32_t kRxBufSize     = 2048;   // per RX buffer (DMA size 1536)
// 64 in flight holds four full 16-frame batches while the status-block TX
// consumer catches up; the 512-entry TX ring is a multiple of it.
constexpr uint32_t kTxSlots       = 64;     // TX bounce buffers / max in flight
// slot = txProd % kTxSlots never aliases an in-flight BD only if the ring is
// a multiple of the window.
static_assert(bcm::kTxRingSize % kTxSlots == 0, "TX ring is a multiple of the in-flight window");
constexpr uint32_t kTxBufSize     = 2048;
constexpr uint32_t kSwRingSize    = 256;    // software RX ring (power of two)
constexpr uint64_t kTimerPeriodNs = 1000000;      // 1 ms
static_assert(kRxPosted < bcm::kStdRingSize && kRxPosted < bcm::kRxRetRingSize, "posted fits the rings");
constexpr uint64_t kTxTsTimeoutNs = 300000000;    // 300 ms (tg3: 3 x 100 ms)
constexpr uint64_t kLinkWaitMs    = 5000;

struct DmaBuf {
    IOBufferMemoryDescriptor * md;
    IODMACommand * cmd;
    uint8_t * va;
    uint64_t iova;
    uint64_t len;
};

}  // namespace

struct AVBBcmDriver_IVars {
    IOPCIDevice * pci;
    bool opened;
    uint8_t bar;                 // memory index of BAR0
    uint64_t pcieCap;            // PCIe capability offset (0 if absent)

    uint32_t vendorDevice;
    uint32_t subsys;
    uint32_t asicRev;
    uint32_t stage;              // AvbStage
    uint32_t fault;              // AvbFault, latched
    uint32_t flags;              // AVB_FLAG_*
    uint8_t mac[6];
    // Register state Apple's driver left, read at Start before any write
    // (see kSnapWindows). Read back with kReadSnapshotFlag.
    uint32_t snap[kSnapWords];
    bool snapValid;
    uint32_t snapMiscHostCtrl;   // config-space MISC_HOST_CTRL before Stage A
    struct mach_timebase_info tb;

    IODispatchQueue * queue;     // default queue (timer + user client calls)
    // SendFrame/SendFrames run on their own queue so a talker's batch never
    // waits behind the 1 ms harvest (measured: harvest up to 2.2 ms, while
    // SendFrames itself takes <= 27 us). txLock serialises everything the two
    // queues share: the TX ring and doorbell, the TX-timestamp latch state,
    // the EAV high word, datapath init/shutdown, and every config-window
    // access (so no doorbell can land inside or just before one).
    IODispatchQueue * txQueue;
    IOLock * txLock;
    // Sends waiting for txLock (atomic). A mutex does not hand off in arrival
    // order, so a multi-word stamp refresh re-took the lock between words and
    // a waiting SendFrames sat out the whole refresh (measured: 680-1300 us
    // from the daemon while the dext body took <= 42 us). The harvest yields
    // to a waiting send before each window word instead.
    uint32_t txWaiting;
    IOTimerDispatchSource * timer;
    OSAction * timerAction;
    bool timerRunning;

    // Datapath (Stage B)
    DmaBuf status;
    DmaBuf stdRing;
    DmaBuf retRing;
    DmaBuf retRing1;             // AVB realtime return ring (classifier routes gPTP here)
    DmaBuf txRing;
    DmaBuf rxBufs;
    DmaBuf txBufs;
    uint32_t stdProd;            // std ring producer (ring index)
    uint32_t retCons;            // RX return ring 0 consumer
    uint32_t retCons1;           // RX return ring 1 (AVB) consumer
    uint32_t txProd;             // TX producer
    uint32_t txCons;             // last TX consumer seen in the status block
    uint64_t txSeq;              // frames sent since datapath up
    uint64_t lastDoorbellMach;   // mach time of the last TX producer-mailbox write

    // One outstanding timestamped TX (tg3 behaviour).
    bool tsPending;
    uint64_t tsSeq;
    uint64_t tsPre;              // TX_TSTAMP before the doorbell
    uint64_t tsDeadline;         // mach ticks
    uint32_t tsStatus;           // AvbTxTsStatus for tsSeq
    uint64_t tsValue;
    uint64_t tsMach;

    // Software-tracked EAV high word. EAV_REF_CLCK_MSB reads 0 on this
    // 57766-class part, so a 64-bit RX timestamp cannot be built from it. We
    // advance eavHi on each EAV_LSB wrap and re-align it to the TX_TSTAMP latch
    // epoch (the only register that exposes the true high 32 bits) whenever a
    // TX timestamp latches, so RX (t4) and TX (t1) share one 64-bit epoch.
    uint32_t eavHi;
    uint32_t eavLastLo;
    bool eavSeeded;

    // RX-CPU stamping (AVB_INIT_FLAG_RXCPU_STAMP): attach the firmware's
    // per-frame EAV timestamp to harvested gPTP frames.
    bool rxcpuStamp;
    // The firmware's DONE signature, once seen (reset on init and on a core
    // load), and the host mirror of its stamp ring.
    bool stampDone;
    RxCpuStampCache stampCache;

    // Software RX ring filled by the harvester.
    AvbRxEntry * sw;
    uint32_t swHead;             // next write
    uint32_t swTail;             // next read
    uint32_t swDropped;

    AvbRxBatch * batch;          // scratch for kAvbSelPollRx

    // ABI v3 shared RX rings, same layout as the I210 dext (AVBShared.h).
    // While attached and the datapath is up, the harvester classifies each
    // frame (avbrx::classifyRx) into the control or audio ring instead of the
    // software ring, and the daemon reads them zero-copy.
    AVBBcmUserClient * owner;    // not retained; cleared by RingDetach/ClientGone
    IOBufferMemoryDescriptor * rxMem;
    IOBufferMemoryDescriptor * consumerMem;
    uint8_t * rxVa;
    AvbShmHeader * hdr;          // == rxVa while attached, else nullptr
    AvbShmConsumer * cons;
    uint32_t generation;         // RingAttach count since Start
    uint64_t ctlHead;
    uint64_t audioHead;
    AvbStreamFilter filter;
    bool producerRunning;
    AvbCounters ctr;
};

using IV = AVBBcmDriver_IVars;

// ---------------------------------------------------------------------------
// Hardware access. All of it funnels through these helpers so the fault latch
// is enforced in one place.

static void setFault(IV * v, uint32_t code, const char * why)
{
    if (v->fault == kAvbFaultNone) {
        v->fault = code;
        LOG("FAULT %u: %{public}s; refusing further MMIO until Stop", code, why);
    }
}

static inline bool ok(IV * v) { return v->fault == kAvbFaultNone; }

// Read with the all-ones check (device gone / hung).
static uint32_t rd(IV * v, uint32_t off)
{
    if (!ok(v)) {
        return 0xffffffff;
    }
    uint32_t x = 0xffffffff;
    v->pci->MemoryRead32(v->bar, off, &x);
    if (x == 0xffffffff) {
        setFault(v, kAvbFaultAllOnes, "MMIO read returned 0xFFFFFFFF");
    }
    return x;
}

// Read without the all-ones check, for registers whose value is arbitrary
// data (timestamp LSBs, NIC SRAM window data, the read-only debug dump).
static uint32_t rdRaw(IV * v, uint32_t off)
{
    if (!ok(v)) {
        return 0xffffffff;
    }
    uint32_t x = 0xffffffff;
    v->pci->MemoryRead32(v->bar, off, &x);
    return x;
}

static void wr(IV * v, uint32_t off, uint32_t val)
{
    if (!ok(v)) {
        return;
    }
    v->pci->MemoryWrite32(v->bar, off, val);
}

// Write then read back to push the posted write out.
static void wrFlush(IV * v, uint32_t off, uint32_t val)
{
    wr(v, off, val);
    (void)rd(v, off);
}

static uint32_t cfgRd32(IV * v, uint32_t off)
{
    uint32_t x = 0xffffffff;
    v->pci->ConfigurationRead32(off, &x);
    if (x == 0xffffffff) {
        setFault(v, kAvbFaultAllOnes, "config read returned 0xFFFFFFFF");
    }
    return x;
}

static uint16_t cfgRd16(IV * v, uint32_t off)
{
    uint16_t x = 0xffff;
    v->pci->ConfigurationRead16(off, &x);
    return x;
}

// Bounded poll: returns true once cond() holds, false after `timeoutUs`.
template <typename F>
static bool pollUs(IV * v, uint64_t timeoutUs, uint64_t stepUs, F cond)
{
    for (uint64_t waited = 0; waited <= timeoutUs; waited += stepUs) {
        if (!ok(v)) {
            return false;
        }
        if (cond()) {
            return true;
        }
        IODelay(stepUs);
    }
    return false;
}

// NIC SRAM through the memory window (tg3 MMIO path), base restored to 0.
static uint32_t sramRead(IV * v, uint32_t addr)
{
    wrFlush(v, bcm::kMemWinBase, addr);
    const uint32_t x = rdRaw(v, bcm::kMemWinData);
    wrFlush(v, bcm::kMemWinBase, 0);
    return x;
}

static void sramWrite(IV * v, uint32_t addr, uint32_t val)
{
    wrFlush(v, bcm::kMemWinBase, addr);
    wr(v, bcm::kMemWinData, val);
    (void)rd(v, bcm::kMemWinBase);
    wrFlush(v, bcm::kMemWinBase, 0);
}

static void cfgWr32(IV * v, uint32_t off, uint32_t val)
{
    if (!ok(v)) {
        return;
    }
    v->pci->ConfigurationWrite32(off, val);
}

// Raw config read (no all-ones fault check: the register-window data port can
// legitimately return 0xFFFFFFFF as scratch content).
static uint32_t cfgRd32Raw(IV * v, uint32_t off)
{
    uint32_t x = 0xffffffff;
    if (!ok(v)) {
        return x;
    }
    v->pci->ConfigurationRead32(off, &x);
    return x;
}

static void guardDoorbell(IV * v);

// Holds txLock for its scope. IOLock is not recursive: a function that takes
// it must never be called with it held (the *Locked variants assume it).
struct TxGuard {
    IV * v;
    bool held;
    explicit TxGuard(IV * iv, bool take = true) : v(iv), held(take)
    {
        if (held) {
            IOLockLock(v->txLock);
        }
    }
    ~TxGuard()
    {
        if (held) {
            IOLockUnlock(v->txLock);
        }
    }
    TxGuard(const TxGuard &) = delete;
    TxGuard & operator=(const TxGuard &) = delete;
};

// Let a send that is waiting for txLock take it before this thread does.
// Called only from the default queue (window accesses never run on the TX
// queue), so it cannot wait on itself. Bounded: past kTxYieldMaxUs the
// harvest proceeds anyway rather than stall the RX path behind a stuck send.
constexpr uint32_t kTxYieldMaxUs = 2000;

static void yieldToTx(IV * v)
{
    for (uint32_t waited = 0;
         waited < kTxYieldMaxUs && __atomic_load_n(&v->txWaiting, __ATOMIC_ACQUIRE) != 0; waited++) {
        IODelay(1);
    }
}

// Internal memory (RX/TX-CPU scratch at 0x30000/0x34000) through the register
// indirect window. Matches ThunderGate: write the big-endian-assembled word
// as-is, byte-swap on read (their htonl), restore the base to 0 after.
// Each access holds txLock (~110 us), never longer: a SendFrames waits at most
// one window word, where it used to wait out a whole multi-word refresh.
static void regWinWrite(IV * v, uint32_t winAddr, uint32_t word)
{
    yieldToTx(v);
    TxGuard g(v);
    guardDoorbell(v);
    cfgWr32(v, bcm::kRegWinBase, winAddr);
    cfgWr32(v, bcm::kRegWinData, word);
    cfgWr32(v, bcm::kRegWinBase, 0);
}

static uint32_t regWinRead(IV * v, uint32_t winAddr)
{
    // The register-window data port round-trips the written value unchanged
    // through ConfigurationRead32/Write32 (confirmed on hardware: writing a
    // big-endian-assembled word reads back identical, and the RX CPU fetches
    // it correctly). No byte swap here -- ThunderGate's htonl compensated for
    // its own Python/userclient layer, which DriverKit config access does not
    // impose.
    yieldToTx(v);
    TxGuard g(v);
    guardDoorbell(v);
    cfgWr32(v, bcm::kRegWinBase, winAddr);
    const uint32_t raw = cfgRd32Raw(v, bcm::kRegWinData);
    cfgWr32(v, bcm::kRegWinBase, 0);
    return raw;
}

static uint64_t nsToMach(IV * v, uint64_t ns)
{
    return v->tb.numer == 0 ? ns : ns * v->tb.denom / v->tb.numer;
}

static uint64_t machToUs(IV * v, uint64_t ticks)
{
    const uint64_t ns = v->tb.denom == 0 ? ticks : ticks * v->tb.numer / v->tb.denom;
    return ns / 1000;
}

// A config-space indirect access in the ~13 us after a TX doorbell corrupts
// the MAC's TX_TSTAMP latch (measured on this part, see drainReturnRing). The
// old guard covered only a doorbell for a TIMESTAMPED frame; at 8 audio
// streams an untimestamped doorbell rings every 125 us, so ~10% of stamp-ring
// reads landed in the window. Whatever else such an access disturbs is
// undocumented, so keep every register-window access clear of every
// doorbell. Waits at most kDoorbellGuardNs, and counts each wait.
constexpr uint64_t kDoorbellGuardNs = 20000;

static void guardDoorbell(IV * v)
{
    if (v->lastDoorbellMach == 0) {
        return;
    }
    const uint64_t guard = nsToMach(v, kDoorbellGuardNs);
    const uint64_t now = mach_absolute_time();
    const uint64_t since = now - v->lastDoorbellMach;
    if (now < v->lastDoorbellMach || since >= guard) {
        return;
    }
    v->ctr.c[kAvbCtrDoorbellGuardWaits]++;
    const uint64_t sinceNs = v->tb.denom == 0 ? since : since * v->tb.numer / v->tb.denom;
    const uint64_t leftNs = sinceNs < kDoorbellGuardNs ? kDoorbellGuardNs - sinceNs : 0;
    IODelay(static_cast<uint32_t>(leftNs / 1000 + 1));
}

// 64-bit timestamp register pair: LSB first (latches MSB / releases interlock).
static bool readTs(IV * v, uint32_t lsbOff, uint32_t msbOff, uint64_t * out)
{
    const uint32_t lo = rdRaw(v, lsbOff);
    const uint32_t hi = rdRaw(v, msbOff);
    if (hi == 0xffffffff) {
        setFault(v, kAvbFaultAllOnes, "timestamp MSB read 0xFFFFFFFF");
        return false;
    }
    *out = join64(hi, lo);
    return ok(v);
}

// Reconstruct a full 64-bit RX timestamp from the per-frame low-32 value the
// hardware writes into the RX return BD's reserved field (offset 0x18) and the
// free-running EAV reference clock, exactly as Apple's receivePackets does:
// read the EAV clock, and if its low word has already wrapped past the frame's
// latched low word, borrow one from the high word.
// Sample the free-running EAV LSB (a BAR read, safe during a TX latch) and
// advance the software high word on wrap. Returns the sampled low word. The
// absolute high word is kept correct by the TX-epoch re-align in checkTx; this
// only tracks wraps between those re-aligns.
static uint32_t eavSample(IV * v)
{
    const uint32_t lo = rdRaw(v, bcm::kEavRefClkLsb);
    if (v->eavSeeded && lo < v->eavLastLo) {
        v->eavHi++;
    }
    v->eavLastLo = lo;
    return lo;
}

// Reconstruct a full 64-bit RX timestamp from the per-frame low-32 value (from
// the RX-CPU stamp ring, or the RX BD reserved field) and the software EAV high
// word. EAV_REF_CLCK_MSB reads 0 on this part, so we cannot read the high word
// directly (as Apple's receivePackets does on parts where it works); instead we
// use eavHi, re-aligned to the TX latch epoch. If the current low word is at or
// behind the frame's, the frame predates the latest wrap -> borrow one.
// Takes txLock: eavHi/eavLastLo are also re-aligned by checkTx on the TX queue.
static uint64_t reconstructRxTs(IV * v, uint32_t frameLsb)
{
    TxGuard g(v);
    const uint32_t eavLsb = eavSample(v);
    return reconstructEavTs(v->eavHi, eavLsb, frameLsb);
}

// ---------------------------------------------------------------------------
// DMA buffers

static bool dmaAlloc(IV * v, DmaBuf * b, uint64_t len)
{
    *b = DmaBuf{};
    kern_return_t ret = IOBufferMemoryDescriptor::Create(kIOMemoryDirectionInOut, len, 4096, &b->md);
    if (ret != kIOReturnSuccess || b->md == nullptr) {
        LOG("IOBufferMemoryDescriptor::Create(%llu) failed 0x%x", len, ret);
        return false;
    }
    IOAddressSegment range{};
    b->md->GetAddressRange(&range);
    b->va = reinterpret_cast<uint8_t *>(range.address);
    b->len = len;
    memset(b->va, 0, len);

    IODMACommandSpecification spec{};
    spec.options = kIODMACommandSpecificationNoOptions;
    spec.maxAddressBits = 64;
    ret = IODMACommand::Create(v->pci, kIODMACommandCreateNoOptions, &spec, &b->cmd);
    if (ret != kIOReturnSuccess || b->cmd == nullptr) {
        LOG("IODMACommand::Create failed 0x%x", ret);
        return false;
    }
    uint64_t flags = 0;
    uint32_t segCount = 32;
    IOAddressSegment segs[32] = {};
    ret = b->cmd->PrepareForDMA(kIODMACommandPrepareForDMANoOptions, b->md, 0, len, &flags, &segCount, segs);
    if (ret != kIOReturnSuccess) {
        LOG("PrepareForDMA(%llu) failed 0x%x", len, ret);
        return false;
    }
    if (segCount != 1 || segs[0].length < len) {
        LOG("PrepareForDMA(%llu) gave %u segments; need exactly one", len, segCount);
        b->cmd->CompleteDMA(kIODMACommandCompleteDMANoOptions);
        OSSafeReleaseNULL(b->cmd);
        return false;
    }
    b->iova = segs[0].address;
    return true;
}

static void dmaFree(DmaBuf * b)
{
    if (b->cmd != nullptr) {
        b->cmd->CompleteDMA(kIODMACommandCompleteDMANoOptions);
    }
    OSSafeReleaseNULL(b->cmd);
    OSSafeReleaseNULL(b->md);
    *b = DmaBuf{};
}

static void dmaFreeAll(IV * v)
{
    dmaFree(&v->status);
    dmaFree(&v->stdRing);
    dmaFree(&v->retRing);
    dmaFree(&v->retRing1);
    dmaFree(&v->txRing);
    dmaFree(&v->rxBufs);
    dmaFree(&v->txBufs);
}

static void readStatusBlock(IV * v, uint8_t (&sb)[0x14])
{
    const volatile uint8_t * p = v->status.va;
    for (uint32_t i = 0; i < sizeof(sb); i++) {
        sb[i] = p[i];
    }
    __atomic_thread_fence(__ATOMIC_ACQUIRE);
}

// ---------------------------------------------------------------------------
// PCI command register

static void setCommandBits(IV * v, uint16_t set, uint16_t clear)
{
    uint16_t cmd = 0;
    v->pci->ConfigurationRead16(bcm::kCfgCommand, &cmd);
    cmd = static_cast<uint16_t>((cmd | set) & ~clear);
    v->pci->ConfigurationWrite16(bcm::kCfgCommand, cmd);
}

// ---------------------------------------------------------------------------
// Read-only capture of the windows in kSnapWindows, before Stage A writes
// anything. Reads only: no register in these windows is written here. Only
// for our own device (the match personality already restricts it; checked
// again because this runs before Stage A's own identity check).
static void takeSnapshot(IV * v)
{
    v->snapValid = false;
    if (!isOurDevice(v->vendorDevice, v->subsys) || !ok(v)) {
        LOG("snapshot: not our device or faulted; skipped");
        return;
    }
    v->snapMiscHostCtrl = cfgRd32(v, bcm::kMiscHostCtrl);
    uint32_t i = 0;
    for (const auto & w : kSnapWindows) {
        for (uint32_t off = w.start; off < w.end && i < kSnapWords; off += 4) {
            v->snap[i++] = rdRaw(v, off);
        }
    }
    v->snapValid = ok(v) && i == kSnapWords;
    LOG("snapshot: %u words of Apple-left register state captured (valid=%d), "
        "MISC_HOST_CTRL 0x%08x, RX_PTP_CTL 0x%08x, GRC_MODE 0x%08x, EAV_CTL 0x%08x",
        i, v->snapValid, v->snapMiscHostCtrl, v->snap[snapIndex(0x06c8)],
        v->snap[snapIndex(0x6800)], v->snap[snapIndex(0x6908)]);
}

// Stage A: time-sync enable (plan item 4). Writes exactly:
//   MISC_HOST_CTRL (config), MEMARB_MODE |= ENABLE, GRC_MODE |= TIME_SYNC_ENABLE.

static void stageA(IV * v)
{
    v->asicRev = cfgRd32(v, bcm::kGen15ProdIdAsicRev);
    const uint32_t mhc = cfgRd32(v, bcm::kMiscHostCtrl);
    LOG("stage A: asic_rev 0x%08x misc_host_ctrl 0x%08x", v->asicRev, mhc);
    if (!ok(v)) {
        return;
    }
    if (!isAsic57766(v->asicRev)) {
        LOG("stage A: ASIC rev is not 57766-class; staying at stage 0, no writes");
        return;
    }

    v->pci->ConfigurationWrite32(bcm::kMiscHostCtrl, bcm::kMhcLittleEndian);
    wr(v, bcm::kMemArbMode, rd(v, bcm::kMemArbMode) | bcm::kMemArbEnable);

    const uint32_t g = rd(v, bcm::kGrcMode);
    wr(v, bcm::kGrcMode, g | bcm::kGrcTimeSyncEnable);
    const uint32_t g2 = rd(v, bcm::kGrcMode);
    LOG("stage A: GRC_MODE 0x%08x -> 0x%08x", g, g2);

    const uint32_t sig = sramRead(v, bcm::kSramDataSig);
    const uint32_t cfg = sramRead(v, bcm::kSramDataCfg);
    v->flags &= ~(AVB_FLAG_APE | AVB_FLAG_ASF | AVB_FLAG_SIG_OK);
    if (sig == bcm::kDataSigMagic) {
        v->flags |= AVB_FLAG_SIG_OK;
        if (cfg & bcm::kDataCfgApeEnable) {
            v->flags |= AVB_FLAG_APE;
        }
        if (cfg & bcm::kDataCfgAsfEnable) {
            v->flags |= AVB_FLAG_ASF;
        }
    }
    LOG("stage A: NIC SRAM sig 0x%08x cfg 0x%08x (ape=%d asf=%d)", sig, cfg,
        (v->flags & AVB_FLAG_APE) != 0, (v->flags & AVB_FLAG_ASF) != 0);

    // Bootcode-loaded station address (read only).
    const uint32_t hi = rd(v, bcm::kMacAddr0High);
    const uint32_t lo = rdRaw(v, bcm::kMacAddr0Low);
    v->mac[0] = static_cast<uint8_t>(hi >> 8);
    v->mac[1] = static_cast<uint8_t>(hi);
    v->mac[2] = static_cast<uint8_t>(lo >> 24);
    v->mac[3] = static_cast<uint8_t>(lo >> 16);
    v->mac[4] = static_cast<uint8_t>(lo >> 8);
    v->mac[5] = static_cast<uint8_t>(lo);

    if (ok(v)) {
        v->stage = kAvbStageTimeSync;
        LOG("stage A complete (stage 1)");
    }
}

// ---------------------------------------------------------------------------
// EAV clock

static void setClock(IV * v, uint64_t ns)
{
    const uint32_t ctl = rd(v, bcm::kEavRefClkCtl) & ~(bcm::kEavCtlStop | bcm::kEavCtlResume);
    wr(v, bcm::kEavRefClkCtl, ctl | bcm::kEavCtlStop);
    wr(v, bcm::kEavRefClkLsb, static_cast<uint32_t>(ns));
    wr(v, bcm::kEavRefClkMsb, static_cast<uint32_t>(ns >> 32));
    wrFlush(v, bcm::kEavRefClkCtl, ctl | bcm::kEavCtlResume);
}

// ---------------------------------------------------------------------------
// MDIO (clause 22, internal PHY). Auto-poll is off after the core reset.

static bool miWait(IV * v, uint32_t * frameOut)
{
    uint32_t f = 0;
    const bool done = pollUs(v, 50000, 10, [&] {
        f = rd(v, bcm::kMacMiCom);
        return (f & bcm::kMiComStartBusy) == 0;
    });
    if (!done) {
        setFault(v, kAvbFaultMdio, "MAC_MI_COM busy timeout");
        return false;
    }
    IODelay(5);
    *frameOut = rd(v, bcm::kMacMiCom);
    return ok(v);
}

static bool miRead(IV * v, uint32_t reg, uint16_t * val)
{
    wrFlush(v, bcm::kMacMiCom, miReadFrame(bcm::kPhyAddr, reg));
    uint32_t f = 0;
    if (!miWait(v, &f) || (f & bcm::kMiComReadFailed) != 0) {
        return false;
    }
    *val = static_cast<uint16_t>(f & bcm::kMiComDataMask);
    return true;
}

static bool miWrite(IV * v, uint32_t reg, uint16_t val)
{
    wrFlush(v, bcm::kMacMiCom, miWriteFrame(bcm::kPhyAddr, reg, val));
    uint32_t f = 0;
    return miWait(v, &f);
}

// Autonegotiate if needed (no PHY reset), wait for link, set MAC port mode.
static void setupLink(IV * v)
{
    v->flags &= ~(AVB_FLAG_LINK_UP | AVB_FLAG_1000FD);
    uint16_t bmcr = 0;
    uint16_t bmsr = 0;
    if (!miRead(v, bcm::kMiiBmcr, &bmcr)) {
        LOG("datapath: BMCR read failed");
        return;
    }
    if ((bmcr & bcm::kBmcrAnEnable) == 0) {
        LOG("datapath: autoneg was off (BMCR 0x%04x); enabling", bmcr);
        if (!miWrite(v, bcm::kMiiBmcr, static_cast<uint16_t>(bmcr | bcm::kBmcrAnEnable | bcm::kBmcrAnRestart))) {
            return;
        }
    }
    bool link = false;
    for (uint64_t waited = 0; waited < kLinkWaitMs && ok(v); waited += 10) {
        // BMSR link status is latched-low: read twice.
        if (miRead(v, bcm::kMiiBmsr, &bmsr) && miRead(v, bcm::kMiiBmsr, &bmsr) &&
            (bmsr & bcm::kBmsrLinkStatus) != 0) {
            link = true;
            break;
        }
        IOSleep(10);
    }
    LinkSpeed speed = LinkSpeed::G1Full;
    if (link) {
        uint16_t aux = 0;
        if (miRead(v, bcm::kMiiAuxStat, &aux)) {
            speed = linkFromAuxStat(aux);
        }
        v->flags |= AVB_FLAG_LINK_UP;
        if (speed == LinkSpeed::G1Full) {
            v->flags |= AVB_FLAG_1000FD;
        }
        LOG("datapath: link up, BMSR 0x%04x speed %u", bmsr, static_cast<unsigned>(speed));
    } else {
        LOG("datapath: no link after %llu ms (BMSR 0x%04x); leaving MAC in GMII", kLinkWaitMs, bmsr);
    }
    wrFlush(v, bcm::kMacMode, macModeForLink(rd(v, bcm::kMacMode), speed));
    IODelay(40);
}

// ---------------------------------------------------------------------------
// Stage B: chip reset (tg3_chip_reset subset for a 57765_PLUS PCIe CPMU part).

static bool chipReset(IV * v)
{
    // Pre-reset signature for the bootcode.
    sramWrite(v, bcm::kSramFirmwareMbox, bcm::kFirmwareMagic);

    // Save what the core-clock reset clobbers.
    const uint16_t savedCmd = cfgRd16(v, bcm::kCfgCommand);

    wr(v, bcm::kGrcFastbootPc, 0);  // 5755_PLUS: force a full bootcode run

    // Core clock reset; bit 29 set on PCIe parts (written once on its own first).
    wr(v, bcm::kGrcMiscCfg, bcm::kGrcMiscPcieBit29);
    wr(v, bcm::kGrcMiscCfg, bcm::kGrcMiscCoreClkReset | bcm::kGrcMiscPcieBit29);
    IODelay(120);
    uint32_t flush = 0;
    v->pci->ConfigurationRead32(bcm::kCfgCommand, &flush);  // flush via config space
    IODelay(120);

    // PCIe: clear relaxed-ordering / no-snoop, clear device error status.
    if (v->pcieCap != 0) {
        uint16_t devctl = 0;
        v->pci->ConfigurationRead16(v->pcieCap + bcm::kPcieDevCtl, &devctl);
        devctl = static_cast<uint16_t>(devctl & ~(bcm::kPcieDevCtlRelaxEn | bcm::kPcieDevCtlNoSnoopEn));
        v->pci->ConfigurationWrite16(v->pcieCap + bcm::kPcieDevCtl, devctl);
        v->pci->ConfigurationWrite16(v->pcieCap + bcm::kPcieDevSta, bcm::kPcieDevStaErrBits);
    }

    // Restore PCI state.
    v->pci->ConfigurationWrite32(bcm::kMiscHostCtrl, bcm::kMhcLittleEndian);
    v->pci->ConfigurationWrite32(bcm::kPciState, bcm::kPciStateRomEnable | bcm::kPciStateRomRetry);
    v->pci->ConfigurationWrite16(bcm::kCfgCommand,
                                 static_cast<uint16_t>(savedCmd | bcm::kCmdMemSpace | bcm::kCmdBusMaster));

    wr(v, bcm::kMemArbMode, bcm::kMemArbEnable);

    // Wait for the bootcode to post ~MAGIC (tg3: 100000 x 10 us).
    const bool fwDone = pollUs(v, 1000000, 10, [&] {
        return sramRead(v, bcm::kSramFirmwareMbox) == static_cast<uint32_t>(~bcm::kFirmwareMagic);
    });
    if (!fwDone) {
        setFault(v, kAvbFaultFirmwarePoll, "bootcode did not post ~MAGIC within 1 s");
        return false;
    }

    wr(v, bcm::kGrcMode, bcm::kGrcSwapLittleEndian);
    wrFlush(v, bcm::kMacMode, 0);
    IODelay(40);

    // MDIO: auto-poll off.
    wrFlush(v, bcm::kMacMiMode, bcm::kMiModeBase);
    IODelay(80);

    // 57765_PLUS: tg3 skips the 0x7c00 bit 25 write. Re-probe the signature.
    if (sramRead(v, bcm::kSramDataSig) != bcm::kDataSigMagic) {
        setFault(v, kAvbFaultNoSignature, "NIC SRAM data signature missing after reset");
        return false;
    }
    return ok(v);
}

// Write the BDINFO host address + maxlen_flags of a NIC-SRAM ring control block
// (5705_PLUS: no NIC address word).
static void sramSetRcb(IV * v, uint32_t rcb, uint64_t iova, uint32_t maxlenFlags)
{
    sramWrite(v, rcb + bcm::kBdInfoHostAddrHi, static_cast<uint32_t>(iova >> 32));
    sramWrite(v, rcb + bcm::kBdInfoHostAddrLo, static_cast<uint32_t>(iova));
    sramWrite(v, rcb + bcm::kBdInfoMaxLenFlags, maxlenFlags);
}

static void postStdBuffer(IV * v, uint32_t bufIndex)
{
    auto * ring = reinterpret_cast<RxBd *>(v->stdRing.va);
    ring[v->stdProd] = makeStdRxBd(v->rxBufs.iova + uint64_t(bufIndex) * kRxBufSize, bufIndex, bcm::kRxStdDmaSize);
    v->stdProd = ringNext(v->stdProd, bcm::kStdRingSize);
}

// tg3_stop_block behaviour: clear the enable bit, poll up to 100 ms.
static bool stopBlock(IV * v, uint32_t off, uint32_t enableBit)
{
    wrFlush(v, off, rd(v, off) & ~enableBit);
    return pollUs(v, 100000, 100, [&] { return (rd(v, off) & enableBit) == 0; });
}

static void programAvbRxRules(IV * v);

// tg3_reset_hw subset (single vector, no TSO frames, no jumbo, no RSS/TSS).
static bool resetHw(IV * v, uint64_t initFlags)
{
    if (!chipReset(v)) {
        return false;
    }

    // 57765_CLASS, not 57765 AX: transmit-hang fix, DL FTS max, 10 Mb MAC clock.
    wr(v, bcm::kCpmuPadRngCtl, rd(v, bcm::kCpmuPadRngCtl) | bcm::kCpmuPadRngRdiv2);
    {
        const uint32_t grc = rd(v, bcm::kGrcMode);
        wr(v, bcm::kGrcMode, (grc & ~bcm::kGrcPciePortMask) | bcm::kGrcPcieDlSel);
        const uint32_t off = bcm::kPcieTlDlPlPort + bcm::kPcieDlLoFtsMax;
        wr(v, off, (rd(v, off) & ~bcm::kPcieFtsMaxMask) | bcm::kPcieFtsMaxValue);
        wr(v, bcm::kGrcMode, grc);
    }
    wr(v, bcm::kCpmuLspd10MbClk,
       (rd(v, bcm::kCpmuLspd10MbClk) & ~bcm::kCpmuLspd10MbMacMask) | bcm::kCpmuLspd10Mb6_25);

    // Host-memory rings (tg3_init_rings): clear, post kRxPosted std buffers.
    memset(v->stdRing.va, 0, v->stdRing.len);
    memset(v->retRing.va, 0, v->retRing.len);
    memset(v->retRing1.va, 0, v->retRing1.len);
    memset(v->txRing.va, 0, v->txRing.len);
    memset(v->status.va, 0, v->status.len);
    v->stdProd = 0;
    v->retCons = 0;
    v->retCons1 = 0;
    v->txProd = 0;
    v->txCons = 0;
    for (uint32_t i = 0; i < kRxPosted; i++) {
        postStdBuffer(v, i);
    }
    __atomic_thread_fence(__ATOMIC_SEQ_CST);

    // DMA_RW_CTRL for 57765_PLUS: tg3's dma_rwctrl is DIS_CACHE_ALIGNMENT here.
    wr(v, bcm::kDmaRwCtrl, (rd(v, bcm::kDmaRwCtrl) & ~bcm::kDmaRwDisCacheAlign) | bcm::kDmaRwDisCacheAlign);

    // RX PTP filter immediately before the GRC_MODE write, as tg3 orders it.
    if (initFlags & AVB_INIT_FLAG_RX_PTP_V2_L2) {
        wr(v, bcm::kRxPtpCtl, bcm::kRxPtpPhase0Value);
    }
    wr(v, bcm::kGrcMode, grcModeDatapath());

    // Timer prescaler (66 MHz core clock -> 1 us).
    wr(v, bcm::kGrcMiscCfg, (rd(v, bcm::kGrcMiscCfg) & ~bcm::kGrcMiscPrescaleMask) | bcm::kGrcMiscPrescale66);

    // Buffer manager watermarks (57765_PLUS, MTU 1500) and enable.
    wr(v, bcm::kBufMgrMbRdmaLowWm, bcm::kMbRdmaLowWm57765);
    wr(v, bcm::kBufMgrMbMacRxLowWm, bcm::kMbMacRxLowWm57765);
    wr(v, bcm::kBufMgrMbHighWm, bcm::kMbHighWm57765);
    wr(v, bcm::kBufMgrDmaLowWm, bcm::kDmaLowWmDefault);
    wr(v, bcm::kBufMgrDmaHighWm, bcm::kDmaHighWmDefault);
    wr(v, bcm::kBufMgrMode, bcm::kBufMgrEnable | bcm::kBufMgrAttn);
    if (!pollUs(v, 20000, 10, [&] { return (rd(v, bcm::kBufMgrMode) & bcm::kBufMgrEnable) != 0; })) {
        setFault(v, kAvbFaultBufmgr, "BUFMGR enable timeout");
        return false;
    }

    // RX BD replenish thresholds.
    wr(v, bcm::kRcvBdiStdThresh, stdReplenishThreshold(kRxPosted));
    wr(v, bcm::kStdReplenishLwm, bcm::kStdBdCacheSize);
    wr(v, bcm::kRcvBdiJumboThresh, jumboReplenishThreshold(bcm::kJumboPendingDefault));
    wr(v, bcm::kJmbReplenishLwm, bcm::kJmbBdCacheSize);

    // Standard RX ring BDINFO; jumbo ring disabled at MTU 1500.
    wr(v, bcm::kRcvDbdiStdBd + bcm::kBdInfoHostAddrHi, static_cast<uint32_t>(v->stdRing.iova >> 32));
    wr(v, bcm::kRcvDbdiStdBd + bcm::kBdInfoHostAddrLo, static_cast<uint32_t>(v->stdRing.iova));
    wr(v, bcm::kRcvDbdiStdBd + bcm::kBdInfoNicAddr, bcm::kSramRxBufferDesc);
    wr(v, bcm::kRcvDbdiJumboBd + bcm::kBdInfoMaxLenFlags, bcm::kBdInfoDisabled);
    wr(v, bcm::kRcvDbdiStdBd + bcm::kBdInfoMaxLenFlags, stdRingMaxLenFlags());
    wr(v, bcm::kMboxRxStdProd, v->stdProd);
    wr(v, bcm::kMboxRxJumboProd, 0);

    // tg3_rings_reset: disable unused RCBs, mask interrupts, zero mailboxes,
    // status block address, then RCB 0 of each kind.
    for (uint32_t i = 1; i < bcm::kSendRcbCount; i++) {
        sramWrite(v, bcm::kSramSendRcb + i * bcm::kBdInfoSize + bcm::kBdInfoMaxLenFlags, bcm::kBdInfoDisabled);
    }
    // Return ring 1 is the AVB realtime RX queue (the classifier routes gPTP
    // frames there and the hardware stamps BD.reserved). Enable it only with
    // the AVB RX rules; otherwise keep every ring above 0 disabled as tg3 does.
    const bool avbRing = (initFlags & AVB_INIT_FLAG_AVB_RX_RULES) != 0;
    const uint32_t firstDisabledRet = avbRing ? 2u : 1u;
    for (uint32_t i = firstDisabledRet; i < bcm::kRxRetRcbCount; i++) {
        sramWrite(v, bcm::kSramRxRetRcb + i * bcm::kBdInfoSize + bcm::kBdInfoMaxLenFlags, bcm::kBdInfoDisabled);
    }
    wrFlush(v, bcm::kMboxInterrupt0, 1);
    wr(v, bcm::kMboxTxHostProd0, 0);
    wr(v, bcm::kMboxRxRetCons0, 0);
    wr(v, bcm::kHostccStatusBlkHi, static_cast<uint32_t>(v->status.iova >> 32));
    wr(v, bcm::kHostccStatusBlkLo, static_cast<uint32_t>(v->status.iova));
    sramSetRcb(v, bcm::kSramSendRcb, v->txRing.iova, sendRcbMaxLenFlags());
    sramSetRcb(v, bcm::kSramRxRetRcb, v->retRing.iova, rxRetRcbMaxLenFlags());
    if (avbRing) {
        wr(v, bcm::kMboxRxRetCons1, 0);
        sramSetRcb(v, bcm::kSramRxRetRcb + bcm::kBdInfoSize, v->retRing1.iova, rxRetRcbMaxLenFlags());
        LOG("AVB realtime return ring 1 enabled at iova 0x%llx", v->retRing1.iova);
    }

    // MAC address registers are left as the bootcode loaded them (read only).
    wr(v, bcm::kMacRxMtu, bcm::kMacRxMtuValue);
    wr(v, bcm::kMacTxLengths, bcm::kTxLengthsDefault);
    wr(v, bcm::kMacRcvRuleCfg, bcm::kRcvRuleCfgDefClass);
    wr(v, bcm::kRcvLpcConfig, bcm::kRcvLpcConfigValue);

    // RDMA reserve control FIFO overflow fix (57765_PLUS).
    wr(v, bcm::kRdmaRsrvCtrl, rd(v, bcm::kRdmaRsrvCtrl) | bcm::kRdmaRsrvFifoOflwFix);

    // Statistics enables (5750_PLUS).
    wr(v, bcm::kRcvLpcStatsEnable, rd(v, bcm::kRcvLpcStatsEnable) & ~bcm::kRcvLpcStatsDackFix);
    wr(v, bcm::kRcvLpcStatsCtrl, bcm::kRcvLpcStatsCtrlEn);
    wr(v, bcm::kSndDataIStatsEnab, 0x00ffffff);
    wr(v, bcm::kSndDataIStatsCtrl, bcm::kSndDataIStatsEn | bcm::kSndDataIStatsFast);

    // Host coalescing: off, wait, program, enable. Interrupts stay masked.
    wr(v, bcm::kHostccMode, 0);
    if (!pollUs(v, 20000, 10, [&] { return (rd(v, bcm::kHostccMode) & bcm::kHostccEnable) == 0; })) {
        setFault(v, kAvbFaultHostcc, "HOSTCC disable timeout");
        return false;
    }
    wr(v, bcm::kHostccTxColTicks, 25);
    wr(v, bcm::kHostccTxMaxFrames, 1);
    wr(v, bcm::kHostccTxMaxfInt, 1);
    wr(v, bcm::kHostccRxColTicks, 25);
    wr(v, bcm::kHostccRxMaxFrames, 15);
    wr(v, bcm::kHostccRxMaxfInt, 1);
    wr(v, bcm::kHostccMode, bcm::kHostccEnable);

    wr(v, bcm::kRcvCcMode, bcm::kRcvCcEnable | bcm::kRcvCcAttn);
    wr(v, bcm::kRcvLpcMode, bcm::kRcvLpcEnable);

    wrFlush(v, bcm::kMacMode, macModeDatapath() | bcm::kMacModeRxStatClr | bcm::kMacModeTxStatClr);
    IODelay(40);

    wrFlush(v, bcm::kWdmacMode, wdmacModeValue());
    IODelay(40);
    wrFlush(v, bcm::kRdmacMode, rdmacModeValue());
    IODelay(40);

    wr(v, bcm::kRcvDccMode, bcm::kRcvDccEnable | bcm::kRcvDccAttn);
    wr(v, bcm::kSndDataCMode, bcm::kSndDataCEnable);
    wr(v, bcm::kSndBdcMode, bcm::kSndBdcEnable | bcm::kSndBdcAttn);
    wr(v, bcm::kRcvBdiMode, bcm::kRcvBdiEnable | bcm::kRcvBdiRcbAttn);
    wr(v, bcm::kRcvDbdiMode, bcm::kRcvDbdiEnable | bcm::kRcvDbdiInvRingSz);
    wr(v, bcm::kSndDataIMode, bcm::kSndDataIEnable);
    wr(v, bcm::kSndDataIMode, bcm::kSndDataIEnable | bcm::kSndDataIHwTso);  // HW_TSO_3 part
    wr(v, bcm::kSndBdiMode, bcm::kSndBdiEnable | bcm::kSndBdiAttn);
    wr(v, bcm::kSndBdsMode, bcm::kSndBdsEnable | bcm::kSndBdsAttn);

    wrFlush(v, bcm::kMacTxMode, bcm::kTxModeEnable | bcm::kTxModeMbufLockFix);
    IODelay(100);

    uint32_t rxMode = bcm::kRxModeEnable | bcm::kRxModeIpv6Csum;
    wrFlush(v, bcm::kMacRxMode, rxMode);
    IODelay(10);
    wr(v, bcm::kMacMiStat, bcm::kMiStatLinkAttnEn);
    wrFlush(v, bcm::kMacRxMode, rxMode);
    IODelay(10);
    wrFlush(v, bcm::kMacLowWmarkMaxRx, 1);  // 57765_CLASS

    setupLink(v);
    if (!ok(v)) {
        return false;
    }

    // RX mode: keep VLAN tags in the frame (unless ASF owns them), promiscuous
    // in Phase 0 instead of programming the multicast hash.
    if ((v->flags & AVB_FLAG_ASF) == 0) {
        rxMode |= bcm::kRxModeKeepVlan;
    }
    if (initFlags & AVB_INIT_FLAG_PROMISC) {
        rxMode |= bcm::kRxModePromisc;
    }
    wrFlush(v, bcm::kMacRxMode, rxMode);
    IODelay(10);

    // Receive rules 0/1 defaults; rules 4..7 cleared (5705_PLUS has 8, ASF keeps 4).
    wr(v, bcm::kMacRcvRule0, bcm::kRcvRule0Default & bcm::kRcvRuleDisableMask);
    wr(v, bcm::kMacRcvValue0, 0xffffffff & bcm::kRcvRuleDisableMask);
    wr(v, bcm::kMacRcvRule1, bcm::kRcvRule1Default & bcm::kRcvRuleDisableMask);
    wr(v, bcm::kMacRcvValue1, 0xffffffff & bcm::kRcvRuleDisableMask);
    if ((v->flags & AVB_FLAG_ASF) == 0) {
        for (uint32_t i = 0; i < 4; i++) {
            wr(v, bcm::kMacRcvRule4 + i * 8, 0);
            wr(v, bcm::kMacRcvRule4 + i * 8 + 4, 0);
        }
    }
    // Apple's AVB RX classifier block, so gPTP frames latch RX timestamps
    // (AVBBcmDriver's reason for existing). Replayed exactly from what
    // AppleBCM5701Ethernet writes, each register READ before it is written
    // and the before/after logged, per the register-provenance rule in
    // AGENTS.md. Gated so a plain datapath bring-up is unchanged.
    if (ok(v) && (initFlags & AVB_INIT_FLAG_AVB_RX_RULES)) {
        programAvbRxRules(v);
    }

    return ok(v);
}

// Replay Apple's AVB RX classifier writes (kAvbRxRuleWrites). Read-first,
// log each. These offsets are outside the tg3-documented set; they come from
// read-only observation of Apple's driver (docs/bcm-rx-timestamp-investigation.md).
static void programAvbRxRules(IV * v)
{
    // Experiment (2026-11-04): the 0x5400 block lives in the RX CPU's address
    // space (tg3 RX_CPU at 0x5000) and read back 0 when written with the CPU
    // running. Halt the RX CPU first, exactly as Apple's HaltCpu does (STATE
    // = all-ones, MODE = HALT), write the config, then release it, and log
    // the readbacks so we can see whether halting makes the writes stick.
    const uint32_t savedMode = rd(v, bcm::kRxCpuMode);
    const uint32_t savedState = rd(v, bcm::kRxCpuState);
    const uint32_t savedPc = rd(v, 0x501c);
    LOG("AVB RX rules: RX CPU before halt mode 0x%08x state 0x%08x pc 0x%08x", savedMode,
        savedState, savedPc);
    wr(v, bcm::kRxCpuState, 0xffffffff);
    wr(v, bcm::kRxCpuMode, savedMode | bcm::kRxCpuModeHalt);
    bool halted = pollUs(v, 5000, 50,
                         [&] { return (rd(v, bcm::kRxCpuMode) & bcm::kRxCpuModeHalt) != 0; });
    LOG("AVB RX rules: RX CPU halt requested, halted=%d mode 0x%08x", halted,
        rd(v, bcm::kRxCpuMode));

    LOG("AVB RX rules: replaying %u Apple classifier write(s)", kAvbRxRuleWriteCount);
    uint32_t stuck = 0;
    for (uint32_t i = 0; i < kAvbRxRuleWriteCount && ok(v); i++) {
        const uint32_t off = kAvbRxRuleWrites[i].off;
        const uint32_t val = kAvbRxRuleWrites[i].val;
        const uint32_t before = rd(v, off);
        wr(v, off, val);
        const uint32_t after = rd(v, off);
        if (after == val) {
            stuck++;
        }
        LOG("AVB RX rule[%u]: 0x%04x 0x%08x -> wrote 0x%08x -> 0x%08x%s", i, off, before, val,
            after, after == val ? " (stuck)" : "");
    }
    LOG("AVB RX rules: %u of %u writes stuck while halted", stuck, kAvbRxRuleWriteCount);

    // Release the RX CPU: restore its original mode (clears HALT).
    wr(v, bcm::kRxCpuMode, savedMode & ~bcm::kRxCpuModeHalt);
    LOG("AVB RX rules done: RX CPU mode 0x%08x, EAV_REF_CLK 0x%08x, RX_PTP_CTL 0x%08x",
        rd(v, bcm::kRxCpuMode), rd(v, bcm::kEavRefClkLsb), rd(v, bcm::kRxPtpCtl));
}

// Map an RX-CPU virtual scratch address (0x0800xxxx) to its memory-window
// address (0x0003xxxx), bounds-checked to the 64 KB pad.
static bool scratchWinAddr(uint32_t virtAddr, uint32_t len, uint32_t * winOut)
{
    if (virtAddr < bcm::kRxCpuScratchVirtBase) {
        return false;
    }
    const uint32_t off = virtAddr - bcm::kRxCpuScratchVirtBase;
    if (off >= bcm::kRxCpuScratchSize || len > bcm::kRxCpuScratchSize - off) {
        return false;
    }
    *winOut = bcm::kRxCpuScratchWinBase + off;
    return true;
}

// M1: load a clean-room RX-CPU image into the scratch pad and start it.
// Mirrors ThunderGate image_load / Apple loadFirmware: halt the core, write the
// flat image word by word through the memory window, point the PC at the image
// base, release. The host reads back a signature/heartbeat with readCoreMem to
// prove the core executed. Image words are assembled big-endian (the core is a
// big-endian MIPS); the readback is self-consistent through the same window.
static kern_return_t loadCoreImage(IV * v, uint32_t loadAddr, const uint8_t * img, uint32_t len,
                                   uint64_t * haltedOut, uint64_t * modeOut)
{
    if (!ok(v)) {
        return kIOReturnNotReady;
    }
    uint32_t winBase = 0;
    if (img == nullptr || len == 0 || (len & 3u) != 0 || !scratchWinAddr(loadAddr, len, &winBase)) {
        return kIOReturnBadArgument;
    }

    const uint32_t savedMode = rd(v, bcm::kRxCpuMode);
    wr(v, bcm::kRxCpuState, 0xffffffff);
    wr(v, bcm::kRxCpuMode, savedMode | bcm::kRxCpuModeHalt);
    const bool halted =
        pollUs(v, 5000, 50, [&] { return (rd(v, bcm::kRxCpuMode) & bcm::kRxCpuModeHalt) != 0; });
    LOG("core load: halt requested, halted=%d mode 0x%08x, writing %u bytes at 0x%08x (win 0x%05x)",
        halted, rd(v, bcm::kRxCpuMode), len, loadAddr, winBase);

    for (uint32_t i = 0; i < len && ok(v); i += 4) {
        const uint32_t word = (static_cast<uint32_t>(img[i]) << 24) |
                              (static_cast<uint32_t>(img[i + 1]) << 16) |
                              (static_cast<uint32_t>(img[i + 2]) << 8) |
                              static_cast<uint32_t>(img[i + 3]);
        regWinWrite(v, winBase + i, word);
    }

    // Point the core at the image base and release it. Setting the PC register
    // then clearing HALT resumes execution there (tg3_load_firmware_cpu order).
    wr(v, bcm::kRxCpuPc, loadAddr);
    wr(v, bcm::kRxCpuMode, savedMode & ~bcm::kRxCpuModeHalt);
    const uint32_t finalMode = rd(v, bcm::kRxCpuMode);
    LOG("core load: released, mode 0x%08x pc 0x%08x", finalMode, rd(v, bcm::kRxCpuPc));

    if (haltedOut != nullptr) {
        *haltedOut = halted ? 1 : 0;
    }
    if (modeOut != nullptr) {
        *modeOut = finalMode;
    }
    return ok(v) ? kIOReturnSuccess : kIOReturnIOError;
}

// M1 readback: one word of RX-CPU scratch, by virtual address.
static kern_return_t readCoreMem(IV * v, uint32_t virtAddr, uint64_t * wordOut)
{
    if (!ok(v)) {
        return kIOReturnNotReady;
    }
    uint32_t winAddr = 0;
    if (!scratchWinAddr(virtAddr, 4, &winAddr)) {
        return kIOReturnBadArgument;
    }
    if (wordOut != nullptr) {
        *wordOut = regWinRead(v, winAddr);
    }
    return ok(v) ? kIOReturnSuccess : kIOReturnIOError;
}

// RX-CPU stamp ring (driver/bcm-fw, AVB_INIT_FLAG_RXCPU_STAMP). The firmware
// records one {eavLo, msgType|seq} entry per gPTP frame in RX-CPU scratch; we
// read them back through the memory window and match a harvested frame to its
// EAV timestamp. These scratch virtual addresses mirror driver/bcm-fw/regs.h.
namespace rxcpu {
constexpr uint32_t kDoneSig    = 0x0800F004u;  // == 'DONE' once the firmware configured
constexpr uint32_t kDoneMagic  = 0x444F4E45u;
constexpr uint32_t kProd       = 0x0800BFF0u;  // ring producer count
constexpr uint32_t kRingBase   = 0x0800C000u;  // 256 x {eavLo, meta} 8-byte slots
constexpr uint32_t kRingMask   = 0xffu;        // 256 entries (power of two)
constexpr uint32_t kRefreshWindow = 4u;        // max new slots read per refresh
static_assert(kRefreshWindow <= kRxCpuStampCacheSize, "refresh fits the host cache");
}  // namespace rxcpu

static uint32_t readScratch(IV * v, uint32_t virt)
{
    uint32_t win = 0;
    if (!scratchWinAddr(virt, 4, &win)) {
        return 0;
    }
    return regWinRead(v, win);
}

// True once the stamping firmware has run far enough to maintain the ring.
// Guards against matching uninitialised scratch before the host loads it.
// The signature is read once; after that this costs nothing.
static bool rxCpuStampReady(IV * v)
{
    if (!v->rxcpuStamp) {
        return false;
    }
    if (!v->stampDone) {
        v->stampDone = readScratch(v, rxcpu::kDoneSig) == rxcpu::kDoneMagic;
    }
    return v->stampDone;
}

static void resetStampCache(IV * v)
{
    v->stampDone = false;
    v->stampCache = RxCpuStampCache{};
}

// Read the ring entries the firmware added since the last refresh: the
// producer count, then for each new slot its meta word, plus its EAV word only
// for an event message. Each word is a ~145 us config-window access.
static void refreshStampCache(IV * v)
{
    v->ctr.c[kAvbCtrStampRefreshes]++;
    RxCpuStampCache & c = v->stampCache;
    const uint32_t prod = readScratch(v, rxcpu::kProd);
    if (prod < c.seen) {
        // The firmware restarted its count (reloaded): every cached entry
        // belongs to the previous run and could match a new frame's key.
        c.count = 0;
    }
    // At most rxcpu::kRefreshWindow new entries per refresh: each costs one
    // or two ~145 us config-window words on the queue that also serves
    // SendFrames, and one gPTP event frame normally adds one entry.
    for (uint32_t i = stampReadStart(c.seen, prod, rxcpu::kRefreshWindow); i != prod; i++) {
        const uint32_t base = rxcpu::kRingBase + (i & rxcpu::kRingMask) * 8u;
        const uint32_t meta = readScratch(v, base + 4u);
        RxCpuStamp s = decodeRxCpuStamp(0, meta);
        if (isPtpEventMsg(s.msgType)) {
            s.eavLo = readScratch(v, base);
        }
        stampCachePush(c, s);
    }
    c.seen = prod;
}

// Look up a harvested gPTP event frame's EAV timestamp. The cache is
// refreshed at most once per drain (`*refreshed`), and only when the frame is
// not already in it. On a match set e.hw_ts (reconstructed to 64 bits from the
// EAV clock) and e.has_ts. `frame`/`len` are the copied frame bytes.
static void attachRxCpuStamp(IV * v, const uint8_t * frame, uint32_t len, AvbRxEntry & e,
                             bool * refreshed)
{
    uint8_t msgType = 0;
    uint16_t seq = 0;
    if (!parseGptpStampKey(frame, len, &msgType, &seq) || !isPtpEventMsg(msgType)) {
        return;
    }
    if (!rxCpuStampReady(v)) {
        return;
    }
    // The high 32 bits come only from a TX latch (EAV MSB reads 0 on this
    // part). Before the first one, a reconstructed time would be in the wrong
    // epoch: deliver unstamped rather than wrong.
    if (!v->eavSeeded) {
        v->ctr.c[kAvbCtrStampSkipped]++;
        return;
    }
    uint32_t eavLo = 0;
    bool hit = matchRxCpuStamp(v->stampCache.e, v->stampCache.count, msgType, seq, &eavLo);
    if (!hit && !*refreshed) {
        refreshStampCache(v);
        *refreshed = true;
        hit = matchRxCpuStamp(v->stampCache.e, v->stampCache.count, msgType, seq, &eavLo);
    }
    if (hit) {
        e.hw_ts = reconstructRxTs(v, eavLo);
        e.has_ts = 1;
        v->ctr.c[kAvbCtrStampHit]++;
    } else {
        v->ctr.c[kAvbCtrStampMiss]++;
    }
}

// An event-message gPTP frame (one that needs an RX timestamp)?
static bool isGptpEventFrame(const uint8_t * frame, uint32_t len)
{
    uint8_t msgType = 0;
    uint16_t seq = 0;
    return parseGptpStampKey(frame, len, &msgType, &seq) && isPtpEventMsg(msgType);
}

// tg3_abort_hw order for a 5705_PLUS part (RCVLSC/DMAC/MBFREE/BUFMGR/MEMARB
// cannot be stopped on this class and are skipped, as tg3 does).
static void abortHw(IV * v)
{
    bool allStopped = true;
    wrFlush(v, bcm::kMacRxMode, rd(v, bcm::kMacRxMode) & ~bcm::kRxModeEnable);
    IODelay(10);
    allStopped &= stopBlock(v, bcm::kRcvBdiMode, bcm::kRcvBdiEnable);
    allStopped &= stopBlock(v, bcm::kRcvLpcMode, bcm::kRcvLpcEnable);
    allStopped &= stopBlock(v, bcm::kRcvDbdiMode, bcm::kRcvDbdiEnable);
    allStopped &= stopBlock(v, bcm::kRcvDccMode, bcm::kRcvDccEnable);
    allStopped &= stopBlock(v, bcm::kRcvCcMode, bcm::kRcvCcEnable);
    allStopped &= stopBlock(v, bcm::kSndBdsMode, bcm::kSndBdsEnable);
    allStopped &= stopBlock(v, bcm::kSndBdiMode, bcm::kSndBdiEnable);
    allStopped &= stopBlock(v, bcm::kSndDataIMode, bcm::kSndDataIEnable);
    allStopped &= stopBlock(v, bcm::kRdmacMode, bcm::kRdmacEnable);
    allStopped &= stopBlock(v, bcm::kSndDataCMode, bcm::kSndDataCEnable);
    allStopped &= stopBlock(v, bcm::kSndBdcMode, bcm::kSndBdcEnable);

    wrFlush(v, bcm::kMacMode, rd(v, bcm::kMacMode) & ~bcm::kMacModeTdeEn);
    IODelay(40);
    wr(v, bcm::kMacTxMode, rd(v, bcm::kMacTxMode) & ~bcm::kTxModeEnable);
    allStopped &= pollUs(v, 100000, 100, [&] { return (rd(v, bcm::kMacTxMode) & bcm::kTxModeEnable) == 0; });

    allStopped &= stopBlock(v, bcm::kHostccMode, bcm::kHostccEnable);
    allStopped &= stopBlock(v, bcm::kWdmacMode, bcm::kWdmacEnable);

    wr(v, bcm::kFtqReset, 0xffffffff);
    wr(v, bcm::kFtqReset, 0x00000000);

    // Do NOT latch a hard fault here. With the RX-CPU stamper firmware holding
    // the RX path, a block enable bit routinely refuses to clear on shutdown
    // ("block stop timeout"). The next InitDatapath performs a full chipReset,
    // so this is benign; latching kAvbFaultStopBlock only wedged the device
    // (all MMIO refused) until a physical replug. Log and move on.
    if (!allStopped) {
        // Counted, so a chip that keeps refusing to stop stays visible
        // (avb-probe counters: stop_block_timeouts) without wedging it.
        v->ctr.c[kAvbCtrStopBlockTimeouts]++;
        LOG("abortHw: a block enable bit did not clear during shutdown "
            "(expected with stamper loaded; chipReset on next InitDatapath clears it)");
    }
}

// ---------------------------------------------------------------------------
// Harvest (1 ms timer and start of kAvbSelPollRx)

static void swPush(IV * v, const AvbRxEntry & e)
{
    if (ringDistance(v->swTail, v->swHead, kSwRingSize) == kSwRingSize - 1) {
        v->swTail = ringNext(v->swTail, kSwRingSize);  // overwrite oldest
        v->swDropped++;
    }
    v->sw[v->swHead] = e;
    v->swHead = ringNext(v->swHead, kSwRingSize);
}

// Per-harvest view of the shared rings: how many slots the producer may still
// fill (validated against the client-writable consumer tails).
struct RingTick {
    bool ringMode;
    uint64_t ctlFree;
    uint64_t audioFree;
};

static RingTick ringTickBegin(IV * v)
{
    RingTick t{v->producerRunning && v->hdr != nullptr && v->cons != nullptr, 0, 0};
    if (!t.ringMode) {
        return t;
    }
    const uint64_t tailC = __atomic_load_n(&v->cons->ctl_tail, __ATOMIC_ACQUIRE);
    const uint64_t tailA = __atomic_load_n(&v->cons->audio_tail, __ATOMIC_ACQUIRE);
    const RingSpace c = ringFree(v->ctlHead, tailC, AVB_RX_CTL_SLOTS);
    const RingSpace a = ringFree(v->audioHead, tailA, AVB_RX_AUDIO_SLOTS);
    if (!c.valid || !a.valid) {
        v->ctr.c[kAvbCtrConsumerIndexInvalid]++;
    }
    t.ctlFree = c.free;  // 0 (treated as full) when invalid
    t.audioFree = a.free;
    return t;
}

// Ring mode: classify the whole frame (the tag stays in the frame on this part:
// RX_MODE KEEP_VLAN), then copy it into the control or audio ring. Drop-newest
// on overrun, exactly as the I210 dext does.
static void deliverRing(IV * v, RingTick * t, const uint8_t * frame, uint32_t flen, const AvbRxEntry & e)
{
    if (flen > AVB_RX_SLOT_DATA_MAX) {
        v->ctr.c[kAvbCtrRxOversize]++;
        return;
    }
    const RxVerdict verdict = classifyRx(frame, flen, v->mac, v->filter, false, 0);
    if (!rxDelivered(verdict)) {
        v->ctr.c[rxVerdictCounter(verdict)]++;
        return;
    }
    const bool audio = verdict == RxVerdict::Stream;
    uint64_t * freeSlots = audio ? &t->audioFree : &t->ctlFree;
    uint64_t * head = audio ? &v->audioHead : &v->ctlHead;
    if (*freeSlots == 0) {
        v->ctr.c[audio ? kAvbCtrRingOverrunAudio : kAvbCtrRingOverrunCtl]++;
        return;
    }
    const uint64_t base = audio ? AVB_RX_AUDIO_OFFSET : AVB_RX_CTL_OFFSET;
    const uint64_t mask = (audio ? AVB_RX_AUDIO_SLOTS : AVB_RX_CTL_SLOTS) - 1;
    auto * slot = reinterpret_cast<AvbRxSlot *>(v->rxVa + base + (*head & mask) * AVB_RX_SLOT_SIZE);
    slot->hw_ts_ns = e.has_ts ? e.hw_ts : 0;
    slot->harvest_mach = e.harvest_mach;
    slot->index = *head;
    slot->status_error = e.type_flags;
    slot->len = static_cast<uint16_t>(flen);
    slot->vlan_tci = 0;
    slot->flags = static_cast<uint16_t>(rxVlanFlags(false, frame, flen) | (e.has_ts ? AVB_RXF_TS_HW : 0u));
    slot->cls = static_cast<uint8_t>(verdict);
    slot->pad0 = 0;
    slot->desc_len = flen;
    slot->clock_gen = 0;  // the BCM clock is never stepped (software GM offset in the daemon)
    memcpy(slot->data, frame, flen);
    (*head)++;
    (*freeSlots)--;
    v->ctr.c[rxVerdictCounter(verdict)]++;
}

// Read a little-endian u16 producer index from the status block at byte `off`.
static uint32_t rxProducer(IV * v, uint32_t off)
{
    const volatile uint8_t * p = v->status.va;
    return (static_cast<uint32_t>(p[off + 1]) << 8) | p[off];
}

// Drain one RX return ring from *consp up to hwProd: decode each BD, attach the
// hardware RX timestamp from its reserved field (Apple's path), copy the frame
// and repost its std buffer. Returns whether any std buffer was reposted.
static bool drainReturnRing(IV * v, RingTick * tick, const volatile RxBd * ret, uint32_t * consp,
                            uint32_t hwProd)
{
    bool reposted = false;
    uint32_t cons = *consp;
    // Reading the RX-CPU stamp ring goes through the config-space register
    // window, and a config-space indirect access landing in the ~13 us window
    // after a TX doorbell corrupts the MAC's TX_TSTAMP latch. So no ring read
    // while a timestamped TX is outstanding. harvestRx runs checkTx first, so
    // a latch that has already happened (the usual case: the peer's
    // Pdelay_Resp arrives after our Pdelay_Req went out) clears tsPending
    // before this; an event frame that still meets a pending TX is delivered
    // unstamped and counted (kAvbCtrStampSkipped). It is NOT picked up later:
    // the frame has left the ring by then.
    // tsPending is set on the TX queue; this read is a snapshot, and any
    // window access later in the drain still takes txLock and the doorbell
    // guard, so a timestamped send that lands meanwhile is kept clear of it.
    const bool stampReady = !__atomic_load_n(&v->tsPending, __ATOMIC_ACQUIRE) && v->rxcpuStamp;
    bool stampRefreshed = false;
    while (cons != hwProd && ok(v)) {
        RxBd bd;
        bd.addrHi = ret[cons].addrHi;
        bd.addrLo = ret[cons].addrLo;
        bd.idxLen = ret[cons].idxLen;
        bd.typeFlags = ret[cons].typeFlags;
        bd.ipTcpCsum = ret[cons].ipTcpCsum;
        bd.errVlan = ret[cons].errVlan;
        bd.reserved = ret[cons].reserved;
        bd.opaque = ret[cons].opaque;
        const RxReturn r = decodeRxReturn(bd);

        AvbRxEntry e{};
        e.harvest_mach = mach_absolute_time();
        e.type_flags = r.flags;
        // Apple's actual RX-timestamp path: the hardware writes the per-frame
        // low-32 timestamp into the RX BD reserved field (offset 0x18), full
        // time reconstructed against the EAV clock. The tg3 register pair
        // (0x6b0/0x6b4) is not wired on the 57766, so prefer the BD field.
        if (bd.reserved != 0) {
            e.hw_ts = reconstructRxTs(v, bd.reserved);
            e.has_ts = 1;
        } else if (r.hasTimestamp) {
            uint64_t ts = 0;
            if (readTs(v, bcm::kRxTstampLsb, bcm::kRxTstampMsb, &ts)) {
                e.hw_ts = ts & bcm::kTstampMask;
                e.has_ts = 1;
            }
        }
        if (r.fromStdRing && r.stdIndex < kRxPosted) {
            v->ctr.c[kAvbCtrRxFrames]++;
            if (r.error) {
                v->ctr.c[kAvbCtrRxErrors]++;
            } else {
                const uint8_t * frame = v->rxBufs.va + uint64_t(r.stdIndex) * kRxBufSize;
                const uint32_t flen = r.len < kRxBufSize ? r.len : kRxBufSize;
                e.len = flen;
                // Our own RX-timestamp path: attach the RX-CPU firmware's EAV
                // stamp for gPTP frames. This overrides the bd.reserved/MAC
                // latch path (neither works on this 57766-class part).
                if (stampReady) {
                    attachRxCpuStamp(v, frame, flen, e, &stampRefreshed);
                } else if (v->rxcpuStamp && isGptpEventFrame(frame, flen)) {
                    v->ctr.c[kAvbCtrStampSkipped]++;
                }
                if (tick->ringMode) {
                    deliverRing(v, tick, frame, flen, e);
                } else {
                    const uint32_t n = flen < AVB_RX_COPY_BYTES ? flen : AVB_RX_COPY_BYTES;
                    memcpy(e.data, frame, n);
                    swPush(v, e);
                }
            }
            postStdBuffer(v, r.stdIndex);
            reposted = true;
        } else {
            LOG("harvest: unexpected return BD opaque 0x%08x", bd.opaque);
        }
        cons = ringNext(cons, bcm::kRxRetRingSize);
    }
    *consp = cons;
    return reposted;
}

static void checkTx(IV * v);

static void harvestRx(IV * v)
{
    // Resolve a TX timestamp that has already latched BEFORE draining: the
    // drain skips stamping while tsPending, and the peer's reply to our
    // timestamped Pdelay_Req is usually in this very batch. BAR reads only.
    checkTx(v);
    uint8_t sb[0x14];
    readStatusBlock(v, sb);
    bool reposted = false;

    // Keep the software EAV high word tracking wraps even when no gPTP frame is
    // stamped this cycle (a BAR read, safe alongside TX). Skip while a TX
    // timestamp is pending so nothing competes with the latch window.
    {
        TxGuard g(v);
        if (!v->tsPending) {
            (void)eavSample(v);
        }
    }

    RingTick tick = ringTickBegin(v);
    const uint32_t prod0 = le16At(sb, bcm::kSbRxProducer0) & (bcm::kRxRetRingSize - 1);
    if (prod0 != v->retCons) {
        reposted |= drainReturnRing(v, &tick, reinterpret_cast<const volatile RxBd *>(v->retRing.va),
                                    &v->retCons, prod0);
    }

    // AVB realtime return ring 1: the classifier routes gPTP (0x88F7) frames
    // here and the hardware stamps BD.reserved. Its producer is the status
    // block's index-1 slot (0x18); it has its own consumer mailbox.
    if (v->retRing1.va != nullptr) {
        const uint32_t prod1 = rxProducer(v, bcm::kSbRxProducer1) & (bcm::kRxRetRingSize - 1);
        if (prod1 != v->retCons1) {
            reposted |= drainReturnRing(v, &tick, reinterpret_cast<const volatile RxBd *>(v->retRing1.va),
                                        &v->retCons1, prod1);
            __atomic_thread_fence(__ATOMIC_SEQ_CST);
            OSSynchronizeIO();
            wr(v, bcm::kMboxRxRetCons1, v->retCons1);
        }
    }
    if (tick.ringMode) {
        // Slots are written before the heads are released (§3.4 order).
        __atomic_store_n(&v->hdr->ctl_head, v->ctlHead, __ATOMIC_RELEASE);
        __atomic_store_n(&v->hdr->audio_head, v->audioHead, __ATOMIC_RELEASE);
    }

    __atomic_thread_fence(__ATOMIC_SEQ_CST);
    OSSynchronizeIO();
    wr(v, bcm::kMboxRxRetCons0, v->retCons);
    if (reposted) {
        wr(v, bcm::kMboxRxStdProd, v->stdProd);
    }
}

static void checkTxLocked(IV * v);

static void checkTx(IV * v)
{
    TxGuard g(v);
    checkTxLocked(v);
}

// Caller holds txLock.
static void checkTxLocked(IV * v)
{
    uint8_t sb[0x14];
    readStatusBlock(v, sb);
    v->txCons = le16At(sb, bcm::kSbTxConsumer0) & (bcm::kTxRingSize - 1);
    if (!v->tsPending) {
        return;
    }
    // tg3 compares TX_TSTAMP against the value read before the doorbell; the
    // register can only change once the timestamped frame has gone out.
    uint64_t ts = 0;
    if (readTs(v, bcm::kTxTstampLsb, bcm::kTxTstampMsb, &ts) && ts != v->tsPre) {
        v->tsValue = ts & bcm::kTstampMask;
        // Re-align the software EAV high word to this TX latch, the only source
        // of the true high 32 bits on this part, so RX reconstruction shares
        // the TX epoch.
        v->eavHi = static_cast<uint32_t>((ts & bcm::kTstampMask) >> 32);
        v->eavLastLo = static_cast<uint32_t>(ts);
        v->eavSeeded = true;
        v->tsMach = mach_absolute_time();
        v->tsStatus = kAvbTxTsOk;
        v->tsPending = false;
        return;
    }
    if (mach_absolute_time() > v->tsDeadline) {
        v->tsStatus = kAvbTxTsTimeout;
        v->tsPending = false;
        LOG("TX timestamp timeout for seq %llu", v->tsSeq);
    }
}

static void armTimer(IV * v)
{
    if (v->timer == nullptr) {
        return;
    }
    const uint64_t period = nsToMach(v, kTimerPeriodNs);
    v->timer->WakeAtTime(kIOTimerClockMachAbsoluteTime, mach_absolute_time() + period, period / 4);
    v->timerRunning = true;
}

// ---------------------------------------------------------------------------
// Datapath up / down

static void shutdownDatapath(IV * v)
{
    if (v->stage != kAvbStageDatapath) {
        return;
    }
    if (ok(v)) {
        abortHw(v);
    }
    setCommandBits(v, 0, bcm::kCmdBusMaster);
    dmaFreeAll(v);
    v->tsPending = false;
    v->flags &= ~(AVB_FLAG_LINK_UP | AVB_FLAG_1000FD);
    v->stage = kAvbStageTimeSync;
    LOG("datapath down (stage 1)");
}

static kern_return_t initDatapath(IV * v, uint64_t initFlags)
{
    if (!ok(v)) {
        return kIOReturnNotReady;
    }
    if (v->stage == kAvbStageDatapath) {
        return kIOReturnSuccess;
    }
    if (v->stage != kAvbStageTimeSync) {
        return kIOReturnNotReady;
    }
    if (v->flags & AVB_FLAG_APE) {
        LOG("datapath: APE firmware present; APE locking is out of Phase 0 scope");
        return kIOReturnUnsupported;
    }
    if ((v->flags & AVB_FLAG_SIG_OK) == 0) {
        setFault(v, kAvbFaultNoSignature, "NIC SRAM data signature missing; refusing core reset");
        return kIOReturnNotReady;
    }

    const bool allocated = dmaAlloc(v, &v->status, 4096) &&
                           dmaAlloc(v, &v->stdRing, bcm::kStdRingSize * sizeof(RxBd)) &&
                           dmaAlloc(v, &v->retRing, bcm::kRxRetRingSize * sizeof(RxBd)) &&
                           dmaAlloc(v, &v->retRing1, bcm::kRxRetRingSize * sizeof(RxBd)) &&
                           dmaAlloc(v, &v->txRing, bcm::kTxRingSize * sizeof(TxBd)) &&
                           dmaAlloc(v, &v->rxBufs, uint64_t(kRxPosted) * kRxBufSize) &&
                           dmaAlloc(v, &v->txBufs, uint64_t(kTxSlots) * kTxBufSize);
    if (!allocated) {
        dmaFreeAll(v);
        setFault(v, kAvbFaultDma, "DMA buffer setup failed");
        return kIOReturnNoMemory;
    }

    setCommandBits(v, bcm::kCmdMemSpace | bcm::kCmdBusMaster, 0);
    v->stage = kAvbStageDatapath;  // so a failure below runs the shutdown path
    v->swHead = v->swTail = v->swDropped = 0;
    v->txSeq = 0;
    v->lastDoorbellMach = 0;
    v->tsPending = false;
    v->tsSeq = 0;
    v->rxcpuStamp = (initFlags & AVB_INIT_FLAG_RXCPU_STAMP) != 0;
    resetStampCache(v);
    v->eavHi = 0;
    v->eavLastLo = 0;
    v->eavSeeded = false;

    LOG("datapath: core reset + init (flags 0x%llx)", initFlags);
    if (!resetHw(v, initFlags)) {
        LOG("datapath: init failed (fault %u)", v->fault);
        setCommandBits(v, 0, bcm::kCmdBusMaster);
        dmaFreeAll(v);
        v->stage = kAvbStageTimeSync;
        return kIOReturnIOError;
    }

    // The core reset may clear the EAV block: re-enable and re-arm the clock.
    wr(v, bcm::kGrcMode, rd(v, bcm::kGrcMode) | bcm::kGrcTimeSyncEnable);
    uint64_t nowNs = mach_absolute_time();
    if (v->tb.denom != 0) {
        nowNs = nowNs * v->tb.numer / v->tb.denom;
    }
    setClock(v, nowNs);

    if (v->timer != nullptr) {
        v->timer->SetEnable(true);
    }
    armTimer(v);
    LOG("datapath up (stage 2), link_up=%d", (v->flags & AVB_FLAG_LINK_UP) != 0);
    return ok(v) ? kIOReturnSuccess : kIOReturnIOError;
}

static kern_return_t sendFrame(IV * v, const uint8_t * frame, uint32_t len, bool wantTs,
                               uint64_t * seqOut, uint64_t * machOut)
{
    if (!ok(v) || v->stage != kAvbStageDatapath) {
        return kIOReturnNotReady;
    }
    if (len < 14 || len > AVB_FRAME_MAX_LEN) {
        return kIOReturnBadArgument;
    }
    checkTxLocked(v);
    if (wantTs && v->tsPending) {
        return kIOReturnBusy;
    }
    if (ringDistance(v->txCons, v->txProd, bcm::kTxRingSize) >= kTxSlots) {
        return kIOReturnBusy;
    }
    const uint32_t slot = v->txProd % kTxSlots;
    uint8_t * buf = v->txBufs.va + uint64_t(slot) * kTxBufSize;
    memcpy(buf, frame, len);
    uint32_t padded = len;
    if (padded < AVB_FRAME_MIN_LEN) {
        memset(buf + len, 0, AVB_FRAME_MIN_LEN - len);
        padded = AVB_FRAME_MIN_LEN;
    }

    uint64_t pre = 0;
    if (wantTs && !readTs(v, bcm::kTxTstampLsb, bcm::kTxTstampMsb, &pre)) {
        return kIOReturnIOError;
    }

    auto * ring = reinterpret_cast<TxBd *>(v->txRing.va);
    ring[v->txProd] = makeTxBd(v->txBufs.iova + uint64_t(slot) * kTxBufSize, padded, wantTs);
    v->txProd = ringNext(v->txProd, bcm::kTxRingSize);
    __atomic_thread_fence(__ATOMIC_SEQ_CST);
    OSSynchronizeIO();

    const uint64_t mach = mach_absolute_time();
    wrFlush(v, bcm::kMboxTxHostProd0, v->txProd);
    v->lastDoorbellMach = mach_absolute_time();

    const uint64_t seq = ++v->txSeq;
    v->ctr.c[kAvbCtrTxFrames]++;
    if (wantTs) {
        v->ctr.c[kAvbCtrTxTsRequested]++;
        v->tsPending = true;
        v->tsSeq = seq;
        v->tsPre = pre;
        v->tsDeadline = mach + nsToMach(v, kTxTsTimeoutNs);
        v->tsStatus = kAvbTxTsPending;
        v->tsValue = 0;
        v->tsMach = 0;
    }
    *seqOut = seq;
    *machOut = mach;
    return ok(v) ? kIOReturnSuccess : kIOReturnIOError;
}

// kAvbSelSendFrames (ABI v4): queue up to AVB_TX_BATCH_MAX untimestamped frames
// with ONE doorbell and one user-client round trip. The talker sends one frame
// per stream every 125 us; per-frame SendFrame calls (~20 000/s) kept the
// driver queue busy enough to miss most observation intervals and to delay the
// 1 ms RX harvest. Frames are queued in order until the in-flight window is
// full; `acceptedOut` says how many went in, so the client re-paces the rest.
// The batch layout is validated by the shared parseTxBatch before any frame is
// touched (malformed -> the whole call is refused).
static kern_return_t sendFrames(IV * v, const uint8_t * in, uint32_t inLen, uint64_t * acceptedOut,
                                uint64_t * machOut)
{
    *acceptedOut = 0;
    *machOut = 0;
    if (!ok(v) || v->stage != kAvbStageDatapath) {
        return kIOReturnNotReady;
    }
    const TxBatchPlan plan = parseTxBatch(in, inLen);
    if (!plan.valid) {
        v->ctr.c[kAvbCtrTxBatchRejected]++;
        return kIOReturnBadArgument;
    }
    checkTxLocked(v);
    auto * ring = reinterpret_cast<TxBd *>(v->txRing.va);
    uint32_t accepted = 0;
    bool refused = false;
    for (uint32_t i = 0; i < plan.count; i++) {
        if (ringDistance(v->txCons, v->txProd, bcm::kTxRingSize) >= kTxSlots) {
            refused = true;
            break;
        }
        const uint32_t len = plan.f[i].len;
        const uint32_t slot = v->txProd % kTxSlots;
        uint8_t * buf = v->txBufs.va + uint64_t(slot) * kTxBufSize;
        memcpy(buf, in + plan.f[i].offset, len);
        uint32_t padded = len;
        if (padded < AVB_FRAME_MIN_LEN) {
            memset(buf + len, 0, AVB_FRAME_MIN_LEN - len);
            padded = AVB_FRAME_MIN_LEN;
        }
        ring[v->txProd] = makeTxBd(v->txBufs.iova + uint64_t(slot) * kTxBufSize, padded, false);
        v->txProd = ringNext(v->txProd, bcm::kTxRingSize);
        accepted++;
    }
    if (accepted == 0) {
        // Nothing queued: no doorbell, not an error -- the client backs off.
        return ok(v) ? kIOReturnSuccess : kIOReturnIOError;
    }
    __atomic_thread_fence(__ATOMIC_SEQ_CST);
    OSSynchronizeIO();
    const uint64_t mach = mach_absolute_time();
    wrFlush(v, bcm::kMboxTxHostProd0, v->txProd);
    v->lastDoorbellMach = mach_absolute_time();

    v->txSeq += accepted;
    v->ctr.c[kAvbCtrTxFrames] += accepted;
    v->ctr.c[kAvbCtrTxBatches]++;
    if (refused) {
        v->ctr.c[kAvbCtrTxBatchPartial]++;
    }
    *acceptedOut = accepted;
    *machOut = mach;
    return ok(v) ? kIOReturnSuccess : kIOReturnIOError;
}

// ---------------------------------------------------------------------------
// IOService

// ---------------------------------------------------------------------------
// Shared rings (ABI v3, same layout and rules as the I210 dext). Only the
// rings confer ownership; PollRx/legacy callers keep working when unattached.

static void storeRel32(uint32_t * p, uint32_t x) { __atomic_store_n(p, x, __ATOMIC_RELEASE); }

// Live header fields, refreshed every harvest tick on the driver queue.
static void publishHeader(IV * v, uint64_t tickMach)
{
    v->producerRunning = producerRunning(v->hdr != nullptr, v->stage, v->fault);
    if (v->hdr == nullptr) {
        return;
    }
    __atomic_store_n(&v->hdr->last_tick_mach, tickMach, __ATOMIC_RELEASE);
    storeRel32(&v->hdr->stage, v->stage);
    storeRel32(&v->hdr->fault, v->fault);
    storeRel32(&v->hdr->flags, v->flags);
    storeRel32(&v->hdr->producer_running, v->producerRunning ? 1u : 0u);
    for (uint32_t i = 0; i < kAvbCtrCount; i++) {
        __atomic_store_n(&v->hdr->counters.c[i], v->ctr.c[i], __ATOMIC_RELAXED);
    }
}

// Zeroed, client-mappable memory of exactly `len` bytes.
static uint8_t * allocShared(uint64_t len, IOBufferMemoryDescriptor ** out)
{
    *out = nullptr;
    IOBufferMemoryDescriptor * md = nullptr;
    kern_return_t ret = IOBufferMemoryDescriptor::Create(kIOMemoryDirectionInOut, len, 16384, &md);
    if (ret != kIOReturnSuccess || md == nullptr) {
        LOG("rings: IOBufferMemoryDescriptor::Create(%llu) failed 0x%x", len, ret);
        return nullptr;
    }
    ret = md->SetLength(len);
    IOAddressSegment range{};
    if (ret == kIOReturnSuccess) {
        ret = md->GetAddressRange(&range);
    }
    if (ret != kIOReturnSuccess || range.address == 0 || range.length < len) {
        LOG("rings: shared buffer setup (%llu) failed 0x%x", len, ret);
        OSSafeReleaseNULL(md);
        return nullptr;
    }
    auto * va = reinterpret_cast<uint8_t *>(range.address);
    memset(va, 0, len);
    *out = md;
    return va;
}

// A client's existing mapping holds its own reference to the memory.
static void releaseRings(IV * v)
{
    v->producerRunning = false;
    v->hdr = nullptr;
    v->cons = nullptr;
    v->rxVa = nullptr;
    OSSafeReleaseNULL(v->rxMem);
    OSSafeReleaseNULL(v->consumerMem);
}

static kern_return_t ringAttach(IV * v, AVBBcmUserClient * client, uint64_t flags, uint64_t * out)
{
    if (flags != 0) {
        return kIOReturnBadArgument;
    }
    if (v->stage < kAvbStageTimeSync) {
        return kIOReturnNotReady;
    }
    if (v->owner != nullptr && v->owner != client) {
        return kIOReturnExclusiveAccess;
    }
    if (v->owner == client && v->hdr != nullptr) {  // idempotent for the owner
        out[0] = v->generation;
        out[1] = AVB_RX_MEM_SIZE;
        out[2] = AVB_CONSUMER_MEM_SIZE;
        return kIOReturnSuccess;
    }
    if (client->RingsUsed()) {
        LOG("rings: reattach on same connection; reopen");
        return kIOReturnNotPermitted;
    }
    IOBufferMemoryDescriptor * rxMd = nullptr;
    IOBufferMemoryDescriptor * consMd = nullptr;
    uint8_t * rxVa = allocShared(AVB_RX_MEM_SIZE, &rxMd);
    uint8_t * consVa = rxVa != nullptr ? allocShared(AVB_CONSUMER_MEM_SIZE, &consMd) : nullptr;
    if (rxVa == nullptr || consVa == nullptr) {
        OSSafeReleaseNULL(rxMd);
        OSSafeReleaseNULL(consMd);
        LOG("rings: attach failed: no memory");
        return kIOReturnNoMemory;
    }
    auto * h = reinterpret_cast<AvbShmHeader *>(rxVa);
    h->magic = AVB_SHM_MAGIC;
    h->abi_version = AVB_ABI_VERSION;
    h->header_size = AVB_SHM_HEADER_SIZE;
    h->slot_size = AVB_RX_SLOT_SIZE;
    h->ctl_slots = AVB_RX_CTL_SLOTS;
    h->audio_slots = AVB_RX_AUDIO_SLOTS;
    h->ctl_offset = AVB_RX_CTL_OFFSET;
    h->audio_offset = AVB_RX_AUDIO_OFFSET;
    h->total_size = AVB_RX_MEM_SIZE;
    h->generation = ++v->generation;
    h->stage = v->stage;
    h->fault = v->fault;
    h->producer_running = 0;
    memcpy(h->mac, v->mac, sizeof(h->mac));
    h->clock_gen = 0;
    __atomic_thread_fence(__ATOMIC_RELEASE);

    v->rxMem = rxMd;
    v->consumerMem = consMd;
    v->rxVa = rxVa;
    v->hdr = h;
    v->cons = reinterpret_cast<AvbShmConsumer *>(consVa);
    v->ctlHead = 0;
    v->audioHead = 0;
    memset(&v->filter, 0, sizeof(v->filter));
    v->owner = client;
    client->MarkRingsUsed();
    v->flags |= AVB_FLAG_RINGS_ATTACHED | AVB_FLAG_OWNED;
    publishHeader(v, mach_absolute_time());  // producer runs once the datapath is up
    LOG("rings: attached generation %u (stage %u, producer %d)", v->generation, v->stage, v->producerRunning);
    out[0] = v->generation;
    out[1] = AVB_RX_MEM_SIZE;
    out[2] = AVB_CONSUMER_MEM_SIZE;
    return kIOReturnSuccess;
}

static void detachRings(IV * v)
{
    v->producerRunning = false;
    if (v->hdr != nullptr) {
        storeRel32(&v->hdr->producer_running, 0);
    }
    memset(&v->filter, 0, sizeof(v->filter));
    v->flags &= ~(AVB_FLAG_RINGS_ATTACHED | AVB_FLAG_OWNED);
    if (v->hdr != nullptr) {
        storeRel32(&v->hdr->flags, v->flags);
    }
    releaseRings(v);
    v->owner = nullptr;
}

static kern_return_t ringDetach(IV * v, AVBBcmUserClient * client)
{
    if (v->hdr == nullptr) {
        return kIOReturnNotReady;
    }
    if (client != v->owner) {
        return kIOReturnExclusiveAccess;
    }
    detachRings(v);
    LOG("rings: detached generation %u", v->generation);
    return kIOReturnSuccess;
}

kern_return_t
AVBBcmDriver::CopyRingMemory(AVBBcmUserClient * client, uint64_t type, uint64_t * options,
                             IOMemoryDescriptor ** memory)
{
    IV * v = ivars;
    if (v->hdr == nullptr) {
        return kIOReturnNotReady;
    }
    if (client != v->owner) {
        return kIOReturnExclusiveAccess;
    }
    switch (type) {
    case kAvbMemRx:
        v->rxMem->retain();  // one reference is consumed by the caller
        *memory = v->rxMem;
        *options = kIOUserClientMemoryReadOnly;
        return kIOReturnSuccess;
    case kAvbMemConsumer:
        v->consumerMem->retain();
        *memory = v->consumerMem;
        *options = 0;
        return kIOReturnSuccess;
    default:
        return kIOReturnBadArgument;
    }
}

// The owning connection went away: stop the producer, take the datapath down
// and release the rings, in the I210's §3.3 order.
IODispatchQueue *
AVBBcmDriver::CopyTxQueue()
{
    IODispatchQueue * q = ivars != nullptr ? ivars->txQueue : nullptr;
    if (q != nullptr) {
        q->retain();
    }
    return q;
}

void
AVBBcmDriver::ClientGone(AVBBcmUserClient * client)
{
    IV * v = ivars;
    TxGuard g(v);
    if (client == nullptr || client != v->owner) {
        return;
    }
    // Release the rings but leave the datapath up. Its RX/TX DMA targets the
    // driver's own buffers, never the shared mapping, so a running datapath
    // with no owner is safe (it falls back to the PollRx software ring). An
    // abortHw here latched "block stop timeout" on hardware with the RX-CPU
    // stamper loaded, and a latched fault needs a replug to clear.
    v->producerRunning = false;
    v->tsPending = false;
    detachRings(v);
    LOG("uc: closed (owner): rings of generation %u released, datapath left up", v->generation);
}

bool
AVBBcmDriver::init()
{
    if (!super::init()) {
        return false;
    }
    ivars = IONewZero(AVBBcmDriver_IVars, 1);
    if (ivars == nullptr) {
        return false;
    }
    ivars->sw = IONewZero(AvbRxEntry, kSwRingSize);
    ivars->batch = IONewZero(AvbRxBatch, 1);
    ivars->txLock = IOLockAlloc();
    return ivars->sw != nullptr && ivars->batch != nullptr && ivars->txLock != nullptr;
}

void
AVBBcmDriver::free()
{
    if (ivars != nullptr) {
        dmaFreeAll(ivars);
        OSSafeReleaseNULL(ivars->timerAction);
        OSSafeReleaseNULL(ivars->timer);
        OSSafeReleaseNULL(ivars->queue);
        OSSafeReleaseNULL(ivars->txQueue);
        releaseRings(ivars);
        IOLockFreeZero(ivars->txLock);
        IOSafeDeleteNULL(ivars->sw, AvbRxEntry, kSwRingSize);
        IOSafeDeleteNULL(ivars->batch, AvbRxBatch, 1);
    }
    IOSafeDeleteNULL(ivars, AVBBcmDriver_IVars, 1);
    super::free();
}

kern_return_t
IMPL(AVBBcmDriver, Start)
{
    kern_return_t ret = Start(provider, SUPERDISPATCH);
    if (ret != kIOReturnSuccess) {
        return ret;
    }
    IV * v = ivars;
    v->pci = OSDynamicCast(IOPCIDevice, provider);
    if (v->pci == nullptr) {
        LOG("provider is not an IOPCIDevice");
        return kIOReturnNoDevice;
    }
    ret = v->pci->Open(this, 0);
    if (ret != kIOReturnSuccess) {
        LOG("IOPCIDevice::Open failed 0x%x", ret);
        return ret;
    }
    v->opened = true;

    // Defence in depth: never touch anything that is not our exact device.
    v->pci->ConfigurationRead32(bcm::kCfgVendorDevice, &v->vendorDevice);
    v->pci->ConfigurationRead32(bcm::kCfgSubsystem, &v->subsys);
    if (!isOurDevice(v->vendorDevice, v->subsys)) {
        LOG("refusing device %08x subsys %08x (only 168214e4 / 00f6106b)", v->vendorDevice, v->subsys);
        v->fault = kAvbFaultIdMismatch;
        v->pci->Close(this, 0);
        v->opened = false;
        return kIOReturnUnsupported;
    }
    LOG("start: %08x subsys %08x", v->vendorDevice, v->subsys);

    uint64_t barSize = 0;
    uint8_t barType = 0;
    ret = v->pci->GetBARInfo(kPCIMemoryRangeBAR0, &v->bar, &barSize, &barType);
    if (ret != kIOReturnSuccess || barSize < 0x10000) {
        LOG("BAR0 unusable (ret 0x%x size 0x%llx)", ret, barSize);
        v->pci->Close(this, 0);
        v->opened = false;
        return ret != kIOReturnSuccess ? ret : kIOReturnNoResources;
    }
    if (v->pci->FindPCICapability(kIOPCICapabilityIDPCIExpress, 0, &v->pcieCap) != kIOReturnSuccess) {
        v->pcieCap = 0;
    }
    mach_timebase_info(&v->tb);

    setCommandBits(v, bcm::kCmdMemSpace | bcm::kCmdBusMaster, 0);

    ret = CopyDispatchQueue(kIOServiceDefaultQueueName, &v->queue);
    if (ret == kIOReturnSuccess) {
        ret = IODispatchQueue::Create("AVBBcmTx", 0, 0, &v->txQueue);
    }
    if (ret == kIOReturnSuccess) {
        ret = IOTimerDispatchSource::Create(v->queue, &v->timer);
    }
    if (ret == kIOReturnSuccess) {
        ret = CreateActionTimerFired(0, &v->timerAction);
    }
    if (ret == kIOReturnSuccess) {
        ret = v->timer->SetHandler(v->timerAction);
    }
    if (ret != kIOReturnSuccess) {
        LOG("timer setup failed 0x%x", ret);
        v->pci->Close(this, 0);
        v->opened = false;
        return ret;
    }

    ret = RegisterService();
    if (ret != kIOReturnSuccess) {
        LOG("RegisterService failed 0x%x", ret);
        v->pci->Close(this, 0);
        v->opened = false;
        return ret;
    }

    takeSnapshot(v);
    stageA(v);
    return kIOReturnSuccess;
}

kern_return_t
IMPL(AVBBcmDriver, Stop)
{
    IV * v = ivars;
    LOG("stop (stage %u fault %u)", v->stage, v->fault);
    if (v->timer != nullptr) {
        v->timer->SetEnable(false);
    }
    v->timerRunning = false;
    if (v->opened) {
        TxGuard g(v);
        shutdownDatapath(v);
        setCommandBits(v, 0, bcm::kCmdBusMaster);
        v->pci->Close(this, 0);
        v->opened = false;
    }
    dmaFreeAll(v);
    return Stop(provider, SUPERDISPATCH);
}

kern_return_t
IMPL(AVBBcmDriver, NewUserClient)
{
    IOService * client = nullptr;
    kern_return_t ret = Create(this, "UserClientProperties", &client);
    if (ret != kIOReturnSuccess) {
        LOG("NewUserClient: Create failed 0x%x", ret);
        return ret;
    }
    *userClient = OSDynamicCast(IOUserClient, client);
    if (*userClient == nullptr) {
        client->release();
        return kIOReturnError;
    }
    return kIOReturnSuccess;
}

void
IMPL(AVBBcmDriver, TimerFired)
{
    (void)action;
    (void)time;
    IV * v = ivars;
    v->timerRunning = false;
    publishHeader(v, mach_absolute_time());  // first, every tick
    if (v->stage != kAvbStageDatapath || !ok(v)) {
        return;
    }
    v->ctr.c[kAvbCtrHarvestTicks]++;
    // Timed: this runs on the queue that also serves SendFrames, so a long
    // harvest is time a talker's batch spends waiting.
    const uint64_t t0 = mach_absolute_time();
    harvestRx(v);
    checkTx(v);
    const uint64_t us = machToUs(v, mach_absolute_time() - t0);
    if (us > 500) {
        v->ctr.c[kAvbCtrHarvestSlowTicks]++;
    }
    if (us > v->ctr.c[kAvbCtrHarvestMaxUs]) {
        v->ctr.c[kAvbCtrHarvestMaxUs] = us;
    }
    if (ok(v) && v->stage == kAvbStageDatapath) {
        armTimer(v);
    }
}

// ---------------------------------------------------------------------------
// User client methods (selectors in AVBShared.h)

static kern_return_t setStructOut(IOUserClientMethodArguments * a, const void * p, size_t len)
{
    if (a->structureOutputMaximumSize < len) {
        return kIOReturnBadArgument;
    }
    a->structureOutput = OSData::withBytes(p, len);
    return a->structureOutput != nullptr ? kIOReturnSuccess : kIOReturnNoMemory;
}

// Selectors that change the device, or touch the config-space window (which
// can corrupt a pending TX timestamp latch). While a client owns the rings,
// only that client may issue them: a second connection could otherwise shut
// the datapath down under the daemon, drain its frames, or load code onto the
// DMA-capable RX CPU. Same rule as the I210 dext, plus the RX-CPU selectors.
static bool isOwnerOnly(uint64_t selector)
{
    switch (selector) {
    case kAvbSelSetClock:
    case kAvbSelAdjFreq:
    case kAvbSelInitDatapath:
    case kAvbSelPollRx:
    case kAvbSelSendFrame:
    case kAvbSelSendFrames:
    case kAvbSelGetTxTimestamp:
    case kAvbSelShutdownDatapath:
    case kAvbSelLoadCoreImage:
    case kAvbSelReadCoreMem:
        return true;
    default:
        return false;
    }
}

kern_return_t
AVBBcmDriver::HandleUserMethod(AVBBcmUserClient * client, uint64_t selector, IOUserClientMethodArguments * a)
{
    IV * v = ivars;
    // Locking: every selector runs under txLock for its whole body, except
    // those whose helpers take it themselves (config-window access, and the
    // TX-timestamp poll). SendFrame/SendFrames arrive on txQueue; everything
    // else on the default queue, where the 1 ms harvest takes the lock only
    // in short pieces.
    const bool selfLocking = selector == kAvbSelLoadCoreImage || selector == kAvbSelReadCoreMem ||
                             selector == kAvbSelGetTxTimestamp;
    // A send announces itself before it waits, so the harvest yields the lock
    // to it between window words (yieldToTx). Timed from here, so the
    // SendFrames maximum includes any lock wait.
    const bool txSel = selector == kAvbSelSendFrame || selector == kAvbSelSendFrames;
    const uint64_t callStart = mach_absolute_time();
    if (txSel) {
        __atomic_add_fetch(&v->txWaiting, 1, __ATOMIC_ACQ_REL);
    }
    TxGuard g(v, !selfLocking);
    if (txSel) {
        __atomic_sub_fetch(&v->txWaiting, 1, __ATOMIC_ACQ_REL);
    }
    if (isOwnerOnly(selector) && v->owner != nullptr && client != v->owner) {
        return kIOReturnExclusiveAccess;
    }
    switch (selector) {
    case kAvbSelGetInfo: {
        AvbInfo info{};
        info.abi_version = AVB_ABI_VERSION;
        info.vendor_device = v->vendorDevice;
        info.subsys = v->subsys;
        info.hw_rev = v->asicRev;
        info.nic_kind = AVB_NIC_BCM57762;
        if (ok(v)) {
            v->pci->ConfigurationRead32(bcm::kMiscHostCtrl, &info.ctrl_a);
            info.ctrl_b = rd(v, bcm::kGrcMode);
        }
        if (ok(v)) {
            info.clock_ctl = rd(v, bcm::kEavRefClkCorrectCtl);
        }
        info.stage = v->stage;
        info.fault = v->fault;
        info.flags = v->flags;
        memcpy(info.mac, v->mac, sizeof(info.mac));
        info.tb_numer = v->tb.numer;
        info.tb_denom = v->tb.denom;
        return setStructOut(a, &info, sizeof(info));
    }

    case kAvbSelReadClock: {
        if (!ok(v) || v->stage < kAvbStageTimeSync) {
            return kIOReturnNotReady;
        }
        const uint64_t n = a->scalarInput[0];
        if (n < 1 || n > AVB_READ_CLOCK_MAX_SAMPLES) {
            return kIOReturnBadArgument;
        }
        AvbClockSample s[AVB_READ_CLOCK_MAX_SAMPLES];
        for (uint64_t i = 0; i < n; i++) {
            s[i].mach_before = mach_absolute_time();
            uint64_t ns = 0;
            if (!readTs(v, bcm::kEavRefClkLsb, bcm::kEavRefClkMsb, &ns)) {
                return kIOReturnIOError;
            }
            s[i].mach_after = mach_absolute_time();
            s[i].eav_ns = ns;
        }
        return setStructOut(a, s, n * sizeof(AvbClockSample));
    }

    case kAvbSelSetClock: {
        if (!ok(v) || v->stage < kAvbStageTimeSync) {
            return kIOReturnNotReady;
        }
        setClock(v, a->scalarInput[0]);
        // The software RX high word is only re-learned from a TX latch; after
        // a clock write it may be in the wrong epoch, so stop stamping until
        // the next latch reseeds it (attachRxCpuStamp checks eavSeeded).
        v->eavSeeded = false;
        uint64_t back = 0;
        if (!readTs(v, bcm::kEavRefClkLsb, bcm::kEavRefClkMsb, &back)) {
            return kIOReturnIOError;
        }
        a->scalarOutput[0] = back;
        return kIOReturnSuccess;
    }

    case kAvbSelAdjFreq: {
        if (!ok(v) || v->stage < kAvbStageTimeSync) {
            return kIOReturnNotReady;
        }
        const int64_t ppb = static_cast<int64_t>(a->scalarInput[0]);
        if (ppb > AVB_ADJ_FREQ_MAX_PPB || ppb < -AVB_ADJ_FREQ_MAX_PPB) {
            return kIOReturnBadArgument;
        }
        const uint32_t reg = eavCorrectionFromPpb(ppb);
        wr(v, bcm::kEavRefClkCorrectCtl, reg);
        a->scalarOutput[0] = reg;
        return ok(v) ? kIOReturnSuccess : kIOReturnIOError;
    }

    case kAvbSelInitDatapath: {
        const kern_return_t ret = initDatapath(v, a->scalarInput[0]);
        a->scalarOutput[0] = v->stage;
        return ret;
    }

    case kAvbSelPollRx: {
        if (v->stage != kAvbStageDatapath) {
            return kIOReturnNotReady;
        }
        if (ok(v)) {
            harvestRx(v);
        }
        const uint16_t filter = static_cast<uint16_t>(a->scalarInput[0]);
        uint64_t max = a->scalarInput[1];
        if (max == 0 || max > AVB_RX_BATCH_MAX) {
            max = AVB_RX_BATCH_MAX;
        }
        AvbRxBatch * b = v->batch;
        memset(b, 0, sizeof(*b));
        while (v->swTail != v->swHead && b->count < max) {
            const AvbRxEntry & e = v->sw[v->swTail];
            const uint32_t copied = e.len < AVB_RX_COPY_BYTES ? e.len : AVB_RX_COPY_BYTES;
            if (filter == 0 || frameEthertype(e.data, copied) == filter) {
                b->e[b->count++] = e;
            }
            v->swTail = ringNext(v->swTail, kSwRingSize);
        }
        b->dropped = v->swDropped;
        v->swDropped = 0;
        return setStructOut(a, b, sizeof(*b));
    }

    case kAvbSelSendFrame: {
        if (a->structureInput == nullptr) {
            return kIOReturnBadArgument;  // frames are <= 1514 B, always inline
        }
        const auto * bytes = static_cast<const uint8_t *>(a->structureInput->getBytesNoCopy());
        const size_t len = a->structureInput->getLength();
        if (bytes == nullptr || len > AVB_FRAME_MAX_LEN) {
            return kIOReturnBadArgument;
        }
        uint64_t seq = 0;
        uint64_t mach = 0;
        const kern_return_t ret =
            sendFrame(v, bytes, static_cast<uint32_t>(len), a->scalarInput[0] != 0, &seq, &mach);
        a->scalarOutput[0] = seq;
        a->scalarOutput[1] = mach;
        return ret;
    }

    case kAvbSelSendFrames: {
        if (a->structureInput == nullptr) {
            return kIOReturnBadArgument;
        }
        const auto * bytes = static_cast<const uint8_t *>(a->structureInput->getBytesNoCopy());
        const size_t len = a->structureInput->getLength();
        if (bytes == nullptr || len > AVB_TX_BATCH_BYTES_MAX) {
            v->ctr.c[kAvbCtrTxBatchRejected]++;
            return kIOReturnBadArgument;
        }
        uint64_t accepted = 0;
        uint64_t mach = 0;
        const kern_return_t ret = sendFrames(v, bytes, static_cast<uint32_t>(len), &accepted, &mach);
        const uint64_t us = machToUs(v, mach_absolute_time() - callStart);
        if (us > v->ctr.c[kAvbCtrSendBatchMaxUs]) {
            v->ctr.c[kAvbCtrSendBatchMaxUs] = us;
        }
        a->scalarOutput[0] = accepted;
        a->scalarOutput[1] = mach;
        return ret;
    }

    case kAvbSelGetTxTimestamp: {
        TxGuard tg(v);  // self-locking selector: check and read in one hold
        if (ok(v) && v->stage == kAvbStageDatapath) {
            checkTxLocked(v);
        }
        const uint64_t seq = a->scalarInput[0];
        if (seq == 0 || seq != v->tsSeq) {
            a->scalarOutput[0] = kAvbTxTsNotRequested;
            a->scalarOutput[1] = 0;
            a->scalarOutput[2] = 0;
            return kIOReturnSuccess;
        }
        a->scalarOutput[0] = v->tsStatus;
        a->scalarOutput[1] = v->tsValue;
        a->scalarOutput[2] = v->tsMach;
        return kIOReturnSuccess;
    }

    case kAvbSelReadReg32: {
        if (!ok(v)) {
            return kIOReturnNotReady;
        }
        const uint64_t off = a->scalarInput[0];
        if ((off & kReadSnapshotFlag) != 0) {
            // The Start snapshot, not the live register.
            const uint64_t o = off & ~kReadSnapshotFlag;
            if (o == 0xffffffffull) {
                // Pseudo-offset: config-space MISC_HOST_CTRL as captured.
                a->scalarOutput[0] = v->snapMiscHostCtrl;
                return v->snapValid ? kIOReturnSuccess : kIOReturnNotReady;
            }
            const int32_t idx = o < 0x100000000ull ? snapIndex(static_cast<uint32_t>(o)) : -1;
            if (idx < 0) {
                return kIOReturnBadArgument;
            }
            if (!v->snapValid) {
                return kIOReturnNotReady;
            }
            a->scalarOutput[0] = v->snap[idx];
            return kIOReturnSuccess;
        }
        if (!isReadableRegOffset(off)) {
            return kIOReturnBadArgument;
        }
        a->scalarOutput[0] = rdRaw(v, static_cast<uint32_t>(off));
        return kIOReturnSuccess;
    }

    case kAvbSelShutdownDatapath:
        shutdownDatapath(v);
        return kIOReturnSuccess;

    case kAvbSelLoadCoreImage: {
        if (a->structureInput == nullptr) {
            return kIOReturnBadArgument;
        }
        const auto * bytes = static_cast<const uint8_t *>(a->structureInput->getBytesNoCopy());
        const size_t len = a->structureInput->getLength();
        if (bytes == nullptr || len == 0 || len > bcm::kRxCpuScratchSize) {
            return kIOReturnBadArgument;
        }
        uint64_t halted = 0;
        uint64_t mode = 0;
        // A new image restarts the firmware's ring and DONE signature.
        resetStampCache(v);
        const kern_return_t ret = loadCoreImage(v, static_cast<uint32_t>(a->scalarInput[0]), bytes,
                                                 static_cast<uint32_t>(len), &halted, &mode);
        a->scalarOutput[0] = halted;
        a->scalarOutput[1] = mode;
        return ret;
    }

    case kAvbSelReadCoreMem: {
        uint64_t word = 0;
        const kern_return_t ret = readCoreMem(v, static_cast<uint32_t>(a->scalarInput[0]), &word);
        a->scalarOutput[0] = word;
        return ret;
    }

    case kAvbSelReadPhy: {
        // One clause-22 MDIO read of the internal PHY. Read-only: the only
        // register written is MAC_MI_COM, the MAC's MDIO command register,
        // exactly as setupLink's existing BMCR/BMSR/AUX_STAT reads do.
        if (!ok(v)) {
            return kIOReturnNotReady;
        }
        const uint64_t reg = a->scalarInput[0];
        if (reg > 31) {
            return kIOReturnBadArgument;
        }
        uint16_t val = 0;
        if (!miRead(v, static_cast<uint32_t>(reg), &val)) {
            return kIOReturnIOError;
        }
        a->scalarOutput[0] = val;
        return kIOReturnSuccess;
    }

    case kAvbSelReadPhyMmd: {
        // One clause-45 register read through the clause-22 MMD access
        // registers (IEEE 802.3 Annex 22D; tg3 behaviour reference): write
        // MMD_CTRL (13) = devad, MMD_ADDR (14) = reg, MMD_CTRL = 0x4000|devad
        // (data, no increment), read 14. Only registers 13/14 are written;
        // 13 is restored to the value it held before (logged).
        if (!ok(v)) {
            return kIOReturnNotReady;
        }
        const uint64_t devad = a->scalarInput[0];
        const uint64_t reg = a->scalarInput[1];
        if (devad > 31 || reg > 0xffff) {
            return kIOReturnBadArgument;
        }
        constexpr uint32_t kMmdCtrl = 13;
        constexpr uint32_t kMmdAddr = 14;
        uint16_t saved = 0;
        if (!miRead(v, kMmdCtrl, &saved)) {
            return kIOReturnIOError;
        }
        uint16_t val = 0;
        const bool okRead = miWrite(v, kMmdCtrl, static_cast<uint16_t>(devad)) &&
                            miWrite(v, kMmdAddr, static_cast<uint16_t>(reg)) &&
                            miWrite(v, kMmdCtrl, static_cast<uint16_t>(0x4000 | devad)) &&
                            miRead(v, kMmdAddr, &val);
        const bool okRestore = miWrite(v, kMmdCtrl, saved);
        LOG("phy mmd read %llu.0x%04llx = 0x%04x (reg13 was 0x%04x, restored %d)", devad, reg, val, saved,
            okRestore);
        if (!okRead) {
            return kIOReturnIOError;
        }
        a->scalarOutput[0] = val;
        return kIOReturnSuccess;
    }

    case kAvbSelReadSram: {
        if (!ok(v)) {
            return kIOReturnNotReady;
        }
        a->scalarOutput[0] = sramRead(v, static_cast<uint32_t>(a->scalarInput[0]));
        return ok(v) ? kIOReturnSuccess : kIOReturnIOError;
    }

    case kAvbSelGetCounters:
        return setStructOut(a, &v->ctr, sizeof(v->ctr));

    case kAvbSelSetStreamFilter: {
        if (v->hdr == nullptr) {
            return kIOReturnNotReady;
        }
        if (client != v->owner) {
            return kIOReturnExclusiveAccess;
        }
        if (a->structureInput == nullptr || a->structureInput->getLength() != sizeof(AvbStreamFilter)) {
            return kIOReturnBadArgument;
        }
        const void * bytes = a->structureInput->getBytesNoCopy();
        if (bytes == nullptr) {
            return kIOReturnBadArgument;
        }
        AvbStreamFilter f{};
        memcpy(&f, bytes, sizeof(f));
        if (!streamFilterValid(f)) {
            return kIOReturnBadArgument;
        }
        v->filter = f;  // same queue as the harvester: applied from the next tick
        LOG("rings: stream filter set (%u entries)", f.count);
        return kIOReturnSuccess;
    }

    case kAvbSelRingAttach:
        return ringAttach(v, client, a->scalarInput[0], a->scalarOutput);

    case kAvbSelRingDetach:
        return ringDetach(v, client);

    default:
        return kIOReturnUnsupported;
    }
}