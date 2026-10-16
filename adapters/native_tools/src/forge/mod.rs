//! Source-hosting tools (`forge.*`) for GitHub and GitLab: repositories, issues, pull/merge
//! requests, diffs and CI status.
//!
//! * The model names an **account** and a **repository** from `data/tools/forge.json`; it cannot
//!   pick a host, a URL or a token. Repositories must match the account's allow-list.
//! * Tokens live in Credential Manager (`secret`), are sent only to the account's API host, and are
//!   masked in every result. An account with no `secret` reads public data anonymously.
//! * Requests go through the same vetted HTTP engine as `http.request`: addresses are checked
//!   after DNS, redirects are followed by hand, private and Aqua-internal addresses are refused.
//! * Everything other people wrote (issue text, comments, diffs, CI logs) is untrusted output.
//! * Posting (issue, comment, pull request) is `EXTERNAL_EFFECT`: it always asks, only on accounts
//!   with `"write": true`. There is no merge, close, delete or settings tool.

use crate::http::client::{Client, Credential, Request};
use crate::http::policy::{AuthKind, AuthProfile, HttpConfig, NetPolicy};
use crate::secrets::{valid_name, Secrets};
use aqua_tool_host::{
    Annotations, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Github,
    Gitlab,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    /// The name the model uses.
    pub name: String,
    pub kind: Kind,
    /// API base. Defaults to `https://api.github.com` or `https://gitlab.com/api/v4`; set it for GitHub Enterprise or a self-hosted GitLab.
    #[serde(default)]
    pub api_base: Option<String>,
    /// Credential Manager name of the access token. Without one, only public data can be read.
    #[serde(default)]
    pub secret: Option<String>,
    /// Repositories this account may touch: `owner/name` or `owner/*`. A bare `*` is refused.
    pub repos: Vec<String>,
    /// Allow posting issues, comments and pull requests (each still always asks).
    #[serde(default)]
    pub write: bool,
    /// The API host is on a private network (self-hosted). Off by default.
    #[serde(default)]
    pub allow_private: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgeConfig {
    #[serde(default)]
    pub accounts: Vec<Account>,
}

fn segment_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 100 && !s.starts_with(['-', '.']) && s != ".." && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '*'))
}

/// `owner/name` (GitLab: `group/subgroup/name`), no traversal, no leading `-`.
pub fn repo_ok(r: &str) -> bool {
    let parts: Vec<&str> = r.split('/').collect();
    (2..=5).contains(&parts.len()) && parts.iter().all(|p| segment_ok(p) && !p.contains('*'))
}

/// Does `repo` match an allow-list pattern (`*` matches within one segment)?
pub fn repo_matches(pattern: &str, repo: &str) -> bool {
    let (p, r): (Vec<&str>, Vec<&str>) = (pattern.split('/').collect(), repo.split('/').collect());
    if p.len() != r.len() {
        return false;
    }
    p.iter().zip(r.iter()).all(|(pp, rr)| {
        if *pp == "*" {
            return true;
        }
        match pp.split_once('*') {
            None => pp.eq_ignore_ascii_case(rr),
            Some((a, b)) => rr.len() >= a.len() + b.len() && rr.to_ascii_lowercase().starts_with(&a.to_ascii_lowercase()) && rr.to_ascii_lowercase().ends_with(&b.to_ascii_lowercase()),
        }
    })
}

impl Account {
    pub fn base(&self) -> String {
        match (&self.api_base, self.kind) {
            (Some(b), _) => b.trim_end_matches('/').to_string(),
            (None, Kind::Github) => "https://api.github.com".into(),
            (None, Kind::Gitlab) => "https://gitlab.com/api/v4".into(),
        }
    }

    fn host_port(&self) -> Result<(String, u16, bool), String> {
        let base = self.base();
        let (scheme, rest) = base.split_once("://").ok_or_else(|| format!("account '{}': api_base must start with https://", self.name))?;
        if scheme != "https" && !(scheme == "http" && self.allow_private) {
            return Err(format!("account '{}': api_base must be https (http only with allow_private)", self.name));
        }
        let authority = rest.split('/').next().unwrap_or("");
        if authority.is_empty() || authority.contains('@') || authority.contains(' ') || authority.contains('\\') {
            return Err(format!("account '{}': api_base host is not valid", self.name));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !h.ends_with(']') || (authority.starts_with('[') && authority.contains("]:")) => (h.to_string(), p.parse::<u16>().map_err(|_| "bad port".to_string())?),
            _ => (authority.to_string(), if scheme == "https" { 443 } else { 80 }),
        };
        Ok((host, port, scheme == "https"))
    }
}

