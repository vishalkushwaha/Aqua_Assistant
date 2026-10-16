//! `aqua-cli` as a client of the core's HTTP API: start a coding run from a terminal or a script, follow it, end it, steer it, and get an exit code (card OA10-05; tasks UX-05 and WF-14 of
//! the OpenCode audit; decisions D-09 and D-07 taken as the audit proposes).
//!
//! ```text
//! aqua-cli run "<goal>" [--dir PATH] [--format text|json] [--detach] [--wait-approval SECS] [--max-steps N] [--profile coding|files|web|desktop]
//!                       [--preapproval ID] [--session NAME] [--idempotency-key K] [--core URL] [--data DIR] [--token-file FILE]
//! aqua-cli runs list | show ID | events ID [--after N] [--follow] | export ID
//! aqua-cli stop [ID]          aqua-cli steer ID "<text>" [--queue]          aqua-cli api <operationId> [--param k=v] [--data JSON] | --list
//! aqua-cli preapprove <folder> [--kinds tests,build,lint,typecheck] [--budget N] [--minutes N]      aqua-cli preapprovals [list | revoke ID]
//! aqua-cli tui ["<goal>"] [--dir PATH] [--session NAME] [--attach ID] [--ui] [--width N]            (card OA10-07: the terminal view, `view.rs`)
//! aqua-cli acp [--core URL] [--data-dir DIR] [--wait-approval SECS]                                 (card OA10-07: the editor protocol, `acp.rs`; it takes over standard input and output)
//! ```
//!
//! * **It is a client.** Everything goes over `127.0.0.1` with the API token (`http`): the runs are the core's, with its policy, its tool host and its questions. The token is read from
//!   `--token-file`, else `AQUA_API_TOKEN`, else `<data>/.aqua_api_token`; it is never printed and never sent to a host other than this computer's loopback.
//! * **It never approves.** A question of the core (a command to run, a file to change) is answered in the Aqua app, with the app's token; a run that needs one says so and ends with exit
//!   code 2 (`run`). The one thing the client does with the app's token is `preapprove`/`preapprovals`: for a person at a terminal, who types `yes`, to allow one run's own project checks in
//!   one folder (the audit's decision D-07, shipped off: the core must be started with `AQUA_PREAPPROVALS=on`).
//! * **`--format json`** prints the events as the core sent them, one JSON object per line (the `data:` of each server-sent frame; the heartbeats are left out), and nothing else on the
//!   output; what the client has to say is on the error stream.
//! * **Exit codes** are in [`run`]: 0 completed, 1 failed, 2 needs the person, 3 a limit, 4 cancelled, 5 the core is not there or too busy.
//! * **Ctrl+C**: the first press asks the core to end the run ("stopping...") and waits for it for ten seconds; the second leaves at once. A stream that breaks is taken up again from the
//!   last event the client has (`?after=`), so nothing is lost and nothing is printed twice.
//! * **Piped text** is added to the goal (`git diff | aqua-cli run "review this"`).
//!
//! The module uses nothing else of the core: the binary hands it the streams, the data folder and the document of the API.

pub mod acp;
pub mod cmds;
pub mod http;
pub mod render;
pub mod run;
pub mod view;

use http::{core_address, read_token, safe_id, Client, TokenKind};
use run::EXIT_FAILED;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

/// How the events are printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
}

/// What the client works with: where to write, what was piped in, whether the output is a terminal, how many times Ctrl+C was pressed, and the document of the API. The binary builds it;
/// a test builds one with in-memory streams.
pub struct Io<'a> {
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    /// What was piped to standard input (`None` when it is a terminal or empty).
    pub stdin_text: Option<String>,
    /// Is standard input a terminal (a person is typing)?
    pub stdin_is_terminal: bool,
    /// Colour is for a terminal.
    pub color: bool,
    /// How many times Ctrl+C was pressed; the binary raises it from its signal handler.
    pub interrupts: &'a AtomicUsize,
    /// The unit of the wait before the stream is taken up again (500 ms; a test makes it short).
    pub backoff_unit: Duration,
    /// `documentation/openapi.json`, for `api`.
    pub openapi: &'a str,
    /// Shows a text to the person and says whether they typed `yes` (false when nobody can: not a terminal).
    pub confirm: &'a dyn Fn(&str) -> bool,
}

/// The words of the commands this module serves.
pub const COMMANDS: [&str; 9] = ["run", "runs", "stop", "steer", "api", "preapprove", "preapprovals", "tui", "acp"];

