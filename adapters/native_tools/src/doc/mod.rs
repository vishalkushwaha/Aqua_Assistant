//! `doc.read` and `doc.search`: read PDF, Word, PowerPoint, Excel and CSV files as text.
//!
//! A document is a file somebody else may have made, so it is read as **untrusted text** and nothing in it is ever run: a macro is a part
//! of the file that is not opened, a link is not followed, an embedded file is not extracted. Files are read through the same folder
//! guard as the file tools (only the allowed folders), and everything is capped: the size of the file, how much a zip may unpack to, the
//! number of pages and sheets and rows, and the text returned.
//!
//! The text has markers (`--- page 3 ---`, `--- slide 2 ---`, `--- sheet Sales ---`) so an answer can say where it found something, and
//! `doc.search` finds the parts of a long document that answer a question (BM25 over pieces of it) instead of the model reading all of it.

mod docx;
mod pdf;
mod pptx;
mod rank;
mod sheets;

use crate::fs::guard::{display, Op, PathGuard};
use aqua_tool_host::{
    Annotations, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider,
};
use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

/// The biggest file that is opened.
pub const MAX_FILE_BYTES: u64 = 25 << 20;
/// A zip (Word, PowerPoint, Excel) may unpack to this much at most, in all, and no entry to more than `MAX_ENTRY_BYTES`.
pub const MAX_UNPACKED_BYTES: u64 = 120 << 20;
pub const MAX_ENTRY_BYTES: u64 = 40 << 20;
pub const MAX_ENTRIES: usize = 5_000;
/// An entry that unpacks to more than this many times its packed size is a bomb.
pub const MAX_RATIO: u64 = 250;
pub const MAX_UNITS: usize = 400;
/// How much text is kept of one document, and how much one `doc.read` answers.
pub const MAX_TEXT_CHARS: usize = 3_000_000;
pub const DEFAULT_READ_CHARS: usize = 8_000;
pub const MAX_READ_CHARS: usize = 24_000;
/// The size of the pieces `doc.search` ranks.
pub const CHUNK_CHARS: usize = 1_200;

/// One part of a document: a page, a slide, a sheet, or a stretch of a long text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub label: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub kind: &'static str,
    pub units: Vec<Unit>,
    /// The text or the number of parts was cut at a cap.
    pub truncated: bool,
}

fn bad(m: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArguments, m)
}

/// The kinds that are read, by extension.
pub fn kind_of(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "pdf" => Some("pdf"),
        "docx" | "docm" => Some("docx"),
        "pptx" | "pptm" => Some("pptx"),
        "xlsx" | "xlsm" | "xls" | "ods" => Some("sheet"),
        "csv" | "tsv" => Some("csv"),
        _ => None,
    }
}

/// Read a file's bytes only when it is a regular file of a size that may be opened.
fn read_capped(path: &Path) -> Result<Vec<u8>, ToolError> {
    let meta = std::fs::metadata(path).map_err(|e| ToolError::new(if e.kind() == std::io::ErrorKind::NotFound { ErrorCode::NotFound } else { ErrorCode::BackendError }, format!("{}: {e}", display(path))))?;
    if !meta.is_file() {
        return Err(bad(format!("{} is not a file", display(path))));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(ToolError::new(ErrorCode::ResourceLimit, format!("{} is larger than {} MB", display(path), MAX_FILE_BYTES >> 20)));
    }
    std::fs::read(path).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("{}: {e}", display(path))))
}

/// An archive of Word, PowerPoint or Excel, opened with the limits that keep a zip bomb out.
pub(crate) struct Archive {
    zip: zip::ZipArchive<std::io::Cursor<Vec<u8>>>,
    unpacked: u64,
}

