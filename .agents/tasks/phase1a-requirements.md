# Phase 1a Requirements: AVB listener bridge (StudioLive 16R to Mac over the Intel I210)

Branch `phase1a-listener` (from `phase0b-i210-dext` 4035832). Never push.
Inputs: `docs/phase0b-i210-results.md`, `.agents/tasks/avb-hw-feasibility.md` (Q6, Q7, "Sample rates", "BCM57762 listener-only design"), `.agents/tasks/phase0b-plan.md` (incl. Deviations), `.agents/tasks/phase0b-build-verification.md`, `driver/AVBShared.h` (ABI v2), `driver/AVBI210Driver/`, `crates/avb-probe`, `crates/avb-installer`.

This document says what must be true at the end of Phase 1a. It does not choose designs. Where it says "the design must state X", the design document owns the decision.

## 1. Summary

Phase 1a gets multichannel audio from a PreSonus StudioLive 16R (Milan firmware 3.3.0.109659) into the Mac over AVB. It uses our own I210 PCIDriverKit dext (`com.avbaudio.installer.i210`) and a new Rust user-space daemon. Apple's AVB stack is not involved.

The Mac is a listener only:
- gPTP slave with the 16R as grandmaster.
- MSRP/MVRP listener.
- AVDECC controller plus a Milan-acceptable listener entity.
- AAF receiver for every 16R STREAM_OUTPUT.
- Media-clock recovery against mach_absolute_time.
- A `record` command that writes a multichannel WAV.

The output feeds Phase 1b, a CoreAudio AudioServerPlugIn plus a macOS app. The protocol libraries and the recovered-audio ring must be reusable there.

Baseline proven in Phase 0b on this exact hardware. Treat these as facts, not open questions:
- Device and port identities:
  - I210 8086:1533 / 1ab6:0214 rev 3, MAC 64:4b:f0:12:73:b8, link 1000FD direct to the 16R.
  - 16R gPTP port identity 00:0a:92:ff:fe:03:3b:a9 / port 1.
- Register values:
  - `TSYNCRXCTL` 0x19, `ETQF3` 0x440088F7, `SRRCTL0` 0xC2000402.
  - TIMINCA golden value 0x00346DC6 for +100 000 ppb; the I210 limit is |ppb| ≤ 488 281.
- Timestamping:
  - In-buffer RX timestamps (TSIP) arrive on every 0x88F7 frame.
  - The TX timestamp latch (TXSTMPL/H) works, with one timestamped TX outstanding at a time.
- Measurements:
  - Link delay is 142.6 ns compensated (TX 178 / RX 448 ns at 1000 Mb/s) and 455.6 ns raw, sd 2.5 ns.
  - NRR is 0.999916820 (sd 0.006 ppm).
  - SYSTIM runs about +75 ppm vs mach, which puts the 16R at about −8 ppm vs mach.
- 16R Pdelay behavior: it sends its own Pdelay_Req at 1 s intervals and answers ours with a correctionField of 0.

The Phase 0 dext data path doesn't scale to audio, and Phase 1a must replace it. It currently has:
- a 2 ms timer harvest;
- a 128-descriptor RX ring;
- a 256-entry software ring;
- a 128 B copy per frame;
- a 24-frame inline batch per user-client call.

An AAF frame at 8 ch × 6 samples × 4 B is about 234 B on the wire without FCS: 14 B Ethernet + 4 B VLAN tag + 24 B AAF header + 192 B payload. Several streams at 8000 pkt/s each arrive at tens of thousands of packets per second.

## 2. Conventions

- IDs: `SAF-n` (safety), `ENV-n` (environment), `DP-n` (dext data path), `PTP-n` (gPTP), `SRP-n` (MSRP/MVRP), `ADC-n` (AVDECC), `AAF-n` (AVTP/AAF), `MCR-n` (media-clock recovery), `RING-n` (recovered-audio ring), `CRT-n` (crates and build), `CLI-n` (daemon CLI and record), `VER-n` (verification and results).
- Verification tag on every requirement:
  - **[HW]**: proven only by a run on the real I210 + 16R. A code read or a unit test does not satisfy it.
  - **[CODE]**: proven by build, unit/host tests, static inspection or review.
  - **[HW+CODE]**: needs both. Typically a unit-tested algorithm whose real-world result must also be measured.
- "16R AEM" means the descriptors the 16R returns at run time. Wherever a value comes from the 16R AEM, a hardcoded constant does not satisfy the requirement.

## 3. Hardware unknowns (resolved only by probing the 16R)

The design must not assume an answer to any of these. Each one must be recorded, with measured evidence, in `docs/phase1a-results.md` (VER-3).

