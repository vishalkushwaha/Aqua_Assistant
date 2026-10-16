//! Who imports whom, and where a name is used (card OA12-03; task CU-09 of the OpenCode audit).
//!
//! "Which files depend on this one" and "where is this name used" are the two questions that a text search answers badly, and "where should I look first" is the one a model with a
//! 6,144-token window cannot answer by reading. This is what `code.deps`, `code.refs` and `code.map` are made of. It is **a guess, not a build**: the imports are read from the shape of the
//! lines (the `use`, `mod`, `import`, `from ... import`, `require` and `import "path"` lines of Rust, TypeScript and JavaScript, Python and Go; no grammar crate is a dependency of this
//! build), and a name is resolved to a file by looking for the file in the folder that was read. So, and the output says so:
//!
//! * a **Rust** `use` is followed inside its own crate (`crate::`, `self::`, `super::`, a module it declares with `mod x;`, `a/b.rs` and `a/b/mod.rs`); another crate of the workspace, a
//!   `#[path = ".."]` module, a re-export (`pub use`: the file that holds the item is not the file that is named) and a macro that writes a `use` are not. Only `use` and `mod` lines that begin a
//!   line are read (`rustfmt` style), and an indented one only when it begins with `crate::` (it is absolute, so it means the same wherever it stands). A `crate::a::b` path written in code
//!   with no `use` for it counts too (a file that calls `crate::flags::is_on(..)` depends on `flags`), unless it is in a string or a comment.
//! * a **TypeScript or JavaScript** specifier is followed when it is relative (`./a`, `../b`, with the extension, the other extensions and `index` tried); an alias (`@/x`, `~/x`, `#x`, a
//!   `tsconfig` path) and a package name are not (a package is counted as outside the folder).
//! * a **Python** import is followed from the folder of the file and each folder above it (a source folder is not looked for), and a relative one by its dots.
//! * a **Go** import is followed through the `go.mod` of the folder: every file of the package, except tests, is imported (the files of one package are not linked to each other).
//!
//! Everything is bounded: a file is read to [`MAX_LINES`] lines of at most [`MAX_LINE_BYTES`] bytes, a statement that goes over several lines is followed for at most 40 lines and all of them
//! together for no more than three times the size of the file (and a little over), a file names at most [`MAX_IMPORTS_PER_FILE`] things, and a graph keeps at most [`MAX_EDGES`] edges. The
//! ranking runs 20 rounds whatever the graph, so a cycle cannot make it run on. Nothing here reads a file or runs anything: the caller gives the text (see [`super::provider`]).

use regex::Regex;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::OnceLock;

/// The most lines of a file that are looked at (the same as the outline's).
pub const MAX_LINES: usize = super::symbols::MAX_LINES;
/// A line longer than this is not looked at for an import (minified code).
pub const MAX_LINE_BYTES: usize = 500;
/// A line longer than this is not looked at for a name (a line of real code can be longer than the 500 bytes an import line is: a string of text, a table). `fs.read` cuts a line at the same size.
pub const MAX_REF_LINE_BYTES: usize = 4_096;
/// The most imports kept for one file.
pub const MAX_IMPORTS_PER_FILE: usize = 400;
/// The most files a graph is made of, and the most edges it keeps.
pub const MAX_GRAPH_FILES: usize = 20_000;
pub const MAX_EDGES: usize = 200_000;
/// The most lines a `use` or a parenthesised `from x import (...)` is followed over.
const MAX_STATEMENT_LINES: usize = 40;
/// The most paths one `use` statement stands for, and how deep its braces may go.
const MAX_USE_PATHS: usize = 64;
const MAX_GROUP_DEPTH: usize = 8;
/// The most files of one Go package that one import is an edge to.
const GO_PACKAGE_FILES: usize = 12;
/// The most names of things outside the folder that are kept for one file.
const MAX_EXTERNAL: usize = 60;
/// A file that is imported by more files than this has the rank it passes on shared out (a barrel file, a prelude, a `utils`).
const HUB_CAP: f64 = 8.0;
/// The rounds of the ranking, and what is kept of a round (the usual damping).
const RANK_ROUNDS: usize = 20;
const DAMPING: f64 = 0.85;

// ---- what a file says it imports -------------------------------------------------------------------------------------------------

/// Where a Rust path starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RustRoot {
    /// `crate::a::b`
    Crate,
    /// `self::a`
    Module,
    /// `super::a` (one or more)
    Super(usize),
    /// `mod a;`: the file of a module this file declares.
    Child,
    /// `a::b`: a module in scope here, or another crate.
    Name,
}

/// What a file says it imports, before it is looked for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RawImport {
    Rust { root: RustRoot, path: Vec<String> },
    /// A TypeScript or JavaScript specifier, as written.
    Script(String),
    /// `import a.b` (level 0, `module` is `a.b`) and `from ..a import b, c` (level 2, `module` is `a`, `names` are `b` and `c`).
    Python { level: usize, module: String, names: Vec<String> },
    /// A Go import path.
    Go(String),
}

impl RawImport {
    /// About how much memory it takes, for the index's cap.
    pub fn weight(&self) -> usize {
        48 + match self {
            RawImport::Rust { path, .. } => path.iter().map(|s| s.len() + 24).sum::<usize>(),
            RawImport::Script(s) | RawImport::Go(s) => s.len(),
            RawImport::Python { module, names, .. } => module.len() + names.iter().map(|s| s.len() + 24).sum::<usize>(),
        }
    }
}

/// What reading a file's imports found, and how much work it took (a test reads that).
#[derive(Debug, Default, Clone)]
pub struct Scan {
    pub imports: Vec<RawImport>,
    /// Lines looked at: each line of the file once, and each line a statement was followed over.
    pub looks: usize,
}

/// The imports of the file `file_name` whose text is `text`; nothing for a language that is not read.
pub fn imports_of(file_name: &str, text: &str) -> Vec<RawImport> {
    scan_imports(file_name, text).imports
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavour {
    Rust,
    Script,
    Python,
    Go,
}

fn flavour_of(file_name: &str) -> Option<Flavour> {
    let ext = file_name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => Flavour::Rust,
        "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => Flavour::Script,
        "py" | "pyi" | "pyw" => Flavour::Python,
        "go" => Flavour::Go,
        _ => return None,
    })
}

/// Is this a file whose imports are read?
pub fn reads_imports(file_name: &str) -> bool {
    flavour_of(file_name).is_some()
}

/// [`imports_of`], with the work it took.
pub fn scan_imports(file_name: &str, text: &str) -> Scan {
    let Some(flavour) = flavour_of(file_name) else { return Scan::default() };
    let mut lines: Vec<&str> = text.lines().take(MAX_LINES).collect();
    if let Some(first) = lines.first().copied() {
        lines[0] = first.strip_prefix('\u{feff}').unwrap_or(first);
    }
    // The bytes a statement may be followed over, in all: three times the size of the lines that are looked at, and some for a small file.
    let budget = lines.iter().map(|l| l.len() + 1).sum::<usize>().saturating_mul(3) + 100_000;
    let mut s = Scanner { lines: &lines, budget, looks: 0, out: Vec::new() };
    match flavour {
        Flavour::Rust => s.rust(),
        Flavour::Script => s.script(),
        Flavour::Python => s.python(),
        Flavour::Go => s.go(),
    }
    let mut seen: HashSet<RawImport> = HashSet::new();
    let mut imports = s.out;
    imports.retain(|i| seen.insert(i.clone()));
    imports.truncate(MAX_IMPORTS_PER_FILE);
    Scan { imports, looks: s.looks }
}

struct Scanner<'a> {
    lines: &'a [&'a str],
    /// Bytes of lines a statement may still be followed over, for the whole file: without it a file of 20,000 `use a::{` lines that are never closed makes each of them read the next 40
    /// (as the outline's scan budget does for the end of a declaration).
    budget: usize,
    looks: usize,
    out: Vec<RawImport>,
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_') && chars.all(|c| c.is_alphanumeric() || c == '_')
}

fn strip_comment<'s>(s: &'s str, marker: &str) -> &'s str {
    s.find(marker).map_or(s, |p| &s[..p])
}

impl<'a> Scanner<'a> {
    fn push(&mut self, i: RawImport) {
        if self.out.len() < MAX_IMPORTS_PER_FILE * 2 {
            self.out.push(i);
        }
    }

