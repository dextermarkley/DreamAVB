/*
 * BCM57762 RX-CPU gPTP frame-stamping firmware (clean-room).
 *
 * Runs on the chip's on-die RX CPU. It routes gPTP frames (ethertype 0x88F7)
 * to the RX CPU via a basic EMAC receive rule, is woken per frame by the RDIQ
 * event, reads the EAV reference clock, and records a per-frame timestamp into
 * a scratch ring that our host dext reads with ReadCoreMem. This is our own
 * timestamp-delivery scheme end to end -- no Apple framework, no capture.
 *
 * Status (2026-11-05, hardware): WORKS. The core receives every gPTP frame
 * from the 16R, reads its PTP messageType and sequenceId from the mbuf,
 * stamps it from the EAV clock, and passes it on, so the same frame also
 * reaches the host ring. See docs/bcm-firmware-shim-design.md (2026-11-05,
 * "Bit-order correction").
 *
 * The earlier "blocked" verdict came from decoding the Tigon3 register
 * bitfields LSB-first; they are MSB-first (see regs.h). With that, the
 * receive rule was never enabled.
 *
 * Open: the stamp is the EAV clock read when the core services the frame,
 * so it includes RDIQ queueing latency. The MAC's own RX_TSTAMP latch
 * (0x6b0) still reads 0 and RX_PTP_CTL does not hold its value.
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
    REG(RX_RULE_CTRL(RX_RULE_GPTP)) = RULE_GPTP_FIELDS;
    REG(RX_RULE_CTRL(RX_RULE_GPTP)) = RULE_GPTP_CTRL;

    /* Arm the MAC's own RX PTP latch (V2 L2 events + interlock) so each frame's
     * hardware arrival time can be compared with the core's EAV read. */
    REG(RX_PTP_CTL) = RX_PTP_ENABLE;

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
        if ((peek & PEEK_VALID) && !(peek & PEEK_PASS)) {
            peeks++;
            SCRATCH(TS_PEEKS) = peeks;

            const uint32_t head = (peek >> PEEK_HEAD_SHIFT) & PEEK_HEAD_MASK;
            const uint32_t dword =
                NIC_MEM(RXMBUF_BASE + head * MBUF_SIZE + MBUF_DATA_OFF + ETHERTYPE_WORD * 4u);
            const uint32_t etype = dword >> 16;
            SCRATCH(TS_LASTETYPE) = etype;

            if (etype == GPTP_ETHERTYPE) {
                const uint32_t ts = REG(EAV_REF_CLK_LSB);
                /* PTP header: messageType = low nibble of frame byte 14 (data
                 * word 13 bits 11-8); sequenceId = frame bytes 44-45 (data
                 * word 21 high half). Data word n = mbuf + 8 + 4n. */
                const uint32_t mb = RXMBUF_BASE + head * MBUF_SIZE + MBUF_DATA_OFF;
                const uint32_t mtype = (dword >> 8) & 0xFu;
                const uint32_t seq = NIC_MEM(mb + 21u * 4u) >> 16;
                const uint32_t slot = prod & (TS_RING_ENTRIES - 1u);
                SCRATCH(TS_RING_BASE + slot * 8u) = ts;
                SCRATCH(TS_RING_BASE + slot * 8u + 4u) = (mtype << 16) | seq;
                /* MAC hardware latch for the same frame (LSB then MSB, which
                 * releases the interlock), parked in a parallel ring. */
                const uint32_t mac_lo = REG(RX_TSTAMP_LSB);
                (void)REG(RX_TSTAMP_LSB + 4u);
                SCRATCH(TS_MAC_RING + slot * 4u) = mac_lo;
                SCRATCH(TS_MAC_TS) = mac_lo;
                prod++;
                count++;
                SCRATCH(TS_PROD) = prod;
                SCRATCH(TS_COUNT) = count;
            }

            /* Consume the frame: advance the RDIQ (skip) and return its mbuf
             * cluster (head+tail = low 18 bits) to the free queue, as the
             * Tigon3 programming model does. Without the free the rxmbuf pool
             * drains and all receive stalls. Forwarding to the host is a later
             * step; for now routed gPTP frames end at the core. */
#ifdef FWD_CONSUME
            /* Consume: skip + return the mbuf cluster to the free queue. The
             * frame then never reaches the host (verified on hardware). */
            REG(FTQ_RDIQ_PEEK) = peek | PEEK_SKIP;
            REG(FTQ_MBUF_FREE_Q) = peek & PEEK_MBUFS_MASK;
#else
            /* Pass: hand the frame on to normal receive placement, so it
             * still reaches the host return ring (verified on hardware: every
             * stamped sequenceId also arrived at the host). */
            REG(FTQ_RDIQ_PEEK) = peek | PEEK_PASS;
#endif
            REG(RXCPU_EVENT) = REG(RXCPU_EVENT) & ~EVENT_RDIQ;
        }
        SCRATCH(SHIM_SCRATCH_HB) = ++hb;
    }
}
