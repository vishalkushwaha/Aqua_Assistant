//! Which programs `term.run` and `term.start` may launch.
//!
//! Programs are named, not typed as a command line: there is no shell, so pipes, redirects and
//! `&&` do nothing. The name must be a bare program found in a configured folder (or a full path
//! inside one), and only `.exe` and `.com` files run, which also rules out `.bat` and `.cmd`
//! scripts whose argument parsing has been used to smuggle commands. Shells, script hosts and
//! programs that fetch or run code from elsewhere are refused outright; interpreters run only
//! after a fresh approval every time.

use crate::fs::guard::{canonical, expand_env, within};
use aqua_tool_host::{ErrorCode, PermClass, ToolError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TermConfig {
    /// Folders programs are looked up in, in order. `%NAME%` is expanded.
    #[serde(default = "default_dirs")]
    pub path_dirs: Vec<String>,
    #[serde(default = "default_sessions")]
    pub max_sessions: usize,
    #[serde(default = "default_memory")]
    pub memory_mb: u64,
    #[serde(default = "default_procs")]
    pub max_processes: u32,
    /// More programs from the Windows system folder that may be run, besides the built-in list ([`SYSTEM_PROGRAMS`]).
    /// Names only, without `.exe`. Everything else in that folder is refused: it holds hundreds of programs, several of
    /// which format disks, shut the computer down, change accounts or reach other machines.
    #[serde(default)]
    pub extra_system_programs: Vec<String>,
    /// Also look for Python where it installs itself (`%LOCALAPPDATA%\Programs\Python\Python3*`, `%ProgramFiles%\Python3*` and
    /// their `Scripts` folders), so `python` and `pytest` are found without editing `path_dirs`. The Microsoft Store stub in
    /// `WindowsApps` is never used: it is not the interpreter, it opens the Store.
    #[serde(default = "default_true")]
    pub discover: bool,
    /// How contained programs are when a call does not ask for more (cards R6-35 and R6-10, decision D-5): `none` (the default: a Job Object only), `low` (Low integrity,
    /// what `isolated: true` asks for) or `mxc` (Microsoft's MXC container, used where its probe says it works here, Low integrity where it does not). A call can ask for more
    /// than this, never for less. See `sandbox`.
    #[serde(default)]
    pub default_tier: crate::sandbox::Tier,
    /// The same for the MCP servers the person added: `none` or `mxc` (`low` is read as `none`; a server cannot be started that way). With `mxc` and no usable container a
    /// server is not started.
    #[serde(default)]
    pub mcp_tier: crate::sandbox::Tier,
    /// The MXC tier's settings: the launcher, the addresses it may reach (none by default), extra read-only folders. Nothing here turns the tier on.
    #[serde(default)]
    pub mxc: crate::sandbox::MxcConfig,
    /// A `term.run` of a build or test program (see [`long_running_program`]) that is still running when its time is up is kept as a background job (`term.output` reads it,
    /// `term.stop` ends it) instead of being stopped (card OA4-03, TM-04). Off unless the person turns it on here or with `AQUA_TERM_AUTO_BACKGROUND=on`: a run that went on past its time used to
    /// end at it. A call can ask for it, or refuse it, with `background_on_timeout`, whatever this says.
    #[serde(default)]
    pub auto_background: bool,
    /// The lines a program prints are shown in the app while `term.run`, `term.shell` and `term.start` run (card OA7-04, TM-14): up to four pieces a second of at most 8 KB, masked like a result, to the event
    /// stream of the run and to nothing else (the model reads only the digest the call ends with). Off unless the person turns it on here or with `AQUA_TERM_LIVE_OUTPUT=on`.
    #[serde(default)]
    pub live_output: bool,
    /// The wider list of a project's own checks (card OA4-05, decision D-07): `go test|vet|build`, `dotnet test|build`, `pnpm|yarn|bun test`, `npm|pnpm|yarn|bun run test|lint|typecheck|build`,
    /// `tsc --noEmit` and `ruff check` ask once for the exact command in a trusted folder, as `cargo test` and `npm test` do (see [`super::extended`]). Off unless the person turns it on here or
    /// with `AQUA_TERM_PROJECT_CHECKS=extended`: until then those commands ask every time, as they always did.
    #[serde(default)]
    pub extended_project_checks: bool,
    /// One answer of "approve for this session" to a project check covers the same program and subcommand in the same folder, and not only the exact call (card OA4-05, task TM-16; see
    /// [`super::arity`]). Off unless the person turns it on here or with `AQUA_TERM_CHECK_GRANTS=on`: until then each different filter or package asks again.
    #[serde(default)]
    pub check_grants: bool,
    /// The journal of what commands changed (card OA7-03, TM-12; see `git/journal.rs`): a `term.run` or `term.shell` that may change files, started in a git repository inside the folders
    /// Aqua may use, is looked at before and after, the files it changed are listed at the end of its result, and `term.undo` (offered only then) and `fs.undo_run` put them back. Off
    /// unless the person turns it on here or with `AQUA_TERM_JOURNAL=on`: it costs a few git calls on each command, and it keeps small commits in the repository's own object store.
    #[serde(default)]
    pub journal: bool,
}

fn default_true() -> bool {
    true
}

fn default_dirs() -> Vec<String> {
    ["%SystemRoot%/System32", "%ProgramFiles%/Git/cmd", "%ProgramFiles%/nodejs", "%USERPROFILE%/.cargo/bin"].iter().map(|s| s.to_string()).collect()
}
fn default_sessions() -> usize {
    8
}
fn default_memory() -> u64 {
    1024
}
fn default_procs() -> u32 {
    32
}

impl Default for TermConfig {
    fn default() -> Self {
        Self {
            path_dirs: default_dirs(),
            max_sessions: 8,
            memory_mb: 1024,
            max_processes: 32,
            extra_system_programs: Vec::new(),
            discover: true,
            default_tier: crate::sandbox::Tier::None,
            mcp_tier: crate::sandbox::Tier::None,
            mxc: crate::sandbox::MxcConfig::default(),
            auto_background: false,
            live_output: false,
            extended_project_checks: false,
            check_grants: false,
            journal: false,
        }
    }
}

/// Is this a program that builds or tests a project, which takes minutes and not seconds the first time (card OA4-03, WF-08)? cargo, npm, npx, pnpm, yarn, bun, `python -m pytest|unittest`,
/// go, dotnet, mvn, gradle, make, cmake, ninja, msbuild. `term.run` gives these 300 s when the call names no time, and any other program 30 s. The class is the program, not what it is
/// asked: a command of these that ends at once is not made to wait, and one that hangs is the same hang at 300 s.
pub fn long_running_program(exe: &str, argv: &[String]) -> bool {
    let name = program_name(exe);
    match name.as_str() {
        "cargo" | "npm" | "npx" | "pnpm" | "yarn" | "bun" | "go" | "dotnet" | "mvn" | "gradle" | "make" | "cmake" | "ninja" | "msbuild" => true,
        n if n == "py" || n.starts_with("python") => {
            argv.first().map(|a| a.eq_ignore_ascii_case("-m")).unwrap_or(false) && matches!(argv.get(1).map(|a| a.to_ascii_lowercase()).as_deref(), Some("pytest" | "unittest"))
        }
        _ => false,
    }
}

/// The folders Python installs itself to, newest version first, each followed by its `Scripts` folder.
pub fn discover_dirs() -> Vec<PathBuf> {
    discover_dirs_in(std::env::var_os("LOCALAPPDATA").map(PathBuf::from), std::env::var_os("ProgramFiles").map(PathBuf::from))
}

/// [`discover_dirs`] with the two parent folders given (so a test can use a temporary one).
pub fn discover_dirs_in(local_app_data: Option<PathBuf>, program_files: Option<PathBuf>) -> Vec<PathBuf> {
    let mut found: Vec<(u32, PathBuf)> = Vec::new();
    for parent in [local_app_data.map(|p| p.join("Programs").join("Python")), program_files].into_iter().flatten() {
        let Ok(entries) = std::fs::read_dir(&parent) else { continue };
        for dir in entries.flatten().map(|e| e.path()) {
            let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
            // `Python313`, `Python39`: a real install has python.exe in the folder itself.
            let Some(digits) = name.strip_prefix("python3") else { continue };
            if !dir.is_dir() || !dir.join("python.exe").is_file() {
                continue;
            }
            let version = format!("3{digits}").parse::<u32>().unwrap_or(0);
            found.push((version, dir));
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut out = Vec::new();
    for (_, dir) in found {
        let scripts = dir.join("Scripts");
        out.push(dir);
        if scripts.is_dir() {
            out.push(scripts);
        }
    }
    out
}

/// `npm` and `npx` are `.cmd` shims, and a `.cmd` is never run (`cmd.exe` parses its arguments). What a shim starts is
/// `node.exe <nodejs>\node_modules\npm\bin\npm-cli.js`, which runs directly with no shell. Returns `None` when `name` is neither
/// (a path, or any other program), `Some(Err)` when Node.js or the script is not there.
pub fn resolve_script_host(cfg: &TermConfig, name: &str) -> Option<Result<(PathBuf, PathBuf), ToolError>> {
    if name.contains(['/', '\\']) {
        return None;
    }
    let lower = name.trim().to_ascii_lowercase();
    let base = lower.strip_suffix(".cmd").unwrap_or(&lower);
    let script = match base {
        "npm" => "npm-cli.js",
        "npx" => "npx-cli.js",
        _ => return None,
    };
    Some((|| {
        let node = resolve_exe(cfg, "node").map_err(|_| {
            ToolError::new(ErrorCode::NotFound, format!("'{base}' needs Node.js, and node.exe was not found in: {}", cfg.path_dirs.join(", ")))
        })?;
        let js = node.parent().map(|d| d.join("node_modules").join("npm").join("bin").join(script)).unwrap_or_default();
        if !js.is_file() {
            return Err(ToolError::new(ErrorCode::NotFound, format!("{} was not found next to node.exe", js.display())));
        }
        Ok((node, js))
    })())
}

/// The build and test commands of a project: `cargo check|test|build|clippy`, `npm test`, `python -m pytest|unittest`. They run
/// the project's own code like any build, so they still ask; in a trusted project folder the answer is remembered for the exact
/// command, instead of being asked again every time (they are the first thing a developer wants run).
///
/// Every form here is an **allowlist**: the subcommand comes first, and each argument must be one that is known not to change what is
/// executed or where it writes. A list of refused flags is not enough: `npm` reads abbreviated options (`--script-s=calc.exe` is
/// `--script-shell`), clap-style tools accept spellings the check does not know, and a project can redefine the short cargo aliases
/// (`t`, `b`, `c`) in its own `.cargo/config.toml`. Anything not listed simply asks every time, which is the safe way to be wrong.
pub fn is_project_check(exe: &str, argv: &[String]) -> bool {
    let name = program_name(exe);
    let lower: Vec<String> = argv.iter().map(|a| a.to_ascii_lowercase()).collect();
    match name.as_str() {
        "cargo" => matches!(lower.first().map(String::as_str), Some("check" | "test" | "build" | "clippy")) && cargo_args_are_plain(&lower[0], &lower[1..]),
        "npm" => matches!(lower.first().map(String::as_str), Some("test" | "t")) && lower[1..].iter().all(|a| a == "--silent" || a == "-s"),
        n if n == "py" || n.starts_with("python") => {
            lower.first().map(String::as_str) == Some("-m") && matches!(lower.get(1).map(String::as_str), Some("pytest" | "unittest")) && python_test_args_are_plain(&lower[2..])
        }
        _ => false,
    }
}

/// A value for a flag or a test filter: a name, a feature list, a lint, never an option or a path.
pub(super) fn plain_value(v: &str) -> bool {
    !v.is_empty() && !v.starts_with('-') && !v.contains(['/', '\\']) && !v.contains("..")
}

/// What may follow `cargo check|test|build|clippy`: flags that choose what to build or how loudly, never where cargo reads its
/// configuration or writes its output (`--config`, `--manifest-path`, `--target-dir`, `-Z`, `-C` are not here).
fn cargo_args_are_plain(sub: &str, args: &[String]) -> bool {
    const FLAGS: &[&str] = &[
        "--workspace", "--all", "--all-targets", "--lib", "--bins", "--tests", "--examples", "--benches", "--doc", "--release", "-r", "--no-run", "--quiet", "-q", "--verbose", "-v", "-vv", "--locked", "--frozen",
        "--offline", "--no-default-features", "--all-features", "--no-fail-fast",
    ];
    const WITH_VALUE: &[&str] = &["-p", "--package", "--exclude", "--bin", "--test", "--example", "--bench", "--features", "-f", "--message-format", "--color", "--jobs", "-j", "--profile"];
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            return cargo_passthrough_is_plain(sub, &args[i + 1..]);
        }
        if FLAGS.contains(&a) {
            i += 1;
            continue;
        }
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) => (f, Some(v)),
            None => (a, None),
        };
        if WITH_VALUE.contains(&flag) {
            let value = match inline {
                Some(v) => v,
                None => {
                    i += 1;
                    args.get(i).map(String::as_str).unwrap_or("")
                }
            };
            if !plain_value(value) {
                return false;
            }
            i += 1;
            continue;
        }
        // `cargo test <name>`: a test name filter.
        if sub == "test" && !a.starts_with('-') && plain_value(a) {
            i += 1;
            continue;
        }
        return false;
    }
    true
}

/// After `--`: for `test` the harness's own flags and test names; for `clippy` lint levels (`-D warnings`). Anything else, a compiler flag
/// such as `-C linker=...` among them, is not a project check.
fn cargo_passthrough_is_plain(sub: &str, rest: &[String]) -> bool {
    let mut it = rest.iter().map(String::as_str);
    match sub {
        "test" => {
            const OK: &[&str] = &["--nocapture", "--show-output", "--ignored", "--include-ignored", "--exact", "--quiet", "-q", "--list"];
            while let Some(a) = it.next() {
                let threads = a.strip_prefix("--test-threads=").is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
                    || (a == "--test-threads" && it.next().is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())));
                if !(OK.contains(&a) || threads || (!a.starts_with('-') && plain_value(a))) {
                    return false;
                }
            }
            true
        }
        "clippy" => {
            let lint = |l: &str| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':'));
            while let Some(a) = it.next() {
                let level = a.len() > 2 && matches!(&a[..2], "-d" | "-w" | "-a" | "-f") && lint(&a[2..]);
                if !(level || (matches!(a, "-d" | "-w" | "-a" | "-f") && it.next().is_some_and(lint))) {
                    return false;
                }
            }
            true
        }
        _ => false,
    }
}

/// What may follow `python -m pytest|unittest`. Unlike cargo and npm, which are checked by what they refuse, this is an allowlist:
/// `-p` and `-c` load a plugin or a config of the caller's choosing, `--rootdir`, `-o`, `--pyargs` and `@file` change what is
/// collected or read, and `--junitxml`, `--basetemp` or `--log-file` write where the file guard never looked. A test path must
/// stay inside the folder (relative, no `..`).
fn python_test_args_are_plain(args: &[String]) -> bool {
    const FLAGS: &[&str] = &[
        "-q", "-qq", "-v", "-vv", "-x", "-s", "-l", "-b", "-f", "-ra", "-rA", "-rf", "-rfE", "--exitfirst", "--failfast", "--buffer", "--lf", "--ff", "--co", "--collect-only", "--no-header",
        "discover",
    ];
    const WITH_VALUE: &[&str] = &["-k", "--maxfail", "--tb"];
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if FLAGS.contains(&a.as_str()) {
            continue;
        }
        let name = a.split_once('=').map_or(a.as_str(), |(n, _)| n);
        if WITH_VALUE.contains(&name) {
            // `-k expr` and `--tb style` take the next argument; `--tb=short` carries its own.
            if !a.contains('=') && it.next().is_none() {
                return false;
            }
            continue;
        }
        // A test path or id (`tests/test_a.py::test_b`, `pkg.tests`); a lone `:` would be a drive such as `C:x`.
        let local_path = !a.starts_with(['-', '@', '~']) && !looks_absolute(a) && !has_parent_segment(a) && !a.replace("::", "").contains(':');
        if !local_path {
            return false;
        }
    }
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExeKind {
    /// An ordinary program: it asks before it runs.
    Plain,
    /// Runs arbitrary code from its arguments or files: it always asks.
    Interpreter,
}

/// Programs that are never run through these tools, with the reason shown to the model.
const BLOCKED: &[(&str, &str)] = &[
    ("cmd", "cmd.exe is a shell"),
    ("powershell", "PowerShell is a shell"),
    ("pwsh", "PowerShell is a shell"),
    ("powershell_ise", "PowerShell is a shell"),
    ("wsl", "wsl.exe starts a Linux shell"),
    ("bash", "bash is a shell"),
    ("sh", "sh is a shell"),
    ("zsh", "zsh is a shell"),
    ("conhost", "conhost starts a console host"),
    ("mshta", "mshta runs scripts"),
    ("wscript", "Windows Script Host runs scripts"),
    ("cscript", "Windows Script Host runs scripts"),
    ("regsvr32", "regsvr32 loads code from files and URLs"),
    ("rundll32", "rundll32 loads and runs library code"),
    ("certutil", "certutil downloads and decodes files"),
    ("bitsadmin", "bitsadmin downloads files"),
    ("msiexec", "msiexec installs packages"),
    ("installutil", "installutil runs .NET installers"),
    ("regasm", "regasm registers .NET code"),
    ("regsvcs", "regsvcs registers .NET code"),
    ("forfiles", "forfiles runs a command per file"),
    ("pcalua", "pcalua launches programs"),
    ("schtasks", "scheduled tasks are not managed through these tools"),
    ("at", "scheduled tasks are not managed through these tools"),
    ("sc", "services are not managed through these tools"),
    ("reg", "the registry is not edited through these tools"),
    ("regedit", "the registry is not edited through these tools"),
    ("net", "net changes accounts and shares"),
    ("net1", "net changes accounts and shares"),
    ("netsh", "netsh changes network and firewall settings"),
    ("runas", "runas starts programs as another user"),
    ("psexec", "psexec runs programs on other machines"),
    ("wmic", "wmic runs system management commands"),
    ("taskkill", "use proc.kill, which checks what it stops"),
    ("shutdown", "shutting down or restarting is not allowed"),
    ("format", "format erases disks"),
    ("diskpart", "diskpart changes disks"),
    ("bcdedit", "bcdedit changes boot settings"),
    ("takeown", "takeown changes file ownership"),
    ("icacls", "icacls changes file permissions"),
    ("cipher", "cipher can wipe free space"),
    ("vssadmin", "vssadmin deletes backups"),
    ("curl", "use http.request for web requests"),
    ("wget", "use http.request for web requests"),
    ("ssh", "use ssh.exec for remote machines"),
    ("scp", "use ssh.* for remote machines"),
    ("sftp", "use ssh.* for remote machines"),
    ("ftp", "use http.request or ssh.* instead"),
    ("telnet", "telnet sends everything unencrypted"),
    ("explorer", "use win.app to open apps"),
    ("start", "there is no shell to run start"),
    // Programs that read or write any path the caller names would sidestep the path checks of the fs tools.
    ("robocopy", "use the fs tools, which check every path"),
    ("xcopy", "use the fs tools, which check every path"),
    ("findstr", "use fs.find, which skips secret files"),
    ("more", "use fs.read, which checks the path"),
    ("sort", "use fs.read, which checks the path"),
    ("tar", "use the fs tools, which check every path"),
    ("expand", "use the fs tools, which check every path"),
    ("extrac32", "use the fs tools, which check every path"),
    ("makecab", "use the fs tools, which check every path"),
    ("esentutl", "esentutl copies locked system files"),
    ("replace", "use the fs tools, which check every path"),
    ("fsutil", "fsutil changes file system settings"),
    ("comp", "use fs.read, which checks the path"),
    ("fc", "use fs.read, which checks the path"),
    ("print", "print reads any file"),
    ("cacls", "cacls changes file permissions"),
    ("wevtutil", "event logs are read through the log tools"),
    ("cmdkey", "cmdkey manages stored credentials"),
    ("vaultcmd", "vaultcmd manages stored credentials"),
    ("klist", "klist shows Kerberos tickets"),
    ("dsquery", "directory queries are not run through these tools"),
    ("ntdsutil", "ntdsutil manages the directory database"),
    // Windows programs that load or run code from somewhere else, install it, fetch it, or change the system's
    // security settings. Each one has been used to run code without a script or a shell.
    ("cmstp", "cmstp installs and runs code from a setup file"),
    ("odbcconf", "odbcconf loads libraries"),
    ("mavinject", "mavinject injects a library into a running program"),
    ("certreq", "certreq sends requests and fetches files"),
    ("hh", "hh runs scripts inside help files"),
    ("diskshadow", "diskshadow runs scripts and snapshots volumes"),
    ("setx", "setx changes environment variables for good"),
];

/// The programs of the Windows system folder that may be run: they only report on the machine and take no path that reads or
/// writes files. Network tools such as `ping` and `nslookup` are not here on purpose (the name they are given is a message
/// to whoever runs that server); add them in `term.json` under `extra_system_programs` if you want them.
pub const SYSTEM_PROGRAMS: &[&str] = &["hostname", "ipconfig", "whoami", "tasklist", "systeminfo", "netstat", "getmac", "arp", "driverquery", "quser"];

/// Is this the Windows system folder (`%SystemRoot%\System32`)? Compared after resolving links and case.
fn in_system_folder(path: &Path) -> bool {
    let Some(root) = std::env::var_os("SystemRoot").map(PathBuf::from) else { return false };
    let (Ok(sys), Some(parent)) = (canonical(&root.join("System32")), path.parent()) else { return false };
    canonical(parent).map(|p| p == sys).unwrap_or(false)
}

/// A program found in the Windows system folder must be on the list of allowed system programs.
fn check_system_program(cfg: &TermConfig, path: &Path) -> Result<(), ToolError> {
    if !in_system_folder(path) {
        return Ok(());
    }
    let name = program_name(&path.to_string_lossy());
    if SYSTEM_PROGRAMS.contains(&name.as_str()) || cfg.extra_system_programs.iter().any(|n| program_name(n) == name) {
        return Ok(());
    }
    Err(ToolError::new(
        ErrorCode::PermissionDenied,
        format!("'{name}' is in the Windows system folder and is not on the list of system programs that may be run ({}). Add it to extra_system_programs in term.json if you need it.", SYSTEM_PROGRAMS.join(", ")),
    ))
}

