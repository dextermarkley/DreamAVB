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
