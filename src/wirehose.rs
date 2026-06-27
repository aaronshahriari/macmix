//! Event-based audio backend.
//!
//! Originally a wrapper around pipewire-rs; the macmix fork reimplements the
//! internals against macOS CoreAudio while preserving the same public API
//! (`CommandSender` for control, `StateEvent`/`Event` for updates).
mod capture;
mod command;
mod event;
mod event_sender;
mod hal;
pub mod media_class;
mod mute;
mod object_id;
mod property_store;
mod session;
pub mod state;
mod stream;

pub use command::{Command, CommandSender};
pub use event::{Event, PipewireError, StateEvent};
pub use event_sender::EventHandler;
pub use object_id::ObjectId;
pub use property_store::PropertyStore;
pub use session::Session;
pub use stream::PeakProcessor;
