# Phase 1a hardware results

All four Phase 1a hardware milestones pass on the real **PreSonus StudioLive 16R**
over the **Intel I210** (Thunderbolt), driven by our DriverKit dext and the
`avb-daemon`. Full engineering detail and the bugs found are in
`.agents/tasks/phase1a-impl-verification.md`; raw evidence JSON is under
`.agents/tasks/`.

Device: PreSonus StudioLive 16R, entity `0x000a92fffe033ba9`, model
`0x000a92e001080501`, firmware 3.2.0.109659, Milan. 9 talker stream sources,
9 listener stream sinks. Direct cable to the I210, no switch.

## hw-gptp — PASS

gPTP locks to the 16R grandmaster and holds for 5+ minutes:
`state=SLAVE`, `asCapable=true`, servo `LOCKED`, **offset mean 0.05 ns / sd
5.2 ns**, path delay ~138 ns, nrr → 1.0, 0 unlocks. Five hardware-only bugs
fixed (datapath RX flags, mapped-ring RX, inverted TX-timestamp status polarity,
clock-generation seeding/resync, SetClock vs bounded step). Commit `353f662`.

## hw-avdecc — PASS

ADP discovery + full AEM enumeration over AECP READ_DESCRIPTOR: **21/21
descriptors** (entity, configuration, audio unit, 9 × stream input, 9 × stream
output), 0 timeouts, written as `avb-aem-dump/1` JSON. Headline bug: an inverted
AVTP control-header byte order across the whole `avb-avdecc` crate (encode and
decode shared the error, so all unit tests passed while nothing worked on the
wire), plus a missing READ_DESCRIPTOR reserved field. Commit `bcfe889`.

## hw-streams — PASS

Receives all of the 16R's AAF output streams end to end: **8 streams at
7999–8000 pkt/s each (Class A), 0 sequence gaps**, `ring_overrun_audio=0`,
`drop_unbound/untagged=0` (`rx_stream` = 8 × 8000/s). Stream 0 is the 16R's one
IEC-61883/AM824 output (not AAF). Evidence `hw-streams-pass.json`. Three root
causes fixed:

1. **Milan settled-state ordering** (Milan v1.2 tables 5.28–5.30): declare MSRP
   Listener Ready ONLY after the talker registers a Talker Advertise, not
   proactively at bind; establish MVRP VID + MSRP Domain first and wait for the
   16R to register our Domain. This made the strict Milan talker advertise.
2. **AAF decode was fed the whole Ethernet frame** — strip the Ethernet/VLAN
   header before `aaf::decode`.
3. **1 ms drain throttle overran the audio ring** at ~64k pkt/s — busy-drain.

Commits `d6ada09` (streams) and `0d30231` (channel-map tool). The connection
model was reverse-engineered against a known-good Apple AVB capture taken on the
Broadcom NIC (ACMP CONNECT_RX/TX + MSRP/MVRP).

## hw-audio — PASS

600 s multichannel recording of the primary stream to a WAV, with sequence-gap
and media-clock-drift reporting. Evidence `hw-audio-pass.json`:

- **600.0 s, 8-channel 48 kHz 32-bit WAV, 28,799,778 frames (921 MB)**, valid
  RIFF/WAVE.
- **0 sequence gaps** for the full run; `ring_overrun_audio=0`.
- **Media-clock drift −7.8 ppm** (47999.63 Hz vs 48000 nominal) — well inside
  AVB ±100 ppm.
- Channel map verified: 16R input channel 5 → AVB stream `0x000a92033ba9_0001`,
  channel index 4 (0-based); stream 1 carries 16R channels 1–8 in order.

Commit `d3fbfb3`.

## Reproduce

```
# gPTP lock
avb-daemon gptp   --seconds 310 --json-out gptp.json
# AEM dump
avb-daemon aem-dump --out aem.json
# stream packet-rate check
avb-daemon streams --seconds 25 --interval 5 --json-out streams.json
# 600 s recording (feed signal into 16R inputs first)
avb-daemon record  --seconds 600 --out rec.wav --summary-out rec-summary.json
```

Run the signed daemon from the packaged app
(`build/AVBInstaller.app/Contents/MacOS/avb-daemon`); the dext must own the I210
(`avb-installer activate`, replug Thunderbolt if a rematch is needed).
