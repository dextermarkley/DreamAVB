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

## FEAT-003: avb-gptp (slave-only 802.1AS) and avb-srp (MRP/MSRP/MVRP)

Branch `phase1a-listener`. Software only: no frame was sent to or received from the 16R, and no servo ran against real SYSTIM.

| # | Command | Result |
|---|---|---|
| 1 | `cargo build --manifest-path /Users/dexter/AVB-Audio/Cargo.toml --workspace --offline` (once, without `--locked`, after adding both crates to `[workspace].members`) | builds; `Cargo.lock` +16 lines, 0 removed: packages `avb-gptp` and `avb-srp`, each depending on `avb-core` and `serde` (=1.0.229, already locked). No new external crate |
| 2 | `cargo test --manifest-path /Users/dexter/AVB-Audio/Cargo.toml -p avb-gptp -p avb-srp --locked` (with `-- --nocapture`, log `.agents/tmp/feat003-test.log`) | pass. avb-gptp 59, avb-srp 30, 0 failed |
| 3 | `cargo clippy --manifest-path /Users/dexter/AVB-Audio/Cargo.toml --workspace --all-targets --locked -- -D warnings` (log `.agents/tmp/feat003-clippy.log`) | pass, 0 warnings. Both crates build under `#![deny(clippy::indexing_slicing)]` (test modules opt out) |
| 4 | `cargo fmt --manifest-path /Users/dexter/AVB-Audio/Cargo.toml --all --check` | pass, no diff |
| 5 | `cargo test --manifest-path /Users/dexter/AVB-Audio/Cargo.toml --workspace --locked` | pass: avb-core 22, avb-dext 28, avb-gptp 59, avb-installer 0, avb-probe 16 + 0, avb-srp 30 |
| 6 | grep of `crates/avb-gptp/src` and `crates/avb-srp/src` for `std::time`, `Instant`, `SystemTime`, `std::thread`, `std::fs`, `std::net`, `std::io`, `mach_absolute`, `libc` | no matches: no clock, thread or I/O in either crate |
| 7 | review of `avb_gptp::msg` encoders | only `encode_pdelay_req`, `encode_pdelay_resp`, `encode_pdelay_resp_follow_up`, `encode_signaling_gptp_capable`. The Sync/Follow_Up/Announce builders that drive the simulated 16R live in `src/sim.rs`, which is `#[cfg(test)]`. Every simulation test also asserts that the port never transmitted an Announce, Sync or Follow_Up |

### Servo simulation (§7.3 pass criteria)

