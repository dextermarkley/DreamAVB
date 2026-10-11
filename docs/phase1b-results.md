<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# Phase 1b results

Phase 1b exposes the recovered 16R audio to Core Audio DAWs through a native
Rust **AudioServerPlugIn** (HAL) driver, in both directions, plus a desktop
companion app. The input direction is verified on the real hardware; the output
direction is fully implemented and gate-green in software and is pending a
hardware session to finalize the two timing-critical bits that can only be wired
against the live 16R.

Device: PreSonus StudioLive 16R, Milan, 9 talker stream sources + 9 listener
stream sinks, direct cable to the Intel I210 (Thunderbolt), no switch.

## 1b-1 — input (16R → Mac → DAW): PASS

Recovered audio reaches Reaper as a clean 8-channel 48 kHz device. Verified
against a live room-mic signal on 16R input channel 5: the recording is a smooth
analog waveform (13k+ distinct sample values, correct clap transients), **zero**
`0x800000`/silence garble.

Two root causes were found by analysing the actual DAW recording (not by reading
code), after a clock-rate theory proved to be the wrong layer:

1. **Int-vs-float ASBD (the garble).** The device advertised
   `kAudioFormatFlagIsSignedInteger` while the modern CoreAudio ASPL model
   expects **Float32** samples. We handed CoreAudio raw S32 bytes, which it
   reinterpreted as `f32`: large negative S32 → huge negative float → clamped to
   `0x800000`; large positive S32 → denormal → `0`. The byte arithmetic
   reproduced the recording exactly. Fix: advertise
   `Float | NativeEndian | Packed` and convert each S32 ring sample to f32
   (`s / 2^31`) in the ReadInput delivery path — matching BlackHole, the
   reference HAL input device. Commit `1075108`.
2. **Hardened-runtime signing (the "device disappeared").** `build-coreaudio-
   plugin.sh` signed the bundle with an Apple Development cert + `--options
   runtime`. Hardened runtime enables library validation in the sandboxed
   `com.apple.audio.Core-Audio-Driver-Service.helper`, which a Development cert
   cannot satisfy without a provisioning profile, so the helper was SIGKILLed at
   `dlopen` (`CODESIGNING / Invalid Page`) and the device silently never
   appeared. Fix: sign **adhoc, no hardened runtime** for a locally-installed
   plug-in. Commit `3384e0b`.

A cross-clock rate correction in `GetZeroTimeStamp` (report the ring's
disciplined media-clock rate instead of nominal 48 kHz) was also added and kept
— it is principled for a device whose data clock is the 16R, not the Mac host —
though it was not the cause of the garble. Simplified in `2a3e262`.

## 1b-2 — output (DAW → Mac → 16R): BUILT, pending hardware

The talker direction mirrors the listener path and is implemented end to end in
software. The 16R advertises 9 listener stream sinks, so it accepts our stream.

Components (all gate-green, unit-tested where sans-IO):

- **Output ring** — `SHM_NAME_OUT` (`/Users/Shared/avb16r-out.ring`), same ABI
  as the input ring run the other way: the plug-in's WriteMix is the producer,
  the daemon's talker loop the consumer. Commit `be10eff`.
- **AAF packetizer** — `avb_avtp::talker::AafTalker` turns S32LE ring frames
  into a complete AAF AVTPDU (common stream header + AAF header + big-endian
  payload), with the per-stream sequence counter and a caller-supplied
  `avtp_timestamp`. Commit `be10eff`.
- **Output HAL stream + WriteMix** — the device now owns two streams; the output
  stream answers direction=output / terminal=Speaker; `write_mix` converts the
  HAL's f32 buffer to S32 and places it into the output ring. Commit `9218a11`.
- **Daemon `play` + ACMP** — `avb-daemon play` creates the output ring, derives
  the Milan AAF format, builds the packetizer, and issues `CONNECT_RX_COMMAND`
  to bind the 16R's STREAM_INPUT to our talker. Commit `78305fb`.
- **TX pacing loop** — one PDU per `spf/rate` interval (125 µs @ 48k/6spf),
  drains the ring, stamps the presentation timestamp (Milan 2 ms floor + one
  125 µs class-A interval), packetizes and transmits; underrun zero-fill,
  jitter catch-up, fault/shutdown/budget handling. Commit `3c5e109`.

Deferred to the hardware session (cannot be done without the live 16R):

- read the dext's disciplined **gPTP-now** for the AVTP presentation timestamp
  (the loop currently uses a mach-time stand-in);
- real **I210 TX-ring hardware timestamping**;
- **MSRP Talker Advertise** for our stream;
- **16R entity-id discovery** to fill `listener_entity_id` in the connect;
- read the **real output format** from the 16R AEM instead of the Milan default.

## Hardware test script (output path)

Run from the repo root, after installing the latest HAL bundle. The daemon
binary must be the entitlement-carrying one from the app bundle.

```sh
# 1. Install the HAL plug-in (adhoc-signed) and reload coreaudiod.
sudo cp -R /Users/dexter/AVB-Audio/build/AVBAudio.driver /Library/Audio/Plug-Ins/HAL/
sudo launchctl kickstart -k system/com.apple.audio.coreaudiod

# 2. Start the talker. Creates the output ring and binds the 16R as listener.
sudo /Users/dexter/AVB-Audio/build/AVBInstaller.app/Contents/MacOS/avb-daemon \
  play --seconds 180 --listener-uid 0 --log-level info

# 3. In the DAW / System Settings, select "AVB StudioLive 16R" as an OUTPUT
#    device and play audio. Confirm it reaches the 16R's physical outputs.
```

Expected: the daemon logs the output ring creation, the `CONNECT_RX_COMMAND`,
then a steady `tx_pdus` count climbing at ~8000/s with few/zero underruns once
the DAW is producing. If audio is silent, check: the output ring exists and the
plug-in's WriteMix is producing (ring write frontier advancing), and the 16R
reports the stream connected.

## Companion app

A desktop menu-bar companion (`apps/dreamavb-companion`, Tauri + Rust) lets the
user hand a NIC to the DreamAVB driver or leave it to the OS default driver,
with live stats under the hood (media-clock lock + drift ppm, ring
under/overruns, stream rate, RX frames, hardware rev — read live from the shared
ring and `avb-probe`). Default policy is hands-off: the driver stays deactivated
until the user enables it. Logic is Rust (reused on a future Linux/Windows
build); the UI is web for cross-platform polish.
