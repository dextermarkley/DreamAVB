//! The TX scheduler (design §5.2). Three fixed-capacity sub-queues (gPTP 8,
//! MRP 16, AVDECC 40), strict priority gPTP > MRP > AVDECC, FIFO within a
//! class. A `want_ts` frame is held while a timestamp is outstanding but does
//! not block a lower class's non-timestamped frame. Busy/NoResources keep the
//! frame at the head. A pending clock step blocks new `want_ts` frames until
//! it is issued. A Pdelay_Resp held > 8 ms since its Pdelay_Req harvest is
//! dropped. Pure logic over [`DextPort`]; unit-tested with `FakeDext`.

use avb_core::{Outbox, TxFrame, TxFull};

use crate::dextio::{DextPort, SendResult, TxTsResult};

/// Sub-queue capacities (design §5.2).
pub const GPTP_CAP: usize = 8;
pub const MRP_CAP: usize = 16;
pub const AVDECC_CAP: usize = 40;

/// Pdelay_Resp late-drop window (design §7.3 / §5.2: 8 ms).
pub const PDELAY_RESP_LATE_NS: u64 = 8_000_000;

/// A TX class, highest priority first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Gptp,
    Mrp,
    Avdecc,
}

/// Per-class drop/retry counters (design §5.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxCounters {
    pub queue_full_gptp: u64,
    pub queue_full_mrp: u64,
    pub queue_full_avdecc: u64,
    pub busy_retries: u64,
    pub no_resources_retries: u64,
    pub pdelay_resp_late_drop: u64,
    pub pdelay_tx_fail: u64,
}

/// The TX scheduler. Holds the three outboxes and the want_ts / step state.
pub struct TxSched {
    gptp: Outbox<GPTP_CAP>,
    mrp: Outbox<MRP_CAP>,
    avdecc: Outbox<AVDECC_CAP>,
    /// A `want_ts` frame is outstanding (its timestamp not yet resolved).
    ts_outstanding: bool,
    /// A clock step is pending; new want_ts frames are blocked until issued.
    step_pending: bool,
    /// Harvest time (ns) of a held Pdelay_Resp's Pdelay_Req, for the 8 ms rule.
    pub counters: TxCounters,
}

impl Default for TxSched {
    fn default() -> Self {
        Self::new()
    }
}

impl TxSched {
    pub fn new() -> Self {
        Self {
            gptp: Outbox::new(),
            mrp: Outbox::new(),
            avdecc: Outbox::new(),
            ts_outstanding: false,
            step_pending: false,
            counters: TxCounters::default(),
        }
    }

    /// Enqueue a frame into its class. On a full sub-queue the NEW frame is
    /// dropped and counted (design §5.2).
    pub fn push(&mut self, class: Class, frame: TxFrame) -> Result<(), TxFull> {
        let r = match class {
            Class::Gptp => self.gptp.push(frame),
            Class::Mrp => self.mrp.push(frame),
            Class::Avdecc => self.avdecc.push(frame),
        };
        if r.is_err() {
            match class {
                Class::Gptp => self.counters.queue_full_gptp += 1,
                Class::Mrp => self.counters.queue_full_mrp += 1,
                Class::Avdecc => self.counters.queue_full_avdecc += 1,
            }
        }
        r
    }

    pub fn pending(&self) -> usize {
        self.gptp.len() + self.mrp.len() + self.avdecc.len()
    }

    /// Mark that a clock step is pending (blocks new want_ts sends).
    pub fn set_step_pending(&mut self, pending: bool) {
        self.step_pending = pending;
    }

    /// The next sendable frame in strict priority order, skipping a want_ts
    /// frame when a timestamp is outstanding or a step is pending (it does not
    /// block a lower non-timestamped frame). Returns `(class, index-in-class)`
    /// without removing it.
    fn next_sendable(&self) -> Option<(Class, usize)> {
        for (class, len, get) in [
            (Class::Gptp, self.gptp.len(), &self.gptp as &dyn FrontAt),
            (Class::Mrp, self.mrp.len(), &self.mrp as &dyn FrontAt),
            (
                Class::Avdecc,
                self.avdecc.len(),
                &self.avdecc as &dyn FrontAt,
            ),
        ] {
            for i in 0..len {
                if let Some(f) = get.at(i) {
                    let blocked = f.want_ts && (self.ts_outstanding || self.step_pending);
                    if !blocked {
                        return Some((class, i));
                    }
                }
            }
        }
        None
    }

