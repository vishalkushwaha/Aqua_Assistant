//! Generates `documentation/TOOLS_REFERENCE.md` and the default-config section of
//! `documentation/TOOLS_CONFIG.md` from the real host, so the docs cannot drift from the code.
//!
//! `cargo test -p aqua-core --lib docs_gen` fails when a file is out of date;
//! `AQUA_UPDATE_DOCS=1 cargo test -p aqua-core --lib docs_gen` rewrites them.

use crate::tool_host::build_tool_host;
use aqua_mcp_adapters::browser::BrowserBackend;
use aqua_mcp_adapters::{new_browser_client, new_windows_client};
use aqua_tool_host::*;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn docs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("documentation")
}

fn domain_title(d: &str) -> &'static str {
    match d {
        "win" => "Desktop (`win.*`) — Windows UI automation",
        "browser" => "Browser (`browser.*`) — Aqua's managed browser",
        "fs" => "Files (`fs.*`)",
        "term" => "Terminal (`term.*`)",
        "check" => "Project checks (`check.*`, `project.*`) — off unless AQUA_CHECK_TOOLS=on",
        "code" => "Code symbols (`code.*`) — off unless AQUA_CODE_TOOLS=on",
        "sys" => "System (`sys.*`)",
        "proc" => "Processes (`proc.*`)",
        "ssh" => "SSH (`ssh.*`)",
        "git" => "Git (`git.*`)",
        "db" => "Databases (`db.*`)",
        "http" | "web" => "Web and HTTP (`http.*`, `web.*`)",
        "docker" => "Docker (`docker.*`)",
        "k8s" => "Kubernetes (`k8s.*`)",
        "iac" => "Terraform (`iac.*`)",
        "forge" => "GitHub and GitLab (`forge.*`)",
        "cloud" => "Cloud inspection (`cloud.*`)",
        "mon" => "Monitoring and logs (`mon.*`)",
        "doc" => "Documents (`doc.*`)",
        "flows" => "AI Flows (`flows.*`)",
        "memory" => "Remembered facts (`memory.*`)",
        "reminder" => "Reminders (`reminder.*`)",
        "screen" => "Screen reading (`screen.*`)",
        "tools" => "Finding tools (`tools.*`)",
        "skill" => "Skills (`skill.*`)",
        "vault" => "Secrets (`vault.*`)",
        "clipboard" => "Clipboard (`clipboard.*`)",
        _ => "Other",
    }
}

fn domain_key(d: &str) -> &str {
    match d {
        "web" => "http",
        "project" => "check",
        other => other,
    }
}

fn what_happens(m: &ToolManifest, engine: &PolicyEngine) -> String {
    let ctx = |target_allowed| PolicyContext { caller: Caller::Agent, tainted: false, private_data: false, target_allowed };
    let d = |t| engine.decide(m, &m.classes, &ctx(t)).decision;
    let word = |x: Decision| match x {
        Decision::Auto => "runs",
        Decision::Ask => "asks",
        Decision::AlwaysAsk => "always asks",
        Decision::Deny => "refused",
    };
    let (open, allowed) = (d(false), d(true));
    if open == allowed {
        word(open).to_string()
    } else {
        format!("{} ({} inside an allowed target)", word(open), word(allowed))
    }
}

/// The front matter (`summary`, `read_when`) every page under `documentation/` carries. This page is rewritten whole by the test below, so its front matter is written here.
const REFERENCE_FRONT: &str = "---\nsummary: \"Every tool the host offers, with its arguments, classes and what the policy does by default. Generated from the code; do not edit by hand.\"\nread_when:\n  - \"needing a tool's exact name, arguments or default decision\"\n  - \"a tool was added, renamed or changed (regenerate with AQUA_UPDATE_DOCS=1 cargo test -p aqua-core --lib docs_gen)\"\n---\n\n";

/// As `REFERENCE_FRONT`, for `CAPABILITIES.md`.
const CAPABILITIES_FRONT: &str = "---\nsummary: \"What Aqua can really do, by area, with a status (implemented, partial, simulated, planned) and the code and test that show it. Generated from capabilities.toml.\"\nread_when:\n  - \"needing the true status of a feature before saying it works\"\n  - \"adding or changing a capability (edit capabilities.toml, then regenerate with AQUA_UPDATE_DOCS=1)\"\n---\n\n";