const INTERPRETERS: &[&str] = &[
    "python", "python3", "pythonw", "py", "node", "deno", "bun", "ruby", "perl", "php", "java", "javaw", "lua", "dotnet", "msbuild", "go", "rustc", "cargo-run", "npx", "pnpx", "bunx",
];

/// Where the subcommand of a developer tool is, read past the options in front of it, and `Err(why)` when something in front of it can change what the tool runs.
///
/// "The first argument that does not start with a dash" is not the subcommand: `cargo +stable build` starts with a toolchain, `cargo --config build.rustc-wrapper=x check` has the value
/// of an option first and `npm --prefix . install` has a folder first, and each of them was read as a word that is no subcommand, so the call was taken for an ordinary one.
/// So the options that may stand in front are a short allowlist (`harmless`: they take no value; `with_value`: they take a plain value, never a path or another option), and anything
/// else, or a `+toolchain`, is reported. `Ok(None)` is a call with no subcommand (`cargo --version`, `npm -v`).
fn tool_subcommand(argv: &[String], harmless: &[&str], with_value: &[&str]) -> Result<Option<usize>, &'static str> {
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        if a.starts_with('+') {
            return Err("a +toolchain in front makes the tool run another version of itself, which is not the one that was judged");
        }
        if !a.starts_with('-') || a == "-" {
            return Ok(Some(i));
        }
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (a, None),
        };
        if inline.is_none() && harmless.contains(&name) {
            i += 1;
        } else if with_value.contains(&name) {
            let value = match inline {
                Some(v) => v,
                None => {
                    i += 1;
                    argv.get(i).map(String::as_str).unwrap_or("")
                }
            };
            if !plain_value(value) {
                return Err("an option in front of the command has a value that is a path or another option, and what it changes cannot be read");
            }
            i += 1;
        } else {
            return Err("an option in front of the command can change where the tool reads its settings or what it runs, and this one is not known to be harmless");
        }
    }
    Ok(None)
}

/// The options that may stand in front of the subcommand of each tool (see `tool_subcommand`).
const CARGO_FRONT: (&[&str], &[&str]) = (&["-v", "-vv", "--verbose", "-q", "--quiet", "--offline", "--locked", "--frozen", "-V", "--version", "-h", "--help", "--list"], &["--color", "--manifest-path", "-p"]);
const NODE_PM_FRONT: (&[&str], &[&str]) = (
    &["-s", "--silent", "-q", "--quiet", "--json", "--no-color", "--no-progress", "--offline", "--long", "--parseable", "-g", "--global", "--verbose", "-d", "-dd", "-ddd", "-v", "-V", "--version", "-h", "--help"],
    &["--loglevel"],
);
const PIP_FRONT: (&[&str], &[&str]) = (&["-v", "-vv", "-vvv", "--verbose", "-q", "-qq", "-qqq", "--quiet", "--no-color", "--isolated", "--disable-pip-version-check", "--no-input", "-V", "--version", "-h", "--help"], &[]);

/// The subcommands of cargo that build or run the project's code (its build scripts, procedural macros, tests and binaries), and the short names the tool gives them (`r` is
/// `run`; the first version of this list had `b`, `c`, `d` and `t` and not `r`). `publish` and `package` build the crate to check it.
const CARGO_RUNS_CODE: &[&str] = &["run", "r", "build", "b", "test", "t", "bench", "check", "c", "clippy", "doc", "d", "install", "fix", "rustc", "rustdoc", "miri", "xtask", "publish", "package"];

/// The subcommands of npm, pnpm and yarn that only look. This is an allowlist because npm reads any unambiguous start of a command name (`npm inst` is `npm install`, `npm rum` is
/// `npm run`) and keeps a long list of short names for its commands: a list of the commands that run code is always one short, and `npm update`, `npm uninstall` and `npm pack` were
/// not on it.
const NODE_PM_LOOKS: &[&str] = &["ls", "list", "ll", "la", "view", "info", "show", "v", "search", "s", "find", "outdated", "why", "explain", "help", "whoami", "ping", "root", "bin", "prefix"];

/// The subcommands of pip that only look (the same reasoning: `pip uninstall` runs the uninstall code of a package, `pip download` the setup code).
const PIP_LOOKS: &[&str] = &["list", "show", "freeze", "check", "inspect", "index", "search", "help", "debug"];

/// Corepack's own commands (anything else it is given is the package manager to run).
const COREPACK_COMMANDS: &[&str] = &["enable", "disable", "prepare", "hydrate", "install", "pack", "up", "use", "cache", "init"];

/// Does this call to an otherwise ordinary developer tool run code that is in the arguments or in the project, rather
/// than just doing its job? `git status` is plain; `git -c core.fsmonitor=calc.exe status` runs a program, and
/// `cargo build` runs the build scripts and macros of whatever the project contains. Such a call asks every time, like an
/// interpreter, even when the tool's name is on the ordinary list. Returns why.
pub fn args_run_code(exe: &str, argv: &[String]) -> Option<&'static str> {
    let name = program_name(exe);
    let lower: Vec<String> = argv.iter().map(|a| a.to_ascii_lowercase()).collect();
    match name.as_str() {
        "git" => {
            // The subcommand, read past `-C dir` and the other options with a value: `-C repo status` is `status`, not `repo`.
            // Read exactly as written: git finds its own commands by their exact names, and anything else (`Status`, `STATUS`) is looked up as an alias, whose name it does not
            // mind the case of.
            let first_word = git_subcommand_index(argv).map(|i| argv[i].as_str()).unwrap_or("");
            // A name git does not have as its own may be an alias of the repository's configuration for a shell command, which git runs when the name is used.
            if !first_word.is_empty() && !GIT_SUBCOMMANDS.contains(&first_word) {
                return Some("this git command is not one of git's own: in a repository it can be an alias that runs a program");
            }
            // `-c` sets a value for this command (and can name a program); `-C` only changes the folder, which is checked and scanned where it is used. They differ by case only.
            let flagged = argv.iter().any(|a| a == "-c")
                || lower.iter().any(|a| {
                a.starts_with("--config-env")
                    || a.starts_with("--exec-path")
                    || a.starts_with("--upload-pack")
                    || a.starts_with("--receive-pack")
                    || a == "--exec"
                    || a.starts_with("--exec=")
                    || a.starts_with("ext::")
                    || a.contains("core.fsmonitor")
                    || a.contains("core.sshcommand")
                    || a.contains("core.pager")
                    || a.contains("core.editor")
                    || a.contains("core.hookspath")
                    || a.starts_with("alias.")
                    || a.contains("credential.helper")
                    || a.contains("diff.external")
                    || a.contains("filter.")
            });
            let runs = match first_word {
                "submodule" => lower.iter().any(|a| a == "foreach"),
                "bisect" => lower.iter().any(|a| a == "run"),
                "filter-branch" | "difftool" | "mergetool" | "daemon" => true,
                _ => false,
            };
            (flagged || runs).then_some("this git command runs a program named in its arguments or configuration")
        }
        "cargo" => {
            match tool_subcommand(argv, CARGO_FRONT.0, CARGO_FRONT.1) {
                Err(why) => return Some(why),
                Ok(Some(i)) if CARGO_RUNS_CODE.contains(&lower[i].as_str()) => return Some("cargo runs the build scripts and macros of the project"),
                _ => {}
            }
            // `--config` (a setting of the call itself, which can name a compiler wrapper, a runner or a program that supplies credentials) and `-Z` / `-C` (cargo's unstable flags) are read
            // by every subcommand, wherever they stand before a `--`.
            argv.iter().take_while(|a| a.as_str() != "--").any(|a| a == "--config" || a.starts_with("--config=") || a.starts_with("-Z") || a == "-C").then_some("a setting or unstable flag in the call itself can name a program that cargo runs")
        }
        "npm" | "pnpm" | "yarn" | "bun" => match tool_subcommand(argv, NODE_PM_FRONT.0, NODE_PM_FRONT.1) {
            Err(why) => Some(why),
            // `yarn` alone is `yarn install`; the others print their usage.
            Ok(None) => (name == "yarn" && !argv.iter().any(|a| matches!(a.as_str(), "--version" | "-v" | "-V" | "--help" | "-h"))).then_some("yarn with no command installs the packages and runs their scripts"),
            Ok(Some(i)) => (!NODE_PM_LOOKS.contains(&argv[i].as_str())).then_some("this package manager runs install and project scripts, and this command is not one known to only look"),
        },
        "pip" | "pip3" => match tool_subcommand(argv, PIP_FRONT.0, PIP_FRONT.1) {
            Err(why) => Some(why),
            Ok(None) => None,
            Ok(Some(i)) => (!PIP_LOOKS.contains(&argv[i].as_str())).then_some("pip runs the setup code of what it installs, and this command is not one known to only look"),
        },
        // The tools that start other versions of tools, or fetch them. `rustup run stable <program>` and the like are judged as the program they run (`judge_launched`); what is left is
        // their own work, which downloads and switches toolchains, runtimes and package managers.
        "rustup" => {
            // `rustup +nightly show`: the toolchain in front is rustup's own syntax.
            let args = if argv.first().is_some_and(|a| a.starts_with('+')) { &argv[1..] } else { argv };
            match tool_subcommand(args, &["-v", "--verbose", "-q", "--quiet", "-V", "--version", "-h", "--help"], &[]) {
                Err(why) => Some(why),
                Ok(None) => None,
                Ok(Some(i)) => {
                    let after = args.get(i + 1).map(String::as_str).unwrap_or("");
                    let looks = match args[i].as_str() {
                        "run" | "show" | "which" | "check" | "help" => true,
                        "toolchain" | "target" | "component" => after == "list",
                        _ => false,
                    };
                    (!looks).then_some("rustup downloads and switches toolchains and programs")
                }
            }
        }
        "volta" => match tool_subcommand(argv, &["--verbose", "--quiet", "-V", "--version", "-h", "--help"], &[]) {
            Err(why) => Some(why),
            Ok(None) => None,
            Ok(Some(i)) => (!matches!(argv[i].as_str(), "run" | "list" | "which" | "help")).then_some("volta downloads and switches runtimes and package managers"),
        },
        "corepack" => match tool_subcommand(argv, &["-v", "-V", "--version", "-h", "--help"], &[]) {
            Err(why) => Some(why),
            Ok(None) => None,
            // The package manager it names is run and judged as what it is; its own commands fetch and install them.
            Ok(Some(i)) => COREPACK_COMMANDS.contains(&argv[i].as_str()).then_some("corepack downloads and installs package managers"),
        },
        "make" | "nmake" | "gmake" | "ninja" | "cmake" | "gradle" | "mvn" | "ant" | "meson" => Some("a build tool runs the commands written in the project"),
        _ => None,
    }
}

/// Programs that run another program given in their arguments. The program they run is what is judged: `env curl ...` is `curl`, and a block list that reads only
/// the first name is read by the wrapper's name instead of by what runs (the Copilot CLI's allow list ran `env curl ... | env sh` because it did not unwrap `env`).
///
/// `rustup run <toolchain> <program>`, `volta run <program>`, `corepack <package manager>`, `npx <program>` and `pnpx <program>` are launchers too (card OA8-01, SE-02): `rustup run stable
/// powershell -c x` was judged by the name `rustup` and so was an ordinary program, while it starts the shell that is refused by name.
const LAUNCHERS: &[&str] = &[
    "env", "xargs", "timeout", "nohup", "nice", "ionice", "stdbuf", "setsid", "sudo", "doas", "taskset", "chroot", "flock", "busybox", "time", "command", "exec", "unbuffer", "watch", "parallel",
    "rustup", "volta", "corepack", "npx", "pnpx",
];

/// What a launcher runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launched {
    /// It runs nothing (a bare `env` only prints).
    Nothing,
    /// It runs something that cannot be read from here (`env -S "..."` splits a string into a command).
    Unclear,
    /// It runs this program with these arguments.
    Program(String, Vec<String>),
}

/// Read the program a launcher runs out of its arguments: the options of the launcher (and their values) and its own positional arguments come first.
pub fn launched_program(wrapper: &str, args: &[String]) -> Launched {
    match wrapper {
        "rustup" => return rustup_launches(args),
        "volta" => return volta_launches(args),
        "corepack" => return corepack_launches(args),
        "npx" | "pnpx" => return npx_launches(args),
        _ => {}
    }
    let value_options: &[&str] = match wrapper {
        "env" => &["-u", "--unset", "-C", "--chdir"],
        "xargs" => &["-I", "-L", "-n", "-P", "-s", "-d", "-E", "-a", "--max-args", "--max-procs", "--delimiter", "--arg-file", "--max-lines", "--max-chars", "--replace"],
        "timeout" => &["-s", "-k", "--signal", "--kill-after"],
        "nice" => &["-n", "--adjustment"],
        "ionice" => &["-c", "-n", "-p", "-P", "-u"],
        "stdbuf" => &["-i", "-o", "-e"],
        "sudo" | "doas" => &["-u", "-g", "-C", "-h", "-p", "-r", "-t", "-U", "-D", "-R"],
        "watch" => &["-n", "-d", "--interval"],
        _ => &[],
    };
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            i += 1;
            break;
        }
        if wrapper == "env" && (a.starts_with("-S") || a.starts_with("--split-string")) {
            return Launched::Unclear;
        }
        if a.len() > 1 && a.starts_with('-') {
            i += if value_options.contains(&a) { 2 } else { 1 };
            continue;
        }
        if wrapper == "env" && a.contains('=') && !a.starts_with('=') {
            i += 1;
            continue;
        }
        break;
    }
    // The positional arguments that come before the program.
    if matches!(wrapper, "timeout" | "taskset" | "chroot" | "flock") {
        i += 1;
    }
    match args.get(i) {
        None => Launched::Nothing,
        Some(program) => Launched::Program(program.clone(), args[i + 1..].to_vec()),
    }
}

/// What follows the options at `args[i..]` as the program a launcher runs.
fn program_at(args: &[String], i: usize) -> Launched {
    match args.get(i) {
        None => Launched::Nothing,
        Some(program) => Launched::Program(program.clone(), args[i + 1..].to_vec()),
    }
}

/// `rustup [+toolchain] run [--install] <toolchain> <program> [args]`. Every other rustup command is rustup's own work (`args_run_code` asks about it).
fn rustup_launches(args: &[String]) -> Launched {
    let mut i = 0;
    while args.get(i).is_some_and(|a| a.starts_with('+') || matches!(a.as_str(), "-v" | "--verbose" | "-q" | "--quiet")) {
        i += 1;
    }
    if args.get(i).map(String::as_str) != Some("run") {
        return Launched::Nothing;
    }
    i += 1;
    while args.get(i).is_some_and(|a| a.starts_with('-')) {
        // `--install` is the only option of `run` that stands alone; one that is not known may take the next word, and then the program cannot be told from its value.
        if args[i] != "--install" {
            return Launched::Unclear;
        }
        i += 1;
    }
    // The toolchain, then the program (a `--` between them is not the program).
    let at = i + 1 + usize::from(args.get(i + 1).map(String::as_str) == Some("--"));
    program_at(args, at)
}

/// `volta run [--node <v>] [--npm <v>] [--env NAME=value]... <program> [args]`.
fn volta_launches(args: &[String]) -> Launched {
    let mut i = 0;
    while args.get(i).is_some_and(|a| matches!(a.as_str(), "--verbose" | "--quiet")) {
        i += 1;
    }
    if args.get(i).map(String::as_str) != Some("run") {
        return Launched::Nothing;
    }
    i += 1;
    while let Some(a) = args.get(i).map(String::as_str) {
        if a == "--" {
            i += 1;
            break;
        }
        if !a.starts_with('-') {
            break;
        }
        i += match a {
            "--bundled-npm" | "--no-yarn" | "--no-npm" | "--no-pnpm" => 1,
            "--node" | "--npm" | "--pnpm" | "--yarn" | "--env" => 2,
            _ if a.contains('=') => 1,
            _ => return Launched::Unclear,
        };
    }
    program_at(args, i)
}

/// `corepack <package manager> [args]`. Its own commands (`enable`, `prepare`, `use`...) and its flags run nothing.
fn corepack_launches(args: &[String]) -> Launched {
    match args.first().map(String::as_str) {
        None => Launched::Nothing,
        Some(a) if a.starts_with('-') => if matches!(a, "-v" | "-V" | "--version" | "-h" | "--help") { Launched::Nothing } else { Launched::Unclear },
        Some(a) if COREPACK_COMMANDS.contains(&a) || a == "help" => Launched::Nothing,
        Some(_) => program_at(args, 0),
    }
}

/// `npx [options] <program> [args]` and `pnpx`, which fetch a package and run a program of it, or a program that is already on the path (npx runs the command through a shell,
/// so `npx cmd /c calc` starts `cmd`). The shell-string option (`-c`) and any option that is not known cannot be read past: the call asks every time, as an interpreter does.
fn npx_launches(args: &[String]) -> Launched {
    const FLAGS: &[&str] = &["-y", "--yes", "-n", "--no", "--no-install", "--ignore-existing", "-q", "--quiet", "--offline", "--prefer-offline", "--prefer-online", "--no-update-notifier", "--no-audit", "--no-fund"];
    const VALUES: &[&str] = &["-p", "--package", "--registry", "--prefix", "--cache", "--loglevel", "-w", "--workspace"];
    let mut i = 0;
    while let Some(a) = args.get(i).map(String::as_str) {
        if a == "--" {
            i += 1;
            break;
        }
        if !a.starts_with('-') || a == "-" {
            break;
        }
        if FLAGS.contains(&a) {
            i += 1;
        } else if VALUES.contains(&a) {
            i += 2;
        } else if a.starts_with("--") && a.contains('=') && !a.starts_with("--call") && !a.starts_with("--shell") {
            i += 1;
        } else {
            return Launched::Unclear;
        }
    }
    program_at(args, i)
}

/// Judge a program by what it runs when it is a launcher: refused when what it runs is refused (or when it is wrapped more than four deep), an interpreter when what it
/// runs is one or runs code from its arguments, and an interpreter (asks every time) when it cannot be read.
pub fn judge_launched(exe: &str, argv: &[String], kind: ExeKind) -> Result<ExeKind, ToolError> {
    let (mut name, mut args, mut kind) = (program_name(exe), argv.to_vec(), kind);
    for _ in 0..4 {
        if !LAUNCHERS.contains(&name.as_str()) {
            return Ok(kind);
        }
        match launched_program(&name, &args) {
            Launched::Nothing => return Ok(kind),
            Launched::Unclear => return Ok(ExeKind::Interpreter),
            Launched::Program(inner, rest) => {
                // Git is run directly or not at all: its safety options, the check of the paths it is given and the person's own rules about it apply to the program that is
                // run first, and a launcher in front of it would take all three away.
                if program_name(&inner) == "git" {
                    return Err(ToolError::new(ErrorCode::PermissionDenied, format!("'{name}' would run 'git': run git directly, so that the options and checks for git apply")));
                }
                match classify_exe(&inner) {
                    Err(e) => return Err(ToolError::new(ErrorCode::PermissionDenied, format!("'{name}' would run '{}': {}", program_name(&inner), e.message))),
                    Ok(ExeKind::Interpreter) => kind = ExeKind::Interpreter,
                    Ok(ExeKind::Plain) => {}
                }
                if args_run_code(&inner, &rest).is_some() {
                    kind = ExeKind::Interpreter;
                }
                name = program_name(&inner);
                args = rest;
            }
        }
    }
    // Wrapped deeper than the loop reads: not run, and not asked about either. A question would show the whole command, but what the checks cannot read they cannot apply
    // (git's options, the check of its paths, the person's rules): the review of the commit that asked found `env env env env env git status` got through that way.
    if LAUNCHERS.contains(&name.as_str()) {
        return Err(ToolError::new(ErrorCode::PermissionDenied, "this command is wrapped in more than four programs that run other programs: it is not run (the checks cannot read it)"));
    }
    Ok(kind)
}

/// The place named as git's hooks folder: the null device, which cannot hold a hook. (Not a folder that was made for the purpose: a folder in a shared place with a
/// name that can be known is a place where a hook could be put, and it would then run in every repository.)
pub const NO_HOOKS_PATH: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// The programs a launcher runs, outermost first, with their arguments (`env FOO=1 timeout 9 npm publish` gives `timeout 9 npm publish` and then `npm publish`): the
/// person's own rules are asked about each of them, so wrapping a command in `env` does not step around a rule about it.
pub fn launched_chain(exe: &str, argv: &[String]) -> Vec<(String, Vec<String>)> {
    let (mut name, mut args, mut out) = (program_name(exe), argv.to_vec(), Vec::new());
    for _ in 0..4 {
        if !LAUNCHERS.contains(&name.as_str()) {
            break;
        }
        match launched_program(&name, &args) {
            Launched::Program(inner, rest) => {
                name = program_name(&inner);
                out.push((inner, rest.clone()));
                args = rest;
            }
            _ => break,
        }
    }
    out
}

/// The subcommands of git that are its own: anything else is a name that a repository's configuration may have given to a shell command (`alias.name = !program`), which
/// git runs when the name is used.
const GIT_SUBCOMMANDS: &[&str] = &[
    "add", "am", "annotate", "apply", "archive", "bisect", "blame", "branch", "bundle", "cat-file", "check-attr", "check-ignore", "check-ref-format", "checkout", "cherry", "cherry-pick",
    "clean", "clone", "column", "commit", "commit-graph", "commit-tree", "config", "count-objects", "describe", "diff", "diff-files", "diff-index", "diff-tree", "difftool", "fast-export",
    "fast-import", "fetch", "filter-branch", "for-each-ref", "format-patch", "fsck", "gc", "grep", "hash-object", "help", "init", "interpret-trailers", "log", "ls-files", "ls-remote",
    "ls-tree", "maintenance", "merge", "merge-base", "mergetool", "mktag", "mktree", "mv", "name-rev", "notes", "pack-objects", "prune", "pull", "push", "range-diff", "read-tree", "rebase",
    "reflog", "remote", "repack", "replace", "rerere", "reset", "restore", "rev-list", "rev-parse", "revert", "rm", "shortlog", "show", "show-branch", "show-ref", "sparse-checkout", "stash",
    "status", "submodule", "switch", "symbolic-ref", "tag", "update-index", "update-ref", "var", "verify-commit", "verify-pack", "verify-tag", "version", "whatchanged", "worktree", "write-tree",
];

