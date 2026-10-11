// SPDX-License-Identifier: MIT OR Apache-2.0

//! avb-coreaudio: a CoreAudio AudioServerPlugIn (HAL plug-in) that will expose
//! the 16R's AVB streams as a macOS input device (Phase 1b).
//!
//! `coreaudiod` loads the `.driver` bundle, reads `CFPlugInFactories` from the
//! `Info.plist`, and calls our factory [`avb_coreaudio_factory`]. The factory
//! returns a pointer to an [`abi::AudioServerPlugInDriverInterface`] whose
//! first slots are the COM `IUnknown` methods and whose rest are the HAL
//! driver entry points. This crate is a `cdylib`; the bundle is assembled by
//! `scripts/package.sh` (Phase 1b).
//!
//! STATUS: scaffold. The object model (plug-in -> device -> input stream) and
//! property answers are implemented enough for the HAL to publish an
//! 8-channel 48 kHz 32-bit input device; the IO path returns silence until the
//! recovered-audio ring is wired (1b-1), and CFString properties (UID/name)
//! are added with the CoreFoundation bindings in a follow-up.

#![allow(non_snake_case, non_upper_case_globals)]

mod abi;
mod plugin;

use std::os::raw::c_void;

use abi::*;

// --- COM plumbing -----------------------------------------------------------
//
// The HAL only ever QueryInterface's for the AudioServerPlugIn driver UUID or
// IUnknown; both resolve to our single static interface. We are a process-wide
// singleton, so AddRef/Release just return a non-zero refcount.

extern "C" fn query_interface(
    _this: *mut c_void,
    _iid: CFUUIDBytes,
    out: *mut *mut c_void,
) -> OSStatus {
    if out.is_null() {
        return kAudioHardwareIllegalOperationError;
    }
    // Hand back our interface pointer for any requested UUID (the HAL asks
    // only for the driver / IUnknown UUIDs). It must be the SAME shape the
    // factory returns: a pointer to the pointer-to-vtable (COM `interface**`).
    // Writing `&DRIVER_INTERFACE` (a single pointer straight at the vtable)
    // makes the HAL dereference once and read `_reserved` (0) as the vtable
    // pointer, then call through 0x10 (AddRef) -> SIGSEGV at 0x10.
    unsafe {
        *out = &DRIVER_INTERFACE_PTR as *const _ as *mut c_void;
    }
    kAudioHardwareNoError
}

extern "C" fn add_ref(_this: *mut c_void) -> u32 {
    1
}
extern "C" fn release(_this: *mut c_void) -> u32 {
    1
}

// --- Unused vtable slots (real signatures, no-op bodies) --------------------