impl Archive {
    pub(crate) fn open(bytes: Vec<u8>) -> Result<Self, ToolError> {
        let zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| bad(format!("this is not a valid Office file: {e}")))?;
        if zip.len() > MAX_ENTRIES {
            return Err(ToolError::new(ErrorCode::ResourceLimit, format!("the archive has {} parts, more than the {MAX_ENTRIES} that are read", zip.len())));
        }
        Ok(Self { zip, unpacked: 0 })
    }

    /// Look at every part's declared sizes (without unpacking any): too many bytes in all, or one that unpacks to far more than it packs, is a bomb.
    pub(crate) fn check_all(&mut self) -> Result<(), ToolError> {
        let mut total: u64 = 0;
        for i in 0..self.zip.len() {
            let f = self.zip.by_index_raw(i).map_err(|e| bad(format!("a part of the archive cannot be read: {e}")))?;
            let (packed, size) = (f.compressed_size().max(1), f.size());
            if size > MAX_ENTRY_BYTES || (size > (1 << 20) && size / packed > MAX_RATIO) {
                return Err(ToolError::new(ErrorCode::ResourceLimit, format!("a part unpacks to {} MB from {} KB: not read", size >> 20, packed >> 10)));
            }
            total += size;
            if total > MAX_UNPACKED_BYTES {
                return Err(ToolError::new(ErrorCode::ResourceLimit, format!("the archive unpacks to more than {} MB: not read", MAX_UNPACKED_BYTES >> 20)));
            }
        }
        Ok(())
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.zip.file_names().map(str::to_string).collect()
    }

    /// One part's text, if it is there. A part that unpacks to too much, or far more than it packs from, is refused.
    pub(crate) fn text(&mut self, name: &str) -> Result<Option<String>, ToolError> {
        let Ok(file) = self.zip.by_name(name) else { return Ok(None) };
        let (packed, size) = (file.compressed_size().max(1), file.size());
        if size > MAX_ENTRY_BYTES || size / packed > MAX_RATIO && size > (1 << 20) {
            return Err(ToolError::new(ErrorCode::ResourceLimit, format!("the part {name} unpacks to {} MB from {} KB: not read", size >> 20, packed >> 10)));
        }
        if self.unpacked + size > MAX_UNPACKED_BYTES {
            return Err(ToolError::new(ErrorCode::ResourceLimit, format!("the archive unpacks to more than {} MB: not read", MAX_UNPACKED_BYTES >> 20)));
        }
        let mut buf = Vec::new();
        // Read at most what the entry said it is plus a little: a header that lies about the size does not get past the cap.
        file.take(MAX_ENTRY_BYTES + 1).read_to_end(&mut buf).map_err(|e| bad(format!("{name} cannot be read: {e}")))?;
        if buf.len() as u64 > MAX_ENTRY_BYTES {
            return Err(ToolError::new(ErrorCode::ResourceLimit, format!("the part {name} unpacks to more than {} MB: not read", MAX_ENTRY_BYTES >> 20)));
        }
        self.unpacked += buf.len() as u64;
        Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
    }
}

/// Parse a part of an Office file as XML: no DTD (so no entity tricks) and a cap on how many nodes.
pub(crate) fn parse_xml(xml: &str) -> Result<roxmltree::Document<'_>, ToolError> {
    let opts = roxmltree::ParsingOptions { allow_dtd: false, nodes_limit: 4_000_000 };
    roxmltree::Document::parse_with_options(xml, opts).map_err(|e| bad(format!("a part of the file is not valid XML: {e}")))
}

/// Cut a long text into units of about `chunk` characters at line ends: (`label`, text).
pub(crate) fn split_text(label: &str, text: &str, chunk: usize) -> Vec<Unit> {
    let mut units = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        if !cur.is_empty() && cur.chars().count() + line.chars().count() + 1 > chunk {
            units.push(Unit { label: format!("{label} {}", units.len() + 1), text: std::mem::take(&mut cur) });
        }
        cur.push_str(line);
        cur.push('\n');
    }
    if !cur.trim().is_empty() {
        units.push(Unit { label: format!("{label} {}", units.len() + 1), text: cur });
    }
    units
}

/// Read a document: its kind is from its extension.
pub fn extract(path: &Path) -> Result<Document, ToolError> {
    let kind = kind_of(path).ok_or_else(|| bad(format!("{} is not a file this reads (pdf, docx, pptx, xlsx, xls, ods, csv, tsv)", display(path))))?;
    let bytes = read_capped(path)?;
    let mut doc = match kind {
        "pdf" => pdf::extract(&bytes)?,
        "docx" => docx::extract(bytes)?,
        "pptx" => pptx::extract(bytes)?,
        "sheet" => sheets::extract_workbook(bytes)?,
        _ => sheets::extract_csv(&bytes, path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("tsv")))?,
    };
    // Caps on the whole: how many parts, and how much text.
    if doc.units.len() > MAX_UNITS {
        doc.units.truncate(MAX_UNITS);
        doc.truncated = true;
    }
    let mut total = 0usize;
    for (i, u) in doc.units.iter_mut().enumerate() {
        let n = u.text.chars().count();
        if total + n > MAX_TEXT_CHARS {
            u.text = u.text.chars().take(MAX_TEXT_CHARS.saturating_sub(total)).collect();
            doc.units.truncate(i + 1);
            doc.truncated = true;
            break;
        }
        total += n;
    }
    Ok(doc)
}

/// Which parts a `pages` argument names: `3`, `2-4`, or `1,3,5-6` (1-based). `None` or empty means all.
pub fn parse_range(spec: &str, count: usize) -> Result<Vec<usize>, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok((0..count).collect());
    }
    let mut out: Vec<usize> = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => (a.trim().parse::<usize>(), b.trim().parse::<usize>()),
            None => {
                let n = part.parse::<usize>();
                (n.clone(), n)
            }
        };
        let (Ok(a), Ok(b)) = (a, b) else { return Err(format!("{part:?} is not a page or a range such as 3 or 2-4")) };
        if a == 0 || b < a || a > count {
            return Err(format!("{part:?} is outside the document, which has {count} part(s)"));
        }
        for n in a..=b.min(count) {
            if !out.contains(&(n - 1)) {
                out.push(n - 1);
            }
        }
    }
    Ok(out)
}

