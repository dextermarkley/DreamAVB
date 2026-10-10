//! Thin IOKit wrapper around the AVBBcmDriver user client.
//!
//! One typed method per selector in `abi::sel`. All math lives elsewhere.

use std::ffi::{c_void, CString};
use std::mem::size_of;

use anyhow::{anyhow, bail, Result};
use io_kit_sys::types::{io_connect_t, io_service_t};
use io_kit_sys::{
    IOConnectCallMethod, IOObjectRelease, IOServiceClose, IOServiceGetMatchingService,
    IOServiceNameMatching, IOServiceOpen,
};

use crate::abi::{self, sel, AvbInfo, ClockSample, RxBatch};

/// Registry name of the dext's service (its IOUserClass).
pub const SERVICE_NAME: &str = "AVBBcmDriver";
/// kIOMainPortDefault (MACH_PORT_NULL).
const MAIN_PORT_DEFAULT: u32 = 0;

pub struct Device {
    conn: io_connect_t,
}

fn kr_check(kr: i32, what: &str) -> Result<()> {
    if kr == 0 {
        return Ok(());
    }
    let name = match kr as u32 {
        0xe00002bc => "kIOReturnError",
        0xe00002c2 => "kIOReturnBadArgument",
        0xe00002c7 => "kIOReturnUnsupported",
        0xe00002d5 => "kIOReturnBusy",
        0xe00002d6 => "kIOReturnTimeout",
        0xe00002d8 => "kIOReturnNotReady",
        0xe00002e2 => "kIOReturnNotPermitted",
        0xe00002c1 => "kIOReturnNotPrivileged",
        0xe00002ed => "kIOReturnNoDevice",
        0xe00002cd => "kIOReturnNotOpen",
        0xe00002f0 => "kIOReturnNotFound",
        0xe00002bd => "kIOReturnNoMemory",
        0xe00002c5 => "kIOReturnExclusiveAccess",
        0xe00002ca => "kIOReturnIOError",
        _ => "unknown",
    };
    bail!("{what} failed: 0x{:08x} ({name})", kr as u32)
}

impl Device {
    /// Find the AVBBcmDriver service and open user client type 0.
    pub fn open() -> Result<Self> {
        let name = CString::new(SERVICE_NAME).expect("static name");
        let service: io_service_t = unsafe {
            let matching = IOServiceNameMatching(name.as_ptr());
            if matching.is_null() {
                bail!("IOServiceNameMatching returned NULL");
            }
            // IOServiceGetMatchingService consumes the matching dictionary.
            IOServiceGetMatchingService(MAIN_PORT_DEFAULT, matching as _)
        };
        if service == 0 {
            bail!(
                "{SERVICE_NAME} service not found (is the dext activated and matched to the \
                 14e4:1682 adapter? see `ioreg -l -w0 | grep -A20 {SERVICE_NAME}`)"
            );
        }
        let mut conn: io_connect_t = 0;
        let kr = unsafe {
            let kr = IOServiceOpen(service, mach2::traps::mach_task_self(), 0, &mut conn);
            IOObjectRelease(service);
            kr
        };
        kr_check(kr, "IOServiceOpen")?;
        Ok(Self { conn })
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
        if info.abi_version != abi::ABI_VERSION {
            bail!(
                "dext ABI version {} != probe ABI version {}",
                info.abi_version,
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

    /// Load the EAV clock; returns the immediate readback.
    pub fn set_clock(&self, ns: u64) -> Result<u64> {
        let mut out = [0u64; 1];
        self.call(sel::SET_CLOCK, &[ns], &[], &mut out, None, "SetClock")?;
        Ok(out[0])
    }

    /// Program the frequency correction; returns the raw register value written.
    pub fn adj_freq(&self, ppb: i64) -> Result<u32> {
        if ppb.abs() > abi::ADJ_FREQ_MAX_PPB {
            bail!("|ppb| must be <= {}", abi::ADJ_FREQ_MAX_PPB);
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
        if off >= abi::REG_READ_LIMIT || off & 3 != 0 {
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
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            IOServiceClose(self.conn);
        }
    }
}
