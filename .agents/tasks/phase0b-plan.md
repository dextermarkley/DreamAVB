# Phase 0b Implementation Plan — Intel I210 PCIDriverKit dext + PTP RX/TX timestamp proof

Branch `phase0b-i210-dext` (from `phase0-bcm-dext` f6b6cad), checked out in `/Users/dexter/AVB-Audio`. Never push.
Inputs: `docs/phase0-results.md`, `.agents/tasks/phase0-plan.md` (patterns, deviations), `.agents/tasks/avb-hw-feasibility.md` ("Intel I210", Q5), the existing `driver/`, `crates/`, `scripts/package.sh`.

Loop contract (unchanged):
- Implementer first. Reviewer last, writes `/Users/dexter/AVB-Audio/.agents/tasks/phase0b-review.json` with top-level `"verdict"`; the loop stops on `"APPROVED"`.
- Then the hardware-test step (§6/§7), then the final commit step on `phase0b-i210-dext`.

## 0. Ground rules (every item)

- Environment (`.kiro/steering/scratch-files.md`):
  - Never touch `/tmp` or the system `$TMPDIR`. Scratch, logs and test binaries go in `/Users/dexter/AVB-Audio/.agents/tmp/`. Prefix cargo/xcodebuild/clang/codesign with `TMPDIR=/Users/dexter/AVB-Audio/.agents/tmp/`.
  - No `cd`/`pushd`. Use absolute paths, `git -C`, `cargo --manifest-path /Users/dexter/AVB-Audio/Cargo.toml`, `xcodebuild -project /abs/...xcodeproj`, `make -C /abs/dir`.
  - xcodebuild: `-derivedDataPath` can't be combined with `-target` (Phase 0 deviation). Use `SYMROOT=/Users/dexter/AVB-Audio/build/driver OBJROOT=/Users/dexter/AVB-Audio/build/driver/obj`, both gitignored.
- GPL boundary. Implement from the Intel I210 datasheet 333016 (public PDF).
  - Linux `igb` and OpenAvnu `igb_avb` (kernel parts are GPLv2) may be read only to learn offsets, bit values and the order of operations.
  - Never copy code, comments, structs or macro blocks.
  - `I210Regs.h` header comment: "offsets/bit values from the Intel I210 datasheet (333016); order of operations cross-checked against the observable behaviour of Linux igb; no code copied".
  - Each constant marked *(verify)* in §4 must be confirmed in the datasheet before use. If the datasheet differs, follow the datasheet and record it under "Deviations".
- Device scope.
  - Only PCI 8086:1533 with subsystem 1ab6:0214 (config 0x2c reads `0x02141ab6`).
  - Never match, open or touch any other device: the BCM57762 14e4:1682, an I211 8086:1539, or an I210 with another subsystem.
  - Enforced three ways: the personality, the entitlement, and the `Start()` ID check.
- Register safety.
  - Write only registers documented in 333016, and only in the named sequences in §5.
  - There is no generic write selector.
  - Every poll is bounded.
  - A poll timeout or an all-ones read (`0xFFFFFFFF`, device gone or hung over Thunderbolt) latches `fault`. After that, all MMIO is refused until `Stop`.
  - Never "try other values": back off, log, report.
- Hard-won Phase 0 lessons (must hold for the new dext):
  - `ARCHS = arm64e`. An arm64 DriverKit binary is refused at exec and the kernel falls back to Apple's driver.
  - Kext-style `CFBundleVersion`/`CFBundleShortVersionString`, stamped by `package.sh` as `<days since 2026-01-01>.<UTC hour>.<UTC minute>`. Rebuild at least a minute after the previous one so the version increases.
  - Signing identity "Apple Development: armitage787@gmail.com", Team 8NG8NMGYVF.
- Unproven until hardware: G1–G4 are hypotheses until the hardware-test step shows §6 evidence. The build loop only proves that the code builds, the pure logic is correct, and the bundle is laid out and signed. The reviewer must not accept "works" claims based on code reading.
- No user-visible system changes in the build loop. The loop does not install or activate, does not run `systemextensionsctl`/`csrutil`/`nvram`/sudo, and does not touch the network. Host state is already set by the user: SIP off, `amfi_get_out_of_my_way=1`, developer mode on.
- The BCM dext source stays in the repo and must keep building. The installer and package activate only the I210 dext.

## 1. Design decisions

1. **Separate dext, separate Xcode project.**
   - New files: `driver/AVBI210Driver.xcodeproj` and `driver/AVBI210Driver/`.
   - Target `AVBI210Driver`; bundle ID `com.avbaudio.installer.i210`; classes `AVBI210Driver : IOService` and `AVBI210UserClient : IOUserClient`.
   - Rationale: the BCM project stays untouched apart from the ABI rename. The proven pbxproj settings are copied verbatim with new object IDs (`7B…` instead of `7A…`), `ARCHS = arm64e`, `DRIVERKIT_DEPLOYMENT_TARGET = 25.0`.
   - Sharing iig classes across two dexts is not worth the build complexity. The user client is about 150 lines and gets a near-copy with I210 names.
2. **Matching** (`driver/AVBI210Driver/Info.plist`). One personality `AVBI210Driver`:
   - `IOProviderClass = IOPCIDevice`
   - `IOPCIPrimaryMatch = "0x15338086"`
   - `IOPCISecondaryMatch = "0x02141ab6"` (subsystem-id << 16 | subsystem-vendor, the same encoding that won G1 on the BCM)
   - `IOPCITunnelCompatible = true`
   - `IOProbeScore = 100000`. Apple's `DriverKit_AppleEthernetE1000` is 3000 and uses `IOPCIMatch 0x00008086&0x0000ffff` + class match.
   - `IOClass = IOUserService`, `IOUserClass = AVBI210Driver`, `IOUserServerName = com.avbaudio.installer.i210`
   - `UserClientProperties = { IOClass = IOUserUserClient; IOUserClass = AVBI210UserClient; }`
   - No `IOMatchCategory`, `IONameMatch`, `IOPCIMatch` or `IOPCIClassMatch`.
   - Entitlements (`AVBI210Driver.entitlements`), the same shape as the BCM's, which passed validation:
     - `com.apple.developer.driverkit` = true
     - `com.apple.developer.driverkit.transport.pci` = `[{ IOPCIPrimaryMatch = "0x15338086" }]`
     - `com.apple.developer.driverkit.allow-any-userclient-access` = true
3. **Two stages, like Phase 0.**
   - Stage A in `Start` does no reset and no DMA, and does not enable Bus Master. It masks interrupts, enables SYSTIM, zeroes TIMINCA and loads SYSTIM. This is enough for G1/G2 with the fewest writes.
   - Stage B (`kAvbSelInitDatapath`) runs the software reset, PHY/link, one RX and one TX queue, and PTP timestamping.
   - Bus Master is set only in Stage B (stricter than the BCM). Apple's driver may have left queues enabled with IOVAs that are no longer mapped.
