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
DEXT_ID="com.avbaudio.installer.bcm57762"
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

echo "== build dext"
/usr/bin/xcodebuild -project "$ROOT/driver/AVBBcmDriver.xcodeproj" -target AVBBcmDriver \
    -configuration Debug -sdk driverkit \
    SYMROOT="$BUILD/driver" OBJROOT="$BUILD/driver/obj" CODE_SIGNING_ALLOWED=NO build

echo "== build Rust (release)"
cargo build --manifest-path "$ROOT/Cargo.toml" --workspace --release --locked

VERSION="$(/bin/date -u +%Y%m%d%H%M)"
echo "== assemble $APP (CFBundleVersion $VERSION)"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Library/SystemExtensions"
cp "$ROOT/packaging/Installer-Info.plist" "$APP/Contents/Info.plist"
cp "$ROOT/target/release/avb-installer" "$APP/Contents/MacOS/avb-installer"
cp "$ROOT/target/release/avb-probe" "$APP/Contents/MacOS/avb-probe"
DEXT="$APP/Contents/Library/SystemExtensions/$DEXT_ID.dext"
cp -R "$DEXT_BUILT" "$DEXT"

# sysextd only replaces an installed extension when the version increases.
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $VERSION" "$APP/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $VERSION" "$DEXT/Info.plist"

for p in "$APP/Contents/Info.plist" "$DEXT/Info.plist" \
         "$ROOT/packaging/installer.entitlements" "$ROOT/packaging/probe.entitlements" \
         "$ROOT/driver/AVBBcmDriver/AVBBcmDriver.entitlements"; do
    /usr/bin/plutil -lint "$p"
done

echo "== sign (inside-out)"
/usr/bin/codesign --force --sign "$IDENTITY" --timestamp=none \
    --entitlements "$ROOT/driver/AVBBcmDriver/AVBBcmDriver.entitlements" "$DEXT"
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
echo "packaged: $APP"
