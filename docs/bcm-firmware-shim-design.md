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

### Apple's full classifier sequence traced; shim write list rebuilt (2026-11-05)

Traced `startAVB` (0xffffff800272c11a), `updateAVBStreamFilters` (0x...be58)
and `setPerfectMatch` from the boot KC. Apple's writes are direct MMIO
(`mov dword ptr [regbase + 0xNNNN], imm`); the `call 0xaa0a30` before each is a
posted-write flush, not the write. Complete ordered sequence:

updateAVBStreamFilters (runs first):
  0x5400=0x8400e000  0x5480=<stationMAC>
  0x5404=0x8400e000  0x5484=<stationMAC>
  0x5408=0x8400000c  0x5488=<stationMAC>
  0x540c=0x8400000c  0x548c=<stationMAC>
then setPerfectMatch x5 (station MAC match; slots 1-4 zeroed, slot 5 a const),
then startAVB:
  0xd20=<r14 & 0xffff8000>   0x5c8=<(r15-r14) & 0xf8000>   (realtime-queue derived)
  0x5410=0x8400000c  0x5494=0x88f7ffff  0x5414=0x84006002  0x5494=0x013fffff
  0x5418=0x84006002  0x5498=0x0140ffff
  0x5500=5 0x5504=0xa 0x5508=0x10 0x550c=0x20 0x5510=0x40
  0x5608=0 0x560c=0xe0 0x5610=0 0x5614=0xe1 0x5618=0x25 0x561c=0xe4e3a2 0x560=0xe

Findings:
- **Apple never writes RX_PTP_CTL (0x6c8).** Our Phase-0 guess was wrong;
  dropped from the shim. Arming is the classifier itself.
