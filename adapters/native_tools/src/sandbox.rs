//! Sandbox tiers for the programs Aqua starts (cards R6-35 and R6-10, decision D-5 of 2026-10-08).
//!
//! Three tiers, from the least contained to the most:
//!
//! * `none`: a Job Object only (`contain`): limits on memory and processes, not a security boundary. A child runs with the person's rights.
//! * `low`: Low integrity with a stripped token (`contain_low`): it cannot write outside its scratch folder or change the registry, but it can read what the person can read
//!   and use the network.
//! * `mxc`: Microsoft's MXC process container (`wxc-exec`, MIT, signed by Microsoft): it reads only the folders it is granted, writes only its scratch folder, has no network and
//!   not the loopback address, and sees only the environment it is given. Measured in `scripts/bench/mxc_spike.py` (section 2r of the progress document).
//!
//! The rule of D-5: MXC where its probe says the native tier works on this machine without administrator rights; Low integrity where it does not; nothing elevated (the
//! launcher `wxc-host-prep.exe`, which needs administrator rights, is never run); and **off by default**: `default_tier` of `data/tools/term.json` is `none` until the person
//! chooses. A call can ask for more than the default (`isolated: true` is `low`), never for less.
//!
//! What is here: the tiers, the policy MXC is given (built by [`policy_json`], a plain function so that a test can read it), the probe at start-up ([`Mxc::with_runner`]),
//! the command that starts a program inside the container ([`Mxc::plan`]) and the clean-up afterwards. What is not: the launcher is never started by a test of this file
//! (the tests give it a fake program), and the things only a live run can say (whether `deniedPaths` wins over a grant of a parent folder, how the launcher passes stdin, whether
//! the contained process ends when the launcher is killed, which programs start with `ui.disable`) are for the live measurement. Two limits measured in the spike are kept
//! in the open: with any address allowed the container can still resolve names (the host's name service answers for it), so a program that may reach one address can send
//! data out in names it looks up; only "everything denied" closes that. And the loopback address is never allowed (it is every port on 127.0.0.1, Aqua's own API included).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How contained a program is. The order is the order of strength: a call can only move up it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    #[default]
    None,
    Low,
    Mxc,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::None => "none",
            Tier::Low => "low",
            Tier::Mxc => "mxc",
        }
    }
}

/// The settings of the MXC tier, in `data/tools/term.json` under `mxc`. Every field is optional; nothing here turns the tier on (that is `default_tier`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MxcConfig {
    /// The path of `wxc-exec.exe` (npm `@microsoft/mxc-sdk`, folder `package/bin/x64`). Empty: the environment variable `AQUA_MXC_EXE`. Neither: the tier is not available.
    /// Aqua does not download or install it.
    #[serde(default)]
    pub exe: String,
    /// Addresses the container may reach, each `1.2.3.4`, `1.2.3.4:443` or `10.1.0.0/16` (TCP; no port means every port). **Empty: no network at all, which is the only setting
    /// that also closes name lookups.** A range that includes the loopback address (`0.0.0.0/0` does) is refused.
    #[serde(default)]
    pub network_allow: Vec<String>,
    /// Folders the container may read besides its program's folder and the working folder.
    #[serde(default)]
    pub read_paths: Vec<String>,
    /// Switch off the window system for the container. `false` is what was measured (PowerShell does not start with it on); the clipboard is off and input injection is blocked
    /// either way.
    #[serde(default)]
    pub ui_disable: bool,
}

/// A program to start, and what it may reach.
#[derive(Debug, Clone)]
pub struct Launch {
    pub exe: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// The environment the program gets and no other.
    pub env: Vec<(String, String)>,
    /// Folders granted read-only.
    pub read_paths: Vec<PathBuf>,
    /// Folders granted read and write (the scratch folder).
    pub write_paths: Vec<PathBuf>,
    /// Aqua's own files and folders: denied by name besides not being granted.
    pub denied_paths: Vec<PathBuf>,
    /// How long it may run; `None` for a server that stays up.
    pub timeout: Option<Duration>,
}

/// What a tier that starts the program through a launcher gives back: the command to run in place of the program, and what to clean up when the run is over.
pub struct Wrapped {
    pub exe: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    /// The container's name (also the name of its policy file).
    pub container: String,
    /// Delete the container and the policy file. Dropped when the run is over; [`Cleanup::finish`] does it now.
    pub cleanup: Cleanup,
}

/// Deletes a container (MXC leaves an AppContainer profile behind for each container name, found in the spike) and its policy file.
pub struct Cleanup {
    launcher: PathBuf,
    container: String,
    policy: PathBuf,
    runner: Runner,
    done: bool,
}

impl Cleanup {
    pub fn finish(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        let _ = std::fs::remove_file(&self.policy);
        let args = ["--delete".to_string(), "--containername".to_string(), self.container.clone()];
        let _ = (self.runner)(&self.launcher, &args, Duration::from_secs(15));
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.finish();
    }
}

/// One tier. `wrap` gives the command that starts the program through the tier's launcher, or `None` when the tier starts the program itself (`none`, and `low`, which
/// `term` starts with `contain_low`).
pub trait Sandbox: Send + Sync {
    fn tier(&self) -> Tier;
    /// Can this tier run here now? `Err` says why not.
    fn available(&self) -> Result<(), String>;
    fn wrap(&self, launch: &Launch) -> Result<Option<Wrapped>, String>;
}

/// The Job Object only.
pub struct NoSandbox;

impl Sandbox for NoSandbox {
    fn tier(&self) -> Tier {
        Tier::None
    }
    fn available(&self) -> Result<(), String> {
        Ok(())
    }
    fn wrap(&self, _launch: &Launch) -> Result<Option<Wrapped>, String> {
        Ok(None)
    }
}

/// Low integrity with a stripped token.
pub struct LowIntegrity;

