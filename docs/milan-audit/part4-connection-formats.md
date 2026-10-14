<!-- Original four-way Milan v1.3 audit, 2026-10-12. SUPERSEDED for planning
purposes by docs/milan-compliance-roadmap.md, which carries the re-audited
verdicts against the code as it stands. Kept because these tables hold the
verbatim clause text and the file:line evidence the roadmap only summarises.
Treat any verdict here as of its date, not as current. -->

## PART 4: Connection management + Base Stream Formats

**MCP servers unavailable this session:** `@kirocrew-computer` (declared by the agent spec but not configured). No tools from it were needed; the audit used file-read, grep, and code tools only.

> Scope read in full: `v13.txt` lines 4150–6008 (§5.5) and 6405–6540 (Ch. 6). All compliance claims cite code I read. **Critical framing issue up front:** the real listener state machine in `acmp_listener.rs` is written to a *design doc §9.4*, not to Milan §5.5.3. It has **8 states that are not the spec's 8 states**, a different event set, and probing/ACMP-status reporting is entirely absent. Many rows "work" but are not the spec's rows. I flag each divergence rather than rubber-stamping the design doc.

### Verbatim requirement table

| Clause | Requirement (short quote) | Binding? | Our state | Evidence (file:line) |
|---|---|---|---|---|
| 5.5.1.2 | "the Listener shall maintain its SRP attributes declaration for a specific stream … of a particular talker" | shall | COMPLIES | `acmp_listener.rs:300-321` (TaRegistered→Declare(Ready)); daemon MSRP in `stack.rs:~1470+` |
| 5.5.1.2 | "A sink … can only be bound to a single source at a time" | shall (implied) | COMPLIES | one `Binding` per `ListenerSm`, replaced on rebind `acmp_listener.rs:166-171, 408-430` |
| 5.5.1.2 | binding "will retain that state across power cycles … only transition … due to … a controller" (non-volatile) | shall | VIOLATES | No NV storage. `Binding` is in-memory only (`acmp_listener.rs:160-171`); daemon seeds BindRx fresh each run (`stack.rs:1490-1512`). No save/restore path exists. |
| 5.5.1.3 / 5.5.2.6 | After binding, Listener "continuously attempts to establish and maintain the settlement" via probing | shall | PARTIAL | Probe cycle exists (`acmp_listener.rs:on_timer` 440-490) but gives up after 3 cycles into a GET_STREAM_INFO *fallback* (`fallback` 540-560), a design-doc deviation not in §5.5. |
| 5.5.2.2 | ACMP CONNECT_RX→BIND_RX, DISCONNECT_RX→UNBIND_RX, CONNECT_TX→PROBE_TX renames | shall | PARTIAL | Only aliases exist: `PROBE_TX_* = GET_TX_STATE_*` (`constants.rs:199-201`). BIND_RX/UNBIND_RX are **not** defined; code still uses `CONNECT_RX_COMMAND`/`DISCONNECT_RX` names and opcodes (`constants.rs:189-193`, `controller.rs:126`). On the wire this is still classic ACMP, which is backward-compatible but the renames/MVU are not implemented. |
| 5.5.2.2 | PROBE_TX uses CONNECT_TX message type (0x00) | shall | VIOLATES | We map PROBE_TX to **GET_TX_STATE (0x04)**, not CONNECT_TX (0x00) (`constants.rs:199-201`). Spec §5.5.2.2 renames CONNECT_TX→PROBE_TX; GET_TX_STATE stays a separate message (§5.5.4.3). See Notes — this is a genuine ambiguity but the literal spec text says CONNECT_TX. |
| 5.5.2.2 | "connected_listeners_entries … reserved … CL_ENTRIES_VALID flag shall always be 0" | shall | COMPLIES (vacuous) | Encoder never sets CL_ENTRIES_VALID and writes reserved 0 (`acmp.rs:encode` 100-120). |
| 5.5.2.2 | SRP_REGISTRATION_FAILED renamed REGISTERING_FAILED (bit 0x40) | shall | UNKNOWN | No REGISTERING_FAILED flag constant found; `acmp_flags` (`constants.rs:230-238`) has no bit 6=0x40 named for it (0x40 = TALKER_FAILED there). Not emitted in any GET_RX_STATE response because GET_RX_STATE is not implemented. |
| 5.5.2.2 | "A Milan device shall send and accept this truncated [56-byte] PDU" | shall | COMPLIES | `ACMPDU_LEN = 56`, `encode` asserts 56 (`acmp.rs:16, 118`). |
| 5.5.2.2 | Table 5.25 fields (CLASS_B, SAVED_STATE, …) "should always be set to 0" | should | COMPLIES | `flags` defaults 0; binder/talker never set them (`controller.rs:128-137`, `acmp.rs:149-156`). |
| 5.5.2.3 | PROBE_TX_COMMAND timeout 200 ms | shall | COMPLIES | `PROBE_RESP_NS = 200_000_000` (`acmp_listener.rs:25`). |
| 5.5.2.3 | GET_TX_STATE / BIND_RX / UNBIND_RX / GET_RX_STATE timeout 200 ms | shall | PARTIAL | Listener's probe wait is 200 ms (OK). Controller's CONNECT_RX timeout is **4500 ms** (`controller.rs:19`) — but that is the controller→listener bind, which §5.5.2.3 lists at 200 ms for BIND_RX. VIOLATES for BIND_RX. GET_RX_STATE path absent → UNKNOWN. |
| 5.5.2.4(1-2) | Controller sends BIND_RX, Listener acknowledges with BIND_RX_RESPONSE | shall | PARTIAL | Controller sends CONNECT_RX and matches CONNECT_RX_RESPONSE (`controller.rs:124-205`). Works, but it is the pre-rename form and we are controller+listener in one station, so the response path is internal, not spec-shaped. |
| 5.5.2.4 | Listener stores binding params "in non-volatile memory" | shall | VIOLATES | No NV store (see 5.5.1.2 row). |
| 5.5.2.4 | Listener checks locked state before processing bind; reject if locked by other controller | shall | VIOLATES | `ListenerSm` has **no lock awareness**; BindRx always proceeds (`acmp_listener.rs:244-258`, `408-430`). No `CONTROLLER_NOT_AUTHORIZED`/`ENTITY_LOCKED` path. Spec §5.5.3.6.3 step 1 is not implemented. |
| 5.5.2.5 | Controller Unbind: Listener acknowledges, discontinues reservation, withdraws Listener Ready | shall | COMPLIES | UnbindRx → WithdrawListener + RemoveStreamEntry + Unbound (`acmp_listener.rs:432-442`). |
| 5.5.2.5 | Lock check before unbind | shall | VIOLATES | Same as bind — no lock check (`acmp_listener.rs:432-442`). |
| 5.5.2.6 | On interruption: discontinue SRP, clear saved params, listen for ADP, PROBE_TX, reserve | shall | PARTIAL | TalkerGone path withdraws + re-enters PRB_W_AVAIL (`acmp_listener.rs:366-375`); reboot path re-probes (`on_talker_available` 495-535). But "clear saved SRP params" on every interruption and the exact §5.5.2.6 ordering is not mirrored; and steps are keyed to design states, not spec states. |
| 5.5.2.7 | Talkers declare Talker Advertise/Failed "as soon as possible"; rely only on SRP for interest | shall | NOT APPLICABLE to this file set / UNKNOWN | MSRP TA declaration is daemon-side (`stack.rs` SRP participant). Our *talker* MSRP advertise path not in scope files; the talker here only answers ACMP. |
| 5.5.2.7 | "Talkers do not maintain any internal state related to bound/settled Listeners" | shall | COMPLIES | `talker_response` is pure; `TalkerStream` carries no per-listener state and `connection_count` is passed in, not accumulated (`acmp.rs:159-205`). |
| 5.5.2.7 | "DISCONNECT_TX_COMMAND always returns SUCCESS without modifying … state" | shall | COMPLIES | `talker_response` maps DISCONNECT_TX→DISCONNECT_TX_RESPONSE, status SUCCESS, no state change (`acmp.rs:166-205`). |
| 5.5.2.7 | "GET_TX_CONNECTION_COMMAND is not implemented" | shall | PARTIAL/VIOLATES | `talker_response` returns `None` for GET_TX_CONNECTION (falls through `_ => return None`, `acmp.rs:166-171`), i.e. **silence**. §5.5.4.4 says we *shall* answer GET_TX_CONNECTION_RESPONSE with status **NOT_SUPPORTED** (Table 5.53). We send nothing. VIOLATES §5.5.4.4. |
| 5.5.2.7 | "PROBE_TX_COMMAND … has no impact on the internal state of the Talker" | shall | COMPLIES | pure function (`acmp.rs:159-205`). |
| 5.5.3.1 | On invalid listener_unique_id, send *_RESPONSE with status LISTENER_UNKNOWN_ID (Table 5.27) | shall | PARTIAL | `AcmpResponse::ListenerUnknownId` variant exists but SM comment says the entity layer handles it (`acmp_listener.rs:118, 870-878`); no code in scope actually emits Table 5.27 fields. UNKNOWN whether entity layer does. |
| 5.5.3.1 | On PROBE_TX_RESPONSE with bad listener_unique_id, ignore | shall | COMPLIES | daemon routes by probe_route; unmatched dropped (`stack.rs:1364-1366, 1397`). |
| 5.5.3.1 | "The listener shall ignore all other ACMP messages" | shall | COMPLIES | `on_event` `_ => ignored` (`acmp_listener.rs:449-452`). |
| 5.5.3.2 | MVU BIND_STREAM/UNBIND_STREAM handling | shall (if MVU) | NOT APPLICABLE | MVU binding not implemented; §5.5.2.2 allows ACMP-only. No BIND_STREAM path (`grep` found none). Acceptable but means better-compat MVU path is absent. |
| 5.5.3.3 | 8 sink states: UNBOUND, PRB_W_AVAIL, PRB_W_DELAY, PRB_W_RESP, PRB_W_RESP2, PRB_W_RETRY, SETTLED_NO_RSV, SETTLED_RSV_OK | shall | PARTIAL | Our `State` enum has the **same 8 names** (`acmp_listener.rs:40-49`) — good. But semantics diverge (see state-machine section): e.g. SETTLED_NO_RSV uses a 2 s "TA_WAIT" (`acmp_listener.rs:30`) where the spec uses TMR_NO_TK = **10 s** (Table 5.30). |
| 5.5.3.4 | Events incl. TMR_NO_RESP, TMR_RETRY, TMR_DELAY, TMR_NO_TK, RCV_*, EVT_TK_* (Table 5.30) | shall | PARTIAL | Most present; `TMR_NO_TK` is 10 s in spec but implemented as 2 s TA_WAIT. `RCV_GET_RX_STATE` event has **no handler** (missing). |
| 5.5.3.4 | TMR_NO_TK = 10 seconds | shall | VIOLATES | `TA_WAIT_NS = 2_000_000_000` (2 s) (`acmp_listener.rs:30`). Spec Table 5.30: 10 s. |
| 5.5.3.4 | TMR_RETRY = 4 s; TMR_DELAY 0–1 s | shall | COMPLIES | `RETRY_NS=4s`, `DELAY_MAX_NS=1s` (`acmp_listener.rs:23,27`). |
| 5.5.3.6.3 | UNBOUND+BIND: store params, respond SUCCESS, start ADP, **send PROBE_TX immediately**, TMR_NO_RESP 200 ms, →PRB_W_RESP | shall | VIOLATES | We go UNBOUND+BIND → **PRB_W_DELAY or PRB_W_AVAIL** (`acmp_listener.rs:244-258`), never straight to PRB_W_RESP, and do not send a probe immediately. Spec §5.5.3.6.3 step 5 sends PROBE_TX right away and enters PRB_W_RESP. |
| 5.5.3.6.4/.7/.12/.19/.32/.38/.44 | Every state must answer RCV_GET_RX_STATE with a GET_RX_STATE_RESPONSE | shall | VIOLATES | **No RCV_GET_RX_STATE event and no GET_RX_STATE_RESPONSE emission anywhere** (`acmp_listener.rs` Event enum 66-110 has none; grep: no GET_RX_STATE handler). A controller polling our listener state gets silence. |
| 5.5.3.6.9 | PRB_W_AVAIL+EVT_TK_DISCOVERED → arm TMR_DELAY, →PRB_W_DELAY | shall | COMPLIES | `TalkerAvailable` in PRB_W_AVAIL → PRB_W_DELAY (`acmp_listener.rs:518-521`). |
| 5.5.3.6.16/.23 | PRB_W_RESP timeout→resend→PRB_W_RESP2; RESP2 timeout→TMR_RETRY, **ACMP status 7 (LISTENER_TALKER_TIMEOUT)** | shall | PARTIAL | Resend chain exists (`on_timer` 455-475). But RESP2 timeout increments `probe_cycles` and may jump to a **GET_STREAM_INFO fallback** at 3 cycles (`acmp_listener.rs:466-472`) instead of plain PRB_W_RETRY; and **ACMP status is never set to 7** (no ACMP status field maintained). VIOLATES the status-7 requirement of §5.5.3.6.23. |
| 5.5.3.6.18 step 1 | On PROBE_TX_RESP, check controller/talker/unique/sequence match; else ignore | shall | PARTIAL | Matching is done in the daemon via `probe_route` keyed only on talker_unique_id (`stack.rs:1364,1397`); the SM itself does **not** verify controller_entity_id/sequence_id (`acmp_listener.rs:266-299`). Weaker than §5.5.3.6.18 step 1. |
| 5.5.3.6.18 step 3 | If status≠SUCCESS: TMR_RETRY 4 s, set ACMP status=status, →PRB_W_RETRY | shall | PARTIAL | Retry+classify done (`acmp_listener.rs:289-298`) but ACMP status is only stored as `a5_classified` boolean, not the actual status value, and no GET_RX_STATE exposes it. |
| 5.5.3.6.18 step 4 | On SUCCESS: note stream_id/mac/vlan, start SRP, **TMR_NO_TK 10 s**, Probing=COMPLETED, →SETTLED_NO_RSV | shall | PARTIAL | Settles and arms TA_WAIT (`acmp_listener.rs:266-287`, `enter` 580-600) but timer is 2 s not 10 s, and Probing status (PROBING_COMPLETED) is never set/reported. |
| 5.5.3.6.36 | SETTLED_NO_RSV + TMR_NO_TK: clear SRP, →PRB_W_AVAIL or PRB_W_DELAY | shall | VIOLATES | Our TA_WAIT expiry does **not** leave SETTLED_NO_RSV; it declares Asking-Failed and stays (`on_timer` SettledNoRsv arm 477-487). Spec §5.5.3.6.36 clears SRP and returns to probing. Different behaviour entirely. |
| 5.5.3.6.42 | SETTLED_NO_RSV + EVT_TK_REGISTERED → SETTLED_RSV_OK | shall | COMPLIES | TaRegistered → SettledRsvOk (`acmp_listener.rs:300-321`). |
| 5.5.3.6.48 | SETTLED_RSV_OK + EVT_TK_UNREGISTERED: clear SRP, →PRB_W_AVAIL/PRB_W_DELAY | shall | VIOLATES | TaWithdrawn → **SETTLED_NO_RSV** (re-arm TA_WAIT) (`acmp_listener.rs:349-357`). Spec returns to the probing states, not SETTLED_NO_RSV. |
| 5.5.3 (states) | PROBING status (DISABLED/PASSIVE/ACTIVE/COMPLETED) and ACMP status set on nearly every transition | shall | VIOLATES | Neither "Probing status" nor "ACMP status" is modelled as a reportable field. `pbsta`/`acmpsta` (§5.5.2.6) cannot be answered. Only an advisory `a5_classified` bool exists (`acmp_listener.rs:186`). |
| 5.5.4.1 step1 | PROBE_TX bad talker_unique_id → PROBE_TX_RESPONSE status TALKER_UNKNOWN_ID (Table 5.45) | shall | VIOLATES | We return **TALKER_NO_STREAM_INDEX (0x04)**, not TALKER_UNKNOWN_ID (0x02) (`acmp.rs:184-187`). Wrong status code. |
| 5.5.4.1 step2 | Check command ingressed on the AVB iface of the Stream Output; else INCOMPATIBLE_REQUEST or ignore (Table 5.46) | shall | VIOLATES | No ingress-interface check in `talker_response` (`acmp.rs:159-205`). Single-NIC so low practical risk, but the check is absent. |
| 5.5.4.1 step3 | Check a valid Destination MAC is available; else TALKER_DEST_MAC_FAILED (Table 5.47) | shall | VIOLATES | `talker_response` unconditionally fills `stream_dest_mac` and returns SUCCESS (`acmp.rs:195-204`); daemon passes `stream_dest_mac(o.stream_id)` with no MAAP-allocated check (`stack.rs:3443-3451`). If MAAP has not allocated, we still answer SUCCESS. |
| 5.5.4.1 | Response on success = Table 5.48 (connection_count 0, echo FAST_CONNECT/STREAMING_WAIT, stream_id/mac/vlan filled) | shall | PARTIAL | We fill stream_id/mac/vlan and status SUCCESS (`acmp.rs:195-204`) but set `connection_count = s.connection_count` (passed as **1** from daemon, `stack.rs:3449`) where Table 5.48 requires **0**; and FAST_CONNECT/STREAMING_WAIT are echoed via `..*cmd` (OK). VIOLATES connection_count. |
| 5.5.4.1 | "Talker PAAD shall ignore the STREAMING_WAIT bit … shall always stream … as long as bandwidth is reserved" | shall | COMPLIES (by omission) | We never read STREAMING_WAIT in the talker path (`acmp.rs:159-205`); streaming is driven by SRP in the daemon. |
| 5.5.4.3 | GET_TX_STATE bad id → TALKER_UNKNOWN_ID (Table 5.51); else Table 5.52 | shall | PARTIAL | GET_TX_STATE is answered by the **same** `talker_response` path, so a bad id yields TALKER_NO_STREAM_INDEX not TALKER_UNKNOWN_ID (`acmp.rs:166-187`), and success fields are the PROBE_TX layout not Table 5.52 (listener_entity_id should be 0, listener_unique_id 0, REGISTERING_FAILED per 5.23). VIOLATES field values. |
| 5.5.4.4 | GET_TX_CONNECTION → GET_TX_CONNECTION_RESPONSE status NOT_SUPPORTED (Table 5.53) | shall | VIOLATES | `talker_response` returns `None` (silence) for GET_TX_CONNECTION (`acmp.rs:166-171`). |
| 6.2 | Base Format: AAF, PCM, 32-bit, SR∈{48,96,192} (opt {44.1,88.2,176.4}), N∈{1,2,4,6,8}, NS=6/12/24 | shall | PARTIAL | Our format decoder supports all these rates/depths/channels (`stream_format.rs:BINDABLE_NSR 37`, decodable 206-224) **plus more** (16/24-bit, up to 64 ch) — superset is fine. But we **advertise only one format** (AAF 32/48k/8ch/6spf): see 6.5. |
| 6.4 | Talker shall advertise ≥1 Base format with SR∈{48,96,192} | shall | COMPLIES (minimally) | STREAM_OUTPUT advertises `formats: vec![fmt]` = 48 kHz/8 ch Base (`talker.rs:encode_descriptor STREAM_OUTPUT`, `formats` from model). One Base-48k format satisfies the minimum. |
| 6.4 | Talker shall transport per SR Class A | shall | COMPLIES | STREAM_OUTPUT flags `CLASS_A` (`talker.rs` stream_flags::CLASS_A). |
| 6.5 | Listener shall advertise ≥1 Base format SR∈{48,96,192} | shall | PARTIAL | Listener binds the 16R's advertised format; but what *we* advertise as a STREAM_INPUT format list is listener-model-side (not in these files). Our *acceptance* set (`decodable`) covers it. UNKNOWN whether the STREAM_INPUT descriptor lists all channel counts. |
| 6.5 | If Listener advertises a 48k Base format, it **shall advertise all other 48k Base formats** (all of 1,2,4,6,8 ch) | shall | VIOLATES (likely) | We advertise/transmit a single 8-ch format (`talker.rs` `formats: vec![fmt]`); nothing enumerates the 1/2/4/6-ch siblings. If the STREAM_INPUT descriptor mirrors this single-format list, §6.5's "all channel counts" rule is violated. Should use the `ut` bit (§6.6) or list all five. |
| 6.6 | Should use the `ut` bit to describe 1..N in one format string | should | VIOLATES (non-binding) | `decodable()` **rejects** `ut=1` (`stream_format.rs:220`), and we emit `ut=0` single-count formats. We neither accept nor advertise the `ut` form. |

