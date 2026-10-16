//! What a spreadsheet step does to a book: read rows, add a row, change a row, write rows out. Columns are named by
//! their header; nothing is positional, and nothing invents a column silently in a sheet that already has headers.

use super::book::{Book, Sheet, Val, MAX_CELL_CHARS, MAX_COLS, MAX_ROWS};
use serde_json::{json, Map, Value};

pub type R<T> = Result<T, String>;

/// A value from a flow into a cell. A string that is plainly a number becomes a number (so it sums and sorts);
/// anything else stays text, and text never becomes a formula.
pub fn to_val(v: &Value) -> Val {
    match v {
        Value::Null => Val::Empty,
        Value::Bool(b) => Val::Bool(*b),
        Value::Number(n) => n.as_f64().map(Val::Num).unwrap_or(Val::Empty),
        Value::String(s) => {
            let t = s.trim();
            let plain_number = !t.is_empty()
                && t.len() <= 15
                && t.parse::<f64>().is_ok()
                && t.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | '-'))
                && !(t.len() > 1 && t.starts_with('0') && !t.starts_with("0."))
                && !t.starts_with("-0") || t == "0";
            if plain_number {
                Val::Num(t.parse::<f64>().unwrap_or(0.0))
            } else if s.is_empty() {
                Val::Empty
            } else {
                Val::Text(s.clone())
            }
        }
        other => Val::Text(other.to_string()),
    }
}

pub fn from_val(v: &Val) -> Value {
    match v {
        Val::Empty => Value::Null,
        Val::Text(s) => json!(s),
        Val::Num(n) => {
            if n.fract() == 0.0 && n.abs() < 1e15 {
                json!(*n as i64)
            } else {
                json!(n)
            }
        }
        Val::Bool(b) => json!(b),
        Val::Date(_) => json!(v.show()),
        Val::Formula(_, Some(c)) => from_val(c),
        Val::Formula(f, None) => json!(format!("={f}")),
    }
}

fn headers(sheet: &Sheet) -> Vec<String> {
    sheet.rows.first().map(|r| r.iter().map(|v| v.show().trim().to_string()).collect()).unwrap_or_default()
}

fn column_index(heads: &[String], name: &str) -> Option<usize> {
    let want = name.trim().to_lowercase();
    heads.iter().position(|h| !h.is_empty() && h.to_lowercase() == want)
}

fn find_sheet<'a>(book: &'a mut Book, name: &str) -> R<&'a mut Sheet> {
    let names: Vec<String> = book.sheets.iter().map(|s| s.name.clone()).collect();
    let idx = if name.is_empty() {
        0
    } else {
        names.iter().position(|n| n.eq_ignore_ascii_case(name)).ok_or_else(|| format!("invalid setting: there is no sheet called \"{name}\" (the sheets are: {})", names.join(", ")))?
    };
    book.sheets.get_mut(idx).ok_or_else(|| "the workbook has no sheets".to_string())
}

fn unknown_columns(heads: &[String], cols: &Map<String, Value>) -> R<()> {
    let missing: Vec<&String> = cols.keys().filter(|k| column_index(heads, k).is_none()).collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!("invalid setting: the sheet has no column called {} (its columns are: {}). A column is never added silently to a sheet that already has headers", missing.iter().map(|m| format!("\"{m}\"")).collect::<Vec<_>>().join(", "), heads.iter().filter(|h| !h.is_empty()).cloned().collect::<Vec<_>>().join(", ")))
}

fn set_cell(row: &mut Vec<Val>, col: usize, v: Val) -> R<()> {
    if col >= MAX_COLS {
        return Err("too many columns".into());
    }
    if row.len() <= col {
        row.resize(col + 1, Val::Empty);
    }
    if matches!(&v, Val::Text(t) if t.chars().count() > MAX_CELL_CHARS) {
        return Err(format!("a value is longer than the {MAX_CELL_CHARS} characters a spreadsheet cell can hold"));
    }
    row[col] = v;
    Ok(())
}

/// The data rows of a sheet as objects keyed by header, at most `limit`, and how many there are in all.
pub fn read(book: &mut Book, sheet: &str, limit: usize) -> R<(Vec<Map<String, Value>>, usize)> {
    let s = find_sheet(book, sheet)?;
    let heads = headers(s);
    let data: Vec<&Vec<Val>> = s.rows.iter().skip(1).filter(|r| r.iter().any(|v| !v.is_empty())).collect();
    let total = data.len();
    let rows = data
        .into_iter()
        .take(limit)
        .map(|r| {
            let mut m = Map::new();
            for (i, h) in heads.iter().enumerate() {
                if !h.is_empty() {
                    m.insert(h.clone(), r.get(i).map(from_val).unwrap_or(Value::Null));
                }
            }
            m
        })
        .collect();
    Ok((rows, total))
}

