//! `project.profile` and `check.run`: one structured way to test, build, lint and type-check a project (card OA4-05; tasks WF-04 and WF-05 of the OpenCode audit).
//!
//! Before, the model had to guess the command, the folder, the flags and the time (`term.run` with `cargo test --offline` in an example of the prompt), and what came back was a log.
//!
//! * `project.profile {path?}` reads the manifests of the project that holds `path` (the first allowed folder when it is left out) and says its languages, its package manager, its test
//!   runner and the command of each kind of check. It runs nothing and is a read.
//! * `check.run {kind, path?, filter?, package?, timeout_s?, background?}` picks that command, runs it **as a `term.run`** (or `term.start`) of the terminal tools, and answers with a card:
//!   `check tests: FAILED 2 of 14 (cargo test --offline, 38 s)` and the failures as numbered lines with their file and line ([`card`], [`parse`]). So it is classified, asked about,
//!   timed, contained, kept as a job and audited exactly as the same command would be: asking once for the exact command in a trusted folder, every time anywhere else; a rule of
//!   `term_rules.json` and of `permissions.json` that refuses the command refuses the check. A check is no way round the terminal rules, and `cargo` runs with `--offline` (a failure that
//!   says a package is not downloaded is told as such and is never retried online).
//!
//! The commands come from [`crate::code::profile`]; they are suggestions that the terminal's rules judge, and nothing a manifest says is put in front of the model but names of scripts.
//!
//! [`diagnostics`] (card OA6-03, task CU-10) reads what the check that follows an edit printed and makes the errors of it a short block per file (`<diagnostics file="...">`), for the loop to
//! put under the result of the edit. It is pure text in, text out, and runs nothing.

pub mod card;
pub mod diagnostics;
pub mod parse;
pub mod runner;

use crate::code::CheckKind;
use crate::fs::guard::{display, PathGuard};
use crate::term::{rules, TermProvider};
use aqua_tool_host::{Annotations, Availability, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider};
use card::Outcome;
use runner::Planned;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

/// The two tools. It holds the terminal tools it runs its commands through (a clone of the provider the host has, which shares its jobs and rules) and the file where profiles are kept.
pub struct CheckProvider {
    guard: Arc<PathGuard>,
    term: TermProvider,
    cache: Option<PathBuf>,
    /// Are the tools on offer? Asked each time. Not set: they are (a test, or a caller that made the provider to use it).
    switch: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl CheckProvider {
    pub fn new(guard: Arc<PathGuard>, term: TermProvider, cache: Option<PathBuf>) -> Self {
        Self { guard, term, cache, switch: None }
    }

    /// The tools are on offer only while `on` says so (`AQUA_CHECK_TOOLS`: new behaviour is off until it is measured, card OA4-05). They stay registered, so that the reference and the status
    /// list them, but are unavailable, are not shown to the agent and cannot be called.
    pub fn with_switch(mut self, on: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        self.switch = Some(on);
        self
    }

    fn profile(&self, args: &Value) -> Result<ProviderOutput, ToolError> {
        let (profile, base) = runner::profile_for(&self.guard, args, self.cache.as_deref())?;
        let text = runner::profile_text(&profile);
        // What counts as a plain project check for the terminal rules: the commands the profile suggests are listed with it, so that the model and the person can see which ask once.
        let data = runner::profile_json(&profile, &base, &|s| rules::is_project_check(&s.exe, &s.args));
        Ok(ProviderOutput { data, text, ..Default::default() })
    }

    async fn run(&self, args: &Value, ctx: &ExecCtx) -> Result<ProviderOutput, ToolError> {
        let plan = match runner::plan(&self.guard, args, self.cache.as_deref())? {
            Planned::Run(plan) => *plan,
            Planned::NothingToRun { kind, root, why } => {
                let mut o = Outcome::new(kind, String::new(), String::new(), display(&root));
                o.not_run = Some(why);
                return Ok(ProviderOutput { data: o.data(), text: o.text(), ..Default::default() });
            }
        };
        let answer = self.term.call(plan.term_tool(), plan.term_args(), ctx).await;
        let outcome = match &answer {
            Ok(out) => runner::outcome_of(&self.guard, &plan, Ok(&out.data)),
            Err(e) => runner::outcome_of(&self.guard, &plan, Err(e)),
        }
        .map_err(|e| missing_toolchain(e, &plan.spec.exe))?;
        Ok(ProviderOutput { data: outcome.data(), text: outcome.text(), ..Default::default() }.marked_private())
    }
}

/// A program that is not there says which one: `TOOL_MISSING: cargo ...`.
fn missing_toolchain(e: ToolError, exe: &str) -> ToolError {
    if e.code == ErrorCode::NotFound {
        let mut e = e;
        e.message = format!("TOOL_MISSING: {exe} is not installed here, or not in a folder programs may be run from (path_dirs of data/tools/term.json). {}", e.message);
        return e;
    }
    e
}

fn manifest(name: &str, description: &str, classes: Vec<PermClass>, schema: Value, default_ms: u64, max_ms: u64, read_only: bool) -> ToolManifest {
    ToolManifest {
        name: name.into(),
        title: name.into(),
        description: description.into(),
        classes,
        default_decision: Decision::Auto,
        input_schema: schema,
        timeout_default_ms: default_ms,
        timeout_max_ms: max_ms,
        annotations: Annotations { read_only, destructive: !read_only, idempotent: read_only, open_world: !read_only },
        // What a manifest names and what a compiler prints is text from the project: it is data, never instructions.
        untrusted_output: true,
    }
}

impl ToolProvider for CheckProvider {
    fn id(&self) -> &str {
        "check"
    }

