//! Which parts of a long document answer a question: the document is cut into pieces and ranked with BM25.

use super::{Document, CHUNK_CHARS};
use std::collections::HashMap;

const K1: f64 = 1.5;
const B: f64 = 0.75;
const STOP: &[&str] = &["the", "a", "an", "and", "or", "of", "to", "in", "on", "for", "with", "is", "are", "it", "this", "that", "what", "how", "does", "do"];

fn words(text: &str) -> Vec<String> {
    text.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 2 && !STOP.contains(w)).map(str::to_string).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub label: String,
    pub text: String,
}

/// A part of the document: a unit, or a piece of one that is longer than [`CHUNK_CHARS`].
fn pieces(doc: &Document) -> Vec<Hit> {
    let mut out = Vec::new();
    for u in &doc.units {
        if u.text.chars().count() <= CHUNK_CHARS {
            out.push(Hit { label: u.label.clone(), text: u.text.clone() });
            continue;
        }
        let mut cur = String::new();
        let mut n = 0;
        for line in u.text.lines() {
            if !cur.is_empty() && cur.chars().count() + line.chars().count() + 1 > CHUNK_CHARS {
                n += 1;
                out.push(Hit { label: format!("{} (piece {n})", u.label), text: std::mem::take(&mut cur) });
            }
            cur.push_str(line);
            cur.push('\n');
        }
        if !cur.trim().is_empty() {
            n += 1;
            out.push(Hit { label: format!("{} (piece {n})", u.label), text: cur });
        }
    }
    out
}

/// The best `limit` pieces for `query`, best first; none when the query shares no word with the document.
pub fn search(doc: &Document, query: &str, limit: usize) -> Vec<Hit> {
    let parts = pieces(doc);
    let q = words(query);
    if parts.is_empty() || q.is_empty() {
        return Vec::new();
    }
    let docs: Vec<HashMap<String, u32>> = parts
        .iter()
        .map(|p| {
            let mut tf = HashMap::new();
            for w in words(&p.text) {
                *tf.entry(w).or_insert(0u32) += 1;
            }
            tf
        })
        .collect();
    let lens: Vec<usize> = docs.iter().map(|d| d.values().map(|&v| v as usize).sum()).collect();
    let avg = lens.iter().sum::<usize>() as f64 / lens.len() as f64;
    let n = docs.len() as f64;
    let mut scored: Vec<(usize, f64)> = Vec::new();
    for (i, tf) in docs.iter().enumerate() {
        let mut score = 0.0;
        for w in &q {
            let Some(&f) = tf.get(w) else { continue };
            let df = docs.iter().filter(|d| d.contains_key(w)).count() as f64;
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
            let f = f as f64;
            let norm = if avg > 0.0 { 1.0 - B + B * lens[i] as f64 / avg } else { 1.0 };
            score += idf * f * (K1 + 1.0) / (f + K1 * norm);
        }
        if score > 0.0 {
            scored.push((i, score));
        }
    }
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    scored.into_iter().take(limit).map(|(i, _)| parts[i].clone()).collect()
}
