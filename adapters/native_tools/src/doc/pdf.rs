//! PDF files: the text of each page. Nothing in the file is run (scripts, forms and links are not touched), and a scan with no text is said
//! to be one.

use super::{Document, Unit, MAX_UNITS};
use aqua_tool_host::{ErrorCode, ToolError};

pub fn extract(bytes: &[u8]) -> Result<Document, ToolError> {
    let doc = lopdf::Document::load_mem(bytes).map_err(|e| ToolError::new(ErrorCode::InvalidArguments, format!("this is not a PDF that can be read: {e}")))?;
    if doc.is_encrypted() {
        return Err(ToolError::new(ErrorCode::InvalidArguments, "this PDF is encrypted: it is not read"));
    }
    let pages = doc.get_pages();
    let mut units = Vec::new();
    let mut with_text = 0;
    for &n in pages.keys().take(MAX_UNITS) {
        let text = doc.extract_text(&[n]).unwrap_or_default();
        if !text.trim().is_empty() {
            with_text += 1;
        }
        units.push(Unit { label: format!("page {n}"), text: text.trim().to_string() });
    }
    if !units.is_empty() && with_text == 0 {
        units[0].text = "(this PDF has no text that can be read: it may be a scan, and reading a picture of text is not built)".into();
    }
    let truncated = pages.len() > MAX_UNITS;
    Ok(Document { kind: "pdf", units, truncated })
}
