//! Database tools (`db.*`) for SQLite, PostgreSQL, MySQL/MariaDB and Redis. (MongoDB is not
//! included: its driver is large and asynchronous; a profile of that kind is refused with a message.)
//!
//! SQL engines share `db.schema`, `db.query` and `db.execute`; Redis uses `db.command`, whose
//! commands are an allow-list (see `redis`). Network engines add server-enforced read-only
//! sessions, side-effect-function checks and TLS for remote hosts (see `net`).
//!
//! - `db.query` runs on a connection opened read-only, so it cannot change anything whatever the
//!   SQL says, and only `SELECT`, `WITH`, `VALUES` and `EXPLAIN` are accepted, one statement at a time.
//! - `db.execute` runs `INSERT`, `UPDATE`, `DELETE` or `REPLACE`, never schema changes. `UPDATE` and
//!   `DELETE` need a real `WHERE`. The statement is tried first in a transaction that is rolled back,
//!   to tell the user how many rows it would change; the real run rolls back if it changes more than
//!   the profile allows.
//! - Profiles tagged `prod` always ask for writes.
//! - Values in columns named like credentials (`password`, `token`, `secret`…) are masked in results.

pub mod sql;
#[cfg(feature = "netdb")]
pub mod net;
#[cfg(feature = "netdb")]
pub mod redis;

use crate::fs::guard::{display, Op, PathGuard};
use crate::secrets::{valid_name, Secrets};
use aqua_tool_host::output::is_secret_key;
use aqua_tool_host::{
    Annotations, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider,
};
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Ro,
    Rw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Env {
    Dev,
    Staging,
    Prod,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbProfile {
    pub alias: String,
    /// `sqlite`, `postgres`, `mysql` (also MariaDB) or `redis`.
    pub kind: String,
    /// SQLite: the database file (inside a filesystem root).
    #[serde(default)]
    pub path: String,
    /// Network engines: the server. A host that is not this machine must use TLS.
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    /// PostgreSQL and MySQL: the database name. Redis: the database number (default 0).
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    /// Name of the password in Credential Manager (`aqua-cli secret-set NAME`). Never the password itself.
    #[serde(default)]
    pub secret: Option<String>,
    /// TLS to the server. Defaults to on for any host but this machine; off is refused for those.
    #[serde(default)]
    pub tls: Option<bool>,
    #[serde(default = "default_mode")]
    pub mode: Mode,
    #[serde(default = "default_env")]
    pub env: Env,
    /// Writes to a trusted `dev` profile run without a prompt; everything else asks.
    #[serde(default)]
    pub trusted: bool,
    #[serde(default = "default_rows")]
    pub max_rows: usize,
    #[serde(default = "default_write_rows")]
    pub max_write_rows: usize,
}

fn default_mode() -> Mode {
    Mode::Ro
}
fn default_env() -> Env {
    Env::Dev
}
fn default_rows() -> usize {
    200
}
fn default_write_rows() -> usize {
    100
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbConfig {
    #[serde(default)]
    pub profiles: Vec<DbProfile>,
}

impl DbConfig {
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for p in &self.profiles {
            if p.alias.is_empty() || p.alias.len() > 40 || !p.alias.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                return Err(format!("profile alias '{}' must be 1-40 letters, digits, '_' or '-'", p.alias));
            }
            if !seen.insert(p.alias.to_ascii_lowercase()) {
                return Err(format!("profile alias '{}' is listed twice", p.alias));
            }
            p.validate_engine()?;
            if p.max_rows == 0 || p.max_rows > 1000 || p.max_write_rows == 0 || p.max_write_rows > 10_000 {
                return Err(format!("profile '{}': max_rows must be 1-1000 and max_write_rows 1-10000", p.alias));
            }
        }
        Ok(())
    }
}

impl DbProfile {
    pub fn is_network(&self) -> bool {
        matches!(self.kind.as_str(), "postgres" | "mysql" | "redis")
    }

    pub fn default_port(&self) -> u16 {
        match self.kind.as_str() {
            "postgres" => 5432,
            "mysql" => 3306,
            _ => 6379,
        }
    }

    fn loopback(&self) -> bool {
        matches!(self.host.as_deref().map(|h| h.to_ascii_lowercase()).as_deref(), Some("localhost") | Some("127.0.0.1") | Some("::1") | Some("[::1]"))
    }

    /// TLS on the wire: as configured, else on unless the server is this machine.
    pub fn use_tls(&self) -> bool {
        self.tls.unwrap_or(!self.loopback())
    }

    fn validate_engine(&self) -> Result<(), String> {
        let a = &self.alias;
        match self.kind.as_str() {
            "sqlite" => {
                if self.path.trim().is_empty() {
                    return Err(format!("profile '{a}': a sqlite profile needs a path"));
                }
                if self.host.is_some() || self.port.is_some() || self.database.is_some() || self.user.is_some() || self.secret.is_some() || self.tls.is_some() {
                    return Err(format!("profile '{a}': host, port, database, user, secret and tls are for network engines, not sqlite"));
                }
                Ok(())
            }
            "postgres" | "mysql" | "redis" => {
                if !cfg!(feature = "netdb") {
                    return Err(format!("profile '{a}': this build has no network database drivers (the netdb feature is off)"));
                }
                if !self.path.is_empty() {
                    return Err(format!("profile '{a}': path is for sqlite; use host, port and database"));
                }
                let host = self.host.as_deref().unwrap_or("");
                if host.is_empty() || host.len() > 253 || !host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']')) {
                    return Err(format!("profile '{a}': host must be a plain name or address"));
                }
                let port = self.port.unwrap_or(self.default_port());
                if port == 0 || (self.loopback() && crate::http::policy::AQUA_PORTS.contains(&port)) {
                    return Err(format!("profile '{a}': port {port} is not allowed"));
                }
                if self.kind != "redis" {
                    let db = self.database.as_deref().unwrap_or("");
                    if db.is_empty() || db.len() > 63 || !db.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '$')) {
                        return Err(format!("profile '{a}': database is required (letters, digits, _ - $)"));
                    }
                    let user = self.user.as_deref().unwrap_or("");
                    if user.is_empty() || user.len() > 63 || !user.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '$' | '@')) {
                        return Err(format!("profile '{a}': user is required (letters, digits, _ - . $ @)"));
                    }
                } else {
                    if let Some(d) = &self.database {
                        if d.parse::<u32>().map_or(true, |n| n > 255) {
                            return Err(format!("profile '{a}': a Redis database is a number from 0 to 255"));
                        }
                    }
                    if let Some(u) = &self.user {
                        if u.is_empty() || u.len() > 63 || !u.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) {
                            return Err(format!("profile '{a}': the Redis user is not valid"));
                        }
                    }
                }
                if let Some(s) = &self.secret {
                    if !valid_name(s) {
                        return Err(format!("profile '{a}': '{s}' is not a valid secret name"));
                    }
                }
                if !self.use_tls() && !self.loopback() {
                    return Err(format!("profile '{a}': a connection to another machine must use TLS (do not set tls to false)"));
                }
                Ok(())
            }
            "mongodb" => Err(format!("profile '{a}': MongoDB is not included in this build (its driver is large and asynchronous)")),
            other => Err(format!("profile '{a}': unknown engine '{other}' (sqlite, postgres, mysql, redis)")),
        }
    }
}