4. **Polling, no interrupts.** IMC/EIMC mask everything. A 2 ms `IOTimerDispatchSource` on the default queue harvests RX and the TX timestamp. ExternalMethod is forwarded to the default queue with `DispatchSync_f`, the Phase 0 pattern, so all access is serialized without locks.
5. **Descriptors: advanced, one buffer per packet.** I210 TX timestamping needs the advanced data descriptor (`MAC_TSTAMP` in `cmd_type_len`), and the in-buffer RX timestamp needs the advanced RX descriptor. Legacy descriptors are not used.
   - RX ring: 128 × 16 B descriptors, 128 × 2 KiB buffers.
   - TX ring: 64 × 16 B descriptors, 16 × 2 KiB bounce slots.
   - Each ring and buffer pool is one single-segment DMA buffer (the Phase 0 `dmaAlloc` pattern; fault if `segCount != 1`).
6. **RX timestamp method: per-packet timestamp in the receive buffer.**
   - Settings: `RXPBS.CFG_TS_EN`, `SRRCTL0.TIMESTAMP`, `TSYNCRXCTL = ENABLED | TYPE_ALL`, `ETQF[3] = 0x88F7 | FILTER_ENABLE | 1588_TS`.
   - When the writeback has `TSIP`, the first 16 bytes of the buffer are a timestamp header (8 reserved bytes, then SYSTIML ns u32 LE, SYSTIMH sec u32 LE), and the frame follows at +16. The writeback length includes the header.
   - Rationale: this is how the I210 is designed to be used (82580+ class). Every frame carries its own timestamp, so there is no one-deep latch to race. That latch is exactly what failed on the BCM.
   - The legacy RXSTMPL/H latch is not used. The probe's `regs` dump still shows `TSYNCRXCTL.RXTT` for diagnosis.
7. **TX timestamp: the TXSTMPL/H latch.**
   - Only one timestamped TX may be outstanding (the BCM rule).
   - Before the doorbell: if `TSYNCTXCTL.VALID` is already set, read TXSTMPL then TXSTMPH to release the stale latch.
   - After the doorbell: the timer polls `TSYNCTXCTL.VALID`, reads TXSTMPL then TXSTMPH (the H read unlocks), and has a 300 ms timeout.
8. **Clock representation.** The I210 SYSTIM is time-of-day: SYSTIMH = seconds, SYSTIML = nanoseconds (< 1e9). The dext converts every clock, RX and TX timestamp to `u64 ns = sec·1e9 + ns` before it crosses the ABI. The probe's G2/G4 math and `stats::unwrap32` work unchanged: values exceed 2³² once SYSTIM is loaded with host time, so unwrap32 is a no-op.
9. **Frequency adjust: TIMINCA.**
   - Encoding: `inca = ISGN(bit31 if ppb<0) | (round(|ppb|·2³⁵/1e9) & 0x00FFFFFF)`, or 0 when the magnitude rounds to 0. The base period is 8 ns, and incvalue is in 2⁻³² ns per 8 ns tick *(verify field width in 333016 §8.16)*.
   - Max |ppb| = 488 281 (the magnitude fits 24 bits). The I210 dext rejects anything larger with `kIOReturnBadArgument`. The header's generic `AVB_ADJ_FREQ_MAX_PPB` stays 1 000 000 as an upper bound, and the per-NIC limit lives in the probe's NIC profile.
   - Golden values, shared by the C++ and Rust tests:

     | ppb | TIMINCA |
     |---|---|
     | 0 | 0x00000000 |
     | 1 | 0x00000022 |
     | −1 | 0x80000022 |
     | 29 | 0x000003E4 |
     | 100 000 | 0x00346DC6 |
     | −100 000 | 0x80346DC6 |
     | 488 281 | 0x00FFFFF7 |
     | −488 281 | 0x80FFFFF7 |

     `expected_rate_ppb(0x00346DC6)` = 100 000.0047.
10. **ABI v2, generalized, in one renamed header.** `git mv driver/AVBBcmShared.h driver/AVBShared.h`, then bump `AVB_ABI_VERSION` to 2. Selectors 0–9 keep their numbers and arguments. Changes:
    - `AvbInfo` renames: `asic_rev → hw_rev`, `misc_host_ctrl → ctrl_a`, `grc_mode → ctrl_b`.
    - `AvbInfo` appends `uint32_t nic_kind` and `uint32_t clock_ctl`, giving size **60**. Field meanings:

      | Field | BCM | I210 |
      |---|---|---|
      | `nic_kind` | 1 | 2 |
      | `hw_rev` | ASIC rev | PCI revision ID (cfg 0x08 & 0xff, expect 3) |
      | `ctrl_a` | MISC_HOST_CTRL | CTRL |
      | `ctrl_b` | GRC_MODE | STATUS |
      | `clock_ctl` | EAV_REF_CLK_CORRECT_CTL | TIMINCA |

    - New fault codes, appended (BCM codes unchanged):
      - 11 `kAvbFaultMasterDisable` (GIO master-disable poll)
      - 12 `kAvbFaultResetTimeout` (reset/config-done poll)
      - 13 `kAvbFaultSwFwSync` (semaphore)
      - 14 `kAvbFaultQueueEnable` (RXDCTL/TXDCTL enable or disable poll)
    - `AVB_REG_READ_LIMIT` becomes 0x20000, the maximum over NICs. Each dext enforces its own range: the BCM keeps 0x10000 as a local constant in `Tg3Logic.hpp`.
    - `AvbRxEntry.type_flags` holds the low 16 bits of the NIC's RX status (I210: advanced writeback `status_error`; TSIP is bit 15).
    - `AvbClockSample.eav_ns` keeps its name and is documented as "NIC clock ns (EAV on BCM, SYSTIM on I210)", to limit churn.
    - The BCM dext is updated to fill the new fields (one function). Rationale: one ABI and one probe for both NICs. Phase 0 BCM data stays reproducible.
