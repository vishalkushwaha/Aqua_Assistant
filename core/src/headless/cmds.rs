//! The smaller commands of the client: `runs`, `stop`, `steer`, `api`, and the two that use the app's own token to make and take back a pre-approval (card OA10-05; tasks UX-05 and
//! WF-14 of the OpenCode audit).
//!
//! With the API token a client reaches the runs it started and no others (`documentation/RUN_EVENTS.md`): `runs list` is the runs of this token that are not over, `runs show` is the
//! state of one, `runs events` and `runs export` are its event log in the form the API token may read (what a call was given and what it returned are the app's).

use super::http::{safe_id, Client};
use super::render::{plain, Renderer};
use super::run::{follow, http_failure, net_failure, EXIT_COMPLETED, EXIT_FAILED};
use super::{Format, Io};
use serde_json::{json, Value};
use std::io::Write;

fn say(w: &mut dyn Write, text: &str) {
    let _ = writeln!(w, "{text}");
    let _ = w.flush();
}

/// Reads one answer that has to be 2xx; says what went wrong and gives the exit code when it is not.
fn expect(client: &Client, method: &str, path: &str, body: Option<&Value>, what: &str, io: &mut Io) -> Result<Value, i32> {
    match client.send(method, path, body) {
        Err(e) => Err(net_failure(io, &e)),
        Ok(r) if (200..300).contains(&r.status) => Ok(r.json()),
        Ok(r) => {
            let (msg, code) = r.error();
            Err(http_failure(io, what, r.status, &msg, &code))
        }
    }
}

fn need_id<'a>(id: Option<&'a String>, usage: &str, io: &mut Io) -> Result<&'a str, i32> {
    match id {
        Some(i) if safe_id(i) => Ok(i.as_str()),
        Some(_) => {
            say(io.err, "that is not an id (letters, digits, _ - . only)");
            Err(EXIT_FAILED)
        }
        None => {
            say(io.err, usage);
            Err(EXIT_FAILED)
        }
    }
}

/// `aqua-cli stop [ID]`: ends a run. With no id, the one run of this token that is going.
pub fn stop(client: &Client, id: Option<&String>, format: Format, io: &mut Io) -> i32 {
    let id = match id {
        Some(_) => match need_id(id, "", io) {
            Ok(i) => i.to_string(),
            Err(c) => return c,
        },
        None => {
            let active = match expect(client, "GET", "/api/v1/runs/active", None, "the runs could not be listed", io) {
                Ok(v) => v,
                Err(c) => return c,
            };
            let runs = active["runs"].as_array().cloned().unwrap_or_default();
            match runs.as_slice() {
                [one] => one["run_id"].as_str().unwrap_or("").to_string(),
                [] => {
                    say(io.err, "no run of this token is going: nothing to stop");
                    return EXIT_FAILED;
                }
                many => {
                    say(io.err, &format!("{} runs are going: say which one (aqua-cli stop <id>; aqua-cli runs list)", many.len()));
                    return EXIT_FAILED;
                }
            }
        }
    };
    if !safe_id(&id) {
        say(io.err, "the core named a run in a way this client will not use");
        return EXIT_FAILED;
    }
    let v = match expect(client, "POST", &format!("/api/v1/runs/{id}/interrupt"), None, "the run was not stopped", io) {
        Ok(v) => v,
        Err(c) => return c,
    };
    match format {
        Format::Json => say(io.out, &v.to_string()),
        Format::Text => say(io.out, &format!("{id}: {}", if v["ended"] == true { "stopped" } else { "told to stop; it ends at its next step" })),
    }
    EXIT_COMPLETED
}

