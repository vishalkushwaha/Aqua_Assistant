//! The tool `research.run(question, depth, name?)` (card R2-10), registered with the tool host only when `AQUA_RESEARCH=on`.
//!
//! Like every tool it goes through `ToolHost::call`: the arguments are validated, the call is decided (the classes are Network and Write: it reads the web and saves a report), asked about
//! if the policy says so, written in the audit log, and its output is labelled as outside text, so the run counts as having read something untrusted. Inside the call, the searches and the
//! pages are `web.search` and `web.read` calls to the same host, each decided and audited by itself (a site that is not on the allow list asks, as it does for the agent).
//!
//! The report is saved as a new file in the `research` folder of the first folder the file tools may use. It is written through the file tools' own path guard, never over a file that
//! is there (a name that exists gets `-2`, `-3`), and a name a person asks for cannot hold a folder, a drive or `..`. Where the file tools are off, no file is written and the text is
//! returned all the same.

use super::plan::Tier;
use super::{report, Answer, Hit, LocalBook, Page, RunBook, Services};
use aqua_native_tools::fs::guard::{check_opened, display, Op, PathGuard};
use aqua_tool_host::provider::ClassifyContext;
use aqua_tool_host::{
    Annotations, Artifact, Caller, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, RequestContext, ToolError, ToolHost, ToolManifest, ToolProvider, ToolRequest,
    ToolResult,
};
use chrono::NaiveDateTime;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

pub const TOOL: &str = "research.run";
/// The folder of the workspace the reports are saved in.
pub const REPORT_DIR: &str = "research";
/// The longest text the tool hands back (the host cuts at 6,000 characters); the whole report is in the file.
const MAX_TEXT_CHARS: usize = 4_500;
const MAX_QUESTION_CHARS: usize = 2_000;
/// How many characters of a page `web.read` is asked for.
const PAGE_CHARS: u64 = 4_000;

/// What the services of one call need to know.
pub struct ServiceCtx {
    /// The agent run the call belongs to (empty outside a run).
    pub run_id: String,
    /// Nobody is there to answer a question (a background run, a flow): a search or a page that would have to ask is refused at once.
    pub alone: bool,
    pub cancel: Arc<AtomicBool>,
}

/// Makes the search and the reader for one call. The real one calls the tool host; a test gives a fake.
pub type MakeServices = Arc<dyn Fn(&ServiceCtx) -> Box<dyn Services> + Send + Sync>;

/// The search and the reader through the tool host: `web.search` and `web.read`, as the agent. A page is read only if the research gives its address, which it takes from a search result or
/// the person's own words.
pub struct HostServices {
    host: Weak<ToolHost>,
    run_id: String,
    alone: bool,
    cancel: Arc<AtomicBool>,
}

impl HostServices {
    pub fn new(host: Weak<ToolHost>, c: &ServiceCtx) -> Self {
        Self { host, run_id: c.run_id.clone(), alone: c.alone, cancel: c.cancel.clone() }
    }

    fn call(&self, tool: &str, arguments: Value) -> Result<ToolResult, String> {
        let host = self.host.upgrade().ok_or_else(|| "the tool host is not available".to_string())?;
        let mut ctx = RequestContext::new(Caller::Agent);
        ctx.agent_run_id = self.run_id.clone();
        ctx.unattended = self.alone;
        ctx.no_one_to_ask = self.alone;
        let res = host.call_blocking(ToolRequest { tool: tool.to_string(), arguments, context: ctx });
        if res.ok {
            Ok(res)
        } else {
            Err(res.error.as_ref().map(|e| e.message.clone()).unwrap_or_else(|| res.text.clone()))
        }
    }
}

/// The results of a `web.search` call, from its data.
pub(crate) fn hits_from(data: &Value) -> Vec<Hit> {
    let text = |r: &Value, k: &str| r.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    data.get("results")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(|r| Hit { title: text(r, "title"), url: text(r, "url"), snippet: text(r, "snippet"), date: r.get("date").and_then(|v| v.as_str()).map(str::to_string) }).collect())
        .unwrap_or_default()
}

/// The text of a page out of what `web.read` prints: without the lines of its head (title, address, date), which the data holds, and without the list of links at the end.
pub(crate) fn page_body(text: &str) -> String {
    let mut rest = text;
    if rest.starts_with("Title: ") || rest.starts_with("URL: ") {
        if let Some((_, after)) = rest.split_once("\n\n") {
            rest = after;
        }
    }
    let rest = rest.split("\n\nLinks:\n").next().unwrap_or(rest);
    rest.trim().to_string()
}

