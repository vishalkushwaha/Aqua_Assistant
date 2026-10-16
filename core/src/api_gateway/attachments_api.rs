//! `POST /api/v1/attachments`: a file the person drops on the app (a PDF, a spreadsheet, a picture) is kept in a folder of its own, where the file tools can read it
//! (card R4-03). The upload is the raw bytes of the file as the body; the name is `?name=report.pdf` (or the header `X-Aqua-Filename`). The UI token is required: a client with
//! the API token can start runs and must not be able to put files in front of them.
//!
//! * **Size.** At most `AQUA_ATTACHMENT_MAX_MB` megabytes a file (25 by default, 1 to 200), and `AQUA_ATTACHMENT_STORE_MB` megabytes in all (512 by default): over either
//!   the file is refused (413, 507) and nothing is written. The disk may be nearly full, so the store also forgets what it kept more than 30 days ago.
//! * **Hash.** The SHA-256 of the file is its identity: it is the answer's `sha256`, and the first 16 characters name the folder it is kept in, so the same file sent twice is
//!   kept once.
//! * **Name.** The name is cut to its last part, characters Windows does not allow become `_`, device names (`CON`, `NUL` ...) get a `_` in front, and a name is at most 100
//!   characters. Files that Windows runs when they are opened (`.exe`, `.bat`, `.ps1`, `.lnk`, `.js` ...) are refused (415): an attachment is something to read.
//! * **Where.** Under the data folder (`data/`) nothing can be read by the file tools: the whole folder is Aqua's own (`tool_host::protected_paths`). So the files go in the
//!   `attachments` folder of the first read-write folder the file tools may use (the workspace), and `AQUA_ATTACHMENTS_DIR` puts them somewhere else. Only when the file tools are
//!   off do they go under `data/attachments`, and the answer then says they cannot be read with `doc.read`. The answer gives the path as the file tools take it (`tool_path`).
//! * **What an attachment is.** A file from outside: what is inside it is data and not instructions. `doc.read` fences it as text from outside and marks the run, as it does
//!   for any file. The upload route reads nothing out of the file and runs nothing.
//!
//! The route is merged into the router of `route_api.rs` with the routes of the voice choice (`voice_choice_api.rs`), the other half of the same card.

use super::auth::{ui_scope_required, Scope};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Query},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use aqua_native_tools::fs::{Access, Op, PathGuard};
use aqua_tool_host::hex_lower;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// A file of this name is in every folder this module made, so that only those are ever counted or forgotten.
const MARKER: &str = ".aqua-attachment";
const MAX_NAME_CHARS: usize = 100;
const KEEP_DAYS: u64 = 30;

/// What is allowed, from the environment.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_bytes: u64,
    pub store_bytes: u64,
    pub keep: Duration,
}

impl Limits {
    pub fn from_env() -> Self {
        let mb = |name: &str, default: u64, lo: u64, hi: u64| std::env::var(name).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(default).clamp(lo, hi);
        Self { max_bytes: mb("AQUA_ATTACHMENT_MAX_MB", 25, 1, 200) * 1024 * 1024, store_bytes: mb("AQUA_ATTACHMENT_STORE_MB", 512, 10, 20_000) * 1024 * 1024, keep: Duration::from_secs(KEEP_DAYS * 86_400) }
    }
}

/// Why a file was not kept.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    Empty,
    TooBig { limit: u64 },
    /// A kind of file Windows runs when it is opened.
    Program(String),
    StoreFull { used: u64, cap: u64 },
    Io(String),
}

