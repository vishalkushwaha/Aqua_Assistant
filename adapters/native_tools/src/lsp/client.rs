//! One language server process and the conversation with it (card OA12-04, task CU-15 of the OpenCode audit).
//!
//! * [`verify_program`] decides whether a program may be started: it has a hash in `lsp.json`, it is a real program file (on Windows a `.exe`), it is not in a folder that the file tools may
//!   write (a program the model could replace, or a library next to it that it could plant), and the SHA-256 of its bytes is the one in the file. The file stays open, so that nobody can
//!   write to it, until the process has been started from it (Windows: opened without write sharing).
//! * [`Server::start`] starts it the way the terminal tools start a program: the environment is the allow-list of `contain::build_env` (no token, no password, no `AQUA_*` variable, no proxy with
//!   a login), the folder is the project's, the program is put in a Job Object that kills the whole tree when it is closed (a memory limit and a limit of processes hold), nothing is written to
//!   its input but the protocol, and its error output goes nowhere. A thread reads what it writes (every size bounded: see [`super::frame`]) and a thread writes what it is sent, so that a
//!   server that stops reading cannot stop a caller: every wait has a time and ends at once when the call is cancelled.
//! * What the server asks of the client (`window/workDoneProgress/create`, `workspace/configuration` ...) is answered with the least that satisfies it. It is never given a way to change a
//!   file or to show something to the person: `workspace/applyEdit` is answered "not applied".
//!
//! Requests are made one at a time on a server. Any answer is data: nothing in it is run.

use super::config::{how_to_add_a_hash, ServerSpec};
use super::frame::{self, Frame, FrameError};
use super::uri::file_uri;
use crate::contain::{build_env, Contained, Limits};
use crate::fs::guard::{canonical, check_opened_at};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The largest program that is hashed (rust-analyzer is about 40 MB).
const MAX_PROGRAM_BYTES: u64 = 512 << 20;

/// Why a call to a server did not give an answer. None of the words come from the server, except its error number. (A server that writes what is not the protocol is `Gone`, and the reason says so.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspError {
    /// The program was not started: the sentence says why and what to do about it.
    Refused(String),
    /// The program could not be started or did not finish starting.
    Start(String),
    /// No answer within the time (what was asked, and the seconds).
    NoAnswer(String, u64),
    /// The server ended, or was ended: the reason.
    Gone(String),
    /// The answer was larger than the cap and was thrown away.
    TooLarge { bytes: u64, cap: u64 },
    /// The server answered with an error (its number).
    Server(i64),
    /// The call was cancelled while it waited.
    Cancelled,
}

impl LspError {
    /// A failure after which the server is not asked again in this run (it was ended, it did not answer or it did not speak the protocol).
    pub fn ends_the_server(&self) -> bool {
        matches!(self, LspError::NoAnswer(..) | LspError::Gone(_) | LspError::Start(_))
    }

    /// The sentence the model is told.
    pub fn sentence(&self) -> String {
        match self {
            LspError::Refused(s) => s.clone(),
            LspError::Start(s) => format!("the language server could not be started: {s}"),
            LspError::NoAnswer(what, secs) => format!("the language server did not answer ({what}, {secs} s), so it was stopped"),
            LspError::Gone(s) => format!("the language server is gone ({s})"),
            LspError::TooLarge { bytes, cap } => format!("the language server's answer was {bytes} bytes, more than the {cap} that are read, so it was thrown away: ask about a smaller part"),
            LspError::Server(code) => match code {
                -32601 => "this language server does not support that request".to_string(),
                -32802..=-32800 => "the language server was busy (it may still be indexing, or the file changed); ask again in a moment".to_string(),
                c => format!("the language server answered with an error (code {c})"),
            },
            LspError::Cancelled => "the call was cancelled".to_string(),
        }
    }
}

/// What a request got.
enum Reply {
    Result(Value),
    Error(i64),
    TooLarge(u64),
    Gone(String),
}

