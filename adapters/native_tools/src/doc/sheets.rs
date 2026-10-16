//! Excel and OpenDocument workbooks (through `calamine`), and CSV or TSV files: rows as lines, cells separated by a tab.

use super::{Archive, Document, Unit};
use aqua_tool_host::{ErrorCode, ToolError};
use calamine::{Data, Reader};

const MAX_SHEETS: usize = 50;
const MAX_ROWS: usize = 5_000;
const ROWS_PER_UNIT: usize = 200;

fn cell(d: &Data) -> String {
    match d {
        Data::Empty => String::new(),
        Data::String(s) => s.replace(['\t', '\r', '\n'], " "),
        other => other.to_string().replace(['\t', '\r', '\n'], " "),
    }
}

fn unit_rows(label: &str, rows: &[String], units: &mut Vec<Unit>) {
    for (k, chunk) in rows.chunks(ROWS_PER_UNIT).enumerate() {
        let from = k * ROWS_PER_UNIT + 1;
        units.push(Unit { label: format!("{label} rows {from}-{}", from + chunk.len() - 1), text: chunk.join("\n") });
    }
}

pub fn extract_workbook(bytes: Vec<u8>) -> Result<Document, ToolError> {
    // The zip formats are checked for a bomb before anything is unpacked: every entry's own size and the sum.
    if bytes.starts_with(b"PK") {
        let mut zip = Archive::open(bytes.clone())?;
        zip.check_all()?;
    }
    let mut wb = calamine::open_workbook_auto_from_rs(std::io::Cursor::new(bytes)).map_err(|e| ToolError::new(ErrorCode::InvalidArguments, format!("this is not a workbook that can be read: {e}")))?;
    let names: Vec<String> = wb.sheet_names().into_iter().take(MAX_SHEETS).collect();
    let mut units = Vec::new();
    let mut truncated = false;
    for name in names {
        let Ok(range) = wb.worksheet_range(&name) else { continue };
        let mut rows: Vec<String> = Vec::new();
        for r in range.rows() {
            if rows.len() >= MAX_ROWS {
                truncated = true;
                break;
            }
            let line = r.iter().map(cell).collect::<Vec<_>>().join("\t");
            rows.push(line.trim_end_matches('\t').to_string());
        }
        while rows.last().is_some_and(|l| l.is_empty()) {
            rows.pop();
        }
        if rows.is_empty() {
            continue;
        }
        unit_rows(&format!("sheet {name}"), &rows, &mut units);
    }
    if units.is_empty() {
        return Err(ToolError::new(ErrorCode::InvalidArguments, "the workbook has no cells with anything in them"));
    }
    Ok(Document { kind: "sheet", units, truncated })
}

pub fn extract_csv(bytes: &[u8], tsv: bool) -> Result<Document, ToolError> {
    let text = String::from_utf8_lossy(bytes);
    let first = text.lines().next().unwrap_or("");
    let delimiter = if tsv {
        '\t'
    } else {
        [',', ';', '\t'].into_iter().max_by_key(|d| first.matches(*d).count()).filter(|d| first.matches(*d).count() > 0).unwrap_or(',')
    };
    let mut rows: Vec<String> = Vec::new();
    let mut truncated = false;
    for line in text.lines() {
        if rows.len() >= MAX_ROWS {
            truncated = true;
            break;
        }
        rows.push(if delimiter == '\t' { line.to_string() } else { line.replace('\t', " ").replace(delimiter, "\t") });
    }
    if rows.iter().all(|r| r.trim().is_empty()) {
        return Err(ToolError::new(ErrorCode::InvalidArguments, "the file has nothing in it"));
    }
    let mut units = Vec::new();
    unit_rows("table", &rows, &mut units);
    Ok(Document { kind: "csv", units, truncated })
}
