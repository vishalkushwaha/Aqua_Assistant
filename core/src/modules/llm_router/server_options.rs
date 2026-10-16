//! What the speed experiments of card OA9-06 (tasks PR-11 to PR-14 and CX-18 of the OpenCode audit) add to the command line of the model server, and what Aqua says about it.
//!
//! The first four switches of the model server (`AQUA_LLAMA_KV`, `AQUA_LLAMA_FA`, `AQUA_LLAMA_SPEC`, `AQUA_LLAMA_THINK_BUDGET`) are in `launch_args.rs` since card R3-01. This file holds
//! the ones that came after, each a flag of the registry (`flags.rs`, off, and a promotion may not set it), and none of them changes anything until it is on:
//!
//! * `AQUA_LLAMA_CACHE_REUSE` adds `--cache-reuse N` and `AQUA_LLAMA_CACHE_RAM` adds `--cache-ram N` (the server's own cache of idle prompts, in MiB);
//! * `AQUA_LLAMA_SLOT_SAVE` adds `--slot-save-path data/cache/kv`, a folder of Aqua's own that holds nothing but the saved state of a slot (`kv_slots.rs`);
//! * `AQUA_LLAMA_DRAFT` (`cpu` or `gpu`; `on` is `cpu`) adds a draft model for speculative decoding (`--spec-draft-model` and its tuning), the file named by `AQUA_LLAMA_DRAFT_MODEL`
//!   and checked by `draft_model.rs`. The n-gram types of speculation (`AQUA_LLAMA_SPEC=ngram`, card R3-01) are in `launch_args.rs`; with both on, the draft is not used.
//!
//! **Each value is read against a short list** ([`CACHE_REUSE_VALUES`], [`CACHE_RAM_VALUES`]) and the text of the environment is never passed on: a value that is not on the list is
//! refused with a sentence in the log (and in [`report`]) and the switch stays off. **Each option is added only if the build of `llama-server` lists it in its `--help`** (read when a
//! switch is on, and not otherwise: the usual launch runs nothing it did not run before), so a build without the option is started as before and the report says why. With every
//! switch off [`extra_args`] adds nothing at all, and a test holds the whole command line of the usual launch to what it always was.
//!
//! The option names are the ones of the vendored source (`vendor/llama.cpp/common/arg.cpp`, read on 2026-10-12); what the build that Aqua really starts does with them has not been
//! measured here: no model was run for this card.

use super::draft_model::{self, Place};
use super::launch_args::{self, Capabilities};
use crate::flags;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What `AQUA_LLAMA_CACHE_REUSE` may be (the smallest piece of a prompt, in tokens, that the server takes from its cache by shifting it). `off` is the default and passes no option.
pub(crate) const CACHE_REUSE_VALUES: [&str; 5] = ["off", "64", "128", "256", "512"];
/// What `AQUA_LLAMA_CACHE_RAM` may be (MiB of memory for the prompts of idle slots; 0 keeps none). `off` is the default and passes no option.
pub(crate) const CACHE_RAM_VALUES: [&str; 6] = ["off", "0", "512", "1024", "2048", "4096"];

/// The text of the variables, as the environment has them (`None`: not set).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RawExtras {
    pub cache_reuse: Option<String>,
    pub cache_ram: Option<String>,
    pub slot_save: Option<String>,
    pub draft: Option<String>,
    /// The settings that name a file or a folder (they are text, not switches, so they are not in the registry): `AQUA_LLAMA_DRAFT_MODEL` and `AQUA_LLAMA_DRAFT_FOLDER`.
    pub draft_model: Option<String>,
    pub draft_folder: Option<String>,
}

/// The variables of the environment (or of a test's pinned values: see `flags::with_overrides`).
pub(crate) fn raw_extras() -> RawExtras {
    RawExtras {
        cache_reuse: flags::raw(&flags::LLAMA_CACHE_REUSE),
        cache_ram: flags::raw(&flags::LLAMA_CACHE_RAM),
        slot_save: flags::raw(&flags::LLAMA_SLOT_SAVE),
        draft: flags::raw(&flags::LLAMA_DRAFT),
        draft_model: std::env::var("AQUA_LLAMA_DRAFT_MODEL").ok(),
        draft_folder: std::env::var("AQUA_LLAMA_DRAFT_FOLDER").ok(),
    }
}

/// What the variables ask for: each one of its allowed values (the text of the environment is never kept, only the entry of the list it matched).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Extras {
    pub cache_reuse: &'static str,
    pub cache_ram: &'static str,
    pub slot_save: bool,
    /// Where the draft model runs, when `AQUA_LLAMA_DRAFT` is on.
    pub draft: Option<Place>,
    /// The agent's model is spoken to in native tool calls (`AQUA_PLANNER_TOOL_MODE` or its profile): the server needs its own templates. Set by the launch from the model's file, not by `parse_extras`.
    pub native_tools: bool,
}

impl Default for Extras {
    fn default() -> Self {
        Extras { cache_reuse: CACHE_REUSE_VALUES[0], cache_ram: CACHE_RAM_VALUES[0], slot_save: false, draft: None, native_tools: false }
    }
}

impl Extras {
    /// Is every switch off (the usual launch)?
    pub(crate) fn is_default(&self) -> bool {
        *self == Extras::default()
    }
}

/// A value that was refused. The log line quotes the start of what the environment said (the caller prints it with `{:?}`, which escapes what is not text); **the status never does**,
/// since a variable can hold whatever someone typed into it (the route that lists the flags says the same and shows only what the code made of the value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    name: String,
    shown: String,
    why: String,
}

impl Refusal {
    fn new(name: &str, text: &str, why: String) -> Refusal {
        Refusal { name: name.to_string(), shown: text.chars().take(24).collect(), why }
    }

