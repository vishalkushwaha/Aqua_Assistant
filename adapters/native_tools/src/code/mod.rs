//! What a folder of code is and how it is checked (card OA4-05, task CU-06 of the OpenCode audit).
//!
//! * [`lang`]: the one table of languages (by file extension), ecosystems and the usual command of each.
//! * [`profile`]: [`profile::detect`] reads the manifests of a folder (never runs anything) and says which ecosystems it holds, its package manager, its test runner and the commands
//!   that test, build, lint and type-check it, as suggestions that `term`'s rules judge later. [`profile::nearest_root`] climbs from a file to the folder that holds its project,
//!   and [`profile::detect_cached`] keeps the answer in a file that is read again only when a manifest has changed.
//!
//! * [`symbols`] and [`provider`] (card OA12-02, tasks CU-08 and TL-13): the symbols of a source file made from the shape of its lines, an index of them that parses only what changed, and the
//!   tools `code.outline` and `code.symbols` (off unless `AQUA_CODE_TOOLS=on`) that answer "what is in this file" and "where is this defined".
//!
//! * [`graph`] and [`map`] (card OA12-03, task CU-09): the files a file imports and the files that import it, read from the shape of the `use`, `import`, `from ... import`, `require` and Go
//!   import lines (a guess, bounded, no parser), a personalised ranking over that graph, the lines that mention a name, and a map of about 250 tokens built from the ranking, the words of a
//!   goal and the names of the symbols. The tools `code.deps` and `code.refs` (with `AQUA_CODE_TOOLS=on`) and `code.map` (with `AQUA_REPO_MAP=on`) are in [`provider`].
//!
//! The `check` module (the tools `project.profile` and `check.run`) is the user of [`lang`] and [`profile`].

pub mod graph;
pub mod lang;
pub mod map;
pub mod profile;
pub mod provider;
pub mod symbols;

pub use lang::{is_source_path, language_of, Ecosystem, Language};
pub use profile::{detect, detect_cached, has_loose_python_tests, nearest_root, CheckKind, CmdSpec, ProjectProfile};
