//! Device connection and battery monitoring for Aqua.
//!
//! * `model`    plain data: devices, readings, events.
//! * `rules`    pure rules: safe names, device types, stable ids.
//! * `settings` what the user can change, and the remembered devices (one atomic JSON file).
//! * `battery`  the per-device battery state machine.
//! * `notify`   the announcement queue (priority, expiry, redelivery of critical items).
//! * `engine`   the decision maker: baseline, settle, flap guard, de-duplication, preferences, rate limit.
//! * `source`   Windows: device arrival and removal, battery readers, the computer's own battery.
//! * `service`  the threads that connect the sources to the engine and keep a snapshot for the API.
//!
//! The engine is pure (no clock, no Windows, no speech), so everything that decides *whether and what to say* is tested
//! with a scripted clock. Plan and review record: `documentation/DEVICE_MONITOR_PLAN.md`.

pub mod battery;
pub mod engine;
pub mod model;
pub mod notify;
pub mod rules;
pub mod service;
pub mod settings;
pub mod source;

#[cfg(test)]
mod engine_tests;

pub use engine::{Action, DeliveryContext, Engine, Snapshot};
pub use service::{global, init_global, Monitor};
pub use model::*;
pub use notify::{Allow, Announcement};
pub use settings::{DevicePrefs, Settings, State};
