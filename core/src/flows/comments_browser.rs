//! The browser the comment inbox posts replies with. Set once when the server starts; tests set their own.

use crate::flows::browser_exec::Driver;
use std::sync::{Arc, OnceLock};

static BROWSER: OnceLock<Arc<dyn Driver>> = OnceLock::new();

pub fn install(d: Arc<dyn Driver>) {
    let _ = BROWSER.set(d);
}

pub fn get() -> Option<Arc<dyn Driver>> {
    BROWSER.get().cloned()
}
