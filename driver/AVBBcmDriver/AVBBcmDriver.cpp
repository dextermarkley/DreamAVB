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
//                    ring, one TX ring, RX PTP filter, polled by a 2 ms timer.

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
#include "../AVBBcmShared.h"
#include "Bcm57762Regs.h"
#include "Tg3Logic.hpp"

#define LOG(fmt, ...) os_log(OS_LOG_DEFAULT, "AVBBcm: " fmt, ##__VA_ARGS__)

using namespace tg3logic;

namespace {

constexpr uint32_t kRxPosted      = 64;     // std RX buffers posted
constexpr uint32_t kRxBufSize     = 2048;   // per RX buffer (DMA size 1536)
constexpr uint32_t kTxSlots       = 16;     // TX bounce buffers / max in flight
constexpr uint32_t kTxBufSize     = 2048;
constexpr uint32_t kSwRingSize    = 256;    // software RX ring (power of two)
constexpr uint64_t kTimerPeriodNs = 2000000;      // 2 ms
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
    struct mach_timebase_info tb;

    IODispatchQueue * queue;     // default queue (timer + user client calls)
    IOTimerDispatchSource * timer;
    OSAction * timerAction;
    bool timerRunning;

    // Datapath (Stage B)
    DmaBuf status;
    DmaBuf stdRing;
    DmaBuf retRing;
    DmaBuf txRing;
    DmaBuf rxBufs;
    DmaBuf txBufs;
    uint32_t stdProd;            // std ring producer (ring index)
    uint32_t retCons;            // RX return ring consumer
    uint32_t txProd;             // TX producer
    uint32_t txCons;             // last TX consumer seen in the status block
    uint64_t txSeq;              // frames sent since datapath up

    // One outstanding timestamped TX (tg3 behaviour).
    bool tsPending;
    uint64_t tsSeq;
    uint64_t tsPre;              // TX_TSTAMP before the doorbell
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

static uint64_t nsToMach(IV * v, uint64_t ns)
{
    return v->tb.numer == 0 ? ns : ns * v->tb.denom / v->tb.numer;
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
    memset(v->txRing.va, 0, v->txRing.len);
    memset(v->status.va, 0, v->status.len);
    v->stdProd = 0;
    v->retCons = 0;
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
    for (uint32_t i = 1; i < bcm::kRxRetRcbCount; i++) {
        sramWrite(v, bcm::kSramRxRetRcb + i * bcm::kBdInfoSize + bcm::kBdInfoMaxLenFlags, bcm::kBdInfoDisabled);
    }
    wrFlush(v, bcm::kMboxInterrupt0, 1);
    wr(v, bcm::kMboxTxHostProd0, 0);
    wr(v, bcm::kMboxRxRetCons0, 0);
    wr(v, bcm::kHostccStatusBlkHi, static_cast<uint32_t>(v->status.iova >> 32));
    wr(v, bcm::kHostccStatusBlkLo, static_cast<uint32_t>(v->status.iova));
    sramSetRcb(v, bcm::kSramSendRcb, v->txRing.iova, sendRcbMaxLenFlags());
    sramSetRcb(v, bcm::kSramRxRetRcb, v->retRing.iova, rxRetRcbMaxLenFlags());

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
    return ok(v);
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

    if (!allStopped) {
        setFault(v, kAvbFaultStopBlock, "a block enable bit did not clear during shutdown");
    }
}

// ---------------------------------------------------------------------------
// Harvest (2 ms timer and start of kAvbSelPollRx)

static void swPush(IV * v, const AvbRxEntry & e)
{
    if (ringDistance(v->swTail, v->swHead, kSwRingSize) == kSwRingSize - 1) {
        v->swTail = ringNext(v->swTail, kSwRingSize);  // overwrite oldest
        v->swDropped++;
    }
    v->sw[v->swHead] = e;
    v->swHead = ringNext(v->swHead, kSwRingSize);
}

static void harvestRx(IV * v)
{
    uint8_t sb[0x14];
    readStatusBlock(v, sb);
    const uint32_t hwProd = le16At(sb, bcm::kSbRxProducer0) & (bcm::kRxRetRingSize - 1);
    if (hwProd == v->retCons) {
        return;
    }
    const auto * ret = reinterpret_cast<const volatile RxBd *>(v->retRing.va);
    bool reposted = false;
    while (v->retCons != hwProd && ok(v)) {
        RxBd bd;
        bd.addrHi = ret[v->retCons].addrHi;
        bd.addrLo = ret[v->retCons].addrLo;
        bd.idxLen = ret[v->retCons].idxLen;
        bd.typeFlags = ret[v->retCons].typeFlags;
        bd.ipTcpCsum = ret[v->retCons].ipTcpCsum;
        bd.errVlan = ret[v->retCons].errVlan;
        bd.reserved = ret[v->retCons].reserved;
        bd.opaque = ret[v->retCons].opaque;
        const RxReturn r = decodeRxReturn(bd);
        const uint64_t now = mach_absolute_time();

        AvbRxEntry e{};
        e.harvest_mach = now;
        e.type_flags = r.flags;
        if (r.hasTimestamp) {
            // Read LSB then MSB for every PTP-flagged frame, even an errored
            // one, so the one-deep RX latch is released for the next event.
            uint64_t ts = 0;
            if (readTs(v, bcm::kRxTstampLsb, bcm::kRxTstampMsb, &ts)) {
                e.hw_ts = ts & bcm::kTstampMask;
                e.has_ts = 1;
            }
        }
        if (r.fromStdRing && r.stdIndex < kRxPosted) {
            if (!r.error) {
                e.len = r.len;
                const uint32_t n = r.len < AVB_RX_COPY_BYTES ? r.len : AVB_RX_COPY_BYTES;
                memcpy(e.data, v->rxBufs.va + uint64_t(r.stdIndex) * kRxBufSize, n);
                swPush(v, e);
            }
            postStdBuffer(v, r.stdIndex);
            reposted = true;
        } else {
            LOG("harvest: unexpected return BD opaque 0x%08x", bd.opaque);
        }
        v->retCons = ringNext(v->retCons, bcm::kRxRetRingSize);
    }
    __atomic_thread_fence(__ATOMIC_SEQ_CST);
    OSSynchronizeIO();
    wr(v, bcm::kMboxRxRetCons0, v->retCons);
    if (reposted) {
        wr(v, bcm::kMboxRxStdProd, v->stdProd);
    }
}

static void checkTx(IV * v)
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
    v->tsPending = false;
    v->tsSeq = 0;

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
    checkTx(v);
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