impl Sandbox for LowIntegrity {
    fn tier(&self) -> Tier {
        Tier::Low
    }
    fn available(&self) -> Result<(), String> {
        if cfg!(windows) {
            Ok(())
        } else {
            Err("Low integrity is only available on Windows".into())
        }
    }
    fn wrap(&self, _launch: &Launch) -> Result<Option<Wrapped>, String> {
        Ok(None)
    }
}

/// What the launcher printed and how it ended.
#[derive(Debug, Clone, Default)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs the launcher with arguments and a time limit. The real one starts a process; a test gives a closure (or a fake program, through [`process_runner`]).
pub type Runner = Arc<dyn Fn(&Path, &[String], Duration) -> Result<Output, String> + Send + Sync>;

/// The runner that starts the launcher as a process. It never inherits a window or a console, reads at most a megabyte of each stream, and stops the process at the limit.
pub fn process_runner() -> Runner {
    Arc::new(|exe: &Path, args: &[String], limit: Duration| run_to_end(exe, args, limit))
}

fn run_to_end(exe: &Path, args: &[String], limit: Duration) -> Result<Output, String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().map_err(|e| format!("could not start {}: {e}", exe.display()))?;
    let read_all = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(p) = pipe {
                let _ = p.take(1 << 20).read_to_end(&mut bytes);
            }
            String::from_utf8_lossy(&bytes).into_owned()
        })
    };
    let out = read_all(child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
    let err = read_all(child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {}
            Err(e) => return Err(format!("could not wait for {}: {e}", exe.display())),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{} did not finish in {} s and was stopped", exe.display(), limit.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok(Output { code: status.code(), stdout: out.join().unwrap_or_default(), stderr: err.join().unwrap_or_default() })
}

/// What `wxc-exec --probe` says about this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub tier: String,
    pub needs_dacl_augmentation: bool,
    pub warnings: Vec<String>,
}

/// Reads the probe's answer (JSON; any text before the first `{` or after the last `}` is ignored, as the spike did).
pub fn parse_probe(text: &str) -> Result<Probe, String> {
    let (start, end) = (text.find('{'), text.rfind('}'));
    let (Some(start), Some(end)) = (start, end) else {
        return Err("the probe printed no JSON".into());
    };
    let v: Value = serde_json::from_str(&text[start..=end]).map_err(|e| format!("the probe's answer is not JSON ({e})"))?;
    let tier = v.get("tier").and_then(Value::as_str).ok_or("the probe did not say which tier this host gets")?.to_string();
    let needs = v.get("needsDaclAugmentation").and_then(Value::as_bool).ok_or("the probe did not say whether administrator preparation is needed")?;
    let warnings = v.get("warnings").and_then(Value::as_array).map(|a| a.iter().filter_map(|w| w.as_str().map(String::from)).collect()).unwrap_or_default();
    Ok(Probe { tier, needs_dacl_augmentation: needs, warnings })
}

/// The native tier that needs no preparation is the only one used: the others ask for `wxc-host-prep.exe`, which needs administrator rights (D-5: nothing elevated).
pub fn usable(p: &Probe) -> Result<(), String> {
    if p.tier != "base-container" {
        return Err(format!("this host gets MXC's '{}' tier, which needs a one-time administrator preparation that Aqua does not do", p.tier));
    }
    if p.needs_dacl_augmentation {
        return Err("MXC would have to change folder permissions on this host (administrator rights), which Aqua does not do".into());
    }
    Ok(())
}

/// One address Aqua may allow: a CIDR (a single address is /32 or /128) and, for TCP, one port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetRule {
    pub cidr: String,
    pub port: Option<u16>,
}

fn same_prefix_u32(a: u32, b: u32, prefix: u8) -> bool {
    prefix == 0 || (a >> (32 - u32::from(prefix))) == (b >> (32 - u32::from(prefix)))
}

fn same_prefix_u128(a: u128, b: u128, prefix: u8) -> bool {
    prefix == 0 || (a >> (128 - u32::from(prefix))) == (b >> (128 - u32::from(prefix)))
}

/// Does the range `ip/prefix` include a loopback address (`127.0.0.1`, anything in `127.0.0.0/8`, `::1`, or `::ffff:127.0.0.1`)?
fn covers_loopback(ip: IpAddr, prefix: u8) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || same_prefix_u32(u32::from(v4), 0x7f00_0001, prefix),
        IpAddr::V6(v6) => v6.is_loopback() || same_prefix_u128(u128::from(v6), 1, prefix) || same_prefix_u128(u128::from(v6), 0xffff_7f00_0001, prefix),
    }
}

fn parse_one(text: &str) -> Result<NetRule, String> {
    let s = text.trim();
    let bad = |why: &str| format!("mxc.network_allow '{s}': {why}");
    let (ip, prefix, port): (IpAddr, u8, Option<u16>) = if let Ok(sa) = s.parse::<SocketAddr>() {
        (sa.ip(), if sa.is_ipv4() { 32 } else { 128 }, Some(sa.port()))
    } else if let Ok(ip) = s.parse::<IpAddr>() {
        (ip, if ip.is_ipv4() { 32 } else { 128 }, None)
    } else if let Some((addr, bits)) = s.split_once('/') {
        let ip: IpAddr = addr.trim().parse().map_err(|_| bad("that is not an address"))?;
        let max = if ip.is_ipv4() { 32 } else { 128 };
        let prefix: u8 = bits.trim().parse().map_err(|_| bad("the part after / is not a number"))?;
        if prefix > max {
            return Err(bad("the part after / is longer than the address"));
        }
        (ip, prefix, None)
    } else {
        return Err(bad("write an address (1.2.3.4), an address and port (1.2.3.4:443), or a range (10.1.0.0/16)"));
    };
    if port == Some(0) {
        return Err(bad("port 0 is not a port"));
    }
    let single = prefix == if ip.is_ipv4() { 32 } else { 128 };
    if single && ip.is_unspecified() {
        return Err(bad("the unspecified address is not a destination"));
    }
    if covers_loopback(ip, prefix) {
        return Err(bad("it includes the loopback address, which is never allowed (it is every port on this computer, Aqua's own API included)"));
    }
    Ok(NetRule { cidr: format!("{ip}/{prefix}"), port })
}