impl Refusal {
    fn status(&self) -> StatusCode {
        match self {
            Refusal::Empty => StatusCode::BAD_REQUEST,
            Refusal::TooBig { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Refusal::Program(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Refusal::StoreFull { .. } => StatusCode::INSUFFICIENT_STORAGE,
            Refusal::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Refusal::Empty => "the file is empty".to_string(),
            Refusal::TooBig { limit } => format!("the file is over the {} MB an attachment may be (AQUA_ATTACHMENT_MAX_MB)", limit / 1024 / 1024),
            Refusal::Program(ext) => format!("a .{ext} file is a program or a script that Windows runs when it is opened, and Aqua does not keep those as attachments; an attachment is something to read"),
            Refusal::StoreFull { used, cap } => format!("the attachments already take {} MB of the {} MB they may (AQUA_ATTACHMENT_STORE_MB); nothing was written", used / 1024 / 1024, cap / 1024 / 1024),
            Refusal::Io(why) => format!("the file could not be stored: {why}"),
        }
    }
}

/// A file that was kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub sha256: String,
    pub path: PathBuf,
}

/// Extensions of files that Windows runs (or hands to a script host) when they are opened.
const PROGRAMS: &[&str] = &[
    "exe", "com", "scr", "pif", "msi", "msp", "bat", "cmd", "ps1", "psm1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "hta", "lnk", "url", "reg", "jar", "cpl", "dll", "sys", "appx", "msix", "gadget",
];

const DEVICE_NAMES: &[&str] = &["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];

/// The name a file is kept under: the last part of what was given, with nothing Windows would change or refuse. Never empty.
pub fn safe_file_name(raw: &str) -> String {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = last.chars().filter(|c| !c.is_control()).map(|c| if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') { '_' } else { c }).collect();
    let cleaned = cleaned.trim_matches(|c: char| c == ' ' || c == '.').to_string();
    let mut name = if cleaned.is_empty() { "attachment".to_string() } else { cleaned };
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let numbered_device = (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4 && stem.as_bytes()[3].is_ascii_digit();
    if DEVICE_NAMES.contains(&stem.as_str()) || numbered_device {
        name.insert(0, '_');
    }
    if name.chars().count() > MAX_NAME_CHARS {
        // Keep the extension (a short one), cut the middle.
        let (stem, ext) = match name.rsplit_once('.') {
            Some((s, e)) if e.chars().count() <= 10 && !s.is_empty() => (s.to_string(), format!(".{e}")),
            _ => (name.clone(), String::new()),
        };
        let keep = MAX_NAME_CHARS.saturating_sub(ext.chars().count());
        name = format!("{}{ext}", stem.chars().take(keep).collect::<String>());
    }
    name
}

/// The extension of a name, lower case, without the dot.
fn extension_of(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

/// What kind of file this is by its name, for the answer: `(kind, read with)`.
pub fn kind_and_reader(name: &str) -> (&'static str, Option<&'static str>) {
    if aqua_native_tools::doc::kind_of(Path::new(name)).is_some() {
        return ("document", Some("doc.read"));
    }
    match extension_of(name).as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "tif" | "tiff" => ("image", None),
        "txt" | "md" | "json" | "xml" | "html" | "htm" | "log" | "yaml" | "yml" | "toml" | "ini" | "rs" | "py" | "rtf" => ("text", Some("fs.read")),
        _ => ("other", None),
    }
}

/// The folders this module made in `root` (those with the marker): `(folder, bytes in it, newest change)`.
fn kept_folders(root: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else { return out };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() || !dir.join(MARKER).is_file() {
            continue;
        }
        let (mut bytes, mut newest) = (0u64, SystemTime::UNIX_EPOCH);
        if let Ok(files) = std::fs::read_dir(&dir) {
            for f in files.flatten() {
                if let Ok(meta) = f.metadata() {
                    if meta.is_file() {
                        bytes += meta.len();
                        newest = newest.max(meta.modified().unwrap_or(SystemTime::UNIX_EPOCH));
                    }
                }
            }
        }
        out.push((dir, bytes, newest));
    }
    out
}

/// Keeps `bytes` under `name` in `root`, or says why not. `now` is the time the age of what is already kept is measured against (a test passes a later one).
pub fn store_attachment(root: &Path, name: &str, bytes: &[u8], limits: &Limits, now: SystemTime) -> Result<Stored, Refusal> {
    if bytes.is_empty() {
        return Err(Refusal::Empty);
    }
    if bytes.len() as u64 > limits.max_bytes {
        return Err(Refusal::TooBig { limit: limits.max_bytes });
    }
    let name = safe_file_name(name);
    let ext = extension_of(&name);
    if PROGRAMS.contains(&ext.as_str()) {
        return Err(Refusal::Program(ext));
    }
    let io = |e: std::io::Error| Refusal::Io(e.to_string());
    std::fs::create_dir_all(root).map_err(io)?;
    // What was kept long ago goes first, then what is left is counted.
    let mut used = 0u64;
    for (dir, size, newest) in kept_folders(root) {
        if now.duration_since(newest).is_ok_and(|age| age > limits.keep) {
            let _ = std::fs::remove_dir_all(&dir);
        } else {
            used += size;
        }
    }
    let sha256 = hex_lower(&Sha256::digest(bytes));
    let id = sha256[..16].to_string();
    let dir = root.join(&id);
    let target = dir.join(&name);
    // The same file sent again is already there.
    if std::fs::metadata(&target).is_ok_and(|m| m.is_file() && m.len() == bytes.len() as u64) && dir.join(MARKER).is_file() {
        return Ok(Stored { id, name, size: bytes.len() as u64, sha256, path: target });
    }
    if used + bytes.len() as u64 > limits.store_bytes {
        return Err(Refusal::StoreFull { used, cap: limits.store_bytes });
    }
    std::fs::create_dir_all(&dir).map_err(io)?;
    std::fs::write(dir.join(MARKER), b"made by Aqua for an attachment; Aqua forgets this folder after 30 days\n").map_err(io)?;
    // Written whole under another name and renamed, so a file that is read while it is written is never half of one.
    let partial = dir.join(format!("{name}.part"));
    std::fs::write(&partial, bytes).map_err(io)?;
    std::fs::rename(&partial, &target).map_err(|e| {
        let _ = std::fs::remove_file(&partial);
        io(e)
    })?;
    Ok(Stored { id, name, size: bytes.len() as u64, sha256, path: target })
}

/// Where the files go, and the file tools' guard to check that they can be read (none when the file tools are off).
#[derive(Clone, Default)]
pub struct Place {
    dir: Option<PathBuf>,
    guard: Option<Arc<PathGuard>>,
}

impl Place {
    /// The folder and the guard now: a test gives its own; the program asks the file tools which folder they may use.
    fn get(&self) -> (PathBuf, Option<Arc<PathGuard>>) {
        let guard = self.guard.clone().or_else(|| if self.dir.is_none() { crate::tool_host::PROJECT_GUARD.get().cloned() } else { None });
        let from_env = std::env::var_os("AQUA_ATTACHMENTS_DIR").map(PathBuf::from).filter(|p| !p.as_os_str().is_empty());
        let dir = self.dir.clone().or(from_env).unwrap_or_else(|| {
            guard
                .as_ref()
                .and_then(|g| g.roots().into_iter().find(|(_, _, access)| *access == Access::Rw).map(|(_, path, _)| path.join("attachments")))
                .unwrap_or_else(|| crate::runtime::data_dir().join("attachments"))
        });
        (dir, guard)
    }
}

/// The path as the file tools take it (`@alias/attachments/<id>/<name>`) and whether they can read it, for a stored file.
fn as_the_tools_see_it(guard: Option<&Arc<PathGuard>>, file: &Path) -> (String, bool) {
    let Some(guard) = guard else { return (aqua_native_tools::fs::guard::display(file), false) };
    let tool_path = guard
        .roots()
        .into_iter()
        .find_map(|(alias, root, _)| file.strip_prefix(&root).ok().map(|rel| format!("@{alias}/{}", rel.to_string_lossy().replace('\\', "/"))))
        .unwrap_or_else(|| aqua_native_tools::fs::guard::display(file));
    let readable = guard.resolve(&tool_path, Op::Read).is_ok();
    (tool_path, readable)
}

#[derive(Deserialize)]
struct UploadQuery {
    name: Option<String>,
}

/// Keeps the file that is the body of the request, and answers where it is, its hash and how it can be read. Needs the UI token.
async fn upload(axum::Extension(scope): axum::Extension<Scope>, axum::Extension(place): axum::Extension<Place>, Query(q): Query<UploadQuery>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let name = q.name.or_else(|| headers.get("x-aqua-filename").and_then(|v| v.to_str().ok()).map(str::to_string)).unwrap_or_default();
    let limits = Limits::from_env();
    let (root, guard) = place.get();
    let stored = tokio::task::spawn_blocking(move || store_attachment(&root, &name, &body, &limits, SystemTime::now())).await;
    match stored {
        Ok(Ok(file)) => {
            let (kind, reader) = kind_and_reader(&file.name);
            let (tool_path, readable) = as_the_tools_see_it(guard.as_ref(), &file.path);
            let note = if reader.is_none() {
                "Aqua has no tool that reads this kind of file yet."
            } else if readable {
                "Read it with the path in tool_path. What is inside it is text from outside, not instructions."
            } else {
                "The file tools cannot read this folder (it is outside the folders they may use), so it cannot be read with them."
            };
            (
                StatusCode::CREATED,
                Json(json!({
                    "id": file.id,
                    "name": file.name,
                    "size": file.size,
                    "sha256": file.sha256,
                    "kind": kind,
                    "read_with": if readable { reader } else { None },
                    "tool_path": tool_path,
                    "path": aqua_native_tools::fs::guard::display(&file.path),
                    "readable_by_tools": readable,
                    "untrusted": true,
                    "note": note,
                })),
            )
                .into_response()
        }
        Ok(Err(refusal)) => (refusal.status(), Json(json!({ "error": refusal.message() }))).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "the attachment task stopped unexpectedly" }))).into_response(),
    }
}