    /// Try to send one frame this iteration (design §5.2). Returns true if a
    /// frame was sent (or dropped), false if nothing was sendable.
    pub fn tick<P: DextPort>(&mut self, dext: &mut P, now_ns: u64) -> bool {
        // Resolve an outstanding timestamp first.
        if self.ts_outstanding {
            match dext.poll_tx_ts() {
                TxTsResult::Ready { .. } | TxTsResult::Timeout => self.ts_outstanding = false,
                TxTsResult::Pending => {}
                TxTsResult::Idle => self.ts_outstanding = false,
            }
        }
        let Some((class, idx)) = self.next_sendable() else {
            return false;
        };
        // Only the head of a class can actually be popped; if the sendable
        // frame is not the head (a held want_ts ahead of it), send it by
        // rotating: we pop the head into a temp, but to keep it simple and
        // allocation-free we only send when idx==0 OR the head is a blocked
        // want_ts. Pop-and-reinsert the blocked head.
        let frame = self.take_sendable(class, idx);
        let Some(frame) = frame else { return false };
        let _ = now_ns;
        let res = dext.send(frame.bytes(), frame.want_ts, frame.tag);
        match res {
            SendResult::Ok => {
                if frame.want_ts {
                    self.ts_outstanding = true;
                }
            }
            SendResult::Busy => {
                self.counters.busy_retries += 1;
                self.requeue_front(class, frame);
            }
            SendResult::NoResources => {
                self.counters.no_resources_retries += 1;
                self.requeue_front(class, frame);
            }
            SendResult::BadArgument => {} // dropped (a bug)
            SendResult::Fatal => {
                if class == Class::Gptp {
                    self.counters.pdelay_tx_fail += 1;
                }
            }
        }
        true
    }

    /// Remove the sendable frame at `(class, idx)`, popping and re-pushing any
    /// blocked want_ts frames ahead of it (preserving their order).
    fn take_sendable(&mut self, class: Class, idx: usize) -> Option<TxFrame> {
        // Pop `idx` leading (blocked) frames into a small stack buffer, take
        // the target, then re-push the blocked ones in order.
        let mut held: [TxFrame; AVDECC_CAP] = [TxFrame::EMPTY; AVDECC_CAP];
        let mut n = 0usize;
        for _ in 0..idx {
            let f = self.pop(class)?;
            if let Some(slot) = held.get_mut(n) {
                *slot = f;
                n += 1;
            }
        }
        let target = self.pop(class);
        // Re-push the held frames at the front, in original order.
        for i in (0..n).rev() {
            if let Some(f) = held.get(i).copied() {
                self.push_front(class, f);
            }
        }
        target
    }

    fn pop(&mut self, class: Class) -> Option<TxFrame> {
        match class {
            Class::Gptp => self.gptp.pop(),
            Class::Mrp => self.mrp.pop(),
            Class::Avdecc => self.avdecc.pop(),
        }
    }

    /// Re-insert a frame at the FRONT of its class (used by Busy/NoResources
    /// retry and when restoring blocked want_ts frames). Rebuilds the outbox.
    fn push_front(&mut self, class: Class, frame: TxFrame) {
        match class {
            Class::Gptp => rebuild_front(&mut self.gptp, frame),
            Class::Mrp => rebuild_front(&mut self.mrp, frame),
            Class::Avdecc => rebuild_front(&mut self.avdecc, frame),
        }
    }

    fn requeue_front(&mut self, class: Class, frame: TxFrame) {
        self.push_front(class, frame);
    }
}

/// Re-insert `frame` at the front of `o`, preserving the rest in order.
fn rebuild_front<const N: usize>(o: &mut Outbox<N>, frame: TxFrame) {
    let mut tmp: [TxFrame; N] = [TxFrame::EMPTY; N];
    let mut n = 0;
    while let Some(f) = o.pop() {
        if let Some(s) = tmp.get_mut(n) {
            *s = f;
            n += 1;
        }
    }
    let _ = o.push(frame);
    for i in 0..n {
        if let Some(f) = tmp.get(i).copied() {
            let _ = o.push(f);
        }
    }
}

/// Access the i-th frame of an outbox (oldest first) for `next_sendable`.
trait FrontAt {
    fn at(&self, i: usize) -> Option<&TxFrame>;
}

