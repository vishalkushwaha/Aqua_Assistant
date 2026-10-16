//! `aqua-cli doctor`: checks that the tool side of an install is healthy, without needing Aqua to
//! be running. It is also the first-run setup: it writes the default config files that are
//! missing (with nothing dangerous enabled) and, with `--fix`, creates missing workspace folders.
//!
//! It never prints a secret, never changes an existing config file, and its only side effect on
//! the credential store is a temporary entry it removes again.
//!
//! A second report (`probe_lines`) follows the first: one line `key=value current|drift` for each fact about the two token files (exists, is a plain file, size,
//! not tracked by git, who can open it) and, with `--live`, for one health probe of the running core. The command exits non-zero when any of its lines is `drift`.

use crate::tool_host::*;
use aqua_native_tools::cli::Cli;
use aqua_native_tools::secrets::{CredentialManager, Secrets};
use aqua_tool_host::PolicyFile;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// Fine, just worth knowing (an optional program is not installed).
    Info,
    /// Works, but probably not what you want.
    Warn,
    /// A safety or setup problem that needs fixing.
    Fail,
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
    pub fix: Option<String>,
}

fn c(name: &str, status: Status, detail: impl Into<String>, fix: Option<&str>) -> Check {
    Check { name: name.into(), status, detail: detail.into(), fix: fix.map(String::from) }
}

/// Does this URL carry a login that is a credential? A web address with a user name and a password before the host does, and so does one
/// with a lone token in the user-name place; `ssh://git@host/...` and the scp style `git@host:team/repo.git` (a plain user name) do not.
/// Only the user-info part of a `scheme://` URL is looked at, and it is never returned.
fn url_has_credential(url: &str) -> bool {
    let Some((_, rest)) = url.trim().split_once("://") else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let Some((userinfo, _host)) = authority.rsplit_once('@') else { return false };
    if userinfo.is_empty() {
        return false;
    }
    // `user:password`, or a lone token used as the user name (long, or one of the well-known token prefixes).
    userinfo.contains(':') || userinfo.len() >= 20 || ["glpat-", "ghp_", "gho_", "ghs_", "github_pat_", "xox", "sk-"].iter().any(|p| userinfo.starts_with(p))
}

/// The remotes of a git config (`.git/config`) whose URL holds a credential: the names only, never the values. A `[url "..."]`
/// rewrite rule with a credential in it counts too (it is reported as `url rewrite`).
pub fn remotes_with_credentials(git_config: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut section: Option<String> = None;
    for raw in git_config.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            section = None;
            if let Some(name) = line.strip_prefix("[remote \"").and_then(|r| r.split('"').next()) {
                section = Some(name.to_string());
            } else if let Some(rule) = line.strip_prefix("[url \"").and_then(|r| r.rsplit_once('"')).map(|(r, _)| r) {
                if url_has_credential(rule) && !found.contains(&"url rewrite".to_string()) {
                    found.push("url rewrite".into());
                }
            }
            continue;
        }
        let (Some(name), Some((key, value))) = (section.as_ref(), line.split_once('=')) else { continue };
        if matches!(key.trim().to_ascii_lowercase().as_str(), "url" | "pushurl") && url_has_credential(value) && !found.contains(name) {
            found.push(name.clone());
        }
    }
    found
}

/// A remote whose URL carries a password or token leaves it in `.git/config`, in every clone and backup of the folder, and in some tools'
/// output. Reports which remote, never the value. `repo_root` is the folder that holds `.git`.
pub fn check_remote_credentials(repo_root: &Path) -> Check {
    let config = repo_root.join(".git").join("config");
    let Ok(text) = std::fs::read_to_string(&config) else {
        return c("git remotes", Status::Info, "this folder is not a git checkout, or its config cannot be read", None);
    };
    let bad = remotes_with_credentials(&text);
    if bad.is_empty() {
        return c("git remotes", Status::Ok, "no remote URL holds a password or token", None);
    }
    let named = bad.iter().map(|n| if n == "url rewrite" { "a url rewrite rule".to_string() } else { format!("remote '{n}'") }).collect::<Vec<_>>().join(", ");
    c(
        "git remotes",
        Status::Fail,
        format!("{named} has a credential in its URL (not shown)"),
        Some("revoke that token, make a new one with the least scope, set the remote to the plain https URL, and let the credential manager hold the login (see documentation/SECURITY_AND_PRIVACY.md)"),
    )
}

/// The doctor's line about web search (card R6-18): which services `web.search` asks, in order, and whether the ones that need a key have it in the credential store. It reads what the
/// configuration says and does not ask any service, so it says which one is asked first and not which one answered. It never shows a key (it names the secret, which is a name).
pub fn check_web_search(cfg: &aqua_native_tools::http::HttpConfig, store: &dyn Secrets) -> Check {
    let engine_name = |e: aqua_native_tools::http::SearchEngine| serde_json::to_value(e).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "a service".to_string());
    let mut order: Vec<String> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let mut fix: Option<String> = None;
    for s in cfg.search.iter().chain(cfg.search_fallback.iter()) {
        let name = engine_name(s.engine);
        let mut label = name.clone();
        if let Some(profile) = &s.auth {
            match cfg.auth.get(profile) {
                None => {
                    problems.push(format!("{name} names the auth profile '{profile}', which http.json does not have"));
                    label.push_str(" (no such auth profile: skipped)");
                }
                Some(p) => match store.get(&p.secret) {
                    Ok(Some(v)) if !v.trim().is_empty() => label.push_str(" (key stored)"),
                    Ok(_) => {
                        problems.push(format!("{name}'s key '{}' is not in the credential store", p.secret));
                        fix = Some(format!("store the key yourself with: aqua-cli secret-set {}", p.secret));
                        label.push_str(" (key missing: skipped)");
                    }
                    Err(e) => {
                        problems.push(format!("{name}'s key could not be looked up ({e})"));
                        label.push_str(" (key not readable)");
                    }
                },
            }
        }
        order.push(label);
    }
    let set_up = !order.is_empty();
    order.push("DuckDuckGo's results page (no key)".to_string());
    order.push("Wikipedia (no key)".to_string());
    let detail = format!("web.search asks, in this order: {}", order.join(", "));
    if !problems.is_empty() {
        return c("web search", Status::Warn, format!("{detail}. {}", problems.join("; ")), fix.as_deref());
    }
    if !set_up {
        return c(
            "web search",
            Status::Info,
            format!("{detail}. No search service is set up, so the answers come from a results page that may stop answering and from Wikipedia"),
            Some("name a Brave or Tavily key (with an auth profile) or a SearXNG address under \"search\" in data/tools/http.json (see documentation/TOOLS_CONFIG.md)"),
        );
    }
    c("web search", Status::Ok, detail, None)
}

// The second report: lines of the form `key=value current` or `key=value drift`, about the two token files and, with `--live`, one health probe of the running core. It asks
// the file system, `icacls` and `git` about a token file and never opens it, so no function below can return the text of a token. A finding is a report and not a repair: nothing
// here changes a permission or a file.

/// Whether one line of the second report is as it should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Current,
    Drift,
}

/// One line of the second report: `key=value`, its verdict, and a note that says why (empty when there is nothing to add).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub key: String,
    pub value: String,
    pub verdict: Verdict,
    pub note: String,
}

fn line(key: &str, value: impl Into<String>, verdict: Verdict, note: impl Into<String>) -> Line {
    Line { key: key.into(), value: value.into(), verdict, note: note.into() }
}

/// The lines as text, and whether any of them is `drift`. A value has no spaces in it, so a line always reads `key=value state`.
pub fn render_lines(lines: &[Line]) -> (String, bool) {
    let mut s = String::from("\nToken files, and the running core with --live (key=value, then current or drift):\n");
    for l in lines {
        let value = l.value.split_whitespace().collect::<Vec<_>>().join("_");
        let state = if l.verdict == Verdict::Drift { "drift" } else { "current" };
        let shown = if value.is_empty() { "-" } else { value.as_str() };
        s.push_str(&format!("{}={shown} {state}", l.key));
        if !l.note.is_empty() {
            s.push_str(&format!("  ({})", l.note));
        }
        s.push('\n');
    }
    let drift = lines.iter().filter(|l| l.verdict == Verdict::Drift).count();
    s.push_str(&format!("\n{drift} line(s) drift.\n"));
    (s, drift > 0)
}

/// What kind of thing a name is on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Regular,
    /// A symbolic link, a junction or another reparse point: the name can lead somewhere else.
    Link,
    Directory,
    Other,
}

/// From what the file system says about the name itself, not about what a link points to.
pub fn file_kind(is_dir: bool, is_symlink: bool, is_reparse_point: bool, is_file: bool) -> FileKind {
    if is_symlink || is_reparse_point {
        FileKind::Link
    } else if is_dir {
        FileKind::Directory
    } else if is_file {
        FileKind::Regular
    } else {
        FileKind::Other
    }
}

/// Whether `path` exists, what kind of thing it is, and its size in bytes. The file is not opened.
pub fn stat_file(path: &Path) -> (bool, FileKind, u64) {
    match std::fs::symlink_metadata(path) {
        Ok(md) => {
            let ft = md.file_type();
            #[cfg(windows)]
            let reparse = std::os::windows::fs::MetadataExt::file_attributes(&md) & 0x400 != 0;
            #[cfg(not(windows))]
            let reparse = false;
            (true, file_kind(ft.is_dir(), ft.is_symlink(), reparse, ft.is_file()), md.len())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (false, FileKind::Other, 0),
        Err(_) => (true, FileKind::Other, 0),
    }
}

/// What git says about a file: tracked, not tracked, or no answer (with the reason).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tracked {
    Yes,
    No,
    Unknown(String),
}

/// From the exit code of `git ls-files --error-unmatch -- <file>`: 0 when the file is tracked, 1 when it is not, 128 when the folder is not in a checkout (or git refuses it);
/// `None` is a git that did not finish.
pub fn tracked_from_git(code: Option<i32>) -> Tracked {
    match code {
        Some(0) => Tracked::Yes,
        Some(1) => Tracked::No,
        Some(128) => Tracked::Unknown("not checked: git could not say (this folder is not in a checkout, or git refuses it)".into()),
        Some(n) => Tracked::Unknown(format!("not checked: git answered with exit code {n}")),
        None => Tracked::Unknown("not checked: git did not finish in time".into()),
    }
}

