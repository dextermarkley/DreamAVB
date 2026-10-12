# AVB-Audio: context handoff (2026-10-10)

Native macOS AVB audio bridge. Our own NIC driver plus a Rust stack exposes a PreSonus StudioLive 16R (Milan firmware 3.3.0.109659) to DAWs (Pro Tools, Reaper) as a Core Audio device. Apple's AVB stack is bypassed entirely.

Read this file first, then `AGENTS.md` (rules) and the docs it points to.

## Where things stand

- Phase 0 is done: the Broadcom BCM57762 in Apple's Thunderbolt Gigabit adapter is rejected. RX PTP timestamps never latch. See `docs/phase0-results.md`.
- Phase 0b is done: the Intel I210 passes every hardware gate against the 16R. Link delay is about 143 ns ± 2.5 ns and NRR is stable. See `docs/phase0b-i210-results.md`.
- Phase 1a (16R → Mac recording to WAV, listener only) is in progress on branch `phase1a-listener`.
  - The design is approved at rev 5: `.agents/tasks/phase1a-design.md`. Design review pass 5 is APPROVED.
  - The plan is `.agents/tasks/phase1a-plan.md`. It is split into FEAT files under `.agents/tasks/task-phase1a-listener/`.
- Phase 1b (Mac → 16R playback, talker) is in progress on branch `feat/companion-app`.
  See "Phase 1b: talker" below for the resume point. Audio does not yet reach
  the 16R's outputs.

| FEAT | Scope | Status | Commit |
|---|---|---|---|
| 001 | Dext ABI v3: shared RX rings, classifier, StepClock, counters, stream filter (C++) | done | `013d733` |
| 002 | `avb-core`, `avb-dext` (ABI mirror, shm consumer), probe `counters` | done | `b002a39` |
| 003 | `avb-gptp` (slave-only 802.1AS, servo), `avb-srp` (MRP/MSRP/MVRP) | done | `ffb83e3` |
| 004 | `avb-avtp` (AAF, stream format) + `avb-avdecc` (ADP/AECP/ACMP, controller, listener entity, ListenerSm) | done | `802891b` |
| 005 | `avb-mediaclock`, `avb-audioring`, `avb-daemon` (`record` CLI), packaging | done | `a631a82` |

After FEAT-005 the plan is:
- An implement-and-review loop over the whole diff. The reviewer always runs last and gates the loop.
- Four hardware milestones, in order:
  1. hw-gptp: lock to the 16R as GM for at least 5 minutes, with asCapable true.
  2. hw-avdecc: dump the full AEM to JSON.
  3. hw-streams: bind all of the 16R's STREAM_OUTPUTs and confirm 8000 pkt/s per stream.
  4. hw-audio: a 600 s multichannel WAV with no sequence gaps and the media-clock drift in ppm. Pause and ask the user to feed signal into specific 16R inputs so channel mapping can be checked.
- `docs/phase1a-results.md`.

Phase 1b (next) covers three things. The first is a Rust Core Audio AudioServerPlugIn, an input-only device fed from the audio ring. The second is a small macOS app showing status and a sample-rate selector driven by the 16R's AEM. Rate changes go through the daemon, then AVDECC, then `RequestDeviceConfigurationChange`. The third is validation in Reaper, then Pro Tools. The feasibility report covers the model under "Sample rates".

### Resume point: all code FEATs done; review loop + hardware next

FEAT-001…FEAT-005 are all **done** on `phase1a-listener`. The review loop is
done (`104df2b`). **hw-gptp** (`353f662`) and **hw-avdecc** (`bcfe889`) are
done on the real hardware. The whole Rust workspace (11 crates, 320 tests) and
the C++ dext tests build and pass; the packaged `avb-daemon` is signed with
`packaging/daemon.entitlements` (I210 only). Decisions and every FEAT's
deviations, plus the hw-gptp/hw-avdecc results and the seven hardware bugs
found, are recorded in `.agents/tasks/phase1a-impl-verification.md`.

Hardware state (as of the milestone runs): the **ABI v3 dext (283.0.31) is
activated and matched** to the I210 (`AVBI210Driver`, `avb-probe info` reads
it). It was activated via `avb-installer activate`; a reboot + Thunderbolt
replug were needed once to clear a tangled sysextd and let the dext win the
match over Apple's generic E1000 driver. The 16R is cabled to the I210.

Phase 1a hardware: COMPLETE. All four milestones pass on the live 16R over the
I210 — see `docs/phase1a-results.md` and `.agents/tasks/phase1a-impl-verification.md`.

1. **hw-gptp** — DONE (`353f662`). gPTP locks to the 16R GM, 5+ min, offset
   mean 0.05 ns / sd 5.2 ns.
