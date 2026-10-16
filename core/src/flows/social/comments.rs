//! Reading the comments under a post from what the browser shows.
//!
//! The page is read as the accessibility tree a screen reader would get, so a comment is found by what it is (an
//! article named "Comment by <name>"), not by where it sits or a stored selector. Who wrote it, what it says, how old it
//! is and whether it is a reply are read from inside that block. Nothing here decides what to do about a comment.
//!
//! **Comment text comes from strangers.** It is cleaned (control and invisible characters removed, length capped) and
//! handed on only as data. It is never a setting of a step, never reaches a step that can act by itself, and every reply
//! is approved by the person.

use serde::Serialize;
use sha2::{Digest, Sha256};

/// The most comments one read returns, and the most text kept from one.
pub const MAX_COMMENTS: usize = 200;
pub const MAX_COMMENT_CHARS: usize = 2000;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Comment {
    /// Stable across reads: the same comment has the same id, so a comment already dealt with is recognised.
    pub id: String,
    pub author: String,
    pub text: String,
    /// How old it says it is ("2h", "3d"), as shown.
    pub age: String,
    /// Written by the person whose post it is (when their name was given).
    pub is_mine: bool,
    /// The id of the comment this answers, for a reply.
    pub parent: Option<String>,
    /// For a top-level comment: has the post's owner already answered it?
    pub answered: bool,
    /// The address of the author's profile, when the page shows one.
    pub profile: String,
    /// The reference of the comment's block on the page (valid only for the read it came from).
    #[serde(skip)]
    pub block_ref: String,
    /// The page lines of this comment without the replies nested inside it (where its own buttons are).
    #[serde(skip)]
    pub own_lines: Vec<String>,
    /// All the page lines of this comment, including its replies.
    #[serde(skip)]
    pub all_lines: Vec<String>,
}

/// Remove what a person cannot see or should not act on: control characters, zero-width and direction-changing
/// characters, runs of blank lines; cut to length.
pub fn clean(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        let invisible = matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{FEFF}');
        if invisible || (c.is_control() && c != '\n') {
            continue;
        }
        out.push(c);
    }
    let mut lines: Vec<&str> = out.lines().map(|l| l.trim()).collect();
    lines.dedup_by(|a, b| a.is_empty() && b.is_empty());
    lines.join("\n").trim().chars().take(MAX_COMMENT_CHARS).collect()
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// `- article "Comment by Priya Shah" [ref=e12]:` -> (role, name, ref, inline text after the colon).
fn parse_line(line: &str) -> Option<(String, String, String, String)> {
    let t = line.trim_start().trim_start_matches("- ");
    let role_end = t.find([' ', ':']).unwrap_or(t.len());
    let role = t[..role_end].to_string();
    let rest = &t[role_end..];
    let (name, after_name) = match (rest.find('"'), rest.rfind('"')) {
        (Some(a), Some(b)) if b > a => (rest[a + 1..b].to_string(), &rest[b + 1..]),
        _ => (String::new(), rest),
    };
    let reference = after_name.find("[ref=").and_then(|i| after_name[i + 5..].find(']').map(|j| after_name[i + 5..i + 5 + j].to_string())).unwrap_or_default();
    // Text after the last "]:" or after the first ": " that follows the attributes.
    let inline = after_name.rfind("]:").map(|i| &after_name[i + 2..]).or_else(|| after_name.strip_prefix(':')).unwrap_or("").trim().to_string();
    Some((role, name, reference, inline))
}

fn looks_like_age(s: &str) -> bool {
    let t = s.trim().trim_start_matches('•').trim().to_lowercase();
    if matches!(t.as_str(), "now" | "just now" | "moments ago" | "a moment ago" | "today" | "yesterday") {
        return true;
    }
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || t.len() > 14 {
        return false;
    }
    let unit = t[digits..].trim();
    matches!(unit.trim_end_matches(" ago"), "s" | "m" | "h" | "d" | "w" | "mo" | "y" | "yr" | "min" | "hr" | "sec" | "day" | "days" | "week" | "weeks" | "month" | "months" | "hour" | "hours" | "minute" | "minutes" | "year" | "years")
}

struct Block {
    start: usize,
    end: usize,
    indent: usize,
    author: String,
    reference: String,
}

/// Every comment block: an article (or list item) named "Comment by ...", with the lines it spans.
fn blocks(lines: &[&str]) -> Vec<Block> {
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let Some((role, name, reference, _)) = parse_line(l) else { continue };
        let lower = name.to_lowercase();
        if matches!(role.as_str(), "article" | "listitem" | "group") && lower.starts_with("comment by ") {
            let ind = indent(l);
            let end = lines.iter().enumerate().skip(i + 1).find(|(_, x)| !x.trim().is_empty() && indent(x) <= ind).map(|(j, _)| j).unwrap_or(lines.len());
            out.push(Block { start: i, end, indent: ind, author: clean(name.trim()[11..].trim()), reference });
        }
    }
    out
}

