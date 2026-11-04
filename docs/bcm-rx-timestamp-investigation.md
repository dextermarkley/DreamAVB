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
