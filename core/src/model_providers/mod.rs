//! The model servers on this machine that the person has listed, read from `data/models/providers.json` and kept for the routes (card OA9-04, tasks PR-07 and PR-08 of the OpenCode audit).
//!
//! The model layer (`core/src/model/`) knows how to speak to such a server ([`OpenAiCompatProvider`]) and what the file may say ([`providers_config`]); it does not read files
//! (it may not name `config_files`, `core/tests/layering_test.rs`). This module is the part that does: it reads the file with the loader of the other config files
//! (`config_files::load`, spec [`PROVIDERS`]), builds the providers, gives them a router with the gate, and answers the two routes of `api_gateway/models_api.rs`:
//!
//! * [`ProviderRegistry::status`]: what the file says and comes to (the providers that may be used, the ones that are refused and why), the models each lists, whether the key it names is
//!   in the credential store (a yes or a no, never the key), and what the egress gate has decided lately.
//! * [`ProviderRegistry::test`]: does the server answer (`GET {base_url}/models`), which of the models it has are listed in the file and which are not, and, when asked, one short call to a
//!   listed model through the router and its gate.
//!
//! **Off unless `AQUA_MODEL_PROVIDERS` is on.** Off, the file is not read at all and the status says so. On, the file is read again whenever its bytes change, so a provider added to it is
//! there on the next call and no restart is needed (the other config files are read once, at start).
//!
//! **Nothing here makes the engine, the planner, the chat or the flows ask these servers.** They go on asking the local server; a provider listed here is asked by a test call, or by code
//! that builds a router from [`ProviderRegistry::router`] (the flows and the triage take a `TextModel` of the router with `AsLegacy(RouterText::new(..))`). That wiring is a change of what
//! model Aqua uses and is the person's to decide, with a measurement: see `documentation/MODEL_PROVIDER.md`.

use crate::config_files::{self, ConfigSpec, Loaded, Missing, OnBroken};
use crate::model::egress::{GateReport, LocalOnlyGate};
use crate::model::providers_config::{self, ProvidersFile, Resolution};
use crate::model::{CallControl, ChatRequest, Flow, Message, ModelProvider, ModelRef, ModelRouter, OpenAiCompatProvider, Purpose, Role, StructuredMode, ToolMode};
use aqua_native_tools::secrets::{CredentialManager, Secrets};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// `data/models/providers.json`, read like the other config files. A file that is not there is no provider (and nothing is written: the file is the person's to make, see
/// `documentation/model_providers.example.json`); a file that cannot be used turns every provider off until it is fixed.
pub const PROVIDERS: ConfigSpec<ProvidersFile> = ConfigSpec {
    name: "providers.json",
    rel: providers_config::REL,
    label: "model providers",
    known_keys: &["version", "providers"],
    on_missing: Missing::Empty(ProvidersFile::default),
    on_broken: OnBroken::NoProviders,
    parse: config_files::parse_json::<ProvidersFile>,
    validate: Some(providers_config::validate),
};

/// The words a test call says to the model, and the most of its answer that is kept.
const TEST_PROMPT: &str = "Reply with the single word OK.";
const TEST_TEXT_CHARS: usize = 200;
const TEST_SECS: u64 = 30;

/// What was read and what it came to, at one moment.
pub struct Snapshot {
    /// `AQUA_MODEL_PROVIDERS` was on when this was made.
    pub enabled: bool,
    /// `off` (not read), `absent`, `ok` or `broken`.
    pub state: &'static str,
    /// Where it is wrong, without any value of the file (kind, line, column, message).
    pub error: Option<Value>,
    pub warnings: Vec<String>,
    pub resolution: Resolution,
    providers: Vec<Arc<OpenAiCompatProvider>>,
    fingerprint: u64,
}

impl Snapshot {
    fn off() -> Snapshot {
        Snapshot { enabled: false, state: "off", error: None, warnings: Vec::new(), resolution: Resolution::default(), providers: Vec::new(), fingerprint: 0 }
    }
}

/// The providers of one data folder.
pub struct ProviderRegistry {
    data_dir: PathBuf,
    secrets: Arc<dyn Secrets>,
    gate: Arc<LocalOnlyGate>,
    snapshot: Mutex<Option<Arc<Snapshot>>>,
}

impl ProviderRegistry {
    /// A registry for the models folder `models_dir` (`data/models`), with the credential store `secrets` for the keys the providers name.
    pub fn open(models_dir: &Path, secrets: Arc<dyn Secrets>) -> ProviderRegistry {
        ProviderRegistry { data_dir: models_dir.parent().unwrap_or(models_dir).to_path_buf(), secrets, gate: Arc::new(LocalOnlyGate::new()), snapshot: Mutex::new(None) }
    }

