//! Runtime paths and model inventory checks (no downloads — presence only).

mod models_inventory;

/// The data-folder tripwire and the per-thread folder overrides of the tests (QA-04, CX-15). Not in a release build.
#[cfg(any(test, feature = "test-support"))]
pub mod data_tripwire;

pub use models_inventory::{inspect_models, ModelsInventory};

use std::path::PathBuf;

/// Repository-relative `data/models` directory.
#[track_caller]
pub fn default_models_dir() -> PathBuf {
    data_dir().join("models")
}

/// Aqua's data folder (`<workspace root>/data`): tokens, databases, tool policy, audit log, models. Everything
/// that needs a path under it asks here, so no part of Aqua can end up looking in a different folder.
#[track_caller]
pub fn data_dir() -> PathBuf {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(dir) = data_tripwire::data_dir_override() {
        return dir;
    }
    workspace_root().join("data")
}

/// The folder Aqua keeps its data in: `AQUA_WORKSPACE_ROOT`, else the repository root in a development checkout,
/// else `%LOCALAPPDATA%\Aqua`. The rule lives in one place (`aqua_mcp_adapters::aqua_workspace_root`) so the core,
/// `aqua-cli` and the browser setup cannot disagree, and it does not depend on a debug or release build.
///
/// In a test, a lookup that ends in the owner's real folder is reported (or fails with `AQUA_TEST_STRICT_DATA=1`), and a test can give its
/// thread another folder: see [`data_tripwire`]. `#[track_caller]` is there so that the report names the test's own line.
#[track_caller]
pub fn workspace_root() -> PathBuf {
    #[cfg(any(test, feature = "test-support"))]
    {
        if let Some(root) = data_tripwire::workspace_root_override() {
            return root;
        }
        data_tripwire::install();
    }
    aqua_mcp_adapters::aqua_workspace_root()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_core_and_the_other_tools_use_the_same_data_folder() {
        // This test means to look at the real folder (it only compares paths), so the tripwire is told.
        data_tripwire::allow_real_data(|| {
            assert_eq!(workspace_root(), aqua_mcp_adapters::aqua_workspace_root());
            assert_eq!(data_dir(), workspace_root().join("data"));
            assert_eq!(default_models_dir(), data_dir().join("models"));
        });
    }
}

/// Does something already accept connections on this local port? (Used before startup touches anything shared.)
pub fn port_in_use(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(400)).is_ok()
}

#[cfg(test)]
mod port_tests {
    use super::port_in_use;

    #[test]
    fn a_port_that_is_listening_is_in_use_and_a_free_one_is_not() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(port_in_use(port));
        drop(l);
        assert!(!port_in_use(port));
    }
}
