//! The person's pattern rules (card OA8-04, task SE-20 of the OpenCode audit): `data/tools/permissions.json`.
//!
//! A rule names a tool (`fs.*`, `term.run`, `*`) and a pattern for what the call acts on, and says what happens: `ask`, `always_ask` or `deny`. **A rules file can only tighten.**
//! There is no `allow`: a file that says it is refused as a wrong file, so nobody can believe that a line of it lets something through. The rules are applied after the policy has
//! decided (the hard-deny list comes first, and a refusal stays a refusal), the strictest rule that matches wins (`deny` over `always_ask` over `ask`), and the result is never
//! looser than what the policy decided: the same ladder as `tool_overrides` in `policy.json` and the person's hooks, but by target, which those cannot see.
//!
//! What a rule is matched against, for one call (see [`subjects_of`]): the target the provider names in its question (a resolved path for the file tools, `host:port` for the web
//! tools, the program for a command), and where the call has them the `url` argument and the command line (`exe` and `args`, or `command`). A rule sees what the question shows and
//! no more: a refusal by a pattern on a spelling the model chose (a URL's path, a command line) is a convenience, and the canonical target is what holds.
//!
//! The pattern language is the smallest that does the job: `*` and `**` stand for any run of characters (a `/` too), `?` for one character; the case does not matter, `\` counts as `/`
//! and a run of spaces is one space. A pattern that ends in `/**` also matches the folder itself.
//!
//! A file that cannot be used does not drop its rules: every call that is not a read asks until it is fixed ([`RuleSet::broken`]), as a broken `hooks.json` does.

use crate::hooks::tool_pattern_matches;
use crate::manifest::Decision;
use serde::Deserialize;
use serde_json::Value;

/// The most rules a file may hold, and the longest pattern and reason: a file this large is a mistake, and every call is matched against every rule.
pub const MAX_RULES: usize = 500;
pub const MAX_PATTERN_CHARS: usize = 500;
pub const MAX_REASON_CHARS: usize = 200;

/// What a rule does to a call that matches it. Ordered by how much it holds back; there is no variant that lets something through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Effect {
    /// The call asks the person, and an answer "for this session" covers the same call for a while, as for any question of the policy.
    Ask,
    /// The call asks every time; no answer is remembered.
    AlwaysAsk,
    /// The call is refused.
    Deny,
}

impl Effect {
    pub fn decision(self) -> Decision {
        match self {
            Effect::Ask => Decision::Ask,
            Effect::AlwaysAsk => Decision::AlwaysAsk,
            Effect::Deny => Decision::Deny,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Effect::Ask => "ask",
            Effect::AlwaysAsk => "always_ask",
            Effect::Deny => "deny",
        }
    }

    fn parse(word: &str) -> Result<Self, String> {
        match word.trim().to_ascii_lowercase().as_str() {
            "ask" => Ok(Effect::Ask),
            "always_ask" => Ok(Effect::AlwaysAsk),
            "deny" => Ok(Effect::Deny),
            "allow" | "auto" | "allowed" | "permit" => {
                Err(format!("effect \"{word}\" is not possible: a rules file can only add questions and refusals, it can never allow anything (use ask, always_ask or deny)"))
            }
            _ => Err(format!("effect \"{word}\" is not one of ask, always_ask, deny")),
        }
    }
}

/// One rule as it is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternRule {
    /// A tool name, a prefix with a star (`fs.*`, `term.r*`) or `*`: the same words as in a skill's list of tools.
    pub tool: String,
    /// The pattern for the call's target (see the module); `*` when the file gave none.
    pub target_glob: String,
    pub effect: Effect,
    /// The person's own words for why, shown in the question or the refusal.
    pub reason: Option<String>,
}

/// What a rule file looks like on disk: `{ "rules": [ { "tool": ..., "target_glob": ..., "effect": ..., "reason": ... } ] }`. Unknown keys are an error, so a typo cannot silently drop a rule.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleFile {
    #[serde(default)]
    pub rules: Vec<RuleEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleEntry {
    pub tool: String,
    #[serde(default)]
    pub target_glob: Option<String>,
    pub effect: String,
    #[serde(default)]
    pub reason: Option<String>,
}

