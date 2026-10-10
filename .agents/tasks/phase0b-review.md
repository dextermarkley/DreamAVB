# Intel I210 PCIDriverKit dext with PTP RX/TX timestamping (Phase 0b)

The branch adds a second dext, `AVBI210Driver` (`com.avbaudio.installer.i210`), that takes the Intel I210 in the 1ab6:0214 Thunderbolt adapter away from Apple's `DriverKit_AppleEthernetE1000`. It replaces the BCM57762, whose RX PTP latch failed G3 in Phase 0. The user-client ABI goes to v2 under a NIC-neutral `AVBShared.h`, and the probe gains a static `NicProfile` table so G1–G4 run against either NIC. The installer and `package.sh` embed and activate only the I210 dext. The BCM dext stays in the tree and still builds. Bring-up keeps the Phase 0 two-stage shape: Stage A only touches the clock, and Stage B does the reset, link, one advanced-descriptor RX/TX queue pair, timestamp-in-buffer RX and the TXSTMPL/H latch. The verification note records clean builds of both dexts (0 warnings), 27 Rust tests, 97 BCM + 126 I210 C++ checks, an arm64e dext executable, a kext-style `CFBundleVersion` (282.7.8) and the exact personality.

Watch for: rollback with `avb-installer deactivate --dext bcm57762` will likely fail now that the BCM dext isn't in the bundle (likely). Start leaves an inherited PCI Bus Master bit as it found it (confirmed). A fault in the middle of an MDIO burst leaves SW_FW_SYNC.SW_PHY_SM held (confirmed). None of these blocks the build gate.

**Verdict**: APPROVED

## High-level view

Matching is exact. The single personality is `IOPCIPrimaryMatch 0x15338086` plus `IOPCISecondaryMatch 0x02141ab6`, `IOPCITunnelCompatible`, and `IOProbeScore 100000` against Apple's 3000. It has no vendor, class, name or match-category keys, so neither the BCM57762 nor an I211 nor an I210 with another subsystem can match. `Start` checks config 0x00/0x2c through `isOurDevice` before any BAR access, and the host tests pin the BCM, I211 and other-subsystem rejections. The PCI entitlement is the Phase 0 shape: PrimaryMatch only, so any 8086:1533. It's broader than the personality, but the personality and the ID check are what bind the device.

Every MMIO write belongs to a named sequence: Stage A, set clock, adjust frequency, Stage B init/quiesce, the TX doorbell or the RX tail. Every target is a 333016 register. There's no generic write path, every poll is bounded, and the fault latch gates all MMIO. Bus Master is turned on only after the Stage B reset. The plan's *(verify)* constants were resolved against the datasheet, with 14 recorded deviations (ICR/IMC primary offsets, TIMINCA width, MDIC PHYADD reserved, TCTL BST, the reset-done triple).

The gaps are in lifecycle edges, not the happy path. Start doesn't normalise a Bus Master bit inherited from the previous owner. A fault in the middle of an MDIO burst skips the PHY semaphore release. The rollback command for the BCM dext probably can't work from a bundle that no longer contains it.

ABI v2 is consistent across C++ and Rust: `AvbInfo` is 60 bytes with `nic_kind` at offset 52 and `clock_ctl` at 56. The BCM dext fills the new fields and keeps its own 0x10000 read window. The probe checks that the opened service reports the `nic_kind` its profile expects. G1–G4 are untested on hardware and nothing here claims otherwise.

<details>
<summary>Issues (3)</summary>

1. **BCM rollback path (likely)**: `avb-installer deactivate --dext bcm57762` submits a deactivation request for an extension that the new `AVBInstaller.app` no longer embeds. sysextd will likely reject it as not found in the bundle. Document `systemextensionsctl uninstall 8NG8NMGYVF com.avbaudio.installer.bcm57762` (SIP is off) as the BCM rollback, or drop the `--dext bcm57762` form.
2. **Inherited Bus Master (confirmed code behaviour, impact possible)**: `Start` calls `setCommandBits(v, kCmdMemSpace, 0)` and never clears Bus Master. If Apple's driver left it on with queues enabled, the device can keep DMAing to dead IOVAs through Stage A. Clear `kCmdBusMaster` in `Start` next to the Memory Space set.
3. **PHY semaphore held after a fault (confirmed)**: `setupLink` calls `phyRelease` only when `ok(v)`, so an MDIC READY timeout or an all-ones read during the burst leaves SW_FW_SYNC.SW_PHY_SM set for the next driver. Either accept this as part of the fault-latch contract and note it in the rollback docs, or allow the single SW_FW_SYNC release write after a fault.

</details>

