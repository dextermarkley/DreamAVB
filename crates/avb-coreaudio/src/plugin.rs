// SPDX-License-Identifier: MIT OR Apache-2.0

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
//! The device is an 8-ch 48 kHz Float32 input (CoreAudio's native sample
//! format). The IO path converts the recovered-audio ring's S32 samples to
//! f32 on delivery (Phase 1b-1).

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

/// Write a list of `AudioObjectID`s into `out_data`, truncated to the room
/// `in_data_size` allows (the HAL may ask for a partial count). Sets the
/// written size to the number of ids actually written × their size.
unsafe fn put_oids(
    ids: &[AudioObjectID],
    in_data_size: UInt32,
    out_data_size: *mut UInt32,
    out_data: *mut c_void,
) -> OSStatus {
    if out_data.is_null() {
        return kAudioHardwareBadPropertySizeError;
    }
    let one = std::mem::size_of::<AudioObjectID>();
    let room = (in_data_size as usize) / one;
    let n = room.min(ids.len());
    for (i, id) in ids.iter().take(n).enumerate() {
        ptr::write_unaligned((out_data as *mut AudioObjectID).add(i), *id);
    }
    if !out_data_size.is_null() {
        *out_data_size = (n * one) as UInt32;
    }
    kAudioHardwareNoError
}

/// Build the device's ASBD for a given rate (interleaved 32-bit float PCM).
///
/// The modern CoreAudio AudioServerPlugIn model expects **Float32** samples
/// (±1.0 full scale), not integer — this is what BlackHole and every stock
/// HAL input device advertise. We advertise float here and convert the ring's
/// S32 samples to f32 in the ReadInput delivery path (`do_io_operation`).
/// Advertising integer while handing CoreAudio integer bytes made CoreAudio
/// reinterpret them as float, collapsing every sample to 0 or full-scale
/// (the 0x800000 garble seen in Reaper).
fn device_asbd(rate: f64) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: rate,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat
            | kAudioFormatFlagsNativeEndian
            | kAudioFormatFlagIsPacked,
        mBytesPerPacket: DEVICE_CHANNELS * 4,
        mFramesPerPacket: 1,
        mBytesPerFrame: DEVICE_CHANNELS * 4,
        mChannelsPerFrame: DEVICE_CHANNELS,
        mBitsPerChannel: DEVICE_BITS,
        mReserved: 0,
    }
}

/// Decode a FourCC selector to a readable string for diagnostics.
fn fourcc(v: UInt32) -> String {
    let b = v.to_be_bytes();
    b.iter()
        .map(|&c| {
            if (32..127).contains(&c) {
                c as char
            } else {
                '?'
            }
        })
        .collect()
}

/// Log an unanswered property probe. coreaudiod captures the plug-in's stderr
/// into the unified log, so this surfaces exactly which (object, selector) we
/// are returning UnknownProperty for. (Dev diagnostic — gated off before
/// release; coreaudiod does not pass AVB_CA_DEBUG so we log unconditionally
/// for now.)
fn log_unknown(what: &str, object_id: AudioObjectID, a: &AudioObjectPropertyAddress) {
    oslog(&format!(
        "[avb-coreaudio] {what} UNKNOWN obj={object_id} sel={} scope={} elem={}",
        fourcc(a.mSelector),
        fourcc(a.mScope),
        a.mElement
    ));
}

