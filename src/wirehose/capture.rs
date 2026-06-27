//! Peak metering via CoreAudio.
//!
//! Two paths, both feeding the same per-channel [`AtomicF32`] peak array and
//! [`StateEvent::NodePeaksDirty`] wake-up (coalesced via the `peaks_dirty` flag):
//!
//! - **Input** (source nodes): attach a HAL IOProc directly to the input device.
//! - **Output** (sink nodes): create a per-device process tap (macOS 14.4+),
//!   wrap it in a private aggregate device, and run an IOProc on that. This
//!   meters whatever is playing *to that specific output device*.
//!
//! The realtime [`meter_ioproc`] is shared: in both cases the audio to meter
//! arrives as the IOProc's input buffer list.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use coreaudio_sys::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceTapAutoStartKey,
    kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey,
    kAudioObjectPropertyScopeGlobal, kAudioSubTapUIDKey, kAudioTapPropertyFormat,
    AudioBufferList, AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID,
    AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop,
    AudioHardwareCreateAggregateDevice, AudioHardwareDestroyAggregateDevice,
    AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
    AudioStreamBasicDescription, AudioTimeStamp, OSStatus,
};

use core_foundation::array::CFArray;
use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;

use objc2::AnyThread;
use objc2_core_audio::{
    AudioHardwareCreateProcessTap, AudioHardwareDestroyProcessTap,
    CATapDescription,
};
use objc2_foundation::{NSArray, NSNumber, NSString};

use crate::atomic_f32::AtomicF32;
use crate::wirehose::event_sender::EventSender;
use crate::wirehose::hal;
use crate::wirehose::stream::{find_peak, PeakProcessor};
use crate::wirehose::{ObjectId, StateEvent};

const NO_ERR: OSStatus = 0;

/// Per-capture state shared with the realtime IOProc via a raw pointer.
pub(crate) struct CaptureState {
    pub(crate) emitter: Arc<EventSender>,
    pub(crate) object_id: ObjectId,
    pub(crate) peaks: Arc<[AtomicF32]>,
    pub(crate) peaks_dirty: Arc<AtomicBool>,
    pub(crate) peak_processor: Option<Arc<dyn PeakProcessor>>,
    pub(crate) rate: u32,
    pub(crate) channels: usize,
}

unsafe fn update_peak(
    state: &CaptureState,
    channel: usize,
    new_peak: f32,
    n_samples: u32,
) {
    let Some(atomic) = state.peaks.get(channel) else {
        return;
    };
    match &state.peak_processor {
        Some(processor) => {
            let _ = atomic.fetch_update(|current| {
                Some(processor.process_peak(
                    new_peak, current, n_samples, state.rate,
                ))
            });
        }
        None => atomic.store(new_peak),
    }
}

/// The IOProc. Runs on a CoreAudio realtime thread.
pub(crate) unsafe extern "C" fn meter_ioproc(
    _device: AudioObjectID,
    _now: *const AudioTimeStamp,
    input_data: *const AudioBufferList,
    _input_time: *const AudioTimeStamp,
    _output_data: *mut AudioBufferList,
    _output_time: *const AudioTimeStamp,
    client: *mut c_void,
) -> OSStatus {
    if client.is_null() || input_data.is_null() {
        return NO_ERR;
    }
    let state = &*(client as *const CaptureState);
    let list = &*input_data;
    if list.mNumberBuffers == 0 {
        return NO_ERR;
    }
    let buffers = std::slice::from_raw_parts(
        list.mBuffers.as_ptr(),
        list.mNumberBuffers as usize,
    );

    let sample = std::mem::size_of::<f32>();
    if buffers.len() >= state.channels {
        // Non-interleaved: one buffer per channel.
        for channel in 0..state.channels {
            let buf = &buffers[channel];
            let n = buf.mDataByteSize as usize / sample;
            if buf.mData.is_null() || n == 0 {
                continue;
            }
            let samples =
                std::slice::from_raw_parts(buf.mData as *const f32, n);
            update_peak(state, channel, find_peak(samples), n as u32);
        }
    } else {
        // Interleaved: a single buffer of `mNumberChannels` channels.
        let buf = &buffers[0];
        let total = buf.mDataByteSize as usize / sample;
        if !buf.mData.is_null() && total > 0 {
            let samples =
                std::slice::from_raw_parts(buf.mData as *const f32, total);
            let stride = (buf.mNumberChannels as usize).max(1);
            for channel in 0..state.channels.min(stride) {
                let mut peak = 0.0f32;
                let mut count = 0u32;
                let mut i = channel;
                while i < total {
                    peak = peak.max(samples[i].abs());
                    i += stride;
                    count += 1;
                }
                update_peak(state, channel, peak, count);
            }
        }
    }

    // Wake the UI once per dirty cycle (the renderer clears the flag).
    if !state.peaks_dirty.swap(true, Ordering::Relaxed) {
        state.emitter.send(StateEvent::NodePeaksDirty {
            object_id: state.object_id,
        });
    }
    NO_ERR
}