### Listener state machine: spec vs ours

**States** — names match, semantics do not:

| Spec state (Table 5.29) | Our `State` (`acmp_listener.rs:40-49`) | Match? |
|---|---|---|
| UNBOUND | Unbound | name ✓ |
| PRB_W_AVAIL | PrbWAvail | name ✓ |
| PRB_W_DELAY | PrbWDelay | name ✓ |
| PRB_W_RESP | PrbWResp | name ✓ |
| PRB_W_RESP2 | PrbWResp2 | name ✓ |
| PRB_W_RETRY | PrbWRetry | name ✓ |
| SETTLED_NO_RSV | SettledNoRsv | name ✓, **timer wrong (2 s vs 10 s), exit wrong** |
| SETTLED_RSV_OK | SettledRsvOk | name ✓ |

**Events** — spec Table 5.30 vs our `Event` enum (`acmp_listener.rs:66-110`):

| Spec event | Ours | Gap |
|---|---|---|
| TMR_NO_RESP | on_timer PrbWResp/Resp2 | ✓ |
| TMR_RETRY | on_timer PrbWRetry | ✓ |
| TMR_DELAY | on_timer PrbWDelay | ✓ |
| TMR_NO_TK (10 s) | on_timer SettledNoRsv (2 s "TA_WAIT") | **wrong duration + wrong action** |
| RCV_BIND_RX_CMD | Event::BindRx | ✓ (but adds `talker_in_adp` branching not in spec step order) |
| RCV_PROBE_TX_RESP | Event::ProbeTxResponse | ✓ |
| **RCV_GET_RX_STATE** | **— none —** | **MISSING: no event, no GET_RX_STATE_RESPONSE** |
| RCV_UNBIND_RX_CMD | Event::UnbindRx | ✓ |
| EVT_TK_DISCOVERED | Event::TalkerAvailable | ✓ |
| EVT_TK_DEPARTED | Event::TalkerGone | ✓ |
| EVT_TK_REGISTERED | Event::TaRegistered | ✓ |
| EVT_TK_UNREGISTERED | Event::TaWithdrawn | ✓ (but transitions to SettledNoRsv, not back to probing) |
| — (ours, extra) | TaChanged, TalkerFailed, TalkerFailedWithdrawn | design-doc additions, not Milan events |