pub fn render_reference(host: &ToolHost) -> String {
    let engine = PolicyEngine::new(Mode::Standard);
    let mut out = String::from(REFERENCE_FRONT);
    out.push_str("# Tool reference\n\n");
    out.push_str("_Generated from the code by `cargo test -p aqua-core --lib docs_gen` (set `AQUA_UPDATE_DOCS=1` to rewrite). Do not edit by hand._\n\n");
    out.push_str("Every tool below is called through the tool host (`ToolHost::call`), which validates the arguments, decides, asks, masks secrets and audits. ");
    out.push_str("**What happens** is the decision in *standard* mode for the AI agent with nothing untrusted read yet; it can only get stricter (untrusted text read in the run, voice, locked mode, your `policy.json`). ");
    out.push_str("Tools marked *(untrusted)* return text from outside Aqua (files, pages, logs, command output); it is fenced and labelled, and reading it makes later changes in the run always ask.\n\n");
    out.push_str("Argument names ending in `?` are optional. Classes: `READ`, `WRITE`, `EXECUTE`, `NETWORK`, `REMOTE`, `DESTRUCTIVE`, `CREDENTIAL`, `PRIVILEGED`, `EXTERNAL_EFFECT`; the last three (and `DESTRUCTIVE`) always ask.\n\n");
    let catalog = host.catalog();
    let mut domains: Vec<String> = Vec::new();
    for (m, _) in &catalog {
        let k = domain_key(m.domain()).to_string();
        if !domains.contains(&k) {
            domains.push(k);
        }
    }
    let order = ["win", "browser", "fs", "term", "check", "sys", "proc", "ssh", "git", "db", "http", "docker", "k8s", "iac", "cloud", "forge", "mon", "doc", "flows", "memory", "reminder", "screen", "tools", "vault", "clipboard"];
    domains.sort_by_key(|d| order.iter().position(|o| o == d).unwrap_or(99));
    out.push_str(&format!("{} tools in {} groups.\n\n", catalog.len(), domains.len()));
    for d in &domains {
        let tools: Vec<&(ToolManifest, Option<String>)> = catalog.iter().filter(|(m, _)| domain_key(m.domain()) == d).collect();
        out.push_str(&format!("## {}\n\n| Tool | Arguments | What it does | Classes | What happens |\n|---|---|---|---|---|\n", domain_title(d)));
        for (m, _) in tools {
            let sig = m.signature();
            let args = sig.split_once('(').map(|(_, r)| format!("({r}")).unwrap_or_default();
            let classes = m.classes.iter().map(|c| format!("{c:?}").to_uppercase()).collect::<Vec<_>>().join(", ");
            out.push_str(&format!(
                "| `{}` | `{}` | {}{} | {} | {} |\n",
                m.name,
                args.replace('|', "\\|"),
                m.description.replace('|', "\\|"),
                if m.untrusted_output { " *(untrusted)*" } else { "" },
                classes.replace("EXTERNALEFFECT", "EXTERNAL_EFFECT"),
                what_happens(m, &engine)
            ));
        }
        out.push('\n');
    }
    out
}

/// A host built the way `main` builds it, over a throwaway data folder, plus the default config
/// files it wrote there.
fn built(name: &str) -> (Arc<ToolHost>, PathBuf) {
    let root = std::env::temp_dir().join(format!("aqua_docs_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let browser = Arc::new(BrowserBackend::new(&root.join("browser"), new_browser_client()));
    let host = build_tool_host(&data, browser, Arc::new(Mutex::new(new_windows_client())));
    (host, data)
}

pub fn render_default_configs(data: &Path) -> String {
    let mut files: Vec<PathBuf> = std::fs::read_dir(data.join("tools")).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "json") && !p.to_string_lossy().contains("audit")).collect();
    files.sort();
    let mut out = String::new();
    for f in files {
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        let text = std::fs::read_to_string(&f).unwrap_or_default();
        let norm = normalise(&text, data);
        out.push_str(&format!("### Default `data/tools/{name}`\n\n```json\n{}\n```\n\n", norm.trim()));
    }
    out
}

/// Replace machine-specific paths so the generated text is the same everywhere.
fn normalise(text: &str, data: &Path) -> String {
    let root = data.parent().unwrap_or(data).to_string_lossy().to_string();
    let mut t = text.to_string();
    for v in [root.clone(), root.replace('\\', "/"), root.replace('\\', "\\\\")] {
        t = t.replace(&v, "<workspace>");
    }
    for var in ["USERPROFILE", "LOCALAPPDATA", "APPDATA"] {
        if let Ok(v) = std::env::var(var) {
            for x in [v.clone(), v.replace('\\', "/"), v.replace('\\', "\\\\")] {
                if !x.is_empty() {
                    t = t.replace(&x, &format!("%{var}%"));
                }
            }
        }
    }
    t
}

const CONFIG_MARKER: &str = "<!-- BEGIN GENERATED DEFAULTS -->";

fn check_or_write(path: &Path, expected: &str) {
    if std::env::var("AQUA_UPDATE_DOCS").is_ok() {
        std::fs::write(path, expected).unwrap();
        return;
    }
    let actual = std::fs::read_to_string(path).unwrap_or_default();
    let norm = |s: &str| s.replace("\r\n", "\n");
    assert!(norm(&actual) == norm(expected), "{} is out of date. Run: AQUA_UPDATE_DOCS=1 cargo test -p aqua-core --lib docs_gen", path.display());
}