struct Profile {
    entry: DbProfile,
    path: std::path::PathBuf,
}

struct Inner {
    profiles: Vec<Profile>,
    secrets: Arc<dyn Secrets>,
}

/// The database tools. State is behind an `Arc` so blocking tasks can own it.
pub struct DbProvider(Arc<Inner>);

impl DbProvider {
    pub fn new(cfg: DbConfig, guard: Arc<PathGuard>, secrets: Arc<dyn Secrets>) -> Result<Self, String> {
        cfg.validate()?;
        let mut profiles = Vec::new();
        for entry in cfg.profiles {
            if entry.is_network() {
                profiles.push(Profile { entry, path: std::path::PathBuf::new() });
                continue;
            }
            let op = if entry.mode == Mode::Rw { Op::Write } else { Op::Read };
            let resolved = guard.resolve(&entry.path, op).map_err(|e| format!("profile '{}': {}", entry.alias, e.message))?;
            if !resolved.path.is_file() {
                return Err(format!("profile '{}': {} is not a file", entry.alias, display(&resolved.path)));
            }
            profiles.push(Profile { entry, path: resolved.path });
        }
        Ok(Self(Arc::new(Inner { profiles, secrets })))
    }
}

fn bad(msg: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArguments, msg)
}

fn rejected(msg: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::QueryRejected, msg)
}

fn arg_str<'a>(a: &'a Value, k: &str) -> &'a str {
    a.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

impl Inner {
    fn profile(&self, a: &Value) -> Result<&Profile, ToolError> {
        let alias = arg_str(a, "profile");
        if alias.is_empty() {
            return match self.profiles.as_slice() {
                [only] => Ok(only),
                [] => Err(ToolError::new(ErrorCode::NotFound, "no database profiles are configured; add them to data/tools/db.json")),
                many => Err(bad(format!("say which profile: {}", many.iter().map(|p| p.entry.alias.as_str()).collect::<Vec<_>>().join(", ")))),
            };
        }
        self.profiles.iter().find(|p| p.entry.alias.eq_ignore_ascii_case(alias)).ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("no database profile '{alias}'. Profiles: {}", self.profiles.iter().map(|p| p.entry.alias.as_str()).collect::<Vec<_>>().join(", "))))
    }

    fn open_ro(&self, p: &Profile) -> Result<Connection, ToolError> {
        let c = Connection::open_with_flags(&p.path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("cannot open {}: {e}", p.entry.alias)))?;
        c.busy_timeout(Duration::from_secs(3)).ok();
        // Belt and braces: even a statement the checks missed cannot write on this connection.
        c.execute_batch("PRAGMA query_only = ON;").ok();
        Ok(c)
    }

    fn open_rw(&self, p: &Profile) -> Result<Connection, ToolError> {
        if p.entry.mode != Mode::Rw {
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("profile '{}' is read-only", p.entry.alias)));
        }
        let c = Connection::open_with_flags(&p.path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("cannot open {}: {e}", p.entry.alias)))?;
        c.busy_timeout(Duration::from_secs(3)).ok();
        c.execute_batch("PRAGMA foreign_keys = ON;").ok();
        Ok(c)
    }
}

fn spec(name: &str, description: &str, classes: Vec<PermClass>, props: Value, required: &[&str], read_only: bool, untrusted: bool) -> ToolManifest {
    ToolManifest {
        name: name.into(),
        title: name.into(),
        description: description.into(),
        classes,
        default_decision: Decision::Auto,
        input_schema: json!({"type": "object", "properties": props, "required": required}),
        timeout_default_ms: 30_000,
        timeout_max_ms: 90_000,
        annotations: Annotations { read_only, destructive: !read_only, idempotent: read_only, open_world: false },
        untrusted_output: untrusted,
    }
}

fn params_schema() -> Value {
    json!({"type": "array", "maxItems": 100, "items": {"type": ["string", "number", "boolean", "null"]}})
}

