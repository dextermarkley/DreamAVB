# Milan conformance: our talker against the normative spec

> **Status update, 2026-10-13:** this document began before the grandmaster,
> merged entity, MAAP, CRF and unsolicited-notification work landed. The older
> “open gaps” narrative below is retained for history, but it is not a resume
> point. Grandmaster capability, MAAP allocation, both entity directions, real
> counters, IDENTIFY, listener ACMP, START/STOP_STREAMING, CRF transmission and
> state/counter unsolicited notifications are implemented and tested. The live
> product blocker is UC Surface rendering our saved-looking routes as raw entity
> IDs marked `(offline)` even while the 16R reports all nine sinks bound and
> receiving our exact streams. Resume from `HANDOFF.md` and
> `docs/milan-compliance-roadmap.md`.

## The real spec is now local

**`Milan_Specification_Consolidated_v1.3.pdf`** in the repo root (1.9 MB, not
committed). Extract with:

```
pdftotext -layout Milan_Specification_Consolidated_v1.3.pdf .agents/tmp/milan/v13.txt
```

427 KB / 8110 lines of text, versus 33 KB for the old baseline — §5 alone
("Discovery, Connection and Control") is larger than the whole baseline was.
This is the document that **deprecates** the Baseline Interoperability
Specification v2.0a, the Discovery/Connection/Control spec, Base Audio
Formats, Media Clocking and Network Redundancy, and it folds in Corrigendum 1.
Milan normatively references **IEEE 1722.1-2021**.

Clause numbers below that have not been re-checked against v1.3 came from the
withdrawn baseline. The requirements already acted on (MAAP allocation, the
TSPEC table, the presentation-time bound) are fundamental and almost certainly
unchanged, but verify the numbering before citing it.

## Settled against v1.3

**§5.5.1.4 Auto Connect** — *"Once a controller has issued a binding request to
a Listener and received an acknowledgement, it has no further involvement in
the connection process until an unbinding is desired."* With §5.5.2.4: the
Listener stores the binding in non-volatile memory and re-invokes Auto Connect
itself whenever the stream is disrupted, across power cycles of talker,
listener or any switch on the path. So binding is ONE controller action and
re-establishment is the listener's duty — implemented in `ce93c40`.

**§5.5.2.2** — our `CONNECT_RX_COMMAND` *is* the Milan bind command.
`CONNECT_RX_COMMAND/RESPONSE` are renamed `BIND_RX_COMMAND/RESPONSE` (same
PDU), `DISCONNECT_RX` to `UNBIND_RX`, and `CONNECT_TX` to `PROBE_TX`. The wire
format was never wrong.

**§5.5.1.2** — *"This specification does not define the concept of a binding
from the perspective of a talker."* A binding is a property of the listener's
sink, which is why the 16R is the only place its state can be read.

**§5.5.2.7 Talker's behavior** — three requirements, all of which we meet:
*"Talkers do not maintain any internal state related to bound/settled
Listeners"* (`talker_response` is a pure fn), `GET_TX_CONNECTION_COMMAND`
*"is not implemented"* (no arm for it), and `PROBE_TX_COMMAND` *"has no impact
on the internal state of the Talker"* (same pure fn). It also requires Talker
Advertise *"as soon as possible"*, relying *"only on SRP (not ACMP)"* to learn
whether listeners are interested — worth re-checking against our gating of the
declaration on the servo.

## Gaps originally found here, now closed

- Grandmaster-capable gPTP, including BTCA and master Announce/Sync/Follow_Up,
  landed and is hardware-tested while correctly losing to the 16R on the final
  identity tie-break.
- The listener and talker AEM models are merged into one entity with truthful
  STREAM_INPUT and STREAM_OUTPUT descriptors.
- MAAP allocation gates Talker Advertise and has been observed conflict-free on
  hardware.
- CRF is implemented behind `--crf`; the 16R binds its Return CRF sink and
  counts the expected 500 PDUs/s.