enum ActiveCapture {
    /// Direct IOProc on an input device.
    Input {
        device: AudioObjectID,
        proc_id: AudioDeviceIOProcID,
        /// Leaked [`CaptureState`]; reclaimed in [`CaptureManager::stop`].
        state: *mut CaptureState,
    },
    /// Process tap + aggregate device for an output device.
    Output {
        tap_id: AudioObjectID,
        agg_id: AudioObjectID,
        proc_id: AudioDeviceIOProcID,
        state: *mut CaptureState,
    },
}

/// Tracks the active metering IOProcs, keyed by node [`ObjectId`].
pub struct CaptureManager {
    active: HashMap<u32, ActiveCapture>,
}

impl CaptureManager {
    pub fn new() -> Self {
        Self {
            active: HashMap::new(),
        }
    }

    /// Start metering the given input `device` for `object_id`.
    pub fn start(
        &mut self,
        emitter: Arc<EventSender>,
        object_id: ObjectId,
        device: AudioObjectID,
        peaks_dirty: Arc<AtomicBool>,
        peak_processor: Option<Arc<dyn PeakProcessor>>,
    ) {
        let key: u32 = object_id.into();
        if self.active.contains_key(&key) {
            return;
        }

        let channels = hal::channel_count(device, hal::SCOPE_INPUT).max(1);
        let rate = hal::nominal_sample_rate(device);
        let peaks: Arc<[AtomicF32]> =
            (0..channels).map(|_| AtomicF32::new(0.0)).collect();

        // Tell the front-end the stream is live so it stores `peaks`.
        emitter.send(StateEvent::NodeStreamStarted {
            object_id,
            rate,
            peaks: Arc::clone(&peaks),
        });

        let state = Box::into_raw(Box::new(CaptureState {
            emitter,
            object_id,
            peaks,
            peaks_dirty,
            peak_processor,
            rate,
            channels,
        }));

        let mut proc_id: AudioDeviceIOProcID = None;
        let created = unsafe {
            AudioDeviceCreateIOProcID(
                device,
                Some(meter_ioproc),
                state as *mut c_void,
                &mut proc_id,
            )
        };
        if created != NO_ERR || proc_id.is_none() {
            unsafe { drop(Box::from_raw(state)) };
            return;
        }

        if unsafe { AudioDeviceStart(device, proc_id) } != NO_ERR {
            unsafe {
                AudioDeviceDestroyIOProcID(device, proc_id);
                drop(Box::from_raw(state));
            }
            return;
        }

        self.active.insert(
            key,
            ActiveCapture::Input {
                device,
                proc_id,
                state,
            },
        );
    }

    /// Start metering an output device (`device_uid`) for `object_id` via a
    /// per-device process tap wrapped in a private aggregate device.
    pub fn start_output(
        &mut self,
        emitter: Arc<EventSender>,
        object_id: ObjectId,
        device_uid: String,
        peaks_dirty: Arc<AtomicBool>,
        peak_processor: Option<Arc<dyn PeakProcessor>>,
    ) {
        if self.active.contains_key(&object_id.into()) {
            return;
        }
        // Tap scoped to this device, excluding no processes.
        let exclude: objc2::rc::Retained<NSArray<NSNumber>> =
            NSArray::from_retained_slice(&[]);
        let ns_uid = NSString::from_str(&device_uid);
        let desc = unsafe {
            CATapDescription::initExcludingProcesses_andDeviceUID_withStream(
                CATapDescription::alloc(),
                &exclude,
                &ns_uid,
                0,
            )
        };
        self.start_tap(emitter, object_id, &desc, peaks_dirty, peak_processor);
    }

