//! The words of a project check that one answer of "approve for this session" covers (card OA4-05, task TM-16 of the OpenCode audit).
//!
//! Today the answer covers the exact call: `cargo test -p core` and `cargo test -p other` are two questions. With `check_grants` on (`term.json`, or `AQUA_TERM_CHECK_GRANTS=on`) the answer
//! covers the program and its subcommand in the same folder: `cargo test`, whatever package or test name follows. Only a command that the terminal rules already call a project check can
//! have one (`rules::is_project_check`, or the wider list when that is on); an interpreter that runs anything else never does, and a run that has read text from outside does not use grants at all
//! (the host's own rule). The folder is part of the scope: the same check in another project asks again.

use super::rules::program_name;

/// How many words after the program name make the scope of a project check: `cargo test` is one, `npm run lint` is two, `python -m pytest` is two. `None`: no scope for this program.
fn arity(program: &str, first_arg: &str) -> Option<usize> {
    match program {
        "cargo" | "go" | "dotnet" | "ruff" => Some(1),
        "npm" | "pnpm" | "yarn" | "bun" => Some(if matches!(first_arg, "run" | "run-script") { 2 } else { 1 }),
        "python" | "python3" | "py" => Some(2),
        n if n.starts_with("python") => Some(2),
        "tsc" => Some(0),
        _ => None,
    }
}

/// The scope of a project check: its program, its subcommand words and the folder, in lower case. `None` for a program that has none.
pub fn scope_of(exe: &str, argv: &[String], cwd: &str) -> Option<String> {
    let program = program_name(exe);
    let first = argv.first().map(|a| a.to_ascii_lowercase()).unwrap_or_default();
    let n = arity(&program, &first)?;
    let words: Vec<String> = argv.iter().take(n).map(|a| a.to_ascii_lowercase()).collect();
    if words.len() < n {
        return None;
    }
    Some(format!("{program} {}@{}", words.join(" "), cwd.replace('\\', "/").to_ascii_lowercase().trim_end_matches('/')))
}

/// What a project check is for, by the words of the command line (card OA10-05, WF-14): `tests`, `build`, `lint` or `typecheck`. It is read from the program and its subcommand only, and
/// only means something for a command that is a project check already (`rules::is_project_check`, or the wider list when that is on): the terminal tools decide that first. `None`: no kind
/// (a program or a script that is not one of the known ones).
pub fn check_kind(exe: &str, argv: &[String]) -> Option<&'static str> {
    let program = program_name(exe);
    let lower: Vec<String> = argv.iter().map(|a| a.to_ascii_lowercase()).collect();
    let word = |i: usize| lower.get(i).map(String::as_str).unwrap_or("");
    match program.as_str() {
        "cargo" => match word(0) {
            "test" => Some("tests"),
            "build" => Some("build"),
            "clippy" => Some("lint"),
            "check" => Some("typecheck"),
            _ => None,
        },
        "go" => match word(0) {
            "test" => Some("tests"),
            "build" => Some("build"),
            "vet" => Some("lint"),
            _ => None,
        },
        "dotnet" => match word(0) {
            "test" => Some("tests"),
            "build" => Some("build"),
            _ => None,
        },
        "npm" | "pnpm" | "yarn" | "bun" => match word(0) {
            "test" | "t" => Some("tests"),
            "run" | "run-script" => match word(1) {
                "test" => Some("tests"),
                "lint" => Some("lint"),
                "typecheck" | "type-check" | "check-types" | "tsc" => Some("typecheck"),
                "build" => Some("build"),
                _ => None,
            },
            _ => None,
        },
        "tsc" => Some("typecheck"),
        "ruff" => (word(0) == "check").then_some("lint"),
        p if p == "py" || p.starts_with("python") => (word(0) == "-m" && matches!(word(1), "pytest" | "unittest")).then_some("tests"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_scope_is_the_program_the_subcommand_and_the_folder_and_not_what_follows() {
        let one = scope_of("C:/Users/x/.cargo/bin/cargo.exe", &args(&["test", "-p", "core"]), r"C:\Work\App");
        let two = scope_of("cargo", &args(&["test", "--offline", "other"]), "c:/work/app/");
        assert_eq!(one.as_deref(), Some("cargo test@c:/work/app"));
        assert_eq!(one, two, "another package or test name is the same check");
        assert_ne!(scope_of("cargo", &args(&["check"]), "c:/work/app"), one, "another subcommand is another question");
        assert_ne!(scope_of("cargo", &args(&["test"]), "c:/work/other"), one, "another folder is another question");
        assert_eq!(scope_of("npm", &args(&["run", "lint"]), "c:/w").as_deref(), Some("npm run lint@c:/w"));
        assert_eq!(scope_of("npm", &args(&["test"]), "c:/w").as_deref(), Some("npm test@c:/w"));
        assert_eq!(scope_of("python", &args(&["-m", "pytest", "-q"]), "c:/w").as_deref(), Some("python -m pytest@c:/w"));
        assert_eq!(scope_of("tsc", &args(&["--noEmit"]), "c:/w").as_deref(), Some("tsc @c:/w"));
        assert_eq!(scope_of("git", &args(&["status"]), "c:/w"), None);
        assert_eq!(scope_of("cargo", &[], "c:/w"), None, "no subcommand, no scope");
    }

    #[test]
    fn a_check_is_of_a_kind_by_its_program_and_subcommand_and_every_command_the_rules_know_has_one() {
        for (exe, a, kind) in [
            ("cargo", vec!["test", "-p", "core"], "tests"),
            ("cargo", vec!["build", "--release"], "build"),
            ("cargo", vec!["clippy", "--", "-D", "warnings"], "lint"),
            ("cargo", vec!["check"], "typecheck"),
            ("npm", vec!["test"], "tests"),
            ("pnpm", vec!["run", "lint"], "lint"),
            ("yarn", vec!["run", "typecheck"], "typecheck"),
            ("npm", vec!["run", "build"], "build"),
            ("python", vec!["-m", "pytest", "-q"], "tests"),
            ("py", vec!["-m", "unittest"], "tests"),
            ("go", vec!["vet", "./..."], "lint"),
            ("go", vec!["test", "./..."], "tests"),
            ("dotnet", vec!["build"], "build"),
            ("tsc", vec!["--noEmit"], "typecheck"),
            ("ruff", vec!["check", "."], "lint"),
        ] {
            assert_eq!(check_kind(exe, &args(&a)), Some(kind), "{exe} {a:?}");
        }
        for (exe, a) in [("git", vec!["status"]), ("cargo", vec!["install", "x"]), ("cargo", vec!["publish"]), ("npm", vec!["install"]), ("npm", vec!["run", "deploy"]), ("python", vec!["script.py"]), ("python", vec!["-m", "pip"]), ("ruff", vec!["format"]), ("cargo", vec![])] {
            assert_eq!(check_kind(exe, &args(&a)), None, "{exe} {a:?}");
        }
        // Every command line that is a project's own check has a kind: no check the terminal counts is left without one.
        for (exe, a) in [("cargo", vec!["test"]), ("cargo", vec!["check", "--offline"]), ("npm", vec!["test"]), ("python", vec!["-m", "pytest"])] {
            assert!(super::super::rules::is_project_check(exe, &args(&a)) && check_kind(exe, &args(&a)).is_some(), "{exe} {a:?}");
        }
    }
}