/// The addresses of `mxc.network_allow`, or the first one that is wrong.
pub fn parse_network_allow(entries: &[String]) -> Result<Vec<NetRule>, String> {
    entries.iter().map(|e| parse_one(e)).collect()
}

/// Quote one argument the way `CommandLineToArgvW` and the C runtime read it back.
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '"')) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                out.push(c);
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// A path as MXC is given it: without the `\\?\` prefix `canonicalize` adds.
fn plain(p: &Path) -> String {
    let s = p.to_string_lossy();
    s.strip_prefix(r"\\?\").map(|rest| if let Some(unc) = rest.strip_prefix(r"UNC\") { format!(r"\\{unc}") } else { rest.to_string() }).unwrap_or_else(|| s.into_owned())
}

fn unique(paths: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for p in paths {
        if !p.is_empty() && !seen.iter().any(|q| q.eq_ignore_ascii_case(&p)) {
            seen.push(p);
        }
    }
    seen
}

/// The policy MXC is given for one run (contract 1.0.0; the fields are the ones `scripts/bench/mxc_spike.py` used, checked against the SDK's `wire.d.ts`).
///
/// Whatever `rules` holds, the network is denied by default, ingress is denied, and the host loopback is denied: the person's addresses are the only holes, and the loopback
/// is never one. Reads: the program's own folder, the folders of `launch.read_paths` and `extra_read`. Writes: `launch.write_paths`. Aqua's own files are denied by name as well
/// as not being granted. Telemetry is off. The environment is exactly `launch.env`.
pub fn policy_json(rules: &[NetRule], ui_disable: bool, extra_read: &[PathBuf], launch: &Launch, container_id: &str) -> Value {
    let command_line = std::iter::once(quote_arg(&plain(&launch.exe))).chain(launch.args.iter().map(|a| quote_arg(a))).collect::<Vec<_>>().join(" ");
    let mut process = json!({
        "commandLine": command_line,
        "cwd": plain(&launch.cwd),
        "env": launch.env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
    });
    if let Some(t) = launch.timeout {
        process["timeout"] = json!(t.as_millis() as u64);
    }
    let read = unique(launch.exe.parent().map(plain).into_iter().chain(launch.read_paths.iter().map(|p| plain(p))).chain(extra_read.iter().map(|p| plain(p))));
    let write = unique(launch.write_paths.iter().map(|p| plain(p)));
    let denied = unique(launch.denied_paths.iter().map(|p| plain(p)));
    let allow: Vec<Value> = rules
        .iter()
        .map(|r| {
            let mut rule = json!({ "to": [{ "cidr": r.cidr }] });
            if let Some(port) = r.port {
                rule["ports"] = json!([{ "protocol": "tcp", "port": port }]);
            }
            rule
        })
        .collect();
    let mut egress = json!({ "default": "deny" });
    if !allow.is_empty() {
        egress["allow"] = Value::Array(allow);
    }
    json!({
        "version": "1.0.0",
        "containerId": container_id,
        "containment": "processcontainer",
        "lifecycle": { "destroyOnExit": true },
        "process": process,
        "filesystem": { "readonlyPaths": read, "readwritePaths": write, "deniedPaths": denied },
        "network": { "egress": egress, "ingress": { "default": "deny", "hostLoopback": "deny" } },
        "ui": { "disable": ui_disable, "clipboard": "none", "injection": false },
        "telemetry": { "enabled": false },
    })
}

/// The MXC tier: the launcher, what the probe said about this host, and the policy settings.
pub struct Mxc {
    exe: PathBuf,
    rules: Vec<NetRule>,
    ui_disable: bool,
    read_paths: Vec<PathBuf>,
    policy_dir: PathBuf,
    runner: Runner,
    ready: Result<Probe, String>,
    counter: AtomicU64,
}

impl Mxc {
    /// The tier as `term.json` describes it, probed now with the real launcher (read-only: `wxc-exec --probe`). Only called when the person chose the tier.
    pub fn from_config(cfg: &MxcConfig) -> Self {
        let named = if cfg.exe.trim().is_empty() { std::env::var("AQUA_MXC_EXE").unwrap_or_default() } else { cfg.exe.clone() };
        Self::with_runner(cfg, PathBuf::from(named.trim()), std::env::temp_dir().join("aqua_mxc"), process_runner())
    }

    /// The same with the launcher's path, the folder for policy files and the way to run the launcher given (a test gives a fake). The probe runs here, once.
    pub fn with_runner(cfg: &MxcConfig, exe: PathBuf, policy_dir: PathBuf, runner: Runner) -> Self {
        let rules = parse_network_allow(&cfg.network_allow);
        let ready = match &rules {
            _ if exe.as_os_str().is_empty() => Err("no MXC launcher is named (set mxc.exe in term.json, or the environment variable AQUA_MXC_EXE)".to_string()),
            Err(e) => Err(e.clone()),
            Ok(_) => probe(&exe, &runner),
        };
        Self {
            exe,
            rules: rules.unwrap_or_default(),
            ui_disable: cfg.ui_disable,
            read_paths: cfg.read_paths.iter().filter(|p| !p.trim().is_empty()).map(PathBuf::from).collect(),
            policy_dir,
            runner,
            ready,
            counter: AtomicU64::new(0),
        }
    }

    /// What the probe said, when it was usable.
    pub fn probe(&self) -> Option<&Probe> {
        self.ready.as_ref().ok()
    }

    /// Whether the tier can run here, and if not, why.
    pub fn available(&self) -> Result<(), String> {
        self.ready.as_ref().map(|_| ()).map_err(|e| e.clone())
    }

    /// The command that starts `launch` inside a container of its own, and the clean-up for afterwards. Writes the policy file; starts nothing.
    pub fn plan(&self, launch: &Launch) -> Result<Wrapped, String> {
        self.available()?;
        let n = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let container = format!("aqua-{}-{stamp}-{n}", std::process::id());
        // A folder that is not there cannot be granted (MXC refuses a policy that names one); a scratch folder always is.
        let exists = |ps: &[PathBuf]| ps.iter().filter(|p| p.exists()).cloned().collect::<Vec<_>>();
        let launch = Launch { read_paths: exists(&launch.read_paths), write_paths: exists(&launch.write_paths), denied_paths: exists(&launch.denied_paths), ..launch.clone() };
        let extra = exists(&self.read_paths);
        let policy = policy_json(&self.rules, self.ui_disable, &extra, &launch, &container);
        std::fs::create_dir_all(&self.policy_dir).map_err(|e| format!("could not make {}: {e}", self.policy_dir.display()))?;
        let file = self.policy_dir.join(format!("{container}.json"));
        std::fs::write(&file, serde_json::to_string_pretty(&policy).map_err(|e| e.to_string())?).map_err(|e| format!("could not write {}: {e}", file.display()))?;
        let cleanup = Cleanup { launcher: self.exe.clone(), container: container.clone(), policy: file.clone(), runner: self.runner.clone(), done: false };
        Ok(Wrapped { exe: self.exe.clone(), args: vec![file.to_string_lossy().into_owned()], env: launch.env.clone(), cwd: launch.cwd.clone(), container, cleanup })
    }
}

fn probe(exe: &Path, runner: &Runner) -> Result<Probe, String> {
    let out = runner(exe, &["--probe".to_string()], Duration::from_secs(20))?;
    if out.code != Some(0) {
        let said: String = out.stderr.lines().next().unwrap_or("").chars().take(160).collect();
        return Err(format!("the MXC probe failed (exit {:?}){}", out.code, if said.is_empty() { String::new() } else { format!(": {said}") }));
    }
    let p = parse_probe(&out.stdout)?;
    usable(&p)?;
    Ok(p)
}

impl Sandbox for Mxc {
    fn tier(&self) -> Tier {
        Tier::Mxc
    }
    fn available(&self) -> Result<(), String> {
        Mxc::available(self)
    }
    fn wrap(&self, launch: &Launch) -> Result<Option<Wrapped>, String> {
        self.plan(launch).map(Some)
    }
}

/// The tier a call runs at, and a note for the person when it is not the one wanted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub tier: Tier,
    pub note: Option<String>,
}

