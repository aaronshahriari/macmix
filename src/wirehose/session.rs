//! Setup and teardown of the CoreAudio HAL monitoring backend.
//!
//! [`Session::spawn()`] starts a background worker thread that enumerates audio
//! devices, emits PipeWire-shaped [`StateEvent`]s describing them, and registers
//! HAL property listeners. Listener callbacks (which fire on CoreAudio-managed
//! threads) push lightweight [`Refresh`] signals to the worker, which re-queries
//! the HAL and re-emits state. [`CommandSender`] writes (volume, mute, default
//! device) are applied directly against the HAL — they're thread-safe — and the
//! resulting change notification drives the reactive re-emit.

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;

use coreaudio_sys::{
    AudioObjectID, AudioObjectPropertyAddress, AudioObjectPropertyListenerProc,
    OSStatus,
};

use crate::wirehose::capture::CaptureManager;
use crate::wirehose::event_sender::EventSender;
use crate::wirehose::stream::PeakProcessor;
use crate::wirehose::{
    hal, CommandSender, EventHandler, ObjectId, PropertyStore, StateEvent,
};

/// Synthetic object ID for the "default" metadata object. Device
/// `AudioObjectID`s are small integers, so the top of the `u32` range won't
/// collide with them.
const METADATA_ID: u32 = u32::MAX;

/// High bit tagging an *input* node so the input and output halves of a duplex
/// device get distinct [`ObjectId`]s.
const INPUT_FLAG: u32 = 0x8000_0000;

/// Signals from HAL property listeners telling the worker to re-query state.
enum Refresh {
    /// The device list changed (a device was added or removed).
    Devices,
    /// A default device (output or input) changed.
    Default,
    /// A specific device's volume or mute changed. Any signal triggers a full
    /// re-enumeration, so the device isn't carried.
    Device,
}

/// Listener client data. mpsc `Sender` is `Send` but not `Sync`, and the HAL may
/// invoke the callback from multiple threads, so guard it with a mutex.
struct ListenerCtx {
    tx: Mutex<Sender<Refresh>>,
}

/// Map a node [`ObjectId`] back to its device and scope.
fn node_target(id: ObjectId) -> (AudioObjectID, u32, bool) {
    let raw: u32 = id.into();
    if raw & INPUT_FLAG != 0 {
        (raw & !INPUT_FLAG, hal::SCOPE_INPUT, false)
    } else {
        (raw, hal::SCOPE_OUTPUT, true)
    }
}

fn output_node_id(device: AudioObjectID) -> ObjectId {
    ObjectId::from_raw_id(device)
}

fn input_node_id(device: AudioObjectID) -> ObjectId {
    ObjectId::from_raw_id(device | INPUT_FLAG)
}

/// Internal `node.name` for an output node (its device UID).
fn output_node_name(uid: &str) -> String {
    String::from(uid)
}

/// Internal `node.name` for an input node (UID with a prefix so it's distinct
/// from the device's output node).
fn input_node_name(uid: &str) -> String {
    format!("in:{uid}")
}

/// Handle for the audio monitoring thread.
///
/// On drop, the monitoring thread is signaled to stop and is joined.
pub struct Session {
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    /// Shared event emitter, also used by capture IOProcs.
    emitter: Arc<EventSender>,
    /// Active input-metering IOProcs.
    captures: Mutex<CaptureManager>,
}

impl Session {
    /// Spawns a thread to monitor the system's audio devices.
    ///
    /// [`Event`](`crate::wirehose::event::Event`)s are sent to the provided
    /// `handler`. Returns a [`Session`] handle for sending commands and for
    /// automatically cleaning up the thread.
    ///
    /// When `show_all_devices` is false, virtual (software-installed) devices
    /// such as those from Teams/Zoom/AppVolume are hidden.
    pub fn spawn<F: EventHandler>(
        show_all_devices: bool,
        handler: F,
    ) -> Result<Self> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let emitter = Arc::new(EventSender::new(handler, Arc::clone(&shutdown)));

        let handle = thread::spawn({
            let emitter = Arc::clone(&emitter);
            let shutdown = Arc::clone(&shutdown);
            move || run(emitter, shutdown, show_all_devices)
        });

