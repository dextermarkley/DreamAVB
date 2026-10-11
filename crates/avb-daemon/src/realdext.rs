//! `RealDext`: the production [`DextPort`] over `avb_dext::Device` (design
//! §5.2, §5.3). The start/shutdown sequencing and the mapped-ring consumer
//! live in the stack wiring; this type is the per-call command bridge. It is
//! exercised only on hardware (the test gate uses `FakeDext`), so it carries
//! no unit tests.

use avb_core::MachTicks;
use avb_dext::abi::mem;
use avb_dext::abi::{StreamFilter, StreamFilterEntry, FLAG_LINK_UP};
use avb_dext::iokit::{io_return_of, is_device_removed, Device, MemMap};
use avb_dext::shm::{Ring, RxRings};
use avb_gptp::msg::ETHERTYPE_PTP;

use crate::dextio::{DextPort, Health, RxSlot, SendResult, TxTsResult};

// kIOReturn codes we classify (design §5.2 / §13.1).
const K_IO_RETURN_BUSY: u32 = 0xe00002e8;
const K_IO_RETURN_NO_RESOURCES: u32 = 0xe00002be;
const K_IO_RETURN_BAD_ARGUMENT: u32 = 0xe00002c2;

/// The production dext port. Holds the open [`Device`] and the last requested
/// TX-timestamp sequence; the mapped rings are owned by the stack wiring.
pub struct RealDext {
    dev: Device,
    /// (hw sequence used to query GetTxTimestamp, owner tag to return).
    pending_ts: Option<(u64, u64)>,
    timebase_numer: u32,
    timebase_denom: u32,
    /// Updated by the stack from the mapped shm header each ctl tick.
    pub producer_running: bool,
    pub last_tick_mach: MachTicks,
    /// Mapped RX memory (ShmHeader + ctl/audio slots, read-only) and the
    /// consumer tails (read-write). `None` until `attach_rings` succeeds. The
    /// maps must outlive every `RxRings` view built from them.
    rx_map: Option<MemMap>,
    consumer_map: Option<MemMap>,
    /// Shm mapping generation, pinned at attach for slot validation.
    shm_generation: u64,
}

impl RealDext {
    pub fn new(dev: Device, timebase_numer: u32, timebase_denom: u32) -> Self {
        Self {
            dev,
            pending_ts: None,
            timebase_numer,
            timebase_denom,
            producer_running: false,
            last_tick_mach: MachTicks(0),
            rx_map: None,
            consumer_map: None,
            shm_generation: 0,
        }
    }

    /// Map the RX memory (read-only) and the consumer tails (read-write) and
    /// pin the mapping generation after validating the header. Call once after
    /// `RingAttach` + `InitDatapath` (design §5.2 steps 4–6). Without this,
    /// `rx_ctl` has nothing to drain.
    pub fn attach_rings(&mut self, generation: u64) -> Result<(), &'static str> {
        let rx = self.dev.map_memory(mem::RX).map_err(|_| "map RX memory")?;
        let consumer = self
            .dev
            .map_memory(mem::CONSUMER)
            .map_err(|_| "map consumer memory")?;
        // Validate the header layout against the pinned generation.
        // SAFETY: the maps are valid for their reported lengths for as long as
        // they are held in `self`; the dext follows the ring protocol.
        let rings =
            unsafe { RxRings::from_raw(rx.as_ptr(), rx.len(), consumer.as_ptr(), consumer.len()) }
                .map_err(|_| "ring layout mismatch")?;
        rings
            .validate(generation)
            .map_err(|_| "shm header mismatch")?;
        self.rx_map = Some(rx);
        self.consumer_map = Some(consumer);
        self.shm_generation = generation;
        Ok(())
    }

    pub fn device(&self) -> &Device {
        &self.dev
    }

    pub fn timebase(&self) -> (u32, u32) {
        (self.timebase_numer, self.timebase_denom)
    }

    /// The dext's current clock generation from the mapped ShmHeader, or 0 if
    /// the rings are not mapped. The gPTP port must be seeded with this so RX
    /// slot generations match and exchanges are not discarded as step-straddles.
    pub fn clock_gen(&self) -> u32 {
        let (Some(rx), Some(consumer)) = (self.rx_map.as_ref(), self.consumer_map.as_ref()) else {
            return 0;
        };
        // SAFETY: maps held in self for their reported lengths; dext follows
        // the ring protocol.
        match unsafe { RxRings::from_raw(rx.as_ptr(), rx.len(), consumer.as_ptr(), consumer.len()) }
        {
            Ok(r) => r.clock_gen(),
            Err(_) => 0,
        }
    }
}

impl DextPort for RealDext {
    fn send(&mut self, frame: &[u8], want_ts: bool, tag: u64) -> SendResult {
        match self.dev.send_frame(frame, want_ts) {
            Ok((seq, _)) => {
                if want_ts {
                    // Keep the hw sequence to query GetTxTimestamp, and the
                    // caller's tag to route the result back to its owner (the
                    // Pdelay exchange, §5.2 / §7.2).
                    self.pending_ts = Some((seq, tag));
                }
                SendResult::Ok
            }
            Err(e) => match io_return_of(&e) {
                Some(K_IO_RETURN_BUSY) => SendResult::Busy,
                Some(K_IO_RETURN_NO_RESOURCES) => SendResult::NoResources,
                Some(K_IO_RETURN_BAD_ARGUMENT) => SendResult::BadArgument,
                _ if is_device_removed(&e) => SendResult::Fatal,
                _ => SendResult::Fatal,
            },
        }
    }

