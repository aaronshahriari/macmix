//! Thin safe-ish wrappers over the CoreAudio HAL property API.
//!
//! Everything here speaks the `AudioObjectGetPropertyData` /
//! `AudioObjectSetPropertyData` idiom against the HAL. These are the building
//! blocks the [`session`](`crate::wirehose::session`) backend uses to enumerate
//! devices, read/write per-device volume and mute, switch the default device,
//! and listen for changes.

#![allow(non_upper_case_globals)]

use std::ffi::c_void;
use std::mem;
use std::ptr;

use coreaudio_sys::{
    kAudioDevicePropertyDeviceUID, kAudioDevicePropertyMute,
    kAudioDevicePropertyNominalSampleRate,
    kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyTransportType,
    kAudioDevicePropertyVolumeScalar, kAudioHardwarePropertyProcessObjectList,
    kAudioProcessPropertyBundleID, kAudioProcessPropertyIsRunningOutput,
    kAudioProcessPropertyPID,
    kAudioHardwarePropertyDefaultInputDevice,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices,
    kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeInput, kAudioObjectPropertyScopeOutput,
    kAudioObjectSystemObject, AudioBufferList, AudioObjectAddPropertyListener,
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectHasProperty, AudioObjectID, AudioObjectIsPropertySettable,
    AudioObjectPropertyAddress, AudioObjectPropertyListenerProc,
    AudioObjectRemovePropertyListener, AudioObjectSetPropertyData, OSStatus,
};

use core_foundation_sys::base::{CFRelease, CFTypeRef};
use core_foundation_sys::string::{
    kCFStringEncodingUTF8, CFStringGetCString, CFStringGetLength, CFStringRef,
};

/// The HAL "main"/"master" element (channel 0 == whole device).
pub const ELEMENT_MAIN: u32 = 0;

pub const SYSTEM_OBJECT: AudioObjectID = kAudioObjectSystemObject;
pub const SCOPE_GLOBAL: u32 = kAudioObjectPropertyScopeGlobal;
pub const SCOPE_OUTPUT: u32 = kAudioObjectPropertyScopeOutput;
pub const SCOPE_INPUT: u32 = kAudioObjectPropertyScopeInput;

pub const PROP_DEVICES: u32 = kAudioHardwarePropertyDevices;
pub const PROP_DEFAULT_OUTPUT: u32 = kAudioHardwarePropertyDefaultOutputDevice;
pub const PROP_DEFAULT_INPUT: u32 = kAudioHardwarePropertyDefaultInputDevice;
pub const PROP_VOLUME_SCALAR: u32 = kAudioDevicePropertyVolumeScalar;
pub const PROP_MUTE: u32 = kAudioDevicePropertyMute;

const NO_ERR: OSStatus = 0;

fn addr(selector: u32, scope: u32, element: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: element,
    }
}

/// Read a fixed-size, `Copy` property value.
unsafe fn get_fixed<T: Copy>(
    object: AudioObjectID,
    address: &AudioObjectPropertyAddress,
) -> Option<T> {
    let mut value: mem::MaybeUninit<T> = mem::MaybeUninit::uninit();
    let mut size = mem::size_of::<T>() as u32;
    let status = AudioObjectGetPropertyData(
        object,
        address,
        0,
        ptr::null(),
        &mut size,
        value.as_mut_ptr() as *mut c_void,
    );
    if status != NO_ERR {
        return None;
    }
    Some(value.assume_init())
}

/// Write a fixed-size, `Copy` property value.
unsafe fn set_fixed<T: Copy>(
    object: AudioObjectID,
    address: &AudioObjectPropertyAddress,
    value: T,
) -> bool {
    let size = mem::size_of::<T>() as u32;
    AudioObjectSetPropertyData(
        object,
        address,
        0,
        ptr::null(),
        size,
        &value as *const T as *const c_void,
    ) == NO_ERR
}

unsafe fn has_property(
    object: AudioObjectID,
    address: &AudioObjectPropertyAddress,
) -> bool {
    AudioObjectHasProperty(object, address) != 0
}

unsafe fn is_settable(
    object: AudioObjectID,
    address: &AudioObjectPropertyAddress,
) -> bool {
    let mut settable: u8 = 0;
    AudioObjectIsPropertySettable(object, address, &mut settable) == NO_ERR
        && settable != 0
}

