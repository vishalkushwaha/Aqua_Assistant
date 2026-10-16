//! The commands of `aqua-cli` that read what is on this computer and need no running core: `config`, `env`, `stats`, `support-bundle`, `door-report` and `slots` (cards OA10-05, OA8-05 and
//! OA9-06; tasks UX-17, UX-18 and SE-16 of the OpenCode audit).
//!
//! ```text
//! aqua-cli config list | validate | get <file> [key] | set <file> <key> <value>      aqua-cli env [--set]
//! aqua-cli stats [--days N] [--json]                                                  aqua-cli support-bundle [--out DIR]
//! aqua-cli door-report [--file PATH] [--min-runs N] [--max-looser N]                 aqua-cli slots list | forget
//! ```
//!
//! * `config` and `env` are in [`config`]: the configuration files and the `AQUA_*` variables, discoverable from a terminal. `set` changes two settings that make nothing looser and
//!   nothing else, for a person at a terminal.
//! * `stats` is in [`stats`]: counts and times of the runs of the last days from the run database, opened for reading only.
//! * `support-bundle` is in [`bundle`]: a folder of six files made from an allow-list, with no token, no database and no word of what a run said, printed and never uploaded.
//! * `door-report` is in [`door`]: what the one-door comparison has found so far (`data/door_shadow.json`), and whether there is enough of it to switch the door on. It reads one file and
//!   writes only to standard output.
//! * `slots` is in [`slots`]: see (`list`) and delete (`forget`) the saved model slots of `data/cache/kv`, the files `AQUA_LLAMA_SLOT_SAVE=on` writes, which are as private as the conversation.
//!   It touches only that folder and only files of the saver's names, never a link, and reads no content.
//!
//! `aqua-cli logs` (card OA3-04) is `aqua_core::logging`; the bundle uses it for the tail of the log.
//!
//! The module takes what the machine gives it (the data folder, the log file, what `doctor` says, the home folder) from the program, so that its tests touch nothing of the person's.

pub mod bundle;
pub mod config;
pub mod door;
pub mod slots;
pub mod stats;

use crate::headless::render::plain;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The words of the commands this module serves.
pub const COMMANDS: [&str; 6] = ["config", "env", "stats", "support-bundle", "door-report", "slots"];

/// The lines the help of `aqua-cli` shows for these commands.
pub const HELP: [&str; 6] = [
    "  aqua-cli config list | validate | get <file> [key] | set <file> <key> <value>   the configuration files of data/tools: where they are and whether they are valid (the doctor agrees), one file or key, and the two settings that make nothing looser",
    "  aqua-cli env [--set]   the AQUA_* variables Aqua reads, and which are set here (a value is shown unless its name says it is a secret)",
    "  aqua-cli stats [--days N] [--json]   runs by how they ended, the model's time to the first token and speed, the tool error rate, the questions asked by day",
    "  aqua-cli support-bundle [--out DIR]   a folder to send with a bug report: versions, the doctor, which config files are valid, the redacted tail of the log, the last 20 runs without content; never a token, a database or a word you said",
    "  aqua-cli door-report [--file PATH] [--min-runs N] [--max-looser N]   what the one-door comparison found (data/door_shadow.json): calls and runs, per tool and caller how often the door would decide the same, stricter or looser than the old routes, every looser decision, and whether there is enough evidence to switch AQUA_DOOR on (default: 50 runs, no looser decision); it only reads",
    "  aqua-cli slots list | forget   the saved model slots (AQUA_LLAMA_SLOT_SAVE=on writes them to data/cache/kv, and each is as private as the conversation): list shows how many there are, their size in all and each one's name, size and age, never a content; forget deletes them all and says how many files and bytes it deleted; only plain files of that name form in that folder are touched, a file that cannot be deleted is named and the exit code is 1; no running core and no switch needed",
];