/// Where the subcommand is in the arguments of a git command: after the options that come before it (`-C dir`, `-c name=value`, `--git-dir dir` and the like).
pub fn git_subcommand_index(argv: &[String]) -> Option<usize> {
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        if !a.starts_with('-') {
            return Some(i);
        }
        i += if matches!(a, "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--super-prefix" | "--config-env") { 2 } else { 1 };
    }
    None
}

/// What a command does to something beyond running its own code (card OA8-01, SE-02): it destroys work that cannot be had back, or it changes another computer or someone else's
/// settings. The tools of Aqua that do the same things say so in their classes (`git.push` is `Network`, `Remote` and `ExternalEffect`, `git.reset` is `Destructive`), and those are
/// the classes that ask every time in every mode, full access included. `term.run` of the same git was only `Execute`, which Developer mode and full access let through unasked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    /// The classes that are added to `Execute`: `Destructive`, or `ExternalEffect` (with `Network` and `Remote` when another computer is reached).
    pub classes: Vec<PermClass>,
    /// What it does, in words for the question ("sends commits to another repository").
    pub why: String,
    /// The tools of Aqua that do the same with a backup or a clearer question, when there are some.
    pub safer: Vec<&'static str>,
}

impl Effect {
    fn new(classes: &[PermClass], why: &str, safer: &[&'static str]) -> Self {
        Self { classes: classes.to_vec(), why: why.to_string(), safer: safer.to_vec() }
    }

    /// The words put in front of the question: what it does, and the safer tool when there is one.
    pub fn note(&self) -> String {
        if self.safer.is_empty() {
            format!("[{}] ", self.why)
        } else {
            format!("[{}; safer: {}] ", self.why, self.safer.join(" or "))
        }
    }
}

/// The effects of several commands together (the segments of `term.shell`, a program and what it launches): every class of each, every reason once.
pub fn merge_effects(list: impl IntoIterator<Item = Effect>) -> Option<Effect> {
    let mut out: Option<Effect> = None;
    for e in list {
        let Some(o) = out.as_mut() else {
            out = Some(e);
            continue;
        };
        for c in e.classes {
            if !o.classes.contains(&c) {
                o.classes.push(c);
            }
        }
        if !o.why.contains(&e.why) {
            o.why = format!("{}; {}", o.why, e.why);
        }
        for s in e.safer {
            if !o.safer.contains(&s) {
                o.safer.push(s);
            }
        }
    }
    out
}

const DESTRUCTIVE: &[PermClass] = &[PermClass::Destructive];
const OUTSIDE: &[PermClass] = &[PermClass::ExternalEffect];
const REACHES: &[PermClass] = &[PermClass::Network, PermClass::Remote, PermClass::ExternalEffect];

/// The arguments of a git subcommand before a lone `--` (after which every word is a path), and those after it.
fn before_and_after_dashdash(rest: &[String]) -> (&[String], &[String]) {
    match rest.iter().position(|a| a == "--") {
        Some(p) => (&rest[..p], &rest[p + 1..]),
        None => (rest, &[]),
    }
}

/// Is the long option `--name` among these arguments, written out or shortened? Git (like every tool that uses its option parser) takes any start of a long option that no other
/// option of the command shares: `git reset --har` is `--hard`. A start that two options share is an error to git, and counting it here is the safe side.
fn long_flag(opts: &[String], name: &str) -> bool {
    opts.iter().any(|a| {
        let Some(body) = a.strip_prefix("--") else { return false };
        let body = body.split('=').next().unwrap_or("").to_ascii_lowercase();
        !body.is_empty() && name.starts_with(&body)
    })
}

/// Is the short option `-c` among these arguments, alone or in a cluster (`-fdx`)? Case matters: `-d` and `-D` are different options.
fn short_flag(opts: &[String], c: char) -> bool {
    opts.iter().any(|a| a.len() > 1 && a.starts_with('-') && !a.starts_with("--") && a[1..].contains(c))
}

/// The words of a command that are not options (`with_value` lists the options whose next word is theirs and not an operand).
fn operands<'a>(opts: &'a [String], with_value: &[&str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < opts.len() {
        let a = opts[i].as_str();
        if a.starts_with('-') && a.len() > 1 {
            i += if with_value.contains(&a) { 2 } else { 1 };
        } else {
            out.push(a);
            i += 1;
        }
    }
    out
}

/// Does `git config` with these arguments write a setting? Reading is `--get`, `--list` and the like, or one name; writing is a name and a value, or `--add`, `--unset`, `--edit`
/// and the other writing options, or (git 2.46 and later) the subcommands `set`, `unset` and `edit`.
fn git_config_writes(opts: &[String]) -> bool {
    let writes = ["add", "replace-all", "unset", "unset-all", "rename-section", "remove-section", "edit"].iter().any(|f| long_flag(opts, f)) || short_flag(opts, 'e');
    let reads = ["get", "get-all", "get-regexp", "get-urlmatch", "list", "get-color", "get-colorbool"].iter().any(|f| long_flag(opts, f)) || short_flag(opts, 'l');
    let words = operands(opts, &["-f", "--file", "--blob", "--default", "--type", "--comment"]);
    let new_style = words.first().is_some_and(|w| matches!(*w, "set" | "unset" | "edit" | "rename-section" | "remove-section"));
    writes || new_style || (!reads && words.len() >= 2)
}

/// Does `gc` get a `--prune` that is not `never`?
fn gc_prunes(opts: &[String]) -> bool {
    opts.iter().any(|a| {
        let Some(body) = a.strip_prefix("--") else { return false };
        let (name, value) = body.split_once('=').map_or((body, None), |(n, v)| (n, Some(v)));
        !name.is_empty() && "prune".starts_with(name) && !matches!(value, Some("never" | "false"))
    })
}

/// The effect of a git command, if it has one: the classes it adds to `Execute`, why, and which tool of Aqua does it with a backup. `None` for `status`, `log`, `diff`, `add`, `commit`
/// and the other ordinary work. `dir` is the folder the command runs in; with it, `git checkout <name>` is also read as a restore when `<name>` is a file there.
///
/// Every subcommand is read by its exact name, and the words after it by the options git itself accepts (including shortened long options). When it is in doubt it says yes: an extra
/// question costs a click, a missed one costs work or a push.
pub fn git_effect_in(argv: &[String], dir: Option<&Path>) -> Option<Effect> {
    let at = git_subcommand_index(argv)?;
    let sub = argv[at].to_ascii_lowercase();
    let rest = &argv[at + 1..];
    let (opts, after) = before_and_after_dashdash(rest);
    let has_word = |words: &[&str]| rest.iter().any(|a| words.contains(&a.as_str()));
    // The folder the command works in: where it runs, moved by each `-C <dir>` in front of the subcommand.
    let work = dir.map(|d| {
        let mut p = d.to_path_buf();
        for w in argv[..at].windows(2) {
            if w[0] == "-C" {
                p = p.join(&w[1]);
            }
        }
        p
    });
    match sub.as_str() {
        "push" => Some(Effect::new(REACHES, "sends commits to another repository", &["git.push (never forced)"])),
        "reset" => {
            let moves = ["hard", "merge", "keep", "soft", "mixed"].iter().any(|m| long_flag(opts, m));
            // `git reset` and `git reset HEAD -- file` only take things out of the staging area; a name that is not HEAD may be a commit, and then the branch moves. (A name that is a
            // file of the folder is a path: git refuses a name that is both.)
            let is_a_file = |w: &str| work.as_deref().is_some_and(|d| d.join(w).exists());
            let unstages = !after.is_empty() || operands(opts, &[]).iter().all(|w| matches!(*w, "HEAD" | "@") || is_a_file(w));
            if long_flag(opts, "hard") || long_flag(opts, "merge") {
                Some(Effect::new(DESTRUCTIVE, "throws away uncommitted changes", &["git.reset (keeps a backup ref)"]))
            } else if moves || !unstages {
                Some(Effect::new(DESTRUCTIVE, "moves the branch to another commit", &["git.reset (keeps a backup ref)"]))
            } else {
                None
            }
        }
        "clean" => (!(short_flag(opts, 'n') || long_flag(opts, "dry-run"))).then(|| Effect::new(DESTRUCTIVE, "deletes the files git does not track, for good", &[])),
        "checkout" => {
            let forces = short_flag(opts, 'f') || long_flag(opts, "force") || short_flag(opts, 'B') || long_flag(opts, "ours") || long_flag(opts, "theirs") || short_flag(opts, 'p') || long_flag(opts, "patch");
            let makes_branch = short_flag(opts, 'b') || long_flag(opts, "orphan");
            let words = operands(opts, &["-b", "-B", "--orphan"]);
            // `git checkout name` is a branch or a file: when `name` is a file in the folder it is a restore, and git refuses the case where it is both.
            let names_a_file = !makes_branch && words.len() == 1 && work.as_deref().is_some_and(|d| d.join(words[0]).exists());
            (forces || rest.iter().any(|a| a == "--") || words.contains(&".") || (!makes_branch && words.len() >= 2) || names_a_file)
                .then(|| Effect::new(DESTRUCTIVE, "overwrites changes in the folder with a saved version, or resets a branch", &["git.discard (keeps a backup ref)"]))
        }
        "switch" => (short_flag(opts, 'f') || long_flag(opts, "force") || long_flag(opts, "discard-changes") || short_flag(opts, 'C') || long_flag(opts, "force-create"))
            .then(|| Effect::new(DESTRUCTIVE, "overwrites uncommitted changes, or resets a branch", &["git.discard (keeps a backup ref)"])),
        "restore" => {
            let index_only = (short_flag(opts, 'S') || long_flag(opts, "staged")) && !(short_flag(opts, 'W') || long_flag(opts, "worktree"));
            (!index_only).then(|| Effect::new(DESTRUCTIVE, "overwrites changes in the folder with a saved version", &["git.discard (keeps a backup ref)"]))
        }
        "branch" => (short_flag(opts, 'd') || short_flag(opts, 'D') || long_flag(opts, "delete") || short_flag(opts, 'f') || long_flag(opts, "force") || short_flag(opts, 'M') || short_flag(opts, 'C'))
            .then(|| Effect::new(DESTRUCTIVE, "deletes a branch, or moves one over another", &["git.branch_delete"])),
        "tag" => (short_flag(opts, 'd') || long_flag(opts, "delete") || short_flag(opts, 'f') || long_flag(opts, "force")).then(|| Effect::new(DESTRUCTIVE, "deletes a tag, or moves one", &[])),
        "stash" => has_word(&["drop", "clear"]).then(|| Effect::new(DESTRUCTIVE, "throws away stashed work for good", &["git.stash_drop"])),
        "rebase" => (!(long_flag(opts, "abort") || long_flag(opts, "quit"))).then(|| Effect::new(DESTRUCTIVE, "rewrites commits", &[])),
        "commit" => long_flag(opts, "amend").then(|| Effect::new(DESTRUCTIVE, "rewrites the last commit", &[])),
        "reflog" => has_word(&["expire", "delete"]).then(|| Effect::new(DESTRUCTIVE, "deletes the record that lets lost commits be found", &[])),
        "gc" => gc_prunes(opts).then(|| Effect::new(DESTRUCTIVE, "deletes unreachable commits for good", &[])),
        "prune" => (!(short_flag(opts, 'n') || long_flag(opts, "dry-run"))).then(|| Effect::new(DESTRUCTIVE, "deletes unreachable commits and files for good", &[])),
        "update-ref" => Some(Effect::new(DESTRUCTIVE, "moves or deletes a branch or a tag", &[])),
        "symbolic-ref" => (short_flag(opts, 'd') || long_flag(opts, "delete")).then(|| Effect::new(DESTRUCTIVE, "deletes a reference", &[])),
        "worktree" => has_word(&["remove", "prune"]).then(|| Effect::new(DESTRUCTIVE, "deletes a working folder", &[])),
        "filter-branch" | "filter-repo" => Some(Effect::new(DESTRUCTIVE, "rewrites the whole history", &[])),
        "notes" => has_word(&["prune", "remove"]).then(|| Effect::new(DESTRUCTIVE, "deletes notes", &[])),
        "rm" => (short_flag(opts, 'f') || long_flag(opts, "force")).then(|| Effect::new(DESTRUCTIVE, "deletes files without git's check for unsaved changes", &[])),
        "remote" if has_word(&["prune"]) => Some(Effect::new(DESTRUCTIVE, "deletes remote-tracking branches", &[])),
        "remote" => has_word(&["add", "set-url", "remove", "rm", "rename", "set-head", "set-branches"]).then(|| Effect::new(OUTSIDE, "changes where this repository sends to and fetches from", &[])),
        "config" => git_config_writes(opts).then(|| Effect::new(OUTSIDE, "changes git's settings, which later commands can use to run programs or send credentials", &[])),
        "submodule" if has_word(&["add", "update"]) => Some(Effect::new(REACHES, "fetches code from another repository into the folder", &[])),
        "submodule" if has_word(&["sync", "set-url", "set-branch"]) => Some(Effect::new(OUTSIDE, "changes where the submodules come from", &[])),
        "submodule" => has_word(&["deinit"]).then(|| Effect::new(DESTRUCTIVE, "removes a submodule's working folder", &[])),
        "maintenance" => has_word(&["start", "register", "unregister", "stop"]).then(|| Effect::new(OUTSIDE, "registers or removes a scheduled job and changes the person's git settings", &[])),
        _ => None,
    }
}

/// [`git_effect_in`] without a folder (a command line that has not been resolved, such as a segment of `term.shell`).
pub fn git_effect(argv: &[String]) -> Option<Effect> {
    git_effect_in(argv, None)
}

/// The classes a git command adds to `Execute` (empty for `status`, `log`, `diff` and the other ordinary work).
pub fn git_effects(argv: &[String]) -> Vec<PermClass> {
    git_effect(argv).map(|e| e.classes).unwrap_or_default()
}

/// Registry commands: they publish, sign in, or hand over ownership on a package registry that other people install from.
const PUBLISHES: &[(&str, &[&str])] = &[
    ("cargo", &["publish", "yank", "unyank", "owner", "login", "logout"]),
    ("npm", &["publish", "unpublish", "deprecate", "dist-tag", "access", "owner", "team", "org", "login", "adduser", "add-user", "logout", "token", "hook", "star", "unstar", "profile"]),
    ("twine", &["upload", "register"]),
];

/// The effect of a package manager command that publishes or signs in (`cargo publish`, `npm publish`, `twine upload`).
pub fn package_effect(exe: &str, argv: &[String]) -> Option<Effect> {
    let name = program_name(exe);
    let key = match name.as_str() {
        "pnpm" | "yarn" | "bun" | "npm" => "npm",
        other => other,
    };
    let (_, words) = PUBLISHES.iter().find(|(n, _)| *n == key)?;
    let front = match key {
        "cargo" => CARGO_FRONT,
        "npm" => NODE_PM_FRONT,
        _ => (&[][..], &[][..]),
    };
    // The word where the subcommand stands; and, when an option in front cannot be read past, every word before a `--`, which can only say yes more often.
    let mut candidates: Vec<&str> = match tool_subcommand(argv, front.0, front.1) {
        Ok(Some(i)) => vec![argv[i].as_str()],
        Ok(None) => Vec::new(),
        Err(_) => argv.iter().take_while(|a| a.as_str() != "--").filter(|a| !a.starts_with('-') && !a.starts_with('+')).map(String::as_str).collect(),
    };
    // `yarn npm publish`.
    if name == "yarn" && candidates.first() == Some(&"npm") {
        candidates = argv.iter().skip_while(|a| a.as_str() != "npm").skip(1).take(1).map(String::as_str).collect();
    }
    candidates.iter().any(|w| words.contains(w)).then(|| Effect::new(REACHES, "publishes to, or signs in to, a package registry that other people install from", &[]))
}

/// The effect of a command: git's (read in the folder the command runs in, when it is known) or a package manager's.
pub fn command_effect(exe: &str, argv: &[String], dir: Option<&Path>) -> Option<Effect> {
    match program_name(exe).as_str() {
        "git" => git_effect_in(argv, dir),
        _ => package_effect(exe, argv),
    }
}

/// The whole argument list a `git` is started with: what the repository's own configuration names, switched off, then the safety options, then what was asked, with the flags that stop a diff from running a program put straight after
/// the subcommand. A repository's own `.gitattributes` and config can name a program as the converter or the external diff of any file (`diff.<name>.textconv`), and
/// `git diff`, `log`, `show` and `format-patch` run it; `--no-ext-diff` and `--no-textconv` switch that off. (Filters, which `git status` and `git add` run, have names of
/// their own and cannot be switched off from here: that stays a limit, said in the register.)
pub fn git_run_args(argv: &[String], repo_programs: &[String]) -> Vec<String> {
    // The programs the repository's own configuration names for converting, merging, signing and asking for passwords, each switched off by name, come FIRST, and the safe
    // options after them: git takes the last of two `-c` for one key, so where both name it (the hooks folder, the fsmonitor, the pager, the editor) the safe one wins. In
    // the other order the empty value found in the scan would take back `core.hooksPath=NUL`, and the repository's hooks would run.
    let mut run: Vec<String> = repo_programs.to_vec();
    run.extend(git_safe_options(NO_HOOKS_PATH));
    let flags: &[&str] = match git_subcommand_index(argv).map(|i| argv[i].as_str()) {
        Some("diff" | "log" | "show" | "format-patch" | "range-diff" | "diff-index" | "diff-files" | "diff-tree") => &["--no-ext-diff", "--no-textconv"],
        Some("blame") => &["--no-textconv"],
        _ => &[],
    };
    match git_subcommand_index(argv) {
        Some(i) if !flags.is_empty() => {
            // After what was asked, not before it: the last of two opposite flags wins, so `git diff --textconv` would take back a flag that came first. And before a `--`,
            // after which everything is a path.
            let end = argv[i + 1..].iter().position(|a| a == "--").map_or(argv.len(), |p| i + 1 + p);
            run.extend(argv[..end].iter().cloned());
            run.extend(flags.iter().map(|f| f.to_string()));
            run.extend(argv[end..].iter().cloned());
        }
        _ => run.extend(argv.iter().cloned()),
    }
    run
}

/// The arguments of the `git config` that lists the configuration git will use in the repository the command is aimed at, with the scope of each entry (the repository's own
/// file, the worktree's own file, the person's, the system's): `switch_off_program_keys` keeps the first two. (The first version asked for the local file only and missed the
/// worktree's; it also took a scan that failed to mean there was nothing to switch off.) Aimed at the repository (the `-C`, `--git-dir` and `--work-tree` that
/// come before its subcommand; its own `-c` values are not read: they are the caller's, and are judged by `args_run_code`). Includes are followed: a configuration file
/// can pull in another.
pub fn git_config_scan_args(argv: &[String]) -> Vec<String> {
    let end = git_subcommand_index(argv).unwrap_or(argv.len());
    let mut scan: Vec<String> = Vec::new();
    let mut i = 0;
    while i < end {
        let a = argv[i].as_str();
        if matches!(a, "-C" | "--git-dir" | "--work-tree") {
            scan.extend(argv[i..(i + 2).min(end)].iter().cloned());
            i += 2;
        } else {
            if a.starts_with("--git-dir=") || a.starts_with("--work-tree=") {
                scan.push(a.to_string());
            }
            i += if matches!(a, "-c" | "--namespace" | "--super-prefix" | "--config-env") { 2 } else { 1 };
        }
    }
    scan.extend(["config", "--show-scope", "--includes", "--list", "-z"].map(String::from));
    scan
}

/// Variable names that hold a program, or the name of one, in whatever section of a git configuration they are found: git runs what they say. Matching by the variable name and
/// not by a list of sections means a section nobody listed is caught as well (the review of the first version named `remote.<name>.uploadpack` and
/// `core.alternateRefsCommand`, which a list of sections had missed).
const PROGRAM_VARIABLES: &[&str] = &[
    "cmd", "command", "helper", "clean", "smudge", "process", "textconv", "external", "gitproxy", "askpass", "fsmonitor", "hookspath", "editor", "pager", "uploadpack", "receivepack",
    "packobjectshook", "alternaterefscommand", "smtpserver", "browser", "difffilter", "httpd", "hook",
];

/// Does this key of a repository's configuration name a program that git would run? And what is it set to, to switch it off.
fn program_key_reset(key: &str) -> Option<&'static str> {
    let (section, rest) = key.split_once('.')?;
    let (middle, var) = match rest.rsplit_once('.') {
        Some((m, v)) => (Some(m), v),
        None => (None, rest),
    };
    let (section, var) = (section.to_ascii_lowercase(), var.to_ascii_lowercase());
    match (section.as_str(), middle.is_some(), var.as_str()) {
        ("pager", false, _) => Some("false"),
        ("submodule", true, "update") => Some("checkout"),
        ("difftool" | "mergetool" | "browser" | "man", true, "path") => Some(""),
        // A merge driver that is switched off fails the merge (a conflict to resolve by hand), where an empty one would quietly take one side.
        (_, _, "driver") => Some("false"),
        ("gpg", _, "program") => Some("gpg"),
        (_, _, "sshcommand") => Some("ssh"),
        // By what the name says, so that a variable nobody listed is caught: `sendemail.tocmd`, `sendemail.cccmd`, `core.defaultKeyCommand`, `man.<tool>.cmd`, `x.y.sendmailcmd`.
        (_, _, v) if v.contains("cmd") || v.contains("command") || v.ends_with("program") || v.ends_with("helper") => Some(""),
        (_, _, v) if PROGRAM_VARIABLES.contains(&v) => Some(""),
        _ => None,
    }
}

