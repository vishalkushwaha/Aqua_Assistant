//! The JSON schema a planner reply is held to, built from the tools the planner is shown.
//!
//! The model server turns the schema into a grammar and lets the model pick only tokens that keep the reply valid, so the reply can
//! name only a shown tool and give only that tool's arguments. A reply is one of:
//!
//! * `{"tool": "<shown tool>", "arguments": {...}}`: one call (the form the prompt teaches);
//! * `{"actions": [{"tool", "arguments"}, ...]}`: up to a few calls in one reply (the loop runs them in order and stops at the first
//!   that fails or changes what the next one was written for);
//! * `{"final": "..."}` or `{"ask": "...", "choices"?: [...]}`.
//!
//! Argument schemas come from each tool's `input_schema`, reduced to the part a grammar can enforce: types, enums, required names,
//! array items. Anything it cannot (a `$ref`, a union) becomes "any JSON value" and the tool host's own validation decides, as it always
//! did. Descriptions and defaults are dropped: they are for the prompt, and every character of schema is grammar the server has to track.

use aqua_mcp_adapters::McpTool;
use serde_json::{json, Map, Value};

/// How much of the planner's reply is enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Nothing: free text read with the tolerant parser (the behaviour before this existed).
    Off,
    /// The reply is JSON of the right shape and names a shown tool; the arguments are any JSON object.
    Names,
    /// Names, and the arguments of each tool fit that tool's schema.
    Schema,
}

impl Mode {
    pub fn is_on(self) -> bool {
        self != Mode::Off
    }
}

/// `AQUA_PLANNER_CONSTRAINT=names|schema|off`. The default is `names`: the reply is JSON of the right shape that names a shown tool. Measured
/// on the local 8B model (`scripts/bench/planner_constraint.py`, report in `scripts/bench/results/`): as accurate as free text on every
/// goal tried, no slower to decode, and it cannot name a tool that was not shown. `schema` (the arguments of each tool are held to its schema
/// as well) decoded about a fifth slower on a list of 36 tools, so it is opt-in. A server that refuses the constraint is asked again without it.
pub fn mode() -> Mode {
    mode_from(std::env::var("AQUA_PLANNER_CONSTRAINT").ok().as_deref())
}

pub fn mode_from(value: Option<&str>) -> Mode {
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("schema" | "full" | "args") => Mode::Schema,
        Some("off" | "0" | "false" | "none") => Mode::Off,
        _ => Mode::Names,
    }
}

/// The most calls one reply may carry (`AQUA_PLANNER_BATCH`, default 1: one call a reply, as before; at most 3). A reply that carries
/// several is run call by call and cut short at the first that fails or changes the page. Only used while the reply is constrained.
pub fn max_batch() -> usize {
    std::env::var("AQUA_PLANNER_BATCH").ok().and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(1).clamp(1, 3)
}

/// May the planner ask the person a question instead of guessing (`AQUA_ASK=on`)? Off until the follow-up tasks pass on the real model.
pub fn ask_enabled() -> bool {
    matches!(std::env::var("AQUA_ASK").ok().as_deref().map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("on" | "1" | "true"))
}

/// Does the planner write a short `thought` before its action? (`AQUA_PLANNER_THOUGHT=1`.) Off by default: every character the model
/// writes costs about 15 ms here, and the small model does not need a scratchpad to pick a tool.
pub fn thought_enabled() -> bool {
    matches!(std::env::var("AQUA_PLANNER_THOUGHT").ok().as_deref().map(str::trim), Some("1" | "on" | "true"))
}

const MAX_DEPTH: usize = 4;

/// One argument's schema, reduced to what a grammar can enforce.
pub fn value_schema(v: &Value, depth: usize) -> Value {
    let Some(obj) = v.as_object() else { return json!({}) };
    if depth >= MAX_DEPTH || ["$ref", "anyOf", "oneOf", "allOf", "not", "if"].iter().any(|k| obj.contains_key(*k)) {
        return json!({});
    }
    if let Some(c) = obj.get("const") {
        return json!({ "const": c });
    }
    if let Some(e) = obj.get("enum").and_then(Value::as_array).filter(|e| !e.is_empty() && e.len() <= 64) {
        return json!({ "enum": e });
    }
    let simple = |t: &str| matches!(t, "string" | "integer" | "number" | "boolean" | "null");
    match obj.get("type") {
        Some(Value::String(t)) if simple(t) => json!({ "type": t }),
        Some(Value::String(t)) if t == "array" => {
            let mut a = Map::new();
            a.insert("type".into(), json!("array"));
            a.insert("items".into(), obj.get("items").map(|i| value_schema(i, depth + 1)).unwrap_or_else(|| json!({})));
            for key in ["minItems", "maxItems"] {
                if let Some(n) = obj.get(key).filter(|n| n.is_u64()) {
                    a.insert(key.into(), n.clone());
                }
            }
            Value::Object(a)
        }
        Some(Value::String(t)) if t == "object" => object_schema(obj, depth),
        // ["string", "null"] and the like.
        Some(Value::Array(ts)) if !ts.is_empty() && ts.iter().all(|t| t.as_str().is_some_and(simple)) => json!({ "type": ts }),
        None if obj.contains_key("properties") => object_schema(obj, depth),
        _ => json!({}),
    }
}

