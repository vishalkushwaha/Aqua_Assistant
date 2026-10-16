//! Infrastructure-as-code tools (`iac.*`) built on `terraform` (OpenTofu works if you point
//! `terraform_path` at `tofu.exe`).
//!
//! The model names a *configured* working folder, never a path of its own. `iac.plan` saves the
//! plan and returns a `plan_id`; `iac.apply` applies only that saved plan, after checking its
//! SHA-256 has not changed, and shows the counts of resources to add, change and destroy in the
//! approval. There is no `destroy` tool, no `-var`, no `-target` and no `-auto-approve` on
//! anything that was not planned. Cloud credentials come from Credential Manager by name
//! (`env_secrets`), are put only in terraform's environment, and are masked in every result.

use crate::cli::{first_line, s, Cli, Ran};
use crate::fs::guard::{display, Op, PathGuard};
use crate::secrets::{valid_name, Secrets};
use aqua_tool_host::{
    hex_lower, Annotations, Availability, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IacDir {
    /// A name the model uses instead of the path.
    pub name: String,
    /// A folder inside the allowed roots that holds the `.tf` files.
    pub path: String,
    /// Allow `iac.apply` here. Off unless set, and it always asks.
    #[serde(default)]
    pub apply: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IacConfig {
    #[serde(default)]
    pub terraform_path: Option<String>,
    #[serde(default)]
    pub dirs: Vec<IacDir>,
    /// Environment variable name -> the name of a secret in Credential Manager (`aqua-cli secret-set`).
    #[serde(default)]
    pub env_secrets: BTreeMap<String, String>,
    /// Refuse to apply a plan that would destroy more than this many resources.
    #[serde(default = "default_max_destroy")]
    pub max_destroy: u32,
}

fn default_max_destroy() -> u32 {
    5
}

impl Default for IacConfig {
    fn default() -> Self {
        Self { terraform_path: None, dirs: Vec::new(), env_secrets: BTreeMap::new(), max_destroy: 5 }
    }
}

impl IacConfig {
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for d in &self.dirs {
            if d.name.is_empty() || d.name.len() > 60 || !d.name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
                return Err(format!("folder name '{}' may only use letters, digits and - _ .", d.name));
            }
            if !seen.insert(d.name.clone()) {
                return Err(format!("folder '{}' is listed twice", d.name));
            }
            if d.path.trim().is_empty() {
                return Err(format!("folder '{}' has no path", d.name));
            }
        }
        for (env, secret) in &self.env_secrets {
            if env.is_empty() || !env.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') || env.starts_with(|c: char| c.is_ascii_digit()) {
                return Err(format!("'{env}' is not a valid environment variable name (upper case letters, digits, _)"));
            }
            if ["PATH", "PATHEXT", "SYSTEMROOT", "COMSPEC", "TF_CLI_ARGS", "TF_CLI_CONFIG_FILE", "TF_PLUGIN_CACHE_DIR", "TF_DATA_DIR", "TF_WORKSPACE", "TF_LOG", "TF_LOG_PATH"].contains(&env.as_str()) || env.starts_with("TF_CLI_ARGS") || env.starts_with("TF_VAR_") {
                return Err(format!("{env} cannot be set from a secret"));
            }
            if !valid_name(secret) {
                return Err(format!("'{secret}' is not a valid secret name"));
            }
        }
        if self.max_destroy > 1000 {
            return Err("max_destroy must be 0-1000".into());
        }
        Ok(())
    }
}

/// What a saved plan would do, read from `terraform show -json`. Values are never read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub add: u32,
    pub change: u32,
    pub destroy: u32,
    pub replace: u32,
    /// Addresses that would be destroyed or replaced, for the approval text.
    pub risky: Vec<String>,
}

