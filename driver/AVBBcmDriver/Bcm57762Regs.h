// Bcm57762Regs.h - register map for the Broadcom BCM57762 (57766-class) as
// used by the AVB-Audio Phase 0 dext.
//
// Offsets/bit values derived from the observable behaviour of the Linux tg3
// driver; no code copied. Names and layout are our own. Only the registers the
// Phase 0 sequences touch are listed. Values marked "tg3 behaviour" come from
// what tg3 programs for a 57765_PLUS / 57765_CLASS / ASIC_REV_57766 part rather
// than from a published datasheet (none exists for this silicon).

#ifndef BCM57762_REGS_H
#define BCM57762_REGS_H

#include <stdint.h>

namespace bcm {

// ---------------------------------------------------------------------------
// Device identity (PCI config space)
constexpr uint32_t kVendorDevice        = 0x168214e4;  // 14e4:1682
constexpr uint32_t kSubsystem           = 0x00f6106b;  // 106b:00f6 (Apple TB GbE)
constexpr uint32_t kCfgVendorDevice     = 0x00;
constexpr uint32_t kCfgCommand          = 0x04;        // 16-bit
constexpr uint32_t kCfgSubsystem        = 0x2c;
constexpr uint16_t kCmdMemSpace         = 0x0002;
constexpr uint16_t kCmdBusMaster        = 0x0004;

// PCIe capability: device control / status (offsets from capability base)
constexpr uint32_t kPcieDevCtl          = 0x08;        // 16-bit
constexpr uint32_t kPcieDevSta          = 0x0a;        // 16-bit
constexpr uint16_t kPcieDevCtlRelaxEn   = 0x0010;
constexpr uint16_t kPcieDevCtlNoSnoopEn = 0x0800;
constexpr uint16_t kPcieDevStaErrBits   = 0x000f;      // CED|NFED|FED|URD (write-1-clear)

// ---------------------------------------------------------------------------
// Host interface block (config space, mirrored in BAR0 at the same offsets)
constexpr uint32_t kMiscHostCtrl        = 0x68;
constexpr uint32_t  kMhcClearInt        = 0x00000001;
constexpr uint32_t  kMhcMaskPciInt      = 0x00000002;
constexpr uint32_t  kMhcWordSwap        = 0x00000008;
constexpr uint32_t  kMhcPcistateRw      = 0x00000010;
constexpr uint32_t  kMhcIndirAccess     = 0x00000080;
constexpr uint32_t  kMhcTaggedStatus    = 0x00000200;
constexpr uint32_t  kMhcChipRevMask     = 0xffff0000;
// Value tg3 programs on a little-endian host (tg3 behaviour).
constexpr uint32_t  kMhcLittleEndian    = kMhcMaskPciInt | kMhcWordSwap |
                                          kMhcIndirAccess | kMhcPcistateRw;

constexpr uint32_t kDmaRwCtrl           = 0x6c;
constexpr uint32_t  kDmaRwDisCacheAlign = 0x00000001;

constexpr uint32_t kPciState            = 0x70;
constexpr uint32_t  kPciStateRomEnable  = 0x00000020;
constexpr uint32_t  kPciStateRomRetry   = 0x00000040;

constexpr uint32_t kClockCtrl           = 0x74;
constexpr uint32_t kMemWinBase          = 0x7c;
constexpr uint32_t kMemWinData          = 0x84;
constexpr uint32_t kGen15ProdIdAsicRev  = 0xfc;
constexpr uint32_t  kAsicRev57766       = 0x57766;     // (asic_rev >> 12)
constexpr uint32_t  kChipRev57765Ax     = 0x577650;    // (asic_rev >> 8)

// ---------------------------------------------------------------------------
// Mailboxes (direct MMIO on 57765-class; low 32 bits of each 64-bit mailbox)
constexpr uint32_t kMboxInterrupt0      = 0x0204;
constexpr uint32_t kMboxRxStdProd       = 0x026c;
constexpr uint32_t kMboxRxJumboProd     = 0x0274;
constexpr uint32_t kMboxRxRetCons0      = 0x0284;
constexpr uint32_t kMboxTxHostProd0     = 0x0304;

// ---------------------------------------------------------------------------
// MAC
constexpr uint32_t kMacMode             = 0x0400;
constexpr uint32_t  kMacModeHalfDuplex  = 0x00000002;
constexpr uint32_t  kMacModePortMask    = 0x0000000c;
constexpr uint32_t  kMacModePortGmii    = 0x00000008;
constexpr uint32_t  kMacModePortMii     = 0x00000004;
constexpr uint32_t  kMacModeRxStatEn    = 0x00000800;
constexpr uint32_t  kMacModeRxStatClr   = 0x00001000;
constexpr uint32_t  kMacModeTxStatEn    = 0x00004000;
constexpr uint32_t  kMacModeTxStatClr   = 0x00008000;
constexpr uint32_t  kMacModeTdeEn       = 0x00200000;
constexpr uint32_t  kMacModeRdeEn       = 0x00400000;
constexpr uint32_t  kMacModeFhdeEn      = 0x00800000;
constexpr uint32_t kMacStatus           = 0x0404;
constexpr uint32_t kMacAddr0High        = 0x0410;
constexpr uint32_t kMacAddr0Low         = 0x0414;
constexpr uint32_t kMacRxMtu            = 0x043c;
constexpr uint32_t kMacMiCom            = 0x044c;
constexpr uint32_t  kMiComCmdWrite      = 0x04000000;
constexpr uint32_t  kMiComCmdRead       = 0x08000000;
constexpr uint32_t  kMiComReadFailed    = 0x10000000;
constexpr uint32_t  kMiComStartBusy     = 0x20000000;
constexpr uint32_t  kMiComPhyShift      = 21;
constexpr uint32_t  kMiComPhyMask       = 0x03e00000;
constexpr uint32_t  kMiComRegShift      = 16;
constexpr uint32_t  kMiComRegMask       = 0x001f0000;
constexpr uint32_t  kMiComDataMask      = 0x0000ffff;
constexpr uint32_t kMacMiStat           = 0x0450;
constexpr uint32_t  kMiStatLinkAttnEn   = 0x00000001;
constexpr uint32_t kMacMiMode           = 0x0454;
constexpr uint32_t  kMiModeAutoPoll     = 0x00000010;
constexpr uint32_t  kMiModeBase         = 0x000c0000;  // tg3 behaviour (default MDIO clock)
constexpr uint32_t kMacTxMode           = 0x045c;
constexpr uint32_t  kTxModeEnable       = 0x00000002;
constexpr uint32_t  kTxModeMbufLockFix  = 0x00000100;
constexpr uint32_t kMacTxLengths        = 0x0464;
// IPG_CRS=2, IPG=6, slot time=32 (tg3 behaviour).
constexpr uint32_t  kTxLengthsDefault   = (2u << 12) | (6u << 8) | 32u;
constexpr uint32_t kMacRxMode           = 0x0468;
constexpr uint32_t  kRxModeEnable       = 0x00000002;
constexpr uint32_t  kRxModePromisc      = 0x00000100;
constexpr uint32_t  kRxModeKeepVlan     = 0x00000400;
constexpr uint32_t  kRxModeIpv6Csum     = 0x01000000;
constexpr uint32_t kMacRcvRule0         = 0x0480;
constexpr uint32_t kMacRcvValue0        = 0x0484;
constexpr uint32_t kMacRcvRule1         = 0x0488;
constexpr uint32_t kMacRcvValue1        = 0x048c;
constexpr uint32_t kMacRcvRule4         = 0x04a0;      // rules 4..7 at +8 each
constexpr uint32_t  kRcvRuleDisableMask = 0x7fffffff;
constexpr uint32_t  kRcvRule0Default    = 0xc2000000;  // tg3 behaviour
constexpr uint32_t  kRcvRule1Default    = 0x86000004;  // tg3 behaviour
constexpr uint32_t kMacRcvRuleCfg       = 0x0500;
constexpr uint32_t  kRcvRuleCfgDefClass = 0x00000008;
constexpr uint32_t kMacLowWmarkMaxRx    = 0x0504;

// ---------------------------------------------------------------------------
// PTP timestamp capture
constexpr uint32_t kTxTstampLsb         = 0x05c0;
constexpr uint32_t kTxTstampMsb         = 0x05c4;
constexpr uint32_t kRxTstampLsb         = 0x06b0;
constexpr uint32_t kRxTstampMsb         = 0x06b4;
constexpr uint64_t kTstampMask          = 0x7fffffffffffffffull;
constexpr uint32_t kRxPtpCtl            = 0x06c8;
constexpr uint32_t  kRxPtpSync          = 0x00000001;
constexpr uint32_t  kRxPtpDelayReq      = 0x00000002;
constexpr uint32_t  kRxPtpPdelayReq     = 0x00000004;
constexpr uint32_t  kRxPtpPdelayResp    = 0x00000008;
constexpr uint32_t  kRxPtpAllV2Events   = 0x0000000f;
constexpr uint32_t  kRxPtpV2L2Enable    = 0x00800000;
constexpr uint32_t  kRxPtpV2L4Enable    = 0x01000000;
constexpr uint32_t  kRxPtpV1Enable      = 0x02000000;
constexpr uint32_t  kRxPtpHwtsInterlock = 0x04000000;
constexpr uint32_t  kRxPtpPhase0Value   = kRxPtpV2L2Enable | kRxPtpAllV2Events |
                                          kRxPtpHwtsInterlock;

// ---------------------------------------------------------------------------
// Send / receive engines
constexpr uint32_t kSndDataIMode        = 0x0c00;
constexpr uint32_t  kSndDataIEnable     = 0x00000002;
constexpr uint32_t  kSndDataIHwTso      = 0x00000008;  // tg3 behaviour for HW_TSO parts
constexpr uint32_t kSndDataIStatsCtrl   = 0x0c08;
constexpr uint32_t  kSndDataIStatsEn    = 0x00000001;
constexpr uint32_t  kSndDataIStatsFast  = 0x00000002;
constexpr uint32_t kSndDataIStatsEnab   = 0x0c0c;
constexpr uint32_t kSndDataCMode        = 0x1000;
constexpr uint32_t  kSndDataCEnable     = 0x00000002;
constexpr uint32_t kSndBdsMode          = 0x1400;
constexpr uint32_t  kSndBdsEnable       = 0x00000002;
constexpr uint32_t  kSndBdsAttn         = 0x00000004;
constexpr uint32_t kSndBdiMode          = 0x1800;
constexpr uint32_t  kSndBdiEnable       = 0x00000002;
constexpr uint32_t  kSndBdiAttn         = 0x00000004;
constexpr uint32_t kSndBdcMode          = 0x1c00;
constexpr uint32_t  kSndBdcEnable       = 0x00000002;
constexpr uint32_t  kSndBdcAttn         = 0x00000004;
constexpr uint32_t kRcvLpcMode          = 0x2000;
constexpr uint32_t  kRcvLpcEnable       = 0x00000002;
constexpr uint32_t kRcvLpcConfig        = 0x2010;
constexpr uint32_t  kRcvLpcConfigValue  = 0x00000181;  // tg3 behaviour
constexpr uint32_t kRcvLpcStatsCtrl     = 0x2014;
constexpr uint32_t  kRcvLpcStatsCtrlEn  = 0x00000001;
constexpr uint32_t kRcvLpcStatsEnable   = 0x2018;
constexpr uint32_t  kRcvLpcStatsDackFix = 0x00040000;
constexpr uint32_t kRcvDbdiMode         = 0x2400;
constexpr uint32_t  kRcvDbdiEnable      = 0x00000002;
constexpr uint32_t  kRcvDbdiInvRingSz   = 0x00000010;
constexpr uint32_t kRcvDbdiJumboBd      = 0x2440;      // BDINFO block
constexpr uint32_t kRcvDbdiStdBd        = 0x2450;      // BDINFO block
constexpr uint32_t kRcvDccMode          = 0x2800;
constexpr uint32_t  kRcvDccEnable       = 0x00000002;
constexpr uint32_t  kRcvDccAttn         = 0x00000004;
constexpr uint32_t kRcvBdiMode          = 0x2c00;
constexpr uint32_t  kRcvBdiEnable       = 0x00000002;
constexpr uint32_t  kRcvBdiRcbAttn      = 0x00000004;
constexpr uint32_t kRcvBdiStdThresh     = 0x2c18;
constexpr uint32_t kRcvBdiJumboThresh   = 0x2c1c;
constexpr uint32_t kStdReplenishLwm     = 0x2d00;
constexpr uint32_t kJmbReplenishLwm     = 0x2d04;
constexpr uint32_t kRcvCcMode           = 0x3000;
constexpr uint32_t  kRcvCcEnable        = 0x00000002;
constexpr uint32_t  kRcvCcAttn          = 0x00000004;

// BDINFO block layout (both register-resident and NIC-SRAM-resident copies)
constexpr uint32_t kBdInfoHostAddrHi    = 0x0;
constexpr uint32_t kBdInfoHostAddrLo    = 0x4;
constexpr uint32_t kBdInfoMaxLenFlags   = 0x8;
constexpr uint32_t kBdInfoNicAddr       = 0xc;
constexpr uint32_t kBdInfoSize          = 0x10;
constexpr uint32_t  kBdInfoDisabled     = 0x00000002;
constexpr uint32_t  kBdInfoMaxLenShift  = 16;

// ---------------------------------------------------------------------------
// CPMU (57765-class writes only, tg3 behaviour)
constexpr uint32_t kCpmuLspd10MbClk     = 0x3604;
constexpr uint32_t  kCpmuLspd10MbMacMask= 0x001f0000;
constexpr uint32_t  kCpmuLspd10Mb6_25   = 0x00130000;
constexpr uint32_t kCpmuPadRngCtl       = 0x3668;
constexpr uint32_t  kCpmuPadRngRdiv2    = 0x00040000;

// ---------------------------------------------------------------------------
// Host coalescing
constexpr uint32_t kHostccMode          = 0x3c00;
constexpr uint32_t  kHostccEnable       = 0x00000002;
constexpr uint32_t kHostccRxColTicks    = 0x3c08;
constexpr uint32_t kHostccTxColTicks    = 0x3c0c;
constexpr uint32_t kHostccRxMaxFrames   = 0x3c10;
constexpr uint32_t kHostccTxMaxFrames   = 0x3c14;
constexpr uint32_t kHostccRxMaxfInt     = 0x3c20;
constexpr uint32_t kHostccTxMaxfInt     = 0x3c24;
constexpr uint32_t kHostccStatusBlkHi   = 0x3c38;
constexpr uint32_t kHostccStatusBlkLo   = 0x3c3c;

// ---------------------------------------------------------------------------
// Memory arbiter / buffer manager / DMA engines
constexpr uint32_t kMemArbMode          = 0x4000;
constexpr uint32_t  kMemArbEnable       = 0x00000002;
constexpr uint32_t kBufMgrMode          = 0x4400;
constexpr uint32_t  kBufMgrEnable       = 0x00000002;
constexpr uint32_t  kBufMgrAttn         = 0x00000004;
constexpr uint32_t kBufMgrMbRdmaLowWm   = 0x4410;
constexpr uint32_t kBufMgrMbMacRxLowWm  = 0x4414;
constexpr uint32_t kBufMgrMbHighWm      = 0x4418;
constexpr uint32_t kBufMgrDmaLowWm      = 0x4434;
constexpr uint32_t kBufMgrDmaHighWm     = 0x4438;
// 57765_PLUS watermarks (tg3 behaviour).
constexpr uint32_t  kMbRdmaLowWm57765   = 0x00000000;
constexpr uint32_t  kMbMacRxLowWm57765  = 0x0000002a;
constexpr uint32_t  kMbHighWm57765      = 0x000000a0;
constexpr uint32_t  kDmaLowWmDefault    = 0x00000005;
constexpr uint32_t  kDmaHighWmDefault   = 0x0000000a;
constexpr uint32_t kRdmacMode           = 0x4800;
constexpr uint32_t  kRdmacEnable        = 0x00000002;
constexpr uint32_t  kRdmacErrEnables    = 0x000003fc;  // TGTABORT..LNGREAD (bits 2-9)
constexpr uint32_t  kRdmacFifoLongBurst = 0x00030000;
constexpr uint32_t  kRdmacJmb2kMmrr     = 0x00800000;
constexpr uint32_t  kRdmacIpv4LsoEn     = 0x08000000;
constexpr uint32_t  kRdmacIpv6LsoEn     = 0x10000000;
constexpr uint32_t kRdmaRsrvCtrl        = 0x4900;
constexpr uint32_t  kRdmaRsrvFifoOflwFix= 0x00000004;
constexpr uint32_t kWdmacMode           = 0x4c00;
constexpr uint32_t  kWdmacEnable        = 0x00000002;
constexpr uint32_t  kWdmacErrEnables    = 0x000003fc;  // TGTABORT..LNGREAD (bits 2-9)
constexpr uint32_t  kWdmacStatusTagFix  = 0x20000000;
constexpr uint32_t kFtqReset            = 0x5c00;

// ---------------------------------------------------------------------------
// GRC
constexpr uint32_t kGrcMode             = 0x6800;
constexpr uint32_t  kGrcWswapNonFrm     = 0x00000004;
constexpr uint32_t  kGrcBswapData       = 0x00000010;
constexpr uint32_t  kGrcWswapData       = 0x00000020;
constexpr uint32_t  kGrcSwapLittleEndian= kGrcWswapData | kGrcBswapData | kGrcWswapNonFrm;
constexpr uint32_t  kGrcHostStackup     = 0x00010000;
constexpr uint32_t  kGrcHostSendBds     = 0x00020000;
constexpr uint32_t  kGrcTimeSyncEnable  = 0x00080000;
constexpr uint32_t  kGrcNoTxPhdrCsum    = 0x00100000;
constexpr uint32_t  kGrcNoRxPhdrCsum    = 0x00800000;
constexpr uint32_t  kGrcIrqOnMacAttn    = 0x04000000;
constexpr uint32_t  kGrcPciePortMask    = 0xa0400000;  // PL/DL select + HI_1K (TL = 0)
constexpr uint32_t  kGrcPcieDlSel       = 0x20000000;
constexpr uint32_t kGrcMiscCfg          = 0x6804;
constexpr uint32_t  kGrcMiscCoreClkReset= 0x00000001;
constexpr uint32_t  kGrcMiscPcieBit29   = 0x20000000;  // set with core reset on PCIe (tg3 behaviour)
constexpr uint32_t  kGrcMiscPrescaleMask= 0x000000ff;
constexpr uint32_t  kGrcMiscPrescale66  = 65u << 1;    // 66 MHz -> 1 us timer tick
constexpr uint32_t kGrcLocalCtrl        = 0x6808;
constexpr uint32_t kGrcFastbootPc       = 0x6894;      // zeroed before core reset on 5755_PLUS (tg3 behaviour)

// EAV time-sync block
constexpr uint32_t kEavRefClkLsb        = 0x6900;
constexpr uint32_t kEavRefClkMsb        = 0x6904;
constexpr uint32_t kEavRefClkCtl        = 0x6908;
constexpr uint32_t  kEavCtlStop         = 0x00000002;
constexpr uint32_t  kEavCtlResume       = 0x00000004;
constexpr uint32_t  kEavCtlGpioMask     = 0x00030000;  // never touched in Phase 0
constexpr uint32_t kEavWatchdog0Lsb     = 0x6918;      // never touched in Phase 0
constexpr uint32_t kEavWatchdog0Msb     = 0x691c;      // never touched in Phase 0
constexpr uint32_t kEavRefClkCorrectCtl = 0x6928;
constexpr uint32_t  kEavCorrectEnable   = 0x80000000;
constexpr uint32_t  kEavCorrectNegative = 0x40000000;
constexpr uint32_t  kEavCorrectMagMask  = 0x00ffffff;

// PCIe DL port window (accessed with GRC_MODE DL select; tg3 behaviour)
constexpr uint32_t kPcieTlDlPlPort      = 0x7c00;
constexpr uint32_t kPcieDlLoFtsMax      = 0x000c;
constexpr uint32_t  kPcieFtsMaxMask     = 0x000000ff;
constexpr uint32_t  kPcieFtsMaxValue    = 0x0000002c;

// ---------------------------------------------------------------------------
// NIC SRAM (through the MEM_WIN window)
constexpr uint32_t kSramSendRcb         = 0x0100;
constexpr uint32_t kSramRxRetRcb        = 0x0200;
constexpr uint32_t kSramFirmwareMbox    = 0x0b50;
constexpr uint32_t  kFirmwareMagic      = 0x4b657654;
constexpr uint32_t kSramDataSig         = 0x0b54;
constexpr uint32_t  kDataSigMagic       = 0x4b657654;
constexpr uint32_t kSramDataCfg         = 0x0b58;
constexpr uint32_t  kDataCfgAsfEnable   = 0x00000080;
constexpr uint32_t  kDataCfgApeEnable   = 0x00200000;
constexpr uint32_t kSramTxBufferDesc    = 0x4000;
constexpr uint32_t kSramRxBufferDesc    = 0x6000;

// ---------------------------------------------------------------------------
// MII (clause 22) registers on the internal PHY
constexpr uint32_t kPhyAddr             = 1;           // 57765-class internal PHY
constexpr uint32_t kMiiBmcr             = 0x00;
constexpr uint16_t  kBmcrAnRestart      = 0x0200;
constexpr uint16_t  kBmcrAnEnable       = 0x1000;
constexpr uint32_t kMiiBmsr             = 0x01;
constexpr uint16_t  kBmsrLinkStatus     = 0x0004;
constexpr uint16_t  kBmsrAnComplete     = 0x0020;
constexpr uint32_t kMiiAuxStat          = 0x19;
constexpr uint16_t  kAuxStatSpeedMask   = 0x0700;
constexpr uint16_t  kAuxStat100Half     = 0x0300;
constexpr uint16_t  kAuxStat100Full     = 0x0500;
constexpr uint16_t  kAuxStat1000Half    = 0x0600;
constexpr uint16_t  kAuxStat1000Full    = 0x0700;

// ---------------------------------------------------------------------------
// Ring geometry for this class (tg3 behaviour: no LRG_PROD_RING_CAP, jumbo-capable)
constexpr uint32_t kStdRingSize         = 512;
constexpr uint32_t kRxRetRingSize       = 1024;
constexpr uint32_t kTxRingSize          = 512;
constexpr uint32_t kRxStdDmaSize        = 1536;
constexpr uint32_t kStdBdCacheSize      = 128;         // 57765_PLUS
constexpr uint32_t kJmbBdCacheSize      = 64;
constexpr uint32_t kStatusBlockSize     = 0x50;
// NIC-SRAM ring control blocks present on 57765_CLASS (tg3 behaviour): send
// RCBs 0..1 and RX-return RCBs 0..3; all but RCB 0 are disabled.
constexpr uint32_t kSendRcbCount        = 2;
constexpr uint32_t kRxRetRcbCount       = 4;
// Default jumbo-ring pending count used only for the jumbo replenish threshold.
constexpr uint32_t kJumboPendingDefault = 100;
constexpr uint32_t kMacRxMtuValue       = 1500 + 14 + 4 + 4;  // MTU + ETH_HLEN + FCS + VLAN

// Descriptor flags
constexpr uint32_t kTxdFlagEnd          = 0x0004;
constexpr uint32_t kTxdFlagHwTstamp     = 0x0020;
constexpr uint32_t kTxdLenShift         = 16;
constexpr uint32_t kRxdFlagEnd          = 0x0004;
constexpr uint32_t kRxdFlagVlan         = 0x0040;
constexpr uint32_t kRxdFlagError        = 0x0400;
constexpr uint32_t kRxdPtpStatMask      = 0x0210;
constexpr uint32_t kRxdPtpStatV1        = 0x0010;
constexpr uint32_t kRxdPtpStatV2        = 0x0200;
constexpr uint32_t kRxdOpaqueRingStd    = 0x00010000;
constexpr uint32_t kRxdOpaqueIndexMask  = 0x0000ffff;
// err_vlan error bits: BAD_CRC, COLLISION, LINK_LOST, PHY_DECODE, MAC_ABRT,
// TOO_SMALL, NO_RESOURCES, HUGE_FRAME (odd-nibble 0x00100000 is not an error).
constexpr uint32_t kRxdErrMask          = 0x01ef0000;

// Status block byte offsets (little-endian layout with WSWAP_NONFRM)
constexpr uint32_t kSbStatus            = 0x00;
constexpr uint32_t kSbRxStdConsumer     = 0x0a;        // u16
constexpr uint32_t kSbRxProducer0       = 0x10;        // u16 (return ring 0 producer)
constexpr uint32_t kSbTxConsumer0       = 0x12;        // u16

}  // namespace bcm

#endif  // BCM57762_REGS_H
