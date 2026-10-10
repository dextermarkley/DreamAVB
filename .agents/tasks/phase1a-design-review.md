# Phase 1a design review (pass 5, convergence)

Reviewed: `.agents/tasks/phase1a-design.md` (rev 5) against the pass-4 review and §21 of the design. This pass only checks closure. I checked each pass-4 fix in the text it touches and looked for new HIGH/MEDIUM correctness issues that rev 5 introduced. Sections approved in earlier passes were not re-reviewed. For the code claims rev 5 adds, I checked `driver/AVBI210Driver/AVBI210Driver.cpp`: the `setClock` callers and `armClock`/`stageA`.

Verdict: **APPROVED** (0 HIGH, 0 MEDIUM, 3 NIT).

## Pass-4 findings: closure

| # | Sev | Status | Evidence |
|---|---|---|---|
| 1 | MED | Fixed | §3.1: `AvbRxSlot.clock_gen` at 40 and `pad1[20]` at 44, so data stays at 64. `AvbShmHeader.clock_gen` at 896 and `reserved[124]` at 900, so the header stays 1024 B. `AVB_RXF_TS_STEP_AMBIG` = bit 3 in the u16 `flags`. Static_asserts and Rust literals are updated. §3.4: `ambig` is computed once per tick from the measured `tickStart`, and each slot is tagged with `clock_gen`/AMBIG. §3.6: the `setClock()` helper bumps `clockGen`, release-stores it into the header and calls `rxStepArm`. `rxStepAmbiguous` settles on the first tick that *starts* ≥ 1 ms after the SYSTIM write, so a late timer can only lengthen the ambiguous span. The wrong 2048-descriptor justification is gone. §7.3: the daemon now classifies from the slot alone and re-reads `gen` from `hdr.clock_gen` after each owner-only Step/SetClock. §13.2: adds `slot_gen_invalid`. §3.7: invariant row. §14.1: host tests, including the 5 ms tick delay. |
| 2 | MED | Fixed | §9.4: the SETTLED_NO_RSV entry evaluation order is now fixed (TA → Talker Failed → arm TA_WAIT). The Talker Failed row covers "already registered on entry". A new `SETTLED_RSV_OK \| TA attributes changed` row keeps Ready declared. §8.2 points to both rows and defines `TalkerChanged`. One test per row in §14.1. |
| 3 | NIT | Fixed | §7.3: the NRR is held across the ACQUIRE `SetClock`. Test: asCapable stays true, no HOLDOVER. |
| 4 | NIT | Fixed | §7.3: the formula now sums FU and Sync correctionField, with a non-zero Sync correctionField vector. |
| 5 | NIT | Fixed | §7.2: an 8 ms late-drop with `pdelay_resp_late_drop`, and a stated reason for rejecting the bypass option. |
| 6 | NIT | Fixed | §5.1: `SeqLock<T: SeqWords<N>, N>` over `[AtomicU64; N]` with the specified fence protocol and an `Err(Contended)` retry bound. §10.6: the clock block uses the same protocol. Tests are in avb-core and avb-audioring. |
| 7 | NIT | Fixed | §3.3 ClientGone step 3, §3.6 RingAttach/RingDetach step 2, §3.4 writer table and §14.4: flags are set and cleared through `setFlags`, and the clear happens before `rxMem` is released. |
| 8 | NIT | Fixed | §9.4: `last_available_index` is initialized from the ADP table at BIND_RX, then updated on every ENTITY_AVAILABLE after the reboot comparison. Two-reboot test. |

## New findings (rev 5)

### 1. NIT: The settle argument assumes the descriptor ring wasn't exhausted during the settle window

Where: §3.6 "Why this is correct regardless of timer latency".