    fn spend(&mut self, bytes: usize) -> bool {
        if self.budget < bytes {
            self.budget = 0;
            return false;
        }
        self.budget -= bytes;
        true
    }

    /// The text of a statement that begins with `first` (the rest of line `i`) and ends at the first `end` character, followed over at most `max_extra` more lines; the text of a comment
    /// that begins with `comment` is left out. `None` when it does not end there, or the budget of the file is spent.
    fn gather(&mut self, first: &str, i: usize, end: char, comment: &str, max_extra: usize) -> Option<String> {
        let mut acc = String::new();
        let mut frag = strip_comment(first, comment);
        let mut k = i;
        loop {
            if let Some(pos) = frag.find(end) {
                acc.push_str(&frag[..pos]);
                return Some(acc);
            }
            acc.push_str(frag);
            acc.push(' ');
            k += 1;
            if k > i + max_extra || k >= self.lines.len() {
                return None;
            }
            let next: &'a str = self.lines[k];
            self.looks += 1;
            if next.len() > MAX_LINE_BYTES || !self.spend(next.len() + 1) {
                return None;
            }
            frag = strip_comment(next.trim(), comment);
        }
    }

    // ---- Rust ----

    /// A `crate::a::b` path written in code (`crate::flags::is_on(..)` with no `use` for it) is an import of the module it names: Rust code is written this way as often as with a `use`, and a
    /// graph that left these out would say a module is used by a tenth of the files that use it. Only the absolute form is read (`self::` and `super::` mean something else inside an inline
    /// module); a path in a string, after `$` (a macro's `$crate`) or after a longer name is not one; the segments are the lower-case names (a module or a function), and end at a type.
    fn inline_crate_paths(&mut self, line: &str) {
        let mut from = 0usize;
        while let Some(found) = line[from..].find("crate::") {
            let at = from + found;
            from = at + "crate::".len();
            if line[..at].chars().next_back().is_some_and(|c| is_name_char(c) || c == ':') || line[..at].bytes().filter(|b| *b == b'"').count() % 2 == 1 {
                continue;
            }
            let mut path: Vec<String> = Vec::new();
            let mut rest = &line[from..];
            while path.len() < 8 {
                let seg: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                if !seg.chars().next().is_some_and(|c| c.is_lowercase() || c == '_') {
                    break;
                }
                rest = &rest[seg.len()..];
                path.push(seg);
                match rest.strip_prefix("::") {
                    Some(r) => rest = r,
                    None => break,
                }
            }
            if !path.is_empty() {
                self.push(RawImport::Rust { root: RustRoot::Crate, path });
            }
        }
    }

    fn rust(&mut self) {
        for i in 0..self.lines.len() {
            self.looks += 1;
            let line: &'a str = self.lines[i];
            if line.len() > MAX_LINE_BYTES {
                continue;
            }
            let t = line.trim_start();
            let indent = line.len() - t.len();
            if t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') {
                continue;
            }
            let t = strip_visibility(t);
            if let Some(rest) = t.strip_prefix("use ") {
                let rest = rest.trim_start();
                // An indented `use` may be inside an inline module, where `self` and `super` mean something else: only an absolute path means the same there.
                if indent > 0 && !rest.trim_start_matches("::").starts_with("crate::") {
                    continue;
                }
                let Some(stmt) = self.gather(rest, i, ';', "//", MAX_STATEMENT_LINES) else { continue };
                for path in use_paths(&stmt) {
                    if let Some(imp) = classify_use(path) {
                        self.push(imp);
                    }
                }
            } else {
                self.inline_crate_paths(line);
                if indent != 0 {
                    continue;
                }
                if let Some(rest) = t.strip_prefix("mod ") {
                    if let Some(end) = rest.find(';') {
                        let name = rest[..end].trim();
                        if is_ident(name) {
                            self.push(RawImport::Rust { root: RustRoot::Child, path: vec![name.to_string()] });
                        }
                    }
                } else if let Some(rest) = t.strip_prefix("extern crate ") {
                    let name = rest.split(';').next().unwrap_or("").split_whitespace().next().unwrap_or("");
                    if is_ident(name) {
                        self.push(RawImport::Rust { root: RustRoot::Name, path: vec![name.to_string()] });
                    }
                }
            }
        }
    }

    // ---- TypeScript and JavaScript ----

    fn script(&mut self) {
        let re = script_re();
        for i in 0..self.lines.len() {
            self.looks += 1;
            let line: &'a str = self.lines[i];
            if line.len() > MAX_LINE_BYTES {
                continue;
            }
            let t = line.trim_start();
            if t.is_empty() || t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') {
                continue;
            }
            if t.starts_with("import") || t.starts_with("export") {
                if let Some(c) = re.from.captures(t).or_else(|| re.bare.captures(t)) {
                    self.push(RawImport::Script(c[1].to_string()));
                }
            } else if t.starts_with('}') {
                if let Some(c) = re.close.captures(t) {
                    self.push(RawImport::Script(c[1].to_string()));
                }
            }
            if t.contains("require") || t.contains("import") {
                for c in re.call.captures_iter(t).take(8) {
                    self.push(RawImport::Script(c[1].to_string()));
                }
            }
        }
    }

    // ---- Python ----

    fn python(&mut self) {
        for i in 0..self.lines.len() {
            self.looks += 1;
            let line: &'a str = self.lines[i];
            if line.len() > MAX_LINE_BYTES {
                continue;
            }
            let t = line.trim_start();
            if let Some(rest) = t.strip_prefix("import ") {
                for part in strip_comment(rest, "#").split(',').take(16) {
                    let name = part.split_whitespace().next().unwrap_or("");
                    if is_dotted(name) {
                        self.push(RawImport::Python { level: 0, module: name.to_string(), names: Vec::new() });
                    }
                }
            } else if let Some(rest) = t.strip_prefix("from ") {
                let Some(at) = rest.find(" import") else { continue };
                let module = rest[..at].trim();
                let after = &rest[at + " import".len()..];
                if !after.is_empty() && !after.starts_with(|c: char| c.is_whitespace() || c == '(') {
                    continue;
                }
                let level = module.chars().take_while(|c| *c == '.').count();
                let dotted = &module[level..];
                if module == "__future__" || (!dotted.is_empty() && !is_dotted(dotted)) || (level == 0 && dotted.is_empty()) {
                    continue;
                }
                let after = after.trim_start();
                let names_text = if let Some(open) = after.strip_prefix('(') {
                    self.gather(open, i, ')', "#", MAX_STATEMENT_LINES).unwrap_or_else(|| strip_comment(open, "#").to_string())
                } else {
                    strip_comment(after, "#").trim_end_matches('\\').to_string()
                };
                let names: Vec<String> = names_text.split(',').take(32).filter_map(|n| n.split_whitespace().next()).filter(|n| is_ident(n)).map(str::to_string).collect();
                self.push(RawImport::Python { level, module: dotted.to_string(), names });
            }
        }
    }

    // ---- Go ----

    fn go(&mut self) {
        let mut in_block = false;
        for i in 0..self.lines.len() {
            self.looks += 1;
            let line: &'a str = self.lines[i];
            if line.len() > MAX_LINE_BYTES {
                continue;
            }
            let t = line.trim();
            if t.is_empty() || t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') {
                continue;
            }
            if in_block {
                if t.starts_with(')') {
                    in_block = false;
                } else if let Some(p) = go_quoted(t) {
                    self.push(RawImport::Go(p.to_string()));
                }
                continue;
            }
            if let Some(rest) = t.strip_prefix("import") {
                if !rest.is_empty() && !rest.starts_with(|c: char| c.is_whitespace() || c == '(' || c == '"' || c == '`') {
                    continue;
                }
                let rest = rest.trim_start();
                if let Some(inner) = rest.strip_prefix('(') {
                    if let Some(close) = inner.find(')') {
                        for piece in inner[..close].split(';') {
                            if let Some(p) = go_quoted(piece) {
                                self.push(RawImport::Go(p.to_string()));
                            }
                        }
                    } else {
                        in_block = true;
                        if let Some(p) = go_quoted(inner) {
                            self.push(RawImport::Go(p.to_string()));
                        }
                    }
                } else if let Some(p) = go_quoted(rest) {
                    self.push(RawImport::Go(p.to_string()));
                }
                continue;
            }
            // Imports come before every declaration.
            if t.starts_with("func ") || t.starts_with("type ") || t.starts_with("var ") || t.starts_with("const ") {
                break;
            }
        }
    }
}