/// From the output of `git config --show-scope --includes --list -z` (a scope, a NUL, then `key`, a line break, `value` and a NUL, for each entry), the options that switch off
/// every program that the repository's own configuration names (see `program_key_reset`), as `-c name=value` pairs. The repository's own is the `local` scope and the `worktree`
/// scope: the person's own (`global`) and the system's are theirs. An alias whose value starts with `!` runs a shell command whenever its name is used, so it is switched off
/// too. A filter that is switched off is also made not required, so that git does not fail for want of it. A key that cannot be given to `-c` (one with an `=` in it) or more
/// programs than can be listed are an error: what cannot be switched off is not run.
pub fn switch_off_program_keys(scan: &str) -> Result<Vec<String>, String> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut tokens = scan.split('\0');
    while let (Some(scope), Some(entry)) = (tokens.next(), tokens.next()) {
        if !matches!(scope, "local" | "worktree" | "command") {
            continue;
        }
        let (key, value) = entry.split_once('\n').unwrap_or((entry, ""));
        let key = key.trim();
        let reset = if key.starts_with("alias.") && value.starts_with('!') { Some(String::new()) } else { program_key_reset(key).map(String::from) };
        let Some(reset) = reset else { continue };
        if key.contains(['=', '\r']) {
            return Err(format!("the repository's own configuration names a program under a key that cannot be switched off ({}): git is not run in it", key.chars().take(60).collect::<String>()));
        }
        if !pairs.iter().any(|(k, _)| k == key) {
            pairs.push((key.to_string(), reset));
            if let Some(filter) = key.strip_prefix("filter.").and_then(|r| r.rsplit_once('.')).map(|(sub, _)| sub.to_string()) {
                let required = format!("filter.{filter}.required");
                if !pairs.iter().any(|(k, _)| *k == required) {
                    pairs.push((required, "false".to_string()));
                }
            }
        }
        if pairs.len() > 64 {
            return Err("the repository's own configuration names more than 64 programs: git is not run in it".into());
        }
    }
    Ok(pairs.into_iter().flat_map(|(key, value)| ["-c".to_string(), format!("{key}={value}")]).collect())
}

/// Options put in front of every `git` the terminal runs, so that what a repository's own configuration could make it run (a `core.fsmonitor` helper, hooks, an
/// external diff, a pager, an editor, an `ext::` remote) is switched off. Without them `git status` in a folder that came from an archive runs the program its
/// `.git/config` names (CVE-2026-19592 was this, in another agent). They are not part of the command the person is shown: they only take away. What the caller
/// writes itself, after them, is judged by `args_run_code` and wins.
pub fn git_safe_options(hooks_path: &str) -> Vec<String> {
    let mut v: Vec<String> = vec!["--no-pager".into(), "--no-optional-locks".into()];
    for c in [
        format!("core.hooksPath={hooks_path}"),
        "core.fsmonitor=false".to_string(),
        "core.pager=cat".to_string(),
        "core.editor=true".to_string(),
        "sequence.editor=true".to_string(),
        "diff.external=".to_string(),
        "protocol.ext.allow=never".to_string(),
        // Programs a repository's config could name for other work: ssh, signing, passwords.
        "core.sshCommand=ssh".to_string(),
        "core.askPass=".to_string(),
        "gpg.program=gpg".to_string(),
        "commit.gpgsign=false".to_string(),
        "tag.gpgsign=false".to_string(),
        "push.gpgsign=false".to_string(),
        "log.showsignature=false".to_string(),
        "gpg.format=openpgp".to_string(),
        "gc.auto=0".to_string(),
        "maintenance.auto=false".to_string(),
    ] {
        v.extend(["-c".into(), c]);
    }
    v
}

/// Lower-case program name without a directory or a `.exe` / `.com` / `.cmd` suffix. (`.cmd` is only ever a script host's own shim name: `npm.cmd` and `NPM.CMD` run
/// as `npm` through Node.js, see `resolve_script_host`, and must be judged as `npm`: judged by the longer name they were an ordinary program that `npm install` did not
/// ask about; any other `.cmd` is refused before this is read.)
pub fn program_name(exe: &str) -> String {
    let base = exe.rsplit(['/', '\\']).next().unwrap_or(exe).to_ascii_lowercase();
    base.strip_suffix(".exe").or_else(|| base.strip_suffix(".com")).or_else(|| base.strip_suffix(".cmd")).unwrap_or(&base).to_string()
}

pub fn classify_exe(exe: &str) -> Result<ExeKind, ToolError> {
    let name = program_name(exe);
    if let Some((_, why)) = BLOCKED.iter().find(|(n, _)| *n == name) {
        return Err(ToolError::new(ErrorCode::PermissionDenied, format!("'{name}' cannot be run through term tools: {why}.")));
    }
    Ok(if INTERPRETERS.contains(&name.as_str()) || name.starts_with("python") { ExeKind::Interpreter } else { ExeKind::Plain })
}

/// Resolve `exe` to a real file. A bare name is looked up in `cfg.path_dirs`; a path must sit inside one.
pub fn resolve_exe(cfg: &TermConfig, exe: &str) -> Result<PathBuf, ToolError> {
    let bad = |m: String| ToolError::new(ErrorCode::InvalidArguments, m);
    if exe.is_empty() || exe.contains('\0') || exe.contains(['"', '\'', '`', '|', '&', ';', '<', '>', '%', '^', '*', '?']) {
        return Err(bad("exe must be a program name such as 'git' with no shell characters; there is no shell here".into()));
    }
    let lower = exe.to_ascii_lowercase();
    for ext in [".bat", ".cmd", ".ps1", ".vbs", ".js", ".lnk", ".msi", ".scr", ".hta", ".wsf"] {
        if lower.ends_with(ext) {
            // Say what can be done instead (card OA7-01, TM-10): a Node.js tool runs by its bare name, and Maven and Gradle cannot run here.
            let instead = match batch_only_note(exe) {
                Some(note) => note,
                None if ext == ".cmd" => " A Node.js tool whose .cmd shim npm wrote (tsc.cmd, eslint.cmd, pnpm.cmd) runs by its bare name (tsc), from node_modules\\.bin of the working folder or from the npm folder of the user.".to_string(),
                None => String::new(),
            };
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("{ext} files are scripts and are not run through term tools; only .exe programs are.{instead}")));
        }
    }
    classify_exe(exe)?;
    let mut dirs: Vec<PathBuf> = cfg.path_dirs.iter().map(|d| PathBuf::from(expand_env(d))).filter(|d| d.is_dir()).collect();
    if cfg.discover {
        dirs.extend(discover_dirs());
    }
    let is_path = exe.contains(['/', '\\']) || exe.as_bytes().get(1) == Some(&b':');
    if is_path {
        let p = canonical(Path::new(exe)).map_err(|_| ToolError::new(ErrorCode::NotFound, format!("'{exe}' does not exist")))?;
        let ext_ok = p.extension().map(|e| e.eq_ignore_ascii_case("exe") || e.eq_ignore_ascii_case("com")).unwrap_or(false);
        if !ext_ok {
            return Err(ToolError::new(ErrorCode::PermissionDenied, "only .exe and .com programs run through term tools"));
        }
        let allowed = dirs.iter().filter_map(|d| canonical(d).ok()).any(|d| within(&p, &d));
        if !allowed {
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("'{exe}' is not in a folder programs may be run from ({})", cfg.path_dirs.join(", "))));
        }
        classify_exe(&p.to_string_lossy())?;
        check_system_program(cfg, &p)?;
        return Ok(p);
    }
    let stem = exe.strip_suffix(".exe").or_else(|| exe.strip_suffix(".EXE")).unwrap_or(exe);
    for d in &dirs {
        for ext in ["exe", "com"] {
            let cand = d.join(format!("{stem}.{ext}"));
            if cand.is_file() {
                let real = canonical(&cand).map_err(|e| ToolError::new(ErrorCode::BackendError, e.to_string()))?;
                // What the link points at must pass the same checks (no shell behind a friendly name).
                classify_exe(&real.to_string_lossy())?;
                check_system_program(cfg, &cand)?;
                // Run it under the name it was found by: a rustup `cargo.exe` is a link to rustup.exe,
                // which picks what to be from argv[0], so launching the target runs `rustup test`.
                let dir = canonical(d).map_err(|e| ToolError::new(ErrorCode::BackendError, e.to_string()))?;
                return Ok(dir.join(cand.file_name().unwrap_or_default()));
            }
        }
    }
    // Maven and Gradle are batch files and nothing else: say why they cannot run, rather than that they were not found.
    if let Some(note) = batch_only_note(exe) {
        return Err(ToolError::new(ErrorCode::PermissionDenied, format!("'{}' cannot be run through term tools.{note}", program_name(exe))));
    }
    Err(ToolError::new(ErrorCode::NotFound, format!("program '{exe}' was not found in: {}", cfg.path_dirs.join(", "))))
}

/// The build tools that exist on Windows only as batch files (`mvn.cmd`, `gradlew.bat`...). They need `cmd.exe`, which is never started, so they cannot be run by these tools (card OA7-01, TM-10).
const BATCH_ONLY_TOOLS: &[&str] = &["mvn", "mvnw", "gradle", "gradlew"];

/// The sentence that says why `exe` cannot run, when it is one of [`BATCH_ONLY_TOOLS`] (by its name, with or without a folder or a `.cmd`, `.bat`, `.exe` ending).
pub(crate) fn batch_only_note(exe: &str) -> Option<String> {
    let name = program_name(exe);
    let name = name.strip_suffix(".bat").unwrap_or(&name);
    BATCH_ONLY_TOOLS.contains(&name).then(|| {
        format!(" {name} is a batch file and needs cmd.exe, which term tools never start (cmd.exe parses the arguments itself, and that has been used to smuggle commands in). Run it yourself in a terminal, or run the Java program it starts.")
    })
}

/// A path that `git` would read or write, found in its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitPath {
    pub path: String,
    pub write: bool,
}

pub fn looks_absolute(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/') || p.starts_with('\\') || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\'))
}

pub(super) fn has_parent_segment(p: &str) -> bool {
    p.split(['/', '\\']).any(|s| s == "..")
}

/// Which files will this `git` command read or write, and is it asking for something that cannot be judged?
///
/// `git` takes paths in many places (`-C`, `--git-dir`, `--output=`, `diff --no-index`, `archive -o`, `bundle create`,
/// `clone <url> <dir>`) and several of them reach any file on the computer, including Aqua's own policy and audit files.
/// The caller runs every returned path through the same folder guard the file tools use. A command that edits git's
/// global or system settings (which would apply to every repository) is refused here.
///
/// `Err` is a refusal with the reason; `Ok` is the list of paths to check (empty for an ordinary `git status`).
pub fn git_path_uses(argv: &[String]) -> Result<Vec<GitPath>, String> {
    let mut out: Vec<GitPath> = Vec::new();
    let mut i = 0;
    // Options that come before the subcommand. `-C` (change directory) and `-c` (set a value) differ only by case.
    while i < argv.len() {
        let a = argv[i].as_str();
        if !a.starts_with('-') {
            break;
        }
        let lower = a.to_ascii_lowercase();
        if a == "-C" {
            if let Some(v) = argv.get(i + 1) {
                out.push(GitPath { path: v.clone(), write: false });
            }
            i += 2;
        } else if a == "-c" || lower == "--namespace" || lower == "--super-prefix" || lower == "--config-env" {
            i += 2;
        } else if lower == "--git-dir" || lower == "--work-tree" {
            if let Some(v) = argv.get(i + 1) {
                out.push(GitPath { path: v.clone(), write: false });
            }
            i += 2;
        } else if lower.starts_with("--git-dir=") || lower.starts_with("--work-tree=") {
            out.push(GitPath { path: a[a.find('=').unwrap_or(0) + 1..].to_string(), write: false });
            i += 1;
        } else {
            i += 1;
        }
    }
    let sub = argv.get(i).map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    let rest: &[String] = if i < argv.len() { &argv[i + 1..] } else { &[] };

    let mut positionals: Vec<&str> = Vec::new();
    let mut no_index = false;
    let mut j = 0;
    while j < rest.len() {
        let a = rest[j].as_str();
        let lower = a.to_ascii_lowercase();
        if a == "--" {
            positionals.extend(rest[j + 1..].iter().map(String::as_str));
            break;
        }
        // An option that names a file or folder to write or read, with the value attached or in the next word.
        let take_value = |name: &str| -> Option<(String, usize)> {
            if lower == name {
                rest.get(j + 1).map(|v| (v.clone(), 2))
            } else {
                lower.strip_prefix(&format!("{name}=")).map(|_| (a[name.len() + 1..].to_string(), 1))
            }
        };
        let mut handled = false;
        for (name, write) in [("--output", true), ("--output-directory", true), ("--git-dir", false), ("--work-tree", false), ("--file", true), ("--template", false)] {
            if let Some((v, adv)) = take_value(name) {
                if sub == "config" && name == "--file" {
                    return Err("`git config --file` writes to any file; it is not allowed".into());
                }
                out.push(GitPath { path: v, write });
                j += adv;
                handled = true;
                break;
            }
        }
        if handled {
            continue;
        }
        match lower.as_str() {
            "--no-index" => no_index = true,
            "--global" | "--system" if sub == "config" => return Err("`git config --global/--system` changes settings for every repository; it is not allowed".into()),
            "-o" if matches!(sub.as_str(), "archive" | "format-patch") => {
                if let Some(v) = rest.get(j + 1) {
                    out.push(GitPath { path: v.clone(), write: true });
                }
                j += 2;
                continue;
            }
            _ if lower.starts_with("-o") && lower.len() > 2 && matches!(sub.as_str(), "archive" | "format-patch") => {
                out.push(GitPath { path: a[2..].to_string(), write: true });
            }
            _ if a.starts_with('-') => {}
            _ => positionals.push(a),
        }
        j += 1;
    }

    // Subcommands whose operands are destinations.
    match sub.as_str() {
        "clone" if positionals.len() >= 2 => out.push(GitPath { path: positionals[positionals.len() - 1].to_string(), write: true }),
        "init" => {
            if let Some(d) = positionals.first() {
                out.push(GitPath { path: (*d).to_string(), write: true });
            }
        }
        "worktree" if positionals.first().map(|s| s.eq_ignore_ascii_case("add")).unwrap_or(false) => {
            if let Some(d) = positionals.get(1) {
                out.push(GitPath { path: (*d).to_string(), write: true });
            }
        }
        "bundle" => match positionals.first().map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("create") => {
                if let Some(d) = positionals.get(1) {
                    out.push(GitPath { path: (*d).to_string(), write: true });
                }
            }
            Some("unbundle" | "verify" | "list-heads") => {
                if let Some(d) = positionals.get(1) {
                    out.push(GitPath { path: (*d).to_string(), write: false });
                }
            }
            _ => {}
        },
        _ => {}
    }
    // Any other operand that names a place outside the working folder (an absolute path, or one climbing with `..`) is
    // read: `git diff` outside a repository, `git apply <patch>`, `git hash-object <file>` and so on. Addresses of
    // remote repositories contain `://` or `@host:` and are not paths.
    for pth in &positionals {
        let is_url = pth.contains("://") || pth.starts_with("ext::");
        let already = out.iter().any(|o| o.path == *pth);
        if !already && !is_url && (no_index || looks_absolute(pth) || has_parent_segment(pth)) {
            out.push(GitPath { path: (*pth).to_string(), write: false });
        }
    }
    Ok(out)
}

// ---- the person's own rules: data/tools/term_rules.json -------------------------------------------------------------------------------

