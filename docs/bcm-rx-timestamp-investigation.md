# BCM57762 RX timestamps: investigation log

Goal: get hardware RX PTP timestamps on the Broadcom BCM57762 in Apple's
Thunderbolt Gigabit Ethernet adapter (14e4:1682 / 106b:00f6, ASIC rev
0x57766000). Phase 0 (`docs/phase0-results.md`) rejected this chip because
our tg3-style `RX_PTP_CTL` setup never latched an RX timestamp.

## Evidence that the hardware can do it (2026-11-04)

Under Apple's own driver (`com.apple.iokit.AppleBCM5701Ethernet` 11.0.0,
`BCM5701Enet` + `BCM5701EnetClock` + `IOTimeSyncEthernetPort`), with the
adapter cabled to the MOTU AVB switch, `ioreg -r -c IOTimeSyncEthernetPort`
reports:

- `ASCapable = Yes`, grandmaster `0x0001f2fffeffab61` (the MOTU switch), Sync
  received and processed continuously.
- `LinkPropagationDelay = 58` ns (min 55, max 63). Software RX timestamps
  could not resolve a link delay this small, so Apple gets hardware RX
  timestamps from this chip.

## Research

- Linux tg3 (behavior reference only) sets PTP_CAPABLE only for ASIC revs
  5719, 5720 and 5762, and sets `GRC_MODE_TIME_SYNC_ENABLE` only for those.
  It never enables PTP on 57765/57766. Our dext already sets
  `GRC_MODE_TIME_SYNC_ENABLE` in Stage A, so that is not the missing step.
- No public Broadcom document covering this chip's RX timestamp registers was
  found. The BCM57785 family programmer's guide (57785-PG105-R, 2012) exists
  but is not on Broadcom's public site.

## Snapshot 1: power-on state, not Apple's

`avb-probe --nic bcm snapshot` (dext 307.22.13) after the user approved the
dext and replugged the adapter. Raw output:
`.agents/tmp/bcm-snapshot-poweron.txt` / `.json`.

Our dext has the higher probe score, so it matched the freshly enumerated
device first and Apple's driver never initialized the chip. The snapshot is
the bootcode / power-on state:

- `GRC_MODE` (0x6800) 0x00000000 (no `TIME_SYNC_ENABLE`).
- `RX_PTP_CTL` (0x6c8) 0, RX timestamp registers (0x6b0/0x6b4) 0.
- `MAC_RX_MODE` (0x468) 0x02700000.
- `EAV_REF_CLK_CORRECT_CTL` (0x6928) 0x00000001 — so this value is the
  bootcode's, not something Apple's driver writes (Phase 0 finding 2 assumed
  otherwise).
- `TX_TSTAMP_LSB/MSB` (0x5c0/0x5c4) hold a latched value at power-on.

The 13 words that differ from the live state are explained by Stage A
(`GRC_MODE |= TIME_SYNC_ENABLE`), free-running counters (`GRC_TIMER`, EAV
clock LSB), the MAC/MI and CPMU status registers, and link activity.

## Next: capture Apple's state

Our dext is activated (`[activated enabled]`), but Apple's `BCM5701Enet`
re-matched after the deactivation and still owns the device: a dext only
wins the match when the device is enumerated, and a replug re-enumerates it
before Apple's driver runs (snapshot 1). To capture Apple's programming, its
driver instance must be terminated in place, without re-enumeration, so the
provider re-matches to our dext with the chip still configured:

    sudo kmutil unload -c BCM5701Enet

