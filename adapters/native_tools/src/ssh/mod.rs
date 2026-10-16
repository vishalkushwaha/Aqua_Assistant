//! SSH tools (`ssh.*`) built on the system OpenSSH programs.
//!
//! Aqua runs `ssh`, `scp`, `ssh-keyscan` and `ssh-keygen` itself, with its own empty config file,
//! its own `known_hosts`, no agent, no forwarding, no password prompts and strict host-key
//! checking. Hosts come only from `data/tools/ssh.json`; the model names an alias.
//!
//! Not in this release: connection reuse (every call opens a connection), password login,
//! passphrase-protected keys, port forwarding and interactive sessions.

pub mod config;
pub mod remote;

use crate::contain::{build_env, Limits};
use crate::fs::guard::{display, Op, PathGuard};
use crate::term::run_command;
use aqua_tool_host::{
    Annotations, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider,
};
pub use config::{HostEntry, SshConfig};
use remote::{classify as classify_remote, valid_remote_path, Risk};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

struct Inner {
    cfg: SshConfig,
    guard: Arc<PathGuard>,
    known_hosts: PathBuf,
    empty_config: PathBuf,
    scratch: PathBuf,
    /// Fingerprint shown to the user for a pending `ssh.trust_host`, per alias.
    pending: Mutex<HashMap<String, (String, String)>>,
}

/// The SSH tools. State is behind an `Arc` so blocking tasks can own it.
pub struct SshProvider(Arc<Inner>);

impl SshProvider {
    /// `dir` is a folder Aqua owns (`data/tools`): it holds `known_hosts` and an empty ssh config.
    pub fn new(cfg: SshConfig, guard: Arc<PathGuard>, dir: &Path) -> Result<Self, String> {
        cfg.validate()?;
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let empty_config = dir.join("ssh_config");
        if !empty_config.exists() {
            std::fs::write(&empty_config, "# Aqua's own ssh config: intentionally empty. Your ~/.ssh/config is not used.\n").map_err(|e| e.to_string())?;
        }
        let known_hosts = dir.join("known_hosts");
        if !known_hosts.exists() {
            std::fs::write(&known_hosts, "").map_err(|e| e.to_string())?;
        }
        Ok(Self(Arc::new(Inner { cfg, guard, known_hosts, empty_config, scratch: dir.to_path_buf(), pending: Mutex::new(HashMap::new()) })))
    }
}

fn program(configured: &Option<String>, default_name: &str) -> PathBuf {
    if let Some(p) = configured {
        return PathBuf::from(crate::fs::guard::expand_env(p));
    }
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
    let win = PathBuf::from(root).join("System32").join("OpenSSH").join(format!("{default_name}.exe"));
    if win.is_file() {
        win
    } else {
        PathBuf::from(default_name)
    }
}

impl Inner {
    fn host(&self, args: &Value) -> Result<&HostEntry, ToolError> {
        let alias = args.get("host").and_then(|v| v.as_str()).unwrap_or("");
        self.cfg.find(alias).ok_or_else(|| {
            let known: Vec<&str> = self.cfg.hosts.iter().map(|h| h.alias.as_str()).collect();
            ToolError::new(ErrorCode::NotFound, format!("no configured host '{alias}'. Configured hosts: {}", if known.is_empty() { "(none; add them to data/tools/ssh.json)".into() } else { known.join(", ") }))
        })
    }

