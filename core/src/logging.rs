//! The core's own log: how it is set up, where it goes, how it is kept small, and how it is read back (card OA3-04; tasks EX-15 and AQ-19 of the OpenCode audit).
//!
//! Before, `main` built one `tracing_subscriber::fmt` subscriber that printed to the standard output, and the desktop shell caught that output into `logs/core.log` (the shell starts
//! the file again at 5 MB). A core started by hand wrote to its console only, a line of a run could be told from the next run's only by the clock, and nothing but the shell cared
//! how big the file got. This module is the one place the subscriber is built:
//!
//! * **The text format is the default and is what it was**, with the one difference that a line written inside a run or a tool call now says so: `run{run_id=... route=... caller=...}:
//!   tool{tool=fs.read call_id=...}: ...` (the spans of `agent_runs::scope::RunScope` and `ToolHost::call`; the turn span is older). `RUST_LOG` still decides what is shown; without it
//!   the filter is [`DEFAULT_FILTER`].
//! * **`AQUA_LOG_FORMAT=json`** writes one JSON object per line instead: `ts`, `level`, `target`, `message`, `fields`, `spans` (every span the line was written in, from the outermost,
//!   with its fields), and `run_id`, `call_id` and `request_id` lifted to the top when a span or the event carries one. Written here (a layer of about eighty lines) because the
//!   `json` feature of `tracing-subscriber` needs `tracing-serde`, which is not a dependency of this workspace.
//! * **`AQUA_LOG_FILE=<path>`** makes the core write its log to that file itself, rotated by hand ([`RotatingFile`]: 5 MB a file, at most five files, `<name>.1` the newest of the old
//!   ones) and, when the standard output is a terminal, to the console as well. It is for a core that is started by hand or by a script; the desktop shell does not set it, so the shell
//!   goes on catching the output as it did.
//! * A line is never lost to a rotation that fails, and the core never stops because its log cannot be written: a file that cannot be opened (an editor holds it) drops the lines
//!   and counts them, says how many when it can write again, and a file that cannot be renamed is written past its size until the next try.
//! * **`aqua-cli logs`** ([`run_command`]) reads the files back: `--run ID` keeps the lines of one run (the JSON lines that carry the id in a span, the text lines that name it), `--level
//!   L` the lines at that level and above, `--tail N` the last N, `--json` prints each as a JSON object.
//!
//! What is logged is a name, an id, a number or a state. A tool's arguments and results are not in a span or in the line that closes a call (the audit log keeps them, masked);
//! the query string of a request is not logged.

use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::fmt::Debug;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tracing::field::{Field, Visit};
use tracing::{span, Event, Subscriber};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer, Registry};

/// What is shown when `RUST_LOG` is not set (or is not a filter): the core's own crates at `info`, the tool host's (one line per tool call) too, everything else at `warn`.
pub const DEFAULT_FILTER: &str = "warn,aqua_core=info,aqua_native_tools=info,aqua_audio_adapter=info,aqua_mcp_adapters::app_launch=info,aqua_tool_host=info";
/// The size at which the log file is started again.
pub const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
/// How many files there are at most: the one written and four rotated ones (`.1` to `.4`).
pub const KEEP_FILES: usize = 5;
/// The longest a string in a JSON line is (a panic with its backtrace is long; a pasted page is not a log line).
const MAX_FIELD_CHARS: usize = 16 * 1024;
/// A file that could not be opened is tried again after this long (not on every line).
const REOPEN_AFTER: Duration = Duration::from_secs(2);

/// The format of a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
}

/// How the log is set up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogConfig {
    pub format: Format,
    /// `AQUA_LOG_FILE`: the file to write to (with rotation).
    pub file: Option<PathBuf>,
}

impl LogConfig {
    /// From `AQUA_LOG_FORMAT` (`json`, anything else is text) and `AQUA_LOG_FILE` (a path; empty is none).
    pub fn from_env() -> LogConfig {
        LogConfig::from_values(std::env::var("AQUA_LOG_FORMAT").ok().as_deref(), std::env::var("AQUA_LOG_FILE").ok().as_deref())
    }

    /// The same from the values (for tests).
    pub fn from_values(format: Option<&str>, file: Option<&str>) -> LogConfig {
        LogConfig {
            format: if format.is_some_and(|f| f.trim().eq_ignore_ascii_case("json")) { Format::Json } else { Format::Text },
            file: file.map(str::trim).filter(|f| !f.is_empty()).map(PathBuf::from),
        }
    }
}

/// The filter for the value of `RUST_LOG`: it when it is a filter, else [`DEFAULT_FILTER`]. (`RUST_LOG` wins; a mistake in it is not a reason to have no log.)
pub fn filter_for(rust_log: Option<&str>) -> EnvFilter {
    rust_log.map(str::trim).filter(|v| !v.is_empty()).and_then(|v| EnvFilter::try_new(v).ok()).unwrap_or_else(|| EnvFilter::new(DEFAULT_FILTER))
}

/// The boxed form of a layer, so that the layers of a log (the console, the file) can be put together at run time.
pub type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>;

/// One layer that writes lines of `format` to `writer`.
pub fn layer<W>(format: Format, writer: W, ansi: bool) -> BoxedLayer
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    match format {
        Format::Text => tracing_subscriber::fmt::layer().with_writer(writer).with_ansi(ansi).boxed(),
        Format::Json => JsonLayer { writer }.boxed(),
    }
}

/// A subscriber of `layers` that shows what `filter` says.
pub fn subscriber(filter: EnvFilter, layers: Vec<BoxedLayer>) -> impl Subscriber + Send + Sync + 'static {
    Registry::default().with(layers).with(filter)
}