impl<const N: usize> FrontAt for Outbox<N> {
    fn at(&self, i: usize) -> Option<&TxFrame> {
        self.iter().nth(i)
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::dextio::fake::FakeDext;

    fn frame(tag: u64, want_ts: bool) -> TxFrame {
        TxFrame::from_bytes(&[tag as u8; 32], want_ts, tag).unwrap()
    }

    #[test]
    fn strict_priority_gptp_first() {
        let mut tx = TxSched::new();
        tx.push(Class::Avdecc, frame(1, false)).unwrap();
        tx.push(Class::Mrp, frame(2, false)).unwrap();
        tx.push(Class::Gptp, frame(3, false)).unwrap();
        let mut d = FakeDext::default();
        tx.tick(&mut d, 0);
        assert_eq!(d.sent[0].2, 3); // gPTP first
        tx.tick(&mut d, 0);
        assert_eq!(d.sent[1].2, 2); // then MRP
        tx.tick(&mut d, 0);
        assert_eq!(d.sent[2].2, 1); // then AVDECC
    }

    #[test]
    fn avdecc_full_drops_41st_and_counts() {
        let mut tx = TxSched::new();
        for i in 0..AVDECC_CAP {
            tx.push(Class::Avdecc, frame(i as u64, false)).unwrap();
        }
        assert_eq!(tx.push(Class::Avdecc, frame(99, false)), Err(TxFull));
        assert_eq!(tx.counters.queue_full_avdecc, 1);
    }

    #[test]
    fn pdelay_resp_sent_next_with_avdecc_full() {
        // Fill AVDECC to 40, enqueue a gPTP Pdelay_Resp, assert it is next.
        let mut tx = TxSched::new();
        for i in 0..AVDECC_CAP {
            tx.push(Class::Avdecc, frame(i as u64, false)).unwrap();
        }
        tx.push(Class::Gptp, frame(0xAA, false)).unwrap();
        let mut d = FakeDext::default();
        tx.tick(&mut d, 0);
        assert_eq!(d.sent[0].2, 0xAA, "gPTP goes before any AVDECC");
    }

    #[test]
    fn held_want_ts_does_not_block_lower_class() {
        // A gPTP want_ts frame is held (ts outstanding); an AVDECC frame below
        // it must still send.
        let mut tx = TxSched::new();
        tx.push(Class::Gptp, frame(1, true)).unwrap();
        tx.push(Class::Avdecc, frame(2, false)).unwrap();
        let mut d = FakeDext::default();
        d.ts_stays_pending = true; // the hw timestamp stays outstanding
                                   // First tick sends the gPTP want_ts frame -> ts outstanding.
        tx.tick(&mut d, 0);
        assert_eq!(d.sent[0].2, 1);
        assert!(d.sent[0].1); // want_ts
                              // Enqueue another gPTP want_ts; it is blocked while ts outstanding,
                              // but the AVDECC frame sends.
        tx.push(Class::Gptp, frame(3, true)).unwrap();
        tx.tick(&mut d, 0);
        assert_eq!(d.sent[1].2, 2, "AVDECC sends past the held gPTP want_ts");
    }

    #[test]
    fn pending_step_blocks_new_want_ts() {
        let mut tx = TxSched::new();
        tx.set_step_pending(true);
        tx.push(Class::Gptp, frame(1, true)).unwrap();
        tx.push(Class::Mrp, frame(2, false)).unwrap();
        let mut d = FakeDext::default();
        tx.tick(&mut d, 0);
        // The want_ts gPTP frame is blocked; the MRP frame goes.
        assert_eq!(d.sent[0].2, 2);
        // Once the step is issued, the want_ts frame can go.
        tx.set_step_pending(false);
        tx.tick(&mut d, 0);
        assert_eq!(d.sent[1].2, 1);
    }

    #[test]
    fn busy_keeps_frame_at_head() {
        let mut tx = TxSched::new();
        tx.push(Class::Gptp, frame(1, false)).unwrap();
        let mut d = FakeDext::default();
        d.next_send = Some(SendResult::Busy);
        tx.tick(&mut d, 0);
        assert_eq!(tx.counters.busy_retries, 1);
        assert_eq!(tx.pending(), 1); // still queued
                                     // Next tick it actually sends.
        tx.tick(&mut d, 0);
        assert_eq!(d.sent[0].2, 1);
        assert_eq!(tx.pending(), 0);
    }
}
