//! The "Spreadsheet" step. Files are reached through the same path guard as the file tools: only the folders Aqua may
//! use, never Aqua's own files, never a file that looks like it holds secrets, and writes only where a folder is
//! trusted for writing without a prompt (a flow cannot answer one). Changing a workbook keeps a dated backup and is
//! serialised per file, so two steps cannot overwrite each other.

use crate::flows::model::NodeKind;
use crate::flows::run::exec::{NodeCtx, NodeError, NodeExecutor, NodeResult};
use crate::flows::sheet::{book, ops};
use aqua_native_tools::fs::guard::{Op, PathGuard};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct SheetNodeExecutor {
    guard: Arc<PathGuard>,
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl SheetNodeExecutor {
    pub fn new(guard: Arc<PathGuard>) -> Arc<Self> {
        Arc::new(Self { guard, locks: Mutex::new(HashMap::new()) })
    }

    fn lock_for(&self, path: &Path) -> Arc<Mutex<()>> {
        self.locks.lock().unwrap_or_else(std::sync::PoisonError::into_inner).entry(path.to_path_buf()).or_default().clone()
    }

    fn resolve(&self, path: &str, write: bool) -> Result<PathBuf, NodeError> {
        let r = self.guard.resolve(path, if write { Op::Write } else { Op::Read }).map_err(|e| NodeError::Fatal(format!("not allowed: {}", e.message)))?;
        if r.sensitive {
            return Err(NodeError::Fatal("not allowed: the file name looks like it holds secrets, so a flow will not open it".into()));
        }
        if write && r.executable_kind {
            return Err(NodeError::Fatal("not allowed: a flow will not write a file that can run code".into()));
        }
        if write && !r.trusted {
            return Err(NodeError::Fatal(format!("not allowed: the folder @{} is not set to allow writing without asking, and a flow cannot be asked. Use a trusted folder (the default workspace is)", r.root_alias)));
        }
        if !matches!(r.path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref(), Some("xlsx" | "csv")) && (write || !matches!(r.path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref(), Some("xls" | "xlsm"))) {
            return Err(NodeError::Fatal(format!("invalid setting: the file must be .xlsx or .csv{}", if write { "" } else { " (.xls and .xlsm can be read)" })));
        }
        Ok(r.path)
    }
}

fn text(cfg: &Value, key: &str) -> String {
    cfg.get(key).and_then(|v| v.as_str()).unwrap_or("").trim().to_string()
}

fn object(cfg: &Value, key: &str) -> Result<Map<String, Value>, NodeError> {
    match cfg.get(key) {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(m)) => Ok(m.clone()),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(Map::new()),
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Object(m)) => Ok(m),
            _ => Err(NodeError::Fatal(format!("invalid setting: {key} must be a JSON object"))),
        },
        _ => Err(NodeError::Fatal(format!("invalid setting: {key} must be a JSON object"))),
    }
}

fn rows_of(cfg: &Value) -> Result<Vec<Map<String, Value>>, NodeError> {
    let v = match cfg.get("rows") {
        Some(Value::String(s)) if !s.trim().is_empty() => serde_json::from_str::<Value>(s).map_err(|_| NodeError::Fatal("invalid setting: rows must be a JSON list of objects".into()))?,
        Some(v) => v.clone(),
        None => Value::Null,
    };
    match v {
        Value::Array(a) => a.into_iter().map(|x| x.as_object().cloned().ok_or_else(|| NodeError::Fatal("invalid setting: every row must be a JSON object".into()))).collect(),
        Value::Null => Ok(vec![]),
        _ => Err(NodeError::Fatal("invalid setting: rows must be a JSON list of objects".into())),
    }
}

fn flag(cfg: &Value, key: &str) -> bool {
    match cfg.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.trim().to_lowercase().as_str(), "true" | "yes" | "1"),
        _ => false,
    }
}

fn fatal(e: String) -> NodeError {
    if e.contains("is it open in Excel") {
        NodeError::Retryable(e)
    } else if e.starts_with("invalid setting") || e.starts_with("no row") || e.contains("nothing was changed") {
        NodeError::Fatal(e)
    } else {
        NodeError::Fatal(e)
    }
}

impl NodeExecutor for SheetNodeExecutor {
    fn kinds(&self) -> &[NodeKind] {
        &[NodeKind::Excel]
    }

