use anyhow::Result;
use aqua_core::memory::SolutionsDb;
use std::io;

/// How many times Ctrl+C was pressed since the client started (read by the run loop of `aqua_core::headless`).
static INTERRUPTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// What the person types at a terminal, a line at a time, for the terminal view (card OA10-07): read in a thread, so that a line typed while a run goes is heard at once. The channel closes
/// when the input ends. A line is read at most 64 KiB at a time.
fn typed_lines() -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::{BufRead, Read};
        let mut stdin = io::stdin().lock();
        loop {
            let mut line = String::new();
            match (&mut stdin).take(64 * 1024).read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    if tx.send(line.trim_end_matches(['\r', '\n']).to_string()).is_err() {
                        return;
                    }
                }
            }
        }
    });
    rx
}

/// `aqua-cli acp` (card OA10-07): the Agent Client Protocol on standard input and output, for an editor that starts it. What it does is in `aqua_core::headless::acp`; this gives it the three
/// streams. Standard output carries protocol frames and nothing else; what the process says about itself goes to standard error.
fn acp_command(args: &[String]) -> i32 {
    use std::io::IsTerminal;
    let data = aqua_mcp_adapters::browser_mcp::health::aqua_workspace_root().join("data");
    let acp_io = aqua_core::headless::acp::AcpIo {
        input: Box::new(io::BufReader::new(io::stdin())),
        output: Box::new(io::stdout()),
        log: Box::new(io::stderr()),
        stdin_is_terminal: io::stdin().is_terminal(),
        backoff_unit: std::time::Duration::from_millis(500),
    };
    aqua_core::headless::acp::run(args, &data, acp_io)
}

/// `aqua-cli run | runs | stop | steer | api | preapprove | preapprovals | tui` (cards OA10-05 and OA10-07): the core's HTTP API from a terminal or a script. The words and what they do are in
/// `aqua_core::headless`; this gives it the streams, a Ctrl+C counter, what was piped in, whether the output is a terminal, the data folder and the document of the API. For `tui` it also
/// gives the lines the person types.
fn headless_command(args: &[String]) -> i32 {
    use std::io::{IsTerminal, Read};
    // Ctrl+C is counted, not fatal: the first press asks the core to end the run, the second leaves.
    std::thread::spawn(|| {
        if let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
            rt.block_on(async {
                while tokio::signal::ctrl_c().await.is_ok() {
                    INTERRUPTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            });
        }
    });
    let stdin_is_terminal = io::stdin().is_terminal();
    // What is piped in is added to the goal of `run`. It is read in a thread and waited for a few seconds only: a standard input that a parent left open and never writes to must not hold
    // the command for ever.
    let piped = if stdin_is_terminal || !matches!(args.first().map(String::as_str), Some("run" | "tui")) {
        None
    } else {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (b, d) = (buffer.clone(), done.clone());
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            let mut stdin = io::stdin().lock();
            while let Ok(n) = stdin.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut held = b.lock().unwrap_or_else(|e| e.into_inner());
                if held.len() < 64 * 1024 {
                    held.extend_from_slice(&chunk[..n]);
                }
            }
            d.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !done.load(std::sync::atomic::Ordering::SeqCst) && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let held = buffer.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let text = String::from_utf8_lossy(&held).trim_start_matches('\u{feff}').to_string();
        (!text.trim().is_empty()).then_some(text)
    };
    let stdout_is_terminal = io::stdout().is_terminal();
    let color = stdout_is_terminal && std::env::var_os("NO_COLOR").is_none() && (cfg!(not(windows)) || ["WT_SESSION", "TERM", "ANSICON"].iter().any(|v| std::env::var_os(v).is_some()));
    let data = aqua_mcp_adapters::browser_mcp::health::aqua_workspace_root().join("data");
    // A person at a terminal is asked to type yes; a script is never asked (nothing is made for it).
    let confirm = |shown: &str| -> bool {
        if !stdin_is_terminal {
            return false;
        }
        print!("{shown}\nType yes to go on: ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut line = String::new();
        io::stdin().read_line(&mut line).is_ok() && line.trim().eq_ignore_ascii_case("yes")
    };
    let (mut out, mut err) = (io::stdout().lock(), io::stderr().lock());
    let mut client_io = aqua_core::headless::Io {
        out: &mut out,
        err: &mut err,
        stdin_text: piped,
        stdin_is_terminal,
        color,
        interrupts: &INTERRUPTS,
        backoff_unit: std::time::Duration::from_millis(500),
        openapi: aqua_core::api_gateway::openapi::SPEC,
        confirm: &confirm,
    };
    if args.first().map(String::as_str) == Some("tui") {
        // The terminal view hears what a person types; a script has nobody to hear.
        let typed = stdin_is_terminal.then(typed_lines);
        return aqua_core::headless::view::run(args, &data, &mut client_io, typed.as_ref());
    }
    aqua_core::headless::run_cli(args, &data, &mut client_io)
}

