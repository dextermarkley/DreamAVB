# Phase 0 environment

```
$ xcode-select -p
/Applications/Xcode.app/Contents/Developer
$ xcodebuild -version
Xcode 26.6
Build version 17F113
$ rustup --version
rustup 1.26.0 (5af9b9484 2023-04-05)
(active rustc 1.69.0 (84c898d65 2023-04-16))
$ cargo --version
cargo 1.69.0 (6e9a83356 2023-04-12)
$ security find-identity -v -p codesigning
1 valid identity: "Apple Development: <account email> (32P75VK9W8)"
```

Notes:
- Rust toolchain is old (1.69, April 2023); likely needs `rustup update` later (not done here).
- Only an "Apple Development" identity exists. The parenthesized ID is the certificate's user ID, not necessarily the Team ID; confirm the Team ID in Xcode/developer account. No Developer ID identity present.
- DriverKit entitlements needed (must be granted to the team by Apple for distribution; dev-signing with SIP off/`systemextensionsctl developer on` for local testing):
  - dext: `com.apple.developer.driverkit`, `com.apple.developer.driverkit.transport.pci`
  - host app: `com.apple.developer.system-extension.install`