/// `asked` is what the call asks for (`isolated: true` is `Low`), `default` is `term.json`'s `default_tier`. The stronger of the two wins. MXC wanted and not usable here
/// (the probe said no, or no launcher is named) means Low integrity, said in the note: D-5's fallback, never "no sandbox".
pub fn choose(asked: Tier, default: Tier, mxc: Option<&Mxc>) -> Choice {
    match asked.max(default) {
        Tier::Mxc => match mxc.map(Mxc::available) {
            Some(Ok(())) => Choice { tier: Tier::Mxc, note: None },
            Some(Err(why)) => Choice { tier: Tier::Low, note: Some(format!("MXC is not usable here ({why}), so this ran at Low integrity")) },
            None => Choice { tier: Tier::Low, note: Some("MXC was asked for but is not set up, so this ran at Low integrity".into()) },
        },
        tier => Choice { tier, note: None },
    }
}

/// Put a mandatory integrity label on a file: Medium, with no-read-up and no-write-up. A process of Low integrity (a Low child, or anything in an AppContainer, which is Low
/// too) can then neither read nor write it, while the person's own programs (Medium) can, so the shell, the scripts and Aqua itself are not affected. It does not stop a
/// program of Medium integrity, which is what an ordinary approved program is.
///
/// Fails softly by design: the caller logs the error and carries on (a file system without permissions, or a file Aqua does not own, is not a reason to stop). A file that is
/// replaced by renaming another over it loses the label; the caller labels again when it writes.
///
/// Already labelled: nothing is done. Setting a label needs the right to change the owner (`WRITE_OWNER`), which the owner of a file does not always hold: the data folder
/// sits on a drive whose root gives Authenticated Users only Modify, and a token file is restricted to read and write. When the label is refused for that reason, the owner is
/// given that one right on this one file (an `icacls` grant of `WO`, which the owner could always give itself: it changes who can do what to nobody) and the label is tried once
/// more. `icacls <file> /remove:g <user>` takes the grant away again.
#[cfg(windows)]
pub fn label_no_read_up(path: &Path) -> Result<(), String> {
    const ERROR_ACCESS_DENIED: u32 = 5;
    if has_the_label(path) {
        return Ok(());
    }
    match set_label(path) {
        Ok(()) => Ok(()),
        Err((ERROR_ACCESS_DENIED, _)) => {
            // The account by its SID and the program by its full path (card OA1-04, SE-18): `%USERNAME%` is whatever the environment says, and `icacls` by name is the first one found.
            let sid = current_user_sid().map_err(|e| format!("could not label {}: access denied, and the account is not known to ask for the right to ({e})", path.display()))?;
            let mut cmd = std::process::Command::new(icacls_exe());
            cmd.arg(path).arg("/grant").arg(format!("*{sid}:(WO)")).stdin(std::process::Stdio::null());
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            let granted = cmd.output().map(|o| o.status.success()).unwrap_or(false);
            if !granted {
                return Err(format!("could not label {}: access denied, and the right to change the owner could not be given to {sid}", path.display()));
            }
            set_label(path).map_err(|(status, why)| if status == 0 { why } else { format!("{why} even with the right to change the owner") })
        }
        Err((_, why)) => Err(why),
    }
}