impl ForgeConfig {
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for a in &self.accounts {
            if a.name.is_empty() || a.name.len() > 60 || !a.name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
                return Err(format!("account name '{}' may only use letters, digits and - _ .", a.name));
            }
            if !seen.insert(a.name.clone()) {
                return Err(format!("account '{}' is listed twice", a.name));
            }
            if let Some(s) = &a.secret {
                if !valid_name(s) {
                    return Err(format!("account '{}': '{s}' is not a valid secret name", a.name));
                }
            }
            if a.repos.is_empty() {
                return Err(format!("account '{}' needs at least one repository pattern", a.name));
            }
            for p in &a.repos {
                let parts: Vec<&str> = p.split('/').collect();
                let ok = (2..=5).contains(&parts.len()) && parts.iter().all(|s| segment_ok(s)) && !parts.iter().all(|s| *s == "*") && parts[0] != "*";
                if !ok {
                    return Err(format!("account '{}': '{p}' is not a repository pattern (owner/name or owner/*; the owner cannot be *)", a.name));
                }
            }
            a.host_port()?;
        }
        Ok(())
    }
}

struct Inner {
    cfg: ForgeConfig,
    secrets: Arc<dyn Secrets>,
}

pub struct ForgeProvider(Arc<Inner>);

impl ForgeProvider {
    pub fn new(cfg: ForgeConfig, secrets: Arc<dyn Secrets>) -> Result<Self, String> {
        cfg.validate()?;
        Ok(Self(Arc::new(Inner { cfg, secrets })))
    }
}

fn bad(m: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArguments, m)
}

fn s<'a>(a: &'a Value, k: &str) -> &'a str {
    a.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Accounts,
    Repo,
    Issues,
    Issue,
    Prs,
    Pr,
    PrDiff,
    Checks,
    IssueCreate,
    Comment,
    PrCreate,
}

fn specs() -> Vec<(&'static str, &'static str, Op, Value, Vec<&'static str>)> {
    let acct = json!({"type": "string", "minLength": 1, "maxLength": 60});
    let repo = json!({"type": "string", "minLength": 3, "maxLength": 300, "description": "owner/name"});
    let num = json!({"type": "integer", "minimum": 1, "maximum": 1_000_000_000});
    let state = json!({"type": "string", "enum": ["open", "closed", "all"], "default": "open"});
    let limit = json!({"type": "integer", "minimum": 1, "maximum": 50, "default": 20});
    vec![
        ("forge.accounts", "List the configured GitHub/GitLab accounts, the repositories each may use, and whether posting is enabled.", Op::Accounts, json!({}), vec![]),
        ("forge.repo", "Show a repository: description, default branch, visibility, open issue count.", Op::Repo, json!({"account": acct.clone(), "repo": repo.clone()}), vec!["account", "repo"]),
        ("forge.issues", "List issues (not pull requests) of a repository.", Op::Issues, json!({"account": acct.clone(), "repo": repo.clone(), "state": state.clone(), "labels": {"type": "string", "maxLength": 200}, "limit": limit.clone()}), vec!["account", "repo"]),
        ("forge.issue", "Read one issue with its first comments.", Op::Issue, json!({"account": acct.clone(), "repo": repo.clone(), "number": num.clone()}), vec!["account", "repo", "number"]),
        ("forge.prs", "List pull requests (GitLab: merge requests) of a repository.", Op::Prs, json!({"account": acct.clone(), "repo": repo.clone(), "state": state, "limit": limit}), vec!["account", "repo"]),
        ("forge.pr", "Read one pull request: description, branches, status and the list of changed files.", Op::Pr, json!({"account": acct.clone(), "repo": repo.clone(), "number": num.clone()}), vec!["account", "repo", "number"]),
        ("forge.pr_diff", "Read the diff of a pull request, cut to max_kb.", Op::PrDiff, json!({"account": acct.clone(), "repo": repo.clone(), "number": num.clone(), "max_kb": {"type": "integer", "minimum": 1, "maximum": 256, "default": 64}}), vec!["account", "repo", "number"]),
        ("forge.checks", "CI status for a branch or commit: check runs (GitHub) or pipelines (GitLab).", Op::Checks, json!({"account": acct.clone(), "repo": repo.clone(), "ref": {"type": "string", "maxLength": 200, "default": "HEAD"}}), vec!["account", "repo"]),
        ("forge.issue_create", "Open a new issue. Always asks; needs write enabled on the account.", Op::IssueCreate, json!({"account": acct.clone(), "repo": repo.clone(), "title": {"type": "string", "minLength": 1, "maxLength": 256}, "body": {"type": "string", "maxLength": 20000}, "labels": {"type": "string", "maxLength": 200}}), vec!["account", "repo", "title"]),
        ("forge.comment", "Comment on an issue or pull request. Always asks; needs write enabled.", Op::Comment, json!({"account": acct.clone(), "repo": repo.clone(), "number": num, "on": {"type": "string", "enum": ["issue", "pr"], "default": "issue"}, "body": {"type": "string", "minLength": 1, "maxLength": 20000}}), vec!["account", "repo", "number", "body"]),
        ("forge.pr_create", "Open a pull request from an existing branch. Always asks; needs write enabled.", Op::PrCreate, json!({"account": acct, "repo": repo, "head": {"type": "string", "minLength": 1, "maxLength": 200}, "base": {"type": "string", "minLength": 1, "maxLength": 200}, "title": {"type": "string", "minLength": 1, "maxLength": 256}, "body": {"type": "string", "maxLength": 20000}}), vec!["account", "repo", "head", "base", "title"]),
    ]
}

