// SPDX-License-Identifier: MIT OR Apache-2.0

// AVBI210Driver.cpp - Phase 0b PCIDriverKit driver for the Intel I210 in the
// Thunderbolt AVB adapter (PCI 8086:1533, subsystem 1ab6:0214).
//
// Register sequences follow the Intel I210 datasheet (333016): 4.3 software
// reset, 4.5 initialization, 4.6 shared-resource semaphores, 7.1.6 receive
// timestamp in buffer, 7.8 time sync. The order of operations was cross-checked
// against the observable behaviour of Linux igb; no code copied. Constants are
// in I210Regs.h and pure helpers in I210Logic.hpp (host-tested in driver/tests).
//
// Safety rules (plan section 0):
//   * only 8086:1533 with subsystem 1ab6:0214 is ever touched (personality,
//     entitlement and the Start() check below);
//   * every poll is bounded; a timeout or an all-ones read latches ivars->fault
//     and all further MMIO is refused until Stop;
//   * there is no generic register-write path: every write belongs to a named
//     sequence (Stage A, SetClock, AdjFreq, Stage B init/shutdown, TX doorbell,
//     RX tail re-arm).
//
// Bring-up has two stages:
//   Stage A (Start): mask interrupts, enable SYSTIM, TIMINCA = 0, load SYSTIM.
//                    No reset, no DMA, no Bus Master.
//   Stage B (kAvbSelInitDatapath): software reset, PHY/link, one RX and one TX
//                    queue with advanced descriptors, RX timestamp in buffer,
//                    TX timestamp latch, polled by a 500 us timer.
//
// Phase 1a (ABI v3, design §3): with shared rings attached (kAvbSelRingAttach)
// the harvester classifies every frame (classifyRx) and copies it into the
// client-mapped control or audio ring; without rings it keeps filling the
// Phase 0 software ring for kAvbSelPollRx. The only new register accesses are
// reads of CRCERRS, MPC, RNBC and RQDPC(0). The live header fields are written
// only by setStage/setFlags/setFault/setClock and the harvester.

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

#include "AVBI210Driver.h"
#include "AVBI210UserClient.h"
#include "../AVBShared.h"
#include "I210Regs.h"
#include "I210Logic.hpp"

#define LOG(fmt, ...) os_log(OS_LOG_DEFAULT, "AVBI210: " fmt, ##__VA_ARGS__)

using namespace i210logic;

namespace {

constexpr uint32_t kRxRingSize    = 2048;   // descriptors (power of two); RDLEN 32 KiB
constexpr uint32_t kRxBufSize     = 2048;   // = SRRCTL.BSIZEPACKET 2 KiB
constexpr uint32_t kRxBufChunks   = 16;     // RX buffers live in 16 DMA chunks (AS-4)
constexpr uint64_t kRxBufChunkBytes = 256 * 1024;  // the Phase 0b single-segment size
constexpr uint32_t kRxBufsPerChunk = static_cast<uint32_t>(kRxBufChunkBytes / kRxBufSize);  // 128
constexpr uint32_t kTxRingSize    = 64;     // descriptors (power of two)
constexpr uint32_t kTxSlots       = 16;     // bounce buffers / max in flight
constexpr uint32_t kTxBufSize     = 2048;
constexpr uint32_t kSwRingSize    = 256;    // software RX ring (power of two)
constexpr uint64_t kTimerPeriodNs = 500000;       // 500 us
constexpr uint64_t kTimerLeewayNs = 50000;        // 50 us
constexpr uint64_t kTxTsTimeoutNs = 20000000;     // 20 ms
constexpr uint64_t kLinkWaitMs    = 10000;
constexpr uint64_t kLateTickNs    = 2000000;      // HarvestLateTicks: tick-to-tick gap > 2 ms
constexpr uint32_t kLinkPollTicks = 10;           // STATUS poll every 5 ms
constexpr uint32_t kStatsTicks    = 200;          // statistics read every 100 ms

static_assert(isPow2(kRxRingSize) && isPow2(kTxRingSize) && isPow2(kSwRingSize), "ring sizes");
static_assert((kRxRingSize * sizeof(AdvRxDesc)) % 128 == 0, "RDLEN must be a multiple of 128");
static_assert((kTxRingSize * sizeof(AdvTxDesc)) % 128 == 0, "TDLEN must be a multiple of 128");
static_assert(kRxBufSize == kRxBufKiB * 1024, "RX buffer size matches SRRCTL");
static_assert(kRxBufChunks * kRxBufsPerChunk == kRxRingSize, "one RX buffer per descriptor");

struct DmaBuf {
    IOBufferMemoryDescriptor * md;
    IODMACommand * cmd;
    uint8_t * va;
    uint64_t iova;
    uint64_t len;
};

}  // namespace

struct AVBI210Driver_IVars {
    IOPCIDevice * pci;
    bool opened;
    uint8_t bar;                 // memory index of BAR0

    uint32_t vendorDevice;
    uint32_t subsys;
    uint32_t hwRev;              // PCI revision ID
    uint32_t stage;              // AvbStage
    uint32_t fault;              // AvbFault, latched
    uint32_t flags;              // AVB_FLAG_*
    uint8_t mac[6];
    struct mach_timebase_info tb;

    IODispatchQueue * queue;     // default queue (timer + user client calls)
    IOTimerDispatchSource * timer;
    OSAction * timerAction;
    bool timerRunning;

    // Datapath (Stage B)
    DmaBuf rxRing;
    DmaBuf rxBufs[kRxBufChunks]; // descriptor i -> chunk i / 128, offset (i % 128) * 2048
    DmaBuf txRing;
    DmaBuf txBufs;
    uint32_t rxNext;             // next RX descriptor to examine
    bool rxDiscardToEop;         // inside a multi-descriptor frame (rxHarvestStep)
    uint32_t txProd;             // next TX descriptor to fill
    uint32_t txCons;             // oldest TX descriptor not yet reclaimed
    uint64_t txSeq;              // frames sent since datapath up

    // One outstanding timestamped TX.
    bool tsPending;
    uint64_t tsSeq;
    uint64_t tsDeadline;         // mach ticks
    uint32_t tsStatus;           // AvbTxTsStatus for tsSeq
    uint64_t tsValue;
    uint64_t tsMach;

    // Software RX ring filled by the harvester.
    AvbRxEntry * sw;
    uint32_t swHead;             // next write
    uint32_t swTail;             // next read
    uint32_t swDropped;

    AvbRxBatch * batch;          // scratch for kAvbSelPollRx

    // ABI v3 shared rings (design §3.2-§3.6). Only rings confer ownership.
    AVBI210UserClient * owner;   // not retained; cleared by RingDetach/ClientGone
    IOBufferMemoryDescriptor * rxMem;        // header + control + audio slots
    IOBufferMemoryDescriptor * consumerMem;  // AvbShmConsumer (client-writable)
    uint8_t * rxVa;
    AvbShmHeader * hdr;          // == rxVa while attached, else nullptr
    AvbShmConsumer * cons;
    uint32_t generation;         // RingAttach count since Start
    bool producerRunning;        // producerRunning(hdr != nullptr, stage, fault)
    uint64_t ctlHead;            // records written (monotonic per attach)
    uint64_t audioHead;
    AvbStreamFilter filter;      // applied by classifyRx from the next tick

    // SYSTIM generation and step tagging (§3.6).
    uint32_t clockGen;           // incremented by every SYSTIM write; never reset
    uint64_t settleTicks;        // AVB_STEP_SETTLE_NS in mach ticks
    RxStepAmbig stepAmbig;

    TokenBucket txBucket;        // SAF-6 TX rate bound

    AvbCounters ctr;             // published to hdr->counters every tick
    uint32_t tickCount;          // harvest ticks since InitDatapath
    uint64_t lastTickMach;       // start of the previous harvest tick (0: none)
    uint32_t rqdpcLast;          // last RQDPC(0) value (free-running counter)
};