/// Enumerate all audio device object IDs known to the HAL.
pub fn device_ids() -> Vec<AudioObjectID> {
    let address = addr(PROP_DEVICES, SCOPE_GLOBAL, ELEMENT_MAIN);
    unsafe {
        let mut size: u32 = 0;
        if AudioObjectGetPropertyDataSize(
            SYSTEM_OBJECT,
            &address,
            0,
            ptr::null(),
            &mut size,
        ) != NO_ERR
        {
            return Vec::new();
        }
        let count = size as usize / mem::size_of::<AudioObjectID>();
        let mut ids: Vec<AudioObjectID> = vec![0; count];
        if AudioObjectGetPropertyData(
            SYSTEM_OBJECT,
            &address,
            0,
            ptr::null(),
            &mut size,
            ids.as_mut_ptr() as *mut c_void,
        ) != NO_ERR
        {
            return Vec::new();
        }
        ids.truncate(size as usize / mem::size_of::<AudioObjectID>());
        ids
    }
}

/// Total number of channels the device exposes on the given scope.
pub fn channel_count(device: AudioObjectID, scope: u32) -> usize {
    let address =
        addr(kAudioDevicePropertyStreamConfiguration, scope, ELEMENT_MAIN);
    unsafe {
        let mut size: u32 = 0;
        if AudioObjectGetPropertyDataSize(
            device,
            &address,
            0,
            ptr::null(),
            &mut size,
        ) != NO_ERR
            || size == 0
        {
            return 0;
        }
        let mut buf = vec![0u8; size as usize];
        if AudioObjectGetPropertyData(
            device,
            &address,
            0,
            ptr::null(),
            &mut size,
            buf.as_mut_ptr() as *mut c_void,
        ) != NO_ERR
        {
            return 0;
        }
        let list = &*(buf.as_ptr() as *const AudioBufferList);
        let buffers = std::slice::from_raw_parts(
            list.mBuffers.as_ptr(),
            list.mNumberBuffers as usize,
        );
        buffers.iter().map(|b| b.mNumberChannels as usize).sum()
    }
}

/// Whether the device has at least one channel on the given scope. Used to
/// classify a device as an output (Output scope) or input (Input scope).
pub fn has_channels(device: AudioObjectID, scope: u32) -> bool {
    channel_count(device, scope) > 0
}

pub fn is_output(device: AudioObjectID) -> bool {
    has_channels(device, SCOPE_OUTPUT)
}

pub fn is_input(device: AudioObjectID) -> bool {
    has_channels(device, SCOPE_INPUT)
}

/// The device's nominal sample rate in Hz (defaults to 48000 if unavailable).
pub fn nominal_sample_rate(device: AudioObjectID) -> u32 {
    let address = addr(
        kAudioDevicePropertyNominalSampleRate,
        SCOPE_GLOBAL,
        ELEMENT_MAIN,
    );
    unsafe {
        get_fixed::<f64>(device, &address)
            .map(|r| r as u32)
            .filter(|&r| r > 0)
            .unwrap_or(48_000)
    }
}

unsafe fn cfstring_to_string(cfstr: CFStringRef) -> Option<String> {
    if cfstr.is_null() {
        return None;
    }
    // Worst case 4 UTF-8 bytes per UTF-16 unit, plus NUL.
    let len = CFStringGetLength(cfstr);
    let capacity = (len as usize * 4) + 1;
    let mut buf = vec![0i8; capacity];
    if CFStringGetCString(
        cfstr,
        buf.as_mut_ptr(),
        capacity as isize,
        kCFStringEncodingUTF8,
    ) == 0
    {
        return None;
    }
    let cstr = std::ffi::CStr::from_ptr(buf.as_ptr());
    Some(cstr.to_string_lossy().into_owned())
}

unsafe fn cfstring_property(
    object: AudioObjectID,
    selector: u32,
) -> Option<String> {
    let address = addr(selector, SCOPE_GLOBAL, ELEMENT_MAIN);
    let mut cfstr: CFStringRef = ptr::null();
    let mut size = mem::size_of::<CFStringRef>() as u32;
    let status = AudioObjectGetPropertyData(
        object,
        &address,
        0,
        ptr::null(),
        &mut size,
        &mut cfstr as *mut CFStringRef as *mut c_void,
    );
    if status != NO_ERR || cfstr.is_null() {
        return None;
    }
    let result = cfstring_to_string(cfstr);
    CFRelease(cfstr as CFTypeRef);
    result
}

