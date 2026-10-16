//! The words of a project, to help the speech model hear them (card OA10-06; task UX-16 of the OpenCode audit).
//!
//! A speech model that has never seen `parse_strict`, `AppState` or `tokio` writes "pars strict", "app state" and "toe key oh". It can be given a short prompt of the words it should expect, which is
//! what the dictation settings already do with the person's own words (`dictation::Settings::terms`). This module finds a project's words so the person does not have to write them:
//!
//! * [`collect`] walks the project folder (at most [`MAX_FILES`] files; never a link, never a hidden folder, never `target`, `node_modules`, `vendor`, `data` and the like) and takes the
//!   names of its files, the package and dependency names of `Cargo.toml` and `package.json`, and the names the newest source files declare (`fn`, `struct`, `class`, `def`, `function` ...).
//! * Only words that are hard to hear are kept: a name made of several words (`parse_strict`, `AppState`), one with a digit, or a long one; not `new`, `main` or `test`.
//! * **Nothing secret goes in.** A file whose name looks like a key, a certificate or a credential is never opened and never named (the same rule as the file index); a line that talks of a
//!   key, a token, a password or a secret is not read for names (the rule of `project_brief`); and a word that the secret masker would change, or that looks like a key, is dropped.
//! * They are ranked by how often they are declared and how recently their file changed, and cut to the dictation limits: at most [`MAX_TERMS`] words and [`MAX_PROMPT_CHARS`] characters.
//! * [`merge`] puts the person's own words first (they win), then the project's, without a word twice.
//!
//! Where it is used: the prompt of a dictation clip when `AQUA_PROJECT_TERMS=on` (`api_gateway::dictation_api`), and `GET /api/v1/coding/terms` (the UI token), which shows the person what
//! would be given. The voice path (the wake phrase) is not primed with them: that prompt is `Hey Aqua.`, and replacing it can cost the wake word; whether adding to it helps is to be measured
//! with the speech model, which this change has not run.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Files looked at at most.
pub const MAX_FILES: usize = 2_000;
/// Words in a prompt (the dictation limit).
pub const MAX_TERMS: usize = 200;
/// Characters in a prompt (the dictation limit).
pub const MAX_PROMPT_CHARS: usize = 800;
/// The longest word kept.
const MAX_TERM_CHARS: usize = 40;
const MIN_TERM_CHARS: usize = 4;
/// Source files opened at most (the newest first), and the largest of them.
const MAX_SOURCE_FILES: usize = 300;
/// Manifests read at most (a monorepo has one in every package).
const MAX_MANIFESTS: usize = 40;
const MAX_SOURCE_BYTES: u64 = 256 * 1024;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_DEPTH: usize = 8;
const MAX_ENTRIES_PER_DIR: usize = 500;
/// Dependencies named from one manifest.
const MAX_DEPENDENCIES: usize = 30;
/// How long what was found for a folder is kept, for the next clip.
const CACHE_FOR: Duration = Duration::from_secs(300);

/// Folders never entered. Hidden folders (a name that starts with a dot) are never entered either.
const SKIP_DIRS: &[&str] = &[
    "target", "node_modules", "vendor", "dist", "build", "out", "data", "logs", "log", "tmp", "temp", "coverage", "venv", "env", "__pycache__", "site-packages", "bower_components", "packages_cache",
    "obj", "bin", "pkg", "third_party", "third-party", "external", "generated", "fixtures",
];

/// The extensions of the files that are read for the names they declare.
const SOURCE_EXTENSIONS: &[&str] = &["rs", "py", "ts", "tsx", "js", "jsx", "mjs", "go", "java", "kt", "cs", "rb", "php", "c", "cc", "cpp", "h", "hpp", "swift", "scala"];

/// Words that are said as they are written and that a speech model does not miss, whatever the project.
const STOP: &[&str] = &[
    "main", "mod", "lib", "index", "test", "tests", "mock", "mocks", "util", "utils", "helper", "helpers", "common", "config", "setup", "readme", "license", "changelog", "default", "handler",
    "error", "errors", "result", "state", "types", "model", "models", "service", "services", "client", "server", "module", "package", "script", "scripts", "source", "static", "public",
    "private", "return", "struct", "string", "number", "object", "import", "export", "function", "class", "interface", "constructor", "component", "components", "context", "request", "response",
    "options", "params", "props", "value", "values", "version", "display", "debug", "clone", "format", "parse", "build", "builder", "write", "writer", "reader", "create", "update", "delete",
];