/// A `web.read` result as a page, or why it is none.
pub(crate) fn page_from(res: &ToolResult, asked: &str) -> Result<Page, String> {
    if let Some(blocked) = res.data.get("redirect_blocked").and_then(|v| v.as_str()) {
        return Err(format!("the page redirects to {blocked}, which is not allowed"));
    }
    let status = res.data.get("status").and_then(|v| v.as_u64()).unwrap_or(0);
    if status >= 400 {
        return Err(format!("the site answered HTTP {status}"));
    }
    let body = match aqua_tool_host::output::split_untrusted(&res.text) {
        Some((_, inner)) => inner,
        None => res.text.as_str(),
    };
    Ok(Page {
        url: res.data.get("url").and_then(|v| v.as_str()).unwrap_or(asked).to_string(),
        title: res.data.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        date: res.data.get("date").and_then(|v| v.as_str()).map(str::to_string),
        text: page_body(body),
    })
}

impl Services for HostServices {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>, String> {
        let res = self.call("web.search", json!({"query": query, "limit": limit}))?;
        Ok(hits_from(&res.data))
    }

    fn read(&self, url: &str, goal: &str) -> Result<Page, String> {
        let res = self.call("web.read", json!({"url": url, "goal": goal, "max_chars": PAGE_CHARS}))?;
        page_from(&res, url)
    }

    fn may_read(&self, url: &str) -> bool {
        // In a run that records its sources, an address in the question is read only if the person's own request held it (or a search of the run named it).
        if !self.run_id.is_empty() && crate::sources::is_tracking(&self.run_id) {
            crate::sources::may_read(&self.run_id, url)
        } else {
            true
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

// ---- the arguments and the report file --------------------------------------------------------------------------------------------

struct Args {
    question: String,
    depth: u8,
    name: Option<String>,
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArguments, message)
}

fn parse_args(args: &Value) -> Result<Args, ToolError> {
    let question = args.get("question").and_then(|v| v.as_str()).unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
    if question.is_empty() || question.chars().count() > MAX_QUESTION_CHARS {
        return Err(invalid(format!("question is the question to research, 1 to {MAX_QUESTION_CHARS} characters")));
    }
    let depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(3).clamp(2, 3) as u8;
    let name = match args.get("name").and_then(|v| v.as_str()).map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => Some(report::requested_name(n).map_err(invalid)?),
        None => None,
    };
    Ok(Args { question, depth, name })
}

fn io_error(e: std::io::Error, what: &str) -> ToolError {
    ToolError::new(ErrorCode::OsPermissionDenied, format!("{what}: {e}"))
}

/// Saves the report as a new file in the report folder; returns where, how many bytes and the SHA-256. A file that is there is never overwritten: the name gets `-2`, `-3`.
fn write_report(guard: &PathGuard, file: &str, content: &str) -> Result<(String, u64, String), ToolError> {
    // The report holds sentences of pages: what hides text or breaks a terminal (escape sequences, zero-width and bidirectional controls) is taken out, as the host does for a result.
    let content = aqua_tool_host::output::sanitize(content);
    let content = content.as_str();
    let (stem, ext) = file.rsplit_once('.').unwrap_or((file, "md"));
    for n in 1..=20u32 {
        let candidate = if n == 1 { file.to_string() } else { format!("{stem}-{n}.{ext}") };
        let r = guard.resolve(&format!("{REPORT_DIR}/{candidate}"), Op::Write)?;
        if r.sensitive {
            return Err(invalid("that name looks like the name of a file that holds secrets; choose another"));
        }
        if let Some(dir) = r.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| io_error(e, "the report folder could not be made"))?;
        }
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&r.path) {
            Ok(mut f) => {
                check_opened(&f, &r)?;
                f.write_all(content.as_bytes()).map_err(|e| io_error(e, "the report could not be written"))?;
                let sha: String = Sha256::digest(content.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
                return Ok((display(&r.path), content.len() as u64, sha));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_error(e, "the report could not be made")),
        }
    }
    Err(ToolError::new(ErrorCode::AlreadyExists, "twenty reports with this name are there already; choose another name"))
}

// ---- the provider -----------------------------------------------------------------------------------------------------------------