/// The lines the help of `aqua-cli` shows for these commands.
pub const HELP: [&str; 9] = [
    "  aqua-cli run \"<goal>\" [--dir PATH] [--format text|json] [--detach] [--wait-approval SECS] [--max-steps N] [--preapproval ID] [--session NAME]   start a run on the running core and follow it; exit 0 completed, 1 failed, 2 needs you, 3 a limit, 4 cancelled, 5 no core",
    "  aqua-cli runs list | show ID | events ID [--follow] | export ID   the runs of this token that are going, one run's state, its events",
    "  aqua-cli stop [ID]   end a run (the only one that is going when no id is given)",
    "  aqua-cli steer ID \"<text>\" [--queue]   say something to a run that is going, for its next step (or for after it)",
    "  aqua-cli api <operationId> [--param k=v] [--data JSON] | --list   call any operation the API token may call, by its name in openapi.json",
    "  aqua-cli preapprove <folder> [--kinds tests,build,lint,typecheck] [--budget N] [--minutes N]   (at a terminal, with the app's token) let ONE run's project checks in one folder run unasked; needs AQUA_PREAPPROVALS=on on the core",
    "  aqua-cli preapprovals [list | revoke ID]   (at a terminal) what was allowed, and taking it back",
    "  aqua-cli tui [\"<goal>\"] [--dir PATH] [--session NAME] [--attach ID] [--ui] [--width N]   follow a run as a terminal view (steps, tool rows, the diff of a change that waits, questions, a status line; it never answers anything), or at a terminal ask for goals one after another",
    "  aqua-cli acp [--core URL] [--data-dir DIR] [--wait-approval SECS]   speak the Agent Client Protocol on standard input and output, for an editor that starts it (Zed, JetBrains, Neovim); approvals stay in the Aqua app",
];

struct Parsed {
    values: Vec<(String, String)>,
    flags: Vec<String>,
    rest: Vec<String>,
}

impl Parsed {
    fn value(&self, name: &str) -> Option<&str> {
        self.values.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }
}

/// Splits the words after the command into the options it takes, its flags and the rest. A word after a lone `--` is always part of the rest.
fn parse(args: &[String], with_value: &[&str], flags: &[&str]) -> Result<Parsed, String> {
    let mut p = Parsed { values: Vec::new(), flags: Vec::new(), rest: Vec::new() };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            p.rest.extend(it.by_ref().cloned());
            break;
        }
        match a.strip_prefix("--") {
            Some(name) if with_value.contains(&name) => {
                let v = it.next().ok_or_else(|| format!("--{name} needs a value"))?;
                p.values.push((name.to_string(), v.clone()));
            }
            Some(name) if flags.contains(&name) => p.flags.push(name.to_string()),
            Some(name) => return Err(format!("{}: there is no such option (words that look like one go after --)", render::plain(&format!("--{name}")))),
            None => p.rest.push(a.clone()),
        }
    }
    Ok(p)
}

fn say(w: &mut dyn Write, text: &str) {
    let _ = writeln!(w, "{text}");
    let _ = w.flush();
}

fn format_of(p: &Parsed) -> Result<Format, String> {
    match p.value("format") {
        None | Some("text") => Ok(Format::Text),
        Some("json") => Ok(Format::Json),
        Some(other) => Err(format!("--format is text or json, not {}", render::plain(other))),
    }
}

/// The client for the command: the address of the core, the data folder (`--data DIR`, or `--data-dir DIR`, which is the only way for `api`, where `--data` is the body) and the token.
fn client_for(p: &Parsed, default_data: &Path, kind: TokenKind, data_is_the_folder: bool) -> Result<Client, String> {
    let data = p.value("data-dir").or(if data_is_the_folder { p.value("data") } else { None }).map(PathBuf::from).unwrap_or_else(|| default_data.to_path_buf());
    let addr = core_address(p.value("core"))?;
    let token_file = p.value("token-file").map(PathBuf::from);
    if kind == TokenKind::Ui && token_file.is_some() {
        return Err("the app's token is read from the data folder only".into());
    }
    Ok(Client::new(addr, read_token(kind, token_file.as_deref(), &data)?))
}

const COMMON: [&str; 5] = ["core", "data-dir", "token-file", "format", "data"];

