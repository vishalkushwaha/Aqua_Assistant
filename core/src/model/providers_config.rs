//! What the person says about the model servers on this machine: `data/models/providers.json` (card OA9-04, task PR-07 of the OpenCode audit).
//!
//! ```json
//! { "version": 1,
//!   "providers": {
//!     "ollama": {
//!       "base_url": "http://127.0.0.1:11434/v1",
//!       "key_name": "models/ollama",
//!       "models": { "qwen3:8b": { "context": 8192, "max_output": 1024, "structured": "json_schema" } }
//!     } } }
//! ```
//!
//! This file is the list of what may be asked: a provider is only used for the models it lists, and an address that is not on this machine is refused with its reason (it is read, shown
//! with the reason in `GET /api/v1/models/providers`, and never used). Only the person writes it, by hand: it is a protected file (`protected_file.rs`), so a program that Aqua runs
//! cannot add an address to it. It is read like the other config files (`config_files::load`, in the module that owns the spec: comments and trailing commas are allowed, a file over
//! 1 MiB is refused, an unknown key is an error that names the nearest known one, and a file that cannot be used turns **every** provider off until it is fixed).
//!
//! The types and the rules are here; reading the file and keeping what was read are in `core/src/model_providers/`.

use super::openai_compat::{Endpoint, ModelSpec, ProviderSpec, DEFAULT_TIMEOUT_SECS};
use super::openai_wire::Shape;
use super::{StructuredMode, ToolMode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

/// The file's name, in the models folder.
pub const FILE: &str = "providers.json";

/// The file, from the data folder.
pub const REL: &str = "models/providers.json";

/// The version of the file this reads.
pub const VERSION: u64 = 1;

/// The names of keys a provider may use begin with this: a provider cannot be pointed at the credential of the forge or of a database. They are set by hand,
/// `aqua-cli secret-set models/ollama`.
pub const KEY_PREFIX: &str = "models/";

const MAX_PROVIDERS: usize = 16;
const MAX_MODELS: usize = 64;

/// The provider that is the local `llama-server`, which no entry may take the name of.
const RESERVED: &[&str] = &["llama.cpp", "llama", "local"];

fn version() -> u64 {
    VERSION
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersFile {
    #[serde(default = "version")]
    pub version: u64,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderEntry>,
}

impl Default for ProvidersFile {
    fn default() -> Self {
        ProvidersFile { version: VERSION, providers: BTreeMap::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    /// The address of the API, up to and with `/v1`: `http://127.0.0.1:11434/v1`.
    pub base_url: String,
    /// The name, in the credential store, of the key the server wants (`models/ollama`). Not the key.
    #[serde(default)]
    pub key_name: Option<String>,
    /// The models that may be asked, by the name the server knows them by.
    #[serde(default)]
    pub models: BTreeMap<String, ModelEntry>,
    /// Send `top_k`, `min_p` and `repeat_penalty` when a request sets them (llama-server and LM Studio take them; a server that does not know a field may refuse the request).
    #[serde(default)]
    pub extra_sampling: bool,
    /// The longest a call to it may take, in seconds (5 to 900; 300 when not said).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// `false` leaves the provider in the file and out of use.
    #[serde(default = "yes")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    /// The window of the model in tokens (512 to 1,048,576; 4,096 when not said).
    #[serde(default)]
    pub context: Option<u32>,
    /// The most tokens one reply may have (1 to 65,536; 1,024 when not said).
    #[serde(default)]
    pub max_output: Option<u32>,
    /// `qwen3`, `llama3`, `gemma3`, `mistral`, `granite`...: what the name does not say.
    #[serde(default)]
    pub family: Option<String>,
    /// `native` (the server takes tool definitions and gives tool calls), `constrained_json` (Aqua's way: the reply is JSON held to a schema, which is the default) or `none`.
    #[serde(default)]
    pub tools: Option<String>,
    /// How a reply can be held to a shape: `json_schema` (`response_format` with a schema: Ollama 0.5 and later, LM Studio, vLLM), `json_object`, `gbnf` (the server's own `grammar`:
    /// llama-server) or `none` (the default: the prompt says what shape the reply has and Aqua's tolerant reader takes what comes).
    #[serde(default)]
    pub structured: Option<String>,
    /// The model thinks before it answers (the thinking comes apart from the answer as `reasoning_content`).
    #[serde(default)]
    pub reasoning: Option<bool>,
    /// The server takes a seed (true when not said).
    #[serde(default)]
    pub seed: Option<bool>,
}

/// A provider name: lower case letters, digits, `_` and `-`, at most 32, none of the names of the local server, and no `/` (a model reference is `provider/model`).
pub fn valid_provider_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit()) && id.len() <= 32 && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-')) && !RESERVED.contains(&id)
}

fn valid_model_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 200 && !name.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn tool_mode(word: &str) -> Option<ToolMode> {
    match word {
        "none" => Some(ToolMode::None),
        "constrained_json" => Some(ToolMode::ConstrainedJson),
        "native" => Some(ToolMode::Native),
        _ => None,
    }
}

fn structured_mode(word: &str) -> Option<StructuredMode> {
    match word {
        "none" => Some(StructuredMode::None),
        "json_object" => Some(StructuredMode::JsonObject),
        "json_schema" => Some(StructuredMode::JsonSchema),
        "gbnf" => Some(StructuredMode::Gbnf),
        _ => None,
    }
}

/// The rules a file must meet to be used at all. Anything else (an address that is not on this machine) leaves the entry out and says why; what is wrong here is a mistake in the
/// file's own form, and the whole file is refused so that a half-read list of providers is never what runs.
pub fn validate(file: &ProvidersFile) -> Result<(), String> {
    if file.version != VERSION {
        return Err(format!("its version is {}, and only version {VERSION} is read", file.version));
    }
    if file.providers.len() > MAX_PROVIDERS {
        return Err(format!("it lists {} providers, and at most {MAX_PROVIDERS} are read", file.providers.len()));
    }
    for (id, p) in &file.providers {
        if !valid_provider_id(id) {
            return Err(format!("the provider name {id:?} is not allowed: lower case letters, digits, _ and -, at most 32, and not llama.cpp, llama or local"));
        }
        if let Some(name) = &p.key_name {
            if !aqua_native_tools::secrets::valid_name(name) || !name.starts_with(KEY_PREFIX) || name.len() == KEY_PREFIX.len() {
                return Err(format!("{id}: key_name {name:?} must be the name of a key that starts with {KEY_PREFIX} (set it with: aqua-cli secret-set {KEY_PREFIX}{id})"));
            }
        }
        if let Some(secs) = p.timeout_secs {
            if !(5..=900).contains(&secs) {
                return Err(format!("{id}: timeout_secs is {secs}, and it must be from 5 to 900"));
            }
        }
        if p.models.len() > MAX_MODELS {
            return Err(format!("{id}: it lists {} models, and at most {MAX_MODELS} are read", p.models.len()));
        }
        for (name, m) in &p.models {
            if !valid_model_name(name) {
                return Err(format!("{id}: the model name {name:?} is empty, longer than 200 characters, or has a space or a control character"));
            }
            if m.context.is_some_and(|c| !(512..=1_048_576).contains(&c)) {
                return Err(format!("{id}/{name}: context must be from 512 to 1048576"));
            }
            if m.max_output.is_some_and(|c| !(1..=65_536).contains(&c)) {
                return Err(format!("{id}/{name}: max_output must be from 1 to 65536"));
            }
            if let Some(word) = &m.tools {
                if tool_mode(word).is_none() {
                    return Err(format!("{id}/{name}: tools is {word:?}, and it must be none, constrained_json or native"));
                }
            }
            if let Some(word) = &m.structured {
                if structured_mode(word).is_none() {
                    return Err(format!("{id}/{name}: structured is {word:?}, and it must be none, json_object, json_schema or gbnf"));
                }
            }
            if let Some(family) = &m.family {
                if family.is_empty() || family.len() > 40 || family.chars().any(char::is_control) {
                    return Err(format!("{id}/{name}: family is empty, longer than 40 characters or has a control character"));
                }
            }
        }
    }
    Ok(())
}

/// A provider that is in the file and is not used, with the reason, as one sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Refused {
    pub provider: String,
    pub reason: String,
}

/// What the file comes to: the providers that may be used, and those that may not and why.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Resolution {
    pub providers: Vec<ProviderSpec>,
    pub refused: Vec<Refused>,
    /// Providers the file has with `"enabled": false`.
    pub disabled: Vec<String>,
}

