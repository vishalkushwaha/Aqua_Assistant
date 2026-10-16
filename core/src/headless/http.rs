//! A small HTTP/1.1 client for the core on this computer, and a reader of server-sent events (card OA10-05; task UX-05 of the OpenCode audit).
//!
//! It talks to `127.0.0.1` only, with `std::net` (no new crate): the address is checked before anything is sent, so a token read from the data folder can never be sent to another
//! computer. Every request is `Connection: close`; an answer is read to its end. A stream is read as it comes: [`Stream::next_frame`] gives one frame of the server-sent stream at a
//! time and says when nothing came within the read time, so that the caller can look at its own flags (the person pressed Ctrl+C) between two reads.
//!
//! The token is read from a file or the environment ([`read_token`]) and goes into one header; it is never printed and never put in an error message.

use serde_json::Value;
use std::collections::VecDeque;
use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

/// The port of the core when nothing says otherwise.
pub const DEFAULT_PORT: u16 = 8767;
/// The file of the API token in the data folder.
pub const API_TOKEN_FILE: &str = ".aqua_api_token";
/// The file of the UI token in the data folder.
pub const UI_TOKEN_FILE: &str = ".aqua_ui_token";

/// How long a connect may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a plain request may take to be answered (a start of a run answers at once; a wait answers within its own limit, 120 s at most).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(150);
/// The most bytes of an answer that are read (a page of events is far below it).
const MAX_ANSWER: usize = 16 * 1024 * 1024;

/// The address of the core: `http://127.0.0.1:PORT` or `http://localhost:PORT`, and nothing else. With no `base`, the port is `AQUA_API_PORT` (the second core of a live check) or 8767.
pub fn core_address(base: Option<&str>) -> Result<SocketAddr, String> {
    let Some(base) = base else {
        let port = std::env::var("AQUA_API_PORT").ok().and_then(|p| p.trim().parse::<u16>().ok()).unwrap_or(DEFAULT_PORT);
        return Ok(SocketAddr::from(([127, 0, 0, 1], port)));
    };
    let rest = base.trim().trim_end_matches('/').strip_prefix("http://").ok_or("the core's address must be like http://127.0.0.1:8767 (plain HTTP on this computer)")?;
    let (host, port) = rest.split_once(':').ok_or("the core's address must name a port, like http://127.0.0.1:8767")?;
    if host != "127.0.0.1" && host != "localhost" {
        return Err("the core's address must be on this computer (127.0.0.1): the token is never sent anywhere else".into());
    }
    let port: u16 = port.parse().map_err(|_| "the port of the core's address is not a number")?;
    Ok(SocketAddr::from(([127, 0, 0, 1], port)))
}

/// Which token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    /// The token of scripts and tools: it cannot approve anything.
    Api,
    /// The app's own token. Only `preapprove` reads it, and only for a person at a terminal.
    Ui,
}

/// A token is one line of visible characters: anything else would not be a token, and could break the request it is put in.
fn clean_token(raw: &str) -> Option<String> {
    let t = raw.trim_start_matches('\u{feff}').trim();
    (!t.is_empty() && t.len() <= 512 && t.bytes().all(|b| b.is_ascii_graphic())).then(|| t.to_string())
}

/// The token, never printed: for the API token `token_file` if it was given, else `AQUA_API_TOKEN`, else `<data>/.aqua_api_token`; for the UI token only `<data>/.aqua_ui_token`
/// (it is not taken from the environment or a file the caller names: it is the app's).
pub fn read_token(kind: TokenKind, token_file: Option<&Path>, data_dir: &Path) -> Result<String, String> {
    if kind == TokenKind::Api {
        if token_file.is_none() {
            if let Some(t) = std::env::var("AQUA_API_TOKEN").ok().and_then(|v| clean_token(&v)) {
                return Ok(t);
            }
        }
    }
    let path = match (kind, token_file) {
        (TokenKind::Api, Some(f)) => f.to_path_buf(),
        (TokenKind::Api, None) => data_dir.join(API_TOKEN_FILE),
        (TokenKind::Ui, _) => data_dir.join(UI_TOKEN_FILE),
    };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("the token could not be read from {} ({}): is this the data folder of Aqua? (--data DIR)", path.display(), e.kind()))?;
    clean_token(&text).ok_or_else(|| format!("{} does not hold a token", path.display()))
}

/// Is `id` fit to be a part of a path (a run id, a pre-approval id): letters, digits and `_ - .`, not empty, not long? Nothing else is put in a request line.
pub fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && !id.starts_with('.') && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// Why a request did not get an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetError {
    /// Nothing answered on the address: Aqua is not running there.
    Unreachable(String),
    /// The connection broke, or what came back was not an HTTP answer.
    Broken(String),
}

