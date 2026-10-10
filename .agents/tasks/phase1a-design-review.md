# Phase 1a design review (pass 4)

Reviewed: `.agents/tasks/phase1a-design.md` (revision 4) against `.agents/tasks/phase1a-requirements.md`, `docs/phase0b-i210-results.md`, `.agents/tasks/avb-hw-feasibility.md` and `.kiro/steering/scratch-files.md`. Code claims were re-checked against the tree at `phase1a-listener` (4035832): `driver/AVBShared.h`, `driver/AVBI210Driver/AVBI210Driver.cpp`, both user clients, `packaging/*.entitlements`, `Cargo.lock`, `rust-toolchain.toml`, and the DriverKit SDK `IOUserClient.h`.

Verdict: **CHANGES_REQUESTED** (0 HIGH, 2 MEDIUM, 6 NIT).

All 8 pass-3 findings are in the text: step bookkeeping and the NRR window rules (§7.2/§7.3), the Announce-timeout port edges and the HOLDOVER exit (§7.3), TA_WAIT and the Asking Failed edge plus the "ask the user" branch (§4, §8.2, §9.4), `rxHarvestStep` (§3.4), `sync_channel(4)` (§5.1), the effective-Domain `msrp_mappings` (§9.3), ACMP controller sequencing and matching (§9.2), and the named `__atomic_*` primitives (§3.4). The ABI v3 layout, the dext changes, the filters, the dependency set and the scope are unchanged from pass 3 and still check out. The dext side is buildable as written.

The two remaining MEDIUM findings are both in rev-4 text. One is a timing assumption in the new step bookkeeping whose stated justification is wrong. The other is a hole in the listener SM that the new TA_WAIT rule exposed.

## Findings

### 1. MEDIUM: The 2 ms step-ambiguity window depends on timer latency, and the design's justification for it is wrong

Where: §7.3 "Step bookkeeping", RX generation rule (`harvest_mach > s.step_end_mach + 2 ms` → new generation).

Problem:
- The design says a frame received before the step "is harvested within that bound unless the harvester is more than 2048 descriptors behind, which DP-6 sizing excludes". That's the wrong quantity. Whether a pre-step frame is harvested within 2 ms depends on `IOTimerDispatchSource` dispatch latency, not on descriptor backlog. One tick drains the whole ring (the loop is bounded by `kRxRingSize`).
- The design expects late ticks elsewhere. It defines `kAvbCtrHarvestLateTicks` for tick gaps > 2 ms, and the daemon watchdog tolerates `last_tick_mach` up to 50 ms old (§5.2). A single tick that slips by more than about 1.5 ms after a step gets pre-step frames labeled as the new generation.
- What a mislabel costs:
  - a pre-step Sync gives an offset that is wrong by the full step, so the servo issues a second step that undoes the first;
  - a pre-step 16R Pdelay_Req (t2, old timebase) is paired with a post-step t3, so we send a Pdelay_Resp_Follow_Up whose t3 − t2 contains the step. After the ACQUIRE `SetClock` that step is about 1.7e9 s. The 16R computes a garbage link delay, may drop asCapable on its side and may withhold Sync, which delays PTP-7 lock;
  - a pre-step Pdelay_Resp t4 enters the new NRR window with an error of about Δ/1 s, which is 100 ppm for a 100 µs step. That passes the 1000 ppm sanity bound.
- The ACQUIRE `SetClock` happens on every run, so the case comes up on every run whenever the timer slips at the wrong moment.

Fix: make the classification come from the dext, with no dependence on timing. This is a small ABI v3 addition, and ABI v3 isn't frozen yet:

```c
// AvbRxSlot: use 4 of pad1's 24 bytes
uint32_t clock_gen;     // 40  dext clock generation at harvest
uint8_t  pad1[20];      // 44
#define AVB_RXF_TS_STEP_AMBIG (1u << 3)  // harvested in the step's settling window; don't use hw_ts for timing
// AvbShmHeader: use 4 of reserved[128]
uint32_t clock_gen;     // 896 incremented by every SetClock/StepClock; release-stored
uint8_t  reserved[124]; // 900
```

Dext rule (on the driver queue, so it is serialized with `TimerFired`):
- `SetClock` and `StepClock` increment `ivars->clockGen`, release-store it into `hdr->clock_gen`, and record `ivars->stepMach = mach_absolute_time()` taken right after the SYSTIM write.
- Every slot gets `clock_gen = ivars->clockGen` at harvest.
- A slot gets `AVB_RXF_TS_STEP_AMBIG` if it is harvested by any tick from the step up to and including the first tick that starts at least 100 µs after `stepMach`. That tick drains every descriptor whose DMA completed before the step, and later ticks contain only post-step receptions.
- `AdjFreq` doesn't change `clockGen`.
- Add a host-tested pure helper, `stepAmbiguous(tickStartMach, stepMach, firstSettledTickSeen)`.