        Ok(Self {
            shutdown,
            handle: Some(handle),
            emitter,
            captures: Mutex::new(CaptureManager::new()),
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The shared listener callback. Distinguishes system-object notifications
/// (device list / default device) from per-device notifications (volume / mute)
/// and forwards a [`Refresh`] signal to the worker.
unsafe extern "C" fn listener_cb(
    object_id: AudioObjectID,
    n_addresses: u32,
    addresses: *const AudioObjectPropertyAddress,
    client_data: *mut c_void,
) -> OSStatus {
    if client_data.is_null() {
        return 0;
    }
    let ctx = &*(client_data as *const ListenerCtx);
    let Ok(tx) = ctx.tx.lock() else {
        return 0;
    };

    if object_id == hal::SYSTEM_OBJECT {
        let addrs = std::slice::from_raw_parts(addresses, n_addresses as usize);
        for addr in addrs {
            if addr.mSelector == hal::PROP_DEVICES {
                let _ = tx.send(Refresh::Devices);
            } else {
                let _ = tx.send(Refresh::Default);
            }
        }
    } else {
        let _ = tx.send(Refresh::Device);
    }
    0
}

const LISTENER: AudioObjectPropertyListenerProc = Some(listener_cb);

/// Backend monitoring loop: enumerate, register listeners, emit, then react to
/// HAL change notifications until shut down.
fn run(
    sender: Arc<EventSender>,
    shutdown: Arc<AtomicBool>,
    show_all_devices: bool,
) {
    let (tx, rx) = mpsc::channel::<Refresh>();
    // Leak a stable context pointer for the listeners; reclaimed on teardown.
    let ctx = Box::into_raw(Box::new(ListenerCtx {
        tx: Mutex::new(tx),
    }));
    let ctx_void = ctx as *mut c_void;

    // System-object listeners: device list and default-device changes.
    unsafe {
        hal::add_listener(
            hal::SYSTEM_OBJECT,
            hal::PROP_DEVICES,
            hal::SCOPE_GLOBAL,
            LISTENER,
            ctx_void,
        );
        hal::add_listener(
            hal::SYSTEM_OBJECT,
            hal::PROP_DEFAULT_OUTPUT,
            hal::SCOPE_GLOBAL,
            LISTENER,
            ctx_void,
        );
        hal::add_listener(
            hal::SYSTEM_OBJECT,
            hal::PROP_DEFAULT_INPUT,
            hal::SCOPE_GLOBAL,
            LISTENER,
            ctx_void,
        );
    }

    // Devices we've registered per-device (volume/mute) listeners on.
    let mut watched: HashSet<AudioObjectID> = HashSet::new();
    // Node IDs emitted on the previous pass, to detect removals.
    let mut prev_nodes: HashSet<u32> = HashSet::new();

    // Initial enumeration, then announce readiness.
    enumerate(&sender, &mut watched, &mut prev_nodes, ctx_void, show_all_devices);
    sender.send_ready();

    while !shutdown.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(_) => {
                // Coalesce any queued signals, then do one full re-enumeration.
                while rx.try_recv().is_ok() {}
                enumerate(
                    &sender,
                    &mut watched,
                    &mut prev_nodes,
                    ctx_void,
                    show_all_devices,
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Teardown: remove listeners, then reclaim the context box.
    unsafe {
        for selector in
            [hal::PROP_DEVICES, hal::PROP_DEFAULT_OUTPUT, hal::PROP_DEFAULT_INPUT]
        {
            hal::remove_listener(
                hal::SYSTEM_OBJECT,
                selector,
                hal::SCOPE_GLOBAL,
                LISTENER,
                ctx_void,
            );
        }
        for &device in &watched {
            remove_device_listeners(device, ctx_void);
        }
        drop(Box::from_raw(ctx));
    }
}

/// Register volume/mute listeners for a device on both scopes.
unsafe fn add_device_listeners(device: AudioObjectID, ctx: *mut c_void) {
    for scope in [hal::SCOPE_OUTPUT, hal::SCOPE_INPUT] {
        hal::add_listener(device, hal::PROP_VOLUME_SCALAR, scope, LISTENER, ctx);
        hal::add_listener(device, hal::PROP_MUTE, scope, LISTENER, ctx);
    }
}

unsafe fn remove_device_listeners(device: AudioObjectID, ctx: *mut c_void) {
    for scope in [hal::SCOPE_OUTPUT, hal::SCOPE_INPUT] {
        hal::remove_listener(
            device,
            hal::PROP_VOLUME_SCALAR,
            scope,
            LISTENER,
            ctx,
        );
        hal::remove_listener(device, hal::PROP_MUTE, scope, LISTENER, ctx);
    }
}

/// Build a node [`PropertyStore`] for a device on a given scope.
fn node_props(
    node_name: &str,
    description: &str,
    media_class: &str,
    serial: u32,
) -> PropertyStore {
    PropertyStore::from_pairs([
        ("media.class", media_class),
        ("node.name", node_name),
        ("node.description", description),
        ("node.nick", description),
        ("media.name", description),
        ("object.serial", &serial.to_string()),
    ])
}

/// Convert a CoreAudio scalar volume (0.0–1.0) to the cubic volume the
/// front-end expects (it displays `cbrt` and applies `powi(3)` on set).
fn to_cubic(scalar: f32) -> f32 {
    scalar.powi(3)
}

/// Emit the state for one device-scope as a node.
fn emit_node(
    sender: &EventSender,
    node_id: ObjectId,
    node_name: String,
    description: &str,
    media_class: &str,
    scope: u32,
    device: AudioObjectID,
) {
    let serial: u32 = node_id.into();
    sender.send(StateEvent::NodeProperties {
        object_id: node_id,
        props: node_props(&node_name, description, media_class, serial),
    });

    let scalar = hal::volume_scalar(device, scope).unwrap_or(0.0);
    sender.send(StateEvent::NodeVolumes {
        object_id: node_id,
        volumes: vec![to_cubic(scalar)],
    });

    let muted = hal::mute(device, scope).unwrap_or(false);
    sender.send(StateEvent::NodeMute {
        object_id: node_id,
        mute: muted,
    });
}

/// Full enumeration pass: emit nodes for all current devices, remove vanished
/// ones, sync per-device listeners, and update the default-device metadata.
fn enumerate(
    sender: &EventSender,
    watched: &mut HashSet<AudioObjectID>,
    prev_nodes: &mut HashSet<u32>,
    ctx: *mut c_void,
    show_all_devices: bool,
) {
    // Hide virtual (software-installed) devices unless asked to show everything.
    let devices: Vec<AudioObjectID> = hal::device_ids()
        .into_iter()
        .filter(|&id| show_all_devices || !hal::is_virtual(id))
        .collect();
    let current_devices: HashSet<AudioObjectID> =
        devices.iter().copied().collect();

    // Sync per-device listeners.
    let added: Vec<AudioObjectID> =
        current_devices.difference(watched).copied().collect();
    let removed: Vec<AudioObjectID> =
        watched.difference(&current_devices).copied().collect();
    for device in added {
        unsafe { add_device_listeners(device, ctx) };
        watched.insert(device);
    }
    for device in removed {
        unsafe { remove_device_listeners(device, ctx) };
        watched.remove(&device);
    }

    let mut nodes_now: HashSet<u32> = HashSet::new();

    // Default metadata object.
    sender.send(StateEvent::MetadataMetadataName {
        object_id: ObjectId::from_raw_id(METADATA_ID),
        metadata_name: String::from("default"),
    });

    for &device in &devices {
        let description = hal::name(device)
            .unwrap_or_else(|| format!("Audio Device {device}"));
        let uid = hal::uid(device).unwrap_or_else(|| device.to_string());

        if hal::is_output(device) {
            let id = output_node_id(device);
            emit_node(
                sender,
                id,
                output_node_name(&uid),
                &description,
                "Audio/Sink",
                hal::SCOPE_OUTPUT,
                device,
            );
            nodes_now.insert(id.into());
        }
        if hal::is_input(device) {
            let id = input_node_id(device);
            emit_node(
                sender,
                id,
                input_node_name(&uid),
                &description,
                "Audio/Source",
                hal::SCOPE_INPUT,
                device,
            );
            nodes_now.insert(id.into());
        }
    }

    // Default sink/source, keyed by the corresponding node names.
    if let Some(default_out) = hal::default_device(true) {
        if let Some(uid) = hal::uid(default_out) {
            emit_default(sender, "default.audio.sink", &output_node_name(&uid));
        }
    }
    if let Some(default_in) = hal::default_device(false) {
        if let Some(uid) = hal::uid(default_in) {
            emit_default(
                sender,
                "default.audio.source",
                &input_node_name(&uid),
            );
        }
    }

    // Remove nodes that disappeared since the previous pass.
    for old in prev_nodes.iter() {
        if !nodes_now.contains(old) {
            sender.send(StateEvent::Removed {
                object_id: ObjectId::from_raw_id(*old),
            });
        }
    }
    *prev_nodes = nodes_now;
}

/// Emit a default-device metadata property (value is a `{"name": ...}` JSON
/// object, matching how the front-end reads PipeWire defaults).
fn emit_default(sender: &EventSender, key: &str, node_name: &str) {
    let value = serde_json::json!({ "name": node_name }).to_string();
    sender.send(StateEvent::MetadataProperty {
        object_id: ObjectId::from_raw_id(METADATA_ID),
        subject: 0,
        key: Some(String::from(key)),
        value: Some(value),
    });
}

/// Find a device whose UID matches `uid`.
fn device_by_uid(uid: &str) -> Option<AudioObjectID> {
    hal::device_ids()
        .into_iter()
        .find(|&device| hal::uid(device).as_deref() == Some(uid))
}

impl CommandSender for Session {
    fn node_capture_start(
        &self,
        object_id: ObjectId,
        _object_serial: u64,
        _capture_sink: bool,
        peaks_dirty: Arc<AtomicBool>,
        peak_processor: Option<Arc<dyn PeakProcessor>>,
    ) {
        let (device, _scope, is_output) = node_target(object_id);
        // Input (source) metering only; output (sink) metering needs a tap.
        if is_output {
            return;
        }
        if let Ok(mut captures) = self.captures.lock() {
            captures.start(
                Arc::clone(&self.emitter),
                object_id,
                device,
                peaks_dirty,
                peak_processor,
            );
        }
    }

    fn node_capture_stop(&self, object_id: ObjectId) {
        if let Ok(mut captures) = self.captures.lock() {
            captures.stop(object_id);
        }
    }

    fn node_mute(&self, object_id: ObjectId, mute: bool) {
        let (device, scope, _) = node_target(object_id);
        hal::set_mute(device, scope, mute);
    }

    fn node_volumes(&self, object_id: ObjectId, volumes: Vec<f32>) {
        let Some(&cubic) = volumes.first() else {
            return;
        };
        let (device, scope, _) = node_target(object_id);
        // The front-end stores cubic volume; CoreAudio wants the scalar.
        hal::set_volume_scalar(device, scope, cubic.cbrt());
    }

    fn device_mute(
        &self,
        _object_id: ObjectId,
        _route_index: i32,
        _route_device: i32,
        _mute: bool,
    ) {
        // macmix models devices as plain nodes; no route/device-level path.
    }

    fn device_set_profile(&self, _object_id: ObjectId, _profile_index: i32) {}

    fn device_set_route(
        &self,
        _object_id: ObjectId,
        _route_index: i32,
        _route_device: i32,
    ) {
    }

    fn device_volumes(
        &self,
        _object_id: ObjectId,
        _route_index: i32,
        _route_device: i32,
        _volumes: Vec<f32>,
    ) {
    }

    fn metadata_set_property(
        &self,
        _object_id: ObjectId,
        _subject: u32,
        key: String,
        _type_: Option<String>,
        value: Option<String>,
    ) {
        // Only default-device changes are meaningful on macOS.
        let output = match key.as_str() {
            "default.configured.audio.sink" => true,
            "default.configured.audio.source" => false,
            _ => return,
        };
        let Some(value) = value else {
            return;
        };
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&value)
        else {
            return;
        };
        let Some(name) = parsed["name"].as_str() else {
            return;
        };
        // Recover the device UID from the node name.
        let uid = if output {
            name
        } else {
            name.strip_prefix("in:").unwrap_or(name)
        };
        if let Some(device) = device_by_uid(uid) {
            hal::set_default_device(output, device);
        }
    }
}
