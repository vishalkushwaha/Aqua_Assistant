//! Declarative command tools (card OA11-02; tasks EX-09 and TL-11 of the OpenCode audit).
//!
//! A person (or a repository whose `.aqua/tools.json` the person trusted) names a command of their own: a fixed program, a template of arguments and a typed value for each blank.
//! The model sees one tool for it (`proj.test(crate)`, `cmd.lint(path)`) and does not have to compose a command line.
//!
//! ```json
//! {"tools": [{
//!   "name": "proj.test",
//!   "description": "Run the tests of one crate.",
//!   "command": ["cargo", "test", "-p", "{crate}", "--lib"],
//!   "args": {"crate": {"type": "string", "pattern": "^[a-z0-9_-]{1,40}$"}},
//!   "cwd": "project", "timeout_ms": 600000, "env": {"RUST_LOG": "warn"}
//! }]}
//! ```
//!
//! **It is not a way round `term.run`.** A call of such a tool is turned into the `term.run` call it stands for (program, arguments, folder, environment, time) and is judged and run
//! **by the terminal provider itself**: the same classifier, the same rules of `term_rules.json` (a `forbid` rule still forbids it, a `prompt` rule still asks), the same folder
//! guard, the same environment allow-list, the same limits and the same question that shows the whole command. What the declaration adds is only the typed blanks. So a declaration
//! can never be looser than the command it stands for.
//!
//! * **No shell, no concatenation.** `{name}` is accepted only as a whole argument; the value goes in as exactly one argument of the program, never through a shell and never split.
//!   The program (the first element) is fixed text and is never a blank.
//! * **Every value is checked before it is placed** against its declaration: text of a length, a pattern (anchored), a list of choices; a whole number in bounds. A value that
//!   begins with `-` (it would be read as an option) is refused unless the argument says `"allow_dash": true`. A value never contains a control character.
//! * **Who may name what.** The file of the person (`data/tools/custom.json`) names `cmd.*` tools; a trusted repository (`.aqua/tools.json`) names `proj.*` tools. A name in any other
//!   domain (`term.*`, `fs.*`) is refused, so a declaration cannot stand in for a tool Aqua has. The file's own word counts for `read_only` only in the person's file.
//! * **Where the declarations come from** is the business of a [`CommandSource`]: the person's file is read once when Aqua starts; the tools of a repository are looked up for the
//!   run that makes the call, so that two projects may both have a `proj.test`, and a run in a project that is not trusted has none.

use super::TermProvider;
use crate::contain::ENV_MODEL_MAY_SET;
use aqua_tool_host::provider::ClassifyContext;
use aqua_tool_host::{Annotations, Availability, Caller, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// The most tools one file may declare.
pub const MAX_TOOLS: usize = 24;
/// The most arguments one tool may declare, and the most elements of its command.
pub const MAX_ARGS: usize = 8;
pub const MAX_COMMAND_ELEMENTS: usize = 32;
/// The most characters of one element of a command, and of one value.
const MAX_ELEMENT_CHARS: usize = 512;
const DEFAULT_MAX_VALUE_CHARS: usize = 200;
const HARD_MAX_VALUE_CHARS: usize = 1000;
/// The longest one `term.run` call waits: a declaration that asks for more is kept as a background job at that time (`term.output` reads it).
const TERM_RUN_MAX_MS: u64 = 300_000;
const MAX_TIMEOUT_MS: u64 = 3_600_000;

/// Which kind of declaration a file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    /// `data/tools/custom.json`: the person's own, `cmd.*`.
    Cmd,
    /// `.aqua/tools.json` of a repository the person trusted, `proj.*`.
    Proj,
}

impl Domain {
    pub fn prefix(self) -> &'static str {
        match self {
            Domain::Cmd => "cmd.",
            Domain::Proj => "proj.",
        }
    }
}

/// A path as a program and the folder guard take it: the extended-length prefix that `canonicalize` puts in front of a path on Windows (`\\?\E:\work`) is taken off, because the guard reads it as a
/// network path and refuses it. A real network path (`\\?\UNC\server\share`) is left as it is, and so is refused.
pub fn plain_path(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => rest.to_string(),
        _ => text.to_string(),
    }
}

/// What a blank accepts.
#[derive(Debug, Clone)]
pub enum ArgKind {
    Text { pattern: Option<Regex>, pattern_source: Option<String>, min: usize, max: usize, choices: Vec<String> },
    Number { min: i64, max: i64 },
}

/// One blank of a command.
#[derive(Debug, Clone)]
pub struct ArgSpec {
    pub name: String,
    pub description: String,
    pub kind: ArgKind,
    /// What is used when the call leaves the blank out; without one the blank is required.
    pub default: Option<String>,
    /// A value that begins with `-` is allowed (the program wants an option there).
    pub allow_dash: bool,
}

/// Where the program runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cwd {
    /// The folder of the project the run works in (the workspace when there is none).
    Project,
    /// The workspace, the folder `term.run` uses when no folder is named.
    Workspace,
    /// A folder the person named (`@ws/tools`, an absolute path): `term.run`'s guard decides whether it may be used. Only the person's own file may name one.
    Folder(String),
}

