//! Filesystem tools (`fs.*`).

pub mod diff;
pub mod guard;
pub mod lines;
pub mod locks;
pub mod masked;
pub mod outline;
pub mod page;
pub mod patch;
pub mod provider;
pub mod trash;
pub mod walk;
pub mod ast;

pub use guard::{Access, FsConfig, GrantPlan, Kept, Limits, Op, PathGuard, Resolved, Root, MAX_GRANTS, OUT_ALIAS};
pub use provider::{EffectReport, EffectUndo, FsProvider};
pub use trash::{EffectRecord, Trash};
