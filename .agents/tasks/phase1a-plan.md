# Phase 1a Implementation Plan

Spec: `.agents/tasks/phase1a-design.md` rev 5 (APPROVED, `.agents/tasks/phase1a-design-review.json`). Branch `phase1a-listener`, never push. Section numbers (§) refer to the design. The same work is split into FEAT files under `.agents/tasks/task-phase1a-listener/` (`context.json`, `features/FEAT-001..005.json`); plan items 1–5 below are FEAT-001..005, item 6 is the integration/review loop.

All commands run with `TMPDIR=/Users/dexter/AVB-Audio/.agents/tmp/`, no `cd`, absolute paths. Gate commands (referred to as "the gate" below):

```
cargo fmt     --manifest-path /Users/dexter/AVB-Audio/Cargo.toml --all --check
cargo clippy  --manifest-path /Users/dexter/AVB-Audio/Cargo.toml --workspace --all-targets --locked -- -D warnings
cargo test    --manifest-path /Users/dexter/AVB-Audio/Cargo.toml --workspace --locked
make -C /Users/dexter/AVB-Audio/driver/tests test
/usr/bin/xcodebuild -project /Users/dexter/AVB-Audio/driver/<P>.xcodeproj -scheme <P> -configuration Debug -sdk driverkit \
  -derivedDataPath /Users/dexter/AVB-Audio/.agents/tmp/DerivedData SYMROOT=/Users/dexter/AVB-Audio/build/driver \
  OBJROOT=/Users/dexter/AVB-Audio/build/driver/obj CODE_SIGNING_ALLOWED=NO build     # P = AVBI210Driver, AVBBcmDriver; 0 warnings
/Users/dexter/AVB-Audio/scripts/package.sh --adhoc
```

Baseline checked while planning: `cargo test --workspace --locked` passes (27 probe tests) and `make -C driver/tests test` passes (126 i210 checks).

## Planning decisions (not fixed by the design)

- **`-scheme` instead of `-target` for `-derivedDataPath`.** xcodebuild rejects `-derivedDataPath` with `-target` ("-scheme … is required"). Both projects have a scheme named after the project, and with `SYMROOT`/`OBJROOT` kept, `BUILT_PRODUCTS_DIR` stays `build/driver/Debug-driverkit` (checked with `-showBuildSettings`). So `package.sh` switches to `-scheme`, and `DEXT_BUILT` doesn't change.
- **`nic.rs` and `freq.rs` move into `avb-dext` with `abi.rs`/`iokit.rs`.** `iokit::Device::open` uses `nic::NicProfile`, and `nic` uses `freq`. Moving all four keeps one copy. `avb-probe/src/lib.rs` re-exports `pub use avb_dext::{abi, freq, iokit, nic};`, so the probe's `use avb_probe::…` paths don't change.
- **Keeping the §6.1 dependency graph.** `avb-avdecc` depends only on `avb-core` and `avb-avtp`. MSRP and gPTP inputs reach `ListenerSm`/the responder as plain data events, which the daemon translates (it computes `a5` from `avb_srp::A5_FAILURE_CODES`). `avb-mediaclock` depends only on `avb-core`. The daemon passes it the reference index from `avb_avdecc::derive::reference_index`, so derive and the media clock still use one function.
- **Feature order puts the dext first.** The Rust ABI mirror asserts the C++ literals, so FEAT-001 comes first. FEAT-003 comes before FEAT-004 because its 802.1Q §35.2.4.3/§35.2.6 check decides whether `ListenerSm`'s TA_WAIT Asking-Failed edge exists.
- **The probe gets a read-only `counters` subcommand.** It is needed for the M0/M5 evidence in §14.4 and §4 ("avb-probe info … counters").
- **Hardware milestone mapping.** The workflow's hardware steps run after the implementation loop, in this order: hw-gptp (M0 regression + M2), hw-avdecc (M1 `aem-dump`, plus our entity's acceptance from M3), hw-streams (M3), hw-audio (M4, plus the M5 robustness evidence where it fits), and results. The design ran M1 before M2–M4 were written; here the software is complete first, and an M1 contradiction (§4) becomes a fix in the hardware step. The three non-adaptable contradictions in §4 still go to the user.

## Items

