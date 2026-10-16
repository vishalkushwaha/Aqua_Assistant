//! `aqua-cli door-report [--file PATH] [--min-runs N] [--max-looser N]`: the evidence for the decision to switch the one door on (card OA8-05; tasks SE-16, AQ-08 and AG-13 of the
//! OpenCode audit, decision D-03).
//!
//! In `AQUA_DOOR=shadow`, the default, the agent's browser and desktop calls go by their own routes and the tool host is only asked what it would have decided; `core/src/door_shadow.rs`
//! keeps the counts in `data/door_shadow.json`. This reads that file and says how many calls and runs were compared, per tool and per caller how often the door would have decided the
//! same as the old route, a stricter way, or a looser way, lists every looser decision and every kind of stricter one, and ends with one line: `enough evidence to flip: yes` or `no`,
//! against two numbers the person can change (`--min-runs`, 50 by default, and `--max-looser`, 0 by default). The line is advice for the person who decides: the report changes nothing, and
//! a stricter decision still has to be read by a person.
//!
//! It only reads one file (the path may be given; the default is the data folder) and writes only to standard output. It never contacts the core. The file holds tool names, caller words,
//! permission classes and decision words, never an argument; and every word of it that is printed is run through the tool host's redactor and cut to letters, digits and `_-.+:[]`, so that a
//! file somebody edited cannot make the report print more than words.
//!
//! The reader is its own and does not use the writer's types: it must keep reading the file of an earlier version (version 1 has no runs and no callers) and say what it does not know.

use aqua_tool_host::output::Redactor;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// The name of the file in the data folder.
pub const FILE_NAME: &str = "door_shadow.json";
/// How many runs are wanted before the report says "enough", unless `--min-runs` says another number. A proposal for the owner to adjust, not a rule of the program.
pub const DEFAULT_MIN_RUNS: u64 = 50;
/// How many looser decisions are allowed, unless `--max-looser` says another number.
pub const DEFAULT_MAX_LOOSER: u64 = 0;
/// A file larger than this is not read (the real one is a few kilobytes).
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// How many lines of stricter decisions are listed.
const MAX_STRICTER_LINES: usize = 30;

/// The two numbers the verdict is held against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    pub min_runs: u64,
    pub max_looser: u64,
}

impl Default for Params {
    fn default() -> Self {
        Self { min_runs: DEFAULT_MIN_RUNS, max_looser: DEFAULT_MAX_LOOSER }
    }
}

/// One of the latest looser calls of a version 2 file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Event {
    pub tool: String,
    pub caller: String,
    pub host: String,
    pub door: String,
    pub old: String,
    pub classes: String,
}

/// What the file says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Evidence {
    pub version: Option<u64>,
    /// `tool|host|door|old route` to a number of calls.
    pub counts: BTreeMap<String, u64>,
    /// The same keys, the ones the writer saw as looser.
    pub looser: BTreeMap<String, u64>,
    /// `tool|caller|verdict` to a number of calls (version 2).
    pub by_caller: BTreeMap<String, u64>,
    /// Runs that made a compared call. `None` in a file that does not count them (version 1).
    pub runs: Option<u64>,
    pub calls_without_run: Option<u64>,
    pub events: Vec<Event>,
    /// Entries of the tables whose number could not be read.
    pub unreadable_numbers: u64,
}

/// What was found at the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// There is no file: the door has been in shadow without a single compared call, or it was never started.
    Missing,
    /// There is one, and it cannot be used (empty, damaged, not the right kind of file, too large). The words say why.
    Unreadable(String),
    Data(Evidence),
}

/// Reads the file. It reads and nothing else.
pub fn read_file(path: &Path) -> Source {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Source::Missing,
        Err(e) => return Source::Unreadable(format!("it cannot be opened ({})", e.kind())),
    };
    if !meta.is_file() {
        return Source::Unreadable("it is not a file".into());
    }
    if meta.len() > MAX_FILE_BYTES {
        return Source::Unreadable(format!("it is {} bytes, more than the {MAX_FILE_BYTES} this report reads", meta.len()));
    }
    match std::fs::read(path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => match parse(&text) {
                Ok(evidence) => Source::Data(evidence),
                Err(why) => Source::Unreadable(why),
            },
            Err(_) => Source::Unreadable("it is not text".into()),
        },
        Err(e) => Source::Unreadable(format!("it cannot be read ({})", e.kind())),
    }
}

