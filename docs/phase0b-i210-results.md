# Phase 0b hardware results: Intel I210 vs StudioLive 16R

Host Mac16,8, macOS 26.5.2, SIP off, `amfi_get_out_of_my_way=1`, developer mode on.
NIC: Intel I210 8086:1533 / 1ab6:0214, PCI rev 3, MAC 64:4b:f0:12:73:b8, behind Thunderbolt
(was en7 on `DriverKit_AppleEthernetE1000`). Directly cabled to a StudioLive 16R
(Milan fw 3.3.0.109659, port 00:0a:92:ff:fe:03:3b:a9/1), link 1000FD.
Dext `com.avbaudio.installer.i210` 282.7.31, arm64e, team 8NG8NMGYVF.
Raw probe output: `.agents/tasks/phase0b-hw-results.json`.

## Results

| Gate | Result | Evidence |
|---|---|---|
| G1 match | PASS | `AVBI210Driver` (IOUserService) owns the 8086:1533 `ethernet@0`; `DriverKit_AppleEthernetE1000` and en7 are gone; `[activated enabled]`; Broadcom on Apple's `BCM5701Enet` (+`BCM5701EnetClock`), the BCM dext is uninstalled |
| G2a clock ticks | PASS | ns rate, +74.86 ppm vs mach, residual 168 ns RMS (86–129 ns in the adjfreq windows), monotonic |
| G2b freq correction | PASS | +100 000 ppb → +99 988.8 ppb measured (scale 0.9999); −100 000 → −100 042.3 (scale 1.0004); baseline drift −59.6 ppb; TIMINCA restored to 0 |
| G3 RX timestamps | PASS | 35/35 Pdelay_Req from the 16R carry an in-buffer RX timestamp (TSIP); no repeats, no drops; hw intervals 1 000 283 235 ns ± 907 ns |
| G4 TX timestamps + link delay | PASS | 40/40 TX timestamps, 40 complete exchanges; link delay compensated mean 142.6 ns, sd 2.5 ns (raw 455.6 ns, sd 2.5 ns); NRR 0.999916820 (−83.18 ppm), sd 0.006 ppm |

### G1 detail
- kernelmanagerd `DextValidation`: "Validating dext" / "Realize dext", category validation result `(nil, nil)`. No validation or exec errors.
- Kernel log after approval and a replug of the adapter: `AVBI210: start: 15338086 subsys 02141ab6 rev 3`, `stage A: STATUS 0x00380780 CTRL 0x081c0241 TSAUXC 0x40000000 TIMINCA 0x00000000`, `start(ethernet-0x10000475e) ok`.
- `avb-probe --nic i210 info`: nic_kind 2, `0x15338086` / `0x02141ab6`, rev 3, fault 0, STATUS link up 1000 Mb/s full.
- The user's reboot had already completed, so the Broadcom was back on `BCM5701Enet` before the I210 work started. The I210 dext never touches it.

### G2 detail
- `clock --seconds 10`: 99 samples, slope 1.000074859, bracket median 10.25 µs (max 14.8 µs).
- Windows (10 s each): baseline +74.846 ppm, plus +174.835 ppm, minus −25.196 ppm, baseline +74.787 ppm.
- Registers: `0x00346dc6` / `0x80346dc6`, exactly the golden values. No scale factor needed (the BCM needed 1.0011/0.9982).

### G3 detail (`datapath up`, then `rx-ptp --seconds 30`)
- Datapath: reset, GIO master disable, PHY (BMCR 0x1140, 1000FD advertised) and link all succeeded on the first try. `TSYNCRXCTL` 0x19 (enabled, TYPE_ALL, latch valid), `ETQF3` 0x440088F7, `SRRCTL0` 0xC2000402.
- 35 Pdelay_Req, seq 20605–20639 contiguous, every one with `type_flags` 0x9003 (DD, EOP, TSIP) and `has_ts=1`.
- Successive hw_ts intervals: mean 1 000 283 235 ns, sd 907 ns, range 1 000 282 200–1 000 284 640 ns.
- The ratio of the hw interval to the harvest interval means (+72 ppm) matches the G2a SYSTIM-vs-mach offset (+75 ppm), so the timestamps are in the SYSTIM timebase.
- The plan's "hw vs harvest interval within ±1 ms" sub-check measured 2.20 ms max. The excess is polling jitter on the host side: the harvest interval sd is 1.04 ms (dext 2 ms timer plus probe poll). The hw_ts intervals themselves are stable to about 1 µs. This doesn't affect the verdict.

### G4 detail (`pdelay --count 40`)
- Every t1 lies inside the [doorbell, next clock sample] bracket. t1 − doorbell is 23.1–27.9 µs (mean 24.4 µs).
- 16R turnaround t3 − t2: 23.1–25.6 µs. correctionField is 0 in both Pdelay_Resp and Follow_Up.
- Link delay raw: mean 455.6, sd 2.5, median 455, min 450, max 461 ns.
- Compensated (TX 178 / RX 448 ns at 1000 Mb/s): mean 142.6, sd 2.5, median 142, min 137, max 148 ns.
- NRR (n = 39): mean 0.999916820, sd 5.7e-9.
  - −83.2 ppm is the 16R clock relative to our SYSTIM.
  - SYSTIM is +74.9 ppm vs mach, which puts the 16R at about −8 ppm vs host.

## Findings
1. The adapter's 25 MHz reference runs +75 ppm fast vs mach. That is inside the probe's ±200 ppm bound but above a typical ±50 ppm crystal spec. It doesn't matter for gPTP: TIMINCA has ±488 ppm of range, and the NRR is computed and stable.
2. Raw link delay alone (455.6 ns) also meets the 800 ns asCapable threshold, so G4 passes even without the unverified 1000 Mb/s latency constants. With them, 142.6 ns is plausible for a short cable plus PHY. The exact split between cable and PHY is still unverified against an independent reference (for example, a known gPTP stack on the same cable).
3. TX doorbell-to-timestamp latency is about 24 µs (BCM: 13 µs). That latency is not part of the PTP math, only the probe bracket.
4. The 16R answers our Pdelay_Req with valid Pdelay_Resp + Follow_Up and sends its own Pdelay_Req at 1.000 s (in its clock).
5. The dext ran Stage A and Stage B with no fault latched, and `datapath down` returned it to stage 1 cleanly. No driver fixes were needed after the build loop.

## Verdict: proceed to Phase 1 on the I210

All four gates pass on silicon. The I210 dext owns only the I210, and the Broadcom is back on Apple's driver. Hardware RX and TX timestamps work. The clock is adjustable to within 0.05 % of the commanded ppb. The measured link delay is about 143 ns compensated (456 ns raw) with 2.5 ns sd, and the NRR is stable to 0.006 ppm: well within 802.1AS asCapable limits. That is the measurement the Broadcom could not make.

Rollback (run only on request): `avb-installer deactivate`, or `systemextensionsctl uninstall 8NG8NMGYVF com.avbaudio.installer.i210`, then replug the adapter. Apple's E1000 and en7 return.