    /// Options every OpenSSH call gets. Nothing the model supplies goes into this list.
    fn base_options(&self) -> Vec<String> {
        let mut v: Vec<String> = vec!["-F".into(), self.empty_config.to_string_lossy().to_string()];
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let opts = [
            "BatchMode=yes",
            "StrictHostKeyChecking=yes",
            "HashKnownHosts=no",
            "IdentitiesOnly=yes",
            "IdentityAgent=none",
            "ForwardAgent=no",
            "ForwardX11=no",
            "ClearAllForwardings=yes",
            "PermitLocalCommand=no",
            "ProxyCommand=none",
            "ProxyJump=none",
            "ControlMaster=no",
            "ControlPath=none",
            "PasswordAuthentication=no",
            "KbdInteractiveAuthentication=no",
            "ServerAliveInterval=15",
            "ServerAliveCountMax=2",
            "LogLevel=ERROR",
        ];
        for o in opts {
            v.push("-o".into());
            v.push(o.into());
        }
        v.push("-o".into());
        v.push(format!("ConnectTimeout={}", self.cfg.connect_timeout_s));
        v.push("-o".into());
        v.push(format!("UserKnownHostsFile={}", self.known_hosts.to_string_lossy()));
        v.push("-o".into());
        v.push(format!("GlobalKnownHostsFile={null}"));
        v
    }

    fn identity_args(&self, h: &HostEntry) -> Vec<String> {
        match &h.identity_file {
            Some(k) => vec!["-i".into(), crate::fs::guard::expand_env(k)],
            None => Vec::new(),
        }
    }

    fn ssh_args(&self, h: &HostEntry, command: &str) -> Vec<String> {
        let mut v = self.base_options();
        v.push("-T".into());
        v.extend(self.identity_args(h));
        v.extend(["-p".into(), h.port.to_string(), "-l".into(), h.user.clone(), "--".into(), h.host.clone(), command.to_string()]);
        v
    }

    fn env(&self) -> Vec<(String, String)> {
        // No SSH_AUTH_SOCK: the agent is never reachable. Only the allowlisted variables pass.
        build_env(std::env::vars(), &HashMap::new()).unwrap_or_default()
    }

    fn limits(&self) -> Limits {
        Limits { memory_mb: 512, max_processes: 8 }
    }

    fn run(&self, exe: PathBuf, args: Vec<String>, timeout_s: u64, cap_kb: usize, cancel: &AtomicBool, stdin: Option<String>) -> Result<crate::term::RunResult, ToolError> {
        run_command(exe, args, self.scratch.clone(), self.env(), stdin, self.limits(), Duration::from_secs(timeout_s), cap_kb * 1024, cancel)
    }

    fn known_host_line(&self, h: &HostEntry) -> bool {
        let needle = if h.port == 22 { h.host.clone() } else { format!("[{}]:{}", h.host, h.port) };
        std::fs::read_to_string(&self.known_hosts).map(|t| t.lines().any(|l| !l.starts_with('#') && l.split_whitespace().next().map(|hosts| hosts.split(',').any(|x| x.eq_ignore_ascii_case(&needle))).unwrap_or(false))).unwrap_or(false)
    }

    /// Scan the host's key and return (known_hosts line, fingerprint).
    fn scan_key(&self, h: &HostEntry, cancel: &AtomicBool) -> Result<(String, String), ToolError> {
        let scan = self.run(program(&self.cfg.keyscan_path, "ssh-keyscan"), vec!["-t".into(), "ed25519,ecdsa,rsa".into(), "-p".into(), h.port.to_string(), "-T".into(), "8".into(), "--".into(), h.host.clone()], 15, 16, cancel, None)?;
        let mut found: Vec<String> = scan.stdout.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')).map(String::from).collect();
        if found.is_empty() {
            // Some servers offer key exchanges the Windows ssh-keyscan cannot do while ssh.exe can.
            found = self.scan_key_with_ssh(h, cancel)?;
        }
        let lines: Vec<&str> = found.iter().map(|s| s.as_str()).collect();
        let pick = lines.iter().find(|l| l.contains("ssh-ed25519")).or_else(|| lines.first()).ok_or_else(|| ToolError::new(ErrorCode::Unavailable, format!("could not read a host key from {}:{}", h.host, h.port)).with_details(json!({"stderr": scan.stderr})))?;
        let fp = self.run(program(&self.cfg.keygen_path, "ssh-keygen"), vec!["-l".into(), "-f".into(), "-".into()], 10, 4, cancel, Some(format!("{pick}\n")))?;
        let fingerprint = fp.stdout.split_whitespace().nth(1).filter(|f| f.starts_with("SHA256:")).map(String::from).ok_or_else(|| ToolError::new(ErrorCode::BackendError, "could not compute the host key fingerprint"))?;
        Ok((pick.to_string(), fingerprint))
    }
}