fn object_schema(obj: &Map<String, Value>, depth: usize) -> Value {
    let Some(props) = obj.get("properties").and_then(Value::as_object).filter(|p| !p.is_empty()) else {
        // An object with no named properties takes whatever it is given.
        return json!({ "type": "object" });
    };
    let properties: Map<String, Value> = props.iter().map(|(k, v)| (k.clone(), value_schema(v, depth + 1))).collect();
    let required: Vec<&str> = obj.get("required").and_then(Value::as_array).map(|r| r.iter().filter_map(Value::as_str).filter(|k| properties.contains_key(*k)).collect()).unwrap_or_default();
    let mut o = Map::new();
    o.insert("type".into(), json!("object"));
    o.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        o.insert("required".into(), json!(required));
    }
    // The server must not invent arguments the tool does not have.
    o.insert("additionalProperties".into(), json!(false));
    Value::Object(o)
}

/// The schema of the `arguments` of one tool.
pub fn argument_schema(tool: &McpTool) -> Value {
    match value_schema(&tool.input_schema, 0) {
        v if v.get("type") == Some(&json!("object")) => v,
        // No schema, or one that is not an object schema: any object.
        _ => json!({ "type": "object" }),
    }
}

/// The parts of a reply beyond a call or an answer that are switched on.
#[derive(Debug, Clone, Copy, Default)]
pub struct Extras {
    /// A short `thought` before the action.
    pub thought: bool,
    /// `{"ask": ...}`: a question for the person.
    pub ask: bool,
    /// `{"recall": "hN"}`: ask for an earlier result again (the context manager).
    pub recall: bool,
    /// An optional `plan` list beside a call.
    pub plan: bool,
}

/// The `plan` a call may carry: up to five short steps.
fn plan_schema() -> Value {
    json!({ "type": "array", "items": { "type": "string", "maxLength": 80 }, "minItems": 1, "maxItems": 5 })
}

fn call_schema(tools: &[McpTool], mode: Mode, with_plan: bool) -> Value {
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    let with_plan_prop = |mut props: Value| {
        if with_plan {
            props["plan"] = plan_schema();
        }
        props
    };
    if mode == Mode::Names {
        return json!({
            "type": "object",
            "properties": with_plan_prop(json!({ "tool": { "enum": names }, "arguments": { "type": "object" } })),
            "required": ["tool", "arguments"],
            "additionalProperties": false
        });
    }
    let alternatives: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "type": "object",
                "properties": with_plan_prop(json!({ "tool": { "const": t.name }, "arguments": argument_schema(t) })),
                "required": ["tool", "arguments"],
                "additionalProperties": false
            })
        })
        .collect();
    json!({ "oneOf": alternatives })
}

/// The schema of one planner reply for `tools`. `batch` is the most calls one reply may carry (1: no `actions` form).
/// With no tools shown the reply can only be an answer or a question.
pub fn action_schema(tools: &[McpTool], mode: Mode, batch: usize, with_thought: bool, with_ask: bool) -> Value {
    action_schema_with(tools, mode, batch, Extras { thought: with_thought, ask: with_ask, ..Extras::default() })
}

