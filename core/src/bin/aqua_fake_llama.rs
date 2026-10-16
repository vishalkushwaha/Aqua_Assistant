//! A stand-in for `llama-server` for the tests of the model client (`modules/llm_router/llama_server.rs`): the same HTTP the real program speaks, with replies and faults taken
//! from a script, and no model. Card OA2-02 (QA-02 of the OpenCode audit). It is started by the real launch path (`AQUA_LLAMA_SERVER_BIN` for the processor build,
//! `AQUA_LLAMA_GPU_SERVER_BIN` for the graphics-card build), so a test goes through `spawn_server`, the health check, the Job Object and the request threads exactly as Aqua does,
//! and needs no model file and no GPU. It only exists with the `test-support` feature (`required-features` in `core/Cargo.toml`), so a release build does not contain it.
//!
//! **What it speaks** (the parts of `llama-server` that Aqua uses):
//! * `--host` (loopback only), `--port` (default: a free one), `--parallel`, `-c`, `--log-file`, `-dev`, `-m`; every other option is read and ignored. It refuses ports 8767, 8766 and 1420
//!   (Aqua's own) and anything below 1024, and any host that is not loopback. `--list-devices`, `--help` and `--version` print and exit.
//! * `GET /health` (no key; `503` while the model is "loading"), `GET /slots` (with the key), `POST /tokenize`, `GET /v1/models`, and `POST /v1/chat/completions` in both
//!   shapes: one JSON body, or (`"stream": true`) server-sent events in HTTP chunks, ending with a finish reason, the `timings` and `data: [DONE]`. The key in the `LLAMA_API_KEY`
//!   environment variable is asked for as `Authorization: Bearer <key>`, answered `401` otherwise.
//! * It keeps what each slot has read, so `timings.cache_n` is the shared start of the prompt (about four characters a token) and `prompt_n` the rest, as with `cache_prompt`. One
//!   request at a time per slot; a second waits for the first, and is dropped if its client leaves while it waits.
//! * **`POST /slots/N?action=save|restore`** (card OA9-06, second round) the way the real server has it, but only when `FAKE_LLAMA_SLOTS=on` (a build without the endpoint answers `404`, which is
//!   what the older builds do and what the first round's test expects) and, as in the real one, only when it was started with `--slot-save-path` (else `501`). A save writes what the slot
//!   has read, with the name of the model file, to `<path>/<filename>`; a restore reads it back and the slot then shares its start with the next prompt (`cache_n`). A file that is missing,
//!   is not one this program wrote, or was written by another model file is refused with the real server's `400`. The script options `slot_save_status` and `slot_restore_status` make the
//!   server refuse every save or restore with that status. Every request of the kind is one `slot` line of the events file (`save`, `restore`, the slot, the file, the status).
//! * Admin: `GET /_hits` (what was asked and how each request came out) and `POST /_script` (replace the script). Both need the key.
//!
//! **The script** is JSON in the file named by `FAKE_LLAMA_SCRIPT` (`FAKE_LLAMA_GPU_SCRIPT` instead when it was started with `-dev`, the graphics-card build), read at start:
//! ```json
//! { "load_ms": 0, "exit_while_loading": {"code": 1, "log": "error: ..."}, "prompt_ms_per_token": 0, "enforce_context": false, "repeat_last": false, "warmups_are_free": false,
//!   "items": [ "a short text", {"type": "text", ...}, {"type": "hang"} ] }
//! ```
//! (a bare array is the `items`). With `warmups_are_free` a request for one token or less (the core reads the start of the agent's prompt ahead of the first round, with `max_tokens` 1) is answered
//! with one token and takes no item, so that a script that was written for the replies of the agent is not shifted by that read (the kill task of AquaBench, `scripts/eval/tasks/recovery_kill.yaml`,
//! starts a whole core on this server and has no other way to tell the two kinds of request apart). Each completion request takes the next item; **the position is kept in `<script file>.cursor`**, so the script describes the requests of a whole test,
//! also across the restarts of the server. When the items are used up the answer is `500` naming the script (or the last item again with `repeat_last`). The items:
//! * a text, or `{"type":"text","chunks":["a","b"],"chunk_delay_ms":0,"prompt_delay_ms":0,"finish_reason":"stop","timings":{..},"split_utf8":false,"top_logprobs":[["A",-0.1]]}`: the
//!   answer. `text` instead of `chunks` is one chunk (`chunk_chars` cuts it into pieces). `finish_reason` `"length"` is an answer cut at the token limit. `timings` replaces
//!   numbers of the computed ones. `split_utf8` sends the first non-ASCII character of each event in two writes, with a pause between.
//! * `{"type":"json","value":{..}}`: this JSON as the reply (a reply of a shape Aqua does not know).
//! * `{"type":"http_error","status":500,"body":{..}|"text","prompt_delay_ms":0}`: that status (`400` for a prompt past the window, `500` for a server that fell over).
//! * `{"type":"hang","max_ms":600000,"ignore_hangup":false}`: takes the request and never answers. It notices the client hanging up, as the real server does, and lets go of its
//!   slot; `ignore_hangup` is a server that is stuck and keeps it.
//! * `{"type":"reset","after_ms":0}`: the connection is torn down with a reset, before any byte of the answer.
//! * `{"type":"cut","after_chunks":2,"chunks":[..],"abrupt":false}`: the head of the answer and then the end of the connection (a reset when `abrupt`), with no finish reason and no
//!   `[DONE]`: a server killed in the middle of the answer. Without streaming, a body cut in half.
//! * `{"type":"exit_after_load","code":3}`: the process ends when this request arrives, after the model "loaded" (a lost device, an out-of-memory kill).
//!
//! `FAKE_LLAMA_FAST_MODEL` and `FAKE_LLAMA_FAST_SCRIPT` (card OA9-06, second round): a process started with a `-m` whose file name is `FAKE_LLAMA_FAST_MODEL` reads the script of
//! `FAKE_LLAMA_FAST_SCRIPT` and not the usual one, so that the main model and the fast tier's small model, two processes of this program, answer differently. Both write to the same events file
//! (each line has the process id; the `start` line has the model).
//! `FAKE_LLAMA_EVENTS` names a file that gets one JSON line per event (`start` with the port, `request`, `done` with the outcome), for a test that cannot reach the port.
//! `FAKE_LLAMA_DEVICES` is what `--list-devices` prints, and `FAKE_LLAMA_HELP` is what `--help` prints (the part Aqua reads: `launch_args::parse_help`; card OA9-06 gives a test the
//! options of a build that has `--cache-reuse` or a draft model). The names do not start with `AQUA_` on purpose: they are not settings of Aqua.

use serde_json::{json, Map, Value};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The ports of Aqua itself (the core, the tools, the desktop shell). The fake never listens on them, whatever it is told.
const AQUA_PORTS: [u16; 3] = [8767, 8766, 1420];

/// The listing `--list-devices` prints unless `FAKE_LLAMA_DEVICES` says otherwise: a card that `gpu::pick` takes.
const DEVICES: &str = "Available devices:\n  Vulkan0: NVIDIA GeForce GTX 1650 (4152 MiB, 3561 MiB free)\n";

/// The part of `llama-server --help` that Aqua reads (`launch_args::parse_help`).
const HELP: &str = "-fa,   --flash-attn [on|off|auto]       set Flash Attention use ('on', 'off', or 'auto', default: 'auto')\n-ctk,  --cache-type-k TYPE              KV cache data type for K\n                                        allowed values: f32, f16, bf16, q8_0, q4_0\n-ctv,  --cache-type-v TYPE              KV cache data type for V\n                                        allowed values: f32, f16, bf16, q8_0, q4_0\n--spec-type none,ngram-simple,ngram-map-k\n                                        comma-separated list of types of speculative decoding to use (default:\n                                        none)\n--reasoning-budget N                    token budget for thinking\n";

// ---------------------------------------------------------------------------------------------------------------------------------------------------
// The command line
// ---------------------------------------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct Args {
    model: String,
    host: String,
    port: Option<u16>,
    parallel: usize,
    window: u32,
    log_file: Option<PathBuf>,
    /// `-dev`: the server was started for a graphics card.
    device: Option<String>,
    /// `--slot-save-path`: where the saved state of a slot is written and read.
    slot_save_path: Option<PathBuf>,
    list_devices: bool,
    help: bool,
    version: bool,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut a = Args { model: String::new(), host: "127.0.0.1".into(), port: None, parallel: 1, window: 6144, log_file: None, device: None, slot_save_path: None, list_devices: false, help: false, version: false };
    let mut i = 0;
    let value = |i: usize, flag: &str| argv.get(i + 1).cloned().ok_or_else(|| format!("error: the option {flag} needs a value"));
    while i < argv.len() {
        let flag = argv[i].as_str();
        match flag {
            "-m" | "--model" => {
                a.model = value(i, flag)?;
                i += 1;
            }
            "--host" => {
                a.host = value(i, flag)?;
                i += 1;
            }
            "--port" => {
                let v = value(i, flag)?;
                a.port = Some(v.parse().map_err(|_| format!("error: --port wants a number, not {v:?}"))?);
                i += 1;
            }
            "-np" | "--parallel" => {
                let v = value(i, flag)?;
                a.parallel = v.parse::<usize>().map_err(|_| format!("error: {flag} wants a number, not {v:?}"))?.clamp(1, 16);
                i += 1;
            }
            "-c" | "--ctx-size" => {
                let v = value(i, flag)?;
                a.window = v.parse().map_err(|_| format!("error: {flag} wants a number, not {v:?}"))?;
                i += 1;
            }
            "--log-file" => {
                a.log_file = Some(PathBuf::from(value(i, flag)?));
                i += 1;
            }
            "-dev" | "--device" => {
                a.device = Some(value(i, flag)?);
                i += 1;
            }
            "--slot-save-path" => {
                a.slot_save_path = Some(PathBuf::from(value(i, flag)?));
                i += 1;
            }
            "--list-devices" => a.list_devices = true,
            "-h" | "--help" => a.help = true,
            "--version" => a.version = true,
            // Everything else the real launch passes (threads, layers, cache types, --no-webui ...) changes nothing here.
            _ => {}
        }
        i += 1;
    }
    if let Some(port) = a.port {
        if AQUA_PORTS.contains(&port) {
            return Err(format!("error: refusing port {port}: it is one of Aqua's own"));
        }
        if port != 0 && port < 1024 {
            return Err(format!("error: refusing port {port}: ports below 1024 are not used by the tests"));
        }
    }
    if !matches!(a.host.as_str(), "127.0.0.1" | "localhost" | "::1") {
        return Err(format!("error: refusing host {:?}: the fake model server listens on the loopback address only", a.host));
    }
    Ok(a)
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------
// The script
// ---------------------------------------------------------------------------------------------------------------------------------------------------

