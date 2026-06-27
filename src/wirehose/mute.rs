//! Per-application metering and mute.
//!
//! A process node keeps at most one tap, wrapped in a **tap-only** aggregate
//! device (never a real output device, so this can never "absorb" hardware the
//! way a re-render engine could). The tap's mute behavior reflects the desired
//! state:
//!
//! - **Unmuted**: the app plays normally and we meter it (when visible).
//! - **Muted** (`CATapMuteBehavior::Muted`): the app's output is suppressed
//!   while the tap exists; we still meter it.
//!
//! A tap exists whenever the node is being metered (visible) or muted, so mute
//! persists across tab changes. Worst case on an unclean exit is an app staying
//! muted until CoreAudio is restarted — never a vanished device.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use coreaudio_sys::{
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID,
    AudioDeviceStart, AudioDeviceStop, AudioHardwareDestroyAggregateDevice,
    AudioObjectID, OSStatus,
};

use objc2::AnyThread;
use objc2_core_audio::{
    AudioHardwareCreateProcessTap, AudioHardwareDestroyProcessTap,
    CATapDescription, CATapMuteBehavior,
};
use objc2_foundation::{NSArray, NSNumber};

use crate::atomic_f32::AtomicF32;
use crate::wirehose::capture::{
    create_tap_aggregate, meter_ioproc, tap_format, CaptureState,
};
use crate::wirehose::event_sender::EventSender;
use crate::wirehose::stream::PeakProcessor;
use crate::wirehose::{ObjectId, StateEvent};

const NO_ERR: OSStatus = 0;

/// Active tap resources for a process node.
struct Active {
    tap_id: AudioObjectID,
    agg_id: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    state: *mut CaptureState,
    /// Whether the tap was built with `Muted` behavior.
    muted: bool,
}

/// Desired state plus active resources for one process node.
struct Proc {
    process: AudioObjectID,
    /// The UI is metering this node (visible on the Playback tab).
    capturing: bool,
    muted: bool,
    peaks_dirty: Arc<AtomicBool>,
    peak_processor: Option<Arc<dyn PeakProcessor>>,
    active: Option<Active>,
}

/// Manages per-application taps for metering and mute.
pub struct ProcessManager {
    emitter: Arc<EventSender>,
    procs: HashMap<u32, Proc>,
}

impl ProcessManager {
    pub fn new(emitter: Arc<EventSender>) -> Self {
        Self {
            emitter,
            procs: HashMap::new(),
        }
    }

    fn entry(
        &mut self,
        object_id: ObjectId,
        process: AudioObjectID,
    ) -> &mut Proc {
        self.procs.entry(object_id.into()).or_insert_with(|| Proc {
            process,
            capturing: false,
            muted: false,
            peaks_dirty: Arc::new(AtomicBool::new(false)),
            peak_processor: None,
            active: None,
        })
    }

    /// The UI wants to meter this process node (it became visible).
    pub fn start_metering(
        &mut self,
        object_id: ObjectId,
        process: AudioObjectID,
        peaks_dirty: Arc<AtomicBool>,
        peak_processor: Option<Arc<dyn PeakProcessor>>,
    ) {
        let proc = self.entry(object_id, process);
        proc.capturing = true;
        proc.peaks_dirty = peaks_dirty;
        proc.peak_processor = peak_processor;
        self.reconcile(object_id);
    }

    /// The UI stopped metering this node (scrolled off / tab changed).
    pub fn stop_metering(&mut self, object_id: ObjectId) {
        if let Some(proc) = self.procs.get_mut(&object_id.into()) {
            proc.capturing = false;
            self.reconcile(object_id);
        }
    }

    /// Set the application's mute state.
    pub fn set_mute(
        &mut self,
        object_id: ObjectId,
        process: AudioObjectID,
        muted: bool,
    ) {
        self.entry(object_id, process).muted = muted;
        self.reconcile(object_id);
        self.echo_mute(object_id);
    }

