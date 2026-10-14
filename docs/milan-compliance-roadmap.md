# Milan v1.3 compliance roadmap

Updated 2026-10-13 after the grandmaster, merged entity, CRF, unsolicited
notification and live Apple-reference experiments.

Re-audited 2026-10-12 against the code as it stands, after the 19 commits that
followed the original audit. Method: four read-only agents, one per audit part,
each re-verifying its own findings against current code with `file:line`
evidence. Their raw reports are not committed; their verdicts are folded in
here, and **every claim I acted on I re-checked myself** — two of their
findings were wrong (see "Audit corrections" at the bottom), so treat the rest
as leads too.

The per-clause tables with verbatim spec text live in
`docs/milan-audit/part1..part4`. Those are the ORIGINAL audit, superseded for
planning by this file but still the best source for the exact clause wording.

## Closed since the original audit

Verified present, correct and tested. Grouped by the commit that closed them.

| Area | Clauses | Commit |
|---|---|---|
| Grandmaster capable: BTCA, Master state, Announce/Sync/Follow_Up TX, asCapable TX gate | 4.3.1.1, 4.2.6.1 | `62ea99a` |
| Servo time-source mode (frequency held, not reverted) | 4.3.1.1 | `56323e3` |
| `priority1` 248 + `GPTP_GRANDMASTER_SUPPORTED`; AVB_INTERFACE clock quality fed from the gPTP layer instead of a drifted copy | 4.2.6.2.1 | `3e873a0` |
| One entity, both directions: `listener_stream_sinks`, STREAM_INPUT, STREAM_PORT_INPUT, input clusters/maps, `number_of_stream_input_ports` | 5.3.3.1, 5.3.3.4, 5.3.3.7, 5.6.2 | `714e02c` |
| Sink count in the entity model id | 5.3.3.1 | `3198eb9` |
| Real GET_COUNTERS per descriptor, block ordering anchored on the 16R's own response | 5.3.6.3, 5.4.2.25 | `43ea8be` |
| GET/SET_NAME for every named descriptor, GET/SET_CONTROL for IDENTIFY, both lock-gated | 5.4.2.11/.12/.17/.18 | `26365bd` |
| Listener ACMP: BIND_RX / UNBIND_RX / GET_RX_STATE, Tables 5.27/5.32/5.37/5.44 | 5.5.3.1, 5.5.3.6 | `950fc0c` |
| START/STOP_STREAMING per STREAM_INPUT, NOT_SUPPORTED for STREAM_OUTPUT | 5.4.2.19/.20 | `950fc0c` |
| INPUT_STREAM clock source for the sink side | 5.3.3.6 | this commit |
| Counter honesty: untracked counters omitted from `counters_valid` | 5.4.2.25 | this commit |
| Earlier: GET_TX_CONNECTION NOT_SUPPORTED, TALKER_UNKNOWN_ID, `connection_count` 0, REGISTERING_FAILED/CL_ENTRIES_VALID cleared, `specification_version` 1.3.0.0, STREAM_INPUT buffer floor | 5.5.4.4, 5.5.2.2, 5.4.4.1, 5.3.3.4 | `0b00184` |
| Advertise state machine: TMR_DELAY, random delay, ENTITY_DISCOVER re-advertise and target filter | 5.6.3, 5.6.3.1, 5.6.3.5 | `1d98ff5` |

## What actually remains

Ordered by how much a real controller would notice.

### Large

1. **MVU BIND_STREAM / UNBIND_STREAM, then set `features_flags.MVU_BINDING`**
   (5.4.4.6/.7). The Milan-native binding path. The ACMP equivalent now works,
   so this is a second front end onto the same sink state machines. The flag
   must stay clear until they are served — `milan.rs` already refuses to claim
   it. Wire formats are transcribed in `part3-control-mvu.md`.
2. **MVU GET_STREAM_INPUT_INFO_EX** (5.4.4.8). *Unconditionally* mandatory —
   no "if implemented" escape. Needs the sink's probing status and ACMP status,
   which the listener SM now tracks.
3. **Complete unsolicited-controller lifecycle** (5.4.5.1-.4). Registration
   is capped and de-duplicated; state-change notifications now emit, and the
   daemon pushes per-STREAM_OUTPUT unsolicited GET_COUNTERS at Apple's captured
   2.5 s cadence (verified 633 pushes/200 s). Still missing: the 30–60 s
   CONTROLLER_AVAILABLE monitor and eviction of a departed controller.

### Medium

4. **The eight unmeasured STREAM_INPUT counters** (5.4.2.25, Table 5.13).
   MEDIA_LOCKED, MEDIA_UNLOCKED, STREAM_INTERRUPTED, SEQ_NUM_MISMATCH,
   MEDIA_RESET, TIMESTAMP_UNCERTAIN, LATE_TIMESTAMP, EARLY_TIMESTAMP are all
   mandatory and none are tracked. As of this commit they are honestly absent
   from `counters_valid` rather than reported as zero, so this is now visible
   non-compliance instead of a false claim — but it is still non-compliance.
   The receive path already sees every one of these events.
5. **Pdelay cease on multiple responders** (4.2.6.2.5). We detect the
   condition and drop asCapable but keep transmitting; the spec requires
   ceasing until a link toggle or five minutes. Detection exists; suppression
   and the timer do not.
6. **AVTP `tu` bit** (4.3.5.2). Hardcoded false on every PDU. The enabling
   API now exists — `ServoState::is_usable_clock()` — but nothing plumbs it
   from the gPTP port through the daemon into `avb-avtp`.