    /// What the file says now. Read again only when its bytes have changed since the last call.
    pub fn snapshot(&self, enabled: bool) -> Arc<Snapshot> {
        let mut held = self.snapshot.lock().unwrap_or_else(PoisonError::into_inner);
        if !enabled {
            let off = Arc::new(Snapshot::off());
            *held = Some(off.clone());
            return off;
        }
        let seen = config_files::inspect(&self.data_dir, &PROVIDERS).fingerprint;
        if let Some(current) = held.as_ref().filter(|s| s.enabled && s.fingerprint == seen) {
            return current.clone();
        }
        let loaded = config_files::load(&self.data_dir, &PROVIDERS);
        let warnings = config_files::inspect(&self.data_dir, &PROVIDERS).warnings;
        let made = match loaded {
            Loaded::Ok(file) | Loaded::Defaulted(file) => {
                let resolution = providers_config::resolve(&file);
                let providers = resolution.providers.iter().map(|spec| Arc::new(OpenAiCompatProvider::new(spec.clone(), self.secrets.clone()))).collect();
                Snapshot { enabled: true, state: if seen == 0 { "absent" } else { "ok" }, error: None, warnings, resolution, providers, fingerprint: seen }
            }
            Loaded::Broken(e) => Snapshot {
                enabled: true,
                state: "broken",
                error: Some(json!({ "kind": e.kind.word(), "line": e.line, "column": e.column, "message": e.message })),
                warnings,
                resolution: Resolution::default(),
                providers: Vec::new(),
                fingerprint: seen,
            },
        };
        let made = Arc::new(made);
        *held = Some(made.clone());
        made
    }

    /// A router over the providers the file lists, with the gate of this registry and the program's counters. Empty when the switch is off or the file lists none.
    pub fn router(&self, enabled: bool) -> ModelRouter {
        let snapshot = self.snapshot(enabled);
        let providers: Vec<Arc<dyn ModelProvider>> = snapshot.providers.iter().map(|p| p.clone() as Arc<dyn ModelProvider>).collect();
        ModelRouter::new(providers).with_counters(crate::metrics::inc).with_gate(self.gate.clone())
    }

    pub fn gate_report(&self) -> GateReport {
        self.gate.report()
    }

    /// Is the key a provider names in the credential store? `None` when it names none or the store cannot be read. Never the key.
    fn key_present(&self, name: &Option<String>) -> Option<bool> {
        let name = name.as_ref()?;
        self.secrets.get(name).ok().map(|k| k.is_some_and(|k| !k.is_empty()))
    }