using IV = AVBI210Driver_IVars;

// ---------------------------------------------------------------------------
// Live header fields. Each has exactly one writer path (design §3.4 table),
// all on the driver queue: setStage, setFlags, setFault, setClock, and the
// harvester (heads, counters, last_tick_mach).

static void storeRel32(uint32_t * p, uint32_t x) { __atomic_store_n(p, x, __ATOMIC_RELEASE); }

static void updateProducer(IV * v)
{
    v->producerRunning = producerRunning(v->hdr != nullptr, v->stage, v->fault);
    if (v->hdr != nullptr) {
        storeRel32(&v->hdr->producer_running, v->producerRunning ? 1u : 0u);
    }
}

static void stopProducer(IV * v)
{
    v->producerRunning = false;
    if (v->hdr != nullptr) {
        storeRel32(&v->hdr->producer_running, 0);
    }
}

static void setStage(IV * v, uint32_t stage)
{
    v->stage = stage;
    if (v->hdr != nullptr) {
        storeRel32(&v->hdr->stage, stage);
    }
    updateProducer(v);
}

static void setFlags(IV * v, uint32_t set, uint32_t clear)
{
    v->flags = (v->flags | set) & ~clear;
    if (v->hdr != nullptr) {
        storeRel32(&v->hdr->flags, v->flags);
    }
}

static void publishCounters(IV * v)
{
    if (v->hdr == nullptr) {
        return;
    }
    for (uint32_t i = 0; i < kAvbCtrCount; i++) {
        __atomic_store_n(&v->hdr->counters.c[i], v->ctr.c[i], __ATOMIC_RELAXED);
    }
}

// ---------------------------------------------------------------------------
// Hardware access. All of it funnels through these helpers so the fault latch
// is enforced in one place.

static void setFault(IV * v, uint32_t code, const char * why)
{
    if (v->fault == kAvbFaultNone) {
        v->fault = code;
        if (v->hdr != nullptr) {
            storeRel32(&v->hdr->fault, code);
        }
        stopProducer(v);
        LOG("FAULT %u: %{public}s; refusing further MMIO until Stop", code, why);
    }
}

static inline bool ok(IV * v) { return v->fault == kAvbFaultNone; }

// Read with the all-ones check (device gone / hung over Thunderbolt).
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
// data (SYSTIML/R, timestamp LSBs, RAL, ICR, the read-only debug dump).
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

// Write, then read STATUS to push the posted write out (write-only registers
// such as IMC cannot be read back themselves).
static void wrFlush(IV * v, uint32_t off, uint32_t val)
{
    wr(v, off, val);
    (void)rd(v, i210::kStatus);
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
        if (stepUs >= 1000) {
            IOSleep(stepUs / 1000);
        } else {
            IODelay(stepUs);
        }
    }
    return false;
}

static uint64_t nsToMach(IV * v, uint64_t ns)
{
    return v->tb.numer == 0 ? ns : ns * v->tb.denom / v->tb.numer;
}

static uint64_t machToNs(IV * v, uint64_t t)
{
    return v->tb.denom == 0 ? t : t * v->tb.numer / v->tb.denom;
}

static uint64_t hostNowNs(IV * v)
{
    return machToNs(v, mach_absolute_time());
}

// ---------------------------------------------------------------------------
// PCI command register (config space; allowed even after a fault so Bus Master
// can always be dropped).

static void setCommandBits(IV * v, uint16_t set, uint16_t clear)
{
    uint16_t cmd = 0;
    v->pci->ConfigurationRead16(i210::kCfgCommand, &cmd);
    cmd = static_cast<uint16_t>((cmd | set) & ~clear);
    v->pci->ConfigurationWrite16(i210::kCfgCommand, cmd);
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
    dmaFree(&v->rxRing);
    for (uint32_t i = 0; i < kRxBufChunks; i++) {
        dmaFree(&v->rxBufs[i]);
    }
    dmaFree(&v->txRing);
    dmaFree(&v->txBufs);
}

static uint8_t * rxBufVa(IV * v, uint32_t idx)
{
    return v->rxBufs[idx / kRxBufsPerChunk].va + uint64_t(idx % kRxBufsPerChunk) * kRxBufSize;
}

static uint64_t rxBufIova(IV * v, uint32_t idx)
{
    return v->rxBufs[idx / kRxBufsPerChunk].iova + uint64_t(idx % kRxBufsPerChunk) * kRxBufSize;
}

// ---------------------------------------------------------------------------
// SYSTIM clock (7.8.3.2)

// The only SYSTIM write sequence (Stage A, SetClock, StepClock, datapath
// re-arm): SYSTIML first, then SYSTIMH. Every write bumps the clock
// generation and opens the RX step-ambiguity window (§3.6).
static void setClock(IV * v, uint64_t ns)
{
    const Systim s = nsToSystim(ns);
    wr(v, i210::kSystiml, s.ns);
    wrFlush(v, i210::kSystimh, s.sec);
    v->clockGen += 1;
    if (v->hdr != nullptr) {
        storeRel32(&v->hdr->clock_gen, v->clockGen);
    }
    rxStepArm(&v->stepAmbig, mach_absolute_time(), v->settleTicks);
}

// Read sequence: SYSTIMR (latches the set), SYSTIML, SYSTIMH.
static bool readClock(IV * v, uint64_t * out)
{
    (void)rdRaw(v, i210::kSystimr);
    const uint32_t lo = rdRaw(v, i210::kSystiml);
    const uint32_t hi = rd(v, i210::kSystimh);
    if (!ok(v)) {
        return false;
    }
    *out = systimToNs(hi, lo);
    return true;
}

// Enable SYSTIM counting, nominal 8 ns per tick, load host time.
static void armClock(IV * v)
{
    const uint32_t aux = rd(v, i210::kTsauxc);
    wr(v, i210::kTsauxc, aux & ~i210::kTsauxcDisableSystime);
    wr(v, i210::kTiminca, 0);
    setClock(v, hostNowNs(v));
}

// ---------------------------------------------------------------------------
// Stage A (plan item 4). Writes exactly: IMC, EIMC (mask all), TSAUXC
// (clear DISABLE_SYSTIME), TIMINCA = 0, SYSTIML/SYSTIMH. Reads ICR once.

static void stageA(IV * v)
{
    const uint32_t status = rd(v, i210::kStatus);
    if (!ok(v)) {
        LOG("stage A: STATUS read all-ones; staying at stage 0");
        return;
    }
    wr(v, i210::kImc, 0xffffffff);
    wrFlush(v, i210::kEimc, 0xffffffff);
    (void)rdRaw(v, i210::kIcr);

    const uint32_t ral = rdRaw(v, i210::kRal0);
    const uint32_t rah = rd(v, i210::kRah0);
    const Mac m = macFromRal(ral, rah);
    memcpy(v->mac, m.b, sizeof(v->mac));

    armClock(v);

    const uint32_t ctrl = rd(v, i210::kCtrl);
    const uint32_t aux = rd(v, i210::kTsauxc);
    const uint32_t inca = rd(v, i210::kTiminca);
    LOG("stage A: STATUS 0x%08x CTRL 0x%08x TSAUXC 0x%08x TIMINCA 0x%08x", status, ctrl, aux, inca);
    if (ok(v)) {
        setStage(v, kAvbStageTimeSync);
        LOG("stage A complete (stage 1)");
    }
}

// ---------------------------------------------------------------------------
// Shared-resource semaphore for the PHY (4.6.1 / 4.6.2). Each poll <= 100 ms.

