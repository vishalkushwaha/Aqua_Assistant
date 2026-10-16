//! The wider list of a project's own checks (card OA4-05, decision D-07 of the OpenCode audit; tasks WF-04 and WF-19).
//!
//! [`super::rules::is_project_check`] knows `cargo check|test|build|clippy`, `npm test` and `python -m pytest|unittest`. A project that is Go, .NET, TypeScript or has a linter had no check that
//! asked once: `go test` and `dotnet test` always asked (their programs are interpreters by name), and so did `pnpm test`, `npm run lint` and `tsc --noEmit`. This is the list that adds them:
//!
//! * `go test|vet|build` with packages (`./...`) and plain flags, never `-o`, `-exec`, `-toolexec`, `-overlay`, `-coverprofile` or any other flag that names a program or a file to write;
//! * `dotnet test|build` with plain flags (`--no-restore`, `--filter <name>`, `-c Debug|Release`, `-v <level>`) and at most one solution or project file of the folder;
//! * `pnpm|yarn|bun|npm test`, and `<manager> run test|lint|typecheck|type-check|check-types|tsc|build`: the script is the project's own code, the same trust as `npm test`;
//! * `tsc --noEmit` with `--pretty <bool>` and `-p <tsconfig file of the folder>`;
//! * `ruff check` with `--output-format <name>` and paths of the folder.
//!
//! It is **off** unless `extended_project_checks` is set in `term.json` or `AQUA_TERM_PROJECT_CHECKS=extended` is in the environment, and then it only decides that a command asks once (for the
//! exact command, in a trusted folder) where it asked every time. Every form is an allowlist: an argument that is not listed makes the command an ordinary interpreter call again.

use super::rules::{has_parent_segment, looks_absolute, plain_value, program_name};

/// Is `exe argv` one of the checks above?
pub fn is_extended_check(exe: &str, argv: &[String]) -> bool {
    let name = program_name(exe);
    let lower: Vec<String> = argv.iter().map(|a| a.to_ascii_lowercase()).collect();
    match name.as_str() {
        "go" => go_check(argv, &lower),
        "dotnet" => dotnet_check(argv, &lower),
        "npm" | "pnpm" | "yarn" | "bun" => node_check(&name, &lower),
        "tsc" => tsc_check(argv, &lower),
        "ruff" => ruff_check(argv, &lower),
        _ => false,
    }
}

/// A path of the folder: relative, no `..`, no drive, no `~`, no option.
fn local_path(p: &str) -> bool {
    !p.is_empty() && !p.starts_with(['-', '@', '~']) && !looks_absolute(p) && !has_parent_segment(p) && !p.contains(':') && !p.contains(['|', '&', ';', '<', '>', '`', '$', '%', '"', '\'', '*', '?'])
}

/// A Go package pattern: `./...`, `./pkg/...`, `./cmd/tool`, `.`.
fn go_package(p: &str) -> bool {
    (p == "." || p.starts_with("./")) && !has_parent_segment(p) && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '_' | '-'))
}

/// The value of `go test -run`: a regular expression of test names, no spaces and nothing that is a shell's.
fn go_run_value(v: &str) -> bool {
    !v.is_empty() && !v.starts_with('-') && v.len() <= 120 && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '^' | '$' | '|' | '(' | ')' | '-'))
}

fn go_check(argv: &[String], lower: &[String]) -> bool {
    let Some(sub) = lower.first().map(String::as_str) else { return false };
    if !matches!(sub, "test" | "vet" | "build") {
        return false;
    }
    const ANY: &[&str] = &["-v", "-race", "-x"];
    const TEST_ONLY: &[&str] = &["-short", "-failfast", "-json", "-count=1", "-vet=off"];
    let mut i = 1;
    while i < argv.len() {
        let a = argv[i].as_str();
        let al = lower[i].as_str();
        if ANY.contains(&al) || (sub == "test" && TEST_ONLY.contains(&al)) {
            i += 1;
        } else if let Some(n) = al.strip_prefix("-count=") {
            if !(sub == "test" && !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())) {
                return false;
            }
            i += 1;
        } else if sub == "test" && (al == "-run" || al == "-timeout") {
            let Some(v) = argv.get(i + 1) else { return false };
            let ok = if al == "-run" { go_run_value(v) } else { v.len() <= 12 && !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || matches!(c, 'm' | 's' | 'h' | '.')) };
            if !ok {
                return false;
            }
            i += 2;
        } else if go_package(a) {
            i += 1;
        } else {
            return false;
        }
    }
    true
}

fn dotnet_check(argv: &[String], lower: &[String]) -> bool {
    let Some(sub) = lower.first().map(String::as_str) else { return false };
    if !matches!(sub, "test" | "build") {
        return false;
    }
    const FLAGS: &[&str] = &["--no-restore", "--no-build", "--nologo"];
    let mut i = 1;
    let mut project_given = false;
    while i < argv.len() {
        let a = argv[i].as_str();
        let al = lower[i].as_str();
        if FLAGS.contains(&al) && !(sub == "build" && al == "--no-build") {
            i += 1;
        } else if matches!(al, "-v" | "--verbosity") {
            let ok = argv.get(i + 1).is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "q" | "quiet" | "m" | "minimal" | "n" | "normal" | "d" | "detailed"));
            if !ok {
                return false;
            }
            i += 2;
        } else if matches!(al, "-c" | "--configuration") {
            let ok = argv.get(i + 1).is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "debug" | "release"));
            if !ok {
                return false;
            }
            i += 2;
        } else if sub == "test" && al == "--filter" {
            let ok = argv.get(i + 1).is_some_and(|v| !v.is_empty() && v.len() <= 120 && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '~' | '=' | '!' | '(' | ')' | '&' | '|')) && !v.starts_with('-'));
            if !ok {
                return false;
            }
            i += 2;
        } else if !project_given && local_path(a) && [".sln", ".slnx", ".csproj", ".fsproj"].iter().any(|e| al.ends_with(e)) {
            project_given = true;
            i += 1;
        } else {
            return false;
        }
    }
    true
}

