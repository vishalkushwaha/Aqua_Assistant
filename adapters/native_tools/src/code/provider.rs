//! `code.outline` and `code.symbols`: what is in a source file, and where a name is defined (card OA12-02; tasks CU-08 and TL-13 of the OpenCode audit).
//!
//! * `code.outline {path, max_symbols?}` lists the symbols of one file with their lines (`L120-188 fn parse_config(path: &Path) -> Config`, a method indented under its type), in a tenth of the
//!   characters of the file, so that the model asks `fs.read` for exactly the lines it needs.
//! * `code.symbols {query, path?, kind?, limit?}` finds the symbols of the folder (the first allowed folder when `path` is left out) whose name, or whose name with the type it is in, is
//!   the query or has its words, best first, as `file:line signature`.
//!
//! Card OA12-03 (task CU-09) adds three more, from the import lines of the files (see [`super::graph`] and [`super::map`]):
//!
//! * `code.deps {path, root?}` says what one file imports and which files import it, in the folder of its project.
//! * `code.refs {symbol, path?, limit?}` lists the lines that have a name, as a name and not as a part of a longer one, by file, tagged `call`, `import`, `test` or `ref`, with where it is
//!   defined named first.
//! * `code.map {goal?, focus?, path?, budget_tokens?}` is a map of the project of about 250 tokens: the files most worth looking at for a goal, and the names in each. It is on offer only
//!   while `AQUA_REPO_MAP` is on, apart from `AQUA_CODE_TOOLS` (it is what a prompt would carry, and a prompt that changes is measured before it is offered: the map is not in any prompt, a run
//!   asks for it).
//!
//! All are **reads through the file tools' own checks**: the path goes through [`PathGuard::resolve`] (a file outside the allowed folders, Aqua's own files, credential folders and a file that
//! looks like it holds secrets are refused), the folder is walked by [`crate::fs::walk`] (every entry is checked, links are not followed, `.gitignore` is read, the file and byte budgets of
//! `fs.grep` hold), a file is opened and checked again on the handle that was opened, and the text of a file is never run. The result is text from a file, so the manifests say
//! `untrusted_output` and the host fences it as it does a page of `fs.read`; a result from a folder that is not the trusted one is private to the person, as for the file tools. The names
//! that are shown are cleaned as the project profile cleans them (a name with a space, a quote or a control character in it is left out and counted). They are on offer only while
//! `AQUA_CODE_TOOLS` (and for the map `AQUA_REPO_MAP`) is on (the provider is given the switches), and the symbols and imports come from the shape of the lines, not a parse (see
//! [`super::symbols`] and [`super::graph`]).