/// Human-readable device name.
pub fn name(device: AudioObjectID) -> Option<String> {
    unsafe { cfstring_property(device, kAudioObjectPropertyName) }
}

/// Stable unique identifier for the device.
pub fn uid(device: AudioObjectID) -> Option<String> {
    unsafe { cfstring_property(device, kAudioDevicePropertyDeviceUID) }
}

/// The device's transport type as a raw `UInt32` four-char code (e.g. `bltn`,
/// `usb `, `hdmi`, `dprt`, `virt`, `grup`). Returns 0 (Unknown) if unavailable.
pub fn transport_type(device: AudioObjectID) -> u32 {
    let address =
        addr(kAudioDevicePropertyTransportType, SCOPE_GLOBAL, ELEMENT_MAIN);
    unsafe { get_fixed::<u32>(device, &address).unwrap_or(0) }
}

/// CoreAudio transport type for software/virtual devices (`virt`), used by
/// apps like Teams, Zoom, and AppVolume that install their own audio devices.
const TRANSPORT_VIRTUAL: u32 = u32::from_be_bytes([b'v', b'i', b'r', b't']);

/// Whether the device is a virtual (software-installed) device rather than real
/// hardware.
pub fn is_virtual(device: AudioObjectID) -> bool {
    transport_type(device) == TRANSPORT_VIRTUAL
}

/// Enumerate all audio process objects known to the HAL (macOS 14.4+).
pub fn process_ids() -> Vec<AudioObjectID> {
    let address = addr(
        kAudioHardwarePropertyProcessObjectList,
        SCOPE_GLOBAL,
        ELEMENT_MAIN,
    );
    unsafe {
        let mut size: u32 = 0;
        if AudioObjectGetPropertyDataSize(
            SYSTEM_OBJECT,
            &address,
            0,
            ptr::null(),
            &mut size,
        ) != NO_ERR
        {
            return Vec::new();
        }
        let count = size as usize / mem::size_of::<AudioObjectID>();
        let mut ids: Vec<AudioObjectID> = vec![0; count];
        if AudioObjectGetPropertyData(
            SYSTEM_OBJECT,
            &address,
            0,
            ptr::null(),
            &mut size,
            ids.as_mut_ptr() as *mut c_void,
        ) != NO_ERR
        {
            return Vec::new();
        }
        ids.truncate(size as usize / mem::size_of::<AudioObjectID>());
        ids
    }
}

/// Whether the process is currently producing output audio.
pub fn process_is_running_output(process: AudioObjectID) -> bool {
    let address = addr(
        kAudioProcessPropertyIsRunningOutput,
        SCOPE_GLOBAL,
        ELEMENT_MAIN,
    );
    unsafe {
        get_fixed::<u32>(process, &address).is_some_and(|v| v != 0)
    }
}

/// The process's bundle identifier (e.g. `com.apple.Music`), if available.
pub fn process_bundle_id(process: AudioObjectID) -> Option<String> {
    unsafe { cfstring_property(process, kAudioProcessPropertyBundleID) }
}

/// The process's POSIX process id.
pub fn process_pid(process: AudioObjectID) -> i32 {
    let address = addr(kAudioProcessPropertyPID, SCOPE_GLOBAL, ELEMENT_MAIN);
    unsafe { get_fixed::<i32>(process, &address).unwrap_or(0) }
}

extern "C" {
    /// From `<libproc.h>` (libSystem); returns the executable name for a pid.
    fn proc_name(
        pid: std::os::raw::c_int,
        buffer: *mut c_void,
        buffersize: u32,
    ) -> std::os::raw::c_int;
}

/// The executable name for a pid (e.g. `Spotify`), via libproc.
pub fn process_name(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let mut buf = [0u8; 256];
    let n = unsafe {
        proc_name(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32)
    };
    if n <= 0 {
        return None;
    }
    let name = String::from_utf8_lossy(&buf[..n as usize])
        .trim()
        .to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Render a four-char-code transport type as a readable string.
#[allow(dead_code)]
pub fn fourcc(code: u32) -> String {
    if code == 0 {
        return String::from("unkn");
    }
    let bytes = code.to_be_bytes();
    bytes
        .iter()
        .map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '?' })
        .collect()
}