impl Inner {
    /// Let ssh learn the host key into a throwaway file (public-key login is off, so it stops after
    /// the key exchange). The real known_hosts is untouched until the user has confirmed the fingerprint.
    fn scan_key_with_ssh(&self, h: &HostEntry, cancel: &AtomicBool) -> Result<Vec<String>, ToolError> {
        let tmp = self.scratch.join(format!("known_hosts.scan.{}", h.alias));
        let _ = std::fs::remove_file(&tmp);
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let mut argv: Vec<String> = vec!["-F".into(), self.empty_config.to_string_lossy().to_string()];
        for o in [
            "BatchMode=yes",
            "StrictHostKeyChecking=accept-new",
            "HashKnownHosts=no",
            "PubkeyAuthentication=no",
            "PasswordAuthentication=no",
            "KbdInteractiveAuthentication=no",
            "IdentityAgent=none",
            "ForwardAgent=no",
            "ClearAllForwardings=yes",
            "ProxyCommand=none",
            "ProxyJump=none",
            "ControlMaster=no",
            "ControlPath=none",
            "LogLevel=ERROR",
        ] {
            argv.extend(["-o".into(), o.into()]);
        }
        argv.extend(["-o".into(), format!("ConnectTimeout={}", self.cfg.connect_timeout_s), "-o".into(), format!("UserKnownHostsFile={}", tmp.to_string_lossy()), "-o".into(), format!("GlobalKnownHostsFile={null}")]);
        argv.extend(["-T".into(), "-p".into(), h.port.to_string(), "-l".into(), h.user.clone(), "--".into(), h.host.clone(), "exit".into()]);
        // The login is expected to fail; only the key it learned matters.
        let _ = self.run(program(&self.cfg.ssh_path, "ssh"), argv, self.cfg.connect_timeout_s + 10, 8, cancel, None);
        let text = std::fs::read_to_string(&tmp).unwrap_or_default();
        let _ = std::fs::remove_file(&tmp);
        Ok(text.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')).map(String::from).collect())
    }
}

/// Turn ssh's stderr into the right error code.
fn ssh_failure(stderr: &str, alias: &str) -> ToolError {
    let low = stderr.to_ascii_lowercase();
    if low.contains("remote host identification has changed") || low.contains("host key for") && low.contains("has changed") {
        return ToolError::new(ErrorCode::HostKeyMismatch, format!("the host key of '{alias}' changed since you trusted it. This can mean someone is intercepting the connection. Do not continue; check with whoever runs the machine."));
    }
    if low.contains("host key verification failed") || low.contains("no matching host key") && low.contains("known_hosts") {
        return ToolError::new(ErrorCode::HostKeyUnknown, format!("the host key of '{alias}' is not trusted yet. Use ssh.trust_host, which shows its fingerprint for you to confirm."));
    }
    if low.contains("permission denied") || low.contains("authentication failed") || low.contains("no supported authentication") {
        return ToolError::new(ErrorCode::AuthFailed, format!("the server refused the key for '{alias}' (check identity_file in ssh.json and that the key is authorised on the server)"));
    }
    if ["could not resolve", "connection refused", "connection timed out", "no route to host", "network is unreachable", "connection reset", "operation timed out"].iter().any(|m| low.contains(m)) {
        return ToolError::new(ErrorCode::Unavailable, format!("cannot reach '{alias}': {}", stderr.lines().next().unwrap_or("").trim()));
    }
    ToolError::new(ErrorCode::BackendError, format!("ssh failed: {}", stderr.lines().next().unwrap_or("no message").trim()))
}

fn manifest(name: &str, description: &str, classes: Vec<PermClass>, props: Value, required: &[&str], read_only: bool, default_ms: u64, max_ms: u64) -> ToolManifest {
    ToolManifest {
        name: name.into(),
        title: name.into(),
        description: description.into(),
        classes,
        default_decision: Decision::Auto,
        input_schema: json!({"type": "object", "properties": props, "required": required}),
        timeout_default_ms: default_ms,
        timeout_max_ms: max_ms,
        annotations: Annotations { read_only, destructive: !read_only, idempotent: read_only, open_world: true },
        untrusted_output: true,
    }
}

impl ToolProvider for SshProvider {
    fn id(&self) -> &str {
        "ssh"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        let host = json!({"type": "string", "minLength": 1, "maxLength": 40});
        let cmd_timeout = json!({"type": "integer", "minimum": 1, "maximum": 300, "default": 30});
        vec![
            manifest("ssh.hosts", "List the configured remote machines (alias, user, address, whether its host key is trusted).", vec![PermClass::Read], json!({}), &[], true, 10_000, 30_000),
            manifest("ssh.test", "Connect to a configured host and confirm login works.", vec![PermClass::Remote],
                json!({"host": host.clone()}), &["host"], true, 30_000, 60_000),
            manifest("ssh.exec", "Run one command on a configured host. The remote shell parses it, so plain read-only commands ask once per session and everything else asks every time.", vec![PermClass::Remote, PermClass::Execute],
                json!({"host": host.clone(), "command": {"type": "string", "minLength": 1, "maxLength": 4096}, "timeout_s": cmd_timeout, "max_output_kb": {"type": "integer", "minimum": 1, "maximum": 512, "default": 64}}), &["host", "command"], false, 330_000, 330_000),
            manifest("ssh.trust_host", "Read a host's key and, after you confirm its fingerprint, remember it. Needed once per host.", vec![PermClass::Remote, PermClass::ExternalEffect],
                json!({"host": host.clone()}), &["host"], false, 60_000, 90_000),
            manifest("ssh.upload", "Copy a local file to a configured host (over sftp).", vec![PermClass::Remote, PermClass::Write, PermClass::ExternalEffect],
                json!({"host": host.clone(), "local_path": {"type": "string", "minLength": 1, "maxLength": 1024}, "remote_path": {"type": "string", "minLength": 1, "maxLength": 1024}, "timeout_s": {"type": "integer", "minimum": 1, "maximum": 900, "default": 120}}), &["host", "local_path", "remote_path"], false, 930_000, 930_000),
            manifest("ssh.download", "Copy a file from a configured host to a local folder Aqua may write to.", vec![PermClass::Remote, PermClass::Write],
                json!({"host": host, "remote_path": {"type": "string", "minLength": 1, "maxLength": 1024}, "local_path": {"type": "string", "minLength": 1, "maxLength": 1024}, "overwrite": {"type": "boolean", "default": false}, "timeout_s": {"type": "integer", "minimum": 1, "maximum": 900, "default": 120}}), &["host", "remote_path", "local_path"], false, 930_000, 930_000),
        ]
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        let s = &self.0;
        let cancel = AtomicBool::new(false);
        match tool {
            "ssh.hosts" => Ok(Classification { classes: vec![PermClass::Read], target: "ssh.json".into(), summary: "List configured hosts".into(), target_allowed: true }),
            "ssh.test" => {
                let h = s.host(args)?;
                Ok(Classification { classes: vec![PermClass::Remote], target: h.alias.clone(), summary: format!("Connect to {} ({}@{}:{})", h.alias, h.user, h.host, h.port), target_allowed: false })
            }
            "ssh.exec" => {
                let h = s.host(args)?;
                let cmd = args["command"].as_str().unwrap_or("");
                if cmd.contains('\0') {
                    return Err(ToolError::new(ErrorCode::InvalidArguments, "the command contains a NUL character"));
                }
                let mut classes = vec![PermClass::Remote, PermClass::Execute];
                match classify_remote(cmd) {
                    Risk::Refused => return Err(ToolError::new(ErrorCode::PermissionDenied, "this command would wipe or disable a machine and is never sent")),
                    Risk::Changes => classes.push(PermClass::ExternalEffect),
                    Risk::ReadOnly => {}
                }
                let shown = if cmd.chars().count() > 500 { format!("{}…", cmd.chars().take(500).collect::<String>()) } else { cmd.to_string() };
                // The target is the host, so a session grant for a read-only command covers only read-only commands on that host.
                Ok(Classification { classes, target: h.alias.clone(), summary: format!("Run `{shown}` on {} ({}@{})", h.alias, h.user, h.host), target_allowed: false })
            }
            "ssh.trust_host" => {
                let h = s.host(args)?;
                let (line, fp) = s.scan_key(h, &cancel)?;
                let kind = line.split_whitespace().nth(1).unwrap_or("key").to_string();
                s.pending.lock().unwrap_or_else(PoisonError::into_inner).insert(h.alias.clone(), (fp.clone(), line));
                Ok(Classification {
                    classes: vec![PermClass::Remote, PermClass::ExternalEffect],
                    target: format!("{} {}", h.alias, fp),
                    summary: format!("Trust the {kind} host key {fp} for {} ({}:{}). Compare it with the fingerprint the server shows before approving.", h.alias, h.host, h.port),
                    target_allowed: false,
                })
            }
            "ssh.upload" => {
                let h = s.host(args)?;
                let local = s.guard.resolve(args["local_path"].as_str().unwrap_or(""), Op::Read)?;
                valid_remote_path(args["remote_path"].as_str().unwrap_or("")).map_err(|m| ToolError::new(ErrorCode::InvalidArguments, m))?;
                if !local.path.is_file() {
                    return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{} is not a file", display(&local.path))));
                }
                let mut classes = vec![PermClass::Remote, PermClass::Write, PermClass::ExternalEffect];
                if local.sensitive {
                    classes.push(PermClass::Credential);
                }
                Ok(Classification { classes, target: h.alias.clone(), summary: format!("Upload {} to {}:{}", display(&local.path), h.alias, args["remote_path"].as_str().unwrap_or("")), target_allowed: false })
            }
            "ssh.download" => {
                let h = s.host(args)?;
                let local = s.guard.resolve(args["local_path"].as_str().unwrap_or(""), Op::Write)?;
                valid_remote_path(args["remote_path"].as_str().unwrap_or("")).map_err(|m| ToolError::new(ErrorCode::InvalidArguments, m))?;
                let mut classes = vec![PermClass::Remote, PermClass::Write];
                if local.executable_kind {
                    classes.push(PermClass::Execute);
                }
                if local.sensitive {
                    classes.push(PermClass::Credential);
                }
                Ok(Classification { classes, target: h.alias.clone(), summary: format!("Download {}:{} to {}", h.alias, args["remote_path"].as_str().unwrap_or(""), display(&local.path)), target_allowed: false })
            }
            other => Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
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
            .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("ssh task failed: {e}")))?.map(ProviderOutput::marked_private)
        })
    }
}