const ITEM_TYPES: [&str; 7] = ["text", "json", "http_error", "hang", "reset", "cut", "exit_after_load"];

#[derive(Debug, Clone, Default)]
struct Script {
    items: Vec<Value>,
    repeat_last: bool,
    load_ms: u64,
    /// The exit code and what the server writes to its log file before it ends: a start that fails while the model loads.
    exit_while_loading: Option<(i32, String)>,
    prompt_ms_per_token: f64,
    /// Refuse a prompt that does not fit the window (`-c` divided between the slots) with the real server's `400`, instead of taking the next item.
    enforce_context: bool,
    /// A request for one token or less is answered with one token and takes no item (see the head of this file).
    warmups_are_free: bool,
    /// Every save of a slot is refused with this status (a server that cannot write the file), and every restore with the other.
    slot_save_status: Option<u16>,
    slot_restore_status: Option<u16>,
}

impl Script {
    fn parse(v: &Value) -> Result<Script, String> {
        let (list, options) = match v {
            Value::Array(items) => (items.clone(), &Value::Null),
            Value::Object(o) => (o.get("items").and_then(Value::as_array).cloned().unwrap_or_default(), v),
            _ => return Err("the script must be a JSON array of items or an object with \"items\"".into()),
        };
        let mut items = Vec::new();
        for (i, item) in list.into_iter().enumerate() {
            let item = match item {
                Value::String(text) => json!({"type": "text", "chunks": [text]}),
                Value::Object(_) => item,
                other => return Err(format!("item {i} of the script is {other}: expected a text or an object")),
            };
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
            if !ITEM_TYPES.contains(&kind) {
                return Err(format!("item {i} of the script has the type {kind:?}: expected one of {}", ITEM_TYPES.join(", ")));
            }
            items.push(item);
        }
        let exit_while_loading = options.get("exit_while_loading").filter(|e| !e.is_null()).map(|e| (e["code"].as_i64().unwrap_or(1) as i32, e["log"].as_str().unwrap_or("").to_string()));
        Ok(Script {
            items,
            repeat_last: options["repeat_last"].as_bool().unwrap_or(false),
            load_ms: options["load_ms"].as_u64().unwrap_or(0),
            exit_while_loading,
            prompt_ms_per_token: options["prompt_ms_per_token"].as_f64().unwrap_or(0.0).max(0.0),
            enforce_context: options["enforce_context"].as_bool().unwrap_or(false),
            warmups_are_free: options["warmups_are_free"].as_bool().unwrap_or(false),
            slot_save_status: options["slot_save_status"].as_u64().and_then(|s| u16::try_from(s).ok()).filter(|s| (400..600).contains(s)),
            slot_restore_status: options["slot_restore_status"].as_u64().and_then(|s| u16::try_from(s).ok()).filter(|s| (400..600).contains(s)),
        })
    }
}

/// Which item the next completion request takes. Kept in a file beside the script when the script came from one, so that the position survives a restart of the server.
struct Cursor {
    file: Option<PathBuf>,
    next: usize,
}

impl Cursor {
    fn for_script_file(path: &Path) -> Cursor {
        let mut name = path.as_os_str().to_owned();
        name.push(".cursor");
        Cursor { file: Some(PathBuf::from(name)), next: 0 }
    }

    fn take(&mut self) -> usize {
        match &self.file {
            Some(file) => {
                let n = std::fs::read_to_string(file).ok().and_then(|t| t.trim().parse::<usize>().ok()).unwrap_or(0);
                let _ = std::fs::write(file, (n + 1).to_string());
                n
            }
            None => {
                self.next += 1;
                self.next - 1
            }
        }
    }

