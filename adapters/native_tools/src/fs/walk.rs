//! One walker for the file tools that look through a folder: `fs.list`, `fs.find` and `fs.grep` (card OA6-06: SE-04, CU-02, CU-05).
//!
//! What it does that the three hand-written walkers did not:
//!
//! * **Every entry is checked against the guard**, not only the folder the search started in (`PathGuard::walk_gate`): Aqua's own files, the credential folders (`.ssh`, `.aws`, `.docker`,
//!   `.config/gcloud`...) and `.git/config` are neither entered nor read, whatever `include_hidden` says, and are counted (`skipped_protected`). Links are never followed.
//! * **A folder is walked level by level.** A file near the top is met before a file deep inside the first folder in alphabetical order, so a budget that runs out leaves the deep part of
//!   the tree and not the shallow part.
//! * **Ignore files are read** (`.gitignore`, `.ignore`, `.git/info/exclude`, a std-only subset: see [`parse_ignore`]) and what they hide is left out and counted. A tree with no ignore rule at
//!   all gets a short list of folders nobody searches ([`JUNK_DIRS`]); `include_ignored` turns both off. Nothing is run and no git setting is read.
//! * **A glob is matched against a path** when it has a `/` or a `**` in it (relative to the searched folder) and against the file name at any depth when it has not ([`Glob`]); `{a,b}` and
//!   `[a-z]` work; one that cannot be read is an error that says why, never an empty answer.
//! * **It counts what it left out** and says so ([`WalkStats::footer`]), so that "No matches." can be believed or doubted.

use super::guard::{DirGate, PathGuard};
use std::collections::VecDeque;
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Folders nobody searches, used when the tree has no ignore rule at all (a repository that has a `.gitignore` says for itself what it does not want).
/// `vendor`, `bin` and `obj` are left out of the list on purpose: they are often the code a question is about.
pub const JUNK_DIRS: &[&str] = &["node_modules", "target", ".git", "__pycache__", ".venv", "venv", ".pytest_cache", ".next", ".turbo", ".gradle", "dist", "build", "out"];

/// The most files a walk may meet for each file it may hand over (the rest is a glob or a filter saying no): a tree of a million files that the glob refuses is not read to its end.
const VISIT_FACTOR: usize = 10;

/// The largest ignore file that is read, and the most rules of one that are kept (every rule is compared with every entry).
const IGNORE_FILE_MAX: u64 = 256 << 10;
const IGNORE_RULES_MAX: usize = 2_000;

/// The most alternatives one glob may expand to (`{a,b}{c,d}` is four).
const MAX_GLOB_ALTS: usize = 64;

// ---- glob -------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Lit(char),
    /// `?`: one character.
    Any,
    /// `*`: any run of characters (inside one path segment).
    Star,
    Class { negated: bool, items: Vec<(char, char)> },
}

#[derive(Debug, Clone, PartialEq)]
enum Seg {
    Pat(Vec<Tok>),
    /// `**` as a whole segment: any number of segments.
    Deep,
}

#[derive(Debug, Clone)]
struct Alt {
    segs: Vec<Seg>,
    /// No `/` in the pattern: it is matched against the file name, at any depth.
    by_name: bool,
    ci: bool,
}

/// A file glob: `*` and `?` (inside one name), `[a-z]` and `[!a-z]`, `{a,b}`, and `**` for any number of folders. A pattern without a `/` is matched against the file name at any depth
/// (and ignores case, as the name patterns of the file tools always did); one with a `/` or a `**` is matched against the path from the searched folder (ignoring case on Windows only).
/// `src/` means everything in `src`.
#[derive(Debug, Clone)]
pub struct Glob {
    alts: Vec<Alt>,
    source: String,
}

impl Glob {
    pub fn new(pattern: &str) -> Result<Glob, String> {
        let mut text = pattern.trim().to_string();
        // On Windows a backslash is the separator, as the model writes paths there.
        if cfg!(windows) {
            text = text.replace('\\', "/");
        }
        if text.is_empty() {
            return Err("the glob is empty".into());
        }
        let mut expanded = Vec::new();
        expand_braces(&text, &mut expanded)?;
        let alts = expanded.iter().map(|e| compile_alt(e)).collect::<Result<Vec<_>, _>>()?;
        Ok(Glob { alts, source: pattern.to_string() })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Does the file `name`, in the folder `head` (the components from the searched folder), match?
    pub fn is_match(&self, head: &[String], name: &str) -> bool {
        let view = PathView { head, last: name };
        self.alts.iter().any(|a| if a.by_name { matches!(a.segs.first(), Some(Seg::Pat(t)) if seg_match(t, name, a.ci)) } else { match_segs(&a.segs, &view, a.ci) })
    }

    /// `is_match` for a path written with `/`.
    pub fn is_match_path(&self, path: &str) -> bool {
        let mut parts: Vec<String> = path.split('/').filter(|p| !p.is_empty()).map(|p| p.to_string()).collect();
        match parts.pop() {
            Some(name) => self.is_match(&parts, &name),
            None => false,
        }
    }
}

fn compile_alt(e: &str) -> Result<Alt, String> {
    let mut t = e;
    while let Some(r) = t.strip_prefix("./") {
        t = r;
    }
    let t = t.trim_start_matches('/');
    let owned;
    // `src/` is everything in `src`.
    let t = if let Some(dir) = t.strip_suffix('/') {
        owned = format!("{dir}/**");
        owned.as_str()
    } else {
        t
    };
    if t.is_empty() {
        return Err("the glob names nothing".into());
    }
    let has_slash = t.contains('/');
    let mut segs: Vec<Seg> = Vec::new();
    for seg in t.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return Err("a glob cannot go up with '..': give a path inside the searched folder".into());
        }
        if seg == "**" {
            if !matches!(segs.last(), Some(Seg::Deep)) {
                segs.push(Seg::Deep);
            }
            continue;
        }
        segs.push(Seg::Pat(parse_segment(seg)?));
    }
    let by_name = !has_slash && segs.len() == 1 && matches!(segs[0], Seg::Pat(_));
    Ok(Alt { segs, by_name, ci: by_name || cfg!(windows) })
}