impl ToolProvider for DbProvider {
    fn id(&self) -> &str {
        "db"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        let profile = json!({"type": "string", "maxLength": 40});
        vec![
            spec("db.profiles", "List the configured databases (name, engine, read-only or read-write, environment).", vec![PermClass::Read], json!({}), &[], true, false),
            spec("db.schema", "List tables with their columns and indexes, or describe one table.", vec![PermClass::Read],
                json!({"profile": profile.clone(), "table": {"type": "string", "maxLength": 128}}), &[], true, true),
            spec("db.query", "Run one read-only SELECT (or WITH/VALUES/EXPLAIN). Use ? placeholders with params instead of pasting values into the SQL.", vec![PermClass::Read],
                json!({"profile": profile.clone(), "sql": {"type": "string", "minLength": 1, "maxLength": 20000}, "params": params_schema(), "max_rows": {"type": "integer", "minimum": 1, "maximum": 1000}, "timeout_s": {"type": "integer", "minimum": 1, "maximum": 60, "default": 10}}), &["sql"], true, true),
            spec("db.execute", "Run one INSERT, UPDATE, DELETE or REPLACE. UPDATE and DELETE need a real WHERE. Asks first and reports how many rows it will change. No schema changes.", vec![PermClass::Write],
                json!({"profile": profile.clone(), "sql": {"type": "string", "minLength": 1, "maxLength": 20000}, "params": params_schema()}), &["sql"], false, false),
            spec("db.command", "Run one Redis command from an allow-list, e.g. [\"GET\",\"key\"] or [\"SCAN\",\"0\",\"COUNT\",\"100\"]. Reads run; writes ask.", vec![PermClass::Read],
                json!({"profile": profile, "command": {"type": "array", "minItems": 1, "maxItems": 64, "items": {"type": "string", "maxLength": 65536}}, "max_items": {"type": "integer", "minimum": 1, "maximum": 1000}}), &["command"], false, true),
        ]
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        let s = &self.0;
        match tool {
            "db.profiles" => Ok(Classification { classes: vec![PermClass::Read], target: "db.json".into(), summary: "List database profiles".into(), target_allowed: true }),
            "db.schema" => {
                let p = s.profile(args)?;
                Ok(Classification { classes: vec![PermClass::Read], target: p.entry.alias.clone(), summary: format!("Read the schema of {}", p.entry.alias), target_allowed: true })
            }
            "db.query" => {
                let p = s.profile(args)?;
                s.require_sql(p, "db.query")?;
                check_read_sql(arg_str(args, "sql"))?;
                s.check_dialect(p, arg_str(args, "sql"), false)?;
                Ok(Classification { classes: vec![PermClass::Read], target: p.entry.alias.clone(), summary: format!("Query {}: {}", p.entry.alias, one_line(arg_str(args, "sql"), 200)), target_allowed: true })
            }
            "db.execute" => {
                let p = s.profile(args)?;
                s.require_sql(p, "db.execute")?;
                let kind = check_write_sql(arg_str(args, "sql"))?;
                s.check_dialect(p, arg_str(args, "sql"), true)?;
                if p.entry.mode != Mode::Rw {
                    return Err(ToolError::new(ErrorCode::PermissionDenied, format!("profile '{}' is read-only", p.entry.alias)));
                }
                // Try it and roll back, so the prompt can say how many rows the statement would change.
                let would = s.dry_run(p, arg_str(args, "sql"), &args["params"])?;
                let mut classes = vec![PermClass::Write];
                let prod = p.entry.env == Env::Prod;
                if prod {
                    classes.push(PermClass::Destructive);
                }
                let mut summary = format!("{} on {} ({}): {}. It would change {would} row(s)", kind.to_uppercase(), p.entry.alias, env_name(p.entry.env), one_line(arg_str(args, "sql"), 300));
                if would > p.entry.max_write_rows {
                    summary.push_str(&format!(" — more than the limit of {}, so it will be rolled back", p.entry.max_write_rows));
                }
                Ok(Classification { classes, target: p.entry.alias.clone(), summary, target_allowed: p.entry.trusted && p.entry.env == Env::Dev })
            }
            "db.command" => s.classify_command(args),
            other => Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
        }
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let inner = self.0.clone();
        let tool = tool.to_string();
        let cancel = ctx.cancel.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || inner.dispatch(&tool, &args, &cancel))
                .await
                .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("database task failed: {e}")))?.map(ProviderOutput::marked_private)
        })
    }
}

fn env_name(e: Env) -> &'static str {
    match e {
        Env::Dev => "dev",
        Env::Staging => "staging",
        Env::Prod => "PRODUCTION",
    }
}

fn one_line(s: &str, n: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= n {
        flat
    } else {
        format!("{}…", flat.chars().take(n).collect::<String>())
    }
}

/// Accept a single read-only statement.
fn check_read_sql(sql_text: &str) -> Result<(), ToolError> {
    if sql_text.contains('\0') {
        return Err(bad("the SQL contains a NUL character"));
    }
    let skel = sql::skeleton(sql_text);
    if sql::has_second_statement(&skel) {
        return Err(rejected("only one statement per call; remove the ';' and what follows it"));
    }
    let kw = sql::first_keyword(&skel);
    if !matches!(kw.as_str(), "select" | "with" | "values" | "explain") {
        return Err(rejected(format!("db.query only runs SELECT, WITH, VALUES and EXPLAIN, not '{kw}'. To change data use db.execute.")));
    }
    for w in ["attach", "detach", "load_extension", "pragma", "vacuum"] {
        if sql::contains_word(&skel, w) {
            return Err(rejected(format!("'{w}' is not allowed in a query")));
        }
    }
    if kw == "with" || kw == "explain" {
        for w in ["insert", "update", "delete", "replace", "create", "drop", "alter"] {
            if sql::contains_word(&skel, w) {
                return Err(rejected(format!("a read-only query may not contain '{w}'")));
            }
        }
    }
    Ok(())
}

/// Accept a single INSERT, UPDATE, DELETE or REPLACE and return its kind.
fn check_write_sql(sql_text: &str) -> Result<String, ToolError> {
    if sql_text.contains('\0') {
        return Err(bad("the SQL contains a NUL character"));
    }
    let skel = sql::skeleton(sql_text);
    if sql::has_second_statement(&skel) {
        return Err(rejected("only one statement per call; remove the ';' and what follows it"));
    }
    let kw = sql::first_keyword(&skel);
    match kw.as_str() {
        "insert" | "update" | "delete" | "replace" => {}
        k if sql::is_ddl_keyword(k) => {
            return Err(rejected(format!("'{k}' changes the schema or the database settings. Schema changes are not available through these tools yet (a reviewed migration tool is planned).")));
        }
        "select" | "with" | "values" | "explain" => return Err(rejected("that is a read; use db.query")),
        other => return Err(rejected(format!("db.execute runs INSERT, UPDATE, DELETE or REPLACE, not '{other}'"))),
    }
    if sql::contains_word(&skel, "returning") {
        return Err(rejected("RETURNING is not supported; run db.query afterwards to see the rows"));
    }
    for w in ["attach", "detach", "load_extension", "pragma", "vacuum", "drop", "create", "alter"] {
        if sql::contains_word(&skel, w) {
            return Err(rejected(format!("'{w}' is not allowed in a statement")));
        }
    }
    if matches!(kw.as_str(), "update" | "delete") {
        if !sql::contains_word(&skel, "where") {
            return Err(rejected(format!("{} without WHERE would change every row; add a WHERE that selects the rows", kw.to_uppercase())));
        }
        if sql::tautological_where(&skel) {
            return Err(rejected("that WHERE is true for every row; write a condition that selects the rows you mean"));
        }
    }
    Ok(kw)
}

fn bind(params: &Value) -> Result<Vec<SqlValue>, ToolError> {
    let Some(arr) = params.as_array() else { return Ok(Vec::new()) };
    arr.iter()
        .map(|v| match v {
            Value::Null => Ok(SqlValue::Null),
            Value::Bool(b) => Ok(SqlValue::Integer(*b as i64)),
            Value::Number(n) => Ok(match n.as_i64() {
                Some(i) => SqlValue::Integer(i),
                None => SqlValue::Real(n.as_f64().unwrap_or(0.0)),
            }),
            Value::String(s) => Ok(SqlValue::Text(s.clone())),
            _ => Err(bad("params must be strings, numbers, booleans or null")),
        })
        .collect()
}

