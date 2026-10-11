// SPDX-License-Identifier: MIT OR Apache-2.0

//! AECP: AVDECC Enumeration and Control Protocol (IEEE 1722.1-2021 §9;
//! design §9.2 AECP engine, §9.3 responder).
//!
//! AECP is sent unicast to the target's MAC. This module has the AEM and MVU
//! PDU codecs and the controller's in-flight/retry engine. The engine is
//! sans-IO: it emits frames to send and consumes received responses and clock
//! ticks; the daemon owns the socket and the clock.

use avb_core::{Eui64, MacAddr};

use crate::constants::{aecp_msg, aecp_status, subtype};
use crate::milan::MVU_PROTOCOL_ID;
use crate::wire::{Rd, Wr};

/// Common AEM PDU bytes before the command-specific payload (IEEE
/// 1722.1-2021 §9.2.1.1): control header (12) + target_entity_id(8) +
/// controller_entity_id(8) + sequence_id(2) + u|command_type(2).
pub const AEM_HEADER_LEN: usize = 24;

/// A decoded AEM command or response (IEEE 1722.1-2021 §9.2.1.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AemPdu {
    pub message_type: u8,
    pub status: u8,
    pub target_entity_id: Eui64,
    pub controller_entity_id: Eui64,
    pub sequence_id: u16,
    /// The `u` (unsolicited) bit of the command_type word.
    pub unsolicited: bool,
    pub command_type: u16,
    /// Command-specific payload after the fixed header.
    pub payload: Vec<u8>,
}

