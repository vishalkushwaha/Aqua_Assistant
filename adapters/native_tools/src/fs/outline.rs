//! An outline of a source file: the declarations in it and the lines each one spans (card OA5-03, CX-17).
//!
//! A file of 300 lines costs 3,000 characters to read, and the model sees the first 2,700 of them (a 6,144-token window); an outline of it is a tenth of that and says where each function
//! is, so the model asks `fs.read` for exactly that range. It is made from the shape of the lines (the keyword that begins a declaration, the braces or the indentation that end it),
//! not by a parser: a file that does not parse still has an outline, and a tree-sitter grammar for each language (which would need crates this build does not have) would make the
//! ranges exact where a macro or a string breaks the count of braces. It says so in its first line. Nothing here reads a file: the caller gives the text.

/// One declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// `fn`, `struct`, `enum`, `trait`, `impl`, `mod`, `const`, `static`, `type`, `class`, `interface`, `method`, `h1` to `h6`, ...
    pub kind: &'static str,
    pub name: String,
    /// The first and the last line it spans (1-based, inclusive).
    pub start: usize,
    pub end: usize,
    /// How many declarations it is inside (a method of a class is 1).
    pub depth: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outline {
    pub language: &'static str,
    pub total_lines: usize,
    pub items: Vec<Item>,
}

/// How many declarations an outline shows at most, and how deep.
pub const MAX_ITEMS: usize = 80;
const MAX_DEPTH: usize = 2;
/// A line longer than this is not looked at for a declaration (minified code).
const MAX_DECL_LINE: usize = 1000;
/// The most lines looked at to find where one declaration ends.
const MAX_SCAN_LINES: usize = 20_000;

/// The most bytes looked at in all the searches for the end of a declaration, for one outline: eight times the size of the text (a method in a class in a module is looked at three times)
/// and at least 200 KB. Without it a file of thousands of `fn a() {` lines that are never closed makes each of them read the next `MAX_SCAN_LINES` lines: a few seconds of one core for
/// 60,000 lines, and the tool call cannot be cut short while it runs. A declaration that finds the budget spent ends where it begins, as one whose end is not found does.
fn scan_budget(text: &str) -> usize {
    text.len().saturating_mul(8).max(200_000)
}

/// Takes the bytes of one line (and its line break) from the budget; false when there are not enough left.
fn spend(budget: &mut usize, line: &str) -> bool {
    let cost = line.len() + 1;
    if *budget < cost {
        *budget = 0;
        return false;
    }
    *budget -= cost;
    true
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    Braces,
    Indent,
    Markdown,
}

fn language_of(file_name: &str) -> (&'static str, Style) {
    let ext = file_name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "rs" => ("rust", Style::Braces),
        "py" | "pyi" => ("python", Style::Indent),
        "ts" | "tsx" | "mts" | "cts" => ("typescript", Style::Braces),
        "js" | "jsx" | "mjs" | "cjs" => ("javascript", Style::Braces),
        "go" => ("go", Style::Braces),
        "md" | "markdown" => ("markdown", Style::Markdown),
        "java" | "kt" | "cs" | "c" | "h" | "cpp" | "hpp" | "cc" | "swift" | "php" | "scala" => ("braces", Style::Braces),
        _ => ("text", Style::Braces),
    }
}

/// The words that may stand in front of a declaration.
const MODIFIERS: &[&str] = &["pub", "export", "default", "declare", "async", "unsafe", "extern", "public", "private", "protected", "internal", "static", "abstract", "final", "override", "readonly", "open", "sealed", "partial", "virtual"];

