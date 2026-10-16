//! The `fs.*` tools: the [`FsProvider`] that the tool host registers.

use super::diff;
use super::guard::{check_opened, check_opened_at, display, final_path_of, same_place, within, Access, Op, PathGuard, Resolved};
use super::locks;
use super::masked::MaskedView;
use super::page::{self, looks_binary};
use super::trash::{copy_recursive, dir_size, with_run, Entry, Trash};
use super::walk::{self, Entry as WalkEntry, Glob, Kind, Order, Visit, WalkOpts, WalkStats};
use aqua_tool_host::{
    Annotations, ChangePreview, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError,
    ToolManifest, ToolProvider,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub struct FsProvider {
    guard: Arc<PathGuard>,
    trash: Arc<Trash>,
    /// Is `fs.outline` on offer? Asked each time the tools are listed. Not set: it is not (the tool is new, and new behaviour is off until it is measured: card OA5-03, CX-17).
    outline: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// What puts back the effect of a command or a checkpoint (card OA7-03). Not set: a record of either kind is listed as one the file tools cannot put back.
    effects: Option<Arc<dyn EffectUndo>>,
    /// What the syntax gate is set to, asked at every call (card OA6-03, `AQUA_SYNTAX_GATE`). Not set: off, the change is made as it always was.
    syntax: Option<Arc<dyn Fn() -> super::ast::Gate + Send + Sync>>,
    /// Is `fs.edit_lines` on offer? Asked each time the tools are listed and at each call. Not set: it is not (the tool is a prototype, and new behaviour is off until it is measured: card OA6-02, ED-07).
    edit_lines: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

/// What an undo of one record of a command or a checkpoint did (card OA7-03).
#[derive(Debug, Clone, Default)]
pub struct EffectReport {
    /// What was put back (or would be, in a dry run), one line each.
    pub restored: Vec<String>,
    /// Paths left as they are, and why: changed since, not allowed, could not be written.
    pub left: Vec<(String, String)>,
    /// Everything the record held is dealt with (put back, or already as it was): the record is of no further use and is forgotten. False when something was left.
    pub settled: bool,
}

/// Puts back the effect of a command or a checkpoint of a run: implemented by the git module (`git::journal::Journal` for the commands, `git::checkpoint::Checkpoints` for the
/// checkpoints), so that the file tools name nothing of git. The undo of a run (`fs.undo_run`) asks it for each record of those kinds, newest first, checkpoints last.
pub trait EffectUndo: Send + Sync {
    /// Puts back what `entry` (reason `command` or `checkpoint`) recorded, or with `dry_run` says what it would put back. `handled` holds the files (lower-cased, with `/`) that the
    /// undo has dealt with already; a checkpoint leaves those out, and every record adds what it dealt with.
    fn undo(&self, entry: &Entry, guard: &PathGuard, dry_run: bool, handled: &mut std::collections::HashSet<String>, cancel: &AtomicBool) -> Result<EffectReport, ToolError>;
}

/// The key under which a file is remembered as dealt with by an undo: its path with `/` and in lower case.
pub fn handled_key(path: &Path) -> String {
    display(path).to_ascii_lowercase()
}

impl FsProvider {
    pub fn new(guard: Arc<PathGuard>, trash: Arc<Trash>) -> Self {
        Self { guard, trash, outline: None, effects: None, syntax: None, edit_lines: None }
    }

    /// Offers `fs.edit_lines` (replace a range of lines, held to an anchor line) whenever `on` says so (card OA6-02, ED-07). Without it the tool is not offered, is not listed by the host and
    /// is refused if it is called.
    pub fn with_edit_lines(mut self, on: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        self.edit_lines = Some(on);
        self
    }

    /// Looks at what `fs.write` (an overwrite), `fs.edit` and `fs.patch` leave behind, as `gate()` says each time (card OA6-03): off, a note after a change that unbalances a file's brackets or
    /// quotes (`warn`), or also the refusal of a change that breaks a JSON file that was valid (`on`). See [`super::ast`].
    pub fn with_syntax_gate(mut self, gate: Arc<dyn Fn() -> super::ast::Gate + Send + Sync>) -> Self {
        self.syntax = Some(gate);
        self
    }

    /// Offers `fs.outline` whenever `on` says so.
    pub fn with_outline(mut self, on: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        self.outline = Some(on);
        self
    }

    /// Lets `fs.undo_run` put back what commands and checkpoints recorded.
    pub fn with_effects(mut self, effects: Arc<dyn EffectUndo>) -> Self {
        self.effects = Some(effects);
        self
    }
}

/// The ways a path may be written for a file tool by a model that spells it another way: `file_path`, `filePath`, `filepath`, `file`.
const PATH_ALIASES: [&str; 4] = ["file_path", "filePath", "filepath", "file"];

fn io_err(e: std::io::Error, what: &str) -> ToolError {
    use std::io::ErrorKind::*;
    let code = match e.kind() {
        NotFound => ErrorCode::NotFound,
        PermissionDenied => ErrorCode::OsPermissionDenied,
        AlreadyExists => ErrorCode::AlreadyExists,
        _ => ErrorCode::BackendError,
    };
    ToolError::new(code, format!("{what}: {e}"))
}

/// An error from putting a file in place, in words the model can act on: a file that changed since it was read, a read-only file.
fn write_err(e: std::io::Error, path: &Path, what: &str) -> ToolError {
    if is_changed_underneath(&e) {
        return ToolError::new(ErrorCode::Conflict, format!("ERROR [FILE_CHANGED] {} changed while the change was being made; read it again and repeat the change", display(path)));
    }
    if e.kind() == std::io::ErrorKind::PermissionDenied && std::fs::metadata(path).is_ok_and(|m| m.permissions().readonly()) {
        return ToolError::new(ErrorCode::OsPermissionDenied, format!("{} is read-only (fs.set_attrs readonly=false first)", display(path)));
    }
    io_err(e, what)
}

/// A file with the read-only attribute is not changed: the person (or `fs.set_attrs`) has to say it may be.
fn refuse_read_only(path: &Path) -> Result<(), ToolError> {
    if std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().readonly()) {
        return Err(ToolError::new(ErrorCode::OsPermissionDenied, format!("{} is read-only (fs.set_attrs readonly=false first)", display(path))));
    }
    Ok(())
}

/// How many lines a file has: its line breaks, and one more when the last line has none.
fn count_lines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|b| **b == b'\n').count() + usize::from(bytes.last().is_some_and(|b| *b != b'\n'))
}

fn arg_str<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn arg_u64(args: &Value, key: &str, default: u64) -> u64 {
    args.get(key).and_then(|v| v.as_u64()).unwrap_or(default)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_file(p: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn modified_ms(m: &std::fs::Metadata) -> u64 {
    m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn kind_of(m: &std::fs::Metadata) -> &'static str {
    if m.file_type().is_symlink() {
        "link"
    } else if m.is_dir() {
        "dir"
    } else {
        "file"
    }
}

/// What a write expects of the file it is about to put in place (card OA6-01, ED-06).
#[derive(Clone, Copy)]
enum Expect<'a> {
    /// Whatever is there is replaced.
    Any,
    /// The file must still have the content with this SHA-256 (what the call read and worked from); if it does not, nothing is written.
    Unchanged(&'a str),
    /// No file may be there: the name is taken only if it is free at the moment of the rename, so two creations racing for one name cannot both win.
    Absent,
}

/// The mark of a write that stopped because the file was changed by somebody else after it was read.
#[derive(Debug)]
struct ChangedUnderneath;

impl std::fmt::Display for ChangedUnderneath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the file changed after it was read")
    }
}

impl std::error::Error for ChangedUnderneath {}

fn is_changed_underneath(e: &std::io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<ChangedUnderneath>())
}

/// Renames `from` to `to` only when `to` does not exist (the operating system decides, at the moment of the rename): the file is on disk when this returns.
#[cfg(windows)]
fn rename_without_replacing(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
    let wide = |p: &Path| p.as_os_str().encode_wide().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (a, b) = (wide(from), wide(to));
    // SAFETY: both buffers are NUL-terminated UTF-16 that live for the call; without MOVEFILE_REPLACE_EXISTING the call fails when the target is there.
    let ok = unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn rename_without_replacing(from: &Path, to: &Path) -> std::io::Result<()> {
    // A hard link is made only when the name is free; the temporary name is then taken off.
    std::fs::hard_link(from, to)?;
    let _ = std::fs::remove_file(from);
    Ok(())
}

/// The folders whose leftover temporary files have been swept in this process (a bounded list: past the bound it starts again).
fn sweep_leftovers_once(dir: &Path) {
    static SWEPT: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> = std::sync::OnceLock::new();
    let mut swept = SWEPT.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if swept.len() >= 2048 {
        swept.clear();
    }
    if swept.insert(dir.to_path_buf()) {
        drop(swept);
        // A write that was cut short by a crash leaves its temporary file (`name.aqua-tmp-<pid>-<n>`); one an hour old is nobody's work in progress.
        aqua_tool_host::fsutil::sweep_tmp(dir, std::time::Duration::from_secs(3600));
    }
}

/// Write `bytes` to `dest` through a temporary file in the same folder, so a crash never leaves a half-written file.
///
/// The temporary file is created new (`create_new`: a link planted at its name is not followed, the call fails instead), and once it is open it must be in the folder
/// that was checked: a folder swapped for a junction since the check would otherwise take the file, and the rename, somewhere else.
fn write_atomic(dest: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_expecting(dest, bytes, Expect::Any)
}

/// [`write_atomic`] for a file that is expected to be as the caller found it (`Expect::Unchanged`), or not to exist (`Expect::Absent`). The check is made after the new content is
/// on disk and just before it takes the name, so what is left open is the time of one hash and one rename.
fn write_atomic_expecting(dest: &Path, bytes: &[u8], expect: Expect<'_>) -> std::io::Result<()> {
    if let Some(dir) = dest.parent() {
        sweep_leftovers_once(dir);
    }
    for attempt in 0..16 {
        let tmp = dest.with_extension(format!("aqua-tmp-{}-{attempt}", std::process::id()));
        let mut f = match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let result = (|| {
            match (final_path_of(&f), dest.parent()) {
                (Some(Err(e)), _) => return Err(e),
                (Some(Ok(actual)), Some(want)) if !actual.parent().is_some_and(|p| same_place(p, want)) => {
                    return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, format!("the folder {} is not where it was checked to be (a link was put in its place): nothing was written", display(want))));
                }
                _ => {}
            }
            f.write_all(bytes)?;
            f.sync_all()?;
            drop(f);
            // A test makes this write fail here, with the new content on disk and nothing renamed: the temporary file has to go and the patch has to undo what it did (`properties.rs`).
            #[cfg(test)]
            if let Some(e) = properties::injected_failure() {
                return Err(e);
            }
            match expect {
                // The rename waits until it is on disk (`fsutil`, the shared durable-write library).
                Expect::Any => aqua_tool_host::fsutil::replace_durable(&tmp, dest),
                Expect::Unchanged(want) => {
                    match sha256_file(dest) {
                        Ok(now) if now == want => {}
                        // Changed, or taken away, since the caller read it: the new content was made from text that is gone.
                        Ok(_) => return Err(std::io::Error::other(ChangedUnderneath)),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(std::io::Error::other(ChangedUnderneath)),
                        Err(e) => return Err(e),
                    }
                    aqua_tool_host::fsutil::replace_durable(&tmp, dest)
                }
                Expect::Absent => rename_without_replacing(&tmp, dest),
            }
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        return result;
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "no free name for the temporary file"))
}

/// Open the file the guard checked, for reading, and check again on what was opened (see `check_opened`).
fn open_checked(r: &Resolved) -> Result<std::fs::File, ToolError> {
    let f = std::fs::File::open(&r.path).map_err(|e| io_err(e, &display(&r.path)))?;
    check_opened(&f, r)?;
    Ok(f)
}

/// The whole content of the file the guard checked, read from the handle that was checked.
fn read_checked(r: &Resolved) -> Result<Vec<u8>, ToolError> {
    let mut f = open_checked(r)?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).map_err(|e| io_err(e, "read"))?;
    Ok(bytes)
}