use super::graph::{self, GoModule, GraphFile, Mention, MAX_GRAPH_FILES};
use super::map::{self, MapInput};
use super::profile::{nearest_root, safe_name};
use super::symbols::{self, hit_lines, render_outline, Parsed, Query, Skipped, Stamp, SymbolIndex, TopHits, Unreadable, MAX_FILE_BYTES, SOURCE_GLOB};
use crate::fs::guard::{check_opened, check_opened_at, display, within, Op, PathGuard, Resolved};
use crate::fs::page;
use crate::fs::walk::{self, Glob, Visit, WalkOpts, WalkStats};
use aqua_tool_host::{Annotations, Availability, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The other spellings of the path of a file a model writes (`x-aliases`, read as `path` by the host before anything is checked): the file tools' own list.
const PATH_ALIASES: [&str; 4] = ["file_path", "filePath", "filepath", "file"];
/// The most files `code.symbols` looks at in one call, whatever the file tools' own limit says.
const MAX_FILES: usize = 20_000;

/// The tools and the index of symbols and imports they share.
pub struct CodeProvider {
    guard: Arc<PathGuard>,
    index: Arc<SymbolIndex>,
    /// Are the tools on offer? Asked each time the tools are listed. Not set: they are (a test, or a caller that made the provider to use it).
    switch: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// The same for `code.map`, which has a switch of its own (`AQUA_REPO_MAP`).
    map_switch: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl CodeProvider {
    pub fn new(guard: Arc<PathGuard>) -> Self {
        Self { guard, index: Arc::new(SymbolIndex::new()), switch: None, map_switch: None }
    }

    /// The tools are on offer only while `on` says so (`AQUA_CODE_TOOLS`: new behaviour is off until it is measured, card OA12-02). They stay registered, so that the reference and the status
    /// list them, but are unavailable, are not shown to the agent and cannot be called.
    pub fn with_switch(mut self, on: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        self.switch = Some(on);
        self
    }

    /// `code.map` is on offer only while `on` says so (`AQUA_REPO_MAP`, card OA12-03): the map is what a prompt would carry, and it is off until the rounds a coding task needs have been measured.
    pub fn with_map_switch(mut self, on: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        self.map_switch = Some(on);
        self
    }

    /// The index the tools keep their symbols in (a test reads how many files it has parsed).
    pub fn index(&self) -> Arc<SymbolIndex> {
        self.index.clone()
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

fn arg_u64(args: &Value, key: &str, default: u64) -> u64 {
    args.get(key).and_then(|v| v.as_u64()).unwrap_or(default)
}

/// The files `code.map` is told the run works on: the first eight non-empty strings of `focus`.
fn focus_of(args: &Value) -> Vec<String> {
    args.get("focus").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str()).filter(|s| !s.trim().is_empty()).take(8).map(str::to_string).collect()).unwrap_or_default()
}

fn manifest(name: &str, description: &str, schema: Value) -> ToolManifest {
    ToolManifest {
        name: name.into(),
        title: name.into(),
        description: description.into(),
        classes: vec![PermClass::Read],
        default_decision: Decision::Auto,
        input_schema: schema,
        timeout_default_ms: 15_000,
        timeout_max_ms: 60_000,
        annotations: Annotations { read_only: true, destructive: false, idempotent: true, open_world: false },
        // Names, lines and signatures are text from the project's files: data, never instructions.
        untrusted_output: true,
    }
}

impl ToolProvider for CodeProvider {
    fn id(&self) -> &str {
        "code"
    }

    fn schema_extensions(&self) -> bool {
        true
    }

    fn availability(&self, tool: &str) -> Availability {
        // The map has a switch of its own when it was given one; a provider that was given only the switch of the tools has the map follow it.
        if tool == "code.map" {
            if let Some(on) = &self.map_switch {
                return if on() { Availability::Available } else { Availability::Unavailable("the repo map is off (AQUA_REPO_MAP=on turns it on)".into()) };
            }
        }
        match &self.switch {
            Some(on) if !on() => Availability::Unavailable("the code tools are off (AQUA_CODE_TOOLS=on turns them on)".into()),
            _ => Availability::Available,
        }
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        vec![
            manifest(
                "code.outline",
                "Symbols of one source file with their lines (L12-40 fn name(args)), methods under their type. Then fs.read offset_line for the part you need.",
                json!({"type": "object", "properties": {
                    "path": {"type": "string", "minLength": 1, "maxLength": 1024, "x-aliases": PATH_ALIASES},
                    "max_symbols": {"type": "integer", "minimum": 1, "maximum": 200, "default": 40, "x-clamp": true}
                }, "required": ["path"]}),
            ),
            manifest(
                "code.symbols",
                "Find where a function, class or type is defined, by its name or some words of it, in the project: file:line signature, best first.",
                json!({"type": "object", "properties": {
                    "query": {"type": "string", "minLength": 1, "maxLength": 200, "x-aliases": ["name", "symbol", "pattern"]},
                    "path": {"type": "string", "default": "@ws", "x-aliases": PATH_ALIASES},
                    "kind": {"type": "string", "maxLength": 20, "description": "function, method, class, struct, enum, trait, interface, type, const, module or impl"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 20, "x-clamp": true}
                }, "required": ["query"]}),
            ),
            manifest(
                "code.deps",
                "What one source file imports and which files import it (Rust, TypeScript, JavaScript, Python, Go), from the import lines: a guess.",
                json!({"type": "object", "properties": {
                    "path": {"type": "string", "minLength": 1, "maxLength": 1024, "x-aliases": PATH_ALIASES},
                    "root": {"type": "string", "maxLength": 1024, "description": "the folder whose files make the graph; the project folder of the file when left out"}
                }, "required": ["path"]}),
            ),
            manifest(
                "code.refs",
                "Where a function, class or type is used: the lines with its name, tagged call, import, test or ref, by file, after where it is defined.",
                json!({"type": "object", "properties": {
                    "symbol": {"type": "string", "minLength": 1, "maxLength": 200, "x-aliases": ["name", "query", "pattern"]},
                    "path": {"type": "string", "default": "@ws", "x-aliases": PATH_ALIASES},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 20, "x-clamp": true}
                }, "required": ["symbol"]}),
            ),
            manifest(
                "code.map",
                "A map of the project in about 250 tokens: the files most worth looking at for your goal and the names in each. A guess; give goal and focus.",
                json!({"type": "object", "properties": {
                    "goal": {"type": "string", "maxLength": 500, "x-aliases": ["task", "query"]},
                    "focus": {"type": "array", "maxItems": 8, "items": {"type": "string", "maxLength": 1024}, "description": "files you are working on"},
                    "path": {"type": "string", "x-aliases": ["root"], "description": "the folder to map; the project folder of the first focus file, or the first allowed folder, when left out"},
                    "budget_tokens": {"type": "integer", "minimum": 60, "maximum": 1000, "default": 250, "x-clamp": true}
                }}),
            ),
        ]
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        let raw = match tool {
            "code.outline" | "code.deps" => arg_str(args, "path"),
            "code.symbols" | "code.refs" | "code.map" => match arg_str(args, "path") {
                "" => "@ws",
                p => p,
            },
            other => return Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
        };
        let r = self.guard.resolve(raw, Op::Read)?;
        // Every other path a call names (card OA12-03: the `root` of `code.deps`, the `focus` files of `code.map`) is judged by the guard here too, before anything runs, and a call that names
        // one the guard refuses is refused whole: nothing is left to be checked later, or silently left out.
        let mut sensitive = r.sensitive;
        if tool == "code.deps" && !arg_str(args, "root").is_empty() {
            sensitive |= self.guard.resolve(arg_str(args, "root"), Op::Read)?.sensitive;
        }
        if tool == "code.map" {
            for f in focus_of(args) {
                sensitive |= self.guard.resolve(&f, Op::Read)?.sensitive;
            }
        }
        let mut classes = vec![PermClass::Read];
        if sensitive {
            classes.push(PermClass::Credential);
        }
        let target = display(&r.path);
        Ok(Classification { classes, summary: format!("{} {target}", tool.trim_start_matches("code.")), target, target_allowed: true })
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let guard = self.guard.clone();
        let index = self.index.clone();
        let cancel = ctx.cancel.clone();
        let tool = tool.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || match tool.as_str() {
                "code.outline" => outline(&guard, &args),
                "code.symbols" => symbols(&guard, &index, &args, &cancel),
                "code.deps" => deps(&guard, &index, &args, &cancel),
                "code.refs" => refs(&guard, &index, &args, &cancel),
                "code.map" => repo_map(&guard, &index, &args, &cancel),
                other => Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
            })
            .await
            .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("code task failed: {e}")))?
        })
    }
}

fn io_err(e: std::io::Error, what: &str) -> ToolError {
    let code = match e.kind() {
        std::io::ErrorKind::NotFound => ErrorCode::NotFound,
        std::io::ErrorKind::PermissionDenied => ErrorCode::OsPermissionDenied,
        _ => ErrorCode::BackendError,
    };
    ToolError::new(code, format!("{what}: {e}"))
}