/// `pub(crate) async fn name` -> (`async fn name` without its modifiers = `fn name`, ...): the rest of a line after the modifiers.
fn after_modifiers(line: &str) -> &str {
    let mut rest = line.trim_start();
    loop {
        let word_end = rest.find(|c: char| c.is_whitespace() || c == '(').unwrap_or(rest.len());
        let word = &rest[..word_end];
        if !MODIFIERS.contains(&word) {
            return rest;
        }
        rest = rest[word_end..].trim_start();
        // `pub(crate)`, `extern "C"`.
        if rest.starts_with('(') {
            if let Some(close) = rest.find(')') {
                rest = rest[close + 1..].trim_start();
            }
        }
        if word == "extern" && rest.starts_with('"') {
            if let Some(close) = rest[1..].find('"') {
                rest = rest[close + 2..].trim_start();
            }
        }
    }
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// The identifier at the start of `s` (empty when there is none).
fn ident(s: &str) -> &str {
    let end = s.find(|c: char| !is_ident_char(c)).unwrap_or(s.len());
    &s[..end]
}

/// A name for the output: only what a name is made of, and not longer than it needs to be.
fn clean(name: &str) -> String {
    name.chars().filter(|c| !c.is_control()).take(60).collect::<String>().trim().to_string()
}

/// The declaration a line begins, as (kind, name), for a language that is written with braces. `None` for any other line.
fn declaration(line: &str, language: &str) -> Option<(&'static str, String)> {
    if line.len() > MAX_DECL_LINE {
        return None;
    }
    let rest = after_modifiers(line);
    let first = ident(rest);
    let after = rest[first.len()..].trim_start();
    let named = |kind: &'static str, tail: &str| -> Option<(&'static str, String)> {
        let tail = tail.trim_start().trim_start_matches('*').trim_start();
        let n = ident(tail);
        (!n.is_empty()).then(|| (kind, clean(n)))
    };
    match (language, first) {
        ("rust", "fn") => named("fn", after),
        ("rust", "const") => {
            let rest = after;
            if let Some(f) = rest.strip_prefix("fn ") {
                return named("fn", f);
            }
            if let Some(f) = rest.strip_prefix("async fn ").or_else(|| rest.strip_prefix("unsafe fn ")) {
                return named("fn", f);
            }
            named("const", rest)
        }
        ("rust", "struct") => named("struct", after),
        ("rust", "enum") => named("enum", after),
        ("rust", "trait") => named("trait", after),
        ("rust", "union") => named("union", after),
        ("rust", "mod") => named("mod", after),
        ("rust", "type") => named("type", after),
        ("rust", "static") => named("static", after.strip_prefix("mut ").unwrap_or(after)),
        ("rust", "impl") => {
            // `impl<T> Trait for Type<T> where ...`: the text up to the brace or the where clause.
            let head = after.split(" where ").next().unwrap_or(after);
            let head = head.split('{').next().unwrap_or(head).trim();
            let head = if head.starts_with('<') { head.find('>').map(|i| head[i + 1..].trim()).unwrap_or(head) } else { head };
            (!head.is_empty()).then(|| ("impl", clean(head)))
        }
        ("rust", "macro_rules") => after.strip_prefix('!').and_then(|a| named("macro", a)),
        ("go", "func") => {
            // `func (r *T) Name(` or `func Name(`.
            let tail = if after.starts_with('(') { after.find(')').map(|i| after[i + 1..].trim_start()).unwrap_or("") } else { after };
            named("func", tail)
        }
        ("go", "type") => named("type", after),
        ("typescript" | "javascript", "function") => named("function", after),
        ("typescript" | "javascript", "class") => named("class", after),
        ("typescript", "interface") => named("interface", after),
        ("typescript", "type") => named("type", after),
        ("typescript", "enum") => named("enum", after),
        ("typescript", "namespace") => named("namespace", after),
        ("typescript" | "javascript", "const" | "let" | "var") => {
            let n = ident(after);
            if n.is_empty() {
                return None;
            }
            let value = after[n.len()..].trim_start();
            let value = value.strip_prefix(':').map(|v| v.split('=').nth(1).unwrap_or("")).unwrap_or(value).trim_start();
            let value = value.strip_prefix('=').unwrap_or(value).trim_start();
            let is_function = value.starts_with("function") || value.starts_with("async") || value.contains("=>") || value.starts_with('(');
            Some((if is_function { "function" } else { "const" }, clean(n)))
        }
        ("braces" | "text", "fn" | "func" | "function" | "def" | "struct" | "enum" | "impl" | "class" | "interface" | "trait" | "namespace" | "module" | "mod" | "type") => {
            let kind: &'static str = match first {
                "fn" | "func" | "function" | "def" => "fn",
                "struct" => "struct",
                "enum" => "enum",
                "impl" => "impl",
                "class" => "class",
                "interface" => "interface",
                "trait" => "trait",
                "namespace" | "module" | "mod" => "mod",
                _ => "type",
            };
            named(kind, after)
        }
        _ => None,
    }
}

