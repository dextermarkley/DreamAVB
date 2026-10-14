# gPTP Announce / Sync / Follow_Up, transcribed from the 16R

Becoming Grandmaster Capable (Milan v1.3 §4.3.1.1, *"a Talker PAAD shall be
capable of operating as a gPTP grandmaster"*) needs three message encoders
`avb-gptp` does not have: `msg.rs` carries encoders for Pdelay and Signaling
only, and Announce/Sync/Follow_Up are decode-only because the port was built
slave-only on purpose.

These layouts are **transcribed from a PreSonus StudioLive 16R**, the
grandmaster we already slave to, rather than written from the standard's field
tables. That is the project rule, and it exists because a guessed bit-packed
constant has shipped here before — the `WriteMix` FourCC, which passed its own
round-trip tests and failed only against real hardware. A captured message is
correct by construction; a transcription of a table is a guess with good
intentions.

Captured live over the I210 with a one-shot raw hex dump in the daemon's gPTP
RX path, undecoded, so a decoder written to a wrong layout could not
misrepresent the bytes being captured.

## ANNOUNCE — 76 bytes

```
1b 02 00 4c 00 00 00 0e 00 00 00 00 00 00 00 00 00 00 00 00
00 0a 92 ff fe 03 3b a9 00 01 0c 48 05 00
00 00 00 00 00 00 00 00 00 00 00 25 00 f8 f8 20 43 6a f8
00 0a 92 ff fe 03 3b a9 00 00 a0 00 08 00 08
00 0a 92 ff fe 03 3b a9
```

| Offset | Bytes | Field |
|---|---|---|
| 0 | `1b` | `majorSdoId`=1 (high nibble) \| `messageType`=0xB ANNOUNCE |
| 1 | `02` | `minorVersionPTP`=0 \| `versionPTP`=2 |
| 2-3 | `00 4c` | `messageLength` = 76 |
| 4 | `00` | `domainNumber` |
| 5 | `00` | `minorSdoId` |
| 6-7 | `00 0e` | `flagField` |
| 8-15 | zero | `correctionField` |
| 16-19 | zero | reserved / `messageTypeSpecific` |
| 20-27 | `000a92fffe033ba9` | `sourcePortIdentity.clockIdentity` |
| 28-29 | `00 01` | `sourcePortIdentity.portNumber` = 1 |
| 30-31 | `0c 48` | `sequenceId` |
| 32 | `05` | `controlField` = 5 |
| 33 | `00` | `logMessageInterval` = 0 → **1 s** |
| 34-43 | zero | reserved (10 bytes) |
| 44-45 | `00 25` | `currentUtcOffset` = 37 (TAI−UTC) |
| 46 | `00` | reserved |
| 47 | `f8` | `grandmasterPriority1` = **248** |
| 48 | `f8` | `clockQuality.clockClass` = 248 |
| 49 | `20` | `clockQuality.clockAccuracy` = 0x20 |
| 50-51 | `43 6a` | `clockQuality.offsetScaledLogVariance` |
| 52 | `f8` | `grandmasterPriority2` = 248 |
| 53-60 | `000a92fffe033ba9` | `grandmasterIdentity` |
| 61-62 | `00 00` | `stepsRemoved` = 0 |
| 63 | `a0` | `timeSource` = 0xA0 INTERNAL_OSCILLATOR |
| 64-65 | `00 08` | `tlvType` = PATH_TRACE |
| 66-67 | `00 08` | `lengthField` = 8 |
| 68-75 | `000a92fffe033ba9` | `pathSequence[0]` |

`priority1 = 248` is what Milan §4.2.6.2.1 requires of a Grandmaster-Capable
PAAD. Our `PRIORITY1` is 255, set specifically so we lose BMCA — that constant
is the conformance gap, and raising it to 248 is only honest once the transmit
paths below exist.

## SYNC — 50 bytes on the wire (`messageLength` 44 + Ethernet padding)

```
10 02 00 2c 00 00 02 08 00 00 00 00 00 00 00 00 00 00 00 00
00 0a 92 ff fe 03 3b a9 00 01 41 fd 00 fd
00 00 00 00 00 00 00 00 00 00
00 00 00 00 00 00
```

| Offset | Bytes | Field |
|---|---|---|
| 0 | `10` | `majorSdoId`=1 \| `messageType`=0 SYNC |
| 2-3 | `00 2c` | `messageLength` = 44 (the 6 trailing bytes are frame padding) |
| 6-7 | `02 08` | `flagField` — `twoStepFlag` set |
| 20-29 | … `00 01` | `sourcePortIdentity` |
| 30-31 | `41 fd` | `sequenceId` |
| 32 | `00` | `controlField` = 0 |
| 33 | `fd` | `logMessageInterval` = −3 → **125 ms** |
| 34-43 | zero | `originTimestamp` — zero, because two-step puts the real time in the Follow_Up |

## FOLLOW_UP — 76 bytes

```
18 02 00 4c 00 00 00 08 00 00 00 00 00 00 00 00 00 00 00 00
00 0a 92 ff fe 03 3b a9 00 01 41 fd 02 fd
00 00 00 00 0e 69 29 2e e4 32
00 03 00 1c 00 80 c2 00 00 01 00 00 00 00 00 0c
00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
```

| Offset | Bytes | Field |
|---|---|---|
| 0 | `18` | `majorSdoId`=1 \| `messageType`=8 FOLLOW_UP |
| 30-31 | `41 fd` | `sequenceId` — **same as the Sync it follows** |
| 32 | `02` | `controlField` = 2 |
| 33 | `fd` | `logMessageInterval` = −3 |
| 34-39 | `00 00 00 00 0e 69` | `preciseOriginTimestamp.secondsField` = 3689 |
| 40-43 | `29 2e e4 32` | `.nanosecondsField` = 690 840 114 |
| 44-45 | `00 03` | `tlvType` = ORGANIZATION_EXTENSION |
| 46-47 | `00 1c` | `lengthField` = 28 |
| 48-50 | `00 80 c2` | `organizationId` = IEEE 802.1 |
| 51-53 | `00 00 01` | `organizationSubType` = Follow_Up information TLV |
| 54-57 | zero | `cumulativeScaledRateOffset` |
| 58-59 | `00 0c` | `gmTimeBaseIndicator` |
| 60-71 | zero | `lastGmPhaseChange` (12 bytes) |
| 72-75 | zero | `scaledLastGmFreqChange` |

## What the capture self-validates

Nothing here rests on one reading being right:

- Sync and Follow_Up carry the **same `sequenceId`** (`0x41fd`), which is what
  pairs them — a layout off by a byte would not produce a match.
- Both carry `logMessageInterval = 0xfd` = −3 = 125 ms, the interval
  802.1AS specifies for Sync, and the Announce carries 0 = 1 s, its own.
- `sourcePortIdentity` reads as `000a92fffe033ba9` in all three, which is the
  16R's AVDECC entity id — independently known from its ADP.
- The Follow_Up's `nanosecondsField` is 690 840 114, i.e. under 1e9, which a
  shifted field would almost certainly violate.
- `currentUtcOffset` reads 37, the correct TAI−UTC value.

## Remaining work for §4.3.1.1

1. `encode_announce`, `encode_sync`, `encode_follow_up` in `msg.rs`, each with
   a byte-for-byte re-encode test against the hex above.
2. `PortState::Master` (today only `Listening` and `Slave`).
3. BTCA (802.1AS Clause 10.3) treating our own clock as a candidate dataset.
4. Announce TX at 1 s, Sync + Follow_Up TX at 125 ms, with TX timestamping for
   the `preciseOriginTimestamp`.
5. `PRIORITY1` 255 → 248, in `avb-gptp/src/lib.rs` **and** the AVB_INTERFACE
   descriptor in `avb-avdecc/src/entity/talker.rs`, which advertises 255 for
   the same reason.
6. `GRANDMASTER_CAPABLE` on the PTP_INSTANCE descriptor.
7. A servo mode for being the time source rather than disciplining to one.

Do not do 5 or 6 before 1-4: advertising a capability we do not have is the
same class of false claim as reporting a Milan certification we never passed.
