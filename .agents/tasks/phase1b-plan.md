# Phase 1b plan — the macOS product (CoreAudio device + app)

Branch `phase1a-listener` (continuing). Never push.

Goal: turn the proven Phase 1a listener into a complete macOS product — the 16R
appears as a native multichannel **input** device in CoreAudio apps (Reaper,
Pro Tools), with a small status/rate app, driven by the daemon. All Rust except
the C++ dext. Grounded in `avb-hw-feasibility.md` ("Recommended Phase 1 scope")
and `phase1a-requirements.md` §4.8–§4.9 (ring + clock handoff).

## Dependency-ordered steps

### 1b-1 Live audio pipeline through the ring + clock recovery  [daemon]
The `record`/`streams` loop currently decodes AAF ad hoc. Replace that with the
real path the plugin consumes:
- Build an `avb_audioring::AudioRing` (shared memory, ABI v1) sized from the
  bound streams (channels, rate from the AEM).
- Build `avb_daemon::audio::AudioEngine` (MediaClock + Producer + BoundStreams)
  and feed every AAF slot through `on_packet`.
- Drive `avb_mediaclock::MediaClock` on the reference stream; publish
  `ClockModel` into the ring's clock block seqlock.
- `record` consumes via `avb_audioring::Consumer` (same interface the plugin
  uses) and writes the WAV, so the ring path is exercised end to end.
- Expose the ring over POSIX shared memory (`shm_open`) with a stable name so a
  separate process (the plugin) can map it.

### 1b-2 Daemon control API  [daemon]
A Unix-domain socket (JSON lines) the plugin and app use: get device config
(channels, available rates from AEM, current rate), get status (gPTP lock,
asCapable, per-stream state, clock model), request rate change (drives the
AVDECC SET_* + re-reservation), and the shm ring name/handle.

### 1b-3 CoreAudio AudioServerPlugIn  [new crate `avb-coreaudio`, Rust cdylib]
Input-only `.driver` bundle:
- `extern "C"` factory + `AudioServerPlugInDriverInterface` vtable
  (`coreaudio-sys` bindings; patterns from NullAudio / libASPL / Ozzy).
- One device, channel count + rates from the daemon control API, S32/F32.
- Maps the shm ring; `IO` thread reads committed frames by the clock model's
  zero-timestamp period (derived from `ClockModel`, not re-recovered).
- Rate change via `RequestDeviceConfigurationChange`.
- Install to `/Library/Audio/Plug-Ins/HAL/`; validate in Reaper then Pro Tools.

### 1b-4 macOS status/rate app  [new crate `avb-app`, Rust]
Menu-bar/status app: gPTP lock, asCapable, stream state, media-clock ppm/lock,
rate selector (rates from AEM). Talks to the daemon control API.

### 1b-5 Service + lifecycle  [packaging]
Daemon as a launchd user/daemon service: auto start, connect to the 16R on
appearance, survive unplug/replug (binding re-probe already designed), expose
the ring + control socket for the plugin/app.

### 1b-6 Latency + results  [verification]
Measure HW-timestamp→ring latency p50/p99/max (NFR-2). Write
`docs/phase1b-results.md`.

## Out of Phase 1b (Phase 2)
Talker/playback (AAF TX, 802.1Qav shaping, LaunchTime, CRF), switches, multiple
mixers, distribution signing (DriverKit provisioning profile + notarization).

## Gate (unchanged)
fmt, clippy -D warnings, cargo test, C++ host tests, dext build 0 warnings,
package.sh. Hardware claims from real runs only.
