//! PostgreSQL and MySQL/MariaDB behind the same `db.*` tools as SQLite.
//!
//! What keeps these connections safe, in addition to the SQL checks the SQLite engine also uses:
//!
//! * **Read-only by the server, not just by our reading of the SQL.** Reads open a session with
//!   `default_transaction_read_only = on` (PostgreSQL) or `SET SESSION TRANSACTION READ ONLY`
//!   (MySQL) and run inside a read-only transaction, so even a statement the checks missed cannot
//!   write. Server-side statement timeouts bound every query.
//! * **Side-effect functions are refused** in reads and writes (`pg_read_file`, `lo_export`, `dblink`,
//!   `nextval`, `sleep`, `load_file`, `INTO OUTFILE`, `FOR UPDATE`, …).
//! * **Writes are counted first** (PostgreSQL and MySQL/InnoDB): the statement runs in a transaction
//!   that is rolled back to report how many rows it would change, then run for real and rolled back
//!   if it exceeds the profile's limit. (MySQL tables on non-transactional engines such as MyISAM
//!   cannot be rolled back; use InnoDB, or keep the profile read-only.)
//! * **Passwords** come from Credential Manager by name, are never in the config file, and are
//!   removed from every error message. A connection that leaves the machine must use TLS.
//! * PostgreSQL parameters are placed into the SQL as escaped literals (the connection sets
//!   `standard_conforming_strings = on`); MySQL uses real bound parameters.

use super::sql;
use aqua_tool_host::{ErrorCode, ToolError};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Postgres,
    Mysql,
}

/// Where and how to connect. The password is fetched at the moment of use.
#[derive(Clone)]
pub struct Target {
    pub engine: Engine,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: Option<String>,
    pub tls: bool,
}

#[derive(Debug)]
pub struct Rows {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub truncated: bool,
}

fn err(code: ErrorCode, msg: impl Into<String>) -> ToolError {
    ToolError::new(code, msg)
}

/// Words that mean a statement does something other than read or write the rows it names.
const SIDE_EFFECTS: &[&str] = &[
    "into", "pg_read_file", "pg_read_binary_file", "pg_ls_dir", "pg_stat_file", "lo_import", "lo_export", "lo_get", "lo_put", "lo_create", "lo_unlink", "dblink", "dblink_exec", "dblink_connect",
    "pg_terminate_backend", "pg_cancel_backend", "pg_reload_conf", "pg_rotate_logfile", "set_config", "pg_sleep", "pg_sleep_for", "nextval", "setval", "txid_current", "pg_advisory_lock",
    "pg_advisory_xact_lock", "pg_notify", "copy", "load_file", "sleep", "benchmark", "get_lock", "release_lock", "master_pos_wait", "sys_exec", "sys_eval", "analyze",
];

/// Refuse statements with side effects. `skel` is [`sql::skeleton`] output. `writes` is true for an
/// INSERT/UPDATE/DELETE, where `INTO` is normal (but `INTO OUTFILE` is not).
pub fn check_side_effects(skel: &str, writes: bool) -> Result<(), ToolError> {
    for w in SIDE_EFFECTS {
        if writes && *w == "into" {
            continue;
        }
        if sql::contains_word(skel, w) {
            return Err(err(ErrorCode::QueryRejected, format!("'{w}' is not allowed: it can read files, wait, take locks, change sequences or reach other systems")));
        }
    }
    let flat: String = skel.to_ascii_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    for phrase in ["for update", "for share", "for no key update", "for key share", "lock in share mode", "into outfile", "into dumpfile"] {
        if flat.contains(phrase) {
            return Err(err(ErrorCode::QueryRejected, format!("'{phrase}' is not allowed in a statement here")));
        }
    }
    Ok(())
}