MVU BIND/UNBIND and GET_STREAM_INPUT_INFO_EX remain certification work, not a
candidate explanation for the current routing UI: Apple's plain 1722.1 endpoint
is listed without them, and the 16R advertises no controller capability.

## Conformant

| § | Requirement | Our state |
|---|---|---|
| 5.4 | `protocol_version` = 1 in the `GET_MILAN_INFO` response | 1 |
| 5.7.2.1 | SR Class A uses Priority 3 and VLAN ID 2 after startup/link-up | `DEFAULT_DOMAIN`, observed on the wire as `vlan=2` |
| 5.7.2.1 | MSRP Domain messages not gated on gPTP state | declared independently of the servo |
| 6.2 | Talker joins the VLAN via MVRP before sending stream frames | MVRP VID declared first |
| 6.3.2 | TSPEC `MaxFrameSize` = `24*N + 24 + 1` (AAF PCM32 48 kHz) | 217 for N=8 — fixed, was 234 |
| 6.3.2 | TSPEC `MaxIntervalFrames` = 1 | 1 |
| 6.5.1 | Stream destination MAC inside the MAAP dynamic pool | `91:E0:F0:00:xx:xx`, fixed in `65f5676` |
| 6.5.1 | MAAP implemented, addresses **allocated** by it | `avb-avtp/src/maap.rs`, layout transcribed from the 16R's own announce |
| 6.3.1 (1) | Destination address allocated and conflict-free before declaring a Talker attribute | arming gated on `Maap::acquired()` |
| 7.2.1 | Presentation time offset at most 2 ms | 2 ms exactly — fixed, was 2.125 ms |
| 7.2.2 | Stream format matches the listener's `STREAM_INPUT` descriptor | byte-identical to the 16R's own PDU on the wire |

## Violations still open

**§6.3.1 condition 2 — Talker attributes are declared unconditionally.**
We declare regardless of whether a `PROBE_TX_COMMAND` arrived in the last 15
seconds or a matching Listener attribute is registered. This wastes network
resources rather than being invalid, so it is lower priority than the rest.

**§6.4 — no credit-based shaper.** "A Talker PAAD shall implement the CBS" and
"shall shape each individual stream, as well as the overall SR class". We pace
in software on a 125 µs tick, and the dext has a token-bucket rate limiter,
which is not a CBS. Probably harmless on a direct cable with one talker;
real on a switched network.

**§6.5.2 — `tu` bit is hardcoded 0.** Should track the servo per
[AVTP 4.4.4.7]. Defensible today only because we refuse to stream unless the
servo is LOCKED, so the bit is never *wrong* — but it is not derived.

## PreSonus-specific, outside the spec

PreSonus's own procedure for this exact pairing is
["How to connect your Series III console running firmware v3.1+ to a Mac over AVB"](https://support.presonus.com/hc/en-us/articles/38925260615437-How-to-connect-your-Series-III-console-running-firmware-v3-1-to-a-Mac-over-AVB).
Three things in it matter to us:

1. **Routing an AVB stream in is not enough.** After assigning the stream you
   must "enable [Network] input-type on the channels of the StudioLive itself
   to receive audio signal from the Mac". An assigned-but-not-switched channel
   stays silent.
2. **The network clock light turns BLUE when clocked correctly** — a hardware
   readout we can ask the user for, independent of any counter.
3. Their clocking table agrees with our configuration: with the StudioLive on
   `PTP Clock` (which its AEM reports), the Mac clocks from `Input Stream 1`,
   i.e. recovers its media clock from the incoming stream. That is what
   `avb-mediaclock` does.

Also worth knowing when judging the "(offline)" label: PreSonus says this
pairing is "not officially supported" and that "macOS does not yet fully adhere
to the Milan standard", and Apple's own ATDECC controller warns that a Milan
entity "may not be compliant with IEEE 1722.1". So some of what we are fighting
is a vendor boundary, not necessarily our bug.