Daemon rule (replaces the `harvest_mach` windows in §7.3):
- `AMBIG` set → AMBIGUOUS. The existing consequences stay: no timing use, `rx_ts_ambiguous_step`, and a Pdelay_Req is answered without Follow_Up.
- Otherwise the RX generation is `slot.clock_gen`.
- The current generation is `hdr.clock_gen`, read after each `StepClock`/`SetClock` returns. TX generation stays the generation current at `SendFrame` (the held-step rule is unchanged).
- Update the §3.1 static_asserts and offset literals (C++ and Rust), the §3.7 tests, and the §14.1 boundary test, which becomes: slots from the first settled tick are AMBIG, slots from the next tick are new-gen, under a simulated 5 ms tick delay.

If the designer prefers to keep the daemon-side window, then at minimum: replace the 2048-descriptor justification with the timer-latency argument, make the window adaptive (`step_end_mach` + the larger of 2 ms and 2 × the worst tick gap observed since `InitDatapath`), and have the daemon wait until `hdr.last_tick_mach > step_end_mach + window` before it uses any RX timestamp. Either way, state the rule so that it doesn't depend on an unmeasured timer bound.

### 2. MEDIUM: ACMP listener SM: entering SETTLED_NO_RSV with Talker Failed already registered declares nothing, and a TA change in SETTLED_RSV_OK is unspecified

Where: §9.4 state list (TA_WAIT is "armed on entry if neither a TA nor a Talker Failed is registered"), the transition table, §8.2 ("When the TA … registers (or changes)"), and the §14.1 test "TA_WAIT not armed when Talker Failed is registered on entry".

Problem:
- **Talker Failed on entry.** The table is declared exhaustive and edge-driven. The TA row covers "TA registered (or already registered on entry)". The Talker Failed row has no "already registered on entry" clause.
  - If the 16R's Talker Failed was registered while the stream was in PRB_W_* (where the MSRP event is ignored), then on entry to SETTLED_NO_RSV, through PROBE_TX SUCCESS or the fallback, TA_WAIT is not armed, no Talker Failed event follows, and **no Listener declaration is ever made**.
  - The A-5 classification from the Talker Failed failure code never happens either. `record` then exits 8 at start-timeout with the wrong reason, instead of `not_simultaneously_streamable`.
  - The §14.1 test name confirms the case exists, but no action is defined for it.
- **TA change while SETTLED_RSV_OK.** §8.2 says the TA is compared "when it registers (or changes)", with the effective MAC/VID switched and the filter re-issued. The §9.4 table has no "TA changed" row for SETTLED_RSV_OK, so by the exhaustive-table rule the event is `acmp_listener_ignored`. The two sections conflict, and an implementer has to pick one.

Fix: in §9.4,
- change the Talker Failed row's event to "Talker Failed registered (or already registered on entry, with no TA registered)". The action is unchanged: declare Asking Failed and, if the failure code is in `A5_FAILURE_CODES`, classify A-5;
- add the row `SETTLED_RSV_OK | TA attributes changed (same stream_id; dest MAC, VID, TSpec or accumulated latency differ) | SETTLED_RSV_OK | apply §8.2 (compare, ta_probe_mismatch, effective values, SetStreamFilter + audio table on MAC/VID change); Listener Ready stays declared`;
- add both rows to the §14.1 one-test-per-row list.

### 3. NIT: The ACQUIRE SetClock needlessly drops asCapable and bounces the servo through HOLDOVER

Where: §7.3 "NRR window" ("After the ACQUIRE `SetClock` there is no previous valid NRR (SYSTIM was near 0), so nothing is held"), together with the HOLDOVER entry "asCapable false" and the Slave → Listening edge "asCapable false > 3·Ta".

Problem: NRR is a ratio of differences within one generation, so it doesn't depend on SYSTIM's absolute value. A pre-SetClock NRR from one generation is as valid as any other. What pass 3 flagged was a window that *straddles* the step, and that is now prevented by clearing the window. With nothing held:
- asCapable goes false for 1–2 Pdelay intervals right after every ACQUIRE;
- the servo leaves ACQUIRE for TRACKING and then drops straight into HOLDOVER;
- if the 16R's Announce interval is ≤ 0.5 s, the port drops to Listening, since 3·Ta ≤ 1.5 s.

None of this breaks acceptance, because it all happens before lock. It does add a spurious asCapable transition and a port flap on every run and every GM change.

Fix: hold the last valid same-generation NRR across the ACQUIRE `SetClock` exactly as across a `StepClock`. The ACQUIRE seed after a later GM change must still come from an NRR computed from a single generation, which this rule already guarantees. Update the §14.1 test to: asCapable stays true across the ACQUIRE SetClock.

