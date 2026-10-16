//! The language servers the person allowed, from `data/tools/lsp.json` (card OA12-04, task CU-15 of the OpenCode audit).
//!
//! The only place a server comes from is this file, which the person writes: no tool can read or change `data/tools/`. It is never read from the project that is being edited (a repository
//! could hold anything), from a project pack, from the model's words, from an environment variable or from a search of `PATH` when a call is made. A server is named by language, with an
//! absolute path to its program, fixed arguments, and the SHA-256 of the program: the bridge starts a program only when its bytes have that hash.
//!
//! ```json
//! {"servers": {
//!   "rust": {"command": "C:/tools/rust-analyzer/rust-analyzer.exe", "args": [], "sha256": "<64 hex digits>"},
//!   "python": {"command": "C:/tools/pyright/pyright-langserver.exe", "args": ["--stdio"], "sha256": "<64 hex digits>", "request_timeout_secs": 20}
//! }}
//! ```
//!
//! A server without a hash is **not started** (the answer says how to add it). Nothing is downloaded or installed, ever.

use aqua_tool_host::config_text::{self, ConfigError, ConfigErrorKind};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The file, inside `data/tools/`.
pub const FILE_NAME: &str = "lsp.json";

/// The languages a server can be named for: the names of the keys of `servers`, and the language of a file by its extension.
pub const LANGUAGES: &[&str] = &["rust", "python", "typescript", "javascript", "go", "csharp", "java", "kotlin", "c", "cpp", "ruby", "php"];

pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_STARTUP_TIMEOUT_SECS: u64 = 20;
pub const DEFAULT_IDLE_SECS: u64 = 300;
pub const DEFAULT_MEMORY_MB: u64 = 4096;
pub const DEFAULT_MAX_PROCESSES: u32 = 64;
pub const DEFAULT_MAX_REQUESTS_PER_RUN: u32 = 60;
pub const DEFAULT_MAX_RESPONSE_BYTES: u64 = 4 << 20;

/// What the file says about one language, as it was written.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ServerFile {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    request_timeout_secs: Option<u64>,
    #[serde(default)]
    startup_timeout_secs: Option<u64>,
    #[serde(default)]
    idle_secs: Option<u64>,
    #[serde(default)]
    memory_mb: Option<u64>,
    #[serde(default)]
    max_processes: Option<u32>,
    #[serde(default)]
    max_requests_per_run: Option<u32>,
    #[serde(default)]
    max_response_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    servers: BTreeMap<String, ServerFile>,
}

/// One server, checked and with its limits filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    /// The language it is for (a name of [`LANGUAGES`]).
    pub language: String,
    /// The program: an absolute path.
    pub command: PathBuf,
    pub args: Vec<String>,
    /// The SHA-256 of the program, in lower-case hex; `None` when the file does not give one (the server is then never started).
    pub sha256: Option<String>,
    pub request_timeout_secs: u64,
    pub startup_timeout_secs: u64,
    pub idle_secs: u64,
    pub memory_mb: u64,
    pub max_processes: u32,
    pub max_requests_per_run: u32,
    pub max_response_bytes: u64,
}

/// The servers of the file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LspConfig {
    servers: BTreeMap<String, ServerSpec>,
}

impl LspConfig {
    /// A configuration made in memory (a test, or a caller that has the servers already).
    pub fn from_specs(specs: Vec<ServerSpec>) -> Self {
        Self { servers: specs.into_iter().map(|s| (s.language.clone(), s)).collect() }
    }

    pub fn server_for(&self, language: &str) -> Option<&ServerSpec> {
        self.servers.get(language)
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// The languages that have a server, in order.
    pub fn languages(&self) -> Vec<&str> {
        self.servers.keys().map(String::as_str).collect()
    }

    /// Read `lsp.json` from the folder `tools_dir` (`data/tools`). `Ok(None)`: there is no such file, which is what it is until the person writes one. A file that does not parse, names a
    /// language that is not on the list, or a program that is not an absolute path is an error that says where; nothing of it is used.
    pub fn load(tools_dir: &Path) -> Result<Option<LspConfig>, ConfigError> {
        let path = tools_dir.join(FILE_NAME);
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > config_text::MAX_CONFIG_BYTES as u64) {
            return Err(ConfigError::new(ConfigErrorKind::Unreadable, format!("{FILE_NAME} is too large to be a configuration")));
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(ConfigError::new(ConfigErrorKind::Unreadable, format!("{FILE_NAME} could not be read: {}", e.kind()))),
        };
        Self::parse(&text).map(Some)
    }

