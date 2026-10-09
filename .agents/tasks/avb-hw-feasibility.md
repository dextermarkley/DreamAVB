# AVB-on-macOS (Rust, no Apple AVB stack) — Hardware Feasibility Report

Host: Mac16,8 (Apple Silicon, arm64), macOS 26.5.2 (25F84), SIP currently enabled.
Mixer: PreSonus StudioLive 16R, firmware 3.3.0.109659 (Milan firmware), direct cable, no switch (switch + more mixers later).

## Summary answer

Feasible. No hard blocker found. The host already has the best possible NIC for this attached: an Intel I210 (8086:1533) behind Thunderbolt. The I210 has a public datasheet, hardware IEEE 1588/802.1AS timestamping, an adjustable 64-bit hardware clock, two hardware credit-based-shaper (802.1Qav) queues, and per-packet LaunchTime. It is currently driven by Apple's DriverKit dext (`DriverKit_AppleEthernetE1000`, probe score 3000), which has no AVB functionality, so Apple's AVB stack is not attached to it. A third-party PCIDriverKit dext with a higher probe score and a specific `IOPCIPrimaryMatch` can take it over.

The Broadcom BCM57762 in the Apple Thunderbolt Gigabit adapter does have a hardware time-sync clock (Apple uses it: `BCM5701EnetClock`), but it has no public programmer's guide, Linux does not enable PTP on it, it has no known hardware shaper, and Apple's AVB stack is bound to it. Treat it as a second-tier target.

Technical recommendation was I210 first. The user has decided BCM57762 first (en8, already cabled to the 16R), record-only (Mac is a listener only), with a selectable sample rate. That is workable: listener-only removes the need for a TX shaper, which was the BCM's biggest gap. The new top risk is whether the 57762 hardware packet timestamps actually work (see "BCM57762 listener-only design" below). The Phase 1 scope at the end is re-scoped to these decisions.

Language note: DriverKit dexts must be C++ (no Rust toolchain support for DriverKit). Everything else (daemon, AudioServerPlugIn, macOS app, including the `OSSystemExtensionRequest` installer via objc2 bindings) can be Rust.

Confidence legend: High = confirmed from primary source or the host itself; Medium = strong indirect evidence; Low = inference, must be validated on hardware.

---

## Q1. Chip in the Apple Thunderbolt to Gigabit Ethernet Adapter — High

