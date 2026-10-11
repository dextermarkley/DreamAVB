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
    // RING_FRAMES apart. Both are computed as a pure function of `now` and a
    // single captured anchor, so the returned pair is always mutually
    // consistent even under concurrent calls (no per-call counter to race on).
    //
    // Cross-clock correction: frames arrive on the 16R MEDIA clock (disciplined
    // rate_hz_vs_mach, e.g. ~47994 Hz at −118 ppm), not the Mac host clock's
    // nominal 48 kHz. Reporting the true rate keeps the HAL's device timeline
    // aligned with actual data arrival; falls back to nominal when the ring is
    // unattached or the media clock is not yet locked.
    use std::sync::atomic::{AtomicU64, Ordering};
    static ANCHOR_HOST: AtomicU64 = AtomicU64::new(0);

    const RING_FRAMES: f64 = 8192.0;
    const NOMINAL_RATE_HZ: f64 = 48_000.0;

    let now = unsafe { mach_absolute_time() };
    let (numer, denom) = timebase();

    // True media-clock rate from the ring when locked, else nominal.
    let rate_hz = match crate::plugin::ring_disciplined_rate_hz() {
        Some(r) if r > 0.0 => r,
        _ => NOMINAL_RATE_HZ,
    };

    let anchor = match ANCHOR_HOST.compare_exchange(0, now, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(_) => now,              // first call: we just set it
        Err(existing) => existing, // subsequent calls
    };
    // host ticks -> nanoseconds elapsed since the anchor.
    let elapsed_ns = (now.wrapping_sub(anchor)) as f64 * numer as f64 / denom as f64;
    // frames elapsed at the TRUE rate.
    let elapsed_frames = elapsed_ns * rate_hz / 1_000_000_000.0;
    // Round DOWN to the start of the current ring period.
    let periods = (elapsed_frames / RING_FRAMES).floor();
    let sample_time = periods * RING_FRAMES;
    // Host time of that period boundary, back-converted at the SAME true rate
    // so (sampleTime, hostTime) stay consistent.
    let period_ns = periods * RING_FRAMES / rate_hz * 1_000_000_000.0;
    let host_time = anchor + (period_ns * denom as f64 / numer as f64) as u64;

    unsafe {
        if !out_sample_time.is_null() {
            *out_sample_time = sample_time;
        }
        if !out_host_time.is_null() {
            *out_host_time = host_time;
        }
        if !out_seed.is_null() {
            *out_seed = 1;
        }
    }
    kAudioHardwareNoError
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
    _op: UInt32,
    out_will_do: *mut Boolean,
    out_in_place: *mut Boolean,
) -> OSStatus {
    unsafe {
        if !out_will_do.is_null() {
            *out_will_do = 1;
        }
        if !out_in_place.is_null() {
            *out_in_place = 1;
        }
    }
    kAudioHardwareNoError
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