/// One declared command.
#[derive(Debug, Clone)]
pub struct CustomTool {
    pub name: String,
    pub description: String,
    /// The program, then its arguments; an argument that is exactly `{name}` is a blank.
    pub command: Vec<String>,
    pub args: Vec<ArgSpec>,
    pub cwd: Cwd,
    pub timeout_ms: u64,
    pub env: Vec<(String, String)>,
    /// The person said in their own file that this command only looks at things. Never honoured for a repository's tool.
    pub read_only: bool,
    pub domain: Domain,
}

/// What a file declared, and what was left out and why.
#[derive(Debug, Default)]
pub struct Loaded {
    pub tools: Vec<CustomTool>,
    pub warnings: Vec<String>,
}

fn valid_word(w: &str, max: usize) -> bool {
    !w.is_empty() && w.len() <= max && w.chars().next().is_some_and(|c| c.is_ascii_lowercase()) && w.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn is_placeholder(element: &str) -> Option<&str> {
    let inner = element.strip_prefix('{')?.strip_suffix('}')?;
    valid_word(inner, 32).then_some(inner)
}

/// Does the element hold something that looks like a blank without being exactly one (`--out={path}`)? That is concatenation, which is refused.
fn has_embedded_blank(element: &str) -> bool {
    let bytes = element.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = element[i + 1..].find('}') {
                if valid_word(&element[i + 1..i + 1 + end], 32) {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

fn one_line(s: &str, max: usize) -> Option<String> {
    let line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    (!line.is_empty() && line.chars().count() <= max && !s.contains('\0')).then_some(line)
}

fn parse_arg(name: &str, spec: &Value, warnings: &mut Vec<String>, tool: &str) -> Result<ArgSpec, String> {
    if !valid_word(name, 32) {
        return Err(format!("the argument name {name:?} must be 1 to 32 of a-z, 0-9 and _, and begin with a letter"));
    }
    let obj = spec.as_object().ok_or_else(|| format!("argument {name}: it must be an object ({{\"type\": \"string\"}})"))?;
    for k in obj.keys() {
        if !["type", "description", "pattern", "minLength", "maxLength", "enum", "minimum", "maximum", "default", "allow_dash"].contains(&k.as_str()) {
            warnings.push(format!("{tool}: argument {name}: the key {k:?} is not one this knows and was ignored"));
        }
    }
    let description = match obj.get("description") {
        None => String::new(),
        Some(d) => one_line(d.as_str().unwrap_or(""), 80).ok_or_else(|| format!("argument {name}: description must be one line of at most 80 characters"))?,
    };
    let allow_dash = obj.get("allow_dash").and_then(Value::as_bool).unwrap_or(false);
    let default = match obj.get("default") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(_) => return Err(format!("argument {name}: default must be text or a number")),
    };
    let kind = match obj.get("type").and_then(Value::as_str) {
        Some("string") => {
            let min = obj.get("minLength").and_then(Value::as_u64).unwrap_or(1).min(HARD_MAX_VALUE_CHARS as u64) as usize;
            let max = obj.get("maxLength").and_then(Value::as_u64).unwrap_or(DEFAULT_MAX_VALUE_CHARS as u64).clamp(1, HARD_MAX_VALUE_CHARS as u64) as usize;
            if min > max {
                return Err(format!("argument {name}: minLength is more than maxLength"));
            }
            let (pattern, pattern_source) = match obj.get("pattern") {
                None => (None, None),
                Some(p) => {
                    let p = p.as_str().filter(|p| !p.is_empty() && p.len() <= 200).ok_or_else(|| format!("argument {name}: pattern must be text of at most 200 characters"))?;
                    // Anchored on both ends, whatever it was written as: a pattern that finds its text somewhere inside a value would let the rest through.
                    let body = p.strip_prefix('^').unwrap_or(p);
                    let body = body.strip_suffix('$').filter(|b| !b.ends_with('\\')).unwrap_or(body);
                    let anchored = format!("^(?:{body})$");
                    let re = regex::RegexBuilder::new(&anchored).size_limit(1 << 20).build().map_err(|e| format!("argument {name}: the pattern is not valid ({e})"))?;
                    (Some(re), Some(p.to_string()))
                }
            };
            let choices: Vec<String> = match obj.get("enum") {
                None => Vec::new(),
                Some(Value::Array(a)) if !a.is_empty() && a.len() <= 32 && a.iter().all(|v| v.as_str().is_some_and(|s| !s.is_empty() && s.chars().count() <= max.max(1))) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
                Some(_) => return Err(format!("argument {name}: enum must be a list of 1 to 32 non-empty texts")),
            };
            ArgKind::Text { pattern, pattern_source, min, max, choices }
        }
        Some("integer") => {
            let min = obj.get("minimum").and_then(Value::as_i64).unwrap_or(0);
            let max = obj.get("maximum").and_then(Value::as_i64).unwrap_or(1_000_000);
            if min > max {
                return Err(format!("argument {name}: minimum is more than maximum"));
            }
            ArgKind::Number { min, max }
        }
        Some(other) => return Err(format!("argument {name}: the type {other:?} is not one this takes (string, integer)")),
        None => return Err(format!("argument {name}: it has no type (string or integer)")),
    };
    let spec = ArgSpec { name: name.to_string(), description, kind, default, allow_dash };
    if let Some(d) = &spec.default {
        // The default is the person's own text, but it is held to the same checks as a value: a default that the declaration refuses would fail on every call that leaves it out.
        check_value(&spec, &Value::String(d.clone())).map_err(|why| format!("argument {name}: the default is refused by the argument's own rules ({why})"))?;
    }
    Ok(spec)
}

fn parse_tool(entry: &Value, domain: Domain, warnings: &mut Vec<String>) -> Result<CustomTool, String> {
    let obj = entry.as_object().ok_or("a tool must be an object")?;
    let name = obj.get("name").and_then(Value::as_str).ok_or("it has no name")?.to_string();
    let Some(short) = name.strip_prefix(domain.prefix()) else {
        return Err(format!("the name {name:?} must begin with {} (a declaration cannot take the place of a tool of another domain)", domain.prefix()));
    };
    if !valid_word(short, 40) {
        return Err(format!("the name {name:?}: after the dot it must be 1 to 40 of a-z, 0-9 and _, and begin with a letter"));
    }
    for k in obj.keys() {
        if !["name", "description", "command", "args", "cwd", "timeout_ms", "env", "read_only"].contains(&k.as_str()) {
            warnings.push(format!("{name}: the key {k:?} is not one this knows and was ignored"));
        }
    }
    let description = one_line(obj.get("description").and_then(Value::as_str).unwrap_or(""), 160).ok_or("description must be one line of 1 to 160 characters")?;
    let command: Vec<String> = match obj.get("command") {
        Some(Value::Array(a)) if !a.is_empty() && a.len() <= MAX_COMMAND_ELEMENTS && a.iter().all(|v| v.as_str().is_some_and(|s| s.chars().count() <= MAX_ELEMENT_CHARS && !s.contains('\0'))) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        _ => return Err(format!("command must be a list of 1 to {MAX_COMMAND_ELEMENTS} texts: the program, then its arguments (no shell line)")),
    };
    if command[0].trim().is_empty() || is_placeholder(&command[0]).is_some() || has_embedded_blank(&command[0]) {
        return Err("the program (the first element of command) is fixed text: it cannot be a blank".into());
    }
    let mut args = Vec::new();
    if let Some(a) = obj.get("args") {
        let a = a.as_object().ok_or("args must be an object: each blank's name and what it takes")?;
        if a.len() > MAX_ARGS {
            return Err(format!("at most {MAX_ARGS} arguments"));
        }
        for (k, v) in a {
            args.push(parse_arg(k, v, warnings, &name)?);
        }
    }
    let mut used: Vec<&str> = Vec::new();
    for el in &command[1..] {
        match is_placeholder(el) {
            Some(blank) => {
                if !args.iter().any(|a| a.name == blank) {
                    return Err(format!("the blank {{{blank}}} in command is not declared in args"));
                }
                used.push(blank);
            }
            None if has_embedded_blank(el) => return Err(format!("{el:?}: a blank must be a whole argument ({{name}}), not a part of one: the value is one argument and is never joined to other text")),
            None => {}
        }
    }
    for a in &args {
        if !used.contains(&a.name.as_str()) {
            return Err(format!("the argument {} is declared and not used in command", a.name));
        }
    }
    let cwd = match obj.get("cwd").map(|v| v.as_str()) {
        None => Cwd::Workspace,
        Some(Some("project")) => Cwd::Project,
        Some(Some("workspace" | "")) => Cwd::Workspace,
        Some(Some(other)) if domain == Domain::Cmd && !other.contains('\0') && other.len() <= 1024 => Cwd::Folder(other.to_string()),
        Some(Some(_)) => return Err("cwd of a project's tool is \"project\" or \"workspace\"".into()),
        Some(None) => return Err("cwd must be text".into()),
    };
    let timeout_ms = match obj.get("timeout_ms") {
        None => 120_000,
        Some(v) => v.as_u64().filter(|t| (1_000..=MAX_TIMEOUT_MS).contains(t)).ok_or_else(|| format!("timeout_ms must be a whole number from 1000 to {MAX_TIMEOUT_MS}"))?,
    };
    let mut env = Vec::new();
    if let Some(e) = obj.get("env") {
        let e = e.as_object().ok_or("env must be an object of names and texts")?;
        for (k, v) in e {
            // The same list the model may set for `term.run`: nothing else, whoever writes it (a declaration cannot hand a program a variable `term.run` would refuse).
            if !ENV_MODEL_MAY_SET.iter().any(|a| a.eq_ignore_ascii_case(k)) {
                return Err(format!("env {k}: only {} may be set", ENV_MODEL_MAY_SET.join(", ")));
            }
            let v = v.as_str().filter(|v| v.len() <= 256 && !v.contains('\0')).ok_or_else(|| format!("env {k}: the value must be text of at most 256 characters"))?;
            env.push((k.clone(), v.to_string()));
        }
    }
    let read_only = match (obj.get("read_only").and_then(Value::as_bool), domain) {
        (Some(true), Domain::Cmd) => true,
        (Some(true), Domain::Proj) => {
            warnings.push(format!("{name}: read_only is for your own file only; a repository's tool is never read-only"));
            false
        }
        _ => false,
    };
    Ok(CustomTool { name, description, command, args, cwd, timeout_ms, env, read_only, domain })
}

/// Reads the text of a declaration file. `Err` when it is not a `{"tools": [...]}` document at all; a tool that is not right is left out and named in the warnings, and the others
/// still load.
pub fn parse_file(text: &str, domain: Domain) -> Result<Loaded, String> {
    let doc: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| format!("not valid JSON: {e}"))?;
    let list = doc.get("tools").and_then(Value::as_array).ok_or("the file must be {\"tools\": [ ... ]}")?;
    let mut out = Loaded::default();
    if let Some(obj) = doc.as_object() {
        for k in obj.keys().filter(|k| *k != "tools") {
            out.warnings.push(format!("the key {k:?} is not one this knows and was ignored"));
        }
    }
    if list.len() > MAX_TOOLS {
        out.warnings.push(format!("the file declares {} tools; only the first {MAX_TOOLS} are read", list.len()));
    }
    for entry in list.iter().take(MAX_TOOLS) {
        let label = entry.get("name").and_then(Value::as_str).unwrap_or("(no name)").to_string();
        match parse_tool(entry, domain, &mut out.warnings) {
            Ok(t) if out.tools.iter().any(|x| x.name == t.name) => out.warnings.push(format!("{}: declared twice; the second one was left out", t.name)),
            Ok(t) => out.tools.push(t),
            Err(why) => out.warnings.push(format!("{label}: {why}; left out")),
        }
    }
    Ok(out)
}

/// What one value must be.
fn check_value(spec: &ArgSpec, value: &Value) -> Result<String, String> {
    let text = match (&spec.kind, value) {
        (ArgKind::Text { .. }, Value::String(s)) => s.clone(),
        (ArgKind::Text { .. }, _) => return Err("it must be text".into()),
        (ArgKind::Number { min, max }, v) => {
            let n = match v {
                Value::Number(n) => n.as_i64(),
                Value::String(s) => s.trim().parse::<i64>().ok(),
                _ => None,
            }
            .ok_or("it must be a whole number")?;
            if n < *min || n > *max {
                return Err(format!("{n} is outside {min}..{max}"));
            }
            n.to_string()
        }
    };
    if text.contains('\0') || text.chars().any(|c| c.is_control()) {
        return Err("it contains a control character or a line break".into());
    }
    if text.starts_with('-') && !spec.allow_dash {
        return Err("it begins with '-', which the program would read as an option (the declaration may say \"allow_dash\": true if the program wants that)".into());
    }
    if let ArgKind::Text { pattern, pattern_source, min, max, choices } = &spec.kind {
        let n = text.chars().count();
        if n < *min || n > *max {
            return Err(format!("it must be {min} to {max} characters, not {n}"));
        }
        if !choices.is_empty() && !choices.contains(&text) {
            return Err(format!("it must be one of: {}", choices.join(", ")));
        }
        if let Some(re) = pattern {
            if !re.is_match(&text) {
                return Err(format!("it does not match the pattern {}", pattern_source.as_deref().unwrap_or("")));
            }
        }
    }
    Ok(text)
}

impl CustomTool {
    /// The input schema of the tool as the model reads it. (The host's own checks read the keywords it knows; the pattern and the rest are enforced by [`CustomTool::render`].)
    pub fn schema(&self) -> Value {
        let mut props = Map::new();
        let mut required = Vec::new();
        for a in &self.args {
            let mut p = Map::new();
            if !a.description.is_empty() {
                p.insert("description".into(), json!(a.description));
            }
            match &a.kind {
                ArgKind::Text { min, max, choices, .. } => {
                    p.insert("type".into(), json!("string"));
                    p.insert("minLength".into(), json!(min));
                    p.insert("maxLength".into(), json!(max));
                    if !choices.is_empty() {
                        p.insert("enum".into(), json!(choices));
                    }
                }
                ArgKind::Number { min, max } => {
                    p.insert("type".into(), json!("integer"));
                    p.insert("minimum".into(), json!(min));
                    p.insert("maximum".into(), json!(max));
                }
            }
            if let Some(d) = &a.default {
                p.insert("default".into(), if matches!(a.kind, ArgKind::Number { .. }) { d.parse::<i64>().map(|n| json!(n)).unwrap_or_else(|_| json!(d)) } else { json!(d) });
            } else {
                required.push(a.name.clone());
            }
            props.insert(a.name.clone(), Value::Object(p));
        }
        json!({"type": "object", "properties": props, "required": required})
    }

    /// The manifest the host registers. `permissive`: the host's own check lets any arguments through, for a tool that is looked up per run (the real checks are [`CustomTool::render`], made for
    /// the run's own declaration), and the description says nothing of the project.
    pub fn manifest(&self, permissive: bool) -> ToolManifest {
        let timeout = self.timeout_ms.min(TERM_RUN_MAX_MS) + 15_000;
        ToolManifest {
            name: self.name.clone(),
            title: self.name.clone(),
            description: if permissive { "A command of the project this run works in (its own declaration says what it takes).".into() } else { self.description.clone() },
            classes: vec![PermClass::Execute],
            default_decision: Decision::Auto,
            input_schema: if permissive { json!({"type": "object", "properties": {}, "additionalProperties": true}) } else { self.schema() },
            timeout_default_ms: timeout,
            timeout_max_ms: timeout,
            annotations: Annotations { read_only: self.read_only, destructive: !self.read_only, idempotent: false, open_world: true },
            // What the program prints is outside text, whoever declared the program: the same as for `term.run`, which this is a way to call.
            untrusted_output: true,
        }
    }

    /// The command line of a call: the program and its arguments, each blank replaced by its checked value as one argument. `Err` says which value was refused and why, in words the
    /// model can act on.
    pub fn render(&self, args: &Value) -> Result<Vec<String>, String> {
        let given: Map<String, Value> = match args {
            Value::Null => Map::new(),
            Value::Object(o) => o.clone(),
            _ => return Err("the arguments must be an object".into()),
        };
        if let Some(extra) = given.keys().find(|k| !self.args.iter().any(|a| &a.name == *k)) {
            let known: Vec<&str> = self.args.iter().map(|a| a.name.as_str()).collect();
            return Err(format!("{}: there is no argument {extra:?}; it takes {}", self.name, if known.is_empty() { "none".to_string() } else { known.join(", ") }));
        }
        let mut values: HashMap<&str, String> = HashMap::new();
        for a in &self.args {
            let value = match (given.get(&a.name), &a.default) {
                (Some(v), _) => check_value(a, v).map_err(|why| format!("{}: argument {}: {why}", self.name, a.name))?,
                (None, Some(d)) => check_value(a, &Value::String(d.clone())).map_err(|why| format!("{}: argument {}: the default {why}", self.name, a.name))?,
                (None, None) => return Err(format!("{}: argument {} is required", self.name, a.name)),
            };
            values.insert(a.name.as_str(), value);
        }
        Ok(self.command.iter().map(|el| is_placeholder(el).and_then(|b| values.get(b).cloned()).unwrap_or_else(|| el.clone())).collect())
    }

    /// `(crate: string)` and the description, the line the model is shown for the tool.
    pub fn shown_line(&self) -> String {
        let manifest = self.manifest(false);
        let sig = manifest.typed_signature();
        let args = sig.split_once('(').map(|(_, rest)| format!("({rest}")).unwrap_or_else(|| "()".into());
        format!("{args} {}", self.description)
    }
}

/// Where the declarations of a tool are found when a call is made.
pub trait CommandSource: Send + Sync {
    /// The declaration of `name` for the run `run` (a source that does not depend on the run ignores it). `None`: the run has no such tool.
    fn tool(&self, run: &str, name: &str) -> Option<Arc<CustomTool>>;
    /// The folder of the project a run works in, for a tool whose `cwd` is `project`.
    fn project_dir(&self, _run: &str) -> Option<PathBuf> {
        None
    }
    /// Is the tool there for what is asking now (listing the tools, a call)? Asked without a run, so a source that depends on the run answers from the run of the thread.
    fn available(&self, _name: &str) -> bool {
        true
    }
}

/// Declarations that do not depend on the run: the person's own file.
pub struct FixedSource(pub HashMap<String, Arc<CustomTool>>);

impl CommandSource for FixedSource {
    fn tool(&self, _run: &str, name: &str) -> Option<Arc<CustomTool>> {
        self.0.get(name).cloned()
    }
}

/// The tools of a [`CommandSource`], offered through the tool host as ordinary tools.
pub struct CommandToolProvider {
    id: String,
    term: TermProvider,
    source: Arc<dyn CommandSource>,
    manifests: Vec<ToolManifest>,
}

impl CommandToolProvider {
    /// The person's own tools: every one is registered with its real manifest.
    pub fn fixed(term: TermProvider, tools: Vec<CustomTool>) -> Self {
        let manifests = tools.iter().map(|t| t.manifest(false)).collect();
        let source = Arc::new(FixedSource(tools.into_iter().map(|t| (t.name.clone(), Arc::new(t))).collect()));
        Self { id: "cmd".into(), term, source, manifests }
    }

    /// One name that is looked up for each run (a repository's tool): the host knows the name, the run's own declaration says what it is.
    pub fn for_run_lookup(term: TermProvider, name: &str, source: Arc<dyn CommandSource>) -> Self {
        let stand_in = CustomTool { name: name.to_string(), description: String::new(), command: vec!["x".into()], args: Vec::new(), cwd: Cwd::Workspace, timeout_ms: 1_000, env: Vec::new(), read_only: false, domain: Domain::Proj };
        Self { id: "proj".into(), term, source, manifests: vec![stand_in.manifest(true)] }
    }

    /// The declaration of the tool for this run and the `term.run` call a call of it stands for.
    fn run_args(&self, run: &str, tool: &str, args: &Value) -> Result<(Arc<CustomTool>, Value), ToolError> {
        let def = self.source.tool(run, tool).ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("{tool} is not a command of this run's project (a project's tools are there only when you have trusted its tools.json)")))?;
        let argv = def.render(args).map_err(|why| ToolError::new(ErrorCode::InvalidArguments, why))?;
        let mut call = json!({"exe": argv[0], "args": argv[1..]});
        match &def.cwd {
            Cwd::Project => {
                if let Some(dir) = self.source.project_dir(run) {
                    call["cwd"] = json!(plain_path(&dir));
                }
            }
            Cwd::Folder(f) => call["cwd"] = json!(f),
            Cwd::Workspace => {}
        }
        if !def.env.is_empty() {
            call["env"] = Value::Object(def.env.iter().map(|(k, v)| (k.clone(), json!(v))).collect());
        }
        call["timeout_s"] = json!(def.timeout_ms.min(TERM_RUN_MAX_MS).div_ceil(1000));
        if def.timeout_ms > TERM_RUN_MAX_MS {
            // Past what one call waits: kept as a background job, which `term.output` reads and `term.stop` ends.
            call["background_on_timeout"] = json!(true);
        }
        Ok((def, call))
    }
}