fn go_quoted(s: &str) -> Option<&str> {
    let start = s.find(['"', '`'])?;
    let quote = s[start..].chars().next()?;
    let rest = &s[start + 1..];
    let end = rest.find(quote)?;
    let p = &rest[..end];
    (!p.is_empty() && p.len() <= 300 && !p.chars().any(|c| c.is_control() || c.is_whitespace())).then_some(p)
}

/// `a`, `a.b.c`: the name of a Python module.
fn is_dotted(s: &str) -> bool {
    !s.is_empty() && s.split('.').all(is_ident)
}

/// `pub use x` is `use x`, `pub(crate) use x` too.
fn strip_visibility(t: &str) -> &str {
    let Some(rest) = t.strip_prefix("pub") else { return t };
    if let Some(r) = rest.strip_prefix(' ') {
        return r.trim_start();
    }
    if rest.starts_with('(') {
        if let Some(close) = rest.find(')') {
            return rest[close + 1..].trim_start();
        }
    }
    t
}

// ---- the paths of a Rust `use` -----------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Id(String),
    Sep,
    Open,
    Close,
    Comma,
    Star,
}

/// The tokens of the text of a `use` statement (without the word `use` and the `;`), or `None` when it holds anything else (a macro, a string).
fn tokens(stmt: &str) -> Option<Vec<Tok>> {
    let chars: Vec<char> = stmt.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            ':' if chars.get(i + 1) == Some(&':') => {
                out.push(Tok::Sep);
                i += 2;
            }
            '{' => {
                out.push(Tok::Open);
                i += 1;
            }
            '}' => {
                out.push(Tok::Close);
                i += 1;
            }
            ',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            '*' => {
                out.push(Tok::Star);
                i += 1;
            }
            c if c.is_alphanumeric() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                out.push(Tok::Id(chars[start..i].iter().collect()));
            }
            _ => return None,
        }
        if out.len() > 2_000 {
            return None;
        }
    }
    Some(out)
}

/// One item of a `use` tree (a path, with or without a group or a glob at its end), added to `out` as the full path(s) it stands for.
fn use_item(t: &[Tok], i: &mut usize, prefix: &[String], out: &mut Vec<Vec<String>>, depth: usize) {
    let mut path: Vec<String> = prefix.to_vec();
    if matches!(t.get(*i), Some(Tok::Sep)) {
        *i += 1;
    }
    loop {
        match t.get(*i) {
            Some(Tok::Id(s)) if s == "as" && path.len() > prefix.len() => {
                // `a::b as c`: the alias is not a path.
                *i += 1;
                if matches!(t.get(*i), Some(Tok::Id(_))) {
                    *i += 1;
                }
                break;
            }
            Some(Tok::Id(s)) => {
                path.push(s.clone());
                *i += 1;
            }
            Some(Tok::Star) => {
                *i += 1;
                break;
            }
            Some(Tok::Open) => {
                *i += 1;
                if depth < MAX_GROUP_DEPTH {
                    while !matches!(t.get(*i), Some(Tok::Close) | None) && out.len() < MAX_USE_PATHS {
                        let before = *i;
                        use_item(t, i, &path, out, depth + 1);
                        if matches!(t.get(*i), Some(Tok::Comma)) {
                            *i += 1;
                        }
                        if *i == before {
                            *i += 1;
                        }
                    }
                } else {
                    // Too deep: skip the group.
                    let mut open = 1usize;
                    while open > 0 && *i < t.len() {
                        match t[*i] {
                            Tok::Open => open += 1,
                            Tok::Close => open -= 1,
                            _ => {}
                        }
                        *i += 1;
                    }
                    return;
                }
                if matches!(t.get(*i), Some(Tok::Close)) {
                    *i += 1;
                }
                return;
            }
            _ => break,
        }
        if matches!(t.get(*i), Some(Tok::Sep)) {
            *i += 1;
        } else if !matches!(t.get(*i), Some(Tok::Id(s)) if s == "as") {
            break;
        }
    }
    if path.len() > prefix.len() && out.len() < MAX_USE_PATHS {
        out.push(path);
    }
}

/// The full paths a `use` statement stands for: `a::{b, c::d}` is `a::b` and `a::c::d`; a glob `a::*` is `a`; an alias is dropped.
pub fn use_paths(stmt: &str) -> Vec<Vec<String>> {
    let Some(t) = tokens(stmt) else { return Vec::new() };
    let mut out = Vec::new();
    let mut i = 0;
    while i < t.len() && out.len() < MAX_USE_PATHS {
        let before = i;
        use_item(&t, &mut i, &[], &mut out, 0);
        if matches!(t.get(i), Some(Tok::Comma)) {
            i += 1;
        }
        if i == before {
            i += 1;
        }
    }
    out
}

fn classify_use(mut path: Vec<String>) -> Option<RawImport> {
    while path.len() > 1 && path.last().is_some_and(|s| s == "self") {
        path.pop();
    }
    let first = path.first()?.clone();
    if first.starts_with('$') {
        return None;
    }
    Some(match first.as_str() {
        "crate" => RawImport::Rust { root: RustRoot::Crate, path: path[1..].to_vec() },
        "self" => RawImport::Rust { root: RustRoot::Module, path: path[1..].to_vec() },
        "super" => {
            let n = path.iter().take_while(|s| s.as_str() == "super").count();
            RawImport::Rust { root: RustRoot::Super(n), path: path[n..].to_vec() }
        }
        _ => RawImport::Rust { root: RustRoot::Name, path },
    })
}

// ---- TypeScript and JavaScript patterns --------------------------------------------------------------------------------------------

struct ScriptRe {
    /// `import x from 'a'`, `import type {x} from "a"`, `export * from './a'`, `export {x} from './a'`.
    from: Regex,
    /// `import './a'`
    bare: Regex,
    /// `} from './a'`: the last line of an import written over several.
    close: Regex,
    /// `require('a')` and `import('a')` anywhere in a line.
    call: Regex,
}

