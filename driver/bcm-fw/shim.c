/*
 * BCM57762 RX-CPU enable shim (clean-room).
 *
 * Runs on the chip's on-die RX CPU. Its only job is to apply, from the core
 * (where the writes stick -- see the halt test in
 * docs/bcm-rx-timestamp-investigation.md), the AVB RX classifier and the
 * RX_PTP_CTL enable that the host cannot make stick over MMIO, then park. Once
 * armed, the MAC latches RX PTP timestamps into 0x6b0/0x6b4 and flags each
 * frame's descriptor; the host RX path consumes that normally.
 *
 * The classifier write list is the exact ordered sequence Apple's driver was
 * observed making (also in Tg3Logic.hpp). Those are hardware facts, not code.
 */
#include "regs.h"

struct reg_write {
    uint32_t off;
    uint32_t val;
};

/* Apple AVB RX classifier block (0x5400-0x561c), duplicates and order preserved
 * (0x5494/0x5498 are indexed ports written twice). 0x0560 is the final low-reg
 * write Apple makes. Applied from the core at REG(off) == 0xC0000000+off. */
static const struct reg_write kClassifier[] = {
    { 0x5400, 0x8400e000 }, { 0x5404, 0x8400e000 }, { 0x5408, 0x8400000c },
    { 0x540c, 0x8400000c }, { 0x5410, 0x8400000c }, { 0x5494, 0x88f7ffff },
    { 0x5414, 0x84006002 }, { 0x5494, 0x013fffff }, { 0x5418, 0x84006002 },
    { 0x5498, 0x0140ffff }, { 0x5500, 0x00000005 }, { 0x5504, 0x0000000a },
    { 0x5508, 0x00000010 }, { 0x550c, 0x00000020 }, { 0x5510, 0x00000040 },
    { 0x5608, 0x00000000 }, { 0x560c, 0x000000e0 }, { 0x5610, 0x00000000 },
    { 0x5614, 0x000000e1 }, { 0x5618, 0x00000025 }, { 0x561c, 0x00e4e3a2 },
    { 0x0560, 0x0000000e },
};

void shim_main(void)
{
    SCRATCH(SHIM_SCRATCH_RUN) = SHIM_SIG_RUN; /* host: the shim is executing */

    for (unsigned i = 0; i < sizeof(kClassifier) / sizeof(kClassifier[0]); i++) {
        REG(kClassifier[i].off) = kClassifier[i].val;
    }
    REG(RX_PTP_CTL) = RX_PTP_ENABLE; /* arm the RX-PTP hardware capture */

    SCRATCH(SHIM_SCRATCH_DONE) = SHIM_SIG_DONE; /* host: config applied */

    /* Park. The heartbeat lets the host confirm the core stayed alive (and did
     * not fault on a classifier/PTP write). Keep it a tight, side-effect-only
     * loop -- the enable is one-shot; the MAC does the timestamping. */
    volatile uint32_t hb = 0;
    for (;;) {
        SCRATCH(SHIM_SCRATCH_HB) = ++hb;
    }
}
