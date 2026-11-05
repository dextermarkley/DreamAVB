/*
 * BCM57762 RX-CPU gPTP frame-stamping firmware (clean-room).
 *
 * Runs on the chip's on-die RX CPU. It routes gPTP frames (ethertype 0x88F7)
 * to the RX CPU via a basic EMAC receive rule, is woken per frame by the RDIQ
 * event, reads the EAV reference clock, and records a per-frame timestamp into
 * a scratch ring that our host dext reads with ReadCoreMem. This is our own
 * timestamp-delivery scheme end to end -- no Apple framework, no capture.
 *
 * Milestone 1 goal: prove the core sees gPTP frames and can stamp them.
 * Diagnostics (TS_PEEKS, TS_LASTETYPE) distinguish "rule didn't route"
 * from "mbuf access wrong" from "working".
 *
 * !!! BLOCKED (2026-11-04, hardware-proven) !!!
 * This firmware's receive path does NOT yet work on the BCM57762 in our
 * hardware-DMA datapath. Proven on hardware (see docs/bcm-firmware-shim-design.md):
 *   - The RX CPU runs our code and reads the EAV clock live (0x6900 advances).
 *   - Our scratch-ring delivery round-trips perfectly (sentinel test passed).
 *   - BUT setting activate_rxcpu on a receive rule does NOT divert frames to
 *     the RX CPU's RDIQ: the RDIQ count/peek (0x5cf4/0x5cfc) stay 0 and gPTP
 *     keeps reaching the host ring. The RX CPU is not in the RX datapath, so
 *     the per-frame RDIQ wake below never fires.
 * Making this work needs a ThunderGate-style full receive-processor bring-up
 * (the core owns RX and forwards frames to the host via WDMA/return rings),
 * which is a much larger firmware than this stamper. Left here as the correct
 * stamping logic pending that routing work.
 *
 * Clean-room: register offsets and bit layouts are hardware facts (see regs.h);
 * ThunderGate's rx.c was a programming-model reference only, not copied.
 */
#include "regs.h"

void shim_main(void)
{
    SCRATCH(SHIM_SCRATCH_RUN) = SHIM_SIG_RUN;

    /* Route gPTP (0x88F7) to the RX CPU: clear the rule, set its match value,
     * then enable it with activate_rxcpu. */
    REG(RX_RULE_CTRL(RX_RULE_GPTP)) = 0;
    REG(RX_RULE_VALUE(RX_RULE_GPTP)) = RULE_GPTP_VALUE;
    REG(RX_RULE_CTRL(RX_RULE_GPTP)) = RULE_GPTP_CTRL;

    /* Wake the RX CPU on EMAC receive (RDIQ) events. */
    REG(RXCPU_EVENT_EN) = REG(RXCPU_EVENT_EN) | EVENT_EMAC | EVENT_RDIQ;

    SCRATCH(SHIM_SCRATCH_DONE) = SHIM_SIG_DONE;

    SCRATCH(TS_PROD) = 0;
    SCRATCH(TS_COUNT) = 0;
    SCRATCH(TS_PEEKS) = 0;
    SCRATCH(TS_LASTETYPE) = 0;

    uint32_t hb = 0;
    uint32_t prod = 0;
    uint32_t count = 0;
    uint32_t peeks = 0;

    for (;;) {
        uint32_t peek = REG(FTQ_RDIQ_PEEK);
        if (peek & PEEK_VALID) {
            peeks++;
            SCRATCH(TS_PEEKS) = peeks;

            const uint32_t head = (peek >> PEEK_HEAD_SHIFT) & PEEK_HEAD_MASK;
            const uint32_t dword =
                NIC_MEM(RXMBUF_BASE + head * MBUF_SIZE + MBUF_DATA_OFF + ETHERTYPE_WORD * 4u);
            const uint32_t etype = dword >> 16;
            SCRATCH(TS_LASTETYPE) = etype;

            if (etype == GPTP_ETHERTYPE) {
                const uint32_t ts = REG(EAV_REF_CLK_LSB);
                const uint32_t slot = prod & (TS_RING_ENTRIES - 1u);
                SCRATCH(TS_RING_BASE + slot * 8u) = ts;
                SCRATCH(TS_RING_BASE + slot * 8u + 4u) = count;
                prod++;
                count++;
                SCRATCH(TS_PROD) = prod;
                SCRATCH(TS_COUNT) = count;
            }

            /* Advance the RDIQ (set skip) WITHOUT freeing the mbuf clusters, so
             * the frame continues to the host return ring, then clear the event.
             * Whether skip-without-free truly forwards is the hardware question
             * this milestone answers. */
            REG(FTQ_RDIQ_PEEK) = peek | PEEK_SKIP;
            REG(RXCPU_EVENT) = REG(RXCPU_EVENT) & ~EVENT_RDIQ;
        }
        SCRATCH(SHIM_SCRATCH_HB) = ++hb;
    }
}