/// The evidence in the text of a file.
pub fn parse(text: &str) -> Result<Evidence, String> {
    let text = text.trim_start_matches('\u{feff}');
    if text.trim().is_empty() {
        return Err("the file is empty".into());
    }
    let value: Value = serde_json::from_str(text).map_err(|e| format!("the file is not valid JSON ({})", plain_words(&e.to_string())))?;
    let Some(object) = value.as_object() else { return Err("the file is JSON but not an object, so it is not a one-door comparison file".into()) };
    let table = |key: &str| -> Option<&serde_json::Map<String, Value>> { object.get(key).and_then(Value::as_object) };
    let Some(counts) = table("counts") else { return Err("the file has no `counts` table, so it is not a one-door comparison file".into()) };
    let mut unreadable_numbers = 0u64;
    let mut numbers = |t: Option<&serde_json::Map<String, Value>>| -> BTreeMap<String, u64> {
        let mut out = BTreeMap::new();
        for (k, v) in t.into_iter().flatten() {
            match v.as_u64() {
                Some(n) => {
                    out.insert(k.clone(), n);
                }
                None => unreadable_numbers += 1,
            }
        }
        out
    };
    let counts = numbers(Some(counts));
    let looser = numbers(table("looser"));
    let by_caller = numbers(table("by_caller"));
    let events = object
        .get("looser_events")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|e| {
                    let word = |key: &str| e.get(key).and_then(Value::as_str).unwrap_or("").to_string();
                    Event { tool: word("tool"), caller: word("caller"), host: word("host"), door: word("door"), old: word("old"), classes: word("classes") }
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Evidence { version: object.get("version").and_then(Value::as_u64), counts, looser, by_caller, runs: object.get("runs").and_then(Value::as_u64), calls_without_run: object.get("calls_without_run").and_then(Value::as_u64), events, unreadable_numbers })
}

// ---------------------------------------------------------------------------------------------

/// How a decision of the door stands against what the old route did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Same,
    Stricter,
    Looser,
}

/// The rung of the ladder `auto < ask < always_ask < deny` a decision word names.
fn door_rank(word: &str) -> Option<u8> {
    match word {
        "auto" => Some(0),
        "ask" => Some(1),
        "always_ask" => Some(2),
        "deny" => Some(3),
        _ => None,
    }
}

/// The rung the old route stood on: it ran or failed (nobody was asked), it asked, or a rule refused it.
fn old_rank(word: &str) -> Option<u8> {
    match word {
        "ran" | "failed" => Some(0),
        "asked" => Some(1),
        "refused" => Some(3),
        _ => None,
    }
}

/// Is the door looser, the same or stricter than the old route? The same comparison `door_shadow::is_looser` makes.
pub fn kind_of(door: &str, old: &str) -> Option<Kind> {
    let (d, o) = (door_rank(door)?, old_rank(old)?);
    Some(match d.cmp(&o) {
        std::cmp::Ordering::Less => Kind::Looser,
        std::cmp::Ordering::Equal => Kind::Same,
        std::cmp::Ordering::Greater => Kind::Stricter,
    })
}

/// One entry of the `counts` table.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    tool: String,
    host: String,
    door: String,
    old: String,
    n: u64,
    kind: Kind,
}

/// What the counts add up to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Analysis {
    pub calls: u64,
    pub same: u64,
    pub stricter: u64,
    pub looser: u64,
    /// Entries of `counts` whose key is not `tool|host|door|old route` with words this report knows.
    pub unknown_entries: u64,
    /// Per tool: same, stricter, looser.
    pub per_tool: BTreeMap<String, [u64; 3]>,
    rows: Vec<Row>,
}

