// SPDX-License-Identifier: MIT OR Apache-2.0

//! Thin IOKit wrapper around the AVB dext user client (AVBI210Driver or
//! AVBBcmDriver; same ABI).
//!
//! One typed method per selector in `abi::sel`, plus the shared-memory
//! mapping calls. All math lives elsewhere.
//!
//! Failed calls return an `anyhow::Error` wrapping [`IoReturnError`], so a
//! caller can recover the raw `IOReturn` with [`io_return_of`] (e.g. the
//! daemon's ExclusiveAccess → exit 10 and device-removed → exit 7 rules).

use std::ffi::{c_void, CString};
use std::fmt;
use std::mem::size_of;

use anyhow::{anyhow, bail, Result};
use io_kit_sys::types::{io_connect_t, io_service_t, kIOMapAnywhere};
use io_kit_sys::{
    IOConnectCallMethod, IOConnectMapMemory64, IOConnectUnmapMemory64, IOObjectRelease,
    IOServiceClose, IOServiceGetMatchingService, IOServiceNameMatching, IOServiceOpen,
};

use crate::abi::{self, sel, AvbInfo, ClockSample, Counters, RxBatch, StreamFilter};
use crate::nic::{self, NicProfile};

/// `IOReturn` / `kern_return_t` values the AVB tools name or act on.
pub mod kr {
    pub const ERROR: u32 = 0xe000_02bc;
    pub const NO_MEMORY: u32 = 0xe000_02bd;
    pub const NO_RESOURCES: u32 = 0xe000_02be;
    pub const NO_DEVICE: u32 = 0xe000_02c0;
    pub const NOT_PRIVILEGED: u32 = 0xe000_02c1;
    pub const BAD_ARGUMENT: u32 = 0xe000_02c2;
    pub const EXCLUSIVE_ACCESS: u32 = 0xe000_02c5;
    pub const UNSUPPORTED: u32 = 0xe000_02c7;
    pub const IO_ERROR: u32 = 0xe000_02ca;
    pub const NOT_OPEN: u32 = 0xe000_02cd;
    pub const BUSY: u32 = 0xe000_02d5;
    pub const TIMEOUT: u32 = 0xe000_02d6;
    pub const NOT_READY: u32 = 0xe000_02d8;
    pub const NOT_ATTACHED: u32 = 0xe000_02d9;
    pub const NOT_PERMITTED: u32 = 0xe000_02e2;
    pub const NOT_RESPONDING: u32 = 0xe000_02ed;
    pub const NOT_FOUND: u32 = 0xe000_02f0;
    /// mach_msg: the user client's port is dead (dext gone).
    pub const MACH_SEND_INVALID_DEST: u32 = 0x1000_0003;
}

/// Symbolic name of an `IOReturn`, "unknown" if not one we list.
pub fn kr_name(kr: u32) -> &'static str {
    match kr {
        kr::ERROR => "kIOReturnError",
        kr::NO_MEMORY => "kIOReturnNoMemory",
        kr::NO_RESOURCES => "kIOReturnNoResources",
        kr::NO_DEVICE => "kIOReturnNoDevice",
        kr::NOT_PRIVILEGED => "kIOReturnNotPrivileged",
        kr::BAD_ARGUMENT => "kIOReturnBadArgument",
        kr::EXCLUSIVE_ACCESS => "kIOReturnExclusiveAccess",
        kr::UNSUPPORTED => "kIOReturnUnsupported",
        kr::IO_ERROR => "kIOReturnIOError",
        kr::NOT_OPEN => "kIOReturnNotOpen",
        kr::BUSY => "kIOReturnBusy",
        kr::TIMEOUT => "kIOReturnTimeout",
        kr::NOT_READY => "kIOReturnNotReady",
        kr::NOT_ATTACHED => "kIOReturnNotAttached",
        kr::NOT_PERMITTED => "kIOReturnNotPermitted",
        kr::NOT_RESPONDING => "kIOReturnNotResponding",
        kr::NOT_FOUND => "kIOReturnNotFound",
        kr::MACH_SEND_INVALID_DEST => "MACH_SEND_INVALID_DEST",
        _ => "unknown",
    }
}

