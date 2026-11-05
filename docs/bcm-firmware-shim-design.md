# BCM57762 RX-timestamp enable-shim: firmware design + build/load plan

Status: design draft, 2026-11-04. Prereq reading:
`docs/bcm-rx-timestamp-investigation.md` (why this is needed and the resolved
feasibility). No code has been written yet; this is the plan to review before
building.

## 1. Goal and principle

Get hardware RX PTP timestamps out of the Broadcom BCM57762 (Apple Thunderbolt
Gigabit Ethernet adapter, `14e4:1682` / `106b:00f6`) by running a **minimal
enable shim** on the chip's on-die RX CPU — the one place the AVB classifier
and `RX_PTP_CTL` writes actually stick (see the halt test: host-side writes to
`0x6c8` and the `0x5400` block read back 0; a core-side write is expected to
stick).

Principles:
- **Smallest thing that works.** The shim arms the MAC's documented RX-PTP
  capture, then gets out of the way. It is not a NIC firmware and does not move
  packets. The host dext keeps driving the normal RX ring.
- **Clean-room.** We write our own shim source. ThunderGate (GPLv3) and Linux
  `tg3` (GPL) are *programming-model and behaviour references only*, per
  AGENTS.md. The register offsets/values we replay are hardware facts we
  observed read-only from Apple's driver (already recorded in `Tg3Logic.hpp`).
- **RAM only, never NVRAM.** The image is loaded into the RX-CPU scratch pad
  (volatile). We never write the adapter's flash. A power cycle fully restores
  the factory state. This keeps us clear of the AGENTS.md "ask before NVRAM /
  boot security" rule and makes every experiment reversible.

## 2. What the shim does

The MAC's RX-PTP timestamp path is documented hardware (Linux `tg3.h`): on a
matched frame the MAC latches the EAV reference clock (`0x6900`) into the
`RX_TSTAMP` register pair (`0x6b0`/`0x6b4`) and sets a PTP-status code in the RX
descriptor `type_flags` (`RXD_FLAG_PTPSTAT_MASK 0x0210`, V1 `0x0010`, V2
`0x0200`). On ordinary tg3 PTP parts the host just writes `RX_PTP_CTL 0x6c8`
and it works; on the 57762 those config registers are RX-CPU-owned, so the
write has to come from the core.

Shim lifecycle:
1. **Hardware housekeeping** equivalent to ThunderGate `init()` so replacing the
   resident firmware does not break basic operation: enable the memory arbiter
   and buffer manager (`ma.mode.enable`, `bufman.mode.enable`), set the CPMU
   clock policies, set the timer prescaler. (Scope to confirm on hardware; keep
   only what basic RX actually needs.)
2. **Arm the RX-PTP capture.** Replay the exact classifier/config writes Apple
   makes — the 22 ordered writes already captured in
   `kAvbRxRuleWrites[]` (the `0x5400`–`0x561c` block, ethertype `0x88F7`) —
   then set `RX_PTP_CTL 0x6c8` = `V2_L2_EN | ALL_V2_EVENTS | HWTS_INTERLOCK`
   (`0x0480000f`, our existing `kRxPtpPhase0Value`). Executed from the core,
   where the writes stick.
   - Deliberately we do **not** set the receive rules' `activate_rxcpu` bit for
     the gPTP frames: we are not stealing the frame to the core, we are only
     arming the timestamp unit. The frame must continue to the host RX ring.
     Replaying Apple's exact block (which produces host-path timestamps in
     Apple's own driver) is the safe encoding; the ThunderGate `emac.rx_rule`
     named model is the cross-check for what each bit means.
3. **Park.** Enter a tiny idle loop (or halt) and leave the hardware armed.
   - Open question (test on hardware): whether the armed config persists after
     the core parks. If it does, the shim is run-once. If the config is
     core-local and decays, the shim stays resident in a minimal loop that does
     nothing else. Either way it is small.

Explicitly **out of scope**: the on-core TCP/IP stack, zForth/uBASIC, the gate
control protocol, beaconing, DMA engines. None are built.

## 3. Memory map and image format (from ThunderGate `fw/image.x`)

- RX-CPU scratch pad: `0x08000000`–`0x0800FFFF` (64 KB). Image loads at
  `0x08000000`; stack at `0x08010000` growing down; text/rodata/data/bss grow
  up and must stop below `0x0800E000`. Apple's own on-NIC image loads at the
  same base (~40 KB), so our few-KB shim fits with vast headroom.
- Output is a **flat binary** (`objcopy -O binary` of `.text/.rodata/.data/
  .bss`), loaded verbatim to `0x08000000`. This matches the per-segment
  `FWIMG_INFO` layout we decoded from Apple's `loadFirmware`.