<details>
<summary>Details</summary>

### Bus Master and inherited device state

```cpp
// Memory space only; Bus Master waits for the Stage B reset.
setCommandBits(v, i210::kCmdMemSpace, 0);
```

The plan's reason for deferring Bus Master is that Apple's driver "may have left queues enabled with IOVAs that are no longer mapped". The code meets the letter of that rule: it never *sets* the bit before the reset. But it also leaves an inherited set bit in place (confirmed). If the previous owner didn't drop Bus Master on close, RX DMA can continue during Stage A until the Stage B GIO master-disable. DART should block those writes, but a DART fault on a Thunderbolt device can show up as the all-ones reads this design treats as fatal. Clearing the bit in `Start` costs one config write and makes the rule hold whatever the previous owner did. The BCM dext set Bus Master in `Start`, so this isn't a regression, just an unfinished hardening.

### PHY semaphore release after a fault

The PHY step takes SWSM.SMBI → SWESMBI → SW_FW_SYNC.SW_PHY_SM, each bounded, but the release path has a hole:

```cpp
if (ok(v)) {
    phyRelease(v);
}
```

A READY timeout inside `mdicWait` latches `kAvbFaultMdio`, and the release is then skipped (confirmed). That's consistent with "refuse all MMIO after a fault", but SW_PHY_SM survives a port software reset. Apple's E1000 driver or a re-started dext could then find the PHY semaphore busy and fail its own bring-up until a power cycle (likely, not traced in Apple's driver). An MDI ERROR, by contrast, is not a fault and does release correctly.

### Rollback for the BCM dext

`main.rs` in `avb-installer` accepts `deactivate --dext bcm57762` and submits `OSSystemExtensionRequest.deactivationRequest` for `com.avbaudio.installer.bcm57762`. `package.sh` now embeds only `com.avbaudio.installer.i210.dext`. sysextd resolves a request's identifier against the requesting app's `Contents/Library/SystemExtensions`, so a deactivation for an extension that isn't in the bundle will likely fail with ExtensionNotFound (likely: this matches how activation resolves extensions, but it wasn't exercised here). The plan lists this form as the BCM rollback. `systemextensionsctl uninstall` with the team ID is the dependable path on this SIP-off host.

### Test coverage

The pure logic is covered: identity, the TIMINCA golden table shared by C++ and Rust, descriptor layouts, composite register values, the denylist, ABI v2 sizes, compensated delay and NRR.

Not tested: anything that needs silicon, i.e. sysextd validation of the real-identity build, matching, every register sequence, the 178/448 ns 1000 Mb/s latencies (recorded as not in 333016), and whether the writeback length really includes the 16 B header. These belong to the hardware step.

</details>

<details>
<summary>File map</summary>

- `driver/AVBShared.h` (renamed from `AVBBcmShared.h`): ABI v2, `nic_kind`/`clock_ctl`, fault codes 11–14, read limit 0x20000.
- `driver/AVBI210Driver/AVBI210Driver.cpp`: new dext: Start ID gate, Stage A clock, Stage B reset/link/queues/timestamps, selectors.
- `driver/AVBI210Driver/AVBI210UserClient.cpp`, `*.iig`: user client and dispatch table, forwarded to the default queue.
- `driver/AVBI210Driver/I210Regs.h`, `I210Logic.hpp`: 333016 constants and pure helpers.
- `driver/AVBI210Driver/Info.plist`, `AVBI210Driver.entitlements`: exact personality; PCI transport entitlement 0x15338086.
- `driver/AVBI210Driver.xcodeproj/project.pbxproj`: new project, `ARCHS = arm64e`, bundle ID `com.avbaudio.installer.i210`.
- `driver/AVBBcmDriver/*`, `AVBBcmDriver.xcodeproj`: header rename, GetInfo v2 fields, local 0x10000 read window.
- `driver/tests/{Makefile,i210logic_tests.cpp,tg3logic_tests.cpp}`: I210 host tests; ABI v2 asserts.
- `crates/avb-probe/src/{nic.rs,freq.rs,abi.rs,iokit.rs,main.rs,ptp.rs,stats.rs,lib.rs}`: NIC profiles, `--nic`, per-NIC frequency math, compensated delay/NRR, summaries.
- `crates/avb-installer/src/main.rs`: activates I210; `deactivate --dext`; status for both IDs.
- `packaging/probe.entitlements`: userclient-access for both IDs.
- `scripts/package.sh`: builds both dexts, embeds and signs only the I210 dext, arm64e check, personality dump.

Full diff: `git -C /Users/dexter/AVB-Audio diff phase0-bcm-dext...phase0b-i210-dext`.

</details>