pub fn analyse(evidence: &Evidence) -> Analysis {
    let mut a = Analysis::default();
    for (key, n) in &evidence.counts {
        a.calls = a.calls.saturating_add(*n);
        let parts: Vec<&str> = key.split('|').collect();
        let kind = match parts.as_slice() {
            [_, _, door, old] => kind_of(door, old),
            _ => None,
        };
        let Some(kind) = kind else {
            a.unknown_entries += 1;
            continue;
        };
        let slot = match kind {
            Kind::Same => 0,
            Kind::Stricter => 1,
            Kind::Looser => 2,
        };
        match kind {
            Kind::Same => a.same = a.same.saturating_add(*n),
            Kind::Stricter => a.stricter = a.stricter.saturating_add(*n),
            Kind::Looser => a.looser = a.looser.saturating_add(*n),
        }
        let tool = parts[0].to_string();
        let per = a.per_tool.entry(tool.clone()).or_insert([0; 3]);
        per[slot] = per[slot].saturating_add(*n);
        a.rows.push(Row { tool, host: parts[1].into(), door: parts[2].into(), old: parts[3].into(), n: *n, kind });
    }
    a
}

/// The advice, with the reasons that make it "no".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub enough: bool,
    pub reasons: Vec<String>,
}

/// How many looser decisions there are: what the counts show and what the file's own `looser` table says, whichever is more.
fn looser_total(evidence: &Evidence, analysis: &Analysis) -> u64 {
    analysis.looser.max(evidence.looser.values().fold(0u64, |s, n| s.saturating_add(*n)))
}

pub fn verdict(source: &Source, params: &Params) -> Verdict {
    let mut reasons: Vec<String> = Vec::new();
    match source {
        Source::Missing => reasons.push("there is no file: no browser or desktop call has been compared yet".into()),
        Source::Unreadable(why) => reasons.push(format!("the file cannot be used: {}", plain_words(why))),
        Source::Data(e) => {
            let a = analyse(e);
            if a.calls == 0 {
                reasons.push("no call has been compared yet".into());
            }
            match e.runs {
                None => reasons.push(format!("the file is version {} and does not count runs, so the number of runs is not known (this build writes version 2: start the trial on a core of this build)", e.version.map_or("?".to_string(), |v| v.to_string()))),
                Some(r) if r < params.min_runs => reasons.push(format!("{r} runs seen, {} wanted", params.min_runs)),
                Some(_) => {}
            }
            let looser = looser_total(e, &a);
            if looser > params.max_looser {
                reasons.push(format!("{looser} looser decisions, {} allowed", params.max_looser));
            }
        }
    }
    Verdict { enough: reasons.is_empty(), reasons }
}

// ---------------------------------------------------------------------------------------------

/// A word from the file as it may be printed: cut, run through the tool host's redactor, and only letters, digits and `_-.+:[]` kept.
fn word(redactor: &Redactor, text: &str) -> String {
    let clipped: String = text.chars().take(200).collect();
    let masked = redactor.redact(&clipped);
    let kept: String = masked.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | ':' | '[' | ']') { c } else { '?' }).take(48).collect();
    if kept.is_empty() {
        "?".into()
    } else {
        kept
    }
}

/// Words of our own or of an error message, drawn for a terminal.
fn plain_words(text: &str) -> String {
    crate::headless::render::plain(&text.chars().take(200).collect::<String>())
}

fn age_text(age_secs: Option<u64>) -> String {
    match age_secs {
        None => "not known".into(),
        Some(s) if s < 90 => format!("{s} seconds ago"),
        Some(s) if s < 5_400 => format!("{} minutes ago", s / 60),
        Some(s) if s < 129_600 => format!("{} hours ago", s / 3_600),
        Some(s) => format!("{} days ago", s / 86_400),
    }
}