fn out(data: Value, text: String) -> Result<ProviderOutput, ToolError> {
    Ok(ProviderOutput { data, text, ..Default::default() })
}

impl Inner {
    fn dispatch(&self, tool: &str, args: &Value, cancel: &Arc<AtomicBool>) -> Result<ProviderOutput, ToolError> {
        match tool {
            "ssh.hosts" => self.hosts(),
            "ssh.test" => self.exec_inner(args, "echo aqua-ok", 30, 4, cancel).map(|r| {
                let ok = r.exit_code == Some(0) && r.stdout.contains("aqua-ok");
                ProviderOutput { data: json!({"reachable": ok}), text: if ok { "Login works.".into() } else { format!("Connected, but the test command failed (exit code {:?}).", r.exit_code) }, ..Default::default() }
            }),
            "ssh.exec" => {
                let timeout = args["timeout_s"].as_u64().unwrap_or(30).clamp(1, 300);
                let cap = args["max_output_kb"].as_u64().unwrap_or(64).clamp(1, 512) as usize;
                let cmd = args["command"].as_str().unwrap_or("");
                // The command is judged again here: nothing between classify and call is trusted.
                if classify_remote(cmd) == Risk::Refused {
                    return Err(ToolError::new(ErrorCode::PermissionDenied, "this command would wipe or disable a machine and is never sent"));
                }
                let r = self.exec_inner(args, cmd, timeout, cap, cancel)?;
                let mut text = format!("exit code {}", r.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into()));
                if !r.stdout.is_empty() {
                    text.push_str(&format!("\n--- stdout ---\n{}", r.stdout));
                }
                if !r.stderr.is_empty() {
                    text.push_str(&format!("\n--- stderr ---\n{}", r.stderr));
                }
                if r.truncated {
                    text.push_str(&format!("\n… output cut at {cap} KB"));
                }
                out(json!({"exit_code": r.exit_code, "stdout": r.stdout, "stderr": r.stderr, "truncated": r.truncated, "duration_ms": r.duration_ms}), text)
            }
            "ssh.trust_host" => self.trust(args, cancel),
            "ssh.upload" => self.scp(args, true, cancel),
            "ssh.download" => self.scp(args, false, cancel),
            other => Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
        }
    }

    fn hosts(&self) -> Result<ProviderOutput, ToolError> {
        let rows: Vec<Value> = self.cfg.hosts.iter().map(|h| json!({"alias": h.alias, "user": h.user, "host": h.host, "port": h.port, "description": h.description, "host_key_trusted": self.known_host_line(h)})).collect();
        let text = rows.iter().map(|r| format!("{} = {}@{}:{}{}", r["alias"].as_str().unwrap_or(""), r["user"].as_str().unwrap_or(""), r["host"].as_str().unwrap_or(""), r["port"], if r["host_key_trusted"].as_bool().unwrap_or(false) { "" } else { "  (host key not trusted yet)" })).collect::<Vec<_>>().join("\n");
        out(json!({ "hosts": rows }), if text.is_empty() { "No hosts are configured. Add them to data/tools/ssh.json.".into() } else { text })
    }

    fn exec_inner(&self, args: &Value, command: &str, timeout_s: u64, cap_kb: usize, cancel: &AtomicBool) -> Result<crate::term::RunResult, ToolError> {
        let h = self.host(args)?;
        let r = self.run(program(&self.cfg.ssh_path, "ssh"), self.ssh_args(h, command), timeout_s + self.cfg.connect_timeout_s, cap_kb, cancel, None)?;
        if r.cancelled {
            return Err(ToolError::new(ErrorCode::Cancelled, "the run was cancelled and the connection was closed"));
        }
        if r.timed_out {
            let within = if r.limit_is_the_calls { r.limit_words() } else { format!("{timeout_s} s") };
            return Err(ToolError::new(ErrorCode::Timeout, format!("no result within {within}; the connection was closed")).with_details(json!({"stdout": r.stdout})));
        }
        // ssh itself exits with 255 when it fails; any other code is the remote command's.
        if r.exit_code == Some(255) || r.exit_code.is_none() {
            return Err(ssh_failure(&r.stderr, &h.alias));
        }
        Ok(r)
    }

    fn trust(&self, args: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        let h = self.host(args)?;
        let approved = self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&h.alias).ok_or_else(|| ToolError::new(ErrorCode::Conflict, "no fingerprint was shown for this host; run the request again"))?;
        // Scan again: what is written must be the key the user saw.
        let (line, fp) = self.scan_key(h, cancel)?;
        if fp != approved.0 {
            return Err(ToolError::new(ErrorCode::HostKeyMismatch, format!("the host key changed between the prompt and now (was {}, now {fp}); nothing was trusted", approved.0)));
        }
        if self.known_host_line(h) {
            let existing = std::fs::read_to_string(&self.known_hosts).unwrap_or_default();
            if existing.lines().any(|l| l.trim() == line.trim()) {
                return out(json!({"trusted": true, "fingerprint": fp, "already": true}), format!("The host key {fp} was already trusted."));
            }
            return Err(ToolError::new(ErrorCode::HostKeyMismatch, format!("'{}' already has a different trusted key. Remove its line from data/tools/known_hosts yourself if the server was reinstalled.", h.alias)));
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.known_hosts).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("cannot write known_hosts: {e}")))?;
        writeln!(f, "{}", line.trim()).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("cannot write known_hosts: {e}")))?;
        out(json!({"trusted": true, "fingerprint": fp, "already": false}), format!("Trusted host key {fp} for {}.", h.alias))
    }

    fn scp(&self, args: &Value, upload: bool, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        let h = self.host(args)?;
        let remote = args["remote_path"].as_str().unwrap_or("");
        valid_remote_path(remote).map_err(|m| ToolError::new(ErrorCode::InvalidArguments, m))?;
        let timeout = args["timeout_s"].as_u64().unwrap_or(120).clamp(1, 900);
        let local = self.guard.resolve(args["local_path"].as_str().unwrap_or(""), if upload { Op::Read } else { Op::Write })?;
        if upload {
            if !local.path.is_file() {
                return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{} is not a file", display(&local.path))));
            }
        } else if local.path.exists() && !args["overwrite"].as_bool().unwrap_or(false) {
            return Err(ToolError::new(ErrorCode::AlreadyExists, format!("{} already exists; set overwrite=true to replace it", display(&local.path))));
        }
        let mut argv = self.base_options();
        // `-s` forces the SFTP protocol, so the remote path is never handed to a remote shell.
        argv.extend(["-s".into(), "-q".into(), "-P".into(), h.port.to_string()]);
        argv.extend(self.identity_args(h));
        argv.push("--".into());
        let remote_spec = format!("{}@{}:{}", h.user, if h.host.contains(':') { format!("[{}]", h.host) } else { h.host.clone() }, remote);
        let local_spec = local.path.to_string_lossy().to_string();
        if upload {
            argv.extend([local_spec.clone(), remote_spec]);
        } else {
            argv.extend([remote_spec, local_spec.clone()]);
        }
        let r = self.run(program(&self.cfg.scp_path, "scp"), argv, timeout + self.cfg.connect_timeout_s, 16, cancel, None)?;
        if r.cancelled {
            return Err(ToolError::new(ErrorCode::Cancelled, "the transfer was cancelled"));
        }
        if r.timed_out {
            let within = if r.limit_is_the_calls { r.limit_words() } else { format!("{timeout} s") };
            return Err(ToolError::new(ErrorCode::Timeout, format!("the transfer took longer than {within} and was stopped")));
        }
        if r.exit_code != Some(0) {
            let e = ssh_failure(&r.stderr, &h.alias);
            return Err(if e.code == ErrorCode::BackendError { ToolError::new(ErrorCode::BackendError, format!("transfer failed: {}", r.stderr.lines().next().unwrap_or("no message").trim())) } else { e });
        }
        let bytes = std::fs::metadata(&local.path).map(|m| m.len()).unwrap_or(0);
        out(json!({"direction": if upload { "upload" } else { "download" }, "local": display(&local.path), "remote": remote, "bytes": bytes}), format!("{} {} bytes {} {}:{}.", if upload { "Uploaded" } else { "Downloaded" }, bytes, if upload { "to" } else { "from" }, h.alias, remote))
    }
}