extern "C" fn create_device(
    _d: AudioServerPlugInDriverRef,
    _desc: CFDictionaryRef,
    _ci: *const c_void,
    _out: *mut AudioObjectID,
) -> OSStatus {
    kAudioHardwareIllegalOperationError // we publish a fixed device, not dynamic
}
extern "C" fn destroy_device(_d: AudioServerPlugInDriverRef, _id: AudioObjectID) -> OSStatus {
    kAudioHardwareIllegalOperationError
}
extern "C" fn add_device_client(
    _d: AudioServerPlugInDriverRef,
    _id: AudioObjectID,
    _ci: *const c_void,
) -> OSStatus {
    kAudioHardwareNoError
}
extern "C" fn remove_device_client(
    _d: AudioServerPlugInDriverRef,
    _id: AudioObjectID,
    _ci: *const c_void,
) -> OSStatus {
    kAudioHardwareNoError
}
extern "C" fn perform_config_change(
    _d: AudioServerPlugInDriverRef,
    _id: AudioObjectID,
    _a: u64,
    _i: *mut c_void,
) -> OSStatus {
    kAudioHardwareNoError
}
extern "C" fn abort_config_change(
    _d: AudioServerPlugInDriverRef,
    _id: AudioObjectID,
    _a: u64,
    _i: *mut c_void,
) -> OSStatus {
    kAudioHardwareNoError
}
extern "C" fn get_zero_time_stamp(
    _d: AudioServerPlugInDriverRef,
    _dev: AudioObjectID,
    _c: UInt32,
    out_sample_time: *mut Float64,
    out_host_time: *mut u64,
    out_seed: *mut u64,
) -> OSStatus {
    // The HAL models device timing as (sampleTime, hostTime) pairs spaced
    // ZeroTimeStampPeriod apart. They come from ZtsClock, which keeps the
    // timeline continuous across media-clock rate changes; the lock makes the
    // pair consistent under concurrent calls.
    //
    // Cross-clock correction: frames arrive on the 16R MEDIA clock (disciplined
    // rate_hz_vs_mach, e.g. ~47994 Hz at −118 ppm), not the Mac host clock's
    // nominal 48 kHz. Reporting the true rate keeps the HAL's device timeline
    // aligned with actual data arrival; falls back to nominal when the ring is
    // unattached or the media clock is not yet locked.
    const NOMINAL_RATE_HZ: f64 = 48_000.0;

    // True media-clock rate from the ring when locked, else nominal. Read
    // BEFORE taking the clock lock (it takes the IO lock internally).
    let rate_hz = match crate::plugin::ring_disciplined_rate_hz() {
        Some(r) if r.is_finite() && r > 0.0 => r,
        _ => NOMINAL_RATE_HZ,
    };

    let (numer, denom) = timebase();
    let (sample_time, host_time) = {
        let Ok(mut clk) = ZTS_CLOCK.lock() else {
            // Only reachable if a prior holder panicked (poisoned mutex).
            return kAudioHardwareBadObjectError;
        };
        // `now` is read under the lock so concurrent callers are serialised in
        // time order and the timeline can never be observed going backwards.
        let now = unsafe { mach_absolute_time() };
        let base = *clk.base_ticks.get_or_insert(now);
        let now_ns = now.wrapping_sub(base) as f64 * numer as f64 / denom as f64;
        let (st, boundary_ns) = clk.clock.step(now_ns, rate_hz);
        let ticks = (boundary_ns * denom as f64 / numer as f64).max(0.0) as u64;
        (st, base.wrapping_add(ticks))
    };

    unsafe {
        if !out_sample_time.is_null() {
            *out_sample_time = sample_time;
        }
        if !out_host_time.is_null() {
            *out_host_time = host_time;
        }
        if !out_seed.is_null() {
            // The timeline is continuous across rate changes (ZtsClock
            // re-anchors), so there is never a discontinuity to signal.
            *out_seed = 1;
        }
    }
    kAudioHardwareNoError
}

/// Shared zero-timestamp state: the mach tick of the first call (all math is
/// in ns relative to it, keeping f64 precision) and the timeline clock.
struct ZtsState {
    base_ticks: Option<u64>,
    clock: ZtsClock,
}

static ZTS_CLOCK: std::sync::Mutex<ZtsState> = std::sync::Mutex::new(ZtsState {
    base_ticks: None,
    clock: ZtsClock::new(),
});

/// The device timeline behind GetZeroTimeStamp (review H3).
///
/// The old code computed `sample_time = elapsed_since_anchor × rate_now`, so
/// any change in the ring's disciplined rate retroactively rewrote ALL past
/// time — at −100 ppm after an hour it jumped back 16384 frames (two periods),
/// a discontinuity the HAL was never told about. This clock instead keeps an
/// (anchor_ns, anchor_frames, rate) basis and, when the rate changes,
/// re-anchors at the current position computed with the OLD rate. Position is
/// therefore continuous and non-decreasing; a rate change only bends the slope
/// from now on. Pure (no clock reads) so it is unit-testable.
struct ZtsClock {
    init: bool,
    anchor_ns: f64,
    anchor_frames: f64,
    rate_hz: f64,
}

/// ZeroTimeStampPeriod (frames). Must match the device property.
const ZTS_PERIOD: f64 = 8192.0;
/// Ignore rate changes below this relative size (0.1 ppm) so DLL jitter does
/// not re-anchor on every call.
const ZTS_RATE_EPS: f64 = 1e-7;

impl ZtsClock {
    const fn new() -> Self {
        Self {
            init: false,
            anchor_ns: 0.0,
            anchor_frames: 0.0,
            rate_hz: 48_000.0,
        }
    }