    fn execute(&self, ctx: &NodeCtx) -> Result<NodeResult, NodeError> {
        let cfg = &ctx.config;
        let op = match text(cfg, "operation").as_str() {
            "" => "append_row".to_string(),
            o => o.to_string(),
        };
        if !matches!(op.as_str(), "read_rows" | "append_row" | "update_row" | "export") {
            return Err(NodeError::Fatal(format!("invalid setting: the spreadsheet operation \"{op}\" is not supported (read_rows, append_row, update_row, export)")));
        }
        let write = op != "read_rows";
        let raw_path = text(cfg, "path");
        if raw_path.is_empty() {
            return Err(NodeError::Fatal("invalid setting: the file is empty".into()));
        }
        let path = self.resolve(&raw_path, write)?;
        let sheet = text(cfg, "sheet");
        let lock = self.lock_for(&path);
        let _held = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut logs = Vec::new();
        let mut out = Map::new();
        out.insert("file".into(), json!(path.to_string_lossy()));

        if !write {
            if !path.exists() {
                return Err(NodeError::Fatal(format!("no such file: {}", path.display())));
            }
            let mut b = book::load(&path).map_err(fatal)?;
            let limit = cfg.get("limit").and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))).unwrap_or(500).clamp(1, 5000) as usize;
            let (rows, total) = ops::read(&mut b, &sheet, limit).map_err(fatal)?;
            logs.push(format!("Read {} of {total} rows from {}", rows.len(), path.file_name().and_then(|n| n.to_str()).unwrap_or("the file")));
            out.insert("rows".into(), Value::Array(rows.into_iter().map(Value::Object).collect()));
            out.insert("row_count".into(), json!(total));
            return Ok(NodeResult { outputs: out, logs });
        }

        // ---- changing a file ------------------------------------------------------------------------------------------------------
        let exists = path.exists();
        let xlsx = !book::is_csv(&path);
        let mut b = if exists && op != "export" {
            book::load(&path).map_err(fatal)?
        } else {
            book::Book { sheets: vec![book::Sheet { name: if sheet.is_empty() { "Sheet1".into() } else { sheet.clone() }, rows: vec![] }], made_by_aqua: true }
        };
        if exists && xlsx {
            // A workbook someone else made would lose its formatting when rewritten; say so and let the person decide.
            let existing = book::load(&path).map_err(fatal)?;
            if !existing.made_by_aqua {
                let lost = book::would_lose(&path).map_err(fatal)?;
                if !lost.is_empty() {
                    return Err(NodeError::Fatal(format!("not allowed: this workbook has {}, which saving it again would lose, so Aqua will not change it. Use a workbook Aqua made, or a .csv file", lost.join(", "))));
                }
                if !flag(cfg, "rewrite_existing") {
                    return Err(NodeError::Fatal("not allowed: this workbook was not made by Aqua, and saving it again keeps values, formulas and dates but not its formatting (colours, fonts, column widths). If that is fine, switch on \"Allow rewriting\" (a dated backup is kept)".into()));
                }
            }
        }
        let (row_no, written) = match op.as_str() {
            "append_row" => {
                let cols = object(cfg, "columns")?;
                if !b.sheets.iter().any(|s| sheet.is_empty() || s.name.eq_ignore_ascii_case(&sheet)) {
                    b.sheets.push(book::Sheet { name: sheet.clone(), rows: vec![] });
                }
                let (n, w) = ops::append(&mut b, &sheet, &cols).map_err(fatal)?;
                logs.push(format!("Added a row (row {n})"));
                (n, vec![w])
            }
            "update_row" => {
                let cols = object(cfg, "columns")?;
                let (n, w) = ops::update(&mut b, &sheet, &text(cfg, "key_column"), &text(cfg, "key"), &cols).map_err(fatal)?;
                logs.push(format!("Changed row {n}"));
                (n, vec![w])
            }
            _ => {
                let rows = rows_of(cfg)?;
                let s = ops::export(if sheet.is_empty() { "Sheet1" } else { &sheet }, &rows).map_err(fatal)?;
                if exists && !flag(cfg, "rewrite_existing") {
                    return Err(NodeError::Fatal("not allowed: the file already exists, and export replaces it. Switch on \"Allow rewriting\" to replace it (a dated backup is kept)".into()));
                }
                let n = s.rows.len();
                b = book::Book { sheets: vec![s], made_by_aqua: true };
                logs.push(format!("Wrote {} rows", n - 1));
                (n, rows)
            }
        };
        if exists {
            if let Some(bak) = book::backup(&path, chrono::Utc::now().timestamp()).map_err(fatal)? {
                logs.push(format!("Kept a backup: {}", bak.file_name().and_then(|n| n.to_str()).unwrap_or("")));
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| NodeError::Fatal(format!("could not create the folder: {e}")))?;
        }
        book::save(&b, &path).map_err(fatal)?;
        let total = b.sheets.iter().find(|s| sheet.is_empty() || s.name.eq_ignore_ascii_case(&sheet)).or(b.sheets.first()).map(|s| s.rows.iter().skip(1).filter(|r| r.iter().any(|v| !v.is_empty())).count()).unwrap_or(0);
        out.insert("rows".into(), Value::Array(written.into_iter().map(Value::Object).collect()));
        out.insert("row_count".into(), json!(total));
        let _ = row_no;
        Ok(NodeResult { outputs: out, logs })
    }
}