static bool swsmAcquire(IV * v)
{
    // Software/software semaphore: reading SWSM returns SMBI=0 once, then 1.
    if (!pollUs(v, 100000, 100, [&] { return (rd(v, i210::kSwsm) & i210::kSwsmSmbi) == 0; })) {
        setFault(v, kAvbFaultSwFwSync, "SWSM.SMBI not acquired within 100 ms");
        return false;
    }
    // Software/firmware semaphore: set SWESMBI and read it back.
    const bool got = pollUs(v, 100000, 100, [&] {
        const uint32_t s = rd(v, i210::kSwsm);
        wr(v, i210::kSwsm, s | i210::kSwsmSwesmbi);
        return (rd(v, i210::kSwsm) & i210::kSwsmSwesmbi) != 0;
    });
    if (!got) {
        wr(v, i210::kSwsm, rd(v, i210::kSwsm) & ~(i210::kSwsmSmbi | i210::kSwsmSwesmbi));
        setFault(v, kAvbFaultSwFwSync, "SWSM.SWESMBI not acquired within 100 ms");
        return false;
    }
    return true;
}

static void swsmRelease(IV * v)
{
    wr(v, i210::kSwsm, rd(v, i210::kSwsm) & ~(i210::kSwsmSmbi | i210::kSwsmSwesmbi));
}

static bool phyAcquire(IV * v)
{
    for (uint32_t waitedMs = 0; waitedMs <= 100 && ok(v); waitedMs++) {
        if (!swsmAcquire(v)) {
            return false;
        }
        const uint32_t sync = rd(v, i210::kSwFwSync);
        if ((sync & (i210::kSwFwSwPhy | i210::kSwFwFwPhy)) == 0) {
            wr(v, i210::kSwFwSync, sync | i210::kSwFwSwPhy);
            swsmRelease(v);
            return ok(v);
        }
        swsmRelease(v);
        IOSleep(1);
    }
    setFault(v, kAvbFaultSwFwSync, "SW_FW_SYNC PHY bit busy for 100 ms");
    return false;
}

static void phyRelease(IV * v)
{
    if (!swsmAcquire(v)) {
        return;
    }
    wr(v, i210::kSwFwSync, rd(v, i210::kSwFwSync) & ~i210::kSwFwSwPhy);
    swsmRelease(v);
}

// ---------------------------------------------------------------------------
// MDIC (8.2.4). READY poll <= 10 ms; a READY timeout latches kAvbFaultMdio,
// an MDI ERROR is reported to the caller (link step stops, no fault).

static bool mdicWait(IV * v, MdicResult * out)
{
    uint32_t m = 0;
    if (!pollUs(v, 10000, 10, [&] {
            m = rd(v, i210::kMdic);
            return (m & i210::kMdicReady) != 0;
        })) {
        setFault(v, kAvbFaultMdio, "MDIC ready timeout (10 ms)");
        return false;
    }
    *out = mdicDecode(m);
    return !out->error;
}

static bool phyRead(IV * v, uint32_t reg, uint16_t * val)
{
    wr(v, i210::kMdic, mdicReadFrame(reg));
    MdicResult r{};
    if (!mdicWait(v, &r)) {
        LOG("datapath: MDIC read reg %u failed (error=%d fault=%u)", reg, r.error, v->fault);
        return false;
    }
    *val = r.data;
    return true;
}

static bool phyWrite(IV * v, uint32_t reg, uint16_t val)
{
    wr(v, i210::kMdic, mdicWriteFrame(reg, val));
    MdicResult r{};
    if (!mdicWait(v, &r)) {
        LOG("datapath: MDIC write reg %u failed (error=%d fault=%u)", reg, r.error, v->fault);
        return false;
    }
    return true;
}

// Copper autoneg through the internal PHY; no PHY reset, advertisements kept.
static void setupLink(IV * v)
{
    setFlags(v, 0, AVB_FLAG_LINK_UP | AVB_FLAG_1000FD);

    const uint32_t ctrl = rd(v, i210::kCtrl);
    wr(v, i210::kCtrl, (ctrl | i210::kCtrlSlu) & ~(i210::kCtrlFrcSpd | i210::kCtrlFrcDplx));
    const uint32_t phpm = rd(v, i210::kPhpm);
    if (phpm & i210::kPhpmD0Lplu) {
        LOG("datapath: PHPM 0x%08x has D0LPLU set; clearing", phpm);
        wr(v, i210::kPhpm, phpm & ~i210::kPhpmD0Lplu);
    }
    if (!ok(v)) {
        return;
    }

    if (phyAcquire(v)) {
        uint16_t bmcr = 0;
        uint16_t bmsr = 0;
        uint16_t anar = 0;
        uint16_t gbc = 0;
        if (phyRead(v, i210::kPhyBmcr, &bmcr) && phyRead(v, i210::kPhyBmsr, &bmsr) &&
            phyRead(v, i210::kPhyAnar, &anar) && phyRead(v, i210::kPhyGbCtrl, &gbc)) {
            LOG("datapath: PHY BMCR 0x%04x BMSR 0x%04x ANAR 0x%04x 1000T_CTRL 0x%04x (1000FD adv %d)", bmcr, bmsr,
                anar, gbc, (gbc & i210::kGbCtrl1000Fd) != 0);
            if (bmcrNeedsAutoneg(bmcr)) {
                const uint16_t nb = bmcrAutoneg(bmcr);
                LOG("datapath: BMCR 0x%04x -> 0x%04x (autoneg restart)", bmcr, nb);
                (void)phyWrite(v, i210::kPhyBmcr, nb);
            }
        }
        if (ok(v)) {
            phyRelease(v);
        }
    }
    if (!ok(v)) {
        return;
    }

    uint32_t status = 0;
    const bool up = pollUs(v, kLinkWaitMs * 1000, 10000, [&] {
        status = rd(v, i210::kStatus);
        return (status & i210::kStatusLu) != 0;
    });
    const Link l = linkFromStatus(status);
    if (up) {
        setFlags(v, AVB_FLAG_LINK_UP | ((l.mbps == 1000 && l.fd) ? AVB_FLAG_1000FD : 0u), 0);
        LOG("datapath: link up STATUS 0x%08x %u Mb/s %{public}s", status, l.mbps, l.fd ? "full" : "half");
    } else if (ok(v)) {
        LOG("datapath: no link after %llu ms (STATUS 0x%08x); continuing", kLinkWaitMs, status);
    }
}

// ---------------------------------------------------------------------------
// Software reset (4.3.1, 5.2.3.3)

static bool resetDevice(IV * v)
{
    wr(v, i210::kImc, 0xffffffff);
    wr(v, i210::kRctl, 0);
    wrFlush(v, i210::kTctl, i210::kTctlPsp);
    IOSleep(10);

    wr(v, i210::kCtrl, rd(v, i210::kCtrl) | i210::kCtrlGioMasterDis);
    if (!pollUs(v, 80000, 100, [&] { return (rd(v, i210::kStatus) & i210::kStatusGioMasterEn) == 0; })) {
        setFault(v, kAvbFaultMasterDisable, "STATUS.GIO_MASTER_ENABLE did not clear within 80 ms");
        return false;
    }
    LOG("datapath: GIO master disabled");

    wr(v, i210::kCtrl, rd(v, i210::kCtrl) | i210::kCtrlRst);
    IOSleep(10);  // datasheet: wait >= 3 ms before any register access

    uint32_t status = 0;
    uint32_t eec = 0;
    uint32_t mng = 0;
    const bool done = pollUs(v, 100000, 1000, [&] {
        status = rd(v, i210::kStatus);
        eec = rd(v, i210::kEec);
        mng = rd(v, i210::kEemngctl);
        return (status & i210::kStatusPfRstDone) != 0 && (eec & i210::kEecAutoRd) != 0 &&
               (mng & i210::kEemngCfgDone0) != 0;
    });
    if (!done) {
        LOG("datapath: reset poll STATUS 0x%08x EEC 0x%08x EEMNGCTL 0x%08x", status, eec, mng);
        setFault(v, kAvbFaultResetTimeout, "PF_RST_DONE / Auto_RD / CFG_DONE0 not set within 100 ms");
        return false;
    }
    LOG("datapath: reset done STATUS 0x%08x EEC 0x%08x EEMNGCTL 0x%08x", status, eec, mng);

    // Interrupts again after the reset (4.5.4).
    wr(v, i210::kImc, 0xffffffff);
    wrFlush(v, i210::kEimc, 0xffffffff);
    (void)rdRaw(v, i210::kIcr);
    return ok(v);
}

