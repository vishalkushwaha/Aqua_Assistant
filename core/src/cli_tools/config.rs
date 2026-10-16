//! `aqua-cli config list | get | set | validate` and `aqua-cli env` (card OA10-05; task UX-17 of the OpenCode audit).
//!
//! * `config list` and `config validate` read the same status as `aqua-cli doctor` and `GET /api/v1/config/status` (`config_files::status`): the file, its state and, for a broken one, the
//!   line, the column and what using it as it is would turn off. The two agree because they read one function. `validate` exits 1 when a file is broken.
//! * `config get <file> [key]` prints a file as it is read (comments and trailing commas removed), or one key of it (`a.b` for a key inside an object). Values under a key that looks like a
//!   secret are written `[hidden]`, and the rest goes through the redactor.
//! * `config set <file> <key> <value>` changes **one of two settings** that make nothing looser: `live_output` and `auto_background` of `term.json` (the lines a command prints are shown as
//!   they come, a long build is kept as a background job). Every other key, every other file, and above all `policy.json`, the rules and the settings that ask less (`check_grants`,
//!   `extended_project_checks`), are the person's to change by hand or in the app: the command says so and changes nothing. It needs a person at a terminal who types `yes`, keeps the
//!   comments and the line endings of the file (it reads and writes bytes), checks the new text with the loader's own parser before it writes it, and writes it whole or not at all.
//! * `env` lists the `AQUA_*` variables the source mentions (the table of `documentation/CONFIGURATION.md`) with whether each is set here. A value is shown only for a variable whose name
//!   does not say it holds a secret; the others say `set (hidden)`.

use crate::config_files::{status, ConfigState};
use serde_json::Value;
use std::path::Path;

/// The two keys `config set` may change: (file name as `config list` shows it, the key).
pub const SETTABLE: [(&str, &str); 2] = [("term", "live_output"), ("term", "auto_background")];

/// The table of `AQUA_*` names that the documentation carries, as (name, first file).
fn documented_variables() -> Vec<(String, String)> {
    let doc = include_str!("../../../documentation/CONFIGURATION.md");
    let table = doc.split("### Every name the source mentions").nth(1).unwrap_or("");
    table
        .lines()
        .filter_map(|l| l.strip_prefix("| `AQUA_"))
        .filter_map(|rest| {
            let (name, tail) = rest.split_once('`')?;
            let file = tail.split('`').nth(1).unwrap_or("").to_string();
            Some((format!("AQUA_{name}"), file))
        })
        .collect()
}

/// Does the name of a variable say that it holds a secret?
fn looks_secret(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    ["TOKEN", "KEY", "SECRET", "PASSWORD", "PASSPHRASE", "CREDENTIAL"].iter().any(|w| n.contains(w))
}

/// `aqua-cli env [--set]`: the variables the source mentions, and what each is here. `only_set` leaves out the ones that are not set.
pub fn env_text(only_set: bool, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut shown = 0;
    for (name, file) in documented_variables() {
        let value = lookup(&name);
        if only_set && value.is_none() {
            continue;
        }
        shown += 1;
        let what = match value {
            None => "(not set)".to_string(),
            Some(_) if looks_secret(&name) => "set (hidden)".to_string(),
            Some(v) => format!("{}", crate::headless::render::plain(&v.chars().take(120).collect::<String>())),
        };
        out.push_str(&format!("{name:<42} {what:<24} read in {file}\n"));
    }
    if shown == 0 {
        out.push_str("none of the variables Aqua reads is set in this environment\n");
    }
    out
}

/// `config list`: one line for each configuration file.
pub fn list_text(data_dir: &Path) -> String {
    let mut out = String::new();
    for r in status(data_dir) {
        out.push_str(&format!("{:<12} {:<9} {}\n", r.name, r.state.word(), r.path.display()));
    }
    out
}

/// `config validate`: what is wrong with each file that is, and whether any is. The same files and the same reading as the doctor.
pub fn validate_text(data_dir: &Path) -> (String, bool) {
    let mut out = String::new();
    let mut broken = false;
    for r in status(data_dir) {
        match r.state {
            ConfigState::Broken => {
                broken = true;
                let at = r.error.as_ref().map(|e| e.detail()).unwrap_or_default();
                out.push_str(&format!("[ FAIL ] {} ({}): {at}\n         {}\n", r.name, r.file, r.effect.clone().unwrap_or_default()));
            }
            ConfigState::Absent => out.push_str(&format!("[ info ] {} ({}): not there; the default is used\n", r.name, r.file)),
            _ => out.push_str(&format!("[ ok   ] {} ({})\n", r.name, r.file)),
        }
        for w in &r.warnings {
            out.push_str(&format!("         note: {w}\n"));
        }
    }
    (out, broken)
}