/// The text of the chosen parts with markers, cut to `max_chars` with a note when it was.
pub fn render(doc: &Document, which: &[usize], max_chars: usize) -> (String, bool) {
    let mut out = String::new();
    let mut cut = false;
    for &i in which {
        let u = &doc.units[i];
        let block = format!("--- {} ---\n{}\n", u.label, u.text.trim_end());
        if out.chars().count() + block.chars().count() > max_chars {
            let room = max_chars.saturating_sub(out.chars().count());
            out.extend(block.chars().take(room));
            out.push_str("\n… [cut: ask for fewer pages, or use doc.search]");
            cut = true;
            break;
        }
        out.push_str(&block);
    }
    (out, cut)
}

/// The tools `doc.read` and `doc.search`.
pub struct DocProvider {
    guard: Arc<PathGuard>,
}

impl DocProvider {
    pub fn new(guard: Arc<PathGuard>) -> Self {
        Self { guard }
    }
}

impl ToolProvider for DocProvider {
    fn id(&self) -> &str {
        "doc"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        let m = |name: &str, description: &str, schema: Value| ToolManifest {
            name: name.into(),
            title: name.into(),
            description: description.into(),
            classes: vec![PermClass::Read],
            default_decision: Decision::Auto,
            input_schema: schema,
            timeout_default_ms: 30_000,
            timeout_max_ms: 60_000,
            annotations: Annotations { read_only: true, destructive: false, idempotent: true, open_world: false },
            untrusted_output: true,
        };
        vec![
            m(
                "doc.read",
                "Read a PDF, Word, PowerPoint, Excel or CSV file as text, with page markers. pages: 3, 2-4 or 1,3,5-6.",
                json!({"type": "object", "properties": {"path": {"type": "string", "minLength": 1, "maxLength": 1024}, "pages": {"type": "string", "maxLength": 100}, "max_chars": {"type": "integer", "minimum": 500, "maximum": MAX_READ_CHARS, "default": DEFAULT_READ_CHARS}}, "required": ["path"]}),
            ),
            m(
                "doc.search",
                "Find the parts of a long PDF, Word, PowerPoint or Excel file that answer a question, with where they are.",
                json!({"type": "object", "properties": {"path": {"type": "string", "minLength": 1, "maxLength": 1024}, "query": {"type": "string", "minLength": 1, "maxLength": 300}, "limit": {"type": "integer", "minimum": 1, "maximum": 6, "default": 3}}, "required": ["path", "query"]}),
            ),
        ]
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        let raw = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let r = self.guard.resolve(raw, Op::Read)?;
        let mut classes = vec![];
        if r.sensitive {
            classes.push(PermClass::Credential);
        }
        Ok(Classification { classes, target: display(&r.path), summary: format!("{} {}", tool.trim_start_matches("doc."), display(&r.path)), target_allowed: true })
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, _ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let guard = self.guard.clone();
        let tool = tool.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || run(&guard, &tool, &args))
                .await
                .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("document task failed: {e}")))?
        })
    }
}

fn run(guard: &PathGuard, tool: &str, args: &Value) -> Result<ProviderOutput, ToolError> {
    let raw = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let r = guard.resolve(raw, Op::Read)?;
    // A file extractor is a parser of somebody else's bytes: a crash in it is an error for this call, not for the program.
    let path = r.path.clone();
    let doc = std::panic::catch_unwind(move || extract(&path)).map_err(|_| ToolError::new(ErrorCode::BackendError, "this file could not be read (the reader failed on it)"))??;
    let private = !r.trusted || r.sensitive;
    let mut out = match tool {
        "doc.read" => {
            let which = parse_range(args.get("pages").and_then(|v| v.as_str()).unwrap_or(""), doc.units.len()).map_err(bad)?;
            if which.is_empty() {
                return Err(bad("the document has nothing in it to read"));
            }
            let max = args.get("max_chars").and_then(|v| v.as_u64()).map_or(DEFAULT_READ_CHARS, |n| n as usize).clamp(500, MAX_READ_CHARS);
            let (text, cut) = render(&doc, &which, max);
            let note = if doc.truncated { " The document was longer than what is read at most." } else { "" };
            ProviderOutput { data: json!({"path": display(&r.path), "kind": doc.kind, "parts": doc.units.len(), "shown": which.len(), "cut": cut || doc.truncated}), text: format!("{} ({}, {} part(s)){note}\n{text}", display(&r.path), doc.kind, doc.units.len()), ..Default::default() }
        }
        _ => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let limit = args.get("limit").and_then(|v| v.as_u64()).map_or(3, |n| n as usize).clamp(1, 6);
            let hits = rank::search(&doc, query, limit);
            if hits.is_empty() {
                ProviderOutput { data: json!({"path": display(&r.path), "hits": []}), text: format!("Nothing in {} fits \"{}\".", display(&r.path), query.chars().take(80).collect::<String>()), ..Default::default() }
            } else {
                let text = hits.iter().map(|h| format!("--- {} ---\n{}", h.label, h.text.trim_end())).collect::<Vec<_>>().join("\n");
                ProviderOutput { data: json!({"path": display(&r.path), "hits": hits.iter().map(|h| h.label.clone()).collect::<Vec<_>>()}), text, ..Default::default() }
            }
        }
    };
    out.private = private;
    Ok(out)
}
