//! The schemas of the API's bodies, and which route has which (card OA10-02, tasks UX-04 and EX-18 of the OpenCode audit).
//!
//! `documentation/openapi.json` is generated from the router's own source by `openapi.rs`; this file is what that generator reads to say what a body looks like. The audit asked for
//! the schemas to be *derived* from the Rust types with `schemars`. That crate is not in the build and is not added without the owner's say (decision D-06), so the schemas are written
//! here by hand, and **each one is held to the code by a test**, not by trust:
//!
//! * a request that has a Rust type (`PipelineTextRequest`, `UnifiedMcpAgentRequest`, `StartRequest`, ...) is built from its schema (what a client would send) and must be read by that
//!   type, so a field that is renamed, or becomes required, fails the test;
//! * an answer that has a Rust type that can be serialised (`Event`, `McpAgentResult`, `ToolResult`, `StatusResponse` ...) is serialised and must conform to its schema, **and** every
//!   field it has must be in the schema (a field that is added and not described fails the test);
//! * an answer that is built by the handler out of `json!` is checked as the route gives it, over a gateway state of the test's own (the routes that can be asked without a model).
//!
//! What no test reaches is marked `x-aqua-typed: false` in the document (the body is `Untyped`), so a client knows it has nothing to rely on. About a tenth of the routes are typed
//! now: the ones a coding client, a script or the app's run screens use first. Typing another route is one entry in [`ROUTE_TYPES`] and, where it is new, one schema in [`schemas`].
//!
//! The vocabulary of a schema is the part of OpenAPI 3.0 that `scripts/gen_ts_api.py` reads: `type`, `properties`, `required`, `items`, `enum`, `oneOf`, `nullable`, `$ref` and
//! `additionalProperties`. An object is open (a field it does not list is allowed) unless it says otherwise: the API adds fields and a client has to let them through.

use super::contract::ErrorCode;
use serde_json::{json, Map, Value};

/// One field of an object: its name, its schema and whether it is always there.
type Field = (&'static str, Value, bool);

fn req(name: &'static str, schema: Value) -> Field {
    (name, schema, true)
}

fn opt(name: &'static str, schema: Value) -> Field {
    (name, schema, false)
}

fn s() -> Value {
    json!({ "type": "string" })
}

fn i() -> Value {
    json!({ "type": "integer" })
}

fn n() -> Value {
    json!({ "type": "number" })
}

fn b() -> Value {
    json!({ "type": "boolean" })
}

/// A value of any kind (JSON null included).
fn any() -> Value {
    json!({ "nullable": true })
}

fn arr(item: Value) -> Value {
    json!({ "type": "array", "items": item })
}

/// A reference to a schema of this file.
fn r(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{name}") })
}

/// The schema, which may also be `null`.
fn nullable(schema: Value) -> Value {
    if schema.get("$ref").is_some() {
        return json!({ "nullable": true, "allOf": [schema] });
    }
    let mut schema = schema;
    schema["nullable"] = json!(true);
    schema
}

fn en(words: &[&str]) -> Value {
    json!({ "type": "string", "enum": words })
}

/// A word that is always the same: the `type` of one kind of event.
fn tag(word: &str) -> Value {
    json!({ "type": "string", "enum": [word] })
}

/// An object with these fields and nothing said about any other.
fn obj(about: &str, fields: Vec<Field>) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for (name, schema, always) in fields {
        properties.insert(name.to_string(), schema);
        if always {
            required.push(json!(name));
        }
    }
    let mut out = json!({ "type": "object", "description": about, "properties": Value::Object(properties) });
    if !required.is_empty() {
        out["required"] = Value::Array(required);
    }
    out
}

/// An object whose fields are not described (its name says what it is).
fn open(about: &str) -> Value {
    json!({ "type": "object", "description": about, "additionalProperties": true })
}

/// The part of an event that every one has: its version, and where it is in the log (a frame of a stream that is no row has no number).
fn envelope(kind: &str, mut fields: Vec<Field>) -> Value {
    let mut all = vec![req("v", i()), req("type", tag(kind)), opt("id", i()), opt("seq", i()), opt("run_id", s()), opt("ts", i()), opt("truncated", b())];
    all.append(&mut fields);
    obj(kind, all)
}