**Transitions** — notable divergences from Table 5.31:
- **UNBOUND+BIND_RX**: spec §5.5.3.6.3 → send PROBE_TX now → **PRB_W_RESP**. Ours → PRB_W_DELAY/PRB_W_AVAIL, no immediate probe (`acmp_listener.rs:244-258`). Missing a transition target the spec requires.
- **Any state+RCV_GET_RX_STATE**: spec requires a GET_RX_STATE_RESPONSE in *every* state (8 rows). Ours: entirely absent.
- **PRB_W_RESP2 timeout**: spec → PRB_W_RETRY with ACMP status 7. Ours → increments probe_cycles, may enter a non-spec GET_STREAM_INFO fallback at 3 cycles (`acmp_listener.rs:466-472`); no ACMP status 7.
- **SETTLED_NO_RSV + TMR_NO_TK**: spec clears SRP and returns to PRB_W_AVAIL/DELAY. Ours declares Asking-Failed and *stays* (`on_timer` 477-487).
- **SETTLED_RSV_OK + EVT_TK_UNREGISTERED**: spec clears SRP → probing. Ours → SETTLED_NO_RSV (`acmp_listener.rs:349-357`).
- **Lock check** on BIND_RX/UNBIND_RX (§5.5.3.6.3/.5 step 1): absent in every row.
- **Probing status / ACMP status** writes required on almost every transition (DISABLED/PASSIVE/ACTIVE/COMPLETED): not modelled at all.