/// The text of a file a walk met: opened, checked again on the handle that was opened, read to at most the size limit, and not a binary file.
fn read_walked(path: &Path) -> Result<String, Unreadable> {
    let mut f = std::fs::File::open(path).map_err(|_| Unreadable::Failed)?;
    check_opened_at(&f, path).map_err(|_| Unreadable::Failed)?;
    let mut bytes = Vec::new();
    (&mut f).take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).map_err(|_| Unreadable::Failed)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(Unreadable::Failed);
    }
    if page::sniff(&bytes) != page::Sniff::Text {
        return Err(Unreadable::NotText);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

// ---- code.outline -----------------------------------------------------------------------------------------------------------------

fn outline(g: &PathGuard, args: &Value) -> Result<ProviderOutput, ToolError> {
    let asked = arg_str(args, "path");
    let r = g.resolve(asked, Op::Read)?;
    let shown = display(&r.path);
    let meta = std::fs::metadata(&r.path).map_err(|e| io_err(e, &shown))?;
    if meta.is_dir() {
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{shown} is a folder; use code.symbols to search it, or fs.list")));
    }
    if r.sensitive {
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{shown} looks like it holds secrets: it has no outline; fs.read shows it with its values hidden")));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(ToolError::new(ErrorCode::ResourceLimit, format!("{shown} is {} bytes: too large for an outline (the most is {MAX_FILE_BYTES}); search it with fs.grep", meta.len())));
    }
    let file_name = r.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if let Some(why) = page::refused_by_extension(&shown, &file_name) {
        return Err(ToolError::new(ErrorCode::InvalidArguments, why));
    }
    let mut f = std::fs::File::open(&r.path).map_err(|e| io_err(e, &shown))?;
    check_opened(&f, &r)?;
    let mut bytes = Vec::new();
    (&mut f).take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).map_err(|e| io_err(e, "read"))?;
    match page::sniff(&bytes) {
        page::Sniff::Utf16 => return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{shown} is a UTF-16 text file; code.outline reads UTF-8 text only"))),
        page::Sniff::Binary => return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{shown} looks like a binary file; code.outline only reads text"))),
        page::Sniff::Text => {}
    }
    let found = symbols::symbols_of(&file_name, &String::from_utf8_lossy(&bytes));
    let max = arg_u64(args, "max_symbols", 40).clamp(1, 200) as usize;
    let text = render_outline(&shown, &found, max, g.limits().view_chars);
    let items: Vec<Value> = found
        .symbols
        .iter()
        .take(200)
        .map(|s| json!({"kind": s.kind, "name": s.name, "start": s.start, "end": s.end, "depth": s.depth, "parent": s.parent, "signature": s.signature}))
        .collect();
    Ok(ProviderOutput {
        data: json!({"path": shown, "language": found.language, "lines": found.total_lines, "symbol_count": found.symbols.len(), "symbols": items, "size": meta.len(), "from": "the shape of the lines"}),
        text,
        private: !r.trusted || r.sensitive,
        ..Default::default()
    })
}

// ---- code.symbols -----------------------------------------------------------------------------------------------------------------

/// The most threads that look at files at once, and the fewest files worth starting them for.
const MAX_WORKERS: usize = 8;
const PARALLEL_FROM: usize = 64;

/// A file the walk handed over, not yet looked at.
struct Pending {
    path: PathBuf,
    /// How it is named in the answer (the path from the searched folder, written with `/`).
    rel: String,
    meta: std::fs::Metadata,
}

/// What looking at some files met besides the answers.
#[derive(Default)]
struct Met {
    generated: usize,
    minified: usize,
    /// Files that were no text, or could not be read: the walk counted them as searched.
    binary: usize,
    unreadable: usize,
    /// The size of those two kinds together.
    unsearched_bytes: u64,
}

/// The answers found in some files, and what was met.
struct Looked {
    top: TopHits,
    met: Met,
}

impl Looked {
    fn new(limit: usize) -> Looked {
        Looked { top: TopHits::new(limit), met: Met::default() }
    }

    fn merge(&mut self, other: Looked) {
        self.top.merge(other.top);
        let (m, o) = (&mut self.met, other.met);
        m.generated += o.generated;
        m.minified += o.minified;
        m.binary += o.binary;
        m.unreadable += o.unreadable;
        m.unsearched_bytes += o.unsearched_bytes;
    }
}

/// The symbols of each of `files` (kept in the index, or parsed now) are offered to the answers. Stops at the next file when `cancel` is set.
fn look_at(index: &SymbolIndex, generation: u64, q: &Query, limit: usize, files: &[Pending], cancel: &AtomicBool) -> Looked {
    let mut out = Looked::new(limit);
    for f in files {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let name = f.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        match index.get(&f.path, &name, Stamp::of(&f.meta), generation, || read_walked(&f.path)) {
            Ok(parsed) => match parsed.skipped {
                Some(Skipped::Generated) => out.met.generated += 1,
                Some(Skipped::Minified) => out.met.minified += 1,
                None => out.top.offer_file(q, &f.rel, &parsed.symbols),
            },
            Err(Unreadable::NotText) => {
                out.met.binary += 1;
                out.met.unsearched_bytes += f.meta.len();
            }
            Err(Unreadable::Failed) => {
                out.met.unreadable += 1;
                out.met.unsearched_bytes += f.meta.len();
            }
        }
    }
    out
}

/// [`look_at`] for all the files, by several threads when there are many: the first call on a big tree reads and parses every source file once, which is most of its time, and the files do
/// not depend on one another. The answer is the same as one thread's (the answers are ordered by what they are and where they are, not by who met them first).
fn look_at_all(index: &SymbolIndex, generation: u64, q: &Query, limit: usize, files: &[Pending], cancel: &AtomicBool) -> Result<Looked, ToolError> {
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, MAX_WORKERS);
    if workers == 1 || files.len() < PARALLEL_FROM {
        return Ok(look_at(index, generation, q, limit, files, cancel));
    }
    let chunk = files.len().div_ceil(workers);
    let parts: Vec<std::thread::Result<Looked>> = std::thread::scope(|s| {
        let handles: Vec<_> = files.chunks(chunk).map(|c| s.spawn(move || look_at(index, generation, q, limit, c, cancel))).collect();
        handles.into_iter().map(|h| h.join()).collect()
    });
    let mut all = Looked::new(limit);
    for part in parts {
        all.merge(part.map_err(|_| ToolError::new(ErrorCode::BackendError, "a thread of the search failed"))?);
    }
    Ok(all)
}