/// Sets the log up for the process: the filter from `RUST_LOG` (else [`DEFAULT_FILTER`]), the console (colour only on a terminal) unless a file is set and the output is not a
/// terminal, and the rotated file if `AQUA_LOG_FILE` is set and can be opened. Returns what the person should be told (a file that could not be opened, in which case the console
/// is used), or an error if a subscriber was set already. Never panics.
pub fn init(config: &LogConfig) -> Result<Option<String>, String> {
    use std::io::IsTerminal;
    let terminal = std::io::stdout().is_terminal();
    let mut layers: Vec<BoxedLayer> = Vec::new();
    let mut note = None;
    let mut console = true;
    if let Some(path) = &config.file {
        match RotatingFile::open(path, MAX_FILE_BYTES, KEEP_FILES, config.format) {
            Ok(file) => {
                layers.push(layer(config.format, file, false));
                // A core started by hand is read on its console; one a program started is read in its file.
                console = terminal;
            }
            Err(e) => note = Some(format!("the log file {} could not be opened ({e}); the log goes to the console", path.display())),
        }
    }
    if console {
        layers.push(layer(config.format, std::io::stdout, terminal));
    }
    let filter = filter_for(std::env::var("RUST_LOG").ok().as_deref());
    // `try_init` also forwards what the libraries write through the `log` crate (the HTTP client's, for one) to this subscriber, as the `fmt().init()` that was here before did.
    tracing_subscriber::util::SubscriberInitExt::try_init(subscriber(filter, layers)).map_err(|e| format!("a log was already set up: {e}"))?;
    Ok(note)
}

// ---- the JSON line --------------------------------------------------------------------------------------------

/// The fields of a span, kept with it from when it was made.
struct SpanFields(Map<String, Value>);

fn clean(text: &str) -> String {
    if text.chars().count() <= MAX_FIELD_CHARS {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(MAX_FIELD_CHARS).collect();
    cut.push_str("...");
    cut
}

/// Collects the fields of an event or a span into a JSON object.
struct Collect<'a>(&'a mut Map<String, Value>);

impl Visit for Collect<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), Value::String(clean(value)));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_string(), json!(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_string(), json!(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_string(), json!(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.insert(field.name().to_string(), serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.0.insert(field.name().to_string(), Value::String(clean(&format!("{value:?}"))));
    }
}

/// A layer that writes each event as one JSON object on one line.
struct JsonLayer<W> {
    writer: W,
}

impl<S, W> Layer<S> for JsonLayer<W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'a> MakeWriter<'a> + 'static,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let mut fields = Map::new();
            attrs.record(&mut Collect(&mut fields));
            span.extensions_mut().insert(SpanFields(fields));
        }
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            if let Some(fields) = span.extensions_mut().get_mut::<SpanFields>() {
                values.record(&mut Collect(&mut fields.0));
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut fields = Map::new();
        event.record(&mut Collect(&mut fields));
        let message = fields.remove("message").unwrap_or(Value::Null);
        let meta = event.metadata();
        let mut spans: Vec<Value> = Vec::new();
        let (mut run_id, mut call_id, mut request_id) = (None, None, None);
        if let Some(scope) = ctx.event_scope(event) {
            // Innermost first, so that the nearest span's id is the one lifted.
            for span in scope {
                let ext = span.extensions();
                let mut entry = Map::new();
                entry.insert("name".to_string(), json!(span.name()));
                if let Some(f) = ext.get::<SpanFields>() {
                    for (k, v) in &f.0 {
                        entry.insert(k.clone(), v.clone());
                    }
                }
                for (slot, key) in [(&mut run_id, "run_id"), (&mut call_id, "call_id"), (&mut request_id, "request_id")] {
                    if slot.is_none() {
                        *slot = entry.get(key).cloned();
                    }
                }
                spans.push(Value::Object(entry));
            }
            spans.reverse();
        }
        // An id the event itself carries counts as well, and is the nearest of all.
        for (slot, key) in [(&mut run_id, "run_id"), (&mut call_id, "call_id"), (&mut request_id, "request_id")] {
            if let Some(v) = fields.get(key) {
                *slot = Some(v.clone());
            }
        }
        let mut line = Map::new();
        line.insert("ts".to_string(), json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)));
        line.insert("level".to_string(), json!(meta.level().as_str()));
        line.insert("target".to_string(), json!(meta.target()));
        line.insert("message".to_string(), message);
        for (key, value) in [("run_id", run_id), ("call_id", call_id), ("request_id", request_id)] {
            if let Some(v) = value {
                line.insert(key.to_string(), v);
            }
        }
        if !fields.is_empty() {
            line.insert("fields".to_string(), Value::Object(fields));
        }
        if !spans.is_empty() {
            line.insert("spans".to_string(), Value::Array(spans));
        }
        let mut text = Value::Object(line).to_string();
        text.push('\n');
        // One write for the whole line, so that two threads' lines are not mixed. A log that cannot be written is not a reason to stop.
        let _ = self.writer.make_writer().write_all(text.as_bytes());
    }
}

// ---- the rotated file -----------------------------------------------------------------------------------------

struct FileState {
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    format: Format,
    file: Option<File>,
    size: u64,
    /// Lines that were not written because the file could not be opened.
    dropped: u64,
    /// Lines dropped since the last notice.
    unreported: u64,
    reopen_after: Duration,
    /// Not before this: opening the file again after it could not be opened.
    retry_at: Option<Instant>,
    /// Not before this: trying again to start the file over after it could not be renamed.
    rotate_after: Option<Instant>,
}

