<!-- Original four-way Milan v1.3 audit, 2026-10-12. SUPERSEDED for planning
purposes by docs/milan-compliance-roadmap.md, which carries the re-audited
verdicts against the code as it stands. Kept because these tables hold the
verbatim clause text and the file:line evidence the roadmap only summarises.
Treat any verdict here as of its date, not as current. -->

## PART 2: Entity model + Discovery

Scope read in full: v13.txt lines 1455–2484 (§5.1–5.3) and 6009–6404 (§5.6). The MCP server `@kirocrew-computer` was declared but not configured; it was not needed for this read-only audit. All other tools were available.

The product advertises exactly **one** AEM entity on the wire: `TalkerEntity` (`/Users/dexter/AVB-Audio/crates/avb-avdecc/src/entity/talker.rs`), constructed and advertised by the daemon at `/Users/dexter/AVB-Audio/crates/avb-daemon/src/stack.rs:2860` and `:2866`. A separate `ListenerEntity` exists in the library (`/Users/dexter/AVB-Audio/crates/avb-avdecc/src/entity/mod.rs:66`) and *does* model sinks and STREAM_INPUT descriptors, but **it is never instantiated or advertised by the daemon** — no `ListenerEntity::new` / `listener_model` call exists in `avb-daemon`. This single fact drives most violations below.

### Verbatim requirement table