/// Add a row at the bottom. A sheet with no header row gets one, from the names given, in the order given.
pub fn append(book: &mut Book, sheet: &str, cols: &Map<String, Value>) -> R<(usize, Map<String, Value>)> {
    if cols.is_empty() {
        return Err("invalid setting: there is nothing to add: set Columns to a JSON object such as {\"Platform\": \"LinkedIn\"}".into());
    }
    let s = find_sheet(book, sheet)?;
    if s.rows.is_empty() || headers(s).iter().all(|h| h.is_empty()) {
        s.rows = vec![cols.keys().map(|k| Val::Text(k.clone())).collect()];
    }
    let heads = headers(s);
    unknown_columns(&heads, cols)?;
    if s.rows.len() >= MAX_ROWS {
        return Err(format!("the sheet already has {MAX_ROWS} rows"));
    }
    let mut row = vec![Val::Empty; heads.len()];
    let mut written = Map::new();
    for (k, v) in cols {
        let i = column_index(&heads, k).expect("checked");
        set_cell(&mut row, i, to_val(v))?;
        written.insert(heads[i].clone(), v.clone());
    }
    s.rows.push(row);
    Ok((s.rows.len(), written))
}

/// Change the one row where `key_column` holds `key`. More than one match, or none, is an error, not a guess.
pub fn update(book: &mut Book, sheet: &str, key_column: &str, key: &str, cols: &Map<String, Value>) -> R<(usize, Map<String, Value>)> {
    if key_column.trim().is_empty() {
        return Err("invalid setting: update_row needs the column to find the row by (Key column) and its value (Key)".into());
    }
    if cols.is_empty() {
        return Err("invalid setting: there is nothing to change: set Columns to a JSON object".into());
    }
    let s = find_sheet(book, sheet)?;
    let heads = headers(s);
    let kc = column_index(&heads, key_column).ok_or_else(|| format!("invalid setting: the sheet has no column called \"{key_column}\" (its columns are: {})", heads.join(", ")))?;
    unknown_columns(&heads, cols)?;
    let hits: Vec<usize> = s.rows.iter().enumerate().skip(1).filter(|(_, r)| r.get(kc).is_some_and(|v| v.show().trim() == key.trim())).map(|(i, _)| i).collect();
    match hits.len() {
        0 => Err(format!("no row has \"{key}\" in the column \"{key_column}\", so nothing was changed")),
        1 => {
            let mut written = Map::new();
            for (k, v) in cols {
                let i = column_index(&heads, k).expect("checked");
                set_cell(&mut s.rows[hits[0]], i, to_val(v))?;
                written.insert(heads[i].clone(), v.clone());
            }
            Ok((hits[0] + 1, written))
        }
        n => Err(format!("{n} rows have \"{key}\" in the column \"{key_column}\", so it is not clear which to change and nothing was changed")),
    }
}

/// A sheet made from rows: the header is every name that appears, in the order first seen.
pub fn export(name: &str, rows: &[Map<String, Value>]) -> R<Sheet> {
    if rows.is_empty() {
        return Err("invalid setting: there are no rows to write: set Rows to a JSON list of objects".into());
    }
    if rows.len() >= MAX_ROWS {
        return Err(format!("there are more than {MAX_ROWS} rows"));
    }
    let mut heads: Vec<String> = Vec::new();
    for r in rows {
        for k in r.keys() {
            if !heads.contains(k) {
                heads.push(k.clone());
            }
        }
    }
    if heads.len() > MAX_COLS {
        return Err("there are too many columns".into());
    }
    let mut out = vec![heads.iter().map(|h| Val::Text(h.clone())).collect::<Vec<_>>()];
    for r in rows {
        let mut row = vec![Val::Empty; heads.len()];
        for (i, h) in heads.iter().enumerate() {
            if let Some(v) = r.get(h) {
                set_cell(&mut row, i, to_val(v))?;
            }
        }
        out.push(row);
    }
    Ok(Sheet { name: name.to_string(), rows: out })
}