// ---------------------------------------------------------------------------
// Queues (4.5.9, 4.5.10)

static void armRxDesc(IV * v, uint32_t idx)
{
    auto * ring = reinterpret_cast<volatile AdvRxDesc *>(v->rxRing.va);
    const AdvRxDesc d = makeAdvRxRead(rxBufIova(v, idx));
    ring[idx].pktAddr = d.pktAddr;
    ring[idx].hdrAddr = d.hdrAddr;  // clears the write-back DD bit
}

static bool setupRx(IV * v, uint64_t initFlags)
{
    for (uint32_t i = 0; i < kRxRingSize; i++) {
        armRxDesc(v, i);
    }
    v->rxNext = 0;
    v->rxDiscardToEop = false;
    __atomic_thread_fence(__ATOMIC_SEQ_CST);

    wr(v, i210::kRxpbs, rd(v, i210::kRxpbs) | i210::kRxpbsCfgTsEn);
    wr(v, i210::kSrrctl0, srrctlValue());
    wr(v, i210::kRdbal0, static_cast<uint32_t>(v->rxRing.iova));
    wr(v, i210::kRdbah0, static_cast<uint32_t>(v->rxRing.iova >> 32));
    wr(v, i210::kRdlen0, kRxRingSize * sizeof(AdvRxDesc));
    wr(v, i210::kRdh0, 0);
    wr(v, i210::kRdt0, 0);
    wr(v, i210::kRxdctl0, rxdctlEnable());
    if (!pollUs(v, 10000, 100, [&] { return (rd(v, i210::kRxdctl0) & i210::kDctlEnable) != 0; })) {
        setFault(v, kAvbFaultQueueEnable, "RXDCTL0.ENABLE did not read back within 10 ms");
        return false;
    }
    wr(v, i210::kRdt0, kRxRingSize - 1);

    // PTP timestamping: ETQF[3] marks 0x88F7 for 1588, TYPE = all packets.
    wr(v, i210::kEtqf3, etqf1588Value());
    wr(v, i210::kTsyncRxCtl, tsyncRxCtlValue(rd(v, i210::kTsyncRxCtl)));
    wr(v, i210::kTsyncRxCfg, 0);

    wrFlush(v, i210::kRctl, rctlValue((initFlags & AVB_INIT_FLAG_PROMISC) != 0));
    LOG("datapath: RX up RXPBS 0x%08x SRRCTL0 0x%08x RXDCTL0 0x%08x TSYNCRXCTL 0x%08x RCTL 0x%08x",
        rd(v, i210::kRxpbs), rd(v, i210::kSrrctl0), rd(v, i210::kRxdctl0), rd(v, i210::kTsyncRxCtl),
        rd(v, i210::kRctl));
    return ok(v);
}

static bool setupTx(IV * v)
{
    memset(v->txRing.va, 0, v->txRing.len);
    v->txProd = 0;
    v->txCons = 0;
    __atomic_thread_fence(__ATOMIC_SEQ_CST);

    wr(v, i210::kTdbal0, static_cast<uint32_t>(v->txRing.iova));
    wr(v, i210::kTdbah0, static_cast<uint32_t>(v->txRing.iova >> 32));
    wr(v, i210::kTdlen0, kTxRingSize * sizeof(AdvTxDesc));
    wr(v, i210::kTdh0, 0);
    wr(v, i210::kTdt0, 0);
    wr(v, i210::kTxdctl0, txdctlEnable());
    if (!pollUs(v, 10000, 100, [&] { return (rd(v, i210::kTxdctl0) & i210::kDctlEnable) != 0; })) {
        setFault(v, kAvbFaultQueueEnable, "TXDCTL0.ENABLE did not read back within 10 ms");
        return false;
    }
    wr(v, i210::kTctl, tctlValue());
    wrFlush(v, i210::kTsyncTxCtl, rd(v, i210::kTsyncTxCtl) | i210::kTsyncTxEnabled);
    LOG("datapath: TX up TXDCTL0 0x%08x TCTL 0x%08x TSYNCTXCTL 0x%08x", rd(v, i210::kTxdctl0),
        rd(v, i210::kTctl), rd(v, i210::kTsyncTxCtl));
    return ok(v);
}

// ---------------------------------------------------------------------------
// Harvest (2 ms timer and start of kAvbSelPollRx)

static void swPush(IV * v, const AvbRxEntry & e)
{
    if (ringDistance(v->swTail, v->swHead, kSwRingSize) == kSwRingSize - 1) {
        v->swTail = ringNext(v->swTail, kSwRingSize);  // overwrite oldest
        v->swDropped++;
        v->ctr.c[kAvbCtrLegacySwOverrun]++;
    }
    v->sw[v->swHead] = e;
    v->swHead = ringNext(v->swHead, kSwRingSize);
}

// Legacy (no rings attached): Phase 0 software ring for kAvbSelPollRx.
// flen <= AVB_RX_SLOT_DATA_MAX (rxFrameBounds), copy <= AVB_RX_COPY_BYTES.
static void deliverLegacy(IV * v, uint32_t statusError, bool tsip, const uint8_t * buf, const uint8_t * frame,
                          uint32_t flen, uint64_t now)
{
    AvbRxEntry e{};
    e.harvest_mach = now;
    e.type_flags = static_cast<uint16_t>(statusError & 0xffffu);
    if (tsip) {
        e.hw_ts = parseRxTsHeader(buf);
        e.has_ts = 1;
    }
    e.len = flen;
    const uint32_t c = flen < AVB_RX_COPY_BYTES ? flen : AVB_RX_COPY_BYTES;
    memcpy(e.data, frame, c);
    swPush(v, e);
}

struct RingTick {
    uint64_t ctlFree;    // slots the producer may still fill this tick
    uint64_t audioFree;
    bool ambig;          // AVB_RXF_TS_STEP_AMBIG for every slot of this tick
};

// Ring mode: classify, then copy into the control or audio ring
// (drop-newest on overrun). The caller has applied both length guards, so
// flen <= AVB_RX_SLOT_DATA_MAX and the copy stays inside the slot.
static void deliverRing(IV * v, RingTick * t, uint32_t statusError, const RxWb & w, uint16_t vlanField,
                        const uint8_t * buf, const uint8_t * frame, uint32_t flen, uint64_t now)
{
    const bool vp = (statusError & i210::kRxdStaVp) != 0;
    const RxVerdict verdict = classifyRx(frame, flen, v->mac, v->filter, vp, vlanField);
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
    slot->hw_ts_ns = w.tsip ? parseRxTsHeader(buf) : 0;
    slot->harvest_mach = now;
    slot->index = *head;
    slot->status_error = statusError;
    slot->len = static_cast<uint16_t>(flen);
    slot->vlan_tci = vlanField;
    slot->flags = static_cast<uint16_t>(rxVlanFlags(vp, frame, flen) | (w.tsip ? AVB_RXF_TS_HW : 0u) |
                                        (t->ambig ? AVB_RXF_TS_STEP_AMBIG : 0u));
    slot->cls = static_cast<uint8_t>(verdict);
    slot->pad0 = 0;
    slot->desc_len = w.len;
    slot->clock_gen = v->clockGen;
    memcpy(slot->data, frame, flen);
    (*head)++;
    (*freeSlots)--;
    v->ctr.c[rxVerdictCounter(verdict)]++;
}

