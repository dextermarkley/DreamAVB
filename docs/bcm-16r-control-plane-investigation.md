# Broadcom path: the 16R's control plane dies under 8-stream TX

Investigation record, 2026-11-06 to 2026-11-07, branch `fix/driver-deactivation`.
Status: **open, root cause not found.** Read this before touching the Broadcom
(BCM57762, Apple Thunderbolt Gigabit adapter) TX path again.

## Symptom

With the Broadcom dext streaming 8 AAF streams (8 ch, 48 kHz, 8000 frames/s
each, 64 000 frames/s total, plus CRF and control) to the StudioLive 16R over
a **direct cable**, the 16R's control plane goes silent after 30–545 s:
ADP, AECP and ACMP stop first, Sync about 3 s later, its Pdelay answers last.
The daemon's `[peer]` line then reports `0 ADP, 0 AEM` every 10 s and gPTP
goes LOCKED -> HOLDOVER. Only a 16R power cycle recovers it.

The I210 with the same daemon build survives 600 s on the same direct cable.
The Broadcom survives 600 s through the MOTU AVB switch.

How to tell a run's outcome (read the log only after the run finishes):

- `grep -c '0 ADP, 0 AEM' <log>`: silent 10 s windows. 0 = survived.
- `grep -o 'servo=[A-Z]*' <log> | uniq -c`: LOCKED throughout vs HOLDOVER.
- `talker done:` line: PDUs, missed intervals.
- On death the daemon writes a flight-recorder pcap of the last 4096 control
  frames: `~/Library/Logs/DreamAVB-flightrec-*.pcap`.

The time of death is random. One 600 s survival proves little (run A below
survived and the identical run B died).

## Run procedure

1. Power-cycle the 16R (authorized any time), cwd `/Users/dexter/python-kasa`:
   `export KASA_HOST=10.0.0.92; uv run kasa off && sleep 5 && uv run kasa on`.
   MOTU switch is `10.0.0.107`. Wait ~90 s.
2. Start detached:
   `nohup /bin/sh -c "$D --nic bcm duplex --seconds 600 > $L 2>&1; echo \"daemon exit \$?\" >> $L" >/dev/null 2>&1 & disown`
   with `D=/Users/dexter/AVB-Audio/build/AVBInstaller.app/Contents/MacOS/avb-daemon`.
3. Optional PHY polling every 30 s: `/bin/sh .agents/tmp/phy_poll.sh <out>`.
4. Poll with `pgrep -f "avb-daemon --nic bcm duplex"`; read results after exit.

A test hook set by env var applies only if the datapath is brought up by that
run. `AVB_DEBUG_PHY_MASTER` shuts an up datapath down itself; for other
datapath-level changes run `avb-probe --nic bcm datapath down` first.

## Results (all direct cable unless noted, each after a 16R power cycle)

| Run | Change | 16R | Notes |
|---|---|---|---|
| I210 control | same daemon on the I210 | survived 600 s | 173 missed intervals |
| BCM 0 streams (`AVB_DEBUG_NO_AUDIO_TX`) | no audio | OK | |
| BCM 1 stream (`AVB_DEBUG_TX_STREAMS=1`) | | OK | |
| BCM 4 streams | | OK | |
| BCM 8 streams | baseline | dies 30–545 s | many runs |
| BCM 8 via MOTU switch | switch re-spaces and re-PHYs our traffic | survived 600 s | needed the rx-latency recalibration (`bab85d8`) or the switch withholds Sync |
| EEE off (dext 309.23.9) | EEE was active, 63 us LPI timer | died ~30 s | EEE kept off |
| rx latency recal (`bab85d8`) | | died ~160 s | |
| `AVB_DEBUG_TX_SPLIT=2`, spin-wait version | 2 x 4-frame sub-bursts | survived | confounded: rate fell to 46 861 fps, 1.23M missed |
| `AVB_DEBUG_TX_SPLIT=2`, paced (`d755087`) | | survived twice | 61 100–61 500 fps, ~2.9% missed |
| dext doorbell pacing (310.5.18, `cd6a1d2`, reverted in `2318835`) | descriptors at once, doorbell per 4 frames from a TX-queue timer | died ~400 s | 62 700 fps, 0.1% missed; 1.8% of sub-bursts merged (timer late up to 859 us) |
| run A, dext 310.6.51, auto MASTER/SLAVE | resolved Broadcom = MASTER | survived 600 s | |
| run B, `AVB_DEBUG_PHY_MASTER=1` | forced MASTER | died ~265 s | same role as run A |
| NRR fix (`a699b52`) | rate ratio now tracks | died ~135 s | |

Ruled out as the cause: frame bytes, burst shape on the wire, EEE, pause
frames, multicast hash collisions, latency calibration, PHY errors (16R ->
Broadcom direction), PHY MASTER/SLAVE role, gPTP clock quality, the frozen NRR,
our Pdelay t2/t3 timestamps, anything new in our control traffic before death.

## Measurements

### Wire captures (no 16R; Broadcom cabled to the I210)