    /// For the log: `AQUA_LLAMA_CACHE_RAM="99" is not one of ...`.
    pub(crate) fn log_line(&self) -> String {
        format!("{}={:?} {}", self.name, self.shown, self.why)
    }

    /// For the status: the name of the variable and why, with none of what was typed.
    pub(crate) fn status_line(&self) -> String {
        format!("{} {}", self.name, self.why)
    }
}

/// The entry of `list` that `raw` names (any case, spaces around ignored). Nothing set, or blank, is the default without a warning; a value that is not on the list is refused with one.
fn allowed(name: &str, raw: &Option<String>, list: &[&'static str], warnings: &mut Vec<Refusal>) -> Option<&'static str> {
    let text = raw.as_deref()?.trim();
    if text.is_empty() {
        return None;
    }
    let found = list.iter().find(|v| v.eq_ignore_ascii_case(text)).copied();
    if found.is_none() {
        warnings.push(Refusal::new(name, text, format!("is not one of {}; it is refused and the option is not passed to the server", list.join(", "))));
    }
    found
}

/// A switch that is on or off. Its own spellings turn it on; `off`, `0`, `false` and `no` are off; anything else is refused with a sentence and is off.
fn switch(flag: &flags::Flag, raw: &Option<String>, warnings: &mut Vec<Refusal>) -> bool {
    let Some(text) = raw.as_deref().map(str::trim).filter(|t| !t.is_empty()) else { return false };
    if flag.parse_switch(Some(text)) {
        return true;
    }
    if !["off", "0", "false", "no"].iter().any(|o| o.eq_ignore_ascii_case(text)) {
        warnings.push(Refusal::new(flag.name, text, "is not one of on, 1, true, off, 0, false or no; it is refused and the switch stays off".to_string()));
    }
    false
}

/// The switches the variables ask for, and a sentence for each value that is not allowed (the switch stays off).
pub(crate) fn parse_extras(raw: &RawExtras) -> (Extras, Vec<Refusal>) {
    let mut warnings = Vec::new();
    let cache_reuse = allowed(flags::LLAMA_CACHE_REUSE.name, &raw.cache_reuse, &CACHE_REUSE_VALUES, &mut warnings).unwrap_or(CACHE_REUSE_VALUES[0]);
    let cache_ram = allowed(flags::LLAMA_CACHE_RAM.name, &raw.cache_ram, &CACHE_RAM_VALUES, &mut warnings).unwrap_or(CACHE_RAM_VALUES[0]);
    let slot_save = switch(&flags::LLAMA_SLOT_SAVE, &raw.slot_save, &mut warnings);
    let draft = draft_place(&raw.draft, &mut warnings);
    (Extras { cache_reuse, cache_ram, slot_save, draft, native_tools: false }, warnings)
}

/// What `AQUA_LLAMA_DRAFT` asks for: `cpu` (also `on`, `1`, `true`, `yes`) or `gpu`; `off` and the other off words are nothing; anything else is refused with a sentence.
fn draft_place(raw: &Option<String>, warnings: &mut Vec<Refusal>) -> Option<Place> {
    let text = raw.as_deref().map(str::trim).filter(|t| !t.is_empty())?;
    match text.to_ascii_lowercase().as_str() {
        "cpu" | "on" | "1" | "true" | "yes" => Some(Place::Processor),
        "gpu" => Some(Place::Card),
        "off" | "0" | "false" | "no" => None,
        _ => {
            warnings.push(Refusal::new(flags::LLAMA_DRAFT.name, text, "is not one of cpu, gpu, on, off; it is refused and the draft model is not used".to_string()));
            None
        }
    }
}

/// What the switches add to the command line, and what was asked for and could not be added.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Added {
    pub args: Vec<OsString>,
    /// What was added, as the log says it ("cache_reuse=256").
    pub applied: Vec<String>,
    /// What was asked for and not added, and why.
    pub notes: Vec<String>,
}

fn os(list: &[&str]) -> Vec<OsString> {
    list.iter().map(|s| OsString::from(*s)).collect()
}

/// What the launch has to look at to add the options that need it: the folder of saved slots (made when it is asked for), the draft model's file (checked when it is asked for), and
/// whether the n-gram speculation of `AQUA_LLAMA_SPEC` is already on the command line.
pub(crate) struct Needs<'a> {
    pub slot_dir: &'a dyn Fn() -> Result<PathBuf, String>,
    pub draft_file: &'a dyn Fn() -> Result<PathBuf, String>,
    pub speculating: bool,
}

