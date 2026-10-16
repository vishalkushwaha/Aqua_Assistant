//! Redis behind `db.command`. A small RESP2 client of our own (no extra crate), and a command
//! policy that is an **allow-list**: a command that is not listed here is refused.
//!
//! * Reads (`GET`, `HGETALL`, `SCAN`, `LRANGE`, `INFO` …) run on profiles of any mode.
//! * Writes (`SET`, `DEL`, `EXPIRE`, `HSET`, `LPUSH`, `INCR` …) need a `rw` profile and ask; `DEL` and
//!   `UNLINK` count as destructive, and on a `prod` profile every write is.
//! * Never available: `FLUSHALL`, `FLUSHDB`, `CONFIG`, `EVAL`/`SCRIPT`/`FUNCTION` (they run code),
//!   `KEYS` (unbounded; use `SCAN`), `DEBUG`, `SHUTDOWN`, `ACL`, `CLIENT`, `MODULE`, `MIGRATE`,
//!   `REPLICAOF`, `SORT` (can write), `MONITOR`, pub/sub and transactions.
//! * The password comes from Credential Manager by name; a connection that leaves the machine uses TLS.
//! * Values of hash fields named like credentials are masked in results.

use aqua_tool_host::output::is_secret_key;
use aqua_tool_host::{ErrorCode, ToolError};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Read,
    Write,
    /// A write that deletes.
    Delete,
}

const READ: &[&str] = &[
    "GET", "MGET", "EXISTS", "TYPE", "TTL", "PTTL", "STRLEN", "GETRANGE", "HGET", "HMGET", "HGETALL", "HKEYS", "HVALS", "HLEN", "HEXISTS", "LRANGE", "LLEN", "LINDEX", "SMEMBERS", "SCARD", "SISMEMBER",
    "SRANDMEMBER", "ZRANGE", "ZRANGEBYSCORE", "ZCARD", "ZSCORE", "ZRANK", "ZCOUNT", "SCAN", "SSCAN", "HSCAN", "ZSCAN", "DBSIZE", "PING", "ECHO", "TIME", "INFO", "GETBIT", "BITCOUNT", "XLEN", "XRANGE",
    "PFCOUNT",
];
const WRITE: &[&str] = &[
    "SET", "SETEX", "SETNX", "MSET", "EXPIRE", "PEXPIRE", "PERSIST", "INCR", "DECR", "INCRBY", "DECRBY", "APPEND", "HSET", "HMSET", "HSETNX", "HINCRBY", "LPUSH", "RPUSH", "LPOP", "RPOP", "SADD", "ZADD",
];
const DELETE: &[&str] = &["DEL", "UNLINK", "HDEL", "SREM", "ZREM", "LREM"];
/// Named so the refusal can say why; anything else unknown gets the generic message.
const NEVER: &[&str] = &[
    "FLUSHALL", "FLUSHDB", "CONFIG", "EVAL", "EVALSHA", "SCRIPT", "FUNCTION", "FCALL", "KEYS", "DEBUG", "SHUTDOWN", "ACL", "CLIENT", "MODULE", "MIGRATE", "REPLICAOF", "SLAVEOF", "SORT", "MONITOR",
    "SUBSCRIBE", "PSUBSCRIBE", "PUBLISH", "MULTI", "EXEC", "WATCH", "SAVE", "BGSAVE", "BGREWRITEAOF", "SYNC", "PSYNC", "RESTORE", "DUMP", "COPY", "MOVE", "SWAPDB", "SELECT", "AUTH", "HELLO", "RENAME",
    "RENAMENX", "OBJECT", "SLOWLOG", "LATENCY", "MEMORY", "CLUSTER", "FAILOVER", "WAIT", "LOLWUT",
];

