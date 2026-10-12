#!/usr/bin/env bash
# Assemble the AVB-Audio AudioServerPlugIn into an AVBAudio.driver bundle.
#
# Produces build/AVBAudio.driver. Install it by replacing the bundle in
# /Library/Audio/Plug-Ins/HAL/ and reloading coreaudiod:
#
#   sudo rm -rf /Library/Audio/Plug-Ins/HAL/AVBAudio.driver
#   sudo cp -R build/AVBAudio.driver /Library/Audio/Plug-Ins/HAL/
#   sudo launchctl kickstart -k system/com.apple.audio.coreaudiod
#
# Reload with launchctl kickstart, NOT `killall -9 coreaudiod`: the hard kill
# strands every connected audio client (a DAW, system sound) silent and wedged
# until each app is itself restarted, even though coreaudiod respawns fine.
# Remove the old bundle before copying rather than copying over the top, so a
# file dropped from a later build cannot survive inside it.
#
# Then the device "AVB StudioLive 16R" appears in Audio MIDI Setup, with an
# input side fed by the daemon's recovered-audio ring and an output side that
# writes into the talker's ring.
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

# Sign AD-HOC, WITHOUT hardened runtime. This is the form the sandboxed HAL
# host (com.apple.audio.Core-Audio-Driver-Service.helper) actually dlopens for
# a locally-installed plug-in.
#
# Do NOT sign with an Apple Development cert + --options runtime: hardened
# runtime turns on library validation in the helper, and a Development cert
# cannot grant com.apple.security.cs.disable-library-validation without a
# provisioning profile. The helper then SIGKILLs the load with
# "EXC_BAD_ACCESS (Code Signature Invalid) / CODESIGNING Invalid Page" at
# dlopen — the device silently never appears. (Observed 2026-10-11: the
# Development+runtime form crashed the helper three times; adhoc loads cleanly.)
# Override with CODESIGN_IDENTITY only for a Developer-ID distribution build.
CODESIGN_IDENTITY="${CODESIGN_IDENTITY:--}"
echo "signing as: ${CODESIGN_IDENTITY} (adhoc = '-')"
if [ "$CODESIGN_IDENTITY" = "-" ]; then
  codesign --force --sign - "$BUNDLE"
else
  # A real distribution identity: Developer-ID style, hardened runtime is fine.
  codesign --force --options runtime --timestamp=none \
    --sign "$CODESIGN_IDENTITY" "$BUNDLE"
fi

echo "done: $BUNDLE"
echo
echo "install with:"
echo "  sudo rm -rf /Library/Audio/Plug-Ins/HAL/AVBAudio.driver"
echo "  sudo cp -R $BUNDLE /Library/Audio/Plug-Ins/HAL/"
echo "  sudo launchctl kickstart -k system/com.apple.audio.coreaudiod"