/// `aqua-cli steer ID "<text>" [--queue]`: says something to a run that is going: added to the task at its next step, or (`--queue`) kept for after it.
pub fn steer(client: &Client, id: Option<&String>, text: &str, queue: bool, format: Format, io: &mut Io) -> i32 {
    let id = match need_id(id, "usage: aqua-cli steer <run id> \"<text>\" [--queue]", io) {
        Ok(i) => i.to_string(),
        Err(c) => return c,
    };
    if text.trim().is_empty() {
        say(io.err, "say what to tell the run: aqua-cli steer <run id> \"<text>\"");
        return EXIT_FAILED;
    }
    let body = json!({ "text": text, "delivery": if queue { "queue" } else { "steer" } });
    let v = match expect(client, "POST", &format!("/api/v1/runs/{id}/messages"), Some(&body), "the run was not told", io) {
        Ok(v) => v,
        Err(c) => return c,
    };
    match format {
        Format::Json => say(io.out, &v.to_string()),
        Format::Text => say(io.out, &format!("{id}: {}", plain(v["note"].as_str().unwrap_or(if v["stopped"] == true { "stopped" } else { "told" })))),
    }
    EXIT_COMPLETED
}

/// `aqua-cli runs list`: the runs of this token that are not over.
pub fn runs_list(client: &Client, format: Format, io: &mut Io) -> i32 {
    let v = match expect(client, "GET", "/api/v1/runs/active", None, "the runs could not be listed", io) {
        Ok(v) => v,
        Err(c) => return c,
    };
    if format == Format::Json {
        say(io.out, &v.to_string());
        return EXIT_COMPLETED;
    }
    let runs = v["runs"].as_array().cloned().unwrap_or_default();
    if runs.is_empty() {
        say(io.out, "no run of this token is going (finished runs are in the Aqua app: Runs and proof)");
    }
    for r in runs {
        let place = r["position"].as_u64().filter(|p| *p > 0).map(|p| format!(" place {p}")).unwrap_or_default();
        say(io.out, &format!("{}  {}{}  {}", plain(r["run_id"].as_str().unwrap_or("")), plain(r["state"].as_str().unwrap_or("")), place, plain(r["goal"].as_str().unwrap_or(""))));
    }
    EXIT_COMPLETED
}

/// `aqua-cli runs show ID`: the state of one run, now.
pub fn runs_show(client: &Client, id: Option<&String>, format: Format, io: &mut Io) -> i32 {
    let id = match need_id(id, "usage: aqua-cli runs show <run id>", io) {
        Ok(i) => i.to_string(),
        Err(c) => return c,
    };
    let v = match expect(client, "GET", &format!("/api/v1/runs/{id}/wait?timeout=0"), None, "the run could not be read", io) {
        Ok(v) => v,
        Err(c) => return c,
    };
    if format == Format::Json {
        say(io.out, &v.to_string());
        return EXIT_COMPLETED;
    }
    say(io.out, &format!("{id}: {}{}", plain(v["state"].as_str().unwrap_or("")), if v["finished"] == true { " (over)" } else { "" }));
    for (label, key) in [("status", "status"), ("stopped by", "stopped_by"), ("needs", "needs"), ("answer", "answer")] {
        if let Some(s) = v[key].as_str().filter(|s| !s.is_empty()) {
            say(io.out, &format!("  {label}: {}", plain(s)));
        }
    }
    EXIT_COMPLETED
}

/// `aqua-cli runs events ID [--after N] [--follow]` and `runs export ID`: the events of a run. Without `--follow`, the pages are read until the log ends; with it, the stream is followed to
/// the end of the run (and the exit code is the run's, as for `run`).
pub fn runs_events(client: &Client, id: Option<&String>, after: i64, follow_on: bool, format: Format, io: &mut Io) -> i32 {
    let id = match need_id(id, "usage: aqua-cli runs events <run id> [--after N] [--follow] [--format json]", io) {
        Ok(i) => i.to_string(),
        Err(c) => return c,
    };
    if follow_on {
        return follow(client, &id, after, format, io);
    }
    let mut renderer = Renderer::new(io.color && format == Format::Text);
    let mut cursor = after.max(0);
    // A page is at most 500 events; a log of any length is read to its end, but never for ever (a runaway answer would be a bug of the core).
    for _ in 0..10_000 {
        let page = match expect(client, "GET", &format!("/api/v1/runs/{id}/events?after={cursor}&limit=500"), None, "the events could not be read", io) {
            Ok(v) => v,
            Err(c) => return c,
        };
        let events = page["events"].as_array().cloned().unwrap_or_default();
        for e in &events {
            match format {
                Format::Json => say(io.out, &e.to_string()),
                Format::Text => {
                    for line in renderer.lines(e) {
                        say(io.out, &line);
                    }
                }
            }
        }
        let last = page["last"].as_i64().unwrap_or(cursor);
        if events.is_empty() || last <= cursor || page["finished"] == true {
            return EXIT_COMPLETED;
        }
        cursor = last;
    }
    EXIT_COMPLETED
}

