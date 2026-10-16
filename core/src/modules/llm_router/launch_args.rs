//! The command line of the model server, and the four switches that can change it for a speed experiment (card R3-01: experiments E3, E6 and E7).
//!
//! [`launch_args`] is a pure function: the same values give the same list, so a test can pin the exact arguments without starting anything. With no switch on, the list is what the
//! launch always passed, byte for byte. The switches are read from the environment at launch only (a running server is never changed), each against a short list of allowed values (an
//! environment value is never passed on as text), and each only if THIS build of `llama-server` has the option, which is read from `llama-server --help` (that loads no model) and only
//! when a switch is on.
//!
//! Read from the two builds in `data/engines/llama-gpu` and `data/engines/llama` on 2026-10-08 (both print `version: 0.5.0-dev (build 11371, commit 99b95488c)`):
//! * `-ctk` and `-ctv` take f32, f16, bf16, q8_0, q4_0, q4_1, iq4_nl, q5_0 or q5_1 (default f16).
//! * `-fa` takes a value, `[on|off|auto]` (default auto). A quantised V cache needs flash attention: that is llama.cpp's rule as reported by others, the help text does not say it and it
//!   was not run here, so a quantised cache is started with `-fa on`.
//! * `--spec-type` takes none, draft-simple, draft-eagle3, draft-mtp, draft-dflash, draft-dspark, ngram-simple, ngram-map-k, ngram-map-k4v, ngram-mod or ngram-cache (default none).
//!   The older `--draft-max`, `--draft-min` and `--spec-ngram-size-*` print "the argument has been removed". `AQUA_LLAMA_SPEC=ngram` is `ngram-mod` (the type the card names first),
//!   else `ngram-simple`, else `ngram-map-k`, whichever the build lists first in that order.
//! * `--reasoning-budget N` (-1 unrestricted, 0 ends thinking at once, N tokens; default -1) and `--reasoning [on|off|auto]`. Aqua starts the server with `--reasoning off` and each request
//!   says `enable_thinking: false`, so a budget changes nothing while thinking is off: a budget above 0 starts the server with `--reasoning on` and lets the requests think
//!   (`llama_server::request_body_with`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// What `AQUA_LLAMA_KV` may be: the cache type for K and V. The first is the default and passes no option.
pub(crate) const KV_VALUES: [&str; 3] = ["f16", "q8_0", "q4_0"];
/// What `AQUA_LLAMA_FA` may be. `auto` is the default and passes no option, as before.
pub(crate) const FA_VALUES: [&str; 3] = ["auto", "on", "off"];
/// What `AQUA_LLAMA_SPEC` may be. `off` is the default; `ngram` is the first of [`NGRAM_ORDER`] that the build has; the others name a type.
pub(crate) const SPEC_VALUES: [&str; 5] = ["off", "ngram", "ngram-simple", "ngram-map-k", "ngram-mod"];
/// The most tokens `AQUA_LLAMA_THINK_BUDGET` may name.
pub(crate) const MAX_THINK_BUDGET: u32 = 4096;
/// The n-gram types `AQUA_LLAMA_SPEC=ngram` looks for, in this order.
const NGRAM_ORDER: [&str; 3] = ["ngram-mod", "ngram-simple", "ngram-map-k"];

/// The text of the four variables, as the environment has them (`None`: not set).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RawSwitches {
    pub kv: Option<String>,
    pub fa: Option<String>,
    pub spec: Option<String>,
    pub think_budget: Option<String>,
}

/// The four variables of the environment.
pub(crate) fn raw_switches() -> RawSwitches {
    RawSwitches {
        kv: std::env::var("AQUA_LLAMA_KV").ok(),
        fa: std::env::var("AQUA_LLAMA_FA").ok(),
        spec: std::env::var("AQUA_LLAMA_SPEC").ok(),
        think_budget: std::env::var("AQUA_LLAMA_THINK_BUDGET").ok(),
    }
}

/// What the four variables ask for, each one of its allowed values (the text of the environment is never kept, only the entry of the list it matched).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Switches {
    pub kv: &'static str,
    pub fa: &'static str,
    pub spec: &'static str,
    pub think_budget: Option<u32>,
}

impl Default for Switches {
    fn default() -> Self {
        Switches { kv: KV_VALUES[0], fa: FA_VALUES[0], spec: SPEC_VALUES[0], think_budget: None }
    }
}

impl Switches {
    /// Is every switch off (the usual launch)?
    pub(crate) fn is_default(&self) -> bool {
        *self == Switches::default()
    }
}

/// A value from the environment for a log line: cut short (the caller prints it with `{:?}`, which escapes what is not text).
fn shown(text: &str) -> String {
    text.chars().take(24).collect()
}

