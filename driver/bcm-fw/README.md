# bcm-fw — BCM57762 RX-CPU enable shim (clean-room)

A tiny firmware image that runs on the Broadcom BCM57762's on-die RX CPU. Its
only job is to arm the MAC's hardware RX-PTP timestamp capture from the core —
the one place the AVB classifier and `RX_PTP_CTL` writes stick — then park.
The host RX path then reads the hardware timestamp the normal documented way.

Full rationale, feasibility and the load/build plan: `docs/bcm-firmware-shim-design.md`
and `docs/bcm-rx-timestamp-investigation.md`.

## Clean-room boundary

This is our own source. The register offsets and the classifier write values are
hardware facts observed read-only from Apple's driver (already recorded in
`driver/AVBBcmDriver/Tg3Logic.hpp`) and from public tg3 register documentation.
No GPL source (ThunderGate, Linux tg3) is copied; those were references for the
hardware programming model only. The MIPS toolchain is ThunderGate's
`build-toolchain.sh` (an external build tool), not part of this image.

## Files

- `entry.S`   — reset stub: set stack, zero bss, call `shim_main`, park.
- `shim.c`    — apply the classifier + `RX_PTP_CTL` enable, then park with a heartbeat.
- `regs.h`    — the registers/values this shim touches (RX-CPU address views).
- `image.ld`  — flat image at `0x08000000`, stack at `0x08010000`.
- `Makefile`  — build with `mips-elf-gcc -mtigon` → `bcmfw.img`.
- `bcmfw.img` — the built flat image (committed; consumers don't need the toolchain).

## Build

```
make -C driver/bcm-fw                       # uses .agents/tmp/toolchain by default
make -C driver/bcm-fw TOOLCHAIN=/path/to/tc # override the toolchain location
```

`scripts/package.sh` rebuilds `bcmfw.img` automatically when the toolchain is
present, and otherwise uses the committed copy.

## Host handshake

The shim writes signatures into scratch so `avb-probe` can confirm it ran:

| scratch addr | meaning            | value         |
|--------------|--------------------|---------------|
| `0x0800F000` | shim started       | `0x53484D31` ("SHM1") |
| `0x0800F004` | config applied     | `0x444F4E45` ("DONE") |
| `0x0800F008` | heartbeat (parked) | increments    |
