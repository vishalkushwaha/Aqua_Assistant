//! How a model is asked to choose its words: the sampling settings, layered (card OA9-03; task PR-06 of the OpenCode audit).
//!
//! Until now every request to the local model carried the same two numbers, `temperature 0.3` and `top_p 0.9`, written in `llama_server::request_body_with` and meant for the planner's
//! JSON (a low temperature keeps a plan steady from one run to the next). Chat, edits and summaries got them too, and a person could not change them, and an evaluation could not be
//! repeated because nothing was seeded.
//!
//! Now a request's [`Sampling`] is the **layers** of: the model card's recommendation for its family (off unless `AQUA_SAMPLING_PRESETS` is on), the model's profile (`profiles.json`),
//! the role (`router.json`), the request itself and the seed of `AQUA_SEED`; each layer sets only what it names, a later layer wins field by field, and a field no layer names is left
//! out of the request, so the server's own 0.3 and 0.9 stand. **With nothing configured, nothing is sent that was not sent before** (a golden test holds the body to that).
//!
//! The values of a file are checked, not trusted: a temperature outside 0 to 2, a `top_p` outside 0 to 1, a `top_k` above 1000, a `min_p` outside 0 to 1, a repeat penalty outside
//! 0.5 to 2, more than four stop words or one of more than 64 characters is refused with the field and the range in the sentence.

use super::types::{ReasoningMode, Role, Sampling};
use super::Family;
use crate::flags;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// The most stop words a layer may name, and the longest one.
pub const MAX_STOPS: usize = 4;
pub const MAX_STOP_CHARS: usize = 64;

/// Sampling as a file writes it: every field optional, no field that is not known.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SamplingSpec {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub min_p: Option<f64>,
    pub seed: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    pub repeat_penalty: Option<f64>,
}

fn within(name: &str, value: Option<f64>, low: f64, high: f64) -> Result<Option<f64>, String> {
    match value {
        None => Ok(None),
        Some(v) if v.is_finite() && (low..=high).contains(&v) => Ok(Some(v)),
        Some(v) => Err(format!("{name} {v} is not between {low} and {high}")),
    }
}

impl SamplingSpec {
    /// The settings this names, or the first field that is out of range, in a sentence.
    pub fn checked(&self) -> Result<Sampling, String> {
        if self.top_k.is_some_and(|k| k > 1000) {
            return Err(format!("top_k {} is more than 1000", self.top_k.unwrap_or(0)));
        }
        if self.stop.len() > MAX_STOPS {
            return Err(format!("stop has {} words; at most {MAX_STOPS} are taken", self.stop.len()));
        }
        if let Some(bad) = self.stop.iter().find(|s| s.is_empty() || s.chars().count() > MAX_STOP_CHARS || s.chars().any(|c| c.is_control() && c != '\n')) {
            return Err(format!("a stop word of {} characters is empty, longer than {MAX_STOP_CHARS} or has a control character", bad.chars().count()));
        }
        Ok(Sampling {
            temperature: within("temperature", self.temperature, 0.0, 2.0)?,
            top_p: within("top_p", self.top_p, 0.0, 1.0)?,
            top_k: self.top_k,
            min_p: within("min_p", self.min_p, 0.0, 1.0)?,
            seed: self.seed,
            stop: self.stop.clone(),
            repeat_penalty: within("repeat_penalty", self.repeat_penalty, 0.5, 2.0)?,
        })
    }
}

/// The settings of the layers, a later one over an earlier one, field by field. A list of stop words replaces the one before it (it is not added to) when it is not empty.
pub fn layer(layers: &[&Sampling]) -> Sampling {
    let mut out = Sampling::default();
    for l in layers {
        out.temperature = l.temperature.or(out.temperature);
        out.top_p = l.top_p.or(out.top_p);
        out.top_k = l.top_k.or(out.top_k);
        out.min_p = l.min_p.or(out.min_p);
        out.seed = l.seed.or(out.seed);
        if !l.stop.is_empty() {
            out.stop = l.stop.clone();
        }
        out.repeat_penalty = l.repeat_penalty.or(out.repeat_penalty);
    }
    out
}

