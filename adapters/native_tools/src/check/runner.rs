//! Choosing the command of a check and turning what it printed into an [`Outcome`] (card OA4-05, task WF-04).
//!
//! [`plan`] reads the call (`kind`, `path`, `filter`, `package`, `timeout_s`, `background`) and the profile of the project and says which command runs, in which folder, for how long.
//! It never runs anything. [`outcome_of`] reads what the terminal tools answered. The terminal does the running: a check is a `term.run` (or `term.start`) of the command the plan names,
//! so it is classified, asked about, timed, contained and audited exactly as that command would be.

use super::card::{tail_of, Outcome, Snippet};
use super::parse::{self, parse_sniffed, tool_for, Failure, Parsed};
use crate::code::{self, CheckKind, CmdSpec, ProjectProfile};
use crate::fs::guard::{display, Op, PathGuard};
use aqua_tool_host::{ErrorCode, ToolError};
use regex::Regex;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The longest a check may run in the foreground (what `term.run` allows).
pub const FOREGROUND_MAX_S: u64 = 300;
/// The longest a check may run as a background job (what `term.start` allows).
pub const BACKGROUND_MAX_S: u64 = 3600;
/// What a background check is given when the call names no time.
const BACKGROUND_DEFAULT_S: u64 = 600;
/// The most of one source file that a snippet reads.
const SNIPPET_MAX_BYTES: u64 = 1024 * 1024;
/// How many lines of source are shown around a failure, on each side of it.
const SNIPPET_CONTEXT: u32 = 3;

/// What a call to `check.run` resolved to.
#[derive(Debug, Clone)]
pub struct Plan {
    pub kind: CheckKind,
    /// The project folder.
    pub root: PathBuf,
    /// Where the command runs.
    pub cwd: PathBuf,
    pub spec: CmdSpec,
    /// The arguments with the filter, the package and the output format of the tool in them.
    pub argv: Vec<String>,
    pub timeout_s: u64,
    /// The call named a time: the command is stopped at it, and not kept as a job.
    pub explicit_timeout: bool,
    /// Run it as a background job from the start.
    pub background: bool,
    /// Keep it as a background job when it is still running at its time (a build, when the call named no time).
    pub promote: bool,
}

impl Plan {
    /// The command as one line.
    pub fn line(&self) -> String {
        std::iter::once(self.spec.exe.as_str()).chain(self.argv.iter().map(String::as_str)).collect::<Vec<_>>().join(" ")
    }

    /// The arguments of the `term.run` (or `term.start`) that runs it.
    pub fn term_args(&self) -> Value {
        if self.background {
            json!({"exe": self.spec.exe, "args": self.argv, "cwd": display(&self.cwd), "name": format!("check {}", self.kind.as_str()), "max_runtime_s": self.timeout_s})
        } else {
            json!({"exe": self.spec.exe, "args": self.argv, "cwd": display(&self.cwd), "timeout_s": self.timeout_s, "max_output_kb": 512, "background_on_timeout": self.promote})
        }
    }

    /// The terminal tool that runs it.
    pub fn term_tool(&self) -> &'static str {
        if self.background {
            "term.start"
        } else {
            "term.run"
        }
    }
}

/// What a call came to before anything ran.
#[derive(Debug, Clone)]
pub enum Planned {
    /// The command to run.
    Run(Box<Plan>),
    /// There is nothing to run, and why (the project has no such command). A result, not an error: the model is told what the project has instead.
    NothingToRun { kind: CheckKind, root: PathBuf, why: String },
}

/// A word the model gave for a test name, a package or a script: a name, never an option or a path.
fn plain_name(v: &str, what: &str) -> Result<String, ToolError> {
    let v = v.trim();
    let ok = !v.is_empty() && v.chars().count() <= 120 && !v.starts_with('-') && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '[' | ']' | '='));
    if ok {
        Ok(v.to_string())
    } else {
        Err(ToolError::new(ErrorCode::InvalidArguments, format!("{what} must be a plain name (letters, digits and _ - . : [ ]), not an option or a path: got `{}`", parse::clip(v, 60))))
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or("").trim()
}