/// The method a line inside a class begins (`  async load(path) {`, `  private get name(): string {`). `None` for any other line, and for the words that begin a statement.
fn method(line: &str) -> Option<String> {
    if line.len() > MAX_DECL_LINE || !line.trim_end().ends_with('{') {
        return None;
    }
    let rest = after_modifiers(line);
    let rest = rest.strip_prefix("get ").or_else(|| rest.strip_prefix("set ")).unwrap_or(rest).trim_start_matches('*').trim_start();
    let n = ident(rest);
    if n.is_empty() || !rest[n.len()..].trim_start().starts_with(['(', '<']) {
        return None;
    }
    const STATEMENTS: &[&str] = &["if", "for", "while", "switch", "catch", "function", "return", "else", "do", "try", "with", "await", "new", "typeof"];
    (!STATEMENTS.contains(&n)).then(|| clean(n))
}

fn leading_ws(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

/// The line on which the declaration that begins at line index `start` ends, found by its braces: strings, character literals and comments are stepped over; a declaration with no body ends
/// at its `;`. A declaration whose end is not found (a macro that makes the braces uneven, a string that goes on over several lines) ends where it begins.
fn braces_end(lines: &[&str], start: usize, rust: bool, budget: &mut usize) -> usize {
    let (mut depth, mut parens, mut started, mut in_comment) = (0i64, 0i64, false, false);
    for (n, line) in lines.iter().enumerate().skip(start).take(MAX_SCAN_LINES) {
        if !spend(budget, line) {
            return start;
        }
        let b = line.as_bytes();
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if in_comment {
                if c == b'*' && b.get(i + 1) == Some(&b'/') {
                    in_comment = false;
                    i += 1;
                }
                i += 1;
                continue;
            }
            match c {
                b'/' if b.get(i + 1) == Some(&b'/') => break,
                b'/' if b.get(i + 1) == Some(&b'*') => {
                    in_comment = true;
                    i += 1;
                }
                b'"' | b'`' => {
                    // To the closing quote on this line; a backslash escapes the next byte.
                    let mut j = i + 1;
                    while j < b.len() && b[j] != c {
                        j += if b[j] == b'\\' { 2 } else { 1 };
                    }
                    i = j;
                }
                b'\'' => {
                    // A character literal (`'x'`, `'\n'`) is stepped over; a lifetime (`'a`) is not a quote.
                    if rust {
                        if b.get(i + 1) == Some(&b'\\') && b.get(i + 3) == Some(&b'\'') {
                            i += 3;
                        } else if b.get(i + 2) == Some(&b'\'') {
                            i += 2;
                        }
                    } else {
                        let mut j = i + 1;
                        while j < b.len() && b[j] != b'\'' {
                            j += if b[j] == b'\\' { 2 } else { 1 };
                        }
                        i = j;
                    }
                }
                b'(' | b'[' => parens += 1,
                b')' | b']' => parens -= 1,
                b'{' => {
                    depth += 1;
                    started = true;
                }
                b'}' => {
                    depth -= 1;
                    if started && depth <= 0 {
                        return n;
                    }
                }
                b';' if !started && parens <= 0 && depth == 0 => return n,
                _ => {}
            }
            i += 1;
        }
        // A declaration that has neither a body nor an end within a few lines was not one.
        if !started && n >= start + 12 {
            break;
        }
    }
    start
}

