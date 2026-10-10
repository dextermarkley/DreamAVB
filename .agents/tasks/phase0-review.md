# Phase 0 review (pass 3): BCM57762 PCIDriverKit dext, Rust probe and Rust installer

Commit `c6e05c0` on `phase0-bcm-dext` delivers all ten plan items against `master`:

- a C++ PCIDriverKit dext (`AVBBcmDriver` + `AVBBcmUserClient`) with two-stage bring-up. Stage A in `Start` only enables time sync. Stage B is the chip reset and a one-ring polled datapath, run on request;
- a 10-selector ExternalMethod ABI;
- a Rust `avb-probe` covering the G2–G4 experiments;
- a Rust objc2 `avb-installer`;
- an ad-hoc/identity signing script.

Matching is scoped to exactly 14e4:1682 with subsystem 106b:00f6, in three independent places. The register sequences follow tg3's 57765-class behaviour; I checked them against the reference `tg3.c`/`tg3.h` under `.agents/tmp/tg3`, and no tg3 code or comments were copied. Build evidence for all six item-10 commands is recorded and passing.

Watch for: the dev-only `allow-any-userclient-access` entitlement combined with an unchecked `NewUserClient` lets any local process reset the NIC and inject raw frames (confirmed, accepted for Phase 0, must not ship). There are also some lifecycle gaps on the Start/Stop error paths: the timer is never cancelled, and Bus Master stays on after a failed Start (confirmed, minor).

**Verdict**: APPROVED

## High-level view

The device is scoped in three layers, and the I210 cannot match structurally. The single personality uses `IOPCIPrimaryMatch 0x168214e4` + `IOPCISecondaryMatch 0x00f6106b` with `IOPCITunnelCompatible` and `IOProbeScore 100000`. It has no `IONameMatch`, no `IOPCIClassMatch` and no `IOMatchCategory`. The PCI transport entitlement allows only `0x168214e4`, and `Start` re-reads config 0x00/0x2c and refuses anything else before touching BAR0. 8086:1533 fails all three.

All MMIO goes through `rd`/`wr` helpers that enforce a latched fault. Every poll is bounded, and there is no generic write selector. Stage A performs exactly the three planned writes: `MISC_HOST_CTRL` via config, `MEMARB |= ENABLE` and `GRC_MODE |= TIME_SYNC_ENABLE`. All three are gated on the GEN15 ASIC rev being 57766.

The EAV, PTP and Stage B values match tg3 for this class. I checked:

- EAV STOP/LSB/MSB/RESUME at 0x6900–0x6908;
- the 24-bit EN/NEG correction at 0x6928;
- `RX_PTP_CTL` = V2_L2 | events | HWTS_INTERLOCK, written immediately before the `GRC_MODE` write that ORs in TIME_SYNC (tg3 `reset_hw` order);
- TX/RX timestamp pairs read LSB then MSB;
- `DMA_RW_CTRL` = DIS_CACHE_ALIGNMENT for 57765_PLUS;
- `PCISTATE` restore;
- the core-reset order;
- RX return ring size 1024;
- status-block LE offsets.

The deviations from the plan's summary table are documented and follow tg3 more closely than the table did.

The user client exposes everything the task requires: read/set/adjust the EAV clock, RX timestamps attached to `PollRx` entries, TX timestamps via `GetTxTimestamp`, raw frame send, and received-frame reads. Calls are serialized with the 2 ms poll timer by dispatching synchronously onto the driver's default queue. The Rust side mirrors the ABI with size and offset tests, and every dependency is pinned exactly with `Cargo.lock` committed. The installer stays in Rust, and nothing in code or scripts runs sudo, csrutil, nvram or boot-args.

<details>
<summary>Issues (4)</summary>

1. **Open user client with dev entitlement (non-blocking, confirmed).** `allow-any-userclient-access` is on and `NewUserClient` does no caller check, so once loaded any local process can call `InitDatapath` (chip reset), `SendFrame` and `SetClock`/`AdjFreq`. This is acceptable for Phase 0 as planned. Before any distribution, remove the entitlement and rely on the probe's `userclient-access`. Consider allowing only one open client.
2. **Timer not cancelled on Stop (non-blocking, likely).** `Stop` only calls `SetEnable(false)`, and `free()` releases the `IOTimerDispatchSource` and `OSAction` without `Cancel()`. Call `timer->Cancel(...)` in `Stop` before releasing, as DriverKit dispatch sources expect.
3. **Start error paths leave Bus Master on (non-blocking, confirmed).** `Start` sets Memory+Bus Master before creating the timer and calling `RegisterService`. Those failure paths call `Close` without clearing Bus Master, and none of them undo `super::Start`. Clear Bus Master on every failure exit after `setCommandBits`.
4. **IOReturn name table mislabel (minor, confirmed).** `kr_check` in `iokit.rs` maps `0xe00002ed` to `kIOReturnNoDevice`. That value is `kIOReturnNotResponding`; `kIOReturnNoDevice` is `0xe00002c0`. Fix the mapping so hardware-test logs aren't misleading.