/// The `AQUA_*` environment variables the code reads, found by searching the source for quoted names, as a table for `CONFIGURATION.md`.
/// Test files are left out, and so is this file (its own test strings would count). Each row gives the first file (alphabetically) that mentions the name and how many files do.
#[cfg(test)]
mod env_vars {
    use super::docs_dir;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    pub const MARKER: &str = "<!-- BEGIN GENERATED ENV -->";

    pub(super) fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let path = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if !matches!(name.as_str(), "target" | "node_modules" | "vendor" | "tests" | "dist" | ".git") {
                    rust_files(&path, out);
                }
            } else if name.ends_with(".rs") && !name.ends_with("_test.rs") && !name.ends_with("_tests.rs") && name != "docs_gen.rs" {
                out.push(path);
            }
        }
    }

    /// `"AQUA_NAME"` occurrences in one source text.
    fn names_in(text: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut rest = text;
        while let Some(at) = rest.find("\"AQUA_") {
            let after = &rest[at + 1..];
            let end = after.find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')).unwrap_or(after.len());
            let name = &after[..end];
            // A whole quoted name (the closing quote follows), not a prefix used to build one.
            if after[end..].starts_with('"') && name.len() > "AQUA_".len() && !name.ends_with('_') {
                found.push(name.to_string());
            }
            rest = &after[end..];
        }
        found
    }

    pub fn render(root: &Path) -> String {
        let mut files = Vec::new();
        for top in ["core/src", "adapters", "apps/aqua-cli/src", "apps/aqua-desktop/src-tauri/src", "learning"] {
            rust_files(&root.join(top), &mut files);
        }
        let mut seen: BTreeMap<String, (String, usize)> = BTreeMap::new();
        for f in &files {
            let rel = f.strip_prefix(root).unwrap_or(f).to_string_lossy().replace('\\', "/");
            let mut here = names_in(&std::fs::read_to_string(f).unwrap_or_default());
            here.sort();
            here.dedup();
            for n in here {
                let e = seen.entry(n).or_insert((rel.clone(), 0));
                e.1 += 1;
            }
        }
        let mut s = String::from("## 13. Environment variables (generated)\n\n### The typed flags\n\n");
        s.push_str("Generated from the flag registry (`core/src/flags.rs`), where these switches are read in one place (`GET /api/v1/flags` lists them with what each is now). The value is read each time it is asked for, so a variable set before start, or one a signed promotion set at start, is what counts. A variable you set yourself wins over a promotion.\n\n");
        s.push_str(&crate::flags::markdown_table());
        s.push_str("\n### Every name the source mentions\n\n");
        s.push_str("Generated from the source: every `\"AQUA_*\"` name the first-party Rust code mentions as a string (test files are not searched). It lists where to read what a variable does; `cargo test -p aqua-core --lib docs_gen` fails when this table is out of date, and `AQUA_UPDATE_DOCS=1` rewrites it.\n\n");
        s.push_str(&format!("{} variables.\n\n| Variable | First file | Files |\n|---|---|---|\n", seen.len()));
        for (name, (file, n)) in &seen {
            s.push_str(&format!("| `{name}` | `{file}` | {n} |\n"));
        }
        s
    }

    #[test]
    fn the_environment_variable_table_is_current() {
        let path = docs_dir().join("CONFIGURATION.md");
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let head = text.split(MARKER).next().unwrap_or("").trim_end().to_string();
        let expected = format!("{head}\n\n{MARKER}\n\n{}", render(&docs_dir().join("..")));
        super::check_or_write(&path, &expected);
    }

    #[test]
    fn names_are_found_whole_and_prefixes_are_not_taken_for_names() {
        let src = r#"std::env::var("AQUA_API_PORT"); let k = format!("AQUA_{}", x); const A: &str = "AQUA_"; env!("AQUA_LLAMA_CTX"); "not AQUA_THIS"; "AQUA_X_""#;
        assert_eq!(names_in(src), vec!["AQUA_API_PORT".to_string(), "AQUA_LLAMA_CTX".to_string()]);
    }
}

/// The capability register (`documentation/capabilities.toml`): what Aqua can really do, one entry per capability, with the code that does
/// it and the tests that show it. A claim of "implemented" without a file, a symbol and a test is a test failure; `CAPABILITIES.md` is
/// generated from it.
#[cfg(test)]
mod capabilities {
    use super::{check_or_write, docs_dir, CAPABILITIES_FRONT};
    use std::collections::BTreeSet;
    use std::path::Path;

    #[derive(Debug, Clone, Default, PartialEq)]
    pub struct Capability {
        pub id: String,
        pub area: String,
        pub title: String,
        /// `implemented` (works, and a test shows it), `partial` (works for some cases), `simulated` (the code exists but pretends:
        /// fixed answers or stubs), `planned` (does not exist yet).
        pub status: String,
        /// `path/from/the/repository::text that must appear in that file` (a function, a struct, a constant).
        pub evidence: String,
        /// `path::test_name`: the file must contain `fn test_name`.
        pub tests: Vec<String>,
        pub doc: String,
        pub note: String,
        /// Where the code is reached from when it is not by a call in the Rust code (a route, a start-up call, a tool name): `route POST /api/v1/vault/index`. An
        /// entry that has one is not asked for a caller.
        pub entry_point: String,
    }

