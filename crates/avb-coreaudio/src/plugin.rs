//! The AudioServerPlugIn driver-interface method implementations — a faithful
//! port of Apple's `NullAudio.c` property model, trimmed to one input-only
//! device.
//!
//! Object graph (every HAL object MUST answer BaseClass/Class/Owner, and the
//! HAL rejects the whole plug-in — "the object is not valid" — if any required
//! property is missing):
//!
//!   plug-in (id 1)
//!     ├─ owns box (id 2)      — must be `Acquired` for the device to publish
//!     └─ owns device (id 3)
//!           └─ owns input stream (id 4)
//!
//! The device is an 8-ch 48 kHz 32-bit signed-integer input. The IO path
//! returns silence until the recovered-audio ring is wired (Phase 1b-1).

#![allow(non_snake_case)]

use std::os::raw::c_void;
use std::ptr;

use crate::abi::*;

// Device identity + format (overridden later from the daemon control API,
// 1b-2). Never hardcode these downstream — read them from here.
const DEVICE_CHANNELS: u32 = 8;
const DEVICE_RATE: f64 = 48_000.0;
const DEVICE_BITS: u32 = 32;
const DEVICE_UID: &str = "com.avbaudio.16r.input";
const DEVICE_MODEL_UID: &str = "com.avbaudio.16r.model";
const DEVICE_NAME: &str = "AVB StudioLive 16R (Input)";
const BOX_UID: &str = "com.avbaudio.16r.box";
const BOX_NAME: &str = "AVB-Audio Box";
const MANUFACTURER: &str = "AVB-Audio";
/// Device ring/zero-timestamp period (frames). Must match the IO ring; a
/// round power-of-two keeps GetZeroTimeStamp arithmetic exact.
const RING_FRAMES: u32 = 8192;
const SAFETY_OFFSET: u32 = 0;
const LATENCY: u32 = 0;

/// Write a value of type `T` into `out_data` if there is room; set the written
/// size. Returns the status.
unsafe fn put<T: Copy>(
    v: T,
    in_data_size: UInt32,
    out_data_size: *mut UInt32,
    out_data: *mut c_void,
) -> OSStatus {
    let need = std::mem::size_of::<T>() as UInt32;
    if in_data_size < need || out_data.is_null() {
        return kAudioHardwareBadPropertySizeError;
    }
    ptr::write_unaligned(out_data as *mut T, v);
    if !out_data_size.is_null() {
        *out_data_size = need;
    }
    kAudioHardwareNoError
}

/// Build the device's ASBD for a given rate (interleaved signed 32-bit PCM).
fn device_asbd(rate: f64) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: rate,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked,
        mBytesPerPacket: DEVICE_CHANNELS * 4,
        mFramesPerPacket: 1,
        mBytesPerFrame: DEVICE_CHANNELS * 4,
        mChannelsPerFrame: DEVICE_CHANNELS,
        mBitsPerChannel: DEVICE_BITS,
        mReserved: 0,
    }
}

pub extern "C" fn initialize(
    _driver: AudioServerPlugInDriverRef,
    _host: AudioServerPlugInHostRef,
) -> OSStatus {
    kAudioHardwareNoError
}