/// Every schema of the API's bodies, by name (`components.schemas` of the document).
pub fn schemas() -> Map<String, Value> {
    let mut m = Map::new();
    let mut add = |name: &str, schema: Value| {
        m.insert(name.to_string(), schema);
    };
    // ---- the contract itself ---------------------------------------------------------------------------------------------------------------------------------------------------------
    add("ErrorCode", json!({ "type": "string", "description": "What went wrong, in a word that does not change (documentation/ERROR_CODES.md).", "enum": ErrorCode::ALL.map(|c| c.as_str()) }));
    add("Error", obj("The body of every error answer.", vec![req("error", s()), req("code", r("ErrorCode")), opt("hint", s()), opt("retryable", b())]));
    add("Untyped", open("The body is not described yet: it is a JSON object, and what it holds is in the document of the route."));
    add("Version", obj("Which core this is and which contract it speaks.", vec![req("core", s()), req("contract", i()), req("min_client", i())]));
    // ---- the status ------------------------------------------------------------------------------------------------------------------------------------------------------------------
    add("Health", obj("What a caller with no token is told: up, and which version. A caller with a token is told the whole Status.", vec![req("status", s()), req("version", s())]));
    add(
        "VoiceSecurityStatus",
        obj("Whether speaker verification can do its job.", vec![req("enabled", b()), opt("model_present", nullable(b())), opt("enrolled", nullable(b())), req("ready", b()), req("note", s())]),
    );
    add(
        "Status",
        obj(
            "Whether Aqua is up, which engines are ready and how startup ended.",
            vec![
                req("status", s()),
                req("version", s()),
                req("ram_sla_mb", i()),
                opt("ram_usage_mb", i()),
                req("voice_standby_active", b()),
                req("database_ready", b()),
                req("startup_state", s()),
                req("voice_security", r("VoiceSecurityStatus")),
                req("biometric_enrolled", b()),
                req("noise_engine_ready", b()),
                req("whisper_engine_ready", b()),
                req("llm_engine_ready", b()),
                opt("llm_gpu", s()),
                opt("llm_gpu_off", open("Why the graphics card is being left alone, and until when.")),
                opt("background_failed", arr(s())),
                opt("llm_roles", arr(open("The model one role uses and why."))),
                req("tts_engine_ready", b()),
                req("pipeline_ready", b()),
                req("text_pipeline_ready", b()),
                req("models", open("The models found in the models folder.")),
            ],
        ),
    );
    // ---- a turn ----------------------------------------------------------------------------------------------------------------------------------------------------------------------
    add("PipelineTextRequest", obj("A sentence to answer.", vec![req("text", s()), opt("voice", s()), opt("lang", s()), opt("tts", b())]));
    add(
        "PipelineTextResponse",
        obj(
            "The answer of a turn, with its sound.",
            vec![
                req("input_text", s()),
                req("assistant_text", s()),
                req("tts_sample_count", i()),
                req("tts_wav_base64", s()),
                req("tts_sample_rate", i()),
                req("llm_engine_ready", b()),
                req("tts_engine_ready", b()),
                opt("question", open("A question the agent asks and waits on: text and choices (the UI token's answer only).")),
                req("latency_ms", i()),
                req("stages_ms", arr(obj("How long one stage took.", vec![req("stage", s()), req("ms", i())]))),
            ],
        ),
    );
    add("TurnHeard", obj("What was heard (event `heard`).", vec![req("text", s())]));
    add("TurnRun", obj("Which run the turn is, and where to read it again (event `run`).", vec![req("type", tag("run")), req("run_id", s()), req("events", s()), req("stream", s())]));
    add("TurnStep", obj("What the agent is doing (event `step` of a voice or text turn).", vec![req("title", s()), req("status", s())]));
    add("TurnApproval", open("A question that needs the person's answer (event `approval`, the UI token only)."));
    add("TurnSpeech", obj("One spoken piece (event `speech`).", vec![req("index", i()), req("text", s()), req("wav_base64", s())]));
    add(
        "TurnDone",
        obj(
            "The result of the turn (event `done`): the fields of PipelineTextResponse (a voice turn's has its own, see UI_CONTRACT.md), and how it was streamed.",
            vec![
                req("input_text", s()),
                req("assistant_text", s()),
                opt("tts_sample_count", i()),
                opt("tts_wav_base64", s()),
                opt("tts_sample_rate", i()),
                opt("llm_engine_ready", b()),
                opt("tts_engine_ready", b()),
                req("streamed", b()),
                req("first_audio_ms", nullable(i())),
                opt("question", open("The question the agent waits on.")),
                req("latency_ms", i()),
                opt("stages_ms", arr(obj("How long one stage took.", vec![req("stage", s()), req("ms", i())]))),
            ],
        ),
    );
    add("TurnError", obj("The turn failed (event `error`); no `done` follows.", vec![req("error", s()), req("status", i()), opt("code", r("ErrorCode"))]));
    // ---- an agent run ----------------------------------------------------------------------------------------------------------------------------------------------------------------
    add(
        "AgentRequest",
        obj(
            "A task for the agent.",
            vec![
                req("goal", s()),
                opt("max_steps", i()),
                opt("use_browser", b()),
                opt("use_windows", b()),
                opt("force_agent", b()),
                opt("background", b()),
                opt("idempotency_key", nullable(s())),
                opt("profile", nullable(en(&["coding", "files", "web", "desktop"]))),
                opt("allowed_tools", arr(s())),
                opt("forced_packs", arr(s())),
                opt("session_id", s()),
                opt("agent", nullable(s())),
            ],
        ),
    );
    add("AgentStep", obj("One step of a run: a phase, a call of a tool or what the model said.", vec![req("step", i()), req("kind", s()), req("detail", any())]));
    add(
        "AgentResult",
        obj(
            "What an agent run came to.",
            vec![
                req("final_answer", s()),
                req("steps", arr(r("AgentStep"))),
                req("tool_rounds", i()),
                req("stopped_reason", s()),
                opt("gate", open("What the gate decided about the request.")),
                opt("run_id", s()),
                opt("question", s()),
                opt("choices", arr(s())),
                opt("sources", arr(open("A page the answer cites: id, url, title, date."))),
            ],
        ),
    );
    add(
        "JobStarted",
        obj(
            "A background run that was accepted (202), or the one started the first time (200, duplicate).",
            vec![req("job_id", i()), req("state", s()), req("place", i()), req("duplicate", b()), req("status", s())],
        ),
    );
    add("AgentStreamRun", obj("Which run the stream is of (event `run`).", vec![req("type", tag("run")), req("run_id", s()), req("events", s()), req("stream", s())]));
    add(
        "AgentStreamStep",
        obj(
            "One step, in full for the UI token and reduced for any other (event `step`).",
            vec![req("type", tag("step")), req("step", open("The step: number, kind and what the token may see of it."))],
        ),
    );
    add(
        "AgentStreamConfirmation",
        obj("A question that needs the person's answer (event `confirmation`, the UI token only).", vec![req("type", tag("confirmation")), req("request", open("The question."))]),
    );
    add("AgentStreamDone", obj("The result (event `done`).", vec![req("type", tag("done")), req("result", r("AgentResult"))]));
    add("AgentStreamError", obj("The run could not be made (event `error`).", vec![req("type", tag("error")), req("error", s()), opt("code", r("ErrorCode"))]));
    add(
        "RunStartRequest",
        obj(
            "A run that is not tied to the connection.",
            vec![
                req("goal", s()),
                opt("max_steps", i()),
                opt("tools", obj("Which of the two outside tools the run may use.", vec![opt("browser", b()), opt("desktop", b())])),
                opt("force_agent", b()),
                opt("profile", nullable(en(&["coding", "files", "web", "desktop"]))),
                opt("idempotency_key", s()),
                opt("approval_wait_secs", i()),
                opt("directory", s()),
                opt("agent", en(&["build", "plan"])),
                opt("session_id", s()),
                opt("preapproval_id", s()),
            ],
        ),
    );
    add(
        "PreapprovalRequest",
        obj(
            "A pre-approval of a project's checks for one run (UI token; the core must run with AQUA_PREAPPROVALS=on).",
            vec![req("folder", s()), opt("kinds", arr(en(&["tests", "build", "lint", "typecheck"]))), opt("budget", i()), opt("minutes", i())],
        ),
    );
    let preapproval = || {
        vec![
            req("id", s()),
            req("folder", s()),
            req("kinds", arr(en(&["tests", "build", "lint", "typecheck"]))),
            req("budget", i()),
            req("used", i()),
            req("minutes", i()),
            req("expires_in_secs", i()),
            req("run_id", nullable(s())),
            req("state", en(&["open", "in_use", "spent", "expired"])),
        ]
    };
    let mut made = preapproval();
    made.push(req("trusted", b()));
    add("Preapproval", obj("A pre-approval as it was just made: with whether the terminal tools trust its folder (a check in a folder that is not trusted asks as it always did).", made));
    add("PreapprovalView", obj("A pre-approval: what is left of it.", preapproval()));
    add("PreapprovalList", obj("The pre-approvals there are.", vec![req("enabled", b()), req("preapprovals", arr(r("PreapprovalView")))]));
    add("PreapprovalRevoked", obj("A pre-approval taken back.", vec![req("id", s()), req("revoked", b())]));
    add(
        "RunStarted",
        obj(
            "A run that was accepted (202), or the one started the first time (200, duplicate).",
            vec![
                req("run_id", s()),
                req("state", en(&["queued", "running"])),
                req("position", i()),
                req("duplicate", b()),
                req("approval_wait_secs", i()),
                req("stream", s()),
                req("events", s()),
                req("wait", s()),
                req("interrupt", s()),
            ],
        ),
    );
    let summary = || {
        vec![
            req("id", s()),
            req("started_at", i()),
            req("ended_at", nullable(i())),
            req("goal", s()),
            req("caller", s()),
            req("route", s()),
            req("status", s()),
            req("model", s()),
            req("rounds", i()),
            req("stopped_reason", s()),
            req("final_answer", s()),
            req("parent_run_id", nullable(s())),
            req("tool_calls", i()),
            req("stopped_by", s()),
            req("profile", s()),
        ]
    };
    add("RunSummary", obj("A run in a list.", summary()));
    add(
        "RunStep",
        obj(
            "One step of a recorded run: a call of a tool or a reply of the planner, with its arguments masked.",
            vec![
                req("seq", i()),
                req("step", i()),
                req("kind", s()),
                req("tool", s()),
                req("arguments", any()),
                req("observation_digest", s()),
                req("observation", s()),
                req("ok", nullable(b())),
                req("status", s()),
                req("decision", s()),
                req("approval", s()),
                req("ms", nullable(i())),
                req("tokens_in", nullable(i())),
                req("tokens_out", nullable(i())),
                req("cache_read", nullable(i())),
                req("planner_reply", s()),
                req("idempotency_digest", s()),
                req("ts", i()),
            ],
        ),
    );
    let mut whole = summary();
    whole.extend([req("tokens_in", i()), req("tokens_out", i()), req("plan", any()), req("taint", any()), req("triage", any()), req("diagnosis", any()), req("steps", arr(r("RunStep")))]);
    // What the sessions and the state of a run add to it (card OA10-04): the session it is in, its checklist and the lines the person added to it, the plan it wrote and what became of it, and
    // whether a run that used its rounds or its time can be continued.
    whole.extend([opt("session_id", s()), opt("checklist", arr(open("One item of the checklist: text and status."))), opt("steered", arr(s())), opt("plan_approval", any()), opt("continuable", b()), opt("record", any())]);
    add("Run", obj("A run with all its steps (the UI token).", whole));
    add("RunList", obj("The newest runs first.", vec![req("runs", arr(r("RunSummary")))]));
    add("AnswerRequest", obj("The answer to a question a run ended with: exactly one of the two.", vec![opt("text", s()), opt("choice", i())]));
    add(
        "AnswerResponse",
        obj(
            "What the run that went on from the answer came to.",
            vec![
                req("run_id", s()),
                req("continued_by", nullable(s())),
                req("answer", s()),
                req("stopped_reason", s()),
                req("tool_rounds", i()),
                req("question", nullable(open("The next question, when it asks again."))),
                req("latency_ms", i()),
            ],
        ),
    );
    add("RunInterrupted", obj("A run that was told to end.", vec![req("run_id", s()), req("interrupted", b()), req("ended", b()), req("state", en(&["cancelled", "stopping"])), opt("note", s())]));
    add("MessageRequest", obj("A line said to a run: one line, at most 500 characters.", vec![req("text", s()), opt("delivery", en(&["steer", "queue"]))]));
    add(
        "MessageAccepted",
        obj(
            "What became of a line said to a run: added to the next step (`queued`), kept for after the run (`kept`) or the run was stopped by it (`stopped`).",
            vec![req("run_id", s()), opt("delivery", en(&["steer", "queue"])), opt("queued", i()), opt("kept", i()), opt("stopped", b()), opt("note", s())],
        ),
    );
    add(
        "RunQueue",
        obj(
            "What was said to a run and is waiting.",
            vec![
                req("run_id", s()),
                req("steer", arr(obj("A line.", vec![req("n", i()), req("text", s())]))),
                req("queue", arr(obj("A line.", vec![req("n", i()), req("text", s())]))),
                req("max", i()),
            ],
        ),
    );
    add(
        "RunWait",
        obj(
            "Where a run stands: `finished` is true once it has ended (and, for a detached run, its outcome is written down).",
            vec![
                req("run_id", s()),
                req("state", s()),
                req("finished", b()),
                opt("position", i()),
                opt("goal", s()),
                req("needs", nullable(en(&["approval", "answer"]))),
                opt("status", s()),
                opt("stopped_reason", s()),
                opt("answer", s()),
                opt("started_at", i()),
                opt("ended_at", nullable(i())),
                opt("stopped_by", s()),
            ],
        ),
    );
    add(
        "ActiveRuns",
        obj(
            "The runs that are not over.",
            vec![req(
                "runs",
                arr(obj(
                    "A run that is going or waits.",
                    vec![req("run_id", s()), req("state", s()), opt("position", i()), req("goal", s()), req("origin", s()), req("detached", b()), req("started_at", i()), opt("route", s())],
                )),
            )],
        ),
    );
    add("ResumeRequest", obj("What to do about the calls that were running when the run stopped.", vec![opt("unknown", en(&["skip", "retry", "stop"]))]));
    add(
        "ResumeResponse",
        obj(
            "What the run that went on came to; or, for `unknown: stop`, that the run will not be offered again.",
            vec![
                req("run_id", s()),
                opt("state", s()),
                opt("note", s()),
                opt("continued_by", nullable(s())),
                opt("answer", s()),
                opt("question", nullable(open("The question the run asks and waits on."))),
                opt("latency_ms", i()),
            ],
        ),
    );
    add("UndoRequest", obj("What to put back.", vec![opt("dry_run", b()), opt("from_step", i())]));
    add(
        "UndoResponse",
        obj(
            "What the undo did (or would do), and what no undo can reach.",
            vec![req("run_id", s()), req("undo", open("The result of the tool `fs.undo_run`.")), req("not_undoable", arr(any())), req("note", s())],
        ),
    );
    add("SteerRequest", obj("A line for the task that is running.", vec![req("text", s())]));
    add("SteerAccepted", obj("What became of the line.", vec![opt("queued", i()), opt("later", i()), opt("stopped", b()), opt("note", s())]));
    // ---- the events of a run -------------------------------------------------------------------------------------------------------------------------------------------------------
    let events = vec![
        envelope("run.started", vec![opt("session_id", s()), req("goal_head", s()), req("caller", s()), req("route", s()), opt("directory", s()), req("started_ms", i())]),
        envelope("status", vec![req("state", en(&["queued", "running", "waiting_approval", "waiting_user", "retry", "idle"])), opt("position", i()), opt("reason", s())]),
        envelope("step", vec![req("step", i()), req("phase", s()), req("title", s()), req("status", s())]),
        envelope("text", vec![req("index", i()), req("text", s())]),
        envelope("tool.started", vec![req("call_id", i()), req("tool", s()), opt("args_head", s())]),
        envelope("tool.finished", vec![req("call_id", i()), req("ok", b()), req("status", s()), req("ms", i()), req("decision", s()), req("approval", s()), opt("observation_head", s())]),
        envelope("tool.progress", vec![req("call_id", i()), req("piece", i()), req("text", s()), req("skipped", i())]),
        envelope("approval.opened", vec![opt("request", open("The question, with the exact action and its target (the UI token only)."))]),
        envelope("approval.closed", vec![req("request_id", s()), req("outcome", en(&["approved", "approved_session", "denied", "timeout", "cancelled"]))]),
        envelope("usage", vec![opt("tokens_in", i()), opt("tokens_out", i()), opt("ttft_ms", i()), opt("decode_tps", n()), opt("ctx_used", i()), opt("ctx_window", i())]),
        envelope("limit", vec![req("stopped_by", s()), req("used", i()), req("cap", i())]),
        envelope("question", vec![opt("text", s()), opt("choices", arr(s()))]),
        envelope("run.finished", vec![req("status", s()), req("stopped_reason", s()), req("answer_head", s()), opt("why_it_stopped", s()), req("files_changed", i()), req("ms", i())]),
        envelope("error", vec![req("code", s()), req("message", s()), req("retryable", b())]),
        envelope("heartbeat", vec![]),
        envelope("end", vec![req("last", i())]),
        envelope("server.connected", vec![req("last", i())]),
    ];
    add(
        "RunEvent",
        json!({ "description": "One event of a run, as a stream or a page gives it: the envelope (v, id, seq, run_id, ts, type) and the fields of its type. The fields that show what a call was given or returned, and a question, are sent to the UI token only.", "oneOf": events }),
    );
    add(
        "RunEventsPage",
        obj(
            "A page of a run's events: ask again with `after=<last>` until `finished`.",
            vec![req("run_id", s()), req("events", arr(r("RunEvent"))), req("last", i()), req("finished", b()), opt("state", s()), opt("position", i())],
        ),
    );
    // ---- tools, jobs, commands, flags --------------------------------------------------------------------------------------------------------------------------------------
    add("ToolCallRequest", obj("A call of one tool of the tool host.", vec![req("tool", s()), opt("arguments", any()), opt("dry_run", b()), opt("timeout_ms", i()), opt("session_id", nullable(s()))]));
    add(
        "ToolResult",
        obj(
            "The envelope every tool call answers with, whatever the status code: `error` is an object with the tool host's own codes (not the codes of ErrorCode).",
            vec![
                req("ok", b()),
                req("tool", s()),
                req("request_id", s()),
                req("data", any()),
                req("text", s()),
                req("truncated", b()),
                req("artifacts", arr(open("A file the tool made."))),
                req("warnings", arr(s())),
                req("approval", nullable(open("How the question about the call was answered."))),
                req("duration_ms", i()),
                req("error", nullable(obj("What went wrong with the call.", vec![req("code", s()), req("message", s()), req("retryable", b()), opt("details", any())]))),
            ],
        ),
    );
    add(
        "QueueJob",
        obj(
            "A background run as the queue shows it.",
            vec![
                req("id", i()),
                req("goal", s()),
                req("state", en(&["queued", "running", "waiting", "done", "failed", "cancelled"])),
                req("queued_at_ms", i()),
                req("started_at_ms", nullable(i())),
                req("ended_at_ms", nullable(i())),
                req("run_id", nullable(s())),
                req("answer", s()),
                req("stopped_reason", s()),
                req("error", s()),
                req("outcome", s()),
                req("paused_ms", i()),
                req("waiting_for", s()),
            ],
        ),
    );
    add(
        "QueueList",
        obj(
            "The background runs, newest first.",
            vec![
                req("jobs", arr(r("QueueJob"))),
                req("waiting", i()),
                req("running", b()),
                req("foreground_active", i()),
                req("max_waiting", i()),
                req(
                    "depth",
                    obj(
                        "How full the queue is.",
                        vec![req("queued", i()), req("running", b()), req("waiting_for_the_person", b()), req("max_waiting", i()), req("free_places", i()), req("to_tell", i())],
                    ),
                ),
            ],
        ),
    );
    add("JobCancelled", obj("The state a background run was left in.", vec![req("job_id", i()), req("state", s())]));
    add(
        "JobsList",
        obj(
            "The background jobs of the core.",
            vec![req("jobs", arr(open("A job: name, kind, state, last sign of life."))), req("running", i()), req("stuck", i()), req("stuck_after_beats", i()), req("now_ms", i())],
        ),
    );
    add(
        "CommandRunRequest",
        obj(
            "Start a custom command.",
            vec![opt("arguments", s()), opt("folder", nullable(s())), opt("background", b()), opt("max_steps", nullable(i())), opt("use_browser", nullable(b())), opt("use_windows", nullable(b()))],
        ),
    );
    add(
        "CommandsList",
        obj(
            "The commands that can be started (off unless AQUA_COMMANDS=on).",
            vec![
                req("enabled", b()),
                req("commands", arr(open("A command: name, description, hint, where it came from."))),
                opt("problems", arr(any())),
                opt("warnings", arr(any())),
                opt("note", s()),
            ],
        ),
    );
    add(
        "AgentsList",
        obj(
            "The agents there are: the built-in ones, yours, and those of a project whose .aqua text you trusted (off unless AQUA_AGENTS=on).",
            vec![
                req("enabled", b()),
                req("agents", arr(open("An agent: name, description, mode, where it came from, the tools it allows, its packs and limits; never the words of its instructions."))),
                opt("problems", arr(any())),
                opt("warnings", arr(any())),
                opt("note", s()),
            ],
        ),
    );
    add("FlagsList", obj("The typed flags: name, kind, default, what each is now.", vec![req("flags", arr(open("A flag.")))]));
    add("Cancelled", obj("The running agents were told to stop.", vec![req("cancelled", b())]));
    add("TaskStopped", obj("The task that was running was stopped.", vec![req("stopped", b()), req("idle", b()), req("waiting", i())]));
    add("OpenApiDocument", open("This document."));
    m
}