    fn availability(&self, _tool: &str) -> Availability {
        match &self.switch {
            Some(on) if !on() => Availability::Unavailable("the check tools are off (AQUA_CHECK_TOOLS=on turns them on)".into()),
            _ => Availability::Available,
        }
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        vec![
            manifest(
                "project.profile",
                "What the project is and how it is checked: its languages, test runner and the command for tests, build, lint and type check. Runs nothing.",
                vec![PermClass::Read],
                json!({"type": "object", "properties": {"path": {"type": "string", "maxLength": 1024, "description": "A file or folder of the project. Left out: the first allowed folder."}}}),
                20_000,
                30_000,
                true,
            ),
            manifest(
                "check.run",
                "Run the project's own tests, build, lint or type check and get a short list of what failed and where. It picks the command and folder.",
                vec![PermClass::Execute],
                json!({"type": "object", "properties": {
                    "kind": {"type": "string", "enum": ["tests", "build", "lint", "typecheck"], "description": "What to run."},
                    "path": {"type": "string", "maxLength": 1024, "description": "A file or folder of the project to check. Left out: the first allowed folder."},
                    "filter": {"type": "string", "maxLength": 120, "description": "Tests only: one test name or part of one, to run just that."},
                    "package": {"type": "string", "maxLength": 120, "description": "A Cargo workspace only: the crate to check."},
                    "timeout_s": {"type": "integer", "minimum": 1, "maximum": 3600, "description": "Stop it after this many seconds (300 at most, 3600 with background). Left out: 300; a build that is still running then is kept as a background job."},
                    "background": {"type": "boolean", "description": "Start it as a background job and answer at once with its job_id (read it with term.output, end it with term.stop). For a run that takes minutes."}
                }, "required": ["kind"]}),
                330_000,
                330_000,
                false,
            ),
        ]
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        match tool {
            "project.profile" => {
                let (base, _) = runner::base_of(&self.guard, args)?;
                Ok(Classification { classes: vec![PermClass::Read], target: display(&base), summary: format!("Read the project files of {} to say how it is checked", display(&base)), target_allowed: true })
            }
            "check.run" => match runner::plan(&self.guard, args, self.cache.as_deref())? {
                Planned::NothingToRun { kind, root, why } => Ok(Classification { classes: vec![PermClass::Read], target: display(&root), summary: format!("Check ({}): nothing to run: {why}", kind.as_str()), target_allowed: true }),
                Planned::Run(plan) => {
                    // The question is the terminal's own for this command, with the kind of check in front of it: it asks once for the exact command in a trusted folder, and every time anywhere else.
                    let mut c = self.term.classify(plan.term_tool(), &plan.term_args()).map_err(|e| missing_toolchain(e, &plan.spec.exe))?;
                    c.summary = format!("Check ({}): {}", plan.kind.as_str(), c.summary);
                    // The target is the whole command line, so that a rule on a command (`permissions.json`) sees what will run.
                    c.target = plan.line();
                    Ok(c)
                }
            },
            other => Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
        }
    }

    fn grant_scope(&self, tool: &str, args: &Value) -> Option<String> {
        if tool != "check.run" {
            return None;
        }
        match runner::plan(&self.guard, args, self.cache.as_deref()).ok()? {
            Planned::Run(plan) => self.term.grant_scope(plan.term_tool(), &plan.term_args()),
            Planned::NothingToRun { .. } => None,
        }
    }

    fn project_check(&self, tool: &str, args: &Value) -> Option<aqua_tool_host::ProjectCheck> {
        if tool != "check.run" {
            return None;
        }
        match runner::plan(&self.guard, args, self.cache.as_deref()).ok()? {
            Planned::Run(plan) => self.term.project_check(plan.term_tool(), &plan.term_args()),
            Planned::NothingToRun { .. } => None,
        }
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        Box::pin(async move {
            match tool {
                "project.profile" => self.profile(&args),
                "check.run" => self.run(&args, ctx).await,
                other => Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
            }
        })
    }
}

/// Which kinds a call may name: for the text of an error.
pub fn kinds() -> [&'static str; 4] {
    CheckKind::ALL.map(CheckKind::as_str)
}
