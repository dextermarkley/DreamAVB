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

/* ---- Frame-stamping path (clean-room; offsets/bits from the Tigon3 register
 * model, hardware facts). The RX CPU routes gPTP (ethertype 0x88F7) to itself
 * via a basic EMAC receive rule, is woken per frame through the RDIQ event,
 * reads the EAV clock, and records a per-frame timestamp to a scratch ring the
 * host reads with ReadCoreMem. ---- */

/* Bit numbering: the Tigon3 register model's C bitfields are laid out for a
 * big-endian MIPS ABI, where the FIRST listed field is the MOST significant
 * bit. (Verified with our mips-elf-gcc, against tg3's rule 0 value 0xc2000000
 * / RCV_RULE_DISABLE_MASK 0x7fffffff, and against our hardware readback of
 * 0xFFFFFFFF -> 0xE7FFFFFF = the two reserved bits 28/27.) An earlier revision
 * decoded these LSB-first, which left the rule disabled. */

/* EMAC basic receive rules at 0x480 (rule n: control 0x480+n*8, value +4).
 * control: enable=b31, and_with_next=b30, activate_rxcpu=b29, (b28/b27
 * reserved), mask=b26, discard=b25, map=b24, comparison_op=b17-16,
 * header_type=b15-13, pclass=b12-8, offset=b7-0. */
#define RX_RULE_CTRL(n)   (0x0480u + (n) * 8u)
#define RX_RULE_VALUE(n)  (0x0484u + (n) * 8u)
#define RX_RULE_GPTP      4u                 /* a free rule index */
#define RULE_ENABLE       0x80000000u
#define RULE_ACT_RXCPU    0x20000000u
#define RULE_MASK         0x04000000u
#define RULE_PCLASS(c)    ((uint32_t)(c) << 8)
#define RULE_OFFSET(o)    ((uint32_t)(o) & 0xFFu)
/* offset=12 (ethertype), pclass=1, mask=1, activate_rxcpu=1, enable=1 */
#define RULE_GPTP_FIELDS  (RULE_ACT_RXCPU | RULE_MASK | RULE_PCLASS(1) | RULE_OFFSET(12))
#define RULE_GPTP_CTRL    (RULE_ENABLE | RULE_GPTP_FIELDS)   /* 0xA400010C */
#define RULE_GPTP_VALUE   0xFFFF88F7u        /* as ThunderGate: 0xffff0000 | etype */

/* RX-CPU event (GRC): status 0x6810, enable 0x684c; emac=b25, rdiq=b8. */
#define RXCPU_EVENT       0x6810u
#define RXCPU_EVENT_EN    0x684Cu
#define EVENT_EMAC        0x02000000u
#define EVENT_RDIQ        0x00000100u

/* FTQ RDIQ peek (0x5c00 + 0xf0 + 0xc): valid=b20, skip=b19, pass=b18,
 * head_rxmbuf_ptr=b17-9, tail_rxmbuf_ptr=b8-0. */
#define FTQ_RDIQ_PEEK     0x5CFCu
#define PEEK_VALID        0x00100000u
#define PEEK_SKIP         0x00080000u
#define PEEK_PASS         0x00040000u
#define PEEK_HEAD_SHIFT   9u
#define PEEK_HEAD_MASK    0x1FFu
#define PEEK_MBUFS_MASK   0x3FFFFu           /* head|tail mbuf pointers */
/* FTQ mbuf-cluster free queue enqueue (0x5c00 + 0xc0 + 0x8). */
#define FTQ_MBUF_FREE_Q   0x5CC8u

/* EAV reference clock low 32 bits (the per-frame timestamp source). */
#define EAV_REF_CLK_LSB   0x6900u
/* MAC RX timestamp latch (tg3 TG3_RX_TSTAMP_LSB). */
#define RX_TSTAMP_LSB     0x06B0u

/* Receive mbuf pool in NIC memory (CPU view): base 0x10000, 128-byte entries;
 * data at +0x8, ethertype at data.word[13] >> 16 (40-byte frame desc prefix). */
#define RXMBUF_BASE       0x00010000u
#define MBUF_SIZE         128u
#define MBUF_DATA_OFF     0x8u
#define ETHERTYPE_WORD    13u
#define GPTP_ETHERTYPE    0x88F7u

/* Scratch channel the host polls with ReadCoreMem. Ring of {ts,seq} pairs
 * plus diagnostic counters, all well clear of code (bottom) and stack (top). */
/* These are SCRATCH OFFSETS (SCRATCH() adds the 0x08000000 base); the host
 * reads them at 0x08000000+offset via ReadCoreMem. */
#define TS_PROD           0xBFF0u  /* ring producer index (host: 0x0800BFF0) */
#define TS_COUNT          0xBFF4u  /* gPTP frames stamped   (host: 0x0800BFF4) */
#define TS_PEEKS          0xBFF8u  /* ALL valid RDIQ events (host: 0x0800BFF8) */
#define TS_LASTETYPE      0xBFFCu  /* last ethertype seen   (host: 0x0800BFFC) */
#define TS_RING_BASE      0xC000u  /* ring of {ts,seq}      (host: 0x0800C000) */
#define TS_RING_ENTRIES   256u     /* power of two */
#define TS_MAC_TS         0xBFECu  /* last RX_TSTAMP_LSB read (host: 0x0800BFEC) */
#define TS_MAC_RING       0xD000u  /* per-slot RX_TSTAMP_LSB (host: 0x0800D000) */
#define DIAG_GRC_BEFORE   0xBFE0u  /* GRC_MODE before EAV-mode write */
#define DIAG_GRC_AFTER    0xBFE4u  /* GRC_MODE after */
#define DIAG_PTPCTL_AFTER 0xBFE8u  /* RX_PTP_CTL readback after the core's write */

/* GRC mode (0x6800). Bit 18: eav_mode_enable in the Tigon3 register model
 * (MSB-first field 14); tg3 names the same bit GRC_MODE_HTX2B_ENABLE. */
#define GRC_MODE_REG        0x6800u
#define GRC_EAV_MODE_ENABLE 0x00040000u
/* Absolute NIC-memory read (not a scratch offset): rxmbuf etc. */
#define NIC_MEM(addr)     (*(volatile uint32_t *)(uint32_t)(addr))