/// What a route's body is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    /// No body is sent.
    Empty,
    /// A JSON body of this schema.
    Of(&'static str),
}

/// What a route says about its bodies. A route that is not here is `Untyped`.
#[derive(Debug, Clone, Copy)]
pub struct RouteType {
    pub method: &'static str,
    /// The path as the document writes it (`{run_id}`).
    pub path: &'static str,
    pub request: Body,
    /// The answers that are not errors: the status and the schema of the body.
    pub ok: &'static [(u16, &'static str)],
    /// The answer is a stream of server-sent events: the schema of the `data:` of an event (or, with `events`, the event names and the schema of each).
    pub stream: Option<&'static str>,
    /// For a stream of named events whose data are different schemas: (event name, schema).
    pub events: &'static [(&'static str, &'static str)],
    /// The query parameters: (name, JSON type).
    pub query: &'static [(&'static str, &'static str)],
    /// The path parameters that are numbers.
    pub ints: &'static [&'static str],
    /// The schema of the body of an error answer (`Error` unless the route says otherwise).
    pub errors: &'static str,
}

const NO_QUERY: &[(&str, &str)] = &[];
const NO_EVENTS: &[(&str, &str)] = &[];

const fn json_route(method: &'static str, path: &'static str, request: Body, ok: &'static [(u16, &'static str)]) -> RouteType {
    RouteType { method, path, request, ok, stream: None, events: NO_EVENTS, query: NO_QUERY, ints: &[], errors: "Error" }
}