fn branch_ok(b: &str) -> bool {
    !b.is_empty() && b.len() <= 200 && !b.starts_with(['-', '/']) && !b.contains("..") && !b.contains("//") && b.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | '#' | '+' | '@' | ':'))
}

fn labels_ok(l: &str) -> bool {
    l.is_empty() || (l.split(',').count() <= 10 && l.split(',').all(|x| !x.is_empty() && x.len() <= 50 && x.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '.' | ':' | '/' | '-'))))
}

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

struct Ctx<'a> {
    acct: &'a Account,
    repo: String,
}

impl Inner {
    fn account(&self, a: &Value) -> Result<&Account, ToolError> {
        let name = s(a, "account");
        self.cfg.accounts.iter().find(|x| x.name == name).ok_or_else(|| {
            ToolError::new(ErrorCode::NotFound, format!("no configured account '{name}'. Configured: {}", self.cfg.accounts.iter().map(|x| x.name.as_str()).collect::<Vec<_>>().join(", ")))
        })
    }

    fn ctx<'a>(&'a self, a: &Value) -> Result<Ctx<'a>, ToolError> {
        let acct = self.account(a)?;
        let repo = s(a, "repo").trim_matches('/').to_string();
        if !repo_ok(&repo) {
            return Err(bad(format!("'{repo}' is not a repository name (owner/name)")));
        }
        if !acct.repos.iter().any(|p| repo_matches(p, &repo)) {
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("'{repo}' is not allowed for account '{}'. Allowed: {}", acct.name, acct.repos.join(", "))));
        }
        if acct.kind == Kind::Github && repo.split('/').count() != 2 {
            return Err(bad("a GitHub repository is owner/name"));
        }
        Ok(Ctx { acct, repo })
    }

    fn project(&self, c: &Ctx) -> String {
        match c.acct.kind {
            Kind::Github => format!("/repos/{}", c.repo),
            Kind::Gitlab => format!("/projects/{}", enc(&c.repo)),
        }
    }

    fn client(&self, acct: &Account) -> Result<(Client, Option<Credential>, Vec<String>), ToolError> {
        let (host, port, _) = acct.host_port().map_err(|e| ToolError::new(ErrorCode::InvalidArguments, e))?;
        let hp = if port == 443 || port == 80 { host.clone() } else { format!("{host}:{port}") };
        let cfg = HttpConfig {
            allow: vec![hp.clone()],
            allow_private: if acct.allow_private { vec![format!("{host}:{port}")] } else { vec![] },
            unknown_hosts: "deny".into(),
            ..HttpConfig::default()
        };
        let mut masked = Vec::new();
        let cred = match &acct.secret {
            None => None,
            Some(name) => {
                let token = self
                    .secrets
                    .get(name)
                    .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("credential store: {e}")))?
                    .ok_or_else(|| ToolError::new(ErrorCode::AuthRequired, format!("the token for '{}' is not stored. Store it yourself with: aqua-cli secret-set {name}", acct.name)))?;
                masked.push(token.clone());
                let (header, value) = match acct.kind {
                    Kind::Github => ("Authorization".to_string(), format!("Bearer {token}")),
                    Kind::Gitlab => ("PRIVATE-TOKEN".to_string(), token),
                };
                Some(Credential { header, value, allowed: AuthProfile { kind: AuthKind::Header, secret: name.clone(), hosts: vec![hp], name: None } })
            }
        };
        Ok((Client::new(Arc::new(NetPolicy::new(cfg))), cred, masked))
    }

    fn api(&self, c: &Ctx, client: &Client, cred: &Option<Credential>, method: &str, path: &str, body: Option<Value>, accept: Option<&str>, cancel: &AtomicBool, max_bytes: usize) -> Result<(u16, Vec<u8>, bool), ToolError> {
        let mut headers = vec![("Accept".to_string(), accept.unwrap_or(if c.acct.kind == Kind::Github { "application/vnd.github+json" } else { "application/json" }).to_string())];
        if c.acct.kind == Kind::Github {
            headers.push(("X-GitHub-Api-Version".into(), "2022-11-28".into()));
        }
        let body_bytes = body.as_ref().map(|b| serde_json::to_vec(b).unwrap_or_default());
        if body_bytes.is_some() {
            headers.push(("Content-Type".into(), "application/json".into()));
        }
        let req = Request {
            method: method.into(),
            url: format!("{}{}", c.acct.base(), path),
            headers,
            body: body_bytes,
            credential: cred.clone(),
            timeout: Duration::from_secs(30),
            max_bytes,
            follow_redirects: true,
            max_redirects: 3,
        };
        let r = client.execute(&req, cancel)?;
        match r.status {
            200..=299 => Ok((r.status, r.body, r.truncated)),
            401 => Err(ToolError::new(ErrorCode::AuthFailed, "the forge rejected the token (401). Check that the stored secret is valid and not expired.")),
            403 | 429 => {
                let limited = r.headers.iter().any(|(k, v)| (k.eq_ignore_ascii_case("x-ratelimit-remaining") && v == "0") || k.eq_ignore_ascii_case("retry-after"));
                if limited || r.status == 429 {
                    Err(ToolError::new(ErrorCode::RateLimited, "the forge's rate limit was hit; wait and retry"))
                } else {
                    Err(ToolError::new(ErrorCode::OsPermissionDenied, "the token is not allowed to do that (403). It may lack the needed scope for this repository."))
                }
            }
            404 => Err(ToolError::new(ErrorCode::NotFound, format!("{} was not found (or the token cannot see it)", path.split('?').next().unwrap_or("")))),
            422 => Err(ToolError::new(ErrorCode::InvalidArguments, format!("the forge rejected the request: {}", first_message(&r.body)))),
            code => Err(ToolError::new(ErrorCode::BackendError, format!("the forge answered {code}: {}", first_message(&r.body)))),
        }
    }
}

