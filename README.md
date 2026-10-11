# DreamAVB

A native macOS AVB (Audio Video Bridging) audio bridge. A custom Intel I210
DriverKit driver plus a Rust stack (gPTP / 802.1AS, MSRP/MVRP, IEEE 1722 AAF,
1722.1 AVDECC, Milan) exposes a PreSonus StudioLive 16R to Core Audio DAWs
(Reaper, Pro Tools) as an input and output device. Apple's own AVB stack is
bypassed entirely.

The implementation is clean-room: it is written from the public IEEE 802.1AS,
802.1Q (MSRP/MVRP), 1722, 1722.1, Milan v1.2 specifications and the Intel I210
datasheet (333016). GPL/LGPL projects were consulted for behavior reference
only; no third-party code is copied.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option. This is the conventional dual license for the Rust ecosystem;
the Apache-2.0 option additionally carries an explicit patent grant, which
matters for an implementation of patent-adjacent networking standards.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