impl NetError {
    pub fn message(&self) -> &str {
        match self {
            NetError::Unreachable(m) | NetError::Broken(m) => m,
        }
    }
}

/// An answer: the status and the body.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Reply {
    /// The body as JSON; a body that is not JSON is `{ "error": "<its first 300 characters>" }`.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| serde_json::json!({ "error": String::from_utf8_lossy(&self.body).chars().take(300).collect::<String>() }))
    }

    /// The `error` sentence of an error body, and its `code`.
    pub fn error(&self) -> (String, String) {
        let v = self.json();
        (v["error"].as_str().unwrap_or("no reason given").to_string(), v["code"].as_str().unwrap_or("").to_string())
    }
}

/// The core on this computer, with a token.
#[derive(Clone)]
pub struct Client {
    addr: SocketAddr,
    token: String,
}

impl Client {
    pub fn new(addr: SocketAddr, token: String) -> Self {
        Self { addr, token }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    fn connect(&self) -> Result<TcpStream, NetError> {
        let stream = TcpStream::connect_timeout(&self.addr, CONNECT_TIMEOUT).map_err(|e| NetError::Unreachable(format!("nothing answered on {} ({}): is Aqua running? (`aqua-cli doctor --live` says)", self.addr, e.kind())))?;
        let _ = stream.set_nodelay(true);
        Ok(stream)
    }

    fn request_head(&self, method: &str, path: &str, body: Option<&str>, accept: &str) -> String {
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nAccept: {accept}\r\nConnection: close\r\nUser-Agent: aqua-cli\r\n", self.addr, self.token);
        match body {
            Some(b) => head.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}", b.len())),
            None => {
                if matches!(method, "POST" | "PUT" | "PATCH") {
                    head.push_str("Content-Length: 0\r\n");
                }
                head.push_str("\r\n");
            }
        }
        head
    }

    /// One request, answered to its end. `path` is the path and query of the request line; it must not hold a space or a line break.
    pub fn send(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Reply, NetError> {
        if path.bytes().any(|b| b <= b' ' || b == 0x7f) || !path.starts_with('/') {
            return Err(NetError::Broken("the path of a request may not hold a space or a control character".into()));
        }
        let mut stream = self.connect()?;
        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
        let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
        let payload = body.map(|b| b.to_string());
        stream.write_all(self.request_head(method, path, payload.as_deref(), "application/json").as_bytes()).map_err(|e| NetError::Broken(format!("the request could not be sent ({})", e.kind())))?;
        let mut raw = Vec::new();
        let mut limited = (&mut stream).take(MAX_ANSWER as u64);
        limited.read_to_end(&mut raw).map_err(|e| NetError::Broken(format!("no answer ({})", e.kind())))?;
        let (head, rest) = split_head(&raw).ok_or_else(|| NetError::Broken("what came back was not an HTTP answer".into()))?;
        let status = parse_status(&head).ok_or_else(|| NetError::Broken("what came back was not an HTTP answer".into()))?;
        let body = if is_chunked(&head) { unchunk(rest).ok_or_else(|| NetError::Broken("the answer of the core was cut short".into()))? } else { rest.to_vec() };
        Ok(Reply { status, body })
    }

    /// Opens `GET path` as a stream of server-sent events. A status other than 200 is `Ok(Err(reply))`: the caller says what it was.
    pub fn open_stream(&self, path: &str, read_timeout: Duration) -> Result<Result<Stream, Reply>, NetError> {
        if path.bytes().any(|b| b <= b' ' || b == 0x7f) || !path.starts_with('/') {
            return Err(NetError::Broken("the path of a request may not hold a space or a control character".into()));
        }
        let mut stream = self.connect()?;
        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
        stream.write_all(self.request_head("GET", path, None, "text/event-stream").as_bytes()).map_err(|e| NetError::Broken(format!("the request could not be sent ({})", e.kind())))?;
        // The head is read with the connect-time read limit; after it, the caller's read time applies.
        let _ = stream.set_read_timeout(Some(CONNECT_TIMEOUT.max(read_timeout)));
        let mut reader = BufReader::new(stream);
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match reader.read(&mut byte) {
                Ok(0) => return Err(NetError::Broken("the core closed the connection before it answered".into())),
                Ok(_) => head.push(byte[0]),
                Err(e) => return Err(NetError::Broken(format!("no answer ({})", e.kind()))),
            }
            if head.len() > 64 * 1024 {
                return Err(NetError::Broken("the head of the answer is too long".into()));
            }
        }
        let head_text = String::from_utf8_lossy(&head).to_string();
        let status = parse_status(&head_text).ok_or_else(|| NetError::Broken("what came back was not an HTTP answer".into()))?;
        let chunked = is_chunked(&head_text);
        if status != 200 {
            let mut rest = Vec::new();
            let mut limited = (&mut reader).take(1024 * 1024);
            let _ = limited.read_to_end(&mut rest);
            let body = if chunked { unchunk(&rest).unwrap_or_default() } else { rest };
            return Ok(Err(Reply { status, body }));
        }
        let _ = reader.get_ref().set_read_timeout(Some(read_timeout));
        Ok(Ok(Stream { body: Dechunk::new(reader, chunked), parser: SseParser::default(), ready: VecDeque::new(), closed: false }))
    }
}