/// The entry of `list` that `raw` names (any case, spaces around ignored). A value that is not on the list is ignored with a warning; nothing set, or blank, is the default without one.
fn allowed(name: &str, raw: &Option<String>, list: &[&'static str], warnings: &mut Vec<String>) -> Option<&'static str> {
    let text = raw.as_deref()?.trim();
    if text.is_empty() {
        return None;
    }
    let found = list.iter().find(|v| v.eq_ignore_ascii_case(text)).copied();
    if found.is_none() {
        warnings.push(format!("{name}={:?} is not one of {}; it is ignored", shown(text), list.join(", ")));
    }
    found
}

/// The switches the variables ask for, and a warning for each value that is not allowed (the default is used for it).
pub(crate) fn parse_switches(raw: &RawSwitches) -> (Switches, Vec<String>) {
    let mut warnings = Vec::new();
    let kv = allowed("AQUA_LLAMA_KV", &raw.kv, &KV_VALUES, &mut warnings).unwrap_or(KV_VALUES[0]);
    let fa = allowed("AQUA_LLAMA_FA", &raw.fa, &FA_VALUES, &mut warnings).unwrap_or(FA_VALUES[0]);
    let spec = allowed("AQUA_LLAMA_SPEC", &raw.spec, &SPEC_VALUES, &mut warnings).unwrap_or(SPEC_VALUES[0]);
    let mut think_budget = None;
    if let Some(text) = raw.think_budget.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        match text.parse::<u32>() {
            Ok(n) if n <= MAX_THINK_BUDGET => think_budget = Some(n),
            _ => warnings.push(format!("AQUA_LLAMA_THINK_BUDGET={:?} is not a number of tokens from 0 to {MAX_THINK_BUDGET}; it is ignored", shown(text))),
        }
    }
    (Switches { kv, fa, spec, think_budget }, warnings)
}

/// How a build takes flash attention.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum FlashAttn {
    /// No `-fa` option (or the help could not be read).
    #[default]
    Absent,
    /// `-fa` alone turns it on (older builds).
    Switch,
    /// `-fa on`, `-fa off` or `-fa auto` (this build).
    Value,
}

/// What one build of `llama-server` has, from its `--help`. Empty (the default) when the help could not be read: then no switch can do anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Capabilities {
    /// The values both `-ctk` and `-ctv` take.
    pub cache_types: Vec<String>,
    pub flash_attn: FlashAttn,
    /// The values `--spec-type` takes.
    pub spec_types: Vec<String>,
    /// Is there a `--reasoning-budget`?
    pub reasoning_budget: bool,
    /// The options of the speed experiments of card OA9-06 (`server_options.rs`): which of them this build lists. Read from the same help text; all false when it could not be read.
    pub extra: ExtraOptions,
}

/// Which of the options that card OA9-06 may add to the command line a build of `llama-server` lists in its `--help` (the names are those of the vendored source, `common/arg.cpp`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ExtraOptions {
    /// `--cache-reuse N`.
    pub cache_reuse: bool,
    /// `--cache-ram N` (`-cram`).
    pub cache_ram: bool,
    /// `--slot-save-path PATH`.
    pub slot_save_path: bool,
    /// `--spec-draft-model FNAME` (`-md`, `--model-draft`).
    pub draft_model: bool,
    /// `--spec-draft-n-max N`, `--spec-draft-n-min N` and `--spec-draft-p-min P`: all three.
    pub draft_tuning: bool,
    /// `--spec-draft-ngl N` (`-ngld`).
    pub draft_layers: bool,
    /// `--jinja` (and `--no-jinja`).
    pub jinja: bool,
    /// The build's help says its templates are on by default (`default: enabled`), so `--jinja` changes nothing.
    pub jinja_on_by_default: bool,
}

/// One option of the help text: the flags of its first line, the rest of that line, and the whole text of the option (the first line's rest and the indented lines after it).
struct Block {
    flags: Vec<String>,
    rest: String,
    text: String,
}

fn blocks(help: &str) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    for line in help.lines() {
        if line.starts_with("---") {
            // A heading such as "----- sampling params -----" ends the option before it.
            out.push(Block { flags: Vec::new(), rest: String::new(), text: String::new() });
        } else if line.starts_with('-') {
            let mut flags = Vec::new();
            let mut rest = line.trim_start();
            while let Some(word) = rest.split_whitespace().next().filter(|w| w.starts_with('-')) {
                flags.push(word.trim_end_matches(',').to_string());
                rest = rest[word.len()..].trim_start();
            }
            let rest = rest.trim_end().to_string();
            out.push(Block { flags, text: rest.clone(), rest });
        } else if line.starts_with(char::is_whitespace) {
            if let Some(block) = out.last_mut() {
                block.text.push('\n');
                block.text.push_str(line.trim());
            }
        }
    }
    out
}

