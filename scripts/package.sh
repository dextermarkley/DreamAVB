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
    /usr/bin/xcodebuild -project "$ROOT/driver/$proj.xcodeproj" -target "$proj" \
        -configuration Debug -sdk driverkit \
        SYMROOT="$BUILD/driver" OBJROOT="$BUILD/driver/obj" CODE_SIGNING_ALLOWED=NO build
done

echo "== build Rust (release)"
cargo build --manifest-path "$ROOT/Cargo.toml" --workspace --release --locked

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
         "$DEXT_ENT"; do
    /usr/bin/plutil -lint "$p"
done

echo "== sign (inside-out)"
/usr/bin/codesign --force --sign "$IDENTITY" --timestamp=none \
    --entitlements "$DEXT_ENT" "$DEXT"
/usr/bin/codesign --force --sign "$IDENTITY" --timestamp=none -o runtime \
    --entitlements "$ROOT/packaging/probe.entitlements" "$APP/Contents/MacOS/avb-probe"
/usr/bin/codesign --force --sign "$IDENTITY" --timestamp=none -o runtime \
    --entitlements "$ROOT/packaging/installer.entitlements" "$APP"

echo "== verify"
/usr/bin/codesign --verify --deep --strict --verbose=2 "$APP"
for item in "$DEXT" "$APP/Contents/MacOS/avb-probe" "$APP"; do
    echo "-- entitlements: $item"
    /usr/bin/codesign -d --entitlements - "$item"
done
echo "-- dext IOKitPersonalities"
/usr/bin/plutil -extract IOKitPersonalities xml1 -o - "$DEXT/Info.plist" | /usr/bin/plutil -p -
echo "-- embedded system extensions"
/bin/ls "$APP/Contents/Library/SystemExtensions"
echo "packaged: $APP"
