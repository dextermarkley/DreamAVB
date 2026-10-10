# Agent rules for AVB-Audio

Start with `HANDOFF.md`. It has the current state, the resume point and the gate commands.

## Shell and files (hard rules)

- Never use `/tmp`, `/private/tmp` or the system `$TMPDIR`. Put scratch files in `/Users/dexter/AVB-Audio/.agents/tmp/`, which is gitignored.
- Prefix cargo, xcodebuild, clang, codesign and mktemp with `TMPDIR=/Users/dexter/AVB-Audio/.agents/tmp/`.
- Pass xcodebuild `-derivedDataPath /Users/dexter/AVB-Audio/.agents/tmp/DerivedData`.
- Never `cd` or `pushd`. Use absolute paths, `cargo --manifest-path`, `git -C`, `make -C` and `xcodebuild -project`.
- The same rules are in `.kiro/steering/scratch-files.md`.

## Engineering rules

- Rust everywhere except the DriverKit dexts, which are C++.
- Pin dependency versions exactly (`=x.y.z`). Use `--locked`.
- Every change passes the gate in `HANDOFF.md`: fmt, clippy `-D warnings`, cargo test, C++ host tests, a dext build with 0 warnings, and `package.sh`.
- Hardware claims must come from real probe or daemon output, never from reading code.
- Implement from public specs and datasheets: IEEE 802.1AS, 802.1Q (MSRP/MVRP), 1722, 1722.1, Milan v1.2, and the Intel I210 datasheet 333016. GPL/LGPL code is for behavior reference only.
- The I210 dext writes only documented registers and matches only 8086:1533 / subsystem 1ab6:0214. Never touch the Broadcom adapter or any other device. Back off if the device faults.
- Never hardcode sample rates, channel counts or stream counts. Derive them from the 16R's AEM.

## Process

- Work on branch `phase1a-listener`. Commit at checkpoints. Never push.
- Keep the FEAT JSON status under `.agents/tasks/task-phase1a-listener/` up to date.
- Reviews happen after implementation, and the reviewer has the final verdict.
- Ask the user before anything that changes boot security, NVRAM, SIP, or kext/dext approval, and before any physical action (replugging, cabling, routing signal on the 16R). Batch those requests into one visit.
