//! Reading and writing spreadsheet files. `.xlsx` through calamine (read) and rust_xlsxwriter (write); `.csv` through
//! the csv crate.
//!
//! Neither xlsx library edits a file in place, so changing a workbook means reading all of it and writing all of it
//! again. That is faithful for values, formulas and dates. It is not faithful for formatting, charts, pictures,
//! tables, merged cells, validation or macros, so a workbook Aqua did not create is only rewritten when it has none of
//! those parts *and* the person has said rewriting is fine (`rewrite_existing`). A workbook Aqua created has nothing
//! else in it. Every rewrite keeps a dated backup and is saved by writing a temporary file and renaming it, so a
//! crash never leaves half a workbook.

use calamine::{open_workbook_auto, Data, Reader};
use rust_xlsxwriter::{DocProperties, Format, Formula, Workbook};
use std::io::Read;
use std::path::{Path, PathBuf};

pub type R<T> = Result<T, String>;

/// One cell.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Empty,
    Text(String),
    Num(f64),
    Bool(bool),
    /// A date and time as Excel stores it (days since 1899-12-30).
    Date(f64),
    /// A formula (without the `=`), and what it last calculated to.
    Formula(String, Option<Box<Val>>),
}

impl Val {
    /// The value as text, as a person would read it in a cell.
    pub fn show(&self) -> String {
        match self {
            Val::Empty => String::new(),
            Val::Text(s) => s.clone(),
            Val::Num(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    format!("{}", *n as i64)
                } else {
                    format!("{n}")
                }
            }
            Val::Bool(b) => b.to_string(),
            Val::Date(d) => excel_date_text(*d),
            Val::Formula(_, Some(v)) => v.show(),
            Val::Formula(f, None) => format!("={f}"),
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Val::Empty) || matches!(self, Val::Text(s) if s.is_empty())
    }
}

