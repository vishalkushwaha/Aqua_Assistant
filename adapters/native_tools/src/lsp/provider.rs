//! `code.definition`, `code.references`, `code.hover` and `code.document_symbols`: what a language server knows about a place in a source file (card OA12-04; tasks CU-15 and WF-20 of the OpenCode
//! audit). **Off unless `AQUA_LSP=on`, and only for a language that `data/tools/lsp.json` has a server for.**
//!
//! * `code.definition {path, line, symbol? | column?}` says where the name at that place is defined, exactly (the compiler's own answer, not a guess from the shape of lines).
//! * `code.references {path, line, symbol? | column?, limit?}` lists the places that use it, in the project only.
//! * `code.hover {path, line, symbol? | column?}` gives its type and its documentation, a few lines.
//! * `code.document_symbols {path, max_symbols?}` lists the symbols of one file with their lines.
//!
//! `line` and `column` count from 1, a column in characters; `symbol` is the name as it is written on that line, and its column is found there (a small model counts columns badly).
//!
//! What the bridge does, and what it never does:
//!
//! * The first call of a run for a language **starts a program** (the one `lsp.json` names, whose SHA-256 is checked, in the project folder, in a Job Object, with the terminal tools' scrubbed
//!   environment). That call is classified as running a program (`Execute`): in standard mode the person is asked, and the question names the program and its hash. Later calls in the same run for
//!   the same folder are reads. A program is never started for a folder that is not marked trusted in `fs.json`, nor for a call outside a run.
//! * The file is sent to the server (`didOpen`) and closed again (`didClose`) after the answer: the server sees the text as it is on disk now. A file that looks like it holds secrets is never sent.
//! * Everything the server answers is **untrusted text**: the manifests say `untrusted_output`, so the host fences it as data; the lines are cleaned and cut ([`super::render`]); a place outside the
//!   project is counted and never shown, and no file outside the project is opened for its line; a `file:` address is read strictly ([`super::uri`]).
//! * A server that does not answer, ends, or writes what is not the protocol is ended, and its language is unavailable for the rest of the run: the answer says to use `code.symbols`, `code.refs` or
//!   `fs.grep`. A run may make a limited number of requests.
//!
//! Nothing waits longer than the times in `lsp.json` (10 s for a request and 20 s for a start by default), and a wait ends at once when the call is cancelled.

use super::bridge::Bridge;
use super::client::LspError;
use super::config::LspConfig;
use super::render::{self, find_word, render_hover, render_locations, render_symbols, utf16_of_char_col, Site};
use super::uri::file_uri;
use crate::code::lang::{language_of, Language};
use crate::code::profile::nearest_root;
use crate::fs::guard::{canonical, check_opened, display, within, Op, PathGuard, Resolved};
use crate::fs::page;
use aqua_tool_host::provider::ClassifyContext;
use aqua_tool_host::{Annotations, Availability, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

/// The other spellings of the path of a file a model writes: the file tools' own list.
const PATH_ALIASES: [&str; 4] = ["file_path", "filePath", "filepath", "file"];
/// The most distinct files of one answer whose lines are read to show them.
const MAX_FILES_READ: usize = 30;
/// The longest hover text, in characters.
const HOVER_CHARS: usize = 1200;

/// The four requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    Definition,
    References,
    Hover,
    Symbols,
}

impl Request {
    fn of(tool: &str) -> Option<Request> {
        Some(match tool {
            "code.definition" => Request::Definition,
            "code.references" => Request::References,
            "code.hover" => Request::Hover,
            "code.document_symbols" => Request::Symbols,
            _ => return None,
        })
    }

    fn method(self) -> &'static str {
        match self {
            Request::Definition => "textDocument/definition",
            Request::References => "textDocument/references",
            Request::Hover => "textDocument/hover",
            Request::Symbols => "textDocument/documentSymbol",
        }
    }

    fn words(self) -> &'static str {
        match self {
            Request::Definition => "find the definition",
            Request::References => "find the references",
            Request::Hover => "read the hover text",
            Request::Symbols => "list the symbols",
        }
    }

    fn needs_position(self) -> bool {
        self != Request::Symbols
    }
}