/// The device (or its dext) went away: kIOReturnNoDevice, NotResponding,
/// NotAttached or MACH_SEND_INVALID_DEST (design §5.2, daemon exit 7).
pub fn is_device_removed_kr(kr: u32) -> bool {
    matches!(
        kr,
        kr::NO_DEVICE | kr::NOT_RESPONDING | kr::NOT_ATTACHED | kr::MACH_SEND_INVALID_DEST
    )
}

/// A failed IOKit call: which call and the raw return code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoReturnError {
    pub what: String,
    pub kr: u32,
}

impl fmt::Display for IoReturnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} failed: 0x{:08x} ({})",
            self.what,
            self.kr,
            kr_name(self.kr)
        )
    }
}

impl std::error::Error for IoReturnError {}

/// The `IOReturn` behind `err` (anywhere in its context chain), if any.
pub fn io_return_of(err: &anyhow::Error) -> Option<u32> {
    err.chain()
        .find_map(|e| e.downcast_ref::<IoReturnError>())
        .map(|e| e.kr)
}

/// `err` came from an IOKit call that reported the device removed.
pub fn is_device_removed(err: &anyhow::Error) -> bool {
    io_return_of(err).is_some_and(is_device_removed_kr)
}

/// A shared-memory region mapped into this task with `IOConnectMapMemory64`.
/// Unmap it with [`Device::unmap_memory`] (design §5.3 order); closing the
/// connection or exiting also tears it down.
#[derive(Debug)]
pub struct MemMap {
    mem_type: u32,
    addr: u64,
    len: u64,
}