impl ToolProvider for CommandToolProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        self.manifests.clone()
    }

    fn availability(&self, tool: &str) -> Availability {
        if self.source.available(tool) {
            Availability::Available
        } else {
            Availability::Unavailable("not a command of the project this run works in".into())
        }
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        self.classify_for(tool, args, &ClassifyContext::clean(Caller::Agent))
    }

    /// The classification of the `term.run` the call stands for, said by the terminal provider (its rules, its folder guard, its question), so a declaration is never looser than the command.
    fn classify_for(&self, tool: &str, args: &Value, ctx: &ClassifyContext) -> Result<Classification, ToolError> {
        let (def, call) = self.run_args(&ctx.run, tool, args)?;
        let mut c = self.term.classify_for("term.run", &call, ctx)?;
        if !c.classes.contains(&PermClass::Execute) {
            c.classes.push(PermClass::Execute);
        }
        // The question says what the person said of it: a command they declared as only looking is asked about as one that runs a program, and says so.
        c.summary = if def.read_only { format!("[{tool}, declared read-only by you] {}", c.summary) } else { format!("[{tool}] {}", c.summary) };
        Ok(c)
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        match self.run_args(&ctx.agent_run_id, tool, &args) {
            Ok((_, call)) => self.term.call("term.run", call, ctx),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str, domain: Domain) -> Loaded {
        parse_file(text, domain).unwrap()
    }

    const TEST_TOOL: &str = r#"{"tools":[{"name":"proj.test","description":"Run the tests of one crate.","command":["cargo","test","-p","{crate}","--lib"],"args":{"crate":{"type":"string","pattern":"^[a-z0-9_-]{1,40}$"}},"cwd":"project","timeout_ms":600000,"env":{"RUST_LOG":"warn"}}]}"#;

    #[test]
    fn the_example_of_the_audit_is_read() {
        let l = parse(TEST_TOOL, Domain::Proj);
        assert!(l.warnings.is_empty(), "{:?}", l.warnings);
        let t = &l.tools[0];
        assert_eq!((t.name.as_str(), t.cwd.clone(), t.timeout_ms), ("proj.test", Cwd::Project, 600_000));
        assert_eq!(t.render(&json!({"crate": "aqua-core"})).unwrap(), vec!["cargo", "test", "-p", "aqua-core", "--lib"]);
        let m = t.manifest(false);
        assert_eq!(m.signature(), "proj.test(crate)");
        assert_eq!(m.typed_signature(), "proj.test(crate: string)");
        assert_eq!(t.shown_line(), "(crate: string) Run the tests of one crate.");
        assert!(m.check().is_ok() && m.untrusted_output && !m.annotations.read_only);
        assert!(t.manifest(true).check().is_ok());
    }

    #[test]
    fn a_value_is_one_argument_whatever_it_holds() {
        let l = parse(r#"{"tools":[{"name":"cmd.say","description":"d","command":["echo","{text}"],"args":{"text":{"type":"string","maxLength":100}}}]}"#, Domain::Cmd);
        let t = &l.tools[0];
        for v in ["a b c", "x; rm -rf /", "a && b", "$(whoami)", "\"quoted\"", "%PATH%", "a|b>c"] {
            assert_eq!(t.render(&json!({"text": v})).unwrap(), vec!["echo".to_string(), v.to_string()], "{v}");
        }
    }

    #[test]
    fn a_value_that_would_be_read_as_an_option_is_refused_unless_the_declaration_says_it_may_be() {
        let strict = parse(r#"{"tools":[{"name":"cmd.show","description":"d","command":["git","show","{rev}"],"args":{"rev":{"type":"string"}}}]}"#, Domain::Cmd);
        let why = strict.tools[0].render(&json!({"rev": "--output=x"})).unwrap_err();
        assert!(why.contains("begins with '-'"), "{why}");
        let lax = parse(r#"{"tools":[{"name":"cmd.show","description":"d","command":["git","show","{rev}"],"args":{"rev":{"type":"string","allow_dash":true}}}]}"#, Domain::Cmd);
        assert_eq!(lax.tools[0].render(&json!({"rev": "--stat"})).unwrap(), vec!["git", "show", "--stat"], "one argument, as it was given");
        let n = parse(r#"{"tools":[{"name":"cmd.head","description":"d","command":["head","{n}"],"args":{"n":{"type":"integer","minimum":-5,"maximum":5}}}]}"#, Domain::Cmd);
        assert!(n.tools[0].render(&json!({"n": -1})).unwrap_err().contains("begins with '-'"));
        assert_eq!(n.tools[0].render(&json!({"n": "3"})).unwrap(), vec!["head", "3"]);
    }

    #[test]
    fn values_are_held_to_their_declaration() {
        let l = parse(TEST_TOOL, Domain::Proj);
        let t = &l.tools[0];
        for bad in [json!({"crate": "Aqua Core"}), json!({"crate": "a;b"}), json!({"crate": "a\nb"}), json!({"crate": ""}), json!({"crate": 5}), json!({}), json!({"crate": "a", "extra": 1})] {
            assert!(t.render(&bad).is_err(), "{bad}");
        }
        // The pattern is anchored whatever it was written as: it cannot be satisfied by a part of the value.
        let loose = parse(r#"{"tools":[{"name":"cmd.x","description":"d","command":["x","{v}"],"args":{"v":{"type":"string","pattern":"[a-z]+"}}}]}"#, Domain::Cmd);
        assert!(loose.tools[0].render(&json!({"v": "abc"})).is_ok());
        assert!(loose.tools[0].render(&json!({"v": "abc;rm"})).is_err());
        assert!(loose.tools[0].render(&json!({"v": "1abc"})).is_err());
        let choice = parse(r#"{"tools":[{"name":"cmd.x","description":"d","command":["x","{v}"],"args":{"v":{"type":"string","enum":["debug","release"]}}}]}"#, Domain::Cmd);
        assert!(choice.tools[0].render(&json!({"v": "debug"})).is_ok() && choice.tools[0].render(&json!({"v": "fast"})).is_err());
    }

    #[test]
    fn a_default_fills_a_blank_that_the_call_leaves_out() {
        let l = parse(r#"{"tools":[{"name":"cmd.lint","description":"d","command":["npm","run","{script}","--","{n}"],"args":{"script":{"type":"string","default":"lint"},"n":{"type":"integer","default":3,"minimum":1,"maximum":9}}}]}"#, Domain::Cmd);
        assert_eq!(l.tools[0].render(&json!({})).unwrap(), vec!["npm", "run", "lint", "--", "3"]);
        assert_eq!(l.tools[0].schema()["required"], json!([]));
    }

    #[test]
    fn a_declaration_that_could_be_a_way_round_is_left_out() {
        let bad = [
            // the domain
            (r#"{"name":"term.run","description":"d","command":["x"]}"#, Domain::Cmd, "must begin with cmd."),
            (r#"{"name":"fs.write","description":"d","command":["x"]}"#, Domain::Proj, "must begin with proj."),
            (r#"{"name":"proj.x","description":"d","command":["x"]}"#, Domain::Cmd, "must begin with cmd."),
            // the program is not a blank, a blank is a whole argument, and every blank is declared
            (r#"{"name":"cmd.a","description":"d","command":["{prog}"],"args":{"prog":{"type":"string"}}}"#, Domain::Cmd, "fixed text"),
            (r#"{"name":"cmd.a","description":"d","command":["x","--out={p}"],"args":{"p":{"type":"string"}}}"#, Domain::Cmd, "whole argument"),
            (r#"{"name":"cmd.a","description":"d","command":["x","{p}"]}"#, Domain::Cmd, "not declared"),
            (r#"{"name":"cmd.a","description":"d","command":["x"],"args":{"p":{"type":"string"}}}"#, Domain::Cmd, "not used"),
            // the environment is what term.run allows the model to set
            (r#"{"name":"cmd.a","description":"d","command":["x"],"env":{"AQUA_API_TOKEN":"x"}}"#, Domain::Cmd, "may be set"),
            (r#"{"name":"cmd.a","description":"d","command":["x"],"env":{"PATH":"x"}}"#, Domain::Cmd, "may be set"),
            // a pack's folder is the project or the workspace only
            (r#"{"name":"proj.a","description":"d","command":["x"],"cwd":"C:/Users"}"#, Domain::Proj, "project"),
            // text that is not one line, a shell line
            (r#"{"name":"cmd.a","description":"d","command":"cargo test"}"#, Domain::Cmd, "list"),
            (r#"{"name":"cmd.A","description":"d","command":["x"]}"#, Domain::Cmd, "a-z"),
        ];
        for (entry, domain, why) in bad {
            let l = parse(&format!(r#"{{"tools":[{entry}]}}"#), domain);
            assert!(l.tools.is_empty(), "{entry}");
            assert!(l.warnings.iter().any(|w| w.contains(why)), "{entry}: {:?}", l.warnings);
        }
        let long = format!(r#"{{"tools":[{{"name":"cmd.a","description":"{}","command":["x"]}}]}}"#, "d".repeat(170));
        let l = parse(&long, Domain::Cmd);
        assert!(l.tools.is_empty() && l.warnings.iter().any(|w| w.contains("one line")), "{:?}", l.warnings);
        // The others in the file still load.
        let l = parse(r#"{"tools":[{"name":"term.run","description":"d","command":["x"]},{"name":"cmd.ok","description":"d","command":["x"]}]}"#, Domain::Cmd);
        assert_eq!(l.tools.len(), 1);
    }

    #[test]
    fn read_only_is_the_persons_word_in_their_own_file_only() {
        let own = parse(r#"{"tools":[{"name":"cmd.look","description":"d","command":["x"],"read_only":true}]}"#, Domain::Cmd);
        assert!(own.tools[0].manifest(false).annotations.read_only);
        let theirs = parse(r#"{"tools":[{"name":"proj.look","description":"d","command":["x"],"read_only":true}]}"#, Domain::Proj);
        assert!(!theirs.tools[0].manifest(false).annotations.read_only && theirs.warnings.iter().any(|w| w.contains("read_only")));
    }

    #[test]
    fn a_file_that_is_not_a_list_of_tools_is_an_error_and_a_long_list_is_cut() {
        assert!(parse_file("{", Domain::Cmd).is_err());
        assert!(parse_file(r#"{"commands":[]}"#, Domain::Cmd).is_err());
        let many: Vec<String> = (0..30).map(|i| format!(r#"{{"name":"cmd.t{i}","description":"d","command":["x"]}}"#)).collect();
        let l = parse(&format!(r#"{{"tools":[{}]}}"#, many.join(",")), Domain::Cmd);
        assert_eq!(l.tools.len(), MAX_TOOLS);
        assert!(l.warnings.iter().any(|w| w.contains("only the first")));
        let twice = parse(r#"{"tools":[{"name":"cmd.a","description":"d","command":["x"]},{"name":"cmd.a","description":"d","command":["y"]}]}"#, Domain::Cmd);
        assert_eq!((twice.tools.len(), twice.tools[0].command[0].as_str()), (1, "x"));
    }

    #[test]
    fn the_folder_of_a_project_is_given_to_the_terminal_without_the_prefix_a_canonical_path_has() {
        assert_eq!(plain_path(std::path::Path::new(r"\\?\E:\work\proj")), r"E:\work\proj");
        assert_eq!(plain_path(std::path::Path::new(r"E:\work\proj")), r"E:\work\proj");
        assert_eq!(plain_path(std::path::Path::new(r"\\?\UNC\server\share")), r"\\?\UNC\server\share", "a network path stays one, and is refused where it is used");
        struct Here;
        impl CommandSource for Here {
            fn tool(&self, _: &str, _: &str) -> Option<Arc<CustomTool>> {
                parse_file(TEST_TOOL, Domain::Proj).ok().and_then(|l| l.tools.into_iter().next()).map(Arc::new)
            }
            fn project_dir(&self, _: &str) -> Option<PathBuf> {
                Some(PathBuf::from(r"\\?\E:\work\proj"))
            }
        }
        let provider = CommandToolProvider { id: "proj".into(), term: super::super::TermProvider::new(Arc::new(crate::fs::PathGuard::new(crate::fs::FsConfig::with_workspace(std::env::temp_dir().to_string_lossy().to_string()), vec![]).unwrap()), Default::default()), source: Arc::new(Here), manifests: vec![] };
        let call = provider.run_args("run_1", "proj.test", &json!({"crate": "aqua-core"})).unwrap().1;
        assert_eq!(call["cwd"], json!(r"E:\work\proj"));
    }

    #[test]
    fn a_time_past_what_one_call_waits_is_kept_as_a_job() {
        struct One(Arc<CustomTool>);
        impl CommandSource for One {
            fn tool(&self, _: &str, _: &str) -> Option<Arc<CustomTool>> {
                Some(self.0.clone())
            }
        }
        let l = parse(TEST_TOOL, Domain::Proj);
        let provider = CommandToolProvider { id: "proj".into(), term: super::super::TermProvider::new(Arc::new(crate::fs::PathGuard::new(crate::fs::FsConfig::with_workspace(std::env::temp_dir().to_string_lossy().to_string()), vec![]).unwrap()), Default::default()), source: Arc::new(One(Arc::new(l.tools[0].clone()))), manifests: vec![] };
        let call = provider.run_args("run_1", "proj.test", &json!({"crate": "aqua-core"})).unwrap().1;
        assert_eq!(call["timeout_s"], json!(300));
        assert_eq!(call["background_on_timeout"], json!(true));
        assert_eq!(call["env"], json!({"RUST_LOG": "warn"}));
        assert_eq!((call["exe"].clone(), call["args"].clone()), (json!("cargo"), json!(["test", "-p", "aqua-core", "--lib"])));
    }
}