/// Runs a program with no input and waits at most `limit` for it. Returns its exit code (`None` when it had to be killed) and what it wrote to standard output.
fn run_with_deadline(cmd: &mut std::process::Command, limit: Duration) -> std::io::Result<(Option<i32>, String)> {
    use std::process::Stdio;
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let end = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            let mut bytes = Vec::new();
            if let Some(mut out) = child.stdout.take() {
                let _ = out.read_to_end(&mut bytes);
            }
            return Ok((status.code(), String::from_utf8_lossy(&bytes).into_owned()));
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            return Ok((None, String::new()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Is `file` (a name inside `dir`) tracked by the git checkout that holds `dir`? Asks `git ls-files`; a missing git is a note, not a finding.
fn git_tracked(dir: &Path, file: &str) -> Tracked {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["ls-files", "--error-unmatch", "--"]).arg(file).current_dir(dir);
    match run_with_deadline(&mut cmd, Duration::from_secs(10)) {
        Ok((code, _)) => tracked_from_git(code),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Tracked::Unknown("not checked: git is not installed".into()),
        Err(e) => Tracked::Unknown(format!("not checked: git could not be started ({})", e.kind())),
    }
}

/// One line of the access list of a file, as `icacls` prints it: who, and the flags in brackets (`F`, `M`, `RX`, `R,W`, `I`, `OI`, `IO`, `DENY`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclEntry {
    pub principal: String,
    pub flags: Vec<String>,
}

impl AclEntry {
    /// Does the entry give anyone access to the file itself? Not when it is a refusal (`DENY`), when it is meant only for what a folder holds (`IO`, inherit only), when it
    /// says "no access" (`N`), or when it is the integrity label that `icacls` lists next to the access entries.
    fn grants(&self) -> bool {
        let has = |f: &str| self.flags.iter().any(|x| x.eq_ignore_ascii_case(f));
        !has("DENY") && !has("IO") && !has("N") && !self.principal.to_ascii_lowercase().starts_with("mandatory label")
    }
}

/// The access entries in the text `icacls <path>` prints. The first entry follows the path on the first line, the others are indented under it, and a last line says how many
/// files were processed (it has no entry in it). Only the names and the flags are kept.
pub fn parse_icacls(text: &str, path: &str) -> Vec<AclEntry> {
    let norm = |s: &str| s.replace('/', "\\").to_ascii_lowercase();
    let want = norm(path);
    // The names start in the same column on every line, so an indented line shows where they start on the first one too.
    let column = text.lines().skip(1).find(|l| l.starts_with(' ') && l.contains(":(")).map(|l| l.len() - l.trim_start().len());
    let mut out = Vec::new();
    for raw in text.lines() {
        let Some(at) = raw.find(":(") else { continue };
        let left = &raw[..at];
        let principal = if raw.starts_with(' ') {
            left.trim()
        } else if norm(raw).starts_with(&want) {
            raw.get(path.len()..at).unwrap_or(left).trim()
        } else if let Some(col) = column.filter(|col| *col <= at) {
            left.get(col..).unwrap_or(left).trim()
        } else {
            left.trim()
        };
        if principal.is_empty() {
            continue;
        }
        let mut rest = &raw[at + 1..];
        let mut flags = Vec::new();
        while let Some(inner) = rest.strip_prefix('(') {
            let Some(close) = inner.find(')') else { break };
            flags.push(inner[..close].to_string());
            rest = &inner[close + 1..];
        }
        out.push(AclEntry { principal: principal.to_string(), flags });
    }
    out
}

/// The account the doctor runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub domain: String,
}

pub fn current_account() -> Account {
    let get = |k: &str| std::env::var(k).unwrap_or_default();
    let name = if get("USERNAME").is_empty() { get("USER") } else { get("USERNAME") };
    Account { name, domain: get("USERDOMAIN") }
}

/// Who may open a file, sorted into the accounts that are expected and the ones that are not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AclVerdict {
    /// The current user, SYSTEM and Administrators.
    pub fine: Vec<String>,
    /// Groups that put the file within reach of every account: Everyone, Users, Authenticated Users and the like.
    pub broad: Vec<String>,
    /// Any other account.
    pub other: Vec<String>,
}

/// Names (the part after the last backslash, in lower case) of the groups that stand for every account, and the same groups as well-known SIDs, which `icacls` prints when it cannot
/// find the name.
const BROAD_GROUPS: [&str; 8] = ["everyone", "users", "authenticated users", "guests", "anonymous logon", "interactive", "network", "domain users"];
const BROAD_SIDS: [&str; 3] = ["s-1-1-0", "s-1-5-11", "s-1-5-32-545"];
/// `creator owner` is the stand-in for whoever made the file, which is the current user.
const FINE_GROUPS: [&str; 3] = ["system", "administrators", "creator owner"];
const FINE_SIDS: [&str; 2] = ["s-1-5-18", "s-1-5-32-544"];

pub fn classify_acl(entries: &[AclEntry], who: &Account) -> AclVerdict {
    let me = who.name.to_ascii_lowercase();
    let me_full = format!("{}\\{me}", who.domain.to_ascii_lowercase());
    let mut v = AclVerdict::default();
    for e in entries.iter().filter(|e| e.grants()) {
        let name = e.principal.trim().to_string();
        let low = name.to_ascii_lowercase();
        let last = low.rsplit('\\').next().unwrap_or(&low);
        let is_me = !me.is_empty() && (low == me || (!who.domain.is_empty() && low == me_full));
        let list = if is_me || FINE_GROUPS.contains(&last) || FINE_SIDS.contains(&low.as_str()) {
            &mut v.fine
        } else if BROAD_GROUPS.contains(&last) || BROAD_SIDS.contains(&low.as_str()) {
            &mut v.broad
        } else {
            &mut v.other
        };
        if !list.contains(&name) {
            list.push(name);
        }
    }
    v
}

/// The access list of a file, or the reason it could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    Listed(Vec<AclEntry>),
    Unknown(String),
}

#[cfg(windows)]
fn query_access(path: &Path) -> Access {
    // The system's own program by its full path, not the first `icacls.exe` the search finds (card OA1-04, SE-18).
    let mut cmd = std::process::Command::new(aqua_native_tools::sandbox::icacls_exe());
    cmd.arg(path);
    match run_with_deadline(&mut cmd, Duration::from_secs(10)) {
        Ok((Some(0), text)) => {
            let entries = parse_icacls(&text, &path.to_string_lossy());
            if entries.is_empty() {
                Access::Unknown("not checked: no access entry could be read from what icacls printed".into())
            } else {
                Access::Listed(entries)
            }
        }
        Ok((Some(n), _)) => Access::Unknown(format!("not checked: icacls answered with exit code {n}")),
        Ok((None, _)) => Access::Unknown("not checked: icacls did not finish in time".into()),
        Err(e) => Access::Unknown(format!("not checked: icacls could not be started ({})", e.kind())),
    }
}

#[cfg(not(windows))]
fn query_access(_path: &Path) -> Access {
    Access::Unknown("not checked: the access list is only read on Windows".into())
}

/// What is known about one secret file: only its shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretFile {
    pub exists: bool,
    pub kind: FileKind,
    pub size: u64,
    pub tracked: Tracked,
    pub access: Access,
}

/// A token is 64 characters; the doctor's token check already fails anything under 32.
const MIN_TOKEN_BYTES: u64 = 32;

/// The lines for one secret file. `label` is the key prefix (`api_token`), `file` the name inside the data folder.
pub fn secret_file_lines(label: &str, file: &str, f: &SecretFile, who: &Account) -> Vec<Line> {
    let key = |what: &str| format!("{label}.{what}");
    let mut out = Vec::new();
    if !f.exists {
        out.push(line(&key("exists"), "no", Verdict::Current, format!("data/{file} is not created yet; Aqua makes it the first time it starts")));
        return out;
    }
    out.push(line(&key("exists"), "yes", Verdict::Current, ""));
    if f.kind != FileKind::Regular {
        let what = match f.kind {
            FileKind::Link => "link",
            FileKind::Directory => "directory",
            _ => "other",
        };
        out.push(line(&key("regular_file"), what, Verdict::Drift, "it is not a plain file (a link or a junction can lead the token somewhere else), so nothing else about it is checked"));
        return out;
    }
    out.push(line(&key("regular_file"), "yes", Verdict::Current, ""));
    if f.size < MIN_TOKEN_BYTES {
        out.push(line(&key("size"), f.size.to_string(), Verdict::Drift, format!("present, {} bytes: too short to be a token (delete data/{file} and restart Aqua to make a new one)", f.size)));
    } else {
        out.push(line(&key("size"), f.size.to_string(), Verdict::Current, format!("present, {} bytes", f.size)));
    }
    match &f.tracked {
        Tracked::No => out.push(line(&key("tracked_by_git"), "no", Verdict::Current, "")),
        Tracked::Yes => out.push(line(
            &key("tracked_by_git"),
            "yes",
            Verdict::Drift,
            "git tracks this file, so the token is in the history of the repository: revoke it (delete the file and restart Aqua) and take the file out of git; a secret that was pushed is leaked",
        )),
        Tracked::Unknown(why) => out.push(line(&key("tracked_by_git"), "skipped", Verdict::Current, why.clone())),
    }
    match &f.access {
        Access::Unknown(why) => out.push(line(&key("access"), "skipped", Verdict::Current, why.clone())),
        Access::Listed(entries) => {
            let v = classify_acl(entries, who);
            if v.broad.is_empty() && v.other.is_empty() {
                let note = if v.fine.is_empty() { "no entry grants access".to_string() } else { format!("only {}", v.fine.join(", ")) };
                out.push(line(&key("access"), "restricted", Verdict::Current, note));
            } else {
                let bad: Vec<String> = v.broad.iter().chain(v.other.iter()).cloned().collect();
                out.push(line(
                    &key("access"),
                    bad.join(","),
                    Verdict::Drift,
                    format!("{} can open this file; Aqua limits it to your account each time it starts, or run: icacls data\\{file} /inheritance:r /grant:r %USERNAME%:(R,W)", bad.join(", ")),
                ));
            }
        }
    }
    out
}

/// The two token files, as the data folder names them and as the keys call them.
const SECRET_FILES: [(&str, &str); 2] = [(".aqua_api_token", "api_token"), (".aqua_ui_token", "ui_token")];

/// The address of the user's running core. `--live` sends it one GET and nothing else.
pub const CORE_URL: &str = "http://127.0.0.1:8767";
/// The whole probe, connecting and waiting for the answer, is over after this long.
pub const LIVE_DEADLINE: Duration = Duration::from_secs(5);

