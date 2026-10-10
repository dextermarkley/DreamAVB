# Phase 0 Implementation Plan — BCM57762 PCIDriverKit dext + EAV/PTP timestamp proof

Branch: `phase0-bcm-dext` (already checked out). Repo root: `/Users/dexter/AVB-Audio`.
Chip facts come from `.agents/tasks/avb-hw-feasibility.md` (Q2, Q5, "BCM57762 listener-only design"). Tool versions come from `.agents/tasks/phase0-env.md`.

Loop contract (unchanged):
- The implementer runs first.
- The reviewer runs last and writes `/Users/dexter/AVB-Audio/.agents/tasks/phase0-review.json` with a top-level `"verdict"`. The loop stops on `"APPROVED"`.
- Then the hardware-test step runs (§6/§7), then the final commit on `phase0-bcm-dext`.

## 0. Ground rules (apply to every item)

- **GPL boundary.**
  - Linux `tg3.c`/`tg3.h` (https://raw.githubusercontent.com/torvalds/linux/master/drivers/net/ethernet/broadcom/tg3.{c,h}) may be read only to learn register offsets, bit values and the order of operations.
  - Never copy code, comments, structs or macro blocks.
  - `Bcm57762Regs.h` uses our own names and layout, with the header comment "offsets/bit values derived from the observable behaviour of the Linux tg3 driver; no code copied".
- **Device scope.**
  - Only PCI 14e4:1682 with subsystem 106b:00f6.
  - Never match, open or touch any other device. Never touch en7 (Intel I210 8086:1533, the user's main LAN).
- **Register safety.**
  - Write only what appears in the tg3 sequences for the 57765/57766 class (`ASIC_REV_57766`, flags `57765_PLUS`/`57765_CLASS`/`5755_PLUS`/`5750_PLUS`/`5705_PLUS`/`PCI_EXPRESS`/`CPMU_PRESENT`).
  - Every poll has a bounded timeout.
  - If a poll times out, or any read returns `0xFFFFFFFF` (device gone or hung), set `ivars->fault` and refuse all further hardware access until `Stop`.
  - Never "try other values". Back off, log, and report.
- **Unproven on this silicon.** Linux never sets `PTP_CAPABLE` for 57766. These three claims are hypotheses until the hardware-test step shows the §6 evidence:
  - (a) the EAV clock ticks and accepts frequency correction;
  - (b) the RX PTP latch works;
  - (c) the TX latch works.

  The build loop only proves that the code builds and that the pure logic is correct. The reviewer must not accept "works" claims based on code reading.
- **Pinned dependencies.** Exact `=x.y.z` versions, committed `Cargo.lock`, toolchain pinned in `rust-toolchain.toml`.
- **No user-visible system changes in the build loop.** The loop builds, unit-tests and packages only. It does not install or activate anything, does not run `csrutil`/`nvram`/`systemextensionsctl`, and does not touch the network. Everything that needs the device or user action happens in the hardware-test step (§7).

## 1. Design decisions

1. **Bring-up has two stages, driven by the user client.**
   - `Start()` performs Stage A only: map BAR0, program host control and swap settings, and read-modify-write `GRC_MODE |= GRC_MODE_TIME_SYNC_ENABLE`. No chip reset and no DMA.
   - Stage B (chip reset, rings, MAC/PHY, RX PTP filter) runs only when the probe calls `kInitDatapath`.
   - Rationale: G1 and G2 are proven with the fewest possible writes. A Stage B hang is isolated, and a replug brings the device back to Stage A.
2. **ExternalMethod-only user client in Phase 0. `CopyClientMemoryForType` is deferred to Phase 1.** gPTP runs at about 1–3 frames/s and every payload fits under the 4096-byte inline struct limit, so shared rings would add risk without proving anything here. Selectors 16–31 are reserved for Phase 1 shared rings.
3. **Polling, no interrupts.**
   - `MISC_HOST_CTRL_MASK_PCI_INT` stays set.
   - A 2 ms `IOTimerDispatchSource` on the driver's default queue harvests the RX return ring and the pending TX timestamp. The one-deep RX latch is then read before the next event frame even if the probe isn't polling.
   - ExternalMethod also runs on the default queue, so all hardware access is serialized without locks.
4. **Promiscuous RX in Phase 0** (`RX_MODE_PROMISC`) instead of programming the multicast hash. The dext keeps a 256-entry software RX ring. `kPollRx` filters by ethertype.
5. **Matching** (facts read on this host: Apple's `PCI Matching` personality for `BCM5701Enet` has no `IOProbeScore` and uses `IONameMatch` including `pci14e4,1682`; the device's `compatible` is `pci106b,f6`, `pci14e4,1682`, `pciclass,020000`):
   - One personality with:
     - `IOProviderClass = IOPCIDevice`
     - `IOPCIPrimaryMatch = "0x168214e4"`
     - `IOPCISecondaryMatch = "0x00f6106b"` (subsystem)
     - `IOPCITunnelCompatible = true`
     - `IOProbeScore = 100000`
     - `IOClass = IOUserService`, `IOUserClass = AVBBcmDriver`, `IOUserServerName = com.avbaudio.installer.bcm57762`
   - **No `IOMatchCategory`.** A custom category would let Apple's driver attach alongside ours instead of competing.
   - `UserClientProperties = { IOClass = IOUserUserClient; IOUserClass = AVBBcmUserClient; }`
6. **Build: hand-written Xcode project, then manual codesign.**
   - `driver/AVBBcmDriver.xcodeproj` has one DriverKit driver target. Build it with `xcodebuild -project driver/AVBBcmDriver.xcodeproj -target AVBBcmDriver -configuration Debug -sdk driverkit SYMROOT=$PWD/build/driver CODE_SIGNING_ALLOWED=NO build`. Xcode runs `iig` and lays out the dext bundle.
   - `scripts/package.sh` signs everything afterwards with `codesign` and explicit entitlement files.
   - Rationale: the host has only an "Apple Development" identity (probably a personal team without DriverKit grants). Xcode automatic signing would fail on provisioning, so the dev path is manual codesign + SIP off + `amfi_get_out_of_my_way=1`.
7. **The installer is Rust: `crates/avb-installer`, using `objc2` + `objc2-system-extensions`.**
   - It is a CLI binary placed at `build/AVBInstaller.app/Contents/MacOS/avb-installer` and executed directly, so `NSBundle.mainBundle` is the app bundle.
   - Subcommands: `activate`, `deactivate`.
   - It submits `OSSystemExtensionRequest activationRequestForExtension:queue:` (or `deactivationRequestForExtension:queue:`) on the main dispatch queue and implements `OSSystemExtensionRequestDelegate` with `objc2::define_class!`:
     - `actionForReplacingExtension` returns `.replace`.
     - `requestNeedsUserApproval` prints the System Settings instruction.
     - `didFinishWithResult` and `didFailWithError` print the result and exit.
   - It runs `NSRunLoop.mainRunLoop` with a 300 s timeout.
   - objc2-system-extensions 0.3.2 binds the whole framework (default features), so Rust is practical. Swift is not planned. Moving to a minimal Swift host is allowed only after a concrete objc2 failure, recorded under "Deviations" at the end of this file.
8. **The probe is Rust: `crates/avb-probe`.**
   - IOKit access uses `io-kit-sys` (`IOServiceNameMatching("AVBBcmDriver")`, `IOServiceGetMatchingService`, `IOServiceOpen` type 0, `IOConnectCallMethod`). Timing uses `mach2` (`mach_absolute_time`, `mach_timebase_info`).
   - All math lives in pure library modules with unit tests. `iokit.rs` stays thin.
9. **IDs.**
   - App `com.avbaudio.installer`. Dext `com.avbaudio.installer.bcm57762` (prefixed by the app ID).
   - Classes `AVBBcmDriver : IOService` and `AVBBcmUserClient : IOUserClient`.
10. **Toolchain.**
    - `rust-toolchain.toml` pins `channel = "1.99.0"` (current stable) with `clippy` and `rustfmt`. The host has 1.69, and objc2 0.6 needs ≥ 1.71.
    - `rustup toolchain install 1.99.0` is a medium-risk local action: mention it, don't ask.
    - DriverKit SDK on host: `DriverKit25.5.sdk` (Xcode 26.6).
11. **DMA.**
    - `IOBufferMemoryDescriptor::Create(kIOMemoryDirectionInOut, size, 4096, &buf)`, then `IODMACommand::Create(pci, kIODMACommandCreateNoOptions, &spec{maxAddressBits=64}, &cmd)`, then `PrepareForDMA`.
    - Each ring and the status block get one buffer, each a single segment. Assert `segmentCount == 1`, else fault.
    - The DART IOVA is what goes into descriptors and `HOSTCC_STATUS_BLK_HOST_ADDR`.

## 2. Deliverable layout

```
/Users/dexter/AVB-Audio/
├── Cargo.toml              # [workspace] members=["crates/avb-installer","crates/avb-probe"], resolver="2"
├── Cargo.lock              # committed
├── rust-toolchain.toml
├── crates/
│   ├── avb-installer/{Cargo.toml, src/main.rs}
│   └── avb-probe/{Cargo.toml, src/main.rs, src/lib.rs, src/abi.rs, src/iokit.rs,
│                  src/ptp.rs, src/stats.rs, src/freq.rs, src/report.rs}
├── driver/
│   ├── AVBBcmDriver.xcodeproj/project.pbxproj
│   ├── AVBBcmShared.h             # user-client ABI (§3); included by dext, mirrored in abi.rs
│   ├── AVBBcmDriver/
│   │   ├── Info.plist             # decision 5
│   │   ├── AVBBcmDriver.entitlements
│   │   ├── AVBBcmDriver.iig, AVBBcmDriver.cpp
│   │   ├── AVBBcmUserClient.iig, AVBBcmUserClient.cpp
│   │   ├── Bcm57762Regs.h         # our constants (§4)
│   │   └── Tg3Logic.hpp           # pure helpers, no DriverKit includes
│   └── tests/{Makefile, tg3logic_tests.cpp}
├── packaging/{Installer-Info.plist, installer.entitlements, probe.entitlements}
└── scripts/package.sh
```

Exact pins:
- avb-installer: `objc2 = "=0.6.5"`, `objc2-foundation = "=0.3.2"`, `objc2-system-extensions = "=0.3.2"`, `block2 = "=0.6.2"`, `dispatch2 = "=0.3.1"`, `anyhow = "=1.0.104"`.
- avb-probe: `io-kit-sys = "=0.5.0"`, `core-foundation-sys = "=0.8.7"`, `mach2 = "=0.7.0"`, `clap = { version = "=4.6.7", features = ["derive"] }`, `anyhow = "=1.0.104"`, `serde` + `serde_json` with exact versions looked up on crates.io at implementation time.

Entitlements:
- Dext (`driver/AVBBcmDriver/AVBBcmDriver.entitlements`):
  - `com.apple.developer.driverkit` = true
  - `com.apple.developer.driverkit.transport.pci` = `[{ IOPCIPrimaryMatch = "0x168214e4" }]`
  - `com.apple.developer.driverkit.allow-any-userclient-access` = true (dev only)
- Installer (`packaging/installer.entitlements`): `com.apple.developer.system-extension.install` = true.
- Probe (`packaging/probe.entitlements`): `com.apple.developer.driverkit.userclient-access` = `["com.avbaudio.installer.bcm57762"]`.

The installer's Info.plist needs: `CFBundleIdentifier=com.avbaudio.installer`, `CFBundleExecutable=avb-installer`, `CFBundlePackageType=APPL`, `CFBundleShortVersionString`/`CFBundleVersion`, `LSMinimumSystemVersion=26.0`.
The dext's Info.plist needs: `CFBundlePackageType=DEXT`, `OSBundleUsageDescription`, and matching `CFBundleShortVersionString`/`CFBundleVersion`. sysextd replaces an extension only when the version increases, so `package.sh` stamps `CFBundleVersion` with a UTC timestamp.

## 3. User-client ABI (`driver/AVBBcmShared.h` ⇄ `crates/avb-probe/src/abi.rs`)

- Fixed-width types and natural alignment. Every struct has a `static_assert(sizeof == N)` in C and a matching `#[repr(C)]` size test in Rust. `AVB_ABI_VERSION = 1`.
- Structure output is kept ≤ 4096 bytes so it travels inline.

| Sel | Name | Input | Output |
|---|---|---|---|
| 0 | `kGetInfo` | — | struct `AvbInfo` (fields below) |
| 1 | `kReadClock` | scalar n (1..64) | struct: n × `ClockSample { u64 mach_before; u64 eav_ns; u64 mach_after; }` |
| 2 | `kSetClock` | scalar u64 ns | scalar: immediate readback ns |
| 3 | `kAdjFreq` | scalar i64 ppb, \|ppb\| ≤ 1,000,000, else `kIOReturnBadArgument` | scalar: raw `EAV_REF_CLK_CORRECT_CTL` value written |
| 4 | `kInitDatapath` | scalar flags (bit0 promisc, bit1 rx_ptp_v2_l2) | scalar: stage |
| 5 | `kPollRx` | scalars: ethertype filter (0 = all), max entries | struct `RxBatch { u32 count; u32 dropped; RxEntry e[≤24]; }` (fields below) |
| 6 | `kSendFrame` | struct: frame bytes (60..1514, no FCS); scalar want_ts | scalars: tx_seq, mach_at_doorbell |
| 7 | `kGetTxTimestamp` | scalar tx_seq | scalars: status (0 pending, 1 ok, 2 timeout, 3 not-requested), hw_ts, mach_latched |
| 8 | `kReadReg32` | scalar BAR0 offset (< 0x10000, 4-aligned) | scalar value |
| 9 | `kShutdownDatapath` | — | — |
| 16–31 | reserved (Phase 1 shared rings) | | |

`AvbInfo` fields:
- `u32 abi_version`
- `u32 vendor_device`, `u32 subsys`
- `u32 asic_rev`: cfg 0xFC `GEN15_PRODID_ASICREV`
- `u32 misc_host_ctrl`, `u32 grc_mode`
- `u32 stage`: 0 none, 1 timesync, 2 datapath
- `u32 fault`: last fault code
- `u32 flags`: bit0 ape, bit1 asf, bit2 link_up, bit3 1000FD
- `u8 mac[6]`, `u8 pad[2]`
- `u32 tb_numer`, `u32 tb_denom`

`RxEntry` fields:
- `u64 harvest_mach`, `u64 hw_ts`
- `u32 len`
- `u16 type_flags`: raw RX BD flags
- `u8 has_ts`, `u8 pad`
- `u8 data[128]`

There is no generic register-write selector. All writes go through the named, sequence-checked operations.

## 4. Register reference (offsets from tg3 behaviour; BAR0 = memory index 0, 64 KiB per host ioreg `reg`)

Host interface. The PCI config registers are also reachable through BAR0 at the same offsets:
- `MISC_HOST_CTRL` 0x68:
  - Bits: CLEAR_INT 0x1, MASK_PCI_INT 0x2, WORD_SWAP 0x8, PCISTATE_RW 0x10, INDIR_ACCESS 0x80, TAGGED_STATUS 0x200, CHIPREV mask 0xffff0000.
  - Little-endian value: `MASK_PCI_INT|WORD_SWAP|INDIR_ACCESS|PCISTATE_RW`, written via `ConfigurationWrite32`.
- Other registers: `DMA_RW_CTRL` 0x6c, `PCISTATE` 0x70, `CLOCK_CTRL` 0x74, `MEM_WIN_BASE_ADDR` 0x7c, `MEM_WIN_DATA` 0x84 (NIC SRAM window), `GEN15_PRODID_ASICREV` 0xfc (config; ASIC rev for 57762).
- NIC SRAM: `FIRMWARE_MBOX` 0xb50 (magic `0x4B657654`; bootcode writes `~magic` when done), `DATA_SIG` 0xb54 (`0x4B657654`), `DATA_CFG` 0xb58 (APE/ASF enable bits; read only).

GRC:
- `GRC_MODE` 0x6800:
  - Swap/endianness bits: WSWAP_NONFRM 0x4, BSWAP_DATA 0x10, WSWAP_DATA 0x20. Little-endian value: `WSWAP_DATA|BSWAP_DATA|WSWAP_NONFRM_DATA`.
  - Host bits: HOST_STACKUP 0x10000, HOST_SENDBDS 0x20000, NO_TX_PHDR_CSUM 0x100000, NO_RX_PHDR_CSUM 0x800000.
  - **TIME_SYNC_ENABLE 0x00080000.**
- `GRC_MISC_CFG` 0x6804: CORECLK_RESET 0x1, plus bit 29 on PCIe.
- `GRC_LOCAL_CTRL` 0x6808.
- `MEMARB_MODE` 0x4000: ENABLE 0x2.

EAV time-sync block:
- `EAV_REF_CLCK_LSB` 0x6900, `EAV_REF_CLCK_MSB` 0x6904. Read LSB first, then MSB; reading LSB is expected to latch MSB. Verify on hardware (§6 G2 monotonicity check).
- `EAV_REF_CLCK_CTL` 0x6908: STOP 0x2, RESUME 0x4, TSYNC_GPIO mask 0x3<<16.
  - Set-clock sequence: `ctl = rd(CTL)`; `wr(CTL, ctl|STOP)`; `wr(LSB, lo)`; `wr(MSB, hi)`; `wr(CTL, ctl|RESUME)`; then flush with a read.
- `EAV_WATCHDOG0_LSB/MSB` 0x6918/0x691c. **Do not touch in Phase 0** (periodic output, not needed).
- `EAV_REF_CLK_CORRECT_CTL` 0x6928: EN bit31, NEG bit30, magnitude mask 0xffffff.
  - Semantics: a 24-bit accumulator; on each clock the correction is added, and overflow steps the counter by ±1 extra.
  - Encoding: `corr = round(|ppb| × 2^24 / 1e9)`.
  - Value written: `EN | (ppb<0 ? NEG : 0) | (corr & 0xffffff)`, or `0` when corr == 0.
- PTP timestamp registers:
  - `TX_TSTAMP_LSB/MSB` 0x5c0/0x5c4
  - `RX_TSTAMP_LSB/MSB` 0x6b0/0x6b4
  - Timestamp mask `0x7fffffffffffffff`
- `RX_PTP_CTL` 0x6c8:
  - Message bits: SYNC 0x1, DELAY_REQ 0x2, PDLAY_REQ 0x4, PDLAY_RES 0x8 (ALL_V2_EVENTS = 0xF).
  - Mode bits: V2_L2_EN 0x00800000, V2_L4_EN 0x01000000, V1_EN 0x02000000, **HWTS_INTERLOCK 0x04000000**.
  - Phase 0 value: `V2_L2_EN | 0xF | HWTS_INTERLOCK`. Write it before the GRC_MODE write that sets TIME_SYNC_ENABLE, which is tg3's order in `reset_hw`.
- Descriptor flags:
  - TX BD `len_flags`: END 0x0004, HWTSTAMP 0x0020.
  - RX BD `type_flags`: END 0x0004, VLAN 0x0040, ERROR 0x0400.
  - PTPSTAT mask 0x0210: V1 = 0x0010, V2 = 0x0200. When PTPSTAT is V1 or V2, read `RX_TSTAMP_LSB` then `MSB` during harvest.

MAC / engines (Stage B):
- MAC: `MAC_MODE` 0x400, `MAC_STATUS` 0x404, `MAC_EVENT` 0x408, `MAC_ADDR_0_HIGH/LOW` 0x410/0x414, `MAC_RX_MTU_SIZE` 0x43c, `MAC_MI_COM` 0x44c, `MAC_MI_MODE` 0x454, `MAC_TX_MODE` 0x45c, `MAC_TX_LENGTHS` 0x464, `MAC_RX_MODE` 0x468, `MAC_HASH_REG_0..3` 0x470.., `MAC_RCV_RULE_0/VALUE_0` 0x480/0x484.
- Send path: `SNDDATAI_MODE` 0xc00, `SNDDATAC_MODE` 0x1000, `SNDBDS_MODE` 0x1400, `SNDBDI_MODE` 0x1800, `SNDBDC_MODE` 0x1c00.
- Receive path: `RCVLPC_MODE` 0x2000 (`RCVLPC_CONFIG` 0x2010, `RCVLPC_STATS_ENABLE` 0x2018), `RCVDBDI_MODE` 0x2400, `RCVDBDI_STD_BD` 0x2450 (BDINFO: host_addr hi/lo +0x0/+0x4, maxlen_flags +0x8, nic_addr +0xc), `RCVDCC_MODE` 0x2800, `RCVBDI_STD_THRESH` 0x2c18, `RCVCC_MODE` 0x3000, `RCVLSC_MODE` 0x3400.
- CPMU: `TG3_CPMU_CTRL` 0x3600 and related; read-only except where tg3 writes them for 57765-class.
- Host coalescing: `HOSTCC_MODE` 0x3c00, `RXCOL_TICKS` 0x3c08, `TXCOL_TICKS` 0x3c0c, `RXMAX_FRAMES` 0x3c10, `TXMAX_FRAMES` 0x3c14, `STATUS_BLK_HOST_ADDR` 0x3c38, `STATUS_BLK_NIC_ADDR` 0x3c44.
- Buffer manager and DMA: `BUFMGR_MODE` 0x4400 (and MB pool/watermarks 0x4408–0x4418), `RDMAC_MODE` 0x4800, `WDMAC_MODE` 0x4c00, `FTQ_RESET` 0x5c00, `MSGINT_MODE` 0x6000.
- NIC SRAM ring control blocks: `SEND_RCB` 0x100, `RCV_RET_RCB` 0x200.
- Mailboxes (direct MMIO on 57765-class; tg3 uses plain `writel` at `regs + off`, low 32 bits at `+4`):
  - `INTERRUPT_0` 0x204
  - `RCV_STD_PROD_IDX` 0x26c
  - `RCVRET_CON_IDX_0` 0x284
  - `SNDHOST_PROD_IDX_0` 0x304
- Host-memory structures, implemented as our own C structs with the same word order:
  - TX BD: 16 B = addr_hi, addr_lo, len_flags (len<<16 | flags), vlan_tag.
  - RX BD: 32 B = addr_hi, addr_lo, idx_len, type_flags, ip_tcp_csum, err_vlan, reserved, opaque.
  - Status block: status, status_tag, rx consumer pair, mini/reserved pair, then idx[0]: tx_consumer/rx_producer. With `WSWAP_NONFRM` on little-endian, the 16-bit pairs are swapped as tg3's `__LITTLE_ENDIAN` layout shows.
  - Ring sizes for this class: std RX ring 512, RX return ring per `tg3_rx_ret_ring_size()` for a 57765-class part, TX ring 512.

Before coding, the implementer must check each Stage B value against the tg3 functions named in §5 item 5. Record any value derived from tg3 *behaviour* (rather than this table) in a comment next to the constant.

## 5. Implementation items (build loop)

- [x] 1. **Cargo workspace and toolchain.**
  - Create the root `Cargo.toml`, `rust-toolchain.toml`, and the two crates as compiling stubs. `avb-installer` prints usage. `avb-probe` has the clap subcommand skeleton from item 7 and an empty lib.
  - Install the toolchain: `rustup toolchain install 1.99.0 --component clippy rustfmt`.
  - Files: `Cargo.toml`, `rust-toolchain.toml`, `crates/avb-installer/{Cargo.toml,src/main.rs}`, `crates/avb-probe/{Cargo.toml,src/main.rs,src/lib.rs}`, `Cargo.lock`.
  - Verify: `cargo build --workspace --locked` and `cargo fmt --all --check` both succeed. `cargo tree -p avb-probe` shows the exact pinned versions.

- [x] 2. **Shared ABI and pure helpers, with host tests.** Write `driver/AVBBcmShared.h` (§3), `driver/AVBBcmDriver/Bcm57762Regs.h` (§4) and `driver/AVBBcmDriver/Tg3Logic.hpp`. `Tg3Logic.hpp` is header-only, has no DriverKit includes, and provides:
  - `eavCorrectionFromPpb(int64)`, with round-trip tests at 0, ±1, ±100 000, ±1 000 000 ppb, `NEG` set for negative values, and `0` for corr==0;
  - TX BD encode (len<<16 | END | optional HWTSTAMP);
  - RX BD decode (len, ring idx, error, `ptpstat` → none/v1/v2);
  - the ring index wrap helper.

  Add `driver/tests/Makefile` (uses `xcrun clang++ -std=c++20 -Wall -Wextra -Werror`) and `driver/tests/tg3logic_tests.cpp`, a plain assert-based runner that also `static_assert`s every ABI struct size.
  - Verify: `make -C driver/tests test` prints all pass and exits 0.

- [x] 3. **Dext skeleton that matches, starts and serves the user client (G1 code path).**
  - Create the Xcode project with one DriverKit driver target `AVBBcmDriver`: product type `com.apple.product-type.driver-extension`, `SDKROOT=driverkit`, `DRIVERKIT_DEPLOYMENT_TARGET=25.0`, `ARCHS=arm64`, `PRODUCT_BUNDLE_IDENTIFIER=com.avbaudio.installer.bcm57762`, `INFOPLIST_FILE`, `CODE_SIGN_ENTITLEMENTS`, linking `DriverKit.framework` and `PCIDriverKit.framework`.
  - Add `Info.plist` per decision 5 and the entitlements per §2.
  - Implement `AVBBcmDriver::Start`:
    1. `Start(provider)` on super.
    2. `OSDynamicCast(IOPCIDevice)`, then `pci->Open(this, 0)`.
    3. Read config vendor/device (0x00) and subsystem (0x2c). If they are not `0x168214e4` / `0x00f6106b`, return `kIOReturnUnsupported` without touching anything. This is defence in depth.
    4. Set Memory Space + Bus Master in `kIOPCIConfigurationOffsetCommand`.
    5. `RegisterService()`.
  - Implement `Stop`: call `kShutdownDatapath` logic, clear Bus Master, `pci->Close`.
  - Implement `NewUserClient` via `Create(this, "UserClientProperties", &client)`.
  - Implement `AVBBcmUserClient::ExternalMethod` with an `IOUserClientMethodDispatch` table for §3. Selector 0 works; the others return `kIOReturnNotReady` until items 4–6.
  - Log with `os_log` under the prefix `AVBBcm:`, so the hardware-test step can run `log stream --predicate 'eventMessage CONTAINS "AVBBcm"'`.
  - Files: `driver/AVBBcmDriver.xcodeproj/project.pbxproj`, `driver/AVBBcmDriver/{Info.plist,AVBBcmDriver.entitlements,AVBBcmDriver.iig,AVBBcmDriver.cpp,AVBBcmUserClient.iig,AVBBcmUserClient.cpp}`.
  - Verify:
    - `xcodebuild -project driver/AVBBcmDriver.xcodeproj -target AVBBcmDriver -configuration Debug -sdk driverkit SYMROOT=$PWD/build/driver CODE_SIGNING_ALLOWED=NO build` ends in `** BUILD SUCCEEDED **` and produces `build/driver/Debug-driverkit/com.avbaudio.installer.bcm57762.dext`.
    - `plutil -lint` passes on its Info.plist.
    - `plutil -p` shows `IOPCIPrimaryMatch` 0x168214e4, `IOPCISecondaryMatch` 0x00f6106b, `IOProbeScore` 100000, `IOPCITunnelCompatible` 1, and no `IOMatchCategory`.

- [x] 4. **Stage A: time-sync enable and EAV clock access (G2 code path).** In `Start` after `RegisterService`, run Stage A:
  1. Read `GEN15_PRODID_ASICREV` (cfg 0xFC) and `MISC_HOST_CTRL`, and log both. tg3 maps 57762 to `ASIC_REV_57766` (`asic_rev >> 12 == 0x57766`). If the ASIC rev is not 57766, stay at stage 0 and log. Do not write anything.
  2. Write `MISC_HOST_CTRL = MASK_PCI_INT|WORD_SWAP|INDIR_ACCESS|PCISTATE_RW` via config space.
  3. Write `MEMARB_MODE |= ENABLE` (read-modify-write).
  4. `g = rd(GRC_MODE)`, then `wr(GRC_MODE, g | TIME_SYNC_ENABLE)`. Read it back and log it.
  5. Read `NIC_SRAM DATA_SIG/DATA_CFG` through the `MEM_WIN` window, then restore the window base to 0. Record the APE/ASF bits in `AvbInfo.flags`.
  6. Set `stage = 1`.

  Then implement `kReadClock`: per sample, `mach_before = mach_absolute_time()`, read LSB, read MSB, `mach_after`. Implement `kSetClock` with the STOP/LSB/MSB/RESUME sequence plus readback, and `kAdjFreq` using `eavCorrectionFromPpb`. Do not touch WATCHDOG or the GPIO bits.
  - Files: `driver/AVBBcmDriver/AVBBcmDriver.{iig,cpp}`, `AVBBcmUserClient.cpp`.
  - Verify: the item 3 `xcodebuild` command succeeds, and `make -C driver/tests test` passes.

- [x] 5. **Stage B: minimal datapath, with RX PTP latch and TX timestamp (G3/G4 code path).** Implement `kInitDatapath`:
  - **Step 1: refuse unsafe states.** Return `kIOReturnUnsupported` and stay at stage 1 if `AvbInfo.flags` has APE (APE locking is out of Phase 0 scope) or if `fault` is set.
  - **Step 2: chip reset, following the `tg3_chip_reset` order for a 57765_PLUS PCIe part with CPMU.**
    1. Write `FIRMWARE_MBOX = 0x4B657654` (pre-reset signature).
    2. Save `PCI_COMMAND` and the cache line / `MISC_HOST_CTRL` config.
    3. Write `GRC_MISC_CFG = CORECLK_RESET | (1<<29)`.
    4. Wait 120 µs, read config `PCI_COMMAND` (flush), wait 120 µs.
    5. Clear the PCIe DEVCTL relaxed-ordering and no-snoop bits, and clear DEVSTA errors. Use the PCIe capability found with `FindPCICapability`.
    6. Restore config (`MISC_HOST_CTRL`, `COMMAND`).
    7. Write `MEMARB_MODE = ENABLE`.
    8. Poll `FIRMWARE_MBOX` for `~0x4B657654`, every 10 µs for up to 1 s. On timeout set `fault` and stop.
    9. Write `GRC_MODE = swap bits`, then `MAC_MODE = 0` and wait 40 µs.

    Skip, as tg3 does for this class: the 5701/5705/5750/5906/57780 quirks and the 0x7c00 bit 25 write (not for `57765_PLUS`). Skip the 57766 EEE firmware patch: tg3 loads it only when `NO_NVRAM`, and this adapter has NVRAM (it boots with a MAC). If `DATA_SIG` was not the magic, set `fault` instead.
  - **Step 3: `tg3_reset_hw` subset for 57765-class, in tg3 order.** Single vector, no TSO, no jumbo, no RSS/TSS, no ASF handshake beyond the mailbox signature.
    - CPMU/clock writes tg3 does for 57765-class.
    - `DMA_RW_CTRL` value for 57765_PLUS.
    - `GRC_MODE` = swap bits | HOST_SENDBDS | HOST_STACKUP | NO_TX_PHDR_CSUM | NO_RX_PHDR_CSUM | IRQ_ON_MAC_ATTN.
    - Immediately before that GRC_MODE write: `wr(RX_PTP_CTL, V2_L2_EN|0xF|HWTS_INTERLOCK)`, and OR `TIME_SYNC_ENABLE` into the GRC_MODE value.
    - Buffer manager pool/watermarks and `BUFMGR_MODE` enable, with the poll bounded at 10 ms.
    - FTQ reset pulse.
    - Std RX ring BDINFO (`RCVDBDI_STD_BD`: host addr, maxlen 1536 << 16, nic addr `NIC_SRAM_RX_BUFFER_DESC` 0x6000). Post 64 × 2 KiB buffers via the `RCV_STD_PROD_IDX` mailbox.
    - Disable all but send RCB 0 and RX return RCB 0. Program them in NIC SRAM at 0x100 / 0x200 with the host ring IOVA and size.
    - Status block host address, `HOSTCC` ticks (rx 25, tx 25, max frames 15/1, per the feasibility report), `HOSTCC_MODE` enable.
    - MAC address from the bootcode-loaded `MAC_ADDR_0_*`. Read it, do not write it.
    - Engine enables in tg3 order (WDMAC, RDMAC, RCVDCC, RCVDBDI, RCVLPC, RCVLSC, SNDDATAI, SNDDATAC, SNDBDS, SNDBDI, SNDBDC, MAC TX/RX modes with `RX_MODE_PROMISC`).
    - MI: auto-poll off. Read BMSR/BMCR via `MAC_MI_COM`. Write BMCR `ANENABLE|ANRESTART` only if autoneg is not already enabled. Poll link for up to 5 s. Set `MAC_MODE` port mode GMII + full duplex for 1000FD and MII for 100.
    - Do not reset the PHY.
  - **Step 4: re-arm the EAV clock.** `kSetClock(mach→ns now)`, because the core reset may clear the EAV block. Set `stage = 2`.
  - **Harvest** runs from the 2 ms timer and at the start of `kPollRx`. It reads the status block `rx_producer`. For each return BD it reads `type_flags`, and if PTPSTAT ∈ {V1,V2} it reads `RX_TSTAMP_LSB` then `MSB`. Hypothesis: reading MSB releases HWTS_INTERLOCK; the hardware test checks this. The frame is copied (≤128 B) into the software ring, the buffer is reposted, and the `RCVRET_CON_IDX_0` mailbox is updated.
  - **`kSendFrame`:**
    1. Copy the frame into the slot's 2 KiB bounce buffer and pad to 60 B.
    2. If `want_ts`, read `TX_TSTAMP` into `pre_tx_ts` and set `TXD_FLAG_HWTSTAMP`. Only one timestamped TX may be outstanding; a second one returns `kIOReturnBusy`.
    3. Write the BD, then the `SNDHOST_PROD_IDX_0` mailbox, then read it back (flush).
  - **TX timestamp.** The timer checks `tx_consumer` and then `TX_TSTAMP != pre_tx_ts`. It latches the value with `mach_latched`. The timeout is 300 ms, i.e. tg3's three retries × 100 ms.
  - **`kShutdownDatapath`:** clear the MAC TX/RX enables and the DMA engine enables in reverse order (tg3 `tg3_abort_hw` order), then `HOSTCC` off, then clear Bus Master, then complete DMA commands and free buffers. It also runs from `Stop`.
  - Read these tg3 functions for behaviour before coding: `tg3_chip_reset`, `tg3_reset_hw`, `tg3_abort_hw`, `tg3_poll_fw`, `tg3_rx`, `tg3_tx`, `tg3_start_xmit`, `tg3_read_tx_tstamp`, `tg3_ptp_ts_aux_work`, `tg3_setup_copper_phy`, `tg3_readphy`/`tg3_writephy`.
  - Files: `driver/AVBBcmDriver/{AVBBcmDriver.iig,AVBBcmDriver.cpp,AVBBcmUserClient.cpp,Bcm57762Regs.h,Tg3Logic.hpp}`, `driver/tests/tg3logic_tests.cpp`.
  - Verify:
    - The item 3 `xcodebuild` command succeeds with no new warnings.
    - `make -C driver/tests test` passes, including new tests for the BD encode/decode and ring-wrap helpers.

- [x] 6. **Probe library: pure logic and tests.**
  - `abi.rs`: `#[repr(C)]` mirrors of §3 plus selector consts. Tests assert sizes equal the C `static_assert` values.
  - `ptp.rs`:
    - Build an 802.1AS `Pdelay_Req`:
      - Ethernet header: dst 01:80:C2:00:00:0E, src = our MAC, ethertype 0x88F7.
      - PTP header (34 B): byte0 `0x12` (transportSpecific 1, msgType 2), versionPTP 2, messageLength 54, domain 0, flags `0x0008` (ptpTimescale), correction 0.
      - sourcePortIdentity = EUI-64 from MAC (insert FF:FE) + port 1, sequenceId, control 5, logMessageInterval 0.
      - Body: 10 B zero originTimestamp + 10 B reserved.
      - Total frame 68 B.
    - Parse `Pdelay_Req` (0x2), `Pdelay_Resp` (0x3; requestReceiptTimestamp + requestingPortIdentity) and `Pdelay_Resp_Follow_Up` (0xA; responseOriginTimestamp), plus `Sync`/`Follow_Up`/`Announce` type identification.
    - 48+32-bit PTP timestamp ↔ ns.
    - `link_delay_ns(t1,t2,t3,t4,corr_resp,corr_fu) = ((t4−t1) − (t3−t2) − corrections)/2`.
  - `stats.rs`: least-squares slope/intercept/residual-RMS of eav_ns vs mach-ns (convert with the timebase), monotonicity check, and the midpoint of `[mach_before, mach_after]`.
  - `freq.rs`: ppb → expected correction (mirrors the C++ helper), expected rate change, and measured-vs-expected scale factor.
  - Tests cover:
    - golden byte vectors for `Pdelay_Req`;
    - a parse round-trip;
    - a hand-built `Pdelay_Resp` + Follow_Up giving a known delay;
    - regression on synthetic data with known ppm;
    - correction encoding values identical to the C++ tests (shared table of expected values).
  - Files: `crates/avb-probe/src/{lib.rs,abi.rs,ptp.rs,stats.rs,freq.rs}`.
  - Verify: `cargo test -p avb-probe --locked` passes, and `cargo clippy --workspace --all-targets --locked -- -D warnings` is clean.

- [x] 7. **Probe CLI and IOKit wrapper.**
  - `iokit.rs`: `Device::open()` (service by name `AVBBcmDriver`; clear error if absent), typed wrappers for each selector via `IOConnectCallMethod`, and close on Drop.
  - Subcommands in `main.rs`, each printing human-readable output and, with `--json <path>`, appending results to `report.rs`'s JSON:

    | Subcommand | Behaviour |
    |---|---|
    | `info` | Print `AvbInfo` decoded. |
    | `regs` | Read-only dump of MISC_HOST_CTRL, GRC_MODE, EAV CTL/LSB/MSB, CORRECT_CTL, RX_PTP_CTL, MAC_MODE/STATUS. |
    | `clock --seconds S --interval-ms 100` | **G2a**: sample, regress, print ppm vs mach, residual, bracket-width stats, monotonic yes/no. |
    | `adjfreq --ppb P --seconds S` | **G2b**: four windows (baseline 0, +P, −P, 0). Print measured Δppb per window vs expected and the scale factor. Always restore 0 at exit, including on Ctrl-C. |
    | `datapath up\|down` | Init or shut down the datapath. |
    | `rx-ptp --seconds S` | **G3**: poll ethertype 0x88F7. For each frame print msgType, seqId, srcPortId, has_ts, hw_ts, harvest_mach, and the interval between successive Pdelay_Req hw_ts vs harvest_mach. |
    | `pdelay --count N` | **G4**: send `Pdelay_Req` with `want_ts`, get the TX ts (t1), collect the matching `Pdelay_Resp` (t2 from body, t4 = its RX hw_ts) and `Follow_Up` (t3), print the link delay per exchange plus the median. |
    | `all --json <path>` | info → clock 10 s → adjfreq ±100000 ppb 10 s/window → datapath up → rx-ptp 10 s → pdelay 5 → datapath down. |
  - Files: `crates/avb-probe/src/{iokit.rs,main.rs,report.rs}`.
  - Verify:
    - `cargo build --workspace --release --locked` succeeds.
    - `cargo run -p avb-probe -- --help` lists all subcommands.
    - `cargo run -p avb-probe -- info` without the dext exits non-zero with "AVBBcmDriver service not found" (no panic).
    - `cargo test --workspace --locked` passes.

- [x] 8. **Installer (Rust, objc2).** Implement the decision 7 behaviour, plus `status`, which prints `systemextensionsctl list` filtered to our bundle ID by running the process.
  - Map errors to `OSSystemExtensionError` codes with readable names.
  - Files: `crates/avb-installer/src/main.rs`.
  - Verify:
    - `cargo build -p avb-installer --release --locked` succeeds.
    - `cargo clippy --workspace --all-targets --locked -- -D warnings` is clean.
    - Running the bare binary outside a bundle prints a clear "must run from inside AVBInstaller.app" error and exits non-zero. Check `NSBundle.mainBundle.bundleIdentifier == com.avbaudio.installer`.

- [x] 9. **Packaging and signing script.** `scripts/package.sh [--adhoc | --identity "<name or SHA1>"]`. `set -euo pipefail`, and all paths are quoted. Steps:
  1. Build the dext (item 3 command) and `cargo build --release --locked`.
  2. Stamp `CFBundleVersion` (UTC `YYYYMMDDHHMM`) into the built dext and app plists.
  3. Assemble `build/AVBInstaller.app/Contents/{Info.plist,MacOS/avb-installer,MacOS/avb-probe,Library/SystemExtensions/com.avbaudio.installer.bcm57762.dext}`.
  4. Sign inside-out:
     - dext: `codesign --force --sign "$ID" --entitlements driver/AVBBcmDriver/AVBBcmDriver.entitlements --timestamp=none`
     - probe: `codesign --force --sign "$ID" --entitlements packaging/probe.entitlements -o runtime`
     - app: `codesign --force --sign "$ID" --entitlements packaging/installer.entitlements -o runtime`
  5. Run `codesign --verify --deep --strict` and print `codesign -d --entitlements :- <each>`.

  Default identity = the first "Apple Development" identity from `security find-identity -v -p codesigning`. Print its Team ID from the cert OU (`security find-certificate -c "Apple Development" -p | openssl x509 -noout -subject`).
  - Files: `scripts/package.sh`, `packaging/{Installer-Info.plist,installer.entitlements,probe.entitlements}`.
  - Verify:
    - `scripts/package.sh --adhoc` completes.
    - `codesign --verify --strict build/AVBInstaller.app` passes.
    - `plutil -lint` passes on all three plists.
    - `codesign -d --entitlements :- build/AVBInstaller.app/Contents/Library/SystemExtensions/com.avbaudio.installer.bcm57762.dext` shows the three dext entitlements.

- [x] 10. **Full loop gate.** Run everything in order:
  1. `cargo fmt --all --check`
  2. `cargo clippy --workspace --all-targets --locked -- -D warnings`
  3. `cargo test --workspace --locked`
  4. `make -C driver/tests test`
  5. the item 3 `xcodebuild` command
  6. `scripts/package.sh --adhoc`

  Record each command and its result in `.agents/tasks/phase0-build-evidence.md` for the reviewer. Make sure `git status` shows no build outputs (`build/`, `target/` are git-ignored), and remove `/tmp` scratch files.
  - Verify: all six commands exit 0.

## 6. Hardware validation gates (run in the hardware-test step only; pass criteria)

G2–G4 are hypotheses until these pass on the real 16R link. Report each with raw numbers in `.agents/tasks/phase0-hw-results.json` (`avb-probe all --json …`) and a short `.agents/tasks/phase0-hw-results.md`.

- **G1: match won.**
  - `ioreg -p IOService -l -w0 | grep -B2 -A20 AVBBcmDriver` shows our service under the 14e4:1682 `IOPCIDevice`.
  - `BCM5701Enet` is absent from that device.
  - `ifconfig en8` is gone.
  - `systemextensionsctl list` shows `[activated enabled]`.
  - en7 is unchanged: `ifconfig en7` still up, and the I210 is still owned by `DriverKit_AppleEthernetE1000`.
  - **If the dext does not win:**
    - Capture `ioreg` of the device (the `IOProbeScore`/`IOMatchedPersonality` of whoever owns it), `log show --last 10m --predicate 'process == "kernelmanagerd" OR process == "sysextd" OR eventMessage CONTAINS "AVBBcm"'`, and `systemextensionsctl list`.
    - Try one rematch by replugging, then a reboot.
    - If it still loses, write up why (score ignored for kext vs dext? `IONameMatch` precedence? dext never realized, as in the macOS 26.4 sysextd reports?).
    - Then evaluate the kext fallback in writing: Reduced Security + "Allow user management of kernel extensions" in 1TR, a C++ kext with the same personality and `IOProbeScore` 100000, `kmutil load`, approval, and reboot.
    - **Do not build-install or load a kext without explicit user confirmation.** Ask via `send_message` severity `warning`.
- **G2a: EAV clock ticks.**
  - `avb-probe clock --seconds 10`: the samples are strictly monotonic, and the slope vs `mach_absolute_time` is 1.0 ± 200 ppm (i.e. it counts nanoseconds).
  - If the slope is a clean multiple (e.g. 8 ns ticks), record it as a finding.
  - Bracket width (mach_after − mach_before) median is reported. A few µs is expected over Thunderbolt.
  - If the clock does not advance with `TIME_SYNC_ENABLE` set at Stage A, retry once after `datapath up` (post-reset) before declaring failure.
- **G2b: frequency correction.**
  - `avb-probe adjfreq --ppb 100000 --seconds 10`: measured Δrate for +P and −P has the right sign, with magnitude within ±10 % of 100 ppm (or a consistent scale factor, recorded). The baseline windows agree within ±5 ppm.
  - The probe always restores correction 0.
- **G3: RX PTP latch.**
  - `datapath up`, then `rx-ptp --seconds 10`: at least 8 `Pdelay_Req` frames from the 16R (≈1/s), each with `has_ts=1`.
  - Successive hw_ts intervals match successive harvest_mach intervals within ±1 ms (the latch tracks real arrival time, not stale data).
  - No stuck latch, i.e. the interlock releases after the read. If timestamps repeat, record that MSB-read does not release the interlock and stop. Do not guess other release writes.
- **G4: TX latch.**
  - `pdelay --count 5`: every sent `Pdelay_Req` gets status ok, with t1 strictly between the doorbell mach time and the next sample (converted with the G2 fit).
  - If the 16R answers with `Pdelay_Resp` + `Follow_Up`, the median link delay on the direct cable is > 0 and < 800 ns (expect a few hundred ns or less).
  - Negative or > 800 ns values are reported as a finding. They point to a timestamp-point/offset issue, which feeds the Phase 1 asCapable risk.
- **Fallback note.** If G3 or G4 fail, record the failure. The plan's fallback (software timestamps; likely asCapable failure; move to the I210) is a decision for the user, not for this phase.

## 7. User-action gates (batched for the hardware-test step, in this order)

None of these happen in the build loop. The hardware-test step should present U0–U4 to the user in one `send_message` warning so they can do them in one sitting.

- **U0. Before anything: confirm the Team ID and the plan to disable SIP.**
  - The probe/installer are signed with the existing "Apple Development" identity. The step prints the Team ID (cert OU) and the user confirms it.
  - If the user has a paid team with DriverKit entitlements, they may provide a provisioning profile instead. That would let AMFI stay enabled, though SIP still needs to be off for developer mode.
- **U1. One Recovery visit:** shut down, hold the power button, Options → Terminal, `csrutil disable`, reboot.
  - Optional in the same visit, only if the user agrees now to save a second trip: Startup Security Utility → Reduced Security + "Allow user management of kernel extensions". This is needed only for the G1 kext fallback.
  - Note it lowers security, and that it is reversible from Recovery.
- **U2. After reboot:** `sudo nvram boot-args="amfi_get_out_of_my_way=1"`, merged with any existing `nvram boot-args` value (read it first and preserve it), then reboot.
  - Needed because the restricted DriverKit/sysext entitlements are self-signed without a profile.
  - Risk: it disables AMFI policy system-wide. Reversible with `sudo nvram -d boot-args` (or restore the old value).
- **U3. `systemextensionsctl developer on`** (needs SIP off).
- **U4. Install:**
  1. The agent runs `scripts/package.sh`, then `build/AVBInstaller.app/Contents/MacOS/avb-installer activate`.
  2. The user approves in System Settings → General → Login Items & Extensions → Driver Extensions.
  3. **The user unplugs and replugs the Thunderbolt adapter** (or reboots) so the 14e4:1682 device rematches.
  4. Expect en8 to disappear and Apple's AVB stack to detach from it (accepted; the adapter is dedicated to the mixer). en7 must be unaffected.
- **U5. Mixer:** the 16R stays powered and cabled directly to the adapter during G3/G4. The agent confirms with the user that it is on before running `rx-ptp`.
- **U6 (only if G1 fails and the user confirms the kext fallback).** If it wasn't done in U1, a second Recovery visit for Reduced Security, then kext approval in System Settings and a reboot.
- **Rollback (document; run only on request):**
  - `avb-installer deactivate`, or `systemextensionsctl uninstall <TeamID> com.avbaudio.installer.bcm57762`
  - replug the adapter (BCM5701Enet and en8 return)
  - `systemextensionsctl developer off`
  - `sudo nvram -d boot-args` (or restore)
  - `csrutil enable` from Recovery

## 8. Notes for the reviewer (phase0-review.json)

- Check that only 14e4:1682 / 106b:00f6 can match: the personality, the entitlement, and the `Start` defence-in-depth check.
- Check that no `IOMatchCategory` is present and that there is no generic register-write path.
- Check that Stage A performs only the writes listed in item 4.
- Check that every poll is bounded and that the fault latch blocks further MMIO.
- Check that `Bcm57762Regs.h`/driver sources contain no copied tg3 code or comments.
- Check that every dependency is pinned exactly and `Cargo.lock` is committed.
- Check that `phase0-build-evidence.md` shows all item 10 commands passing.
- Do not mark G2–G4 behaviour as proven. That is the hardware-test step's job.

## Deviations

(Implementer: record any deviation from the decisions above here, with the concrete failure that forced it.)

Build evidence lives in `.agents/tasks/phase0-build-verification.md` (the step brief's name for the item 10 evidence file; it plays the role of `phase0-build-evidence.md`).

Toolchain and build:
- `rust-toolchain.toml` pins `channel = "1.99.0-aarch64-apple-darwin"`, not the bare `1.99.0`. The rustup proxy on this host runs under Rosetta (default host `x86_64-apple-darwin`), so a bare `1.99.0` resolves to an x86_64 toolchain. The probe and installer have to be native arm64.
- The xcodebuild command uses absolute `SYMROOT=/Users/dexter/AVB-Audio/build/driver` and adds `OBJROOT=.../build/driver/obj`, so all intermediates stay under the gitignored `build/`. `-derivedDataPath` cannot be combined with `-target`: xcodebuild exits 64 with "-scheme ... is required".
- `package.sh` prints entitlements with `codesign -d --entitlements -`. The `:-` form in item 9 is the deprecated spelling of the same output.
- The installer stays in Rust (objc2 0.6.5 / objc2-system-extensions 0.3.2). No Swift or ObjC host was needed.

Stage B register sequence: values follow tg3 behaviour where it differs from the §4/§5 summary.
- No FTQ reset pulse during init. tg3 pulses `FTQ_RESET` only in its abort path, never in reset_hw, so the pulse is in `abortHw` (shutdown) only.
- Datapath `GRC_MODE` does not set `NO_RX_PHDR_CSUM`. tg3 clears that bit and sets only `NO_TX_PHDR_CSUM`. The value is `0x041B0034`, pinned in the host tests.
- The `BUFMGR_MODE` enable poll is bounded at 20 ms (tg3: 2000 × 10 µs) instead of the 10 ms in item 5.
- Writes added because tg3 performs them for this class:
  - `GRC_FASTBOOT_PC = 0` before the core reset (5755_PLUS);
  - `GRC_MISC_CFG` prescaler;
  - jumbo BDINFO disabled, jumbo producer mailbox 0, jumbo/std replenish thresholds and LWMs;
  - `RDMA_RSRVCTRL |= FIFO_OFLW_FIX`;
  - statistics enables;
  - `MAC_LOW_WMARK_MAX_RX_FRAME = 1`;
  - `MAC_MI_STAT` link-attention enable;
  - receive rules 0/1 defaults and rules 4..7 cleared (skipped when ASF is on);
  - `SNDDATAI_MODE |= 0x8` (HW_TSO_3 part);
  - `RDMAC` LSO bits.
- tg3 writes, deliberately skipped:
  - `MAC_LED_CTRL` and `GRC_LOCAL_CTRL` (left at the bootcode values; GPIO state is board-specific);
  - the per-vector `HOSTCC_*_VEC1` zeroing (single vector);
  - MAC address registers (read only, per the plan);
  - NVRAM arbitration lock;
  - ASF pause/handshake;
  - the 57766 EEE firmware patch.
- `KEEP_VLAN_TAG` is set in `MAC_RX_MODE` when ASF is off, matching tg3's no-VLAN-acceleration path. That keeps the AVTP VLAN tag in the frame.

Dext behaviour:
- TX timestamp completion is decided by `TX_TSTAMP != pre_tx_ts` alone, as in tg3's aux worker. The status-block TX consumer is still tracked, but only for bounce-slot reuse; the register cannot change until the timestamped frame has gone out.
- Serialization: `ExternalMethod` validates counts with an `IOUserClientMethodDispatch` table, then runs the call on the driver's default queue with `IODispatchQueue::DispatchSync_f`. It does not re-target the user client's ExternalMethod queue. The effect is the same: user-client calls and the 2 ms timer are serialized without locks.
- All-ones detection: timestamp pairs check only the MSB, because an LSB of `0xFFFFFFFF` is a legal clock value. `kReadReg32` (the debug dump) never latches a fault.
- `kSendFrame` accepts 14..1514 bytes and zero-pads to 60, instead of rejecting frames under 60.
- The software RX ring overwrites the oldest entry when full and counts it in `dropped`. `kPollRx` consumes and discards entries that do not match the ethertype filter, so non-PTP traffic cannot fill the ring.
