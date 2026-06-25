//! Feasibility probe for output metering via the macOS 14.4+ process-tap API.
//!
//! Creates a global stereo tap, wraps it in a private aggregate device, runs an
//! IOProc for a few seconds, and reports whether macOS granted access (status
//! codes) and whether audio was observed (peak levels). Play some audio while
//! this runs to see non-zero peaks.
//!
//! Run with: `cargo run --example tap_probe`

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use core_foundation::array::CFArray;
use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;

use coreaudio_sys::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceTapAutoStartKey,
    kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey,
    kAudioSubTapUIDKey, AudioDeviceCreateIOProcID, AudioDeviceIOProcID,
    AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
    AudioHardwareDestroyAggregateDevice, AudioObjectID, OSStatus,
};

use objc2::AnyThread;
use objc2_core_audio::{
    AudioHardwareCreateProcessTap, AudioHardwareDestroyProcessTap,
    CATapDescription,
};
use objc2_foundation::{NSArray, NSNumber};

struct Probe {
    calls: AtomicU64,
    max_peak: core_atomic::AtomicF32Bits,
}

// Tiny inline atomic-f32 so the example is self-contained.
mod core_atomic {
    use std::sync::atomic::{AtomicU32, Ordering};
    pub struct AtomicF32Bits(AtomicU32);
    impl AtomicF32Bits {
        pub fn new(v: f32) -> Self {
            Self(AtomicU32::new(v.to_bits()))
        }
        pub fn max_with(&self, v: f32) {
            let mut cur = self.0.load(Ordering::Relaxed);
            loop {
                let m = f32::from_bits(cur).max(v);
                match self.0.compare_exchange_weak(
                    cur,
                    m.to_bits(),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(e) => cur = e,
                }
            }
        }
        pub fn get(&self) -> f32 {
            f32::from_bits(self.0.load(Ordering::Relaxed))
        }
    }
}

unsafe extern "C" fn io_proc(
    _device: AudioObjectID,
    _now: *const coreaudio_sys::AudioTimeStamp,
    input_data: *const coreaudio_sys::AudioBufferList,
    _input_time: *const coreaudio_sys::AudioTimeStamp,
    _output_data: *mut coreaudio_sys::AudioBufferList,
    _output_time: *const coreaudio_sys::AudioTimeStamp,
    client: *mut c_void,
) -> OSStatus {
    if client.is_null() || input_data.is_null() {
        return 0;
    }
    let probe = &*(client as *const Probe);
    probe.calls.fetch_add(1, Ordering::Relaxed);
    let list = &*input_data;
    let buffers = std::slice::from_raw_parts(
        list.mBuffers.as_ptr(),
        list.mNumberBuffers as usize,
    );
    let mut peak = 0.0f32;
    for buf in buffers {
        if buf.mData.is_null() {
            continue;
        }
        let n = buf.mDataByteSize as usize / std::mem::size_of::<f32>();
        let samples = std::slice::from_raw_parts(buf.mData as *const f32, n);
        for &s in samples {
            peak = peak.max(s.abs());
        }
    }
    probe.max_peak.max_with(peak);
    0
}

fn cfkey(bytes: &[u8]) -> CFString {
    // The coreaudio-sys key constants are NUL-terminated byte strings.
    let s = std::str::from_utf8(&bytes[..bytes.len() - 1]).unwrap();
    CFString::new(s)
}

fn main() {
    // 1. Global stereo tap, excluding no processes (capture everything).
    let exclude: objc2::rc::Retained<NSArray<NSNumber>> =
        NSArray::from_retained_slice(&[]);
    let desc = unsafe {
        CATapDescription::initStereoGlobalTapButExcludeProcesses(
            CATapDescription::alloc(),
            &exclude,
        )
    };

    // 2. Create the process tap.
    let mut tap_id: AudioObjectID = 0;
    let tap_status =
        unsafe { AudioHardwareCreateProcessTap(Some(&desc), &mut tap_id) };
    let uuid = unsafe { desc.UUID() };
    let uuid_str = unsafe { uuid.UUIDString() }.to_string();
    println!("tap: status={tap_status} tap_id={tap_id} uuid={uuid_str}");
    if tap_status != 0 || tap_id == 0 {
        println!(">>> tap creation FAILED (likely a permission/entitlement wall)");
        return;
    }

    // 3. Build the aggregate device description with our tap.
    let sub_tap = CFDictionary::from_CFType_pairs(&[(
        cfkey(kAudioSubTapUIDKey).as_CFType(),
        CFString::new(&uuid_str).as_CFType(),
    )]);
    let tap_list = CFArray::from_CFTypes(&[sub_tap]);
    let agg_desc = CFDictionary::from_CFType_pairs(&[
        (
            cfkey(kAudioAggregateDeviceUIDKey).as_CFType(),
            CFString::new("macmix.tap.probe").as_CFType(),
        ),
        (
            cfkey(kAudioAggregateDeviceIsPrivateKey).as_CFType(),
            CFBoolean::true_value().as_CFType(),
        ),
        (
            cfkey(kAudioAggregateDeviceTapAutoStartKey).as_CFType(),
            CFBoolean::true_value().as_CFType(),
        ),
        (
            cfkey(kAudioAggregateDeviceTapListKey).as_CFType(),
            tap_list.as_CFType(),
        ),
    ]);

    let mut agg_id: AudioObjectID = 0;
    let agg_status = unsafe {
        AudioHardwareCreateAggregateDevice(
            agg_desc.as_concrete_TypeRef() as *const _ as _,
            &mut agg_id,
        )
    };
    println!("aggregate: status={agg_status} agg_id={agg_id}");
    if agg_status != 0 || agg_id == 0 {
        println!(">>> aggregate creation FAILED");
        unsafe { AudioHardwareDestroyProcessTap(tap_id) };
        return;
    }

    // 4. Run an IOProc on the aggregate to read the tapped audio.
    let probe = Box::into_raw(Box::new(Probe {
        calls: AtomicU64::new(0),
        max_peak: core_atomic::AtomicF32Bits::new(0.0),
    }));
    let mut proc_id: AudioDeviceIOProcID = None;
    let create = unsafe {
        AudioDeviceCreateIOProcID(
            agg_id,
            Some(io_proc),
            probe as *mut c_void,
            &mut proc_id,
        )
    };
    let start = unsafe { AudioDeviceStart(agg_id, proc_id) };
    println!("ioproc: create={create} start={start}");
    println!("--- listening 4s (play some audio) ---");
    std::thread::sleep(Duration::from_millis(4000));

    let probe_ref = unsafe { &*probe };
    println!(
        "RESULT: ioproc_calls={} max_peak={}",
        probe_ref.calls.load(Ordering::Relaxed),
        probe_ref.max_peak.get()
    );

    // 5. Teardown.
    unsafe {
        AudioDeviceStop(agg_id, proc_id);
        if let Some(pid) = proc_id {
            coreaudio_sys::AudioDeviceDestroyIOProcID(agg_id, Some(pid));
        }
        AudioHardwareDestroyAggregateDevice(agg_id);
        AudioHardwareDestroyProcessTap(tap_id);
        drop(Box::from_raw(probe));
    }
}