pub fn count_changes(show_json: &str) -> Result<Counts, String> {
    let v: Value = serde_json::from_str(show_json).map_err(|e| format!("terraform show did not return JSON: {e}"))?;
    let mut c = Counts::default();
    for rc in v.get("resource_changes").and_then(|x| x.as_array()).map(|a| a.as_slice()).unwrap_or(&[]) {
        let actions: Vec<&str> = rc["change"]["actions"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
        let addr = rc["address"].as_str().unwrap_or("?").to_string();
        let has = |a: &str| actions.contains(&a);
        if has("delete") && has("create") {
            c.replace += 1;
            c.risky.push(format!("replace {addr}"));
        } else if has("delete") {
            c.destroy += 1;
            c.risky.push(format!("destroy {addr}"));
        } else if has("create") {
            c.add += 1;
        } else if has("update") {
            c.change += 1;
        }
    }
    Ok(c)
}

impl Counts {
    /// Replacements destroy the old resource, so they count as destroys for the limit.
    pub fn destroys(&self) -> u32 {
        self.destroy + self.replace
    }
    pub fn text(&self) -> String {
        format!("{} to add, {} to change, {} to destroy, {} to replace", self.add, self.change, self.destroy, self.replace)
    }
}

/// Replace the value of every output marked sensitive; keep the rest.
pub fn mask_outputs(json_text: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_str(json_text).map_err(|e| format!("terraform output did not return JSON: {e}"))?;
    let Some(map) = v.as_object() else { return Err("terraform output is not an object".into()) };
    let mut out = serde_json::Map::new();
    for (k, o) in map {
        let sensitive = o.get("sensitive").and_then(|x| x.as_bool()).unwrap_or(false);
        out.insert(k.clone(), if sensitive { json!("<sensitive, hidden>") } else { o.get("value").cloned().unwrap_or(Value::Null) });
    }
    Ok(Value::Object(out))
}

struct PlanRecord {
    dir: String,
    file: PathBuf,
    sha256: String,
}

struct Inner {
    cfg: IacConfig,
    guard: Arc<PathGuard>,
    secrets: Arc<dyn Secrets>,
    cli: Cli,
    plans: Mutex<HashMap<String, PlanRecord>>,
    plan_dir: PathBuf,
}

pub struct IacProvider(Arc<Inner>);

impl IacProvider {
    pub fn new(cfg: IacConfig, guard: Arc<PathGuard>, secrets: Arc<dyn Secrets>, scratch: &Path) -> Result<Self, String> {
        cfg.validate()?;
        let plan_dir = scratch.join("iac_plans");
        std::fs::create_dir_all(&plan_dir).map_err(|e| e.to_string())?;
        let cli = Cli::find("terraform", &cfg.terraform_path, &["%ProgramFiles%/Terraform", "%LOCALAPPDATA%/Programs/Terraform"], scratch);
        Ok(Self(Arc::new(Inner { cfg, guard, secrets, cli, plans: Mutex::new(HashMap::new()), plan_dir })))
    }

    pub fn installed(&self) -> bool {
        self.0.cli.installed()
    }
}

fn bad(msg: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArguments, msg)
}

fn arg_str<'a>(a: &'a Value, k: &str) -> &'a str {
    a.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Read,
    Init,
    Plan,
    Apply,
}

fn specs() -> Vec<(&'static str, &'static str, Kind, Value, Vec<&'static str>)> {
    let dir = json!({"type": "string", "minLength": 1, "maxLength": 60});
    vec![
        ("iac.dirs", "List the configured Terraform folders and whether apply is enabled for each.", Kind::Read, json!({}), vec![]),
        ("iac.validate", "Check a configured folder's Terraform files for errors. Changes nothing.", Kind::Read, json!({"dir": dir.clone()}), vec!["dir"]),
        ("iac.state_list", "List the resource addresses in a folder's state. Values are not shown.", Kind::Read, json!({"dir": dir.clone()}), vec!["dir"]),
        ("iac.output", "Show a folder's outputs. Outputs marked sensitive are hidden.", Kind::Read, json!({"dir": dir.clone()}), vec!["dir"]),
        ("iac.init", "Download providers and set up a configured folder. Asks.", Kind::Init, json!({"dir": dir.clone()}), vec!["dir"]),
        ("iac.plan", "Work out what would change and save the plan. Returns a plan_id. Changes nothing. Asks.", Kind::Plan, json!({"dir": dir.clone()}), vec!["dir"]),
        ("iac.apply", "Apply a saved plan by plan_id. Fails if the plan changed. Always asks and shows the counts.", Kind::Apply, json!({"plan_id": {"type": "string", "minLength": 8, "maxLength": 64}}), vec!["plan_id"]),
    ]
}