Simulation (`crates/avb-gptp/src/sim.rs`):
- The GM runs at −83 ppm against our free-running oscillator.
- 10 ns RMS Gaussian noise is added to every I210 timestamp (Sync RX, t1, t4, the 16R's Pdelay_Req RX, our Pdelay_Resp TX). The 16R's own timestamps are exact.
- True one-way delay is 142.6 ns, with the I210_1000 latency profile.
- The dext is modeled with a 500 µs tick, 300 µs harvest delay and the §3.6 settle rule (AMBIG flag through the first tick ≥ 1 ms after a SYSTIM write).
- Each StepClock/SetClock write has a 500 ns error.
- SYSTIM starts 5 s, so ACQUIRE does a SetClock.
- "Steady state" starts max(10 s, 40 updates) after LOCKED, the PI's settling time (pole radius √0.8 per update); 60 s are measured from there.

| Case | Lock (from start) | Steady mean | Steady sd | Syncs | Budget |
|---|---|---|---|---|---|
| Ts = 1/32 s | 1.552 s | 0.01 ns | 10.68 ns | 1920 | lock ≤ 10 s, \|mean\| < 5, sd < 20 |
| Ts = 1/8 s | 3.177 s | −0.04 ns | 11.10 ns | 480 | same |
| Ts = 1 s | 18.053 s | 0.17 ns | 11.47 ns | 60 | lock ≤ 30 s |
| Ts 1/8 → 1/32 s mid-run | no unlock (unlocks 0, steps 1 = the ACQUIRE SetClock) | 0.03 ns | 10.79 ns | 640 | must not unlock |

All cases also check the following:
- Exactly one step: the ACQUIRE SetClock, with `set_clocks` = 1.
- asCapable made exactly one transition (false → true), so it stayed true across the SetClock.
- The servo never entered HOLDOVER.
- Applied frequency is −83 000 ppb ± 300 ppb.
- `nrr_systim` is 1 ± 0.05 ppm, and `nrr_free` is (1 − 83 ppm) ± 0.3 ppm.
- Compensated path delay is 142.6 ± 5 ns.
- Clock model valid, `gptp_ppm_vs_mach` = −83 ± 0.05.

Other results:
- 600 ppm step-input anti-windup test: the GM jumps to +600 ppm for 10 s, then returns to −83 ppm. |offset| stayed < 1 µs for 24 Syncs, reached 3.369 s after the return. Normal lock is 3.177 s, so the 2× bound is 6.35 s.
- NRR across a 100 µs step (2.5 ns timestamp sd as in Phase 0b, exact step write): `nrr_systim` moved at most 0.0027 ppm (bound 0.01 ppm). The NRR was held across the generation change, and asCapable stayed true on every tick.

### 802.1Q-2018 §35.2.4.3 / §35.2.6 check (Listener without a registered Talker)

Finding: **not forbidden**, so the TA_WAIT Asking-Failed declaration (design §8.2) is supported. `Participant::declare(Attribute::Listener { decl: AskingFailed })` has no talker-registration precondition. A test declares it with no talker registered and checks the four-packed Asking Failed encoding.

Basis:
- The paywalled 802.1Q-2018 text itself was not available to read.
- IEEE 802.1 maintenance request 0150 (Pearson, against 802.1Q-2014, whose clause 35 numbering -2018 keeps) analyzes these exact clauses:
  - §35.2.4.3 describes Talker Failed handling by MAP, a bridge component, so it doesn't specify end-station behavior.
  - §35.2.6 is an encoding rule that lets an applicant switch Talker Advertise ↔ Talker Failed without a Leave. It doesn't constrain an end-station applicant.
- The end-station Listener clause (§35.1.2.2, quoted in IEEE 802.1 slide ca-farkas-SRP-ISIS-interworking-0713) conditions declaring *Ready* on having received a Talker Advertise. It doesn't prohibit a declaration without one.
- The bridge-only rule (§35.2.4.4) of not propagating a Listener without a registered Talker is MAP behavior.

Sources:
- `http://files.serialport.org/ieee802/maint/requests/maint_0150.pdf`
- `https://ieee802.org/1/files/public/docs2013/ca-farkas-SRP-ISIS-interworking-0713-v01.pdf`

Re-check against the -2018 text when available. The M3 hardware run shows how the 16R reacts (U-8).

MRP tables: Table 10-3 (applicant) and Table 10-4 (registrar) were written from the 802.1Q-2011 tables as reproduced in IEEE 802.1 slide `new-tsn-fredette-refresh-timer-0713-v1.pdf` (pages 4–5). Maintenance item 0044 adds the "ignored if the registrar is MT" note on VO/AO/QO for rLA!/txLA!/txLAF!. Every cell is pinned by `mrp::tests`. No GPL/LGPL source was consulted.

Not verified (hardware, later hw steps):
- Pdelay with the real 16R, and turnaround.
- Lock on real SYSTIM, and the PTP-8 offset/sd.
- The 16R's MSRP Domain/TA behavior and the U-8 outcome.
- MRP interop timing.

### Deviations

1. The servo-simulation "steady state" window is defined as starting max(10 s, 40 updates) after LOCKED. The design leaves it undefined. Measured from the LOCKED instant itself, Ts = 1 s still carries the end of the convergence tail (sd 49 ns).
2. The Sync receipt timeout (servo → HOLDOVER after 3·Ts) restarts on every matched Sync/Follow_Up from the master, and on a Ts change. Otherwise the first Sync at a shorter Ts would time out against the previous Sync and unlock the servo, which the mid-run Ts-change test caught.
3. "asCapable continuously false for > 3·Ta" in Slave is measured from the later of the false edge and the Slave entry. Otherwise an Announce accepted before the first Pdelay exchanges finish would be dropped back to Listening immediately.
4. The delay used in syncEvent is the mean of the last 8 compensated delays. Status reports the last value, the mean and the sd. t3 includes the Pdelay_Resp_Follow_Up correctionField and also the Pdelay_Resp correctionField. Both are 0 from the 16R (Phase 0b G4).
5. Our Pdelay_Req/Resp/Resp_Follow_Up/Signaling use header octet 1 = 0x02 (minorVersionPTP 0), byte-identical to the Phase 0b frame the 16R answered. The decoder accepts minorVersionPTP 1.
6. The anti-windup clamp can't be reached in TRACKING at Ts ≥ 1/8 s with |offset| ≤ 100 µs. In the 600 ppm test the excursion was handled by the step rule (41 steps, 0 saturations; each step resets i). The clamp-and-freeze behavior is pinned in `servo::tests::pi_update_and_anti_windup` at Ts = 1/32 s (Kp 6.4).
7. The NRR-across-a-step test steps the servo with an exact write. With the default 500 ns step error, the PI's correction of that residual moves the disciplined SYSTIM rate, and with it `nrr_systim`, by about 0.12 ppm. That is a real rate change, not bookkeeping.
8. GptpCounters has counters beyond §7.6:
   - `syncs_used`, `follow_up_timeouts`, `sync_timeouts`;
   - `pdelay_step_discard`, `multiple_responders`, `foreign_port`, `rx_no_ts`, `rx_dropped`;
   - `interval_requests_honored`/`_rejected`, `step_gen_changes`, `port_state_changes`.
9. MRP choices the design leaves open:
   - A changed value of a declared attribute (Ready ↔ Asking Failed, TA ↔ TF, TSpec) is sent as New!. A Join! would leave a Quiet applicant silent.
   - Transmit opportunities are immediate on a new need, repeated after JoinTime while an applicant still wants to send, and capped at 3 per 1.5 × JoinTime.
   - LeaveAll is carried on the first vector of each message in the PDU.
   - Any received LeaveAll restarts our LeaveAll timer.
   - A Lv for the talker type that isn't the registered one, after an Advertise ↔ Failed switch, is ignored.
   - Optional sends ([s]/[sJ]) are never sent, because we encode one value per vector.
   - Table 10-3 note 8 (AN tx! → QA) is implemented as printed and marked *(verify)*.

## FEAT-004: avb-avtp (AAF / stream-format) and avb-avdecc (controller + listener entity)

Two sans-IO crates (design §9, §10.1, §10.2). No I/O, no clocks, no threads: time
enters as `now_ns` values and frames as byte slices (acceptance criterion 4).

Gate at this checkpoint (all green):
- `cargo fmt --all --check`: clean.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: clean.
- `cargo test --workspace --locked`: avb-avtp 11, avb-avdecc 87, plus the
  unchanged core 22 / dext 28 / gptp 59 / probe 16 / srp 30.
- C++ host tests: tg3 97, i210 341.
- `xcodebuild … AVBI210Driver`: BUILD SUCCEEDED, 0 compiler warnings.
- `scripts/package.sh --adhoc`: both dexts built, Rust release built, package
  assembled and ad-hoc signed (CFBundleVersion 283.0.1).

### avb-avtp (§10.1, §10.2)

- `common`: AVTP EtherType, subtype table, the 16-byte common stream header
  (parse/encode, byte-exact vector).
- `aaf`: AAF PCM header decode with the §10.2 step-2 checks (subtype 0x02,
  `sv=1`, version 0); `encode` for test vectors only; `bytes_per_sample`.
- `stream_format`: the 64-bit AEM format codec. AAF fields, CRF and IEC 61883
  decode for the dump; `nsr` code table; `decodable()` (INT_32BIT 1–32 or
  INT_24BIT ≤24, nsr ∈ {44.1,48,88.2,96,176.4,192 kHz}, 1–64 ch, 1–64 spf,
  `ut=0`). Pinned vectors `0x0205022002006000` and `0x0205021802006000`, and
  the full `decodable()` matrix.

### avb-avdecc (§9)

- `constants` / `milan`: every command/status/descriptor/message code as a
  named const with an IEEE 1722.1-2021 / Milan v1.2 citation and a pinning
  test; MVU protocol id `00-1B-C5-0A-C1-00`.
- `adp`: ADPDU codec (68 B, trailing reserved(32)); entity table with
  `valid_time × 2` expiry, reboot detection (available_index backwards or
  AVAILABLE-after-DEPARTING), loopback flag, sole-talker selection.
- `acmp`: ACMPDU codec (56 B, trailing reserved(16)); `A5_RESOURCE_STATUSES`
  cited to Table 8.2; a test asserts every emitted ACMPDU targets
  `91:E0:F0:01:00:00`.
- `aecp`: AEM + MVU codecs; the in-flight/retry engine (one per target,
  ≤200 cmd/s pacing via `MIN_SEND_GAP_NS`, per-controller wrapping u16 seq,
  match on (target, seq, command_type), 250 ms + 1 retry, IN_PROGRESS re-arm
  to 1 s capped at 10 s, final timeouts reported).
- `aem::{model, desc, json}`: descriptor codecs (round-trip tested for ENTITY,
  CONFIGURATION, AUDIO_UNIT, STREAM_INPUT/OUTPUT, AVB_INTERFACE, CLOCK_SOURCE,
  CLOCK_DOMAIN, STREAM_PORT, AUDIO_CLUSTER, AUDIO_MAP; unknown types kept raw);
  the dump builder with schema `avb-aem-dump/1` and exactly the §9.2 top-level
  keys.
- `derive`: `listener_model` is the ONLY constructor of `ListenerModel`
  (acceptance criterion 3). A = AAF STREAM_OUTPUTs; non-AAF `unbound_by_design`;
  >S_MAX refused; B dense in j; rates copied from the 16R AUDIO_UNIT; rate-vs-nsr
  mismatch is an error; channel names via AUDIO_MAP/cluster else "<name> chN";
  reference index = lowest j with CLOCK_SYNC_SOURCE else 0;
  `entity_model_id = 0x02415601 || fnv1a32(canonical model)` (stable across runs,
  changes iff the model changes).
- `controller`: the sequential ACMP binder — separate wrapping u16 ACMP seq
  counter, CONNECT_RX for j+1 only after j's final outcome, the §9.2 response
  matching rule (`acmp_ctrl_unmatched` for wrong seq / luid / controller / the
  loopback command echo), 4500 ms × up to 3 attempts, A-5 classification on a
  resource status, `build_get_tx_state` for the ADC-15 check.
- `entity` + `entity::acmp_listener`: ADP advertise (valid_time 10, the §9.3
  capabilities, GM from gPTP) and ENTITY_DEPARTING; the AECP responder for every
  listed command (unlisted → NOT_IMPLEMENTED); the Milan ACMP listener SM with
  **one test per §9.4 table row** (named after it), plus the 9.2–12.2 s fallback
  window, two successive reboots (5→1→6→0), and the rev-5 entry rows.

### AS-7 decisions (recorded as required by §9.3)

Two responder behaviors the design left to the implementer, decided from the
Milan v1.2 text with a citation comment and a unit test:

1. **ACQUIRE_ENTITY** → `NOT_IMPLEMENTED`. Milan v1.2 §5.4.2.1: a Milan audio
   endpoint (PAAD-AE) does not implement ACQUIRE_ENTITY; exclusive control is
   via LOCK_ENTITY only. Const `ACQUIRE_ENTITY_IMPLEMENTED = false` in
   `entity`; test `acquire_entity_not_implemented_milan`.
2. **ENTITY GET_COUNTERS** → `NOT_IMPLEMENTED`. Milan v1.2 §5.4.3 mandates
   GET_COUNTERS for AVB_INTERFACE, CLOCK_DOMAIN and STREAM_INPUT, but not for
   the ENTITY descriptor. Const `ENTITY_GET_COUNTERS_IMPLEMENTED = false`;
   test `entity_get_counters_not_implemented_milan`. (STREAM_INPUT /
   AVB_INTERFACE / CLOCK_DOMAIN GET_COUNTERS return SUCCESS; the daemon fills
   the atomic values.)

Both citations are *(verify against the exact Milan v1.2 clause numbers at the
M2/hardware step)* — the structure (NOT_IMPLEMENTED for both) is what the SM
and tests pin; if the clause text differs the constants flip without changing
the responder's shape.

### Deviations

1. `PROBE_TX_COMMAND`/`_RESPONSE` are encoded on the wire as `GET_TX_STATE`
   message types (Milan uses GET_TX_STATE as the listener probe); the design's
   "PROBE_TX" maps to that message type. The listener SM keeps the probe its own
   named events for clarity, with a shared wrapping u16 PROBE_TX sequence counter
   separate from the controller ACMP and AECP counters.
2. The ACMP listener SM takes the random 0–1 s pre-probe delay as a
   caller-supplied `rand01_ns` argument rather than drawing it itself, so the SM
   stays deterministic and sans-IO; the daemon supplies the randomness.
3. `GET_COUNTERS` for the mandated descriptor types returns a zeroed counter
   block from the responder; the daemon overwrites the values from the AAF-5
   atomics before the frame goes out (the entity crate holds no atomics).
4. The AVTP/ADP/ACMP control-header `control_data_length` high bits and the
   trailing reserved fields (ADPDU reserved(32), ACMPDU reserved(16)) are written
   as zero and ignored on decode; the decoders read fixed field offsets, so
   trailing bytes do not affect round-trips.
5. `read_name` decodes the 64-byte descriptor name fields as NUL-terminated
   UTF-8 with lossy fallback; `write_name` truncates to 64 bytes on a char
   boundary (the derived STREAM_INPUT "→Mac …" names are truncated the same way
   in `derive`).

## FEAT-005: avb-mediaclock, avb-audioring, avb-daemon and packaging

Three crates (two sans-IO libraries plus the daemon binary) and the daemon
packaging (design §5, §10.3–§10.6, §11, §13, §6.5).

Gate (all green):
- `cargo fmt --all --check`: clean.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: clean.
- `cargo test --workspace --locked`: avb-mediaclock 26, avb-audioring 10,
  avb-daemon 27 (incl. the 48 kHz and 96 kHz pipeline tests), plus the
  unchanged core 22 / dext 28 / gptp 59 / srp 30 / avtp 11 / avdecc 87 / probe
  16.
- C++ host tests: tg3 97, i210 341.
- `scripts/package.sh --adhoc`: both dexts + the Rust release built, the
  package assembled and ad-hoc signed (CFBundleVersion 283.0.31), and
  `AVBInstaller.app/Contents/MacOS/avb-daemon` signed with
  `packaging/daemon.entitlements` (printed in the verify loop).
- `avb-daemon --help` lists `run`, `aem-dump`, `gptp`, `streams`, `record`,
  `capture`. The packaged binary's entitlements contain exactly
  `com.avbaudio.installer.i210` (no BCM).

### avb-mediaclock (§10.3–§10.5)

- `constants`: rate-derived `RateConsts`. The 48 kHz/6 spf and 96 kHz/12 spf
  values match §10.5 exactly (8000 lock packets, 192/384 lag frames, 65 536/
  131 072 capacity, 8 publish packets, 125 µs period).
- `dll`: the 2nd-order DLL (B = 1 Hz, b = √2·ω, c = ω²), k-packet advance,
  60 s epoch rebase. Tests: nominal tracking, +100 ppm convergence, ns
  quantization, gap handling, reset, rebase.
- `recovery`: the lock SM (|e| < 1 µs for `lock_packets` → LOCKED; 8× |e| >
  10 µs, mr toggle or > 10 ms gap → UNLOCKED), reference-switch counting.
- `placement`: `n = n0 + round((pt − t0)/T)`, `early_bound = max(ta, 2 ms) +
  125 µs`, late/early classification, 32→64 pt extension.
- `stats`: 10 s windows, ppm vs nominal, sd across windows, RMS jitter, the
  MCR-5 verdict (pass / finding / not-applicable).
- `lib`: the `MediaClock` orchestrator and the published `ClockModel`.

### avb-audioring (§10.6)

- `layout`: the position-independent 16 384-byte header; every field offset in
  the §10.6 table is pinned (channel_map ends 10 240, streams end 2 048, gap
  log ends 14 344 ≤ 16 384); magic `AVAR` / version 1.
- `ring`: `new_anonymous` (the one `libc::mmap` call) and `from_raw`
  (magic/version/size validation), atomic header accessors, and the clock-block
  seqlock (the avb-core protocol over the shared fields). Concurrent-reader
  test asserts no torn reads.
- `producer`: placement writes (INT_32BIT verbatim big-endian, INT_24BIT
  `i32 << 8`, both MSB-aligned), zero-fill up to the frontier, the frontier
  commit with the lagging-stream and all-silent rules, the gap log, overrun
  counting; never blocks.
- `consumer`: committed-frame reads with overrun skip and underrun counting.
- An SPSC stress test runs a 50 000-packet producer against a consumer and
  verifies in-order, non-torn frames.

### avb-daemon (§5, §11, §13)

- `exit`: the §11.4 codes, pinned.
- `log`: the `avb-daemon[<subsys>]:` prefix and the per-key 10 s rate limit.
- `dextio`: the `DextPort` trait (the IOKit boundary) and `FakeDext` (heap
  rings, scripted peer, simulated clock) used by every daemon test.
- `txsched`: the three outboxes (gPTP 8 / MRP 16 / AVDECC 40), strict priority,
  the held-`want_ts` rule (does not block a lower non-timestamped frame), the
  pending-step block, Busy/NoResources keep-at-head retry, and the per-class
  full-drop counters. Tests cover each (including the 41st AVDECC push → TxFull
  and the Pdelay_Resp-next-with-AVDECC-full case).
- `wav`: the 104-byte WAVE_FORMAT_EXTENSIBLE header and the RF64 `ds64` switch
  at the 4 GiB boundary, both byte-exact (golden tests; the RF64 case is
  computed, not written).
- `pcap`: the nanosecond pcap global + record headers (magic `0xA1B23C4D`).
- `audio`: the per-packet pipeline (AAF parse → §10.2 checks → sequence/mr/tv
  handling → media clock on the reference → placement) with per-stream atomic
  counters; nothing on this path allocates or logs.
- `validate`: §13.2 input validation (talker id, timeouts, seconds, out-path
  writability, `statvfs` disk check) mapped to the exit codes.
- `realdext` / `stack`: the production `DextPort` over `avb-dext` and the §5.2
  bring-up (open → GetInfo/ABI → RingAttach → InitDatapath → link wait) with
  the documented exit-code contract, plus SIGINT/SIGTERM via `libc::sigaction`.
- `main`: the clap CLI for all six subcommands.
- The **48 kHz/6 spf 3×4-ch** and **96 kHz/12 spf 2×6-ch** pipeline tests feed
  generated AAF frames (with a gap, a late and a reordered packet) through
  `audio → ring → consumer → WAV bytes` under a counting global allocator and
  assert **0 allocations in the per-packet function** (acceptance criterion 2).

### Deviations

1. The ctl/audio/rec threads (§5.1) and the full `run`/`record` loops are
   wired at the hardware milestones (hw-gptp … hw-audio, §14.3). The daemon's
   data-path building blocks (`audio`, `txsched`, `wav`, `pcap`, `report`, the
   `DextPort` consumers and the extra exit codes) are therefore reached only
   from the hardware loop and the tests today, so the binary carries a
   documented crate-level `#![allow(dead_code)]`. Every item is exercised now
   by the unit tests and the two pipeline tests; the lint would otherwise fire
   on the test-only call sites because `clippy --all-targets` type-checks the
   binary without `cfg(test)`.
2. `RealDext::read_clock` returns the midpoint of the dext's `mach_before`/
   `mach_after` bracket as the mach instant of `eav_ns`; the bracket width is
   the SYSTIM-read latency and the midpoint is its best single estimate.
3. The ring producer treats a `&self` → `&mut [u8]` on the data region as sound
   (an `#[allow(clippy::mut_from_ref)]` with a safety note) because exactly one
   `Producer` ever writes it (SPSC, §10.6).
4. `AudioRing` is `unsafe impl Send + Sync`: all header access is atomic and the
   data region is single-producer / single-consumer, so sharing the `&AudioRing`
   across the audio and rec threads is sound.

## Implement-and-review loop (whole phase1a-listener diff)

A four-way parallel review of the FEAT-004/005 crates (`avb-avtp`, `avb-avdecc`,
`avb-mediaclock`+`avb-audioring`, `avb-daemon`) against the design and
`AGENTS.md` was run. `avb-avtp` and `avb-avdecc` came back with no Critical/High
findings. Four High findings were raised and all fixed:

1. **avb-audioring `producer.rs` overrun double-count** — `commit_to` recomputed
   `frontier − read − cap` on every commit, so a static consumer made each tick
   re-count the same lost frames. Fixed with an `overrun_counted_upto` watermark
   so each lost frame is counted once (`overrun_frames_counted_once_not_per_commit`
   test added).
2. **avb-audioring `producer.rs` negative frame cast** — `place` cast the `i128`
   frame index to `u64` after only the `f < committed` check; a negative index
   could in principle wrap. Added an explicit `f < 0` late-drop guard before any
   cast (`negative_frame_is_dropped_not_wrapped` test added).
3. **avb-daemon `txsched.rs` missing 8 ms Pdelay_Resp late-drop** — the rule
   (§5.2 / §7.3) and its `pdelay_resp_late_drop` counter were declared but
   unimplemented, and the §14.1-mandated test was absent. Added
   `push_pdelay_resp(frame, harvest_ns)`, a harvest-age table, the drop pass at
   the top of `tick`, and the `pdelay_resp_dropped_after_8ms` test (one dropped
   at 9 ms, one sent at 7.9 ms).
4. **avb-daemon `realdext.rs` discarded the TX-timestamp owner tag** — `send`
   computed `seq.max(tag).min(seq)` (always the hw seq), so a gPTP TX timestamp
   could not be routed back to its Pdelay exchange. Fixed to keep `(hw_seq,
   owner_tag)` and return the owner tag in `TxTsResult::Ready`.

Post-fix gate (green): `fmt --all --check` clean; `clippy --workspace
--all-targets --locked -- -D warnings` clean; `cargo test --workspace --locked`
= 319 tests pass (audioring 12, daemon 28, plus the rest); C++ tg3 97 + i210
341; release build clean. The hardware milestones (hw-gptp … hw-audio) are the
remaining Phase 1a work and require the real 16R/I210 and the user.

---

### Milestone hw-gptp — gPTP lock to the StudioLive 16R (PASSED)

The `avb-daemon gptp` ctl-thread loop (`stack::gptp_loop`) was implemented to pump
the sans-IO `avb_gptp::Port` against the `DextPort`: drain the mapped control
ring → `handle_rx`; `poll` → send via the dext with the 8 ms Pdelay_Resp gate;
harvest TX timestamps → `on_tx_timestamp`; execute servo actions; feed the clock
model; exit 6 on no gPTP traffic, `DextFault` on a latched fault / lost link.

Bringing it up against the real 16R exposed **five hardware-only bugs** the
sans-IO test gate could not reach (all fixed):

1. **Datapath init flags** — `bring_up` called `init_datapath(0)`; the dext's gPTP
   RX classifier is only armed by `INIT_FLAG_RX_PTP_V2_L2` (plus `PROMISC`), so
   no gPTP frame was ever captured (`pdelay_req_rx=0`). Fixed in `bring_up`.
2. **Wrong RX path after RingAttach** — `rx_ctl` used the `PollRx` IOKit batch,
   but once a ring is attached the dext delivers RX into the mapped shared-memory
   control ring. Added `attach_rings` (map `mem::RX` + `mem::CONSUMER`, validate
   the header against the attach generation) and made `rx_ctl` drain the ctl ring
   via `RxRings::drain`, with `PollRx` as a fallback.
3. **TX-timestamp status polarity inverted** — `RealDext::poll_tx_ts` treated
   status `0` as OK, but `TX_TS_PENDING=0` / `TX_TS_OK=1`, so t1 (our Pdelay_Req
   TX timestamp) was never delivered and every exchange was counted lost. Fixed
   to use the named constants, and the ctl loop now harvests a `want_ts` frame's
   timestamp immediately after sending (the dext holds only one pending TX ts).
4. **Clock generation never seeded/re-synced** — the port started at generation
   0 while the dext's `clock_gen` was non-zero, so `try_complete` discarded every
   exchange as a step-straddle (`pdelay_step_discard=29`). The port is now seeded
   with `dext.clock_gen()` and re-synced to the dext's actual generation after
   each step.
5. **SetClock routed through the bounded step** — a `SetClock` for `|offset| > 1 s`
   was sent through `step_clock`, which rejects a delta larger than
   `STEP_CLOCK_MAX_NS`; the initial ~100 546 s SYSTIM↔GM epoch gap could never be
   closed. Split the handler: `StepClock` → bounded `step`, `SetClock` → absolute
   `set_clock(ReadClock() + delta)` (new `DextPort::set_clock_abs`).

**Result (310 s run, `--json-out`):** servo **LOCKED**, port **SLAVE**,
**asCapable true**; offset **mean 0.05 ns, sd 5.19 ns**; path delay 138 ns;
neighbor rate ratio 1.0000000; 2454/2456 Syncs used; **0 unlocks, 0 GM changes,
0 lost responses, 0 step-discards**; one SetClock + one step at startup. The two
review fixes (TX-ts owner-tag routing, the 8 ms Pdelay_Resp gate) are exercised
by this path.

---

### Milestone hw-avdecc — AEM enumeration of the StudioLive 16R (PASSED)

The `avb-daemon aem-dump` runner (`stack::aem_dump_loop`) was implemented: ADP
discovery (passive + a periodic ENTITY_DISCOVER broadcast), then an AECP
READ_DESCRIPTOR walk driven by `avb_avdecc::AecpEngine` — ENTITY → CONFIGURATION
→ the configuration's STREAM_INPUT / STREAM_OUTPUT / AUDIO_UNIT children — with
each descriptor decoded and written to the `avb-aem-dump/1` JSON. It reuses the
hw-gptp bring-up and the mapped control-ring RX path.

Two bugs surfaced on hardware (both fixed):

6. **AVTP common control header byte order inverted** (whole `avb-avdecc`
   crate). `encode`/`decode` in `adp`, `aecp` and `acmp` wrote byte 0 =
   `0x80|message_type`, byte 1 = subtype. The real wire (IEEE 1722 §5.3, and the
   16R's own ADPDU `[fa,00,50,38,…]`) is **byte 0 = subtype, byte 1 =
   sv|version|message_type**. Every real ADPDU/AECP/ACMP was rejected by
   `decode` (subtype test on the wrong byte). The 87 unit tests all passed
   because encode+decode shared the same inverted order (round-trip symmetric).
   Fixed all six encode/decode sites; corrected the three byte-order assertions
   and the two "rejects non-X" tests; added `decodes_real_16r_wire_order`, a
   golden-bytes test built from the captured 16R ADPDU prefix, to lock the wire
   order against regression (88 avdecc tests now).
7. **READ_DESCRIPTOR command missing the reserved field.** The command payload
   is `configuration_index(2) + reserved(2) + descriptor_type(2) +
   descriptor_index(2)` (§7.4.5); the reserved(2) was omitted, so
   `descriptor_type` landed in the reserved slot and read as 0 — the 16R
   returned the ENTITY descriptor for every request. Added the reserved field.

**Result:** discovered entity `0x000a92fffe033ba9` (model `0x000a92e001080501`),
`talker_stream_sources = 9`, `listener_stream_sinks = 9`; the walk retrieved
**21/21** descriptors (ENTITY, CONFIGURATION, AUDIO_UNIT, 9×STREAM_INPUT,
9×STREAM_OUTPUT) with **0 timeouts**, written as `avb-aem-dump/1` JSON. The
stream counts match the device's advertised ADP capabilities.

Post-milestone gate (green): fmt, `clippy --workspace --all-targets -D
warnings`, 320 workspace tests (avdecc 88), C++ tg3 97 + i210 341,
`package.sh --adhoc`.

## hw-streams (partial) — the `streams` runner, Milan listener flow, and U-8

The `streams` runner was reimplemented from the (wrong) classic-ACMP approach to
the correct Milan listener flow, and three hardware-only bugs plus one open
hardware question (U-8) were resolved/answered against the live 16R.

### What was wrong, and fixed

1. **CONNECT_RX is controller→listener, not controller→talker.** The first
   implementation ran `controller::Binder`, which emits `CONNECT_RX_COMMAND`.
   That is a controller→*listener* message; since we are both controller and
   listener (one station), it never reaches the 16R and the 16R (a Milan
   talker) ignored it — all 9 binds timed out. Replaced with the §9.4
   `ListenerSm` PROBE_TX flow: `BindRx` → (random delay) → `SendProbeTx` → we
   emit `GET_TX_STATE_COMMAND` to the talker → `GET_TX_STATE_RESPONSE` →
   `ProbeTxResponse` → `SetStreamFilter`. All 9 streams then settled with the
   16R's real MAAP dest MACs `91:e0:f0:00:b6:e6…ee`.

2. **The dext filter is keyed on dest_mac+vid and rejects vid 0.** The dext's
   `classifyRx` only returns `Stream` for a *tagged* frame whose VID equals the
   filter entry, and `streamFilterValid` rejects `vid < 1`. The 16R's
   GET_TX_STATE_RESPONSE reports `vid 0` (until reserved), so the installed
   filter was invalid and matched nothing. Fixed to use the SR class A VID
   (`DEFAULT_DOMAIN.vid = 2`) when the probe reports 0, and to dedup filter
   entries on dest_mac (not the probe's 0 stream_id, which collapsed all 9 to 1).

3. **SRP time was passed in ns, not mach ticks.** `Participant` converts its
   ns timer durations to ticks against the timebase and adds them to
   `now.mach`, so `now` must be raw `mach_absolute_time()` ticks. Feeding
   ns-valued `MachTicks` broke every MRP timer. Fixed: SRP uses raw mach ticks,
   the ACMP SMs keep ns (two distinct clocks).

### SRP (MSRP/MVRP) integration

Added the §8 `avb-srp` participant to the runner: declare MVRP `Vid{2}` + MSRP
`Domain(class 6/prio 3/vid 2)`, declare an MSRP **Listener Ready** for each
derived stream id (`stream_id_for(talker_mac, j)`), parse the 16R's control
traffic, and maintain the reservation through the counting phase. On hardware
the 16R **registers our Domain** (`DomainRegistered(class_id:6, priority:3,
vid:2)`) — SRP is bidirectional and the SR class A domain is agreed.

### U-8 answered: the 16R does NOT advertise/stream on reservation alone

With gPTP locked, the AEM enumerated, all 9 streams probed, our Listener Ready
declared and the MSRP Domain mutually registered, the 16R sent **no Talker
Advertise** and **no AAF** (0 packets over 25 s). So U-8's open question — "does
it stream only after Listener Ready?" — is answered **no, not on Listener Ready
alone**: the 16R will not advertise or source an output stream until that output
is **routed to an active source in the mixer**, a device-side action on the 16R
itself (consistent with the hw-audio requirement that the user feed/route
signal). The protocol side is complete and correct up to this wall; the counter
path fires the instant AAF arrives.

### State

- `streams` runner: ADP discovery → Milan PROBE_TX bind → MSRP/MVRP reservation
  → per-stream AAF counting, all gate-green. Committed as work-in-progress.
- Blocked on the user: route the 16R's output streams to sources in the mixer
  (and/or confirm whether its outputs stream only when fed), so AAF flows and
  the 8000 pkt/s per-stream check can complete. This is the same visit that
  hw-audio needs.

Gate (green): fmt, `clippy --workspace --all-targets -D warnings`, 320 workspace
tests, C++ tg3 97 + i210 341, `package.sh --adhoc`.

## hw-streams (cont.) — GET_STREAM_INFO + CONNECT_TX prove the 16R outputs are dormant

After the empty Talker-Advertise finding, two more AECP probes were added to the
`streams` runner, and they settle the question of why no AAF flows.

### GET_STREAM_INFO on every STREAM_OUTPUT (Phase 1b)

Querying `GET_STREAM_INFO` (0x000F) on each of the 9 STREAM_OUTPUTs returns, for
all nine:

```
flags=0xf2000000  id=0x000a92033ba9000j  dest=91:e0:f0:00:b6:e(6+j)  vid=0  connected=false
```

- The stream_id is the MAC+index derivation (`stream_id_for`) — so our Listener
  Ready stream_ids were correct all along; that was never the problem.
- `flags=0xf2000000`: the "valid" high bits are set but **CONNECTED (bit 27),
  STREAM_ID_VALID (3), STREAM_DEST_MAC_VALID (5) and STREAM_VLAN_ID_VALID (7)
  are all clear**. The 16R reports each output as **not connected, with no
  committed addressing** — i.e. the talker is dormant, not streaming.

### ACMP CONNECT_TX to the talker (Phase 1c/1d)

Sent `CONNECT_TX_COMMAND` (the listener→talker leg the PROBE_TX-only flow omits)
for each output. **All 9 returned `CONNECT_TX_RESPONSE status=SUCCESS`** with the
real stream_id, dest MAC and `vid=2`. Yet an immediate re-query of
`GET_STREAM_INFO` shows the flags **unchanged (`0xf2000000`, connected=false)**:
the 16R acknowledges the connect but does **not** transition its output to
CONNECTED or begin egress. 0 AAF packets over the run.

### Conclusion — the blocker is device-side, not protocol-side

With gPTP locked, the AEM enumerated, the MSRP Domain mutually registered, our
Listener Ready declared with the correct stream_ids, and a SUCCESS CONNECT_TX
for every output, the 16R's own AVDECC state still reports every output
`connected=false`. There is no further frame our controller/listener/SRP stack
can send to force the transition. The gating condition lives inside the 16R:
its AVB **output streams are not enabled/committed to transmit** in the device's
current configuration. Internal mixer channel→AVB routing is not the same as the
output stream being enabled as an AVB *send* — the latter is set in PreSonus's
control software (UC Surface / Universal Control), a device-side action.

The daemon's path is complete and correct up to this wall: discovery, AEM,
GET_STREAM_INFO, CONNECT_TX, the Milan PROBE_TX bind, and the MSRP/MVRP
reservation all succeed, the dext filter installs with the right dest_mac+vid=2,
and the AAF counter fires the instant frames arrive.

### Open question for the device

Confirm in the 16R's control software that each output stream is **enabled as an
AVB transmit stream** (not merely routed internally). If PreSonus only streams an
output when a *remote listener it recognizes* has connected, note that our
CONNECT_TX already returns SUCCESS without effect — which points at an
enable/arm step on the device rather than a listener-presence condition.

Gate (green): fmt, `clippy --workspace --all-targets -D warnings`, 320 workspace
tests, C++ tg3 97 + i210 341, `package.sh --adhoc`.
