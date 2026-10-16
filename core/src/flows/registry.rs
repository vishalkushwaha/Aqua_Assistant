//! The catalogue of node kinds: what each is called, how risky it is, which ports it has, what it
//! produces, and what a person may configure. The editor draws its forms from this and the validator
//! checks against it, so a new kind of node is one entry here (plus its executor).

use super::model::NodeKind;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// Reads or creates things that stay inside Aqua.
    Low,
    /// Changes local files, spreadsheets or other data, or calls other services.
    Medium,
    /// Has an effect on the outside world that cannot be undone: publishing, sending, deleting.
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Text,
    LongText,
    Number,
    Bool,
    Choice,
    Json,
    Url,
}

#[derive(Debug, Clone, Serialize)]
pub struct FieldSpec {
    pub name: String,
    pub label: String,
    pub kind: FieldKind,
    pub required: bool,
    pub choices: Vec<String>,
    pub default: Option<Value>,
    pub help: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeSpec {
    pub kind: NodeKind,
    pub label: String,
    pub description: String,
    pub category: String,
    pub risk: Risk,
    /// The output ports edges may leave from. Most nodes have just "out".
    pub out_ports: Vec<String>,
    /// The values the node produces, reachable as `{{nodes.<id>.<output>}}`.
    pub default_outputs: Vec<String>,
    pub fields: Vec<FieldSpec>,
    /// What the node can do, so the planner can pick nodes by need (see the capability resolver).
    pub capabilities: Vec<String>,
    /// Whether a run may wait here for a person.
    pub waits_for_person: bool,
    /// Whether anything can run this kind of step yet. A step that is in the editor but not runnable can be drawn and saved, and
    /// the checker says it cannot run; a run that contains one is refused before it starts.
    pub runnable: bool,
}

/// Does anything run this kind of step? Every kind has an engine now: the For each step is run by the engine itself and the Database step by the tool host's `db.query`
/// (card R7-12). The flag stays, so that a kind added to the editor before its engine can be drawn, saved and refused at run time.
fn can_run(_kind: NodeKind) -> bool {
    true
}

fn f(name: &str, label: &str, kind: FieldKind, required: bool) -> FieldSpec {
    FieldSpec { name: name.into(), label: label.into(), kind, required, choices: vec![], default: None, help: String::new() }
}

impl FieldSpec {
    fn choices(mut self, c: &[&str], default: &str) -> Self {
        self.kind = FieldKind::Choice;
        self.choices = c.iter().map(|s| s.to_string()).collect();
        self.default = Some(json!(default));
        self
    }
    fn help(mut self, h: &str) -> Self {
        self.help = h.into();
        self
    }
    fn default(mut self, v: Value) -> Self {
        self.default = Some(v);
        self
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[allow(clippy::too_many_arguments)]
fn spec_of(kind: NodeKind, label: &str, category: &str, description: &str, risk: Risk, out_ports: &[&str], outputs: &[&str], fields: Vec<FieldSpec>, capabilities: &[&str]) -> NodeSpec {
    NodeSpec {
        kind,
        label: label.into(),
        description: description.into(),
        category: category.into(),
        risk,
        out_ports: s(out_ports),
        default_outputs: s(outputs),
        fields,
        capabilities: s(capabilities),
        waits_for_person: matches!(kind, NodeKind::Approval | NodeKind::UserInput),
        runnable: can_run(kind),
    }
}

const PLATFORMS: &[&str] = &["linkedin", "instagram", "x", "facebook", "youtube", "tiktok"];
const ACTIONS: &[&str] = &["create_post", "upload_media", "publish", "schedule", "get_post", "read_comments", "reply_comment", "update_post", "delete_post"];

fn build() -> Vec<NodeSpec> {
    use FieldKind::*;
    use NodeKind::*;
    use Risk::*;
    vec![
        spec_of(
            Trigger,
            "Trigger",
            "Start",
            "How the flow starts: by hand, or on a time once a schedule is made for it. (A webhook, Aqua starting and an event are listed but nothing starts a flow from them yet.)",
            Low,
            &["out"],
            &["payload"],
            vec![
                f("trigger_type", "Starts", Choice, true).choices(&["manual", "schedule", "webhook", "startup", "event"], "manual"),
                f("cron", "Schedule (cron)", Text, false).help("For schedule triggers, e.g. 0 9 * * 1-5 for weekdays at 9:00."),
            ],
            &["start"],
        ),
        spec_of(
            UserInput,
            "Ask the user",
            "People",
            "Stops and asks a question. The flow waits for the answer.",
            Low,
            &["out"],
            &["answer"],
            vec![
                f("question", "Question", LongText, true),
                f("answer_kind", "Answer", Choice, false).choices(&["text", "yes_no", "choice", "number", "file"], "text"),
                f("options", "Options (comma separated)", Text, false),
            ],
            &["ask_user"],
        ),
        spec_of(
            Research,
            "Research",
            "AI",
            "Searches the web, reads sources and summarises what it found, with the sources.",
            Low,
            &["out"],
            &["summary", "sources"],
            vec![
                f("topic", "Topic", LongText, true),
                f("depth", "Depth", Choice, false).choices(&["quick", "standard", "deep"], "standard"),
                f("max_sources", "Most sources", Number, false).default(json!(5)),
            ],
            &["web_search", "summarize"],
        ),
        spec_of(
            TextGeneration,
            "Generate text",
            "AI",
            "Writes text with the language model, from a prompt that can use earlier results.",
            Low,
            &["out"],
            &["text"],
            vec![
                f("prompt", "Prompt", LongText, true),
                f("tone", "Tone", Choice, false).choices(&["professional", "friendly", "casual", "technical", "persuasive"], "professional"),
                f("max_words", "Most words", Number, false).default(json!(200)),
            ],
            &["write_text", "summarize"],
        ),
        spec_of(
            ImageGeneration,
            "Generate image",
            "AI",
            "Makes a real image file. \"diffusion\" draws the prompt with an image generator running on this computer; \"card\" draws a designed text graphic (headline, subtitle, colours). It says so when no generator is set up, and never makes a placeholder.",
            Low,
            &["out"],
            &["image", "alt_text"],
            vec![
                f("provider", "How to make it", Choice, false).choices(&["diffusion", "card", "local"], "diffusion").help("(local is the old name for diffusion.) diffusion needs a local Stable Diffusion service (AQUA_IMAGE_ENDPOINT). card needs nothing but draws text, not pictures."),
                f("prompt", "What the picture shows (diffusion)", LongText, false),
                f("title", "Headline (card)", Text, false),
                f("subtitle", "Subtitle (card)", Text, false),
                f("style", "Colours (card)", Choice, false).choices(&["ocean", "sunset", "forest", "slate", "violet"], "ocean"),
                f("size", "Size", Choice, false).choices(&["512x512", "1024x1024", "1080x1080", "1080x1350", "1200x627"], "1024x1024"),
                f("alt_text", "Description for people who cannot see it", Text, false).help("Left empty, one is written from the prompt or headline."),
            ],
            &["generate_image"],
        ),
        spec_of(
            AiAgent,
            "Ask the Aqua agent",
            "AI",
            "Gives a goal to the Aqua agent, which uses the browser and tools. For work that needs judgement.",
            Medium,
            &["out"],
            &["result", "steps"],
            vec![f("goal", "Goal", LongText, true), f("max_steps", "Most planning rounds", Number, false).default(json!(8)).help("From 1 to 12; a larger number counts as 12.")],
            &["agent", "web_search"],
        ),
        spec_of(
            Browser,
            "Browser action",
            "Automation",
            "One step in the browser: click, type, upload, read, wait. Finds elements by role, label and text, not by position.",
            Medium,
            &["out"],
            &["result"],
            vec![
                f("action", "Action", Choice, true).choices(&["click", "type", "upload", "read", "wait_for", "screenshot", "select"], "click"),
                f("target", "Element (role, label or text)", Text, false),
                f("value", "Value", LongText, false),
                f("session", "Browser session or account", Text, false).help("A saved browser profile or connected account; never a password."),
            ],
            &["browser"],
        ),
        spec_of(
            Website,
            "Open website",
            "Automation",
            "Opens a website in the browser and checks whether you are signed in.",
            Low,
            &["out"],
            &["page", "logged_in"],
            vec![
                f("url", "Website", Url, true),
                f("browser", "Browser", Choice, false).choices(&["managed", "chrome"], "managed"),
                f("session", "Browser session or account", Text, false),
                f("require_login", "Stop if not signed in", Bool, false).help("Stops with a clear message when the page shows a sign-in form. Aqua never asks for or stores the password."),
            ],
            &["browser", "open_website"],
        ),
        spec_of(
            N8n,
            "n8n workflow",
            "Integrations",
            "Calls a workflow you built in n8n (through its webhook) and hands its answer on. n8n is a separate program running on this computer; Aqua only sends what you set here. An n8n workflow can do anything, so this asks first, like publishing.",
            High,
            &["out"],
            &["result", "status"],
            vec![
                f("workflow", "Webhook path", Text, true).help("The path of the workflow's Webhook node, for example post-to-sheet. It must be active for the production address."),
                f("payload", "Data to send (JSON)", Json, false).help("Sent as the request body. No secrets here: name a credential on the step for the webhook's password."),
                f("method", "Method", Choice, false).choices(&["POST", "GET"], "POST"),
                f("mode", "Which address", Choice, false).choices(&["production", "test"], "production").help("production needs the workflow to be active; test is the address n8n shows while you are building."),
                f("auth_header", "Header for the credential", Text, false).help("The header your webhook checks (Header Auth). Default X-Aqua-Token."),
                f("timeout_secs", "Wait up to (seconds)", Number, false).default(json!(60)),
            ],
            &["n8n", "integration"],
        ),
        spec_of(
            CommentReplies,
            "Draft replies to comments",
            "AI",
            "Takes the comments a \"read comments\" step found, leaves out your own, ones you already answered and other people's conversations, and writes a draft reply to each of the rest with the local model. It posts nothing: a draft is text for you to approve, edit or skip. Spam is never answered, and a draft with a link or contact detail that is not in your post is dropped.",
            Low,
            &["out"],
            &["drafts", "count", "skipped", "in_inbox"],
            vec![
                f("comments", "Comments", Json, true).help("Usually {{nodes.<read comments step>.comments}}."),
                f("post_text", "What the post said", LongText, false).help("So replies fit it. Usually {{nodes.<post step>.text}} or typed here."),
                f("facts", "Facts the replies may state", LongText, false).help("What the model may say about you and your work, for example: We schedule posts with Aqua. Without facts it is told to claim nothing, but it can still make things up, so every question is marked to check."),
                f("post_url", "Post address", Text, false).help("Set it to keep the drafts in your comment inbox, where you approve and post them."),
                f("my_name", "Your own name", Text, false).help("Kept with the drafts so the reply can tell your comments from others'."),
                f("tone", "Tone", Text, false).help("For example friendly and professional. Default friendly and professional."),
                f("max_drafts", "Most drafts at a time", Number, false).help("Default 10, at most 50."),
            ],
            &["local_model"],
        ),
        spec_of(
            Http,
            "HTTP request",
            "Integrations",
            "Calls a web API.",
            Medium,
            &["out"],
            &["response", "status"],
            vec![
                f("method", "Method", Choice, true).choices(&["GET", "POST", "PUT", "PATCH", "DELETE"], "GET"),
                f("url", "URL", Url, true),
                f("headers", "Headers (JSON)", Json, false).help("No secrets here: name a credential on the node instead."),
                f("body", "Body (JSON)", Json, false),
            ],
            &["http"],
        ),
        spec_of(
            Tool,
            "Run a tool",
            "Automation",
            "Calls one tool of Aqua's guarded tool host (fs.read, web.get, git.status, ...).",
            Medium,
            &["out"],
            &["result"],
            vec![f("tool", "Tool", Text, true), f("arguments", "Arguments (JSON)", Json, false)],
            &["tool"],
        ),
        spec_of(
            File,
            "File",
            "Data",
            "Reads, writes or copies a file inside the allowed folders.",
            Medium,
            &["out"],
            &["file", "content"],
            vec![
                f("operation", "Operation", Choice, true).choices(&["read", "write", "copy", "list"], "read"),
                f("path", "Path", Text, true),
                f("content", "Content", LongText, false),
            ],
            &["files"],
        ),
        spec_of(
            Excel,
            "Spreadsheet",
            "Data",
            "Reads rows from, adds a row to, changes a row in, or writes rows out to an Excel (.xlsx) or CSV file inside Aqua's folders. Columns are found by their header name. Changing a file keeps a dated backup.",
            Medium,
            &["out"],
            &["rows", "row_count", "file"],
            vec![
                f("operation", "Operation", Choice, true).choices(&["read_rows", "append_row", "update_row", "export"], "append_row"),
                f("path", "File", Text, true).help("Inside a folder Aqua may use, for example posts.xlsx or @ws/reports/week.xlsx."),
                f("sheet", "Sheet", Text, false).help("Left empty: the first sheet."),
                f("columns", "Columns (JSON)", Json, false).help("For a new or changed row: column header to value, e.g. {\"Platform\": \"{{vars.platform}}\"}. A name the sheet does not have is an error."),
                f("key_column", "Find the row by this column", Text, false).help("update_row only."),
                f("key", "...where it holds", Text, false).help("update_row only. It must match exactly one row."),
                f("rows", "Rows (JSON list)", Json, false).help("export only: a list of objects."),
                f("limit", "Most rows to read", Number, false).help("read_rows only. Default 500, at most 5000."),
                f("rewrite_existing", "Allow rewriting a workbook Aqua did not make", Bool, false).help("Saving it again keeps values, formulas and dates but not formatting. Workbooks with charts, pictures, tables, merged cells or macros are never changed."),
            ],
            &["spreadsheet", "excel"],
        ),
        spec_of(
            Database,
            "Database",
            "Data",
            "Runs one read-only query (SELECT) on a database from data/tools/db.json, through the guarded tool host, and hands the rows on. It cannot change data: writes are not available in a flow yet. Put the values in \"Values\" and write ? in the query, so that text from a page or a step is only ever a value and never part of the query.",
            Medium,
            &["out"],
            &["rows", "count", "truncated"],
            vec![
                f("profile", "Database", Text, false).help("The name of a database in data/tools/db.json. Left empty, the only one that is set up."),
                f("query", "Query", LongText, true).help("One SELECT (or WITH, VALUES, EXPLAIN). Write ? where a value goes and list the values below. {{ }} is not filled in here."),
                f("params", "Values (JSON list)", Json, false).help("The values for the ? marks, in order: [\"{{vars.name}}\", 5]. Each is text, a number, true/false or empty."),
                f("max_rows", "Most rows", Number, false).help("Default is the database's own limit (200 unless set); at most 1000. A For each step over the rows takes at most 200."),
            ],
            &["database"],
        ),
        spec_of(
            SocialMedia,
            "Social platform",
            "Platforms",
            "Does one thing on a platform (create a post, upload media, publish, schedule, read, edit, delete). Uses the platform's connector when there is one, the browser otherwise.",
            High,
            &["out"],
            &["post_url", "status", "comments", "count", "reply_id"],
            vec![
                f("platform", "Platform", Choice, true).choices(PLATFORMS, "linkedin"),
                f("action", "Action", Choice, true).choices(ACTIONS, "publish"),
                f("text", "Text", LongText, false),
                f("media", "Media", Text, false).help("Usually {{nodes.<image node>.image}}."),
                f("hashtags", "Hashtags", Text, false),
                f("post_url", "Post address (to read one)", Text, false).help("For reading a post back, or its comments."),
                f("my_name", "Your own name (to recognise your comments)", Text, false).help("read_comments: your own comments are marked, and a comment you have already answered is marked as answered."),
                f("limit", "Most comments to read", Number, false).help("read_comments only. Default 50, at most 200."),
                f("comment_id", "Comment id", Text, false).help("reply_comment only: the id from a read_comments step."),
                f("reply_text", "Reply", LongText, false).help("reply_comment only. At most 1250 characters."),
                f("mode", "How", Choice, false).choices(&["auto", "connector", "browser"], "auto").help("auto: the connector when one is set up, the browser otherwise."),
            ],
            &["social_post"],
        ),
        spec_of(
            Condition,
            "If / else",
            "Logic",
            "Chooses a path. A value is true unless it is empty, false, 0 or no. Compare with == != < <= > >= contains; join with && || ! and ( ). If it cannot be judged (a value is missing, or is not a number where one is needed) the flow stops, or when the step is set to carry on after a failure it takes the \"false\" exit, never the \"true\" one.",
            Low,
            &["true", "false"],
            &["result"],
            vec![f("expression", "Condition", Text, true).help("For example {{nodes.open_site.logged_in}} or {{vars.platform}}.")],
            &["branch"],
        ),
        spec_of(
            Loop,
            "For each",
            "Logic",
            "Runs the steps connected to \"each\" once for every item of a list, one item after another, then continues by \"done\". Inside, {{nodes.<this step>.item}} is the item, .index counts from 0 and .number from 1. After it, {{nodes.<this step>.results}} holds what the steps inside made for each item. At most 200 items and 2000 step runs; a longer list stops it before it begins.",
            Low,
            &["each", "done"],
            &["item", "index", "number", "count", "results"],
            vec![f("items", "List", LongText, true).help("For example {{nodes.read_sheet.rows}}. Or a JSON list such as [\"a\", \"b\"], or text with one item on each line (one line is cut at its commas).")],
            &["loop"],
        ),
        spec_of(
            Delay,
            "Wait",
            "Logic",
            "Waits a number of seconds.",
            Low,
            &["out"],
            &[],
            vec![f("seconds", "Seconds", Number, true).default(json!(60))],
            &["delay"],
        ),
        spec_of(
            Schedule,
            "Schedule",
            "Logic",
            "Holds the flow until a date and time, then continues. Times are in the flow's time zone.",
            Low,
            &["out"],
            &["scheduled_for"],
            vec![
                f("when", "When", Text, false).help("For a one-time schedule: a date and time such as 2026-10-01 10:00."),
                f("timezone", "Time zone", Text, false),
                f("repeat", "Repeat", Choice, false).choices(&["once", "daily", "weekly", "monthly", "cron"], "once"),
                f("cron", "Cron", Text, false),
            ],
            &["schedule"],
        ),
        spec_of(
            Approval,
            "Ask for approval",
            "People",
            "Stops the flow and shows a preview. You can approve, edit or reject.",
            Low,
            &["approved", "rejected"],
            &["decision", "edits", "text"],
            vec![
                f("title", "Title", Text, true).default(json!("Ready to go ahead")),
                f("preview", "What to show", LongText, false).help("Text or a template, e.g. {{nodes.write.text}}."),
                f("allow_edit", "Allow edits", Bool, false).default(json!(true)),
            ],
            &["approval"],
        ),
        spec_of(
            Notification,
            "Notify",
            "People",
            "Tells you something: in the app, or by email or chat once those are connected.",
            Low,
            &["out"],
            &[],
            vec![f("channel", "Where", Choice, true).choices(&["in_app", "email", "slack"], "in_app"), f("message", "Message", LongText, true)],
            &["notify"],
        ),
        spec_of(
            SetVariable,
            "Set a value",
            "Logic",
            "Stores a value for later steps: {{vars.name}}.",
            Low,
            &["out"],
            &["value"],
            vec![f("name", "Name", Text, true), f("value", "Value", LongText, true)],
            &["variable"],
        ),
    ]
}

static REGISTRY: OnceLock<Vec<NodeSpec>> = OnceLock::new();

pub fn all() -> &'static [NodeSpec] {
    REGISTRY.get_or_init(build)
}

/// Does running this step have an effect outside Aqua that cannot be taken back, so a person must have
/// approved it? Publishing and sending always do; in the browser, clicking (Send, Post, Buy, Delete...) and
/// uploading do, while typing, choosing, reading and waiting do not.
pub fn high_impact(node: &crate::flows::model::Node) -> bool {
    match node.kind {
        NodeKind::Browser => matches!(node.config.get("action").and_then(|v| v.as_str()).unwrap_or("click"), "click" | "upload"),
        // Reading a post or its comments changes nothing, so it does not ask; writing does.
        NodeKind::SocialMedia => !matches!(node.config.get("action").and_then(|v| v.as_str()).unwrap_or("publish"), "get_post" | "read_comments"),
        k => spec(k).risk == Risk::High,
    }
}

/// Does this step hand on text that came from outside Aqua without going through the tool host (card OA8-04, SE-12)? A page the browser read, what another service answered, the
/// comments or the post read from a site. The tool, file, web and database steps go through the host, which keeps what the run has read; an agent or research step is a run of its own and
/// the live executor carries what it read. For the steps that are not those the engine tells the executors that the run has read outside text, and a step after it that changes
/// something asks.
pub fn reads_outside_text(node: &crate::flows::model::Node) -> bool {
    match node.kind {
        NodeKind::Browser | NodeKind::Website | NodeKind::N8n | NodeKind::CommentReplies => true,
        NodeKind::SocialMedia => matches!(node.config.get("action").and_then(|v| v.as_str()).unwrap_or("publish"), "get_post" | "read_comments"),
        _ => false,
    }
}

/// Must an interrupted or repeated run of this step be prevented from doing it twice? Everything high-impact, and a
/// spreadsheet step that writes (a row must not be added twice after a restart). This does not make it ask first.
pub fn ledgered(node: &crate::flows::model::Node) -> bool {
    high_impact(node) || (node.kind == NodeKind::Excel && !matches!(node.config.get("operation").and_then(|v| v.as_str()).unwrap_or("append_row"), "read_rows")) || changes_things_outside(node)
}

/// An HTTP request that is not a plain read, or a tool that is not read-only, may be sent twice after a timeout or a restart
/// unless the engine remembers it was started. They do not ask first (that is `high_impact`); they are only never repeated
/// blindly. A method or tool name that is filled in from another step cannot be judged ahead of time, so it counts as a write.
fn changes_things_outside(node: &crate::flows::model::Node) -> bool {
    let setting = |key: &str, default: &str| node.config.get(key).and_then(|v| v.as_str()).unwrap_or(default).trim().to_string();
    match node.kind {
        NodeKind::Http => !matches!(setting("method", "GET").to_ascii_uppercase().as_str(), "GET" | "HEAD" | "OPTIONS"),
        NodeKind::Tool => {
            let name = setting("tool", "");
            !tool_is_read_only(&name)
        }
        _ => false,
    }
}

/// Does the tool host say this tool only reads? Anything it does not know (or no host at all) is treated as not read-only.
pub(crate) fn tool_is_read_only(name: &str) -> bool {
    crate::agent_tools::host().map(|h| h.manifests().iter().any(|m| m.name == name && m.annotations.read_only)).unwrap_or(false)
}

pub fn spec(kind: NodeKind) -> &'static NodeSpec {
    all().iter().find(|s| s.kind == kind).expect("every node kind has a registry entry")
}

/// The node kinds that can do `capability` (for the planner's capability resolver).
pub fn kinds_with(capability: &str) -> Vec<NodeKind> {
    all().iter().filter(|s| s.capabilities.iter().any(|c| c == capability)).map(|s| s.kind).collect()
}

/// The platform actions a connector may offer (a platform exposes only the ones it supports).
pub fn platform_actions() -> &'static [&'static str] {
    ACTIONS
}

pub fn platforms() -> &'static [&'static str] {
    PLATFORMS
}