fn find_file(data_dir: &Path, name: &str) -> Result<std::path::PathBuf, String> {
    let found = status(data_dir).into_iter().find(|r| r.name == name || r.file == name || r.file.trim_start_matches("tools/") == name || r.file.trim_end_matches(".json").trim_start_matches("tools/") == name);
    found.map(|r| r.path).ok_or_else(|| format!("there is no configuration file called {name}: `aqua-cli config list` lists them"))
}

/// Reads a file as the loader would (comments and trailing commas out).
fn read_value(path: &Path) -> Result<(String, Value), String> {
    let text = std::fs::read(path).map_err(|e| format!("{} could not be read ({})", path.display(), e.kind()))?;
    let text = String::from_utf8(text).map_err(|_| format!("{} is not UTF-8 text", path.display()))?;
    let stripped = aqua_tool_host::config_text::prepare(&text).map_err(|e| e.to_string())?;
    let value = serde_json::from_str::<Value>(&stripped).map_err(|e| format!("{} is not valid: {e}", path.display()))?;
    Ok((text, value))
}

fn hide_secrets(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if looks_secret(k) || aqua_tool_host::output::is_secret_key(&k.to_ascii_lowercase()) {
                    *val = Value::String("[hidden]".into());
                } else {
                    hide_secrets(val);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(hide_secrets),
        _ => {}
    }
}

/// `config get <file> [key]`.
pub fn get_text(data_dir: &Path, file: &str, key: Option<&str>) -> Result<String, String> {
    let path = find_file(data_dir, file)?;
    if !path.is_file() {
        return Err(format!("{} is not there: the default is used (`aqua-cli doctor --fix` writes it)", path.display()));
    }
    let (_, mut value) = read_value(&path)?;
    hide_secrets(&mut value);
    let shown = match key {
        None => &value,
        Some(k) => k.split('.').try_fold(&value, |v, part| v.get(part)).ok_or_else(|| format!("{file} has no key {k}"))?,
    };
    let text = serde_json::to_string_pretty(shown).unwrap_or_default();
    Ok(format!("{}\n", aqua_tool_host::output::Redactor::new().redact(&text)))
}