/// The second report for the data folder `data_dir`. With `live_base` (a base URL such as [`CORE_URL`]) it also asks the running core whether it is healthy.
pub fn probe_lines(data_dir: &Path, live_base: Option<&str>) -> Vec<Line> {
    let who = current_account();
    let mut out = Vec::new();
    for (file, label) in SECRET_FILES {
        let path = data_dir.join(file);
        let (exists, kind, size) = stat_file(&path);
        // Git and icacls are asked about a plain file only; for anything else the lines say so before they would be used.
        let plain = exists && kind == FileKind::Regular;
        let none = || String::from("not checked");
        let facts = SecretFile {
            exists,
            kind,
            size,
            tracked: if plain { git_tracked(data_dir, file) } else { Tracked::Unknown(none()) },
            access: if plain { query_access(&path) } else { Access::Unknown(none()) },
        };
        out.extend(secret_file_lines(label, file, &facts, &who));
    }
    if let Some(base) = live_base {
        out.extend(live_lines(base, LIVE_DEADLINE));
    }
    out
}

/// The answer of the health route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Ok,
    /// Another status than 200 (503 while Aqua starts).
    Status(u16),
    Unreachable(String),
}

/// `http://127.0.0.1:PORT` (or `http://[::1]:PORT`) as an address. Anything else, a name or a host that is not this computer, is refused: the probe never leaves the machine.
fn loopback_addr(base: &str) -> Option<SocketAddr> {
    let rest = base.trim().strip_prefix("http://")?.trim_end_matches('/');
    let addr: SocketAddr = rest.parse().ok()?;
    addr.ip().is_loopback().then_some(addr)
}

/// The status code in the first line of an HTTP answer (`HTTP/1.1 200 OK`).
pub fn parse_status_line(head: &[u8]) -> Option<u16> {
    let end = head.iter().position(|b| *b == b'\n').unwrap_or(head.len());
    let first = std::str::from_utf8(&head[..end]).ok()?.trim_end();
    let mut parts = first.split(' ');
    if !parts.next()?.starts_with("HTTP/1.") {
        return None;
    }
    let code = parts.next()?;
    if code.len() != 3 {
        return None;
    }
    code.parse().ok()
}

/// One `GET /api/v1/health` to `base`, which is always over when `deadline` is (connecting, sending and waiting for the first line together). It sends nothing else and no token:
/// the route needs none. A listener that never answers gives `Unreachable`, not a hang.
pub fn probe_health(base: &str, deadline: Duration) -> Health {
    let Some(addr) = loopback_addr(base) else { return Health::Unreachable("only an address like http://127.0.0.1:8767 is probed".into()) };
    let end = Instant::now() + deadline;
    let left = || end.saturating_duration_since(Instant::now());
    let tick = Duration::from_millis(1);
    let mut stream = match TcpStream::connect_timeout(&addr, left().max(tick)) {
        Ok(s) => s,
        Err(e) => return Health::Unreachable(format!("nothing answered on {addr} ({})", e.kind())),
    };
    let _ = stream.set_write_timeout(Some(left().max(tick)));
    let request = format!("GET /api/v1/health HTTP/1.1\r\nHost: {addr}\r\nUser-Agent: aqua-cli-doctor\r\nAccept: */*\r\nConnection: close\r\n\r\n");
    if let Err(e) = stream.write_all(request.as_bytes()) {
        return Health::Unreachable(format!("the request could not be sent ({})", e.kind()));
    }
    let mut head: Vec<u8> = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        if left().is_zero() {
            return Health::Unreachable(format!("no answer within {} ms", deadline.as_millis()));
        }
        let _ = stream.set_read_timeout(Some(left().max(tick)));
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                head.extend_from_slice(&buf[..n]);
                if head.contains(&b'\n') || head.len() > 4096 {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Health::Unreachable(format!("no answer ({})", e.kind())),
        }
    }
    match parse_status_line(&head) {
        Some(200) => Health::Ok,
        Some(code) => Health::Status(code),
        None => Health::Unreachable("what came back was not an HTTP answer".into()),
    }
}

/// The `core_health` line: `ok` is current; anything else is drift.
pub fn live_lines(base: &str, deadline: Duration) -> Vec<Line> {
    vec![match probe_health(base, deadline) {
        Health::Ok => line("core_health", "ok", Verdict::Current, format!("GET /api/v1/health on {base} answered 200")),
        Health::Status(503) => line("core_health", "http_503", Verdict::Drift, "Aqua answers but says it is not ready (it is still starting, or it stopped being healthy)"),
        Health::Status(code) => line("core_health", format!("http_{code}"), Verdict::Drift, "the health route should answer 200"),
        Health::Unreachable(why) => line("core_health", "unreachable", Verdict::Drift, why),
    }]
}

/// Run every check against `data_dir` (usually `<workspace>/data`), with Windows Credential Manager as the credential store.
pub fn run(data_dir: &Path, fix: bool) -> Vec<Check> {
    run_with(data_dir, fix, Arc::new(CredentialManager))
}