pub extern "C" fn initialize(
    _driver: AudioServerPlugInDriverRef,
    _host: AudioServerPlugInHostRef,
) -> OSStatus {
    oslog("[avb-coreaudio] Initialize called");
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
        K_STREAM_OBJECT | K_STREAM_OUT_OBJECT => matches!(
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
        (K_DEVICE_OBJECT, s) if s == kAudioObjectPropertyOwnedObjects => 2 * oidsz,
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
        (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyStreams => 2 * oidsz,
        (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyNominalSampleRate => f64sz,
        (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyAvailableNominalSampleRates => rangesz,
        (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyPreferredChannelsForStereo => 2 * u32sz,
        (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyPreferredChannelLayout => {
            // AudioChannelLayout header (12 bytes) + N descriptions (20 bytes).
            12 + DEVICE_CHANNELS * 20
        }
        // Stream.
        (K_STREAM_OBJECT | K_STREAM_OUT_OBJECT, s)
            if s == kAudioStreamPropertyDirection
                || s == kAudioStreamPropertyTerminalType
                || s == kAudioStreamPropertyStartingChannel
                || s == kAudioStreamPropertyIsActive =>
        {
            u32sz
        }
        (K_STREAM_OBJECT | K_STREAM_OUT_OBJECT, s)
            if s == kAudioStreamPropertyVirtualFormat
                || s == kAudioStreamPropertyPhysicalFormat =>
        {
            asbdsz
        }
        (K_STREAM_OBJECT | K_STREAM_OUT_OBJECT, s)
            if s == kAudioStreamPropertyAvailableVirtualFormats
                || s == kAudioStreamPropertyAvailablePhysicalFormats =>
        {
            rangedsz
        }
        _ => {
            log_unknown("size", object_id, a);
            return kAudioHardwareUnknownPropertyError;
        }
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
                    K_STREAM_OBJECT | K_STREAM_OUT_OBJECT => kAudioObjectClassID,
                    _ => return kAudioHardwareBadObjectError,
                };
                return put(base, in_data_size, out_data_size, out_data);
            }
            s if s == kAudioObjectPropertyClass => {
                let class = match object_id {
                    K_PLUGIN_OBJECT => kAudioPlugInClassID,
                    K_BOX_OBJECT => kAudioBoxClassID,
                    K_DEVICE_OBJECT => kAudioDeviceClassID,
                    K_STREAM_OBJECT | K_STREAM_OUT_OBJECT => kAudioStreamClassID,
                    _ => return kAudioHardwareBadObjectError,
                };
                return put(class, in_data_size, out_data_size, out_data);
            }
            s if s == kAudioObjectPropertyOwner => {
                let owner = match object_id {
                    K_PLUGIN_OBJECT => kAudioObjectUnknown, // the plug-in has no owner
                    K_BOX_OBJECT => K_PLUGIN_OBJECT,
                    K_DEVICE_OBJECT => K_PLUGIN_OBJECT,
                    K_STREAM_OBJECT | K_STREAM_OUT_OBJECT => K_DEVICE_OBJECT,
                    _ => return kAudioHardwareBadObjectError,
                };
                return put(owner, in_data_size, out_data_size, out_data);
            }
            s if s == kAudioObjectPropertyName => {
                let name = match object_id {
                    K_BOX_OBJECT => BOX_NAME,
                    K_DEVICE_OBJECT => DEVICE_NAME,
                    K_STREAM_OBJECT => "AVB 16R Input Stream",
                    K_STREAM_OUT_OBJECT => "AVB 16R Output Stream",
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
            (K_DEVICE_OBJECT, s) if s == kAudioObjectPropertyOwnedObjects => put_oids(
                &[K_STREAM_OBJECT, K_STREAM_OUT_OBJECT],
                in_data_size,
                out_data_size,
                out_data,
            ),
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
                // Scope selects which streams: input scope -> the input stream,
                // output scope -> the output stream, global/wildcard -> both.
                match a.mScope {
                    x if x == kAudioObjectPropertyScopeInput => {
                        put(K_STREAM_OBJECT, in_data_size, out_data_size, out_data)
                    }
                    x if x == kAudioObjectPropertyScopeOutput => {
                        put(K_STREAM_OUT_OBJECT, in_data_size, out_data_size, out_data)
                    }
                    _ => put_oids(
                        &[K_STREAM_OBJECT, K_STREAM_OUT_OBJECT],
                        in_data_size,
                        out_data_size,
                        out_data,
                    ),
                }
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

            // Stream (both input and output).
            (K_STREAM_OBJECT | K_STREAM_OUT_OBJECT, s) if s == kAudioObjectPropertyOwnedObjects => {
                if !out_data_size.is_null() {
                    *out_data_size = 0;
                }
                kAudioHardwareNoError
            }
            (K_STREAM_OBJECT, s) if s == kAudioStreamPropertyDirection => {
                // 0 = output, 1 = input.
                put(1u32, in_data_size, out_data_size, out_data)
            }
            (K_STREAM_OUT_OBJECT, s) if s == kAudioStreamPropertyDirection => {
                put(0u32, in_data_size, out_data_size, out_data)
            }
            (K_STREAM_OBJECT, s) if s == kAudioStreamPropertyTerminalType => put(
                kAudioStreamTerminalTypeMicrophone,
                in_data_size,
                out_data_size,
                out_data,
            ),
            (K_STREAM_OUT_OBJECT, s) if s == kAudioStreamPropertyTerminalType => put(
                kAudioStreamTerminalTypeSpeaker,
                in_data_size,
                out_data_size,
                out_data,
            ),
            (K_STREAM_OBJECT | K_STREAM_OUT_OBJECT, s)
                if s == kAudioStreamPropertyStartingChannel =>
            {
                put(1u32, in_data_size, out_data_size, out_data)
            }
            (K_STREAM_OBJECT | K_STREAM_OUT_OBJECT, s) if s == kAudioStreamPropertyIsActive => {
                put(1u32, in_data_size, out_data_size, out_data)
            }
            (K_STREAM_OBJECT | K_STREAM_OUT_OBJECT, s)
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
            (K_STREAM_OBJECT | K_STREAM_OUT_OBJECT, s)
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
            _ => {
                log_unknown("data", object_id, a);
                kAudioHardwareUnknownPropertyError
            }
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

// IO: the HAL calls DoIOOperation per cycle. Phase 1b-1 — ReadInput is filled
// from the daemon's recovered-audio ring over POSIX shared memory. When the
// daemon is not running (no segment yet) or the ring underruns, we zero-fill:
// the device stays alive and silent rather than erroring.
use std::sync::Mutex;

use avb_audioring::{Consumer, ShmRing};

/// IO state held between StartIO and StopIO. The `ShmRing` owns the mapping;
/// the `read_frame` cursor tracks our consume position across cycles (we
/// rebuild a `Consumer` per cycle from this cursor to avoid a self-referential
/// borrow of the ring held in the same struct).
struct IoState {
    shm: ShmRing,
    read_frame: u64,
}

static IO: Mutex<Option<IoState>> = Mutex::new(None);

/// Output (playback) IO state (Phase 1b-2). The plug-in is the PRODUCER on the
/// output ring: WriteMix hands us mixed f32 samples, we convert to S32LE and
/// place them into the ring the daemon's talker loop consumes. The daemon
/// creates `SHM_NAME_OUT`; we attach it. `write_frame` is our place cursor.
struct OutIoState {
    shm: ShmRing,
    write_frame: u64,
}

static OUT_IO: Mutex<Option<OutIoState>> = Mutex::new(None);

pub extern "C" fn start_io(
    _d: AudioServerPlugInDriverRef,
    _dev: AudioObjectID,
    _c: UInt32,
) -> OSStatus {
    // Attach the shm ring if the daemon has created it. Absence is not an
    // error — IO proceeds as silence and a later StartIO (after the daemon
    // starts) will attach.
    if let Ok(mut guard) = IO.lock() {
        if guard.is_none() {
            match ShmRing::attach() {
                Ok(shm) => {
                    // Start a small margin behind the write frontier (see the
                    // DoIO retry path) so the first reads have committed data.
                    let w = Consumer::new(shm.ring()).write_frame();
                    let read_frame = w.saturating_sub(2048);
                    *guard = Some(IoState { shm, read_frame });
                    oslog("[avb-coreaudio] IO: attached recovered-audio ring");
                }
                Err(_) => {
                    oslog("[avb-coreaudio] IO: daemon ring not present — silent");
                }
            }
        }
    }
    // Attach the OUTPUT ring (playback). Absence is not an error — WriteMix is
    // dropped until the daemon's `play` loop has created the ring.
    if let Ok(mut guard) = OUT_IO.lock() {
        if guard.is_none() {
            match ShmRing::attach_at(avb_audioring::SHM_NAME_OUT) {
                Ok(shm) => {
                    let write_frame = Consumer::new(shm.ring()).write_frame();
                    *guard = Some(OutIoState { shm, write_frame });
                    oslog("[avb-coreaudio] IO: attached output ring");
                }
                Err(_) => {
                    oslog("[avb-coreaudio] IO: output ring not present — playback muted");
                }
            }
        }
    }
    kAudioHardwareNoError
}

pub extern "C" fn stop_io(
    _d: AudioServerPlugInDriverRef,
    _dev: AudioObjectID,
    _c: UInt32,
) -> OSStatus {
    if let Ok(mut guard) = IO.lock() {
        *guard = None; // drops ShmRing -> munmap/close
    }
    if let Ok(mut guard) = OUT_IO.lock() {
        *guard = None;
    }
    kAudioHardwareNoError
}

/// The ring's disciplined media-clock rate in Hz, if the ring is attached and
/// its clock is locked.
///
/// The daemon fills the ring on the 16R's MEDIA clock, a different 48 kHz
/// oscillator from the Mac host clock (measured ≈ −100 ppm). A HAL device whose
/// `GetZeroTimeStamp` reports the NOMINAL rate makes the HAL reclock our stream
/// against a timeline that does not match data arrival — audible as warble even
/// though every ring byte is correct (confirmed against BlackHole's source: it
/// avoids this only because it is a shared-HAL-clock loopback; CamillaDSP's docs
/// name adjusting the virtual clock rate as the fix). Returning the ring's
/// disciplined rate here lets `get_zero_time_stamp` run the device timeline at
/// the true data rate.
///
/// `None` when the ring is unattached or unlocked, so the caller falls back to
/// the nominal-rate stand-in and the device stays alive.
pub fn ring_disciplined_rate_hz() -> Option<f64> {
    let guard = IO.lock().ok()?;
    let state = guard.as_ref()?;
    let cb = state.shm.ring().read_clock()?;
    if cb.locked && cb.rate_hz_vs_mach.is_finite() && cb.rate_hz_vs_mach > 0.0 {
        Some(cb.rate_hz_vs_mach)
    } else {
        None
    }
}

pub extern "C" fn do_io_operation(
    _d: AudioServerPlugInDriverRef,
    _dev: AudioObjectID,
    _stream: AudioObjectID,
    _c: UInt32,
    operation_id: UInt32,
    io_buffer_frame_size: UInt32,
    _cycle: *const c_void,
    io_main_buffer: *mut c_void,
    _secondary: *mut c_void,
) -> OSStatus {
    if io_main_buffer.is_null() {
        return kAudioHardwareNoError;
    }
    // WriteMix (playback): the HAL hands us mixed output samples to transmit.
    if operation_id == kAudioServerPlugInIOOperationWriteMix {
        return write_mix(io_buffer_frame_size, io_main_buffer);
    }
    if operation_id != kAudioServerPlugInIOOperationReadInput {
        return kAudioHardwareNoError;
    }
    let frame_bytes = (DEVICE_CHANNELS * 4) as usize;
    let total_bytes = io_buffer_frame_size as usize * frame_bytes;
    // The HAL buffer is interleaved Float32 (device ASBD = float). Our ring
    // holds interleaved S32LE, so we read raw ring bytes into `out` and then
    // convert each 4-byte S32 sample to f32 IN PLACE below — same 4B stride,
    // so the frame layout is identical and only the value encoding changes.
    // SAFETY: the HAL guarantees io_main_buffer holds io_buffer_frame_size
    // frames of the stream's physical format (8ch × 4B).
    let out = unsafe { std::slice::from_raw_parts_mut(io_main_buffer as *mut u8, total_bytes) };

    let mut filled = 0usize;
    // 1-Hz throttle so the log reflects the LIVE cycle, not just the first N
    // cycles (a capped counter hides current state behind ancient history).
    let log_now = {
        use std::sync::atomic::{AtomicU64, Ordering};
        static LAST: AtomicU64 = AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let prev = LAST.load(Ordering::Relaxed);
        if now != prev {
            LAST.store(now, Ordering::Relaxed);
            true
        } else {
            false
        }
    };
    if let Ok(mut guard) = IO.lock() {
        // Lazy attach-retry: StartIO may have run before the daemon created
        // the ring file, and attach is otherwise one-shot. Retry here so we
        // pick it up whenever the daemon appears — regardless of start order.
        if guard.is_none() {
            match ShmRing::attach() {
                Ok(shm) => {
                    // Start a small margin BEHIND the write frontier so the
                    // first reads have committed data (avoids immediate
                    // underrun) while keeping latency low. ~2048 fr ≈ 43 ms.
                    let w = Consumer::new(shm.ring()).write_frame();
                    let read_frame = w.saturating_sub(2048);
                    *guard = Some(IoState { shm, read_frame });
                    oslog("[avb-coreaudio] IO: attached ring (lazy retry in DoIO)");
                }
                Err(e) => {
                    if log_now {
                        let errno = std::io::Error::last_os_error();
                        oslog(&format!(
                            "[avb-coreaudio] DoIO attach FAILED: {e:?} errno={errno}"
                        ));
                    }
                }
            }
        }
        if let Some(state) = guard.as_mut() {
            let ring = state.shm.ring();
            // Seed from OUR saved cursor (set live on StartIO), not the ring's
            // shared read_frame — the daemon never advances that, so reading it
            // would replay the whole backlog each cycle (= latency / no live
            // signal). read_into advances from here and we save it back.
            let mut consumer = Consumer::with_read_frame(ring, state.read_frame);
            let want = io_buffer_frame_size as usize;
            let avail = consumer.available();
            let got = consumer.read_into(out, want);
            filled = got * frame_bytes;
            // Convert the S32LE ring samples just copied into `out` to the
            // Float32 the device ASBD advertises. ±2^31 maps to ±1.0. Done in
            // place over the filled region: each sample is 4 bytes either way.
            // (Without this, CoreAudio reads our integer bit patterns as float
            // → every sample collapses to 0 or full-scale: the 0x800000
            // garble. See do_io / device_asbd notes.)
            let (chunks, _rem) = out
                .get_mut(..filled)
                .unwrap_or(&mut [])
                .as_chunks_mut::<4>();
            for chunk in chunks {
                let s = i32::from_le_bytes(*chunk);
                let f = s as f32 / 2_147_483_648.0_f32;
                *chunk = f.to_le_bytes();
            }
            state.read_frame = consumer.read_frame();
            if log_now {
                oslog(&format!(
                    "[avb-coreaudio] read got={got} want={want} avail={avail} write={} cursor={}",
                    consumer.write_frame(),
                    state.read_frame
                ));
            }
        }
    }
    // Zero-fill any remainder (underrun, or no daemon): silence, never garbage.
    if let Some(tail) = out.get_mut(filled..) {
        tail.fill(0);
    }
    kAudioHardwareNoError
}

/// WriteMix (Phase 1b-2 playback): the HAL hands us `frames` of interleaved
/// Float32 output samples; convert to S32 big-endian and `place` them into the
/// output ring the daemon's talker loop consumes. The HAL buffer's format is
/// the device ASBD (Float32, ±1.0 full scale); the ring container is S32.
/// `place` takes big-endian wire bytes and stores little-endian, so the daemon
/// reads LE S32 — the same container the input ring uses.
fn write_mix(frames: UInt32, io_main_buffer: *mut c_void) -> OSStatus {
    use avb_audioring::{Producer, SampleFormat};

    let channels = DEVICE_CHANNELS as usize;
    let frame_bytes = channels * 4;
    let n_frames = frames as usize;
    let total_bytes = n_frames * frame_bytes;
    // SAFETY: the HAL guarantees the buffer holds `frames` frames of the
    // stream's physical format (8ch × 4B f32).
    let inp = unsafe { std::slice::from_raw_parts(io_main_buffer as *const u8, total_bytes) };

    let Ok(mut guard) = OUT_IO.lock() else {
        return kAudioHardwareNoError;
    };
    let Some(state) = guard.as_mut() else {
        return kAudioHardwareNoError; // ring not attached yet — drop (muted)
    };
    let ring = state.shm.ring();

    // Convert f32 → S32 big-endian into a scratch, then place one frame-group.
    // Max buffer: a HAL IO buffer is small (hundreds of frames); cap the
    // scratch at a generous bound and process in that unit.
    const MAX_FR: usize = 4096;
    let mut be = [0u8; MAX_FR * 8 * 4]; // 8ch × 4B, up to MAX_FR frames
    let fr_this = n_frames.min(MAX_FR);
    let want_bytes = fr_this * frame_bytes;
    let (src_chunks, _r) = inp.get(..want_bytes).unwrap_or(&[]).as_chunks::<4>();
    let be_slice = be.get_mut(..want_bytes).unwrap_or(&mut []);
    let (be_chunks, _r2) = be_slice.as_chunks_mut::<4>();
    for (dst, src) in be_chunks.iter_mut().zip(src_chunks.iter()) {
        let f = f32::from_le_bytes(*src);
        // ±1.0 full scale → S32. Clamp to avoid overflow on >1.0 peaks.
        let scaled = (f as f64 * 2_147_483_648.0).clamp(-2_147_483_648.0, 2_147_483_647.0);
        *dst = (scaled as i32).to_be_bytes();
    }

    let mut producer = Producer::new(ring, &[(0, channels, 32)], 0, 0);
    let f = state.write_frame;
    if producer.place(0, f as i128, fr_this, SampleFormat::Int32, be_slice) {
        producer.commit(false, None);
        state.write_frame = f + fr_this as u64;
    }
    kAudioHardwareNoError
}