/// The folder the call is about: the file or folder `path` names (inside the allowed folders), or the first allowed folder.
pub fn base_of(guard: &PathGuard, args: &Value) -> Result<(PathBuf, PathBuf), ToolError> {
    let given = arg_str(args, "path");
    let base = if given.is_empty() {
        guard.roots().first().map(|r| r.1.clone()).ok_or_else(|| ToolError::new(ErrorCode::NotFound, "no folder is allowed (data/tools/fs.json)"))?
    } else {
        guard.resolve(given, Op::Read)?.path
    };
    if !base.exists() {
        return Err(ToolError::new(ErrorCode::NotFound, format!("{} does not exist", display(&base))));
    }
    let start = if base.is_dir() { base.clone() } else { base.parent().map(Path::to_path_buf).unwrap_or_else(|| base.clone()) };
    let limit = guard.root_path_of(&start).ok_or_else(|| ToolError::new(ErrorCode::PermissionDenied, format!("{} is not inside a folder the tools may use", display(&base))))?;
    Ok((base, limit))
}

/// The profile of the project that holds `args.path`, and the file or folder it was found from.
pub fn profile_for(guard: &PathGuard, args: &Value, cache: Option<&Path>) -> Result<(ProjectProfile, PathBuf), ToolError> {
    let (base, limit) = base_of(guard, args)?;
    let found = code::nearest_root(&base, &limit).map(|root| code::detect_cached(&root, cache));
    if let Some(profile) = found.as_ref().filter(|p| !p.is_empty()) {
        return Ok((profile.clone(), base));
    }
    // No marker, or a marker whose files say nothing (a `.git` folder above a folder of scripts): the folder the call names is the project when it holds Python tests of its own.
    let start = if base.is_dir() { base.clone() } else { base.parent().map(Path::to_path_buf).unwrap_or_else(|| base.clone()) };
    if code::has_loose_python_tests(&start) {
        return Ok((code::detect_cached(&start, cache), base));
    }
    match found {
        Some(profile) => Ok((profile, base)),
        None => Err(ToolError::new(
            ErrorCode::NotFound,
            format!("no project file (Cargo.toml, package.json, pyproject.toml, go.mod, a solution file, .git ...) is in {} or above it, up to {}", display(&base), display(&limit)),
        )),
    }
}