/// What a walk of a project found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Collected {
    /// The words, the first the most likely to matter. At most [`MAX_TERMS`], and their prompt at most [`MAX_PROMPT_CHARS`].
    pub terms: Vec<String>,
    pub files_seen: usize,
    /// Files that were not opened or named because their name looks like a secret.
    pub skipped_secret: usize,
    /// The walk stopped at [`MAX_FILES`].
    pub truncated: bool,
}

/// The likely meaning of a line that mentions something secret: a line of this kind is never read for names (the rule of `project_brief::secret_line`).
fn secret_line(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("api_key") || l.contains("api-key") || l.contains("apikey") || l.contains("token") || l.contains("password") || l.contains("passwd") || l.contains("secret") || l.contains("credential") || l.contains("private_key")
}

/// Is this word something that must not go into a prompt: it talks of a secret, the masker would change it, or it looks like a key (a long run of hex or of mixed letters and digits)?
fn looks_secret(word: &str) -> bool {
    if secret_line(word) || aqua_tool_host::secret_patterns::mask(word) != word {
        return true;
    }
    let (digits, letters) = (word.chars().filter(char::is_ascii_digit).count(), word.chars().filter(|c| c.is_alphabetic()).count());
    let hex = word.len() >= 12 && word.chars().all(|c| c.is_ascii_hexdigit());
    hex || (word.len() >= 16 && digits * 3 >= word.len() && letters > 0)
}

/// Is this word one a speech model may miss: made of several words (`parse_strict`, `AppState`), with a digit, or long?
fn is_distinctive(word: &str) -> bool {
    let humps = word.chars().skip(1).any(|c| c.is_uppercase());
    word.contains('_') || word.contains('-') || humps || word.chars().any(|c| c.is_ascii_digit()) || word.chars().count() >= 7
}

/// A word as a prompt may hold it: letters, digits, `_` and `-`, starting with a letter, [`MIN_TERM_CHARS`] to [`MAX_TERM_CHARS`] characters, and not a [`STOP`] word.
fn usable(word: &str) -> bool {
    let n = word.chars().count();
    (MIN_TERM_CHARS..=MAX_TERM_CHARS).contains(&n)
        && word.chars().next().is_some_and(char::is_alphabetic)
        && word.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        && !STOP.contains(&word.to_ascii_lowercase().as_str())
        && !looks_secret(word)
}

/// How a word scores: how many times it was found (at most five) times how new its file is (3 for the newest fifth, 2 for the next, 1 for the rest), plus a bonus for a name a manifest gave.
#[derive(Default)]
struct Score {
    spelling: String,
    found: u32,
    weight: u32,
    manifest: bool,
}

#[derive(Default)]
struct Tally {
    by_word: BTreeMap<String, Score>,
}

impl Tally {
    fn add(&mut self, word: &str, weight: u32, manifest: bool) {
        if !usable(word) {
            return;
        }
        let entry = self.by_word.entry(word.to_ascii_lowercase()).or_default();
        if entry.spelling.is_empty() {
            entry.spelling = word.to_string();
        }
        entry.found += 1;
        entry.weight = entry.weight.max(weight);
        entry.manifest |= manifest;
    }

    fn ranked(self) -> Vec<String> {
        let mut scored: Vec<(u32, String)> = self
            .by_word
            .into_values()
            .filter(|s| s.manifest || is_distinctive(&s.spelling))
            .map(|s| (s.found.min(5) * s.weight.max(1) + if s.manifest { 4 } else { 0 }, s.spelling))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.to_ascii_lowercase().cmp(&b.1.to_ascii_lowercase())));
        scored.into_iter().map(|(_, w)| w).collect()
    }
}