/// The language of a file in the words of `lsp.json` (a key of [`super::config::LANGUAGES`]), and the id the protocol knows it by.
fn language_key(l: Language) -> Option<&'static str> {
    Some(match l {
        Language::Rust => "rust",
        Language::Python => "python",
        Language::JavaScript => "javascript",
        Language::TypeScript => "typescript",
        Language::Go => "go",
        Language::CSharp => "csharp",
        Language::Java => "java",
        Language::Kotlin => "kotlin",
        Language::C => "c",
        Language::Cpp => "cpp",
        Language::Ruby => "ruby",
        Language::Php => "php",
        Language::Shell | Language::PowerShell => return None,
    })
}

fn language_id(key: &str, file_name: &str) -> String {
    let ext = file_name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match (key, ext.as_str()) {
        ("typescript", "tsx") => "typescriptreact".into(),
        ("javascript", "jsx") => "javascriptreact".into(),
        _ => key.to_string(),
    }
}

struct Inner {
    guard: Arc<PathGuard>,
    bridge: Bridge,
    /// Are the tools on offer? Asked each time the tools are listed. Not set: they are (a test, or a caller that made the provider to use it).
    switch: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

/// The four tools and the servers they use.
pub struct LspProvider {
    inner: Arc<Inner>,
}

impl LspProvider {
    /// The tools for the servers of `config`, reading through `guard`.
    pub fn new(guard: Arc<PathGuard>, config: LspConfig) -> Self {
        let for_programs = guard.clone();
        let bridge = Bridge::new(config, move |p: &Path| for_programs.resolve(&p.to_string_lossy(), Op::Write).is_ok());
        Self { inner: Arc::new(Inner { guard, bridge, switch: None }) }
    }

    /// The tools are on offer only while `on` says so (`AQUA_LSP`: new behaviour is off until it is measured, and this one starts a program).
    pub fn with_switch(mut self, on: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.switch = Some(on),
            // Something still holds the inner part (a call that began before this one): it keeps what it has, and this provider gets a new one over the same guard and the same bridge, so no run or server is lost.
            None => self.inner = Arc::new(Inner { guard: self.inner.guard.clone(), bridge: self.inner.bridge.clone(), switch: Some(on) }),
        }
        self
    }

    /// The bridge the tools use (a test asks it what is running).
    pub fn bridge(&self) -> Bridge {
        self.inner.bridge.clone()
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(|v| v.as_u64())
}

fn manifest(name: &str, description: &str, schema: Value) -> ToolManifest {
    ToolManifest {
        name: name.into(),
        title: name.into(),
        description: description.into(),
        // A call that has to start the server is classified as running a program by `classify_for`; the manifest holds what every call is.
        classes: vec![PermClass::Read],
        default_decision: Decision::Auto,
        input_schema: schema,
        // The first call of a run starts the program: the start-up time and the request time are both inside this.
        timeout_default_ms: 60_000,
        timeout_max_ms: 600_000,
        // The server is a program of the person's choosing that may reach out (rust-analyzer runs cargo): it changes no file, and it is not "closed world".
        annotations: Annotations { read_only: true, destructive: false, idempotent: true, open_world: true },
        // Names, lines and hover texts are text from the project's files and from a program: data, never instructions.
        untrusted_output: true,
    }
}

fn position_schema(extra: Value) -> Value {
    let mut props = json!({
        "path": {"type": "string", "minLength": 1, "maxLength": 1024, "x-aliases": PATH_ALIASES},
        "line": {"type": "integer", "minimum": 1, "maximum": 10_000_000, "description": "the line of the name, from 1"},
        "symbol": {"type": "string", "maxLength": 200, "x-aliases": ["name"], "description": "the name as it is written on that line; its column is found there"},
        "column": {"type": "integer", "minimum": 1, "maximum": 100_000, "description": "the column of the name in characters, from 1, when symbol is not given"}
    });
    if let (Some(p), Some(e)) = (props.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            p.insert(k.clone(), v.clone());
        }
    }
    json!({"type": "object", "properties": props, "required": ["path", "line"]})
}

