//! A small BM25 ranker over short texts (the names and descriptions of skills and tools). No model, no index on disk: the texts are
//! tokenised when the ranker is made and a query is scored against all of them.

use std::collections::HashMap;

const K1: f64 = 1.5;
const B: f64 = 0.75;

const STOP: &[&str] = &["the", "a", "an", "and", "or", "of", "to", "in", "on", "for", "with", "is", "are", "it", "this", "that", "me", "my", "please", "do", "can", "you"];

/// Words of a text: lower case, split at anything that is not a letter or digit (so `fs.read_file` is `fs`, `read`, `file`), single letters and
/// the commonest small words left out.
pub fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 2 && !STOP.contains(w))
        .map(str::to_string)
        .collect()
}

pub struct Bm25 {
    docs: Vec<HashMap<String, u32>>,
    lens: Vec<usize>,
    avg_len: f64,
    df: HashMap<String, usize>,
}

impl Bm25 {
    pub fn new(texts: &[String]) -> Self {
        let mut docs = Vec::with_capacity(texts.len());
        let mut lens = Vec::with_capacity(texts.len());
        let mut df: HashMap<String, usize> = HashMap::new();
        for t in texts {
            let toks = tokens(t);
            lens.push(toks.len());
            let mut tf: HashMap<String, u32> = HashMap::new();
            for w in toks {
                *tf.entry(w).or_insert(0) += 1;
            }
            for w in tf.keys() {
                *df.entry(w.clone()).or_insert(0) += 1;
            }
            docs.push(tf);
        }
        let avg_len = if lens.is_empty() { 0.0 } else { lens.iter().sum::<usize>() as f64 / lens.len() as f64 };
        Self { docs, lens, avg_len, df }
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// The texts that share a word with the query, best first: `(index, score)`. A text that shares none is not returned, so a query about
    /// nothing in the collection gets nothing back.
    pub fn rank(&self, query: &str) -> Vec<(usize, f64)> {
        let q = tokens(query);
        if q.is_empty() || self.docs.is_empty() {
            return Vec::new();
        }
        let n = self.docs.len() as f64;
        let mut scored: Vec<(usize, f64)> = Vec::new();
        for (i, tf) in self.docs.iter().enumerate() {
            let mut score = 0.0;
            for w in &q {
                let Some(&f) = tf.get(w) else { continue };
                let df = *self.df.get(w).unwrap_or(&0) as f64;
                let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                let f = f as f64;
                let norm = if self.avg_len > 0.0 { 1.0 - B + B * self.lens[i] as f64 / self.avg_len } else { 1.0 };
                score += idf * f * (K1 + 1.0) / (f + K1 * norm);
            }
            if score > 0.0 {
                scored.push((i, score));
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
        scored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn words_are_split_at_dots_and_underscores_and_the_common_ones_dropped() {
        assert_eq!(tokens("fs.read_file: Read a text FILE, by line range"), vec!["fs", "read", "file", "read", "text", "file", "by", "line", "range"]);
        assert!(tokens("a the of").is_empty() && tokens("").is_empty());
    }

    #[test]
    fn the_text_that_is_about_the_query_comes_first_and_one_about_nothing_is_not_returned() {
        let b = Bm25::new(&texts(&["fs.read read a text file by line range", "git.commit commit staged changes to the repository", "web.search search the web for a numbered list of results", "term.run run a program"]));
        assert_eq!(b.rank("commit my changes")[0].0, 1);
        assert_eq!(b.rank("search the internet for results")[0].0, 2);
        assert_eq!(b.rank("read this file")[0].0, 0);
        assert!(b.rank("xylophone").is_empty());
        assert!(b.rank("").is_empty() && Bm25::new(&[]).rank("anything").is_empty());
    }

    #[test]
    fn a_rarer_word_counts_for_more_and_a_short_text_beats_a_long_one_with_the_same_hit() {
        let b = Bm25::new(&texts(&["file file file file", "file stat size", "file read a very long text with many other words that are about nothing in particular at all"]));
        let r = b.rank("stat");
        assert_eq!(r.len(), 1, "only the text with the rare word");
        let r = b.rank("file");
        assert_eq!(r.len(), 3);
        assert!(r.iter().position(|x| x.0 == 1).unwrap() < r.iter().position(|x| x.0 == 2).unwrap(), "{r:?}");
    }
}