/// The words a prompt holds: the first of `terms` that fit in [`MAX_TERMS`] words and [`MAX_PROMPT_CHARS`] characters, comma separated.
pub fn prompt_of(terms: &[String]) -> String {
    let mut out = String::new();
    for (i, t) in terms.iter().map(|t| t.trim()).filter(|t| !t.is_empty()).enumerate() {
        if i >= MAX_TERMS || out.chars().count() + t.chars().count() + 2 > MAX_PROMPT_CHARS {
            break;
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str(t);
    }
    out
}

/// The person's own words first, then the project's, each word once (the case of the first is kept), cut to the limits of a prompt ([`MAX_TERMS`] words, [`MAX_PROMPT_CHARS`] characters).
pub fn merge(personal: &[String], project: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut chars = 0usize;
    for term in personal.iter().chain(project) {
        let t = term.trim();
        if t.is_empty() || !seen.insert(t.to_lowercase()) {
            continue;
        }
        if out.len() >= MAX_TERMS || chars + t.chars().count() + 2 > MAX_PROMPT_CHARS {
            break;
        }
        chars += t.chars().count() + 2;
        out.push(t.to_string());
    }
    out
}

/// Names from a `Cargo.toml`: the package and the dependencies. Read as text, line by line: no value is used except a name.
fn cargo_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if secret_line(line) {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            section = rest.trim_end_matches(']').trim().to_string();
            // `[dependencies.serde]`
            if let Some(dep) = section.rsplit_once('.').filter(|(head, _)| head.ends_with("dependencies")).map(|(_, dep)| dep) {
                names.push(dep.trim_matches('"').to_string());
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim().trim_matches('"');
        if section == "package" && key == "name" {
            names.push(value.trim().trim_matches('"').to_string());
        } else if section.ends_with("dependencies") && !key.contains(' ') && !key.contains('.') && names.len() < MAX_DEPENDENCIES + 1 {
            names.push(key.to_string());
        }
    }
    names
}

/// Names from a `package.json`: the package and the dependencies (not the scripts, which are commands).
fn package_json_names(text: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else { return Vec::new() };
    let mut names: Vec<String> = v["name"].as_str().map(|n| n.trim_start_matches('@').replace('/', "-")).into_iter().collect();
    for section in ["dependencies", "devDependencies"] {
        if let Some(map) = v[section].as_object() {
            names.extend(map.keys().take(MAX_DEPENDENCIES).map(|k| k.trim_start_matches('@').replace('/', "-")));
        }
    }
    names
}

/// The name a line declares, when it starts with a declaration (`fn`, `struct`, `class`, `def`, `function` ...), after the words that may stand before one (`pub`, `export`, `async` ...). A line that is
/// indented more than a method is not read, and one that talks of a secret is not read at all.
fn declared_name(line: &str) -> Option<String> {
    if secret_line(line) {
        return None;
    }
    let indent = line.len() - line.trim_start().len();
    if indent > 4 {
        return None;
    }
    const MODIFIERS: &[&str] = &[
        "pub", "export", "default", "async", "unsafe", "static", "final", "public", "private", "protected", "abstract", "extern", "declare", "inline", "internal", "sealed", "open", "override", "readonly", "data",
    ];
    const DECLARATIONS: &[&str] = &["fn", "struct", "enum", "trait", "impl", "mod", "type", "class", "interface", "def", "function", "func", "const", "record", "object", "namespace"];
    let mut words = line.split_whitespace();
    let mut word = words.next()?;
    loop {
        // `pub(crate)` is a modifier too.
        let base = word.split('(').next().unwrap_or(word);
        if MODIFIERS.contains(&base) {
            word = words.next()?;
        } else {
            break;
        }
    }
    // `impl<T>` is `impl` with its generics attached.
    if !DECLARATIONS.contains(&word.split(['<', '(']).next().unwrap_or(word)) {
        return None;
    }
    // After the keyword: skip generics (`impl <T>`), then take the name.
    let mut rest = words.next()?;
    if rest.starts_with('<') {
        rest = words.next()?;
    }
    let name: String = rest.trim_start_matches(['&', '*']).chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').collect();
    (!name.is_empty()).then_some(name)
}

/// Is a name this file's own: a file name that looks like a secret is never opened and never named.
fn secret_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let ext = Path::new(&lower).extension().and_then(|e| e.to_str()).unwrap_or("");
    aqua_mcp_adapters::file_index::rules::sensitive_file(&lower, ext)
}

/// Reads at most `max` bytes of a plain file as text; `None` for a link, a file that is too large, or one that is not text.
fn read_small(path: &Path, max: u64) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > max {
        return None;
    }
    String::from_utf8(std::fs::read(path).ok()?).ok()
}

