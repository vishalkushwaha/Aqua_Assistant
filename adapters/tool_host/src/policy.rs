//! Turns "this tool, with these classes, from this caller" into a decision.
//!
//! Decisions form a ladder (`Auto < Ask < AlwaysAsk < Deny`) and every rule can only move a
//! call up it. The order of rules is: hard deny, the tool's own default, the mode, then the
//! caller and taint overlays. A rule that cannot be evaluated resolves to the stricter side.

use crate::manifest::{is_valid_tool_name, Decision, PermClass, ToolManifest};
use crate::rules::RuleSet;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{PoisonError, RwLock};

/// How much the person wants to be asked about what Aqua does on its own (`data/tools/autonomy.json`, card R7-18). Ordered by how much it asks. A level can only move a
/// decision up the ladder (`Auto < Ask < AlwaysAsk < Deny`), never down, and it does not apply to a call the person makes themselves in the app ([`Caller::Ui`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskLevel {
    /// Today's rules, unchanged.
    #[default]
    AskRisky,
    /// Today's rules, and a call that reaches outside this computer (an effect on someone else's system, a request to an address on no list) asks a fresh question every
    /// time, also under full access.
    AskOutside,
    /// Every call that today's rules would let through unasked asks too, and what reaches outside asks every time.
    AskAll,
}

impl AskLevel {
    /// The levels in the order the card names them: ask for everything, for risky things, for what reaches outside.
    pub const ALL: [AskLevel; 3] = [AskLevel::AskAll, AskLevel::AskRisky, AskLevel::AskOutside];

    pub fn id(self) -> &'static str {
        match self {
            AskLevel::AskRisky => "ask_risky",
            AskLevel::AskOutside => "ask_outside",
            AskLevel::AskAll => "ask_all",
        }
    }

    /// The level a word names, or `None`: an unknown word is not a level.
    pub fn from_id(id: &str) -> Option<Self> {
        AskLevel::ALL.into_iter().find(|l| l.id() == id)
    }

    fn code(self) -> u8 {
        self as u8
    }

    fn from_code(code: u8) -> Self {
        match code {
            2 => AskLevel::AskAll,
            1 => AskLevel::AskOutside,
            _ => AskLevel::AskRisky,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Reads ask, everything else is refused. Used when the policy could not be loaded.
    Locked,
    Standard,
    /// Fewer prompts for local work. Destructive, privileged and external actions still always ask.
    Developer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Caller {
    Agent,
    Api,
    Ui,
    Voice,
    /// An external MCP client using Aqua as a server.
    McpClient,
    /// A stored recipe replayed without the planner.
    Replay,
}

impl Caller {
    pub fn as_str(self) -> &'static str {
        match self {
            Caller::Agent => "agent",
            Caller::Api => "api",
            Caller::Ui => "ui",
            Caller::Voice => "voice",
            Caller::McpClient => "mcp_client",
            Caller::Replay => "replay",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PolicyContext {
    pub caller: Caller,
    /// The run has already read text from outside Aqua (a web page, a file, a log line).
    pub tainted: bool,
    /// The run holds something private to the person (a file outside the project, a database row, what a remote machine printed, the
    /// clipboard). Only [`PolicyEngine::decide_caps`] looks at it.
    pub private_data: bool,
    /// The provider says the target is on its allow list (a trusted domain, an allowed root).
    pub target_allowed: bool,
}

/// How the capability-tracking rule ([`PolicyEngine::decide_caps`]) is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapsMode {
    /// Not looked at.
    Off,
    /// Looked at beside the rule that is applied, and a difference is written in the audit line (the default while it is being tried).
    Shadow,
    /// It is the rule that is applied.
    On,
}

impl CapsMode {
    /// `AQUA_CAPS=off|shadow|on` (default `shadow`).
    pub fn from_env() -> Self {
        Self::parse(std::env::var("AQUA_CAPS").ok().as_deref())
    }

    pub fn parse(v: Option<&str>) -> Self {
        match v.map(|x| x.trim().to_ascii_lowercase()).as_deref() {
            Some("on" | "1" | "true") => CapsMode::On,
            Some("off" | "0" | "false") => CapsMode::Off,
            _ => CapsMode::Shadow,
        }
    }
}

/// The tool that deletes a file or a folder (it moves it to the undo store). Named, because its classes are Write: the policy has no class for "removes something that can be put back".
fn is_file_delete(tool: &str) -> bool {
    tool == "fs.delete"
}

/// Does this call send something out of the computer, or act on another machine? A change to an outside system, a request to a host that is not
/// on the allow list (a GET with a file's contents in its query is how data leaves without a "change"), a form submitted, a file uploaded.
pub fn is_egress(classes: &[PermClass], target_allowed: bool) -> bool {
    classes.contains(&PermClass::ExternalEffect)
        || (classes.contains(&PermClass::Network) && !target_allowed)
        || (classes.contains(&PermClass::Remote) && classes.iter().any(|c| c.changes_state()))
}

/// What the host knows about a run's edit grant (card OA6-05, ED-10; see [`crate::run_grant`]) when it decides one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditGrantFacts {
    /// A grant of this run, not suspended, names the folder of every file the call would write.
    pub covers: bool,
    /// The run has read text from outside the project (the web, a page, a mail, a tool's result that is not known to be local, a file outside the project folder). Taint of this kind is never
    /// lifted, grant or not; taint from reading files inside the project (`ProviderOutput::local`) is the only kind a grant answers.
    pub external_taint: bool,
}

/// Could a run's edit grant ever answer this call? Only a call that the agent makes, that only changes the text of a file (`fs.edit`, `fs.write`, `fs.patch`), whose classes are
/// exactly `Write` (no `Execute` for a script or a build file, no `Credential` for a file of secrets, nothing destructive, no network), and whose target is inside a folder the provider
/// vouches for. Everything else asks as it always did. (A name the file tools never write, `.vscode` or `.cargo`, is refused when the call is classified: it never gets this far.)
pub fn edit_grant_possible(tool: &str, classes: &[PermClass], ctx: &PolicyContext) -> bool {
    ctx.caller == Caller::Agent && ctx.target_allowed && crate::run_grant::is_edit_tool(tool) && classes == [PermClass::Write]
}

/// Does the grant of the run lift the one rule that makes this call ask: "this run has read untrusted content and this changes something" (the taint rule of
/// [`PolicyEngine::decide`])? Only when the run is tainted by files of the project alone (`external_taint` is false), a grant covers the folder of the call, and the call is one a grant can
/// answer ([`edit_grant_possible`]). It lifts that rule and no other: the host decides the call again as for a clean run and accepts the result only if it is `Auto` (a hook, a rule of the
/// person's, the autonomy level, the tool's own override or a reason of the provider's to ask every time all still ask).
pub fn edit_grant_lifts_taint(tool: &str, classes: &[PermClass], ctx: &PolicyContext, facts: &EditGrantFacts) -> bool {
    ctx.tainted && facts.covers && !facts.external_taint && edit_grant_possible(tool, classes, ctx)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub decision: Decision,
    pub reason: String,
}

/// The policy file (`data/tools/policy.json`). Unknown fields are an error, so a typo cannot
/// silently drop a rule; a file that does not parse puts the host in locked mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyFile {
    #[serde(default = "default_mode")]
    pub mode: Mode,
    /// Brakes on runaway loops: calls per minute per tool, and calls running at once.
    #[serde(default)]
    pub limits: Limits,
    /// Tools refused in every mode: exact names, or `domain.*`.
    #[serde(default)]
    pub hard_deny: Vec<String>,
    /// Per-tool decisions. They can only tighten what the rules would decide, never loosen it.
    #[serde(default)]
    pub tool_overrides: BTreeMap<String, Decision>,
}

/// How fast tools may be called. A model stuck in a loop (or steered into one) hits these long
/// before it does damage, and gets a `RATE_LIMITED` answer that says how long to wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Calls per minute to any one tool that only reads.
    #[serde(default = "d_read")]
    pub reads_per_minute: u32,
    /// Calls per minute to any one tool that changes something.
    #[serde(default = "d_change")]
    pub changes_per_minute: u32,
    /// Tool calls running at the same moment, across all tools.
    #[serde(default = "d_conc")]
    pub max_concurrent: u32,
}

fn d_read() -> u32 {
    300
}
fn d_change() -> u32 {
    60
}
fn d_conc() -> u32 {
    16
}

impl Default for Limits {
    fn default() -> Self {
        Self { reads_per_minute: 300, changes_per_minute: 60, max_concurrent: 16 }
    }
}

impl Limits {
    pub fn check(&self) -> Result<(), String> {
        if !(1..=10_000).contains(&self.reads_per_minute) || !(1..=10_000).contains(&self.changes_per_minute) {
            return Err("limits: calls per minute must be 1-10000".into());
        }
        if !(1..=64).contains(&self.max_concurrent) {
            return Err("limits: max_concurrent must be 1-64".into());
        }
        Ok(())
    }
}

fn default_mode() -> Mode {
    Mode::Standard
}

impl Default for PolicyFile {
    fn default() -> Self {
        Self { mode: Mode::Standard, limits: Limits::default(), hard_deny: Vec::new(), tool_overrides: BTreeMap::new() }
    }
}

impl PolicyFile {
    pub fn parse(text: &str) -> Result<Self, String> {
        let file: PolicyFile = serde_json::from_str(text).map_err(|e| format!("policy file is not valid: {e}"))?;
        file.check()?;
        Ok(file)
    }

    pub fn check(&self) -> Result<(), String> {
        self.limits.check()?;
        for p in &self.hard_deny {
            let name = p.strip_suffix(".*").map(|d| format!("{d}.x")).unwrap_or_else(|| p.clone());
            if !is_valid_tool_name(&name) {
                return Err(format!("hard_deny entry '{p}' is not a tool name or 'domain.*'"));
            }
        }
        for name in self.tool_overrides.keys() {
            if !is_valid_tool_name(name) {
                return Err(format!("tool_overrides key '{name}' is not a tool name"));
            }
        }
        Ok(())
    }
}

pub struct PolicyEngine {
    /// Full access for the project (see `set_full_access`). A switch of its own, not a mode: a locked host stays locked.
    full_access: AtomicBool,
    /// The person's autonomy level (see [`AskLevel`]); it is not part of the policy file.
    ask_level: AtomicU8,
    mode: RwLock<Mode>,
    hard_deny: RwLock<Vec<String>>,
    overrides: RwLock<BTreeMap<String, Decision>>,
    limits: RwLock<Limits>,
    /// The person's pattern rules (`data/tools/permissions.json`, card OA8-04): they can only tighten what the rules above decided.
    rules: RwLock<std::sync::Arc<RuleSet>>,
}

