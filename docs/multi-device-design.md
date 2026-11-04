# Multiple AVB devices: design

Status: **draft, decisions pending** (see "Open decisions"). Nothing here is
implemented yet beyond the device rename.

## Goal

One DreamAVB Core Audio device carries the streams of every connected AVB
device (16R first, then 24R and others). The user maps device streams onto
DreamAVB's input channels. Two rules govern the map:

1. **The configuration never changes without the user knowing.** No channel
   moves, appears or disappears in the DAW unless the user did it, or the app
   told them and they accepted.
2. **A device leaving does not move anything.** Its channels stay where they
   are, reserved and silent, and resume when it returns.

## What the code assumes today (inventory)

All references are to `crates/avb-daemon/src/stack.rs` unless stated.

**Discovery: exactly one peer.**
- `discover_listener` (~7487) waits for `--listener <id>` or the *sole*
  entity on the wire (`table.sole_talker()`). With two entities and no
  `--listener` it exits `TalkerNotFound`: "N entities discovered; pass
  --listener <id>". A second device today stops the daemon from starting.
- `talker_loop` builds one `AecpPeers { target, target_mac }` (~2653) and
  every AECP read (`derive_talker_format`, `derive_inbound_sources`,
  `read_stream_ids`) uses it.

**Per-device state held as singletons in the duplex loop.**
- `sink_uids` / `out_streams`: our STREAM_OUTPUTs, one per *that device's*
  audio sink, `uid = i`, stream id = our MAC + uid (~2770).
- `inbound_uids` / `inbound_stream_ids`: that device's STREAM_OUTPUTs.
- `connect_listener(..., listener_id, ...)` binds every one of our streams to
  that one listener (~3000), plus the CRF stream to its Return CRF sink.
- The connection watch, GET_COUNTERS diagnostics and `listener_ready` all
  address `peers.target`.

**Ring layout: device index = ring slot.**
- `avb_audioring::DEVICE_CHANNELS = 64`, `CHANNELS_PER_STREAM = 8`; the plug-in
  publishes a fixed 64-channel device and refuses a ring of any other width.
- Input: `ListenerRx::with_streams(inbound_stream_ids)` puts `expected[i]` on
  ring channels `i*8 .. i*8+8` (`listenerrx.rs`), i.e. 16R Send 1-8 on 1-8.
- Output: `out_streams[i]` gathers ring channels `uid*channels ..` (TX gather
  in the talker loop).

**Clock.**
- The media clock DLL follows the first timestamped inbound stream
  (`mclk_slot`, `listenerrx.rs`). The clock-source setting `16r`
  (`avb-config`, `ClockSource::SixteenRStream`) means "the device's stream".
- gPTP: we are slave to whichever grandmaster wins BMCA (the 16R today).

**Config.** `~/Library/Application Support/DreamAVB/config.txt`, `key=value`
(`avb-config`): `clock_source`, `launch_at_login`, `auto_select_nic`. Nothing
per-device.

**Companion app.** One "Audio running" toggle, a clock-source picker, NIC
selection. No device list, no mapping UI. Tauri commands in
`apps/dreamavb-companion/src-tauri/src/lib.rs` (`status`, `stats`,
`get/set_clock_source`, `start/stop_audio`, ...).

**Also single-device by construction.**
- MAAP destination addresses are derived from our MAC + stream uid
  (`avb_core::maap_stream_dest`); uids restart at 0, so streams for a second
  device need uids that do not collide with the first device's.
- The AEM we advertise has one STREAM_OUTPUT per 16R sink (8 audio + CRF).
  More devices means more STREAM_OUTPUTs, and the AEM is static per run.

## Proposed model

**Device identity** is the AVDECC `entity_id` (EUI-64), stable across reboots
and IP/link changes. Each known device has a user-visible label (entity name
from its AEM, editable) and its model id.

**Input map** is a persisted list of assignments:

```
input <first-channel> <entity-id> <stream-output-index> [<count>]
```

meaning: that device's STREAM_OUTPUT `index` lands on DreamAVB inputs
`first .. first+count`. Stored with the rest of the config, written only by an
explicit user action in the app (or by accepting the app's offer for a new
device).

**Runtime behaviour.**
- The daemon discovers *all* entities, enumerates each one's AEM, and binds
  the streams that appear in the map. Unmapped streams are not bound.
- A mapped device that is absent: its channels read silence. Nothing shifts.
- A device that returns: rebound into the same channels.
- A new, unmapped device: nothing is mapped automatically; the app shows it
  as "new device" with a proposed placement in free channels for the user to
  accept (pending decision 2).
- A map entry whose stream no longer exists on the device (device
  reconfigured) is shown as an error in the app, never silently remapped.

**Device width** is fixed (pending decision 1) so the DAW's channel list
never changes when devices come and go.

**Daemon structure.** `talker_loop` becomes a loop over a set of per-device
sessions (`DeviceSession { entity, aem, inbound streams, outbound streams,
sinks, connection state }`) sharing one dext, one gPTP service and one ring
pair. ADP runs continuously: an `ENTITY_AVAILABLE` for a mapped device starts
its session; `ENTITY_DEPARTING` or timeout ends it and silences its channels.

## Open decisions

1. Fixed device width (recommended: 128 in / 128 out) versus growing it with
   the user's sign-off.
2. A new unmapped device: show and wait for assignment, or auto-assign to free
   channels and notify.
3. Per-channel mapping, or whole streams (8 channels) at an offset first.
4. Outputs: map them now too, or inputs only for the first version.
5. Clock: which device's stream (or gPTP) drives the media clock when several
   are present; proposed: a user-chosen "clock device", defaulting to the
   first mapped device, never switching without telling the user.

## Planned order of work

1. Device rename to "DreamAVB" (done; UIDs unchanged on purpose).
2. Pure, tested map model in `avb-config`: parse/serialize, "never moves"
   invariants, free-channel proposal.
3. Daemon: multi-entity discovery and per-device sessions, still mapping one
   device, verified on the 16R alone.
4. Ring/plug-in width change to the chosen fixed width.
5. Companion: device list and mapping UI.
6. Hardware: 16R + 24R together, then remove/re-add each to verify rule 2.
