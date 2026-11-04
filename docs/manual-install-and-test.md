# DreamAVB manual installation and test runbook

This runbook installs and tests a local DreamAVB build without an agent. It is for
the supported Intel I210 Thunderbolt adapter only (`8086:1533`, subsystem
`1ab6:0214`) and a PreSonus StudioLive 16R connected by AVB Ethernet.

## Scope and safety

- DreamAVB takes the supported I210 away from macOS's normal Ethernet driver
  while it is active. Do not use that adapter for an important network session
  during this test.
- The build and installation steps do **not** change SIP, AMFI, NVRAM, boot
  arguments, or DriverKit approval policy. Those developer-machine settings
  must already be configured by the machine owner.
- Never use `killall -9 coreaudiod`. Reload Core Audio with `launchctl
  kickstart` as shown below.
- Do not use `AVB_TXCAP` for audio testing. Its synchronous capture path can
  reduce the required 8,000 AAF packets/second/stream cadence.
- Only DreamAVB should own the I210 at a time. Stop an unmanaged
  `avb-daemon duplex` before starting audio from the companion app.

## 1. Build the local artifacts

Open Terminal and run the commands below. They use the repository's project
scratch directory rather than the system temporary directory.

```sh
ROOT=/Users/dexter/AVB-Audio
mkdir -p "$ROOT/.agents/tmp"
export TMPDIR="$ROOT/.agents/tmp"
export PATH="$HOME/.cargo/bin:$PATH"

# Builds and signs the DriverKit installer bundle. This does not activate it.
"$ROOT/scripts/package.sh"

# Builds the menu-bar companion as build/DreamAVB.app.
"$ROOT/scripts/build-companion.sh" --release

# Builds the Core Audio HAL plug-in bundle.
"$ROOT/scripts/build-coreaudio-plugin.sh"
```

Expected artifacts:

- `/Users/dexter/AVB-Audio/build/AVBInstaller.app`
- `/Users/dexter/AVB-Audio/build/DreamAVB.app`
- `/Users/dexter/AVB-Audio/build/AVBAudio.driver`

If the package step fails, do not activate a partially built bundle.

## 2. Install the local apps and Core Audio plug-in

Quit DreamAVB and stop its **Audio** toggle before replacing any installed
bundle. The following commands require an administrator password because they
write to `/Applications` and `/Library`.

```sh
ROOT=/Users/dexter/AVB-Audio

sudo rm -rf /Applications/AVBInstaller.app /Applications/DreamAVB.app
sudo ditto "$ROOT/build/AVBInstaller.app" /Applications/AVBInstaller.app
sudo ditto "$ROOT/build/DreamAVB.app" /Applications/DreamAVB.app

sudo rm -rf /Library/Audio/Plug-Ins/HAL/AVBAudio.driver
sudo ditto "$ROOT/build/AVBAudio.driver" \
  /Library/Audio/Plug-Ins/HAL/AVBAudio.driver
sudo launchctl kickstart -k system/com.apple.audio.coreaudiod
```

Wait a few seconds, then open **Audio MIDI Setup**. The device named
**DreamAVB** should appear when the HAL plug-in loaded. If it does
not appear, read the latest
`/Library/Logs/DiagnosticReports/com.apple.audio.Core-Audio-Driver-Service.helper-*.ips`
report before changing code or signing settings.

## 3. Start DreamAVB and activate the I210

Launch the installed companion:

```sh
open -a /Applications/DreamAVB.app
```

1. Open the menu-bar app and select **Intel I210 (Thunderbolt)**.
2. Turn on the **DreamAVB driver** toggle.
3. If macOS asks for Driver Extension approval, approve only
   `AVBI210Driver` in **System Settings > General > Login Items & Extensions >
   Driver Extensions**. Do not approve an unknown extension.
4. Wait for the app to show **Active**.

Use the installer status command to verify the kernel, rather than trusting a
version number in `systemextensionsctl` alone:

```sh
/Applications/AVBInstaller.app/Contents/MacOS/avb-installer status
```

Expected result: it names an attached `AVBI210Driver` code hash and says the
bundle's dext is **ALREADY RUNNING**. A read-only low-level check is also
available:

```sh
/usr/sbin/ioreg -rn AVBI210Driver -w0
```

The output must include both `AVBI210Driver` and
`com.avbaudio.installer.i210`. Do not use a class-filtered `ioreg -c` query;
it can hide a valid name match.

