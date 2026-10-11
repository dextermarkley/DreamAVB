#!/usr/bin/env bash
# Assemble the AVB-Audio AudioServerPlugIn into an AVBAudio.driver bundle.
#
# Produces build/AVBAudio.driver. Install it by copying to
# /Library/Audio/Plug-Ins/HAL/ and restarting coreaudiod:
#
#   sudo cp -R build/AVBAudio.driver /Library/Audio/Plug-Ins/HAL/
#   sudo killall coreaudiod        # it respawns and loads the plug-in
#
# Then the device "AVB StudioLive 16R (Input)" appears in Audio MIDI Setup and
# in Reaper's input-device list. (STATUS: scaffold — the device publishes but
# the IO path is silence until the recovered-audio ring is wired, Phase 1b-1.)
#
# Scratch/build rules per AGENTS.md: absolute paths, no cd, TMPDIR under
# .agents/tmp.
set -euo pipefail

ROOT=/Users/dexter/AVB-Audio
export TMPDIR="$ROOT/.agents/tmp/"
CARGO="$HOME/.cargo/bin/cargo"
BUNDLE="$ROOT/build/AVBAudio.driver"
DYLIB="$ROOT/target/release/libavb_coreaudio.dylib"

echo "building the cdylib..."
"$CARGO" build --release --locked --manifest-path "$ROOT/Cargo.toml" -p avb-coreaudio

echo "assembling $BUNDLE ..."
rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS"
cp "$ROOT/crates/avb-coreaudio/Info.plist" "$BUNDLE/Contents/Info.plist"
# CFBundleExecutable is "AVBAudio".
cp "$DYLIB" "$BUNDLE/Contents/MacOS/AVBAudio"

echo "ad-hoc signing..."
codesign --force --sign - --timestamp=none "$BUNDLE"

echo "done: $BUNDLE"
echo
echo "install with:"
echo "  sudo cp -R $BUNDLE /Library/Audio/Plug-Ins/HAL/"
echo "  sudo killall coreaudiod"