### 4. NIT: It's unclear which correctionField syncEvent uses

Where: §7.3 offset formula `syncEvent = preciseOriginTimestamp + correctionField(ns) + …`.

Problem: it doesn't say whether this is the Follow_Up correctionField, the Sync correctionField, or both.

Fix: `syncEvent = preciseOriginTimestamp + (FU.correctionField + Sync.correctionField) · 2^-16 ns + delay_comp · rateRatio / NRR_systim`, with a comment that the Sync value is expected to be 0 in two-step 802.1AS *(verify clause)*. Pin it with a vector that has a non-zero Sync correctionField.

### 5. NIT: A held step can push a Pdelay_Resp past the 10 ms turnaround bound

Where: §7.3 TX generation ("starts no new `want_ts` frame while a step is pending", hold ≤ 20 ms) and §7.2 responder turnaround (≤ 3 ms worst case, bound 10 ms).

Problem: if a TX timestamp times out while a step is pending, a 16R Pdelay_Req that arrives in that window waits up to 20 ms for its Pdelay_Resp. That breaks the stated worst case. It's rare: it needs a TS timeout, where the DP-9 target is 0.

Fix: state the exception in §7.2: "a Pdelay_Resp held behind a pending step is dropped if t_now − t2 > 8 ms, and `pdelay_resp_late_drop` is counted". Alternatively, add Pdelay_Resp to the frames that may go out before the held step, since its Follow_Up is then skipped by the existing responder rule anyway.

### 6. NIT: The ring clock block's seqlock in shared memory isn't specified as atomic fields

Where: §10.6 clock block at offset 384; §5.1 `avb_core::SeqLock<T: Copy>`.

Problem: a seqlock whose reader copies plain `f64`/`u64` fields while the writer stores them is a data race in Rust (UB), and here the memory is shared across processes for Phase 1b.

Fix: specify the clock block fields as `AtomicU64` (f64 via `to_bits`/`from_bits`) and `AtomicU32`. The writer does `seq` odd (Relaxed) → `fence(Release)` → field stores (Relaxed) → `seq` even (Release). The reader does `seq` (Acquire) → field loads (Relaxed) → `fence(Acquire)` → `seq` re-check. Apply the same pattern to `avb_core::SeqLock` (per-word atomics, `T: Copy` limited to u64-word-sized PODs).

### 7. NIT: No writer is named for AVB_FLAG_OWNED and AVB_FLAG_RINGS_ATTACHED

Where: §3.1 (new flag bits) and the §3.4 writer table (`flags` only through `setFlags`).

Fix: add to §3.6: `RingAttach` calls `setFlags(v, AVB_FLAG_RINGS_ATTACHED | AVB_FLAG_OWNED, 0)`, and `RingDetach`/`ClientGone` call `setFlags(v, 0, AVB_FLAG_RINGS_ATTACHED | AVB_FLAG_OWNED)` before `rxMem` is released, so the header mirror is still valid when the write happens.

### 8. NIT: `last_available_index` is never said to be updated

Where: §9.4 per-binding state and the reboot row (`available_index < last_available_index`).

Fix: "`last_available_index` := the talker's `available_index` on every ENTITY_AVAILABLE received from it, in every state including UNBOUND (initialized from the ADP table at BIND_RX)". Otherwise a second reboot isn't detected.

## Verified assumptions

- **Tree state.** HEAD is `phase1a-listener` at 4035832, and only design and review docs are untracked. `AVBShared.h` is ABI 2, includes only `<stdint.h>`, and still has the "16..31 reserved" comment and flags bits 0–4. Both user clients use `kAvbSelCount` at lines 54, 66 and 131 with `static_assert(kAvbSelCount == 10)`. The §3.1 edits apply as described.
- **Harvest.** `AVBI210Driver.cpp` reads `harvest_mach` per descriptor inside the harvest loop (line 645), and `rxBufs` is a single `DmaBuf` (line 96, allocated as `kRxRingSize × kRxBufSize` at line 788). This matches the design's description of what changes into 16 chunks. Every `v->stage =` write is in Stage A, `initDatapath` or `shutdownDatapath` (lines 346, 771, 807), which matches the writer table. `setFault` is the single latch point (line 127). The timer re-arms with leeway `period/4` (line 728). The design's "leeway 50 µs" at 500 µs is a deliberate change of that value, which is acceptable.
- **SDK.** `kIOUserClientMemoryReadOnly` and `IOUserClient::CopyClientEntitlements` both exist in the DriverKit SDK `IOUserClient.h`.
- **Entitlement check.** `packaging/probe.entitlements` already lists `com.avbaudio.installer.i210` under `userclient-access`, so the new `Start` check accepts the probe. `daemon.entitlements` is specified with the same key.
- **Pins.** `Cargo.lock` has anyhow 1.0.104, clap/clap_derive 4.6.7, serde/serde_derive 1.0.229, serde_json 1.0.151, io-kit-sys 0.5.0, core-foundation-sys 0.8.7, mach2 0.7.0 (+ transitive 0.5.0) and libc 0.2.190. The toolchain is `1.99.0-aarch64-apple-darwin`. No new crates; all permissive.
- **ABI v3 arithmetic (recomputed).**
  - `AvbRxSlot` is 2048 B with data at 64.
  - `AvbShmHeader` is 1024 B with heads at 128/256, counters at 384–896 and reserved 896–1024. Finding 1 fits inside reserved and pad1 without moving anything.
  - `AvbShmConsumer` is 256 B; `AvbStreamFilter` is 136 B.
  - `AVB_RX_MEM_SIZE` = 17 842 176.
  - The ring v1 header offsets (clock block 64 B used of 128; streams 1024–2048; channel_map 2048–10 240; gap log ending at 14 344) are consistent.
