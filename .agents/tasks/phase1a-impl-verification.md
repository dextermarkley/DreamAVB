# Phase 1a implementation verification

One section per FEAT, appended by each implementation step. Every command ran with
`TMPDIR=/Users/dexter/AVB-Audio/.agents/tmp/`, absolute paths and no `cd`. Hardware ([HW])
items are not verified in software steps (VER-5).

## FEAT-001: dext ABI v3 (C++)

Branch `phase1a-listener`. Software only: the dext was built and packaged, not activated, and no
hardware milestone was run.

| # | Command | Result |
|---|---|---|
| 1 | `make -C /Users/dexter/AVB-Audio/driver/tests test` | pass. tg3: `all tests passed (97 checks)`. i210: `all i210 tests passed (341 checks)`, 22 groups including classifyRx, rxFrameBounds, rxHarvestStep, rxVlanFlags, ringFree, stepTarget, rxStepArm/rxStepAmbiguous, tokenBucketTake (1063 sends in 1 s), stats accumulation, filter validation, isAvdeccDest, producerRunning, `!isReadableRegOffsetI210(0xC030)`. ABI v3 sizes and offsets are static_asserts in both test files |
| 2 | `/usr/bin/xcodebuild -project /Users/dexter/AVB-Audio/driver/AVBI210Driver.xcodeproj -scheme AVBI210Driver -configuration Debug -sdk driverkit -derivedDataPath /Users/dexter/AVB-Audio/.agents/tmp/DerivedData SYMROOT=/Users/dexter/AVB-Audio/build/driver OBJROOT=/Users/dexter/AVB-Audio/build/driver/obj CODE_SIGNING_ALLOWED=NO build` (log `.agents/tmp/xcb-AVBI210Driver.log`) | `** BUILD SUCCEEDED **`, 0 ` warning: ` lines; both .iig files and both .cpp files recompiled |
| 3 | same for `AVBBcmDriver` (log `.agents/tmp/xcb-AVBBcmDriver.log`) | `** BUILD SUCCEEDED **`, 0 ` warning: ` lines |
| 4 | `/Users/dexter/AVB-Audio/scripts/package.sh --adhoc` (log `.agents/tmp/package-feat001.log`) | exit 0; both dexts `BUILD SUCCEEDED` through the new `-scheme`/`-derivedDataPath` invocation; `dext executable archs: arm64e`; `packaged: /Users/dexter/AVB-Audio/build/AVBInstaller.app` |
| 5 | `git -C /Users/dexter/AVB-Audio diff -U0 driver/AVBI210Driver/AVBI210Driver.cpp`, grepped for `wr(`/`wrFlush(`/`Write32`/`ConfigurationWrite` | no added or removed register or config writes. The added reads are CRCERRS, MPC, RNBC, RQDPC(0), plus STATUS for the 5 ms link poll (already read in Phase 0b). SYSTIML/SYSTIMH are written only in `setClock()` |

Review checklist items (§14.4) checked by reading the code:
- `v->stage` and `v->flags` are assigned only inside `setStage()` and `setFlags()`. `setFault()` mirrors `fault` into the header and clears `producer_running`. The Start-time `kAvbFaultIdMismatch` assignment runs before any rings can exist.
- In ring and legacy mode, `rxFrameBounds()` (TSIP-short, then oversize) runs before `classifyRx()` and before every `memcpy` in `harvestRx`.
- `ClientGone` follows the §3.3 order and `RingDetach` follows the §3.6 order. Both clear `AVB_FLAG_RINGS_ATTACHED | AVB_FLAG_OWNED` before `rxMem` is released.
- `RingAttach` checks `RingsUsed()`. `InitDatapath` and `ShutdownDatapath` never change `owner`.

Not verified (needs hardware, later hw steps):
- `CopyClientEntitlements` behavior for our signed clients.
- Ring-mode harvesting, the `RxOversize` count (expected 0), RQDPC(0) behavior on real hardware.
- Mapping both memory types from user space.

Rust: no Rust file changed. `avb-probe` still asserts ABI 2, so it rejects this dext's ABI 3 `GetInfo` until FEAT-002. FEAT-001 allows this.