fn symbols(g: &PathGuard, index: &SymbolIndex, args: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
    let query = arg_str(args, "query");
    if query.trim().is_empty() || query.chars().count() > 200 {
        return Err(ToolError::new(ErrorCode::InvalidArguments, "give a query of 1 to 200 characters: a name, or some words of one"));
    }
    let kind = Some(arg_str(args, "kind")).filter(|k| !k.is_empty());
    let q = Query::new(query, kind).map_err(|e| ToolError::new(ErrorCode::InvalidArguments, e))?;
    let raw = match arg_str(args, "path") {
        "" => "@ws",
        p => p,
    };
    let r = g.resolve(raw, Op::Read)?;
    let shown = display(&r.path);
    let limit = arg_u64(args, "limit", 20).clamp(1, 50) as usize;
    let meta = std::fs::metadata(&r.path).map_err(|e| io_err(e, &shown))?;
    let generation = index.begin();
    let parsed_before = index.parses();
    let (mut st, looked) = if meta.is_file() {
        // A file named by its path: no ignore file applies (it was asked for); the size, binary and secret-name checks do.
        let name = r.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut st = WalkStats { visited: 1, matched: 1, ..WalkStats::default() };
        let mut looked = Looked::new(limit);
        if r.sensitive {
            st.skipped_sensitive += 1;
        } else if meta.len() > MAX_FILE_BYTES {
            st.skipped_big += 1;
        } else {
            st.searched = 1;
            st.bytes = meta.len();
            looked = look_at(index, generation, &q, limit, &[Pending { path: r.path.clone(), rel: name, meta: meta.clone() }], cancel);
        }
        (st, looked)
    } else {
        let glob = Glob::new(SOURCE_GLOB).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the list of source files is not a glob: {e}")))?;
        let opts = WalkOpts {
            max_depth: 12,
            max_files: g.limits().find_max_files.min(MAX_FILES),
            max_bytes: g.limits().find_max_scan_bytes,
            skip_sensitive: true,
            glob: Some(glob),
            ..WalkOpts::default()
        };
        // The walk is one thread (it is a level by level reading of folders and decides what is left out); the files it hands over are looked at afterwards, by several.
        let mut pending: Vec<Pending> = Vec::new();
        let st = walk::walk(g, &r.path, &opts, cancel, &mut |e| {
            if e.meta.len() > MAX_FILE_BYTES {
                return Visit::Big;
            }
            pending.push(Pending { path: e.path.to_path_buf(), rel: e.rel.to_string(), meta: e.meta.clone() });
            Visit::Searched
        });
        let looked = look_at_all(index, generation, &q, limit, &pending, cancel)?;
        // What the walk met all of is what the folder holds now: what the index kept of the rest of it was deleted or hidden since.
        if !st.budget_hit && !st.stopped && !cancel.load(Ordering::Relaxed) {
            index.finish(&r.path, generation);
        }
        (st, looked)
    };
    // The walk counted every file it handed over as searched; the ones that were no text or could not be read were not.
    st.searched = st.searched.saturating_sub(looked.met.binary + looked.met.unreadable);
    st.bytes = st.bytes.saturating_sub(looked.met.unsearched_bytes);
    st.skipped_binary += looked.met.binary;
    st.unreadable += looked.met.unreadable;
    let Looked { top, met } = looked;
    let parsed_now = index.parses() - parsed_before;
    let hits = &top.hits;
    let mut notes = String::new();
    if st.budget_hit {
        notes.push_str("\n… the walk stopped at its limit of files or bytes; the deeper folders were not looked at. Narrow path.");
    }
    if met.generated + met.minified > 0 {
        let n = met.generated + met.minified;
        notes.push_str(&format!("\n[{n} generated or minified file{} no symbols here.]", if n == 1 { " has" } else { "s have" }));
    }
    let footer = st.footer(MAX_FILE_BYTES, true).map(|f| format!("\n{f}")).unwrap_or_default();
    // `(kind function)` after the query says that the answer was narrowed.
    let narrowed = kind.map(|k| format!(" (kind {k})")).unwrap_or_default();
    let text = if hits.is_empty() {
        format!("No symbol matching \"{}\"{narrowed}. It looks at names (and the type or the path words), not at text: try fewer words, or fs.grep.{notes}{footer}", query.trim())
    } else {
        let lines = hit_lines(hits);
        let head = format!("{} symbol{} matching \"{}\"{narrowed} in {} file{}", top.matched, if top.matched == 1 { "" } else { "s" }, query.trim(), top.files_with_matches, if top.files_with_matches == 1 { "" } else { "s" });
        let room = g.limits().view_chars.saturating_sub(head.chars().count() + notes.chars().count() + footer.chars().count() + 100);
        let fits = page::fit_lines(&lines, room);
        let more = if top.matched > fits { format!("\n… {} more match{} not shown; narrow with kind, path or more words", top.matched - fits, if top.matched - fits == 1 { "" } else { "es" }) } else { String::new() };
        format!("{head}\n{}{more}{notes}{footer}", lines[..fits].join("\n"))
    };
    let items: Vec<Value> = hits
        .iter()
        .map(|h| json!({"path": h.rel, "line": h.symbol.start, "end": h.symbol.end, "kind": h.symbol.kind, "name": h.symbol.name, "parent": h.symbol.parent, "signature": h.symbol.signature}))
        .collect();
    Ok(ProviderOutput {
        data: json!({
            "path": shown, "query": query.trim(), "symbols": items, "matched": top.matched, "files_with_matches": top.files_with_matches,
            "files_scanned": st.searched, "files_parsed": parsed_now, "generated_or_minified": met.generated + met.minified,
            "truncated": st.budget_hit || top.matched > hits.len(), "left_out": st.counts(), "from": "the shape of the lines",
        }),
        text,
        private: !r.trusted || r.sensitive,
        ..Default::default()
    })
}

// ---- the graph of imports, references and the map (card OA12-03) --------------------------------------------------------------------

/// The most time a call spends reading and looking at files, besides the walk's own limits and the host's time limit: past it the call answers with what it has and says so.
const TIME_BUDGET: Duration = Duration::from_secs(10);
/// The lines `code.refs` keeps for one file.
const REFS_PER_FILE: usize = 6;

/// The files a graph is made of: the source files, and `go.mod` for the module path of Go imports.
fn graph_glob() -> String {
    format!("{{{SOURCE_GLOB},go.mod}}")
}

