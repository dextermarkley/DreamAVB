# AVB-Audio: context handoff (2026-10-14)

Native macOS AVB audio bridge. Our own NIC driver plus a Rust stack exposes a
PreSonus StudioLive 16R (Milan firmware 3.3.0.109659) to DAWs (Pro Tools,
Reaper) as a Core Audio device. The long-term goal is a fully Milan-compliant
Mac AVB driver because the macOS AVB implementation is old and broken for this
use. Our path bypasses Apple's built-in ATDECC entirely; the project owns
discovery, enumeration, connection management, timing, and media transport.
The current phase deliberately prioritizes a working end-to-end connection
over full Milan compliance. Record conformance gaps, but do not treat
implementing more of Milan as the next fix without evidence that a specific
gap blocks this connection.

Read this file first, then `AGENTS.md` (rules) and the docs it points to.

## Authoritative resume point (2026-10-14)

Phase 1a is complete on real hardware. Phase 1b is active on branch
`feat/companion-app`: 64 channels each way, gPTP, MAAP, MSRP/MVRP, ACMP, AEM,
AAF and an optional CRF talker all work against the live 16R. A user-authorized
signed `duplex` daemon has locked gPTP, and the 16R reports
all eight audio STREAM_INPUT sinks bound to our exact stream IDs, MAAP addresses,
VLAN 2, formats, and `CONNECTED|SAVED_STATE|FAST_CONNECT` state.

The `STREAM_PORT` descriptor fix below made `AVB-Audio Mac` a live, selectable
source in UC Surface. The remaining symptom is no audible or metered Channel 1
return despite a connected stream and a nonzero Mac output ring. **Do not treat
this as console routing alone:** the Channel 1 test ran under `AVB_TXCAP`, which
cut the 48 kHz / 6-sample audio stream to about 4,700 PDUs/s instead of the
required 8,000 PDUs/s. The 16R also recorded late timestamps. See the
"Channel 1 transport timing review" below before another hardware test.
A known-good Apple/Broadcom session is retained only as a behavioral reference
in `.agents/tmp/apple-avdecc.pcap` and `.agents/tmp/apple-enum.pcap`; Apple's
ATDECC remains bypassed.

The latest live ACMP diagnostic resolves a suspected index failure. Audio talker
UIDs 0–7 bind the 16R's Return sinks 1–8. The repeated UID 8 request targets Return
CRF sink 0, a persisted CRF route from an older model; the current product model
intentionally has no CRF output, so `TALKER_UNKNOWN_ID` is the correct response.
Do not reindex audio streams or add a synthetic ninth output.

Do **not** resume from the older MAAP, grandmaster, talker-only entity, missing
CRF or inert-unsolicited notes below: all are implemented and hardware-tested.
CRF, `MEDIA_CLOCK_SOURCE`, expanded AEM descriptor coverage, sink symmetry, entity lifetime,
available-index reset, state notifications, Apple's per-stream GET_COUNTERS
heartbeat, controller capability, prior descriptor index and offset fixes,
static PTP descriptors, PTP grandmaster notifications, model-ID prefix, and
initial `CONNECT_RX.stream_id=0` were tested before the `STREAM_PORT` fix and
did not resolve the source-listing problem.

### Connection research (2026-10-14)

The current failure should not be described as an inability to establish an AVB
connection. The signed `duplex` daemon log at
`.agents/tmp/unmanaged-daemon-20261014.log` shows gPTP `LOCKED`, the 16R as
grandmaster, Listener Ready for audio talker UIDs 0–7, and 16R `GET_STREAM_INFO`
for Return sinks 1–8 with `CONNECTED|SAVED_STATE|FAST_CONNECT`, the expected
stream IDs and destination MACs, and VLAN 2. The 16R's `FRAMES_RX` counters
increase during live probes (for example 20,147 → 44,316 → 68,876 in
`.agents/tmp/controller-capability-live.log`). This demonstrates binding and
frame reception; it does not prove the console is using the samples as audio or
explain UC Surface's `(offline)` classification.

At that time, the issue was the mismatch between this live AVDECC/data-plane
state and UC Surface's source list. **Plain IEEE 1722.1 research found a definite
AEM wire-format bug and a matching enumeration retry loop; the hardware retest
below confirmed that correcting it cleared UC Surface's `(offline)` label.** The
repeated `CONNECT_TX` for talker UID 8 is the previously saved CRF route to
sink 0, separate from all eight connected audio streams.