- [ ] 1. **FEAT-001: Dext ABI v3 (C++)** (§3.1–§3.7)
      Change `AVBShared.h` to ABI 3 (selectors 10–14, `kAvbSelBaseCount`, memory types, flags, constants, `AvbRxSlot`/`AvbCounters`/`AvbShmHeader`/`AvbShmConsumer`/`AvbStreamFilter`, every static_assert, `<stddef.h>`). Move the BCM user client to `kAvbSelBaseCount` (3 sites).
      In the I210 dext:
      - new pure helpers in `I210Logic.hpp`: `classifyRx`, `rxFrameBounds`, `rxHarvestStep`, `rxVlanFlags`, `ringFree`, `stepTarget`, `RxStepAmbig`, `tokenBucketTake`, `producerRunning`, filter validation, `isAvdeccDest`;
      - four stats reads, and RQDPC(0) added to the denylist;
      - a 2048-descriptor ring with 16×256 KiB RX chunks, a 500 µs timer and a 20 ms TX-ts timeout;
      - `setStage`/`setFlags`/`setFault`/`setClock` as the single writers;
      - ring-mode harvest and counters;
      - StepClock, GetCounters, SetStreamFilter, RingAttach, RingDetach;
      - ownership rules, `CopyRingMemory` and `ClientGone`, the TX token bucket;
      - the user-client changes: dispatch rows, `CopyClientMemoryForType`, `Stop` → `ClientGone`, `RingsUsed`, and the entitlement check.

      Update both C++ test files to ABI 3 and add every §3.7 host test. Switch `package.sh` to `-scheme`/`-derivedDataPath`.
      Files: `driver/AVBShared.h`, `driver/AVBI210Driver/{AVBI210Driver.iig,AVBI210Driver.cpp,AVBI210UserClient.iig,AVBI210UserClient.cpp,I210Regs.h,I210Logic.hpp}`, `driver/AVBBcmDriver/AVBBcmUserClient.cpp`, `driver/tests/{i210logic_tests.cpp,tg3logic_tests.cpp}`, `scripts/package.sh`.
      Verify: `make -C driver/tests test` passes; both xcodebuild builds succeed with 0 warnings; `package.sh --adhoc` prints `packaged:`.

- [ ] 2. **FEAT-002: `avb-core`, `avb-dext`, probe migration** (§5.1, §5.2, §6.1, §3.1 Rust mirror). Depends on 1.
      - `avb-core` (std only): MAC/EUI-64/StreamId, Ethernet/VLAN, MachTicks/Timebase, ext32, atomic-word `SeqLock`, `Outbox<N>`, `is_avdecc_dest`, `Now`/`RxMeta`/`DropReason`.
      - `avb-dext`: moved `abi`/`iokit`/`nic`/`freq`; the ABI v3 mirror with offset tests; new Device methods (step, counters, filter, attach/detach, map/unmap); `shm` header validation, the slot view and the consumer with desync handling.
      - `avb-probe` re-exports these and gains `counters`.
      - Add the crates to the workspace members and update `Cargo.lock` offline once.

      Files: `crates/avb-core/**`, `crates/avb-dext/**`, `crates/avb-probe/{Cargo.toml,src/lib.rs,src/main.rs}`, `Cargo.toml`, `Cargo.lock`.
      Verify: `cargo test -p avb-core -p avb-dext -p avb-probe --locked` passes; clippy and fmt are clean; `cargo tree -p avb-core --offline` shows no third-party deps.

- [ ] 3. **FEAT-003: `avb-gptp` + `avb-srp`** (§7, §8). Depends on 2.
      - `avb-gptp`: message codecs, pdelay initiator and responder, asCapable, port SM, PI servo, step bookkeeping, the gPTP↔mach model, status. There is no Announce/Sync/Follow_Up TX path.
      - `avb-srp`: MRP applicant, registrar and LeaveAll; the vector PDU codec; MSRP TA/TF/Listener/Domain with adoption and `TalkerChanged`; MVRP; Participant with `withdraw_all`; `A5_FAILURE_CODES`.
      - Check the 802.1Q §35.2.4.3/§35.2.6 Listener-without-Talker rule. If the text forbids it, ask the user (`send_message` warning) and leave the TA_WAIT declaration out.
      - Tests: every item in the §14.1 avb-gptp and avb-srp rows, including the servo simulation criteria at Ts ∈ {1/32, 1/8, 1} s and the mid-run Ts change.

      Files: `crates/avb-gptp/**`, `crates/avb-srp/**`, `Cargo.toml`, `Cargo.lock`.
      Verify: `cargo test -p avb-gptp -p avb-srp --locked` passes; clippy and fmt are clean.