/// What the switches add to the command line (nothing with every switch off), given what the build lists. Each closure of `needs` is called only when its switch is on and the build
/// has the option.
pub(crate) fn extra_args(e: &Extras, caps: &Capabilities, needs: &Needs<'_>) -> Added {
    let mut out = Added::default();
    let slot_dir = needs.slot_dir;
    let unavailable = |what: &str, option: &str| format!("{what} was not used: this build of llama-server does not list {option}");
    if e.cache_reuse != CACHE_REUSE_VALUES[0] {
        if caps.extra.cache_reuse {
            out.args.extend(os(&["--cache-reuse", e.cache_reuse]));
            out.applied.push(format!("cache_reuse={}", e.cache_reuse));
        } else {
            out.notes.push(unavailable(&format!("{}={}", flags::LLAMA_CACHE_REUSE.name, e.cache_reuse), "--cache-reuse"));
        }
    }
    if e.cache_ram != CACHE_RAM_VALUES[0] {
        if caps.extra.cache_ram {
            out.args.extend(os(&["--cache-ram", e.cache_ram]));
            out.applied.push(format!("cache_ram={}", e.cache_ram));
        } else {
            out.notes.push(unavailable(&format!("{}={}", flags::LLAMA_CACHE_RAM.name, e.cache_ram), "--cache-ram"));
        }
    }
    if e.slot_save {
        if !caps.extra.slot_save_path {
            out.notes.push(unavailable(flags::LLAMA_SLOT_SAVE.name, "--slot-save-path"));
        } else {
            match slot_dir() {
                Ok(dir) => {
                    out.args.push(OsString::from("--slot-save-path"));
                    out.args.push(dir.into_os_string());
                    out.applied.push("slot_save".to_string());
                }
                Err(why) => out.notes.push(format!("{} was not used: {why}", flags::LLAMA_SLOT_SAVE.name)),
            }
        }
    }
    if let Some(place) = e.draft {
        let name = flags::LLAMA_DRAFT.name;
        if needs.speculating {
            out.notes.push(format!("{name} was not used: AQUA_LLAMA_SPEC is on, and the draft model and the n-gram speculation are separate experiments (turn one of them off)"));
        } else {
            // The build is asked first (nothing is read from the disk for a build that could not use the file), then the file.
            match draft_model::args_for(place, Path::new("draft.gguf"), caps).and_then(|_| (needs.draft_file)()).and_then(|file| draft_model::args_for(place, &file, caps)) {
                Ok((args, applied)) => {
                    out.args.extend(args);
                    out.applied.push(applied);
                }
                Err(why) => out.notes.push(format!("{name} was not used: {why}")),
            }
        }
    }
    if e.native_tools {
        let name = "AQUA_PLANNER_TOOL_MODE (native)";
        if !caps.extra.jinja {
            out.notes.push(format!("{name}: this build of llama-server does not list --jinja, so it is started as it was and may refuse a request that has tools (the planner then asks the JSON way)"));
        } else if caps.extra.jinja_on_by_default {
            out.applied.push("native_tools (the server's templates are on in this build)".to_string());
        } else {
            out.args.push(OsString::from("--jinja"));
            out.applied.push("native_tools (--jinja)".to_string());
        }
    }
    out
}

/// The whole argument list of a launch: the launch's own with the four older switches (`launch_args::launch_args`), and after it what the newer switches add. One function, so that a
/// test sees exactly what the launch passes to the server; with every switch off the list is the one the launch has always passed.
pub(crate) fn full_launch(values: &launch_args::Values, old: &launch_args::Switches, more: &Extras, caps: &Capabilities, slot_dir: &dyn Fn() -> Result<PathBuf, String>, draft_file: &dyn Fn() -> Result<PathBuf, String>) -> launch_args::Launch {
    let mut launch = launch_args::launch_args(values, old, caps);
    let added = extra_args(more, caps, &Needs { slot_dir, draft_file, speculating: old.spec != launch_args::SPEC_VALUES[0] });
    launch.args.extend(added.args);
    launch.applied.extend(added.applied);
    launch.notes.extend(added.notes);
    launch
}

/// What the last launch of the server did with the switches (what was added, what was refused), for [`report`]. `None` before the first launch.
#[derive(Debug, Clone, Default)]
struct LastLaunch {
    applied: Vec<String>,
    notes: Vec<String>,
}

static LAST: Mutex<Option<LastLaunch>> = Mutex::new(None);

/// The server has started: what its switches added and what they could not add. The two lists are the ones of `launch_args` and of [`extra_args`] together.
pub(crate) fn note_launch(applied: &[String], notes: &[String]) {
    draft_model::reset();
    crate::model::native_tools::forget_refusals();
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some(LastLaunch { applied: applied.to_vec(), notes: notes.to_vec() });
}

/// Did the last launch of the server add this (`slot_save`, or `cache_reuse` for any value of it)? False before the first launch.
pub(crate) fn last_launch_applied(what: &str) -> bool {
    LAST.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|l| l.applied.iter().any(|a| a == what || a.strip_prefix(what).is_some_and(|rest| rest.starts_with('='))))
}

/// One switch as [`report`] tells it.
fn view(name: &str, default: &str, value: &str, wanted: bool, refused: bool) -> Value {
    let state = if refused {
        "refused"
    } else if wanted {
        "on"
    } else {
        "off"
    };
    json!({ "name": name, "state": state, "value": value, "default": default })
}