fn split_head(raw: &[u8]) -> Option<(String, &[u8])> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    Some((String::from_utf8_lossy(&raw[..split]).to_string(), &raw[split + 4..]))
}

fn parse_status(head: &str) -> Option<u16> {
    head.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse::<u16>().ok())
}

fn is_chunked(head: &str) -> bool {
    head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    })
}

/// The body of a chunked answer, put together. `None` when the chunks are not well formed.
pub fn unchunk(mut rest: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = rest.windows(2).position(|w| w == b"\r\n")?;
        let size = usize::from_str_radix(std::str::from_utf8(&rest[..line_end]).ok()?.split(';').next()?.trim(), 16).ok()?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        if rest.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

/// What a read that came to nothing within its time is called on this system.
fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChunkState {
    /// Reading the hexadecimal size line; what has been read of it so far.
    Size(Vec<u8>),
    /// This many bytes of the chunk are left.
    Data(usize),
    /// The line break after a chunk: this many of its two bytes are left.
    After(usize),
    Done,
}

/// The body of an answer as it arrives: chunked, or not (read to the end). It can be read again after a read that timed out: what was read of a size line stays.
struct Dechunk {
    inner: BufReader<TcpStream>,
    chunked: bool,
    state: ChunkState,
}

impl Dechunk {
    fn new(inner: BufReader<TcpStream>, chunked: bool) -> Self {
        Self { inner, chunked, state: ChunkState::Size(Vec::new()) }
    }
}

impl Read for Dechunk {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !self.chunked {
            return self.inner.read(buf);
        }
        loop {
            match &mut self.state {
                ChunkState::Done => return Ok(0),
                ChunkState::Size(line) => {
                    let mut byte = [0u8; 1];
                    if self.inner.read(&mut byte)? == 0 {
                        return Ok(0);
                    }
                    if byte[0] == b'\n' {
                        let text = String::from_utf8_lossy(line).to_string();
                        let size = usize::from_str_radix(text.split(';').next().unwrap_or("").trim(), 16).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "a chunk size that is not a number"))?;
                        self.state = if size == 0 { ChunkState::Done } else { ChunkState::Data(size) };
                    } else if line.len() > 32 {
                        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "a chunk size line that is too long"));
                    } else if byte[0] != b'\r' {
                        line.push(byte[0]);
                    }
                }
                ChunkState::Data(left) => {
                    let want = buf.len().min(*left);
                    let n = self.inner.read(&mut buf[..want])?;
                    if n == 0 {
                        return Ok(0);
                    }
                    *left -= n;
                    if *left == 0 {
                        self.state = ChunkState::After(2);
                    }
                    return Ok(n);
                }
                ChunkState::After(n) => {
                    let mut byte = [0u8; 1];
                    if self.inner.read(&mut byte)? == 0 {
                        return Ok(0);
                    }
                    *n -= 1;
                    if *n == 0 {
                        self.state = ChunkState::Size(Vec::new());
                    }
                }
            }
        }
    }
}

/// One frame of a server-sent stream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Frame {
    /// The `event:` (empty for a frame that has none).
    pub event: String,
    /// The `id:` as sent.
    pub id: Option<String>,
    /// The `data:` lines joined with a line break.
    pub data: String,
}

/// Cuts a byte stream into frames (the rules of `text/event-stream`: lines end in `\n`, `\r\n` or `\r`; a blank line ends a frame; a line that starts with `:` is a comment).
#[derive(Default)]
pub struct SseParser {
    line: Vec<u8>,
    event: String,
    id: Option<String>,
    data: Vec<String>,
    saw_field: bool,
    skip_lf: bool,
}

impl SseParser {
    /// Takes the next bytes; gives the frames they completed.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Frame> {
        let mut out = Vec::new();
        for &b in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if b == b'\n' {
                    continue;
                }
            }
            match b {
                b'\r' => {
                    self.skip_lf = true;
                    self.end_line(&mut out);
                }
                b'\n' => self.end_line(&mut out),
                _ => {
                    // A line longer than a frame may be is cut: nothing here lets one frame use all the memory.
                    if self.line.len() < 1024 * 1024 {
                        self.line.push(b);
                    }
                }
            }
        }
        out
    }

    fn end_line(&mut self, out: &mut Vec<Frame>) {
        let line = String::from_utf8_lossy(&std::mem::take(&mut self.line)).to_string();
        if line.is_empty() {
            if self.saw_field {
                out.push(Frame { event: std::mem::take(&mut self.event), id: self.id.take(), data: std::mem::take(&mut self.data).join("\n") });
                self.saw_field = false;
            }
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line.as_str(), ""),
        };
        match field {
            "event" => {
                self.event = value.to_string();
                self.saw_field = true;
            }
            "id" => {
                if !value.contains('\0') {
                    self.id = Some(value.to_string());
                }
                self.saw_field = true;
            }
            "data" => {
                self.data.push(value.to_string());
                self.saw_field = true;
            }
            _ => {}
        }
    }
}