    const STATUSES: &[&str] = &["implemented", "partial", "simulated", "planned"];
    const KEYS: &[&str] = &["id", "area", "title", "status", "evidence", "tests", "doc", "note", "entry_point"];

    /// A string in double quotes, with `\"` and `\\` and `\n` escapes. Returns it and what is left of the text.
    fn quoted(s: &str) -> Result<(String, &str), String> {
        let mut out = String::new();
        let mut chars = s.char_indices();
        match chars.next() {
            Some((_, '"')) => {}
            _ => return Err(format!("expected a string in double quotes at: {s}")),
        }
        let mut escaped = false;
        for (i, c) in chars {
            if escaped {
                out.push(match c {
                    'n' => '\n',
                    other => other,
                });
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                return Ok((out, &s[i + 1..]));
            } else {
                out.push(c);
            }
        }
        Err("a string is not closed".into())
    }

    /// The small part of TOML the register uses: `[[capability]]` tables whose values are strings or one-line lists of strings, and `#`
    /// comments. Anything else is an error with its line number, so a typo cannot quietly drop an entry. A key set twice in one entry is an error too, as TOML says: an entry written
    /// without its `[[capability]]` header (the commit of OA4-04 did it, after the last entry of OA11-02) would otherwise be read as part of the entry above it, whose values it overwrote, and the
    /// entry above it would be missing from the register and from `CAPABILITIES.md` without a word.
    pub fn parse(text: &str) -> Result<Vec<Capability>, String> {
        let mut out: Vec<Capability> = Vec::new();
        let mut keys_of_this_entry: BTreeSet<&str> = BTreeSet::new();
        for (n, raw) in text.lines().enumerate() {
            let at = n + 1;
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line == "[[capability]]" {
                out.push(Capability::default());
                keys_of_this_entry.clear();
                continue;
            }
            let Some(current) = out.last_mut() else { return Err(format!("line {at}: a value before the first [[capability]]")) };
            let Some((key, value)) = line.split_once('=') else { return Err(format!("line {at}: expected key = value")) };
            let (key, value) = (key.trim(), value.trim());
            if !KEYS.contains(&key) {
                return Err(format!("line {at}: unknown key '{key}' (known: {})", KEYS.join(", ")));
            }
            if !keys_of_this_entry.insert(key) {
                return Err(format!("line {at}: '{key}' is set twice in one entry: a [[capability]] header is missing before this line (the entry above it would be overwritten)"));
            }
            if key == "tests" {
                let inner = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')).ok_or_else(|| format!("line {at}: tests must be a list in square brackets"))?;
                let mut rest = inner.trim();
                let mut list = Vec::new();
                while !rest.is_empty() {
                    let (item, after) = quoted(rest).map_err(|e| format!("line {at}: {e}"))?;
                    list.push(item);
                    rest = after.trim_start().strip_prefix(',').unwrap_or(after).trim();
                }
                current.tests = list;
                continue;
            }
            let (s, after) = quoted(value).map_err(|e| format!("line {at}: {e}"))?;
            if !after.trim().is_empty() && !after.trim().starts_with('#') {
                return Err(format!("line {at}: something follows the string: {after}"));
            }
            match key {
                "id" => current.id = s,
                "area" => current.area = s,
                "title" => current.title = s,
                "status" => current.status = s,
                "evidence" => current.evidence = s,
                "doc" => current.doc = s,
                "entry_point" => current.entry_point = s,
                _ => current.note = s,
            }
        }
        Ok(out)
    }

    fn repo_root() -> std::path::PathBuf {
        docs_dir().join("..")
    }

    /// `path::symbol` must name an existing file that contains the symbol text. `fn_prefix` makes it `fn symbol` (for a test name).
    fn reference_problem(root: &Path, what: &str, reference: &str, fn_prefix: bool) -> Option<String> {
        let Some((path, symbol)) = reference.split_once("::") else { return Some(format!("{what}: '{reference}' is not path::symbol")) };
        let file = root.join(path);
        let Ok(body) = std::fs::read_to_string(&file) else { return Some(format!("{what}: {path} does not exist")) };
        let needle = if fn_prefix { format!("fn {symbol}") } else { symbol.to_string() };
        (!body.contains(&needle)).then(|| format!("{what}: '{needle}' is not in {path}"))
    }