- [ ] 4. **FEAT-004: `avb-avtp` + `avb-avdecc`** (§9, §10.1, §10.2). Depends on 2; logically follows 3.
      - `avb-avtp`: AVTP header, AAF decode (encode only for tests), the stream-format codec, `decodable()`.
      - `avb-avdecc`:
        - cited constants; ADP and the entity table;
        - AECP with the in-flight/retry engine and MVU; ACMP, always to 91:E0:F0:01:00:00;
        - descriptor codecs, the model, and the AEM dump JSON (`avb-aem-dump/1`);
        - controller: discovery, enumeration, unsolicited notifications, and the sequential CONNECT_RX Binder with A-5 classification and S_max refusal;
        - `derive` as the only `ListenerModel` constructor, plus `reference_index`;
        - the entity: ADP advertise/depart, the full AECP responder (ACQUIRE_ENTITY and ENTITY GET_COUNTERS decided from Milan v1.2 with a citation, recorded in the verification doc), and `ListenerSm` per every §9.4 row.
      - Tests: the §14.1 avb-avtp and avb-avdecc rows.

      Files: `crates/avb-avtp/**`, `crates/avb-avdecc/**`, `Cargo.toml`, `Cargo.lock`.
      Verify: `cargo test -p avb-avtp -p avb-avdecc --locked` passes; clippy and fmt are clean.

- [ ] 5. **FEAT-005: `avb-mediaclock`, `avb-audioring`, `avb-daemon`, packaging** (§5, §10.3–§10.6, §11, §13, §14, §6.5). Depends on 1–4.
      - `avb-mediaclock`: rate-derived constants, DLL, reference/lock SM, placement, late/early, stats, MCR-5.
      - `avb-audioring`: the 16 KiB v1 header, `new_anonymous`/`from_raw`, Producer (zero-fill, frontier, gaps, overrun), Consumer, the clock seqlock.
      - `avb-daemon`:
        - CLI with every subcommand and flag, exit codes, logging;
        - `DextPort` with `RealDext` (§5.2 start, §5.3 shutdown, health checks) and `FakeDext`;
        - `txsched`, software loopback, the ctl/audio/rec threads (time-constraint audio thread), signal flag;
        - WAV/RF64, channels/summary JSON, pcap capture, reports.
      - Packaging: `packaging/daemon.entitlements`; `package.sh` embeds, signs and prints `avb-daemon`.
      - Tests: the §14.1 daemon row, including the counting-allocator pipeline tests at 48 kHz/6 spf and 96 kHz/12 spf and the FakeDext `gptp` exit-6 / Pdelay-only cases.

      Files: `crates/avb-mediaclock/**`, `crates/avb-audioring/**`, `crates/avb-daemon/**`, `packaging/daemon.entitlements`, `scripts/package.sh`, `Cargo.toml`, `Cargo.lock`.
      Verify: the full gate passes; `build/AVBInstaller.app/Contents/MacOS/avb-daemon --help` lists run/aem-dump/gptp/streams/record/capture; the verify section of `package.sh` prints the daemon entitlements.

- [ ] 6. **Integration and review loop.** Iteration 1 runs the full gate across all FEATs and fixes the seams. It also runs the §14.4 checklist greps: no rate/channel literals, every SYSTIM write goes through `setClock`, `setStage`/`setFlags` are the only writers, there is no `/tmp`/`cd`/`sudo`/`nvram`, and every pin is `=`. Results are recorded in `.agents/tasks/phase1a-impl-verification.md`. Later iterations fix every finding in `.agents/tasks/phase1a-impl-review.md`. The loop stops when `.agents/tasks/phase1a-impl-review.json` has `"verdict": "APPROVED"`.
      Verify: the full gate passes, and the reviewer's verdict is APPROVED.

Hardware items ([HW]: AS-3, AS-4, AS-5, AS-6, U-1…U-13, PTP-7/8, MCR-3/5, NFR-1/2, DP-11) are not claimed by items 1–6 (VER-5). They are proven only by the hardware steps that follow.