/// Open for appending, creating the file when it is not there. A file that was just created in the wrong place is taken away again (it is ours: `create_new`).
fn open_append_checked(r: &Resolved) -> Result<std::fs::File, ToolError> {
    let (f, created) = match std::fs::OpenOptions::new().append(true).create_new(true).open(&r.path) {
        Ok(f) => (f, true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (std::fs::OpenOptions::new().append(true).open(&r.path).map_err(|e| io_err(e, "open"))?, false),
        Err(e) => return Err(io_err(e, "open")),
    };
    if let Err(refused) = check_opened(&f, r) {
        if created {
            if let Some(Ok(actual)) = final_path_of(&f) {
                drop(f);
                let _ = std::fs::remove_file(actual);
            }
        }
        return Err(refused);
    }
    Ok(f)
}

fn spec(name: &str, description: &str, classes: Vec<PermClass>, schema: Value, untrusted: bool, read_only: bool) -> ToolManifest {
    ToolManifest {
        name: name.into(),
        title: name.into(),
        description: description.into(),
        classes,
        default_decision: Decision::Auto,
        input_schema: schema,
        timeout_default_ms: 15_000,
        timeout_max_ms: 60_000,
        annotations: Annotations { read_only, destructive: false, idempotent: read_only, open_world: false },
        untrusted_output: untrusted,
    }
}

impl ToolProvider for FsProvider {
    fn id(&self) -> &str {
        "fs"
    }

    fn schema_extensions(&self) -> bool {
        true
    }

    fn availability(&self, tool: &str) -> aqua_tool_host::Availability {
        if tool == "fs.outline" && !self.outline.as_ref().is_some_and(|on| on()) {
            return aqua_tool_host::Availability::Unavailable("fs.outline is off (AQUA_FS_OUTLINE=on turns it on)".into());
        }
        if tool == "fs.edit_lines" && !self.edit_lines.as_ref().is_some_and(|on| on()) {
            return aqua_tool_host::Availability::Unavailable("fs.edit_lines is off (AQUA_EDIT_LINES=on turns it on)".into());
        }
        aqua_tool_host::Availability::Available
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        let path = json!({"type": "string", "minLength": 1, "maxLength": 1024});
        // The path of a tool that reads or edits one file, with the other spellings a model writes (`x-aliases`, read as `path` by the host before anything is checked).
        let file_path = json!({"type": "string", "minLength": 1, "maxLength": 1024, "x-aliases": PATH_ALIASES});
        let read = vec![PermClass::Read];
        let write = vec![PermClass::Write];
        vec![
            spec("fs.list", "List a folder (path=@ lists the allowed roots). depth 1-3. Leaves out dotfiles and .gitignore'd names (include_hidden, include_ignored); says how many.", read.clone(),
                json!({"type": "object", "properties": {"path": {"type": "string", "default": "@", "x-aliases": PATH_ALIASES}, "depth": {"type": "integer", "minimum": 1, "maximum": 3, "default": 1, "x-clamp": true}, "pattern": {"type": "string", "maxLength": 200}, "include_hidden": {"type": "boolean", "default": false}, "include_ignored": {"type": "boolean", "default": false}, "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 200, "x-clamp": true}}}), true, true),
            spec("fs.find", "Find files by name glob (*.rs, src/**/*.ts, *.{ts,tsx}) and/or text inside. Leaves out dotfiles, .gitignore'd and secret files; says how many.", read.clone(),
                json!({"type": "object", "properties": {"path": {"type": "string", "default": "@ws", "x-aliases": PATH_ALIASES}, "name": {"type": "string", "maxLength": 200}, "text": {"type": "string", "maxLength": 500}, "case_sensitive": {"type": "boolean", "default": false}, "max_depth": {"type": "integer", "minimum": 1, "maximum": 12, "default": 8, "x-clamp": true}, "include_hidden": {"type": "boolean", "default": false}, "include_ignored": {"type": "boolean", "default": false}, "sort": {"type": "string", "enum": ["name", "mtime"], "default": "name"}, "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 50, "x-clamp": true}}}), true, true),
            spec("fs.read", "Read a text file in pages. Line 1: lines shown, of how many, how to go on, version. Then numbered lines. .env/key values hidden unless reveal=true.", read.clone(),
                json!({"type": "object", "properties": {
                    "path": file_path.clone(),
                    "offset_line": {"type": "integer", "minimum": 1, "default": 1, "x-clamp": true, "x-aliases": ["offset", "start_line"]},
                    "max_lines": {"type": "integer", "minimum": 1, "maximum": 2000, "default": 500, "x-clamp": true, "x-aliases": ["limit", "lines"]},
                    "max_chars": {"type": "integer", "minimum": page::MIN_PAGE_CHARS, "maximum": page::MAX_PAGE_CHARS, "x-clamp": true},
                    "max_bytes": {"type": "integer", "minimum": 1, "maximum": 1048576, "default": 65536, "x-clamp": true},
                    "reveal": {"type": "boolean", "default": false}}, "required": ["path"]}), true, true),
            spec("fs.outline", "Outline of a source file: its declarations (fn, struct, class...) with the lines each spans. Then fs.read offset_line for the part you need.", read.clone(),
                json!({"type": "object", "properties": {"path": file_path.clone(), "max_items": {"type": "integer", "minimum": 1, "maximum": 80, "default": 80, "x-clamp": true}}, "required": ["path"]}), true, true),
            spec("fs.stat", "Size, type, modified time and (optionally) SHA-256 of a file or folder.", read,
                json!({"type": "object", "properties": {"path": file_path.clone(), "hash": {"type": "boolean", "default": false}}, "required": ["path"]}), true, true),
            spec("fs.write", "Create or overwrite a text file, or create a folder. Parents are created unless make_parents is false.", write.clone(),
                json!({"type": "object", "properties": {"path": path.clone(), "content": {"type": "string", "maxLength": 1048576}, "kind": {"type": "string", "enum": ["file", "dir"], "default": "file"}, "mode": {"type": "string", "enum": ["create", "overwrite", "append"], "default": "create"}, "make_parents": {"type": "boolean", "default": true}, "expect_version": {"type": "string", "maxLength": 64}}, "required": ["path"]}), false, false),
            spec("fs.edit", "Replace one unique span. Whitespace-only differences still match. expect_version refuses a changed file. dry_run only checks and counts what it would change.", write.clone(),
                json!({"type": "object", "properties": {"path": file_path.clone(), "old": {"type": "string", "minLength": 1, "maxLength": 65536, "x-aliases": ["oldString", "old_string", "old_text"]}, "new": {"type": "string", "maxLength": 65536, "x-aliases": ["newString", "new_string", "new_text"]}, "replace_all": {"type": "boolean", "default": false}, "expect_count": {"type": "integer", "minimum": 1, "maximum": 10000}, "expect_version": {"type": "string", "maxLength": 64}, "dry_run": {"type": "boolean", "default": false}}, "required": ["path", "old", "new"]}), false, false),
            // On offer only while `AQUA_EDIT_LINES` is on (see `availability`; card OA6-02, ED-07): a prototype that is measured before it is taught.
            spec("fs.edit_lines", "Replace lines start..end with new (empty deletes; end=start-1 inserts). expect_first_line = text of line start; if wrong, nothing changes, real lines show.", write.clone(),
                json!({"type": "object", "properties": {
                    "path": file_path.clone(),
                    "start": {"type": "integer", "minimum": 1, "maximum": 100000000, "x-aliases": ["start_line", "first_line", "line"], "description": "The first line to replace, counted from 1 as fs.read counts."},
                    "end": {"type": "integer", "minimum": 0, "maximum": 100000000, "x-aliases": ["end_line", "last_line"], "description": "The last line to replace; start - 1 inserts before start."},
                    "new": {"type": "string", "maxLength": 65536, "x-aliases": ["newString", "new_string", "new_text"], "description": "The new lines (empty: take the lines out)."},
                    "expect_first_line": {"type": "string", "maxLength": 4096, "x-aliases": ["anchor", "first_line_text"], "description": "The text of line start as fs.read shows it, without the number. Refused, with the real lines shown, if the file has other text there."},
                    "expect_prev_line": {"type": "string", "maxLength": 4096, "description": "The text of line start - 1 (needed when line start is blank, or to add after the last line)."},
                    "expect_version": {"type": "string", "maxLength": 64},
                    "dry_run": {"type": "boolean", "default": false}}, "required": ["path", "start", "end", "new"]}), false, false),
            spec("fs.grep", "Search file contents with a regex: file:line: text. glob like fs.find's name. Skips dotfiles, .gitignore'd, binary, big, secret files; says how many.", vec![PermClass::Read],
                json!({"type": "object", "properties": {"pattern": {"type": "string", "minLength": 1, "maxLength": 500, "x-aliases": ["query", "regex"]}, "path": {"type": "string", "default": "@ws", "x-aliases": PATH_ALIASES}, "glob": {"type": "string", "maxLength": 200}, "fixed_string": {"type": "boolean", "default": false}, "ignore_case": {"type": "boolean", "default": false}, "context": {"type": "integer", "minimum": 0, "maximum": 5, "default": 0, "x-clamp": true}, "max_results": {"type": "integer", "minimum": 1, "maximum": 200, "default": 50, "x-clamp": true}, "include_hidden": {"type": "boolean", "default": false}, "include_ignored": {"type": "boolean", "default": false}}, "required": ["pattern"]}), true, true),
            spec("fs.patch", "Change files all-or-nothing: edits=[{path,old,new}] (old must occur once) or a unified diff. dry_run only checks. One undo id (p_...) undoes it all.", write.clone(),
                json!({"type": "object", "properties": {"edits": {"type": "array", "minItems": 1, "maxItems": 50, "items": {"type": "object", "properties": {"path": path.clone(), "old": {"type": "string", "minLength": 1, "maxLength": 65536}, "new": {"type": "string", "maxLength": 65536}, "replace_all": {"type": "boolean", "default": false}, "expect_count": {"type": "integer", "minimum": 1, "maximum": 10000}}, "required": ["path", "old", "new"]}}, "diff": {"type": "string", "minLength": 1, "maxLength": 262144}, "dry_run": {"type": "boolean", "default": false}}}), false, false),
            spec("fs.copy", "Copy a file, or a folder with recursive=true.", write.clone(),
                json!({"type": "object", "properties": {"src": path.clone(), "dst": path.clone(), "recursive": {"type": "boolean", "default": false}, "overwrite": {"type": "boolean", "default": false}}, "required": ["src", "dst"]}), false, false),
            spec("fs.move", "Move or rename a file or folder.", write.clone(),
                json!({"type": "object", "properties": {"src": path.clone(), "dst": path.clone(), "overwrite": {"type": "boolean", "default": false}}, "required": ["src", "dst"]}), false, false),
            spec("fs.delete", "Delete a file or folder. It goes to the undo store; restore it with fs.restore.", write.clone(),
                json!({"type": "object", "properties": {"path": path.clone(), "recursive": {"type": "boolean", "default": false}}, "required": ["path"]}), false, false),
            spec("fs.restore", "List the undo store, or restore an entry by undo_id to where it came from.", write.clone(),
                json!({"type": "object", "properties": {"undo_id": {"type": "string", "maxLength": 64}, "force": {"type": "boolean", "default": false}, "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20}, "to": {"type": "string", "maxLength": 1024, "description": "Where to put a single entry instead of where it came from; needed for an entry listed as unrecorded (its record was lost)."}}}), false, false),
            spec("fs.undo_run", "Put back every file change one agent run made, newest first; files changed since are left. dry_run only lists. from_step undoes from that step on.", write.clone(),
                json!({"type": "object", "properties": {"run_id": {"type": "string", "minLength": 1, "maxLength": 128}, "dry_run": {"type": "boolean", "default": false}, "from_step": {"type": "integer", "minimum": 1, "description": "The first step of the run to undo (the first file-changing call of the run is step 1); later steps go too."}, "scope": {"type": "string", "enum": ["all", "files", "commands"], "default": "all", "description": "files: only what the file tools changed; commands: only what commands changed (their journal and the run's checkpoint); all: both."}}, "required": ["run_id"]}), false, false),
            // The manifest's classes are the least the call is; `classify` adds Privileged when a folder is really being allowed (the host joins the two), so that a folder a root holds already asks nothing.
            spec("fs.allow_folder", "Ask the person to allow a folder they named in their request (full path). They are asked every time; it lasts until Aqua restarts.", vec![PermClass::Read],
                json!({"type": "object", "properties": {"path": path.clone(), "access": {"type": "string", "enum": ["rw", "ro"], "default": "rw"}}, "required": ["path"]}), false, false),
            // Taking access away asks nothing (it can only make the file tools see less), but it is a change, so a spoken request, a replayed recipe and a locked host do not get it.
            spec("fs.revoke_folder", "Take back a folder allowed with fs.allow_folder (its @name or full path). A folder from the settings cannot be taken back.", write.clone(),
                json!({"type": "object", "properties": {"path": {"type": "string", "minLength": 1, "maxLength": 1024}}, "required": ["path"]}), false, false),
            spec("fs.set_attrs", "Set or clear the read-only attribute of a file.", write,
                json!({"type": "object", "properties": {"path": path, "readonly": {"type": "boolean"}}, "required": ["path", "readonly"]}), false, false),
        ]
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        let g = &self.guard;
        if tool == "fs.allow_folder" {
            // Allowing a folder is the person's decision, in every mode (a privileged action always asks): the question names the folder and says what it lets the tools do. A folder a root holds
            // already is nothing to ask about.
            let access = access_arg(args)?;
            let plan = g.plan_grant(arg_str(args, "path"), access)?;
            let shown = display(&plan.path);
            if let Some(alias) = &plan.already {
                return Ok(Classification { classes: vec![PermClass::Read], target: shown.clone(), summary: format!("The folder {shown} is already allowed (@{alias})"), target_allowed: true });
            }
            let what = if access == Access::Rw { "read and write files" } else { "read files" };
            let made = if plan.exists { "" } else { " The folder does not exist yet and will be made." };
            return Ok(Classification {
                classes: vec![PermClass::Privileged],
                target: shown.clone(),
                summary: format!("Allow Aqua's file tools to {what} in the folder {shown} until Aqua restarts. Each change there still asks you.{made}"),
                target_allowed: false,
            });
        }
        if tool == "fs.revoke_folder" {
            // Nothing to ask: the call only removes a grant. A Write class with a target the provider vouches for runs unasked in the ordinary modes and stays out of the voice's list.
            let asked = truncate(arg_str(args, "path").trim(), 120);
            if asked.is_empty() {
                return Err(ToolError::new(ErrorCode::InvalidArguments, "name the folder by its @name or by its full path"));
            }
            return Ok(Classification { classes: vec![PermClass::Write], target: asked.clone(), summary: format!("Take back the folder {asked}: the file tools lose access to it"), target_allowed: true });
        }
        let is_read_tool = matches!(tool, "fs.list" | "fs.find" | "fs.grep" | "fs.read" | "fs.outline" | "fs.stat");
        let restore_list = tool == "fs.restore" && arg_str(args, "undo_id").is_empty();
        if is_read_tool || restore_list {
            let raw = if tool == "fs.grep" && arg_str(args, "path").is_empty() { "@ws" } else { arg_str(args, "path") };
            if tool == "fs.restore" {
                return Ok(Classification { classes: vec![PermClass::Read], target: "undo store".into(), summary: "List the undo store".into(), target_allowed: true });
            }
            if raw == "@" || raw.is_empty() && tool == "fs.list" {
                return Ok(Classification { classes: vec![PermClass::Read], target: "roots".into(), summary: "List the allowed folders".into(), target_allowed: true });
            }
            let r = g.resolve(raw, Op::Read)?;
            let mut classes = vec![PermClass::Read];
            if r.sensitive {
                classes.push(PermClass::Credential);
            }
            let summary = if tool == "fs.read" && r.sensitive && arg_bool(args, "reveal") { format!("read {} and show its real values to the model", display(&r.path)) } else { format!("{} {}", tool.trim_start_matches("fs."), display(&r.path)) };
            return Ok(Classification { classes, target: display(&r.path), summary, target_allowed: true });
        }

        // Everything below changes something.
        let mut classes = vec![PermClass::Write];
        let mut trusted = true;
        let note = |r: &Resolved, classes: &mut Vec<PermClass>, trusted: &mut bool| {
            *trusted &= r.trusted;
            if r.executable_kind && !classes.contains(&PermClass::Execute) {
                classes.push(PermClass::Execute);
            }
            if r.sensitive && !classes.contains(&PermClass::Credential) {
                classes.push(PermClass::Credential);
            }
        };
        let summary;
        let target;
        match tool {
            "fs.write" | "fs.edit" | "fs.edit_lines" | "fs.delete" | "fs.set_attrs" => {
                let r = g.resolve(arg_str(args, "path"), Op::Write)?;
                note(&r, &mut classes, &mut trusted);
                target = display(&r.path);
                let exists = r.path.exists();
                summary = match tool {
                    "fs.write" if arg_str(args, "kind") == "dir" => format!("Create folder {target}"),
                    "fs.write" => {
                        let n = arg_str(args, "content").len();
                        let mode = match arg_str(args, "mode") { "" => "create", m => m };
                        format!("Write {n} bytes to {target} ({mode}{})", if exists && mode == "overwrite" { "; the old content is kept for undo" } else { "" })
                    }
                    "fs.edit" => format!("Edit {target}: replace {:?}{}", truncate(arg_str(args, "old"), 60), if arg_bool(args, "dry_run") { " (dry run: nothing is written)" } else { "" }),
                    "fs.edit_lines" => edit_lines::question_summary(&target, args),
                    "fs.delete" => format!("Delete {target} (recoverable with fs.restore)"),
                    _ => format!("Set the read-only attribute of {target} to {}", arg_bool(args, "readonly")),
                };
            }
            "fs.copy" | "fs.move" => {
                let from = g.resolve(arg_str(args, "src"), if tool == "fs.move" { Op::Write } else { Op::Read })?;
                let to = g.resolve(arg_str(args, "dst"), Op::Write)?;
                note(&to, &mut classes, &mut trusted);
                if tool == "fs.move" {
                    note(&from, &mut classes, &mut trusted);
                } else {
                    trusted &= from.trusted || true;
                    if from.sensitive && !classes.contains(&PermClass::Credential) {
                        classes.push(PermClass::Credential);
                    }
                }
                target = format!("{} -> {}", display(&from.path), display(&to.path));
                summary = format!("{} {target}", if tool == "fs.move" { "Move" } else { "Copy" });
            }
            "fs.patch" => {
                let paths = patch_targets(g, args)?;
                for (r, _) in &paths {
                    note(r, &mut classes, &mut trusted);
                }
                let names: Vec<String> = paths.iter().map(|(r, _)| display(&r.path)).collect();
                target = names.join(", ");
                let dry = if arg_bool(args, "dry_run") { " (dry run: nothing is written)" } else { "" };
                summary = format!("Patch {} file{}: {}{dry}", names.len(), if names.len() == 1 { "" } else { "s" }, truncate(&target, 200));
            }
            "fs.restore" => {
                let id = arg_str(args, "undo_id");
                if id.starts_with("p_") {
                    if !arg_str(args, "to").is_empty() {
                        return Err(ToolError::new(ErrorCode::InvalidArguments, "to puts back one entry; a patch group is put back where its files were"));
                    }
                    let entries = self.trash.group(id);
                    if entries.is_empty() {
                        return Err(ToolError::new(ErrorCode::NotFound, format!("no undo group '{id}'")));
                    }
                    let mut names = Vec::new();
                    for e in &entries {
                        let r = g.resolve(&e.original_path, Op::Write)?;
                        note(&r, &mut classes, &mut trusted);
                        names.push(display(&r.path));
                    }
                    target = names.join(", ");
                    summary = format!("Restore {} files from undo group {id}", names.len());
                } else {
                    let entry = self.trash.get(id).ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("no undo entry '{id}'")))?;
                    let r = restore_target(g, &entry, args)?;
                    note(&r, &mut classes, &mut trusted);
                    target = display(&r.path);
                    summary = format!("Restore {target} from undo entry {id}");
                }
            }
            "fs.undo_run" => {
                let id = arg_str(args, "run_id");
                let scope = undo_scope(args)?;
                let entries = run_entries_from(&self.trash, id, arg_u64(args, "from_step", 0), scope);
                let mut effects = 0usize;
                for e in &entries {
                    // An entry that cannot be resolved is skipped when the undo runs; the others decide how it is judged.
                    if let Ok(r) = g.resolve(&e.original_path, Op::Write) {
                        note(&r, &mut classes, &mut trusted);
                    }
                    // Putting back what a command or a checkpoint changed can touch many files at once, and the person has to say yes to that every time, in every mode (card OA7-03):
                    // the same rule as `term.undo`.
                    if matches!(e.reason.as_str(), "command" | "checkpoint") {
                        effects += 1;
                    }
                }
                if effects > 0 {
                    if !classes.contains(&PermClass::Destructive) {
                        classes.push(PermClass::Destructive);
                    }
                    trusted = false;
                }
                target = format!("run {id}");
                let dry = if arg_bool(args, "dry_run") { " (dry run: nothing is changed)" } else { "" };
                // As it always said it, until the run has a record of what a command changed.
                let of = if effects > 0 { format!(", {effects} of them what a command or the run's checkpoint recorded") } else { String::new() };
                summary = format!("Undo the {} file change{} of run {id}{of}{dry}", entries.len(), if entries.len() == 1 { "" } else { "s" });
            }
            other => return Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
        }
        Ok(Classification { classes, target, summary, target_allowed: trusted })
    }

    /// What an edit, a write, a patch or a delete of a file would do, as a diff for the question (card OA6-04, ED-05, SE-03, UX-07). Worked out in memory with the code the call itself runs, from
    /// the file as it is now, with the guard's checked open and the same size limits; a file that looks like it holds secrets is not read at all.
    fn approval_preview(&self, tool: &str, args: &Value) -> Option<ChangePreview> {
        match tool {
            "fs.edit" => preview_edit(&self.guard, args),
            "fs.edit_lines" => edit_lines::preview_edit_lines(&self.guard, args),
            "fs.write" => preview_write(&self.guard, args),
            "fs.patch" => preview_patch(&self.guard, args),
            "fs.delete" => preview_delete(&self.guard, args),
            _ => None,
        }
    }

    /// The folders an edit, a write or a patch would write in (card OA6-05, ED-10): the folder that holds each file, as the guard resolves it (links and `..` are gone from it). Only for a call that
    /// changes or creates the text of files in a trusted folder: not a folder that is made, not a delete (a patch that deletes a file is none), not a move or a copy, and not a path the guard
    /// refuses to write (`.vscode`, `.cargo`, `.husky`...: `resolve` says no, and so does this). The host reads it to see whether a run's edit grant names the folder.
    fn edit_scope(&self, tool: &str, args: &Value) -> Option<aqua_tool_host::EditScope> {
        let g = &self.guard;
        let folder_of = |r: &Resolved| r.path.parent().map(display);
        match tool {
            "fs.edit" | "fs.write" => {
                if tool == "fs.write" && arg_str(args, "kind") == "dir" {
                    return None;
                }
                let r = g.resolve(arg_str(args, "path"), Op::Write).ok().filter(|r| r.trusted)?;
                Some(aqua_tool_host::EditScope::new(folder_of(&r)))
            }
            "fs.patch" => {
                let targets = patch_targets(g, args).ok()?;
                if targets.is_empty() || targets.iter().any(|(r, op)| *op == PatchOp::Delete || !r.trusted) {
                    return None;
                }
                Some(aqua_tool_host::EditScope::new(targets.iter().filter_map(|(r, _)| folder_of(r))))
            }
            _ => None,
        }
    }

    /// Reading a file of secrets with its real values (`reveal: true`) asks every time (card OA8-03, SE-08): the values go into the model's context, and from there into whatever the run
    /// sends out later. Without `reveal` the read shows the names and the length of each value (`MaskedView`), which asks as a read of such a file always did.
    fn always_ask_reason(&self, tool: &str, args: &Value) -> Option<String> {
        if tool != "fs.read" || !arg_bool(args, "reveal") {
            return None;
        }
        let r = self.guard.resolve(arg_str(args, "path"), Op::Read).ok()?;
        r.sensitive.then(|| "this puts the raw secrets into the model's context".to_string())
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let guard = self.guard.clone();
        let trash = self.trash.clone();
        let effects = self.effects.clone();
        let gate = self.syntax.as_ref().map(|g| g()).unwrap_or_default();
        // The host does not call a tool that `availability` says is off; this is the second lock, for a caller that does not ask the host (card OA6-02).
        let lines_off = tool == "fs.edit_lines" && !self.edit_lines.as_ref().is_some_and(|on| on());
        let cancel = ctx.cancel.clone();
        let tool = tool.to_string();
        let run_id = ctx.agent_run_id.clone();
        let shown = ctx.shown.clone();
        Box::pin(async move {
            if lines_off {
                return Err(ToolError::new(ErrorCode::Unavailable, "fs.edit_lines is off (AQUA_EDIT_LINES=on turns it on)"));
            }
            tokio::task::spawn_blocking(move || {
                // What this call changes is recorded under the run that made it, so the run can be undone as a whole; and it is held to what the person was shown, when they were asked.
                let mut out = with_run(&run_id, || with_shown(shown.as_deref(), || with_gate(gate, || run(&guard, &trash, effects.as_deref(), &tool, &args, &cancel))))?;
                mark_read(&guard, &tool, &args, &mut out);
                Ok(out)
            })
                .await
                .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("file task failed: {e}")))?
        })
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// What a read tells the host about its result: anything from a folder other than the trusted one, or a file that looks like it holds secrets,
/// is private to the person. The text of a file is still untrusted wherever it is (a cloned repository's README can say anything), so a read
/// never lifts the taint.
fn mark_read(g: &PathGuard, tool: &str, args: &Value, out: &mut ProviderOutput) {
    if !matches!(tool, "fs.list" | "fs.find" | "fs.grep" | "fs.read" | "fs.outline" | "fs.stat") {
        return;
    }
    let raw = if tool == "fs.grep" && arg_str(args, "path").is_empty() { "@ws" } else { arg_str(args, "path") };
    if raw == "@" || raw.is_empty() && tool == "fs.list" {
        // The list of the allowed folders is made of names from Aqua's own settings: no text of any file (card OA6-05).
        out.local = true;
        return;
    }
    if let Ok(r) = g.resolve(raw, Op::Read) {
        out.private = !r.trusted || r.sensitive;
        // (Card OA6-05, ED-10.) Text of files inside a trusted folder of the project is the one kind of text that does not count as read from outside the project, which is what suspends a
        // run's edit grant. Not a file outside a trusted folder (those are `private` above), not a file of secrets, and not a search that could reach the folder of kept tool output: it holds
        // the text of web pages and of commands, and may sit inside the workspace.
        out.local = r.trusted && !r.sensitive && !g.out_root().is_some_and(|kept| within(&kept, &r.path));
    }
}

fn out(data: Value, text: String) -> Result<ProviderOutput, ToolError> {
    Ok(ProviderOutput { data, text, ..Default::default() })
}

// ---- the preview of a change (card OA6-04: ED-05, SE-03, UX-07, SE-13) ----------------------------------------------------------------------------------------

/// A file is shown in a question only up to this size; a bigger one is described by its size.
const PREVIEW_MAX_FILE_BYTES: u64 = 1 << 20;
/// Lines of hunks of the diff a question shows for one file, and for a patch: in each file and in all of it.
const PREVIEW_LINES: usize = 60;
const PREVIEW_PATCH_FILE_LINES: usize = 40;
const PREVIEW_PATCH_LINES: usize = 120;
/// What a delete shows of the file it takes away.
const PREVIEW_DELETE_LINES: usize = 20;
/// Lines of unchanged text around a change in a diff.
const PREVIEW_CONTEXT: usize = 2;
/// Lines of the diff a result carries for a file (`data.changes`), and for all the files of one call.
const RESULT_DIFF_LINES: usize = 40;
const RESULT_DIFF_TOTAL_LINES: usize = 200;

/// What the question showed of the files one call changes: for each path, the SHA-256 of the file as the preview read it, or `absent` when it was not there (card OA6-04, SE-13). It goes
/// to the host as the preview's guard and comes back with the call (`ExecCtx::shown`), and the call refuses to change a file that is no longer what was shown.
#[derive(Default)]
struct ShownFiles(std::collections::BTreeMap<String, String>);

impl ShownFiles {
    fn one(path: &str, sha: &str) -> Self {
        Self(std::iter::once((path.to_string(), sha.to_string())).collect())
    }

    fn to_guard(&self) -> Option<String> {
        (!self.0.is_empty()).then(|| json!({"fs": 1, "files": self.0}).to_string())
    }

    fn parse(guard: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(guard).ok()?;
        let files = v.get("files")?.as_object()?;
        Some(Self(files.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect()))
    }
}

thread_local! {
    /// What the person was shown of the files the call running on this thread changes (see [`ShownFiles`]); nothing when no question was put for it.
    static SHOWN: std::cell::RefCell<Option<ShownFiles>> = const { std::cell::RefCell::new(None) };
}

/// Runs `f` with `guard` (what the host kept of the question, `ExecCtx::shown`) as what this thread's call was shown. The previous value comes back when `f` ends, however it ends.
fn with_shown<R>(guard: Option<&str>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<ShownFiles>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SHOWN.with(|s| *s.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(SHOWN.with(|s| s.replace(guard.and_then(ShownFiles::parse))));
    f()
}

thread_local! {
    /// What the syntax gate is set to for the call running on this thread (card OA6-03).
    static GATE: std::cell::Cell<super::ast::Gate> = const { std::cell::Cell::new(super::ast::Gate::Off) };
}

/// Runs `f` with the syntax gate set to `gate`; the previous setting comes back when `f` ends, however it ends.
fn with_gate<R>(gate: super::ast::Gate, f: impl FnOnce() -> R) -> R {
    struct Restore(super::ast::Gate);
    impl Drop for Restore {
        fn drop(&mut self) {
            GATE.with(|g| g.set(self.0));
        }
    }
    let _restore = Restore(GATE.with(|g| g.replace(gate)));
    f()
}

/// What the syntax gate says about a change of `path` from `old` to `new`, with the setting of the call running on this thread (card OA6-03).
fn syntax_verdict(path: &Path, old: &str, new: &str) -> super::ast::Verdict {
    super::ast::check_change(GATE.with(|g| g.get()), &display(path), old, new)
}

/// The refusal of a change by the syntax gate, as the error of the call: nothing was changed.
fn syntax_refusal(path: &Path, why: &str) -> ToolError {
    ToolError::new(ErrorCode::Conflict, format!("nothing was changed: {}: {why}", display(path)))
}

/// `Ok` when the file is what the question showed (`now` is its SHA-256 as the call has just read it, or `absent`), or the question showed nothing of it. A file that changed between the
/// question and the answer is not changed by this call: the person said yes to another change (card OA6-04, SE-13).
fn check_shown(path: &Path, now: &str) -> Result<(), ToolError> {
    let name = display(path);
    let was = SHOWN.with(|s| s.borrow().as_ref().and_then(|files| files.0.get(&name).cloned()));
    match was {
        Some(was) if was != now => Err(ToolError::new(
            ErrorCode::Conflict,
            format!("ERROR [FILE_CHANGED] {name} is not what the question showed: it changed after you were asked. Read it again and repeat the change; the person is asked again"),
        )),
        _ => Ok(()),
    }
}

/// A text file the preview may read: the one the guard resolved, a text file of at most [`PREVIEW_MAX_FILE_BYTES`], read from the handle that was checked. Its SHA-256 (what the change is
/// later held to) and its text. `None` for a folder, a binary file, a file that is not UTF-8, or one that is too big.
fn read_for_preview(g: &PathGuard, r: &Resolved) -> Option<(String, String)> {
    let meta = std::fs::metadata(&r.path).ok()?;
    if !meta.is_file() || meta.len() > PREVIEW_MAX_FILE_BYTES.min(g.limits().edit_max_file_bytes) {
        return None;
    }
    let bytes = read_checked(r).ok()?;
    if looks_binary(&bytes[..bytes.len().min(8192)]) {
        return None;
    }
    let sha = sha256_hex(&bytes);
    String::from_utf8(bytes).ok().map(|text| (sha, text))
}

/// A file's text as a diff compares it: no byte order mark, every line break `\n`.
fn plain(text: &str) -> String {
    super::patch::lf(text.strip_prefix('\u{feff}').unwrap_or(text))
}

/// The edit of a call of `fs.edit` (and of one entry of `fs.patch`'s `edits`).
fn edit_of(args: &Value) -> super::patch::Edit {
    super::patch::Edit {
        old: arg_str(args, "old").to_string(),
        new: arg_str(args, "new").to_string(),
        replace_all: arg_bool(args, "replace_all"),
        expect_count: args.get("expect_count").and_then(|v| v.as_u64()).map(|n| n as usize),
    }
}

/// The text of a diff for a question: the diff, and, for a file that can run code, a line that says so when only part of it is shown (a script cut at its sixtieth line can hide what it does).
fn shown_diff(d: &diff::Diff, r: &Resolved) -> String {
    let mut text = d.text.clone();
    if r.executable_kind && d.truncated {
        text.push_str("This file can run code, and only part of it is shown above.\n");
    }
    text
}

fn guarded(text: String, files: ShownFiles) -> ChangePreview {
    ChangePreview { text, guard: files.to_guard() }
}

/// What `fs.edit` would change, as a diff of the file as it is now (card OA6-04, ED-05). Worked out with the code the edit itself runs, in memory; any error gives no preview, and the call
/// reports it itself.
fn preview_edit(g: &PathGuard, args: &Value) -> Option<ChangePreview> {
    let r = g.resolve(arg_str(args, "path"), Op::Write).ok()?;
    if r.sensitive {
        return Some(ChangePreview::new(aqua_tool_host::PREVIEW_HIDDEN));
    }
    let (sha, text) = read_for_preview(g, &r)?;
    if refuse_read_only(&r.path).is_err() || args.get("expect_version").and_then(|v| v.as_str()).is_some_and(|v| !v.is_empty() && v != sha) {
        return None;
    }
    let mut doc = super::patch::Doc::parse(&text);
    let before = doc.text().to_string();
    super::patch::apply_edit(&mut doc, &edit_of(args)).ok()?;
    let name = display(&r.path);
    let d = diff::diff(&name, &name, &before, doc.text(), PREVIEW_CONTEXT, PREVIEW_LINES);
    let body = if d.is_empty() { format!("No change: {name} stays as it is.") } else { shown_diff(&d, &r) };
    Some(guarded(body, ShownFiles::one(&name, &sha)))
}

/// What `fs.write` would put on disk: the first lines of a new file, the lines appended, or the diff of a file that is overwritten (card OA6-04, ED-05, SE-03). A change of line ending or of
/// the byte order mark is said, since the diff of the text does not show it.
fn preview_write(g: &PathGuard, args: &Value) -> Option<ChangePreview> {
    if arg_str(args, "kind") == "dir" {
        return None;
    }
    let r = g.resolve(arg_str(args, "path"), Op::Write).ok()?;
    if r.sensitive {
        return Some(ChangePreview::new(aqua_tool_host::PREVIEW_HIDDEN));
    }
    let content = arg_str(args, "content");
    if content.len() as u64 > g.limits().write_max_bytes {
        return None;
    }
    let name = display(&r.path);
    let new_text = plain(content);
    let exists = r.path.exists();
    if exists && !r.path.is_file() {
        return None;
    }
    match if arg_str(args, "mode").is_empty() { "create" } else { arg_str(args, "mode") } {
        "create" if !exists => {
            let d = diff::created(&name, &new_text, PREVIEW_LINES);
            let body = if d.is_empty() { format!("A new, empty file: {name}.") } else { shown_diff(&d, &r) };
            Some(guarded(body, ShownFiles::one(&name, "absent")))
        }
        "append" => {
            let d = diff::diff(&name, &format!("{name} (added at the end)"), "", &new_text, 0, PREVIEW_LINES);
            (!d.is_empty()).then(|| ChangePreview::new(shown_diff(&d, &r)))
        }
        "overwrite" if !exists => {
            let d = diff::created(&name, &new_text, PREVIEW_LINES);
            let body = if d.is_empty() { format!("A new, empty file: {name}.") } else { shown_diff(&d, &r) };
            Some(guarded(body, ShownFiles::one(&name, "absent")))
        }
        "overwrite" => {
            let Some((sha, old)) = read_for_preview(g, &r) else {
                // A file that is binary, not text, or too big for a diff is described, not shown; a change to it is still held to what the preview read when it can be hashed.
                let size = std::fs::metadata(&r.path).map(|m| m.len()).unwrap_or(0);
                let note = format!("Replaces {name} ({size} bytes, a file that is binary or too large to show) with {} bytes.", content.len());
                let sha = (size <= g.limits().hash_max_bytes).then(|| sha256_file(&r.path).ok()).flatten();
                return Some(match sha {
                    Some(sha) => guarded(note, ShownFiles::one(&name, &sha)),
                    None => ChangePreview::new(note),
                });
            };
            let before = plain(&old);
            let d = diff::diff(&name, &name, &before, &new_text, PREVIEW_CONTEXT, PREVIEW_LINES);
            let mut body = if d.is_empty() { format!("No change in the lines of {name}.\n") } else { shown_diff(&d, &r) };
            // What the diff of the text cannot show: the line endings and the byte order mark are written as the content gives them, and the content is not changed to match the file.
            let (was, becomes) = (super::patch::Doc::parse(&old), super::patch::Doc::parse(content));
            if was.eol_label() != becomes.eol_label() && old.contains('\n') && content.contains('\n') {
                body.push_str(&format!("The line endings change from {} to {}.\n", was.eol_label().to_ascii_uppercase(), becomes.eol_label().to_ascii_uppercase()));
            }
            if was.bom() != becomes.bom() {
                body.push_str(if was.bom() { "The byte order mark at the start of the file is dropped.\n" } else { "A byte order mark is added at the start of the file.\n" });
            }
            Some(guarded(body, ShownFiles::one(&name, &sha)))
        }
        _ => None,
    }
}

/// What `fs.patch` would change, one diff for each file that changes (card OA6-04, ED-05): the patch is planned in memory with the code the patch itself runs, so that what is shown is what
/// would be written. At most [`PREVIEW_PATCH_LINES`] lines in all; a file that is not shown is counted.
fn preview_patch(g: &PathGuard, args: &Value) -> Option<ChangePreview> {
    let targets = patch_targets(g, args).ok()?;
    if targets.iter().any(|(r, _)| r.sensitive) {
        return Some(ChangePreview::new(aqua_tool_host::PREVIEW_HIDDEN));
    }
    let plan = patch_plan(g, args, targets).ok()?;
    let mut files = ShownFiles::default();
    let mut parts: Vec<String> = Vec::new();
    let mut left = PREVIEW_PATCH_LINES;
    let mut not_shown = 0usize;
    for p in plan.files.iter().filter(|p| p.original != p.updated || p.op != PatchOp::Update) {
        let name = display(&p.resolved.path);
        files.0.insert(name.clone(), if p.op == PatchOp::Create { "absent".to_string() } else { p.original_sha.clone() });
        if left == 0 {
            not_shown += 1;
            continue;
        }
        let lines = PREVIEW_PATCH_FILE_LINES.min(left);
        let d = match p.op {
            PatchOp::Create => diff::created(&name, &plain(&p.updated), lines),
            PatchOp::Delete => diff::removed(&name, &plain(&p.original), PREVIEW_DELETE_LINES.min(lines)),
            PatchOp::Update => diff::diff(&name, &name, &plain(&p.original), &plain(&p.updated), PREVIEW_CONTEXT, lines),
        };
        left = left.saturating_sub(d.text.lines().count().saturating_sub(2));
        parts.push(shown_diff(&d, &p.resolved));
    }
    if parts.is_empty() && not_shown == 0 {
        return Some(ChangePreview::new("The patch changes nothing: every file already has this content."));
    }
    let mut text = parts.join("\n");
    if not_shown > 0 {
        text.push_str(&format!("\n\u{2026} {not_shown} more file{} changed, not shown here\n", if not_shown == 1 { "" } else { "s" }));
    }
    Some(guarded(text, files))
}

/// What `fs.delete` of a file would take away: its first lines (card OA6-04, UX-07). A folder is not shown: the question names it and says it can be restored.
fn preview_delete(g: &PathGuard, args: &Value) -> Option<ChangePreview> {
    let r = g.resolve(arg_str(args, "path"), Op::Write).ok()?;
    if r.sensitive {
        return Some(ChangePreview::new(aqua_tool_host::PREVIEW_HIDDEN));
    }
    let (_, text) = read_for_preview(g, &r)?;
    let name = display(&r.path);
    let d = diff::removed(&name, &plain(&text), PREVIEW_DELETE_LINES);
    (!d.is_empty()).then(|| ChangePreview::new(d.text))
}

/// One entry of `data.changes` of a result: what a call did to one file, with its diff as the model may read it ([`diff::Diff::model_text`]: the hunks, the counts and the line numbers, the
/// lines the model wrote itself, and no line of the file that it did not write; card OA6-04, follow-up of the security review). A file that looks like it holds secrets has no diff in the
/// entry, only the counts. The whole diff, with the lines around the change and the lines taken out, is what the person's question carries (`approval_preview`), and nothing else.
fn change_entry(name: &str, op: &str, hidden: bool, d: Option<&diff::Diff>) -> Value {
    let (added, removed) = d.map(|d| (json!(d.added), json!(d.removed))).unwrap_or((Value::Null, Value::Null));
    json!({
        "path": name, "op": op, "added": added, "removed": removed,
        "diff": if hidden { Value::Null } else { d.map(|d| json!(d.model_text)).unwrap_or(Value::Null) },
        "hidden": hidden, "truncated": d.is_some_and(|d| d.model_truncated),
    })
}

/// The lines the model supplied in a call of `fs.edit`: the text it put in `new`.
fn supplied_by_edit(edit: &super::patch::Edit) -> diff::Supplied {
    diff::Supplied::of([edit.new.as_str()])
}

/// The lines the model supplied in a call of `fs.patch`: the `new` of every edit, or the lines that start with `+` in the diff it gave.
fn supplied_by_patch(args: &Value) -> diff::Supplied {
    let mut texts: Vec<&str> = args.get("edits").and_then(|v| v.as_array()).map(|edits| edits.iter().map(|e| arg_str(e, "new")).collect()).unwrap_or_default();
    let added: String = arg_str(args, "diff").lines().filter(|l| !l.starts_with("+++ ")).filter_map(|l| l.strip_prefix('+')).flat_map(|l| [l, "\n"]).collect();
    texts.push(&added);
    diff::Supplied::of(texts)
}

/// The failure of an edit as a tool error: a miss that quotes lines of the file is marked, so that the host fences it and counts the run as having read the file (it is not a tool whose
/// results are all untrusted, and every other answer of it holds only what the model wrote).
fn edit_failure(code: ErrorCode, message: String, of_the_project: bool) -> ToolError {
    let holds = super::patch::holds_file_text(&message);
    let e = ToolError::new(code, message);
    // (Card OA6-05.) The lines quoted are of the file this call was asked to edit: when that is a file of a trusted folder (and no file of secrets), they are text of the project, which does
    // not end a run's edit grant; a file anywhere else is outside text.
    match (holds, of_the_project) {
        (true, true) => e.holding_outside_text().holding_local_text(),
        (true, false) => e.holding_outside_text(),
        (false, _) => e,
    }
}

fn run(g: &PathGuard, trash: &Trash, effects: Option<&dyn EffectUndo>, tool: &str, args: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
    match tool {
        "fs.undo_run" => undo_run(g, trash, effects, args, cancel),
        "fs.list" => list(g, args, cancel),
        "fs.find" => find(g, args, cancel),
        "fs.grep" => grep(g, args, cancel),
        "fs.read" => read(g, args),
        "fs.outline" => outline(g, args),
        "fs.stat" => stat(g, args),
        "fs.write" => write(g, trash, args),
        "fs.edit" => edit(g, trash, args),
        "fs.edit_lines" => edit_lines::edit_lines(g, trash, args),
        "fs.patch" => patch(g, trash, args),
        "fs.copy" => copy(g, trash, args),
        "fs.move" => mv(g, trash, args),
        "fs.delete" => delete(g, trash, args),
        "fs.restore" => restore(g, trash, args),
        "fs.set_attrs" => set_attrs(g, args),
        "fs.allow_folder" => allow_folder(g, args),
        "fs.revoke_folder" => revoke_folder(g, args),
        other => Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
    }
}

fn access_arg(args: &Value) -> Result<Access, ToolError> {
    match arg_str(args, "access") {
        "" | "rw" => Ok(Access::Rw),
        "ro" => Ok(Access::Ro),
        other => Err(ToolError::new(ErrorCode::InvalidArguments, format!("access must be \"rw\" or \"ro\", not \"{other}\""))),
    }
}

/// Add the folder the person said yes to (the host asked before this ran). Nothing here decides whether the person may be asked: the guard refuses the places that are never handed out.
fn allow_folder(g: &PathGuard, args: &Value) -> Result<ProviderOutput, ToolError> {
    let access = access_arg(args)?;
    let (alias, path, added) = g.grant_folder(arg_str(args, "path"), access)?;
    let shown = display(&path);
    let text = if added {
        format!("Allowed {shown} as @{alias} ({}). It stays allowed until Aqua restarts; each change there still asks the person.", if access == Access::Rw { "read and write" } else { "read only" })
    } else {
        format!("{shown} is already allowed (@{alias}).")
    };
    out(json!({"alias": format!("@{alias}"), "path": shown, "access": access, "added": added}), text)
}

/// Take back a folder that `fs.allow_folder` added. Only a grant can go; a folder from the settings, and a name or place nothing was allowed under, are refused with the reason.
fn revoke_folder(g: &PathGuard, args: &Value) -> Result<ProviderOutput, ToolError> {
    let asked = arg_str(args, "path");
    match g.take_back_folder(asked)? {
        Some((alias, path)) => {
            let shown = display(&path);
            out(json!({"alias": format!("@{alias}"), "path": shown, "revoked": true}), format!("Took back {shown} (@{alias}): the file tools can no longer use it."))
        }
        None => {
            let now: Vec<String> = g.granted_folders().iter().map(|(a, p, _)| format!("@{a} ({})", display(p))).collect();
            let now = if now.is_empty() { "none".to_string() } else { now.join(", ") };
            Err(ToolError::new(ErrorCode::NotFound, format!("no folder is allowed by \"{}\". Folders allowed for a time now: {now}", truncate(asked.trim(), 120))))
        }
    }
}

/// A glob argument (`pattern`, `name`, `glob`) made into a [`Glob`]: nothing when it is empty, and an error that says what is wrong when it cannot be read, never a search that matches nothing.
fn glob_arg(raw: &str, what: &str) -> Result<Option<Glob>, ToolError> {
    if raw.trim().is_empty() {
        return Ok(None);
    }
    Glob::new(raw).map(Some).map_err(|why| {
        ToolError::new(
            ErrorCode::InvalidArguments,
            format!("the {what} \"{}\" cannot be used: {why}. A pattern without a / matches file names at any depth (*.rs, test_?.py, *.{{ts,tsx}}); one with a / or ** matches the path from the searched folder (src/**/*.ts).", truncate(raw, 80)),
        )
    })
}

/// A line that says why a glob found nothing: it is looked for when files were looked at and none matched.
fn glob_note(glob: &Option<Glob>, st: &WalkStats) -> String {
    match glob {
        Some(g) if st.matched == 0 && st.glob_rejected > 0 => format!(
            "\n… the pattern \"{}\" matched none of the {} files seen. A pattern without a / is a file name at any depth; with a / or ** it is the path from the searched folder (src/**/*.ts).",
            truncate(g.source(), 80),
            st.glob_rejected
        ),
        _ => String::new(),
    }
}

/// The footer of a result, on its own line (see `WalkStats::footer`).
fn footer_line(g: &PathGuard, st: &WalkStats, with_searched: bool) -> String {
    st.footer(g.limits().read_max_bytes, with_searched).map(|f| format!("\n{f}")).unwrap_or_default()
}

fn list(g: &PathGuard, args: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
    let raw = arg_str(args, "path");
    if raw == "@" || raw.is_empty() {
        let roots: Vec<Value> = g.roots().into_iter().map(|(a, p, acc)| json!({"alias": format!("@{a}"), "path": display(&p), "access": acc})).collect();
        let text = roots.iter().map(|r| format!("{} {} ({})", r["alias"].as_str().unwrap_or(""), r["path"].as_str().unwrap_or(""), r["access"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n");
        return out(json!({ "roots": roots }), text);
    }
    let r = g.resolve(raw, Op::Read)?;
    let depth = arg_u64(args, "depth", 1).clamp(1, 3) as usize;
    let limit = (arg_u64(args, "limit", 200) as usize).min(g.limits().list_max_entries);
    let glob = glob_arg(arg_str(args, "pattern"), "pattern")?;
    let meta = std::fs::metadata(&r.path).map_err(|e| io_err(e, &display(&r.path)))?;
    if !meta.is_dir() {
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{} is a file; use fs.read or fs.stat", display(&r.path))));
    }
    // Level by level, so that a limit that is reached leaves out the deep entries and not the shallow ones; shown as a tree, a folder's own entries after the folder, folders first.
    let opts = WalkOpts { include_hidden: arg_bool(args, "include_hidden"), include_ignored: arg_bool(args, "include_ignored"), max_depth: depth, emit_dirs: true, glob: glob.clone(), ..WalkOpts::default() };
    let mut entries: Vec<ListItem> = Vec::new();
    let mut truncated = false;
    let st = walk::walk(g, &r.path, &opts, cancel, &mut |e| {
        if entries.len() >= limit {
            truncated = true;
            return Visit::Stop;
        }
        let kind = match e.kind {
            Kind::File => "file",
            Kind::Dir => "dir",
            Kind::Link => "link",
        };
        entries.push(ListItem { rel: e.rel.to_string(), kind, size: if e.kind == Kind::File { e.meta.len() } else { 0 }, modified: modified_ms(e.meta) });
        Visit::Searched
    });
    entries.sort_by(tree_order);
    let values: Vec<Value> = entries.iter().map(|e| json!({"path": e.rel, "kind": e.kind, "size": e.size, "modified_unix_ms": e.modified})).collect();
    let listed: Vec<String> = entries
        .iter()
        .map(|e| match e.kind {
            "dir" => format!("{}/", e.rel),
            "link" => format!("{} (link, not followed)", e.rel),
            _ => format!("{} ({} bytes)", e.rel, e.size),
        })
        .collect();
    // What is shown is what fits the view of the model (the entries are all in the data): the rest is counted and the way to narrow it is said (card OA5-03, CU-03).
    let fits = page::fit_lines(&listed, g.limits().view_chars);
    let text = listed[..fits].join("\n");
    let text = if fits < listed.len() { format!("{text}\n… {} more entries not shown; narrow with path, pattern or a smaller depth", listed.len() - fits) } else { text };
    let text = if truncated { format!("{text}\n… more entries not shown (raise limit or narrow the path)") } else { text };
    let text = if text.is_empty() { if st.left_out() > 0 { "(nothing to show here)".to_string() } else { "(empty folder)".to_string() } } else { text };
    // Only files count as matched, so a pattern that found folders or links only is not told it found nothing.
    let why_none = if entries.is_empty() { glob_note(&glob, &st) } else { String::new() };
    let text = format!("{text}{why_none}{}", footer_line(g, &st, false));
    out(json!({"path": display(&r.path), "entries": values, "truncated": truncated, "left_out": st.counts()}), text)
}

struct ListItem {
    rel: String,
    kind: &'static str,
    size: u64,
    modified: u64,
}

/// Folders before files at every level, then by name: the order of a listing read as a tree. A folder comes before what is inside it.
fn tree_order(a: &ListItem, b: &ListItem) -> std::cmp::Ordering {
    let (pa, pb): (Vec<&str>, Vec<&str>) = (a.rel.split('/').collect(), b.rel.split('/').collect());
    for i in 0..pa.len().min(pb.len()) {
        if pa[i] == pb[i] {
            continue;
        }
        let a_is_dir = i + 1 < pa.len() || a.kind == "dir";
        let b_is_dir = i + 1 < pb.len() || b.kind == "dir";
        return b_is_dir.cmp(&a_is_dir).then_with(|| pa[i].to_lowercase().cmp(&pb[i].to_lowercase())).then_with(|| pa[i].cmp(pb[i]));
    }
    pa.len().cmp(&pb.len())
}

fn find(g: &PathGuard, args: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
    let name = arg_str(args, "name");
    let text = arg_str(args, "text");
    if name.is_empty() && text.is_empty() {
        return Err(ToolError::new(ErrorCode::InvalidArguments, "give a name pattern, a text to search for, or both"));
    }
    let glob = glob_arg(name, "name")?;
    let order = match arg_str(args, "sort") {
        "" | "name" => Order::Breadth,
        "mtime" => Order::MtimeDesc,
        other => return Err(ToolError::new(ErrorCode::InvalidArguments, format!("sort must be \"name\" or \"mtime\", not \"{other}\""))),
    };
    let r = g.resolve(arg_str(args, "path"), Op::Read)?;
    let case = arg_bool(args, "case_sensitive");
    let limit = arg_u64(args, "limit", 50) as usize;
    let searching = !text.is_empty();
    let opts = WalkOpts {
        include_hidden: arg_bool(args, "include_hidden"),
        include_ignored: arg_bool(args, "include_ignored"),
        max_depth: arg_u64(args, "max_depth", 8) as usize,
        max_files: g.limits().find_max_files,
        // Only a search of contents reads files; a search of names does not count bytes.
        max_bytes: if searching { g.limits().find_max_scan_bytes } else { u64::MAX },
        skip_sensitive: searching,
        glob: glob.clone(),
        order,
        ..WalkOpts::default()
    };
    let needle = if case { text.to_string() } else { text.to_lowercase() };
    let mut hits: Vec<Value> = Vec::new();
    let st = walk::walk(g, &r.path, &opts, cancel, &mut |e| {
        if hits.len() >= limit {
            return Visit::Stop;
        }
        if !searching {
            hits.push(json!({"path": e.rel}));
            return if hits.len() >= limit { Visit::Stop } else { Visit::Searched };
        }
        find_text_in(g, e, &needle, case, limit, &mut hits)
    });
    let lines: Vec<String> = hits.iter().map(|h| match h["line"].as_u64() {
        Some(n) => format!("{}:{}: {}", h["path"].as_str().unwrap_or(""), n, h["snippet"].as_str().unwrap_or("")),
        None => h["path"].as_str().unwrap_or("").to_string(),
    }).collect();
    let mut note = String::new();
    if st.budget_hit {
        note.push_str("\n… search stopped at its size limit; narrow the path.");
    }
    if hits.len() >= limit {
        note.push_str("\n… result limit reached.");
    }
    note.push_str(&glob_note(&glob, &st));
    let fits = page::fit_lines(&lines, g.limits().view_chars);
    let more = if fits < lines.len() { format!("\n… {} more not shown; narrow with path or name", lines.len() - fits) } else { String::new() };
    let body = if lines.is_empty() { format!("No matches.{note}") } else { format!("{}{more}{note}", lines[..fits].join("\n")) };
    out(
        json!({"path": display(&r.path), "matches": hits, "files_scanned": st.searched, "truncated": st.budget_hit || hits.len() >= limit, "left_out": st.counts(), "stopped_at_limit": st.budget_hit}),
        format!("{body}{}", footer_line(g, &st, true)),
    )
}

/// How much of one line `fs.find` looks at for its text (what is past it is not searched).
const FIND_LINE_BYTES: usize = 64 * 1024;

/// The lines of one file that hold `needle` (already lower case unless `case`): never a file that is too big, binary or not where it was met.
fn find_text_in(g: &PathGuard, e: &WalkEntry<'_>, needle: &str, case: bool, limit: usize, hits: &mut Vec<Value>) -> Visit {
    if e.meta.len() > g.search_limit_for(e.path) {
        return Visit::Big;
    }
    let Ok(f) = std::fs::File::open(e.path) else { return Visit::Unreadable };
    if check_opened_at(&f, e.path).is_err() {
        return Visit::Unreadable;
    }
    let mut reader = BufReader::new(f);
    // The start of the file says whether it is text (what is buffered: 8 KiB), not its first line.
    match reader.fill_buf() {
        Ok(start) if looks_binary(start) => return Visit::Binary,
        Err(_) => return Visit::Unreadable,
        Ok(_) => {}
    }
    let mut line_no = 0u64;
    let mut buf = Vec::new();
    loop {
        // A line is kept up to a size: a file of one long line is not read into memory whole (card OA5-03, CU-12). The text searched for is looked for in what is kept.
        match page::read_line_bounded(&mut reader, &mut buf, FIND_LINE_BYTES) {
            Ok(None) | Err(_) => break,
            Ok(Some(_)) => {}
        }
        line_no += 1;
        let line = String::from_utf8_lossy(&buf);
        let hay = if case { line.to_string() } else { line.to_lowercase() };
        if hay.contains(needle) {
            hits.push(json!({"path": e.rel, "line": line_no, "snippet": truncate(line.trim_end(), 200)}));
            if hits.len() >= limit {
                return Visit::Stop;
            }
        }
    }
    Visit::Searched
}

// ---- fs.read ----------------------------------------------------------------------------------------------------------------

/// How much of a line of a file that looks like it holds secrets a read keeps: it shows the name and the length of each value, and the length of a value is what the line has.
const MASKED_LINE_BYTES: usize = 65_536;

/// What a read counts as it goes through a file: the bytes (hashed, when the file is small enough to have a version), the line breaks and the last byte, so that after the page the rest
/// of the file is passed through the same reader and the version and the number of lines are those of the bytes that were read (not of a second look at the file).
struct Scan<R: Read> {
    inner: BufReader<R>,
    stats: ScanStats,
}

struct ScanStats {
    hasher: Option<Sha256>,
    bytes: u64,
    newlines: u64,
    last: Option<u8>,
}

impl ScanStats {
    fn add(&mut self, data: &[u8]) {
        let Some(last) = data.last() else { return };
        self.bytes += data.len() as u64;
        self.newlines += data.iter().filter(|b| **b == b'\n').count() as u64;
        self.last = Some(*last);
        if let Some(h) = &mut self.hasher {
            h.update(data);
        }
    }
}

impl<R: Read> Read for Scan<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(out)?;
        self.stats.add(&out[..n]);
        Ok(n)
    }
}

impl<R: Read> BufRead for Scan<R> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.inner.fill_buf()
    }

    fn consume(&mut self, amt: usize) {
        let Scan { inner, stats } = self;
        let n = amt.min(inner.buffer().len());
        stats.add(&inner.buffer()[..n]);
        inner.consume(amt);
    }
}

/// One line of a page: its number, what is shown of it, and how many of its bytes are not shown.
struct PageLine {
    no: u64,
    text: String,
    lost: u64,
}

/// How many characters of numbered lines a page may have: what the call says (`max_chars`), else what the limits say the model is shown of a result, never below the least or above the
/// most a page may be.
fn page_budget(g: &PathGuard, args: &Value) -> usize {
    (arg_u64(args, "max_chars", g.limits().view_chars as u64) as usize).clamp(page::MIN_PAGE_CHARS, page::MAX_PAGE_CHARS)
}

/// The names in the folder of a file that is not there which look like its name, up to three, and only ones the file tools may read and show: a name the guard would refuse, or a file
/// that looks like it holds secrets, is not offered.
fn names_like(g: &PathGuard, path: &Path) -> Vec<String> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(parent) else { return Vec::new() };
    let siblings = entries.take(2000).filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string());
    page::similar_names(&name.to_string_lossy(), siblings, 12).into_iter().filter(|n| g.resolve(&display(&parent.join(n)), Op::Read).is_ok_and(|r| !r.sensitive)).take(3).collect()
}

/// The names in a folder that the file tools may show, folders marked with a `/`: the first `max` by name.
fn names_in(g: &PathGuard, dir: &Path, max: usize) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut all: Vec<(String, bool)> = entries.take(2000).filter_map(|e| e.ok()).map(|e| (e.file_name().to_string_lossy().to_string(), e.file_type().is_ok_and(|t| t.is_dir()))).collect();
    all.sort_by_key(|(n, _)| n.to_lowercase());
    all.into_iter().filter(|(n, _)| g.resolve(&display(&dir.join(n)), Op::Read).is_ok_and(|r| !r.sensitive)).take(max).map(|(n, dir)| if dir { format!("{n}/") } else { n }).collect()
}

/// The failure of a read of a path that is not there: it says so, and offers the names in the same folder that look like it, written as the path was (so that one can be copied).
fn read_failure(g: &PathGuard, r: &Resolved, asked: &str, e: std::io::Error) -> ToolError {
    if e.kind() != std::io::ErrorKind::NotFound {
        return io_err(e, &display(&r.path));
    }
    let mut message = format!("File not found: {}.", display(&r.path));
    let names = names_like(g, &r.path);
    if !names.is_empty() {
        let folder = asked.rfind(['/', '\\']).map(|i| &asked[..=i]).unwrap_or("");
        message.push_str(&format!(" Did you mean: {}?", names.iter().map(|n| format!("{folder}{n}")).collect::<Vec<_>>().join(", ")));
    }
    ToolError::new(ErrorCode::NotFound, message)
}

fn read(g: &PathGuard, args: &Value) -> Result<ProviderOutput, ToolError> {
    let asked = arg_str(args, "path");
    let r = g.resolve(asked, Op::Read)?;
    let offset = arg_u64(args, "offset_line", 1).max(1);
    let max_lines = arg_u64(args, "max_lines", 500).clamp(1, 2000);
    let max_bytes = arg_u64(args, "max_bytes", 65536).min(g.limits().read_max_bytes);
    let max_chars = page_budget(g, args);
    let shown_path = display(&r.path);
    let meta = std::fs::metadata(&r.path).map_err(|e| read_failure(g, &r, asked, e))?;
    if meta.is_dir() {
        let names = names_in(g, &r.path, 10);
        let holds = if names.is_empty() { String::new() } else { format!(" It holds: {}.", names.join(", ")) };
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{shown_path} is a folder; use fs.list.{holds}")));
    }
    let file_name = r.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if let Some(why) = page::refused_by_extension(&shown_path, &file_name) {
        return Err(ToolError::new(ErrorCode::InvalidArguments, why));
    }
    let mut file = open_checked(&r).map_err(|e| if e.code == ErrorCode::NotFound { read_failure(g, &r, asked, std::io::ErrorKind::NotFound.into()) } else { e })?;
    // The start of the file decides whether it is text (the first 8 KiB, not the first line), and a big file with no line break at all is not paged through.
    let mut sample = Vec::new();
    (&mut file).take(page::NO_BREAK_PROBE_BYTES as u64).read_to_end(&mut sample).map_err(|e| io_err(e, "read"))?;
    match page::sniff(&sample) {
        page::Sniff::Utf16 => return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{shown_path} is a UTF-16 text file; fs.read reads UTF-8 text only"))),
        page::Sniff::Binary => return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{shown_path} looks like a binary file; fs.read only reads text"))),
        page::Sniff::Text => {}
    }
    if meta.len() > g.limits().read_max_bytes.saturating_mul(page::LONG_FILE_FACTOR) && sample.len() >= page::NO_BREAK_PROBE_BYTES && !sample.contains(&b'\n') {
        return Err(ToolError::new(
            ErrorCode::InvalidArguments,
            format!("{shown_path} is {} bytes and has no line break in its first {} bytes: it looks minified or binary. Search it with fs.grep; reading it by line cannot work.", meta.len(), page::NO_BREAK_PROBE_BYTES),
        ));
    }
    file.seek(SeekFrom::Start(0)).map_err(|e| io_err(e, "read"))?;
    // The version is the SHA-256 of the whole file: made from the bytes this one pass reads, so that it is the version of what was shown (and `expect_version` of an edit takes it).
    let versioned = meta.len() <= g.limits().hash_max_bytes;
    let mut scan = Scan { inner: BufReader::new(file), stats: ScanStats { hasher: versioned.then(Sha256::new), bytes: 0, newlines: 0, last: None } };

    // A file whose name says it holds secrets shows its names and the length of each value, unless the person agreed to the real text (`reveal`, which the host asked about): card OA8-03, SE-08.
    let reveal = arg_bool(args, "reveal");
    let masked = r.sensitive && !reveal;
    let mut view = MaskedView::for_file(&file_name);
    let keep = if masked { MASKED_LINE_BYTES } else { page::MAX_LINE_BYTES.min(max_chars) };
    let mut lines: Vec<PageLine> = Vec::new();
    let (mut buf, mut line_no, mut chars_used, mut bytes_used) = (Vec::new(), 0u64, 0usize, 0u64);
    let (mut bom, mut more) = (false, false);
    loop {
        let Some(info) = page::read_line_bounded(&mut scan, &mut buf, keep).map_err(|e| io_err(e, "read"))? else { break };
        line_no += 1;
        // The line is longer than what was kept: the bytes after them are skipped, and so is the end of a character that the cut split.
        let cut_line = info.total > buf.len() as u64;
        let mut byte_order_mark = 0u64;
        if line_no == 1 && buf.starts_with(&[0xEF, 0xBB, 0xBF]) {
            buf.drain(..3);
            byte_order_mark = 3;
            bom = true;
        }
        if !cut_line {
            while buf.last() == Some(&b'\r') {
                buf.pop();
            }
        }
        // Every line goes through the view, also the ones above the first that is shown: a key can start there.
        if line_no < offset {
            if masked {
                view.line(&page::text_of_prefix(&buf, cut_line));
            }
            continue;
        }
        let mut text = page::text_of_prefix(&buf, cut_line);
        let lost = if cut_line { info.total.saturating_sub(text.len() as u64 + byte_order_mark) } else { 0 };
        if masked {
            text = view.line(&text);
        }
        if lost > 0 {
            text.push_str(&format!(" … (line truncated, {lost} more bytes)"));
        }
        let cost = line_no.to_string().len() + 2 + text.chars().count();
        // A page is as many whole lines as fit its budgets; the first line is always shown, however long it is, so that a read always goes on.
        if !lines.is_empty() && (lines.len() as u64 >= max_lines || bytes_used + info.total + 1 > max_bytes || chars_used + cost > max_chars) {
            more = true;
            break;
        }
        bytes_used += info.total + 1;
        chars_used += cost;
        lines.push(PageLine { no: line_no, text, lost });
    }
    // The rest of the file goes through the same reader, to the end of the version and of the count of lines (a file too big to have a version is not read on).
    let mut counted = versioned;
    if versioned {
        loop {
            let n = scan.fill_buf().map_err(|e| io_err(e, "read"))?.len();
            if n == 0 {
                break;
            }
            scan.consume(n);
            if scan.stats.bytes > g.limits().hash_max_bytes {
                counted = false;
                break;
            }
        }
    }
    let stats = scan.stats;
    let version = if counted { stats.hasher.map(|h| h.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()) } else { None };
    let total_lines = counted.then(|| if stats.bytes == 0 { 0 } else { stats.newlines + u64::from(stats.last != Some(b'\n')) });

    if lines.is_empty() {
        if line_no == 0 && offset == 1 {
            let tail = version.as_deref().map(|v| format!("; version {v}")).unwrap_or_default();
            return out(
                json!({"path": shown_path, "content": "", "first_line": 1, "last_line": 0, "lines": 0, "total_lines": 0, "next_offset": null, "truncated": false, "complete": true, "version": version, "size": meta.len(), "bom": bom, "lines_cut": 0, "masked": masked, "revealed": r.sensitive && reveal}),
                format!("(empty file); 0 bytes{tail}"),
            );
        }
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("offset_line {offset} is past the end of {shown_path} ({line_no} lines)")));
    }
    let last = lines.last().map(|l| l.no).unwrap_or(offset);
    let cut = lines.iter().filter(|l| l.lost > 0).count();
    let complete = offset == 1 && !more && cut == 0;
    let status = if complete {
        page::Status::Whole
    } else if more {
        page::Status::Continue(last + 1)
    } else {
        page::Status::End
    };
    let tail = format!("; {} bytes{}", meta.len(), match &version { Some(v) => format!("; version {v}"), None => "; too large to version".to_string() });
    let header = page::Header { first: lines[0].no, last, total: total_lines, status, tail };
    let numbered = lines.iter().map(|l| format!("{}\t{}", l.no, l.text)).collect::<Vec<_>>().join("\n");
    let raw = lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
    let mut text = format!("{}\n{numbered}", header.render());
    if masked {
        text.push_str("\n(The values in this file are hidden: the names and the length of each value are shown. To see the real values call fs.read again with reveal=true; the person is asked every time.)");
    }
    out(
        json!({
            "path": shown_path, "content": raw, "first_line": offset, "last_line": last, "lines": lines.len(), "total_lines": total_lines,
            "next_offset": if more { json!(last + 1) } else { Value::Null }, "truncated": more, "complete": complete, "version": version, "size": meta.len(),
            "bom": bom, "lines_cut": cut, "masked": masked, "revealed": r.sensitive && reveal,
        }),
        text,
    )
}

/// The largest file an outline is made of.
const OUTLINE_MAX_BYTES: u64 = 1 << 20;

/// The declarations of a source file and the lines each one spans, in about a tenth of the characters of the file (card OA5-03, CX-17). Read through the same guard as `fs.read`.
fn outline(g: &PathGuard, args: &Value) -> Result<ProviderOutput, ToolError> {
    let asked = arg_str(args, "path");
    let r = g.resolve(asked, Op::Read)?;
    let shown_path = display(&r.path);
    let meta = std::fs::metadata(&r.path).map_err(|e| read_failure(g, &r, asked, e))?;
    if meta.is_dir() {
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{shown_path} is a folder; use fs.list")));
    }
    if r.sensitive {
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{shown_path} looks like it holds secrets: it has no outline; fs.read shows it with its values hidden")));
    }
    if meta.len() > OUTLINE_MAX_BYTES {
        return Err(ToolError::new(ErrorCode::ResourceLimit, format!("{shown_path} is {} bytes: too large for an outline (the most is {OUTLINE_MAX_BYTES}); search it with fs.grep", meta.len())));
    }
    let file_name = r.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if let Some(why) = page::refused_by_extension(&shown_path, &file_name) {
        return Err(ToolError::new(ErrorCode::InvalidArguments, why));
    }
    let mut bytes = Vec::new();
    open_checked(&r)?.take(OUTLINE_MAX_BYTES + 1).read_to_end(&mut bytes).map_err(|e| io_err(e, "read"))?;
    match page::sniff(&bytes) {
        page::Sniff::Utf16 => return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{shown_path} is a UTF-16 text file; fs.outline reads UTF-8 text only"))),
        page::Sniff::Binary => return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{shown_path} looks like a binary file; fs.outline only reads text"))),
        page::Sniff::Text => {}
    }
    let found = super::outline::outline(&file_name, &String::from_utf8_lossy(&bytes));
    let max_items = (arg_u64(args, "max_items", super::outline::MAX_ITEMS as u64) as usize).clamp(1, super::outline::MAX_ITEMS);
    let items: Vec<Value> = found.items.iter().map(|i| json!({"kind": i.kind, "name": i.name, "start": i.start, "end": i.end, "depth": i.depth})).collect();
    out(
        json!({"path": shown_path, "language": found.language, "lines": found.total_lines, "declarations": found.items.len(), "items": items, "size": meta.len()}),
        found.render(max_items, g.limits().view_chars),
    )
}

fn stat(g: &PathGuard, args: &Value) -> Result<ProviderOutput, ToolError> {
    let r = g.resolve(arg_str(args, "path"), Op::Read)?;
    let m = std::fs::metadata(&r.path).map_err(|e| io_err(e, &display(&r.path)))?;
    // The size of a folder is of what the file tools may see in it: Aqua's own files and credential folders are not counted.
    let mut data = json!({"path": display(&r.path), "kind": kind_of(&m), "size": if m.is_dir() { walk::guarded_dir_size(g, &r.path, &AtomicBool::new(false)).0 } else { m.len() }, "readonly": m.permissions().readonly(), "modified_unix_ms": modified_ms(&m)});
    if arg_bool(args, "hash") && m.is_file() {
        if m.len() > g.limits().hash_max_bytes {
            return Err(ToolError::new(ErrorCode::ResourceLimit, format!("file is larger than the {} byte hash limit", g.limits().hash_max_bytes)));
        }
        data["sha256"] = json!(sha256_file(&r.path).map_err(|e| io_err(e, "hash"))?);
    }
    let text = format!("{} {} bytes{}", data["kind"].as_str().unwrap_or(""), data["size"], data.get("sha256").and_then(|h| h.as_str()).map(|h| format!(", sha256 {h}")).unwrap_or_default());
    out(data, text)
}

fn write(g: &PathGuard, trash: &Trash, args: &Value) -> Result<ProviderOutput, ToolError> {
    let r = g.resolve(arg_str(args, "path"), Op::Write)?;
    // One writer at a time for this file: a second call reads what the first wrote (card OA6-01, ED-06).
    let _held = locks::lock(&[r.path.as_path()])?;
    let make_parents = args.get("make_parents").and_then(|v| v.as_bool()).unwrap_or(true);
    let mode = match arg_str(args, "mode") { "" => "create", m => m };
    if let Some(parent) = r.path.parent() {
        if !parent.exists() {
            if !make_parents {
                return Err(ToolError::new(ErrorCode::NotFound, format!("the folder {} does not exist; set make_parents=true to create it", display(parent))));
            }
            std::fs::create_dir_all(parent).map_err(|e| io_err(e, "create folders"))?;
        }
    }
    if arg_str(args, "kind") == "dir" {
        if r.path.exists() {
            return if r.path.is_dir() { out(json!({"path": display(&r.path), "created": false}), format!("{} already exists", display(&r.path))) } else { Err(ToolError::new(ErrorCode::AlreadyExists, format!("{} exists and is a file", display(&r.path)))) };
        }
        std::fs::create_dir(&r.path).map_err(|e| io_err(e, "create folder"))?;
        trash.note_created(&r.path);
        return out(json!({"path": display(&r.path), "created": true}), format!("Created folder {}", display(&r.path)));
    }
    let content = arg_str(args, "content");
    if content.len() as u64 > g.limits().write_max_bytes {
        return Err(ToolError::new(ErrorCode::ResourceLimit, format!("content is larger than the {} byte write limit", g.limits().write_max_bytes)));
    }
    let exists = r.path.exists();
    if exists && r.path.is_dir() {
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{} is a folder", display(&r.path))));
    }
    let mut undo_id = None;
    // The text the file had before an overwrite, for the diff in the result (card OA6-04).
    let mut old_text: Option<String> = None;
    // What the syntax gate says of an overwrite (card OA6-03): a note after the write. A file that is made, or added to, is not judged: a long file is written in parts.
    let mut syntax_note: Option<String> = None;
    // The name is taken only if it is free at the moment of the rename: a call that lost the race to create it is told so.
    let taken = |e: std::io::Error| if e.kind() == std::io::ErrorKind::AlreadyExists { ToolError::new(ErrorCode::AlreadyExists, format!("{} already exists; use mode=overwrite to replace it", display(&r.path))) } else { write_err(e, &r.path, "write") };
    match mode {
        "create" => {
            if exists {
                return Err(ToolError::new(ErrorCode::AlreadyExists, format!("{} already exists; use mode=overwrite to replace it", display(&r.path))));
            }
            write_atomic_expecting(&r.path, content.as_bytes(), Expect::Absent).map_err(taken)?;
        }
        "overwrite" => {
            if exists {
                refuse_read_only(&r.path)?;
                // What the file is now: the version the caller named must be it, and it is what the new content replaces, so a change made behind this call's back stops the write.
                let meta_len = std::fs::metadata(&r.path).map(|m| m.len()).unwrap_or(0);
                let before = if meta_len <= g.limits().hash_max_bytes { Some(sha256_file(&r.path).map_err(|e| io_err(e, "hash"))?) } else { None };
                // The file the person was shown is the file this call replaces (card OA6-04, SE-13).
                if let Some(h) = &before {
                    check_shown(&r.path, h)?;
                }
                if !r.sensitive {
                    old_text = read_for_preview(g, &r).map(|(_, text)| text);
                }
                if let Some(v) = args.get("expect_version").and_then(|v| v.as_str()) {
                    let now = match &before {
                        Some(h) => h.clone(),
                        None => sha256_file(&r.path).map_err(|e| io_err(e, "hash"))?,
                    };
                    if now != v {
                        return Err(ToolError::new(ErrorCode::Conflict, "the file changed since you read it (version mismatch); read it again"));
                    }
                }
                // The syntax gate (card OA6-03): the new content is judged against what the file holds now, before anything is written.
                if let Some(old) = old_text.as_deref() {
                    match syntax_verdict(&r.path, old, content) {
                        super::ast::Verdict::Refuse(why) => return Err(syntax_refusal(&r.path, &why)),
                        super::ast::Verdict::Note(note) => syntax_note = Some(note),
                        super::ast::Verdict::Fine => {}
                    }
                }
                let backup = trash.backup(&r.path, "overwritten").map_err(|e| ToolError::new(ErrorCode::BackendError, e))?;
                undo_id = Some(backup.undo_id.clone());
                let expect = before.as_deref().map_or(Expect::Any, Expect::Unchanged);
                if let Err(e) = write_atomic_expecting(&r.path, content.as_bytes(), expect) {
                    if is_changed_underneath(&e) {
                        trash.drop_entry(&backup.undo_id);
                    }
                    return Err(write_err(e, &r.path, "write"));
                }
                // What the file is now, so that the same content saved again later is not taken for somebody else's change.
                trash.record_after(&backup, &sha256_hex(content.as_bytes()));
            } else {
                check_shown(&r.path, "absent")?;
                write_atomic(&r.path, content.as_bytes()).map_err(|e| write_err(e, &r.path, "write"))?;
            }
        }
        "append" => {
            let existing = if exists { std::fs::metadata(&r.path).map(|m| m.len()).unwrap_or(0) } else { 0 };
            if existing + content.len() as u64 > g.limits().write_max_bytes * 8 {
                return Err(ToolError::new(ErrorCode::ResourceLimit, "the file would become too large"));
            }
            if exists {
                refuse_read_only(&r.path)?;
            }
            let mut f = open_append_checked(&r)?;
            f.write_all(content.as_bytes()).map_err(|e| io_err(e, "append"))?;
        }
        other => return Err(ToolError::new(ErrorCode::InvalidArguments, format!("unknown mode '{other}'"))),
    }
    if !exists {
        // A file that was not there holds exactly what was given: its hash is kept, to tell later whether anybody changed it.
        trash.note_created_with_hash(&r.path, &sha256_hex(content.as_bytes()));
    }
    let now = read_checked(&r).unwrap_or_default();
    let version = sha256_hex(&now);
    let total_lines = count_lines(&now);
    let undo_note = undo_id.as_ref().map(|u| format!(" The previous content is kept as undo entry {u}.")).unwrap_or_default();
    // A long file is written in parts (the planner's reply is short): after each part the model is told how long the file is, so that it knows what has been written.
    let text = if mode == "append" { format!("Appended {} bytes to {}; the file now has {total_lines} lines.", content.len(), display(&r.path)) } else { format!("Wrote {} bytes to {}.{undo_note}", content.len(), display(&r.path)) };
    // What the write did, as a diff (card OA6-04): the first lines of a file that was made, the lines that were added at the end, or the lines an overwrite changed.
    let name = display(&r.path);
    let new_text = plain(content);
    let (op, made) = match (mode, exists) {
        ("append", _) => ("append", Some(diff::diff_with(&name, &format!("{name} (added at the end)"), "", &new_text, 0, RESULT_DIFF_LINES, &diff::Supplied::everything()))),
        (_, false) => ("create", Some(diff::created(&name, &new_text, RESULT_DIFF_LINES))),
        _ => ("update", old_text.as_deref().map(|old| diff::diff_with(&name, &name, &plain(old), &new_text, PREVIEW_CONTEXT, RESULT_DIFF_LINES, &diff::Supplied::everything()))),
    };
    let mut data = json!({"path": name, "bytes": content.len(), "created": !exists, "undo_id": undo_id, "version": version, "total_lines": total_lines, "total_bytes": now.len(), "changes": [change_entry(&name, op, r.sensitive, made.as_ref())]});
    let text = match &syntax_note {
        Some(note) => {
            data["syntax_note"] = json!(note);
            format!("{text}\nNote: {note}")
        }
        None => text,
    };
    out(data, text)
}

fn edit(g: &PathGuard, trash: &Trash, args: &Value) -> Result<ProviderOutput, ToolError> {
    let r = g.resolve(arg_str(args, "path"), Op::Write)?;
    // One writer at a time for this file: a second edit reads what the first wrote (card OA6-01, ED-06).
    let _held = locks::lock(&[r.path.as_path()])?;
    let meta = std::fs::metadata(&r.path).map_err(|e| io_err(e, &display(&r.path)))?;
    if !meta.is_file() {
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{} is not a file", display(&r.path))));
    }
    if meta.len() > g.limits().edit_max_file_bytes {
        return Err(ToolError::new(ErrorCode::ResourceLimit, "the file is too large to edit; use fs.write with the full content"));
    }
    if let Some(v) = args.get("expect_version").and_then(|v| v.as_str()).filter(|v| !v.is_empty()) {
        let now = sha256_file(&r.path).map_err(|e| io_err(e, "hash"))?;
        if now != v {
            return Err(ToolError::new(ErrorCode::Conflict, "ERROR [FILE_CHANGED] the file changed since that version; read it again"));
        }
    }
    refuse_read_only(&r.path)?;
    let bytes = read_checked(&r)?;
    if looks_binary(&bytes[..bytes.len().min(8192)]) {
        return Err(ToolError::new(ErrorCode::InvalidArguments, "binary files cannot be edited"));
    }
    let before = sha256_hex(&bytes);
    // The file the person was shown is the file this call changes (card OA6-04, SE-13).
    check_shown(&r.path, &before)?;
    let text = String::from_utf8(bytes).map_err(|_| ToolError::new(ErrorCode::InvalidArguments, "the file is not valid UTF-8"))?;
    // The text is worked on with its line breaks read as LF and written back with the ones the file had, and a byte order mark is kept (card OA6-01, ED-02, ED-03).
    let mut doc = super::patch::Doc::parse(&text);
    let before_text = doc.text().to_string();
    let edit = edit_of(args);
    let applied = super::patch::apply_edit(&mut doc, &edit).map_err(|f| {
        let code = match f.kind {
            super::patch::FailKind::Invalid => ErrorCode::InvalidArguments,
            super::patch::FailKind::NotFound => ErrorCode::NotFound,
            super::patch::FailKind::Conflict => ErrorCode::Conflict,
        };
        edit_failure(code, f.message, r.trusted && !r.sensitive)
    })?;
    let updated = doc.render();
    // The syntax gate (card OA6-03): an edit that breaks a JSON file that was valid is not made (a dry run says so too), and one that unbalances brackets or quotes is made and noted.
    let syntax_note = match syntax_verdict(&r.path, &before_text, doc.text()) {
        super::ast::Verdict::Refuse(why) => return Err(syntax_refusal(&r.path, &why)),
        super::ast::Verdict::Note(note) => Some(note),
        super::ast::Verdict::Fine => None,
    };
    // What the edit does to the file, as a diff (card OA6-04): in the answer of a dry run and in the data of the result. The model reads the diff of what it wrote and the counts; the lines of
    // the file are in the question the person was asked and nowhere in the answer (follow-up of the security review).
    let name = display(&r.path);
    let mine = supplied_by_edit(&edit);
    let changed = diff::diff_with(&name, &name, &before_text, doc.text(), PREVIEW_CONTEXT, if arg_bool(args, "dry_run") { PREVIEW_LINES } else { RESULT_DIFF_LINES }, &mine);
    if arg_bool(args, "dry_run") {
        let how = if applied.kind == super::patch::MatchKind::Exact { String::new() } else { format!(" {}", applied.kind.how()) };
        return out(
            json!({
                "dry_run": true, "path": name, "replacements": applied.count, "match_kind": applied.kind.as_str(), "trimmed_match": applied.kind != super::patch::MatchKind::Exact,
                "lines": [applied.first_line, applied.last_line], "eol": doc.eol_label(), "bom": doc.bom(), "version": before,
                "changes": [change_entry(&name, "update", r.sensitive, Some(&changed))],
            }),
            format!(
                "Dry run: the edit applies cleanly to {} place(s) in {name}{how} (+{} -{} lines). Nothing was written.\n{}{}",
                applied.count,
                changed.added,
                changed.removed,
                if r.sensitive { "(the lines are not shown: this file looks like it holds secrets)" } else { changed.model_text.as_str() },
                syntax_note.as_deref().map(|n| format!("\nNote: {n}")).unwrap_or_default()
            ),
        );
    }
    let undo = trash.backup(&r.path, "edited").map_err(|e| ToolError::new(ErrorCode::BackendError, e))?;
    if let Err(e) = write_atomic_expecting(&r.path, updated.as_bytes(), Expect::Unchanged(&before)) {
        if is_changed_underneath(&e) {
            trash.drop_entry(&undo.undo_id);
        }
        return Err(write_err(e, &r.path, "write"));
    }
    let version = sha256_hex(updated.as_bytes());
    trash.record_after(&undo, &version);
    let how = if applied.kind == super::patch::MatchKind::Exact { String::new() } else { format!(" {}", applied.kind.how()) };
    // What the model reads of the lines it changed: its own lines, and not the lines around them (they are the file's). Nothing of a file that looks like it holds secrets.
    let snippet = if r.sensitive { "(the lines are not shown: this file looks like it holds secrets)".to_string() } else { doc.own_snippet(applied.first_line, applied.last_line, &mine) };
    let mut data = json!({
        "path": display(&r.path), "replacements": applied.count, "match_kind": applied.kind.as_str(), "trimmed_match": applied.kind != super::patch::MatchKind::Exact,
        "lines": [applied.first_line, applied.last_line], "eol": doc.eol_label(), "bom": doc.bom(), "undo_id": undo.undo_id, "version": version,
        "changes": [change_entry(&name, "update", r.sensitive, Some(&changed))],
    });
    let mut text = format!("Replaced {} occurrence(s) in {}{how}. The previous version is undo entry {}.\n{snippet}", applied.count, display(&r.path), undo.undo_id);
    if let Some(note) = &syntax_note {
        data["syntax_note"] = json!(note);
        text.push_str(&format!("\nNote: {note}"));
    }
    out(data, text)
}

// ---- fs.grep ----------------------------------------------------------------------------------------------------------------

#[derive(Default)]
struct GrepAcc {
    matches: Vec<Value>,
    lines: Vec<String>,
}

fn grep(g: &PathGuard, args: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
    let pattern = arg_str(args, "pattern");
    if pattern.is_empty() || pattern.len() > 500 {
        return Err(ToolError::new(ErrorCode::InvalidArguments, "give a pattern of 1 to 500 characters"));
    }
    let literal = arg_bool(args, "fixed_string");
    let source = if literal { regex::escape(pattern) } else { pattern.to_string() };
    // Linear-time matching (the `regex` crate has no backtracking), and a size limit on what a pattern may compile to.
    let re = regex::RegexBuilder::new(&source)
        .case_insensitive(arg_bool(args, "ignore_case"))
        .size_limit(1 << 20)
        .build()
        .map_err(|e| ToolError::new(ErrorCode::InvalidArguments, format!("the pattern is not a valid regular expression ({}); use fixed_string=true to search for the text as it is", e.to_string().lines().last().unwrap_or(""))))?;
    let glob = glob_arg(arg_str(args, "glob"), "glob")?;
    let raw = match arg_str(args, "path") {
        "" => "@ws",
        p => p,
    };
    let r = g.resolve(raw, Op::Read)?;
    let context = arg_u64(args, "context", 0).min(5) as usize;
    let limit = arg_u64(args, "max_results", 50).clamp(1, 200) as usize;
    let mut acc = GrepAcc::default();
    let c = GrepCtx { re: &re, context, limit };
    let meta = std::fs::metadata(&r.path).map_err(|e| io_err(e, &display(&r.path)))?;
    let st = if meta.is_file() {
        // A file named by its path: no ignore file applies (it was asked for), the size, binary and secret-name checks do.
        let name = r.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut st = WalkStats { visited: 1, matched: 1, ..WalkStats::default() };
        let visit = if g.is_sensitive_name(&name) && !g.in_out_root(&r.path) {
            st.skipped_sensitive += 1;
            None
        } else {
            Some(grep_one(g, &r.path, &name, &meta, &c, &mut acc))
        };
        match visit {
            Some(Visit::Searched) | Some(Visit::Stop) => {
                st.searched = 1;
                st.bytes = meta.len();
            }
            Some(Visit::Big) => st.skipped_big += 1,
            Some(Visit::Binary) => st.skipped_binary += 1,
            Some(Visit::Unreadable) => st.unreadable += 1,
            None => {}
        }
        st
    } else {
        let opts = WalkOpts {
            include_hidden: arg_bool(args, "include_hidden"),
            include_ignored: arg_bool(args, "include_ignored"),
            max_depth: 12,
            max_files: g.limits().find_max_files,
            max_bytes: g.limits().find_max_scan_bytes,
            skip_sensitive: true,
            glob: glob.clone(),
            ..WalkOpts::default()
        };
        walk::walk(g, &r.path, &opts, cancel, &mut |e| grep_one(g, e.path, e.rel, e.meta, &c, &mut acc))
    };
    let mut note = String::new();
    if st.budget_hit {
        note.push_str("\n… search stopped at its size limit; narrow the path or the glob.");
    }
    if acc.matches.len() >= limit {
        note.push_str("\n… result limit reached.");
    }
    note.push_str(&glob_note(&glob, &st));
    // First what was found and in how many files, then the lines that fit what the model is shown (every match is in the data), then how to narrow it (card OA5-03, CU-03).
    let text = if acc.lines.is_empty() {
        format!("No matches.{note}")
    } else {
        let files = acc.matches.iter().filter_map(|m| m["path"].as_str()).collect::<std::collections::HashSet<_>>().len();
        let count = acc.matches.len();
        let fits = page::fit_lines(&acc.lines, g.limits().view_chars);
        let more = if fits < acc.lines.len() { format!("\n… {} more lines not shown; narrow with path or glob", acc.lines.len() - fits) } else { String::new() };
        format!("{count} match{} in {files} file{}\n{}{more}{note}", if count == 1 { "" } else { "es" }, if files == 1 { "" } else { "s" }, acc.lines[..fits].join("\n"))
    };
    out(
        json!({"path": display(&r.path), "matches": acc.matches, "files_scanned": st.searched, "truncated": st.budget_hit || acc.matches.len() >= limit, "left_out": st.counts(), "stopped_at_limit": st.budget_hit}),
        format!("{text}{}", footer_line(g, &st, true)),
    )
}

struct GrepCtx<'a> {
    re: &'a regex::Regex,
    context: usize,
    limit: usize,
}

/// The matching lines of one file (`rel` is how it is named in the result): not a file that is too big, binary, not readable, or not where it was met.
fn grep_one(g: &PathGuard, path: &Path, rel: &str, meta: &std::fs::Metadata, c: &GrepCtx<'_>, acc: &mut GrepAcc) -> Visit {
    let max = g.search_limit_for(path);
    // A file of the folder of kept tool output may be bigger than the read limit: `search_limit_for`.
    if meta.len() > max {
        return Visit::Big;
    }
    let Ok(mut f) = std::fs::File::open(path) else { return Visit::Unreadable };
    if check_opened_at(&f, path).is_err() {
        return Visit::Unreadable;
    }
    // Read from the handle that was checked, no more than the limit (a file that grew since the size was read is too big).
    let mut bytes = Vec::with_capacity(meta.len().min(max) as usize);
    if (&mut f).take(max + 1).read_to_end(&mut bytes).is_err() {
        return Visit::Unreadable;
    }
    if bytes.len() as u64 > max {
        return Visit::Big;
    }
    if looks_binary(&bytes[..bytes.len().min(8192)]) {
        return Visit::Binary;
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let mut last_printed: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if !c.re.is_match(line) {
            continue;
        }
        if acc.matches.len() >= c.limit {
            return Visit::Stop;
        }
        let from = i.saturating_sub(c.context);
        let to = (i + c.context + 1).min(lines.len());
        // A gap between this group and the last one in the same file is marked, as grep does.
        if c.context > 0 && last_printed.is_some_and(|l| from > l + 1) {
            acc.lines.push("--".into());
        }
        for (k, l) in lines.iter().enumerate().take(to).skip(from.max(last_printed.map_or(0, |l| l + 1))) {
            if k == i {
                acc.lines.push(format!("{rel}:{}: {}", k + 1, truncate(l, 240)));
            } else {
                acc.lines.push(format!("{rel}-{}- {}", k + 1, truncate(l, 240)));
            }
        }
        last_printed = Some(to - 1);
        acc.matches.push(json!({"path": rel, "line": i + 1, "text": truncate(line.trim_end(), 240)}));
    }
    if acc.matches.len() >= c.limit {
        Visit::Stop
    } else {
        Visit::Searched
    }
}

// ---- fs.patch ---------------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum PatchOp {
    Update,
    Create,
    Delete,
}

#[cfg(test)]
thread_local! {
    /// A test's way to change the world between the plan of a patch and its write (the thread of the test is the thread that runs the patch).
    static AFTER_PLAN: std::cell::RefCell<Option<Box<dyn Fn()>>> = const { std::cell::RefCell::new(None) };
}

/// One file a patch changes: what it was, what it becomes.
struct Planned {
    resolved: Resolved,
    original: String,
    updated: String,
    op: PatchOp,
    /// SHA-256 of the file as the plan read it (empty for a file the patch creates): the write puts the new content in place only while the file still has it.
    original_sha: String,
    /// `"lf"`, `"crlf"` or `"mixed"` for an edit-list patch (what the file had).
    eol: &'static str,
}

/// What the plan of a patch found out besides the new content.
struct Plan {
    files: Vec<Planned>,
    /// Things that did not stop the patch (a hunk header that counts other lines than the body has).
    warnings: Vec<String>,
}

fn diff_candidates(raw: &str) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(rest) = raw.strip_prefix("a/").or_else(|| raw.strip_prefix("b/")) {
        v.push(rest.to_string());
    }
    v.push(raw.to_string());
    v
}

/// The files a patch names, resolved for writing (the same guard and protected paths as `fs.edit`). A path in a diff may carry git's `a/` or
/// `b/` prefix: whichever form is a file that exists is the one used.
fn patch_targets(g: &PathGuard, args: &Value) -> Result<Vec<(Resolved, PatchOp)>, ToolError> {
    let edits = args.get("edits").and_then(|v| v.as_array());
    let diff = arg_str(args, "diff");
    if edits.is_some() == !diff.is_empty() {
        return Err(ToolError::new(ErrorCode::InvalidArguments, "give either edits or a diff, not both and not neither"));
    }
    let mut raw: Vec<(String, PatchOp)> = Vec::new();
    if let Some(edits) = edits {
        for e in edits {
            let p = e.get("path").and_then(|p| p.as_str()).unwrap_or("");
            if !raw.iter().any(|r| r.0 == p) {
                raw.push((p.to_string(), PatchOp::Update));
            }
        }
    } else {
        let files = super::patch::parse_unified(diff).map_err(|e| ToolError::new(ErrorCode::InvalidArguments, e))?;
        for f in &files {
            let bare = |p: &str| p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p).to_string();
            if !f.creates_file() && !f.deletes_file() && bare(&f.old_path) != bare(&f.new_path) {
                return Err(ToolError::new(ErrorCode::InvalidArguments, format!("a diff that renames a file ({} to {}) is not supported: move the file with fs.move, then patch it", f.old_path, f.new_path)));
            }
            let op = if f.creates_file() { PatchOp::Create } else if f.deletes_file() { PatchOp::Delete } else { PatchOp::Update };
            let mut found = None;
            let mut first_err = None;
            for cand in diff_candidates(f.target()) {
                match g.resolve(&cand, Op::Write) {
                    Ok(r) if op == PatchOp::Create && !r.path.exists() => {
                        found = Some(cand);
                        break;
                    }
                    Ok(r) if op != PatchOp::Create && r.path.is_file() => {
                        found = Some(cand);
                        break;
                    }
                    Ok(r) if op == PatchOp::Create && r.path.exists() => {
                        return Err(ToolError::new(ErrorCode::AlreadyExists, format!("{} already exists", display(&r.path))));
                    }
                    Ok(_) => {}
                    Err(e) => first_err = first_err.or(Some(e)),
                }
            }
            match (found, first_err) {
                (Some(p), _) => {
                    if !raw.iter().any(|r| r.0 == p) {
                        raw.push((p, op));
                    }
                }
                (None, Some(e)) => return Err(e),
                (None, None) => return Err(ToolError::new(ErrorCode::NotFound, format!("{} does not exist", f.target()))),
            }
        }
    }
    if raw.len() > 20 {
        return Err(ToolError::new(ErrorCode::ResourceLimit, "a patch may change at most 20 files"));
    }
    raw.iter().map(|(p, op)| g.resolve(p, Op::Write).map(|r| (r, *op))).collect()
}

fn read_text_for_patch(g: &PathGuard, r: &Resolved) -> Result<String, ToolError> {
    let meta = std::fs::metadata(&r.path).map_err(|e| io_err(e, &display(&r.path)))?;
    if !meta.is_file() {
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{} is not a file", display(&r.path))));
    }
    if meta.len() > g.limits().edit_max_file_bytes {
        return Err(ToolError::new(ErrorCode::ResourceLimit, format!("{} is too large to patch; use fs.write with the full content", display(&r.path))));
    }
    let bytes = read_checked(r)?;
    if looks_binary(&bytes[..bytes.len().min(8192)]) {
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{} is a binary file and cannot be patched", display(&r.path))));
    }
    String::from_utf8(bytes).map_err(|_| ToolError::new(ErrorCode::InvalidArguments, format!("{} is not valid UTF-8", display(&r.path))))
}

/// Applies the whole patch in memory. Every file is tried, and every problem is reported together, so one round fixes them all.
fn patch_plan(g: &PathGuard, args: &Value, targets: Vec<(Resolved, PatchOp)>) -> Result<Plan, ToolError> {
    let mut planned = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let edits = args.get("edits").and_then(|v| v.as_array());
    let diff_files = if edits.is_none() { Some(super::patch::parse_unified(arg_str(args, "diff")).map_err(|e| ToolError::new(ErrorCode::InvalidArguments, e))?) } else { None };
    for f in diff_files.iter().flatten() {
        warnings.extend(f.warnings.iter().cloned());
    }
    for (r, op) in targets {
        let label = display(&r.path);
        let original = if op == PatchOp::Create {
            String::new()
        } else {
            match read_text_for_patch(g, &r) {
                Ok(t) => t,
                Err(e) => {
                    problems.push(e.message.clone());
                    continue;
                }
            }
        };
        if op != PatchOp::Create {
            if let Err(e) = refuse_read_only(&r.path) {
                problems.push(e.message);
                continue;
            }
        }
        let original_sha = if op == PatchOp::Create { String::new() } else { sha256_hex(original.as_bytes()) };
        let mut eol = "lf";
        let updated = if let Some(edits) = edits {
            let mine: Vec<super::patch::Edit> = edits
                .iter()
                .filter(|e| g.resolve(e.get("path").and_then(|p| p.as_str()).unwrap_or(""), Op::Write).is_ok_and(|x| x.path == r.path))
                .map(|e| super::patch::Edit {
                    old: e.get("old").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    new: e.get("new").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    replace_all: e.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false),
                    expect_count: e.get("expect_count").and_then(|v| v.as_u64()).map(|n| n as usize),
                })
                .collect();
            super::patch::apply_edits_detailed(&original, &mine, &label).map(|o| {
                eol = o.eol;
                o.text
            })
        } else {
            // The hunks of every `---`/`+++` block that names this file, in order.
            let mut hunks = Vec::new();
            for f in diff_files.as_ref().into_iter().flatten() {
                let same = diff_candidates(f.target()).iter().any(|c| g.resolve(c, Op::Write).is_ok_and(|x| x.path == r.path));
                if same {
                    hunks.extend(f.hunks.iter().cloned());
                }
            }
            super::patch::apply_hunks(&original, &hunks, &label)
        };
        match updated {
            Ok(updated) => {
                // The syntax gate (card OA6-03, `AQUA_SYNTAX_GATE`): a change that breaks a JSON file that was valid stops the whole patch (nothing is written, as for any other problem), and a change
                // that leaves brackets or quotes unbalanced is a warning in the answer.
                match syntax_verdict(&r.path, &original, &updated) {
                    super::ast::Verdict::Refuse(why) => {
                        problems.push(format!("{label}: {why}"));
                        continue;
                    }
                    super::ast::Verdict::Note(note) => warnings.push(note),
                    super::ast::Verdict::Fine => {}
                }
                planned.push(Planned { resolved: r, original, updated, op, original_sha, eol })
            }
            Err(e) => problems.push(e),
        }
    }
    if problems.is_empty() {
        Ok(Plan { files: planned, warnings })
    } else {
        let holds = problems.iter().any(|p| super::patch::holds_file_text(p));
        let e = ToolError::new(ErrorCode::Conflict, format!("nothing was changed: {}", problems.join(" | ")));
        Err(if holds { e.holding_outside_text() } else { e })
    }
}

/// How many lines a change adds and removes (by comparing lines, not by diffing: enough for a summary).
fn line_change(old: &str, new: &str) -> (usize, usize) {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..].iter().rev().zip(b[prefix..].iter().rev()).take_while(|(x, y)| x == y).count();
    (b.len() - prefix - suffix, a.len() - prefix - suffix)
}