/// Finds the words of the project in `root`. Nothing is written, nothing is run, and no link is followed.
pub fn collect(root: &Path) -> Collected {
    let mut out = Collected::default();
    // (path, modified) of the source files, and the manifests, found by a walk that goes level by level.
    let mut sources: Vec<(PathBuf, SystemTime)> = Vec::new();
    let mut manifests: Vec<(PathBuf, bool)> = Vec::new();
    let mut tally = Tally::default();
    let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
    queue.push_back((root.to_path_buf(), 0));
    'walk: while let Some((dir, depth)) = queue.pop_front() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = read.flatten().take(MAX_ENTRIES_PER_DIR).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
            if meta.file_type().is_symlink() {
                continue;
            }
            let lower = name.to_ascii_lowercase();
            if meta.is_dir() {
                let sensitive = aqua_mcp_adapters::file_index::rules::sensitive_dir(&lower);
                if name.starts_with('.') || SKIP_DIRS.contains(&lower.as_str()) || sensitive || depth >= MAX_DEPTH {
                    continue;
                }
                queue.push_back((entry.path(), depth + 1));
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            if out.files_seen >= MAX_FILES {
                out.truncated = true;
                break 'walk;
            }
            out.files_seen += 1;
            if secret_file(&name) {
                out.skipped_secret += 1;
                continue;
            }
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let ext = Path::new(&lower).extension().and_then(|e| e.to_str()).unwrap_or("").to_string();
            if lower == "cargo.toml" || lower == "package.json" {
                if manifests.len() < MAX_MANIFESTS {
                    manifests.push((entry.path(), lower == "cargo.toml"));
                }
            } else if SOURCE_EXTENSIONS.contains(&ext.as_str()) {
                sources.push((entry.path(), modified));
            }
            // The name of the file, without its extension, is a word of the project.
            if let Some(stem) = Path::new(&name).file_stem().and_then(|s| s.to_str()) {
                // Counted at the lowest weight: a file name says less than what a newer file declares.
                tally.add(stem, 1, false);
            }
        }
    }
    // The manifests first (their names are the project's), then the newest source files, each with the weight its age gives.
    for (path, is_cargo) in &manifests {
        let Some(text) = read_small(path, MAX_MANIFEST_BYTES) else { continue };
        let names = if *is_cargo { cargo_names(&text) } else { package_json_names(&text) };
        for name in names {
            tally.add(&name, 3, true);
        }
    }
    sources.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let fifth = (sources.len() / 5).max(1);
    for (rank, (path, _)) in sources.iter().take(MAX_SOURCE_FILES).enumerate() {
        let weight = if rank < fifth { 3 } else if rank < 2 * fifth { 2 } else { 1 };
        let Some(text) = read_small(path, MAX_SOURCE_BYTES) else { continue };
        for line in text.lines() {
            if let Some(name) = declared_name(line) {
                tally.add(&name, weight, false);
            }
        }
    }
    let ranked = tally.ranked();
    out.terms = merge(&[], &ranked);
    out
}

/// The terms of `root`, as they were found in the last five minutes (a walk of a project is not done for every clip of dictation).
pub fn terms_for(root: &Path) -> Vec<String> {
    static CACHE: std::sync::Mutex<Option<(PathBuf, Instant, Vec<String>)>> = std::sync::Mutex::new(None);
    let now = Instant::now();
    {
        let cache = CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached_root, at, terms)) = cache.as_ref() {
            if cached_root == root && now.duration_since(*at) < CACHE_FOR {
                return terms.clone();
            }
        }
    }
    let terms = collect(root).terms;
    *CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((root.to_path_buf(), now, terms.clone()));
    terms
}