/// What the help text of a build says it has. A pure function of the text.
pub(crate) fn parse_help(help: &str) -> Capabilities {
    let blocks = blocks(help);
    let find = |flag: &str| blocks.iter().find(|b| b.flags.iter().any(|f| f == flag));
    // "allowed values: f32, f16, ..." on a line of the option's text.
    let values_of = |flag: &str| -> Vec<String> {
        find(flag)
            .and_then(|b| b.text.split_once("allowed values:"))
            .map(|(_, tail)| tail.lines().next().unwrap_or("").split(',').map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect())
            .unwrap_or_default()
    };
    let (k, v) = (values_of("--cache-type-k"), values_of("--cache-type-v"));
    let cache_types = k.into_iter().filter(|t| v.contains(t)).collect();
    let flash_attn = match find("--flash-attn") {
        None => FlashAttn::Absent,
        Some(b) if b.rest.contains("on|off") => FlashAttn::Value,
        Some(_) => FlashAttn::Switch,
    };
    // "--spec-type none,draft-simple,...,ngram-mod": the list is the word after the flag.
    let spec_types = find("--spec-type")
        .map(|b| b.rest.split_whitespace().next().unwrap_or("").split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect())
        .unwrap_or_default();
    let has = |flag: &str| find(flag).is_some();
    let extra = ExtraOptions {
        cache_reuse: has("--cache-reuse"),
        cache_ram: has("--cache-ram"),
        slot_save_path: has("--slot-save-path"),
        draft_model: has("--spec-draft-model"),
        draft_tuning: has("--spec-draft-n-max") && has("--spec-draft-n-min") && has("--spec-draft-p-min"),
        draft_layers: has("--spec-draft-ngl"),
        jinja: has("--jinja"),
        jinja_on_by_default: find("--jinja").is_some_and(|b| b.text.contains("default: enabled")),
    };
    Capabilities { cache_types, flash_attn, spec_types, reasoning_budget: has("--reasoning-budget"), extra }
}

type HelpKey = (PathBuf, u64, Option<SystemTime>);

/// What was read from each build's help so far (a build is run with `--help` once; it is told apart by its path, size and time).
static HELP_READ: Mutex<Vec<(HelpKey, Capabilities)>> = Mutex::new(Vec::new());

/// What this build has: its `--help` is run (no model is loaded; 20 seconds at the most) the first time and the answer is kept. An empty answer (the build did not start or did not
/// answer) is not kept, so a later launch asks again.
pub(crate) fn capabilities_of(binary: &Path) -> Capabilities {
    let meta = std::fs::metadata(binary).ok();
    let key: HelpKey = (binary.to_path_buf(), meta.as_ref().map_or(0, |m| m.len()), meta.and_then(|m| m.modified().ok()));
    if let Some((_, caps)) = HELP_READ.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(k, _)| *k == key) {
        return caps.clone();
    }
    let caps = parse_help(&super::gpu::text_of(binary, "--help", 20));
    if caps != Capabilities::default() {
        HELP_READ.lock().unwrap_or_else(|e| e.into_inner()).push((key, caps.clone()));
    }
    caps
}

/// The plain values of a launch.
pub(crate) struct Values<'a> {
    pub model: &'a Path,
    pub port: u16,
    /// `-c`: the whole window, which the server divides between the slots.
    pub window: u32,
    pub threads: &'a str,
    pub threads_batch: &'a str,
    pub slots: u32,
    pub log_file: &'a Path,
    /// `-ngl`; `None` passes no option.
    pub layers: Option<&'a str>,
    /// The card: its id for `-dev` and the options that go with it (`--fit-target`); `None` for the processor.
    pub card: Option<(&'a str, &'a [String])>,
}

/// The arguments of a launch, and what the switches did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Launch {
    pub args: Vec<OsString>,
    /// What a switch changed, as the log says it ("kv=q8_0", "fa=on").
    pub applied: Vec<String>,
    /// Switches that were asked for and could not be applied, and why.
    pub notes: Vec<String>,
    /// Does the server let the model think (a budget above 0)? Requests then must not say that thinking is off.
    pub thinking: bool,
}

struct Experiment {
    args: Vec<String>,
    applied: Vec<String>,
    notes: Vec<String>,
    thinking: bool,
}