fn sql_err(e: rusqlite::Error, what: &str) -> ToolError {
    let msg = e.to_string();
    let low = msg.to_ascii_lowercase();
    if matches!(e, rusqlite::Error::MultipleStatement) {
        return rejected("only one statement per call");
    }
    if low.contains("interrupted") {
        return ToolError::new(ErrorCode::Timeout, "the query ran too long and was stopped");
    }
    if low.contains("no such table") || low.contains("no such column") || low.contains("no such index") {
        return ToolError::new(ErrorCode::NotFound, format!("{what}: {msg}"));
    }
    if low.contains("syntax error") || low.contains("incomplete input") || low.contains("unrecognized token") || low.contains("wrong number of") || low.contains("column index out of range") || low.contains("invalid parameter") {
        return bad(format!("{what}: {msg}"));
    }
    if low.contains("constraint failed") || low.contains("unique constraint") {
        return ToolError::new(ErrorCode::Conflict, format!("{what}: {msg}"));
    }
    if low.contains("readonly") || low.contains("read-only") {
        return ToolError::new(ErrorCode::PermissionDenied, format!("{what}: {msg}"));
    }
    if low.contains("locked") || low.contains("busy") {
        return ToolError::new(ErrorCode::Unavailable, format!("{what}: the database is busy: {msg}"));
    }
    ToolError::new(ErrorCode::BackendError, format!("{what}: {msg}"))
}

fn cell(v: ValueRef, masked: bool) -> Value {
    if masked {
        return Value::String("[REDACTED]".into());
    }
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => {
            let s = String::from_utf8_lossy(t);
            if s.chars().count() > 500 {
                Value::String(format!("{}… [cut, {} chars]", s.chars().take(500).collect::<String>(), s.chars().count()))
            } else {
                Value::String(s.to_string())
            }
        }
        ValueRef::Blob(b) => Value::String(format!("<blob {} bytes>", b.len())),
    }
}

impl Inner {
    /// Run `sql_text` in a transaction that is always rolled back and report the rows it would change.
    fn dry_run(&self, p: &Profile, sql_text: &str, params: &Value) -> Result<usize, ToolError> {
        if p.entry.is_network() {
            return self.net_execute(p, sql_text, params, true).map(|(n, _)| n);
        }
        let c = self.open_rw(p)?;
        c.execute_batch("BEGIN IMMEDIATE").map_err(|e| sql_err(e, "begin"))?;
        let bound = bind(params);
        let out = bound.and_then(|b| c.execute(sql_text, rusqlite::params_from_iter(b.iter())).map_err(|e| sql_err(e, "the statement")));
        let _ = c.execute_batch("ROLLBACK");
        out
    }

    fn dispatch(&self, tool: &str, a: &Value, cancel: &Arc<AtomicBool>) -> Result<ProviderOutput, ToolError> {
        match tool {
            "db.profiles" => self.profiles_out(),
            "db.schema" => self.schema(a),
            "db.query" => self.query(a, cancel),
            "db.execute" => self.execute(a),
            "db.command" => self.command(a),
            other => Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
        }
    }