Named missing pieces: the `RCV_GET_RX_STATE` event + response, the lock check, the Probing-status and ACMP-status fields, the 10 s TMR_NO_TK semantics, and the spec's "probe immediately on bind → PRB_W_RESP" path.

### Talker behaviour: spec vs ours

Per message (our implementation = `acmp.rs::talker_response` + `stack.rs:3443-3451`):

| Message | Spec required response/status | What we do | Evidence |
|---|---|---|---|
| PROBE_TX (=CONNECT_TX) valid | §5.5.4.1 Table 5.48: SUCCESS, conn_count **0**, stream_id/mac/vlan filled | SUCCESS, stream_id/mac/vlan filled, **conn_count=1** | `acmp.rs:195-204`, `stack.rs:3449` |
| PROBE_TX bad talker_unique_id | TALKER_UNKNOWN_ID (0x02), Table 5.45 | **TALKER_NO_STREAM_INDEX (0x04)** | `acmp.rs:184-187` |
| PROBE_TX wrong ingress iface | INCOMPATIBLE_REQUEST or ignore, Table 5.46 | no check (always proceeds) | `acmp.rs:159-205` |
| PROBE_TX dest MAC unavailable | TALKER_DEST_MAC_FAILED (0x03), Table 5.47 | no check; SUCCESS with a derived MAC | `acmp.rs:195-204`, `stack.rs:3443-3451` |
| PROBE_TX STREAMING_WAIT bit | ignore it; always stream when reserved | ignored (not read) ✓ | `acmp.rs:159-205` |
| DISCONNECT_TX | SUCCESS, no state change (Table 5.50) | SUCCESS, pure ✓ | `acmp.rs:166-205` |
| GET_TX_STATE valid | Table 5.52: SUCCESS, listener_entity_id=0, listener_unique_id=0, REGISTERING_FAILED per 5.23 | echoes command's listener ids, PROBE_TX field layout | `acmp.rs:166-205` |
| GET_TX_STATE bad id | TALKER_UNKNOWN_ID (Table 5.51) | TALKER_NO_STREAM_INDEX | `acmp.rs:184-187` |
| GET_TX_CONNECTION | GET_TX_CONNECTION_RESPONSE, NOT_SUPPORTED (Table 5.53) | **None → silence** | `acmp.rs:166-171` |