/// Runs one command of this module (`args[0]` is its word) and gives the exit code of the process.
pub fn run_cli(args: &[String], default_data: &Path, io: &mut Io) -> i32 {
    let Some(command) = args.first().map(String::as_str) else { return EXIT_FAILED };
    let rest = &args[1..];
    let with = |extra: &[&'static str]| -> Vec<&'static str> { COMMON.iter().copied().chain(extra.iter().copied()).collect() };
    match command {
        "run" => {
            let p = match parse(rest, &with(&["dir", "wait-approval", "max-steps", "profile", "preapproval", "idempotency-key", "session"]), &["detach", "no-color"]) {
                Ok(p) => p,
                Err(e) => return fail(io, &format!("run: {e}")),
            };
            let format = match format_of(&p) {
                Ok(f) => f,
                Err(e) => return fail(io, &format!("run: {e}")),
            };
            let mut goal = p.rest.join(" ").trim().to_string();
            if let Some(piped) = io.stdin_text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                goal = if goal.is_empty() { piped.to_string() } else { format!("{goal}\n\n{piped}") };
            }
            if goal.is_empty() {
                return fail(io, "usage: aqua-cli run \"<goal>\" [--dir PATH] [--format text|json] [--detach] [--wait-approval SECS] [--max-steps N] [--preapproval ID]");
            }
            let number = |name: &str| -> Result<Option<u64>, String> { p.value(name).map(|v| v.trim().parse::<u64>().map_err(|_| format!("--{name} is a whole number"))).transpose() };
            let (wait, steps) = match (number("wait-approval"), number("max-steps")) {
                (Ok(w), Ok(s)) => (w.unwrap_or(0), s),
                (Err(e), _) | (_, Err(e)) => return fail(io, &format!("run: {e}")),
            };
            let preapproval = p.value("preapproval").map(str::to_string);
            if preapproval.as_deref().is_some_and(|i| !safe_id(i)) {
                return fail(io, "run: --preapproval is the id that `aqua-cli preapprove` printed");
            }
            let session_id = p.value("session").map(str::to_string);
            if session_id.as_deref().is_some_and(|n| n.is_empty() || n.len() > 64 || !n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))) {
                return fail(io, "run: --session is a name of 1 to 64 letters, digits and _ . : -");
            }
            let directory = p.value("dir").map(|d| whole_path(d));
            if preapproval.is_some() && directory.is_none() {
                return fail(io, "run: a pre-approval is for one folder: say which with --dir");
            }
            let client = match client_for(&p, default_data, TokenKind::Api, true) {
                Ok(c) => c,
                Err(e) => return fail(io, &format!("run: {e}")),
            };
            if p.flag("no-color") {
                io.color = false;
            }
            let args = run::StartArgs {
                goal,
                directory,
                max_steps: steps.map(|s| s.min(u32::MAX as u64) as u32),
                profile: p.value("profile").map(str::to_string),
                wait_approval: wait,
                preapproval,
                idempotency_key: p.value("idempotency-key").map(str::to_string),
                session_id,
            };
            run::run(&client, &args, format, p.flag("detach"), io)
        }
        "runs" => {
            let p = match parse(rest, &with(&["after"]), &["follow"]) {
                Ok(p) => p,
                Err(e) => return fail(io, &format!("runs: {e}")),
            };
            let format = match format_of(&p) {
                Ok(f) => f,
                Err(e) => return fail(io, &format!("runs: {e}")),
            };
            let client = match client_for(&p, default_data, TokenKind::Api, true) {
                Ok(c) => c,
                Err(e) => return fail(io, &format!("runs: {e}")),
            };
            let after = match p.value("after").map(|a| a.trim().parse::<i64>()).transpose() {
                Ok(a) => a.unwrap_or(0),
                Err(_) => return fail(io, "runs: --after is the number of the last event you have"),
            };
            match p.rest.first().map(String::as_str) {
                Some("list") | None => cmds::runs_list(&client, format, io),
                Some("show") => cmds::runs_show(&client, p.rest.get(1), format, io),
                Some("events") => cmds::runs_events(&client, p.rest.get(1), after, p.flag("follow"), format, io),
                Some("export") => cmds::runs_events(&client, p.rest.get(1), 0, false, Format::Json, io),
                Some(other) => fail(io, &format!("runs: {} is not list, show, events or export", render::plain(other))),
            }
        }
        "stop" => {
            let p = match parse(rest, &with(&[]), &[]) {
                Ok(p) => p,
                Err(e) => return fail(io, &format!("stop: {e}")),
            };
            let format = match format_of(&p) {
                Ok(f) => f,
                Err(e) => return fail(io, &format!("stop: {e}")),
            };
            match client_for(&p, default_data, TokenKind::Api, true) {
                Ok(c) => cmds::stop(&c, p.rest.first(), format, io),
                Err(e) => fail(io, &format!("stop: {e}")),
            }
        }
        "steer" => {
            let p = match parse(rest, &with(&[]), &["queue"]) {
                Ok(p) => p,
                Err(e) => return fail(io, &format!("steer: {e}")),
            };
            let format = match format_of(&p) {
                Ok(f) => f,
                Err(e) => return fail(io, &format!("steer: {e}")),
            };
            let text = p.rest.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
            match client_for(&p, default_data, TokenKind::Api, true) {
                Ok(c) => cmds::steer(&c, p.rest.first(), &text, p.flag("queue"), format, io),
                Err(e) => fail(io, &format!("steer: {e}")),
            }
        }
        "api" => {
            let p = match parse(rest, &with(&["param"]), &["list"]) {
                Ok(p) => p,
                Err(e) => return fail(io, &format!("api: {e}")),
            };
            // `--data` names the data folder in every command but this one: here it is the body of the request (`--data-dir` names the folder).
            let mut params: Vec<(String, String)> = Vec::new();
            for kv in p.values.iter().filter(|(k, _)| k == "param") {
                match kv.1.split_once('=') {
                    Some((k, v)) if !k.is_empty() => params.push((k.to_string(), v.to_string())),
                    _ => return fail(io, "api: --param is NAME=VALUE"),
                }
            }
            match client_for(&p, default_data, TokenKind::Api, false) {
                Ok(c) => cmds::api(&c, p.rest.first(), &params, p.value("data"), p.flag("list"), io),
                // `--list` needs no core and no token.
                Err(_) if p.flag("list") || p.rest.is_empty() => cmds::api(&Client::new(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), String::new()), None, &[], None, true, io),
                Err(e) => fail(io, &format!("api: {e}")),
            }
        }
        "preapprove" | "preapprovals" => {
            let p = match parse(rest, &with(&["kinds", "budget", "minutes"]), &[]) {
                Ok(p) => p,
                Err(e) => return fail(io, &format!("{command}: {e}")),
            };
            let format = match format_of(&p) {
                Ok(f) => f,
                Err(e) => return fail(io, &format!("{command}: {e}")),
            };
            if !io.stdin_is_terminal {
                return fail(io, &format!("{command}: it uses the app's own token and needs a person at a terminal; nothing was done"));
            }
            let client = match client_for(&p, default_data, TokenKind::Ui, true) {
                Ok(c) => c,
                Err(e) => return fail(io, &format!("{command}: {e}")),
            };
            if command == "preapprovals" {
                return match p.rest.first().map(String::as_str) {
                    Some("revoke") => cmds::preapprovals(&client, cmds::PreAction::Revoke(p.rest.get(1)), format, io),
                    Some("list") | None => cmds::preapprovals(&client, cmds::PreAction::List, format, io),
                    Some(other) => fail(io, &format!("preapprovals: {} is not list or revoke", render::plain(other))),
                };
            }
            let Some(folder) = p.rest.first() else { return fail(io, "usage: aqua-cli preapprove <folder> [--kinds tests,build,lint,typecheck] [--budget N] [--minutes N]") };
            let kinds: Vec<String> = p.value("kinds").unwrap_or("tests").split(',').map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect();
            if kinds.is_empty() || kinds.iter().any(|k| !matches!(k.as_str(), "tests" | "build" | "lint" | "typecheck")) {
                return fail(io, "preapprove: --kinds is some of tests, build, lint, typecheck (comma between)");
            }
            let num = |name: &str| -> Result<Option<u32>, String> { p.value(name).map(|v| v.trim().parse::<u32>().map_err(|_| format!("--{name} is a whole number"))).transpose() };
            let (budget, minutes) = match (num("budget"), num("minutes")) {
                (Ok(b), Ok(m)) => (b, m),
                (Err(e), _) | (_, Err(e)) => return fail(io, &format!("preapprove: {e}")),
            };
            cmds::preapprove(&client, &cmds::PreapproveArgs { folder: whole_path(folder), kinds, budget, minutes }, format, io)
        }
        "tui" => view::run(args, default_data, io, None),
        // The editor protocol takes over standard input and output: the program starts it itself (`acp::run`), not through the streams of `Io`.
        "acp" => fail(io, "acp: it speaks on standard input and output and is started by an editor: aqua-cli acp"),
        _ => EXIT_FAILED,
    }
}

fn fail(io: &mut Io, message: &str) -> i32 {
    say(io.err, &render::plain(message));
    // A request that was not made at all is the caller's mistake (1); the core is not even asked.
    EXIT_FAILED
}

/// A folder as the core can use it: whole, as the system writes it (a relative one is made whole here: the core does not know where this was typed).
fn whole_path(path: &str) -> String {
    std::fs::canonicalize(path).map(|d| d.to_string_lossy().trim_start_matches(r"\\?\").to_string()).unwrap_or_else(|_| path.to_string())
}

#[cfg(test)]
mod tests;