impl FileState {
    fn open_file(&mut self) -> bool {
        if self.retry_at.is_some_and(|at| Instant::now() < at) {
            return false;
        }
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(dir);
        }
        match OpenOptions::new().create(true).append(true).open(&self.path) {
            Ok(file) => {
                self.size = file.metadata().map(|m| m.len()).unwrap_or(0);
                self.file = Some(file);
                self.retry_at = None;
                true
            }
            Err(_) => {
                self.retry_at = Some(Instant::now() + self.reopen_after);
                false
            }
        }
    }

    fn numbered(&self, n: usize) -> PathBuf {
        let mut name = self.path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(format!(".{n}"));
        self.path.with_file_name(name)
    }

    /// Starts the file again: `.4` goes, `.3` becomes `.4` ... the file becomes `.1`, and a new file is opened. When the file cannot be renamed (another program holds it) it is kept
    /// and written on, and the next try is a little later.
    fn rotate(&mut self) {
        // The handle is let go first: Windows will not rename a file this process holds open without sharing.
        self.file = None;
        let moved = if self.keep > 1 {
            let _ = std::fs::remove_file(self.numbered(self.keep - 1));
            for n in (1..self.keep - 1).rev() {
                let from = self.numbered(n);
                if from.exists() {
                    let _ = std::fs::rename(&from, self.numbered(n + 1));
                }
            }
            std::fs::rename(&self.path, self.numbered(1)).is_ok()
        } else {
            std::fs::remove_file(&self.path).is_ok()
        };
        self.rotate_after = (!moved).then(|| Instant::now() + self.reopen_after);
        // The file is opened again at once, whether it is a new one or the old one that could not be moved.
        self.retry_at = None;
        self.open_file();
    }

    /// What to say about lines that were dropped.
    fn notice(&self, n: u64) -> String {
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let text = format!("{n} log line(s) were dropped because the log file could not be written (another program held it)");
        match self.format {
            Format::Json => format!("{}\n", json!({ "ts": ts, "level": "WARN", "target": "aqua_core::logging", "message": text })),
            Format::Text => format!("{ts}  WARN aqua_core::logging: {text}\n"),
        }
    }

    fn write_line(&mut self, buf: &[u8]) {
        if self.file.is_none() && !self.open_file() {
            self.dropped += 1;
            self.unreported += 1;
            return;
        }
        if self.size > 0 && self.size + buf.len() as u64 > self.max_bytes && self.rotate_after.is_none_or(|at| Instant::now() >= at) {
            self.rotate();
            if self.file.is_none() {
                self.dropped += 1;
                self.unreported += 1;
                return;
            }
        }
        if self.unreported > 0 {
            let notice = self.notice(self.unreported);
            if let Some(file) = self.file.as_mut() {
                if file.write_all(notice.as_bytes()).is_ok() {
                    self.size += notice.len() as u64;
                    self.unreported = 0;
                }
            }
        }
        let Some(file) = self.file.as_mut() else { return };
        match file.write_all(buf) {
            Ok(()) => self.size += buf.len() as u64,
            Err(_) => {
                self.file = None;
                self.dropped += 1;
                self.unreported += 1;
            }
        }
    }
}

/// A log file that keeps itself small: when a line would take it past `max_bytes` it becomes `<name>.1`, the older ones move up one (`.1` to `.2` ...), and the oldest is deleted,
/// so there are never more than `keep` files. It is the writer of the log (cloning it shares the file). Writing never fails: a line that cannot be written is counted
/// ([`dropped`](RotatingFile::dropped)) and reported when writing works again.
#[derive(Clone)]
pub struct RotatingFile {
    state: Arc<Mutex<FileState>>,
}

impl RotatingFile {
    /// Opens (or makes) `path`, to be appended to. The folder is made if it is not there. An error here is the only one this type gives: the file could not be opened at all.
    pub fn open(path: impl Into<PathBuf>, max_bytes: u64, keep: usize, format: Format) -> std::io::Result<RotatingFile> {
        let mut state = FileState {
            path: path.into(),
            max_bytes: max_bytes.max(1),
            keep: keep.max(1),
            format,
            file: None,
            size: 0,
            dropped: 0,
            unreported: 0,
            reopen_after: REOPEN_AFTER,
            retry_at: None,
            rotate_after: None,
        };
        if !state.open_file() {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "the file could not be opened for appending"));
        }
        Ok(RotatingFile { state: Arc::new(Mutex::new(state)) })
    }

    /// How many lines were lost because the file could not be written (since this writer was made).
    pub fn dropped(&self) -> u64 {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).dropped
    }

    /// How long to wait before trying to open a file again after it failed (a test makes it short).
    pub fn retry_after(self, wait: Duration) -> RotatingFile {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).reopen_after = wait;
        self
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).write_line(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(file) = self.state.lock().unwrap_or_else(PoisonError::into_inner).file.as_mut() {
            let _ = file.flush();
        }
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for RotatingFile {
    type Writer = RotatingFile;

    fn make_writer(&'a self) -> RotatingFile {
        self.clone()
    }
}

// ---- reading the log back --------------------------------------------------------------------------------------

/// What `aqua-cli logs` was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// Only the lines of this run.
    pub run: Option<String>,
    /// Only lines at this level or above (`error` is the highest).
    pub level: Option<Level>,
    /// The last this many matching lines.
    pub tail: usize,
    /// Print each line as a JSON object.
    pub json: bool,
    /// The file to read (the older ones beside it are read first).
    pub file: Option<PathBuf>,
}

/// A level of a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Level {
    pub fn parse(word: &str) -> Option<Level> {
        match word.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "warn" | "warning" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            "trace" => Some(Level::Trace),
            _ => None,
        }
    }
}

/// How many lines `aqua-cli logs` shows when it is not told.
pub const DEFAULT_TAIL: usize = 200;

/// The words after `aqua-cli logs`.
pub fn parse_args(args: &[String]) -> Result<Query, String> {
    let mut q = Query { run: None, level: None, tail: DEFAULT_TAIL, json: false, file: None };
    let mut words = args.iter();
    while let Some(word) = words.next() {
        let mut value = |what: &str| words.next().map(String::as_str).ok_or_else(|| format!("{word} needs {what}"));
        match word.as_str() {
            "--run" => q.run = Some(value("a run id")?.to_string()).filter(|r| !r.trim().is_empty()),
            "--level" => {
                let v = value("a level (error, warn, info, debug, trace)")?;
                q.level = Some(Level::parse(v).ok_or_else(|| format!("{v:?} is not a level (error, warn, info, debug, trace)"))?);
            }
            "--tail" => {
                let v = value("a number")?;
                q.tail = v.trim().parse::<usize>().ok().filter(|n| *n >= 1).ok_or_else(|| format!("{v:?} is not a number of lines (1 or more)"))?;
            }
            "--file" => q.file = Some(PathBuf::from(value("a path")?)),
            "--json" => q.json = true,
            other => return Err(format!("{other:?} is not an option of logs (--run ID, --level L, --tail N, --json, --file PATH)")),
        }
    }
    Ok(q)
}

/// The log files of `base` that exist, oldest first: `base.4`, `base.3`, ..., `base.1`, `base`.
pub fn files_of(base: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for n in (1..KEEP_FILES).rev() {
        let mut name = base.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(format!(".{n}"));
        let p = base.with_file_name(name);
        if p.is_file() {
            files.push(p);
        }
    }
    if base.is_file() {
        files.push(base.to_path_buf());
    }
    files
}