impl Inner {
    fn dir(&self, name: &str) -> Result<(&IacDir, PathBuf), ToolError> {
        let d = self.cfg.dirs.iter().find(|d| d.name == name).ok_or_else(|| {
            ToolError::new(ErrorCode::NotFound, format!("no configured folder '{name}'. Configured: {}", self.cfg.dirs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ")))
        })?;
        let r = self.guard.resolve(&d.path, Op::Read)?;
        if !r.path.is_dir() {
            return Err(ToolError::new(ErrorCode::NotFound, format!("the folder for '{name}' does not exist: {}", display(&r.path))));
        }
        Ok((d, r.path))
    }

    /// Terraform runs with the scrubbed environment plus the configured secrets. Returns the secret
    /// values too, so the host can mask them.
    fn tf(&self, dir: &Path, args: &[String], timeout_s: u64, cap_kb: usize, cancel: &AtomicBool) -> Result<(Ran, Vec<String>), ToolError> {
        let mut env: Vec<(String, String)> = vec![("TF_IN_AUTOMATION".into(), "1".into()), ("TF_INPUT".into(), "0".into()), ("CHECKPOINT_DISABLE".into(), "1".into())];
        let mut values = Vec::new();
        for (name, secret) in &self.cfg.env_secrets {
            let v = self.secrets.get(secret).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("credential store: {e}")))?.ok_or_else(|| {
                ToolError::new(ErrorCode::AuthRequired, format!("the secret '{secret}' for {name} is not set. Store it yourself with: aqua-cli secret-set {secret}"))
            })?;
            values.push(v.clone());
            env.push((name.clone(), v));
        }
        let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let r = self.cli.run(args, &env_refs, Some(dir), timeout_s, cap_kb, cancel)?;
        Ok((r, values))
    }

    fn ok(&self, dir: &Path, args: &[String], timeout_s: u64, cap_kb: usize, cancel: &AtomicBool) -> Result<(Ran, Vec<String>), ToolError> {
        let (r, secrets) = self.tf(dir, args, timeout_s, cap_kb, cancel)?;
        if r.code == Some(0) {
            Ok((r, secrets))
        } else {
            Err(tf_failure(&r.stderr, &r.stdout))
        }
    }

    fn record(&self, id: &str) -> Result<(PlanRecord, PathBuf), ToolError> {
        let id = id.trim();
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(bad("plan_id must be the hexadecimal id returned by iac.plan"));
        }
        let plans = self.plans.lock().unwrap_or_else(|e| e.into_inner());
        let p = plans.get(id).ok_or_else(|| ToolError::new(ErrorCode::NotFound, "no saved plan with that id (plans are forgotten when Aqua restarts). Run iac.plan again."))?;
        let (_, dir) = self.dir(&p.dir)?;
        Ok((PlanRecord { dir: p.dir.clone(), file: p.file.clone(), sha256: p.sha256.clone() }, dir))
    }

    fn verify_plan(&self, rec: &PlanRecord) -> Result<(), ToolError> {
        let bytes = std::fs::read(&rec.file).map_err(|_| ToolError::new(ErrorCode::NotFound, "the saved plan file is gone. Run iac.plan again."))?;
        if hex_lower(&Sha256::digest(&bytes)) != rec.sha256 {
            return Err(ToolError::new(ErrorCode::PermissionDenied, "the saved plan file has changed since it was made and will not be applied. Run iac.plan again."));
        }
        Ok(())
    }

    fn counts_for(&self, rec: &PlanRecord, dir: &Path, cancel: &AtomicBool) -> Result<Counts, ToolError> {
        let (r, _) = self.ok(dir, &s(&["show", "-json", "-no-color", &rec.file.to_string_lossy()]), 60, 4096, cancel)?;
        count_changes(&r.stdout).map_err(|e| ToolError::new(ErrorCode::BackendError, e))
    }
}