/// `work` on the files, by several threads when there are many (the same split `code.symbols` makes), the parts in the order of the files.
fn in_parallel<T: Send>(files: &[Pending], work: impl Fn(&[Pending]) -> T + Sync) -> Result<Vec<T>, ToolError> {
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, MAX_WORKERS);
    if workers == 1 || files.len() < PARALLEL_FROM {
        return Ok(vec![work(files)]);
    }
    let chunk = files.len().div_ceil(workers);
    let work = &work;
    let parts: Vec<std::thread::Result<T>> = std::thread::scope(|s| {
        let handles: Vec<_> = files.chunks(chunk).map(|c| s.spawn(move || work(c))).collect();
        handles.into_iter().map(|h| h.join()).collect()
    });
    parts.into_iter().map(|p| p.map_err(|_| ToolError::new(ErrorCode::BackendError, "a thread of the search failed"))).collect()
}

/// What some files gave to a graph.
#[derive(Default)]
struct GraphChunk {
    files: Vec<(String, Arc<Parsed>)>,
    met: Met,
    timed_out: bool,
}

/// The symbols and imports of each of `files` (kept in the index, or parsed now).
fn load_chunk(index: &SymbolIndex, generation: u64, files: &[Pending], cancel: &AtomicBool, deadline: Instant) -> GraphChunk {
    let mut out = GraphChunk::default();
    for f in files {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if Instant::now() > deadline {
            out.timed_out = true;
            break;
        }
        let name = f.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        match index.get(&f.path, &name, Stamp::of(&f.meta), generation, || read_walked(&f.path)) {
            Ok(parsed) => out.files.push((f.rel.clone(), parsed)),
            Err(Unreadable::NotText) => {
                out.met.binary += 1;
                out.met.unsearched_bytes += f.meta.len();
            }
            Err(Unreadable::Failed) => {
                out.met.unreadable += 1;
                out.met.unsearched_bytes += f.meta.len();
            }
        }
    }
    out
}

/// The files of a folder with what they hold, sorted by path, and what the walk left out.
struct Loaded {
    files: Vec<(String, Arc<Parsed>)>,
    go_modules: Vec<GoModule>,
    st: WalkStats,
    timed_out: bool,
}

fn parent_of(rel: &str) -> String {
    rel.rfind('/').map_or("", |p| &rel[..p]).to_string()
}

/// The source files under `root`, walked as `code.symbols` walks (the guard on every entry, `.gitignore`, no links, 20,000 files and 200 MiB at most), with what each says it imports.
fn load_graph(g: &PathGuard, index: &SymbolIndex, root: &Path, cancel: &AtomicBool) -> Result<Loaded, ToolError> {
    let glob = Glob::new(&graph_glob()).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the list of source files is not a glob: {e}")))?;
    let opts = WalkOpts {
        max_depth: 12,
        max_files: g.limits().find_max_files.min(MAX_GRAPH_FILES),
        max_bytes: g.limits().find_max_scan_bytes,
        skip_sensitive: true,
        glob: Some(glob),
        ..WalkOpts::default()
    };
    let deadline = Instant::now() + TIME_BUDGET;
    let generation = index.begin();
    let mut pending: Vec<Pending> = Vec::new();
    let mut go_modules: Vec<GoModule> = Vec::new();
    let mut st = walk::walk(g, root, &opts, cancel, &mut |e| {
        if e.meta.len() > MAX_FILE_BYTES {
            return Visit::Big;
        }
        if e.name == "go.mod" {
            if let Some(path) = read_walked(e.path).ok().and_then(|text| graph::go_module_path(&text)) {
                go_modules.push(GoModule { dir: parent_of(e.rel), path });
            }
            return Visit::Searched;
        }
        pending.push(Pending { path: e.path.to_path_buf(), rel: e.rel.to_string(), meta: e.meta.clone() });
        Visit::Searched
    });
    let chunks = in_parallel(&pending, |c| load_chunk(index, generation, c, cancel, deadline))?;
    let mut files: Vec<(String, Arc<Parsed>)> = Vec::with_capacity(pending.len());
    let mut met = Met::default();
    let mut timed_out = false;
    for c in chunks {
        files.extend(c.files);
        met.binary += c.met.binary;
        met.unreadable += c.met.unreadable;
        met.unsearched_bytes += c.met.unsearched_bytes;
        timed_out |= c.timed_out;
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    st.searched = st.searched.saturating_sub(met.binary + met.unreadable);
    st.bytes = st.bytes.saturating_sub(met.unsearched_bytes);
    st.skipped_binary += met.binary;
    st.unreadable += met.unreadable;
    // What the walk met all of is what the folder holds now: what the index kept of the rest of it was deleted or hidden since.
    if !st.budget_hit && !st.stopped && !timed_out && !cancel.load(Ordering::Relaxed) {
        index.finish(root, generation);
    }
    Ok(Loaded { files, go_modules, st, timed_out })
}

fn build_graph(l: &Loaded) -> graph::Graph {
    let inputs: Vec<GraphFile<'_>> = l.files.iter().map(|(rel, p)| GraphFile { rel, imports: &p.imports }).collect();
    graph::build(&inputs, &l.go_modules)
}

/// A name from the project's files as it may be shown (see [`safe_name`]): `None` for one that has a space, a quote, a control character or anything else a real name does not.
fn shown_name(raw: &str) -> Option<String> {
    safe_name(raw).filter(|n| n == raw)
}

/// `names` as `a, b, c (+4 more)` within `max_chars`; the names that cannot be shown are not, and are counted in the second answer.
fn name_list(names: &[String], max_chars: usize) -> (String, usize) {
    let safe: Vec<String> = names.iter().filter_map(|n| shown_name(n)).collect();
    let hidden = names.len() - safe.len();
    let mut out = String::new();
    let mut taken = 0usize;
    for n in &safe {
        let add = n.chars().count() + 2;
        if out.chars().count() + add > max_chars && taken > 0 {
            break;
        }
        if taken > 0 {
            out.push_str(", ");
        }
        out.push_str(n);
        taken += 1;
    }
    if taken < safe.len() {
        out.push_str(&format!(" (+{} more)", safe.len() - taken));
    }
    (out, hidden)
}

/// The folder whose files make the graph of `file`: `root` when it is given (it must hold the file), else the folder of the project that holds the file (its nearest manifest or `.git`, never
/// above the allowed folder it is in), else the folder of the file. The second answer is whether the folder is a trusted one (a result made from a folder that is not is private to the person).
fn graph_root(g: &PathGuard, file: &Resolved, root_arg: &str) -> Result<(PathBuf, bool), ToolError> {
    if !root_arg.is_empty() {
        let rr = g.resolve(root_arg, Op::Read)?;
        let shown = display(&rr.path);
        if !std::fs::metadata(&rr.path).map_err(|e| io_err(e, &shown))?.is_dir() {
            return Err(ToolError::new(ErrorCode::InvalidArguments, format!("root {shown} is not a folder")));
        }
        if !within(&file.path, &rr.path) {
            return Err(ToolError::new(ErrorCode::InvalidArguments, format!("root {shown} does not hold the file: give a folder above it")));
        }
        return Ok((rr.path, rr.trusted));
    }
    let parent = file.path.parent().map(Path::to_path_buf).unwrap_or_else(|| file.path.clone());
    let limit = g.root_path_of(&file.path).unwrap_or_else(|| parent.clone());
    let found = nearest_root(&file.path, &limit).unwrap_or_else(|| parent.clone());
    // What was found is looked at once more by the guard: a folder it would not let a search start in is not the root, and the file's own folder is.
    Ok(match g.resolve(&display(&found), Op::Read) {
        Ok(r) => (r.path, r.trusted),
        Err(_) => (parent, file.trusted),
    })
}

// ---- code.deps ----

/// What the reading of a language's imports does not follow, in a few words.
fn limits_of(language: &str) -> &'static str {
    match language {
        "Rust" => "inside the crate only: no other crate, re-export or #[path]",
        "Go" => "through go.mod: the files of one package are not linked to each other",
        "Python" => "from the folder of the file and those above it: no installed package",
        _ => "relative paths only: no alias or package",
    }
}