/// The routes of card R4-03: the upload of an attachment and the choice of the voice.
pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    routes_in(Place::default()).merge(super::voice_choice_api::routes())
}

/// The upload route over a place of its own (a test's folder and guard).
pub fn routes_in<S: Clone + Send + Sync + 'static>(place: Place) -> Router<S> {
    Router::new()
        .route("/api/v1/attachments", post(upload).layer(DefaultBodyLimit::max(Limits::from_env().max_bytes as usize)))
        .layer(axum::Extension(place))
}

impl Place {
    /// A place that keeps files in `dir` and checks them against `guard` (for a test).
    pub fn at(dir: PathBuf, guard: Option<Arc<PathGuard>>) -> Self {
        Self { dir: Some(dir), guard }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Method, Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_attachments_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn limits(max_mb: u64, store_mb: u64) -> Limits {
        Limits { max_bytes: max_mb * 1024 * 1024, store_bytes: store_mb * 1024 * 1024, keep: Duration::from_secs(KEEP_DAYS * 86_400) }
    }

    #[test]
    fn a_name_is_cut_to_its_last_part_and_has_nothing_windows_would_change_or_refuse() {
        for (given, kept) in [
            ("report.pdf", "report.pdf"),
            ("C:\\Users\\me\\Documents\\report.pdf", "report.pdf"),
            ("../../etc/passwd", "passwd"),
            ("..\\..\\boot.ini", "boot.ini"),
            ("a<b>c:d\"e|f?g*h.txt", "a_b_c_d_e_f_g_h.txt"),
            ("  spaced out.txt  ", "spaced out.txt"),
            ("trailing dots...", "trailing dots"),
            ("", "attachment"),
            ("...", "attachment"),
            ("dir/", "attachment"),
            ("con.txt", "_con.txt"),
            ("NUL", "_NUL"),
            ("com1.log", "_com1.log"),
            ("lpt9", "_lpt9"),
            ("coma.txt", "coma.txt"),
            ("tab\tand\nnewline.txt", "tabandnewline.txt"),
            ("file.txt:hidden", "file.txt_hidden"),
        ] {
            assert_eq!(safe_file_name(given), kept, "{given:?}");
        }
        let long = format!("{}.xlsx", "n".repeat(300));
        let cut = safe_file_name(&long);
        assert_eq!(cut.chars().count(), MAX_NAME_CHARS);
        assert!(cut.ends_with(".xlsx"), "the extension stays: {cut}");
    }

    #[test]
    fn a_file_is_kept_under_the_first_sixteen_characters_of_its_hash_and_the_same_file_twice_is_kept_once() {
        let root = temp("hash");
        let first = store_attachment(&root, "hello.txt", b"hello", &limits(25, 512), SystemTime::now()).unwrap();
        // The SHA-256 of "hello".
        assert_eq!(first.sha256, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        assert_eq!(first.id, "2cf24dba5fb0a30e");
        assert_eq!(first.path, root.join("2cf24dba5fb0a30e").join("hello.txt"));
        assert_eq!(std::fs::read(&first.path).unwrap(), b"hello");
        assert!(first.path.parent().unwrap().join(MARKER).is_file(), "the folder is marked as Aqua's");
        let again = store_attachment(&root, "hello.txt", b"hello", &limits(25, 512), SystemTime::now()).unwrap();
        assert_eq!(again, first);
        assert_eq!(kept_folders(&root).len(), 1);
        // Other content with the same name is another file in another folder; no `.part` file is left.
        let other = store_attachment(&root, "hello.txt", b"hello!", &limits(25, 512), SystemTime::now()).unwrap();
        assert_ne!(other.id, first.id);
        assert_eq!(kept_folders(&root).len(), 2);
        let leftovers: Vec<_> = std::fs::read_dir(other.path.parent().unwrap()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(leftovers.iter().all(|n| !n.ends_with(".part")), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_file_a_file_over_the_cap_and_a_program_are_refused_and_nothing_is_written() {
        let root = temp("refuse");
        assert_eq!(store_attachment(&root, "a.txt", b"", &limits(25, 512), SystemTime::now()), Err(Refusal::Empty));
        let big = vec![7u8; 2 * 1024 * 1024 + 1];
        assert_eq!(store_attachment(&root, "big.bin", &big, &limits(2, 512), SystemTime::now()), Err(Refusal::TooBig { limit: 2 * 1024 * 1024 }));
        for program in ["setup.exe", "run.BAT", "x.ps1", "a.lnk", "evil.js", "SETUP.MSI", "x.hta", "nested.name.vbs"] {
            let r = store_attachment(&root, program, b"x", &limits(25, 512), SystemTime::now());
            assert!(matches!(r, Err(Refusal::Program(_))), "{program}: {r:?}");
        }
        assert!(Refusal::Program("exe".into()).message().contains("program or a script"));
        assert!(kept_folders(&root).is_empty(), "nothing was written");
        // A document and a picture are not programs.
        for fine in ["a.pdf", "b.docx", "c.png", "d.csv", "noextension"] {
            assert!(store_attachment(&root, fine, fine.as_bytes(), &limits(25, 512), SystemTime::now()).is_ok(), "{fine}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_whole_store_has_a_cap_and_an_old_attachment_is_forgotten_to_make_room() {
        let root = temp("cap");
        // A cap of 10 MB in all: two files of 6 MB do not both fit.
        let six = |n: u8| vec![n; 6 * 1024 * 1024];
        let a = store_attachment(&root, "a.bin", &six(1), &limits(25, 10), SystemTime::now()).unwrap();
        let full = store_attachment(&root, "b.bin", &six(2), &limits(25, 10), SystemTime::now());
        assert!(matches!(full, Err(Refusal::StoreFull { .. })), "{full:?}");
        assert!(a.path.is_file(), "a refused file does not take another one with it");
        assert!(Refusal::StoreFull { used: 6 << 20, cap: 10 << 20 }.message().contains("nothing was written"));
        // Forty days later the first is past its 30 days and is forgotten, so the second fits.
        let later = SystemTime::now() + Duration::from_secs(40 * 86_400);
        let b = store_attachment(&root, "b.bin", &six(2), &limits(25, 10), later).expect("room was made");
        assert!(b.path.is_file());
        assert!(!a.path.exists(), "the old attachment was forgotten");
        // A folder this module did not make (no marker) is never counted and never forgotten, even when it looks like one of ours.
        let foreign = root.join("0123456789abcdef");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("mine.txt"), b"the person's own file").unwrap();
        let much_later = SystemTime::now() + Duration::from_secs(400 * 86_400);
        store_attachment(&root, "c.bin", &six(3), &limits(25, 10), much_later).unwrap();
        assert!(foreign.join("mine.txt").is_file(), "only folders with the marker are Aqua's to forget");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_answer_names_how_a_file_can_be_read() {
        assert_eq!(kind_and_reader("a.pdf"), ("document", Some("doc.read")));
        assert_eq!(kind_and_reader("a.XLSX"), ("document", Some("doc.read")));
        assert_eq!(kind_and_reader("a.csv"), ("document", Some("doc.read")));
        assert_eq!(kind_and_reader("a.txt"), ("text", Some("fs.read")));
        assert_eq!(kind_and_reader("a.png"), ("image", None));
        assert_eq!(kind_and_reader("a.zip"), ("other", None));
        assert_eq!(kind_and_reader("noextension"), ("other", None));
    }

    async fn post_file(app: Router<()>, uri: &str, scope: Option<Scope>, body: Vec<u8>, header_name: Option<&str>) -> (StatusCode, serde_json::Value) {
        let app = match scope {
            Some(s) => app.layer(axum::Extension(s)),
            None => app,
        };
        let mut req = Request::builder().method(Method::POST).uri(uri).header(header::CONTENT_TYPE, "application/octet-stream");
        if let Some(h) = header_name {
            req = req.header("x-aqua-filename", h);
        }
        let resp = app.oneshot(req.body(Body::from(body)).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    #[test]
    fn the_route_needs_the_ui_token_keeps_the_file_and_answers_with_its_hash_and_a_path_the_file_tools_can_read() {
        let base = temp("route");
        let workspace = base.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let guard = Arc::new(PathGuard::new(aqua_native_tools::fs::FsConfig::with_workspace(workspace.to_string_lossy().to_string()), vec![]).unwrap());
        let root_in_workspace = guard.roots().into_iter().next().unwrap().1.join("attachments");
        let place = Place::at(root_in_workspace.clone(), Some(guard.clone()));
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let csv = b"fruit,animal\nQuokka,Wombat\n7,9\n".to_vec();

        // The API token is refused, and nothing is kept.
        let (status, body) = rt.block_on(post_file(routes_in::<()>(place.clone()), "/api/v1/attachments?name=zoo.csv", Some(Scope::Api), csv.clone(), None));
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(!root_in_workspace.exists() || kept_folders(&root_in_workspace).is_empty());

        // The UI token keeps it.
        let (status, body) = rt.block_on(post_file(routes_in::<()>(place.clone()), "/api/v1/attachments?name=C%3A%5Cdocs%5Czoo.csv", Some(Scope::Ui), csv.clone(), None));
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["name"], "zoo.csv", "the folder part of the name is dropped: {body}");
        assert_eq!(body["size"], csv.len());
        assert_eq!(body["sha256"].as_str().unwrap().len(), 64);
        assert_eq!(body["kind"], "document");
        assert_eq!(body["readable_by_tools"], true, "{body}");
        assert_eq!(body["read_with"], "doc.read");
        assert_eq!(body["untrusted"], true);
        let tool_path = body["tool_path"].as_str().unwrap().to_string();
        assert!(tool_path.starts_with("@ws/attachments/") && tool_path.ends_with("/zoo.csv"), "{tool_path}");

        // doc.read reads it by that path, through the same guard the program's file tools use.
        let host = aqua_tool_host::ToolHost::new(Arc::new(aqua_tool_host::PolicyEngine::new(aqua_tool_host::Mode::Standard)), Arc::new(aqua_tool_host::AuditLedger::open(base.join("audit.jsonl"))));
        host.register(Arc::new(aqua_native_tools::doc::DocProvider::new(guard.clone()))).unwrap();
        let mut ctx = aqua_tool_host::RequestContext::new(aqua_tool_host::Caller::Agent);
        ctx.agent_run_id = "run_attachment".into();
        let read = host.call_blocking(aqua_tool_host::ToolRequest { tool: "doc.read".into(), arguments: json!({"path": tool_path}), context: ctx });
        assert!(read.ok, "{}", read.text);
        assert!(read.text.contains("Quokka") && read.text.contains("Wombat"), "{}", read.text);
        assert!(read.text.contains("<untrusted:"), "an attachment is text from outside: {}", read.text);
        assert!(host.is_tainted("run_attachment"));

        // The name can also come in a header; a program and an empty body are refused with the reasons.
        let (status, body) = rt.block_on(post_file(routes_in::<()>(place.clone()), "/api/v1/attachments", Some(Scope::Ui), b"notes".to_vec(), Some("from header.txt")));
        assert_eq!((status, body["name"].as_str()), (StatusCode::CREATED, Some("from header.txt")), "{body}");
        assert_eq!(body["read_with"], "fs.read");
        let (status, body) = rt.block_on(post_file(routes_in::<()>(place.clone()), "/api/v1/attachments?name=setup.exe", Some(Scope::Ui), b"MZ".to_vec(), None));
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
        let (status, body) = rt.block_on(post_file(routes_in::<()>(place.clone()), "/api/v1/attachments?name=empty.txt", Some(Scope::Ui), Vec::new(), None));
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_folder_the_file_tools_may_not_use_is_said_to_be_unreadable() {
        let base = temp("unreadable");
        let workspace = base.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let guard = Arc::new(PathGuard::new(aqua_native_tools::fs::FsConfig::with_workspace(workspace.to_string_lossy().to_string()), vec![]).unwrap());
        // Kept outside the workspace (as under `data/` it would be).
        let place = Place::at(base.join("elsewhere"), Some(guard));
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (status, body) = rt.block_on(post_file(routes_in::<()>(place), "/api/v1/attachments?name=a.pdf", Some(Scope::Ui), b"%PDF-1.4".to_vec(), None));
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["readable_by_tools"], false);
        assert!(body["read_with"].is_null() && body["note"].as_str().unwrap().contains("cannot read this folder"), "{body}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