/// The requests that wait for an answer, and whether the server is over: one lock, so that a request made as the server ends is never left waiting.
#[derive(Default)]
struct Waiting {
    pending: HashMap<i64, mpsc::Sender<Reply>>,
    over: Option<String>,
}

struct Inner {
    waiting: Mutex<Waiting>,
    next_id: AtomicI64,
    /// The way to the thread that writes; taken away when the server is over, which ends that thread and closes the server's input.
    writer: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    child: Mutex<Option<Child>>,
    contained: Mutex<Option<Contained>>,
    /// The answer to `workspace/workspaceFolders`.
    folders: Value,
    max_response: usize,
    /// One request at a time.
    one_at_a_time: Mutex<()>,
}

impl Inner {
    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn is_over(&self) -> Option<String> {
        Self::lock(&self.waiting).over.clone()
    }

    /// The server is over (ended, silent or wrong): everything that waits is told, the process tree is ended, the input is closed.
    fn die(&self, reason: impl Into<String>) {
        let reason = reason.into();
        let senders: Vec<mpsc::Sender<Reply>> = {
            let mut w = Self::lock(&self.waiting);
            if w.over.is_some() {
                return;
            }
            w.over = Some(reason.clone());
            w.pending.drain().map(|(_, s)| s).collect()
        };
        for s in senders {
            let _ = s.send(Reply::Gone(reason.clone()));
        }
        Self::lock(&self.writer).take();
        self.kill_process();
    }

    fn kill_process(&self) {
        if let Some(c) = Self::lock(&self.contained).as_ref() {
            c.kill_tree();
        }
        if let Some(mut child) = Self::lock(&self.child).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // The job is closed with the last of the handles: nothing of the tree is left whichever way it was started.
        Self::lock(&self.contained).take();
    }

    fn send(&self, message: &Value) -> Result<(), LspError> {
        let bytes = frame::encode(message.to_string().as_bytes());
        match Self::lock(&self.writer).as_ref() {
            Some(w) => w.send(bytes).map_err(|_| LspError::Gone("its input is closed".into())),
            None => Err(LspError::Gone(self.is_over().unwrap_or_else(|| "it was stopped".into()))),
        }
    }

    /// A message from the server.
    fn dispatch(&self, message: Value) {
        let method = message.get("method").and_then(Value::as_str);
        match (method, message.get("id")) {
            // The server asks something of the client: the least that satisfies it.
            (Some(method), Some(id)) => {
                let reply = match method {
                    "window/workDoneProgress/create" | "client/registerCapability" | "client/unregisterCapability" | "window/showMessageRequest" | "workspace/semanticTokens/refresh" | "workspace/inlayHint/refresh"
                    | "workspace/codeLens/refresh" | "workspace/diagnostic/refresh" | "workspace/inlineValue/refresh" => json!({"jsonrpc": "2.0", "id": id, "result": null}),
                    "workspace/configuration" => {
                        let n = message.pointer("/params/items").and_then(Value::as_array).map_or(0, |a| a.len().min(256));
                        json!({"jsonrpc": "2.0", "id": id, "result": vec![Value::Null; n]})
                    }
                    "workspace/workspaceFolders" => json!({"jsonrpc": "2.0", "id": id, "result": self.folders}),
                    "workspace/applyEdit" => json!({"jsonrpc": "2.0", "id": id, "result": {"applied": false, "failureReason": "this client never applies an edit"}}),
                    "window/showDocument" => json!({"jsonrpc": "2.0", "id": id, "result": {"success": false}}),
                    _ => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "not supported by this client"}}),
                };
                let _ = self.send(&reply);
            }
            // A notification (a log line, a progress report, diagnostics): not used.
            (Some(_), None) => {}
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else { return };
                let sender = Self::lock(&self.waiting).pending.remove(&id);
                if let Some(s) = sender {
                    let reply = match message.get("error") {
                        Some(e) => Reply::Error(e.get("code").and_then(Value::as_i64).unwrap_or(-32603)),
                        None => Reply::Result(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = s.send(reply);
                }
            }
            (None, None) => {}
        }
    }
}