pub struct ResearchProvider {
    guard: Option<Arc<PathGuard>>,
    make: MakeServices,
    clock: fn() -> NaiveDateTime,
}

impl ResearchProvider {
    /// The provider as the program uses it: the search and the pages through `host`, the reports in the first folder of `guard`.
    pub fn new(host: Weak<ToolHost>, guard: Option<Arc<PathGuard>>) -> Self {
        let make: MakeServices = Arc::new(move |c: &ServiceCtx| Box::new(HostServices::new(host.clone(), c)) as Box<dyn Services>);
        Self { guard, make, clock: || chrono::Local::now().naive_local() }
    }

    /// The provider with services and a clock that are given (tests).
    pub fn with_services(guard: Option<Arc<PathGuard>>, make: MakeServices, clock: fn() -> NaiveDateTime) -> Self {
        Self { guard, make, clock }
    }
}

/// Registers `research.run` with the host when `enabled`; true if it is registered. With the switch off nothing is registered, so the tool does not exist for anyone.
pub fn register(host: &Arc<ToolHost>, guard: Option<Arc<PathGuard>>, enabled: bool) -> bool {
    if !enabled {
        return false;
    }
    match host.register(Arc::new(ResearchProvider::new(Arc::downgrade(host), guard))) {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("tool host: could not register {TOOL}: {e}");
            false
        }
    }
}

/// The whole of a call, on a thread that may wait.
fn execute(args: Value, make: MakeServices, guard: Option<Arc<PathGuard>>, clock: fn() -> NaiveDateTime, c: ServiceCtx) -> Result<ProviderOutput, ToolError> {
    let a = parse_args(&args)?;
    let now = clock();
    let services = make(&c);
    let outcome = if !c.run_id.is_empty() && crate::sources::is_tracking(&c.run_id) {
        super::run(&a.question, a.depth, &*services, &mut RunBook { run_id: c.run_id.clone() }, now)
    } else {
        super::run(&a.question, a.depth, &*services, &mut LocalBook::default(), now)
    };
    if let Answer::SearchFailed(why) = &outcome.answer {
        return Err(ToolError::new(ErrorCode::BackendError, format!("the research could not search the web: {why}")));
    }
    let mut data = outcome.to_data();
    let mut artifacts: Vec<Artifact> = Vec::new();
    let mut after = String::new();
    // Only a research that searched has a report worth keeping.
    if matches!(outcome.tier, Tier::T2 | Tier::T3) {
        match &guard {
            None => after.push_str("\nNo report file was written: the file tools are off."),
            Some(g) => {
                // A name the person asked for is used as it is (and refused if it looks like a secrets file); a name the program makes from the question falls back to a plain one
                // when the question gave it a name that looks like one.
                let file = match &a.name {
                    Some(name) => name.clone(),
                    None => {
                        let made = report::file_name(&a.question, now);
                        let sensitive = g.resolve(&format!("{REPORT_DIR}/{made}"), Op::Write).map(|r| r.sensitive).unwrap_or(false);
                        if sensitive {
                            report::file_name("research", now)
                        } else {
                            made
                        }
                    }
                };
                match write_report(g, &file, &report::render(&outcome)) {
                    Ok((shown, bytes, sha256)) => {
                        data["report"] = json!(shown);
                        after.push_str(&format!("\nReport file: {shown}"));
                        artifacts.push(Artifact { kind: "report".into(), path: shown, bytes, sha256 });
                    }
                    Err(e) => after.push_str(&format!("\nThe report file could not be written: {}", super::one_line(&e.message, 200))),
                }
            }
        }
    }
    let text = format!("{}{after}", report::capped(&report::render(&outcome), MAX_TEXT_CHARS));
    Ok(ProviderOutput { data, text, artifacts, ..Default::default() })
}