impl RuleFile {
    /// The rules of the file, or the first thing wrong with it, naming the rule by its number.
    pub fn into_rule_set(self) -> Result<RuleSet, String> {
        if self.rules.len() > MAX_RULES {
            return Err(format!("{} rules is more than the {MAX_RULES} a file may hold", self.rules.len()));
        }
        let mut rules = Vec::with_capacity(self.rules.len());
        for (i, entry) in self.rules.into_iter().enumerate() {
            let n = i + 1;
            let tool = entry.tool.trim().to_string();
            if !valid_tool_pattern(&tool) {
                return Err(format!("rule {n}: tool \"{tool}\" is not a tool name, a prefix with a star (fs.*) or *"));
            }
            let target_glob = entry.target_glob.map(|g| g.trim().to_string()).filter(|g| !g.is_empty()).unwrap_or_else(|| "*".to_string());
            if target_glob.chars().count() > MAX_PATTERN_CHARS {
                return Err(format!("rule {n}: target_glob is longer than {MAX_PATTERN_CHARS} characters"));
            }
            let effect = Effect::parse(&entry.effect).map_err(|why| format!("rule {n}: {why}"))?;
            let reason = entry.reason.map(|r| r.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|r| !r.is_empty());
            if reason.as_ref().is_some_and(|r| r.chars().count() > MAX_REASON_CHARS) {
                return Err(format!("rule {n}: reason is longer than {MAX_REASON_CHARS} characters"));
            }
            rules.push(PatternRule { tool, target_glob, effect, reason });
        }
        Ok(RuleSet { rules, broken: None })
    }
}

/// `fs.read`, `fs.*`, `term.r*`, `*`: lower-case letters, digits, `_` and `.`, with a star only at the end.
fn valid_tool_pattern(p: &str) -> bool {
    if p == "*" {
        return true;
    }
    let body = p.strip_suffix('*').unwrap_or(p);
    !body.is_empty() && body.len() <= 128 && body.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.') && body.chars().next().is_some_and(|c| c.is_ascii_lowercase())
}

/// What a rule said about a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub effect: Effect,
    /// A sentence for the question or the refusal.
    pub why: String,
}

/// The rules in force. Cheap to ask when empty, which is how most people run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleSet {
    rules: Vec<PatternRule>,
    /// The file could not be used; the reason. Every call that is not a read asks.
    broken: Option<String>,
}

impl RuleSet {
    /// No rules: nothing is asked or refused by this layer.
    pub fn none() -> Self {
        Self::default()
    }

    /// A file that cannot be used. Its rules are not known, so rather than drop them every call that is not a read asks until it is fixed.
    pub fn broken(why: impl Into<String>) -> Self {
        Self { rules: Vec::new(), broken: Some(why.into()) }
    }

    pub fn from_rules(rules: Vec<PatternRule>) -> Self {
        Self { rules, broken: None }
    }