- **Filters (DP-4).** gPTP 0x88F7 and MSRP 0x22EA go to 01:80:C2:00:00:0E, and MVRP 0x88F5 to 01:80:C2:00:00:21. AVDECC control subtypes 0xFA/0xFB/0xFC and MAAP 0xFE go to 91:E0:F0:01:00:00, 91:E0:F0:00:xx:xx or our MAC. Streams match by subtype, then the filter MAC, then the tag VID. Subtype is tested before the filter, so stream MACs inside the MAAP range are handled correctly.
- **Phase 0b consistency.** SRRCTL0 0xC2000402 (Drop_En, Timestamp, advanced one-buffer, 2 KiB) agrees with RQDPC(0) as the drop counter. TSYNCRXCTL 0x19 and ETQF3 0x440088F7 are unchanged. The latency constants 178/448 ns, link delay 142.6/455.6 ns, NRR −83.18 ppm and SYSTIM +75 ppm vs mach are used consistently in §7.
- **gPTP signs.** The servo sign conventions, the ACQUIRE NRR seed and the delay formula are correct. For syncEvent, NRR_systim cancels out of `rateRatio / NRR_systim`, so the AdjFreq transient at ACQUIRE only perturbs `delay`, by about 2 ns.
- **The rest of rev 4 checks out.**
  - The port-state edges are complete for Announce timeout, asCapable loss, same-GM and new-GM return, and Slave → Slave GM change.
  - HOLDOVER exits only on an accepted Sync with asCapable true.
  - The Announce interval range and placeholder are defined.
  - ACMP controller matching excludes loopback echoes by message_type.
  - Three separate sequence counters are defined.
  - `msrp_mappings` follows the effective Domain.
- **Policy and scope.**
  - No new register writes.
  - The four clear-on-read reads are denylisted, including RQDPC(0) at 0xC030.
  - Device scope is unchanged (SAF-1).
  - No GPL/LGPL material.
  - No sudo, `nvram`, `csrutil`, `bputil`, `/tmp` or `cd` in anything the design adds; ENV-1 is honored with `-derivedDataPath` under `.agents/tmp`.
  - Rates, channels and stream counts come from `derive`, with the §10.5 durations and the §14.4 grep list.

## Unverified or wrong assumptions

- **Wrong (finding 1):** that a pre-step frame is always harvested within 2 ms of the step, justified by descriptor backlog. The real bound is timer dispatch latency, which the design itself expects to exceed 2 ms at times (`kAvbCtrHarvestLateTicks`, the 50 ms watchdog).
- **Wrong (finding 3):** that the pre-SetClock NRR is invalid because SYSTIM was near 0. NRR is offset-invariant within one generation.
- **Unverified, with a hole in the defined branch (finding 2):** the order in which the 16R registers Talker Failed relative to our PROBE_TX completion.
- **Unverified, legitimately HW-gated and listed in §15:** AS-3, AS-5, AS-6, the 17 MiB `rxMem` and 16 × 256 KiB within DriverKit limits, the 32 KiB descriptor ring as one segment, 500 µs timer jitter against NFR-2, `CopyClientEntitlements` results for ad-hoc-signed clients, U-1…U-13, every *(verify)* clause and code in Milan / 1722.1 / 802.1Q / 802.1AS (including 802.1Q §35.2.4.3/§35.2.6 for Listener-without-Talker), and the 333016 sections for CRCERRS/MPC/RNBC/RQDPC(0) and the VP bit.
- **Not re-checked in this pass** (verified in pass 2/3): `mach2 0.7.0` exports of `thread_policy_set` / `mach_wait_until`, `io-kit-sys 0.5.0` exports of `IOConnectMapMemory64` / `IOConnectUnmapMemory64`, and the feasibility doc's listener plan (unchanged since pass 3).