/// `{a,b}` made into the alternatives it stands for; braces without a comma are letters (`{id}.json`, as a shell has it); an unclosed one is an error.
fn expand_braces(pat: &str, out: &mut Vec<String>) -> Result<(), String> {
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0;
    let mut in_class = false;
    let mut open = None;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                i += 2;
                continue;
            }
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '{' if !in_class => {
                open = Some(i);
                break;
            }
            _ => {}
        }
        i += 1;
    }
    let Some(o) = open else {
        out.push(pat.to_string());
        if out.len() > MAX_GLOB_ALTS {
            return Err(format!("the glob has more than {MAX_GLOB_ALTS} alternatives; split it into several searches"));
        }
        return Ok(());
    };
    let (mut j, mut depth, mut close) = (o, 0usize, None);
    let mut commas = Vec::new();
    in_class = false;
    while j < chars.len() {
        match chars[j] {
            '\\' => {
                j += 2;
                continue;
            }
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '{' if !in_class => depth += 1,
            '}' if !in_class => {
                depth -= 1;
                if depth == 0 {
                    close = Some(j);
                    break;
                }
            }
            ',' if !in_class && depth == 1 => commas.push(j),
            _ => {}
        }
        j += 1;
    }
    let Some(c) = close else { return Err("a '{' has no closing '}'".into()) };
    let prefix: String = chars[..o].iter().collect();
    let suffix: String = chars[c + 1..].iter().collect();
    if commas.is_empty() {
        let inner: String = chars[o + 1..c].iter().collect();
        return expand_braces(&format!("{prefix}[{{]{inner}[}}]{suffix}"), out);
    }
    let mut starts = vec![o + 1];
    starts.extend(commas.iter().map(|k| k + 1));
    let mut ends = commas.clone();
    ends.push(c);
    for (s, e) in starts.into_iter().zip(ends) {
        let alt: String = chars[s..e].iter().collect();
        expand_braces(&format!("{prefix}{alt}{suffix}"), out)?;
    }
    Ok(())
}

/// One path segment (no `/` in it) made into tokens.
fn parse_segment(seg: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = seg.chars().collect();
    let mut toks: Vec<Tok> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                if !matches!(toks.last(), Some(Tok::Star)) {
                    toks.push(Tok::Star);
                }
                i += 1;
            }
            '?' => {
                toks.push(Tok::Any);
                i += 1;
            }
            '\\' => {
                let Some(&next) = chars.get(i + 1) else { return Err("a '\\' at the end of the glob".into()) };
                toks.push(Tok::Lit(next));
                i += 2;
            }
            '[' => {
                let mut j = i + 1;
                let negated = matches!(chars.get(j), Some('!') | Some('^'));
                if negated {
                    j += 1;
                }
                let mut items: Vec<(char, char)> = Vec::new();
                let mut first = true;
                loop {
                    let Some(&c) = chars.get(j) else { return Err(format!("a '[' has no closing ']' in \"{seg}\"")) };
                    if c == ']' && !first {
                        j += 1;
                        break;
                    }
                    first = false;
                    let lo = if c == '\\' {
                        j += 1;
                        *chars.get(j).ok_or_else(|| format!("a '\\' at the end of \"{seg}\""))?
                    } else {
                        c
                    };
                    j += 1;
                    if chars.get(j) == Some(&'-') && chars.get(j + 1).is_some_and(|&n| n != ']') {
                        let mut hi = chars[j + 1];
                        let mut step = 2;
                        if hi == '\\' {
                            hi = *chars.get(j + 2).ok_or_else(|| format!("a '\\' at the end of \"{seg}\""))?;
                            step = 3;
                        }
                        if hi < lo {
                            return Err(format!("the range {lo}-{hi} in \"{seg}\" runs backwards"));
                        }
                        items.push((lo, hi));
                        j += step;
                    } else {
                        items.push((lo, lo));
                    }
                }
                toks.push(Tok::Class { negated, items });
                i = j;
            }
            c => {
                toks.push(Tok::Lit(c));
                i += 1;
            }
        }
    }
    Ok(toks)
}

fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn upper(c: char) -> char {
    c.to_uppercase().next().unwrap_or(c)
}

fn char_eq(a: char, b: char, ci: bool) -> bool {
    a == b || (ci && lower(a) == lower(b))
}

fn class_has(negated: bool, items: &[(char, char)], c: char, ci: bool) -> bool {
    let hit = |x: char| items.iter().any(|&(lo, hi)| lo <= x && x <= hi);
    let found = hit(c) || (ci && (hit(lower(c)) || hit(upper(c))));
    found != negated
}

/// Does one path segment match the tokens? (`*` backtracks to the last star; no allocation.)
fn seg_match(toks: &[Tok], text: &str, ci: bool) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    loop {
        if ti >= text.len() {
            return toks[pi.min(toks.len())..].iter().all(|t| matches!(t, Tok::Star));
        }
        let c = text[ti..].chars().next().unwrap_or('\0');
        let step = c.len_utf8();
        match toks.get(pi) {
            Some(Tok::Star) => {
                star = Some((pi, ti));
                pi += 1;
                continue;
            }
            Some(Tok::Any) => {
                pi += 1;
                ti += step;
                continue;
            }
            Some(Tok::Lit(l)) if char_eq(*l, c, ci) => {
                pi += 1;
                ti += step;
                continue;
            }
            Some(Tok::Class { negated, items }) if class_has(*negated, items, c, ci) => {
                pi += 1;
                ti += step;
                continue;
            }
            _ => {}
        }
        // No match here: the last star takes one more character.
        match star {
            Some((sp, st)) => {
                let sc = text[st..].chars().next().unwrap_or('\0');
                let st = st + sc.len_utf8();
                star = Some((sp, st));
                pi = sp + 1;
                ti = st;
            }
            None => return false,
        }
    }
}

/// A path as the components of a folder and one name.
struct PathView<'a> {
    head: &'a [String],
    last: &'a str,
}

