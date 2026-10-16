//! PowerPoint files: the text of each slide and of its speaker notes.

use super::{parse_xml, Archive, Document, Unit};
use aqua_tool_host::{ErrorCode, ToolError};

fn slide_number(name: &str, prefix: &str) -> Option<u32> {
    name.strip_prefix(prefix)?.strip_suffix(".xml")?.parse().ok()
}

fn text_of(xml: &str) -> Result<String, ToolError> {
    let doc = parse_xml(xml)?;
    let mut lines = Vec::new();
    for p in doc.descendants().filter(|n| n.tag_name().name() == "p") {
        let t: String = p.descendants().filter(|n| n.tag_name().name() == "t").map(|n| n.text().unwrap_or("")).collect();
        if !t.trim().is_empty() {
            lines.push(t);
        }
    }
    Ok(lines.join("\n"))
}

pub fn extract(bytes: Vec<u8>) -> Result<Document, ToolError> {
    let mut zip = Archive::open(bytes)?;
    let mut slides: Vec<(u32, String)> = zip.names().into_iter().filter_map(|n| slide_number(&n, "ppt/slides/slide").map(|k| (k, n))).collect();
    if slides.is_empty() {
        return Err(ToolError::new(ErrorCode::InvalidArguments, "this is not a PowerPoint file (it has no slides)"));
    }
    slides.sort();
    let mut units = Vec::new();
    for (k, name) in slides {
        let mut text = zip.text(&name)?.map(|x| text_of(&x)).transpose()?.unwrap_or_default();
        if let Some(notes) = zip.text(&format!("ppt/notesSlides/notesSlide{k}.xml"))? {
            let n = text_of(&notes)?;
            if !n.trim().is_empty() {
                text.push_str(&format!("\nNotes: {}", n.replace('\n', " ")));
            }
        }
        units.push(Unit { label: format!("slide {k}"), text });
    }
    Ok(Document { kind: "pptx", units, truncated: false })
}
