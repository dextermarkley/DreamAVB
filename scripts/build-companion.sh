#!/bin/bash
# Assemble the DreamAVB companion into a real .app bundle.
#
# WHY THIS EXISTS: the companion is a menu-bar app -- its window starts hidden
# and it sets ActivationPolicy::Accessory, so there is no Dock icon and the
# tray icon is the ONLY way in. Running the bare `cargo build` binary registers
# a tray with NO icon, because `app.default_window_icon()` reads the icon from
# an app bundle's resources; an iconless tray item is a blank gap in the menu
# bar, so the app looks like it failed to start when it is running fine.
#
# `cargo tauri build` would do this, but the Tauri CLI is not installed and
# pulling it in is a bigger change than assembling four files. This produces
# the same thing the CLI's "app" target would for local use: binary, Info.plist,
# icon, and an adhoc signature.
#
# Usage: scripts/build-companion.sh [--release|--debug]   (default: release)
set -euo pipefail

ROOT="/Users/dexter/AVB-Audio"
export TMPDIR="$ROOT/.agents/tmp/"
PROFILE="release"
[[ "${1:-}" == "--debug" ]] && PROFILE="debug"

MANIFEST="$ROOT/apps/dreamavb-companion/src-tauri/Cargo.toml"
APP="$ROOT/build/DreamAVB.app"
ICONS="$ROOT/apps/dreamavb-companion/src-tauri/icons"
CONF="$ROOT/apps/dreamavb-companion/src-tauri/tauri.conf.json"

# The toolchain: this app is NOT a workspace member, so the repo's
# rust-toolchain.toml does not apply to it, and the rustup proxy resolves to
# whatever `default_toolchain` is (x86_64 stable here, too old for this
# lockfile). Resolve the native toolchain's own bin directly, the same way
# package.sh does.
NATIVE_BIN="$HOME/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin"
if [[ -x "$NATIVE_BIN/cargo" ]]; then
    export PATH="$NATIVE_BIN:$PATH"
else
    export PATH="$HOME/.cargo/bin:$PATH"
fi

echo "== build ($PROFILE)"
BUILD_FLAGS=()
[[ "$PROFILE" == "release" ]] && BUILD_FLAGS+=(--release)
cargo build --manifest-path "$MANIFEST" "${BUILD_FLAGS[@]}"

BIN="$ROOT/apps/dreamavb-companion/src-tauri/target/$PROFILE/dreamavb-companion"
[[ -x "$BIN" ]] || { echo "no binary at $BIN" >&2; exit 1; }

# Version and identity come from tauri.conf.json, so the bundle cannot drift
# from the app's own configuration.
VERSION="$(/usr/bin/plutil -extract version raw -o - "$CONF")"
IDENT="$(/usr/bin/plutil -extract identifier raw -o - "$CONF")"
NAME="$(/usr/bin/plutil -extract productName raw -o - "$CONF")"

echo "== assemble $APP ($NAME $VERSION, $IDENT)"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/$NAME"
cp "$ICONS/icon.icns" "$APP/Contents/Resources/icon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>$NAME</string>
    <key>CFBundleDisplayName</key><string>$NAME</string>
    <key>CFBundleExecutable</key><string>$NAME</string>
    <key>CFBundleIdentifier</key><string>$IDENT</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleIconFile</key><string>icon</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <!-- Menu-bar app: no Dock icon, no app switcher entry. This mirrors
         ActivationPolicy::Accessory, which the app also sets at runtime. -->
    <key>LSUIElement</key><true/>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# Adhoc, like the HAL plug-in: enough for local use, and no identity needed.
# This app holds no entitlements -- it shells out to the entitlement-carrying
# avb-installer / avb-probe rather than talking to the dext itself.
/usr/bin/codesign --force --sign - "$APP" >/dev/null 2>&1
/usr/bin/codesign --verify --strict "$APP"

echo "== verify"
/usr/bin/file "$APP/Contents/MacOS/$NAME" | /usr/bin/sed 's/.*: /  arch: /'
echo "  icon: $(ls -1 "$APP/Contents/Resources" | tr '\n' ' ')"
echo
echo "bundled: $APP"
echo "run it with:  open $APP"