/// The last line of the block of the Python declaration at index `start`, which is indented `indent`: the last line before the next line that is no deeper.
fn indent_end(lines: &[&str], start: usize, indent: usize, budget: &mut usize) -> usize {
    let mut end = start;
    for (n, line) in lines.iter().enumerate().skip(start + 1).take(MAX_SCAN_LINES) {
        if !spend(budget, line) {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        if leading_ws(line) <= indent {
            break;
        }
        end = n;
    }
    end
}

/// A Python declaration (`def`, `async def`, `class`) at the start of a line, as (kind, name).
fn python_declaration(line: &str) -> Option<(&'static str, String)> {
    if line.len() > MAX_DECL_LINE {
        return None;
    }
    let t = line.trim_start();
    let t = t.strip_prefix("async ").unwrap_or(t);
    let (kind, rest): (&'static str, &str) = if let Some(r) = t.strip_prefix("def ") {
        ("fn", r)
    } else if let Some(r) = t.strip_prefix("class ") {
        ("class", r)
    } else {
        return None;
    };
    let n = ident(rest.trim_start());
    (!n.is_empty()).then(|| (kind, clean(n)))
}

/// The outline of `text`, a file called `file_name`.
pub fn outline(file_name: &str, text: &str) -> Outline {
    let (language, style) = language_of(file_name);
    let lines: Vec<&str> = text.lines().map(|l| l.strip_prefix('\u{feff}').unwrap_or(l)).collect();
    let total_lines = lines.len();
    let mut budget = scan_budget(text);
    let mut found: Vec<Item> = Vec::new();
    match style {
        Style::Markdown => {
            let mut fence = false;
            let mut heads: Vec<(usize, usize, String)> = Vec::new();
            for (n, l) in lines.iter().enumerate() {
                let t = l.trim_start();
                if t.starts_with("```") || t.starts_with("~~~") {
                    fence = !fence;
                    continue;
                }
                if fence {
                    continue;
                }
                let level = t.chars().take_while(|c| *c == '#').count();
                if (1..=6).contains(&level) && t[level..].starts_with(' ') {
                    heads.push((n, level, clean(t[level..].trim().trim_end_matches('#').trim())));
                }
            }
            for (i, (n, level, name)) in heads.iter().enumerate() {
                let end = heads[i + 1..].iter().find(|(_, l, _)| l <= level).map(|(m, _, _)| m - 1).unwrap_or(total_lines.saturating_sub(1));
                let kind: &'static str = ["h1", "h2", "h3", "h4", "h5", "h6"][level - 1];
                found.push(Item { kind, name: name.clone(), start: n + 1, end: end + 1, depth: 0 });
            }
        }
        Style::Indent => {
            for (n, l) in lines.iter().enumerate() {
                if let Some((kind, name)) = python_declaration(l) {
                    let end = indent_end(&lines, n, leading_ws(l), &mut budget);
                    found.push(Item { kind, name, start: n + 1, end: end + 1, depth: 0 });
                }
            }
        }
        Style::Braces => {
            let rust = language == "rust";
            // `end` of the class that the lines being read are inside, for the methods of a class.
            let mut class_end: Option<usize> = None;
            for (n, l) in lines.iter().enumerate() {
                if class_end.is_some_and(|e| n > e) {
                    class_end = None;
                }
                if let Some((kind, name)) = declaration(l, language) {
                    let end = braces_end(&lines, n, rust, &mut budget);
                    if kind == "class" {
                        class_end = Some(end);
                    }
                    found.push(Item { kind, name, start: n + 1, end: end + 1, depth: 0 });
                } else if class_end.is_some() && matches!(language, "typescript" | "javascript") {
                    if let Some(name) = method(l) {
                        let end = braces_end(&lines, n, false, &mut budget);
                        found.push(Item { kind: "method", name, start: n + 1, end: end + 1, depth: 0 });
                    }
                }
            }
        }
    }
    // How deep each one is: the declarations whose range holds it.
    let mut stack: Vec<usize> = Vec::new();
    for item in &mut found {
        while stack.last().is_some_and(|end| *end < item.start) {
            stack.pop();
        }
        item.depth = stack.len();
        stack.push(item.end);
    }
    found.retain(|i| i.depth <= MAX_DEPTH);
    Outline { language, total_lines, items: found }
}

impl Outline {
    /// The text of an outline: a first line that says what it is, then one line for each declaration (`12-48 fn parse`), indented by how deep it is, at most `max_items` of them and no
    /// more than `budget_chars` characters; what is left out is counted.
    pub fn render(&self, max_items: usize, budget_chars: usize) -> String {
        let head = format!("outline of {} lines ({}, from the shape of the lines, not a parse): {} declarations", self.total_lines, self.language, self.items.len());
        let lines: Vec<String> = self
            .items
            .iter()
            .map(|i| {
                let span = if i.end > i.start { format!("{}-{}", i.start, i.end) } else { format!("{}", i.start) };
                format!("{span} {}{} {}", "  ".repeat(i.depth), i.kind, i.name)
            })
            .collect();
        if lines.is_empty() {
            return format!("{head}\n(no declarations found; read the file with fs.read)");
        }
        let room = budget_chars.saturating_sub(head.chars().count() + 120);
        let shown = super::page::fit_lines(&lines[..lines.len().min(max_items)], room);
        let mut out = format!("{head}\n{}", lines[..shown].join("\n"));
        if shown < lines.len() {
            out.push_str(&format!("\n… {} more declarations not shown; read a range with fs.read offset_line", lines.len() - shown));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lines of the outline as the text shows them, without its first line.
    fn shape(o: &Outline) -> Vec<String> {
        o.render(MAX_ITEMS, 1_000_000).lines().skip(1).map(str::to_string).collect()
    }

    const RUST: &str = r#"//! A module.
use std::fmt;

/// A thing.
#[derive(Debug)]
pub struct Thing {
    name: String,
    n: u32,
}

pub enum Kind {
    A,
    B,
}

impl Thing {
    pub fn new(name: &str) -> Self {
        let s = "a { in a string";
        Self { name: s.into(), n: 0 }
    }

    pub(crate) async fn load(&self, path: &str) -> Result<(), ()> {
        // a brace in a comment }
        let c = '}';
        Ok(())
    }
}

impl fmt::Display for Thing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

pub trait Run {
    fn run(&self);
    fn stop(&self) -> bool;
}

pub const LIMIT: usize = 10;
pub type Pair = (u8, u8);
const fn twice(x: u32) -> u32 { x * 2 }
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(1, 1);
    }
}
"#;

    #[test]
    fn a_rust_file_has_its_declarations_with_the_lines_they_span() {
        let o = outline("lib.rs", RUST);
        assert_eq!(o.language, "rust");
        assert_eq!(
            shape(&o),
            vec![
                "6-9 struct Thing",
                "11-14 enum Kind",
                "16-27 impl Thing",
                "17-20   fn new",
                "22-26   fn load",
                "29-33 impl fmt::Display for Thing",
                "30-32   fn fmt",
                "35-38 trait Run",
                "36   fn run",
                "37   fn stop",
                "40 const LIMIT",
                "41 type Pair",
                "42 fn twice",
                "43-48 mod tests",
                "45-47   fn it_works",
            ]
        );
        assert_eq!(o.total_lines, 48);
    }

    #[test]
    fn a_python_file_ends_a_block_where_the_indentation_comes_back() {
        let py = "import os\n\n\nclass Loader:\n    \"\"\"doc\"\"\"\n\n    def load(self, path):\n        return open(path)\n\n    async def close(self):\n        pass\n\n\ndef main():\n    Loader().load('x')\n";
        let o = outline("tool.py", py);
        assert_eq!(shape(&o), vec!["4-11 class Loader", "7-8   fn load", "10-11   fn close", "14-15 fn main"]);
    }

    #[test]
    fn a_typescript_file_has_functions_classes_methods_and_arrow_functions() {
        let ts = "import { a } from './a';\n\nexport interface Options {\n  path: string;\n}\n\nexport type Id = string;\n\nexport class Store {\n  private items = new Map<string, number>();\n\n  async load(path: string): Promise<void> {\n    if (path) {\n      await go(path);\n    }\n  }\n\n  get size(): number {\n    return this.items.size;\n  }\n}\n\nexport const make = (o: Options) => {\n  return new Store();\n};\n\nexport const LIMIT = 10;\n\nexport default function main() {\n  return make({ path: 'x' });\n}\n";
        let o = outline("store.ts", ts);
        assert_eq!(
            shape(&o),
            vec!["3-5 interface Options", "7 type Id", "9-21 class Store", "12-16   method load", "18-20   method size", "23-25 function make", "27 const LIMIT", "29-31 function main"]
        );
    }

    #[test]
    fn a_javascript_file_and_a_go_file_and_a_markdown_file_have_outlines_too() {
        let js = "function a() {\n  return 1;\n}\nconst b = function () {\n  return 2;\n};\nclass C {\n  m() {\n    return 3;\n  }\n}\n";
        assert_eq!(shape(&outline("x.js", js)), vec!["1-3 function a", "4-6 function b", "7-11 class C", "8-10   method m"]);
        let go = "package main\n\ntype Server struct {\n\tn int\n}\n\nfunc (s *Server) Run() error {\n\treturn nil\n}\n\nfunc main() {\n}\n";
        assert_eq!(shape(&outline("main.go", go)), vec!["3-5 type Server", "7-9 func Run", "11-12 func main"]);
        let md = "# Title\n\ntext\n\n## One\n\n```\n# not a heading\n```\n\n## Two\n\n### Deep\n\nend\n";
        assert_eq!(shape(&outline("README.md", md)), vec!["1-15 h1 Title", "5-10   h2 One", "11-15   h2 Two", "13-15     h3 Deep"]);
    }

    #[test]
    fn a_file_with_no_declarations_says_so_and_an_unknown_language_still_has_the_common_keywords() {
        let o = outline("notes.txt", "just\nsome text\n");
        assert!(o.items.is_empty() && o.render(80, 2400).contains("no declarations found"), "{}", o.render(80, 2400));
        let c = "int x;\nstruct point {\n  int a;\n};\nclass Foo {\n  void m() {}\n};\n";
        assert_eq!(shape(&outline("a.cpp", c)), vec!["2-4 struct point", "5-7 class Foo"]);
        assert_eq!(outline("empty.rs", "").items.len(), 0);
    }

    #[test]
    fn a_declaration_that_never_ends_ends_where_it_begins_and_minified_lines_are_not_declarations() {
        // A string that goes over lines breaks the count of braces: the item is kept, with no range.
        let broken = "fn a() {\n    let s = \"\n}\n";
        assert_eq!(outline("a.rs", broken).items[0].start, 1);
        let minified = format!("{} fn hidden() {{}}", "x".repeat(2000));
        assert!(outline("a.rs", &minified).items.is_empty());
        // A `;` inside the arguments of a declaration with no body does not end it early.
        let trait_fn = "trait T {\n    fn f(a: [u8; 4]) -> u32;\n}\n";
        assert_eq!(shape(&outline("t.rs", trait_fn)), vec!["1-3 trait T", "2   fn f"]);
    }

    #[test]
    fn the_output_is_cut_at_the_budget_and_at_the_number_of_items_and_says_what_it_left_out() {
        let many: String = (0..200).map(|i| format!("fn function_number_{i}() {{}}\n")).collect();
        let o = outline("many.rs", &many);
        assert_eq!(o.items.len(), 200);
        let text = o.render(MAX_ITEMS, 100_000);
        assert_eq!(text.lines().count(), 1 + MAX_ITEMS + 1, "{text}");
        assert!(text.ends_with("… 120 more declarations not shown; read a range with fs.read offset_line"), "{text}");
        let small = o.render(MAX_ITEMS, 600);
        assert!(small.chars().count() < 800 && small.contains("more declarations not shown"), "{small}");
        assert!(text.starts_with("outline of 200 lines (rust, from the shape of the lines, not a parse): 200 declarations"));
    }

    #[test]
    fn a_file_of_declarations_that_never_end_is_outlined_in_time_and_each_ends_where_it_begins() {
        // 60,000 `fn a() {` lines with no closing brace. Without the scan budget each of them read the next 20,000 lines (1.2 billion lines in all: tens of seconds of one core).
        let hostile = "fn a() {\n".repeat(60_000);
        let started = std::time::Instant::now();
        let o = outline("hostile.rs", &hostile);
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "took {:?}", started.elapsed());
        assert_eq!(o.total_lines, 60_000);
        // The last declarations found the budget spent: they end where they begin.
        let last = o.items.last().expect("an item");
        assert_eq!((last.start, last.end), (60_000, 60_000));
        // The same goes for Python blocks that nest deeper with every line: each one's block runs to the end of the file (4.5 billion bytes in all without the budget).
        let mut nested = String::new();
        for i in 0..3_000 {
            nested.push_str(&" ".repeat(i));
            nested.push_str("def f():\n");
        }
        let started = std::time::Instant::now();
        let p = outline("nested.py", &nested);
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "took {:?}", started.elapsed());
        assert_eq!(p.total_lines, 3_000);
    }

    #[test]
    fn the_scan_budget_is_eight_times_the_text_and_not_less_than_200_kb_so_an_ordinary_file_keeps_exact_ranges() {
        assert_eq!(scan_budget(""), 200_000);
        assert_eq!(scan_budget(&"x".repeat(1_000_000)), 8_000_000);
        let mut b = 10;
        assert!(spend(&mut b, "abcd")); // 4 bytes and a line break
        assert_eq!(b, 5);
        assert!(!spend(&mut b, "abcdef")); // 7 are not left
        assert_eq!(b, 0);
        // A file of 3,000 small functions inside one module is far inside the budget: every range is exact.
        let mut text = String::from("mod m {\n");
        for i in 0..3_000 {
            text.push_str(&format!("    fn f{i}() {{\n        let x = {i};\n    }}\n"));
        }
        text.push_str("}\n");
        let o = outline("m.rs", &text);
        assert_eq!(o.items.len(), 3_001);
        assert_eq!((o.items[0].start, o.items[0].end), (1, 9_002));
        assert_eq!((o.items[3_000].start, o.items[3_000].end), (8_999, 9_001));
    }
}