/// The scripts of a package that are a project's checks, by name.
const CHECK_SCRIPTS: &[&str] = &["test", "lint", "typecheck", "type-check", "check-types", "tsc", "build"];

fn node_check(manager: &str, lower: &[String]) -> bool {
    let rest_is_quiet = |from: usize| lower[from..].iter().all(|a| a == "--silent" || a == "-s");
    match lower.first().map(String::as_str) {
        Some("test" | "t") if manager != "bun" => rest_is_quiet(1),
        Some("run" | "run-script") => lower.get(1).is_some_and(|s| CHECK_SCRIPTS.contains(&s.as_str())) && rest_is_quiet(2),
        _ => false,
    }
}

fn tsc_check(argv: &[String], lower: &[String]) -> bool {
    let mut no_emit = false;
    let mut i = 0;
    while i < argv.len() {
        match lower[i].as_str() {
            "--noemit" => {
                no_emit = true;
                i += 1;
            }
            "--pretty" => {
                if !argv.get(i + 1).is_some_and(|v| matches!(v.as_str(), "true" | "false")) {
                    return false;
                }
                i += 2;
            }
            "-p" | "--project" => {
                let ok = argv.get(i + 1).is_some_and(|v| local_path(v) && v.to_ascii_lowercase().ends_with(".json"));
                if !ok {
                    return false;
                }
                i += 2;
            }
            _ => return false,
        }
    }
    no_emit
}

fn ruff_check(argv: &[String], lower: &[String]) -> bool {
    if lower.first().map(String::as_str) != Some("check") {
        return false;
    }
    let mut i = 1;
    while i < argv.len() {
        match lower[i].as_str() {
            "--no-cache" | "--quiet" | "-q" => i += 1,
            "--output-format" => {
                if !argv.get(i + 1).is_some_and(|v| matches!(v.as_str(), "concise" | "full" | "json" | "grouped" | "github" | "text")) {
                    return false;
                }
                i += 2;
            }
            _ => {
                if !local_path(&argv[i]) || !plain_value_or_path(&argv[i]) {
                    return false;
                }
                i += 1;
            }
        }
    }
    true
}

/// A path to lint: the folder itself or a path of it (it may hold `/`, which a plain value may not).
fn plain_value_or_path(p: &str) -> bool {
    p == "." || p.split(['/', '\\']).all(|seg| seg == "." || plain_value(seg))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yes(exe: &str, args: &[&str]) -> bool {
        is_extended_check(exe, &args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn the_checks_of_go_dotnet_and_the_node_managers_are_listed_and_nothing_wider() {
        for (exe, args) in [
            ("go", &["test", "./..."][..]),
            ("go", &["test", "-run", "TestParse", "-count=1", "./pkg/..."]),
            ("go", &["vet", "./..."]),
            ("go", &["build", "./..."]),
            ("dotnet", &["test"]),
            ("dotnet", &["test", "--no-restore", "--filter", "FullyQualifiedName~Parse", "-c", "Release"]),
            ("dotnet", &["build", "App.sln"]),
            ("pnpm", &["test"]),
            ("yarn", &["test", "--silent"]),
            ("bun", &["run", "test"]),
            ("npm", &["run", "lint"]),
            ("npm", &["run", "typecheck"]),
            ("pnpm.cmd", &["run", "build"]),
            ("tsc", &["--noEmit"]),
            ("tsc", &["--noEmit", "-p", "tsconfig.app.json", "--pretty", "false"]),
            ("ruff", &["check"]),
            ("ruff", &["check", "--output-format", "concise", "src"]),
        ] {
            assert!(yes(exe, args), "{exe} {args:?} is a project check");
        }
        for (exe, args) in [
            // A flag that names a program or a file to write is no plain flag.
            ("go", &["build", "-o", "out.exe", "./..."][..]),
            ("go", &["test", "-exec", "calc.exe", "./..."]),
            ("go", &["test", "-coverprofile=c.out", "./..."]),
            ("go", &["test", "-run", "-bad", "./..."]),
            ("go", &["run", "main.go"]),
            ("go", &["test", "../elsewhere/..."]),
            ("go", &["install", "./..."]),
            ("dotnet", &["run"]),
            ("dotnet", &["test", "--logger", "trx;LogFileName=x"]),
            ("dotnet", &["build", "..\\other.csproj"]),
            ("dotnet", &["publish"]),
            ("dotnet", &["build", "--no-build"]),
            // Scripts that are not checks, a package manager's installs, and options that can name a shell.
            ("npm", &["run", "start"]),
            ("npm", &["run", "build", "--script-shell=calc.exe"]),
            ("npm", &["install"]),
            ("pnpm", &["dlx", "x"]),
            ("yarn", &[]),
            ("bun", &["test"]),
            ("tsc", &[]),
            ("tsc", &["--noEmit", "--outDir", "out"]),
            ("tsc", &["--noEmit", "-p", "..\\x\\tsconfig.json"]),
            ("tsc", &["--build"]),
            ("ruff", &["format"]),
            ("ruff", &["check", "--fix"]),
            ("ruff", &["check", "--config", "x.toml"]),
            ("ruff", &["check", "C:\\Windows"]),
            ("ruff", &["check", "--output-format", "weird"]),
            ("make", &["test"]),
        ] {
            assert!(!yes(exe, args), "{exe} {args:?} is not a project check");
        }
    }
}
