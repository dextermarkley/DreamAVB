# Phase 0b build verification (plan item 9 gate)

Host: macOS 26 (Apple Silicon), Xcode 26.6 (17F113), DriverKit SDK, Rust 1.99.0 (pinned).
Branch `phase0b-i210-dext`. All commands run with `TMPDIR=/Users/dexter/AVB-Audio/.agents/tmp/`, absolute paths, no `cd`. Logs: `.agents/tmp/logs/v1..v6*.log` (gitignored).
Nothing was installed or activated. No sudo, csrutil, nvram or systemextensionsctl was run. `avb-installer activate` was **not** run from the bundle in this step: the plan (§0) keeps activation in the hardware-test step, because it takes en7 away from Apple's driver.

| # | Command | Exit | Result |
|---|---|---|---|
| 1 | `cargo fmt --manifest-path …/Cargo.toml --all --check` | 0 | clean |
| 2 | `cargo clippy --manifest-path …/Cargo.toml --workspace --all-targets --locked -- -D warnings` | 0 | 0 warnings |
| 3 | `cargo test --manifest-path …/Cargo.toml --workspace --locked` | 0 | avb-probe lib: 27 passed, 0 failed |
| 4 | `make -C …/driver/tests clean test` | 0 | `all tests passed (97 checks)` (BCM) and `all i210 tests passed (126 checks)` |
| 5a | `xcodebuild -project …/driver/AVBI210Driver.xcodeproj -target AVBI210Driver -configuration Debug -sdk driverkit SYMROOT=…/build/driver OBJROOT=…/build/driver/obj CODE_SIGNING_ALLOWED=NO build`, from a deleted `build/` | 0 | `** BUILD SUCCEEDED **`, 0 warnings |
| 5b | same for `AVBBcmDriver.xcodeproj` / `AVBBcmDriver` | 0 | `** BUILD SUCCEEDED **`, 0 warnings (compile regression check after the ABI v2 rename) |
| 6 | `scripts/package.sh --adhoc` | 0 | assembled, ad-hoc signed, `codesign --verify --deep --strict` valid, 0 warnings |

Rust tests cover: ABI v2 sizes/offsets (`AvbInfo` 60, `nic_kind` @52, `clock_ctl` @56), the BCM and I210 frequency golden tables (same values as the C++ tests), I210 `expected_rate_ppb(0x00346DC6)` = 100 000.0047, the I210 `reg_dump` entries all passing the readable-offset mirror, `by_kind`, compensated delay = raw − 313 ns, neighbor rate ratio 1.0 and 1 + 1e-6, `mean_stddev`, plus the Phase 0 ptp/stats tests.

C++ I210 tests cover: identity (ours accepted; BCM 168214e4/00f6106b, I210 with subsys 00008086, I211 15398086 rejected), the TIMINCA golden table, SYSTIM split/join incl. ns = 999 999 999, advanced TX descriptor with and without 2STEP_1588 (len 60 and 1514), RX write-back decode (TSIP, RXE, non-EOP), the 16 B timestamp-header parse, composite values (SRRCTL 0xC2000402, RCTL, TCTL 0x000400FA, RXDCTL 0x02010A0C, TXDCTL 0x02010000, TSYNCRXCTL, ETQF3 0x440088F7), MDIC frames, BMCR autoneg (never sets reset), STATUS speed decode, and the read denylist.

Extra checks:
- `lipo -archs build/driver/Debug-driverkit/com.avbaudio.installer.i210.dext/com.avbaudio.installer.i210` → `arm64e`. `package.sh` re-checks this on the embedded copy (`dext executable archs: arm64e`) and fails otherwise.
- Embedded dext `IOKitPersonalities` (`plutil -p`): single personality `AVBI210Driver`, `IOProviderClass` IOPCIDevice, `IOPCIPrimaryMatch` "0x15338086", `IOPCISecondaryMatch` "0x02141ab6", `IOPCITunnelCompatible` true, `IOProbeScore` 100000, `IOClass` IOUserService, `IOUserClass` AVBI210Driver, `IOUserServerName` com.avbaudio.installer.i210, `UserClientProperties` IOUserUserClient / AVBI210UserClient. `grep` for IOMatchCategory/IOPCIMatch/IOPCIClassMatch/IONameMatch: 0 hits.
- `Contents/Library/SystemExtensions/` contains only `com.avbaudio.installer.i210.dext`. Its `CFBundleVersion` is `282.7.8` (kext-style, matches `^[0-9]{1,4}\.[0-9]{1,2}\.[0-9]{1,2}$`).
- `plutil -lint`: OK for app Info.plist, dext Info.plist, installer/probe entitlements, `AVBI210Driver.entitlements`.
- `codesign -d --entitlements -`: dext has `driverkit` = true, `transport.pci` = [{IOPCIPrimaryMatch = "0x15338086"}], `allow-any-userclient-access` = true. avb-probe has `userclient-access` = [com.avbaudio.installer.i210, com.avbaudio.installer.bcm57762].
- `target/release/avb-probe --help` lists `--nic <NIC>` (auto, i210, bcm; default auto). `pdelay --count` defaults to 30.
- `avb-probe info` with no dext loaded: exit 1, `neither AVBI210Driver nor AVBBcmDriver service found …`. `avb-probe --nic i210 info`: exit 1, `AVBI210Driver service not found (is com.avbaudio.installer.i210 activated and matched to the 8086:1533 / 1ab6:0214 adapter? …)`. No panic. (This also shows that no AVB dext service is currently registered on the host.)
- `target/release/avb-installer activate` outside the bundle: exit 1, `must run from inside AVBInstaller.app …`. `avb-installer deactivate --dext foo`: usage, exit 64.
- `git status`: only source changes; `build/`, `target/`, `.agents/tmp/` are ignored.

Not verified here (hardware-test step, plan §6/§7): sysextd/kernelmanagerd validation and exec of the real-identity build, matching against the 8086:1533 device, Stage A/B register behaviour on silicon, SYSTIM rate and TIMINCA (G2), RX timestamps in buffer (G3), TX latch and link delay / NRR (G4). The 178/448 ns 1000 Mb/s latencies are not in 333016 (see plan Deviations).
