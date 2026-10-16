//! Self-repair: a record of what Aqua could not do and why, fixes that a person approves with one click, and a report of what happened (cards R7-05, R7-09 and R7-20), with a research on a gap (R7-08, `research`) and a watch that notices problems on a timer (R7-19, `watch`).
//!
//! The loop it starts: notice (`gaps`: a platform that is not built, a folder that is not allowed), propose (`proposals`: a typed change, never code), wait for the person
//! (approval needs the UI token, so a script, the voice or the model cannot give it), apply with an undo that was recorded at the time, and report (`report`).
//! It is data and approvals only. Recording a gap stores a few lines nobody sees until a screen shows them; nothing is applied without `proposals::approve`.
//! See `documentation/SELF_REPAIR.md`.

pub mod gaps;
pub mod proposals;
pub mod report;
pub mod research;
pub mod store;
pub mod watch;

pub use gaps::{note_folder_ask_failed, note_folder_not_allowed, note_refused_file_result, note_refused_steps, record_gap, FolderRefusal};
pub use store::{Gap, GapKind, NewGap, NewProposal, Proposal, RepairError, RepairStore};

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

static STORE: OnceLock<Arc<RepairStore>> = OnceLock::new();

/// Where the gaps and proposals live: `data/self_repair.db`. `AQUA_SELF_REPAIR_DB` moves it (tests use this so they never touch real data).
pub fn db_path() -> PathBuf {
    std::env::var_os("AQUA_SELF_REPAIR_DB").map(PathBuf::from).unwrap_or_else(|| crate::runtime::data_dir().join("self_repair.db"))
}

/// Open the process-wide store (once) and return it. The API gateway calls this when it starts; until it is called, the places that notice a gap write nothing.
/// With `AQUA_SELF_REPAIR_WATCH=on` it also starts the watch thread (once), which records the problems it finds (card R7-19; see `watch`).
pub fn install() -> Arc<RepairStore> {
    let store = STORE.get_or_init(|| Arc::new(RepairStore::new(db_path()))).clone();
    watch::start_if_enabled(&store);
    store
}

/// The process-wide store, if it has been installed.
pub fn installed() -> Option<Arc<RepairStore>> {
    STORE.get().cloned()
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tests_b;

#[cfg(test)]
mod hardening_tests;

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    static NEXT: AtomicI64 = AtomicI64::new(1_000_000);

    /// A number no other test of this process has used: for ids and keys, so tests that share the process-wide store never see each other's rows.
    pub fn unique() -> i64 {
        NEXT.fetch_add(1, Ordering::SeqCst)
    }

    /// A new empty folder for one test.
    pub fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aqua_self_repair_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The process-wide store of the unit tests, installed on first use (in a temporary folder). Tests that use it look at their own rows only.
    pub fn global() -> Arc<RepairStore> {
        STORE.get_or_init(|| Arc::new(RepairStore::new(temp_dir("global").join("self_repair.db")))).clone()
    }
}