11. **Probe: a NIC profile module, not a trait object.** New `crates/avb-probe/src/nic.rs` with `pub struct NicProfile` holding:
    - `kind`, `label`, `service_name`, `dext_bundle_id`, `reg_read_limit`, `max_ppb`
    - `correction_from_ppb: fn(i64) -> u32`, `expected_rate_ppb: fn(u32) -> f64`
    - `reg_dump: &'static [(&'static str, u32)]`
    - `ctrl_a_name`, `ctrl_b_name`, `clock_ctl_name`
    - `tx_latency_1000_ns: i64`, `rx_latency_1000_ns: i64`

    Plus `pub const BCM57762`, `pub const I210`, and `pub fn by_kind(u32) -> Option<&'static NicProfile>`.
    - `freq.rs` splits into `freq::bcm` (the existing functions, moved) and `freq::i210`. `scale_factor` stays at the top level.
    - `Device::open(sel: NicSelect)`, with `NicSelect = Auto | I210 | Bcm`. Auto tries the `AVBI210Driver` service first, then `AVBBcmDriver`. It checks `info.nic_kind` against the opened service's profile and keeps `&'static NicProfile`.
    - Global CLI flag `--nic auto|i210|bcm` (default auto).
    - Rationale: the NIC-specific parts are data plus two pure functions. A static table is simpler than `dyn Trait` and is testable without hardware.
12. **G4 statistics and latency compensation.**
    - Raw link delay: `((t4−t1) − (t3−t2) − corr)/2`, the existing `ptp::link_delay_ns`.
    - Compensated: `t1c = t1 + tx_latency`, `t4c = t4 − rx_latency`. At 1000 Mb/s on the I210 that is TX 178 ns and RX 448 ns. These are the PHY/MAC timestamp-point latencies Intel publishes for the I210 (*verify* in 333016 §7.8; they are also the values Linux igb uses). Net effect: compensated = raw − 313 ns. BCM: 0/0.
    - Report raw and compensated per exchange, plus mean, stddev, median, min and max over all complete exchanges.
    - Neighbor rate ratio per consecutive pair: `(t3ₙ − t3ₙ₋₁)/(t4ₙ − t4ₙ₋₁)`. Report mean and stddev.
    - **The G4 pass criterion uses the compensated delay**, since that is what a real 802.1AS stack would use. Raw is reported alongside.
13. **Installer and packaging.**
    - `avb-installer` targets `com.avbaudio.installer.i210`: `activate` and `deactivate` default to it. `deactivate --dext bcm57762` is accepted for rollback, using an allowlist of the two IDs only. `status` prints lines for both IDs.
    - `package.sh` builds both dexts (the BCM build is a compile-regression check) but embeds and signs **only** the I210 dext.
    - The probe's `userclient-access` lists both bundle IDs.

## 2. Deliverable layout (new or changed)

```
driver/
├── AVBShared.h                      # renamed from AVBBcmShared.h, ABI v2 (§1.10)
├── AVBBcmDriver.xcodeproj/project.pbxproj   # file ref AVBBcmShared.h -> AVBShared.h
├── AVBBcmDriver/{AVBBcmDriver.cpp,AVBBcmUserClient.cpp,Tg3Logic.hpp}  # include rename, GetInfo v2
├── AVBI210Driver.xcodeproj/project.pbxproj  # new, copy of BCM settings, IDs 7B…
├── AVBI210Driver/
│   ├── Info.plist, AVBI210Driver.entitlements
│   ├── AVBI210Driver.iig, AVBI210Driver.cpp
│   ├── AVBI210UserClient.iig, AVBI210UserClient.cpp
│   ├── I210Regs.h                   # our constants (§4)
│   └── I210Logic.hpp                # pure helpers, no DriverKit includes
└── tests/{Makefile, tg3logic_tests.cpp, i210logic_tests.cpp}
crates/avb-probe/src/{abi.rs, nic.rs (new), freq.rs, iokit.rs, main.rs, stats.rs, ptp.rs, lib.rs}
crates/avb-installer/src/main.rs
packaging/probe.entitlements
scripts/package.sh
.agents/tasks/phase0b-build-verification.md  # item 9 evidence
```

## 3. ABI v2 struct (driver/AVBShared.h ⇄ crates/avb-probe/src/abi.rs)

```c
struct AvbInfo {            // 60 bytes
    uint32_t abi_version;   // 2
    uint32_t vendor_device; // cfg 0x00
    uint32_t subsys;        // cfg 0x2c
    uint32_t hw_rev;
    uint32_t ctrl_a;
    uint32_t ctrl_b;
    uint32_t stage, fault, flags;
    uint8_t  mac[6]; uint8_t pad[2];
    uint32_t tb_numer, tb_denom;
    uint32_t nic_kind;      // AVB_NIC_BCM57762 = 1, AVB_NIC_I210 = 2
    uint32_t clock_ctl;
};
```
`static_assert(sizeof(AvbInfo) == 60)`. The other struct sizes are unchanged (24 / 152 / 3656). Rust gets the matching `offset_of!` tests (`nic_kind` 52, `clock_ctl` 56).

## 4. I210 register reference (BAR0 = memory index from `GetBARInfo(kPCIMemoryRangeBAR0)`; host ioreg shows BAR0 at 0x10, 32-bit mem; require size ≥ 0x20000)

Device and control:
- `CTRL` 0x0000: GIO_MASTER_DISABLE bit 2, SLU bit 6, FRCSPD bit 11, FRCDPLX bit 12, RST bit 26, PHY_RST bit 31 (never set).
- `STATUS` 0x0008: FD bit 0, LU bit 1, SPEED bits 7:6 (00=10, 01=100, 1x=1000), GIO_MASTER_ENABLE bit 19, PF_RST_DONE bit 21 *(verify)*.
- `CTRL_EXT` 0x0018 (read only here).
- `MDIC` 0x0020: DATA 15:0, REGADD 20:16, PHYADD 25:21 (internal PHY addr 1 *(verify)*), OP 27:26 (01 write, 10 read), READY bit 28, ERROR bit 30.
- `PHPM` 0x0E14: D0LPLU bit 1 *(verify)*. Clear it only if set, so link isn't limited to 10/100.

Interrupts:
- `ICR` 0x00C0 (read-to-clear), `IMC` 0x00D8, `EIMC` 0x1528.

Shared-resource semaphore (datasheet §4.7):
- `SWSM` 0x5B50: SMBI bit 0, SWESMBI bit 1.
- `SW_FW_SYNC` 0x5B5C: SW_PHY_SM bit 1, FW_PHY_SM bit 17 *(verify)*.

NVM/config done:
- `EEMNGCTL` 0x12030: CFG_DONE0 bit 18 *(verify)*.

RX:
- `RCTL` 0x0100: EN bit 1, UPE bit 3, MPE bit 4, BAM bit 15, SECRC bit 26.
- `RXPBS` 0x2404: CFG_TS_EN bit 31.
- Queue 0:
  - `RDBAL0` 0xC000, `RDBAH0` 0xC004, `RDLEN0` 0xC008 (multiple of 128), `RDH0` 0xC010, `RDT0` 0xC018
  - `SRRCTL0` 0xC00C: BSIZEPACKET 6:0 in KiB = 2, DESCTYPE 27:25 = 001 (advanced one-buffer), TIMESTAMP bit 30, DROP_EN bit 31
  - `RXDCTL0` 0xC028: ENABLE bit 25, WTHRESH 20:16 = 1
- `ETQF(n)` 0x5CB0 + 4n; we use n = 3: ETYPE 15:0 = 0x88F7, FILTER_ENABLE bit 26, 1588_TIME_STAMP bit 30 *(verify)*.
- Advanced RX descriptor:
  - Read format: `pkt_addr` u64, `hdr_addr` u64 = 0.
  - Writeback: `status_error` u32 at +8 with DD bit 0, EOP bit 1, TSIP bit 15, RXE/errors 31:20 *(verify)*; `length` u16 at +12; `vlan` u16 at +14.

TX:
- `TCTL` 0x0400: EN bit 1, PSP bit 3, CT 11:4 = 0xF, COLD 21:12 = 0x3F.
- Queue 0: `TDBAL0` 0xE000, `TDBAH0` 0xE004, `TDLEN0` 0xE008, `TDH0` 0xE010, `TDT0` 0xE018, `TXDCTL0` 0xE028 (ENABLE bit 25, WTHRESH 20:16 = 1).
- Advanced data descriptor:
  - `buffer_addr` u64.
  - `cmd_type_len` u32: DTALEN 15:0, MAC_TSTAMP bit 19, DTYP 23:20 = 0x3, EOP bit 24, IFCS bit 25, RS bit 27, DEXT bit 29.
  - `olinfo_status` u32: PAYLEN 31:14. Writeback DD = bit 0 of this dword.

Time sync:
- `SYSTIML` 0xB600 (ns), `SYSTIMH` 0xB604 (sec), `SYSTIMR` 0xB6F8 (residue).
  - Read order: SYSTIMR, SYSTIML, SYSTIMH. The first read latches the set *(verify)*.
  - Write order: SYSTIML then SYSTIMH.
- `TIMINCA` 0xB608: ISGN bit 31, INCVALUE 23:0.
- `TSAUXC` 0xB640: DISABLE_SYSTIME bit 31; clear it.
- `TSYNCTXCTL` 0xB614: VALID bit 0, ENABLED bit 4.
- `TXSTMPL` 0xB618, `TXSTMPH` 0xB61C.
- `TSYNCRXCTL` 0xB620: RXTT/VALID bit 0, TYPE 3:1 (TYPE_ALL value 0x08), ENABLED bit 4.
- `TSYNCRXCFG` 0x5F50 (left 0).
- `RXSTMPL/H` 0xB624/0xB628 and `RXSATRL/H` 0xB62C/0xB630: unused, and never read (reads change latch state).
- Station address `RAL0` 0x5400 / `RAH0` 0x5404, loaded from NVM; read only.

`isReadableRegOffsetI210(off)`: `off < 0x20000`, 4-aligned, and **not** one of:
- ICR 0xC0
- statistics block 0x4000–0x4FFF (clear-on-read)
- SYSTIMR 0xB6F8
- TXSTMPL/H 0xB618/0xB61C
- RXSTMPL/H 0xB624/0xB628
- RXSATRL/H 0xB62C/0xB630

These reads have side effects.

## 5. Implementation items (build loop)

- [ ] 1. **ABI v2 rename and generalization; BCM dext and the C++ tests still green.**
  - `git -C /Users/dexter/AVB-Audio mv driver/AVBBcmShared.h driver/AVBShared.h` and apply §1.10/§3: version 2, renamed fields, `nic_kind`, `clock_ctl`, `AVB_NIC_*` defines, fault codes 11–14, `AVB_REG_READ_LIMIT 0x20000`, header guard `AVB_SHARED_H`.
  - Update the includes in `driver/AVBBcmDriver/{AVBBcmDriver.cpp,AVBBcmUserClient.cpp,Tg3Logic.hpp}` and `driver/tests/tg3logic_tests.cpp`, the `driver/tests/Makefile` dependency, and the BCM `project.pbxproj` file reference (path and name `AVBShared.h`).
  - In `AVBBcmDriver::HandleUserMethod` `kAvbSelGetInfo`: fill `hw_rev`/`ctrl_a`/`ctrl_b`, `nic_kind = AVB_NIC_BCM57762`, and `clock_ctl = rd(EAV_REF_CLK_CORRECT_CTL)` (only when `ok(v)`).
  - In `Tg3Logic.hpp`: `isReadableRegOffset` uses a local `kBcmRegReadLimit = 0x10000`, not the header limit.
  - Update the size `static_assert`s and tests to 60.
  - Files: `driver/AVBShared.h`, `driver/AVBBcmDriver/*.cpp`, `driver/AVBBcmDriver/Tg3Logic.hpp`, `driver/AVBBcmDriver.xcodeproj/project.pbxproj`, `driver/tests/{Makefile,tg3logic_tests.cpp}`.
  - Verify:
    - `make -C /Users/dexter/AVB-Audio/driver/tests test` passes.
    - `TMPDIR=/Users/dexter/AVB-Audio/.agents/tmp/ xcodebuild -project /Users/dexter/AVB-Audio/driver/AVBBcmDriver.xcodeproj -target AVBBcmDriver -configuration Debug -sdk driverkit SYMROOT=/Users/dexter/AVB-Audio/build/driver OBJROOT=/Users/dexter/AVB-Audio/build/driver/obj CODE_SIGNING_ALLOWED=NO build` ends with `** BUILD SUCCEEDED **` and 0 warnings.

- [ ] 2. **Probe ABI mirror, NIC profiles, I210 frequency math (pure Rust, tests).** Depends on 1.
  - `abi.rs`: v2 mirror, `NIC_BCM57762 = 1`, `NIC_I210 = 2`, `fault_name` for 11–14, size/offset tests (60; `nic_kind` @52, `clock_ctl` @56).
  - `freq.rs`: move the current functions into `pub mod bcm`. Add `pub mod i210` with `correction_from_ppb`, `ppb_from_correction`, `expected_rate_ppb`, and `MAX_PPB = 488_281`, using the §1.9 golden table. Keep `scale_factor` at the top level.
  - New `nic.rs` per §1.11:
    - `BCM57762`: service `AVBBcmDriver`, bundle `com.avbaudio.installer.bcm57762`, limit 0x10000, max_ppb 1e6, the current `REG_DUMP` moved here, names MISC_HOST_CTRL/GRC_MODE/EAV_REF_CLK_CORRECT_CTL, latencies 0/0.
    - `I210`: service `AVBI210Driver`, bundle `com.avbaudio.installer.i210`, limit 0x20000, max_ppb 488 281, names CTRL/STATUS/TIMINCA, latencies 178/448.
    - I210 `reg_dump` (all safe per §4): CTRL, STATUS, CTRL_EXT, PHPM, RCTL, TCTL, RXPBS, SRRCTL0, RXDCTL0, RDH0, RDT0, TXDCTL0, TDH0, TDT0, ETQF3, TSYNCRXCTL, TSYNCTXCTL, TIMINCA, TSAUXC, SYSTIML, SYSTIMH, EEMNGCTL.
    - A unit test asserts every I210 `reg_dump` offset passes a Rust mirror of `isReadableRegOffsetI210`.
  - `stats.rs`: add `mean_stddev(&[f64]) -> Option<(f64, f64)>` (sample stddev, n ≥ 2).
  - `ptp.rs`: add `neighbor_rate_ratio(prev: (t3, t4), cur: (t3, t4)) -> Option<f64>` and `compensated_link_delay_ns(t1, t2, t3, t4, corr_resp, corr_fu, tx_lat, rx_lat)`. Tests: a known synthetic exchange; compensated = raw − (tx+rx)/2; ratio 1.0 for identical intervals and 1 + 1e-6 for a +1 ppm case.
  - `lib.rs` exports `nic`.
  - Files: `crates/avb-probe/src/{abi.rs,freq.rs,nic.rs,stats.rs,ptp.rs,lib.rs}`, plus the `main.rs` call-site updates needed to compile (`freq::bcm::…`, field renames).
  - Verify:
    - `TMPDIR=/Users/dexter/AVB-Audio/.agents/tmp/ cargo test --manifest-path /Users/dexter/AVB-Audio/Cargo.toml -p avb-probe --locked` passes.
    - `cargo clippy --manifest-path /Users/dexter/AVB-Audio/Cargo.toml --workspace --all-targets --locked -- -D warnings` is clean.

- [ ] 3. **I210 pure helpers and host tests.** Depends on 1.
  - `driver/AVBI210Driver/I210Regs.h`: namespace `i210`, every §4 constant, with *(verify)* resolved against 333016.
  - `driver/AVBI210Driver/I210Logic.hpp`: header-only, no DriverKit includes, namespace `i210logic`. Functions:
    - `isOurDevice(vd, ss)` (only `0x15338086`/`0x02141ab6`)
    - `timincaFromPpb(int64)`
    - `systimToNs(sec, ns)`, `nsToSystim(u64) -> {sec, ns}`
    - `makeAdvTxDesc(iova, len, wantTs) -> {u64 addr; u32 cmdTypeLen; u32 olinfo}`
    - `makeAdvRxRead(iova)`
    - `decodeAdvRxWb(u32 statusError, u16 length) -> {len, dd, eop, tsip, error}`
    - `parseRxTsHeader(const uint8_t*)` (bytes 8–11 ns, 12–15 sec → u64 ns)
    - `srrctlValue()`, `rctlValue(promisc)`, `tctlValue()`, `rxdctlEnable()`/`txdctlEnable()`, `tsyncRxCtlValue()`, `etqf1588Value()`
    - `mdicReadFrame(phy, reg)`, `mdicWriteFrame(phy, reg, val)`, `mdicDecode(u32) -> {ready, error, data}`
    - `linkFromStatus(u32) -> {up, speed, fd}`
    - `isReadableRegOffsetI210(off)`
    - `ringNext`/`ringDistance` (copied as our own code from `Tg3Logic.hpp`, not shared, to keep the two dexts independent)
  - `driver/tests/i210logic_tests.cpp`: the same assert-runner style as `tg3logic_tests.cpp`. Cover:
    - identity: accept ours; reject BCM `0x168214e4`/`0x00f6106b`, I210 with subsys `0x00008086`, and I211 `0x15398086`
    - the TIMINCA golden table (identical to Rust)
    - SYSTIM split/join round trip incl. ns = 999 999 999
    - TX descriptor bits with and without TSTAMP, len 60 and 1514
    - RX writeback decode incl. TSIP and an error bit
    - ts-header parse from a hand-built 16-byte buffer
    - the composite register values
    - MDIC frames
    - STATUS speed decode for 10/100/1000
    - the readable-offset denylist
    - `static_assert`s of ABI v2 sizes
  - `driver/tests/Makefile`: add an `$(OUT)/i210logic_tests` rule. `test` builds and runs both binaries, under the existing `OUT` in `.agents/tmp/driver-tests`.
  - Files: `driver/AVBI210Driver/{I210Regs.h,I210Logic.hpp}`, `driver/tests/{i210logic_tests.cpp,Makefile}`.
  - Verify: `make -C /Users/dexter/AVB-Audio/driver/tests test` prints all-pass for both runners and exits 0.

- [ ] 4. **I210 dext skeleton plus Stage A (G1 and G2 code paths).** Depends on 3.
  - Create `driver/AVBI210Driver.xcodeproj/project.pbxproj`: a copy of the BCM project with fresh object IDs, files `AVBI210Driver.{iig,cpp}`, `AVBI210UserClient.{iig,cpp}`, `I210Regs.h`, `I210Logic.hpp`, `Info.plist`, `AVBI210Driver.entitlements`, and group ref `../AVBShared.h`. Settings: `PRODUCT_BUNDLE_IDENTIFIER = com.avbaudio.installer.i210`, `ARCHS = arm64e`, `INFOPLIST_FILE`/`CODE_SIGN_ENTITLEMENTS` pointing at the new files.
  - Add `Info.plist` and entitlements per §1.2. The `OSBundleUsageDescription` mentions the Intel I210 and Phase 0b PTP validation.
  - `.iig` files mirror the BCM ones (same methods: `Start`, `Stop`, `NewUserClient`, `HandleUserMethod LOCALONLY`, `TimerFired TYPE(IOTimerDispatchSource::TimerOccurred)`; the user client has `Forward LOCALONLY`).
  - `AVBI210UserClient.cpp` is a near-copy of `AVBBcmUserClient.cpp`: same dispatch table, `OSDynamicCast(AVBI210Driver, provider)`, log prefix `AVBI210: uc:`.
  - `AVBI210Driver.cpp`:
    - MMIO helpers (`rd`/`rdRaw`/`wr`/`wrFlush`/`pollUs`/`setFault`), DMA helpers and the ivars layout follow the BCM file.
    - Log prefix `AVBI210:`, so the hardware step can run `log stream --predicate 'eventMessage CONTAINS "AVBI210"'`.
    - `Start` flow:
      1. `super::Start`, `OSDynamicCast(IOPCIDevice)`, `Open`.
      2. Read cfg 0x00/0x2c. If `!isOurDevice`: `fault = kAvbFaultIdMismatch`, `Close`, return `kIOReturnUnsupported` with no MMIO.
      3. Read cfg 0x08 → `hw_rev`.
      4. `GetBARInfo(kPCIMemoryRangeBAR0)` with size ≥ 0x20000.
      5. Set Memory Space only.
      6. Timer, queue and action setup, then `RegisterService`.
      7. `stageA`.
    - `stageA` writes exactly:
      1. Read `STATUS` (all-ones → fault, stop).
      2. `IMC = 0xFFFFFFFF`, `EIMC = 0xFFFFFFFF`, then read `ICR` once to clear.
      3. Read MAC from RAL0/RAH0.
      4. `TSAUXC &= ~DISABLE_SYSTIME`.
      5. `TIMINCA = 0`.
      6. `setClock(host ns)`.
      7. Log STATUS, CTRL, TSAUXC and TIMINCA. `stage = 1`.
    - `setClock(ns)`: `nsToSystim`, write SYSTIML then SYSTIMH, then flush.
    - `readClock`: SYSTIMR, SYSTIML, SYSTIMH. All-ones check on SYSTIMH only, because SYSTIML is arbitrary data.
    - Selectors working after this item:
      - `kGetInfo`: `ctrl_a = CTRL`, `ctrl_b = STATUS`, `clock_ctl = TIMINCA`, `nic_kind = AVB_NIC_I210`.
      - `kReadClock`, `kSetClock` (readback via `readClock`).
      - `kAdjFreq`: reject `|ppb| > 488281`, write `timincaFromPpb`, return the value.
      - `kReadReg32`: `isReadableRegOffsetI210`, `rdRaw`, never latches a fault.
    - `kInitDatapath`, `kPollRx`, `kSendFrame` and `kGetTxTimestamp` return `kIOReturnNotReady` until item 5. `kShutdownDatapath` is a no-op.
    - `Stop`: disable the timer, shutdown, clear Bus Master, `Close`.
  - Files: `driver/AVBI210Driver.xcodeproj/project.pbxproj`, `driver/AVBI210Driver/{Info.plist,AVBI210Driver.entitlements,AVBI210Driver.iig,AVBI210Driver.cpp,AVBI210UserClient.iig,AVBI210UserClient.cpp}`.
  - Verify:
    - `TMPDIR=/Users/dexter/AVB-Audio/.agents/tmp/ xcodebuild -project /Users/dexter/AVB-Audio/driver/AVBI210Driver.xcodeproj -target AVBI210Driver -configuration Debug -sdk driverkit SYMROOT=/Users/dexter/AVB-Audio/build/driver OBJROOT=/Users/dexter/AVB-Audio/build/driver/obj CODE_SIGNING_ALLOWED=NO build` gives `** BUILD SUCCEEDED **`, 0 warnings, and `build/driver/Debug-driverkit/com.avbaudio.installer.i210.dext`.
    - `lipo -archs` on its executable prints `arm64e`.
    - `plutil -lint` passes. `plutil -p` shows `IOPCIPrimaryMatch` 0x15338086, `IOPCISecondaryMatch` 0x02141ab6, `IOProbeScore` 100000, `IOPCITunnelCompatible` 1, and no `IOMatchCategory`/`IOPCIMatch`/`IOPCIClassMatch`/`IONameMatch`.
    - `make -C /Users/dexter/AVB-Audio/driver/tests test` still passes.

- [ ] 5. **Stage B: reset, PHY/link, queues, PTP timestamping (G3 and G4 code paths).** Depends on 4. Implement `kInitDatapath(flags)`; log every step and its bounded-poll result.
  1. **Preconditions.** `stage == 1` and no fault, else `kIOReturnNotReady`. Allocate DMA: status-free design, so allocate the RX ring 2048 B, RX buffers 128×2 KiB, TX ring 1024 B, TX bounce 16×2 KiB. On any failure: `kAvbFaultDma`, free, return.
  2. **Quiesce and reset** (333016 §4.3 software reset / §4.5 init):
     1. `IMC = all`, `RCTL = 0`, `TCTL = PSP`, flush, `IOSleep(10)`.
     2. `CTRL |= GIO_MASTER_DISABLE`, then poll `STATUS.GIO_MASTER_ENABLE == 0` every 100 µs for up to 80 ms. Timeout → `kAvbFaultMasterDisable`.
     3. `CTRL |= RST`, then `IOSleep(10)`.
     4. Poll `EEMNGCTL.CFG_DONE0` for up to 100 ms (1 ms steps) and `STATUS.PF_RST_DONE`, per the datasheet. Timeout → `kAvbFaultResetTimeout`.
     5. `IMC = all`, `EIMC = all`, read `ICR`.
     6. Then set Bus Master. Order: reset first, so nothing DMAs with stale IOVAs.
  3. **PHY/link.**
     - Acquire the SW/FW PHY semaphore (SWSM.SMBI, then SWESMBI, then SW_FW_SYNC.SW_PHY_SM when FW_PHY_SM is clear; each poll ≤ 100 ms; timeout → `kAvbFaultSwFwSync`). Release it after every MDIO burst.
     - `CTRL`: set SLU, clear FRCSPD/FRCDPLX (copper autoneg through the internal PHY).
     - If `PHPM.D0LPLU` is set, clear it.
     - Over MDIC (READY poll ≤ 10 ms each; ERROR → log and stop the link step, not a fault): read BMCR (0), BMSR (1), ANAR (4), 1000T_CTRL (9).
     - If BMCR.PDOWN is set or AN is off, write BMCR = AN_ENABLE | AN_RESTART (PDOWN cleared). Do not reset the PHY and do not change advertisements. Log 1000T_CTRL bit 9 (1000FD advertised).
     - Poll `STATUS.LU` every 10 ms for up to 10 s, then `linkFromStatus` → `AVB_FLAG_LINK_UP`/`AVB_FLAG_1000FD`. No link is not a fault: log and continue, and the probe reports it.
  4. **RX queue 0.**
     - Fill 128 advanced read descriptors.
     - `RXPBS |= CFG_TS_EN`; `SRRCTL0 = srrctlValue()`.
     - RDBAL/RDBAH/RDLEN; RDH = RDT = 0.
     - `RXDCTL0 = rxdctlEnable()`, poll ENABLE readback ≤ 10 ms (`kAvbFaultQueueEnable`), then `RDT0 = 127`.
     - Timestamping: `ETQF3 = etqf1588Value()`, `TSYNCRXCTL = (rd & ~(TYPE_MASK|ENABLED)) | ENABLED | TYPE_ALL`, `TSYNCRXCFG = 0`.
     - `RCTL = rctlValue(flags & PROMISC)`: always EN | BAM | SECRC | MPE, which accepts 01:80:C2:00:00:0E; UPE added when promisc.
  5. **TX queue 0.** TDBAL/TDBAH/TDLEN; TDH = TDT = 0. `TXDCTL0 = txdctlEnable()`, poll ENABLE. `TCTL = tctlValue()`. `TSYNCTXCTL |= ENABLED`.
  6. **Clock re-arm.** The reset may clear the timer, so repeat the Stage A clock writes (TSAUXC, TIMINCA = 0, setClock host ns). `stage = 2`, enable and arm the 2 ms timer.
  7. **Harvest** (timer and start of `kPollRx`). While the descriptor at `rxNext` has DD:
     - Decode the writeback and take `harvest_mach`.
     - If TSIP: `hw_ts = parseRxTsHeader(buf)`, `has_ts = 1`, frame = buf + 16, len − 16.
     - If no error and EOP: copy ≤ 128 B into the software ring (BCM `swPush` semantics).
     - Re-arm the descriptor with its buffer IOVA, advance. Finally write `RDT0 = last re-armed index`.
     - `type_flags = status_error & 0xFFFF`.
  8. **`kSendFrame`** (14..1514 B, zero-pad to 60):
     - Busy if a timestamped TX is pending or the ring has no free slot (DD-based reclaim).
     - If `wantTs`: release a stale `TSYNCTXCTL.VALID` (read TXSTMPL/H).
     - Write the advanced descriptor (RS on every descriptor), fence, `mach` at the doorbell, `wrFlush(TDT0)`.
  9. **TX timestamp check** (timer and `kGetTxTimestamp`): if pending and `TSYNCTXCTL.VALID`, read TXSTMPL then TXSTMPH → ns, status ok, `tsMach`. Timeout 300 ms → `kAvbTxTsTimeout`.
  10. **`kShutdownDatapath` / `Stop`**, each step bounded and failures latched as faults:
      1. `RCTL &= ~EN`.
      2. `RXDCTL0`/`TXDCTL0` clear ENABLE, poll clear ≤ 10 ms.
      3. `TCTL &= ~EN`; `TSYNCRXCTL &= ~ENABLED`; `TSYNCTXCTL &= ~ENABLED`; `RXPBS &= ~CFG_TS_EN`.
      4. Clear Bus Master, free DMA, `stage = 1`.

      Never PHY reset or power-down. Apple's driver re-initialises the device when it rebinds.
  - Files: `driver/AVBI210Driver/{AVBI210Driver.cpp,AVBI210Driver.iig,I210Regs.h,I210Logic.hpp}`, `driver/tests/i210logic_tests.cpp` (any new helper gets a test).
  - Verify:
    - The item 4 xcodebuild command succeeds with 0 warnings.
    - `make -C /Users/dexter/AVB-Audio/driver/tests test` passes.

- [ ] 6. **Probe CLI on the NIC profile (G1–G4 runnable against the I210).** Depends on 2 (and on the 5 ABI for runtime).
  - `iokit.rs`:
    - `Device::open(NicSelect)` per §1.11. Not found → "neither AVBI210Driver nor AVBBcmDriver service found …" (or the specific one), with no panic.
    - `Device::profile()`.
    - `read_reg32` checks `profile.reg_read_limit`; `adj_freq` checks `profile.max_ppb`.
  - `main.rs`:
    - Global `--nic` flag. The `about` text becomes NIC-neutral.
    - `info` prints the profile label and uses `ctrl_a_name`/`ctrl_b_name`/`clock_ctl_name`. The BCM-only lines (57766 check, TIME_SYNC_ENABLE, APE/ASF/SIG) print only for the BCM. The I210 prints link and speed from STATUS.
    - `regs` uses `profile.reg_dump`.
    - `adjfreq` uses `profile.correction_from_ppb`/`expected_rate_ppb`. The default `--ppb 100000` is valid for both.
    - `pdelay`: default `--count 30`; for each exchange, raw and compensated delay (§1.12) and NRR versus the previous complete exchange; a summary with mean/stddev/median/min/max (raw and compensated) and NRR mean/stddev; the JSON `PdelayRecord` gains those fields.
    - `all`: info → clock 10 s → adjfreq ±100 000 ppb 10 s/window → datapath up → rx-ptp 10 s → pdelay 30 → datapath down.
  - Files: `crates/avb-probe/src/{iokit.rs,main.rs}`.
  - Verify:
    - `TMPDIR=/Users/dexter/AVB-Audio/.agents/tmp/ cargo build --manifest-path /Users/dexter/AVB-Audio/Cargo.toml --workspace --release --locked` succeeds.
    - `/Users/dexter/AVB-Audio/target/release/avb-probe --help` shows `--nic`.
    - `/Users/dexter/AVB-Audio/target/release/avb-probe info` (no dext loaded) exits non-zero with the not-found message.
    - `cargo test --workspace --locked` passes.

- [ ] 7. **Installer targets the I210 dext.**
  - `crates/avb-installer/src/main.rs`:
    - `DEXT_I210 = "com.avbaudio.installer.i210"`, `DEXT_BCM = "com.avbaudio.installer.bcm57762"`.
    - `activate` always uses I210.
    - `deactivate [--dext i210|bcm57762]` defaults to i210. Anything else → usage, exit 64.
    - `status` prints the `systemextensionsctl list` lines for both IDs.
    - The approval message names `AVBI210Driver`. The module doc is NIC-neutral.
  - `packaging/probe.entitlements`: `userclient-access` = both bundle IDs.
  - Files: `crates/avb-installer/src/main.rs`, `packaging/probe.entitlements`.
  - Verify:
    - `cargo build --manifest-path /Users/dexter/AVB-Audio/Cargo.toml -p avb-installer --release --locked` succeeds; clippy `-D warnings` is clean.
    - `/Users/dexter/AVB-Audio/target/release/avb-installer activate` outside the bundle prints the "must run from inside AVBInstaller.app" error, exit 1.
    - `avb-installer deactivate --dext foo` exits 64.

- [ ] 8. **`package.sh` builds, embeds, stamps and signs the I210 dext.**
  - `DEXT_ID="com.avbaudio.installer.i210"`, `DEXT_ENT="$ROOT/driver/AVBI210Driver/AVBI210Driver.entitlements"`.
  - Build the I210 project, then the BCM project, with the same SYMROOT/OBJROOT (compile check only; the BCM dext is not copied).
  - Embed only `$BUILD/driver/Debug-driverkit/com.avbaudio.installer.i210.dext`.
  - After copying, fail if `lipo -archs "$DEXT/$(PlistBuddy -c 'Print :CFBundleExecutable' "$DEXT/Info.plist")"` is not exactly `arm64e`.
  - Keep the version-stamp block unchanged (it applies to the I210 dext and the app), the `plutil -lint` list (swap in the I210 entitlements path), and the inside-out signing order (dext with `$DEXT_ENT`, then probe, then app).
  - Also print `plutil -p` of the embedded dext's `IOKitPersonalities` so the evidence shows the match dictionary.
  - Files: `scripts/package.sh`.
  - Verify:
    - `/Users/dexter/AVB-Audio/scripts/package.sh --adhoc` exits 0.
    - `codesign --verify --deep --strict /Users/dexter/AVB-Audio/build/AVBInstaller.app` passes.
    - `Contents/Library/SystemExtensions/` contains only `com.avbaudio.installer.i210.dext`.
    - Its `CFBundleVersion` matches `^[0-9]{1,4}\.[0-9]{1,2}\.[0-9]{1,2}$`.
    - `codesign -d --entitlements -` on the dext shows `IOPCIPrimaryMatch` 0x15338086, and on the probe shows both bundle IDs.
    - The hardware step runs the real-identity `package.sh` (no `--adhoc`).

- [ ] 9. **Full loop gate and evidence.** Run in order:
  1. `cargo fmt --manifest-path /Users/dexter/AVB-Audio/Cargo.toml --all --check`
  2. clippy `--workspace --all-targets --locked -- -D warnings`
  3. `cargo test --workspace --locked`
  4. `make -C /Users/dexter/AVB-Audio/driver/tests test`
  5. both xcodebuild commands, from a deleted `build/`
  6. `scripts/package.sh --adhoc`

  All with `TMPDIR=/Users/dexter/AVB-Audio/.agents/tmp/`. Record each command, its exit code and its key output (test counts, warnings, `lipo -archs`, `plutil -p` personality, entitlements) in `.agents/tasks/phase0b-build-verification.md`. Then confirm `git -C /Users/dexter/AVB-Audio status` shows no build outputs and nothing new under `/tmp`.
  - Verify: all commands exit 0, and the evidence file exists.

## 6. Hardware gates (hardware-test step only; every claim below needs the real I210 + 16R)

**All of G1–G4 require real hardware.** Code reading and the build loop prove none of them. Report raw numbers in `.agents/tasks/phase0b-hw-results.json` (`avb-probe --nic i210 all --json …`, plus separate runs as needed) and in `docs/phase0b-results.md`, in the same format as `docs/phase0-results.md`.

- **G1: match won, nothing else touched.** Measure:
  - `ioreg -p IOService -l -w0` shows `AVBI210Driver` (IOUserService) under the 8086:1533 / 1ab6:0214 `ethernet` IOPCIDevice.
  - `DriverKit_AppleEthernetE1000` is absent from it, and en7 is gone (expected and approved).
  - `systemextensionsctl list` shows `com.avbaudio.installer.i210 … [activated enabled]`. The BCM entry may still show "terminating for uninstall" until the user reboots later (see §7).
  - The BCM57762 14e4:1682 is untouched by the I210 dext. Record its owner: `AVBBcmDriver` until the reboot, `BCM5701Enet` after. `avb-probe info` shows `nic_kind` I210, vendor `0x15338086`, subsys `0x02141ab6`, rev 3, the MAC, and no fault.
  - If it doesn't win: capture the owner's `IOProbeScore`/`IOMatchedPersonality`, and `log show --last 10m --predicate 'process == "kernelmanagerd" OR process == "sysextd" OR eventMessage CONTAINS "AVBI210"'`. Try a replug, then a reboot, then write it up. No kext fallback without explicit user confirmation.
- **G2a: SYSTIM ticks at ns rate.** `avb-probe --nic i210 clock --seconds 10`:
  - strictly monotonic;
  - slope versus `mach_absolute_time` = 1.0 ± 200 ppm;
  - report the residual RMS (8 ns granularity expected) and the bracket median/max.
- **G2b: TIMINCA frequency adjust.** `adjfreq --ppb 100000 --seconds 10`:
  - the +P and −P windows change the rate by the right sign, within ±10 % of 100 ppm, or with a consistent recorded scale factor;
  - the baselines agree within ±5 ppm;
  - the register shows `0x00346DC6`/`0x80346DC6`;
  - TIMINCA is restored to 0.
  - If the clock fails, retry once after `datapath up`.
- **G3: RX timestamps (the gate that failed on the BCM).** `datapath up` (expect link_up=1, 1000FD=1), then `rx-ptp --seconds 30`:
  - at least 25 of the 16R's Pdelay_Req (~1/s), every one with `has_ts=1`;
  - successive hw_ts intervals match harvest intervals within ±1 ms;
  - no repeated hw_ts;
  - report counts and the max interval mismatch.
- **G4: TX timestamps and link delay.** `pdelay --count 30` or more:
  - every Pdelay_Req has TX status ok, with t1 bracketed;
  - at least 30 complete exchanges (t1–t4);
  - **pass: mean compensated link delay < 800 ns with a small stddev** (expect tens of ns on a direct cable; report mean, stddev, median, min, max, raw and compensated);
  - neighbor rate ratio mean within 1 ± 200 ppm with a small stddev (report both).
  - A negative or > 800 ns delay is a finding (timestamp point or latency), not a silent pass.

## 7. User-action gates (current host state, per user update)

Already done; do not ask the user about these:
- The 16R is cabled directly to the I210 adapter.
- The user's LAN is on Wi-Fi (en0). en7 going away is fine. Never ask the user to move a LAN cable.
- The user has approved taking over the I210 as soon as the build is ready.

Not done yet: **the user has not rebooted**, and will reboot later. The BCM dext is still "terminating for uninstall but still running" and still owns the Broadcom. That's acceptable, because it's a different device and a different bundle ID.

Hardware-step procedure:
1. **Package and activate yourself, without asking first.** Run `scripts/package.sh` (real identity, Team 8NG8NMGYVF), then `build/AVBInstaller.app/Contents/MacOS/avb-installer activate`.
2. **Do not wait for a reboot**, unless sysextd actually refuses to activate the I210 dext while the BCM uninstall is pending. If it refuses, report the exact error text and code, and only then ask for a reboot.
3. **After activation, check `ioreg` for `AVBI210Driver` on the 8086:1533 device.**
4. **Pause for the user only when it is actually needed**, in one `send_message` warning:
   - if the installer prints "user approval required": approve the new dext in System Settings → General → Login Items & Extensions → Driver Extensions;
   - and/or if `ioreg` still shows `DriverKit_AppleEthernetE1000` after activation: unplug and replug the I210 Thunderbolt adapter.

   Then re-check `ioreg`.
5. **G1 adjustment for this state.** While the reboot is pending, the Broadcom may still be owned by `AVBBcmDriver` instead of `BCM5701Enet`. Record which driver owns it. The G1 requirement is "Broadcom untouched by the I210 work": our I210 dext never touches it, and the BCM returns to Apple's driver after the user's later reboot. Do not fail G1 on that alone.
6. **The 16R stays powered through G3/G4.** Assume it is on; confirm via `rx-ptp` traffic instead of asking.
- Rollback (document; run only on request): `avb-installer deactivate` (or `systemextensionsctl uninstall 8NG8NMGYVF com.avbaudio.installer.i210`), then replug. Apple's E1000 and en7 return.
- Never run sudo commands that change boot security or NVRAM. The host is already configured.

## 8. Reviewer checklist (phase0b-review.json)

- Only 8086:1533 / 1ab6:0214 can match or be touched: personality, entitlement, the `Start` check, and the `isOurDevice` tests (BCM, I211 and other-subsystem rejections).
- No `IOMatchCategory`/`IOPCIClassMatch`/`IOPCIMatch`/`IONameMatch`; `IOProbeScore` 100000; arm64e; kext-style version stamp.
- Every write is in a named §5 sequence and is a documented 333016 register. No generic write path. The `kReadReg32` denylist is present.
- Every poll is bounded, and the fault latch blocks MMIO. Bus Master is set only after the Stage B reset.
- No igb/igb_avb code or comments copied, and the *(verify)* constants are resolved (or deviations recorded).
- ABI v2 sizes agree in C++ and Rust. The BCM dext still builds. The probe works against both profiles (unit tests).
- The installer and package embed and activate only the I210 dext. The probe entitlement lists both IDs.
- `phase0b-build-verification.md` shows every item 9 command passing.
- G1–G4 are not marked proven. That is the hardware-test step's job.

## Deviations

(Implementer: record any deviation from the decisions above here, with the datasheet section or concrete failure that forced it.)

Checked against 333016 rev 3.7 (public PDF). Where the plan differed, the datasheet was followed:

1. TIMINCA (8.15.11): Incvalue is bits 30:0, not 23:0, and INC_TIME = 8 ns ± Incvalue·2⁻³² ns, so TIMINCA = 0 means the nominal 8 ns per tick. The dext still caps Incvalue at 24 bits (|ppb| ≤ 488 281), so the golden table is unchanged.
2. ICR/IMC (8.8): the primary offsets are ICR 0x1500 and IMC 0x150C. 0x00C0/0x00D8 are documented aliases. The dext uses the primary ones, and both ICR offsets are on the read denylist.
3. MDIC (8.2.4): bits 25:21 are reserved on the I210, and the PHY address comes from MDICNFG (default = internal PHY). The frames therefore carry no PHYADD, and `mdicReadFrame/WriteFrame` take only the register.
4. TCTL (8.12.1): bits 21:12 are BST (back-off slot time, default 0x40), not COLD. `tctlValue()` = EN | PSP | CT 0xF | BST 0x40 = 0x000400FA.
5. RX writeback (7.1.4.2.2): the extended error field is 31:20, with RXE at bit 31 (error bit 11). Only RXE is treated as a frame error, since L4E/IPE don't apply to L2 PTP.
6. Reset completion (4.3.1 note): the datasheet requires EEC.Auto_RD (0x12010 bit 9) and STATUS.PF_RST_DONE after a ≥ 3 ms wait. The dext polls Auto_RD, PF_RST_DONE and EEMNGCTL.CFG_DONE0 together (≤ 100 ms) after a 10 ms sleep.
7. Shutdown order: 4.5.9.2 says the RX path can only be disabled after all RX queues are. The dext clears RXDCTL0.ENABLE (and polls it) before clearing RCTL.RXEN. Per 4.5.10.1, TX waits up to 10 ms for TDH == TDT before clearing TXDCTL0.ENABLE.
8. Timestamp latencies: 333016 7.8.3.1 characterises latency only at 100 Mb/s (Table 7-62: TX 984–1024 ns, RX 2148–2228 ns). The 1000 Mb/s values TX 178 / RX 448 ns are not in the datasheet. They are the Intel-published constants Linux igb uses (its 100 Mb/s pair, 1024/2213, matches Table 7-62). Treat them as unverified until G4.
9. RCTL also keeps DPF (bit 22, power-on default 1), so pause frames stay discarded after RCTL is rewritten.
10. Read denylist extended with other side-effect reads from 8.x: EICR 0x1580 and TSICR 0xB66C (read-to-clear), SWSM 0x5B50 (a read sets SMBI), AUXSTMPL0..AUXSTMPH1 0xB65C–0xB668 (latch release), and PEIND 0x1084, RR2DCDELAY 0x5BF4, BMTPDC/B2OSPC/O2BGPTC 0x8FDC–0x8FE4 (RC).
11. ETQF[3] sets only EType | Filter enable | 1588 time stamp. Queue Enable (bit 31) stays 0: no steering is needed with one queue.
12. SRRCTL0 keeps BSIZEHEADER at its default 4. It's ignored for DESCTYPE 001b, but writing the default avoids changing a field the plan didn't name.
13. TX timestamp descriptor bit: bit 19 is `MAC.2STEP_1588` (7.2.2.3.3), which latches into TXSTMPL/H. This is the plan's "MAC_TSTAMP".
14. Stage A also reads RAL0/RAH0 for the MAC (read only), as the plan lists, and flushes posted writes by reading STATUS, because IMC/EIMC are write-only.