impl PathView<'_> {
    fn len(&self) -> usize {
        self.head.len() + 1
    }

    fn at(&self, i: usize) -> &str {
        if i < self.head.len() {
            &self.head[i]
        } else {
            self.last
        }
    }
}

/// Does the whole path match the segments? `**` in the middle may match no folder at all; at the end it must match at least one segment (`src/**` is what is in `src`).
fn match_segs(segs: &[Seg], p: &PathView<'_>, ci: bool) -> bool {
    let n = p.len();
    let mut reach = vec![false; n + 1];
    reach[0] = true;
    let mut next = vec![false; n + 1];
    for (i, seg) in segs.iter().enumerate() {
        next.iter_mut().for_each(|x| *x = false);
        match seg {
            Seg::Pat(toks) => {
                for j in 0..n {
                    if reach[j] && seg_match(toks, p.at(j), ci) {
                        next[j + 1] = true;
                    }
                }
            }
            Seg::Deep => {
                let last = i + 1 == segs.len();
                let mut seen = false;
                for j in 0..=n {
                    if last {
                        if j > 0 {
                            seen |= reach[j - 1];
                        }
                    } else {
                        seen |= reach[j];
                    }
                    next[j] = seen;
                }
            }
        }
        std::mem::swap(&mut reach, &mut next);
        if !reach.iter().any(|&b| b) {
            return false;
        }
    }
    reach[n]
}

// ---- ignore files -----------------------------------------------------------------------------------------------------------

/// One line of an ignore file.
#[derive(Debug, Clone)]
struct Rule {
    segs: Vec<Seg>,
    /// The pattern had a `/` in it (other than at its end): it is matched against the path from the ignore file's folder; otherwise against the name, at any depth below.
    anchored: bool,
    dir_only: bool,
    negated: bool,
}

#[derive(Debug)]
struct IgnoreFile {
    /// How many components below the allowed folder the file's own folder is.
    base_len: usize,
    rules: Vec<Rule>,
    ci: bool,
}

/// The subset of the `.gitignore` language that is read: blank lines and `#` comments, `!` to take a name back, a trailing `/` for folders only, a `/` inside for a path from the file's
/// folder, `*` `?` `[a-z]` `**`, and `\` to make a character a letter. It is not all of git (no `core.excludesFile`, no `info/attributes`), and a line it cannot read is skipped.
fn parse_ignore(text: &str) -> Vec<Rule> {
    let mut rules = Vec::new();
    for raw in text.trim_start_matches('\u{feff}').lines() {
        if rules.len() >= IGNORE_RULES_MAX {
            break;
        }
        let mut line = raw.to_string();
        // Trailing spaces go, unless one is escaped.
        while line.ends_with(' ') && !line.ends_with("\\ ") {
            line.pop();
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let negated = line.starts_with('!');
        if negated {
            line.remove(0);
        }
        let dir_only = line.ends_with('/') && !line.ends_with("\\/");
        if dir_only {
            line.pop();
        }
        let line = line.trim_start_matches("./").to_string();
        let anchored = line.contains('/');
        let line = line.trim_start_matches('/');
        if line.is_empty() {
            continue;
        }
        let mut segs: Vec<Seg> = Vec::new();
        let mut broken = false;
        for seg in line.split('/') {
            if seg.is_empty() {
                continue;
            }
            if seg == "**" {
                if !matches!(segs.last(), Some(Seg::Deep)) {
                    segs.push(Seg::Deep);
                }
                continue;
            }
            match parse_segment(seg) {
                Ok(t) => segs.push(Seg::Pat(t)),
                Err(_) => {
                    broken = true;
                    break;
                }
            }
        }
        if broken || segs.is_empty() {
            continue;
        }
        rules.push(Rule { segs, anchored, dir_only, negated });
    }
    rules
}

fn rule_hits(rule: &Rule, ci: bool, below: &[String], name: &str, is_dir: bool) -> bool {
    if rule.dir_only && !is_dir {
        return false;
    }
    if rule.anchored {
        return match_segs(&rule.segs, &PathView { head: below, last: name }, ci);
    }
    match rule.segs.as_slice() {
        [Seg::Pat(t)] => seg_match(t, name, ci),
        [Seg::Deep] => true,
        _ => false,
    }
}

/// What the ignore files say about an entry: `Some(true)` ignored, `Some(false)` taken back, `None` nothing. The deepest file speaks first, and in a file the last matching line wins.
fn decision(layers: &[Arc<IgnoreFile>], rel: &[String], name: &str, is_dir: bool) -> Option<bool> {
    for layer in layers.iter().rev() {
        let below = &rel[layer.base_len.min(rel.len())..];
        for rule in layer.rules.iter().rev() {
            if rule_hits(rule, layer.ci, below, name, is_dir) {
                return Some(!rule.negated);
            }
        }
    }
    None
}

/// The text of an ignore file, when it is a plain file (not a link: a link could lead out of the allowed folder) of a size that is read.
fn read_ignore(path: &Path) -> Option<String> {
    let m = std::fs::symlink_metadata(path).ok()?;
    if !m.is_file() || m.len() > IGNORE_FILE_MAX {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The ignore files of one folder as layers, lowest first (`.git/info/exclude`, `.gitignore`, `.ignore`). A file with no rule in it is no layer: a comment is not an ignore rule.
fn layers_here(dir: &Path, base_len: usize, gitignore: bool, ignore: bool, git_dir: bool) -> Vec<Arc<IgnoreFile>> {
    let mut v = Vec::new();
    let mut add = |path: PathBuf| {
        if let Some(text) = read_ignore(&path) {
            let rules = parse_ignore(&text);
            if !rules.is_empty() {
                v.push(Arc::new(IgnoreFile { base_len, rules, ci: cfg!(windows) }));
            }
        }
    };
    if git_dir {
        add(dir.join(".git").join("info").join("exclude"));
    }
    if gitignore {
        add(dir.join(".gitignore"));
    }
    if ignore {
        add(dir.join(".ignore"));
    }
    v
}

fn is_plain_file(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file())
}

// ---- the walk ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Level by level, and by name in a folder.
    Breadth,
    /// Files only, newest first (what changed lately). The whole walk is read before the first file is handed over.
    MtimeDesc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Link,
}

/// What the walk hands to the caller for each entry.
pub struct Entry<'a> {
    pub path: &'a Path,
    /// The path from the searched folder, written with `/`.
    pub rel: &'a str,
    pub name: &'a str,
    pub kind: Kind,
    pub meta: &'a Metadata,
}