fn script_re() -> &'static ScriptRe {
    static RE: OnceLock<ScriptRe> = OnceLock::new();
    RE.get_or_init(|| ScriptRe {
        from: Regex::new(r#"^(?:import|export)\b[^'"`;]*?\bfrom\s*['"]([^'"\n]{1,300})['"]"#).expect("a constant pattern"),
        bare: Regex::new(r#"^import\s*['"]([^'"\n]{1,300})['"]"#).expect("a constant pattern"),
        close: Regex::new(r#"^\}\s*from\s*['"]([^'"\n]{1,300})['"]"#).expect("a constant pattern"),
        call: Regex::new(r#"\b(?:require|import)\s*\(\s*['"]([^'"\n]{1,300})['"]\s*\)"#).expect("a constant pattern"),
    })
}

// ---- the graph ----------------------------------------------------------------------------------------------------------------------

/// One file of a graph: its path from the folder (written with `/`), and what it says it imports.
pub struct GraphFile<'a> {
    pub rel: &'a str,
    pub imports: &'a [RawImport],
}

/// A Go module: the folder of its `go.mod` (from the graph's folder, `""` for the folder itself) and its module path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoModule {
    pub dir: String,
    pub path: String,
}

/// The module path a `go.mod` names (`module example.com/x`), or `None`.
pub fn go_module_path(go_mod: &str) -> Option<String> {
    for line in go_mod.lines().take(200) {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("module") {
            if !rest.starts_with(|c: char| c.is_whitespace() || c == '"' || c == '`') {
                continue;
            }
            let name = strip_comment(rest, "//").trim().trim_matches(|c| c == '"' || c == '`');
            if !name.is_empty() && !name.chars().any(|c| c.is_control() || c.is_whitespace()) {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Who imports whom among some files. `files` are sorted by path and every index is a position in it.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    pub files: Vec<String>,
    /// What each file imports (sorted, no file twice, never itself).
    pub out: Vec<Vec<u32>>,
    /// Who imports each file (the reverse of `out`).
    pub inc: Vec<Vec<u32>>,
    /// The names of things outside the folder (a crate, a package, a standard library path) each file imports.
    pub external: Vec<Vec<String>>,
    /// How many of each file's imports were not found (an alias, a re-export, a file the folder does not hold).
    pub unresolved: Vec<u32>,
    pub edges: usize,
    /// The graph holds [`MAX_EDGES`] edges and the rest were not kept.
    pub capped: bool,
}

struct Index<'a> {
    ids: HashMap<&'a str, u32>,
    /// The Go files of each folder that are not tests.
    go_dirs: HashMap<&'a str, Vec<u32>>,
    /// The folders that hold a `lib.rs` or a `main.rs`: the roots of crates.
    rust_roots: HashSet<&'a str>,
}

fn parent_dir(rel: &str) -> &str {
    rel.rfind('/').map_or("", |p| &rel[..p])
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else if name.is_empty() {
        dir.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

impl<'a> Index<'a> {
    fn new(files: &[GraphFile<'a>]) -> Index<'a> {
        let mut ids = HashMap::with_capacity(files.len());
        let mut go_dirs: HashMap<&'a str, Vec<u32>> = HashMap::new();
        let mut rust_roots = HashSet::new();
        for (i, f) in files.iter().enumerate() {
            ids.insert(f.rel, i as u32);
            let name = f.rel.rsplit('/').next().unwrap_or(f.rel);
            if name.ends_with(".go") && !name.ends_with("_test.go") {
                go_dirs.entry(parent_dir(f.rel)).or_default().push(i as u32);
            }
            if name == "lib.rs" || name == "main.rs" {
                rust_roots.insert(parent_dir(f.rel));
            }
        }
        Index { ids, go_dirs, rust_roots }
    }

    fn get(&self, path: &str) -> Option<u32> {
        self.ids.get(path).copied()
    }

    fn first_of(&self, candidates: &[String]) -> Option<u32> {
        candidates.iter().find_map(|c| self.get(c))
    }

    /// The file of the module `path` of the crate whose root folder is `root`.
    fn rust_module(&self, root: &str, path: &[String]) -> Option<u32> {
        if path.is_empty() {
            return self.get(&join(root, "lib.rs")).or_else(|| self.get(&join(root, "main.rs")));
        }
        let base = join(root, &path.join("/"));
        self.get(&format!("{base}.rs")).or_else(|| self.get(&format!("{base}/mod.rs")))
    }
}

enum Res {
    Files(Vec<u32>),
    External(String),
    Unresolved,
}

/// The graph of `files`, which must be sorted by path.
pub fn build(files: &[GraphFile<'_>], go_modules: &[GoModule]) -> Graph {
    build_with_cap(files, go_modules, MAX_EDGES)
}

/// [`build`] with another limit on the edges (a test's way to reach the cap).
pub fn build_with_cap(files: &[GraphFile<'_>], go_modules: &[GoModule], max_edges: usize) -> Graph {
    debug_assert!(files.windows(2).all(|w| w[0].rel < w[1].rel), "the files of a graph are sorted by path");
    let ix = Index::new(files);
    let mut modules: Vec<&GoModule> = go_modules.iter().collect();
    modules.sort_by(|a, b| b.path.len().cmp(&a.path.len()).then_with(|| a.dir.cmp(&b.dir)));
    let n = files.len();
    let mut g = Graph { files: files.iter().map(|f| f.rel.to_string()).collect(), out: vec![Vec::new(); n], inc: vec![Vec::new(); n], external: vec![Vec::new(); n], unresolved: vec![0; n], ..Graph::default() };
    for (i, f) in files.iter().enumerate() {
        let mut targets: Vec<u32> = Vec::new();
        let mut external: BTreeSet<String> = BTreeSet::new();
        for imp in f.imports {
            match resolve(&ix, f.rel, imp, &modules) {
                Res::Files(v) => targets.extend(v),
                Res::External(name) => {
                    if external.len() < MAX_EXTERNAL {
                        external.insert(name);
                    }
                }
                Res::Unresolved => g.unresolved[i] += 1,
            }
        }
        targets.retain(|t| *t != i as u32);
        targets.sort_unstable();
        targets.dedup();
        if g.edges + targets.len() > max_edges {
            g.capped = true;
            targets.truncate(max_edges.saturating_sub(g.edges));
        }
        g.edges += targets.len();
        g.out[i] = targets;
        g.external[i] = external.into_iter().collect();
    }
    for (i, targets) in g.out.iter().enumerate() {
        for t in targets {
            g.inc[*t as usize].push(i as u32);
        }
    }
    g
}

fn resolve(ix: &Index<'_>, rel: &str, imp: &RawImport, modules: &[&GoModule]) -> Res {
    match imp {
        RawImport::Rust { root, path } => resolve_rust(ix, rel, *root, path),
        RawImport::Script(spec) => resolve_script(ix, rel, spec),
        RawImport::Python { level, module, names } => resolve_python(ix, rel, *level, module, names),
        RawImport::Go(path) => resolve_go(ix, path, modules),
    }
}

// ---- Rust ----

/// The folder of the crate that holds `rel`: the nearest folder at or above it that has a `lib.rs` or a `main.rs`.
fn crate_root<'r>(ix: &Index<'_>, rel: &'r str) -> Option<&'r str> {
    let mut dir = parent_dir(rel);
    loop {
        if ix.rust_roots.contains(dir) {
            return Some(dir);
        }
        if dir.is_empty() {
            return None;
        }
        dir = parent_dir(dir);
    }
}

/// The path of the module a file is (`a/b.rs` and `a/b/mod.rs` are `a::b`; `lib.rs` and `main.rs` are the crate itself).
fn module_path(rel: &str, root: &str) -> Vec<String> {
    let inside = if root.is_empty() { rel } else { rel.strip_prefix(root).and_then(|r| r.strip_prefix('/')).unwrap_or(rel) };
    if inside == "lib.rs" || inside == "main.rs" {
        return Vec::new();
    }
    let mut segs: Vec<&str> = inside.split('/').collect();
    let Some(last) = segs.pop() else { return Vec::new() };
    let stem = last.strip_suffix(".rs").unwrap_or(last);
    if stem != "mod" {
        segs.push(stem);
    }
    segs.into_iter().map(str::to_string).collect()
}

fn resolve_rust(ix: &Index<'_>, rel: &str, root: RustRoot, path: &[String]) -> Res {
    let Some(crate_dir) = crate_root(ix, rel) else {
        // No crate root among the files: only a name that is no module can be said to be outside.
        return match (root, path.first()) {
            (RustRoot::Name, Some(first)) => Res::External(first.clone()),
            _ => Res::Unresolved,
        };
    };
    let own = module_path(rel, crate_dir);
    let (full, lowest): (Vec<String>, usize) = match root {
        RustRoot::Crate => (path.to_vec(), 0),
        RustRoot::Module => ([own.as_slice(), path].concat(), 0),
        RustRoot::Super(n) => {
            if own.len() < n {
                return Res::Unresolved;
            }
            ([&own[..own.len() - n], path].concat(), 0)
        }
        RustRoot::Child => {
            let full = [own.as_slice(), path].concat();
            let at = full.len();
            (full, at)
        }
        // A module declared in this one, or another crate: the first name decides.
        RustRoot::Name => ([own.as_slice(), path].concat(), own.len() + 1),
    };
    for k in (lowest..=full.len()).rev() {
        if let Some(id) = ix.rust_module(crate_dir, &full[..k]) {
            return Res::Files(vec![id]);
        }
    }
    match (root, path.first()) {
        (RustRoot::Name, Some(first)) => Res::External(first.clone()),
        _ => Res::Unresolved,
    }
}

// ---- TypeScript and JavaScript ----

const SCRIPT_EXTS: [&str; 8] = ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

/// `dir` and a relative `spec` as one path from the graph's folder; `None` when it leaves the folder.
fn normalize(dir: &str, spec: &str) -> Option<String> {
    let mut parts: Vec<&str> = if dir.is_empty() { Vec::new() } else { dir.split('/').collect() };
    for seg in spec.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

fn resolve_script(ix: &Index<'_>, rel: &str, spec: &str) -> Res {
    let relative = spec == "." || spec == ".." || spec.starts_with("./") || spec.starts_with("../");
    if !relative {
        // An alias is not followed (it is the project's own setting); a package is outside the folder.
        if spec.starts_with("@/") || spec.starts_with("~/") || spec.starts_with('#') || spec.starts_with('/') || spec.contains("://") {
            return Res::Unresolved;
        }
        let name = spec.strip_prefix("node:").unwrap_or(spec);
        let mut parts = name.split('/');
        let root = match (parts.next(), parts.next()) {
            (Some(scope), Some(second)) if scope.starts_with('@') => format!("{scope}/{second}"),
            (Some(first), _) => first.to_string(),
            _ => return Res::Unresolved,
        };
        return Res::External(root);
    }
    let Some(base) = normalize(parent_dir(rel), spec) else { return Res::Unresolved };
    let mut candidates: Vec<String> = Vec::new();
    candidates.push(base.clone());
    // `./a.js` written in a TypeScript file is `./a.ts`.
    for (js, ts) in [(".js", [".ts", ".tsx"].as_slice()), (".jsx", [".tsx"].as_slice()), (".mjs", [".mts"].as_slice()), (".cjs", [".cts"].as_slice())] {
        if let Some(stem) = base.strip_suffix(js) {
            candidates.extend(ts.iter().map(|e| format!("{stem}{e}")));
        }
    }
    candidates.extend(SCRIPT_EXTS.iter().map(|e| format!("{base}.{e}")));
    candidates.extend(SCRIPT_EXTS.iter().map(|e| join(&base, &format!("index.{e}"))));
    match ix.first_of(&candidates) {
        Some(id) => Res::Files(vec![id]),
        None => Res::Unresolved,
    }
}

// ---- Python ----

/// The file of the module `parts` under the folder `root`.
fn python_module(ix: &Index<'_>, root: &str, parts: &[&str]) -> Option<u32> {
    if parts.is_empty() {
        return ix.get(&join(root, "__init__.py"));
    }
    let base = join(root, &parts.join("/"));
    ix.first_of(&[format!("{base}.py"), format!("{base}/__init__.py"), format!("{base}.pyi")])
}

fn resolve_python(ix: &Index<'_>, rel: &str, level: usize, module: &str, names: &[String]) -> Res {
    let parts: Vec<&str> = if module.is_empty() { Vec::new() } else { module.split('.').collect() };
    let mut roots: Vec<&str> = Vec::new();
    if level > 0 {
        let mut dir = parent_dir(rel);
        for _ in 1..level {
            if dir.is_empty() {
                return Res::Unresolved;
            }
            dir = parent_dir(dir);
        }
        roots.push(dir);
    } else {
        // The folder of the file and each folder above it: the first that holds the module is the one.
        let mut dir = parent_dir(rel);
        loop {
            roots.push(dir);
            if dir.is_empty() {
                break;
            }
            dir = parent_dir(dir);
        }
    }
    let find = |parts: &[&str]| roots.iter().find_map(|r| python_module(ix, r, parts));
    let mut found: Vec<u32> = Vec::new();
    // `from a import b`: `b` may be a module of `a`.
    for name in names {
        let mut with = parts.clone();
        with.push(name.as_str());
        if let Some(id) = find(&with) {
            found.push(id);
        }
    }
    if found.is_empty() {
        // The module itself, or the longest start of its name that is a file (`import a.b.c` where `a/b.py` holds `c`).
        let shortest = if parts.is_empty() { 0 } else { 1 };
        for k in (shortest..=parts.len()).rev() {
            if let Some(id) = find(&parts[..k]) {
                found.push(id);
                break;
            }
        }
    }
    if !found.is_empty() {
        return Res::Files(found);
    }
    match (level, parts.first()) {
        (0, Some(first)) => Res::External((*first).to_string()),
        _ => Res::Unresolved,
    }
}

// ---- Go ----

fn go_external_name(path: &str) -> String {
    let mut parts = path.split('/');
    match parts.next() {
        Some(first) if first.contains('.') => path.split('/').take(3).collect::<Vec<_>>().join("/"),
        _ => path.to_string(),
    }
}

fn resolve_go(ix: &Index<'_>, path: &str, modules: &[&GoModule]) -> Res {
    // `modules` is sorted by the length of the module path, longest first.
    let module = modules.iter().find(|m| path == m.path || path.strip_prefix(m.path.as_str()).is_some_and(|rest| rest.starts_with('/')));
    let Some(m) = module else { return Res::External(go_external_name(path)) };
    let rest = path[m.path.len()..].trim_start_matches('/');
    let dir = join(&m.dir, rest);
    match ix.go_dirs.get(dir.as_str()) {
        Some(files) => Res::Files(files.iter().take(GO_PACKAGE_FILES).copied().collect()),
        None => Res::Unresolved,
    }
}

// ---- the ranking -----------------------------------------------------------------------------------------------------------------

impl Graph {
    pub fn id_of(&self, rel: &str) -> Option<u32> {
        self.files.binary_search_by(|f| f.as_str().cmp(rel)).ok().map(|i| i as u32)
    }

    /// How much of the rank of a file that is imported by many files it keeps passing on: the most a file's `HUB_CAP` importers can give it is what that many give.
    fn hub(&self, id: usize) -> f64 {
        let importers = self.inc[id].len() as f64;
        if importers <= HUB_CAP {
            1.0
        } else {
            HUB_CAP / importers
        }
    }

    /// The rank of each file, personalised: a walker that follows an import (a file imported is worth reading), or half as readily the reverse of one (a file that imports this one is where
    /// it is used), and returns to one of `seeds` with probability 0.15 at each step (to any file when there are none). The rank a file passes on is cut when many files import it, so that a
    /// barrel file does not take it all. 20 rounds whatever the graph, so a cycle ends; the ranks sum to one.
    pub fn rank(&self, seeds: &[u32]) -> Vec<f64> {
        let n = self.files.len();
        if n == 0 {
            return Vec::new();
        }
        let mut teleport = vec![0.0f64; n];
        let seeds: BTreeSet<u32> = seeds.iter().copied().filter(|s| (*s as usize) < n).collect();
        if seeds.is_empty() {
            teleport.iter_mut().for_each(|t| *t = 1.0 / n as f64);
        } else {
            let share = 1.0 / seeds.len() as f64;
            for s in &seeds {
                teleport[*s as usize] = share;
            }
        }
        let hub: Vec<f64> = (0..n).map(|i| self.hub(i)).collect();
        // What each node sends along its imports (weight 1 for the file it imports, scaled by that file's hub factor) and along the reverse of the imports of it (weight a half).
        let total: Vec<f64> = (0..n).map(|u| self.out[u].iter().map(|v| hub[*v as usize]).sum::<f64>() + 0.5 * hub[u] * self.inc[u].len() as f64).collect();
        let mut rank = teleport.clone();
        for _ in 0..RANK_ROUNDS {
            let mut next = vec![0.0f64; n];
            let mut dangling = 0.0f64;
            for u in 0..n {
                if total[u] <= 0.0 {
                    dangling += rank[u];
                    continue;
                }
                let share = rank[u] / total[u];
                for v in &self.out[u] {
                    next[*v as usize] += share * hub[*v as usize];
                }
                for v in &self.inc[u] {
                    next[*v as usize] += share * 0.5 * hub[u];
                }
            }
            for i in 0..n {
                next[i] = (1.0 - DAMPING) * teleport[i] + DAMPING * (next[i] + dangling * teleport[i]);
            }
            rank = next;
        }
        rank
    }
}

// ---- references ---------------------------------------------------------------------------------------------------------------------

/// Does a character belong to a name?
pub fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// The last name of what a model wrote (`Config::parse`, `store.load` and `parse_config` give `parse`, `load` and `parse_config`), or `None` when it is not a name.
pub fn symbol_name(raw: &str) -> Option<String> {
    let last = raw.trim().rsplit(['.', ':', '/', '#']).find(|s| !s.is_empty())?;
    let last = last.trim_end_matches(['(', ')', '!']);
    (!last.is_empty() && last.chars().count() <= 100 && last.chars().all(is_name_char) && !last.chars().next().is_some_and(|c| c.is_ascii_digit())).then(|| last.to_string())
}

/// Is a line that begins with `#` a comment in this file's language (it is a directive in C and an attribute in Rust)?
pub fn hash_comments(file_name: &str) -> bool {
    matches!(file_name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).as_deref(), Some("py" | "pyi" | "pyw" | "rb" | "sh" | "bash" | "ps1" | "yaml" | "yml" | "toml"))
}

/// The positions in `line` where `word` stands as a name (not as a part of a longer one).
fn word_positions(line: &str, word: &str) -> Vec<usize> {
    line.match_indices(word)
        .filter(|(at, _)| {
            let before = line[..*at].chars().next_back();
            let after = line[at + word.len()..].chars().next();
            !before.is_some_and(is_name_char) && !after.is_some_and(is_name_char)
        })
        .map(|(at, _)| at)
        .collect()
}

/// One line that holds a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    /// 1-based.
    pub line: usize,
    pub text: String,
    pub tag: &'static str,
    /// Where the name is in the line (the first time), in bytes.
    pub at: usize,
}

/// The lines of `text` that hold `word` as a name, comments left out. Every line that does is counted; the first `keep` are returned (a line of more than [`MAX_LINE_BYTES`] bytes is not looked at).
/// `in_test` says whether a line is inside a test (a function or a module of tests), and `defs` which lines are a definition of the name (they are not mentions).
pub fn mentions(file_name: &str, rel: &str, text: &str, word: &str, defs: &HashSet<usize>, in_test: &dyn Fn(usize) -> bool, keep: usize) -> (Vec<Mention>, usize) {
    let hash = hash_comments(file_name);
    let test_file = is_test_path(rel);
    let mut out = Vec::new();
    let mut total = 0usize;
    if !text.contains(word) {
        return (out, 0);
    }
    for (i, line) in text.lines().enumerate().take(MAX_LINES) {
        let n = i + 1;
        if line.len() > MAX_REF_LINE_BYTES || !line.contains(word) || defs.contains(&n) {
            continue;
        }
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') || (hash && t.starts_with('#')) {
            continue;
        }
        let at = word_positions(line, word);
        let Some(first) = at.first().copied() else { continue };
        total += 1;
        if out.len() < keep {
            let tag = if is_import_line(t) {
                "import"
            } else if test_file || in_test(n) {
                "test"
            } else if is_call(&line[first + word.len()..]) {
                "call"
            } else {
                "ref"
            };
            out.push(Mention { line: n, text: around(line, first, 110), tag, at: first });
        }
    }
    (out, total)
}

/// What follows a name when it is called: `(`, `!(` of a macro, `::<T>(` of a Rust call with a type.
fn is_call(after: &str) -> bool {
    let a = after.trim_start();
    if a.starts_with('(') {
        return true;
    }
    if let Some(rest) = a.strip_prefix('!') {
        return rest.trim_start().starts_with(['(', '[', '{']);
    }
    if let Some(rest) = a.strip_prefix("::<") {
        return rest.find('>').is_some_and(|close| rest[close + 1..].trim_start().starts_with('('));
    }
    false
}

/// A line that brings names in from another file.
fn is_import_line(t: &str) -> bool {
    let t = strip_visibility(t);
    t.starts_with("use ")
        || t.starts_with("import ")
        || t.starts_with("from ")
        || t.starts_with("extern crate ")
        || t.starts_with("#include")
        || t.starts_with("} from")
        || (t.starts_with("export") && (t.contains(" from '") || t.contains(" from \"")))
        || ((t.starts_with("const ") || t.starts_with("let ") || t.starts_with("var ")) && t.contains("require("))
}

/// Does the path say it is a file of tests?
pub fn is_test_path(rel: &str) -> bool {
    let l = rel.to_ascii_lowercase();
    let name = l.rsplit('/').next().unwrap_or("");
    l.split('/').any(|s| matches!(s, "tests" | "test" | "__tests__" | "spec" | "specs" | "e2e"))
        || name.starts_with("test_")
        || name.contains("_test.")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.ends_with("_tests.rs")
}

/// The part of `line` that has the name, which begins at byte `at`: all of it when it is short, else about forty characters before the name and what follows, as one line of at most `max` characters.
fn around(line: &str, at: usize, max: usize) -> String {
    let before = line[..at].chars().count();
    if line.chars().count() <= max || before <= 40 {
        return one_line(line, max);
    }
    let rest: String = line.chars().skip(before - 40).collect();
    format!("…{}", one_line(&rest, max.saturating_sub(1)))
}

/// `text` as one line of at most `max` characters: control characters are spaces, runs of spaces are one, the ends are cut.
pub fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let joined = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() > max {
        format!("{}…", joined.chars().take(max).collect::<String>().trim_end())
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A graph made from files given as (path, text).
    fn graph_of(files: &[(&str, &str)]) -> Graph {
        let mut files: Vec<(&str, &str)> = files.to_vec();
        files.sort_by(|a, b| a.0.cmp(b.0));
        let scanned: Vec<(&str, Vec<RawImport>)> = files.iter().map(|(rel, text)| (*rel, imports_of(rel.rsplit('/').next().unwrap(), text))).collect();
        let inputs: Vec<GraphFile<'_>> = scanned.iter().map(|(rel, imports)| GraphFile { rel, imports }).collect();
        let mods: Vec<GoModule> = files.iter().filter(|(rel, _)| rel.ends_with("go.mod")).filter_map(|(rel, text)| go_module_path(text).map(|path| GoModule { dir: parent_dir(rel).to_string(), path })).collect();
        build(&inputs, &mods)
    }

    fn imports(g: &Graph, rel: &str) -> Vec<String> {
        let id = g.id_of(rel).unwrap_or_else(|| panic!("{rel} is not in the graph")) as usize;
        g.out[id].iter().map(|t| g.files[*t as usize].clone()).collect()
    }

    fn imported_by(g: &Graph, rel: &str) -> Vec<String> {
        let id = g.id_of(rel).unwrap() as usize;
        g.inc[id].iter().map(|t| g.files[*t as usize].clone()).collect()
    }

    fn external(g: &Graph, rel: &str) -> Vec<String> {
        g.external[g.id_of(rel).unwrap() as usize].clone()
    }

    const RUST: &[(&str, &str)] = &[
        ("src/lib.rs", "//! The crate.\npub mod config;\npub mod util;\nmod net;\nuse std::fmt;\nuse crate::config::Config;\n"),
        ("src/config.rs", "use crate::util::{self, helper};\nuse super::net;\nuse serde_json::Value;\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n    use crate::util::helper as h;\n}\n"),
        ("src/util.rs", "use std::path::Path;\nuse crate::config::Config;\n"),
        ("src/net/mod.rs", "use super::util::helper;\nuse self::tcp::Conn;\npub mod tcp;\n"),
        ("src/net/tcp.rs", "use crate::util;\nuse tokio::net::TcpStream;\n"),
    ];

    #[test]
    fn imports_resolve_in_rust_ts_python_go_fixtures() {
        // Rust: crate::, self::, super::, `mod x;` (a file or a folder with mod.rs), a group, an alias and a `use` inside an inline module that begins with crate::.
        let g = graph_of(RUST);
        assert_eq!(imports(&g, "src/lib.rs"), ["src/config.rs", "src/net/mod.rs", "src/util.rs"]);
        assert_eq!(imports(&g, "src/config.rs"), ["src/net/mod.rs", "src/util.rs"], "`use super::*` inside the inline test module is not read, `crate::util::helper as h` is");
        assert_eq!(imports(&g, "src/util.rs"), ["src/config.rs"]);
        assert_eq!(imports(&g, "src/net/mod.rs"), ["src/net/tcp.rs", "src/util.rs"]);
        assert_eq!(imports(&g, "src/net/tcp.rs"), ["src/util.rs"]);
        assert_eq!(external(&g, "src/lib.rs"), ["std"]);
        assert_eq!(external(&g, "src/config.rs"), ["serde_json"]);
        assert_eq!(external(&g, "src/net/tcp.rs"), ["tokio"]);
        // A module in scope is not another crate: `use config::Config;` after `mod config;`.
        let g = graph_of(&[("src/main.rs", "mod config;\nuse config::Config;\nuse anyhow::Result;\n"), ("src/config.rs", "")]);
        assert_eq!(imports(&g, "src/main.rs"), ["src/config.rs"]);
        assert_eq!(external(&g, "src/main.rs"), ["anyhow"]);

        // TypeScript and JavaScript: a relative file, with the extension left out, `.js` that is `.ts`, a folder with `index`, `require`, a side-effect import, a multi-line import; a package is outside.
        let g = graph_of(&[
            ("web/src/api.ts", "import { format } from './format';\nimport type { Cfg } from \"./config.js\";\nimport React from 'react';\nexport * from './components';\nconst legacy = require('../lib/legacy');\nimport './side-effect.css';\nimport {\n  a,\n  b,\n} from './multi';\nimport fs from 'node:fs';\nimport { x } from '@scope/pkg/deep';\nimport { y } from '@/alias';\n"),
            ("web/src/format.ts", ""),
            ("web/src/config.ts", "const lazy = () => import('./format');\n"),
            ("web/src/components/index.ts", ""),
            ("web/lib/legacy.js", ""),
            ("web/src/multi.ts", ""),
        ]);
        assert_eq!(imports(&g, "web/src/api.ts"), ["web/lib/legacy.js", "web/src/components/index.ts", "web/src/config.ts", "web/src/format.ts", "web/src/multi.ts"]);
        assert_eq!(external(&g, "web/src/api.ts"), ["@scope/pkg", "fs", "react"]);
        assert_eq!(g.unresolved[g.id_of("web/src/api.ts").unwrap() as usize], 2, "the stylesheet is no source file and an alias is not followed");
        assert_eq!(imports(&g, "web/src/config.ts"), ["web/src/format.ts"], "a dynamic import is read");

        // Python: a package, `from . import`, `from .x import`, `from ..x import`, the names of `from a import b` that are modules, a parenthesised list over several lines.
        let g = graph_of(&[
            ("app/main.py", "import os\nimport app.util\nfrom app import models\nfrom .views import render, other\nfrom . import helpers\nfrom ..top import x\nfrom pkg.sub import (\n    alpha,\n    beta,\n)\nfrom __future__ import annotations\n"),
            ("app/__init__.py", ""),
            ("app/util.py", ""),
            ("app/models.py", ""),
            ("app/views.py", ""),
            ("app/helpers.py", ""),
            ("top.py", ""),
            ("pkg/sub/__init__.py", ""),
        ]);
        assert_eq!(imports(&g, "app/main.py"), ["app/helpers.py", "app/models.py", "app/util.py", "app/views.py", "pkg/sub/__init__.py", "top.py"]);
        assert_eq!(external(&g, "app/main.py"), ["os"]);

        // Go: through go.mod, every file of the package but its tests, the standard library and other modules are outside.
        let g = graph_of(&[
            ("go.mod", "module example.com/svc\n\ngo 1.22\n"),
            ("cmd/main.go", "package main\n\nimport (\n\t\"fmt\"\n\t\"example.com/svc/internal/store\"\n\tlog \"github.com/x/y/log/v2\"\n\t_ \"net/http/pprof\"\n)\n\nfunc main() {}\n"),
            ("internal/store/store.go", "package store\n\nimport \"example.com/svc/internal/util\"\n"),
            ("internal/store/extra.go", "package store\n"),
            ("internal/store/store_test.go", "package store\n"),
            ("internal/util/util.go", "package util\n"),
        ]);
        assert_eq!(imports(&g, "cmd/main.go"), ["internal/store/extra.go", "internal/store/store.go"]);
        assert_eq!(external(&g, "cmd/main.go"), ["fmt", "github.com/x/y", "net/http/pprof"], "a package of another module is named by the first three parts of its path");
        assert_eq!(imports(&g, "internal/store/store.go"), ["internal/util/util.go"]);
        assert_eq!(go_module_path("// c\nmodule \"example.com/q\" // x\n"), Some("example.com/q".to_string()));
    }

    #[test]
    fn dependents_are_the_reverse_of_imports() {
        let g = graph_of(RUST);
        let mut forward = 0;
        for (a, targets) in g.out.iter().enumerate() {
            for t in targets {
                forward += 1;
                assert!(g.inc[*t as usize].contains(&(a as u32)), "{} is imported by {}", g.files[*t as usize], g.files[a]);
            }
        }
        assert_eq!(forward, g.edges);
        assert_eq!(g.inc.iter().map(Vec::len).sum::<usize>(), forward, "no dependent that is not an import");
        assert_eq!(imported_by(&g, "src/util.rs"), ["src/config.rs", "src/lib.rs", "src/net/mod.rs", "src/net/tcp.rs"]);
        assert_eq!(imported_by(&g, "src/lib.rs"), Vec::<String>::new());
        // A file never imports itself, and one import written twice is one edge.
        let g = graph_of(&[("a.ts", "import './a';\nimport './b';\nimport { x } from './b';\n"), ("b.ts", "")]);
        assert_eq!(imports(&g, "a.ts"), ["b.ts"]);
        assert_eq!(g.edges, 1);
    }

    #[test]
    fn a_cycle_does_not_hang_the_ranking() {
        let g = graph_of(&[("a.ts", "import './b';\nimport './a';\n"), ("b.ts", "import './c';\n"), ("c.ts", "import './a';\n"), ("lone.ts", "")]);
        let r = g.rank(&[g.id_of("a.ts").unwrap()]);
        assert_eq!(r.len(), 4);
        assert!(r.iter().all(|x| x.is_finite() && *x >= 0.0), "{r:?}");
        assert!((r.iter().sum::<f64>() - 1.0).abs() < 1e-9, "the ranks sum to one: {r:?}");
        let seed = r[g.id_of("a.ts").unwrap() as usize];
        assert!(r.iter().all(|x| *x <= seed + 1e-12), "the seed ranks highest: {r:?}");
        assert!(r[g.id_of("lone.ts").unwrap() as usize] < seed / 4.0, "a file nothing leads to gets only what comes back to the seed: {r:?}");
        // Same graph and seeds, same ranks to the last digit; and no seeds is a rank over all files.
        assert_eq!(r, g.rank(&[g.id_of("a.ts").unwrap()]));
        let flat = g.rank(&[]);
        assert!((flat.iter().sum::<f64>() - 1.0).abs() < 1e-9);
        assert!(Graph::default().rank(&[]).is_empty());
        // A big ring of imports, as many rounds as a small one.
        let text: Vec<(String, String)> = (0..2_000).map(|i| (format!("m{i:04}.ts"), format!("import './m{:04}';\n", (i + 1) % 2_000))).collect();
        let refs: Vec<(&str, &str)> = text.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let ring = graph_of(&refs);
        assert_eq!(ring.edges, 2_000);
        assert!((ring.rank(&[0]).iter().sum::<f64>() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_file_that_many_files_import_does_not_take_the_rank() {
        // Fifty files import `index.ts`; the seed imports `deep.ts` once. Without the cut the barrel would pass on a fiftieth of its rank to each of them and keep most of it.
        let mut files: Vec<(String, String)> = (0..50).map(|i| (format!("f{i:02}.ts"), "import './index';\n".to_string())).collect();
        files.push(("index.ts".into(), String::new()));
        files.push(("seed.ts".into(), "import './deep';\nimport './index';\n".into()));
        files.push(("deep.ts".into(), String::new()));
        let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let g = graph_of(&refs);
        let r = g.rank(&[g.id_of("seed.ts").unwrap()]);
        let (hub, deep) = (r[g.id_of("index.ts").unwrap() as usize], r[g.id_of("deep.ts").unwrap() as usize]);
        // Worked out by hand (the test is what shows it holds): with no cut the barrel ends with about three times the rank of `deep`; with the cut `deep` has about twice the barrel's.
        assert!(deep > hub, "deep {deep} against the barrel {hub}");
        assert!((r.iter().sum::<f64>() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn use_paths_expand_groups_aliases_and_globs() {
        let p = |s: &str| use_paths(s).into_iter().map(|v| v.join("::")).collect::<Vec<_>>();
        assert_eq!(p("a::b::{c, d::e, self}"), ["a::b::c", "a::b::d::e", "a::b::self"]);
        assert_eq!(p("a::{b as c, d::*}"), ["a::b", "a::d"]);
        assert_eq!(p("crate::x::{self as y, z}"), ["crate::x::self", "crate::x::z"]);
        assert_eq!(p("::std::io"), ["std::io"]);
        let nested = |depth: usize| format!("a::{}j{}", "{b::".repeat(depth), "}".repeat(depth));
        assert_eq!(p(&nested(3)), ["a::b::b::b::j"], "a few groups deep are followed");
        assert!(p(&nested(12)).is_empty(), "a group nested more than {MAX_GROUP_DEPTH} deep is skipped, not followed");
        assert!(p("a::b; evil()").is_empty() && p("\"not a path\"").is_empty(), "anything that is no path is no import");
        let many: String = (0..200).map(|i| format!("m{i}")).collect::<Vec<_>>().join(", ");
        assert_eq!(p(&format!("a::{{{many}}}")).len(), MAX_USE_PATHS, "a statement stands for at most {MAX_USE_PATHS} paths");
        assert_eq!(classify_use(vec!["crate".into(), "a".into(), "self".into()]), Some(RawImport::Rust { root: RustRoot::Crate, path: vec!["a".into()] }));
        assert_eq!(classify_use(vec!["super".into(), "super".into(), "x".into()]), Some(RawImport::Rust { root: RustRoot::Super(2), path: vec!["x".into()] }));
        assert_eq!(classify_use(vec!["$crate".into(), "x".into()]), None);
    }

    #[test]
    fn a_use_over_several_lines_and_a_comment_inside_it_are_read() {
        let text = "use crate::a::{\n    b, // the b\n    c::D,\n};\nuse crate::e;\n";
        let got = imports_of("x.rs", text);
        let paths: Vec<String> = got.iter().map(|i| match i {
            RawImport::Rust { path, .. } => path.join("::"),
            other => panic!("{other:?}"),
        }).collect();
        assert_eq!(paths, ["a::b", "a::c::D", "e"]);
    }

    #[test]
    fn a_crate_path_written_in_code_is_an_import_and_one_in_a_string_or_a_comment_is_not() {
        let g = graph_of(&[
            ("src/lib.rs", "pub mod flags;\npub mod util;\npub mod server;\npub mod net;\n"),
            ("src/flags.rs", "pub fn is_on() -> bool { true }\n"),
            ("src/util.rs", "pub fn helper() {}\n"),
            ("src/net.rs", "pub struct Conn;\n"),
            (
                "src/server.rs",
                "fn f() {\n    if crate::flags::is_on() {\n        let s = \"crate::util::helper\";\n    }\n    // crate::util::helper in a comment\n    let t = $crate::util::helper; let u = my_crate::util::helper;\n    let c = crate::net::Conn::new();\n}\n",
            ),
        ]);
        assert_eq!(imports(&g, "src/server.rs"), ["src/flags.rs", "src/net.rs"], "a path in code, up to the type; not one in a string, a comment, `$crate` or `my_crate`");
        assert_eq!(imported_by(&g, "src/flags.rs"), ["src/lib.rs", "src/server.rs"]);
        // A `use` of the same module is the same edge.
        let g = graph_of(&[("src/lib.rs", "pub mod a;\npub mod b;\n"), ("src/a.rs", "pub fn x() {}\n"), ("src/b.rs", "use crate::a::x;\nfn g() { crate::a::x(); crate::a::x::<u8>(); }\n")]);
        assert_eq!((imports(&g, "src/b.rs"), g.edges), (vec!["src/a.rs".to_string()], 3));
    }

    #[test]
    fn reading_imports_is_bounded_by_the_size_of_the_file() {
        // 60,000 statements that never end: each would follow the next 40 lines. The budget of the file (four times its size) stops that, and no more than 20,000 lines are looked at.
        let text = "use a::{\n".repeat(60_000);
        let s = scan_imports("big.rs", &text);
        assert!(s.looks <= MAX_LINES * 6, "{} looks for {} lines", s.looks, MAX_LINES);
        assert!(s.imports.is_empty());
        // A line of minified code is not looked at, and a file of an unread language gives nothing.
        let long = format!("import x from '{}';\n", "a".repeat(800));
        assert!(imports_of("a.ts", &long).is_empty());
        assert!(imports_of("a.java", "import a.b.C;\n").is_empty() && !reads_imports("a.java") && reads_imports("A.TSX"));
        // At most 400 imports are kept for one file.
        let many: String = (0..1_000).map(|i| format!("import './m{i}';\n")).collect();
        assert_eq!(imports_of("a.ts", &many).len(), MAX_IMPORTS_PER_FILE);
        // A byte order mark and Windows line ends change nothing.
        assert_eq!(imports_of("a.ts", "\u{feff}import './a';\r\nimport './b';\r\n").len(), 2);
    }

    #[test]
    fn the_edges_kept_stop_at_the_cap_and_say_so() {
        let text: Vec<(String, String)> = (0..10).map(|i| (format!("m{i}.ts"), (0..10).map(|j| format!("import './m{j}';\n")).collect::<String>())).collect();
        let scanned: Vec<(String, Vec<RawImport>)> = text.iter().map(|(a, b)| (a.clone(), imports_of(a, b))).collect();
        let inputs: Vec<GraphFile<'_>> = scanned.iter().map(|(rel, imports)| GraphFile { rel, imports }).collect();
        let g = build_with_cap(&inputs, &[], 25);
        assert!(g.capped && g.edges == 25, "{} edges", g.edges);
        assert_eq!(g.inc.iter().map(Vec::len).sum::<usize>(), 25);
        assert!(!build_with_cap(&inputs, &[], 1_000).capped);
    }

    #[test]
    fn a_name_is_found_where_it_is_used_and_not_where_it_is_a_part_of_another() {
        let text = "use crate::config::parse_config;\n\npub fn parse_config() {}\n\n// parse_config in a comment\nfn main() {\n    let a = parse_config(1);\n    let b = reparse_config(2);\n    let c = parse_config_file(3);\n    let d = parse_config;\n    parse_config!(x);\n}\n#[cfg(test)]\nmod tests {\n    fn t() { parse_config(); }\n}\n";
        let defs: HashSet<usize> = [3].into_iter().collect();
        let in_test = |line: usize| line >= 14;
        let (got, total) = mentions("lib.rs", "src/lib.rs", text, "parse_config", &defs, &in_test, 20);
        let show: Vec<(usize, &str)> = got.iter().map(|m| (m.line, m.tag)).collect();
        assert_eq!(show, [(1, "import"), (7, "call"), (10, "ref"), (11, "call"), (15, "test")], "{got:?}");
        assert_eq!(total, 5);
        // `keep` shows the first lines and the count is of all.
        let (some, total) = mentions("lib.rs", "src/lib.rs", text, "parse_config", &defs, &in_test, 2);
        assert_eq!((some.len(), total), (2, 5));
        // A file of tests tags every line `test`, but an import is still an import; `#` is a comment in Python and not in Rust.
        let (t, _) = mentions("a.py", "tests/test_a.py", "# parse_config\nfrom a import parse_config\nparse_config()\n", "parse_config", &HashSet::new(), &|_| false, 9);
        assert_eq!(t.iter().map(|m| (m.line, m.tag)).collect::<Vec<_>>(), [(2, "import"), (3, "test")]);
        let (r, _) = mentions("a.rs", "src/a.rs", "#[derive(parse_config)]\n", "parse_config", &HashSet::new(), &|_| false, 9);
        assert_eq!(r.len(), 1);
        // A long line of code that has the name far along is found and shown around the name; one of more than 4 KiB is not looked at.
        let long = format!("const DOC: &str = \"{}\"; use_it(parse_config);\n", "x".repeat(1_500));
        let (m, total) = mentions("a.rs", "src/a.rs", &long, "parse_config", &HashSet::new(), &|_| false, 9);
        assert_eq!(total, 1);
        assert!(m[0].text.starts_with('…') && m[0].text.contains("use_it(parse_config);") && m[0].text.chars().count() <= 110, "{}", m[0].text);
        assert_eq!(mentions("a.rs", "src/a.rs", &format!("{}parse_config\n", "x ".repeat(3_000)), "parse_config", &HashSet::new(), &|_| false, 9).1, 0);
        assert_eq!(symbol_name("Config::parse_config").as_deref(), Some("parse_config"));
        assert_eq!(symbol_name(" store.load() ").as_deref(), Some("load"));
        assert_eq!((symbol_name("a b"), symbol_name("9x"), symbol_name("")), (None, None, None));
        assert!(is_test_path("core/tests/x.rs") && is_test_path("web/a.test.ts") && is_test_path("pkg/test_a.py") && is_test_path("go/a_test.go") && !is_test_path("src/latest.rs"));
    }
}