/// The folder of the project the person works in: the folder of the newest session that has one, when it is still a folder.
pub fn current_project() -> Option<PathBuf> {
    let store = crate::agent_runs::store()?;
    let (sessions, _) = store.list_sessions(&crate::agent_runs::sessions::SessionQuery { q: "", directory: "", limit: 10, cursor: None, api_own_only: false }).ok()?;
    sessions.iter().filter_map(|s| s["directory"].as_str()).filter(|d| !d.trim().is_empty()).map(PathBuf::from).find(|p| p.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aqua_project_terms_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn put(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn the_names_of_the_files_the_manifests_and_the_declarations_are_the_words_of_a_project() {
        let root = scratch("words");
        put(&root, "Cargo.toml", "[package]\nname = \"quill-feather\"\n\n[dependencies]\ntokio = \"1\"\nserde_json = { version = \"1\" }\n\n[dependencies.rusqlite]\nversion = \"0.31\"\n");
        put(&root, "package.json", "{\"name\": \"@acme/ui-kit\", \"dependencies\": {\"react-query\": \"1\"}, \"scripts\": {\"deploy-everything\": \"x\"}}");
        put(&root, "src/speech_stream.rs", "pub struct SentenceSplitter {}\nimpl SentenceSplitter {\n    pub fn push(&mut self) {}\n}\npub(crate) async fn parse_strict() {}\nconst MAX_PIECES: usize = 3;\n");
        put(&root, "web/app.ts", "export class AppState {}\nexport function loadDocument() {}\n");
        let found = collect(&root);
        for word in ["quill-feather", "tokio", "serde_json", "rusqlite", "acme-ui-kit", "react-query", "speech_stream", "SentenceSplitter", "parse_strict", "MAX_PIECES", "AppState", "loadDocument"] {
            assert!(found.terms.iter().any(|t| t == word), "{word} is missing from {:?}", found.terms);
        }
        assert!(!found.terms.iter().any(|t| t == "deploy-everything"), "a script is a command, not a word: {:?}", found.terms);
        // Plain words that a speech model does not miss are not there: `main`, `push`, `new`.
        assert!(!found.terms.iter().any(|t| matches!(t.as_str(), "push" | "main" | "src" | "app")), "{:?}", found.terms);
        assert!(found.files_seen >= 4 && !found.truncated && found.skipped_secret == 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nothing_secret_is_opened_named_or_kept() {
        let root = scratch("secret");
        put(&root, ".env", "API_TOKEN=abc");
        put(&root, "config/credentials.json", "{\"password\": \"hunter2\"}");
        put(&root, "keys/server.pem", "-----BEGIN PRIVATE KEY-----");
        put(&root, "secrets.rs", "pub struct VeryDistinctSecretHolder {}\n");
        put(&root, "src/auth.rs", "pub struct SessionTokenStore {}\npub const API_KEY_NAME: &str = \"x\";\npub fn passwordHasher() {}\npub struct HarmlessWidgetFactory {}\npub struct ghp_abcdefghijklmnopqrstuvwxyz0123456789 {}\npub struct abcdef0123456789abcdef {}\n");
        put(&root, ".git/config", "[remote \"origin\"]\n\turl = https://user:pw@example.com/x.git\n");
        put(&root, "node_modules/leftpad/index.js", "function leftpadding_helper() {}\n");
        put(&root, "target/debug/build_artifact.rs", "pub struct FromTarget {}\n");
        put(&root, "data/.aqua_api_token", "tok");
        let found = collect(&root);
        let all = found.terms.join(" ");
        for banned in ["VeryDistinctSecretHolder", "SessionTokenStore", "API_KEY_NAME", "passwordHasher", "ghp_", "abcdef0123456789abcdef", "credentials", "hunter2", "FromTarget", "leftpadding_helper", "server", ".env", "aqua_api_token"] {
            assert!(!all.contains(banned), "{banned} must not be in {all}");
        }
        assert!(found.terms.iter().any(|t| t == "HarmlessWidgetFactory"), "{:?}", found.terms);
        assert!(found.skipped_secret >= 3, "{found:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn at_most_two_thousand_files_are_looked_at_and_the_prompt_stays_within_the_dictation_limits() {
        let root = scratch("caps");
        // Ten folders of 210 files: a folder is read to 500 entries at most, so the 2,000 files are what stops the walk.
        for i in 0..2_100 {
            put(&root, &format!("d{}/module_{:04}_widget_thing.rs", i % 10, i), "pub fn x() {}\n");
        }
        let found = collect(&root);
        assert!(found.truncated && found.files_seen == MAX_FILES, "{} {}", found.truncated, found.files_seen);
        assert!(found.terms.len() <= MAX_TERMS, "{}", found.terms.len());
        assert!(prompt_of(&found.terms).chars().count() <= MAX_PROMPT_CHARS);
        assert!(found.terms.len() >= 20, "there are many words to choose from, and the prompt takes as many as fit: {}", found.terms.len());
        let long: Vec<String> = (0..300).map(|i| format!("a_distinct_term_number_{i:04}")).collect();
        assert!(prompt_of(&long).chars().count() <= MAX_PROMPT_CHARS && prompt_of(&long).split(", ").count() <= MAX_TERMS);
        assert_eq!(prompt_of(&[]), "");
        assert_eq!(prompt_of(&["alpha_one".into(), " ".into(), "beta_two".into()]), "alpha_one, beta_two");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn newer_files_and_more_declarations_rank_first_and_a_link_is_never_followed() {
        let root = scratch("rank");
        put(&root, "old/older_module.rs", "pub struct OldFashionedThing {}\n");
        // A small gap in time between the two writes, so that the second is the newer.
        std::thread::sleep(Duration::from_millis(30));
        put(&root, "new/newer_module.rs", "pub struct FreshlyWrittenThing {}\npub struct FreshlyWrittenThing2 {}\n");
        let found = collect(&root);
        let at = |w: &str| found.terms.iter().position(|t| t == w).unwrap_or(usize::MAX);
        assert!(at("FreshlyWrittenThing") <= at("OldFashionedThing"), "{:?}", found.terms);
        // A folder that is only a link is not entered (a link needs a privilege on Windows; the test is skipped where it cannot be made).
        let outside = scratch("rank_outside");
        put(&outside, "outside.rs", "pub struct BehindTheLink {}\n");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();
            assert!(!collect(&root).terms.iter().any(|t| t == "BehindTheLink"));
        }
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn the_persons_own_words_come_first_and_a_word_is_never_twice() {
        let personal: Vec<String> = vec!["Kubernetes".into(), "parse_strict".into()];
        let project: Vec<String> = vec!["Parse_Strict".into(), "AppState".into(), "kubernetes".into(), " ".into()];
        assert_eq!(merge(&personal, &project), ["Kubernetes", "parse_strict", "AppState"]);
        let many: Vec<String> = (0..300).map(|i| format!("term_{i:03}")).collect();
        let merged = merge(&personal, &many);
        assert!(merged.len() <= MAX_TERMS && merged.len() > 50, "{}", merged.len());
        assert!(merged[0] == "Kubernetes" && prompt_of(&merged).chars().count() <= MAX_PROMPT_CHARS);
        // The same limit as the dictation settings are held to.
        assert_eq!((MAX_TERMS, MAX_PROMPT_CHARS), (crate::dictation::MAX_TERMS, 800));
    }

    #[test]
    fn a_line_that_declares_a_name_is_read_the_way_the_languages_write_it() {
        assert_eq!(declared_name("pub fn load_all(&self) {").as_deref(), Some("load_all"));
        assert_eq!(declared_name("pub(crate) async fn run_it() {").as_deref(), Some("run_it"));
        assert_eq!(declared_name("impl<T> Store<T> {").as_deref(), Some("Store"));
        assert_eq!(declared_name("export default class AppShell extends Base {").as_deref(), Some("AppShell"));
        assert_eq!(declared_name("def parse_strict(text):").as_deref(), Some("parse_strict"));
        assert_eq!(declared_name("    def method_in_class(self):").as_deref(), Some("method_in_class"));
        assert_eq!(declared_name("            def too_deep(self):"), None);
        assert_eq!(declared_name("let x = fn_call();"), None);
        assert_eq!(declared_name("// fn commented_out() {}"), None);
        assert_eq!(declared_name("pub const API_KEY: &str = \"x\";"), None, "a line about a key is not read");
        assert_eq!(declared_name(""), None);
    }

    #[test]
    fn the_words_of_a_manifest_are_names_only() {
        assert_eq!(cargo_names("[package]\nname = \"a-b\"\nversion = \"1\"\n[dependencies]\nx_y = \"1\"\nz = { path = \"../z\" }\n[dev-dependencies]\nproptest = \"1\""), ["a-b", "x_y", "z", "proptest"]);
        assert_eq!(package_json_names("{\"name\":\"@s/pkg\",\"dependencies\":{\"a\":\"1\"},\"devDependencies\":{\"@t/b\":\"2\"}}"), ["s-pkg", "a", "t-b"]);
        assert!(package_json_names("not json").is_empty());
        assert!(cargo_names("").is_empty());
    }
}
