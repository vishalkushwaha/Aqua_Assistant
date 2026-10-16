//! A map of a repository the model can afford: about 250 tokens (card OA12-03; task CU-09 of the OpenCode audit).
//!
//! A 6,144-token window cannot hold a listing of a folder, and a model that is asked to fix `parse_config` does not know which six files of two thousand to look at. The map answers with the
//! files most worth looking at and the names in each: `src/config.rs: parse_config, Config`. A file is worth looking at by three things that are added: the goal **names** it (its path, its
//! name or a word that is its name), the **words of the goal** are in the names of its symbols and of the type they are in (BM25 over them, as a search engine ranks), and the **import graph**
//! leads to it or from it (a personalised PageRank that starts again from the files the goal names and the files the run is working on, see [`super::graph`]). It is a guess from the shape of
//! the lines, and its first line says so.
//!
//! The map is a pure function of the files, the goal and the focus: the same ones give the same text to the character, so that a prompt that carried it would not change between rounds (the
//! cache of the model's own prefix stays valid). It is counted the way the planner counts a prompt (three characters to a token) and never goes over its budget. A file or a name that cannot be
//! shown safely (the names are text from the project's files, so a name with a space, a quote or a control character in it is left out, as the project profile leaves one out) is left out and
//! counted, and a project of more than [`super::graph::MAX_GRAPH_FILES`] files, or whose graph is cut, gets only its top-level folders with their file counts.

use super::graph::Graph;
use super::profile::safe_name;
use super::symbols::{words_of, Symbol};
use std::collections::{BTreeSet, HashMap, HashSet};

/// The size of a map when the call does not say, and the least and most it may be.
pub const DEFAULT_BUDGET_TOKENS: usize = 250;
pub const MIN_BUDGET_TOKENS: usize = 60;
pub const MAX_BUDGET_TOKENS: usize = 1_000;
/// The most names shown for one file, and the most characters of one line.
const MAX_SYMBOLS_PER_FILE: usize = 4;
const MAX_LINE_CHARS: usize = 120;
/// The longest name of a symbol that is shown.
const MAX_SYMBOL_CHARS: usize = 40;
/// How many files the goal's words may start the ranking from when it names none.
const WORD_SEEDS: usize = 3;
/// The most words of a goal that are looked for.
const MAX_GOAL_WORDS: usize = 24;
/// What the three things that make a file worth looking at are worth (the goal names it; its symbols have the goal's words; the import graph leads to it).
const NAMED_WEIGHT: f64 = 3.0;
const WORDS_WEIGHT: f64 = 2.0;
const GRAPH_WEIGHT: f64 = 2.0;
/// BM25's usual constants.
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

/// Words of a goal that say nothing about where the code is.
const STOP_WORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "are", "was", "were", "has", "have", "had", "not", "but", "can", "could", "should", "would", "will", "does", "did", "its", "our", "your", "you",
    "fix", "fixes", "failing", "fails", "failed", "make", "makes", "please", "need", "needs", "want", "add", "change", "update", "find", "show", "look", "where", "what", "when", "why", "how", "all", "any", "some",
    "then", "than", "out", "get", "set", "use", "uses", "using", "run", "runs", "also", "code", "file", "files", "bug", "issue", "error", "errors", "problem", "work", "works", "working", "broken",
];
/// File names that are not a name of what is in them (`main.rs`, `index.ts`, `mod.rs`): a goal that says "main" does not name them.
const GENERIC_STEMS: &[&str] = &["main", "lib", "mod", "index", "init", "__init__", "utils", "util", "types", "common", "test", "tests", "app", "core", "src"];

/// A rough count of tokens, as the planner counts a prompt: three characters to a token.
pub fn tokens(text: &str) -> usize {
    text.chars().count().div_ceil(3)
}

