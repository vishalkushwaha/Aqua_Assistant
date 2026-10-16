//! The language-server bridge (card OA12-04, tasks CU-15 and WF-20 of the OpenCode audit): an optional, contained way to ask a language server where a name is defined and used.
//!
//! **It is off.** The tools exist only with `AQUA_LSP=on`, and only for a language that `data/tools/lsp.json` (written by the person; read from nowhere else) has a server for. The compiler checks
//! of `check.run` and the symbol index of `code.symbols` and `code.refs` come first and need none of this; a server is for the person who wants exact answers on a typed language and has
//! measured that the guess from the shape of lines is not enough. Page: `documentation/LSP_BRIDGE.md`.
//!
//! * [`config`]: `lsp.json`: per language an absolute path to a program, its fixed arguments and the SHA-256 of the program. A server without a hash is never started. Nothing is downloaded.
//! * [`client`]: [`client::verify_program`] (the hash, the place of the program) and one running [`client::Server`]: started like the terminal tools start a program (scrubbed environment, the project
//!   folder, a Job Object that ends the whole tree), spoken to over its input and output with every wait timed, every size bounded and every wait ended by a cancel.
//! * [`frame`]: the `Content-Length` framing, read with every size bounded. [`uri`]: `file:` addresses, read strictly.
//! * [`provider`] and [`render`]: the four tools `code.definition`, `code.references`, `code.hover` and `code.document_symbols` (a call that starts a server is classified as running a program, and
//!   the first one of a run asks), and the few lines the model reads in place of the JSON of the server: cleaned, cut, and with nothing that is outside the project.
//! * [`bridge`]: the servers of the runs that are going on: at most one process for a language in a run, started by the first call that needs it, ended when idle, when its run ends, when it did not
//!   answer, or when the bridge is dropped; a limit of requests for a run; a language whose server failed is unavailable for the rest of the run.

pub mod bridge;
pub mod client;
pub mod config;
pub mod frame;
pub mod provider;
pub mod render;
pub mod uri;

pub use bridge::{Bridge, Lease};
pub use client::{LspError, Server};
pub use config::{LspConfig, ServerSpec};
pub use provider::LspProvider;

/// A folder for a unit test to make files in, inside this repository's `target` folder (never the system's temporary folder): empty, and different for each name and each run of the tests.
#[cfg(test)]
pub(crate) fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("target").join("aqua_lsp_unit").join(format!("{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}