    /// The servers of the text of a file (comments and a trailing comma are allowed, as in the other files).
    pub fn parse(text: &str) -> Result<LspConfig, ConfigError> {
        let prepared = config_text::prepare(text)?;
        let file: File = serde_json::from_str(&prepared).map_err(|e| ConfigError::from_serde(&e))?;
        let wrong = |msg: String| ConfigError::new(ConfigErrorKind::Invalid, msg);
        let mut servers = BTreeMap::new();
        for (language, s) in file.servers {
            if !LANGUAGES.contains(&language.as_str()) {
                return Err(wrong(format!("'{language}' is not a language a server can be named for (one of: {})", LANGUAGES.join(", "))));
            }
            let command = PathBuf::from(&s.command);
            if !is_absolute_program(&s.command) {
                return Err(wrong(format!("{language}: \"command\" must be the absolute path of the program (a name to be looked up on PATH is never used)")));
            }
            if s.args.len() > 32 || s.args.iter().any(|a| a.len() > 512 || a.contains('\0')) {
                return Err(wrong(format!("{language}: \"args\" has too many arguments, or one that is too long")));
            }
            let sha256 = match s.sha256.as_deref().map(str::trim) {
                None | Some("") => None,
                Some(h) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) => Some(h.to_ascii_lowercase()),
                Some(_) => return Err(wrong(format!("{language}: \"sha256\" must be 64 hexadecimal digits"))),
            };
            let within = |what: &str, v: u64, lo: u64, hi: u64| -> Result<u64, ConfigError> {
                if (lo..=hi).contains(&v) {
                    Ok(v)
                } else {
                    Err(wrong(format!("{language}: \"{what}\" must be from {lo} to {hi}")))
                }
            };
            let spec = ServerSpec {
                language: language.clone(),
                command,
                args: s.args,
                sha256,
                request_timeout_secs: within("request_timeout_secs", s.request_timeout_secs.unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS), 1, 120)?,
                startup_timeout_secs: within("startup_timeout_secs", s.startup_timeout_secs.unwrap_or(DEFAULT_STARTUP_TIMEOUT_SECS), 1, 300)?,
                idle_secs: within("idle_secs", s.idle_secs.unwrap_or(DEFAULT_IDLE_SECS), 1, 3600)?,
                memory_mb: within("memory_mb", s.memory_mb.unwrap_or(DEFAULT_MEMORY_MB), 256, 16_384)?,
                max_processes: within("max_processes", s.max_processes.map_or(DEFAULT_MAX_PROCESSES as u64, u64::from), 2, 256)? as u32,
                max_requests_per_run: within("max_requests_per_run", s.max_requests_per_run.map_or(DEFAULT_MAX_REQUESTS_PER_RUN as u64, u64::from), 1, 500)? as u32,
                max_response_bytes: within("max_response_bytes", s.max_response_bytes.unwrap_or(DEFAULT_MAX_RESPONSE_BYTES), 4096, 16 << 20)?,
            };
            servers.insert(language, spec);
        }
        Ok(LspConfig { servers })
    }
}

/// Is this the absolute path of a program (a drive and a folder, or a root), and not a name that a search of `PATH` would find, nor a network path, nor a device path?
fn is_absolute_program(text: &str) -> bool {
    if text.is_empty() || text.contains('\0') || text.starts_with("\\\\") || text.starts_with("//") {
        return false;
    }
    let b = text.as_bytes();
    let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
    let rooted = cfg!(not(windows)) && text.starts_with('/');
    (drive || rooted) && !text.split(['/', '\\']).any(|p| p == ".." || p == ".")
}