/// `run` with a chosen credential store. The unit tests give it an in-memory one: they run the doctor several times at once, and concurrent writes and deletes in the
/// real Credential Manager lost some of the throwaway entries (eight `Aqua/doctor/probe-...` entries were found left behind in a real one).
pub fn run_with(data_dir: &Path, fix: bool, store: Arc<dyn Secrets>) -> Vec<Check> {
    let mut out = Vec::new();
    if let Err(e) = std::fs::create_dir_all(data_dir.join("tools")) {
        out.push(c("data folder", Status::Fail, format!("cannot create {}: {e}", data_dir.display()), Some("check the folder's permissions")));
        return out;
    }
    out.push(c("data folder", Status::Ok, data_dir.display().to_string(), None));
    if let Some(root) = data_dir.parent() {
        out.push(check_remote_credentials(root));
    }

    // Tokens: created by the gateway on first start.
    for (file, what) in [(".aqua_api_token", "API token"), (".aqua_ui_token", "UI token")] {
        match std::fs::read_to_string(data_dir.join(file)) {
            Ok(t) if t.trim().len() >= 32 => out.push(c(what, Status::Ok, "present (not shown)", None)),
            Ok(_) => out.push(c(what, Status::Fail, "the file exists but the token is too short to be safe", Some(&format!("delete data/{file} and restart Aqua to make a new one")))),
            Err(_) => out.push(c(what, Status::Info, "not created yet (Aqua makes it the first time it starts)", None)),
        }
    }

    // The policy: a broken file locks the whole host.
    let mode_env = std::env::var("AQUA_TOOL_MODE").ok();
    match std::fs::read_to_string(policy_path(data_dir)) {
        Ok(text) => match aqua_tool_host::config_text::prepare(&text).map_err(|e| e.detail()).and_then(|t| PolicyFile::parse(&t)) {
            Ok(p) => out.push(c("policy.json", Status::Ok, format!("mode {:?}, {} hard-denied, limits {}/{} per minute, {} at once", p.mode, p.hard_deny.len(), p.limits.reads_per_minute, p.limits.changes_per_minute, p.limits.max_concurrent), None)),
            Err(e) => out.push(c("policy.json", Status::Fail, format!("rejected, so the host is LOCKED: {e}"), Some("fix the file, or delete it to get the defaults back"))),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = load_policy(data_dir);
            out.push(c("policy.json", Status::Ok, "created with the defaults (standard mode)", None));
        }
        Err(e) => out.push(c("policy.json", Status::Fail, format!("unreadable, so the host is LOCKED: {e}"), Some("check the file's permissions"))),
    }
    if let Some(m) = mode_env {
        let known = ["locked", "standard", "developer"].contains(&m.to_lowercase().as_str());
        out.push(c("AQUA_TOOL_MODE", if known { Status::Info } else { Status::Fail }, if known { format!("set to '{m}' and overrides policy.json") } else { format!("'{m}' is not a mode, so the host is LOCKED") }, if known { None } else { Some("use locked, standard or developer, or unset it") }));
    }

    // Each family: present, valid, and what it enables.
    macro_rules! family {
        ($name:expr, $load:expr, $path:expr, $describe:expr) => {{
            match $load(data_dir) {
                Some(cfg) => out.push(c($name, Status::Ok, $describe(&cfg), None)),
                None => {
                    let path = $path(data_dir);
                    let why = crate::config_files::last_error(&path).map(|e| format!(": {}", e.detail())).unwrap_or_default();
                    out.push(c($name, Status::Fail, format!("{} was rejected, so these tools are OFF{why}", path.display()), Some("fix the file (see documentation/TOOLS_CONFIG.md) and restart")))
                }
            }
        }};
    }
    family!("fs.json", load_fs_config, fs_config_path, |x: &aqua_native_tools::fs::FsConfig| format!("{} folder root(s)", x.roots.len()));
    family!("term.json", load_term_config, term_config_path, |x: &aqua_native_tools::term::TermConfig| format!("up to {} background sessions", x.max_sessions));
    family!("term_rules.json", load_term_rules, term_rules_path, |x: &aqua_native_tools::term::ExecRules| if x.is_empty() { "no rules of your own".to_string() } else { format!("{} rule(s) that make commands ask or refuse", x.len()) });
    match crate::hooks::load_hooks(data_dir) {
        Ok(h) if h.is_empty() => out.push(c("hooks.json", Status::Ok, "no hooks".to_string(), None)),
        Ok(h) => {
            let (with_env, with_cwd) = h.counts_env_and_cwd();
            out.push(c("hooks.json", Status::Ok, format!("{} hook(s) that can make a call ask or refuse it (events: {}; {with_env} with env, {with_cwd} with cwd)", h.len(), h.events().join(", ")), None));
        }
        Err(e) => out.push(c("hooks.json", Status::Fail, format!("{} is wrong ({e}), so every call that is not a read asks", crate::hooks::hooks_path(data_dir).display()), Some("fix the file (see documentation/TOOLS_CONFIG.md) and restart"))),
    }
    {
        let skills = crate::skills::Skills::load(&crate::skills::skills_dir(data_dir));
        let warned = if skills.warnings.is_empty() { String::new() } else { format!("; {} note(s): {}", skills.warnings.len(), skills.warnings.join(" | ")) };
        let detail = format!("{} skill(s){}{warned}", skills.len(), if skills.problems.is_empty() { String::new() } else { format!("; {} could not be read: {}", skills.problems.len(), skills.problems.join(" | ")) });
        out.push(c("skills", if skills.problems.is_empty() { Status::Ok } else { Status::Warn }, detail, if skills.problems.is_empty() { None } else { Some("fix the SKILL.md files named above (see documentation/TOOLS_CONFIG.md, skills)") }));
        // The skills that ship with Aqua (card OA12-01): how many, and whether they are on offer.
        let shipped = crate::skills::shipped::all();
        out.push(c("shipped skills", if shipped.problems.is_empty() { Status::Ok } else { Status::Warn }, crate::skills::shipped::status_line(), None));
    }
    {
        let dir = data_dir.join("tools");
        let path = dir.join("mcp.json");
        if path.exists() {
            match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| aqua_mcp_adapters::user_servers::parse_file(&t)) {
                Ok(servers) => {
                    let trusted = std::fs::read_to_string(dir.join("mcp_trusted.json")).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()).and_then(|v| v["servers"].as_object().map(|o| o.len())).unwrap_or(0);
                    out.push(c("mcp.json", Status::Ok, format!("{} server(s), {trusted} trusted (a server is started only when you trust it)", servers.len()), None));
                    // Card OA11-03: which servers are on another machine, which are for some projects or agents only, and how the tools of each are shown.
                    let remote = servers.values().filter(|s| s.is_remote()).count();
                    let scoped = servers.values().filter(|s| !s.scope.projects.is_empty() || !s.scope.agents.is_empty()).count();
                    let wide = servers.values().filter(|s| s.expose != aqua_mcp_adapters::user_servers::Expose::Named).count();
                    if remote + scoped + wide > 0 {
                        out.push(c("mcp servers, exposure", Status::Ok, format!("{remote} remote, {scoped} with a scope (projects or agents), {wide} shown as ranked or always (the others when the goal names them)"), None));
                    }
                    if remote > 0 && !crate::mcp_remote::enabled() {
                        out.push(c("mcp remote", Status::Warn, format!("{remote} remote server(s) in mcp.json, and remote servers are off, so none is started"), Some("set AQUA_MCP_REMOTE=on and list the host in data/tools/http.json (allow) to use one")));
                    }
                }
                Err(e) => out.push(c("mcp.json", Status::Fail, format!("{} is wrong ({e}), so none of its servers is used", path.display()), Some("fix the file (see documentation/TOOLS_CONFIG.md) and restart"))),
            }
        }
    }
    out.push(if crate::project_context::enabled() {
        c("project packs", Status::Ok, format!("on: a repository's .aqua folder is read for a run in it, each kind (text, code) only after you trusted it by its hash; {} pack(s) recorded as trusted", crate::project_pack::recorded_count(data_dir)), None)
    } else {
        c("project packs", Status::Ok, "off (AQUA_PROJECT_PACK=on to use a repository's .aqua folder); nothing in such a folder is read".to_string(), None)
    });
    // The command tools (card OA11-02): off unless their switch says so, and then what the file declares and what was left out.
    for line in crate::command_tools::status_lines(data_dir) {
        let bad = line.contains("cannot be used") || line.contains("larger than");
        out.push(c("command tools", if bad { Status::Warn } else { Status::Ok }, line, if bad { Some("fix data/tools/custom.json (see documentation/TOOLS_CONFIG.md, command tools)") } else { None }));
    }
    family!("repos.json", load_git_config, git_config_path, |x: &aqua_native_tools::git::GitConfig| format!("{} repositor{} named{}", x.repos.len(), if x.repos.len() == 1 { "y" } else { "ies" }, if x.discover { ", and the ones found in the allowed folders (git.repos lists them)" } else { ", none looked for" }));
    family!("ssh.json", load_ssh_config, ssh_config_path, |x: &aqua_native_tools::ssh::SshConfig| format!("{} host(s)", x.hosts.len()));
    family!("db.json", load_db_config, db_config_path, |x: &aqua_native_tools::db::DbConfig| format!("{} database(s)", x.profiles.len()));
    family!("http.json", load_http_config, http_config_path, |x: &aqua_native_tools::http::HttpConfig| format!("{} allowed host pattern(s), {} auth profile(s)", x.allow.len(), x.auth.len()));
    if let Some(cfg) = load_http_config(data_dir) {
        out.push(check_web_search(&cfg, &*store));
    }
    family!("docker.json", load_docker_config, docker_config_path, |x: &aqua_native_tools::docker::DockerConfig| format!("{} allowed image pattern(s)", x.allow_images.len()));
    family!("k8s.json", load_k8s_config, k8s_config_path, |x: &aqua_native_tools::k8s::K8sConfig| format!("{} cluster(s)", x.contexts.len()));
    family!("iac.json", load_iac_config, iac_config_path, |x: &aqua_native_tools::iac::IacConfig| format!("{} Terraform folder(s)", x.dirs.len()));
    family!("cloud.json", load_cloud_config, cloud_config_path, |x: &aqua_native_tools::cloud::CloudConfig| format!("providers enabled: {}", if x.enabled.is_empty() { "none".to_string() } else { x.enabled.join(", ") }));
    family!("forge.json", load_forge_config, forge_config_path, |x: &aqua_native_tools::forge::ForgeConfig| format!("{} account(s)", x.accounts.len()));
    family!("mon.json", load_mon_config, mon_config_path, |x: &aqua_native_tools::mon::MonConfig| format!("{} probe(s)", x.probes.len()));

    // A key of a config file that is not one of the known keys, with the nearest one (`unknown key `rots`: did you mean `roots`?`).
    for report in crate::config_files::status(data_dir) {
        for hint in &report.warnings {
            out.push(c(&format!("{} key", report.name), Status::Warn, hint.clone(), Some("correct the name of the key (see documentation/TOOLS_CONFIG.md)")));
        }
    }

    // The families of tools a switch controls, and what the switch says in this environment (the doctor does not build the host; `GET /api/v1/tools/providers` says what the running core did).
    for (family, var, why) in crate::tool_providers::switch_lines() {
        out.push(c(&format!("{family} tools"), Status::Info, match why { Some(why) => format!("off: {why}"), None => format!("on ({var})") }, None));
    }

    // The file roots must exist, or file tools answer NOT_FOUND for everything.
    if let Some(cfg) = load_fs_config(data_dir) {
        for r in &cfg.roots {
            let path = aqua_native_tools::fs::guard::expand_env(&r.path);
            if Path::new(&path).is_dir() {
                out.push(c(&format!("root @{}", r.alias), Status::Ok, path, None));
            } else if fix && std::fs::create_dir_all(&path).is_ok() {
                out.push(c(&format!("root @{}", r.alias), Status::Ok, format!("{path} (created)"), None));
            } else {
                out.push(c(&format!("root @{}", r.alias), Status::Warn, format!("{path} does not exist"), Some("create it, or run `aqua-cli doctor --fix`")));
            }
        }
    }

    // The credential store, end to end, with a throwaway entry.
    let probe = format!("doctor/probe-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    let round_trip = store.set(&probe, "doctor-probe-value").and_then(|_| store.get(&probe)).map(|v| v.as_deref() == Some("doctor-probe-value"));
    let _ = store.delete(&probe);
    match round_trip {
        Ok(true) => out.push(c("credential store", Status::Ok, "Windows Credential Manager works (write, read, delete)", None)),
        Ok(false) => out.push(c("credential store", Status::Fail, "a stored value did not read back correctly", Some("secrets cannot be used until this works"))),
        Err(e) => out.push(c("credential store", Status::Fail, format!("not usable: {e}"), Some("secrets cannot be used until this works"))),
    }

    // The enrolled voice is sealed with a key that lives in the credential store. When the key is gone the voiceprint cannot be opened, the voice lock stays on and
    // every voice request is refused until the voice is enrolled again: the core says so once in its log at start-up, and the doctor says so here.
    if data_dir.join("aqua_core.db").is_file() {
        let sealed = crate::storage::DatabaseManager::with_secrets(data_dir, store.clone()).and_then(|db| db.get_setting("master_voice_embedding"));
        match sealed {
            Ok(Some(sealed)) if !sealed.trim().is_empty() => match crate::storage::voiceprint::reveal(&*store, &sealed) {
                Ok(_) => out.push(c("enrolled voice", Status::Ok, "the stored voiceprint can be opened", None)),
                Err(e) => out.push(c("enrolled voice", Status::Fail, format!("the stored voiceprint cannot be opened: {e}"), Some("voice requests are refused until you enrol your voice again (the voice security settings)"))),
            },
            Ok(_) => out.push(c("enrolled voice", Status::Info, "no voice is enrolled", None)),
            Err(e) => out.push(c("enrolled voice", Status::Warn, format!("the settings database could not be read: {e}"), None)),
        }
    }

    // The audit chain.
    let audit = data_dir.join("tools").join("host.audit.jsonl");
    if audit.exists() {
        match aqua_tool_host::audit::verify_chain(&audit) {
            Ok(n) => out.push(c("audit log", Status::Ok, format!("{n} records, chain intact"), None)),
            Err(e) => out.push(c("audit log", Status::Fail, format!("chain broken at line {}: {}", e.line, e.reason), Some("the log was edited or damaged; keep a copy, then move it aside to start a new one"))),
        }
    } else {
        out.push(c("audit log", Status::Info, "empty (nothing has been called yet)", None));
    }

    // Files a store moved aside instead of deleting them: a database or a state file that was damaged (`*.corrupt-<time>`, `*.bad-<time>`). Nothing removes them; they still hold
    // what they held (possibly private), so the person is told where they are and that they can delete them when they no longer need them. Said only when there are some.
    let aside = crate::storage::sqlite_store::leftovers(data_dir);
    if !aside.is_empty() {
        let shown: Vec<String> = aside.iter().take(5).map(|p| p.strip_prefix(data_dir).unwrap_or(p).display().to_string()).collect();
        let more = if aside.len() > 5 { format!(" and {} more", aside.len() - 5) } else { String::new() };
        out.push(c(
            "damaged files set aside",
            Status::Warn,
            format!("{} file(s) in the data folder were damaged and moved aside, not deleted: {}{more}. A new file took the place of each", aside.len(), shown.join(", ")),
            Some("look at them if you need what they held; delete them when you do not (they may hold private text, and Aqua never deletes them)"),
        ));
    }

    // Panics: a background thread that panicked too often is stopped until Aqua restarts (the flow scheduler, which fires reminders, is one), and the last panic is kept.
    // The doctor runs in a process of its own and cannot see the core's memory, so it reads the record the core wrote in data/crash.
    if let Some(failed) = crate::supervise::read_failed(data_dir) {
        out.push(c(
            "background threads",
            Status::Fail,
            format!("stopped after repeated panics: {} (reminders and scheduled flows may not run until Aqua restarts)", failed.threads.join(", ")),
            Some("restart Aqua; if it happens again, keep logs/core.log and report it"),
        ));
    }
    // Jobs the running core's monitor found stuck (no sign of life for five times what they promised) or ended by an error or a panic: the core writes `data/crash/jobs.json` every 30
    // seconds while there is something to say. A record that is not fresh is from a core that has since stopped (or one whose monitor died), and is only mentioned.
    if let Some(record) = crate::jobs::read_record(data_dir) {
        if let Some(line) = jobs_line(&record, crate::jobs::now_ms()) {
            out.push(line);
        }
    }
    if let Some(p) = crate::supervise::read_last_panic(data_dir) {
        let place = p.location.as_deref().map(|l| format!(" at {l}")).unwrap_or_default();
        out.push(c("last panic", Status::Info, format!("thread {}{place}, unix time {} ms: {}", p.thread, p.at_unix_ms, p.message), None));
    }

    // The switches of the remote coding planner are retired (card OA1-03). Someone who still has one set is told that it does nothing, so nobody believes the planner is remote (or that a prompt can leave).
    if let Some(notice) = crate::modules::coding_runner::retired_switches_notice() {
        out.push(c("remote coding planner", Status::Warn, notice, Some("remove the AQUA_REMOTE_CODING* settings from the environment; they are ignored")));
    }

    // Programs the tools drive. Missing ones just hide their tools.
    let scratch = data_dir.join("tools");
    for (name, dirs, needed_for) in [
        ("git", &["%ProgramFiles%/Git/cmd"][..], "git.*"),
        ("ssh", &["%SystemRoot%/System32/OpenSSH"][..], "ssh.*"),
        ("scp", &["%SystemRoot%/System32/OpenSSH"][..], "ssh.upload, ssh.download"),
        ("uvx", &["%USERPROFILE%/.local/bin", "%USERPROFILE%/.cargo/bin"][..], "win.* (Windows-MCP)"),
        ("docker", &["%ProgramFiles%/Docker/Docker/resources/bin"][..], "docker.*"),
        ("kubectl", &["%ProgramFiles%/Kubernetes"][..], "k8s.*"),
        ("terraform", &["%ProgramFiles%/Terraform"][..], "iac.*"),
        ("aws", &["%ProgramFiles%/Amazon/AWSCLIV2"][..], "cloud.call (aws)"),
        ("az", &["%ProgramFiles%/Microsoft SDKs/Azure/CLI2/wbin"][..], "cloud.call (azure)"),
        ("gcloud", &["%LOCALAPPDATA%/Google/Cloud SDK/google-cloud-sdk/bin"][..], "cloud.call (gcp)"),
        ("wevtutil", &["%SystemRoot%/System32"][..], "mon.eventlog"),
    ] {
        let cli = Cli::find(program_name(name), &None, dirs, &scratch);
        match &cli.exe {
            Some(p) => out.push(c(&format!("program {name}"), Status::Ok, p.display().to_string(), None)),
            None => out.push(c(&format!("program {name}"), Status::Info, format!("not found; {needed_for} will be hidden"), None)),
        }
    }

    // Is Aqua running? (Then a new build needs a restart to take effect.)
    let running = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], 8767)), Duration::from_millis(300)).is_ok();
    out.push(c("gateway", Status::Info, if running { "Aqua is running on 127.0.0.1:8767; restart it to apply new builds or config changes" } else { "Aqua is not running" }, None));
    out
}