</details>

<details>
<summary>Details</summary>

### Matching and the I210 exclusion

`driver/AVBBcmDriver/Info.plist` has one personality. It specifies `IOProviderClass IOPCIDevice`, `IOPCIPrimaryMatch "0x168214e4"` (device 1682, vendor 14e4), `IOPCISecondaryMatch "0x00f6106b"` (Apple subsystem), `IOPCITunnelCompatible true` and `IOProbeScore 100000`, with no mask, class or name wildcard. Even without the secondary match, PCI primary matching is an exact compare without a mask, so 0x15338086 cannot match. The dext entitlement's `transport.pci` array has one dict with the same primary match, which also stops the dext from opening any other PCI device at runtime.

`Start` reads vendor/device and subsystem through config space before `GetBARInfo` or any write, and returns `kIOReturnUnsupported` on a mismatch. `isOurDevice` is host-tested, including I210 rejection (build evidence: 96 C++ checks). The built-plist `plutil -p` output recorded in the evidence confirms the post-build personality has none of `IOMatchCategory`, `IONameMatch` or `IOPCIClassMatch`.

### Register sequences versus tg3 behaviour

I compared the dext against the tg3 reference copy rather than relying on the plan table. The comparisons that matter for G2–G4:

- **Set clock.** `setClock` does an RMW on `EAV_REF_CLCK_CTL`, preserving the GPIO bits. It writes STOP, then LSB and MSB, then RESUME, and flushes with a readback. tg3 writes the bare STOP/RESUME values. The RMW form is what the plan asked for, and it leaves `TSYNC_GPIO` untouched.
- **Frequency correction.** `eavCorrectionFromPpb` rounds `|ppb|·2^24/1e9`, masks to 24 bits, and adds EN plus NEG for negative values. A zero magnitude gives a 0 register value. tg3 truncates instead of rounding, a sub-LSB difference that the shared golden table pins in both the C++ and Rust tests.
- **RX filter.** `resetHw` writes `RX_PTP_CTL` immediately before `GRC_MODE = grcModeDatapath()`, and that value includes `TIME_SYNC_ENABLE`. This is exactly tg3 lines 10152–10159. `grcModeDatapath` sets `NO_TX_PHDR_CSUM` and not `NO_RX_PHDR_CSUM`, matching tg3. The plan text was wrong here, and the deviation is logged.
- **Chip reset.** `FASTBOOT_PC = 0`; `GRC_MISC_CFG` written first with bit 29 alone, then with CORECLK_RESET; 120 µs, config flush, 120 µs; DEVCTL RO/NS cleared and DEVSTA W1C; then the `MISC_HOST_CTRL`, `PCISTATE` (ROM_ENABLE|ROM_RETRY) and COMMAND restores; `MEMARB = ENABLE`; firmware mailbox poll; `GRC_MODE` swap; `MAC_MODE = 0`. The order matches `tg3_chip_reset` for a non-5780, CPMU-present PCIe part. `tg3_override_clk` doesn't apply to 57766, so it is correctly absent.
- **Firmware poll timeout.** The dext latches a fault on timeout, where tg3 tolerates "no firmware running". The behaviour is deliberately stricter than tg3's.
- **`DMA_RW_CTRL`.** `(rd & ~DIS_CACHE_ALIGN) | DIS_CACHE_ALIGN` reproduces tg3's 57765_PLUS result, since `tg3_calc_dma_bndry` overwrites `dma_rwctrl` with DIS_CACHE_ALIGNMENT and `tg3_test_dma` exits early for 57765_PLUS.
- **Ring geometry.** `RX_RET` is 1024 (JUMBO_CAPABLE, no LRG_PROD_RING_CAP). The std BDINFO `maxlen_flags` is `512<<16 | 1536<<2`. Send RCBs 0..1 and RX-return RCBs 0..3 have everything but 0 disabled, and there is no NIC address word (5705_PLUS).
- **RX descriptors.** The status-block offsets 0x0a/0x10/0x12 match the LE `tg3_hw_status` layout. The RX error mask `0x01ef0000` reproduces tg3's "odd nibble alone is not an error" rule.

The GPL boundary holds. A grep of `driver/` for `tw32`, `tr32`, `tg3_flag`, `TG3_`, `NIC_SRAM_` and similar finds only provenance comments that name tg3 functions. Constants are re-expressed as `bcm::k…` `constexpr` values with our own names. The tg3 sources stay under the gitignored `.agents/tmp/`, and `git ls-files` shows none of them tracked.

### Fault latch and bounded polls

