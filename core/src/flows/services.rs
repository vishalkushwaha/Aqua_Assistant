//! The stores of the flows, held by one object (card OA13-03, task AQ-12 of the OpenCode audit).
//!
//! The flow store, the run store, the schedule store, the notification store, the notifier and the comment inbox used to be six `static OnceLock`s in three files of the HTTP layer
//! (`api_gateway/flow_api.rs`, `flow_sched_api.rs`, `flow_comments_api.rs`), and the domain code that needed a store had to reach up into the HTTP layer for it. They are now the fields of
//! [`FlowServices`], which belongs to the flows; the HTTP layer keeps its old functions (`flow_api::store()`, `run_store()`, `flow_sched_api::sched_store()`, `notify_store()`, `notifier()`,
//! `flow_comments_api::inbox()`, `flow_api::db_path()`) as thin calls to [`global`], so every caller, every test and every route gets the same value it got before.
//!
//! **Same values at the same times.** Nothing is made at start-up. As before, each store is made the first time somebody asks for it, from the path [`db_path`] gives at that moment
//! (`AQUA_FLOWS_DB` moves it; the tests set it before their first call), and every later call gets that same store. What is new is that a set of stores can also be made on a database of its
//! own ([`FlowServices::at`]), which a test can use without touching the one of the process and without a lock that serialises it with the other tests.

use crate::flows::comments_inbox::CommentInbox;
use crate::flows::notify::{Notifier, NotifyStore};
use crate::flows::run::store::RunStore;
use crate::flows::sched::store::SchedStore;
use crate::flows::store::FlowStore;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// Where flows, their runs and their history live. `AQUA_FLOWS_DB` moves it (tests use this so they
/// never touch real data).
pub fn db_path() -> PathBuf {
    std::env::var_os("AQUA_FLOWS_DB").map(PathBuf::from).unwrap_or_else(|| crate::runtime::workspace_root().join("data").join("flows.db"))
}

/// The stores of the flows. Each is made on first use and kept.
pub struct FlowServices {
    /// The database file of every store of this set; `None` asks [`db_path`] when each store is first made (what the one of the process does).
    at: Option<PathBuf>,
    flows: OnceLock<Arc<FlowStore>>,
    runs: OnceLock<Arc<RunStore>>,
    schedules: OnceLock<Arc<SchedStore>>,
    notifications: OnceLock<Arc<NotifyStore>>,
    notifier: OnceLock<Arc<Notifier>>,
    comments: OnceLock<Arc<CommentInbox>>,
}

impl FlowServices {
    const fn lazy() -> Self {
        FlowServices { at: None, flows: OnceLock::new(), runs: OnceLock::new(), schedules: OnceLock::new(), notifications: OnceLock::new(), notifier: OnceLock::new(), comments: OnceLock::new() }
    }

    /// A set of stores on the database file `path` (for a test; the process uses [`global`]).
    pub fn at(path: PathBuf) -> Self {
        FlowServices { at: Some(path), ..Self::lazy() }
    }

    fn path(&self) -> PathBuf {
        self.at.clone().unwrap_or_else(db_path)
    }

    /// The flows, their versions and their history.
    pub fn flows(&self) -> Arc<FlowStore> {
        self.flows.get_or_init(|| Arc::new(FlowStore::new(self.path()))).clone()
    }

    /// The runs of the flows (the same database file).
    pub fn runs(&self) -> Arc<RunStore> {
        self.runs.get_or_init(|| Arc::new(RunStore::new(self.path()))).clone()
    }

    /// The schedules (the flows database file).
    pub fn schedules(&self) -> Arc<SchedStore> {
        self.schedules.get_or_init(|| Arc::new(SchedStore::new(self.path()))).clone()
    }

    /// The notifications.
    pub fn notifications(&self) -> Arc<NotifyStore> {
        self.notifications.get_or_init(|| Arc::new(NotifyStore::new(self.path()))).clone()
    }

    /// The notifier. The `voice` channel puts a due reminder in the queue the app's player empties (`announce_api.rs`).
    pub fn notifier(&self) -> Arc<Notifier> {
        self.notifier.get_or_init(|| Arc::new(Notifier::new(self.notifications()).with_channel(Arc::new(crate::flows::notify_voice::VoiceChannel::live())))).clone()
    }

    /// The inbox of the comment drafts.
    pub fn comments(&self) -> Arc<CommentInbox> {
        self.comments.get_or_init(|| Arc::new(CommentInbox::new(self.path()))).clone()
    }
}

static GLOBAL: FlowServices = FlowServices::lazy();

/// The stores of this process.
pub fn global() -> &'static FlowServices {
    &GLOBAL
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_flow_services_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_store_is_made_once_and_every_call_gets_the_same_one() {
        let d = folder("once");
        let services = FlowServices::at(d.join("flows.db"));
        assert!(Arc::ptr_eq(&services.flows(), &services.flows()));
        assert!(Arc::ptr_eq(&services.runs(), &services.runs()));
        assert!(Arc::ptr_eq(&services.schedules(), &services.schedules()));
        assert!(Arc::ptr_eq(&services.notifications(), &services.notifications()));
        assert!(Arc::ptr_eq(&services.notifier(), &services.notifier()));
        assert!(Arc::ptr_eq(&services.comments(), &services.comments()));
        // The notifier writes to the notification store of its own set, not to another.
        assert!(Arc::ptr_eq(&services.notifier().store, &services.notifications()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn two_sets_of_stores_do_not_share_a_database_and_the_process_has_one() {
        let (a, b) = (folder("set_a"), folder("set_b"));
        let (one, other) = (FlowServices::at(a.join("flows.db")), FlowServices::at(b.join("flows.db")));
        assert!(!Arc::ptr_eq(&one.flows(), &other.flows()));
        assert!(!Arc::ptr_eq(&one.notifier(), &other.notifier()));
        let owner = crate::flows::model::current_owner();
        let def = crate::flows::model::FlowDef { name: "only in the first".into(), ..Default::default() };
        one.flows().create(owner, def, "user", "", 1).unwrap();
        assert_eq!(one.flows().list(owner).unwrap().len(), 1);
        assert!(other.flows().list(owner).unwrap().is_empty(), "the second set sees the flow of the first");
        // The process has one set, and the functions of the HTTP layer are calls to it.
        assert!(std::ptr::eq(global(), global()));
        let _ = (std::fs::remove_dir_all(&a), std::fs::remove_dir_all(&b));
    }
}