/// The `background jobs` line for the record of the core's monitor, as of `now` (milliseconds since 1970): a failure when the record is fresh, a note when it is old; nothing when it
/// names no job.
fn jobs_line(record: &crate::jobs::Record, now: i64) -> Option<Check> {
    if record.jobs.is_empty() {
        return None;
    }
    let what: Vec<String> = record
        .jobs
        .iter()
        .map(|j| match (j.state.as_str(), j.error.as_deref()) {
            ("failed" | "panicked", why) => format!("{} {}{}", j.name, j.state, why.map(|w| format!(" ({w})")).unwrap_or_default()),
            _ => format!("{} shows no sign of life for {} s{}", j.name, j.silent_ms / 1000, j.expect_beat_ms.map(|e| format!(" (it promises one every {} s)", e / 1000)).unwrap_or_default()),
        })
        .collect();
    let age_s = now.saturating_sub(record.at_unix_ms).max(0) / 1000;
    Some(if now.saturating_sub(record.at_unix_ms) <= crate::jobs::RECORD_FRESH_MS {
        c("background jobs", Status::Fail, what.join("; "), Some("restart Aqua; if it happens again, look at the run of the log with `aqua-cli logs --level warn` and report it"))
    } else {
        c("background jobs", Status::Info, format!("a record from {age_s} s ago (the core that wrote it may be gone) names: {}", what.join("; ")), None)
    })
}

fn program_name(n: &str) -> &'static str {
    match n {
        "git" => "git",
        "ssh" => "ssh",
        "scp" => "scp",
        "uvx" => "uvx",
        "docker" => "docker",
        "kubectl" => "kubectl",
        "terraform" => "terraform",
        "aws" => "aws",
        "az" => "az",
        "gcloud" => "gcloud",
        _ => "wevtutil",
    }
}