/// `aqua-cli config | env | stats | support-bundle` (card OA10-05): what is on this computer, read without a running core. The words and what they do are in `aqua_core::cli_tools`; this gives
/// it the streams, the data folder, the log, what `doctor` says, the home folder and the clock.
fn tools_command(args: &[String]) -> i32 {
    use std::io::IsTerminal;
    let stdin_is_terminal = io::stdin().is_terminal();
    let root = aqua_mcp_adapters::browser_mcp::health::aqua_workspace_root();
    let doctor = |data: &std::path::Path| {
        let (text, _) = aqua_core::doctor::render(&aqua_core::doctor::run(data, false));
        let (probes, _) = aqua_core::doctor::render_lines(&aqua_core::doctor::probe_lines(data, None));
        format!("{text}{probes}")
    };
    let confirm = |shown: &str| -> bool {
        if !stdin_is_terminal {
            return false;
        }
        print!("{shown}\nType yes to go on: ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut line = String::new();
        io::stdin().read_line(&mut line).is_ok() && line.trim().eq_ignore_ascii_case("yes")
    };
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let (mut out, mut err) = (io::stdout().lock(), io::stderr().lock());
    let mut env = aqua_core::cli_tools::Env {
        out: &mut out,
        err: &mut err,
        data_dir: root.join("data"),
        log_file: root.join("logs").join("core.log"),
        home: std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).ok(),
        doctor: &doctor,
        stdin_is_terminal,
        confirm: &confirm,
        var: &var,
        now_secs: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
    };
    aqua_core::cli_tools::run_cli(args, &mut env)
}