`rd` latches on all-ones, and `rdRaw` is used only where all-ones is legal data: timestamp LSBs, the SRAM window and the debug dump. `readTs` checks the MSB. Every `pollUs` call has an explicit bound: firmware 1 s, BUFMGR 20 ms, HOSTCC 20 ms, MDIO 50 ms, stop-block 100 ms, link 5 s. The real wall time can exceed the nominal bound, because each firmware-poll iteration does five MMIO round trips over Thunderbolt, but it stays finite.

On Stage B failure or `Stop`, Bus Master is cleared through config space, outside the latch, before `CompleteDMA` and buffer release. A faulted device therefore can't DMA into freed memory.

### User-client surface and serialization

```
probe ──IOConnectCallMethod──▶ AVBBcmUserClient::ExternalMethod
                                   │ kDispatch[sel] validates scalar/struct counts
                                   ▼
                     driver default queue (DispatchSync_f)
                                   │
          AVBBcmDriver::HandleUserMethod ◀── TimerFired (2 ms harvest/checkTx)
```

Selectors 0–9 match plan §3. Out-of-range selectors return `kIOReturnUnsupported` before the dispatch table is indexed. `ReadClock` bounds n to 1..64, `AdjFreq` bounds |ppb| ≤ 1e6, and `ReadReg32` requires an offset below 0x10000 and 4-aligned. `SendFrame` takes 14..1514 bytes and pads to 60 (documented deviation). Only one timestamped TX may be outstanding, and a second returns `kIOReturnBusy`.

The security posture is the open point. `allow-any-userclient-access` disables the system's client-entitlement check, and `NewUserClient` adds none of its own. Any process on the host can open the service once the dext is loaded. It can then core-reset the adapter, change the PTP clock, and transmit arbitrary L2 frames on the mixer link. The plan specifies this entitlement as dev-only, so it doesn't block Phase 0. It must come out before anything leaves this machine, and a single-client guard would also stop two probes from interleaving `PollRx` consumption.

### Lifecycle gaps

`Stop` disables the timer but never calls `Cancel`, and `free()` releases the timer and its `OSAction` directly. `TimerFired` checks `stage` first, so a late fire after Stop is harmless in practice. DriverKit still expects dispatch sources to be cancelled before release (likely).

In `Start`, `setCommandBits(MemSpace|BusMaster)` runs before timer creation and `RegisterService`. Those failure branches `Close` the device without clearing Bus Master. At that point no DMA addresses have been programmed, so the exposure is small (confirmed).

### Build evidence

`.agents/tasks/phase0-build-verification.md` records exit 0 for all six item-10 gates:

- `fmt`;
- `clippy -D warnings`;
- `cargo test` (18 tests);
- `make -C driver/tests test` (96 checks);
- `xcodebuild` from a clean `build/`, with 0 warnings;
- `package.sh --adhoc`, followed by `codesign --verify --deep --strict`.

It also records the extra item 3/7/8/9 checks: the "service not found" exit, the installer's bundle check, `cargo tree` pins, `plutil` lint, and the entitlement dumps for all three binaries. I re-ran nothing. Hardware behaviour (G1–G4) was not assessed.

</details>

<details>
<summary>File map</summary>

- `driver/AVBBcmDriver/Info.plist`: single exact-ID personality, DEXT bundle, user-client properties.
- `driver/AVBBcmDriver/AVBBcmDriver.entitlements`: driverkit, `transport.pci` (0x168214e4 only), allow-any-userclient-access (dev).
- `driver/AVBBcmDriver/AVBBcmDriver.{iig,cpp}`: Start/Stop, Stage A, Stage B reset/init/abort, harvest, TX timestamp, selector handlers.
- `driver/AVBBcmDriver/AVBBcmUserClient.{iig,cpp}`: dispatch table and forwarding onto the driver queue.
- `driver/AVBBcmDriver/Bcm57762Regs.h`, `Tg3Logic.hpp`: own-name register map and pure helpers.
- `driver/AVBBcmShared.h`: ABI v1 with size asserts.
- `driver/tests/*`: host C++ test runner.
- `driver/AVBBcmDriver.xcodeproj/project.pbxproj`: DriverKit target, arm64, DK 25.0, manual signing.
- `crates/avb-probe/*`: ABI mirror, PTP build/parse, stats, freq, IOKit wrapper, CLI (info/regs/clock/adjfreq/datapath/rx-ptp/pdelay/all), JSON report.
- `crates/avb-installer/src/main.rs`: objc2 activation/deactivation/status.
- `packaging/*`: installer Info.plist and installer/probe entitlements.
- `scripts/package.sh`: build, assemble, version-stamp, sign inside-out, verify.
- `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `.gitignore`: workspace, exact pins, arm64 toolchain.

Full diff: `git -C /Users/dexter/AVB-Audio diff master..phase0-bcm-dext`.

</details>