pub fn classify(cmd: &str) -> Result<Kind, ToolError> {
    let c = cmd.to_ascii_uppercase();
    if READ.contains(&c.as_str()) {
        Ok(Kind::Read)
    } else if WRITE.contains(&c.as_str()) {
        Ok(Kind::Write)
    } else if DELETE.contains(&c.as_str()) {
        Ok(Kind::Delete)
    } else if NEVER.contains(&c.as_str()) {
        Err(ToolError::new(ErrorCode::QueryRejected, format!("{c} is never available through these tools (it can wipe data, run code, change the server or read unbounded amounts)")))
    } else {
        Err(ToolError::new(ErrorCode::QueryRejected, format!("{c} is not on the list of Redis commands these tools allow")))
    }
}

/// Check a whole command: the name, its argument count and shape.
pub fn check(args: &[String]) -> Result<Kind, ToolError> {
    let bad = |m: String| ToolError::new(ErrorCode::InvalidArguments, m);
    let name = args.first().ok_or_else(|| bad("the command is empty".into()))?;
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(bad("the first item must be a command name such as GET".into()));
    }
    let kind = classify(name)?;
    if args.len() > 64 {
        return Err(bad("too many arguments (64 at most)".into()));
    }
    if args.iter().any(|a| a.len() > 65_536 || a.contains('\0')) {
        return Err(bad("an argument is too long or contains a NUL character".into()));
    }
    let up = name.to_ascii_uppercase();
    match up.as_str() {
        "DEL" | "UNLINK" if args.len() > 11 => return Err(bad("delete at most 10 keys per call".into())),
        "DEL" | "UNLINK" | "GET" | "TYPE" | "TTL" | "PTTL" | "STRLEN" | "HGETALL" | "HKEYS" | "HVALS" | "HLEN" | "LLEN" | "SMEMBERS" | "SCARD" | "ZCARD" | "INCR" | "DECR" | "PERSIST" if args.len() < 2 => {
            return Err(bad(format!("{up} needs a key")));
        }
        "MGET" | "MSET" if args.len() > 101 => return Err(bad("at most 100 keys per call".into())),
        "SCAN" | "SSCAN" | "HSCAN" | "ZSCAN" => {
            // Bounded: a COUNT above 1000 would make one call scan the whole keyspace.
            if let Some(i) = args.iter().position(|a| a.eq_ignore_ascii_case("COUNT")) {
                if args.get(i + 1).and_then(|n| n.parse::<u64>().ok()).is_none_or(|n| n > 1000) {
                    return Err(bad("COUNT must be a number up to 1000".into()));
                }
            }
        }
        "LRANGE" | "ZRANGE" | "ZRANGEBYSCORE" | "XRANGE" | "GETRANGE" if args.len() < 4 => return Err(bad(format!("{up} needs a key and a range"))),
        "INFO" => {
            if let Some(s) = args.get(1) {
                let s = s.to_ascii_lowercase();
                if !["server", "clients", "memory", "stats", "keyspace", "cpu", "persistence", "default"].contains(&s.as_str()) {
                    return Err(bad("INFO takes one of: server, clients, memory, stats, keyspace, cpu, persistence".into()));
                }
            }
        }
        _ => {}
    }
    Ok(kind)
}

// ------------------------------------------------------------------ RESP

pub fn encode(args: &[String]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Simple(String),
    Error(String),
    Int(i64),
    Bulk(Option<Vec<u8>>),
    Array(Option<Vec<Reply>>),
}

const MAX_BULK: usize = 8 * 1024 * 1024;
const MAX_ARRAY: usize = 100_000;