impl AemPdu {
    pub fn decode(pdu: &[u8]) -> Option<Self> {
        let mut r = Rd::new(pdu);
        let st = r.u8()?; // byte0 = subtype
        let b1 = r.u8()?; // byte1 = sv(1)|version(3)|message_type(4)
        if st != subtype::AECP {
            return None;
        }
        let message_type = b1 & 0x0F;
        if message_type != aecp_msg::AEM_COMMAND && message_type != aecp_msg::AEM_RESPONSE {
            return None;
        }
        let b2 = r.u8()?;
        let status = (b2 >> 3) & 0x1F;
        r.skip(1)?; // low byte of control_data_length
        let target_entity_id = Eui64(r.arr()?);
        let controller_entity_id = Eui64(r.arr()?);
        let sequence_id = r.u16()?;
        let ct = r.u16()?;
        let payload = r.rest().to_vec();
        Some(Self {
            message_type,
            status,
            target_entity_id,
            controller_entity_id,
            sequence_id,
            unsolicited: ct & 0x8000 != 0,
            command_type: ct & 0x7FFF,
            payload,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Wr::new();
        w.u8(subtype::AECP); // byte0 = subtype
        w.u8(self.message_type & 0x0F); // byte1 = sv|version|message_type
                                        // control_data_length = everything after the first 12 bytes:
                                        // 20 fixed (target+controller+seq+cmd) + payload.
        let cdl = (12 + self.payload.len()) as u16;
        w.u8((self.status & 0x1F) << 3 | ((cdl >> 8) as u8 & 0x07));
        w.u8(cdl as u8);
        w.put(&self.target_entity_id.0);
        w.put(&self.controller_entity_id.0);
        w.u16(self.sequence_id);
        w.u16((u16::from(self.unsolicited) << 15) | (self.command_type & 0x7FFF));
        w.put(&self.payload);
        w.into_vec()
    }

    pub fn is_response(&self) -> bool {
        self.message_type == aecp_msg::AEM_RESPONSE
    }
}

/// A decoded MVU (Milan Vendor Unique) command/response (Milan v1.2 §5.4.4):
/// a VENDOR_UNIQUE AECPDU whose first 6 payload bytes are [`MVU_PROTOCOL_ID`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MvuPdu {
    pub message_type: u8,
    pub status: u8,
    pub target_entity_id: Eui64,
    pub controller_entity_id: Eui64,
    pub sequence_id: u16,
    pub command_type: u16,
    pub payload: Vec<u8>,
}

impl MvuPdu {
    pub fn decode(pdu: &[u8]) -> Option<Self> {
        let mut r = Rd::new(pdu);
        let st = r.u8()?; // byte0 = subtype
        let b1 = r.u8()?; // byte1 = sv(1)|version(3)|message_type(4)
        if st != subtype::AECP {
            return None;
        }
        let message_type = b1 & 0x0F;
        if message_type != aecp_msg::VENDOR_UNIQUE_COMMAND
            && message_type != aecp_msg::VENDOR_UNIQUE_RESPONSE
        {
            return None;
        }
        let b2 = r.u8()?;
        let status = (b2 >> 3) & 0x1F;
        r.skip(1)?;
        let target_entity_id = Eui64(r.arr()?);
        let controller_entity_id = Eui64(r.arr()?);
        let sequence_id = r.u16()?;
        let proto: [u8; 6] = r.arr()?;
        if proto != MVU_PROTOCOL_ID {
            return None;
        }
        let command_type = r.u16()? & 0x7FFF;
        let payload = r.rest().to_vec();
        Some(Self {
            message_type,
            status,
            target_entity_id,
            controller_entity_id,
            sequence_id,
            command_type,
            payload,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Wr::new();
        w.u8(subtype::AECP); // byte0 = subtype
        w.u8(self.message_type & 0x0F); // byte1 = sv|version|message_type
        let cdl = (12 + 6 + 2 + self.payload.len()) as u16;
        w.u8((self.status & 0x1F) << 3 | ((cdl >> 8) as u8 & 0x07));
        w.u8(cdl as u8);
        w.put(&self.target_entity_id.0);
        w.put(&self.controller_entity_id.0);
        w.u16(self.sequence_id);
        w.put(&MVU_PROTOCOL_ID);
        w.u16(self.command_type & 0x7FFF);
        w.put(&self.payload);
        w.into_vec()
    }
}

// ---- The in-flight / retry engine (design §9.2 AECP engine) ----

/// Default per-command timeout before the single retry (§9.2: 250 ms).
pub const TIMEOUT_NS: u64 = 250_000_000;
/// IN_PROGRESS re-arm interval (§9.2: 1 s), capped by [`IN_PROGRESS_CAP_NS`].
pub const IN_PROGRESS_NS: u64 = 1_000_000_000;
/// IN_PROGRESS total cap (§9.2: 10 s).
pub const IN_PROGRESS_CAP_NS: u64 = 10_000_000_000;
/// Max commands per second across all targets (§9.2: ≤ 200/s).
pub const MAX_CMD_PER_SEC: u32 = 200;
/// Minimum spacing between command sends to honour [`MAX_CMD_PER_SEC`].
pub const MIN_SEND_GAP_NS: u64 = 1_000_000_000 / MAX_CMD_PER_SEC as u64;

/// A command the engine wants to (re)send on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AecpSend {
    pub target_mac: MacAddr,
    pub frame: Vec<u8>,
    pub sequence_id: u16,
    pub command_type: u16,
}

/// The outcome handed back for a completed command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AecpOutcome {
    /// A response arrived (any status). Carries the response.
    Response(AemPdu),
    /// The command finally timed out after its retry (§9.2 `timeouts[]`).
    TimedOut { command_type: u16, sequence_id: u16 },
}

#[derive(Clone, Debug)]
struct InFlight {
    target: Eui64,
    target_mac: MacAddr,
    sequence_id: u16,
    command_type: u16,
    frame: Vec<u8>,
    /// Deadline (ns) for the current attempt.
    deadline_ns: u64,
    /// Whether the single retry has been used.
    retried: bool,
    /// Accumulated IN_PROGRESS time (capped at [`IN_PROGRESS_CAP_NS`]).
    in_progress_ns: u64,
}

/// One in-flight AEM command per target entity, with pacing, 250 ms + 1
/// retry, and IN_PROGRESS handling (design §9.2). Per-controller wrapping u16
/// sequence ids. Sans-IO: `enqueue` adds work, `poll` emits sends, `on_rx`
/// consumes responses, `on_tick` ages deadlines.
#[derive(Clone, Debug)]
pub struct AecpEngine {
    controller_id: Eui64,
    next_seq: u16,
    /// At most one in flight per target.
    inflight: Vec<InFlight>,
    /// Pending commands not yet in flight, FIFO.
    queue: Vec<Pending>,
    last_send_ns: u64,
}

#[derive(Clone, Debug)]
struct Pending {
    target: Eui64,
    target_mac: MacAddr,
    command_type: u16,
    payload: Vec<u8>,
}

impl AecpEngine {
    pub fn new(controller_id: Eui64) -> Self {
        Self {
            controller_id,
            next_seq: 0,
            inflight: Vec::new(),
            queue: Vec::new(),
            last_send_ns: 0,
        }
    }

