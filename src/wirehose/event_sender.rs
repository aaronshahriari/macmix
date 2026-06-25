use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::wirehose::{Event, StateEvent};

/// Trait for handling [`Event`]s.
pub trait EventHandler: Send + 'static {
    /// Returns `true` if the event was handled successfully, `false` if the
    /// backend thread should shut down.
    fn handle_event(&mut self, event: Event) -> bool;
}

impl<F> EventHandler for F
where
    F: FnMut(Event) -> bool + Send + 'static,
{
    fn handle_event(&mut self, event: Event) -> bool {
        self(event)
    }
}

/// Dispatches [`Event`]s to a handler. When the handler signals that it can no
/// longer accept events (returns `false`), the shared `shutdown` flag is set so
/// the backend thread can stop.
///
/// Uses a `Mutex` so it can be shared (via `Arc`) between the monitoring thread,
/// the [`Session`](`crate::wirehose::Session`) command path, and capture IOProcs
/// running on CoreAudio threads.
pub struct EventSender {
    handler: Mutex<Box<dyn EventHandler>>,
    shutdown: Arc<AtomicBool>,
}

impl EventSender {
    pub fn new<F: EventHandler>(
        handler: F,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            handler: Mutex::new(Box::new(handler)),
            shutdown,
        }
    }

    fn dispatch(&self, event: Event) {
        let ok = match self.handler.lock() {
            Ok(mut handler) => handler.handle_event(event),
            Err(_) => false,
        };
        if !ok {
            self.shutdown.store(true, Ordering::Relaxed);
        }
    }

    pub fn send(&self, event: StateEvent) {
        self.dispatch(Event::State(event));
    }

    pub fn send_ready(&self) {
        self.dispatch(Event::Ready);
    }

    #[allow(dead_code)]
    pub fn send_error(&self, error: String) {
        self.dispatch(Event::Error(error));
    }
}
