# Handing the I210 back to macOS without a reboot

Status: **not planned**. Recorded for a future decision.

## The problem

Turning the DreamAVB driver off in the companion app does not give the I210
back to macOS. The app shows **Releasing…** and the user has to replug the
adapter, reboot, or toggle the extension in System Settings. The goal would
be to keep the driver extension installed and enabled while the app switches
the port between AVB use and ordinary macOS networking.

## Why it cannot work with the current dext

- IOKit lets one driver own a PCI device. While `AVBI210Driver` is matched to
  the I210, macOS's own Ethernet driver cannot attach to it.
- A matched dext cannot un-match itself. DriverKit documents
  `IOService::Terminate()` only for services the dext created with
  `Create()`, and there is no supported call that re-runs PCI matching.
- A deactivation request for an always-matched dext is deferred until the
  device goes away (replug) or the machine reboots. The companion app reports
  this honestly as **Releasing…** since `0a6134e`.

## Options

### 1. Dual-mode driver: our dext is also the Ethernet driver (recommended)

The dext registers the I210 as a normal macOS network interface (`enN`)
through NetworkingDriverKit (`IOUserNetworkEthernet`) and never releases the
device. The app's toggle becomes a mode switch inside the driver:

- **Network mode:** macOS uses the port for ordinary traffic.
- **AVB mode:** the daemon gets the AVB queues and rings.

The I210 has multiple hardware queues and a credit-based shaper (Qav) for
time-sensitive streams, so both could even run at once: best-effort traffic
on one queue and AVB class A on another. That is the I210's intended use.

Result: instant switching, no reboot, no System Settings, no replug.

Cost and risk:

- A full network data path in the dext: ordinary TX/RX descriptor handling,
  link state, offloads, and integration with the macOS network stack.
- The restricted entitlement `com.apple.developer.driverkit.family.networking`,
  which has to be requested from Apple.
- An Apple Developer Forums post reports a third-party DriverKit Ethernet
  driver running at line rate, so the approach is feasible, but the thread
  could not be read in full to confirm the details.
- Estimated as a multi-week project.

First steps if pursued:

1. Confirm the entitlement path with Apple.
2. Identify which driver macOS uses for this I210 today, so network mode can
   be compared against it.
3. Prototype network mode alone (RX, TX, link) before integrating AVB mode.

### 2. Keep the dext matched but release the hardware

In "off" mode the dext stops its datapath and resets the I210. This frees the
hardware electrically, but macOS still has no driver attached, so there is no
networking. It does not solve the problem.

### 3. Decline the device at start so macOS's driver takes over

If the dext's `Start` fails, IOKit should fall back to the next matching
driver. The dext could read a mode flag and decline the device when AVB is
off. Matching only happens when the device appears, though, so every switch
still needs a replug or reboot. It removes the trip to System Settings but
not the replug, and it is unverified that the fallback reaches macOS's driver
for this device.

## Constraints that apply to any option

From `AGENTS.md`: the I210 dext writes only documented registers (Intel I210
datasheet 333016), matches only `8086:1533` / subsystem `1ab6:0214`, never
touches the Broadcom adapter, and backs off if the device faults. Anything
that changes kext/dext approval needs the machine owner's sign-off first.
