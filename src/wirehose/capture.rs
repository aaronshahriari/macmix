//! Input-device peak metering via a CoreAudio HAL IOProc.
//!
//! When the front-end asks to capture a source node's levels
//! ([`node_capture_start`](`crate::wirehose::CommandSender::node_capture_start`)),
//! we attach an IOProc to that input device. On each input buffer it computes a
//! per-channel peak (with the front-end's ballistics applied), stores it into a
//! shared [`AtomicF32`] array, and — coalesced via the `peaks_dirty` flag — wakes
//! the UI with a [`StateEvent::NodePeaksDirty`]. Output-device (sink) metering
//! needs a process tap and is handled separately.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use coreaudio_sys::{
    AudioBufferList, AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID,
    AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop, AudioObjectID,
    AudioTimeStamp, OSStatus,
};

use crate::atomic_f32::AtomicF32;
use crate::wirehose::event_sender::EventSender;
use crate::wirehose::hal;
use crate::wirehose::stream::{find_peak, PeakProcessor};
use crate::wirehose::{ObjectId, StateEvent};

const NO_ERR: OSStatus = 0;

/// Per-capture state shared with the realtime IOProc via a raw pointer.
struct CaptureState {
    emitter: Arc<EventSender>,
    object_id: ObjectId,
    peaks: Arc<[AtomicF32]>,
    peaks_dirty: Arc<AtomicBool>,
    peak_processor: Option<Arc<dyn PeakProcessor>>,
    rate: u32,
    channels: usize,
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
unsafe extern "C" fn meter_ioproc(
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

struct ActiveCapture {
    device: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    /// Leaked [`CaptureState`]; reclaimed in [`CaptureManager::stop`].
    state: *mut CaptureState,
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
            ActiveCapture {
                device,
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
            AudioDeviceStop(active.device, active.proc_id);
            AudioDeviceDestroyIOProcID(active.device, active.proc_id);
            // Notify the front-end and reclaim the leaked state.
            let state = Box::from_raw(active.state);
            state
                .emitter
                .send(StateEvent::NodeStreamStopped { object_id });
            drop(state);
        }
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
