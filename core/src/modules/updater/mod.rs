//! Verification building blocks for updates and model downloads (Ed25519 signature check against a pinned key, SHA-256
//! model check, `.bak` swap, a manifest of model files).
//!
//! **Not connected.** Nothing in Aqua downloads, verifies or installs an update today: no route, no schedule and no
//! UI calls this code (only tests do). Before wiring it in, note what it does not yet cover: the signature is over the
//! binary's bytes only, so the manifest's `version`, `binary_url` and model hashes are not authenticated; there is no
//! rollback protection (an old signed release can be replayed); and the staged file is not the running program. The
//! model downloader writes a manifest of expected files but fetches and hashes nothing. Models are fetched with the
//! pinned-hash scripts under `scripts/` instead.
pub mod model_downloader;
pub mod ota_updater;

pub use model_downloader::ModelDownloader;
pub use ota_updater::{ModelChecksum, OtaUpdater, UpdateManifest};

pub fn status() -> &'static str {
    "Updater: verification code only, not connected to anything"
}