fn deps(g: &PathGuard, index: &SymbolIndex, args: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
    let asked = arg_str(args, "path");
    let r = g.resolve(asked, Op::Read)?;
    let shown = display(&r.path);
    let meta = std::fs::metadata(&r.path).map_err(|e| io_err(e, &shown))?;
    if meta.is_dir() {
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{shown} is a folder; code.deps takes a file (code.symbols searches a folder, code.map maps a project)")));
    }
    if r.sensitive {
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{shown} looks like it holds secrets: it has no import graph")));
    }
    let file_name = r.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if !graph::reads_imports(&file_name) {
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{shown}: imports are read for Rust, TypeScript, JavaScript, Python and Go files only")));
    }
    let (root, root_trusted) = graph_root(g, &r, arg_str(args, "root"))?;
    // The folder's path is shown as one line (it is a name from the disk).
    let root_shown = graph::one_line(&display(&root), 300);
    let loaded = load_graph(g, index, &root, cancel)?;
    let gr = build_graph(&loaded);
    let rel = r.path.strip_prefix(&root).map(display).unwrap_or_default();
    let Some(id) = gr.id_of(&rel).map(|i| i as usize) else {
        return Err(ToolError::new(
            ErrorCode::InvalidArguments,
            format!("{shown} is not among the {} files read from {root_shown} (it is hidden, ignored, too big, or past the limit of the walk): give root, a folder that holds it", gr.files.len()),
        ));
    };
    let language = super::lang::language_of(&file_name).map_or("source", |l| l.name());
    let imports: Vec<String> = gr.out[id].iter().map(|t| gr.files[*t as usize].clone()).collect();
    let importers: Vec<String> = gr.inc[id].iter().map(|t| gr.files[*t as usize].clone()).collect();
    let outside = &gr.external[id];
    let missing = gr.unresolved[id];
    let room = g.limits().view_chars.saturating_sub(700);
    let (imports_text, hidden_a) = name_list(&imports, room / 3);
    let (importers_text, hidden_b) = name_list(&importers, room / 3);
    let (outside_text, _) = name_list(outside, 240);
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("{}: {language} file. Its imports are read from the shape of the lines, a guess ({}).", shown_name(&rel).unwrap_or_else(|| "this file".into()), limits_of(language)));
    lines.push(format!("paths are from {root_shown}"));
    lines.push(if imports.is_empty() { "imports 0 files of this folder".to_string() } else { format!("imports {}: {imports_text}", imports.len()) });
    if !outside.is_empty() {
        lines.push(format!("outside the folder {}: {outside_text}", outside.len()));
    }
    if missing > 0 {
        lines.push(format!("not found {missing}: an alias, a re-export, a file that is not read or one that is not here"));
    }
    lines.push(if importers.is_empty() { "imported by 0 files of this folder".to_string() } else { format!("imported by {}: {importers_text}", importers.len()) });
    if hidden_a + hidden_b > 0 {
        lines.push(format!("({} names with a character that cannot be shown are left out)", hidden_a + hidden_b));
    }
    lines.push(format!("graph: {} files, {} imports between them{}", gr.files.len(), gr.edges, if gr.capped { " (cut at the most a graph keeps)" } else { "" }));
    if loaded.st.budget_hit {
        lines.push("… the walk stopped at its limit of files or bytes; the deeper folders were not read, so the lists may be short. Give root, a smaller folder.".to_string());
    }
    if loaded.timed_out {
        lines.push("… stopped at the time limit; the lists may be short. Give root, a smaller folder.".to_string());
    }
    if let Some(f) = loaded.st.footer(MAX_FILE_BYTES, true) {
        lines.push(f);
    }
    // The data holds the same names as the text: a name that cannot be shown is in neither.
    let all = |ids: &[u32]| -> Vec<Value> { ids.iter().filter_map(|t| shown_name(&gr.files[*t as usize])).take(200).map(|n| json!(n)).collect() };
    let outside_shown: Vec<String> = outside.iter().filter_map(|n| shown_name(n)).collect();
    Ok(ProviderOutput {
        data: json!({
            "path": shown, "relative_path": rel, "root": root_shown, "language": language,
            "imports": all(&gr.out[id]), "imported_by": all(&gr.inc[id]), "external": outside_shown, "not_found": missing,
            "graph_files": gr.files.len(), "graph_edges": gr.edges, "truncated": loaded.st.budget_hit || loaded.timed_out || gr.capped,
            "left_out": loaded.st.counts(), "from": "the shape of the lines",
        }),
        text: lines.join("\n"),
        private: !r.trusted || !root_trusted || r.sensitive,
        ..Default::default()
    })
}

// ---- code.refs ----