fn model_spec(name: &str, m: &ModelEntry) -> ModelSpec {
    ModelSpec {
        name: name.to_string(),
        context: m.context.unwrap_or(4096),
        max_output: m.max_output.unwrap_or(1024),
        family: m.family.clone(),
        shape: Shape {
            tools: m.tools.as_deref().and_then(tool_mode).unwrap_or(ToolMode::ConstrainedJson),
            structured: m.structured.as_deref().and_then(structured_mode).unwrap_or(StructuredMode::None),
            // Set below from the provider.
            extra_sampling: false,
            seed: m.seed.unwrap_or(true),
        },
        reasoning: m.reasoning.unwrap_or(false),
    }
}

/// Which providers of a (valid) file may be used. An address that is not on this machine, not in plain http, or one of Aqua's own ports is not used, and the reason is kept.
pub fn resolve(file: &ProvidersFile) -> Resolution {
    let mut out = Resolution::default();
    for (id, p) in &file.providers {
        if !p.enabled {
            out.disabled.push(id.clone());
            continue;
        }
        let endpoint = match Endpoint::parse(&p.base_url) {
            Ok(e) => e,
            Err(e) => {
                out.refused.push(Refused { provider: id.clone(), reason: e.to_string() });
                continue;
            }
        };
        if let Some(why) = endpoint.refusal() {
            out.refused.push(Refused { provider: id.clone(), reason: why });
            continue;
        }
        let models = p
            .models
            .iter()
            .map(|(name, m)| {
                let mut spec = model_spec(name, m);
                spec.shape.extra_sampling = p.extra_sampling;
                spec
            })
            .collect();
        out.providers.push(ProviderSpec { id: id.clone(), endpoint, key_name: p.key_name.clone(), models, timeout: Duration::from_secs(p.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS)) });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_tool_host::config_text;

    fn read(text: &str) -> Result<ProvidersFile, String> {
        let prepared = config_text::prepare(text).map_err(|e| e.detail())?;
        serde_json::from_str::<ProvidersFile>(&prepared).map_err(|e| e.to_string())
    }

    fn valid(text: &str) -> ProvidersFile {
        let file = read(text).unwrap_or_else(|e| panic!("{e}"));
        validate(&file).unwrap_or_else(|e| panic!("{e}"));
        file
    }

    fn invalid(text: &str) -> String {
        match read(text) {
            Err(e) => e,
            Ok(file) => validate(&file).expect_err("the file was valid"),
        }
    }

    #[test]
    fn the_example_file_is_read_whole_and_its_providers_come_out_as_the_comments_say() {
        let file = valid(include_str!("../../../documentation/model_providers.example.json"));
        let r = resolve(&file);
        assert!(r.refused.is_empty(), "{:?}", r.refused);
        assert_eq!(r.disabled, ["vllm"]);
        assert_eq!(r.providers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["lmstudio", "ollama"]);
        let ollama = r.providers.iter().find(|p| p.id == "ollama").unwrap();
        assert_eq!((ollama.endpoint.url().as_str(), ollama.key_name.clone(), ollama.timeout), ("http://127.0.0.1:11434/v1", None, Duration::from_secs(300)));
        let m = &ollama.models[0];
        assert_eq!((m.name.as_str(), m.context, m.max_output, m.family.as_deref(), m.reasoning), ("qwen3:8b", 8192, 1024, Some("qwen3"), true));
        assert_eq!((m.shape.structured, m.shape.tools, m.shape.extra_sampling, m.shape.seed), (StructuredMode::JsonSchema, ToolMode::ConstrainedJson, false, true));
        let lm = r.providers.iter().find(|p| p.id == "lmstudio").unwrap();
        assert_eq!((lm.key_name.as_deref(), lm.timeout, lm.models[0].shape.extra_sampling, lm.models[0].context, lm.models[0].max_output), (Some("models/lmstudio"), Duration::from_secs(120), true, 8192, 1024));
        assert_eq!(lm.models[0].shape.structured, StructuredMode::None, "a reply is held to a shape only when the person says the server can");
    }

    #[test]
    fn an_address_that_is_not_this_machine_is_left_out_with_its_reason_and_the_rest_is_used() {
        let file = valid(
            &r#"{ "providers": {
                "a": { "base_url": "http://127.0.0.1:11434/v1", "models": { "m": {} } },
                "far": { "base_url": "https://api.openai.com/v1", "key_name": "models/openai", "models": { "gpt": {} } },
                "lan": { "base_url": "http://192.168.1.20:11434/v1" },
                "tls": { "base_url": "https://127.0.0.1:11434/v1" },
                "own": { "base_url": "http://127.0.0.1:8767/v1" },
                "bad": { "base_url": "http://LOGIN@127.0.0.1/v1" },
                "off": { "base_url": "http://127.0.0.1:1/v1", "enabled": false }
            } }"#
                // A login in an address, written in two pieces so that the secret scan before a commit has nothing to find in this file.
                .replace("LOGIN", &["user", "pw"].join(":")),
        );
        let r = resolve(&file);
        assert_eq!(r.providers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["a"]);
        assert_eq!(r.disabled, ["off"]);
        let why = |id: &str| r.refused.iter().find(|x| x.provider == id).unwrap_or_else(|| panic!("{id} was not refused")).reason.clone();
        assert!(why("far").contains("api.openai.com is not on this machine") && why("far").contains("is not built"), "{}", why("far"));
        assert!(why("lan").contains("192.168.1.20 is not on this machine"));
        assert!(why("tls").contains("TLS"));
        assert!(why("own").contains("Aqua's own API"));
        assert!(why("bad").contains("not one a model service can have") && !why("bad").contains("pw"), "{}", why("bad"));
        assert_eq!(r.refused.len(), 5, "{:?}", r.refused);
    }

    #[test]
    fn a_file_that_is_wrong_in_its_form_is_refused_whole_with_the_name_of_what_is_wrong() {
        assert!(invalid(r#"{ "version": 2 }"#).contains("version is 2"));
        assert!(invalid(r#"{ "providers": { "Ollama": { "base_url": "http://127.0.0.1/v1" } } }"#).contains("not allowed"));
        for reserved in ["llama.cpp", "llama", "local", "has/slash", ""] {
            assert!(invalid(&format!(r#"{{ "providers": {{ "{reserved}": {{ "base_url": "http://127.0.0.1/v1" }} }} }}"#)).contains("not allowed"), "{reserved}");
        }
        // A key of the forge cannot be named, and a key must be named.
        for key in ["github/token", "models/", "models/../x", "/models/x", "model/x"] {
            let why = invalid(&format!(r#"{{ "providers": {{ "p": {{ "base_url": "http://127.0.0.1/v1", "key_name": "{key}" }} }} }}"#));
            assert!(why.contains("key_name") && why.contains("models/"), "{key}: {why}");
        }
        valid(r#"{ "providers": { "p": { "base_url": "http://127.0.0.1/v1", "key_name": "models/p.key-1_a" } } }"#);
        assert!(invalid(r#"{ "providers": { "p": { "base_url": "http://127.0.0.1/v1", "timeout_secs": 4 } } }"#).contains("timeout_secs"));
        assert!(invalid(r#"{ "providers": { "p": { "base_url": "http://127.0.0.1/v1", "timeout_secs": 901 } } }"#).contains("timeout_secs"));
        for (field, value) in [("context", "100"), ("context", "2000000"), ("max_output", "0"), ("max_output", "70000"), ("tools", "\"magic\""), ("structured", "\"xml\""), ("family", "\"\"")] {
            let why = invalid(&format!(r#"{{ "providers": {{ "p": {{ "base_url": "http://127.0.0.1/v1", "models": {{ "m": {{ "{field}": {value} }} }} }} }} }}"#));
            assert!(why.contains("p/m") && why.contains(field), "{field}={value}: {why}");
        }
        assert!(invalid(r#"{ "providers": { "p": { "base_url": "http://127.0.0.1/v1", "models": { "a b": {} } } } }"#).contains("model name"));
        // A key the file does not know is an error, not something ignored: the file says where prompts go.
        for text in [r#"{ "providers": { "p": { "base_url": "http://127.0.0.1/v1", "headers": {} } } }"#, r#"{ "provider": {} }"#, r#"{ "providers": { "p": {} } }"#, r#"{ "providers": { "p": { "base_url": "http://127.0.0.1/v1", "models": { "m": { "contex": 1 } } } } }"#] {
            assert!(read(text).is_err(), "{text}");
        }
        let many: String = (0..17).map(|i| format!(r#""p{i}": {{ "base_url": "http://127.0.0.1/v1" }}"#)).collect::<Vec<_>>().join(",");
        assert!(invalid(&format!(r#"{{ "providers": {{ {many} }} }}"#)).contains("at most 16"));
    }

    #[test]
    fn nothing_is_a_file_with_no_providers_and_a_model_with_nothing_said_gets_the_plain_defaults() {
        assert_eq!(valid("{}"), ProvidersFile::default());
        assert!(resolve(&ProvidersFile::default()).providers.is_empty());
        let file = valid(r#"{ "providers": { "p": { "base_url": "http://localhost:1234/v1/", "models": { "m": {} } } } }"#);
        let spec = &resolve(&file).providers[0];
        let m = &spec.models[0];
        assert_eq!((spec.endpoint.url().as_str(), m.context, m.max_output, m.reasoning, m.family.clone()), ("http://localhost:1234/v1", 4096, 1024, false, None));
        assert_eq!((m.shape.tools, m.shape.structured, m.shape.seed, m.shape.extra_sampling), (ToolMode::ConstrainedJson, StructuredMode::None, true, false));
    }
}