    /// Queue an AEM command to `target`. It is sent by [`AecpEngine::poll`]
    /// once no command is in flight to that target and pacing allows it.
    pub fn enqueue(
        &mut self,
        target: Eui64,
        target_mac: MacAddr,
        command_type: u16,
        payload: Vec<u8>,
    ) {
        self.queue.push(Pending {
            target,
            target_mac,
            command_type,
            payload,
        });
    }

    pub fn in_flight_count(&self) -> usize {
        self.inflight.len()
    }

    pub fn queued_count(&self) -> usize {
        self.queue.len()
    }

    fn next_sequence(&mut self) -> u16 {
        let s = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        s
    }

    /// Emit any command whose target is free and whose pacing gap elapsed.
    /// Returns at most one send per call (pacing is one command per
    /// [`MIN_SEND_GAP_NS`]).
    pub fn poll(&mut self, now_ns: u64) -> Option<AecpSend> {
        if now_ns.saturating_sub(self.last_send_ns) < MIN_SEND_GAP_NS && self.last_send_ns != 0 {
            return None;
        }
        let idx = self
            .queue
            .iter()
            .position(|p| !self.inflight.iter().any(|f| f.target == p.target))?;
        let p = self.queue.remove(idx);
        let seq = self.next_sequence();
        let frame = AemPdu {
            message_type: aecp_msg::AEM_COMMAND,
            status: aecp_status::SUCCESS,
            target_entity_id: p.target,
            controller_entity_id: self.controller_id,
            sequence_id: seq,
            unsolicited: false,
            command_type: p.command_type,
            payload: p.payload,
        }
        .encode();
        self.inflight.push(InFlight {
            target: p.target,
            target_mac: p.target_mac,
            sequence_id: seq,
            command_type: p.command_type,
            frame: frame.clone(),
            deadline_ns: now_ns.saturating_add(TIMEOUT_NS),
            retried: false,
            in_progress_ns: 0,
        });
        self.last_send_ns = now_ns;
        Some(AecpSend {
            target_mac: p.target_mac,
            frame,
            sequence_id: seq,
            command_type: p.command_type,
        })
    }

    /// Fold a received AECP response. Matches on
    /// (target_entity_id, sequence_id, command_type) (§9.2). IN_PROGRESS
    /// re-arms the timer; any other status completes the command.
    pub fn on_rx(&mut self, resp: &AemPdu, now_ns: u64) -> Option<AecpOutcome> {
        if !resp.is_response() {
            return None;
        }
        let pos = self.inflight.iter().position(|f| {
            f.target == resp.target_entity_id
                && f.sequence_id == resp.sequence_id
                && f.command_type == resp.command_type
        })?;
        if resp.status == aecp_status::IN_PROGRESS {
            if let Some(f) = self.inflight.get_mut(pos) {
                let add = IN_PROGRESS_NS.min(IN_PROGRESS_CAP_NS.saturating_sub(f.in_progress_ns));
                f.in_progress_ns = f.in_progress_ns.saturating_add(IN_PROGRESS_NS);
                if add == 0 {
                    // Cap reached: treat the next timeout as final.
                    f.retried = true;
                    f.deadline_ns = now_ns.saturating_add(TIMEOUT_NS);
                } else {
                    f.deadline_ns = now_ns.saturating_add(add);
                }
            }
            return None;
        }
        let f = self.inflight.remove(pos);
        let _ = f;
        Some(AecpOutcome::Response(resp.clone()))
    }