fn patch(g: &PathGuard, trash: &Trash, args: &Value) -> Result<ProviderOutput, ToolError> {
    let targets = patch_targets(g, args)?;
    // Every file the patch changes is held until it is done: a second patch on one of them waits and then reads what this one wrote (card OA6-01, ED-06).
    let paths: Vec<&Path> = targets.iter().map(|(r, _)| r.path.as_path()).collect();
    let _held = locks::lock(&paths)?;
    let Plan { files: planned, warnings } = patch_plan(g, args, targets)?;
    // The files the person was shown are the files this call changes (card OA6-04, SE-13): checked with the files held, before anything is written.
    for p in &planned {
        check_shown(&p.resolved.path, if p.op == PatchOp::Create { "absent" } else { &p.original_sha })?;
    }
    #[cfg(test)]
    AFTER_PLAN.with(|hook| {
        if let Some(f) = hook.borrow().as_ref() {
            f();
        }
    });
    let changed: Vec<&Planned> = planned.iter().filter(|p| p.original != p.updated).collect();
    // What the patch does to each file that it changes, as a diff (card OA6-04): at most 40 lines for a file and 200 for the whole patch; a file that looks like it holds secrets is counted and not shown.
    let mut budget = RESULT_DIFF_TOTAL_LINES;
    let mine = supplied_by_patch(args);
    let diffs: Vec<Option<diff::Diff>> = planned
        .iter()
        .map(|p| {
            if budget == 0 || (p.original == p.updated && p.op == PatchOp::Update) {
                return None;
            }
            let (name, lines) = (display(&p.resolved.path), RESULT_DIFF_LINES.min(budget));
            let d = match p.op {
                PatchOp::Create => diff::created(&name, &plain(&p.updated), lines),
                PatchOp::Delete => diff::removed(&name, &plain(&p.original), PREVIEW_DELETE_LINES.min(lines)),
                PatchOp::Update => diff::diff_with(&name, &name, &plain(&p.original), &plain(&p.updated), PREVIEW_CONTEXT, lines, &mine),
            };
            budget = budget.saturating_sub(d.model_text.lines().count().saturating_sub(2));
            Some(d)
        })
        .collect();
    let op_name = |p: &Planned| match p.op {
        PatchOp::Create => "create",
        PatchOp::Update => "update",
        PatchOp::Delete => "delete",
    };
    let stats: Vec<Value> = planned
        .iter()
        .zip(&diffs)
        .map(|(p, d)| {
            let (added, removed) = line_change(&p.original, &p.updated);
            json!({
                "path": display(&p.resolved.path), "op": op_name(p), "lines_added": added, "lines_removed": removed, "changed": p.original != p.updated, "eol": p.eol,
                "diff": if p.resolved.sensitive { Value::Null } else { d.as_ref().map(|d| json!(d.model_text)).unwrap_or(Value::Null) }, "diff_truncated": d.as_ref().is_some_and(|d| d.model_truncated),
            })
        })
        .collect();
    let summary = planned
        .iter()
        .map(|p| {
            let (a, r) = line_change(&p.original, &p.updated);
            format!("{} (+{a} -{r})", display(&p.resolved.path))
        })
        .collect::<Vec<_>>()
        .join(", ");
    let warned = if warnings.is_empty() { String::new() } else { format!("\nWarning: {}", warnings.join("\nWarning: ")) };
    if arg_bool(args, "dry_run") {
        let shown: Vec<&str> = planned.iter().zip(&diffs).filter(|(p, _)| !p.resolved.sensitive).filter_map(|(_, d)| d.as_ref().map(|d| d.model_text.as_str())).collect();
        let hidden = if planned.iter().any(|p| p.resolved.sensitive) { "\n(the lines of a file that looks like it holds secrets are not shown)" } else { "" };
        let diffs_text = if shown.is_empty() { String::new() } else { format!("\n{}", shown.join("\n")) };
        return out(
            json!({"dry_run": true, "files": stats, "warnings": warnings}),
            format!("Dry run: the patch applies cleanly to {} file(s): {summary}. Nothing was written.{warned}{hidden}{diffs_text}", planned.len()),
        );
    }
    if changed.is_empty() {
        return out(json!({"files": stats, "undo_id": Value::Null, "warnings": warnings}), format!("The patch changes nothing: every file already has this content.{warned}"));
    }
    let group = trash.new_group_id();
    let mut backups: std::collections::HashMap<PathBuf, Entry> = std::collections::HashMap::new();
    for p in &changed {
        if p.op == PatchOp::Update {
            let backup = trash.backup_grouped(&p.resolved.path, "patched", &group).map_err(|e| ToolError::new(ErrorCode::BackendError, e))?;
            backups.insert(p.resolved.path.clone(), backup);
        }
    }
    let mut written: Vec<&Planned> = Vec::new();
    // The records of the files this patch took away, to put them back if a later write fails.
    let mut removed: std::collections::HashMap<PathBuf, Entry> = std::collections::HashMap::new();
    for p in &changed {
        let path = &p.resolved.path;
        let write_result: Result<(), ToolError> = match p.op {
            PatchOp::Delete => {
                // The file is taken away only if it is still the one the plan read.
                if sha256_file(path).map_or(true, |now| now != p.original_sha) {
                    Err(ToolError::new(ErrorCode::Conflict, format!("ERROR [FILE_CHANGED] {} changed while the patch was prepared; read it again", display(path))))
                } else {
                    trash.stash(path, "patched").map_err(|e| ToolError::new(ErrorCode::BackendError, e)).and_then(|entry| {
                        let grouped = trash.in_group(entry.clone(), &group).map_err(|e| ToolError::new(ErrorCode::BackendError, e))?;
                        removed.insert(path.clone(), grouped);
                        Ok(())
                    })
                }
            }
            PatchOp::Create => match path.parent().map(std::fs::create_dir_all).transpose() {
                Err(e) => Err(io_err(e, "create folders")),
                // A name another call took since the plan is not overwritten.
                Ok(_) => write_atomic_expecting(path, p.updated.as_bytes(), Expect::Absent).map_err(|e| if e.kind() == std::io::ErrorKind::AlreadyExists { ToolError::new(ErrorCode::Conflict, format!("{} appeared while the patch was prepared; it was not overwritten", display(path))) } else { write_err(e, path, "write") }),
            },
            PatchOp::Update => write_atomic_expecting(path, p.updated.as_bytes(), Expect::Unchanged(&p.original_sha)).map_err(|e| if is_changed_underneath(&e) { ToolError::new(ErrorCode::Conflict, format!("ERROR [FILE_CHANGED] {} changed while the patch was prepared; read it again", display(path))) } else { write_err(e, path, "write") }),
        };
        if let Err(e) = write_result {
            let mut stuck: Vec<String> = Vec::new();
            for done in &written {
                match done.op {
                    PatchOp::Create => {
                        let _ = std::fs::remove_file(&done.resolved.path);
                    }
                    PatchOp::Update => {
                        if write_atomic(&done.resolved.path, done.original.as_bytes()).is_err() {
                            stuck.push(display(&done.resolved.path));
                        }
                    }
                    PatchOp::Delete => {
                        let put_back = removed.get(&done.resolved.path).map(|entry| trash.restore_to(entry, &done.resolved.path).is_ok()).unwrap_or(false);
                        if !put_back {
                            stuck.push(display(&done.resolved.path));
                        }
                    }
                }
            }
            let tail = if stuck.is_empty() { "(the files already written were put back)".to_string() } else { format!("(the files already written were put back, except {}: restore them with fs.restore undo_id={group} force=true)", stuck.join(", ")) };
            return Err(ToolError::new(e.code, format!("{} {tail}", e.message)));
        }
        match p.op {
            PatchOp::Create => {
                trash.note_created_with_hash(&p.resolved.path, &sha256_hex(p.updated.as_bytes()));
            }
            PatchOp::Update => {
                if let Some(backup) = backups.get(&p.resolved.path) {
                    trash.record_after(backup, &sha256_hex(p.updated.as_bytes()));
                }
            }
            PatchOp::Delete => {}
        }
        written.push(p);
    }
    let changes: Vec<Value> = planned.iter().zip(&diffs).filter(|(p, _)| p.original != p.updated || p.op != PatchOp::Update).map(|(p, d)| change_entry(&display(&p.resolved.path), op_name(p), p.resolved.sensitive, d.as_ref())).collect();
    out(
        json!({"files": stats, "undo_id": group, "warnings": warnings, "changes": changes, "version": changed.iter().map(|p| json!({"path": display(&p.resolved.path), "version": sha256_hex(p.updated.as_bytes())})).collect::<Vec<_>>()}),
        format!("Patched {} file(s): {summary}. Undo all of it with fs.restore undo_id={group} force=true.{warned}", changed.len()),
    )
}