fn first_message(body: &[u8]) -> String {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let m = v.get("message").map(|m| m.to_string()).unwrap_or_default();
    let m = m.trim_matches('"');
    m.chars().take(300).collect()
}

fn clip(t: &str, n: usize) -> String {
    if t.chars().count() > n {
        format!("{}… [cut]", t.chars().take(n).collect::<String>())
    } else {
        t.to_string()
    }
}

fn str_of(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

impl ToolProvider for ForgeProvider {
    fn id(&self) -> &str {
        "forge"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        specs()
            .into_iter()
            .map(|(name, description, op, props, required)| {
                let post = matches!(op, Op::IssueCreate | Op::Comment | Op::PrCreate);
                ToolManifest {
                    name: name.into(),
                    title: name.into(),
                    description: description.into(),
                    classes: if post { vec![PermClass::Network, PermClass::Write, PermClass::ExternalEffect] } else if op == Op::Accounts { vec![PermClass::Read] } else { vec![PermClass::Read, PermClass::Network] },
                    default_decision: Decision::Auto,
                    input_schema: json!({"type": "object", "properties": props, "required": required}),
                    timeout_default_ms: 45_000,
                    timeout_max_ms: 90_000,
                    annotations: Annotations { read_only: !post, destructive: false, idempotent: !post, open_world: true },
                    untrusted_output: true,
                }
            })
            .collect()
    }

    fn classify(&self, tool: &str, a: &Value) -> Result<Classification, ToolError> {
        let g = &self.0;
        let (_, _, op, _, _) = specs().into_iter().find(|x| x.0 == tool).ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("unknown tool '{tool}'")))?;
        if op == Op::Accounts {
            return Ok(Classification { classes: vec![PermClass::Read], target: "forge.json".into(), summary: "List forge accounts".into(), target_allowed: true });
        }
        let c = g.ctx(a)?;
        match op {
            Op::Issues | Op::Prs => {
                if !labels_ok(s(a, "labels")) {
                    return Err(bad("labels: up to 10 comma-separated names of plain characters"));
                }
            }
            Op::Checks => {
                let r = a.get("ref").and_then(|v| v.as_str()).unwrap_or("HEAD");
                if !branch_ok(r) {
                    return Err(bad("ref is not a valid branch, tag or commit name"));
                }
            }
            _ => {}
        }
        if matches!(op, Op::IssueCreate | Op::Comment | Op::PrCreate) {
            if !c.acct.write {
                return Err(ToolError::new(ErrorCode::PermissionDenied, format!("posting is not enabled for account '{}'. Set \"write\": true in data/tools/forge.json to allow it (each post will still ask).", c.acct.name)));
            }
            if !labels_ok(s(a, "labels")) {
                return Err(bad("labels: up to 10 comma-separated names of plain characters"));
            }
            if op == Op::PrCreate {
                for k in ["head", "base"] {
                    if !branch_ok(s(a, k)) {
                        return Err(bad(format!("{k} is not a valid branch name")));
                    }
                }
            }
            let what = match op {
                Op::IssueCreate => format!("Open issue \"{}\" in {}/{}", clip(s(a, "title"), 120), c.acct.name, c.repo),
                Op::Comment => format!("Comment on #{} in {}/{}: \"{}\"", a["number"], c.acct.name, c.repo, clip(s(a, "body"), 200)),
                _ => format!("Open pull request \"{}\" ({} into {}) in {}/{}", clip(s(a, "title"), 120), s(a, "head"), s(a, "base"), c.acct.name, c.repo),
            };
            return Ok(Classification { classes: vec![PermClass::Network, PermClass::Write, PermClass::ExternalEffect], target: format!("{}/{}", c.acct.name, c.repo), summary: what, target_allowed: false });
        }
        Ok(Classification { classes: vec![PermClass::Read, PermClass::Network], target: format!("{}/{}", c.acct.name, c.repo), summary: format!("{tool} {}/{}", c.acct.name, c.repo), target_allowed: true })
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let inner = self.0.clone();
        let cancel = ctx.cancel.clone();
        let tool = tool.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || inner.dispatch(&tool, &args, &cancel)).await.map_err(|e| ToolError::new(ErrorCode::BackendError, format!("forge task failed: {e}")))?
        })
    }
}