| Clause | Requirement (short quote) | Binding? | Our state | Evidence (file:line) |
|---|---|---|---|---|
| 5.3.2 | "each of the descriptors above, except the ENTITY and CONFIGURATION descriptors, shall have one, and only one, parent descriptor" | shall | COMPLIES — descriptor tree is parented via STREAM_PORT_OUTPUT base_cluster/base_map | `talker.rs:400-430` |
| 5.3.3.1 | "The PAAD-AE shall have exactly one ENTITY descriptor" | shall | COMPLIES | `talker.rs:356-371` (ENTITY index 0 only) |
| 5.3.3.1 | "entity_model_id field shall be a valid EUI-64 (neither all zeros nor all ones)" | shall | COMPLIES — computed non-zero `0x02415601||fnv1a32` | `talker.rs:181-199`, test `entity_descriptor_advertises_talker_not_listener` asserts `!=0` |
| 5.3.3.1 | "AEM_SUPPORTED, VENDOR_UNIQUE_SUPPORTED, CLASS_A_SUPPORTED and GPTP_SUPPORTED bits shall be set to 1" (ENTITY descriptor) | shall | COMPLIES — all four set | `talker.rs:258-293`; bit math = 0xC588 incl. these |
| 5.3.3.1 | "AEM_PERSISTENT_ACQUIRE_SUPPORTED, GENERAL_CONTROLLER_IGNORE and ENTITY_NOT_READY bits shall be set to 0" | shall | COMPLIES — none set (0xC588 excludes 1<<13,1<<16,1<<17) | `talker.rs:258-293`, `constants.rs:128-149` |
| 5.3.3.1 | "talker_stream_sources shall be set to the maximum number of Streams the ATDECC Talker is capable of sourcing" | shall | COMPLIES — = stream_count() (8) | `talker.rs:363`, `stack.rs` wiring |
| 5.3.3.1 | "listener_stream_sinks shall be set to the maximum number of Streams the ATDECC Listener is capable of sinking simultaneously" | shall | **VIOLATES** — hardcoded `listener_stream_sinks: 0` while device sinks 8 streams of the 16R | `talker.rs:365`, `talker.rs:313` (ADP) |
| 5.3.3.4 | "A Configuration shall contain at least one STREAM_INPUT or STREAM_OUTPUT descriptor" | shall | COMPLIES (has STREAM_OUTPUT) — but see note: no STREAM_INPUT despite sinking | `talker.rs:308` descriptor_counts |
| 5.3.3.4 | "In a STREAM_INPUT descriptor, the buffer_length field shall … be greater than or equal to 2126000 ns" | shall | **VIOLATES** (two faults): no STREAM_INPUT descriptor exists at all; and the STREAM_OUTPUT buffer_length is `2_000_000` (< 2126000) — if the input descriptor is cloned from this code path it would also be too small | `talker.rs:384` (buffer_length: 2_000_000); no STREAM_INPUT branch in `encode_descriptor` |
| 5.3.3.4 | "In the stream_flags field, the CLASS_A bit shall be set to 1" | shall | COMPLIES for STREAM_OUTPUT; N/A for the absent STREAM_INPUT | `talker.rs:380` |
| 5.3.3.4 | "list of supported formats … shall correctly report the formats supported" | shall | COMPLIES — formats = [current_format] derived from the AAF we transmit | `talker.rs:382`, `talker.rs:154` |
| 5.3.3.4 | "shall always report a format in the current_format field that is one of the supported formats" | shall | COMPLIES | `talker.rs:381-382` |
| 5.3.3.3 | "If at least one Stream Input or Output supports a Milan Audio Stream Format, then this Configuration shall contain at least one AUDIO_UNIT descriptor" | shall | COMPLIES — AUDIO_UNIT x1 | `talker.rs:300`, `talker.rs:373` |
| 5.3.3.3 | "shall always report a sampling rate in the current_sampling_rate field that is one of the supported sampling rates" | shall | COMPLIES | `talker.rs:373-379` |
| 5.3.3.5 | "Each Configuration … shall contain at least one AVB_INTERFACE descriptor" | shall | COMPLIES — AVB_INTERFACE x1 | `talker.rs:307`, `talker.rs:447` |
| 5.3.3.5 | "In the interface_flags field, the GPTP_SUPPORTED and SRP_SUPPORTED bits shall be set to 1" | shall | COMPLIES | `talker.rs:452` |
| 5.3.3.5 | "shall always use the same index, in all Configurations, for the AVB_INTERFACE descriptors that represent the same physical AVB interface" | shall | N/A — single Configuration, single interface (index 0) | `talker.rs:447` |
| 5.3.3.6 | "if the Configuration has at least one Stream Output, then there shall be at least one CLOCK_SOURCE descriptor associated with an internal clock" (type INTERNAL) | shall | COMPLIES — CLOCK_SOURCE x1, type INTERNAL | `talker.rs:470-474` |
| 5.3.3.6 | "For each Stream Input that supports a Milan Audio Stream Format … there shall be exactly one associated CLOCK_SOURCE descriptor with clock_source_type = INPUT_STREAM / clock_source_location_type = STREAM_INPUT" | shall | **VIOLATES** — device sinks Milan audio streams but exposes no STREAM_INPUT and no INPUT_STREAM CLOCK_SOURCE (only the INTERNAL one) | `talker.rs:470-474` (only INTERNAL); contrast `mod.rs:303-320` which the daemon does not use |
| 5.3.3.7 | "If a Configuration has at least one Stream Input which supports a Milan Audio Stream Format, then at least one of the Audio Units shall contain a STREAM_PORT_INPUT descriptor" | shall | **VIOLATES** — sinks audio but exposes no STREAM_PORT_INPUT; AUDIO_UNIT reports `number_of_stream_input_ports: 0` | `talker.rs:376` |
| 5.3.3.7 | "If a Configuration has at least one Stream Output which supports a Milan Audio Stream Format, then … a STREAM_PORT_OUTPUT descriptor" | shall | COMPLIES — one STREAM_PORT_OUTPUT per stream | `talker.rs:302`, `talker.rs:410` |
| 5.3.3.7 | "STREAM_PORT_INPUT descriptors shall not have any attached AUDIO_MAP descriptors" | shall | N/A — no STREAM_PORT_INPUT exists | — |
| 5.3.3.8 | "Each Stream Port Output … shall contain at least one AUDIO_CLUSTER descriptor" + "channel_count field shall be equal to 1" | shall | COMPLIES — one AUDIO_CLUSTER per channel, channel_count=1 | `talker.rs:432-443` |
| 5.3.3.9 | "The Stream Port Input … shall not contain any AUDIO_MAP descriptor" | shall | N/A — no Stream Port Input | — |
| 5.3.3.9 | "there shall not exist more than 1 static mapping … referencing a given channel of a given Stream Output" | shall | COMPLIES — identity map, each (stream,channel) once | `talker.rs:447-460` |
| 5.3.3.10 | "primary 'IDENTIFY' CONTROL shall exist in all Configurations and shall have always the same index" | shall | COMPLIES — CONTROL x1 at fixed index 0 (IDENTIFY_CONTROL_INDEX) | `talker.rs:214`, `talker.rs:462-487` |
| 5.3.3.11 | "A Configuration shall contain at least one CLOCK_DOMAIN descriptor" | shall | COMPLIES — CLOCK_DOMAIN x1 | `talker.rs:306`, `talker.rs:489` |
| 5.3.3.12/.13/.14 | TIMING/PTP_INSTANCE/PTP_PORT "mandatory for PAADs supporting gPTP as a media clock source" | shall (conditional) | NOT APPLICABLE — device does not offer gPTP as media clock source (CLOCK_SOURCE is INTERNAL/none INPUT_STREAM exposed; port is slave-only, never GM) | `talker.rs:450-456`, `talker.rs:470` |
| 5.3.4.1 | "A PAAD-AE cannot be locked by two different controllers at the same time" / LOCK_ENTITY | shall | COMPLIES — ControlState lock refuses second controller | `talker.rs` LOCK_ENTITY dispatch; test `acquire_is_not_implemented_but_lock_works` |
| 5.3.4.2 | "shall maintain a list of registered controllers … able to register at least 16 controllers per AVB Interface" | shall | PARTIAL — registration implemented and capped at MAX_UNSOL_CONTROLLERS; must confirm cap ≥16. Entry fields (seq IDs for AEM+MVU unsol) not fully modeled | `talker.rs` REGISTER_UNSOLICITED path, `control.rs`; test `unsolicited_registration_is_capped` |
| 5.3.5.1 | "an Audio Unit must be running at one of the sampling rates it supports" + "saved in non-volatile memory and restored" | shall | PARTIAL — single rate reported correctly; non-volatile persistence not implemented in this crate (daemon-level, out of scope / UNKNOWN) | `talker.rs:375-379` |
| 5.3.6.3 | "the PAAD-AE shall keep track of the counters in Table 5.1" (LINK_UP, LINK_DOWN, GPTP_GM_CHANGED) | shall | UNKNOWN — GET_COUNTERS returns a zeroed 128-byte block; the daemon is said to own counters but no AVB_INTERFACE counter wiring was found in scope | `talker.rs` GET_COUNTERS (zeroed), `mod.rs:get_counters` |
| 5.3.7.x | STREAM_OUTPUT dynamic state (format persisted, MSRP Talker attr, 2 ms presentation offset, counters) | shall | UNKNOWN/PARTIAL — SRP/streaming lives in daemon; format persistence & presentation-offset persistence not verified in scope | out of file scope (`stack.rs` SRP path at `:1757+`) |
| 5.3.8.x | STREAM_INPUT dynamic state (bound state, binding params, discovered state, probing/settled, counters, non-volatile persistence) | shall | PARTIAL — ACMP listener SMs exist (`acmp_listener.rs`, `mod.rs:ListenerSm`) but the listener entity is not advertised, so from a controller's view the device has no sinks to carry this state | `mod.rs:72-75`; daemon never builds it |
| 5.6.1 | "A talker shall start ADP only when it is ready to accept AECP commands and probe requests" | shall | COMPLIES — advertise() called after model built and responder connected (comment at `stack.rs:2862`) | `stack.rs:2860-2866` |
| 5.6.2 | ADPDU: "AEM_SUPPORTED, VENDOR_UNIQUE_SUPPORTED, CLASS_A_SUPPORTED, GPTP_SUPPORTED and AEM_INTERFACE_INDEX_VALID bits shall be set to 1" | shall | COMPLIES — 0xC588 includes all five | `talker.rs:258-293`, `talker.rs:309` |
| 5.6.2 | ADPDU: "AEM_PERSISTENT_ACQUIRE_SUPPORTED, GENERAL_CONTROLLER_IGNORE, ENTITY_NOT_READY and ACMP_ACQUIRE_WITH_AEM bits shall be set to 0" | shall | COMPLIES — none set | `talker.rs:258-293` |
| 5.6.2 | ADPDU talker_stream_sources = max sources | shall | COMPLIES — stream_count() = 8 | `talker.rs:310` |
| 5.6.2 | ADPDU "listener_stream_sinks shall be set to the maximum number of Streams the ATDECC Listener is capable of sinking" | shall | **VIOLATES** — hardcoded 0 in the ADP builder, device sinks 8 | `talker.rs:313` |
| 5.6.2 | "gptp_grandmaster_id and gptp_domain_number fields shall be populated as per [ATDECC 6.2.2.16/.17]" | shall | COMPLIES — GM = servo GM, falls back to own clockIdentity (never 0) before lock | `talker.rs:303-321`, test `adp_reports_our_own_identity_as_gm_until_gptp_locks` |
| 5.6.2 | "identify_control_index field shall be populated as per [ATDECC 6.2.2.19]" | shall | COMPLIES — = IDENTIFY_CONTROL_INDEX (0), and AEM_IDENTIFY_CONTROL_INDEX_VALID set | `talker.rs:316`, `talker.rs:287` |
| 5.6.2 | "interface_index field shall be populated as per [ATDECC 6.2.2.20]" | shall | COMPLIES — 0, with AEM_INTERFACE_INDEX_VALID set | `talker.rs:317`, `talker.rs:286` |
| 5.6.2 | "valid_time field shall be set to 10 in ENTITY_AVAILABLE messages" | shall | COMPLIES — `VALID_TIME = 10` | `talker.rs:44`, `talker.rs:302` |
| 5.6.2 | "entity_model_id field shall be a valid EUI-64 (neither all zeros nor all ones)" (ADPDU) | shall | COMPLIES | `talker.rs:306` |
| 5.6.3 | "shall implement an independent instance of the Advertise state machine … for each AVB Interface" | shall | PARTIAL — periodic 5 s advertise exists (ADVERTISE_INTERVAL_NS); the full DOWN/WAITING/DELAY SM with LINK/GM/DISCOVER-triggered random delay is not implemented as specified | `mod.rs:34`, `stack.rs:1979` advertise loop; no DELAY/random-delay logic found |
| 5.6.3.1 | On ENTITY_DISCOVER: respond only if entity_id==0 or ==ours, else discard | shall | UNKNOWN — ENTITY_DISCOVER RX handling not located in scope | not found in scope |
| 5.6.3.5.9 | DELAY→WAITING: "Send ENTITY_AVAILABLE; Start TMR_ADVERTISE (5 s)" | shall | PARTIAL — 5 s re-advertise cadence present; random pre-send delay (0–4 s / 0–2 s at startup) to avoid storms not implemented | `mod.rs:34` (5 s), `stack.rs:1979` |
| 5.6.3.5.8/.11 | On SHUTDOWN: "Send an ADP ENTITY_DEPARTING message" | shall | COMPLIES — `departing()` builds ENTITY_DEPARTING | `talker.rs:329-331`, test `departing_differs_from_available_only_in_message_type` |
| available_index semantics (ATDECC 6.2.2, referenced by 5.6.2) | must increase monotonically; repeat/decrease = restart | shall (ATDECC) | COMPLIES — bumped (wrapping_add) on each advertise, not on departing | `talker.rs:323-327`, test `available_index_increases_every_advertisement` |
| 5.6.4 | "shall implement an independent instance of the Listener's discovery state machine … for each Stream Input" | shall | **VIOLATES / UNKNOWN** — a device advertising 0 sinks and no STREAM_INPUT has no sinks over which to run it; the ADP-driven TK_DISCOVERED/NOT_DISCOVERED SM (5.6.4.5) was not found implemented (the listener SMs in `acmp_listener.rs` are the ACMP connection SM, a different machine) | `mod.rs:72-75`, `acmp_listener.rs`; no RCV_ADP_AVAILABLE/TMR_NO_ADP handling found |
| 5.6.4.5.1 | TK_NOT_DISCOVERED/RCV_ADP_AVAILABLE: check gptp GM/domain, save interface_index & available_index, set TMR_NO_ADP from valid_time | shall | UNKNOWN — not located in scope | not found |