    /// Shared tap setup: create the tap from `desc`, wrap it in a private
    /// aggregate device, and run a metering IOProc on it.
    fn start_tap(
        &mut self,
        emitter: Arc<EventSender>,
        object_id: ObjectId,
        desc: &CATapDescription,
        peaks_dirty: Arc<AtomicBool>,
        peak_processor: Option<Arc<dyn PeakProcessor>>,
    ) {
        let key: u32 = object_id.into();

        let mut tap_id: AudioObjectID = 0;
        if unsafe { AudioHardwareCreateProcessTap(Some(desc), &mut tap_id) }
            != NO_ERR
            || tap_id == 0
        {
            return;
        }
        let tap_uuid = unsafe { desc.UUID().UUIDString() }.to_string();
        let (channels, rate) = tap_format(tap_id);

        let agg_uid = format!("macmix.tap.{key}");
        let Some(agg_id) = create_tap_aggregate(&agg_uid, &tap_uuid) else {
            unsafe { AudioHardwareDestroyProcessTap(tap_id) };
            return;
        };

        let peaks: Arc<[AtomicF32]> =
            (0..channels).map(|_| AtomicF32::new(0.0)).collect();
        emitter.send(StateEvent::NodeStreamStarted {
            object_id,
            rate,
            peaks: Arc::clone(&peaks),
        });

        let state = Box::into_raw(Box::new(CaptureState {
            emitter,
            object_id,
            peaks,
            peaks_dirty,
            peak_processor,
            rate,
            channels,
        }));

        // Run an IOProc on the aggregate; the tapped audio is its input.
        let mut proc_id: AudioDeviceIOProcID = None;
        let created = unsafe {
            AudioDeviceCreateIOProcID(
                agg_id,
                Some(meter_ioproc),
                state as *mut c_void,
                &mut proc_id,
            )
        };
        if created != NO_ERR
            || proc_id.is_none()
            || unsafe { AudioDeviceStart(agg_id, proc_id) } != NO_ERR
        {
            unsafe {
                if proc_id.is_some() {
                    AudioDeviceDestroyIOProcID(agg_id, proc_id);
                }
                AudioHardwareDestroyAggregateDevice(agg_id);
                AudioHardwareDestroyProcessTap(tap_id);
                drop(Box::from_raw(state));
            }
            return;
        }

        self.active.insert(
            key,
            ActiveCapture::Output {
                tap_id,
                agg_id,
                proc_id,
                state,
            },
        );
    }

    /// Stop metering for `object_id`, if active.
    pub fn stop(&mut self, object_id: ObjectId) {
        let key: u32 = object_id.into();
        let Some(active) = self.active.remove(&key) else {
            return;
        };
        unsafe {
            let state = match active {
                ActiveCapture::Input {
                    device,
                    proc_id,
                    state,
                } => {
                    AudioDeviceStop(device, proc_id);
                    AudioDeviceDestroyIOProcID(device, proc_id);
                    state
                }
                ActiveCapture::Output {
                    tap_id,
                    agg_id,
                    proc_id,
                    state,
                } => {
                    AudioDeviceStop(agg_id, proc_id);
                    AudioDeviceDestroyIOProcID(agg_id, proc_id);
                    AudioHardwareDestroyAggregateDevice(agg_id);
                    AudioHardwareDestroyProcessTap(tap_id);
                    state
                }
            };
            // Notify the front-end and reclaim the leaked state.
            let state = Box::from_raw(state);
            state
                .emitter
                .send(StateEvent::NodeStreamStopped { object_id });
            drop(state);
        }
    }
}

/// Read a tap's channel count and sample rate from its stream format.
pub(crate) fn tap_format(tap_id: AudioObjectID) -> (usize, u32) {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioTapPropertyFormat,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: 0,
    };
    let mut asbd: AudioStreamBasicDescription = unsafe { std::mem::zeroed() };
    let mut size =
        std::mem::size_of::<AudioStreamBasicDescription>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            tap_id,
            &address,
            0,
            std::ptr::null(),
            &mut size,
            &mut asbd as *mut _ as *mut c_void,
        )
    };
    if status != NO_ERR || asbd.mChannelsPerFrame == 0 {
        (2, 48_000)
    } else {
        (asbd.mChannelsPerFrame as usize, asbd.mSampleRate as u32)
    }
}

/// A `CATapDescription`/aggregate dictionary key constant (a NUL-terminated
/// byte string) as a `CFString`.
pub(crate) fn cfkey(bytes: &[u8]) -> CFString {
    CFString::new(std::str::from_utf8(&bytes[..bytes.len() - 1]).unwrap_or(""))
}

/// Create a private aggregate device containing the given sub-tap.
pub(crate) fn create_tap_aggregate(
    agg_uid: &str,
    sub_tap_uuid: &str,
) -> Option<AudioObjectID> {
    let sub_tap = CFDictionary::from_CFType_pairs(&[(
        cfkey(kAudioSubTapUIDKey).as_CFType(),
        CFString::new(sub_tap_uuid).as_CFType(),
    )]);
    let tap_list = CFArray::from_CFTypes(&[sub_tap]);
    let description = CFDictionary::from_CFType_pairs(&[
        (
            cfkey(kAudioAggregateDeviceUIDKey).as_CFType(),
            CFString::new(agg_uid).as_CFType(),
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
    let status = unsafe {
        AudioHardwareCreateAggregateDevice(
            description.as_concrete_TypeRef() as *const _ as _,
            &mut agg_id,
        )
    };
    if status != NO_ERR || agg_id == 0 {
        None
    } else {
        Some(agg_id)
    }
}

impl Drop for CaptureManager {
    fn drop(&mut self) {
        let ids: Vec<u32> = self.active.keys().copied().collect();
        for key in ids {
            self.stop(ObjectId::from_raw_id(key));
        }
    }
}