/// Read the device's scalar volume (0.0–1.0). Tries the main element, then
/// falls back to channel 1.
pub fn volume_scalar(device: AudioObjectID, scope: u32) -> Option<f32> {
    unsafe {
        let main = addr(PROP_VOLUME_SCALAR, scope, ELEMENT_MAIN);
        if has_property(device, &main) {
            if let Some(v) = get_fixed::<f32>(device, &main) {
                return Some(v);
            }
        }
        let ch1 = addr(PROP_VOLUME_SCALAR, scope, 1);
        get_fixed::<f32>(device, &ch1)
    }
}

/// Set the device's scalar volume (0.0–1.0). Sets the main element if it's
/// settable, otherwise sets each stereo channel. Returns whether any write
/// succeeded.
pub fn set_volume_scalar(
    device: AudioObjectID,
    scope: u32,
    value: f32,
) -> bool {
    let value = value.clamp(0.0, 1.0);
    unsafe {
        let main = addr(PROP_VOLUME_SCALAR, scope, ELEMENT_MAIN);
        if has_property(device, &main) && is_settable(device, &main) {
            if set_fixed(device, &main, value) {
                return true;
            }
        }
        let mut ok = false;
        for channel in 1u32..=2 {
            let a = addr(PROP_VOLUME_SCALAR, scope, channel);
            if has_property(device, &a) && is_settable(device, &a) {
                ok |= set_fixed(device, &a, value);
            }
        }
        ok
    }
}

/// Read the device's mute state on the given scope.
pub fn mute(device: AudioObjectID, scope: u32) -> Option<bool> {
    unsafe {
        let main = addr(PROP_MUTE, scope, ELEMENT_MAIN);
        if has_property(device, &main) {
            return get_fixed::<u32>(device, &main).map(|m| m != 0);
        }
        let ch1 = addr(PROP_MUTE, scope, 1);
        get_fixed::<u32>(device, &ch1).map(|m| m != 0)
    }
}

/// Set the device's mute state on the given scope.
pub fn set_mute(device: AudioObjectID, scope: u32, muted: bool) -> bool {
    let value: u32 = muted as u32;
    unsafe {
        let main = addr(PROP_MUTE, scope, ELEMENT_MAIN);
        if has_property(device, &main) && is_settable(device, &main) {
            if set_fixed(device, &main, value) {
                return true;
            }
        }
        let mut ok = false;
        for channel in 1u32..=2 {
            let a = addr(PROP_MUTE, scope, channel);
            if has_property(device, &a) && is_settable(device, &a) {
                ok |= set_fixed(device, &a, value);
            }
        }
        ok
    }
}

/// Get the current system default output (or input) device.
pub fn default_device(output: bool) -> Option<AudioObjectID> {
    let selector = if output {
        PROP_DEFAULT_OUTPUT
    } else {
        PROP_DEFAULT_INPUT
    };
    let address = addr(selector, SCOPE_GLOBAL, ELEMENT_MAIN);
    let id = unsafe { get_fixed::<AudioObjectID>(SYSTEM_OBJECT, &address)? };
    if id == 0 {
        None
    } else {
        Some(id)
    }
}

/// Set the system default output (or input) device.
pub fn set_default_device(output: bool, device: AudioObjectID) -> bool {
    let selector = if output {
        PROP_DEFAULT_OUTPUT
    } else {
        PROP_DEFAULT_INPUT
    };
    let address = addr(selector, SCOPE_GLOBAL, ELEMENT_MAIN);
    unsafe { set_fixed(SYSTEM_OBJECT, &address, device) }
}

/// Register a property listener. `proc_` fires on a CoreAudio-managed thread.
/// `context` is passed through verbatim as the listener's client data.
///
/// # Safety
/// `context` must remain valid until the matching [`remove_listener`] call.
pub unsafe fn add_listener(
    object: AudioObjectID,
    selector: u32,
    scope: u32,
    proc_: AudioObjectPropertyListenerProc,
    context: *mut c_void,
) -> bool {
    let address = addr(selector, scope, ELEMENT_MAIN);
    AudioObjectAddPropertyListener(object, &address, proc_, context) == NO_ERR
}

/// Remove a previously registered property listener. Must match the
/// `object`/`selector`/`scope`/`proc_`/`context` used to register.
///
/// # Safety
/// See [`add_listener`].
pub unsafe fn remove_listener(
    object: AudioObjectID,
    selector: u32,
    scope: u32,
    proc_: AudioObjectPropertyListenerProc,
    context: *mut c_void,
) -> bool {
    let address = addr(selector, scope, ELEMENT_MAIN);
    AudioObjectRemovePropertyListener(object, &address, proc_, context)
        == NO_ERR
}