### Violations and partials, in priority order

1. **listener_stream_sinks = 0 and listener_capabilities = 0 while the device sinks 8 streams** — *the headline Milan violation.*
   - Spec (5.3.3.1 and 5.6.2): "The listener_stream_sinks shall be set to the maximum number of Streams the ATDECC Listener is capable of sinking simultaneously." A device that consumes eight 16R streams into its input ring is an ATDECC Listener and must advertise it.
   - What we do: `talker.rs:313` ADP `listener_stream_sinks: 0, listener_capabilities: 0`; `talker.rs:365` ENTITY descriptor `listener_stream_sinks: 0`. `TalkerEntity::entity_capabilities` (0xC588) and `talker_capabilities` (IMPLEMENTED|AUDIO_SOURCE) describe a talker only.
   - Change needed: either (a) extend `TalkerEntity` into a combined Talker+Listener entity that advertises `listener_stream_sinks = N`, `listener_capabilities = IMPLEMENTED|AUDIO_SINK`, or (b) wire the existing `ListenerEntity` into the daemon and advertise a single entity that is both. A single entity ID must carry *both* source and sink counts (one ADPDU per entity). The in-tree `ListenerEntity` already produces the right sink fields (`mod.rs:150-152`) and STREAM_INPUT descriptors (`mod.rs:290-310`) — the gap is that nothing merges the two into one advertised AEM.
   - Size: **large** (unifying two half-entities into one coherent descriptor tree + one ADPDU, plus the ACMP listener role, is the core of the device model).