/// What the caller says about a file it was handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visit {
    /// It was looked at (counted in `searched` and its size in `bytes`).
    Searched,
    /// It was too big to open (counted as left out).
    Big,
    /// It looked like a binary file (counted as left out).
    Binary,
    /// It could not be opened, or turned out not to be where it was met (counted as left out).
    Unreadable,
    /// The caller has what it needs: the walk ends.
    Stop,
}

#[derive(Debug, Clone)]
pub struct WalkOpts {
    pub include_hidden: bool,
    /// Also what `.gitignore` and the like hide, and the folders of [`JUNK_DIRS`].
    pub include_ignored: bool,
    /// Also look inside `.git` (it is never looked into otherwise, whatever `include_hidden` says).
    pub include_vcs: bool,
    /// How many levels: 1 is the entries of the searched folder itself.
    pub max_depth: usize,
    /// The most files handed over; the next one stops the walk (`budget_hit`).
    pub max_files: usize,
    /// The most bytes of files that were searched; passing it stops the walk (`budget_hit`).
    pub max_bytes: u64,
    /// Also hand over folders and links (a listing); otherwise only files.
    pub emit_dirs: bool,
    /// Leave out files whose name the guard calls a secret (a search of contents), and count them.
    pub skip_sensitive: bool,
    pub glob: Option<Glob>,
    pub order: Order,
}

impl Default for WalkOpts {
    fn default() -> Self {
        WalkOpts { include_hidden: false, include_ignored: false, include_vcs: false, max_depth: 8, max_files: 20_000, max_bytes: u64::MAX, emit_dirs: false, skip_sensitive: false, glob: None, order: Order::Breadth }
    }
}

/// What a walk met and what it left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalkStats {
    /// Files met (not hidden, ignored or protected), before the glob.
    pub visited: usize,
    /// Files the glob let through, handed to the caller.
    pub matched: usize,
    /// Files the caller looked at.
    pub searched: usize,
    /// The size of what was searched.
    pub bytes: u64,
    /// Entries `.gitignore`, `.ignore`, `.git/info/exclude` or the junk list hid (a folder counts once).
    pub skipped_ignored: usize,
    /// Entries whose name starts with a dot (a folder counts once).
    pub skipped_hidden: usize,
    pub skipped_big: usize,
    pub skipped_binary: usize,
    pub skipped_sensitive: usize,
    /// Aqua's own files and folders and the credential folders (a folder counts once).
    pub skipped_protected: usize,
    /// Folders or entries the system would not show.
    pub unreadable: usize,
    /// Files the glob said no to.
    pub glob_rejected: usize,
    /// The file or byte budget ran out.
    pub budget_hit: bool,
    /// The caller ended the walk.
    pub stopped: bool,
}

impl WalkStats {
    /// Everything left out for a reason the person can change or should know (not the files a glob said no to).
    pub fn left_out(&self) -> usize {
        self.skipped_ignored + self.skipped_hidden + self.skipped_big + self.skipped_binary + self.skipped_sensitive + self.skipped_protected + self.unreadable
    }

    /// The counts as JSON, for the `data` of a result.
    pub fn counts(&self) -> serde_json::Value {
        serde_json::json!({
            "ignored": self.skipped_ignored, "hidden": self.skipped_hidden, "too_big": self.skipped_big, "binary": self.skipped_binary,
            "sensitive": self.skipped_sensitive, "protected": self.skipped_protected, "unreadable": self.unreadable,
        })
    }

    /// The line that ends a result: how many files were looked at and what was left out, so that "No matches." says what it covered. `big_limit` is the size above which a file is not opened.
    /// With `with_searched` false (a listing) nothing is said when nothing was left out.
    pub fn footer(&self, big_limit: u64, with_searched: bool) -> Option<String> {
        let mut left: Vec<String> = Vec::new();
        let mut push = |n: usize, what: String| {
            if n > 0 {
                left.push(format!("{n} {what}"));
            }
        };
        push(self.skipped_ignored, "ignored".into());
        push(self.skipped_hidden, "hidden".into());
        push(self.skipped_big, format!("over {}", human(big_limit)));
        push(self.skipped_binary, "binary".into());
        push(self.skipped_sensitive, if self.skipped_sensitive == 1 { "sensitive name".into() } else { "sensitive names".into() });
        push(self.skipped_protected, if self.skipped_protected == 1 { "protected entry not searched".into() } else { "protected entries not searched".into() });
        push(self.unreadable, "unreadable".into());
        if !with_searched && left.is_empty() {
            return None;
        }
        let mut text = String::from("[");
        if with_searched {
            text.push_str(&format!("searched {} file{}", self.searched, if self.searched == 1 { "" } else { "s" }));
            if self.bytes > 0 {
                text.push_str(&format!(" ({})", human(self.bytes)));
            }
            text.push_str(if left.is_empty() { "." } else { "; " });
        }
        if !left.is_empty() {
            text.push_str(&format!("left out: {}.", left.join(", ")));
        }
        if self.skipped_ignored > 0 {
            text.push_str(" include_ignored=true also looks in what .gitignore hides.");
        }
        if self.skipped_hidden > 0 {
            text.push_str(" include_hidden=true also shows names that start with a dot.");
        }
        text.push(']');
        Some(text)
    }
}

/// A size in the units a person reads (`1 MiB`).
pub fn human(bytes: u64) -> String {
    const KIB: u64 = 1 << 10;
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        if bytes % MIB == 0 {
            format!("{} MiB", bytes / MIB)
        } else {
            format!("{:.1} MiB", bytes as f64 / MIB as f64)
        }
    } else if bytes >= KIB {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{bytes} B")
    }
}