fn restore_group(g: &PathGuard, trash: &Trash, group: &str, force: bool) -> Result<ProviderOutput, ToolError> {
    let entries = trash.group(group);
    if entries.is_empty() {
        return Err(ToolError::new(ErrorCode::NotFound, format!("no undo group '{group}'")));
    }
    let mut dests: Vec<PathBuf> = Vec::new();
    for e in &entries {
        let dest = g.resolve(&e.original_path, Op::Write)?.path;
        if dest.exists() && !force {
            return Err(ToolError::new(ErrorCode::Conflict, format!("{} exists and was changed by the patch; set force=true to put the old content back (the current content is kept for undo)", display(&dest))));
        }
        dests.push(dest);
    }
    let _held = locks::lock(&dests.iter().map(|d| d.as_path()).collect::<Vec<_>>())?;
    let mut displaced = Vec::new();
    for (e, dest) in entries.iter().zip(&dests) {
        if dest.exists() {
            displaced.push(trash.stash(dest, "overwritten").map_err(|e| ToolError::new(ErrorCode::BackendError, e))?.undo_id);
        }
        trash.restore_to(e, dest).map_err(|e| ToolError::new(ErrorCode::BackendError, e))?;
    }
    out(
        json!({"restored": group, "files": dests.iter().map(|d| display(d)).collect::<Vec<_>>(), "displaced_undo_ids": displaced}),
        format!("Restored {} file(s) from {group}: {}.", dests.len(), dests.iter().map(|d| display(d)).collect::<Vec<_>>().join(", ")),
    )
}