/// `aqua-cli secret-set NAME` reads the value from standard input (so it never appears in a shell
/// history or a process list) and stores it in the operating system's credential store.
/// `secret-check NAME` says whether one is stored, without printing it; `secret-delete NAME` removes it.
fn secret_command(args: &[String]) -> Result<()> {
    use aqua_native_tools::secrets::{valid_name, CredentialManager, Secrets};
    let (cmd, name) = (args[1].as_str(), args.get(2).map(String::as_str).unwrap_or(""));
    if !valid_name(name) {
        anyhow::bail!("usage: aqua-cli {cmd} NAME   (NAME: letters, digits, '_', '-', '/', '.')");
    }
    let store = CredentialManager;
    match cmd {
        "secret-set" => {
            eprintln!("Type or paste the secret for '{name}', then press Enter (it is not shown again):");
            let mut value = String::new();
            io::stdin().read_line(&mut value)?;
            // Windows PowerShell 5.1 puts a byte-order mark in front of piped text; it is never part of a secret.
            let value = value.trim_start_matches('\u{feff}').trim_end_matches(['\r', '\n']);
            if value.is_empty() {
                anyhow::bail!("nothing was entered; nothing was stored");
            }
            store.set(name, value).map_err(|e| anyhow::anyhow!(e))?;
            println!("Stored '{name}' in the credential store ({} characters).", value.chars().count());
        }
        "secret-check" => match store.get(name).map_err(|e| anyhow::anyhow!(e))? {
            Some(v) => println!("'{name}' is stored ({} characters).", v.chars().count()),
            None => println!("'{name}' is not stored."),
        },
        _ => println!("{}", if store.delete(name).map_err(|e| anyhow::anyhow!(e))? { format!("Deleted '{name}'.") } else { format!("'{name}' was not stored.") }),
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if matches!(args.get(1).map(String::as_str), Some("secret-set" | "secret-check" | "secret-delete")) {
        return secret_command(&args);
    }
    
    // `aqua-cli acp`: the Agent Client Protocol for an editor (card OA10-07). Standard output is for protocol frames only, so nothing else is printed before or after it.
    if args.get(1).map(String::as_str) == Some("acp") {
        let code = acp_command(&args[1..]);
        std::process::exit(code);
    }

    // `aqua-cli run | runs | stop | steer | api | preapprove | preapprovals | tui`: the core's HTTP API as a client (cards OA10-05 and OA10-07).
    if args.get(1).is_some_and(|c| aqua_core::headless::COMMANDS.contains(&c.as_str())) {
        let code = headless_command(&args[1..]);
        std::process::exit(code);
    }

    // `aqua-cli config | env | stats | support-bundle`: what is on this computer, read without a running core (card OA10-05).
    if args.get(1).is_some_and(|c| aqua_core::cli_tools::COMMANDS.contains(&c.as_str())) {
        let code = tools_command(&args[1..]);
        std::process::exit(code);
    }

    // `aqua-cli doctor [--fix] [--live] [--data DIR]`: check (and on first run, set up) the tool side of an install. A second report follows the first: one line
    // `key=value current|drift` for each fact about the two token files (never their content), and with `--live` one health probe of the running core on 127.0.0.1:8767
    // (a single GET of /api/v1/health, over after 5 seconds at the latest). The exit code is 1 when a check failed or any of the second report's lines is `drift`.
    if args.get(1).map(String::as_str) == Some("doctor") {
        let fix = args.iter().any(|a| a == "--fix");
        let live = args.iter().any(|a| a == "--live");
        let data = args.iter().position(|a| a == "--data").and_then(|i| args.get(i + 1)).map(std::path::PathBuf::from).unwrap_or_else(|| aqua_mcp_adapters::browser_mcp::health::aqua_workspace_root().join("data"));
        let (text, failed) = aqua_core::doctor::render(&aqua_core::doctor::run(&data, fix));
        print!("{text}");
        let (probes, drift) = aqua_core::doctor::render_lines(&aqua_core::doctor::probe_lines(&data, live.then_some(aqua_core::doctor::CORE_URL)));
        print!("{probes}");
        std::process::exit(if failed || drift { 1 } else { 0 });
    }
    // `aqua-cli logs [--run ID] [--level L] [--tail N] [--json] [--file PATH]`: reads the core's log back (card OA3-04): the lines of one run, the lines at a level and above, the last N.
    // It reads `AQUA_LOG_FILE` (the file a core writes when it is told to), else `logs/core.log` of the workspace (where the desktop app keeps what the core printed), and the older
    // files beside it (`.1` to `.4`). It never talks to the running core.
    if args.get(1).map(String::as_str) == Some("logs") {
        let default_file = aqua_mcp_adapters::browser_mcp::health::aqua_workspace_root().join("logs").join("core.log");
        match aqua_core::logging::run_command(&args[2..], &default_file) {
            Ok(text) => print!("{text}"),
            Err(e) => {
                eprintln!("logs: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    // `aqua-cli audit-key`, `verify-run <id>`, `verify-bundle <file> --key <hex>`: the signed record of what a run did (see `audit_key`).
    if let Some(cmd) = args.get(1).map(String::as_str).filter(|c| matches!(*c, "audit-key" | "verify-run" | "verify-bundle")) {
        let data = args.iter().position(|a| a == "--data").and_then(|i| args.get(i + 1)).map(std::path::PathBuf::from).unwrap_or_else(|| aqua_mcp_adapters::browser_mcp::health::aqua_workspace_root().join("data"));
        match cmd {
            "audit-key" => println!("{}", aqua_core::audit_key::describe(&data)),
            "verify-run" => {
                let Some(id) = args.get(2) else { anyhow::bail!("usage: aqua-cli verify-run <run id> [--data DIR]") };
                let (text, good) = aqua_core::audit_key::verify_run_local(&data, id);
                print!("{text}");
                std::process::exit(if good { 0 } else { 1 });
            }
            _ => {
                let (Some(file), Some(key)) = (args.get(2), args.iter().position(|a| a == "--key").and_then(|i| args.get(i + 1))) else {
                    anyhow::bail!("usage: aqua-cli verify-bundle <file> --key <public key in hex>")
                };
                let (text, good) = aqua_core::audit_key::verify_bundle_file(std::path::Path::new(file), key).map_err(|e| anyhow::anyhow!(e))?;
                print!("{text}");
                std::process::exit(if good { 0 } else { 1 });
            }
        }
        return Ok(());
    }
    // `aqua-cli sign-promoted <pending file> [--data DIR]`: a person signs the promotion that `scripts/eval/quality.py promote` wrote, with the audit key, and it becomes
    // `data/promoted_config.json`, which the core uses at start only when it is signed (card R6-13). It shows what the file holds and asks for a "yes" typed at a terminal: a
    // script or an agent that pipes an answer in is refused, because a promotion changes how Aqua starts and only a person says that it may.
    if args.get(1).map(String::as_str) == Some("sign-promoted") {
        use std::io::IsTerminal;
        let data = args.iter().position(|a| a == "--data").and_then(|i| args.get(i + 1)).map(std::path::PathBuf::from).unwrap_or_else(|| aqua_mcp_adapters::browser_mcp::health::aqua_workspace_root().join("data"));
        let Some(file) = args.get(2).filter(|a| !a.starts_with("--")) else { anyhow::bail!("usage: aqua-cli sign-promoted <pending file> [--data DIR]") };
        if !std::io::stdin().is_terminal() {
            eprintln!("sign-promoted: it needs a person at a terminal to say yes; nothing was signed");
            std::process::exit(1);
        }
        let pending = std::path::PathBuf::from(file);
        let confirm = |shown: &str| -> bool {
            print!("{shown}Type yes to sign it: ");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).is_ok() && line.trim().eq_ignore_ascii_case("yes")
        };
        match aqua_core::promoted::sign_file(&aqua_native_tools::secrets::CredentialManager, &data, &pending, &confirm) {
            Ok(text) => {
                print!("{text}");
                let _ = std::fs::remove_file(&pending);
            }
            Err(e) => {
                eprintln!("sign-promoted: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    // `aqua-cli vault add <folder> | index | status | forget <folder>|--all [--data DIR]`: fill, read and empty the vault from a terminal (card R6-15).
    if args.get(1).map(String::as_str) == Some("vault") {
        let data = args.iter().position(|a| a == "--data").and_then(|i| args.get(i + 1)).map(std::path::PathBuf::from).unwrap_or_else(|| aqua_mcp_adapters::browser_mcp::health::aqua_workspace_root().join("data"));
        // The words after `vault`, without `--data DIR`.
        let mut rest: Vec<String> = Vec::new();
        let mut words = args.iter().skip(2);
        while let Some(w) = words.next() {
            if w == "--data" {
                words.next();
            } else {
                rest.push(w.clone());
            }
        }
        match aqua_core::vault::cli::run(&data, &rest) {
            Ok(text) => println!("{text}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    // `aqua-cli pack import <dir> [--from claude|opencode] | status <dir> | trust <dir> [--kinds text,code] | untrust <dir> [--kinds ...] [--data DIR]`: a repository's `.aqua` pack (card OA11-01).
    // `import` copies the commands, skills and agents of a `.claude` or `.opencode` folder into `.aqua` (never over a file, never hooks or MCP servers) and trusts nothing. `trust` shows what
    // would be trusted (the code whole) and asks for a "yes" typed at a terminal: a script or an agent that pipes an answer in is refused, because trusting a pack lets its text speak to the
    // agent and, for code, runs its programs, and only a person says that it may.
    if args.get(1).map(String::as_str) == Some("pack") {
        use std::io::IsTerminal;
        let data = args.iter().position(|a| a == "--data").and_then(|i| args.get(i + 1)).map(std::path::PathBuf::from).unwrap_or_else(|| aqua_mcp_adapters::browser_mcp::health::aqua_workspace_root().join("data"));
        let mut rest: Vec<String> = Vec::new();
        let mut words = args.iter().skip(2);
        while let Some(w) = words.next() {
            if w == "--data" {
                words.next();
            } else {
                rest.push(w.clone());
            }
        }
        let key_for = |create: bool| aqua_core::tool_host::pack_trust_key(create);
        let confirm = |shown: &str| -> bool {
            if !std::io::stdin().is_terminal() {
                eprintln!("pack trust: it needs a person at a terminal to say yes; nothing was trusted");
                return false;
            }
            print!("{shown}\nType yes to trust these files: ");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).is_ok() && line.trim().eq_ignore_ascii_case("yes")
        };
        match aqua_core::project_pack::run_cli(&data, &rest, &key_for, &confirm) {
            Ok(text) => print!("{text}"),
            Err(e) => {
                eprintln!("pack: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    // `aqua-cli cmd list | show <name> | run <name> [words] [--folder DIR] [--dry-run] [--background] [--core URL] [--data DIR]`: the commands the person wrote in `data/commands` (card OA11-02).
    // `list` and `show` read the files. `run --dry-run` puts the words in and prints the goal and the steps, and runs nothing. `run` asks the running core (on this computer only, with the API token
    // read from the data folder) to start it: the core runs the steps through its tool host, where each asks like any command, and then the agent. The core needs `AQUA_COMMANDS=on`.
    if args.get(1).map(String::as_str) == Some("cmd") {
        let data = args.iter().position(|a| a == "--data").and_then(|i| args.get(i + 1)).map(std::path::PathBuf::from).unwrap_or_else(|| aqua_mcp_adapters::browser_mcp::health::aqua_workspace_root().join("data"));
        let mut rest: Vec<String> = Vec::new();
        let mut words = args.iter().skip(2);
        while let Some(w) = words.next() {
            if w == "--data" {
                words.next();
            } else {
                rest.push(w.clone());
            }
        }
        let open = |base: &str| -> Result<Box<dyn aqua_core::commands::cli::Core>, String> { aqua_core::commands::cli::Loopback::open(base, &data).map(|c| Box::new(c) as Box<dyn aqua_core::commands::cli::Core>) };
        match aqua_core::commands::cli::run_cli(&data, &rest, &open) {
            Ok(text) => print!("{text}"),
            Err(e) => {
                eprintln!("cmd: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    // `aqua-cli purge-recipes`: drop stored recipes that use retired tools and redact the rest.
    if std::env::args().nth(1).as_deref() == Some("purge-recipes") {
        // The same folder every other part of Aqua uses (not whatever folder the command was typed in).
        let workspace = aqua_mcp_adapters::aqua_workspace_root();
        let report = SolutionsDb::new(&workspace)
            .purge_unsafe_recipes()
            .map_err(|e| anyhow::anyhow!(e))?;
        println!(
            "Scanned {} recipes: deleted {} (retired tools), redacted {}.",
            report.scanned, report.deleted, report.redacted
        );
        return Ok(());
    }

    // No subcommand: say what this is and report the real state of the install. (This used to print "Status OK",
    // "100% Operational" and invented microphone and speaker-match figures, none of which were measured.)
    println!("Aqua core v{}", aqua_core::version());
    println!();
    println!("Commands:");
    println!("  aqua-cli doctor [--fix] [--live] [--data DIR]   check the install (--fix sets up what can be set up; --live also asks the running core if it is healthy)");
    println!("  aqua-cli logs [--run ID] [--level L] [--tail N] [--json]   read the core's log: the lines of one run, a level and above, the last N");
    println!("  aqua-cli secret-set|secret-check|secret-delete NAME   manage a secret in the credential store");
    println!("  aqua-cli vault add <folder> | index | status | forget <folder>|--all   choose, read and empty the folders the vault searches");
    println!("  aqua-cli pack import <dir> | status <dir> | trust <dir> | untrust <dir>   a repository's .aqua pack: copy commands, skills and agents from .claude or .opencode, see and trust it");
    println!("  aqua-cli cmd list | show <name> | run <name> [words]   the commands you wrote in data/commands: read them, see what one would do (--dry-run), or ask the running core to start one");
    for line in aqua_core::headless::HELP.iter().chain(aqua_core::cli_tools::HELP.iter()) {
        println!("{line}");
    }
    println!("  aqua-cli purge-recipes                 drop stored recipes that use retired tools and redact the rest");
    println!();
    println!("The assistant itself is the aqua-core program (or the desktop app); this tool does not run it.");
    println!();
    let data = aqua_mcp_adapters::browser_mcp::health::aqua_workspace_root().join("data");
    println!("Checking {} ...", data.display());
    let (text, failed) = aqua_core::doctor::render(&aqua_core::doctor::run(&data, false));
    print!("{text}");
    std::process::exit(if failed { 1 } else { 0 });
}