/// Where the value of `key` is in `text` (a file with comments): the byte range of the value after `"key":`, found by reading the text the way the loader does, so that a key that is only
/// mentioned in a comment or a string is not taken for the key.
fn value_span(text: &str, key: &str) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let needle = format!("\"{key}\"");
    let (mut i, mut depth) = (0usize, 0i32);
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                i += 1;
            }
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
                // A key of the top-level object: at depth 1, the string `"key"`, then a colon.
                if depth == 1 && text.get(start..i) == Some(needle.as_str()) {
                    let mut j = i;
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if bytes.get(j) == Some(&b':') {
                        j += 1;
                        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        let from = j;
                        while j < bytes.len() && !matches!(bytes[j], b',' | b'}' | b'\r' | b'\n' | b'/') && !bytes[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        return Some((from, j));
                    }
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// The text of `term.json` with `key` set to `value`, keeping its comments and its line endings. `Err` is a sentence.
fn set_in_text(text: &str, key: &str, value: bool) -> Result<String, String> {
    let word = if value { "true" } else { "false" };
    if let Some((from, to)) = value_span(text, key) {
        return Ok(format!("{}{word}{}", &text[..from], &text[to..]));
    }
    // The key is not there: it is put first in the object, in the file's own line ending.
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let open = text.find('{').ok_or("the file has no object to put the key in")?;
    let rest = &text[open + 1..];
    let empty = rest.trim_start().starts_with('}');
    let indent = "  ";
    let line = format!("{eol}{indent}\"{key}\": {word}{}", if empty { "" } else { "," });
    Ok(format!("{}{line}{}", &text[..open + 1], rest))
}

/// `config set <file> <key> <value>`: see the top of this file. `confirm` shows the text to a person and says whether they typed yes. `Ok` is what to print.
pub fn set_text(data_dir: &Path, file: &str, key: &str, value: &str, confirm: &dyn Fn(&str) -> bool) -> Result<String, String> {
    let path = find_file(data_dir, file)?;
    let name = status(data_dir).into_iter().find(|r| r.path == path).map(|r| r.file.trim_end_matches(".json").trim_start_matches("tools/")).unwrap_or("");
    if !SETTABLE.contains(&(name, key)) {
        return Err(format!(
            "{file} {key}: this command changes only {} and nothing that lets Aqua do more or ask less; edit the file by hand, or change it in the app (policy.json and the rules are the app's)",
            SETTABLE.iter().map(|(f, k)| format!("{f}.json {k}")).collect::<Vec<_>>().join(" and ")
        ));
    }
    let flag = match value.trim().to_ascii_lowercase().as_str() {
        "true" | "on" | "1" | "yes" => true,
        "false" | "off" | "0" | "no" => false,
        _ => return Err(format!("{key} is true or false, not {}", crate::headless::render::plain(value))),
    };
    if !path.is_file() {
        return Err(format!("{} is not there: run `aqua-cli doctor --fix` to write the default, then set the key", path.display()));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("{} could not be read ({})", path.display(), e.kind()))?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8 text", path.display()))?;
    let new_text = set_in_text(&text, key, flag)?;
    // The new text has to be a configuration the loader would use: parsed as the loader parses it, into the type it loads.
    let prepared = aqua_tool_host::config_text::prepare(&new_text).map_err(|e| format!("the new text would not be valid: {e}"))?;
    serde_json::from_str::<aqua_native_tools::term::TermConfig>(&prepared).map_err(|e| format!("the new text would not be a valid {file}.json: {}", aqua_tool_host::config_text::scrub(&e.to_string())))?;
    if new_text == text {
        return Ok(format!("{file} {key} is already {flag}: nothing changed\n"));
    }
    if !confirm(&format!("This sets {key} to {flag} in {} (the core reads the file when it starts).\nIt keeps the comments and the line endings of the file.\n", path.display())) {
        return Err("nothing was changed".into());
    }
    aqua_tool_host::config_text::write_atomic(&path, new_text.as_bytes()).map_err(|e| format!("the file could not be written ({})", e.kind()))?;
    Ok(format!("{file} {key} is now {flag}: restart Aqua for it to be read\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(name: &str) -> std::path::PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("target").join("scratch_cli_tools").join(format!("config_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        dir
    }

    fn hide_secrets_of(text: &str) -> Value {
        let mut v: Value = serde_json::from_str(text).unwrap();
        hide_secrets(&mut v);
        v
    }

    const TERM_WITH_COMMENTS: &str = "{\r\n  // how many background jobs\r\n  \"max_sessions\": 4,\r\n  \"live_output\": false, // the lines of a running command\r\n  /* \"auto_background\": true in a comment */\r\n  \"path_dirs\": []\r\n}\r\n";

    #[test]
    fn set_keeps_the_comments_and_the_crlf_and_changes_only_the_value() {
        let d = data("set_crlf");
        std::fs::write(d.join("tools/term.json"), TERM_WITH_COMMENTS).unwrap();
        let said = set_text(&d, "term", "live_output", "on", &|_| true).unwrap();
        assert!(said.contains("now true"), "{said}");
        let after = std::fs::read(d.join("tools/term.json")).unwrap();
        let expected = TERM_WITH_COMMENTS.replacen("\"live_output\": false,", "\"live_output\": true,", 1);
        assert_eq!(String::from_utf8(after.clone()).unwrap(), expected, "only the value changed: the comments and the CRLF are as they were");
        let (lf, crlf) = (after.iter().filter(|b| **b == b'\n').count(), after.windows(2).filter(|w| *w == b"\r\n").count());
        assert_eq!((lf, crlf), (7, 7), "every line ending is still CRLF");
        // A key that is only in a comment is not the key: it is added, first, with the file's own line ending.
        set_text(&d, "term", "auto_background", "true", &|_| true).unwrap();
        let after = String::from_utf8(std::fs::read(d.join("tools/term.json")).unwrap()).unwrap();
        assert!(after.starts_with("{\r\n  \"auto_background\": true,\r\n  // how many"), "{after:?}");
        assert!(after.contains("/* \"auto_background\": true in a comment */"), "the comment is untouched");
        assert!(!after.replace("\r\n", "").contains('\n'), "no bare line feed was written");
        // Asking for what already is changes nothing and asks nothing.
        let same = set_text(&d, "term", "auto_background", "true", &|_| panic!("no question for no change")).unwrap();
        assert!(same.contains("nothing changed"), "{same}");
    }

    #[test]
    fn a_setting_that_asks_less_or_a_file_of_the_apps_is_refused_and_nothing_is_written() {
        let d = data("refused");
        let original = "{ \"check_grants\": false }\n";
        std::fs::write(d.join("tools/term.json"), original).unwrap();
        std::fs::write(d.join("tools/policy.json"), "{}\n").unwrap();
        for (file, key) in [("term", "check_grants"), ("term", "extended_project_checks"), ("term", "max_sessions"), ("policy", "mode"), ("fs", "roots"), ("hooks", "x")] {
            let e = set_text(&d, file, key, "true", &|_| panic!("a refused change asks nothing")).unwrap_err();
            assert!(e.contains("changes only term.json live_output and term.json auto_background") || e.contains("no configuration file"), "{file} {key}: {e}");
        }
        assert_eq!(std::fs::read_to_string(d.join("tools/term.json")).unwrap(), original);
        assert_eq!(std::fs::read_to_string(d.join("tools/policy.json")).unwrap(), "{}\n");
        // A value that is not a switch, and a person who does not say yes.
        assert!(set_text(&d, "term", "live_output", "maybe", &|_| true).unwrap_err().contains("true or false"));
        assert!(set_text(&d, "term", "live_output", "true", &|_| false).unwrap_err().contains("nothing was changed"));
        assert_eq!(std::fs::read_to_string(d.join("tools/term.json")).unwrap(), original);
    }

    #[test]
    fn a_file_that_would_not_load_after_the_change_is_not_written() {
        let d = data("invalid");
        // A file the loader refuses already (an unknown key): the change would leave it refused, so it is said and not written.
        let original = "{ \"live_outpt\": true }\n";
        std::fs::write(d.join("tools/term.json"), original).unwrap();
        let e = set_text(&d, "term", "live_output", "true", &|_| true).unwrap_err();
        assert!(e.contains("would not be a valid"), "{e}");
        assert_eq!(std::fs::read_to_string(d.join("tools/term.json")).unwrap(), original);
    }

    #[test]
    fn list_and_validate_read_the_same_status_as_the_doctor_and_say_what_is_broken() {
        let d = data("validate");
        std::fs::write(d.join("tools/term.json"), "{ \"max_sessions\": }").unwrap();
        let (text, broken) = validate_text(&d);
        assert!(broken, "{text}");
        assert!(text.contains("[ FAIL ] term") && text.contains("line 1"), "{text}");
        let listed = list_text(&d);
        assert!(listed.lines().any(|l| l.starts_with("term") && l.contains("broken")), "{listed}");
        // The report the doctor and the route read is the one these lines come from.
        let status = crate::config_files::status(&d);
        assert_eq!(status.iter().filter(|r| r.state == ConfigState::Broken).count(), 1);
        std::fs::write(d.join("tools/term.json"), "{ \"max_sessions\": 4 }").unwrap();
        assert!(!validate_text(&d).1);
    }

    #[test]
    fn get_prints_a_file_or_one_key_and_hides_what_looks_like_a_secret() {
        let d = data("get");
        std::fs::write(d.join("tools/term.json"), "{ // c\n \"max_sessions\": 4, \"live_output\": true }\n").unwrap();
        std::fs::write(d.join("tools/mcp.json"), "{ \"servers\": { \"x\": { \"command\": \"npx\", \"env\": { \"API_KEY\": \"sk-live-123456\", \"MODE\": \"fast\" } } } }\n").unwrap();
        assert!(get_text(&d, "term", None).unwrap().contains("\"max_sessions\": 4"));
        assert_eq!(get_text(&d, "term", Some("live_output")).unwrap().trim(), "true");
        let mcp = get_text(&d, "mcp", None).unwrap();
        // The value under a key that looks like a secret is never shown (the redactor writes its own mask over the word this writes); the rest is.
        assert!((mcp.contains("[hidden]") || mcp.contains("[REDACTED]")) && !mcp.contains("sk-live-123456") && mcp.contains("fast") && mcp.contains("npx"), "{mcp}");
        let direct = hide_secrets_of(r#"{ "token": "abc", "nested": { "my_password": "x", "name": "n" } }"#);
        assert_eq!(direct, serde_json::json!({ "token": "[hidden]", "nested": { "my_password": "[hidden]", "name": "n" } }));
        assert!(get_text(&d, "term", Some("nope")).is_err());
        assert!(get_text(&d, "nothing", None).unwrap_err().contains("config list"));
        assert!(get_text(&d, "fs", None).unwrap_err().contains("not there"));
    }

    #[test]
    fn env_lists_the_documented_variables_and_never_shows_the_value_of_one_that_holds_a_secret() {
        let lookup = |name: &str| match name {
            "AQUA_API_PORT" => Some("8790".to_string()),
            "AQUA_API_TOKEN" => Some("super-secret-token".to_string()),
            _ => None,
        };
        let all = env_text(false, &lookup);
        assert!(all.contains("AQUA_API_PORT") && all.contains("8790") && all.contains("AQUA_PREAPPROVALS") && all.contains("(not set)"), "{all}");
        assert!(!all.contains("super-secret-token"), "{all}");
        assert!(all.lines().any(|l| l.starts_with("AQUA_API_TOKEN") && l.contains("set (hidden)")), "{all}");
        let set = env_text(true, &lookup);
        assert_eq!(set.lines().count(), 2, "{set}");
        assert_eq!(env_text(true, &|_| None).trim(), "none of the variables Aqua reads is set in this environment");
    }
}