2. **hw-avdecc** — DONE (`bcfe889`). 21/21 AEM descriptors, 0 timeouts.
3. **hw-streams** — DONE (`d6ada09`, `0d30231`). 8 streams × 8000 pkt/s, 0
   sequence gaps. Evidence `hw-streams-pass.json`.
4. **hw-audio** — DONE (`d3fbfb3`). 600 s 8-ch 48 kHz WAV, 0 gaps, -7.8 ppm
   drift. Channel map verified (16R ch5 → stream 1 ch4). Evidence
   `hw-audio-pass.json`.
5. `docs/phase1a-results.md` — DONE.

### Hardware bugs fixed (all in `phase1a-impl-verification.md`)
gPTP: init_datapath flags, mapped ctl-ring RX vs PollRx, TX_TS status polarity,
clock-gen seed/resync, SetClock vs bounded step. AVDECC: AVTP header byte-order
inversion (adp/aecp/acmp), READ_DESCRIPTOR missing reserved field.

<!-- superseded resume notes below -->

3. `docs/phase1a-results.md` from the milestone runs.

The ctl/audio/rec threads (§5.1) and the full `run`/`record` loops are wired
during the hardware milestones; the daemon binary carries a documented
crate-level `#![allow(dead_code)]` until then (see the FEAT-005 deviations).

## Phase 1b: talker (Mac → 16R)

Branch `feat/companion-app`. The playback path exists end to end — CoreAudio
output device → shm ring → AAF PDUs on the wire — and is confirmed moving real
audio into the ring on hardware. **Audio does not yet come out of the 16R.**

Found on the live 16R, none of it caught by the unit suite (each is now
covered by a test that would have):

1. `kAudioServerPlugInIOOperationWriteMix` was guessed as `'wmix'`. Apple's
   value is `'rite'`. The plug-in declined WriteMix, the HAL never dispatched
   it, and every transmitted PDU was zero-fill. `31279be`. The one test
   covering it was a tautology (our constant against itself). All 66 FourCCs
   and terminal types are now pinned to literal SDK numbers, and
   `scripts/audit-coreaudio-constants.py` re-checks them against the SDK
   headers (exits non-zero on a mismatch).
2. `kAudioStreamTerminalTypeSpeaker` was `0x0301`, the USB Audio Class value,
   not CoreAudio's `'spkr'`. `e1e4d68`, found by that audit.
3. The plug-in kept writing into a *deleted* ring after a daemon restart: the
   daemon unlinks and recreates the ring file (new inode), the old mapping
   stays valid, and `start_io` attached only `if guard.is_none()`. Silent until
   coreaudiod restarted. Now re-attaches on an inode change. `7676de3`.
4. No MSRP Talker Advertise was ever declared, so the 16R listed no input
   device — a Milan listener accepts no stream it has not been advertised. The
   talker loop also had to learn to RECEIVE, since MRP is two-way and Listener
   Ready arrives via RX. `c00c4d8`. The TSpec is spec-derived, **not** copied
   from a working talker: the 16R sent no Advertise of its own to compare
   against. `talker_tspec` is pure and tested.
5. `AVB_TX_RATE_PER_SEC` was 1000, shared across every frame the dext sends. A
   class A talker needs 8000/s, so 7 of 8 audio PDUs never reached the wire and
   the audio flood starved our own gPTP/MSRP/MVRP of tokens (measured: 382 553
   of 441 373 dropped in 60 s). Now 20 000/s. The send site also discarded
   `SendResult`, so the summary counted attempts as sends. `f7d81d0`.
6. We answered no AECP at all, so the 16R could discover us over ADP and then
   learn nothing: a Milan controller enumerates by walking ENTITY ->
   CONFIGURATION 0 -> every descriptor in `descriptor_counts`, and we had no
   CONFIGURATION descriptor and `talker_stream_sources = 0`. The responder had
   existed in `avb-avdecc` since Phase 1a and was never connected to the
   socket — the daemon only LOGGED inbound AECP. `entity::talker` now answers
   the whole walk for the playback direction, and the daemon dispatches to it.
   `04f7541`.
7. The AVTP presentation timestamp was nanoseconds-since-loop-start, and the
   talker loop never serviced gPTP, so the clock it would have read was
   free-running anyway. `gptpsvc::GptpService` is the `gptp` command's loop
   body, now shared; the talker pumps it, reads the disciplined clock, and
   refuses to stream until the servo is LOCKED. `04f7541`.

### Resume point

The dext must be REINSTALLED for (5) to take effect — the ceiling is compiled
into it. Packaged dext is `284.1.16`; `283.0.31` is installed. The HAL plug-in
installed on this machine already postdates every plug-in fix above, so only
the dext needs replacing.