IEEE 1722.1 §7.2.13's 20-byte `STREAM_PORT_INPUT`/`STREAM_PORT_OUTPUT`
descriptor has `number_of_controls` and `base_control` at offsets 8 and 10,
followed by cluster count/base at 12 and 14, then map count/base at 16 and 18.
The current `encode_stream_port` in `crates/avb-avdecc/src/aem/desc.rs` omits
the two control fields and emits only 16 bytes; its matching decoder makes
local round-trip tests unable to catch the omission. The source layout is
independently reflected in the [PipeWire AVB descriptor
implementation](https://github.com/PipeWire/pipewire/blob/master/src/modules/module-avb/descriptors.c)
and the [Wireshark AVDECC dissector source](https://sources.debian.org/src/wireshark/2.6.7-1~deb9u1/epan/dissectors/packet-ieee17221.c/).

The transmitted `STREAM_PORT_INPUT.0` response in
`.agents/tmp/our-controller-capability-live-20261013.pcap` is exactly 16 bytes:
`000e 0000 0000 0000 0008 0040 0001 0008`. A conforming reader interprets
this as **8 controls starting at index 64** and **1 cluster starting at index
8**, with the required map fields absent. With no port controls, the intended
20-byte descriptor would be `000e 0000 0000 0000 0000 0000 0008 0040 0001 0008`.
The 16R then reads `AUDIO_CLUSTER.8`
successfully, asks for `CONTROL.64`, receives `NO_SUCH_DESCRIPTOR`, and begins
again at `ENTITY.0` about 67 ms later. This exact sequence repeats five times
within about three seconds. The query is therefore explained by our shifted
fields. This is
strong evidence that malformed AEM enumeration blocks the live source listing,
despite the independent audio stream bindings and received-frame counters.

**Hardware retest (2026-10-14): confirmed.** The corrected signed daemon emitted
20-byte STREAM_PORT descriptors with zero controls, and the prior `CONTROL.64`
request and enumeration retry loop disappeared. UCNet then listed `AVB-Audio Mac`
in `mixerlist` and exposed all eight named `Mac→16R` stream banks as live
candidates in every `avb_src_*` list. This fixes the UC Surface `(offline)`
classification gate.

### Successful connection status and Channel 1 audio boundary (2026-10-14)

The AVB control connection and Ethernet frame reception are proven. Audio
playout inside the 16R is not yet proven:

- The corrected entity is live in UC Surface as `AVB-Audio Mac`; all eight named
  source banks are selectable, not `(offline)`.
- gPTP is `LOCKED` to the 16R grandmaster. All eight 16R Return sinks report
  `CONNECTED|SAVED_STATE|FAST_CONNECT`, exact stream IDs, MAAP destination
  addresses, VLAN 2, and accepted MSRP Listener Ready declarations.
- A user-authorized unmanaged signed `duplex` daemon was used for hardware
  tests. Its captured run transmitted 60,866,728 PDUs with zero rate
  limits/refusals, all eight sinks accepted, and a measured outbound peak of
  -14.4 dBFS. The Mac Core Audio output ring is therefore not silent, but the
  packet count over 1,619 s is far below the required cadence (see below).
- `stageboxsetup/avb_src_1_8` reads back as
  `AVB-Audio Mac:Mac→16R 1-8`; the console's AVB Send 1 is assigned digital
  source 0. These are successful connection/routing assignments, not pending
  protocol work.

The remaining failure is **Channel 1 audio playout**, with a demonstrated
transport-timing fault in the test run.
A scratch channel-isolated Core Audio tone was verified nonzero in output-ring
channel 1 and was transmitted by the daemon, but neither the daemon's returned
input ring nor any UCNet UDP meter group reported a Channel 1 increase. A
six-second 1 kHz Channel 1 tone at 0.25 amplitude left UDP meters unchanged;
only pre-existing low background on channels 5 and 14 appeared. The test does
not isolate console routing from transport timing.

### Channel 1 transport timing review (2026-10-14)

Spec-level review found a real media-cadence violation in the run containing
the Channel 1 tone. The 16R's selected AAF format is 48 kHz, eight channels,
six samples per PDU (`0x0205022002006000`). Six samples at 48 kHz require one
PDU every 125 us, or **8,000 PDUs/s per stream**
([Milan consolidated v1.3](Milan_Specification_Consolidated_v1.3.pdf),
Table 6.3 and §5.3.7.6; the [IEEE 1722 AAF working-group timing
notes](https://grouper.ieee.org/groups/1722/contributions/2014/IEEE%201722a%20Assumptions-v17.pdf)
place each packet's timestamp at its first sample). In
`.agents/tmp/stream-port-fix-20261014.pcap`, stream 0
(`0x644bf01273b80000`, 16R Return 1-8) has 47,007 PDUs from 08:20:00 to
08:20:10 local time: **4,700.7 PDUs/s**. Its channel-1 sample bytes are nonzero
in that window, so the transmit path did not merely packetize silence. The
16R's sink-1 `GET_COUNTERS` in `.agents/tmp/stream-port-fix-20261014.log`
shows `LATE_TIMESTAMP` (counter slot 9) rising 14 -> 15 -> 19 over the three
early probes. `FRAMES_RX` rises, confirming frame reception, not sample
playout.

The captured run's summary is 60,866,728 audio PDUs over 1,619 s and eight
streams = **4,699.4 PDUs/s/stream**, with 3,772,824 missed transmit
intervals. The earlier `.agents/tmp/unmanaged-daemon-20261014.log` summary is
294,312,872 PDUs over 4,622 s and eight streams = **7,959.6
PDUs/s/stream**, with only 69,444 missed intervals. This comparison strongly
implicates diagnostic self-capture: when `AVB_TXCAP` is set,
`crates/avb-daemon/src/realdext.rs::append_pcap` synchronously opens, appends
and closes the pcap **once per frame on the transmit thread**, before the
batched send. The 14 GB capture spans the Channel 1 tests. The pcap records
attempted frames before `send_batch`, but the daemon reports zero refusals;
the 16R's advancing receive counter corroborates the reduced arrival rate.

There is an independent timestamp-design flaw to review: `talker_loop` stamps
every PDU as `gptp.now_ns(now_mach()) + 2 ms`. On a missed deadline it moves
`next_pdu_ns` to `elapsed + one interval`, but consumes only the next six ring
frames. Presentation time therefore follows when the loop happened to run
instead of the continuous media-sample timeline. This is especially severe
under capture load; its effect during a near-8,000-PDU/s run remains unproven.
Derive successive PDU timestamps from a media-clock/sample cursor, and account
for skipped sample intervals when late. The 2 ms presentation offset itself is
not the finding: Milan v1.3 §5.3.7.6 prescribes it by default and allows an
additional timing-uncertainty term of up to 125 us.

**Clean-cadence retest and final Channel 1 timing repair (2026-10-14):
confirmed.** The scheduler now derives presentation timestamps from a
media-interval cursor and discards matching stale ring frames when it misses
intervals. The first uncaptured retest improved cadence and held the sink's
late-timestamp counter flat, but its outgoing nonzero Channel 1 AAF trace was
still +2.720 ms ahead of gPTP. That exposed a final cursor-ahead case.

The repair re-anchors the media cursor only when its next timestamp would exceed
the live 2 ms gPTP presentation target. A bounded trace run logged a nonzero
outgoing Channel 1 sample (`70075712`) in stream 0 with a +1.949 ms presentation
offset, inside the 2 ms limit. An earlier raw ring observation appeared to show
a returned Channel 1 peak, but it was not reproducible and must **not** be
considered proof of end-to-end audio.

Fresh direct recordings resolve the ambiguity: a 12-second mono Channel 1 WAV
captured from `/Users/Shared/avb16r.ring` during the same six-second 1 kHz
Channel 1 tone contains no Channel 1 sample above 10,000,000, and a simultaneous
raw ring sampler reports only background on channels 2–8. This recorder reads
the daemon's world-readable mapped ring directly; it does not use macOS
microphone permission or a Core Audio input device. macOS is therefore not
blocking the recording. The Channel 1 playout/return path remains unresolved.

Do not change UC Control settings or revisit AEM, ADP, ACMP, MSRP, MAAP, gPTP,
or source-listing hypotheses without a new regression. The user-authorized
unmanaged daemon remains active after the bounded test; stop it cleanly before
starting DreamAVB-managed audio.

Do not broaden testing beyond Channel 1. UC Control settings have now been
**confirmed correct by the user**, including Channel 1 Digital Return set to
Network. The raw UCNet node `/return/ch1/inputsrc` was restored to its original
`0.0` after exploratory enum trials; do not infer labels from that raw value or
retry console-setting changes. The configuration is no longer the hypothesis.

After the clean-cadence test, preserve the confirmed UC Control routing and
collect aligned Channel 1 signal-path evidence: Core Audio/output-ring sample,
outgoing AAF payload and timing, 16R receive state, and returned input-ring
sample. Avoid `AVB_TXCAP` on the time-critical path while collecting it.

Plain IEEE 1722.1 narrows the interpretation: ADP advertises a live entity and
answers discovery, while ACMP and `GET_STREAM_INFO` report connection state.
The standard's `CONNECTED` flag means connected with ACMP; `SAVED_STATE` and
`FAST_CONNECT` describe saved/reconnection state, not UC Surface's definition
of a selectable source. See the IEEE working-group material on
[ADP announce/discover](https://grouper.ieee.org/groups/1722/1/contributions/2016/1722.1-butterworth-Scaling.pdf)
and the [1722.1 draft's `GET_STREAM_INFO` flag table](https://grouper.ieee.org/groups/1722/1/contributions/2019/Rimas%20P1722.1REV1%20changes/IEEE-P1722.1REV1-changes.pdf).
In `.agents/tmp/our-controller-capability-live-20261013.pcap`, our nine
`ENTITY_AVAILABLE` frames have consecutive `available_index` values 0–8 and
2.03–8.24 s gaps against a 20 s advertised validity, followed by one
`ENTITY_DEPARTING` at shutdown. This rules out an obvious transmit-side ADP
expiry in that capture, but it does not show whether the 16R received each
advertisement or how UC Surface classified it. The malformed AEM descriptor
was subsequently confirmed by the hardware retest above.

### AVB and Milan version compatibility (research, 2026-10-14)

There is no single negotiated "AVB version": AVB combines IEEE timing,
reservation, transport and AVDECC protocols. Individual AVTP/AVDECC packets
carry a protocol `version` field (the current AVDECC frames here use 0), while
ADP/AEM descriptors expose capabilities and supported stream formats. A
controller discovers a peer, reads its model and selects a mutually supported
configuration; it does not exchange a stack-wide version and automatically
downgrade. See the [IEEE 1722.1-2021 standard page](https://standards.ieee.org/ieee/1722.1/6670/)
and the [Avnu AVB packet-format example](https://avnu.org/wp-content/uploads/2014/05/AutoCDSFunctionalSpec-1_4-public_with_legal_notices.pdf).

Milan adds explicit profile identification through the vendor-unique
`GET_MILAN_INFO` response. The [Milan consolidated v1.3
specification](https://avnu.org/resource/milan-specification/) §5.4.4.1 gives
separate `protocol_version` (currently 1), `features_flags`,
`certification_version` (0 if uncertified), and `specification_version`
(e.g. 1.3.0.0) fields. `specification_version` was added in v1.3; a zero in
an older device's response does not mean Milan protocol version 0. The live
16R capture in `crates/avb-avdecc/src/milan.rs` has protocol version 1,
certification version 1.1.0.0, and no declared specification version. Our
response currently reports protocol version 1, certification 0 and
specification version 1.3.0.0. The specification revision describes supported
behavior, not just the document used during development: §5.4.4.1 recommends
that a noncompliant active Configuration not answer `GET_MILAN_INFO` (a future
requirement), and the introduction requires full compliance when advertising
Milan compatibility. Reconcile our advertised 1.3 behavior with actual
conformance in the later Milan phase; retain the working interoperability
experiment for the current phase.

Backward compatibility should be deliberate and per feature. Milan v1.3 itself
keeps ACMP binding commands for older controllers without MVU binding
(§5.5.2.1), requires sending and accepting the 56-byte ACMP PDU for backward
compatibility (§5.5.2.2), and describes revision-sensitive meanings for
`GET_STREAM_INFO` length/latency (§5.4.2.10) and Stream Output counter indices
(§5.4.2.25). The controller should read available version/capability/format
signals, handle known older layouts by their defined length or revision, and
use the common supported feature set. Unsupported extensions should receive
the specified response. Do not infer that two peers interoperate just because
both report protocol version 1 or because one has a Milan certification. No
evidence ties the current UC Surface `(offline)` state to a version mismatch;
the malformed base IEEE 1722.1 `STREAM_PORT` descriptor and matching 16R
retry loop remain the first concrete blocker to test.

The existing Apple captures show a selectable baseline, but the current
`AVB_TXCAP` path captures only Mac transmissions (see
`crates/avb-daemon/src/realdext.rs`). Our transmit-only capture cannot reveal
the 16R's complete response or the controller decision. The I210 cannot be
captured with `tcpdump` while its dext owns it; use an external capture point
or add receive-side instrumentation in a later code phase. ADP
capability/model fields differ between Apple and our entity, but none is
established as the cause. If correcting the malformed STREAM_PORT descriptors
does not restore UC Surface's source listing, the next observation should be a
**bidirectional** capture of the running daemon and 16R while UC Surface
changes or refreshes its source list, aligned with daemon logs and the
displayed state. Compare the resulting ADP, AEM, ACMP, and any vendor traffic
with the known-good capture, then test one hardware-grounded hypothesis at a
time.
PreSonus's
[Mac AVB connection guide](https://support.presonus.com/hc/en-us/articles/38925260615437-How-to-connect-your-Series-III-console-running-firmware-v3-1-to-a-Mac-over-AVB)
describes Apple's routing workflow and is useful only as reference behavior
for this independent driver.

Suggested next steps, in order:
1. Preserve the user-confirmed correct UC Control configuration. Do not change
   Channel 1 Digital Return, AVB source banks, AVB Send 1, or raw UCNet enum
   values without new evidence.
2. Replay the existing Channel 1-only 1 kHz tone through `AVB StudioLive 16R`
   while collecting aligned evidence at four boundaries: Core Audio/output ring,
   outgoing AAF frame, 16R AVB receive state, and returned input ring or UDP
   meter. Keep all experiments scoped to Channel 1.
3. Use the first boundary that lacks the tone to investigate only that layer of
   the Channel 1 internal return/send routing. Do not reopen resolved AEM, ADP,
   ACMP, MSRP, MAAP, gPTP, or source-listing hypotheses.
4. Before returning control to DreamAVB, stop the user-authorized unmanaged
   daemon cleanly so the companion can again own the I210 exclusively.

Validation for the staged batch is green: workspace fmt, warning-denied clippy,
576 Rust tests, 97 tg3 host checks, 390 I210 host checks, and `scripts/package.sh`
with both DriverKit builds and signing.

**Historical notes below are retained as an implementation record. When they
conflict with this section or the current code, this section wins.**

## Where things stand

- Phase 0 is done: the Broadcom BCM57762 in Apple's Thunderbolt Gigabit adapter is rejected. RX PTP timestamps never latch. See `docs/phase0-results.md`.
- Phase 0b is done: the Intel I210 passes every hardware gate against the 16R. Link delay is about 143 ns ± 2.5 ns and NRR is stable. See `docs/phase0b-i210-results.md`.
- Phase 1a (16R → Mac listener/recording) is complete. All software and live
  hardware milestones passed; see `docs/phase1a-results.md`.
- Phase 1b (full-duplex Core Audio product) is in progress on branch
  `feat/companion-app`. The data plane works in both directions at 64 channels;
  the open blocker is the 16R/UC Surface live-routing candidate described in
  the authoritative resume point above.

### Resume point (2026-10-12, `65f5676`) — why the 16R still will not route us

Eight talkers reach 63 809 frames/s on the wire via the batched dext send, the
AEM answers everything the 16R asks (`refused=0`), the Milan MVU responder is
in, and stream destination addressing is now spec-correct.

**UC Surface DOES see us, and the routing surface is `Network > AVB Inputs`.**
A screenshot (2026-10-12 16:56) settled the question the protocol probes could
not. It lists our entity per 8-channel bank:

```
Input 1-8    None Available
Input 9-16   0x644BF0FFFE1273B8:1 (offline)
Input 17-24  0x644BF0FFFE1273B8:2 (offline)
 ...
Input 57-64  0x644BF0FFFE1273B8:7 (offline)
```

So bank N maps to 16R `STREAM_INPUT N`, Input 1-8 taking our stream `:0`.
Seven of our eight streams were discovered and remembered as assignments.
`(offline)` is the controller's SAVED routing for a talker that is not
advertising ADP at that moment — it appears the instant the daemon exits, and
is why the user saw the banks "flash in and out": each test run advertised for
~20 s and then stopped. It is not a discovery or protocol failure. The daemon
must be running continuously for the banks to be assignable.

`Input 1-8` reading `None Available` is Fault A seen from the controller side:
our stream `:0` has no saved assignment because the 16R never bound that sink.

The remaining faults, settled by measurement rather than reasoning:

**Read `docs/milan-conformance.md` FIRST.** It checks our talker against the
Avnu Milan Baseline Interoperability Specification v2.0a clause by clause. An
afternoon of inferring requirements from the 16R's behaviour found nothing;
an hour with the spec found three real violations. The open one is the likely
blocker: **MAAP is not implemented**, and Milan §6.3.1 makes MAAP allocation a
precondition for a Talker attribute's destination address being VALID — so
every Talker attribute we declare is formally invalid, which is consistent
with the 16R reserving the path, counting our frames and never media-locking.
Do not add more descriptors or capability bits before implementing MAAP.

- `GET_STREAM_INFO` on the 16R's sinks 2..8 reports CONNECTED, probing
  COMPLETED, ACMP SUCCESS, our exact stream ids and MAAP addresses, and
  `FRAMES_RX` climbing at 8000/s — with no format, timestamp or sequence
  counter set. `MEDIA_LOCKED` is nonetheless still 0. **A wire dump in both
  directions has now cleared the PDU itself**: our AAF header is
  byte-identical to the 16R's own apart from the `mr` toggle, the stream id,
  the sequence number and the timestamp (`format`, `nsr`,
  `channels_per_frame`, `bit_depth`, `stream_data_length`, `sp`, `evt` all
  match), and the presentation offsets are both sane on the locked clock —
  ours +2125 us, the 16R's own +1327 us.
- **`MEDIA_LOCKED=0` IS NOT A FAULT — settled from the 16R's own AEM, no
  hardware needed.** Its `CLOCK_DOMAIN` (`clock_sources_offset=76`,
  `clock_sources_count=11`) selects `clock_source_index=0`, which is
  `CLOCK_SOURCE 0 = "PTP Clock"` (EXTERNAL, `loc=TIMING[0]`). Its other ten
  sources are `Internal Clock` and nine `Input Stream N` entries pointing at
  `STREAM_INPUT[0..8]` — and it is on NONE of them. So the 16R derives its
  media clock from gPTP and never recovers one from our stream, which is why
  the counter has no reason to assert. Do NOT add descriptors or conformance
  fixes chasing it; four hours went that way. Decode it yourself with the
  CLOCK_SOURCE/CLOCK_DOMAIN snippet against `.agents/tmp/16r-aem-full.json`.
- **Fault A: the 16R never binds its `STREAM_INPUT 1`** ("Return 1-8",
  channels 1-8 — and `Input 1-8` in UC Surface). `stream_id=0`,
  `dest_mac=00:00:00:00:00:00`, `probing_status=DISABLED`. Identical with
  eight talkers or with `--listener-uid 1` as the only stream, so it is not
  contention. It uses descriptor indices for `listener_unique_id` (naming 2
  binds descriptor sink 2), so it is not an index-base mismatch on our side.
  **Not our bug, and the flag word says so:** a bound sink reads
  `flags=0xf6000006` and this one reads `0xf2000000`; the delta `0x04000006`
  is `CONNECTED | SAVED_STATE | FAST_CONNECT`. `SAVED_STATE` clear means the
  CONTROLLER never saved an assignment for that bank — which is also why the
  other seven re-bound by themselves after a 16R power cycle with no
  controller action and this one did not. Its descriptor is byte-identical in
  kind to the seven that bind (`flags=0x0003 CLOCK_SYNC_SOURCE|CLASS_A`,
  `clock_domain=0`, 10 formats, same current format), so nothing in the model
  explains it. Fix: the user assigns `Input 1-8` in UC Surface. The daemon now
  prints `saved_assignment=no -- assign this bank in the 16R's controller`.
- **Fault B: inbound slot 0 starves under eight concurrent talkers** — 0
  frames placed, while one talker leaves all eight inbound slots balanced
  within 6 frames. A listener-only run shows the 16R transmitting all eight
  streams at 8000/s with zero sequence gaps, so reception is fine and this is
  our own TX/RX contention in the duplex loop. The same run reports
  1 065 764 overrun frames and a nonsensical 98.9e9 "gap frames", so the gap
  accounting regressed too.

Useful instruments added here: `GET_STREAM_INFO` + repeated `GET_COUNTERS`
probes in the duplex talker (see `stack.rs`, `SINK_PROBE_COUNT`);
`dump_aaf_pdu`, which prints one PDU per direction through ONE formatter so
ours is directly comparable to the 16R's known-good bytes, with each PDU's
presentation offset against our own gPTP clock; and `aem-dump` now walks EVERY
advertised descriptor type rather than filtering to
`AUDIO_UNIT | STREAM_INPUT | STREAM_OUTPUT` — the filtered version is why 21
earlier captures contained no CONTROL and no CLOCK_DOMAIN. Full 37-descriptor
capture: `.agents/tmp/16r-aem-full.json`.

The inbound dump is gated on `armed` deliberately. Dumping the first inbound
PDU reads a PRE-STEP clock — the first arrives during discovery and the servo's
opening step can be hundreds of ms — which measured the 16R's presentation
offset as −475 ms, purely the step not yet applied. Both dumps must come off
the same locked clock to be comparable at all.

Read off the AEM capture, all hardware-anchored: the 16R's `STREAM_OUTPUT 0` /
`STREAM_INPUT 0` are **CRF** ("Send CRF" / "Return CRF"), its audio streams are
indices 1..8 at `0x0205022002006000` (48 kHz, INT_32, depth 32, 8 ch, 6 spf),
its CLOCK_DOMAIN 0 runs on clock source 0 "PTP Clock" (EXTERNAL → TIMING), and
its `counters_valid` is `0x0f3f` = Milan's STREAM_INPUT set minus
`TIMESTAMP_VALID`/`NOT_VALID` at bits 6-7.

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

**Bidirectional audio WORKS on the live 16R** (`1e88c37`). `avb-daemon duplex`
reaches `listener_ready=true` in both directions with real audio measured on
each. The verified 30 s / 60 s runs are under "Verified on hardware at
`1e88c37`" below. Four hardware-only bugs had to be fixed to get there; all
four are described in `54c5d92`'s commit message and summarised under
"Hard-won lessons".

**64 channels each way are bound and accepted by the 16R.** Eight inbound
streams (`--all-inbound-streams`) and eight outbound talkers, all eight
`Listener Ready` in both directions, with the 8-stream AEM (8 `STREAM_OUTPUT`s,
64 `AUDIO_CLUSTER`s) the 16R needs in order to bind them.

**The outbound frame rate is now SUSTAINED at the full width, and that is
VERIFIED on hardware.** Eight class-A streams need 64000 frames/s. A single
`SendFrame` user-client call measures ~27 us, so the per-frame path saturated
at ~37000/s with `0 rate-limited` — the token bucket was never the constraint,
the per-CALL cost was. ABI **v4** `sel::SEND_FRAMES` (selector 15) queues up to
8 frames per call behind one doorbell. Measured on the live I210 with
`284.21.58` attached:

```
                        per-frame      batched
frames/s on the wire       37063         63008
intervals missed           49272          1353   (23% -> 0.6%)
rate-limited                   0             0
outbound streams     8 of 8 accepted   8 of 8 accepted
```

63008 of 63992 nominal is 1.5% short, which is residual pacing jitter in a
userspace loop, not a throughput ceiling. The C++ host test
`batched full-width talker unthrottled (64000 frames/s)` covers the dext side.
The daemon picks the path from the ABI the ATTACHED dext reports (never from
our own compiled constant) and names the path it used, so a silent fallback
cannot look like a batched run.

### Replacing the dext: TWO independent traps, both now understood

Getting `284.21.58` to actually load cost most an afternoon because two
separate things were in the way. Check both.

**1. The incumbent owns the PCI device, so the upgrade defers.** `activate`
stages the new build and marks it `activated enabled` while the kernel keeps
running the OLD one. Un-match it by removing the device: **unplug and replug
the Thunderbolt adapter**, which takes seconds and is confirmed working here.
Do NOT reboot for this — see the research note below for why no reboot-free
API exists.

**2. Adhoc and signed dexts are SEPARATE records that never replace each
other.** macOS keys a system extension on (teamID, bundleID). An adhoc-signed
dext shows teamID `-`, the Apple-Development one shows `8NG8NMGYVF`, and both
can sit `activated enabled` at once with the adhoc one winning the match:

```
-            284.21.16  [activated enabled]   <- attached, shadowing
8NG8NMGYVF   284.21.58  [activated enabled]   <- the build we wanted
```

`package.sh --adhoc` runs in the gate, so activating from `build/` afterwards
creates that shadow line. The tell is in activate's own output: `replacing
installed extension version 282.7.31 with 284.21.58` — it replaced the other
SIGNED record and left the adhoc one running. Clear it with
`systemextensionsctl uninstall - com.avbaudio.installer.i210` (prompts for auth
even as root), then replug. **Always repackage SIGNED before activating.**

**Trust only `avb-installer status`.** It compares the code-signing hash the
kernel reports for the attached service against the staged copies — the one
instrument that was never wrong, where the version list named a version as
active that had never loaded three separate times:

```
attached in the kernel: code hash 91a1acd2... = staged version 284.21.58
this bundle would install: code hash 91a1acd2... — ALREADY RUNNING
```

**Prefer a Thunderbolt REPLUG over a reboot.** Researched 2026-10-12: there is
no documented reboot-free way to replace an always-matched dext, and the
cheapest real option is to un-match it by removing its device.

- An in-place `activationRequest` ALWAYS defers the incumbent's termination to
  reboot for an always-matched dext. Someone with the identical problem (an
  always-matched `SCSIControllerDriverKit` dext) reports exactly our symptom in
  Apple Developer Forums thread 836344: "the new version reaches
  `[activated enabled]` but never starts; the old process keeps running until
  reboot". Karabiner-DriverKit-VirtualHIDDevice's own `DEVELOPMENT.md` says the
  same: "submit does not restart your driverkit userspace process. The most
  reliable way to restart your userspace process is reboot."
- `IOService::Terminate()` is NOT a self-termination escape hatch. The
  DriverKit 25.5 SDK header documents it for services the driver itself made:
  "An IOService object created with `Create()` may be removed by calling
  `Terminate()`." Do not ship a self-terminate selector on that basis — it
  would be an undocumented use of the API on a matched driver.
- A dext cannot un-match itself either: "There is no way for a DriverKit driver
  to programmatically modify the DriverKit driver's matching rules."
- `systemextensionsctl developer on` does NOT help here. It allows loading from
  outside `/Applications` with SIP disabled; it does not replace a running
  driver.
- macOS has no software PCIe/Thunderbolt re-enumeration command, so the device
  must actually go away.

Since the I210 is behind Thunderbolt, **unplug and replug the adapter** instead
of rebooting: that un-matches the dext, which is precisely what sysextd is
waiting for, and it costs seconds instead of a full restart. Then `activate`
and check hashes with `avb-installer status`.

One UNTESTED alternative, worth a try only because its worst case is the reboot
we would otherwise do: `avb-installer deactivate` then `activate`. If
deactivation terminates the incumbent, the device is released and the new build
loads. If it merely defers (`terminating for uninstall but still running` is a
documented state) nothing is lost. The risk is that the device IS released,
Apple's generic `AppleEthernetE1000` claims it, and the activate cannot win the
match until a reboot — leaving no AVB driver in the meantime.

Note also that most iteration does NOT need this: the dext ABI changes rarely,
and daemon, plug-in and companion changes all reload without touching it.

The HAL plug-in IS installed (`/Library/Audio/Plug-Ins/HAL/AVBAudio.driver`)
and is at **64 channels** in and out; `avb_audioring::DEVICE_CHANNELS` is the
single source of that width because the plug-in refuses a ring that disagrees.
A duplex run with nothing playing shows `outbound peak 0` and output-ring
underruns — that is no audio source, not a broken path.

To re-verify the whole chain, nothing needs to be run by hand — use the
companion app's **Audio** toggle. For a bounded CLI run instead:

```
/Users/dexter/AVB-Audio/build/AVBInstaller.app/Contents/MacOS/avb-daemon duplex --seconds 30 --log-level info
```

Read, in order: `selected 16R STREAM_INPUT 1 (Return 1-8)`,
`output format derived`, `answered CONNECT_TX_COMMAND`,
`16R declared Listener Ready`, `gPTP locked ... arming the talker`, then the
two summary lines. The outbound peak distinguishes audio from
correctly-formed silence; `frames placed into the input ring` must be ~48000/s.

The output format is no longer hardcoded (`d01d534`). `duplex`/`play` discover
the 16R, read its CONFIGURATION, every STREAM_INPUT/STREAM_OUTPUT and
AUDIO_UNIT 0 over AECP, and derive both directions' formats from the streams
actually bound. Any failure REFUSES to transmit rather than falling back.

- `--listener-uid` is now OPTIONAL and defaults to **derived**, not 0. The
  16R's STREAM_INPUT 0 is `Return CRF` and STREAM_OUTPUT 0 is `Send CRF` — its
  media clock, not audio — so a fixed 0 reserved and connected a stream that
  carries no audio. Its audio streams are 1..=8 (`Return 1-8` ..
  `Return 57-64`, 8 ch each = 64 channels).
- `--listener <id>` selects which entity to WAIT for rather than skipping
  discovery, because a unicast AECP read needs the 16R's MAC and only an
  observed ADPDU carries it.
- RESOLVED (was an unverified risk): not advertising ourselves during the
  discovery window is FINE. The 16R answers AECP from a controller it has not
  yet discovered — `9 of 9 STREAM_INPUT sinks read` on every run. No bootstrap
  advertisement is needed.

Still unbuilt, and the next likely blockers:

- **The 16R asks us for CONTROL (`0x001a`) index 0 and we answer
  NO_SUCH_DESCRIPTOR** — five times per run. We never advertise CONTROL in our
  `descriptor_counts`, so the refusal is spec-correct and it does NOT block
  binding (`listener_ready=true` with the refusals present). Milan does
  require an IDENTIFY control, so a third-party controller may want it. Not
  urgent, and it must not be guessed: a CONTROL descriptor with an invented
  `control_type` is exactly the unanchored-wire-field mistake `AGENTS.md`
  forbids.
- **Our AEM advertises a talker-only model.** The inbound data path works
  without it (ACMP CONNECT_TX + MSRP Listener Ready do not need us to
  advertise a STREAM_INPUT), but a third-party controller would not see our
  input stream. Merging the listener and talker AEM models into one entity is
  the fix, and is also what CLOCK_SOURCE needs below.
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
  not advertise CLOCK_SYNC_SOURCE. Unaddressed, and the most likely cause of
  audible artefacts over a long outbound run.
- **Only one stream per direction.** We bind STREAM_INPUT 1 / STREAM_OUTPUT 1,
  so 8 channels each way out of the 16R's 64. Binding all eight is the next
  capacity step.

## Verified on hardware at `1e88c37` (duplex, live 16R over the I210)

Two runs, `avb-daemon duplex --seconds 30` and `--seconds 60`, both exit 0:

```
selected 16R STREAM_INPUT 1 (Return 1-8) as our audio sink
output format derived from 16R STREAM_INPUT 1: 0x0205022002006000
  (48000 Hz, 8 ch, 6 spf, 32-bit)
16R AEM: 9 of 9 STREAM_INPUT sinks read, AUDIO_UNIT rate 48000 Hz
16R AEM: 9 of 9 STREAM_OUTPUT sources read
selected 16R STREAM_OUTPUT 1 (Send 1-8) as our audio source
answered CONNECT_TX_COMMAND from the 16R: type 0x00 -> 0x01 status 0
16R declared Listener Ready for our stream — accepted
gPTP locked (asCapable=true offset 85 ns, path delay 144 ns)

talker done:   163007 PDUs on the wire, 0 rate-limited, 0 otherwise refused,
               0 underruns over 30s; outbound peak 788886592 (-8.7 dBFS);
               16R listener_ready=true; AEM answered=51 refused=5;
               ACMP talker commands answered=1; gPTP servo=LOCKED offset 0 ns
listener done: 239267 audio slots, 0 not decodable as AAF, 1435602 frames
               placed into the input ring, 0 format mismatches;
               Listener Ready declared for 1 stream(s), dext filter entries 1
```

- Inbound rate: 1435602 frames / 30 s = **47853/s**, and 2878416 / 60 s =
  **47974/s** — 48 kHz within 0.06 %.
- Outbound peak **-8.7 dBFS** proves the whole CoreAudio -> HAL plug-in ->
  output ring -> AAF -> 16R chain carries real audio, not framed silence.
- The 5 AEM refusals are all CONTROL `0x001a` index 0 (see above) and do not
  prevent binding.

## Product packaging and the companion app (at `22dbef0`)

The five product goals the user set, and where each stands:

1. **Install from a macOS package — DONE.** `scripts/build-pkg.sh` produces
   `build/DreamAVB-<version>.pkg` installing `/Applications/DreamAVB.app`,
   `/Applications/AVBInstaller.app` (which keeps the dext inside it) and
   `/Library/Audio/Plug-Ins/HAL/AVBAudio.driver`. The dext deliberately stays
   inside AVBInstaller.app: macOS only honours an `OSSystemExtensionRequest`
   from the bundle that CONTAINS the extension, so relocating it into
   DreamAVB.app would break the verified activation path. Payload staged with
   `ditto` so the adhoc-signed plug-in stays byte-identical. Bundle relocation
   is pinned OFF via a component plist — `pkgbuild` defaults app bundles to
   relocatable, which would install over a stray copy found elsewhere instead
   of `/Applications`. Verify with `pkgutil --expand`; `PackageInfo` must show
   `<relocate/>` EMPTY.
2. **Driver does nothing until told — DONE.** The postinstall only kickstarts
   coreaudiod and fixes ownership. It must never contain the string
   `activate`; that is worth grepping after any change to it. Taking control
   of the device remains the companion's toggle, which calls
   `avb-installer activate`.
3. **Optional launch at login — DONE.** Hand-written LaunchAgent at
   `~/Library/LaunchAgents/com.avbaudio.companion.plist`, default OFF. NOT
   `tauri-plugin-autostart` — its transitive deps float and break this build.
   Off DELETES the plist. Validated with `plutil -lint`; whether this
   machine's session accepts `launchctl bootstrap` is only confirmed by
   toggling it in the real app.
4. **Optional auto-select NIC — DONE.** Default OFF. Exactly one supported NIC
   is selected; zero or two-or-more selects nothing and says why. It SELECTS
   ONLY — it never activates the driver, so (2) still holds.
5. **Companion starts everything for full duplex — DONE.** `avb-daemon duplex`
   runs BOTH directions in one process, and the companion supervises exactly
   one of them (Audio toggle in the popover). It must be one process: the dext
   permits one `RingAttach` per connection, so a second daemon is refused with
   `kIOReturnExclusiveAccess` and exits `Owned` (9). Both directions also share
   ONE control-ring drain and ONE `GptpService`, because the control ring is a
   single queue — a slot read by one direction is gone for the other.

   Implementation notes that matter if this is touched again:
   - `duplex` is `talker_loop(.., duplex: true)`, not a second loop. The talker
     loop already did every shared piece (drain routing, gPTP, SRP, ADP, AECP,
     clock-source publish); duplex adds the inbound declaration side and the
     audio drain. There is deliberately no second implementation of either
     direction.
   - The inbound data path is `crate::listenerrx`, extracted from
     `streams_loop` and shared with it. `streams_loop` calls the same code, so
     the two cannot drift. Two copies of a wire path is how the wrong
     `WriteMix` FourCC survived five reviews: one copy was never exercised.
   - The inbound drain runs BEFORE the TX readiness gate on purpose. That gate
     waits for OUR servo to lock and `continue`s while waiting, but inbound
     audio does not depend on our clock at all — the 16R stamps its own PDUs.
     Draining first means audio flows IN during the lock wait.
   - The companion stops the daemon with SIGTERM, never SIGKILL: the daemon
     sends an ADP DEPARTING on clean shutdown, and without it the 16R holds a
     stale entry for 2 × valid_time (20 s) that a restarted daemon collides
     with. It escalates to kill only after ~2 s, so a wedged daemon cannot
     leak and hold the rings.
   - `start_audio` refuses unless the driver is Active, because starting
     against the OS default driver fails in a way that reads like a daemon bug.
   - The daemon's own output goes to `~/Library/Logs/DreamAVB-daemon.log`
     (appended, not truncated — the previous run usually explains this one).
     The app has no console to inherit, so without this a failed start is
     silent.
   - UNVERIFIED ON HARDWARE. The duplex loop has never run against the 16R.
     The summary is built so a dead direction cannot look healthy: it reports
     inbound frames PLACED, and warns separately when the 16R never advertised
     a talker (so no Listener Ready was declared) and when Listener Ready was
     declared but not one frame arrived. Those two warnings are the first
     things to read on the first run.

Settings live in `~/Library/Application Support/DreamAVB/config.txt`, keys
`clock_source`, `launch_at_login`, `auto_select_nic`. `avb-config` is
deliberately dependency-free and serde-free: the main workspace pins serde
`=1.0.229` and the Tauri app pins `=1.0.210`, and two exact requirements on
one major version cannot resolve together. Any writer of that file must
load-then-mutate, never construct a fresh `Config` — doing the latter silently
clobbered the other settings (fixed in `22dbef0`).

## Verified at the duplex commit

- `cargo fmt --all --check`: clean (exit 0).
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: clean.
- `cargo test --workspace --locked`: 418 pass.
- Companion app (its OWN cargo workspace): 30 tests, clippy `-D warnings`
  clean, fmt clean. `node --check ui/main.js` OK; every element id main.js
  reaches exists in index.html.
- C++ host tests: 342 i210 checks pass.
- `xcodebuild … AVBI210Driver`: BUILD SUCCEEDED, 0 compiler warnings.
- `scripts/package.sh --adhoc`: packaged; the shipped `avb-daemon` has the
  `duplex` subcommand.
- `scripts/build-pkg.sh`: payload re-verified, `<relocate/>` empty.

## Verified at `22dbef0`

- `cargo fmt --all --check`: clean.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: clean.
- `cargo test --workspace --locked`: 418 pass (avdecc 116 -> 123, daemon 44).
- Companion app (its OWN cargo workspace): 25 tests, clippy `-D warnings`
  clean, fmt clean. `node --check ui/main.js` OK.
- `scripts/build-pkg.sh`: `DreamAVB-284.16.50.pkg`, payload verified by
  `pkgutil --expand`, `<relocate/>` empty, postinstall free of `activate`.

Note: `cargo` is NOT on the default subprocess `PATH` in this environment, and
the rustup default-host is x86_64, so a bare `cargo` resolves the 1.99.0 pin
to the wrong host. Prefix cargo commands with
`PATH="/Users/dexter/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin:$PATH"`.

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
- **Why AMFI specifically, and what would replace it** (checked 2026-10-12):
  the dext claims three *restricted* entitlements —
  `com.apple.developer.driverkit`,
  `...driverkit.transport.pci` (`IOPCIPrimaryMatch 0x15338086`) and
  `...driverkit.allow-any-userclient-access`. Its signature is already
  **valid** (Apple Development → WWDR → Apple Root, `flags=0x0(none)`), and
  `find ... -name '*.provisionprofile'` in `build/AVBInstaller.app` returns
  nothing. So the check AMFI fails is *entitlement authorization* ("restricted
  entitlements, but the validation of its code signature failed"), not
  signature validity. `amfi_allow_any_signature=1` only relaxes signature
  validity, so it is **not** expected to be sufficient; only
  `amfi_get_out_of_my_way=1` (full AMFI disable) bypasses the entitlement
  allowlist.
- **Cost of AMFI off:** with AMFI disabled, TCC cannot attribute a code
  identity, so microphone/camera permission prompts silently fail in unrelated
  apps (documented: OpenCore-Legacy-Patcher issue #380). This is a real,
  known side effect, not a coincidence.
- **The escape route now exists.** Apple changed DriverKit entitlement policy:
  "development" variants of the DriverKit entitlements are available on **any
  paid** developer account with **no Apple approval** (Apple DTS, forums
  threads 718069 / 809202). A paid account ($99/yr) → enable the DriverKit
  capability on the App ID → create a *DriverKit development provisioning
  profile* → embed it in the dext → **AMFI can go back on**. SIP must stay
  disabled plus `systemextensionsctl developer on` (Apple's own documented
  dext dev requirement), but SIP-off alone does not break TCC the way AMFI-off
  does. PCI/USB transport entitlements need the `IOPCIPrimaryMatch` dict added
  to the plist by hand, outside Xcode's capabilities editor.
- **When SIP can come back** (checked 2026-10-12). SIP-off buys exactly one
  thing: `systemextensionsctl developer on` for the dext. It is **not** needed
  for the CoreAudio plug-in — `/Library/Audio/Plug-Ins/HAL` carries no
  `restricted` flag and is absent from `/System/Library/Sandbox/rootless.conf`.
  Three gates, in increasing difficulty:
  1. *AMFI on, SIP off* — paid account + DriverKit development profile. The
     development entitlement variant is a **wildcard** (`*`), so it authorizes
     matching any PCI device and needs no per-vendor grant; the IOKit
     personality still restricts actual matching to 8086:1533 (Apple DTS,
     forums thread 798049).
  2. *SIP on, still developing* — Apple DTS says the newer Xcode-integrated
     signing process replaces "disabling SIP and local signing" (thread
     809202), and the analogous EndpointSecurity dev-profile path is reported
     working with SIP enabled (thread 125508). Undocumented, so prove it.
     Needs: the host app in **`/Applications`** (thread 767152) rather than
     `build/`, and profiles for the APP too — `avb-installer` carries
     `com.apple.developer.system-extension.install`, `avb-daemon` and
     `avb-probe` carry `...driverkit.userclient-access`, all restricted. Dev
     profiles are device-scoped, so this Mac must be registered.
  3. *Shipping* — production `...transport.pci` for 8086:1533, which is
     per-vendor-ID and Apple-approved. Weak: 8086 is **Intel's** VID, not
     ours, on a NIC Apple's own `AppleEthernetE1000` claims, and Apple asks
     about written authorization from the VID owner (thread 842748). One
     USB-transport request sat in "Submitted" for months (thread 845213).
     Do not plan around this.
- **Landmine: re-enabling AMFI may break the CoreAudio plug-in too.** Our
  installed `AVBAudio.driver` is `flags=0x2(adhoc)`, `TeamIdentifier=not set`,
  `spctl: rejected`. Library validation is an AMFI MACF policy
  (`file_check_library_validation`), so the adhoc bundle loads today only
  *because AMFI is off*. Every third-party HAL plug-in on this machine that
  works on a stock Mac is at least Developer ID signed: Waves SoundGrid is
  Developer ID, **not** notarized, no hardened runtime — that is the minimum
  working configuration to copy. Avid's is Developer ID + runtime + notarized.
  So a paid account is needed twice over: a dev profile for the dext, and a
  **Developer ID Application** signature (no hardened runtime) for the HAL
  plug-in. Flip AMFI and the dext before assuming the audio device survives.
- **Provisioning procedure** (prepared 2026-10-12). `package.sh` now embeds a
  profile when one is present at `packaging/profiles/i210-dext.provisionprofile`
  (dext) or `packaging/profiles/installer-app.provisionprofile` (app), and
  prints "entitlements stay UNAUTHORIZED" when they are absent, so the current
  AMFI-off build is unchanged. The directory is gitignored (profiles are
  device- and account-scoped). Facts needed in the portal:
  - Team `8NG8NMGYVF`; cert "Apple Development: armitage787@gmail.com"
    (`D6DED86246DE6B98EB5A31FA4C2F2C240DBDAA92`), expires 2027-08-10.
  - This Mac's **Provisioning UDID** is `00006040-001C30880E20801C`
    (`system_profiler SPHardwareDataType`). Dev profiles are device-scoped.
  - App IDs to register: `com.avbaudio.installer` (app) and
    `com.avbaudio.installer.i210` (dext, must stay a prefix-extension).
  - Profile type is **DriverKit development** (Profiles → + → Development);
    only App IDs with a DriverKit capability are offered it. Apple's steps:
    developer.apple.com/help/account/manage-provisioning-profiles/create-a-driverkit-development-provisioning-profile
  - Embedded profile filename is `embedded.provisionprofile`, confirmed from
    Xcode's own DriverKit platform plug-in
    (`IDEDriverKitSupportCore.xcplugindata`, `embeddedProvisioningProfileName`).
    A dext is a flat bundle so it sits at the bundle **root**; the app's goes
    in `Contents/`. It must be copied **before** `codesign` — it is a sealed
    resource.
  - The dext entitlements file needs **no change**: a wildcard profile permits
    a narrower `IOPCIPrimaryMatch`, and keeping `0x15338086` stays accurate.
  - **Open question:** forum thread 795835 states "There's no 'development
    only' capability for PCI", contradicting the DTS claim of development
    variants for *all* DriverKit entitlements (thread 809202, which also says
    they "allow a DEXT to match against any hardware"). The portal's
    capability list for the App ID decides it; if no PCI development
    capability is offered, fall back to Capability Requests. Resolve this
    before assuming $99 is sufficient.
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
- **`unique_id` 0 on a Milan device is not audio.** The 16R's STREAM_INPUT 0 is
  `Return CRF` and STREAM_OUTPUT 0 is `Send CRF` — its media-clock stream.
  Binding it succeeds at every protocol layer (reservation, ACMP, Listener
  Ready) and moves no audio. Always derive the stream index from the AEM
  (`derive::first_audio_stream`) and refuse when there is no AAF stream, rather
  than defaulting to 0.
- **A talker MUST answer the listener's `CONNECT_TX_COMMAND`.** Sending
  `CONNECT_RX_COMMAND` as the controller is only half of ACMP: the listener
  then asks the TALKER to connect, and only after `CONNECT_TX_RESPONSE` does it
  declare MSRP Listener Ready. We never routed inbound ACMP in the talker loop,
  so the 16R waited on us forever and discarded every PDU — which looks exactly
  like a transmit fault. `avb-daemon`'s summary now reports
  `ACMP talker commands answered`, so "never asked to connect" is
  distinguishable from "asked and then withheld Ready".
- **Descriptor `*_offset` fields are ABSOLUTE from the descriptor's first byte,
  and the array does not follow the fixed part.** The 16R puts STREAM formats
  at byte 132 (not 86) and AUDIO_UNIT `current_sampling_rate` at 136 (not 80 —
  there are 14 `(number_of_*, base_*)` port pairs in between). Reading them
  sequentially gave ten all-zero formats and a healthy 48 kHz unit decoded as
  "0 Hz". Every unit test passed because our encoder wrote the same wrong
  layout our decoder read — a **round-trip tautology**, the same class as the
  wrong `WriteMix` FourCC. The fix is `aem::desc::hw_16r`: decode tests pinned
  to hex captured off the wire by `avb-daemon aem-dump`, which cannot agree
  with a bug in our own layout. Never accept `decode(encode(x)) == x` as
  evidence of wire-format correctness.
- **A protocol success is not an audio success.** A duplex run can report
  Listener Ready, 8000 PDUs/s, zero underruns and zero rate-limited while
  transmitting pure zeros. The talker now reports the outbound peak in dBFS, or
  says "digital silence" outright.

## User decisions

- Listener only (record 16R → Mac) until Phase 1 works. Keep the design talker-ready.
- I210 first. Switches and other mixer models come later.
- Start at 48 kHz. Add 44.1 kHz only if the AEM advertises it. The sample rate must be selectable in a macOS app.
- Receive all of the 16R's output streams. Our daemon is the AVDECC controller, so no Hive is needed.
- gPTP priority1 = 255, so the 16R is grandmaster.
- Pin dependency versions exactly. Commit locally only and never push. Never change boot security or NVRAM yourself; give the user the commands.