/// What the program gives these commands.
pub struct Env<'a> {
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    /// The data folder, unless `--data DIR` says another.
    pub data_dir: PathBuf,
    /// Where the core's log is (the shell's `logs/core.log`).
    pub log_file: PathBuf,
    /// The home folder of the person, to be left out of what a bundle writes.
    pub home: Option<String>,
    /// What `aqua-cli doctor` says now (for the bundle).
    pub doctor: &'a dyn Fn(&Path) -> String,
    /// Standard input is a terminal (a person is typing).
    pub stdin_is_terminal: bool,
    /// Shows a text and says whether the person typed `yes`.
    pub confirm: &'a dyn Fn(&str) -> bool,
    /// The environment, by name (a test gives its own).
    pub var: &'a dyn Fn(&str) -> Option<String>,
    /// Seconds since 1970 now.
    pub now_secs: u64,
}

fn say(w: &mut dyn Write, text: &str) {
    let _ = write!(w, "{text}");
    let _ = w.flush();
}

fn fail(env: &mut Env, message: &str) -> i32 {
    say(env.err, &format!("{}\n", plain(message)));
    1
}

/// What a command that made its own report says: its lines on standard output and on standard error, and how it ends.
fn finish(env: &mut Env, report: slots::Report) -> i32 {
    say(env.out, &report.out);
    say(env.err, &report.err);
    report.code
}