/// What the card of a model family recommends for talking, as far as the card was read. **Not measured by Aqua:** the numbers are the ones the Qwen3 card gives for its
/// non-thinking mode (temperature 0.7, top_p 0.8, top_k 20, min_p 0) [inferred from the published model card; verify before relying on them], and they are used for the roles that
/// speak to the person (chat and summary) and never for the planner, whose JSON wants the steadier 0.3. Used only with `AQUA_SAMPLING_PRESETS` on: changing what chat sounds like is
/// an experiment that needs the bake-off, not a default.
pub fn card_preset(family: &Family, role: Role) -> Sampling {
    match (family, role) {
        (Family::Qwen3, Role::Chat | Role::Summary) => Sampling { temperature: Some(0.7), top_p: Some(0.8), top_k: Some(20), min_p: Some(0.0), ..Sampling::default() },
        _ => Sampling::default(),
    }
}

/// The seed `AQUA_SEED` names, for runs that must be repeatable (an evaluation). `None` when it is not set (the default is `-1`, which is not a seed).
pub fn env_seed() -> Option<u64> {
    let n = flags::int(&flags::SEED);
    (n >= 0).then_some(n as u64)
}

/// Everything a call's request needs besides its words, for one model and one role: the layered sampling and what to do about thinking.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelSettings {
    pub sampling: Sampling,
    pub reasoning: ReasoningMode,
}

impl ModelSettings {
    /// Does this change the request the engine has always sent? (Nothing set and the server's own way of thinking: it does not.)
    pub fn is_default(&self) -> bool {
        self.sampling.is_empty() && self.reasoning == ReasoningMode::ServerDefault
    }
}