/// The operations of the API as the document of `documentation/openapi.json` lists them: (operationId, method, path, token, streams).
fn operations(spec: &Value) -> Vec<(String, String, String, String, bool)> {
    let mut out = Vec::new();
    let Some(paths) = spec["paths"].as_object() else { return out };
    for (path, methods) in paths {
        let Some(methods) = methods.as_object() else { continue };
        for (method, op) in methods {
            let Some(id) = op["operationId"].as_str() else { continue };
            let streams = op["responses"].as_object().is_some_and(|r| r.values().any(|v| v["content"].as_object().is_some_and(|c| c.contains_key("text/event-stream"))));
            out.push((id.to_string(), method.to_uppercase(), path.clone(), op["x-aqua-token"].as_str().unwrap_or("api").to_string(), streams));
        }
    }
    out.sort();
    out
}

pub(super) fn encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The path and query of operation `op` with the `--param k=v` pairs put in: the ones the path names replace `{k}`, the others are the query. `Err` is a sentence for the person.
pub fn operation_path(path: &str, params: &[(String, String)]) -> Result<String, String> {
    let mut out = String::new();
    let mut used: Vec<&str> = Vec::new();
    let mut rest = path;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let close = rest[open..].find('}').map(|c| open + c).ok_or("the path of the operation is not well formed")?;
        let name = &rest[open + 1..close];
        let value = params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).ok_or_else(|| format!("the operation needs --param {name}=VALUE"))?;
        if value.is_empty() || value.len() > 256 || value.contains(['/', '\\']) || value == "." || value == ".." {
            return Err(format!("--param {name}: a value that is one name (no slash)"));
        }
        out.push_str(&encode(value));
        used.push(name);
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    let query: Vec<String> = params.iter().filter(|(k, _)| !used.contains(&k.as_str())).map(|(k, v)| format!("{}={}", encode(k), encode(v))).collect();
    if !query.is_empty() {
        out.push('?');
        out.push_str(&query.join("&"));
    }
    Ok(out)
}

/// `aqua-cli api <operationId> [--param k=v] [--data JSON]`, and `aqua-cli api --list`: any operation of the API that the API token may call, by the name `openapi.json` gives it.
/// An operation that needs the UI token is refused here, and one that is a stream is pointed to the command that follows it. The answer's body goes to the output as it came.
pub fn api(client: &Client, operation: Option<&String>, params: &[(String, String)], data: Option<&str>, list: bool, io: &mut Io) -> i32 {
    let spec: Value = serde_json::from_str(io.openapi).unwrap_or(Value::Null);
    let ops = operations(&spec);
    if list || operation.is_none() {
        for (id, method, path, token, streams) in &ops {
            say(io.out, &format!("{id:<44} {method:<6} {path}{}{}", if token == "ui" { "   (needs the UI token: not for this command)" } else { "" }, if *streams { "   (a stream: see runs events --follow)" } else { "" }));
        }
        return EXIT_COMPLETED;
    }
    let name = operation.map(String::as_str).unwrap_or("");
    let Some((_, method, path, token, streams)) = ops.iter().find(|(id, ..)| id == name) else {
        say(io.err, &format!("there is no operation called {}: `aqua-cli api --list` lists them", plain(name)));
        return EXIT_FAILED;
    };
    if token == "ui" {
        say(io.err, "this operation needs the UI token, which is the app's: this command never uses it");
        return EXIT_FAILED;
    }
    if *streams {
        say(io.err, "this operation is a stream: follow a run with `aqua-cli runs events <id> --follow`");
        return EXIT_FAILED;
    }
    let path = match operation_path(path, params) {
        Ok(p) => p,
        Err(e) => {
            say(io.err, &e);
            return EXIT_FAILED;
        }
    };
    let body: Option<Value> = match data {
        Some(d) if d.len() > 1024 * 1024 => {
            say(io.err, "--data is larger than 1 MiB");
            return EXIT_FAILED;
        }
        Some(d) => match serde_json::from_str(d) {
            Ok(v) => Some(v),
            Err(e) => {
                say(io.err, &format!("--data is not JSON: {e}"));
                return EXIT_FAILED;
            }
        },
        None => None,
    };
    if body.is_some() && !matches!(method.as_str(), "POST" | "PUT" | "PATCH") {
        say(io.err, "--data goes with POST, PUT and PATCH only");
        return EXIT_FAILED;
    }
    match client.send(method, &path, body.as_ref()) {
        Err(e) => net_failure(io, &e),
        Ok(r) => {
            let text = String::from_utf8_lossy(&r.body).to_string();
            if (200..300).contains(&r.status) {
                if !text.trim().is_empty() {
                    say(io.out, text.trim_end());
                }
                EXIT_COMPLETED
            } else {
                // The body of an error is the core's sentence and code: it goes to the error stream with the status.
                say(io.err, &format!("{} {path}: {}", r.status, plain(text.trim_end())));
                http_failure_code(r.status)
            }
        }
    }
}