/// The report of the speed-experiment switches, as one value: each switch with its state (`off`, `on`, or `refused` when its value is not on the list), the sentences about what was
/// refused, and what the last launch of the model server did with them. Pure: the caller gives what the environment said.
pub(crate) fn report_from(raw: &RawExtras, old_raw: &launch_args::RawSwitches, running: bool, model_folder: &Path) -> Value {
    let (extras, refusals) = parse_extras(raw);
    let (old, old_warnings) = launch_args::parse_switches(old_raw);
    // The sentences of the four older switches quote the value (`AQUA_LLAMA_FA="loud" is not one of ...`): the status keeps the name and the reason and drops the quote.
    let mut warnings: Vec<String> = refusals.iter().map(Refusal::status_line).collect();
    warnings.extend(old_warnings.iter().map(|w| match (w.split_once('='), w.find(" is not")) {
        (Some((name, _)), Some(at)) => format!("{name}{}", &w[at..]),
        _ => "a value was refused".to_string(),
    }));
    let refused = |name: &str| warnings.iter().any(|w| w.starts_with(&format!("{name} ")));
    let defaults = launch_args::Switches::default();
    let think = old.think_budget.map_or_else(|| "off".to_string(), |n| n.to_string());
    let switches = vec![
        view("AQUA_LLAMA_KV", defaults.kv, old.kv, old.kv != defaults.kv, refused("AQUA_LLAMA_KV")),
        view("AQUA_LLAMA_FA", defaults.fa, old.fa, old.fa != defaults.fa, refused("AQUA_LLAMA_FA")),
        view("AQUA_LLAMA_SPEC", defaults.spec, old.spec, old.spec != defaults.spec, refused("AQUA_LLAMA_SPEC")),
        view("AQUA_LLAMA_THINK_BUDGET", "off", &think, old.think_budget.is_some(), refused("AQUA_LLAMA_THINK_BUDGET")),
        view(flags::LLAMA_CACHE_REUSE.name, CACHE_REUSE_VALUES[0], extras.cache_reuse, extras.cache_reuse != CACHE_REUSE_VALUES[0], refused(flags::LLAMA_CACHE_REUSE.name)),
        view(flags::LLAMA_CACHE_RAM.name, CACHE_RAM_VALUES[0], extras.cache_ram, extras.cache_ram != CACHE_RAM_VALUES[0], refused(flags::LLAMA_CACHE_RAM.name)),
        view(flags::LLAMA_SLOT_SAVE.name, "off", if extras.slot_save { "on" } else { "off" }, extras.slot_save, refused(flags::LLAMA_SLOT_SAVE.name)),
        view(flags::LLAMA_DRAFT.name, "off", extras.draft.map_or("off", |p| if p == Place::Processor { "cpu" } else { "gpu" }), extras.draft.is_some(), refused(flags::LLAMA_DRAFT.name)),
    ];
    let on: Vec<&str> = switches.iter().filter(|s| s["state"] == "on").filter_map(|s| s["name"].as_str()).collect();
    let last = LAST.lock().unwrap_or_else(|e| e.into_inner()).clone();
    json!({
        "about": "What the speed experiments (card OA9-06) change in the command line of the model server. Every switch is off unless it says on; the values are read at the launch of the server only.",
        "switches": switches,
        "draft_model": draft_model::report(raw.draft_model.as_deref(), raw.draft_folder.as_deref(), model_folder, draft_model::counts()),
        "on": on,
        "warnings": warnings,
        "last_launch": {
            "running": running,
            "applied": last.as_ref().map(|l| l.applied.clone()).unwrap_or_default(),
            "notes": last.as_ref().map(|l| l.notes.clone()).unwrap_or_default(),
        },
    })
}

/// Adds one more switch to a report made by [`report_from`]: to the list, and to the ones that are on when it is.
fn add_switch(report: &mut Value, view: Value) {
    if view["state"] == "on" {
        if let (Some(on), Some(name)) = (report["on"].as_array_mut(), view["name"].as_str()) {
            on.push(json!(name));
        }
    }
    if let Some(list) = report["switches"].as_array_mut() {
        list.push(view);
    }
}