- Broadcom -> I210 (I210 RX hardware timestamps; `avb-probe tx-burst` +
  `avb-daemon --nic i210 capture --streams 8`): 635 368 / 635 368 frames,
  0 byte mismatches, 0 sequence breaks, frames inside a burst exactly
  back-to-back (2064 ns for 234 B at 1 Gb/s).
- I210 -> Broadcom (Broadcom RX-CPU stamper; `tx-burst --ptp` +
  `avb-probe stamp-ring`): 8 x 234 B bursts arrive 1984–2240 ns apart, span
  14.4 us = 7 x 2064 ns. Mixed lengths confirm the stamps track wire time.
  **The I210 sends the same back-to-back 8-frame trains.** Burst shape is not
  what separates the NICs.

### PHY

`avb-probe --nic bcm phy` now decodes local/remote receiver OK, idle errors,
receive errors and false carrier. Over full runs (live and dying): all 0 after
link-up (one false-carrier 255 reading at link-up). Register 0x14 reads
`0xffff`; its meaning on this PHY is unverified. We cannot see the 16R's own
receive counters.

### gPTP comparison (healthy windows only; `.agents/tmp/clock_compare.py`)

Same in surviving and dying runs: servo LOCKED within a few hundred ns, 80 Sync
per 10 s, compensated path delay 0.1 us, 16R Pdelay turnaround ~25–29 us.
Differences between NICs that do not track death: our Pdelay turnaround
0.92 ms (I210) vs 1.23–1.54 ms (Broadcom); NRR was frozen on the Broadcom
(fixed, see below). The Broadcom loses inbound intervals from the 16R in groups
of 8 (8–40 per run) in surviving runs too; a separate RX-path drop, not the
trigger. Inbound "forward jumps" begin exactly at each death (a symptom).

The 16R's Pdelay turnaround is NOT an early warning: in run B it stayed normal
until the death and only rose afterwards.

### Flight recorder (3 deaths: pcaps 1794014753, 1794031814, 1794035721)

Scripts: `.agents/tmp/pdelay_analyze.py <pcap> 38:c9:86:46:7e:8b`,
`ctl_rate.py`, `aem_burst.py`.

- Our Pdelay_Resp t2 advance 1000.100 ms ±20 us per exchange, corrections 0,
  no jumps before death.
- In every death the **16R's own 1 s Pdelay_Req timer slips first**
  (1000.1 ms -> 1000.6–1005 ms per interval), and in the same second it stops
  answering ACMP and AECP. Sync stops ~3 s later. It looks like the 16R's CPU
  is progressively overloaded, not reacting to one message.
- Our control traffic is steady through the death: 8 ACMP/s, 16 unsolicited
  GET_COUNTERS every 2–3 s, REGISTER_UNSOL every 10 s, nothing new.

## Code changes made during the investigation (all committed)

- `a699b52` gPTP: the BCM software clock step called
  `on_clock_stepped(now, 0)`, which returns early because the BCM generation
  never changes, so the NRR window kept pre-ACQUIRE points and every later
  exchange was rejected. New `Port::on_soft_step`; the window also restarts
  after 3 consecutive rejections. Clock line logs `nrr_win`, `nrr_held`,
  `nrr_rejected`, `step_discards`, `gen_changes`. Real bug, not the cause.
- `e27dab1` dext: `AVB_INIT_FLAG_PHY_MASTER` (MII reg 9 bits 12/11, read first
  and logged, cleared when not requested); daemon hook `AVB_DEBUG_PHY_MASTER`;
  probe PHY counter decode.
- `2318835` reverts the dext doorbell pacing (`cd6a1d2`).
- `39c81cb`, `192b610` tools: `avb-probe tx-burst [--ptp --lens]`,
  `avb-probe stamp-ring`, `avb-daemon capture --streams N` (I210 hardware
  timestamps, stripped VLAN tag restored).
- `d755087`, `eb22126` daemon `AVB_DEBUG_TX_SPLIT` (kept as a test hook).
- Earlier: EEE off (`9992ea5`), ReadPhy/ReadPhyMmd, TX queue + txLock,
  yieldToTx, doorbell guard, flight recorder, `AVB_DEBUG_TX_STREAMS`,
  `AVB_DEBUG_NO_AUDIO_TX`.

## How Apple's driver transmits AVB (behavior reference)