fn copy(g: &PathGuard, trash: &Trash, args: &Value) -> Result<ProviderOutput, ToolError> {
    let src = g.resolve(arg_str(args, "src"), Op::Read)?;
    let dst = g.resolve(arg_str(args, "dst"), Op::Write)?;
    let _held = locks::lock(&[dst.path.as_path()])?;
    let meta = std::fs::symlink_metadata(&src.path).map_err(|e| io_err(e, &display(&src.path)))?;
    if meta.file_type().is_symlink() {
        return Err(ToolError::new(ErrorCode::InvalidArguments, "links are not copied"));
    }
    if meta.is_dir() {
        if !arg_bool(args, "recursive") {
            return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{} is a folder; set recursive=true", display(&src.path))));
        }
        if super::guard::within(&dst.path, &src.path) {
            return Err(ToolError::new(ErrorCode::InvalidArguments, "cannot copy a folder into itself"));
        }
        // A folder that holds Aqua's own files or a credential folder is not copied, not even without them: the copy would put them where the file tools read freely (card OA6-06, SE-04).
        // Nothing is copied and nothing is named: a partial copy would look like a whole one.
        let (barred, unchecked) = walk::protected_inside(g, &src.path, &AtomicBool::new(false));
        if barred > 0 {
            return Err(ToolError::new(
                ErrorCode::PermissionDenied,
                format!("{} holds {barred} protected entr{} (Aqua's own files or credential folders), so nothing was copied. Copy the files you need one by one, or a folder that does not hold them.", display(&src.path), if barred == 1 { "y" } else { "ies" }),
            ));
        }
        if unchecked {
            return Err(ToolError::new(ErrorCode::ResourceLimit, format!("{} is too big to be checked before it is copied; copy part of it", display(&src.path))));
        }
        let size = dir_size(&src.path);
        if size > 512 << 20 {
            return Err(ToolError::new(ErrorCode::ResourceLimit, "the folder is larger than 512 MB"));
        }
    }
    let mut undo_id = None;
    let dst_existed = dst.path.exists();
    if dst_existed {
        if !arg_bool(args, "overwrite") {
            return Err(ToolError::new(ErrorCode::AlreadyExists, format!("{} already exists; set overwrite=true to replace it", display(&dst.path))));
        }
        undo_id = Some(trash.stash(&dst.path, "overwritten").map_err(|e| ToolError::new(ErrorCode::BackendError, e))?.undo_id);
    }
    copy_recursive(&src.path, &dst.path).map_err(|e| io_err(e, "copy"))?;
    if !dst_existed {
        trash.note_created(&dst.path);
    }
    out(json!({"src": display(&src.path), "dst": display(&dst.path), "undo_id": undo_id}), format!("Copied {} to {}.", display(&src.path), display(&dst.path)))
}