/// One file's lines that have the name, and where it defines the name.
struct FileRefs {
    rel: String,
    mentions: Vec<Mention>,
    total: usize,
    defs: Vec<(usize, String)>,
}

#[derive(Default)]
struct RefsChunk {
    files: Vec<FileRefs>,
    met: Met,
    timed_out: bool,
}

/// The lines of each of `files` that have `word` as a name.
fn refs_in(index: &SymbolIndex, generation: u64, word: &str, files: &[Pending], cancel: &AtomicBool, deadline: Instant) -> RefsChunk {
    let mut out = RefsChunk::default();
    for f in files {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if Instant::now() > deadline {
            out.timed_out = true;
            break;
        }
        let text = match read_walked(&f.path) {
            Ok(t) => t,
            Err(Unreadable::NotText) => {
                out.met.binary += 1;
                out.met.unsearched_bytes += f.meta.len();
                continue;
            }
            Err(Unreadable::Failed) => {
                out.met.unreadable += 1;
                out.met.unsearched_bytes += f.meta.len();
                continue;
            }
        };
        if !text.contains(word) {
            continue;
        }
        let name = f.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        // The symbols of a file that has the name say which of its lines define it and which lie in tests (and the index keeps them for the next call).
        let (mut defs, mut test_ranges): (Vec<(usize, String)>, Vec<(usize, usize)>) = (Vec::new(), Vec::new());
        if let Ok(parsed) = index.get(&f.path, &name, Stamp::of(&f.meta), generation, || Ok(text.clone())) {
            for s in &parsed.symbols {
                if s.name == word && s.kind != "impl" {
                    defs.push((s.start, s.signature.clone()));
                }
                if (s.kind == "mod" && matches!(s.name.as_str(), "tests" | "test")) || (matches!(s.kind, "fn" | "function" | "func") && (s.name.starts_with("test_") || s.name.starts_with("Test"))) {
                    test_ranges.push((s.start, s.end));
                }
            }
        }
        let def_lines: HashSet<usize> = defs.iter().map(|d| d.0).collect();
        let in_test = |line: usize| test_ranges.iter().any(|(a, b)| *a <= line && line <= *b);
        let (mentions, total) = graph::mentions(&name, &f.rel, &text, word, &def_lines, &in_test, REFS_PER_FILE);
        if total > 0 || !defs.is_empty() {
            out.files.push(FileRefs { rel: f.rel.clone(), mentions, total, defs });
        }
    }
    out
}

fn refs(g: &PathGuard, index: &SymbolIndex, args: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
    let Some(word) = graph::symbol_name(arg_str(args, "symbol")) else {
        return Err(ToolError::new(ErrorCode::InvalidArguments, "give a name: letters, digits and _ (`Config::parse` and `store.load` stand for `parse` and `load`)"));
    };
    let raw = match arg_str(args, "path") {
        "" => "@ws",
        p => p,
    };
    let r = g.resolve(raw, Op::Read)?;
    let shown = display(&r.path);
    let limit = arg_u64(args, "limit", 20).clamp(1, 50) as usize;
    let meta = std::fs::metadata(&r.path).map_err(|e| io_err(e, &shown))?;
    let generation = index.begin();
    let deadline = Instant::now() + TIME_BUDGET;
    let (mut st, chunks) = if meta.is_file() {
        // A file named by its path is looked at alone; the size, binary and secret-name checks apply.
        let name = r.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut st = WalkStats { visited: 1, matched: 1, ..WalkStats::default() };
        let mut chunks = Vec::new();
        if r.sensitive {
            st.skipped_sensitive += 1;
        } else if meta.len() > MAX_FILE_BYTES {
            st.skipped_big += 1;
        } else {
            st.searched = 1;
            st.bytes = meta.len();
            chunks.push(refs_in(index, generation, &word, &[Pending { path: r.path.clone(), rel: name, meta: meta.clone() }], cancel, deadline));
        }
        (st, chunks)
    } else {
        let glob = Glob::new(SOURCE_GLOB).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the list of source files is not a glob: {e}")))?;
        let opts = WalkOpts { max_depth: 12, max_files: g.limits().find_max_files.min(MAX_GRAPH_FILES), max_bytes: g.limits().find_max_scan_bytes, skip_sensitive: true, glob: Some(glob), ..WalkOpts::default() };
        let mut pending: Vec<Pending> = Vec::new();
        let st = walk::walk(g, &r.path, &opts, cancel, &mut |e| {
            if e.meta.len() > MAX_FILE_BYTES {
                return Visit::Big;
            }
            pending.push(Pending { path: e.path.to_path_buf(), rel: e.rel.to_string(), meta: e.meta.clone() });
            Visit::Searched
        });
        let chunks = in_parallel(&pending, |c| refs_in(index, generation, &word, c, cancel, deadline))?;
        (st, chunks)
    };
    let mut files: Vec<FileRefs> = Vec::new();
    let mut met = Met::default();
    let mut timed_out = false;
    for c in chunks {
        files.extend(c.files);
        met.binary += c.met.binary;
        met.unreadable += c.met.unreadable;
        met.unsearched_bytes += c.met.unsearched_bytes;
        timed_out |= c.timed_out;
    }
    st.searched = st.searched.saturating_sub(met.binary + met.unreadable);
    st.bytes = st.bytes.saturating_sub(met.unsearched_bytes);
    st.skipped_binary += met.binary;
    st.unreadable += met.unreadable;
    // The files of the project first, then the tests, each by path: the answer does not depend on which thread met a file first.
    files.sort_by(|a, b| (graph::is_test_path(&a.rel), &a.rel).cmp(&(graph::is_test_path(&b.rel), &b.rel)));
    let total: usize = files.iter().map(|f| f.total).sum();
    let with_mentions = files.iter().filter(|f| f.total > 0).count();
    let mut defs: Vec<(String, usize, String)> = files.iter().flat_map(|f| f.defs.iter().map(|(l, s)| (f.rel.clone(), *l, s.clone()))).collect();
    defs.sort();
    let mut body: Vec<String> = Vec::new();
    let mut listed: Vec<Value> = Vec::new();
    let mut hidden_files = 0usize;
    for (rel, line, sig) in defs.iter().take(3) {
        if let Some(name) = shown_name(rel) {
            body.push(format!("defined: {name}:{line} {sig}"));
        }
    }
    let mut shown_count = 0usize;
    for f in files.iter().filter(|f| !f.mentions.is_empty()) {
        if shown_count >= limit {
            break;
        }
        let Some(name) = shown_name(&f.rel) else {
            hidden_files += 1;
            continue;
        };
        body.push(name.clone());
        let mut here = 0usize;
        for m in &f.mentions {
            if shown_count >= limit {
                break;
            }
            body.push(format!("  {:>5} {:<6} {}", m.line, m.tag, m.text));
            listed.push(json!({"path": name, "line": m.line, "tag": m.tag, "text": m.text}));
            shown_count += 1;
            here += 1;
        }
        if f.total > here {
            body.push(format!("  (+{} more in this file)", f.total - here));
        }
    }
    let plural = |n: usize, one: &str, many: &str| if n == 1 { one.to_string() } else { many.to_string() };
    let head = if total == 0 {
        format!("No reference to \"{word}\" found: it matches the name as a whole word, in code (not comment lines).")
    } else {
        format!("{total} {} to \"{word}\" in {with_mentions} {}", plural(total, "reference", "references"), plural(with_mentions, "file", "files"))
    };
    let mut notes = String::new();
    if st.budget_hit {
        notes.push_str("\n… the walk stopped at its limit of files or bytes; the deeper folders were not looked at. Narrow path.");
    }
    if timed_out {
        notes.push_str("\n… stopped at the time limit; some files were not looked at. Narrow path.");
    }
    if hidden_files > 0 {
        notes.push_str(&format!("\n({hidden_files} {} with a character that cannot be shown left out)", plural(hidden_files, "file name", "file names")));
    }
    let footer = st.footer(MAX_FILE_BYTES, true).map(|f| format!("\n{f}")).unwrap_or_default();
    let room = g.limits().view_chars.saturating_sub(head.chars().count() + notes.chars().count() + footer.chars().count() + 100);
    let fits = page::fit_lines(&body, room);
    let unseen = total.saturating_sub(shown_count);
    let more = if fits < body.len() {
        format!("\n… {} more lines not shown; narrow with path, or ask for fewer with limit", body.len() - fits)
    } else if unseen > 0 {
        format!("\n… {unseen} more references not shown; narrow with path, or ask for more with limit")
    } else {
        String::new()
    };
    let text = if body.is_empty() { format!("{head}{notes}{footer}") } else { format!("{head}\n{}{more}{notes}{footer}", body[..fits].join("\n")) };
    let definitions: Vec<Value> = defs.iter().filter_map(|(p, l, s)| shown_name(p).map(|p| json!({"path": p, "line": l, "signature": s}))).take(10).collect();
    Ok(ProviderOutput {
        data: json!({
            "path": shown, "symbol": word, "total": total, "files_with_references": with_mentions, "references": listed, "definitions": definitions,
            "files_scanned": st.searched, "truncated": st.budget_hit || timed_out || total > shown_count, "left_out": st.counts(), "from": "the lines that have the name as a whole word",
        }),
        text,
        private: !r.trusted || r.sensitive,
        ..Default::default()
    })
}

