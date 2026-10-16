//! The whole command line the model server is started with for the baseline model (Qwen3-8B Q4_K_M on the 4 GB card), held byte for byte (card OA9-03, task PR-04: argv identical for
//! the baseline model whatever the profiles and the family switch say). Move only: this was the inline module `launch_golden_tests` of `llama_server.rs`; the path of every test is now `modules::llm_router::llama_server::launch_golden_tests::<name>`.

use super::{default_gpu_layers, layers_for_launch, profile_gpu_layers, DEFAULT_CONTEXT};
use crate::flags;
use crate::modules::llm_router::launch_args::{launch_args, Capabilities, Switches, Values};
use crate::test_support::TempWorld;
use std::path::Path;

const BASELINE: &str = "Qwen3-8B-Q4_K_M.gguf";

/// The arguments of a launch on the card with `layers`, for `model` (the values the launch gives them: the window of 6144, one slot, six threads, the card's fit target).
fn argv(model: &Path, layers: &str) -> Vec<String> {
    let fit = vec!["--fit-target".to_string(), "1660".to_string()];
    let values = Values { model, port: 8123, window: DEFAULT_CONTEXT, threads: "6", threads_batch: "6", slots: 1, log_file: Path::new("llama-server.log"), layers: Some(layers), card: Some(("Vulkan1", fit.as_slice())) };
    launch_args(&values, &Switches::default(), &Capabilities::default()).args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
}