const fn with_query(mut route: RouteType, query: &'static [(&'static str, &'static str)]) -> RouteType {
    route.query = query;
    route
}

const fn streaming(method: &'static str, path: &'static str, request: Body, data: &'static str) -> RouteType {
    RouteType { method, path, request, ok: &[], stream: Some(data), events: NO_EVENTS, query: NO_QUERY, ints: &[], errors: "Error" }
}

const fn named_events(mut route: RouteType, events: &'static [(&'static str, &'static str)]) -> RouteType {
    route.events = events;
    route
}

const TURN_EVENTS: &[(&str, &str)] =
    &[("heard", "TurnHeard"), ("run", "TurnRun"), ("step", "TurnStep"), ("approval", "TurnApproval"), ("speech", "TurnSpeech"), ("done", "TurnDone"), ("error", "TurnError")];
const AGENT_EVENTS: &[(&str, &str)] =
    &[("run", "AgentStreamRun"), ("step", "AgentStreamStep"), ("confirmation", "AgentStreamConfirmation"), ("done", "AgentStreamDone"), ("error", "AgentStreamError")];
const AFTER: &[(&str, &str)] = &[("after", "integer")];
/// `?tts=0` asks for the words only, as `"tts": false` in the body does (card OA10-06, UX-08).
const TTS: &[(&str, &str)] = &[("tts", "string")];

/// The routes that have schemas for their bodies.
pub const ROUTE_TYPES: &[RouteType] = &[
    json_route("get", "/api/v1/health", Body::Empty, &[(200, "Health")]),
    json_route("get", "/api/v1/status", Body::Empty, &[(200, "Status")]),
    json_route("get", "/api/v1/version", Body::Empty, &[(200, "Version")]),
    json_route("get", "/api/v1/openapi.json", Body::Empty, &[(200, "OpenApiDocument")]),
    with_query(json_route("post", "/api/v1/pipeline/text", Body::Of("PipelineTextRequest"), &[(200, "PipelineTextResponse")]), TTS),
    with_query(named_events(streaming("post", "/api/v1/pipeline/text/stream", Body::Of("PipelineTextRequest"), "TurnDone"), TURN_EVENTS), TTS),
    json_route("post", "/api/v1/mcp/agent", Body::Of("AgentRequest"), &[(200, "AgentResult"), (202, "JobStarted")]),
    named_events(streaming("post", "/api/v1/mcp/agent/stream", Body::Of("AgentRequest"), "AgentStreamDone"), AGENT_EVENTS),
    json_route("post", "/api/v1/runs", Body::Of("RunStartRequest"), &[(200, "RunStarted"), (202, "RunStarted")]),
    with_query(json_route("get", "/api/v1/runs", Body::Empty, &[(200, "RunList")]), &[("limit", "integer"), ("offset", "integer"), ("status", "string"), ("resumable", "string")]),
    json_route("post", "/api/v1/preapprovals", Body::Of("PreapprovalRequest"), &[(200, "Preapproval")]),
    json_route("get", "/api/v1/preapprovals", Body::Empty, &[(200, "PreapprovalList")]),
    json_route("delete", "/api/v1/preapprovals/{id}", Body::Empty, &[(200, "PreapprovalRevoked")]),
    json_route("get", "/api/v1/runs/{run_id}", Body::Empty, &[(200, "Run")]),
    json_route("post", "/api/v1/runs/{run_id}/answer", Body::Of("AnswerRequest"), &[(200, "AnswerResponse")]),
    json_route("post", "/api/v1/runs/{run_id}/interrupt", Body::Empty, &[(200, "RunInterrupted")]),
    json_route("post", "/api/v1/runs/{run_id}/messages", Body::Of("MessageRequest"), &[(200, "MessageAccepted"), (202, "MessageAccepted")]),
    with_query(json_route("get", "/api/v1/runs/{run_id}/events", Body::Empty, &[(200, "RunEventsPage")]), &[("after", "integer"), ("limit", "integer")]),
    with_query(streaming("get", "/api/v1/runs/{run_id}/stream", Body::Empty, "RunEvent"), AFTER),
    with_query(streaming("get", "/api/v1/events", Body::Empty, "RunEvent"), AFTER),
    with_query(json_route("get", "/api/v1/runs/{run_id}/wait", Body::Empty, &[(200, "RunWait")]), &[("timeout", "integer")]),
    json_route("get", "/api/v1/runs/active", Body::Empty, &[(200, "ActiveRuns")]),
    json_route("get", "/api/v1/runs/{run_id}/queue", Body::Empty, &[(200, "RunQueue")]),
    json_route("post", "/api/v1/runs/{run_id}/resume", Body::Of("ResumeRequest"), &[(200, "ResumeResponse")]),
    json_route("post", "/api/v1/runs/{run_id}/undo", Body::Of("UndoRequest"), &[(200, "UndoResponse"), (422, "UndoResponse")]),
    json_route("post", "/api/v1/turn/steer", Body::Of("SteerRequest"), &[(200, "SteerAccepted"), (202, "SteerAccepted")]),
    RouteType { errors: "ToolResult", ..json_route("post", "/api/v1/tools/call", Body::Of("ToolCallRequest"), &[(200, "ToolResult")]) },
    json_route("get", "/api/v1/queue", Body::Empty, &[(200, "QueueList")]),
    RouteType { ints: &["job_id"], ..json_route("post", "/api/v1/queue/{job_id}/cancel", Body::Empty, &[(200, "JobCancelled")]) },
    json_route("get", "/api/v1/jobs", Body::Empty, &[(200, "JobsList")]),
    with_query(json_route("get", "/api/v1/commands", Body::Empty, &[(200, "CommandsList")]), &[("folder", "string")]),
    json_route("post", "/api/v1/commands/{name}/run", Body::Of("CommandRunRequest"), &[(200, "AgentResult"), (202, "JobStarted")]),
    with_query(json_route("get", "/api/v1/agents", Body::Empty, &[(200, "AgentsList")]), &[("folder", "string")]),
    json_route("get", "/api/v1/flags", Body::Empty, &[(200, "FlagsList")]),
    json_route("post", "/api/v1/agent/cancel", Body::Empty, &[(200, "Cancelled")]),
    json_route("post", "/api/v1/tasks/stop", Body::Empty, &[(200, "TaskStopped")]),
];

/// The entry of a route, by method (lower case) and path (as the document writes it).
pub fn route_type(method: &str, path: &str) -> Option<&'static RouteType> {
    ROUTE_TYPES.iter().find(|t| t.method == method && t.path == path)
}

