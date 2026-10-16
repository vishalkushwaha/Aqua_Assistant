//! What each family of model needs said to it, and taken out of what it says (card OA9-03; tasks PR-06 and D-PR-3, D-PR-12 of the OpenCode audit).
//!
//! Aqua was tuned on one family. Every prompt ends with a line `/no_think` (Qwen3's switch that keeps its chain of thought out of a spoken answer), every request says
//! `chat_template_kwargs.enable_thinking = false`, and every reply has `<think>`, `<thinking>`, `<reason>` and `<reasoning>` blocks cut out of it, a closing tag with no opening one taking
//! the text before it and an opening tag that is never closed taking the rest. For a Llama, a Gemma or a Mistral the first is a stray line of the person's prompt (and a changing end of
//! a prompt the server's cache has to read again), the second is a field its template ignores, and the third deletes the answer of a reply that has XML in it.
//!
//! A [`ModelAdapter`] is the answer to "how is this family spoken to":
//!
//! | | prompt | request | reply |
//! | :--- | :--- | :--- | :--- |
//! | [`Qwen3`] (Qwen3 and Qwen3-Coder) | ends with `\n/no_think` (once) | `enable_thinking: false`, as the server was started | balanced and leading `<think>` blocks out; text before a closing tag with no opening one out (the template put the opening tag in the prompt) |
//! | [`Generic`] (every other family) | as it is | no `chat_template_kwargs` at all | balanced and leading `<think>` blocks out; **nothing else**: a reply that has `<reasoning>` in it as its own words keeps them |
//!
//! **With `AQUA_MODEL_PROFILES` off (the default) none of this is used:** the one recipe of the Qwen3 column goes to every model and the reply is cut by the old rules
//! ([`CleanPolicy::Legacy`]), byte for byte what was done before. With it on, the family decides, and the family is found by [`super::profile::family_for`] (the profile, the header, the
//! name). For a Qwen3 file the request is the same bytes as before (a golden test holds that).

use super::sampling::{self, ModelSettings};
use super::types::{ReasoningMode, Role, Sampling};
use super::Family;
use crate::flags;
use std::sync::atomic::{AtomicU8, Ordering};

/// How a reply is cleaned of a model's thinking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanPolicy {
    /// The rules Aqua always had: four tag names, a closing tag with no opener takes the text before it, an opening tag with no closer takes the rest.
    Legacy,
    /// Qwen3: `<think>` and `<thinking>` blocks that are balanced or open the reply, and the text before a closing tag whose opening tag was in the prompt.
    Qwen3,
    /// Any other family: `<think>` and `<thinking>` blocks that are balanced or open the reply, and nothing else.
    Generic,
}

/// How a family is spoken to.
pub trait ModelAdapter: Send + Sync {
    /// "qwen3" or "generic".
    fn name(&self) -> &'static str;
    /// The prompt as this family is given it.
    fn adapt_prompt(&self, prompt: &str) -> String;
    /// What the request says about thinking: [`ReasoningMode::ServerDefault`] is the request the engine has always sent.
    fn reasoning(&self) -> ReasoningMode;
    fn policy(&self) -> CleanPolicy;
    /// The reply with the model's thinking taken out, by this family's rules.
    fn clean_output(&self, text: &str) -> String {
        clean_with(self.policy(), text)
    }
}

/// Qwen3 and Qwen3-Coder.
pub struct Qwen3;
/// Every other family.
pub struct Generic;
/// What every model was spoken to by before this card: Qwen3's prompt and request, and the reply cleaned by the old rules. Used while `AQUA_MODEL_PROFILES` is off.
pub struct Legacy;

pub static QWEN3: Qwen3 = Qwen3;
pub static GENERIC: Generic = Generic;
pub static LEGACY: Legacy = Legacy;

/// The prompt with Qwen3's "do not think" switch, so that its chain of thought does not leak into a spoken answer. A prompt that has the switch already is not given a second.
pub fn qwen3_prompt(trimmed: &str) -> String {
    if trimmed.contains("/no_think") {
        trimmed.to_string()
    } else {
        format!("{trimmed}\n/no_think")
    }
}

