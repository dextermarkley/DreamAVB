//! The CoreAudio / AudioServerPlugIn C ABI, hand-declared.
//!
//! A HAL plug-in is a CFPlugIn: `coreaudiod` calls our factory (named in
//! `Info.plist` `CFPlugInFactories`), which returns a pointer to an
//! `AudioServerPlugInDriverInterface` — a COM-style vtable whose first three
//! slots are the `IUnknown` methods (`QueryInterface`/`AddRef`/`Release`) and
//! the rest are the HAL driver entry points (Apple's
//! `AudioServerPlugIn.h`). We declare only what we use; the layout and the
//! field ORDER must match Apple's header exactly, so the full vtable is
//! declared with every slot (unused ones as raw function pointers) to keep the
//! offsets correct.
//!
//! References: Apple `CoreAudio/AudioServerPlugIn.h`,
//! `CoreFoundation/CFPlugInCOM.h`. Types mirrored from `<CoreAudio/*.h>`.

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::os::raw::{c_int, c_void};

pub type OSStatus = i32;
pub type UInt32 = u32;
pub type Float64 = f64;
pub type Boolean = u8;

// CoreFoundation opaque pointers (we only pass them through).
pub type CFAllocatorRef = *const c_void;
pub type CFUUIDRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFPropertyListRef = *const c_void;
pub type CFTypeRef = *const c_void;

/// AudioObjectID (a HAL object handle). 0 = kAudioObjectUnknown.
pub type AudioObjectID = UInt32;
pub type AudioServerPlugInDriverRef = *mut c_void;
pub type AudioServerPlugInHostRef = *const c_void;

pub const kAudioObjectUnknown: AudioObjectID = 0;
/// coreaudiod always addresses the plug-in object as id 1 and the single
/// device/stream as ids we choose (Apple's convention: plug-in=1).
pub const K_PLUGIN_OBJECT: AudioObjectID = 1;
pub const K_BOX_OBJECT: AudioObjectID = 2;
pub const K_DEVICE_OBJECT: AudioObjectID = 3;
pub const K_STREAM_OBJECT: AudioObjectID = 4;

/// `AudioObjectPropertyAddress` (selector/scope/element), 4-byte-packed.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioObjectPropertyAddress {
    pub mSelector: UInt32,
    pub mScope: UInt32,
    pub mElement: UInt32,
}

/// `AudioStreamBasicDescription` (<CoreAudio/CoreAudioTypes.h>).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct AudioStreamBasicDescription {
    pub mSampleRate: Float64,
    pub mFormatID: UInt32,
    pub mFormatFlags: UInt32,
    pub mBytesPerPacket: UInt32,
    pub mFramesPerPacket: UInt32,
    pub mBytesPerFrame: UInt32,
    pub mChannelsPerFrame: UInt32,
    pub mBitsPerChannel: UInt32,
    pub mReserved: UInt32,
}

/// LPCM, as used for our 32-bit signed interleaved stream.
pub const kAudioFormatLinearPCM: UInt32 = u32::from_be_bytes(*b"lpcm");
pub const kAudioFormatFlagIsSignedInteger: UInt32 = 1 << 2;
pub const kAudioFormatFlagIsPacked: UInt32 = 1 << 3;

// `AudioServerPlugInClientInfo` and IO timing structs are passed through as
// opaque for the skeleton; declared as raw pointers in the vtable below.