- We had been **missing the 0x5480-0x548c per-stream match block** (written by
  updateAVBStreamFilters) and had the first-bank order slightly off. The shim
  now replays the exact sequence, with 0x5480-0x548c = 0 (multicast gPTP needs
  no unicast station match; matches Apple's zeroed slots).
- Two writes remain unreplayed: **0xd20 and 0x5c8**, computed from the
  realtime-AVB receive-queue buffers that `setupAVB` allocates via
  IOEthernetController (`setNumberOfRealtimeReceiveQueues` /
  `createRealtimeAVBPacketPool`, latencies 125 us / 250 us). Our dext uses its
  own standard RX ring, not that realtime queue, so these are deferred. If the
  rebuilt classifier still does not arm the capture, the realtime-queue path
  (and 0xd20/0x5c8) is the next and most likely hypothesis.

Shim rebuilt to 388 bytes; rodata table verified against the trace.

### Rebuilt-classifier hardware run (2026-11-05): still no timestamp

Loaded the rebuilt shim (388 bytes, Apple's exact classifier sequence). Shim ran
(`SHM1`/`DONE`, heartbeat climbing), but `rx-ptp` still shows
`type_flags=0x0004 has_ts=0` on every Pdelay_Req. So an exact classifier replay,
from the core, is **not sufficient** to arm the capture. This narrows it hard:

- The documented tg3 path our harvest reads (RX_PTP_CTL 0x6c8 + descriptor
  PTPSTAT 0x0210 + RX_TSTAMP 0x6b0/0x6b4) does not engage on the 57766: 0x6c8 is
  inert (reads 0 from host and after a core write), and no PTPSTAT bit ever
  appears. This is consistent with Linux tg3 never marking the 57766
  `PTP_CAPABLE` -- the register-driven RX-PTP mechanism is simply not wired on
  this variant.
- Apple's working path does NOT use 0x6c8. It uses the classifier PLUS the
  realtime-AVB receive queue that `setupAVB` builds through IOEthernetController
  (`setNumberOfRealtimeReceiveQueues` / `createRealtimeAVBPacketPool`, 125/250 us
  latencies) and the `0xd20`/`0x5c8` DMA writes computed from that queue's
  buffers. The timestamp is almost certainly delivered through that realtime
  queue's own descriptors, not the standard RX BD our harvest reads.

So the wall is specific and now well-understood: on the 57766 the RX timestamp
rides Apple's realtime-AVB-queue machinery (a kernel IOEthernetController
subsystem), not the public tg3 register path. Replicating it clean-room means
reverse-engineering that queue's descriptor format and the superclass framework
writes -- a large effort against undocumented Apple AVB internals, well beyond
the classifier.

Cheap next probe before any of that: poll RX_TSTAMP 0x6b0/0x6b4 directly during
gPTP traffic. If it advances, the capture hardware IS latching and only the
delivery/flagging differs (recoverable); if it stays put, the capture never
arms without the realtime queue. That one read decides whether to go deeper or
stop with the I210 as the shipping timestamping NIC.

### Direct 0x6b0 poll + realtime-queue dig (2026-11-05): the real mechanism

`rx-tstamp-poll` after loading the rebuilt shim: RX_TSTAMP 0x6b0/0x6b4 stayed
`0` across 1481 samples over 10 s of live gPTP traffic, while EAV_REF_CLK
0x6900 advanced the whole time. So the register-pair capture is simply not
engaged on the 57766 -- not a delivery/flagging bug, the latch never fires.

Disassembling setupAVB's helpers (0x26e5b12 / 0x26e5ae4 / 0x26e5c8a) shows they
are not register programming: they are accessors on an AVB queue-management
object at `[this+0x110]` -- array lookups (`[obj+0x20][i]`, `[obj+0x18][i]`) and
a setter that stores the 125 us / 250 us latencies into one of those arrays.
The pointers they return, masked (`& 0xffff8000` = 32 KB-aligned base,
`(r15-r14) & 0xf8000` = size), are what startAVB writes to the MAC at `0xd20`
and `0x5c8`.

Conclusion: on the 57766 the RX timestamp is delivered through a **realtime-AVB
receive DMA ring** whose buffer base/size are programmed into 0xd20/0x5c8 from
buffers the realtime-queue object allocates. The timestamp sits in that ring's
descriptors -- not the tg3 RX_TSTAMP register pair, and not the standard RX ring
we harvest. The classifier routes matched frames into that ring; without the
ring, nothing latches. That is why an exact classifier replay changes nothing.

### Where this leaves the Broadcom effort

Everything upstream is proven and working: feasibility (the hardware latch
exists), the full clean-room toolchain, the M1 load/run path, and a shim that
configures the chip from the core. The remaining gap is a bounded but real
sub-project: reverse-engineer the realtime-AVB RX ring's complete register set
(beyond 0xd20/0x5c8 -- producer/consumer mailboxes, enable, descriptor-ring
control) and its descriptor format, allocate an equivalent ring from our dext
(we already have DMA-ring infrastructure), point the MAC at it, and harvest the
timestamp from its descriptors. Multiple dext iterations and hardware runs, with
real uncertainty in the undocumented descriptor layout.

Decision point: pursue the realtime-ring reimplementation, or stop here with the
I210 as the shipping timestamping NIC and this document as the complete record.
The I210 path is fully working today; the Broadcom adapter would be a
personal-machine capability at best (RAM-only shim, no shipping).

### Realtime-AVB RX ring: register set reverse-engineered (2026-11-05)

Confirmed first: with the classifier loaded, `bd.reserved` (BD offset 0x18) is
still 0 on the standard ring, so the hardware only stamps frames routed into the
realtime queue. The timestamp READ is now correct in harvestRx (BD.reserved +
EAV clock, Apple's algorithm); the missing half is the queue itself.

Complete register map recovered from startAVB / DoAVBGated / receivePackets:

- **Ring base**  `0xd20` = `ring_buffer_phys & 0xffff8000` (32 KB-aligned base).
- **Ring size**  `0x5c8` = `(end_phys - base_phys) & 0xf8000` (size field).
  Both derived from DMA buffers the realtime-queue object allocates
  (superclass accessors at [this+0x110]); we must allocate our own and encode
  them the same way.
- **Enable/control**  `0x3c08` = ivar[0x6bc], `0x3c10` = ivar[0x6c4],
  `0x3c20` = ivar[0x6d4] (saved register values written by DoAVBGated on
  enable; the disable path writes 1). The 0x3c00 block is the realtime-queue
  control; exact values must be read from our own chip at init.
- **Classifier routing**  the 0x5608-0x561c block (queue selectors 0xe0/0xe1 at
  0x560c/0x5614, counts 0x25 at 0x5618, 0xe4e3a2 at 0x561c) routes matched
  0x88F7 frames into the realtime queue. Already replayed by the shim.
- **Per-frame timestamp**  low-32 in each realtime-ring BD's reserved field
  (0x18); full = (EAV_msb borrow-corrected << 32) | reserved.

Still unknown (needed before an implementation can work):
1. The realtime ring's RCB (receive control block) / descriptor layout and how
   RX buffers are posted to it (our standard ring uses sramSetRcb + postStdBuffer;
   the realtime ring's control block location is not yet found).
2. The producer/consumer mailbox the MAC advances for this ring (how harvest
   knows where new entries are).
3. Whether the 0xd20/0x5c8/0x3c00 writes stick from the host or must come from
   the core (shim), as the classifier did.

### Honest status at this point

Mechanism fully understood; timestamp-read path implemented and in the shipped
dext. Remaining work is a real DMA-subsystem build: allocate a realtime RX ring,
find+program its RCB, program 0xd20/0x5c8 and the 0x3c00 control, wire a harvest
off its mailbox, and confirm each write sticks (host vs core). That is several
more build+replug hardware cycles with genuine uncertainty in the undocumented
RCB/mailbox layout, for a personal-machine-only payoff (the I210 remains the
shipping timestamping NIC). This is the decision point on whether to invest it.

### Return ring 1 built; routing is the last gap (2026-11-05)

Return ring 1 (AVB realtime RX queue) is implemented and working: enabled with
its RCB at SRAM 0x210 when AVB rules are set, drained in harvestRx via a shared
drainReturnRing helper (producer at status-block index-1 0x18, consumer mailbox
0x28c), attaching hw_ts from BD.reserved + EAV clock. The shim loads cleanly
(SHM1/DONE, heartbeat climbing).

Hardware result: gPTP frames still arrive on ring 0, untimestamped
(type_flags=0x0004, has_ts=0). So the classifier rule bank (0x5400-0x561c) alone
matches the 0x88F7 ethertype but does NOT route those frames to the realtime
queue. The routing+timestamp-enable is installed separately, by
`setRealtimeReceiveQueueFilter` -> a framework vtable call (`[rax+0x1c8]` on the
object at `[this+0x398]`), which builds a 64-bit ingress-filter descriptor
(queue 0: 0xefff6002ffff22f0, queue 1: 0xefff4002ffff22f0) and programs the
hardware ingress filter through IONetworkingFamily, below the BCM driver.

So everything host-side is built -- the ring, the BD.reserved read, the EAV
reconstruction -- and the single missing link is that ingress-filter
programming, one framework layer down. Next concrete step: RE the vtable method
at `[rax+0x1c8]` of `[this+0x398]` to find the register writes that install the
realtime-queue ingress filter (what actually routes 0x88F7 to the queue and
arms per-frame BD.reserved stamping), then replicate them from the shim/dext.
This is a deeper RE layer (framework, not the BCM driver) with real uncertainty.

### Apple-state capture attempt (2026-11-05): nothing armed to capture

Set up Apple's AVB via Audio MIDI Setup: all 8 MacBook Pro Audio Input Streams
showed "Connected" against the 16R Send banks (screenshot), but "not acquired",
and Apple's stack threw an error (the broken macOS AVB we are replacing). Took
over with our dext and snapshotted before any reset:

- Realtime-queue registers all ZERO: ring base 0xd20, size 0x5c8, HOSTCC AVB
  tuning 0x3c08/0x3c10/0x3c20. Classifier block (0x5400-0x561c) also zero.
- Only unrelated registers retained values (0x3c44=0xb00 status-block coalescing,
  0x3cd4=0x0f, 0x5c0=0x408404a0).

Conclusion: Apple arms the realtime RX queue only when a stream is ACQUIRED and
media flows, not merely "connected". macOS AVB errored before acquisition, so
the receive path was never armed -- there was no live config to capture. The
capture-and-replay strategy therefore depends on Apple's AVB working well enough
to acquire a stream, which on this machine it does not. (A Thunderbolt replug
also risks power-cycling the adapter, a second obstacle to the snapshot path.)

So both non-framework paths to the realtime queue are now blocked:
- Capture-and-replay: needs Apple to acquire a stream; its AVB cannot.
- Pure register replay (classifier + ring 1): proven insufficient -- the
  routing/arming lives in IONetworkingFamily's realtime-queue logic, active only
  during acquisition.

What remains is to derive and program the realtime RX queue entirely ourselves
(allocate the ring, work out 0xd20/0x5c8/0x3c00 and the classifier->ring routing
by experiment, drive it from our own gPTP/stream), i.e. reimplement the receive
queue from first principles rather than copy Apple. That is a large, open-ended
effort. Everything up to it is built and proven: feasibility, toolchain, the
RX-CPU load path (M1), the BD.reserved timestamp read, return ring 1, and the
full classifier replay.

---

## 2026-11-04 — Own-firmware frame-stamping: hardware result (RX-CPU not in the RX datapath)

After the capture/replay paths were ruled out, we built our own clean-room
frame-stamping RX-CPU firmware (`driver/bcm-fw/`, `stamp-watch` probe command)
and tested it on hardware against the live 16R gPTP source. The firmware's job:
route gPTP (0x88F7) to the RX CPU with a basic EMAC receive rule, wake per frame
on the RDIQ event, read the EAV clock, and write a per-frame timestamp to a
scratch ring the host reads with `ReadCoreMem`.

### What is proven to WORK (hardware)
- **RX-CPU executes our code.** `started`/`config_applied` signatures (SHM1/DONE
  at scratch 0xF000/0xF004) read back every run.
- **RX-CPU reads chip registers live.** The EAV clock (`REG(0xC0000000+0x6900)`)
  returns a large, monotonically advancing counter (e.g. two reads 200k-spin
  apart differed by ~104 ticks; later 0xca77b938). So `REG(0xC0000000+off)` is
  the correct register view and EAV timestamping is available to the core.
- **Scratch-ring delivery round-trips perfectly.** A sentinel build wrote
  0x11111111/0x22222222/0x33333333/0x44444444 to the TS_* scratch offsets
  (0xBFF0..0xBFFC) and the host read them back exactly. The earlier "garbage"
  was the real loop self-corrupting on an empty/garbage FTQ, not a plumbing bug.

### What is BLOCKED (hardware)
- **`activate_rxcpu` receive rules do not divert frames to the RX CPU.** With a
  rule set (confirmed stuck: mask_value 0xFFFF88F7; control 0x00080025 for an
  offset-0 dest-MAC match), the FTQ RDIQ count (0x5CF4) and peek (0x5CFC) stay
  **0**, and gPTP **keeps reaching the host ring** (`rx-ptp` shows ~21
  Pdelay_Req/s from 00:0a:92:ff:fe:03:3b:a9 with the rule active). The frames
  are DMA'd to the host return ring by hardware and are never made visible to
  the core.

### Register-format finding (BCM57762 vs ThunderGate's 5719)
- The rule control register has **bits 27 and 28 hardwired 0** (writing
  0xFFFFFFFF reads back 0xE7FFFFFF). ThunderGate's 5719 treats b24–31 as an
  8-bit `offset` field, so `offset=12` (0x0C000000, needs bit 27) cannot be
  encoded here — it reads back as 4. Our dext already uses the Linux tg3 rule
  format (`kRcvRule0Default=0xc2000000`, `kRcvRule1Default=0x86000004`,
  `kRcvRuleDisableMask=0x7fffffff`), which differs from the ThunderGate
  bitfield. Matching the gPTP multicast dest MAC at offset 0 sidesteps the dead
  bits — but still routes nothing, because the blocker above is not the offset.

### Root cause
On the BCM57762 in our **hardware-DMA** datapath, the RX CPU is **not the
receive processor**: halting/replacing it does not stop host RX, and the RDIQ is
not populated for the core. ThunderGate's RDIQ-peek model works because its
firmware does a full **cold bring-up** where the core owns the entire receive
path (and forwards frames to the host itself via WDMA + return rings). The
`activate_rxcpu` rule bit is inert in hardware-DMA mode.

### Forward options
1. **Full receive-processor firmware** (ThunderGate model): the RX CPU owns RX —
   drains the RDIQ, gathers mbuf clusters, stamps gPTP, and re-DMAs every frame
   to the host return ring via WDMA. This is a much larger firmware (mbuf
   walk, WDMA/return-BD management, net-rx re-injection) and must not regress
   the host datapath. It is the only identified path that makes RX-CPU
   interception work on this chip.
2. **Stop pursuing BCM RX timestamps.** The I210 path already does full gPTP +
   streaming to the 16R. The BCM (Apple Thunderbolt adapter) remains usable for
   non-timestamped traffic; gPTP stays on the I210.

The stamper firmware (`driver/bcm-fw/shim.c`) is kept as the correct stamping
logic, with a header note marking it blocked pending option 1's routing work.

### 2026-11-05 — Feasibility probe: the RX-CPU is fully out of the receive path

A final firmware probe (v8) polled the receive datapath from the core for 6 s
while gPTP + audio traffic flowed (host `rx-ptp` confirms frames arriving). It
tracked the MAX depth of all four receive FTQs and counted EMAC receive events:

| signal | register | result over 4.34M polls |
|---|---|---|
| rdiq depth | 0x5CF4 | **0** (never non-empty) |
| rcv_list_plmt depth | 0x5CE4 | **0** |
| rcv_bd_comp depth | 0x5CD4 | **0** |
| rcv_data_comp depth | 0x5D04 | **0** |
| rdiq peek word | 0x5CFC | **0** |
| EMAC rxcpu-event | grc 0x6810 b6 | **0 assertions** (enable set) |
| loop iterations | — | 4,340,549 (core alive) |
| EAV clock | 0x6900 | live (0x15ee1560) |

So from the core's view the entire receive FTQ pipeline is permanently empty and
the EMAC never raises a receive event — the hardware-DMA receive path does not
involve the RX CPU at all. This is independent of the receive rule and of the
offset-field limitation; the frames are simply never presented to the core.

**Consequence for "option 1".** Putting the RX CPU into the receive path is not
a bolt-on. It requires reconfiguring the EMAC to raise rxcpu receive events and
feed the core's FTQs, then having the core own the *entire* receive engine —
mbuf cluster management, frame gather, WDMA back to host memory, and receive
return-BD ring maintenance — because enabling core receive would displace the
hardware-DMA host delivery that currently works. That is a ground-up
reimplementation of the Tigon3 receive processor (ThunderGate's full init.c +
rx.c + net stack), with a high risk of destabilising the NIC for macOS, to
obtain a capability the I210 already provides cleanly.

**Recommendation:** stop pursuing BCM RX timestamps. gPTP + AVB streaming to the
16R already work fully on the Intel I210. Keep the Broadcom adapter for
non-timestamped use; leave this firmware and its probes in the tree as the
documented record of why the on-core interception path is closed on this chip.

### 2026-11-05 — Bit-order correction: the stamper WORKS (supersedes the two sections above)

The "RX-CPU not in the receive path" and "offset bits 27/28 hardwired" results
above are **wrong**. Both came from our own encoding bug.

**Cause.** ThunderGate's register headers are C bitfields built for big-endian
MIPS. GCC lays bitfields out per the target ABI, and on this target the first
listed field is the most significant bit. We decoded them LSB-first. Checks:
- `mips-elf-gcc -mtigon` compiles `enable=1` to `0x80000000`,
  `activate_rxcpu=1` to `0x20000000` and `offset=12` to `0x0000000C`.
- Linux tg3 writes rule 0 as `0xc2000000` and disables rules with
  `RCV_RULE_DISABLE_MASK 0x7fffffff`, so enable is bit 31.
- Our readback of `0xFFFFFFFF` → `0xE7FFFFFF` clears exactly the two
  `reserved:1` fields at bits 28/27.

So our rule control `0x0C080025` had **enable clear**, and the event-enable,
RDIQ-peek valid/skip/pass and head-pointer constants were also wrong. The
empty FTQs and the silent EMAC event were exactly what a disabled rule
produces. `driver/bcm-fw/regs.h` now uses MSB-first constants (rule
`0xA400010C`, event emac `0x02000000` / rdiq `0x00000100`, peek valid b20 /
skip b19 / pass b18, head `(peek>>9)&0x1ff`).

**Hardware result with the corrected constants** (16R live, `stamp-watch`):
- The core receives every gPTP frame: ethertype `0x88f7`, PTP messageType 2
  (Pdelay_Req), consecutive sequenceIds, no gaps.
- Consecutive EAV stamps are 1,000,200,000–1,000,202,300 ns apart for the 16R's
  1 s Pdelay_Req interval, with ~2 µs spread.
- **Forwarding works with `pass`.** With RDIQ `pass` (b18) the frame continues
  to normal receive placement: the same sequenceIds the core stamped also
  arrived on the host ring. With `skip` + mbuf-cluster free (`0x5CC8`) the core
  consumes the frame and the host never sees it. The default build uses
  `pass`; `-DFWD_CONSUME` selects the other.
- No non-PTP frames reached the host during these runs, even after a
  `datapath down/up` with no firmware loaded. That is the 16R's traffic at
  the time (no audio stream), not a firmware stall.

**Still open.**
1. The stamp is the EAV clock read when the core services the RDIQ, so it
   includes queueing latency. The ~2 µs spread above is an upper bound on
   that jitter for 1 frame/s. The I210 gives ns-level MAC stamps.
2. The MAC's own latch (`RX_TSTAMP` 0x6b0/0x6b4) reads 0, and `RX_PTP_CTL`
   (0x6c8) reads back 0 even right after the core writes `0x0480000f`. Either
   the register is not writable from this context or it needs another enable.
   ThunderGate's `grc_mode` names bit `0x00040000` `eav_mode_enable` (tg3 calls
   it `HTX2B_ENABLE`); GRC_MODE is currently `0x041b0034` with that bit clear.
3. Connecting the ring to the gPTP daemon: match on sequenceId +
   messageType, and extend `eav_lo` to 64 bits from `EAV_REF_CLK_MSB`.

### 2026-11-05 — eav_mode_enable experiment wedged the EAV clock; interlock arming removed

Tried GRC_MODE bit 18 (ThunderGate `eav_mode_enable` / tg3 `HTX2B_ENABLE`,
0x00040000) plus arming `RX_PTP_CTL = V2_L2 | ALL_V2 | HWTS_INTERLOCK` from the
core, to get the MAC's own RX_TSTAMP latch (0x6b0) working. Result:

- The MAC latch still never fired: RX_TSTAMP stayed 0, PTPSTAT never set. The
  tg3-documented MAC RX-PTP path is simply not wired on this 57766-class part
  (matches Phase 0 and Linux never setting PTP_CAPABLE for it).
- **It wedged the EAV reference-clock read.** 0x6900/0x6904 now read 0 and
  won't accept writes (SetClock's stop/write/resume leaves CTL and LSB at 0),
  from BOTH the host register window AND the core's own `REG(0x6900)` (the
  stamp ring's `eav_lo` reads 0 for every frame). The underlying counter is
  still running — the TX latch (0x5c0/0x5c4) reads a live, advancing ~28902 s
  value in the same timebase — so only the 0x6900 read path is frozen.
- A full `datapath down/up` does NOT recover it, and that path does a real
  core-clock reset (GRC_MISC_CFG CORECLK_RESET + bootcode ~MAGIC handshake).
  So the time-sync block is in the always-on power domain; recovery needs a
  Thunderbolt power cycle (unplug/replug the adapter).

Cause: `HWTS_INTERLOCK` freezes the EAV clock read until a captured timestamp's
MSB is read to release it; the committed stamper armed it but read only the
LSB, so the read stayed interlocked. Combined with the eav_mode_enable write,
the block latched in a state a chip reset doesn't clear.

Fix (this commit): the stamper no longer arms RX_PTP_CTL. It writes
`RX_PTP_CTL = 0` and reads the RX_TSTAMP and EAV pairs (LSB then MSB) to flush
any pending interlock, then uses the free-running EAV clock as the stamp
source. This prevents recurrence, but does not un-wedge an already-frozen
block — that still needs a replug. The earlier working stamp results
(1 s deltas, forwarding) stand; they were taken before this experiment.

### 2026-11-05 — Stamp ring wired into the gPTP RX-timestamp path (dext-side)

The daemon's gPTP consumes `RxSlot.hw_ts_ns`, which the dext fills in
`harvestRx`. Rather than make the daemon NIC-aware, the stamp ring is wired in
at the dext so the existing, NIC-agnostic `hw_ts` plumbing carries it.

- New init flag `AVB_INIT_FLAG_RXCPU_STAMP` (1<<3). When set, the dext records
  it (`v->rxcpuStamp`) but loads nothing itself — the host loads the firmware
  via the existing `LoadCoreImage` selector after `datapath up`.
- `harvestRx` → `drainReturnRing`: for each harvested frame, once the firmware
  signature is present (`rxCpuStampReady`: scratch 0x0800F004 == 'DONE', which
  guards against matching uninitialised scratch), it parses the gPTP key
  (messageType + sequenceId) and scans the newest 16 ring entries for a match;
  on a hit it sets `e.hw_ts = reconstructRxTs(eavLo)` (64-bit, borrow-corrected
  against the EAV clock) and `e.has_ts = 1`. This overrides the dead
  bd.reserved / MAC-latch path for gPTP.
- Pure helpers in `Tg3Logic.hpp` (`decodeRxCpuStamp`, `parseGptpStampKey`,
  `matchRxCpuStamp`) with host unit tests in `tg3logic_tests.cpp`.
- `avb-probe datapath up` now, for BCM, uses `RXCPU_STAMP` (its own receive
  rule + ring 0) instead of the superseded `AVB_RX_RULES` classifier replay,
  and loads the firmware. So after the datapath is up, `rx-ptp` shows `has_ts`
  per gPTP frame and `pdelay` gets its t4 → a hardware link delay — the Phase 0
  G3 blocker, via our own firmware.

Validation is pending the Thunderbolt replug (the EAV clock is wedged from the
earlier experiment; `reconstructRxTs` needs it live). After the replug:
`avb-probe --nic bcm datapath up` then `avb-probe --nic bcm pdelay --count 5`
should yield complete exchanges with a real link delay.

Remaining for a full daemon-on-BCM gPTP run: the daemon's `bring_up` opens the
I210 only and uses `PortConfig::i210`. A BCM run needs NIC selection plus a
calibrated BCM `LatencyProfile` — and the RX stamp is taken at RDIQ service
time (not MAC ingress), so its rx-latency constant must be measured on hardware
once the clock is back. Left as a follow-up rather than shipped untuned.

### 2026-11-05 — Validated RX stamping on hardware; fixed a TX-timestamp conflict

After two replugs (to recover the wedged EAV clock and to bind the new dext),
the stamp ring + dext attach work end to end:

- `rx-ptp` shows **has_ts=1** on every gPTP frame, with `hw_ts` deltas of
  ~1,000,200,000 ns matching the 16R's 1 s Pdelay cadence. (A 2^32 ns wrap
  artifact appears on the first sample after load — the EAV LSB wraps every
  ~4.29 s; the daemon unwraps with `stats::unwrap32`, and the steady-state
  interval is correct.)

