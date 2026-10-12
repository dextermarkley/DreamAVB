#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Audit our CoreAudio constants against the real SDK headers.

Parses every FourCC constant out of abi.rs, finds the same-named constant in
the SDK headers, and reports agreement or mismatch. A guessed FourCC compiles
and tests green while silently disabling the feature (that is exactly how
kAudioServerPlugInIOOperationWriteMix shipped as 'wmix' instead of 'rite'), so
the only trustworthy check is against Apple's own text.
"""
import re
import sys
import pathlib

ABI = pathlib.Path("/Users/dexter/AVB-Audio/crates/avb-coreaudio/src/abi.rs")
SDK = pathlib.Path(
    "/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/"
    "Developer/SDKs/MacOSX.sdk/System/Library/Frameworks"
)

# Our side: `pub const kName: Ty = u32::from_be_bytes(*b"abcd");`
# also tolerate `as OSStatus` suffixes and i32 casts.
OURS = re.compile(
    r'pub\s+const\s+(k\w+)\s*:\s*\w+\s*=\s*u32::from_be_bytes\(\*b"(.{4})"\)'
)
# Apple's side: `kName = 'abcd'` or `#define kName 'abcd'`
THEIRS_ENUM = re.compile(r"\b(k\w+)\s*=\s*'(.{4})'")
THEIRS_DEF = re.compile(r"#define\s+(k\w+)\s+'(.{4})'")

ours = {m.group(1): m.group(2) for m in OURS.finditer(ABI.read_text())}

theirs: dict[str, str] = {}
headers = 0
for h in SDK.rglob("*.h"):
    if "CoreAudio" not in str(h) and "AudioToolbox" not in str(h):
        continue
    try:
        text = h.read_text(errors="ignore")
    except OSError:
        continue
    headers += 1
    for rx in (THEIRS_ENUM, THEIRS_DEF):
        for m in rx.finditer(text):
            theirs.setdefault(m.group(1), m.group(2))

print(f"parsed {len(ours)} FourCC constants from abi.rs")
print(f"indexed {len(theirs)} FourCC constants from {headers} SDK headers\n")

ok, bad, missing = [], [], []
for name, cc in sorted(ours.items()):
    if name not in theirs:
        missing.append((name, cc))
    elif theirs[name] == cc:
        ok.append((name, cc))
    else:
        bad.append((name, cc, theirs[name]))

if bad:
    print("=== MISMATCH (ours != SDK) ===")
    for name, cc, real in bad:
        print(f"  {name}\n    ours={cc!r}  SDK={real!r}")
    print()
if missing:
    print("=== NOT FOUND in the indexed headers (verify by hand) ===")
    for name, cc in missing:
        print(f"  {name} = {cc!r}")
    print()
print(f"=== AGREES WITH SDK: {len(ok)} ===")
for name, cc in ok:
    print(f"  {name} = {cc!r}")

sys.exit(1 if bad else 0)