/// The words for a person who has a server without a hash, or with the wrong one.
pub fn how_to_add_a_hash(language: &str, exe: &Path) -> String {
    format!(
        "add \"sha256\": \"<64 hex digits>\" to the {language} entry of data/tools/lsp.json: the hash of {} (Windows: Get-FileHash -Algorithm SHA256 \"{}\")",
        exe.display(),
        exe.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn win_or_unix_exe() -> &'static str {
        if cfg!(windows) {
            "C:/tools/rust-analyzer/rust-analyzer.exe"
        } else {
            "/opt/tools/rust-analyzer"
        }
    }

    #[test]
    fn a_file_names_servers_by_language_with_their_limits_filled_in() {
        let text = format!(
            r#"{{
  // the Rust server
  "servers": {{
    "rust": {{"command": "{}", "args": ["--log-file", "x"], "sha256": "{}"}},
    "python": {{"command": "{}", "sha256": "{}", "request_timeout_secs": 20, "idle_secs": 60,}},
  }}
}}"#,
            win_or_unix_exe(),
            HASH.to_uppercase(),
            win_or_unix_exe(),
            HASH
        );
        let c = LspConfig::parse(&text).unwrap();
        assert_eq!(c.languages(), vec!["python", "rust"]);
        let rust = c.server_for("rust").unwrap();
        assert_eq!(rust.sha256.as_deref(), Some(HASH), "the hash is kept in lower case");
        assert_eq!(rust.args, vec!["--log-file", "x"]);
        assert_eq!((rust.request_timeout_secs, rust.startup_timeout_secs, rust.idle_secs), (10, 20, 300));
        assert_eq!((rust.memory_mb, rust.max_processes, rust.max_requests_per_run, rust.max_response_bytes), (4096, 64, 60, 4 << 20));
        let py = c.server_for("python").unwrap();
        assert_eq!((py.request_timeout_secs, py.idle_secs), (20, 60));
        assert!(c.server_for("go").is_none());
    }

    #[test]
    fn a_server_without_a_hash_is_kept_without_one_so_that_the_call_can_say_how_to_add_it() {
        let text = format!(r#"{{"servers": {{"rust": {{"command": "{}"}}}}}}"#, win_or_unix_exe());
        let c = LspConfig::parse(&text).unwrap();
        assert_eq!(c.server_for("rust").unwrap().sha256, None);
        let text = format!(r#"{{"servers": {{"rust": {{"command": "{}", "sha256": ""}}}}}}"#, win_or_unix_exe());
        assert_eq!(LspConfig::parse(&text).unwrap().server_for("rust").unwrap().sha256, None);
        let sentence = how_to_add_a_hash("rust", Path::new("C:/x/ra.exe"));
        assert!(sentence.contains("sha256") && sentence.contains("data/tools/lsp.json") && sentence.contains("Get-FileHash"), "{sentence}");
    }

    #[test]
    fn what_is_wrong_in_the_file_is_an_error_and_nothing_of_it_is_used() {
        let ok = win_or_unix_exe();
        let bad: Vec<(String, &str)> = vec![
            (r#"{"servers": {"cobol": {"command": "C:/x/a.exe"}}}"#.into(), "not a language"),
            (format!(r#"{{"servers": {{"rust": {{"command": "rust-analyzer"}}}}}}"#), "absolute path"),
            (format!(r#"{{"servers": {{"rust": {{"command": "C:/tools/../x.exe"}}}}}}"#), "absolute path"),
            (format!(r#"{{"servers": {{"rust": {{"command": "\\\\server\\share\\ra.exe"}}}}}}"#), "absolute path"),
            (format!(r#"{{"servers": {{"rust": {{"command": "./ra.exe"}}}}}}"#), "absolute path"),
            (format!(r#"{{"servers": {{"rust": {{"command": "{ok}", "sha256": "abc"}}}}}}"#), "64 hexadecimal"),
            (format!(r#"{{"servers": {{"rust": {{"command": "{ok}", "sha256": "{}z"}}}}}}"#, &HASH[..63]), "64 hexadecimal"),
            (format!(r#"{{"servers": {{"rust": {{"command": "{ok}", "request_timeout_secs": 0}}}}}}"#), "request_timeout_secs"),
            (format!(r#"{{"servers": {{"rust": {{"command": "{ok}", "memory_mb": 99999}}}}}}"#), "memory_mb"),
            (format!(r#"{{"servers": {{"rust": {{"command": "{ok}", "commmand": "x"}}}}}}"#), ""),
            (r#"{"servers": {}, "other": 1}"#.into(), ""),
            (r#"{"servers": ["rust"]}"#.into(), ""),
            ("not json".into(), ""),
        ];
        for (text, why) in bad {
            let e = LspConfig::parse(&text).expect_err(&text);
            assert!(e.detail().contains(why), "{why}: {}", e.detail());
        }
    }

    #[test]
    fn a_missing_file_is_no_servers_and_a_broken_one_is_an_error() {
        let dir = crate::lsp::scratch("cfg");
        assert_eq!(LspConfig::load(&dir).unwrap(), None);
        std::fs::write(dir.join(FILE_NAME), "{ nope").unwrap();
        assert!(LspConfig::load(&dir).is_err());
        std::fs::write(dir.join(FILE_NAME), format!(r#"{{"servers": {{"go": {{"command": "{}"}}}}}}"#, win_or_unix_exe())).unwrap();
        assert_eq!(LspConfig::load(&dir).unwrap().unwrap().languages(), vec!["go"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