**TX-timestamp conflict found and fixed.** With the stamper active, `pdelay`
TX timestamps timed out (0/N), while they latched 3/3 with `AVB_SKIP_STAMPER`
(no firmware → no ring reads). Cause: the stamp ring is only reachable through
the **config-space register window** (0x78/0x80) — the memory window (0x7c/0x84)
reads garbage at 0x30000, so it does not alias the scratch. A config-space
indirect access landing in the ~13 us TX_TSTAMP latch window corrupts the
latch, and `rxCpuStampReady` probed the DONE signature on every 2 ms harvest,
reliably clobbering it. Fix: `drainReturnRing` now short-circuits ALL ring
reads while `v->tsPending` (a timestamped TX is outstanding), so no config-space
access occurs in the latch window. The firmware keeps stamping into the
256-entry ring, so frames are picked up on a later harvest once tsPending
clears. TX timestamps latch within ~13 us (Phase 0 G4), so RX stamping defers
only briefly.

Pending validation of the fix: dext 308.17.16 activated; needs the device to
re-enumerate (replug) to bind, then `pdelay` should show BOTH t1 (TX) and t4
(RX) → a complete exchange with a hardware link delay.

### 2026-11-05 — EAV high-word reconstruction (fixes the absolute link delay)

The validated run showed correct nrr (-1.13 ppm) but an absolute link delay of
~-51 s: `reconstructRxTs` built t4's high 32 bits from `EAV_REF_CLCK_MSB`, which
reads 0 on this part, while t1 (TX) carries the real high word (~25) from the
TX_TSTAMP latch. t4 was effectively 32-bit, t1 64-bit, so (t4 - t1) was garbage.

