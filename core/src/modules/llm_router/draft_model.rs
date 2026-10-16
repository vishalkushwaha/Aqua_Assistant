//! A small draft model for speculative decoding (card OA9-06, task PR-12 of the OpenCode audit).
//!
//! Writing a token takes the 8B model about 130 ms on this card, and a small model of the same family can guess several tokens ahead cheaply: the big model then checks the guesses all at
//! once and keeps the ones it agrees with. With `AQUA_LLAMA_DRAFT` on, the launch of the model server adds the draft model and its tuning (the numbers of audit 5, issue 33: at most 5 tokens
//! guessed at a time, at least 1, none below a probability of 0.5), and the planner's JSON, which repeats its own keys, is the kind of text that the guesses should hit.
//!
//! **The file is the owner's.** `AQUA_LLAMA_DRAFT_MODEL` names it (a file name in the model folder, or a full path) and nothing in Aqua downloads one. It must be a `.gguf` file that is
//! really a GGUF file (its header is read), **inside the model folder or inside the one folder `AQUA_LLAMA_DRAFT_FOLDER` allows** (links are followed before the test, so a link that
//! leaves is refused), must not be the main model itself, and must have as many tokens in its vocabulary as the main model (speculation needs the same tokens: a draft of another family
//! is refused with that sentence; the server's own check is the last word). A file that fails any of this means **the launch is unchanged** and the status says why.
//!
//! **Where the draft runs** is a choice: `cpu` (the default of `AQUA_LLAMA_DRAFT=on`, `--spec-draft-ngl 0`) keeps it off a card that the main model already fills (3.6 of 4 GB at the
//! launch's 22 layers; a draft of 0.7 GB there would make the server give the card up), `gpu` leaves the server to place it. A build that does not list `--spec-draft-ngl` cannot be told to
//! keep the draft off the card, so with `cpu` it is not used. The draft and `AQUA_LLAMA_SPEC` (the n-gram types) are separate experiments: with both on, the draft is not used and the
//! status says so.
//!
//! **What it costs is counted, not guessed:** the server's reply carries `draft_n` and `draft_n_accepted`; they are added up since the launch, the share that was accepted is in the status,
//! and under 55 percent after 200 drafted tokens the log says once that the switch is better off.
//!
//! It has not been run on a model. The option names are those of the vendored source (`common/arg.cpp`); whether the build of `llama-server` that Aqua starts accepts a draft of the
//! family of the Qwen3-8B, and what the guesses gain on this machine, is what `scripts/eval/speed_experiments/run_experiment.py draft_model` is for.

use super::launch_args::Capabilities;
use crate::model::gguf;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The tokens guessed at most, at least, and the least probability, as the audit gave them (issue 33).
pub(crate) const N_MAX: &str = "5";
pub(crate) const N_MIN: &str = "1";
pub(crate) const P_MIN: &str = "0.5";
/// The share of the guesses that must be accepted for the draft to be worth its memory (the audit's criterion), and how many guesses must have been made before it is judged.
pub(crate) const MIN_SHARE: f64 = 0.55;
pub(crate) const JUDGED_AFTER: u64 = 200;
/// The longest text a setting may be.
const MAX_SETTING_CHARS: usize = 520;

/// Where the draft model runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    /// `--spec-draft-ngl 0`: on the processor, off the card.
    Processor,
    /// The server places it (on the card when there is room).
    Card,
}

impl Place {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Place::Processor => "processor",
            Place::Card => "card",
        }
    }
}