    /// Reads the text of a rules file (JSON, already free of comments).
    pub fn parse(text: &str) -> Result<Self, String> {
        let file: RuleFile = serde_json::from_str(text).map_err(|e| format!("not valid: {e}"))?;
        file.into_rule_set()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.broken.is_none()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn rules(&self) -> &[PatternRule] {
        &self.rules
    }

    pub fn is_broken(&self) -> Option<&str> {
        self.broken.as_deref()
    }

    /// What the rules say about a call to `tool` whose subjects are `subjects` (see [`subjects_of`]); `changes` is whether the call can change something. `None`: no rule applies.
    /// The strictest matching rule wins, so a refusal beats a question whatever the order of the file.
    pub fn judge(&self, tool: &str, subjects: &[String], changes: bool) -> Option<Hit> {
        if let Some(why) = &self.broken {
            return changes.then(|| Hit { effect: Effect::AlwaysAsk, why: format!("data/tools/permissions.json is wrong ({why}), so every call that is not a read asks until it is fixed") });
        }
        let mut best: Option<&PatternRule> = None;
        for rule in &self.rules {
            if !tool_pattern_matches(&rule.tool, tool) || !subjects.iter().any(|s| glob_matches(&rule.target_glob, s)) {
                continue;
            }
            if best.is_none_or(|b| rule.effect > b.effect) {
                best = Some(rule);
            }
        }
        best.map(|rule| {
            let because = rule.reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default();
            let what = match rule.effect {
                Effect::Deny => "refuses this",
                Effect::AlwaysAsk => "asks every time for this",
                Effect::Ask => "asks before this",
            };
            Hit { effect: rule.effect, why: format!("your rule ({} {}) in permissions.json {what}{because}", rule.tool, rule.target_glob) }
        })
    }
}

/// What a call is matched on: the target the provider names, and where the call has them its `url` and its command line. Each is [`normalize`]d; none is empty.
pub fn subjects_of(target: &str, args: &Value) -> Vec<String> {
    let mut subjects = vec![normalize(target)];
    if let Some(url) = args.get("url").and_then(Value::as_str) {
        subjects.push(normalize(url));
    }
    if let Some(line) = command_line(args) {
        subjects.push(normalize(&line));
    }
    subjects.retain(|s| !s.is_empty());
    subjects.dedup();
    subjects
}

/// The command line of a call of `term.run`, `term.start` (`exe` and `args`) or `term.shell` (`command`).
fn command_line(args: &Value) -> Option<String> {
    if let Some(command) = args.get("command").and_then(Value::as_str) {
        return Some(command.to_string());
    }
    let exe = args.get("exe").and_then(Value::as_str)?;
    let rest = args.get("args").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")).unwrap_or_default();
    Some(format!("{exe} {rest}"))
}

/// The text a pattern is matched on: lower case, `\` as `/`, a run of spaces as one, no space at either end, and no Windows extended-path prefix (`\\?\`).
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.extend(if c == '\\' { '/' } else { c }.to_lowercase());
    }
    match out.strip_prefix("//?/") {
        Some(rest) if !rest.is_empty() => rest.to_string(),
        _ => out,
    }
}

/// Does `pattern` match `text`? Both are [`normalize`]d here, so a caller may pass them as they are. `text` already normalised is not changed by this.
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    let pattern = normalize(pattern);
    let text = normalize(text);
    if wildcard(&pattern, &text) {
        return true;
    }
    // `folder/**` is the folder and what is inside it.
    pattern.strip_suffix("/**").is_some_and(|folder| wildcard(folder, &text))
}