    fn poll_tx_ts(&mut self) -> TxTsResult {
        let Some((seq, tag)) = self.pending_ts else {
            return TxTsResult::Idle;
        };
        match self.dev.get_tx_timestamp(seq) {
            Ok((status, systim_ns, _)) => match status {
                avb_dext::abi::TX_TS_OK => {
                    self.pending_ts = None;
                    TxTsResult::Ready { tag, systim_ns }
                }
                avb_dext::abi::TX_TS_PENDING => TxTsResult::Pending,
                // TIMEOUT / NOT_REQUESTED: the exchange is abandoned.
                _ => {
                    self.pending_ts = None;
                    TxTsResult::Timeout
                }
            },
            Err(_) => {
                self.pending_ts = None;
                TxTsResult::Timeout
            }
        }
    }

    fn read_clock(&mut self) -> (MachTicks, u64) {
        match self.dev.read_clock(1) {
            Ok(samples) => match samples.first() {
                // mach_before/mach_after bracket the SYSTIM read; use the
                // midpoint as the mach instant of eav_ns.
                Some(s) => (
                    MachTicks(s.mach_before.wrapping_add(s.mach_after) / 2),
                    s.eav_ns,
                ),
                None => (MachTicks(0), 0),
            },
            Err(_) => (MachTicks(0), 0),
        }
    }

    fn step(&mut self, delta_ns: i64) {
        let _ = self.dev.step_clock(delta_ns);
    }

    fn set_clock_abs(&mut self, ns: u64) {
        let _ = self.dev.set_clock(ns);
    }

    fn adj_freq(&mut self, ppb: i64) {
        let _ = self.dev.adj_freq(ppb);
    }

    fn set_filter(&mut self, streams: &[(u64, [u8; 6], u16)]) {
        let entries: Vec<StreamFilterEntry> = streams
            .iter()
            .map(|&(_sid, dest_mac, vid)| StreamFilterEntry { dest_mac, vid })
            .collect();
        if let Some(filter) = StreamFilter::from_entries(&entries) {
            let _ = self.dev.set_stream_filter(&filter);
        }
    }

    fn rx_ctl(&mut self) -> Vec<RxSlot> {
        // Primary path: drain the mapped control ring (design §5.2, §14.2).
        // After RingAttach the dext delivers RX frames into the shared-memory
        // ctl ring, NOT the PollRx batch queue, so this is where gPTP frames
        // arrive. Fall back to PollRx only when the rings are not mapped.
        if let (Some(rx), Some(consumer)) = (self.rx_map.as_ref(), self.consumer_map.as_ref()) {
            // SAFETY: the maps are held in `self` for their reported lengths;
            // the dext follows the ring protocol. A fresh view each call just
            // re-reads the consumer tails.
            let mut rings = match unsafe {
                RxRings::from_raw(rx.as_ptr(), rx.len(), consumer.as_ptr(), consumer.len())
            } {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            let gen = rings.generation();
            let mut out = Vec::new();
            rings.drain(Ring::Ctl, avb_dext::abi::RX_BATCH_MAX, |slot| {
                if let Ok(s) = slot {
                    out.push(RxSlot {
                        cls: s.cls() as u8,
                        data: s.data().to_vec(),
                        hw_ts_ns: s.hw_ts(),
                        clock_gen: s.clock_gen(),
                        step_ambig: s.step_ambig(),
                        desc_vlan_tci: s.desc_vlan_tci(),
                    });
                }
            });
            let _ = gen;
            return out;
        }

        // Fallback: IOKit PollRx filtered to the PTP ethertype.
        let batch = match self.dev.poll_rx(ETHERTYPE_PTP, avb_dext::abi::RX_BATCH_MAX) {
            Ok(b) => b,
            Err(_) => return Vec::new(),
        };
        batch
            .entries()
            .iter()
            .map(|e| RxSlot {
                cls: avb_dext::abi::RxClass::Gptp as u8,
                data: e.bytes().to_vec(),
                hw_ts_ns: (e.has_ts != 0).then_some(e.hw_ts),
                clock_gen: 0,
                step_ambig: false,
                desc_vlan_tci: None,
            })
            .collect()
    }

    fn rx_audio(&mut self) -> Vec<RxSlot> {
        // Drain the mapped audio ring (design §5.2, §10.6). Stream AAF frames
        // land here after the stream filter is set and ACMP binds the stream.
        let (Some(rx), Some(consumer)) = (self.rx_map.as_ref(), self.consumer_map.as_ref()) else {
            return Vec::new();
        };
        // SAFETY: maps held in self for their reported lengths; dext follows
        // the ring protocol.
        let mut rings = match unsafe {
            RxRings::from_raw(rx.as_ptr(), rx.len(), consumer.as_ptr(), consumer.len())
        } {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        rings.drain(Ring::Audio, avb_dext::abi::RX_BATCH_MAX, |slot| {
            if let Ok(s) = slot {
                out.push(RxSlot {
                    cls: s.cls() as u8,
                    data: s.data().to_vec(),
                    hw_ts_ns: s.hw_ts(),
                    clock_gen: s.clock_gen(),
                    step_ambig: s.step_ambig(),
                    desc_vlan_tci: s.desc_vlan_tci(),
                });
            }
        });
        out
    }

    fn health(&self) -> Health {
        match self.dev.info() {
            Ok(info) => Health {
                fault: info.fault,
                link_up: info.flags & FLAG_LINK_UP != 0,
                stage: info.stage,
                producer_running: self.producer_running,
                last_tick_mach: self.last_tick_mach,
            },
            Err(_) => Health {
                fault: u32::MAX,
                link_up: false,
                stage: 0,
                producer_running: false,
                last_tick_mach: MachTicks(0),
            },
        }
    }
}