/// Days since 1899-12-30 to "YYYY-MM-DD" or "YYYY-MM-DD HH:MM".
pub fn excel_date_text(serial: f64) -> String {
    let base = chrono::NaiveDate::from_ymd_opt(1899, 12, 30).expect("valid date").and_hms_opt(0, 0, 0).expect("valid time");
    let secs = (serial * 86_400.0).round() as i64;
    match base.checked_add_signed(chrono::Duration::seconds(secs)) {
        Some(d) if d.time() == chrono::NaiveTime::MIN => d.format("%Y-%m-%d").to_string(),
        Some(d) => d.format("%Y-%m-%d %H:%M").to_string(),
        None => serial.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sheet {
    pub name: String,
    pub rows: Vec<Vec<Val>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Book {
    pub sheets: Vec<Sheet>,
    /// Aqua wrote this file (so it holds nothing that rewriting would lose).
    pub made_by_aqua: bool,
}

/// The limits on what a step will open.
pub const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
pub const MAX_ROWS: usize = 100_000;
pub const MAX_COLS: usize = 256;
/// Excel's own limit on the length of a cell.
pub const MAX_CELL_CHARS: usize = 32_767;

fn ext(path: &Path) -> String {
    path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase()
}

pub fn is_csv(path: &Path) -> bool {
    ext(path) == "csv"
}

fn size_ok(path: &Path) -> R<()> {
    let len = std::fs::metadata(path).map_err(|e| format!("the file cannot be read: {e}"))?.len();
    if len > MAX_FILE_BYTES {
        return Err(format!("the file is {} MB; a step opens at most {} MB", len / 1024 / 1024, MAX_FILE_BYTES / 1024 / 1024));
    }
    Ok(())
}

// ---- reading ------------------------------------------------------------------------------------------------------------------

fn from_data(d: &Data) -> Val {
    match d {
        Data::Empty => Val::Empty,
        Data::String(s) => Val::Text(s.clone()),
        Data::Float(f) => Val::Num(*f),
        Data::Int(i) => Val::Num(*i as f64),
        Data::Bool(b) => Val::Bool(*b),
        Data::DateTime(dt) if !dt.is_duration() => Val::Date(dt.as_f64()),
        Data::DateTime(dt) => Val::Num(dt.as_f64()),
        Data::DateTimeIso(s) | Data::DurationIso(s) => Val::Text(s.clone()),
        Data::Error(e) => Val::Text(format!("#{e:?}")),
    }
}

/// The file's package parts and whether Aqua wrote it.
fn peek(path: &Path) -> R<(Vec<String>, bool, Vec<String>)> {
    let f = std::fs::File::open(path).map_err(|e| format!("the file cannot be opened: {e}"))?;
    let mut zip = zip::ZipArchive::new(f).map_err(|_| "the file is not a valid .xlsx workbook".to_string())?;
    let names: Vec<String> = (0..zip.len()).filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string())).collect();
    let mut made_by_aqua = false;
    if let Ok(mut core) = zip.by_name("docProps/core.xml") {
        let mut s = String::new();
        let _ = core.by_ref().take(64 * 1024).read_to_string(&mut s);
        made_by_aqua = s.contains("<dc:creator>Aqua</dc:creator>");
    }
    let mut features = Vec::new();
    for i in 0..zip.len() {
        let Ok(mut file) = zip.by_index(i) else { continue };
        let name = file.name().to_string();
        if name.starts_with("xl/worksheets/") && name.ends_with(".xml") {
            let mut s = String::new();
            let _ = file.by_ref().take(8 * 1024 * 1024).read_to_string(&mut s);
            for (tag, what) in [("<mergeCell ", "merged cells"), ("<conditionalFormatting", "conditional formatting"), ("<dataValidation", "data validation")] {
                if s.contains(tag) && !features.iter().any(|w: &String| w == what) {
                    features.push(what.to_string());
                }
            }
        }
    }
    Ok((names, made_by_aqua, features))
}

/// What in a workbook a rewrite would lose. Empty means values, formulas and dates are all there is.
pub fn would_lose(path: &Path) -> R<Vec<String>> {
    let (names, _, mut features) = peek(path)?;
    for (needle, what) in [("xl/drawings/", "pictures or drawings"), ("xl/charts/", "charts"), ("xl/pivotTables/", "pivot tables"), ("xl/tables/", "tables"), ("xl/media/", "images"), ("xl/externalLinks/", "links to other workbooks"), ("vbaProject.bin", "macros")] {
        if names.iter().any(|n| n.contains(needle)) {
            features.push(what.to_string());
        }
    }
    Ok(features)
}

pub fn load(path: &Path) -> R<Book> {
    size_ok(path)?;
    if is_csv(path) {
        return load_csv(path);
    }
    if !matches!(ext(path).as_str(), "xlsx" | "xls" | "xlsm") {
        return Err(format!("only .xlsx and .csv files are supported, not .{}", ext(path)));
    }
    let made_by_aqua = if ext(path) == "xlsx" { peek(path)?.1 } else { false };
    let mut wb = open_workbook_auto(path).map_err(|e| format!("the workbook could not be read: {e}"))?;
    let mut sheets = Vec::new();
    for name in wb.sheet_names().to_vec() {
        let range = wb.worksheet_range(&name).map_err(|e| format!("the sheet \"{name}\" could not be read: {e}"))?;
        let formulas = wb.worksheet_formula(&name).ok();
        let (r0, c0) = range.start().map(|(r, c)| (r as usize, c as usize)).unwrap_or((0, 0));
        let (h, w) = (range.height() + r0, range.width() + c0);
        if h > MAX_ROWS || w > MAX_COLS {
            return Err(format!("the sheet \"{name}\" is {h} rows by {w} columns; a step opens at most {MAX_ROWS} by {MAX_COLS}"));
        }
        let mut rows = vec![vec![Val::Empty; w]; h];
        for (r, row) in range.rows().enumerate() {
            for (c, d) in row.iter().enumerate() {
                rows[r0 + r][c0 + c] = from_data(d);
            }
        }
        if let Some(f) = formulas {
            let (fr0, fc0) = f.start().map(|(r, c)| (r as usize, c as usize)).unwrap_or((0, 0));
            for (r, row) in f.rows().enumerate() {
                for (c, text) in row.iter().enumerate() {
                    if text.is_empty() {
                        continue;
                    }
                    let (rr, cc) = (fr0 + r, fc0 + c);
                    if rr < rows.len() && cc < rows[rr].len() {
                        let cached = std::mem::replace(&mut rows[rr][cc], Val::Empty);
                        rows[rr][cc] = Val::Formula(text.trim_start_matches('=').to_string(), Some(Box::new(cached)).filter(|v| **v != Val::Empty));
                    }
                }
            }
        }
        sheets.push(Sheet { name, rows });
    }
    Ok(Book { sheets, made_by_aqua })
}

fn load_csv(path: &Path) -> R<Book> {
    let mut rdr = csv::ReaderBuilder::new().has_headers(false).flexible(true).from_path(path).map_err(|e| format!("the CSV file could not be read: {e}"))?;
    let mut rows = Vec::new();
    for rec in rdr.records() {
        let rec = rec.map_err(|e| format!("the CSV file could not be read: {e}"))?;
        if rows.len() >= MAX_ROWS {
            return Err(format!("the CSV file has more than {MAX_ROWS} rows"));
        }
        rows.push(rec.iter().map(|s| if s.is_empty() { Val::Empty } else { Val::Text(s.to_string()) }).collect());
    }
    Ok(Book { sheets: vec![Sheet { name: "Sheet1".into(), rows }], made_by_aqua: true })
}

// ---- writing ---------------------------------------------------------------------------------------------------------------------

/// A CSV cell that a spreadsheet program would run as a formula is neutralised with a leading quote.
fn csv_safe(s: &str) -> String {
    if s.starts_with(['=', '+', '-', '@', '\t', '\r']) && s.parse::<f64>().is_err() {
        format!("'{s}")
    } else {
        s.to_string()
    }
}

fn write_csv(book: &Book, path: &Path) -> R<()> {
    let sheet = book.sheets.first().ok_or("there is no sheet to write")?;
    let mut w = csv::WriterBuilder::new().flexible(true).from_path(path).map_err(|e| format!("the file could not be written: {e}"))?;
    for row in &sheet.rows {
        let cells: Vec<String> = row.iter().map(|v| csv_safe(&v.show())).collect();
        w.write_record(&cells).map_err(|e| format!("the file could not be written: {e}"))?;
    }
    w.flush().map_err(|e| format!("the file could not be written: {e}"))
}

fn write_xlsx(book: &Book, path: &Path) -> R<()> {
    let mut wb = Workbook::new();
    wb.set_properties(&DocProperties::new().set_author("Aqua").set_comment("Written by Aqua").set_title("Aqua workbook"));
    let bold = Format::new().set_bold();
    let date = Format::new().set_num_format("yyyy-mm-dd hh:mm");
    let e = |x: rust_xlsxwriter::XlsxError| format!("the workbook could not be written: {x}");
    for sheet in &book.sheets {
        let ws = wb.add_worksheet();
        ws.set_name(&sheet.name).map_err(e)?;
        let cols = sheet.rows.iter().map(|r| r.len()).max().unwrap_or(0);
        for (r, row) in sheet.rows.iter().enumerate() {
            for (c, v) in row.iter().enumerate() {
                let (r, c) = (r as u32, c as u16);
                match v {
                    Val::Empty => {}
                    // Always as text: a value that starts with "=" is a string, never a formula, unless it was one.
                    Val::Text(s) if r == 0 => {
                        ws.write_string_with_format(r, c, s, &bold).map_err(e)?;
                    }
                    Val::Text(s) => {
                        ws.write_string(r, c, s).map_err(e)?;
                    }
                    Val::Num(n) => {
                        ws.write_number(r, c, *n).map_err(e)?;
                    }
                    Val::Bool(b) => {
                        ws.write_boolean(r, c, *b).map_err(e)?;
                    }
                    Val::Date(d) => {
                        ws.write_number_with_format(r, c, *d, &date).map_err(e)?;
                    }
                    Val::Formula(f, cached) => {
                        let mut formula = Formula::new(f);
                        if let Some(v) = cached {
                            formula = formula.set_result(v.show());
                        }
                        ws.write_formula(r, c, formula).map_err(e)?;
                    }
                }
            }
        }
        for c in 0..cols {
            let widest = sheet.rows.iter().filter_map(|r| r.get(c)).map(|v| v.show().chars().take(60).count()).max().unwrap_or(8);
            ws.set_column_width(c as u16, (widest as f64 + 2.0).clamp(8.0, 50.0)).map_err(e)?;
        }
        if sheet.rows.len() > 1 {
            ws.set_freeze_panes(1, 0).map_err(e)?;
        }
    }
    wb.save(path).map_err(e)
}

const KEEP_BACKUPS: usize = 5;

/// Copy the existing file aside as `name.aqua-backup-YYYYMMDD-HHMMSS.ext`, and keep only the newest few.
pub fn backup(path: &Path, at: i64) -> R<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let stamp = chrono::DateTime::from_timestamp(at, 0).map(|d| d.format("%Y%m%d-%H%M%S").to_string()).unwrap_or_else(|| at.to_string());
    let dest = path.with_file_name(format!("{stem}.aqua-backup-{stamp}.{ext}"));
    std::fs::copy(path, &dest).map_err(|e| format!("a backup could not be made, so the file was not changed: {e}"))?;
    let prefix = format!("{stem}.aqua-backup-");
    let mut old: Vec<PathBuf> = std::fs::read_dir(path.parent().unwrap_or(Path::new("."))).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(&prefix))).collect();
    old.sort();
    while old.len() > KEEP_BACKUPS {
        let _ = std::fs::remove_file(old.remove(0));
    }
    Ok(Some(dest))
}

/// Save the whole book: a temporary file beside the target, then a rename.
pub fn save(book: &Book, path: &Path) -> R<()> {
    for s in &book.sheets {
        if s.rows.len() > MAX_ROWS || s.rows.iter().any(|r| r.len() > MAX_COLS) {
            return Err("the sheet is larger than a step will write".into());
        }
        if s.rows.iter().flatten().any(|v| matches!(v, Val::Text(t) if t.chars().count() > MAX_CELL_CHARS)) {
            return Err(format!("a cell holds more than {MAX_CELL_CHARS} characters, which is more than a spreadsheet cell can hold"));
        }
    }
    let tmp = path.with_file_name(format!(".{}.aqua-tmp", path.file_name().and_then(|n| n.to_str()).unwrap_or("file")));
    let _ = std::fs::remove_file(&tmp);
    let written = if is_csv(path) { write_csv(book, &tmp) } else { write_xlsx(book, &tmp) };
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("the file could not be saved (is it open in Excel?): {e}")
    })
}