Fix (dext 308.18.29): track the EAV high word in software (`v->eavHi`). It is
advanced on each EAV_LSB wrap (`eavSample`, a BAR read sampled every harvest and
on each RX stamp) and RE-ALIGNED to the TX latch epoch in `checkTx` whenever a
TX timestamp latches — the TX_TSTAMP register is the only source of the true
high 32 bits here. `reconstructRxTs` then combines `eavHi` with a fresh low
sample via the pure, unit-tested `reconstructEavTs(hi, curLo, frameLsb)` (borrow
when the frame low is at/after the current low). Since gPTP sends a TX-stamped
frame at least every second and the LSB wraps every ~4.29 s, eavHi stays aligned
with the TX epoch, so t4 and t1 share one 64-bit epoch and the link delay reads
its true ~1.4 us.

Needs the device to re-enumerate (replug) to bind 308.18.29, then `pdelay`
should report a small positive link delay.

### 2026-11-05 — VALIDATED END TO END (dext 308.18.29)

`avb-probe --nic bcm pdelay --count 8` against the live 16R, full Pdelay
exchanges with our own RX-CPU firmware supplying t4:

- TX timestamps 8/8; complete exchanges 8/8 (t1, t2, t3, t4 all present).
- Link delay mean **1372 ns, sd 49 ns** (1275-1415) — small, positive, stable.
- neighborRateRatio **-1.86 ppm, sd 0.12 ppm**.
- t4 now 64-bit and epoch-aligned with t1 (e.g. t1=144988417216,
  t4=144988444704, t4-t1=27488 ns), confirming the EAV high-word fix.

This closes the Phase 0 G3 blocker ("BCM RX packet timestamps don't work"):
the BCM57762 produces gPTP RX timestamps via our clean-room RX-CPU firmware +
the dext harvest attach, with precision suitable for gPTP asCapable.

Known operational quirk: immediately after a Thunderbolt replug, TX timestamps
time out for the first few `datapath up` cycles (link/time-sync settling, seen
on every dext version); a `datapath down/up` or two clears it, after which TX
latches 8/8. Not specific to the stamping firmware (reproduced with
AVB_SKIP_STAMPER).
