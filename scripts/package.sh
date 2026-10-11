#!/usr/bin/env bash
# Build, assemble and sign build/AVBInstaller.app (plan item 9).
#
#   scripts/package.sh [--adhoc | --identity "<name or SHA-1>"]
#
# Default identity: the first "Apple Development" codesigning identity.
# This script only builds and signs. It never installs or activates anything,
# never runs sudo, and never touches csrutil / nvram / boot-args /
# systemextensionsctl.
set -euo pipefail

ROOT="$(/bin/realpath "$(/usr/bin/dirname "$0")/..")"
SCRATCH="$ROOT/.agents/tmp"
BUILD="$ROOT/build"
# Only the I210 dext is embedded and signed. The BCM dext is still built as a
# compile-regression check but never packaged.
DEXT_ID="com.avbaudio.installer.i210"
DEXT_ENT="$ROOT/driver/AVBI210Driver/AVBI210Driver.entitlements"
APP="$BUILD/AVBInstaller.app"
DEXT_BUILT="$BUILD/driver/Debug-driverkit/$DEXT_ID.dext"

mkdir -p "$SCRATCH"
export TMPDIR="$SCRATCH/"

IDENTITY=""
ADHOC=0
while [ $# -gt 0 ]; do
    case "$1" in
        --adhoc) ADHOC=1; shift ;;
        --identity)
            [ $# -ge 2 ] || { echo "--identity needs a value" >&2; exit 64; }
            IDENTITY="$2"; shift 2 ;;
        -h|--help)
            echo "usage: $0 [--adhoc | --identity \"<name or SHA-1>\"]"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 64 ;;
    esac
done

if [ "$ADHOC" -eq 1 ]; then
    IDENTITY="-"
    echo "signing: ad-hoc"
elif [ -z "$IDENTITY" ]; then
    IDENTITY="$(/usr/bin/security find-identity -v -p codesigning \
        | /usr/bin/awk -F '"' '/Apple Development/ { print $2; exit }')"
    if [ -z "$IDENTITY" ]; then
        echo "no \"Apple Development\" identity found; use --adhoc or --identity" >&2
        exit 1
    fi
fi
if [ "$IDENTITY" != "-" ]; then
    echo "signing identity: $IDENTITY"
    SUBJECT="$(/usr/bin/security find-certificate -c "$IDENTITY" -p 2>/dev/null \
        | /usr/bin/openssl x509 -noout -subject 2>/dev/null || true)"
    TEAM="$(printf '%s\n' "$SUBJECT" | /usr/bin/sed -n 's/.*OU *= *\([A-Z0-9]*\).*/\1/p')"
    echo "certificate subject: ${SUBJECT:-unknown}"
    echo "team ID (cert OU): ${TEAM:-unknown}"
fi

for proj in AVBI210Driver AVBBcmDriver; do
    echo "== build dext $proj"
    # -derivedDataPath keeps Xcode's scratch out of ~/Library (ENV-1); xcodebuild
    # rejects it together with -target, hence -scheme. SYMROOT/OBJROOT keep
    # the products in $BUILD/driver/Debug-driverkit (DEXT_BUILT).
    /usr/bin/xcodebuild -project "$ROOT/driver/$proj.xcodeproj" -scheme "$proj" \
        -configuration Debug -sdk driverkit -derivedDataPath "$SCRATCH/DerivedData" \
        SYMROOT="$BUILD/driver" OBJROOT="$BUILD/driver/obj" CODE_SIGNING_ALLOWED=NO build
done

echo "== build Rust (release)"
# Resolve the pinned toolchain explicitly instead of trusting the ambient PATH.
# rust-toolchain.toml pins a plain version (it must: a host-qualified channel is
# rejected by rustup and broke CI), but this machine's rustup default-host is
# x86_64, so a bare `cargo` here would silently build an x86_64 installer. The
# shipped probe and installer must be native arm64, so prefer the native
# toolchain's own bin directory and verify the result below.
HOST_TC="$(/usr/bin/uname -m)"
case "$HOST_TC" in
    arm64) RUST_TC="$HOME/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin" ;;
    *)     RUST_TC="" ;;
esac
if [ -n "$RUST_TC" ] && [ -x "$RUST_TC/cargo" ]; then
    # Prepend to PATH rather than calling cargo by absolute path: cargo resolves
    # rustc from PATH, so an absolute cargo with the bin dir absent fails with
    # "could not execute process `rustc -vV`".
    PATH="$RUST_TC:$PATH"
    export PATH
else
    echo "warning: pinned native toolchain not found, using 'cargo' from PATH" >&2