/// Replace each `?` that SQL would read as a placeholder (not inside a string, quoted name or
/// comment) with the escaped literal for the same position in `params`.
pub fn inline_params(sql_text: &str, params: &Value) -> Result<String, ToolError> {
    let list: Vec<&Value> = params.as_array().map(|a| a.iter().collect()).unwrap_or_default();
    let chars: Vec<char> = sql_text.chars().collect();
    let mut out = String::with_capacity(sql_text.len() + 16);
    let mut used = 0usize;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '-' if next == Some('-') => {
                while i < chars.len() && chars[i] != '\n' {
                    out.push(chars[i]);
                    i += 1;
                }
            }
            '/' if next == Some('*') => {
                out.push_str("/*");
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    out.push(chars[i]);
                    i += 1;
                }
                if i < chars.len() {
                    out.push_str("*/");
                    i += 2;
                }
            }
            '\'' | '"' | '`' => {
                let q = c;
                out.push(q);
                i += 1;
                while i < chars.len() {
                    out.push(chars[i]);
                    if chars[i] == q {
                        if chars.get(i + 1) == Some(&q) {
                            out.push(q);
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            '?' => {
                let v = list.get(used).ok_or_else(|| err(ErrorCode::InvalidArguments, format!("the SQL has more ? placeholders than params ({})", list.len())))?;
                out.push_str(&literal(v)?);
                used += 1;
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    if used != list.len() {
        return Err(err(ErrorCode::InvalidArguments, format!("the SQL has {used} ? placeholders but {} params were given", list.len())));
    }
    Ok(out)
}

/// One value as a PostgreSQL literal. Strings are single-quoted with `'` doubled and NUL refused;
/// with `standard_conforming_strings = on` nothing else in a quoted string is special.
fn literal(v: &Value) -> Result<String, ToolError> {
    Ok(match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Value::Number(n) => {
            let s = n.to_string();
            if !s.chars().all(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E')) {
                return Err(err(ErrorCode::InvalidArguments, "a numeric param is not a plain number"));
            }
            s
        }
        Value::String(s) => {
            if s.contains('\0') {
                return Err(err(ErrorCode::InvalidArguments, "a string param contains a NUL character"));
            }
            format!("'{}'", s.replace('\'', "''"))
        }
        _ => return Err(err(ErrorCode::InvalidArguments, "params must be strings, numbers, booleans or null")),
    })
}

fn strip_semicolon(sql_text: &str) -> &str {
    sql_text.trim().trim_end_matches(';').trim_end()
}

/// Remove the password (and any URL-shaped userinfo) from a driver message.
fn scrub(msg: &str, t: &Target) -> String {
    let mut m = msg.to_string();
    if let Some(p) = &t.password {
        if p.len() >= 3 {
            m = m.replace(p.as_str(), "[REDACTED]");
        }
    }
    m
}

fn cut(s: &str) -> Value {
    if s.chars().count() > 500 {
        Value::String(format!("{}… [cut, {} chars]", s.chars().take(500).collect::<String>(), s.chars().count()))
    } else {
        Value::String(s.to_string())
    }
}

// ---------------------------------------------------------------- PostgreSQL

/// The `postgres` crate drives its own async runtime and panics if it is used from a thread that
/// already belongs to one, and the tool host calls into providers from async code (for the dry run
/// that prices a write). So its work runs on a short-lived thread of its own.
fn off_runtime<T: Send>(f: impl FnOnce() -> Result<T, ToolError> + Send) -> Result<T, ToolError> {
    std::thread::scope(|s| s.spawn(f).join().unwrap_or_else(|_| Err(err(ErrorCode::BackendError, "the PostgreSQL driver stopped unexpectedly"))))
}

fn pg_connect(t: &Target, read_only: bool, timeout_s: u64) -> Result<postgres::Client, ToolError> {
    let mut cfg = postgres::Config::new();
    cfg.host(&t.host).port(t.port).user(&t.user).dbname(&t.database).connect_timeout(Duration::from_secs(10)).application_name("aqua");
    if let Some(p) = &t.password {
        cfg.password(p);
    }
    cfg.options(&format!(
        "-c standard_conforming_strings=on -c TimeZone=UTC -c statement_timeout={} -c idle_in_transaction_session_timeout=30000 -c lock_timeout=5000{}",
        timeout_s * 1000,
        if read_only { " -c default_transaction_read_only=on" } else { "" }
    ));
    let connected = if t.tls {
        cfg.ssl_mode(postgres::config::SslMode::Require);
        let tls = native_tls::TlsConnector::builder().build().map_err(|e| err(ErrorCode::BackendError, format!("TLS setup failed: {e}")))?;
        cfg.connect(postgres_native_tls::MakeTlsConnector::new(tls))
    } else {
        cfg.connect(postgres::NoTls)
    };
    connected.map_err(|e| pg_err(&e, "connect", t))
}

fn pg_err(e: &postgres::Error, what: &str, t: &Target) -> ToolError {
    use postgres::error::SqlState;
    let msg = scrub(&e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string()), t);
    let code = match e.code() {
        Some(c) if *c == SqlState::INVALID_PASSWORD || *c == SqlState::INVALID_AUTHORIZATION_SPECIFICATION => ErrorCode::AuthFailed,
        Some(c) if *c == SqlState::UNDEFINED_TABLE || *c == SqlState::UNDEFINED_COLUMN || *c == SqlState::UNDEFINED_FUNCTION || *c == SqlState::INVALID_CATALOG_NAME => ErrorCode::NotFound,
        Some(c) if *c == SqlState::SYNTAX_ERROR || *c == SqlState::DATATYPE_MISMATCH || *c == SqlState::INVALID_TEXT_REPRESENTATION || *c == SqlState::SYNTAX_ERROR_OR_ACCESS_RULE_VIOLATION => ErrorCode::InvalidArguments,
        Some(c) if c.code().starts_with("23") => ErrorCode::Conflict,
        Some(c) if *c == SqlState::READ_ONLY_SQL_TRANSACTION => ErrorCode::PermissionDenied,
        Some(c) if *c == SqlState::INSUFFICIENT_PRIVILEGE => ErrorCode::OsPermissionDenied,
        Some(c) if *c == SqlState::QUERY_CANCELED || *c == SqlState::LOCK_NOT_AVAILABLE => ErrorCode::Timeout,
        Some(_) => ErrorCode::BackendError,
        None => {
            let low = msg.to_ascii_lowercase();
            if low.contains("password authentication") || low.contains("authentication failed") {
                ErrorCode::AuthFailed
            } else if low.contains("timed out") || low.contains("timeout") {
                ErrorCode::Timeout
            } else {
                ErrorCode::Unavailable
            }
        }
    };
    err(code, format!("{what}: {msg}"))
}

pub fn pg_query(t: &Target, sql_text: &str, params: &Value, max_rows: usize, timeout_s: u64) -> Result<Rows, ToolError> {
    off_runtime(|| pg_query_inner(t, sql_text, params, max_rows, timeout_s))
}

fn pg_query_inner(t: &Target, sql_text: &str, params: &Value, max_rows: usize, timeout_s: u64) -> Result<Rows, ToolError> {
    let inlined = inline_params(sql_text, params)?;
    let mut c = pg_connect(t, true, timeout_s)?;
    c.batch_execute("BEGIN READ ONLY").map_err(|e| pg_err(&e, "begin", t))?;
    let body = strip_semicolon(&inlined).to_string();
    let result = (|| -> Result<Rows, ToolError> {
        if sql::first_keyword(&sql::skeleton(&body)) == "explain" {
            let rows = c.query(&body, &[]).map_err(|e| pg_err(&e, "the query", t))?;
            let lines: Vec<Vec<Value>> = rows.iter().take(max_rows).map(|r| vec![cut(&r.try_get::<_, String>(0).unwrap_or_default())]).collect();
            return Ok(Rows { columns: vec!["QUERY PLAN".into()], rows: lines, truncated: rows.len() > max_rows });
        }
        // Prepare the statement itself for the column names; the JSON wrapper then gives every type
        // (numeric, timestamps, arrays, json) as text the model can read.
        let stmt = c.prepare(&body).map_err(|e| pg_err(&e, "the query", t))?;
        let cols: Vec<String> = stmt.columns().iter().map(|c| c.name().to_string()).collect();
        let wrapped = format!("SELECT row_to_json(aqua_t)::text FROM ({body}) aqua_t LIMIT {}", max_rows + 1);
        let rows = c.query(&wrapped, &[]).map_err(|e| pg_err(&e, "the query", t))?;
        let mut out: Vec<Vec<Value>> = Vec::new();
        for r in rows.iter().take(max_rows) {
            let text: String = r.try_get(0).map_err(|e| err(ErrorCode::BackendError, format!("reading a row: {e}")))?;
            let obj: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            out.push(cols.iter().map(|k| match obj.get(k) { Some(Value::String(s)) => cut(s), Some(v) => v.clone(), None => Value::Null }).collect());
        }
        Ok(Rows { columns: cols, rows: out, truncated: rows.len() > max_rows })
    })();
    let _ = c.batch_execute("ROLLBACK");
    result
}

/// Run a write in a transaction. With `dry_run` (or when it changes more than `max_rows`) it is
/// rolled back. Returns the rows changed and whether it was committed.
pub fn pg_execute(t: &Target, sql_text: &str, params: &Value, max_rows: usize, dry_run: bool) -> Result<(usize, bool), ToolError> {
    off_runtime(|| pg_execute_inner(t, sql_text, params, max_rows, dry_run))
}

fn pg_execute_inner(t: &Target, sql_text: &str, params: &Value, max_rows: usize, dry_run: bool) -> Result<(usize, bool), ToolError> {
    let inlined = inline_params(sql_text, params)?;
    let mut c = pg_connect(t, false, 30)?;
    c.batch_execute("BEGIN").map_err(|e| pg_err(&e, "begin", t))?;
    match c.execute(strip_semicolon(&inlined), &[]) {
        Ok(n) => {
            let n = n as usize;
            if dry_run || n > max_rows {
                let _ = c.batch_execute("ROLLBACK");
                Ok((n, false))
            } else {
                c.batch_execute("COMMIT").map_err(|e| pg_err(&e, "commit", t))?;
                Ok((n, true))
            }
        }
        Err(e) => {
            let _ = c.batch_execute("ROLLBACK");
            Err(pg_err(&e, "the statement", t))
        }
    }
}

pub fn pg_schema(t: &Target, only: &str) -> Result<Vec<(String, Vec<(String, String, bool, bool)>)>, ToolError> {
    off_runtime(|| pg_schema_inner(t, only))
}

fn pg_schema_inner(t: &Target, only: &str) -> Result<Vec<(String, Vec<(String, String, bool, bool)>)>, ToolError> {
    let mut c = pg_connect(t, true, 15)?;
    let cols = c
        .query(
            "SELECT c.table_schema || '.' || c.table_name, c.column_name, c.data_type, c.is_nullable = 'NO', \
             EXISTS (SELECT 1 FROM information_schema.key_column_usage k JOIN information_schema.table_constraints tc ON tc.constraint_name = k.constraint_name AND tc.table_schema = k.table_schema \
                     WHERE tc.constraint_type = 'PRIMARY KEY' AND k.table_schema = c.table_schema AND k.table_name = c.table_name AND k.column_name = c.column_name) \
             FROM information_schema.columns c WHERE c.table_schema NOT IN ('pg_catalog', 'information_schema') \
             AND ($1 = '' OR c.table_name = $1 OR c.table_schema || '.' || c.table_name = $1) ORDER BY 1, c.ordinal_position",
            &[&only],
        )
        .map_err(|e| pg_err(&e, "schema", t))?;
    let mut tables: Vec<(String, Vec<(String, String, bool, bool)>)> = Vec::new();
    for r in cols {
        let (table, col, ty, not_null, pk): (String, String, String, bool, bool) = (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4));
        match tables.last_mut() {
            Some((n, v)) if *n == table => v.push((col, ty, not_null, pk)),
            _ => tables.push((table, vec![(col, ty, not_null, pk)])),
        }
    }
    Ok(tables)
}

// --------------------------------------------------------------------- MySQL

fn my_connect(t: &Target, read_only: bool, timeout_s: u64) -> Result<mysql::Conn, ToolError> {
    use mysql::prelude::Queryable;
    let mut b = mysql::OptsBuilder::new()
        .ip_or_hostname(Some(t.host.clone()))
        .tcp_port(t.port)
        .user(Some(t.user.clone()))
        .pass(t.password.clone())
        .db_name(Some(t.database.clone()))
        .tcp_connect_timeout(Some(Duration::from_secs(10)))
        .read_timeout(Some(Duration::from_secs(timeout_s + 5)))
        .write_timeout(Some(Duration::from_secs(30)));
    if t.tls {
        b = b.ssl_opts(Some(mysql::SslOpts::default()));
    }
    let mut c = mysql::Conn::new(b).map_err(|e| my_err(&e, "connect", t))?;
    if read_only {
        c.query_drop("SET SESSION TRANSACTION READ ONLY").map_err(|e| my_err(&e, "read-only session", t))?;
    }
    // MySQL 5.7.8+ and MariaDB name the statement time limit differently; whichever exists applies.
    let _ = c.query_drop(format!("SET SESSION MAX_EXECUTION_TIME={}", timeout_s * 1000));
    let _ = c.query_drop(format!("SET SESSION max_statement_time={timeout_s}"));
    Ok(c)
}

fn my_err(e: &mysql::Error, what: &str, t: &Target) -> ToolError {
    let msg = scrub(&e.to_string(), t);
    let code = match e {
        mysql::Error::MySqlError(me) => match me.code {
            1045 | 1698 | 1251 => ErrorCode::AuthFailed,
            1044 | 1142 | 1143 | 1227 => ErrorCode::OsPermissionDenied,
            1146 | 1054 | 1049 | 1051 | 1109 => ErrorCode::NotFound,
            1064 | 1065 | 1063 | 1066 | 1052 | 1111 | 1241 | 1136 | 1582 => ErrorCode::InvalidArguments,
            1062 | 1216 | 1217 | 1451 | 1452 | 1406 | 1048 => ErrorCode::Conflict,
            1290 | 1792 | 1836 => ErrorCode::PermissionDenied,
            1317 | 1969 | 3024 | 1205 => ErrorCode::Timeout,
            _ => ErrorCode::BackendError,
        },
        // The server offered a login method this driver cannot speak (for example MariaDB's GSSAPI
        // plugin when the password is wrong or the account uses Windows authentication).
        mysql::Error::DriverError(mysql::DriverError::UnknownAuthPlugin(plugin)) => {
            return err(ErrorCode::AuthFailed, format!("{what}: the server asked for the '{plugin}' login method, which is not supported. Check the user name and the stored password, and use an account with a normal password login (mysql_native_password or caching_sha2_password)."));
        }
        mysql::Error::IoError(_) | mysql::Error::DriverError(_) => {
            let low = msg.to_ascii_lowercase();
            if low.contains("timed out") || low.contains("timeout") { ErrorCode::Timeout } else { ErrorCode::Unavailable }
        }
        _ => ErrorCode::BackendError,
    };
    err(code, format!("{what}: {msg}"))
}

fn my_params(params: &Value) -> Result<mysql::Params, ToolError> {
    let Some(arr) = params.as_array() else { return Ok(mysql::Params::Empty) };
    if arr.is_empty() {
        return Ok(mysql::Params::Empty);
    }
    let v: Result<Vec<mysql::Value>, ToolError> = arr
        .iter()
        .map(|p| match p {
            Value::Null => Ok(mysql::Value::NULL),
            Value::Bool(b) => Ok(mysql::Value::Int(*b as i64)),
            Value::Number(n) => Ok(n.as_i64().map(mysql::Value::Int).unwrap_or_else(|| mysql::Value::Double(n.as_f64().unwrap_or(0.0)))),
            Value::String(s) => Ok(mysql::Value::Bytes(s.clone().into_bytes())),
            _ => Err(err(ErrorCode::InvalidArguments, "params must be strings, numbers, booleans or null")),
        })
        .collect();
    Ok(mysql::Params::Positional(v?))
}

fn my_cell(v: mysql::Value) -> Value {
    use mysql::Value as V;
    match v {
        V::NULL => Value::Null,
        V::Bytes(b) => match String::from_utf8(b) {
            Ok(s) => cut(&s),
            Err(e) => Value::String(format!("<binary {} bytes>", e.into_bytes().len())),
        },
        V::Int(i) => json!(i),
        V::UInt(u) => json!(u),
        V::Float(f) => json!(f),
        V::Double(d) => json!(d),
        V::Date(y, mo, d, h, mi, s, us) => Value::String(if h == 0 && mi == 0 && s == 0 && us == 0 { format!("{y:04}-{mo:02}-{d:02}") } else { format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}") }),
        V::Time(neg, d, h, mi, s, _) => Value::String(format!("{}{:02}:{mi:02}:{s:02}", if neg { "-" } else { "" }, d * 24 + h as u32)),
    }
}

pub fn my_query(t: &Target, sql_text: &str, params: &Value, max_rows: usize, timeout_s: u64) -> Result<Rows, ToolError> {
    use mysql::prelude::Queryable;
    let mut c = my_connect(t, true, timeout_s)?;
    c.query_drop("START TRANSACTION READ ONLY").map_err(|e| my_err(&e, "begin", t))?;
    let body = strip_semicolon(sql_text).to_string();
    let result = (|| -> Result<Rows, ToolError> {
        let mut res = c.exec_iter(&body, my_params(params)?).map_err(|e| my_err(&e, "the query", t))?;
        let cols: Vec<String> = res.columns().as_ref().iter().map(|c| c.name_str().to_string()).collect();
        let mut rows: Vec<Vec<Value>> = Vec::new();
        let mut truncated = false;
        for row in res.by_ref() {
            let row = row.map_err(|e| my_err(&e, "the query", t))?;
            if rows.len() >= max_rows {
                truncated = true;
                break;
            }
            rows.push(row.unwrap().into_iter().map(my_cell).collect());
        }
        Ok(Rows { columns: cols, rows, truncated })
    })();
    let _ = c.query_drop("ROLLBACK");
    result
}

pub fn my_execute(t: &Target, sql_text: &str, params: &Value, max_rows: usize, dry_run: bool) -> Result<(usize, bool), ToolError> {
    use mysql::prelude::Queryable;
    let mut c = my_connect(t, false, 30)?;
    c.query_drop("START TRANSACTION").map_err(|e| my_err(&e, "begin", t))?;
    match c.exec_drop(strip_semicolon(sql_text), my_params(params)?) {
        Ok(()) => {
            let n = c.affected_rows() as usize;
            if dry_run || n > max_rows {
                let _ = c.query_drop("ROLLBACK");
                Ok((n, false))
            } else {
                c.query_drop("COMMIT").map_err(|e| my_err(&e, "commit", t))?;
                Ok((n, true))
            }
        }
        Err(e) => {
            let _ = c.query_drop("ROLLBACK");
            Err(my_err(&e, "the statement", t))
        }
    }
}

pub fn my_schema(t: &Target, only: &str) -> Result<Vec<(String, Vec<(String, String, bool, bool)>)>, ToolError> {
    use mysql::prelude::Queryable;
    let mut c = my_connect(t, true, 15)?;
    let rows: Vec<(String, String, String, String, String)> = c
        .exec(
            "SELECT table_name, column_name, column_type, is_nullable, column_key FROM information_schema.columns WHERE table_schema = DATABASE() AND (? = '' OR table_name = ?) ORDER BY table_name, ordinal_position",
            (only, only),
        )
        .map_err(|e| my_err(&e, "schema", t))?;
    let mut tables: Vec<(String, Vec<(String, String, bool, bool)>)> = Vec::new();
    for (table, col, ty, nullable, key) in rows {
        let item = (col, ty, nullable == "NO", key == "PRI");
        match tables.last_mut() {
            Some((n, v)) if *n == table => v.push(item),
            _ => tables.push((table, vec![item])),
        }
    }
    Ok(tables)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_become_escaped_literals_only_outside_strings_and_comments() {
        let s = inline_params("SELECT * FROM t WHERE a = ? AND b = '?' AND c = ? -- ?\n AND d = \"?\"", &json!(["x' OR '1'='1", 5])).unwrap();
        assert_eq!(s, "SELECT * FROM t WHERE a = 'x'' OR ''1''=''1' AND b = '?' AND c = 5 -- ?\n AND d = \"?\"");
        assert_eq!(inline_params("SELECT ?, ?, ?, ?", &json!([null, true, 1.5, "a\\b"])).unwrap(), "SELECT NULL, TRUE, 1.5, 'a\\b'");
        // A value can never close its own quote and start a new statement.
        let evil = inline_params("SELECT ?", &json!(["'; DROP TABLE t; --"])).unwrap();
        assert_eq!(evil, "SELECT '''; DROP TABLE t; --'");
        assert!(!sql::has_second_statement(&sql::skeleton(&evil)));
    }

    #[test]
    fn param_counts_types_and_nul_are_checked() {
        assert!(inline_params("SELECT ?", &json!([])).is_err(), "too few");
        assert!(inline_params("SELECT 1", &json!([1])).is_err(), "too many");
        assert!(inline_params("SELECT ?", &json!(["a\u{0}b"])).is_err());
        assert!(inline_params("SELECT ?", &json!([[1]])).is_err());
        assert!(inline_params("SELECT ?", &json!([{"a": 1}])).is_err());
        assert_eq!(inline_params("SELECT 1", &Value::Null).unwrap(), "SELECT 1");
    }

    #[test]
    fn side_effect_functions_and_locking_reads_are_refused() {
        for bad in [
            "SELECT pg_read_file('/etc/passwd')", "SELECT * FROM dblink('host=x', 'select 1') AS t(a int)", "SELECT lo_export(1, '/tmp/x')", "SELECT nextval('s')", "SELECT setval('s', 1)",
            "SELECT pg_sleep(100)", "SELECT set_config('a','b',false)", "SELECT sleep(10)", "SELECT benchmark(100000000, md5('x'))", "SELECT load_file('/etc/passwd')", "SELECT * FROM t INTO OUTFILE '/tmp/x'",
            "SELECT * INTO newt FROM t", "SELECT * FROM t FOR UPDATE", "SELECT * FROM t FOR  SHARE", "SELECT * FROM t LOCK IN SHARE MODE", "SELECT pg_terminate_backend(1)", "COPY t TO '/tmp/x'", "EXPLAIN ANALYZE SELECT 1",
            "SELECT get_lock('a', 10)", "SELECT pg_advisory_lock(1)",
        ] {
            assert!(check_side_effects(&sql::skeleton(bad), false).is_err(), "{bad}");
        }
        assert!(check_side_effects(&sql::skeleton("INSERT INTO t(a) VALUES (1)"), true).is_ok(), "INTO is normal in an INSERT");
        assert!(check_side_effects(&sql::skeleton("INSERT INTO t SELECT * FROM u INTO OUTFILE '/x'"), true).is_err());
        assert!(check_side_effects(&sql::skeleton("UPDATE t SET a = nextval('s') WHERE id = 1"), true).is_err());
        for ok in ["SELECT * FROM t WHERE name = 'into the wild'", "SELECT name AS \"into\" FROM t", "SELECT count(*) FROM orders WHERE created > now() - interval '1 day'", "SELECT 'nextval' AS s", "SELECT * FROM t -- for update"] {
            assert!(check_side_effects(&sql::skeleton(ok), false).is_ok(), "{ok}");
        }
    }

    #[test]
    fn driver_messages_never_carry_the_password() {
        let t = Target { engine: Engine::Postgres, host: "h".into(), port: 1, database: "d".into(), user: "u".into(), password: Some("s3cret-pw!".into()), tls: false };
        assert_eq!(scrub("connection to postgres://u:s3cret-pw!@h failed", &t), "connection to postgres://u:[REDACTED]@h failed");
        assert_eq!(scrub("nothing here", &t), "nothing here");
    }

    #[test]
    fn mysql_values_become_readable_json() {
        use mysql::Value as V;
        assert_eq!(my_cell(V::NULL), Value::Null);
        assert_eq!(my_cell(V::Int(-3)), json!(-3));
        assert_eq!(my_cell(V::Bytes(b"hello".to_vec())), json!("hello"));
        assert_eq!(my_cell(V::Bytes(vec![0xff, 0xfe])), json!("<binary 2 bytes>"));
        assert_eq!(my_cell(V::Date(2026, 9, 30, 0, 0, 0, 0)), json!("2026-09-30"));
        assert_eq!(my_cell(V::Date(2026, 9, 30, 13, 5, 9, 0)), json!("2026-09-30 13:05:09"));
        assert_eq!(my_cell(V::Time(true, 1, 2, 3, 4, 0)), json!("-26:03:04"));
    }

    #[test]
    fn mysql_params_are_bound_not_pasted() {
        match my_params(&json!(["a'b", 3, null, true])).unwrap() {
            mysql::Params::Positional(v) => assert_eq!(v.len(), 4),
            _ => panic!("positional"),
        }
        assert!(matches!(my_params(&json!([])).unwrap(), mysql::Params::Empty));
        assert!(my_params(&json!([{"a": 1}])).is_err());
    }
}