fn reader_loop(inner: Arc<Inner>, stdout: ChildStdout) {
    let mut r = BufReader::with_capacity(64 * 1024, stdout);
    loop {
        match frame::read_frame(&mut r, inner.max_response) {
            Ok(Frame::Body(bytes)) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(v) if v.is_object() => inner.dispatch(v),
                _ => return inner.die("what it wrote is not a JSON-RPC message"),
            },
            Ok(Frame::Skipped(n)) => {
                // The body was not read, so the request it answers is not known; only one request is made at a time, so it is the one that waits.
                let senders: Vec<mpsc::Sender<Reply>> = Inner::lock(&inner.waiting).pending.drain().map(|(_, s)| s).collect();
                for s in senders {
                    let _ = s.send(Reply::TooLarge(n));
                }
            }
            Err(FrameError::Eof) => return inner.die("it closed its output: it exited"),
            Err(FrameError::Truncated) => return inner.die("it ended in the middle of a message"),
            Err(FrameError::Malformed(why)) => return inner.die(format!("bad framing: {why}")),
            Err(FrameError::Io(_)) => return inner.die("its output could not be read"),
        }
    }
}

fn writer_loop(inner: Arc<Inner>, rx: mpsc::Receiver<Vec<u8>>, mut stdin: ChildStdin) {
    for bytes in rx {
        if stdin.write_all(&bytes).and_then(|_| stdin.flush()).is_err() {
            return inner.die("it stopped reading its input");
        }
    }
}

/// A program that has passed [`verify_program`]: it is held open until the process has been started from it.
pub struct Verified {
    _file: std::fs::File,
    pub path: PathBuf,
    pub sha256: String,
}

/// May this program be started? `may_write` says whether the file tools may write to a path (a program there is not started).
pub fn verify_program(spec: &ServerSpec, may_write: &dyn Fn(&Path) -> bool) -> Result<Verified, LspError> {
    let lang = &spec.language;
    let Some(expected) = &spec.sha256 else {
        return Err(LspError::Refused(format!("the {lang} language server has no hash in data/tools/lsp.json, so it was not started: {}", how_to_add_a_hash(lang, &spec.command))));
    };
    let refuse = |why: String| LspError::Refused(format!("the {lang} language server was not started: {why}"));
    let canon = canonical(&spec.command).map_err(|e| refuse(format!("{} cannot be opened ({})", spec.command.display(), e.kind())))?;
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Others may read it, none may write to it or delete it while it is held.
        opts.share_mode(1);
    }
    let mut file = opts.open(&canon).map_err(|e| refuse(format!("{} cannot be opened ({})", canon.display(), e.kind())))?;
    check_opened_at(&file, &canon).map_err(|e| refuse(e.message))?;
    let meta = file.metadata().map_err(|e| refuse(format!("{} cannot be read ({})", canon.display(), e.kind())))?;
    if !meta.is_file() {
        return Err(refuse(format!("{} is not a file", canon.display())));
    }
    if meta.len() > MAX_PROGRAM_BYTES {
        return Err(refuse(format!("{} is larger than {} MB", canon.display(), MAX_PROGRAM_BYTES >> 20)));
    }
    if cfg!(windows) && !canon.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) {
        return Err(refuse(format!("{} is not an .exe (a script or a shortcut starts another program that the hash does not cover)", canon.display())));
    }
    if may_write(&canon) {
        return Err(refuse(format!("{} is in a folder the file tools may write, so a program there could be replaced by what a run wrote, or loaded next to a library it planted; put the program outside the project and the allowed folders", canon.display())));
    }
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut head = [0u8; 2];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf).map_err(|e| refuse(format!("{} cannot be read ({})", canon.display(), e.kind())))?;
        if n == 0 {
            break;
        }
        if total == 0 {
            head[..n.min(2)].copy_from_slice(&buf[..n.min(2)]);
        }
        total += n as u64;
        if total > MAX_PROGRAM_BYTES {
            return Err(refuse(format!("{} is larger than {} MB", canon.display(), MAX_PROGRAM_BYTES >> 20)));
        }
        hasher.update(&buf[..n]);
    }
    let actual: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if &actual != expected {
        return Err(refuse(format!(
            "the program's SHA-256 ({}...) is not the one in data/tools/lsp.json ({}...): it was changed or replaced. If you updated it on purpose, put the new hash in the file",
            &actual[..16],
            &expected[..16]
        )));
    }
    if cfg!(windows) && &head != b"MZ" {
        return Err(refuse(format!("{} is not a Windows program", canon.display())));
    }
    Ok(Verified { _file: file, path: canon, sha256: actual })
}