fn is_root(g: &PathGuard, p: &Path) -> bool {
    g.roots().iter().any(|(_, rp, _)| super::guard::within(rp, p) && super::guard::within(p, rp))
}

fn mv(g: &PathGuard, trash: &Trash, args: &Value) -> Result<ProviderOutput, ToolError> {
    let src = g.resolve(arg_str(args, "src"), Op::Write)?;
    let dst = g.resolve(arg_str(args, "dst"), Op::Write)?;
    let _held = locks::lock(&[src.path.as_path(), dst.path.as_path()])?;
    if is_root(g, &src.path) {
        return Err(ToolError::new(ErrorCode::PermissionDenied, "an allowed root itself cannot be moved"));
    }
    if !src.path.exists() {
        return Err(ToolError::new(ErrorCode::NotFound, format!("{} does not exist", display(&src.path))));
    }
    if super::guard::within(&dst.path, &src.path) {
        return Err(ToolError::new(ErrorCode::InvalidArguments, "cannot move a folder into itself"));
    }
    let mut undo_id = None;
    if dst.path.exists() {
        if !arg_bool(args, "overwrite") {
            return Err(ToolError::new(ErrorCode::AlreadyExists, format!("{} already exists; set overwrite=true to replace it", display(&dst.path))));
        }
        undo_id = Some(trash.stash(&dst.path, "overwritten").map_err(|e| ToolError::new(ErrorCode::BackendError, e))?.undo_id);
    }
    if let Some(p) = dst.path.parent() {
        std::fs::create_dir_all(p).map_err(|e| io_err(e, "create folders"))?;
    }
    if std::fs::rename(&src.path, &dst.path).is_err() {
        copy_recursive(&src.path, &dst.path).map_err(|e| io_err(e, "move"))?;
        if src.path.is_dir() {
            std::fs::remove_dir_all(&src.path).map_err(|e| io_err(e, "remove the original"))?;
        } else {
            std::fs::remove_file(&src.path).map_err(|e| io_err(e, "remove the original"))?;
        }
    }
    trash.note_moved(&src.path, &dst.path);
    out(json!({"src": display(&src.path), "dst": display(&dst.path), "undo_id": undo_id}), format!("Moved {} to {}.", display(&src.path), display(&dst.path)))
}