/// Why a stream gave no frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamStop {
    /// Nothing came within the read time; read again.
    Quiet,
    /// The core closed the stream.
    Closed,
    /// The connection broke.
    Broken(String),
}

/// An open stream of server-sent events.
pub struct Stream {
    body: Dechunk,
    parser: SseParser,
    ready: VecDeque<Frame>,
    closed: bool,
}

impl Stream {
    /// The next frame. `Err(Quiet)` means that nothing came in the read time (the stream is still open: call again); `Err(Closed)` that the core ended it.
    pub fn next_frame(&mut self) -> Result<Frame, StreamStop> {
        loop {
            if let Some(f) = self.ready.pop_front() {
                return Ok(f);
            }
            if self.closed {
                return Err(StreamStop::Closed);
            }
            let mut buf = [0u8; 4096];
            match self.body.read(&mut buf) {
                Ok(0) => {
                    self.closed = true;
                }
                Ok(n) => self.ready.extend(self.parser.feed(&buf[..n])),
                Err(e) if is_timeout(&e) => return Err(StreamStop::Quiet),
                Err(e) => {
                    self.closed = true;
                    return Err(StreamStop::Broken(format!("the stream broke ({})", e.kind())));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_address_of_the_core_is_this_computer_or_nothing() {
        assert_eq!(core_address(Some("http://127.0.0.1:8790")).unwrap().port(), 8790);
        assert_eq!(core_address(Some("http://localhost:8790/")).unwrap().port(), 8790);
        for bad in ["https://127.0.0.1:8767", "http://example.com:8767", "http://127.0.0.1", "http://127.0.0.1:x", "http://192.168.1.4:8767", "127.0.0.1:8767", "http://127.0.0.1.evil.test:80"] {
            assert!(core_address(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_token_is_one_line_of_visible_characters() {
        assert_eq!(clean_token("\u{feff}abc123\r\n").as_deref(), Some("abc123"));
        assert_eq!(clean_token("a b"), None);
        assert_eq!(clean_token("abc\nGET / HTTP/1.1"), None);
        assert_eq!(clean_token("   "), None);
        assert_eq!(clean_token(&"x".repeat(600)), None);
    }

    #[test]
    fn an_id_that_goes_into_a_path_has_no_slash_no_space_and_no_dots_at_the_start() {
        assert!(safe_id("run_123_abc_4"));
        assert!(safe_id("pa_0f3c"));
        for bad in ["", "../x", "a/b", "a b", "a?b", "a\r\nb", ".hidden", &"x".repeat(200)] {
            assert!(!safe_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_frame_split_across_reads_comes_out_whole_and_comments_are_skipped() {
        let mut p = SseParser::default();
        let text = ": hello\nevent: tool.finished\nid: 7\ndata: {\"a\":1}\n\nevent: end\ndata: {\"last\":7}\n\n";
        let mut frames = Vec::new();
        for chunk in text.as_bytes().chunks(3) {
            frames.extend(p.feed(chunk));
        }
        assert_eq!(frames.len(), 2);
        assert_eq!((frames[0].event.as_str(), frames[0].id.as_deref(), frames[0].data.as_str()), ("tool.finished", Some("7"), "{\"a\":1}"));
        assert_eq!((frames[1].event.as_str(), frames[1].id.as_deref()), ("end", None));
    }

    #[test]
    fn crlf_and_several_data_lines_are_read_as_the_rules_say() {
        let mut p = SseParser::default();
        let frames = p.feed(b"event: x\r\ndata: one\r\ndata: two\r\n\r\ndata: three\r\r");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data, "one\ntwo");
        assert_eq!(frames[1].data, "three");
    }

    #[test]
    fn a_blank_line_with_nothing_before_it_makes_no_frame() {
        let mut p = SseParser::default();
        assert!(p.feed(b"\n\n: only a comment\n\n").is_empty());
    }

    #[test]
    fn a_chunked_body_is_put_together_and_a_cut_one_is_refused() {
        assert_eq!(unchunk(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n").unwrap(), b"hello world");
        assert!(unchunk(b"5\r\nhel").is_none());
        assert!(unchunk(b"zz\r\nhello\r\n").is_none());
    }
}