/// A running language server.
pub struct Server {
    inner: Arc<Inner>,
    pub language: String,
    pub program: PathBuf,
    pub sha256: String,
    pub pid: u32,
    pub started: Instant,
}

impl Server {
    /// Start the program of `spec` in `root` and make it ready for requests (the `initialize` exchange, within the start-up time of the spec). `cancel` ends the wait.
    pub fn start(spec: &ServerSpec, program: Verified, root: &Path, cancel: &AtomicBool) -> Result<Server, LspError> {
        let env = build_env(std::env::vars(), &HashMap::new()).map_err(LspError::Start)?;
        let mut cmd = Command::new(&program.path);
        cmd.args(&spec.args).current_dir(root).env_clear().envs(env).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn().map_err(|e| LspError::Start(format!("{} did not start ({})", program.path.display(), e.kind())))?;
        // The program is running now, so what was held open is let go.
        let (path, sha256) = (program.path.clone(), program.sha256.clone());
        drop(program);
        let contained = match Contained::attach(&child, Limits { memory_mb: spec.memory_mb, max_processes: spec.max_processes }) {
            Ok(c) => c,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(LspError::Start(format!("it could not be contained, so it was stopped: {e}")));
            }
        };
        let pid = child.id();
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(LspError::Start("its input and output could not be opened".into()));
        };
        let root_uri = file_uri(root);
        let folders = json!([{"uri": root_uri, "name": root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "project".into())}]);
        let inner = Arc::new(Inner {
            waiting: Mutex::new(Waiting::default()),
            next_id: AtomicI64::new(1),
            writer: Mutex::new(None),
            child: Mutex::new(Some(child)),
            contained: Mutex::new(Some(contained)),
            folders: folders.clone(),
            max_response: spec.max_response_bytes as usize,
            one_at_a_time: Mutex::new(()),
        });
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        *Inner::lock(&inner.writer) = Some(tx);
        let (r_inner, w_inner) = (inner.clone(), inner.clone());
        let threads = std::thread::Builder::new()
            .name("lsp-reader".into())
            .spawn(move || reader_loop(r_inner, stdout))
            .and_then(|_| std::thread::Builder::new().name("lsp-writer".into()).spawn(move || writer_loop(w_inner, rx, stdin)));
        if let Err(e) = threads {
            inner.die("its threads could not be started");
            return Err(LspError::Start(format!("a thread could not be started ({})", e.kind())));
        }
        let server = Server { inner, language: spec.language.clone(), program: path, sha256, pid, started: Instant::now() };
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "aqua", "version": "1"},
            "locale": "en",
            "rootPath": root.to_string_lossy(),
            "rootUri": root_uri,
            "workspaceFolders": folders,
            "capabilities": {
                "general": {"positionEncodings": ["utf-16"]},
                "textDocument": {
                    "synchronization": {"dynamicRegistration": false, "didSave": false, "willSave": false},
                    "definition": {"dynamicRegistration": false, "linkSupport": false},
                    "references": {"dynamicRegistration": false},
                    "hover": {"dynamicRegistration": false, "contentFormat": ["plaintext", "markdown"]},
                    "documentSymbol": {"dynamicRegistration": false, "hierarchicalDocumentSymbolSupport": true}
                },
                "workspace": {"workspaceFolders": true, "configuration": true, "applyEdit": false},
                "window": {"workDoneProgress": true}
            },
            "trace": "off"
        });
        match server.request("initialize", params, Duration::from_secs(spec.startup_timeout_secs), cancel) {
            Ok(_) => {}
            Err(LspError::NoAnswer(_, secs)) => {
                server.kill();
                return Err(LspError::Start(format!("it did not finish starting in {secs} s")));
            }
            Err(LspError::Cancelled) => {
                server.kill();
                return Err(LspError::Cancelled);
            }
            Err(LspError::Server(code)) => {
                server.kill();
                return Err(LspError::Start(format!("it refused to start (code {code})")));
            }
            Err(e) => {
                server.kill();
                return Err(match e {
                    LspError::Gone(why) => LspError::Start(why),
                    other => other,
                });
            }
        }
        server.notify("initialized", json!({}))?;
        Ok(server)
    }

    pub fn is_alive(&self) -> bool {
        self.inner.is_over().is_none()
    }

    /// Why the server is over, when it is.
    pub fn why_over(&self) -> Option<String> {
        self.inner.is_over()
    }

    /// Ask for something and wait for the answer: at most `timeout`, and not after `cancel` is set.
    pub fn request(&self, method: &str, params: Value, timeout: Duration, cancel: &AtomicBool) -> Result<Value, LspError> {
        let _one = Inner::lock(&self.inner.one_at_a_time);
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        {
            let mut w = Inner::lock(&self.inner.waiting);
            if let Some(why) = &w.over {
                return Err(LspError::Gone(why.clone()));
            }
            w.pending.insert(id, tx);
        }
        if let Err(e) = self.inner.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})) {
            Inner::lock(&self.inner.waiting).pending.remove(&id);
            return Err(e);
        }
        let deadline = Instant::now() + timeout;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(Reply::Result(v)) => return Ok(v),
                Ok(Reply::Error(code)) => return Err(LspError::Server(code)),
                Ok(Reply::TooLarge(bytes)) => return Err(LspError::TooLarge { bytes, cap: self.inner.max_response as u64 }),
                Ok(Reply::Gone(why)) => return Err(LspError::Gone(why)),
                Err(RecvTimeoutError::Disconnected) => return Err(LspError::Gone(self.inner.is_over().unwrap_or_else(|| "it was stopped".into()))),
                Err(RecvTimeoutError::Timeout) => {
                    if cancel.load(Ordering::SeqCst) {
                        Inner::lock(&self.inner.waiting).pending.remove(&id);
                        let _ = self.inner.send(&json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": id}}));
                        return Err(LspError::Cancelled);
                    }
                    if Instant::now() >= deadline {
                        Inner::lock(&self.inner.waiting).pending.remove(&id);
                        let _ = self.inner.send(&json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": id}}));
                        return Err(LspError::NoAnswer(method.to_string(), timeout.as_secs().max(1)));
                    }
                }
            }
        }
    }

    /// Tell the server something that has no answer.
    pub fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        self.inner.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    /// End the process tree now.
    pub fn kill(&self) {
        self.inner.die("it was stopped");
    }

    /// Ask the server to end (`shutdown`, `exit`), and end whatever is left of it a moment later.
    pub fn shutdown(&self, grace: Duration) {
        if self.is_alive() {
            let never = AtomicBool::new(false);
            let _ = self.request("shutdown", Value::Null, grace, &never);
            let _ = self.notify("exit", Value::Null);
            let until = Instant::now() + Duration::from_millis(300);
            while Instant::now() < until {
                let exited = match Inner::lock(&self.inner.child).as_mut() {
                    Some(c) => c.try_wait().map(|s| s.is_some()).unwrap_or(true),
                    None => true,
                };
                if exited {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        self.kill();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.inner.die("it was stopped");
    }
}