struct Frame<'g> {
    dir: PathBuf,
    gate: DirGate<'g>,
    layers: Arc<Vec<Arc<IgnoreFile>>>,
    depth: usize,
}

struct Pending {
    path: PathBuf,
    rel: String,
    name: String,
    meta: Metadata,
}

struct Run<'a> {
    o: &'a WalkOpts,
    st: WalkStats,
    f: &'a mut dyn FnMut(&Entry<'_>) -> Visit,
    pending: Vec<Pending>,
    /// How many components the searched folder is below its root: what a glob is matched from.
    start_len: usize,
}

fn join_rel(head: &[String], name: &str) -> String {
    let mut s = String::new();
    for h in head {
        s.push_str(h);
        s.push('/');
    }
    s.push_str(name);
    s
}

impl Run<'_> {
    /// An entry that passed the filters: the glob, the secret names, the budget, and then the caller. `false` ends the walk.
    fn deliver(&mut self, g: &PathGuard, e: &std::fs::DirEntry, name: &str, kind: Kind, rel_dir: &[String]) -> bool {
        let o = self.o;
        let below = &rel_dir[self.start_len.min(rel_dir.len())..];
        if kind == Kind::File {
            self.st.visited += 1;
            if self.st.visited > o.max_files.saturating_mul(VISIT_FACTOR) {
                self.st.budget_hit = true;
                return false;
            }
        }
        if let Some(glob) = &o.glob {
            if !glob.is_match(below, name) {
                if kind == Kind::File {
                    self.st.glob_rejected += 1;
                }
                return true;
            }
        }
        if kind == Kind::File {
            if o.skip_sensitive && g.is_sensitive_name(name) && !g.in_out_root(&e.path()) {
                self.st.skipped_sensitive += 1;
                return true;
            }
            self.st.matched += 1;
            if self.st.matched > o.max_files {
                self.st.budget_hit = true;
                return false;
            }
        }
        let Ok(meta) = e.metadata() else {
            self.st.unreadable += 1;
            return true;
        };
        let path = e.path();
        let rel = join_rel(below, name);
        if o.order == Order::MtimeDesc && kind == Kind::File {
            self.pending.push(Pending { path, rel, name: name.to_string(), meta });
            return true;
        }
        self.give(&path, &rel, name, kind, &meta)
    }

    fn give(&mut self, path: &Path, rel: &str, name: &str, kind: Kind, meta: &Metadata) -> bool {
        let entry = Entry { path, rel, name, kind, meta };
        match (self.f)(&entry) {
            Visit::Stop => {
                self.st.stopped = true;
                false
            }
            Visit::Searched => {
                if kind == Kind::File {
                    self.st.searched += 1;
                    self.st.bytes += meta.len();
                    if self.st.bytes > self.o.max_bytes {
                        self.st.budget_hit = true;
                        return false;
                    }
                }
                true
            }
            Visit::Big => {
                self.st.skipped_big += 1;
                true
            }
            Visit::Binary => {
                self.st.skipped_binary += 1;
                true
            }
            Visit::Unreadable => {
                self.st.unreadable += 1;
                true
            }
        }
    }
}

/// Is `name` a folder of [`JUNK_DIRS`]?
pub fn is_junk_dir(name: &str) -> bool {
    JUNK_DIRS.iter().any(|j| name.eq_ignore_ascii_case(j))
}

/// Is the entry hidden by an ignore file, or (when no ignore rule applies) by the junk list?
fn ignored(layers: &[Arc<IgnoreFile>], rel: &[String], name: &str, is_dir: bool) -> bool {
    match decision(layers, rel, name, is_dir) {
        Some(hide) => hide,
        None => layers.is_empty() && is_dir && is_junk_dir(name),
    }
}

/// Walk the folder `start` (canonical, as `PathGuard::resolve` gives it) level by level and hand each entry that passes the filters to `f`. See the module's description for what is left out;
/// `cancel` is looked at for every entry.
pub fn walk(g: &PathGuard, start: &Path, o: &WalkOpts, cancel: &AtomicBool, f: &mut dyn FnMut(&Entry<'_>) -> Visit) -> WalkStats {
    let mut run = Run { o, st: WalkStats::default(), f, pending: Vec::new(), start_len: 0 };
    let Some(gate) = g.walk_gate(start) else { return run.st };
    run.start_len = gate.rel().len();
    // The ignore files of the folders above the start, inside the allowed folder, the allowed folder's first.
    let mut layers: Vec<Arc<IgnoreFile>> = Vec::new();
    if !o.include_ignored {
        let above: Vec<&Path> = start.ancestors().skip(1).take(run.start_len).collect();
        for (base_len, dir) in above.iter().rev().enumerate() {
            let git_dir = std::fs::symlink_metadata(dir.join(".git")).is_ok_and(|m| m.is_dir());
            layers.extend(layers_here(dir, base_len, is_plain_file(&dir.join(".gitignore")), is_plain_file(&dir.join(".ignore")), git_dir));
        }
    }
    let mut queue: VecDeque<Frame<'_>> = VecDeque::new();
    queue.push_back(Frame { dir: start.to_path_buf(), gate, layers: Arc::new(layers), depth: 1 });
    'frames: while let Some(frame) = queue.pop_front() {
        let rd = match std::fs::read_dir(&frame.dir) {
            Ok(rd) => rd,
            Err(_) => {
                run.st.unreadable += 1;
                continue;
            }
        };
        let mut items: Vec<(String, std::fs::DirEntry)> = rd.flatten().map(|e| (e.file_name().to_string_lossy().to_string(), e)).collect();
        items.sort_by_cached_key(|(n, _)| (n.to_lowercase(), n.clone()));
        let mut chain = frame.layers.clone();
        if !o.include_ignored {
            let has_file = |n: &str| items.iter().any(|(name, e)| name.eq_ignore_ascii_case(n) && e.file_type().is_ok_and(|t| t.is_file()));
            let git_dir = items.iter().any(|(name, e)| name == ".git" && e.file_type().is_ok_and(|t| t.is_dir()));
            let here = layers_here(&frame.dir, frame.gate.rel().len(), has_file(".gitignore"), has_file(".ignore"), git_dir);
            if !here.is_empty() {
                let mut v = (*chain).clone();
                v.extend(here);
                chain = Arc::new(v);
            }
        }
        for (name, e) in &items {
            if cancel.load(Ordering::SeqCst) {
                run.st.stopped = true;
                break 'frames;
            }
            let Ok(ft) = e.file_type() else {
                run.st.unreadable += 1;
                continue;
            };
            if frame.gate.check(name).is_some() {
                run.st.skipped_protected += 1;
                continue;
            }
            if !o.include_vcs && name.eq_ignore_ascii_case(".git") {
                continue;
            }
            if !o.include_hidden && name.starts_with('.') {
                run.st.skipped_hidden += 1;
                continue;
            }
            let is_link = ft.is_symlink();
            let is_dir = ft.is_dir() && !is_link;
            if !o.include_ignored && ignored(&chain, frame.gate.rel(), name, is_dir) {
                run.st.skipped_ignored += 1;
                continue;
            }
            if is_link {
                // Never followed; a listing shows that it is there.
                if o.emit_dirs && !run.deliver(g, e, name, Kind::Link, frame.gate.rel()) {
                    break 'frames;
                }
            } else if is_dir {
                if o.emit_dirs && !run.deliver(g, e, name, Kind::Dir, frame.gate.rel()) {
                    break 'frames;
                }
                if frame.depth < o.max_depth {
                    queue.push_back(Frame { dir: e.path(), gate: frame.gate.enter(name), layers: chain.clone(), depth: frame.depth + 1 });
                }
            } else if ft.is_file() {
                if !run.deliver(g, e, name, Kind::File, frame.gate.rel()) {
                    break 'frames;
                }
            }
        }
    }
    if !run.pending.is_empty() && !cancel.load(Ordering::SeqCst) {
        let mut pending = std::mem::take(&mut run.pending);
        pending.sort_by(|a, b| b.meta.modified().ok().cmp(&a.meta.modified().ok()).then_with(|| a.rel.cmp(&b.rel)));
        for p in pending {
            if !run.give(&p.path, &p.rel, &p.name, Kind::File, &p.meta) {
                break;
            }
        }
    }
    run.st
}