impl ModelAdapter for Qwen3 {
    fn name(&self) -> &'static str {
        "qwen3"
    }

    fn adapt_prompt(&self, prompt: &str) -> String {
        qwen3_prompt(prompt)
    }

    fn reasoning(&self) -> ReasoningMode {
        // What the running server was started with: thinking is off unless AQUA_LLAMA_THINK_BUDGET is above 0, and the request says so, exactly as it always did.
        ReasoningMode::ServerDefault
    }

    fn policy(&self) -> CleanPolicy {
        CleanPolicy::Qwen3
    }
}

impl ModelAdapter for Legacy {
    fn name(&self) -> &'static str {
        "legacy"
    }

    fn adapt_prompt(&self, prompt: &str) -> String {
        qwen3_prompt(prompt)
    }

    fn reasoning(&self) -> ReasoningMode {
        ReasoningMode::ServerDefault
    }

    fn policy(&self) -> CleanPolicy {
        CleanPolicy::Legacy
    }
}

impl ModelAdapter for Generic {
    fn name(&self) -> &'static str {
        "generic"
    }

    fn adapt_prompt(&self, prompt: &str) -> String {
        prompt.to_string()
    }

    fn reasoning(&self) -> ReasoningMode {
        ReasoningMode::NoSwitch
    }

    fn policy(&self) -> CleanPolicy {
        CleanPolicy::Generic
    }
}

/// The adapter of a family. Qwen3-Coder is spoken to as Qwen3 is, as it was before this card; every other family is generic.
pub fn adapter_for(family: &Family) -> &'static dyn ModelAdapter {
    match family {
        Family::Qwen3 | Family::Qwen3Coder => &QWEN3,
        _ => &GENERIC,
    }
}

/// The recipe of a model when `AQUA_MODEL_PROFILES` is off: Qwen3's, for every model, and the old rules for cleaning what it says.
pub fn legacy() -> &'static dyn ModelAdapter {
    &LEGACY
}

/// Is every model spoken to by its family (`AQUA_MODEL_PROFILES`)?
pub fn profiles_on() -> bool {
    flags::is_on(&flags::MODEL_PROFILES)
}

/// The adapter that speaks to a model of `family`: its family's with the switch on, the one old recipe with it off.
pub fn adapter_in_use(family: &Family) -> &'static dyn ModelAdapter {
    if profiles_on() {
        adapter_for(family)
    } else {
        legacy()
    }
}

// ---- cleaning a reply --------------------------------------------------------------------------------------------------------------

const THINK_TAGS: [(&str, &str); 2] = [("<think>", "</think>"), ("<thinking>", "</thinking>")];

/// Takes the blocks `open` ... `close` out of `text`: every balanced one wherever it is, and one that is never closed only when it opens the text (the model was still thinking when it
/// ran out of room, so there is no answer). One that is never closed in the middle of a reply is the reply's own words and stays. Tags in any case.
fn remove_blocks(text: &mut String, open: &str, close: &str) {
    loop {
        // The tags are ASCII, so lower-casing ASCII keeps every position the same.
        let lower = text.to_ascii_lowercase();
        let Some(at) = lower.find(open) else { return };
        let after = at + open.len();
        match lower[after..].find(close) {
            Some(rel) => text.replace_range(at..after + rel + close.len(), ""),
            None => {
                if lower[..at].trim().is_empty() {
                    text.truncate(at);
                }
                return;
            }
        }
    }
}

/// Takes out the text before a closing tag that has no opening tag (a template that put the opening tag in the prompt: what the model wrote first was thinking).
fn remove_stray_closers(text: &mut String, close: &str) {
    loop {
        let lower = text.to_ascii_lowercase();
        let Some(at) = lower.find(close) else { return };
        text.replace_range(..at + close.len(), "");
    }
}