/// The report of the switches now: the answer that the status of the models carries (`GET /api/v1/models/roles`, key `speed_experiments`). `model_folder` is the folder of the model files,
/// where a draft model named by its file name is looked for; `agent_model` is the file the agent's planner uses and `profiles` the person's `profiles.json`, for the tool mode.
pub fn report(model_folder: &Path, agent_model: &str, installed: &[String], profiles: &crate::model::profile::Profiles) -> Value {
    let mut v = report_from(&raw_extras(), &launch_args::raw_switches(), super::llama_server::is_running(), model_folder);
    let native = crate::model::native_tools::switch_is_native();
    add_switch(&mut v, json!({ "name": flags::TOOL_MODE.name, "state": if native { "on" } else { "off" }, "value": if native { "native" } else { "json" }, "default": "json" }));
    v["native_tools"] = crate::model::native_tools::report(agent_model, profiles);
    let mut fast = crate::model::fast_tier::report_now(installed, agent_model);
    // Whether it is off, waiting (the second server starts at the first call that needs it), starting, ready or failed, and why (card OA9-06, second round). The small model's file is
    // checked here as the call checks it, so that a file that cannot be used is said so before any call.
    let on = fast["switch"] == "on";
    let unusable = if on {
        fast["why_not"].as_str().map(str::to_string).or_else(|| fast["fast_model"].as_str().and_then(|name| super::fast_server::check_model(model_folder, name, &model_folder.join(agent_model)).err()))
    } else {
        None
    };
    let (state, why) = super::fast_server::state_word(on, unusable.as_deref());
    fast["state"] = json!(state);
    fast["state_why"] = json!(why);
    fast["server"] = super::fast_server::status();
    add_switch(&mut v, json!({ "name": flags::FAST_TIER.name, "state": if on { "on" } else { "off" }, "value": fast["switch"].clone(), "default": "off" }));
    v["fast_tier"] = fast;
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::llm_router::launch_args::{parse_help, ExtraOptions};

    /// The part of `llama-server --help` that holds the options read here (written from the option definitions of `vendor/llama.cpp/common/arg.cpp`, as `--help` prints them: the
    /// first name of each option first, then its other names and its value).
    const HELP_WITH_THE_OPTIONS: &str = r#"
-cram, --cache-ram N                    set the maximum cache size in MiB (default: 8192, -1 - no limit, 0 - disable)
--cache-reuse N                         min chunk size to attempt reusing from the cache via KV shifting, requires prompt caching to be enabled (default: 0)
--slot-save-path PATH                   path to save slot kv cache (default: disabled)
--spec-type none,draft-simple,ngram-simple,ngram-mod
                                        comma-separated list of types of speculative decoding to use (default:
                                        none)
--spec-draft-model, -md, --model-draft FNAME
                                        draft model for speculative decoding (default: unused)
--spec-draft-n-max N                    number of tokens to draft for speculative decoding (default: 16)
--spec-draft-n-min N                    minimum number of draft tokens to use for speculative decoding (default: 0)
--spec-draft-p-min, --draft-p-min P     minimum speculative decoding probability (greedy) (default: 0.75)
--spec-draft-ngl, -ngld, --gpu-layers-draft, --n-gpu-layers-draft N
                                        max. number of draft model layers to store in VRAM (default: all)
"#;

    fn caps() -> Capabilities {
        parse_help(HELP_WITH_THE_OPTIONS)
    }

    fn no_folder() -> Result<PathBuf, String> {
        panic!("the folder of saved slots is asked for only when that switch is on")
    }

    fn raw(reuse: &str, ram: &str, save: &str) -> RawExtras {
        let some = |s: &str| Some(s.to_string());
        RawExtras { cache_reuse: some(reuse), cache_ram: some(ram), slot_save: some(save), ..RawExtras::default() }
    }

    fn no_draft() -> Result<PathBuf, String> {
        panic!("the draft model's file is checked only when that switch is on and the build can use it")
    }

    /// The needs of a launch with a folder that is always there and no draft file.
    fn needs<'a>(slot: &'a dyn Fn() -> Result<PathBuf, String>) -> Needs<'a> {
        Needs { slot_dir: slot, draft_file: &no_draft, speculating: false }
    }

    fn strings(list: &[OsString]) -> Vec<String> {
        list.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn the_options_are_read_from_the_help_of_the_build() {
        assert_eq!(caps().extra, ExtraOptions { cache_reuse: true, cache_ram: true, slot_save_path: true, draft_model: true, draft_tuning: true, draft_layers: true, ..ExtraOptions::default() });
        // A build that lists none of them, and a help that could not be read.
        assert_eq!(parse_help("-fa, --flash-attn [on|off|auto]  set Flash Attention use\n").extra, ExtraOptions::default());
        assert_eq!(parse_help("").extra, ExtraOptions::default());
        // The draft model's own cache types are not the options read here, and neither is a longer name that only starts like one.
        assert!(!parse_help("--cache-reuse-extra N   something else\n").extra.cache_reuse);
    }

    #[test]
    fn with_every_switch_off_nothing_is_added_whatever_the_build_has() {
        for build in [caps(), Capabilities::default(), parse_help("")] {
            let added = extra_args(&Extras::default(), &build, &needs(&no_folder));
            assert_eq!(added, Added::default());
        }
        let (extras, warnings) = parse_extras(&RawExtras::default());
        assert!(extras.is_default() && warnings.is_empty());
        // Blank is not set, and says nothing.
        let (extras, warnings) = parse_extras(&raw("", "  ", "\t"));
        assert!(extras.is_default() && warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_switch_that_is_on_adds_its_option_and_only_that() {
        let (extras, warnings) = parse_extras(&raw("256", "", ""));
        assert!(warnings.is_empty());
        let added = extra_args(&extras, &caps(), &needs(&no_folder));
        assert_eq!(strings(&added.args), ["--cache-reuse", "256"]);
        assert_eq!(added.applied, ["cache_reuse=256"]);
        assert!(added.notes.is_empty());

        let (extras, _) = parse_extras(&raw("", " 2048 ", ""));
        assert_eq!(strings(&extra_args(&extras, &caps(), &needs(&no_folder)).args), ["--cache-ram", "2048"]);
        // Zero is a value of its own (the server keeps no prompt of an idle slot), not "off".
        let (extras, _) = parse_extras(&raw("", "0", ""));
        assert_eq!(strings(&extra_args(&extras, &caps(), &needs(&no_folder)).args), ["--cache-ram", "0"]);

        let folder = std::env::temp_dir().join("aqua_kv_not_made_here");
        let (extras, _) = parse_extras(&raw("", "", "ON"));
        let added = extra_args(&extras, &caps(), &needs(&|| Ok(folder.clone())));
        assert_eq!(added.args, vec![OsString::from("--slot-save-path"), folder.clone().into_os_string()]);
        assert_eq!(added.applied, ["slot_save"]);
        assert!(!added.applied.concat().contains(folder.to_string_lossy().as_ref()), "the log line holds no path");
    }

    #[test]
    fn all_three_together_keep_each_option_with_its_value() {
        let (extras, _) = parse_extras(&raw("64", "1024", "on"));
        let added = extra_args(&extras, &caps(), &needs(&|| Ok(PathBuf::from("kv"))));
        assert_eq!(strings(&added.args), ["--cache-reuse", "64", "--cache-ram", "1024", "--slot-save-path", "kv"]);
        assert_eq!(added.applied, ["cache_reuse=64", "cache_ram=1024", "slot_save"]);
    }

    #[test]
    fn an_option_the_build_does_not_list_is_not_added_and_the_reason_is_kept() {
        let (extras, _) = parse_extras(&raw("128", "512", "on"));
        for build in [Capabilities::default(), parse_help("-fa, --flash-attn [on|off|auto]  set Flash Attention use\n")] {
            let added = extra_args(&extras, &build, &needs(&no_folder));
            assert!(added.args.is_empty() && added.applied.is_empty());
            assert_eq!(added.notes.len(), 3, "{:?}", added.notes);
            assert!(added.notes[0].contains("AQUA_LLAMA_CACHE_REUSE=128") && added.notes[0].contains("--cache-reuse"), "{:?}", added.notes);
            assert!(added.notes[1].contains("--cache-ram") && added.notes[2].contains("--slot-save-path"), "{:?}", added.notes);
        }
        // One option missing does not take the others with it.
        let mut some = caps();
        some.extra.cache_ram = false;
        let added = extra_args(&extras, &some, &needs(&|| Ok(PathBuf::from("kv"))));
        assert_eq!(strings(&added.args), ["--cache-reuse", "128", "--slot-save-path", "kv"]);
        assert_eq!(added.notes.len(), 1);
    }

    #[test]
    fn a_folder_that_cannot_be_made_leaves_the_option_out_and_says_why() {
        let (extras, _) = parse_extras(&raw("", "", "on"));
        let added = extra_args(&extras, &caps(), &needs(&|| Err("the folder could not be made (permission denied)".to_string())));
        assert!(added.args.is_empty() && added.applied.is_empty());
        assert_eq!(added.notes, ["AQUA_LLAMA_SLOT_SAVE was not used: the folder could not be made (permission denied)"]);
    }

    #[test]
    fn a_value_that_is_not_on_the_list_is_refused_with_a_sentence_and_never_reaches_the_command_line() {
        let hostile = ["256 --evil", "--evil", "256; calc", "\"256\"", "256\n-x", "99999999999", "-5", "1.5", "all", "auto", "../../x", "256\0", "q8_0", "yes please"];
        for h in hostile {
            let (extras, warnings) = parse_extras(&raw(h, h, h));
            assert!(extras.is_default(), "{h:?} gave {extras:?}");
            // The two lists refuse it, and the switch refuses it unless it is one of the plain off words.
            assert_eq!(warnings.len(), 3, "{h:?}: {warnings:?}");
            assert!(warnings.iter().all(|w| w.log_line().contains("refused") && w.log_line().chars().all(|c| !c.is_control())), "{warnings:?}");
            assert!(warnings.iter().all(|w| w.status_line().chars().all(|c| !c.is_control())), "the status holds no control character either");
            assert_eq!(extra_args(&extras, &caps(), &needs(&no_folder)), Added::default(), "{h:?}");
        }
        // Real numbers that are not on the list are refused too: the list is the list.
        let (extras, warnings) = parse_extras(&raw("100", "8192", ""));
        assert!(extras.is_default());
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].log_line().starts_with("AQUA_LLAMA_CACHE_REUSE=") && warnings[0].log_line().contains("64, 128, 256, 512"));
        assert!(!warnings[0].status_line().contains("100") && warnings[0].status_line().starts_with("AQUA_LLAMA_CACHE_REUSE is not one of"), "{}", warnings[0].status_line());
        // The off words of a switch are not a mistake.
        for off in ["off", "0", "false", "NO"] {
            let (extras, warnings) = parse_extras(&raw("", "", off));
            assert!(!extras.slot_save && warnings.is_empty(), "{off:?}: {warnings:?}");
        }
        // A long value is cut in the log line.
        let long = "x".repeat(500);
        let (_, warnings) = parse_extras(&raw(&long, "", ""));
        assert!(warnings[0].log_line().len() < 220, "{}", warnings[0].log_line().len());
    }

    #[test]
    fn whatever_the_values_and_the_build_every_argument_is_one_of_a_fixed_set() {
        let mut runs = 0;
        let fixed = ["--cache-reuse", "--cache-ram", "--slot-save-path", "kv"];
        for reuse in CACHE_REUSE_VALUES {
            for ram in CACHE_RAM_VALUES {
                for save in [false, true] {
                    for build in [caps(), Capabilities::default()] {
                        let extras = Extras { cache_reuse: reuse, cache_ram: ram, slot_save: save, draft: None, native_tools: false };
                        let added = extra_args(&extras, &build, &needs(&|| Ok(PathBuf::from("kv"))));
                        runs += 1;
                        for a in strings(&added.args) {
                            assert!(fixed.contains(&a.as_str()) || CACHE_REUSE_VALUES.contains(&a.as_str()) || CACHE_RAM_VALUES.contains(&a.as_str()), "{a:?} from {extras:?}");
                        }
                        if extras.is_default() {
                            assert!(added.args.is_empty());
                        }
                        // Each switch is reported once: added, or not added with a reason.
                        let asked = usize::from(reuse != "off") + usize::from(ram != "off") + usize::from(save);
                        assert_eq!(added.applied.len() + added.notes.len(), asked);
                    }
                }
            }
        }
        assert_eq!(runs, 5 * 6 * 2 * 2);
    }

    #[test]
    fn the_report_says_which_switches_are_on_which_were_refused_and_what_the_last_launch_did() {
        let off = report_from(&RawExtras::default(), &launch_args::RawSwitches::default(), false, Path::new("models"));
        assert_eq!(off["on"], json!([]));
        assert_eq!(off["warnings"], json!([]));
        let states: Vec<&str> = off["switches"].as_array().unwrap().iter().map(|s| s["state"].as_str().unwrap()).collect();
        assert!(states.iter().all(|s| *s == "off"), "{states:?}");
        assert_eq!(off["switches"].as_array().unwrap().len(), 8);

        let old = launch_args::RawSwitches { kv: Some("q8_0".into()), fa: Some("loud".into()), ..Default::default() };
        let on = report_from(&raw("256", "99", "on"), &old, true, Path::new("models"));
        assert_eq!(on["on"], json!(["AQUA_LLAMA_KV", "AQUA_LLAMA_CACHE_REUSE", "AQUA_LLAMA_SLOT_SAVE"]));
        let state = |name: &str| on["switches"].as_array().unwrap().iter().find(|s| s["name"] == name).unwrap()["state"].clone();
        assert_eq!(state("AQUA_LLAMA_FA"), json!("refused"));
        assert_eq!(state("AQUA_LLAMA_CACHE_RAM"), json!("refused"));
        assert_eq!(state("AQUA_LLAMA_CACHE_REUSE"), json!("on"));
        assert_eq!(on["warnings"].as_array().unwrap().len(), 2);
        assert_eq!(on["last_launch"]["running"], json!(true));
        // The values are the entries of the lists, never the text of the environment.
        let text = on.to_string();
        assert!(!text.contains("loud") && !text.contains("\"99\""), "{text}");
    }

    fn draft_raw(text: &str) -> RawExtras {
        RawExtras { draft: Some(text.to_string()), ..RawExtras::default() }
    }

    #[test]
    fn the_draft_switch_means_the_processor_unless_it_says_the_card_and_anything_else_is_refused() {
        for (text, want) in [("cpu", Some(Place::Processor)), ("ON", Some(Place::Processor)), (" 1 ", Some(Place::Processor)), ("true", Some(Place::Processor)), ("yes", Some(Place::Processor)), ("gpu", Some(Place::Card)), ("GPU", Some(Place::Card)), ("off", None), ("0", None), ("no", None), ("", None)] {
            let (extras, warnings) = parse_extras(&draft_raw(text));
            assert_eq!(extras.draft, want, "{text:?}");
            assert!(warnings.is_empty(), "{text:?}: {warnings:?}");
        }
        for text in ["maybe", "cpu --evil", "gpu;calc", "2", "q8_0", "draft-simple"] {
            let (extras, warnings) = parse_extras(&draft_raw(text));
            assert!(extras.is_default(), "{text:?}");
            assert_eq!(warnings.len(), 1, "{text:?}");
            assert!(warnings[0].status_line().starts_with("AQUA_LLAMA_DRAFT is not one of"), "{}", warnings[0].status_line());
            assert!(!warnings[0].status_line().contains(text), "the status does not quote what was typed");
        }
        assert!(Extras { draft: Some(Place::Card), ..Extras::default() } != Extras::default(), "the draft switch is part of what makes a launch the usual one");
    }

    #[test]
    fn native_tools_add_the_servers_own_templates_only_where_the_build_has_them_and_does_not_use_them_already() {
        let native = Extras { native_tools: true, ..Extras::default() };
        let off_by_default = parse_help("--jinja, --no-jinja\n  whether to use jinja template engine for chat (default: disabled)\n");
        assert!(off_by_default.extra.jinja && !off_by_default.extra.jinja_on_by_default);
        let n = needs(&no_folder);
        let added = extra_args(&native, &off_by_default, &n);
        assert_eq!(strings(&added.args), ["--jinja"], "a build whose templates are off gets them");
        assert_eq!(added.applied, ["native_tools (--jinja)"]);
        // A build whose templates are on already is left as it is (the argument would change nothing), and says so.
        let on = parse_help("--jinja, --no-jinja   whether to use jinja template engine for chat (default: enabled)\n");
        assert!(on.extra.jinja && on.extra.jinja_on_by_default);
        let added = extra_args(&native, &on, &n);
        assert!(added.args.is_empty());
        assert_eq!(added.applied, ["native_tools (the server's templates are on in this build)"]);
        // A build without the option: nothing is added, and the note says the request may be refused and what happens then.
        let added = extra_args(&native, &Capabilities::default(), &n);
        assert!(added.args.is_empty() && added.applied.is_empty());
        assert!(added.notes[0].contains("does not list --jinja") && added.notes[0].contains("JSON way"), "{:?}", added.notes);
        // Not native: nothing, whatever the build lists.
        assert_eq!(extra_args(&Extras::default(), &on, &n), Added::default());
        assert!(!Extras { native_tools: true, ..Extras::default() }.is_default(), "the native mode is part of what makes a launch the usual one");
    }

    #[test]
    fn the_report_has_the_fast_tier_with_what_is_not_built_and_names_it_a_switch_only_when_it_is_on() {
        let profiles = crate::model::profile::Profiles::default();
        let installed = vec!["Qwen3-8B-Q4_K_M.gguf".to_string(), "Qwen3.5-4B-Q4_K_M.gguf".to_string()];
        crate::flags::with_overrides(&[("AQUA_FAST_TIER", None), ("AQUA_PLANNER_TOOL_MODE", None)], || {
            let r = report(Path::new("models"), "Qwen3-8B-Q4_K_M.gguf", &installed, &profiles);
            assert_eq!(r["fast_tier"]["switch"], "off");
            assert_eq!(r["fast_tier"]["state"], "off");
            assert!(r["fast_tier"]["state_why"].is_null());
            assert_eq!(r["fast_tier"]["built"]["the_launch_of_a_second_server"], true);
            assert!(!r["on"].as_array().unwrap().iter().any(|n| n == "AQUA_FAST_TIER"));
        });
        crate::flags::with_overrides(&[("AQUA_FAST_TIER", Some("on")), ("AQUA_PLANNER_TOOL_MODE", None)], || {
            let r = report(Path::new("models"), "Qwen3-8B-Q4_K_M.gguf", &installed, &profiles);
            assert_eq!(r["fast_tier"]["switch"], "on");
            assert!(r["on"].as_array().unwrap().iter().any(|n| n == "AQUA_FAST_TIER"));
            assert!(r["fast_tier"]["why_not"].as_str().unwrap().contains("does not name a file") || std::env::var("AQUA_FAST_TIER_MODEL").is_ok(), "{}", r["fast_tier"]);
            // On, but no usable small model: the state says so and why, and the server's own counts are there.
            assert_eq!(r["fast_tier"]["state"], "not_used");
            assert!(r["fast_tier"]["state_why"].is_string());
            assert!(r["fast_tier"]["server"]["phase"].is_string());
        });
    }

    #[test]
    fn the_report_has_the_tool_mode_among_the_switches_and_its_counts() {
        let profiles = crate::model::profile::Profiles::default();
        crate::flags::with_overrides(&[("AQUA_PLANNER_TOOL_MODE", None), ("AQUA_LLAMA_CACHE_REUSE", None), ("AQUA_LLAMA_CACHE_RAM", None), ("AQUA_LLAMA_SLOT_SAVE", None), ("AQUA_LLAMA_DRAFT", None)], || {
            let r = report(Path::new("models"), "Qwen3-8B-Q4_K_M.gguf", &[], &profiles);
            let find = |v: &Value| v["switches"].as_array().unwrap().iter().find(|s| s["name"] == "AQUA_PLANNER_TOOL_MODE").cloned();
            assert_eq!(find(&r).unwrap()["state"], "off");
            assert_eq!(r["native_tools"]["switch"], "json");
            assert_eq!(r["native_tools"]["applies_to"], "the agent's planner only");
        });
        crate::flags::with_overrides(&[("AQUA_PLANNER_TOOL_MODE", Some("native"))], || {
            let r = report(Path::new("models"), "Qwen3-8B-Q4_K_M.gguf", &[], &profiles);
            assert!(r["on"].as_array().unwrap().iter().any(|n| n == "AQUA_PLANNER_TOOL_MODE"), "{}", r["on"]);
            assert_eq!(r["native_tools"]["agent_model_mode"], "native");
        });
    }

    fn with_draft(place: Place) -> Extras {
        Extras { draft: Some(place), ..Extras::default() }
    }

    #[test]
    fn a_draft_model_that_passed_its_checks_adds_the_draft_and_its_tuning_and_where_it_runs() {
        let file = PathBuf::from("C:/m/Qwen3-0.6B-Q8_0.gguf");
        let found = || Ok(file.clone());
        let n = Needs { slot_dir: &no_folder, draft_file: &found, speculating: false };
        let added = extra_args(&with_draft(Place::Processor), &caps(), &n);
        assert_eq!(
            strings(&added.args),
            ["--spec-draft-model", "C:/m/Qwen3-0.6B-Q8_0.gguf", "--spec-type", "draft-simple", "--spec-draft-n-max", "5", "--spec-draft-n-min", "1", "--spec-draft-p-min", "0.5", "--spec-draft-ngl", "0"]
        );
        assert_eq!(added.applied, ["draft=Qwen3-0.6B-Q8_0.gguf (processor)"]);
        assert!(added.notes.is_empty());
        let card = extra_args(&with_draft(Place::Card), &caps(), &n);
        assert_eq!(strings(&card.args).last().unwrap(), "99");
        // With the others: the cache options first, the draft after them.
        let all = Extras { cache_reuse: "256", draft: Some(Place::Processor), ..Extras::default() };
        let args = strings(&extra_args(&all, &caps(), &n).args);
        assert_eq!(&args[..2], ["--cache-reuse", "256"]);
        assert_eq!(args[2], "--spec-draft-model");
    }

    #[test]
    fn a_draft_that_cannot_be_used_leaves_the_launch_as_it_was_and_says_why() {
        let boom = || -> Result<PathBuf, String> { panic!("the file is not looked at when the build or the other switch rules the draft out") };
        // The n-gram speculation is on: the two are separate experiments.
        let n = Needs { slot_dir: &no_folder, draft_file: &boom, speculating: true };
        let added = extra_args(&with_draft(Place::Processor), &caps(), &n);
        assert!(added.args.is_empty() && added.applied.is_empty());
        assert!(added.notes[0].contains("AQUA_LLAMA_SPEC is on") && added.notes[0].starts_with("AQUA_LLAMA_DRAFT was not used"), "{:?}", added.notes);
        // A build without the draft options.
        let n = Needs { slot_dir: &no_folder, draft_file: &boom, speculating: false };
        for build in [Capabilities::default(), parse_help("-fa, --flash-attn [on|off|auto]  x
")] {
            let added = extra_args(&with_draft(Place::Card), &build, &n);
            assert!(added.args.is_empty() && added.notes[0].contains("does not list --spec-draft-model"), "{:?}", added.notes);
        }
        // The file does not pass: the reason is the note, and nothing is added.
        let missing = || Err("the draft model's file was not found".to_string());
        let n = Needs { slot_dir: &no_folder, draft_file: &missing, speculating: false };
        let added = extra_args(&with_draft(Place::Processor), &caps(), &n);
        assert!(added.args.is_empty() && added.applied.is_empty());
        assert_eq!(added.notes, ["AQUA_LLAMA_DRAFT was not used: the draft model's file was not found"]);
        // A draft that cannot be kept off the card is not used on a build that has no --spec-draft-ngl.
        let mut no_layers = caps();
        no_layers.extra.draft_layers = false;
        let found = || Ok(PathBuf::from("d.gguf"));
        let n = Needs { slot_dir: &no_folder, draft_file: &found, speculating: false };
        assert!(extra_args(&with_draft(Place::Processor), &no_layers, &n).notes[0].contains("kept off the graphics card"));
    }

    #[test]
    fn the_report_says_whether_the_draft_model_is_named_usable_or_refused_and_never_gives_its_path() {
        let dir = std::env::temp_dir().join("aqua_draft_report_never_made");
        let none = report_from(&RawExtras::default(), &launch_args::RawSwitches::default(), false, &dir);
        assert_eq!(none["draft_model"]["model_setting"], "not set");
        let raw = RawExtras { draft: Some("cpu".into()), draft_model: Some("C:\\secrets\\Missing.gguf".into()), ..RawExtras::default() };
        let r = report_from(&raw, &launch_args::RawSwitches::default(), false, &dir);
        assert_eq!(r["draft_model"]["model_setting"], "refused");
        assert!(r["draft_model"]["problem"].as_str().unwrap().contains("not inside the model folder") || r["draft_model"]["problem"].as_str().unwrap().contains("not found"), "{r}");
        assert!(!r.to_string().contains("secrets"), "no path in the report: {r}");
        assert_eq!(r["on"], json!(["AQUA_LLAMA_DRAFT"]));
        let state = r["switches"].as_array().unwrap().iter().find(|s| s["name"] == "AQUA_LLAMA_DRAFT").unwrap().clone();
        assert_eq!((state["state"].clone(), state["value"].clone()), (json!("on"), json!("cpu")));
    }
}