fn tf_failure(stderr: &str, stdout: &str) -> ToolError {
    let all = format!("{stderr}\n{stdout}");
    let low = all.to_ascii_lowercase();
    let first = all.lines().map(|l| l.trim()).find(|l| l.to_ascii_lowercase().contains("error")).map(String::from).unwrap_or_else(|| first_line(&all));
    let has = |m: &str| low.contains(m);
    let code = if has("state lock") || has("error acquiring the state lock") {
        ErrorCode::Conflict
    } else if has("could not load plugin") || has("please run \"terraform init\"") || has("terraform init") && has("required") || has("module not installed") || has("inconsistent dependency lock") {
        return ToolError::new(ErrorCode::BackendError, format!("Run iac.init for this folder first. {first}"));
    } else if has("no valid credential") || has("noCredentialProviders") || has("no credentials") || has("expiredtoken") || has("unauthorized") || has("authentication") && has("failed") {
        ErrorCode::AuthRequired
    } else if has("accessdenied") || has("forbidden") || has("403") {
        ErrorCode::OsPermissionDenied
    } else if has("no configuration files") {
        ErrorCode::NotFound
    } else if has("timeout") || has("timed out") {
        ErrorCode::Timeout
    } else if has("failed to query available provider") || has("connection refused") || has("no such host") || has("dial tcp") {
        ErrorCode::Unavailable
    } else if has("invalid") || has("unsupported argument") || has("syntax") {
        ErrorCode::InvalidArguments
    } else {
        ErrorCode::BackendError
    };
    ToolError::new(code, first)
}

impl ToolProvider for IacProvider {
    fn id(&self) -> &str {
        "iac"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        specs()
            .into_iter()
            .map(|(name, description, kind, props, required)| ToolManifest {
                name: name.into(),
                title: name.into(),
                description: description.into(),
                classes: match kind {
                    Kind::Read => vec![PermClass::Read],
                    Kind::Init => vec![PermClass::Execute, PermClass::Network],
                    Kind::Plan => vec![PermClass::Execute, PermClass::Remote],
                    Kind::Apply => vec![PermClass::Execute, PermClass::Remote, PermClass::ExternalEffect],
                },
                default_decision: Decision::Auto,
                input_schema: json!({"type": "object", "properties": props, "required": required}),
                timeout_default_ms: 600_000,
                timeout_max_ms: 900_000,
                annotations: Annotations { read_only: kind == Kind::Read, destructive: kind == Kind::Apply, idempotent: kind == Kind::Read, open_world: true },
                untrusted_output: true,
            })
            .collect()
    }

    fn availability(&self, _tool: &str) -> Availability {
        if !self.0.cli.installed() {
            return Availability::Unavailable(self.0.cli.not_installed());
        }
        Availability::Available
    }