pub fn read_reply<R: BufRead>(r: &mut R, depth: usize) -> Result<Reply, ToolError> {
    let io = |e: std::io::Error| ToolError::new(if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::WouldBlock { ErrorCode::Timeout } else { ErrorCode::Unavailable }, format!("Redis connection: {e}"));
    let proto = |m: &str| ToolError::new(ErrorCode::BackendError, format!("unexpected Redis reply: {m}"));
    if depth > 8 {
        return Err(proto("nesting too deep"));
    }
    let mut line = Vec::new();
    r.read_until(b'\n', &mut line).map_err(io)?;
    if line.len() < 3 || !line.ends_with(b"\r\n") {
        return Err(ToolError::new(ErrorCode::Unavailable, "Redis closed the connection"));
    }
    let body = String::from_utf8_lossy(&line[1..line.len() - 2]).to_string();
    match line[0] {
        b'+' => Ok(Reply::Simple(body)),
        b'-' => Ok(Reply::Error(body)),
        b':' => Ok(Reply::Int(body.parse().map_err(|_| proto("bad integer"))?)),
        b'$' => {
            let n: i64 = body.parse().map_err(|_| proto("bad length"))?;
            if n < 0 {
                return Ok(Reply::Bulk(None));
            }
            let n = n as usize;
            if n > MAX_BULK {
                return Err(ToolError::new(ErrorCode::ResourceLimit, format!("a Redis value is larger than {} MB", MAX_BULK / 1024 / 1024)));
            }
            let mut buf = vec![0u8; n + 2];
            r.read_exact(&mut buf).map_err(io)?;
            buf.truncate(n);
            Ok(Reply::Bulk(Some(buf)))
        }
        b'*' => {
            let n: i64 = body.parse().map_err(|_| proto("bad length"))?;
            if n < 0 {
                return Ok(Reply::Array(None));
            }
            if n as usize > MAX_ARRAY {
                return Err(ToolError::new(ErrorCode::ResourceLimit, "a Redis reply has too many items; use SCAN with a COUNT"));
            }
            let mut items = Vec::with_capacity((n as usize).min(1024));
            for _ in 0..n {
                items.push(read_reply(r, depth + 1)?);
            }
            Ok(Reply::Array(Some(items)))
        }
        other => Err(proto(&format!("unknown type byte {other}"))),
    }
}

// ------------------------------------------------------------- connection

pub struct Target {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: u32,
    pub tls: bool,
}

trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

pub struct Conn {
    io: BufReader<Box<dyn Stream>>,
}

/// Connection message with the password removed.
fn scrub(m: &str, t: &Target) -> String {
    match &t.password {
        Some(p) if p.len() >= 3 => m.replace(p.as_str(), "[REDACTED]"),
        _ => m.to_string(),
    }
}

impl Conn {
    pub fn connect(t: &Target, timeout: Duration) -> Result<Self, ToolError> {
        let unavailable = |m: String| ToolError::new(ErrorCode::Unavailable, format!("cannot connect to Redis at {}:{}: {}", t.host, t.port, scrub(&m, t)));
        let addr = (t.host.as_str(), t.port).to_socket_addrs().map_err(|e| unavailable(e.to_string()))?.next().ok_or_else(|| unavailable("the host name did not resolve".into()))?;
        let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(10)).map_err(|e| unavailable(e.to_string()))?;
        tcp.set_read_timeout(Some(timeout)).ok();
        tcp.set_write_timeout(Some(Duration::from_secs(10))).ok();
        tcp.set_nodelay(true).ok();
        let stream: Box<dyn Stream> = if t.tls {
            let connector = native_tls::TlsConnector::builder().build().map_err(|e| ToolError::new(ErrorCode::BackendError, format!("TLS setup failed: {e}")))?;
            Box::new(connector.connect(&t.host, tcp).map_err(|e| unavailable(format!("TLS handshake failed: {e}")))?)
        } else {
            Box::new(tcp)
        };
        let mut c = Self { io: BufReader::new(stream) };
        if let Some(pw) = &t.password {
            let mut auth = vec!["AUTH".to_string()];
            if let Some(u) = &t.user {
                auth.push(u.clone());
            }
            auth.push(pw.clone());
            match c.send(&auth)? {
                Reply::Error(e) => return Err(ToolError::new(ErrorCode::AuthFailed, format!("Redis refused the login: {}", scrub(&e, t)))),
                _ => {}
            }
        }
        if t.database != 0 {
            if let Reply::Error(e) = c.send(&["SELECT".to_string(), t.database.to_string()])? {
                let low = e.to_ascii_lowercase();
                let code = if low.starts_with("noauth") { ErrorCode::AuthRequired } else if low.contains("wrongpass") { ErrorCode::AuthFailed } else { ErrorCode::NotFound };
                let hint = if code == ErrorCode::AuthRequired { " Add \"secret\": \"<name>\" to the profile and store the password with: aqua-cli secret-set <name>" } else { "" };
                return Err(ToolError::new(code, format!("cannot select database {}: {}.{hint}", t.database, scrub(&e, t))));
            }
        }
        Ok(c)
    }

    pub fn send(&mut self, args: &[String]) -> Result<Reply, ToolError> {
        self.io.get_mut().write_all(&encode(args)).and_then(|_| self.io.get_mut().flush()).map_err(|e| ToolError::new(ErrorCode::Unavailable, format!("Redis connection: {e}")))?;
        read_reply(&mut self.io, 0)
    }
}