### Deviations

1. `AVBShared.h`: the design's verbatim class-counter assert `kAvbCtrRxClassBase + kAvbRxGptp - 1 == 1 && …` fails under C++20 `-Werror` (`-Wdeprecated-enum-enum-conversion`, arithmetic between two enum types). It now casts the first operand: `uint32_t(kAvbCtrRxClassBase) + kAvbRxGptp - 1 == 1 && uint32_t(kAvbCtrRxClassBase) + kAvbRxStream - 1 == 6`. The assertion itself is unchanged.
2. RQDPC(0): the local 333016 rev 3.7 text (§8.10.10) lists RQDPC as **RW and wrapping at 0xFFFFFFFF**, not clear-on-read as the design assumed. The accumulator therefore adds the modulo-2^32 delta since the last read (`statsAccumulate`, baseline read at `InitDatapath`). It never writes the register, and RQDPC(0) stays on the `ReadReg32` denylist as designed. Section citations were corrected from the same text: CRCERRS §8.18.1, MPC §8.18.5, RNBC §8.18.34 (the design said §8.19), RQDPC §8.10.10 (the design said §8.11). Each keeps its "(verify)" tag until hardware confirms it. The VP bit 3 was checked in §7.1.4.2.2 Table 7-13.
3. `rxVlanFlags(vp, frame, len)` takes the frame length so it never reads past a short frame. The design wrote `rxVlanFlags(vp, frame)`.
4. AVTP control (0xFA–0xFC) and MAAP (0xFE) subtypes sent to a destination outside the AVDECC set return `DropOther`. §3.4 rule 5 doesn't say, so rule 6 applies.
5. Statistics baseline: at a successful `InitDatapath` the dext reads the four statistics registers once and discards the values, so counters start at 0 for the new datapath. These are reads only.
6. `IOBufferMemoryDescriptor::SetLength(len)` is called after `Create` for the two shared buffers. The SDK documents zero length until `SetLength`.
7. The user client's `Stop` no longer logs `closed`. The driver's `ClientGone` logs `AVBI210: uc: closed (not owner)` or `AVBI210: uc: closed (owner): datapath down, rings of generation N released`.

## FEAT-002: avb-core, avb-dext (ABI v3 mirror), avb-probe migration

Branch `phase1a-listener`. Software only: nothing was run against the dext or hardware.

| # | Command | Result |
|---|---|---|
| 1 | `cargo build --manifest-path /Users/dexter/AVB-Audio/Cargo.toml --workspace --offline` (once, without `--locked`, to record the new crates) | builds; `Cargo.lock` +15 lines, 0 removed: new packages `avb-core` (no deps) and `avb-dext` (anyhow, core-foundation-sys, io-kit-sys, mach2 0.7.0), new edge `avb-probe -> avb-dext`. No new external crate or version |
| 2 | `cargo test --manifest-path /Users/dexter/AVB-Audio/Cargo.toml -p avb-core -p avb-dext -p avb-probe --locked` (log `.agents/tmp/feat002-test.log`) | pass. avb-core 22, avb-dext 28, avb-probe 16 (lib) + 0 (bin), 0 failed. The probe's original 27 tests all still run: 16 in avb-probe, 11 moved with abi/freq/nic into avb-dext |
| 3 | `cargo clippy --manifest-path /Users/dexter/AVB-Audio/Cargo.toml --workspace --all-targets --locked -- -D warnings` (log `.agents/tmp/feat002-clippy.log`) | pass, 0 warnings. avb-core builds under `#![deny(clippy::indexing_slicing)]` (test modules opt out) |
| 4 | `cargo fmt --manifest-path /Users/dexter/AVB-Audio/Cargo.toml --all --check` | pass, no diff |
| 5 | `cargo tree --manifest-path /Users/dexter/AVB-Audio/Cargo.toml -p avb-core --offline` | `avb-core v0.1.0 (/Users/dexter/AVB-Audio/crates/avb-core)` only, no dependencies |
| 6 | `cargo test --manifest-path /Users/dexter/AVB-Audio/Cargo.toml --workspace --locked` | pass (same counts plus avb-installer 0) |
| 7 | `make -C /Users/dexter/AVB-Audio/driver/tests test` (after a comment-only edit to `driver/AVBShared.h`) | `all tests passed (97 checks)`, `all i210 tests passed (341 checks)` |
| 8 | `target/debug/avb-probe --help` | lists `info regs counters clock adjfreq datapath rx-ptp pdelay all`; no command retired |

