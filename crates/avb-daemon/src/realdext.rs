//! `RealDext`: the production [`DextPort`] over `avb_dext::Device` (design
//! §5.2, §5.3). The start/shutdown sequencing and the mapped-ring consumer
//! live in the stack wiring; this type is the per-call command bridge. It is
//! exercised only on hardware (the test gate uses `FakeDext`), so it carries
//! no unit tests.

use avb_core::MachTicks;
use avb_dext::abi::{StreamFilter, StreamFilterEntry, FLAG_LINK_UP};
use avb_dext::iokit::{io_return_of, is_device_removed, Device};

use crate::dextio::{DextPort, Health, RxSlot, SendResult, TxTsResult};

// kIOReturn codes we classify (design §5.2 / §13.1).
const K_IO_RETURN_BUSY: u32 = 0xe00002e8;
const K_IO_RETURN_NO_RESOURCES: u32 = 0xe00002be;
const K_IO_RETURN_BAD_ARGUMENT: u32 = 0xe00002c2;

/// The production dext port. Holds the open [`Device`] and the last requested
/// TX-timestamp sequence; the mapped rings are owned by the stack wiring.
pub struct RealDext {
    dev: Device,
    pending_ts_seq: Option<u64>,
    timebase_numer: u32,
    timebase_denom: u32,
    /// Updated by the stack from the mapped shm header each ctl tick.
    pub producer_running: bool,
    pub last_tick_mach: MachTicks,
}

impl RealDext {
    pub fn new(dev: Device, timebase_numer: u32, timebase_denom: u32) -> Self {
        Self {
            dev,
            pending_ts_seq: None,
            timebase_numer,
            timebase_denom,
            producer_running: false,
            last_tick_mach: MachTicks(0),
        }
    }

    pub fn device(&self) -> &Device {
        &self.dev
    }

    pub fn timebase(&self) -> (u32, u32) {
        (self.timebase_numer, self.timebase_denom)
    }
}

impl DextPort for RealDext {
    fn send(&mut self, frame: &[u8], want_ts: bool, tag: u64) -> SendResult {
        match self.dev.send_frame(frame, want_ts) {
            Ok((seq, _)) => {
                if want_ts {
                    self.pending_ts_seq = Some(seq.max(tag).min(seq));
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
        let Some(seq) = self.pending_ts_seq else {
            return TxTsResult::Idle;
        };
        match self.dev.get_tx_timestamp(seq) {
            Ok((0, systim_ns, _)) => {
                self.pending_ts_seq = None;
                TxTsResult::Ready {
                    tag: seq,
                    systim_ns,
                }
            }
            Ok(_) => TxTsResult::Pending,
            Err(_) => {
                self.pending_ts_seq = None;
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
        // The real control-ring drain is done by the ctl thread over the
        // mapped `RxRings` (the stack owns the mapping). RealDext is the
        // command bridge, so this returns empty.
        Vec::new()
    }

    fn rx_audio(&mut self) -> Vec<RxSlot> {
        Vec::new()
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
