//! `FakeModelServer`: a model server on a loopback port that says what a test tells it to (card OA9-04).
//!
//! The provider for an Ollama or an LM Studio (`model::openai_compat`) is tested against real sockets: the bytes of its request are read and kept, and the handler of the test writes
//! the bytes of the answer, in whatever pieces, as slowly as it wants, or not at all. Nothing but this machine's loopback address is bound, on a port the system picks.
//!
//! A handler is called on its own thread for each connection with what was asked ([`SeenRequest`]), the stream to write to, and the flag that is set when the server is dropped: a
//! handler that waits (a model that never answers) must look at it so that the test can end.

use serde_json::Value;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What a client asked.
#[derive(Clone, Debug)]
pub struct SeenRequest {
    pub method: String,
    pub path: String,
    /// Names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl SeenRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// The body as JSON (`Null` when it is not).
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    /// The whole request as text, head and body, for a test that looks for a word in it.
    pub fn text(&self) -> String {
        let head: Vec<String> = self.headers.iter().map(|(n, v)| format!("{n}: {v}")).collect();
        format!("{} {}\n{}\n\n{}", self.method, self.path, head.join("\n"), String::from_utf8_lossy(&self.body))
    }
}

type Handler = dyn Fn(&SeenRequest, &mut TcpStream, &AtomicBool) + Send + Sync;

pub struct FakeModelServer {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    connections: Arc<AtomicUsize>,
}

fn read_request(stream: &mut TcpStream) -> Option<SeenRequest> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut raw: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    let head_end = loop {
        if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let (method, path) = (first.next()?.to_string(), first.next()?.to_string());
    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    let length = headers.iter().find(|(n, _)| n == "content-length").and_then(|(_, v)| v.parse::<usize>().ok()).unwrap_or(0);
    let mut body: Vec<u8> = raw[head_end + 4..].to_vec();
    while body.len() < length {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => body.extend_from_slice(&buf[..n]),
        }
    }
    Some(SeenRequest { method, path, headers, body })
}

impl FakeModelServer {
    pub fn start(handler: impl Fn(&SeenRequest, &mut TcpStream, &AtomicBool) + Send + Sync + 'static) -> FakeModelServer {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a loopback port");
        listener.set_nonblocking(true).expect("a listener that does not block");
        let port = listener.local_addr().expect("an address").port();
        let stop = Arc::new(AtomicBool::new(false));
        let seen: Arc<Mutex<Vec<SeenRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let handler: Arc<Handler> = Arc::new(handler);
        let (stop_t, seen_t, connections_t) = (stop.clone(), seen.clone(), connections.clone());
        let thread = std::thread::spawn(move || {
            while !stop_t.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        connections_t.fetch_add(1, Ordering::SeqCst);
                        let (handler, stop, seen) = (handler.clone(), stop_t.clone(), seen_t.clone());
                        std::thread::spawn(move || {
                            if let Some(request) = read_request(&mut stream) {
                                seen.lock().unwrap_or_else(|e| e.into_inner()).push(request.clone());
                                handler(&request, &mut stream, &stop);
                            }
                            let _ = stream.flush();
                            let _ = stream.shutdown(Shutdown::Both);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(2)),
                    Err(_) => break,
                }
            }
        });
        FakeModelServer { port, stop, thread: Some(thread), seen, connections }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `http://127.0.0.1:<port>/v1`
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// What it was asked, in order.
    pub fn requests(&self) -> Vec<SeenRequest> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// How many connections were made to it (a request that was refused before it was sent makes none).
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

impl Drop for FakeModelServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A whole answer with a length: `status`, extra headers, the body.
pub fn respond(stream: &mut TcpStream, status: u16, headers: &[(&str, &str)], body: &str) {
    let mut head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (n, v) in headers {
        head.push_str(&format!("{n}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}

/// A JSON answer.
pub fn respond_json(stream: &mut TcpStream, status: u16, body: &Value) {
    respond(stream, status, &[("Content-Type", "application/json")], &body.to_string());
}

/// The head of a streamed answer: chunked, `text/event-stream`.
pub fn begin_stream(stream: &mut TcpStream) {
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
    let _ = stream.flush();
}

/// One chunk of a chunked body. `false` when the client has gone.
pub fn chunk(stream: &mut TcpStream, bytes: &[u8]) -> bool {
    let ok = write!(stream, "{:x}\r\n", bytes.len()).is_ok() && stream.write_all(bytes).is_ok() && stream.write_all(b"\r\n").is_ok();
    let _ = stream.flush();
    ok
}

/// The end of a chunked body.
pub fn end_chunks(stream: &mut TcpStream) {
    let _ = stream.write_all(b"0\r\n\r\n");
    let _ = stream.flush();
}

/// An event of a stream: `data: <json>` and a blank line.
pub fn event(stream: &mut TcpStream, data: &Value) -> bool {
    chunk(stream, format!("data: {data}\n\n").as_bytes())
}

/// The words of a stream chunk of the OpenAI shape: a delta with `content`.
pub fn delta(text: &str) -> Value {
    serde_json::json!({ "choices": [{ "index": 0, "delta": { "content": text }, "finish_reason": null }] })
}

/// A whole `chat.completion` with this text.
pub fn completion(text: &str) -> Value {
    serde_json::json!({ "choices": [{ "index": 0, "message": { "role": "assistant", "content": text }, "finish_reason": "stop" }], "usage": { "prompt_tokens": 12, "completion_tokens": 3 } })
}