The §5.5.2.7 "no talker state", "DISCONNECT_TX always SUCCESS", "PROBE_TX has no state impact" requirements are met. The error/field-value cases are where the talker diverges.

### Violations and partials, in priority order

1. **GET_RX_STATE not implemented (listener)** — spec text: §5.5.3.6.4/.7/.12/.19/.26/.32/.38/.44 "Send a GET_RX_STATE_RESPONSE message as shown in Table 5.37/5.42/5.43/5.44." We have no RCV_GET_RX_STATE event and emit nothing. A controller cannot read our sink state. Change: add `Event::GetRxState`, a per-state responder building Tables 5.37/5.42/5.43/5.44 (needs Probing/ACMP-status + effective SRP params), and wire it in the daemon's ACMP RX. **Size: large** (needs the Probing/ACMP-status fields too).

2. **Probing status + ACMP status not modelled** — §5.5.3 sets PROBING_{DISABLED,PASSIVE,ACTIVE,COMPLETED} and an ACMP status on nearly every transition, and §5.5.2.6 exposes them via GET_STREAM_INPUT_INFO_EX (`pbsta`/`acmpsta`). We keep only `a5_classified: bool`. Change: add two fields to `ListenerSm`, set them in every transition per §5.5.3.6, surface them. **Size: large.**