/// Does `object_id` answer `selector`? Mirrors NullAudio's Has*Property.
fn property_known(object_id: AudioObjectID, selector: UInt32) -> bool {
    // Properties every object answers.
    if matches!(
        selector,
        x if x == kAudioObjectPropertyBaseClass
            || x == kAudioObjectPropertyClass
            || x == kAudioObjectPropertyOwner
            || x == kAudioObjectPropertyOwnedObjects
            || x == kAudioObjectPropertyName
    ) {
        return true;
    }
    match object_id {
        K_PLUGIN_OBJECT => matches!(
            selector,
            x if x == kAudioObjectPropertyManufacturer
                || x == kAudioPlugInPropertyBoxList
                || x == kAudioPlugInPropertyTranslateUIDToBox
                || x == kAudioPlugInPropertyDeviceList
                || x == kAudioPlugInPropertyTranslateUIDToDevice
        ),
        K_BOX_OBJECT => matches!(
            selector,
            x if x == kAudioObjectPropertyManufacturer
                || x == kAudioObjectPropertyModelName
                || x == kAudioBoxPropertyBoxUID
                || x == kAudioBoxPropertyAcquired
        ),
        K_DEVICE_OBJECT => matches!(
            selector,
            x if x == kAudioObjectPropertyManufacturer
                || x == kAudioDevicePropertyDeviceUID
                || x == kAudioDevicePropertyModelUID
                || x == kAudioDevicePropertyTransportType
                || x == kAudioDevicePropertyRelatedDevices
                || x == kAudioDevicePropertyClockDomain
                || x == kAudioDevicePropertyDeviceIsAlive
                || x == kAudioDevicePropertyDeviceIsRunning
                || x == kAudioDevicePropertyDeviceCanBeDefaultDevice
                || x == kAudioDevicePropertyDeviceCanBeDefaultSystemDevice
                || x == kAudioDevicePropertyLatency
                || x == kAudioDevicePropertyStreams
                || x == kAudioObjectPropertyControlList
                || x == kAudioDevicePropertySafetyOffset
                || x == kAudioDevicePropertyNominalSampleRate
                || x == kAudioDevicePropertyAvailableNominalSampleRates
                || x == kAudioDevicePropertyIsHidden
                || x == kAudioDevicePropertyZeroTimeStampPeriod
                || x == kAudioDevicePropertyPreferredChannelsForStereo
                || x == kAudioDevicePropertyPreferredChannelLayout
        ),
        K_STREAM_OBJECT => matches!(
            selector,
            x if x == kAudioStreamPropertyDirection
                || x == kAudioStreamPropertyTerminalType
                || x == kAudioStreamPropertyStartingChannel
                || x == kAudioStreamPropertyIsActive
                || x == kAudioStreamPropertyVirtualFormat
                || x == kAudioStreamPropertyPhysicalFormat
                || x == kAudioStreamPropertyAvailableVirtualFormats
                || x == kAudioStreamPropertyAvailablePhysicalFormats
        ),
        _ => false,
    }
}

pub extern "C" fn has_property(
    _driver: AudioServerPlugInDriverRef,
    object_id: AudioObjectID,
    _pid: libc::pid_t,
    address: *const AudioObjectPropertyAddress,
) -> Boolean {
    if address.is_null() {
        return 0;
    }
    let a = unsafe { &*address };
    Boolean::from(property_known(object_id, a.mSelector))
}

pub extern "C" fn is_property_settable(
    _driver: AudioServerPlugInDriverRef,
    object_id: AudioObjectID,
    _pid: libc::pid_t,
    address: *const AudioObjectPropertyAddress,
    out_settable: *mut Boolean,
) -> OSStatus {
    if out_settable.is_null() || address.is_null() {
        return kAudioHardwareIllegalOperationError;
    }
    let a = unsafe { &*address };
    // Only the nominal sample rate is settable (rate change, 1b-2).
    let settable =
        object_id == K_DEVICE_OBJECT && a.mSelector == kAudioDevicePropertyNominalSampleRate;
    unsafe {
        *out_settable = Boolean::from(settable);
    }
    kAudioHardwareNoError
}