/// Which command a call runs.
pub fn plan(guard: &PathGuard, args: &Value, cache: Option<&Path>) -> Result<Planned, ToolError> {
    let kind_word = arg_str(args, "kind");
    let kind = CheckKind::parse(kind_word).ok_or_else(|| ToolError::new(ErrorCode::InvalidArguments, format!("kind must be one of tests, build, lint, typecheck: got `{}`", parse::clip(kind_word, 40))))?;
    let (profile, base) = profile_for(guard, args, cache)?;
    if profile.is_empty() {
        return Ok(Planned::NothingToRun { kind, root: profile.root.clone(), why: "the folder has a project marker but no manifest that is understood (project.profile says what was read)".into() });
    }
    let candidates = profile.specs(kind);
    // A file picks the commands of its own language when the project has several ecosystems.
    let file_eco = base.is_file().then(|| code::language_of(&base.to_string_lossy()).and_then(|l| l.ecosystem())).flatten();
    let Some(spec) = file_eco.and_then(|e| candidates.iter().find(|s| s.eco == e)).or_else(|| candidates.first()) else {
        let mut why = format!("this project has no {} command", kind.as_str());
        let others: Vec<String> = CheckKind::ALL.iter().filter(|k| !profile.specs(**k).is_empty()).map(|k| k.as_str().to_string()).collect();
        if !others.is_empty() {
            why.push_str(&format!("; it has: {}", others.join(", ")));
        }
        if let Some(note) = profile.notes.first() {
            why.push_str(&format!(" ({})", parse::clip(note, 160)));
        }
        return Ok(Planned::NothingToRun { kind, root: profile.root.clone(), why });
    };
    let filter = match args.get("filter") {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(plain_name(s, "filter")?),
        _ => None,
    };
    let package = match args.get("package") {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(plain_name(s, "package")?),
        _ => None,
    };
    let mut argv = spec.args.clone();
    let exe = spec.exe.as_str();
    match exe {
        "cargo" => {
            if spec.package_required && package.is_none() {
                let members = profile.workspace_members.iter().take(8).cloned().collect::<Vec<_>>().join(", ");
                return Err(ToolError::new(
                    ErrorCode::InvalidArguments,
                    format!("this folder is a Cargo workspace of {} crates ({members}{}): give package=<crate>, or a path inside one crate. One crate at a time: a whole workspace is not run.", profile.workspace_members.len(), if profile.workspace_members.len() > 8 { ", ..." } else { "" }),
                ));
            }
            if let Some(p) = &package {
                argv.extend(["-p".to_string(), p.clone()]);
            }
            if kind != CheckKind::Tests {
                argv.push("--message-format=short".into());
            }
            if let (CheckKind::Tests, Some(f)) = (kind, &filter) {
                argv.push(f.clone());
            }
        }
        "python" if spec.args.get(1).map(String::as_str) == Some("pytest") => {
            argv.extend(["-q".to_string(), "--tb=short".to_string()]);
            if let Some(f) = &filter {
                argv.extend(["-k".to_string(), f.clone()]);
            }
            if base.is_file() && base.extension().is_some_and(|e| e == "py") {
                if let Ok(rel) = base.strip_prefix(&profile.root) {
                    argv.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
            reject_package(&package, "pytest")?;
        }
        "python" if spec.args.get(1).map(String::as_str) == Some("unittest") => {
            if let Some(f) = &filter {
                argv.push(f.clone());
            }
            reject_package(&package, "unittest")?;
        }
        "go" => {
            if let (CheckKind::Tests, Some(f)) = (kind, &filter) {
                argv.splice(1..1, ["-run".to_string(), f.clone()]);
            }
            reject_package(&package, "go")?;
        }
        "dotnet" => {
            if let (CheckKind::Tests, Some(f)) = (kind, &filter) {
                argv.extend(["--filter".to_string(), f.clone()]);
            }
            reject_package(&package, "dotnet")?;
        }
        _ => {
            if filter.is_some() || package.is_some() {
                return Err(ToolError::new(ErrorCode::InvalidArguments, format!("filter and package are not supported for `{}` (the project's own {} command): run the whole check", spec.line(), kind.as_str())));
            }
        }
    }
    let background = args.get("background").and_then(Value::as_bool).unwrap_or(false);
    let named = args.get("timeout_s").and_then(Value::as_u64);
    let (timeout_s, explicit) = match (named, background) {
        (Some(t), true) => (t.clamp(1, BACKGROUND_MAX_S), true),
        (Some(t), false) => (t.clamp(1, FOREGROUND_MAX_S), true),
        (None, true) => (BACKGROUND_DEFAULT_S, false),
        (None, false) => ((spec.timeout_s as u64).clamp(1, FOREGROUND_MAX_S), false),
    };
    // A build still running at its time is kept as a job, when the call set no time of its own: a build has no good limit, and stopping it throws the work away.
    let promote = kind == CheckKind::Build && !explicit && !background;
    let cwd = if spec.cwd_rel.is_empty() { profile.root.clone() } else { profile.root.join(&spec.cwd_rel) };
    Ok(Planned::Run(Box::new(Plan { kind, root: profile.root.clone(), cwd, spec: spec.clone(), argv, timeout_s, explicit_timeout: explicit, background, promote })))
}

fn reject_package(package: &Option<String>, tool: &str) -> Result<(), ToolError> {
    match package {
        Some(_) => Err(ToolError::new(ErrorCode::InvalidArguments, format!("package is for a Cargo workspace; {tool} has none: use path or filter"))),
        None => Ok(()),
    }
}

/// What `cargo` says when it cannot get a package it was told not to download.
fn offline_failure(text: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"no matching package|failed to get|could not resolve|unable to update registry|failed to download|offline mode").expect("a constant pattern")).is_match(text)
}

/// A path as the card shows it: relative to the folder the command ran in when it is inside it, with forward slashes.
fn relative(file: &str, cwd: &Path) -> String {
    let f = file.replace('\\', "/");
    let base = display(cwd).replace('\\', "/");
    let base = base.trim_end_matches('/');
    if f.len() > base.len() && f.is_char_boundary(base.len()) && f[..base.len()].eq_ignore_ascii_case(base) && f[base.len()..].starts_with('/') {
        f[base.len() + 1..].to_string()
    } else {
        f
    }
}