/// The schema names a schema refers to.
pub fn refs_of(schema: &Value, out: &mut Vec<String>) {
    match schema {
        Value::Object(map) => {
            for (key, value) in map {
                if key == "$ref" {
                    if let Some(name) = value.as_str().and_then(|r| r.strip_prefix("#/components/schemas/")) {
                        out.push(name.to_string());
                    }
                } else {
                    refs_of(value, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| refs_of(v, out)),
        _ => {}
    }
}

#[cfg(test)]
pub(crate) mod conform {
    //! A small checker of a value against one of these schemas: types, required fields, enums, items, `oneOf` and references. With `strict`, a field an object does not list is an error
    //! (to catch a field that was added to a type and not to its schema); the API itself is open.
    use super::*;

    pub fn check(name: &str, value: &Value, strict: bool) -> Result<(), String> {
        let all = schemas();
        let schema = all.get(name).ok_or_else(|| format!("no schema called {name}"))?;
        walk(&all, schema, value, strict, name)
    }

    fn walk(all: &Map<String, Value>, schema: &Value, value: &Value, strict: bool, at: &str) -> Result<(), String> {
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            let name = reference.strip_prefix("#/components/schemas/").ok_or_else(|| format!("{at}: a reference that is not to a schema: {reference}"))?;
            let target = all.get(name).ok_or_else(|| format!("{at}: {name} is not a schema"))?;
            return walk(all, target, value, strict, &format!("{at}/{name}"));
        }
        if let Some(inner) = schema.get("allOf").and_then(Value::as_array) {
            if value.is_null() && schema.get("nullable") == Some(&json!(true)) {
                return Ok(());
            }
            return inner.iter().try_for_each(|s| walk(all, s, value, strict, at));
        }
        if value.is_null() && schema.get("nullable") == Some(&json!(true)) {
            return Ok(());
        }
        if let Some(options) = schema.get("oneOf").and_then(Value::as_array) {
            let mut failures = Vec::new();
            let mut hits = 0;
            for option in options {
                match walk(all, option, value, strict, at) {
                    Ok(()) => hits += 1,
                    Err(e) => failures.push(e),
                }
            }
            return match hits {
                1 => Ok(()),
                0 => Err(format!("{at}: none of the {} kinds fits; the nearest said: {}", options.len(), failures.iter().min_by_key(|f| f.len()).cloned().unwrap_or_default())),
                _ => Err(format!("{at}: {hits} kinds fit, one should")),
            };
        }
        if let Some(words) = schema.get("enum").and_then(Value::as_array) {
            if !words.contains(value) {
                return Err(format!("{at}: {value} is not one of {words:?}"));
            }
        }
        let Some(kind) = schema.get("type").and_then(Value::as_str) else { return Ok(()) };
        let fits = match kind {
            "string" => value.is_string(),
            "integer" => value.is_i64() || value.is_u64(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            other => return Err(format!("{at}: the schema has a type this checker does not know: {other}")),
        };
        if !fits {
            return Err(format!("{at}: expected {kind}, found {value}"));
        }
        match kind {
            "array" => {
                if let (Some(items), Some(values)) = (schema.get("items"), value.as_array()) {
                    for (n, v) in values.iter().enumerate() {
                        walk(all, items, v, strict, &format!("{at}[{n}]"))?;
                    }
                }
            }
            "object" => {
                let map = value.as_object().expect("an object");
                let properties = schema.get("properties").and_then(Value::as_object);
                for needed in schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                    if !map.contains_key(needed) {
                        return Err(format!("{at}: the field {needed} is missing from {value}"));
                    }
                }
                for (key, v) in map {
                    match properties.and_then(|p| p.get(key)) {
                        Some(property) => walk(all, property, v, strict, &format!("{at}.{key}"))?,
                        None if strict && properties.is_some() && schema.get("additionalProperties").is_none() => return Err(format!("{at}: the field {key} is not in the schema")),
                        None => {}
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// A value a client could send for this schema: every required field, filled with a value of its type (a first word of an enum, `1`, `"x"`, `true`).
    pub fn example(name: &str) -> Value {
        let all = schemas();
        fill(&all, all.get(name).unwrap_or_else(|| panic!("no schema called {name}")))
    }

    fn fill(all: &Map<String, Value>, schema: &Value) -> Value {
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            return fill(all, &all[reference.trim_start_matches("#/components/schemas/")]);
        }
        if let Some(inner) = schema.get("allOf").and_then(Value::as_array) {
            return fill(all, &inner[0]);
        }
        if let Some(options) = schema.get("oneOf").and_then(Value::as_array) {
            return fill(all, &options[0]);
        }
        if let Some(words) = schema.get("enum").and_then(Value::as_array) {
            return words[0].clone();
        }
        match schema.get("type").and_then(Value::as_str) {
            Some("string") => json!("x"),
            Some("integer") => json!(1),
            Some("number") => json!(1.5),
            Some("boolean") => json!(true),
            Some("array") => json!([]),
            Some("object") => {
                let mut out = Map::new();
                for needed in schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                    out.insert(needed.to_string(), fill(all, &schema["properties"][needed]));
                }
                Value::Object(out)
            }
            _ => json!(null),
        }
    }

    /// The example with every field of an object filled in, not only the required ones.
    pub fn full_example(name: &str) -> Value {
        let all = schemas();
        fill_all(&all, all.get(name).unwrap_or_else(|| panic!("no schema called {name}")))
    }

    fn fill_all(all: &Map<String, Value>, schema: &Value) -> Value {
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            return fill_all(all, &all[reference.trim_start_matches("#/components/schemas/")]);
        }
        if let Some(inner) = schema.get("allOf").and_then(Value::as_array) {
            return fill_all(all, &inner[0]);
        }
        if schema.get("type").and_then(Value::as_str) == Some("object") {
            let mut out = Map::new();
            for (key, property) in schema.get("properties").and_then(Value::as_object).into_iter().flatten() {
                out.insert(key.clone(), fill_all(all, property));
            }
            return Value::Object(out);
        }
        fill(all, schema)
    }
}

#[cfg(test)]
mod tests {
    use super::conform::{check, example, full_example};
    use super::*;
    use crate::agent_runs::events::{self, ApprovalOutcome, Event, RunState, Stored};

    #[test]
    fn every_reference_of_every_schema_names_a_schema_and_none_is_left_unused_by_a_route() {
        let all = schemas();
        let mut used = Vec::new();
        for (name, schema) in &all {
            let mut found = Vec::new();
            refs_of(schema, &mut found);
            for target in &found {
                assert!(all.contains_key(target), "{name} refers to {target}, which is not a schema");
            }
            used.extend(found);
        }
        for route in ROUTE_TYPES {
            if let Body::Of(name) = route.request {
                used.push(name.to_string());
            }
            used.extend(route.ok.iter().map(|(_, n)| n.to_string()));
            used.extend(route.stream.map(str::to_string));
            used.extend(route.events.iter().map(|(_, n)| n.to_string()));
            used.push(route.errors.to_string());
            for name in route.ok.iter().map(|(_, n)| *n).chain(route.stream).chain(route.events.iter().map(|(_, n)| *n)).chain(std::iter::once(route.errors)).chain(match route.request {
                Body::Of(n) => Some(n),
                Body::Empty => None,
            }) {
                assert!(all.contains_key(name), "{} {} names {name}, which is not a schema", route.method, route.path);
            }
        }
        used.push("Untyped".into());
        let unused: Vec<&String> = all.keys().filter(|k| !used.contains(k)).collect();
        assert!(unused.is_empty(), "schemas that no route and no other schema uses: {unused:?}");
    }

    #[test]
    fn the_table_has_no_route_twice_and_gives_every_stream_a_schema_for_its_events() {
        let mut seen = std::collections::BTreeSet::new();
        for route in ROUTE_TYPES {
            assert!(seen.insert((route.method, route.path)), "{} {} is typed twice", route.method, route.path);
            assert!(route.path.starts_with("/api/v1/") && matches!(route.method, "get" | "post" | "put" | "delete"), "{} {}", route.method, route.path);
            assert_eq!(route.ok.is_empty(), route.stream.is_some(), "{} {}: a route is a stream or has answers, not both and not neither", route.method, route.path);
            for name in route.ints {
                assert!(route.path.contains(&format!("{{{name}}}")), "{} has no path parameter {name}", route.path);
            }
        }
        assert!(ROUTE_TYPES.len() >= 30, "{} typed routes", ROUTE_TYPES.len());
    }

    // ---- requests: what a client sends is read by the type the handler reads ----------------------------------------------------------------------------------------

    #[test]
    fn the_requests_that_a_client_builds_from_the_schema_are_read_by_the_types_the_handlers_read() {
        use super::super::run_control_api::{AnswerRequest, StartRequest};
        use super::super::server::{CommandRunRequest, PipelineTextRequest, ToolCallRequest, UnifiedMcpAgentRequest};
        fn reads<T: serde::de::DeserializeOwned>(schema: &str) {
            let required = example(schema);
            serde_json::from_value::<T>(required.clone()).unwrap_or_else(|e| panic!("{schema}: the type does not read what the schema asks of a client ({required}): {e}"));
            let everything = full_example(schema);
            serde_json::from_value::<T>(everything.clone()).unwrap_or_else(|e| panic!("{schema}: the type does not read a body with every field of the schema ({everything}): {e}"));
        }
        // (The default voice of a turn is read from the person's settings: a folder of the test's own is Aqua's while it is read.)
        let world = crate::test_support::TempWorld::new("contract_requests");
        crate::runtime::data_tripwire::with_workspace_root(&world.root, || {
            reads::<PipelineTextRequest>("PipelineTextRequest");
            reads::<UnifiedMcpAgentRequest>("AgentRequest");
            reads::<StartRequest>("RunStartRequest");
            reads::<AnswerRequest>("AnswerRequest");
            reads::<ToolCallRequest>("ToolCallRequest");
            reads::<CommandRunRequest>("CommandRunRequest");
            reads::<super::super::preapproval_api::CreateRequest>("PreapprovalRequest");
            // A field the type needs and the schema does not ask for is a field a client would leave out: it fails here.
            assert!(serde_json::from_value::<PipelineTextRequest>(json!({})).is_err(), "the check can fail");
        });
    }

    #[test]
    fn the_checker_fails_when_it_should() {
        assert!(check("Version", &json!({ "core": "1", "contract": 1 }), false).is_err(), "a missing required field");
        assert!(check("Version", &json!({ "core": "1", "contract": "one", "min_client": 1 }), false).is_err(), "a wrong type");
        assert!(check("Error", &json!({ "error": "x", "code": "no_such_code" }), false).is_err(), "a word that is not a code");
        assert!(check("Version", &json!({ "core": "1", "contract": 1, "min_client": 1, "extra": 1 }), true).is_err(), "strict: a field that is not listed");
        assert!(check("Version", &json!({ "core": "1", "contract": 1, "min_client": 1, "extra": 1 }), false).is_ok(), "open: a field that is not listed");
        assert!(check("RunEvent", &json!({ "v": 1, "type": "no.such.event" }), false).is_err(), "a kind that is not one");
    }

    // ---- answers that are Rust types -------------------------------------------------------------------------------------------------------------------------------------

    fn every_event() -> Vec<Event> {
        vec![
            Event::RunStarted { session_id: Some("s".into()), goal_head: "g".into(), caller: "agent".into(), route: "agent".into(), directory: Some("d".into()), started_ms: 5 },
            Event::Status { state: RunState::WaitingApproval, position: Some(1), reason: Some("r".into()) },
            Event::Step { step: 2, phase: "act".into(), title: "Calling fs.read".into(), status: "running".into() },
            Event::Text { index: 0, text: "Done.".into() },
            Event::ToolStarted { call_id: 3, tool: "fs.read".into(), args_head: "{}".into() },
            Event::ToolFinished { call_id: 3, ok: true, status: "completed".into(), ms: 12, decision: "auto".into(), approval: "".into(), observation_head: "x".into() },
            Event::ToolProgress { call_id: 3, piece: 1, text: "line".into(), skipped: 0 },
            Event::ApprovalOpened { request: json!({ "request_id": "q1" }) },
            Event::ApprovalClosed { request_id: "q1".into(), outcome: ApprovalOutcome::ApprovedSession },
            Event::Usage { tokens_in: Some(10), tokens_out: Some(5), ttft_ms: Some(300), decode_tps: Some(14.5), ctx_used: Some(900), ctx_window: Some(6144) },
            Event::Limit { stopped_by: "max_tool_calls".into(), used: 30, cap: 30 },
            Event::Question { text: "Which file?".into(), choices: vec!["a".into()] },
            Event::RunFinished { status: "completed".into(), stopped_reason: "final".into(), answer_head: "ok".into(), why_it_stopped: Some("it finished".into()), files_changed: 2, ms: 900 },
            Event::Error { code: "internal".into(), message: "boom".into(), retryable: false },
            Event::Heartbeat,
            Event::End { last: 9 },
            Event::Connected { last: 9 },
        ]
    }

    #[test]
    fn every_kind_of_run_event_conforms_to_its_schema_and_the_schema_lists_every_field_it_has() {
        let all = every_event();
        assert_eq!(all.len(), Event::TYPES.len() + 1, "an event kind was added: give it an entry in every_event and in the RunEvent schema");
        let kinds: std::collections::BTreeSet<&str> = all.iter().map(|e| e.kind()).collect();
        assert_eq!(kinds.len(), all.len(), "every kind once");
        for event in &all {
            // As a stream's frame: the envelope and every field of the event, the UI-only ones too.
            let frame = events::frame(event);
            check("RunEvent", &frame, true).unwrap_or_else(|e| panic!("{} as a frame: {e}", event.kind()));
            // As a row of the log, read back for the UI token and for the API token.
            let prepared = event.prepare();
            let stored = Stored { id: 41, run_id: "run_1".into(), seq: 3, ts: 1_700_000_000, kind: prepared.kind.to_string(), data: prepared.public.clone(), ui: prepared.ui.clone() };
            for ui in [true, false] {
                check("RunEvent", &stored.wire(ui), true).unwrap_or_else(|e| panic!("{} as a row (ui {ui}): {e}", event.kind()));
            }
        }
        // The ones that are not rows (progress aside) have no number of their own.
        let heartbeat = events::frame(&Event::Heartbeat);
        assert_eq!(heartbeat["type"], "heartbeat");
        assert!(heartbeat.get("seq").is_none());
    }

    #[test]
    fn the_agent_result_and_the_tool_envelope_conform_and_the_schema_lists_every_field() {
        use crate::modules::browser_agent::{AgentStep, McpAgentResult};
        let result = McpAgentResult {
            final_answer: "Done.".into(),
            steps: vec![AgentStep { step: 1, kind: "phase".into(), detail: json!({ "title": "t" }) }],
            tool_rounds: 2,
            stopped_reason: "final".into(),
            gate: None,
            run_id: Some("run_1".into()),
            question: Some("Which?".into()),
            choices: vec!["a".into(), "b".into()],
            sources: vec![json!({ "id": 1, "url": "https://example.com", "title": "t", "date": "" })],
        };
        check("AgentResult", &serde_json::to_value(&result).unwrap(), true).unwrap_or_else(|e| panic!("{e}"));
        let bare =
            McpAgentResult { final_answer: "x".into(), steps: vec![], tool_rounds: 0, stopped_reason: "final".into(), gate: None, run_id: None, question: None, choices: vec![], sources: vec![] };
        check("AgentResult", &serde_json::to_value(&bare).unwrap(), true).unwrap_or_else(|e| panic!("{e}"));
        let ok = aqua_tool_host::ToolResult {
            ok: true,
            tool: "fs.read".into(),
            request_id: "r".into(),
            data: json!({ "bytes": 3 }),
            text: "abc".into(),
            truncated: false,
            artifacts: vec![],
            warnings: vec!["w".into()],
            approval: None,
            duration_ms: 4,
            error: None,
        };
        check("ToolResult", &serde_json::to_value(&ok).unwrap(), true).unwrap_or_else(|e| panic!("{e}"));
        let refused = aqua_tool_host::ToolResult::failure("fs.write", "r", aqua_tool_host::ToolError::new(aqua_tool_host::ErrorCode::InvalidArguments, "no"), 1);
        check("ToolResult", &serde_json::to_value(&refused).unwrap(), true).unwrap_or_else(|e| panic!("{e}"));
    }

    #[test]
    fn a_job_of_the_queue_conforms_and_the_schema_lists_every_field() {
        use crate::queue::{JobView, State};
        for state in [State::Queued, State::Running, State::Waiting, State::Done, State::Failed, State::Cancelled] {
            let job = JobView {
                id: 3,
                goal: "tidy the notes".into(),
                state,
                queued_at_ms: 1,
                started_at_ms: Some(2),
                ended_at_ms: None,
                run_id: Some("run_1".into()),
                answer: String::new(),
                stopped_reason: String::new(),
                error: String::new(),
                outcome: String::new(),
                paused_ms: 0,
                waiting_for: String::new(),
            };
            check("QueueJob", &serde_json::to_value(&job).unwrap(), true).unwrap_or_else(|e| panic!("{state:?}: {e}"));
        }
    }

    #[test]
    fn the_answers_of_a_turn_conform_and_the_schema_lists_every_field() {
        use super::super::server::PipelineTextResponse;
        use crate::pipeline::PipelineTextResult;
        let answer = PipelineTextResponse {
            result: PipelineTextResult {
                input_text: "hi".into(),
                assistant_text: "hello".into(),
                tts_sample_count: 0,
                tts_wav_base64: String::new(),
                tts_sample_rate: 24_000,
                llm_engine_ready: true,
                tts_engine_ready: true,
            },
            question: Some(json!({ "text": "Which?", "choices": [] })),
            latency_ms: 40,
            stages_ms: vec![crate::turn_timing::StageMs { stage: "queued", ms: 1 }],
        };
        check("PipelineTextResponse", &serde_json::to_value(&answer).unwrap(), true).unwrap_or_else(|e| panic!("{e}"));
        // The `done` event of a streamed turn is that, with how it was streamed (`streamed_done` in server.rs).
        let mut done = serde_json::to_value(&answer).unwrap();
        done["streamed"] = json!(true);
        done["first_audio_ms"] = json!(null);
        check("TurnDone", &done, true).unwrap_or_else(|e| panic!("{e}"));
        done["first_audio_ms"] = json!(120);
        check("TurnDone", &done, true).unwrap_or_else(|e| panic!("{e}"));
    }

    // ---- answers that the handlers build ---------------------------------------------------------------------------------------------------------------------------------

    #[test]
    fn the_answers_of_the_run_routes_conform_as_the_store_and_the_handlers_make_them() {
        use crate::agent_runs::store::{status, NewRun, NewStep, RunEnd, StepOutcome};
        use crate::agent_runs::AgentRunStore;
        let world = crate::test_support::TempWorld::new("contract_runs");
        let store = AgentRunStore::open(world.path("agent_runs.db")).unwrap();
        let now = crate::agent_runs::now_ms();
        store
            .create_run(&NewRun {
                id: "run_a".into(),
                started_at: now - 50,
                goal_redacted: "tidy".into(),
                goal_full: "tidy up".into(),
                caller: "agent".into(),
                route: "agent".into(),
                parent_run_id: None,
            })
            .unwrap();
        let seq = store
            .add_step(
                "run_a",
                &NewStep {
                    step: 1,
                    kind: "tool".into(),
                    tool: "fs.read".into(),
                    args_redacted: "{\"path\":\"a.txt\"}".into(),
                    idempotency_digest: "d".into(),
                    planner_reply: "{}".into(),
                    ts: now - 40,
                },
            )
            .unwrap();
        store
            .finish_step(
                "run_a",
                seq,
                &StepOutcome {
                    ok: true,
                    status: "completed".into(),
                    observation_digest: "o".into(),
                    observation_head: "text".into(),
                    decision: "allowed".into(),
                    approval: "".into(),
                    ms: 7,
                    tokens_in: Some(5),
                    tokens_out: None,
                    cache_read: Some(1),
                    ttft_ms: None,
                    decode_tps_x10: None,
                },
            )
            .unwrap();
        store
            .end_run(
                "run_a",
                now,
                &RunEnd {
                    status: status::COMPLETED.into(),
                    stopped_reason: "final".into(),
                    rounds: 2,
                    final_answer_head: "done".into(),
                    model: "m".into(),
                    tokens_in: 10,
                    tokens_out: 4,
                    ..Default::default()
                },
            )
            .unwrap();
        let listed = store.list(10, 0, None).unwrap();
        assert!(!listed.is_empty());
        check("RunList", &json!({ "runs": listed }), true).unwrap_or_else(|e| panic!("{e}"));
        let run = store.get("run_a").unwrap().expect("the run");
        check("Run", &run, false).unwrap_or_else(|e| panic!("{e}"));
        // A page of events, as `GET /runs/{id}/events` makes it (events the store was given, read back as a client reads them).
        let started = Event::RunStarted { session_id: None, goal_head: "tidy".into(), caller: "agent".into(), route: "agent".into(), directory: None, started_ms: now };
        let finished = Event::RunFinished { status: "completed".into(), stopped_reason: "final".into(), answer_head: "done".into(), why_it_stopped: None, files_changed: 0, ms: 50 };
        store.append_event("run_a", &started).unwrap();
        store.append_event("run_a", &finished).unwrap();
        let rows = store.events_after("run_a", 0, 10).unwrap();
        assert_eq!(rows.len(), 2);
        let page = json!({ "run_id": "run_a", "events": rows.iter().map(|r| r.wire(true)).collect::<Vec<_>>(), "last": rows.last().unwrap().seq, "finished": true });
        check("RunEventsPage", &page, true).unwrap_or_else(|e| panic!("{e}"));
    }

    // ---- the routes as they are served -------------------------------------------------------------------------------------------------------------------------------------

    mod served {
        //! The route table that is served (`api_routes`), behind the real token check and the contract layer, over a state of the test's own: no model, no engine and no process is
        //! started. What the routes that need none of those answer must fit the schema the document gives them, and the refusals of the gate must carry the code of the contract.
        use super::super::super::auth::{require_local_auth, ApiAuth};
        use super::super::super::contract::{contract_layer, CONTRACT_HEADER};
        use super::super::super::server::{api_routes, AppState};
        use super::*;
        use crate::test_support::{app_state, TempWorld};
        use aqua_tool_host::{AuditLedger, Mode, PolicyEngine, ToolHost};
        use axum::http::StatusCode;
        use axum::Router;
        use std::sync::Arc;
        use tower::ServiceExt;

        const API: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const UI: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

        fn gateway(world: &TempWorld) -> (Router, AppState) {
            let host = Arc::new(ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(&world.path("host.audit.jsonl")))));
            let state = app_state(world, host);
            let auth = ApiAuth::new(API.to_string(), 8767).with_ui_token(UI.to_string());
            let app = api_routes().layer(axum::middleware::from_fn_with_state(auth, require_local_auth)).layer(axum::middleware::from_fn(contract_layer)).with_state(state.clone());
            (app, state)
        }

        struct Ask<'a> {
            method: &'a str,
            path: &'a str,
            token: Option<&'a str>,
            host: &'a str,
            origin: Option<&'a str>,
            body: Option<Value>,
        }

        fn get<'a>(path: &'a str, token: Option<&'a str>) -> Ask<'a> {
            Ask { method: "GET", path, token, host: "127.0.0.1:8767", origin: None, body: None }
        }

        async fn ask(app: &Router, a: Ask<'_>) -> (StatusCode, axum::http::HeaderMap, Value) {
            let mut request = axum::http::Request::builder().method(a.method).uri(a.path).header("host", a.host);
            if let Some(token) = a.token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            if let Some(origin) = a.origin {
                request = request.header("origin", origin);
            }
            let body = match a.body {
                Some(b) => {
                    request = request.header("content-type", "application/json");
                    axum::body::Body::from(b.to_string())
                }
                None => axum::body::Body::empty(),
            };
            let response = app.clone().oneshot(request.body(body).unwrap()).await.unwrap();
            let (status, headers) = (response.status(), response.headers().clone());
            let bytes = axum::body::to_bytes(response.into_body(), 4 << 20).await.unwrap();
            (status, headers, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
        }

        /// Runs `f` on this thread, with the test's own folder as Aqua's (nothing of the owner's `data/` is read: the handlers run on this thread).
        fn run<F: std::future::Future>(world: &TempWorld, f: F) -> F::Output {
            crate::runtime::data_tripwire::with_workspace_root(&world.root, || tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f))
        }

        /// The answer is `ok` and fits `schema`, or it is an error that fits `Error`.
        fn fits(path: &str, status: StatusCode, body: &Value, schema: &str) {
            if status.is_success() {
                check(schema, body, false).unwrap_or_else(|e| panic!("GET {path} ({status}): {e}"));
            } else {
                check("Error", body, false).unwrap_or_else(|e| panic!("GET {path} ({status}) is an error that does not fit Error: {e}"));
            }
        }

        #[test]
        fn the_routes_that_need_no_model_answer_as_their_schemas_say() {
            let world = TempWorld::new("contract_served");
            let (app, _state) = gateway(&world);
            run(&world, async {
                // The health route: with no token the little answer, with one the whole status.
                let (status, headers, body) = ask(&app, get("/api/v1/health", None)).await;
                assert_eq!(status, StatusCode::OK);
                check("Health", &body, true).unwrap_or_else(|e| panic!("{e}"));
                assert_eq!(headers.get(CONTRACT_HEADER).unwrap(), "1");
                let (status, _, body) = ask(&app, get("/api/v1/health", Some(API))).await;
                assert_eq!(status, StatusCode::OK);
                check("Status", &body, true).unwrap_or_else(|e| panic!("health with a token: {e}"));
                let (status, _, body) = ask(&app, get("/api/v1/status", Some(API))).await;
                assert_eq!(status, StatusCode::OK);
                check("Status", &body, true).unwrap_or_else(|e| panic!("status: {e}"));
                // The version route (either token).
                for token in [API, UI] {
                    let (status, _, body) = ask(&app, get("/api/v1/version", Some(token))).await;
                    assert_eq!(status, StatusCode::OK);
                    check("Version", &body, true).unwrap_or_else(|e| panic!("{e}"));
                    assert_eq!(body, super::super::super::contract::version());
                }
                // The routes of the UI token that read the core's own state.
                for (path, schema) in [("/api/v1/queue", "QueueList"), ("/api/v1/jobs", "JobsList"), ("/api/v1/flags", "FlagsList")] {
                    let (status, _, body) = ask(&app, get(path, Some(UI))).await;
                    assert_eq!(status, StatusCode::OK, "{path}: {body}");
                    check(schema, &body, false).unwrap_or_else(|e| panic!("GET {path}: {e}"));
                }
                // Routes whose answer depends on what the process has (the run database, a switch): the answer, or the refusal, fits.
                for (path, token, schema) in [
                    ("/api/v1/commands", API, "CommandsList"),
                    ("/api/v1/agents", API, "AgentsList"),
                    ("/api/v1/runs/active", API, "ActiveRuns"),
                    ("/api/v1/preapprovals", UI, "PreapprovalList"),
                    ("/api/v1/runs", UI, "RunList"),
                    ("/api/v1/runs/no_such_run_anywhere", UI, "Run"),
                    ("/api/v1/runs/no_such_run_anywhere/events", UI, "RunEventsPage"),
                    ("/api/v1/runs/no_such_run_anywhere/wait", UI, "RunWait"),
                ] {
                    let (status, _, body) = ask(&app, get(path, Some(token))).await;
                    fits(path, status, &body, schema);
                }
                // The OpenAPI document is itself served.
                let (status, _, body) = ask(&app, get("/api/v1/openapi.json", Some(API))).await;
                assert_eq!(status, StatusCode::OK);
                assert_eq!(body["info"]["x-aqua-contract"], 1);
            });
        }

        #[test]
        fn the_refusals_of_the_gate_and_of_the_handlers_carry_the_code_of_the_contract() {
            let world = TempWorld::new("contract_refusals");
            let (app, _state) = gateway(&world);
            run(&world, async {
                // No token: unauthorized, with the hint and no retry.
                let (status, headers, body) = ask(&app, get("/api/v1/status", None)).await;
                assert_eq!((status, body["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("unauthorized")), "{body}");
                check("Error", &body, true).unwrap_or_else(|e| panic!("{e}"));
                assert!(body["hint"].as_str().is_some_and(|h| !h.contains(API) && !h.contains(UI)), "a hint never holds a token: {body}");
                assert_eq!(headers.get(CONTRACT_HEADER).unwrap(), "1", "a refusal carries the header too");
                // The API token on a route of the UI token: forbidden_scope, and the sentence it always was.
                let (status, _, body) = ask(&app, get("/api/v1/queue", Some(API))).await;
                assert_eq!((status, body["code"].as_str()), (StatusCode::FORBIDDEN, Some("forbidden_scope")), "{body}");
                assert!(body["error"].as_str().unwrap().starts_with("this action needs the UI token"), "{body}");
                let (status, _, body) =
                    ask(&app, Ask { method: "POST", path: "/api/v1/runs/anything/answer", token: Some(API), host: "127.0.0.1:8767", origin: None, body: Some(json!({ "text": "yes" })) }).await;
                assert_eq!((status, body["code"].as_str()), (StatusCode::FORBIDDEN, Some("forbidden_scope")), "an answer is never given with the API token: {body}");
                // A host that is not this machine's, an origin that is not the app's.
                let (status, _, body) = ask(&app, Ask { host: "evil.example:8767", ..get("/api/v1/status", Some(API)) }).await;
                assert_eq!((status, body["code"].as_str()), (StatusCode::FORBIDDEN, Some("host_not_allowed")), "{body}");
                let (status, _, body) = ask(&app, Ask { origin: Some("http://evil.example"), ..get("/api/v1/status", Some(API)) }).await;
                assert_eq!((status, body["code"].as_str()), (StatusCode::FORBIDDEN, Some("origin_not_allowed")), "{body}");
                // A handler that builds its own error body is covered by the layer: a run that does not exist.
                let (status, _, body) = ask(&app, get("/api/v1/runs/no_such_run_anywhere", Some(UI))).await;
                let expected = if status == StatusCode::NOT_FOUND { "not_found" } else { "not_ready" };
                assert!(matches!(status, StatusCode::NOT_FOUND | StatusCode::SERVICE_UNAVAILABLE), "{status} {body}");
                assert_eq!(body["code"], expected, "{body}");
                assert_eq!(body["retryable"].as_bool(), (expected == "not_ready").then_some(true), "{body}");
                // A body that the framework refuses (not JSON) is not one of these: it has the status and no code.
                let (status, _, body) =
                    ask(&app, Ask { method: "POST", path: "/api/v1/pipeline/text", token: Some(API), host: "127.0.0.1:8767", origin: None, body: Some(json!("not an object")) }).await;
                assert!(status.is_client_error(), "{status}");
                assert!(body.get("code").is_none() || body["code"].is_string(), "{body}");
            });
        }

        /// Card OA10-04 (AG-06, WF-12): a run is continued and a plan approved or revised by the person, in the app, with the UI token: the API token is refused by every one of these routes, and so
        /// are the routes that change or delete a session, fork one, import a file or read a bundle. (The same token on the reads of sessions is served: it sees its own.)
        #[test]
        fn the_routes_that_continue_a_run_approve_a_plan_or_change_a_session_refuse_the_api_token() {
            let world = TempWorld::new("contract_oa10_04_scope");
            let (app, _state) = gateway(&world);
            run(&world, async {
                for (method, path) in [
                    ("POST", "/api/v1/runs/anything/continue"),
                    ("POST", "/api/v1/runs/anything/plan/approve"),
                    ("POST", "/api/v1/runs/anything/plan/revise"),
                    ("POST", "/api/v1/runs/import"),
                    ("GET", "/api/v1/runs/anything/bundle"),
                    ("PATCH", "/api/v1/sessions/anything"),
                    ("DELETE", "/api/v1/sessions/anything"),
                    ("POST", "/api/v1/sessions/anything/fork"),
                    ("POST", "/api/v1/sessions/import"),
                    ("GET", "/api/v1/plans"),
                    ("GET", "/api/v1/coding/sessions"),
                ] {
                    let body = (method != "GET" && method != "DELETE").then(|| json!({ "text": "yes", "title": "x" }));
                    let (status, _, answer) = ask(&app, Ask { method, path, token: Some(API), host: "127.0.0.1:8767", origin: None, body }).await;
                    assert_eq!((status, answer["code"].as_str()), (StatusCode::FORBIDDEN, Some("forbidden_scope")), "{method} {path}: {answer}");
                }
                // The reads of sessions are the API token's own: a session that is not there is `404` or `503` (no record in this test), never `403`.
                let (status, _, answer) = ask(&app, get("/api/v1/sessions/anything", Some(API))).await;
                assert_ne!(status, StatusCode::FORBIDDEN, "{answer}");
                let (status, _, answer) = ask(&app, get("/api/v1/sessions", Some(API))).await;
                assert_ne!(status, StatusCode::FORBIDDEN, "{answer}");
            });
        }
    }

    // ---- the errors --------------------------------------------------------------------------------------------------------------------------------------------------------------

    #[test]
    fn the_error_bodies_the_contract_builds_conform_to_the_error_schema_for_every_code() {
        use super::super::contract::{api_error_code, error_body};
        for code in ErrorCode::ALL {
            check("Error", &error_body(code, "a sentence", Some("a hint")), true).unwrap_or_else(|e| panic!("{}: {e}", code.as_str()));
            check("Error", &error_body(code, "a sentence", None), true).unwrap_or_else(|e| panic!("{}: {e}", code.as_str()));
        }
        let response = api_error_code(axum::http::StatusCode::CONFLICT, ErrorCode::Cancelled, "stopped", None);
        assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    }
}