    /// Advance to `now_ns` at `rate_hz`. Returns (sample_time of the current
    /// period boundary, the ns time of that boundary on the same timeline).
    fn step(&mut self, now_ns: f64, rate_hz: f64) -> (f64, f64) {
        if !self.init {
            self.init = true;
            self.anchor_ns = now_ns;
            self.anchor_frames = 0.0;
            self.rate_hz = rate_hz;
        } else if (rate_hz - self.rate_hz).abs() > self.rate_hz * ZTS_RATE_EPS {
            // Re-anchor at the current position under the OLD rate, then adopt
            // the new rate going forward.
            let pos = self.position(now_ns);
            self.anchor_ns = now_ns.max(self.anchor_ns);
            self.anchor_frames = pos;
            self.rate_hz = rate_hz;
        }
        let pos = self.position(now_ns);
        let sample_time = (pos / ZTS_PERIOD).floor() * ZTS_PERIOD;
        let boundary_ns =
            self.anchor_ns + (sample_time - self.anchor_frames) * 1_000_000_000.0 / self.rate_hz;
        (sample_time, boundary_ns)
    }

    fn position(&self, now_ns: f64) -> f64 {
        let dt = (now_ns - self.anchor_ns).max(0.0);
        self.anchor_frames + dt * self.rate_hz / 1_000_000_000.0
    }
}

extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

/// Mach timebase (numer/denom): host ticks * numer/denom = nanoseconds.
fn timebase() -> (u32, u32) {
    let mut tb = MachTimebaseInfo { numer: 0, denom: 0 };
    unsafe {
        mach_timebase_info(&mut tb);
    }
    if tb.numer == 0 || tb.denom == 0 {
        (1, 1)
    } else {
        (tb.numer, tb.denom)
    }
}
extern "C" fn will_do_io_operation(
    _d: AudioServerPlugInDriverRef,
    _dev: AudioObjectID,
    _c: UInt32,
    op: UInt32,
    out_will_do: *mut Boolean,
    out_in_place: *mut Boolean,
) -> OSStatus {
    // Claim ONLY the operations DoIOOperation implements (review C2). Saying
    // yes to everything made the HAL hand us ops like ProcessOutput/ConvertMix
    // that we silently ignored, leaving any non-in-place buffer unfilled.
    let will = op_is_handled(op);
    unsafe {
        if !out_will_do.is_null() {
            *out_will_do = Boolean::from(will);
        }
        if !out_in_place.is_null() {
            *out_in_place = Boolean::from(will);
        }
    }
    kAudioHardwareNoError
}

/// The IO operations this plug-in performs: ReadInput (input stream) and
/// WriteMix (output stream). Both run in place on the HAL's buffer.
fn op_is_handled(op: UInt32) -> bool {
    op == kAudioServerPlugInIOOperationReadInput || op == kAudioServerPlugInIOOperationWriteMix
}

extern "C" fn begin_io_operation(
    _d: AudioServerPlugInDriverRef,
    _dev: AudioObjectID,
    _c: UInt32,
    _op: UInt32,
    _frames: UInt32,
    _cycle: *const c_void,
) -> OSStatus {
    kAudioHardwareNoError
}
extern "C" fn end_io_operation(
    _d: AudioServerPlugInDriverRef,
    _dev: AudioObjectID,
    _c: UInt32,
    _op: UInt32,
    _frames: UInt32,
    _cycle: *const c_void,
) -> OSStatus {
    kAudioHardwareNoError
}

// --- The static driver interface and the pointer-to-pointer the HAL holds ---

static DRIVER_INTERFACE: AudioServerPlugInDriverInterface = AudioServerPlugInDriverInterface {
    _reserved: 0,
    QueryInterface: query_interface,
    AddRef: add_ref,
    Release: release,
    Initialize: plugin::initialize,
    CreateDevice: create_device,
    DestroyDevice: destroy_device,
    AddDeviceClient: add_device_client,
    RemoveDeviceClient: remove_device_client,
    PerformDeviceConfigurationChange: perform_config_change,
    AbortDeviceConfigurationChange: abort_config_change,
    HasProperty: plugin::has_property,
    IsPropertySettable: plugin::is_property_settable,
    GetPropertyDataSize: plugin::get_property_data_size,
    GetPropertyData: plugin::get_property_data,
    SetPropertyData: plugin::set_property_data,
    StartIO: plugin::start_io,
    StopIO: plugin::stop_io,
    GetZeroTimeStamp: get_zero_time_stamp,
    WillDoIOOperation: will_do_io_operation,
    BeginIOOperation: begin_io_operation,
    DoIOOperation: plugin::do_io_operation,
    EndIOOperation: end_io_operation,
};