/// Is the file already labelled Medium with no-read-up and no-write-up?
#[cfg(windows)]
fn has_the_label(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{LABEL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated; the unused out-parameters are null; the descriptor is freed below.
    let status = unsafe { GetNamedSecurityInfoW(wide.as_ptr(), SE_FILE_OBJECT, LABEL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), &mut descriptor) };
    if status != 0 || descriptor.is_null() {
        return false;
    }
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: the descriptor was just returned and is alive; `text` is an out-pointer that is freed below.
    let converted = unsafe { ConvertSecurityDescriptorToStringSecurityDescriptorW(descriptor, SDDL_REVISION_1, LABEL_SECURITY_INFORMATION, &mut text, std::ptr::null_mut()) };
    let same = converted != 0 && !text.is_null() && {
        // SAFETY: the string is NUL-terminated by the call.
        let len = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
        let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
        s.contains("(ML;;NRNW;;;ME)")
    };
    // SAFETY: both were allocated by the calls above and are freed once.
    unsafe {
        if !text.is_null() {
            LocalFree(text as _);
        }
        LocalFree(descriptor as _);
    }
    same
}

/// Sets the label. `Err((status, why))`: the Windows error code (0 when it is not one) and the reason.
#[cfg(windows)]
fn set_label(path: &Path) -> Result<(), (u32, String)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{GetSecurityDescriptorSacl, ACL, LABEL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

    // "ML" a mandatory label, "NRNW" no read up and no write up, "ME" Medium.
    let sddl: Vec<u16> = "S:(ML;;NRNW;;;ME)".encode_utf16().chain(std::iter::once(0)).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: valid, NUL-terminated input and a valid out-pointer; the descriptor is freed below on every path.
    let built = unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut descriptor, std::ptr::null_mut()) };
    if built == 0 {
        return Err((0, format!("could not build the label: {}", std::io::Error::last_os_error())));
    }
    let result = (|| {
        let (mut present, mut defaulted) = (0i32, 0i32);
        let mut sacl: *mut ACL = std::ptr::null_mut();
        // SAFETY: the descriptor was just built and outlives the call; the out-pointers are valid.
        let got = unsafe { GetSecurityDescriptorSacl(descriptor, &mut present, &mut sacl, &mut defaulted) };
        if got == 0 || present == 0 || sacl.is_null() {
            return Err((0, "the label has no system ACL".to_string()));
        }
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call; the label ACL belongs to `descriptor`, which is alive.
        let status = unsafe { SetNamedSecurityInfoW(wide.as_ptr(), SE_FILE_OBJECT, LABEL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), sacl) };
        if status != 0 {
            return Err((status, format!("could not label {} (error {status})", path.display())));
        }
        Ok(())
    })();
    // SAFETY: allocated by the call above, freed once.
    unsafe {
        LocalFree(descriptor as _);
    }
    result
}

/// Other systems have file permissions of their own (`auth.rs` sets mode 0600): nothing to do here.
#[cfg(not(windows))]
pub fn label_no_read_up(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// The system's own `icacls.exe`, by its full path (card OA1-04, SE-18). Started by name, the program is the first `icacls.exe` the search finds: one in the folder the process runs from, or
/// earlier on the path, would run with Aqua's rights and be handed the name of a secret file. The system folder is asked of Windows, not of the environment.
#[cfg(windows)]
pub fn icacls_exe() -> PathBuf {
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = [0u16; 260];
    // SAFETY: the buffer is valid for `buf.len()` code units, which is what the call is told.
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return PathBuf::from(r"C:\Windows\System32\icacls.exe");
    }
    PathBuf::from(String::from_utf16_lossy(&buf[..n])).join("icacls.exe")
}

/// Not Windows: there is no `icacls`; the name is returned so that a caller that builds a command still has something to build it with.
#[cfg(not(windows))]
pub fn icacls_exe() -> PathBuf {
    PathBuf::from("icacls")
}

