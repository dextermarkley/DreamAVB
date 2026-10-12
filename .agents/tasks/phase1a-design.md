# Phase 1a Design: AVB listener bridge (StudioLive 16R → Mac over the Intel I210)

Branch `phase1a-listener` (from 4035832). Never push. Requirements: `.agents/tasks/phase1a-requirements.md` (IDs below refer to it). Revision 3: revision 2 addressed every pass-1 finding (1 HIGH, 9 MEDIUM, 9 NIT; §18). Revision 3 addressed every pass-2 finding (0 HIGH, 4 MEDIUM, 12 NIT; §19). Revision 4 addressed every pass-3 finding (0 HIGH, 3 MEDIUM, 5 NIT; §20). Revision 5 addresses every pass-4 finding of `.agents/tasks/phase1a-design-review.md` (0 HIGH, 2 MEDIUM, 6 NIT); responses are in §21.

## 0. Overview

Phase 1a extends the Phase 0b I210 dext with a shared-memory receive path (ABI v3) and adds a Rust user-space stack that turns the Mac into a Milan listener for every 16R audio (AAF) STREAM_OUTPUT (AS-9).

The dext stays small and policy-free:
- it harvests the hardware RX ring every 500 µs;
- it classifies each frame into one of six classes with a pure, host-tested function;
- it copies each accepted frame once into one of two single-producer/single-consumer slot rings in memory the daemon maps (`CopyClientMemoryForType`);
- it keeps the Phase 0b TX path (ExternalMethod, one timestamped frame outstanding), adds a TX rate bound, a bounded clock step, counters and a stream destination filter.

No new register is written in Phase 1a. The only new register accesses are reads of four clear-on-read statistics registers.

Everything protocol-related lives in Rust, in sans-IO library crates (`avb-gptp`, `avb-srp`, `avb-avtp`, `avb-avdecc`, `avb-mediaclock`, `avb-audioring`, plus `avb-core` and `avb-dext`). Every one of them is unit-testable without hardware. The `avb-daemon` binary wires them to the dext with three threads:
- control: gPTP, MRP, AVDECC, TX and the clock model;
- audio: AAF receive, media clock and ring writes; time-constraint scheduled;
- recorder: ring to WAV.

The 16R's AEM is read first (`aem-dump` is the first hardware milestone), and every rate, format, stream and channel count is derived from it at run time.

The technology stack is locked:
- C++ (DriverKit 25.5 SDK, arm64e) for the dext;
- Rust 1.99.0 (`rust-toolchain.toml`, edition 2021) for everything else;
- dependencies are the eight crates already in `Cargo.lock` (§6.3);
- no new third-party code.

## 1. Decisions at a glance

| # | Decision | Chosen | Rejected and why |
|---|---|---|---|
| D1 | RX delivery | Dext copies each frame once into fixed 2048 B slots of two SPSC rings (control, audio) in a read-only mapping; consumer indices in a separate writable mapping | Zero-copy mapping of the DMA buffers: the daemon would have to return descriptors through a call per batch, and a buggy or crashed client could hold DMA buffers. Variable-length byte ring: saves memory but adds PAD-record and wrap logic on both sides of the ABI for no requirement benefit |
| D2 | Harvest trigger | 500 µs `IOTimerDispatchSource` (leeway 50 µs), interrupts stay masked | MSI + EITR moderation: new interrupt registers (GPIE/IVAR/EIMS/EITR), more SAF-2 surface. Kept as the documented fallback if NFR-2 fails (§10.4, §16) |
| D3 | Control vs audio isolation (DP-7) | Separate shared rings; classification before copy; audio-ring full never touches the control ring | One ring with priority: a full ring would still block control |
| D4 | HW filtering | Keep Phase 0b RCTL (BAM, MPE, SECRC, DPF; UPE only with PROMISC). The classifier in the dext drops everything outside the DP-4 set | MTA hash and VLAN filter tables: new register writes, imprecise for dynamic MAAP addresses |
| D5 | VLAN tag | Leave CTRL.VME at its reset value 0, so the tag stays in the buffer. The slot also carries the descriptor VLAN field and a flag (U-12) | Setting VME: a new write, and the tag would move into the descriptor |
| D6 | Clock step | New `kAvbSelStepClock` (|Δ| ≤ 1 s) implemented as an in-dext read-add-write of SYSTIM. Large initial offsets use the existing `kAvbSelSetClock` | TIMADJL/H atomic adjust: its format isn't verified against 333016 yet. Backlogged (§16) |
| D7 | Datapath ownership | Only `RingAttach` confers ownership; it is released only by the owner's `RingDetach` or the owner's user-client `Stop` (`ClientGone`), which quiesces everything on the driver queue. With no owner, Phase 0b semantics apply unchanged (a probe-started data path outlives the probe process) | Per-client datapaths: a single NIC queue makes it meaningless. Ownership on `InitDatapath`: breaks the multi-process `avb-probe datapath up` → `rx-ptp`/`pdelay` flow |
| D8 | Daemon structure | One process; sans-IO protocol crates; three threads; AVDECC controller and listener entity in the same process with software loopback | Separate controller process: needs IPC that is Phase 1b scope |
| D9 | Servo | PI on Sync offset, one update per Sync (period Ts from the received logMessageInterval, gains scaled by Ts), driving TIMINCA. Freq seeded from the rate ratio. Step only at acquisition or above 100 µs | linuxptp-style adaptive filters: unnecessary on a direct link with 2.5 ns delay jitter |
| D10 | Media clock | Second-order DLL on presentation times in the gPTP domain, composed with the gPTP→mach linear model | Direct fit of pt to mach: mixes TB MMIO bracket noise (10 µs brackets) into the audio clock |
| D11 | WAV > 4 GiB | RF64 (EBU Tech 3306) via a JUNK→ds64 placeholder | Refuse: 16R stream counts are unknown and 32 ch × 600 s is already 3.7 GB |
| D12 | Dependencies | No new third-party crates. WAV, pcap, JSON schema and signal handling are our own code over `libc`, `serde_json` | `hound`, `signal-hook`, `crossbeam`: not needed, and each adds review and pin burden |

## 2. Assumptions (explicit)

- AS-1: The 16R is the only other station, direct-cabled at 1000FD (A-1). The stack still ignores and counts frames from other entities.
- AS-2: All 16R STREAM_OUTPUTs belong to one CLOCK_DOMAIN, so they share one media clock (expected for a single mixer; confirm with U-5 from the AEM dump).
- AS-3: `IOUserClient::Stop` runs when the client process exits or is killed. This is DriverKit's documented behavior when the kernel tears down the connection; DP-11 [HW] proves it on the host.
- AS-4: The RX buffer pool is 16 chunks × 256 KiB (128 buffers of 2 KiB each), the allocation size Phase 0/0b proved single-segment (`dmaAlloc` requires exactly one segment). No larger, unproven chunk size is used, so there is no fallback path. A chunk that still returns more than one segment latches `kAvbFaultDma` (existing behavior). Sixteen `IOBufferMemoryDescriptor`s plus sixteen `IODMACommand`s is a small, fixed cost at `InitDatapath`.
- AS-5: With TSYNCRXCTL.TYPE = all packets (Phase 0b value), the I210 places an in-buffer timestamp on every received frame, including AVTP, not only 0x88F7. G3 proved it for 0x88F7 only. If AVTP frames arrive without TSIP, AAF arrival time falls back to harvest mach time converted through the clock model (`AVB_RXF_TS_HW` clear), at about 250 µs resolution. Late/early counts stay meaningful at that resolution, but results must report the fallback.
- AS-6: With RCTL.LPE = 0, the I210 accepts VLAN-tagged frames up to 1522 B (333016 §8.10.1 RCTL.LPE description, *verify*). AVTP stream frames from the 16R are about 234 B, so this only matters for DP-3's limit statement.
- AS-7: Milan v1.2 (Avnu, public) is the reference for listener ACMP probing, MVU, GET_STREAM_INFO extensions and counters (A-4). Numbers below marked *(verify)* are from that spec or IEEE 1722.1-2021 and must be checked against the text by the implementer. Each one is pinned by a unit test with a citation comment.
- AS-8: Phase 1a needs the 16R to send gPTP Sync only after it sees our port as asCapable. Phase 0b never answered its Pdelay_Req, which likely explains why no Sync was recorded (U-11). The 16R's Sync interval is not assumed: the servo period follows the received logMessageInterval (§7.3).
- AS-9: "Every 16R STREAM_OUTPUT" (ADC-14, acceptance 8) means every **audio** STREAM_OUTPUT: those whose current format is AAF. A Milan talker may also expose a CRF media-clock output or other non-AAF formats; CRF reception is out of scope (requirements §7) and our listener model only has audio inputs. Non-AAF outputs are dumped, listed as `unbound_by_design` with their format in every summary and in the results doc, and never fail a run (§4, §9.2). This interpretation is confirmable only from the M1 AEM dump; if the 16R's audio is carried in a non-AAF format (for example IEC 61883-6), that is a deviation to raise with the user, not to adapt silently.

## 3. Dext: ABI v3 and data path

### 3.1 What changes in `driver/AVBShared.h`

- `AVB_ABI_VERSION` 2 → **3**.
- Selectors 0–9 keep their numbers and argument shapes. `AvbInfo`, `AvbClockSample`, `AvbRxEntry` and `AvbRxBatch` are unchanged (sizes 60/24/152/3656).
- New selectors 10–14, two memory types, five new structs, two new `AvbInfo.flags` bits, the counter index enum and the RX class enum. The comment "16..31 reserved for Phase 1 shared rings" is replaced, because Phase 1a numbers its selectors contiguously from 10. Selector numbers 32–47 are reserved for the future talker path (§12).
- `kAvbSelBaseCount = 10` is added. It marks the selectors every NIC dext implements. `AVBBcmUserClient.cpp` uses `kAvbSelCount` in three places, and **all three** switch to `kAvbSelBaseCount`:
  - line 54, the array size `kDispatch[kAvbSelBaseCount]`;
  - line 66, the static_assert, which becomes `static_assert(sizeof(kDispatch) / sizeof(kDispatch[0]) == kAvbSelBaseCount, "BCM dispatch covers the base selectors")`;
  - line 131, the guard `if (selector >= kAvbSelBaseCount) return kIOReturnUnsupported;` in `ExternalMethod`. Without this, selectors 10–14 would index past the end of the 10-entry array.

  That is the only BCM source change (DP-12). The BCM dext reports ABI 3 in `GetInfo` (it includes the same header) and returns `kIOReturnUnsupported` for selectors ≥ 10. The I210 user client gets the same `sizeof`-based assert against `kAvbSelCount` (15), and its guard stays `selector >= kAvbSelCount`.

```c
#define AVB_ABI_VERSION 3u

enum AvbSelector : uint32_t {
    /* 0..9 unchanged */
    kAvbSelStepClock       = 10, // in: delta_ns (i64)        out: before_ns, after_ns, mach_mid
    kAvbSelGetCounters     = 11, // in: -                     out: struct AvbCounters
    kAvbSelSetStreamFilter = 12, // in: struct AvbStreamFilter out: -
    kAvbSelRingAttach      = 13, // in: flags (must be 0)     out: generation, rx_mem_size, consumer_mem_size
    kAvbSelRingDetach      = 14, // in: -                     out: -
    kAvbSelCount           = 15,
    kAvbSelBaseCount       = 10, // implemented by every NIC dext (BCM: only these)
    // 32..47 reserved: talker TX ring / Qav (not built in Phase 1a)
};

// CopyClientMemoryForType types
enum AvbMemType : uint64_t {
    kAvbMemRx       = 1,  // AvbShmHeader + control slots + audio slots; client read-only
    kAvbMemConsumer = 2,  // AvbShmConsumer; client read-write
    // 3 reserved: talker audio TX ring
};

#define AVB_FLAG_RINGS_ATTACHED (1u << 5)
#define AVB_FLAG_OWNED          (1u << 6)   // some client owns the datapath

#define AVB_SHM_MAGIC           0x33425641u // "AVB3" in little-endian byte order
#define AVB_SHM_HEADER_SIZE     16384u      // one arm64 page; AvbShmHeader at offset 0
#define AVB_RX_SLOT_SIZE        2048u
#define AVB_RX_SLOT_HDR         64u
#define AVB_RX_SLOT_DATA_MAX    1984u       // >= 1522 (VLAN-tagged max frame)
#define AVB_RX_CTL_SLOTS        512u        // power of two
#define AVB_RX_AUDIO_SLOTS      8192u       // power of two
#define AVB_RX_CTL_OFFSET       AVB_SHM_HEADER_SIZE
#define AVB_RX_AUDIO_OFFSET     (AVB_RX_CTL_OFFSET + AVB_RX_CTL_SLOTS * AVB_RX_SLOT_SIZE)
#define AVB_RX_MEM_SIZE         (AVB_RX_AUDIO_OFFSET + AVB_RX_AUDIO_SLOTS * AVB_RX_SLOT_SIZE) // 17 842 176
#define AVB_CONSUMER_MEM_SIZE   16384u
#define AVB_MAX_STREAMS         16u         // S_max
#define AVB_STEP_CLOCK_MAX_NS   1000000000ll
#define AVB_TX_RATE_PER_SEC     20000u      // dext token bucket (SAF-6)
#define AVB_TX_BURST            64u

// AvbRxSlot.cls
enum AvbRxClass : uint8_t {
    kAvbRxNone = 0, kAvbRxGptp = 1, kAvbRxMsrp = 2, kAvbRxMvrp = 3,
    kAvbRxAvdecc = 4, kAvbRxMaap = 5, kAvbRxStream = 6,
};
#define AVB_RX_CLASS_COUNT 7u   // kAvbRxNone..kAvbRxStream

// AvbRxSlot.flags
#define AVB_RXF_TS_HW      (1u << 0)  // hw_ts_ns valid (SYSTIM ns, in-buffer timestamp)
#define AVB_RXF_VLAN_FRAME (1u << 1)  // set iff data[12..14] == 0x8100 (tag present in data[])
#define AVB_RXF_VLAN_DESC  (1u << 2)  // set iff descriptor VP set AND no in-buffer 0x8100 at offset 12;
                                      // vlan_tci then holds the tag. Never set together with VLAN_FRAME
#define AVB_RXF_TS_STEP_AMBIG (1u << 3) // harvested inside a SYSTIM step's settling window (§3.6):
                                      // hw_ts_ns may belong to either timebase; never use it for timing
#define AVB_STEP_SETTLE_NS 1000000u   // settling window after a SYSTIM write (§3.6)

struct AvbRxSlot {              // 2048 B
    uint64_t hw_ts_ns;          //  0 SYSTIM ns of reception (if AVB_RXF_TS_HW)
    uint64_t harvest_mach;      //  8 mach_absolute_time() at harvest
    uint64_t index;             // 16 ring record index (== producer head at write)
    uint32_t status_error;      // 24 advanced RX write-back status_error, verbatim
    uint16_t len;               // 28 bytes in data[] (no FCS, no TS header)
    uint16_t vlan_tci;          // 30 descriptor VLAN field (valid if AVB_RXF_VLAN_DESC)
    uint16_t flags;             // 32 AVB_RXF_*
    uint8_t  cls;               // 34 AvbRxClass
    uint8_t  pad0;              // 35
    uint32_t desc_len;          // 36 raw PKT_LEN (includes 16 B TS header if TSIP)
    uint32_t clock_gen;         // 40 dext clock generation (ivars->clockGen) at harvest
    uint8_t  pad1[20];          // 44
    uint8_t  data[AVB_RX_SLOT_DATA_MAX]; // 64
};

// Counter indices into AvbCounters.c[]. All monotonic for the life of the
// datapath (reset only by InitDatapath), except those marked GAUGE.
enum AvbCounterIndex : uint32_t {
    kAvbCtrRxFrames = 0,            // EOP descriptors harvested (counted after the EOP check)
    kAvbCtrRxClassBase = 1,         // delivered per AvbRxClass: index = cls (== kAvbCtrRxClassBase + cls - 1),
                                    // so Gptp -> 1 ... Stream -> 6; index 7 reserved; cls 0 (None) is never delivered
    kAvbCtrRxDropOther = 8,         // not in the DP-4 set (IP, mDNS, broadcast, ...)
    kAvbCtrRxDropUnboundStream = 9, // AVTP stream subtype to a MAC/VID not in the filter
    kAvbCtrRxDropUntaggedStream = 10,
    kAvbCtrRxErrors = 11,           // RXE set: discarded
    kAvbCtrRxNonEop = 12,           // multi-buffer frame: discarded
    kAvbCtrRingOverrunCtl = 13,
    kAvbCtrRingOverrunAudio = 14,
    kAvbCtrConsumerIndexInvalid = 15,
    kAvbCtrHwMpc = 16,              // accumulated MPC  (missed packets)
    kAvbCtrHwRnbc = 17,             // accumulated RNBC (receive no buffers)
    kAvbCtrHwRqdpc0 = 18,           // accumulated RQDPC(0) (queue drops)
    kAvbCtrHwCrcErrs = 19,          // accumulated CRCERRS
    kAvbCtrTxFrames = 20,
    kAvbCtrTxRateLimited = 21,
    kAvbCtrTxTsRequested = 22,
    kAvbCtrTxTsOk = 23,
    kAvbCtrTxTsTimeout = 24,
    kAvbCtrTxTsBusyRejected = 25,
    kAvbCtrTxStaleLatch = 26,
    kAvbCtrLinkUpEvents = 27,
    kAvbCtrLinkDownEvents = 28,
    kAvbCtrHarvestTicks = 29,
    kAvbCtrHarvestMaxBatch = 30,    // GAUGE (max descriptors in one tick)
    kAvbCtrHarvestLateTicks = 31,   // tick-to-tick gap > 2 ms
    kAvbCtrLegacySwOverrun = 32,    // Phase 0 software ring (PollRx mode)
    kAvbCtrRxOversize = 33,         // payload after TS header > AVB_RX_SLOT_DATA_MAX: discarded (expected 0; confirms AS-6)
    kAvbCtrCount = 64,
};
struct AvbCounters { uint64_t c[kAvbCtrCount]; };   // 512 B

struct AvbShmHeader {           // 1024 B at offset 0 of kAvbMemRx
    uint32_t magic;             //   0 AVB_SHM_MAGIC
    uint32_t abi_version;       //   4 AVB_ABI_VERSION
    uint32_t header_size;       //   8 AVB_SHM_HEADER_SIZE
    uint32_t slot_size;         //  12 AVB_RX_SLOT_SIZE
    uint32_t ctl_slots;         //  16
    uint32_t audio_slots;       //  20
    uint32_t ctl_offset;        //  24
    uint32_t audio_offset;      //  28
    uint32_t total_size;        //  32 AVB_RX_MEM_SIZE
    uint32_t generation;        //  36 RingAttach count since dext Start
    uint32_t stage;             //  40 live AvbStage
    uint32_t fault;             //  44 live AvbFault
    uint32_t flags;             //  48 live AVB_FLAG_*
    uint32_t producer_running;  //  52 1 while the harvester writes slots
    uint64_t last_tick_mach;    //  56 mach of the last harvest tick
    uint8_t  mac[6];            //  64
    uint8_t  pad0[58];          //  70
    uint64_t ctl_head;          // 128 records written (monotonic), release-stored
    uint8_t  pad1[120];         // 136
    uint64_t audio_head;        // 256
    uint8_t  pad2[120];         // 264
    struct AvbCounters counters;// 384
    uint32_t clock_gen;         // 896 incremented by every SYSTIM write (SetClock/StepClock); release-stored
    uint8_t  reserved[124];     // 900
};

struct AvbShmConsumer {         // 256 B at offset 0 of kAvbMemConsumer
    uint64_t ctl_tail;          //   0 records consumed (monotonic), release-stored by the daemon
    uint8_t  pad0[120];
    uint64_t audio_tail;        // 128
    uint8_t  pad1[120];
};

struct AvbStreamFilterEntry {   // 8 B
    uint8_t  dest_mac[6];       // must have the multicast bit set
    uint16_t vid;               // 1..4094; must match the frame's tag
};
struct AvbStreamFilter {        // 136 B
    uint32_t count;             // 0..AVB_MAX_STREAMS
    uint32_t pad;
    struct AvbStreamFilterEntry e[AVB_MAX_STREAMS];
};

#ifdef __cplusplus
static_assert(sizeof(AvbRxSlot) == AVB_RX_SLOT_SIZE, "AvbRxSlot ABI size");
static_assert(offsetof(AvbRxSlot, data) == AVB_RX_SLOT_HDR, "AvbRxSlot data offset");
static_assert(offsetof(AvbRxSlot, clock_gen) == 40 && offsetof(AvbRxSlot, pad1) == 44, "AvbRxSlot clock_gen");
static_assert(sizeof(AvbCounters) == 512, "AvbCounters ABI size");
static_assert(sizeof(AvbShmHeader) == 1024, "AvbShmHeader ABI size");
static_assert(offsetof(AvbShmHeader, ctl_head) == 128 && offsetof(AvbShmHeader, audio_head) == 256 &&
              offsetof(AvbShmHeader, counters) == 384, "AvbShmHeader offsets");
static_assert(offsetof(AvbShmHeader, clock_gen) == 896 && offsetof(AvbShmHeader, reserved) == 900,
              "AvbShmHeader clock_gen");
static_assert(sizeof(AvbShmConsumer) == 256 && offsetof(AvbShmConsumer, audio_tail) == 128, "consumer");
static_assert(sizeof(AvbStreamFilter) == 136, "AvbStreamFilter ABI size");
static_assert(sizeof(AvbStreamFilter) <= 4096 && sizeof(AvbCounters) <= 4096, "inline");
static_assert(AVB_RX_MEM_SIZE == 17842176u, "rx mem size");
static_assert((AVB_RX_CTL_SLOTS & (AVB_RX_CTL_SLOTS - 1)) == 0 &&
              (AVB_RX_AUDIO_SLOTS & (AVB_RX_AUDIO_SLOTS - 1)) == 0, "pow2");
static_assert(kAvbCtrRxOversize < kAvbCtrCount, "counter space");
static_assert(kAvbCtrRxClassBase + AVB_RX_CLASS_COUNT == kAvbCtrRxDropOther, "class counters 1..7");
static_assert(kAvbCtrRxClassBase + kAvbRxGptp - 1 == 1 && kAvbCtrRxClassBase + kAvbRxStream - 1 == 6,
              "class counter index = cls");
#endif
```

The header gains `#include <stddef.h>` next to the existing `#include <stdint.h>`, because the new `static_assert`s use `offsetof`; today the header compiles only because its includers happen to pull `<stddef.h>` in.

The 128 B separation of the hot indices matches the Apple Silicon cache-line size. The Rust mirror in `crates/avb-dext/src/abi.rs` repeats every size and offset above in `offset_of!` tests (DP-1), including `AvbRxSlot.clock_gen` = 40, `pad1` = 44, `AvbShmHeader.clock_gen` = 896, `reserved` = 900, the `AVB_RXF_TS_STEP_AMBIG` bit value 8 and `AVB_STEP_SETTLE_NS` = 1 000 000, plus `const` asserts that the per-class counter index of `Gptp` is 1 and of `Stream` is 6 (`fn class_counter_index(cls) -> usize { cls as usize }`, the only Rust helper that maps a class to a counter). `driver/tests/i210logic_tests.cpp` gains the same `offsetof` checks, so both sides assert the same literals.

### 3.2 Dispatch table (AVBI210UserClient.cpp)

| Sel | Name | scalarIn | structIn | scalarOut | structOut | Owner-only |
|---|---|---|---|---|---|---|
| 0 | GetInfo | 0 | 0 | 0 | var (60) | no |
| 1 | ReadClock | 1 | 0 | 0 | var | no |
| 2 | SetClock | 1 | 0 | 1 | 0 | yes* |
| 3 | AdjFreq | 1 | 0 | 1 | 0 | yes* |
| 4 | InitDatapath | 1 | 0 | 1 | 0 | yes* (never claims ownership) |
| 5 | PollRx | 2 | 0 | 0 | var | yes*; `kIOReturnNotReady` while rings are attached |
| 6 | SendFrame | 1 | var | 2 | 0 | yes* |
| 7 | GetTxTimestamp | 1 | 0 | 3 | 0 | yes* |
| 8 | ReadReg32 | 1 | 0 | 1 | 0 | no |
| 9 | ShutdownDatapath | 0 | 0 | 0 | 0 | yes* (never changes ownership) |
| 10 | StepClock | 1 | 0 | 3 | 0 | yes* |
| 11 | GetCounters | 0 | 0 | 0 | var (512) | no |
| 12 | SetStreamFilter | 0 | var (136) | 0 | 0 | yes (rings attached; strictly owner) |
| 13 | RingAttach | 1 | 0 | 3 | 0 | claims ownership |
| 14 | RingDetach | 0 | 0 | 0 | 0 | yes (strictly owner; releases ownership) |

\* "Owner-only" (selectors 2, 3, 4, 5, 6, 7, 9, 10): if an owner exists and the caller isn't the owner, the call returns `kIOReturnExclusiveAccess`. With no owner, any client may call them: these are exactly the Phase 0b semantics. Selectors 12 and 14 are "strictly owner": they need attached rings, so with no owner they return `kIOReturnNotReady`.