Problem: the argument says every pre-step frame is DMA'd within 1 ms. That only holds when host descriptors are available. Suppose the ring was full (RNBC) when the step happened. Pre-step frames, timestamped in the old timebase, then sit in the RX packet buffer until the settling tick rearms descriptors. They are DMA'd after that tick and harvested by the next one as new-generation and non-AMBIG. In practice this needs a harvester stall longer than the ring fill time (2048 descriptors, tens of ms at stream rates), which the 50 ms daemon watchdog already treats as abnormal. That is why this is not blocking.

Fix: settle only when the settling tick's harvest loop ended on a descriptor without DD, i.e. it harvested fewer than `kRxRingSize` descriptors. Otherwise keep `pending`. Add an argument to the helper: `rxStepAmbiguous(s, tickStartMach, ringWasFullLastTick)`. Add a host test: a full-ring tick doesn't settle, and the next tick that isn't full does.

### 2. NIT: §9.4 doesn't say whether a self-transition counts as "entry"

Where: §9.4 SETTLED_NO_RSV entry evaluation, the Talker Failed row (SETTLED_NO_RSV → SETTLED_NO_RSV) and the TA_WAIT row (also a self-loop).

Problem: suppose an implementer re-runs entry evaluation on a self-transition. The Talker Failed row would then re-trigger itself through the "already registered on entry" clause, and the TA_WAIT row would re-arm the timer every 2 s.

Fix: add to the state list: "A row whose next state equals the current state is not an entry: no entry evaluation, and the state timer is not re-armed. Entry evaluation runs only when the state changes."

### 3. NIT: The late-drop bound is measured from harvest, not from t2

Where: §7.2 responder exception (`mach_now − harvest_mach > 8 ms`).

Problem: `harvest_mach` lags reception by the harvest delay. With a late tick, a Pdelay_Resp we do send can exceed 10 ms after t2 even though it passed the 8 ms test.

Fix: use `mach_now − harvest_mach + (harvest delay bound)`, or convert t2 to mach through the published `GptpMachModel` when it's valid. Otherwise, document the bound as "≤ 8 ms + harvest delay".

## Verified assumptions

- **ABI arithmetic.**
  - Slot: 36 + 4 = 40 for `desc_len`, so `clock_gen` at 40, `pad1` at 44, and data at 64 (2048 B total).
  - Header: 896 + 4 = 900, plus 124 = 1024.
  - The AMBIG bit fits in the u16 `flags`.
  - No other offset and no selector moved, and `AVB_RX_MEM_SIZE` is unchanged.
- **`setClock` is the only SYSTIM write sequence.** In `AVBI210Driver.cpp`, `setClock` (line 290) is called from `armClock` (line 316, inside Stage A in `Start`) and from the `SetClock` selector (line 1129). `initDatapath` (line 775) never writes SYSTIM, so the daemon's `InitDatapath` after mapping (§5.2 step 6) can't make the cached `gen` stale.
- **Generation logic.** SYSTIM writes and `TimerFired` are both on the driver queue, so one `ambig` per tick, computed at tick start, is sound. Slot generation ≤ header generation always holds, so the §13.2 `slot_gen_invalid` rule is a pure trust-boundary check.
- **NRR held across SetClock.** The pre-ACQUIRE NRR was measured before `AdjFreq(f)`, so it is off by up to about 83 ppm until 2 new exchanges complete. The effect on `delay` is about (t4 − t1)·83e-6/2 ≈ 0.04 ns, and `rateRatio/NRR_systim` cancels in syncEvent. Holding it is harmless.
- **SM entry ordering.** The TA → Talker Failed → TA_WAIT order makes entry from PROBE_TX, the fallback and TA-withdrawn deterministic.

## Unverified or wrong assumptions

- **Unverified (NIT 1):** that pre-step frames are always DMA'd before the settling tick. This holds only if no RNBC occurs during the settle window.
- **Unverified, HW-gated and correctly marked *(verify)*:** the RXPBS size behind the 1 ms settle margin (333016 §8.10), and the 802.1AS clause for Sync correctionField in two-step mode.
- **Unchanged from pass 4:** the §15 hardware-only items.
