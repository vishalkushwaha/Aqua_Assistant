//! The HTTP calls to the model server: the plain POST that can be hung up on, the streamed POST that reads server-sent events, and the watch on a stream that went quiet (the last event, the verdict and the two limits) (move only: this was part of `llama_server.rs`).

use super::*;

/// When the last event of a streamed answer arrived: written by the thread that reads the stream, read by the one that waits for it. An event is any `data:` line of the stream, a
/// piece of the thinking as much as a piece of the answer, so a model that thinks for a while is not taken for one that stopped.
pub(super) struct Pulse {
    began: Instant,
    /// Milliseconds from `began` to the last event, plus one: 0 until the first event has come.
    last: std::sync::atomic::AtomicU64,
}

impl Pulse {
    pub(super) fn new() -> Self {
        Pulse { began: Instant::now(), last: std::sync::atomic::AtomicU64::new(0) }
    }

    pub(super) fn beat(&self) {
        self.last.store(self.began.elapsed().as_millis() as u64 + 1, std::sync::atomic::Ordering::SeqCst);
    }

    /// How long nothing has come since the last event; `None` before the first.
    pub(super) fn quiet_for(&self) -> Option<Duration> {
        match self.last.load(std::sync::atomic::Ordering::SeqCst) {
            0 => None,
            at => Some(self.began.elapsed().saturating_sub(Duration::from_millis(at - 1))),
        }
    }
}

/// Which limit a stream has run past.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Quiet {
    /// No event at all in this long (the server has not begun to answer).
    BeforeFirst(Duration),
    /// The answer began and then nothing came for this long.
    InTheMiddle(Duration),
}

/// Has a streamed call been quiet for too long? `idle` is armed by the first event and measured from the last one; `first_event` is measured from the start and ends when the first event
/// comes. The wait for the first event is not an idle time: the server is reading the prompt (8 seconds for a cold one, 40 for a long one on the processor), and a stall there is for
/// `first_event` or for the whole call's time limit to find.
pub(super) fn quiet_verdict(idle: Option<Duration>, first_event: Option<Duration>, since_start: Duration, quiet_for: Option<Duration>) -> Option<Quiet> {
    match quiet_for {
        None => first_event.filter(|limit| since_start >= *limit).map(|_| Quiet::BeforeFirst(since_start)),
        Some(quiet) => idle.filter(|limit| quiet >= *limit).map(|_| Quiet::InTheMiddle(quiet)),
    }
}

/// The limits on a streamed answer's silence, as (after the first event, before it): what the call was given, else `AQUA_STREAM_IDLE_SECS` and `AQUA_STREAM_FIRST_EVENT_SECS`. A limit is
/// off unless it is above zero. A call that is not streamed has no events and so no such limit: its server writes nothing until the whole answer is ready.
pub(super) fn stream_watch(extras: &Extras<'_>) -> (Option<Duration>, Option<Duration>) {
    let setting = |flag: &crate::flags::Flag| Some(crate::flags::int(flag)).filter(|secs| *secs > 0).map(|secs| Duration::from_secs(secs as u64));
    (extras.idle.or_else(|| setting(&crate::flags::STREAM_IDLE_SECS)), extras.first_event.or_else(|| setting(&crate::flags::STREAM_FIRST_EVENT_SECS)))
}

/// One JSON POST to the local server that can be given up on: the connection is read in short slices, and when `abort` is
/// set it is closed on the spot. (A plain HTTP call cannot be interrupted, and killing the server to stop one request
/// would cost a model reload.) Errors keep the `HTTP <code>: <body>` shape the callers already read.
pub(crate) fn post_json(port: u16, key: &str, path: &str, body: &Value, timeout: Duration, abort: &std::sync::atomic::AtomicBool) -> Result<Value, String> {
    use std::io::{Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpStream};
    use std::sync::atomic::Ordering;

    let payload = body.to_string();
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| format!("connection: {e}"))?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(request.as_bytes()).map_err(|e| format!("send: {e}"))?;

    let deadline = Instant::now() + timeout;
    let mut raw: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        if abort.load(Ordering::SeqCst) {
            let _ = stream.shutdown(Shutdown::Both);
            return Err("cancelled".to_string());
        }
        if Instant::now() >= deadline {
            let _ = stream.shutdown(Shutdown::Both);
            return Err("timed out waiting for the reply".to_string());
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) => return Err(format!("read: {e}")),
        }
    }

    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| "the reply had no header".to_string())?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let code: u16 = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| "the reply had no status".to_string())?;
    let body_bytes = &raw[split + 4..];
    let body_bytes = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { dechunk(body_bytes) } else { body_bytes.to_vec() };
    if code != 200 {
        return Err(format!("HTTP {code}: {}", String::from_utf8_lossy(&body_bytes).chars().take(300).collect::<String>()));
    }
    serde_json::from_slice::<Value>(&body_bytes).map_err(|e| e.to_string())
}