/// The paths a draft model may be named by, and the checks of its file. Pure but for the files it reads.
///
/// `text` is `AQUA_LLAMA_DRAFT_MODEL`, `extra_folder` is `AQUA_LLAMA_DRAFT_FOLDER`, `model_folder` is the model folder and `target` is the main model's file (when it is known: the status
/// does not know it, and then the vocabulary is not compared). Gives the file to pass to the server, or the reason it is not used.
pub(crate) fn check_file(text: Option<&str>, extra_folder: Option<&str>, model_folder: &Path, target: Option<&Path>) -> Result<PathBuf, String> {
    let text = text.map(str::trim).filter(|t| !t.is_empty()).ok_or_else(|| "AQUA_LLAMA_DRAFT_MODEL is not set: name the draft model's file (Aqua never downloads one)".to_string())?;
    if text.chars().count() > MAX_SETTING_CHARS || text.chars().any(char::is_control) {
        return Err("AQUA_LLAMA_DRAFT_MODEL is not a file name or a path (too long, or it holds a control character)".to_string());
    }
    let named = if text.contains(['/', '\\', ':']) {
        let p = PathBuf::from(text);
        if !p.is_absolute() {
            return Err("AQUA_LLAMA_DRAFT_MODEL must be a file name of the model folder or a full path".to_string());
        }
        p
    } else {
        model_folder.join(text)
    };
    if !named.extension().is_some_and(|e| e.eq_ignore_ascii_case("gguf")) {
        return Err("the draft model's file is not a .gguf file".to_string());
    }
    let real = std::fs::canonicalize(&named).map_err(|_| "the draft model's file was not found".to_string())?;
    if !real.is_file() {
        return Err("the draft model is not a file".to_string());
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(root) = std::fs::canonicalize(model_folder) {
        roots.push(root);
    }
    if let Some(folder) = extra_folder.map(str::trim).filter(|f| !f.is_empty()) {
        match std::fs::canonicalize(folder) {
            Ok(root) if root.is_dir() => roots.push(root),
            _ => return Err("AQUA_LLAMA_DRAFT_FOLDER is not a folder".to_string()),
        }
    }
    if !roots.iter().any(|root| real.starts_with(root)) {
        return Err("the draft model's file is not inside the model folder or the folder AQUA_LLAMA_DRAFT_FOLDER allows".to_string());
    }
    let main = target.and_then(|t| std::fs::canonicalize(t).ok());
    if main.as_deref() == Some(real.as_path()) {
        return Err("the draft model is the main model itself".to_string());
    }
    let draft_info = gguf::read_path(&real).map_err(|e| format!("the draft model's file is not a usable GGUF file: {e}"))?;
    if let Some(main) = main.as_deref().and_then(|m| gguf::read_cached(m).ok()) {
        if let (Some(a), Some(b)) = (main.vocab_size, draft_info.vocab_size) {
            if a != b {
                return Err(format!("the draft model has {b} tokens in its vocabulary and the main model has {a}: speculative decoding needs the same tokens, so it is not used"));
            }
        }
    }
    Ok(strip_verbatim(real))
}

/// `\\?\C:\x` as `C:\x` (a path that `canonicalize` made on Windows): the server is given the path in the form a person writes. A network path is left as it is.
pub(crate) fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") && rest.chars().nth(1) == Some(':') => PathBuf::from(rest),
        _ => path,
    }
}

/// What the launch adds for a draft model that passed [`check_file`], given what the build lists, and the note for what could not be added (then nothing is).
pub(crate) fn args_for(place: Place, file: &Path, caps: &Capabilities) -> Result<(Vec<OsString>, String), String> {
    if !caps.extra.draft_model || !caps.spec_types.iter().any(|t| t == "draft-simple") {
        return Err("this build of llama-server does not list --spec-draft-model and --spec-type draft-simple".to_string());
    }
    if place == Place::Processor && !caps.extra.draft_layers {
        return Err("this build of llama-server does not list --spec-draft-ngl, so the draft could not be kept off the graphics card".to_string());
    }
    let mut args: Vec<OsString> = vec![OsString::from("--spec-draft-model"), file.as_os_str().to_owned(), OsString::from("--spec-type"), OsString::from("draft-simple")];
    if caps.extra.draft_tuning {
        for (flag, value) in [("--spec-draft-n-max", N_MAX), ("--spec-draft-n-min", N_MIN), ("--spec-draft-p-min", P_MIN)] {
            args.push(OsString::from(flag));
            args.push(OsString::from(value));
        }
    }
    if caps.extra.draft_layers {
        args.push(OsString::from("--spec-draft-ngl"));
        args.push(OsString::from(if place == Place::Processor { "0" } else { "99" }));
    }
    let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("draft").to_string();
    Ok((args, format!("draft={name} ({})", place.word())))
}

