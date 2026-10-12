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
/// User-visible device name. NOT "… (Input)" any more: the device carries an
/// input AND an output stream (Phase 1b-2), and the old name made it look
/// input-only in System Settings → Sound → Output, where it is a valid output
/// device offering 8 channels. The stale name actively misled a hardware test
/// session into reading "(Input)" as a scope rather than a label.
///
/// `DEVICE_UID` deliberately keeps its `.input` suffix: macOS persists the
/// user's default-device choice by UID, so changing it would silently reset
/// their selection. It is an internal identifier and never shown.
const DEVICE_NAME: &str = "AVB StudioLive 16R";
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
        // Scope-aware, matching the DATA handler exactly (review M1): input or
        // output scope has ONE stream, global/wildcard has both. Returning
        // 2 ids here regardless of scope told a client to expect two and then
        // handed it one, so it read a stale id.
        (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyStreams => {
            if a.mScope == kAudioObjectPropertyScopeInput
                || a.mScope == kAudioObjectPropertyScopeOutput
            {
                oidsz
            } else {
                2 * oidsz
            }
        }
        (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyNominalSampleRate => f64sz,
        (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyAvailableNominalSampleRates => rangesz,
        (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyPreferredChannelsForStereo => 2 * u32sz,
        // NOTE: kAudioDevicePropertyPreferredChannelLayout is deliberately NOT
        // advertised. It was listed as known and sized here but had no case in
        // GetPropertyData, so HasProperty said yes and the get returned
        // UnknownProperty (review M2). The HAL derives a sensible default
        // layout from the channel count when the property is absent.
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
/// output ring: WriteMix hands us mixed f32 samples, we convert to S32 and
/// place them into the ring the daemon's talker loop consumes. The daemon
/// creates `SHM_NAME_OUT`; we attach it.
///
/// ONE long-lived producer per attach (review C1): it resumes at the ring's
/// live write frontier and keeps its committed/written bookkeeping across IO
/// cycles. Building a fresh `Producer` every cycle started `committed` at 0,
/// which rejected every place beyond cap/2 and reset the shared frontier —
/// playback was silent. `scratch` is the f32→S32 conversion buffer, allocated
/// once at attach so the realtime path never allocates or grows its stack.
struct OutIoState {
    prod: avb_audioring::ShmProducer,
    scratch: Vec<u8>,
    /// Inode of the ring file this mapping came from, so a ring the daemon has
    /// since replaced can be detected. See `out_ring_is_stale`.
    ino: u64,
}

/// The inode of the output ring path right now, or `None` if it is absent.
fn out_ring_ino() -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(avb_audioring::SHM_NAME_OUT)
        .ok()
        .map(|m| m.ino())
}

/// True if the mapping we hold is no longer the ring the daemon is using.
///
/// The daemon UNLINKS its output ring when `play` exits and creates a fresh
/// file — a new inode — on the next run. Our mmap of the old inode stays
/// perfectly valid, so without this check the plug-in keeps writing into a
/// deleted ring nobody reads while the live ring's write frontier sits at 0
/// forever. `start_io` only attached when it held nothing, and only `stop_io`
/// cleared it, so a device whose IO never stopped (ours is the default output)
/// never re-attached. Observed directly: a run begun after a previous daemon
/// exited transmitted 191,489 PDUs and 191,489 underruns while the plug-in was
/// writing happily to the orphaned mapping.
fn out_ring_is_stale(state: &OutIoState) -> bool {
    out_ring_ino().is_none_or(|ino| ino != state.ino)
}

/// Frames converted per inner step of `write_mix` (bounds the scratch buffer;
/// a HAL IO buffer is typically 64–1024 frames and is processed in chunks).
const OUT_CHUNK_FRAMES: usize = 512;

static OUT_IO: Mutex<Option<OutIoState>> = Mutex::new(None);

/// Attach the daemon's output ring and build the long-lived producer. Returns
/// `None` if the ring is absent or its geometry does not match this device.
fn attach_out() -> Option<OutIoState> {
    let channels = DEVICE_CHANNELS as usize;
    // Read the inode BEFORE mapping, so a ring swapped between the two reads
    // is caught by the next staleness check rather than remembered wrongly.
    let ino = out_ring_ino()?;
    let prod = avb_audioring::ShmProducer::attach_at(
        avb_audioring::SHM_NAME_OUT,
        &[(0, channels, 32)],
        0,
        0,
    )
    .ok()?;
    if prod.ring().channels() != DEVICE_CHANNELS {
        oslog("[avb-coreaudio] IO: output ring channel count mismatch — not attaching");
        return None;
    }
    Some(OutIoState {
        prod,
        scratch: vec![0u8; OUT_CHUNK_FRAMES * channels * 4],
        ino,
    })
}

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
    // Attach the OUTPUT ring (playback). Absence is not an error — write_mix
    // retries the attach (throttled) until the daemon's `play` loop creates it.
    // A mapping of a ring the daemon has already replaced is dropped first:
    // `if guard.is_none()` alone meant a stale mapping was kept forever.
    if let Ok(mut guard) = OUT_IO.lock() {
        if guard.as_ref().is_some_and(out_ring_is_stale) {
            *guard = None;
        }
        if guard.is_none() {
            *guard = attach_out();
            oslog(if guard.is_some() {
                "[avb-coreaudio] IO: attached output ring"
            } else {
                "[avb-coreaudio] IO: output ring not present — playback muted"
            });
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
/// The disciplined rate most recently seen on the OUTPUT ring, as `f64::to_bits`
/// (0 = none), plus the daemon's verdict code.
///
/// These are ATOMICS, not a mutex, because `GetZeroTimeStamp` reads them on
/// every call from the HAL's timing thread while `write_mix` runs on the
/// realtime IO thread. An earlier version of this read the rings directly and
/// took the `OUT_IO` lock to do it — the same lock `write_mix` holds inside the
/// audio callback. That put a mutex between the timing thread and the realtime
/// path and sent coreaudiod's CPU spiking. A publish/subscribe pair of atomics
/// has no such coupling: the IO paths store what they already have in hand,
/// and the timing thread only loads.
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

static OUT_RATE_BITS: AtomicU64 = AtomicU64::new(0);
static OUT_VERDICT: AtomicU32 = AtomicU32::new(0);
/// The disciplined rate most recently seen on the INPUT ring (0 = none).
static IN_RATE_BITS: AtomicU64 = AtomicU64::new(0);

/// A clock block's rate, if it is usable as a device timeline basis.
fn usable_rate(cb: &avb_audioring::ClockBlock) -> Option<f64> {
    (cb.locked && cb.rate_hz_vs_mach.is_finite() && cb.rate_hz_vs_mach > 0.0)
        .then_some(cb.rate_hz_vs_mach)
}

fn store_rate(slot: &AtomicU64, rate: Option<f64>) {
    slot.store(rate.unwrap_or(0.0).to_bits(), Ordering::Relaxed);
}

fn load_rate(slot: &AtomicU64) -> Option<f64> {
    let r = f64::from_bits(slot.load(Ordering::Relaxed));
    (r.is_finite() && r > 0.0).then_some(r)
}

/// Publish what the OUTPUT ring says. Called from the write path, which
/// already holds the output ring.
pub fn publish_out_clock(ring: &avb_audioring::AudioRing) {
    OUT_VERDICT.store(ring.effective_clock(), Ordering::Relaxed);
    store_rate(
        &OUT_RATE_BITS,
        ring.read_clock().as_ref().and_then(usable_rate),
    );
}

/// Publish what the INPUT ring says. Called from the read path.
pub fn publish_in_clock(ring: &avb_audioring::AudioRing) {
    store_rate(
        &IN_RATE_BITS,
        ring.read_clock().as_ref().and_then(usable_rate),
    );
}

/// Forget the output ring's basis when its mapping is dropped, so a stale rate
/// cannot outlive the ring it came from.
pub fn clear_out_clock() {
    OUT_VERDICT.store(0, Ordering::Relaxed);
    OUT_RATE_BITS.store(0, Ordering::Relaxed);
}

/// The rate the device timeline should run at, or `None` for nominal.
///
/// LOCK-FREE and safe to call from the HAL's timing path: it only loads
/// atomics published by the IO paths.
///
/// THE DAEMON DECIDES, THIS FOLLOWS. The daemon is the only process that sees
/// the user's selection, the gPTP servo and a recovered media clock at once,
/// so it resolves them and publishes both a basis and a verdict. Deciding here
/// would override the user: a session both recording and playing has a locked
/// input-ring clock, so preferring it would put the device on the 16R's clock
/// even when the user explicitly chose the Mac's.
///
/// A `MacInternal` verdict returns `None` and STOPS — it is a decision, not an
/// absence, so we must not go looking for another clock. Verdict 0 means no
/// clock-aware daemon has written one (or none is running), and only then does
/// the input ring's recovered clock apply; that covers a record-only session
/// and a daemon predating clock selection.
///
/// This device has ONE timeline for both directions (a single
/// `GetZeroTimeStamp` serving the input and output streams), so whatever is
/// returned here governs the rate CoreAudio both delivers and consumes at.
pub fn ring_disciplined_rate_hz() -> Option<f64> {
    match avb_config::EffectiveClock::from_code(OUT_VERDICT.load(Ordering::Relaxed)) {
        Some(avb_config::EffectiveClock::MacInternal(_)) => None,
        Some(_) => load_rate(&OUT_RATE_BITS),
        None => load_rate(&IN_RATE_BITS),
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
                // Publish the recovered media clock for GetZeroTimeStamp,
                // which must not take this lock itself (see IN_RATE_BITS).
                publish_in_clock(state.shm.ring());
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
    use avb_audioring::SampleFormat;
    use std::sync::atomic::{AtomicU64, Ordering};

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
    // Lazy attach (review H2): coreaudiod usually starts IO before the daemon
    // creates the output ring, so retry here — throttled to once per second so
    // a missing ring costs one failed open() per second, not one per cycle.
    // The same throttled tick drops a mapping whose ring the daemon has
    // replaced, which is what makes a daemon RESTART recover instead of
    // writing into an orphaned ring forever.
    {
        static LAST_TRY: AtomicU64 = AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if LAST_TRY.swap(now, Ordering::Relaxed) != now {
            if guard.as_ref().is_some_and(out_ring_is_stale) {
                oslog("[avb-coreaudio] IO: output ring replaced — reattaching");
                *guard = None; // drop the old mapping before remapping
                               // Forget its basis too: a rate must not outlive its ring.
                clear_out_clock();
            }
            if guard.is_none() {
                *guard = attach_out();
                if guard.is_some() {
                    oslog("[avb-coreaudio] IO: attached output ring (lazy retry in WriteMix)");
                }
            }
            // Publish the daemon's basis and verdict for GetZeroTimeStamp,
            // which must not take this lock itself (see OUT_RATE_BITS).
            match guard.as_ref() {
                Some(s) => publish_out_clock(s.prod.ring()),
                None => clear_out_clock(),
            }
        }
    }
    let Some(state) = guard.as_mut() else {
        return kAudioHardwareNoError; // ring not attached yet — drop (muted)
    };

    // Convert f32 → S32 big-endian in fixed chunks into the preallocated
    // scratch (no per-cycle stack array or allocation, review H1), and place
    // each chunk at the producer's committed frontier. `place` takes BE wire
    // bytes and stores LE S32 — the same container the input ring uses.
    let mut done = 0usize;
    while done < n_frames {
        let fr = (n_frames - done).min(OUT_CHUNK_FRAMES);
        let bytes = fr * frame_bytes;
        let src = inp
            .get(done * frame_bytes..done * frame_bytes + bytes)
            .unwrap_or(&[]);
        let Some(dst) = state.scratch.get_mut(..bytes) else {
            break;
        };
        let (src_chunks, _) = src.as_chunks::<4>();
        let (dst_chunks, _) = dst.as_chunks_mut::<4>();
        for (d, s) in dst_chunks.iter_mut().zip(src_chunks.iter()) {
            *d = f32_to_s32(f32::from_le_bytes(*s)).to_be_bytes();
        }
        let p = state.prod.producer();
        let at = p.committed();
        if p.place(0, i128::from(at), fr, SampleFormat::Int32, dst) {
            p.commit(false, None);
        }
        done += fr;
    }
    kAudioHardwareNoError
}

/// ±1.0 full-scale f32 → S32, clamped (the inverse of the ReadInput s/2^31).
fn f32_to_s32(f: f32) -> i32 {
    (f64::from(f) * 2_147_483_648.0).clamp(-2_147_483_648.0, 2_147_483_647.0) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_s32_round_trip_and_clamp() {
        assert_eq!(f32_to_s32(0.0), 0);
        assert_eq!(f32_to_s32(1.0), i32::MAX); // clamped, not wrapped
        assert_eq!(f32_to_s32(-1.0), i32::MIN);
        assert_eq!(f32_to_s32(4.0), i32::MAX); // over-range peak clamps
                                               // Inverse of the ReadInput conversion within one S32 LSB of f32 precision.
        let s = 123_456_789i32;
        let back = f32_to_s32(s as f32 / 2_147_483_648.0);
        assert!((i64::from(back) - i64::from(s)).abs() <= 64);
    }

    /// THE REGRESSION THIS EXISTS FOR.
    ///
    /// `GetZeroTimeStamp` calls `ring_disciplined_rate_hz` on every call from
    /// the HAL's timing thread. An earlier version read the rings directly and
    /// took the `OUT_IO` lock — the same lock `write_mix` holds inside the
    /// realtime audio callback — which put a mutex between the timing thread
    /// and the realtime path and sent coreaudiod's CPU spiking on hardware.
    ///
    /// So the contract is: it must be callable WHILE the IO locks are held.
    /// Against the old lock-taking version this deadlocks; against the
    /// lock-free one it returns at once. Bounded by a channel timeout so a
    /// regression fails loudly instead of hanging the suite.
    #[test]
    fn the_timing_path_does_not_wait_on_the_io_locks() {
        let out = OUT_IO.lock().expect("OUT_IO");
        let inp = IO.lock().expect("IO");
        let (tx, rx) = std::sync::mpsc::channel();
        let h = std::thread::spawn(move || {
            let _ = tx.send(ring_disciplined_rate_hz());
        });
        let got = rx.recv_timeout(std::time::Duration::from_secs(5));
        assert!(
            got.is_ok(),
            "ring_disciplined_rate_hz blocked while the IO locks were held — \
             the HAL's timing thread must never wait on the realtime path"
        );
        drop(out);
        drop(inp);
        h.join().expect("joined");
    }

    /// The daemon's verdict decides, and a Mac verdict is a DECISION: it must
    /// stop the search rather than fall through to the input ring's clock.
    /// Otherwise a session that both records and plays would be put on the
    /// 16R's clock even when the user explicitly chose the Mac's.
    #[test]
    fn a_mac_verdict_overrides_a_locked_input_ring() {
        use avb_config::{EffectiveClock, FallbackReason};
        // Serialised against the other atomic-touching test by running the
        // state changes in one test body.
        store_rate(&IN_RATE_BITS, Some(47_994.0));

        // No verdict (code 0: no clock-aware daemon) -> the input ring applies.
        OUT_VERDICT.store(0, Ordering::Relaxed);
        assert_eq!(ring_disciplined_rate_hz(), Some(47_994.0));

        // A Mac verdict -> nominal, NOT the input ring's rate.
        OUT_VERDICT.store(
            EffectiveClock::MacInternal(FallbackReason::Selected).code(),
            Ordering::Relaxed,
        );
        assert_eq!(
            ring_disciplined_rate_hz(),
            None,
            "a Mac verdict must not fall through to the input ring"
        );

        // A network verdict -> the OUTPUT ring's basis, not the input's.
        OUT_VERDICT.store(EffectiveClock::NetworkGptp.code(), Ordering::Relaxed);
        store_rate(&OUT_RATE_BITS, Some(48_004.8));
        assert_eq!(ring_disciplined_rate_hz(), Some(48_004.8));

        // A network verdict with no basis published yet is nominal, not the
        // input ring's: the daemon said which clock, we just have no rate.
        store_rate(&OUT_RATE_BITS, None);
        assert_eq!(ring_disciplined_rate_hz(), None);

        // Clearing forgets both, so a rate cannot outlive its ring.
        clear_out_clock();
        store_rate(&IN_RATE_BITS, None);
        assert_eq!(ring_disciplined_rate_hz(), None);
    }

    /// A zero, non-finite or negative rate is not a clock. These reach us from
    /// shared memory a separate process writes, so they are not hypothetical.
    #[test]
    fn a_nonsense_rate_is_not_published_as_one() {
        let slot = AtomicU64::new(0);
        assert_eq!(load_rate(&slot), None, "0 bits means 'none'");
        store_rate(&slot, Some(f64::NAN));
        assert_eq!(load_rate(&slot), None);
        store_rate(&slot, Some(f64::INFINITY));
        assert_eq!(load_rate(&slot), None);
        store_rate(&slot, Some(-48_000.0));
        assert_eq!(load_rate(&slot), None);
        store_rate(&slot, Some(0.0));
        assert_eq!(load_rate(&slot), None);
        // A real rate survives the round trip exactly.
        store_rate(&slot, Some(47_994.123));
        assert_eq!(load_rate(&slot), Some(47_994.123));
    }
}
