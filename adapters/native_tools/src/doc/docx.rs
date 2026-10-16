//! Word files: the paragraphs and tables of `word/document.xml`, as text.

use super::{parse_xml, split_text, Archive, Document};
use aqua_tool_host::ToolError;
use roxmltree::Node;

fn paragraph(p: Node) -> String {
    let mut s = String::new();
    for d in p.descendants() {
        match d.tag_name().name() {
            "t" => s.push_str(d.text().unwrap_or("")),
            "tab" => s.push('\t'),
            "br" | "cr" => s.push('\n'),
            _ => {}
        }
    }
    // A heading keeps a mark, so the shape of the document shows.
    let heading = p
        .children()
        .filter(|c| c.tag_name().name() == "pPr")
        .flat_map(|ppr| ppr.children().filter(|c| c.tag_name().name() == "pStyle"))
        .filter_map(|st| st.attributes().find(|a| a.name() == "val").map(|a| a.value().to_string()))
        .any(|v| v.starts_with("Heading") || v == "Title");
    if heading && !s.trim().is_empty() {
        format!("# {}", s.trim())
    } else {
        s
    }
}

fn cell_text(tc: Node) -> String {
    tc.descendants().filter(|n| n.tag_name().name() == "p").map(paragraph).filter(|t| !t.trim().is_empty()).collect::<Vec<_>>().join(" ")
}

fn walk(node: Node, out: &mut Vec<String>) {
    for c in node.children().filter(Node::is_element) {
        match c.tag_name().name() {
            "p" => out.push(paragraph(c)),
            "tbl" => {
                for tr in c.children().filter(|n| n.tag_name().name() == "tr") {
                    let cells: Vec<String> = tr.children().filter(|n| n.tag_name().name() == "tc").map(cell_text).collect();
                    out.push(cells.join(" | "));
                }
            }
            _ => walk(c, out),
        }
    }
}

/// The text of the document as it reads, in parts of about 3,000 characters.
pub fn text_of(xml: &str) -> Result<String, ToolError> {
    let doc = parse_xml(xml)?;
    let mut lines = Vec::new();
    walk(doc.root_element(), &mut lines);
    Ok(lines.join("\n"))
}

pub fn extract(bytes: Vec<u8>) -> Result<Document, ToolError> {
    let mut zip = Archive::open(bytes)?;
    let xml = zip.text("word/document.xml")?.ok_or_else(|| ToolError::new(aqua_tool_host::ErrorCode::InvalidArguments, "this is not a Word document (it has no word/document.xml)"))?;
    let text = text_of(&xml)?;
    Ok(Document { kind: "docx", units: split_text("part", &text, 3000), truncated: false })
}