| ID | Unknown | Why it matters |
|---|---|---|
| U-1 | The 16R entity model: entity_id, entity_model_id, number of CONFIGURATIONs, current configuration, descriptor counts per type | Drives the AEM walk, the JSON dump and the AEM of our listener entity |
| U-2 | Number of STREAM_OUTPUT descriptors (talker streams) and how many can stream at once | Sets the stream count, ring sizing (DP-6) and WAV channel count |
| U-3 | Per stream: current_format and the `formats` list (AAF subtype, format INT_32BIT vs INT_24BIT, bit_depth, channels_per_frame, samples_per_frame); STREAM_PORT/AUDIO_CLUSTER/AUDIO_MAP channel names | Our STREAM_INPUT formats must match exactly. Sets the WAV layout |
| U-4 | AUDIO_UNIT current_sampling_rate and the `sampling_rates` list (is 44.1 kHz offered?) | Rate support. Phase 1a runs at 48 kHz only |
| U-5 | CLOCK_DOMAIN current clock source (PTP, Internal, Input Stream CRF) and the CLOCK_SOURCE list | Decides whether the media clock is tied to gPTP time. It also governs what MCR-5 can compare |
| U-6 | Milan revision the 16R implements (1.1, 1.2, …) and the gPTP flavor (802.1AS-2011 vs -2020 behavior: Signaling / gPTP-capable TLV, Announce and Sync intervals, path trace) | Which commands and messages our side must support for the 16R to accept us |
| U-7 | Milan ACMP binding flow the 16R expects from a listener: PROBE_TX/CONNECT_TX semantics, GET_TX_STATE, whether it validates our entity (ADP fields, AEM reads, MVU GET_MILAN_INFO) before it accepts | Our entity can be bound by ACMP (ADC-13, ADC-14) |
| U-8 | The 16R's MSRP behavior: does it declare Talker Advertise before any listener exists? Which Domain values (class A id 6 / priority 3 / VID 2, or others)? Stream dest MACs (MAAP-allocated 91:E0:F0:00:xx:xx) and VLAN ID? Does it stream only after Listener Ready? | SRP-1…SRP-6 |
| U-9 | AAF timing parameters: presentation-time offset, `tv` set on every PDU or sparse mode (`sp`), `mr` behavior, MSRP accumulated latency | Late/early classification (AAF-6) and MCR |
| U-10 | Whether the 16R accepts an entity_id / entity_model_id that isn't under a registered vendor OUI (we have no OUI) | ADC-7 |
| U-11 | Whether the 16R's Announce and Sync carry the GM identity we expect, and its Sync interval (expect 125 ms) | PTP-3, PTP-7 |
| U-12 | How the I210 presents VLAN-tagged AVTP frames to software with our configuration (tag kept in the buffer vs stripped into the descriptor) | DP-3 |
| U-13 | Whether the 1000 Mb/s latency constants (TX 178 / RX 448 ns, not in datasheet 333016) are right. An independent cross-check is the 16R's own propagation-delay figure (GET_AVB_INFO) | PTP-9, VER-1 |

## 4. Functional requirements

### 4.1 Safety and environment

- **SAF-1 [CODE]** The dext matches, opens and touches only PCI 8086:1533 with subsystem 1ab6:0214. The Phase 0b triple enforcement (personality, entitlement, `Start` ID check) stays in place. Host tests keep rejecting the BCM57762, the I211 and I210s with other subsystems.
- **SAF-2 [CODE]** The dext writes only registers documented in Intel datasheet 333016, only in named sequences.
  - There is no generic register-write selector.
  - Every new register and bit used in Phase 1a cites its 333016 section in `I210Regs.h`. A deviation from this document or the design is recorded with the section that forced it.
- **SAF-3 [CODE]** Every new hardware poll is bounded.
  - An all-ones read, or a poll timeout, latches a fault. After that the dext refuses MMIO and DMA until `Stop`.
  - The fault code is visible to the daemon.
- **SAF-4 [HW+CODE]** On a latched dext fault, a link loss, or a device removal, the daemon:
  - stops issuing hardware operations;
  - reports the fault code;
  - exits or idles with a non-zero status.

  It never retries register writes in a loop or "tries other values". [HW] Demonstrate at least once with a cable pull or an adapter unplug: the daemon reports it cleanly and the dext doesn't panic or wedge.
- **SAF-5 [CODE]** Nothing in Phase 1a runs sudo, `csrutil`, `nvram`, `bputil`, or changes boot security. Code and scripts must not contain such calls. `systemextensionsctl` is used only for read-only listing.
- **SAF-6 [CODE]** The daemon sends only low-rate control frames:
  - gPTP Pdelay_Req, Pdelay_Resp and Pdelay_Resp_Follow_Up, plus anything else U-6 shows is mandatory;
  - MSRP and MVRP PDUs;
  - ADP, AECP and ACMP.

  It sends no AVTP stream data. The design states the worst-case TX frame rate, and the dext enforces an upper bound on it.
- **SAF-7 [CODE]** GPL/LGPL boundary.
  - No code, comments, structs or tables are copied from GPL or LGPL sources: Linux igb, OpenAvnu kernel parts and igb_avb, L-Acoustics avdecc (LGPL), or GPL parts of any other project.
  - Those sources may be read only to learn behavior.
  - Protocol implementations follow the IEEE standards and the public Avnu Milan specification.
- **ENV-1 [CODE]** All scratch, logs, captures, WAV files and intermediate build output go under `/Users/dexter/AVB-Audio/.agents/tmp/`. Nothing goes to `/tmp`, `/private/tmp` or the system `$TMPDIR`.
  - Tool commands set `TMPDIR=/Users/dexter/AVB-Audio/.agents/tmp/`.
  - Commands don't use `cd`/`pushd`. They use `cargo --manifest-path`, `git -C`, `make -C`, and `xcodebuild -project` with `SYMROOT`/`OBJROOT` under `build/`.
- **ENV-2 [HW]** The daemon runs as the logged-in user without sudo, with access to the dext user client through the signed bundle's entitlements, like `avb-probe` today.

### 4.2 Dext data path (C++, `driver/AVBI210Driver`, `driver/AVBShared.h`)

- **DP-1 [CODE]** The user-client ABI version increases (to 3 or later).
  - The C++ header and the Rust mirror assert identical sizes and offsets for every shared structure, including the shared-memory ring layout.
  - On a version mismatch, the daemon refuses to run and prints both versions.
- **DP-2 [HW+CODE]** Received frames reach the daemon through shared memory that the dext exports with `IOUserClient::CopyClientMemoryForType` and the daemon maps. The per-packet path needs no user-client call.
  - The Phase 0 polled single-frame `kAvbSelPollRx` path may remain for diagnostics, but it isn't the audio path.
  - The design states the per-packet copy count between DMA buffer and daemon, and how producer/consumer indices are published without locks.
- **DP-3 [HW+CODE]** Each delivered frame:
  - contains the complete frame up to the configured maximum length, at least 1522 B with a VLAN tag (no truncation);
  - carries the VLAN ID and PCP when the frame was tagged, either in the frame bytes or in per-frame metadata (U-12);
  - carries an arrival timestamp and a flag saying which timebase it is in: I210 SYSTIM ns from the in-buffer timestamp, or dext harvest mach time;
  - carries enough status to detect RX errors.