// One harvest pass (TimerFired, and the start of kAvbSelPollRx in legacy
// mode). Bounded by kRxRingSize descriptors. Returns descriptors processed.
static uint32_t harvestRx(IV * v, bool ambig)
{
    const auto * ring = reinterpret_cast<const volatile uint32_t *>(v->rxRing.va);
    uint64_t * ctr = v->ctr.c;
    const bool ringMode = v->producerRunning;
    RingTick t{0, 0, ambig};
    if (ringMode) {
        // The consumer page is client-writable: validate the tails.
        const uint64_t tailC = __atomic_load_n(&v->cons->ctl_tail, __ATOMIC_ACQUIRE);
        const uint64_t tailA = __atomic_load_n(&v->cons->audio_tail, __ATOMIC_ACQUIRE);
        const RingSpace c = ringFree(v->ctlHead, tailC, AVB_RX_CTL_SLOTS);
        const RingSpace a = ringFree(v->audioHead, tailA, AVB_RX_AUDIO_SLOTS);
        if (!c.valid) {
            ctr[kAvbCtrConsumerIndexInvalid]++;
        }
        if (!a.valid) {
            ctr[kAvbCtrConsumerIndexInvalid]++;
        }
        t.ctlFree = c.free;  // 0 (full) when invalid
        t.audioFree = a.free;
    }

    bool rearmed = false;
    uint32_t last = 0;
    uint32_t n = 0;
    for (; n < kRxRingSize && ok(v); n++) {
        const uint32_t idx = v->rxNext;
        const uint32_t statusError = ring[idx * 4 + 2];
        if ((statusError & i210::kRxdStaDd) == 0) {
            break;
        }
        __atomic_thread_fence(__ATOMIC_ACQUIRE);
        const uint32_t lenVlan = ring[idx * 4 + 3];
        const RxWb w = decodeAdvRxWb(statusError, static_cast<uint16_t>(lenVlan & 0xffffu));
        const uint16_t vlanField = static_cast<uint16_t>(lenVlan >> i210::kRxdVlanShift);

        const RxStep step = rxHarvestStep(&v->rxDiscardToEop, w.eop);
        if (step == RxStep::NonEop) {
            ctr[kAvbCtrRxNonEop]++;
        } else if (step == RxStep::Frame) {
            ctr[kAvbCtrRxFrames]++;
            // Both length guards precede classification and every copy.
            const RxBounds b = rxFrameBounds(w.len, w.tsip);
            if (w.error || b.kind == RxBoundsKind::ShortTs) {
                ctr[kAvbCtrRxErrors]++;
            } else if (b.kind == RxBoundsKind::Oversize) {
                ctr[kAvbCtrRxOversize]++;
            } else {
                const uint8_t * buf = rxBufVa(v, idx);
                const uint8_t * frame = w.tsip ? buf + i210::kRxTsHeaderLen : buf;
                const uint64_t now = mach_absolute_time();
                if (ringMode) {
                    deliverRing(v, &t, statusError, w, vlanField, buf, frame, b.flen, now);
                } else {
                    deliverLegacy(v, statusError, w.tsip, buf, frame, b.flen, now);
                }
            }
        }
        armRxDesc(v, idx);
        last = idx;
        rearmed = true;
        v->rxNext = ringNext(idx, kRxRingSize);
    }
    if (ringMode) {
        __atomic_store_n(&v->hdr->ctl_head, v->ctlHead, __ATOMIC_RELEASE);
        __atomic_store_n(&v->hdr->audio_head, v->audioHead, __ATOMIC_RELEASE);
    }
    if (rearmed) {
        __atomic_thread_fence(__ATOMIC_SEQ_CST);
        OSSynchronizeIO();
        wr(v, i210::kRdt0, last);
    }
    return n;
}

// 5 ms STATUS poll: link transitions go through setFlags (header mirror).
static void pollLink(IV * v)
{
    const uint32_t status = rd(v, i210::kStatus);
    if (!ok(v)) {
        return;
    }
    const Link l = linkFromStatus(status);
    const uint32_t mask = AVB_FLAG_LINK_UP | AVB_FLAG_1000FD;
    const uint32_t want = l.up ? (AVB_FLAG_LINK_UP | ((l.mbps == 1000 && l.fd) ? AVB_FLAG_1000FD : 0u)) : 0u;
    const bool wasUp = (v->flags & AVB_FLAG_LINK_UP) != 0;
    if ((v->flags & mask) == want) {
        return;
    }
    setFlags(v, want, mask & ~want);
    if (l.up && !wasUp) {
        v->ctr.c[kAvbCtrLinkUpEvents]++;
    } else if (!l.up && wasUp) {
        v->ctr.c[kAvbCtrLinkDownEvents]++;
    }
    LOG("link %{public}s STATUS 0x%08x %u Mb/s %{public}s", l.up ? "up" : "down", status, l.mbps,
        l.fd ? "full" : "half");
}

// Statistics (reads only). CRCERRS/MPC/RNBC are clear-on-read and
// therefore denied to ReadReg32; RQDPC(0) wraps and is denied as well, so
// this accumulator is their only reader (DP-8). accumulate == false takes
// the baseline at InitDatapath.
static void readHwStats(IV * v, bool accumulate)
{
    HwStatsRead r{};
    r.crcerrs = rd(v, i210::kCrcerrs);
    r.mpc = rd(v, i210::kMpc);
    r.rnbc = rd(v, i210::kRnbc);
    r.rqdpc0 = rdRaw(v, i210::kRqdpc0);  // any value is legal for a wrapping counter
    if (!ok(v)) {
        return;
    }
    if (accumulate) {
        statsAccumulate(&v->ctr, &v->rqdpcLast, r);
    } else {
        v->rqdpcLast = r.rqdpc0;
    }
}

// Reclaim TX descriptors whose DD write-back has landed.
static void reclaimTx(IV * v)
{
    const auto * ring = reinterpret_cast<const volatile AdvTxDesc *>(v->txRing.va);
    while (v->txCons != v->txProd && txDescDone(ring[v->txCons].olinfo)) {
        v->txCons = ringNext(v->txCons, kTxRingSize);
    }
}

// Read TXSTMPL then TXSTMPH (the H read releases the latch).
static bool readTxStamp(IV * v, uint64_t * out)
{
    const uint32_t lo = rdRaw(v, i210::kTxStmpl);
    const uint32_t hi = rd(v, i210::kTxStmph);
    if (!ok(v)) {
        return false;
    }
    *out = systimToNs(hi, lo & 0x3fffffffu);
    return true;
}

static void checkTx(IV * v)
{
    reclaimTx(v);
    if (!v->tsPending) {
        return;
    }
    const uint32_t ctl = rd(v, i210::kTsyncTxCtl);
    if (ok(v) && (ctl & i210::kTsyncTxValid) != 0) {
        uint64_t ts = 0;
        if (readTxStamp(v, &ts)) {
            v->tsValue = ts;
            v->tsMach = mach_absolute_time();
            v->tsStatus = kAvbTxTsOk;
            v->tsPending = false;
            v->ctr.c[kAvbCtrTxTsOk]++;
            return;
        }
    }
    if (mach_absolute_time() > v->tsDeadline) {
        // A latch that becomes VALID later is released (and counted stale)
        // by the pre-doorbell check of the next timestamped send.
        v->tsStatus = kAvbTxTsTimeout;
        v->tsPending = false;
        v->ctr.c[kAvbCtrTxTsTimeout]++;
        LOG("TX timestamp timeout for seq %llu (TSYNCTXCTL 0x%08x)", v->tsSeq, ctl);
    }
}