fn delete(g: &PathGuard, trash: &Trash, args: &Value) -> Result<ProviderOutput, ToolError> {
    let r = g.resolve(arg_str(args, "path"), Op::Write)?;
    let _held = locks::lock(&[r.path.as_path()])?;
    if is_root(g, &r.path) {
        return Err(ToolError::new(ErrorCode::PermissionDenied, "an allowed root itself cannot be deleted"));
    }
    let meta = std::fs::symlink_metadata(&r.path).map_err(|e| io_err(e, &display(&r.path)))?;
    if meta.is_dir() && !meta.file_type().is_symlink() && !arg_bool(args, "recursive") && std::fs::read_dir(&r.path).map(|mut d| d.next().is_some()).unwrap_or(false) {
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{} is not empty; set recursive=true to delete it with its contents", display(&r.path))));
    }
    let entry = trash.stash(&r.path, "deleted").map_err(|e| ToolError::new(ErrorCode::BackendError, e))?;
    out(
        json!({"path": display(&r.path), "undo_id": entry.undo_id, "kind": entry.kind, "bytes": entry.bytes}),
        format!("Deleted {}. Restore it with fs.restore undo_id={}.", display(&r.path), entry.undo_id),
    )
}

/// Whether a file or folder was changed after the change a record describes: its modification time is later than the record's (made right
/// before or after that change) by more than the time one change takes. Then something else changed it, and putting it back would throw that
/// away.
///
/// When the record holds the hash of the file as the change left it, that decides: the same content is the same file, however recently an editor saved it again, and different
/// content is a change however close to the run it was made. Without a hash (an older record, a folder, a move) the modification time decides.
fn changed_after(path: &Path, entry: &Entry) -> bool {
    const SLACK_MS: u64 = 2_000;
    if let Some(want) = entry.after_sha256.as_deref() {
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file()) {
            if let Ok(now) = sha256_file(path) {
                return now != want;
            }
        }
    }
    std::fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .is_some_and(|d| d.as_millis() as u64 > entry.at_unix_ms.saturating_add(SLACK_MS))
}

/// Which records an undo of a run looks at (card OA7-03): what the file tools changed, what commands changed (their journal and the checkpoint of the run), or both.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UndoScope {
    All,
    Files,
    Commands,
}

fn undo_scope(args: &Value) -> Result<UndoScope, ToolError> {
    match arg_str(args, "scope") {
        "" | "all" => Ok(UndoScope::All),
        "files" => Ok(UndoScope::Files),
        "commands" => Ok(UndoScope::Commands),
        other => Err(ToolError::new(ErrorCode::InvalidArguments, format!("scope must be \"all\", \"files\" or \"commands\", not \"{other}\""))),
    }
}

/// Is this record of what a command or a checkpoint changed (and holds no copy of its own)?
fn is_effect(e: &Entry) -> bool {
    matches!(e.reason.as_str(), "command" | "checkpoint")
}

/// What one run changed, from step `from_step` on (the first step is 1; 0 and 1 mean all of it). A record with no step (made before steps were kept) counts as step 0, and so does the
/// record of a checkpoint, which belongs to the whole run: an undo from a step on leaves it alone.
fn run_entries_from(trash: &Trash, run: &str, from_step: u64, scope: UndoScope) -> Vec<Entry> {
    let mut entries = trash.run_entries(run);
    if from_step > 1 {
        entries.retain(|e| u64::from(e.step.unwrap_or(0)) >= from_step);
    }
    match scope {
        UndoScope::All => {}
        UndoScope::Files => entries.retain(|e| !is_effect(e)),
        UndoScope::Commands => entries.retain(is_effect),
    }
    entries
}

/// Where `fs.restore` puts an entry: where the caller says (`to`), else where it came from. An entry whose record was lost has no place of its own, so it needs `to`. Either way the
/// guard decides, as for any write.
fn restore_target(g: &PathGuard, entry: &Entry, args: &Value) -> Result<Resolved, ToolError> {
    let to = arg_str(args, "to");
    if !to.is_empty() {
        return g.resolve(to, Op::Write);
    }
    if entry.original_path.is_empty() {
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("'{}' has no record of where it came from (its record was lost): say where to put it with to=<path>", entry.undo_id)));
    }
    g.resolve(&entry.original_path, Op::Write)
}

