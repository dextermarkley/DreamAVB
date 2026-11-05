/*
 * Clean-room register definitions for the BCM57762 RX-CPU enable shim.
 *
 * These are hardware facts (register offsets and the values Apple's driver was
 * observed writing, already recorded in driver/AVBBcmDriver/Tg3Logic.hpp), not
 * copied code. The RX CPU sees the chip register file at virtual base
 * 0xC0000000 + block_offset, and its scratch pad at 0x08000000 (confirmed by
 * M1 and by ThunderGate's symbol map). Nothing here is derived from GPL source.
 */
#pragma once
#include <stdint.h>

/* RX-CPU views of the two address spaces it touches. */
#define TG_REG_BASE 0xC0000000u
#define REG(off)     (*(volatile uint32_t *)(TG_REG_BASE + (uint32_t)(off)))
#define SCRATCH(off) (*(volatile uint32_t *)(0x08000000u + (uint32_t)(off)))

/* RX PTP timestamp control (tg3 RX_PTP_CTL at 0x6c8). Arming it makes the MAC
 * latch a matched frame's arrival time into RX_TSTAMP (0x6b0/0x6b4) and set the
 * PTPSTAT code in the frame's RX descriptor. See docs/bcm-rx-timestamp-investigation.md. */
#define RX_PTP_CTL          0x06c8u
#define RX_PTP_V2_L2_EN     0x00800000u
#define RX_PTP_ALL_V2       0x0000000fu
#define RX_PTP_HWTS_ILOCK   0x04000000u
#define RX_PTP_ENABLE       (RX_PTP_V2_L2_EN | RX_PTP_ALL_V2 | RX_PTP_HWTS_ILOCK)

/* Host<->shim handshake, written to scratch so avb-probe can confirm the shim
 * ran. Kept well clear of the code (bottom) and the stack (top of the pad). */
#define SHIM_SCRATCH_RUN   0xF000u   /* "SHM1" once the shim starts */
#define SHIM_SCRATCH_DONE  0xF004u   /* "DONE" once the config is applied */
#define SHIM_SCRATCH_HB    0xF008u   /* heartbeat counter while parked */
#define SHIM_SIG_RUN       0x53484D31u  /* 'S','H','M','1' */
#define SHIM_SIG_DONE      0x444F4E45u  /* 'D','O','N','E' */