fn http_failure_code(status: u16) -> i32 {
    if matches!(status, 429 | 503) {
        super::run::EXIT_UNREACHABLE
    } else {
        EXIT_FAILED
    }
}

/// What `preapprove` asks the core: one folder, some kinds of checks, a number of calls and a time.
pub struct PreapproveArgs {
    pub folder: String,
    pub kinds: Vec<String>,
    pub budget: Option<u32>,
    pub minutes: Option<u32>,
}

/// `aqua-cli preapprove <folder> [--kinds tests,build,lint,typecheck] [--budget N] [--minutes N]` (card OA10-05, task WF-14, decision D-07): the person allows the project's own checks
/// (the tests, the build, the linter, the type check: only the command lines the terminal tools already count as a project's checks) in one folder, for one run, up to a number of calls and
/// for a time, so that an unattended run does not stop at each of them. It uses the app's own token for this one request, and only for a person at a terminal who types `yes`; it
/// cannot approve anything that is asked, and it makes nothing the API token can make. The answer carries the id to give to `aqua-cli run --preapproval`.
pub fn preapprove(client: &Client, args: &PreapproveArgs, format: Format, io: &mut Io) -> i32 {
    let mut body = json!({ "folder": args.folder, "kinds": args.kinds });
    if let Some(b) = args.budget {
        body["budget"] = json!(b);
    }
    if let Some(m) = args.minutes {
        body["minutes"] = json!(m);
    }
    let shown = format!(
        "This lets ONE run of the agent run the project's own checks ({}) in {} without asking you each time: at most {} calls, for {} minutes. It still asks about every other command, every edit and everything on the network. It cannot be used by another run.\nIt is made with the app's own token (the UI token): you are the person at this terminal.\n",
        args.kinds.join(", "),
        plain(&args.folder),
        args.budget.map_or("the default number of".to_string(), |b| b.to_string()),
        args.minutes.map_or("the default number of".to_string(), |m| m.to_string())
    );
    if !(io.confirm)(&shown) {
        say(io.err, "nothing was made");
        return EXIT_FAILED;
    }
    let v = match expect(client, "POST", "/api/v1/preapprovals", Some(&body), "the pre-approval was not made", io) {
        Ok(v) => v,
        Err(c) => return c,
    };
    match format {
        Format::Json => say(io.out, &v.to_string()),
        Format::Text => {
            let id = plain(v["id"].as_str().unwrap_or(""));
            say(io.out, &format!("pre-approval {id}: {} in {}, {} calls, {} minutes", plain(&v["kinds"].to_string()), plain(v["folder"].as_str().unwrap_or("")), v["budget"], v["minutes"]));
            if v["trusted"] == false {
                say(io.out, "note: that folder is not a trusted one (the workspace, or the project under full access): checks there still ask");
            }
            say(io.out, &format!("use it once: aqua-cli run \"<goal>\" --dir \"{}\" --preapproval {id}", plain(v["folder"].as_str().unwrap_or(""))));
        }
    }
    EXIT_COMPLETED
}

