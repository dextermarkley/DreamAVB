// I210Regs.h - Intel I210 register offsets and bit values used by the
// AVBI210Driver dext.
//
// Offsets/bit values from the Intel I210 datasheet (333016, rev 3.7);
// order of operations cross-checked against the observable behaviour of
// Linux igb; no code copied. Section numbers below refer to 333016.
//
// Only the registers the dext actually touches are listed. Anything written
// here must also appear in a named sequence in AVBI210Driver.cpp.

#ifndef I210_REGS_H
#define I210_REGS_H

#include <stdint.h>

namespace i210 {

// ---------------------------------------------------------------------------
// Identity (PCI config space)

constexpr uint32_t kVendorDevice = 0x15338086;  // 8086:1533 (I210 copper)
constexpr uint32_t kSubsystem    = 0x02141ab6;  // subsystem 1ab6:0214 (config 0x2c)

constexpr uint32_t kCfgVendorDevice = 0x00;
constexpr uint32_t kCfgCommand      = 0x04;
constexpr uint32_t kCfgRevision     = 0x08;     // revision ID = low byte
constexpr uint32_t kCfgSubsystem    = 0x2c;

constexpr uint16_t kCmdMemSpace  = 0x0002;
constexpr uint16_t kCmdBusMaster = 0x0004;

constexpr uint64_t kMinBar0Size = 0x20000;      // 128 KiB register window

// ---------------------------------------------------------------------------
// General control (8.2)

constexpr uint32_t kCtrl              = 0x0000;
constexpr uint32_t kCtrlGioMasterDis  = 1u << 2;
constexpr uint32_t kCtrlSlu           = 1u << 6;
constexpr uint32_t kCtrlFrcSpd        = 1u << 11;
constexpr uint32_t kCtrlFrcDplx       = 1u << 12;
constexpr uint32_t kCtrlRst           = 1u << 26;  // port software reset (self clearing)
// CTRL.PHY_RST (bit 31) and CTRL.DEV_RST (bit 29) are never set by this dext.

constexpr uint32_t kStatus               = 0x0008;
constexpr uint32_t kStatusFd             = 1u << 0;
constexpr uint32_t kStatusLu             = 1u << 1;
constexpr uint32_t kStatusSpeedShift     = 6;
constexpr uint32_t kStatusSpeedMask      = 3u << 6;   // 00 10M, 01 100M, 1x 1000M
constexpr uint32_t kStatusGioMasterEn    = 1u << 19;
constexpr uint32_t kStatusPfRstDone      = 1u << 21;

constexpr uint32_t kCtrlExt = 0x0018;  // read only here

// MDIC (8.2.4). On the I210 bits 25:21 are reserved: the PHY address comes
// from MDICNFG (0x0E04), whose default targets the internal PHY.
constexpr uint32_t kMdic            = 0x0020;
constexpr uint32_t kMdicDataMask    = 0x0000ffffu;
constexpr uint32_t kMdicRegShift    = 16;
constexpr uint32_t kMdicRegMask     = 0x001f0000u;
constexpr uint32_t kMdicOpWrite     = 1u << 26;
constexpr uint32_t kMdicOpRead      = 2u << 26;
constexpr uint32_t kMdicReady       = 1u << 28;
constexpr uint32_t kMdicError       = 1u << 30;

// PHY power management (8.27.2)
constexpr uint32_t kPhpm       = 0x0E14;
constexpr uint32_t kPhpmD0Lplu = 1u << 1;

// Clause-22 PHY registers (IEEE 802.3) and the bits we look at.
constexpr uint32_t kPhyBmcr       = 0;
constexpr uint32_t kPhyBmsr       = 1;
constexpr uint32_t kPhyAnar       = 4;
constexpr uint32_t kPhyGbCtrl     = 9;
constexpr uint16_t kBmcrAnRestart = 1u << 9;
constexpr uint16_t kBmcrPowerDown = 1u << 11;
constexpr uint16_t kBmcrAnEnable  = 1u << 12;
constexpr uint16_t kGbCtrl1000Fd  = 1u << 9;

// ---------------------------------------------------------------------------
// Interrupts (8.8). Primary offsets; 0x00C0 / 0x00D8 are documented aliases.

constexpr uint32_t kIcr      = 0x1500;  // read-to-clear
constexpr uint32_t kIcrAlias = 0x00C0;
constexpr uint32_t kImc      = 0x150C;  // write-only
constexpr uint32_t kEicr     = 0x1580;  // read-to-clear (never read)
constexpr uint32_t kEimc     = 0x1528;  // write-only

// ---------------------------------------------------------------------------
// NVM / shared-resource semaphores (4.6, 8.4, 8.7)

constexpr uint32_t kEec          = 0x12010;
constexpr uint32_t kEecAutoRd    = 1u << 9;
constexpr uint32_t kEemngctl     = 0x12030;
constexpr uint32_t kEemngCfgDone0 = 1u << 18;

constexpr uint32_t kSwsm         = 0x5B50;  // read sets SMBI
constexpr uint32_t kSwsmSmbi     = 1u << 0;
constexpr uint32_t kSwsmSwesmbi  = 1u << 1;

constexpr uint32_t kSwFwSync     = 0x5B5C;
constexpr uint32_t kSwFwSwPhy    = 1u << 1;
constexpr uint32_t kSwFwFwPhy    = 1u << 17;

// ---------------------------------------------------------------------------
// Receive (8.3, 8.10, 8.11)

constexpr uint32_t kRctl       = 0x0100;
constexpr uint32_t kRctlRxEn   = 1u << 1;
constexpr uint32_t kRctlUpe    = 1u << 3;
constexpr uint32_t kRctlMpe    = 1u << 4;
constexpr uint32_t kRctlBam    = 1u << 15;
constexpr uint32_t kRctlDpf    = 1u << 22;  // discard pause frames (power-on default 1)
constexpr uint32_t kRctlSecrc  = 1u << 26;

constexpr uint32_t kRxpbs        = 0x2404;  // RXPBSIZE
constexpr uint32_t kRxpbsCfgTsEn = 1u << 31;

constexpr uint32_t kRdbal0  = 0xC000;
constexpr uint32_t kRdbah0  = 0xC004;
constexpr uint32_t kRdlen0  = 0xC008;
constexpr uint32_t kSrrctl0 = 0xC00C;
constexpr uint32_t kRdh0    = 0xC010;
constexpr uint32_t kRdt0    = 0xC018;
constexpr uint32_t kRxdctl0 = 0xC028;

constexpr uint32_t kSrrctlBsizePktShift = 0;       // KiB units, 6:0
constexpr uint32_t kSrrctlBsizeHdrShift = 8;       // 64 B units, 13:8
constexpr uint32_t kSrrctlDescTypeAdvOneBuf = 1u << 25;  // DESCTYPE 27:25 = 001b
constexpr uint32_t kSrrctlTimestamp = 1u << 30;
constexpr uint32_t kSrrctlDropEn    = 1u << 31;

constexpr uint32_t kDctlPthreshShift = 0;
constexpr uint32_t kDctlHthreshShift = 8;
constexpr uint32_t kDctlWthreshShift = 16;
constexpr uint32_t kDctlEnable       = 1u << 25;   // RXDCTL / TXDCTL queue enable

constexpr uint32_t kEtqfBase        = 0x5CB0;      // + 4*n, n = 0..7
constexpr uint32_t kEtqfIndexPtp    = 3;
constexpr uint32_t kEtqfFilterEn    = 1u << 26;
constexpr uint32_t kEtqf1588        = 1u << 30;
constexpr uint32_t kEtqf3           = kEtqfBase + 4 * kEtqfIndexPtp;

constexpr uint32_t kRal0 = 0x5400;  // read only (loaded from NVM)
constexpr uint32_t kRah0 = 0x5404;

// ---------------------------------------------------------------------------
// Transmit (8.12). On the I210 TCTL bits 21:12 are BST (back-off slot time,
// default 0x40); the collision distance lives in TCTL_EXT and is left alone.

constexpr uint32_t kTctl         = 0x0400;
constexpr uint32_t kTctlEn       = 1u << 1;
constexpr uint32_t kTctlPsp      = 1u << 3;
constexpr uint32_t kTctlCtShift  = 4;
constexpr uint32_t kTctlCtValue  = 0xF;
constexpr uint32_t kTctlBstShift = 12;
constexpr uint32_t kTctlBstValue = 0x40;

constexpr uint32_t kTdbal0  = 0xE000;
constexpr uint32_t kTdbah0  = 0xE004;
constexpr uint32_t kTdlen0  = 0xE008;
constexpr uint32_t kTdh0    = 0xE010;
constexpr uint32_t kTdt0    = 0xE018;
constexpr uint32_t kTxdctl0 = 0xE028;

// ---------------------------------------------------------------------------
// Advanced descriptors (7.1.4.2, 7.2.2.3)

// TX data descriptor, cmd_type_len dword
constexpr uint32_t kTxdDtalenMask = 0x0000ffffu;
constexpr uint32_t kTxdMac2Step   = 1u << 19;   // MAC.2STEP_1588: latch TXSTMPL/H
constexpr uint32_t kTxdDtypData   = 0x3u << 20;
constexpr uint32_t kTxdCmdEop     = 1u << 24;
constexpr uint32_t kTxdCmdIfcs    = 1u << 25;
constexpr uint32_t kTxdCmdRs      = 1u << 27;
constexpr uint32_t kTxdCmdDext    = 1u << 29;
// TX olinfo_status dword
constexpr uint32_t kTxdPaylenShift = 14;
constexpr uint32_t kTxdStaDd       = 1u << 0;   // write-back

// RX write-back status_error dword (offset +8)
constexpr uint32_t kRxdStaDd   = 1u << 0;
constexpr uint32_t kRxdStaEop  = 1u << 1;
constexpr uint32_t kRxdStaTsip = 1u << 15;
constexpr uint32_t kRxdStaTs   = 1u << 16;
constexpr uint32_t kRxdErrRxe  = 1u << 31;      // extended error bit 11 (31:20)

constexpr uint32_t kRxTsHeaderLen = 16;         // 7.1.6 timestamp-in-buffer header

// ---------------------------------------------------------------------------
// Time sync (7.8, 8.15)

constexpr uint32_t kSystiml   = 0xB600;  // ns (< 1e9)
constexpr uint32_t kSystimh   = 0xB604;  // seconds
constexpr uint32_t kTiminca   = 0xB608;
constexpr uint32_t kTsyncTxCtl = 0xB614;
constexpr uint32_t kTxStmpl   = 0xB618;
constexpr uint32_t kTxStmph   = 0xB61C;
constexpr uint32_t kTsyncRxCtl = 0xB620;
constexpr uint32_t kRxStmpl   = 0xB624;  // never read (latch side effects)
constexpr uint32_t kRxStmph   = 0xB628;
constexpr uint32_t kRxSatrl   = 0xB62C;
constexpr uint32_t kRxSatrh   = 0xB630;
constexpr uint32_t kTsauxc    = 0xB640;
constexpr uint32_t kAuxStmpl0 = 0xB65C;
constexpr uint32_t kAuxStmph1 = 0xB668;
constexpr uint32_t kTsicr     = 0xB66C;  // read-to-clear
constexpr uint32_t kSystimr   = 0xB6F8;  // read first: latches SYSTIML/H
constexpr uint32_t kTsyncRxCfg = 0x5F50;

// TIMINCA: Incvalue 30:0 (2^-32 ns added to / subtracted from each 8 ns tick),
// ISGN bit 31. The dext limits Incvalue to 24 bits (|ppb| <= 488281).
constexpr uint32_t kTimincaIsgn     = 1u << 31;
constexpr uint32_t kTimincaUsedMask = 0x00ffffffu;
constexpr int64_t  kMaxPpb          = 488281;

constexpr uint32_t kTsauxcDisableSystime = 1u << 31;

constexpr uint32_t kTsyncTxValid   = 1u << 0;
constexpr uint32_t kTsyncTxEnabled = 1u << 4;

constexpr uint32_t kTsyncRxValid    = 1u << 0;
constexpr uint32_t kTsyncRxTypeMask = 0x7u << 1;
constexpr uint32_t kTsyncRxTypeAll  = 0x4u << 1;   // 100b = timestamp all packets
constexpr uint32_t kTsyncRxEnabled  = 1u << 4;

// ---------------------------------------------------------------------------
// Statistics block (clear on read) - never read through kAvbSelReadReg32.

constexpr uint32_t kStatsBegin = 0x4000;
constexpr uint32_t kStatsEnd   = 0x5000;  // exclusive

}  // namespace i210

#endif  // I210_REGS_H