impl MemMap {
    pub fn mem_type(&self) -> u32 {
        self.mem_type
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.addr as usize as *mut u8
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Which dext service to open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NicSelect {
    /// AVBI210Driver first, then AVBBcmDriver.
    Auto,
    I210,
    Bcm,
}
/// kIOMainPortDefault (MACH_PORT_NULL).
const MAIN_PORT_DEFAULT: u32 = 0;

pub struct Device {
    conn: io_connect_t,
    profile: &'static NicProfile,
}

/// Look up a service by its registry name; 0 when absent.
fn find_service(name: &str) -> Result<io_service_t> {
    let name = CString::new(name).expect("static name");
    unsafe {
        let matching = IOServiceNameMatching(name.as_ptr());
        if matching.is_null() {
            bail!("IOServiceNameMatching returned NULL");
        }
        // IOServiceGetMatchingService consumes the matching dictionary.
        Ok(IOServiceGetMatchingService(
            MAIN_PORT_DEFAULT,
            matching as _,
        ))
    }
}

fn kr_check(kr: i32, what: &str) -> Result<()> {
    if kr == 0 {
        return Ok(());
    }
    Err(IoReturnError {
        what: what.to_owned(),
        kr: kr as u32,
    }
    .into())
}

impl Device {
    /// Find the selected dext service, open user client type 0 and check that
    /// the dext reports the NIC kind the service name promises.
    pub fn open(sel: NicSelect) -> Result<Self> {
        let candidates: &[&'static NicProfile] = match sel {
            NicSelect::Auto => &nic::ALL,
            NicSelect::I210 => &[&nic::I210],
            NicSelect::Bcm => &[&nic::BCM57762],
        };
        let mut found = None;
        for p in candidates {
            let s = find_service(p.service_name)?;
            if s != 0 {
                found = Some((s, *p));
                break;
            }
        }
        let Some((service, profile)) = found else {
            if let [p] = candidates {
                bail!(
                    "{} service not found (is {} activated and matched to the {} adapter? see \
                     `ioreg -l -w0 | grep -A20 {}`)",
                    p.service_name,
                    p.dext_bundle_id,
                    p.match_desc,
                    p.service_name
                );
            }
            bail!(
                "neither AVBI210Driver nor AVBBcmDriver service found (is a dext activated and \
                 matched? see `ioreg -l -w0 | grep -A20 AVB`)"
            );
        };
        let mut conn: io_connect_t = 0;
        let kr = unsafe {
            let kr = IOServiceOpen(service, mach2::traps::mach_task_self(), 0, &mut conn);
            IOObjectRelease(service);
            kr
        };
        kr_check(kr, "IOServiceOpen")?;
        let dev = Self { conn, profile };
        let info = dev.info()?;
        if info.nic_kind != profile.kind {
            bail!(
                "{} reports nic_kind {} but the {} profile expects {}",
                profile.service_name,
                info.nic_kind,
                profile.label,
                profile.kind
            );
        }
        Ok(dev)
    }

    /// Open the AVBI210Driver service (the only NIC with ABI v3 rings).
    pub fn open_i210() -> Result<Self> {
        Self::open(NicSelect::I210)
    }

    pub fn profile(&self) -> &'static NicProfile {
        self.profile
    }

    fn call(
        &self,
        selector: u32,
        scalars_in: &[u64],
        struct_in: &[u8],
        scalars_out: &mut [u64],
        struct_out: Option<(*mut c_void, usize)>,
        what: &str,
    ) -> Result<(u32, usize)> {
        let mut out_cnt = scalars_out.len() as u32;
        let (out_ptr, mut out_size) = struct_out.unwrap_or((std::ptr::null_mut(), 0));
        let kr = unsafe {
            IOConnectCallMethod(
                self.conn,
                selector,
                if scalars_in.is_empty() {
                    std::ptr::null()
                } else {
                    scalars_in.as_ptr()
                },
                scalars_in.len() as u32,
                if struct_in.is_empty() {
                    std::ptr::null()
                } else {
                    struct_in.as_ptr().cast()
                },
                struct_in.len(),
                if scalars_out.is_empty() {
                    std::ptr::null_mut()
                } else {
                    scalars_out.as_mut_ptr()
                },
                &mut out_cnt,
                out_ptr,
                if out_ptr.is_null() {
                    std::ptr::null_mut()
                } else {
                    &mut out_size
                },
            )
        };
        kr_check(kr, what)?;
        Ok((out_cnt, out_size))
    }

    pub fn info(&self) -> Result<AvbInfo> {
        let mut info = AvbInfo::default();
        let (_, n) = self.call(
            sel::GET_INFO,
            &[],
            &[],
            &mut [],
            Some(((&mut info as *mut AvbInfo).cast(), size_of::<AvbInfo>())),
            "GetInfo",
        )?;
        if n != size_of::<AvbInfo>() {
            bail!(
                "GetInfo returned {n} bytes, expected {}",
                size_of::<AvbInfo>()
            );
        }
        if !abi::abi_supported(info.abi_version) {
            bail!(
                "dext ABI version {} outside the supported range {}..={}",
                info.abi_version,
                abi::ABI_VERSION_MIN,
                abi::ABI_VERSION
            );
        }
        Ok(info)
    }

    pub fn read_clock(&self, n: usize) -> Result<Vec<ClockSample>> {
        let n = n.clamp(1, abi::READ_CLOCK_MAX_SAMPLES);
        let mut buf = vec![ClockSample::default(); n];
        let (_, got) = self.call(
            sel::READ_CLOCK,
            &[n as u64],
            &[],
            &mut [],
            Some((buf.as_mut_ptr().cast(), n * size_of::<ClockSample>())),
            "ReadClock",
        )?;
        buf.truncate(got / size_of::<ClockSample>());
        if buf.is_empty() {
            return Err(anyhow!("ReadClock returned no samples"));
        }
        Ok(buf)
    }

    /// Load the NIC clock; returns the immediate readback.
    pub fn set_clock(&self, ns: u64) -> Result<u64> {
        let mut out = [0u64; 1];
        self.call(sel::SET_CLOCK, &[ns], &[], &mut out, None, "SetClock")?;
        Ok(out[0])
    }

    /// Program the frequency correction; returns the raw register value written.
    pub fn adj_freq(&self, ppb: i64) -> Result<u32> {
        if ppb.abs() > self.profile.max_ppb {
            bail!(
                "|ppb| must be <= {} on the {}",
                self.profile.max_ppb,
                self.profile.label
            );
        }
        let mut out = [0u64; 1];
        self.call(sel::ADJ_FREQ, &[ppb as u64], &[], &mut out, None, "AdjFreq")?;
        Ok(out[0] as u32)
    }

    pub fn init_datapath(&self, flags: u64) -> Result<u32> {
        let mut out = [0u64; 1];
        self.call(
            sel::INIT_DATAPATH,
            &[flags],
            &[],
            &mut out,
            None,
            "InitDatapath",
        )?;
        Ok(out[0] as u32)
    }

    pub fn shutdown_datapath(&self) -> Result<()> {
        self.call(
            sel::SHUTDOWN_DATAPATH,
            &[],
            &[],
            &mut [],
            None,
            "ShutdownDatapath",
        )?;
        Ok(())
    }

    /// Drain up to `max` frames whose ethertype matches (0 = all).
    pub fn poll_rx(&self, ethertype: u16, max: usize) -> Result<Box<RxBatch>> {
        let mut batch = Box::<RxBatch>::default();
        let max = max.clamp(1, abi::RX_BATCH_MAX);
        self.call(
            sel::POLL_RX,
            &[u64::from(ethertype), max as u64],
            &[],
            &mut [],
            Some(((&mut *batch as *mut RxBatch).cast(), size_of::<RxBatch>())),
            "PollRx",
        )?;
        Ok(batch)
    }

    /// Queue a frame (no FCS). Returns (tx_seq, mach_at_doorbell).
    pub fn send_frame(&self, frame: &[u8], want_ts: bool) -> Result<(u64, u64)> {
        if frame.len() > abi::FRAME_MAX_LEN {
            bail!("frame too long ({} > {})", frame.len(), abi::FRAME_MAX_LEN);
        }
        let mut out = [0u64; 2];
        self.call(
            sel::SEND_FRAME,
            &[u64::from(want_ts)],
            frame,
            &mut out,
            None,
            "SendFrame",
        )?;
        Ok((out[0], out[1]))
    }

    /// Queue up to [`abi::tx_batch::MAX`] frames (no FCS, no timestamps) in
    /// ONE user-client call. Returns (accepted, mach_at_doorbell).
    ///
    /// `accepted < frames.len()` is normal back-pressure, not an error: the
    /// dext queues in order and stops at the first frame the token bucket or
    /// the TX ring refuses. The caller re-paces the remainder.
    ///
    /// Requires an ABI >= [`abi::ABI_VERSION_SEND_FRAMES`] dext; check with
    /// [`abi::abi_has_send_frames`] before calling, since a v3 dext has no
    /// such selector and fails the call.
    pub fn send_frames(&self, frames: &[&[u8]]) -> Result<(u32, u64)> {
        let mut buf = [0u8; abi::tx_batch::BYTES_MAX];
        let n = abi::tx_batch::encode(&mut buf, frames)?;
        let mut out = [0u64; 2];
        self.call(
            sel::SEND_FRAMES,
            &[],
            &buf[..n],
            &mut out,
            None,
            "SendFrames",
        )?;
        Ok((out[0] as u32, out[1]))
    }

    /// Returns (status, hw_ts, mach_latched).
    pub fn get_tx_timestamp(&self, seq: u64) -> Result<(u64, u64, u64)> {
        let mut out = [0u64; 3];
        self.call(
            sel::GET_TX_TIMESTAMP,
            &[seq],
            &[],
            &mut out,
            None,
            "GetTxTimestamp",
        )?;
        Ok((out[0], out[1], out[2]))
    }

    pub fn read_reg32(&self, off: u32) -> Result<u32> {
        if off >= self.profile.reg_read_limit || off & 3 != 0 {
            bail!("register offset 0x{off:x} out of range or unaligned");
        }
        let mut out = [0u64; 1];
        self.call(
            sel::READ_REG32,
            &[u64::from(off)],
            &[],
            &mut out,
            None,
            "ReadReg32",
        )?;
        Ok(out[0] as u32)
    }

    /// Step the NIC clock by `delta_ns`. Returns (before_ns, after_ns,
    /// mach_mid): the clock read before the step, the readback after it, and
    /// the mach time the target was computed for.
    pub fn step_clock(&self, delta_ns: i64) -> Result<(u64, u64, u64)> {
        if delta_ns.unsigned_abs() > abi::STEP_CLOCK_MAX_NS.unsigned_abs() {
            bail!(
                "|step| must be <= {} ns, got {delta_ns}",
                abi::STEP_CLOCK_MAX_NS
            );
        }
        let mut out = [0u64; 3];
        self.call(
            sel::STEP_CLOCK,
            &[delta_ns as u64],
            &[],
            &mut out,
            None,
            "StepClock",
        )?;
        Ok((out[0], out[1], out[2]))
    }

    /// Read-only snapshot of the dext counters (works without rings).
    pub fn get_counters(&self) -> Result<Box<Counters>> {
        let mut c = Box::<Counters>::default();
        let (_, n) = self.call(
            sel::GET_COUNTERS,
            &[],
            &[],
            &mut [],
            Some(((&mut *c as *mut Counters).cast(), size_of::<Counters>())),
            "GetCounters",
        )?;
        if n != size_of::<Counters>() {
            bail!(
                "GetCounters returned {n} bytes, expected {}",
                size_of::<Counters>()
            );
        }
        Ok(c)
    }

    /// Replace the dext's stream filter (owner only, rings attached).
    pub fn set_stream_filter(&self, filter: &StreamFilter) -> Result<()> {
        // SAFETY: StreamFilter is repr(C) plain data without padding (136 B,
        // asserted in abi tests); the slice lives only for this call.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (filter as *const StreamFilter).cast::<u8>(),
                size_of::<StreamFilter>(),
            )
        };
        self.call(
            sel::SET_STREAM_FILTER,
            &[],
            bytes,
            &mut [],
            None,
            "SetStreamFilter",
        )?;
        Ok(())
    }

    /// Attach the shared RX rings and become the datapath owner. Returns
    /// (generation, rx_mem_size, consumer_mem_size); the mapped header's
    /// `generation` must equal the first value.
    pub fn ring_attach(&self, flags: u64) -> Result<(u64, u64, u64)> {
        let mut out = [0u64; 3];
        self.call(
            sel::RING_ATTACH,
            &[flags],
            &[],
            &mut out,
            None,
            "RingAttach",
        )?;
        Ok((out[0], out[1], out[2]))
    }

    /// Release the rings and ownership (owner only).
    pub fn ring_detach(&self) -> Result<()> {
        self.call(sel::RING_DETACH, &[], &[], &mut [], None, "RingDetach")?;
        Ok(())
    }

    /// Map memory type `mem_type` (`abi::mem::RX` read-only,
    /// `abi::mem::CONSUMER` read-write) anywhere in this task.
    pub fn map_memory(&self, mem_type: u32) -> Result<MemMap> {
        let mut addr = 0u64;
        let mut len = 0u64;
        let kr = unsafe {
            IOConnectMapMemory64(
                self.conn,
                mem_type,
                mach2::traps::mach_task_self(),
                &mut addr,
                &mut len,
                kIOMapAnywhere as u32,
            )
        };
        kr_check(kr, "IOConnectMapMemory64")?;
        Ok(MemMap {
            mem_type,
            addr,
            len,
        })
    }

    /// Unmap a region returned by [`Device::map_memory`].
    pub fn unmap_memory(&self, map: MemMap) -> Result<()> {
        let kr = unsafe {
            IOConnectUnmapMemory64(
                self.conn,
                map.mem_type,
                mach2::traps::mach_task_self(),
                map.addr,
            )
        };
        kr_check(kr, "IOConnectUnmapMemory64")
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            IOServiceClose(self.conn);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn return_codes_match_io_kit_sys() {
        use io_kit_sys::ret::*;
        let pairs = [
            (kr::ERROR, kIOReturnError),
            (kr::NO_MEMORY, kIOReturnNoMemory),
            (kr::NO_RESOURCES, kIOReturnNoResources),
            (kr::NO_DEVICE, kIOReturnNoDevice),
            (kr::NOT_PRIVILEGED, kIOReturnNotPrivileged),
            (kr::BAD_ARGUMENT, kIOReturnBadArgument),
            (kr::EXCLUSIVE_ACCESS, kIOReturnExclusiveAccess),
            (kr::UNSUPPORTED, kIOReturnUnsupported),
            (kr::IO_ERROR, kIOReturnIOError),
            (kr::NOT_OPEN, kIOReturnNotOpen),
            (kr::BUSY, kIOReturnBusy),
            (kr::TIMEOUT, kIOReturnTimeout),
            (kr::NOT_READY, kIOReturnNotReady),
            (kr::NOT_ATTACHED, kIOReturnNotAttached),
            (kr::NOT_PERMITTED, kIOReturnNotPermitted),
            (kr::NOT_RESPONDING, kIOReturnNotResponding),
            (kr::NOT_FOUND, kIOReturnNotFound),
        ];
        for (ours, theirs) in pairs {
            assert_eq!(ours, theirs as u32, "{}", kr_name(ours));
            assert_ne!(kr_name(ours), "unknown");
        }
        assert_eq!(
            kr::MACH_SEND_INVALID_DEST,
            mach2::message::MACH_SEND_INVALID_DEST as u32
        );
        // Values the FEAT-001 dext returns.
        assert_eq!(kr::EXCLUSIVE_ACCESS, 0xe00002c5);
        assert_eq!(kr::NOT_PERMITTED, 0xe00002e2);
        assert_eq!(kr::NO_RESOURCES, 0xe00002be);
    }

    #[test]
    fn device_removed_classification() {
        for k in [
            kr::NO_DEVICE,
            kr::NOT_RESPONDING,
            kr::NOT_ATTACHED,
            kr::MACH_SEND_INVALID_DEST,
        ] {
            assert!(is_device_removed_kr(k), "{}", kr_name(k));
        }
        for k in [
            0,
            kr::EXCLUSIVE_ACCESS,
            kr::BUSY,
            kr::NO_RESOURCES,
            kr::NOT_READY,
            kr::BAD_ARGUMENT,
        ] {
            assert!(!is_device_removed_kr(k), "{}", kr_name(k));
        }
        let e = kr_check(kr::NOT_RESPONDING as i32, "SendFrame").unwrap_err();
        assert_eq!(
            e.to_string(),
            "SendFrame failed: 0xe00002ed (kIOReturnNotResponding)"
        );
        assert!(is_device_removed(&e));
        let e = kr_check(kr::EXCLUSIVE_ACCESS as i32, "RingAttach")
            .context("attaching rings")
            .unwrap_err();
        assert_eq!(io_return_of(&e), Some(kr::EXCLUSIVE_ACCESS));
        assert!(!is_device_removed(&e));
        assert_eq!(io_return_of(&anyhow!("plain")), None);
        assert!(kr_check(0, "x").is_ok());
    }
}