    fn classify(&self, tool: &str, a: &Value) -> Result<Classification, ToolError> {
        let s = &self.0;
        let (_, _, kind, _, _) = specs().into_iter().find(|x| x.0 == tool).ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("unknown tool '{tool}'")))?;
        if tool == "iac.dirs" {
            return Ok(Classification { classes: vec![PermClass::Read], target: "iac.json".into(), summary: "List Terraform folders".into(), target_allowed: true });
        }
        match kind {
            Kind::Read => {
                let (d, _) = s.dir(arg_str(a, "dir"))?;
                Ok(Classification { classes: vec![PermClass::Read], target: d.name.clone(), summary: format!("{tool} in {}", d.name), target_allowed: true })
            }
            Kind::Init => {
                let (d, path) = s.dir(arg_str(a, "dir"))?;
                Ok(Classification { classes: vec![PermClass::Execute, PermClass::Network], target: d.name.clone(), summary: format!("terraform init in {} (downloads providers and modules)", display(&path)), target_allowed: false })
            }
            Kind::Plan => {
                let (d, path) = s.dir(arg_str(a, "dir"))?;
                Ok(Classification {
                    classes: vec![PermClass::Execute, PermClass::Remote],
                    target: d.name.clone(),
                    summary: format!("terraform plan in {} (runs provider code and reads the real infrastructure; changes nothing)", display(&path)),
                    target_allowed: false,
                })
            }
            Kind::Apply => {
                let (rec, dir) = s.record(arg_str(a, "plan_id"))?;
                let (d, _) = s.dir(&rec.dir)?;
                if !d.apply {
                    return Err(ToolError::new(ErrorCode::PermissionDenied, format!("apply is not enabled for '{}'. Set \"apply\": true for it in data/tools/iac.json to allow it (each apply will still ask).", d.name)));
                }
                s.verify_plan(&rec)?;
                let counts = s.counts_for(&rec, &dir, &AtomicBool::new(false))?;
                if counts.destroys() > s.cfg.max_destroy {
                    return Err(ToolError::new(
                        ErrorCode::ResourceLimit,
                        format!("this plan would destroy or replace {} resources ({}), which is above the limit of {}. Review it yourself, or raise max_destroy in data/tools/iac.json.", counts.destroys(), counts.risky.iter().take(8).cloned().collect::<Vec<_>>().join(", "), s.cfg.max_destroy),
                    ));
                }
                let mut classes = vec![PermClass::Execute, PermClass::Remote, PermClass::ExternalEffect];
                if counts.destroys() > 0 {
                    classes.push(PermClass::Destructive);
                }
                let risky = if counts.risky.is_empty() { String::new() } else { format!(" Includes: {}.", counts.risky.iter().take(10).cloned().collect::<Vec<_>>().join(", ")) };
                Ok(Classification { classes, target: d.name.clone(), summary: format!("APPLY the saved plan for {}: {}.{risky}", d.name, counts.text()), target_allowed: false })
            }
        }
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let inner = self.0.clone();
        let cancel = ctx.cancel.clone();
        let tool = tool.to_string();
        let timeout_ms = ctx.timeout_ms;
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                // The programs of this call get no more than what is left of the host's time for it (card OA4-03).
                let _clock = crate::term::CallClock::start(timeout_ms);
                inner.dispatch(&tool, &args, &cancel)
            })
            .await
            .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("terraform task failed: {e}")))?.map(ProviderOutput::marked_private)
        })
    }
}