// ---- code.map ----

fn repo_map(g: &PathGuard, index: &SymbolIndex, args: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
    let goal: String = arg_str(args, "goal").chars().take(500).collect();
    let budget = arg_u64(args, "budget_tokens", map::DEFAULT_BUDGET_TOKENS as u64).clamp(map::MIN_BUDGET_TOKENS as u64, map::MAX_BUDGET_TOKENS as u64) as usize;
    // A file the run names is judged by the guard like any path (one it refuses refuses the call, as `classify` does first), and is never opened here: it only says where the walk starts.
    let focus_files: Vec<Resolved> = focus_of(args).iter().map(|f| g.resolve(f, Op::Read)).collect::<Result<Vec<_>, _>>()?.into_iter().filter(|r| !r.sensitive).collect();
    let (root, private) = match arg_str(args, "path") {
        "" => match focus_files.first() {
            Some(f) => {
                let (root, trusted) = graph_root(g, f, "")?;
                (root, !f.trusted || !trusted)
            }
            None => {
                let r = g.resolve("@ws", Op::Read)?;
                (r.path.clone(), !r.trusted || r.sensitive)
            }
        },
        p => {
            let r = g.resolve(p, Op::Read)?;
            let shown = display(&r.path);
            let is_dir = std::fs::metadata(&r.path).map_err(|e| io_err(e, &shown))?.is_dir();
            if is_dir {
                (r.path.clone(), !r.trusted || r.sensitive)
            } else if r.sensitive {
                return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{shown} looks like it holds secrets: it names no project to map")));
            } else {
                let (root, trusted) = graph_root(g, &r, "")?;
                (root, !r.trusted || !trusted || r.sensitive)
            }
        }
    };
    let root_shown = graph::one_line(&display(&root), 300);
    let loaded = load_graph(g, index, &root, cancel)?;
    let gr = build_graph(&loaded);
    let focus: Vec<String> = focus_files.iter().filter_map(|f| f.path.strip_prefix(&root).ok().map(display)).collect();
    let too_big = loaded.st.budget_hit || gr.capped || gr.files.len() >= MAX_GRAPH_FILES;
    let m = if too_big {
        map::folder_map(&gr.files, budget, "more files than a map of files is made from")
    } else {
        let symbols: Vec<&[symbols::Symbol]> = loaded.files.iter().map(|(_, p)| p.symbols.as_slice()).collect();
        map::repo_map(&MapInput { graph: &gr, symbols: &symbols, goal: &goal, focus: &focus, budget_tokens: budget })
    };
    Ok(ProviderOutput {
        data: json!({
            "path": root_shown, "files": gr.files.len(), "edges": gr.edges, "tokens": m.tokens, "budget_tokens": budget, "listed": m.listed, "seeds": m.seeds,
            "candidates": m.candidates, "folders_only": m.folders_only, "names_left_out": m.unsafe_names,
            "truncated": loaded.st.budget_hit || loaded.timed_out || gr.capped, "left_out": loaded.st.counts(), "from": "the shape of the import lines and of the declarations",
        }),
        text: m.text,
        private,
        ..Default::default()
    })
}