// ---------------------------------------------------------------- results

fn text(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) if s.chars().count() > 500 => Value::String(format!("{}… [cut, {} chars]", s.chars().take(500).collect::<String>(), s.chars().count())),
        Ok(s) => Value::String(s.to_string()),
        Err(_) => Value::String(format!("<binary {} bytes>", b.len())),
    }
}

/// A reply as JSON, arrays cut to `max_items`. `pairs` treats an array as `field, value, …`
/// (HGETALL) and masks values whose field is named like a credential.
pub fn to_json(r: &Reply, max_items: usize, pairs: bool) -> (Value, bool) {
    match r {
        Reply::Simple(s) => (json!(s), false),
        Reply::Error(e) => (json!({ "error": e }), false),
        Reply::Int(i) => (json!(i), false),
        Reply::Bulk(None) | Reply::Array(None) => (Value::Null, false),
        Reply::Bulk(Some(b)) => (text(b), false),
        Reply::Array(Some(items)) => {
            let truncated = items.len() > max_items;
            let mut out: Vec<Value> = Vec::new();
            let mut hide_next = false;
            for (i, it) in items.iter().take(max_items).enumerate() {
                let (v, _) = to_json(it, max_items, false);
                if pairs && i % 2 == 1 && hide_next {
                    out.push(json!("[REDACTED]"));
                } else {
                    if pairs && i % 2 == 0 {
                        hide_next = v.as_str().is_some_and(is_secret_key);
                    }
                    out.push(v);
                }
            }
            (Value::Array(out), truncated)
        }
    }
}

