<!-- Original four-way Milan v1.3 audit, 2026-10-12. SUPERSEDED for planning
purposes by docs/milan-compliance-roadmap.md, which carries the re-audited
verdicts against the code as it stands. Kept because these tables hold the
verbatim clause text and the file:line evidence the roadmap only summarises.
Treat any verdict here as of its date, not as current. -->

## PART 3: Control (AECP / MVU)

Scope read in full: Milan v1.3 §5.4.1 through §5.5.1.3 (lines 2485–4149). The product claims to be *both* a Talker and Listener PAAD, but on the live wire (`stack.rs:2860`) **only a `TalkerEntity` is instantiated and served**; no `ListenerEntity` responds to AECP/MVU on the wire. The `ListenerEntity` responder in `entity/mod.rs` exists and is unit-tested but is never wired to the socket. This single fact drives most VIOLATES rows below.

### Verbatim requirement table

| Clause | Requirement (short quote) | Binding? | Our state | Evidence (file:line) |
|---|---|---|---|---|
| 5.4.1 | "A PAAD-AE shall implement the AECP common format as described in [ATDECC 9.2.1]" | shall | PARTIAL — AEM/MVU PDU codec present and well-formed, but no response-length relaxation handling and no large variable-length responses implemented | `avb-avdecc/src/aecp.rs:36-88` (AemPdu codec), `:104-166` (MvuPdu codec) |
| 5.4.1 | "A PAAD-AE shall implement AECP AEM as described in [ATDECC 9.3]" | shall | PARTIAL — a subset of AEM commands answered; several mandatory ones refused (see inventory) | `avb-avdecc/src/entity/talker.rs:398-444`; `entity/mod.rs:179-234` |
| 5.4.1 | "A PAAD-AE shall implement the AECP vendor unique extension defined in 5.4.3" | shall | PARTIAL — MVU framing implemented and dispatched; only GET_MILAN_INFO answered, all other MVU → NOT_IMPLEMENTED | `talker.rs:363-388`; `stack.rs:3511-3534` |
| 5.4.2.1 | "The PAAD-AE shall implement the ACQUIRE_ENTITY command" / "shall not reply SUCCESS … should reply NOT_SUPPORTED" | shall / should | PARTIAL — we answer, but with `NOT_IMPLEMENTED` (0x01), not the recommended `NOT_SUPPORTED` (0x0B) | `talker.rs:423` (`ACQUIRE_ENTITY => NOT_IMPLEMENTED`); `entity/mod.rs:186-190` |
| 5.4.2.2 | "The PAAD-AE shall implement the LOCK_ENTITY command" | shall | PARTIAL — lock/unlock state machine + 60 s auto-release implemented, but the UNLOCK flag is not parsed and locking is not restricted to the ENTITY descriptor | `entity/control.rs:51-70`; `talker.rs:424` |
| 5.4.2.2 | "The PAAD-AE shall support the UNLOCK flag" | shall | VIOLATES — LOCK_ENTITY payload (and its flags/descriptor fields) is never decoded; `control.lock()` ignores the flag so UNLOCK cannot release or query | `control.rs:51-70`; `talker.rs:424` (payload echoed, flags unread) |
| 5.4.2.2 | "shall not allow locking another descriptor than the ENTITY descriptor (NOT_SUPPORTED …)" | shall | VIOLATES — descriptor_type/index in the LOCK_ENTITY payload are not checked | `talker.rs:424` |
| 5.4.2.3 | "shall implement the ENTITY_AVAILABLE command" | shall | COMPLIES — answered SUCCESS | `talker.rs:425-427` |
| 5.4.2.4 | "shall implement the READ_DESCRIPTOR command" | shall | PARTIAL — implemented for the descriptors we expose; see §9 notes | `talker.rs:428`, `:446-460`; `entity/mod.rs:215` |
| 5.4.2.5 | "shall implement SET_CONFIGURATION" | shall | PARTIAL — accepts cfg 0, else BAD_ARGUMENTS; does NOT return STREAM_IS_RUNNING when a stream is bound/streaming, and no lock-ownership check | `talker.rs:429-436` |
| 5.4.2.5 | "shall not accept SET_CONFIGURATION if a Stream Input is bound or Stream Output is streaming … STREAM_IS_RUNNING shall be returned" | shall | VIOLATES — no streaming-state guard on SET_CONFIGURATION | `talker.rs:429-436` |
| 5.4.2.7 | "shall implement SET_STREAM_FORMAT … not accept if bound/streaming → STREAM_IS_RUNNING" | shall | PARTIAL — talker refuses with STREAM_IS_RUNNING while connected, else BAD_ARGUMENTS (never SUCCESS, as talker has one fixed format); listener accepts a known format but is not on the wire; the "all mapped channels still exist in new format" check is absent | `talker.rs:472-481`; `entity/mod.rs:225-230,:258-284` |
| 5.4.2.8 | "shall implement GET_STREAM_FORMAT" | shall | PARTIAL — talker implements (`get_stream_format`); listener path not wired | `talker.rs:437` |
| 5.4.2.10 | "shall implement GET_STREAM_INFO … set flags per Table 5.8/5.9" | shall | PARTIAL — talker `get_stream_info` present; flag bit positions transcribed from 16R capture (see §9 ambiguity) | `talker.rs:438`; `milan.rs:95-134` |
| 5.4.2.11 | "shall implement SET_NAME … support setting all names" | shall | VIOLATES — SET_NAME (0x0010) not in dispatch → NOT_IMPLEMENTED | `talker.rs:443` (fallthrough); `constants.rs:70` (code defined, unused) |
| 5.4.2.12 | "shall implement GET_NAME … support getting all names" | shall | VIOLATES — GET_NAME (0x0011) not in dispatch → NOT_IMPLEMENTED | `talker.rs:443`; `constants.rs:71` |
| 5.4.2.13 | "shall implement SET_SAMPLING_RATE" | shall | PARTIAL — talker refuses (BAD_ARGUMENTS/STREAM_IS_RUNNING); listener accepts matching rate; no mapping-rate check | `talker.rs:472-481`; `entity/mod.rs:236-247` |
| 5.4.2.14 | "shall implement GET_SAMPLING_RATE" | shall | COMPLIES — returns current rate | `talker.rs:439-442`; `entity/mod.rs:231-237` |
| 5.4.2.15 | "shall implement SET_CLOCK_SOURCE" | shall | VIOLATES — SET_CLOCK_SOURCE (0x0016) not in dispatch → NOT_IMPLEMENTED | `talker.rs:443`; `constants.rs:74` |
| 5.4.2.16 | "shall implement GET_CLOCK_SOURCE" | shall | VIOLATES — GET_CLOCK_SOURCE (0x0017) not in dispatch → NOT_IMPLEMENTED | `talker.rs:443`; `constants.rs:75` |
| 5.4.2.17 | "shall implement SET_CONTROL for the Identify Control" | shall | VIOLATES — SET_CONTROL (0x0017? no — 0x0018) not dispatched; command code not even defined | `talker.rs:443`; `constants.rs:63-86` (no SET_CONTROL const) |
| 5.4.2.18 | "shall implement GET_CONTROL for the Identify Control" | shall | VIOLATES — GET_CONTROL not dispatched / not defined | `talker.rs:443`; `constants.rs:63-86` |
| 5.4.2.19 | "shall implement START_STREAMING for each Stream Input … not support for a Stream Output (NOT_SUPPORTED)" | shall | VIOLATES — START_STREAMING not dispatched (no code, no handler) → generic NOT_IMPLEMENTED, and listener not on wire | `talker.rs:443`; repo grep: no START_STREAMING |
| 5.4.2.20 | "shall implement STOP_STREAMING for each Stream Input … Stream Output NOT_SUPPORTED" | shall | VIOLATES — not dispatched / not defined | `talker.rs:443`; repo grep: no STOP_STREAMING |
| 5.4.2.21 | "shall implement REGISTER_UNSOLICITED_NOTIFICATION" | shall | PARTIAL — registration list (≤16, NO_RESOURCES) implemented, but no actual unsolicited notifications are ever sent (see 5.4.5), and no CONTROLLER_AVAILABLE eviction | `control.rs:72-88`; `talker.rs:482-484` |
| 5.4.2.21 | "If … not enough resources … it shall return the NO_RESOURCES error code" | shall | COMPLIES | `control.rs:80-84`; test `entity/mod.rs:register_unsolicited_limit` |
| 5.4.2.22 | "shall implement DEREGISTER_UNSOLICITED_NOTIFICATION" | shall | PARTIAL — deregister implemented (always SUCCESS); never triggers the auto-deregister unsolicited notification | `control.rs:86-89`; `talker.rs:485-487` |
| 5.4.2.23 | "shall implement GET_AVB_INFO" | shall | PARTIAL — talker `get_avb_info` present | `talker.rs:444` |
| 5.4.2.24 | "shall implement GET_AS_PATH" | shall | VIOLATES — GET_AS_PATH (0x0028) not dispatched → NOT_IMPLEMENTED | `talker.rs:443`; `constants.rs` (no GET_AS_PATH const) |
| 5.4.2.25 | "shall implement GET_COUNTERS … for each AVB Interface, Clock Domain, Stream Input and Stream Output … mandatory counters Tables 5.10/5.12/5.13/5.14" | shall | PARTIAL — returns SUCCESS with an all-zero 128-byte block and `counters_valid`=0; no mandatory counters actually reported | `talker.rs:488-493`; `entity/mod.rs:386-409` |
| 5.4.2.26 | "shall implement GET_AUDIO_MAP …" | shall | PARTIAL — talker `get_audio_map` present; partitioning/number_of_maps rules unverified | `talker.rs:445` |
| 5.4.2.27 | "shall implement ADD_AUDIO_MAPPINGS …" | shall | VIOLATES — not dispatched → NOT_IMPLEMENTED | `talker.rs:443`; repo grep: none |
| 5.4.2.28 | "shall implement REMOVE_AUDIO_MAPPINGS …" | shall | VIOLATES — not dispatched → NOT_IMPLEMENTED | `talker.rs:443`; repo grep: none |
| 5.4.2.29 | "shall implement GET_DYNAMIC_INFO" | shall | VIOLATES — not dispatched / not defined | `talker.rs:443`; repo grep: none |
| 5.4.2.30 | "shall implement SET_MAX_TRANSIT_TIME on STREAM_OUTPUT …" | shall | VIOLATES — not dispatched / not defined | `talker.rs:443`; repo grep: none |
| 5.4.2.31 | "shall implement GET_MAX_TRANSIT_TIME on STREAM_OUTPUT" | shall | VIOLATES — not dispatched / not defined | `talker.rs:443`; repo grep: none |
| 5.4.3.2.1 | "protocol_id … 00-1B-C5-0A-C1-00" | shall (format) | COMPLIES | `milan.rs:9` (`MVU_PROTOCOL_ID`); `aecp.rs:127-130` (decode guard) |
| 5.4.3.2.2 | "u field is set to zero for all commands and direct responses … one if the response is the result of a notification" | shall | PARTIAL — command_type masked with 0x7FFF so `u` is implicitly 0 on direct responses (correct); but no notification path exists to ever set u=1 | `aecp.rs:131,:152-159` |
| 5.4.3.2 | "sequence_id … shall be independent from AECP AEM sequence_id" | shall | NOT APPLICABLE on responder — our MVU responses echo the command's sequence_id (correct for a responder); independence applies to a sender of MVU commands, which only the controller-walk does and it reuses the AEM engine seq | `talker.rs:364` (echo); `aecp.rs:242-250` |
| 5.4.3.3 | MVU status codes table (SUCCESS=0, NOT_IMPLEMENTED=1, NO_SUCH_DESCRIPTOR=2, ENTITY_LOCKED=3, BAD_ARGUMENTS=7 …) | shall | PARTIAL — reused AECP status constants happen to match for these values; MVU-specific `ENTITY_MISBEHAVING=10` and `PAYLOAD_TOO_SHORT=13` not used | `constants.rs:38-55`; `talker.rs:363-388` |
| 5.4.3.4 | "All MVU messages shall have a 250 ms timeout" | shall | COMPLIES (sender side) — `TIMEOUT_NS = 250 ms` | `aecp.rs:171` |
| 5.4.3.4 | "Entities shall respond to all MVU commands within 240 ms" | shall | UNKNOWN — responder is synchronous/in-loop; no measured latency bound, cannot confirm 240 ms under load | `stack.rs:3511-3534` |
| 5.4.4.1 | "shall implement GET_MILAN_INFO" | shall | COMPLIES | `talker.rs:371-384`; `milan.rs:46-57` |
| 5.4.4.1 | "protocol_version … value of this field is 1" | shall | COMPLIES | `milan.rs:22` (`MILAN_PROTOCOL_VERSION=1`); `talker.rs:373` |
| 5.4.4.1 | "certification_version … set to 0 if the PAAD-AE has not passed any Milan certification" | shall | COMPLIES — we set 0, which is correct and honest (we are uncertified) | `talker.rs:376-383`; `milan.rs` test `our_milan_info_claims_no_certification` |
| 5.4.4.1 | "specification_version shall be set to the version … supports … last two numbers set to 0" | shall | VIOLATES — our GET_MILAN_INFO response omits the `specification_version` field entirely (26-byte payload modeled on a v1.1 device; field is at offset 44 per Fig 5.6) | `milan.rs:40-61` (encode writes only reserved/proto/features/cert, 26 bytes) |
| 5.4.4.1 | "MVU_BINDING flag … The ATDECC Entity supports … binding and unbinding" | shall (if claimed) | NOT APPLICABLE / consistent — we set features_flags=0, so we correctly do NOT claim MVU_BINDING | `talker.rs:374-375`; `milan.rs:48-49` |
| 5.4.4.4 | "shall implement SET_MEDIA_CLOCK_REFERENCE_INFO" | shall | VIOLATES — MVU command 0x0003 not handled → NOT_IMPLEMENTED | `talker.rs:386`; `milan.rs:11-13` (only GET_MILAN_INFO defined) |
| 5.4.4.5 | "shall implement GET_MEDIA_CLOCK_REFERENCE_INFO" | shall | VIOLATES — MVU 0x0004 not handled → NOT_IMPLEMENTED | `talker.rs:386`; `milan.rs:11-13` |
| 5.4.4.6 | "it shall implement BIND_STREAM" (conditioned on MVU_BINDING) | conditional shall | NOT APPLICABLE *as a Talker* (binding is a Listener function; we don't advertise MVU_BINDING) — but **VIOLATES for the Listener role we claim**: listener never serves MVU, and BIND_STREAM (0x0005) is unimplemented everywhere | `talker.rs:386`; `stack.rs:2860` (only TalkerEntity on wire); repo grep: no BIND_STREAM |
| 5.4.4.7 | "it shall implement UNBIND_STREAM" (conditioned) | conditional shall | Same as 5.4.4.6 — VIOLATES for the claimed Listener role | `talker.rs:386`; repo grep: no UNBIND_STREAM |
| 5.4.4.8 | "The PAAD-AE shall implement GET_STREAM_INPUT_INFO_EX" (unconditional) | shall | VIOLATES — MVU 0x0007 unimplemented; and a Listener-side command, but no listener MVU responder exists on the wire | `talker.rs:386`; repo grep: no STREAM_INPUT_INFO_EX |
| 5.4.5.1 | "shall implement AEM unsolicited notifications … one message to each registered controller except the requesting one … increments Sequence ID" | shall | VIOLATES — registrations stored but NO unsolicited notification is ever emitted; no per-controller sequence counter | `control.rs:72-95` (stores list only); no emit path in `talker.rs`/`stack.rs` |
| 5.4.5.2 | "shall implement MVU unsolicited notifications … Sequence ID independent from AEM" | shall | VIOLATES — not implemented | repo grep: no MVU unsolicited emit |
| 5.4.5.3 | "Each time a PAAD-AE sends a successful AEM/MVU response that modifies state … shall immediately send an unsolicited notification" | shall | VIOLATES — no state-change notification path | n/a (feature absent) |
| 5.4.5.4 | "shall monitor the availability of each registered controller … 30–60 s timer … CONTROLLER_AVAILABLE … remove on no-reply" | shall | VIOLATES — no monitor timer, no CONTROLLER_AVAILABLE probing of registered controllers | `control.rs` (no timer); `talker.rs:425` (answers inbound CONTROLLER_AVAILABLE only) |
| 5.4.5.5 | "it should implement the Identification Notification" | should | NOT APPLICABLE / non-binding — no front-panel button; recommendation only | n/a |

### MVU and AEM command inventory

**MVU commands** (Milan Table 5.15). "Mandatory?" per §5.4.4 (BIND/UNBIND conditional on MVU_BINDING; we don't claim it, but we claim the Listener role those commands require).

| Command | Code | Dir (to us) | Mandatory? | Implemented? | Evidence |
|---|---|---|---|---|---|
| GET_MILAN_INFO | 0x0000 | cmd→resp | Yes | **Yes** | `talker.rs:371-384` |
| SET_SYSTEM_UNIQUE_ID | 0x0001 | cmd→resp | Optional | No (→NOT_IMPLEMENTED) | `talker.rs:386` |
| GET_SYSTEM_UNIQUE_ID | 0x0002 | cmd→resp | Optional | No | `talker.rs:386` |
| SET_MEDIA_CLOCK_REFERENCE_INFO | 0x0003 | cmd→resp | **Yes** | **No** | `talker.rs:386` |
| GET_MEDIA_CLOCK_REFERENCE_INFO | 0x0004 | cmd→resp | **Yes** | **No** | `talker.rs:386` |
| BIND_STREAM | 0x0005 | cmd→resp | Cond. (Listener) | **No** | repo grep: none |
| UNBIND_STREAM | 0x0006 | cmd→resp | Cond. (Listener) | **No** | repo grep: none |
| GET_STREAM_INPUT_INFO_EX | 0x0007 | cmd→resp | **Yes** | **No** | repo grep: none |

Only `mvu_cmd::GET_MILAN_INFO` is even *defined* as a constant (`milan.rs:11-13`); the other seven codes do not exist in the codebase.

**AEM commands** (Milan §5.4.2; "answered" = non-fallthrough handler). Codes from `constants.rs:63-86` / IEEE Table 7.126.

| Command | Code | Mandatory (Milan)? | We answer? | Evidence |
|---|---|---|---|---|
| ACQUIRE_ENTITY | 0x0000 | shall-not-SUCCEED | Yes, NOT_IMPLEMENTED (spec recommends NOT_SUPPORTED) | `talker.rs:423` |
| LOCK_ENTITY | 0x0001 | Yes | Partial (no UNLOCK flag, no descriptor check) | `talker.rs:424` |
| ENTITY_AVAILABLE | 0x0002 | Yes | Yes (SUCCESS) | `talker.rs:425` |
| CONTROLLER_AVAILABLE | 0x0003 | (inbound) | Yes (SUCCESS) | `talker.rs:425` |
| READ_DESCRIPTOR | 0x0004 | Yes | Yes | `talker.rs:428,446` |
| SET_CONFIGURATION | 0x0006 | Yes | Partial (no STREAM_IS_RUNNING guard) | `talker.rs:429` |
| GET_CONFIGURATION | 0x0007 | Yes | Yes | `talker.rs:429` |
| SET_STREAM_FORMAT | 0x0008 | Yes | Partial | `talker.rs:472` |
| GET_STREAM_FORMAT | 0x0009 | Yes | Yes (talker) | `talker.rs:437` |
| SET_STREAM_INFO | 0x000E | Optional | Refused (NOT_IMPLEMENTED) — OK | `entity/mod.rs:238` |
| GET_STREAM_INFO | 0x000F | Yes | Yes (talker) | `talker.rs:438` |
| SET_NAME | 0x0010 | **Yes** | **No** (NOT_IMPLEMENTED) | `talker.rs:443` |
| GET_NAME | 0x0011 | **Yes** | **No** | `talker.rs:443` |
| SET_SAMPLING_RATE | 0x0012 | Yes | Partial | `talker.rs:472` |
| GET_SAMPLING_RATE | 0x0013 | Yes | Yes | `talker.rs:439` |
| SET_CLOCK_SOURCE | 0x0016 | **Yes** | **No** | `talker.rs:443` |
| GET_CLOCK_SOURCE | 0x0017 | **Yes** | **No** | `talker.rs:443` |
| SET_CONTROL | 0x0018 | **Yes** (Identify) | **No** (code undefined) | `constants.rs` none |
| GET_CONTROL | 0x0019 | **Yes** (Identify) | **No** | `constants.rs` none |
| START_STREAMING | 0x0022 | **Yes** (Stream Input) | **No** | repo grep none |
| STOP_STREAMING | 0x0023 | **Yes** (Stream Input) | **No** | repo grep none |
| REGISTER_UNSOLICITED_NOTIFICATION | 0x0024 | Yes | Partial (list only, no emit) | `talker.rs:482` |
| DEREGISTER_UNSOLICITED_NOTIFICATION | 0x0025 | Yes | Partial | `talker.rs:485` |
| GET_AVB_INFO | 0x0027 | Yes | Yes (talker) | `talker.rs:444` |
| GET_AS_PATH | 0x0028 | **Yes** | **No** (code undefined) | `constants.rs` none |
| GET_COUNTERS | 0x0029 | Yes | Partial (zeroed block) | `talker.rs:488` |
| GET_AUDIO_MAP | 0x002B | Yes | Yes (talker) | `talker.rs:445` |
| ADD_AUDIO_MAPPINGS | 0x002C | **Yes** | **No** | repo grep none |
| REMOVE_AUDIO_MAPPINGS | 0x002D | **Yes** | **No** | repo grep none |
| GET_DYNAMIC_INFO | 0x004B | **Yes** | **No** | repo grep none |
| SET_MAX_TRANSIT_TIME | 0x004E | **Yes** (Stream Output) | **No** | repo grep none |
| GET_MAX_TRANSIT_TIME | 0x004F | **Yes** (Stream Output) | **No** | repo grep none |

(Codes for the unimplemented AEM commands are from IEEE 1722.1-2021 Table 7.126; those constants are absent from `constants.rs:63-86`, which stops at GET_AUDIO_MAP=0x002B.)

### Wire formats for anything we must implement

All MVU PDUs share the common AECP VENDOR_UNIQUE header (Fig 5.4). Offsets are from the start of the AVTP/AECP payload (`payload[0]` = subtype), big-endian.

**Common MVU header (offsets 0–29), already implemented in `aecp.rs:134-166`:**

| Off | Width | Field |
|---|---|---|
| 0 | 1 B | subtype = 0xFB (AECP) |
| 1 | 1 B | sv(1) \| version(3) \| message_type(4): cmd=0x06, resp=0x07 |
| 2 | 11 bits | status (top 5 bits of byte 2) \| control_data_length (11 bits) |
| 3 | — | low byte of control_data_length |
| 4 | 8 B | target_entity_id |
| 12 | 8 B | controller_entity_id |
| 20 | 2 B | sequence_id |
| 22 | 6 B | protocol_id = 00-1B-C5-0A-C1-00 |
| 28 | 2 bytes | u(1 bit) \| command_type(15 bits) |
| 30 | var | command_specific_data |

**BIND_STREAM (§5.4.4.6, Fig 5.10) — command_specific_data starts at payload offset 30:**

| Off (from PDU start) | Off (in cmd-specific) | Width | Field | Notes |
|---|---|---|---|---|
| 30 | 0 | 2 B | flags | Table 5.19 (below) |
| 32 | 2 | 2 B | descriptor_type | shall = STREAM_INPUT (0x0005) |
| 34 | 4 | 2 B | descriptor_index | = ACMP listener_unique_id |
| 36 | 6 | 8 B | talker_entity_id | |
| 44 | 14 | 2 B | talker_stream_index | = ACMP talker_unique_id |
| 46 | 16 | 2 B | reserved | set 0 by sender, ignored by receiver |

Response: identical layout; `descriptor_type, descriptor_index, talker_entity_id, talker_stream_index, flags` echoed from command. Errors: `controller_entity_id` ≠ lock owner → ENTITY_LOCKED; descriptor_index absent → NO_SUCH_DESCRIPTOR; descriptor_type ≠ STREAM_INPUT → BAD_ARGUMENTS. On success → MVU unsolicited notification.

**Table 5.19 — BIND_STREAM flags (16-bit field):**

| Bit # | Value | Name | Meaning |
|---|---|---|---|
| 15 (LSB of the 16-bit field, i.e. 0x0001) | 0x0001 | STREAMING_WAIT | 1 ⇒ set STREAM_INPUT to **stopped**; 0 ⇒ **started** |
| 0–14 | — | reserved | future use |

(Note the spec's bit-numbering convention: "Bit #15" with "Field value 0x00000001" means the low bit of the flags word.)

**UNBIND_STREAM (§5.4.4.7, Fig 5.11) — command_specific_data at offset 30:**

| Off (PDU) | Off (cmd-specific) | Width | Field | Notes |
|---|---|---|---|---|
| 30 | 0 | 2 B | reserved | set 0, ignored |
| 32 | 2 | 2 B | descriptor_type | shall = STREAM_INPUT |
| 34 | 4 | 2 B | descriptor_index | = ACMP listener_unique_id |

Response echoes descriptor_type + descriptor_index. Error: controller_entity_id ≠ lock owner → ENTITY_LOCKED. On success → MVU unsolicited notification.

**GET_STREAM_INPUT_INFO_EX (§5.4.4.8):** command uses the UNBIND_STREAM layout (reserved(2) + descriptor_type(2) + descriptor_index(2)). Response (Fig 5.12), command_specific_data at offset 30:

| Off (PDU) | Off (cmd-specific) | Width | Field |
|---|---|---|---|
| 30 | 0 | 2 B | descriptor_type (= STREAM_INPUT) |
| 32 | 2 | 2 B | descriptor_index |
| 34 | 4 | 8 B | talker_entity_id |
| 42 | 12 | 2 B | talker_unique_id |
| 44 | 14 | 1 B | pbsta (probing status, §5.3.8.4 — values in `milan.rs:67-72`: DISABLED=0/PASSIVE=1/ACTIVE=2/COMPLETED=3) |
| 45 | 15 | 1 B | acmpsta (ACMP status) |
| 46 | 16 | 2 B | reserved |

Errors: descriptor absent → NO_SUCH_DESCRIPTOR; type ≠ STREAM_INPUT → BAD_ARGUMENTS.

**GET_MILAN_INFO response (§5.4.4.1, Fig 5.6)** — the field we're missing is `specification_version`:

| Off (PDU) | Off (cmd-specific) | Width | Field | Our encoder |
|---|---|---|---|---|
| 30 | 0 | 2 B | reserved | `milan.rs:50` ✓ |
| 32 | 2 | 4 B | protocol_version (=1) | `milan.rs:51` ✓ |
| 36 | 6 | 4 B | features_flags (Table 5.17) | `milan.rs:52` ✓ |
| 40 | 10 | 4 B | certification_version | `milan.rs:53` ✓ |
| 44 | 14 | 4 B | **specification_version** (major.minor.0.0) | **MISSING** — encoder zero-pads to 26 B (`milan.rs:54`) instead of writing this field |

Our encoder (`milan.rs:46-57`) models a 26-byte payload = reserved(2)+proto(4)+feat(4)+cert(4)+12 zero-pad, which was anchored on a *v1.1-era* 16R capture. Per Fig 5.6 the proper payload is 16 bytes of significant data (ending at offset 46), with `specification_version` as a real field, not padding. For a v1.3 implementation this should read `0x01030000` (1.3.0.0), per the §5.4.4.1 note.

**Table 5.17 — GET_MILAN_INFO features_flags** (for reference; we send 0):
REDUNDANCY=0x1, TALKER_DYNAMIC_MAPPINGS_WHILE_RUNNING=0x2, MVU_BINDING=0x4, TALKER_SIGNAL_PRESENCE=0x8.

### Violations and partials, in priority order

1. **Listener role has no on-wire responder at all.** Spec: a Listener PAAD-AE shall serve BIND_STREAM (5.4.4.6), UNBIND_STREAM (5.4.4.7), GET_STREAM_INPUT_INFO_EX (5.4.4.8), START/STOP_STREAMING (5.4.2.19/20). We do: `stack.rs:2860` instantiates only `TalkerEntity`; `ListenerEntity::handle_aecp` is never called on received frames. **Change:** instantiate the `ListenerEntity` (or a combined entity) in the live loop, route AECP commands whose `target_entity_id` is our listener id to it, and add MVU dispatch for the binding commands. **Size: large.** This is the single highest-impact gap — without it the product cannot act as a Milan Listener despite advertising listener sinks.

2. **GET_MILAN_INFO omits `specification_version` (5.4.4.1).** Spec: "The specification_version field shall be set to the version number of the Milan specification that the PAAD-AE supports … last two numbers shall be set to 0." We do: emit 26-byte payload with the field position zero-padded. **Change:** in `milan.rs:46-57`, replace the `resize(26,0)` tail with an explicit `specification_version: u32` field (e.g. `0x01030000`), making a 16-byte significant payload per Fig 5.6; add the field to `MilanInfo` + decode. **Size: small.** High priority because this is the one MVU command controllers always send to classify us, and a short/mis-shaped response can get us mis-identified.

3. **BIND/UNBIND/GET_STREAM_INPUT_INFO_EX + MVU unsolicited notifications unimplemented (5.4.4.6–8, 5.4.5.2/3).** Spec: GET_STREAM_INPUT_INFO_EX is *unconditionally* mandatory; BIND/UNBIND are mandatory for the binding we need as a Listener and are the MVU equivalent of ACMP connect. We do: nothing; `handle_mvu` returns NOT_IMPLEMENTED for all but GET_MILAN_INFO. **Change:** define `mvu_cmd` constants 0x0005–0x0007, implement handlers wired to the ACMP listener SM (`acmp_listener.rs`), and set `features_flags |= MVU_BINDING` once BIND/UNBIND work. Add the per-controller unsolicited-notification emitter. **Size: large.**

4. **Mandatory AEM commands silently refused: GET_NAME/SET_NAME (5.4.2.11/12), GET_CLOCK_SOURCE/SET_CLOCK_SOURCE (5.4.2.15/16), GET_CONTROL/SET_CONTROL for Identify (5.4.2.17/18), GET_AS_PATH (5.4.2.24), START/STOP_STREAMING (5.4.2.19/20), ADD/REMOVE_AUDIO_MAPPINGS (5.4.2.27/28), GET_DYNAMIC_INFO (5.4.2.29), SET/GET_MAX_TRANSIT_TIME (5.4.2.30/31).** We do: fallthrough `_ => NOT_IMPLEMENTED` (`talker.rs:443`). **Change:** add the command constants to `constants.rs` and dispatch arms in both entities. SET_CONTROL/GET_CONTROL for Identify and GET_NAME are the most likely to block a real controller's enumeration. **Size: medium per command, large in aggregate.**

5. **LOCK_ENTITY ignores UNLOCK flag and descriptor scope (5.4.2.2).** Spec: "shall support the UNLOCK flag"; "shall not allow locking another descriptor than ENTITY (NOT_SUPPORTED)." We do: `control.lock()` only takes/refreshes, never reads the flags/descriptor fields of the payload. **Change:** parse LOCK_ENTITY payload (flags at offset 0, descriptor_type/index), handle UNLOCK (release or query), and return NOT_SUPPORTED for non-ENTITY descriptors. **Size: small.**

6. **No STREAM_IS_RUNNING guard on SET_CONFIGURATION (5.4.2.5), and no "mapped channels still exist" check on SET_STREAM_FORMAT (5.4.2.7).** **Change:** add bound/streaming checks before accepting. **Size: small–medium.**

7. **GET_COUNTERS returns an all-zero block with counters_valid=0 (5.4.2.25).** Spec requires the *mandatory* counters of Tables 5.10/5.12/5.13/5.14 to be implemented and returned. We do: zeroed 128-byte block (`talker.rs:488-493`). **Change:** have the daemon populate at least LINK_UP/LINK_DOWN/GPTP_GM_CHANGED (AVB_IF), LOCKED/UNLOCKED (Clock Domain), MEDIA_LOCKED/UNLOCKED etc. (Stream Input), STREAM_START/STOP/FRAMES_TX (Stream Output). **Size: medium.**

8. **No unsolicited notifications or departing-controller monitor (5.4.5.1/3/4).** Registrations are stored but never used. **Change:** emit AEM/MVU unsolicited notifications on state-changing successful responses, with per-controller independent sequence ids and the 30–60 s CONTROLLER_AVAILABLE monitor. **Size: large.**

9. **ACQUIRE_ENTITY returns NOT_IMPLEMENTED, spec recommends NOT_SUPPORTED (5.4.2.1, "should").** Non-binding but trivially fixable. **Change:** `talker.rs:423` / `entity/mod.rs:188` → `aecp_status::NOT_SUPPORTED`. **Size: small.**

### Notes

- **The "Talker + Listener" claim is only half-served on the wire.** Everything in `entity/mod.rs` (`ListenerEntity`) is dead code with respect to live AECP: it is unit-tested but `stack.rs` wires only `TalkerEntity`. Every Listener-specific requirement (BIND, UNBIND, START/STOP_STREAMING, GET_STREAM_INPUT_INFO_EX, Stream-Input GET_STREAM_INFO flags per Table 5.8) is therefore unreachable in production even where listener code exists. The next engineer should decide whether the daemon runs two entity instances (distinct entity_ids) or one dual-role entity.

- **certification_version=0 is correct and should stay.** Both the code comment and the spec (§5.4.4.1: "set to 0 if the PAAD-AE has not passed any Milan certification") agree. Do **not** copy the 16R's 1.1.0.0 — that would be a false claim. This is the one place where honest non-compliance (we're uncertified) is itself the compliant behavior.

- **Genuine ambiguity — GET_STREAM_INFO flag bit positions.** `milan.rs:95-134` deliberately transcribes `CONNECTED` as bit 26 (0x04000000) from a 16R capture, contradicting a naive reading. Milan Table 5.8 lists CONNECTED at "Bit # 5 / 0x04000000" using its own MSB-0 numbering where "Bit 5" = 0x04000000. So the code's `1 << 26` **matches** Table 5.8's value column — the apparent conflict is just the spec's inverted bit-numbering. Worth a comment tying `milan.rs` to Table 5.8 explicitly. Note also that §5.4.2.10 says the lower 16 bits come from ACMP Table 8-4, not GET_STREAM_INFO's own table — a subtlety the current code doesn't address since it only models the upper flags.

- **Status-code reuse is coincidental, not audited.** `handle_mvu` reuses `aecp_status::*` constants. For the values we emit (SUCCESS=0, NOT_IMPLEMENTED=1) they match MVU Table 5.16, but MVU defines distinct `ENTITY_MISBEHAVING=10` and `PAYLOAD_TOO_SHORT=13` that AECP numbers differently. If MVU handlers grow, add a dedicated `mvu_status` module rather than leaning on AECP constants.

- **240 ms response-time requirement (5.4.3.4) is untestable as written.** The responder runs inline in the rx loop; there's no latency instrumentation. Marked UNKNOWN rather than COMPLIES.

- **Unavailable MCP servers this session:** `@kirocrew-computer` (declared but not configured). It was not needed for this read-only audit.