    const uint64_t seq = ++v->txSeq;
    if (wantTs) {
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

// ---------------------------------------------------------------------------
// IOService

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
    return ivars->sw != nullptr && ivars->batch != nullptr;
}

void
AVBBcmDriver::free()
{
    if (ivars != nullptr) {
        dmaFreeAll(ivars);
        OSSafeReleaseNULL(ivars->timerAction);
        OSSafeReleaseNULL(ivars->timer);
        OSSafeReleaseNULL(ivars->queue);
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
IMPL(AVBBcmDriver, Stop)
{
    IV * v = ivars;
    LOG("stop (stage %u fault %u)", v->stage, v->fault);
    if (v->timer != nullptr) {
        v->timer->SetEnable(false);
    }
    v->timerRunning = false;
    if (v->opened) {
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
    if (v->stage != kAvbStageDatapath || !ok(v)) {
        return;
    }
    harvestRx(v);
    checkTx(v);
    if (ok(v) && v->stage == kAvbStageDatapath) {
        armTimer(v);
    }
}

// ---------------------------------------------------------------------------
// User client methods (selectors in AVBBcmShared.h)

static kern_return_t setStructOut(IOUserClientMethodArguments * a, const void * p, size_t len)
{
    if (a->structureOutputMaximumSize < len) {
        return kIOReturnBadArgument;
    }
    a->structureOutput = OSData::withBytes(p, len);
    return a->structureOutput != nullptr ? kIOReturnSuccess : kIOReturnNoMemory;
}

kern_return_t
AVBBcmDriver::HandleUserMethod(uint64_t selector, IOUserClientMethodArguments * a)
{
    IV * v = ivars;
    switch (selector) {
    case kAvbSelGetInfo: {
        AvbInfo info{};
        info.abi_version = AVB_ABI_VERSION;
        info.vendor_device = v->vendorDevice;
        info.subsys = v->subsys;
        info.asic_rev = v->asicRev;
        if (ok(v)) {
            v->pci->ConfigurationRead32(bcm::kMiscHostCtrl, &info.misc_host_ctrl);
            info.grc_mode = rd(v, bcm::kGrcMode);
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
        if (!isReadableRegOffset(off)) {
            return kIOReturnBadArgument;
        }
        a->scalarOutput[0] = rdRaw(v, static_cast<uint32_t>(off));
        return kIOReturnSuccess;
    }

    case kAvbSelShutdownDatapath:
        shutdownDatapath(v);
        return kIOReturnSuccess;

    default:
        return kIOReturnUnsupported;
    }
}