static void armTimer(IV * v)
{
    if (v->timer == nullptr) {
        return;
    }
    const uint64_t period = nsToMach(v, kTimerPeriodNs);
    v->timer->WakeAtTime(kIOTimerClockMachAbsoluteTime, mach_absolute_time() + period,
                         nsToMach(v, kTimerLeewayNs));
    v->timerRunning = true;
}

// ---------------------------------------------------------------------------
// Datapath up / down

// Each step bounded; failures latch a fault. Never PHY reset or power-down.
static void quiesceHw(IV * v)
{
    // TX queue: wait (bounded) for TDH == TDT, then disable (4.5.10.1).
    (void)pollUs(v, 10000, 100, [&] { return rd(v, i210::kTdh0) == rd(v, i210::kTdt0); });
    wr(v, i210::kTxdctl0, rd(v, i210::kTxdctl0) & ~i210::kDctlEnable);
    const bool txOff = pollUs(v, 10000, 100, [&] { return (rd(v, i210::kTxdctl0) & i210::kDctlEnable) == 0; });
    // RX: queue first, then the RX path (4.5.9.2).
    wr(v, i210::kRxdctl0, rd(v, i210::kRxdctl0) & ~i210::kDctlEnable);
    const bool rxOff = pollUs(v, 10000, 100, [&] { return (rd(v, i210::kRxdctl0) & i210::kDctlEnable) == 0; });
    wr(v, i210::kRctl, rd(v, i210::kRctl) & ~i210::kRctlRxEn);
    wr(v, i210::kTctl, rd(v, i210::kTctl) & ~i210::kTctlEn);
    wr(v, i210::kTsyncRxCtl, rd(v, i210::kTsyncRxCtl) & ~i210::kTsyncRxEnabled);
    wr(v, i210::kTsyncTxCtl, rd(v, i210::kTsyncTxCtl) & ~i210::kTsyncTxEnabled);
    wrFlush(v, i210::kRxpbs, rd(v, i210::kRxpbs) & ~i210::kRxpbsCfgTsEn);
    if (ok(v) && (!txOff || !rxOff)) {
        setFault(v, kAvbFaultQueueEnable, "RXDCTL0/TXDCTL0 ENABLE did not clear within 10 ms");
    }
}

static void shutdownDatapath(IV * v)
{
    if (v->stage != kAvbStageDatapath) {
        return;
    }
    if (v->timer != nullptr) {
        v->timer->SetEnable(false);
    }
    v->timerRunning = false;
    if (ok(v)) {
        quiesceHw(v);
    }
    setCommandBits(v, 0, i210::kCmdBusMaster);
    dmaFreeAll(v);
    v->tsPending = false;
    v->rxNext = 0;
    v->rxDiscardToEop = false;
    v->lastTickMach = 0;
    setFlags(v, 0, AVB_FLAG_LINK_UP | AVB_FLAG_1000FD);
    // Rings (if any) stay attached and mapped; producer_running drops to 0.
    setStage(v, kAvbStageTimeSync);
    LOG("datapath down (stage 1, fault %u)", v->fault);
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

    // Each RX chunk must be a single DMA segment (dmaAlloc); no retry with
    // another size.
    bool allocated = dmaAlloc(v, &v->rxRing, kRxRingSize * sizeof(AdvRxDesc));
    for (uint32_t i = 0; allocated && i < kRxBufChunks; i++) {
        allocated = dmaAlloc(v, &v->rxBufs[i], kRxBufChunkBytes);
    }
    allocated = allocated && dmaAlloc(v, &v->txRing, kTxRingSize * sizeof(AdvTxDesc)) &&
                dmaAlloc(v, &v->txBufs, uint64_t(kTxSlots) * kTxBufSize);
    if (!allocated) {
        dmaFreeAll(v);
        setFault(v, kAvbFaultDma, "DMA buffer setup failed");
        return kIOReturnNoMemory;
    }

    v->swHead = v->swTail = v->swDropped = 0;
    v->txSeq = 0;
    v->tsPending = false;
    v->tsSeq = 0;
    // Counters are monotonic for the life of a datapath: reset only here.
    memset(&v->ctr, 0, sizeof(v->ctr));
    v->tickCount = 0;
    v->lastTickMach = 0;

    LOG("datapath: reset + init (flags 0x%llx)", initFlags);
    bool good = resetDevice(v);
    if (good) {
        // Bus Master only after the reset, so nothing DMAs with stale IOVAs.
        setCommandBits(v, i210::kCmdMemSpace | i210::kCmdBusMaster, 0);
        setStage(v, kAvbStageDatapath);  // so a failure below runs the shutdown path
        setupLink(v);
        good = ok(v) && setupRx(v, initFlags) && setupTx(v);
    }
    if (good) {
        // The reset re-disables SYSTIM: re-arm the clock.
        armClock(v);
        good = ok(v);
    }
    if (good) {
        // Statistics baseline (reads only): clears the clear-on-read
        // counters and records RQDPC(0), so this datapath counts from 0.
        readHwStats(v, false);
        good = ok(v);
    }
    if (!good) {
        LOG("datapath: init failed (fault %u)", v->fault);
        if (v->stage == kAvbStageDatapath) {
            shutdownDatapath(v);
        } else {
            setCommandBits(v, 0, i210::kCmdBusMaster);
            dmaFreeAll(v);
        }
        return kIOReturnIOError;
    }

    if (v->timer != nullptr) {
        v->timer->SetEnable(true);
    }
    armTimer(v);
    LOG("datapath up (stage 2), link_up=%d 1000fd=%d", (v->flags & AVB_FLAG_LINK_UP) != 0,
        (v->flags & AVB_FLAG_1000FD) != 0);
    return kIOReturnSuccess;
}

// Stage one frame into its bounce buffer and TX descriptor and advance the
// producer index. Does NOT ring the doorbell -- the caller does that once,
// which is the whole point of the batched path. The caller has already run
// checkTx(), taken a token and confirmed ring space.
//
// Both send paths go through here on purpose: a second copy of the
// bounce-slot/descriptor layout is how a wire path ends up tested in one
// direction and wrong in the other.
static void stageFrame(IV * v, const uint8_t * frame, uint32_t len, bool wantTs)
{
    // In-flight <= kTxSlots and kTxRingSize is a multiple of kTxSlots, so the
    // bounce slot of descriptor i is free once the descriptors before it are.
    const uint32_t slot = v->txProd % kTxSlots;
    uint8_t * buf = v->txBufs.va + uint64_t(slot) * kTxBufSize;
    memcpy(buf, frame, len);
    uint32_t padded = len;
    if (padded < AVB_FRAME_MIN_LEN) {
        memset(buf + len, 0, AVB_FRAME_MIN_LEN - len);
        padded = AVB_FRAME_MIN_LEN;
    }
    auto * ring = reinterpret_cast<volatile AdvTxDesc *>(v->txRing.va);
    const AdvTxDesc d = makeAdvTxDesc(v->txBufs.iova + uint64_t(slot) * kTxBufSize, padded, wantTs);
    ring[v->txProd].addr = d.addr;
    ring[v->txProd].cmdTypeLen = d.cmdTypeLen;
    ring[v->txProd].olinfo = d.olinfo;
    v->txProd = ringNext(v->txProd, kTxRingSize);
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
    checkTx(v);
    if (wantTs && v->tsPending) {
        v->ctr.c[kAvbCtrTxTsBusyRejected]++;
        return kIOReturnBusy;
    }
    if (ringDistance(v->txCons, v->txProd, kTxRingSize) >= kTxSlots) {
        return kIOReturnBusy;
    }
    // SAF-6: hard TX ceiling independent of the client. Not queued when empty.
    if (!tokenBucketTake(&v->txBucket, hostNowNs(v))) {
        v->ctr.c[kAvbCtrTxRateLimited]++;
        return kIOReturnNoResources;
    }

    if (wantTs) {
        // Release a stale latch so this frame's timestamp can be captured.
        const uint32_t ctl = rd(v, i210::kTsyncTxCtl);
        uint64_t stale = 0;
        if (ok(v) && (ctl & i210::kTsyncTxValid) != 0) {
            (void)readTxStamp(v, &stale);
            v->ctr.c[kAvbCtrTxStaleLatch]++;
            LOG("TX: released stale timestamp %llu", stale);
        }
        if (!ok(v)) {
            return kIOReturnIOError;
        }
    }

    stageFrame(v, frame, len, wantTs);
    __atomic_thread_fence(__ATOMIC_SEQ_CST);
    OSSynchronizeIO();

    const uint64_t mach = mach_absolute_time();
    wrFlush(v, i210::kTdt0, v->txProd);

    const uint64_t seq = ++v->txSeq;
    v->ctr.c[kAvbCtrTxFrames]++;
    if (wantTs) {
        v->ctr.c[kAvbCtrTxTsRequested]++;
        v->tsPending = true;
        v->tsSeq = seq;
        v->tsDeadline = mach + nsToMach(v, kTxTsTimeoutNs);
        v->tsStatus = kAvbTxTsPending;
        v->tsValue = 0;
        v->tsMach = 0;
    }
    *seqOut = seq;
    *machOut = mach;
    return ok(v) ? kIOReturnSuccess : kIOReturnIOError;
}