/// What a spoken request may do: read, and look things up on the web. A spoken sentence can be a television or a stranger, so nothing that changes anything and nothing that
/// sends something out: the web tools it may use are `web.search` (the person's own search services; it may carry the key of the one that has a key), and `web.read` and
/// `web.fetch` for a target that is allowed (a page that a search of this run returned or the person named, or a host on the allow list). Any other network tool
/// (`http.request` with its headers and credentials, `web.scrape` that runs a page's script) and any network call that carries a credential, an effect or a write is not
/// the voice's.
///
/// The named list of the voice (decisions D-13 and D-14, card R6-08): the tools that only read, the three web tools above, and two on the desktop that the provider has judged
/// as the voice's own: `win.type` (text into the window in front, no Enter, no clicking first) and `win.app` (opening or switching to an app by name). The provider says "allowed"
/// for those only where the desktop guard lets a spoken request through (`target_allowed` with the one class `Write`); anywhere it would ask, or refuse, it says so in another
/// way and this list does not match. The clipboard is never the voice's, whatever its classes say: `clipboard.read` is a read that always asks, and a question put for a spoken
/// request is one a television can cause.
fn voice_may(tool: &str, classes: &[PermClass], target_allowed: bool) -> bool {
    if tool.starts_with("clipboard.") {
        return false;
    }
    if classes.iter().all(|c| *c == PermClass::Read) {
        return true;
    }
    match tool {
        "web.search" => classes.iter().all(|c| matches!(c, PermClass::Network | PermClass::Credential)),
        "web.read" | "web.fetch" => classes == [PermClass::Network] && target_allowed,
        "win.type" | "win.app" => classes == [PermClass::Write] && target_allowed,
        _ => false,
    }
}

impl PolicyEngine {
    pub fn new(mode: Mode) -> Self {
        Self {
            full_access: AtomicBool::new(false),
            ask_level: AtomicU8::new(AskLevel::AskRisky.code()),
            mode: RwLock::new(mode),
            hard_deny: RwLock::new(Vec::new()),
            overrides: RwLock::new(BTreeMap::new()),
            limits: RwLock::new(Limits::default()),
            rules: RwLock::new(std::sync::Arc::new(RuleSet::none())),
        }
    }

    /// Install the person's pattern rules (`data/tools/permissions.json`, see [`crate::rules`]). Without a call there are none.
    pub fn set_rules(&self, rules: RuleSet) {
        *self.rules.write().unwrap_or_else(PoisonError::into_inner) = std::sync::Arc::new(rules);
    }