/// What the switches add to the command line (it is empty with every switch off), given what the build has.
fn experiment(switches: &Switches, caps: &Capabilities) -> Experiment {
    let mut x = Experiment { args: Vec::new(), applied: Vec::new(), notes: Vec::new(), thinking: false };
    let mut fa = switches.fa;
    // The cache: a type the build lacks, a build without flash attention and a request to turn flash attention off each leave the cache at f16, because the server would not start.
    let kv = switches.kv;
    if kv != KV_VALUES[0] {
        let problem = if !caps.cache_types.iter().any(|t| t == kv) {
            Some(format!("that cache type is not available in this build (it offers: {})", caps.cache_types.join(", ")))
        } else if caps.flash_attn == FlashAttn::Absent {
            Some("a quantised cache needs flash attention, which is not available in this build".to_string())
        } else if fa == "off" {
            Some("a quantised cache needs flash attention, which AQUA_LLAMA_FA=off turns off".to_string())
        } else {
            None
        };
        match problem {
            Some(why) => x.notes.push(format!("AQUA_LLAMA_KV={kv} was not used: {why}; the cache stays {}", KV_VALUES[0])),
            None => {
                x.args.extend(["-ctk", kv, "-ctv", kv].map(String::from));
                x.applied.push(format!("kv={kv}"));
                fa = "on";
            }
        }
    }
    if fa != FA_VALUES[0] {
        match caps.flash_attn {
            FlashAttn::Value => {
                x.args.extend(["-fa", fa].map(String::from));
                x.applied.push(format!("fa={fa}"));
            }
            FlashAttn::Switch if fa == "on" => {
                x.args.push("-fa".to_string());
                x.applied.push("fa=on".to_string());
            }
            // A build whose `-fa` is a plain switch has it off unless asked.
            FlashAttn::Switch => {}
            FlashAttn::Absent => x.notes.push(format!("AQUA_LLAMA_FA={fa} is not available in this build")),
        }
    }
    if switches.spec != SPEC_VALUES[0] {
        let has = |t: &str| caps.spec_types.iter().any(|s| s == t);
        let found = if switches.spec == "ngram" { NGRAM_ORDER.iter().copied().find(|t| has(*t)) } else { Some(switches.spec).filter(|t| has(*t)) };
        match found {
            Some(t) => {
                x.args.extend(["--spec-type", t].map(String::from));
                x.applied.push(format!("spec={t}"));
            }
            None => x.notes.push(format!("AQUA_LLAMA_SPEC={} is not available in this build", switches.spec)),
        }
    }
    if let Some(n) = switches.think_budget {
        if caps.reasoning_budget {
            x.args.extend(["--reasoning-budget".to_string(), n.to_string()]);
            x.applied.push(format!("think_budget={n}"));
            x.thinking = n > 0;
        } else {
            x.notes.push("AQUA_LLAMA_THINK_BUDGET is not available in this build".to_string());
        }
    }
    x
}

fn os(list: &[&str]) -> Vec<OsString> {
    list.iter().map(|s| OsString::from(*s)).collect()
}