    fn profiles_out(&self) -> Result<ProviderOutput, ToolError> {
        let rows: Vec<Value> = self.profiles.iter().map(|p| json!({"profile": p.entry.alias, "engine": p.entry.kind, "mode": p.entry.mode, "env": p.entry.env, "file": if p.entry.is_network() { None } else { p.path.file_name().map(|n| n.to_string_lossy().to_string()) }, "server": if p.entry.is_network() { Some(format!("{}:{}", p.entry.host.as_deref().unwrap_or(""), p.entry.port.unwrap_or(p.entry.default_port()))) } else { None }})).collect();
        let text = rows.iter().map(|r| format!("{} ({}, {}, {})", r["profile"].as_str().unwrap_or(""), r["engine"].as_str().unwrap_or(""), r["mode"].as_str().unwrap_or(""), r["env"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n");
        Ok(ProviderOutput { data: json!({ "profiles": rows }), text: if text.is_empty() { "No databases are configured. Add them to data/tools/db.json.".into() } else { text }, ..Default::default() })
    }

    fn schema(&self, a: &Value) -> Result<ProviderOutput, ToolError> {
        let p = self.profile(a)?;
        if p.entry.kind == "redis" {
            return self.redis_schema(p);
        }
        if p.entry.is_network() {
            return self.net_schema(p, arg_str(a, "table"));
        }
        let c = self.open_ro(p)?;
        let only = arg_str(a, "table");
        let mut names: Vec<String> = c
            .prepare("SELECT name FROM sqlite_master WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .and_then(|mut s| s.query_map([], |r| r.get::<_, String>(0))?.collect())
            .map_err(|e| sql_err(e, "schema"))?;
        if !only.is_empty() {
            names.retain(|n| n.eq_ignore_ascii_case(only));
            if names.is_empty() {
                return Err(ToolError::new(ErrorCode::NotFound, format!("no table or view '{only}'")));
            }
        }
        let mut tables = Vec::new();
        let mut text = String::new();
        for n in names {
            // Names come from sqlite_master, and are quoted for the PRAGMA.
            let q = n.replace('"', "\"\"");
            let cols: Vec<Value> = c
                .prepare(&format!("PRAGMA table_info(\"{q}\")"))
                .and_then(|mut s| s.query_map([], |r| Ok(json!({"name": r.get::<_, String>(1)?, "type": r.get::<_, String>(2)?, "not_null": r.get::<_, i64>(3)? != 0, "primary_key": r.get::<_, i64>(5)? > 0})))?.collect())
                .map_err(|e| sql_err(e, "schema"))?;
            let idx: Vec<Value> = c
                .prepare(&format!("PRAGMA index_list(\"{q}\")"))
                .and_then(|mut s| s.query_map([], |r| Ok(json!({"name": r.get::<_, String>(1)?, "unique": r.get::<_, i64>(2)? != 0})))?.collect())
                .unwrap_or_default();
            text.push_str(&format!("{n}({})\n", cols.iter().map(|c| format!("{} {}{}", c["name"].as_str().unwrap_or(""), c["type"].as_str().unwrap_or(""), if c["primary_key"] == true { " PK" } else { "" })).collect::<Vec<_>>().join(", ")));
            tables.push(json!({"table": n, "columns": cols, "indexes": idx}));
        }
        Ok(ProviderOutput { data: json!({"profile": p.entry.alias, "tables": tables}), text: text.trim_end().to_string(), ..Default::default() })
    }

    fn query(&self, a: &Value, cancel: &Arc<AtomicBool>) -> Result<ProviderOutput, ToolError> {
        let p = self.profile(a)?;
        let sql_text = arg_str(a, "sql");
        self.require_sql(p, "db.query")?;
        check_read_sql(sql_text)?;
        let max_rows = a.get("max_rows").and_then(|v| v.as_u64()).map(|n| n as usize).unwrap_or(p.entry.max_rows).min(1000);
        let timeout = Duration::from_secs(a.get("timeout_s").and_then(|v| v.as_u64()).unwrap_or(10).clamp(1, 60));
        if p.entry.is_network() {
            self.check_dialect(p, sql_text, false)?;
            return self.net_query(p, sql_text, &a["params"], max_rows, timeout.as_secs());
        }
        let conn = self.open_ro(p)?;
        // A watchdog interrupts the query at the deadline or when the run is cancelled.
        let handle = conn.get_interrupt_handle();
        let done = Arc::new(AtomicBool::new(false));
        let (d2, c2) = (done.clone(), cancel.clone());
        let watchdog = std::thread::spawn(move || {
            let until = std::time::Instant::now() + timeout;
            while !d2.load(Ordering::SeqCst) {
                if std::time::Instant::now() >= until || c2.load(Ordering::SeqCst) {
                    handle.interrupt();
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let result = (|| -> Result<ProviderOutput, ToolError> {
            let mut stmt = conn.prepare(sql_text).map_err(|e| sql_err(e, "the query"))?;
            if !stmt.readonly() {
                return Err(rejected("that statement is not read-only"));
            }
            let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
            let masked: Vec<bool> = cols.iter().map(|c| is_secret_key(c)).collect();
            let bound = bind(&a["params"])?;
            let mut rows = stmt.query(rusqlite::params_from_iter(bound.iter())).map_err(|e| sql_err(e, "the query"))?;
            let mut out: Vec<Value> = Vec::new();
            let mut truncated = false;
            while let Some(row) = rows.next().map_err(|e| sql_err(e, "the query"))? {
                if out.len() >= max_rows {
                    truncated = true;
                    break;
                }
                let mut obj = serde_json::Map::new();
                for (i, name) in cols.iter().enumerate() {
                    obj.insert(name.clone(), cell(row.get_ref(i).map_err(|e| sql_err(e, "the query"))?, masked[i]));
                }
                out.push(Value::Object(obj));
            }
            let mut text = format!("{} row(s){}", out.len(), if truncated { format!(" (more exist; stopped at {max_rows})") } else { String::new() });
            if !cols.is_empty() {
                text.push_str(&format!("\n{}", cols.join(" | ")));
                for r in &out {
                    text.push_str(&format!("\n{}", cols.iter().map(|c| match &r[c] { Value::String(s) => s.clone(), v => v.to_string() }).collect::<Vec<_>>().join(" | ")));
                }
            }
            let mut warnings = Vec::new();
            if masked.iter().any(|m| *m) {
                warnings.push("Values of columns named like credentials are masked.".to_string());
            }
            Ok(ProviderOutput { data: json!({"profile": p.entry.alias, "columns": cols, "rows": out, "row_count": out.len(), "truncated": truncated}), text, warnings, ..Default::default() })
        })();
        done.store(true, Ordering::SeqCst);
        let _ = watchdog.join();
        if cancel.load(Ordering::SeqCst) && result.is_err() {
            return Err(ToolError::new(ErrorCode::Cancelled, "the run was cancelled and the query was stopped"));
        }
        result
    }

    fn execute(&self, a: &Value) -> Result<ProviderOutput, ToolError> {
        let p = self.profile(a)?;
        let sql_text = arg_str(a, "sql");
        self.require_sql(p, "db.execute")?;
        let kind = check_write_sql(sql_text)?;
        if p.entry.mode != Mode::Rw {
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("profile '{}' is read-only", p.entry.alias)));
        }
        if p.entry.is_network() {
            self.check_dialect(p, sql_text, true)?;
            let (changed, committed) = self.net_execute(p, sql_text, &a["params"], false)?;
            if !committed {
                return Err(ToolError::new(ErrorCode::ResourceLimit, format!("that statement would change {changed} rows, more than the limit of {} for {}; nothing was changed. Narrow the WHERE or change it in smaller batches.", p.entry.max_write_rows, p.entry.alias)));
            }
            return Ok(ProviderOutput {
                data: json!({"profile": p.entry.alias, "statement": kind, "rows_affected": changed}),
                text: format!("{} changed {changed} row(s) in {}.", kind.to_uppercase(), p.entry.alias),
                secrets: self.password(p).ok().flatten().into_iter().collect(),
                ..Default::default()
            });
        }
        let conn = self.open_rw(p)?;
        conn.execute_batch("BEGIN IMMEDIATE").map_err(|e| sql_err(e, "begin"))?;
        let bound = bind(&a["params"]);
        let run = bound.and_then(|b| conn.execute(sql_text, rusqlite::params_from_iter(b.iter())).map_err(|e| sql_err(e, "the statement")));
        match run {
            Ok(changed) if changed <= p.entry.max_write_rows => {
                let last = conn.last_insert_rowid();
                conn.execute_batch("COMMIT").map_err(|e| sql_err(e, "commit"))?;
                Ok(ProviderOutput {
                    data: json!({"profile": p.entry.alias, "statement": kind, "rows_affected": changed, "last_insert_rowid": if kind == "insert" || kind == "replace" { Some(last) } else { None }}),
                    text: format!("{} changed {changed} row(s) in {}.", kind.to_uppercase(), p.entry.alias),
                    ..Default::default()
                })
            }
            Ok(changed) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(ToolError::new(ErrorCode::ResourceLimit, format!("that statement would change {changed} rows, more than the limit of {} for {}; nothing was changed. Narrow the WHERE or change it in smaller batches.", p.entry.max_write_rows, p.entry.alias)))
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }
}

/// Rows from any SQL engine in the shape `db.query` returns.
fn rows_output(alias: &str, cols: Vec<String>, rows: Vec<Vec<Value>>, truncated: bool, max_rows: usize, secrets: Vec<String>) -> ProviderOutput {
    let masked: Vec<bool> = cols.iter().map(|c| is_secret_key(c)).collect();
    let objs: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut o = serde_json::Map::new();
            for (i, name) in cols.iter().enumerate() {
                o.insert(name.clone(), if masked[i] { Value::String("[REDACTED]".into()) } else { r.get(i).cloned().unwrap_or(Value::Null) });
            }
            Value::Object(o)
        })
        .collect();
    let mut text = format!("{} row(s){}", objs.len(), if truncated { format!(" (more exist; stopped at {max_rows})") } else { String::new() });
    if !cols.is_empty() {
        text.push_str(&format!("\n{}", cols.join(" | ")));
        for r in &objs {
            text.push_str(&format!("\n{}", cols.iter().map(|c| match &r[c] { Value::String(s) => s.clone(), v => v.to_string() }).collect::<Vec<_>>().join(" | ")));
        }
    }
    let warnings = if masked.iter().any(|m| *m) { vec!["Values of columns named like credentials are masked.".to_string()] } else { vec![] };
    ProviderOutput { data: json!({"profile": alias, "columns": cols, "rows": objs, "row_count": objs.len(), "truncated": truncated}), text, warnings, secrets, ..Default::default() }
}

fn schema_output(alias: &str, tables: Vec<(String, Vec<(String, String, bool, bool)>)>, secrets: Vec<String>) -> ProviderOutput {
    let mut text = String::new();
    let mut out = Vec::new();
    for (name, cols) in tables {
        text.push_str(&format!("{name}({})\n", cols.iter().map(|(c, t, _, pk)| format!("{c} {t}{}", if *pk { " PK" } else { "" })).collect::<Vec<_>>().join(", ")));
        out.push(json!({"table": name, "columns": cols.iter().map(|(c, t, nn, pk)| json!({"name": c, "type": t, "not_null": nn, "primary_key": pk})).collect::<Vec<_>>()}));
    }
    ProviderOutput { data: json!({"profile": alias, "tables": out}), text: if text.is_empty() { "No tables.".into() } else { text.trim_end().to_string() }, secrets, ..Default::default() }
}

#[cfg(feature = "netdb")]
impl Inner {
    /// The password from Credential Manager, at the moment it is needed.
    fn password(&self, p: &Profile) -> Result<Option<String>, ToolError> {
        let Some(name) = &p.entry.secret else { return Ok(None) };
        let v = self
            .secrets
            .get(name)
            .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("credential store: {e}")))?
            .ok_or_else(|| ToolError::new(ErrorCode::AuthRequired, format!("the password for '{}' is not stored. Store it yourself with: aqua-cli secret-set {name}", p.entry.alias)))?;
        Ok(Some(v))
    }

    fn require_sql(&self, p: &Profile, tool: &str) -> Result<(), ToolError> {
        if p.entry.kind == "redis" {
            return Err(bad(format!("'{}' is a Redis profile: use db.command, not {tool}", p.entry.alias)));
        }
        Ok(())
    }

    /// Extra checks for server engines: side-effect functions and locking reads.
    fn check_dialect(&self, p: &Profile, sql_text: &str, writes: bool) -> Result<(), ToolError> {
        if p.entry.is_network() {
            // MySQL and MariaDB run the text of `/*! ... */` and `/*M! ... */` as SQL, while every check here reads a comment as nothing: the
            // statement `SELECT 1 /*!50000 UNION SELECT 2 INTO OUTFILE '/x' */` showed no "into outfile" to any of them and counted as a plain read.
            if sql_text.contains("/*!") || sql_text.contains("/*M!") {
                return Err(ToolError::new(
                    ErrorCode::QueryRejected,
                    "a comment that starts with /*! or /*M! is run as SQL by MySQL and MariaDB, so it cannot be left unchecked; take it out of the statement",
                ));
            }
            net::check_side_effects(&sql::skeleton(sql_text), writes)?;
        }
        Ok(())
    }

    fn net_target(&self, p: &Profile) -> Result<net::Target, ToolError> {
        Ok(net::Target {
            engine: if p.entry.kind == "postgres" { net::Engine::Postgres } else { net::Engine::Mysql },
            host: p.entry.host.clone().unwrap_or_default(),
            port: p.entry.port.unwrap_or(p.entry.default_port()),
            database: p.entry.database.clone().unwrap_or_default(),
            user: p.entry.user.clone().unwrap_or_default(),
            password: self.password(p)?,
            tls: p.entry.use_tls(),
        })
    }

    fn net_query(&self, p: &Profile, sql_text: &str, params: &Value, max_rows: usize, timeout_s: u64) -> Result<ProviderOutput, ToolError> {
        let t = self.net_target(p)?;
        let r = match t.engine {
            net::Engine::Postgres => net::pg_query(&t, sql_text, params, max_rows, timeout_s)?,
            net::Engine::Mysql => net::my_query(&t, sql_text, params, max_rows, timeout_s)?,
        };
        Ok(rows_output(&p.entry.alias, r.columns, r.rows, r.truncated, max_rows, t.password.into_iter().collect()))
    }

    fn net_execute(&self, p: &Profile, sql_text: &str, params: &Value, dry_run: bool) -> Result<(usize, bool), ToolError> {
        let t = self.net_target(p)?;
        match t.engine {
            net::Engine::Postgres => net::pg_execute(&t, sql_text, params, p.entry.max_write_rows, dry_run),
            net::Engine::Mysql => net::my_execute(&t, sql_text, params, p.entry.max_write_rows, dry_run),
        }
    }

    fn net_schema(&self, p: &Profile, only: &str) -> Result<ProviderOutput, ToolError> {
        let t = self.net_target(p)?;
        let tables = match t.engine {
            net::Engine::Postgres => net::pg_schema(&t, only)?,
            net::Engine::Mysql => net::my_schema(&t, only)?,
        };
        if !only.is_empty() && tables.is_empty() {
            return Err(ToolError::new(ErrorCode::NotFound, format!("no table or view '{only}'")));
        }
        Ok(schema_output(&p.entry.alias, tables, t.password.into_iter().collect()))
    }

    fn redis_target(&self, p: &Profile) -> Result<redis::Target, ToolError> {
        Ok(redis::Target {
            host: p.entry.host.clone().unwrap_or_default(),
            port: p.entry.port.unwrap_or(p.entry.default_port()),
            user: p.entry.user.clone(),
            password: self.password(p)?,
            database: p.entry.database.as_deref().and_then(|d| d.parse().ok()).unwrap_or(0),
            tls: p.entry.use_tls(),
        })
    }

    fn command_args(a: &Value) -> Vec<String> {
        a.get("command").and_then(|v| v.as_array()).map(|x| x.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
    }

    fn classify_command(&self, a: &Value) -> Result<Classification, ToolError> {
        let p = self.profile(a)?;
        if p.entry.kind != "redis" {
            return Err(bad(format!("'{}' is a {} profile: use db.query and db.execute", p.entry.alias, p.entry.kind)));
        }
        let args = Self::command_args(a);
        let kind = redis::check(&args)?;
        let name = args[0].to_ascii_uppercase();
        let shown = one_line(&args.iter().map(|x| if x.len() > 80 { format!("{}…", x.chars().take(80).collect::<String>()) } else { x.clone() }).collect::<Vec<_>>().join(" "), 300);
        if kind == redis::Kind::Read {
            return Ok(Classification { classes: vec![PermClass::Read], target: p.entry.alias.clone(), summary: format!("Redis {name} on {}: {shown}", p.entry.alias), target_allowed: true });
        }
        if p.entry.mode != Mode::Rw {
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("profile '{}' is read-only", p.entry.alias)));
        }
        let mut classes = vec![PermClass::Write];
        if kind == redis::Kind::Delete || p.entry.env == Env::Prod {
            classes.push(PermClass::Destructive);
        }
        Ok(Classification { classes, target: p.entry.alias.clone(), summary: format!("Redis {name} on {} ({}): {shown}", p.entry.alias, env_name(p.entry.env)), target_allowed: p.entry.trusted && p.entry.env == Env::Dev })
    }

    fn command(&self, a: &Value) -> Result<ProviderOutput, ToolError> {
        let p = self.profile(a)?;
        if p.entry.kind != "redis" {
            return Err(bad(format!("'{}' is not a Redis profile", p.entry.alias)));
        }
        let args = Self::command_args(a);
        let kind = redis::check(&args)?;
        if kind != redis::Kind::Read && p.entry.mode != Mode::Rw {
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("profile '{}' is read-only", p.entry.alias)));
        }
        let max_items = a.get("max_items").and_then(|v| v.as_u64()).map(|n| n as usize).unwrap_or(p.entry.max_rows).clamp(1, 1000);
        let t = self.redis_target(p)?;
        let mut conn = redis::Conn::connect(&t, Duration::from_secs(15))?;
        let reply = conn.send(&args)?;
        let secrets: Vec<String> = t.password.iter().cloned().collect();
        if let redis::Reply::Error(e) = &reply {
            let low = e.to_ascii_lowercase();
            let code = if low.starts_with("noauth") || low.contains("wrongpass") { ErrorCode::AuthRequired } else if low.contains("readonly") { ErrorCode::PermissionDenied } else if low.contains("wrong number") || low.contains("syntax") || low.contains("not an integer") { ErrorCode::InvalidArguments } else if low.contains("wrongtype") { ErrorCode::InvalidArguments } else { ErrorCode::BackendError };
            return Err(ToolError::new(code, format!("Redis: {e}")));
        }
        let (value, truncated) = redis::to_json(&reply, max_items, args[0].eq_ignore_ascii_case("HGETALL"));
        let mut text = redis::render(&value);
        if truncated {
            text.push_str(&format!("\n… more items exist; stopped at {max_items}"));
        }
        Ok(ProviderOutput { data: json!({"profile": p.entry.alias, "command": args[0].to_ascii_uppercase(), "result": value, "truncated": truncated}), text, secrets, ..Default::default() })
    }