What the tests cover:
- avb-dext `abi`: every §3.1 literal as `offset_of!`/`size_of` (including RxSlot `clock_gen` 40 and `pad1` 44, header `clock_gen` 896 and `reserved` 900, `ctl_head` 128, `audio_head` 256, `counters` 384, `RX_MEM_SIZE` 17 842 176, consumer 256/128, filter 136/8 with `vid` @6). Also selectors 10–14, `COUNT` 15, `BASE_COUNT` 10, flags 0x20/0x40, `RXF_TS_STEP_AMBIG` 8, `STEP_SETTLE_NS` 1 000 000. Compile-time `const` asserts check `class_counter_index(Gptp) == 1` and `(Stream) == 6`. The counter-name table covers 0..=33 without 7. The stream filter rules match the dext's.
- avb-dext `shm`, all over `HeapRxMem`: header accept, then reject for each of the 9 constant fields plus generation. Size and alignment checks in `from_raw`. Live fields and counters. In-order delivery with the tail published. The `max` bound. Ctl ring wrap at 512 with drop-newest. Index-mismatch desync jumping to head. Impossible head/tail distance. Slot rejects: len 1985, cls 0/7/0x80/0xFF, both VLAN flags, clock_gen ahead with wrapping comparison.
- avb-dext `iokit`: `kr::*` checked against `io_kit_sys::ret` and mach2's `MACH_SEND_INVALID_DEST`; device-removed classification; `io_return_of` through an anyhow context chain.
- avb-core: ext32 around the 2^32 boundaries, at ±2^31, and below 0 / above `u64::MAX`; EUI-64; MAC/StreamId; VLAN parse (untagged, tagged, descriptor tag, in-frame tag wins, short and double-tag rejects) and build round-trip; SeqLock with 1 writer and 2 readers × 200 000 reads, no torn or backwards values; `Err(Contended)` against a writer held at odd `seq`; raw-word protocol; `is_avdecc_dest` with the dext's boundary table (plus other-unicast and broadcast); Timebase 125/3 (24 000 ticks = 1 ms, the §5.1 policy ticks 6000/1440/4800) and 1/1; Outbox FIFO, drop-new, wrap and N = 0; counters.

Not verified (needs hardware, later hw steps): `GetCounters`/`StepClock`/`SetStreamFilter`/`RingAttach`/`RingDetach` and `IOConnectMapMemory64`/`IOConnectUnmapMemory64` against the real dext; the `counters` subcommand output (M0).

### Deviations

1. `abi::ClockSample` no longer derives `serde::Serialize`. avb-dext's dependencies are the four in §6.3, and serde isn't one of them. `avb-probe` serializes clock samples through a local `ClockSampleJson` with the same field names, so the `--json` output is unchanged.
2. Moved code fixes, both needed for the device-removed classification. `kr_check` mapped 0xe00002ed to "kIOReturnNoDevice", but that code is kIOReturnNotResponding; NoDevice is 0xe00002c0. Both are now named correctly and tested against `io_kit_sys::ret`. Errors are now a typed `IoReturnError` inside `anyhow::Error`. The message text is unchanged (`<call> failed: 0x… (<name>)`).
3. `nic::i210_readable` (the probe-side mirror of `isReadableRegOffsetI210`) gains 0xC030 (RQDPC(0)) to match the FEAT-001 dext denylist. No probe dump lists it.
4. `avb-probe/Cargo.toml` keeps its `io-kit-sys` and `core-foundation-sys` entries, now unused by the probe itself, so `Cargo.lock` changes are additions only.
5. `driver/AVBShared.h` header comment now points at `crates/avb-dext/src/abi.rs` (comment only).