- Start PC: `0x08000000` (the reset/entry stub).

## 4. Toolchain (from ThunderGate `misc/build-toolchain.sh` + `fw/Makefile`)

- `mips-elf-gcc` / `ld` / `objcopy`, a custom `-mtigon` target (GCC 14), built
  by `misc/build-toolchain.sh`. The core is MIPS (confirmed by our own opcode
  read and the OpenBSD Tigon=R4000 note).
- Compile flags of record: `-mips2 -march=r6000 -mfix-r4000 -msoft-float -G 0
  -fno-pic -mno-abicalls -mno-llsc -mtigon -ffreestanding`. Link:
  `-nostdlib -T <our linker script> -static`, plus `libgcc` for soft
  multiply/divide.
- Register-block base addresses come from a generated `fixed.sym`
  (`py/symgen.py` from the datamodel). For a clean-room shim we instead define
  our own small header of only the registers we touch, with the offsets we
  already use in `Bcm57762Regs.h` / `Tg3Logic.hpp` — no dependency on
  ThunderGate's generated symbols.

## 5. Load protocol (host side, in our dext)

ThunderGate `py/blocks/cpu.py::image_load` and Apple's `loadFirmware` agree on
the mechanism, which our dext already has the primitives for (`wr`/`rd`, the
HaltCpu sequence we added in `programAvbRxRules`):

1. **Halt the RX CPU** — `STATE = 0xffffffff`, `MODE |= HALT (0x400)`, poll
   halted. (Already implemented.)
2. **Write the image** into the scratch pad through the indirect memory window:
   scratch address `0x08000000+i` maps to chip offset `0x30000+i`
   (`(addr & 0xffff) | 0x30000`), written via the `reg_base_addr`/`reg_data`
   indirect port — i.e. Apple's `memWrInd`. Word-by-word, big-endian, length
   padded to 4.
3. **Set the start PC**: `grc.fastboot_pc.addr = 0x08000000`,
   `grc.fastboot_pc.enable = 1` (ThunderGate `init` uses fastboot PC), and/or
   set the CPU PC register directly as `image_load` does.
4. **Release** the RX CPU (clear HALT), exactly as our release step does.
5. Optionally handshake: Apple follows load with `WriteFirmwareSig` +
   `WaitForFirmwareInit` (poll a signature word). We can add a tiny "I'm armed"
   signature the host polls, so the dext knows the shim ran before it trusts
   the timestamp path.

This replaces the current host-side `programAvbRxRules` register replay (which
cannot stick) with: load shim -> shim does the replay from the core.

## 6. Host dext integration

Minimal. Our RX path already reads `desc->type_flags`; the failing capture
showed `type_flags=0x0004 has_ts=0` (no PTPSTAT bit). Add:
- when `(type_flags & 0x0210)` is `PTPV1 (0x0010)` or `PTPV2 (0x0200)`, read the
  64-bit timestamp from `RX_TSTAMP_LSB 0x6b0` / `RX_TSTAMP_MSB 0x6b4` and attach
  it to the harvested frame (the field `avb-probe rx-ptp` already prints as
  `hw_ts`).
No change to ring management, classifier offsets, or the gPTP consumer.

## 7. Open hardware questions (the test plan answers these)

1. Does a **core-side** write to `0x6c8` / the `0x5400` block stick where the
   host-side write did not? (Predicted yes by the firmware-owned picture; first
   thing to confirm.)
2. Is `0x6c8` alone sufficient, or is the `0x5400` rule block also required to
   mark gPTP frames for the timestamp unit on this ASIC? (Replay both to be
   safe; then minimise.)
3. Does the armed config **persist after the core parks**, or must the shim stay
   resident?
4. Does basic host RX still work once our shim replaces the resident firmware
   (is our housekeeping subset sufficient)?

## 8. Build / load / test milestones

- M0 Toolchain: run `misc/build-toolchain.sh`, get `mips-elf-gcc -mtigon`
  working in `$KIROCREW_SCRATCH` / `.agents/tmp`. Back up the adapter's factory
  NVRAM image first (read-only dump) before any core experiments.
- M1 Hello-core: build a trivial image that writes a signature word to a known
  scratch/GENCOMM location and parks; load it via the dext; confirm from the
  host that it ran (proves halt/load/PC/release end-to-end on our hardware).
- M2 Enable shim: add the housekeeping subset + the classifier/`0x6c8` replay;
  load it; read back `0x6c8` and snapshot to confirm the writes now stick.
- M3 Timestamp: `avb-probe --nic bcm datapath up` then `rx-ptp`; confirm
  `type_flags` shows a PTPSTAT bit and `hw_ts` is non-zero and advances with the
  EAV clock.