    /// A sample of keys with their types: `SCAN`, never `KEYS`.
    fn redis_schema(&self, p: &Profile) -> Result<ProviderOutput, ToolError> {
        let t = self.redis_target(p)?;
        let mut conn = redis::Conn::connect(&t, Duration::from_secs(15))?;
        let mut keys: Vec<String> = Vec::new();
        let mut cursor = "0".to_string();
        for _ in 0..20 {
            let r = conn.send(&["SCAN".into(), cursor.clone(), "COUNT".into(), "200".into()])?;
            let redis::Reply::Array(Some(parts)) = r else { break };
            if let (Some(redis::Reply::Bulk(Some(c))), Some(redis::Reply::Array(Some(ks)))) = (parts.first(), parts.get(1)) {
                cursor = String::from_utf8_lossy(c).to_string();
                keys.extend(ks.iter().filter_map(|k| if let redis::Reply::Bulk(Some(b)) = k { Some(String::from_utf8_lossy(b).to_string()) } else { None }));
            }
            if cursor == "0" || keys.len() >= 100 {
                break;
            }
        }
        keys.truncate(100);
        let mut rows = Vec::new();
        for k in &keys {
            let ty = match conn.send(&["TYPE".into(), k.clone()])? {
                redis::Reply::Simple(s) => s,
                _ => "?".into(),
            };
            rows.push((k.clone(), ty));
        }
        let size = match conn.send(&["DBSIZE".into()])? {
            redis::Reply::Int(n) => n,
            _ => -1,
        };
        let text = format!("{size} key(s); a sample of {}:\n{}", rows.len(), rows.iter().map(|(k, t)| format!("{k} ({t})")).collect::<Vec<_>>().join("\n"));
        Ok(ProviderOutput { data: json!({"profile": p.entry.alias, "keys": size, "sample": rows.iter().map(|(k, t)| json!({"key": k, "type": t})).collect::<Vec<_>>()}), text, secrets: t.password.into_iter().collect(), ..Default::default() })
    }
}