Ownership (DP-11, single data-path client). Only rings confer ownership:
- `owner := client` on a successful `RingAttach`. `InitDatapath` never claims ownership.
- `owner` is released only by `RingDetach` from the owner, or by the owner's `ClientGone` (§3.3).
- `ShutdownDatapath` never changes ownership. If rings are attached and the caller is the owner, it stops the harvester and leaves the rings attached and mapped (`producer_running = 0`); a later `InitDatapath` from the owner restarts ring-mode harvesting (`producer_running = 1`).
- With no owner, a data path started by one client survives that client's exit, exactly as in Phase 0b. That is what keeps `avb-probe datapath up` (one process) followed by `avb-probe rx-ptp` / `pdelay` (later processes) working.
- While a daemon owns the rings, the probe's data-path commands get `kIOReturnExclusiveAccess`; its read-only commands (`info`, `clock`, `regs`, counters) keep working.

`HandleUserMethod` gains a first parameter `AVBI210UserClient * client` (LOCALONLY). `Forward` passes `this`.

### 3.3 New and changed driver methods (`AVBI210Driver.iig`)

```cpp
kern_return_t HandleUserMethod(AVBI210UserClient * client, uint64_t selector,
                               IOUserClientMethodArguments * arguments) LOCALONLY;
kern_return_t CopyRingMemory(AVBI210UserClient * client, uint64_t type, uint64_t * options,
                             IOMemoryDescriptor ** memory) LOCALONLY;
void ClientGone(AVBI210UserClient * client) LOCALONLY;
```

`AVBI210UserClient` overrides `CopyClientMemoryForType` and forwards it to `CopyRingMemory` on the driver queue (same `DispatchSync_f` pattern as `Forward`). Its `Stop` calls `ClientGone(this)` on the driver queue before `super::Stop`.

`CopyRingMemory`:
- not attached → `kIOReturnNotReady`;
- caller ≠ owner → `kIOReturnExclusiveAccess`;
- `type == kAvbMemRx` → retains and returns `rxMem` with `*options = kIOUserClientMemoryReadOnly`;
- `type == kAvbMemConsumer` → `consumerMem` with `*options = 0`;
- other → `kIOReturnBadArgument`.

`ClientGone(client)`, if `client == owner`, runs in this order on the driver queue, so it is serialized with `TimerFired`:
1. `producerRunning = false` and `hdr->producer_running = 0`;
2. `shutdownDatapath(v)` (existing bounded quiesce, Bus Master off, DMA freed);
3. clear the stream filter, the TX timestamp state and the token bucket; `setFlags(v, 0, AVB_FLAG_RINGS_ATTACHED | AVB_FLAG_OWNED)` (before the release, so the header mirror is still valid);
4. `OSSafeReleaseNULL(rxMem)` and `OSSafeReleaseNULL(consumerMem)`. The kernel mapping holds its own reference, so a dead client's mapping is torn down independently;
5. `owner = nullptr`.

Non-owner clients (for example `avb-probe info` while the daemon runs, or `avb-probe datapath up` with no owner) only log `AVBI210: uc: closed (not owner)`. Their `ClientGone` changes nothing: a data path a probe started with no owner stays up after the probe exits, as in Phase 0b.

Client entitlement check (hardening, security_awareness): in `AVBI210UserClient::Start`, `CopyClientEntitlements` must return a dictionary whose `com.apple.developer.driverkit.userclient-access` array contains the string `com.avbaudio.installer.i210`. Otherwise `Start` returns `kIOReturnNotPermitted` and logs `AVBI210: uc: client lacks userclient-access entitlement`.

The dext keeps `allow-any-userclient-access`, which is what lets ad-hoc and development-signed clients open it. This check restores the scoping that entitlement removes. Both `avb-probe` and `avb-daemon` carry the entitlement (§6.5). If hardware shows `CopyClientEntitlements` failing for our correctly signed binaries, the hardware step records the verbatim result as a deviation and **stops to ask the user** before removing the check, because removing it is an access-control change. The check is never removed without that confirmation.

### 3.4 RX path: harvest, classify, copy

Constants in `AVBI210Driver.cpp`:
- `kRxRingSize` 128 → **2048** descriptors (RDLEN 32 KiB, a multiple of 128 B per 333016 §8.10.x);
- `kRxBufSize` 2048;
- RX buffers in `kRxBufChunks = 16` DMA buffers of `kRxBufChunkBytes = 256 KiB` (128 buffers each; the Phase 0b single-segment size, AS-4). Descriptor `i` points at chunk `i / 128`, offset `(i % 128) * 2048`. `IV::rxBufs` becomes `DmaBuf rxBufs[kRxBufChunks]`, and `dmaFreeAll` frees all of them;
- `kTimerPeriodNs` 2 ms → **500 µs**, leeway 50 µs;
- `kTxTsTimeoutNs` 300 ms → **20 ms**;
- the TX ring stays at 64 descriptors / 16 slots.

Capacity check (DP-6) at S_max = 16 streams × 8000 pkt/s = 128 000 pkt/s, plus control traffic (< 1000 pkt/s):

| Buffer | Size | Time covered at S_max | At 8 streams |
|---|---|---|---|
| HW descriptor ring | 2048 | 16 ms (32 dext ticks) | 32 ms |
| Audio shared ring | 8192 slots | **64 ms** (≥ 50 ms required) | 128 ms |
| Control shared ring | 512 slots | 0.5 s at 1000 control pkt/s | same |

What the headroom covers:
- A daemon stall shorter than 64 ms at S_max loses nothing.
- A dext stall shorter than 16 ms loses nothing.

If the 16R AEM reports more than 16 STREAM_OUTPUTs, the daemon refuses to bind indices ≥ 16 with a clear error (§9.2). Nothing is dropped silently.

Harvest (`TimerFired`, every 500 µs) in ring mode. The loop is bounded by `kRxRingSize` per tick.

```
tickStart = mach_absolute_time()
if rxMem: atomic_store_release(&hdr->last_tick_mach, tickStart)   // first, every tick
ambig = rxStepAmbiguous(&ivars->stepAmbig, tickStart)            // pure, I210Logic.hpp (§3.6); once per tick
tail_c = atomic_load_acquire(&consumer->ctl_tail); tail_a = ...audio_tail
validate: 0 <= ctl_head - tail_c <= ctl_slots  (same for audio);
          if violated: count kAvbCtrConsumerIndexInvalid, treat that ring as full this tick
for each descriptor with DD:
    status = status_error; len = PKT_LEN; buf = rxbuf(idx)
    switch rxHarvestStep(&ivars->rxDiscardToEop, eop(status)):   // pure, I210Logic.hpp
      case Discard: rearm; continue          // continuation of a multi-descriptor frame; not counted as RxFrames
      case NonEop:  count RxNonEop; rearm; continue   // first descriptor of a multi-descriptor frame; flag now set
      case Frame:   break                    // single-descriptor frame
    count RxFrames
    if RXE:  count RxErrors; rearm; continue
    if TSIP && len < kRxTsHeaderLen (16): count RxErrors; rearm; continue   // existing harvestRx guard, kept
    ts = TSIP ? parseRxTsHeader(buf) : none; frame = buf + (TSIP ? 16 : 0); flen = len - (TSIP ? 16 : 0)
    if flen > AVB_RX_SLOT_DATA_MAX (1984): count RxOversize; rearm; continue   // memory safety; never relies on AS-6
    verdict = classifyRx(frame, flen, ourMac, filter, vpBit(status), vlanField)   // pure, I210Logic.hpp
    if verdict is a drop: count the reason; rearm; continue
    ring = (verdict == Stream) ? audio : control
    if head - tail == slots: count RingOverrun{Ctl|Audio}; rearm; continue      // drop-newest
    slot = ring[head & (slots-1)]; fill header (index = head, clock_gen = ivars->clockGen,
           flags |= ambig ? AVB_RXF_TS_STEP_AMBIG : 0), memcpy(slot.data, frame, flen)
    head++ (local)
    rearm(idx)
after loop: atomic_store_release(&hdr->ctl_head, ...), (&hdr->audio_head, ...);
            write RDT once (existing pattern); update counters (relaxed 64-bit stores)
every 10 ticks (5 ms): read STATUS; link transitions -> setFlags() (mirrored into hdr->flags)
                       + LinkUp/LinkDown counters
every 200 ticks (100 ms): read MPC, RNBC, RQDPC(0), CRCERRS (clear-on-read) and accumulate
checkTx() (existing) every tick
```

`rxHarvestStep(bool *discarding, bool eop)` (finding 4 of pass 3): if `*discarding`, it returns `Discard` and clears `*discarding` when `eop`; otherwise, if `!eop`, it sets `*discarding` and returns `NonEop`; otherwise it returns `Frame`. `IV::rxDiscardToEop` (bool) is zeroed by `setupRx` and `shutdownDatapath` together with the ring indices. A multi-descriptor frame therefore costs one `RxNonEop` count and no `RxFrames` count, and its EOP tail is never classified as a frame. The same helper replaces the `!EOP` branch of the legacy `harvestRx` path, so both RX modes behave the same.

Atomic primitives (dext side, finding 8 of pass 3). The dext uses the compiler builtins it already uses (`__atomic_thread_fence`), not `<atomic>`:
- `atomic_store_release(p, v)` is `__atomic_store_n(p, v, __ATOMIC_RELEASE)`; used for `ctl_head`, `audio_head`, `last_tick_mach`, `stage`, `flags`, `fault`, `producer_running`, `clock_gen`. Slot fields (including `clock_gen` and `flags`) are plain stores published by the release store of the head.
- `atomic_load_acquire(p)` is `__atomic_load_n(p, __ATOMIC_ACQUIRE)`; used for `consumer->ctl_tail` and `consumer->audio_tail`.
- Counters are stored with `__atomic_store_n(&hdr->counters[i], v, __ATOMIC_RELAXED)` on naturally aligned `uint64_t`, which is a single tear-free 64-bit store on arm64.

Rust mirror (`avb-dext::ring`): each shared field is viewed as `&*(p as *const AtomicU64)` (or `AtomicU32` for the 32-bit fields) on the mapping; heads and the live header fields are read with `Ordering::Acquire`, tails are written with `Ordering::Release`, counters are read with `Ordering::Relaxed`. Alignment is guaranteed by the §3.1 offsets (all multiples of 8) and asserted in `abi.rs`.

Delivery properties:
- **Copies per packet:** one, DMA buffer → shared slot. The daemon parses in place in its read-only mapping.
- **Bounds:** the two length guards run before classification and before any copy, in ring mode and legacy mode alike. A 2 KiB RX buffer can report PKT_LEN up to 2048, i.e. 2032 B after the TS header, 48 B more than a slot holds; the oversize guard makes that a counted drop instead of an overrun into the next slot (or past the end of `rxMem` for the last audio slot). `memcpy` length is therefore always ≤ 1984, independent of RCTL.LPE behavior (AS-6). M0 and M4 report `RxOversize`; expected 0, which confirms AS-6.
- **Indices:** published lock-free, with a release store by the producer, an acquire load by the consumer and the mirror pair for the tails.
- **Overrun policy:** drop-newest, so the producer never overwrites a slot the consumer may be reading. That rules out torn reads, and `slot.index` lets the consumer verify it.
- **Legacy mode:** with no rings attached, the harvester keeps filling the Phase 0 software ring for `PollRx` (unchanged, 128 B copies), so `avb-probe rx-ptp`/`pdelay` keep working (DP-12).