/// What `preapprovals` does.
pub enum PreAction<'a> {
    List,
    Revoke(Option<&'a String>),
}

/// `aqua-cli preapprovals [list]` and `preapprovals revoke ID`: what has been allowed, and taking it back (the next call of the run asks again). The app's token, for a person at a terminal.
pub fn preapprovals(client: &Client, action: PreAction, format: Format, io: &mut Io) -> i32 {
    if let PreAction::Revoke(id) = action {
        let id = match need_id(id, "usage: aqua-cli preapprovals revoke <id>", io) {
            Ok(i) => i.to_string(),
            Err(c) => return c,
        };
        return match expect(client, "DELETE", &format!("/api/v1/preapprovals/{id}"), None, "the pre-approval was not taken back", io) {
            Ok(v) => {
                match format {
                    Format::Json => say(io.out, &v.to_string()),
                    Format::Text => say(io.out, &format!("{id}: taken back; the next check of that run asks again")),
                }
                EXIT_COMPLETED
            }
            Err(c) => c,
        };
    }
    let v = match expect(client, "GET", "/api/v1/preapprovals", None, "the pre-approvals could not be listed", io) {
        Ok(v) => v,
        Err(c) => return c,
    };
    if format == Format::Json {
        say(io.out, &v.to_string());
        return EXIT_COMPLETED;
    }
    let list = v["preapprovals"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        say(io.out, "no pre-approval is open");
    }
    for p in list {
        say(io.out, &format!("{}  {}  {}  {}/{} calls used  {} s left  run: {}", plain(p["id"].as_str().unwrap_or("")), plain(p["state"].as_str().unwrap_or("")), plain(p["folder"].as_str().unwrap_or("")), p["used"], p["budget"], p["expires_in_secs"], plain(p["run_id"].as_str().unwrap_or("none yet"))));
    }
    EXIT_COMPLETED
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_of_an_operation_takes_its_names_from_the_params_and_the_rest_is_the_query() {
        let p = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert_eq!(operation_path("/api/v1/runs/{run_id}/events", &[p("run_id", "run_1_2"), p("after", "5")]).unwrap(), "/api/v1/runs/run_1_2/events?after=5");
        assert_eq!(operation_path("/api/v1/runs/active", &[]).unwrap(), "/api/v1/runs/active");
        assert_eq!(operation_path("/api/v1/x", &[p("q", "a b&c")]).unwrap(), "/api/v1/x?q=a%20b%26c");
        assert!(operation_path("/api/v1/runs/{run_id}", &[]).unwrap_err().contains("--param run_id"));
        for bad in ["a/b", "..", ".", "", "a\\b"] {
            assert!(operation_path("/api/v1/runs/{run_id}", &[p("run_id", bad)]).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_operations_are_read_from_the_document_with_the_token_each_needs() {
        let spec = json!({ "paths": {
            "/api/v1/runs/active": { "get": { "operationId": "getRunsActive", "x-aqua-token": "api", "responses": { "200": {} } } },
            "/api/v1/runs/{run_id}/stream": { "get": { "operationId": "getRunsRunIdStream", "x-aqua-token": "api", "responses": { "200": { "content": { "text/event-stream": {} } } } } },
            "/api/v1/vault": { "get": { "operationId": "getVault", "x-aqua-token": "ui", "responses": { "200": {} } } }
        } });
        let ops = operations(&spec);
        assert_eq!(ops.len(), 3);
        assert!(ops.iter().any(|o| o.0 == "getRunsRunIdStream" && o.4));
        assert!(ops.iter().any(|o| o.0 == "getVault" && o.3 == "ui"));
    }

    #[test]
    fn the_real_document_names_the_operations_the_client_depends_on() {
        let spec: Value = serde_json::from_str(include_str!("../../../documentation/openapi.json")).unwrap();
        let ids: Vec<String> = operations(&spec).into_iter().map(|o| o.0).collect();
        for needed in ["postRuns", "getRunsRunIdStream", "getRunsRunIdEvents", "getRunsRunIdWait", "postRunsRunIdInterrupt", "postRunsRunIdMessages", "getRunsActive"] {
            assert!(ids.iter().any(|i| i == needed), "{needed}");
        }
    }
}