    /// Everything wrong with the register, one line each. Empty means it holds.
    pub fn problems(register: &[Capability], root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for c in register {
            let id = if c.id.is_empty() { "(no id)".to_string() } else { c.id.clone() };
            if c.id.is_empty() || !c.id.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '.') {
                out.push(format!("{id}: the id must be lower case letters, digits, _ and ."));
            }
            if !seen.insert(c.id.clone()) {
                out.push(format!("{id}: the id is used twice"));
            }
            if c.area.is_empty() || c.title.is_empty() {
                out.push(format!("{id}: area and title are required"));
            }
            if !STATUSES.contains(&c.status.as_str()) {
                out.push(format!("{id}: status '{}' is not one of {}", c.status, STATUSES.join(", ")));
                continue;
            }
            match c.status.as_str() {
                "implemented" => {
                    if c.evidence.is_empty() {
                        out.push(format!("{id}: implemented needs evidence (the code that does it)"));
                    }
                    if c.tests.is_empty() {
                        out.push(format!("{id}: implemented needs at least one test that shows it"));
                    }
                }
                "partial" | "simulated" if c.evidence.is_empty() => out.push(format!("{id}: {} needs evidence (the code that exists)", c.status)),
                "planned" if !c.tests.is_empty() => out.push(format!("{id}: a planned capability cannot have tests yet")),
                _ => {}
            }
            if !c.evidence.is_empty() {
                out.extend(reference_problem(root, &format!("{id} evidence"), &c.evidence, false));
            }
            for t in &c.tests {
                out.extend(reference_problem(root, &format!("{id} test"), t, true));
            }
            if !c.doc.is_empty() && !root.join(&c.doc).is_file() && !root.join("documentation").join(&c.doc).is_file() {
                out.push(format!("{id}: the document '{}' does not exist", c.doc));
            }
        }
        out
    }

    /// The name a piece of evidence defines, when it is a definition (`pub fn plan`, `pub struct Skills`, `pub(crate) const MAX`): `plan`, `Skills`, `MAX`.
    pub fn defined_name(symbol: &str) -> Option<&str> {
        let words: Vec<&str> = symbol.split_whitespace().collect();
        let i = words.iter().position(|w| matches!(*w, "fn" | "struct" | "enum" | "trait" | "const" | "static" | "type"))?;
        let name = words.get(i + 1)?.split(|c: char| !(c.is_alphanumeric() || c == '_')).next()?;
        (!name.is_empty()).then_some(name)
    }

    /// The code of a file that is not a test: up to the first `#[cfg(test)]` module.
    fn live_code(text: &str) -> &str {
        let end = text.find("#[cfg(test)]\nmod ").or_else(|| text.find("#[cfg(test)]\r\nmod ")).unwrap_or(text.len());
        &text[..end]
    }

    /// Does this code use `name` somewhere other than where it is defined, declared, imported or implemented? (A whole word on a line that is not a comment, a `use`,
    /// an `impl` or the definition itself.)
    pub fn uses_name(code: &str, name: &str) -> bool {
        const DEFINES: &[&str] = &["fn", "struct", "enum", "trait", "const", "static", "type", "mod"];
        code.lines().any(|line| {
            let t = line.trim_start();
            if t.starts_with("//") || t.starts_with("use ") || t.starts_with("pub use ") || t.starts_with("pub(crate) use ") || t.starts_with("impl") {
                return false;
            }
            line.match_indices(name).any(|(i, _)| {
                let before = line[..i].chars().next_back();
                let after = line[i + name.len()..].chars().next();
                let whole = !before.is_some_and(|c| c.is_alphanumeric() || c == '_') && !after.is_some_and(|c| c.is_alphanumeric() || c == '_');
                let defines = DEFINES.iter().any(|k| line[..i].trim_end().ends_with(k));
                whole && !defines
            })
        })
    }

    /// The entries (implemented or partial, with a definition as evidence, and no `entry_point`) whose defined name is used by no first-party code outside tests.
    /// A register that says "implemented" for code nothing calls claims a feature that does not run.
    pub fn uncalled(register: &[Capability], root: &Path) -> Vec<String> {
        let mut files = Vec::new();
        for top in ["core/src", "adapters", "apps/aqua-cli/src", "apps/aqua-desktop/src-tauri/src"] {
            super::env_vars::rust_files(&root.join(top), &mut files);
        }
        let code: Vec<String> = files.iter().map(|f| std::fs::read_to_string(f).unwrap_or_default()).collect();
        let mut out = Vec::new();
        for c in register.iter().filter(|c| matches!(c.status.as_str(), "implemented" | "partial") && c.entry_point.is_empty()) {
            let Some((path, symbol)) = c.evidence.split_once("::") else { continue };
            let Some(name) = defined_name(symbol) else { continue };
            if !code.iter().any(|text| uses_name(live_code(text), name)) {
                out.push(format!("{}: nothing outside tests uses `{name}` (evidence {path}); say where it is reached from with entry_point, or it is not wired", c.id));
            }
        }
        out
    }

    /// `CAPABILITIES.md`: the register as tables, by area, with a count by status.
    pub fn render(register: &[Capability]) -> String {
        let mut s = format!("{CAPABILITIES_FRONT}# What Aqua can do\n\n");
        s.push_str("Generated from [`capabilities.toml`](capabilities.toml); do not edit this file. `cargo test -p aqua-core --lib capabilities` checks that every file, symbol and test named there exists, and `AQUA_UPDATE_DOCS=1` rewrites this page.\n\n");
        s.push_str("**implemented**: works, and a test shows it. **partial**: works for some cases. **simulated**: the code exists but returns fixed answers or does nothing real. **planned**: does not exist yet.\n\n");
        let count = |st: &str| register.iter().filter(|c| c.status == st).count();
        s.push_str(&format!("{} capabilities: {} implemented, {} partial, {} simulated, {} planned.\n", register.len(), count("implemented"), count("partial"), count("simulated"), count("planned")));
        let areas: Vec<String> = {
            let mut a: Vec<String> = Vec::new();
            for c in register {
                if !a.contains(&c.area) {
                    a.push(c.area.clone());
                }
            }
            a
        };
        for area in areas {
            s.push_str(&format!("\n## {area}\n\n| Capability | Status | Where | Shown by |\n|---|---|---|---|\n"));
            for c in register.iter().filter(|c| c.area == area) {
                let note = if c.note.is_empty() { String::new() } else { format!(" {}", c.note) };
                let tests = c.tests.iter().map(|t| format!("`{}`", t.split_once("::").map_or(t.as_str(), |(_, n)| n))).collect::<Vec<_>>().join(", ");
                let where_ = if c.evidence.is_empty() { String::new() } else { format!("`{}`", c.evidence.replace('|', "\\|")) };
                s.push_str(&format!("| {}{} | {} | {} | {} |\n", c.title, note, c.status, where_, tests));
            }
        }
        s
    }

    fn load() -> Vec<Capability> {
        let path = docs_dir().join("capabilities.toml");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    #[test]
    fn what_the_names_in_the_register_mean_is_read_and_a_call_is_found_only_outside_comments_imports_and_definitions() {
        assert_eq!(defined_name("pub fn plan"), Some("plan"));
        assert_eq!(defined_name("pub(crate) struct Skills {"), Some("Skills"));
        assert_eq!(defined_name("pub const MAX_QUEUED"), Some("MAX_QUEUED"));
        assert_eq!(defined_name("this run has read untrusted content"), None);
        let code = "use crate::x::plan;\n// plan(1)\npub fn plan(a: u8) {}\nimpl Plan for X {}\nlet planner = 1;\n";
        assert!(!uses_name(code, "plan"), "an import, a comment, the definition and a longer word are not uses");
        assert!(uses_name("fn main() { let r = plan(1); }", "plan"));
        assert!(uses_name("self.engine.plan()", "plan"));
        assert!(uses_name("Skills::new()", "Skills"));
        // The register of the claims of a made-up crate: one entry with a caller, one without, one that says how it is reached.
        let root = std::env::temp_dir().join(format!("aqua_uncalled_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("core/src")).unwrap();
        std::fs::write(root.join("core/src/a.rs"), "pub fn used_one() {}\npub fn never_called() {}\npub fn by_route() {}\nfn main() { used_one(); }\n#[cfg(test)]\nmod tests {\n    fn t() { never_called(); by_route(); }\n}\n").unwrap();
        let entry = |id: &str, evidence: &str, entry_point: &str| super::capabilities::Capability { id: id.into(), status: "implemented".into(), evidence: format!("core/src/a.rs::{evidence}"), entry_point: entry_point.into(), ..Default::default() };
        let register = vec![entry("x.used", "pub fn used_one", ""), entry("x.dead", "pub fn never_called", ""), entry("x.route", "pub fn by_route", "route GET /x")];
        let found = uncalled(&register, &root);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("x.dead: nothing outside tests uses `never_called`"), "{found:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A claim of "implemented" or "partial" whose evidence nothing outside tests uses is a feature that does not run (the vault, reminder and memory tools, and
    /// `promoted::apply`, were each registered, tested and never reached). Name where it is reached from with `entry_point`, or wire it.
    #[test]
    fn every_claimed_symbol_has_a_caller_outside_tests() {
        let found = uncalled(&load(), &repo_root());
        assert!(found.is_empty(), "claimed in the register and used by nothing:\n{}", found.join("\n"));
    }

    #[test]
    fn capability_evidence_exists() {
        let register = load();
        assert!(register.len() >= 30, "the register is not meant to be a stub: {} entries", register.len());
        let found = problems(&register, &repo_root());
        assert!(found.is_empty(), "the capability register does not hold:\n  {}", found.join("\n  "));
    }

    #[test]
    fn capabilities_doc_is_current() {
        check_or_write(&docs_dir().join("CAPABILITIES.md"), &render(&load()));
    }

    #[test]
    fn a_register_that_claims_more_than_the_code_shows_is_refused() {
        let root = repo_root();
        let text = r#"
# a negative fixture
[[capability]]
id = "x.fine"
area = "Test"
title = "Has evidence and a test"
status = "implemented"
evidence = "core/src/docs_gen.rs::fn render_reference"
tests = ["core/src/docs_gen.rs::tool_reference_is_current"]

[[capability]]
id = "x.no_test"
area = "Test"
title = "Claims to work and shows nothing"
status = "implemented"
evidence = "core/src/docs_gen.rs::fn render_reference"

[[capability]]
id = "x.no_file"
area = "Test"
title = "Evidence in a file that is not there"
status = "partial"
evidence = "core/src/does_not_exist.rs::fn nothing"

[[capability]]
id = "x.no_symbol"
area = "Test"
title = "Evidence the file does not contain (a different file from this one, whose own fixture text would count)"
status = "partial"
evidence = "core/src/doctor.rs::fn this_function_is_not_written_anywhere"

[[capability]]
id = "x.bad_test"
area = "Test"
title = "A test that is not there"
status = "implemented"
evidence = "core/src/docs_gen.rs::fn render_reference"
tests = ["core/src/doctor.rs::a_test_nobody_wrote"]

[[capability]]
id = "x.fine"
area = "Test"
title = "A repeated id"
status = "planned"

[[capability]]
id = "x.status"
area = "Test"
title = "A status that is not one of the four"
status = "done"

[[capability]]
id = "x.planned"
area = "Test"
title = "Not built yet, which is fine"
status = "planned"
"#;
        let register = parse(text).unwrap();
        assert_eq!(register.len(), 8);
        let found = problems(&register, &root).join("\n");
        for expected in [
            "x.no_test: implemented needs at least one test",
            "x.no_file evidence: core/src/does_not_exist.rs does not exist",
            "x.no_symbol evidence: 'fn this_function_is_not_written_anywhere' is not in core/src/doctor.rs",
            "x.bad_test test: 'fn a_test_nobody_wrote' is not in core/src/doctor.rs",
            "x.fine: the id is used twice",
            "x.status: status 'done' is not one of",
        ] {
            assert!(found.contains(expected), "missing '{expected}' in:\n{found}");
        }
        assert!(!found.contains("x.planned"), "a planned capability needs nothing else:\n{found}");
        assert_eq!(found.lines().filter(|l| l.starts_with("x.fine:") && !l.contains("used twice")).count(), 0, "the first x.fine is fine");
    }

    #[test]
    fn the_register_format_is_strict_so_a_typo_cannot_drop_an_entry() {
        assert!(parse("id = \"a\"").unwrap_err().contains("before the first [[capability]]"));
        assert!(parse("[[capability]]\nstatus = implemented").unwrap_err().contains("double quotes"));
        assert!(parse("[[capability]]\nstatuss = \"x\"").unwrap_err().contains("unknown key 'statuss'"));
        assert!(parse("[[capability]]\ntests = \"a::b\"").unwrap_err().contains("list in square brackets"));
        assert!(parse("[[capability]]\ntitle = \"open").unwrap_err().contains("not closed"));
        assert!(parse("[[capability]]\ntitle = \"a\" b").unwrap_err().contains("follows the string"));
        let ok = parse("[[capability]]\nid = \"a.b\"  # a comment\ntitle = \"It says \\\"hi\\\"\"\ntests = [\"x.rs::one\", \"y.rs::two\"]\n").unwrap();
        assert_eq!((ok[0].id.as_str(), ok[0].title.as_str(), ok[0].tests.len()), ("a.b", "It says \"hi\"", 2));
    }

    /// Gate w4 repair: an entry pasted after another one with no `[[capability]]` header between them was read as one entry (the second one's values over the first one's), so the
    /// first one was missing from the register and from `CAPABILITIES.md`, and the file was not TOML (`tomllib` said "Cannot overwrite a value"). It is an error now, with the line.
    #[test]
    fn an_entry_without_its_header_is_an_error_and_never_a_silent_merge() {
        let entry = |id: &str| format!("id = \"{id}\"\narea = \"Agent\"\ntitle = \"t\"\nstatus = \"planned\"\nnote = \"n\"\n");
        let broken = format!("[[capability]]\n{}\n{}", entry("a.one"), entry("a.two"));
        let e = parse(&broken).unwrap_err();
        assert!(e.starts_with("line 8: 'id' is set twice in one entry") && e.contains("header is missing"), "{e}");
        // No blank line either, as in the commit that did it: the note line of the one entry, and straight after it the id of the next.
        let glued = format!("[[capability]]\n{}{}", entry("a.one"), entry("a.two"));
        assert!(parse(&glued).unwrap_err().contains("'id' is set twice"), "{:?}", parse(&glued));
        // The same two entries with their headers are two entries, and a key may be used again in the next one.
        let fine = format!("[[capability]]\n{}\n[[capability]]\n{}", entry("a.one"), entry("a.two"));
        let two = parse(&fine).unwrap();
        assert_eq!(two.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["a.one", "a.two"]);
        // Any key counts, not only the id.
        assert!(parse("[[capability]]\ntitle = \"a\"\ntitle = \"b\"\n").unwrap_err().contains("'title' is set twice"));
    }

    /// The register as a file, line by line: every `id =` line is the first line of an entry (the line before it is the header), and the number of entries read is the number of headers,
    /// which is the number of ids. This is what a TOML reader would hold the file to.
    #[test]
    fn every_entry_of_the_register_file_has_its_header_and_none_is_lost_in_the_reading() {
        let text = std::fs::read_to_string(docs_dir().join("capabilities.toml")).unwrap();
        let lines: Vec<&str> = text.lines().map(str::trim).collect();
        let headers = lines.iter().filter(|l| **l == "[[capability]]").count();
        let ids: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| l.starts_with("id =") || l.starts_with("id=")).map(|(i, _)| i).collect();
        for &i in &ids {
            assert!(i > 0 && lines[i - 1] == "[[capability]]", "line {}: an entry's `id =` line is not directly after a [[capability]] header ({:?} is before it)", i + 1, i.checked_sub(1).map(|p| lines[p]));
        }
        assert_eq!(headers, ids.len(), "{headers} headers and {} id lines", ids.len());
        assert_eq!(load().len(), headers, "the register reads {} entries from {headers} headers: an entry was lost or merged", load().len());
    }
}

#[test]
fn every_error_code_is_explained_in_the_troubleshooting_page() {
    let text = std::fs::read_to_string(docs_dir().join("TOOLS_TROUBLESHOOTING.md")).unwrap();
    for c in ErrorCode::ALL {
        let start = format!("| `{}` |", c.as_str());
        let row = text.lines().find(|l| l.starts_with(&start)).unwrap_or_else(|| panic!("{} is missing from TOOLS_TROUBLESHOOTING.md", c.as_str()));
        let cells: Vec<&str> = row.split('|').map(str::trim).collect();
        let yes = |s: &str| s == "yes";
        assert_eq!(yes(cells[2]), c.stops_run(), "{}: 'stops the run' column is wrong", c.as_str());
        assert_eq!(yes(cells[3]), c.retryable(), "{}: 'retryable' column is wrong", c.as_str());
    }
}

#[test]
fn every_tool_the_host_registers_can_be_shown_to_the_agent() {
    // The browser and desktop tools reach the agent through its own tools (`browser_*`, Windows-MCP) until the one door (card R6-08), a user's MCP server by its name,
    // and the skill and tool-search tools by their own rules; every other tool is offered by a pack of `agent_tools::PACKS`. The vault, reminder and memory tools were
    // registered and tested through the host and offered to no goal, because no pack named them.
    // (The command tools, `cmd.*` and `proj.*`, are shown for a goal that names them and for a coding goal, by `agent_tools::command_tools_shown`; they exist only with `AQUA_COMMAND_TOOLS=on`.)
    const OTHER_WAYS: &[&str] = &["browser.", "win.", "mcp_", "skill.", "tools.", "cmd.", "proj."];
    let (host, _) = built("every_tool_has_a_pack");
    let mut unreachable: Vec<String> = host.manifests().into_iter().map(|m| m.name).filter(|n| crate::agent_tools::pack_of(n).is_none() && !OTHER_WAYS.iter().any(|p| n.starts_with(p))).collect();
    unreachable.sort();
    assert!(unreachable.is_empty(), "registered but offered to no goal (name a pack for them in agent_tools::PACKS): {unreachable:?}");
}

#[test]
fn tool_reference_is_current() {
    let (host, _) = built("ref");
    check_or_write(&docs_dir().join("TOOLS_REFERENCE.md"), &render_reference(&host));
}

#[test]
fn every_group_of_the_tool_reference_has_a_title_of_its_own_and_none_is_called_other() {
    // Six groups used to share the heading "Other" (documents, flows, memory, reminders, screen reading and tool search), so a reader could not tell them apart.
    let (host, _) = built("ref_titles");
    let text = render_reference(&host);
    let headings: Vec<&str> = text.lines().filter(|l| l.starts_with("## ")).collect();
    assert!(!headings.contains(&"## Other"), "a group has no title of its own (add it to domain_title): {headings:?}");
    let mut once = headings.clone();
    once.sort();
    once.dedup();
    assert_eq!(once.len(), headings.len(), "two groups share a heading: {headings:?}");
}

#[test]
fn config_defaults_in_the_docs_are_current() {
    let (_host, data) = built("cfg");
    let path = docs_dir().join("TOOLS_CONFIG.md");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let head = text.split(CONFIG_MARKER).next().unwrap_or("").trim_end().to_string();
    let head = if head.is_empty() { "# Tool configuration".to_string() } else { head };
    let expected = format!("{head}\n\n{CONFIG_MARKER}\n\n{}", render_default_configs(&data));
    check_or_write(&path, &expected);
}