pub fn render(v: &Value) -> String {
    match v {
        Value::Null => "(nil)".into(),
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().enumerate().map(|(i, x)| format!("{}) {}", i + 1, render(x))).collect::<Vec<_>>().join("\n"),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn only_listed_commands_run_and_the_dangerous_ones_say_why() {
        for (c, k) in [("get", Kind::Read), ("HGETALL", Kind::Read), ("scan", Kind::Read), ("SET", Kind::Write), ("incr", Kind::Write), ("DEL", Kind::Delete), ("hdel", Kind::Delete)] {
            assert_eq!(classify(c).unwrap(), k, "{c}");
        }
        for c in ["FLUSHALL", "flushdb", "CONFIG", "EVAL", "KEYS", "SHUTDOWN", "ACL", "CLIENT", "SORT", "MONITOR", "MULTI", "SELECT", "AUTH", "RENAME", "SCRIPT", "MODULE", "DEBUG", "REPLICAOF"] {
            let e = classify(c).unwrap_err();
            assert_eq!(e.code, ErrorCode::QueryRejected, "{c}");
            assert!(e.message.contains("never available"), "{c}: {}", e.message);
        }
        assert!(classify("SOMETHINGNEW").unwrap_err().message.contains("not on the list"));
    }

    #[test]
    fn arguments_are_checked() {
        assert!(check(&s(&["GET", "k"])).is_ok());
        assert!(check(&s(&["GET"])).is_err(), "needs a key");
        assert!(check(&[]).is_err());
        assert!(check(&s(&["G ET", "k"])).is_err());
        assert!(check(&s(&["GET", "a\u{0}b"])).is_err());
        assert!(check(&s(&["DEL", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11"])).is_err(), "at most 10 keys");
        assert!(check(&s(&["SCAN", "0", "COUNT", "100"])).is_ok());
        assert!(check(&s(&["SCAN", "0", "COUNT", "100000"])).is_err());
        assert!(check(&s(&["SCAN", "0", "COUNT", "x"])).is_err());
        assert!(check(&s(&["INFO", "keyspace"])).is_ok() && check(&s(&["INFO", "all"])).is_err() && check(&s(&["INFO", "commandstats"])).is_err());
        assert!(check(&s(&["LRANGE", "k", "0"])).is_err());
        assert_eq!(check(&vec!["GET".to_string(); 70]).unwrap_err().code, ErrorCode::InvalidArguments);
    }

    #[test]
    fn commands_are_encoded_as_bulk_strings_so_no_argument_can_inject_another_command() {
        assert_eq!(encode(&s(&["SET", "k", "a\r\nDEL k"])), b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$8\r\na\r\nDEL k\r\n".to_vec());
    }

    #[test]
    fn replies_are_parsed_and_bad_ones_refused() {
        let mut r = Cursor::new(b"+OK\r\n-ERR nope\r\n:42\r\n$5\r\nhello\r\n$-1\r\n*2\r\n$1\r\na\r\n:1\r\n*-1\r\n".to_vec());
        assert_eq!(read_reply(&mut r, 0).unwrap(), Reply::Simple("OK".into()));
        assert_eq!(read_reply(&mut r, 0).unwrap(), Reply::Error("ERR nope".into()));
        assert_eq!(read_reply(&mut r, 0).unwrap(), Reply::Int(42));
        assert_eq!(read_reply(&mut r, 0).unwrap(), Reply::Bulk(Some(b"hello".to_vec())));
        assert_eq!(read_reply(&mut r, 0).unwrap(), Reply::Bulk(None));
        assert_eq!(read_reply(&mut r, 0).unwrap(), Reply::Array(Some(vec![Reply::Bulk(Some(b"a".to_vec())), Reply::Int(1)])));
        assert_eq!(read_reply(&mut r, 0).unwrap(), Reply::Array(None));
        for bad in [&b"?what\r\n"[..], b"$99999999999\r\n", b"*999999999\r\n", b"", b":xx\r\n", b"+no newline"] {
            assert!(read_reply(&mut Cursor::new(bad.to_vec()), 0).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
        let deep = "*1\r\n".repeat(20);
        assert!(read_reply(&mut Cursor::new(deep.into_bytes()), 0).is_err(), "nesting is bounded");
    }

    #[test]
    fn hash_values_under_secret_looking_fields_are_masked_and_arrays_are_cut() {
        let h = Reply::Array(Some(vec![
            Reply::Bulk(Some(b"name".to_vec())),
            Reply::Bulk(Some(b"bob".to_vec())),
            Reply::Bulk(Some(b"api_token".to_vec())),
            Reply::Bulk(Some(b"abc123".to_vec())),
            Reply::Bulk(Some(b"city".to_vec())),
            Reply::Bulk(Some(b"Oslo".to_vec())),
        ]));
        let (v, cut) = to_json(&h, 100, true);
        assert!(!cut);
        assert_eq!(v, json!(["name", "bob", "api_token", "[REDACTED]", "city", "Oslo"]));
        let (v2, cut2) = to_json(&h, 2, false);
        assert!(cut2 && v2.as_array().unwrap().len() == 2);
        assert_eq!(text(&[0xff, 0xfe]), json!("<binary 2 bytes>"));
        assert_eq!(render(&json!(["a", null, 3])), "1) a\n2) (nil)\n3) 3");
    }
}