/// The lines around the place of a failure, read through the folder guard: a file the guard refuses, a secret by its name, a link out of the folders or a big file gives none.
pub fn snippet_of(guard: &PathGuard, plan_cwd: &Path, root: &Path, f: &Failure) -> Option<Snippet> {
    let (file, line) = (f.file.as_deref()?, f.line?);
    if line == 0 {
        return None;
    }
    let direct = Path::new(file);
    let candidates: Vec<PathBuf> = if direct.is_absolute() { vec![direct.to_path_buf()] } else { vec![plan_cwd.join(file), root.join(file)] };
    for c in candidates {
        let Ok(r) = guard.resolve(&c.to_string_lossy(), Op::Read) else { continue };
        if r.sensitive || !r.path.is_file() {
            continue;
        }
        if r.path.file_name().is_some_and(|n| guard.is_sensitive_name(&n.to_string_lossy())) {
            continue;
        }
        if std::fs::metadata(&r.path).map_or(true, |m| m.len() > SNIPPET_MAX_BYTES) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&r.path) else { continue };
        let text = String::from_utf8_lossy(&bytes);
        let from = line.saturating_sub(SNIPPET_CONTEXT).max(1);
        let to = line + SNIPPET_CONTEXT;
        let lines: Vec<(u32, String)> = text.lines().enumerate().map(|(i, l)| (i as u32 + 1, l.to_string())).filter(|(n, _)| *n >= from && *n <= to).collect();
        if lines.iter().any(|(n, _)| *n == line) {
            return Some(Snippet { file: relative(file, plan_cwd), line, lines });
        }
    }
    None
}

/// Everything a `term.run` (or `term.start`) answered, read as a check.
pub fn outcome_of(guard: &PathGuard, plan: &Plan, answer: Result<&Value, &ToolError>) -> Result<Outcome, ToolError> {
    let mut o = Outcome::new(plan.kind, plan.line(), plan.spec.why.clone(), display(&plan.cwd));
    o.limit_s = plan.timeout_s;
    let (stdout, stderr): (String, String);
    match answer {
        Ok(data) => {
            if let Some(id) = data.get("job_id").and_then(Value::as_str) {
                o.job_id = Some(id.to_string());
                o.tail = data.get("last_output").and_then(Value::as_str).unwrap_or("").to_string();
                return Ok(o);
            }
            o.exit_code = data.get("exit_code").and_then(Value::as_i64).map(|c| c as i32);
            o.duration_ms = data.get("duration_ms").and_then(Value::as_u64).unwrap_or(0);
            stdout = data.get("stdout").and_then(Value::as_str).unwrap_or("").to_string();
            stderr = data.get("stderr").and_then(Value::as_str).unwrap_or("").to_string();
        }
        Err(e) if e.code == ErrorCode::Timeout => {
            o.timed_out = true;
            let details = &e.details;
            stdout = details.get("stdout").and_then(Value::as_str).unwrap_or("").to_string();
            stderr = details.get("stderr").and_then(Value::as_str).unwrap_or("").to_string();
            if let Some(s) = details.get("limit_s").and_then(Value::as_u64) {
                o.limit_s = s;
            }
            // The message holds the last of what the program printed when the details were cut.
            if stdout.is_empty() && stderr.is_empty() {
                o.tail = e.message.split("--- the last of what it printed ---\n").nth(1).unwrap_or("").to_string();
            }
        }
        Err(e) => return Err(e.clone()),
    }
    let cleaned_out = parse::clean(&stdout);
    let cleaned_err = parse::clean(&stderr);
    if o.tail.is_empty() {
        // stderr first: a build prints its progress there, and the failure is the last of the two streams.
        let joined = if cleaned_err.trim().is_empty() { cleaned_out.clone() } else if cleaned_out.trim().is_empty() { cleaned_err.clone() } else { format!("{cleaned_err}\n{cleaned_out}") };
        o.tail = tail_of(&joined, 40, 4_000);
    }
    if !o.timed_out {
        let exit = o.exit_code.unwrap_or(-1);
        let tool = tool_for(&plan.spec.exe, &plan.argv);
        let mut parsed: Option<Parsed> = match tool {
            Some(t) => Some(parse::parse(t, &stdout, &stderr, exit)),
            None => parse_sniffed(&stdout, &stderr, exit),
        };
        if let Some(p) = parsed.as_mut() {
            for f in p.failures.iter_mut() {
                if let Some(file) = f.file.take() {
                    f.file = Some(relative(&file, &plan.cwd));
                }
            }
        }
        // Cargo that was told not to download, and could not go on without: it never got as far as a test (a test that says "failed to get" in its own message is not this).
        if plan.spec.offline && exit != 0 && parsed.as_ref().map_or(true, |p| p.counts.is_none()) {
            o.offline_failure = offline_failure(&format!("{cleaned_err}\n{cleaned_out}"));
        }
        if let Some(first) = parsed.as_ref().and_then(|p| p.failures.iter().find(|f| f.file.is_some() && f.line.is_some())) {
            if let Some(s) = snippet_of(guard, &plan.cwd, &plan.root, first) {
                o.snippets.push(s);
            }
        }
        o.parsed = parsed.filter(|p| p.recognised);
    }
    Ok(o)
}