pub extern "C" fn get_property_data_size(
    _driver: AudioServerPlugInDriverRef,
    object_id: AudioObjectID,
    _pid: libc::pid_t,
    address: *const AudioObjectPropertyAddress,
    _qd_size: UInt32,
    _qd: *const c_void,
    out_data_size: *mut UInt32,
) -> OSStatus {
    if address.is_null() || out_data_size.is_null() {
        return kAudioHardwareIllegalOperationError;
    }
    let a = unsafe { &*address };
    let u32sz = std::mem::size_of::<UInt32>() as UInt32;
    let f64sz = std::mem::size_of::<Float64>() as UInt32;
    let cfsz = std::mem::size_of::<CFStringRef>() as UInt32;
    let oidsz = std::mem::size_of::<AudioObjectID>() as UInt32;
    let asbdsz = std::mem::size_of::<AudioStreamBasicDescription>() as UInt32;
    let rangedsz = std::mem::size_of::<AudioStreamRangedDescription>() as UInt32;
    let rangesz = std::mem::size_of::<AudioValueRange>() as UInt32;

    let size: UInt32 = match (object_id, a.mSelector) {
        // Object-common.
        (_, s) if s == kAudioObjectPropertyBaseClass || s == kAudioObjectPropertyClass => u32sz,
        (_, s) if s == kAudioObjectPropertyOwner => oidsz,
        (_, s)
            if s == kAudioObjectPropertyName
                || s == kAudioObjectPropertyManufacturer
                || s == kAudioObjectPropertyModelName =>
        {
            cfsz
        }
        // Owned objects.
        (K_PLUGIN_OBJECT, s) if s == kAudioObjectPropertyOwnedObjects => 2 * oidsz,
        (K_DEVICE_OBJECT, s) if s == kAudioObjectPropertyOwnedObjects => oidsz,
        (_, s) if s == kAudioObjectPropertyOwnedObjects => 0,
        // Plug-in.
        (K_PLUGIN_OBJECT, s) if s == kAudioPlugInPropertyBoxList => oidsz,
        (K_PLUGIN_OBJECT, s) if s == kAudioPlugInPropertyDeviceList => oidsz,
        (K_PLUGIN_OBJECT, s)
            if s == kAudioPlugInPropertyTranslateUIDToBox
                || s == kAudioPlugInPropertyTranslateUIDToDevice =>
        {
            oidsz
        }
        // Box.
        (K_BOX_OBJECT, s) if s == kAudioBoxPropertyBoxUID => cfsz,
        (K_BOX_OBJECT, s) if s == kAudioBoxPropertyAcquired => u32sz,
        // Device.
        (K_DEVICE_OBJECT, s)
            if s == kAudioDevicePropertyDeviceUID || s == kAudioDevicePropertyModelUID =>
        {
            cfsz
        }
        (K_DEVICE_OBJECT, s)
            if s == kAudioDevicePropertyTransportType
                || s == kAudioDevicePropertyClockDomain
                || s == kAudioDevicePropertyDeviceIsAlive
                || s == kAudioDevicePropertyDeviceIsRunning
                || s == kAudioDevicePropertyDeviceCanBeDefaultDevice
                || s == kAudioDevicePropertyDeviceCanBeDefaultSystemDevice
                || s == kAudioDevicePropertyLatency
                || s == kAudioDevicePropertySafetyOffset
                || s == kAudioDevicePropertyIsHidden
                || s == kAudioDevicePropertyZeroTimeStampPeriod =>
        {
            u32sz
        }
        (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyRelatedDevices => oidsz,
        (K_DEVICE_OBJECT, s) if s == kAudioObjectPropertyControlList => 0,
        (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyStreams => oidsz,
        (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyNominalSampleRate => f64sz,
        (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyAvailableNominalSampleRates => rangesz,
        (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyPreferredChannelsForStereo => 2 * u32sz,
        (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyPreferredChannelLayout => {
            // AudioChannelLayout header (12 bytes) + N descriptions (20 bytes).
            12 + DEVICE_CHANNELS * 20
        }
        // Stream.
        (K_STREAM_OBJECT, s)
            if s == kAudioStreamPropertyDirection
                || s == kAudioStreamPropertyTerminalType
                || s == kAudioStreamPropertyStartingChannel
                || s == kAudioStreamPropertyIsActive =>
        {
            u32sz
        }
        (K_STREAM_OBJECT, s)
            if s == kAudioStreamPropertyVirtualFormat
                || s == kAudioStreamPropertyPhysicalFormat =>
        {
            asbdsz
        }
        (K_STREAM_OBJECT, s)
            if s == kAudioStreamPropertyAvailableVirtualFormats
                || s == kAudioStreamPropertyAvailablePhysicalFormats =>
        {
            rangedsz
        }
        _ => return kAudioHardwareUnknownPropertyError,
    };
    unsafe {
        *out_data_size = size;
    }
    kAudioHardwareNoError
}

pub extern "C" fn get_property_data(
    _driver: AudioServerPlugInDriverRef,
    object_id: AudioObjectID,
    _pid: libc::pid_t,
    address: *const AudioObjectPropertyAddress,
    _qd_size: UInt32,
    _qd: *const c_void,
    in_data_size: UInt32,
    out_data_size: *mut UInt32,
    out_data: *mut c_void,
) -> OSStatus {
    if address.is_null() {
        return kAudioHardwareIllegalOperationError;
    }
    let a = unsafe { &*address };
    unsafe {
        // --- object-common ---
        match a.mSelector {
            s if s == kAudioObjectPropertyBaseClass => {
                let base = match object_id {
                    K_PLUGIN_OBJECT => kAudioObjectClassID,
                    K_BOX_OBJECT => kAudioObjectClassID,
                    K_DEVICE_OBJECT => kAudioObjectClassID,
                    K_STREAM_OBJECT => kAudioObjectClassID,
                    _ => return kAudioHardwareBadObjectError,
                };
                return put(base, in_data_size, out_data_size, out_data);
            }
            s if s == kAudioObjectPropertyClass => {
                let class = match object_id {
                    K_PLUGIN_OBJECT => kAudioPlugInClassID,
                    K_BOX_OBJECT => kAudioBoxClassID,
                    K_DEVICE_OBJECT => kAudioDeviceClassID,
                    K_STREAM_OBJECT => kAudioStreamClassID,
                    _ => return kAudioHardwareBadObjectError,
                };
                return put(class, in_data_size, out_data_size, out_data);
            }
            s if s == kAudioObjectPropertyOwner => {
                let owner = match object_id {
                    K_PLUGIN_OBJECT => kAudioObjectUnknown, // the plug-in has no owner
                    K_BOX_OBJECT => K_PLUGIN_OBJECT,
                    K_DEVICE_OBJECT => K_PLUGIN_OBJECT,
                    K_STREAM_OBJECT => K_DEVICE_OBJECT,
                    _ => return kAudioHardwareBadObjectError,
                };
                return put(owner, in_data_size, out_data_size, out_data);
            }
            s if s == kAudioObjectPropertyName => {
                let name = match object_id {
                    K_BOX_OBJECT => BOX_NAME,
                    K_DEVICE_OBJECT => DEVICE_NAME,
                    K_STREAM_OBJECT => "AVB 16R Input Stream",
                    _ => "AVB-Audio",
                };
                return put(cfstring(name), in_data_size, out_data_size, out_data);
            }
            s if s == kAudioObjectPropertyManufacturer => {
                return put(
                    cfstring(MANUFACTURER),
                    in_data_size,
                    out_data_size,
                    out_data,
                );
            }
            s if s == kAudioObjectPropertyModelName => {
                return put(cfstring(DEVICE_NAME), in_data_size, out_data_size, out_data);
            }
            _ => {}
        }

        // --- per-object ---
        match (object_id, a.mSelector) {
            // Plug-in owns [box, device].
            (K_PLUGIN_OBJECT, s) if s == kAudioObjectPropertyOwnedObjects => {
                if in_data_size < 2 * std::mem::size_of::<AudioObjectID>() as UInt32 {
                    return kAudioHardwareBadPropertySizeError;
                }
                let p = out_data as *mut AudioObjectID;
                ptr::write_unaligned(p, K_BOX_OBJECT);
                ptr::write_unaligned(p.add(1), K_DEVICE_OBJECT);
                if !out_data_size.is_null() {
                    *out_data_size = 2 * std::mem::size_of::<AudioObjectID>() as UInt32;
                }
                kAudioHardwareNoError
            }
            (K_PLUGIN_OBJECT, s) if s == kAudioPlugInPropertyBoxList => {
                put(K_BOX_OBJECT, in_data_size, out_data_size, out_data)
            }
            (K_PLUGIN_OBJECT, s) if s == kAudioPlugInPropertyDeviceList => {
                put(K_DEVICE_OBJECT, in_data_size, out_data_size, out_data)
            }
            (K_PLUGIN_OBJECT, s) if s == kAudioPlugInPropertyTranslateUIDToBox => {
                put(K_BOX_OBJECT, in_data_size, out_data_size, out_data)
            }
            (K_PLUGIN_OBJECT, s) if s == kAudioPlugInPropertyTranslateUIDToDevice => {
                put(K_DEVICE_OBJECT, in_data_size, out_data_size, out_data)
            }

            // Box.
            (K_BOX_OBJECT, s) if s == kAudioObjectPropertyOwnedObjects => {
                if !out_data_size.is_null() {
                    *out_data_size = 0;
                }
                kAudioHardwareNoError
            }
            (K_BOX_OBJECT, s) if s == kAudioBoxPropertyBoxUID => {
                put(cfstring(BOX_UID), in_data_size, out_data_size, out_data)
            }
            (K_BOX_OBJECT, s) if s == kAudioBoxPropertyAcquired => {
                // The box is permanently acquired so the device publishes.
                put(1u32, in_data_size, out_data_size, out_data)
            }

            // Device.
            (K_DEVICE_OBJECT, s) if s == kAudioObjectPropertyOwnedObjects => {
                put(K_STREAM_OBJECT, in_data_size, out_data_size, out_data)
            }
            (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyDeviceUID => {
                put(cfstring(DEVICE_UID), in_data_size, out_data_size, out_data)
            }
            (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyModelUID => put(
                cfstring(DEVICE_MODEL_UID),
                in_data_size,
                out_data_size,
                out_data,
            ),
            (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyTransportType => put(
                kAudioDeviceTransportTypeVirtual,
                in_data_size,
                out_data_size,
                out_data,
            ),
            (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyRelatedDevices => {
                put(K_DEVICE_OBJECT, in_data_size, out_data_size, out_data)
            }
            (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyClockDomain => {
                put(0u32, in_data_size, out_data_size, out_data)
            }
            (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyDeviceIsAlive => {
                put(1u32, in_data_size, out_data_size, out_data)
            }
            (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyDeviceIsRunning => {
                put(0u32, in_data_size, out_data_size, out_data)
            }
            (K_DEVICE_OBJECT, s)
                if s == kAudioDevicePropertyDeviceCanBeDefaultDevice
                    || s == kAudioDevicePropertyDeviceCanBeDefaultSystemDevice =>
            {
                put(1u32, in_data_size, out_data_size, out_data)
            }
            (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyLatency => {
                put(LATENCY, in_data_size, out_data_size, out_data)
            }
            (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertySafetyOffset => {
                put(SAFETY_OFFSET, in_data_size, out_data_size, out_data)
            }
            (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyIsHidden => {
                put(0u32, in_data_size, out_data_size, out_data)
            }
            (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyZeroTimeStampPeriod => {
                put(RING_FRAMES, in_data_size, out_data_size, out_data)
            }
            (K_DEVICE_OBJECT, s) if s == kAudioObjectPropertyControlList => {
                if !out_data_size.is_null() {
                    *out_data_size = 0;
                }
                kAudioHardwareNoError
            }
            (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyStreams => {
                // Only input streams exist, so any scope returns the one stream.
                put(K_STREAM_OBJECT, in_data_size, out_data_size, out_data)
            }
            (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyNominalSampleRate => {
                put(DEVICE_RATE, in_data_size, out_data_size, out_data)
            }
            (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyAvailableNominalSampleRates => put(
                AudioValueRange {
                    mMinimum: DEVICE_RATE,
                    mMaximum: DEVICE_RATE,
                },
                in_data_size,
                out_data_size,
                out_data,
            ),
            (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyPreferredChannelsForStereo => {
                if in_data_size < 2 * std::mem::size_of::<UInt32>() as UInt32 {
                    return kAudioHardwareBadPropertySizeError;
                }
                let p = out_data as *mut UInt32;
                ptr::write_unaligned(p, 1);
                ptr::write_unaligned(p.add(1), 2);
                if !out_data_size.is_null() {
                    *out_data_size = 2 * std::mem::size_of::<UInt32>() as UInt32;
                }
                kAudioHardwareNoError
            }

            // Stream.
            (K_STREAM_OBJECT, s) if s == kAudioObjectPropertyOwnedObjects => {
                if !out_data_size.is_null() {
                    *out_data_size = 0;
                }
                kAudioHardwareNoError
            }
            (K_STREAM_OBJECT, s) if s == kAudioStreamPropertyDirection => {
                // 0 = output, 1 = input.
                put(1u32, in_data_size, out_data_size, out_data)
            }
            (K_STREAM_OBJECT, s) if s == kAudioStreamPropertyTerminalType => put(
                kAudioStreamTerminalTypeMicrophone,
                in_data_size,
                out_data_size,
                out_data,
            ),
            (K_STREAM_OBJECT, s) if s == kAudioStreamPropertyStartingChannel => {
                put(1u32, in_data_size, out_data_size, out_data)
            }
            (K_STREAM_OBJECT, s) if s == kAudioStreamPropertyIsActive => {
                put(1u32, in_data_size, out_data_size, out_data)
            }
            (K_STREAM_OBJECT, s)
                if s == kAudioStreamPropertyVirtualFormat
                    || s == kAudioStreamPropertyPhysicalFormat =>
            {
                put(
                    device_asbd(DEVICE_RATE),
                    in_data_size,
                    out_data_size,
                    out_data,
                )
            }
            (K_STREAM_OBJECT, s)
                if s == kAudioStreamPropertyAvailableVirtualFormats
                    || s == kAudioStreamPropertyAvailablePhysicalFormats =>
            {
                put(
                    AudioStreamRangedDescription {
                        mFormat: device_asbd(DEVICE_RATE),
                        mSampleRateRange: AudioValueRange {
                            mMinimum: DEVICE_RATE,
                            mMaximum: DEVICE_RATE,
                        },
                    },
                    in_data_size,
                    out_data_size,
                    out_data,
                )
            }
            _ => kAudioHardwareUnknownPropertyError,
        }
    }
}

pub extern "C" fn set_property_data(
    _driver: AudioServerPlugInDriverRef,
    _object_id: AudioObjectID,
    _pid: libc::pid_t,
    _address: *const AudioObjectPropertyAddress,
    _qd_size: UInt32,
    _qd: *const c_void,
    _in_data_size: UInt32,
    _in_data: *const c_void,
) -> OSStatus {
    // Rate change is wired with the control socket (1b-2); accept for now.
    kAudioHardwareNoError
}

// IO: the HAL calls DoIOOperation per cycle. Until the ring is wired (1b-1)
// we return silence (the HAL zero-fills if we write nothing).
pub extern "C" fn start_io(
    _d: AudioServerPlugInDriverRef,
    _dev: AudioObjectID,
    _c: UInt32,
) -> OSStatus {
    kAudioHardwareNoError
}
pub extern "C" fn stop_io(
    _d: AudioServerPlugInDriverRef,
    _dev: AudioObjectID,
    _c: UInt32,
) -> OSStatus {
    kAudioHardwareNoError
}
pub extern "C" fn do_io_operation(
    _d: AudioServerPlugInDriverRef,
    _dev: AudioObjectID,
    _stream: AudioObjectID,
    _c: UInt32,
    _op: UInt32,
    _frames: UInt32,
    _cycle: *const c_void,
    _main: *mut c_void,
    _secondary: *mut c_void,
) -> OSStatus {
    kAudioHardwareNoError
}