/// The security identifier of the account Aqua runs as (`S-1-5-21-...`), read from the process's own token. `icacls` takes it as `*S-1-5-21-...`, which names the account exactly
/// (a name from `%USERNAME%` can be anything the environment says, and is ambiguous between a local and a domain account of the same name).
#[cfg(windows)]
pub fn current_user_sid() -> Result<String, String> {
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = std::ptr::null_mut();
    // SAFETY: the pseudo-handle of this process is always valid; `token` is a valid out-pointer and is closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(format!("could not open the process token: {}", std::io::Error::last_os_error()));
    }
    let result = (|| {
        let mut needed = 0u32;
        // SAFETY: the first call only asks how big the answer is (a null buffer of length 0).
        unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            return Err(format!("could not size the token's user: {}", std::io::Error::last_os_error()));
        }
        // Eight-byte units: the answer holds pointers and is read in place, so the buffer must be aligned for them.
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: the buffer is at least `needed` bytes long and outlives the call.
        if unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), needed, &mut needed) } == 0 {
            return Err(format!("could not read the token's user: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: the call filled the buffer with a TOKEN_USER (aligned as above), whose SID points into the same buffer.
        let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        let mut text: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sid` is valid while `buf` is alive; `text` is an out-pointer freed below.
        if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 || text.is_null() {
            return Err(format!("could not write the SID as text: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: the string is NUL-terminated by the call, and freed once.
        let len = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
        let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
        unsafe { LocalFree(text as _) };
        Ok(s)
    })();
    // SAFETY: the handle was opened above and is closed once.
    unsafe { CloseHandle(token) };
    result
}

#[cfg(not(windows))]
pub fn current_user_sid() -> Result<String, String> {
    Err("not Windows: accounts here are not named by a SID".into())
}

/// Makes a new file that only the current account can open, **from the first byte**: it is created with its access list already set (full access for this account and nobody else,
/// nothing inherited), so there is no moment in which the folder's inherited list lets another account read it. Fails if the file exists (the caller removes a bad one first). On
/// other systems the file is made with mode 0600. Used for the two API token files (card OA1-04, SE-18): they used to be written with the inherited list and restricted afterwards.
#[cfg(windows)]
pub fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{LocalFree, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL};

    let sid = current_user_sid().map_err(std::io::Error::other)?;
    // D: a discretionary list; P: protected, so nothing is inherited; one entry: allow (A) all file rights (FA) to this account.
    let sddl: Vec<u16> = format!("D:P(A;;FA;;;{sid})").encode_utf16().chain(std::iter::once(0)).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: valid, NUL-terminated input and a valid out-pointer; the descriptor is freed below on every path.
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut descriptor, std::ptr::null_mut()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: descriptor, bInheritHandle: 0 };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is NUL-terminated; `attributes` and the descriptor it points to are alive for the call.
    let handle = unsafe { CreateFileW(wide.as_ptr(), GENERIC_WRITE, 0, &attributes, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut()) };
    let made = if handle == INVALID_HANDLE_VALUE { Err(std::io::Error::last_os_error()) } else { Ok(handle) };
    // SAFETY: allocated by the conversion above, freed once.
    unsafe { LocalFree(descriptor as _) };
    // SAFETY: the handle is a new, open file handle that nothing else owns; the file takes it over and closes it.
    let mut file = unsafe { std::fs::File::from_raw_handle(made? as _) };
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(windows))]
pub fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_sandbox_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn launch(dir: &Path) -> Launch {
        Launch {
            exe: dir.join("tool dir").join("python.exe"),
            args: vec!["-c".into(), "print(\"hi there\")".into()],
            cwd: dir.join("project"),
            env: vec![("PATH".into(), "C:\\Windows".into()), ("TEMP".into(), dir.join("scratch").to_string_lossy().into_owned())],
            read_paths: vec![dir.join("project")],
            write_paths: vec![dir.join("scratch")],
            denied_paths: vec![dir.join("aqua data")],
            timeout: Some(Duration::from_secs(30)),
        }
    }

    const PROBE_OK: &str = r#"{"tier":"base-container","needsDaclAugmentation":false,"warnings":[]}"#;

    /// A runner that records every call and answers the probe with `probe_answer`.
    fn recording(probe_answer: &'static str) -> (Runner, Arc<Mutex<Vec<Vec<String>>>>) {
        let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let runner: Runner = Arc::new(move |_exe: &Path, args: &[String], _limit: Duration| {
            seen.lock().unwrap().push(args.to_vec());
            Ok(Output { code: Some(0), stdout: if args.first().map(String::as_str) == Some("--probe") { probe_answer.to_string() } else { String::new() }, stderr: String::new() })
        });
        (runner, calls)
    }

    #[test]
    fn the_default_tier_is_none_and_the_tiers_order_by_strength() {
        assert_eq!(Tier::default(), Tier::None);
        assert_eq!(MxcConfig::default().network_allow.len(), 0);
        assert!(Tier::None < Tier::Low && Tier::Low < Tier::Mxc);
        assert_eq!(serde_json::from_str::<Tier>("\"mxc\"").unwrap(), Tier::Mxc);
        assert_eq!(serde_json::to_string(&Tier::Low).unwrap(), "\"low\"");
        assert!(serde_json::from_str::<MxcConfig>(r#"{"surprise": 1}"#).is_err(), "an unknown setting is an error, not silently ignored");
    }

    #[test]
    fn a_policy_denies_the_network_and_the_host_loopback_whatever_the_config_allows() {
        let d = temp("net");
        let none = policy_json(&[], false, &[], &launch(&d), "c1");
        assert_eq!(none["network"]["egress"], json!({ "default": "deny" }));
        assert_eq!(none["network"]["ingress"], json!({ "default": "deny", "hostLoopback": "deny" }));
        let rules = parse_network_allow(&["1.1.1.1:443".to_string(), "10.1.0.0/16".to_string()]).unwrap();
        let some = policy_json(&rules, false, &[], &launch(&d), "c2");
        assert_eq!(some["network"]["egress"]["default"], "deny");
        assert_eq!(some["network"]["ingress"], json!({ "default": "deny", "hostLoopback": "deny" }));
        assert_eq!(some["network"]["egress"]["allow"][0], json!({ "to": [{ "cidr": "1.1.1.1/32" }], "ports": [{ "protocol": "tcp", "port": 443 }] }));
        assert_eq!(some["network"]["egress"]["allow"][1], json!({ "to": [{ "cidr": "10.1.0.0/16" }] }), "no port means every port, and no ports key");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_network_rule_that_includes_the_loopback_address_is_refused_in_every_spelling() {
        for bad in ["127.0.0.1", "127.0.0.1:8767", "127.0.0.2", "127.0.0.0/8", "0.0.0.0/0", "126.0.0.0/7", "::1", "[::1]:443", "::/0", "::ffff:127.0.0.1", "0.0.0.0", "1.1.1.1:0"] {
            assert!(parse_network_allow(&[bad.to_string()]).is_err(), "{bad} must be refused");
        }
        for text in ["nonsense", "1.1.1.1/33", "1.1.1.1/x", ""] {
            assert!(parse_network_allow(&[text.to_string()]).is_err(), "{text:?} must be refused");
        }
        assert_eq!(parse_network_allow(&["9.9.9.9".to_string()]).unwrap(), vec![NetRule { cidr: "9.9.9.9/32".into(), port: None }]);
        assert_eq!(parse_network_allow(&["[2606:4700::1111]:443".to_string()]).unwrap(), vec![NetRule { cidr: "2606:4700::1111/128".into(), port: Some(443) }]);
    }

    #[test]
    fn the_policy_grants_reads_to_the_programs_folder_and_the_working_folder_writes_to_the_scratch_folder_and_denies_aquas_own_data() {
        let d = temp("fs");
        let l = launch(&d);
        let p = policy_json(&[], false, &[PathBuf::from("C:\\extra")], &l, "c3");
        let list = |v: &Value| v.as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect::<Vec<_>>();
        let read = list(&p["filesystem"]["readonlyPaths"]);
        assert_eq!(read[0], d.join("tool dir").to_string_lossy(), "the program's own folder first");
        assert!(read.contains(&d.join("project").to_string_lossy().into_owned()) && read.contains(&"C:\\extra".to_string()));
        assert_eq!(list(&p["filesystem"]["readwritePaths"]), vec![d.join("scratch").to_string_lossy().into_owned()]);
        assert_eq!(list(&p["filesystem"]["deniedPaths"]), vec![d.join("aqua data").to_string_lossy().into_owned()]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_policy_carries_the_environment_the_command_line_the_timeout_and_no_telemetry() {
        let d = temp("proc");
        let p = policy_json(&[], true, &[], &launch(&d), "c4");
        assert_eq!(p["version"], "1.0.0");
        assert_eq!(p["containment"], "processcontainer");
        assert_eq!(p["containerId"], "c4");
        assert_eq!(p["lifecycle"]["destroyOnExit"], true);
        assert_eq!(p["telemetry"]["enabled"], false);
        assert_eq!(p["ui"], json!({ "disable": true, "clipboard": "none", "injection": false }));
        let line = p["process"]["commandLine"].as_str().unwrap();
        assert!(line.starts_with('"') && line.contains("python.exe\" -c \"print(\\\"hi there\\\")\""), "quoted the way the program reads it back: {line}");
        assert_eq!(p["process"]["timeout"], 30_000);
        assert_eq!(p["process"]["env"][0], "PATH=C:\\Windows");
        let server = Launch { timeout: None, ..launch(&d) };
        assert!(policy_json(&[], false, &[], &server, "c5")["process"].get("timeout").is_none(), "a server that stays up has no time limit");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_probe_that_says_base_container_without_preparation_is_usable_and_every_other_answer_is_not() {
        assert!(usable(&parse_probe(PROBE_OK).unwrap()).is_ok());
        assert!(usable(&parse_probe(&format!("note before\n{PROBE_OK}\n")).unwrap()).is_ok(), "text around the JSON is ignored");
        let other = parse_probe(r#"{"tier":"appcontainer","needsDaclAugmentation":false}"#).unwrap();
        assert!(usable(&other).unwrap_err().contains("administrator"));
        let dacl = parse_probe(r#"{"tier":"base-container","needsDaclAugmentation":true,"warnings":["w"]}"#).unwrap();
        assert!(usable(&dacl).is_err() && dacl.warnings == vec!["w".to_string()]);
        for broken in ["", "no json here", "{\"tier\":\"base-container\"}", "{\"needsDaclAugmentation\":false}", "{not json}"] {
            assert!(parse_probe(broken).is_err(), "{broken:?}");
        }
    }

    #[test]
    fn a_tier_whose_probe_failed_is_not_available_and_a_run_that_wants_it_falls_back_to_low_integrity_with_a_note() {
        let d = temp("fallback");
        let (runner, calls) = recording(r#"{"tier":"isolation-session","needsDaclAugmentation":false}"#);
        let mxc = Mxc::with_runner(&MxcConfig::default(), PathBuf::from("wxc-exec.exe"), d.join("policies"), runner);
        assert!(mxc.available().is_err() && mxc.probe().is_none());
        assert_eq!(calls.lock().unwrap().len(), 1, "probed once, at start");
        assert!(mxc.plan(&launch(&d)).is_err(), "no container is made on a host that is not usable");
        let c = choose(Tier::None, Tier::Mxc, Some(&mxc));
        assert_eq!(c.tier, Tier::Low);
        assert!(c.note.unwrap().contains("Low integrity"));
        let unset = choose(Tier::Low, Tier::Mxc, None);
        assert_eq!(unset.tier, Tier::Low);
        assert!(unset.note.is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_default_is_no_sandbox_and_a_call_can_only_ask_for_more_than_the_default_never_less() {
        let off = choose(Tier::None, Tier::None, None);
        assert_eq!((off.tier, off.note), (Tier::None, None), "off by default");
        assert_eq!(choose(Tier::Low, Tier::None, None).tier, Tier::Low, "isolated: true is Low");
        assert_eq!(choose(Tier::None, Tier::Low, None).tier, Tier::Low, "a call that asks for less than the default gets the default");
        let d = temp("choose");
        let (runner, _) = recording(PROBE_OK);
        let mxc = Mxc::with_runner(&MxcConfig::default(), PathBuf::from("wxc-exec.exe"), d.join("p"), runner);
        assert_eq!(choose(Tier::Low, Tier::Mxc, Some(&mxc)).tier, Tier::Mxc);
        assert_eq!(choose(Tier::None, Tier::Mxc, Some(&mxc)), Choice { tier: Tier::Mxc, note: None });
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn with_no_launcher_named_or_a_bad_network_setting_the_tier_is_not_available_and_nothing_is_started() {
        let d = temp("nolauncher");
        let runner: Runner = Arc::new(|_: &Path, _: &[String], _: Duration| -> Result<Output, String> { panic!("the launcher must not be started") });
        let none = Mxc::with_runner(&MxcConfig::default(), PathBuf::new(), d.join("p"), runner.clone());
        assert!(none.available().unwrap_err().contains("AQUA_MXC_EXE"));
        let cfg = MxcConfig { network_allow: vec!["0.0.0.0/0".into()], ..MxcConfig::default() };
        let wide = Mxc::with_runner(&cfg, PathBuf::from("wxc-exec.exe"), d.join("p"), runner);
        assert!(wide.available().unwrap_err().contains("loopback"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_run_gets_a_policy_file_and_a_command_and_the_container_is_deleted_when_the_run_is_over() {
        let d = temp("plan");
        std::fs::create_dir_all(d.join("project")).unwrap();
        std::fs::create_dir_all(d.join("scratch")).unwrap();
        let (runner, calls) = recording(PROBE_OK);
        let cfg = MxcConfig { network_allow: vec!["1.1.1.1:443".into()], ..MxcConfig::default() };
        let mxc = Mxc::with_runner(&cfg, PathBuf::from("wxc-exec.exe"), d.join("policies"), runner);
        assert!(mxc.available().is_ok() && mxc.probe().unwrap().tier == "base-container");
        let wrapped = mxc.plan(&launch(&d)).unwrap();
        assert_eq!(wrapped.exe, PathBuf::from("wxc-exec.exe"));
        assert_eq!(wrapped.args.len(), 1);
        let file = PathBuf::from(&wrapped.args[0]);
        let policy: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(policy["containerId"], wrapped.container.as_str());
        assert_eq!(policy["network"]["egress"]["allow"][0]["to"][0]["cidr"], "1.1.1.1/32");
        let granted: Vec<String> = policy["filesystem"]["readwritePaths"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        assert_eq!(granted, vec![d.join("scratch").to_string_lossy().into_owned()]);
        assert!(policy["filesystem"]["deniedPaths"].as_array().unwrap().is_empty(), "a folder that is not there is left out");
        assert_eq!(calls.lock().unwrap().len(), 1, "making the plan does not start the launcher");
        let id = wrapped.container.clone();
        drop(wrapped);
        assert!(!file.exists(), "the policy file is removed");
        let all = calls.lock().unwrap().clone();
        assert_eq!(all.last().unwrap(), &vec!["--delete".to_string(), "--containername".to_string(), id]);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The test provides the launcher: a batch file that answers `--probe`, copies the policy it is given and records a `--delete`.
    #[cfg(windows)]
    #[test]
    fn a_fake_launcher_the_test_provides_is_probed_started_with_the_policy_file_and_asked_to_delete_the_container() {
        let d = temp("fake");
        std::fs::create_dir_all(d.join("project")).unwrap();
        std::fs::create_dir_all(d.join("scratch")).unwrap();
        let fake = d.join("fake_wxc.cmd");
        std::fs::write(
            &fake,
            "@echo off\r\nif \"%~1\"==\"--probe\" goto probe\r\nif \"%~1\"==\"--delete\" goto del\r\ncopy /y \"%~1\" \"%~dp0last_policy.json\" >nul\r\necho ran> \"%~dp0ran.txt\"\r\nexit /b 0\r\n:probe\r\necho {\"tier\":\"base-container\",\"needsDaclAugmentation\":false,\"warnings\":[]}\r\nexit /b 0\r\n:del\r\necho %2 %3> \"%~dp0deleted.txt\"\r\nexit /b 0\r\n",
        )
        .unwrap();
        let mxc = Mxc::with_runner(&MxcConfig::default(), fake.clone(), d.join("policies"), process_runner());
        assert!(mxc.available().is_ok(), "the fake answered the probe: {:?}", mxc.available());
        let wrapped = mxc.plan(&launch(&d)).unwrap();
        let out = std::process::Command::new(&wrapped.exe).args(&wrapped.args).output().unwrap();
        assert!(out.status.success());
        assert!(d.join("ran.txt").exists());
        let seen: Value = serde_json::from_str(&std::fs::read_to_string(d.join("last_policy.json")).unwrap()).unwrap();
        assert_eq!(seen["network"]["ingress"]["hostLoopback"], "deny");
        let id = wrapped.container.clone();
        drop(wrapped);
        let deleted = std::fs::read_to_string(d.join("deleted.txt")).unwrap();
        assert!(deleted.contains("--containername") && deleted.contains(&id), "{deleted}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Card OA1-04 (SE-18): the program that sets who can open a secret file is the system's own, and the account is a SID.
    #[cfg(windows)]
    #[test]
    fn icacls_is_the_system_one_by_full_path_and_the_account_is_a_sid() {
        let exe = icacls_exe();
        assert!(exe.is_absolute() && exe.exists(), "{}", exe.display());
        assert!(exe.file_name().is_some_and(|n| n.eq_ignore_ascii_case("icacls.exe")));
        assert!(exe.parent().is_some_and(|p| p.ends_with("System32") || p.ends_with("system32")), "{}", exe.display());
        let sid = current_user_sid().expect("the process has a user");
        assert!(sid.starts_with("S-1-") && sid.len() > 8 && sid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'), "{sid}");
        assert_eq!(current_user_sid().unwrap(), sid, "the same every time");
    }

    /// Card OA1-04 (SE-18): a file made by `write_private_file` has an access list of its own from the first byte: no entry inherited from the folder, no group, one account.
    #[cfg(windows)]
    #[test]
    fn a_private_file_is_made_with_one_account_in_its_access_list_and_nothing_inherited() {
        let d = temp("private");
        let f = d.join("token");
        write_private_file(&f, b"0123456789abcdef").unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"0123456789abcdef", "its owner reads it back");
        assert!(write_private_file(&f, b"again").is_err(), "an existing file is never overwritten here");
        assert_eq!(std::fs::read(&f).unwrap(), b"0123456789abcdef");
        let out = std::process::Command::new(icacls_exe()).arg(&f).output().expect("icacls runs");
        // `icacls` prints the path first; the path is not part of the list.
        let text = String::from_utf8_lossy(&out.stdout).replace(&*f.to_string_lossy(), "");
        let entries: Vec<&str> = text.lines().filter(|l| l.contains(":(") && !l.contains("Mandatory Label")).collect();
        assert_eq!(entries.len(), 1, "one entry in the list: {text}");
        assert!(!entries[0].contains("(I)"), "nothing is inherited from the folder: {text}");
        for group in ["Everyone", "Users", "Authenticated Users", "Administrators", "SYSTEM"] {
            assert!(!text.contains(group), "{group} can open it: {text}");
        }
        // The account that made it keeps the right to remove it.
        std::fs::remove_file(&f).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(windows)]
    #[test]
    fn a_labelled_file_can_still_be_read_and_written_and_removed_by_this_process() {
        let d = temp("label");
        let f = d.join("secret.txt");
        std::fs::write(&f, "one").unwrap();
        label_no_read_up(&f).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "one");
        std::fs::write(&f, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "two");
        label_no_read_up(&f).unwrap();
        assert!(label_no_read_up(&d.join("missing.txt")).is_err(), "a file that is not there is an error the caller can log");
        std::fs::remove_file(&f).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }
}
