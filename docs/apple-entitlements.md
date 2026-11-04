# Requesting Apple entitlements for DreamAVB

How to get DreamAVB signed with real Apple entitlements, so it runs with SIP
and AMFI on and can eventually ship to other Macs.

There are two tracks:

- **Track A, development:** this Mac only. Needs a paid developer account but
  no approval from Apple.
- **Track B, distribution:** any Mac. Needs entitlements Apple grants case by
  case.

Do Track A first. It gets this Mac running with system security back on, and
nothing ships until Track B comes through. `scripts/package.sh` already embeds
a provisioning profile automatically once the file is in place (see step A6).

## What DreamAVB needs

| Component | Bundle ID | Entitlements |
|---|---|---|
| I210 driver (dext) | `com.avbaudio.installer.i210` | `com.apple.developer.driverkit`<br>`com.apple.developer.driverkit.transport.pci` (`IOPCIPrimaryMatch` = `0x15338086`)<br>`com.apple.developer.driverkit.allow-any-userclient-access` |
| Installer / host app | `com.avbaudio.installer` | `com.apple.developer.system-extension.install` |
| `avb-daemon`, `avb-probe` | (inside the app) | `com.apple.developer.driverkit.userclient-access` = `com.avbaudio.installer.i210` |
| Core Audio plug-in | `AVBAudio.driver` | none, but it needs a **Developer ID Application** signature |

The entitlement files are `driver/AVBI210Driver/AVBI210Driver.entitlements`,
`packaging/installer.entitlements`, `packaging/daemon.entitlements` and
`packaging/probe.entitlements`.

Details you'll need along the way:

- **Team ID:** `8NG8NMGYVF`
- **This Mac's provisioning UDID:** `00006040-001C30880E20801C` (from
  `system_profiler SPHardwareDataType`)
- **The adapter's PCI IDs:** vendor `0x8086`, device `0x1533`, subsystem
  `0x1ab6:0x0214`

Most of the steps below need the **Account Holder** role.

## Track A: development, this Mac only (no Apple approval)

An Apple DTS engineer has said on the forums that every DriverKit entitlement
has a development variant. These variants work only in development-signed
builds, but any paid account can use them without special approval
([forum thread 809202](https://developer.apple.com/forums/thread/809202)).

1. **Register the App IDs.** Go to
   [developer.apple.com/account](https://developer.apple.com/account), then
   Certificates, Identifiers & Profiles, then Identifiers, then +, then App
   IDs, then App.
   - Create `com.avbaudio.installer` and enable the **System Extension**
     capability.
   - Create `com.avbaudio.installer.i210` and enable the **DriverKit**
     capabilities it offers, including the PCI transport.
2. **Check that a PCI development capability is actually offered.** This is
   the open question from `HANDOFF.md`. One developer reports that only USB
   had a "development only" variant and PCI didn't
   ([forum thread 795835](https://developer.apple.com/forums/thread/795835)).
   If the App ID's capability list has no PCI transport entry, skip to
   Track B.
3. **Register this Mac.** Go to Devices, then +, and enter the UDID above.
4. **Create the driver's profile.** Go to Profiles, then +, then Development,
   then **DriverKit development**. Pick `com.avbaudio.installer.i210`, your
   Apple Development certificate and this Mac, then Generate and Download.
   Only App IDs that have DriverKit enabled are offered this profile type
   ([Apple's instructions](https://developer.apple.com/help/account/manage-provisioning-profiles/create-a-driverkit-development-provisioning-profile)).
5. **Create the app's profile.** Use a macOS App Development profile for
   `com.avbaudio.installer`, with the same certificate and this Mac.
6. **Put the profiles in the repo.** Save them as:
   - `packaging/profiles/i210-dext.provisionprofile`
   - `packaging/profiles/installer-app.provisionprofile`

   The directory is gitignored, because profiles are tied to one account and
   one Mac. Check that each profile grants the entitlements we claim:

   ```
   security cms -D -i packaging/profiles/i210-dext.provisionprofile
   ```

   Then repackage with `scripts/package.sh`. It copies each profile into its
   bundle as `embedded.provisionprofile` before signing.
7. **Install.** Install the app in `/Applications`, which macOS requires for
   signed system extensions, then activate the driver. Turning AMFI or SIP
   back on needs a reboot. Test that as one batch, because the current
   ad-hoc-signed plug-in may stop loading once AMFI is on (see `HANDOFF.md`).

## Track B: distribution (needs Apple's approval)

1. **Get a Developer ID Application certificate.** It's under Certificates,
   then +, then Developer ID Application. The dext, the app and the Core
   Audio plug-in all need it.
2. **Submit capability requests.** On each App ID, open the **Capability
   Requests** tab
   ([Apple's help page](https://developer.apple.com/help/account/capabilities/capability-requests/)).
   This replaces the older "Request an Entitlement / Contact us" form on the
   [System Extensions page](https://developer.apple.com/system-extensions/).
   For `com.avbaudio.installer.i210`, request:
   - **DriverKit**, the base entitlement;
   - **DriverKit PCI transport**, giving vendor ID `0x8086`;
   - **DriverKit allow-any-userclient-access**, or userclient-access scoped
     to our own driver.
3. **Write the justification.** Keep it factual. Roughly:
   - It's an AVB/Milan audio driver for an Intel I210 NIC (8086:1533) that
     macOS doesn't support for AVB.
   - The driver matches only subsystem 1ab6:0214.
   - It writes only registers documented in Intel datasheet 333016.
   - It's needed because macOS's own AVB support doesn't cover this adapter.
   - The user client is reached only by our own daemon, signed by the same
     team.
4. **Expect resistance on the vendor ID.** Apple grants PCI transport per
   vendor ID, and 0x8086 is Intel's, not ours. Apple has asked other
   applicants for written permission from the vendor ID's owner. Others report
   PCI requests pending for months
   ([forum thread 849407](https://developer.apple.com/forums/thread/849407)).
   Plan for a long wait and a possible refusal.
5. **Make new profiles after approval.** You can't add entitlements to an
   existing profile, so create fresh ones
   ([forum thread 818358](https://developer.apple.com/forums/thread/818358)).
   Then check that the profile's PCI match covers `0x15338086`. The value in
   our entitlements file has to fit within what the profile grants, or the
   driver fails its entitlement check
   ([forum thread 818837](https://developer.apple.com/forums/thread/818837)).
6. **Notarize.** Submit the app with `xcrun notarytool submit`, then run
   `xcrun stapler staple`. `package.sh` doesn't do this yet; add it once the
   Developer ID certificate exists.