    /// Age deadlines. A first timeout retries once (new deadline, same
    /// frame/seq per §9.2 "one retry"); a second removes the command and
    /// reports [`AecpOutcome::TimedOut`]. Returns the resends to put on the
    /// wire and the final timeouts.
    pub fn on_tick(&mut self, now_ns: u64) -> (Vec<AecpSend>, Vec<AecpOutcome>) {
        let mut resends = Vec::new();
        let mut timeouts = Vec::new();
        let mut keep = Vec::new();
        for mut f in std::mem::take(&mut self.inflight) {
            if now_ns < f.deadline_ns {
                keep.push(f);
                continue;
            }
            if !f.retried {
                f.retried = true;
                f.deadline_ns = now_ns.saturating_add(TIMEOUT_NS);
                resends.push(AecpSend {
                    target_mac: f.target_mac,
                    frame: f.frame.clone(),
                    sequence_id: f.sequence_id,
                    command_type: f.command_type,
                });
                keep.push(f);
            } else {
                timeouts.push(AecpOutcome::TimedOut {
                    command_type: f.command_type,
                    sequence_id: f.sequence_id,
                });
            }
        }
        self.inflight = keep;
        (resends, timeouts)
    }

    /// Earliest in-flight deadline, for the daemon's next-wake computation.
    pub fn next_deadline_ns(&self) -> Option<u64> {
        self.inflight.iter().map(|f| f.deadline_ns).min()
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::constants::aem_cmd;
    use crate::milan::mvu_cmd;

    fn ids() -> (Eui64, Eui64) {
        (
            Eui64::from_u64(0x644b_f0ff_fe12_73b8), // controller
            Eui64::from_u64(0x000A_9203_3BA9_0001), // target
        )
    }

    #[test]
    fn aem_round_trip() {
        let (ctrl, tgt) = ids();
        let p = AemPdu {
            message_type: aecp_msg::AEM_COMMAND,
            status: aecp_status::SUCCESS,
            target_entity_id: tgt,
            controller_entity_id: ctrl,
            sequence_id: 0x0042,
            unsolicited: false,
            command_type: aem_cmd::READ_DESCRIPTOR,
            payload: vec![0, 0, 0, 0],
        };
        let bytes = p.encode();
        assert_eq!(bytes.first(), Some(&subtype::AECP)); // byte0 = subtype
        assert_eq!(bytes.get(1), Some(&aecp_msg::AEM_COMMAND)); // byte1 = message_type
        assert_eq!(AemPdu::decode(&bytes), Some(p));
    }

    #[test]
    fn aem_unsolicited_bit() {
        let (ctrl, tgt) = ids();
        let mut p = AemPdu {
            message_type: aecp_msg::AEM_RESPONSE,
            status: aecp_status::SUCCESS,
            target_entity_id: tgt,
            controller_entity_id: ctrl,
            sequence_id: 1,
            unsolicited: true,
            command_type: aem_cmd::GET_STREAM_INFO,
            payload: vec![],
        };
        let d = AemPdu::decode(&p.encode()).unwrap();
        assert!(d.unsolicited);
        assert_eq!(d.command_type, aem_cmd::GET_STREAM_INFO);
        p.unsolicited = false;
        assert!(!AemPdu::decode(&p.encode()).unwrap().unsolicited);
    }

    #[test]
    fn mvu_round_trip_and_proto_guard() {
        let (ctrl, tgt) = ids();
        let m = MvuPdu {
            message_type: aecp_msg::VENDOR_UNIQUE_COMMAND,
            status: aecp_status::SUCCESS,
            target_entity_id: tgt,
            controller_entity_id: ctrl,
            sequence_id: 7,
            command_type: mvu_cmd::GET_MILAN_INFO,
            payload: vec![],
        };
        let bytes = m.encode();
        assert_eq!(MvuPdu::decode(&bytes), Some(m));
        // A VENDOR_UNIQUE PDU with a different protocol id is not MVU.
        let mut bad = bytes.clone();
        if let Some(b) = bad.get_mut(24) {
            *b = 0xAA;
        }
        assert_eq!(MvuPdu::decode(&bad), None);
    }

    #[test]
    fn engine_paces_and_tracks_inflight() {
        let (ctrl, tgt) = ids();
        let mac = MacAddr([0, 0x0a, 0x92, 3, 0x3b, 0xa9]);
        let mut e = AecpEngine::new(ctrl);
        e.enqueue(tgt, mac, aem_cmd::READ_DESCRIPTOR, vec![0; 4]);
        e.enqueue(tgt, mac, aem_cmd::GET_STREAM_INFO, vec![0; 4]);
        // First send at t=0.
        let s0 = e.poll(0).expect("first send");
        assert_eq!(s0.command_type, aem_cmd::READ_DESCRIPTOR);
        assert_eq!(e.in_flight_count(), 1);
        // Second command to same target cannot go until the first completes.
        assert!(e.poll(MIN_SEND_GAP_NS).is_none());
        // Respond to the first; now the second can go.
        let resp = AemPdu {
            message_type: aecp_msg::AEM_RESPONSE,
            status: aecp_status::SUCCESS,
            target_entity_id: tgt,
            controller_entity_id: ctrl,
            sequence_id: s0.sequence_id,
            unsolicited: false,
            command_type: s0.command_type,
            payload: vec![],
        };
        assert!(matches!(
            e.on_rx(&resp, 1_000),
            Some(AecpOutcome::Response(_))
        ));
        assert_eq!(e.in_flight_count(), 0);
        let s1 = e.poll(2 * MIN_SEND_GAP_NS + 1).expect("second send");
        assert_eq!(s1.command_type, aem_cmd::GET_STREAM_INFO);
        assert_ne!(s1.sequence_id, s0.sequence_id); // seq advances
    }

    #[test]
    fn engine_retries_once_then_times_out() {
        let (ctrl, tgt) = ids();
        let mac = MacAddr([0, 0x0a, 0x92, 3, 0x3b, 0xa9]);
        let mut e = AecpEngine::new(ctrl);
        e.enqueue(tgt, mac, aem_cmd::GET_COUNTERS, vec![]);
        let s = e.poll(0).unwrap();
        // First timeout -> one resend, still in flight.
        let (resends, outs) = e.on_tick(TIMEOUT_NS);
        assert_eq!(resends.len(), 1);
        assert_eq!(resends.first().map(|r| r.sequence_id), Some(s.sequence_id));
        assert!(outs.is_empty());
        assert_eq!(e.in_flight_count(), 1);
        // Second timeout -> final TimedOut, removed.
        let (resends, outs) = e.on_tick(2 * TIMEOUT_NS);
        assert!(resends.is_empty());
        assert_eq!(
            outs.first(),
            Some(&AecpOutcome::TimedOut {
                command_type: aem_cmd::GET_COUNTERS,
                sequence_id: s.sequence_id
            })
        );
        assert_eq!(e.in_flight_count(), 0);
    }

    #[test]
    fn engine_in_progress_resets_timer() {
        let (ctrl, tgt) = ids();
        let mac = MacAddr([0, 0x0a, 0x92, 3, 0x3b, 0xa9]);
        let mut e = AecpEngine::new(ctrl);
        e.enqueue(tgt, mac, aem_cmd::READ_DESCRIPTOR, vec![]);
        let s = e.poll(0).unwrap();
        let ip = AemPdu {
            message_type: aecp_msg::AEM_RESPONSE,
            status: aecp_status::IN_PROGRESS,
            target_entity_id: tgt,
            controller_entity_id: ctrl,
            sequence_id: s.sequence_id,
            unsolicited: false,
            command_type: s.command_type,
            payload: vec![],
        };
        // IN_PROGRESS does not complete the command, and pushes the deadline.
        assert!(e.on_rx(&ip, 10_000_000).is_none());
        assert_eq!(e.in_flight_count(), 1);
        // No timeout at the original 250 ms deadline anymore.
        let (resends, outs) = e.on_tick(TIMEOUT_NS);
        assert!(resends.is_empty() && outs.is_empty());
    }
}