/// Text for the terminal. Returns the report and whether anything failed.
pub fn render(checks: &[Check]) -> (String, bool) {
    let mut s = String::new();
    for k in checks {
        let tag = match k.status {
            Status::Ok => "  ok  ",
            Status::Info => " info ",
            Status::Warn => " WARN ",
            Status::Fail => " FAIL ",
        };
        s.push_str(&format!("[{tag}] {}: {}\n", k.name, k.detail));
        if let Some(f) = &k.fix {
            s.push_str(&format!("         fix: {f}\n"));
        }
    }
    let (f, w) = (checks.iter().filter(|k| k.status == Status::Fail).count(), checks.iter().filter(|k| k.status == Status::Warn).count());
    s.push_str(&format!("\n{} failure(s), {} warning(s).\n", f, w));
    (s, f > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_native_tools::secrets::MemorySecrets;

    /// The doctor against an in-memory credential store: a test must not write to (or race on) the real one.
    fn run(data_dir: &Path, fix: bool) -> Vec<Check> {
        run_with(data_dir, fix, Arc::new(MemorySecrets::new()))
    }

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_doctor_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn find<'a>(v: &'a [Check], name: &str) -> &'a Check {
        v.iter().find(|k| k.name == name).unwrap_or_else(|| panic!("no check '{name}' in {:?}", v.iter().map(|k| &k.name).collect::<Vec<_>>()))
    }

    /// A background thread that was given up and the last panic are read from the record the core left in data/crash.
    #[test]
    fn the_doctor_reports_a_thread_that_was_given_up_and_the_last_panic_from_the_record_in_the_data_folder() {
        let d = dir("crash_record");
        let crash = d.join("crash");
        std::fs::create_dir_all(&crash).unwrap();
        // Nothing recorded: no line about it.
        assert!(run(&d, false).iter().all(|k| k.name != "background threads" && k.name != "last panic"));
        std::fs::write(crash.join("last_panic.json"), r#"{"thread":"aqua-flow-scheduler","at_unix_ms":1700000000000,"location":"core/src/flows/sched/tick.rs:10:5","message":"index out of bounds","gave_up":true}"#).unwrap();
        std::fs::write(crash.join("background_failed.json"), r#"{"threads":["aqua-flow-scheduler"],"at_unix_ms":1700000000000}"#).unwrap();
        let checks = run(&d, false);
        let failed = find(&checks, "background threads");
        assert!(failed.status == Status::Fail && failed.detail.contains("aqua-flow-scheduler") && failed.detail.contains("reminders"), "{failed:?}");
        assert!(failed.fix.as_deref().unwrap_or("").contains("restart Aqua"));
        let last = find(&checks, "last panic");
        assert!(last.status == Status::Info && last.detail.contains("tick.rs:10:5") && last.detail.contains("index out of bounds"), "{last:?}");
        // A record that cannot be read says nothing rather than something wrong.
        std::fs::write(crash.join("background_failed.json"), "{\"threads\":").unwrap();
        assert!(run(&d, false).iter().all(|k| k.name != "background threads"));
    }

    /// Card OA3-04: the jobs the core's monitor found stuck or ended badly are read from `data/crash/jobs.json`: a failure while the record is fresh, a note when it is old, nothing when
    /// there is no record or it cannot be read, and never what the jobs work on.
    #[test]
    fn the_doctor_reports_the_jobs_the_monitor_found_stuck_or_ended_badly_and_not_a_record_that_is_old_as_a_failure() {
        let d = dir("jobs_record");
        let crash = d.join("crash");
        std::fs::create_dir_all(&crash).unwrap();
        assert!(run(&d, false).iter().all(|k| k.name != "background jobs"), "no record, no line");
        let now = crate::jobs::now_ms();
        let record = crate::jobs::Record {
            at_unix_ms: now,
            jobs: vec![
                crate::jobs::RecordedJob { name: "aqua-flow-scheduler".into(), kind: "scheduler".into(), state: "running".into(), silent_ms: 125_000, expect_beat_ms: Some(10_000), error: None },
                crate::jobs::RecordedJob { name: "aqua-workflows".into(), kind: "scheduler".into(), state: "panicked".into(), silent_ms: 5_000, expect_beat_ms: Some(10_000), error: Some("index out of bounds".into()) },
            ],
        };
        crate::jobs::write_record(&d, &record);
        let checks = run(&d, false);
        let line = find(&checks, "background jobs");
        assert_eq!(line.status, Status::Fail, "{line:?}");
        assert!(line.detail.contains("aqua-flow-scheduler shows no sign of life for 125 s") && line.detail.contains("promises one every 10 s"), "{line:?}");
        assert!(line.detail.contains("aqua-workflows panicked (index out of bounds)"), "{line:?}");
        assert!(line.fix.as_deref().unwrap_or("").contains("restart Aqua"));
        // An old record is from a core that may be gone: said, not failed.
        let old = jobs_line(&record, now + crate::jobs::RECORD_FRESH_MS + 60_000).unwrap();
        assert!(old.status == Status::Info && old.detail.contains("may be gone"), "{old:?}");
        // A record that cannot be read, or one that names nothing, says nothing.
        assert!(jobs_line(&crate::jobs::Record { at_unix_ms: now, jobs: Vec::new() }, now).is_none());
        std::fs::write(crash.join("jobs.json"), "{\"at_unix").unwrap();
        assert!(run(&d, false).iter().all(|k| k.name != "background jobs"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Card OA1-03: a retired AQUA_REMOTE_CODING* switch that is still set is reported as ignored (a warning, not a failure), without the address it holds; with none set the doctor says nothing.
    #[test]
    fn the_doctor_says_a_retired_remote_coding_switch_is_ignored_and_stays_quiet_when_none_is_set() {
        let _env = crate::modules::planner::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let names = crate::modules::coding_runner::RETIRED_REMOTE_SWITCHES;
        let saved: Vec<_> = names.iter().map(|n| (*n, std::env::var_os(n))).collect();
        for n in names {
            std::env::remove_var(n);
        }
        let d = dir("retired_remote_coding");
        let quiet = run(&d, false);
        std::env::set_var("AQUA_REMOTE_CODING", "on");
        std::env::set_var("AQUA_REMOTE_CODING_URL", "https://example.invalid/v1?key=not-a-real-key"); // secret-scan: ignore
        let told = run(&d, false);
        for (name, old) in saved {
            match old {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
        assert!(quiet.iter().all(|k| k.name != "remote coding planner"));
        let said = find(&told, "remote coding planner");
        assert!(said.status == Status::Warn && said.detail.contains("AQUA_REMOTE_CODING") && said.detail.contains("no longer supported"), "{said:?}");
        assert!(!said.detail.contains("example.invalid") && !said.detail.contains("not-a-real-key"), "the address is not repeated: {said:?}");
        assert!(said.fix.as_deref().unwrap_or("").contains("ignored"));
    }

    // The URLs below are made up (the host does not exist): they are the shape of a leaked remote, not a real login.
    const LEAKED: &str = "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = https://bench-user:not-a-real-token-1234567890@example.invalid/team/repo.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n[branch \"main\"]\n\tremote = origin\n"; // secret-scan: ignore
    const CLEAN: &str = "[remote \"origin\"]\n\turl = https://example.invalid/team/repo.git\n[remote \"backup\"]\n\turl = ssh://git@example.invalid/team/repo.git\n[remote \"mirror\"]\n\turl = git@example.invalid:team/repo.git\n";

    /// Card R6-18: the doctor says which services web search asks and whether the key of each is in the credential store; it never shows the key.
    #[test]
    fn the_doctor_says_which_search_services_are_asked_and_whether_their_keys_are_stored_without_showing_a_key() {
        let config = |search: serde_json::Value| -> aqua_native_tools::http::HttpConfig {
            serde_json::from_value(serde_json::json!({
                "search": search,
                "auth": { "brave": { "kind": "header", "secret": "search/brave", "hosts": ["api.search.brave.com"], "name": "X-Subscription-Token" } }
            }))
            .expect("a configuration")
        };
        let store = MemorySecrets::new();
        // Nothing set up: DuckDuckGo and Wikipedia, and the way to do better.
        let none: aqua_native_tools::http::HttpConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        let k = check_web_search(&none, &store);
        assert!(k.status == Status::Info && k.detail.contains("DuckDuckGo's results page (no key), Wikipedia (no key)") && k.fix.as_deref().unwrap_or("").contains("Brave or Tavily"), "{k:?}");
        // Brave named, its key not stored: a warning that names the secret and how to store it, and the service is said to be skipped.
        let brave = config(serde_json::json!({ "engine": "brave", "auth": "brave" }));
        let k = check_web_search(&brave, &store);
        assert!(k.status == Status::Warn && k.detail.contains("brave (key missing: skipped)") && k.fix.as_deref() == Some("store the key yourself with: aqua-cli secret-set search/brave"), "{k:?}");
        // The key stored: ok, brave first, and the value is nowhere in the line.
        store.set("search/brave", "BSA-not-a-real-key-0123456789").unwrap();
        let k = check_web_search(&brave, &store);
        assert!(k.status == Status::Ok && k.detail.starts_with("web.search asks, in this order: brave (key stored), DuckDuckGo"), "{k:?}");
        assert!(!format!("{k:?}").contains("BSA-not-a-real-key"), "the key is never shown");
        // A profile http.json does not have.
        let lost = config(serde_json::json!({ "engine": "searxng", "url": "https://searx.example.invalid", "auth": "nobody" }));
        let k = check_web_search(&lost, &store);
        assert!(k.status == Status::Warn && k.detail.contains("auth profile 'nobody'"), "{k:?}");
        // And the whole doctor carries the line, whatever the configuration.
        let d = dir("search_line");
        let checks = run(&d, false);
        assert!(find(&checks, "web search").detail.starts_with("web.search asks, in this order:"), "{:?}", find(&checks, "web search"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Card OA3-02: a database or a state file that a store moved aside as damaged is listed (never deleted), and nothing is said when there is none.
    #[test]
    fn files_a_store_moved_aside_are_listed_as_a_warning_and_only_then() {
        let root = dir("aside");
        let data = root.join("data");
        let clean = run(&data, false);
        assert!(clean.iter().all(|k| k.name != "damaged files set aside"), "nothing moved aside, nothing said");
        std::fs::create_dir_all(data.join("vault")).unwrap();
        std::fs::write(data.join("agent_runs.db.corrupt-1790000000"), b"old").unwrap();
        std::fs::write(data.join("agent_runs.db.corrupt-1790000000-wal"), b"old log").unwrap();
        std::fs::write(data.join("tools").join("audit_keys.json.bad-1790000000123"), b"old").unwrap();
        std::fs::write(data.join("vault").join("vault.db.corrupt-1790000001"), b"old").unwrap();
        let k = run(&data, false).into_iter().find(|k| k.name == "damaged files set aside").expect("listed");
        assert_eq!(k.status, Status::Warn);
        assert!(k.detail.starts_with("3 file(s)") && k.detail.contains("agent_runs.db.corrupt-1790000000") && k.detail.contains("audit_keys.json.bad-1790000000123"), "{}", k.detail);
        assert!(!k.detail.contains("-wal"), "the log of a damaged database is not listed on its own: {}", k.detail);
        assert!(k.fix.as_deref().unwrap_or("").contains("never deletes"), "it says that nothing is deleted for the person");
        assert!(data.join("agent_runs.db.corrupt-1790000000").exists(), "the doctor deletes nothing");
    }

    #[test]
    fn a_remote_url_with_a_password_or_a_token_is_found_by_name_and_a_plain_login_is_not() {
        assert_eq!(remotes_with_credentials(LEAKED), vec!["origin".to_string()]);
        assert!(remotes_with_credentials(CLEAN).is_empty(), "a plain https URL, a user name on ssh, and scp-style addresses are not credentials");
        // A lone token used as the user name, and a push URL, are found too; so is a rewrite rule that carries one.
        let token_only = "[remote \"origin\"]\n\turl = https://example.invalid/a.git\n\tpushurl = https://glpat-abcdefghijklmnopqrstuvwxyz@example.invalid/a.git\n"; // secret-scan: ignore
        assert_eq!(remotes_with_credentials(token_only), vec!["origin".to_string()]);
        let rewrite = "[url \"https://user:pass1234@example.invalid/\"]\n\tinsteadOf = https://example.invalid/\n"; // secret-scan: ignore
        assert_eq!(remotes_with_credentials(rewrite), vec!["url rewrite".to_string()]);
        // The same remote listed twice is named once; a comment line and other sections are ignored.
        let twice = format!("{LEAKED}# url = https://x:y@example.invalid/\n[remote \"origin\"]\n\turl = https://u:p@example.invalid/b.git\n"); // secret-scan: ignore
        assert_eq!(remotes_with_credentials(&twice), vec!["origin".to_string()]);
    }

    #[test]
    fn the_check_names_the_remote_and_never_shows_the_credential() {
        let root = dir("remote");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git").join("config"), LEAKED).unwrap();
        let k = check_remote_credentials(&root);
        assert_eq!(k.status, Status::Fail);
        assert!(k.detail.contains("remote 'origin' has a credential in its URL"), "{}", k.detail);
        let everything = format!("{} {} {}", k.name, k.detail, k.fix.clone().unwrap_or_default());
        for secret in ["bench-user", "not-a-real-token", "1234567890", "example.invalid"] {
            assert!(!everything.contains(secret), "{secret} must not be shown: {everything}");
        }
        assert!(k.fix.as_deref().unwrap_or("").contains("revoke"), "it says what to do");
        // A clean config passes, and a folder with no .git is only information.
        std::fs::write(root.join(".git").join("config"), CLEAN).unwrap();
        assert_eq!(check_remote_credentials(&root).status, Status::Ok);
        assert_eq!(check_remote_credentials(&dir("nogit")).status, Status::Info);
        // It is part of the doctor's run, next to the data folder.
        std::fs::write(root.join(".git").join("config"), LEAKED).unwrap();
        let r = run(&root.join("data"), false);
        assert_eq!(find(&r, "git remotes").status, Status::Fail);
    }

    #[test]
    fn a_fresh_install_is_set_up_with_safe_defaults_and_passes() {
        let d = dir("fresh").join("data");
        let r = run(&d, false);
        for f in ["fs.json", "term.json", "repos.json", "ssh.json", "db.json", "http.json", "docker.json", "k8s.json", "iac.json", "cloud.json", "mon.json", "policy.json"] {
            assert!(matches!(find(&r, f).status, Status::Ok), "{f}: {:?}", find(&r, f));
            assert!(d.join("tools").join(if f == "repos.json" { "repos.json" } else { f }).exists(), "{f} was written");
        }
        assert!(find(&r, "credential store").status == Status::Ok, "{:?}", find(&r, "credential store"));
        assert!(find(&r, "k8s.json").detail.contains("0 cluster"));
        assert!(find(&r, "cloud.json").detail.contains("none"));
        assert_eq!(find(&r, "API token").status, Status::Info);
        assert!(!r.iter().any(|k| k.status == Status::Fail), "{:?}", r.iter().filter(|k| k.status == Status::Fail).collect::<Vec<_>>());
        // A second run changes nothing.
        let before: Vec<Vec<u8>> = ["policy.json", "fs.json"].iter().map(|f| std::fs::read(d.join("tools").join(f)).unwrap()).collect();
        let _ = run(&d, false);
        let after: Vec<Vec<u8>> = ["policy.json", "fs.json"].iter().map(|f| std::fs::read(d.join("tools").join(f)).unwrap()).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn broken_files_are_failures_with_a_fix_and_are_never_overwritten() {
        let d = dir("broken").join("data");
        let _ = run(&d, false);
        std::fs::write(d.join("tools").join("policy.json"), r#"{"mode": "yolo"}"#).unwrap();
        std::fs::write(d.join("tools").join("k8s.json"), r#"{"contexts":[{"name":"x","namespaces":["*"]}]}"#).unwrap();
        std::fs::write(d.join(".aqua_api_token"), "short").unwrap();
        let r = run(&d, false);
        for name in ["policy.json", "k8s.json", "API token"] {
            let k = find(&r, name);
            assert_eq!(k.status, Status::Fail, "{name}");
            assert!(k.fix.is_some(), "{name} says how to fix it");
        }
        assert_eq!(std::fs::read_to_string(d.join("tools").join("k8s.json")).unwrap(), r#"{"contexts":[{"name":"x","namespaces":["*"]}]}"#);
        let (text, failed) = render(&r);
        assert!(failed && text.contains("[ FAIL ] policy.json") && text.contains("fix:"), "{text}");
    }

    #[test]
    fn a_mistake_in_a_config_file_is_placed_in_the_persons_file_and_a_misspelt_key_gets_a_hint() {
        let d = dir("positions").join("data");
        let _ = run(&d, false);
        // Comments are fine; the missing comma on line 4 is the mistake, and the doctor says where it is in the file as written.
        std::fs::write(d.join("tools").join("k8s.json"), "// my clusters\n{\n  \"contexts\": []\n  \"max_replicas\": 3\n}").unwrap();
        // A misspelt key in a file that is otherwise right: refused, with the nearest known key.
        std::fs::write(d.join("tools").join("db.json"), "{\"profile\": []}").unwrap();
        // A file with comments and a trailing comma is read, and the doctor says nothing is wrong with it.
        std::fs::write(d.join("tools").join("mon.json"), "// nothing yet\n{\"probes\": [],}\n").unwrap();
        let r = run(&d, false);
        let k = find(&r, "k8s.json");
        assert_eq!(k.status, Status::Fail);
        assert!(k.detail.contains("line 4, column 3: expected"), "{}", k.detail);
        let db = find(&r, "db.json");
        assert_eq!(db.status, Status::Fail);
        assert_eq!(find(&r, "db.json key").detail, "unknown key `profile`: did you mean `profiles`?");
        assert_eq!(find(&r, "db.json key").status, Status::Warn);
        assert_eq!(find(&r, "mon.json").status, Status::Ok);
        assert!(!r.iter().any(|c| c.name == "mon.json key"));
    }

    #[test]
    fn a_tampered_audit_log_and_a_missing_root_are_reported_and_fix_creates_the_root() {
        let d = dir("audit").join("data");
        let _ = run(&d, false);
        let audit = aqua_tool_host::AuditLedger::open(d.join("tools").join("host.audit.jsonl"));
        for _ in 0..3 {
            audit.append(&aqua_tool_host::AuditEvent {
                request_id: "r".into(),
                session_id: "s".into(),
                agent_run_id: "".into(),
                caller: "agent".into(),
                tool: "fs.read".into(),
                args: serde_json::json!({}),
                decision: "auto".into(),
                classes: vec!["READ".into()],
                ok: true,
                error_code: None,
                duration_ms: 1,
                shadow: None,
                approval: None,
            }).unwrap();
        }
        assert!(find(&run(&d, false), "audit log").detail.contains("3 records"));
        let text = std::fs::read_to_string(audit.path()).unwrap().replacen("fs.read", "fs.wipe", 1);
        std::fs::write(audit.path(), text).unwrap();
        assert_eq!(find(&run(&d, false), "audit log").status, Status::Fail);
        // A root that does not exist.
        let root = d.parent().unwrap().join("missing_ws");
        let cfg = aqua_native_tools::fs::FsConfig::with_workspace(root.to_string_lossy().to_string());
        std::fs::write(d.join("tools").join("fs.json"), serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(find(&run(&d, false), "root @ws").status, Status::Warn);
        assert!(!root.exists());
        assert_eq!(find(&run(&d, true), "root @ws").status, Status::Ok);
        assert!(root.is_dir());
    }

    #[test]
    fn a_voiceprint_whose_key_is_gone_is_reported_with_what_to_do_and_one_whose_key_is_there_is_fine() {
        let d = dir("voiceprint").join("data");
        // Nothing is looked at where there is no database (a fresh install gets no such line).
        assert!(!run(&d, false).iter().any(|k| k.name == "enrolled voice"));
        let keys = Arc::new(MemorySecrets::new());
        let db = crate::storage::DatabaseManager::with_secrets(&d, keys.clone()).unwrap();
        assert_eq!(find(&run_with(&d, false, keys.clone()), "enrolled voice").status, Status::Info, "no voice enrolled yet");
        db.save_master_voice_profile(&[0.1, 0.2, 0.3], "aqua", "").unwrap();
        drop(db);
        assert_eq!(find(&run_with(&d, false, keys.clone()), "enrolled voice").status, Status::Ok);
        // The key is gone from the credential store: the print is there and cannot be opened.
        let lost = run_with(&d, false, Arc::new(MemorySecrets::new()));
        let v = find(&lost, "enrolled voice");
        assert_eq!(v.status, Status::Fail);
        assert!(v.detail.contains("key is missing") && v.fix.as_deref().is_some_and(|f| f.contains("enrol your voice again")), "{v:?}");
    }

    #[test]
    fn the_report_never_contains_secret_values() {
        let d = dir("secret").join("data");
        std::fs::create_dir_all(&d).unwrap();
        let token = "SECRET-TOKEN-VALUE-0123456789abcdefghijklmnop";
        std::fs::write(d.join(".aqua_api_token"), token).unwrap();
        let (text, _) = render(&run(&d, false));
        assert!(!text.contains(token) && text.contains("API token: present (not shown)"), "{text}");
    }

    // ---- The second report: the token files and the live probe. Nothing below runs git or icacls, or touches a real token.

    const P: &str = r"E:\Personal_AI\Aqua\data\.aqua_api_token";

    fn me() -> Account {
        Account { name: "lenovo".into(), domain: "VISHAL".into() }
    }

    /// The text `icacls <path>` prints: the first entry after the path, the others indented to the same column, an empty line and the count of files.
    fn icacls_text(path: &str, entries: &[&str]) -> String {
        let pad = " ".repeat(path.len() + 1);
        let mut s = String::new();
        for (i, e) in entries.iter().enumerate() {
            if i == 0 {
                s.push_str(&format!("{path} {e}\n"));
            } else {
                s.push_str(&format!("{pad}{e}\n"));
            }
        }
        s.push_str("\nSuccessfully processed 1 files; Failed processing 0 files\n");
        s
    }

    fn acl(entries: &[&str]) -> Access {
        Access::Listed(parse_icacls(&icacls_text(P, entries), P))
    }

    fn secret(access: Access) -> SecretFile {
        SecretFile { exists: true, kind: FileKind::Regular, size: 64, tracked: Tracked::No, access }
    }

    /// `key=value Verdict` for each line, to compare a whole report at once.
    fn shape(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| format!("{}={} {:?}", l.key, l.value, l.verdict)).collect()
    }

    #[test]
    fn the_access_list_is_read_from_icacls_text_with_the_path_written_any_way() {
        let one = parse_icacls(&icacls_text(P, &[r"VISHAL\lenovo:(R,W)"]), P);
        assert_eq!(one, vec![AclEntry { principal: r"VISHAL\lenovo".into(), flags: vec!["R,W".into()] }], "the count line is not an entry");
        let many = icacls_text(
            P,
            &[
                r"NT AUTHORITY\SYSTEM:(I)(F)",
                r"BUILTIN\Administrators:(I)(F)",
                r"BUILTIN\Users:(I)(RX)",
                r"NT AUTHORITY\Authenticated Users:(I)(M)",
                r"Everyone:(DENY)(W)",
                r"VISHAL\lenovo:(I)(F)",
                r"CREATOR OWNER:(I)(OI)(CI)(IO)(F)",
                r"Mandatory Label\Medium Mandatory Level:(NW)",
            ],
        );
        let entries = parse_icacls(&many, P);
        let names: Vec<&str> = entries.iter().map(|e| e.principal.as_str()).collect();
        assert_eq!(
            names,
            vec![r"NT AUTHORITY\SYSTEM", r"BUILTIN\Administrators", r"BUILTIN\Users", r"NT AUTHORITY\Authenticated Users", "Everyone", r"VISHAL\lenovo", "CREATOR OWNER", r"Mandatory Label\Medium Mandatory Level"]
        );
        assert_eq!(entries[4].flags, vec!["DENY", "W"]);
        assert_eq!(entries[6].flags, vec!["I", "OI", "CI", "IO", "F"]);
        // icacls echoes the path as it was typed: forward slashes in the path give the same names.
        let slashes = icacls_text("E:/x/data/.aqua_api_token", &[r"NT AUTHORITY\SYSTEM:(F)", r"VISHAL\lenovo:(F)"]);
        let e = parse_icacls(&slashes, r"E:\x\data\.aqua_api_token");
        assert_eq!(e.iter().map(|e| e.principal.as_str()).collect::<Vec<_>>(), vec![r"NT AUTHORITY\SYSTEM", r"VISHAL\lenovo"]);
        // An echo that is not the path given: the column of the indented lines shows where the names start.
        let odd = icacls_text("some other echo", &[r"NT AUTHORITY\SYSTEM:(F)", r"VISHAL\lenovo:(F)"]);
        assert_eq!(parse_icacls(&odd, r"E:\x\data\.aqua_api_token")[0].principal, r"NT AUTHORITY\SYSTEM");
        // Text with no entry in it gives none.
        assert!(parse_icacls("The system cannot find the file specified.\n", P).is_empty());
    }

    #[test]
    fn the_current_user_system_and_administrators_are_fine_and_everyone_users_and_authenticated_users_are_findings() {
        let many = icacls_text(
            P,
            &[
                r"NT AUTHORITY\SYSTEM:(I)(F)",
                r"BUILTIN\Administrators:(I)(F)",
                r"BUILTIN\Users:(I)(RX)",
                r"NT AUTHORITY\Authenticated Users:(I)(M)",
                r"Everyone:(DENY)(W)",
                r"VISHAL\lenovo:(I)(F)",
                r"CREATOR OWNER:(I)(OI)(CI)(IO)(F)",
                r"Mandatory Label\Medium Mandatory Level:(NW)",
            ],
        );
        let v = classify_acl(&parse_icacls(&many, P), &me());
        assert_eq!(v.fine, vec![r"NT AUTHORITY\SYSTEM", r"BUILTIN\Administrators", r"VISHAL\lenovo"]);
        assert_eq!(v.broad, vec![r"BUILTIN\Users", r"NT AUTHORITY\Authenticated Users"], "a refusal, an inherit-only entry and the label are not grants");
        assert!(v.other.is_empty());
        // Everyone and a well-known SID granted access are findings; so is another account, even one with the same short name.
        let more = icacls_text(P, &[r"Everyone:(R)", r"S-1-1-0:(R)", r"OTHERPC\lenovo:(F)", r"OTHERPC\bob:(R)", r"VISHAL\lenovo:(F)"]);
        let v = classify_acl(&parse_icacls(&more, P), &me());
        assert_eq!(v.broad, vec!["Everyone", "S-1-1-0"]);
        assert_eq!(v.other, vec![r"OTHERPC\lenovo", r"OTHERPC\bob"]);
        assert_eq!(v.fine, vec![r"VISHAL\lenovo"]);
        // The user is also found by the short name alone, and without a known domain.
        let alone = classify_acl(&[AclEntry { principal: "lenovo".into(), flags: vec!["F".into()] }], &Account { name: "lenovo".into(), domain: String::new() });
        assert_eq!(alone.fine, vec!["lenovo"]);
    }

    #[test]
    fn git_exit_codes_say_tracked_not_tracked_or_not_checked() {
        assert_eq!(tracked_from_git(Some(0)), Tracked::Yes);
        assert_eq!(tracked_from_git(Some(1)), Tracked::No);
        for unknown in [tracked_from_git(Some(128)), tracked_from_git(Some(7)), tracked_from_git(None)] {
            assert!(matches!(&unknown, Tracked::Unknown(why) if why.starts_with("not checked")), "{unknown:?}");
        }
    }

    #[test]
    fn a_file_that_is_as_it_should_be_gives_five_current_lines_and_each_fault_is_drift() {
        let who = me();
        let good = secret(acl(&[r"VISHAL\lenovo:(R,W)"]));
        let lines = secret_file_lines("api_token", ".aqua_api_token", &good, &who);
        assert_eq!(
            shape(&lines),
            vec!["api_token.exists=yes Current", "api_token.regular_file=yes Current", "api_token.size=64 Current", "api_token.tracked_by_git=no Current", "api_token.access=restricted Current"]
        );
        assert_eq!(lines[2].note, "present, 64 bytes");
        // Not created yet is not a fault: Aqua makes it at its first start. Nothing else is said about a file that is not there.
        let absent = SecretFile { exists: false, ..good.clone() };
        assert_eq!(shape(&secret_file_lines("ui_token", ".aqua_ui_token", &absent, &who)), vec!["ui_token.exists=no Current"]);
        // A link (or a junction) is drift, and the rest is not asked about it.
        let link = SecretFile { kind: FileKind::Link, ..good.clone() };
        assert_eq!(shape(&secret_file_lines("api_token", ".aqua_api_token", &link, &who)), vec!["api_token.exists=yes Current", "api_token.regular_file=link Drift"]);
        // A file too short to hold a token.
        let short = SecretFile { size: 5, ..good.clone() };
        let l = secret_file_lines("api_token", ".aqua_api_token", &short, &who);
        assert_eq!(shape(&l)[2], "api_token.size=5 Drift");
        assert!(l[2].note.starts_with("present, 5 bytes"), "{:?}", l[2]);
        // Tracked by git is drift; git missing is a note.
        let tracked = SecretFile { tracked: Tracked::Yes, ..good.clone() };
        let l = secret_file_lines("api_token", ".aqua_api_token", &tracked, &who);
        assert_eq!(shape(&l)[3], "api_token.tracked_by_git=yes Drift");
        assert!(l[3].note.contains("revoke"), "{:?}", l[3]);
        let no_git = SecretFile { tracked: Tracked::Unknown("not checked: git is not installed".into()), ..good.clone() };
        let l = secret_file_lines("api_token", ".aqua_api_token", &no_git, &who);
        assert_eq!(shape(&l)[3], "api_token.tracked_by_git=skipped Current");
        assert_eq!(l[3].note, "not checked: git is not installed");
        // Who can open it.
        let everyone = secret(acl(&[r"VISHAL\lenovo:(F)", "Everyone:(R)"]));
        let l = secret_file_lines("api_token", ".aqua_api_token", &everyone, &who);
        assert_eq!(shape(&l)[4], "api_token.access=Everyone Drift");
        assert!(l[4].note.contains("Everyone can open this file") && l[4].note.contains("icacls"), "{:?}", l[4]);
        let groups = secret(acl(&[r"NT AUTHORITY\SYSTEM:(I)(F)", r"BUILTIN\Users:(I)(RX)", r"NT AUTHORITY\Authenticated Users:(I)(M)", r"VISHAL\lenovo:(I)(F)"]));
        let l = secret_file_lines("api_token", ".aqua_api_token", &groups, &who);
        assert_eq!(shape(&l)[4], r"api_token.access=BUILTIN\Users,NT AUTHORITY\Authenticated Users Drift");
        let unreadable = secret(Access::Unknown("not checked: the access list is only read on Windows".into()));
        let l = secret_file_lines("api_token", ".aqua_api_token", &unreadable, &who);
        assert_eq!(shape(&l)[4], "api_token.access=skipped Current");
        // The system and the administrators alone are fine.
        let admin = secret(acl(&[r"NT AUTHORITY\SYSTEM:(F)", r"BUILTIN\Administrators:(F)"]));
        assert_eq!(shape(&secret_file_lines("api_token", ".aqua_api_token", &admin, &who))[4], "api_token.access=restricted Current");
    }

    #[test]
    fn the_lines_read_key_value_and_state_and_the_report_says_whether_anything_drifts() {
        let lines = vec![
            line("a.b", "x y", Verdict::Current, ""),
            line("c", "d", Verdict::Drift, "why"),
            line("e", "", Verdict::Current, ""),
        ];
        let (text, drift) = render_lines(&lines);
        assert!(drift);
        assert!(text.contains("\na.b=x_y current\n") && text.contains("\nc=d drift  (why)\n") && text.contains("\ne=- current\n"), "{text}");
        assert!(text.ends_with("1 line(s) drift.\n"), "{text}");
        let (text, drift) = render_lines(&lines[..1]);
        assert!(!drift && text.ends_with("0 line(s) drift.\n"), "{text}");
    }

    #[test]
    fn kinds_of_file_are_told_apart_by_the_name_itself() {
        assert_eq!(file_kind(false, false, false, true), FileKind::Regular);
        assert_eq!(file_kind(true, false, false, false), FileKind::Directory);
        assert_eq!(file_kind(false, true, false, false), FileKind::Link);
        assert_eq!(file_kind(false, false, true, true), FileKind::Link, "a reparse point is a link even when it looks like a file");
        assert_eq!(file_kind(false, false, false, false), FileKind::Other);
    }

    #[test]
    fn a_token_file_is_measured_and_never_opened_so_its_text_is_nowhere_in_the_report() {
        let d = dir("stat");
        std::fs::create_dir_all(&d).unwrap();
        let token = format!("MARKER-{}", "x".repeat(57));
        assert_eq!(token.len(), 64);
        std::fs::write(d.join(".aqua_api_token"), &token).unwrap();
        let (exists, kind, size) = stat_file(&d.join(".aqua_api_token"));
        assert_eq!((exists, kind, size), (true, FileKind::Regular, 64));
        assert_eq!(stat_file(&d.join("nothing_here")), (false, FileKind::Other, 0));
        let (exists, kind, _) = stat_file(&d);
        assert!(exists && kind == FileKind::Directory);
        let facts = SecretFile { exists, kind: FileKind::Regular, size, tracked: Tracked::No, access: Access::Unknown("not checked".into()) };
        let (text, drift) = render_lines(&secret_file_lines("api_token", ".aqua_api_token", &facts, &me()));
        assert!(!drift && text.contains("api_token.size=64 current  (present, 64 bytes)") && !text.contains("MARKER"), "{text}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_data_folder_without_tokens_gives_two_lines_and_asks_neither_git_nor_icacls() {
        let d = dir("probe_absent");
        let lines = probe_lines(&d, None);
        assert_eq!(shape(&lines), vec!["api_token.exists=no Current", "ui_token.exists=no Current"]);
    }

    #[test]
    fn a_status_line_is_read_and_anything_else_is_not() {
        assert_eq!(parse_status_line(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"), Some(200));
        assert_eq!(parse_status_line(b"HTTP/1.0 503 Service Unavailable"), Some(503));
        for bad in [&b""[..], b"garbage\r\n", b"HTTP/1.1 2000 OK\r\n", b"HTTP/1.1 abc OK\r\n", b"HTTP/2 200\r\n", b"\xff\xfe\r\n"] {
            assert_eq!(parse_status_line(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
    }

    /// A listener on a free loopback port that reads one request, answers it with `response`, and hands back the request text.
    fn serve_once(response: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 256];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = s.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
            }
            s.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (base, handle)
    }

    #[test]
    fn the_health_probe_sends_one_plain_get_without_a_token_and_reads_the_status() {
        let (base, server) = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        assert_eq!(probe_health(&base, Duration::from_secs(5)), Health::Ok);
        let request = server.join().unwrap();
        let lower = request.to_ascii_lowercase();
        assert!(request.starts_with("GET /api/v1/health HTTP/1.1\r\n") && request.ends_with("\r\n\r\n"), "{request}");
        assert_eq!(request.matches("GET ").count(), 1, "one request and no other");
        assert!(!lower.contains("authorization") && !lower.contains("cookie") && !lower.contains("token"), "{request}");
        // The line the doctor prints for it.
        let (base, server) = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        let lines = live_lines(&base, Duration::from_secs(5));
        server.join().unwrap();
        assert_eq!(shape(&lines), vec!["core_health=ok Current"]);
        // A core that is still starting answers 503, which is not healthy.
        let (base, server) = serve_once("HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(probe_health(&base, Duration::from_secs(5)), Health::Status(503));
        server.join().unwrap();
        let (base, server) = serve_once("HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n");
        let lines = live_lines(&base, Duration::from_secs(5));
        server.join().unwrap();
        assert_eq!(shape(&lines), vec!["core_health=http_503 Drift"]);
    }

    #[test]
    fn a_listener_that_never_answers_a_closed_port_and_a_stranger_are_unreachable_and_never_a_hang() {
        // A listener that takes the connection into its queue and says nothing: the deadline ends the probe.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", silent.local_addr().unwrap());
        let started = Instant::now();
        match probe_health(&base, Duration::from_millis(300)) {
            Health::Unreachable(why) => assert!(why.contains("no answer"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(5), "the deadline holds");
        assert_eq!(shape(&live_lines(&base, Duration::from_millis(300))), vec!["core_health=unreachable Drift"]);
        drop(silent);
        // A port nothing listens on.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        assert!(matches!(probe_health(&closed, Duration::from_secs(5)), Health::Unreachable(_)));
        // A listener that accepts and hangs up.
        let hangup = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", hangup.local_addr().unwrap());
        let server = std::thread::spawn(move || drop(hangup.accept().unwrap()));
        assert!(matches!(probe_health(&base, Duration::from_secs(5)), Health::Unreachable(_)));
        server.join().unwrap();
        // Something that is not an HTTP answer.
        let (base, server) = serve_once("this is not http\r\n");
        assert!(matches!(probe_health(&base, Duration::from_secs(5)), Health::Unreachable(why) if why.contains("not an HTTP answer")));
        server.join().unwrap();
        // The probe goes to this computer only, and it does not even try another address or a name.
        for stranger in ["http://192.168.1.5:8767", "http://example.invalid:8767", "https://127.0.0.1:8767", "127.0.0.1:8767", "http://127.0.0.1:8767/x"] {
            assert!(matches!(probe_health(stranger, Duration::from_secs(5)), Health::Unreachable(why) if why.contains("only an address")), "{stranger}");
        }
    }
}