7. **MVU GET/SET_MEDIA_CLOCK_REFERENCE_INFO** (5.4.4.4/.5). Unhandled.
8. **GET/SET_CLOCK_SOURCE** (5.4.2.15/.16). Constants exist; no dispatch arm,
   so a controller's clock-source read gets NOT_IMPLEMENTED. Now more visible
   because there are genuinely two clock sources to choose between.
9. **Formalise the listener discovery state machine** (5.6.4). The connect
   path works, but there is no explicit per-STREAM_INPUT TK_DISCOVERED /
   TMR_NO_ADP(valid_time) machine. Possibly a documentation and extraction
   job rather than new behaviour.
10. **ADD/REMOVE_AUDIO_MAPPINGS** (5.4.2.27/.28) and **GET_DYNAMIC_INFO**
    (5.4.2.29). Not started.
11. **Base formats** (6.5). A Base-format stream shall advertise all five
    channel counts (1/2/4/6/8) per rate; `talker_model` builds
    `formats: vec![fmt]` — exactly one 8-channel format.

### Small

12. **TMR_NO_TK is 2 s, should be 10 s** (5.5.3.4). Note a test currently
    *pins* 2 s, so the test changes with it.
13. **LOCK_ENTITY ignores the UNLOCK flag and the descriptor scope**
    (5.4.2.2). Exclusivity and the 60 s expiry work; the payload is unparsed.
14. **SET/GET_MAX_TRANSIT_TIME** (5.4.2.30/.31) and **GET_AS_PATH**
    (5.4.2.24). Not started; AS_PATH's constant exists.
15. **`ACQUIRE_ENTITY` should answer NOT_SUPPORTED**, not NOT_IMPLEMENTED
    (5.4.2.1, non-binding "should").
16. **asCapable not explicitly bounded to the 2..5 Pdelay window**
    (4.2.6.2.4). The lower bound is implicit via NRR validity.
17. **`neighborPropDelayThresh`** is a single hardcoded 800 ns with no
    media-type split or fiber-disable path (4.2.6.1.1). Correct for the copper
    I210; incomplete as conformance.
18. **SET_CONFIGURATION has no STREAM_IS_RUNNING guard** (5.4.2.5). Only one
    configuration exists, so low value.

### Unknown / out of this tree

19. **Non-volatile persistence** of sampling rate, stream format and sink
    bindings (5.3.5.1, 5.3.7.x, 5.3.8.x, 5.5.1.2, 5.5.2.4). Not verifiable
    inside `avb-avdecc`; needs a daemon-and-driver persistence audit. Note the
    16R persists its own sink bindings, so we are the worse citizen.
20. **MVU 240 ms responder bound** (5.4.3.4). Dispatch is inline and
    synchronous; there is no latency instrumentation, so it cannot be
    confirmed under load.

## Reachability: what none of this will fix

Established by measurement, not inference, and it bounds what compliance can
buy on this bench:

- The 16R advertises `controller_capabilities = 0x00000000`. It is **not a
  controller**. It will never send BIND_RX or GET_RX_STATE, and it never reads
  `listener_stream_sinks`.
- Our DriverKit dext owns the I210 exclusively, so **no external controller
  can reach the link either**.

So the entire listener-side surface — items 1, 2, 3 included — currently has
**no consumer on this topology**. It is required for compliance and correct to
build, but it will not change the 16R's behaviour, and a `--all-inbound-streams`
run proved that directly: advertising eight sinks instead of one left the 16R's
ADP byte-identical and produced zero inbound listener commands.

The corollary matters for diagnosis: the remaining UC Surface `(offline)`
state is not explained by the open compliance list. A CRF talker now exists,
its descriptor format matches the 16R's Return CRF format, the sink binds and
counts exactly 500 PDUs/s, and selecting/feeding it did not populate the live
AVB Inputs candidate list or turn the UI clock indicator blue. The next
routing experiment is therefore a byte-level comparison of our real ADP/MSRP
traffic (captured with `AVB_TXCAP`) against the known-good Apple capture, then
an inspection of the 16R's saved-vs-live candidate state over UCNet if the wire
advertisements match.

## Deliberately not doing

**Seamless network redundancy (Chapter 8)** is a `may`, not a `shall` (4.2.5),
and the `REDUNDANCY` features_flags bit must stay clear while it is
unimplemented. Claiming it would be a false statement.

## Audit corrections

Two re-audit findings were wrong, which is why their verdicts were re-checked
before acting:

- *"Only `link_up` and `gptp_gm_changed` are fed to GET_COUNTERS."* False —
  the daemon feeds eight interface and clock-domain counters plus per-stream
  input and output values. The genuine gap is the eight unmeasured Table 5.13
  counters, which is item 4.
- *"STREAM_OUTPUT `buffer_length` 2 000 000 violates the floor."* False — the
  2 126 000 ns floor in 5.3.3.4 applies to STREAM_INPUT only.

And one defect the re-audit missed, found while checking its claims:
`StreamInputCounters::block()` was marking all ten counters valid while the
daemon supplied two, so eight mandatory counters were published as a measured
zero. That is a positive claim that the input stream has never glitched, made
without looking — and worse in that one respect than the all-zero block it
replaced, which at least said `counters_valid = 0`. Fixed in this commit by
making the fields `Option<u32>`, so an untracked counter cannot be claimed by
construction.

The original audit documents also had their authoring agents' raw narration
committed into them by mistake (2–24 lines each, before the real report).
Stripped in this commit.
