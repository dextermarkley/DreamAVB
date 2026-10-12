#!/usr/bin/env bash
# Build a double-clickable macOS .pkg that installs DreamAVB from artifacts
# ALREADY present in build/. This script never runs cargo, xcodebuild or
# scripts/package.sh — it only stages and packages what is already built, so it
# does not contend with concurrent crate/app builds.
#
#   scripts/build-pkg.sh [--identity "<Developer ID Installer name or SHA-1>"]
#
# Default: UNSIGNED (local install). Pass --identity for a distribution build.
#
# PRODUCT RULE (do not change): installing must NOT activate the system
# extension and must NOT claim the network device. The dext stays dormant until
# the user turns it on in the companion. The postinstall therefore NEVER calls
# `avb-installer activate`; it only kickstarts coreaudiod so the freshly
# installed HAL plug-in is picked up, and fixes ownership on the installed
# bundles.
#
# Signatures are preserved: payload is staged with `ditto` (not `cp`) and
# packaged with `pkgbuild --root`. The HAL plug-in is adhoc-signed on purpose
# and stays byte-identical.
#
# Scratch/build rules per AGENTS.md: absolute paths, no cd, scratch under
# .agents/tmp.
set -euo pipefail

ROOT="/Users/dexter/AVB-Audio"
SCRATCH="$ROOT/.agents/tmp"
BUILD="$ROOT/build"
export TMPDIR="$SCRATCH/"
mkdir -p "$SCRATCH"

PKG_ID="com.avbaudio.dreamavb.pkg"

IDENTITY=""
while [ $# -gt 0 ]; do
    case "$1" in
        --identity)
            [ $# -ge 2 ] || { echo "--identity needs a value" >&2; exit 64; }
            IDENTITY="$2"; shift 2 ;;
        -h|--help)
            echo "usage: $0 [--identity \"<Developer ID Installer name or SHA-1>\"]"
            exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 64 ;;
    esac
done

# --- Inputs: all three must already exist in build/. Name the script that
# --- produces each missing one, so the fix is obvious.
COMPANION="$BUILD/DreamAVB.app"
INSTALLER="$BUILD/AVBInstaller.app"
DRIVER="$BUILD/AVBAudio.driver"

[ -d "$COMPANION" ] || { echo "missing $COMPANION — build it with scripts/build-companion.sh" >&2; exit 1; }
[ -d "$INSTALLER" ] || { echo "missing $INSTALLER — build it with scripts/package.sh" >&2; exit 1; }
[ -d "$DRIVER" ]    || { echo "missing $DRIVER — build it with scripts/build-coreaudio-plugin.sh" >&2; exit 1; }

# --- Version: match scripts/package.sh EXACTLY. kernelmanagerd wants a
# --- kext-style version (major <= 4 digits, minor and revision <= 2 digits),
# --- so a raw timestamp is rejected. Scheme:
# ---   <days since 2026-01-01>.<UTC hour>.<UTC minute>
EPOCH="$(/bin/date -u -j -f %Y-%m-%d 2026-01-01 +%s)"
NOW="$(/bin/date -u +%s)"
VERSION="$(( (NOW - EPOCH) / 86400 )).$(( 10#$(/bin/date -u -r "$NOW" +%H) )).$(( 10#$(/bin/date -u -r "$NOW" +%M) ))"
if ! printf '%s\n' "$VERSION" | /usr/bin/grep -Eq '^[0-9]{1,4}\.[0-9]{1,2}\.[0-9]{1,2}$'; then
    echo "computed version '$VERSION' is not a valid kext version" >&2
    exit 1
fi

PKG="$BUILD/DreamAVB-$VERSION.pkg"
echo "== building DreamAVB-$VERSION.pkg"

# --- Stage the payload tree under scratch. The staging root mirrors the
# --- filesystem destinations exactly, and pkgbuild --root installs it at /.
STAGE="$(/usr/bin/mktemp -d -p "$SCRATCH" build-pkg.XXXXXX)"
trap 'rm -rf "$STAGE"' EXIT
ROOTDIR="$STAGE/root"
SCRIPTSDIR="$STAGE/scripts"
mkdir -p "$ROOTDIR/Applications"
mkdir -p "$ROOTDIR/Library/Audio/Plug-Ins/HAL"
mkdir -p "$SCRIPTSDIR"