/// [`action_schema`] with the parts of a reply that are switched on.
pub fn action_schema_with(tools: &[McpTool], mode: Mode, batch: usize, extras: Extras) -> Value {
    let (with_thought, with_ask) = (extras.thought, extras.ask);
    let thought = |mut props: Map<String, Value>| {
        if with_thought {
            let mut with = Map::new();
            with.insert("thought".into(), json!({ "type": "string", "maxLength": 240 }));
            with.extend(props);
            props = with;
        }
        props
    };
    let object = |props: Map<String, Value>, required: &[&str]| json!({ "type": "object", "properties": thought(props), "required": required, "additionalProperties": false });
    let mut alternatives: Vec<Value> = Vec::new();
    let mut root = Map::new();
    if !tools.is_empty() {
        root.insert("$defs".into(), json!({ "call": call_schema(tools, mode, extras.plan) }));
        alternatives.push(json!({ "$ref": "#/$defs/call" }));
        if batch >= 2 {
            let mut p = Map::new();
            p.insert("actions".into(), json!({ "type": "array", "items": { "$ref": "#/$defs/call" }, "minItems": 1, "maxItems": batch.min(3) }));
            alternatives.push(object(p, &["actions"]));
        }
    }
    let mut p = Map::new();
    p.insert("final".into(), json!({ "type": "string" }));
    alternatives.push(object(p, &["final"]));
    if extras.recall {
        let mut p = Map::new();
        p.insert("recall".into(), json!({ "type": "string", "maxLength": 6 }));
        alternatives.push(object(p, &["recall"]));
    }
    if with_ask {
        let mut p = Map::new();
        p.insert("ask".into(), json!({ "type": "string" }));
        p.insert("choices".into(), json!({ "type": "array", "items": { "type": "string" }, "maxItems": 4 }));
        alternatives.push(object(p, &["ask"]));
    }
    root.insert("oneOf".into(), Value::Array(alternatives));
    Value::Object(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, schema: Value) -> McpTool {
        McpTool { name: name.into(), description: "does it".into(), input_schema: schema, source: Some("host".into()) }
    }

    fn read() -> McpTool {
        tool("fs.read", json!({"type": "object", "properties": {"path": {"type": "string", "description": "the file"}, "max_lines": {"type": "integer", "minimum": 1, "default": 200}}, "required": ["path"]}))
    }

    #[test]
    fn a_tools_arguments_keep_what_a_grammar_can_enforce_and_lose_the_rest() {
        let a = argument_schema(&read());
        assert_eq!(
            a,
            json!({"type": "object", "properties": {"path": {"type": "string"}, "max_lines": {"type": "integer"}}, "required": ["path"], "additionalProperties": false}),
            "descriptions, defaults and minimums are dropped; the names and types stay"
        );
        // No schema, no properties, or something that is not an object: any object.
        for s in [json!(null), json!({}), json!({"type": "object"}), json!({"type": "string"}), json!({"type": "object", "properties": {}})] {
            assert_eq!(argument_schema(&tool("x", s.clone())), json!({"type": "object"}), "{s}");
        }
    }

    /// Card OA5-03 (TL-03): the keywords `x-clamp` and `x-aliases` of a native tool's schema are for the host's validation; the grammar the model is held to has neither, so it is no
    /// wider for them (the other spellings are for a model that is not held to a grammar) and no narrower.
    #[test]
    fn the_x_keywords_of_a_schema_are_not_part_of_the_grammar() {
        let with = tool("fs.read", json!({"type": "object", "properties": {
            "path": {"type": "string", "x-aliases": ["file_path", "file"]},
            "offset_line": {"type": "integer", "minimum": 1, "x-clamp": true, "x-aliases": ["offset"]}
        }, "required": ["path"]}));
        let without = tool("fs.read", json!({"type": "object", "properties": {"path": {"type": "string"}, "offset_line": {"type": "integer", "minimum": 1}}, "required": ["path"]}));
        assert_eq!(argument_schema(&with), argument_schema(&without));
        assert!(!argument_schema(&with).to_string().contains("x-") && !argument_schema(&with).to_string().contains("file_path"));
    }

    #[test]
    fn what_a_grammar_cannot_express_becomes_any_value_and_the_host_still_validates() {
        let t = tool(
            "x",
            json!({"type": "object", "properties": {
                "a": {"anyOf": [{"type": "string"}, {"type": "integer"}]},
                "b": {"$ref": "#/defs/b"},
                "c": {"type": ["string", "null"]},
                "d": {"enum": ["asc", "desc"]},
                "e": {"type": "array", "items": {"type": "string"}, "maxItems": 5},
                "f": {"type": "object", "properties": {"k": {"type": "boolean"}}, "required": ["k", "ghost"]},
                "g": {"type": "wat"}
            }, "required": ["a"]}),
        );
        let p = argument_schema(&t)["properties"].clone();
        assert_eq!(p["a"], json!({}));
        assert_eq!(p["b"], json!({}));
        assert_eq!(p["c"], json!({"type": ["string", "null"]}));
        assert_eq!(p["d"], json!({"enum": ["asc", "desc"]}));
        assert_eq!(p["e"], json!({"type": "array", "items": {"type": "string"}, "maxItems": 5}));
        assert_eq!(p["f"]["required"], json!(["k"]), "a required name that is not a property cannot be demanded");
        assert_eq!(p["g"], json!({}));
    }

    #[test]
    fn deeply_nested_schemas_stop_being_followed() {
        let mut v = json!({"type": "string"});
        for _ in 0..8 {
            v = json!({"type": "object", "properties": {"n": v}});
        }
        let s = value_schema(&v, 0).to_string();
        assert!(s.matches("\"properties\"").count() <= MAX_DEPTH, "{s}");
    }

    #[test]
    fn a_reply_is_a_call_a_batch_an_answer_or_a_question_and_names_only_shown_tools() {
        let tools = vec![read(), tool("fs.edit", json!({"type": "object", "properties": {"path": {"type": "string"}, "old": {"type": "string"}, "new": {"type": "string"}}, "required": ["path", "old", "new"]}))];
        let s = action_schema(&tools, Mode::Schema, 2, false, true);
        let call = &s["$defs"]["call"]["oneOf"];
        assert_eq!(call.as_array().unwrap().len(), 2);
        assert_eq!(call[0]["properties"]["tool"], json!({"const": "fs.read"}));
        assert_eq!(call[1]["properties"]["arguments"]["required"], json!(["path", "old", "new"]));
        let top = s["oneOf"].as_array().unwrap();
        assert_eq!(top.len(), 4, "call, actions, final, ask");
        // Without the question form (the default), and without the batch form (one call a reply): just a call and an answer.
        let plain = action_schema(&tools, Mode::Names, 1, false, false);
        assert_eq!(plain["oneOf"].as_array().unwrap().len(), 2, "{plain}");
        assert_eq!(top[0], json!({"$ref": "#/$defs/call"}));
        assert_eq!(top[1]["properties"]["actions"]["maxItems"], 2);
        assert_eq!(top[2]["required"], json!(["final"]));
        assert_eq!(top[3]["required"], json!(["ask"]));
        // Names only: one enum, any object for the arguments, so the grammar stays small.
        let n = action_schema(&tools, Mode::Names, 1, false, true);
        assert_eq!(n["$defs"]["call"]["properties"]["tool"], json!({"enum": ["fs.read", "fs.edit"]}));
        assert_eq!(n["oneOf"].as_array().unwrap().len(), 3, "no batch form when the batch is one call");
        // Nothing shown: the reply can only be an answer or a question, and there are no definitions to refer to.
        let none = action_schema(&[], Mode::Schema, 2, false, true);
        assert!(none.get("$defs").is_none());
        assert_eq!(none["oneOf"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn the_thought_is_optional_short_and_only_there_when_asked_for() {
        let tools = vec![read()];
        let with = action_schema(&tools, Mode::Schema, 2, true, true);
        for alt in &with["oneOf"].as_array().unwrap()[1..] {
            assert_eq!(alt["properties"]["thought"], json!({"type": "string", "maxLength": 240}));
            assert!(!alt["required"].as_array().unwrap().iter().any(|r| r == "thought"), "optional");
        }
        let without = action_schema(&tools, Mode::Schema, 2, false, true);
        assert!(!without.to_string().contains("thought"));
    }

    #[test]
    fn the_settings_are_read_from_the_environment_values_and_default_to_names() {
        assert_eq!(mode_from(None), Mode::Names, "names is the default");
        assert_eq!(mode_from(Some("garbage")), Mode::Names);
        assert_eq!(mode_from(Some("off")), Mode::Off);
        assert_eq!(mode_from(Some(" OFF ")), Mode::Off);
        assert_eq!(mode_from(Some("Schema")), Mode::Schema);
        assert_eq!(mode_from(Some(" names ")), Mode::Names);
        assert!(Mode::Schema.is_on() && Mode::Names.is_on() && !Mode::Off.is_on());
    }

    /// The tools of the real tool host: every manifest's schema must become a schema (never panic, never an invalid shape), and the whole
    /// reply schema must stay small enough for a grammar a small model can follow without slowing down.
    #[test]
    fn every_tool_the_host_registers_gets_a_schema_and_the_whole_thing_stays_small() {
        use aqua_mcp_adapters::browser::BrowserBackend;
        use aqua_mcp_adapters::{new_browser_client, new_windows_client};
        use std::sync::{Arc, Mutex};
        let root = std::env::temp_dir().join(format!("aqua_planner_schema_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let browser = Arc::new(BrowserBackend::new(&root.join("browser"), new_browser_client()));
        let host = crate::tool_host::build_tool_host(&data, browser, Arc::new(Mutex::new(new_windows_client())));
        let manifests = host.manifests();
        assert!(manifests.len() > 100, "the whole host is registered: {}", manifests.len());
        let tools: Vec<McpTool> = manifests.iter().map(|m| tool(&m.name, m.input_schema.clone())).collect();
        for t in &tools {
            let a = argument_schema(t);
            assert_eq!(a["type"], "object", "{}: {a}", t.name);
        }
        // One pack's worth, the way the planner is shown tools: at most a dozen.
        let shown: Vec<McpTool> = tools.iter().filter(|t| t.name.starts_with("fs.") || t.name.starts_with("term.")).take(12).cloned().collect();
        let bytes = action_schema(&shown, Mode::Schema, 2, false, true).to_string().len();
        assert!(bytes < 6_000, "twelve tools make a {bytes} byte schema");
        let names = action_schema(&shown, Mode::Names, 2, false, true).to_string().len();
        assert!(names < bytes, "the names-only form is the smaller one ({names} < {bytes})");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Writes the prompts and schemas of a few real goals for `scripts/bench/planner_constraint.py`, which replays them against a model
    /// server. Run it by hand: `AQUA_PROBE_OUT=<file> cargo test -p aqua-core --lib planner_probe -- --ignored`.
    #[test]
    #[ignore = "writes a file for the planner experiment"]
    fn planner_probe_writes_real_prompts_and_schemas() {
        use aqua_mcp_adapters::browser::BrowserBackend;
        use aqua_mcp_adapters::{new_browser_client, new_windows_client};
        use std::sync::{Arc, Mutex};
        let out = std::env::var("AQUA_PROBE_OUT").expect("set AQUA_PROBE_OUT to the file to write");
        // The most calls one reply may carry in the schemas: 2 as before, or what AQUA_PLANNER_BATCH says (1 to 3) when it is set, so that `scripts/bench/planner_constraint.py` can be given one file
        // for each condition of the tool-call measurement (card OA2-03, TL-04). AQUA_TOOL_SEARCH=on changes the tools shown through `tools_for_goal` as it does in the program.
        let batch = if std::env::var_os("AQUA_PLANNER_BATCH").is_some() { max_batch() } else { 2 };
        let root = std::env::temp_dir().join(format!("aqua_planner_probe_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let browser = Arc::new(BrowserBackend::new(&root.join("browser"), new_browser_client()));
        let host = crate::tool_host::build_tool_host(&data, browser, Arc::new(Mutex::new(new_windows_client())));
        let goals = [
            "fix the failing test in rust_probe",
            "read the files notes.txt and todo.txt in my workspace and tell me what each says",
            "create a file called plan.txt in my workspace with the text hello",
            "show the git status and the last three commits of the repo",
            "list the files in my workspace folder",
            "find the word TODO in the files of my project folder",
        ];
        let mut cases = Vec::new();
        for goal in goals {
            let shown = crate::modules::browser_agent::in_prompt_order(crate::agent_tools::tools_for_goal(&host, goal));
            let system = crate::modules::browser_agent::planner_prompt_block(goal, &shown);
            cases.push(serde_json::json!({
                "goal": goal,
                "system": system,
                "tools": shown.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
                "schema_names": action_schema(&shown, Mode::Names, batch, false, true),
                "schema_full": action_schema(&shown, Mode::Schema, batch, false, true),
            }));
        }
        // Desktop and browser goals: the tools the desktop server really lists (a fixture of the pinned version), the two local tools, and the
        // browser tools under their host names (the live browser server's own list is not available offline; its schemas are of this size).
        let fixture = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/windows_mcp/tools_list.json")).unwrap();
        let list: Value = serde_json::from_str(&fixture).unwrap();
        let windows: Vec<McpTool> = list
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .or_else(|| list.as_array().cloned())
            .unwrap()
            .iter()
            .map(|t| McpTool { name: t["name"].as_str().unwrap().into(), description: t["description"].as_str().unwrap_or("").chars().take(80).collect(), input_schema: t.get("inputSchema").or_else(|| t.get("input_schema")).cloned().unwrap_or(Value::Null), source: Some("windows".into()) })
            .collect();
        let local = vec![
            McpTool { name: "local_find".into(), description: "Find a program, game or file on this computer by name.".into(), input_schema: json!({"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}), source: Some("local".into()) },
            McpTool { name: "local_open".into(), description: "Open one result of the last search.".into(), input_schema: json!({"type": "object", "properties": {"target": {"type": "string"}}, "required": ["target"]}), source: Some("local".into()) },
        ];
        let browser_tools: Vec<McpTool> = host
            .manifests()
            .iter()
            .filter(|m| m.name.starts_with("browser."))
            .map(|m| McpTool { name: m.name.clone(), description: m.description.chars().take(80).collect(), input_schema: m.input_schema.clone(), source: Some("browser".into()) })
            .collect();
        let mixed = |goal: &str, tools: Vec<McpTool>| {
            let shown = crate::modules::browser_agent::in_prompt_order(tools);
            json!({
                "goal": goal,
                "system": crate::modules::browser_agent::planner_prompt_block(goal, &shown),
                "tools": shown.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
                "schema_names": action_schema(&shown, Mode::Names, batch, false, true),
                "schema_full": action_schema(&shown, Mode::Schema, batch, false, true),
            })
        };
        cases.push(mixed("open notepad and type hello world", windows.iter().cloned().chain(local.iter().cloned()).collect()));
        cases.push(mixed("what are the latest headlines on the BBC news site", browser_tools.iter().cloned().chain(windows.iter().cloned()).chain(local.iter().cloned()).collect()));
        std::fs::write(&out, serde_json::to_string_pretty(&cases).unwrap()).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_desktop_servers_own_tool_list_converts_too() {
        // The list Windows-MCP really sends (a fixture taken from the pinned version): unions and defaults included.
        let text = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/windows_mcp/tools_list.json")).unwrap();
        let list: Value = serde_json::from_str(&text).unwrap();
        let tools = list.get("tools").and_then(Value::as_array).cloned().or_else(|| list.as_array().cloned()).expect("a list of tools");
        assert!(tools.len() >= 10, "{}", tools.len());
        let mcp: Vec<McpTool> = tools
            .iter()
            .map(|t| McpTool { name: t["name"].as_str().unwrap().into(), description: String::new(), input_schema: t.get("inputSchema").or_else(|| t.get("input_schema")).cloned().unwrap_or(Value::Null), source: Some("windows".into()) })
            .collect();
        let s = action_schema(&mcp, Mode::Schema, 2, false, true);
        assert!(s["$defs"]["call"]["oneOf"].as_array().unwrap().len() == mcp.len());
        let bytes = s.to_string().len();
        assert!(bytes < 12_000, "{} desktop tools make a {bytes} byte schema", mcp.len());
    }

    #[test]
    fn a_reply_may_carry_a_plan_beside_a_call_and_ask_for_an_earlier_result_only_when_those_are_on() {
        let tools = vec![read()];
        let plain = action_schema_with(&tools, Mode::Names, 1, Extras::default());
        assert!(plain["$defs"]["call"]["properties"].get("plan").is_none());
        assert_eq!(plain["oneOf"].as_array().unwrap().len(), 2, "a call and an answer");
        let on = action_schema_with(&tools, Mode::Names, 1, Extras { plan: true, recall: true, ..Extras::default() });
        let plan = &on["$defs"]["call"]["properties"]["plan"];
        assert_eq!((plan["maxItems"].clone(), plan["items"]["maxLength"].clone()), (json!(5), json!(80)), "{plan}");
        assert!(!on["$defs"]["call"]["required"].as_array().unwrap().contains(&json!("plan")), "a plan is optional");
        let top = on["oneOf"].as_array().unwrap();
        assert!(top.iter().any(|a| a["required"] == json!(["recall"]) && a["properties"]["recall"]["type"] == "string"), "{top:?}");
        // The same for each tool's own alternative in the full-schema mode.
        let full = action_schema_with(&tools, Mode::Schema, 1, Extras { plan: true, ..Extras::default() });
        assert!(full["$defs"]["call"]["oneOf"][0]["properties"].get("plan").is_some());
        // The old five-argument form is the same as the new one with the same choices.
        assert_eq!(action_schema(&tools, Mode::Names, 2, true, true), action_schema_with(&tools, Mode::Names, 2, Extras { thought: true, ask: true, ..Extras::default() }));
    }
}

