pub mod session_watcher;

pub use session_watcher::SessionWatcher;

pub fn platform_name() -> &'static str {
    std::env::consts::OS
}