/// The driver interface vtable. The field ORDER is Apple's
/// `AudioServerPlugInDriverInterface` exactly (one `_reserved` + the IUnknown
/// trio, then the HAL entry points). Unused slots keep real signatures so the
/// offsets are correct.
#[repr(C)]
pub struct AudioServerPlugInDriverInterface {
    /// Always null (Apple's `_reserved`). Declared `usize` (not a raw pointer)
    /// so the static vtable is `Sync`; a null pointer and 0 are the same bits.
    pub _reserved: usize,
    pub QueryInterface:
        extern "C" fn(this: *mut c_void, iid: CFUUIDBytes, out: *mut *mut c_void) -> OSStatus,
    pub AddRef: extern "C" fn(this: *mut c_void) -> u32,
    pub Release: extern "C" fn(this: *mut c_void) -> u32,
    pub Initialize: extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        host: AudioServerPlugInHostRef,
    ) -> OSStatus,
    pub CreateDevice: extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        description: CFDictionaryRef,
        client_info: *const c_void,
        out_device_id: *mut AudioObjectID,
    ) -> OSStatus,
    pub DestroyDevice:
        extern "C" fn(driver: AudioServerPlugInDriverRef, device_id: AudioObjectID) -> OSStatus,
    pub AddDeviceClient: extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_id: AudioObjectID,
        client_info: *const c_void,
    ) -> OSStatus,
    pub RemoveDeviceClient: extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_id: AudioObjectID,
        client_info: *const c_void,
    ) -> OSStatus,
    pub PerformDeviceConfigurationChange: extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_id: AudioObjectID,
        change_action: u64,
        change_info: *mut c_void,
    ) -> OSStatus,
    pub AbortDeviceConfigurationChange: extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_id: AudioObjectID,
        change_action: u64,
        change_info: *mut c_void,
    ) -> OSStatus,
    pub HasProperty: extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        object_id: AudioObjectID,
        client_pid: libc::pid_t,
        address: *const AudioObjectPropertyAddress,
    ) -> Boolean,
    pub IsPropertySettable: extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        object_id: AudioObjectID,
        client_pid: libc::pid_t,
        address: *const AudioObjectPropertyAddress,
        out_settable: *mut Boolean,
    ) -> OSStatus,
    pub GetPropertyDataSize: extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        object_id: AudioObjectID,
        client_pid: libc::pid_t,
        address: *const AudioObjectPropertyAddress,
        qualifier_data_size: UInt32,
        qualifier_data: *const c_void,
        out_data_size: *mut UInt32,
    ) -> OSStatus,
    pub GetPropertyData: extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        object_id: AudioObjectID,
        client_pid: libc::pid_t,
        address: *const AudioObjectPropertyAddress,
        qualifier_data_size: UInt32,
        qualifier_data: *const c_void,
        in_data_size: UInt32,
        out_data_size: *mut UInt32,
        out_data: *mut c_void,
    ) -> OSStatus,
    pub SetPropertyData: extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        object_id: AudioObjectID,
        client_pid: libc::pid_t,
        address: *const AudioObjectPropertyAddress,
        qualifier_data_size: UInt32,
        qualifier_data: *const c_void,
        in_data_size: UInt32,
        in_data: *const c_void,
    ) -> OSStatus,
    pub StartIO: extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_id: AudioObjectID,
        client_id: UInt32,
    ) -> OSStatus,
    pub StopIO: extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_id: AudioObjectID,
        client_id: UInt32,
    ) -> OSStatus,
    pub GetZeroTimeStamp: extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_id: AudioObjectID,
        client_id: UInt32,
        out_sample_time: *mut Float64,
        out_host_time: *mut u64,
        out_seed: *mut u64,
    ) -> OSStatus,
    pub WillDoIOOperation: extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_id: AudioObjectID,
        client_id: UInt32,
        operation_id: UInt32,
        out_will_do: *mut Boolean,
        out_will_do_in_place: *mut Boolean,
    ) -> OSStatus,
    pub BeginIOOperation: extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_id: AudioObjectID,
        client_id: UInt32,
        operation_id: UInt32,
        io_buffer_frame_size: UInt32,
        io_cycle_info: *const c_void,
    ) -> OSStatus,
    pub DoIOOperation: extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_id: AudioObjectID,
        stream_id: AudioObjectID,
        client_id: UInt32,
        operation_id: UInt32,
        io_buffer_frame_size: UInt32,
        io_cycle_info: *const c_void,
        io_main_buffer: *mut c_void,
        io_secondary_buffer: *mut c_void,
    ) -> OSStatus,
    pub EndIOOperation: extern "C" fn(
        driver: AudioServerPlugInDriverRef,
        device_id: AudioObjectID,
        client_id: UInt32,
        operation_id: UInt32,
        io_buffer_frame_size: UInt32,
        io_cycle_info: *const c_void,
    ) -> OSStatus,
}

/// CFUUIDBytes: 16 raw bytes passed by value to QueryInterface.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CFUUIDBytes {
    pub bytes: [u8; 16],
}

// Common HAL property selectors we answer (four-char codes).
pub const kAudioObjectPropertyClass: UInt32 = u32::from_be_bytes(*b"clas");
pub const kAudioObjectPropertyManufacturer: UInt32 = u32::from_be_bytes(*b"lmak");
pub const kAudioPlugInPropertyDeviceList: UInt32 = u32::from_be_bytes(*b"dev#");
pub const kAudioObjectPropertyName: UInt32 = u32::from_be_bytes(*b"lnam");
pub const kAudioDevicePropertyDeviceUID: UInt32 = u32::from_be_bytes(*b"uid ");
pub const kAudioDevicePropertyNominalSampleRate: UInt32 = u32::from_be_bytes(*b"nsrt");
pub const kAudioDevicePropertyStreams: UInt32 = u32::from_be_bytes(*b"stm#");
pub const kAudioStreamPropertyVirtualFormat: UInt32 = u32::from_be_bytes(*b"sfmt");
pub const kAudioStreamPropertyDirection: UInt32 = u32::from_be_bytes(*b"sdir");

// Scopes.
pub const kAudioObjectPropertyScopeGlobal: UInt32 = u32::from_be_bytes(*b"glob");
pub const kAudioObjectPropertyScopeInput: UInt32 = u32::from_be_bytes(*b"inpt");

pub const kAudioHardwareNoError: OSStatus = 0;
pub const kAudioHardwareBadObjectError: OSStatus = u32::from_be_bytes(*b"!obj") as OSStatus;
pub const kAudioHardwareUnknownPropertyError: OSStatus = u32::from_be_bytes(*b"who?") as OSStatus;
pub const kAudioHardwareIllegalOperationError: OSStatus = u32::from_be_bytes(*b"nope") as OSStatus;

/// IO operation ids we care about (read-input).
pub const kAudioServerPlugInIOOperationReadInput: UInt32 = u32::from_be_bytes(*b"read");

// --- CoreFoundation: minimal CFString creation --------------------------
//
// coreaudiod requires the device UID/name/manufacturer as CFStringRefs. We
// link against CoreFoundation symbols by name; they are resolved by the host
// at load time (`-undefined dynamic_lookup`, see build.rs).
pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;

extern "C" {
    pub fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        c_str: *const std::os::raw::c_char,
        encoding: u32,
    ) -> CFStringRef;
}

/// Create a CoreFoundation CFStringRef from a Rust &str. Returns null on
/// failure (the HAL treats that as "no value"). The caller hands ownership to
/// the HAL, which releases it.
pub fn cfstring(s: &str) -> CFStringRef {
    match std::ffi::CString::new(s) {
        Ok(cs) => unsafe {
            CFStringCreateWithCString(std::ptr::null(), cs.as_ptr(), kCFStringEncodingUTF8)
        },
        Err(_) => std::ptr::null(),
    }
}

const _: () = assert!(std::mem::size_of::<c_int>() == 4);