/// What a rule of `term_rules.json` does to a command that matches it. The file can only make the terminal stricter: a rule can make a
/// command ask every time or refuse it, never let something through that the built-in rules stop or ask about (`allow` is not accepted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuleDecision {
    Prompt,
    Forbid,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleExamples {
    #[serde(default, rename = "match")]
    matching: Vec<String>,
    #[serde(default)]
    not_match: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleEntry {
    pattern: String,
    decision: String,
    #[serde(default)]
    reason: String,
    #[serde(default = "no_examples")]
    examples: RuleExamples,
}

fn no_examples() -> RuleExamples {
    RuleExamples { matching: Vec::new(), not_match: Vec::new() }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleFile {
    rules: Vec<RuleEntry>,
}

#[derive(Debug, Clone)]
struct Rule {
    tokens: Vec<String>,
    decision: RuleDecision,
    reason: String,
}

/// The rules of `data/tools/term_rules.json`: `{"rules": [{"pattern": "git push", "decision": "forbid", "reason": "...", "examples":
/// {"match": ["git push origin main"], "not_match": ["git status"]}}]}`. A pattern is a program name and the start of its arguments, each
/// compared without regard to case (`*` stands for any one argument): `git push` matches `git push origin main`. Every rule must come with
/// at least one example that matches it, and the examples are checked when the file is read; a file with one wrong rule is refused
/// whole, so a typo never leaves a command less guarded than the person meant. Of the rules that match, the strictest wins.
#[derive(Debug, Clone, Default)]
pub struct ExecRules {
    rules: Vec<Rule>,
}

const MAX_RULES: usize = 200;
const MAX_PATTERN_TOKENS: usize = 12;

fn tokens_of(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_ascii_lowercase).collect()
}

fn rule_matches(tokens: &[String], program: &str, argv: &[String]) -> bool {
    let Some((first, rest)) = tokens.split_first() else { return false };
    if program_name(first) != program || rest.len() > argv.len() {
        return false;
    }
    rest.iter().zip(argv).all(|(want, got)| want == "*" || *want == got.to_ascii_lowercase())
}

impl ExecRules {
    /// No rules at all (what a missing file means).
    pub fn none() -> Self {
        Self::default()
    }

    /// Read the file's text. `Err` says what is wrong, for the log and for the person: the terminal tools stay off until it is fixed.
    pub fn from_text(text: &str) -> Result<Self, String> {
        let file: RuleFile = serde_json::from_str(text).map_err(|e| format!("not valid: {e}"))?;
        if file.rules.len() > MAX_RULES {
            return Err(format!("{} rules, at most {MAX_RULES}", file.rules.len()));
        }
        let mut rules = Vec::new();
        for (i, entry) in file.rules.iter().enumerate() {
            let at = format!("rule {} ({:?})", i + 1, entry.pattern);
            let tokens = tokens_of(&entry.pattern);
            if tokens.is_empty() || tokens.len() > MAX_PATTERN_TOKENS || tokens[0] == "*" || tokens[0].contains(['/', '\\']) {
                return Err(format!("{at}: the pattern must start with a program name (no path) and have at most {MAX_PATTERN_TOKENS} words"));
            }
            let decision = match entry.decision.as_str() {
                "prompt" => RuleDecision::Prompt,
                "forbid" => RuleDecision::Forbid,
                "allow" => return Err(format!("{at}: \"allow\" is not accepted: this file can only make commands ask (\"prompt\") or refuse (\"forbid\")")),
                other => return Err(format!("{at}: the decision is \"prompt\" or \"forbid\", not {other:?}")),
            };
            if entry.examples.matching.is_empty() {
                return Err(format!("{at}: give at least one command under examples.match that the rule is meant to catch"));
            }
            let rule = Rule { tokens, decision, reason: entry.reason.trim().chars().take(200).collect() };
            for example in &entry.examples.matching {
                let words = tokens_of(example);
                let (program, argv) = match words.split_first() {
                    Some((p, a)) => (program_name(p), a.to_vec()),
                    None => (String::new(), Vec::new()),
                };
                if !rule_matches(&rule.tokens, &program, &argv) {
                    return Err(format!("{at}: the example {example:?} is meant to match and does not"));
                }
            }
            for example in &entry.examples.not_match {
                let words = tokens_of(example);
                if let Some((p, a)) = words.split_first() {
                    if rule_matches(&rule.tokens, &program_name(p), a) {
                        return Err(format!("{at}: the example {example:?} is not meant to match and does"));
                    }
                }
            }
            rules.push(rule);
        }
        Ok(Self { rules })
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// What the rules say about this command (`exe` as the caller named it, `argv` as the caller gave them): the strictest of the rules that
    /// match, with the reason the person wrote. `None`: no rule applies.
    pub fn decide(&self, exe: &str, argv: &[String]) -> Option<(RuleDecision, String)> {
        let program = program_name(exe);
        self.rules.iter().filter(|r| rule_matches(&r.tokens, &program, argv)).max_by_key(|r| r.decision).map(|r| (r.decision, r.reason.clone()))
    }
}

const SHELL_REFUSED: &str = "pipes, redirects, substitution, and newlines are not allowed; use && between whole programs";

/// Split a short command line into program segments joined only by `&&`.
/// Pipes, redirects, newlines, and substitution are rejected outside quotes. A quoted text is one plain word whatever it holds: there is no shell to read `|`, `;`, `$`, `&&`
/// or a bracket in it, so `git commit -m "fix: a && b (c); $d"` is one program with one message, and `&&` inside quotes does not split. A line break is refused everywhere,
/// quoted or not, and so is a quote that is never closed. Each segment is a program name plus arguments.
pub fn shell_segments(line: &str) -> Result<Vec<(String, Vec<String>)>, String> {
    let line = line.trim();
    if line.is_empty() || line.len() > 2000 {
        return Err("command must be 1 to 2000 characters".into());
    }
    if line.contains(['\n', '\r']) {
        return Err(SHELL_REFUSED.into());
    }
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            // Quoted text is kept as it is (with its quotes: `split_words` reads them again, by the same rule) until the quote that opened it comes back.
            cur.push(c);
            if c == q {
                quote = None;
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
            cur.push(c);
        } else if c == '&' && chars.get(i + 1) == Some(&'&') {
            parts.push(std::mem::take(&mut cur));
            i += 1;
        } else if c == '&' {
            return Err("a single & is not allowed".into());
        } else if matches!(c, '|' | ';' | '<' | '>' | '`' | '$' | '(' | ')') {
            return Err(SHELL_REFUSED.into());
        } else {
            cur.push(c);
        }
        i += 1;
    }
    if quote.is_some() {
        return Err("an unmatched quote".into());
    }
    parts.push(cur);
    if parts.len() > 4 {
        return Err("at most 4 programs joined by &&".into());
    }
    let mut out = Vec::new();
    for part in parts {
        let tokens = split_words(part.trim())?;
        if tokens.is_empty() {
            return Err("an empty command between &&".into());
        }
        let exe = tokens[0].clone();
        let args = tokens[1..].to_vec();
        let lower: Vec<String> = std::iter::once(exe.to_ascii_lowercase()).chain(args.iter().map(|a| a.to_ascii_lowercase())).collect();
        if lower.iter().any(|a| a == "-enc" || a == "-encodedcommand" || a.starts_with("-enc:")) {
            return Err("encoded powershell is not allowed".into());
        }
        if matches!(lower[0].as_str(), "cmd" | "powershell" | "pwsh" | "bash" | "sh") {
            return Err("do not start a shell; name the program itself".into());
        }
        out.push((exe, args));
    }
    Ok(out)
}

fn split_words(part: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut buf = String::new();
    let mut quote: Option<char> = None;
    for c in part.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => buf.push(c),
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c.is_whitespace() => {
                if !buf.is_empty() {
                    words.push(std::mem::take(&mut buf));
                }
            }
            None => buf.push(c),
        }
    }
    if quote.is_some() {
        return Err("an unmatched quote".into());
    }
    if !buf.is_empty() {
        words.push(buf);
    }
    if words.len() > 64 {
        return Err("too many arguments".into());
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_program_that_runs_another_is_judged_by_what_it_runs() {
        let judge = |exe: &str, args: &[&str]| judge_launched(exe, &words(args), classify_exe(exe).unwrap());
        // What runs is refused when it would be refused by name, whatever wraps it and however deep.
        for (exe, args) in [
            ("env", &["curl", "http://example.com/x"][..]),
            ("env", &["FOO=1", "BAR=2", "curl", "-s", "http://x"]),
            ("env", &["-u", "HOME", "-C", "C:/x", "powershell", "-c", "calc"]),
            ("env", &["--", "cmd", "/c", "calc"]),
            ("xargs", &["-n", "1", "-I", "{}", "powershell", "-Command", "{}"]),
            ("timeout", &["5", "cmd", "/c", "dir"]),
            ("timeout", &["-s", "KILL", "--kill-after", "2", "10", "bash", "-c", "x"]),
            ("nohup", &["env", "sh", "-c", "x"]),
            ("nice", &["-n", "5", "env", "FOO=1", "timeout", "9", "wscript", "x.vbs"]),
            ("busybox", &["sh", "-c", "x"]),
            ("sudo", &["-u", "root", "certutil", "-urlcache", "x"]),
            ("env.exe", &["CURL.EXE", "x"]),
            ("command", &["mshta", "x"]),
        ] {
            let said = judge(exe, args).unwrap_err();
            assert_eq!(said.code, ErrorCode::PermissionDenied, "{exe} {args:?}");
            assert!(said.message.contains("would run"), "{exe} {args:?}: {}", said.message);
        }
        // What runs code is an interpreter, which asks every time; what only looks stays ordinary.
        assert_eq!(judge("env", &["python", "x.py"]).unwrap(), ExeKind::Interpreter);
        assert_eq!(judge("timeout", &["30", "node", "x.js"]).unwrap(), ExeKind::Interpreter);
        assert_eq!(judge("xargs", &["cargo", "build"]).unwrap(), ExeKind::Interpreter);
        assert_eq!(judge("env", &[]).unwrap(), ExeKind::Plain, "a bare env only prints");
        assert_eq!(judge("env", &["FOO=1"]).unwrap(), ExeKind::Plain);
        assert_eq!(judge("git", &["status"]).unwrap(), ExeKind::Plain, "not a launcher");
        // What cannot be read is not guessed: it asks every time. So does a pile of wrappers.
        assert_eq!(judge("env", &["-S", "curl http://x"]).unwrap(), ExeKind::Interpreter);
        assert_eq!(judge("env", &["--split-string=curl x"]).unwrap(), ExeKind::Interpreter);
        // A pile of wrappers is refused, not asked about: the checks that apply to the program at the bottom cannot be applied to what they cannot read.
        for pile in [&["env", "env", "env", "env", "env", "git", "status"][..], &["nohup", "env", "timeout", "9", "env", "env", "echo", "x"]] {
            let said = judge("env", pile).unwrap_err();
            assert_eq!(said.code, ErrorCode::PermissionDenied, "{pile:?}");
            assert!(said.message.contains("more than four programs"), "{pile:?}: {}", said.message);
        }
        assert_eq!(judge("env", &["env", "env", "env", "echo", "x"]).unwrap(), ExeKind::Plain, "four deep is still read");
    }

    #[test]
    fn git_is_not_run_through_a_launcher_so_its_options_its_path_checks_and_the_persons_rules_all_apply_to_it() {
        let judge = |exe: &str, args: &[&str]| judge_launched(exe, &words(args), classify_exe(exe).unwrap());
        for (exe, args) in [("env", &["git", "status"][..]), ("env", &["FOO=1", "git.exe", "-C", "C:/elsewhere", "log"]), ("timeout", &["9", "GIT", "diff"]), ("nohup", &["env", "git", "status"]), ("xargs", &["git", "add"])] {
            let said = judge(exe, args).unwrap_err();
            assert_eq!(said.code, ErrorCode::PermissionDenied, "{exe} {args:?}");
            assert!(said.message.contains("run git directly"), "{exe} {args:?}: {}", said.message);
        }
    }

    #[test]
    fn what_every_wrapper_runs_is_listed_so_the_persons_rules_are_asked_about_each() {
        assert_eq!(
            launched_chain("env", &words(&["FOO=1", "timeout", "9", "npm", "publish"])),
            vec![("timeout".to_string(), words(&["9", "npm", "publish"])), ("npm".to_string(), words(&["publish"]))]
        );
        assert!(launched_chain("git", &words(&["status"])).is_empty() && launched_chain("env", &[]).is_empty() && launched_chain("env", &words(&["-S", "x y"])).is_empty());
        // A rule about a program applies when it is wrapped.
        let rules = ExecRules::from_text(r#"{"rules": [{"pattern": "npm publish", "decision": "forbid", "examples": {"match": ["npm publish"], "not_match": ["npm test"]}}]}"#).expect("the rule is valid");
        assert_eq!(rules.decide("npm.exe", &words(&["publish"])).map(|d| d.0), Some(RuleDecision::Forbid));
        let wrapped = launched_chain("env", &words(&["npm", "publish"]));
        assert_eq!(rules.decide(&wrapped[0].0, &wrapped[0].1).map(|d| d.0), Some(RuleDecision::Forbid), "the same rule, asked about what env runs");
        assert!(rules.decide("env", &words(&["npm", "publish"])).is_none(), "and asked only about env it would not have applied");
    }

    #[test]
    fn git_gets_the_flags_that_stop_a_diff_from_running_a_program_right_after_its_subcommand() {
        let run = |a: &[&str]| git_run_args(&words(a), &[]);
        let tail = |v: &[String]| v[git_safe_options(NO_HOOKS_PATH).len()..].to_vec();
        // After what was asked, so that a flag in what was asked cannot take them back; and before a `--`.
        assert_eq!(tail(&run(&["diff", "HEAD~1"])), words(&["diff", "HEAD~1", "--no-ext-diff", "--no-textconv"]));
        assert_eq!(tail(&run(&["diff", "--ext-diff", "--textconv"])), words(&["diff", "--ext-diff", "--textconv", "--no-ext-diff", "--no-textconv"]));
        assert_eq!(tail(&run(&["-C", "repo", "-c", "a=b", "log", "-p", "-n", "3"])), words(&["-C", "repo", "-c", "a=b", "log", "-p", "-n", "3", "--no-ext-diff", "--no-textconv"]));
        assert_eq!(tail(&run(&["log", "-p", "--", "a.txt", "b.txt"])), words(&["log", "-p", "--no-ext-diff", "--no-textconv", "--", "a.txt", "b.txt"]));
        assert_eq!(tail(&run(&["show"])), words(&["show", "--no-ext-diff", "--no-textconv"]));
        assert_eq!(tail(&run(&["format-patch", "-1"])), words(&["format-patch", "-1", "--no-ext-diff", "--no-textconv"]));
        assert_eq!(tail(&run(&["blame", "a.txt"])), words(&["blame", "a.txt", "--no-textconv"]));
        // The programs the repository's own configuration names come first, so that the safe options, which come after them, win where both name a key.
        let named = vec!["-c".to_string(), "filter.x.clean=".to_string()];
        let started = git_run_args(&words(&["status"]), &named);
        let safe = git_safe_options(NO_HOOKS_PATH);
        assert_eq!(started[..2], named[..]);
        assert_eq!(started[2..2 + safe.len()], safe[..]);
        assert_eq!(started[2 + safe.len()..], ["status"]);
        // The scan finds the keys the safe options set too, lower-cased, with an empty value: the safe value must come after, or the empty one would take it back.
        let scan = scan_of(&[("local", "core.hookspath", "./evil_hooks"), ("local", "core.fsmonitor", "calc"), ("local", "core.pager", "calc"), ("local", "core.editor", "calc")]);
        let overrides = switch_off_program_keys(&scan).unwrap();
        let committing = git_run_args(&words(&["commit", "-m", "x"]), &overrides);
        let last = |key: &str| committing.iter().rposition(|a| a.to_ascii_lowercase().starts_with(&format!("{key}="))).unwrap_or_else(|| panic!("{key} is not set in {committing:?}"));
        assert!(committing[last("core.hookspath")].ends_with(&format!("={NO_HOOKS_PATH}")), "the last word on the hooks folder is the null device: {committing:?}");
        assert_eq!(committing[last("core.fsmonitor")], "core.fsmonitor=false");
        assert_eq!(committing[last("core.pager")], "core.pager=cat");
        assert_eq!(committing[last("core.editor")], "core.editor=true");
        // Other subcommands are as they were asked (no flag they might not know).
        assert_eq!(tail(&run(&["status", "--short"])), words(&["status", "--short"]));
        assert_eq!(tail(&run(&["branch", "-a"])), words(&["branch", "-a"]));
        assert_eq!(tail(&run(&[])), Vec::<String>::new());
        assert_eq!(git_subcommand_index(&words(&["--no-pager", "-C", "x", "diff"])), Some(3));
        assert_eq!(git_subcommand_index(&words(&["--version"])), None);
    }

    #[test]
    fn a_git_name_that_is_not_gits_own_asks_because_it_may_be_an_alias_for_a_program_and_the_subcommand_is_read_past_the_options() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for sub in ["foo", "st", "lg", "deploy", "!calc", "x-evil"] {
            assert!(args_run_code("git", &a(&[sub])).is_some(), "git {sub} may be an alias that runs a program");
            assert!(args_run_code("git.exe", &a(&["-C", "repo", sub, "arg"])).is_some(), "-C repo {sub}");
        }
        // Git finds its own commands by their exact names; any other spelling is looked up as an alias, whose name it does not mind the case of.
        for sub in ["Status", "STATUS", "Log", "DIFF", "Commit", "sTaTuS"] {
            assert!(args_run_code("git", &a(&[sub])).is_some(), "git {sub} is not git's own name: it can be an alias for a program");
        }
        // Git's own names, however the options in front are written, are plain; the option's value is not taken for the subcommand.
        for args in [&["status"][..], &["-C", "some_folder", "status", "--short"], &["--no-pager", "log", "-n", "3"], &["--no-pager", "--git-dir", "x/.git", "show", "HEAD"], &["branch", "-a"]] {
            assert!(args_run_code("git", &a(args)).is_none(), "{args:?}");
        }
        assert!(args_run_code("git", &a(&["--version"])).is_none() && args_run_code("git", &[]).is_none());
    }

    /// The scan as git prints it: a scope, then `key`, a line break and `value`, each ended by a NUL.
    fn scan_of(entries: &[(&str, &str, &str)]) -> String {
        entries.iter().map(|(scope, key, value)| format!("{scope}\0{key}\n{value}\0")).collect()
    }

    #[test]
    fn the_programs_a_repositorys_own_configuration_names_are_found_by_variable_and_each_is_switched_off() {
        let scan = scan_of(&[
            ("local", "core.repositoryformatversion", "0"),
            ("local", "core.fsmonitor", "calc.exe"),
            ("local", "filter.evil.clean", "echo x"),
            ("local", "filter.evil.smudge", "echo y"),
            ("local", "diff.Evil.textconv", "calc"),
            ("local", "diff.Evil.command", "calc"),
            ("local", "merge.bad.driver", "calc %O"),
            ("local", "credential.helper", "!calc"),
            ("local", "credential.https://x.example.helper", "!calc"),
            ("local", "core.sshCommand", "calc"),
            ("local", "gpg.program", "calc"),
            ("local", "gpg.ssh.program", "calc"),
            ("local", "pager.log", "calc"),
            ("local", "submodule.s.update", "!calc"),
            // the ones the review of the first version named, and a few more by their variable
            ("local", "remote.origin.uploadpack", "calc"),
            ("local", "remote.origin.receivepack", "calc"),
            ("local", "core.alternateRefsCommand", "calc"),
            ("local", "tar.evil.command", "calc"),
            ("local", "trailer.t.cmd", "calc"),
            ("local", "browser.b.cmd", "calc"),
            ("local", "browser.b.path", "calc"),
            ("local", "help.browser", "calc"),
            ("local", "sendemail.smtpserver", "calc"),
            ("local", "interactive.diffFilter", "calc"),
            ("local", "uploadpack.packObjectsHook", "calc"),
            ("local", "instaweb.httpd", "calc"),
            ("local", "man.m.path", "calc"),
            ("local", "sendemail.tocmd", "calc"),
            ("local", "sendemail.cccmd", "calc"),
            ("local", "sendemail.headercmd", "calc"),
            ("local", "core.defaultKeyCommand", "calc"),
            ("local", "some.thing.sendmailcmd", "calc"),
            ("local", "weird.section.anyprogram", "calc"),
            ("local", "some.section.nobody.listed.helper", "calc"),
            // another file of this repository: the worktree's own
            ("worktree", "diff.wt.textconv", "calc"),
            // not programs
            ("local", "user.name", "somebody"),
            ("local", "remote.origin.url", "https://example.com/x.git"),
            ("local", "include.path", "../other.cfg"),
            ("local", "alias.st", "status"),
            // an alias that runs a shell command
            ("local", "alias.deploy", "!calc"),
            // the person's own and the system's: theirs, left alone
            ("global", "credential.helper", "manager"),
            ("system", "diff.astextplain.textconv", "astextplain"),
            ("global", "core.sshCommand", "my-ssh"),
        ]);
        let pairs = switch_off_program_keys(&scan).unwrap();
        let has = |kv: &str| pairs.windows(2).any(|w| w[0] == "-c" && w[1] == kv);
        for kv in [
            "core.fsmonitor=", "filter.evil.clean=", "filter.evil.smudge=", "filter.evil.required=false", "diff.Evil.textconv=", "diff.Evil.command=", "merge.bad.driver=false",
            "credential.helper=", "credential.https://x.example.helper=", "core.sshCommand=ssh", "gpg.program=gpg", "gpg.ssh.program=gpg", "pager.log=false", "submodule.s.update=checkout",
            "remote.origin.uploadpack=", "remote.origin.receivepack=", "core.alternateRefsCommand=", "tar.evil.command=", "trailer.t.cmd=", "browser.b.cmd=", "browser.b.path=", "help.browser=",
            "sendemail.smtpserver=", "interactive.diffFilter=", "uploadpack.packObjectsHook=", "some.section.nobody.listed.helper=", "diff.wt.textconv=", "alias.deploy=",
            "instaweb.httpd=", "man.m.path=", "sendemail.tocmd=", "sendemail.cccmd=", "sendemail.headercmd=", "core.defaultKeyCommand=", "some.thing.sendmailcmd=", "weird.section.anyprogram=",
        ] {
            assert!(has(kv), "{kv} is missing from {pairs:?}");
        }
        // Only the repository's programs: a name, a url, an include, an alias for a git command, and what belongs to the person or the system, stay as they are.
        for not in ["user.name", "remote.origin.url", "include.path", "alias.st", "core.repositoryformatversion", "diff.astextplain"] {
            assert!(!pairs.iter().any(|p| p.starts_with(not)), "{not} must stay: {pairs:?}");
        }
        assert_eq!(pairs.iter().filter(|p| p.starts_with("credential.helper=")).count(), 1, "the person's own helper is not listed beside the repository's: {pairs:?}");
        assert_eq!(pairs.iter().filter(|p| p.starts_with("filter.evil.required")).count(), 1);
        assert!(switch_off_program_keys("").unwrap().is_empty() && switch_off_program_keys(&scan_of(&[("local", "user.name", "x")])).unwrap().is_empty());
        // A key that cannot be given to `-c` is an error, not a program left running; so is a configuration that names too many.
        assert!(switch_off_program_keys(&scan_of(&[("local", "diff.a=b.textconv", "calc")])).unwrap_err().contains("cannot be switched off"));
        let many: Vec<(String, String)> = (0..70).map(|i| (format!("filter.f{i}.clean"), "calc".to_string())).collect();
        let many = scan_of(&many.iter().map(|(k, v)| ("local", k.as_str(), v.as_str())).collect::<Vec<_>>());
        assert!(switch_off_program_keys(&many).unwrap_err().contains("more than 64"));
    }

    #[test]
    fn the_scan_of_a_repositorys_configuration_is_aimed_at_the_repository_the_command_is_aimed_at() {
        let scan = |a: &[&str]| git_config_scan_args(&words(a));
        let tail = ["config", "--show-scope", "--includes", "--list", "-z"];
        assert_eq!(scan(&["status"]), tail);
        assert_eq!(scan(&["-C", "other", "status"])[..2], ["-C", "other"]);
        assert_eq!(scan(&["-c", "a=b", "--no-pager", "-C", "x", "--git-dir=y/.git", "log"]), ["-C", "x", "--git-dir=y/.git", "config", "--show-scope", "--includes", "--list", "-z"]);
        assert_eq!(scan(&["--work-tree", "w", "diff"]), ["--work-tree", "w", "config", "--show-scope", "--includes", "--list", "-z"]);
    }

    #[test]
    fn what_a_launcher_runs_is_read_past_its_own_options_and_arguments() {
        let read = |w: &str, a: &[&str]| launched_program(w, &words(a));
        assert_eq!(read("env", &["-i", "A=b", "git", "log"]), Launched::Program("git".into(), words(&["log"])));
        assert_eq!(read("env", &["-u", "A", "git"]), Launched::Program("git".into(), vec![]), "-u takes a name");
        assert_eq!(read("timeout", &["--signal=KILL", "5", "git", "gc"]), Launched::Program("git".into(), words(&["gc"])));
        assert_eq!(read("timeout", &["-k", "3", "5", "git"]), Launched::Program("git".into(), vec![]), "-k takes a value, then the duration");
        assert_eq!(read("xargs", &["-0", "-n", "2", "echo", "a"]), Launched::Program("echo".into(), words(&["a"])));
        assert_eq!(read("nohup", &["git", "status"]), Launched::Program("git".into(), words(&["status"])));
        assert_eq!(read("env", &[]), Launched::Nothing);
        assert_eq!(read("timeout", &["5"]), Launched::Nothing);
        assert_eq!(read("env", &["-S", "x"]), Launched::Unclear);
    }

    #[test]
    fn the_options_put_before_git_take_away_what_a_repositorys_own_configuration_could_run() {
        let v = git_safe_options(NO_HOOKS_PATH);
        let has = |c: &str| v.windows(2).any(|w| w[0] == "-c" && w[1] == c);
        for c in ["core.fsmonitor=false", &format!("core.hooksPath={NO_HOOKS_PATH}"), "core.pager=cat", "core.editor=true", "diff.external=", "protocol.ext.allow=never", "gc.auto=0", "core.sshCommand=ssh", "gpg.program=gpg", "commit.gpgsign=false", "push.gpgsign=false", "gpg.format=openpgp"] {
            assert!(has(c), "{c} is missing from {v:?}");
        }
        assert!(v.contains(&"--no-pager".to_string()) && v.contains(&"--no-optional-locks".to_string()));
        // They only take away: nothing here sets a program to run.
        assert!(!v.iter().any(|a| a.contains("credential") || a.contains("alias.")));
    }

    /// The case of CVE-2026-19592: the repository's own `.git/config` names a program in `core.fsmonitor`, and `git status` runs it. Real git, a real repository.
    #[cfg(windows)]
    #[test]
    fn a_repositorys_own_fsmonitor_helper_is_not_run_by_git_with_these_options() {
        let Some(git) = ["C:/Program Files/Git/cmd/git.exe", "C:/Program Files (x86)/Git/cmd/git.exe"].iter().map(PathBuf::from).find(|p| p.is_file()) else {
            eprintln!("skipped: git is not installed");
            return;
        };
        let dir = std::env::temp_dir().join(format!("aqua_fsmonitor_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |extra: &[String], args: &[&str]| {
            std::process::Command::new(&git).args(extra).args(args).current_dir(&dir).env("GIT_TERMINAL_PROMPT", "0").output().expect("git runs")
        };
        assert!(run(&[], &["init", "-q"]).status.success());
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        assert!(run(&[], &["add", "a.txt"]).status.success());
        let marker = dir.join("helper_ran.txt");
        let helper = format!("echo ran > \"{}\" #", marker.to_string_lossy().replace('\\', "/"));
        let config = dir.join(".git").join("config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str(&format!("[core]\n\tfsmonitor = {helper}\n"));
        std::fs::write(&config, text).unwrap();
        // The control: without the options the helper runs. (If it does not here, the machine cannot show the problem and nothing is claimed.)
        run(&[], &["status"]);
        if !marker.exists() {
            eprintln!("skipped: this git does not run the helper, so the test could not show anything");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        std::fs::remove_file(&marker).unwrap();
        let out = run(&git_safe_options(NO_HOOKS_PATH), &["status", "--short"]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(!marker.exists(), "the repository's own helper ran");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `diff.<name>.textconv` in the repository's config and `diff=<name>` in its `.gitattributes` make `git diff` run a program on each file: real git, a real repository.
    #[cfg(windows)]
    #[test]
    fn a_repositorys_own_diff_converter_and_ssh_command_are_not_run_by_git_with_these_options() {
        let Some(git) = ["C:/Program Files/Git/cmd/git.exe", "C:/Program Files (x86)/Git/cmd/git.exe"].iter().map(PathBuf::from).find(|p| p.is_file()) else {
            eprintln!("skipped: git is not installed");
            return;
        };
        let dir = std::env::temp_dir().join(format!("aqua_textconv_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[String]| std::process::Command::new(&git).args(args).current_dir(&dir).env("GIT_TERMINAL_PROMPT", "0").output().expect("git runs");
        let plain = |args: &[&str]| run(&words(args));
        assert!(plain(&["init", "-q"]).status.success());
        std::fs::write(dir.join(".gitattributes"), "*.txt diff=evil\n").unwrap();
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        assert!(plain(&["add", "."]).status.success());
        assert!(plain(&["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", "commit", "-q", "-m", "one"]).status.success());
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        let converter = dir.join("converter_ran.txt");
        let ssh = dir.join("ssh_ran.txt");
        let config = dir.join(".git").join("config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str(&format!(
            "[diff \"evil\"]\n\ttextconv = echo ran > \"{}\" #\n[core]\n\tsshCommand = echo ran > \"{}\" #\n",
            converter.to_string_lossy().replace('\\', "/"),
            ssh.to_string_lossy().replace('\\', "/")
        ));
        std::fs::write(&config, text).unwrap();
        // The controls: without the options both run. (Where they do not, the machine cannot show the problem and nothing is claimed for that one.)
        plain(&["diff"]);
        plain(&["ls-remote", "ssh://127.0.0.1:9/none"]);
        for (marker, args, what) in [(&converter, vec!["diff".to_string()], "the diff converter"), (&ssh, words(&["ls-remote", "ssh://127.0.0.1:9/none"]), "the ssh command")] {
            if !marker.exists() {
                eprintln!("skipped: this git does not run {what}, so the test could not show anything");
                continue;
            }
            std::fs::remove_file(marker).unwrap();
            run(&git_run_args(&args, &[]));
            assert!(!marker.exists(), "{what} named in the repository's own config ran");
        }
        // The diff itself still works, and says what changed.
        let out = run(&git_run_args(&words(&["diff", "--stat"]), &[]));
        assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains("a.txt"), "{}", String::from_utf8_lossy(&out.stderr));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The filter that a repository's `.gitattributes` and config name runs a program when `git add` reads the file, the merge driver when `git merge` combines two versions,
    /// and `git diff --textconv` asked for by the caller would take back a flag that came before it: real git, a real repository, the control and then the options.
    #[cfg(windows)]
    #[test]
    fn a_repositorys_own_filter_and_merge_driver_are_not_run_and_a_flag_in_the_command_cannot_take_the_diff_flags_back() {
        let Some(git) = ["C:/Program Files/Git/cmd/git.exe", "C:/Program Files (x86)/Git/cmd/git.exe"].iter().map(PathBuf::from).find(|p| p.is_file()) else {
            eprintln!("skipped: git is not installed");
            return;
        };
        let dir = std::env::temp_dir().join(format!("aqua_drivers_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[String]| std::process::Command::new(&git).args(args).current_dir(&dir).env("GIT_TERMINAL_PROMPT", "0").output().expect("git runs");
        let plain = |args: &[&str]| run(&words(args));
        fn with_identity<'a>(rest: &[&'a str]) -> Vec<&'a str> {
            // The filter of this repository outputs nothing, which would empty every file it touches: where the test is about something else it is told to pass the file through.
            ["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", "-c", "filter.evil.clean=cat", "-c", "filter.evil.smudge=cat"].iter().copied().chain(rest.iter().copied()).collect()
        }
        assert!(plain(&["init", "-q", "-b", "main"]).status.success());
        std::fs::write(dir.join(".gitattributes"), "*.txt filter=evil diff=evil merge=evil\n").unwrap();
        std::fs::write(dir.join("a.txt"), "base\n").unwrap();
        // Config comes after the first commit, so the setup itself runs none of it.
        assert!(plain(&["add", ".gitattributes", "a.txt"]).status.success());
        assert!(plain(&with_identity(&["commit", "-q", "-m", "base"])).status.success());
        let marker = |name: &str| dir.join(name);
        let line = |name: &str| format!("echo ran > \"{}\" #", marker(name).to_string_lossy().replace('\\', "/"));
        let config = dir.join(".git").join("config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str(&format!(
            "[filter \"evil\"]\n\tclean = {}\n\trequired = false\n[diff \"evil\"]\n\ttextconv = {}\n[merge \"evil\"]\n\tdriver = {}\n",
            line("filter_ran.txt"),
            line("textconv_ran.txt"),
            line("merge_ran.txt")
        ));
        std::fs::write(&config, text).unwrap();
        // What is to be switched off, found the way the terminal finds it.
        let scan = run(&git_config_scan_args(&[]));
        let overrides = switch_off_program_keys(&String::from_utf8_lossy(&scan.stdout)).unwrap();
        for kv in ["filter.evil.clean=", "diff.evil.textconv=", "merge.evil.driver=false"] {
            assert!(overrides.iter().any(|o| o == kv), "{kv} was not found in {overrides:?}");
        }
        // 1. The clean filter, run by `git add` of a changed file.
        std::fs::write(dir.join("a.txt"), "changed on main\n").unwrap();
        plain(&["add", "a.txt"]);
        let control_filter = marker("filter_ran.txt").exists();
        if control_filter {
            std::fs::remove_file(marker("filter_ran.txt")).unwrap();
            std::fs::write(dir.join("a.txt"), "changed again\n").unwrap();
            run(&git_run_args(&words(&["add", "a.txt"]), &overrides));
            assert!(!marker("filter_ran.txt").exists(), "the filter named in the repository's own config ran");
        } else {
            eprintln!("skipped the filter: this git does not run it, so nothing can be shown");
        }
        // 2. The diff converter, asked for again by the caller: the flags after it win.
        std::fs::write(dir.join("a.txt"), "for the diff\n").unwrap();
        let _ = std::fs::remove_file(marker("textconv_ran.txt"));
        plain(&["diff", "--textconv"]);
        if marker("textconv_ran.txt").exists() {
            std::fs::remove_file(marker("textconv_ran.txt")).unwrap();
            run(&git_run_args(&words(&["diff", "--textconv", "--ext-diff"]), &overrides));
            assert!(!marker("textconv_ran.txt").exists(), "git diff --textconv in the command took the flags back");
            // The old order, flags first, is what the review found: here it would have run.
            let mut flags_first = git_safe_options(NO_HOOKS_PATH);
            flags_first.extend(words(&["diff", "--no-ext-diff", "--no-textconv", "--textconv"]));
            run(&flags_first);
            assert!(marker("textconv_ran.txt").exists(), "the control for the order: a flag that comes later wins");
            std::fs::remove_file(marker("textconv_ran.txt")).unwrap();
        } else {
            eprintln!("skipped the converter: this git does not run it, so nothing can be shown");
        }
        // 3. The merge driver, run by `git merge` of a branch that changed the same file.
        let _ = std::fs::remove_file(dir.join("a.txt"));
        plain(&with_identity(&["checkout", "-q", "--", "a.txt"]));
        plain(&with_identity(&["reset", "-q", "--hard"]));
        assert!(plain(&with_identity(&["checkout", "-q", "-b", "other"])).status.success());
        std::fs::write(dir.join("a.txt"), "other side\n").unwrap();
        plain(&with_identity(&["add", "a.txt"]));
        plain(&with_identity(&["commit", "-q", "-m", "other"]));
        assert!(plain(&with_identity(&["checkout", "-q", "main"])).status.success());
        std::fs::write(dir.join("a.txt"), "main side\n").unwrap();
        plain(&with_identity(&["add", "a.txt"]));
        plain(&with_identity(&["commit", "-q", "-m", "main side"]));
        let _ = std::fs::remove_file(marker("merge_ran.txt"));
        let before = String::from_utf8_lossy(&plain(&["rev-parse", "HEAD"]).stdout).trim().to_string();
        plain(&with_identity(&["merge", "other", "-m", "merge", "--no-edit"]));
        if marker("merge_ran.txt").exists() {
            // Back to where it was, so that the second merge has something to merge: otherwise it would say "already up to date" and prove nothing.
            plain(&with_identity(&["merge", "--abort"]));
            assert!(plain(&with_identity(&["reset", "-q", "--hard", before.as_str()])).status.success());
            std::fs::remove_file(marker("merge_ran.txt")).unwrap();
            run(&git_run_args(&words(&with_identity(&["merge", "other", "-m", "merge", "--no-edit"])), &overrides));
            assert!(!marker("merge_ran.txt").exists(), "the merge driver named in the repository's own config ran");
        } else {
            eprintln!("skipped the merge driver: this git does not run it, so nothing can be shown");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The hooks of a repository are programs it brings along. The null device as the hooks folder means git finds none: real git, a real hook, a real commit.
    #[cfg(windows)]
    #[test]
    fn a_repositorys_own_hooks_are_not_run_by_git_with_these_options() {
        let Some(git) = ["C:/Program Files/Git/cmd/git.exe", "C:/Program Files (x86)/Git/cmd/git.exe"].iter().map(PathBuf::from).find(|p| p.is_file()) else {
            eprintln!("skipped: git is not installed");
            return;
        };
        let dir = std::env::temp_dir().join(format!("aqua_hooks_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |extra: &[String], args: &[&str]| {
            std::process::Command::new(&git)
                .args(extra)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"])
                .args(args)
                .current_dir(&dir)
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .expect("git runs")
        };
        assert!(run(&[], &["init", "-q"]).status.success());
        let marker = dir.join("hook_ran.txt");
        let hook = dir.join(".git").join("hooks").join("pre-commit");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, format!("#!/bin/sh\necho ran > \"{}\"\n", marker.to_string_lossy().replace('\\', "/"))).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        assert!(run(&[], &["add", "a.txt"]).status.success());
        // The control: without the options the hook runs on a commit. (If it does not here, the machine cannot show the problem and nothing is claimed.)
        run(&[], &["commit", "-q", "-m", "one"]);
        if !marker.exists() {
            eprintln!("skipped: this git does not run the hook, so the test could not show anything");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        std::fs::remove_file(&marker).unwrap();
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        assert!(run(&[], &["add", "b.txt"]).status.success());
        let out = run(&git_safe_options(NO_HOOKS_PATH), &["commit", "-q", "-m", "two"]);
        assert!(out.status.success(), "the commit itself still works: {}", String::from_utf8_lossy(&out.stderr));
        assert!(!marker.exists(), "the repository's own hook ran");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shells_script_hosts_and_fetchers_are_blocked_however_they_are_spelled() {
        for exe in ["cmd", "CMD.EXE", "powershell", "PowerShell.exe", "pwsh", "wsl", "bash", "mshta", "wscript", "regsvr32", "rundll32", "certutil", "curl", "ssh", "reg", "net", "taskkill", "shutdown", "robocopy", "xcopy", "findstr", "MORE.COM", "tar", "cmdkey", r"C:\Windows\System32\cmd.exe", "C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe"] {
            assert_eq!(classify_exe(exe).unwrap_err().code, ErrorCode::PermissionDenied, "{exe}");
        }
    }

    #[test]
    fn an_ordinary_tool_asks_when_its_arguments_would_run_code() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // plain uses stay plain
        for (exe, args) in [("git", &["status"][..]), ("git", &["log", "--oneline"]), ("git", &["diff", "HEAD~1"]), ("cargo", &["--version"]), ("cargo", &["metadata"]), ("npm", &["--version"]), ("ipconfig", &["/all"])] {
            assert!(args_run_code(exe, &a(args)).is_none(), "{exe} {args:?}");
        }
        // the same tools, running code
        for (exe, args) in [
            ("git", &["-c", "core.fsmonitor=calc.exe", "status"][..]),
            ("git", &["-c", "alias.x=!calc", "x"]),
            ("git", &["clone", "ext::sh -c calc"]),
            ("git", &["--upload-pack=calc", "fetch"]),
            ("git", &["submodule", "foreach", "calc"]),
            ("git", &["rebase", "--exec", "calc"]),
            ("cargo", &["build"]),
            ("cargo.exe", &["run", "--release"]),
            ("npm", &["install"]),
            ("pnpm", &["run", "dev"]),
            ("pip", &["install", "somepkg"]),
            ("make", &[]),
            // OA8-01 (SE-02): the subcommand was read as "the first word that does not start with a dash", so a toolchain, an option or a folder in front hid it.
            ("cargo", &["+stable", "build"]),
            ("cargo", &["--config", "build.rustc-wrapper=x", "check"]),
            ("npm", &["--prefix", ".", "install"]),
        ] {
            assert!(args_run_code(exe, &a(args)).is_some(), "{exe} {args:?}");
        }
        // And a program that starts another toolchain's program is judged as that program: the shell that is refused by name is refused here too.
        let said = judge_launched("rustup", &a(&["run", "stable", "powershell", "-c", "x"]), classify_exe("rustup").unwrap()).unwrap_err();
        assert_eq!(said.code, ErrorCode::PermissionDenied);
        assert!(said.message.contains("'rustup' would run 'powershell'"), "{}", said.message);
    }

    #[test]
    fn a_toolchain_an_option_or_a_folder_in_front_of_the_subcommand_does_not_hide_it_and_a_tool_that_only_looks_stays_plain() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Asks every time.
        for (exe, args) in [
            ("cargo", &["+stable", "build"][..]),
            ("cargo", &["+nightly", "--version"]),
            ("cargo", &["--config", "build.rustc-wrapper=x", "check"]),
            ("cargo", &["--config=build.rustc-wrapper=x", "metadata"]),
            ("cargo", &["-Zunstable-options", "metadata"]),
            ("cargo", &["-C", "other", "build"]),
            ("cargo", &["--manifest-path", "other/Cargo.toml", "metadata"]),
            ("cargo", &["metadata", "--config", "net.git-fetch-with-cli=true"]),
            ("cargo", &["tree", "-Z", "unstable-options"]),
            ("cargo", &["r"]),
            ("cargo", &["publish"]),
            ("cargo", &["package"]),
            ("cargo", &["--locked", "run"]),
            ("npm", &["--prefix", ".", "install"]),
            ("npm", &["--registry", "http://x", "view", "y"]),
            ("npm", &["inst"]),
            ("npm", &["rum", "build"]),
            ("npm", &["update"]),
            ("npm", &["uninstall", "x"]),
            ("npm", &["pack"]),
            ("npm", &["test"]),
            ("npm", &["config", "set", "script-shell", "calc.exe"]),
            ("NPM.CMD", &["install"]),
            ("pnpm", &["-C", "dir", "install"]),
            ("pnpm", &["--filter", "x", "run", "y"]),
            ("yarn", &[]),
            ("yarn", &["install"]),
            ("yarn", &["--cwd", "dir", "install"]),
            ("pip", &["uninstall", "x"]),
            ("pip", &["download", "x"]),
            ("pip", &["--python", "other", "list"]),
            ("pip", &["config", "edit"]),
            ("rustup", &["update"]),
            ("rustup", &["toolchain", "install", "stable"]),
            ("rustup", &["self", "update"]),
            ("rustup", &["default", "nightly"]),
            ("rustup", &["+nightly", "target", "add", "x"]),
            ("volta", &["install", "node"]),
            ("corepack", &["enable"]),
            ("corepack", &["prepare", "pnpm@8", "--activate"]),
        ] {
            assert!(args_run_code(exe, &a(args)).is_some(), "{exe} {args:?} asks every time");
        }
        // A tool that only looks, with the options that are known not to matter in front.
        for (exe, args) in [
            ("cargo", &[][..]),
            ("cargo", &["-V"]),
            ("cargo", &["metadata"]),
            ("cargo", &["metadata", "--format-version", "1"]),
            ("cargo", &["--color", "always", "metadata"]),
            ("cargo", &["-q", "tree"]),
            ("cargo", &["--locked", "tree"]),
            ("cargo", &["fmt", "--check"]),
            ("cargo", &["metadata", "--", "-C", "x"]),
            ("npm", &[]),
            ("npm", &["-v"]),
            ("npm", &["ls", "--depth=0"]),
            ("npm", &["view", "left-pad", "version"]),
            ("npm", &["outdated"]),
            ("npm", &["-s", "ls"]),
            ("npm", &["--loglevel", "silent", "ls"]),
            ("pnpm", &["list", "--depth", "1"]),
            ("yarn", &["--version"]),
            ("yarn", &["info", "left-pad"]),
            ("pip", &[]),
            ("pip", &["--version"]),
            ("pip", &["list"]),
            ("pip3", &["show", "x"]),
            ("pip", &["--disable-pip-version-check", "freeze"]),
            ("rustup", &["show"]),
            ("rustup", &["--version"]),
            ("rustup", &["+nightly", "show"]),
            ("rustup", &["which", "cargo"]),
            ("rustup", &["toolchain", "list"]),
            ("rustup", &["run", "stable", "cargo", "--version"]),
            ("volta", &["list"]),
            ("volta", &["run", "node", "x.js"]),
            ("corepack", &["--version"]),
            ("corepack", &["pnpm", "--version"]),
            ("corepack", &["help"]),
        ] {
            assert!(args_run_code(exe, &a(args)).is_none(), "{exe} {args:?} stays plain");
        }
        // `.cmd` is only the shim's own name: it was a different program to the check, and `npm.cmd install` did not ask.
        assert_eq!(program_name("NPM.CMD"), "npm");
        assert_eq!(program_name(r"C:\Program Files\nodejs\npx.cmd"), "npx");
    }

    #[test]
    fn rustup_volta_corepack_npx_and_pnpx_are_judged_by_the_program_they_run() {
        let launched = |w: &str, a: &[&str]| launched_program(w, &words(a));
        let program = |p: &str, a: &[&str]| Launched::Program(p.to_string(), words(a));
        assert_eq!(launched("rustup", &["run", "stable", "cargo", "build"]), program("cargo", &["build"]));
        assert_eq!(launched("rustup", &["+nightly", "run", "--install", "nightly", "rustc", "x.rs"]), program("rustc", &["x.rs"]));
        assert_eq!(launched("rustup", &["run", "stable", "--", "powershell"]), program("powershell", &[]));
        assert_eq!(launched("rustup", &["show"]), Launched::Nothing);
        assert_eq!(launched("rustup", &["run", "stable"]), Launched::Nothing);
        assert_eq!(launched("rustup", &["run", "-x", "stable", "cmd"]), Launched::Unclear, "an option that is not known may take the next word");
        assert_eq!(launched("volta", &["run", "--node", "18", "node", "x.js"]), program("node", &["x.js"]));
        assert_eq!(launched("volta", &["run", "--env", "A=1", "--", "cmd", "/c", "x"]), program("cmd", &["/c", "x"]));
        assert_eq!(launched("volta", &["install", "node"]), Launched::Nothing);
        assert_eq!(launched("volta", &["run", "--weird", "node"]), Launched::Unclear);
        assert_eq!(launched("corepack", &["pnpm", "install"]), program("pnpm", &["install"]));
        for own in [&["enable"][..], &["prepare", "pnpm@8"], &["--version"], &["help"], &[]] {
            assert_eq!(launched("corepack", own), Launched::Nothing, "{own:?}");
        }
        assert_eq!(launched("corepack", &["--weird", "pnpm"]), Launched::Unclear);
        assert_eq!(launched("npx", &["-y", "prettier", "--check", "."]), program("prettier", &["--check", "."]));
        assert_eq!(launched("npx", &["--yes", "--package", "left-pad", "cmd", "/c", "calc"]), program("cmd", &["/c", "calc"]));
        assert_eq!(launched("npx", &["-p", "a", "-p", "b", "tool"]), program("tool", &[]));
        assert_eq!(launched("pnpx", &["--registry=http://x", "pkg", "-v"]), program("pkg", &["-v"]));
        assert_eq!(launched("npx", &["--", "cmd"]), program("cmd", &[]));
        for unreadable in [&["-c", "echo x"][..], &["--call=echo x"], &["--shell-mode", "x"], &["--weird", "pkg"]] {
            assert_eq!(launched("npx", unreadable), Launched::Unclear, "{unreadable:?}");
        }
        let judge = |exe: &str, args: &[&str]| judge_launched(exe, &words(args), classify_exe(exe).unwrap());
        // What would be refused by name is refused, however the program is started.
        for (exe, args) in [
            ("rustup", &["run", "stable", "powershell", "-c", "x"][..]),
            ("rustup", &["+nightly", "run", "nightly", "cmd", "/c", "x"]),
            ("npx", &["cmd", "/c", "calc"]),
            ("npx", &["--yes", "powershell"]),
            ("pnpx", &["curl", "http://x"]),
            ("volta", &["run", "--node", "18", "bash", "-c", "x"]),
            ("corepack", &["wscript", "x.vbs"]),
            ("env", &["rustup", "run", "stable", "mshta", "x"]),
        ] {
            let said = judge(exe, args).unwrap_err();
            assert_eq!(said.code, ErrorCode::PermissionDenied, "{exe} {args:?}");
            assert!(said.message.contains("would run"), "{exe} {args:?}: {}", said.message);
        }
        // Git is run directly or not at all, whoever starts it.
        for (exe, args) in [("rustup", &["run", "stable", "git", "push"][..]), ("npx", &["git", "reset", "--hard"]), ("corepack", &["git", "clean", "-fd"])] {
            let said = judge(exe, args).unwrap_err();
            assert!(said.message.contains("run git directly"), "{exe} {args:?}: {}", said.message);
        }
        // What runs code asks every time; what only looks stays plain; what cannot be read asks every time.
        assert_eq!(judge("rustup", &["run", "stable", "cargo", "build"]).unwrap(), ExeKind::Interpreter);
        assert_eq!(judge("rustup", &["run", "stable", "echo", "hi"]).unwrap(), ExeKind::Plain);
        assert_eq!(judge("npx", &["prettier", "--check", "."]).unwrap(), ExeKind::Interpreter, "npx always fetches and runs a package");
        assert_eq!(judge("pnpx", &["prettier"]).unwrap(), ExeKind::Interpreter);
        assert_eq!(judge("corepack", &["pnpm", "install"]).unwrap(), ExeKind::Interpreter);
        assert_eq!(judge("corepack", &["pnpm", "ls"]).unwrap(), ExeKind::Plain);
        assert_eq!(judge("npx", &["-c", "echo x"]).unwrap(), ExeKind::Interpreter);
        assert_eq!(judge("volta", &["run", "--weird", "node"]).unwrap(), ExeKind::Interpreter);
        // The person's own rules are asked about every program a launcher starts.
        let chain = launched_chain("rustup", &words(&["run", "stable", "cargo", "publish"]));
        assert_eq!(chain, vec![("cargo".to_string(), words(&["publish"]))]);
    }

    /// OA8-01 (SE-02): the regression table of git. A row is a command and what it adds to `Execute`: `Destructive` for what cannot be taken back, `ExternalEffect` (with `Network` and
    /// `Remote` when another computer is reached) for what changes someone else's system; both ask every time in every mode, full access included.
    #[test]
    fn git_commands_that_destroy_work_or_reach_another_computer_are_told_from_the_ones_that_do_not() {
        use PermClass::{Destructive, ExternalEffect, Network, Remote};
        let effect = |args: &[&str]| git_effects(&words(args));
        let destroys: &[&[&str]] = &[
            &["reset", "--hard"],
            &["reset", "--hard", "HEAD~3"],
            &["reset", "--har", "HEAD~1"],
            &["-C", "repo", "reset", "--hard", "origin/main"],
            &["-c", "user.name=x", "reset", "--hard"],
            &["reset", "--merge"],
            &["reset", "--keep", "main"],
            &["reset", "--soft", "HEAD~1"],
            &["reset", "--mixed", "abc123"],
            &["reset", "abc123"],
            &["reset", "HEAD~1"],
            &["clean", "-fd"],
            &["clean", "-fdx"],
            &["clean", "-xdf"],
            &["clean", "-f"],
            &["clean", "--force", "-d"],
            &["clean", "-i"],
            &["clean"],
            &["checkout", "--", "."],
            &["checkout", "--", "src/a.rs"],
            &["checkout", "HEAD", "--", "a.txt"],
            &["checkout", "."],
            &["checkout", "-f", "main"],
            &["checkout", "--force", "main"],
            &["checkout", "-B", "main", "origin/main"],
            &["checkout", "-p"],
            &["checkout", "--ours", "f.txt"],
            &["checkout", "HEAD~2", "a.txt", "b.txt"],
            &["restore", "a.txt"],
            &["restore", "."],
            &["restore", "--worktree", "a.txt"],
            &["restore", "--staged", "--worktree", "a.txt"],
            &["restore", "-SW", "a.txt"],
            &["restore", "--source=HEAD~1", "a.txt"],
            &["switch", "-f", "main"],
            &["switch", "--discard-changes", "main"],
            &["switch", "-C", "main"],
            &["switch", "--force-create", "x"],
            &["branch", "-D", "x"],
            &["branch", "-d", "x"],
            &["branch", "--delete", "x"],
            &["branch", "-vD", "x"],
            &["branch", "-f", "main", "HEAD~3"],
            &["branch", "-M", "main"],
            &["tag", "-d", "v1"],
            &["tag", "--delete", "v1"],
            &["tag", "-f", "v1"],
            &["stash", "drop"],
            &["stash", "clear"],
            &["stash", "drop", "stash@{1}"],
            &["rebase", "main"],
            &["rebase", "-i", "HEAD~3"],
            &["rebase", "--onto", "a", "b"],
            &["rebase", "--continue"],
            &["commit", "--amend"],
            &["commit", "--amend", "--no-edit"],
            &["commit", "-m", "x", "--amend"],
            &["commit", "--amen"],
            &["reflog", "expire", "--expire=now", "--all"],
            &["reflog", "delete", "HEAD@{1}"],
            &["gc", "--prune=now"],
            &["gc", "--prune"],
            &["prune"],
            &["prune", "--expire", "now"],
            &["update-ref", "-d", "refs/heads/x"],
            &["update-ref", "refs/heads/main", "abc123"],
            &["symbolic-ref", "-d", "HEAD"],
            &["symbolic-ref", "--delete", "HEAD"],
            &["worktree", "remove", "../x"],
            &["worktree", "prune"],
            &["filter-branch", "--tree-filter", "x"],
            &["filter-repo", "--path", "x"],
            &["notes", "prune"],
            &["notes", "remove", "HEAD"],
            &["notes", "--ref=x", "remove"],
            &["rm", "-f", "a.txt"],
            &["rm", "--force", "a.txt"],
            &["rm", "-rf", "dir"],
            &["remote", "prune", "origin"],
            &["submodule", "deinit", "x"],
        ];
        for argv in destroys {
            assert_eq!(effect(argv), vec![Destructive], "git {argv:?}");
        }
        let pushes: &[&[&str]] = &[
            &["push"],
            &["push", "origin", "main"],
            &["push", "--force"],
            &["push", "-f", "origin", "main"],
            &["push", "--force-with-lease"],
            &["push", "origin", "--delete", "x"],
            &["push", "--tags"],
            &["push", "--dry-run"],
            &["-C", "repo", "push"],
            &["--no-pager", "push", "origin"],
            &["PUSH"],
            &["submodule", "add", "https://example.com/x.git", "x"],
            &["submodule", "update", "--init"],
        ];
        for argv in pushes {
            assert_eq!(effect(argv), vec![Network, Remote, ExternalEffect], "git {argv:?}");
        }
        let changes_elsewhere: &[&[&str]] = &[
            &["remote", "add", "x", "https://example.com/x.git"],
            &["remote", "set-url", "origin", "https://example.com/x.git"],
            &["remote", "remove", "origin"],
            &["remote", "rm", "origin"],
            &["remote", "rename", "a", "b"],
            &["remote", "set-head", "origin", "-a"],
            &["config", "user.name", "x"],
            &["config", "--global", "user.name", "x"],
            &["config", "--add", "k", "v"],
            &["config", "--replace-all", "k", "v"],
            &["config", "--unset", "k"],
            &["config", "--unset-all", "k"],
            &["config", "--edit"],
            &["config", "-e"],
            &["config", "set", "k", "v"],
            &["config", "unset", "k"],
            &["config", "--file", "f.cfg", "k", "v"],
            &["config", "alias.x", "!calc"],
            &["submodule", "sync"],
            &["submodule", "set-url", "x", "https://example.com/x.git"],
            &["maintenance", "start"],
            &["maintenance", "register"],
            &["maintenance", "unregister"],
        ];
        for argv in changes_elsewhere {
            assert_eq!(effect(argv), vec![ExternalEffect], "git {argv:?}");
        }
        // Looking, and the work that builds up a commit or moves between branches, stays plain: `status`, `log` and `diff` are the first things anyone runs.
        let plain: &[&[&str]] = &[
            &[],
            &["--version"],
            &["status"],
            &["status", "--short"],
            &["log", "--oneline"],
            &["log", "-p", "-n", "3"],
            &["diff"],
            &["diff", "HEAD~1"],
            &["show", "HEAD"],
            &["blame", "a.txt"],
            &["branch"],
            &["branch", "-a"],
            &["branch", "-vv"],
            &["branch", "--list"],
            &["branch", "-r"],
            &["branch", "--show-current"],
            &["branch", "-m", "old", "new"],
            &["branch", "new-name"],
            &["tag"],
            &["tag", "-l"],
            &["tag", "v1"],
            &["tag", "-a", "v1", "-m", "x"],
            &["stash"],
            &["stash", "push", "-m", "x"],
            &["stash", "pop"],
            &["stash", "apply"],
            &["stash", "list"],
            &["add", "."],
            &["add", "-A"],
            &["commit", "-m", "x"],
            &["commit", "-a", "-m", "x", "--author=a <a@b>"],
            &["checkout", "main"],
            &["checkout", "-b", "new"],
            &["checkout", "-b", "new", "origin/main"],
            &["checkout", "feature/x"],
            &["checkout", "-"],
            &["checkout", "--detach", "HEAD~1"],
            &["switch", "main"],
            &["switch", "-c", "new"],
            &["restore", "--staged", "a.txt"],
            &["restore", "-S", "a.txt"],
            &["restore", "--staged", "--source=HEAD~1", "a.txt"],
            &["reset"],
            &["reset", "HEAD"],
            &["reset", "HEAD", "--", "a.txt"],
            &["reset", "--", "a.txt"],
            &["reset", "-q"],
            &["reset", "-p"],
            &["clean", "-n"],
            &["clean", "-nd"],
            &["clean", "-fdn"],
            &["clean", "--dry-run", "-fd"],
            &["merge", "main"],
            &["merge", "--abort"],
            &["rebase", "--abort"],
            &["rebase", "--quit"],
            &["pull"],
            &["fetch"],
            &["fetch", "--prune"],
            &["clone", "https://example.com/x.git"],
            &["init"],
            &["cherry-pick", "abc123"],
            &["revert", "abc123"],
            &["config", "user.name"],
            &["config", "--get", "user.name"],
            &["config", "--get-all", "k"],
            &["config", "--get-regexp", "^alias"],
            &["config", "--list"],
            &["config", "-l"],
            &["config", "--global", "--list"],
            &["config", "--show-origin", "--get", "k"],
            &["remote"],
            &["remote", "-v"],
            &["remote", "show", "origin"],
            &["remote", "get-url", "origin"],
            &["remote", "update"],
            &["submodule", "status"],
            &["submodule", "init"],
            &["submodule", "summary"],
            &["gc"],
            &["gc", "--aggressive"],
            &["gc", "--prune=never"],
            &["prune", "-n"],
            &["prune", "--dry-run"],
            &["worktree", "add", "../x"],
            &["worktree", "list"],
            &["notes"],
            &["notes", "add", "-m", "x"],
            &["notes", "show"],
            &["reflog"],
            &["reflog", "show"],
            &["rm", "a.txt"],
            &["rm", "--cached", "a.txt"],
            &["rm", "-r", "--cached", "d"],
            &["symbolic-ref", "HEAD"],
            &["ls-files"],
            &["ls-remote", "origin"],
            &["maintenance", "run"],
            &["describe"],
        ];
        for argv in plain {
            assert_eq!(effect(argv), Vec::<PermClass>::new(), "git {argv:?}");
        }
    }

    #[test]
    fn a_shortened_long_option_is_read_as_the_option_it_shortens_and_what_comes_after_dash_dash_is_a_path() {
        let effect = |args: &[&str]| git_effects(&words(args));
        // Git takes any start of a long option that no other option of the command shares.
        for argv in [&["reset", "--ha"][..], &["reset", "--har", "HEAD~1"], &["clean", "--forc", "-d"], &["commit", "--ame"], &["branch", "--del", "x"], &["restore", "--wor", "a.txt"]] {
            assert_eq!(effect(argv), vec![PermClass::Destructive], "git {argv:?}");
        }
        assert_eq!(effect(&["clean", "--dry", "-fd"]), Vec::<PermClass>::new(), "a shortened --dry-run is a dry run");
        // After `--` every word is a path, never an option.
        assert_eq!(effect(&["log", "--", "--hard"]), Vec::<PermClass>::new());
        assert_eq!(effect(&["reset", "--", "--hard"]), Vec::<PermClass>::new(), "a file called --hard");
        assert_eq!(effect(&["clean", "-n", "--", "-f"]), Vec::<PermClass>::new());
        // The subcommand is read past what stands in front of it, as git does.
        assert_eq!(effect(&["-C", "reset", "status"]), Vec::<PermClass>::new(), "-C takes the folder 'reset'");
        assert_eq!(effect(&["--git-dir", "x/.git", "--work-tree", "x", "push"]).len(), 3);
    }

    #[test]
    fn git_checkout_of_a_name_that_is_a_file_in_the_folder_is_a_restore_and_of_a_branch_is_not() {
        let dir = scratch("checkout");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("sub").join("b.txt"), "b").unwrap();
        let effect = |args: &[&str], at: &Path| git_effect_in(&words(args), Some(at)).map(|e| e.classes);
        let destroys = Some(vec![PermClass::Destructive]);
        assert_eq!(effect(&["checkout", "a.txt"], &dir), destroys, "a file of the folder: git restores it, and the changes in it are gone");
        assert_eq!(effect(&["checkout", "sub"], &dir), destroys, "a folder");
        assert_eq!(effect(&["checkout", "main"], &dir), None);
        assert_eq!(effect(&["checkout", "feature/x"], &dir), None);
        assert_eq!(effect(&["checkout", "-b", "a.txt"], &dir), None, "a new branch named like a file creates a branch");
        // `-C` moves the folder the name is looked up in.
        assert_eq!(effect(&["-C", "sub", "checkout", "b.txt"], &dir), destroys);
        assert_eq!(effect(&["-C", "sub", "checkout", "a.txt"], &dir), None, "a.txt is not in sub");
        // `git reset a.txt` takes a file out of the staging area (unstaging is the common use); `git reset abc123` moves the branch. A name that is a file is a path.
        assert_eq!(effect(&["reset", "a.txt"], &dir), None);
        assert_eq!(effect(&["reset", "HEAD", "a.txt", "sub"], &dir), None);
        assert_eq!(effect(&["reset", "a.txt", "abc123"], &dir), destroys, "one of the names is not a file");
        assert_eq!(effect(&["reset", "abc123"], &dir), destroys);
        assert_eq!(effect(&["reset", "--hard", "a.txt"], &dir), destroys, "a hard reset is never an unstage");
        assert_eq!(effect(&["-C", "sub", "reset", "b.txt"], &dir), None);
        // Without the folder the word alone cannot be told.
        assert_eq!(git_effect(&words(&["checkout", "a.txt"])), None);
        assert_eq!(git_effect(&words(&["reset", "a.txt"])).map(|e| e.classes), destroys, "a name that cannot be looked up is taken for a commit");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_package_registry_command_that_publishes_or_signs_in_reaches_another_computer_and_the_other_ones_do_not() {
        use PermClass::{ExternalEffect, Network, Remote};
        let effect = |exe: &str, args: &[&str]| package_effect(exe, &words(args)).map(|e| e.classes);
        let reaches = Some(vec![Network, Remote, ExternalEffect]);
        for (exe, args) in [
            ("cargo", &["publish"][..]),
            ("cargo", &["publish", "--dry-run"]),
            ("cargo", &["+stable", "publish"]),
            ("cargo", &["--color", "always", "publish"]),
            ("cargo", &["yank", "--version", "1.0.0", "x"]),
            ("cargo", &["owner", "--add", "someone"]),
            ("cargo", &["login"]),
            ("cargo", &["logout"]),
            ("npm", &["publish"]),
            ("NPM.CMD", &["publish"]),
            ("npm", &["--prefix", ".", "publish"]),
            ("npm", &["login"]),
            ("npm", &["adduser"]),
            ("npm", &["access", "public"]),
            ("npm", &["dist-tag", "add", "x@1", "latest"]),
            ("npm", &["unpublish", "x"]),
            ("npm", &["deprecate", "x", "gone"]),
            ("pnpm", &["publish"]),
            ("yarn", &["publish"]),
            ("yarn", &["npm", "publish"]),
            ("twine", &["upload", "dist/x.whl"]),
        ] {
            assert_eq!(effect(exe, args), reaches, "{exe} {args:?}");
        }
        for (exe, args) in [
            ("cargo", &["test"][..]),
            ("cargo", &["test", "publish"]),
            ("cargo", &["build"]),
            ("cargo", &["metadata"]),
            ("cargo", &["--version"]),
            ("npm", &["install"]),
            ("npm", &["run", "publish"]),
            ("npm", &["test", "login"]),
            ("npm", &["ls"]),
            ("yarn", &["npm", "whoami"]),
            ("pip", &["install", "x"]),
            ("twine", &["check", "dist/x.whl"]),
            ("git", &["status"]),
        ] {
            assert_eq!(effect(exe, args), None, "{exe} {args:?}");
        }
        // The same through `command_effect`, which reads git or a package manager by the program's name, and `.exe` or a path does not hide it.
        assert!(command_effect(r"C:\Program Files\Git\cmd\git.exe", &words(&["push"]), None).is_some());
        assert!(command_effect("cargo.exe", &words(&["publish"]), None).is_some());
        assert!(command_effect("git", &words(&["status"]), None).is_none() && command_effect("hostname", &[], None).is_none());
    }

    #[test]
    fn the_effects_of_several_commands_are_one_effect_that_says_each_thing_once_and_names_the_safer_tools() {
        let one = git_effect(&words(&["reset", "--hard"])).unwrap();
        assert_eq!(one.note(), "[throws away uncommitted changes; safer: git.reset (keeps a backup ref)] ");
        let push = git_effect(&words(&["push", "--force"])).unwrap();
        assert_eq!(push.note(), "[sends commits to another repository; safer: git.push (never forced)] ");
        assert_eq!(git_effect(&words(&["clean", "-fd"])).unwrap().note(), "[deletes the files git does not track, for good] ", "nothing of Aqua does this, so nothing is named");
        let both = merge_effects([one.clone(), push.clone(), one.clone()]).unwrap();
        assert_eq!(both.classes, vec![PermClass::Destructive, PermClass::Network, PermClass::Remote, PermClass::ExternalEffect]);
        assert_eq!(both.why, "throws away uncommitted changes; sends commits to another repository");
        assert_eq!(both.safer, vec!["git.reset (keeps a backup ref)", "git.push (never forced)"]);
        assert!(merge_effects(Vec::new()).is_none());
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_term_rules_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn python_is_found_where_it_installs_itself_newest_first_and_a_folder_without_python_is_not() {
        let root = scratch("python");
        let local = root.join("local");
        let pf = root.join("pf");
        for (base, name, real) in [(local.join("Programs").join("Python"), "Python313", true), (local.join("Programs").join("Python"), "Python39", true), (pf.clone(), "Python312", true), (pf.clone(), "Python311", false), (pf.clone(), "Common Files", true)] {
            let d = base.join(name);
            std::fs::create_dir_all(d.join("Scripts")).unwrap();
            if real {
                std::fs::write(d.join("python.exe"), b"").unwrap();
            }
        }
        let found: Vec<String> = discover_dirs_in(Some(local), Some(pf)).iter().map(|p| p.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(
            found,
            ["local/Programs/Python/Python313", "local/Programs/Python/Python313/Scripts", "pf/Python312", "pf/Python312/Scripts", "local/Programs/Python/Python39", "local/Programs/Python/Python39/Scripts"],
            "newest first, each with its Scripts folder; no python.exe means no install"
        );
        assert!(discover_dirs_in(None, None).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn npm_and_npx_run_through_node_with_no_shell_and_nothing_else_is_a_script_host() {
        let root = scratch("npm");
        let node_dir = root.join("nodejs");
        std::fs::create_dir_all(node_dir.join("node_modules").join("npm").join("bin")).unwrap();
        std::fs::write(node_dir.join("node.exe"), b"").unwrap();
        std::fs::write(node_dir.join("node_modules").join("npm").join("bin").join("npm-cli.js"), b"").unwrap();
        let cfg = TermConfig { path_dirs: vec![node_dir.to_string_lossy().into_owned()], discover: false, ..TermConfig::default() };
        let (node, script) = resolve_script_host(&cfg, "npm").expect("npm is a script host").unwrap();
        assert!(node.ends_with("node.exe") && script.ends_with("npm-cli.js"), "{node:?} {script:?}");
        assert!(resolve_script_host(&cfg, "NPM.CMD").is_some(), "the shim's own name works");
        assert!(resolve_script_host(&cfg, r"C:\Program Files\nodejs\npm.cmd").is_none(), "a path goes by the ordinary rules, which refuse .cmd");
        assert!(resolve_script_host(&cfg, "git").is_none() && resolve_script_host(&cfg, "node").is_none());
        // npx has no script next to node here: said plainly, not guessed.
        let npx = resolve_script_host(&cfg, "npx").expect("npx is a script host");
        assert_eq!(npx.unwrap_err().code, ErrorCode::NotFound);
        // No Node.js at all.
        let empty = TermConfig { path_dirs: vec![], discover: false, ..TermConfig::default() };
        assert_eq!(resolve_script_host(&empty, "npm").unwrap().unwrap_err().code, ErrorCode::NotFound);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_the_build_and_test_commands_of_a_project_count_as_project_checks() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for (exe, args) in [
            ("cargo", &["test"][..]),
            ("cargo.exe", &["test", "-p", "x", "--", "--nocapture"]),
            ("cargo", &["check", "--workspace"]),
            ("cargo", &["clippy"]),
            ("cargo", &["build", "--release"]),
            ("npm", &["test"]),
            ("python", &["-m", "pytest", "-q"]),
            ("python", &["-m", "pytest", "-x", "-k", "parses and not slow", "--tb=short", "tests/test_a.py::test_b"]),
            ("python.exe", &["-m", "unittest"]),
            ("python.exe", &["-m", "unittest", "discover", "-v"]),
            ("cargo", &["test", "parses_the_header", "-p", "aqua-core", "--features=a,b", "--", "--test-threads=1", "--exact"]),
            ("cargo", &["clippy", "--workspace", "--", "-D", "warnings"]),
            ("cargo", &["build", "--release", "--locked", "--message-format", "json"]),
            ("npm", &["test", "--silent"]),
            ("npm", &["t"]),
        ] {
            assert!(is_project_check(exe, &a(args)), "{exe} {args:?}");
        }
        for (exe, args) in [
            ("cargo", &["run"][..]),
            ("cargo", &["install", "x"]),
            ("cargo", &["test", "--config", "build.rustc-wrapper='calc'"]),
            ("cargo", &["test", "--config=build.rustc-wrapper='calc'"]),
            ("cargo", &["test", "-Zunstable-options"]),
            ("cargo", &["test", "--manifest-path", r"C:\elsewhere\Cargo.toml"]),
            ("cargo", &["xtask"]),
            ("npm", &["run", "test"]),
            ("npm", &["install"]),
            ("npm", &["test", "--script-shell=calc.exe"]),
            ("npx", &["something"]),
            ("python", &["script.py"]),
            ("python", &["-m", "http.server"]),
            ("python", &["-c", "print(1)"]),
            ("python", &["-m", "pytest", "-p", "evil_plugin"]),
            ("python", &["-m", "pytest", "-c", "evil.ini"]),
            ("python", &["-m", "pytest", "-o", "addopts=-p evil"]),
            ("python", &["-m", "pytest", "--rootdir=C:/elsewhere"]),
            ("python", &["-m", "pytest", "--pyargs", "somepkg"]),
            ("python", &["-m", "pytest", "--junitxml=C:/Users/x/out.xml"]),
            ("python", &["-m", "pytest", "@args.txt"]),
            ("python", &["-m", "pytest", r"C:\elsewhere\tests"]),
            ("python", &["-m", "pytest", "../other/tests"]),
            ("python", &["-m", "pytest", "-k"]),
            ("git", &["status"]),
            // The short cargo aliases can be redefined by the project's own `.cargo/config.toml`, so only the full names count.
            ("cargo", &["t"]),
            ("cargo", &["b"]),
            ("cargo", &["c"]),
            // Where cargo writes or reads from, and what it passes to the compiler or the test program.
            ("cargo", &["test", "--target-dir", "D:/elsewhere"]),
            ("cargo", &["build", "--target-dir=D:/elsewhere"]),
            ("cargo", &["build", "--artifact-dir", "out"]),
            ("cargo", &["build", "--man=C:/x/Cargo.toml"]),
            ("cargo", &["--locked", "test"]),
            ("cargo", &["+nightly", "test"]),
            ("cargo", &["clippy", "--", "-C", "linker=calc.exe"]),
            ("cargo", &["clippy", "--", "--cap-lints", "allow"]),
            ("cargo", &["build", "--", "-Clink-arg=x"]),
            ("cargo", &["test", "--", "--logfile", "C:/x/log.txt"]),
            ("cargo", &["test", "-p", "../other"]),
            ("cargo", &["test", "-p", "-Zfoo"]),
            ("cargo", &["test", "--features", "a/b/c"]),
            // npm reads abbreviated options (`--script-s` is `--script-shell`), so only the listed ones are accepted, and nothing is passed on.
            ("npm", &["test", "--script-s=calc.exe"]),
            ("npm", &["test", "--user=x"]),
            ("npm", &["test", "--glob=x"]),
            ("npm", &["test", "--regis=http://127.0.0.1:1/"]),
            ("npm", &["test", "--", "--config", "evil.js"]),
            ("npm", &["--prefix", "D:/x", "test"]),
        ] {
            assert!(!is_project_check(exe, &a(args)), "{exe} {args:?}");
        }
    }

    #[test]
    fn programs_that_load_remote_code_or_change_the_system_are_refused_outright() {
        for exe in ["regsvr32", "rundll32.exe", "mshta", "certutil", "bitsadmin", "msiexec", "cmstp", "odbcconf", "mavinject", "wmic", "reg", "schtasks", "netsh", "bcdedit"] {
            assert_eq!(classify_exe(exe).unwrap_err().code, ErrorCode::PermissionDenied, "{exe}");
        }
        for exe in ["hostname", "ipconfig", "whoami", "git", "tasklist", "ping"] {
            assert!(classify_exe(exe).is_ok(), "{exe} must stay usable");
        }
    }

    #[cfg(windows)]
    #[test]
    fn only_listed_programs_of_the_system_folder_run_and_the_list_can_be_extended() {
        let cfg = TermConfig::default();
        for ok in ["hostname", "ipconfig", "whoami", "tasklist", "systeminfo"] {
            assert!(resolve_exe(&cfg, ok).is_ok(), "{ok} must run");
        }
        // Real programs that exist in the system folder but can damage or reach out: refused by the list, not by luck.
        for no in ["shutdown", "format", "diskpart", "ping", "nslookup", "net", "taskkill", "vssadmin", "cipher", "robocopy", "xcopy", "sort", "tree", "where", "attrib", "dism", "sfc", "powercfg", "mountvol", "wbadmin"] {
            let e = resolve_exe(&cfg, no);
            assert!(e.is_err(), "{no} must not run by default");
            if let Err(err) = e {
                assert_eq!(err.code, ErrorCode::PermissionDenied, "{no}: {}", err.message);
            }
        }
        let msg = resolve_exe(&cfg, "ping").unwrap_err().message;
        assert!(msg.contains("extra_system_programs") && msg.contains("hostname"), "{msg}");
        // The person can extend the list in term.json.
        let more = TermConfig { extra_system_programs: vec!["ping".into(), "where.exe".into()], ..TermConfig::default() };
        assert!(resolve_exe(&more, "ping").is_ok() && resolve_exe(&more, "where").is_ok());
        assert!(resolve_exe(&more, "shutdown").is_err(), "extending the list does not open the rest");
        // A full path into the system folder is held to the same list.
        let sys = std::path::PathBuf::from(std::env::var("SystemRoot").unwrap_or("C:/Windows".into())).join("System32");
        assert!(resolve_exe(&cfg, &sys.join("shutdown.exe").to_string_lossy()).is_err());
        assert!(resolve_exe(&cfg, &sys.join("hostname.exe").to_string_lossy()).is_ok());
    }

    #[test]
    fn interpreters_are_flagged_and_ordinary_programs_are_plain() {
        for exe in ["python", "python3.12", "node.exe", "deno", "ruby", "java", "dotnet"] {
            assert_eq!(classify_exe(exe).unwrap(), ExeKind::Interpreter, "{exe}");
        }
        for exe in ["git", "cargo", "ipconfig", "hostname", "whoami", "rg"] {
            assert_eq!(classify_exe(exe).unwrap(), ExeKind::Plain, "{exe}");
        }
    }

    #[test]
    fn shell_metacharacters_scripts_and_unlisted_folders_are_refused() {
        let cfg = TermConfig::default();
        for exe in ["git && calc", "git;calc", "git|more", "ec\"ho", "%COMSPEC%", "a*b", "", "x\0y"] {
            assert_eq!(resolve_exe(&cfg, exe).unwrap_err().code, ErrorCode::InvalidArguments, "{exe}");
        }
        for exe in ["build.bat", "run.cmd", "x.ps1", "evil.vbs", "a.lnk", "setup.msi"] {
            assert_eq!(resolve_exe(&cfg, exe).unwrap_err().code, ErrorCode::PermissionDenied, "{exe}");
        }
        assert_eq!(resolve_exe(&cfg, "definitely-not-a-program").unwrap_err().code, ErrorCode::NotFound);
        let temp = std::env::temp_dir().join("evil.exe");
        assert_eq!(resolve_exe(&cfg, &temp.to_string_lossy()).unwrap_err().code, ErrorCode::NotFound);
    }

    #[cfg(windows)]
    #[test]
    fn system_programs_resolve_and_a_program_outside_the_folders_does_not() {
        let cfg = TermConfig::default();
        let host = resolve_exe(&cfg, "hostname").unwrap();
        assert!(host.to_string_lossy().to_lowercase().ends_with("hostname.exe"));
        // A real exe that exists but lives outside the configured folders.
        let outside = std::env::current_exe().unwrap();
        assert_eq!(resolve_exe(&cfg, &outside.to_string_lossy()).unwrap_err().code, ErrorCode::PermissionDenied);
        // A folder can be added deliberately.
        let cfg2 = TermConfig { path_dirs: vec![outside.parent().unwrap().to_string_lossy().to_string()], ..TermConfig::default() };
        assert!(resolve_exe(&cfg2, &outside.to_string_lossy()).is_ok());
    }

    #[test]
    fn program_names_are_normalised() {
        assert_eq!(program_name(r"C:\Tools\Git.EXE"), "git");
        assert_eq!(program_name("cargo"), "cargo");
        assert_eq!(program_name("/usr/bin/python3"), "python3");
    }

    #[cfg(windows)]
    #[test]
    fn a_program_that_is_a_link_runs_under_its_own_name_but_a_link_to_a_shell_is_still_refused() {
        use std::os::windows::fs::symlink_file;
        let dir = std::env::temp_dir().join(format!("aqua_term_link_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("real_tool.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &target).unwrap();
        let cfg = TermConfig { path_dirs: vec![dir.to_string_lossy().into_owned()], ..TermConfig::default() };
        if symlink_file(&target, dir.join("friendly.exe")).is_ok() {
            let p = resolve_exe(&cfg, "friendly").unwrap();
            assert_eq!(p.file_name().unwrap().to_string_lossy().to_ascii_lowercase(), "friendly.exe", "{p:?}");
            let cmd = std::path::PathBuf::from(std::env::var("SystemRoot").unwrap_or("C:/Windows".into())).join("System32").join("cmd.exe");
            if symlink_file(&cmd, dir.join("harmless.exe")).is_ok() {
                assert!(resolve_exe(&cfg, "harmless").is_err(), "a link to cmd.exe is still cmd.exe");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod git_path_tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }
    fn paths(a: &[&str]) -> Vec<(String, bool)> {
        git_path_uses(&v(a)).unwrap().into_iter().map(|g| (g.path, g.write)).collect()
    }

    #[test]
    fn ordinary_git_commands_name_no_outside_paths() {
        for cmd in [vec!["status"], vec!["log", "--oneline", "-n", "5"], vec!["diff", "HEAD~1", "src/main.rs"], vec!["show", "HEAD:README.md"], vec!["commit", "-m", "msg"], vec!["-c", "user.name=x", "status"], vec!["clone", "https://example.com/r.git"], vec!["fetch", "origin"], vec!["checkout", "-b", "feature/x"]] {
            assert!(paths(&cmd).is_empty(), "{cmd:?} -> {:?}", paths(&cmd));
        }
    }

    #[test]
    fn output_and_directory_options_are_found_in_every_spelling() {
        assert_eq!(paths(&["log", "--output=C:/x/policy.json"]), vec![("C:/x/policy.json".to_string(), true)]);
        assert_eq!(paths(&["log", "--output", "p.txt"]), vec![("p.txt".to_string(), true)]);
        assert_eq!(paths(&["archive", "-o", "a.zip", "HEAD"]), vec![("a.zip".to_string(), true)]);
        assert_eq!(paths(&["archive", "-oa.zip", "HEAD"]), vec![("a.zip".to_string(), true)]);
        assert_eq!(paths(&["format-patch", "-o", "out", "HEAD~1"]), vec![("out".to_string(), true)]);
        assert_eq!(paths(&["-C", "other", "status"]), vec![("other".to_string(), false)]);
        assert_eq!(paths(&["--git-dir=D:/r/.git", "status"]), vec![("D:/r/.git".to_string(), false)]);
        assert_eq!(paths(&["--work-tree", "D:/w", "status"]), vec![("D:/w".to_string(), false)]);
        assert_eq!(paths(&["log", "--OUTPUT=x"]), vec![("x".to_string(), true)], "option names are not case sensitive to this check");
    }

    #[test]
    fn no_index_makes_every_operand_a_read() {
        assert_eq!(paths(&["diff", "--no-index", "NUL", "C:/Users/x/.ssh/id_rsa"]), vec![("NUL".to_string(), false), ("C:/Users/x/.ssh/id_rsa".to_string(), false)]);
        assert_eq!(paths(&["grep", "--no-index", "-e", "secret", "--", "a.txt"]).len(), 2, "pattern and file are both operands, both are checked");
    }

    #[test]
    fn absolute_and_climbing_operands_are_reads_but_remote_addresses_are_not() {
        assert_eq!(paths(&["apply", "C:/patches/x.patch"]), vec![("C:/patches/x.patch".to_string(), false)]);
        assert_eq!(paths(&["hash-object", "../../secret.txt"]), vec![("../../secret.txt".to_string(), false)]);
        assert!(paths(&["clone", "https://host/r.git"]).is_empty());
        assert!(paths(&["remote", "add", "o", "ssh://git@host/r.git"]).is_empty());
    }

    #[test]
    fn destinations_of_clone_init_worktree_and_bundle_are_writes() {
        assert_eq!(paths(&["clone", "https://h/r.git", "dest"]), vec![("dest".to_string(), true)]);
        assert_eq!(paths(&["init", "newrepo"]), vec![("newrepo".to_string(), true)]);
        assert_eq!(paths(&["worktree", "add", "../wt", "main"]), vec![("../wt".to_string(), true)]);
        assert_eq!(paths(&["bundle", "create", "b.bundle", "HEAD"]), vec![("b.bundle".to_string(), true)]);
        assert_eq!(paths(&["bundle", "unbundle", "b.bundle"]), vec![("b.bundle".to_string(), false)]);
    }

    #[test]
    fn config_that_reaches_beyond_the_repository_is_refused() {
        for bad in [vec!["config", "--global", "user.name", "x"], vec!["config", "--system", "core.autocrlf", "true"], vec!["config", "--file", "C:/x", "a.b", "c"], vec!["config", "--file=C:/x", "a.b", "c"]] {
            assert!(git_path_uses(&v(&bad)).is_err(), "{bad:?}");
        }
        assert!(git_path_uses(&v(&["config", "user.name", "x"])).is_ok(), "the repository's own config is fine");
        assert!(git_path_uses(&v(&["config", "--local", "user.name", "x"])).is_ok());
    }

}

#[cfg(test)]
mod exec_rules_tests {
    use super::*;

    fn words(line: &str) -> (String, Vec<String>) {
        let mut it = line.split_whitespace().map(String::from);
        (it.next().unwrap(), it.collect())
    }

    const GOOD: &str = r#"{"rules": [
        {"pattern": "git push", "decision": "forbid", "reason": "pushing is done by hand", "examples": {"match": ["git push origin main", "GIT.exe PUSH"], "not_match": ["git status", "git pull", "gitk push"]}},
        {"pattern": "cargo publish", "decision": "prompt", "examples": {"match": ["cargo publish --dry-run"]}},
        {"pattern": "git reset * --hard", "decision": "prompt", "examples": {"match": ["git reset HEAD~1 --hard"], "not_match": ["git reset --hard"]}},
        {"pattern": "git", "decision": "prompt", "reason": "git asks in this project", "examples": {"match": ["git status"], "not_match": ["cargo test"]}}
    ]}"#;

    #[test]
    fn a_rule_matches_a_program_and_the_start_of_its_arguments_without_regard_to_case() {
        let rules = ExecRules::from_text(GOOD).unwrap();
        assert_eq!(rules.len(), 4);
        let d = |line: &str| {
            let (p, a) = words(line);
            rules.decide(&p, &a)
        };
        assert_eq!(d("git push origin main"), Some((RuleDecision::Forbid, "pushing is done by hand".into())));
        assert_eq!(d("C:/Git/cmd/GIT.EXE Push --force").map(|x| x.0), Some(RuleDecision::Forbid));
        assert_eq!(d("git status").map(|x| x.0), Some(RuleDecision::Prompt), "the shorter rule applies to everything git");
        assert_eq!(d("git reset HEAD~1 --hard").map(|x| x.0), Some(RuleDecision::Prompt));
        assert_eq!(d("cargo publish").map(|x| x.0), Some(RuleDecision::Prompt));
        assert_eq!(d("cargo test"), None);
        assert_eq!(d("gitk push"), None, "the program name is the whole name");
        assert_eq!(d("push git"), None, "order matters");
        assert_eq!(rules.decide("", &[]), None);
    }

    #[test]
    fn of_the_rules_that_match_the_strictest_wins_whatever_their_order() {
        let a = ExecRules::from_text(r#"{"rules": [
            {"pattern": "git", "decision": "prompt", "examples": {"match": ["git log"]}},
            {"pattern": "git push", "decision": "forbid", "examples": {"match": ["git push"]}}]}"#).unwrap();
        let b = ExecRules::from_text(r#"{"rules": [
            {"pattern": "git push", "decision": "forbid", "examples": {"match": ["git push"]}},
            {"pattern": "git", "decision": "prompt", "examples": {"match": ["git log"]}}]}"#).unwrap();
        let (p, args) = words("git push origin");
        assert_eq!(a.decide(&p, &args).unwrap().0, RuleDecision::Forbid);
        assert_eq!(b.decide(&p, &args).unwrap().0, RuleDecision::Forbid);
    }

    #[test]
    fn a_file_with_one_wrong_rule_is_refused_whole_and_says_which_rule_and_why() {
        let bad = |rule: &str| ExecRules::from_text(&format!(r#"{{"rules": [
            {{"pattern": "git status", "decision": "prompt", "examples": {{"match": ["git status"]}}}},
            {rule}]}}"#)).unwrap_err();
        // an example that is meant to match and does not
        assert!(bad(r#"{"pattern": "git push", "decision": "forbid", "examples": {"match": ["git pull"]}}"#).contains("rule 2") && bad(r#"{"pattern": "git push", "decision": "forbid", "examples": {"match": ["git pull"]}}"#).contains("is meant to match and does not"));
        // an example that is not meant to match and does
        assert!(bad(r#"{"pattern": "git", "decision": "forbid", "examples": {"match": ["git x"], "not_match": ["git status"]}}"#).contains("is not meant to match and does"));
        // no example that proves the rule catches something
        assert!(bad(r#"{"pattern": "git push", "decision": "forbid"}"#).contains("at least one command under examples.match"));
        assert!(bad(r#"{"pattern": "git push", "decision": "forbid", "examples": {"match": []}}"#).contains("at least one command"));
        // allow would loosen
        assert!(bad(r#"{"pattern": "git push", "decision": "allow", "examples": {"match": ["git push"]}}"#).contains("only make commands ask"));
        assert!(bad(r#"{"pattern": "git push", "decision": "maybe", "examples": {"match": ["git push"]}}"#).contains("\"prompt\" or \"forbid\""));
        // patterns that name no program, or a path, or too much
        for pattern in ["", "   ", "* push", "C:/git/git.exe push", "..\\git push"] {
            let text = format!(r#"{{"rules": [{{"pattern": {pattern:?}, "decision": "forbid", "examples": {{"match": ["git push"]}}}}]}}"#);
            assert!(ExecRules::from_text(&text).unwrap_err().contains("must start with a program name"), "{pattern:?}");
        }
        let long = (0..13).map(|i| format!("a{i}")).collect::<Vec<_>>().join(" ");
        let text = format!(r#"{{"rules": [{{"pattern": "git {long}", "decision": "forbid", "examples": {{"match": ["git {long}"]}}}}]}}"#);
        assert!(ExecRules::from_text(&text).is_err());
        // not the shape
        for text in ["", "[]", "{}", r#"{"rules": 1}"#, r#"{"rules": [], "extra": 1}"#, r#"{"rules": [{"pattern": "git", "decision": "forbid", "examples": {"match": ["git"]}, "extra": 1}]}"#, "not json"] {
            assert!(ExecRules::from_text(text).is_err(), "{text:?}");
        }
        // too many
        let many = (0..201).map(|i| format!(r#"{{"pattern": "p{i}", "decision": "prompt", "examples": {{"match": ["p{i}"]}}}}"#)).collect::<Vec<_>>().join(",");
        assert!(ExecRules::from_text(&format!(r#"{{"rules": [{many}]}}"#)).unwrap_err().contains("at most 200"));
    }

    #[test]
    fn shell_segments_allow_and_and_reject_a_pipe() {
        let ok = shell_segments(r#"cargo test --offline && python -m py_compile "a file.py""#).unwrap();
        assert_eq!(ok[0].0, "cargo");
        assert_eq!(ok[1].1, vec!["-m", "py_compile", "a file.py"]);
        assert!(shell_segments("git status | more").unwrap_err().contains("pipes"));
        assert!(shell_segments("powershell -enc AAA").unwrap_err().contains("shell"));
        assert!(shell_segments("cmd /c echo hi").is_err());
    }

    #[test]
    fn shell_segments_respects_quotes() {
        // Inside quotes the characters of a shell are plain text, and `&&` does not split.
        let one = shell_segments(r#"git commit -m "fix: a && b; c | d $HOME (e) `f` > g < h & i""#).unwrap();
        assert_eq!(one.len(), 1, "{one:?}");
        assert_eq!((one[0].0.as_str(), one[0].1.len()), ("git", 3));
        assert_eq!(one[0].1[2], "fix: a && b; c | d $HOME (e) `f` > g < h & i");
        let single = shell_segments("echo 'a;b' && echo \"c|d\"").unwrap();
        assert_eq!(single, vec![("echo".to_string(), vec!["a;b".to_string()]), ("echo".to_string(), vec!["c|d".to_string()])]);
        // The other kind of quote inside a quote is text too, and a quote closes only with its own kind.
        assert_eq!(shell_segments(r#"echo "it's && fine""#).unwrap()[0].1, vec!["it's && fine".to_string()]);
        // Outside quotes every one of them is still refused, and a quote that closes before them does not hide them.
        for bad in ["git status | more", "git status; rm x", "echo $HOME", "echo (x)", "echo x > f", "echo x < f", "echo `x`", "echo x &", r#"echo "a" | more"#, r#"echo "a"; rm x"#, r#"echo 'a' $b"#, r#"echo "a"$HOME"#] {
            assert!(shell_segments(bad).is_err(), "{bad}");
        }
        assert!(shell_segments("git status | more").unwrap_err().contains("pipes"));
        // A line break is refused inside quotes too, and so is a quote that is never closed.
        assert!(shell_segments("echo \"a\nb\"").unwrap_err().contains("newlines"));
        assert!(shell_segments("echo \"a\r\nb\"").is_err());
        assert!(shell_segments(r#"echo "never closed && rm x"#).unwrap_err().contains("quote"));
        assert!(shell_segments("echo 'x").is_err());
        // The limits are the same: four programs, none empty, no shell as the program, no encoded command.
        assert!(shell_segments("a && b && c && d && e").unwrap_err().contains("at most 4"));
        assert!(shell_segments(r#"a && "" && c"#).unwrap_err().contains("empty"));
        assert!(shell_segments("a &&").unwrap_err().contains("empty"));
        assert!(shell_segments(r#"echo "x" && cmd /c dir"#).unwrap_err().contains("shell"));
    }

    /// OA7-01 (TM-10): Maven and Gradle exist on Windows as batch files only; they are refused with the reason, whatever the spelling, and a real `.exe` of the same name still runs.
    #[test]
    fn maven_and_gradle_are_refused_because_they_need_cmd_exe() {
        let cfg = TermConfig { discover: false, ..TermConfig::default() };
        for exe in ["mvn", "MVN", "mvn.cmd", "mvnw", "mvnw.cmd", "gradle", "gradle.bat", "gradlew", "gradlew.bat", r"C:\proj\mvnw.cmd", "./gradlew.bat"] {
            let e = resolve_exe(&cfg, exe).unwrap_err();
            assert_eq!(e.code, ErrorCode::PermissionDenied, "{exe}");
            assert!(e.message.contains("batch file") && e.message.contains("cmd.exe"), "{exe}: {}", e.message);
        }
        // A name that is only unknown is still just not found, and another `.cmd` is told how a Node.js tool runs.
        assert_eq!(resolve_exe(&cfg, "definitely-not-a-program").unwrap_err().code, ErrorCode::NotFound);
        assert!(resolve_exe(&cfg, "build.cmd").unwrap_err().message.contains("bare name"));
        // With an `.exe` of that name in a program folder (a build of Maven that has one), the name runs.
        let dir = std::env::temp_dir().join(format!("aqua_rules_mvn_exe_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mvn.exe"), b"").unwrap();
        let with = TermConfig { path_dirs: vec![dir.to_string_lossy().to_string()], discover: false, ..TermConfig::default() };
        assert!(resolve_exe(&with, "mvn").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_blocked_programs_are_listed_once_each() {
        let mut seen = std::collections::BTreeSet::new();
        let twice: Vec<&str> = BLOCKED.iter().map(|(n, _)| *n).filter(|n| !seen.insert(*n)).collect();
        assert!(twice.is_empty(), "listed twice: {twice:?}");
        // Every program that term.shell refuses to start as a shell is also blocked for term.run.
        for shell in ["cmd", "powershell", "pwsh", "bash", "sh"] {
            assert!(BLOCKED.iter().any(|(n, _)| *n == shell), "{shell}");
        }
    }

    #[test]
    fn no_rules_and_an_empty_list_decide_nothing() {
        assert!(ExecRules::none().is_empty());
        let empty = ExecRules::from_text(r#"{"rules": []}"#).unwrap();
        assert!(empty.is_empty() && empty.decide("git", &["push".into()]).is_none());
    }
}