impl Inner {
    fn dispatch(&self, tool: &str, a: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        let out = |r: Ran, data: Value, secrets: Vec<String>| {
            let mut text = format!("{}{}", r.stdout, if r.stderr.trim().is_empty() { String::new() } else { format!("\n{}", r.stderr.trim()) }).trim().to_string();
            if r.truncated {
                text.push_str("\n… output cut");
            }
            Ok(ProviderOutput { data, text: if text.is_empty() { "Done.".into() } else { text }, secrets, ..Default::default() })
        };
        match tool {
            "iac.dirs" => {
                let rows: Vec<Value> = self.cfg.dirs.iter().map(|d| json!({"dir": d.name, "path": d.path, "apply": d.apply})).collect();
                let text = self.cfg.dirs.iter().map(|d| format!("{} -> {} (apply {})", d.name, d.path, if d.apply { "enabled" } else { "off" })).collect::<Vec<_>>().join("\n");
                Ok(ProviderOutput { data: json!({"dirs": rows}), text: if text.is_empty() { "No Terraform folders are configured. Add them to data/tools/iac.json.".into() } else { text }, ..Default::default() })
            }
            "iac.validate" => {
                let (_, dir) = self.dir(arg_str(a, "dir"))?;
                let (r, sec) = self.tf(&dir, &s(&["validate", "-json", "-no-color"]), 120, 256, cancel)?;
                let v: Value = serde_json::from_str(&r.stdout).unwrap_or(Value::Null);
                if v.is_null() {
                    return Err(tf_failure(&r.stderr, &r.stdout));
                }
                let valid = v["valid"].as_bool().unwrap_or(false);
                let diags: Vec<String> = v["diagnostics"].as_array().map(|d| d.iter().map(|x| format!("{}: {}", x["severity"].as_str().unwrap_or("?"), x["summary"].as_str().unwrap_or(""))).collect()).unwrap_or_default();
                Ok(ProviderOutput { data: json!({"valid": valid, "diagnostics": diags}), text: if valid { "The configuration is valid.".into() } else { format!("Not valid:\n{}", diags.join("\n")) }, secrets: sec, ..Default::default() })
            }
            "iac.state_list" => {
                let (_, dir) = self.dir(arg_str(a, "dir"))?;
                let (r, sec) = self.ok(&dir, &s(&["state", "list", "-no-color"]), 120, 256, cancel)?;
                let addrs: Vec<&str> = r.stdout.lines().filter(|l| !l.trim().is_empty()).collect();
                Ok(ProviderOutput { data: json!({"resources": addrs}), text: if addrs.is_empty() { "The state has no resources.".into() } else { addrs.join("\n") }, secrets: sec, ..Default::default() })
            }
            "iac.output" => {
                let (_, dir) = self.dir(arg_str(a, "dir"))?;
                let (r, sec) = self.ok(&dir, &s(&["output", "-json", "-no-color"]), 60, 256, cancel)?;
                let masked = mask_outputs(&r.stdout).map_err(|e| ToolError::new(ErrorCode::BackendError, e))?;
                let text = serde_json::to_string_pretty(&masked).unwrap_or_default();
                Ok(ProviderOutput { data: masked, text, secrets: sec, ..Default::default() })
            }
            "iac.init" => {
                let (_, dir) = self.dir(arg_str(a, "dir"))?;
                let (r, sec) = self.ok(&dir, &s(&["init", "-input=false", "-no-color"]), 600, 256, cancel)?;
                out(r, json!({}), sec)
            }
            "iac.plan" => {
                let (d, dir) = self.dir(arg_str(a, "dir"))?;
                let stamp = format!("{}-{}", d.name, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|t| t.as_nanos()).unwrap_or(0));
                let file = self.plan_dir.join(format!("{stamp}.tfplan"));
                let (r, sec) = self.ok(&dir, &s(&["plan", "-input=false", "-no-color", "-lock=false", "-out", &file.to_string_lossy()]), 600, 256, cancel)?;
                let bytes = std::fs::read(&file).map_err(|_| ToolError::new(ErrorCode::BackendError, "terraform did not save a plan file"))?;
                let sha = hex_lower(&Sha256::digest(&bytes));
                let id = sha[..16].to_string();
                let rec = PlanRecord { dir: d.name.clone(), file: file.clone(), sha256: sha };
                let counts = self.counts_for(&rec, &dir, cancel)?;
                self.plans.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), rec);
                let mut o = out(r, json!({"plan_id": id, "add": counts.add, "change": counts.change, "destroy": counts.destroy, "replace": counts.replace, "risky": counts.risky}), sec)?;
                o.text = format!("plan_id: {id}\nPlan: {}.\n{}\n\n{}", counts.text(), if counts.risky.is_empty() { String::new() } else { format!("Will destroy or replace: {}", counts.risky.join(", ")) }, o.text);
                Ok(o)
            }
            "iac.apply" => {
                let (rec, dir) = self.record(arg_str(a, "plan_id"))?;
                let (d, _) = self.dir(&rec.dir)?;
                if !d.apply {
                    return Err(ToolError::new(ErrorCode::PermissionDenied, format!("apply is not enabled for '{}'", d.name)));
                }
                self.verify_plan(&rec)?;
                let counts = self.counts_for(&rec, &dir, cancel)?;
                if counts.destroys() > self.cfg.max_destroy {
                    return Err(ToolError::new(ErrorCode::ResourceLimit, "the plan destroys more resources than the configured limit"));
                }
                let (r, sec) = self.ok(&dir, &s(&["apply", "-input=false", "-no-color", &rec.file.to_string_lossy()]), 900, 256, cancel)?;
                // A plan is single-use: applying it changes the state it was made against.
                self.plans.lock().unwrap_or_else(|e| e.into_inner()).remove(arg_str(a, "plan_id").trim());
                let _ = std::fs::remove_file(&rec.file);
                out(r, json!({"applied": counts.text()}), sec)
            }
            other => Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_counts_read_actions_and_never_values() {
        let j = r#"{"resource_changes":[
            {"address":"aws_s3_bucket.a","change":{"actions":["create"],"after":{"secret":"hunter2"}}},
            {"address":"aws_instance.b","change":{"actions":["update"]}},
            {"address":"aws_db.c","change":{"actions":["delete"]}},
            {"address":"aws_vm.d","change":{"actions":["delete","create"]}},
            {"address":"aws_x.e","change":{"actions":["no-op"]}},
            {"address":"data.f","change":{"actions":["read"]}}]}"#;
        let c = count_changes(j).unwrap();
        assert_eq!((c.add, c.change, c.destroy, c.replace, c.destroys()), (1, 1, 1, 1, 2));
        assert_eq!(c.risky, vec!["destroy aws_db.c".to_string(), "replace aws_vm.d".to_string()]);
        assert!(!c.text().contains("hunter2"));
        assert!(count_changes("not json").is_err());
        assert_eq!(count_changes("{}").unwrap(), Counts::default());
    }

    #[test]
    fn sensitive_outputs_are_hidden() {
        let m = mask_outputs(r#"{"url":{"value":"https://x","sensitive":false},"pw":{"value":"hunter2","sensitive":true}}"#).unwrap();
        assert_eq!(m["url"], "https://x");
        assert!(!m.to_string().contains("hunter2") && m["pw"].as_str().unwrap().contains("hidden"));
    }

    #[test]
    fn config_rejects_bad_names_and_dangerous_env_secrets() {
        let ok = IacConfig { dirs: vec![IacDir { name: "infra".into(), path: "C:/w/infra".into(), apply: false }], env_secrets: [("AWS_ACCESS_KEY_ID".to_string(), "aws/key".to_string())].into(), ..Default::default() };
        assert!(ok.validate().is_ok());
        for env in ["path", "PATH", "TF_CLI_ARGS", "TF_CLI_ARGS_plan", "TF_VAR_x", "1X", "TF_LOG", "", "A-B"] {
            let c = IacConfig { env_secrets: [(env.to_string(), "aws/key".to_string())].into(), ..Default::default() };
            assert!(c.validate().is_err(), "{env}");
        }
        assert!(IacConfig { env_secrets: [("X".to_string(), "../k".to_string())].into(), ..Default::default() }.validate().is_err());
        assert!(IacConfig { dirs: vec![IacDir { name: "a b".into(), path: "x".into(), apply: false }], ..Default::default() }.validate().is_err());
        let d = IacDir { name: "a".into(), path: "x".into(), apply: false };
        assert!(IacConfig { dirs: vec![d.clone(), d], ..Default::default() }.validate().is_err());
        assert!(serde_json::from_str::<IacConfig>(r#"{"dirs":[],"var":"x=1"}"#).is_err());
    }

    #[test]
    fn terraform_errors_map_to_specific_codes() {
        for (text, want) in [
            ("Error: Error acquiring the state lock", ErrorCode::Conflict),
            ("Error: Inconsistent dependency lock file", ErrorCode::BackendError),
            ("Error: No valid credential sources found", ErrorCode::AuthRequired),
            ("Error: AccessDenied: User is not authorized", ErrorCode::OsPermissionDenied),
            ("Error: No configuration files", ErrorCode::NotFound),
            ("Error: Unsupported argument", ErrorCode::InvalidArguments),
            ("Error: something odd", ErrorCode::BackendError),
        ] {
            assert_eq!(tf_failure(text, "").code, want, "{text}");
        }
    }
}