/// The body of an HTTP "chunked" reply, joined.
pub(super) fn dechunk(mut rest: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(line_end) = rest.windows(2).position(|w| w == b"\r\n") {
        let size = usize::from_str_radix(String::from_utf8_lossy(&rest[..line_end]).split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
        rest = &rest[line_end + 2..];
        if size == 0 || rest.len() < size {
            if size != 0 {
                out.extend_from_slice(rest);
            }
            break;
        }
        out.extend_from_slice(&rest[..size]);
        rest = rest.get(size + 2..).unwrap_or(&[]);
    }
    out
}

/// Open the connection and send a POST to the model server.
fn open_request(port: u16, key: &str, path: &str, body: &Value) -> Result<std::net::TcpStream, String> {
    use std::io::Write;
    let payload = body.to_string();
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| format!("connection: {e}"))?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(request.as_bytes()).map_err(|e| format!("send: {e}"))?;
    Ok(stream)
}

/// The same request as [`post_json`] with `"stream": true`: the answer comes as server-sent events, a few characters at a time.
/// Each piece of text is handed to `on_text` the moment it arrives. The whole reply is returned in the form a reply that was not
/// streamed has (the text, the finish reason, the timings), so nothing after this cares how it came.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn post_stream(port: u16, key: &str, path: &str, body: &Value, timeout: Duration, abort: &std::sync::atomic::AtomicBool, on_text: impl FnMut(&str)) -> Result<Value, String> {
    post_stream_watched(port, key, path, body, timeout, abort, &Pulse::new(), on_text)
}

/// [`post_stream`], and every event of the stream is noted on `pulse`, which the call that waits for the answer reads to find out whether the server has gone quiet.
#[allow(clippy::too_many_arguments)]
pub(super) fn post_stream_watched(port: u16, key: &str, path: &str, body: &Value, timeout: Duration, abort: &std::sync::atomic::AtomicBool, pulse: &Pulse, mut on_text: impl FnMut(&str)) -> Result<Value, String> {
    use std::io::Read;
    use std::net::Shutdown;
    use std::sync::atomic::Ordering;

    let mut body = body.clone();
    body["stream"] = json!(true);
    let mut stream = open_request(port, key, path, &body)?;
    let deadline = Instant::now() + timeout;
    let mut header: Vec<u8> = Vec::new();
    let mut after_header = false;
    let mut chunked = false;
    let mut code = 0u16;
    let (mut decoder, mut events) = (ChunkedDecoder::default(), EventSplitter::default());
    let (mut text, mut finish, mut timings) = (String::new(), Value::Null, Value::Null);
    let mut error_body: Vec<u8> = Vec::new();
    let mut done = false;
    let mut buf = [0u8; 16 * 1024];
    while !done {
        if abort.load(Ordering::SeqCst) {
            let _ = stream.shutdown(Shutdown::Both);
            return Err("cancelled".to_string());
        }
        if Instant::now() >= deadline {
            let _ = stream.shutdown(Shutdown::Both);
            return Err("timed out waiting for the reply".to_string());
        }
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) => return Err(format!("read: {e}")),
        };
        let mut fresh: Vec<u8> = buf[..n].to_vec();
        if !after_header {
            header.extend_from_slice(&fresh);
            let Some(split) = header.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
            let head = String::from_utf8_lossy(&header[..split]).to_string();
            code = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| "the reply had no status".to_string())?;
            chunked = head.to_ascii_lowercase().contains("transfer-encoding: chunked");
            fresh = header[split + 4..].to_vec();
            after_header = true;
        }
        let payload = if chunked { decoder.feed(&fresh) } else { fresh };
        if code != 200 {
            error_body.extend_from_slice(&payload);
            continue;
        }
        for data in events.feed(&payload) {
            // Every event is a sign of life, the role line and the thinking ones as much as a piece of the answer (the watchdog reads this).
            pulse.beat();
            if data.trim() == "[DONE]" {
                done = true;
                break;
            }
            let Ok(event) = serde_json::from_str::<Value>(&data) else { continue };
            if let Some(error) = event.get("error") {
                return Err(format!("the server reported an error: {}", error.to_string().chars().take(300).collect::<String>()));
            }
            if let Some(piece) = event.pointer("/choices/0/delta/content").and_then(|c| c.as_str()).filter(|c| !c.is_empty()) {
                text.push_str(piece);
                on_text(piece);
            }
            if let Some(reason) = event.pointer("/choices/0/finish_reason").filter(|r| !r.is_null()) {
                finish = reason.clone();
            }
            if let Some(t) = event.get("timings") {
                timings = t.clone();
            }
        }
    }
    if code != 200 {
        return Err(format!("HTTP {code}: {}", String::from_utf8_lossy(&error_body).chars().take(300).collect::<String>()));
    }
    if !after_header {
        return Err("the reply had no header".to_string());
    }
    if !done && finish.is_null() {
        // The connection ended with neither the closing `[DONE]` nor a finish reason: the server stopped in the middle of the answer (it
        // crashed, was killed, or ran out of memory). What came is the start of an answer, and must not be shown, spoken and remembered as
        // if it were the whole of one.
        return Err(format!("the model server stopped in the middle of the answer ({} characters had come)", text.chars().count()));
    }
    Ok(json!({ "choices": [{ "message": { "content": text }, "finish_reason": finish }], "timings": timings }))
}