2. **No STREAM_INPUT descriptors (and no STREAM_PORT_INPUT / INPUT_STREAM CLOCK_SOURCE / input AUDIO_CLUSTERs)** — direct consequence of #1.
   - Spec (5.3.3.4, 5.3.3.6, 5.3.3.7): a device sinking a Milan audio stream shall expose a STREAM_INPUT descriptor, a STREAM_PORT_INPUT in an Audio Unit, a CLOCK_SOURCE with `clock_source_type = INPUT_STREAM` / `clock_source_location_type = STREAM_INPUT`, and input AUDIO_CLUSTERs; the AUDIO_UNIT must report `number_of_stream_input_ports > 0`.
   - What we do: `encode_descriptor` has no STREAM_INPUT or STREAM_PORT_INPUT arm; `descriptor_counts` (`talker.rs:296-317`) lists none; `AUDIO_UNIT` reports `number_of_stream_input_ports: 0` (`talker.rs:376`); only an INTERNAL CLOCK_SOURCE is exposed (`talker.rs:470`). A controller walking our AEM sees a pure talker.
   - Change needed: add the input half of the descriptor tree (N STREAM_INPUTs, STREAM_PORT_INPUT with no AUDIO_MAP per 5.3.3.9's note, input AUDIO_CLUSTERs with channel_count=1, an INPUT_STREAM CLOCK_SOURCE pointing at the reference STREAM_INPUT). Keep the module invariant: every advertised count must be answerable.
   - Size: **large** (couples to #1).

3. **STREAM buffer_length = 2_000_000 ns is below the Milan floor of 2_126_000 ns for STREAM_INPUT.**
   - Spec (5.3.3.4): "the buffer_length field … shall be greater than or equal to 2126000 ns (2.126 ms)."
   - What we do: `talker.rs:384` and `mod.rs:299` both set `buffer_length: 2_000_000`. For STREAM_OUTPUT the spec does not impose this floor, so the output value is not itself a violation — but the input descriptor, when added, must not reuse 2_000_000. The listener library code (`mod.rs:299`) already carries the too-small value and would violate the moment it is advertised.
   - Change needed: set STREAM_INPUT `buffer_length` ≥ 2_126_000 (pick a concrete compliant value, e.g. 2_126_000 or the device's real buffer). Fix `mod.rs:299` too.
   - Size: **small**.

4. **Advertise state machine lacks the randomized DELAY state (anti-storm) and the ENTITY_DISCOVER-triggered re-advertise.**
   - Spec (5.6.3.5.2/.3/.4/.9): on startup-with-link-up and on each trigger, start TMR_DELAY (random 0–2 s at startup, 0–4 s otherwise), then send ENTITY_AVAILABLE and start the 5 s TMR_ADVERTISE; respond to RCV_ADP_DISCOVER by going through DELAY.
   - What we do: a fixed 5 s re-advertise (`mod.rs:34`, loop at `stack.rs:1979`) and an immediate advertise at startup (`stack.rs:2866`). No random delay, no DOWN/WAITING/DELAY states, and ENTITY_DISCOVER RX handling was not found in scope.
   - Change needed: implement the three-state Advertise SM per interface with the random TMR_DELAY and RCV_ADP_DISCOVER handling; keep the 5 s TMR_ADVERTISE.
   - Size: **medium**.

5. **Listener's discovery state machine (5.6.4) absent.**
   - Spec: "shall implement an independent instance of the Listener's discovery state machine … for each Stream Input." It consumes ADP ENTITY_AVAILABLE/DEPARTING from bound talkers, validates gptp GM/domain, tracks `available_index`/`interface_index`, and arms TMR_NO_ADP from `valid_time` — feeding the connection SM EVT_TK_DISCOVERED/DEPARTED.
   - What we do: `acmp_listener.rs` implements the ACMP *connection* SM (PRB/SETTLED states referenced by 5.3.8.6), but no ADP-driven discovery SM was found. Blocked on #1/#2 anyway (no advertised sinks).
   - Change needed: add the TK_DISCOVERED/TK_NOT_DISCOVERED SM per STREAM_INPUT, wired to incoming ADP, triggering the connection SM.
   - Size: **medium** (on top of #1).

6. **PARTIAL: registered-controller list and AVB_INTERFACE diagnostic counters.**
   - 5.3.4.2 requires ≥16 controllers/interface and per-entry AEM+MVU unsolicited sequence IDs; the cap (`MAX_UNSOL_CONTROLLERS`) is enforced but I could not confirm from scope that it is ≥16 nor that both sequence counters are tracked (`control.rs` not read in full). 5.3.6.3 mandates LINK_UP/LINK_DOWN/GPTP_GM_CHANGED counters; GET_COUNTERS returns a zeroed block (`talker.rs` GET_COUNTERS, `mod.rs:get_counters`) and no counter source was found in scope. Mark these UNKNOWN pending `control.rs` and the daemon counter wiring.
   - Size: small–medium.

### Notes

- **The Annex (normative) STREAM_INPUT/STREAM_OUTPUT descriptor extensions are OUTSIDE my range.** §5.3.3.4 references them as "the extension of the STREAM_INPUT/OUTPUT descriptors as defined in Section C … shall use it for the Streams that are part of the redundant pair" (v13.txt ≈ line 1990). Annex C itself is not within lines 1455–2484 or 6009–6404, so I did not read it. The rule *in my range* is conditional: the extension is only *required* for streams in a redundant pair. If this device is non-redundant (single I210, single AVB_INTERFACE — consistent with `talker.rs:447` and the slave-only port), the extension is optional ("may use … and shall use it for … redundant pair"). A separate audit part covering Annex C should confirm the exact extension layout and whether redundancy applies.

- **Two half-entities, one wire.** The single most important architectural fact for the next engineer: `ListenerEntity` (`mod.rs`) and `TalkerEntity` (`talker.rs`) are *separate* structs with *separate* entity IDs derived the same way (`Eui64::from_mac(our_mac)`), and **only the talker is advertised** (`stack.rs:2860-2866`). Milan expects *one* PAAD-AE exposing one entity model that declares both `talker_stream_sources` and `listener_stream_sinks`. The fix for violations #1, #2, #5 is fundamentally "make the advertised entity be both talker and listener," not a field tweak. Advertising two different entities on one MAC would also be wrong (duplicate entity_id / two ADPDUs).

- **The 0xC588 capability word is correct and matches the task's stated value**, but the comment at `talker.rs:270-283` is stale: it says "its caps word is 0xc588 against our 0x8588" implying we omit the IDENTIFY bit, yet the code *does* OR in `AEM_IDENTIFY_CONTROL_INDEX_VALID` (1<<14), making the actual value 0xC588. The task's observed `entity_capabilities=0x0000c588` confirms the code, not the comment. No action beyond correcting the misleading comment.

- **`talker_capabilities=0x4001`** (task) = `IMPLEMENTED (1<<0)` | `AUDIO_SOURCE (1<<14)` = 0x4001 ✓ (`talker.rs:296-297`, `constants.rs:151-161`). Compliant for the talker role; the listener-role bits are simply absent because the listener role is unadvertised.

- **GET_STREAM_INFO is deliberately NOT_IMPLEMENTED** (`talker.rs:~560`, documented). Not in my normative scope (§5.4/§5.5 territory), so no verdict here — flagged only so a later part knows it is an intentional refusal, not an oversight.

- **Persistence requirements** (§5.3.7.1, 5.3.8.2/.3/.7, 5.3.9/.10/.11 "shall be saved in a non-volatile memory and restored after a power cycle") are entirely outside these two crates' responsibility as read; they belong to daemon/driver state and were not verifiable in scope → UNKNOWN, needs a dedicated persistence audit.