Read-only disassembly of `BCM5701Enet` in
`/System/Library/KernelCollections/BootKernelExtensions.kc` (x86_64 slice,
the kext's own symbol table). Tools: `.agents/tmp/re_syms.py <substr>` lists
symbols; `.agents/tmp/re-venv/bin/python3 .agents/tmp/re_func.py <substr>`
disassembles functions (capstone lives only in that venv);
`re_bcm.py <hex offsets>` finds register uses. Full dumps:
`.agents/tmp/re-all.txt`, `re-wakeup.txt`, `re-tx.txt`, `re-startavb.txt`.

**Apple sends AVB on a second send ring with a per-packet transmit time.** Our
dext sends everything untimed on ring 0. (The I210 path is untimed too and
survives, so this is not proven to be the cause; it is the one big difference
left from a known-working stack on this chip.)

| | Ring 0 (normal; ours) | Ring 1 (Apple's AVB) |
|---|---|---|
| RCB (NIC SRAM) | `0x100` | `0x110` (BAR `0x8110`–`0x811c` in `wakeUp`) |
| maxlen/flags | `0x02000000` (512) | `0x01000000` (256) |
| NIC ring address | `0x4000` | `0x4800` |
| doorbell | mailbox `0x304` | mailbox `0x300` (`transmitKick(1)`) |
| descriptor | 16 B | 32 B: addr, len, flags, plus 64-bit time at `+0x10/+0x14` |

`transmitRealtimePackets`: refuses unless AVB state (`[this+0x2cb4]`) == 3
and link state (`[this+0x744]`) == 4; converts each packet's time through a
clock object (`[this+0xbd8]` vtable `+0x888`), writes the low word
`& 0xfffff000` and the high word; refuses a packet without a valid time
(`0xe00002d8`); first descriptor flags `0x20`, last `|= 0x04` (end of packet).

AVB state: `DoAVBGated` sets it to 1 (enable) or 0; `setupCopperPhy` moves 2
-> 3 when the link comes up. `startAVB` runs from `wakeUp` when state != 0.

`startAVB` (fully mapped): `0xd20 = base & 0xffff8000`, `0x5c8 = size &
0xf8000` (buffer of the AVB queue object, index 0; `cleanupTransmitQueue`
computes the same size and then reads `0x5cc`–`0x5d8`, so this buffer is
probably transmit-side, inference); `setPerfectMatch(5, …)`, clear 1–4;
`updateAVBStreamFilters`; RX classifier `0x5410`–`0x5418`, `0x5494`/`0x5498`,
`0x5500`–`0x5510` (5, 0xa, 0x10, 0x20, 0x40), `0x5608`–`0x561c` (already
replayed by our shim); `0x560 = 0xe`; reads `0x5c0/0x5c4` (TX_TSTAMP latch,
`kTxTstampLsb/Msb`) to release it. **No timed-TX enable in `startAVB`.**

AVB-gated writes in `wakeUp` (state != 0), the likely timed-TX enable:

- `0xd0c = 0xffffff`, `0xd08 = 1`, `0xd00 = 2` (a block containing `0xd20`;
  likely the launch-time scheduler, inference).
- Second send pipeline, +0x80 from the blocks our dext enables:
  `0x1080 = 2`, `0x1480 = 6`, `0x1880 = 6`, `0x1c80 = 6`
  (copies of `0x1000`/`0x1400`/`0x1800`/`0x1c00`).

`DoAVBGated` also writes `0x3c08`/`0x3c10`/`0x3c20`. These are HOSTCC RX
coalescing (our header names them); not a realtime-queue control, as
`bcm-firmware-shim-design.md` earlier claimed.

### Read-only probe of our chip (dext 310.6.51, datapath up, idle, 2026-11-07)

| Registers | Value |
|---|---|
| `0xd00`–`0xd2c` | all 0 |
| `0x1080`, `0x1480`, `0x1880`, `0x1c80` | 0 |
| `0x1000` / `0x1400` / `0x1800` / `0x1c00` | 2 / 6 / 0x16 / 2 |
| `0x5c8`–`0x5dc`, `0x560` | 0 |
| `0x5c0` / `0x5c4` | `0x56850248` / `0x31d`, static (TX timestamp latch) |
| `0x300` / `0x304` | 0 / `0x1f8` |
| `0x3c08` / `0x3c10` / `0x3c20` | 0x19 / 0xf / 1 |

The dext's Start snapshot of Apple's state has `0x5c8 = 0` (Apple's AVB mode
was off) and does not cover `0xd00` or the +0x80 blocks. We have never seen
these registers with Apple's AVB on.

## Open questions

1. What `0xd08`/`0xd0c`/`0xd00` and the +0x80 blocks mean exactly, and whether
   they are host-writable (the classifier needed the RX CPU).
2. Units of the ring-1 descriptor time and its relation to the EAV clock.
3. Meaning of descriptor flag `0x20`.
4. What `0x5cc`–`0x5d8` count (late/dropped launches?).
5. Why the 16R's CPU degrades only with the Broadcom. Untested ideas: total
   load (we also send CRF and control; the I210 sends the same), something
   about our Broadcom RX path dropping 16R intervals in groups of 8, and
   timed vs untimed TX.

## Next steps (the user chooses)

1. **Observe Apple with AVB on**: hand the adapter back to `BCM5701Enet`
   with AVB/EAV mode enabled (ideally with the 16R streaming) and read the
   registers above. Needs the user's go-ahead: deactivating our dext is a dext
   approval change, plus a replug.
2. **Enable ring 1 ourselves**: program RCB `0x110`, the `0xd00` block and
   the four +0x80 mode registers, send AVB frames on ring 1 with a launch time.
   Every register read first and logged (all read 0 today), dext rebuild,
   replug. Risk: guessed values could stall TX until a replug.
3. Investigate the Broadcom's inbound 8-frame interval drops.