/// What the map is made from.
pub struct MapInput<'a> {
    pub graph: &'a Graph,
    /// The symbols of each file of the graph, in the order of `graph.files`.
    pub symbols: &'a [&'a [Symbol]],
    /// What the run was asked to do, in the person's words (it is only read for names and words, never obeyed).
    pub goal: &'a str,
    /// Paths (as the graph has them) of the files the run is working on.
    pub focus: &'a [String],
    pub budget_tokens: usize,
}

/// A map, and what it is made of.
#[derive(Debug, Clone, PartialEq)]
pub struct RepoMap {
    pub text: String,
    /// What [`tokens`] makes of `text`.
    pub tokens: usize,
    /// The files that have a line, in the order of the map (the folders of a map of folders).
    pub listed: Vec<String>,
    /// The files the ranking started from: the ones the goal names and the focus.
    pub seeds: Vec<String>,
    /// Files that were left out of the map because their name cannot be shown.
    pub unsafe_names: usize,
    /// How many files the map chose among.
    pub candidates: usize,
    /// Only the top-level folders are shown: the project is too big for more.
    pub folders_only: bool,
}

/// The names the goal gives: whole paths, file names and (for a file whose name says what is in it) the word that is its name, all lower case.
fn goal_tokens(goal: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in goal.split(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '.' | '/' | '\\' | '-'))).take(200) {
        let t = t.replace('\\', "/");
        let t = t.trim_matches(|c| c == '.' || c == '/' || c == '-').to_lowercase();
        if t.chars().count() >= 3 && !out.contains(&t) && out.len() < 64 {
            out.push(t);
        }
    }
    out
}

/// The files of `g` that the goal names: its whole path, the end of its path, its file name, or (not for a file whose name is no name: `main`, `index`, `mod`) the word that is its name.
fn named_in_goal(g: &Graph, goal: &str) -> Vec<u32> {
    let tokens = goal_tokens(goal);
    if tokens.is_empty() {
        return Vec::new();
    }
    let words: HashSet<&str> = tokens.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    for (i, rel) in g.files.iter().enumerate() {
        let l = rel.to_lowercase();
        let name = l.rsplit('/').next().unwrap_or(&l);
        let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
        let by_path = tokens.iter().any(|t| *t == l || (t.contains('/') && l.ends_with(&format!("/{t}"))) || (t.contains('.') && t == name));
        let by_stem = stem.chars().count() >= 4 && !GENERIC_STEMS.contains(&stem) && words.contains(stem);
        if by_path || by_stem {
            out.push(i as u32);
        }
    }
    out
}

/// The words of a goal that say where something is: lower case, three letters or more, none of the usual filler, each once.
fn goal_words(goal: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in words_of(goal) {
        if w.chars().count() >= 3 && !STOP_WORDS.contains(&w.as_str()) && !out.contains(&w) && out.len() < MAX_GOAL_WORDS {
            out.push(w);
        }
    }
    out
}