- Broadcom NetXtreme BCM57762, PCI 14e4:1682. Host ioreg (reported by parent session): subsystem 106b:00f6, bound to `BCM5701Enet`, interface en8, link 1000baseT to the mixer.
- Apple's on-disk kext `/System/Library/Extensions/IONetworkingFamily.kext/Contents/PlugIns/AppleBCM5701Ethernet.kext/Contents/Info.plist` (read on this host): `IOClass = BCM5701Enet`, `IONameMatch` includes `pci14e4,1682`, `IOPCITunnelCompatible = true`, arm64 personalities present, and `OSBundleLibraries` includes `com.apple.iokit.IOTimeSyncFamily`.
- External confirmation of the ID: [Matt Ray, Thunderbolt networking on Linux](https://mattray.github.io/2022/03/11/thunderbolt_networking_on_mac_linux.html), [Apple StackExchange 62283](https://apple.stackexchange.com/questions/62283/are-boot-camp-drivers-for-thunderbolt-to-gigabit-ethernet-available).

## Q2. BCM57762 hardware timestamping / clock — Medium (hardware exists), High (Linux doesn't enable it)

Linux tg3 ([tg3.c](https://raw.githubusercontent.com/torvalds/linux/master/drivers/net/ethernet/broadcom/tg3.c)), behavior only:
- `tg3_detect_asic_rev`: 57762 reads its ASIC rev from the "GEN15" product-ID config register and resolves to `ASIC_REV_57766` (57765_CLASS). `tg3_read_partno` confirms 57762 sits under `ASIC_REV_57766`.
- In probe, the `PTP_CAPABLE` flag is set only for `ASIC_REV_5719`, `ASIC_REV_5720`, `ASIC_REV_5762`. The 57762 is not included, so Linux exposes no PHC or HW timestamps for it.
- Mechanics where enabled (the "EAV" time-sync block): a 64-bit reference clock read as LSB/MSB registers (`TG3_EAV_REF_CLCK_LSB/MSB`); writes go through a stop/load/resume sequence on `TG3_EAV_REF_CLCK_CTL`; frequency adjust is a 24-bit accumulator with a signed correction value (`TG3_EAV_REF_CLK_CORRECT_CTL`); RX PTP filtering via `TG3_RX_PTP_CTL` (V1 L4, V2 L2, V2 L4 events); global enable bit `GRC_MODE_TIME_SYNC_ENABLE`; TX timestamps are latched and polled (retry loop in aux work); one periodic output.
- Apple evidence: the host shows `BCM5701EnetClock` (an IOTimeSync hardware clock) attached to the adapter, plus Apple's full AVB stack (IOAVB17221*, IOMSRP*, IOMVRP, IOAVBRealtimeTx/RxQueue). Users report enabling "AVB/EAV mode" on this exact TB2 dongle via the TB3→TB2 adapter ([Apple Community 255813117](https://discussions.apple.com/thread/255813117)). So the 57762 silicon has a usable time-sync block; Linux just never validated/enabled it on this rev.
- Documentation: no public 57762/57766 programmer's reference guide found. Broadcom has published a 5718-family programmer's guide in the past (covers 5717/5718/5719/5720), which may partially apply to the EAV block, but register offsets/behavior on 57766-class silicon are unverified. This means reverse-engineering from tg3 behavior and Apple's driver behavior.

## Q3. BCM57762 traffic shaping / TX queues — Medium

- No evidence of 802.1Qav credit-based shaping or rate-limited priority queues on 57765-class parts. tg3 uses multiple TX rings (TSS) only on 5719/5720; 57765-class runs a single TX ring.
- Pacing options from a driver without a HW shaper:
  1. Driver-timed submission: a high-priority timer/thread posts one class-A frame per 125 µs observation interval (8000 pkt/s per stream). DriverKit timer jitter will be tens of µs; acceptable for a direct link (Milan presentation-time offset is 2 ms by default) but bursts violate Qav on a switch.
  2. Batch with gap shaping: post small batches per interval and rely on line rate; tolerable direct-connect, risky with switches.
  3. Accept that AVB through switches with BCM is best-effort. For switched/multi-mixer setups, use the I210.

## Q4. Apple's historical AVB NICs and driver classes — High (on-disk), Medium (class hierarchy)

- Apple AVB (OS X 10.10+) runs on built-in Broadcom Ethernet (BCM5701Enet family), Apple's TB Gigabit adapter, Aquantia 10G on newer Macs, and qualified adapters like the Sonnet Thunderbolt AVB adapter. Third-party drivers cannot use Apple's direct AVB streaming ([MOTU technote](https://motu.com/techsupport/technotes/direct-avb-limitations)).
- On this host, `IONetworkingFamily.kext/Contents/PlugIns/` contains `IOEthernetAVBController.kext`, `AppleSkywalkAVB.kext`, `AppleBCM5701Ethernet.kext`, Aquantia, etc. `IOAVBFamily.kext` has Control, Streaming, Discovery, and MRP plugins. Executables live in the boot kernel collection and are stripped, so the C++ superclass of `BCM5701Enet` could not be confirmed from symbols. The live ioreg (AVB stack attached to en8) strongly implies it is an `IOEthernetAVBController` subclass.
- `IOEthernetAVBController` is not open source. The public [apple-oss-distributions/IONetworkingFamily](https://github.com/apple-oss-distributions/IONetworkingFamily) tree (tag IONetworkingFamily-188) only has `IOEthernetController`; the macOS 26 SDK Kernel.framework `IOKit/network` headers also lack it (checked on host).
- Practical consequence: we cannot build on Apple's AVB classes, and we must not leave Apple's AVB stack attached to the NIC we use. On the I210 this is a non-issue.

## Q5. PCIDriverKit / kext on Apple Silicon — High (APIs), Medium (override of Apple dext)

- Thunderbolt-tunneled devices: personalities need `IOPCITunnelCompatible = true`. Apple's own `com.apple.DriverKit-AppleEthernetE1000.dext` (on host) does this: `IOProviderClass = IOPCIDevice`, `IOPCIMatch = 0x00008086&0x0000ffff`, `IOPCIClassMatch = 0x02000000&0xffffff00`, `IOProbeScore = 3000`, `IOClass = IOUserNetworkEthernet`. So PCIDriverKit over Thunderbolt is proven on this OS.
- Winning the match: our dext should use `IOPCIPrimaryMatch = 0x15338086` (exact device), `IOPCITunnelCompatible = true`, and `IOProbeScore` well above 3000 (e.g. 10000). Matching picks the higher score. Apple's dexts live on the sealed system volume, so do not try to delete them; out-scoring is the supported route. After install, unplug/replug the adapter (or reboot) to rematch. Validate on hardware (Medium).
- For the BCM adapter: Apple's `BCM5701Enet` is a kext using `IONameMatch` with default probe score. A dext with higher score should win; if not, a kext with higher score is the fallback (requires Reduced Security). The AVB stack attaches only above Apple's driver, so it disappears once we own the device.
- PCIDriverKit capabilities, enough for a full NIC driver:
  - BAR MMIO: `IOPCIDevice::MemoryRead32/MemoryWrite32` (by BAR index), `_CopyDeviceMemoryWithIndex` for a mapped descriptor; config space via `ConfigurationRead/Write`; enable bus mastering/memory space in the command register.
  - DMA: `IOBufferMemoryDescriptor` + `IODMACommand::PrepareForDMA` gives device-visible (DART IOVA) segments for rings and packet buffers.
  - Interrupts: `IOInterruptDispatchSource` (MSI/MSI-X via `IOPCIDevice::ConfigureInterrupts`).
  - Shared memory with user space: `IOUserClient::CopyClientMemoryForType` returns an `IOMemoryDescriptor` that the client maps with `IOConnectMapMemory64`. Same buffer can be DMA-prepared, so the daemon can access rings and payload buffers with zero copy. Doorbell/notify via `ExternalMethod` and async callbacks.
  - Refs: [PCIDriverKit](https://developer.apple.com/documentation/pcidriverkit), [IOUserClient](https://developer.apple.com/documentation/driverkit/iouserclient), [Demystify code signing for DriverKit](https://developer.apple.com/news?id=c63qcok4).
- Loading requirements (macOS 26, Apple Silicon):
  - Dext, development: disable SIP (`csrutil disable` from 1TR Recovery) and run `systemextensionsctl developer on`. The dext must still be signed (free/Apple Development cert is fine) so entitlements are embedded (`com.apple.developer.driverkit`, `...driverkit.transport.pci` with the vendor/device match, `...driverkit.userclient-access` on the app). Some setups also need `amfi_get_out_of_my_way=1` when signing identity and profile don't match. Install through a host app calling `OSSystemExtensionRequest`.
  - Known friction: macOS 26.4 reports of sysextd silently failing to realize dexts even with SIP off + developer mode ([Apple forum 824746](https://developer.apple.com/forums/thread/824746)). Budget time for signing/loading.
  - Kext fallback: 1TR → Startup Security Utility → Reduced Security + "Allow user management of kernel extensions", then `kmutil load` / approval and reboot to rebuild the auxiliary KC ([Apple Platform Security](https://support.apple.com/en-ca/guide/security/sec8e454101b/web)). Unsigned kexts additionally need SIP's kext protection disabled (`csrutil enable --without kext` or full disable).

## Q6. PreSonus StudioLive (Series III, Milan firmware) — High for format/clocking, Medium for counts

From PreSonus KB ([Connect Series III v3.1+ to a Mac over AVB](https://support.presonus.com/hc/en-us/articles/38925260615437-How-to-connect-your-Series-III-console-running-firmware-v3-1-to-a-Mac-over-AVB), Aug 2025):
- Firmware 3.1+ is Avnu Milan; Apple's ATDECC warns the entity "implements Avnu Milan". PreSonus says macOS doesn't fully follow Milan and doesn't officially support Apple AVB; they recommend USB or the RME Digiface AVB.
- Stream format: AAF-PCM, 32-bit in 32-bit int, 8 channels, 6 samples/frame, 48 kHz (class A, 8000 pkt/s). Not IEC 61883-6 AM824. Apple's side uses AAF 24-in-32; the mixer uses full 32-bit.
- Stream counts are 8-channel streams. The 16R's exact talker/listener count must be read from its AVDECC entity/stream descriptors (Phase 1 will enumerate them).
- Clocking: mixer clock source can be "PTP Clock" (default), "Internal", or "Input Stream CRF". The Mac side can recover the media clock from Input Stream 1 or send a CRF stream (CRF 96 intervals, 1 per frame @ 48 kHz). The mixer works as media-clock master; gPTP grandmaster is picked by BMCA between the two endpoints. Our daemon must support being either gPTP GM or slave.
- Direct connection: supported (PreSonus's own Mac guide uses a direct cable). No switch needed for a two-endpoint setup. With a switch later, real Qav shaping matters, which favors the I210.
- Known macOS issues: Apple AVDECC controller broke in 15.0.1 ([Apple Community 255813117](https://discussions.apple.com/thread/255813117)); the mixer doesn't appear in Audio MIDI Setup's browser; restarts often needed. This is the brokenness the project works around.

## Q7. Reference projects and crates — High

- [Avnu/OpenAvnu](https://github.com/Avnu/OpenAvnu) (mostly BSD): gPTP daemon, MRPD (MSRP/MVRP/MMRP), AVTP pipeline, I210 `igb_avb` usage incl. LaunchTime ([PR 290](https://github.com/AVnu/Open-AVB/pull/290)).
- [Avnu/igb_avb](https://github.com/Avnu/igb_avb): I210 AVB driver (kernel part GPLv2; study behavior only, implement from Intel's datasheet).
- [Avnu/gptp](https://github.com/AVnu/gptp): 802.1AS reference.
- [Drumfix/avb4linux](https://github.com/Drumfix/avb4linux): ALSA driver, 8in/8out AVB stream on I210, closest in spirit to this project.
- [Avnu/libavtp](https://github.com/Avnu/libavtp), [COVESA/Open1722](https://github.com/COVESA/Open1722): AVTP/AAF packet formats.
- [L-Acoustics/avdecc](https://github.com/L-Acoustics/avdecc) (LGPL, Milan-aware) and the Hive controller: AVDECC reference and a test controller.
- [pendulum-project/statime](https://github.com/pendulum-project/statime): Rust PTP, no_std-capable, platform supplies clock/network. Useful for structure; 802.1AS profile coverage must be checked. No mature Rust crate for MSRP, AVTP, or AVDECC exists, so those are greenfield.
- Mac-specific: [mischa85/Ozzy](https://github.com/mischa85/Ozzy) (kext + HAL plugin + lock-free ring pattern on Apple Silicon). For the AudioServerPlugIn, Apple's NullAudio sample and gavv/libASPL are good patterns; in Rust, use `coreaudio-sys` bindings plus a small `extern "C"` factory.
- No open-source project found that does AVB on macOS without Apple's stack.

## Intel I210 (8086:1533, subsys 1ab6:0214, rev 03, en7) — High

- Public datasheet: [Intel Ethernet Controller I210 Datasheet (333016)](https://www.intel.com/content/www/us/en/content-details/333016/intel-ethernet-controller-i210-datasheet.html), plus the [spec update](https://www.mouser.com/catalog/additional/intel_i210_ethernetcontrollerspecupdate.pdf). Full register definitions are available, so no reverse-engineering is needed. The [product brief](https://cdrdv2-public.intel.com/327935/327935-Intel%20Ethernet%20Conroller-I210-product-brief.pdf) lists AVB and 802.1AS timestamping.
- PTP: 64-bit SYSTIM (ns), adjustable increment (TIMINCA) for frequency trim plus atomic time adjust; RX timestamping of PTP L2 frames (Ethertype 0x88F7) via TSYNCRXCTL, with an option to timestamp every RX packet into the buffer; TX timestamps via TSYNCTXCTL latched in TXSTMP registers. Programmable SDP pins (PPS out / external timestamp in) help validation.
- 802.1Qav: 4 TX queues; queues 0 and 1 can run hardware credit-based shapers (idle slope / high credit per queue, Qav mode in TQAVCTRL) for SR class A and B.
- LaunchTime: in Qav mode a per-packet launch time in the advanced context descriptor holds frames until a given SYSTIM, giving precise 125 µs pacing without software timers.
- Replacing Apple's driver: `DriverKit_AppleEthernetE1000` matches any Intel class-0x02 device at score 3000. Our dext with `IOPCIPrimaryMatch = 0x15338086`, `IOPCITunnelCompatible`, and a higher score should win after replug (Medium; verify on host).
- Cost: en7 (the user's main LAN) goes away while our driver owns the I210, unless the dext also publishes a NetworkingDriverKit interface (big extra scope). Move LAN to Wi-Fi or to the BCM adapter.
- The subsystem vendor 1ab6 likely identifies a Sonnet-type TB AVB adapter (unverified). PreSonus recommends the Sonnet Thunderbolt AVB adapter for Mac AVB.

## Sample rates: 16R, AVDECC/Milan, CoreAudio — Medium

What the 16R exposes:
- PreSonus states Series III interop is at 48 kHz, 8 channels per stream ([Series III and Third-Party AVB support](https://support.presonus.com/hc/en-us/articles/115002507603-Series-III-and-Third-Party-AVB-support)). The Mac guide uses only 48 kHz AAF formats.
- The Series III system runs at 44.1 or 48 kHz, and PreSonus's AVB-D16 bridge supports AVB at 44.1 and 48 kHz only ([Gearspace announcement](https://gearspace.com/threads/presonus-releases-avb-d16-avb-to-dante-bridge.1329015/)). Expect 44.1 and 48 kHz from the 16R; 88.2/96 kHz are not expected (Low confidence until checked).
- Milan's base audio format is AAF, PCM int 32-bit packets (24-bit audio), 1/2/4/6/8 channels, 48 kHz (6 samples/PDU) or 96 kHz (12 samples/PDU). Talkers and listeners only connect when formats match exactly ([Meyer Sound Milan guide](https://docs.meyersound.com/products/en/user-guide---managingavbnetworks.html)). 44.1 kHz is outside the Milan base set, so it may be offered only as a non-Milan format or not at all.
- Ground truth must come from the device's AEM: READ_DESCRIPTOR on AUDIO_UNIT (current rate and `sampling_rates` list) and on each STREAM_OUTPUT (current format and `formats` list). Phase 1 must dump these and drive the UI from them. Do not hardcode rates.

How the rate is changed over AVDECC (IEEE 1722.1 AEM):
- Device rate: `SET_SAMPLING_RATE` / `GET_SAMPLING_RATE` targeting the AUDIO_UNIT descriptor.
- Stream format: `SET_STREAM_FORMAT` / `GET_STREAM_FORMAT` on STREAM_OUTPUT (the 16R talker) and on our own STREAM_INPUT (Mac listener). The AAF format encodes the rate, so both sides must move together.
- Milan behavior (Medium): entities reject `SET_STREAM_FORMAT` on a stream that is currently streaming/bound, so the order is: unbind or stop, then set the audio unit rate, then set the stream formats on both ends, then rebind. Some entities update their stream formats on their own after a rate change. Subscribe with `REGISTER_UNSOLICITED_NOTIFICATION` so changes made on the mixer (Universal Control or front panel) reach the daemon.
- The 16R may refuse remote `SET_SAMPLING_RATE` (rate owned by the mixer's own settings) or may need a DSP restart. If it refuses, the app must treat the mixer rate as read-only and follow it. Verify on hardware.

CoreAudio AudioServerPlugIn rate handling (High, Apple's HAL model):
- Report `kAudioDevicePropertyAvailableNominalSampleRates` as exactly the rates the 16R's AEM offers, and `kAudioDevicePropertyNominalSampleRate` as the current network rate. Keep stream `kAudioStreamPropertyVirtualFormat`/`PhysicalFormat` in sync.
- Single source of truth is the daemon (the network rate). All three triggers go through it:
  1. DAW sets `NominalSampleRate` (Pro Tools/Reaper session rate): the plug-in's `SetPropertyData` forwards the request to the daemon and returns without changing state. The daemon runs the AVDECC sequence. On success it signals the plug-in, which calls `host->RequestDeviceConfigurationChange()`. The HAL then calls `PerformDeviceConfigurationChange`, where the plug-in swaps the rate and ring geometry and the HAL publishes the property change. On failure, nothing changes and the DAW sees the old rate.
  2. The macOS app changes the rate: same daemon path, same `RequestDeviceConfigurationChange` at the end.
  3. The mixer changes its rate on its own: the unsolicited notification reaches the daemon, which follows with the same path.
- Never change the rate inside `SetPropertyData` or on the IO thread. Stop IO cleanly across the change (the HAL stops IO around `PerformDeviceConfigurationChange`). Reset the zero-timestamp/clock model afterward, because `kAudioDevicePropertyZeroTimeStampPeriod` and `GetZeroTimeStamp` are derived from the recovered media clock.

## BCM57762 listener-only design (record-only milestone)

TX needs (no pacing required):
- gPTP: set the Mac as not grandmaster-capable (priority1 = 255) so the 16R is GM. The Mac then transmits only `Pdelay_Req` and, to answer the 16R's peer-delay, `Pdelay_Resp` + `Pdelay_Resp_Follow_Up`, each about once per second. No Sync, Follow_Up, or Announce in slave state. `Pdelay_Req` and `Pdelay_Resp` need TX hardware timestamps.
- MSRP: Domain declaration (class A, priority 3, default VID 2) and a Listener Ready declaration for the 16R's stream ID. MVRP: join the stream VLAN. AVDECC: ADP ENTITY_AVAILABLE, AEM responses for our listener entity, ACMP responses. All are low-rate, sent from the normal TX ring with no shaping.
- Milan connection: a controller (our daemon acting as controller, or Hive for testing) binds the Mac's STREAM_INPUT to the 16R's STREAM_OUTPUT. The Mac listener then declares Listener Ready on the talker's advertised stream ID (from `GET_STREAM_INFO` or the received Talker Advertise). Debug shortcut: declare Listener Ready directly for a known stream ID to confirm audio flows before ACMP is finished.

RX needs at 8000 pkt/s (class A, 48 kHz, 8 ch × 6 samples × 4 B ≈ 192 B payload, about 1.8 MB/s per stream):
- Accept frames: AVTP Ethertype 0x22F0, usually VLAN-tagged (PCP 3, VID 2), to a MAAP-range multicast destination 91:E0:F0:00:xx:xx; AVDECC multicast 91:E0:F0:01:00:00; gPTP and MSRP/MVRP link-local multicast in 01:80:C2:00:00:xx. Program the tg3-family multicast hash, or use promiscuous mode in the first prototype. Keep the VLAN tag in the frame or descriptor so stream and priority can be checked.
- Hardware RX timestamps are needed only for gPTP event messages (Sync, Pdelay_Req, Pdelay_Resp): enable the RX PTP filter for V2 L2 events. AVTP audio frames do not need RX timestamps. Media-clock recovery uses each AVTP PDU's `avtp_timestamp` (low 32 bits of gPTP ns presentation time) against the locally disciplined gPTP clock.
- tg3 latches one RX PTP timestamp at a time (the "HWTS interlock"), and TX timestamps are latched and polled. At gPTP's few packets per second this is fine, but the driver must read the latch before the next event message arrives.
- Interrupt load: do not take an interrupt per packet. Use RX coalescing (Apple's own config uses `RxCoalescingTicks` 25 / `RxMaxCoalescedFrames` 15) or let the daemon poll the shared RX return ring every ~0.5–1 ms. The Milan default presentation offset (2 ms) leaves room.
- Validation gate (biggest BCM risk): Linux never enables PTP on the 57766-class rev, so first prove on hardware that (a) `GRC_MODE_TIME_SYNC_ENABLE` plus the EAV reference clock ticks and accepts frequency correction, (b) RX PTP events latch timestamps, (c) TX timestamp latch works. If packet timestamps don't work, the fallback is software timestamps taken in the dext around ring events. Thunderbolt MMIO jitter (µs range) could then push the 16R's measured peer delay over the 802.1AS 800 ns `asCapable` threshold. If that happens the 16R treats the link as non-AVB and won't stream, so move to the I210.

## Conclusions

Blockers: none confirmed. One conditional blocker for the BCM-first plan: if 57762 packet timestamping doesn't work, the 16R will likely mark the link as not asCapable. The I210 is the proven escape hatch.

Major risks:
1. 57762 time-sync block undocumented and not enabled by Linux; must be proven in Phase 0.
2. Dext signing/loading on macOS 26 (sysextd quirks). Out-scoring Apple's `BCM5701Enet` kext with a dext is unproven on this host; kext fallback needs Reduced Security.
3. Milan interop: Milan AVDECC (entity model, ACMP bind, unsolicited notifications) and exact AAF format matching.
4. Media clock and CoreAudio timing: gPTP↔`mach_absolute_time` correlation over Thunderbolt MMIO (about 1–3 µs per read) needs filtering. The IO path must be lock-free.
5. The 16R may refuse remote sample-rate changes. The app must handle read-only rate.
6. While our driver owns en8, Apple's AVB stack and the macOS network interface on that adapter go away (fine, it's dedicated to the mixer).

Recommended Phase 1 scope (BCM57762 en8, direct link to 16R, record-only, selectable rate):
0. Phase 0 gate: stub PCIDriverKit dext (C++) matching `pci14e4,1682`, `IOPCITunnelCompatible`, high `IOProbeScore`; wins over `BCM5701Enet`; maps BAR, enables the time-sync block, logs the EAV clock ticking, proves RX/TX PTP timestamp latching. Kext fallback if the dext can't win.
1. Dext: reset/init, one RX ring + one TX ring, MSI with coalescing, multicast filter, PTP RX filter and TX timestamp, user client sharing rings and clock access via `CopyClientMemoryForType`. Rust host app installs it.
2. Rust daemon: gPTP slave-only (Pdelay both directions, Sync/Follow_Up servo of the EAV clock); MSRP Domain + Listener Ready; MVRP join; AVDECC ADP + AEM enumeration of the 16R (dump audio unit rates, stream formats, stream counts), listener entity with ACMP bind, unsolicited notifications.
3. AAF listener: one 8-ch stream from the 16R at 48 kHz (32-bit), media-clock recovery from `avtp_timestamp`, then more streams up to the 16R's talker count.
4. Rust AudioServerPlugIn: input-only device, channel count from enumerated streams, available rates from the 16R AEM, rate changes via the `RequestDeviceConfigurationChange` flow above. Validate in Reaper, then Pro Tools.
5. Rust macOS app: rate selector (rates from the AEM), status (gPTP lock, asCapable, stream state), talks to the daemon over a Unix socket/XPC.
6. Second rate (44.1 kHz) after 48 kHz is stable, only if the 16R advertises it.
7. Out of Phase 1: talker/playback, switches, multiple mixers, I210 path, NetworkingDriverKit LAN passthrough.