/// The HAL holds a pointer to a pointer to the interface (COM convention).
static DRIVER_INTERFACE_PTR: &AudioServerPlugInDriverInterface = &DRIVER_INTERFACE;

/// The CFPlugIn factory. Named in `Info.plist` `CFPlugInFactories` by UUID;
/// `coreaudiod` calls it with the requested type UUID and expects a pointer to
/// our interface (as a `*mut *const vtable`). The `_alloc`/`_type` args are
/// the CFAllocator and the requested CFUUID; we ignore them and always return
/// the one driver interface.
///
/// # Safety
/// Called by `coreaudiod` across the C ABI. Returns a pointer to a
/// process-static interface, valid for the lifetime of the loaded bundle.
#[no_mangle]
pub extern "C" fn avb_coreaudio_factory(
    _alloc: CFAllocatorRef,
    _requested_type: CFUUIDRef,
) -> *mut c_void {
    abi::oslog("[avb-coreaudio] factory called");
    // Return &(&DRIVER_INTERFACE): the HAL dereferences once to reach the
    // vtable, matching the COM `IUnknown**` convention.
    &DRIVER_INTERFACE_PTR as *const _ as *mut c_void
}

#[cfg(test)]
mod io_op_tests {
    use super::*;

    /// Regression (review H3): with the old stateless math a rate change
    /// recomputed all elapsed time at the new rate, so sample_time jumped
    /// backwards. The timeline must stay non-decreasing and continuous.
    #[test]
    fn zts_timeline_never_goes_backwards_on_rate_change() {
        let mut c = ZtsClock::new();
        let hour_ns = 3_600.0e9;
        let (st0, _) = c.step(0.0, 48_000.0);
        assert_eq!(st0, 0.0);
        let (st1, b1) = c.step(hour_ns, 48_000.0);
        // Media clock locks at -100 ppm: the old code jumped back 2 periods.
        let slow = 48_000.0 * (1.0 - 100e-6);
        let (st2, _) = c.step(hour_ns + 1.0, slow);
        assert!(st2 >= st1, "sample_time went backwards: {st1} -> {st2}");
        assert!(st2 - st1 <= ZTS_PERIOD, "sample_time jumped forward");
        // Fallback to nominal (unlock) must not jump forward either.
        let (st3, _) = c.step(hour_ns + 2.0, 48_000.0);
        assert!(st3 >= st2 && st3 - st2 <= ZTS_PERIOD);
        // The boundary pair is self-consistent: boundary precedes/equals now.
        assert!(b1 <= hour_ns);
    }

    #[test]
    fn zts_advances_one_period_per_period_of_time() {
        let mut c = ZtsClock::new();
        c.step(0.0, 48_000.0);
        let period_ns = ZTS_PERIOD / 48_000.0 * 1e9;
        let (st, b) = c.step(period_ns * 3.5, 48_000.0);
        assert_eq!(st, 3.0 * ZTS_PERIOD);
        assert!((b - 3.0 * period_ns).abs() < 1.0);
    }

    #[test]
    fn zts_ignores_sub_eps_rate_jitter() {
        let mut c = ZtsClock::new();
        c.step(0.0, 48_000.0);
        c.step(1e9, 48_000.0 * (1.0 + 1e-9));
        assert_eq!(c.anchor_ns, 0.0, "tiny jitter must not re-anchor");
    }

    #[test]
    fn only_read_input_and_write_mix_are_claimed() {
        assert!(op_is_handled(kAudioServerPlugInIOOperationReadInput));
        assert!(op_is_handled(kAudioServerPlugInIOOperationWriteMix));
        for other in [b"thrd", b"cycl", b"proc", b"mixo", b"cvtm", b"cvti"] {
            assert!(!op_is_handled(u32::from_be_bytes(*other)));
        }
    }
}
