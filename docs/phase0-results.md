# Phase 0 hardware results: BCM57762 (Apple TB Ethernet) vs StudioLive 16R

Host Mac16,8, macOS 26.5.2, SIP off, `amfi_get_out_of_my_way=1`, developer mode on.
NIC: BCM57762 14e4:1682 / 106b:00f6, ASIC rev 0x57766000, no APE/ASF. Directly
cabled to a StudioLive 16R (Milan fw 3.3.0.109659), link 1000FD.
Dext `com.avbaudio.installer.bcm57762` 282.5.35, team 8NG8NMGYVF.
Raw probe output: `.agents/tasks/phase0-hw-results.json`.

## Results

| Gate | Result | Evidence |
|---|---|---|
| G1 match | PASS | `AVBBcmDriver` (IOUserService) owns the 14e4:1682 `ethernet@0`; `BCM5701Enet` and en8 are gone; `[activated enabled]`; en7 still on `DriverKit_AppleEthernetE1000`, up |
| G2a clock ticks | PASS, with a finding | ns rate, −5.03 ppm vs mach, residual 41 ns RMS, monotonic. `EAV_REF_CLCK_MSB` always reads 0 (finding 1) |
| G2b freq correction | PASS | +100 000 ppb → +100 122 ppb measured (scale 1.0011); −100 000 → −99 837 (scale 0.9982); baseline drift 0.18 ppm |
| G3 RX latch | FAIL | 115/115 Pdelay_Req received, 0 with a timestamp |
| G4 TX latch | PASS (TX only) | 5/5 TX timestamps latched, each 12.5–13.0 µs after the doorbell. No link delay possible without t4 |

### G2 detail
- `clock --seconds 10`: 99 samples, slope 0.999994965, register-read bracket median 6.7 µs (max 7.1 µs).
- Command for ±100 000 ppb: `0x8000068e` / `0xc000068e` (corr 1678 = round(1e5·2²⁴/1e9)). The dext restored 0 at exit.
- Window slopes: baseline −5.034 ppm, plus +95.088 ppm, minus −104.871 ppm, baseline −4.855 ppm.

### G3 detail
- The 16R (port 00:0a:92:ff:fe:03:3b:a9/1) sends Pdelay_Req at 1.00 s intervals; frames reach the host ring.
- `RX_PTP_CTL` (0x6c8) reads 0x00000000 after the datapath writes `V2_L2_EN|0xF|HWTS_INTERLOCK`.
- RX BD `type_flags` = 0x0004 on every frame (PTPSTAT bits 0x0210 never set).
- `RX_TSTAMP_LSB/MSB` (0x6b0/0x6b4) stay 0.
- Per the safety rules, I didn't try any value or sequence outside tg3. Linux never enables PTP on 57766, and this silicon matches that: the tg3-documented RX path is not implemented.

### G4 detail (`pdelay --count 5`)

| seq | t1 (EAV ns) | t1 − doorbell | 16R t3 − t2 |
|---|---|---|---|
| 0x4000 | 454 526 267 192 | 12 964 ns | 23 980 ns |
| 0x4001 | 456 531 547 256 | 12 520 ns | 24 430 ns |
| 0x4002 | 458 533 627 704 | 12 702 ns | 24 360 ns |
| 0x4003 | 460 536 493 688 | 12 842 ns | 23 860 ns |
| 0x4004 | 462 541 858 392 | 12 841 ns | 23 730 ns |

- Every t1 lies inside the [doorbell, next clock sample] bracket.
- The 16R answered every request with Pdelay_Resp + Follow_Up, so it accepts our frames.
- The link delay can't be computed because t4 (RX timestamp of Pdelay_Resp) is unavailable.

## Findings
1. EAV counter width: the MSB register reads 0, and the LSB wraps every 2³² ns (4.29 s). The TX latch, though, returns full 64-bit values (MSB 0x4e–0x6b) in the same timebase: low 32 bits match the LSB reads. Software unwrapping works (`stats::unwrap32`).
2. `EAV_REF_CLK_CORRECT_CTL` read 0x00000001 at first load (left by Apple's driver/bootcode); the probe now leaves it 0.
3. Driver build fixes needed on this OS: dext `CFBundleVersion` must be a kext-style version, and the dext binary must be arm64e (an arm64 platform DriverKit binary is refused at exec).

## Verdict: switch to the Intel I210

TX timestamps and the disciplined clock work, but RX packet timestamps don't. Without t4 there is no hardware link-delay measurement. Software RX timestamps through a 2 ms polled ring give tens of µs to ms of jitter, so neighborRateRatio and the delay would far exceed the 800 ns asCapable threshold. The 16R would very likely mark the link not asCapable.

The I210 has documented, working RX+TX PTP timestamps. It has to be a dedicated I210 (en7 is the user's LAN and stays untouched).

Optional future research, not Phase 1: Apple's `BCM5701Enet` publishes a `BCM5701EnetClock` + `IOTimeSyncEthernetPort` on this device. Apple may use an undocumented RX timestamp mechanism, but that is outside the tg3-documented register set this phase was allowed to use.
