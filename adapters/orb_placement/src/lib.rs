//! Smart placement of Aqua's voice orb: which spot on the screen covers the least of what the person is working on.
//!
//! This crate is the pure part (geometry, picture analysis, decision, remembered spots, settings). It opens no windows and
//! installs no hooks; the desktop shell (`apps/aqua-desktop/src-tauri/src/orb_place.rs`) feeds it what Windows reports
//! and moves the orb window accordingly.

pub mod busy;
pub mod context;
pub mod debounce;
pub mod decide;
pub mod geometry;
pub mod memory;
pub mod scroll;
pub mod settings;

pub use busy::{BusyMap, MapStats, BLOCK};
pub use context::{change, is_browser, normalize_title, page_hash, ContextSig, Why};
pub use debounce::Debouncer;
pub use decide::{decide, usable_area, Decision, HomeKind, Homes, Input};
pub use geometry::{Monitor, Rect};
pub use memory::{AppSpot, PageSpot, Saved, Spot};
pub use scroll::{ScrollGate, ScrollRun};
pub use settings::{Sensitivity, Settings, SCROLL_INTERVAL_MAX, SCROLL_INTERVAL_MIN};