/// The report as text. `file` is the path as it is shown; `age_secs` is how long ago the file was written (from the file's own time), if known.
pub fn render(source: &Source, params: &Params, file: &str, age_secs: Option<u64>) -> String {
    let redactor = Redactor::new();
    let w = |t: &str| word(&redactor, t);
    let mut out = String::new();
    let mut line = |text: String| {
        out.push_str(&text);
        out.push('\n');
    };
    line("One-door evidence (aqua-cli door-report)".into());
    line(format!("file: {}", plain_words(file)));
    line(format!("wanted before the advice is yes: at least {} runs and at most {} looser decisions (--min-runs and --max-looser change them; the person who decides sets them)", params.min_runs, params.max_looser));
    line(String::new());
    match source {
        Source::Missing => line("There is no file at that path. A file appears when the first browser or desktop call is compared in shadow mode.".into()),
        Source::Unreadable(why) => line(format!("The file cannot be used: {}.", plain_words(why))),
        Source::Data(e) => {
            let a = analyse(e);
            let looser = looser_total(e, &a);
            let version = e.version.map_or("not stated".to_string(), |v| v.to_string());
            line(format!("version {version}, last written {}", age_text(age_secs)));
            if e.version.is_some_and(|v| v > 2) {
                line("This file is newer than this build of the report knows: what it knows is shown, and nothing else.".into());
            }
            line(String::new());
            line(format!("calls compared: {}", a.calls));
            match e.runs {
                Some(r) => line(format!("runs seen: {r} (a run is counted once; calls with no run behind them, the app's own direct calls: {})", e.calls_without_run.map_or("not counted".to_string(), |n| n.to_string()))),
                None => line("runs seen: not recorded (this file is version 1: it counts calls, not runs)".into()),
            }
            line(format!("the one door against the old routes: same {}, stricter {}, looser {}", a.same, a.stricter, looser.max(a.looser)));
            if a.unknown_entries > 0 || e.unreadable_numbers > 0 {
                line(format!("not understood: {} entries of the counts and {} numbers (left out of the sums above)", a.unknown_entries, e.unreadable_numbers));
            }
            line(String::new());

            line("By tool (same / stricter / looser):".into());
            if a.per_tool.is_empty() {
                line("  none".into());
            }
            for (tool, [same, stricter, looser]) in &a.per_tool {
                line(format!("  {:<30} same {same:>6}  stricter {stricter:>6}  looser {looser:>6}", w(tool)));
            }
            line(String::new());

            line("By tool and caller (same / stricter / looser):".into());
            let mut table: BTreeMap<(String, String), [u64; 3]> = BTreeMap::new();
            for (key, n) in &e.by_caller {
                let parts: Vec<&str> = key.split('|').collect();
                if let [tool, caller, verdict] = parts.as_slice() {
                    let slot = match *verdict {
                        "same" => 0,
                        "stricter" => 1,
                        "looser" => 2,
                        _ => continue,
                    };
                    let per = table.entry((w(tool), w(caller))).or_insert([0; 3]);
                    per[slot] = per[slot].saturating_add(*n);
                }
            }
            if table.is_empty() {
                line("  not recorded (a version 2 file has this; a version 1 file does not)".into());
            }
            for ((tool, caller), [same, stricter, looser]) in &table {
                line(format!("  {tool:<30} {caller:<12} same {same:>6}  stricter {stricter:>6}  looser {looser:>6}"));
            }
            line(String::new());

            line(format!("Looser decisions (the door would decide LOOSER than the old route did): {looser}"));
            let mut shown = 0;
            for row in a.rows.iter().filter(|r| r.kind == Kind::Looser) {
                shown += 1;
                line(format!("  {}: the door says {}, the old route {} (the host says {}), {} times", w(&row.tool), w(&row.door), w(&row.old), w(&row.host), row.n));
            }
            if shown == 0 && looser > 0 {
                line("  the file's own looser table lists some that the counts do not show: look at the file".into());
            }
            for ev in &e.events {
                line(format!("  latest: {} by {}, classes {}: the door says {}, the old route {}, the host says {}", w(&ev.tool), w(&ev.caller), if ev.classes.is_empty() { "none given".to_string() } else { w(&ev.classes) }, w(&ev.door), w(&ev.old), w(&ev.host)));
            }
            if looser == 0 {
                line("  none".into());
            }
            line(String::new());

            let mut stricter: BTreeMap<(String, String, String), u64> = BTreeMap::new();
            for row in a.rows.iter().filter(|r| r.kind == Kind::Stricter) {
                let slot = stricter.entry((w(&row.tool), w(&row.door), w(&row.old))).or_insert(0);
                *slot = slot.saturating_add(row.n);
            }
            let mut ordered: Vec<_> = stricter.into_iter().collect();
            ordered.sort_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(&y.0)));
            line(format!("Stricter decisions to review ({} kinds, {} calls): each is a call where the door would ask or refuse and the old route did not:", ordered.len(), a.stricter));
            for ((tool, door, old), n) in ordered.iter().take(MAX_STRICTER_LINES) {
                line(format!("  {tool}: the door says {door}, the old route {old}, {n} times"));
            }
            if ordered.len() > MAX_STRICTER_LINES {
                line(format!("  and {} more kinds", ordered.len() - MAX_STRICTER_LINES));
            }
            if ordered.is_empty() {
                line("  none".into());
            }
            line(String::new());
        }
    }
    let v = verdict(source, params);
    line(format!("enough evidence to flip: {}", if v.enough { "yes" } else { "no" }));
    for reason in &v.reasons {
        line(format!("  - {reason}"));
    }
    if v.enough {
        line("  - this is advice. Read the stricter decisions above first: the report cannot judge whether the door asking where the old route did not is what you want.".into());
        line("  - flipping is the person's decision (documentation/ONE_DOOR_PLAN.md says how, and how to flip back).".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("door_report_fixtures").join(name)
    }

    fn report(name: &str, params: Params) -> (Source, String) {
        let source = read_file(&fixture(name));
        let text = render(&source, &params, &format!("tests/door_report_fixtures/{name}"), Some(7_200));
        (source, text)
    }

    fn evidence(source: &Source) -> &Evidence {
        match source {
            Source::Data(e) => e,
            other => panic!("expected data, got {other:?}"),
        }
    }

    #[test]
    fn a_clean_file_with_enough_runs_says_yes_and_lists_the_stricter_decisions_for_a_person_to_read() {
        let (source, text) = report("v2_enough.json", Params::default());
        let e = evidence(&source);
        assert_eq!((e.version, e.runs, e.calls_without_run), (Some(2), Some(64), Some(2)));
        let a = analyse(e);
        assert_eq!((a.calls, a.same, a.stricter, a.looser, a.unknown_entries), (401, 386, 15, 0, 0), "{a:?}");
        assert!(text.contains("enough evidence to flip: yes"), "{text}");
        assert!(text.contains("calls compared: 401") && text.contains("runs seen: 64") && text.contains("last written 2 hours ago"), "{text}");
        assert!(text.contains("same 386, stricter 15, looser 0"), "{text}");
        // Per tool, and per tool and caller.
        assert!(text.contains("browser.click") && text.contains("same     55  stricter      9  looser      0"), "{text}");
        assert!(text.contains("win.type") && text.contains("voice") && text.contains("stricter      6"), "{text}");
        // Each kind of stricter decision, and the reminder that a person reads them.
        assert!(text.contains("Stricter decisions to review (2 kinds, 15 calls)"), "{text}");
        assert!(text.contains("browser.click: the door says always_ask, the old route ran, 9 times"), "{text}");
        assert!(text.contains("Read the stricter decisions above first"), "{text}");
        assert!(text.contains("Looser decisions") && text.contains("  none"), "{text}");
    }

    #[test]
    fn a_looser_decision_is_listed_with_its_tool_caller_and_classes_and_the_verdict_is_no() {
        let (source, text) = report("v2_with_looser.json", Params::default());
        let a = analyse(evidence(&source));
        assert_eq!(a.looser, 3);
        assert!(text.contains("enough evidence to flip: no"), "{text}");
        assert!(text.contains("3 looser decisions, 0 allowed"), "{text}");
        assert!(text.contains("browser.click: the door says auto, the old route asked (the host says auto), 2 times"), "{text}");
        assert!(text.contains("win.type: the door says ask, the old route refused (the host says ask), 1 times"), "{text}");
        assert!(text.contains("latest: browser.click by agent, classes network+write: the door says auto, the old route asked, the host says auto"), "{text}");
        assert!(text.contains("latest: win.type by voice, classes ?SECRET?"), "a word with a character that is not one is shown with a ?: {text}");
        // 80 runs is enough runs: it is the looser decisions that make it no.
        assert!(!text.contains("runs seen, 50 wanted"), "{text}");
        // A person who accepts three of them says so, and the verdict follows the numbers they gave.
        let (_, accepted) = report("v2_with_looser.json", Params { min_runs: 50, max_looser: 3 });
        assert!(accepted.contains("enough evidence to flip: yes"), "{accepted}");
        let (_, short) = report("v2_with_looser.json", Params { min_runs: 81, max_looser: 3 });
        assert!(short.contains("enough evidence to flip: no") && short.contains("80 runs seen, 81 wanted"), "{short}");
    }

    #[test]
    fn nothing_that_looks_like_a_secret_is_printed_whatever_the_file_holds() {
        // The fixture holds a placeholder in one class field; the secret-shaped word is made here, so that no file of the repository has one.
        let secret = format!("{}{}", "sk-live-", "A1b2C3d4".repeat(5));
        let text = std::fs::read_to_string(fixture("v2_with_looser.json")).unwrap().replace("@SECRET@", &secret);
        let evidence = parse(&text).unwrap();
        assert!(evidence.events.iter().any(|e| e.classes.contains(&secret)), "the placeholder was replaced");
        let out = render(&Source::Data(evidence), &Params::default(), "x", None);
        assert!(!out.contains(&secret) && !out.contains("A1b2C3d4"), "{out}");
        assert!(out.contains("[REDACTED]"), "{out}");
        // A line break or an escape in a word is not printed either.
        let nasty = parse(r#"{"version": 2, "counts": {"browser.click|ask|auto|asked": 1}, "looser": {}, "looser_events": [{"tool": "x\u001b[31m\ny", "caller": "a b", "host": "ask", "door": "auto", "old": "asked", "classes": ""}]}"#).unwrap();
        let out = render(&Source::Data(nasty), &Params::default(), "f\u{1b}[0m", None);
        assert!(!out.contains('\u{1b}') && out.lines().all(|l| !l.contains('\r')), "{out:?}");
        assert!(out.contains("latest: x?[31m?y by a?b"), "{out}");
    }

    #[test]
    fn too_few_runs_is_no_and_the_numbers_are_the_persons_to_change() {
        let (source, text) = report("v2_few_runs.json", Params::default());
        assert_eq!(evidence(&source).runs, Some(3));
        assert!(text.contains("enough evidence to flip: no") && text.contains("3 runs seen, 50 wanted"), "{text}");
        assert!(!text.contains("looser decisions,"), "no looser decision: only the runs hold it back: {text}");
        assert!(report("v2_few_runs.json", Params { min_runs: 3, max_looser: 0 }).1.contains("enough evidence to flip: yes"));
        assert!(report("v2_few_runs.json", Params { min_runs: 4, max_looser: 0 }).1.contains("enough evidence to flip: no"));
    }

    #[test]
    fn a_file_of_the_older_format_is_read_for_its_counts_and_says_that_it_cannot_show_the_runs() {
        let (source, text) = report("v1_old_format.json", Params::default());
        let e = evidence(&source);
        assert_eq!((e.version, e.runs, e.by_caller.len()), (Some(1), None, 0));
        assert_eq!(analyse(e).calls, 19);
        assert!(text.contains("runs seen: not recorded (this file is version 1"), "{text}");
        assert!(text.contains("By tool and caller") && text.contains("not recorded (a version 2 file has this"), "{text}");
        assert!(text.contains("enough evidence to flip: no") && text.contains("does not count runs"), "{text}");
        // With no runs to count, no number of runs wanted makes it yes: --min-runs 0 does not hide the missing information.
        let (_, zero) = report("v1_old_format.json", Params { min_runs: 0, max_looser: 0 });
        assert!(zero.contains("enough evidence to flip: no"), "{zero}");
    }

    #[test]
    fn an_empty_a_damaged_a_missing_and_a_wrong_file_are_each_said_in_words_and_never_yes() {
        for (name, words) in [
            ("empty.json", "the file is empty"),
            ("corrupt.json", "the file is not valid JSON"),
            ("wrong_shape.json", "has no `counts` table"),
            ("not_an_object.json", "JSON but not an object"),
        ] {
            let (source, text) = report(name, Params::default());
            assert!(matches!(&source, Source::Unreadable(why) if why.contains(words)), "{name}: {source:?}");
            assert!(text.contains("The file cannot be used") && text.contains(words) && text.contains("enough evidence to flip: no"), "{name}: {text}");
        }
        let (source, text) = report("does_not_exist.json", Params::default());
        assert_eq!(source, Source::Missing);
        assert!(text.contains("There is no file at that path") && text.contains("enough evidence to flip: no"), "{text}");
        // A folder is not a file.
        assert!(matches!(read_file(&fixture("")), Source::Unreadable(why) if why.contains("not a file")));
    }

    #[test]
    fn entries_the_report_does_not_understand_are_counted_and_named_not_guessed() {
        let (source, text) = report("odd_entries.json", Params::default());
        let e = evidence(&source);
        let a = analyse(e);
        assert_eq!((a.calls, a.unknown_entries, e.unreadable_numbers), (12, 3, 1), "{a:?}");
        assert_eq!((a.same, a.stricter, a.looser), (6, 0, 0), "an entry it cannot place is in no sum but the total of calls");
        assert!(text.contains("not understood: 3 entries of the counts and 1 numbers"), "{text}");
        // A file with a newer version still gives what it has, and says so.
        let (_, newer) = report("newer_version.json", Params::default());
        assert!(newer.contains("newer than this build") && newer.contains("calls compared: 7"), "{newer}");
    }

    #[test]
    fn the_file_the_writer_makes_is_the_file_the_report_reads() {
        use crate::door_shadow::{is_looser, word_decision, CallFacts, Stance, Tally};
        let mut t = Tally::default();
        let mut note = |tool: &str, host: &str, door: &str, stance: Stance, caller: &str, run: &str| {
            let looser = is_looser(word_decision(door), stance);
            t.note(tool, host, door, stance, looser);
            t.note_facts(tool, host, door, stance, looser, &CallFacts { caller: caller.into(), run: run.into(), classes: "network".into() });
        };
        note("browser.navigate", "auto", "auto", Stance::Ran, "agent", "run_a");
        note("browser.navigate", "auto", "ask", Stance::Ran, "agent", "run_b");
        note("browser.click", "ask", "auto", Stance::Asked, "voice", "run_b");
        let dir = std::env::temp_dir().join(format!("aqua_door_report_writer_{}", std::process::id()));
        let path = dir.join(FILE_NAME);
        t.save(&path).unwrap();
        let source = read_file(&path);
        let e = evidence(&source);
        assert_eq!((e.version, e.runs, e.calls_without_run), (Some(crate::door_shadow::FILE_VERSION), Some(2), Some(0)));
        let a = analyse(e);
        assert_eq!((a.calls, a.same, a.stricter, a.looser), (3, 1, 1, 1));
        assert_eq!(looser_total(e, &a), t.looser_total(), "the reader and the writer count the same looser calls");
        let text = render(&source, &Params { min_runs: 2, max_looser: 0 }, "x", None);
        assert!(text.contains("browser.click") && text.contains("voice") && text.contains("enough evidence to flip: no"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_verdict_is_a_function_of_the_numbers_and_never_of_a_file_it_could_not_read() {
        let params = Params::default();
        assert!(!verdict(&Source::Missing, &params).enough);
        assert!(!verdict(&Source::Unreadable("x".into()), &Params { min_runs: 0, max_looser: u64::MAX }).enough);
        let mut e = Evidence { version: Some(2), runs: Some(50), ..Default::default() };
        assert!(!verdict(&Source::Data(e.clone()), &params).enough, "no call at all is no evidence, whatever the runs say");
        e.counts.insert("browser.click|ask|ask|asked".into(), 5);
        assert!(verdict(&Source::Data(e.clone()), &params).enough);
        // The file's own looser table counts as well, if its counts hide the looser calls.
        e.looser.insert("browser.click|ask|auto|asked".into(), 1);
        let v = verdict(&Source::Data(e), &params);
        assert!(!v.enough && v.reasons.iter().any(|r| r.contains("1 looser decisions, 0 allowed")), "{v:?}");
    }
}