- M4 Quality: compare BCM `hw_ts` jitter against the I210 baseline; decide
  whether it is Milan-grade.

## 9. Licensing / distribution

- ThunderGate is GPLv3 and Linux `tg3` is GPL. Both are references for the
  hardware programming model only; the shim is our own source. The replayed
  register values are observed hardware facts, already in-repo.
- RAM-load only: no NVRAM flash, no redistribution of any Broadcom/Apple blob.
- For a shipped product the I210 remains the timestamping NIC. This shim is a
  personal-machine capability for this owner's adapter; if ever distributed, the
  on-core firmware would ship as its own source-available component, kept
  separate from the proprietary host stack.

## 10. Risks

- Replacing the resident firmware could disturb functions it performs
  (management/housekeeping). Mitigated by RAM-only load (power-cycle recovery)
  and by starting from M1 before M2.
- If the capture is gated on a rule action we have not identified, M2/M3 expose
  it; the fallback is to replay Apple's full block verbatim (already captured).
- Core-side register access assumptions (the `0x30000` window, fastboot PC) are
  cross-checked against two independent sources (ThunderGate and Apple's
  `loadFirmware`) but still need the M1 hardware confirmation.

## 11. Execution status (2026-11-04)

**M0 toolchain — blocked on a manual step.** Homebrew refuses to run inside the
build sandbox (`sandbox-exec: sandbox_apply: Operation not permitted`), so the
five missing deps (`mpfr libmpc isl texinfo gnu-sed`; `gmp` is already present)
cannot be installed from here, and GCC's configure needs them. ThunderGate is
cloned at `.agents/tmp/thundergate`. To build the toolchain, run once in a real
Terminal:

    brew install mpfr libmpc isl texinfo gnu-sed
    TG_TOOLCHAIN_PREFIX=/Users/dexter/AVB-Audio/.agents/tmp/toolchain \
    TG_TOOLCHAIN_BUILD=/Users/dexter/AVB-Audio/.agents/tmp/.toolchain-build \
    bash /Users/dexter/AVB-Audio/.agents/tmp/thundergate/misc/build-toolchain.sh

The toolchain is only needed for the C-based real shim (M2). It is NOT needed
for M1.

**M0 NVRAM backup — deferred, and lower-risk than first stated.** The whole
approach loads into the RX-CPU scratch pad (volatile RAM), never NVRAM, so a
power cycle fully restores the factory state; an NVRAM backup is insurance, not
a prerequisite. A read-only NVRAM dump command (dext + probe) is a small
follow-up to add before M2 if wanted.

**M1 hello-core — implemented, gate-green, ready for a hardware run.** Rather
than wait on the toolchain, the hello-core is hand-assembled (10 big-endian
MIPS words, no mul/div/unaligned) and carried by `avb-probe core-selftest`. The
dext gained two BCM-only selectors (`LoadCoreImage` 16, `ReadCoreMem` 17,
outside the shared `kAvbSelCount` space so the I210 is untouched):
`loadCoreImage` halts the RX CPU, writes the image to the scratch pad through
the memory window (virt `0x08000000` -> window `0x30000`), sets the PC, and
releases; `readCoreMem` reads scratch back by virtual address. Commit `2fdad8c`.

Hardware run (after activating the rebuilt dext and replugging):

    avb-probe --nic bcm core-selftest

Expected on success: `signature 0x7a6b4331 MATCH` and a `heartbeat` that
advances between the two samples (`core RUNNING`) — which proves
halt/load/set-PC/release end to end. A byte-swapped signature means the
scratch-window word order needs flipping in `loadCoreImage`; a correct
signature but a frozen heartbeat means the write landed but the core did not
resume at the PC (revisit the release/fastboot-PC step). Either outcome is a
precise, self-diagnosing result, which is the point of M1.

### Toolchain build outcome (2026-11-04) — M0 DONE

The toolchain is built and verified at `.agents/tmp/toolchain/bin`
(`mips-elf-gcc 16.1.0`, `-mtigon` accepted, emits soft-float MIPS32 with no
mul/div). Two fixes to ThunderGate's `misc/build-toolchain.sh` were needed on
this machine; the scratch clone is ephemeral, so they are recorded here:

1. **`--without-zstd` on both binutils and GCC configure.** binutils 2.46 links
   `libzstd` for compressed debug sections; macOS's linker default-searches
   `/usr/local/lib`, where a stale **x86_64** Homebrew `libzstd` lives, so
   linking `as-new` failed with an arch mismatch. A cross-assembler needs no
   zstd.