fn stem_of(rel: &str) -> &str {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// BM25 of the goal's words over each file: its document is the words of the names of its symbols, of the type each is in, and of its file name. All zero when the goal has no words.
fn bm25(g: &Graph, symbols: &[&[Symbol]], q: &[String]) -> Vec<f64> {
    let n = g.files.len();
    if q.is_empty() || n == 0 {
        return vec![0.0; n];
    }
    let index: HashMap<&str, usize> = q.iter().enumerate().map(|(i, w)| (w.as_str(), i)).collect();
    let mut tf = vec![vec![0u32; q.len()]; n];
    let mut dl = vec![0usize; n];
    for i in 0..n {
        let mut see = |w: &str| {
            dl[i] += 1;
            if let Some(k) = index.get(w) {
                tf[i][*k] += 1;
            }
        };
        for w in words_of(stem_of(&g.files[i])) {
            see(&w);
        }
        for s in symbols.get(i).copied().unwrap_or(&[]) {
            for w in words_of(&s.name) {
                see(&w);
            }
            if s.depth > 0 {
                for w in words_of(&s.parent) {
                    see(&w);
                }
            }
        }
    }
    let avg = (dl.iter().sum::<usize>() as f64 / n as f64).max(1.0);
    let idf: Vec<f64> = (0..q.len())
        .map(|k| {
            let df = tf.iter().filter(|row| row[k] > 0).count() as f64;
            (1.0 + (n as f64 - df + 0.5) / (df + 0.5)).ln()
        })
        .collect();
    (0..n)
        .map(|i| {
            (0..q.len())
                .map(|k| {
                    let f = f64::from(tf[i][k]);
                    if f == 0.0 {
                        0.0
                    } else {
                        idf[k] * f * (BM25_K1 + 1.0) / (f + BM25_K1 * (1.0 - BM25_B + BM25_B * dl[i] as f64 / avg))
                    }
                })
                .sum()
        })
        .collect()
}

/// A name that may be shown: letters, digits, `_` and `$`, not too long. Anything else (a space, a quote, a bracket, a control character) means the name is not shown.
fn safe_ident(name: &str) -> Option<String> {
    (!name.is_empty() && name.chars().count() <= MAX_SYMBOL_CHARS && name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$')).then(|| name.to_string())
}

/// The names to show for a file: the top-level ones the goal's words are in first, then the public ones in the order of the file; a method only when the goal's words are in it or in its type.
fn pick_symbols(symbols: &[Symbol], goal: &[String]) -> Vec<String> {
    let want: HashSet<&str> = goal.iter().map(String::as_str).collect();
    let mut cands: Vec<(i64, usize, String)> = Vec::new();
    for (pos, s) in symbols.iter().enumerate() {
        if s.kind == "impl" || (s.kind == "mod" && matches!(s.name.as_str(), "tests" | "test")) || s.depth > 1 {
            continue;
        }
        let hits = words_of(&s.name).iter().chain(words_of(&s.parent).iter()).filter(|w| want.contains(w.as_str())).count() as i64;
        if s.depth > 0 && hits == 0 {
            continue;
        }
        let Some(name) = safe_ident(&s.name) else { continue };
        cands.push((hits * 10 + i64::from(s.public) * 2 - s.depth as i64, pos, name));
    }
    cands.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut seen: HashSet<String> = HashSet::new();
    cands.into_iter().filter(|(_, _, n)| seen.insert(n.clone())).map(|(_, _, n)| n).take(MAX_SYMBOLS_PER_FILE).collect()
}

fn footer(more: usize) -> String {
    format!("+{more} more files (code.deps, code.symbols)")
}

/// The map of a project. See the module's description.
pub fn repo_map(input: &MapInput<'_>) -> RepoMap {
    let g = input.graph;
    let n = g.files.len();
    let budget = input.budget_tokens.clamp(MIN_BUDGET_TOKENS, MAX_BUDGET_TOKENS);
    if n == 0 {
        let text = "Repo map: no source files of a language read here.".to_string();
        return RepoMap { tokens: tokens(&text), text, listed: Vec::new(), seeds: Vec::new(), unsafe_names: 0, candidates: 0, folders_only: false };
    }
    let words = goal_words(input.goal);
    let named = named_in_goal(g, input.goal);
    let mut seeds: BTreeSet<u32> = named.iter().copied().collect();
    seeds.extend(input.focus.iter().filter_map(|f| g.id_of(f)));
    let named_or_focus: HashSet<u32> = seeds.iter().copied().collect();
    let by_words = bm25(g, input.symbols, &words);
    if seeds.is_empty() {
        // The goal names no file: the files whose symbols have its words are where the walk starts.
        let mut best: Vec<(f64, u32)> = by_words.iter().enumerate().filter(|(_, s)| **s > 0.0).map(|(i, s)| (*s, i as u32)).collect();
        best.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        seeds.extend(best.iter().take(WORD_SEEDS).map(|(_, i)| *i));
    }
    let seed_list: Vec<u32> = seeds.iter().copied().collect();
    let rank = g.rank(&seed_list);
    let max_words = by_words.iter().copied().fold(0.0f64, f64::max);
    let max_rank = rank.iter().copied().fold(0.0f64, f64::max);
    let score = |i: usize| -> f64 {
        let mut s = 0.0;
        if named_or_focus.contains(&(i as u32)) {
            s += NAMED_WEIGHT;
        }
        if max_words > 0.0 {
            s += WORDS_WEIGHT * by_words[i] / max_words;
        }
        if max_rank > 0.0 {
            s += GRAPH_WEIGHT * rank[i] / max_rank;
        }
        s
    };
    let mut unsafe_names = 0usize;
    let mut order: Vec<(f64, usize, String)> = Vec::new();
    for (i, rel) in g.files.iter().enumerate() {
        match safe_name(rel) {
            Some(name) if name == *rel => order.push((score(i), i, name)),
            _ => unsafe_names += 1,
        }
    }
    // Best first; equal scores by path (the paths are sorted, so by position).
    order.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let head = format!("Repo map, a guess from imports and names ({n} files):");
    let reserve = footer(order.len()).chars().count() + 1;
    let mut text = head;
    let mut chars = text.chars().count();
    let mut listed: Vec<String> = Vec::new();
    for (_, i, path) in &order {
        let mut names = pick_symbols(input.symbols.get(*i).copied().unwrap_or(&[]), &words);
        let line = loop {
            let line = if names.is_empty() { path.clone() } else { format!("{path}: {}", names.join(", ")) };
            let fits_line = line.chars().count() <= MAX_LINE_CHARS;
            if fits_line && (chars + 1 + line.chars().count() + reserve).div_ceil(3) <= budget {
                break Some(line);
            }
            if names.pop().is_none() {
                break None;
            }
        };
        let Some(line) = line else { break };
        chars += 1 + line.chars().count();
        text.push('\n');
        text.push_str(&line);
        listed.push(path.clone());
    }
    let more = order.len() - listed.len();
    if more > 0 {
        text.push('\n');
        text.push_str(&footer(more));
    }
    let seeds: Vec<String> = seeds.iter().map(|i| g.files[*i as usize].clone()).collect();
    RepoMap { tokens: tokens(&text), text, listed, seeds, unsafe_names, candidates: order.len(), folders_only: false }
}

/// The map of a project too big for the other: its top-level folders with the number of source files in each, biggest first (a file at the top is counted under `.`).
pub fn folder_map(files: &[String], budget_tokens: usize, why: &str) -> RepoMap {
    let budget = budget_tokens.clamp(MIN_BUDGET_TOKENS, MAX_BUDGET_TOKENS);
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut unsafe_names = 0usize;
    for f in files {
        let top = f.split_once('/').map_or(".", |(d, _)| d);
        match safe_name(top) {
            Some(name) => *counts.entry(name).or_default() += 1,
            None => unsafe_names += 1,
        }
    }
    let mut dirs: Vec<(String, usize)> = counts.into_iter().collect();
    dirs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut text = format!("Repo map, top-level folders only ({why}; {} files):", files.len());
    let mut chars = text.chars().count();
    let mut listed = Vec::new();
    let reserve = footer(dirs.len()).chars().count() + 1;
    for (dir, count) in &dirs {
        let line = if dir == "." { format!(".: {count} files") } else { format!("{dir}/: {count} files") };
        if (chars + 1 + line.chars().count() + reserve).div_ceil(3) > budget {
            break;
        }
        chars += 1 + line.chars().count();
        text.push('\n');
        text.push_str(&line);
        listed.push(dir.clone());
    }
    let more = dirs.len() - listed.len();
    if more > 0 {
        text.push('\n');
        text.push_str(&format!("+{more} more folders"));
    }
    RepoMap { tokens: tokens(&text), text, listed, seeds: Vec::new(), unsafe_names, candidates: dirs.len(), folders_only: true }
}

#[cfg(test)]
mod tests {
    use super::super::graph::{build, scan_imports, GraphFile};
    use super::super::symbols::symbols_of;
    use super::*;

    /// A graph and the symbols of each of its files, from files given as (path, text).
    fn fixture(files: &[(&str, &str)]) -> (Graph, Vec<Vec<Symbol>>) {
        let mut files: Vec<(&str, &str)> = files.to_vec();
        files.sort_by(|a, b| a.0.cmp(b.0));
        let scanned: Vec<(&str, Vec<_>)> = files.iter().map(|(rel, text)| (*rel, scan_imports(rel.rsplit('/').next().unwrap(), text).imports)).collect();
        let inputs: Vec<GraphFile<'_>> = scanned.iter().map(|(rel, imports)| GraphFile { rel, imports }).collect();
        let symbols = files.iter().map(|(rel, text)| symbols_of(rel.rsplit('/').next().unwrap(), text).symbols).collect();
        (build(&inputs, &[]), symbols)
    }

    fn map_of(f: &(Graph, Vec<Vec<Symbol>>), goal: &str, focus: &[&str], budget: usize) -> RepoMap {
        let symbols: Vec<&[Symbol]> = f.1.iter().map(Vec::as_slice).collect();
        let focus: Vec<String> = focus.iter().map(|s| s.to_string()).collect();
        repo_map(&MapInput { graph: &f.0, symbols: &symbols, goal, focus: &focus, budget_tokens: budget })
    }

    const PROJECT: &[(&str, &str)] = &[
        ("src/lib.rs", "pub mod config;\npub mod server;\npub mod util;\npub mod net;\n"),
        ("src/config.rs", "use crate::util::helper;\n\npub struct Config {\n    pub name: String,\n}\n\nimpl Config {\n    pub fn load(&self) {}\n}\n\npub fn parse_config(path: &str) -> Config {\n    helper();\n    Config { name: path.into() }\n}\n\npub fn default_settings() {}\n"),
        ("src/server.rs", "use crate::config::Config;\nuse crate::util::helper;\n\npub struct Server;\n\npub fn serve(c: &Config) {\n    helper();\n}\n"),
        ("src/util.rs", "pub fn helper() {}\n"),
        ("src/net/mod.rs", "pub mod tcp;\npub fn connect() {}\n"),
        ("src/net/tcp.rs", "use crate::util::helper;\n\npub struct Conn;\n\npub fn open_socket() {}\n"),
        ("tests/config_test.rs", "use mylib::config::parse_config;\n\nfn parse_config_reads_a_name() {}\n"),
    ];

    #[test]
    fn the_map_stays_inside_its_budget() {
        // Four hundred files with long names, each importing the next few: a map of any budget is no longer than it.
        let files: Vec<(String, String)> = (0..400)
            .map(|i| {
                let imports: String = (1..4).map(|k| format!("import '../feature_{:02}/module_with_a_long_name_{:03}';\n", ((i + k) % 400) % 17, (i + k) % 400)).collect();
                (format!("packages/feature_{:02}/module_with_a_long_name_{i:03}.ts", i % 17), format!("{imports}export function handle_request_number_{i}(a: number) {{\n  return a;\n}}\nexport class Service_{i} {{}}\n"))
            })
            .collect();
        let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let f = fixture(&refs);
        for budget in [MIN_BUDGET_TOKENS, 100, DEFAULT_BUDGET_TOKENS, 600, MAX_BUDGET_TOKENS] {
            let m = map_of(&f, "handle the request in service 17", &[], budget);
            assert!(m.tokens <= budget && tokens(&m.text) == m.tokens, "{} tokens for {budget}: {}", m.tokens, m.text);
            assert!(m.text.chars().count() <= budget * 3, "{budget}: {}", m.text);
            assert!(!m.listed.is_empty() && m.text.starts_with("Repo map, a guess from imports and names (400 files):"), "{}", m.text);
            assert!(m.text.lines().last().unwrap().starts_with('+') || m.listed.len() == 400, "what is left out is counted: {}", m.text);
        }
        // A budget beyond the limits is held to them.
        assert!(map_of(&f, "", &[], 5).tokens <= MIN_BUDGET_TOKENS && map_of(&f, "", &[], 50_000).tokens <= MAX_BUDGET_TOKENS);
        // The map of 250 tokens holds the six to eight files the audit asks for.
        let m = map_of(&f, "handle the request in service 17", &[], DEFAULT_BUDGET_TOKENS);
        assert!((5..=12).contains(&m.listed.len()), "{} files: {}", m.listed.len(), m.text);
    }

    #[test]
    fn the_map_is_identical_for_the_same_focus_so_the_prompt_prefix_does_not_change() {
        let f = fixture(PROJECT);
        let a = map_of(&f, "fix the failing test in parse_config", &["src/config.rs"], DEFAULT_BUDGET_TOKENS);
        let b = map_of(&f, "fix the failing test in parse_config", &["src/config.rs"], DEFAULT_BUDGET_TOKENS);
        assert_eq!(a, b);
        // The files given in another order make the same graph and so the same map.
        let mut shuffled: Vec<(&str, &str)> = PROJECT.to_vec();
        shuffled.reverse();
        let c = map_of(&fixture(&shuffled), "fix the failing test in parse_config", &["src/config.rs"], DEFAULT_BUDGET_TOKENS);
        assert_eq!(a.text, c.text);
        // Another focus is another map, and so is another goal.
        let other = map_of(&f, "fix the failing test in parse_config", &["src/net/tcp.rs"], DEFAULT_BUDGET_TOKENS);
        assert_ne!(a.text, other.text, "{}\n{}", a.text, other.text);
        // No focus, no goal: still the same map each time, over all the files.
        assert_eq!(map_of(&f, "", &[], 250).text, map_of(&f, "", &[], 250).text);
    }

    #[test]
    fn the_map_prefers_files_the_goal_names() {
        let f = fixture(PROJECT);
        // A path in the goal.
        let m = map_of(&f, "update src/net/tcp.rs so it reconnects", &[], DEFAULT_BUDGET_TOKENS);
        assert_eq!(m.listed[0], "src/net/tcp.rs", "{}", m.text);
        assert_eq!(m.seeds, ["src/net/tcp.rs"]);
        // A file name, and the word that is a file's name.
        let m = map_of(&f, "the serve function in server.rs panics", &[], DEFAULT_BUDGET_TOKENS);
        assert_eq!(m.listed[0], "src/server.rs", "{}", m.text);
        assert!(m.text.contains("src/server.rs: serve, Server") || m.text.contains("src/server.rs: Server, serve"), "{}", m.text);
        // The goal names no file: the words of the goal are in the names of the symbols of the file it means, and it is listed first, with the names that have the goal's words first.
        let m = map_of(&f, "load the default settings", &[], DEFAULT_BUDGET_TOKENS);
        assert_eq!(m.listed[0], "src/config.rs", "{}", m.text);
        assert!(m.text.contains("src/config.rs: default_settings, load"), "{}", m.text);
        // The goal of the audit's example: the file that holds `parse_config` and the test that names it are the first two.
        let m = map_of(&f, "fix the failing test in parse_config", &[], DEFAULT_BUDGET_TOKENS);
        assert!(m.listed[..2].contains(&"src/config.rs".to_string()) && m.listed[..2].contains(&"tests/config_test.rs".to_string()), "{}", m.text);
        assert!(m.text.contains("src/config.rs: parse_config"), "the name the goal has comes first in its line: {}", m.text);
        // What the file imports and what imports it are on the map around it.
        assert!(m.listed.contains(&"src/util.rs".to_string()) && m.listed.contains(&"src/server.rs".to_string()), "{}", m.text);
        // The focus is where the walk starts, with or without a goal.
        let m = map_of(&f, "", &["src/net/tcp.rs"], DEFAULT_BUDGET_TOKENS);
        assert_eq!(m.listed[0], "src/net/tcp.rs", "{}", m.text);
        // A file whose name is not a name (`main`, `index`, `mod`) is not named by the word.
        let small = fixture(&[("src/main.rs", "fn main() {}\n"), ("src/parser.rs", "pub fn x() {}\n"), ("src/utils.rs", "pub fn y() {}\n"), ("web/index.ts", "export const a = 1;\n")]);
        assert!(named_in_goal(&small.0, "the main function and the index page and the utils").is_empty(), "main, index and utils say nothing about what is in them");
        assert_eq!(named_in_goal(&small.0, "the parser module"), vec![small.0.id_of("src/parser.rs").unwrap()]);
        assert_eq!(named_in_goal(&small.0, "see web/index.ts").len(), 1, "a path names a file whatever its name is");
    }

    #[test]
    fn a_name_that_cannot_be_shown_is_left_out_and_counted() {
        let f = fixture(&[("src/a.rs", "pub fn fine() {}\n"), ("src/with space.rs", "pub fn hidden() {}\n"), ("src/(group)/p.ts", "export function page() {}\n"), ("src/b.rs", "pub fn \u{202e}evil() {}\n")]);
        let m = map_of(&f, "", &[], DEFAULT_BUDGET_TOKENS);
        assert_eq!(m.unsafe_names, 2, "{}", m.text);
        assert!(m.text.contains("src/a.rs: fine") && !m.text.contains("space") && !m.text.contains("group") && !m.text.contains("hidden"), "{}", m.text);
        assert!(m.text.contains("src/b.rs") && !m.text.contains("evil"), "a name with a control character in it is not shown: {}", m.text);
        assert_eq!(map_of(&fixture(&[]), "x", &[], 250).text, "Repo map: no source files of a language read here.");
    }

    #[test]
    fn a_project_too_big_for_a_map_is_listed_by_folder() {
        let files: Vec<String> = (0..300).map(|i| format!("{}/f{i}.rs", ["core", "adapters", "scripts", "docs"][i % 4])).chain(["build.rs".to_string()]).collect();
        let m = folder_map(&files, DEFAULT_BUDGET_TOKENS, "too many files");
        assert!(m.folders_only && m.text.starts_with("Repo map, top-level folders only (too many files; 301 files):"), "{}", m.text);
        assert_eq!(m.text.lines().nth(1), Some("adapters/: 75 files"));
        assert!(m.text.contains(".: 1 files") && m.tokens <= DEFAULT_BUDGET_TOKENS, "{}", m.text);
        let many: Vec<String> = (0..500).map(|i| format!("folder_number_{i:03}/a.rs")).collect();
        let m = folder_map(&many, 60, "x");
        assert!(m.tokens <= 60 && m.text.lines().last().unwrap().ends_with("more folders"), "{}", m.text);
    }

    #[test]
    fn the_symbols_of_a_line_are_the_ones_the_goal_has_then_the_public_ones() {
        let o = symbols_of("a.rs", "pub struct Config;\nimpl Config {\n    pub fn load(&self) {}\n}\nfn private_one() {}\npub fn parse_config() {}\npub fn other() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n");
        let words = goal_words("fix the failing parse of the config file");
        assert_eq!(words, ["parse", "config"], "filler is not a word of the goal");
        let picked = pick_symbols(&o.symbols, &words);
        assert_eq!(picked[0], "parse_config", "{picked:?}");
        assert!(picked.contains(&"Config".to_string()) && picked.contains(&"load".to_string()), "a method of the type the goal names is shown: {picked:?}");
        assert!(!picked.contains(&"tests".to_string()) && picked.len() <= MAX_SYMBOLS_PER_FILE, "{picked:?}");
        let none = pick_symbols(&o.symbols, &[]);
        assert_eq!(none[0], "Config", "no goal: the public ones in the order of the file: {none:?}");
        assert!(!none.contains(&"load".to_string()), "a method is shown only when the goal asks for it: {none:?}");
    }
}