## 4. Start and test the managed audio daemon

In DreamAVB, turn on **Audio**. The companion launches exactly one managed
`avb-daemon duplex` process; do not start a second daemon manually.

Use these checks:

1. The companion should show **Audio running** and a daemon PID.
2. DreamAVB should show the I210 as active, not faulted.
3. In UC Surface, `AVB-Audio Mac` should appear as an online source and the
   `Mac→16R` source banks should be selectable. Do not change UC Control
   routing merely to repeat this check; the established Channel 1 settings are
   already correct.
4. In Audio MIDI Setup or a DAW, select **DreamAVB** as the desired
   input and output device. Play a moderate test signal, beginning with
   Channel 1 only if investigating the existing Channel 1 path.
5. Observe companion stats and the daemon log at
   `~/Library/Logs/DreamAVB-audio.log` for gPTP lock, received frames, and
   transport errors.

### Current audio-test boundary

The AVDECC, gPTP, MSRP, AAF connection, descriptor enumeration, and source
listing are verified on the 16R. The current unresolved investigation is the
16R's internal **Channel 1 Digital Return → AVB Send** signal path: a Channel
1 tone has been proven in the Mac output ring and outgoing AAF stream but has
not reproduced in the returned ring or UDP meters. Treat an absent returned
Channel 1 tone as known test evidence, not a reason to change UC Control,
revisit source listing, or alter AVDECC settings.

For a clean Channel 1 observation, keep the test to Channel 1, keep
`AVB_TXCAP` unset, and record these boundaries in order:

1. Mac output-ring sample,
2. outgoing stream-0 AAF sample and presentation offset,
3. 16R receive/connection state,
4. returned input-ring sample or UDP meter.

Investigate only the first boundary where the tone disappears.

## 5. Stop audio and hand the I210 back to macOS

1. Turn off the companion's **Audio** toggle and wait for **Audio stopped**.
   This lets the daemon send a clean AVDECC departure and releases its DriverKit
   user client.
2. Turn off the **DreamAVB driver** toggle.
3. Wait for **Hands off**. This state is reached only after DreamAVB observes
   that the I210 is no longer attached to `AVBI210Driver`.

A DriverKit dext can stay matched to an always-present PCI device even after
macOS accepts a deactivation request. In that case DreamAVB reports
**Releasing…** instead of falsely switching the control back to Active. Close
all audio clients, then use one of these machine-owner actions:

- unplug and replug the Thunderbolt I210 adapter; or
- reboot the Mac.

After the device is re-enumerated, verify that `ioreg -rn AVBI210Driver -w0`
returns no service and that macOS has restored the normal I210 interface.

## 6. Recovery and diagnostics

### New dext build does not take effect

The kernel can keep an older, matched dext running while
`systemextensionsctl` says the newer one is activated. Repackage the signed
bundle, then check the attached code hash with `avb-installer status`. If the
hash belongs to the old build, unplug/replug the Thunderbolt I210 adapter and
check again. Do not add a self-termination call to the dext; DriverKit does
not document that for a matched service.

### Two records with the same I210 bundle ID

An ad-hoc and Apple-Development-signed dext have different team identities and
can coexist. If a stale ad-hoc record is shadowing the signed bundle, the
machine owner can remove that exact stale record, then replug the adapter:

```sh
sudo systemextensionsctl uninstall - com.avbaudio.installer.i210
```

Use this command only after confirming the stale record's team ID is `-` and
that it is not the signed development build you intend to keep.

### Core Audio device disappeared or audio clients are wedged

Do not hard-kill `coreaudiod`. Reinstall the HAL bundle if needed, then run:

```sh
sudo launchctl kickstart -k system/com.apple.audio.coreaudiod
```

If the device still does not appear, inspect the latest Core Audio Driver
Service crash report named in step 2. A locally installed HAL bundle must be
ad-hoc signed **without** hardened runtime.

### Evidence to collect for a bug report

Save the exact command output and these files when available:

- `/Applications/AVBInstaller.app/Contents/MacOS/avb-installer status`
- `/usr/sbin/ioreg -rn AVBI210Driver -w0`
- `~/Library/Logs/DreamAVB-audio.log`
- the companion's displayed driver and audio states
- the Channel 1 boundary observations described above

Do not include private signing certificates, provisioning profiles, or other
credential files in a report.