3. **GET_TX_CONNECTION silence (talker)** — §5.5.4.4 / Table 5.53: shall answer NOT_SUPPORTED. We return `None`. Change: in `talker_response`, add `GET_TX_CONNECTION_COMMAND => GET_TX_CONNECTION_RESPONSE` with `status = NOT_SUPPORTED`, echo routing fields. **Size: small.**

4. **Wrong talker error status codes** — §5.5.4.1/.3 Tables 5.45/5.51: invalid id shall be **TALKER_UNKNOWN_ID (0x02)**; we send TALKER_NO_STREAM_INDEX (0x04). Change: in `talker_response`, return `TALKER_UNKNOWN_ID` on the unknown-id branch (`acmp.rs:184-187`). Note this interacts with the A-5 classifier which currently treats 0x04 as A-5 — but on the *talker response* side it should be 0x02. **Size: small.**

5. **PROBE_TX success conn_count must be 0** — Table 5.48. We set 1. Change: hardcode `connection_count = 0` in the talker success path (don't pass 1 from `stack.rs:3449`). **Size: small.**

6. **No TALKER_DEST_MAC_FAILED / ingress check** — §5.5.4.1 steps 2–3. We answer SUCCESS unconditionally. Change: gate the SUCCESS response on a MAAP-allocated dest MAC (there is a MAAP module, `avb-avtp/src/maap.rs`), returning TALKER_DEST_MAC_FAILED otherwise; optionally check ingress interface. **Size: medium.**

7. **TMR_NO_TK is 2 s, should be 10 s, and SETTLED_NO_RSV exit is wrong** — §5.5.3.4 (10 s) and §5.5.3.6.36 (on expiry: clear SRP → PRB_W_AVAIL/DELAY). We use 2 s and stay in SettledNoRsv declaring Asking-Failed. Change: set `TA_WAIT_NS = 10_000_000_000` and rewrite the SettledNoRsv timer handler to clear SRP and return to probing. **Size: medium** (behavioural change, touches tests).

8. **SETTLED_RSV_OK + TA-withdrawn / unregister returns to SettledNoRsv, not probing** — §5.5.3.6.48. Change: on TaWithdrawn clear SRP and enter PRB_W_AVAIL/PRB_W_DELAY per talker-discovered state. **Size: medium.**

9. **No lock check on bind/unbind** — §5.5.2.4/.5, §5.5.3.6.3/.5 step 1: reject with CONTROLLER_NOT_AUTHORIZED (ACMP) / ENTITY_LOCKED (MVU) if locked by another controller. Absent. Change: thread lock state into `ListenerSm` (or check in the entity layer before `on_event`) and emit the Table 5.32/5.38 responses. **Size: medium.**

10. **No non-volatile binding storage** — §5.5.1.2, §5.5.2.4: bindings "shall … retain … across power cycles" in NV memory. In-memory only. Change: persist `Binding` to disk and restore via the §5.5.3.6.2 "startup with saved binding" path (→ PRB_W_AVAIL). **Size: large.**

11. **UNBOUND+BIND does not probe immediately / enter PRB_W_RESP** — §5.5.3.6.3 steps 5–9. We go to PRB_W_DELAY/AVAIL. This is a deliberate design choice (random delay first) but deviates from the normative step order. Change: on BIND, send PROBE_TX immediately and enter PRB_W_RESP (keeping delay only for the discovered-later paths). **Size: medium** — reconsider against the design doc, since the current behaviour was chosen on purpose.

12. **§6.5 single-format advertisement** — a Base-format Stream Input shall advertise all five channel counts (1,2,4,6,8) at each supported rate. We advertise one 8-ch format. Change: expand the STREAM_INPUT/STREAM_OUTPUT format lists to all five Base channel counts (or set the `ut` bit and accept `ut=1`). **Size: medium.**

### Notes

- **CONNECT_TX vs GET_TX_STATE as the probe (genuine ambiguity).** §5.5.2.2 explicitly *renames* CONNECT_TX_COMMAND → PROBE_TX_COMMAND, which textually means PROBE_TX carries message_type **0x00** (CONNECT_TX). Our code maps PROBE_TX to **GET_TX_STATE (0x04)** (`constants.rs:199-201`) and the daemon probes with `build_get_tx_state` (`stack.rs:1407`). The design doc §9.4 and a code comment assert "Milan uses GET_TX_STATE as the probe." These disagree. §5.5.4 then separately specifies both PROBE_TX_COMMAND handling (5.5.4.1) and GET_TX_STATE_COMMAND handling (5.5.4.3) as *distinct* messages with distinct response tables — which strongly implies PROBE_TX ≠ GET_TX_STATE and the rename interpretation is correct. **This needs a decision**: if the spec interpretation holds, every probe we send is the wrong opcode and interop with a strict Milan talker is at risk. I could not find a §5.5 sentence that equates the two; the equivalence is only in the local design doc. Recommend verifying against §5.3.8.5/5.3.8.6 (probing/ACMP status, out of my range) and the ATDECC base before changing anything.

- **The whole listener SM is written to design-doc §9.4, not Milan §5.5.3.** State *names* coincide, which is reassuring, but the event set, timers (TMR_NO_TK), the probe-fallback to GET_STREAM_INFO, the TaChanged/TalkerFailed events, and the Probing/ACMP-status omission all come from the design doc. Before acting on items 1/2/7/8/11, confirm whether the design doc intends to *replace* the Milan rows (in which case this audit's "violations" are intentional deviations the team accepted) or *implement* them (in which case they are bugs). The module doc comment says "if the Milan text differs the constants change, not the table's structure" — so the team expected the *table structure* to match Milan, which makes the missing GET_RX_STATE row and the wrong TMR_NO_TK duration real defects by their own standard.

- **A-5 classifier reuses ACMP status 0x04.** `A5_RESOURCE_STATUSES` includes TALKER_NO_STREAM_INDEX (0x04) (`acmp.rs:19-26`). If item 4 changes the talker's unknown-id response to 0x02, confirm the listener-side A-5 set is still correct (it classifies *talker-reported* failures; 0x02 TALKER_UNKNOWN_ID is not in the A-5 set, which is probably right).

- **"We bind each sink exactly once" (controller) verified:** `Binder` binds sequentially, one CONNECT_RX per target, advancing cursor only on final outcome (`controller.rs:148-233`). This matches §5.5.1.4's "controller has no further involvement after acknowledgement" and does not re-bind. ✓

- **Single-NIC caveat:** the ingress-interface check (§5.5.4.1 step 2) and some gPTP-domain checks are low-risk because this PAAD has one I210, but they are still normatively required and absent.

- **Base stream format support is a superset and that is fine** (§6 permits optional rates/Extended). The only §6 compliance gap is the *advertisement completeness* rule in §6.5 (all channel counts at a supported rate), not the decode capability.