fn golden(model: &str) -> Vec<String> {
    ["-m", model, "--host", "127.0.0.1", "--port", "8123", "-c", "6144", "-t", "6", "-tb", "6", "--reasoning", "off", "--no-webui", "--parallel", "1", "-lv", "1", "--log-file", "llama-server.log", "-ngl", "22", "-dev", "Vulkan1", "--fit-target", "1660"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[test]
fn the_command_line_for_the_baseline_model_is_the_one_it_always_was_with_the_switch_off_and_with_it_on() {
    let world = TempWorld::new("launch_golden");
    let model = world.write(&format!("models/llm/{BASELINE}"), "not a model");
    let model_text = model.to_string_lossy().into_owned();
    // Off, with no profile: 22 layers at a window of 6144, the pair that was measured.
    let layers = layers_for_launch(true, None, DEFAULT_CONTEXT, profile_gpu_layers(&model));
    assert_eq!(layers, "22");
    assert_eq!(argv(&model, &layers), golden(&model_text));
    // The window and the layers are a pair: the processor gets none, and the environment's number is passed as it is.
    assert_eq!(layers_for_launch(false, None, DEFAULT_CONTEXT, Some(30)), "0", "a profile does not put layers on a processor that has no card");
    assert_eq!(layers_for_launch(true, Some("18".into()), DEFAULT_CONTEXT, Some(30)), "18", "AQUA_LLAMA_NGL wins over everything");
    assert_eq!(layers_for_launch(true, None, 2048, None), default_gpu_layers(2048).to_string());
    // A profile file with an entry for the baseline model that says what the launch says, and one entry that says more: with the switch off it is not read at all.
    world.write("models/profiles.json", format!(r#"{{"profiles": [{{"match": "{BASELINE}", "gpu_layers": 14, "window": 4096, "kv_cache": "q8_0", "sampling": {{"temperature": 0.9}}}}]}}"#));
    assert_eq!(profile_gpu_layers(&model), None, "AQUA_MODEL_PROFILES is off");
    assert_eq!(argv(&model, &layers_for_launch(true, None, DEFAULT_CONTEXT, profile_gpu_layers(&model))), golden(&model_text), "the argv is the same with a profile on disk and the switch off");
    // On, and the profile names no layers (a family or a note only): still the same argv.
    world.write("models/profiles.json", format!(r#"{{"profiles": [{{"match": "{BASELINE}", "family": "qwen3", "notes": "mine"}}]}}"#));
    flags::with_overrides(&[("AQUA_MODEL_PROFILES", Some("on"))], || {
        assert_eq!(profile_gpu_layers(&model), None);
        assert_eq!(argv(&model, &layers_for_launch(true, None, DEFAULT_CONTEXT, profile_gpu_layers(&model))), golden(&model_text));
    });
    // On, and the profile of another model: the baseline is not touched.
    world.write("models/profiles.json", r#"{"profiles": [{"match": "llama-3*", "gpu_layers": 9}]}"#);
    flags::with_overrides(&[("AQUA_MODEL_PROFILES", Some("on"))], || {
        assert_eq!(profile_gpu_layers(&model), None);
        assert_eq!(profile_gpu_layers(&world.path("models/llm/Llama-3.2-3B-Q4_K_M.gguf")), Some(9));
        assert_eq!(argv(&model, &layers_for_launch(true, None, DEFAULT_CONTEXT, profile_gpu_layers(&model))), golden(&model_text));
    });
}

/// The command line as the launch builds it, with the switches of card OA9-06 given: the same function (`server_options::full_launch`) that `launch` calls.
fn full_argv(model: &Path, more: &crate::modules::llm_router::server_options::Extras, caps: &Capabilities) -> Vec<String> {
    full_argv_with(model, &Switches::default(), more, caps, &|| Ok(std::path::PathBuf::from("D:/models/Qwen3-0.6B-Q8_0.gguf")))
}

fn full_argv_with(model: &Path, old: &Switches, more: &crate::modules::llm_router::server_options::Extras, caps: &Capabilities, draft: &dyn Fn() -> Result<std::path::PathBuf, String>) -> Vec<String> {
    use crate::modules::llm_router::server_options::full_launch;
    let fit = vec!["--fit-target".to_string(), "1660".to_string()];
    let values = Values { model, port: 8123, window: DEFAULT_CONTEXT, threads: "6", threads_batch: "6", slots: 1, log_file: Path::new("llama-server.log"), layers: Some("22"), card: Some(("Vulkan1", fit.as_slice())) };
    full_launch(&values, old, more, caps, &|| Ok(std::path::PathBuf::from("kv")), draft).args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
}

#[test]
fn the_command_line_of_the_baseline_model_is_the_same_with_every_switch_of_the_speed_experiments_off_whatever_the_build_lists() {
    use crate::modules::llm_router::launch_args::parse_help;
    use crate::modules::llm_router::server_options::{parse_extras, Extras, RawExtras};
    let model = Path::new(BASELINE);
    let every_option = parse_help("-cram, --cache-ram N   x\n--cache-reuse N   x\n--slot-save-path PATH   x\n--jinja, --no-jinja   x\n--spec-draft-model, -md, --model-draft FNAME   x\n");
    let text = BASELINE.to_string();
    for caps in [Capabilities::default(), every_option.clone()] {
        assert_eq!(full_argv(model, &Extras::default(), &caps), golden(&text), "nothing is added with every switch off");
    }
    // Nothing set in the environment is every switch off, and the environment's own blank values too.
    assert!(parse_extras(&RawExtras::default()).0.is_default());
    // With the three switches of this step on, and a build that lists them, they come after what the launch always passed, and nothing before them changes.
    let on = parse_extras(&RawExtras { cache_reuse: Some("256".into()), cache_ram: Some("2048".into()), slot_save: Some("on".into()), ..RawExtras::default() }).0;
    let mut want = golden(&text);
    want.extend(["--cache-reuse", "256", "--cache-ram", "2048", "--slot-save-path", "kv"].map(String::from));
    assert_eq!(full_argv(model, &on, &every_option), want);
    // A build that lists none of them gets the usual command line even with the switches on.
    assert_eq!(full_argv(model, &on, &Capabilities::default()), golden(&text));
}

#[test]
fn the_native_tool_mode_adds_the_servers_templates_after_the_usual_command_line_and_only_where_the_build_needs_it() {
    use crate::modules::llm_router::launch_args::parse_help;
    use crate::modules::llm_router::server_options::Extras;
    let model = Path::new(BASELINE);
    let text = BASELINE.to_string();
    let native = Extras { native_tools: true, ..Extras::default() };
    let templates_off = parse_help("--jinja, --no-jinja\n  whether to use jinja template engine for chat (default: disabled)\n");
    let templates_on = parse_help("--jinja, --no-jinja   whether to use jinja template engine for chat (default: enabled)\n");
    let mut want = golden(&text);
    want.push("--jinja".to_string());
    assert_eq!(full_argv(model, &native, &templates_off), want);
    assert_eq!(full_argv(model, &native, &templates_on), golden(&text), "a build that has them on is left as it is");
    assert_eq!(full_argv(model, &native, &Capabilities::default()), golden(&text), "a build that does not list the option is left as it is");
    assert_eq!(full_argv(model, &Extras::default(), &templates_off), golden(&text), "not native: the usual command line");
}

#[test]
fn a_request_without_an_offer_is_the_body_it_always_was_and_one_with_an_offer_has_the_tools_and_not_the_schema() {
    use super::{body_with_offer, build_body, Constraint, Extras};
    use crate::model::native_tools::{Offer, ToolSpec};
    let schema = Constraint::JsonSchema(serde_json::json!({ "type": "object", "properties": { "tool": { "enum": ["fs.read"] } } }));
    for (slot, logprobs, constraint) in [(None, None, Some(&schema)), (Some(1), None, Some(&schema)), (Some(0), Some(5), None), (None, None, None)] {
        let plain = build_body("the prompt", 640, slot, constraint, logprobs, &Extras::default());
        assert_eq!(body_with_offer("the prompt", 640, slot, constraint, logprobs, &Extras::default(), None), plain, "no offer: the same bytes");
        assert_eq!(body_with_offer("the prompt", 640, slot, constraint, logprobs, &Extras::default(), None).to_string(), plain.to_string());
    }
    let offer = Offer::new(&[ToolSpec { name: "fs.read".into(), description: "reads".into(), schema: serde_json::json!({ "type": "object" }) }]).unwrap();
    let with = body_with_offer("the prompt", 640, Some(1), Some(&schema), None, &Extras::default(), Some(&offer));
    assert_eq!(with["tools"][0]["function"]["name"], "fs__read");
    assert_eq!(with["tool_choice"], "auto");
    assert!(with.get("response_format").is_none() && with.get("grammar").is_none());
    let plain = build_body("the prompt", 640, Some(1), None, None, &Extras::default());
    for key in ["messages", "max_tokens", "temperature", "top_p", "stream", "cache_prompt", "id_slot", "chat_template_kwargs"] {
        assert_eq!(with[key], plain[key], "{key} is as it was");
    }
}

#[test]
fn a_draft_model_comes_after_what_the_launch_always_passed_and_only_when_it_is_on_valid_and_listed_by_the_build() {
    use crate::modules::llm_router::launch_args::parse_help;
    use crate::modules::llm_router::server_options::{parse_extras, RawExtras};
    let model = Path::new(BASELINE);
    let text = BASELINE.to_string();
    let build = parse_help("--spec-type none,draft-simple,ngram-mod\n   x\n--spec-draft-model, -md, --model-draft FNAME   x\n--spec-draft-n-max N   x\n--spec-draft-n-min N   x\n--spec-draft-p-min, --draft-p-min P   x\n--spec-draft-ngl, -ngld N   x\n");
    let on = parse_extras(&RawExtras { draft: Some("on".into()), ..RawExtras::default() }).0;
    let mut want = golden(&text);
    want.extend(["--spec-draft-model", "D:/models/Qwen3-0.6B-Q8_0.gguf", "--spec-type", "draft-simple", "--spec-draft-n-max", "5", "--spec-draft-n-min", "1", "--spec-draft-p-min", "0.5", "--spec-draft-ngl", "0"].map(String::from));
    assert_eq!(full_argv(model, &on, &build), want, "the usual launch, then the draft with its tuning");
    // The file that fails its checks, a build without the options and the switch off: the usual command line.
    let off = parse_extras(&RawExtras::default()).0;
    assert_eq!(full_argv(model, &off, &build), golden(&text));
    assert_eq!(full_argv(model, &on, &Capabilities::default()), golden(&text));
    assert_eq!(full_argv_with(model, &Switches::default(), &on, &build, &|| Err("the draft model's file was not found".into())), golden(&text));
    // With the n-gram speculation of the older switch on, the draft is not added, and the n-gram type is the only --spec-type.
    let ngram = Switches { spec: "ngram", ..Switches::default() };
    let args = full_argv_with(model, &ngram, &on, &build, &|| Ok(std::path::PathBuf::from("d.gguf")));
    assert_eq!(args.iter().filter(|a| *a == "--spec-type").count(), 1);
    assert!(!args.contains(&"--spec-draft-model".to_string()));
}

#[test]
fn a_profile_that_names_layers_changes_the_minus_ngl_and_only_that_and_only_with_the_switch_on() {
    let world = TempWorld::new("launch_profile_layers");
    let model = world.write(&format!("models/llm/{BASELINE}"), "not a model");
    world.write("models/profiles.json", r#"{"profiles": [{"match": "qwen3-8b*", "gpu_layers": 14}]}"#);
    let off = argv(&model, &layers_for_launch(true, None, DEFAULT_CONTEXT, profile_gpu_layers(&model)));
    let model_text = model.to_string_lossy().into_owned();
    assert_eq!(off, golden(&model_text), "off: a profile is not read");
    let on = flags::with_overrides(&[("AQUA_MODEL_PROFILES", Some("on"))], || argv(&model, &layers_for_launch(true, None, DEFAULT_CONTEXT, profile_gpu_layers(&model))));
    let mut want = golden(&model_text);
    let at = want.iter().position(|a| a == "-ngl").unwrap();
    want[at + 1] = "14".to_string();
    assert_eq!(on, want, "on: the layers of the profile and nothing else in the command line");
    // A profile file that cannot be used changes nothing.
    world.write("models/profiles.json", "not json");
    flags::with_overrides(&[("AQUA_MODEL_PROFILES", Some("on"))], || assert_eq!(profile_gpu_layers(&model), None));
}