    /// The pattern rules in force.
    pub fn rules(&self) -> std::sync::Arc<RuleSet> {
        self.rules.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// `verdict` as the person's pattern rules leave it: the strictest rule that matches the call (by its tool, its target, its address and its command line) can move it up the
    /// ladder (`Auto < Ask < AlwaysAsk < Deny`) and nothing can move it down. A refusal stays the refusal it was, with its own reason. Applied after the policy, so after the hard-deny
    /// list; it holds under full access, in every mode and for every caller.
    pub fn apply_rules(&self, tool: &str, target: &str, args: &serde_json::Value, classes: &[PermClass], verdict: Verdict) -> Verdict {
        let rules = self.rules();
        if rules.is_empty() || verdict.decision == Decision::Deny {
            return verdict;
        }
        let subjects = crate::rules::subjects_of(target, args);
        match rules.judge(tool, &subjects, classes.iter().any(|c| c.changes_state())) {
            Some(hit) if hit.effect.decision() > verdict.decision => Verdict { decision: hit.effect.decision(), reason: hit.why },
            _ => verdict,
        }
    }

    /// The autonomy level in force. A level only makes a decision stricter (see [`AskLevel`]); the default is today's rules.
    pub fn ask_level(&self) -> AskLevel {
        AskLevel::from_code(self.ask_level.load(Ordering::SeqCst))
    }

    pub fn set_ask_level(&self, level: AskLevel) {
        self.ask_level.store(level.code(), Ordering::SeqCst);
    }

    /// "Full access" for the project: the person has said Aqua may work in the project without being asked at every step.
    /// Ordinary work (reads, writes, running programs, the network) runs without a prompt. What it never skips: privileged and
    /// external actions (always ask), destructive actions outside the project (always ask) and inside it (ask once per session),
    /// secrets, other machines, the hard-deny list, per-tool overrides, the caller rules (voice reads only, recipes never change
    /// anything) and the rule that a run which has read untrusted text needs a fresh answer to change anything. A locked host
    /// (a policy file that could not be read) ignores the switch: it never unlocks anything.
    pub fn set_full_access(&self, on: bool) {
        self.full_access.store(on, Ordering::SeqCst);
    }

    pub fn full_access(&self) -> bool {
        self.full_access.load(Ordering::SeqCst)
    }

    pub fn limits(&self) -> Limits {
        *self.limits.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set_limits(&self, limits: Limits) {
        *self.limits.write().unwrap_or_else(PoisonError::into_inner) = limits;
    }

    /// Build an engine from the policy file text. Text that is unreadable or invalid gives a
    /// locked engine and the reason, never a permissive default. `None` (no file) is standard.
    pub fn from_file_text(text: Option<&str>) -> (Self, Option<String>) {
        match text.map(PolicyFile::parse) {
            None => (Self::new(Mode::Standard), None),
            Some(Ok(file)) => {
                let e = Self::new(file.mode);
                e.apply(&file);
                (e, None)
            }
            Some(Err(reason)) => (Self::locked(), Some(reason)),
        }
    }

    pub fn apply(&self, file: &PolicyFile) {
        self.set_mode(file.mode);
        self.set_hard_deny(file.hard_deny.clone());
        self.set_limits(file.limits);
        *self.overrides.write().unwrap_or_else(PoisonError::into_inner) = file.tool_overrides.clone();
    }

    pub fn snapshot(&self) -> PolicyFile {
        PolicyFile {
            mode: self.mode(),
            limits: self.limits(),
            hard_deny: self.hard_deny.read().unwrap_or_else(PoisonError::into_inner).clone(),
            tool_overrides: self.overrides.read().unwrap_or_else(PoisonError::into_inner).clone(),
        }
    }

    /// The engine used when the policy file is missing, unreadable or malformed.
    pub fn locked() -> Self {
        Self::new(Mode::Locked)
    }

    pub fn mode(&self) -> Mode {
        *self.mode.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set_mode(&self, mode: Mode) {
        *self.mode.write().unwrap_or_else(PoisonError::into_inner) = mode;
    }

    /// Tool names that are refused whatever the mode. `git.*` matches a whole domain.
    pub fn set_hard_deny(&self, patterns: Vec<String>) {
        *self.hard_deny.write().unwrap_or_else(PoisonError::into_inner) = patterns;
    }

    fn hard_denied(&self, tool: &str) -> bool {
        self.hard_deny
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|p| match p.strip_suffix(".*") {
                Some(domain) => tool.split('.').next() == Some(domain),
                None => p == tool,
            })
    }

    /// The decision by what the run holds, not by one flag. The ordinary rules (the mode, the tool's own default, the caller, full access, the
    /// hard floors) decide as they do for a clean run, and then:
    ///
    /// * untrusted content **and** private data in one run: anything that changes something, sends something out or uses a secret asks every time;
    /// * untrusted content alone: sending something out (a form, an upload, a request to a host that is not allowed) asks, and so does using a
    ///   secret; reads, same-site navigation and changes the ordinary rules allow (a write inside the trusted project) go on;
    /// * private data alone: sending something out asks;
    /// * neither: the ordinary rules.
    ///
    /// What is never loosened: a refusal, "always ask" for destructive, privileged and external actions, the voice and replay limits, and the
    /// rule that untrusted content cannot write the user's memory.
    pub fn decide_caps(&self, manifest: &ToolManifest, classes: &[PermClass], ctx: &PolicyContext) -> Verdict {
        let clean = PolicyContext { tainted: false, private_data: false, ..*ctx };
        let mut acc = self.decide(manifest, classes, &clean);
        if acc.decision == Decision::Deny {
            return acc;
        }
        let changes = classes.iter().any(|c| c.changes_state());
        let egress = is_egress(classes, ctx.target_allowed);
        let credential = classes.contains(&PermClass::Credential);
        let tighten = |acc: &mut Verdict, to: Decision, why: &str| {
            if to > acc.decision {
                acc.decision = to;
                acc.reason = why.to_string();
            }
        };
        if ctx.tainted && manifest.name.starts_with("memory.") && changes {
            tighten(&mut acc, Decision::Deny, "this run has read untrusted content, and untrusted content may not write to the user's memory");
        }
        match (ctx.tainted, ctx.private_data) {
            (true, true) if changes || egress || credential => {
                tighten(&mut acc, Decision::AlwaysAsk, "this run has read untrusted content and holds private data: anything that changes or sends something asks every time")
            }
            (true, false) => {
                if credential {
                    tighten(&mut acc, Decision::AlwaysAsk, "this run has read untrusted content and this uses a stored secret");
                } else if egress {
                    tighten(&mut acc, Decision::Ask, "this run has read untrusted content and this sends something out");
                }
            }
            (false, true) if egress => tighten(&mut acc, Decision::Ask, "this run holds private data and this sends something out"),
            _ => {}
        }
        acc
    }

    /// The decision the one door will apply to a browser or desktop call (card R6-08): the ordinary rule and the capability rule, whichever is stricter, and a call
    /// that sends something out (`sends_out`: a form submitted, Enter pressed, a page nobody listed opened) held to what the run has read and holds. A run that read
    /// outside text asks every time before it sends; a run that holds private data (and read nothing untrusted) is asked.
    ///
    /// It is **never looser than [`decide`](Self::decide)**: the door does not depend on `AQUA_CAPS=on` to be as strict as the old routes are, and `AQUA_CAPS=on` alone is
    /// looser than `decide` for a change after untrusted text. The host applies it to the browser's and the desktop's calls when the request says the door is on
    /// (`RequestContext::door`, `ToolHost::verdict_for`); otherwise only the one-door comparison reads it (`ToolHost::preview`).
    pub fn decide_door(&self, manifest: &ToolManifest, classes: &[PermClass], applied: &PolicyContext, caps: &PolicyContext, sends_out: bool) -> Verdict {
        let mut acc = self.decide(manifest, classes, applied);
        if acc.decision == Decision::Deny {
            return acc;
        }
        let by_caps = self.decide_caps(manifest, classes, caps);
        if by_caps.decision > acc.decision {
            acc = by_caps;
        }
        if sends_out {
            let to = if applied.tainted || caps.tainted {
                Some((Decision::AlwaysAsk, "this run has read untrusted content and this sends something out"))
            } else if caps.private_data {
                Some((Decision::Ask, "this run holds private data and this sends something out"))
            } else {
                None
            };
            if let Some((decision, why)) = to.filter(|(d, _)| *d > acc.decision) {
                acc = Verdict { decision, reason: why.to_string() };
            }
        }
        acc
    }

    pub fn decide(&self, manifest: &ToolManifest, classes: &[PermClass], ctx: &PolicyContext) -> Verdict {
        if self.hard_denied(&manifest.name) {
            return Verdict { decision: Decision::Deny, reason: "this tool is on the hard-deny list".into() };
        }
        let mode = self.mode();
        // An outside program using Aqua as a server is not the person who switched this on: it gets no part of it. Neither does a call made with the API token (a script, another
        // program of the same user, or Aqua's own lookup that borrows that caller): the switch is the person's, made in the app, and the token cannot make it, so it cannot use it either.
        let full = self.full_access() && mode != Mode::Locked && !matches!(ctx.caller, Caller::McpClient | Caller::Api);
        let changes = classes.iter().any(|c| c.changes_state());
        let only_reads = classes.iter().all(|c| *c == PermClass::Read);

        let mut acc = Verdict { decision: manifest.default_decision, reason: String::from("the tool's default") };
        let tighten = |acc: &mut Verdict, to: Decision, why: &str| {
            if to > acc.decision {
                acc.decision = to;
                acc.reason = why.to_string();
            }
        };

        // Mode.
        match mode {
            Mode::Locked => {
                if only_reads {
                    tighten(&mut acc, Decision::Ask, "locked mode: every call asks");
                } else {
                    tighten(&mut acc, Decision::Deny, "locked mode: only reads are possible");
                }
            }
            Mode::Standard | Mode::Developer if full => {
                if classes.iter().any(|c| matches!(c, PermClass::Privileged | PermClass::ExternalEffect)) {
                    tighten(&mut acc, Decision::AlwaysAsk, "privileged or external action: full access never skips this");
                } else if classes.contains(&PermClass::Destructive) {
                    if ctx.target_allowed {
                        tighten(&mut acc, Decision::Ask, "destructive action in the project: asks once, then is remembered for this session");
                    } else {
                        tighten(&mut acc, Decision::AlwaysAsk, "destructive action outside the project");
                    }
                } else if is_file_delete(&manifest.name) {
                    // `fs.delete` is a Write (what it deletes goes to the undo store), so the arm above never saw it; `FULL_ACCESS.md` and the page of the switch say that deleting in the
                    // project asks once and outside it always (card OA1-04, SE-14). Only under full access: in the ordinary modes a delete in a trusted folder is as it was.
                    if ctx.target_allowed {
                        tighten(&mut acc, Decision::Ask, "deleting in the project: asks once, then is remembered for this session");
                    } else {
                        tighten(&mut acc, Decision::AlwaysAsk, "deleting outside the project");
                    }
                } else if classes.contains(&PermClass::Credential) {
                    tighten(&mut acc, Decision::Ask, "uses a stored secret");
                } else if classes.contains(&PermClass::Remote) {
                    tighten(&mut acc, Decision::Ask, "reaches another machine");
                }
            }
            Mode::Standard => {
                if classes.iter().any(|c| c.always_asks()) {
                    tighten(&mut acc, Decision::AlwaysAsk, "destructive, privileged or external action");
                } else if classes.iter().any(|c| matches!(c, PermClass::Execute | PermClass::Remote | PermClass::Credential))
                    || (classes.contains(&PermClass::Write) && !ctx.target_allowed)
                {
                    // A write to a target the provider vouches for (the workspace, the UI the user
                    // is driving) runs; a write anywhere else asks.
                    tighten(&mut acc, Decision::Ask, "changes state, runs code, reaches another machine or uses a secret");
                } else if classes.contains(&PermClass::Network) && !ctx.target_allowed {
                    tighten(&mut acc, Decision::Ask, "network target is not on the allow list");
                }
            }
            Mode::Developer => {
                if classes.iter().any(|c| c.always_asks()) {
                    tighten(&mut acc, Decision::AlwaysAsk, "destructive, privileged or external action");
                } else if classes.contains(&PermClass::Credential) {
                    tighten(&mut acc, Decision::Ask, "uses a stored secret");
                } else if classes.contains(&PermClass::Network) && !ctx.target_allowed {
                    tighten(&mut acc, Decision::Ask, "network target is not on the allow list");
                }
            }
        }

        // Per-tool override from the policy file (tightens only).
        if let Some(d) = self.overrides.read().unwrap_or_else(PoisonError::into_inner).get(&manifest.name) {
            tighten(&mut acc, *d, "the policy file sets this tool's decision");
        }

        // Caller overlays.
        match ctx.caller {
            Caller::Voice if !voice_may(&manifest.name, classes, ctx.target_allowed) => tighten(&mut acc, Decision::Deny, "a spoken request can only read, look things up on the web, type text into the window in front and open an app by name"),
            Caller::Replay if changes || classes.contains(&PermClass::Remote) => tighten(&mut acc, Decision::Deny, "a replayed recipe may not change anything or reach another machine"),
            // An external client cannot keep a grant, so every prompt is a fresh one.
            Caller::McpClient if acc.decision >= Decision::Ask => tighten(&mut acc, Decision::AlwaysAsk, "external MCP clients get no lasting grants"),
            // Nothing shows that a call made with the API token is the person's own request, so a change made with it is asked like one made after untrusted text (the host also treats such a
            // call as tainted: `ToolHost::contexts`). The reason says what is true; "this run has read untrusted content" would not be.
            Caller::Api if changes || classes.contains(&PermClass::Credential) => tighten(&mut acc, Decision::AlwaysAsk, "this call was made with the API token (a script or another program), so a change asks every time, as it does after untrusted text"),
            _ => {}
        }

        // Taint: text from outside plus a change of state needs a fresh answer, whatever the mode.
        if ctx.tainted && (changes || classes.contains(&PermClass::Credential)) {
            tighten(&mut acc, Decision::AlwaysAsk, "this run has read untrusted content");
        }
        // Memory is the person's: a run that has read untrusted text (a page, a file, a log) may not write to it at all, not even with a fresh
        // answer. An instruction planted in a page that Aqua "remembers" would speak in every later prompt.
        if ctx.tainted && manifest.name.starts_with("memory.") && changes {
            tighten(&mut acc, Decision::Deny, "this run has read untrusted content, and untrusted content may not write to the user's memory");
        }
        // Taint and the network: a request to a target that is not on the allow list is how data leaves without a "change" (a
        // GET with a file's contents in the query string). It asks every time in every mode, full access included, which has no
        // rule of its own for the network. A read from an allowed target (a weather service, documentation) stays automatic.
        if ctx.tainted && classes.contains(&PermClass::Network) && !ctx.target_allowed {
            tighten(&mut acc, Decision::AlwaysAsk, "this run has read untrusted content and the request goes to a target that is not allowed");
        }

        // The person's autonomy level (card R7-18). It only asks more, and not about what the person does themselves in the app. What reaches outside (an effect on
        // someone else's system, a request to an address on no list) asks a fresh question every time, also under full access, which has no rule of its own for the
        // network; and at the strictest level a call that would have run unasked asks too.
        if ctx.caller != Caller::Ui {
            let level = self.ask_level();
            let reaches_outside = classes.contains(&PermClass::ExternalEffect) || (classes.contains(&PermClass::Network) && !ctx.target_allowed);
            if level >= AskLevel::AskOutside && reaches_outside {
                tighten(&mut acc, Decision::AlwaysAsk, "autonomy level: anything that reaches outside this computer asks every time");
            }
            if level == AskLevel::AskAll && acc.decision == Decision::Auto {
                tighten(&mut acc, Decision::Ask, "autonomy level \"ask for everything\": every call asks");
            }
        }

        acc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Annotations;
    use serde_json::json;

    fn manifest(name: &str, classes: &[PermClass], default: Decision) -> ToolManifest {
        ToolManifest {
            name: name.into(),
            title: "t".into(),
            description: "d".into(),
            classes: classes.to_vec(),
            default_decision: default,
            input_schema: json!({"type": "object"}),
            timeout_default_ms: 1000,
            timeout_max_ms: 2000,
            annotations: Annotations::default(),
            untrusted_output: false,
        }
    }

    fn ctx(caller: Caller) -> PolicyContext {
        PolicyContext { caller, tainted: false, target_allowed: false, private_data: false }
    }

    fn decide(mode: Mode, classes: &[PermClass], default: Decision, c: PolicyContext) -> Decision {
        PolicyEngine::new(mode).decide(&manifest("x.y", classes, default), classes, &c).decision
    }

    use PermClass::*;

    #[test]
    fn standard_mode_follows_the_conventions() {
        let c = ctx(Caller::Agent);
        assert_eq!(decide(Mode::Standard, &[Read], Decision::Auto, c), Decision::Auto);
        assert_eq!(decide(Mode::Standard, &[Write], Decision::Auto, c), Decision::Ask);
        assert_eq!(decide(Mode::Standard, &[Execute], Decision::Auto, c), Decision::Ask);
        assert_eq!(decide(Mode::Standard, &[Remote], Decision::Auto, c), Decision::Ask);
        assert_eq!(decide(Mode::Standard, &[Credential], Decision::Auto, c), Decision::Ask);
        for always in [Destructive, Privileged, ExternalEffect] {
            assert_eq!(decide(Mode::Standard, &[Write, always], Decision::Auto, c), Decision::AlwaysAsk, "{always:?}");
        }
    }

    #[test]
    fn a_write_to_an_allowed_target_runs_and_to_any_other_asks() {
        let allowed = PolicyContext { target_allowed: true, ..ctx(Caller::Agent) };
        let other = PolicyContext { target_allowed: false, ..ctx(Caller::Agent) };
        assert_eq!(decide(Mode::Standard, &[Write], Decision::Auto, allowed), Decision::Auto);
        assert_eq!(decide(Mode::Standard, &[Write], Decision::Auto, other), Decision::Ask);
        // Only writes are relaxed: running code, remote hosts and secrets still ask.
        assert_eq!(decide(Mode::Standard, &[Write, Execute], Decision::Auto, allowed), Decision::Ask);
        assert_eq!(decide(Mode::Standard, &[Write, Destructive], Decision::Auto, allowed), Decision::AlwaysAsk);
        // Taint still overrides a trusted target.
        let tainted = PolicyContext { tainted: true, ..allowed };
        assert_eq!(decide(Mode::Standard, &[Write], Decision::Auto, tainted), Decision::AlwaysAsk);
    }

    /// One row of the decision table: who, what, what the run holds, and what the capability rule says.
    struct Row {
        label: &'static str,
        name: &'static str,
        mode: Mode,
        full: bool,
        classes: &'static [PermClass],
        caller: Caller,
        tainted: bool,
        private: bool,
        allowed: bool,
        want: Decision,
        /// This row is one where the capability rule is meant to be looser than the one that says "any change after any read asks every time".
        loosens: bool,
    }

    const fn row(label: &'static str, mode: Mode, classes: &'static [PermClass], tainted: bool, private: bool, allowed: bool, want: Decision, loosens: bool) -> Row {
        Row { label, name: "x.y", mode, full: false, classes, caller: Caller::Agent, tainted, private, allowed, want, loosens }
    }

    const STD: Mode = Mode::Standard;
    use Decision::{Auto as A, AlwaysAsk as AA, Ask as K, Deny as D};

    /// The table. Each line is a decision somebody could be surprised by.
    fn table() -> Vec<Row> {
        let mut t = vec![
            // reads: never held back by what the run holds
            row("a read, a clean run", STD, &[Read], false, false, true, A, false),
            row("a read after untrusted text", STD, &[Read], true, false, true, A, false),
            row("a read holding private data", STD, &[Read], false, true, true, A, false),
            row("a read with both", STD, &[Read], true, true, true, A, false),
            row("a read of an unlisted place after untrusted text", STD, &[Read], true, false, false, A, false),
            // a change inside the trusted project
            row("a write to the project, a clean run", STD, &[Write], false, false, true, A, false),
            row("a write to the project after untrusted text", STD, &[Write], true, false, true, A, true),
            row("a write to the project holding private data", STD, &[Write], false, true, true, A, false),
            row("a write to the project with both", STD, &[Write], true, true, true, AA, false),
            // a change somewhere else
            row("a write elsewhere, a clean run", STD, &[Write], false, false, false, K, false),
            row("a write elsewhere after untrusted text", STD, &[Write], true, false, false, K, true),
            row("a write elsewhere holding private data", STD, &[Write], false, true, false, K, false),
            row("a write elsewhere with both", STD, &[Write], true, true, false, AA, false),
            // running code
            row("running code, a clean run", STD, &[Execute], false, false, true, K, false),
            row("running code after untrusted text", STD, &[Execute], true, false, true, K, true),
            row("running code holding private data", STD, &[Execute], false, true, true, K, false),
            row("running code with both", STD, &[Execute], true, true, true, AA, false),
            row("write and run, after untrusted text", STD, &[Write, Execute], true, false, true, K, true),
            // the network
            row("a read of an allowed site, a clean run", STD, &[Network], false, false, true, A, false),
            row("a read of an allowed site after untrusted text", STD, &[Network], true, false, true, A, false),
            row("a read of an allowed site with both", STD, &[Network], true, true, true, A, false),
            row("a request to an unlisted host, a clean run", STD, &[Network], false, false, false, K, false),
            row("a request to an unlisted host after untrusted text", STD, &[Network], true, false, false, K, true),
            row("a request to an unlisted host holding private data", STD, &[Network], false, true, false, K, false),
            row("a request to an unlisted host with both", STD, &[Network], true, true, false, AA, false),
            row("a download to the project after untrusted text", STD, &[Network, Write], true, false, true, A, true),
            // secrets
            row("using a secret, a clean run", STD, &[Credential], false, false, true, K, false),
            row("using a secret after untrusted text", STD, &[Credential], true, false, true, AA, false),
            row("using a secret holding private data", STD, &[Credential], false, true, true, K, false),
            row("using a secret with both", STD, &[Credential], true, true, true, AA, false),
            // the hard floors: never loosened
            row("something destructive, a clean run", STD, &[Write, Destructive], false, false, true, AA, false),
            row("something destructive after untrusted text", STD, &[Write, Destructive], true, false, true, AA, false),
            row("something privileged after untrusted text", STD, &[Execute, Privileged], true, false, true, AA, false),
            row("an effect on someone else's system, a clean run", STD, &[Network, ExternalEffect], false, false, true, AA, false),
            row("an effect on someone else's system after untrusted text", STD, &[Network, ExternalEffect], true, false, true, AA, false),
            row("an effect on someone else's system with both", STD, &[Network, ExternalEffect], true, true, true, AA, false),
            // another machine
            row("a remote read, a clean run", STD, &[Remote], false, false, true, K, false),
            row("a remote read with both", STD, &[Remote], true, true, true, K, false),
            row("a remote change after untrusted text", STD, &[Remote, Write], true, false, true, K, true),
            row("a remote change with both", STD, &[Remote, Write], true, true, true, AA, false),
            // developer mode relaxes local work, not what leaves
            row("developer: a write, clean", Mode::Developer, &[Write], false, false, false, A, false),
            row("developer: a write after untrusted text", Mode::Developer, &[Write], true, false, false, A, true),
            row("developer: running code after untrusted text", Mode::Developer, &[Execute], true, false, true, A, true),
            row("developer: running code with both", Mode::Developer, &[Execute], true, true, true, AA, false),
            row("developer: an unlisted host after untrusted text", Mode::Developer, &[Network], true, false, false, K, true),
            row("developer: something destructive", Mode::Developer, &[Write, Destructive], false, false, true, AA, false),
            row("developer: a secret after untrusted text", Mode::Developer, &[Credential], true, false, true, AA, false),
            // locked: only reads, and every one asks
            row("locked: a read", Mode::Locked, &[Read], false, false, true, K, false),
            row("locked: a read after untrusted text", Mode::Locked, &[Read], true, false, true, K, false),
            row("locked: a write", Mode::Locked, &[Write], false, false, true, D, false),
            row("locked: a write after untrusted text", Mode::Locked, &[Write], true, false, true, D, false),
        ];
        // full access for the project
        for (label, classes, tainted, private, allowed, want, loosens) in [
            ("full access: a write to the project, a clean run", &[Write][..], false, false, true, A, false),
            ("full access: a write after untrusted text", &[Write][..], true, false, true, A, true),
            ("full access: a write with both", &[Write][..], true, true, true, AA, false),
            ("full access: running code after untrusted text", &[Execute][..], true, false, true, A, true),
            ("full access: an unlisted host after untrusted text", &[Network][..], true, false, false, K, true),
            ("full access: a secret after untrusted text", &[Credential][..], true, false, true, AA, false),
        ] {
            t.push(Row { label, name: "x.y", mode: STD, full: true, classes, caller: Caller::Agent, tainted, private, allowed, want, loosens });
        }
        // who is asking: the caller limits are not touched by what the run holds
        for (label, name, classes, caller, want) in [
            ("voice reads", "x.y", &[Read][..], Caller::Voice, A),
            ("voice cannot write", "x.y", &[Write][..], Caller::Voice, D),
            ("voice cannot run code", "x.y", &[Execute][..], Caller::Voice, D),
            ("voice cannot read the clipboard, though it only reads and always asks the agent", "clipboard.read", &[Read][..], Caller::Voice, D),
            ("voice cannot write the clipboard", "clipboard.write", &[Write][..], Caller::Voice, D),
            ("a replayed recipe cannot write", "x.y", &[Write][..], Caller::Replay, D),
            ("a replayed recipe cannot reach another machine", "x.y", &[Remote][..], Caller::Replay, D),
            ("an outside client's change always asks", "x.y", &[Write][..], Caller::McpClient, AA),
            ("an outside client reads freely", "x.y", &[Read][..], Caller::McpClient, A),
        ] {
            t.push(Row { label, name, mode: STD, full: false, classes, caller, tainted: true, private: true, allowed: false, want, loosens: false });
        }
        // the user's memory
        for (label, name, classes, tainted, private, want) in [
            ("memory: a write, a clean run, asks the person", "memory.remember", &[Write][..], false, false, K),
            ("memory: a write after untrusted text is refused", "memory.remember", &[Write][..], true, false, D),
            ("memory: a write with both is refused", "memory.forget", &[Write][..], true, true, D),
            ("memory: a read after untrusted text is allowed", "memory.search", &[Read][..], true, true, A),
        ] {
            t.push(Row { label, name, mode: STD, full: false, classes, caller: Caller::Agent, tainted, private, allowed: false, want, loosens: false });
        }
        t
    }

    fn decide_row(r: &Row, caps: bool) -> Decision {
        let engine = PolicyEngine::new(r.mode);
        engine.set_full_access(r.full);
        let m = manifest(r.name, r.classes, Decision::Auto);
        let ctx = PolicyContext { caller: r.caller, tainted: r.tainted, private_data: r.private, target_allowed: r.allowed };
        if caps {
            engine.decide_caps(&m, r.classes, &ctx).decision
        } else {
            engine.decide(&m, r.classes, &ctx).decision
        }
    }

    #[test]
    fn the_capability_rule_decides_every_row_of_the_table_as_written() {
        let rows = table();
        assert!(rows.len() >= 60, "the table has {} rows; it must have at least 60", rows.len());
        let wrong: Vec<String> = rows.iter().filter_map(|r| {
            let got = decide_row(r, true);
            (got != r.want).then(|| format!("{}: wanted {:?}, got {:?}", r.label, r.want, got))
        }).collect();
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn it_is_looser_than_the_rule_it_may_replace_only_where_the_table_says_so_and_never_below_a_floor() {
        for r in table() {
            let (old, new) = (decide_row(&r, false), decide_row(&r, true));
            if new < old {
                assert!(r.loosens, "{}: the capability rule ({new:?}) is looser than the ordinary one ({old:?}) and the row does not say it is meant to be", r.label);
                // Only a run that read untrusted text and holds nothing private, and never for something that can only ever be asked about.
                assert!(r.tainted && !r.private, "{}: looser for a run that holds private data", r.label);
                assert!(!r.classes.iter().any(|c| c.always_asks()), "{}: looser than a floor", r.label);
            }
            if r.loosens {
                assert!(new < old, "{}: marked as looser but is not ({new:?} vs {old:?})", r.label);
            }
            // The floors hold in every row: destructive, privileged and external actions always ask (or are refused).
            if r.classes.iter().any(|c| c.always_asks()) && r.caller == Caller::Agent && r.mode != Mode::Locked {
                assert!(new >= AA, "{}: a floor was lowered to {new:?}", r.label);
            }
        }
    }

    #[test]
    fn a_clean_run_is_decided_exactly_as_before_by_both_rules() {
        for r in table() {
            let clean = Row { tainted: false, private: false, ..r };
            assert_eq!(decide_row(&clean, true), decide_row(&clean, false), "{}", clean.label);
        }
    }

    #[test]
    fn a_hard_denied_tool_is_refused_by_both_rules_whatever_the_run_holds() {
        let engine = PolicyEngine::new(Mode::Developer);
        engine.set_hard_deny(vec!["x.*".into()]);
        let m = manifest("x.y", &[Read], Decision::Auto);
        for (t, p) in [(false, false), (true, false), (false, true), (true, true)] {
            let ctx = PolicyContext { caller: Caller::Agent, tainted: t, private_data: p, target_allowed: true };
            assert_eq!(engine.decide(&m, &[Read], &ctx).decision, Decision::Deny);
            assert_eq!(engine.decide_caps(&m, &[Read], &ctx).decision, Decision::Deny);
        }
    }

    #[test]
    fn what_counts_as_sending_something_out() {
        assert!(is_egress(&[ExternalEffect], true), "an effect on someone else's system");
        assert!(is_egress(&[Network], false), "a request to a host that is not allowed");
        assert!(!is_egress(&[Network], true), "a read from an allowed one");
        assert!(is_egress(&[Remote, Write], true) && is_egress(&[Remote, Execute], false));
        assert!(!is_egress(&[Remote], true), "looking at a remote machine is not sending anything");
        assert!(!is_egress(&[Write], false) && !is_egress(&[Read], false) && !is_egress(&[], false));
    }

    #[test]
    fn the_mode_is_read_from_a_word_and_defaults_to_looking_without_applying() {
        assert_eq!(CapsMode::parse(None), CapsMode::Shadow);
        assert_eq!(CapsMode::parse(Some("garbage")), CapsMode::Shadow);
        assert_eq!(CapsMode::parse(Some("on")), CapsMode::On);
        assert_eq!(CapsMode::parse(Some(" OFF ")), CapsMode::Off);
    }

    #[test]
    fn a_run_that_read_untrusted_text_cannot_write_the_users_memory_but_can_read_it() {
        let write = manifest("memory.remember", &[Write], Decision::Auto);
        let read = manifest("memory.search", &[Read], Decision::Auto);
        let engine = PolicyEngine::new(Mode::Standard);
        let clean = PolicyContext { target_allowed: false, ..ctx(Caller::Agent) };
        let tainted = PolicyContext { tainted: true, ..clean };
        assert_eq!(engine.decide(&write, &[Write], &clean).decision, Decision::Ask, "a clean run asks the person every time");
        assert_eq!(engine.decide(&write, &[Write], &tainted).decision, Decision::Deny, "a tainted run cannot, whatever the answer would be");
        assert_eq!(engine.decide(&read, &[Read], &tainted).decision, Decision::Auto, "reading is allowed");
        // Developer mode and full access do not change it.
        let dev = PolicyEngine::new(Mode::Developer);
        assert_eq!(dev.decide(&write, &[Write], &tainted).decision, Decision::Deny);
        let full = PolicyEngine::new(Mode::Standard);
        full.set_full_access(true);
        assert_eq!(full.decide(&write, &[Write], &tainted).decision, Decision::Deny);
        // Another tool with the same classes is not affected by this rule (it asks, as before).
        assert_eq!(engine.decide(&manifest("fs.write", &[Write], Decision::Auto), &[Write], &tainted).decision, Decision::AlwaysAsk);
    }

    #[test]
    fn network_asks_only_when_the_target_is_not_allowed() {
        let allowed = PolicyContext { target_allowed: true, ..ctx(Caller::Agent) };
        let unknown = PolicyContext { target_allowed: false, ..ctx(Caller::Agent) };
        assert_eq!(decide(Mode::Standard, &[Read, Network], Decision::Auto, allowed), Decision::Auto);
        assert_eq!(decide(Mode::Standard, &[Read, Network], Decision::Auto, unknown), Decision::Ask);
    }

    #[test]
    fn developer_mode_relaxes_local_work_but_never_the_dangerous_classes() {
        let c = ctx(Caller::Agent);
        assert_eq!(decide(Mode::Developer, &[Write, Execute], Decision::Auto, c), Decision::Auto);
        assert_eq!(decide(Mode::Developer, &[Write, Destructive], Decision::Auto, c), Decision::AlwaysAsk);
        assert_eq!(decide(Mode::Developer, &[Privileged], Decision::Auto, c), Decision::AlwaysAsk);
        assert_eq!(decide(Mode::Developer, &[Read, Credential], Decision::Auto, c), Decision::Ask);
    }

    #[test]
    fn a_tools_own_default_is_a_floor_no_mode_can_lower() {
        let c = ctx(Caller::Agent);
        assert_eq!(decide(Mode::Developer, &[Read], Decision::Ask, c), Decision::Ask);
        assert_eq!(decide(Mode::Standard, &[Read], Decision::Deny, c), Decision::Deny);
    }

    #[test]
    fn locked_mode_asks_for_reads_and_refuses_everything_else() {
        let c = ctx(Caller::Agent);
        assert_eq!(decide(Mode::Locked, &[Read], Decision::Auto, c), Decision::Ask);
        assert_eq!(decide(Mode::Locked, &[Write], Decision::Auto, c), Decision::Deny);
        assert_eq!(decide(Mode::Locked, &[Read, Network], Decision::Auto, c), Decision::Deny);
    }

    #[test]
    fn a_spoken_request_may_look_things_up_on_the_web_and_do_nothing_that_sends_or_changes()  {
        let engine = PolicyEngine::new(Mode::Standard);
        let voice = |name: &str, classes: &[PermClass], target_allowed: bool, tainted: bool| {
            engine.decide(&manifest(name, classes, Decision::Auto), classes, &PolicyContext { caller: Caller::Voice, tainted, target_allowed, private_data: false }).decision
        };
        // A search, with or without the key of the person's own service, and a page of an allowed target.
        assert_eq!(voice("web.search", &[Network], true, false), Decision::Auto);
        assert_eq!(voice("web.search", &[Network, Credential], true, false), Decision::Ask, "the mode asks for a stored secret, as it does for anyone; the voice is not what refuses it");
        assert_eq!(voice("web.read", &[Network], true, false), Decision::Auto);
        assert_eq!(voice("web.fetch", &[Network], true, false), Decision::Auto);
        // A page that is not allowed, a tool that is not one of the three, and anything that sends, writes or carries a credential.
        assert_eq!(voice("web.read", &[Network], false, false), Decision::Deny, "an unlisted target");
        assert_eq!(voice("web.fetch", &[Network], false, false), Decision::Deny);
        assert_eq!(voice("web.scrape", &[Network], true, false), Decision::Deny, "it runs a page's script");
        assert_eq!(voice("http.request", &[Network], true, false), Decision::Deny, "headers, credentials and every method");
        assert_eq!(voice("http.request", &[Network, ExternalEffect], true, false), Decision::Deny);
        assert_eq!(voice("web.read", &[Network, Credential], true, false), Decision::Deny);
        assert_eq!(voice("web.read", &[Network, ExternalEffect], true, false), Decision::Deny);
        assert_eq!(voice("web.read", &[Network, Write], true, false), Decision::Deny);
        assert_eq!(voice("web.search", &[Network, Write], true, false), Decision::Deny);
        assert_eq!(voice("x.y", &[Network], true, false), Decision::Deny, "a network tool of anyone else's is not a look-up");
        assert_eq!(voice("x.y", &[Read], false, false), Decision::Auto);
        // After a page was read (text from outside) a look-up is still a look-up: the voice could not change anything before and cannot now.
        assert_eq!(voice("web.search", &[Network], true, true), Decision::Auto);
        assert_eq!(voice("web.read", &[Network], true, true), Decision::Auto);
        assert_eq!(voice("web.read", &[Network], false, true), Decision::Deny);
    }

    /// Card R6-08, decisions D-13 and D-14: the voice's list is named. Red first: `clipboard.read` is a read that always asks, and a spoken request was let through to the question.
    #[test]
    fn a_spoken_request_has_a_named_list_the_clipboard_is_never_on_it_and_text_into_the_window_in_front_is() {
        let engine = PolicyEngine::new(Mode::Standard);
        let voice = |name: &str, default: Decision, classes: &[PermClass], target_allowed: bool, tainted: bool| {
            engine.decide(&manifest(name, classes, default), classes, &PolicyContext { caller: Caller::Voice, tainted, target_allowed, private_data: false }).decision
        };
        // The clipboard: a read by its classes, always asked about by its default, and never the voice's.
        assert_eq!(voice("clipboard.read", Decision::AlwaysAsk, &[Read], false, false), Decision::Deny, "the question a television could cause is not put");
        assert_eq!(voice("clipboard.read", Decision::AlwaysAsk, &[Read], true, true), Decision::Deny);
        assert_eq!(voice("clipboard.write", Decision::Ask, &[Write], false, false), Decision::Deny);
        assert_eq!(engine.decide(&manifest("clipboard.read", &[Read], Decision::AlwaysAsk), &[Read], &ctx(Caller::Agent)).decision, Decision::AlwaysAsk, "the agent is still asked, as before");
        // Another read-only tool is still the voice's.
        assert_eq!(voice("vault.search", Decision::Auto, &[Read], false, false), Decision::Auto);
        // Text into the window in front, and opening an app by name: only where the provider vouches for the call (target allowed, the one class Write).
        for tool in ["win.type", "win.app"] {
            assert_eq!(voice(tool, Decision::Auto, &[Write], true, false), Decision::Auto, "{tool}");
            assert_eq!(voice(tool, Decision::Auto, &[Write], false, false), Decision::Deny, "{tool}: a window the provider does not vouch for");
            assert_eq!(voice(tool, Decision::Auto, &[Write, ExternalEffect], true, false), Decision::Deny, "{tool}: where the guard would ask, the voice is refused");
            assert_eq!(voice(tool, Decision::Auto, &[Write, Privileged], false, false), Decision::Deny, "{tool}: a shell");
            // A run that read outside text is still asked every time before it changes anything, the voice's too.
            assert_eq!(voice(tool, Decision::Auto, &[Write], true, true), Decision::AlwaysAsk, "{tool}: after a page was read");
        }
        // Nothing else that changes the desktop, the browser or a file is on the list.
        for tool in ["win.click", "win.shortcut", "win.multi_edit", "win.multi_select", "win.move", "browser.click", "browser.type", "browser.navigate", "browser.press_key", "fs.write", "term.run"] {
            assert_eq!(voice(tool, Decision::Auto, &[Write], true, false), Decision::Deny, "{tool}");
        }
        assert_eq!(voice("win.snapshot", Decision::Auto, &[Read], true, false), Decision::Auto, "looking at the screen");
        // Full access does not widen the list.
        engine.set_full_access(true);
        assert_eq!(voice("win.click", Decision::Auto, &[Write], true, false), Decision::Deny);
        assert_eq!(voice("clipboard.read", Decision::AlwaysAsk, &[Read], true, false), Decision::Deny);
        assert_eq!(voice("win.type", Decision::Auto, &[Write], true, false), Decision::Auto);
    }

    #[test]
    fn voice_and_replay_cannot_act() {
        assert_eq!(decide(Mode::Developer, &[Write], Decision::Auto, ctx(Caller::Voice)), Decision::Deny);
        assert_eq!(decide(Mode::Developer, &[Read], Decision::Auto, ctx(Caller::Voice)), Decision::Auto);
        assert_eq!(decide(Mode::Developer, &[Execute], Decision::Auto, ctx(Caller::Replay)), Decision::Deny);
        assert_eq!(decide(Mode::Developer, &[Remote], Decision::Auto, ctx(Caller::Replay)), Decision::Deny);
        assert_eq!(decide(Mode::Standard, &[Read], Decision::Auto, ctx(Caller::Replay)), Decision::Auto);
    }

    #[test]
    fn external_mcp_clients_never_get_a_lasting_grant() {
        assert_eq!(decide(Mode::Standard, &[Write], Decision::Auto, ctx(Caller::McpClient)), Decision::AlwaysAsk);
        assert_eq!(decide(Mode::Standard, &[Read], Decision::Auto, ctx(Caller::McpClient)), Decision::Auto);
    }

    #[test]
    fn a_tainted_run_cannot_change_anything_without_a_fresh_answer() {
        let t = PolicyContext { tainted: true, ..ctx(Caller::Agent) };
        assert_eq!(decide(Mode::Developer, &[Write], Decision::Auto, t), Decision::AlwaysAsk);
        assert_eq!(decide(Mode::Developer, &[Execute], Decision::Auto, t), Decision::AlwaysAsk);
        assert_eq!(decide(Mode::Developer, &[Read, Credential], Decision::Auto, t), Decision::AlwaysAsk);
        assert_eq!(decide(Mode::Developer, &[Read], Decision::Auto, t), Decision::Auto);
    }

    /// What the host does with a call of a tainted run when the run holds an edit grant (`ToolHost::use_edit_grant`): the decision the ordinary rules give; and when that is a question and the grant
    /// lifts the taint rule, the decision for the same call as for a clean run, which the host accepts only when it is automatic.
    fn with_grant(e: &PolicyEngine, tool: &str, classes: &[PermClass], c: PolicyContext, facts: EditGrantFacts) -> Decision {
        let m = manifest(tool, classes, Decision::Auto);
        let ordinary = e.decide(&m, classes, &c).decision;
        if ordinary >= Decision::Ask && edit_grant_lifts_taint(tool, classes, &c, &facts) {
            let clean = e.decide(&m, classes, &PolicyContext { tainted: false, ..c }).decision;
            if clean == Decision::Auto {
                return Decision::Auto;
            }
        }
        ordinary
    }

    const GRANTED: EditGrantFacts = EditGrantFacts { covers: true, external_taint: false };
    const NO_GRANT: EditGrantFacts = EditGrantFacts { covers: false, external_taint: false };

    /// A call of the agent that reads files of the project and then edits one of them: tainted by what it read, and the target is a folder the provider vouches for.
    fn after_a_local_read() -> PolicyContext {
        PolicyContext { tainted: true, target_allowed: true, ..ctx(Caller::Agent) }
    }

    #[test]
    fn local_read_taint_asks_once_with_a_run_grant() {
        for mode in [Mode::Standard, Mode::Developer] {
            let e = PolicyEngine::new(mode);
            for tool in crate::run_grant::EDIT_TOOLS {
                // The first edit, before any grant: the question is asked, as it always was.
                assert_eq!(with_grant(&e, tool, &[Write], after_a_local_read(), NO_GRANT), Decision::AlwaysAsk, "{mode:?} {tool}: the first diff asks");
                // The person answered with the run option: the next ones in the folder go through, however many.
                for n in 1..=10 {
                    assert_eq!(with_grant(&e, tool, &[Write], after_a_local_read(), GRANTED), Decision::Auto, "{mode:?} {tool}: edit {n} in the granted folder");
                }
                // A call that no grant names asks, and a clean run needs no grant.
                assert_eq!(with_grant(&e, tool, &[Write], after_a_local_read(), EditGrantFacts { covers: false, external_taint: false }), Decision::AlwaysAsk);
                let clean = PolicyContext { tainted: false, ..after_a_local_read() };
                assert_eq!(with_grant(&e, tool, &[Write], clean, NO_GRANT), Decision::Auto);
            }
        }
    }

    #[test]
    fn external_taint_always_asks() {
        let external = EditGrantFacts { covers: true, external_taint: true };
        for mode in [Mode::Standard, Mode::Developer] {
            let e = PolicyEngine::new(mode);
            for tool in crate::run_grant::EDIT_TOOLS {
                assert_eq!(with_grant(&e, tool, &[Write], after_a_local_read(), external), Decision::AlwaysAsk, "{mode:?} {tool}: a grant that covers the folder does not answer a run that read the web");
                assert!(!edit_grant_lifts_taint(tool, &[Write], &after_a_local_read(), &external));
                assert!(edit_grant_lifts_taint(tool, &[Write], &after_a_local_read(), &GRANTED));
            }
        }
        // The taint rule itself is not changed by any of this: a tainted change asks every time, in every mode.
        for mode in [Mode::Standard, Mode::Developer] {
            assert_eq!(decide(mode, &[Write], Decision::Auto, after_a_local_read()), Decision::AlwaysAsk, "{mode:?}");
        }
    }

    #[test]
    fn grant_does_not_cover_write_protected_names() {
        // A name the file tools never write (`.vscode`, `.cargo`, `.husky`, `.devcontainer`, `.idea`...) is refused when the call is classified, before any decision, so a grant never sees it
        // (`fs_tools.rs`: `a_grant_never_covers_a_protected_name_a_script_a_secret_file_or_a_delete`). What this test pins is the rest of what a grant must never reach: every call whose classes are
        // more than a plain write (a script or a build file is `Execute`, a file of secrets is `Credential`), every tool that is not an edit, a target the provider does not vouch for, and every
        // caller but the agent.
        for mode in [Mode::Standard, Mode::Developer] {
            let e = PolicyEngine::new(mode);
            let ask = |tool: &str, classes: &[PermClass], c: PolicyContext| with_grant(&e, tool, classes, c, GRANTED);
            for classes in [&[Write, Execute][..], &[Write, Credential], &[Write, Destructive], &[Write, Privileged], &[Write, ExternalEffect], &[Write, Network], &[Write, Remote], &[Execute]] {
                for tool in crate::run_grant::EDIT_TOOLS {
                    assert_eq!(ask(tool, classes, after_a_local_read()), Decision::AlwaysAsk, "{mode:?} {tool} {classes:?}");
                }
            }
            for tool in ["fs.delete", "fs.move", "fs.copy", "fs.restore", "fs.undo_run", "fs.set_attrs", "fs.revoke_folder", "term.run", "term.shell", "git.commit", "git.push", "web.fetch", "memory.write"] {
                assert!(ask(tool, &[Write], after_a_local_read()) >= Decision::AlwaysAsk, "{mode:?} {tool}: not an edit, so no grant answers it");
            }
            let elsewhere = PolicyContext { target_allowed: false, ..after_a_local_read() };
            for tool in crate::run_grant::EDIT_TOOLS {
                assert_eq!(ask(tool, &[Write], elsewhere), Decision::AlwaysAsk, "{mode:?} {tool}: a folder the provider does not vouch for");
                for caller in [Caller::Voice, Caller::Api, Caller::Replay, Caller::McpClient] {
                    let c = PolicyContext { caller, ..after_a_local_read() };
                    assert!(!edit_grant_possible(tool, &[Write], &c), "{caller:?}");
                    assert_ne!(ask(tool, &[Write], c), Decision::Auto, "{mode:?} {tool} {caller:?}");
                }
            }
        }
    }

    #[test]
    fn a_grant_never_makes_a_call_looser_than_the_same_call_of_a_clean_run() {
        // The lift removes the taint rule and nothing else: whatever else makes a call ask (the autonomy level, the tool's own override in the policy file, full access) still does.
        for mode in [Mode::Standard, Mode::Developer] {
            for level in [AskLevel::AskRisky, AskLevel::AskOutside, AskLevel::AskAll] {
                let e = PolicyEngine::new(mode);
                e.set_ask_level(level);
                for tool in crate::run_grant::EDIT_TOOLS {
                    let m = manifest(tool, &[Write], Decision::Auto);
                    let clean = e.decide(&m, &[Write], &PolicyContext { tainted: false, ..after_a_local_read() }).decision;
                    let granted = with_grant(&e, tool, &[Write], after_a_local_read(), GRANTED);
                    assert!(granted >= clean, "{mode:?} {level:?} {tool}: {granted:?} is looser than {clean:?}");
                    assert_eq!(granted == Decision::Auto, level != AskLevel::AskAll, "{mode:?} {level:?} {tool}: ask-for-everything keeps asking");
                }
            }
        }
        let (e, err) = PolicyEngine::from_file_text(Some(r#"{"mode": "developer", "tool_overrides": {"fs.write": "always_ask"}}"#));
        assert!(err.is_none());
        assert_eq!(with_grant(&e, "fs.write", &[Write], after_a_local_read(), GRANTED), Decision::AlwaysAsk, "the person's own override of a tool still asks");
        assert_eq!(with_grant(&e, "fs.edit", &[Write], after_a_local_read(), GRANTED), Decision::Auto);
    }

    #[test]
    fn hard_deny_beats_everything_and_supports_domain_wildcards() {
        let e = PolicyEngine::new(Mode::Developer);
        e.set_hard_deny(vec!["secret.read".into(), "ssh.*".into()]);
        let c = ctx(Caller::Ui);
        for (name, denied) in [("secret.read", true), ("ssh.exec", true), ("ssh.upload", true), ("fs.read", false), ("secret.list", false)] {
            let v = e.decide(&manifest(name, &[Read], Decision::Auto), &[Read], &c);
            assert_eq!(v.decision == Decision::Deny, denied, "{name}");
        }
    }

    #[test]
    fn a_policy_file_sets_mode_hard_deny_and_tightening_overrides() {
        let text = r#"{"mode": "developer", "hard_deny": ["ssh.*"], "tool_overrides": {"mock.write": "always_ask", "mock.read": "auto"}}"#;
        let (e, err) = PolicyEngine::from_file_text(Some(text));
        assert!(err.is_none());
        assert_eq!(e.mode(), Mode::Developer);
        let c = ctx(Caller::Agent);
        let write = manifest("mock.write", &[Write], Decision::Auto);
        assert_eq!(e.decide(&write, &[Write], &c).decision, Decision::AlwaysAsk);
        // An override to auto never loosens what the rules decide.
        let read = manifest("mock.read", &[Read], Decision::Ask);
        assert_eq!(e.decide(&read, &[Read], &c).decision, Decision::Ask);
        assert_eq!(e.decide(&manifest("ssh.exec", &[Read], Decision::Auto), &[Read], &c).decision, Decision::Deny);
        assert_eq!(e.snapshot(), PolicyFile::parse(text).unwrap());
    }

    #[test]
    fn a_broken_policy_file_locks_the_host_and_says_why() {
        for bad in ["not json", r#"{"mode": "yolo"}"#, r#"{"mode": "standard", "hard_deny": ["Not A Tool"]}"#, r#"{"mode": "standard", "typo_field": 1}"#, r#"{"tool_overrides": {"bad name": "deny"}}"#] {
            let (e, err) = PolicyEngine::from_file_text(Some(bad));
            assert_eq!(e.mode(), Mode::Locked, "{bad}");
            assert!(err.is_some(), "{bad}");
        }
        let (e, err) = PolicyEngine::from_file_text(None);
        assert_eq!((e.mode(), err), (Mode::Standard, None));
        assert_eq!(PolicyFile::parse("{}").unwrap(), PolicyFile::default());
    }

    fn full(mode: Mode, classes: &[PermClass], c: PolicyContext) -> Decision {
        let e = PolicyEngine::new(mode);
        e.set_full_access(true);
        e.decide(&manifest("x.y", classes, Decision::Auto), classes, &c).decision
    }

    #[test]
    fn full_access_lets_ordinary_work_run_and_keeps_the_dangerous_classes_asking() {
        let inside = PolicyContext { target_allowed: true, ..ctx(Caller::Agent) };
        let outside = PolicyContext { target_allowed: false, ..ctx(Caller::Agent) };
        for mode in [Mode::Standard, Mode::Developer] {
            // What used to ask in standard mode now runs, wherever the target is.
            for classes in [&[Read][..], &[Write], &[Write, Execute], &[Execute], &[Read, Network], &[Write, Network]] {
                assert_eq!(full(mode, classes, inside), Decision::Auto, "{mode:?} {classes:?} inside");
                assert_eq!(full(mode, classes, outside), Decision::Auto, "{mode:?} {classes:?} outside");
            }
            // Still asking: secrets and other machines (once per session), destructive in the project (once), outside (always).
            assert_eq!(full(mode, &[Read, Credential], inside), Decision::Ask);
            assert_eq!(full(mode, &[Remote], inside), Decision::Ask);
            assert_eq!(full(mode, &[Write, Destructive], inside), Decision::Ask);
            assert_eq!(full(mode, &[Write, Destructive], outside), Decision::AlwaysAsk);
            // Never skipped.
            for always in [Privileged, ExternalEffect] {
                assert_eq!(full(mode, &[Write, always], inside), Decision::AlwaysAsk, "{always:?}");
            }
        }
    }

    #[test]
    fn full_access_keeps_every_other_safeguard() {
        let e = PolicyEngine::new(Mode::Standard);
        e.set_full_access(true);
        e.set_hard_deny(vec!["ssh.*".into()]);
        e.apply(&PolicyFile { mode: Mode::Standard, limits: Limits::default(), hard_deny: vec!["ssh.*".into()], tool_overrides: [("mock.write".to_string(), Decision::AlwaysAsk)].into_iter().collect() });
        assert!(e.full_access(), "applying a policy file does not switch full access off");
        let c = ctx(Caller::Agent);
        assert_eq!(e.decide(&manifest("ssh.exec", &[Read], Decision::Auto), &[Read], &c).decision, Decision::Deny, "hard deny");
        assert_eq!(e.decide(&manifest("mock.write", &[Write], Decision::Auto), &[Write], &c).decision, Decision::AlwaysAsk, "override");
        // Untrusted text still needs a fresh answer to change anything.
        let t = PolicyContext { tainted: true, ..c };
        assert_eq!(e.decide(&manifest("a.b", &[Write], Decision::Auto), &[Write], &t).decision, Decision::AlwaysAsk);
        assert_eq!(e.decide(&manifest("a.b", &[Read], Decision::Auto), &[Read], &t).decision, Decision::Auto);
        // Caller rules.
        assert_eq!(e.decide(&manifest("a.b", &[Write], Decision::Auto), &[Write], &ctx(Caller::Voice)).decision, Decision::Deny);
        assert_eq!(e.decide(&manifest("a.b", &[Write], Decision::Auto), &[Write], &ctx(Caller::Replay)).decision, Decision::Deny);
        assert_eq!(e.decide(&manifest("a.b", &[Write], Decision::Auto), &[Write], &ctx(Caller::McpClient)).decision, Decision::AlwaysAsk, "an outside MCP client gets no part of full access");
    }

    /// Card OA1-04 (SE-14): `FULL_ACCESS.md` says that under full access deleting in the project asks (once, then it is remembered for the session) and outside it always asks. `fs.delete` is a
    /// Write, so full access let it run unasked, a folder with everything in it included.
    #[test]
    fn under_full_access_a_delete_asks_once_inside_the_project_and_every_time_outside_and_other_writes_still_run() {
        let del = |full: bool, mode: Mode, allowed: bool| {
            let e = PolicyEngine::new(mode);
            e.set_full_access(full);
            let c = PolicyContext { target_allowed: allowed, ..ctx(Caller::Agent) };
            e.decide(&manifest("fs.delete", &[Write], Decision::Auto), &[Write], &c).decision
        };
        for mode in [Mode::Standard, Mode::Developer] {
            assert_eq!(del(true, mode, true), Decision::Ask, "{mode:?}: in the project it asks (the session remembers it)");
            assert_eq!(del(true, mode, false), Decision::AlwaysAsk, "{mode:?}: outside the project it asks every time");
        }
        // Full access off: nothing changed for the ordinary modes.
        assert_eq!(del(false, Mode::Standard, true), Decision::Auto);
        assert_eq!(del(false, Mode::Developer, true), Decision::Auto);
        assert_eq!(del(false, Mode::Standard, false), Decision::Ask);
        // Under full access the other writes still run, and a tool that merely has "delete" in its name is not the file delete.
        let e = PolicyEngine::new(Mode::Standard);
        e.set_full_access(true);
        let c = PolicyContext { target_allowed: true, ..ctx(Caller::Agent) };
        for tool in ["fs.write", "fs.move", "fs.edit", "fs.delete_everything", "memory.forget"] {
            assert_eq!(e.decide(&manifest(tool, &[Write], Decision::Auto), &[Write], &c).decision, Decision::Auto, "{tool}");
        }
        // The Credential rule still applies to a delete of a secret file (asks at least as much).
        assert!(e.decide(&manifest("fs.delete", &[Write, Credential], Decision::Auto), &[Write, Credential], &c).decision >= Decision::Ask);
    }

    /// Card OA1-04 (SE-01): the switch is the person's, made in the app; a call made with the API token cannot make it, so it cannot use it. It gets the ordinary rules of the mode and,
    /// for a change, a fresh answer every time. Before, the API caller was in the full-access arm and a script with the token wrote and ran programs in the project without a question.
    #[test]
    fn full_access_does_not_apply_to_the_api_caller() {
        let e_off = PolicyEngine::new(Mode::Standard);
        let e_on = PolicyEngine::new(Mode::Standard);
        e_on.set_full_access(true);
        for mode in [Mode::Standard, Mode::Developer] {
            e_off.set_mode(mode);
            e_on.set_mode(mode);
            for inside in [true, false] {
                let api = PolicyContext { target_allowed: inside, ..ctx(Caller::Api) };
                let agent = PolicyContext { target_allowed: inside, ..ctx(Caller::Agent) };
                for classes in [&[Read][..], &[Write], &[Write, Execute], &[Execute], &[Read, Network], &[Write, Network], &[Read, Credential], &[Remote], &[Write, Destructive]] {
                    let m = manifest("x.y", classes, Decision::Auto);
                    let with = e_on.decide(&m, classes, &api);
                    assert_eq!(with.decision, e_off.decide(&m, classes, &api).decision, "{mode:?} {classes:?} inside={inside}: full access changes nothing for the API caller");
                    if classes.iter().any(|c| c.changes_state() || *c == Credential) {
                        assert_eq!(with.decision, Decision::AlwaysAsk, "{mode:?} {classes:?} inside={inside}: a change made with the API token asks every time");
                        // (A destructive call already asked every time, for its own reason.)
                        assert!(classes.contains(&Destructive) || with.reason.contains("API token"), "the reason says what is true: {}", with.reason);
                    }
                }
                // The contrast that was the defect: the agent, under full access, writes in the project without a question; the API caller is asked.
                let w = manifest("x.y", &[Write], Decision::Auto);
                assert_eq!(e_on.decide(&w, &[Write], &agent).decision, Decision::Auto);
                assert_eq!(e_on.decide(&w, &[Write], &api).decision, Decision::AlwaysAsk);
            }
            // Reads stay automatic for a script that only looks.
            let read = manifest("x.y", &[Read], Decision::Auto);
            assert_eq!(e_on.decide(&read, &[Read], &PolicyContext { target_allowed: true, ..ctx(Caller::Api) }).decision, Decision::Auto);
        }
        // The one door's rule is never looser than the ordinary one, so it asks too.
        let m = manifest("x.y", &[Write], Decision::Auto);
        let api = PolicyContext { target_allowed: true, ..ctx(Caller::Api) };
        assert_eq!(e_on.decide_door(&m, &[Write], &api, &api, false).decision, Decision::AlwaysAsk);
        // The person's own calls in the app are not the API caller's and stay as they were.
        let ui = PolicyContext { target_allowed: true, ..ctx(Caller::Ui) };
        assert_eq!(e_on.decide(&m, &[Write], &ui).decision, Decision::Auto);
    }

    /// Found in the audit: after a run read a web page, a GET to an unknown host with data in its address was automatic under
    /// full access (which has no rule for the network) and in developer mode, because taint only tightened changes.
    #[test]
    fn a_tainted_run_asks_before_any_request_to_a_target_that_is_not_allowed_in_every_mode() {
        let unknown = PolicyContext { tainted: true, target_allowed: false, ..ctx(Caller::Agent) };
        let allowed = PolicyContext { tainted: true, target_allowed: true, ..ctx(Caller::Agent) };
        let clean = PolicyContext { tainted: false, target_allowed: false, ..ctx(Caller::Agent) };
        for mode in [Mode::Standard, Mode::Developer] {
            assert_eq!(decide(mode, &[Read, Network], Decision::Auto, unknown), Decision::AlwaysAsk, "{mode:?} tainted, unknown target");
            assert_eq!(full(mode, &[Read, Network], unknown), Decision::AlwaysAsk, "{mode:?} full access, tainted, unknown target");
            // Still automatic where it was: an allowed target, or a run that has read nothing untrusted (full access only).
            assert_eq!(decide(mode, &[Read, Network], Decision::Auto, allowed), Decision::Auto, "{mode:?} allowed target");
            assert_eq!(full(mode, &[Read, Network], allowed), Decision::Auto, "{mode:?} full access, allowed target");
            assert_eq!(full(mode, &[Read, Network], clean), Decision::Auto, "{mode:?} full access, clean run");
        }
        // Reads that touch no network are unaffected.
        assert_eq!(decide(Mode::Developer, &[Read], Decision::Auto, unknown), Decision::Auto);
    }

    #[test]
    fn full_access_never_unlocks_a_locked_host() {
        let c = ctx(Caller::Agent);
        assert_eq!(full(Mode::Locked, &[Write], c), Decision::Deny);
        assert_eq!(full(Mode::Locked, &[Read], c), Decision::Ask);
    }

    #[test]
    fn full_access_is_off_until_switched_on_and_switches_back() {
        let e = PolicyEngine::new(Mode::Standard);
        assert!(!e.full_access());
        let c = PolicyContext { target_allowed: false, ..ctx(Caller::Agent) };
        let m = manifest("a.b", &[Write], Decision::Auto);
        assert_eq!(e.decide(&m, &[Write], &c).decision, Decision::Ask);
        e.set_full_access(true);
        assert_eq!(e.decide(&m, &[Write], &c).decision, Decision::Auto);
        e.set_full_access(false);
        assert_eq!(e.decide(&m, &[Write], &c).decision, Decision::Ask);
    }

    /// Card R6-08: what the one door will apply is the stricter of the ordinary rule and the capability rule, and a call that sends something out is held to what the
    /// run has read (it asks every time) and holds (it is asked); a clean run's send stays free.
    #[test]
    fn the_doors_decision_is_never_looser_than_the_applied_one_and_holds_a_send_to_what_the_run_has_read_and_holds() {
        let classes_sets: &[&[PermClass]] = &[&[Read], &[Write], &[Network, Write], &[Network], &[Execute, Network], &[Network, ExternalEffect], &[Destructive]];
        for mode in [Mode::Locked, Mode::Standard, Mode::Developer] {
            for full in [false, true] {
                let e = PolicyEngine::new(mode);
                e.set_full_access(full);
                for caller in [Caller::Agent, Caller::Voice, Caller::McpClient, Caller::Replay] {
                    for (tainted, private) in [(false, false), (true, false), (false, true), (true, true)] {
                        for allowed in [false, true] {
                            for sends in [false, true] {
                                for cls in classes_sets {
                                    let applied = PolicyContext { caller, tainted, private_data: false, target_allowed: allowed };
                                    let caps = PolicyContext { private_data: private, ..applied };
                                    let m = manifest("browser.x", cls, Decision::Auto);
                                    let (door, old) = (e.decide_door(&m, cls, &applied, &caps, sends), e.decide(&m, cls, &applied));
                                    assert!(door.decision >= old.decision, "{mode:?} full={full} {caller:?} tainted={tainted} private={private} allowed={allowed} sends={sends} {cls:?}: {:?} is looser than {:?}", door.decision, old.decision);
                                }
                            }
                        }
                    }
                }
            }
        }
        let e = PolicyEngine::new(Mode::Standard);
        e.set_full_access(true);
        let m = manifest("browser.type", &[Network, Write], Decision::Auto);
        let cls: &[PermClass] = &[Network, Write];
        let at = |tainted: bool, private: bool, sends: bool| {
            let applied = PolicyContext { caller: Caller::Agent, tainted, private_data: false, target_allowed: true };
            e.decide_door(&m, cls, &applied, &PolicyContext { private_data: private, ..applied }, sends).decision
        };
        assert_eq!(at(false, false, true), Decision::Auto, "a clean run's submit stays free");
        assert_eq!(at(false, false, false), Decision::Auto);
        assert_eq!(at(false, true, false), Decision::Auto, "typing that sends nothing out is not held for private data");
        assert_eq!(at(false, true, true), Decision::Ask, "a run that holds private data is asked before it sends");
        assert_eq!(at(true, false, true), Decision::AlwaysAsk, "a run that read outside text asks every time");
        // The capability rule alone lets a change after untrusted text go on (a write inside the trusted place); the door does not.
        let caps_alone = e.decide_caps(&m, cls, &PolicyContext { caller: Caller::Agent, tainted: true, private_data: false, target_allowed: true });
        assert_eq!(caps_alone.decision, Decision::Auto);
        assert_eq!(at(true, false, false), Decision::AlwaysAsk, "the door asks where the ordinary rule asks");
        // A refusal is a refusal, whatever else is said.
        let voice = PolicyContext { caller: Caller::Voice, tainted: false, private_data: false, target_allowed: true };
        assert_eq!(e.decide_door(&m, cls, &voice, &voice, false).decision, Decision::Deny);
    }

    #[test]
    fn combining_rules_never_loosens() {
        // Property: for every mode/caller/taint/class combination the result is at least the tool default.
        let classes_sets: &[&[PermClass]] = &[&[Read], &[Write], &[Execute, Network], &[Destructive], &[Read, Credential], &[ExternalEffect, Remote]];
        for mode in [Mode::Locked, Mode::Standard, Mode::Developer] {
            for caller in [Caller::Agent, Caller::Api, Caller::Ui, Caller::Voice, Caller::McpClient, Caller::Replay] {
                for tainted in [false, true] {
                    for cls in classes_sets {
                        for default in [Decision::Auto, Decision::Ask, Decision::AlwaysAsk, Decision::Deny] {
                            let c = PolicyContext { caller, tainted, target_allowed: true, private_data: false };
                            assert!(decide(mode, cls, default, c) >= default);
                        }
                    }
                }
            }
        }
    }

    // ---- the autonomy level (card R7-18) ----------------------------------------------------------------------------------

    fn decide_row_at(r: &Row, caps: bool, level: AskLevel) -> Decision {
        let engine = PolicyEngine::new(r.mode);
        engine.set_full_access(r.full);
        engine.set_ask_level(level);
        let m = manifest(r.name, r.classes, Decision::Auto);
        let ctx = PolicyContext { caller: r.caller, tainted: r.tainted, private_data: r.private, target_allowed: r.allowed };
        if caps {
            engine.decide_caps(&m, r.classes, &ctx).decision
        } else {
            engine.decide(&m, r.classes, &ctx).decision
        }
    }

    #[test]
    fn the_levels_are_named_ordered_by_how_much_they_ask_and_the_default_is_todays_rules() {
        assert_eq!(AskLevel::default(), AskLevel::AskRisky);
        assert_eq!(PolicyEngine::new(Mode::Standard).ask_level(), AskLevel::AskRisky, "a new engine asks as it always did");
        assert!(AskLevel::AskRisky < AskLevel::AskOutside && AskLevel::AskOutside < AskLevel::AskAll);
        for level in AskLevel::ALL {
            assert_eq!(AskLevel::from_id(level.id()), Some(level));
            assert_eq!(serde_json::to_string(&level).unwrap(), format!("\"{}\"", level.id()), "the word in the file is the word of the route");
            let engine = PolicyEngine::new(Mode::Standard);
            engine.set_ask_level(level);
            assert_eq!(engine.ask_level(), level);
        }
        assert_eq!(AskLevel::ALL.map(AskLevel::id), ["ask_all", "ask_risky", "ask_outside"]);
        for not_a_level in ["", "ask", "ASK_ALL", "ask_everything", "ask_all ", "full"] {
            assert_eq!(AskLevel::from_id(not_a_level), None, "{not_a_level:?}");
        }
    }

    #[test]
    fn each_level_decides_the_calls_somebody_could_be_surprised_by_as_written() {
        use AskLevel::{AskAll, AskOutside, AskRisky};
        // (label, level, mode, full access, classes, caller, target allowed, what the decision is): a function, so that the classes of each row are a slice whatever their number.
        let r = |label: &'static str, level: AskLevel, mode: Mode, full: bool, classes: &'static [PermClass], caller: Caller, allowed: bool, want: Decision| (label, level, mode, full, classes, caller, allowed, want);
        let rows = vec![
            r("ask all: a read in the project", AskAll, STD, false, &[Read], Caller::Agent, true, K),
            r("ask all: a read of a file elsewhere", AskAll, STD, false, &[Read], Caller::Agent, false, K),
            r("ask all: a write to the project", AskAll, STD, false, &[Write], Caller::Agent, true, K),
            r("ask all: a write elsewhere asked already", AskAll, STD, false, &[Write], Caller::Agent, false, K),
            r("ask all: something destructive asks every time as before", AskAll, STD, false, &[Write, Destructive], Caller::Agent, true, AA),
            r("ask all: the person's own read in the app is not asked about", AskAll, STD, false, &[Read], Caller::Ui, true, A),
            r("ask all: a spoken read asks", AskAll, STD, false, &[Read], Caller::Voice, true, K),
            r("ask all: a spoken write is still refused", AskAll, STD, false, &[Write], Caller::Voice, true, D),
            r("ask all: a replayed read asks", AskAll, STD, false, &[Read], Caller::Replay, true, K),
            r("ask all: a replayed write is still refused", AskAll, STD, false, &[Write], Caller::Replay, true, D),
            r("ask all: an outside client's read asks", AskAll, STD, false, &[Read], Caller::McpClient, true, K),
            r("ask all: a request to a listed address asks", AskAll, STD, false, &[Network], Caller::Agent, true, K),
            r("ask all: a request to an unlisted address asks every time", AskAll, STD, false, &[Network], Caller::Agent, false, AA),
            r("ask all: a read under full access asks", AskAll, STD, true, &[Read], Caller::Agent, true, K),
            r("ask all: a read when the policy is locked asks as before", AskAll, Mode::Locked, false, &[Read], Caller::Agent, true, K),
            r("ask all: a write when the policy is locked is still refused", AskAll, Mode::Locked, false, &[Write], Caller::Agent, true, D),
            r("ask risky: a read", AskRisky, STD, false, &[Read], Caller::Agent, true, A),
            r("ask risky: a request to an unlisted address under full access runs (full access has no rule for the network)", AskRisky, STD, true, &[Network], Caller::Agent, false, A),
            r("ask outside: a read", AskOutside, STD, false, &[Read], Caller::Agent, true, A),
            r("ask outside: a write to the project", AskOutside, STD, false, &[Write], Caller::Agent, true, A),
            r("ask outside: a request to a listed address", AskOutside, STD, false, &[Network], Caller::Agent, true, A),
            r("ask outside: a request to an unlisted address asks every time", AskOutside, STD, false, &[Network], Caller::Agent, false, AA),
            r("ask outside: the same under full access asks every time", AskOutside, STD, true, &[Network], Caller::Agent, false, AA),
            r("ask outside: a request to a listed address under full access", AskOutside, STD, true, &[Network], Caller::Agent, true, A),
            r("ask outside: an effect on someone else's system asked already", AskOutside, STD, false, &[Network, ExternalEffect], Caller::Agent, true, AA),
            r("ask outside: an effect on someone else's system under full access asked already", AskOutside, STD, true, &[Network, ExternalEffect], Caller::Agent, true, AA),
            r("ask outside: the person's own request in the app keeps the ordinary rule", AskOutside, STD, false, &[Network], Caller::Ui, false, K),
            r("ask outside: developer mode, an unlisted address", AskOutside, Mode::Developer, false, &[Network], Caller::Agent, false, AA),
        ];
        let wrong: Vec<String> = rows
            .iter()
            .filter_map(|(label, level, mode, full, classes, caller, allowed, want)| {
                let engine = PolicyEngine::new(*mode);
                engine.set_full_access(*full);
                engine.set_ask_level(*level);
                let ctx = PolicyContext { caller: *caller, tainted: false, private_data: false, target_allowed: *allowed };
                let got = engine.decide(&manifest("x.y", classes, Decision::Auto), classes, &ctx).decision;
                (got != *want).then(|| format!("{label}: wanted {want:?}, got {got:?}"))
            })
            .collect();
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn no_level_is_looser_than_the_default_on_any_row_of_the_decision_table_and_each_level_asks_at_least_what_the_one_before_it_does() {
        let rows = table();
        for r in &rows {
            for caps in [false, true] {
                let base = decide_row_at(r, caps, AskLevel::AskRisky);
                assert_eq!(base, decide_row(r, caps), "{}: the default level changes nothing", r.label);
                let (outside, all) = (decide_row_at(r, caps, AskLevel::AskOutside), decide_row_at(r, caps, AskLevel::AskAll));
                assert!(outside >= base && all >= outside, "{} (caps rule {caps}): default {base:?}, outside {outside:?}, all {all:?}", r.label);
                // The hard floors and the refusals are the same at every level: a level moves a decision up, and these are at the top or are asked every time already.
                if base == D {
                    assert_eq!((outside, all), (D, D), "{}: a refusal stays a refusal", r.label);
                }
                if base == AA {
                    assert_eq!((outside, all), (AA, AA), "{}: a question asked every time is not made softer or harder", r.label);
                }
            }
        }
        assert!(rows.len() >= 60, "the table the levels are held to has {} rows", rows.len());
    }
}