/// What a call is, worked out from its arguments without asking the server anything.
struct Target {
    op: Request,
    resolved: Resolved,
    /// The path as the call named it, resolved, written with `/`.
    shown: String,
    /// The path from the project folder, as the answers show it.
    rel: String,
    file_name: String,
    language: &'static str,
    spec: super::config::ServerSpec,
    /// The project folder: the nearest folder above the file that has a marker (`Cargo.toml`, `package.json` ...), inside the folder that is allowed.
    root: PathBuf,
}

impl Inner {
    fn target(&self, tool: &str, args: &Value) -> Result<Target, ToolError> {
        let op = Request::of(tool).ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("unknown tool '{tool}'")))?;
        let invalid = |m: String| ToolError::new(ErrorCode::InvalidArguments, m);
        let resolved = self.guard.resolve(arg_str(args, "path"), Op::Read)?;
        let shown = display(&resolved.path);
        if resolved.sensitive {
            return Err(invalid(format!("{shown} looks like it holds secrets: it is never sent to a language server")));
        }
        let file_name = resolved.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let Some(language) = language_of(&file_name).and_then(language_key) else {
            return Err(invalid(format!("{file_name} is not a source file of a language that has a language server (use fs.read)")));
        };
        let Some(spec) = self.bridge.config().server_for(language).cloned() else {
            return Err(invalid(format!("no language server is configured for {language} (data/tools/lsp.json): use code.symbols, code.refs or fs.grep")));
        };
        if !resolved.trusted {
            return Err(invalid(format!(
                "{shown} is not in a folder marked trusted in data/tools/fs.json: a language server runs the project's own code (build scripts, plugins), so none is started there; use code.symbols, code.refs or fs.grep"
            )));
        }
        if op.needs_position() {
            if arg_u64(args, "line").is_none_or(|l| l == 0) {
                return Err(invalid("give the line of the name, counted from 1".into()));
            }
            if arg_str(args, "symbol").trim().is_empty() && arg_u64(args, "column").is_none_or(|c| c == 0) {
                return Err(invalid("give the name as it is written on that line (symbol), or its column".into()));
            }
        }
        let allowed = self.guard.root_path_of(&resolved.path).ok_or_else(|| ToolError::new(ErrorCode::PermissionDenied, format!("{shown} is outside the folders Aqua may use")))?;
        let root = nearest_root(&resolved.path, &allowed).and_then(|r| canonical(&r).ok()).filter(|r| within(r, &allowed)).unwrap_or(allowed);
        let rel = resolved.path.strip_prefix(&root).map(display).unwrap_or_else(|_| shown.clone());
        Ok(Target { op, resolved, shown, rel, file_name, language, spec, root })
    }
}

/// How a failure of the bridge is told to the model: a typed error whose words are Aqua's own.
fn tool_error(e: &LspError) -> ToolError {
    let (code, kind) = match e {
        LspError::Refused(_) => (ErrorCode::BackendError, "refused"),
        LspError::Start(_) => (ErrorCode::BackendError, "start_failed"),
        LspError::NoAnswer(..) => (ErrorCode::BackendError, "no_answer"),
        LspError::Gone(_) => (ErrorCode::BackendError, "gone"),
        LspError::TooLarge { .. } => (ErrorCode::ResourceLimit, "too_large"),
        LspError::Server(_) => (ErrorCode::BackendError, "server_error"),
        LspError::Cancelled => (ErrorCode::Cancelled, "cancelled"),
    };
    let mut message = e.sentence();
    if e.ends_the_server() {
        message.push_str(". For the rest of this run use code.symbols, code.refs or fs.grep.");
    }
    let mut err = ToolError::new(code, message).with_details(json!({"lsp": kind}));
    err.retryable = false;
    err
}

impl ToolProvider for LspProvider {
    fn id(&self) -> &str {
        "lsp"
    }

    fn schema_extensions(&self) -> bool {
        true
    }