#[cfg(not(feature = "netdb"))]
impl Inner {
    fn password(&self, _p: &Profile) -> Result<Option<String>, ToolError> {
        Ok(None)
    }
    fn require_sql(&self, _p: &Profile, _tool: &str) -> Result<(), ToolError> {
        Ok(())
    }
    fn check_dialect(&self, _p: &Profile, _sql: &str, _w: bool) -> Result<(), ToolError> {
        Ok(())
    }
    fn unavailable() -> ToolError {
        ToolError::new(ErrorCode::UnsupportedPlatform, "this build has no network database drivers")
    }
    fn net_query(&self, _p: &Profile, _s: &str, _pa: &Value, _m: usize, _t: u64) -> Result<ProviderOutput, ToolError> {
        Err(Self::unavailable())
    }
    fn net_execute(&self, _p: &Profile, _s: &str, _pa: &Value, _d: bool) -> Result<(usize, bool), ToolError> {
        Err(Self::unavailable())
    }
    fn net_schema(&self, _p: &Profile, _o: &str) -> Result<ProviderOutput, ToolError> {
        Err(Self::unavailable())
    }
    fn redis_schema(&self, _p: &Profile) -> Result<ProviderOutput, ToolError> {
        Err(Self::unavailable())
    }
    fn classify_command(&self, _a: &Value) -> Result<Classification, ToolError> {
        Err(Self::unavailable())
    }
    fn command(&self, _a: &Value) -> Result<ProviderOutput, ToolError> {
        Err(Self::unavailable())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_sql_checks_accept_selects_and_refuse_everything_else() {
        for ok in ["SELECT * FROM t", "select 1", "WITH x AS (SELECT 1) SELECT * FROM x", "VALUES (1),(2)", "EXPLAIN QUERY PLAN SELECT 1", "SELECT 'DROP TABLE t; --' AS s", "select 1;", "SELECT a FROM t -- delete from t"] {
            assert!(check_read_sql(ok).is_ok(), "{ok}");
        }
        for bad in [
            "INSERT INTO t VALUES (1)", "UPDATE t SET a=1", "DELETE FROM t", "DROP TABLE t", "CREATE TABLE x(a)", "ALTER TABLE t ADD b", "ATTACH DATABASE 'x.db' AS x", "PRAGMA writable_schema=1", "PRAGMA table_info(t)", "VACUUM",
            "SELECT 1; DROP TABLE t", "SELECT 1; SELECT 2", "WITH d AS (SELECT 1) DELETE FROM t", "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x", "SELECT load_extension('x')", "BEGIN", "", "  ;  ",
        ] {
            assert!(check_read_sql(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn write_sql_checks_require_a_real_where_and_refuse_schema_changes() {
        for ok in ["INSERT INTO t(a) VALUES (?)", "UPDATE t SET a = 1 WHERE id = ?", "DELETE FROM t WHERE id = 5", "REPLACE INTO t VALUES (1)", "delete from t where name = 'x where y'"] {
            assert!(check_write_sql(ok).is_ok(), "{ok}");
        }
        for bad in [
            "UPDATE t SET a = 1", "DELETE FROM t", "DELETE FROM t WHERE 1=1", "UPDATE t SET a=1 WHERE true", "delete from t -- where id=1", "delete from t /* where id=1 */",
            "DROP TABLE t", "CREATE TABLE x(a)", "ALTER TABLE t RENAME TO u", "TRUNCATE t", "PRAGMA foreign_keys=OFF", "ATTACH DATABASE 'x' AS y", "VACUUM", "BEGIN", "COMMIT",
            "INSERT INTO t VALUES (1); DROP TABLE t", "INSERT INTO t VALUES (1) RETURNING *", "SELECT 1", "WITH x AS (SELECT 1) SELECT * FROM x", "INSERT INTO t SELECT load_extension('x')",
        ] {
            assert!(check_write_sql(bad).is_err(), "{bad:?}");
        }
        assert_eq!(check_write_sql("delete from t where id=1").unwrap(), "delete");
    }

    /// Found in a review: every check reads a comment as nothing, but MySQL and MariaDB run what is inside `/*! ... */`.
    #[cfg(feature = "netdb")]
    #[test]
    fn mysql_comments_that_the_server_runs_are_not_left_unchecked() {
        let inner = Inner { profiles: vec![], secrets: Arc::new(crate::secrets::MemorySecrets::new()) };
        let profile = Profile {
            entry: DbProfile { alias: "m".into(), kind: "mysql".into(), path: "x.db".into(), host: Some("db.example.com".into()), port: None, database: Some("app".into()), user: Some("reader".into()), secret: None, tls: None, mode: Mode::Ro, env: Env::Dev, trusted: false, max_rows: 100, max_write_rows: 10 },
            path: std::path::PathBuf::new(),
        };
        for bad in [
            "SELECT id FROM notes /*!50000 UNION SELECT 1,2 INTO OUTFILE '/tmp/x' */",
            "SELECT 1 /*!SLEEP(30)*/",
            "SELECT 1 /*M!100100 BENCHMARK(1,1) */",
            "SELECT 1 /*!*/",
        ] {
            let e = inner.check_dialect(&profile, bad, false).unwrap_err();
            assert!(e.message.contains("/*!"), "{bad}: {}", e.message);
        }
        for ok in ["SELECT 1 /* a comment */", "SELECT /*+ MAX_EXECUTION_TIME(1000) */ id FROM t", "SELECT 1 -- a note", "SELECT 'plain' AS x"] {
            assert!(inner.check_dialect(&profile, ok, false).is_ok(), "{ok}");
        }
        assert!(inner.check_dialect(&profile, "SELECT SLEEP(5)", false).is_err(), "the other checks still apply");
    }

    #[test]
    fn config_rejects_other_engines_bad_aliases_and_bad_limits() {
        let p = |alias: &str, kind: &str| DbProfile { alias: alias.into(), kind: kind.into(), path: "x.db".into(), host: None, port: None, database: None, user: None, secret: None, tls: None, mode: Mode::Ro, env: Env::Dev, trusted: false, max_rows: 100, max_write_rows: 10 };
        assert!(DbConfig { profiles: vec![p("app", "sqlite")] }.validate().is_ok());
        // Network engines need their own fields, not a file path.
        for kind in ["postgres", "mysql", "redis"] {
            assert!(DbConfig { profiles: vec![p("a", kind)] }.validate().is_err(), "{kind} with a path and no host");
        }
        assert!(DbConfig { profiles: vec![p("a", "mongodb")] }.validate().unwrap_err().contains("MongoDB is not included"));
        assert!(DbConfig { profiles: vec![p("a", "oracle")] }.validate().unwrap_err().contains("unknown engine"));
        assert!(DbConfig { profiles: vec![p("a b", "sqlite")] }.validate().is_err());
        assert!(DbConfig { profiles: vec![p("a", "sqlite"), p("A", "sqlite")] }.validate().is_err());
        let mut big = p("a", "sqlite");
        big.max_rows = 5000;
        assert!(DbConfig { profiles: vec![big] }.validate().is_err());
        assert!(serde_json::from_str::<DbConfig>(r#"{"profiles": [{"alias":"a","kind":"sqlite","path":"x","password":"p"}]}"#).is_err());
    }
}