    /// The body of `GET /api/v1/models/providers`.
    pub fn status(&self, enabled: bool) -> Value {
        let snapshot = self.snapshot(enabled);
        let gate = self.gate.report();
        let providers: Vec<Value> = snapshot
            .providers
            .iter()
            .map(|p| {
                let spec = p.spec();
                json!({
                    "provider": spec.id,
                    "base_url": spec.endpoint.url(),
                    "egress": "loopback",
                    "key_name": spec.key_name,
                    "key_present": self.key_present(&spec.key_name),
                    "timeout_secs": spec.timeout.as_secs(),
                    "models": p.list().iter().zip(spec.models.iter()).map(|(info, m)| json!({
                        "model": info.model.to_string(),
                        "name": m.name,
                        "context": m.context,
                        "max_output": m.max_output,
                        "tools": tools_word(m.shape.tools),
                        "structured": structured_word(m.shape.structured),
                        "reasoning": m.reasoning,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({
            "enabled": snapshot.enabled,
            "switch": "AQUA_MODEL_PROVIDERS",
            "file": { "name": providers_config::FILE, "state": snapshot.state, "error": snapshot.error, "warnings": snapshot.warnings },
            "providers": providers,
            "refused": snapshot.resolution.refused,
            "disabled": snapshot.resolution.disabled,
            "gate": { "name": "local_only", "allowed": gate.allowed, "refused": gate.refused, "recent": gate.recent },
            "remote": "refused: a model on another machine needs the gate that cleans the text, asks for each call, caps the spend and keeps a ledger, and that is not built",
            "note": "Nothing in the agent, the chat or the flows asks these servers; the models Aqua runs are the ones it always ran.",
        })
    }

    /// The body of `POST /api/v1/models/providers/test`: does the server answer, which models does it have, and (when `model` is given) one short call to that model. Blocks for up to
    /// 30 seconds; the route runs it off the async runtime. `Err` is a refusal of the request itself (the switch is off, no such provider, no such model).
    pub fn test(&self, enabled: bool, provider: &str, model: Option<&str>) -> Result<Value, String> {
        let snapshot = self.snapshot(enabled);
        if !snapshot.enabled {
            return Err("model providers are off: set AQUA_MODEL_PROVIDERS=on and restart Aqua".into());
        }
        if let Some(refused) = snapshot.resolution.refused.iter().find(|r| r.provider == provider) {
            return Err(format!("{provider} is not used: {}", refused.reason));
        }
        let Some(found) = snapshot.providers.iter().find(|p| p.spec().id == provider).cloned() else {
            return Err(format!("no provider is called {provider} in providers.json"));
        };
        if let Some(name) = model {
            if !found.spec().models.iter().any(|m| m.name == name) {
                return Err(format!("the model {name} is not listed under {provider} in providers.json"));
            }
        }
        let never = AtomicBool::new(false);
        let began = Instant::now();
        let mut out = json!({ "provider": provider, "base_url": found.spec().endpoint.url() });
        match found.discover(Duration::from_secs(10), &never) {
            Ok(names) => {
                let listed: Vec<&str> = found.spec().models.iter().map(|m| m.name.as_str()).collect();
                out["reachable"] = json!(true);
                out["server_models"] = json!(names);
                out["not_listed"] = json!(names.iter().filter(|n| !listed.contains(&n.as_str())).collect::<Vec<_>>());
                out["missing_on_server"] = json!(listed.iter().filter(|n| !names.iter().any(|s| s == *n)).collect::<Vec<_>>());
            }
            Err(e) => {
                out["reachable"] = json!(false);
                out["error"] = json!({ "class": e.class(), "message": e.to_string() });
            }
        }
        if let Some(name) = model {
            let router = ModelRouter::new(vec![found.clone() as Arc<dyn ModelProvider>]).with_counters(crate::metrics::inc).with_gate(self.gate.clone());
            let messages = [Message::user(TEST_PROMPT)];
            let request = ChatRequest::new(&messages, 16, Purpose::Chat);
            let mut ctl = CallControl::new(&never, Duration::from_secs(TEST_SECS));
            let asked = Instant::now();
            out["chat"] = match router.call_on(&ModelRef::new(provider, name), Role::Fast, &request, &mut ctl, &mut |_| Flow::Continue) {
                Ok(o) => json!({
                    "model": o.model.to_string(),
                    "ok": true,
                    "text": o.text.chars().take(TEST_TEXT_CHARS).collect::<String>(),
                    "finish": format!("{:?}", o.finish),
                    "tokens_in": o.usage.input + o.usage.cached,
                    "tokens_out": o.usage.output,
                    "ms": asked.elapsed().as_millis() as u64,
                }),
                Err(e) => json!({ "model": format!("{provider}/{name}"), "ok": false, "class": e.class(), "error": e.to_string(), "ms": asked.elapsed().as_millis() as u64 }),
            };
        }
        out["ms"] = json!(began.elapsed().as_millis() as u64);
        Ok(out)
    }
}

/// Is `AQUA_MODEL_PROVIDERS` on? The routes ask here (they may not name the model layer: `core/tests/layering_test.rs`), on the thread of their request, and hand the answer to the registry.
pub fn enabled() -> bool {
    crate::model::providers_enabled()
}

fn tools_word(mode: ToolMode) -> &'static str {
    match mode {
        ToolMode::None => "none",
        ToolMode::ConstrainedJson => "constrained_json",
        ToolMode::Native => "native",
    }
}

fn structured_word(mode: StructuredMode) -> &'static str {
    match mode {
        StructuredMode::None => "none",
        StructuredMode::JsonObject => "json_object",
        StructuredMode::JsonSchema => "json_schema",
        StructuredMode::Gbnf => "gbnf",
    }
}

/// The registry of a models folder, made on first use with the credential store of the machine. One per folder, for the life of the process.
pub fn shared(models_dir: &Path) -> Arc<ProviderRegistry> {
    static REGISTRIES: Mutex<Option<HashMap<PathBuf, Arc<ProviderRegistry>>>> = Mutex::new(None);
    let mut all = REGISTRIES.lock().unwrap_or_else(PoisonError::into_inner);
    let map = all.get_or_insert_with(HashMap::new);
    // A test makes a folder of its own for each test, so the table is bounded: when it is full the oldest are dropped, and a registry that is made again reads its file again.
    if map.len() >= 256 && !map.contains_key(models_dir) {
        map.clear();
    }
    map.entry(models_dir.to_path_buf()).or_insert_with(|| Arc::new(ProviderRegistry::open(models_dir, Arc::new(CredentialManager)))).clone()
}

#[cfg(test)]
mod tests;
