//! Headless backend probe. Spawns the CoreAudio session, enumerates devices,
//! then starts input metering on every source (input) device and prints live
//! peak levels for a few seconds. Useful for verifying the backend without the
//! TUI. (Make some noise into a mic to see the peaks move.)
//!
//! Run with: `cargo run --example probe` (add `--all` to include virtual devices)

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use macmix::atomic_f32::AtomicF32;
use macmix::wirehose::{CommandSender, Event, ObjectId, Session, StateEvent};

fn main() {
    let (tx, rx) = mpsc::channel::<Event>();
    let tx = Arc::new(tx);
    let handler = {
        let tx = Arc::clone(&tx);
        move |event| tx.send(event).is_ok()
    };

    let show_all = std::env::args().any(|a| a == "--all");
    let session = Session::spawn(show_all, handler).expect("spawn session");

    let mut sources: Vec<ObjectId> = Vec::new();
    let mut dirty: HashMap<u32, Arc<AtomicBool>> = HashMap::new();
    let mut peaks: HashMap<u32, Arc<[AtomicF32]>> = HashMap::new();
    let mut capture_started = false;

    let deadline = Instant::now() + Duration::from_millis(4000);
    while Instant::now() < deadline {
        // After the first second, start metering every source device.
        if !capture_started && Instant::now() > deadline - Duration::from_millis(3000) {
            capture_started = true;
            for &id in &sources {
                let flag = Arc::new(AtomicBool::new(false));
                dirty.insert(id.into(), Arc::clone(&flag));
                session.node_capture_start(id, 0, true, flag, None);
            }
            println!("--- started metering {} source(s) ---", sources.len());
        }

        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::State(StateEvent::NodeProperties { object_id, props })) => {
                if props.media_class().map(String::as_str) == Some("Audio/Source")
                {
                    if !sources.contains(&object_id) {
                        sources.push(object_id);
                    }
                }
            }
            Ok(Event::State(StateEvent::NodeStreamStarted {
                object_id,
                rate,
                peaks: p,
            })) => {
                println!(
                    "stream started: {:?} rate={rate} channels={}",
                    object_id,
                    p.len()
                );
                peaks.insert(object_id.into(), p);
            }
            Ok(Event::State(StateEvent::NodePeaksDirty { object_id })) => {
                let key: u32 = object_id.into();
                if let Some(p) = peaks.get(&key) {
                    let levels: Vec<f32> =
                        p.iter().map(|a| a.load()).collect();
                    if levels.iter().any(|&v| v > 0.0001) {
                        println!("peaks {key}: {levels:?}");
                    }
                }
                if let Some(flag) = dirty.get(&key) {
                    flag.store(false, Ordering::Relaxed); // mimic the renderer
                }
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    for &id in &sources {
        session.node_capture_stop(id);
    }
}