/// `id` appears in `line` as a whole name: not as the start of a longer one (`run_7_ab_1` is not in `run_7_ab_10`).
fn names_id(line: &str, id: &str) -> bool {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(at) = line[from..].find(id) {
        let start = from + at;
        let end = start + id.len();
        let before_ok = line[..start].chars().next_back().is_none_or(|c| !word(c));
        let after_ok = line[end..].chars().next().is_none_or(|c| !word(c));
        if before_ok && after_ok {
            return true;
        }
        from = start + line[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// The line without the shell's prefix (`[1760000000] stdout: `), which the desktop shell puts before each line of the core's output.
fn without_shell_prefix(line: &str) -> &str {
    let Some(rest) = line.strip_prefix('[') else { return line };
    let Some((secs, after)) = rest.split_once("] ") else { return line };
    if secs.is_empty() || !secs.bytes().all(|b| b.is_ascii_digit()) {
        return line;
    }
    after.strip_prefix("stdout: ").or_else(|| after.strip_prefix("stderr: ")).unwrap_or(line)
}

/// The line without colour codes (`ESC [ ... m`), which a log that went to a terminal once has in it.
fn without_colour(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains('\u{1b}') {
        return std::borrow::Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            for skipped in chars.by_ref() {
                if skipped.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    std::borrow::Cow::Owned(out)
}

/// What a line says about itself: its level, and whether it belongs to the run, when it is the first line of an event. `None` for a line that carries on the one before (the second
/// line of a message, a backtrace).
fn judge(line: &str, run: Option<&str>) -> Option<(Option<Level>, bool)> {
    let plain = without_colour(line);
    let body = without_shell_prefix(&plain);
    if body.starts_with('{') {
        if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(body) {
            let level = object.get("level").and_then(Value::as_str).and_then(Level::parse);
            let in_run = run.is_none_or(|id| {
                object.get("run_id").and_then(Value::as_str) == Some(id) || object.get("spans").and_then(Value::as_array).is_some_and(|spans| spans.iter().any(|s| s.get("run_id").and_then(Value::as_str) == Some(id)))
            });
            return Some((level, in_run));
        }
    }
    // The text format: `<time>  LEVEL target: message`.
    let mut words = body.split_whitespace();
    let _time = words.next()?;
    let level = Level::parse(words.next()?).filter(|_| body.split_whitespace().nth(1).is_some_and(|w| w.chars().all(|c| c.is_ascii_uppercase())))?;
    Some((Some(level), run.is_none_or(|id| names_id(body, id))))
}

/// The lines of `lines` that `query` keeps: the last `tail` of them. A line that carries on a kept event is kept with it.
pub fn select<I: Iterator<Item = String>>(lines: I, query: &Query) -> Vec<String> {
    let mut kept: VecDeque<String> = VecDeque::new();
    let mut carrying = false;
    for line in lines {
        let keep = match judge(&line, query.run.as_deref()) {
            Some((level, in_run)) => {
                carrying = in_run && query.level.is_none_or(|min| level.is_none_or(|l| l <= min));
                carrying
            }
            // A line that is not the start of an event goes with the event before it.
            None => carrying,
        };
        if keep {
            kept.push_back(line);
            if kept.len() > query.tail {
                kept.pop_front();
            }
        }
    }
    kept.into()
}

/// `aqua-cli logs ...`: the text to print. `default_file` is where the log is when `--file` and `AQUA_LOG_FILE` say nothing (the shell's `logs/core.log`).
pub fn run_command(args: &[String], default_file: &Path) -> Result<String, String> {
    let query = parse_args(args)?;
    let base = query.file.clone().or_else(|| std::env::var_os("AQUA_LOG_FILE").filter(|v| !v.is_empty()).map(PathBuf::from)).unwrap_or_else(|| default_file.to_path_buf());
    let files = files_of(&base);
    if files.is_empty() {
        return Err(format!("there is no log file at {} (set AQUA_LOG_FILE for a core you start by hand, or give --file)", base.display()));
    }
    let lines = files.into_iter().filter_map(|f| File::open(f).ok()).flat_map(|f| BufReader::new(f).split(b'\n').map_while(Result::ok).map(|bytes| String::from_utf8_lossy(&bytes).trim_end_matches('\r').to_string()));
    let chosen = select(lines, &query);
    let mut out = String::new();
    for line in &chosen {
        if query.json {
            let body = without_shell_prefix(line);
            let value = serde_json::from_str::<Value>(body).ok().filter(Value::is_object).unwrap_or_else(|| json!({ "text": line }));
            out.push_str(&value.to_string());
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if chosen.is_empty() {
        out.push_str("no line matches\n");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Sink {
        type Writer = Sink;

        fn make_writer(&'a self) -> Sink {
            self.clone()
        }
    }

    impl Sink {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
        }
    }

    /// What `f` logs, in `format`, at every level.
    fn logged(format: Format, f: impl FnOnce()) -> String {
        let sink = Sink::default();
        let s = subscriber(EnvFilter::new("trace"), vec![layer(format, sink.clone(), false)]);
        tracing::subscriber::with_default(s, f);
        sink.text()
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_logging_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn json_lines(text: &str) -> Vec<Value> {
        text.lines().map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}"))).collect()
    }

    #[test]
    fn a_json_line_for_an_event_in_a_run_and_a_tool_call_carries_the_ids_and_every_span_it_was_written_in() {
        let out = logged(Format::Json, || {
            let run = tracing::info_span!("run", run_id = "run_7_ab_1", route = "agent", caller = "api").entered();
            tracing::info!("the planner was asked");
            {
                let call = tracing::info_span!("tool", tool = "fs.read", call_id = tracing::field::Empty);
                call.record("call_id", "req_42");
                let _in = call.enter();
                tracing::info!(ok = true, ms = 12u64, decision = "auto", "tool call ended");
            }
            drop(run);
            tracing::warn!("after the run");
        });
        let lines = json_lines(&out);
        assert_eq!(lines.len(), 3, "{out}");
        assert_eq!((lines[0]["level"].as_str(), lines[0]["message"].as_str(), lines[0]["run_id"].as_str()), (Some("INFO"), Some("the planner was asked"), Some("run_7_ab_1")));
        assert!(lines[0]["ts"].as_str().is_some_and(|t| t.ends_with('Z') && t.len() >= 24) && lines[0]["target"].as_str().is_some());
        let call = &lines[1];
        assert_eq!((call["run_id"].as_str(), call["call_id"].as_str()), (Some("run_7_ab_1"), Some("req_42")), "{call}");
        assert_eq!((call["fields"]["ok"].as_bool(), call["fields"]["ms"].as_u64(), call["fields"]["decision"].as_str()), (Some(true), Some(12), Some("auto")));
        let spans = call["spans"].as_array().unwrap();
        assert_eq!(spans.iter().map(|s| s["name"].as_str().unwrap()).collect::<Vec<_>>(), vec!["run", "tool"], "from the outermost");
        assert_eq!((spans[0]["route"].as_str(), spans[0]["caller"].as_str(), spans[1]["tool"].as_str()), (Some("agent"), Some("api"), Some("fs.read")));
        assert!(lines[2].get("run_id").is_none() && lines[2].get("spans").is_none(), "a line outside any run says nothing of one: {}", lines[2]);
    }

    #[test]
    fn json_lines_stay_one_line_each_with_a_message_that_has_line_breaks_and_quotes_and_a_cut_when_it_is_huge() {
        let out = logged(Format::Json, || {
            tracing::error!("panic\nframe 1: a \"quoted\" name\n\tframe 2");
            tracing::info!(note = %"a\nb", "{}", "x".repeat(MAX_FIELD_CHARS * 2));
        });
        assert_eq!(out.lines().count(), 2, "one line per event");
        let lines = json_lines(&out);
        assert!(lines[0]["message"].as_str().unwrap().contains("frame 2") && lines[0]["message"].as_str().unwrap().contains("\"quoted\""));
        assert_eq!(lines[1]["fields"]["note"].as_str(), Some("a\nb"));
        assert!(lines[1]["message"].as_str().unwrap().chars().count() <= MAX_FIELD_CHARS + 3, "cut: {}", lines[1]["message"].as_str().unwrap().len());
    }

    #[test]
    fn the_text_format_shows_the_spans_in_front_of_the_message() {
        let out = logged(Format::Text, || {
            let _run = tracing::info_span!("run", run_id = "run_7_ab_1").entered();
            let _call = tracing::info_span!("tool", tool = "fs.read", call_id = "req_9").entered();
            tracing::info!("tool call ended");
        });
        assert!(out.contains("run{run_id=\"run_7_ab_1\"}:tool{tool=\"fs.read\" call_id=\"req_9\"}") && out.contains("tool call ended"), "{out}");
        assert!(!out.contains('\u{1b}'), "no colour codes when asked for none");
    }

    #[test]
    fn a_span_that_is_recorded_into_later_is_in_the_line() {
        let out = logged(Format::Json, || {
            let run = tracing::info_span!("run", run_id = "run_1", route = tracing::field::Empty, caller = tracing::field::Empty);
            let _in = run.enter();
            run.record("route", "agent").record("caller", "ui");
            tracing::info!("hello");
        });
        let line = &json_lines(&out)[0];
        assert_eq!((line["spans"][0]["route"].as_str(), line["spans"][0]["caller"].as_str()), (Some("agent"), Some("ui")), "{line}");
    }

    #[test]
    fn rust_log_still_wins_and_a_mistake_in_it_is_not_a_reason_to_have_no_log() {
        let shown = |filter: EnvFilter| {
            let sink = Sink::default();
            tracing::subscriber::with_default(subscriber(filter, vec![layer(Format::Text, sink.clone(), false)]), || {
                tracing::info!(target: "aqua_core::x", "core info");
                tracing::debug!(target: "aqua_core::x", "core debug");
                tracing::info!(target: "aqua_tool_host::host", "host info");
                tracing::info!(target: "some_other_crate", "other info");
                tracing::warn!(target: "some_other_crate", "other warn");
            });
            sink.text()
        };
        // The times differ from one run to the next: what is compared is the rest of each line.
        let shown = |filter: EnvFilter| shown(filter).lines().map(|l| l.split_once(' ').map_or(l, |(_, rest)| rest).to_string()).collect::<Vec<_>>().join("
");
        let by_default = shown(filter_for(None));
        assert!(by_default.contains("core info") && by_default.contains("host info") && by_default.contains("other warn"), "{by_default}");
        assert!(!by_default.contains("core debug") && !by_default.contains("other info"), "{by_default}");
        let from_rust_log = shown(filter_for(Some("some_other_crate=info,aqua_core=debug")));
        assert!(from_rust_log.contains("other info") && from_rust_log.contains("core debug"), "{from_rust_log}");
        assert!(!from_rust_log.contains("host info"), "RUST_LOG replaces the default, it does not add to it: {from_rust_log}");
        for bad in [None, Some(""), Some("   "), Some("aqua_core=loud")] {
            assert_eq!(shown(filter_for(bad)), by_default, "{bad:?}");
        }
    }

    #[test]
    fn the_settings_come_from_the_two_variables() {
        assert_eq!(LogConfig::from_values(None, None), LogConfig { format: Format::Text, file: None });
        assert_eq!(LogConfig::from_values(Some(" JSON "), Some(" C:/logs/core.log ")), LogConfig { format: Format::Json, file: Some(PathBuf::from("C:/logs/core.log")) });
        assert_eq!(LogConfig::from_values(Some("pretty"), Some("  ")), LogConfig { format: Format::Text, file: None });
    }

    #[test]
    fn the_file_is_started_again_at_its_size_and_there_are_never_more_than_keep_files_and_the_newest_lines_are_in_the_newest_file() {
        let dir = temp("rotate");
        let path = dir.join("core.log");
        let file = RotatingFile::open(&path, 200, 3, Format::Text).unwrap();
        let mut w = file.clone();
        for n in 0..60 {
            w.write_all(format!("line number {n:03} of the log, padded out a little\n").as_bytes()).unwrap();
        }
        let names: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        assert!(names.len() <= 3, "at most three files: {names:?}");
        assert!(names.iter().all(|n| n == "core.log" || n == "core.log.1" || n == "core.log.2"), "{names:?}");
        for n in &names {
            assert!(std::fs::metadata(dir.join(n)).unwrap().len() <= 200, "{n} is within the size");
        }
        let read = |n: &str| std::fs::read_to_string(dir.join(n)).unwrap();
        assert!(read("core.log").contains("line number 059"), "the newest line is in the file being written");
        assert!(read("core.log.1").contains("line number 05") && !read("core.log.1").contains("line number 059"), "the file just before it is `.1`");
        let all = files_of(&path).iter().map(|f| std::fs::read_to_string(f).unwrap()).collect::<String>();
        let numbers: Vec<usize> = all.lines().map(|l| l[12..15].parse().unwrap()).collect();
        assert!(numbers.windows(2).all(|p| p[0] < p[1]), "oldest file first, in order: {numbers:?}");
        assert_eq!(file.dropped(), 0);
        // A new writer on a file that is there goes on with the size it has: the next line starts the file over.
        drop(w);
        let mut again = RotatingFile::open(&path, 200, 3, Format::Text).unwrap();
        again.write_all(b"one more line
").unwrap();
        assert_eq!(read("core.log"), "one more line
");
        assert!(read("core.log.1").contains("line number 059"), "what was there is `.1` now");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_the_numbers_the_core_uses_there_are_never_more_than_five_files() {
        let dir = temp("five_files");
        let path = dir.join("core.log");
        let mut w = RotatingFile::open(&path, 300, KEEP_FILES, Format::Text).unwrap();
        for n in 0..400 {
            w.write_all(format!("2026-10-10T12:00:00Z  INFO aqua_core::x: line {n:04} of a log that rotates a good many times\n").as_bytes()).unwrap();
        }
        let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["core.log", "core.log.1", "core.log.2", "core.log.3", "core.log.4"], "{names:?}");
        let total: u64 = names.iter().map(|n| std::fs::metadata(dir.join(n)).unwrap().len()).sum();
        assert!(total <= 5 * 300, "the whole log stays under five files of the size: {total}");
        assert_eq!(files_of(&path).len(), 5);
        assert!(std::fs::read_to_string(&path).unwrap().contains("line 0399"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_cannot_be_renamed_is_written_on_and_a_file_that_is_one_line_per_cap_still_rotates() {
        let dir = temp("one_file");
        let path = dir.join("core.log");
        // keep = 1: no old files at all; the file is started again.
        let mut w = RotatingFile::open(&path, 100, 1, Format::Text).unwrap();
        for n in 0..20 {
            w.write_all(format!("a line of some length, number {n:02}\n").as_bytes()).unwrap();
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        assert!(std::fs::read_to_string(&path).unwrap().contains("number 19"));
        // A line longer than the cap is written whole (never cut), alone in its file.
        let long = format!("{}\n", "L".repeat(400));
        w.write_all(long.as_bytes()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), long);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn lines_are_dropped_and_counted_while_another_program_holds_the_file_and_the_writer_says_so_when_it_can_write_again() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = temp("locked");
        let path = dir.join("core.log");
        let mut w = RotatingFile::open(&path, 10_000, 3, Format::Text).unwrap().retry_after(Duration::from_millis(1));
        w.write_all(b"before the lock\n").unwrap();
        // The writer lets go of the file, as it does when it rotates; an editor then takes it for itself.
        w.state.lock().unwrap().file = None;
        let editor = OpenOptions::new().read(true).write(true).share_mode(0).open(&path).unwrap();
        for _ in 0..3 {
            w.write_all(b"while it is held\n").unwrap();
            std::thread::sleep(Duration::from_millis(3));
        }
        assert_eq!(w.dropped(), 3, "the three lines were counted, and nothing broke");
        drop(editor);
        std::thread::sleep(Duration::from_millis(5));
        w.write_all(b"after the lock\n").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("before the lock") && !text.contains("while it is held") && text.contains("after the lock"), "{text}");
        assert!(text.contains("3 log line(s) were dropped"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_cannot_be_opened_at_all_is_an_error_for_the_caller_who_falls_back_to_the_console() {
        let dir = temp("no_open");
        // A folder where the file should be.
        std::fs::create_dir_all(dir.join("core.log")).unwrap();
        assert!(RotatingFile::open(dir.join("core.log"), 100, 3, Format::Text).is_err());
        // A folder that is not there is made.
        assert!(RotatingFile::open(dir.join("deeper").join("still").join("core.log"), 100, 3, Format::Json).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rotated_json_log_written_through_the_layer_has_lines_that_parse_in_every_file() {
        let dir = temp("json_rotate");
        let path = dir.join("core.log");
        let file = RotatingFile::open(&path, 600, 4, Format::Json).unwrap();
        tracing::subscriber::with_default(subscriber(EnvFilter::new("info"), vec![layer(Format::Json, file, false)]), || {
            let _run = tracing::info_span!("run", run_id = "run_5_x_1").entered();
            for n in 0..40 {
                tracing::info!(n, "a line of the run");
            }
        });
        let files = files_of(&path);
        assert!(files.len() > 1 && files.len() <= 4, "{files:?}");
        let mut seen = 0;
        for f in &files {
            for line in json_lines(&std::fs::read_to_string(f).unwrap()) {
                assert_eq!(line["run_id"].as_str(), Some("run_5_x_1"));
                seen += 1;
            }
        }
        assert!((4..=40).contains(&seen), "the newest lines are kept and the oldest are gone: {seen}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn text_line(level: &str, rest: &str) -> String {
        format!("2026-10-10T12:00:00.123456Z {level:>5} aqua_core::x: {rest}")
    }

    #[test]
    fn the_logs_command_keeps_the_lines_of_one_run_in_text_and_in_json_with_their_continuation_lines() {
        let lines = vec![
            text_line("INFO", "run{run_id=\"run_7_ab_1\" route=\"agent\"}: step one"),
            text_line("INFO", "run{run_id=\"run_7_ab_10\"}: a different run, whose id starts the same"),
            text_line("ERROR", "run{run_id=\"run_7_ab_1\"}:tool{tool=\"fs.read\"}: panic"),
            "frame 1: a::b".to_string(),
            "   0: frame_a".to_string(),
            "[1760000000] stdout: ".to_string() + &text_line("WARN", "run{run_id=run_7_ab_1}: the shell wrote this one"),
            text_line("INFO", "nothing to do with a run"),
            json!({"ts": "t", "level": "INFO", "message": "json line", "run_id": "run_7_ab_1", "spans": [{"name": "run", "run_id": "run_7_ab_1"}]}).to_string(),
            json!({"ts": "t", "level": "INFO", "message": "json line of another", "run_id": "run_7_ab_10"}).to_string(),
            json!({"ts": "t", "level": "INFO", "message": "only in a span", "spans": [{"name": "run", "run_id": "run_7_ab_1"}]}).to_string(),
            "[1760000001] stdout: ".to_string() + &json!({"ts": "t", "level": "DEBUG", "message": "shell-wrapped json", "run_id": "run_7_ab_1"}).to_string(),
        ];
        let q = |run: Option<&str>, level: Option<Level>, tail: usize| Query { run: run.map(str::to_string), level, tail, json: false, file: None };
        let mine = select(lines.clone().into_iter(), &q(Some("run_7_ab_1"), None, 100));
        let said: Vec<&str> = mine.iter().map(String::as_str).collect();
        assert_eq!(said.len(), 8, "{said:#?}");
        assert!(said[0].contains("step one") && said[1].contains("panic") && said[2].contains("frame 1") && said[3].contains("frame_a"), "the continuation goes with its event: {said:#?}");
        assert!(said.iter().any(|l| l.contains("shell wrote this one")) && said.iter().any(|l| l.contains("only in a span")) && said.iter().any(|l| l.contains("shell-wrapped json")));
        assert!(!said.iter().any(|l| l.contains("run_7_ab_10") || l.contains("nothing to do")), "a longer id and a line of no run are not in it");
        // Level: warn and above; the lines of an event that was not kept are not kept either.
        let loud = select(lines.clone().into_iter(), &q(Some("run_7_ab_1"), Some(Level::Warn), 100));
        assert_eq!(loud.len(), 4, "{loud:#?}");
        assert!(loud[0].contains("ERROR") && loud[1].contains("frame 1") && loud[2].contains("frame_a") && loud[3].contains("WARN"));
        // Tail: the last N that match.
        let last_two = select(lines.clone().into_iter(), &q(Some("run_7_ab_1"), None, 2));
        assert_eq!(last_two.len(), 2);
        assert!(last_two[0].contains("only in a span") && last_two[1].contains("shell-wrapped json"));
        // No run given: everything at the level (the debug line is below it, and the continuation lines go with the error).
        assert_eq!(select(lines.into_iter(), &q(None, Some(Level::Info), 100)).len(), 10);
    }

    #[test]
    fn the_logs_command_reads_the_rotated_files_oldest_first_prints_json_objects_and_says_when_nothing_matches() {
        let dir = temp("command");
        let base = dir.join("core.log");
        std::fs::write(dir.join("core.log.2"), format!("{}\n", text_line("INFO", "run{run_id=run_1_a_1}: oldest"))).unwrap();
        std::fs::write(dir.join("core.log.1"), format!("{}\r\n", text_line("INFO", "run{run_id=run_1_a_1}: middle"))).unwrap();
        std::fs::write(&base, format!("{}\n{}\n", json!({"level": "INFO", "message": "newest", "run_id": "run_1_a_1"}), text_line("INFO", "run{run_id=run_2_a_1}: other run"))).unwrap();
        let args = |s: &str| s.split_whitespace().map(str::to_string).collect::<Vec<_>>();
        let out = run_command(&args("--run run_1_a_1"), &base).unwrap();
        let order: Vec<&str> = out.lines().collect();
        assert_eq!(order.len(), 3);
        assert!(order[0].contains("oldest") && order[1].contains("middle") && order[2].contains("newest"), "{out}");
        assert!(!out.contains('\r'));
        let as_json = run_command(&args("--run run_1_a_1 --json"), &base).unwrap();
        for line in as_json.lines() {
            assert!(serde_json::from_str::<Value>(line).unwrap().is_object(), "{line}");
        }
        assert!(as_json.lines().next().unwrap().contains("\"text\""), "a text line is wrapped: {as_json}");
        assert!(as_json.lines().last().unwrap().contains("\"run_id\":\"run_1_a_1\""), "a JSON line is passed on as it is: {as_json}");
        assert_eq!(run_command(&args("--run nobody"), &base).unwrap(), "no line matches\n");
        let err = run_command(&args("--run x"), &dir.join("missing.log")).unwrap_err();
        assert!(err.contains("no log file"), "{err}");
        // `--file` beats the default.
        let elsewhere = dir.join("other.log");
        std::fs::write(&elsewhere, format!("{}\n", text_line("WARN", "only here"))).unwrap();
        let out = run_command(&["--file".into(), elsewhere.display().to_string(), "--level".into(), "warn".into()], &base).unwrap();
        assert!(out.contains("only here"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_line_with_colour_codes_is_still_told_by_its_level_and_its_run() {
        let coloured = "\u{1b}[2m2026-10-10T12:00:00Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m \u{1b}[2maqua_core::x\u{1b}[0m: run{run_id=run_3_c_1}: coloured".to_string();
        let q = Query { run: Some("run_3_c_1".into()), level: Some(Level::Warn), tail: 10, json: false, file: None };
        assert!(select(vec![coloured.clone()].into_iter(), &q).is_empty(), "an INFO line is below warn");
        let q = Query { level: Some(Level::Info), ..q };
        assert_eq!(select(vec![coloured].into_iter(), &q).len(), 1);
    }

    #[test]
    fn the_options_of_the_logs_command_are_checked() {
        let args = |s: &str| s.split_whitespace().map(str::to_string).collect::<Vec<_>>();
        let q = parse_args(&args("--run run_9 --level debug --tail 5 --json")).unwrap();
        assert_eq!((q.run.as_deref(), q.level, q.tail, q.json), (Some("run_9"), Some(Level::Debug), 5, true));
        assert_eq!(parse_args(&[]).unwrap().tail, DEFAULT_TAIL);
        for (bad, says) in [("--tail 0", "number"), ("--tail many", "number"), ("--level loud", "level"), ("--run", "needs"), ("--verbose", "not an option")] {
            let e = parse_args(&args(bad)).unwrap_err();
            assert!(e.contains(says), "{bad}: {e}");
        }
        assert!(Level::Error < Level::Warn && Level::Warn < Level::Info && Level::Info < Level::Debug && Level::Debug < Level::Trace);
        assert!(names_id("run_id=run_7_ab_1 x", "run_7_ab_1") && names_id("(run_7_ab_1)", "run_7_ab_1") && !names_id("run_7_ab_10", "run_7_ab_1") && !names_id("xrun_7_ab_1", "run_7_ab_1"));
        assert!(names_id("run_7_ab_10 run_7_ab_1", "run_7_ab_1"), "found after a longer one");
    }

    /// The spans of a run and of a tool call, through the real `RunScope` and `ToolHost::call` (card OA3-04, EX-15).
    mod spans {
        use super::*;
        use aqua_tool_host::*;
        use serde_json::json;

        struct Probe;

        impl ToolProvider for Probe {
            fn id(&self) -> &str {
                "probe"
            }

            fn manifests(&self) -> Vec<ToolManifest> {
                vec![ToolManifest {
                    name: "probe.echo".into(),
                    title: "probe.echo".into(),
                    description: "Echoes.".into(),
                    classes: vec![PermClass::Read],
                    default_decision: Decision::Auto,
                    input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}, "note": {"type": "string"}}}),
                    timeout_default_ms: 5_000,
                    timeout_max_ms: 5_000,
                    annotations: Annotations { read_only: true, destructive: false, idempotent: true, open_world: false },
                    untrusted_output: false,
                }]
            }

            fn classify(&self, tool: &str, _args: &Value) -> Result<Classification, ToolError> {
                Ok(Classification { classes: vec![PermClass::Read], target: "x".into(), summary: tool.into(), target_allowed: true })
            }

            fn call<'a>(&'a self, _tool: &'a str, args: Value, _ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
                // The result echoes what it was given, as a tool that reads a file with a secret in it would.
                Box::pin(async move { Ok(ProviderOutput { text: format!("echo {args}"), ..Default::default() }) })
            }
        }

        fn host(name: &str) -> Arc<ToolHost> {
            let dir = temp(name);
            let host = ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(dir.join("audit.jsonl"))));
            host.register(Arc::new(Probe)).unwrap();
            Arc::new(host)
        }

        fn call(host: &ToolHost, tool: &str, args: Value, run: &str) -> ToolResult {
            let mut ctx = RequestContext::new(Caller::Agent);
            ctx.agent_run_id = run.to_string();
            host.call_blocking(ToolRequest { tool: tool.into(), arguments: args, context: ctx })
        }

        #[test]
        fn a_tool_call_is_written_inside_its_run_and_its_closing_line_has_the_ids_the_result_and_no_arguments_and_no_secret() {
            let secret = "s3cr3t-token-value-0451";
            let host = host("tool_span");
            host.register_secret(secret);
            let mut run_id = String::new();
            let out = logged(Format::Json, || {
                let scope = crate::agent_runs::scope::RunScope::begin(Some(host.clone()));
                scope.describe("agent", "api");
                run_id = scope.id().to_string();
                tracing::info!("the planner was asked");
                let ok = call(&host, "probe.echo", json!({"path": "C:/private/notes.txt", "note": secret}), scope.id());
                assert!(ok.ok, "{ok:?}");
                let refused = call(&host, "probe.echo", json!({"path": 5}), scope.id());
                assert!(!refused.ok);
            });
            let lines = json_lines(&out);
            assert!(lines.iter().all(|l| l["run_id"].as_str() == Some(run_id.as_str())), "every line of the run names it: {out}");
            let ends: Vec<&Value> = lines.iter().filter(|l| l["message"] == "tool call ended").collect();
            assert_eq!(ends.len(), 2, "{out}");
            let good = ends[0];
            assert_eq!((good["fields"]["ok"].as_bool(), good["fields"]["decision"].as_str(), good["target"].as_str().map(|t| t.starts_with("aqua_tool_host"))), (Some(true), Some("auto"), Some(true)), "{good}");
            assert!(good["fields"]["ms"].is_u64() && good["fields"].get("error_code").is_none(), "{good}");
            let call_id = good["call_id"].as_str().unwrap();
            assert!(call_id.starts_with("req"), "{good}");
            let spans = good["spans"].as_array().unwrap();
            assert_eq!(spans.iter().map(|s| s["name"].as_str().unwrap()).collect::<Vec<_>>(), vec!["run", "tool"], "{good}");
            assert_eq!((spans[0]["route"].as_str(), spans[0]["caller"].as_str(), spans[1]["tool"].as_str(), spans[1]["run_id"].as_str()), (Some("agent"), Some("api"), Some("probe.echo"), Some(run_id.as_str())), "{good}");
            let bad = ends[1];
            assert_eq!((bad["fields"]["ok"].as_bool(), bad["fields"]["decision"].as_str(), bad["fields"]["error_code"].as_str()), (Some(false), Some("invalid"), Some("INVALID_ARGUMENTS")), "{bad}");
            assert_ne!(bad["call_id"], good["call_id"], "a call of its own");
            // What the call was given and what it returned is nowhere in the log.
            assert!(!out.contains(secret) && !out.contains("notes.txt") && !out.contains("echo "), "{out}");
        }

        #[test]
        fn a_tool_name_with_line_breaks_cannot_start_a_line_of_its_own_in_the_text_log_and_is_cut() {
            let host = host("tool_name");
            let out = logged(Format::Text, || {
                let _ = call(&host, "evil\n2026-01-01T00:00:00Z  INFO aqua_core::fake: forged line", json!({}), "");
                let _ = call(&host, &"x".repeat(5000), json!({}), "");
            });
            // Two calls, two lines: what the name says is inside the line of its own call, and no line of the log starts with the forged time.
            assert_eq!(out.lines().count(), 2, "{out}");
            assert!(!out.lines().any(|l| l.starts_with("2026-01-01")), "no forged line: {out}");
            assert!(out.lines().all(|l| l.len() < 1000), "a long name is cut: {}", out.lines().map(str::len).max().unwrap_or(0));
            assert!(out.contains("tool call ended") && out.contains("unknown_tool"), "{out}");
        }

        #[test]
        fn a_call_outside_a_run_has_no_run_in_its_line() {
            let host = host("tool_no_run");
            let out = logged(Format::Json, || {
                let r = call(&host, "probe.echo", json!({}), "");
                assert!(r.ok);
            });
            let line = json_lines(&out).into_iter().find(|l| l["message"] == "tool call ended").unwrap();
            assert!(line.get("run_id").is_none() && line["call_id"].is_string(), "{line}");
        }
    }
}