// ---- what the guesses came to ------------------------------------------------------------------------------------------------------------------------------------------

/// The guesses the server made and the ones it kept, added up since the launch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub drafted: u64,
    pub accepted: u64,
    pub answers: u64,
    warned: bool,
}

impl Counts {
    /// The share of the guesses that were kept, when there were any.
    pub(crate) fn share(&self) -> Option<f64> {
        (self.drafted > 0).then(|| self.accepted as f64 / self.drafted as f64)
    }

    /// Adds one reply. Gives the advice the first time the draft has been judged and found wanting.
    pub(crate) fn add(&mut self, drafted: u64, accepted: u64) -> Option<String> {
        self.drafted += drafted;
        self.accepted += accepted.min(drafted);
        if drafted > 0 {
            self.answers += 1;
        }
        if !self.warned && self.drafted >= JUDGED_AFTER && self.share().is_some_and(|s| s < MIN_SHARE) {
            self.warned = true;
            return Some(format!("the draft model's guesses were kept {:.0} percent of the time over {} guesses (the audit asks for {:.0} or more): AQUA_LLAMA_DRAFT is better off", self.share().unwrap_or(0.0) * 100.0, self.drafted, MIN_SHARE * 100.0));
        }
        None
    }

    pub(crate) fn advice(&self) -> Option<String> {
        (self.drafted >= JUDGED_AFTER && self.share().is_some_and(|s| s < MIN_SHARE)).then(|| "under the share the audit asks for: the draft is better off".to_string())
    }
}

static COUNTS: Mutex<Counts> = Mutex::new(Counts { drafted: 0, accepted: 0, answers: 0, warned: false });

/// The server was started again: the counts begin again.
pub(crate) fn reset() {
    *COUNTS.lock().unwrap_or_else(|e| e.into_inner()) = Counts::default();
}

/// The `timings` of one reply: `draft_n` and `draft_n_accepted` are added up when the server says them. A reply without them (no draft, an older build) changes nothing.
pub(crate) fn note_timings(timings: &Value) {
    let (Some(drafted), accepted) = (timings["draft_n"].as_u64(), timings["draft_n_accepted"].as_u64().unwrap_or(0)) else { return };
    let advice = COUNTS.lock().unwrap_or_else(|e| e.into_inner()).add(drafted, accepted);
    if let Some(advice) = advice {
        tracing::warn!("llama-server draft model: {advice}");
    }
}