```
/Users/dexter/AVB-Audio/build/AVBInstaller.app/Contents/MacOS/avb-installer activate
/Users/dexter/AVB-Audio/build/AVBInstaller.app/Contents/MacOS/avb-daemon play --seconds 60 --log-level info
```

`play` now WAITS for gPTP lock before transmitting a single PDU, so expect a
`gPTP locked ... arming the talker` line first; if the servo does not lock
within the discovery timeout the run exits rather than streaming on an
undisciplined clock. Then read the summary: `PDUs on the wire` should be
~8000/s with `rate-limited 0`, `AEM commands answered` should be non-zero (if
it is zero the 16R never tried to enumerate us, which is a different fault),
and `listener_ready` must become true.

Still unbuilt, and the next likely blockers:

- **GET_STREAM_INFO answers NOT_IMPLEMENTED on purpose.** Its 32-bit flags
  word needs bit positions from 1722.1-2021 Table 7.206. The working group's
  own clarification (Illouz, "AVDECC clarifications Part 2 — AEM commands",
  rev 4, §6.2) confirms the field semantics but gives no numeric positions.
  Every descriptor we DO encode is anchored — the same layouts decoded 21 real
  16R descriptors in hw-avdecc — whereas this response originates a flags word
  we have never parsed, so a wrong bit asserts something false about our
  stream with nothing to catch it. Needs the standard, not a guess.
- **No START_STREAMING / STOP_STREAMING.** The same clarification (§6.1.2)
  says a controller sets a talker source active with START_STREAMING on its
  STREAM_OUTPUT. We are our own controller here (we send CONNECT_RX_COMMAND to
  the 16R), so it may not matter on this link, but a third-party AVDECC
  controller would expect it.
- **The media clock is the Mac's, not the 16R's.** CoreAudio produces on the
  host clock while we transmit paced by gPTP, so the two drift. This is the
  mirror image of the listener's ring problem and is why STREAM_OUTPUT does
  not advertise CLOCK_SYNC_SOURCE. Unaddressed.

## Verified at `04f7541` (Phase 1b)

- `cargo fmt --all --check`: clean.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: clean.
- `cargo test --workspace --locked`: 370 pass — avdecc 116, srp 31, gptp 59,
  daemon 34, dext 28, mediaclock 26, core 22, avtp 16, probe 16, audioring 15,
  coreaudio 7.
- C++ host tests: 342 i210 checks and 97 tg3 checks pass.
- `xcodebuild … AVBI210Driver`: BUILD SUCCEEDED, 0 compiler warnings.
- `scripts/package.sh --adhoc`: dext `284.1.16`, all shipped binaries arm64.

## Verified at the FEAT-005 commit (`a631a82`)

- `cargo fmt --all --check`: clean.
- `cargo test --workspace --locked`: all pass — core 22, dext 28, gptp 59,
  srp 30, avtp 11, avdecc 87, mediaclock 26, audioring 10, daemon 27 (incl. the
  48 kHz/96 kHz pipeline tests with the 0-allocation assertion), probe 16.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: clean.
- C++ host tests: 341 i210 checks and 97 tg3 checks pass.
- `xcodebuild … AVBI210Driver`: BUILD SUCCEEDED, 0 compiler warnings.
- `scripts/package.sh --adhoc`: both dexts + Rust release built, package
  assembled and ad-hoc signed; `avb-daemon` signed with `daemon.entitlements`.

Note: `package.sh` needs `cargo` on `PATH` for its Rust step. In this
environment `~/.cargo/bin` is not on the default subprocess `PATH`, so prefix
the command with `PATH="$HOME/.cargo/bin:$PATH"` (and invoke `cargo` as
`/Users/dexter/.cargo/bin/cargo`).

## Gate commands

Run all of these from the repo root with absolute paths. Never `cd`.

```
TMPDIR=/Users/dexter/AVB-Audio/.agents/tmp/ cargo fmt    --manifest-path /Users/dexter/AVB-Audio/Cargo.toml --all --check
TMPDIR=/Users/dexter/AVB-Audio/.agents/tmp/ cargo clippy --manifest-path /Users/dexter/AVB-Audio/Cargo.toml --workspace --all-targets --locked -- -D warnings
TMPDIR=/Users/dexter/AVB-Audio/.agents/tmp/ cargo test   --manifest-path /Users/dexter/AVB-Audio/Cargo.toml --workspace --locked
TMPDIR=/Users/dexter/AVB-Audio/.agents/tmp/ make -C /Users/dexter/AVB-Audio/driver/tests test
/usr/bin/xcodebuild -project /Users/dexter/AVB-Audio/driver/AVBI210Driver.xcodeproj -scheme AVBI210Driver -configuration Debug -sdk driverkit \
  -derivedDataPath /Users/dexter/AVB-Audio/.agents/tmp/DerivedData SYMROOT=/Users/dexter/AVB-Audio/build/driver \
  OBJROOT=/Users/dexter/AVB-Audio/build/driver/obj CODE_SIGNING_ALLOWED=NO build
/Users/dexter/AVB-Audio/scripts/package.sh          # signed; --adhoc for an unsigned build check
```