fi
cargo build --manifest-path "$ROOT/Cargo.toml" --workspace --release --locked

# The installer and probe are loaded by native arm64 system components; an
# x86_64 build would fail at load time with a confusing error, so refuse to
# package one.
for bin in avb-installer avb-probe avb-daemon; do
    if ! /usr/bin/file "$ROOT/target/release/$bin" | /usr/bin/grep -q 'arm64'; then
        echo "error: target/release/$bin is not arm64 (wrong toolchain host?)" >&2
        /usr/bin/file "$ROOT/target/release/$bin" >&2
        exit 1
    fi
done

# kernelmanagerd requires a kext-style version (major <= 4 digits, minor and
# revision <= 2 digits), so a raw YYYYMMDDHHMM stamp is rejected. Use
# <days since 2026-01-01>.<UTC hour>.<UTC minute>: valid, and increasing with
# every rebuild made at least a minute after the previous one.
EPOCH="$(/bin/date -u -j -f %Y-%m-%d 2026-01-01 +%s)"
NOW="$(/bin/date -u +%s)"
VERSION="$(( (NOW - EPOCH) / 86400 )).$(( 10#$(/bin/date -u -r "$NOW" +%H) )).$(( 10#$(/bin/date -u -r "$NOW" +%M) ))"
if ! printf '%s\n' "$VERSION" | /usr/bin/grep -Eq '^[0-9]{1,4}\.[0-9]{1,2}\.[0-9]{1,2}$'; then
    echo "computed version '$VERSION' is not a valid kext version" >&2
    exit 1
fi
echo "== assemble $APP (CFBundleVersion $VERSION)"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Library/SystemExtensions"
cp "$ROOT/packaging/Installer-Info.plist" "$APP/Contents/Info.plist"
cp "$ROOT/target/release/avb-installer" "$APP/Contents/MacOS/avb-installer"
cp "$ROOT/target/release/avb-probe" "$APP/Contents/MacOS/avb-probe"
cp "$ROOT/target/release/avb-daemon" "$APP/Contents/MacOS/avb-daemon"
DEXT="$APP/Contents/Library/SystemExtensions/$DEXT_ID.dext"
cp -R "$DEXT_BUILT" "$DEXT"

# DriverKit refuses to exec a non-arm64e platform binary (Phase 0 lesson).
DEXT_EXE="$DEXT/$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$DEXT/Info.plist")"
ARCHS="$(/usr/bin/lipo -archs "$DEXT_EXE")"
if [ "$ARCHS" != "arm64e" ]; then
    echo "dext executable archs are '$ARCHS', expected exactly arm64e" >&2
    exit 1
fi
echo "dext executable archs: $ARCHS"

# sysextd only replaces an installed extension when the version increases.
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $VERSION" "$APP/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $VERSION" "$DEXT/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$APP/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$DEXT/Info.plist"

for p in "$APP/Contents/Info.plist" "$DEXT/Info.plist" \
         "$ROOT/packaging/installer.entitlements" "$ROOT/packaging/probe.entitlements" \
         "$ROOT/packaging/daemon.entitlements" \
         "$DEXT_ENT"; do
    /usr/bin/plutil -lint "$p"
done

echo "== sign (inside-out)"
/usr/bin/codesign --force --sign "$IDENTITY" --timestamp=none \
    --entitlements "$DEXT_ENT" "$DEXT"
/usr/bin/codesign --force --sign "$IDENTITY" --timestamp=none -o runtime \
    --entitlements "$ROOT/packaging/probe.entitlements" "$APP/Contents/MacOS/avb-probe"
/usr/bin/codesign --force --sign "$IDENTITY" --timestamp=none -o runtime \
    --entitlements "$ROOT/packaging/daemon.entitlements" "$APP/Contents/MacOS/avb-daemon"
/usr/bin/codesign --force --sign "$IDENTITY" --timestamp=none -o runtime \
    --entitlements "$ROOT/packaging/installer.entitlements" "$APP"

echo "== verify"
/usr/bin/codesign --verify --deep --strict --verbose=2 "$APP"
for item in "$DEXT" "$APP/Contents/MacOS/avb-probe" "$APP/Contents/MacOS/avb-daemon" "$APP"; do
    echo "-- entitlements: $item"
    /usr/bin/codesign -d --entitlements - "$item"
done
echo "-- dext IOKitPersonalities"
/usr/bin/plutil -extract IOKitPersonalities xml1 -o - "$DEXT/Info.plist" | /usr/bin/plutil -p -
echo "-- embedded system extensions"
/bin/ls "$APP/Contents/Library/SystemExtensions"
echo "packaged: $APP"