    /// Echo the stored mute state so the UI reflects it (the front-end waits
    /// for the backend to confirm).
    fn echo_mute(&self, object_id: ObjectId) {
        if let Some(proc) = self.procs.get(&object_id.into()) {
            self.emitter.send(StateEvent::NodeMute {
                object_id,
                mute: proc.muted,
            });
        }
    }

    /// Bring active resources in line with the desired state.
    fn reconcile(&mut self, object_id: ObjectId) {
        let Some(proc) = self.procs.get(&object_id.into()) else {
            return;
        };
        let want_tap = proc.capturing || proc.muted;
        let want_muted = proc.muted;

        let correct = match &proc.active {
            Some(active) => want_tap && active.muted == want_muted,
            None => !want_tap,
        };
        if correct {
            return;
        }

        self.teardown(object_id);
        if want_tap {
            self.build(object_id, want_muted);
        }
    }

    fn build(&mut self, object_id: ObjectId, muted: bool) {
        let Some(proc) = self.procs.get_mut(&object_id.into()) else {
            return;
        };
        let process = proc.process;

        let include: objc2::rc::Retained<NSArray<NSNumber>> =
            NSArray::from_retained_slice(&[NSNumber::new_u32(process)]);
        let desc = unsafe {
            CATapDescription::initStereoMixdownOfProcesses(
                CATapDescription::alloc(),
                &include,
            )
        };
        if muted {
            // Suppress the app's output while the tap exists.
            unsafe { desc.setMuteBehavior(CATapMuteBehavior::Muted) };
        }

        let mut tap_id: AudioObjectID = 0;
        if unsafe { AudioHardwareCreateProcessTap(Some(&desc), &mut tap_id) }
            != NO_ERR
            || tap_id == 0
        {
            return;
        }
        let tap_uuid = unsafe { desc.UUID().UUIDString() }.to_string();
        let (channels, rate) = tap_format(tap_id);

        let key: u32 = object_id.into();
        let agg_uid = format!("macmix.proc.{key}");
        // Tap-only aggregate: no real device is ever wrapped.
        let Some(agg_id) = create_tap_aggregate(&agg_uid, &tap_uuid) else {
            unsafe { AudioHardwareDestroyProcessTap(tap_id) };
            return;
        };

        let peaks: Arc<[AtomicF32]> =
            (0..channels.max(1)).map(|_| AtomicF32::new(0.0)).collect();
        self.emitter.send(StateEvent::NodeStreamStarted {
            object_id,
            rate,
            peaks: Arc::clone(&peaks),
        });

        let state = Box::into_raw(Box::new(CaptureState {
            emitter: Arc::clone(&self.emitter),
            object_id,
            peaks,
            peaks_dirty: Arc::clone(&proc.peaks_dirty),
            peak_processor: proc.peak_processor.clone(),
            rate,
            channels: channels.max(1),
        }));

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

        proc.active = Some(Active {
            tap_id,
            agg_id,
            proc_id,
            state,
            muted,
        });
    }

    fn teardown(&mut self, object_id: ObjectId) {
        let Some(proc) = self.procs.get_mut(&object_id.into()) else {
            return;
        };
        let Some(active) = proc.active.take() else {
            return;
        };
        unsafe {
            AudioDeviceStop(active.agg_id, active.proc_id);
            AudioDeviceDestroyIOProcID(active.agg_id, active.proc_id);
            AudioHardwareDestroyAggregateDevice(active.agg_id);
            AudioHardwareDestroyProcessTap(active.tap_id);
            drop(Box::from_raw(active.state));
        }
        self.emitter
            .send(StateEvent::NodeStreamStopped { object_id });
    }
}

impl Drop for ProcessManager {
    fn drop(&mut self) {
        let ids: Vec<u32> = self.procs.keys().copied().collect();
        for key in ids {
            self.teardown(ObjectId::from_raw_id(key));
        }
    }
}