/// The size of a folder as the file tools may see it: everything below it except what a walk may not look at (Aqua's own files, credential folders), and how many such entries were left out.
pub fn guarded_dir_size(g: &PathGuard, dir: &Path, cancel: &AtomicBool) -> (u64, usize) {
    let o = WalkOpts { include_hidden: true, include_ignored: true, include_vcs: true, max_depth: usize::MAX, max_files: usize::MAX / VISIT_FACTOR, ..WalkOpts::default() };
    let st = walk(g, dir, &o, cancel, &mut |_| Visit::Searched);
    (st.bytes, st.skipped_protected)
}

/// How many entries of the folder `dir` a walk may not look at (Aqua's own files, credential folders), and whether the walk was cut short before it could say. `.git` is not looked into:
/// its `config` stays refused wherever the folder is put (`resolve` reads the path, and the path keeps `.git/config`).
pub fn protected_inside(g: &PathGuard, dir: &Path, cancel: &AtomicBool) -> (usize, bool) {
    let o = WalkOpts { include_hidden: true, include_ignored: true, max_depth: usize::MAX, max_files: usize::MAX / VISIT_FACTOR, emit_dirs: false, ..WalkOpts::default() };
    let st = walk(g, dir, &o, cancel, &mut |_| Visit::Searched);
    (st.skipped_protected, st.budget_hit || st.stopped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::guard::FsConfig;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_walk_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn guard(root: &Path, protected: &[&str]) -> PathGuard {
        PathGuard::new(FsConfig::with_workspace(root.to_string_lossy().to_string()), protected.iter().map(|p| root.join(p)).collect()).unwrap()
    }

    fn found(g: &PathGuard, root: &Path, o: &WalkOpts) -> (Vec<String>, WalkStats) {
        let start = crate::fs::guard::canonical(root).unwrap();
        let mut names = Vec::new();
        let st = walk(g, &start, o, &AtomicBool::new(false), &mut |e| {
            names.push(e.rel.to_string());
            Visit::Searched
        });
        (names, st)
    }

    fn glob(p: &str) -> Glob {
        Glob::new(p).unwrap_or_else(|e| panic!("{p}: {e}"))
    }

    #[test]
    fn a_pattern_without_a_slash_is_a_file_name_at_any_depth_and_ignores_case() {
        let g = glob("*.RS");
        assert!(g.is_match_path("a.rs") && g.is_match_path("src/deep/b.Rs") && !g.is_match_path("src/a.rs.txt"));
        assert!(glob("test_?.py").is_match_path("x/test_1.py") && !glob("test_?.py").is_match_path("x/test_12.py"));
        assert!(glob("[a-c]*.txt").is_match_path("b1.txt") && !glob("[a-c]*.txt").is_match_path("d1.txt"));
        assert!(glob("[!a-c]*.txt").is_match_path("d1.txt") && !glob("[!a-c]*.txt").is_match_path("a1.txt"));
    }

    #[test]
    fn a_pattern_with_a_slash_or_two_stars_is_matched_against_the_path() {
        let g = glob("src/**/*.ts");
        assert!(g.is_match_path("src/a.ts") && g.is_match_path("src/x/y/a.ts") && !g.is_match_path("lib/a.ts") && !g.is_match_path("a.ts") && !g.is_match_path("other/src/a.ts"));
        assert!(glob("**/test_*.py").is_match_path("test_a.py") && glob("**/test_*.py").is_match_path("a/b/test_a.py") && !glob("**/test_*.py").is_match_path("a/b/a_test.py"));
        assert!(glob("src/*.ts").is_match_path("src/a.ts") && !glob("src/*.ts").is_match_path("src/x/a.ts"), "a single star stays inside one folder");
        assert!(glob("src/**").is_match_path("src/a/b.txt") && !glob("src/**").is_match_path("src"), "src/** is what is in src");
        assert!(glob("src/").is_match_path("src/a.txt") && glob("./src/*.ts").is_match_path("src/a.ts") && glob("/src/*.ts").is_match_path("src/a.ts"));
        assert!(glob("a/**/z.txt").is_match_path("a/z.txt") && glob("a/**/z.txt").is_match_path("a/b/c/z.txt"));
    }

    #[test]
    fn braces_make_alternatives_and_braces_without_a_comma_are_letters() {
        let g = glob("*.{ts,tsx}");
        assert!(g.is_match_path("a.ts") && g.is_match_path("x/a.tsx") && !g.is_match_path("a.js"));
        let g = glob("{src,lib}/**/*.{rs,toml}");
        assert!(g.is_match_path("src/a/b.rs") && g.is_match_path("lib/Cargo.toml") && !g.is_match_path("tests/a.rs"));
        assert!(glob("{id}.json").is_match_path("{id}.json") && !glob("{id}.json").is_match_path("id.json"));
        assert!(glob("a{b{c,d}e,f}g").is_match_path("abceg") && glob("a{b{c,d}e,f}g").is_match_path("abdeg") && !glob("a{b{c,d}e,f}g").is_match_path("adeg") && glob("a{b{c,d}e,f}g").is_match_path("afg"));
    }

    #[test]
    fn a_glob_that_cannot_be_read_says_why() {
        for (bad, why) in [("*.{ts,tsx", "closing '}'"), ("a[b", "closing ']'"), ("a[z-b]", "backwards"), ("../x", ".."), ("", "empty")] {
            let e = Glob::new(bad).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
        if !cfg!(windows) {
            assert!(Glob::new("ab\\").unwrap_err().contains("end"));
        }
        let many = "{a,b}".repeat(7);
        assert!(Glob::new(&many).unwrap_err().contains("alternatives"));
    }

    #[test]
    fn the_names_of_a_gitignore_are_read_as_git_reads_them() {
        let rules = parse_ignore("# a comment\n\n*.log\n!keep.log\n/target\ndocs/*.tmp\nbuild/\n**/cache\nlogs/**\n\\#hash\ntrailing   \n");
        let layer = vec![Arc::new(IgnoreFile { base_len: 0, rules, ci: false })];
        let dir = |rel: &[&str], name: &str, d: bool| decision(&layer, &rel.iter().map(|s| s.to_string()).collect::<Vec<_>>(), name, d);
        assert_eq!(dir(&[], "a.log", false), Some(true));
        assert_eq!(dir(&["x", "y"], "a.log", false), Some(true), "no slash: at any depth");
        assert_eq!(dir(&[], "keep.log", false), Some(false), "a name taken back");
        assert_eq!(dir(&[], "target", true), Some(true));
        assert_eq!(dir(&["sub"], "target", true), None, "a leading slash anchors to the file's folder");
        assert_eq!(dir(&["docs"], "x.tmp", false), Some(true));
        assert_eq!(dir(&["other"], "x.tmp", false), None, "a slash inside anchors it too");
        assert_eq!(dir(&["a"], "build", true), Some(true));
        assert_eq!(dir(&["a"], "build", false), None, "a trailing slash is for folders only");
        assert_eq!(dir(&["a", "b"], "cache", true), Some(true));
        assert_eq!(dir(&["logs"], "x", false), Some(true));
        assert_eq!(dir(&[], "#hash", false), Some(true), "an escaped # is a letter");
        assert_eq!(dir(&[], "trailing", false), Some(true), "trailing spaces go");
        assert_eq!(dir(&[], "src", true), None);
    }

    #[test]
    fn a_huge_ignore_file_keeps_its_first_two_thousand_rules() {
        let text: String = (0..IGNORE_RULES_MAX + 500).map(|i| format!("name{i}\n")).collect();
        assert_eq!(parse_ignore(&text).len(), IGNORE_RULES_MAX);
    }

    #[test]
    fn a_deeper_ignore_file_speaks_before_a_higher_one() {
        let high = Arc::new(IgnoreFile { base_len: 0, rules: parse_ignore("*.gen\n"), ci: false });
        let low = Arc::new(IgnoreFile { base_len: 1, rules: parse_ignore("!keep.gen\nsecret/\n"), ci: false });
        let layers = vec![high, low];
        let rel = vec!["pkg".to_string()];
        assert_eq!(decision(&layers, &rel, "keep.gen", false), Some(false));
        assert_eq!(decision(&layers, &rel, "other.gen", false), Some(true));
        assert_eq!(decision(&layers, &rel, "secret", true), Some(true));
    }

    #[test]
    fn a_walk_goes_level_by_level_and_names_what_it_left_out() {
        let root = temp("levels");
        put(&root, "a/deep/deeper/z.txt", "z");
        put(&root, "a/b.txt", "b");
        put(&root, "c.txt", "c");
        put(&root, ".hidden/h.txt", "h");
        put(&root, "node_modules/m/x.js", "x");
        put(&root, "data/token.txt", "t");
        let g = guard(&root, &["data"]);
        let (names, st) = found(&g, &root, &WalkOpts::default());
        assert_eq!(names, ["c.txt", "a/b.txt", "a/deep/deeper/z.txt"], "files near the top come first");
        assert_eq!((st.skipped_hidden, st.skipped_ignored, st.skipped_protected, st.searched), (1, 1, 1, 3));
        let (all, st) = found(&g, &root, &WalkOpts { include_hidden: true, include_ignored: true, ..WalkOpts::default() });
        assert!(all.contains(&".hidden/h.txt".to_string()) && all.contains(&"node_modules/m/x.js".to_string()));
        assert!(!all.iter().any(|n| n.starts_with("data/")), "Aqua's own folder is never entered: {all:?}");
        assert_eq!(st.skipped_protected, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_gitignore_hides_what_it_names_a_nested_one_takes_names_back_and_include_ignored_shows_all() {
        let root = temp("gitignore");
        put(&root, ".gitignore", "generated/\n*.log\n");
        put(&root, "generated/a.rs", "a");
        put(&root, "src/b.rs", "b");
        put(&root, "src/run.log", "l");
        put(&root, "src/.gitignore", "!run.log\n");
        put(&root, "vendor/v.rs", "v");
        put(&root, "top.log", "l");
        let g = guard(&root, &[]);
        let (names, st) = found(&g, &root, &WalkOpts::default());
        assert!(names.contains(&"src/b.rs".to_string()) && names.contains(&"vendor/v.rs".to_string()));
        assert!(names.contains(&"src/run.log".to_string()), "the nested file took the name back: {names:?}");
        assert!(!names.iter().any(|n| n.starts_with("generated/") || n == "top.log"), "{names:?}");
        assert_eq!(st.skipped_ignored, 2);
        let (everything, st) = found(&g, &root, &WalkOpts { include_ignored: true, ..WalkOpts::default() });
        assert!(everything.contains(&"generated/a.rs".to_string()) && everything.contains(&"top.log".to_string()));
        assert_eq!(st.skipped_ignored, 0);
        // Started below the ignore file, the folders above still apply.
        let (below, _) = found(&g, &root.join("src"), &WalkOpts::default());
        assert!(below.contains(&"run.log".to_string()) && below.contains(&"b.rs".to_string()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn with_no_ignore_rule_the_junk_list_applies_but_not_to_vendor() {
        let root = temp("junk");
        put(&root, "node_modules/p/i.js", "i");
        put(&root, "target/debug/x.o", "x");
        put(&root, "vendor/lib/v.rs", "v");
        put(&root, "src/real.rs", "r");
        put(&root, "build", "a file called build is not a folder");
        let g = guard(&root, &[]);
        let (names, st) = found(&g, &root, &WalkOpts::default());
        assert_eq!(names.len(), 3, "{names:?}");
        assert!(names.contains(&"vendor/lib/v.rs".to_string()) && names.contains(&"src/real.rs".to_string()) && names.contains(&"build".to_string()));
        assert_eq!(st.skipped_ignored, 2);
        // A comment-only ignore file is no ignore rule: the junk list stays.
        put(&root, ".gitignore", "# nothing here\n");
        assert_eq!(found(&g, &root, &WalkOpts::default()).1.skipped_ignored, 2);
        // A real rule replaces the list: the repository says for itself.
        put(&root, ".gitignore", "*.o\n");
        let (names, _) = found(&g, &root, &WalkOpts::default());
        assert!(names.contains(&"node_modules/p/i.js".to_string()), "the repository's own file decides: {names:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_budget_is_spent_on_the_top_of_the_tree_first() {
        let root = temp("budget");
        for i in 0..60 {
            put(&root, &format!("gen/a/b/c/f{i:03}.txt"), "j");
        }
        put(&root, "zz/real.rs", "r");
        let g = guard(&root, &[]);
        let o = WalkOpts { max_files: 10, ..WalkOpts::default() };
        let (names, st) = found(&g, &root, &o);
        assert!(names.contains(&"zz/real.rs".to_string()), "the shallow file is met before the deep junk: {names:?}");
        assert!(st.budget_hit && names.len() == 10);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_glob_gates_the_files_and_the_newest_come_first_when_asked() {
        let root = temp("mtime");
        put(&root, "old.rs", "o");
        std::thread::sleep(std::time::Duration::from_millis(60));
        put(&root, "sub/mid.rs", "m");
        std::thread::sleep(std::time::Duration::from_millis(60));
        put(&root, "new.rs", "n");
        put(&root, "x.txt", "t");
        let g = guard(&root, &[]);
        let o = WalkOpts { glob: Some(glob("*.rs")), order: Order::MtimeDesc, ..WalkOpts::default() };
        let (names, st) = found(&g, &root, &o);
        assert_eq!(names, ["new.rs", "sub/mid.rs", "old.rs"]);
        assert_eq!(st.glob_rejected, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_credential_folder_is_not_entered_and_the_count_says_so() {
        let root = temp("credential");
        put(&root, ".docker/config.json", "{}");
        put(&root, ".config/gcloud/creds.json", "{}");
        put(&root, ".config/other/x.json", "{}");
        put(&root, "proj/.ssh/id", "k");
        put(&root, "proj/a.txt", "a");
        let g = guard(&root, &[]);
        let (names, st) = found(&g, &root, &WalkOpts { include_hidden: true, include_ignored: true, ..WalkOpts::default() });
        assert_eq!(st.skipped_protected, 3, "{names:?}");
        assert!(names.contains(&".config/other/x.json".to_string()) && names.contains(&"proj/a.txt".to_string()));
        assert!(!names.iter().any(|n| n.contains("gcloud") || n.contains(".docker") || n.contains(".ssh")), "{names:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_footer_names_each_count_and_says_nothing_about_a_listing_that_left_nothing_out() {
        let st = WalkStats { searched: 18402, bytes: 141 << 20, skipped_ignored: 3210, skipped_big: 12, skipped_binary: 9, skipped_sensitive: 1, skipped_protected: 2, ..WalkStats::default() };
        let f = st.footer(1 << 20, true).unwrap();
        assert!(f.starts_with("[searched 18402 files (141 MiB); left out: 3210 ignored, 12 over 1 MiB, 9 binary, 1 sensitive name, 2 protected entries not searched.") && f.contains("include_ignored=true"), "{f}");
        assert_eq!(WalkStats::default().footer(1 << 20, false), None);
        assert_eq!(WalkStats { searched: 1, ..WalkStats::default() }.footer(1 << 20, true).unwrap(), "[searched 1 file.]");
    }

    #[test]
    fn the_size_of_a_folder_leaves_out_what_a_walk_may_not_see() {
        let root = temp("size");
        put(&root, "a.txt", "12345");
        put(&root, "sub/b.txt", "123");
        put(&root, "data/secret.db", "123456789012345678901234567890");
        let g = guard(&root, &["data"]);
        let start = crate::fs::guard::canonical(&root).unwrap();
        assert_eq!(guarded_dir_size(&g, &start, &AtomicBool::new(false)), (8, 1));
        assert_eq!(protected_inside(&g, &start, &AtomicBool::new(false)), (1, false));
        let _ = std::fs::remove_dir_all(&root);
    }
}