// kAvbSelSendFrames: queue up to AVB_TX_BATCH_MAX frames with ONE doorbell.
// Frames are queued in order until one is refused (token bucket empty or the
// ring full); `acceptedOut` says how many went in, so the client re-paces the
// remainder instead of treating a partial interval as a failure.
static kern_return_t sendFrames(IV * v, const uint8_t * in, uint32_t inLen,
                                uint64_t * acceptedOut, uint64_t * machOut)
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
    checkTx(v);
    // One clock reading for the whole batch: taking N tokens at a single
    // instant is exactly the burst the bucket exists to allow.
    const uint64_t now = hostNowNs(v);
    uint32_t accepted = 0;
    bool refused = false;
    for (uint32_t i = 0; i < plan.count; i++) {
        if (ringDistance(v->txCons, v->txProd, kTxRingSize) >= kTxSlots) {
            refused = true;
            break;
        }
        if (!tokenBucketTake(&v->txBucket, now)) {
            v->ctr.c[kAvbCtrTxRateLimited]++;
            refused = true;
            break;
        }
        stageFrame(v, in + plan.f[i].offset, plan.f[i].len, false);
        accepted++;
    }
    if (accepted == 0) {
        // Nothing queued: no doorbell, and not an error -- the client sees
        // accepted == 0 and backs off.
        return ok(v) ? kIOReturnSuccess : kIOReturnIOError;
    }
    __atomic_thread_fence(__ATOMIC_SEQ_CST);
    OSSynchronizeIO();

    const uint64_t mach = mach_absolute_time();
    wrFlush(v, i210::kTdt0, v->txProd);

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
// Shared rings (design §3.2, §3.3, §3.6). Only rings confer ownership.

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

// Releases rxMem/consumerMem. The caller has already stopped the producer
// and cleared the flags (while the header was still live). A client's
// existing mapping holds its own reference.
static void releaseRings(IV * v)
{
    v->hdr = nullptr;
    v->cons = nullptr;
    v->rxVa = nullptr;
    OSSafeReleaseNULL(v->rxMem);
    OSSafeReleaseNULL(v->consumerMem);
    v->producerRunning = false;
}

static kern_return_t ringAttach(IV * v, AVBI210UserClient * client, uint64_t flags, uint64_t * out)
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
        LOG("rings: attach failed: no memory (nothing latched)");
        return kIOReturnNoMemory;
    }

    // Header valid from the first map: constants plus the live fields.
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
    h->flags = v->flags;
    h->producer_running = 0;
    memcpy(h->mac, v->mac, sizeof(h->mac));
    h->counters = v->ctr;
    h->clock_gen = v->clockGen;
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
    setFlags(v, AVB_FLAG_RINGS_ATTACHED | AVB_FLAG_OWNED, 0);
    updateProducer(v);  // stage 2: the harvester switches to ring mode next tick
    LOG("rings: attached generation %u (stage %u, producer %d)", v->generation, v->stage, v->producerRunning);

    out[0] = v->generation;
    out[1] = AVB_RX_MEM_SIZE;
    out[2] = AVB_CONSUMER_MEM_SIZE;
    return kIOReturnSuccess;
}

// RingDetach (§3.6): producer off, filter cleared, flags cleared while the
// header is live, memory released, ownership released.
static kern_return_t ringDetach(IV * v, AVBI210UserClient * client)
{
    if (v->hdr == nullptr) {
        return kIOReturnNotReady;
    }
    if (client != v->owner) {
        return kIOReturnExclusiveAccess;
    }
    stopProducer(v);
    memset(&v->filter, 0, sizeof(v->filter));
    setFlags(v, 0, AVB_FLAG_RINGS_ATTACHED | AVB_FLAG_OWNED);
    releaseRings(v);
    v->owner = nullptr;
    LOG("rings: detached generation %u (stage %u)", v->generation, v->stage);
    return kIOReturnSuccess;
}

// ---------------------------------------------------------------------------
// IOService

bool
AVBI210Driver::init()
{
    if (!super::init()) {
        return false;
    }
    ivars = IONewZero(AVBI210Driver_IVars, 1);
    if (ivars == nullptr) {
        return false;
    }
    ivars->sw = IONewZero(AvbRxEntry, kSwRingSize);
    ivars->batch = IONewZero(AvbRxBatch, 1);
    return ivars->sw != nullptr && ivars->batch != nullptr;
}

void
AVBI210Driver::free()
{
    if (ivars != nullptr) {
        dmaFreeAll(ivars);
        releaseRings(ivars);
        OSSafeReleaseNULL(ivars->timerAction);
        OSSafeReleaseNULL(ivars->timer);
        OSSafeReleaseNULL(ivars->queue);
        IOSafeDeleteNULL(ivars->sw, AvbRxEntry, kSwRingSize);
        IOSafeDeleteNULL(ivars->batch, AvbRxBatch, 1);
    }
    IOSafeDeleteNULL(ivars, AVBI210Driver_IVars, 1);
    super::free();
}

kern_return_t
IMPL(AVBI210Driver, Start)
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
    v->pci->ConfigurationRead32(i210::kCfgVendorDevice, &v->vendorDevice);
    v->pci->ConfigurationRead32(i210::kCfgSubsystem, &v->subsys);
    if (!isOurDevice(v->vendorDevice, v->subsys)) {
        LOG("refusing device %08x subsys %08x (only 15338086 / 02141ab6)", v->vendorDevice, v->subsys);
        v->fault = kAvbFaultIdMismatch;
        v->pci->Close(this, 0);
        v->opened = false;
        return kIOReturnUnsupported;
    }
    uint32_t rev = 0;
    v->pci->ConfigurationRead32(i210::kCfgRevision, &rev);
    v->hwRev = rev & 0xffu;
    LOG("start: %08x subsys %08x rev %u", v->vendorDevice, v->subsys, v->hwRev);

    uint64_t barSize = 0;
    uint8_t barType = 0;
    ret = v->pci->GetBARInfo(kPCIMemoryRangeBAR0, &v->bar, &barSize, &barType);
    if (ret != kIOReturnSuccess || barSize < i210::kMinBar0Size) {
        LOG("BAR0 unusable (ret 0x%x size 0x%llx)", ret, barSize);
        v->pci->Close(this, 0);
        v->opened = false;
        return ret != kIOReturnSuccess ? ret : kIOReturnNoResources;
    }
    mach_timebase_info(&v->tb);
    v->settleTicks = nsToMach(v, AVB_STEP_SETTLE_NS);  // 24 000 ticks at 24 MHz

    // Memory space only; Bus Master waits for the Stage B reset.
    setCommandBits(v, i210::kCmdMemSpace, 0);

    ret = CopyDispatchQueue(kIOServiceDefaultQueueName, &v->queue);
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

    stageA(v);
    return kIOReturnSuccess;
}