- **DP-4 [HW+CODE]** The dext delivers these frame classes to the daemon and no others:
  - AVTP stream data: Ethertype 0x22F0, VLAN-tagged, to the destination MACs of streams the daemon has bound;
  - gPTP: 0x88F7 to 01:80:C2:00:00:0E;
  - MSRP: 0x22EA;
  - MVRP: 0x88F5 to 01:80:C2:00:00:21;
  - AVDECC: 0x22F0, subtypes ADP/AECP/ACMP and MAAP, to 91:E0:F0:01:00:00, 91:E0:F0:00:xx:xx (including MAAP 91:E0:F0:00:FF:00) and our unicast MAC.

  Frames of other classes (for example the 16R's IP/mDNS traffic) are discarded and counted.
- **DP-5 [HW+CODE]** gPTP event frames keep their hardware RX timestamp in SYSTIM ns, as proven in G3. Phase 1a must not regress any G3 property: every Pdelay_Req and Sync carries a timestamp, with no repeats.
- **DP-6 [HW]** Capacity and loss:
  - At 8000 pkt/s per stream × every 16R STREAM_OUTPUT bound at once (U-2), plus control traffic, over a 10-minute run, the dext reports zero hardware RX misses (I210 missed-packet/no-descriptor counters) and zero ring overruns.
  - The design states:
    - the maximum stream count S_max the rings are sized for;
    - the headroom in milliseconds at S_max (at least 50 ms of traffic, so a daemon scheduling stall shorter than that loses nothing);
    - what happens when the 16R AEM reports more streams than S_max. The daemon must refuse to bind the excess with a clear error, not drop silently.
- **DP-7 [HW+CODE]** Overflow of stream-data delivery must never lose gPTP, MRP or AVDECC frames. A full audio path cannot starve control.
- **DP-8 [CODE]** Each counter is exposed to the daemon and monotonic for the life of the datapath:
  - frames delivered per class;
  - frames discarded by filter;
  - ring overruns per ring;
  - hardware RX missed / no-descriptor counts;
  - RX errors.
- **DP-9 [HW+CODE]** TX path for low-rate control frames:
  - frames of 60–1514 B;
  - an optional hardware TX timestamp on gPTP event messages (Pdelay_Req and Pdelay_Resp);
  - the one-outstanding-timestamp limit of the TXSTMP latch is respected;
  - a TX that can't get a timestamp reports a status, never a stale value.

  [HW] Every Pdelay_Req and Pdelay_Resp we send over a 5-minute run gets a valid TX timestamp, or its failure is counted. The design states the expected failure rate (target 0).
- **DP-10 [CODE]** Clock operations stay available alongside the rings:
  - SYSTIM read with mach bracket, set and frequency adjust.
  - The dext enforces the I210 |ppb| ≤ 488 281 limit.
  - If the servo needs an offset adjustment that the Phase 0 ABI lacks (a time step or an atomic add), it is a named, bounded selector, not a generic write.
- **DP-11 [HW+CODE]** Client lifecycle.
  - Only one data-path client at a time.
  - If the daemon exits or crashes, the dext stops writing into that client's shared memory before the memory is released.
  - The dext stays healthy, and a new daemon instance can open, map and receive again without a replug or reboot.
  - [HW] Shown at least once by killing the daemon with SIGKILL during streaming and restarting it.
- **DP-12 [CODE]** The BCM dext source keeps compiling (compile check only; it needn't gain ring support). `avb-probe` keeps building. Its `info`, `clock`, `adjfreq` and `pdelay` commands either work against the Phase 1a dext, or the design lists each retired command and why.

### 4.3 gPTP (IEEE 802.1AS, slave-only)

- **PTP-1 [CODE]** Our time-aware system never becomes grandmaster.
  - priority1 = 255 in any Announce we send; the design decides whether we send Announce at all, per U-6.
  - BMCA always prefers the 16R.
  - Our clockIdentity is a stable EUI-64 derived from the I210 MAC.
- **PTP-2 [HW+CODE]** Peer delay works in both directions:
  - We send Pdelay_Req at the 802.1AS interval and compute the neighbor propagation delay and neighbor rate ratio from t1–t4 with the 1000 Mb/s latency compensation. Raw and compensated values are both available.
  - We answer every 16R Pdelay_Req with Pdelay_Resp (hardware t2/t3) and Pdelay_Resp_Follow_Up. Each response leaves within the 802.1AS turnaround limit; the design cites the clause and value.
- **PTP-3 [HW+CODE]** We receive Sync/Follow_Up (two-step) and Announce from the 16R and compute offset-from-master.
  - The computation uses the hardware RX timestamp, the Follow_Up preciseOriginTimestamp, the correctionField, the neighbor propagation delay and the rate ratio.
  - The GM identity is taken from Announce, and a GM change is detected and counted.
- **PTP-4 [HW+CODE]** A servo disciplines I210 SYSTIM to the GM through TIMINCA frequency adjustment.
  - The clock is stepped only at initial acquisition, or when |offset| exceeds a threshold the design states. Every step is counted and reported.
  - Adjustment saturation at ±488 281 ppb is detected and reported.
- **PTP-5 [HW+CODE]** asCapable is computed per 802.1AS for our port: neighbor delay below the 800 ns threshold, Pdelay exchanges succeeding, no multiple-responder condition. Every transition is logged.
- **PTP-6 [HW+CODE]** The daemon provides a model mapping disciplined gPTP time to mach_absolute_time, from bracketed SYSTIM reads.
  - The model reports its own uncertainty, from bracket width and residuals.
  - It is updated continuously, so MCR can convert presentation times to mach time.
- **PTP-7 [HW]** Lock: asCapable is true and the servo is in its locked state within 60 s of the daemon start with the link up (assumed target; the design may justify another number). Over a run of at least 5 minutes it then stays locked with no steps and no asCapable drop.
- **PTP-8 [HW]** Offset-from-master stability over at least 5 minutes after lock, reported as mean and stddev. Assumed pass targets, to be confirmed or adjusted with justification in the design: |mean| ≤ 100 ns and stddev ≤ 100 ns.

  The offset is self-reported by our servo, so the results doc also reports the independent cross-checks in VER-1.
- **PTP-9 [HW]** Path delay stays in Phase 0b's range: compensated about 140 ns, raw about 456 ns, both inside 140–460 ns. The NRR stays consistent with Phase 0b: about −83 ppm, unless the hardware changed.
- **PTP-10 [CODE]** gPTP status is available to the CLI and as JSON:
  - port state, asCapable, GM identity, offset, path delay (raw and compensated), NRR, cumulative rate ratio and servo frequency;
  - counters: Sync, Follow_Up, Announce, Pdelay rx/tx and timeouts.

### 4.4 MSRP / MVRP (IEEE 802.1Q)

- **SRP-1 [HW+CODE]** We declare the MSRP Domain attribute with the values the 16R declares (U-8). The default is SR class A: class id 6, priority 3, VID 2. A mismatch between our values and the 16R's is detected and reported.
- **SRP-2 [HW+CODE]** We register and decode the 16R's Talker Advertise (and Talker Failed) attributes, keeping every field:
  - stream ID, destination MAC, VLAN ID;
  - TSpec MaxFrameSize and MaxIntervalFrames;
  - priority/rank;
  - accumulated latency;
  - for Talker Failed, the failure information.
- **SRP-3 [HW+CODE]** For each stream bound to our listener, we declare Listener Ready for that stream ID while a Talker Advertise is registered. Listener Asking Failed is used where 802.1Q requires it, for example when the talker declares Talker Failed. We withdraw the declaration when the stream is unbound.
- **SRP-4 [HW+CODE]** MVRP: we declare membership of the stream VLAN(s) taken from the Talker Advertise / Domain.
- **SRP-5 [HW+CODE]** MRP behavior follows 802.1Q:
  - applicant and registrar state machines;
  - join, leave and leaveAll timers;
  - re-declaration after LeaveAll;
  - declarations stay in place for a 10-minute run with no gap that stops the stream.
- **SRP-6 [HW]** Evidence for each bound stream: the 16R's Talker Advertise attributes, our Listener Ready declaration and the times of both, logged and in the results JSON. Where the 16R exposes it (Milan GET_STREAM_INFO on its STREAM_OUTPUT), the 16R's view that a listener registration exists.
- **SRP-7 [CODE]** On clean shutdown we withdraw every MSRP and MVRP declaration.

### 4.5 AVDECC / ATDECC (IEEE 1722.1 + Milan): controller role

- **ADC-1 [HW+CODE]** ADP discovery:
  - We send ENTITY_DISCOVER and track ENTITY_AVAILABLE / ENTITY_DEPARTING from every entity.
  - We detect the 16R, its entity_id, its available_index changes (reboot detection) and its departure.
- **ADC-2 [HW+CODE]** AEM enumeration: READ_DESCRIPTOR on the 16R reads every descriptor of every CONFIGURATION, by index and type, including at least:
  - ENTITY, CONFIGURATION, AUDIO_UNIT, STREAM_INPUT, STREAM_OUTPUT, AVB_INTERFACE;
  - CLOCK_SOURCE, CLOCK_DOMAIN, STREAM_PORT_INPUT/OUTPUT, AUDIO_CLUSTER, AUDIO_MAP, LOCALE/STRINGS, CONTROL.

  The enumeration tolerates descriptor types it doesn't decode: it records the raw bytes.
- **ADC-3 [HW+CODE]** Dynamic state queried from the 16R:
  - GET_SAMPLING_RATE (AUDIO_UNIT) and GET_STREAM_FORMAT (each STREAM_OUTPUT);
  - GET_STREAM_INFO (each STREAM_OUTPUT);
  - GET_CLOCK_SOURCE (CLOCK_DOMAIN);
  - GET_AVB_INFO and GET_AS_PATH (AVB_INTERFACE);
  - GET_COUNTERS (AVB_INTERFACE, STREAM_OUTPUT, CLOCK_DOMAIN where supported);
  - the Milan MVU GET_MILAN_INFO where supported.

  A NOT_IMPLEMENTED or NOT_SUPPORTED response is recorded, not treated as fatal.
- **ADC-4 [HW+CODE]** AECP robustness:
  - command timeouts and retries per 1722.1;
  - sequence-ID matching;
  - IN_PROGRESS handling.

  A missing response is reported per command. It doesn't hang the enumeration.
- **ADC-5 [HW+CODE]** We register for unsolicited notifications on the 16R (REGISTER_UNSOLICITED_NOTIFICATION) and process them. A rate, format, clock-source or stream-info change made on the mixer is logged and reflected in the daemon's state. Phase 1a needs no automatic re-bind after a rate change; see Out of Scope.
- **ADC-6 [HW]** AEM dump: one command writes the complete 16R entity model to a JSON file. It contains:
  - the entity (IDs, names, firmware version, serial, capabilities);
  - every configuration and its descriptor counts;
  - for each AUDIO_UNIT, the current and supported sampling rates;
  - for each STREAM_OUTPUT and STREAM_INPUT:
    - the current format and every supported format, both decoded (subtype, format, rate, bit depth, channels per frame, samples per frame) and as raw u64;
    - the channel count;
    - the stream name;
  - clock sources and the current clock domain source;
  - AVB_INTERFACE (MAC, clock identity, gPTP fields);
  - channel names and mapping from STREAM_PORT/AUDIO_CLUSTER/AUDIO_MAP;
  - the dynamic state from ADC-3;
  - raw descriptor bytes, hex, for each descriptor.

  The file is stored as `/Users/dexter/AVB-Audio/.agents/tasks/phase1a-aem-16r.json`.

### 4.6 AVDECC / ATDECC: our listener entity

- **ADC-7 [HW+CODE]** Our entity advertises ADP ENTITY_AVAILABLE:
  - with a stable entity_id and entity_model_id (U-10; the design states how they're derived);
  - with AEM_SUPPORTED, listener capabilities including AUDIO_SINK, the listener stream sink count, the current gPTP grandmaster ID and domain, and an available_index per 1722.1;
  - re-advertised before valid_time expires;
  - answering ENTITY_DISCOVER;
  - sending ENTITY_DEPARTING on clean shutdown.
- **ADC-8 [HW+CODE]** Our AEM contains at least:
  - ENTITY, one CONFIGURATION, AUDIO_UNIT, AVB_INTERFACE, CLOCK_SOURCE(s) and CLOCK_DOMAIN;
  - one STREAM_INPUT per 16R STREAM_OUTPUT we bind, plus the stream ports, clusters and maps Milan requires for a listener.
- **ADC-9 [HW+CODE]** No hardcoding.
  - Each of our STREAM_INPUT `formats` lists contains the exact AEM stream-format value(s) the matching 16R STREAM_OUTPUT advertises, and its current_format equals the 16R's current format.
  - Our AUDIO_UNIT sampling rates and our channel counts are derived from the 16R AEM, read before our entity's model is finalized.
  - The design states how our entity behaves before the 16R is discovered, and how entity_model_id stays consistent when the derived model changes.
- **ADC-10 [HW+CODE]** Our entity answers READ_DESCRIPTOR for each of its descriptors, and responds correctly to the AEM commands Milan mandates for a listener (U-6), at minimum:
  - ACQUIRE_ENTITY / LOCK_ENTITY, as Milan specifies;
  - ENTITY_AVAILABLE;
  - GET_CONFIGURATION;
  - GET/SET_STREAM_FORMAT;
  - GET_STREAM_INFO, including Milan's extended flags for a bound STREAM_INPUT: bound/connected state, talker entity and unique ID, stream ID, dest MAC, VLAN;
  - GET/SET_NAME;
  - GET/SET_SAMPLING_RATE;
  - GET/SET_CLOCK_SOURCE;
  - REGISTER/DEREGISTER_UNSOLICITED_NOTIFICATION;
  - GET_AVB_INFO;
  - GET_AS_PATH;
  - GET_COUNTERS on STREAM_INPUT, AVB_INTERFACE, CLOCK_DOMAIN and ENTITY as Milan requires;
  - GET_AUDIO_MAP;
  - MVU GET_MILAN_INFO.

  Any other command gets NOT_IMPLEMENTED, never silence.
- **ADC-11 [HW+CODE]** Our STREAM_INPUT GET_COUNTERS reports the Milan listener counters, sourced from the AAF-5 counters:
  - FRAMES_RX
  - SEQ_NUM_MISMATCH
  - LATE_TIMESTAMP and EARLY_TIMESTAMP
  - MEDIA_LOCKED and MEDIA_UNLOCKED
  - STREAM_INTERRUPTED
  - MEDIA_RESET
  - UNSUPPORTED_FORMAT
  - TIMESTAMP_UNCERTAIN
- **ADC-12 [CODE]** SET_* commands that change state while a stream is bound are rejected with the status Milan specifies. SET_STREAM_FORMAT on a bound STREAM_INPUT is refused.
- **ADC-13 [HW+CODE]** ACMP, listener side:
  - Our entity accepts CONNECT_RX_COMMAND (BIND_RX) and DISCONNECT_RX_COMMAND (UNBIND_RX) for its STREAM_INPUTs from any controller, including our own daemon and Hive.
  - It performs the talker-side exchange Milan specifies with the 16R (U-7: PROBE_TX / CONNECT_TX and GET_TX_STATE semantics) to learn stream ID, dest MAC and VLAN.
  - It answers GET_RX_STATE.
  - Binding state survives transient talker loss: the listener re-probes when the 16R reappears.
- **ADC-14 [HW+CODE]** ACMP, controller side: the daemon binds each of our STREAM_INPUTs to the corresponding 16R STREAM_OUTPUT by sending CONNECT_RX_COMMAND on the wire to our listener entity, so an external controller sees the same binding. Every 16R STREAM_OUTPUT is bound (U-2), not just the first.
- **ADC-15 [HW]** Acceptance by the 16R:
  - The 16R completes the ACMP exchange for every stream with SUCCESS.
  - Its GET_STREAM_INFO / GET_TX_STATE shows our listener, where it exposes that.
  - Hive (optional, for debugging only) lists our entity and shows the bindings as connected. The pass criterion must not depend on Hive.

### 4.7 AVTP / AAF (IEEE 1722) receive

- **AAF-1 [CODE]** The parser handles the AAF PCM header:
  - subtype 0x02; sv, mr, tv, tu, sp;
  - sequence_num, avtp_timestamp, stream_id;
  - format, nsr, channels_per_frame, bit_depth, stream_data_length, evt.

  It also handles 32-bit int and 24-in-32 big-endian payloads, and the format values the 16R AEM uses (U-3).
- **AAF-2 [HW+CODE]** Each frame is assigned to a bound stream by stream_id. The stream's dest MAC and VLAN must match its Talker Advertise.
  - Frames whose stream_id isn't bound, or whose header doesn't match the bound format (format, nsr, channels, bit depth, data length), are counted per reason and not written as audio.
- **AAF-3 [HW]** Rate: each bound stream receives 8000 pkt/s within ±100 ppm, measured over the run. The expected count is 8000 × run seconds × (1 + the 16R's clock offset vs our reference). The per-stream measured rate is reported.
- **AAF-4 [HW+CODE]** Sequence handling:
  - 8-bit wrap;
  - gaps counted as the number of missing packets;
  - duplicates and reordering counted separately.

  [HW] Zero sequence gaps per stream over 10 minutes (VER-2).
- **AAF-5 [CODE]** Per-stream counters, available live and in the results JSON:
  - packets;
  - sequence gaps (missing packets) and gap events;
  - duplicates and out-of-order;
  - late and early timestamps;
  - tv=0 packets;
  - mr toggles;
  - format mismatches;
  - stream_id mismatches;
  - first and last packet times.
- **AAF-6 [HW+CODE]** Late/early classification compares avtp_timestamp (presentation time, low 32 bits of gPTP ns, with wrap handling) to the packet's arrival time in gPTP time.
  - "Late" means the presentation time has already passed on arrival.
  - "Early" means presentation is further ahead than the stream's maximum transit time: the design states the bound, derived from the Talker Advertise accumulated latency or the Milan presentation-time offset (U-9).

  [HW] Report both counts per stream for the 10-minute run.
- **AAF-7 [HW+CODE]** Cross-stream alignment: samples from different streams with the same presentation time land at the same sample-frame index in the recovered-audio ring and in the WAV. Arrival order doesn't decide alignment. A stream joining late or recovering from a gap is placed by presentation time, and the missing span is zero-filled and counted.

### 4.8 Media-clock recovery

- **MCR-1 [HW+CODE]** From (presentation time, sample index) pairs of a reference stream, the daemon recovers the stream's media clock.
  - The reference choice follows the 16R's clock domain. The design states which stream is the reference and why.
  - The output is a model giving the mach_absolute_time of any sample-frame index, plus the sample rate in Hz relative to mach.
- **MCR-2 [CODE]** Recovery handles:
  - 32-bit avtp_timestamp wrap (about 4.295 s);
  - tv=0 or sparse-mode packets (U-9);
  - mr toggles (media clock restart: the model resets and the reset is counted);
  - packet loss with no discontinuity in the sample index.
- **MCR-3 [HW]** Over the 10-minute run, the daemon reports:
  - the recovered rate vs mach in ppm (relative to the nominal rate from the AEM);
  - its stddev over fixed windows (the design states the window);
  - the residual jitter of the model vs observed presentation times.

  Assumed stability target: stddev of 10 s window estimates ≤ 0.5 ppm. The design may revise it with justification.
- **MCR-4 [HW+CODE]** Model state is published (rate, phase, uncertainty, locked/unlocked, reset count) together with the recovered-audio ring, so Phase 1b can derive its zero-timestamp period without re-implementing the recovery.
- **MCR-5 [HW]** Consistency check. When the 16R's clock source is PTP (U-5), the recovered media clock's ppm vs mach must agree with the gPTP-derived GM-vs-mach rate. A disagreement above the bound the design states is a finding in the results doc. Phase 0b suggests about −8 ppm. When the clock source isn't PTP, the results doc records that the check doesn't apply.

### 4.9 Recovered-audio ring (Phase 1b handoff)

- **RING-1 [CODE]** The audio ring is lock-free: single producer (the daemon's stream path), single consumer.
  - The producer never blocks and never allocates on the per-packet path.
  - Overrun and underrun are detected and counted, never silent.
- **RING-2 [CODE]** The ring's memory layout is a versioned, fixed-layout ABI that can live in shared memory a separate process maps (Phase 1b's AudioServerPlugIn). Phase 1a's `record` command consumes through the same interface it will use then.
  - The layout has a header with channel count, sample format, nominal rate, capacity, and indices in sample frames.
  - The layout is documented, and size/offset-tested in Rust.
- **RING-3 [CODE]** Each frame position in the ring maps to a sample-frame index on the recovered media clock (MCR-1), so a consumer can turn ring position into mach time. Gaps are zero-filled with a recorded gap marker or counter (AAF-7).
- **RING-4 [CODE]** Channel count, channel order and sample format in the ring come from the bound streams and the 16R AEM (stream order × channel order), not from constants.

### 4.10 Rust crate layout and build

- **CRT-1 [CODE]** The workspace gains a binary crate `crates/avb-daemon`, with binary name `avb-daemon`, plus separate library crates or clearly separated modules for:
  - dext access (ABI mirror and IOKit binding);
  - gPTP;
  - MSRP/MVRP;
  - AVTP/AAF;
  - AVDECC (ADP/AECP/ACMP/AEM);
  - media-clock recovery;
  - the recovered-audio ring.

  The design fixes the names and boundaries. Phase 1b must be able to depend on the protocol, clock and ring libraries without pulling in the daemon binary.
- **CRT-2 [CODE]** Protocol and clock logic is testable on the host without the dext or the network. Encoders and decoders, state machines and servo/recovery math run under `cargo test` with synthetic or recorded frames. Each protocol has round-trip and known-vector tests: AAF header decode, AEM stream-format decode, MRP attribute encode/decode, gPTP message encode/decode, and offset/delay math.
- **CRT-3 [CODE]** Every dependency, new or existing, is pinned exactly (`=x.y.z`) in its Cargo.toml.
  - `Cargo.lock` is committed. Builds and tests run with `--locked`.
  - Each new dependency has a permissive license (MIT, Apache-2.0, BSD, ISC, Zlib or similar), and the design lists each one with its license. No GPL/LGPL crates.
- **CRT-4 [CODE]** The build gate passes:
  - `cargo fmt --check`;
  - `cargo clippy --workspace --all-targets --locked -- -D warnings`;
  - `cargo test --workspace --locked`;
  - `make -C /Users/dexter/AVB-Audio/driver/tests test`;
  - both dext `xcodebuild` builds with 0 warnings;
  - `scripts/package.sh --adhoc`.
- **CRT-5 [CODE]** Packaging embeds `avb-daemon` in the signed bundle with the user-client access entitlement for `com.avbaudio.installer.i210`, the same way `avb-probe` is embedded today. Only the I210 dext is embedded and activated.
- **CRT-6 [CODE]** Talker-ready: nothing in the ABI, the ring layout, the AEM model or the MSRP layer may prevent adding a talker later. The design names where talker support would attach. No talker code, audio TX or Qav shaping is built.

### 4.11 Daemon CLI and record

- **CLI-1 [HW+CODE]** `avb-daemon` provides subcommands to:
  - run the full listener stack;
  - dump the 16R AEM to JSON (ADC-6);
  - monitor gPTP for N seconds with summary statistics (PTP-8);
  - show live and summary per-stream counters (AAF-5);
  - record (CLI-2).

  The design fixes the names except `record`. Each one exits non-zero with a clear message when the dext service is missing, the ABI mismatches, there's no link, or the 16R isn't found within a stated timeout.
- **CLI-2 [HW]** `avb-daemon record --seconds <N> --out <path>`, run as `--seconds 600 --out /Users/dexter/AVB-Audio/.agents/tmp/16r.wav`:
  - discovers and binds all 16R output streams (or uses an already-running stack, as the design decides);
  - records N seconds;
  - writes one multichannel WAV with every channel of every bound stream at the AEM rate (48 kHz) and 32-bit integer samples (24-in-32 if the AEM format is 24-bit);
  - prints and writes a JSON summary: per-stream counters, gPTP stats, media-clock ppm, and the WAV channel map.
- **CLI-3 [CODE]** The WAV file is valid:
  - WAVE_FORMAT_EXTENSIBLE (needed for more than 2 channels and for 24-in-32), with correct valid-bits and block alignment;
  - samples converted from network byte order bit-exactly (unit test with synthetic AAF payloads).

  If the projected size exceeds the 4 GiB RIFF limit, the command either writes a standard 64-bit variant (RF64/BW64) or refuses before recording starts. The design picks one. It never writes a corrupt file. Free disk space is checked before recording.
- **CLI-4 [CODE]** A sidecar JSON next to the WAV maps each WAV channel to stream index, stream_id, channel within the stream, and the AEM channel/cluster name where available.
- **CLI-5 [HW+CODE]** Clean shutdown on SIGINT/SIGTERM:
  - unbind or withdraw: SRP-7 and ADC-7 ENTITY_DEPARTING;
  - finish the WAV header;
  - release the dext data path.

  A following run starts without a replug.

### 4.12 Verification and results

- **VER-1 [HW]** gPTP evidence (≥ 5 minutes after lock):
  - offset mean and stddev, path delay raw and compensated, NRR, asCapable timeline, step count;
  - independent cross-checks where the 16R provides them: its GET_AVB_INFO propagation delay and as_capable, and GET_AS_PATH.

  A difference of more than 50 ns between its delay and ours is recorded as a finding (U-13), not a silent pass.
- **VER-2 [HW]** Streaming evidence from a 10-minute `record` run, for every bound stream:
  - packets, rate, sequence gaps (must be 0), late, early, format mismatches;
  - the dext counters from DP-8: hardware misses and ring overruns must be 0;
  - media-clock ppm vs mach and its stddev;
  - the WAV channel count, duration, file size, and the peak and RMS level per channel. Silence is expected and acceptable unless the user feeds a signal.

  Where the 16R's STREAM_OUTPUT GET_COUNTERS is available, its frames-TX count is compared with our packets-RX count.
- **VER-3 [HW]** `docs/phase1a-results.md`, in the style of `docs/phase0b-i210-results.md`, contains:
  - host, firmware and dext version;
  - a gate table with the measured numbers;
  - the AEM summary: entity, configuration count, stream count, channels per stream, current and supported rates, formats, clock source;
  - resolutions of U-1…U-13;
  - known issues and deviations;
  - Phase 1b readiness: ring ABI, clock model, open risks.
- **VER-4 [HW]** Raw outputs live under `/Users/dexter/AVB-Audio/.agents/tasks/` (small JSON and logs: AEM dump, gPTP stats, record summary, counters). Large artifacts (WAV, packet captures) stay in `.agents/tmp/` and are referenced by path, size and checksum.
- **VER-5 [CODE]** `.agents/tasks/phase1a-build-verification.md` records every CRT-4 command with exit code and key output, as Phase 0b did. Nothing in it is claimed as hardware proof.
- **VER-6 [HW]** The hardware step:
  - activates the new dext version itself (package, `avb-installer activate`);
  - pauses for the user only when approval or a replug is needed;
  - records any sysextd/kernelmanagerd error text and code verbatim.

  The Broadcom stays on Apple's driver throughout.

## 5. Non-functional requirements

- **NFR-1 [HW]** Daemon CPU at full stream load: assumed budget ≤ 15 % of one core on average over the 10-minute run, to be confirmed by the design. Measured and reported. Dext CPU is reported if observable.
- **NFR-2 [HW+CODE]** End-to-end delivery latency, from frame arrival (hardware timestamp) to the sample being available in the recovered-audio ring, is measured and reported as p50/p99/max. The design states a target that leaves margin inside the 16R's presentation-time offset (U-9), since Phase 1b depends on it.
- **NFR-3 [CODE]** The per-packet daemon path does no heap allocation, no blocking I/O and no logging. Logging happens off the hot path, rate-limited.
- **NFR-4 [CODE]** Logs use a consistent prefix or subsystem, so the hardware step can filter them. The dext keeps the `AVBI210:` prefix.
- **NFR-5 [CODE]** All on-wire encodings are covered by tests against byte-exact vectors derived from the standards. Captured 16R frames, saved under `.agents/tasks/` or test fixtures, are added as regression vectors once available.

## 6. Acceptance criteria

Each item must be shown with the evidence named in section 4. Items marked [HW] need real-hardware evidence; a code read does not count.

1. **[CODE]** The build gate (CRT-4) passes, and `phase1a-build-verification.md` exists (VER-5).
2. **[CODE]** ABI version bumped. C++/Rust size and offset assertions pass, including the ring layout (DP-1, RING-2).
3. **[CODE]** All dependencies are pinned `=x.y.z` with permissive licenses listed. No GPL/LGPL code copied (CRT-3, SAF-7).
4. **[CODE]** Device scope and register-safety review passes (SAF-1, SAF-2, SAF-3). No sudo, NVRAM or boot-security calls anywhere (SAF-5).
5. **[HW]** gPTP: the 16R is GM, and our port reaches asCapable = true and locked within 60 s (PTP-7). Over ≥ 5 min:
   - offset mean and stddev are reported and meet PTP-8, or the deviation is justified in the results;
   - zero steps after lock, and asCapable never drops;
   - compensated path delay is about 140 ns and raw about 456 ns, inside 140–460 ns (PTP-9);
   - every Pdelay_Req and Pdelay_Resp we sent got a TX timestamp, or failures are counted (DP-9).
6. **[HW]** AVDECC: the 16R is discovered, and `/Users/dexter/AVB-Audio/.agents/tasks/phase1a-aem-16r.json` contains the full AEM listed in ADC-6. That includes the audio-unit rates (current + supported), every STREAM_OUTPUT with current and supported formats and channel counts, and clock sources.
7. **[HW]** Our entity: the 16R completes ACMP for every stream with SUCCESS (ADC-15). Our entity answers every Milan-mandatory listener command it receives during the run, with no AECP timeouts on the 16R's side as far as it reports them (ADC-10).
8. **[HW]** Streams: every 16R STREAM_OUTPUT is bound to one of our STREAM_INPUTs (ADC-14). Each shows Talker Advertise → our Listener Ready in the MSRP log (SRP-6), and receives 8000 pkt/s ± 100 ppm (AAF-3).
9. **[HW]** `avb-daemon record --seconds 600 --out /Users/dexter/AVB-Audio/.agents/tmp/16r.wav` exits 0 and writes:
   - a valid WAV with every channel of every bound stream, at the AEM rate (48 kHz), 32-bit (or 24-in-32), lasting 600 s ± 1 frame-period tolerance as the design states (CLI-2, CLI-3);
   - the sidecar channel map (CLI-4).
10. **[HW]** Over that 10-minute run, for every stream: 0 sequence gaps, 0 dext hardware misses and 0 ring overruns. Late and early counts are reported (AAF-4, AAF-6, DP-6).
11. **[HW]** Media clock: the ppm vs mach and its stddev are reported and meet MCR-3. MCR-5 agreement is reported, or marked not applicable with the clock source recorded.
12. **[HW]** Robustness, each shown once:
    - a daemon SIGKILL and restart during streaming recovers without a replug (DP-11);
    - SIGINT gives a clean shutdown with declarations withdrawn and ENTITY_DEPARTING sent (CLI-5);
    - a cable pull is reported cleanly, with no dext fault cascade (SAF-4).
13. **[HW]** `docs/phase1a-results.md` exists with every item in VER-3, including the resolution of each unknown U-1…U-13. Raw outputs are under `.agents/tasks/` (VER-4).
14. **[CODE]** Rates, channel counts, stream counts and formats are not hardcoded. A review shows each comes from the 16R AEM at run time (ADC-9, RING-4). A unit test runs the pipeline with a synthetic AEM of a different stream count and channel count.

## 7. Out of scope (Phase 1a)

- Talker or audio TX of any kind, CRF transmission, and 802.1Qav shaping or LaunchTime. The code stays talker-ready (CRT-6).
- The CoreAudio AudioServerPlugIn, the macOS app, and the XPC/Unix-socket control API (Phase 1b). Phase 1a only provides the ring and the clock model they will use.
- Changing the sampling rate (SET_SAMPLING_RATE on the 16R, or rate-change re-bind sequencing) and 44.1 kHz streaming. Phase 1a runs at 48 kHz, but it must report whether the AEM offers 44.1 kHz.
- Being gPTP grandmaster, BMCA among more than two systems, and multiple gPTP domains.
- Switches, multiple mixers, and talkers other than the 16R. The stack must not break if other entities are present, but nothing beyond the 16R is tested.
- Exposing the I210 as a macOS network interface (NetworkingDriverKit) and LAN passthrough.
- MAAP address allocation by our side (we allocate no stream addresses as a listener).
- Interrupt-driven RX: allowed, but not required. The design chooses polling or interrupts against DP-6 and NFR-2.
- Any BCM57762 work beyond keeping its dext compiling.

## 8. Assumptions (to be confirmed by the design review or hardware)

- A-1: The 16R remains the only other AVB endpoint, direct-cabled at 1000FD, and its clock source stays at its default (PTP) unless the user changes it.
- A-2: Numeric pass targets this document invents are starting points: the 60 s lock time, PTP-8 offset bounds, MCR-3 0.5 ppm, NFR-1 15 % CPU, DP-6 50 ms headroom, the VER-1 50 ns cross-check. The design may tighten or relax them with justification. The user-stated criteria in section 6 (asCapable, 140–460 ns, 8000 pkt/s, 0 gaps over 10 min, ≥ 5 min stats, the WAV format) are fixed.
- A-3: We have no registered OUI. The entity_id and entity_model_id are derived from the I210 MAC, or use a locally administered scheme. If the 16R rejects that (U-10), it is a blocker to report, not to work around silently.
- A-4: The public Avnu Milan specification (v1.1 or v1.2, whichever matches U-6) is the reference for listener-mandatory commands and ACMP flow. Where 1722.1-2013/2021 and Milan differ, Milan behavior wins for interop with the 16R.
- A-5: If the 16R exposes more output streams than it can transmit at once (U-2), "all streams" means all it will stream simultaneously. The results doc records the limit and the reason.
