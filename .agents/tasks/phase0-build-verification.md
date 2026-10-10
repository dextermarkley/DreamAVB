# Phase 0 build verification (plan item 10 gate)

Host: macOS 26 (Apple Silicon), Xcode 26.6 (17F113), DriverKit25.5 SDK, Rust 1.99.0-aarch64-apple-darwin (pinned in `rust-toolchain.toml`).
Branch `phase0-bcm-dext`. All commands run from the workspace root with `TMPDIR=/Users/dexter/AVB-Audio/.agents/tmp/`. Full logs are in `.agents/tmp/logs/g1..g6.log` (gitignored).
Nothing was installed or activated. No sudo, csrutil, nvram, boot-args or systemextensionsctl was run.

| # | Command | Exit | Result |
|---|---|---|---|
| 1 | `cargo fmt --manifest-path Cargo.toml --all --check` | 0 | clean |
| 2 | `cargo clippy --manifest-path Cargo.toml --workspace --all-targets --locked -- -D warnings` | 0 | no warnings |
| 3 | `cargo test --manifest-path Cargo.toml --workspace --locked` | 0 | avb-probe lib: 18 passed, 0 failed (abi sizes + field offsets, freq golden table shared with C++, ptp golden bytes / parse / known link delay, stats fit, report) |
| 4 | `make -C driver/tests test` | 0 | `all tests passed (96 checks)` (EAV correction golden table, TX/RX BD, ring wrap, MDIO, identity incl. I210 rejection, status-block offsets, Stage B composite register values) |
| 5 | `xcodebuild -project driver/AVBBcmDriver.xcodeproj -target AVBBcmDriver -configuration Debug -sdk driverkit SYMROOT=$PWD/build/driver OBJROOT=$PWD/build/driver/obj CODE_SIGNING_ALLOWED=NO build` (from a deleted `build/`) | 0 | `** BUILD SUCCEEDED **`, 0 compiler warnings; product `build/driver/Debug-driverkit/com.avbaudio.installer.bcm57762.dext` (Mach-O arm64) |
| 6 | `scripts/package.sh --adhoc` | 0 | `build/AVBInstaller.app` assembled and ad-hoc signed; `codesign --verify --deep --strict` valid; 0 warnings |

Extra checks (plan items 3, 7, 8, 9):

- `cargo build --manifest-path Cargo.toml --workspace --release --locked`: exit 0.
- `target/release/avb-probe --help` lists info, regs, clock, adjfreq, datapath, rx-ptp, pdelay, all.
- `target/release/avb-probe info` with no dext loaded: exit 1, prints `AVBBcmDriver service not found (...)`, no panic.
- `target/release/avb-installer activate` outside the bundle: exit 1, prints `must run from inside AVBInstaller.app (main bundle identifier is None, expected com.avbaudio.installer ...)`.
- `cargo tree -p avb-probe --depth 1`: anyhow 1.0.104, clap 4.6.7, core-foundation-sys 0.8.7, io-kit-sys 0.5.0, mach2 0.7.0, serde 1.0.229, serde_json 1.0.151.
- `cargo tree -p avb-installer --depth 1`: anyhow 1.0.104, block2 0.6.2, dispatch2 0.3.1, objc2 0.6.5, objc2-foundation 0.3.2, objc2-system-extensions 0.3.2. Every manifest entry is an exact `=x.y.z` pin; `Cargo.lock` is committed.
- `plutil -lint`: OK for the built dext Info.plist, the packaged app Info.plist, `packaging/Installer-Info.plist` and all three entitlement files.
- `plutil -p` on the packaged dext Info.plist, which has a single personality `AVBBcmDriver`:
  - `IOProviderClass` = IOPCIDevice
  - `IOPCIPrimaryMatch` = "0x168214e4"
  - `IOPCISecondaryMatch` = "0x00f6106b"
  - `IOPCITunnelCompatible` = true
  - `IOProbeScore` = 100000
  - `IOClass` = IOUserService, `IOUserClass` = AVBBcmDriver, `IOUserServerName` = com.avbaudio.installer.bcm57762
  - no `IOMatchCategory`, `IONameMatch` or `IOPCIClassMatch`
  - `CFBundleVersion` stamped with UTC `YYYYMMDDHHMM`.
- `codesign -d --entitlements -` on the packaged items:
  - dext: `com.apple.developer.driverkit` = true, `com.apple.developer.driverkit.transport.pci` = [{IOPCIPrimaryMatch = "0x168214e4"}], `com.apple.developer.driverkit.allow-any-userclient-access` = true.
  - avb-probe: `com.apple.developer.driverkit.userclient-access` = ["com.avbaudio.installer.bcm57762"].
  - app: `com.apple.developer.system-extension.install` = true.
- `git status`: no build outputs tracked (`build/`, `target/`, `.agents/tmp/` are ignored).

Not verified here (hardware-test step, plan §6/§7): matching against the real 14e4:1682 device, Stage A/B register behaviour, EAV tick / frequency correction (G2), RX PTP latch (G3), TX latch and link delay (G4). Ad-hoc signing proves the bundle layout and entitlements only; loading needs the U0–U4 user-action gates.

Note on the scratch-dir rule: `xcodebuild -derivedDataPath` is rejected with `-target` ("-scheme ... is required"), so build products and intermediates go to `SYMROOT`/`OBJROOT` under the gitignored `build/` instead.