/// The comments in a snapshot of a post's page. `my_name` is the post owner's name, if known: their own comments are
/// marked, and a comment they have answered is marked as answered.
pub fn parse_comments(snapshot: &str, my_name: &str) -> Vec<Comment> {
    let lines: Vec<&str> = snapshot.lines().collect();
    let all = blocks(&lines);
    let me = my_name.trim().to_lowercase();
    let mut out: Vec<Comment> = Vec::new();
    for b in &all {
        // This block's own lines, without the lines of comments nested inside it (its replies).
        let nested: Vec<&Block> = all.iter().filter(|o| o.start > b.start && o.end <= b.end).collect();
        let own = |idx: usize| !nested.iter().any(|n| idx >= n.start && idx < n.end);
        let mut text_parts: Vec<String> = Vec::new();
        let mut age = String::new();
        let mut profile = String::new();
        for idx in b.start + 1..b.end {
            if !own(idx) {
                continue;
            }
            let Some((role, name, _, inline)) = parse_line(lines[idx]) else { continue };
            match role.as_str() {
                "link" if profile.is_empty() => {
                    // The next line of a link is its address.
                    if let Some(u) = lines.get(idx + 1).and_then(|n| n.trim_start().trim_start_matches("- ").strip_prefix("/url:")) {
                        if u.contains("/in/") || u.contains("linkedin.com") {
                            profile = u.trim().trim_matches('"').to_string();
                        }
                    }
                }
                "time" | "text" | "generic" | "paragraph" | "span" if looks_like_age(&inline) || looks_like_age(&name) => {
                    if age.is_empty() {
                        age = if looks_like_age(&inline) { inline.clone() } else { name.clone() };
                    }
                }
                "paragraph" | "text" | "generic" | "blockquote" | "emphasis" | "strong" if !inline.is_empty() => {
                    // Not the author's name or a label repeated from a control.
                    if inline != b.author && !matches!(inline.to_lowercase().as_str(), "reply" | "like" | "react" | "report" | "more" | "edited") {
                        text_parts.push(inline);
                    }
                }
                _ => {}
            }
        }
        let text = clean(&text_parts.join("\n"));
        let is_mine = !me.is_empty() && b.author.to_lowercase() == me;
        // A block nested directly inside another is a reply to it.
        let parent_block = all.iter().filter(|o| o.start < b.start && o.end >= b.end && o.indent < b.indent).max_by_key(|o| o.start);
        let id = comment_id(&b.author, &text);
        let parent = parent_block.map(|p| {
            let ptext = parse_one_text(&lines, p, &all);
            comment_id(&p.author, &ptext)
        });
        let own_lines: Vec<String> = (b.start..b.end).filter(|i| own(*i)).map(|i| lines[i].to_string()).collect();
        let all_lines: Vec<String> = (b.start..b.end).map(|i| lines[i].to_string()).collect();
        out.push(Comment { id, author: b.author.clone(), text, age, is_mine, parent, answered: false, profile, block_ref: b.reference.clone(), own_lines, all_lines });
        if out.len() >= MAX_COMMENTS {
            break;
        }
    }
    // A top-level comment is answered when its owner wrote a reply under it.
    let mine_parents: Vec<String> = out.iter().filter(|c| c.is_mine).filter_map(|c| c.parent.clone()).collect();
    for c in out.iter_mut() {
        c.answered = mine_parents.contains(&c.id);
    }
    out
}

/// The text of a block, without its nested comments (used to give a parent the same id it has on its own).
fn parse_one_text(lines: &[&str], b: &Block, all: &[Block]) -> String {
    let nested: Vec<&Block> = all.iter().filter(|o| o.start > b.start && o.end <= b.end).collect();
    let mut parts = Vec::new();
    for idx in b.start + 1..b.end {
        if nested.iter().any(|n| idx >= n.start && idx < n.end) {
            continue;
        }
        let Some((role, name, _, inline)) = parse_line(lines[idx]) else { continue };
        if matches!(role.as_str(), "time" | "text" | "generic" | "paragraph" | "span") && (looks_like_age(&inline) || looks_like_age(&name)) {
            continue;
        }
        if matches!(role.as_str(), "paragraph" | "text" | "generic" | "blockquote" | "emphasis" | "strong") && !inline.is_empty() && inline != b.author && !matches!(inline.to_lowercase().as_str(), "reply" | "like" | "react" | "report" | "more" | "edited") {
            parts.push(inline);
        }
    }
    clean(&parts.join("\n"))
}

/// The same author saying the same thing has the same id, on any read.
pub fn comment_id(author: &str, text: &str) -> String {
    let mut h = Sha256::new();
    h.update(author.trim().to_lowercase().as_bytes());
    h.update([0]);
    h.update(text.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ").as_bytes());
    let d = h.finalize();
    d.iter().take(6).map(|b| format!("{b:02x}")).collect()
}