/// Runs one command of this module (`args[0]` is its word); the exit code of the process.
pub fn run_cli(args: &[String], env: &mut Env) -> i32 {
    let Some(command) = args.first().map(String::as_str).filter(|c| COMMANDS.contains(c)) else { return 1 };
    // `--data DIR` anywhere; the rest are the words of the command.
    let mut rest: Vec<String> = Vec::new();
    let mut words = args[1..].iter();
    while let Some(w) = words.next() {
        if w == "--data" {
            match words.next() {
                Some(d) => env.data_dir = PathBuf::from(d),
                None => return fail(env, &format!("{command}: --data needs a folder")),
            }
        } else {
            rest.push(w.clone());
        }
    }
    match command {
        "config" => match rest.first().map(String::as_str) {
            Some("list") | None => {
                let text = config::list_text(&env.data_dir);
                say(env.out, &text);
                0
            }
            Some("validate") => {
                let (text, broken) = config::validate_text(&env.data_dir);
                say(env.out, &text);
                i32::from(broken)
            }
            Some("get") => match (rest.get(1), rest.get(2).map(String::as_str)) {
                (Some(file), key) => match config::get_text(&env.data_dir, file, key) {
                    Ok(text) => {
                        say(env.out, &text);
                        0
                    }
                    Err(e) => fail(env, &format!("config get: {e}")),
                },
                _ => fail(env, "usage: aqua-cli config get <file> [key]"),
            },
            Some("set") => {
                let (Some(file), Some(key), Some(value)) = (rest.get(1), rest.get(2), rest.get(3)) else { return fail(env, "usage: aqua-cli config set <file> <key> <value>") };
                if !env.stdin_is_terminal {
                    return fail(env, "config set: it needs a person at a terminal to say yes; nothing was changed");
                }
                let confirm = env.confirm;
                match config::set_text(&env.data_dir, file, key, value, confirm) {
                    Ok(text) => {
                        say(env.out, &text);
                        0
                    }
                    Err(e) => fail(env, &format!("config set: {e}")),
                }
            }
            Some(other) => fail(env, &format!("config: {} is not list, validate, get or set", plain(other))),
        },
        "env" => {
            let only_set = rest.iter().any(|a| a == "--set");
            if let Some(bad) = rest.iter().find(|a| *a != "--set") {
                return fail(env, &format!("env: {} is not an option (--set)", plain(bad)));
            }
            let text = config::env_text(only_set, env.var);
            say(env.out, &text);
            0
        }
        "stats" => {
            let mut days = 7u32;
            let mut json = false;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--json" => json = true,
                    "--days" => match it.next().and_then(|d| d.trim().parse::<u32>().ok()).filter(|d| (1..=3650).contains(d)) {
                        Some(d) => days = d,
                        None => return fail(env, "stats: --days is a number of days from 1 to 3650"),
                    },
                    other => return fail(env, &format!("stats: {} is not an option (--days N, --json)", plain(other))),
                }
            }
            let now_ms = (env.now_secs as i64) * 1000;
            match stats::collect(&env.data_dir.join("agent_runs.db"), days, now_ms) {
                Ok(s) => {
                    let text = if json { format!("{}\n", stats::to_json(&s)) } else { stats::render(&s) };
                    say(env.out, &text);
                    0
                }
                Err(e) => fail(env, &format!("stats: {e}")),
            }
        }
        "support-bundle" => {
            let mut out: Option<PathBuf> = None;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--out" => match it.next() {
                        Some(d) => out = Some(PathBuf::from(d)),
                        None => return fail(env, "support-bundle: --out needs a folder"),
                    },
                    other => return fail(env, &format!("support-bundle: {} is not an option (--out DIR)", plain(other))),
                }
            }
            let folder = out.unwrap_or_else(|| bundle::default_folder(&env.data_dir, env.now_secs));
            let doctor = (env.doctor)(&env.data_dir);
            match bundle::make(&env.data_dir, &env.log_file, &folder, env.home.clone(), &doctor) {
                Ok(written) => {
                    say(env.out, &format!("{}\n", bundle::summary(&folder, &written)));
                    say(env.err, &format!("The bundle is in {} ({} files). Look through it before you send it; nothing was uploaded.\n", plain(&folder.display().to_string()), written.len()));
                    0
                }
                Err(e) => fail(env, &format!("support-bundle: {e}")),
            }
        }
        "door-report" => {
            let mut params = door::Params::default();
            let mut file: Option<PathBuf> = None;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--file" => match it.next() {
                        Some(p) => file = Some(PathBuf::from(p)),
                        None => return fail(env, "door-report: --file needs a path"),
                    },
                    "--min-runs" => match it.next().and_then(|n| n.trim().parse::<u64>().ok()) {
                        Some(n) => params.min_runs = n,
                        None => return fail(env, "door-report: --min-runs is a number of runs"),
                    },
                    "--max-looser" => match it.next().and_then(|n| n.trim().parse::<u64>().ok()) {
                        Some(n) => params.max_looser = n,
                        None => return fail(env, "door-report: --max-looser is a number of decisions"),
                    },
                    other => return fail(env, &format!("door-report: {} is not an option (--file PATH, --min-runs N, --max-looser N)", plain(other))),
                }
            }
            let path = file.unwrap_or_else(|| env.data_dir.join(door::FILE_NAME));
            let source = door::read_file(&path);
            // How long ago the file was written, from the file's own time; the clock is the program's.
            let age = std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|t| env.now_secs.saturating_sub(t.as_secs()));
            say(env.out, &door::render(&source, &params, &path.display().to_string(), age));
            // A file that is there and cannot be used is a failure of the command; a file that is not there yet is a report that says so.
            i32::from(matches!(source, door::Source::Unreadable(_)))
        }
        "slots" => match rest.as_slice() {
            [w] if w == "list" => {
                let report = slots::list(&env.data_dir, env.now_secs);
                finish(env, report)
            }
            [w] if w == "forget" => {
                let report = slots::forget(&env.data_dir);
                finish(env, report)
            }
            [w, extra, ..] if w == "list" || w == "forget" => fail(env, &format!("slots {w}: {} is not an option (the folder is named with --data DIR)", plain(extra))),
            [other, ..] => fail(env, &format!("slots: {} is not list or forget", plain(other))),
            [] => fail(env, "slots: say list or forget (usage: aqua-cli slots list | forget)"),
        },
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("target").join("scratch_cli_tools").join(format!("run_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        dir
    }

    fn go(args: &[&str], data: &Path, terminal: bool, yes: bool) -> (i32, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let doctor = |_: &Path| "doctor: fine\n".to_string();
        let confirm = move |_: &str| yes;
        let var = |name: &str| (name == "AQUA_API_PORT").then(|| "8790".to_string());
        let code = {
            let mut env = Env { out: &mut out, err: &mut err, data_dir: data.to_path_buf(), log_file: data.join("core.log"), home: None, doctor: &doctor, stdin_is_terminal: terminal, confirm: &confirm, var: &var, now_secs: 1_791_000_000 };
            run_cli(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>(), &mut env)
        };
        (code, String::from_utf8_lossy(&out).to_string(), String::from_utf8_lossy(&err).to_string())
    }

    #[test]
    fn the_help_lists_every_command() {
        let help = HELP.join("\n");
        for word in COMMANDS {
            assert!(help.contains(&format!("aqua-cli {word} ")), "{word}");
        }
        assert_eq!(HELP.len(), COMMANDS.len());
    }

    #[test]
    fn config_validate_and_the_doctor_agree_and_a_broken_file_is_exit_one() {
        let d = scratch("validate");
        std::fs::write(d.join("tools/term.json"), "{ \"max_sessions\": }").unwrap();
        let (code, out, _) = go(&["config", "validate"], &d, false, false);
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("[ FAIL ] term.json"), "{out}");
        // The report the doctor and `GET /api/v1/config/status` are made from is the one these lines are made from: the files it calls broken are the files this names. (The doctor itself is
        // not run here: it looks at the credential store and at the port of the person's own core.)
        let broken: Vec<&str> = crate::config_files::status(&d).iter().filter(|r| r.state == crate::config_files::ConfigState::Broken).map(|r| r.name).collect();
        assert_eq!(broken, vec!["term.json"]);
        assert_eq!(out.matches("[ FAIL ]").count(), broken.len(), "{out}");
        std::fs::write(d.join("tools/term.json"), "{ \"max_sessions\": 4 }").unwrap();
        assert_eq!(go(&["config", "validate"], &d, false, false).0, 0);
        assert_eq!(go(&["config", "list", "--data", d.to_str().unwrap()], Path::new("nowhere"), false, false).0, 0, "--data names the folder");
    }

    #[test]
    fn config_set_needs_a_person_at_a_terminal_who_says_yes() {
        let d = scratch("set");
        std::fs::write(d.join("tools/term.json"), "{ \"live_output\": false }\n").unwrap();
        let (code, _, err) = go(&["config", "set", "term", "live_output", "true"], &d, false, true);
        assert_eq!(code, 1);
        assert!(err.contains("person at a terminal"), "{err}");
        assert_eq!(go(&["config", "set", "term", "live_output", "true"], &d, true, false).0, 1, "no yes, no change");
        assert_eq!(std::fs::read_to_string(d.join("tools/term.json")).unwrap(), "{ \"live_output\": false }\n");
        assert_eq!(go(&["config", "set", "term", "live_output", "true"], &d, true, true).0, 0);
        assert_eq!(std::fs::read_to_string(d.join("tools/term.json")).unwrap(), "{ \"live_output\": true }\n");
    }

    /// What is under `dir`: every file with its size, so that a test can say that a command wrote nothing.
    fn listing(dir: &Path) -> Vec<(String, u64)> {
        fn walk(dir: &Path, out: &mut Vec<(String, u64)>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push((path.display().to_string(), entry.metadata().map(|m| m.len()).unwrap_or(0)));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, &mut out);
        out.sort();
        out
    }

    /// Card OA8-05: `aqua-cli door-report` reads the one file and prints; the numbers of the verdict are the person's (`--min-runs`, `--max-looser`); a file that is there and cannot be used is
    /// a failed command; a file that is not there yet is a report that says so; and it writes nothing, neither in the data folder nor to the file it read.
    #[test]
    fn door_report_prints_the_verdict_against_the_persons_numbers_fails_on_a_file_it_cannot_use_and_writes_nothing() {
        let d = scratch("door_report");
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("door_report_fixtures");
        let fixture = |name: &str| fixtures.join(name).display().to_string();
        let before = (listing(&d), listing(&fixtures));
        // No file in the data folder yet: a report, not a failure.
        let (code, out, _) = go(&["door-report"], &d, false, false);
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("There is no file at that path") && out.contains("enough evidence to flip: no"), "{out}");
        // A file given by path, with the person's numbers.
        let (code, out, _) = go(&["door-report", "--file", &fixture("v2_enough.json"), "--min-runs", "64"], &d, false, false);
        assert_eq!(code, 0);
        assert!(out.contains("enough evidence to flip: yes") && out.contains("at least 64 runs and at most 0 looser"), "{out}");
        let (_, out, _) = go(&["door-report", "--file", &fixture("v2_enough.json"), "--min-runs", "65"], &d, false, false);
        assert!(out.contains("enough evidence to flip: no") && out.contains("64 runs seen, 65 wanted"), "{out}");
        let (_, out, _) = go(&["door-report", "--file", &fixture("v2_with_looser.json"), "--max-looser", "3"], &d, false, false);
        assert!(out.contains("enough evidence to flip: yes"), "{out}");
        // The default file is the data folder's own, and `--data` names the folder.
        std::fs::copy(fixtures.join("v2_few_runs.json"), d.join(door::FILE_NAME)).unwrap();
        let (code, out, _) = go(&["door-report"], &d, false, false);
        assert_eq!(code, 0);
        assert!(out.contains("3 runs seen, 50 wanted"), "{out}");
        let (_, elsewhere, _) = go(&["door-report", "--data", d.to_str().unwrap()], Path::new("nowhere"), false, false);
        assert!(elsewhere.contains("3 runs seen, 50 wanted"), "{elsewhere}");
        // A file that is there and cannot be used is a failure, with the verdict still said.
        for name in ["empty.json", "corrupt.json", "wrong_shape.json"] {
            let (code, out, _) = go(&["door-report", "--file", &fixture(name)], &d, false, false);
            assert_eq!(code, 1, "{name}: {out}");
            assert!(out.contains("The file cannot be used") && out.contains("enough evidence to flip: no"), "{name}: {out}");
        }
        // Bad words are refused with an exit code of 1 and say nothing of the file.
        for bad in [vec!["door-report", "--wat"], vec!["door-report", "--min-runs", "many"], vec!["door-report", "--min-runs"], vec!["door-report", "--max-looser", "-1"], vec!["door-report", "--file"]] {
            let (code, out, err) = go(&bad, &d, false, false);
            assert_eq!((code, out.as_str()), (1, ""), "{bad:?}");
            assert!(err.starts_with("door-report:"), "{err}");
        }
        // It wrote nothing: the data folder holds what the test put there (the copied file) and the fixtures are as they were.
        let after = (listing(&d), listing(&fixtures));
        assert_eq!(after.1, before.1, "the fixtures");
        assert_eq!(after.0.len(), before.0.len() + 1, "{:?}", after.0);
        assert_eq!(std::fs::read(d.join(door::FILE_NAME)).unwrap(), std::fs::read(fixtures.join("v2_few_runs.json")).unwrap(), "the file it read");
    }

    #[test]
    fn env_stats_and_the_bundle_run_through_the_one_door_and_bad_words_are_refused() {
        let d = scratch("door");
        let (code, out, _) = go(&["env", "--set"], &d, false, false);
        assert_eq!((code, out.lines().count()), (0, 1), "{out}");
        assert!(out.contains("AQUA_API_PORT") && out.contains("8790"));
        let (code, _, err) = go(&["stats"], &d, false, false);
        assert_eq!(code, 1);
        assert!(err.contains("no run database"), "{err}");
        for bad in [vec!["stats", "--days", "0"], vec!["stats", "--wat"], vec!["env", "--wat"], vec!["support-bundle", "--wat"], vec!["config", "frobnicate"], vec!["config", "get"]] {
            assert_eq!(go(&bad, &d, false, false).0, 1, "{bad:?}");
        }
        let target = d.join("bundle");
        let (code, out, err) = go(&["support-bundle", "--out", target.to_str().unwrap()], &d, false, false);
        assert_eq!(code, 0, "{err}");
        assert!(out.contains("\"uploaded\":false") && target.join("doctor.txt").is_file(), "{out}");
        assert!(std::fs::read_to_string(target.join("doctor.txt")).unwrap().contains("doctor: fine"));
    }

    /// Card OA9-06: `aqua-cli slots list` and `slots forget` through the one door of `run_cli`: they need no person at a terminal, no switch and no core; the data folder is the program's or
    /// `--data`'s; a missing folder is "no saved slots" and exit 0; bad words are refused with exit 1 and delete nothing; the output never holds what a file holds; a second `forget` finds
    /// nothing; and only the files of the saver's names go.
    #[test]
    fn slots_list_and_forget_run_through_the_one_door_with_exit_codes_the_data_option_and_no_content() {
        let d = scratch("slots");
        let kv = d.join("cache").join("kv");
        let words = "WORDS-OF-THE-CONVERSATION-IN-ANOTHER-FORM";
        // No folder at all, whatever the data folder is: a sentence and exit 0, and nothing is made.
        for args in [vec!["slots", "list"], vec!["slots", "forget"]] {
            assert_eq!(go(&args, &d, false, false), (0, "no saved slots\n".to_string(), String::new()), "{args:?}");
            assert_eq!(go(&args, Path::new("nowhere"), false, false).0, 0, "{args:?}");
        }
        assert!(!d.join("cache").exists(), "nothing was made");
        std::fs::create_dir_all(&kv).unwrap();
        let (a, b) = ("kv_0123456789abcdef_fedcba9876543210.bin", "kv_00000000000000aa_00000000000000bb.bin");
        std::fs::write(kv.join(a), words).unwrap();
        std::fs::write(kv.join(b), words.repeat(3)).unwrap();
        std::fs::write(kv.join("notes.txt"), "keep me").unwrap();
        let before = listing(&d);

        // list: the names, the sizes, the one entry that was left alone, and no word of a file.
        let (code, out, err) = go(&["slots", "list"], &d, false, false);
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert!(out.starts_with(&format!("2 saved slot files in cache/kv: {} bytes", words.len() * 4)), "{out}");
        assert!(out.contains(a) && out.contains(b) && out.contains("left alone: 1 entry"), "{out}");
        for forbidden in [words, "notes.txt", "keep me", "scratch"] {
            assert!(!out.contains(forbidden), "{forbidden}: {out}");
        }
        // `--data` names the folder, as for the other commands.
        let (code, elsewhere, _) = go(&["slots", "list", "--data", d.to_str().unwrap()], Path::new("nowhere"), false, false);
        assert_eq!((code, elsewhere.as_str()), (0, out.as_str()));
        // Bad words are refused with exit 1, say so on standard error only, and delete nothing.
        for bad in [vec!["slots"], vec!["slots", "wat"], vec!["slots", "list", "--yes"], vec!["slots", "forget", "now"], vec!["slots", "forget", "--data"], vec!["slots", "--all"]] {
            let (code, out, err) = go(&bad, &d, false, false);
            assert_eq!((code, out.as_str()), (1, ""), "{bad:?}");
            assert!(err.starts_with("slots"), "{bad:?}: {err}");
        }
        assert_eq!(listing(&d), before, "list and the refused words changed nothing");

        // forget: the two files go, the other stays, the folder stays, and the output holds no word of a file.
        let (code, out, err) = go(&["slots", "forget"], &d, false, false);
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert!(out.starts_with(&format!("deleted 2 saved slot files, {} bytes", words.len() * 4)) && out.contains("left alone: 1 entry"), "{out}");
        assert!(!out.contains(words) && !out.contains("notes.txt"), "{out}");
        assert!(!kv.join(a).exists() && !kv.join(b).exists() && kv.is_dir());
        assert_eq!(std::fs::read_to_string(kv.join("notes.txt")).unwrap(), "keep me");
        // A second run has nothing left to delete and is still a success.
        let (code, out, err) = go(&["slots", "forget"], &d, false, false);
        assert_eq!((code, err.as_str()), (0, ""));
        assert!(out.starts_with("no saved slots\n") && out.contains("left alone: 1 entry"), "{out}");
        assert_eq!(go(&["slots", "list"], &d, false, false).1.lines().next(), Some("no saved slots"));
    }
}