/// The fields of the server's body that settings set: `(the thinking switch for build_body, the fields to write over the body)`.
/// `ReasoningMode::NoSwitch` writes a null over `chat_template_kwargs`, which `build_body` takes to mean "leave that field out".
pub fn body_parts(sampling: &Sampling, reasoning: ReasoningMode) -> (Option<bool>, Map<String, Value>) {
    let mut m = Map::new();
    if let Some(v) = sampling.temperature {
        m.insert("temperature".into(), json!(v));
    }
    if let Some(v) = sampling.top_p {
        m.insert("top_p".into(), json!(v));
    }
    if let Some(v) = sampling.top_k {
        m.insert("top_k".into(), json!(v));
    }
    if let Some(v) = sampling.min_p {
        m.insert("min_p".into(), json!(v));
    }
    if let Some(v) = sampling.seed {
        m.insert("seed".into(), json!(v));
    }
    if !sampling.stop.is_empty() {
        m.insert("stop".into(), json!(sampling.stop));
    }
    if let Some(v) = sampling.repeat_penalty {
        m.insert("repeat_penalty".into(), json!(v));
    }
    let thinking = match reasoning {
        ReasoningMode::ServerDefault => None,
        ReasoningMode::Off => Some(false),
        ReasoningMode::On => Some(true),
        ReasoningMode::NoSwitch => {
            m.insert("chat_template_kwargs".into(), Value::Null);
            None
        }
    };
    (thinking, m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(json: &str) -> Result<Sampling, String> {
        serde_json::from_str::<SamplingSpec>(json).map_err(|e| e.to_string())?.checked()
    }

    #[test]
    fn a_layer_that_names_a_field_wins_and_the_rest_of_the_earlier_layers_stays() {
        let card = Sampling { temperature: Some(0.7), top_p: Some(0.8), top_k: Some(20), ..Sampling::default() };
        let profile = Sampling { temperature: Some(0.5), ..Sampling::default() };
        let role = Sampling { top_p: Some(0.95), stop: vec!["###".into()], ..Sampling::default() };
        let request = Sampling { stop: vec!["END".into()], seed: Some(7), ..Sampling::default() };
        let got = layer(&[&card, &profile, &role, &request]);
        assert_eq!(got.temperature, Some(0.5), "the profile over the card");
        assert_eq!(got.top_p, Some(0.95), "the role over the card");
        assert_eq!(got.top_k, Some(20), "what only the card names stays");
        assert_eq!(got.stop, vec!["END".to_string()], "a list of stop words is replaced, not added to");
        assert_eq!(got.seed, Some(7));
        assert_eq!(got.min_p, None);
        // An empty list of stop words does not wipe the one before it.
        let none = Sampling::default();
        assert_eq!(layer(&[&role, &none]).stop, vec!["###".to_string()]);
        // Nothing named anywhere: nothing at all.
        assert!(layer(&[&none, &none]).is_empty());
        assert!(layer(&[]).is_empty());
    }

    #[test]
    fn the_values_of_a_file_are_held_to_their_ranges_with_the_field_in_the_sentence() {
        assert!(spec(r#"{"temperature": 0.7, "top_p": 0.8, "top_k": 20, "min_p": 0, "seed": 42, "stop": ["END"], "repeat_penalty": 1.05}"#).is_ok());
        assert!(spec("{}").unwrap().is_empty());
        for (json, word) in [
            (r#"{"temperature": 2.5}"#, "temperature"),
            (r#"{"temperature": -0.1}"#, "temperature"),
            (r#"{"top_p": 1.5}"#, "top_p"),
            (r#"{"top_k": 5000}"#, "top_k"),
            (r#"{"min_p": 2}"#, "min_p"),
            (r#"{"repeat_penalty": 0.1}"#, "repeat_penalty"),
            (r#"{"stop": ["a","b","c","d","e"]}"#, "stop"),
            (r#"{"stop": [""]}"#, "stop word"),
        ] {
            let said = spec(json).unwrap_err();
            assert!(said.contains(word), "{json}: {said}");
        }
        let long = format!(r#"{{"stop": ["{}"]}}"#, "x".repeat(65));
        assert!(spec(&long).unwrap_err().contains("stop word"));
        // A field that is not known is named, so a misspelling is not silently ignored.
        let said = spec(r#"{"temprature": 0.2}"#).unwrap_err();
        assert!(said.contains("temprature"), "{said}");
        // A number that is not one.
        assert!(spec(r#"{"temperature": "hot"}"#).is_err());
    }

    #[test]
    fn the_card_preset_is_for_the_qwen3_roles_that_speak_and_not_for_the_planner() {
        let chat = card_preset(&Family::Qwen3, Role::Chat);
        assert_eq!((chat.temperature, chat.top_p, chat.top_k, chat.min_p), (Some(0.7), Some(0.8), Some(20), Some(0.0)));
        assert!(!card_preset(&Family::Qwen3, Role::Summary).is_empty());
        for role in [Role::Agent, Role::Decision, Role::Diagnosis, Role::Edit, Role::Fast] {
            assert!(card_preset(&Family::Qwen3, role).is_empty(), "{role:?}: the planner's JSON keeps its steady numbers");
        }
        assert!(card_preset(&Family::Llama3, Role::Chat).is_empty(), "no card was read for it, so nothing is made up");
        assert!(card_preset(&Family::Other("x".into()), Role::Chat).is_empty());
    }

    #[test]
    fn the_seed_comes_from_the_variable_and_only_when_it_is_a_whole_number_from_zero() {
        for (value, want) in [(None, None), (Some("-1"), None), (Some("0"), Some(0)), (Some("42"), Some(42)), (Some(" 7 "), Some(7)), (Some("seven"), None), (Some("1.5"), None), (Some(""), None)] {
            assert_eq!(flags::with_overrides(&[("AQUA_SEED", value)], env_seed), want, "AQUA_SEED = {value:?}");
        }
    }

    #[test]
    fn settings_that_say_nothing_are_the_default_and_anything_else_is_not() {
        assert!(ModelSettings::default().is_default());
        assert!(!ModelSettings { sampling: Sampling { seed: Some(1), ..Sampling::default() }, reasoning: ReasoningMode::ServerDefault }.is_default());
        assert!(!ModelSettings { sampling: Sampling::default(), reasoning: ReasoningMode::NoSwitch }.is_default());
    }

    #[test]
    fn the_body_fields_are_only_the_ones_named_and_no_switch_takes_the_template_field_out() {
        let (thinking, fields) = body_parts(&Sampling::default(), ReasoningMode::ServerDefault);
        assert_eq!((thinking, fields.len()), (None, 0), "nothing named, nothing written");
        let s = Sampling { temperature: Some(0.7), top_k: Some(20), seed: Some(5), stop: vec!["x".into()], ..Sampling::default() };
        let (thinking, fields) = body_parts(&s, ReasoningMode::Off);
        assert_eq!(thinking, Some(false));
        assert_eq!(Value::Object(fields), json!({"temperature": 0.7, "top_k": 20, "seed": 5, "stop": ["x"]}));
        assert_eq!(body_parts(&Sampling::default(), ReasoningMode::On).0, Some(true));
        let (thinking, fields) = body_parts(&Sampling::default(), ReasoningMode::NoSwitch);
        assert_eq!(thinking, None);
        assert_eq!(fields.get("chat_template_kwargs"), Some(&Value::Null));
    }
}