If Apple's stop path resets the chip, the snapshot will show reset values
again; that is itself an answer (Apple's state cannot be observed this way).

### Result: Apple's driver cannot be evicted in place

`sudo kmutil unload -c BCM5701Enet` (user, 2026-11-04) had no effect: the
same `BCM5701Enet` instance (registry id 0x1000058b1) stayed attached and
`com.apple.iokit.AppleBCM5701Ethernet` 11.0.0 stayed loaded. The kext has no
bundle under `/System/Library/Extensions`; it lives in the boot kernel
collection, whose kexts cannot be unloaded. So the only ways to hand our dext
the device are re-enumeration (a replug, which gives our dext a fresh,
unconfigured chip) or a reboot, neither of which preserves Apple's register
state. Observing Apple's configuration from the chip is not possible this way.

## Breakthrough: Apple's AVB register block at 0x5400-0x561c (2026-11-04)

Extracted `AppleBCM5701Ethernet` (x86_64) from the boot kernel collection
(`/System/Library/KernelCollections/BootKernelExtensions.kc`, fileset entry
`com.apple.iokit.AppleBCM5701Ethernet` at file offset 39919616) and
disassembled it read-only with capstone. Only register offsets and values are
taken from it, never code. Raw dumps in `.agents/tmp/re-avb.txt`.

`BCM5701Enet::startAVB()` writes a block of registers the tg3-documented path
NEVER touches. Each write is guarded by two flag bytes (`+0x3d9` bit 0 and
`+0x7b0` bit 0, i.e. "AVB supported && not suspended"); the register base is
`ivars+0x118` (BAR0). The writes, in order:

| Offset | Value | Likely meaning |
|---|---|---|
| 0xd20 | gPTP clock hi window (masked `& 0xffff8000`) | CPMU/clock, via `+0x118[0xd20]` |
| 0x5c8 | `(serdes-serdes) & 0xf8000` | clock window |
| 0x5410 | 0x8400000c | RX rule/classifier element |
| 0x5494 | 0x88f7ffff | **ethertype 0x88F7 (gPTP/AVTP) match** |
| 0x5414 | 0x84006002 | RX rule element |
| 0x5494 | 0x013fffff | rule mask (second write) |
| 0x5418 | 0x84006002 | RX rule element |
| 0x5498 | 0x0140ffff | rule mask |
| 0x5500 | 5 | queue/config |
| 0x5504 | 0xa | |
| 0x5508 | 0x10 | |
| 0x550c | 0x20 | |
| 0x5510 | 0x40 | |
| 0x5608 | 0 | |
| 0x560c | 0xe0 | |
| 0x5610 | 0 | |
| 0x5614 | 0xe1 | |
| 0x5618 | 0x25 | |
| 0x561c | 0xe4e3a2 | |
| 0x560 | 0xe | MAC-block config |

`setupAVB()` also sets two timing constants (0x1e848 = 125000, 0x3d090 =
250000 ns) through helper calls, calls four sub-helpers, and reads the TX
timestamp at 0x5c0/0x5c4 into `ivars+0x2cf8`.

`BCM5701EnetClock::takeCrossTimestamp()` reads the free-running EAV clock at
**0x6900 (LSB) / 0x6904 (MSB)** between two `mach_absolute_time` fences — the
same EAV block Phase 0 already used. So the CLOCK is the known EAV block; the
missing piece is the **0x5400-0x561c RX classifier/rule block** that routes
gPTP (ethertype 0x88F7) frames so their RX timestamps latch. The tg3
`RX_PTP_CTL` at 0x6c8 is NOT how Apple does RX timestamps on this chip.

### Gap in snapshot 1

The power-on snapshot windows did not include 0x5000-0x5700, so we never saw
this block's reset state. The snapshot has been widened to cover it, so the
next capture under our own driver (power-on) can be compared against these
Apple-written values to confirm which writes are the live difference.

### Next

1. Capture the 0x5400-0x5620 block (added to the snapshot).
2. Disassemble `updateAVBStreamFilters`, `setRealtimeReceiveQueueFilter` and
   `read/regWrInd` to resolve the rule-element encoding (0x84006002, the
   0x5494 ethertype/mask pair) and the queue config at 0x5500-0x5618.
3. In the dext, program the 0x5400 block for ethertype 0x88F7 after GRC time
   sync enable, reading each register first and logging every write, then
   check whether RX timestamps latch (the Phase 0 G3 gate).

## Result: the 0x5400 block is behind the RX CPU, not plain MMIO (2026-11-04)

Built AVBBcmDriver 307.22.57 with `programAvbRxRules` (replaying Apple's exact
0x5400 writes, read-first + logged) behind `AVB_INIT_FLAG_AVB_RX_RULES`, took
the adapter over (replug), brought the datapath up with the flag, and ran the
Phase 0 G3 gate (`avb-probe --nic bcm rx-ptp`). Result: **still 0 RX
timestamps** — `type_flags = 0x0004` on every Pdelay_Req, exactly as Phase 0.

Reading the registers back (`avb-probe --nic bcm snapshot`, live column)
explains why:

- 0x5000-0x53ff is the **RX CPU** (tg3 `RX_CPU_BASE = 0x5000`). It is running
  firmware under our driver: `RX_CPU_STATE` (0x5004) and the program counter
  (0x501c) advance between reads, and 0x5200-0x527c reads back as RISC
  instructions.
- Our writes to the 0x5410 / 0x5494 / 0x5498 / 0x5500-0x561c block did NOT
  stick: every one reads back 0, before and after. 0x5494 taking two different
  writes (0x88f7ffff then 0x013fffff) confirms it is a write-only indexed port.
  Only 0x560 (a real MAC register) took its write (0 -> 0xe).

So the 0x5400 classifier is not a bank of plain registers; it is a write-only
port consumed by the RX CPU firmware. Apple's driver ships that firmware
(symbols `loadFirmware(FWIMG_INFO*)`, `WaitForFirmwareInit`, `WriteFirmwareSig`,
`HaltCpu`) and loads it during bring-up; the classifier entries and the RX BD
timestamp are produced by that firmware. The stock NVRAM firmware the chip
runs under our driver does not do it.

### Conclusion

Hardware RX timestamps on the BCM57762/57766 require Broadcom RX-CPU firmware
with AVB/1588 support. This is consistent with Linux tg3 never setting
PTP_CAPABLE for this ASIC. The clean-room path (public specs + read-only
register observation) cannot enable it: the mechanism lives in a proprietary
firmware blob we do not have and should not extract or ship. The register
offsets and values were learned read-only; replaying them is insufficient
without the firmware that consumes them.

Phase 0's verdict stands, now with the mechanism understood: the I210 remains
the timestamping NIC. `programAvbRxRules` and the snapshot tooling are kept
(behind the opt-in flag, off by default) as the evidence trail; they change no
default behaviour.

### Options not taken (need a decision)

1. Extract and load Apple's/Broadcom's RX-CPU firmware blob. Out of the
   project's clean-room rule, and redistribution is almost certainly not
   permitted. Not pursued without an explicit decision.
2. Reverse-engineer and write a clean RX-CPU firmware. Very large effort,
   undocumented RISC target; disproportionate to the gain.