2. **Build the toolchain as x86_64** (`--build/--host=x86_64-apple-darwin`,
   `CC="clang -arch x86_64"`, `CXX="clang++ -arch x86_64"`). This machine's only
   Homebrew is the **Intel** one at `/usr/local` (`brew --prefix` → `/usr/local`;
   there is no `/opt/homebrew`), so gmp/mpfr/mpc/isl are x86_64 and cannot link
   into a native arm64 GCC. A cross-compiler's host arch is irrelevant to the
   MIPS code it emits; `mips-elf-gcc` simply runs under Rosetta.

If an arm64 Homebrew is ever installed (`/opt/homebrew` with the five deps), the
x86_64 override can be dropped for a native build. The two fixes are applied in
the scratch clone's script; fold them upstream-style into any committed copy of
the build recipe.

### M1 PASSED on hardware (2026-11-04)

`avb-probe --nic bcm core-selftest` on the live adapter:

    image readback @0x08000000:  all 10 words ok
    signature 0x7a6b4331 (expected 0x7a6b4331): MATCH
    heartbeat 26321 -> 717043: core RUNNING
    core self-test PASSED: halt / load / set-PC / release works on hardware

So the RX-CPU bring-up path is proven end to end: halt the core, write a
clean-room image into the scratch pad, set the PC, release, and the core
fetches and executes it (it computed the signature and incremented a counter
from ~0). Two findings fixed along the way, both now locked in:

1. **CPU scratch is reached through the register-indirect window** (config
   `0x78` base / `0x80` data via `ConfigurationRead32`/`Write32`), NOT the
   packet-SRAM memory window (`0x7c`/`0x84`). The memory window read pure noise.
2. **No byte swap on the register-window data port.** A big-endian-assembled
   word written there reads back identical and the core fetches it correctly;
   ThunderGate's `htonl` was compensating for its own Python layer, not the
   hardware.

RX-CPU control (MODE `0x5000` / STATE `0x5004` / PC `0x501c`) stays on BAR0
MMIO, which already worked. The load primitive (`loadCoreImage` selector 16 +
`readCoreMem` 17) is the foundation M2 builds on: replace the hand-assembled
hello-core with the C enable shim, keep the same load path.

**M2 is unblocked.** Next: scaffold `driver/bcm-fw/` (our own clean-room source
tree), build it with the `.agents/tmp/toolchain` mips-elf `-mtigon` compiler to
a flat image, and have `loadCoreImage` carry that image instead of the 10-word
test program. The shim's job (from §2): housekeeping subset + replay the
classifier/`0x6c8` config from the core, then park.

### M3 first hardware run (2026-11-05)

`datapath up` -> `core-load-shim` -> `rx-ptp`. The shim infrastructure works;
the timestamp capture did not arm.

What worked (a real milestone):
- The shim executed: `SHM1` and `DONE` signatures both present, heartbeat
  climbing (`21232 -> 345763`), so the core applied its whole write list and
  parked without faulting.
- **Core-side register writes land and are host-visible.** The live snapshot
  shows `0x0560 = 0x0000000e` (Apple's capture had 0) — exactly the shim's final
  classifier write. Our clean-room firmware genuinely configures the chip from
  the RX CPU.

What did not:
- `RX_PTP_CTL` (0x6c8) reads back `0` from the host after the shim wrote
  `0x0480000f`.
- `rx-ptp` still shows `type_flags=0x0004 has_ts=0` on every Pdelay_Req — no
  PTPSTAT bit, no latched timestamp.

Key finding from re-reading Apple's `startAVB` disassembly: Apple issues its
0x5400-block writes through one register-write helper (~18 calls), then
`setPerfectMatch` x5 (unicast MAC match -- intentionally skipped for multicast
gPTP), then `updateAVBStreamFilters` (which reads the EAV clock at 0x6900).
**Apple never writes 0x6c8.** Our `RX_PTP_CTL = 0x0480000f` is a tg3-derived
Phase-0 guess, not Apple's actual mechanism, which is why it reads back 0 and
changes nothing: the arming on this chip is the classifier block itself, not
`RX_PTP_CTL`.

So the gap is no longer feasibility or the load path -- both are proven. It is
that our replayed classifier is incomplete or missing an activation step.
Next, trace the `startAVB` / `updateAVBStreamFilters` register-write helper
(`0xffffff8000aa0a30`) argument by argument to recover the COMPLETE ordered
(offset, value) sequence -- including any receive-list-processor (RLP, 0x2000)
enable, per-rule enable bits, and EAV/0x6900 setup -- then make the shim replay
that exact sequence instead of just the 0x5400-0x561c window and the bogus
0x6c8 write. Drop the 0x6c8 write.