impl Inner {
    fn dispatch(&self, tool: &str, a: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        let (_, _, op, _, _) = specs().into_iter().find(|x| x.0 == tool).ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("unknown tool '{tool}'")))?;
        if op == Op::Accounts {
            let rows: Vec<Value> = self.cfg.accounts.iter().map(|x| json!({"account": x.name, "kind": x.kind, "repos": x.repos, "write": x.write, "authenticated": x.secret.is_some()})).collect();
            let text = self.cfg.accounts.iter().map(|x| format!("{} ({:?}, {}) repos: {}; posting {}", x.name, x.kind, if x.secret.is_some() { "token" } else { "anonymous" }, x.repos.join(", "), if x.write { "enabled" } else { "off" })).collect::<Vec<_>>().join("\n");
            return Ok(ProviderOutput { data: json!({ "accounts": rows }), text: if text.is_empty() { "No accounts are configured. Add them to data/tools/forge.json.".into() } else { text }, ..Default::default() });
        }
        let c = self.ctx(a)?;
        let (client, cred, masked) = self.client(c.acct)?;
        let gh = c.acct.kind == Kind::Github;
        let base = self.project(&c);
        let num = a.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
        let limit = a.get("limit").and_then(|v| v.as_u64()).unwrap_or(20).clamp(1, 50);
        let state = match s(a, "state") {
            "closed" => if gh { "closed" } else { "closed" },
            "all" => "all",
            _ => if gh { "open" } else { "opened" },
        };
        let get = |path: &str| -> Result<Value, ToolError> {
            let (_, body, _) = self.api(&c, &client, &cred, "GET", path, None, None, cancel, 2 * 1024 * 1024)?;
            serde_json::from_slice(&body).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the forge did not return JSON: {e}")))
        };
        let (text, data) = match op {
            Op::Repo => {
                let v = get(&base)?;
                let (branch, issues) = if gh { (str_of(&v, "default_branch"), v["open_issues_count"].clone()) } else { (str_of(&v, "default_branch"), v["open_issues_count"].clone()) };
                let name = if gh { str_of(&v, "full_name") } else { str_of(&v, "path_with_namespace") };
                let vis = if gh { if v["private"].as_bool().unwrap_or(false) { "private".to_string() } else { "public".to_string() } } else { str_of(&v, "visibility") };
                (format!("{name}\n{}\ndefault branch: {branch}; {vis}; open issues: {issues}", clip(&str_of(&v, if gh { "description" } else { "description" }), 500)), json!({"name": name, "default_branch": branch, "visibility": vis}))
            }
            Op::Issues => {
                let lab = if s(a, "labels").is_empty() { String::new() } else { format!("&labels={}", enc(s(a, "labels"))) };
                let path = if gh { format!("{base}/issues?state={state}&per_page={limit}{lab}") } else { format!("{base}/issues?state={state}&per_page={limit}{lab}") };
                let v = get(&path)?;
                let items: Vec<&Value> = v.as_array().map(|x| x.iter().filter(|i| i.get("pull_request").is_none()).collect()).unwrap_or_default();
                let rows: Vec<Value> = items.iter().map(|i| json!({"number": if gh { i["number"].clone() } else { i["iid"].clone() }, "title": str_of(i, "title"), "state": str_of(i, "state"), "author": if gh { i["user"]["login"].clone() } else { i["author"]["username"].clone() }})).collect();
                let text = rows.iter().map(|r| format!("#{} [{}] {} (by {})", r["number"], r["state"].as_str().unwrap_or(""), clip(r["title"].as_str().unwrap_or(""), 160), r["author"].as_str().unwrap_or("?"))).collect::<Vec<_>>().join("\n");
                (if text.is_empty() { "No issues.".into() } else { text }, json!({ "issues": rows }))
            }
            Op::Issue => {
                let v = get(&format!("{base}/issues/{num}"))?;
                let notes = get(&if gh { format!("{base}/issues/{num}/comments?per_page=10") } else { format!("{base}/issues/{num}/notes?per_page=10&sort=asc") })?;
                let comments: Vec<String> = notes.as_array().map(|n| n.iter().filter(|x| !x["system"].as_bool().unwrap_or(false)).map(|x| format!("- {}: {}", if gh { x["user"]["login"].as_str().unwrap_or("?").to_string() } else { x["author"]["username"].as_str().unwrap_or("?").to_string() }, clip(&str_of(x, "body"), 1500))).collect()).unwrap_or_default();
                let body = if gh { str_of(&v, "body") } else { str_of(&v, "description") };
                (format!("#{num} {} [{}]\n{}\n\nComments ({}):\n{}", str_of(&v, "title"), str_of(&v, "state"), clip(&body, 4000), comments.len(), comments.join("\n")), json!({"number": num, "title": str_of(&v, "title"), "state": str_of(&v, "state"), "comments": comments.len()}))
            }
            Op::Prs => {
                let st = if gh { state } else if state == "opened" { "opened" } else { state };
                let v = get(&if gh { format!("{base}/pulls?state={st}&per_page={limit}") } else { format!("{base}/merge_requests?state={st}&per_page={limit}") })?;
                let rows: Vec<Value> = v.as_array().map(|x| x.iter().map(|i| json!({"number": if gh { i["number"].clone() } else { i["iid"].clone() }, "title": str_of(i, "title"), "state": str_of(i, "state"), "from": if gh { i["head"]["ref"].clone() } else { i["source_branch"].clone() }, "into": if gh { i["base"]["ref"].clone() } else { i["target_branch"].clone() }})).collect()).unwrap_or_default();
                let text = rows.iter().map(|r| format!("#{} [{}] {} ({} → {})", r["number"], r["state"].as_str().unwrap_or(""), clip(r["title"].as_str().unwrap_or(""), 160), r["from"].as_str().unwrap_or("?"), r["into"].as_str().unwrap_or("?"))).collect::<Vec<_>>().join("\n");
                (if text.is_empty() { "No pull requests.".into() } else { text }, json!({ "prs": rows }))
            }
            Op::Pr => {
                let (v, files) = if gh {
                    (get(&format!("{base}/pulls/{num}"))?, get(&format!("{base}/pulls/{num}/files?per_page=50"))?)
                } else {
                    let m = get(&format!("{base}/merge_requests/{num}/changes"))?;
                    let f = m["changes"].clone();
                    (m, f)
                };
                let names: Vec<String> = files.as_array().map(|f| f.iter().map(|x| if gh { format!("{} ({}, +{} -{})", str_of(x, "filename"), str_of(x, "status"), x["additions"], x["deletions"]) } else { str_of(x, "new_path") }).collect()).unwrap_or_default();
                let body = if gh { str_of(&v, "body") } else { str_of(&v, "description") };
                (
                    format!("#{num} {} [{}] {} → {}\n{}\n\nChanged files ({}):\n{}", str_of(&v, "title"), str_of(&v, "state"), if gh { v["head"]["ref"].as_str().unwrap_or("").to_string() } else { str_of(&v, "source_branch") }, if gh { v["base"]["ref"].as_str().unwrap_or("").to_string() } else { str_of(&v, "target_branch") }, clip(&body, 3000), names.len(), names.join("\n")),
                    json!({"number": num, "title": str_of(&v, "title"), "state": str_of(&v, "state"), "files": names.len()}),
                )
            }
            Op::PrDiff => {
                let cap = a.get("max_kb").and_then(|v| v.as_u64()).unwrap_or(64).clamp(1, 256) as usize * 1024;
                let text = if gh {
                    let (_, body, trunc) = self.api(&c, &client, &cred, "GET", &format!("{base}/pulls/{num}"), None, Some("application/vnd.github.diff"), cancel, cap)?;
                    let t = String::from_utf8_lossy(&body).to_string();
                    if trunc { format!("{t}\n… diff cut at {} KB", cap / 1024) } else { t }
                } else {
                    let m = get(&format!("{base}/merge_requests/{num}/changes"))?;
                    let mut out = String::new();
                    for ch in m["changes"].as_array().map(|x| x.as_slice()).unwrap_or(&[]) {
                        out.push_str(&format!("--- {}\n+++ {}\n{}\n", str_of(ch, "old_path"), str_of(ch, "new_path"), str_of(ch, "diff")));
                        if out.len() > cap {
                            out.truncate(cap);
                            out.push_str("\n… diff cut");
                            break;
                        }
                    }
                    out
                };
                (if text.trim().is_empty() { "The diff is empty.".into() } else { text }, json!({"number": num}))
            }
            Op::Checks => {
                let r = a.get("ref").and_then(|v| v.as_str()).filter(|x| !x.is_empty()).unwrap_or("HEAD");
                if gh {
                    let v = get(&format!("{base}/commits/{}/check-runs?per_page=30", enc(r)))?;
                    let rows: Vec<String> = v["check_runs"].as_array().map(|x| x.iter().map(|i| format!("{}: {} ({})", str_of(i, "name"), str_of(i, "status"), i["conclusion"].as_str().unwrap_or("pending"))).collect()).unwrap_or_default();
                    (if rows.is_empty() { "No check runs.".into() } else { rows.join("\n") }, json!({"checks": rows.len()}))
                } else {
                    let v = get(&format!("{base}/pipelines?ref={}&per_page=10", enc(if r == "HEAD" { "main" } else { r })))?;
                    let rows: Vec<String> = v.as_array().map(|x| x.iter().map(|i| format!("pipeline {}: {} ({})", i["id"], str_of(i, "status"), str_of(i, "ref"))).collect()).unwrap_or_default();
                    (if rows.is_empty() { "No pipelines.".into() } else { rows.join("\n") }, json!({"pipelines": rows.len()}))
                }
            }
            Op::IssueCreate => {
                let labels = s(a, "labels");
                let body = if gh { json!({"title": s(a, "title"), "body": s(a, "body"), "labels": labels.split(',').filter(|x| !x.is_empty()).collect::<Vec<_>>()}) } else { json!({"title": s(a, "title"), "description": s(a, "body"), "labels": labels}) };
                let (_, resp, _) = self.api(&c, &client, &cred, "POST", &format!("{base}/issues"), Some(body), None, cancel, 512 * 1024)?;
                let v: Value = serde_json::from_slice(&resp).unwrap_or(Value::Null);
                let n = if gh { v["number"].clone() } else { v["iid"].clone() };
                (format!("Opened issue #{n}: {}", str_of(&v, if gh { "html_url" } else { "web_url" })), json!({"number": n}))
            }
            Op::Comment => {
                let on_pr = s(a, "on") == "pr";
                let path = match (gh, on_pr) {
                    (true, _) => format!("{base}/issues/{num}/comments"),
                    (false, true) => format!("{base}/merge_requests/{num}/notes"),
                    (false, false) => format!("{base}/issues/{num}/notes"),
                };
                self.api(&c, &client, &cred, "POST", &path, Some(json!({"body": s(a, "body")})), None, cancel, 512 * 1024)?;
                (format!("Commented on #{num}."), json!({"number": num}))
            }
            Op::PrCreate => {
                let body = if gh { json!({"title": s(a, "title"), "head": s(a, "head"), "base": s(a, "base"), "body": s(a, "body")}) } else { json!({"title": s(a, "title"), "source_branch": s(a, "head"), "target_branch": s(a, "base"), "description": s(a, "body")}) };
                let (_, resp, _) = self.api(&c, &client, &cred, "POST", &if gh { format!("{base}/pulls") } else { format!("{base}/merge_requests") }, Some(body), None, cancel, 512 * 1024)?;
                let v: Value = serde_json::from_slice(&resp).unwrap_or(Value::Null);
                let n = if gh { v["number"].clone() } else { v["iid"].clone() };
                (format!("Opened pull request #{n}: {}", str_of(&v, if gh { "html_url" } else { "web_url" })), json!({"number": n}))
            }
            Op::Accounts => unreachable!(),
        };
        Ok(ProviderOutput { data, text, secrets: masked, ..Default::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(kind: Kind, repos: &[&str]) -> Account {
        Account { name: "a".into(), kind, api_base: None, secret: None, repos: repos.iter().map(|s| s.to_string()).collect(), write: false, allow_private: false }
    }

    #[test]
    fn repository_names_and_patterns() {
        for ok in ["owner/name", "Owner/na.me_x-1", "group/sub/name"] {
            assert!(repo_ok(ok), "{ok}");
        }
        for bad in ["", "owner", "owner/", "/name", "a/../b", "-o/x", "a/.hidden", "a/b c", "a/b?x=1", "a/b#f", "a/b%2F", "https://x/y", "a/b/c/d/e/f", "a/*"] {
            assert!(!repo_ok(bad), "{bad}");
        }
        assert!(repo_matches("VisKush/*", "viskush/aqua"));
        assert!(repo_matches("VisKush/aqua_assistant", "VisKush/aqua_assistant"));
        assert!(repo_matches("org/svc-*", "org/svc-api"));
        assert!(!repo_matches("VisKush/*", "other/aqua"));
        assert!(!repo_matches("VisKush/*", "VisKush/a/b"), "a star does not cross a segment");
        assert!(!repo_matches("org/svc-*", "org/api-svc"));
    }

    #[test]
    fn config_rejects_wildcard_owners_bad_bases_and_secrets_in_the_file() {
        let ok = ForgeConfig { accounts: vec![Account { secret: Some("github/token".into()), ..acct(Kind::Github, &["me/*", "org/repo"]) }] };
        assert!(ok.validate().is_ok());
        for repos in [vec!["*"], vec!["*/*"], vec!["*/repo"], vec!["me"], vec![], vec!["me/../x"], vec!["me/re po"]] {
            assert!(ForgeConfig { accounts: vec![acct(Kind::Github, &repos)] }.validate().is_err(), "{repos:?}");
        }
        let mut a = acct(Kind::Gitlab, &["g/*"]);
        a.api_base = Some("http://gitlab.internal/api/v4".into());
        assert!(ForgeConfig { accounts: vec![a.clone()] }.validate().is_err(), "http needs allow_private");
        a.allow_private = true;
        assert!(ForgeConfig { accounts: vec![a.clone()] }.validate().is_ok());
        for base in ["https://user:pw@evil.example/api", "ftp://x/y", "gitlab.com/api/v4", "https:///x"] {
            a.api_base = Some(base.into());
            assert!(ForgeConfig { accounts: vec![a.clone()] }.validate().is_err(), "{base}");
        }
        assert!(ForgeConfig { accounts: vec![Account { secret: Some("../x".into()), ..acct(Kind::Github, &["a/*"]) }] }.validate().is_err());
        assert!(ForgeConfig { accounts: vec![acct(Kind::Github, &["a/*"]), acct(Kind::Github, &["b/*"])] }.validate().is_err(), "duplicate names");
        assert!(serde_json::from_str::<ForgeConfig>(r#"{"accounts":[{"name":"a","kind":"github","repos":["a/*"],"token":"ghp_x"}]}"#).is_err(), "a token field does not exist");
    }

    #[test]
    fn helpers_encode_and_validate() {
        assert_eq!(enc("group/sub name"), "group%2Fsub%20name");
        assert!(branch_ok("feature/x-1") && branch_ok("v1.2.3") && !branch_ok("-x") && !branch_ok("a..b") && !branch_ok("a b") && !branch_ok(""));
        assert!(labels_ok("bug,help wanted") && !labels_ok("a,,b") && !labels_ok(&"x,".repeat(12)) && !labels_ok("a&b=c"));
        assert_eq!(acct(Kind::Github, &["a/*"]).base(), "https://api.github.com");
        assert_eq!(acct(Kind::Gitlab, &["a/*"]).base(), "https://gitlab.com/api/v4");
    }
}