    fn reset(&mut self) {
        self.next = 0;
        if let Some(file) = &self.file {
            let _ = std::fs::write(file, "0");
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------------------------------------------------------------------------------

struct Slot {
    busy: bool,
    /// What the slot has read, for the share of the next prompt that comes from the cache.
    prompt: String,
}

struct Scripted {
    script: Script,
    cursor: Cursor,
}

struct Shared {
    key: Option<String>,
    started: Instant,
    /// `cpu` or `gpu` (started with `-dev`).
    mode: &'static str,
    model: String,
    window: u32,
    events: Option<PathBuf>,
    /// `--slot-save-path`, and whether this "build" has the slot endpoint at all (`FAKE_LLAMA_SLOTS=on`).
    slot_dir: Option<PathBuf>,
    slot_endpoint: bool,
    scripted: Mutex<Scripted>,
    slots: Mutex<Vec<Slot>>,
    slot_freed: Condvar,
    hits: Mutex<Vec<Map<String, Value>>>,
    serving: AtomicUsize,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn tokens_of(chars: usize) -> u64 {
    chars.div_ceil(4) as u64
}

fn common_prefix_chars(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

impl Shared {
    fn new(key: Option<String>, mode: &'static str, args: &Args, scripted: Scripted, events: Option<PathBuf>) -> Shared {
        Shared {
            key,
            started: Instant::now(),
            mode,
            model: args.model.clone(),
            window: args.window,
            events,
            slot_dir: args.slot_save_path.clone(),
            slot_endpoint: std::env::var("FAKE_LLAMA_SLOTS").is_ok_and(|v| v.eq_ignore_ascii_case("on")),
            scripted: Mutex::new(scripted),
            slots: Mutex::new((0..args.parallel).map(|_| Slot { busy: false, prompt: String::new() }).collect()),
            slot_freed: Condvar::new(),
            hits: Mutex::new(Vec::new()),
            serving: AtomicUsize::new(0),
        }
    }

    fn loading(&self) -> bool {
        self.started.elapsed() < Duration::from_millis(lock(&self.scripted).script.load_ms)
    }

    fn authorized(&self, req: &Request) -> bool {
        match &self.key {
            None => true,
            Some(key) => req.header("authorization").is_some_and(|h| h.strip_prefix("Bearer ").is_some_and(|got| got.trim() == key)),
        }
    }

    /// One line to the events file (when there is one).
    fn event(&self, kind: &str, mut fields: Value) {
        let Some(path) = &self.events else { return };
        fields["event"] = json!(kind);
        fields["pid"] = json!(std::process::id());
        fields["mode"] = json!(self.mode);
        fields["t_ms"] = json!(self.started.elapsed().as_millis() as u64);
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{fields}");
        }
    }

    fn begin_hit(&self, mut hit: Map<String, Value>) -> usize {
        let mut hits = lock(&self.hits);
        let n = hits.len();
        hit.insert("n".into(), json!(n));
        self.event("request", Value::Object(hit.clone()));
        hits.push(hit);
        n
    }

    fn note_hit(&self, n: usize, key: &str, value: Value) {
        if let Some(hit) = lock(&self.hits).get_mut(n) {
            hit.insert(key.into(), value);
        }
    }

    /// Notes how the request came out, and writes the whole record of it to the events file.
    fn finish_hit(&self, n: usize, outcome: &str) {
        let mut hits = lock(&self.hits);
        let Some(hit) = hits.get_mut(n) else { return };
        let took = (self.started.elapsed().as_millis() as u64).saturating_sub(hit.get("at_ms").and_then(Value::as_u64).unwrap_or(0));
        hit.insert("outcome".into(), json!(outcome));
        hit.insert("took_ms".into(), json!(took));
        let record = Value::Object(hit.clone());
        drop(hits);
        self.event("done", record);
    }

    /// One `slot` line of the events file for a request to `/slots/N?action=...`: the action, the slot, the file asked for, and how it came out.
    fn slot_event(&self, req: &Request, mut fields: Value) {
        let (path, query) = req.path.split_once('?').unwrap_or((req.path.as_str(), ""));
        fields["action"] = json!(query.split('&').find_map(|kv| kv.strip_prefix("action=")).unwrap_or(""));
        fields["slot"] = json!(path.trim_start_matches("/slots/"));
        fields["file"] = serde_json::from_slice::<Value>(&req.body).ok().map_or(Value::Null, |v| v["filename"].clone());
        self.event("slot", fields);
    }

    /// Takes a slot (the one asked for, else the free one that shares most with `prompt`), waiting for one to be free. `None` when the client leaves while it waits.
    fn acquire(&self, wanted: Option<usize>, prompt: &str, conn: &TcpStream) -> Option<(usize, usize)> {
        let mut slots = lock(&self.slots);
        loop {
            let pick = match wanted.filter(|w| *w < slots.len()) {
                Some(w) => (!slots[w].busy).then_some(w),
                None => (0..slots.len()).filter(|i| !slots[*i].busy).max_by_key(|i| (common_prefix_chars(&slots[*i].prompt, prompt), usize::MAX - *i)),
            };
            if let Some(i) = pick {
                slots[i].busy = true;
                let shared = common_prefix_chars(&slots[i].prompt, prompt);
                return Some((i, shared));
            }
            slots = self.slot_freed.wait_timeout(slots, Duration::from_millis(25)).unwrap_or_else(|e| e.into_inner()).0;
            if client_gone(conn) {
                return None;
            }
        }
    }

    fn release(&self, slot: usize, read: Option<&str>) {
        let mut slots = lock(&self.slots);
        slots[slot].busy = false;
        if let Some(prompt) = read {
            slots[slot].prompt = prompt.to_string();
        }
        self.slot_freed.notify_all();
    }

    /// The next item: its place in the script and its text. `None` when the script has no more.
    fn next_item(&self) -> (usize, Option<Value>) {
        let mut s = lock(&self.scripted);
        let at = s.cursor.take();
        let item = s.script.items.get(at).cloned().or_else(|| if s.script.repeat_last { s.script.items.last().cloned() } else { None });
        (at, item)
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------------------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

const MAX_BODY: usize = 16 * 1024 * 1024;

/// The head of a request and the bytes of the body that came with it.
fn read_head(conn: &mut TcpStream) -> Result<(String, Vec<u8>), String> {
    let mut raw: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = conn.read(&mut buf).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("the client closed the connection before the end of its request".into());
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = raw.split_off(split + 4);
            raw.truncate(split);
            return Ok((String::from_utf8_lossy(&raw).into_owned(), rest));
        }
        if raw.len() > 64 * 1024 {
            return Err("the head of the request is longer than 64 KiB".into());
        }
    }
}

fn read_request(conn: &mut TcpStream) -> Result<Request, String> {
    let (head, mut body) = read_head(conn)?;
    let mut lines = head.lines();
    let first = lines.next().unwrap_or("");
    let mut words = first.split_whitespace();
    let (method, path) = (words.next().unwrap_or("").to_string(), words.next().unwrap_or("").to_string());
    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    let req = Request { method, path, headers, body: Vec::new() };
    let length: usize = req.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if length > MAX_BODY {
        return Err(format!("a body of {length} bytes is more than the fake takes"));
    }
    let mut buf = [0u8; 16 * 1024];
    while body.len() < length {
        let n = conn.read(&mut buf).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("the client closed the connection in the middle of its body".into());
        }
        body.extend_from_slice(&buf[..n]);
    }
    body.truncate(length);
    Ok(Request { body, ..req })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

fn respond(conn: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> io::Result<()> {
    let head = format!("HTTP/1.1 {status} {}\r\nServer: aqua-fake-llama\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", reason(status), body.len());
    conn.write_all(head.as_bytes())?;
    conn.write_all(body)?;
    conn.flush()
}

fn respond_json(conn: &mut TcpStream, status: u16, body: &Value) -> io::Result<()> {
    respond(conn, status, "application/json; charset=utf-8", body.to_string().as_bytes())
}

/// The error body of the real server.
fn error_body(status: u16, message: &str) -> Value {
    let kind = match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        404 => "not_found_error",
        503 => "unavailable_error",
        _ => "server_error",
    };
    json!({"error": {"code": status, "message": message, "type": kind}})
}

fn start_stream(conn: &mut TcpStream) -> io::Result<()> {
    conn.write_all(b"HTTP/1.1 200 OK\r\nServer: aqua-fake-llama\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")?;
    conn.flush()
}

fn write_chunk(conn: &mut TcpStream, bytes: &[u8]) -> io::Result<()> {
    conn.write_all(format!("{:x}\r\n", bytes.len()).as_bytes())?;
    conn.write_all(bytes)?;
    conn.write_all(b"\r\n")?;
    conn.flush()
}

/// One server-sent event. With `split_utf8` the first byte of the first non-ASCII character goes out alone and the rest follows after a pause, inside one HTTP chunk: a read of
/// the client then ends in the middle of the character.
fn write_event(conn: &mut TcpStream, data: &str, split_utf8: bool) -> io::Result<()> {
    let framed = format!("data: {data}\n\n").into_bytes();
    if split_utf8 {
        if let Some(at) = framed.iter().position(|b| *b >= 0x80) {
            let cut = at + 1;
            conn.write_all(format!("{:x}\r\n", framed.len()).as_bytes())?;
            conn.write_all(&framed[..cut])?;
            conn.flush()?;
            std::thread::sleep(Duration::from_millis(60));
            conn.write_all(&framed[cut..])?;
            conn.write_all(b"\r\n")?;
            return conn.flush();
        }
    }
    write_chunk(conn, &framed)
}

fn end_stream(conn: &mut TcpStream) -> io::Result<()> {
    conn.write_all(b"0\r\n\r\n")?;
    conn.flush()
}

/// Has the client closed its end? (The request has been read to the last byte, so anything else that happens on the socket is the client leaving.)
fn client_gone(conn: &TcpStream) -> bool {
    if conn.set_nonblocking(true).is_err() {
        return true;
    }
    let mut one = [0u8; 1];
    let gone = match conn.peek(&mut one) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => e.kind() != io::ErrorKind::WouldBlock,
    };
    let _ = conn.set_nonblocking(false);
    gone
}

/// Makes the close of this connection a reset (`SO_LINGER` with no time to linger), which the peer reads as an error and not as the quiet end of a file. `TcpStream::set_linger` is
/// not stable, so the option is set by hand where Windows has it; elsewhere the close stays a plain end (the peer still gets no answer).
#[cfg(windows)]
fn reset_on_close(conn: &TcpStream) {
    use std::os::windows::io::AsRawSocket;
    #[repr(C)]
    struct Linger {
        on: u16,
        seconds: u16,
    }
    #[link(name = "ws2_32")]
    extern "system" {
        fn setsockopt(socket: usize, level: i32, name: i32, value: *const Linger, length: i32) -> i32;
    }
    const SOL_SOCKET: i32 = 0xffff;
    const SO_LINGER: i32 = 0x0080;
    let value = Linger { on: 1, seconds: 0 };
    // SAFETY: the socket is open for as long as `conn` is borrowed, and `value` is a LINGER structure of the size that is passed.
    unsafe { setsockopt(conn.as_raw_socket() as usize, SOL_SOCKET, SO_LINGER, &value, std::mem::size_of::<Linger>() as i32) };
}

#[cfg(not(windows))]
fn reset_on_close(_conn: &TcpStream) {}

/// Waits `ms`, in short slices; `false` when the client hung up meanwhile.
fn wait_watching(conn: &TcpStream, ms: u64) -> bool {
    let until = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < until {
        std::thread::sleep((until - Instant::now()).min(Duration::from_millis(20)));
        if client_gone(conn) {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------------------------------------------------------------

fn handle(shared: &Shared, mut conn: TcpStream) {
    let _ = conn.set_nodelay(true);
    let _ = conn.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = conn.set_write_timeout(Some(Duration::from_secs(30)));
    let Ok(req) = read_request(&mut conn) else { return };
    let route = req.path.split('?').next().unwrap_or("").to_string();
    let open = route == "/health";
    if !open && !shared.authorized(&req) {
        if req.method == "POST" && route == "/v1/chat/completions" {
            let n = shared.begin_hit(hit_of(shared, &req, &Value::Null, "unauthorized"));
            shared.finish_hit(n, "unauthorized");
        }
        if req.method == "POST" && route.starts_with("/slots/") {
            shared.slot_event(&req, json!({"status": 401, "outcome": "unauthorized"}));
        }
        let _ = respond_json(&mut conn, 401, &error_body(401, "Invalid API Key"));
        return;
    }
    let _ = match (req.method.as_str(), route.as_str()) {
        ("GET", "/health") => {
            if shared.loading() {
                respond_json(&mut conn, 503, &error_body(503, "Loading model"))
            } else {
                respond_json(&mut conn, 200, &json!({"status": "ok"}))
            }
        }
        ("GET", "/slots") => {
            let slots: Vec<Value> = lock(&shared.slots).iter().enumerate().map(|(id, s)| json!({"id": id, "id_task": -1, "n_ctx": shared.window, "speculative": false, "is_processing": s.busy})).collect();
            respond_json(&mut conn, 200, &Value::Array(slots))
        }
        ("POST", slots) if slots.starts_with("/slots/") => {
            let (status, body) = slot_action(shared, &req);
            respond_json(&mut conn, status, &body)
        }
        ("GET", "/v1/models") => respond_json(&mut conn, 200, &json!({"object": "list", "data": [{"id": shared.model, "object": "model", "owned_by": "aqua-fake-llama"}]})),
        ("POST", "/tokenize") => {
            let text = serde_json::from_slice::<Value>(&req.body).ok().and_then(|v| v["content"].as_str().map(str::to_string)).unwrap_or_default();
            respond_json(&mut conn, 200, &json!({"tokens": vec![0u64; tokens_of(text.chars().count()) as usize]}))
        }
        ("GET", "/_hits") => respond_json(&mut conn, 200, &Value::Array(lock(&shared.hits).iter().cloned().map(Value::Object).collect())),
        ("POST", "/_script") => match serde_json::from_slice::<Value>(&req.body).map_err(|e| e.to_string()).and_then(|v| Script::parse(&v)) {
            Ok(script) => {
                let items = script.items.len();
                {
                    let mut s = lock(&shared.scripted);
                    s.script = script;
                    s.cursor.reset();
                }
                respond_json(&mut conn, 200, &json!({"ok": true, "items": items}))
            }
            Err(why) => respond_json(&mut conn, 400, &error_body(400, &why)),
        },
        ("POST", "/v1/chat/completions") => {
            // The connection is dropped when this function ends, after the outcome is noted: a client that has seen the end of the answer finds the record complete.
            complete(shared, &mut conn, &req);
            return;
        }
        _ => respond_json(&mut conn, 404, &error_body(404, "File Not Found")),
    };
}

/// The first line of a file this program writes for a saved slot; the second is the name of the model file, the rest is what the slot had read.
const SLOT_FILE_MAGIC: &str = "AQUA-FAKE-KV 1";

/// The error of a request for a slot, as the real server writes it.
fn slot_error(status: u16, message: &str, kind: &str) -> Value {
    let mut body = error_body(status, message);
    body["error"]["type"] = json!(kind);
    body
}

/// A save or a restore of one slot, the way the real server answers it. Gives the status and the body, and notes the request.
fn slot_action(shared: &Shared, req: &Request) -> (u16, Value) {
    let (status, body, outcome) = slot_reply(shared, req);
    shared.slot_event(req, json!({"status": status, "outcome": outcome}));
    (status, body)
}

fn slot_reply(shared: &Shared, req: &Request) -> (u16, Value, &'static str) {
    if !shared.slot_endpoint {
        // A build that has no such endpoint: the route is not there.
        return (404, error_body(404, "File Not Found"), "no_endpoint");
    }
    let Some(dir) = &shared.slot_dir else {
        return (501, slot_error(501, "This server does not support slots action. Start it with `--slot-save-path`", "not_supported_error"), "not_supported");
    };
    let (path, query) = req.path.split_once('?').unwrap_or((req.path.as_str(), ""));
    let action = query.split('&').find_map(|kv| kv.strip_prefix("action=")).unwrap_or("");
    let Some(id) = path.trim_start_matches("/slots/").parse::<usize>().ok().filter(|id| *id < lock(&shared.slots).len()) else {
        return (400, slot_error(400, "Invalid slot ID", "invalid_request_error"), "bad_slot");
    };
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    let Some(file) = body["filename"].as_str().filter(|f| !f.is_empty() && !f.contains(['/', '\\', ':']) && !f.contains("..")) else {
        return (400, slot_error(400, "Invalid filename", "invalid_request_error"), "bad_filename");
    };
    let model_file = Path::new(&shared.model).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match action {
        "save" => {
            if let Some(status) = lock(&shared.scripted).script.slot_save_status {
                return (status, slot_error(status, "the scripted refusal of a save", "server_error"), "refused");
            }
            let prompt = lock(&shared.slots)[id].prompt.clone();
            let text = format!("{SLOT_FILE_MAGIC}\n{model_file}\n{prompt}");
            if std::fs::write(dir.join(file), &text).is_err() {
                return (500, slot_error(500, "Unable to write the slot save file", "server_error"), "write_failed");
            }
            (200, json!({"id_slot": id, "filename": file, "n_saved": tokens_of(prompt.chars().count()), "n_written": text.len(), "timings": {"save_ms": 1.5}}), "saved")
        }
        "restore" => {
            if let Some(status) = lock(&shared.scripted).script.slot_restore_status {
                return (status, slot_error(status, "the scripted refusal of a restore", "server_error"), "refused");
            }
            let refused = || (400, slot_error(400, "Unable to restore slot, no available space in KV cache or invalid slot save file", "invalid_request_error"), "invalid_file");
            let Ok(text) = std::fs::read_to_string(dir.join(file)) else { return refused() };
            let mut parts = text.splitn(3, '\n');
            match (parts.next(), parts.next(), parts.next()) {
                (Some(SLOT_FILE_MAGIC), Some(model), Some(prompt)) if model == model_file => {
                    lock(&shared.slots)[id].prompt = prompt.to_string();
                    (200, json!({"id_slot": id, "filename": file, "n_restored": tokens_of(prompt.chars().count()), "n_read": text.len(), "timings": {"restore_ms": 0.8}}), "restored")
                }
                _ => refused(),
            }
        }
        _ => (400, slot_error(400, "Invalid action", "invalid_request_error"), "bad_action"),
    }
}

/// The prompt: the text of the messages, joined.
fn prompt_of(body: &Value) -> String {
    let mut out = Vec::new();
    for m in body["messages"].as_array().into_iter().flatten() {
        match &m["content"] {
            Value::String(s) => out.push(s.clone()),
            Value::Array(parts) => out.extend(parts.iter().filter_map(|p| p["text"].as_str().map(str::to_string))),
            _ => {}
        }
    }
    out.join("\n")
}

fn hit_of(shared: &Shared, req: &Request, body: &Value, outcome: &str) -> Map<String, Value> {
    let prompt = prompt_of(body);
    let chars = prompt.chars().count();
    let head: String = prompt.chars().take(80).collect();
    let tail: String = prompt.chars().skip(chars.saturating_sub(80)).collect();
    let mut hit = Map::new();
    hit.insert("path".into(), json!(req.path));
    hit.insert("authorized".into(), json!(outcome != "unauthorized"));
    hit.insert("stream".into(), json!(body["stream"].as_bool() == Some(true)));
    hit.insert("max_tokens".into(), body["max_tokens"].clone());
    hit.insert("id_slot".into(), body["id_slot"].clone());
    hit.insert("cache_prompt".into(), body["cache_prompt"].clone());
    hit.insert("constrained".into(), json!(body.get("grammar").is_some() || body.get("response_format").is_some()));
    hit.insert("prompt_chars".into(), json!(chars));
    hit.insert("prompt_head".into(), json!(head));
    hit.insert("prompt_tail".into(), json!(tail));
    hit.insert("at_ms".into(), json!(shared.started.elapsed().as_millis() as u64));
    hit
}

/// The timings of an answer, in the keys the real server uses; `over` replaces some of them.
fn timings_of(prompt_n: u64, cache_n: u64, prompt_ms: u64, predicted_n: u64, predicted_ms: u64, over: &Value) -> Value {
    let per_second = |n: u64, ms: u64| n as f64 * 1000.0 / ms.max(1) as f64;
    let mut t = json!({
        "cache_n": cache_n,
        "prompt_n": prompt_n,
        "prompt_ms": prompt_ms,
        "prompt_per_token_ms": prompt_ms as f64 / prompt_n.max(1) as f64,
        "prompt_per_second": per_second(prompt_n, prompt_ms),
        "predicted_n": predicted_n,
        "predicted_ms": predicted_ms,
        "predicted_per_token_ms": predicted_ms as f64 / predicted_n.max(1) as f64,
        "predicted_per_second": per_second(predicted_n, predicted_ms),
    });
    if let (Some(t), Some(over)) = (t.as_object_mut(), over.as_object()) {
        t.extend(over.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    t
}

/// The pieces of an answer from a `text` item.
fn chunks_of(item: &Value) -> Vec<String> {
    if let Some(list) = item["chunks"].as_array() {
        return list.iter().map(|c| c.as_str().map(str::to_string).unwrap_or_else(|| c.to_string())).collect();
    }
    let text = item["text"].as_str().unwrap_or("ok").to_string();
    match item["chunk_chars"].as_u64().filter(|n| *n > 0) {
        Some(n) => text.chars().collect::<Vec<_>>().chunks(n as usize).map(|c| c.iter().collect()).collect(),
        None => vec![text],
    }
}

/// One `data:` event of a streamed answer.
fn delta_event(id: &str, delta: Value, finish: Value, extra: Value) -> String {
    let mut event = json!({"choices": [{"finish_reason": finish, "index": 0, "delta": delta}], "created": 0, "id": id, "model": "fake", "system_fingerprint": "fake", "object": "chat.completion.chunk"});
    if let (Some(e), Some(extra)) = (event.as_object_mut(), extra.as_object()) {
        e.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    event.to_string()
}

/// The prompt-reading time of an answer: what the item says, and the per-token time of the script for what was not in the cache.
fn prompt_ms_of(item: &Value, per_token: f64, fresh_tokens: u64) -> u64 {
    item["prompt_delay_ms"].as_u64().unwrap_or(0) + (per_token * fresh_tokens as f64).round() as u64
}

fn complete(shared: &Shared, conn: &mut TcpStream, req: &Request) {
    let body: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(e) => {
            let _ = respond_json(conn, 400, &error_body(400, &format!("invalid JSON in the request: {e}")));
            return;
        }
    };
    let prompt = prompt_of(&body);
    let stream = body["stream"].as_bool() == Some(true);
    let queued_at = Instant::now();
    let n = shared.begin_hit(hit_of(shared, req, &body, "ok"));
    if shared.loading() {
        shared.finish_hit(n, "loading");
        let _ = respond_json(conn, 503, &error_body(503, "Loading model"));
        return;
    }
    let wanted = body["id_slot"].as_u64().map(|s| s as usize);
    let Some((slot, shared_chars)) = shared.acquire(wanted, &prompt, conn) else {
        shared.finish_hit(n, "client_gone_while_queued");
        return;
    };
    let concurrent = shared.serving.fetch_add(1, Ordering::SeqCst) + 1;
    shared.note_hit(n, "concurrent", json!(concurrent));
    shared.note_hit(n, "queued_ms", json!(queued_at.elapsed().as_millis() as u64));
    shared.note_hit(n, "slot", json!(slot));
    let cache_n = if body["cache_prompt"].as_bool() == Some(false) { 0 } else { tokens_of(shared_chars) };
    let prompt_tokens = tokens_of(prompt.chars().count());
    shared.note_hit(n, "cache_n", json!(cache_n));
    let (outcome, read) = serve(shared, conn, &body, stream, n, prompt_tokens, cache_n);
    shared.finish_hit(n, outcome);
    shared.serving.fetch_sub(1, Ordering::SeqCst);
    shared.release(slot, read.then_some(prompt.as_str()));
}

/// Answers one request as the next item says. Gives the outcome, and whether the slot has read the prompt.
#[allow(clippy::too_many_arguments)]
fn serve(shared: &Shared, conn: &mut TcpStream, body: &Value, stream: bool, n: usize, prompt_tokens: u64, cache_n: u64) -> (&'static str, bool) {
    let fresh_tokens = prompt_tokens.saturating_sub(cache_n).max(1);
    let (enforce, per_token, window, parallel, warmups_are_free) = {
        let s = lock(&shared.scripted);
        (s.script.enforce_context, s.script.prompt_ms_per_token, shared.window, lock(&shared.slots).len() as u32, s.script.warmups_are_free)
    };
    if enforce && prompt_tokens > u64::from(window / parallel.max(1)) {
        let mut error = error_body(400, "the request exceeds the available context size, try increasing it");
        error["error"]["type"] = json!("exceed_context_size_error");
        error["error"]["n_prompt_tokens"] = json!(prompt_tokens);
        error["error"]["n_ctx"] = json!(window / parallel.max(1));
        let _ = respond_json(conn, 400, &error);
        return ("context_exceeded", false);
    }
    if warmups_are_free && body["max_tokens"].as_u64().is_some_and(|m| m <= 1) {
        shared.note_hit(n, "item", json!("warmup"));
        return serve_text(conn, &json!({"type": "text", "text": "ok"}), body, stream, n, prompt_tokens, cache_n, 0);
    }
    let (at, item) = shared.next_item();
    shared.note_hit(n, "item_index", json!(at));
    let Some(item) = item else {
        shared.note_hit(n, "item", json!("none"));
        let _ = respond_json(conn, 500, &error_body(500, &format!("the fake model server has no scripted reply for request {at}: its script has no more items")));
        return ("script_used_up", false);
    };
    let kind = item["type"].as_str().unwrap_or("text").to_string();
    shared.note_hit(n, "item", json!(kind));
    let prompt_ms = prompt_ms_of(&item, per_token, fresh_tokens);
    match kind.as_str() {
        "text" => serve_text(conn, &item, body, stream, n, prompt_tokens, cache_n, prompt_ms),
        "json" => {
            if !wait_watching(conn, prompt_ms) {
                return ("client_gone", false);
            }
            let value = item["value"].clone();
            let sent = if stream {
                start_stream(conn).and_then(|_| write_event(conn, &value.to_string(), false)).and_then(|_| write_event(conn, "[DONE]", false)).and_then(|_| end_stream(conn))
            } else {
                respond_json(conn, 200, &value)
            };
            (if sent.is_ok() { "answered" } else { "client_gone" }, false)
        }
        "http_error" => {
            if !wait_watching(conn, prompt_ms) {
                return ("client_gone", false);
            }
            let status = item["status"].as_u64().unwrap_or(500) as u16;
            let sent = match &item["body"] {
                Value::Null => respond_json(conn, status, &error_body(status, "the scripted error of the fake model server")),
                Value::String(text) => respond(conn, status, "text/plain; charset=utf-8", text.as_bytes()),
                other => respond_json(conn, status, other),
            };
            (if sent.is_ok() { "http_error" } else { "client_gone" }, false)
        }
        "hang" => {
            let until = Instant::now() + Duration::from_millis(item["max_ms"].as_u64().unwrap_or(600_000));
            let stuck = item["ignore_hangup"].as_bool() == Some(true);
            while Instant::now() < until {
                if !stuck && client_gone(conn) {
                    return ("client_gone", false);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            ("hang_expired", false)
        }
        "reset" => {
            if wait_watching(conn, item["after_ms"].as_u64().unwrap_or(0)) {
                // The connection is dropped by the caller, and a close with no time to linger is a reset.
                reset_on_close(conn);
                return ("reset", false);
            }
            ("client_gone", false)
        }
        "cut" => serve_cut(conn, &item, stream, prompt_tokens, cache_n, prompt_ms),
        "exit_after_load" => {
            shared.finish_hit(n, "exit_after_load");
            std::process::exit(item["code"].as_i64().unwrap_or(3) as i32);
        }
        _ => unreachable!("the script was checked when it was read"),
    }
}

#[allow(clippy::too_many_arguments)]
fn serve_text(conn: &mut TcpStream, item: &Value, body: &Value, stream: bool, n: usize, prompt_tokens: u64, cache_n: u64, prompt_ms: u64) -> (&'static str, bool) {
    let pieces = chunks_of(item);
    let delay = item["chunk_delay_ms"].as_u64().unwrap_or(0);
    let finish = item["finish_reason"].as_str().unwrap_or("stop").to_string();
    let split = item["split_utf8"].as_bool() == Some(true);
    let id = format!("chatcmpl-fake-{n}");
    let fresh = prompt_tokens.saturating_sub(cache_n).max(1);
    let predicted = pieces.len().max(1) as u64;
    let timings = timings_of(fresh, cache_n, prompt_ms, predicted, delay * pieces.len() as u64, &item["timings"]);
    let usage = json!({"prompt_tokens": prompt_tokens, "completion_tokens": predicted, "total_tokens": prompt_tokens + predicted});
    if !wait_watching(conn, prompt_ms) {
        return ("client_gone", false);
    }
    if !stream {
        if !wait_watching(conn, delay * pieces.len() as u64) {
            return ("client_gone", false);
        }
        let mut choice = json!({"finish_reason": finish, "index": 0, "message": {"role": "assistant", "content": pieces.concat()}});
        if body["logprobs"].as_bool() == Some(true) {
            if let Some(top) = item["top_logprobs"].as_array() {
                let listed: Vec<Value> = top.iter().filter_map(|p| Some(json!({"token": p.get(0)?, "logprob": p.get(1)?}))).collect();
                choice["logprobs"] = json!({"content": [{"token": listed.first().map(|t| t["token"].clone()).unwrap_or(Value::Null), "top_logprobs": listed}]});
            }
        }
        let reply = json!({"choices": [choice], "created": 0, "id": id, "model": "fake", "system_fingerprint": "fake", "object": "chat.completion", "usage": usage, "timings": timings});
        return (if respond_json(conn, 200, &reply).is_ok() { "answered" } else { "client_gone" }, true);
    }
    let sent = (|| -> Result<bool, io::Error> {
        start_stream(conn)?;
        write_event(conn, &delta_event(&id, json!({"role": "assistant", "content": null}), Value::Null, json!({})), false)?;
        for piece in &pieces {
            if !wait_watching(conn, delay) {
                return Ok(false);
            }
            write_event(conn, &delta_event(&id, json!({"content": piece}), Value::Null, json!({})), split)?;
        }
        write_event(conn, &delta_event(&id, json!({}), json!(finish), json!({"usage": usage, "timings": timings})), false)?;
        write_event(conn, "[DONE]", false)?;
        end_stream(conn)?;
        Ok(true)
    })();
    match sent {
        Ok(true) => ("answered", true),
        _ => ("client_gone", false),
    }
}

fn serve_cut(conn: &mut TcpStream, item: &Value, stream: bool, prompt_tokens: u64, cache_n: u64, prompt_ms: u64) -> (&'static str, bool) {
    let pieces = if item["chunks"].is_array() { chunks_of(item) } else { vec!["Hello ".to_string(), "wor".to_string(), "ld".to_string()] };
    let after = item["after_chunks"].as_u64().unwrap_or(2) as usize;
    let delay = item["chunk_delay_ms"].as_u64().unwrap_or(0);
    let abrupt = item["abrupt"].as_bool() == Some(true);
    if !wait_watching(conn, prompt_ms) {
        return ("client_gone", false);
    }
    let sent = (|| -> Result<(), io::Error> {
        if stream {
            start_stream(conn)?;
            write_event(conn, &delta_event("chatcmpl-fake-cut", json!({"role": "assistant", "content": null}), Value::Null, json!({})), false)?;
            for piece in pieces.iter().take(after) {
                if !wait_watching(conn, delay) {
                    return Err(io::ErrorKind::ConnectionAborted.into());
                }
                write_event(conn, &delta_event("chatcmpl-fake-cut", json!({"content": piece}), Value::Null, json!({})), false)?;
            }
        } else {
            let full = json!({"choices": [{"finish_reason": "stop", "index": 0, "message": {"role": "assistant", "content": pieces.concat()}}], "timings": timings_of(prompt_tokens, cache_n, prompt_ms, 3, 0, &Value::Null)}).to_string();
            let head = format!("HTTP/1.1 200 OK\r\nServer: aqua-fake-llama\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", full.len());
            conn.write_all(head.as_bytes())?;
            conn.write_all(&full.as_bytes()[..full.len() / 2])?;
            conn.flush()?;
        }
        Ok(())
    })();
    if sent.is_err() {
        return ("client_gone", false);
    }
    // Some time for the client to read what was sent: a reset can throw away what has not been read yet. The caller drops the connection: a plain end, or a reset when `abrupt`.
    std::thread::sleep(Duration::from_millis(60));
    if abrupt {
        reset_on_close(conn);
    }
    ("cut", false)
}

/// Accepts connections for ever, one thread each.
fn serve_forever(listener: TcpListener, shared: Arc<Shared>) {
    for conn in listener.incoming().flatten() {
        let shared = shared.clone();
        let _ = std::thread::Builder::new().name("fake-llama-connection".into()).spawn(move || handle(&shared, conn));
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------
// The program
// ---------------------------------------------------------------------------------------------------------------------------------------------------

fn load_script(args: &Args) -> Result<(Script, Cursor), String> {
    // A second server for another model file (the fast tier's) reads the script of its own: FAKE_LLAMA_FAST_MODEL names that file, FAKE_LLAMA_FAST_SCRIPT is the script.
    let for_the_fast_model = std::env::var("FAKE_LLAMA_FAST_MODEL").ok().filter(|m| !m.is_empty()).is_some_and(|m| Path::new(&args.model).file_name().is_some_and(|f| f.to_string_lossy() == m.as_str()));
    let name = if for_the_fast_model {
        "FAKE_LLAMA_FAST_SCRIPT"
    } else if args.device.is_some() {
        "FAKE_LLAMA_GPU_SCRIPT"
    } else {
        "FAKE_LLAMA_SCRIPT"
    };
    let Ok(path) = std::env::var(name) else {
        // No script: every request is answered `500`, saying so (the start and the health check still work).
        return Ok((Script::default(), Cursor { file: None, next: 0 }));
    };
    let path = PathBuf::from(path);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("error: the script {} cannot be read: {e}", path.display()))?;
    let value: Value = serde_json::from_str(&text).map_err(|e| format!("error: the script {} is not JSON: {e}", path.display()))?;
    let script = Script::parse(&value).map_err(|e| format!("error: the script {}: {e}", path.display()))?;
    Ok((script, Cursor::for_script_file(&path)))
}

/// Writes `message` where the real server writes its errors, so that the client can quote it.
fn write_log(args: &Args, message: &str) {
    if let Some(file) = &args.log_file {
        let _ = std::fs::write(file, format!("{message}\n"));
    }
}

fn run(argv: &[String]) -> i32 {
    let args = match parse_args(argv) {
        Ok(a) => a,
        Err(why) => {
            eprintln!("{why}");
            return 2;
        }
    };
    if args.version {
        println!("version: 0 (aqua-fake-llama)");
        return 0;
    }
    if args.help {
        print!("{}", std::env::var("FAKE_LLAMA_HELP").unwrap_or_else(|_| HELP.to_string()));
        return 0;
    }
    if args.list_devices {
        print!("{}", std::env::var("FAKE_LLAMA_DEVICES").unwrap_or_else(|_| DEVICES.to_string()));
        return 0;
    }
    let (script, cursor) = match load_script(&args) {
        Ok(s) => s,
        Err(why) => {
            write_log(&args, &why);
            eprintln!("{why}");
            return 2;
        }
    };
    if let Some((code, log)) = &script.exit_while_loading {
        std::thread::sleep(Duration::from_millis(script.load_ms.min(5_000)));
        write_log(&args, log);
        eprintln!("{log}");
        return *code;
    }
    let listener = match TcpListener::bind(("127.0.0.1", args.port.unwrap_or(0))) {
        Ok(l) => l,
        Err(e) => {
            let why = format!("error: couldn't bind HTTP server socket, hostname: 127.0.0.1, port: {}: {e}", args.port.unwrap_or(0));
            write_log(&args, &why);
            eprintln!("{why}");
            return 1;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let key = std::env::var("LLAMA_API_KEY").ok().filter(|k| !k.is_empty());
    let mode = if args.device.is_some() { "gpu" } else { "cpu" };
    let events = std::env::var("FAKE_LLAMA_EVENTS").ok().map(PathBuf::from);
    let shared = Arc::new(Shared::new(key, mode, &args, Scripted { script, cursor }, events));
    shared.event("start", json!({"port": port, "args": argv, "model": args.model, "parallel": args.parallel, "window": args.window, "device": args.device, "keyed": shared.key.is_some()}));
    println!("aqua-fake-llama listening on 127.0.0.1:{port}");
    serve_forever(listener, shared);
    0
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(run(&argv));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// The server on a thread of this process, with this script; its port.
    fn start(script: Value, key: Option<&str>, parallel: usize, window: u32) -> (u16, Arc<Shared>) {
        start_with(script, key, parallel, window, None, false)
    }

    /// The server with the slot endpoint of a build that has it (FAKE_LLAMA_SLOTS=on) and, when dir is given, started with --slot-save-path dir.
    fn start_with(script: Value, key: Option<&str>, parallel: usize, window: u32, dir: Option<&Path>, endpoint: bool) -> (u16, Arc<Shared>) {
        let args = Args { model: "m.gguf".into(), host: "127.0.0.1".into(), port: None, parallel, window, log_file: None, device: None, slot_save_path: dir.map(Path::to_path_buf), list_devices: false, help: false, version: false };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut shared = Shared::new(key.map(str::to_string), "cpu", &args, Scripted { script: Script::parse(&script).unwrap(), cursor: Cursor { file: None, next: 0 } }, None);
        shared.slot_endpoint = endpoint;
        let shared = Arc::new(shared);
        let for_thread = shared.clone();
        std::thread::spawn(move || serve_forever(listener, for_thread));
        (port, shared)
    }

    /// What came back: the status, the head, and the body (an HTTP-chunked body is joined).
    struct Answer {
        status: u16,
        head: String,
        body: Vec<u8>,
    }

    impl Answer {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.body).into_owned()
        }
        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
        }
        /// The `data:` of each server-sent event.
        fn events(&self) -> Vec<String> {
            self.text().split("\n\n").filter_map(|e| e.strip_prefix("data: ")).map(str::to_string).collect()
        }
    }

    fn dechunk(mut rest: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(end) = rest.windows(2).position(|w| w == b"\r\n") {
            let size = usize::from_str_radix(String::from_utf8_lossy(&rest[..end]).trim(), 16).unwrap_or(0);
            rest = &rest[end + 2..];
            if size == 0 || rest.len() < size {
                break;
            }
            out.extend_from_slice(&rest[..size]);
            rest = rest.get(size + 2..).unwrap_or(&[]);
        }
        out
    }

    fn send(port: u16, method: &str, path: &str, key: Option<&str>, body: Option<&Value>) -> TcpStream {
        let mut conn = TcpStream::connect(("127.0.0.1", port)).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let payload = body.map(Value::to_string).unwrap_or_default();
        let auth = key.map(|k| format!("Authorization: Bearer {k}\r\n")).unwrap_or_default();
        conn.write_all(format!("{method} {path} HTTP/1.1\r\nHost: x\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len()).as_bytes()).unwrap();
        conn
    }

    fn read_answer(mut conn: TcpStream) -> Answer {
        let mut raw = Vec::new();
        let _ = conn.read_to_end(&mut raw);
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or_else(|| panic!("no head in {:?}", String::from_utf8_lossy(&raw)));
        let head = String::from_utf8_lossy(&raw[..split]).into_owned();
        let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap();
        let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { dechunk(&raw[split + 4..]) } else { raw[split + 4..].to_vec() };
        Answer { status, head, body }
    }

    fn ask(port: u16, method: &str, path: &str, key: Option<&str>, body: Option<&Value>) -> Answer {
        read_answer(send(port, method, path, key, body))
    }

    fn chat(prompt: &str, stream: bool) -> Value {
        json!({"messages": [{"role": "user", "content": prompt}], "max_tokens": 50, "stream": stream, "cache_prompt": true})
    }

    #[test]
    fn the_arguments_of_the_real_launch_are_read_and_the_ports_of_aqua_and_other_hosts_are_refused() {
        let launch = strings(&["-m", "C:/m/model.gguf", "--host", "127.0.0.1", "--port", "52000", "-c", "6144", "-t", "6", "-tb", "6", "--reasoning", "off", "--no-webui", "--parallel", "2", "-lv", "1", "--log-file", "C:/l/s.log", "-ngl", "22", "-dev", "Vulkan1", "--fit-target", "1660"]);
        let a = parse_args(&launch).unwrap();
        assert_eq!((a.model.as_str(), a.port, a.window, a.parallel, a.device.as_deref()), ("C:/m/model.gguf", Some(52000), 6144, 2, Some("Vulkan1")));
        assert_eq!(a.log_file, Some(PathBuf::from("C:/l/s.log")));
        for port in ["8767", "8766", "1420", "80", "1023"] {
            let why = parse_args(&strings(&["--port", port])).unwrap_err();
            assert!(why.contains("refusing port"), "{port}: {why}");
        }
        assert!(parse_args(&strings(&["--port", "1024"])).is_ok() && parse_args(&strings(&["--port", "0"])).is_ok());
        assert!(parse_args(&strings(&["--host", "0.0.0.0"])).unwrap_err().contains("loopback"));
        assert!(parse_args(&strings(&["--port", "many"])).is_err() && parse_args(&strings(&["--port"])).is_err());
        assert_eq!(run(&strings(&["--port", "8767"])), 2, "the program ends without listening");
    }

    #[test]
    fn a_port_that_is_taken_ends_the_program_with_a_message_in_the_log_file() {
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let log = std::env::temp_dir().join(format!("aqua_fake_llama_unit_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&log);
        assert_eq!(run(&strings(&["--port", &port.to_string(), "--log-file", &log.to_string_lossy()])), 1);
        assert!(std::fs::read_to_string(&log).unwrap().contains("couldn't bind HTTP server socket"));
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn a_script_is_a_list_of_items_with_texts_as_a_short_form_and_a_wrong_item_is_named() {
        let s = Script::parse(&json!(["one", {"type": "hang"}])).unwrap();
        assert_eq!((s.items.len(), s.items[0]["chunks"][0].as_str(), s.items[1]["type"].as_str()), (2, Some("one"), Some("hang")));
        let s = Script::parse(&json!({"load_ms": 40, "enforce_context": true, "repeat_last": true, "prompt_ms_per_token": 2.5, "exit_while_loading": {"code": 7, "log": "boom"}, "items": []})).unwrap();
        assert_eq!((s.load_ms, s.enforce_context, s.repeat_last, s.prompt_ms_per_token, s.exit_while_loading), (40, true, true, 2.5, Some((7, "boom".to_string()))));
        assert!(Script::parse(&json!([{"type": "explode"}])).unwrap_err().contains("item 0") );
        assert!(Script::parse(&json!(["ok", 5])).unwrap_err().contains("item 1"));
        assert!(Script::parse(&json!("text")).is_err());
    }

    #[test]
    fn the_position_in_a_script_file_survives_a_restart() {
        let file = std::env::temp_dir().join(format!("aqua_fake_llama_cursor_{}.json", std::process::id()));
        let mut cursor = Cursor::for_script_file(&file);
        let _ = std::fs::remove_file(&cursor.file.clone().unwrap());
        assert_eq!((cursor.take(), cursor.take()), (0, 1));
        let mut after_restart = Cursor::for_script_file(&file);
        assert_eq!(after_restart.take(), 2, "a new process goes on where the last one stopped");
        after_restart.reset();
        assert_eq!(cursor.take(), 0);
        let _ = std::fs::remove_file(cursor.file.clone().unwrap());
    }

    #[test]
    fn the_health_check_has_no_key_and_says_loading_until_the_model_is_loaded() {
        let (port, _) = start(json!({"load_ms": 700, "items": ["x"]}), Some("secret"), 1, 6144);
        let loading = ask(port, "GET", "/health", None, None);
        assert_eq!(loading.status, 503, "{}", loading.text());
        assert_eq!(loading.json()["error"]["message"], "Loading model");
        let too_early = ask(port, "POST", "/v1/chat/completions", Some("secret"), Some(&chat("hi", false)));
        assert_eq!(too_early.status, 503, "a request is refused while the model loads");
        std::thread::sleep(Duration::from_millis(750));
        let ready = ask(port, "GET", "/health", None, None);
        assert_eq!((ready.status, ready.json()["status"].clone()), (200, json!("ok")));
    }

    #[test]
    fn every_route_but_the_health_check_needs_the_key() {
        let (port, shared) = start(json!(["x"]), Some("secret"), 1, 6144);
        for (method, path) in [("GET", "/slots"), ("POST", "/v1/chat/completions"), ("POST", "/tokenize"), ("GET", "/_hits"), ("POST", "/_script"), ("GET", "/v1/models")] {
            let none = ask(port, method, path, None, Some(&chat("hi", false)));
            let wrong = ask(port, method, path, Some("other"), Some(&chat("hi", false)));
            assert_eq!((none.status, wrong.status), (401, 401), "{method} {path}");
            assert_eq!(none.json()["error"]["type"], "authentication_error");
        }
        assert_eq!(ask(port, "GET", "/health", None, None).status, 200);
        assert_eq!(ask(port, "GET", "/slots", Some("secret"), None).status, 200);
        assert!(lock(&shared.hits).iter().all(|h| h["outcome"] == "unauthorized" && h["authorized"] == false), "a refused request is noted, and uses no item");
        assert_eq!(ask(port, "POST", "/v1/chat/completions", Some("secret"), Some(&chat("hi", false))).json()["choices"][0]["message"]["content"], "x", "the first item was still there");
        // Without a key set, nothing is asked for.
        let (open, _) = start(json!(["y"]), None, 1, 6144);
        assert_eq!(ask(open, "GET", "/slots", None, None).status, 200);
    }

    #[test]
    fn a_text_answer_is_one_json_body_or_server_sent_events_like_the_real_server() {
        let (port, _) = start(json!({"repeat_last": true, "items": [{"type": "text", "chunks": ["Hello ", "world"], "timings": {"predicted_per_second": 4.5}}]}), None, 1, 6144);
        let whole = ask(port, "POST", "/v1/chat/completions", None, Some(&chat("a prompt of some length", false)));
        let v = whole.json();
        assert_eq!((whole.status, v["choices"][0]["message"]["content"].as_str(), v["choices"][0]["finish_reason"].as_str()), (200, Some("Hello world"), Some("stop")));
        assert!(whole.head.to_ascii_lowercase().contains("content-length"), "{}", whole.head);
        assert_eq!((v["timings"]["predicted_n"].as_u64(), v["timings"]["predicted_per_second"].as_f64()), (Some(2), Some(4.5)), "an item can replace a number");
        assert_eq!(v["timings"]["prompt_n"].as_u64(), Some(6), "23 characters are 6 tokens when nothing is in the cache");

        let streamed = ask(port, "POST", "/v1/chat/completions", None, Some(&chat("a prompt of some length", true)));
        assert!(streamed.head.to_ascii_lowercase().contains("transfer-encoding: chunked") && streamed.head.contains("text/event-stream"), "{}", streamed.head);
        let events = streamed.events();
        assert_eq!(events.last().map(String::as_str), Some("[DONE]"));
        let parsed: Vec<Value> = events[..events.len() - 1].iter().map(|e| serde_json::from_str(e).unwrap()).collect();
        assert_eq!(parsed[0]["choices"][0]["delta"]["role"], "assistant");
        let words: Vec<&str> = parsed.iter().filter_map(|e| e["choices"][0]["delta"]["content"].as_str()).collect();
        assert_eq!(words, ["Hello ", "world"]);
        let last = parsed.last().unwrap();
        assert_eq!((last["choices"][0]["finish_reason"].as_str(), last["timings"]["cache_n"].as_u64()), (Some("stop"), Some(6)), "the second ask of this prompt came from the cache");
    }

    #[test]
    fn a_request_for_one_token_takes_no_item_when_the_script_says_warmups_are_free() {
        let items = json!([{"type": "text", "chunks": ["first"]}, {"type": "text", "chunks": ["second"]}]);
        let warmup = |stream: bool| json!({"messages": [{"role": "user", "content": "the start of a prompt"}], "max_tokens": 1, "stream": stream, "cache_prompt": true});
        let (port, shared) = start(json!({"warmups_are_free": true, "items": items.clone()}), None, 1, 6144);
        let a = ask(port, "POST", "/v1/chat/completions", None, Some(&warmup(false)));
        assert_eq!((a.status, a.json()["choices"][0]["message"]["content"].as_str()), (200, Some("ok")), "the read of a prompt is answered");
        let first = ask(port, "POST", "/v1/chat/completions", None, Some(&chat("a prompt of some length", false)));
        assert_eq!(first.json()["choices"][0]["message"]["content"].as_str(), Some("first"), "and the first item is still the first");
        let _ = ask(port, "POST", "/v1/chat/completions", None, Some(&warmup(true)));
        let second = ask(port, "POST", "/v1/chat/completions", None, Some(&chat("a prompt of some length", false)));
        assert_eq!(second.json()["choices"][0]["message"]["content"].as_str(), Some("second"));
        assert_eq!(lock(&shared.hits).iter().map(|h| h["item"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>(), ["warmup", "text", "warmup", "text"]);
        // Without the option a request for one token takes an item like any other.
        let (port, _) = start(json!({"items": items}), None, 1, 6144);
        let a = ask(port, "POST", "/v1/chat/completions", None, Some(&warmup(false)));
        assert_eq!(a.json()["choices"][0]["message"]["content"].as_str(), Some("first"));
    }

    #[test]
    fn what_a_slot_has_read_is_the_cache_of_the_next_prompt() {
        let (port, _) = start(json!({"repeat_last": true, "items": ["ok"]}), None, 1, 6144);
        let start_of_prompt = "The rules of the agent are these. ".repeat(10);
        let first = ask(port, "POST", "/v1/chat/completions", None, Some(&chat(&format!("{start_of_prompt}Goal: one"), false))).json();
        let second = ask(port, "POST", "/v1/chat/completions", None, Some(&chat(&format!("{start_of_prompt}Goal: two, a little longer"), false))).json();
        assert_eq!(first["timings"]["cache_n"].as_u64(), Some(0));
        let cached = second["timings"]["cache_n"].as_u64().unwrap();
        assert!(cached >= tokens_of(start_of_prompt.len()) - 1 && second["timings"]["prompt_n"].as_u64().unwrap() < 12, "{second}");
        let mut no_cache = chat(&format!("{start_of_prompt}Goal: three"), false);
        no_cache["cache_prompt"] = json!(false);
        assert_eq!(ask(port, "POST", "/v1/chat/completions", None, Some(&no_cache)).json()["timings"]["cache_n"].as_u64(), Some(0), "cache_prompt false reads everything");
    }

    #[test]
    fn the_items_are_taken_in_order_and_a_script_that_is_used_up_says_so() {
        let (port, shared) = start(json!(["first", {"type": "json", "value": {"odd": true}}, {"type": "http_error", "status": 500, "body": {"error": {"message": "fell over"}}}, {"type": "http_error", "status": 400}]), None, 1, 6144);
        let ask_chat = || ask(port, "POST", "/v1/chat/completions", None, Some(&chat("p", false)));
        assert_eq!(ask_chat().json()["choices"][0]["message"]["content"], "first");
        let odd = ask_chat();
        assert_eq!((odd.status, odd.json()), (200, json!({"odd": true})));
        let broken = ask_chat();
        assert_eq!((broken.status, broken.json()["error"]["message"].clone()), (500, json!("fell over")));
        let default_body = ask_chat();
        assert_eq!((default_body.status, default_body.json()["error"]["type"].clone()), (400, json!("invalid_request_error")));
        let used_up = ask_chat();
        assert_eq!(used_up.status, 500);
        assert!(used_up.json()["error"]["message"].as_str().unwrap().contains("no scripted reply for request 4"), "{}", used_up.text());
        let outcomes: Vec<String> = lock(&shared.hits).iter().map(|h| h["outcome"].as_str().unwrap().to_string()).collect();
        assert_eq!(outcomes, ["answered", "answered", "http_error", "http_error", "script_used_up"]);
        // A script that repeats its last item never runs out.
        let (again, _) = start(json!({"repeat_last": true, "items": ["same"]}), None, 1, 6144);
        for _ in 0..3 {
            assert_eq!(ask(again, "POST", "/v1/chat/completions", None, Some(&chat("p", false))).json()["choices"][0]["message"]["content"], "same");
        }
    }

    #[test]
    fn a_prompt_past_the_window_gets_the_real_400_and_uses_no_item_when_the_script_asks_for_it() {
        let (port, shared) = start(json!({"enforce_context": true, "items": ["fits"]}), None, 2, 2048);
        let too_long = ask(port, "POST", "/v1/chat/completions", None, Some(&chat(&"word ".repeat(1000), false)));
        let e = too_long.json();
        assert_eq!((too_long.status, e["error"]["type"].as_str(), e["error"]["n_ctx"].as_u64()), (400, Some("exceed_context_size_error"), Some(1024)), "2048 divided between 2 slots");
        assert!(e["error"]["n_prompt_tokens"].as_u64().unwrap() > 1024);
        let fits = ask(port, "POST", "/v1/chat/completions", None, Some(&chat("short", false)));
        assert_eq!(fits.json()["choices"][0]["message"]["content"], "fits", "the item was not used by the refused request");
        assert_eq!(lock(&shared.hits)[0]["outcome"], "context_exceeded");
    }

    #[test]
    fn a_hung_request_holds_its_slot_until_the_client_hangs_up_and_then_lets_go() {
        let (port, shared) = start(json!([{"type": "hang"}, "after"]), Some("k"), 1, 6144);
        let mut conn = send(port, "POST", "/v1/chat/completions", Some("k"), Some(&chat("p", false)));
        let busy = || ask(port, "GET", "/slots", Some("k"), None).json()[0]["is_processing"] == json!(true);
        let started = Instant::now();
        while !busy() {
            assert!(started.elapsed() < Duration::from_secs(5), "the slot never became busy");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(ask(port, "GET", "/health", None, None).status, 200, "the server answers its health check while a request hangs");
        conn.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let mut one = [0u8; 1];
        assert!(conn.read(&mut one).is_err(), "no byte of an answer comes");
        let _ = conn.shutdown(Shutdown::Both);
        drop(conn);
        let started = Instant::now();
        while busy() {
            assert!(started.elapsed() < Duration::from_secs(5), "the slot was never let go after the hang-up");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(lock(&shared.hits)[0]["outcome"], "client_gone");
        assert_eq!(ask(port, "POST", "/v1/chat/completions", Some("k"), Some(&chat("p", false))).json()["choices"][0]["message"]["content"], "after");
    }

    #[test]
    fn a_stuck_server_keeps_its_slot_after_the_client_has_gone() {
        let (port, shared) = start(json!([{"type": "hang", "ignore_hangup": true, "max_ms": 1000}]), None, 1, 6144);
        let conn = send(port, "POST", "/v1/chat/completions", None, Some(&chat("p", false)));
        std::thread::sleep(Duration::from_millis(100));
        drop(conn);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(ask(port, "GET", "/slots", None, None).json()[0]["is_processing"], json!(true), "it did not notice, or did not care");
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(ask(port, "GET", "/slots", None, None).json()[0]["is_processing"], json!(false));
        assert_eq!(lock(&shared.hits)[0]["outcome"], "hang_expired");
    }

    #[test]
    fn a_reset_gives_the_client_an_error_and_not_an_answer() {
        let (port, shared) = start(json!([{"type": "reset"}]), None, 1, 6144);
        let mut conn = send(port, "POST", "/v1/chat/completions", None, Some(&chat("p", false)));
        let mut raw = Vec::new();
        let ended = conn.read_to_end(&mut raw);
        assert!(raw.is_empty(), "no byte of an answer: {:?}", String::from_utf8_lossy(&raw));
        #[cfg(windows)]
        assert!(ended.is_err(), "a reset is an error on the client's side, not the quiet end of a file: {ended:?}");
        #[cfg(not(windows))]
        let _ = ended;
        let started = Instant::now();
        while lock(&shared.hits)[0].get("outcome").is_none() && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(lock(&shared.hits)[0]["outcome"], "reset");
    }

    #[test]
    fn a_cut_stream_ends_without_a_finish_reason_and_without_done() {
        let (port, _) = start(json!([{"type": "cut", "after_chunks": 2}, {"type": "cut", "after_chunks": 1, "abrupt": true}]), None, 1, 6144);
        let cut = ask(port, "POST", "/v1/chat/completions", None, Some(&chat("p", true)));
        let events = cut.events();
        assert_eq!(events.len(), 3, "the role and two pieces: {events:?}");
        assert!(events.iter().all(|e| e != "[DONE]" && !e.contains("\"finish_reason\":\"")), "{events:?}");
        // An abrupt end: the pieces that were sent may or may not be read before the error.
        let mut conn = send(port, "POST", "/v1/chat/completions", None, Some(&chat("p", true)));
        let mut raw = Vec::new();
        let _ = conn.read_to_end(&mut raw);
        let text = String::from_utf8_lossy(&raw).into_owned();
        assert!(!text.contains("[DONE]") && !text.contains("\"finish_reason\":\"stop\""), "{text}");
    }

    #[test]
    fn a_body_that_is_not_streamed_can_be_cut_in_half() {
        let (port, _) = start(json!([{"type": "cut"}]), None, 1, 6144);
        let mut conn = send(port, "POST", "/v1/chat/completions", None, Some(&chat("p", false)));
        let mut raw = Vec::new();
        let _ = conn.read_to_end(&mut raw);
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
        let promised: usize = head.split("content-length: ").nth(1).and_then(|r| r.split_whitespace().next()).and_then(|n| n.parse().ok()).unwrap();
        let got = raw.len() - split - 4;
        assert!(got > 0 && got < promised, "{got} of {promised} bytes");
        assert!(serde_json::from_slice::<Value>(&raw[split + 4..]).is_err());
    }

    #[test]
    fn a_character_split_between_two_writes_arrives_whole_after_the_chunks_are_joined() {
        let text = "It is 24\u{b0}C in Z\u{fc}rich, \u{20b9}500, \u{1f600} done.";
        let (port, _) = start(json!([{"type": "text", "text": text, "chunk_chars": 9, "split_utf8": true}]), None, 1, 6144);
        let answer = ask(port, "POST", "/v1/chat/completions", None, Some(&chat("p", true)));
        let pieces: String = answer.events().iter().filter(|e| *e != "[DONE]").filter_map(|e| serde_json::from_str::<Value>(e).ok()).filter_map(|v| v["choices"][0]["delta"]["content"].as_str().map(str::to_string)).collect();
        assert_eq!(pieces, text);
    }

    #[test]
    fn one_slot_serves_one_request_at_a_time_and_a_queued_request_is_dropped_when_its_client_leaves() {
        let (port, shared) = start(json!({"repeat_last": true, "items": [{"type": "text", "chunks": ["a"], "prompt_delay_ms": 400}]}), None, 1, 6144);
        let first = send(port, "POST", "/v1/chat/completions", None, Some(&chat("one", false)));
        std::thread::sleep(Duration::from_millis(80));
        let second = send(port, "POST", "/v1/chat/completions", None, Some(&chat("two", false)));
        let (a, b) = (read_answer(first), read_answer(second));
        assert_eq!((a.status, b.status), (200, 200));
        let hits = lock(&shared.hits).clone();
        assert_eq!(hits.iter().map(|h| h["concurrent"].as_u64().unwrap()).collect::<Vec<_>>(), [1, 1], "never two at once");
        assert!(hits[1]["queued_ms"].as_u64().unwrap() >= 250, "the second waited for the first: {:?}", hits[1]);
        drop(hits);
        // A request that gives up while it waits for the slot is dropped, and does not take an item when the slot frees.
        let blocker = send(port, "POST", "/v1/chat/completions", None, Some(&chat("three", false)));
        std::thread::sleep(Duration::from_millis(80));
        let leaver = send(port, "POST", "/v1/chat/completions", None, Some(&chat("four", false)));
        std::thread::sleep(Duration::from_millis(80));
        drop(leaver);
        assert_eq!(read_answer(blocker).status, 200);
        let started = Instant::now();
        while lock(&shared.hits).get(3).and_then(|h| h.get("outcome")).is_none() && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(lock(&shared.hits)[3]["outcome"], "client_gone_while_queued");
    }

    #[test]
    fn the_hits_can_be_read_and_the_script_replaced_while_the_server_runs() {
        let (port, _) = start(json!(["one"]), Some("k"), 1, 6144);
        ask(port, "POST", "/v1/chat/completions", Some("k"), Some(&json!({"messages": [{"role": "user", "content": "a goal"}], "max_tokens": 77, "id_slot": 0, "grammar": "root ::= \"a\""})));
        let hits = ask(port, "GET", "/_hits", Some("k"), None).json();
        assert_eq!((hits[0]["n"].clone(), hits[0]["max_tokens"].clone(), hits[0]["constrained"].clone(), hits[0]["prompt_head"].clone(), hits[0]["outcome"].clone()), (json!(0), json!(77), json!(true), json!("a goal"), json!("answered")));
        let replaced = ask(port, "POST", "/_script", Some("k"), Some(&json!({"items": ["new one", "new two"]})));
        assert_eq!((replaced.status, replaced.json()["items"].clone()), (200, json!(2)));
        assert_eq!(ask(port, "POST", "/v1/chat/completions", Some("k"), Some(&chat("p", false))).json()["choices"][0]["message"]["content"], "new one", "the new script starts from its first item");
        let bad = ask(port, "POST", "/_script", Some("k"), Some(&json!([{"type": "nonsense"}])));
        assert_eq!(bad.status, 400);
    }

    #[test]
    fn a_decision_request_gets_the_probabilities_of_the_first_word() {
        let (port, _) = start(json!([{"type": "text", "chunks": ["A"], "top_logprobs": [["A", -0.1], ["B", -3.0]]}]), None, 1, 6144);
        let mut body = chat("p", false);
        body["logprobs"] = json!(true);
        let v = ask(port, "POST", "/v1/chat/completions", None, Some(&body)).json();
        assert_eq!(v["choices"][0]["logprobs"]["content"][0]["top_logprobs"][1]["token"], "B");
    }

    fn slot_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aqua_fake_llama_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn slot_call(port: u16, id: usize, action: &str, file: &str) -> Answer {
        ask(port, "POST", &format!("/slots/{id}?action={action}"), None, Some(&json!({"filename": file})))
    }

    #[test]
    fn a_build_without_the_slot_endpoint_answers_404_and_one_started_without_the_folder_answers_501() {
        let dir = slot_dir("no_endpoint");
        let (old_build, _) = start_with(json!(["x"]), None, 1, 6144, Some(&dir), false);
        assert_eq!(slot_call(old_build, 0, "save", "a.bin").status, 404, "the older builds have no such route");
        let (no_folder, _) = start_with(json!(["x"]), None, 1, 6144, None, true);
        let refused = slot_call(no_folder, 0, "save", "a.bin");
        assert_eq!((refused.status, refused.json()["error"]["type"].clone()), (501, json!("not_supported_error")));
        assert!(!dir.join("a.bin").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_saved_slot_is_written_to_the_folder_and_a_restore_puts_it_back_as_the_cache_of_the_next_prompt() {
        let dir = slot_dir("round_trip");
        let file = "kv_0000000000000001_0000000000000002.bin";
        let (port, _) = start_with(json!({"repeat_last": true, "items": ["ok"]}), None, 1, 6144, Some(&dir), true);
        let start_of_prompt = "The rules of the agent are these. ".repeat(10);
        ask(port, "POST", "/v1/chat/completions", None, Some(&chat(&format!("{start_of_prompt}Goal: one"), false)));
        let saved = slot_call(port, 0, "save", file);
        let v = saved.json();
        assert_eq!((saved.status, v["n_saved"].as_u64().unwrap() > 0, v["n_written"].as_u64().unwrap() > 0), (200, true, true), "{v}");
        let text = std::fs::read_to_string(dir.join(file)).unwrap();
        assert!(text.starts_with(SLOT_FILE_MAGIC) && text.contains("m.gguf"), "the file says which model wrote it");

        // Another server (a restart) with the same folder: the slot is empty until the file is put back.
        let (again, _) = start_with(json!({"repeat_last": true, "items": ["ok"]}), None, 1, 6144, Some(&dir), true);
        let cold = ask(again, "POST", "/v1/chat/completions", None, Some(&chat(&format!("{start_of_prompt}Goal: two"), false))).json();
        assert_eq!(cold["timings"]["cache_n"].as_u64(), Some(0));
        let (third, _) = start_with(json!({"repeat_last": true, "items": ["ok"]}), None, 1, 6144, Some(&dir), true);
        let restored = slot_call(third, 0, "restore", file);
        assert_eq!((restored.status, restored.json()["n_restored"].as_u64().unwrap() > 0), (200, true));
        let warm = ask(third, "POST", "/v1/chat/completions", None, Some(&chat(&format!("{start_of_prompt}Goal: two"), false))).json();
        assert!(warm["timings"]["cache_n"].as_u64().unwrap() >= tokens_of(start_of_prompt.len()) - 1, "{warm}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_restore_of_a_missing_corrupt_or_foreign_file_and_a_scripted_refusal_are_the_400_of_the_real_server_and_the_rest() {
        let dir = slot_dir("refusals");
        let (port, _) = start_with(json!(["x"]), None, 2, 6144, Some(&dir), true);
        let invalid = |a: &Answer| (a.status, a.json()["error"]["message"].as_str().unwrap_or("").contains("invalid slot save file"));
        assert_eq!(invalid(&slot_call(port, 0, "restore", "missing.bin")), (400, true));
        std::fs::write(dir.join("garbage.bin"), [0xff, 0xfe, 0x00, 0x80]).unwrap();
        assert_eq!(invalid(&slot_call(port, 0, "restore", "garbage.bin")), (400, true), "bytes that are not text");
        std::fs::write(dir.join("short.bin"), format!("{SLOT_FILE_MAGIC}{}m.gguf", '\n')).unwrap();
        assert_eq!(invalid(&slot_call(port, 0, "restore", "short.bin")), (400, true), "a file that stops after the name of the model");
        std::fs::write(dir.join("foreign.bin"), [SLOT_FILE_MAGIC, "other.gguf", "some prompt"].join("\n")).unwrap();
        assert_eq!(invalid(&slot_call(port, 0, "restore", "foreign.bin")), (400, true), "the state of another model file");
        // A name with a path, a slot the server does not have, and an action it does not know.
        assert_eq!(slot_call(port, 0, "restore", "../x.bin").status, 400);
        assert_eq!(slot_call(port, 7, "save", "a.bin").status, 400);
        assert_eq!(slot_call(port, 0, "erase", "a.bin").status, 400);
        let (refusing, _) = start_with(json!({"slot_save_status": 500, "slot_restore_status": 400, "items": ["x"]}), None, 1, 6144, Some(&dir), true);
        assert_eq!((slot_call(refusing, 0, "save", "a.bin").status, slot_call(refusing, 0, "restore", "a.bin").status), (500, 400));
        assert!(!dir.join("a.bin").exists(), "a refused save writes nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_slot_endpoint_needs_the_key_like_every_other_route() {
        let dir = slot_dir("key");
        let (port, _) = start_with(json!(["x"]), Some("secret"), 1, 6144, Some(&dir), true);
        let body = json!({"filename": "a.bin"});
        assert_eq!(ask(port, "POST", "/slots/0?action=save", None, Some(&body)).status, 401);
        assert_eq!(ask(port, "POST", "/slots/0?action=save", Some("other"), Some(&body)).status, 401);
        assert_eq!(ask(port, "POST", "/slots/0?action=save", Some("secret"), Some(&body)).status, 200);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_other_routes_exist_and_an_unknown_one_is_a_404() {
        let (port, _) = start(json!(["x"]), None, 3, 6144);
        assert_eq!(ask(port, "GET", "/slots", None, None).json().as_array().unwrap().len(), 3);
        assert_eq!(ask(port, "POST", "/tokenize", None, Some(&json!({"content": "twelve chars"}))).json()["tokens"].as_array().unwrap().len(), 3);
        assert_eq!(ask(port, "GET", "/v1/models", None, None).json()["data"][0]["id"], "m.gguf");
        let missing = ask(port, "GET", "/nothing", None, None);
        assert_eq!((missing.status, missing.json()["error"]["type"].clone()), (404, json!("not_found_error")));
        assert_eq!(ask(port, "POST", "/v1/chat/completions", None, None).status, 400, "no body is not JSON");
    }
}