impl ToolProvider for ResearchProvider {
    fn id(&self) -> &str {
        "research"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        vec![ToolManifest {
            name: TOOL.into(),
            title: TOOL.into(),
            description: "Research a question: searches the web, reads one or two pages, cross-checks the sources, saves a cited report. Slow: web.search is for plain lookups.".into(),
            classes: vec![PermClass::Network, PermClass::Write],
            default_decision: Decision::Auto,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string", "minLength": 1, "maxLength": MAX_QUESTION_CHARS},
                    "depth": {"type": "integer", "minimum": 2, "maximum": 3, "default": 3},
                    "name": {"type": "string", "maxLength": 80}
                },
                "required": ["question"]
            }),
            timeout_default_ms: 150_000,
            timeout_max_ms: 300_000,
            annotations: Annotations { read_only: false, destructive: false, idempotent: false, open_world: true },
            untrusted_output: true,
        }]
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        if tool != TOOL {
            return Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{tool}'")));
        }
        let a = parse_args(args)?;
        let (place, trusted) = match &self.guard {
            None => ("no report file (the file tools are off)".to_string(), true),
            Some(g) => {
                let file = a.name.clone().unwrap_or_else(|| "report.md".to_string());
                match g.resolve(&format!("{REPORT_DIR}/{file}"), Op::Write) {
                    Ok(r) if r.sensitive => return Err(invalid("that name looks like the name of a file that holds secrets; choose another")),
                    Ok(r) => (format!("a report in @{}/{REPORT_DIR}/", r.root_alias), r.trusted),
                    // A name the person gave that the guard refuses is refused; the program's own name failing only means no file is made.
                    Err(e) if a.name.is_some() => return Err(e),
                    Err(e) => (format!("no report file ({})", super::one_line(&e.message, 80)), true),
                }
            }
        };
        Ok(Classification {
            classes: vec![],
            target: format!("the web; {place}"),
            summary: format!("Research \"{}\" on the web (depth {}) and save {place}", super::one_line(&a.question, 100), a.depth),
            target_allowed: trusted,
        })
    }

    /// The research uses the web and the person's folder for its own ends, so it is for the agent, the app and the API: an outside MCP client, a replayed recipe and a spoken request are refused.
    fn classify_for(&self, tool: &str, args: &Value, ctx: &ClassifyContext) -> Result<Classification, ToolError> {
        if matches!(ctx.caller, Caller::McpClient | Caller::Replay | Caller::Voice) {
            return Err(ToolError::new(ErrorCode::PermissionDenied, "research.run is for the agent, the app and the API; an outside client, a replayed recipe or a spoken request may not start it"));
        }
        self.classify(tool, args)
    }

    fn call<'a>(&'a self, _tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let (make, guard, clock) = (self.make.clone(), self.guard.clone(), self.clock);
        // Read here, on the thread that made the call: whether nobody is there to answer is a property of that thread.
        let c = ServiceCtx { run_id: ctx.agent_run_id.clone(), alone: crate::agent_tools::nobody_to_ask(), cancel: ctx.cancel.clone() };
        Box::pin(async move {
            tokio::task::spawn_blocking(move || execute(args, make, guard, clock, c))
                .await
                .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the research task stopped unexpectedly: {e}")))?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(data: Value, text: &str) -> ToolResult {
        ToolResult { ok: true, tool: "web.read".into(), request_id: String::new(), data, text: text.into(), truncated: false, artifacts: Vec::new(), warnings: Vec::new(), approval: None, duration_ms: 0, error: None }
    }

    #[test]
    fn the_results_of_a_search_are_read_from_its_data_and_a_missing_list_is_no_results() {
        let data = json!({"query": "q", "results": [
            {"title": "A", "url": "https://a.example/x", "snippet": "S", "date": null, "provider": "brave"},
            {"title": "B", "url": "https://b.example/y", "snippet": "T", "date": "3 days ago", "provider": "brave"},
            {"title": "C"}
        ]});
        let hits = hits_from(&data);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0], Hit { title: "A".into(), url: "https://a.example/x".into(), snippet: "S".into(), date: None });
        assert_eq!(hits[1].date.as_deref(), Some("3 days ago"));
        assert_eq!(hits[2].url, "", "a result with no address is kept as it came and is left out by the research");
        assert!(hits_from(&json!({})).is_empty() && hits_from(&json!({"results": "no"})).is_empty() && hits_from(&Value::Null).is_empty());
    }

    #[test]
    fn the_text_of_a_page_is_what_web_read_prints_without_its_head_and_its_list_of_links() {
        let printed = "Title: Announcing Rust 1.90.0\nURL: https://blog.rust-lang.org/x\nDate: 2025-09-14\n\nThe default linker is now lld.\nIt links large programs faster.\n\nLinks:\n- [Cargo guide](https://doc.rust-lang.org/cargo/)";
        assert_eq!(page_body(printed), "The default linker is now lld.\nIt links large programs faster.");
        assert_eq!(page_body("Plain text with no head."), "Plain text with no head.");
        assert_eq!(page_body("Title: x\nURL: y\n\n(no readable text on this page)"), "(no readable text on this page)");
        // A text that only looks like a head further down is text.
        assert_eq!(page_body("Intro.\n\nTitle: not a head\n\nMore."), "Intro.\n\nTitle: not a head\n\nMore.");
    }

    #[test]
    fn a_page_comes_from_the_fenced_result_and_a_refusal_or_an_error_page_is_no_page() {
        let body = "Title: Rust 1.90\nURL: https://blog.rust-lang.org/x\nDate: 2025-09-14\n\nThe default linker is now lld.";
        let fenced = aqua_tool_host::output::label_untrusted(body, "ab12cd34ef56");
        let ok = result(json!({"status": 200, "url": "https://blog.rust-lang.org/x", "title": "Rust 1.90", "date": "2025-09-14"}), &fenced);
        let page = page_from(&ok, "https://blog.rust-lang.org/x").unwrap();
        assert_eq!(page, Page { url: "https://blog.rust-lang.org/x".into(), title: "Rust 1.90".into(), date: Some("2025-09-14".into()), text: "The default linker is now lld.".into() });
        // No fence (the host always adds one; a result without it is still read).
        assert_eq!(page_from(&result(json!({"status": 200}), body), "https://asked.example/").unwrap().url, "https://asked.example/");
        let missing = result(json!({"status": 404, "url": "https://blog.rust-lang.org/x"}), "HTTP 404 Not Found\nTitle: x\n\nnot here");
        assert_eq!(page_from(&missing, "https://blog.rust-lang.org/x"), Err("the site answered HTTP 404".to_string()));
        let moved = result(json!({"status": 0, "redirect_blocked": "https://other.example/"}), "The page redirects to https://other.example/, which is not on the allow list, so it was not read.");
        assert!(page_from(&moved, "https://blog.rust-lang.org/x").unwrap_err().contains("redirects to https://other.example/"));
    }

    #[test]
    fn the_arguments_are_checked_a_depth_is_held_to_two_or_three_and_a_name_cannot_be_a_path() {
        let a = parse_args(&json!({"question": "  What is   the population of Pune? "})).unwrap();
        assert_eq!((a.question.as_str(), a.depth, a.name.clone()), ("What is the population of Pune?", 3, None));
        assert_eq!(parse_args(&json!({"question": "q q", "depth": 2})).unwrap().depth, 2);
        assert_eq!(parse_args(&json!({"question": "q q", "depth": 9})).unwrap().depth, 3);
        assert_eq!(parse_args(&json!({"question": "q q", "name": "Pune notes"})).unwrap().name.as_deref(), Some("Pune-notes.md"));
        assert_eq!(parse_args(&json!({"question": "q q", "name": "  "})).unwrap().name, None, "a blank name is no name");
        for bad in [json!({}), json!({"question": ""}), json!({"question": "   "}), json!({"question": 5}), json!({"question": "x".repeat(MAX_QUESTION_CHARS + 1)})] {
            assert_eq!(parse_args(&bad).err().map(|e| e.code), Some(ErrorCode::InvalidArguments), "{bad}");
        }
        for name in ["../evil", "a/b", "C:\\x", "..", "x..y"] {
            assert_eq!(parse_args(&json!({"question": "q q", "name": name})).err().map(|e| e.code), Some(ErrorCode::InvalidArguments), "{name}");
        }
    }

    #[test]
    fn the_manifest_is_one_the_host_accepts_and_says_what_the_tool_does() {
        let p = ResearchProvider::with_services(None, Arc::new(|_| unreachable!("no call is made")), || chrono::NaiveDate::from_ymd_opt(2026, 10, 8).unwrap().and_hms_opt(0, 0, 0).unwrap());
        let m = p.manifests();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].check(), Ok(()));
        assert_eq!(m[0].name, TOOL);
        assert!(m[0].classes.contains(&PermClass::Network) && m[0].classes.contains(&PermClass::Write), "it reads the web and saves a file");
        assert!(m[0].untrusted_output && !m[0].annotations.read_only);
        let signature = m[0].signature();
        assert!(signature.starts_with("research.run(") && signature.contains("question") && signature.contains("depth?") && signature.contains("name?"), "{signature}");
    }
}