## Repo map

- `driver/AVBShared.h`: the C++ ABI (v3), mirrored in `crates/avb-dext/src/abi.rs` with offset tests.
- `driver/AVBI210Driver/`: the I210 PCIDriverKit dext. `I210Regs.h` and `I210Logic.hpp` hold pure helpers with host tests.
- `driver/AVBBcmDriver/`: the Broadcom dext. Shelved, but kept in the repo.
- `driver/tests/`: C++ host tests (`make`).
- `crates/`:
  - `avb-core`, `avb-dext`, `avb-gptp`, `avb-srp`: done.
  - `avb-avtp`, `avb-avdecc`: WIP.
  - `avb-probe`: the hardware test CLI.
  - `avb-installer`: the `OSSystemExtensionRequest` activate/deactivate tool.
- `scripts/package.sh`: builds, signs and stamps the version, then produces `build/AVBInstaller.app`.
- `docs/`: hardware results.
- `.agents/tasks/`: requirements, design, reviews, plans, verification evidence and raw hardware JSON.

## Host and hardware state

- Mac16,8 (Apple Silicon), macOS 26.5.2.
- SIP is **disabled**, `boot-args=amfi_get_out_of_my_way=1` is set, and `systemextensionsctl developer on`.
- To undo: `sudo nvram -d boot-args`, `systemextensionsctl developer off`, then `csrutil enable` from Recovery.
- Signing uses "Apple Development: armitage787@gmail.com", Team `8NG8NMGYVF`. This is a free personal team with no DriverKit profile. That is why AMFI is disabled.
- Our dext `com.avbaudio.installer.i210` (282.7.31, the Phase 0b build) owns the I210, 8086:1533 / subsystem 1ab6:0214, behind Thunderbolt. en7 is gone while we own it.
  - The ABI v3 dext from FEAT-001 has **not** been activated on hardware yet.
- The I210 is cabled directly to the 16R. There is no switch. The 16R's gPTP port identity is `00:0a:92:ff:fe:03:3b:a9/1`.
- The Broadcom adapter (en8) is back on Apple's `BCM5701Enet`. Never touch it.
- The user's LAN is Wi-Fi.

## Hard-won lessons

- The dext `CFBundleVersion` must be a kext-style version. `package.sh` stamps `<days since 2026-01-01>.<UTC hour>.<UTC minute>`. Bump it on every rebuild.
- The dext binary must be **arm64e**. A plain arm64 DriverKit binary gets matched, then refused at exec, and Apple's driver takes the device instead.
- Upgrading an already-approved dext activates without re-approval. A replug of the I210 adapter may be needed to rematch. Run `avb-installer activate` yourself and check `ioreg` and the kernelmanagerd log before asking the user to do anything.
- I210: RX timestamps are in-buffer (TSIP), with TSYNCRXCTL 0x19, ETQF3 0x440088F7 and SRRCTL0 0xC2000402. The TIMINCA base is 0x00346dc6. The 1000 Mb/s latency constants are TX 178 ns and RX 448 ns. RQDPC(0) wraps instead of clearing on read. The adapter's crystal runs about +75 ppm against mach.
- 16R / Milan streams are AAF PCM 32-bit, 8 ch per stream, 6 samples/frame at 48 kHz, class A. Rates and channel counts must come from the AEM, never be hardcoded.
- GPL/LGPL references (Linux tg3/igb, igb_avb kernel part, L-Acoustics avdecc) may be used for behavior only. Never copy their code.
- Distribution later needs a paid Apple Developer account plus a DriverKit PCI entitlement request for 8086:1533. The odds are better than for the Broadcom.

## User decisions

- Listener only (record 16R → Mac) until Phase 1 works. Keep the design talker-ready.
- I210 first. Switches and other mixer models come later.
- Start at 48 kHz. Add 44.1 kHz only if the AEM advertises it. The sample rate must be selectable in a macOS app.
- Receive all of the 16R's output streams. Our daemon is the AVDECC controller, so no Hive is needed.
- gPTP priority1 = 255, so the 16R is grandmaster.
- Pin dependency versions exactly. Commit locally only and never push. Never change boot security or NVRAM yourself; give the user the commands.