/// `*` (and `**`) any run of characters, `?` one character, anything else itself. Linear in the text for each star, with no recursion.
fn wildcard(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) && p[pi] != '*' {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn set(text: &str) -> RuleSet {
        RuleSet::parse(text).unwrap_or_else(|e| panic!("{e}"))
    }

    fn subjects(target: &str) -> Vec<String> {
        subjects_of(target, &json!({}))
    }

    #[test]
    fn a_star_is_any_run_of_characters_a_slash_too_and_the_case_does_not_matter() {
        assert!(glob_matches("C:/Users/me/Documents/**", "c:/users/ME/documents/tax/2025.pdf"));
        assert!(glob_matches("*.example.com:*", "api.example.com:443"));
        assert!(!glob_matches("*.example.com:*", "example.org:443"));
        assert!(glob_matches("*git push*", "git push origin main"));
        assert!(glob_matches("a?c", "abc") && !glob_matches("a?c", "ac") && !glob_matches("a?c", "abbc"));
        assert!(glob_matches("*", "") && glob_matches("*", "anything at all"));
        assert!(!glob_matches("", "x") && glob_matches("", ""));
        // `**` is two stars: the same as one.
        assert!(glob_matches("a/**/c", "a/b/b/c") && glob_matches("a/**/c", "a//c"));
    }

    #[test]
    fn a_pattern_that_ends_in_slash_star_star_is_the_folder_and_what_is_in_it() {
        assert!(glob_matches("c:/work/secret/**", "c:/work/secret"), "the folder itself");
        assert!(glob_matches("c:/work/secret/**", "c:/work/secret/a/b.txt"));
        assert!(!glob_matches("c:/work/secret/**", "c:/work/secretary/a.txt"), "a folder whose name only starts the same is not inside it");
        assert!(!glob_matches("c:/work/secret/**", "c:/work"));
    }

    #[test]
    fn the_spelling_of_a_windows_path_does_not_hide_it_from_a_rule() {
        for spelling in ["C:\\Users\\me\\Documents\\tax.pdf", "c:/users/me/documents/tax.pdf", "\\\\?\\C:\\Users\\me\\Documents\\tax.pdf", "C:\\Users\\me\\Documents\\\\tax.pdf"] {
            assert!(glob_matches("C:\\Users\\me\\Documents\\**", spelling), "{spelling}");
            assert!(glob_matches("c:/users/me/documents/**", spelling), "{spelling}");
        }
        assert_eq!(normalize("  git   push\t--force  "), "git push --force");
        assert_eq!(normalize("\\\\?\\C:\\a"), "c:/a");
        assert_eq!(normalize("//?/"), "//?/", "a prefix with nothing after it is left as it is");
    }

    #[test]
    fn a_pattern_made_of_many_stars_does_not_take_forever() {
        let pattern = format!("{}b", "a*".repeat(40));
        let text = "a".repeat(2_000);
        let started = std::time::Instant::now();
        assert!(!glob_matches(&pattern, &text));
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
    }

    #[test]
    fn a_rule_file_can_hold_rules_with_and_without_a_target_and_a_reason() {
        let rules = set(r#"{ "rules": [
            { "tool": "fs.*", "target_glob": "c:/tax/**", "effect": "deny", "reason": "my tax papers" },
            { "tool": "term.*", "target_glob": "*git push*", "effect": "ask" },
            { "tool": "http.request", "effect": "always_ask" }
        ] }"#);
        assert_eq!(rules.len(), 3);
        assert_eq!(rules.rules()[2].target_glob, "*", "no target means any");
        assert_eq!(rules.rules()[0].reason.as_deref(), Some("my tax papers"));
        assert!(set("{}").is_empty() && set(r#"{ "rules": [] }"#).is_empty());
    }

    #[test]
    fn a_rules_file_cannot_allow_anything() {
        for word in ["allow", "Allow", "auto", "permit"] {
            let err = RuleSet::parse(&format!(r#"{{ "rules": [ {{ "tool": "fs.*", "effect": "{word}" }} ] }}"#)).unwrap_err();
            assert!(err.contains("rule 1") && err.contains("never allow anything"), "{word}: {err}");
        }
        let err = RuleSet::parse(r#"{ "rules": [ { "tool": "fs.*", "effect": "maybe" } ] }"#).unwrap_err();
        assert!(err.contains("not one of ask, always_ask, deny"), "{err}");
    }

    #[test]
    fn a_file_with_a_typo_or_a_wrong_tool_is_refused_whole_and_names_the_rule() {
        for (text, needle) in [
            (r#"{ "rule": [] }"#, "unknown field"),
            (r#"{ "rules": [ { "tool": "fs.*", "effect": "deny", "glob": "x" } ] }"#, "unknown field"),
            (r#"{ "rules": [ { "tool": "FS.read", "effect": "deny" } ] }"#, "rule 1: tool"),
            (r#"{ "rules": [ { "tool": "fs.*", "effect": "deny" }, { "tool": "", "effect": "deny" } ] }"#, "rule 2: tool"),
            (r#"{ "rules": [ { "tool": "f*s.read", "effect": "deny" } ] }"#, "rule 1: tool"),
            (r#"{ "rules": [ { "tool": "fs.read" } ] }"#, "missing field"),
        ] {
            let err = RuleSet::parse(text).unwrap_err();
            assert!(err.contains(needle), "{text}: {err}");
        }
        let long = "x".repeat(MAX_PATTERN_CHARS + 1);
        assert!(RuleSet::parse(&format!(r#"{{ "rules": [ {{ "tool": "fs.*", "target_glob": "{long}", "effect": "deny" }} ] }}"#)).unwrap_err().contains("target_glob is longer"));
        let many = vec![r#"{ "tool": "fs.*", "effect": "ask" }"#; MAX_RULES + 1].join(",");
        assert!(RuleSet::parse(&format!(r#"{{ "rules": [ {many} ] }}"#)).unwrap_err().contains("more than"));
    }

    #[test]
    fn the_strictest_matching_rule_wins_whatever_the_order_of_the_file() {
        let a = set(
            r#"{ "rules": [ { "tool": "fs.*", "target_glob": "*", "effect": "ask" }, { "tool": "fs.write", "target_glob": "*.env", "effect": "deny", "reason": "no secrets" }, { "tool": "fs.*", "target_glob": "*", "effect": "always_ask" } ] }"#,
        );
        let hit = a.judge("fs.write", &subjects("c:/p/.env"), true).unwrap();
        assert_eq!(hit.effect, Effect::Deny);
        assert!(hit.why.contains("refuses this: no secrets"), "{}", hit.why);
        assert_eq!(a.judge("fs.write", &subjects("c:/p/a.txt"), true).unwrap().effect, Effect::AlwaysAsk, "of the two that match, the stricter");
        let reversed = set(
            r#"{ "rules": [ { "tool": "fs.*", "target_glob": "*", "effect": "always_ask" }, { "tool": "fs.write", "target_glob": "*.env", "effect": "deny" }, { "tool": "fs.*", "target_glob": "*", "effect": "ask" } ] }"#,
        );
        assert_eq!(reversed.judge("fs.write", &subjects("c:/p/.env"), true).unwrap().effect, Effect::Deny);
        assert_eq!(a.judge("web.fetch", &subjects("example.com:443"), false), None, "another tool");
    }

    #[test]
    fn a_rule_matches_the_url_and_the_command_line_as_well_as_the_target() {
        let rules = set(r#"{ "rules": [
            { "tool": "http.*", "target_glob": "https://api.example.com/admin/*", "effect": "deny" },
            { "tool": "term.*", "target_glob": "*git* push*", "effect": "ask" }
        ] }"#);
        let http = subjects_of("api.example.com:443", &json!({ "url": "https://API.example.com/admin/users", "method": "GET" }));
        assert_eq!(rules.judge("http.request", &http, false).unwrap().effect, Effect::Deny);
        let other = subjects_of("api.example.com:443", &json!({ "url": "https://api.example.com/public" }));
        assert!(rules.judge("http.request", &other, false).is_none());
        let run = subjects_of("C:/Program Files/Git/cmd/git.exe", &json!({ "exe": "git", "args": ["push", "--force", "origin"] }));
        assert_eq!(rules.judge("term.run", &run, true).unwrap().effect, Effect::Ask);
        let shell = subjects_of("git status && git push", &json!({ "command": "git status && git   push" }));
        assert_eq!(rules.judge("term.shell", &shell, true).unwrap().effect, Effect::Ask);
        let status = subjects_of("C:/Program Files/Git/cmd/git.exe", &json!({ "exe": "git", "args": ["status"] }));
        assert!(rules.judge("term.run", &status, true).is_none());
    }

    #[test]
    fn a_broken_file_makes_every_call_that_changes_something_ask_and_leaves_a_read_alone() {
        let broken = RuleSet::broken("line 3: expected a comma");
        assert!(!broken.is_empty() && broken.is_broken().is_some());
        let hit = broken.judge("fs.write", &subjects("c:/p/a.txt"), true).unwrap();
        assert_eq!(hit.effect, Effect::AlwaysAsk);
        assert!(hit.why.contains("permissions.json is wrong") && hit.why.contains("line 3"), "{}", hit.why);
        assert_eq!(broken.judge("fs.read", &subjects("c:/p/a.txt"), false), None);
        assert_eq!(RuleSet::none().judge("fs.write", &subjects("x"), true), None);
    }

    #[test]
    fn an_effect_maps_onto_the_decision_ladder_and_never_below_ask() {
        assert_eq!((Effect::Ask.decision(), Effect::AlwaysAsk.decision(), Effect::Deny.decision()), (Decision::Ask, Decision::AlwaysAsk, Decision::Deny));
        assert!(Effect::Ask < Effect::AlwaysAsk && Effect::AlwaysAsk < Effect::Deny);
        for e in [Effect::Ask, Effect::AlwaysAsk, Effect::Deny] {
            assert!(e.decision() > Decision::Auto, "{e:?} must hold something back");
            assert_eq!(Effect::parse(e.word()), Ok(e));
        }
    }
}