/// `fs.undo_run`: puts back what one agent run changed, newest change first, so a file the run changed twice ends as it was before the run.
/// A change is put back only when nothing else changed the file since (a file edited by hand, or by a later run, is left and reported), and
/// what an undo replaces is kept in the undo store like any other overwrite. Commands, commits and network calls leave no record here: the
/// caller says which of those the run made.
fn undo_run(g: &PathGuard, trash: &Trash, effects: Option<&dyn EffectUndo>, args: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
    let run = arg_str(args, "run_id");
    let dry = arg_bool(args, "dry_run");
    let from_step = arg_u64(args, "from_step", 0);
    let scope = undo_scope(args)?;
    let entries = run_entries_from(trash, run, from_step, scope);
    let from = if from_step > 1 { format!(" from step {from_step}") } else { String::new() };
    if entries.is_empty() {
        return out(json!({"run_id": run, "from_step": from_step, "restored": [], "skipped": [], "changes": 0}), format!("No file change of run {run}{from} is in the undo store (it changed no files{}, or the store no longer keeps them).", if from_step > 1 { " from that step on" } else { "" }));
    }
    // Every file the undo may touch is held while it works (the ones that cannot be resolved are skipped below, as before). What a command or a checkpoint recorded names a folder, not a file.
    let mut held_paths: Vec<PathBuf> = Vec::new();
    for e in entries.iter().filter(|e| !is_effect(e)) {
        if let Ok(r) = g.resolve(&e.original_path, Op::Write) {
            held_paths.push(r.path);
        }
        if let Some(Ok(r)) = e.to_path.as_deref().map(|t| g.resolve(t, Op::Write)) {
            held_paths.push(r.path);
        }
    }
    let _held = locks::lock(&held_paths.iter().map(|p| p.as_path()).collect::<Vec<_>>())?;
    let mut touched: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    // The files this undo has dealt with, for a checkpoint to leave alone: it covers the whole run, and what the file tools and the commands put back is not its to put back again.
    let mut handled: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut restored: Vec<String> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    let skip = |skipped: &mut Vec<Value>, path: &str, why: String| skipped.push(json!({"path": path.replace('\\', "/"), "why": why}));
    // What a command or a checkpoint recorded: put back by what made the record (the git module), the answer merged into this one's.
    let put_back_effect = |e: &Entry, restored: &mut Vec<String>, skipped: &mut Vec<Value>, handled: &mut std::collections::HashSet<String>| {
        let Some(fx) = effects else {
            skip(skipped, &e.original_path, format!("what a {} changed cannot be put back from here", e.reason));
            return;
        };
        match fx.undo(e, g, dry, handled, cancel) {
            Ok(report) => {
                restored.extend(report.restored);
                for (path, why) in report.left {
                    skip(skipped, &path, why);
                }
                if !dry && report.settled {
                    trash.drop_entry(&e.undo_id);
                }
            }
            Err(err) => skip(skipped, &e.original_path, err.message),
        }
    };
    let mut checkpoints: Vec<&Entry> = Vec::new();
    for e in entries.iter().rev() {
        // A command is put back where it stands among the other changes (newest first); a checkpoint covers the whole run, so it comes last.
        match e.reason.as_str() {
            "command" => {
                put_back_effect(e, &mut restored, &mut skipped, &mut handled);
                continue;
            }
            "checkpoint" => {
                checkpoints.push(e);
                continue;
            }
            _ => {}
        }
        let path = match g.resolve(&e.original_path, Op::Write) {
            Ok(r) => r.path,
            Err(err) => {
                skip(&mut skipped, &e.original_path, format!("not in an allowed folder: {}", err.message));
                continue;
            }
        };
        handled.insert(handled_key(&path));
        if let Some(Ok(r)) = e.to_path.as_deref().map(|t| g.resolve(t, Op::Write)) {
            handled.insert(handled_key(&r.path));
        }
        let shown = display(&path);
        // A path this undo already put back is the undo's own doing, not somebody else's change.
        let mine = touched.contains(&path);
        // Set when the record has been dealt with for good (put back, or nothing left to put back): only then is it forgotten.
        let mut settled = false;
        match e.reason.as_str() {
            "created" => {
                if !path.exists() {
                    skip(&mut skipped, &shown, "already gone".into());
                    settled = true;
                } else if !mine && changed_after(&path, e) {
                    skip(&mut skipped, &shown, "changed after the run, so it was left".into());
                } else if dry {
                    touched.insert(path);
                    restored.push(format!("would remove {shown}"));
                } else {
                    let kept = trash.stash(&path, "undone").map_err(|m| ToolError::new(ErrorCode::BackendError, m))?;
                    touched.insert(path);
                    settled = true;
                    restored.push(format!("removed {shown} (kept as {})", kept.undo_id));
                }
            }
            "moved" => {
                let Some(to) = e.to_path.as_deref().and_then(|t| g.resolve(t, Op::Write).ok()).map(|r| r.path) else {
                    skip(&mut skipped, &shown, "where it went is not in an allowed folder".into());
                    continue;
                };
                if path.exists() {
                    skip(&mut skipped, &shown, format!("something is at {shown} again"));
                } else if !to.exists() {
                    skip(&mut skipped, &shown, format!("{} is gone", display(&to)));
                } else if !touched.contains(&to) && changed_after(&to, e) {
                    skip(&mut skipped, &shown, format!("{} changed after the run, so it was left", display(&to)));
                } else if dry {
                    touched.insert(path);
                    restored.push(format!("would move {} back to {shown}", display(&to)));
                } else {
                    if let Some(p) = path.parent() {
                        std::fs::create_dir_all(p).map_err(|er| io_err(er, "create folders"))?;
                    }
                    if std::fs::rename(&to, &path).is_err() {
                        copy_recursive(&to, &path).map_err(|er| io_err(er, "move back"))?;
                        if to.is_dir() {
                            std::fs::remove_dir_all(&to).map_err(|er| io_err(er, "remove the copy"))?;
                        } else {
                            std::fs::remove_file(&to).map_err(|er| io_err(er, "remove the copy"))?;
                        }
                    }
                    touched.insert(path);
                    settled = true;
                    restored.push(format!("moved {} back to {shown}", display(&to)));
                }
            }
            "deleted" => {
                if path.exists() {
                    skip(&mut skipped, &shown, "a file or folder is there again, so it was left".into());
                } else if dry {
                    touched.insert(path);
                    restored.push(format!("would restore {shown}"));
                } else {
                    trash.restore_to(e, &path).map_err(|m| ToolError::new(ErrorCode::BackendError, m))?;
                    touched.insert(path);
                    restored.push(format!("restored {shown}"));
                }
            }
            _ => {
                // overwritten, edited, patched: the record holds what was there before.
                if path.exists() && !mine && changed_after(&path, e) {
                    skip(&mut skipped, &shown, "changed after the run, so it was left".into());
                } else if dry {
                    touched.insert(path);
                    restored.push(format!("would put back the earlier {shown}"));
                } else {
                    let mut note = String::new();
                    if path.exists() {
                        let kept = trash.stash(&path, "undone").map_err(|m| ToolError::new(ErrorCode::BackendError, m))?;
                        note = format!(" (what was there is kept as {})", kept.undo_id);
                    }
                    trash.restore_to(e, &path).map_err(|m| ToolError::new(ErrorCode::BackendError, m))?;
                    touched.insert(path);
                    restored.push(format!("put back the earlier {shown}{note}"));
                }
            }
        }
        if !dry && settled && matches!(e.reason.as_str(), "created" | "moved") {
            trash.drop_entry(&e.undo_id);
        }
    }
    for e in checkpoints {
        put_back_effect(e, &mut restored, &mut skipped, &mut handled);
    }
    let text = format!(
        "{} run {run}{from}: {} put back, {} left.{}{}",
        if dry { "Dry run of the undo of" } else { "Undid" },
        restored.len(),
        skipped.len(),
        if restored.is_empty() { String::new() } else { format!("\n{}", restored.join("\n")) },
        if skipped.is_empty() { String::new() } else { format!("\nLeft as they are:\n{}", skipped.iter().map(|s| format!("{}: {}", s["path"].as_str().unwrap_or(""), s["why"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n")) },
    );
    out(json!({"run_id": run, "from_step": from_step, "dry_run": dry, "changes": entries.len(), "restored": restored, "skipped": skipped}), text)
}

fn restore(g: &PathGuard, trash: &Trash, args: &Value) -> Result<ProviderOutput, ToolError> {
    let id = arg_str(args, "undo_id");
    if id.is_empty() {
        let limit = arg_u64(args, "limit", 20) as usize;
        let entries = trash.list(limit);
        let text = entries
            .iter()
            .map(|e| {
                // A slot whose record was lost, and a move that was cut short, are shown as what they are.
                let place = if e.unrecorded && e.original_path.is_empty() { "(its record was lost: restore it with to=<path>)".to_string() } else { e.original_path.replace('\\', "/") };
                let flag = if e.unrecorded { " [unrecorded]" } else if e.is_interrupted() { " [interrupted: the copy may be incomplete]" } else { "" };
                format!("{}  {}  {} ({} bytes){flag}", e.undo_id, e.reason, place, e.bytes)
            })
            .collect::<Vec<_>>()
            .join("\n");
        return out(json!({ "entries": entries }), if text.is_empty() { "The undo store is empty.".into() } else { text });
    }
    if id.starts_with("p_") {
        if !arg_str(args, "to").is_empty() {
            return Err(ToolError::new(ErrorCode::InvalidArguments, "to puts back one entry; a patch group is put back where its files were"));
        }
        return restore_group(g, trash, id, arg_bool(args, "force"));
    }
    let entry = trash.get(id).ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("no undo entry '{id}'")))?;
    if matches!(entry.reason.as_str(), "created" | "moved") {
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("'{id}' only records that a run {} something: it holds no copy. Use fs.undo_run with the run's id.", entry.reason)));
    }
    if is_effect(&entry) {
        let what = if entry.reason == "command" { "a command changed files (term.undo puts them back)" } else { "the files of a run were checkpointed" };
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("'{id}' only records that {what}: it holds no copy of its own. Use fs.undo_run with the run's id, or term.undo with the call_id its result named.")));
    }
    let dest: PathBuf = restore_target(g, &entry, args)?.path;
    let _held = locks::lock(&[dest.as_path()])?;
    let mut displaced = None;
    if dest.exists() {
        if !arg_bool(args, "force") {
            return Err(ToolError::new(ErrorCode::Conflict, format!("{} exists again; set force=true to replace it (its current content is kept for undo)", display(&dest))));
        }
        displaced = Some(trash.stash(&dest, "overwritten").map_err(|e| ToolError::new(ErrorCode::BackendError, e))?.undo_id);
    }
    trash.restore_to(&entry, &dest).map_err(|e| ToolError::new(ErrorCode::BackendError, e))?;
    out(json!({"path": display(&dest), "restored": id, "displaced_undo_id": displaced}), format!("Restored {}.", display(&dest)))
}

fn set_attrs(g: &PathGuard, args: &Value) -> Result<ProviderOutput, ToolError> {
    let r = g.resolve(arg_str(args, "path"), Op::Write)?;
    let m = std::fs::metadata(&r.path).map_err(|e| io_err(e, &display(&r.path)))?;
    let mut perms = m.permissions();
    perms.set_readonly(arg_bool(args, "readonly"));
    std::fs::set_permissions(&r.path, perms).map_err(|e| io_err(e, "set attributes"))?;
    out(json!({"path": display(&r.path), "readonly": arg_bool(args, "readonly")}), format!("Set read-only={} on {}.", arg_bool(args, "readonly"), display(&r.path)))
}


/// `fs.edit_lines`, the tool that replaces a range of lines held to an anchor (card OA6-02, ED-07): in `provider/edit_lines.rs`, which uses what this file keeps to itself.
mod edit_lines;

/// Property test of the all-or-nothing rule of a patch (card OA13-01): a write is made to fail at each place in turn, in `provider/properties.rs`.
#[cfg(test)]
mod properties;

#[cfg(test)]
mod toctou_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn rig(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aqua_fsopen_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ws")).unwrap();
        std::fs::create_dir_all(dir.join("outside")).unwrap();
        dir
    }

    #[test]
    fn a_search_skips_build_folders() {
        // The list moved to the walker (card OA6-06): the same names, one list for list, find and grep.
        assert!(walk::is_junk_dir("target") && walk::is_junk_dir("node_modules") && walk::is_junk_dir(".git"));
        assert!(!walk::is_junk_dir("src") && !walk::is_junk_dir("vendor"));
    }

    /// A folder junction (`mklink /J`): it needs no privilege, unlike a symbolic link.
    #[cfg(windows)]
    fn junction(link: &Path, target: &Path) {
        let out = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).output().expect("cmd runs");
        assert!(out.status.success(), "mklink /J failed: {}", String::from_utf8_lossy(&out.stdout));
    }

    fn resolved(path: PathBuf) -> Resolved {
        Resolved { path, root_alias: "ws".into(), trusted: true, sensitive: false, executable_kind: false }
    }

    #[test]
    fn a_file_that_is_written_goes_through_a_temporary_file_that_is_new_so_a_planted_link_at_its_name_is_not_followed() {
        let dir = rig("planted");
        let outside = dir.join("outside").join("keep.txt");
        std::fs::write(&outside, "keep").unwrap();
        let dest = dir.join("ws").join("note.txt");
        // The first temporary name is already taken by a second name of a file outside the folder (a hard link needs no privilege).
        let first = dest.with_extension(format!("aqua-tmp-{}-0", std::process::id()));
        std::fs::hard_link(&outside, &first).unwrap();
        write_atomic(&dest, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep", "the file the planted name led to was not written through");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
        assert!(first.exists(), "what was planted is left as it was");
    }

    #[cfg(windows)]
    #[test]
    fn a_write_into_a_folder_that_is_no_longer_where_it_was_checked_to_be_writes_nothing() {
        let dir = rig("write_swapped");
        junction(&dir.join("ws").join("door"), &dir.join("outside"));
        // The path the guard had checked named a plain folder `door`; by the time of the write it leads outside.
        let said = write_atomic(&dir.join("ws").join("door").join("new.txt"), b"data").unwrap_err();
        assert_eq!(said.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(said.to_string().contains("a link was put in its place") && said.to_string().contains("nothing was written"), "{said}");
        let left: Vec<_> = std::fs::read_dir(dir.join("outside")).unwrap().collect();
        assert!(left.is_empty(), "nothing was left outside, not even the temporary file: {left:?}");
    }

    #[cfg(windows)]
    #[test]
    fn an_append_through_a_swapped_folder_is_refused_and_the_file_it_created_there_is_taken_away() {
        let dir = rig("append_swapped");
        junction(&dir.join("ws").join("door"), &dir.join("outside"));
        let said = open_append_checked(&resolved(dir.join("ws").join("door").join("log.txt"))).unwrap_err();
        assert_eq!(said.code, ErrorCode::PermissionDenied);
        assert!(said.message.contains("nothing was read or written"), "{}", said.message);
        assert!(!dir.join("outside").join("log.txt").exists(), "the file that was created outside by the open is gone");
        // An existing file outside is refused too, and left as it was.
        std::fs::write(dir.join("outside").join("old.txt"), "old").unwrap();
        assert!(open_append_checked(&resolved(dir.join("ws").join("door").join("old.txt"))).is_err());
        assert_eq!(std::fs::read_to_string(dir.join("outside").join("old.txt")).unwrap(), "old");
    }

    #[cfg(windows)]
    #[test]
    fn a_read_through_a_swapped_folder_returns_nothing() {
        let dir = rig("read_swapped");
        std::fs::write(dir.join("outside").join("secret.txt"), "SECRET").unwrap();
        junction(&dir.join("ws").join("door"), &dir.join("outside"));
        let said = read_checked(&resolved(dir.join("ws").join("door").join("secret.txt"))).unwrap_err();
        assert_eq!(said.code, ErrorCode::PermissionDenied);
        assert!(open_checked(&resolved(dir.join("ws").join("door").join("secret.txt"))).is_err());
        // The same file, read where it is, is read.
        std::fs::write(dir.join("ws").join("fine.txt"), "fine").unwrap();
        let ok = std::fs::canonicalize(dir.join("ws").join("fine.txt")).unwrap();
        let ok = PathBuf::from(ok.to_string_lossy().trim_start_matches(r"\\?\"));
        assert_eq!(read_checked(&resolved(ok)).unwrap(), b"fine");
    }

    // ---- card OA6-01 (ED-06): a write that expects the file as the caller found it, or not to exist --------------------------------

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn a_write_that_expects_the_file_unchanged_stops_when_it_was_changed_or_taken_away_and_leaves_nothing_behind() {
        let dir = rig("expect_unchanged");
        let dest = dir.join("ws").join("f.txt");
        std::fs::write(&dest, "as read").unwrap();
        let seen = sha256_hex(b"as read");
        write_atomic_expecting(&dest, b"new", Expect::Unchanged(&seen)).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
        // Somebody else saved it after it was read: the write is refused and their content stays.
        std::fs::write(&dest, "changed by hand").unwrap();
        let said = write_atomic_expecting(&dest, b"newer", Expect::Unchanged(&seen)).unwrap_err();
        assert!(is_changed_underneath(&said), "{said}");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "changed by hand");
        assert_eq!(names(&dir.join("ws")), vec!["f.txt".to_string()], "no temporary file is left");
        let told = write_err(said, &dest, "write");
        assert_eq!(told.code, ErrorCode::Conflict);
        assert!(told.message.contains("FILE_CHANGED") && told.message.contains("read it again"), "{}", told.message);
        // Taken away: the same.
        std::fs::remove_file(&dest).unwrap();
        let said = write_atomic_expecting(&dest, b"newer", Expect::Unchanged(&seen)).unwrap_err();
        assert!(is_changed_underneath(&said));
        assert!(!dest.exists());
        assert_eq!(names(&dir.join("ws")), Vec::<String>::new());
    }

    #[test]
    fn two_creations_of_one_name_cannot_both_succeed() {
        let dir = rig("create_exclusive");
        let dest = dir.join("ws").join("once.txt");
        for round in 0..20 {
            let _ = std::fs::remove_file(&dest);
            let wins = std::sync::atomic::AtomicUsize::new(0);
            let taken = std::sync::atomic::AtomicUsize::new(0);
            std::thread::scope(|s| {
                for t in 0..6 {
                    let (dest, wins, taken) = (&dest, &wins, &taken);
                    s.spawn(move || match write_atomic_expecting(dest, format!("writer {t}").as_bytes(), Expect::Absent) {
                        Ok(()) => {
                            wins.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                            taken.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(e) => panic!("round {round}: {e}"),
                    });
                }
            });
            assert_eq!(wins.load(Ordering::SeqCst), 1, "round {round}: one creation wins");
            assert_eq!(taken.load(Ordering::SeqCst), 5, "round {round}: the others are told the name is taken");
            assert!(std::fs::read_to_string(&dest).unwrap().starts_with("writer "), "whole content of one writer");
            assert_eq!(names(&dir.join("ws")), vec!["once.txt".to_string()], "round {round}: no temporary file is left");
        }
    }

    fn world(name: &str) -> (PathBuf, PathGuard, Trash) {
        let dir = rig(name);
        let guard = PathGuard::new(super::super::guard::FsConfig::with_workspace(dir.join("ws").to_string_lossy().to_string()), vec![dir.join("aqua")]).unwrap();
        let trash = Trash::new(dir.join("aqua").join("trash")).unwrap();
        (dir, guard, trash)
    }

    #[test]
    fn a_patch_whose_file_changed_after_planning_is_refused() {
        let (dir, guard, trash) = world("patch_changed_after_plan");
        let a = dir.join("ws").join("a.txt");
        std::fs::write(&a, "one\ntwo\n").unwrap();
        let args = json!({"edits": [{"path": "@ws/a.txt", "old": "one", "new": "ONE"}]});
        // The world changes between the plan and the write, as a person saving in an editor would change it.
        let target = a.clone();
        AFTER_PLAN.with(|h| *h.borrow_mut() = Some(Box::new(move || std::fs::write(&target, "one\ntwo\nadded by the person\n").unwrap())));
        let said = patch(&guard, &trash, &args);
        AFTER_PLAN.with(|h| *h.borrow_mut() = None);
        let said = said.err().expect("the patch is refused");
        assert_eq!(said.code, ErrorCode::Conflict);
        assert!(said.message.contains("FILE_CHANGED") && said.message.contains("changed while the patch was prepared"), "{}", said.message);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "one\ntwo\nadded by the person\n", "the person's save is not thrown away");
        assert_eq!(names(&dir.join("ws")), vec!["a.txt".to_string()], "no temporary file is left");
        // Without the change in between it goes through.
        assert!(patch(&guard, &trash, &args).is_ok());
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "ONE\ntwo\nadded by the person\n");
    }

    #[test]
    fn a_patch_that_fails_after_deleting_a_file_puts_the_deleted_file_back_and_the_written_ones() {
        let (dir, guard, trash) = world("patch_rollback_unit");
        let (a, b, c) = (dir.join("ws").join("a.txt"), dir.join("ws").join("b.txt"), dir.join("ws").join("c.txt"));
        std::fs::write(&a, "to be deleted\n").unwrap();
        std::fs::write(&b, "b one\n").unwrap();
        std::fs::write(&c, "c one\n").unwrap();
        // Delete a, change b, change c: and after the plan c is replaced by a folder, so that its write fails.
        let diff = "--- a/@ws/a.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-to be deleted\n--- a/@ws/b.txt\n+++ b/@ws/b.txt\n@@ -1 +1 @@\n-b one\n+b two\n--- a/@ws/c.txt\n+++ b/@ws/c.txt\n@@ -1 +1 @@\n-c one\n+c two\n";
        let victim = c.clone();
        AFTER_PLAN.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                std::fs::remove_file(&victim).unwrap();
                std::fs::create_dir(&victim).unwrap();
            }))
        });
        let said = patch(&guard, &trash, &json!({"diff": diff}));
        AFTER_PLAN.with(|h| *h.borrow_mut() = None);
        let said = said.err().expect("the third write fails");
        assert!(said.message.contains("put back") && !said.message.contains("except"), "{}", said.message);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "to be deleted\n", "the deleted file is back");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "b one\n", "the file that was written is as it was");
        assert!(c.is_dir());
    }
}