Live header fields (the daemon's only view of dext health; SAF-4). Each has exactly one writer path, all on the driver queue:

| Field | Written by | When |
|---|---|---|
| `stage` | `setStage(v, s)`, which replaces every direct `v->stage = …` assignment (today in Stage A, `initDatapath` and `shutdownDatapath`) | every stage change; release store into `hdr->stage` if `rxMem` is attached |
| `flags` | `setFlags(v, set, clear)`, which replaces every `v->flags \|= / &= …` (today in `setupLink` and `shutdownDatapath`; new: the 5 ms STATUS poll, and `RingAttach`/`RingDetach`/`ClientGone` for `AVB_FLAG_RINGS_ATTACHED \| AVB_FLAG_OWNED`, §3.6) | every flag change; mirrored likewise |
| `fault` | `setFault()` (existing single latch point) | when the fault latches, before `setFault` returns; release store |
| `producer_running` | `setStage` and `RingAttach`/`RingDetach`/`ClientGone` | recomputed as `rxMem != nullptr && stage == kAvbStageDatapath && fault == 0`; `setFault` also clears it |
| `last_tick_mach` | `TimerFired` | first statement of every tick (above) |
| `clock_gen` | the `setClock()` helper (§3.6), the only SYSTIM write sequence; `RingAttach` initializes it from `ivars->clockGen` | every SYSTIM write; release store |
| `ctl_head`, `audio_head`, `counters` | harvester | end of every tick |

`RingAttach` initializes `stage`, `fault`, `flags` and `producer_running` in the new header from ivars before returning, so the header is valid from the first map. Because `setFault` writes the header itself, a fault latched inside `harvestRx`, `checkTx` or the stats read is visible to the daemon even though `TimerFired` then stops re-arming (existing behavior: it re-arms only while `ok(v) && stage == 2`).

New registers, all reads, cited in `I210Regs.h` (SAF-2):

| Register | Offset | 333016 section | Use |
|---|---|---|---|
| CRCERRS | 0x04000 | §8.19 statistics *(verify offset)* | accumulated, read every 100 ms |
| MPC | 0x04010 | §8.19 *(verify)* | missed packets |
| RNBC | 0x040A0 | §8.19 *(verify)* | receive no buffers |
| RQDPC(0) | 0x0C030 | §8.11 per-queue drop count *(verify)* | queue 0 drops |
| RX desc status VP bit 3 | — | §7.1.4.2.2 write-back status *(verify bit)* | VLAN stripped indication |

All four counters must be unreachable through `ReadReg32`, so their clear-on-read side effect is owned in one place (the dext's 100 ms accumulator) and DP-8 monotonicity holds:
- CRCERRS, MPC and RNBC lie inside the existing `[kStatsBegin 0x4000, kStatsEnd 0x5000)` denylist.
- RQDPC(0) at 0x0C030 does **not**: it is a queue register. `I210Regs.h` gains `constexpr uint32_t kRqdpc0 = 0x0C030; // 333016 §8.11 RQDPC[0], clear on read (verify)`, `isReadableRegOffsetI210` gains `case i210::kRqdpc0: return false;` in its listed-offset denylist, and `testReadable()` gains `CHECK(!isReadableRegOffsetI210(0xC030));`. `avb-probe regs` doesn't dump 0xC030 (it already lists only Phase 0 registers).

`classifyRx` rules (pure function in `I210Logic.hpp`, host-tested; DP-4):
1. `len < 14` → DropOther. `dst = f[0..6]`. If `ethertype == 0x8100`, then `tagged = true`, `vid = TCI & 0x0FFF`, `pcp = TCI >> 13` and the inner ethertype is at 16; the payload starts at 18. Otherwise the payload starts at 14. A frame with descriptor VP set and no in-buffer tag is treated as tagged with `vlan_tci` (U-12). The in-buffer tag always wins: VP set with an in-buffer 0x8100 (possible with VME = 0) is classified from the in-buffer tag and gets `AVB_RXF_VLAN_FRAME` only. Flags come from `rxVlanFlags(vp, frame)` in `I210Logic.hpp`, so a slot carries exactly one of: none, VLAN_FRAME, VLAN_DESC.
2. `0x88F7` and dst `01:80:C2:00:00:0E` → Gptp.
3. `0x22EA` and dst `01:80:C2:00:00:0E` → Msrp.
4. `0x88F5` and dst `01:80:C2:00:00:21` → Mvrp.
5. `0x22F0` with payload ≥ 1 byte; `subtype = payload[0]`:
   - Control subtypes (`0xFA` ADP, `0xFB` AECP, `0xFC` ACMP) with dst = `91:E0:F0:01:00:00`, dst in `91:E0:F0:00:00:00–91:E0:F0:00:FF:FF` (first four bytes `91:E0:F0:00`), or dst = our MAC → Avdecc. This destination predicate is `isAvdeccDest(dst, ourMac)` in `I210Logic.hpp`; its Rust twin is `avb_core::is_avdecc_dest(mac, our_mac)`, used by the daemon's software loopback (§5.2). Both are tested against the same table of boundary MACs: `91:E0:F0:00:00:00`, `91:E0:F0:00:FF:FF`, `91:E0:F0:01:00:00` and our MAC accepted; `91:E0:F0:01:00:01`, `91:E0:F1:00:00:00` and `91:E0:EF:FF:FF:FF` rejected.
   - `0xFE` MAAP with the same destination set → Maap.
   - Any other subtype: dst in the stream filter and tagged with the entry's VID → Stream. In the filter but untagged → DropUntaggedStream. Otherwise → DropUnboundStream.
6. Anything else (IP, ARP, broadcast, LLDP, etc.) → DropOther.

### 3.5 TX path

`SendFrame` is unchanged in shape: 14–1514 B, padded to 60 by the dext.

Changes:
- Token bucket of `AVB_TX_RATE_PER_SEC` with burst `AVB_TX_BURST`, refilled from mach time. When empty, the call returns `kIOReturnNoResources` and counts TxRateLimited. The frame is not queued. The ceiling is shared across EVERY transmitted frame, audio and control alike, so it must clear one SR class A stream (8000 frames/s) with headroom for gPTP/MSRP/MVRP/AVDECC; at the original 1000/s a talker lost 7 of every 8 PDUs and starved its own MSRP declarations of tokens.
- `want_ts` while a timestamp is pending → `kIOReturnBusy` and TxTsBusyRejected (unchanged semantics, now counted).
- A stale-latch release now counts TxStaleLatch.
- Timeout drops to 20 ms. Normal latency is 24 µs (G4).
- On timeout the dext reads TXSTMPL/H if VALID becomes set later, releasing the latch before the next timestamped TX (existing pre-doorbell check), and the status for that sequence is `kAvbTxTsTimeout`. It never reports a stale value: a timestamp read after a timeout is discarded and counted as stale.

Worst-case daemon TX rate (SAF-6):

| Source | Rate |
|---|---|
| Pdelay_Req | 1/s |
| Pdelay_Resp + Follow_Up | 2/s |
| gPTP Signaling | ≤ 1/s, only if the 16R sends a gPTP-capable TLV (§7.4) |
| MSRP | ≤ 10/s (join timer 200 ms, ≤ 2 PDUs) |
| MVRP | ≤ 5/s |
| ADP | 0.2/s, plus 1 per received ENTITY_DISCOVER |
| AECP commands | paced to ≤ 200/s by the controller |
| AECP responses | 1 per command received |
| ACMP | ≤ 50/s |

Sustained worst case is about 300/s. The dext bound of 1000/s with burst 64 is the hard ceiling.

### 3.6 StepClock, filters, attach

- `StepClock(delta)`:
  - Rejects `|delta| > AVB_STEP_CLOCK_MAX_NS` and results outside `[0, 2^32 s)` with `kIOReturnBadArgument`.
  - Sequence: `m0 = mach; s0 = readClock; m1 = mach; mid = (m0+m1)/2; m2 = mach; target = s0 + delta + ns(m2 - mid); setClock(target); after = readClock`. The target math is `stepTarget()` in `I210Logic.hpp`, host-tested.
  - Writes only SYSTIML and SYSTIMH, in the existing order. Expected residual error is the MMIO write latency, a few µs, which the servo absorbs.
- Clock generation and step tagging (rev 5, dext-owned because only the driver queue serializes SYSTIM writes with harvesting):
  - The existing `setClock(v, ns)` helper is the only SYSTIM write sequence (Stage A init, `SetClock`, `StepClock`). Right after its SYSTIMH write it does `ivars->clockGen += 1` (u32, starts at 0 in `Start`, never reset by `InitDatapath`), `atomic_store_release(&hdr->clock_gen, clockGen)` if `rxMem` is attached, and `rxStepArm(&ivars->stepAmbig, mach_absolute_time(), settleTicks)`. `settleTicks` is `AVB_STEP_SETTLE_NS` converted once at `Start` with the timebase (24 000 ticks at 24 MHz). `AdjFreq` changes neither.
  - `RxStepAmbig { bool pending; uint64_t stepMach; uint64_t settleTicks; }` and the two pure helpers live in `I210Logic.hpp`: `rxStepArm(s, nowMach, settle)` sets `pending = true`, `stepMach = nowMach` (a second step before settling just moves `stepMach` forward); `bool rxStepAmbiguous(s, tickStartMach)` returns `false` if `!pending`; otherwise it returns `true`, and clears `pending` if `tickStartMach >= stepMach + settleTicks`. So every tick from the step up to and including the **first tick that starts at least 1 ms after the SYSTIM write** marks all its slots `AVB_RXF_TS_STEP_AMBIG`, and later ticks don't.
  - Why this is correct regardless of timer latency: the condition uses each tick's measured start time, not its schedule, so a late tick only extends the ambiguous span. A frame received before the SYSTIM write has its descriptor written back within the settle window (RXDCTL.WTHRESH = 1, so each descriptor is written back on completion; 1 ms also exceeds the time to receive and DMA a full RX packet buffer at line rate, *verify RXPBS size in 333016 §8.10*). The settling tick reads every DD descriptor in the ring (the loop bound equals `kRxRingSize`), so it harvests every pre-step frame; a later tick only sees descriptors completed after the settling tick started, i.e. after the step. The cost is that frames received up to about 1–1.5 ms after a step (longer if ticks are late) are unusable for timing; steps happen only at ACQUIRE and on > 100 µs excursions.
  - The legacy `PollRx` path (`AvbRxEntry`) is unchanged and carries no generation; only the probe uses it.
- `SetStreamFilter`:
  - Requires rings attached and caller = owner.
  - Validates `count ≤ 16`, every `dest_mac[0] & 1`, and `1 ≤ vid ≤ 4094`. Otherwise `kIOReturnBadArgument`.
  - Copies into ivars. Applied from the next tick, which runs on the same queue, so no tearing.
- `RingAttach(flags)`:
  - `flags != 0` → BadArgument; stage < 1 → NotReady; another owner → ExclusiveAccess; same owner while attached → returns the existing values (idempotent); caller previously detached on this connection → NotPermitted (below).
  - Allocates `rxMem` (`IOBufferMemoryDescriptor::Create(kIOMemoryDirectionInOut, AVB_RX_MEM_SIZE, 16384)`) and `consumerMem` (16 KiB), zeroes both, fills the header constants (including `clock_gen` from `ivars->clockGen`), increments `generation`, sets `owner`, and then calls `setFlags(v, AVB_FLAG_RINGS_ATTACHED | AVB_FLAG_OWNED, 0)`, which also mirrors the flags into the new header.
  - Allocation failure → `kIOReturnNoMemory`, logged once. It is not a hardware fault, so nothing is latched.
  - One attach per connection. `IOUserClient.iig` requires `CopyClientMemoryForType` to return the same `IOMemoryDescriptor` instance for a given type on a given user client, so after a `RingDetach`, a further `RingAttach` from the **same** user client returns `kIOReturnNotPermitted` (logged `AVBI210: rings: reattach on same connection; reopen`). The flag is a user-client ivar `ringsUsed`, set by the driver through `void MarkRingsUsed() LOCALONLY` and read through `bool RingsUsed() LOCALONLY`, both new in `AVBI210UserClient.iig`. A new connection's `RingAttach` allocates **fresh** memory and increments `generation`; it never reuses a previous attach's buffers. `scalarOut[0]` returns the new generation. The daemon already uses exactly one attach per connection (§5.2, §5.3).
  - If `stage == 2` at attach time (a data path a probe left up), the harvester switches from legacy mode to ring mode on the next tick and `producer_running` becomes 1.
- `RingDetach()` (strictly owner; not attached → `kIOReturnNotReady`; caller ≠ owner → `kIOReturnExclusiveAccess`), on the driver queue:
  1. `producerRunning = false`; `hdr->producer_running = 0` (release store).
  2. Clear the stream filter. If `stage == 2`, the harvester reverts to legacy software-ring mode from the next tick (the data path itself stays up). `setFlags(v, 0, AVB_FLAG_RINGS_ATTACHED | AVB_FLAG_OWNED)`, while `rxMem` is still valid, so the mirrored header write lands in live memory.
  3. `OSSafeReleaseNULL(rxMem)`, `OSSafeReleaseNULL(consumerMem)`. The client's existing mappings keep their own reference until it calls `IOConnectUnmapMemory64`; they are then stale (their `generation` no longer matches any live attach) and the producer never writes them again.
  4. `owner = nullptr`.
- Daemon generation check: after mapping (§5.2 step 5) the daemon requires `hdr.generation == RingAttach scalarOut[0]`; a mismatch is exit 4 (`shared-memory generation mismatch`).
- Order on the daemon's clean shutdown: `ShutdownDatapath` → `RingDetach` → unmap both → close (§5.3). The harvester stops before the rings are released.
- RX buffer chunking: 16 × 256 KiB (AS-4). If `PrepareForDMA` on any chunk returns `segCount != 1`, `initDatapath` frees everything and latches `kAvbFaultDma` (existing `dmaAlloc` behavior, logged `AVBI210: FAULT …`). There is no retry with another size.

### 3.7 Invariants owned by the dext

| Invariant | Enforced in | Why there |
|---|---|---|
| Only 8086:1533 / 1ab6:0214 (SAF-1) | personality, entitlement, `Start` `isOurDevice` (unchanged) | Only the dext touches PCI |
| No generic write, documented registers only (SAF-2) | no new write sequences; four new reads cited | Hardware safety can't depend on the daemon |
| Producer never overwrites unconsumed slots; untrusted tails can't corrupt the dext | harvest validation | The consumer page is client-writable |
| Slot copy length ≤ `AVB_RX_SLOT_DATA_MAX`; no `len − 16` underflow | harvest guards (§3.4) | Dext memory safety can't depend on an unverified hardware limit (AS-6) |
| ≤ 1 timestamped TX outstanding; no stale TS | `sendFrame`/`checkTx` | The latch is hardware state |
| TX rate ≤ 1000/s (SAF-6) | token bucket | Bound holds even if the daemon misbehaves |
| \|ppb\| ≤ 488 281, step ≤ 1 s | `AdjFreq`/`StepClock` | Register field limits |
| Stop writing before memory release (DP-11) | `ClientGone` and `RingDetach` order on the driver queue | Only the driver queue serializes with the harvester |
| Ownership only via rings; non-owner exit changes nothing | `RingAttach`/`RingDetach`/`ClientGone` (§3.2) | Keeps Phase 0b multi-process probe semantics when no daemon runs |
| Live header fields always reflect ivars | `setStage`/`setFlags`/`setFault`, the only writers (§3.4) | The daemon's fault and link detection depends on them even after the timer stops |
| Every slot's `clock_gen`/`AVB_RXF_TS_STEP_AMBIG` correctly separates pre-step from post-step receptions | `setClock()` helper + `rxStepAmbiguous` per tick (§3.6) | Only the driver queue orders SYSTIM writes against harvesting; a daemon-side mach window would depend on timer latency |
| Clear-on-read statistics read only by the accumulator | `isReadableRegOffsetI210` denylist incl. RQDPC(0) | DP-8 monotonicity |

Host tests added to `driver/tests/i210logic_tests.cpp`:
- `classifyRx`: every rule and every drop reason, tagged and untagged, descriptor VP;
- `rxFrameBounds(len, tsip) -> {Ok(flen) | ShortTs | Oversize}` (the pure function behind the two harvest guards): TSIP with len 0, 15, 16, 2000, 2048 (→ Oversize, flen 2032), non-TSIP 1984 (Ok) and 1985 (Oversize);
- `rxHarvestStep`: sequences EOP (→ Frame); !EOP, !EOP, EOP (→ NonEop, Discard, Discard, flag clear afterwards); !EOP, EOP, EOP (→ NonEop, Discard, Frame), and the counter deltas (one RxNonEop, RxFrames only for `Frame`);
- VLAN flag derivation `rxVlanFlags(vp, frame)` for all four VP × in-buffer-tag combinations (§3.4 rule 1): VP=0/no tag → none; VP=0/tag → VLAN_FRAME; VP=1/tag → VLAN_FRAME only; VP=1/no tag → VLAN_DESC only;
- ring index validation (`ringFree(head, tail, slots)`, including wrap at 2^64 and invalid tails);
- `stepTarget`;
- `rxStepArm`/`rxStepAmbiguous` (settle = 1 ms in ticks): no step → never ambiguous; step at T, ticks starting at T+0.2 ms and T+0.7 ms → ambiguous, still pending; tick at T+1.2 ms → ambiguous and settles; next tick → not ambiguous; **simulated 5 ms tick delay**: step at T, next tick at T+5 ms → ambiguous and settles (it is the first settled tick), the tick after it → not ambiguous; a second step at T+0.5 ms before settling moves `stepMach` (tick at T+1.2 ms still pending, T+1.6 ms settles); tick start exactly at `stepMach + settle` settles;
- the token bucket (`tokenBucketTake(state, nowNs)`);
- stats accumulation;
- the ABI `offsetof`/`sizeof` literals;
- rejection of filter entries with a unicast MAC or VID 0/4095;
- `isAvdeccDest` boundary table (shared with `avb-core`);
- `CHECK(!isReadableRegOffsetI210(0xC030))` (RQDPC(0));
- `producerRunning(attached, stage, fault)` truth table (the `producer_running` rule above).

## 4. Learning the 16R early (item 5) and milestone order

The 16R's entity model, stream count and Milan flavor are unknown (U-1…U-13). The build is therefore ordered so the first hardware run produces the AEM and a raw capture before any listener logic depends on them.

| Milestone | Built | First hardware run produces | Resolves |
|---|---|---|---|
| M0 | ABI v3 dext + `avb-dext` crate + probe migration | `avb-probe info`, `clock`, then the multi-process `datapath up` → `rx-ptp` → `pdelay` → `datapath down` sequence still pass (no owner); `avb-daemon capture` works and a concurrent `avb-probe pdelay` gets ExclusiveAccess | DP-1, DP-12 regression |
| M1 | `avb-avdecc` controller side (ADP, AECP AEM READ_DESCRIPTOR + GET_*, MVU), `capture`, `aem-dump` | `.agents/tasks/phase1a-aem-16r.json`; `.agents/tmp/m1-idle.pcap` (120 s of all control traffic from the 16R with no listener) | U-1…U-5, U-6 (MVU, ADP fields), U-8 (does it send Talker Advertise without a listener? Domain values), U-11 (Announce/Sync present?), AS-5 for control frames only (TSIP on ADP/AECP/MSRP) |
| M2 | `avb-gptp` slave + servo + clock model, `gptp` subcommand | `gptp --seconds 360` JSON | PTP-2…PTP-9, U-11, U-13 (with GET_AVB_INFO from M1 code) |
| M3 | `avb-srp`, our entity + ACMP listener/controller, `run [--capture P]` | binding log, MSRP log, `.agents/tmp/m3-bound.pcap` (stream frames, filter set by the binding) | U-7, U-8, U-10, U-12 (VLAN placement on the first bound AVTP frames), AS-5 for AVTP, SRP-*, ADC-13…15 |
| M4 | `avb-avtp` AAF, `avb-mediaclock`, `avb-audioring`, `streams`, `record` | 600 s WAV + summary | AAF-*, MCR-*, RING-*, U-9, CLI-* |
| M5 | Robustness runs, results doc | SIGKILL/SIGINT/cable-pull evidence | SAF-4, DP-11, CLI-5, VER-* |

Why U-12 and the AVTP half of AS-5 move to M3: with an empty stream filter, the dext drops every tagged AVTP stream frame as `DropUnboundStream`, and the 16R doesn't transmit a stream before a listener is ready. The first tagged frames the daemon can see are those of the first bound stream. M1 can still show tagged control traffic, if the 16R sends any, which the capture records but doesn't rely on.

The M1 run happens as soon as M0 and M1 build, before M2–M4 are written. If the M1 data contradicts a decision here (for example, the 16R exposes 24 streams, uses INT_24BIT packed, or requires a Milan command we didn't plan), the implementer records it under "Deviations" in the build-verification doc and adapts. Three contradictions must not be adapted silently; they go back to the user:
- S_max must grow beyond 16: that changes the ABI ring sizes.
- The 16R rejects a non-OUI entity_id (U-10): that is a blocker (A-3).
- The 802.1Q-2018 §35.2.4.3/§35.2.6 check (done while writing `avb-srp`, before M3) shows that an end station may not declare a Listener attribute without a registered Talker attribute. Then the §8.2/§9.4 `TA_WAIT` edge is not implemented, and if M1/M3 show the 16R withholding its TA until a Listener exists (U-8), the user decides how to proceed.

`aem-dump` needs no gPTP, MSRP or binding. It brings up the data path, runs ADP discovery and the AECP walk only, and doesn't advertise our entity. Our entity_id is still used as `controller_entity_id`.

`capture --seconds N --out <path.pcap>` captures the control ring only: it sets no stream filter and sends nothing, so it can't see stream frames (there is no `--class` flag). Stream frames are captured with `run --capture <path.pcap>`: the `audio` thread copies the first 4096 audio slots it consumes (about 0.5 s of one stream) into a buffer preallocated at start (a `memcpy`, no allocation, so NFR-3 holds), `ctl` copies every control slot into a 4096-entry preallocated buffer likewise, and the pcap is written at shutdown. Both write classic pcap with nanosecond resolution (magic 0xA1B23C4D). Each record's timestamp is `hw_ts_ns` if `AVB_RXF_TS_HW`, else the harvest time. A tag is re-inserted from `vlan_tci` only when the slot has `AVB_RXF_VLAN_DESC` (which by definition means the buffer has no tag); `AVB_RXF_VLAN_FRAME` frames are written verbatim, so a frame never gains a second tag. A JSON side file lists slot flags per frame index. Our own code writes it; no libpcap. Captures from M1 become regression vectors under `crates/*/tests/vectors/` (NFR-5): ADP, AECP responses, Pdelay, Announce, Sync/Follow_Up and MSRP from the 16R, trimmed to the frames used.

What the AEM dump drives, never hardcoded (ADC-9, RING-4, acceptance 14):
- **Audio talker set A and stream count (AS-9, A-5):**
  - A = the 16R STREAM_OUTPUTs of the current configuration (descriptor_counts[STREAM_OUTPUT]) whose `current_format` is AAF (subtype 0x02), in ascending STREAM_OUTPUT index. The stream count is |A|.
  - Non-AAF outputs (CRF, IEC 61883, vendor-specific) are decoded for the dump, listed in `summary.unbound_by_design[]` as `{stream_output_index, name, format_raw, subtype_name}`, and don't count toward "all streams". They never cause exit 8.
  - Within A: an AAF `current_format` that `decodable()` rejects → that stream is refused (exit 8 unless `--allow-partial`); |A| > 16 → the S_max refusal (§9.2).
  - The bound set B = the elements of A that are not refused, in order, at most 16 (without `--allow-partial`, any refusal has already ended the run with exit 8, so B = A).
  - Our STREAM_INPUT j (j = 0..|B|−1) maps to B[j]. Ring stream order is j. The map `{j → stream_output_index (= talker_unique_id)}` is written to the channel sidecar and the summary.
- **Per-stream format:** the STREAM_OUTPUT `current_format` and `formats[]`, decoded through `avb_avtp::StreamFormat`.
- **Channels per stream, samples per frame, bit depth:** taken from that format.
- **Sample rate:** from the AUDIO_UNIT `current_sampling_rate`, cross-checked against the format's nsr. A disagreement is an error (exit 8).
- **Channel names:** STREAM_PORT_OUTPUT → AUDIO_CLUSTER names in stream-channel order via AUDIO_MAP when present. Otherwise `"<stream name> ch<k>"`.
- **Clock source:** CLOCK_DOMAIN `clock_source_index` → CLOCK_SOURCE type.
- **Milan quirks:** recorded under `quirks[]` in the dump. Examples: commands answered NOT_IMPLEMENTED, IN_PROGRESS use, descriptor types we don't decode, non-zero `ut` in formats, unexpected vendor-unique protocols.

## 5. Daemon runtime model

### 5.1 Threads

| Thread | Scheduling | Loop | Owns |
|---|---|---|---|
| `main` | default | waits on the shutdown flag (100 ms) | CLI, start/stop sequencing, status printing |
| `ctl` | default QoS user-interactive | every 500 µs: drain the control ring; run timers; flush the TX queue; poll a pending TX timestamp; every 125 ms ReadClock(8) for the clock model | `Device`, gPTP port + servo, MRP participant, AVDECC controller + entity, gPTP time model publisher |
| `audio` | Mach `THREAD_TIME_CONSTRAINT_POLICY` (period 250 µs, computation 60 µs, constraint 200 µs, preemptible) via `mach2::thread_policy`. The policy fields are mach absolute ticks: each value is converted as `ns × denom / numer` from `mach_timebase_info` (on this host's 24 MHz timebase, 125/3: period 6000, computation 1440, constraint 4800 ticks) | every 250 µs (`mach_wait_until`): drain the audio ring; AAF parse; placement; ring commit; media-clock DLL | stream table snapshot, `avb_audioring::Producer`, `avb_mediaclock::MediaClock`, per-stream counters |
| `rec` | default | ≥ every 10 ms: consume the audio ring into the WAV writer | WAV file, `avb_audioring::Consumer` |

Cross-thread communication; nothing on the per-packet path locks or allocates:
- **ctl → audio stream table:**
  - Sent over `std::sync::mpsc::sync_channel::<Box<StreamTable>>(4)` on bind/unbind/format change. A bounded channel allocates its buffer at construction, so neither side allocates on send.
  - The audio thread `try_recv`s between batches.
  - It returns the old table over a second `sync_channel(4)` with `try_send`, so the drop happens on `ctl`. On `TrySendError::Full` the audio thread keeps the old box in a single-slot `Option` (preallocated) and retries on the next batch before accepting another new table; each `Full` is counted in `audio_return_full` (status). `ctl` uses blocking-free `try_send` too and, on `Full`, retries on its next 500 µs tick.
- **ctl → audio gPTP time model:** `avb_core::SeqLock<GptpMachModel, N>` (writer `ctl`, readers `audio`/`rec`). To be free of data races in Rust, the seqlock stores no plain fields: its storage is `seq: AtomicU32` plus `[AtomicU64; N]`, and `T: SeqWords<N>` (`fn to_words(self) -> [u64; N]`, `fn from_words([u64; N]) -> Self`; f64 via `to_bits`/`from_bits`, bool/u32 widened). Writer: `seq.store(s + 1, Relaxed)` (odd) → `fence(Release)` → each word `store(Relaxed)` → `seq.store(s + 2, Release)`. Reader: `s1 = seq.load(Acquire)`; if odd, spin and retry; each word `load(Relaxed)`; `fence(Acquire)`; `s2 = seq.load(Relaxed)`; accept iff `s1 == s2`. The writer never blocks; a reader retries at most 1000 times and then returns `Err(Contended)` (counted `seqlock_contended`; the caller keeps its previous copy). The §10.6 ring clock block uses the same protocol on its shared-memory fields.
- **audio → ctl per-stream counters:** `Arc<[StreamCounters; 16]>` of `AtomicU64`, relaxed. AECP GET_COUNTERS and status read them.
- **audio → ring header media-clock model:** `avb_audioring` seqlock block (§10.6).
- **main ↔ all shutdown:** a static `AtomicBool` set from the SIGINT/SIGTERM handler (installed with `libc::sigaction`; the handler only stores the flag).

### 5.2 Dext I/O (`avb-daemon/src/dextio.rs`)

Start sequence; exit codes in §11.4:
1. `Device::open_i210()`.
2. `GetInfo`. ABI ≠ 3 → exit 4: `dext ABI v<n> != avb-daemon ABI v3; rebuild and reactivate (scripts/package.sh, avb-installer activate)`. Fault ≠ 0 → exit 7.
3. `RingAttach(0)`. ExclusiveAccess → exit 10: `the I210 rings are owned by another client (another avb-daemon); stop it first`. This makes the daemon the owner (§3.2).
4. `IOConnectMapMemory64(type 1)` and `(type 2)` with `kIOMapAnywhere`.
5. Validate the header: magic, ABI, header/slot sizes, slot counts, offsets, total size equal to the Rust constants, and `generation` equal to `RingAttach`'s `scalarOut[0]`. Mismatch → exit 4. Then acquire-load `clock_gen` as the initial step generation (§7.3).
6. If `stage == 2` (a data path a probe left up with no owner, which §3.2 allows), call `ShutdownDatapath` first. Ownership is unaffected, so no other client can take the data path between this step and the next. Then `InitDatapath(0)`, which resets the dext counters and sets `producer_running = 1`.
7. Wait ≤ 10 s for `AVB_FLAG_LINK_UP` in the header. Otherwise exit 5: `no link on the I210 (cable to the 16R?)`.

RX dispatch on `ctl`: for each control slot, check `slot.index == tail`. A mismatch counts `consumer_desync`, logs an error at most once per 10 s, and jumps the tail to head. Then match `cls` → `gptp.handle_rx`, `srp.handle_rx(Msrp|Mvrp)`, `avdecc.handle_rx`, or MAAP (counted and ignored; we allocate nothing). Finally release-store the tail.

TX scheduler on `ctl` (`avb-daemon/src/txsched.rs`, pure logic over `DextPort`, unit-tested):
- Three fixed sub-queues, each an `avb_core::Outbox<N>`: **gPTP 8, MRP 16, AVDECC 40** frames. Each protocol crate's `poll` writes only into its own class's outbox, so AVDECC traffic can never occupy gPTP capacity.
- Dequeue order: strict priority gPTP > MRP > AVDECC; FIFO within a class.
- `push()` into a full sub-queue drops the **new** frame, returns `Err(TxFull)` and counts `tx_queue_full[class]`. Caller behavior on `TxFull`:
  - AECP and ACMP commands: counted as a send failure; the command's existing timeout/retry (§9.2, §9.4) covers it;
  - AECP/ACMP/ADP responses and advertisements: dropped and counted; the peer retries or the next advertisement interval covers it;
  - gPTP: counts `pdelay_tx_fail` (Pdelay_Req: the exchange is lost and counted as a lost response; Pdelay_Resp: the 16R's exchange is lost);
  - MRP: nothing else; the next join timer (200 ms) retransmits the declaration.
- One `SendFrame` per frame.
- A `want_ts` frame is held while a timestamp is outstanding. Holding it doesn't block a lower class's non-timestamped frame (the scheduler skips to the next sendable frame in priority order).
- `kIOReturnBusy` / `kIOReturnNoResources` → keep the frame at the head of its sub-queue and retry next iteration, counted.
- Worst-case gPTP queueing: at most 3 gPTP frames are ever pending (Pdelay_Req, Pdelay_Resp, Pdelay_Resp_Follow_Up, plus a rare Signaling), so 8 never fills in normal operation; and because gPTP is dequeued first, a full AVDECC queue (e.g. Hive and the 16R enumerating our entity during our own AEM walk) adds no delay to a Pdelay_Resp beyond one in-flight `SendFrame`. A unit test fills the AVDECC queue to 40, enqueues a Pdelay_Resp and asserts it is the next frame sent; a second test asserts the 41st AVDECC push returns `TxFull` and the counter increments.
- The timestamp is polled with `GetTxTimestamp` every iteration until OK/Timeout.
- The result is delivered to the frame's owner (gPTP) with the sequence number.

Software loopback: every frame sent with an AVDECC control subtype (0xFA/0xFB/0xFC) whose destination satisfies `avb_core::is_avdecc_dest(dst, our_mac)` (the same predicate as the dext's `classifyRx` rule 5) is also delivered to the local AVDECC dispatcher after the send succeeds. The NIC doesn't receive its own frames, and our controller and listener entity share the process (D8). Loopback frames carry a `loopback` flag so ADP doesn't treat our own advertisement as a remote entity.

Health checks every `ctl` iteration (SAF-4), all from header fields with defined writers (§3.4), acquire loads:
- `hdr.fault != 0` → stop TX, report `dext fault <n> (<name>)`, begin shutdown, exit 7.
- `hdr.flags & LINK_UP` cleared → stop protocol TX, report `link lost`, begin shutdown, exit 5.
- Harvester watchdog: if `hdr.producer_running == 1` and `now − hdr.last_tick_mach > 50 ms` (converted to ticks with the timebase), call `GetInfo`:
  - `fault != 0` → exit 7 as above;
  - `stage != 2` → report `dext data path stopped`, exit 7;
  - otherwise count `harvest_stall`, log at warn (≤ 1 / 10 s) and continue. 50 ms is 100 ticks of 500 µs and below the 64 ms audio-ring headroom.
- Any `IOConnectCallMethod` returning `kIOReturnNoDevice`, `kIOReturnNotResponding`, `kIOReturnNotAttached` or `MACH_SEND_INVALID_DEST` → device removed, exit 7.

None of these retries register operations.

### 5.3 Clean shutdown (CLI-5)

On the shutdown flag:
1. `ctl` withdraws every MSRP Listener and Domain declaration and the MVRP VID (sends Lv events; SRP-7).
2. It sends ADP ENTITY_DEPARTING (ADC-7).
3. It flushes the TX queue for ≤ 200 ms.
4. `rec` finalizes the WAV header and sidecars.
5. `ctl` calls `ShutdownDatapath` (harvester stops, rings stay attached), then `RingDetach` (rings released, ownership dropped), then unmaps both memory types, then closes the connection. This order guarantees the producer has stopped before the rings disappear.

The next run starts from step 1 of §5.2 without a replug. On SIGKILL none of this runs: the dext's `ClientGone` performs the hardware part (DP-11), and the 16R times out our declarations per MRP leave timers.

## 6. Rust workspace: crates, boundaries, dependencies

### 6.1 Layout

```
Cargo.toml                       # members: all crates below
crates/
├── avb-core/        lib avb_core      MacAddr, Eui64, StreamId, EthFrame/VLAN parse+build, MachTicks,
│                                       Timebase, GptpNs, ext32 (32->64 bit extension), SeqLock<T: Copy>,
│                                       Outbox<const N> (fixed-capacity TX list; push -> Result<(), TxFull>,
│                                       drop-new on full), Counter helpers
├── avb-dext/        lib avb_dext      abi (ABI v3 mirror; moved from avb-probe), iokit::Device (moved),
│                                       shm::{RxRings, RxSlotRef, ConsumerTails}, counters names
├── avb-gptp/        lib avb_gptp      msg (encode/decode), pdelay (initiator+responder), port (asCapable,
│                                       Announce/Sync/FU), servo (PI), model (gPTP<->mach), status
├── avb-srp/         lib avb_srp       mrp (applicant/registrar/leaveall SMs, PDU vector codec),
│                                       msrp (attributes), mvrp (VID), participant (end-station)
├── avb-avtp/        lib avb_avtp      common (AVTP header, subtypes), aaf (decode; encode for tests),
│                                       stream_format (AEM 64-bit format codec, AAF + CRF decode)
├── avb-avdecc/      lib avb_avdecc    adp, aecp (aem + mvu codecs, inflight/retry), acmp, aem::{desc codecs,
│                                       model, json}, controller (discovery, enumeration, binder),
│                                       entity (our listener: responder, ACMP listener SM, notifications),
│                                       derive (16R AEM -> our listener model), milan (constants)
├── avb-mediaclock/  lib avb_mediaclock dll (2nd-order DLL), recovery (reference stream, lock SM),
│                                       placement (pt -> frame index), stats (windows, jitter)
├── avb-audioring/   lib avb_audioring layout (repr(C) header, consts), Producer, Consumer, ClockModel seqlock
├── avb-daemon/      bin avb-daemon    main (clap CLI), stack (wiring), dextio, txsched, ctl, audio, rec, wav, pcap,
│                                       report (JSON summaries), exit (codes), log (prefix macro)
├── avb-probe/       (existing)        abi/iokit replaced by `pub use avb_dext::{abi, iokit};`
└── avb-installer/   (existing, unchanged)
```

Dependency graph (no cycles; Phase 1b links everything except `avb-daemon`):

```
avb-core  <-  avb-gptp, avb-srp, avb-avtp, avb-avdecc, avb-mediaclock, avb-audioring
avb-avtp  <-  avb-avdecc
avb-dext  (standalone: io-kit-sys, mach2, core-foundation-sys, anyhow)
avb-core has no third-party dependencies (std only; serde is not needed there)
avb-daemon <- all of the above;  avb-probe <- avb-dext
```

Protocol crates are sans-IO. Each exposes this shape:
- `handle_rx(&mut self, now: Now, frame: &[u8], meta: RxMeta) -> Result<(), DropReason>`
- `poll(&mut self, now: Now, out: &mut Outbox)`
- `next_deadline() -> Option<MachTicks>`

`Now` carries mach ticks and, when known, gPTP ns. No crate except `avb-dext` and `avb-daemon` does I/O, spawns threads or reads clocks. That is what makes CRT-2 host tests possible: tests drive time and frames directly.
- `avb_core::MachTicks` is a plain `u64` newtype. Conversions take a `Timebase { numer: u32, denom: u32 }` value; avb-core never calls `mach_absolute_time` or `mach_timebase_info`. The daemon gets the timebase from `AvbInfo.tb_numer/tb_denom` (via avb-dext) and cross-checks it once against `mach2::mach_time::mach_timebase_info` in avb-daemon; a mismatch is exit 4.
- The single exception to "no I/O" is `avb_audioring::AudioRing::new_anonymous`, which calls `libc::mmap`/`munmap` to create the shared mapping. It is a memory allocation at construction, not run-time I/O; `Producer`/`Consumer` and `from_raw` make no system calls.

### 6.2 Boundaries Phase 1b relies on

- `avb-audioring::{layout, Consumer, ClockModel}` is the only thing the AudioServerPlugIn needs to read audio and timing. Its dependencies are `avb-core` and `libc =0.2.190`, the latter used only by `new_anonymous`; the `from_raw` path Phase 1b uses makes no libc calls. It allocates nothing after construction, and its header is position-independent (offsets only), so any shared-memory transport works in 1b.
- `avb-avdecc::{aem::model, controller, entity}` and `avb-gptp::status` are what the 1b app and daemon API will expose.
- The rate-change sequencing that 1b adds (feasibility "Sample rates") attaches at `avb-avdecc::controller::Binder` and `avb-avdecc::derive`.

### 6.3 Dependencies (CRT-3): exact pins, all already in `Cargo.lock`

| Crate | Pin | License | Used by |
|---|---|---|---|
| anyhow | `=1.0.104` | MIT OR Apache-2.0 | avb-dext, avb-daemon (binary-level errors only) |
| clap (features `derive`) | `=4.6.7` | MIT OR Apache-2.0 | avb-daemon |
| serde (features `derive`) | `=1.0.229` | MIT OR Apache-2.0 | avb-gptp (status), avb-avdecc (AEM JSON), avb-mediaclock (stats), avb-daemon |
| serde_json | `=1.0.151` | MIT OR Apache-2.0 | avb-avdecc (dump), avb-daemon |
| io-kit-sys | `=0.5.0` | MIT OR Apache-2.0 | avb-dext (`IOConnectCallMethod`, `IOConnectMapMemory64`, `IOConnectUnmapMemory64`) |
| core-foundation-sys | `=0.8.7` | MIT OR Apache-2.0 | avb-dext |
| mach2 | `=0.7.0` | BSD-2-Clause OR MIT OR Apache-2.0 | avb-dext (`mach_absolute_time` around calls), avb-daemon (`mach_absolute_time`, `mach_timebase_info`, `thread_policy_set`, `mach_wait_until`). Not avb-core |
| libc | `=0.2.190` | MIT OR Apache-2.0 | avb-daemon (`sigaction`, `statvfs`), avb-audioring (`mmap`/`munmap` in the anonymous-shared constructor) |

Licenses were checked against each crate's cached `Cargo.toml` on this host.

Rules:
- Nothing new is downloaded: `libc 0.2.190` is already a transitive dependency.
- `Cargo.lock` is updated once, `cargo build --offline` style, to record the new direct edges, and committed. Every build and test runs with `--locked`.
- Internal crates are `path` dependencies (no version).
- No dev-dependencies are added: tests use std only.
- `mach2 0.7.0` provides `mach_wait_until`, `thread_policy_set` and `THREAD_TIME_CONSTRAINT_POLICY` (checked in the cached source).

The GPL/LGPL boundary (SAF-7): no code, comment, table or struct layout is copied from Linux igb, igb_avb, OpenAvnu kernel parts, L-Acoustics avdecc or any GPL/LGPL source. Encodings come from the IEEE/Avnu texts and are pinned by our own vectors. The review checklist (§14.4) includes this.

### 6.4 Build gate (CRT-4)

The commands are unchanged from Phase 0b; the workspace just has more members:
- `cargo fmt --check --manifest-path …`;
- `cargo clippy --workspace --all-targets --locked -- -D warnings`;
- `cargo test --workspace --locked`;
- `make -C driver/tests test`;
- both `xcodebuild` dext builds with 0 warnings;
- `scripts/package.sh --adhoc`.

All of them run with `TMPDIR=/Users/dexter/AVB-Audio/.agents/tmp/`, no `cd`, and `xcodebuild … -derivedDataPath /Users/dexter/AVB-Audio/.agents/tmp/DerivedData` added to the script's invocations (ENV-1).

### 6.5 Packaging (CRT-5)

- `scripts/package.sh` copies `target/release/avb-daemon` to `AVBInstaller.app/Contents/MacOS/avb-daemon` and signs it with `-o runtime` and the new `packaging/daemon.entitlements`. That file contains only `com.apple.developer.driverkit.userclient-access = [com.avbaudio.installer.i210]`.
- The probe's entitlements are unchanged.
- Only the I210 dext is embedded (unchanged).
- The verify step prints the daemon's entitlements.

## 7. gPTP (`avb-gptp`): slave-only time-aware end station

### 7.1 Identity and messages

- clockIdentity = EUI-64 from the I210 MAC with `FF:FE` inserted (`64:4b:f0:ff:fe:12:73:b8`), portNumber 1. Computed from `AvbInfo.mac` at start, never configured (PTP-1).
- Message header fields:
  - transportSpecific (majorSdoId) = 1;
  - versionPTP = 2;
  - domainNumber 0;
  - our logMessageInterval: Pdelay_Req 0 (1 s); Pdelay_Resp and Pdelay_Resp_Follow_Up use 0x7F.
- Encoded and decoded messages:
  - Sync (two-step, Rx only);
  - Follow_Up with the 802.1AS Follow_Up information TLV (Rx);
  - Pdelay_Req, Pdelay_Resp and Pdelay_Resp_Follow_Up (Tx and Rx);
  - Announce with path-trace TLV (Rx);
  - Signaling with the message-interval-request TLV and the 802.1AS-2020 gPTP-capable TLV (Rx; Tx only as in §7.4).
- **We never send Announce, Sync or Follow_Up.** Our port is never Master, so priority1 = 255 never appears on the wire. The value 255 is still the constant reported in AVB_INTERFACE/GET_AVB_INFO. The 16R wins BMCA by default because we are silent (PTP-1).
- Timestamp correction at 1000 Mb/s, applied to every event timestamp before use (Phase 0b values; U-13):
  - `tx_c = tx + 178 ns`;
  - `rx_c = rx − 448 ns`.

  Raw values are also kept for reporting. The constants live in `avb_gptp::LatencyProfile::I210_1000`.

### 7.2 Peer delay

Initiator: every 1 s ± 0, the port sends Pdelay_Req with `want_ts`:
- t1 = TX timestamp (compensated).
- On Pdelay_Resp for our sequence and portIdentity: t2 = requestReceiptTimestamp, t4 = RX timestamp.
- On Pdelay_Resp_Follow_Up: t3 = responseOriginTimestamp + correctionField.
- The exchange is complete when all of t1–t4 are present within 1 s. Otherwise `lost_responses += 1`.
- Computation:
  - `NRR = (t3_n − t3_{n−k}) / (t4_n − t4_{n−k})` where exchange `n−k` is the oldest entry of the NRR window (k ≤ 8). The window holds only complete exchanges of the **current step generation** (§7.3 "Step bookkeeping"); it is cleared on every generation change, and a candidate NRR with |NRR − 1| > 1000 ppm is rejected (counted `nrr_out_of_range`) and its exchange is not added. "NRR valid" means the window holds ≥ 2 exchanges of the current generation, **or** a held NRR from the previous generation exists (held until 2 new-generation exchanges complete);
  - `delay = ((t4 − t1)·NRR − (t3 − t2)) / 2`, computed for both raw and compensated timestamps.
- A TX timestamp timeout makes the exchange incomplete and counts `pdelay_tx_ts_fail` (DP-9 target 0).

Responder:
- On the 16R's Pdelay_Req: t2 = its RX timestamp. Send Pdelay_Resp (`want_ts`, requestReceiptTimestamp = t2_c, requestingPortIdentity copied, correction 0).
- Then t3 = TX timestamp. Send Pdelay_Resp_Follow_Up (responseOriginTimestamp = t3_c).
- Turnaround budget, Pdelay_Req arrival to Pdelay_Resp on the wire: ≤ 0.5 ms harvest + ≤ 0.5 ms `ctl` loop + about 50 µs SendFrame, so expected ≤ 1.2 ms and worst case ≤ 3 ms. The 802.1AS bound is 10 ms (IEEE 802.1AS-2020 Annex B.2.3, "Pdelay turnaround time" *(verify clause)*). Turnaround is measured per response (t3 − t2) and reported with its max. Exception (rev 5): a Pdelay_Resp can wait behind a pending step (§7.3 TX generation), which is itself held for up to 20 ms behind an outstanding timestamp. When the scheduler is about to hand a Pdelay_Resp to `SendFrame` and `mach_now − harvest_mach` of the 16R's Pdelay_Req slot exceeds **8 ms** (mach is used because it is unaffected by steps), the Pdelay_Resp and its Follow_Up are dropped and `pdelay_resp_late_drop` is counted; the 16R counts one lost response. With the normal ≤ 0.5 ms harvest delay that keeps every Pdelay_Resp we do send within 10 ms of t2. This only arises after a TX timestamp timeout (DP-9 target 0).
- If the TX timestamp for Pdelay_Resp times out, no Follow_Up is sent; this is counted as `pdelay_resp_ts_fail`.
- Pdelay_Resp is serialized behind any outstanding timestamped frame by the TX scheduler (§5.2). Our own Pdelay_Req is deferred by up to 50 ms if a response is in flight, so the two never contend.

asCapable (PTP-5) is true iff all of these hold:
- compensated `delay ≤ 800 ns` (neighborPropDelayThresh);
- `lost_responses` since the last good exchange is ≤ 3 (allowedLostResponses);
- no multiple-responder condition: no exchange in the last 3 had more than one Pdelay_Resp;
- NRR is valid, from ≥ 2 complete exchanges;
- if the 16R has sent a gPTP-capable TLV, its gPTP-capable receipt timeout (per the TLV interval) hasn't expired.

Every transition is logged with the reason and counted.

NRR reporting (PTP-9). SYSTIM is disciplined, so the NRR measured on SYSTIM tends to 1. Both values are reported:
- `nrr_systim`;
- `nrr_free = nrr_systim × (1 + applied_ppb·1e−9)`, which is comparable to Phase 0b's −83 ppm.

### 7.3 Sync, servo, lock (PTP-3, PTP-4, PTP-7)

Port state (`avb_gptp::port`, sans-IO; every edge logged with its reason and counted):
- Announce interval `Ta = 2^logMessageInterval` of the most recent accepted Announce. Valid range [−3, 4]; an Announce with a value outside it (including 0x7F) is still accepted for its content, but Ta keeps its previous value and `announce_interval_invalid` is counted. Placeholder Ta = 1 s before the first accepted Announce. `announceReceiptTimeout = 3·Ta`, restarted by every accepted Announce.
- An Announce is accepted only from a remote portIdentity (not our clockIdentity), on domain 0, with a path trace that doesn't contain our clockIdentity; others are counted `announce_rejected`.
- `Listening` (initial state) → `Slave`: on an accepted Announce. GM identity, priority1/2, clockClass and the path trace are stored, and ADP is re-advertised (§9.3). The servo edge depends on the GM identity compared with `last_gm` (the GM identity of the most recent Slave period, `None` at daemon start):
  - `last_gm == None` (first Slave since start): servo stays in / enters ACQUIRE.
  - same GM identity: servo HOLDOVER → TRACKING on the next Sync accepted with asCapable true (HOLDOVER row below; the step rule applies to that Sync).
  - different GM identity: `gm_changes += 1`, servo → ACQUIRE.
- `Slave` → `Slave` with a different GM identity in an accepted Announce (GM change without a timeout): `gm_changes += 1`, GM data replaced, ADP re-advertised, servo → ACQUIRE.
- `Slave` → `Listening`: when announceReceiptTimeout expires, **or** asCapable has been continuously false for longer than announceReceiptTimeout. Counted `announce_timeouts` (or `ascapable_slave_loss`); the servo goes to HOLDOVER (f and i frozen, SYSTIM keeps free-running at the last f) if it was in TRACKING or LOCKED, and stays in ACQUIRE if it hadn't left ACQUIRE yet; Pdelay (initiator and responder) keeps running in Listening, so asCapable stays meaningful; the advertised GM identity is set back to our own clockIdentity and ADP is re-advertised; `last_gm` keeps the old GM identity for the return edge above. The `GptpMachModel` stays published with `valid = false` (§7.5).
- `Listening` never runs the servo; Syncs received in Listening are counted `sync_while_listening` and ignored.
This covers the 16R-reboot case (ADC-13, §9.4 reboot rows): its Announce stops, we drop to Listening and HOLDOVER within 3·Ta, and on return the same GM identity resumes TRACKING without an ACQUIRE (a reboot that re-initializes the 16R's clock appears as a > 100 µs offset and is handled by the step rule).
- Sync interval: `Ts = 2^logMessageInterval` of the most recent Sync from the master. Values outside [−5, 1] (including 0x7F) are **rejected**: the previous Ts stays and `sync_interval_invalid` is counted (the Sync itself is still used for the offset). Before the first valid Sync, Ts = 0.125 s is only a placeholder for the timeout below. A change of Ts is counted `sync_interval_changes`, logged, and recomputes the gains below without resetting the servo state. Nothing assumes the 16R's interval (U-11).
- Sync is accepted only from the Slave port's master portIdentity, with a Follow_Up of the same sequence within 3·Ts.

Offset per Sync:
```
rateRatio  = (1 + cumulativeScaledRateOffset · 2^-41) · NRR_systim
syncEvent  = preciseOriginTimestamp
           + (FollowUp.correctionField + Sync.correctionField) · 2^-16   // ns; both Scaled ns (i64)
           + delay_comp · rateRatio / NRR_systim
           // Sync.correctionField is expected to be 0 in two-step 802.1AS (802.1AS-2020 §11.4.2 / §11.2.15 *(verify clause)*);
           // it is added, not assumed, and pinned by a vector with a non-zero Sync correctionField
offset_ns  = t_rx_c(Sync) − syncEvent           // positive: our SYSTIM is ahead
```

Servo (`avb_gptp::servo`, one update per Sync, period Ts from the Sync logMessageInterval):

| State | Entered from | Action per Sync | Exit |
|---|---|---|---|
| ACQUIRE | daemon start and GM change **only** | Wait for asCapable and 2 Syncs. `f = f_applied + (rateRatio−1)·1e9` | if \|offset\| > 1 s: `SetClock(syncEvent + elapsed)`; else if \|offset\| > 100 µs: `StepClock(−offset)`; `AdjFreq(f)`; `f_base := f`; `i := 0`; → TRACKING. Every step is counted |
| TRACKING | ACQUIRE, LOCKED, HOLDOVER | **Step rule** first: if \|offset\| > 100 µs → `StepClock(−offset)` (or `SetClock` if > 1 s), count, `i := 0`, stay TRACKING, skip the PI this Sync. Else PI: `u = Kp·offset + i_candidate` with `i_candidate = i + Ki·offset`; `f = f_base − u`; if \|f\| would exceed 488 000 ppb, clamp, count `saturations` and **don't** commit `i_candidate` (anti-windup: the integrator is frozen while clamped); otherwise `i := i_candidate` | 16 consecutive \|offset\| < 1 µs → LOCKED |
| LOCKED | TRACKING | Same step rule (a step → TRACKING, counted `unlock`) and same PI | 4 consecutive \|offset\| > 10 µs → TRACKING (counted `unlock`) |
| HOLDOVER | TRACKING/LOCKED when no Sync for 3·Ts, asCapable false, or port Slave → Listening | freeze f and i; Syncs that arrive while asCapable is false or the port is Listening are counted `sync_in_holdover` and not used | a Sync accepted (port Slave, current step generation, not ambiguous) **and** asCapable true → TRACKING; the step rule applies to that first Sync |

Symbols:
- `f_base` is the frequency (ppb) applied by `AdjFreq` on leaving ACQUIRE. It stays fixed until the next ACQUIRE.
- `elapsed` = (`ReadClock` SYSTIM now) − the Sync's `t_rx_c`, i.e. the SYSTIM time that passed between the Sync's reception and the `SetClock` call.
- `i` is the PI integrator (ppb), reset to 0 in ACQUIRE.

Constants, recomputed whenever Ts changes:
- Kp = 0.2 / Ts ppb/ns and Ki = 0.02 / Ts ppb/ns per update (1.6 and 0.16 at Ts = 0.125 s). These are the starting values.
- A host simulation test pins them at Ts ∈ {0.03125, 0.125, 1} s: a GM at −83 ppm and 10 ns RMS timestamp noise must lock within 10 s (within 30 s at Ts = 1 s, i.e. 30 updates), with steady-state |mean| < 5 ns and sd < 20 ns. A fourth case changes Ts mid-run from 0.125 to 0.03125 s and must not unlock.
- If hardware shows otherwise, the hardware step tunes them and records the values.

Step bookkeeping (`avb_gptp::stepgen`, sans-IO; rev 5: the classification comes from the dext, §3.6, and no longer from daemon-side mach windows, so it doesn't depend on timer latency):
- The current generation `gen` (u32) is `hdr.clock_gen`, acquire-loaded once after mapping (§5.2) and again immediately after every `StepClock`/`SetClock` call returns. Those selectors are owner-only (§3.2), so while the daemon owns the rings no other client can change `clock_gen`, and the cached value is exact between calls. `AdjFreq` doesn't change it.
- Generation of an **RX** event timestamp (Sync `t_rx`, Pdelay_Resp `t4`, the 16R's Pdelay_Req `t2`), from the slot alone:
  - `flags & AVB_RXF_TS_STEP_AMBIG` → **AMBIGUOUS**: the frame is not used for timing and `rx_ts_ambiguous_step` is counted. An ambiguous Pdelay_Req from the 16R is still answered with Pdelay_Resp, but its Pdelay_Resp_Follow_Up is skipped exactly as for `pdelay_resp_step_skip`. An ambiguous Pdelay_Resp/Follow_Up makes our exchange incomplete (not counted as lost).
  - otherwise the generation is `slot.clock_gen`. The pure function is `classify_rx_gen(slot_gen, ambig) -> RxGen::{Gen(u32), Ambiguous}`; comparisons with `gen` are equality only (u32 wrap is irrelevant).
  Any number of steps may be outstanding in the rings: each slot carries its own generation, so nothing depends on steps being far apart.
- Generation of a **TX** timestamp (t1, t3): `gen` at the moment `SendFrame` was called. To make that unambiguous, `ctl` never issues `StepClock`/`SetClock` while a `want_ts` frame is outstanding: the TX scheduler (§5.2) holds the step until the outstanding timestamp resolves or times out (≤ 20 ms, `kTxTsTimeoutNs`), and starts no new `want_ts` frame while a step is pending. The held step uses the offset computed when it was decided; 20 ms at the 488 ppm bound is < 10 µs of extra error, which the next Sync corrects.
- **Sync/Follow_Up:** a Sync whose `t_rx` generation ≠ current `gen`, or is AMBIGUOUS, is not used for the offset or the servo (counted `sync_step_skip`). This prevents a pre-step Sync processed after the step from producing a ~100 µs bogus offset and a second step (acceptance 5).
- **NRR window:** on every `gen` change the window (§7.2) is cleared. The last valid NRR is held, and used for asCapable's "NRR valid" condition, for `delay` and for `rateRatio`, until 2 complete new-generation exchanges have entered the window; then the window's own NRR replaces it. A step therefore doesn't drop asCapable. The ACQUIRE `SetClock` is treated exactly like a `StepClock` (rev 5): NRR is a ratio of differences within one generation and doesn't depend on SYSTIM's absolute value, so the last valid pre-`SetClock` NRR is held across it, and asCapable and the servo don't bounce through HOLDOVER after ACQUIRE. What is never used is an NRR computed across generations; the window clearing guarantees that, so the ACQUIRE seed `f = f_applied + (rateRatio−1)·1e9` on a later GM change always comes from a single-generation NRR (held or current). An NRR with |NRR − 1| > 1000 ppm is rejected and counted `nrr_out_of_range` (§7.2).

Exchanges straddling a step or SetClock are discarded, in both roles:
- Initiator: a Pdelay exchange whose t1 and t4 carry different step generations, or whose t4 is AMBIGUOUS, is dropped (not counted as lost).
- Responder: if a step or SetClock lands between t2 (the 16R's Pdelay_Req RX) and t3 (our Pdelay_Resp TX), the Pdelay_Resp_Follow_Up is **not** sent and `pdelay_resp_step_skip` is counted, so the 16R never sees a t3 − t2 that includes our step. The 16R counts it as one lost response; steps happen only at acquisition and on > 100 µs excursions, so this is far below allowedLostResponses.

Lock target (PTP-7): asCapable + LOCKED within **60 s** of daemon start with link up. The budget:
- ≤ 10 s link;
- ≤ 4 s for asCapable (2–3 Pdelay exchanges);
- the 16R's own BMCA and asCapable on its side (unknown; U-11);
- ≤ 10 s of servo convergence.

PTP-8 targets kept: |mean offset| ≤ 100 ns and sd ≤ 100 ns over ≥ 5 min after lock. The expectation is far better (≤ 20 ns sd) given 2.5 ns delay jitter.

### 7.4 Signaling and 802.1AS-2020 behavior (U-6)

- Received Signaling is decoded and logged: message-interval-request and the gPTP-capable TLV.
- If the 16R sends a gPTP-capable TLV, we answer at the same interval with our own Signaling carrying a gPTP-capable TLV (802.1AS-2020 §10.6.4.4 *(verify)*). That frame is in the SAF-6 TX list as "mandatory per U-6".
- A message-interval-request asking us to change our Pdelay_Req interval is honored within [−3, 3] (log2 s) and counted.
- We never request interval changes.

### 7.5 gPTP ↔ mach model (PTP-6)

- Every 125 ms `ctl` calls `ReadClock(8)` and keeps the sample with the smallest bracket: `x = (mach_before + mach_after)/2` (ns via timebase), `y = SYSTIM − offset_ns(last)`, which is gPTP time.
- Least squares over the last 64 kept samples (8 s), relative to a rebased epoch to keep f64 precision. Samples whose bracket is > 2 × the window median are rejected.
- Output `GptpMachModel { epoch_mach, epoch_gptp, slope (gptp ns per mach ns), valid, uncertainty_ns }`.
  - `uncertainty_ns = sqrt(residual_rms² + (median_bracket/2)²/3)`; Phase 0b suggests about 170 ns.
  - `valid` only in TRACKING or LOCKED.
- Published through `SeqLock`. Reset on every step or GM change.
- `gptp_ppm_vs_mach = (slope − 1)·1e6` is reported; Phase 0b predicts about −8 ppm.

### 7.6 Status (PTP-10)

`avb_gptp::status::GptpStatus` (serde) carries:
- port state, asCapable, GM identity, servo state;
- offset (last, mean, sd over the window), path delay raw and compensated (last, mean, sd);
- `nrr_systim`, `nrr_free`, cumulative rateRatio, applied ppb;
- counters: Sync/Follow_Up/Announce rx, Pdelay req tx/rx, resp tx/rx, FU tx/rx, timeouts, lost responses, TS failures, steps, saturations, GM changes, asCapable transitions (with timestamps);
- revision-4 counters: `rx_ts_ambiguous_step`, `sync_step_skip`, `nrr_out_of_range`, `announce_timeouts`, `ascapable_slave_loss`, `announce_interval_invalid`, `announce_rejected`, `sync_while_listening`, `sync_in_holdover`, plus `step_gen` and `port_state` transitions (with timestamps);
- revision-5 counters: `pdelay_resp_late_drop`, `slot_gen_invalid`.

## 8. MSRP / MVRP (`avb-srp`)

### 8.1 MRP engine

- Implementation of the IEEE 802.1Q-2018 §10.7 applicant (VO, VP, VN, AN, AA, QA, LA, AO, QO, AP, QP, LO) and registrar (IN, LV, MT) state machines, plus the LeaveAll state machine. The tables are hand-written from the standard's figures, and each transition has a unit test.
- Point-to-point operation is assumed for applicant behavior (direct link; `operPointToPointMAC = true`).
- Timers:
  - JoinTime 200 ms;
  - LeaveTime 1000 ms;
  - LeaveAllTime randomized in [10, 15] s;
  - the periodic transmission timer is disabled (802.1Q-2018 Table 10-7 *(verify)*).
- PDU codec:
  - ProtocolVersion 0;
  - Message = AttributeType, AttributeLength, AttributeListLength (MSRP only), VectorAttribute list, EndMark;
  - VectorHeader = LeaveAllEvent (3 bits) + NumberOfValues (13 bits);
  - ThreePackedEvents, plus FourPackedEvents for the MSRP Listener.
- We always encode NumberOfValues = 1 per vector (legal and simple). The decoder supports NumberOfValues > 1 with the per-attribute "FirstValue + n" increment rules (802.1Q §35.2.2.8.4–5 *(verify)*).

### 8.2 MSRP attributes (SRP-1…SRP-3, SRP-7)

| Type | Attribute | Us | Fields stored |
|---|---|---|---|
| 1 | Talker Advertise | registrar | stream_id, dest MAC, VLAN ID, MaxFrameSize, MaxIntervalFrames, priority, rank, accumulated latency (ns), first/last seen time |
| 2 | Talker Failed | registrar | the above + failure bridge id + failure code |
| 3 | Listener | applicant (+ registrar for logging) | stream_id, declaration type: Ready (2), Asking Failed (1) |
| 4 | Domain | applicant + registrar | SRclassID, SRclassPriority, SRclassVID |

Domain:
- Declare `{6, 3, 2}` at start.
- When the 16R's Domain registration for class id 6 arrives with a different priority or VID, re-declare with the 16R's values, count `domain_mismatch`, and report both value sets (SRP-1).

Listener: for each bound stream (§9.4):
- Talker Advertise registered → declare Ready.
- Talker Failed registered → declare Asking Failed (802.1Q §35.2.4.3 *(verify)*). This includes a Talker Failed that was already registered when the stream entered SETTLED_NO_RSV (for example, registered while the stream was still probing); §9.4 has the row.
- Neither, for less than `TA_WAIT = 2 s` since the stream entered SETTLED_NO_RSV → no declaration.
- Neither, after `TA_WAIT` (U-8 outcome "the 16R withholds its TA until a Listener exists"): count `ta_wait_expired`, log once per stream at warn `U-8: no Talker Advertise for <stream_id> after 2 s; declaring Listener (Asking Failed)`, and declare the Listener attribute for the effective stream_id with declaration type **Asking Failed**. A later TA registration then follows the normal rule (re-declare as Ready). The stream_id used is the one PROBE_TX (or the GET_STREAM_INFO fallback) returned, the same one the TA must carry. Basis: an end-station Listener declaration does not require a registered Talker attribute, and Asking Failed is the declaration type for a listener that cannot (yet) receive the stream (802.1Q-2018 §35.2.4.3 and §35.2.6 *(verify)*). **If the 802.1Q text forbids a Listener declaration without a registered Talker attribute**, this edge is not implemented: the implementer stops and asks the user (listed in §4's contradictions that are never adapted silently), and until then the stream stays in SETTLED_NO_RSV with `ta_wait_expired` still counted and logged.
- Withdrawn on unbind and on shutdown.

Talker Advertise is authoritative for stream addressing (AAF-2). When the TA for a bound stream_id registers (§9.4 SETTLED_NO_RSV row) or changes while registered (§9.4 `TA attributes changed` row; `avb_srp` emits `TalkerChanged { stream_id, old, new }` when a registered TA's dest MAC, VID, TSpec or accumulated latency differ from the stored values), `ctl` compares its dest MAC and VLAN ID with the values PROBE_TX (or the GET_STREAM_INFO fallback) returned:
- equal → nothing changes;
- different → count `ta_probe_mismatch`, log both value sets at warn, and switch the binding's **effective** dest MAC/VID to the TA values: `SetStreamFilter` is re-issued and the `audio` stream table is replaced. The §10.2 step 4 check compares against these effective values. A TSpec or accumulated-latency change updates the stored TA and the next `audio` stream table (the §10.3 `early_bound` uses it). In SETTLED_RSV_OK none of this touches the Listener declaration: Ready stays declared.

Before a TA exists, the effective values are the PROBE_TX values, so the filter can be set early; no audio is accepted as "bound" for the record start condition (§11.2 step 4) until SETTLED_RSV_OK, i.e. until a TA has confirmed them.

All declarations, registrations and their times go into the MSRP event log (SRP-6), which is emitted in run and record JSON.

### 8.3 MVRP (SRP-4)

- MVRP declares the VID taken from the bound streams' Talker Advertise, or from the Domain SRclassVID before any Talker Advertise exists.
- One VID attribute (type 1, length 2) per distinct VID.
- 16R MVRP registrations are logged only.

## 9. AVDECC (`avb-avdecc`)

Constants: AVTP control subtypes ADP 0xFA, AECP 0xFB, ACMP 0xFC; ADP/ACMP multicast `91:E0:F0:01:00:00`. AECP is sent unicast to the target's MAC from ADP. **Every ACMPDU we send** (CONNECT_RX/DISCONNECT_RX commands from our controller, PROBE_TX and GET_TX_STATE commands, and every response from our listener entity) goes to `91:E0:F0:01:00:00` (IEEE 1722.1 §8.2, kept by Milan); the endpoints are identified by `talker_entity_id`, `listener_entity_id` and `controller_entity_id`. No ACMP is sent unicast. Received ACMP is accepted on that address (and, defensively, on our MAC).

The command, status, descriptor-type and message-type codes come from IEEE 1722.1-2021 §7.4 / Table 7.126 and Milan v1.2. Each is a named `const` with a citation comment and a unit test.

Examples of codes this design relies on:
- AEM commands: READ_DESCRIPTOR 0x0004, GET_STREAM_INFO 0x000F, REGISTER_UNSOLICITED_NOTIFICATION 0x0024, GET_AVB_INFO 0x0027, GET_COUNTERS 0x0029, GET_AUDIO_MAP 0x002B.
- AEM status: STREAM_IS_RUNNING 12.
- Descriptor types: STREAM_OUTPUT 0x0006, CLOCK_DOMAIN 0x0024.

### 9.1 Identity (ADC-7, U-10, A-3)

- `entity_id` = EUI-64 from the I210 MAC with FF:FE inserted. It is the same value as our gPTP clockIdentity, which 1722.1 permits (both are EUI-64s of the same station). It is stable across runs.
- `entity_model_id` = `0x02_41_56` (locally administered "AV" prefix, no OUI; A-3) ‖ `0x01` (model schema revision) ‖ `fnv1a32(canonical derived model)`.
  - The canonical form is the sorted list of (STREAM_INPUT index, formats[], current_format), the sampling-rates list, and the channel counts.
  - So the model id changes iff the derived model changes, and two runs against the same 16R produce the same id (ADC-9).
- If the 16R refuses us because of either id (U-10), it is reported as a blocker. There is no workaround (A-3).

### 9.2 Controller role (ADC-1…ADC-6, ADC-14)

- **Discovery:**
  - Send ENTITY_DISCOVER (entity_id 0) at start, then every 5 s until the talker is chosen, then every 30 s.
  - Track every remote entity by entity_id with expiry at `valid_time × 2 s`.
  - Talker selection: `--talker <entity_id>` if given. Otherwise the single remote entity with `talker_stream_sources > 0`. Zero candidates after `--discover-timeout` (default 30 s) → exit 6. More than one → exit 6 with the list, asking for `--talker`.
  - Reboot detection: `available_index` lower than last seen, or DEPARTING followed by AVAILABLE. Counted and logged.
- **AECP engine:**
  - One in-flight command per target entity (1722.1 practice). Paced to ≤ 200 commands/s total.
  - Sequence ids are per controller, wrapping u16, and responses are matched on (target_entity_id, sequence_id, command_type).
  - Timeout 250 ms, then one retry. On IN_PROGRESS the timer is reset to 1 s, capped at 10 s in total.
  - A final timeout is reported per command (`timeouts[]` in the dump) and never stalls the walk (ADC-4).
- **Enumeration (ADC-2):**
  - READ_DESCRIPTOR ENTITY(0) → `configurations_count`, `current_configuration`.
  - For every configuration c: READ_DESCRIPTOR CONFIGURATION(c) → `descriptor_counts[]`. Then every (type, index) in c, with `configuration_index = c`.
  - Unknown types are stored as raw bytes with the type code.
  - Decoded types: ENTITY, CONFIGURATION, AUDIO_UNIT, STREAM_INPUT, STREAM_OUTPUT, AVB_INTERFACE, CLOCK_SOURCE, CLOCK_DOMAIN, STREAM_PORT_INPUT/OUTPUT, AUDIO_CLUSTER, AUDIO_MAP, LOCALE, STRINGS, CONTROL, JACK_INPUT/OUTPUT.
  - Limits that guard against a misbehaving peer:
    - ≤ 64 configurations;
    - ≤ 1024 descriptors per type;
    - ≤ 64 formats per stream;
    - ≤ 64 sampling rates;
    - ≤ 1024 mappings per AUDIO_MAP.

    Exceeding a limit truncates, records `quirks[]: "limit exceeded"`, and continues.
- **Dynamic state (ADC-3)** for the current configuration:
  - GET_SAMPLING_RATE (each AUDIO_UNIT);
  - GET_STREAM_FORMAT and GET_STREAM_INFO (each STREAM_OUTPUT);
  - GET_CLOCK_SOURCE (each CLOCK_DOMAIN);
  - GET_AVB_INFO and GET_AS_PATH (each AVB_INTERFACE);
  - GET_COUNTERS (AVB_INTERFACE, each STREAM_OUTPUT, each CLOCK_DOMAIN);
  - MVU GET_MILAN_INFO (vendor-unique protocol id `00-1B-C5-0A-C1-00`, command 0x0000 *(verify)*).

  NOT_IMPLEMENTED / NOT_SUPPORTED are recorded as `"status": "NOT_IMPLEMENTED"` and are not fatal.
- **Unsolicited notifications (ADC-5):**
  - REGISTER_UNSOLICITED_NOTIFICATION after the walk.
  - Unsolicited responses update the cached state, and each change (rate, format, clock source, stream info, name) is logged as `aem change: <descriptor> <field> <old> -> <new>`.
  - Re-registered after a 16R reboot.
  - We answer the 16R's CONTROLLER_AVAILABLE commands with SUCCESS. Milan entities use this to keep registrations alive.
  - A format or rate change on a bound stream marks it `format_changed`. It is not re-bound (out of scope), and `record` exits 8 if this happens before recording starts.
- **AEM dump (ADC-6)** at `/Users/dexter/AVB-Audio/.agents/tasks/phase1a-aem-16r.json`. Top-level keys:
  - `schema` (= `"avb-aem-dump/1"`), `captured_at` (UTC), `talker_entity_id`, `adp` (all ADP fields);
  - `entity` (decoded ENTITY);
  - `configurations[]`, each with `index`, `descriptor_counts`, and `descriptors{type_name: [ {index, decoded…, raw_hex} ]}`;
  - `audio_units[]` with `current_sampling_rate` and `sampling_rates[]`;
  - `streams_out[]` and `streams_in[]`, each with `name`, `current_format{raw: "0x…", subtype, format, nsr_hz, bit_depth, channels, samples_per_frame}`, `formats[]` (same shape), `channels` and `stream_info`;
  - `clock`: `sources[]` and `domain_current_source`;
  - `avb_interface` (MAC, clock identity, gPTP fields, `avb_info`, `as_path`);
  - `channel_map[]` (stream, channel, cluster index, cluster name);
  - `counters` (raw + decoded per descriptor);
  - `milan_info`, `timeouts[]`, `quirks[]`.
- **Binding (ADC-14):**
  - Candidates are the audio talker set A; what is bound is B, A minus refused streams (§4, AS-9). Non-AAF STREAM_OUTPUTs are `unbound_by_design` and never bound.
  - For every j < |B| (§4), the controller binds our STREAM_INPUT j to 16R STREAM_OUTPUT B[j]: it sends CONNECT_RX_COMMAND (talker_unique_id = the STREAM_OUTPUT index B[j], listener_unique_id = j) on the wire to `91:E0:F0:01:00:00`; software loopback delivers it to our entity.
  - Timeout 4500 ms (1722.1 Table 8.1 *(verify)*). Up to 3 attempts 1 s apart.
  - Concurrency: at most one ACMP command in flight per (listener_entity_id, listener_unique_id). Streams are bound **sequentially in j order**: CONNECT_RX for j+1 is sent only after j's final outcome (SUCCESS, or non-SUCCESS after the last attempt, or the last timeout). A retry is a new command with a new sequence_id; the previous one is no longer in flight.
  - Response matching: a received ACMPDU completes the in-flight command iff `message_type == command message_type + 1`, `controller_entity_id == ours`, `listener_entity_id` and `listener_unique_id` equal the command's, and `sequence_id` equals the command's. Anything else is counted `acmp_ctrl_unmatched` and ignored (this includes our own commands echoed back through software loopback, which have the command message_type). The controller's ACMP `sequence_id` is its own u16 counter (wrapping), separate from the AECP counter (above) and from the listener entity's PROBE_TX counter (§9.4). GET_TX_STATE_COMMAND (ADC-15 check, §9.4) uses this controller ACMP counter and the same matching rule with `talker_entity_id`/`talker_unique_id` in place of the listener ids.
  - The final non-SUCCESS status is reported per stream.
  - If |A| > 16, A[16…] are refused before binding: `16R exposes <|A|> AAF STREAM_OUTPUTs; this build supports S_max = 16; refusing <indices>`. `record`/`run` exit 8 unless `--allow-partial` is given, which binds A[0..16] and records the refusal in the summary.
  - A-5 (streams the 16R can't transmit simultaneously): if PROBE_TX, GET_TX_STATE or CONNECT_RX returns a resource status for a stream (the named const list `avb_avdecc::acmp::A5_RESOURCE_STATUSES = [TALKER_NO_BANDWIDTH, TALKER_EXCLUSIVE, TALKER_DEST_MAC_FAIL, TALKER_NO_STREAM_INDEX]`, each with a 1722.1-2021 ACMP status-table citation comment *(verify codes)*; or MSRP Talker Failed with a bandwidth/resources failure code, `avb_srp::A5_FAILURE_CODES` likewise cited), that stream is classified `not_simultaneously_streamable`, reported with the status and failure code, and recorded in the results doc as the A-5 limit. `record`/`run` exit 8 unless `--allow-partial`, which records the remaining streams.

### 9.3 Our listener entity (ADC-7…ADC-12)

The entity exists only after the 16R AEM has been read and our model derived (ADC-9). Before that it doesn't advertise, and ignores ENTITY_DISCOVER and AECP addressed to it. AECP commands to our entity_id before readiness get no answer: the entity doesn't exist yet, as far as the network knows.

ADP, re-advertised every 5 s with valid_time = 10 (20 s):
- `entity_capabilities` = AEM_SUPPORTED | CLASS_A_SUPPORTED | GPTP_SUPPORTED | AEM_IDENTIFY_CONTROL_INDEX_VALID | AEM_INTERFACE_INDEX_VALID | VENDOR_UNIQUE_SUPPORTED (for MVU);
- `talker_stream_sources` 0, `talker_capabilities` 0;
- `listener_stream_sinks` = number of STREAM_INPUTs;
- `listener_capabilities` = IMPLEMENTED | AUDIO_SINK;
- `controller_capabilities` = IMPLEMENTED;
- `gptp_grandmaster_id` and `gptp_domain_number` from the gPTP port. Until the port is Slave, `gptp_grandmaster_id` is our own clockIdentity; when the port reaches Slave (or the GM changes) the new value is re-advertised immediately (the GM-change rule below);
- `identify_control_index` 0, `interface_index` 0, `association_id` 0;
- `available_index` incremented on every ENTITY_AVAILABLE sent.

A GM change triggers an immediate re-advertisement. On shutdown we send ENTITY_DEPARTING.

Derived AEM (`avb_avdecc::derive::listener_model(&TalkerAem) -> Result<ListenerModel, DeriveError>`; configuration 0 only):

| Descriptor | Count | Key content |
|---|---|---|
| ENTITY | 1 | ids above; entity_name "AVB-Audio Mac"; firmware_version "phase1a-<git short sha>"; serial = MAC hex; 1 configuration |
| CONFIGURATION | 1 | descriptor_counts for every descriptor below |
| AUDIO_UNIT | 1 | `sampling_rates` = the 16R AUDIO_UNIT's list (copied); `current_sampling_rate` = the 16R's current; number_of_stream_input_ports = N; number_of_stream_output_ports 0 |
| STREAM_INPUT | N = \|B\| (§4) | STREAM_INPUT j corresponds to 16R STREAM_OUTPUT B[j]; j is dense, so a refused stream doesn't leave a hole (the `{j → stream_output_index}` map records the correspondence). object_name = the 16R STREAM_OUTPUT name with "→Mac " prefix, truncated to 64 B; clock_domain_index 0; stream_flags CLASS_A (+ CLOCK_SYNC_SOURCE on the reference stream §10.5); `formats` = every format of B[j] that `StreamFormat::decodable()` accepts; current_format = B[j]'s current format (decodable by construction of B; `derive` returns `DeriveError::UnsupportedCurrentFormat` for the A elements it refuses); avb_interface_index 0; buffer_length 2 000 000 ns |
| STREAM_PORT_INPUT | N | one per STREAM_INPUT; number_of_clusters = channels; number_of_maps 1 |
| AUDIO_CLUSTER | Σ channels | one per channel (channel_count 1); object_name = the 16R cluster name for that stream channel when the map resolves, else "In <i> ch <k>" |
| AUDIO_MAP | N | identity map: stream_index i, stream_channel k → cluster k |
| AVB_INTERFACE | 1 | MAC, clock_identity, priority1 255, clock_class 248, offset_scaled_log_variance 0x436A, clock_accuracy 0xFE, priority2 248, domain 0, log_sync -3, log_announce 0, log_pdelay 0, port_number 1; flags GPTP_SUPPORTED, SRP_SUPPORTED |
| CLOCK_SOURCE | 2 | 0 INTERNAL; 1 INPUT_STREAM with clock_source_location = STREAM_INPUT r, where r is the reference index chosen by `derive` with the §10.5 rule (the lowest j whose B[j] has CLOCK_SYNC_SOURCE, else 0). The same r gets CLOCK_SYNC_SOURCE in its stream_flags. Run-time `reference_switches` (§10.5) are internal to `avb-mediaclock` and don't change the AEM |
| CLOCK_DOMAIN | 1 | sources [0, 1]; current 1 |
| CONTROL | 1 | IDENTIFY (Milan-mandated *(verify)*), value 0/255; identify only logs `identify requested` |
| LOCALE + STRINGS | 1 + 1 | locale "en-US"; strings[0] "AVB-Audio", [1] "Mac listener" for vendor/model names |

Responses (ADC-10). Any command not listed returns NOT_IMPLEMENTED, never silence.

| Command | Behavior |
|---|---|
| READ_DESCRIPTOR | encoded descriptor, or NO_SUCH_DESCRIPTOR |
| ACQUIRE_ENTITY | Decided by the implementer from the Milan v1.2 text per AS-7, with a citation comment and a unit test: if Milan says PAAD entities don't support acquire (expected), NOT_IMPLEMENTED; if it mandates acquire, SUCCESS with the same controller-ownership rules as LOCK_ENTITY. The choice is recorded in the build-verification doc |
| LOCK_ENTITY | lock and unlock by controller_id; auto-release after 60 s; while locked, SET_* from others → ENTITY_LOCKED |
| ENTITY_AVAILABLE | SUCCESS |
| CONTROLLER_AVAILABLE | SUCCESS |
| GET_CONFIGURATION | 0 |
| SET_CONFIGURATION | SUCCESS if 0, else BAD_ARGUMENTS |
| GET_STREAM_FORMAT | current |
| SET_STREAM_FORMAT | bound → STREAM_IS_RUNNING (ADC-12); unbound: a value in `formats` → SUCCESS (stored); else BAD_ARGUMENTS |
| GET_STREAM_INFO | Milan extended response: flags (CONNECTED/bound, STREAM_ID_VALID, STREAM_DEST_MAC_VALID, STREAM_VLAN_ID_VALID, MSRP_ACC_LAT_VALID, MSRP_FAILURE_VALID, CLASS_A, FAST_CONNECT 0); stream_format, stream_id, msrp_accumulated_latency, stream_dest_mac, msrp_failure, stream_vlan_id; flags_ex REGISTERING (Listener declared); probing_status (DISABLED/PASSIVE/ACTIVE/COMPLETED); acmp_status *(field layout verify, Milan v1.2 §5.4.2.10)* |
| SET_STREAM_INFO | NOT_IMPLEMENTED |
| GET_NAME / SET_NAME | ENTITY entity_name/group_name, STREAM_INPUT, AUDIO_CLUSTER, CONFIGURATION names; stored in memory; SET while bound is allowed (names aren't stream state) |
| GET_SAMPLING_RATE | current |
| SET_SAMPLING_RATE | value == current → SUCCESS; any stream bound → STREAM_IS_RUNNING; else BAD_ARGUMENTS (rate change is out of scope) |
| GET_CLOCK_SOURCE / SET_CLOCK_SOURCE | index 0/1 accepted, stored, notified; no effect on reception |
| REGISTER_UNSOLICITED_NOTIFICATION / DEREGISTER | up to 16 controllers (entity_id + MAC); NO_RESOURCES above that; a registration expires when the controller's ADP entry expires |
| GET_AVB_INFO | gptp_grandmaster_id, propagation_delay (compensated ns), gptp_domain_number 0, flags AS_CAPABLE/GPTP_ENABLED/SRP_ENABLED, msrp_mappings = [{SRclassID, SRclassPriority, SRclassVID}] of the **effective** Domain from `avb_srp` (after any §8.2 adoption of the 16R's values; {6, 3, 2} only until then) |
| GET_AS_PATH | the 16R's Announce path trace + our clockIdentity |
| GET_COUNTERS | STREAM_INPUT, AVB_INTERFACE, CLOCK_DOMAIN as below. ENTITY: decided from the Milan v1.2 text per AS-7 (citation comment, unit test): if mandated, SUCCESS with valid mask 0 and zero values; otherwise NOT_IMPLEMENTED. Recorded in the build-verification doc |
| GET_AUDIO_MAP | identity map |
| MVU GET_MILAN_INFO | protocol_version 1, features_flags 0, certification_version 0 (not certified; `--milan-cert-version` overrides for interop experiments only, and any use is recorded in the results) |

Unsolicited notifications to registered controllers go out on every change to stream info (bind state, probing), format, name, clock source and counters. Counters are sent at most once per second.

GET_COUNTERS (ADC-11). Bit positions follow 1722.1-2021 Table 7.188ff *(verify)*; values are u32 wrapping.
- STREAM_INPUT, from the AAF-5 counters in `StreamCounters`:
  - MEDIA_LOCKED;
  - MEDIA_UNLOCKED;
  - STREAM_INTERRUPTED (Milan's name for STREAM_RESET);
  - SEQ_NUM_MISMATCH;
  - MEDIA_RESET (mr toggles);
  - TIMESTAMP_UNCERTAIN (tu set);
  - UNSUPPORTED_FORMAT;
  - LATE_TIMESTAMP;
  - EARLY_TIMESTAMP;
  - FRAMES_RX.
- AVB_INTERFACE:
  - LINK_UP and LINK_DOWN (dext counters);
  - FRAMES_TX and FRAMES_RX (dext TX frames / RX frames);
  - RX_CRC_ERROR (CRCERRS);
  - GPTP_GM_CHANGED.
- CLOCK_DOMAIN: LOCKED and UNLOCKED (media clock lock transitions).

Locking for the GET_COUNTERS and GET_STREAM_INFO responders: they read atomics only.

### 9.4 ACMP listener state machine (ADC-13, U-7)

Milan listener behavior (Milan v1.2 §5.5.3 *(verify state names and timers)*). The machine is `avb_avdecc::entity::acmp_listener::ListenerSm`, one instance per STREAM_INPUT, sans-IO (events in, actions out, one timer). State names and timer values are named constants with citation comments and are pinned by unit tests; if the Milan text differs, the constants change, not the table's structure.

States:
- UNBOUND: no binding stored;
- PRB_W_AVAIL: bound, talker not (or no longer) in our ADP table;
- PRB_W_DELAY: random delay 0–1 s before probing;
- PRB_W_RESP: first PROBE_TX_COMMAND sent, 200 ms;
- PRB_W_RESP2: second PROBE_TX_COMMAND sent, 200 ms;
- PRB_W_RETRY: 4 s back-off before the next probe cycle;
- SETTLED_NO_RSV: stream addressing known (PROBE_TX or fallback); no Talker Advertise yet, or Listener Ready not yet declared. Its timer is `TA_WAIT = 2 s`, armed on entry if neither a TA nor a Talker Failed is registered for the effective stream_id (§8.2). On entry the SM evaluates the current registrations once, in this order: a registered TA takes the "TA registered (or already registered on entry)" row; otherwise a registered Talker Failed takes the "Talker Failed registered (or already registered on entry)" row; otherwise TA_WAIT is armed. Entry therefore always leads to a declaration or to an armed timer;
- SETTLED_RSV_OK: Talker Advertise registered and our Listener Ready declared.

Per-binding state: (talker_entity_id, talker_unique_id, controller_entity_id, flags), `probe_cycles` (consecutive probe cycles with no response at all), the effective stream_id/dest MAC/VID, and `last_available_index` of the talker. `last_available_index` is initialized at BIND_RX from the talker's entry in the ADP table (§9.2), which tracks `available_index` for every entity, including while this SM is UNBOUND; it is unset if the talker isn't in the table yet (then the first ENTITY_AVAILABLE only sets it). After that it is set to the talker's `available_index` on **every** ENTITY_AVAILABLE received from it, in every bound state and whichever row the event takes (including `acmp_listener_ignored`), after the reboot comparison below has been evaluated. A second reboot is therefore detected against the post-first-reboot value. Entering a state cancels the previous state's timer. "Withdraw Listener" means the MSRP Listener declaration for that stream_id is withdrawn (Lv) and the stream's filter entry and audio-table entry are removed (`SetStreamFilter` re-issued, table replaced).

Transitions (exhaustive; any event not listed for a state is counted `acmp_listener_ignored{state,event}` and has no effect):

| From | Event | To | Action |
|---|---|---|---|
| UNBOUND | BIND_RX (CONNECT_RX_COMMAND), talker not in ADP | PRB_W_AVAIL | store binding; `probe_cycles := 0`; respond CONNECT_RX_RESPONSE SUCCESS |
| UNBOUND | BIND_RX, talker in ADP | PRB_W_DELAY | as above; arm random 0–1 s |
| PRB_W_AVAIL | talker ENTITY_AVAILABLE seen | PRB_W_DELAY | arm random 0–1 s |
| PRB_W_DELAY | timer | PRB_W_RESP | send PROBE_TX_COMMAND; arm 200 ms |
| PRB_W_RESP | timeout | PRB_W_RESP2 | resend PROBE_TX_COMMAND (new sequence_id); arm 200 ms |
| PRB_W_RESP / PRB_W_RESP2 | PROBE_TX_RESPONSE SUCCESS | SETTLED_NO_RSV | set provisional effective values (stream_id, dest MAC, VID); `SetStreamFilter`; send stream table to `audio`; `probe_cycles := 0` |
| PRB_W_RESP / PRB_W_RESP2 | PROBE_TX_RESPONSE non-SUCCESS | PRB_W_RETRY | arm 4 s; record the status; if it is in `A5_RESOURCE_STATUSES` (§9.2), classify the stream `not_simultaneously_streamable`. `probe_cycles` unchanged (the talker answered) |
| PRB_W_RESP2 | timeout | PRB_W_RETRY | arm 4 s; `probe_cycles += 1`; if `probe_cycles == 3`: fallback (below) → SETTLED_NO_RSV instead |
| PRB_W_RETRY | timer | PRB_W_DELAY | arm random 0–1 s |
| SETTLED_NO_RSV | TA registered (or already registered on entry) for the effective stream_id | SETTLED_RSV_OK | compare with the effective values (§8.2, `ta_probe_mismatch`; TA values become effective); declare Listener Ready |
| SETTLED_NO_RSV | TA_WAIT timer (no TA registered for the effective stream_id within 2 s of entry) | SETTLED_NO_RSV | count `ta_wait_expired`; log once per stream `U-8: no Talker Advertise for <stream_id> after 2 s; declaring Listener (Asking Failed)`; declare Listener Asking Failed (§8.2; if the 802.1Q check forbids it, the edge is absent per §4 and only the count and log happen). A later TA registration takes the row above (→ SETTLED_RSV_OK, re-declared as Ready) |
| SETTLED_NO_RSV | Talker Failed withdrawn, no TA registered | SETTLED_NO_RSV | keep the Asking Failed declaration (same result as an expired TA_WAIT); count `talker_failed_withdrawn` |
| SETTLED_RSV_OK | TA withdrawn | SETTLED_NO_RSV | withdraw Listener Ready (no declaration while there is no TA until TA_WAIT expires, §8.2); TA_WAIT armed on entry |
| SETTLED_NO_RSV / SETTLED_RSV_OK | Talker Failed registered (or, on entry to SETTLED_NO_RSV, already registered with no TA registered) for the effective stream_id | SETTLED_NO_RSV | declare Asking Failed (replacing Ready if declared); if the failure code is in `A5_FAILURE_CODES`, classify A-5; TA_WAIT not armed |
| SETTLED_RSV_OK | TA attributes changed (same stream_id; dest MAC, VID, TSpec or accumulated latency differ; `TalkerChanged`) | SETTLED_RSV_OK | apply §8.2: compare with the effective values, count `ta_probe_mismatch` if dest MAC/VID differ from the PROBE_TX values, make the new TA values effective; on a MAC/VID change re-issue `SetStreamFilter` and replace the `audio` stream table; on a TSpec/latency-only change replace the stream table only; Listener Ready stays declared (no Lv) |
| any bound state | talker ENTITY_DEPARTING or ADP entry timeout | PRB_W_AVAIL | withdraw Listener |
| any bound state | talker ENTITY_AVAILABLE with `available_index` < `last_available_index` (reboot) | PRB_W_DELAY | withdraw Listener; `probe_cycles := 0`; arm random 0–1 s (re-probe, ADC-13) |
| any bound state | BIND_RX, same talker_entity_id and talker_unique_id | unchanged | update controller_entity_id/flags; respond SUCCESS |
| any bound state | BIND_RX, different talker_entity_id or talker_unique_id | PRB_W_AVAIL or PRB_W_DELAY (as from UNBOUND) | withdraw old Listener and filter entry; store the new binding; respond SUCCESS |
| any bound state | UNBIND_RX (DISCONNECT_RX_COMMAND) | UNBOUND | withdraw Listener; respond SUCCESS |
| UNBOUND | UNBIND_RX | UNBOUND | respond NOT_CONNECTED |
| any | BIND_RX / UNBIND_RX with listener_unique_id ≥ STREAM_INPUT count | unchanged | respond LISTENER_UNKNOWN_ID |
| any state except PRB_W_RESP / PRB_W_RESP2 | PROBE_TX_RESPONSE | unchanged | count `probe_late_response`; ignore |

PROBE_TX_COMMAND and its response both go to `91:E0:F0:01:00:00` (§9); the response is matched on message_type (PROBE_TX_RESPONSE), listener_entity_id, listener_unique_id and sequence_id, where sequence_id comes from the listener entity's own wrapping u16 PROBE_TX counter (shared by all ListenerSm instances, separate from the controller's ACMP and AECP counters), and gives stream_id, stream_dest_mac and stream_vlan_id. When the Talker Advertise for that stream_id registers, its dest MAC/VID become the effective values if they differ (§8.2). A-5 classification is sticky for the run: the SM keeps retrying, a later SUCCESS is reported as `recovered_at` in the summary, and `record`/`run` apply the exit-8 rule (§9.2) at their start decision.

Probe cycle timing: one unanswered cycle is 0–1 s delay + 200 ms + 200 ms = 0.4–1.4 s, followed by the 4 s retry wait except after the third cycle, which goes straight to the fallback. The fallback therefore triggers 3 × (0.4–1.4 s) + 2 × 4 s = 9.2–12.2 s after probing starts, with total silence from the talker.

Fallback (`probe_cycles == 3`, i.e. the 16R never answered PROBE_TX):
- The listener uses the stream_id/dest MAC/VLAN that our controller already learned from the 16R's GET_STREAM_INFO on that STREAM_OUTPUT, as the provisional effective values.
- It logs `deviation: PROBE_TX unanswered, using GET_STREAM_INFO` and counts `probe_fallback`.
- It is recorded under U-7 in the results. This is the feasibility doc's debug shortcut, kept explicit.
- A later reboot or rebind resets `probe_cycles` and probes again.

GET_RX_STATE → current binding fields + connection_count (1 if SETTLED_RSV_OK, else 0) + flags.

Unit tests cover every row of the table (one test per row, named after it), the 9.2–12.2 s fallback window with a simulated clock, and the reboot and rebind rows with an active filter (asserting the old filter entry is removed).

Controller-side check (ADC-15): after binding, the controller reads the 16R's GET_STREAM_INFO and GET_TX_STATE. ACMP GET_TX_STATE_COMMAND goes to `91:E0:F0:01:00:00` with talker_entity_id = the 16R (timeout 200 ms *(verify)*). The results show whether the 16R sees a listener (connection_count, REGISTERING flags).

## 10. AVTP/AAF receive, media clock, recovered-audio ring

### 10.1 Stream format codec (`avb_avtp::stream_format`)

The AEM stream format (u64) for AAF follows 1722.1-2021 §7.3.2 *(verify)*:
- byte 0 = `v(1)=0 | subtype(7)=0x02`;
- byte 1 = `ut(1) | rsv(3) | nsr(4)`;
- byte 2 = format;
- byte 3 = bit_depth;
- then `channels_per_frame(10) | samples_per_frame(10) | rsv(12)`.

Known vectors, pinned in tests and to be replaced or confirmed with the 16R's values from M1:
- `0x0205022002006000`: 48 kHz INT_32BIT, depth 32, 8 ch, 6 spf (the Q6 format);
- `0x0205021802006000`: same with depth 24, the "24-in-32" format.

`decodable()` accepts AAF with:
- format INT_32BIT (0x02) and bit_depth 1–32;
- or INT_24BIT (0x03, packed 3 B) and bit_depth ≤ 24;
- nsr ∈ {44.1, 48, 88.2, 96, 176.4, 192 kHz};
- channels 1–64, spf 1–64;
- `ut = 0`.

Everything else decodes for the dump but isn't bindable. CRF and IEC 61883 formats are decoded for the dump only.

### 10.2 AAF parse and checks (AAF-1, AAF-2, AAF-4, AAF-5)

Per audio slot, on the `audio` thread:
1. Ethernet/VLAN parse: the tag must be present (in the frame, or via `AVB_RXF_VLAN_DESC`).
2. AVTP stream header with subtype 0x02, `sv = 1`, version 0.
3. Look up the stream by stream_id in the table (≤ 16 entries, linear scan). Not found → `stream_id_mismatch`.
4. dest MAC and VID must equal the binding's **effective** values (the Talker Advertise's once registered, else PROBE_TX's; §8.2), else `binding_mismatch`.
5. Header fields `format`, `nsr`, `channels_per_frame`, `bit_depth` and `stream_data_length == channels × spf × bytes_per_sample` must equal the bound format, else `format_mismatch` (+ UNSUPPORTED_FORMAT). No audio is written for that packet.
6. Flags: `mr` toggle → MEDIA_RESET++ and the media-clock reset (§10.5); `tv`, `tu` (TIMESTAMP_UNCERTAIN++), `sp`, `evt` noted.
7. Sequence (8-bit), with `d = (seq − expected) mod 256`:
   - 0 → ok;
   - 1–127 → gap of d packets (`seq_gap_packets += d`, `seq_gap_events += 1`, SEQ_NUM_MISMATCH++);
   - 255 → duplicate (dropped);
   - 128–254 → out-of-order (dropped).

   A gap > 127 packets is detected by the presentation-time jump (`missing = round(Δpt / packet_period) − 1`) when tv is set on both ends.

Counters per stream (AAF-5), all AtomicU64:
- packets, seq_gap_packets, seq_gap_events, duplicates, out_of_order;
- late, early, tv0, mr_toggles;
- format_mismatch, stream_id_mismatch, binding_mismatch;
- first_hw_ts, last_hw_ts;
- placement_rejects, ring_late_drops.

### 10.3 Arrival time, late and early (AAF-6)

- Arrival gPTP time:
  - with `AVB_RXF_TS_HW`: `hw_ts_ns − 448 ns − servo offset`;
  - otherwise: harvest mach → gPTP via `GptpMachModel`, with `arrival_ts_fallback` counted (AS-5).
- `pt = ext32(avtp_timestamp, arrival)` takes the 64-bit value within ±2^31 ns of arrival, with wrap handling.
- Late: `pt < arrival`.
- Early: `pt − arrival > early_bound`, where `early_bound = max(TA.accumulated_latency_ns, 2 000 000) + 125 000` ns. That is the larger of the MSRP-advertised latency and the Milan 2 ms default presentation offset, plus one class A interval (U-9).
- A histogram of `pt − arrival` (64 buckets of 50 µs) per stream goes into the summary. It resolves U-9 (the actual presentation offset).

### 10.4 Latency target (NFR-2) and CPU (NFR-1)

Measurement: per packet, `ring_commit_mach − arrival_mach`, with arrival from the hardware timestamp mapped through the model. Kept as a 1 µs-resolution histogram; p50/p99/max reported.

Targets:
- p50 ≤ 0.75 ms and p99 ≤ 1.5 ms: the 500 µs dext tick plus the 250 µs audio poll plus commit;
- max reported, expected < 10 ms.

That leaves ≥ 0.5 ms inside the Milan 2 ms presentation offset. If the hardware run misses p99, the documented fallback is MSI with EITR moderation at 250 µs. It is not built in Phase 1a, and recording correctness doesn't depend on it.

NFR-1 budget ≤ 15 % of one core is kept. The estimate is ≤ 5 %: about 0.3 µs per packet at 128 k pkt/s, plus 4000 wakeups/s. Measured with `getrusage` deltas, reported in the summary.

### 10.5 Media-clock recovery (`avb-mediaclock`; MCR-1…MCR-5)

**Rate-derived constants.** Every threshold below is defined as a duration and converted with the bound model's `rate` (AUDIO_UNIT current rate, Hz) and `spf` (samples per frame of the reference stream's format), both from `ListenerModel`, never as a packet or frame literal (acceptance 14):

```
const LOCK_HOLD: Duration = 1 s;     lock_packets    = ceil(LOCK_HOLD · rate / spf)
const LAG_GAP:   Duration = 4 ms;    lag_frames      = ceil(LAG_GAP · rate)
const SILENT_FR: Duration = 4 ms;    silent_lag      = ceil(SILENT_FR · rate)     (§10.6 all-silent rule)
const RING_SPAN: Duration = 1.25 s;  capacity_frames = next_pow2(ceil(RING_SPAN · rate))
const MODEL_PUB: Duration = 1 ms;    publish_packets = max(1, ceil(MODEL_PUB · rate / spf))
Tu = spf / rate (s)                  packet period, DLL ω
P_init = spf · 1e9 / rate (ns)
```

At 48 kHz / 6 spf these give 8000 packets, 192 frames, 192 frames, 65 536 frames, 8 packets and 125 µs; at 96 kHz / 12 spf they give 8000, 384, 384, 131 072, 8 and 125 µs.

**Reference stream:**
- The lowest-index bound stream whose 16R STREAM_OUTPUT is in the 16R's current CLOCK_DOMAIN (all of them, AS-2).
- A stream whose stream_flags has CLOCK_SYNC_SOURCE is preferred. The initial reference is exactly the index r that `derive` put into CLOCK_SOURCE 1's location (§9.3); `derive` and `avb-mediaclock` call the same `reference_index(&ListenerModel)` function.
- If the reference is interrupted > 100 ms, the next active stream takes over without a model reset (same media clock), counted `reference_switches`.
- Why: every 16R stream carries timestamps from the same media clock, so any one is a valid reference, and a deterministic choice keeps runs comparable.

**DLL** in the gPTP domain, per packet with tv = 1. f64 times are relative to an epoch E (u64 ns) that is rebased every 60 s.
```
init (first tv packet):  P = spf · 1e9 / rate_AEM  (ns per packet);  t0 = pt;  t1 = pt + P
per packet k packets after the previous update (k = 1 normally, >1 after gaps/tv=0):
    t1 += (k − 1) · P            // advance over missing / unstamped packets
    e   = pt − t1
    t0  = t1;  t1 += b·e + P;  P += c·e
ω = 2π·B·Tu, B = 1 Hz, Tu = spf / rate (packet period, from the model) → b = √2·ω, c = ω²
```
- Sample period `T = P / spf`.
- Sample index: the reference stream's packet counter × spf, advanced by gap counts, so loss leaves no discontinuity (MCR-2).

**Lock state machine:**
- LOCKED after |e| < 1 µs for `lock_packets` consecutive packets (LOCK_HOLD = 1 s).
- UNLOCKED on 8 consecutive |e| > 10 µs, on an mr toggle, or after > 10 ms without packets.
- The CLOCK_DOMAIN LOCKED/UNLOCKED and STREAM_INPUT MEDIA_LOCKED/MEDIA_UNLOCKED/STREAM_INTERRUPTED counters follow these transitions.
- An mr toggle resets the DLL to init and increments `reset_count`.
- tv = 0 packets advance k only.
- Sparse mode (sp = 1, tv on every 8th packet) works unchanged.

**Mapping to mach:** `mach(n) = M(t0 + (n − n0)·T)`, where M is the `GptpMachModel` inverse and `n0` is the sample-frame index of the DLL's `t0`: the reference stream's packet counter × spf (advanced by gap counts) at the last DLL update. It is published as `{anchor_frame, anchor_mach (f64 ticks), mach_per_frame (f64), rate_hz_vs_mach, media_ppm_vs_gptp, uncertainty_ns, locked, reset_count}` in the ring header seqlock (MCR-4). The update rate is every `publish_packets` reference-stream packets (MODEL_PUB = 1 ms).

**Stats (MCR-3):**
- 10 s windows. The window rate vs mach is computed independently of the DLL from the window's endpoint pairs (sample index, mach of pt).
- Reported: rate ppm vs the nominal AEM rate per window, sd across windows, and RMS of e per window as residual jitter.
- Target: sd of the 10 s windows ≤ 0.5 ppm. The expectation is ≤ 0.05 ppm, since model uncertainty (about 170 ns over 10 s) is about 0.02 ppm.

**MCR-5:**
- When the 16R CLOCK_DOMAIN source is PTP: `|media_ppm_vs_gptp| ≤ 0.1 ppm`, equivalently `media_ppm_vs_mach − gptp_ppm_vs_mach`. A larger value is a finding.
- When it isn't PTP: "not applicable" plus the source type.

### 10.6 Recovered-audio ring ABI (`avb-audioring`; RING-1…RING-4)

Layout v1, little-endian, position-independent. Header region 16 384 B, data at `data_offset`:

| Offset | Field | Notes |
|---|---|---|
| 0 | magic u32 = 0x52415641 ("AVAR") | |
| 4 | version u32 = 1 | |
| 8 | header_size u32 = 16384 | |
| 12 | channels u32 | 1–1024 (16 streams × 64 ch) |
| 16 | sample_format u32 = 1 | S32LE container |
| 20 | valid_bits u32 | `max(bit_depth)` over the bound set B (from the AEM formats); each stream's own depth is in `streams[i].bit_depth` |
| 24 | nominal_rate_hz u32 | from the AEM |
| 28 | capacity_frames u32 | power of two, `next_pow2(ceil(RING_SPAN · rate))` with RING_SPAN = 1.25 s (§10.5); 65 536 at 48 kHz |
| 32 | frame_bytes u32 = channels × 4 | |
| 36 | stream_count u32 | |
| 40 | data_offset u32 = 16384 | |
| 44 | reserved u32 | |
| 48 | config_generation u64 | bumped on any layout/config change |
| 128 | write_frame u64 (AtomicU64) | frames [0, write_frame) committed; absolute frame index; producer release-stores |
| 256 | read_frame u64 (AtomicU64) | consumer release-stores |
| 384 | clock block (128 B) | seq u32, pad u32, anchor_frame u64, anchor_mach f64, mach_per_frame f64, rate_hz_vs_mach f64, media_ppm_vs_gptp f64, uncertainty_ns f64, locked u32, reset_count u32. Every field is accessed only atomically (rev 5): `seq`, `locked`, `reset_count` as `AtomicU32`; the u64 and f64 fields as `AtomicU64` (f64 via `to_bits`/`from_bits`). Writer (`audio`): `seq` odd (Relaxed) → `fence(Release)` → field stores (Relaxed) → `seq` even (Release). Reader (`rec`, Phase 1b): `seq` (Acquire), retry while odd → field loads (Relaxed) → `fence(Acquire)` → `seq` re-check (Relaxed), retry on change (the §5.1 `SeqLock` protocol and retry bound). A C/C++ reader in Phase 1b uses `__atomic_load_n` on the same fields |
| 512 | counters [64] u64 | overrun_frames, overrun_events, consumer_skips, gap_frames_total, late_drops, placement_rejects, zero_fill_frames, … |
| 1024 | streams [16] × 64 B | +0 stream_id u64, +8 first_channel u32, +12 channels u32, +16 gap_frames u64, +24 gap_events u64, +32 last_frame u64, +40 active u32, +44 bit_depth u32, +48 pad [16] |
| 2048 | channel_map [1024] × 8 B (ends 10 240) | stream_index u16, stream_channel u16, reserved u32 |
| 10 240 | gap log: head u64 + 128 × 32 B entries (ends 14 344) | start_frame u64, frames u32, stream u16, reason u16, reserved |
| 14 344 | reserved to 16 384 | |

`size_of::<RingHeader>() == 16384` and every offset above is asserted in Rust tests (RING-2). Data is `capacity_frames × frame_bytes`, interleaved, channel order = stream order (our STREAM_INPUT index j, which maps to 16R STREAM_OUTPUT B[j], §4) × channel within stream (RING-4). At 64 ch and 48 kHz the data is 16 MiB.

Placement (AAF-7): for a packet of stream s with presentation pt:
- `n = n0 + round((pt − t0)/T)` using the current DLL state;
- frame `f = n − n_base`, where `n_base` is fixed at the first reference packet.

For the reference stream itself, `n` must equal its counter; a mismatch counts `placement_slip`, expected 0.

Rules:
- `f < write_frame` (already committed) → `late_drops`.
- `f > write_frame + capacity/2` → `placement_rejects`.
- Otherwise the samples are converted:
  - INT_32BIT: big-endian i32 to native, a bit-exact copy; depth < 32 stays MSB-aligned;
  - INT_24BIT: 3 B big-endian to `i32 << 8`.

  They are stored at frames f…f+spf−1, channels first_channel…+channels−1.

Before any write beyond `zeroed_upto`, the producer zeroes `[zeroed_upto, f+spf)`. Missing data is therefore zero.

Commit (frontier): `write_frame` advances to the minimum over active streams of `stream_written_upto`. Exceptions:
- **Lagging stream:** a stream more than `lag_frames` (LAG_GAP = 4 ms, §10.5) behind the furthest stream is treated as gapped. The frontier passes it, its span stays zero, and `gap_frames`/`gap_events` and a gap-log entry record it.
- **All streams silent:** if all streams are silent for > 20 ms, the frontier advances from the media-clock model and mach now, minus `silent_lag` frames (4 ms). The ring keeps real time and the zero-filled span is logged.

Release-store `write_frame` after the data writes.

The producer never blocks (RING-1). If `write_frame − read_frame > capacity`, it still commits and counts `overrun_frames`. The consumer detects `read_frame < write_frame − capacity`, skips to `write_frame − capacity`, and counts `consumer_skips`. Underrun is simply "nothing to read" and is counted by the consumer when it asked for more than was available.

Constructors:
- `AudioRing::new_anonymous(config)`: `libc::mmap(MAP_ANON|MAP_SHARED)`, used in Phase 1a.
- `AudioRing::from_raw(ptr, len)`: validates magic, version and sizes; this is what Phase 1b uses on its shared mapping.

`Producer` and `Consumer` are the only access paths. `record` uses `Consumer` exactly like Phase 1b will (RING-2).

## 11. CLI, record, WAV (CLI-1…CLI-5)

### 11.1 Subcommands

| Command | Runs | Output |
|---|---|---|
| `avb-daemon run [--seconds N] [--talker ID] [--allow-partial] [--status-interval S] [--json-out P] [--capture P.pcap]` | full stack until SIGINT or N s | status lines; JSON summary on exit; with `--capture`, a pcap of the first 4096 audio slots and up to 4096 control slots (§4) |
| `avb-daemon aem-dump [--out P] [--talker ID] [--discover-timeout S]` | data path + ADP + AECP walk only | AEM JSON (default `/Users/dexter/AVB-Audio/.agents/tasks/phase1a-aem-16r.json`) |
| `avb-daemon gptp --seconds N [--json-out P] [--discover-timeout S]` | data path + gPTP only (no SRP/AVDECC); exit 6 if no gPTP peer within S (below) | live offset/delay lines every 1 s; summary JSON (mean/sd offset after lock, delays, NRR raw/free, timeline, counters, turnaround) |
| `avb-daemon streams --seconds N [--interval S] [--json-out P]` | full stack | per-stream counter table each interval + summary |
| `avb-daemon record --seconds N --out P [--talker ID] [--allow-partial] [--start-timeout S] [--summary-out P]` | full stack + recorder | WAV, `<P>.channels.json`, `<P>.summary.json` (default) |
| `avb-daemon capture --seconds N --out P` | data path only, no TX, no stream filter: control ring only | pcap + `<P>.slots.json` |

`gptp` and "16R not found" (CLI-1): `gptp` runs no AVDECC, so `--discover-timeout` means the gPTP peer here. If, within `--discover-timeout` (default 30 s) after link up, neither an Announce from a remote portIdentity is accepted nor one Pdelay exchange (either direction) completes with a remote portIdentity, `gptp` exits 6 with `no gPTP grandmaster seen within <S> s (16R connected and powered?)`. Either event satisfies the check, so a 16R that answers Pdelay but sends no Announce still runs to `--seconds` and reports it (U-11). Not reaching LOCKED within the PTP-7 budget doesn't change the exit code: the summary carries `locked: false` and `lock_time_s: null`. For the other subcommands `--discover-timeout` keeps its ADP meaning (§9.2).

Common flags:
- `--discover-timeout` (default 30 s; ADP talker for AVDECC subcommands, gPTP peer for `gptp`);
- `--link-timeout` (10 s);
- `--log-level error|warn|info|debug` (info).

Logs go to stderr, prefixed `avb-daemon[<subsys>]:` (subsys ∈ dext, gptp, srp, adp, aecp, acmp, entity, aaf, mclk, ring, rec, main; NFR-4). They are rate-limited to 1 line per key per 10 s for repetitive conditions. Nothing is logged on the audio per-packet path; it only increments counters (NFR-3).

`record` decision: it runs the whole stack in-process. There is no "attach to a running stack", because that needs the Phase 1b IPC.

### 11.2 Record sequence

1. Validate arguments (§13.2).
2. Disk check: `statvfs(dirname(P))` free space ≥ `projected + 64 MiB`, where `projected = 104 + seconds·rate·channels·4` (header: RIFF 12 + JUNK 36 + fmt 48 + data 8 = 104 B). Otherwise exit 9 before touching the network. Rate and channels are known only after the AEM walk, so the check runs right after derivation and before binding.
3. Bring up the stack (§5.2), discover the talker, walk the AEM, derive, advertise, bind all streams.
4. Wait until every bound stream is SETTLED_RSV_OK, receiving, and the media clock is LOCKED, within `--start-timeout` (default 30 s). Otherwise exit 8, naming the failing streams and their states. A stream in SETTLED_NO_RSV whose `ta_wait_expired` is non-zero is named as `SETTLED_NO_RSV (ta_wait_expired: no Talker Advertise; U-8)`, and the message also prints whether our Listener Asking Failed declaration was sent.
5. Start frame `F0 = write_frame` at that moment. Record exactly `seconds × rate` frames. Design tolerance: 0 frames, stricter than acceptance 9's "± 1 frame-period".
6. Finalize and print the summary.

The `rec` thread reads in chunks of ≤ `ceil(0.1 s · rate)` frames (100 ms) through `Consumer`, converts nothing (the ring is already S32LE), and writes through a 1 MiB `BufWriter`.

### 11.3 WAV format (CLI-3) and sidecars (CLI-4)

- WAVE_FORMAT_EXTENSIBLE `fmt ` (40 B):
  - wFormatTag 0xFFFE, nChannels = ring channels, nSamplesPerSec = rate, wBitsPerSample 32, nBlockAlign = 4·channels, nAvgBytesPerSec;
  - cbSize 22, wValidBitsPerSample = ring valid_bits (`max(bit_depth)` over B; 32, or 24 when every stream is 24-in-32). Samples of a shallower stream stay MSB-aligned in the 32-bit container, so they're bit-exact under the wider valid_bits, dwChannelMask 0, SubFormat KSDATAFORMAT_SUBTYPE_PCM `00000001-0000-0010-8000-00aa00389b71`.
- RF64 (D11). The file is written as `RIFF` + `JUNK`(28 B) + `fmt ` + `data`. On finalize:
  - riff size ≤ 0xFFFFFFFF → patch RIFF and data sizes (standard WAV);
  - otherwise → rewrite the first 4 B as `RF64`, set the RIFF and data sizes to 0xFFFFFFFF, and replace `JUNK` with `ds64` {riffSize u64, dataSize u64, sampleCount u64, tableLength 0} (EBU Tech 3306).

  A crash leaves a RIFF with zero sizes, which is detectable, not misparsed.
- Byte order: samples are written little-endian from the ring. The ring holds bit-exact copies of the network big-endian values (§10.6). A unit test feeds synthetic AAF payloads (`0x80000000`, `0x7FFFFFFF`, `0x00000001`, `0x12345678`, 24-bit packed `0x800000`) through parse → ring → WAV and compares bytes.
- `<P>.channels.json`: `[{wav_channel, stream_index, stream_id, stream_channel, bit_depth, name}]`, with `bit_depth` the stream's AEM depth and the name from the AEM cluster or the fallback.
- `<P>.summary.json` contains:
  - per stream: AAF-5 counters, measured rate and ppm vs nominal, late/early, the pt−arrival histogram and the latency histogram percentiles;
  - the dext counters (DP-8) before and after;
  - the gPTP summary;
  - media-clock windows, ppm, sd, jitter and the MCR-5 verdict;
  - ring counters, WAV size and duration in frames. The hardware step adds the file checksum with `/usr/bin/shasum -a 256` (VER-4); the daemon doesn't hash.
  - CPU usage and exit status.

### 11.4 Exit codes

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | internal error (bug, mmap/map failure, unexpected IOKit error) |
| 2 | usage (clap) |
| 3 | dext service `AVBI210Driver` not found |
| 4 | ABI or shared-memory layout mismatch (both versions printed) |
| 5 | no link at start, or link lost |
| 6 | talker not found / ambiguous / ENTITY descriptor unreadable; for `gptp`, no gPTP grandmaster or Pdelay peer seen within `--discover-timeout` |
| 7 | dext fault latched, or device removed |
| 8 | an audio (AAF) stream refused or failed, without `--allow-partial`: > S_max, undecodable AAF current format, ACMP failure after retries, not simultaneously streamable (A-5), start timeout, format change before start. Non-AAF outputs (`unbound_by_design`) never cause exit 8 |
| 9 | output path or disk-space error, WAV write error |
| 10 | data path owned by another client |
| 11 | local data loss during record (ring overrun / consumer skips > 0) |
| 130 | interrupted by SIGINT/SIGTERM before `record` completed (WAV is valid and shorter) |

`run`/`streams`/`gptp` interrupted by a signal exit 0 after a clean shutdown.

## 12. Talker-ready boundary and listener-only scope (CRT-6)

Built in Phase 1a: RX of everything in DP-4; TX of the control frames in §3.5 only. No AVTP stream TX, no CRF, no Qav, no LaunchTime, no second TX queue.

Where talker support attaches later; nothing in Phase 1a blocks any of these:

| Layer | Talker hook | Phase 1a state |
|---|---|---|
| Dext ABI | selectors 32–47; memory type 3 (talker audio TX ring); TX queue 1 + TQAVCTRL/Qav credit registers + LaunchTime context descriptors | reserved numbers only |
| Shared memory | the slot format is direction-agnostic; a TX ring reuses `AvbRxSlot`-like 2048 B slots with a launch-time field in the pad | — |
| `avb-srp` | `Participant::declare(Attribute::TalkerAdvertise{..})` uses the same applicant path as Listener | the API takes any attribute type; tests cover encoding TA (needed for decode vectors anyway) |
| `avb-avdecc` | `aem::model` supports STREAM_OUTPUT/STREAM_PORT_OUTPUT descriptors (used to decode the 16R); `entity` has a `TalkerSm` slot in the ACMP dispatcher returning NOT_SUPPORTED today | our model has 0 STREAM_OUTPUTs; ADP talker fields 0 |
| `avb-avtp` | `aaf::encode` exists for test vectors only | not called by the daemon |
| `avb-audioring` | the same layout serves an output ring (producer = CoreAudio, consumer = talker) | one input ring |
| gPTP | `Port` keeps a role field; a master/Announce TX path would be added there | slave-only, no Announce |

## 13. Error handling, input validation, invariant ownership

### 13.1 Operations that can fail

| Operation | Failure | Recoverable? | Caller gets | Logged |
|---|---|---|---|---|
| Dext `RingAttach` | alloc failure | no (for this run) | `kIOReturnNoMemory`; daemon exit 1 | dext once (`AVBI210: rings: alloc failed`), daemon error |
| | another owner | no | `kIOReturnExclusiveAccess`; exit 10 | daemon error |
| | re-attach after detach on the same connection | no (client bug) | `kIOReturnNotPermitted`; daemon exit 1 (cannot happen with the §5.2/§5.3 sequence) | dext once, daemon error |
| | stage < 1 / fault | no | `kIOReturnNotReady`; daemon reads info: exit 7 | daemon error with fault name |
| Dext `CopyRingMemory` | not attached / not owner / bad type | no | NotReady / ExclusiveAccess / BadArgument; exit 1 | daemon error |
| Dext `RingDetach` | not attached / not owner | no (daemon bug) | NotReady / ExclusiveAccess; daemon logs and continues shutdown | daemon error |
| Dext RX chunk DMA prep (16 × 256 KiB) | > 1 segment or alloc failure | no | `kAvbFaultDma` latched; InitDatapath fails; exit 7 | dext FAULT line |
| Harvester stalled | `last_tick_mach` older than 50 ms while `producer_running` | depends: fault or stage ≠ 2 → no; otherwise yes | GetInfo; exit 7, or `harvest_stall`++ and continue | daemon error / warn ≤ 1 / 10 s |
| Shm generation | header `generation` ≠ `RingAttach` result | no | exit 4 | error with both values |
| Harvest | invalid consumer tail | yes (each tick) | ring treated full; ConsumerIndexInvalid++ | none per tick (counter) |
| | ring full | yes | drop-newest; RingOverrun++ | none (counter) |
| | all-ones read | no | fault latched, producer stops, header.fault set; daemon exit 7 | dext FAULT line |
| `SendFrame` | TS pending | yes | `kIOReturnBusy`; frame retried next loop | counter |
| | token bucket empty | yes | `kIOReturnNoResources`; retried | daemon warn, rate-limited |
| | bad length | no (bug) | `kIOReturnBadArgument`; frame dropped | daemon error |
| | NotReady / IOError | no | daemon checks fault → exit 7 | daemon error |
| TX queue push (§5.2) | class sub-queue full | yes | `Err(TxFull)`; new frame dropped; `tx_queue_full[class]`++; caller behavior per class (§5.2) | warn ≤ 1 / 10 s per class |
| `GetTxTimestamp` | timeout | yes | status Timeout; exchange abandoned, counted | gptp warn, rate-limited |
| `AdjFreq` | > limit | prevented (servo clamps) | BadArgument would be a bug → exit 1 | error |
| `StepClock` | \|Δ\| > 1 s or out of range | yes | BadArgument → servo uses `SetClock` | info |
| Map / unmap | IOKit error | no | exit 1 | error |
| Shm header validation | mismatch | no | exit 4 | error with both values |
| Link | down at start (10 s) | no | exit 5 | error |
| | lost while running | no | shutdown, exit 5 | error |
| `gptp` peer discovery | no remote Announce and no completed Pdelay within `--discover-timeout` | no | exit 6 | error with the timeout |
| `gptp` lock | not LOCKED within the PTP-7 budget | yes (run continues) | `summary.locked = false`, `lock_time_s = null`; exit code unaffected | warn once |
| Device removal | IOKit NoDevice / NotResponding / invalid port | no | shutdown (no HW ops), exit 7 | error |
| Consumer slot index mismatch | desync | yes | tail jumps to head; `consumer_desync`++ | error ≤ 1 / 10 s |
| Frame parse (any protocol) | malformed / short / unknown | yes | dropped, per-reason counter | debug summary every 10 s |
| AECP command | timeout after retry | yes | recorded in `timeouts[]`; walk continues | warn |
| | ENTITY descriptor unreadable | no | exit 6 | error |
| ACMP CONNECT_RX | non-SUCCESS ×3 | no for that stream | stream failed; record/run exit 8 unless `--allow-partial` | error with status name |
| PROBE_TX | unanswered 3 cycles | yes (fallback, §9.4) | GET_STREAM_INFO values used | warn + deviation counter |
| ACMP / MSRP resource status (A-5) | a status in `A5_RESOURCE_STATUSES` (TALKER_NO_BANDWIDTH, TALKER_EXCLUSIVE, TALKER_DEST_MAC_FAIL, TALKER_NO_STREAM_INDEX), or Talker Failed with a code in `A5_FAILURE_CODES` | no for that stream | `not_simultaneously_streamable`; exit 8 unless `--allow-partial` | error with status / failure code |
| Talker Advertise vs PROBE_TX | dest MAC or VID differ | yes | TA values become effective; filter and audio table updated; `ta_probe_mismatch`++ | warn with both value sets |
| Non-AAF STREAM_OUTPUT | CRF / 61883 / vendor current format | yes (by design) | listed `unbound_by_design`; not bound; no exit | info once |
| MSRP | Talker Failed | yes (waits) | Listener Asking Failed; start timeout → exit 8 | warn with failure code |
| MSRP | no TA within TA_WAIT (U-8) | yes (waits) | `ta_wait_expired`++; Listener Asking Failed declared (§8.2); start timeout → exit 8 naming `ta_wait_expired` | warn once per stream |
| Derivation | undecodable AAF current format | no for that stream | stream refused; exit 8 unless `--allow-partial` | error |
| | > S_max streams | no for the excess | refused; exit 8 unless `--allow-partial` | error |
| | AUDIO_UNIT rate ≠ format nsr | no | exit 8 | error with both |
| Disk | insufficient space before start | no | exit 9 | error with bytes needed/free |
| WAV write | ENOSPC / EIO | no | stop, best-effort finalize, exit 9 | error |
| Audio ring | overrun | yes (counted) | record exit 11 at end | warn ≤ 1 / 10 s |
| Signals | SIGINT/SIGTERM | — | clean shutdown (§5.3); exit 0 or 130 (record) | info |

The daemon never retries a register operation or tries alternative values (SAF-4). The dext has no retry paths either: the RX buffer allocation uses the proven 256 KiB chunk size (AS-4) and a failure latches a fault.

### 13.2 External input validation

| Input | Rules | On failure |
|---|---|---|
| CLI `--seconds` | required for `gptp`/`streams`/`record`/`capture`; integer 1–86 400 | clap error, exit 2 |
| CLI `--out` | parent dir must exist and be writable; the file may exist (it is overwritten); for `aem-dump` the default path is used if omitted | exit 9 |
| CLI `--talker` | 16 hex digits, optional `0x` / colons | exit 2 |
| CLI timeouts | 1–600 s | exit 2 |
| `--milan-cert-version` | `major.minor`, each 0–255 | exit 2 |
| Dext shm header | §5.2 step 5, every field equal to the compiled constants | exit 4 |
| Dext slot | `len ≤ 1984` (also guaranteed by the dext's oversize guard, §3.4; the daemon re-checks because the mapping is a trust boundary), `cls` in range, `index` == expected, not both VLAN flags set; `clock_gen` not ahead of `hdr.clock_gen` (wrapping difference `hdr.clock_gen − slot.clock_gen` < 2^31) | drop + counter (desync handling; `slot_gen_invalid` for the last rule) |
| RX descriptor (in the dext) | TSIP ⇒ PKT_LEN ≥ 16; payload ≤ 1984 | RxErrors / RxOversize; buffer re-armed |
| Ethernet frames | length ≥ header sizes before every field access; VLAN at most one tag; all parsers return `Result<_, DropReason>`; no panics or indexing without checks (`clippy::indexing_slicing` denied in the protocol crates) | drop + per-reason counter |
| gPTP | versionPTP 2, majorSdoId 1, domain 0, messageLength ≥ the type's size and ≤ frame; Follow_Up TLV type 3 / organizationId 00-80-C2 / subtype 1 *(verify)*; messages from portIdentities other than the 16R counted and ignored | drop + counter |
| MRP | ProtocolVersion 0 accepted (higher: parse known types, per 802.1Q forward compatibility); AttributeLength must equal the type's defined length; NumberOfValues ≤ 512 per vector; EndMark required | drop PDU + counter |
| ADP | control_data_length 56 *(verify)*; valid_time ≥ 1 | drop + counter |
| AECP | control_data_length within frame; responses matched only to in-flight commands; unsolicited only from registered talkers; commands to our entity validated per command (descriptor type/index range, payload lengths) | malformed command → BAD_ARGUMENTS response (never silence); malformed response → drop + counter |
| ACMP | control_data_length 44 *(verify)*; unique ids < counts | status LISTENER_UNKNOWN_ID / TALKER_UNKNOWN_ID etc. |
| AEM descriptors from the 16R | per-type minimum length; counts/offsets inside the descriptor; the §9.2 limits; strings ≤ 64 B, NUL-padded, UTF-8 lossy | truncate/record a quirk; continue |
| AAF | §10.2 | drop + counters |
| Dext user-client inputs | as Phase 0b, plus §3.6 (StepClock bounds, filter count/MAC/VID, attach flags) | `kIOReturnBadArgument` |

### 13.3 Invariant ownership

| Invariant | Owner | Why |
|---|---|---|
| Device scope, register safety, TX rate, ppb/step bounds, single owner, stop-before-release | dext (§3.7) | Must hold regardless of client behavior |
| Formats, rates, channel counts come from the 16R AEM | `avb_avdecc::derive` (the single constructor of `ListenerModel`; nothing else builds STREAM_INPUT formats or ring geometry) | One place to review for acceptance 14 |
| Ring geometry and channel map match the bound model | `avb-daemon::stack` builds `RingConfig` only from `ListenerModel` | Same |
| Our entity never claims a format it can't decode | `derive` filters through `StreamFormat::decodable()` | Avoids binding something the AAF path drops |
| Our port never becomes master | `avb_gptp::Port` has no Announce/Sync TX code path | Structural, not a flag |
| Per-packet path doesn't allocate, block or log | `audio` thread code only; enforced by review plus a test with a counting global allocator in the pipeline test (§14) | NFR-3 |
| Declarations withdrawn on clean exit | `avb_srp::Participant::withdraw_all` called from the shutdown path | SRP-7 |

## 14. Testability

### 14.1 Unit and host tests (`cargo test --workspace --locked`, `make -C driver/tests test`)

| Crate | Tests |
|---|---|
| avb-core | ext32 wrap (±2^31 around 2^32 boundaries), MAC/EUI-64 derivation, VLAN parse, SeqLock (atomic words, §5.1) concurrent writer/reader test with no torn values and the retry bound returning `Err(Contended)` against a writer held at odd `seq`, `is_avdecc_dest` boundary table (same MACs as the dext test), Timebase conversions for 125/3 and 1/1 |
| avb-dext | ABI sizes/offsets vs the literals in §3.1 (including `clock_gen` at slot 40 and header 896, `AVB_RXF_TS_STEP_AMBIG` = 8); shm header validation accept/reject; slot consumer over a heap buffer simulating producer heads/tails including wrap and desync |
| avb-gptp | encode/decode vectors for every message type (byte-exact, from 802.1AS figures + M1 captures); link delay and NRR math (the Phase 0b numbers as a vector: raw 455.6 → comp 142.6); asCapable transitions; servo simulation at Ts ∈ {1/32, 1/8, 1} s plus a mid-run Ts change (§7.3 pass criteria); Ts derivation from logMessageInterval incl. rejection of −6, 2 and 0x7F (previous Ts kept, counter incremented); servo step rule in TRACKING and LOCKED (→ TRACKING, `i = 0`), ACQUIRE entered only at start and GM change, integrator frozen while clamped (a 600 ppm GM step-input test must not wind up: after the GM returns to −83 ppm, |offset| < 1 µs within 2× the normal lock time); responder skips Pdelay_Resp_Follow_Up across a step; model regression with synthetic brackets and outliers. **Step bookkeeping (rev 4, classification rev 5):** `classify_rx_gen` on slots (gen g, no AMBIG → g; AMBIG with any gen → Ambiguous); with `FakeDext` modelling the §3.6 dext rule and a **simulated 5 ms tick delay** after a step: a pre-step Sync and a pre-step 16R Pdelay_Req DMA'd before the step and harvested by the delayed (first settled) tick are AMBIG (Sync skipped, no second step; Pdelay_Req answered without Follow_Up), and slots from the next tick carry the new generation and are used; the current `gen` follows `hdr.clock_gen` after each StepClock/SetClock; a slot with `clock_gen` ahead of the header is dropped (`slot_gen_invalid`); a 100 µs step in the middle of a full NRR window moves the reported `nrr_systim` by ≤ 0.01 ppm and asCapable stays true; a Sync in an AMBIG slot or with an old `clock_gen` is skipped (`sync_step_skip`) and causes no second step; an ambiguous Pdelay_Req is answered without Follow_Up; a step decided while a `want_ts` frame is outstanding is issued only after the timestamp resolves or times out; an NRR of 1 + 1.5e−3 is rejected (`nrr_out_of_range`); asCapable stays true across the ACQUIRE SetClock (the pre-SetClock NRR is held) and the servo goes ACQUIRE → TRACKING without entering HOLDOVER; SetClock at ACQUIRE followed by a GM change seeds `f` from a single-generation NRR (no window ever mixes pre- and post-SetClock exchanges); syncEvent vector with a non-zero Sync correctionField (both correction fields added, 2^-16 scaling, negative values); a Pdelay_Resp waiting behind a held step for > 8 ms since its Pdelay_Req's harvest is dropped with `pdelay_resp_late_drop` and no Follow_Up, one at 7.9 ms is sent. **Port SM (rev 4):** one test per edge: Listening → Slave (first GM → ACQUIRE); Slave → Listening on announceReceiptTimeout with Ta from logMessageInterval 0 and −3 and the 1 s placeholder (servo → HOLDOVER, advertised GM = our clockIdentity, ADP re-advertise action emitted); Slave → Listening on asCapable false for > 3·Ta; Listening → Slave with the same GM (HOLDOVER → TRACKING on the next Sync with asCapable, no ACQUIRE); with a different GM (`gm_changes`, ACQUIRE); Slave → Slave GM change; Announce interval 5 and 0x7F rejected (Ta kept); Announce from our own clockIdentity or with our clockIdentity in the path trace rejected; HOLDOVER stays HOLDOVER while Syncs arrive with asCapable false (`sync_in_holdover`) |
| avb-srp | applicant/registrar transition tables (every row); LeaveAll; PDU encode/decode vectors for Domain, TA, TF, Listener (four-packed), MVRP VID; NumberOfValues > 1 decode |
| avb-avtp | AAF header decode vectors; stream-format decode/encode (§10.1 vectors); `decodable()` matrix |
| avb-avdecc | ADP/AECP/ACMP encode/decode vectors; descriptor codecs round-trip; enumeration against a scripted fake talker (including timeouts, IN_PROGRESS, NOT_IMPLEMENTED); `derive` with a synthetic AEM of 3 streams × 4 ch, one with 17 AAF streams (refusal), and one with 2 AAF outputs around a CRF output at index 1 (CRF `unbound_by_design`, STREAM_INPUT 0 ↔ output 0, STREAM_INPUT 1 ↔ output 2, no exit 8); ACMP addressing (every encoded ACMPDU goes to 91:E0:F0:01:00:00); TA-vs-PROBE_TX mismatch switching the effective values; A-5 status classification against `A5_RESOURCE_STATUSES` (and a non-member such as LISTENER_EXCLUSIVE not classified); entity responder per command (status codes, STREAM_IS_RUNNING); ACMP listener SM: one test per §9.4 table row (including the rev-4 TA_WAIT row: no TA for 2 s → Asking Failed declared once, `ta_wait_expired` = 1, a TA at 3 s → SETTLED_RSV_OK with Ready; Talker Failed withdrawn row; rev 5: Talker Failed already registered while in PRB_W_RESP, then PROBE_TX SUCCESS → Asking Failed declared on entry, A-5 classified for a code in `A5_FAILURE_CODES` and not for a non-member, TA_WAIT not armed; the same on entry via the probe fallback; TA and Talker Failed both registered on entry → TA row wins (Ready); SETTLED_RSV_OK with the TA's VID changed → `ta_probe_mismatch`, `SetStreamFilter` re-issued with the new VID, Ready still declared and no Lv emitted; SETTLED_RSV_OK with only accumulated latency changed → no filter call, new stream table with the new `early_bound`, Ready still declared; `last_available_index` updated on an ENTITY_AVAILABLE in SETTLED_RSV_OK and in PRB_W_RETRY, initialized from the ADP table at BIND_RX, and two successive reboots (index 5 → 1 → 6 → 0) both detected), plus the fallback window and reboot/rebind filter removal; controller binding (rev 4): with 3 streams, CONNECT_RX for j+1 is not sent before j's final outcome; a response with the wrong sequence_id, wrong listener_unique_id, wrong controller_entity_id, or the command message_type (loopback echo) is ignored and counted `acmp_ctrl_unmatched`; ACMP and AECP sequence counters advance independently; GET_AVB_INFO `msrp_mappings` reflects an adopted Domain (SRclassID 6, priority 2, VID 3) after a mismatch, not {6, 3, 2}; CLOCK_SOURCE 1 location following the reference index; model-id stability |
| avb-mediaclock | DLL convergence with synthetic pt (±100 ppm, ns quantization), wrap, tv=0, sparse, mr reset, gaps; placement rounding; window stats |
| avb-audioring | layout offsets; clock-block seqlock over atomic fields (f64 bit round-trip, concurrent writer/reader); producer/consumer SPSC stress across 2 threads; overrun/skip; zero-fill; gap log |
| avb-daemon | `txsched`: strict priority, drop-new on a full class (`TxFull` + counter), Pdelay_Resp sent next with the AVDECC queue full, a held `want_ts` frame not blocking non-timestamped frames, a pending step blocking new `want_ts` frames until it is issued; the 8 ms Pdelay_Resp late-drop rule; stream-table channels: with the return `sync_channel(4)` full, the audio side keeps the old table, counts `audio_return_full` and returns it on the next batch, all under the counting allocator (0 allocations on the audio side); `gptp` subcommand against `FakeDext` with a silent peer → exit 6 after `--discover-timeout`, with a Pdelay-only peer → runs to `--seconds`, `locked: false`, exit 0; WAV/RF64 header bytes (golden; 104 B header); RF64 switch at the 4 GiB boundary (sizes computed, no huge file written); pcap header; **pipeline test**: synthetic AEM (3 streams × 4 ch, INT_32BIT depth 24, 48 kHz) + generated AAF frames with jitter, a gap, a late packet and a reordered packet, fed through `audio` logic → ring → `rec` → WAV bytes. It checks alignment (AAF-7), zero-fill, counters, channel map and bit exactness, and runs under a counting allocator asserting 0 allocations inside the per-packet function (acceptance 14, NFR-3). A second case runs the same pipeline at 96 kHz / 12 spf with 2 streams × 6 ch and checks that lock_packets, lag_frames, capacity_frames and the WAV header follow the model (§10.5) |
| driver/tests | §3.7 list |

Integration vs hardware: everything tagged [CODE] is covered above. [HW] items are proven only by M1–M5 runs (§4), and the build-verification doc must not claim them (VER-5).

### 14.2 Fakes

`avb-daemon` drives the dext through a trait `DextPort` (`send`, `get_tx_ts`, `read_clock`, `step`, `adj_freq`, `set_filter`, `rx_ctl()`, `rx_audio()`, `health()`). It has two implementations:
- `RealDext` over `avb-dext`;
- `FakeDext` (test-only): heap rings, a scripted peer and a simulated clock.

This lets the full-stack test bind and record against a simulated 16R without IOKit.

### 14.3 Hardware steps (per milestone, §4)

Each milestone run uses `package.sh` → `avb-installer activate` → (user approval/replug only if needed) → the daemon commands, with logs and JSON written per VER-4.

### 14.4 Review checklist additions

- No new register writes in the dext; four new reads cited. (The 2048-descriptor ring only changes the value of the existing RDLEN write; the 16 chunk addresses go into descriptors in DMA memory, not registers.)
- `classifyRx` rules match §3.4; both harvest length guards (TSIP short, oversize) precede every slot `memcpy`.
- One attach per user client (`RingsUsed` checked in `RingAttach`).
- Each protocol crate writes only to its own TX class outbox (§5.2).
- `ClientGone` order matches §3.3; `RingDetach` order matches §3.6.
- RQDPC(0) 0xC030 is on the `ReadReg32` denylist.
- No GPL/LGPL-derived code or tables.
- All dependencies `=` pinned and in §6.3.
- No `/tmp`, `cd`, sudo, `csrutil`, `nvram` or `bputil` in code or scripts.
- No hardcoded rate, channel or stream counts outside tests: grep for `48000`, `8000`, `192`, `65536`, `125_000`, `8 *` in non-test code. Rate-dependent thresholds must be the §10.5 durations converted with the model's rate/spf. The only allowed literals are the S_max = 16 limit, the 1722 nsr code table in `stream_format`, and the §10.3 class A observation interval.
- Ownership rules match §3.2 (no ownership change in `InitDatapath`/`ShutdownDatapath`).
- Every `v->stage`/`v->flags` write goes through `setStage`/`setFlags` (grep). `RingAttach` sets and `RingDetach`/`ClientGone` clear `AVB_FLAG_RINGS_ATTACHED | AVB_FLAG_OWNED` through `setFlags`, the clear before `rxMem` is released (§3.6). M0 checks it on hardware: `avb-probe info` shows both flags while a daemon owns the rings and neither after it exits (clean and SIGKILL).
- `setClock()` is the only SYSTIM write sequence and is where `clockGen`, `hdr->clock_gen` and `rxStepArm` are updated (grep for SYSTIML/SYSTIMH writes).
- Every ACMPDU is sent to 91:E0:F0:01:00:00.

## 15. Only confirmable on the real 16R

| Item | Where it shows up | Design behavior until confirmed |
|---|---|---|
| U-1…U-5 entity model, stream count, formats, rates, clock source | M1 dump | everything derived; S_max = 16 refusal path |
| U-6 Milan revision, 2011 vs 2020 gPTP | MVU response, Signaling in the M1 capture | Signaling answered only if received; ACQUIRE and ENTITY GET_COUNTERS decided from the Milan v1.2 text (AS-7), not from the 16R |
| U-7 ACMP flow | M3 | Milan probing; GET_STREAM_INFO fallback logged |
| U-8 MSRP behavior / Domain / dest MACs | M1 capture, M3 | default Domain then adopt the 16R's; MACs from PROBE_TX/TA; if no TA within TA_WAIT = 2 s, Listener Asking Failed is declared (§8.2) unless the 802.1Q check forbids it (then back to the user, §4); `ta_wait_expired` reported |
| 16R Announce behavior across its own reboot | M3/M5 reboot runs (ADC-13) | Slave → Listening after 3·Ta, servo HOLDOVER; same GM → TRACKING, new GM → ACQUIRE (§7.3) |
| 802.1Q-2018 §35.2.4.3/§35.2.6 Listener-without-Talker rule | text check before M3 (no hardware) | TA_WAIT edge implemented only if permitted |
| U-9 presentation offset, tv/sp, latency | M4 histograms | early bound from TA latency or 2 ms |
| U-10 non-OUI ids accepted | M3 | blocker if rejected (A-3) |
| U-11 Announce/Sync, GM id, Sync interval | M2 | servo period and Follow_Up/HOLDOVER windows follow the received logMessageInterval; lock budget 60 s |
| U-12 VLAN in buffer vs descriptor | M3 `run --capture` slot flags on the first bound AVTP frames | both handled |
| AS-9 / A-5: non-AAF outputs and simultaneous-streaming limits | M1 dump (formats), M3 (ACMP/MSRP statuses) | non-AAF `unbound_by_design`; resource statuses classified per §9.2 |
| U-13 latency constants | M2 vs 16R GET_AVB_INFO propagation_delay; > 50 ns difference → finding | Phase 0b constants |
| AS-3 Stop on SIGKILL | M5 | — |
| AS-4 16 × 256 KiB single-segment DMA | M0 (size proven in Phase 0b; the count of 16 is new) | fault latched on failure |
| 32 KiB RX descriptor ring returned as one DMA segment | M0 (smaller than the proven 256 KiB, but this exact allocation is new) | `kAvbFaultDma` latched on failure |
| DriverKit per-process memory limit vs 17 MiB `rxMem` + 4 MiB RX buffers | M0 `RingAttach` | `kIOReturnNoMemory` → exit 1; a failure here goes back to the user (it would force smaller rings, an ABI change) |
| AS-6 LPE = 0 frame limit | M0/M4 `RxOversize` counter (expected 0) | memory safety doesn't depend on it (§3.4 guards) |
| AS-5 TSIP on AVTP frames | M3 `run --capture` slot flags (control frames in M1) | mach fallback, counted |
| Multi-process probe flow with no owner | M0 (`datapath up` → `rx-ptp` → `pdelay` → `datapath down`) | Phase 0b semantics (§3.2) |
| PTP-7/8, MCR-3/5, NFR-1/2 targets | M2, M4 | targets as stated; any miss reported with numbers |

## 16. Backlog (not in Phase 1a)

- MSI + EITR interrupt-driven RX, if NFR-2 p99 misses (§10.4).
- TIMADJL/H atomic time adjust, once its format is verified in 333016.
- Persisting the derived model across runs.
- Automatic re-bind after a rate/format change. Phase 1b owns rate changes.
- Multiple talkers or switches beyond "doesn't break".
- An IPC/XPC control API (Phase 1b).

## 17. Files

New:
- `crates/avb-core`, `avb-dext`, `avb-gptp`, `avb-srp`, `avb-avtp`, `avb-avdecc`, `avb-mediaclock`, `avb-audioring`, `avb-daemon` (each with `Cargo.toml`, `src/`, `tests/vectors/` where used);
- `packaging/daemon.entitlements`.

Changed:
- `Cargo.toml` (members), `Cargo.lock`;
- `driver/AVBShared.h` (§3.1, including `#include <stddef.h>` and `kAvbCtrRxOversize`);
- `driver/AVBI210Driver/{AVBI210Driver.iig, AVBI210Driver.cpp, AVBI210UserClient.iig (adds `MarkRingsUsed`/`RingsUsed` LOCALONLY), AVBI210UserClient.cpp, I210Regs.h, I210Logic.hpp (adds `rxFrameBounds`, `rxVlanFlags`, `rxHarvestStep`, `RxStepAmbig`/`rxStepArm`/`rxStepAmbiguous`)}`;
- `driver/AVBBcmDriver/AVBBcmUserClient.cpp`: all three `kAvbSelCount` uses (lines 54, 66, 131) become `kAvbSelBaseCount`, with the `sizeof`-based static_assert (§3.1);
- `driver/tests/i210logic_tests.cpp` and `driver/tests/tg3logic_tests.cpp`: both change `static_assert(AVB_ABI_VERSION == 2)` to `== 3` (mandatory: the build fails otherwise). tg3 also asserts `kAvbSelBaseCount == 10`; i210 adds the §3.1 offset literals and the §3.7 tests;
- `crates/avb-probe/{Cargo.toml, src/lib.rs}` (`abi.rs` and `iokit.rs` move into `avb-dext`, re-exported). The probe's ABI check becomes v3. All probe commands keep working: `datapath`/`rx-ptp`/`pdelay` use the legacy PollRx mode, which stays available when no rings are attached. The multi-process flow (`datapath up` in one process, `rx-ptp`/`pdelay` in later ones) works because, with no ring owner, the data path outlives the probe process (§3.2). It works only while no daemon owns the rings; while one does, the probe's data-path commands get `kIOReturnExclusiveAccess` and its read-only commands (`info`, `regs`, `clock`) keep working. None are retired (DP-12);
- `scripts/package.sh` (embed and sign `avb-daemon`, `-derivedDataPath`).

Results and verification artifacts:
- `.agents/tasks/phase1a-build-verification.md`;
- `docs/phase1a-results.md`;
- `.agents/tasks/phase1a-aem-16r.json`;
- `.agents/tasks/phase1a-*.json` (gPTP, record summary, counters);
- large files under `.agents/tmp/`.

## 18. Review findings (pass 1) and responses

All 19 findings are addressed; none are backlogged or ignored.

| # | Sev | Response | Where |
|---|---|---|---|
| 1 | HIGH | Addressed as proposed. Only `RingAttach` confers ownership; released only by the owner's `RingDetach` or `ClientGone`. `InitDatapath`/`ShutdownDatapath` never change ownership; owner `ShutdownDatapath` keeps rings attached. Non-owner exit only logs. With no owner, Phase 0b semantics apply, so the multi-process probe flow works; M0 now tests it. §5.2 step 6 kept and now reachable | D7, §3.2, §3.3, §5.2, §4 M0, §15, §17 |
| 2 | MED | Addressed. Header-field writer table: `setStage`/`setFlags` mirror into the header, `setFault` writes `hdr->fault` and clears `producer_running`, `TimerFired` stamps `last_tick_mach` first, `RingAttach` initializes all. Daemon watchdog at 50 ms → GetInfo → exit 7 or `harvest_stall` | §3.4, §3.7, §5.2, §13.1 |
| 3 | MED | Addressed. `RingDetach` steps specified; fresh memory and a new generation per attach; daemon checks generation (exit 4); shutdown order ShutdownDatapath → RingDetach → unmap → close | §3.6, §5.2, §5.3, §13.1 |
| 4 | MED | Addressed. `kRqdpc0` in `I210Regs.h`, denied in `isReadableRegOffsetI210`, host test `CHECK(!isReadableRegOffsetI210(0xC030))` | §3.4, §3.7, §14.4 |
| 5 | MED | Addressed. All three BCM uses (lines 54, 66, 131) switch to `kAvbSelBaseCount`; `sizeof`-based static_assert in both user clients | §3.1, §17 |
| 6 | MED | Addressed. Audio talker set A (AAF outputs), bound set B, STREAM_INPUT j ↔ B[j]; non-AAF `unbound_by_design`, never exit 8; A-5 resource statuses classified. Recorded as AS-9, because it interprets ADC-14's "every STREAM_OUTPUT" as "every audio STREAM_OUTPUT"; a non-AAF audio format goes back to the user | AS-9, §4, §9.2, §9.3, §10.6, §11.4, §13.1, §14.1, §15 |
| 7 | MED | Addressed. Every ACMPDU goes to 91:E0:F0:01:00:00. TA compared with PROBE_TX on registration; `ta_probe_mismatch`; TA values are effective for the filter, the audio table and the §10.2 check | §8.2, §9, §9.2, §9.4, §10.2, §13.1 |
| 8 | MED | Addressed. Ts from the Sync logMessageInterval (clamped [2^−5, 2^1] s), Follow_Up wait and HOLDOVER 3·Ts, Kp = 0.2/Ts, Ki = 0.02/Ts; simulation at three Ts plus a mid-run change | §7.3, §14.1, §15 |
| 9 | MED | Addressed. LOCK_HOLD, LAG_GAP, SILENT_FR, RING_SPAN, MODEL_PUB as durations converted with the model's rate/spf; Tu = spf/rate; 96 kHz/12 spf pipeline case; wider grep list | §10.5, §10.6, §11.2, §14.1, §14.4 |
| 10 | MED | Addressed as proposed. avb-core has no mach2 (Timebase passed in); avb-audioring depends on avb-core + libc (`new_anonymous` only); avb-core → avb-avdecc edge added | §6.1, §6.2, §6.3 |
| 11 | NIT | Addressed. `AVB_RX_CLASS_COUNT 7`, indices 1..6 = cls, 7 reserved, static_assert; RxFrames counted after the EOP check | §3.1, §3.4 |
| 12 | NIT | Addressed. Shared `isAvdeccDest` / `avb_core::is_avdecc_dest` predicate and boundary table | §3.4, §5.2, §14.1 |
| 13 | NIT | Addressed. U-12 and the AVTP half of AS-5 move to M3. `capture` is documented as control-only (`--class` removed); stream capture is `run --capture` | §4, §11.1, §15 |
| 14 | NIT | Addressed. `f_base`, `elapsed`, `i`, `n0` defined | §7.3, §10.5 |
| 15 | NIT | Addressed. Both test files change to ABI 3 (mandatory); tg3 asserts `kAvbSelBaseCount == 10` | §17 |
| 16 | NIT | Addressed. 16 × 256 KiB chunks (proven size); retry path removed | AS-4, §3.4, §3.6, §13.1, §15 |
| 17 | NIT | Addressed. Policy values converted via `mach_timebase_info` (6000/1440/4800 ticks at 125/3) | §5.1 |
| 18 | NIT | Addressed. Own clockIdentity until Slave, re-advertised on change | §9.3 |
| 19 | NIT | Addressed. "Design tolerance 0 frames, stricter than acceptance 9's ± 1 frame-period" | §11.2 |

Side effects of the fixes, checked for consistency:
- At Ts = 1 s, the LOCKED criterion (16 consecutive Syncs < 1 µs) takes 16 s. That still fits the 60 s PTP-7 budget (§7.3).
- The audio ring's data size now scales with rate. At 96 kHz × 64 ch it is 32 MiB, which is still allocated once at start.
- `run --capture` adds two preallocated 4096-slot buffers of 8 MiB each. They are allocated only when the flag is given.

## 19. Review findings (pass 2) and responses

All 16 findings are addressed; none are backlogged or ignored. §18 is kept as the pass-1 record; where pass 2 changed a pass-1 response (row 8's "clamped" Ts is now "rejected"), the text in §7.3 is authoritative.

| # | Sev | Response | Where |
|---|---|---|---|
| 1 | MED | Addressed as proposed. `TSIP && len < 16` → RxErrors (existing guard kept); `flen > 1984` → new `kAvbCtrRxOversize = 33`; both before classify/copy, in both RX modes. Pure `rxFrameBounds` host-tested; invariant row added; AS-6 now only explains an expected-0 counter | §3.1, §3.4, §3.7, §13.2, §15, §17 |
| 2 | MED | Addressed. §9.4 is now an exhaustive transition table incl. PRB_W_RESP2, `probe_cycles` (incremented only on total silence), RETRY exit, TA withdrawn, Talker Failed, departure/timeout, reboot via `available_index`, duplicate and different-talker BIND_RX, UNBIND, unknown index and late responses. Fallback window derived (9.2–12.2 s), replacing "12 s". One unit test per row | §9.4, §14.1 |
| 3 | MED | Addressed as proposed. `gptp` exits 6 if neither a remote Announce nor a completed Pdelay exchange occurs within `--discover-timeout` (30 s); not locking only sets `locked: false`. Exit-6 row extended; §13.1 rows; FakeDext tests | §11.1, §11.4, §13.1, §14.1 |
| 4 | MED | Addressed as proposed. Sub-queues gPTP 8 / MRP 16 / AVDECC 40, strict priority, drop-new with `Err(TxFull)` and `tx_queue_full[class]`; caller behavior per class; held `want_ts` frame doesn't block other classes; new `txsched` module; §13.1 row; unit tests incl. Pdelay_Resp with AVDECC full | §5.2, §6.1, §13.1, §14.1, §14.4 |
| 5 | NIT | Addressed. ACQUIRE only at start/GM change; step rule (> 100 µs → step, `i := 0`, TRACKING) in TRACKING and LOCKED; anti-windup at the ±488 000 ppb clamp; initiator and responder discard across a step (`pdelay_resp_step_skip`) | §7.3, §14.1 |
| 6 | NIT | Addressed. Out-of-range logMessageInterval (incl. 0x7F) rejected, previous Ts kept, counted; test renamed to rejection | §7.3, §14.1 |
| 7 | NIT | Addressed. CLOCK_SOURCE 1 location = reference index r from `derive` (shared `reference_index`), which also carries CLOCK_SYNC_SOURCE; runtime switches internal | §9.3, §10.5, §14.1 |
| 8 | NIT | Addressed. `valid_bits = max(bit_depth)` over B; per-stream depth at `streams[i] +44` (from the pad) and in `channels.json` | §10.6, §11.3 |
| 9 | NIT | Addressed. NO_RESOURCES replaced by TALKER_NO_STREAM_INDEX; `A5_RESOURCE_STATUSES` and `A5_FAILURE_CODES` are named, cited const lists. NO_RESOURCES remains only as the AEM status for REGISTER_UNSOLICITED_NOTIFICATION, where it is correct | §9.2, §13.1, §14.1 |
| 10 | NIT | Addressed as proposed. VLAN_DESC iff VP && no in-buffer tag; VLAN_FRAME iff in-buffer tag; mutually exclusive via `rxVlanFlags`; pcap re-inserts only for VLAN_DESC; four-combination host test; daemon rejects a slot with both | §3.1, §3.4, §4, §3.7 tests, §13.2 |
| 11 | NIT | Addressed. Comment states index = cls (= base + cls − 1); static_asserts Gptp → 1, Stream → 6 in C++ and Rust | §3.1 |
| 12 | NIT | Addressed. ENTITY GET_COUNTERS and ACQUIRE_ENTITY decided from the Milan v1.2 text per AS-7 with citation and test, recorded in build verification | §9.3, §15 |
| 13 | NIT | Addressed. Signaling ≤ 1/s row in the SAF-6 table | §3.5 |
| 14 | NIT | Addressed. `#include <stddef.h>` added | §3.1, §17 |
| 15 | NIT | Addressed. Re-attach on the same user client → `kIOReturnNotPermitted` (`RingsUsed`/`MarkRingsUsed` LOCALONLY); new connections get fresh memory and generation | §3.6, §13.1, §14.4, §17 |
| 16 | NIT | Addressed. Removing the entitlement check requires asking the user; M5 row moved into the milestone table; WAV header estimate 104 B | §3.3, §4, §11.2 |

The review's unverified assumptions (17 MiB `rxMem` within DriverKit memory limits; the 32 KiB descriptor ring as one segment) are added to §15 as M0 checks with defined failure behavior.

## 20. Review findings (pass 3) and responses

All 8 findings are addressed; none are backlogged or ignored. No ABI v3 layout, selector, register access or dependency changed in this revision (the only dext change is the pure `rxHarvestStep` helper and one bool ivar).

| # | Sev | Response | Where |
|---|---|---|---|
| 1 | MED | Addressed as proposed. `stepgen`: `step_begin_mach`/`step_end_mach`/`gen` per StepClock/SetClock; RX timestamp generation from slot `harvest_mach` with a 2 ms ambiguity window (ambiguous → not used for timing, `rx_ts_ambiguous_step`; Pdelay_Req still answered without Follow_Up); TX generation at SendFrame, with no step issued while a `want_ts` frame is outstanding (held ≤ 20 ms) and no new `want_ts` while a step is pending; Syncs of an old or ambiguous generation skipped (`sync_step_skip`); NRR window holds only current-generation exchanges, cleared on a generation change, last valid NRR held until 2 new exchanges (asCapable not dropped by a step; nothing held after the ACQUIRE SetClock); \|NRR − 1\| > 1000 ppm rejected (`nrr_out_of_range`). Tests for the classification boundaries, mid-window step (≤ 0.01 ppm), ambiguous Sync, held step, out-of-range NRR and the SetClock-then-GM-change seed | §7.2, §7.3, §7.6, §14.1 |
| 2 | MED | Addressed as proposed, plus two details the fix implied: Announce acceptance rules (remote portIdentity, domain 0, path trace without us) and the case of a timeout before the servo left ACQUIRE (stays ACQUIRE). Ta from the Announce logMessageInterval in [−3, 4], 1 s placeholder; Slave → Listening on announceReceiptTimeout or asCapable false > 3·Ta (HOLDOVER, advertised GM reset to our clockIdentity, ADP re-advertised); return with the same GM → TRACKING, different GM → ACQUIRE; Slave → Slave GM change edge kept; HOLDOVER exits only on an accepted Sync **and** asCapable. One test per edge | §7.3, §7.6, §14.1, §15 |
| 3 | MED | Addressed as proposed. TA_WAIT = 2 s timer in SETTLED_NO_RSV (armed only when neither TA nor Talker Failed is registered): `ta_wait_expired`, one U-8 warn log per stream, Listener Asking Failed declared; a later TA → Ready via the existing row. Citation 802.1Q-2018 §35.2.4.3/§35.2.6 *(verify)*; if the text forbids it, the edge isn't built and the case goes back to the user (added as §4's third non-silent contradiction, and a §15 row). The exit-8 message names `ta_wait_expired`; §13.1 row; listener-SM tests for the new rows. A "Talker Failed withdrawn" row was added so the table stays exhaustive for the new timer | §4, §8.2, §9.4, §11.2, §13.1, §14.1, §15 |
| 4 | NIT | Addressed. `rxHarvestStep` + `IV::rxDiscardToEop`: one `RxNonEop` per multi-descriptor frame, continuation descriptors discarded through EOP without counting `RxFrames`; same helper in the legacy path; host test sequences | §3.4, §3.7 tests, §17 |
| 5 | NIT | Addressed. `sync_channel(4)` both directions; `try_send` on both sides; on `Full` the audio side keeps the old table in a preallocated slot and retries next batch (`audio_return_full`); counting-allocator test | §5.1, §14.1 |
| 6 | NIT | Addressed. `msrp_mappings` uses the effective Domain from `avb_srp`; test with an adopted Domain | §9.3, §14.1 |
| 7 | NIT | Addressed as proposed. One ACMP command in flight per listener id pair, sequential in j order, matched on message_type + 1, controller_entity_id, listener ids and sequence_id; separate controller ACMP u16 counter, distinct from AECP and from the listener entity's PROBE_TX counter (whose matching now also names message_type); GET_TX_STATE uses the controller counter with talker ids | §9.2, §9.4, §14.1 |
| 8 | NIT | Addressed. `__atomic_store_n`/`__atomic_load_n` with RELEASE/ACQUIRE for heads, tails and live header fields; RELAXED 64-bit counter stores; Rust mirrors via `AtomicU64`/`AtomicU32` views with Acquire/Release/Relaxed | §3.4 |

## 21. Review findings (pass 4) and responses

All 8 findings are addressed; none are backlogged or ignored. The only ABI v3 change is finding 1's use of 4 bytes of `AvbRxSlot.pad1` and 4 bytes of `AvbShmHeader.reserved`; no selector, register access, dependency or other offset moved, and `AVB_RX_MEM_SIZE` is unchanged.

| # | Sev | Response | Where |
|---|---|---|---|
| 1 | MED | Addressed with the suggested dext-side tagging; the daemon-side `harvest_mach` windows and the wrong 2048-descriptor justification are removed. `AvbRxSlot.clock_gen` (u32 at 40, `pad1` now 20 B at 44), `AVB_RXF_TS_STEP_AMBIG` (bit 3), `AvbShmHeader.clock_gen` (u32 at 896, `reserved` now 124 B at 900), `AVB_STEP_SETTLE_NS`. The `setClock()` helper (the only SYSTIM write sequence) increments `ivars->clockGen`, release-stores it into the header and arms `RxStepAmbig`. The pure `rxStepAmbiguous` marks every tick from the step through the first tick that **starts** ≥ 1 ms after the SYSTIM write as AMBIG; it uses measured tick start times, so a late timer only lengthens the ambiguous span and can't mislabel a pre-step frame. 1 ms is used rather than the suggested 100 µs so the settle window also covers draining a full RX packet buffer at line rate (*verify RXPBS*); WTHRESH = 1 (existing `rxdctlEnable()`) covers descriptor write-back. The daemon takes AMBIG → AMBIGUOUS, else `slot.clock_gen`; current `gen` = `hdr.clock_gen` after each StepClock/SetClock (owner-only, so exact). New invariant row, writer-table row, static_asserts (C++ and Rust literals), a `slot_gen_invalid` trust-boundary check, host tests for `rxStepAmbiguous` including the simulated 5 ms tick delay, and FakeDext-based stepgen tests (first settled tick AMBIG, next tick new generation) | §3.1, §3.4, §3.6, §3.7, §5.2, §7.3, §7.6, §13.2, §14.1, §14.4, §17 |
| 2 | MED | Addressed as proposed. The Talker Failed row now reads "registered (or, on entry to SETTLED_NO_RSV, already registered with no TA registered)": Asking Failed is declared, A-5 is classified for `A5_FAILURE_CODES`, TA_WAIT isn't armed. Entry evaluation order is fixed: TA, then Talker Failed, then arm TA_WAIT, so entry always leads to a declaration or an armed timer. New row `SETTLED_RSV_OK \| TA attributes changed → SETTLED_RSV_OK` applies §8.2 (compare, `ta_probe_mismatch`, effective values, `SetStreamFilter` + stream table on MAC/VID change, stream table only on a TSpec/latency change), and Ready stays declared. §8.2 now points at both rows (`TalkerChanged` from `avb_srp`). Tests for each new row in §14.1 | §8.2, §9.4, §14.1 |
| 3 | NIT | Addressed. The pre-SetClock NRR is held across the ACQUIRE SetClock exactly as across a StepClock (NRR is offset-invariant). The GM-change ACQUIRE seed still always comes from a single-generation NRR. The test is now "asCapable stays true across the ACQUIRE SetClock, no HOLDOVER" | §7.3, §14.1 |
| 4 | NIT | Addressed. `syncEvent = preciseOriginTimestamp + (FU.correctionField + Sync.correctionField)·2^-16 + delay_comp·rateRatio/NRR_systim`, with a comment that Sync's value is expected to be 0 in two-step (*verify clause*). Vector with a non-zero Sync correctionField | §7.3, §14.1 |
| 5 | NIT | Addressed with the drop option. A Pdelay_Resp is dropped together with its Follow_Up if `mach_now − harvest_mach` of the Pdelay_Req exceeds 8 ms when it reaches `SendFrame`, counted `pdelay_resp_late_drop`. Mach is used because it is unaffected by steps. The other option (letting Pdelay_Resp bypass a held step) was not taken because its outstanding timestamp would hold the step again | §7.2, §7.6, §14.1 |
| 6 | NIT | Addressed as proposed. The §10.6 clock block is accessed only through `AtomicU32`/`AtomicU64` (f64 bits), with the specified fence protocol. `avb_core::SeqLock` becomes `SeqLock<T: SeqWords<N>, N>` over `[AtomicU64; N]`, with the same protocol and a 1000-retry bound that returns `Err(Contended)`. Tests in avb-core and avb-audioring | §5.1, §10.6, §14.1 |
| 7 | NIT | Addressed as proposed. `RingAttach` calls `setFlags(set RINGS_ATTACHED \| OWNED)` after allocating and setting the owner. `RingDetach` and `ClientGone` clear both before `rxMem` is released. Writer table, review checklist and an M0 check with `avb-probe info` | §3.3, §3.4, §3.6, §14.4 |
| 8 | NIT | Addressed. `last_available_index` is initialized from the ADP table at BIND_RX and updated on every ENTITY_AVAILABLE from the talker in every bound state, after the reboot comparison. The ADP table tracks it while UNBOUND. Test with two successive reboots | §9.4, §14.1 |