/// The profile as JSON, with what each command is to the terminal rules.
pub fn profile_json(profile: &ProjectProfile, base: &Path, plain: &dyn Fn(&CmdSpec) -> bool) -> Value {
    let spec_json = |kind: CheckKind, s: &CmdSpec| {
        json!({"kind": kind.as_str(), "ecosystem": s.eco.name(), "exe": s.exe, "args": s.args, "line": s.line(), "cwd": s.cwd_rel, "source": s.why, "timeout_s": s.timeout_s, "offline": s.offline, "package_required": s.package_required, "plain": plain(s)})
    };
    json!({
        "root": display(&profile.root),
        "asked_about": display(base),
        "languages": profile.languages(),
        "package_manager": profile.package_manager,
        "test_runner": profile.test_runner,
        "workspaces": profile.workspace_members,
        "checks": profile.all_specs().into_iter().map(|(k, s)| spec_json(k, s)).collect::<Vec<_>>(),
        "format": profile.format.iter().map(|s| s.line()).collect::<Vec<_>>(),
        "ci": profile.ci_files,
        "instruction_files": profile.instruction_files,
        "vcs": profile.vcs,
        "notes": profile.notes,
        "fingerprint": format!("{:016x}", profile.fingerprint),
    })
}

/// The text of `project.profile`: what the project is and the command of each kind, in a few lines.
pub fn profile_text(profile: &ProjectProfile) -> String {
    if profile.is_empty() {
        return format!("project {}: no manifest that is understood", display(&profile.root));
    }
    let mut out = format!("project {}: {}", profile.root.file_name().map_or_else(|| display(&profile.root), |n| n.to_string_lossy().into_owned()), profile.languages().join(", "));
    let mut bits: Vec<String> = Vec::new();
    if let Some(pm) = &profile.package_manager {
        bits.push(pm.clone());
    }
    if let Some(r) = &profile.test_runner {
        bits.push(r.clone());
    }
    if profile.workspace_members.len() > 1 {
        bits.push(format!("workspace of {}", profile.workspace_members.len()));
    }
    if !bits.is_empty() {
        out.push_str(&format!(" ({})", bits.join(", ")));
    }
    for kind in CheckKind::ALL {
        let lines: Vec<String> = profile.specs(kind).iter().map(|s| if profile.ecosystems.len() > 1 { format!("{} [{}]", s.line(), s.eco.name()) } else { s.line() }).collect();
        if !lines.is_empty() {
            out.push_str(&format!("\n{}: {}", kind.as_str(), lines.join("; ")));
        }
    }
    for note in profile.notes.iter().take(3) {
        out.push_str(&format!("\nnote: {}", parse::clip(note, 200)));
    }
    if !profile.ci_files.is_empty() {
        out.push_str(&format!("\nci: {}", profile.ci_files.iter().take(4).cloned().collect::<Vec<_>>().join(", ")));
    }
    out.push_str("\nrun one with check.run kind=<tests|build|lint|typecheck> (path=<folder or file>, filter=<name>, package=<crate>).");
    out
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod runner_tests;