/// The reply cleaned by the rules of `policy`.
pub fn clean_with(policy: CleanPolicy, text: &str) -> String {
    match policy {
        CleanPolicy::Legacy => crate::modules::llm_router::llama_engine::strip_legacy(text),
        CleanPolicy::Qwen3 | CleanPolicy::Generic => {
            let mut out = text.to_string();
            for (open, close) in THINK_TAGS {
                remove_blocks(&mut out, open, close);
                if policy == CleanPolicy::Qwen3 {
                    remove_stray_closers(&mut out, close);
                }
            }
            let removed = text.chars().count().saturating_sub(out.chars().count());
            if removed > 0 {
                tracing::debug!("{removed} characters of a reply were thinking and were taken out ({policy:?})");
            }
            crate::modules::llm_router::llama_engine::tidy_lines(&out)
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

fn code(policy: CleanPolicy) -> u8 {
    match policy {
        CleanPolicy::Legacy => 0,
        CleanPolicy::Qwen3 => 1,
        CleanPolicy::Generic => 2,
    }
}

/// Records which family answered last, so that the places that clean a reply a second time (the agent, the pipeline, the flows: they have only the text) clean it by the rules of the
/// model that wrote it. One model answers at a time, so this is the one that wrote the text in hand; with `AQUA_MODEL_PROFILES` off it is never set and nothing changes.
pub fn note_answered_by(adapter: &dyn ModelAdapter) {
    if profiles_on() {
        CURRENT.store(code(adapter.policy()), Ordering::Relaxed);
    }
}

/// The policy `strip_reasoning_tags` cleans by: [`CleanPolicy::Legacy`] unless `AQUA_MODEL_PROFILES` is on and a model has answered.
pub fn current_policy() -> CleanPolicy {
    if !profiles_on() {
        return CleanPolicy::Legacy;
    }
    match CURRENT.load(Ordering::Relaxed) {
        1 => CleanPolicy::Qwen3,
        2 => CleanPolicy::Generic,
        _ => CleanPolicy::Legacy,
    }
}

// ---- the settings of a call --------------------------------------------------------------------------------------------------------

/// What decides the settings of one call.
pub struct CallInputs<'a> {
    pub role: Role,
    pub family: &'a Family,
    /// The `sampling` of the model's entry in `profiles.json`.
    pub profile_sampling: &'a Sampling,
    /// The role's sampling in `router.json`.
    pub role_sampling: &'a Sampling,
}

/// Does anything ask for settings on a request (the family switch, the card's presets or a seed)? With all three off the engine does not even look at a model's header.
pub fn settings_wanted() -> bool {
    profiles_on() || flags::is_on(&flags::SAMPLING_PRESETS) || sampling::env_seed().is_some()
}

/// The settings of a call, or `None` when they add nothing to what the engine has always sent. Layers, later over earlier: the card's preset for the family and role
/// (`AQUA_SAMPLING_PRESETS`), the profile's sampling and the role's (`AQUA_MODEL_PROFILES`), the seed (`AQUA_SEED`, whatever else is on). The thinking switch is the family's with
/// `AQUA_MODEL_PROFILES` on.
pub fn settings_for(i: &CallInputs<'_>) -> Option<ModelSettings> {
    if !settings_wanted() {
        return None;
    }
    let (profiles, presets, seed) = (profiles_on(), flags::is_on(&flags::SAMPLING_PRESETS), sampling::env_seed());
    let none = Sampling::default();
    let preset = if presets { sampling::card_preset(i.family, i.role) } else { Sampling::default() };
    let (profile, role) = if profiles { (i.profile_sampling, i.role_sampling) } else { (&none, &none) };
    let seed_layer = Sampling { seed, ..Sampling::default() };
    let settings = ModelSettings { sampling: sampling::layer(&[&preset, profile, role, &seed_layer]), reasoning: if profiles { adapter_for(i.family).reasoning() } else { ReasoningMode::ServerDefault } };
    (!settings.is_default()).then_some(settings)
}

/// Tests that set or read which family answered last take this first: it is one number for the whole process.
#[cfg(test)]
pub(crate) static POLICY_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::llama_server_provider::body_for;
    use crate::model::types::{ChatRequest, Message, Purpose};
    use crate::modules::llm_router::llama_server::request_body_with;
    use serde_json::json;

    fn on() -> [(&'static str, Option<&'static str>); 3] {
        [("AQUA_MODEL_PROFILES", Some("on")), ("AQUA_SAMPLING_PRESETS", None), ("AQUA_SEED", None)]
    }

    #[test]
    fn qwen3_and_its_coder_are_spoken_to_as_before_and_every_other_family_is_generic() {
        for f in [Family::Qwen3, Family::Qwen3Coder] {
            assert_eq!(adapter_for(&f).name(), "qwen3");
        }
        for f in [Family::Llama3, Family::Gemma3, Family::Mistral, Family::Phi, Family::Granite, Family::Other("qwen".into())] {
            assert_eq!(adapter_for(&f).name(), "generic", "{f:?}");
        }
        assert_eq!(legacy().name(), "legacy");
        assert_eq!(legacy().adapt_prompt("Hi"), QWEN3.adapt_prompt("Hi"));
        assert_eq!(legacy().reasoning(), QWEN3.reasoning());
    }

    #[test]
    fn the_qwen3_prompt_ends_with_the_switch_once_and_the_generic_prompt_is_left_alone() {
        assert_eq!(QWEN3.adapt_prompt("Hello"), "Hello\n/no_think");
        assert_eq!(QWEN3.adapt_prompt("Hello\n/no_think"), "Hello\n/no_think", "a prompt that has the switch is not given a second");
        assert_eq!(GENERIC.adapt_prompt("Hello"), "Hello");
        assert_eq!(qwen3_prompt("a"), "a\n/no_think");
    }

    #[test]
    fn with_the_switch_off_every_model_gets_the_one_recipe_and_nothing_else_is_asked_of_the_request() {
        let _one = POLICY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        flags::with_overrides(&[("AQUA_MODEL_PROFILES", None), ("AQUA_SAMPLING_PRESETS", None), ("AQUA_SEED", None)], || {
            assert_eq!(adapter_in_use(&Family::Llama3).name(), "legacy", "the old recipe: Qwen3's for everything");
            assert_eq!(adapter_in_use(&Family::Llama3).adapt_prompt("Hi"), "Hi\n/no_think");
            assert!(!settings_wanted());
            assert_eq!(current_policy(), CleanPolicy::Legacy);
            let i = CallInputs { role: Role::Chat, family: &Family::Llama3, profile_sampling: &Sampling { temperature: Some(0.1), ..Sampling::default() }, role_sampling: &Sampling::default() };
            assert_eq!(settings_for(&i), None, "a profile's sampling is not used while the switch is off");
            note_answered_by(&GENERIC);
            assert_eq!(current_policy(), CleanPolicy::Legacy, "and a family that answered changes nothing");
        });
    }

    #[test]
    fn with_the_switch_on_a_qwen3_request_is_the_same_bytes_as_before_and_a_llama_one_has_neither_quirk() {
        flags::with_overrides(&on(), || {
            assert_eq!(adapter_in_use(&Family::Llama3).name(), "generic");
            let none = Sampling::default();
            // Qwen3 with nothing else configured: no settings at all, so the request is the one the engine has always built.
            let qwen = CallInputs { role: Role::Agent, family: &Family::Qwen3, profile_sampling: &none, role_sampling: &none };
            assert_eq!(settings_for(&qwen), None);
            let prompt = adapter_in_use(&Family::Qwen3).adapt_prompt("Goal: open notepad");
            let messages = [Message::user(prompt.as_str())];
            let req = ChatRequest::new(&messages, 200, Purpose::Planner).with_slot(Some(1));
            assert_eq!(body_for(&req), request_body_with("Goal: open notepad\n/no_think", 200, Some(1), None, false), "the golden body of Qwen3");
            // A Llama gets a prompt with no switch and a body with no thinking field; everything else in the body is the same.
            let llama = CallInputs { role: Role::Agent, family: &Family::Llama3, profile_sampling: &none, role_sampling: &none };
            let settings = settings_for(&llama).expect("a family with no switch is not the default request");
            assert_eq!(settings.reasoning, ReasoningMode::NoSwitch);
            assert!(settings.sampling.is_empty());
            let prompt = adapter_in_use(&Family::Llama3).adapt_prompt("Goal: open notepad");
            assert_eq!(prompt, "Goal: open notepad");
            let messages = [Message::user(prompt.as_str())];
            let req = ChatRequest::new(&messages, 200, Purpose::Planner).with_slot(Some(1)).with_reasoning(settings.reasoning);
            let body = body_for(&req);
            assert!(body.get("chat_template_kwargs").is_none(), "{body}");
            let mut want = request_body_with("Goal: open notepad", 200, Some(1), None, false);
            want.as_object_mut().unwrap().remove("chat_template_kwargs");
            assert_eq!(body, want);
            assert_eq!(body["temperature"], json!(0.3));
            assert_eq!(body["top_p"], json!(0.9));
        });
    }

    #[test]
    fn the_layers_are_the_preset_then_the_profile_then_the_role_then_the_seed() {
        let profile = Sampling { temperature: Some(0.5), ..Sampling::default() };
        let role = Sampling { top_p: Some(0.95), ..Sampling::default() };
        let i = CallInputs { role: Role::Chat, family: &Family::Qwen3, profile_sampling: &profile, role_sampling: &role };
        // Only the presets: the card's numbers for a Qwen3 chat.
        let s = flags::with_overrides(&[("AQUA_MODEL_PROFILES", None), ("AQUA_SAMPLING_PRESETS", Some("on")), ("AQUA_SEED", None)], || settings_for(&i)).unwrap();
        assert_eq!((s.sampling.temperature, s.sampling.top_p, s.sampling.top_k), (Some(0.7), Some(0.8), Some(20)));
        assert_eq!(s.reasoning, ReasoningMode::ServerDefault, "the family is not consulted while the profile switch is off");
        // Both: the profile over the card for the temperature, the role over it for top_p, the card's top_k stays.
        let s = flags::with_overrides(&[("AQUA_MODEL_PROFILES", Some("on")), ("AQUA_SAMPLING_PRESETS", Some("on")), ("AQUA_SEED", Some("11"))], || settings_for(&i)).unwrap();
        assert_eq!((s.sampling.temperature, s.sampling.top_p, s.sampling.top_k, s.sampling.seed), (Some(0.5), Some(0.95), Some(20), Some(11)));
        // The profile and the role alone.
        let s = flags::with_overrides(&on(), || settings_for(&i)).unwrap();
        assert_eq!((s.sampling.temperature, s.sampling.top_p, s.sampling.top_k), (Some(0.5), Some(0.95), None));
        // The planner does not get the card's chat numbers.
        let agent = CallInputs { role: Role::Agent, family: &Family::Qwen3, profile_sampling: &Sampling::default(), role_sampling: &Sampling::default() };
        assert_eq!(flags::with_overrides(&[("AQUA_MODEL_PROFILES", None), ("AQUA_SAMPLING_PRESETS", Some("on")), ("AQUA_SEED", None)], || settings_for(&agent)), None);
    }

    #[test]
    fn a_seed_alone_changes_only_the_seed_and_wins_over_a_profiles_own() {
        let none = Sampling::default();
        let profile = Sampling { seed: Some(1), ..Sampling::default() };
        let i = CallInputs { role: Role::Agent, family: &Family::Llama3, profile_sampling: &profile, role_sampling: &none };
        let s = flags::with_overrides(&[("AQUA_MODEL_PROFILES", None), ("AQUA_SAMPLING_PRESETS", None), ("AQUA_SEED", Some("99"))], || settings_for(&i)).unwrap();
        assert_eq!(s.sampling, Sampling { seed: Some(99), ..Sampling::default() });
        assert_eq!(s.reasoning, ReasoningMode::ServerDefault);
        // With profiles on too, the environment's seed beats the profile's.
        let s = flags::with_overrides(&[("AQUA_MODEL_PROFILES", Some("on")), ("AQUA_SAMPLING_PRESETS", None), ("AQUA_SEED", Some("99"))], || settings_for(&i)).unwrap();
        assert_eq!(s.sampling.seed, Some(99));
        // The body carries it.
        let messages = [Message::user("p")];
        let req = ChatRequest::new(&messages, 10, Purpose::Chat).with_sampling(s.sampling);
        assert_eq!(body_for(&req)["seed"], json!(99));
    }

    // ---- cleaning ----

    fn clean(policy: CleanPolicy, text: &str) -> String {
        clean_with(policy, text)
    }

    #[test]
    fn balanced_and_leading_thinking_blocks_go_in_every_family() {
        for policy in [CleanPolicy::Qwen3, CleanPolicy::Generic] {
            assert_eq!(clean(policy, "<think>plan</think>\n\nThe answer."), "The answer.", "{policy:?}");
            assert_eq!(clean(policy, "Before.\n\n<think>hmm</think>\n\nAfter."), "Before.\n\nAfter.", "{policy:?}");
            assert_eq!(clean(policy, "<THINK>x</Think>Hi"), "Hi", "{policy:?}");
            assert_eq!(clean(policy, "<thinking>a</thinking><think>b</think>Done."), "Done.", "{policy:?}");
            assert_eq!(clean(policy, "<think>I was still thinking when I ran out of"), "", "{policy:?}: a reply that opens with thinking and never ends has no answer");
            assert_eq!(clean(policy, "  \n<think>never closed"), "", "{policy:?}");
            assert_eq!(clean(policy, "<think>only thinking</think>"), "", "{policy:?}");
            assert_eq!(clean(policy, "Caf\u{e9} <think>\u{e9}</think>cr\u{e8}me"), "Caf\u{e9} cr\u{e8}me", "{policy:?}");
            // The answer's own layout stays.
            assert_eq!(clean(policy, "<think>x</think>\n\ndef f():\n    return 1\n"), "def f():\n    return 1", "{policy:?}");
        }
    }

    #[test]
    fn an_opening_tag_in_the_middle_that_never_closes_is_the_replys_own_words_and_stays() {
        for policy in [CleanPolicy::Qwen3, CleanPolicy::Generic] {
            assert_eq!(clean(policy, "Answer first.\n<think>and then it never ends"), "Answer first.\n<think>and then it never ends", "{policy:?}");
        }
        // The old rule took the rest: it is still what the old policy does.
        assert_eq!(clean(CleanPolicy::Legacy, "Answer first.\n<think>and then it never ends"), "Answer first.");
    }

    #[test]
    fn a_closing_tag_with_no_opener_takes_the_text_before_it_only_for_qwen3() {
        let raw = "it began thinking in the prompt...</think>\n\nThe answer.";
        assert_eq!(clean(CleanPolicy::Qwen3, raw), "The answer.");
        assert_eq!(clean(CleanPolicy::Legacy, raw), "The answer.");
        assert_eq!(clean(CleanPolicy::Generic, raw), raw, "a family that does not do this keeps what it wrote");
        // Two of them, one after the other.
        assert_eq!(clean(CleanPolicy::Qwen3, "a</think>b</think>c"), "c");
    }

    #[test]
    fn xml_with_reasoning_tags_in_an_answer_is_kept_by_the_new_policies_and_mangled_by_the_old_one() {
        let answer = "Here is the config:\n\n<config>\n  <reasoning>the cache is warm</reasoning>\n  <reason>speed</reason>\n</config>\n\nThat is all.";
        for policy in [CleanPolicy::Qwen3, CleanPolicy::Generic] {
            assert_eq!(clean(policy, answer), answer, "{policy:?}");
        }
        assert_ne!(clean(CleanPolicy::Legacy, answer), answer, "the old rule cut these out (finding D-PR-12)");
        // An unclosed one is not a reason to lose the rest of the reply.
        let open = "Use <reasoning> as the outer tag and fill it in.";
        assert_eq!(clean(CleanPolicy::Generic, open), open);
        assert_eq!(clean(CleanPolicy::Legacy, open), "Use");
    }

    #[test]
    fn the_legacy_policy_is_the_old_function_itself() {
        for raw in ["<reasoning>why</reasoning>\nThe answer.", "a  \nb \t\r\n\r\nc", "<think>x</think>", "plain"] {
            assert_eq!(clean(CleanPolicy::Legacy, raw), crate::modules::llm_router::strip_reasoning_tags(raw));
        }
    }

    #[test]
    fn the_second_cleaning_follows_the_family_that_answered_and_only_with_the_switch_on() {
        let _one = POLICY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let xml = "<reasoning>kept</reasoning> is the tag.";
        flags::with_overrides(&on(), || {
            note_answered_by(&GENERIC);
            assert_eq!(current_policy(), CleanPolicy::Generic);
            assert_eq!(crate::modules::llm_router::strip_reasoning_tags(xml), xml);
            note_answered_by(&QWEN3);
            assert_eq!(current_policy(), CleanPolicy::Qwen3);
        });
        // Off again (this thread's value): the old rule, whatever answered last.
        flags::with_overrides(&[("AQUA_MODEL_PROFILES", None)], || {
            assert_eq!(current_policy(), CleanPolicy::Legacy);
            assert_eq!(crate::modules::llm_router::strip_reasoning_tags(xml), " is the tag.", "the old rule cut the element out and left the space that followed it, as it always did");
        });
    }
}