/// The command line of the model server (after its program name). With every switch off this is what the launch always passed.
pub(crate) fn launch_args(v: &Values, switches: &Switches, caps: &Capabilities) -> Launch {
    let x = experiment(switches, caps);
    let mut args = vec![OsString::from("-m"), v.model.as_os_str().to_owned()];
    args.extend(os(&["--host", "127.0.0.1", "--port", &v.port.to_string(), "-c", &v.window.to_string(), "-t", v.threads, "-tb", v.threads_batch]));
    args.extend(os(&["--reasoning", if x.thinking { "on" } else { "off" }, "--no-webui", "--parallel", &v.slots.to_string(), "-lv", "1", "--log-file"]));
    args.push(v.log_file.as_os_str().to_owned());
    if let Some(layers) = v.layers {
        args.extend(os(&["-ngl", layers]));
    }
    if let Some((id, flags)) = v.card {
        args.extend(os(&["-dev", id]));
        args.extend(flags.iter().map(OsString::from));
    }
    args.extend(x.args.iter().map(OsString::from));
    Launch { args, applied: x.applied, notes: x.notes, thinking: x.thinking }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The part of `llama-server --help` (build 11371, `data/engines/llama-gpu`, 2026-10-08) that holds the options read here, with the draft-model lookalikes that must not be taken for them.
    const REAL_HELP: &str = r#"
-fa,   --flash-attn [on|off|auto]       set Flash Attention use ('on', 'off', or 'auto', default: 'auto')
                                        (env: LLAMA_ARG_FLASH_ATTN)
-ctk,  --cache-type-k TYPE              KV cache data type for K
                                        allowed values: f32, f16, bf16, q8_0, q4_0, q4_1, iq4_nl, q5_0, q5_1
                                        (default: f16)
                                        (env: LLAMA_ARG_CACHE_TYPE_K)
-ctv,  --cache-type-v TYPE              KV cache data type for V
                                        allowed values: f32, f16, bf16, q8_0, q4_0, q4_1, iq4_nl, q5_0, q5_1
                                        (default: f16)
                                        (env: LLAMA_ARG_CACHE_TYPE_V)
--spec-draft-type-k, -ctkd, --cache-type-k-draft TYPE
                                        KV cache data type for K for the draft model
                                        allowed values: f32, f16
                                        (default: f16)
--spec-type none,draft-simple,draft-eagle3,draft-mtp,draft-dflash,draft-dspark,ngram-simple,ngram-map-k,ngram-map-k4v,ngram-mod,ngram-cache
                                        comma-separated list of types of speculative decoding to use (default:
                                        none)

                                        (env: LLAMA_ARG_SPEC_TYPE)
--draft, --draft-n, --draft-max N       the argument has been removed. use --spec-draft-n-max or
                                        --spec-ngram-mod-n-max
                                        (env: LLAMA_ARG_DRAFT_MAX)

----- example-specific params -----
--reasoning-format FORMAT               controls whether thought tags are allowed and/or extracted from the
                                        response, and in which format they're returned; one of:
                                        - none: leaves thoughts unparsed in `message.content`
                                        (default: auto)
-rea,  --reasoning [on|off|auto]        Use reasoning/thinking in the chat ('on', 'off', or 'auto', default:
                                        'auto' (detect from template))
--reasoning-budget N                    token budget for thinking: -1 for unrestricted, 0 for immediate end,
                                        N>0 for token budget (default: -1)
                                        (env: LLAMA_ARG_THINK_BUDGET)
--reasoning-budget-message MESSAGE      message injected before the end-of-thinking tag when reasoning budget
                                        is exhausted (default: none)
"#;

    fn real_caps() -> Capabilities {
        parse_help(REAL_HELP)
    }

    /// An older build as this parser meets it: `-fa` is a plain switch, and there is no `--spec-type` and no `--reasoning-budget`. (Made up from the older help texts; not a measurement.)
    const OLD_HELP: &str = "-fa,   --flash-attn                     enable Flash Attention (default: disabled)\n-ctk,  --cache-type-k TYPE              KV cache data type for K\n                                        allowed values: f32, f16, q8_0\n-ctv,  --cache-type-v TYPE              KV cache data type for V\n                                        allowed values: f32, f16, q8_0, q4_0\n";

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// The arguments of a launch like today's on the card: the 4 GB card with 22 layers.
    fn launch_with(switches: &Switches, caps: &Capabilities) -> Launch {
        let flags = names(&["--fit-target", "1660"]);
        let values = Values {
            model: Path::new("m.gguf"),
            port: 8123,
            window: 6144,
            threads: "6",
            threads_batch: "6",
            slots: 1,
            log_file: Path::new("l.log"),
            layers: Some("22"),
            card: Some(("Vulkan1", flags.as_slice())),
        };
        launch_args(&values, switches, caps)
    }

    fn strings(launch: &Launch) -> Vec<String> {
        launch.args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    /// What the launch passed before this card, argument by argument (`launch` in `llama_server.rs`: `-m`, the address and port, the window, the threads, `--reasoning off`, the
    /// slots, the log, then `-ngl`, then `-dev` and the card's options).
    fn base() -> Vec<String> {
        names(&[
            "-m", "m.gguf", "--host", "127.0.0.1", "--port", "8123", "-c", "6144", "-t", "6", "-tb", "6", "--reasoning", "off", "--no-webui", "--parallel", "1", "-lv", "1", "--log-file", "l.log", "-ngl",
            "22", "-dev", "Vulkan1", "--fit-target", "1660",
        ])
    }

    fn with(base: Vec<String>, tail: &[&str]) -> Vec<String> {
        base.into_iter().chain(tail.iter().map(|s| s.to_string())).collect()
    }

    fn switches(kv: &'static str, fa: &'static str, spec: &'static str, think_budget: Option<u32>) -> Switches {
        Switches { kv, fa, spec, think_budget }
    }

    #[test]
    fn with_every_switch_off_the_arguments_are_exactly_what_the_launch_always_passed() {
        for caps in [real_caps(), Capabilities::default(), parse_help(OLD_HELP)] {
            let launch = launch_with(&Switches::default(), &caps);
            assert_eq!(strings(&launch), base(), "whatever the build has, the usual launch does not change");
            assert!(launch.applied.is_empty() && launch.notes.is_empty() && !launch.thinking);
        }
        // The processor: no card, and the layers `0` that the launch gives it.
        let values = Values { model: Path::new("m.gguf"), port: 8123, window: 2048, threads: "4", threads_batch: "5", slots: 2, log_file: Path::new("l.log"), layers: Some("0"), card: None };
        let on_the_processor = launch_args(&values, &Switches::default(), &Capabilities::default());
        assert_eq!(
            strings(&on_the_processor),
            names(&["-m", "m.gguf", "--host", "127.0.0.1", "--port", "8123", "-c", "2048", "-t", "4", "-tb", "5", "--reasoning", "off", "--no-webui", "--parallel", "2", "-lv", "1", "--log-file", "l.log", "-ngl", "0"])
        );
        // No `-ngl` when none is given.
        let no_layers = launch_args(&Values { layers: None, ..values }, &Switches::default(), &Capabilities::default());
        assert!(!strings(&no_layers).contains(&"-ngl".to_string()));
    }

    #[test]
    fn the_help_of_the_real_build_is_read_for_what_it_has() {
        let caps = real_caps();
        assert_eq!(caps.cache_types, names(&["f32", "f16", "bf16", "q8_0", "q4_0", "q4_1", "iq4_nl", "q5_0", "q5_1"]), "the draft model's cache types are not these");
        assert_eq!(caps.flash_attn, FlashAttn::Value);
        assert_eq!(
            caps.spec_types,
            names(&["none", "draft-simple", "draft-eagle3", "draft-mtp", "draft-dflash", "draft-dspark", "ngram-simple", "ngram-map-k", "ngram-map-k4v", "ngram-mod", "ngram-cache"])
        );
        assert!(caps.reasoning_budget, "--reasoning-budget is there; --reasoning-budget-message alone would not be it");
        // Nothing, or text that is not a help, is a build that has nothing.
        assert_eq!(parse_help(""), Capabilities::default());
        assert_eq!(parse_help("error: unknown option\nusage: nothing here\n"), Capabilities::default());
        // An option that only the message variant has is not the budget.
        assert!(!parse_help("--reasoning-budget-message MESSAGE      message injected\n").reasoning_budget);
        // An older build: `-fa` takes no value, and the two options are not there.
        let old = parse_help(OLD_HELP);
        assert_eq!(old.flash_attn, FlashAttn::Switch);
        assert_eq!(old.cache_types, names(&["f32", "f16", "q8_0"]), "only what both -ctk and -ctv take");
        assert!(old.spec_types.is_empty() && !old.reasoning_budget);
    }

    #[test]
    fn a_build_that_does_not_start_has_nothing_and_is_asked_again_next_time() {
        let missing = Path::new("C:/definitely/not/llama-server.exe");
        assert_eq!(capabilities_of(missing), Capabilities::default());
        assert!(!HELP_READ.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|((p, _, _), _)| p == missing), "an empty answer is not kept");
    }

    #[test]
    fn the_cache_type_switch_quantises_k_and_v_and_turns_flash_attention_on() {
        for kv in ["q8_0", "q4_0"] {
            let launch = launch_with(&switches(kv, "auto", "off", None), &real_caps());
            assert_eq!(strings(&launch), with(base(), &["-ctk", kv, "-ctv", kv, "-fa", "on"]));
            assert_eq!(launch.applied, vec![format!("kv={kv}"), "fa=on".to_string()]);
            assert!(launch.notes.is_empty());
        }
        // Asking for flash attention as well does not pass it twice.
        let both = strings(&launch_with(&switches("q8_0", "on", "off", None), &real_caps()));
        assert_eq!(both, with(base(), &["-ctk", "q8_0", "-ctv", "q8_0", "-fa", "on"]));
        // f16 is the default and passes nothing.
        assert_eq!(strings(&launch_with(&switches("f16", "auto", "off", None), &real_caps())), base());
    }

    #[test]
    fn the_flash_attention_switch_passes_its_value_and_auto_passes_nothing() {
        assert_eq!(strings(&launch_with(&switches("f16", "on", "off", None), &real_caps())), with(base(), &["-fa", "on"]));
        assert_eq!(strings(&launch_with(&switches("f16", "off", "off", None), &real_caps())), with(base(), &["-fa", "off"]));
        assert_eq!(strings(&launch_with(&switches("f16", "auto", "off", None), &real_caps())), base());
        // An older build whose `-fa` is a plain switch: on is `-fa`, off is nothing. A build without the option: nothing, and it says so.
        assert_eq!(strings(&launch_with(&switches("f16", "on", "off", None), &parse_help(OLD_HELP))), with(base(), &["-fa"]));
        assert_eq!(strings(&launch_with(&switches("f16", "off", "off", None), &parse_help(OLD_HELP))), base());
        let none = launch_with(&switches("f16", "on", "off", None), &Capabilities::default());
        assert_eq!(strings(&none), base());
        assert_eq!(none.notes.len(), 1);
        assert!(none.notes[0].contains("AQUA_LLAMA_FA=on") && none.notes[0].contains("not available in this build"), "{:?}", none.notes);
    }

    #[test]
    fn a_quantised_cache_that_the_build_could_not_start_with_stays_at_f16_and_says_why() {
        // Flash attention turned off by the other switch: the server would refuse a quantised V cache, so the cache stays f16 and flash attention stays off.
        let off = launch_with(&switches("q8_0", "off", "off", None), &real_caps());
        assert_eq!(strings(&off), with(base(), &["-fa", "off"]));
        assert_eq!(off.notes.len(), 1);
        assert!(off.notes[0].contains("AQUA_LLAMA_FA=off"), "{:?}", off.notes);
        // A build without flash attention.
        let mut no_fa = real_caps();
        no_fa.flash_attn = FlashAttn::Absent;
        let launch = launch_with(&switches("q8_0", "auto", "off", None), &no_fa);
        assert_eq!(strings(&launch), base());
        assert!(launch.notes[0].contains("flash attention"), "{:?}", launch.notes);
        // A build that does not list the type.
        let mut few = real_caps();
        few.cache_types = names(&["f16", "q8_0"]);
        let launch = launch_with(&switches("q4_0", "auto", "off", None), &few);
        assert_eq!(strings(&launch), base());
        assert!(launch.notes[0].contains("AQUA_LLAMA_KV=q4_0") && launch.notes[0].contains("not available in this build"), "{:?}", launch.notes);
        assert!(launch_with(&switches("q8_0", "auto", "off", None), &few).notes.is_empty(), "the type it lists is used");
    }

    #[test]
    fn the_speculation_switch_takes_the_ngram_type_the_build_has_and_does_nothing_without_one() {
        let ngram = switches("f16", "auto", "ngram", None);
        assert_eq!(strings(&launch_with(&ngram, &real_caps())), with(base(), &["--spec-type", "ngram-mod"]), "ngram-mod is the first choice");
        let mut caps = real_caps();
        caps.spec_types = names(&["none", "ngram-simple", "ngram-map-k"]);
        assert_eq!(strings(&launch_with(&ngram, &caps)), with(base(), &["--spec-type", "ngram-simple"]));
        caps.spec_types = names(&["none", "ngram-map-k"]);
        assert_eq!(strings(&launch_with(&ngram, &caps)), with(base(), &["--spec-type", "ngram-map-k"]));
        // A type by name, when the build has it.
        assert_eq!(strings(&launch_with(&switches("f16", "auto", "ngram-simple", None), &real_caps())), with(base(), &["--spec-type", "ngram-simple"]));
        // A build with draft models only, an old build and an unreadable help: the log says it is not available, and the arguments are today's.
        for caps in [Capabilities { spec_types: names(&["none", "draft-simple"]), ..real_caps() }, parse_help(OLD_HELP), Capabilities::default()] {
            let launch = launch_with(&ngram, &caps);
            assert_eq!(strings(&launch), base());
            assert_eq!(launch.notes.len(), 1);
            assert!(launch.notes[0].contains("AQUA_LLAMA_SPEC=ngram") && launch.notes[0].contains("not available in this build"), "{:?}", launch.notes);
            assert!(launch.applied.is_empty());
        }
        // A named type the build lacks.
        caps.spec_types = names(&["none", "ngram-simple"]);
        let launch = launch_with(&switches("f16", "auto", "ngram-mod", None), &caps);
        assert_eq!(strings(&launch), base());
        assert!(launch.notes[0].contains("AQUA_LLAMA_SPEC=ngram-mod"), "{:?}", launch.notes);
    }

    #[test]
    fn a_thinking_budget_above_zero_lets_the_server_think_and_zero_leaves_it_off() {
        let launch = launch_with(&switches("f16", "auto", "off", Some(256)), &real_caps());
        let args = strings(&launch);
        let mut expected = base();
        let at = expected.iter().position(|a| a == "--reasoning").unwrap();
        expected[at + 1] = "on".to_string();
        assert_eq!(args, with(expected, &["--reasoning-budget", "256"]));
        assert!(launch.thinking, "the requests must not say that thinking is off");
        assert_eq!(launch.applied, vec!["think_budget=256".to_string()]);
        // 0 ends thinking at once: the server stays as it is, with the budget stated.
        let zero = launch_with(&switches("f16", "auto", "off", Some(0)), &real_caps());
        assert_eq!(strings(&zero), with(base(), &["--reasoning-budget", "0"]));
        assert!(!zero.thinking);
        // A build without the option: today's arguments, and no thinking.
        let none = launch_with(&switches("f16", "auto", "off", Some(256)), &Capabilities { reasoning_budget: false, ..real_caps() });
        assert_eq!(strings(&none), base());
        assert!(!none.thinking);
        assert!(none.notes[0].contains("AQUA_LLAMA_THINK_BUDGET") && none.notes[0].contains("not available in this build"), "{:?}", none.notes);
    }

    #[test]
    fn all_four_switches_together() {
        let launch = launch_with(&switches("q8_0", "auto", "ngram", Some(128)), &real_caps());
        let mut expected = base();
        let at = expected.iter().position(|a| a == "--reasoning").unwrap();
        expected[at + 1] = "on".to_string();
        assert_eq!(strings(&launch), with(expected, &["-ctk", "q8_0", "-ctv", "q8_0", "-fa", "on", "--spec-type", "ngram-mod", "--reasoning-budget", "128"]));
        assert_eq!(launch.applied, names(&["kv=q8_0", "fa=on", "spec=ngram-mod", "think_budget=128"]));
        assert!(launch.notes.is_empty() && launch.thinking);
    }

    fn raw(kv: &str, fa: &str, spec: &str, think: &str) -> RawSwitches {
        let some = |s: &str| Some(s.to_string());
        RawSwitches { kv: some(kv), fa: some(fa), spec: some(spec), think_budget: some(think) }
    }

    #[test]
    fn the_variables_are_read_against_their_lists_and_the_default_is_used_for_anything_else() {
        // Not set, and blank: the default, with nothing to warn about.
        for r in [RawSwitches::default(), raw("", "  ", "\t", "")] {
            let (s, warnings) = parse_switches(&r);
            assert!(s.is_default() && warnings.is_empty(), "{s:?} {warnings:?}");
        }
        // Any case and spaces around are accepted; the value kept is the entry of the list.
        let (s, warnings) = parse_switches(&raw(" Q8_0 ", "ON", "NGram", " 12 "));
        assert_eq!(s, switches("q8_0", "on", "ngram", Some(12)));
        assert!(warnings.is_empty(), "{warnings:?}");
        let (s, _) = parse_switches(&raw("q4_0", "off", "ngram-mod", "4096"));
        assert_eq!(s, switches("q4_0", "off", "ngram-mod", Some(4096)));
        assert_eq!(parse_switches(&raw("", "", "", "0")).0.think_budget, Some(0));
        // Real cache types that are not on the list, a budget out of range or not a number: ignored, with one warning each.
        for (r, field) in [
            (raw("f32", "", "", ""), "AQUA_LLAMA_KV"),
            (raw("q5_0", "", "", ""), "AQUA_LLAMA_KV"),
            (raw("", "yes", "", ""), "AQUA_LLAMA_FA"),
            (raw("", "", "draft-simple", ""), "AQUA_LLAMA_SPEC"),
            (raw("", "", "", "4097"), "AQUA_LLAMA_THINK_BUDGET"),
            (raw("", "", "", "-1"), "AQUA_LLAMA_THINK_BUDGET"),
            (raw("", "", "", "many"), "AQUA_LLAMA_THINK_BUDGET"),
        ] {
            let (s, warnings) = parse_switches(&r);
            assert!(s.is_default(), "{s:?}");
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            assert!(warnings[0].starts_with(field) && warnings[0].contains("ignored"), "{warnings:?}");
        }
    }

    #[test]
    fn a_hostile_value_never_reaches_the_command_line() {
        let hostile = [
            "q8_0 --evil",
            "--evil",
            "q8_0; calc",
            "\"q8_0\"",
            "q8_0\n-x",
            "on --port 1",
            "ngram --spec-draft-model x",
            "5 --evil",
            "-5",
            "99999999999",
            "4097",
            "f32",
            "bf16",
            "auto on",
            "../../x",
            "q8_0\0",
        ];
        for h in hostile {
            let (s, warnings) = parse_switches(&raw(h, h, h, h));
            assert!(s.is_default(), "{h:?} gave {s:?}");
            assert_eq!(warnings.len(), 4, "{h:?}: {warnings:?}");
            assert!(warnings.iter().all(|w| w.chars().all(|c| !c.is_control())), "a warning is one line of text: {warnings:?}");
            assert_eq!(strings(&launch_with(&s, &real_caps())), base(), "{h:?}");
        }
        // A long value is cut in the log line.
        let long = "x".repeat(500);
        let (_, warnings) = parse_switches(&raw(&long, "", "", ""));
        assert!(warnings[0].len() < 200, "{}", warnings[0].len());
    }

    #[test]
    fn whatever_the_switches_and_the_build_every_argument_is_one_of_a_fixed_set() {
        let fixed: Vec<String> = base().into_iter().chain(names(&["on", "off", "-ctk", "-ctv", "-fa", "q8_0", "q4_0", "--spec-type", "ngram-mod", "ngram-simple", "ngram-map-k", "--reasoning-budget"])).collect();
        let builds = [real_caps(), parse_help(OLD_HELP), Capabilities::default()];
        let mut runs = 0;
        for kv in KV_VALUES {
            for fa in FA_VALUES {
                for spec in SPEC_VALUES {
                    for think in [None, Some(0), Some(1), Some(MAX_THINK_BUDGET)] {
                        for caps in &builds {
                            let s = switches(kv, fa, spec, think);
                            let launch = launch_with(&s, caps);
                            runs += 1;
                            for a in strings(&launch) {
                                assert!(fixed.contains(&a) || a.chars().all(|c| c.is_ascii_digit()), "{a:?} from {s:?}");
                            }
                            if s.is_default() {
                                assert_eq!(strings(&launch), base());
                            }
                            // Each of the four switches is reported once, as applied or as not used.
                            assert!(launch.applied.len() + launch.notes.len() <= 4);
                        }
                    }
                }
            }
        }
        assert_eq!(runs, 3 * 3 * 5 * 4 * 3);
    }
}
