//! The AudioServerPlugIn driver-interface method implementations.
//!
//! Object model (Apple's convention): the plug-in object (id 1) owns one
//! device (id 3) which owns one input stream (id 4). coreaudiod enumerates by
//! asking the plug-in for its device list, each device for its properties and
//! streams, each stream for its format. This scaffold publishes an 8-channel
//! 48 kHz 32-bit input device; the IO path returns silence until the recovered
//! -audio ring is wired (Phase 1b-1). Property bodies are deliberately small
//! and only cover what the HAL needs to show the device.

#![allow(non_snake_case)]

use std::os::raw::c_void;
use std::ptr;

use crate::abi::*;

// Device defaults (overridden later from the daemon control API, 1b-2).
const DEVICE_CHANNELS: u32 = 8;
const DEVICE_RATE: f64 = 48_000.0;
const DEVICE_BITS: u32 = 32;
const DEVICE_UID: &str = "com.avbaudio.16r.input";
const DEVICE_NAME: &str = "AVB StudioLive 16R (Input)";
const MANUFACTURER: &str = "AVB-Audio";

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
        return kAudioHardwareIllegalOperationError;
    }
    ptr::write_unaligned(out_data as *mut T, v);
    if !out_data_size.is_null() {
        *out_data_size = need;
    }
    kAudioHardwareNoError
}

pub extern "C" fn initialize(
    _driver: AudioServerPlugInDriverRef,
    _host: AudioServerPlugInHostRef,
) -> OSStatus {
    kAudioHardwareNoError
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
    let known = match object_id {
        K_PLUGIN_OBJECT => matches!(
            a.mSelector,
            x if x == kAudioPlugInPropertyDeviceList
                || x == kAudioObjectPropertyManufacturer
                || x == kAudioObjectPropertyClass
        ),
        K_DEVICE_OBJECT => matches!(
            a.mSelector,
            x if x == kAudioDevicePropertyDeviceUID
                || x == kAudioObjectPropertyName
                || x == kAudioObjectPropertyManufacturer
                || x == kAudioDevicePropertyNominalSampleRate
                || x == kAudioDevicePropertyStreams
        ),
        K_STREAM_OBJECT => matches!(
            a.mSelector,
            x if x == kAudioStreamPropertyDirection
                || x == kAudioStreamPropertyVirtualFormat
                || x == kAudioObjectPropertyName
        ),
        _ => false,
    };
    Boolean::from(known)
}

pub extern "C" fn is_property_settable(
    _driver: AudioServerPlugInDriverRef,
    _object_id: AudioObjectID,
    _pid: libc::pid_t,
    address: *const AudioObjectPropertyAddress,
    out_settable: *mut Boolean,
) -> OSStatus {
    if out_settable.is_null() || address.is_null() {
        return kAudioHardwareIllegalOperationError;
    }
    // Only the nominal sample rate is settable (rate change, 1b-2).
    let a = unsafe { &*address };
    unsafe {
        *out_settable = Boolean::from(a.mSelector == kAudioDevicePropertyNominalSampleRate);
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
    let size: UInt32 = match (object_id, a.mSelector) {
        (K_PLUGIN_OBJECT, s) if s == kAudioPlugInPropertyDeviceList => {
            std::mem::size_of::<AudioObjectID>() as UInt32
        }
        (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyStreams => {
            std::mem::size_of::<AudioObjectID>() as UInt32
        }
        (_, s)
            if s == kAudioObjectPropertyName
                || s == kAudioDevicePropertyDeviceUID
                || s == kAudioObjectPropertyManufacturer =>
        {
            std::mem::size_of::<CFStringRef>() as UInt32
        }
        (_, s) if s == kAudioDevicePropertyNominalSampleRate => {
            std::mem::size_of::<Float64>() as UInt32
        }
        (_, s) if s == kAudioStreamPropertyVirtualFormat => {
            std::mem::size_of::<AudioStreamBasicDescription>() as UInt32
        }
        (_, s) if s == kAudioStreamPropertyDirection || s == kAudioObjectPropertyClass => {
            std::mem::size_of::<UInt32>() as UInt32
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
        match (object_id, a.mSelector) {
            (K_PLUGIN_OBJECT, s) if s == kAudioPlugInPropertyDeviceList => {
                put(K_DEVICE_OBJECT, in_data_size, out_data_size, out_data)
            }
            (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyStreams => {
                put(K_STREAM_OBJECT, in_data_size, out_data_size, out_data)
            }
            (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyNominalSampleRate => {
                put(DEVICE_RATE, in_data_size, out_data_size, out_data)
            }
            (K_STREAM_OBJECT, s) if s == kAudioStreamPropertyDirection => {
                // 0 = output, 1 = input.
                put(1u32, in_data_size, out_data_size, out_data)
            }
            (K_STREAM_OBJECT, s) if s == kAudioStreamPropertyVirtualFormat => {
                let asbd = AudioStreamBasicDescription {
                    mSampleRate: DEVICE_RATE,
                    mFormatID: kAudioFormatLinearPCM,
                    mFormatFlags: kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked,
                    mBytesPerPacket: DEVICE_CHANNELS * 4,
                    mFramesPerPacket: 1,
                    mBytesPerFrame: DEVICE_CHANNELS * 4,
                    mChannelsPerFrame: DEVICE_CHANNELS,
                    mBitsPerChannel: DEVICE_BITS,
                    mReserved: 0,
                };
                put(asbd, in_data_size, out_data_size, out_data)
            }
            // CFString properties: the HAL requires the device UID to list the
            // device at all. Hand back an owned CFStringRef (the HAL releases
            // it).
            (K_DEVICE_OBJECT, s) if s == kAudioDevicePropertyDeviceUID => {
                put(cfstring(DEVICE_UID), in_data_size, out_data_size, out_data)
            }
            (_, s) if s == kAudioObjectPropertyName => {
                let name = if object_id == K_STREAM_OBJECT {
                    "AVB 16R Input Stream"
                } else {
                    DEVICE_NAME
                };
                put(cfstring(name), in_data_size, out_data_size, out_data)
            }
            (_, s) if s == kAudioObjectPropertyManufacturer => put(
                cfstring(MANUFACTURER),
                in_data_size,
                out_data_size,
                out_data,
            ),
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