    fn availability(&self, _tool: &str) -> Availability {
        if let Some(on) = &self.inner.switch {
            if !on() {
                return Availability::Unavailable("the language-server tools are off (AQUA_LSP=on turns them on)".into());
            }
        }
        if self.inner.bridge.config().is_empty() {
            return Availability::Unavailable("data/tools/lsp.json names no language server".into());
        }
        Availability::Available
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        vec![
            manifest(
                "code.definition",
                "Where the name at path:line is defined, exactly, from the language server (needs data/tools/lsp.json). Give path, line and the name (symbol) or column.",
                position_schema(json!({})),
            ),
            manifest(
                "code.references",
                "Where the name at path:line is used, from the language server: file:line:col and the line, in the project only. Give path, line and symbol or column.",
                position_schema(json!({"limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 20, "x-clamp": true}})),
            ),
            manifest(
                "code.hover",
                "The type and documentation of the name at path:line, from the language server, as a few lines. Give path, line and the name (symbol) or column.",
                position_schema(json!({})),
            ),
            manifest(
                "code.document_symbols",
                "The symbols of one source file with their lines, from the language server (L12-40 fn name), nested ones indented. Give path.",
                json!({"type": "object", "properties": {
                    "path": {"type": "string", "minLength": 1, "maxLength": 1024, "x-aliases": PATH_ALIASES},
                    "max_symbols": {"type": "integer", "minimum": 1, "maximum": 200, "default": 40, "x-clamp": true}
                }, "required": ["path"]}),
            ),
        ]
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        // Without a run to say what it has started, a call is held to be the one that starts the program (the strictest answer).
        self.classify_in(tool, args, None)
    }

    fn classify_for(&self, tool: &str, args: &Value, ctx: &ClassifyContext) -> Result<Classification, ToolError> {
        if ctx.run.is_empty() {
            return Err(ToolError::new(ErrorCode::InvalidArguments, "the language-server tools work inside an agent run: use code.symbols, code.refs or fs.grep here"));
        }
        self.classify_in(tool, args, Some(&ctx.run))
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let inner = self.inner.clone();
        let (tool, run, cancel) = (tool.to_string(), ctx.agent_run_id.clone(), ctx.cancel.clone());
        Box::pin(async move {
            tokio::task::spawn_blocking(move || run_call(&inner, &tool, &args, &run, &cancel))
                .await
                .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("language-server task failed: {e}")))?
        })
    }

    fn run_ended(&self, run_id: &str) {
        self.inner.bridge.end_run(run_id);
    }
}

impl LspProvider {
    fn classify_in(&self, tool: &str, args: &Value, run: Option<&str>) -> Result<Classification, ToolError> {
        let t = self.inner.target(tool, args)?;
        let launch = match run {
            // A run that has seen its server for this language stop is told so by the call itself, and starts nothing.
            Some(run) => !self.inner.bridge.has_launched(run, t.language, &t.root) && self.inner.bridge.broken_reason(run, t.language).is_none(),
            None => true,
        };
        if !launch {
            return Ok(Classification { classes: vec![PermClass::Read], summary: format!("{} {} with the language server", t.op.words(), t.shown), target: t.shown, target_allowed: true });
        }
        let exe = display(&t.spec.command);
        let hash = t.spec.sha256.clone().unwrap_or_else(|| "NOT SET: it will not start".into());
        Ok(Classification {
            classes: vec![PermClass::Read, PermClass::Execute],
            summary: format!("start the {} language server {exe} (sha256 {hash}, from data/tools/lsp.json) in {}, to {} in {}", t.language, display(&t.root), t.op.words(), t.shown),
            target: exe,
            target_allowed: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{FsConfig, PathGuard};

    fn provider(name: &str) -> LspProvider {
        // The folder as the guard reads it: without the `\\?\` that a canonical path has on Windows.
        let dir = PathBuf::from(crate::lsp::scratch(name).to_string_lossy().trim_start_matches(r"\\?\"));
        let guard = Arc::new(PathGuard::new(FsConfig::with_workspace(dir.to_string_lossy().to_string()), vec![dir.join("aqua")]).unwrap());
        LspProvider::new(guard, LspConfig::default())
    }

    fn words(p: &LspProvider) -> String {
        match p.availability("code.hover") {
            Availability::Unavailable(why) => why,
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn the_switch_is_applied_to_a_provider_that_nobody_else_holds() {
        let off = provider("switch_alone_off").with_switch(Arc::new(|| false));
        assert!(words(&off).contains("are off"), "{}", words(&off));
        // With the switch on, what is left to say is the next reason: no server is named.
        let on = provider("switch_alone_on").with_switch(Arc::new(|| true));
        assert!(words(&on).contains("names no language server"), "{}", words(&on));
    }

    #[test]
    fn the_switch_is_applied_without_a_panic_when_the_inner_part_is_shared_and_the_old_holder_is_left_as_it_was() {
        let p = provider("switch_shared");
        // A call that has begun holds the inner part, so `Arc::get_mut` has nothing to give.
        let held = p.inner.clone();
        assert!(held.switch.is_none());
        let p = p.with_switch(Arc::new(|| false));
        assert!(words(&p).contains("are off"), "{}", words(&p));
        assert!(held.switch.is_none(), "what the earlier holder has is not changed");
        // The new inner part is over the same guard and the same bridge (the config is inside the bridge's shared state), so nothing that was running is lost.
        assert!(Arc::ptr_eq(&p.inner.guard, &held.guard));
        assert!(std::ptr::eq(p.inner.bridge.config(), held.bridge.config()));
        // A second switch on the new inner part, held by nobody else, is set in place.
        let again = p.with_switch(Arc::new(|| true));
        assert!(words(&again).contains("names no language server"), "{}", words(&again));
        assert!(Arc::ptr_eq(&again.inner.guard, &held.guard));
    }
}

// ---- the call ---------------------------------------------------------------------------------------------------------------------

/// The text of the file, as it is on disk now, read as `fs.read` reads: through the guard's handle check, at most the read limit, and a text file.
fn read_text(guard: &PathGuard, r: &Resolved, shown: &str) -> Result<String, ToolError> {
    let io = |e: std::io::Error| ToolError::new(if e.kind() == std::io::ErrorKind::NotFound { ErrorCode::NotFound } else { ErrorCode::BackendError }, format!("{shown}: {e}"));
    let limit = guard.limits().read_max_bytes;
    let meta = std::fs::metadata(&r.path).map_err(io)?;
    if meta.is_dir() {
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{shown} is a folder")));
    }
    if meta.len() > limit {
        return Err(ToolError::new(ErrorCode::ResourceLimit, format!("{shown} is {} bytes: more than the {limit} that are read; search it with fs.grep", meta.len())));
    }
    let mut f = std::fs::File::open(&r.path).map_err(io)?;
    check_opened(&f, r)?;
    let mut bytes = Vec::new();
    (&mut f).take(limit + 1).read_to_end(&mut bytes).map_err(io)?;
    match page::sniff(&bytes) {
        page::Sniff::Text => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        _ => Err(ToolError::new(ErrorCode::InvalidArguments, format!("{shown} is not a UTF-8 text file"))),
    }
}

/// The project, for the renderer: what is shown and what is read goes through the guard, and nothing outside the project folder is either.
struct ProjectSite<'a> {
    guard: &'a PathGuard,
    root: &'a Path,
    files: RefCell<HashMap<PathBuf, Option<Vec<String>>>>,
}

impl ProjectSite<'_> {
    fn readable(&self, path: &Path) -> Option<Resolved> {
        let r = self.guard.resolve(&path.to_string_lossy(), Op::Read).ok()?;
        within(&r.path, self.root).then_some(r)
    }
}

impl Site for ProjectSite<'_> {
    fn shown(&self, path: &Path) -> Option<String> {
        let r = self.readable(path)?;
        let rel = r.path.strip_prefix(self.root).map(|p| p.to_path_buf()).unwrap_or_else(|_| r.path.components().skip(self.root.components().count()).collect());
        Some(display(&rel))
    }

    fn line(&self, path: &Path, line0: u32) -> Option<String> {
        let r = self.readable(path)?;
        if r.sensitive {
            return None;
        }
        let mut files = self.files.borrow_mut();
        if !files.contains_key(&r.path) {
            let lines = if files.len() >= MAX_FILES_READ { None } else { read_text(self.guard, &r, "a file named by the language server").ok().map(|t| t.lines().map(str::to_string).collect()) };
            files.insert(r.path.clone(), lines);
        }
        files.get(&r.path)?.as_ref()?.get(line0 as usize).cloned()
    }
}

fn run_call(inner: &Inner, tool: &str, args: &Value, run: &str, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
    if run.is_empty() {
        return Err(ToolError::new(ErrorCode::InvalidArguments, "the language-server tools work inside an agent run"));
    }
    let t = inner.target(tool, args)?;
    let text = read_text(&inner.guard, &t.resolved, &t.shown)?;
    // The place: a line and a column in characters, then in the UTF-16 units the protocol counts in.
    let (line0, utf16) = if t.op.needs_position() {
        let line1 = arg_u64(args, "line").unwrap_or(1) as usize;
        let Some(line_text) = text.lines().nth(line1 - 1) else {
            return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{} has {} lines: there is no line {line1}", t.shown, text.lines().count())));
        };
        let symbol = arg_str(args, "symbol").trim();
        let col = if !symbol.is_empty() {
            match find_word(line_text, symbol) {
                Some(c) => c,
                None => {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArguments,
                        format!("line {line1} of {} has no name '{}': it reads `{}`", t.shown, render::clean_line(symbol, 60), render::clean_line(line_text, 120)),
                    ))
                }
            }
        } else {
            arg_u64(args, "column").unwrap_or(1) as usize
        };
        ((line1 - 1) as u64, utf16_of_char_col(line_text, col) as u64)
    } else {
        (0, 0)
    };
    let lease = inner.bridge.lease(run, t.language, &t.root, cancel).map_err(|e| tool_error(&e))?;
    inner.bridge.spend(run, &t.spec).map_err(|e| tool_error(&e))?;
    let uri = file_uri(&t.resolved.path);
    let server = lease.server();
    let doc = json!({"uri": uri});
    let params = match t.op {
        Request::Symbols => json!({"textDocument": doc}),
        Request::References => json!({"textDocument": doc, "position": {"line": line0, "character": utf16}, "context": {"includeDeclaration": false}}),
        _ => json!({"textDocument": doc, "position": {"line": line0, "character": utf16}}),
    };
    let asked = server
        .notify("textDocument/didOpen", json!({"textDocument": {"uri": uri, "languageId": language_id(t.language, &t.file_name), "version": 1, "text": text}}))
        .and_then(|_| server.request(t.op.method(), params, Duration::from_secs(t.spec.request_timeout_secs), cancel));
    // The file is closed again whatever the answer was: the server keeps nothing open between calls.
    let _ = server.notify("textDocument/didClose", json!({"textDocument": doc}));
    let answer = match asked {
        Ok(v) => v,
        Err(e) => {
            inner.bridge.fail(run, t.language, &e);
            return Err(tool_error(&e));
        }
    };
    let view = inner.guard.limits().view_chars;
    let site = ProjectSite { guard: &inner.guard, root: &t.root, files: RefCell::new(HashMap::new()) };
    let at = format!("{}:{}", t.rel, line0 + 1);
    let (text_out, listed, total, outside, items) = match t.op {
        Request::Definition => {
            let r = render_locations("definition", render::locations_of(&answer), &site, 10, view, false);
            (r.text, r.listed, r.total, r.outside, r.items)
        }
        Request::References => {
            let limit = arg_u64(args, "limit").unwrap_or(20).clamp(1, 50) as usize;
            let r = render_locations("reference", render::locations_of(&answer), &site, limit, view, true);
            (r.text, r.listed, r.total, r.outside, r.items)
        }
        Request::Hover => match render_hover(&answer, HOVER_CHARS) {
            Some(h) => (format!("{at} from the language server:\n{h}"), 1, 1, 0, vec![]),
            None => (format!("The language server has nothing to say about {at}."), 0, 0, 0, vec![]),
        },
        Request::Symbols => {
            let max = arg_u64(args, "max_symbols").unwrap_or(40).clamp(1, 200) as usize;
            let r = render_symbols(&answer, &t.rel, max, view);
            (r.text, r.listed, r.total, 0, r.items)
        }
    };
    let program = t.spec.command.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    Ok(ProviderOutput {
        data: json!({"tool": tool, "language": t.language, "server": program, "launched": lease.launched_now, "path": t.rel, "listed": listed, "total": total, "outside_project": outside, "results": items, "from": "the language server"}),
        text: text_out,
        warnings: if lease.launched_now { vec![format!("started the {} language server {} (sha256 {})", t.language, display(&t.spec.command), t.spec.sha256.clone().unwrap_or_default())] } else { vec![] },
        ..Default::default()
    })
}
