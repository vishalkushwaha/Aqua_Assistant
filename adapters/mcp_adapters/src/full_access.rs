//! The "full access" switch as the browser policy and the Windows guard see it.
//!
//! Aqua's core flips it together with the tool host's policy (`aqua_tool_host::PolicyEngine::set_full_access`) so that every
//! layer that asks the person a question agrees about whether they have said "work in this project without asking at every
//! step". It only removes questions about ordinary work: opening a site that is not on the list, typing into a browser window,
//! a send shortcut. It never changes a refusal: the deny rules, local-network addresses, shells and administration consoles,
//! Aqua's own window and the browser's own confirmations (uploads, purchases, sending, sensitive fields) are unchanged.

use std::sync::atomic::{AtomicBool, Ordering};

static ON: AtomicBool = AtomicBool::new(false);

pub fn set(on: bool) {
    ON.store(on, Ordering::SeqCst);
}

pub fn is_on() -> bool {
    ON.load(Ordering::SeqCst)
}