kern_return_t
IMPL(AVBI210Driver, Stop)
{
    IV * v = ivars;
    LOG("stop (stage %u fault %u)", v->stage, v->fault);
    if (v->timer != nullptr) {
        v->timer->SetEnable(false);
    }
    v->timerRunning = false;
    if (v->opened) {
        shutdownDatapath(v);
        setCommandBits(v, 0, i210::kCmdBusMaster);
        v->pci->Close(this, 0);
        v->opened = false;
    }
    dmaFreeAll(v);
    if (v->hdr != nullptr) {
        stopProducer(v);
        setFlags(v, 0, AVB_FLAG_RINGS_ATTACHED | AVB_FLAG_OWNED);
        releaseRings(v);
    }
    v->owner = nullptr;
    return Stop(provider, SUPERDISPATCH);
}

kern_return_t
IMPL(AVBI210Driver, NewUserClient)
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
IMPL(AVBI210Driver, TimerFired)
{
    (void)action;
    (void)time;
    IV * v = ivars;
    const uint64_t tickStart = mach_absolute_time();
    if (v->hdr != nullptr) {
        __atomic_store_n(&v->hdr->last_tick_mach, tickStart, __ATOMIC_RELEASE);  // first, every tick
    }
    v->timerRunning = false;
    if (v->stage != kAvbStageDatapath || !ok(v)) {
        return;
    }
    const bool ambig = rxStepAmbiguous(&v->stepAmbig, tickStart);  // once per tick
    uint64_t * ctr = v->ctr.c;
    if (v->lastTickMach != 0 && tickStart - v->lastTickMach > nsToMach(v, kLateTickNs)) {
        ctr[kAvbCtrHarvestLateTicks]++;
    }
    v->lastTickMach = tickStart;
    ctr[kAvbCtrHarvestTicks]++;

    const uint32_t batch = harvestRx(v, ambig);
    if (batch > ctr[kAvbCtrHarvestMaxBatch]) {
        ctr[kAvbCtrHarvestMaxBatch] = batch;
    }
    v->tickCount++;
    if (ok(v) && v->tickCount % kLinkPollTicks == 0) {
        pollLink(v);
    }
    if (ok(v) && v->tickCount % kStatsTicks == 0) {
        readHwStats(v, true);
    }
    checkTx(v);
    publishCounters(v);
    if (ok(v) && v->stage == kAvbStageDatapath) {
        armTimer(v);
    }
}

kern_return_t
AVBI210Driver::CopyRingMemory(AVBI210UserClient * client, uint64_t type, uint64_t * options,
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

// §3.3 order, serialized with TimerFired on the driver queue.
void
AVBI210Driver::ClientGone(AVBI210UserClient * client)
{
    IV * v = ivars;
    if (client == nullptr || client != v->owner) {
        LOG("uc: closed (not owner)");
        return;
    }
    stopProducer(v);                                              // 1
    shutdownDatapath(v);                                          // 2
    memset(&v->filter, 0, sizeof(v->filter));                     // 3
    v->tsPending = false;
    v->tsSeq = 0;
    v->tsStatus = kAvbTxTsPending;
    v->tsValue = 0;
    v->tsMach = 0;
    v->txBucket = TokenBucket{};
    setFlags(v, 0, AVB_FLAG_RINGS_ATTACHED | AVB_FLAG_OWNED);
    releaseRings(v);                                              // 4
    v->owner = nullptr;                                           // 5
    LOG("uc: closed (owner): datapath down, rings of generation %u released", v->generation);
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

// Selectors 2, 3, 4, 5, 6, 7, 9, 10, 15: refused to non-owners while an owner
// exists; with no owner anyone may call them (Phase 0b semantics).
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
    case kAvbSelStepClock:
        return true;
    default:
        return false;
    }
}

kern_return_t
AVBI210Driver::HandleUserMethod(AVBI210UserClient * client, uint64_t selector, IOUserClientMethodArguments * a)
{
    IV * v = ivars;
    if (isOwnerOnly(selector) && v->owner != nullptr && client != v->owner) {
        return kIOReturnExclusiveAccess;
    }
    switch (selector) {
    case kAvbSelGetInfo: {
        AvbInfo info{};
        info.abi_version = AVB_ABI_VERSION;
        info.vendor_device = v->vendorDevice;
        info.subsys = v->subsys;
        info.hw_rev = v->hwRev;
        info.nic_kind = AVB_NIC_I210;
        if (ok(v)) {
            info.ctrl_a = rd(v, i210::kCtrl);
            info.ctrl_b = rd(v, i210::kStatus);
            info.clock_ctl = rdRaw(v, i210::kTiminca);
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
            if (!readClock(v, &ns)) {
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
        uint64_t back = 0;
        if (!readClock(v, &back)) {
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
        if (ppb > i210::kMaxPpb || ppb < -i210::kMaxPpb) {
            return kIOReturnBadArgument;
        }
        const uint32_t reg = timincaFromPpb(ppb);
        wrFlush(v, i210::kTiminca, reg);
        a->scalarOutput[0] = reg;
        return ok(v) ? kIOReturnSuccess : kIOReturnIOError;
    }

    case kAvbSelInitDatapath: {
        const kern_return_t ret = initDatapath(v, a->scalarInput[0]);
        a->scalarOutput[0] = v->stage;
        return ret;
    }

    case kAvbSelPollRx: {
        if (v->stage != kAvbStageDatapath || v->hdr != nullptr) {
            return kIOReturnNotReady;  // legacy mode only: no PollRx while rings are attached
        }
        if (ok(v)) {
            (void)harvestRx(v, false);
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
        a->scalarOutput[0] = accepted;
        a->scalarOutput[1] = mach;
        return ret;
    }

    case kAvbSelGetTxTimestamp: {
        if (ok(v) && v->stage == kAvbStageDatapath) {
            checkTx(v);
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
        if (!isReadableRegOffsetI210(off)) {
            return kIOReturnBadArgument;
        }
        a->scalarOutput[0] = rdRaw(v, static_cast<uint32_t>(off));
        return kIOReturnSuccess;
    }

    case kAvbSelShutdownDatapath:
        // Never changes ownership; attached rings stay mapped.
        shutdownDatapath(v);
        return kIOReturnSuccess;

    case kAvbSelStepClock: {
        if (!ok(v) || v->stage < kAvbStageTimeSync) {
            return kIOReturnNotReady;
        }
        const int64_t delta = static_cast<int64_t>(a->scalarInput[0]);
        if (delta > AVB_STEP_CLOCK_MAX_NS || delta < -AVB_STEP_CLOCK_MAX_NS) {
            return kIOReturnBadArgument;
        }
        const uint64_t m0 = mach_absolute_time();
        uint64_t s0 = 0;
        if (!readClock(v, &s0)) {
            return kIOReturnIOError;
        }
        const uint64_t m1 = mach_absolute_time();
        const uint64_t mid = m0 + (m1 - m0) / 2;
        const uint64_t m2 = mach_absolute_time();
        const StepResult t = stepTarget(s0, delta, machToNs(v, m2 - mid));
        if (!t.ok) {
            return kIOReturnBadArgument;
        }
        setClock(v, t.target);
        uint64_t after = 0;
        if (!readClock(v, &after)) {
            return kIOReturnIOError;
        }
        a->scalarOutput[0] = s0;
        a->scalarOutput[1] = after;
        a->scalarOutput[2] = mid;
        return kIOReturnSuccess;
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