pub(crate) fn counts() -> Counts {
    *COUNTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The part of the report that is about the draft: whether a file is named and usable (by the checks that need no main model), and what the guesses came to.
pub(crate) fn report(text: Option<&str>, extra_folder: Option<&str>, model_folder: &Path, counts: Counts) -> Value {
    let named = text.map(str::trim).filter(|t| !t.is_empty());
    let (state, file, problem) = match (named, check_file(text, extra_folder, model_folder, None)) {
        (None, _) => ("not set", None, None),
        (Some(_), Ok(path)) => ("usable", path.file_name().and_then(|n| n.to_str()).map(str::to_string), None),
        (Some(_), Err(why)) => ("refused", None, Some(why)),
    };
    json!({
        "model_setting": state,
        "file_name": file,
        "problem": problem,
        "drafted": counts.drafted,
        "accepted": counts.accepted,
        "share_accepted": counts.share().map(|s| (s * 1000.0).round() / 1000.0),
        "advice": counts.advice(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::llm_router::launch_args::parse_help;
    use crate::test_support::TempWorld;

    /// What the help of a build lists when it has the draft options (written from `vendor/llama.cpp/common/arg.cpp`: the first name of each option first).
    const HELP: &str = r#"
--spec-type none,draft-simple,draft-eagle3,draft-mtp,draft-dflash,draft-dspark,ngram-simple,ngram-map-k,ngram-map-k4v,ngram-mod,ngram-cache
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
        parse_help(HELP)
    }

    /// A GGUF header with this architecture and a vocabulary of `vocab` tokens, and no tensor (what `gguf::read_path` needs and no more).
    fn gguf_bytes(architecture: &str, vocab: u64) -> Vec<u8> {
        fn string(out: &mut Vec<u8>, s: &str) {
            out.extend((s.len() as u64).to_le_bytes());
            out.extend(s.as_bytes());
        }
        let mut b = Vec::new();
        b.extend(b"GGUF");
        b.extend(3u32.to_le_bytes());
        b.extend(0u64.to_le_bytes());
        b.extend(2u64.to_le_bytes());
        string(&mut b, "general.architecture");
        b.extend(8u32.to_le_bytes());
        string(&mut b, architecture);
        string(&mut b, "tokenizer.ggml.tokens");
        b.extend(9u32.to_le_bytes());
        b.extend(8u32.to_le_bytes());
        b.extend(vocab.to_le_bytes());
        for i in 0..vocab {
            string(&mut b, &format!("t{i}"));
        }
        b
    }

    fn write_gguf(path: &Path, architecture: &str, vocab: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, gguf_bytes(architecture, vocab)).unwrap();
    }

    fn setup(name: &str) -> (TempWorld, PathBuf) {
        let world = TempWorld::new(name);
        let models = world.path("models/llm");
        write_gguf(&models.join("Qwen3-8B-Q4_K_M.gguf"), "qwen3", 8);
        write_gguf(&models.join("Qwen3-0.6B-Q8_0.gguf"), "qwen3", 8);
        (world, models)
    }

    #[test]
    fn the_toy_header_is_read_as_the_checks_read_it() {
        let (_w, models) = setup("draft_toy");
        let info = gguf::read_path(&models.join("Qwen3-0.6B-Q8_0.gguf")).unwrap();
        assert_eq!((info.architecture.as_str(), info.vocab_size), ("qwen3", Some(8)));
    }

    #[test]
    fn a_file_name_of_the_model_folder_or_a_full_path_inside_it_is_a_draft_model() {
        let (_w, models) = setup("draft_ok");
        let target = models.join("Qwen3-8B-Q4_K_M.gguf");
        let by_name = check_file(Some("Qwen3-0.6B-Q8_0.gguf"), None, &models, Some(&target)).expect("a file name of the model folder");
        assert_eq!(by_name.file_name().unwrap(), "Qwen3-0.6B-Q8_0.gguf");
        let full = models.join("Qwen3-0.6B-Q8_0.gguf");
        assert!(check_file(Some(&full.to_string_lossy()), None, &models, Some(&target)).is_ok(), "a full path inside the folder");
        assert!(check_file(Some("  Qwen3-0.6B-Q8_0.gguf  "), None, &models, None).is_ok(), "spaces around are not part of the name");
        assert!(check_file(Some("QWEN3-0.6B-Q8_0.GGUF"), None, &models, None).is_ok() || cfg!(not(windows)), "Windows ignores the case of a file name");
        assert!(!by_name.to_string_lossy().starts_with(r"\\?\"), "the server is given the path a person writes");
    }

    #[test]
    fn a_setting_that_is_not_a_usable_file_is_a_sentence_and_nothing_else() {
        let (world, models) = setup("draft_refused");
        let target = models.join("Qwen3-8B-Q4_K_M.gguf");
        let why = |text: Option<&str>, folder: Option<&str>| check_file(text, folder, &models, Some(&target)).unwrap_err();
        assert!(why(None, None).contains("is not set"));
        assert!(why(Some("   "), None).contains("is not set"));
        assert!(why(Some("a\nb.gguf"), None).contains("control character"));
        assert!(why(Some(&"x".repeat(600)), None).contains("too long"));
        assert!(why(Some("sub/draft.gguf"), None).contains("full path"), "a relative path with a folder in it is not a name");
        assert!(why(Some("..\\x.gguf"), None).contains("full path"));
        assert!(why(Some("Qwen3-0.6B-Q8_0.bin"), None).contains("not a .gguf"));
        assert!(why(Some("Missing.gguf"), None).contains("not found"));
        assert!(why(Some("Qwen3-8B-Q4_K_M.gguf"), None).contains("main model itself"), "the draft cannot be the model it drafts for");
        // Not inside the model folder, and not in the folder that was allowed.
        let elsewhere = world.path("elsewhere/Draft.gguf");
        write_gguf(&elsewhere, "qwen3", 8);
        assert!(why(Some(&elsewhere.to_string_lossy()), None).contains("not inside the model folder"));
        let other = world.path("another");
        std::fs::create_dir_all(&other).unwrap();
        assert!(why(Some(&elsewhere.to_string_lossy()), Some(&other.to_string_lossy())).contains("not inside the model folder"));
        // Inside the folder the person allowed: fine.
        assert!(check_file(Some(&elsewhere.to_string_lossy()), Some(&world.path("elsewhere").to_string_lossy()), &models, Some(&target)).is_ok());
        assert!(why(Some(&elsewhere.to_string_lossy()), Some("Z:\\no\\such\\folder")).contains("AQUA_LLAMA_DRAFT_FOLDER is not a folder"));
        // A folder is not a file; a file that is not a GGUF is not one.
        std::fs::create_dir_all(models.join("dir.gguf")).unwrap();
        assert!(why(Some("dir.gguf"), None).contains("not a file"));
        std::fs::write(models.join("Fake.gguf"), "this is not a model").unwrap();
        assert!(why(Some("Fake.gguf"), None).contains("not a usable GGUF"), "{}", why(Some("Fake.gguf"), None));
        std::fs::write(models.join("Short.gguf"), "GG").unwrap();
        assert!(why(Some("Short.gguf"), None).contains("not a usable GGUF"));
    }

    #[test]
    fn a_draft_with_another_vocabulary_is_refused_and_one_with_the_same_size_or_a_main_model_that_cannot_be_read_is_not() {
        let (_w, models) = setup("draft_vocab");
        let target = models.join("Qwen3-8B-Q4_K_M.gguf");
        write_gguf(&models.join("Llama-0.5B.gguf"), "llama", 11);
        let e = check_file(Some("Llama-0.5B.gguf"), None, &models, Some(&target)).unwrap_err();
        assert!(e.contains("11 tokens") && e.contains("main model has 8") && e.contains("same tokens"), "{e}");
        assert!(check_file(Some("Llama-0.5B.gguf"), None, &models, None).is_ok(), "the status does not know the main model, so it does not compare");
        assert!(check_file(Some("Llama-0.5B.gguf"), None, &models, Some(&models.join("Not-There.gguf"))).is_ok(), "a main model that cannot be read is not a reason: the server's own check is the last word");
    }

    #[cfg(windows)]
    #[test]
    fn a_link_inside_the_model_folder_that_leaves_it_is_refused() {
        let (world, models) = setup("draft_link");
        let outside = world.path("outside");
        write_gguf(&outside.join("Elsewhere.gguf"), "qwen3", 8);
        let link = models.join("linked");
        let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J", &link.to_string_lossy(), &outside.to_string_lossy()]).output();
        if !made.map(|o| o.status.success()).unwrap_or(false) {
            eprintln!("skipped: no junction could be made here");
            return;
        }
        let result = check_file(Some(&link.join("Elsewhere.gguf").to_string_lossy()), None, &models, None);
        let _ = std::process::Command::new("cmd").args(["/C", "rmdir", &link.to_string_lossy()]).output();
        assert!(result.unwrap_err().contains("not inside the model folder"), "a junction that leaves the folder does not make what is behind it a model of the folder");
    }

    #[test]
    fn the_arguments_are_the_draft_and_its_tuning_and_where_it_runs_and_nothing_else() {
        let file = Path::new("C:/models/Qwen3-0.6B-Q8_0.gguf");
        let (args, applied) = args_for(Place::Processor, file, &caps()).unwrap();
        let text: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(
            text,
            ["--spec-draft-model", "C:/models/Qwen3-0.6B-Q8_0.gguf", "--spec-type", "draft-simple", "--spec-draft-n-max", "5", "--spec-draft-n-min", "1", "--spec-draft-p-min", "0.5", "--spec-draft-ngl", "0"]
        );
        assert_eq!(applied, "draft=Qwen3-0.6B-Q8_0.gguf (processor)");
        assert!(!applied.contains("C:/"), "no path in the log line");
        let (args, applied) = args_for(Place::Card, file, &caps()).unwrap();
        assert_eq!(args.last().unwrap().to_string_lossy(), "99");
        assert!(applied.ends_with("(card)"));
    }

    #[test]
    fn a_build_that_lacks_an_option_gets_what_it_can_take_or_no_draft_and_never_an_option_it_does_not_list() {
        let file = Path::new("d.gguf");
        let none = parse_help("-fa, --flash-attn [on|off|auto]  x\n");
        assert!(args_for(Place::Processor, file, &none).unwrap_err().contains("does not list --spec-draft-model"));
        let mut no_type = caps();
        no_type.spec_types = vec!["none".into(), "ngram-simple".into()];
        assert!(args_for(Place::Card, file, &no_type).unwrap_err().contains("draft-simple"));
        let mut no_layers = caps();
        no_layers.extra.draft_layers = false;
        assert!(args_for(Place::Processor, file, &no_layers).unwrap_err().contains("kept off the graphics card"), "a draft that cannot be kept off a full card is not used");
        let (args, _) = args_for(Place::Card, file, &no_layers).unwrap();
        assert!(!args.iter().any(|a| a.to_string_lossy() == "--spec-draft-ngl"), "the card is left to the server, and the option is not passed");
        let mut no_tuning = caps();
        no_tuning.extra.draft_tuning = false;
        let (args, _) = args_for(Place::Processor, file, &no_tuning).unwrap();
        assert!(!args.iter().any(|a| a.to_string_lossy().starts_with("--spec-draft-n-") || a.to_string_lossy() == "--spec-draft-p-min"), "the tuning stays at the server's defaults");
    }

    #[test]
    fn the_guesses_are_added_up_and_the_draft_is_judged_after_enough_of_them() {
        let mut c = Counts::default();
        assert_eq!(c.share(), None);
        assert_eq!(c.add(100, 80), None, "not enough guesses to judge yet, and 80 percent is good anyway");
        assert_eq!(c.add(50, 5), None, "155 guesses: not yet");
        let advice = c.add(100, 10).expect("250 guesses and 38 percent are judged");
        assert!(advice.contains("better off") && advice.contains("38 percent"), "{advice}");
        assert_eq!(c.add(100, 0), None, "said once");
        assert!(c.advice().is_some());
        // More accepted than drafted cannot be.
        let mut c = Counts::default();
        c.add(10, 99);
        assert_eq!((c.drafted, c.accepted), (10, 10));
        // Good guesses are never advised against.
        let mut c = Counts::default();
        assert_eq!(c.add(1000, 700), None);
        assert!(c.advice().is_none());
    }

    #[test]
    fn a_reply_that_says_nothing_of_a_draft_changes_nothing_and_one_that_does_is_added() {
        reset();
        note_timings(&json!({ "prompt_n": 100, "predicted_n": 30 }));
        assert_eq!(counts().drafted, 0);
        note_timings(&json!({ "draft_n": 20, "draft_n_accepted": 15 }));
        note_timings(&json!({ "draft_n": 10 }));
        let c = counts();
        assert_eq!((c.drafted, c.accepted, c.answers), (30, 15, 2));
        reset();
        assert_eq!(counts(), Counts::default());
    }

    #[test]
    fn the_report_names_the_file_and_never_the_path_and_says_why_a_setting_is_refused() {
        let (_w, models) = setup("draft_report");
        let none = report(None, None, &models, Counts::default());
        assert_eq!(none["model_setting"], "not set");
        let ok = report(Some("Qwen3-0.6B-Q8_0.gguf"), None, &models, Counts::default());
        assert_eq!((ok["model_setting"].clone(), ok["file_name"].clone()), (json!("usable"), json!("Qwen3-0.6B-Q8_0.gguf")));
        assert!(!ok.to_string().contains("models"), "no folder in the report: {ok}");
        let refused = report(Some("Missing.gguf"), None, &models, Counts::default());
        assert_eq!(refused["model_setting"], "refused");
        assert!(refused["problem"].as_str().unwrap().contains("not found"));
        let counted = report(Some("Qwen3-0.6B-Q8_0.gguf"), None, &models, Counts { drafted: 400, accepted: 100, answers: 9, ..Counts::default() });
        assert_eq!(counted["share_accepted"], 0.25);
        assert!(counted["advice"].is_string());
    }
}
