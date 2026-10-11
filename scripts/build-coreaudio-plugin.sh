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
echo "  sudo cp -R $BUNDLE /Library/Audio/Plug-Ins/HAL/"
echo "  sudo killall coreaudiod"