# `ditto` preserves resource forks, extended attributes AND code signatures
# byte-for-byte; `cp` would not. The adhoc-signed HAL plug-in must survive
# intact.
echo "   staging $COMPANION -> /Applications/DreamAVB.app"
/usr/bin/ditto "$COMPANION" "$ROOTDIR/Applications/DreamAVB.app"
echo "   staging $INSTALLER -> /Applications/AVBInstaller.app"
/usr/bin/ditto "$INSTALLER" "$ROOTDIR/Applications/AVBInstaller.app"
echo "   staging $DRIVER -> /Library/Audio/Plug-Ins/HAL/AVBAudio.driver"
/usr/bin/ditto "$DRIVER" "$ROOTDIR/Library/Audio/Plug-Ins/HAL/AVBAudio.driver"

# --- postinstall: the ONLY two things allowed. (a) make coreaudiod re-scan HAL
# --- plug-ins so the new AVBAudio.driver is picked up; (b) normalise ownership
# --- of the installed bundles to root:wheel. It must NEVER activate the dext or
# --- claim the device.
cat > "$SCRIPTSDIR/postinstall" <<'POSTINSTALL'
#!/bin/bash
# DreamAVB postinstall.
#
# PRODUCT RULE: installing must NOT activate the system extension and must NOT
# claim the network device. The driver does nothing until the user turns it on
# in the DreamAVB companion. So this script does ONLY two things:
#   (a) kickstart coreaudiod so the new HAL plug-in is picked up;
#   (b) fix ownership/permissions on the installed bundles.
# It never loads or activates the dext.
set -eu

# (b) ownership: installed bundles belong to root:wheel.
/usr/sbin/chown -R root:wheel "/Library/Audio/Plug-Ins/HAL/AVBAudio.driver" || true
/usr/sbin/chown -R root:wheel "/Applications/DreamAVB.app" || true
/usr/sbin/chown -R root:wheel "/Applications/AVBInstaller.app" || true

# (a) make coreaudiod re-scan HAL plug-ins. kickstart -k is the graceful
# reload; never `killall -9 coreaudiod`, which strands connected audio clients.
/bin/launchctl kickstart -k system/com.apple.audio.coreaudiod || true

exit 0
POSTINSTALL

echo "== build component package"

# --- Relocation OFF, explicitly.
#
# pkgbuild defaults a discovered app bundle to BundleIsRelocatable = true, which
# makes Installer search for an existing copy by bundle id and install OVER it
# wherever it happens to live. A stray DreamAVB.app in ~/Downloads would then
# silently win over /Applications, and the pkg would appear to install nothing.
# We install to fixed, known locations (the HAL plug-in path is not negotiable
# and the companion hardcodes /Applications paths), so pin every component to
# its staged path.
COMPONENTS="$STAGE/components.plist"
/usr/bin/pkgbuild --analyze --root "$ROOTDIR" "$COMPONENTS"
i=0
while /usr/libexec/PlistBuddy -c "Print :$i:RootRelativeBundlePath" "$COMPONENTS" >/dev/null 2>&1; do
    /usr/libexec/PlistBuddy -c "Set :$i:BundleIsRelocatable false" "$COMPONENTS" >/dev/null 2>&1 \
        || /usr/libexec/PlistBuddy -c "Add :$i:BundleIsRelocatable bool false" "$COMPONENTS" >/dev/null
    i=$((i + 1))
done
echo "   pinned $i component(s) to their install paths (relocation off)"
if [ "$i" -eq 0 ]; then
    echo "pkgbuild --analyze found no bundles to pin; refusing to ship a pkg that may relocate" >&2
    exit 1
fi

PKGBUILD_ARGS=(
    --root "$ROOTDIR"
    --component-plist "$COMPONENTS"
    --scripts "$SCRIPTSDIR"
    --identifier "$PKG_ID"
    --version "$VERSION"
    --install-location "/"
)
if [ -n "$IDENTITY" ]; then
    echo "   signing with: $IDENTITY"
    PKGBUILD_ARGS+=(--sign "$IDENTITY")
else
    echo "   unsigned (local install)"
fi

/usr/bin/pkgbuild "${PKGBUILD_ARGS[@]}" "$PKG"

SIZE="$(/usr/bin/du -h "$PKG" | /usr/bin/awk '{print $1}')"
echo
echo "built: $PKG"
echo "size:  $SIZE"
