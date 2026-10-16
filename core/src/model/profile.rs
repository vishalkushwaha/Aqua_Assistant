//! What Aqua knows about one model beyond its name: its family, its profile and what its header says (card OA9-03; task PR-04 of the OpenCode audit).
//!
//! **`data/models/profiles.json`** is the person's file of per-model settings. Nothing needs it: with no file every model is treated as it always was. It looks like this (the same text is
//! in `documentation/model_profiles.example.json`):
//!
//! ```json
//! { "version": 1,
//!   "profiles": [
//!     { "match": "Qwen3-8B-Q4_K_M.gguf", "gpu_layers": 22, "window": 6144, "swap_seconds": 25,
//!       "sampling": { "temperature": 0.3, "top_p": 0.9 }, "notes": "measured on a 4 GB card" },
//!     { "match": "llama-3*", "family": "llama3" } ] }
//! ```
//!
//! * `match` names a file (`Qwen3-8B-Q4_K_M.gguf`) or a group of them (`llama-3*`; `*` stands for any text, the comparison ignores case). The first entry that matches wins.
//! * `family` says what family the model is, when its name and its header do not (`qwen3`, `qwen3_coder`, `llama3`, `gemma3`, `mistral`, `phi`, `granite`, or any word).
//! * `sampling` is the model's own sampling, a layer under the role's (`sampling.rs`). **Used only with `AQUA_MODEL_PROFILES` on.**
//! * `roles` (beside `profiles`, not inside an entry) gives a role its own sampling whatever the model: `"roles": { "chat": { "temperature": 0.7, "top_p": 0.8 } }`. The role's layer is over
//!   the model's. Used only with `AQUA_MODEL_PROFILES` on, like the model's own.
//! * `swap_seconds` is how long the server takes to load the model, for the warning that two roles on different models make it load each in turn. Shown, never acted on.
//! * `tool_mode` is `"json"` (the planner's tool calls are held to a schema, as they always were) or `"native"` (the server's own tool calls, card OA9-06, `native_tools.rs`): the mode for that
//!   model, over the switch `AQUA_PLANNER_TOOL_MODE`. **Used only with `AQUA_MODEL_PROFILES` on.**
//! * `gpu_layers`, `window` and `kv_cache` are the launch plan of the model. **They are read, checked and shown, and not yet applied at launch:** the launch still uses `AQUA_LLAMA_NGL`
//!   and `AQUA_LLAMA_CTX` or its measured defaults (22 layers at a window of 6144 for the 8B on the 4 GB card), because a plan that is not the measured one has to pass
//!   `python scripts/eval/speed_gate.py` with Aqua closed first. The profile is where such a plan will be written down.
//!
//! The file is read once, when the engine starts (a change needs a restart, as for every config file). A file that cannot be read as JSON, is larger than 1 MiB or has another version
//! is not used at all, and the reason is kept; an entry that is wrong is left out with its number and the reason, and the others are used. Nothing in the file is ever executed.

use super::gguf::{self, GgufInfo};
use super::sampling::SamplingSpec;
use super::types::{Role, Sampling};
use super::Family;
use aqua_tool_host::config_text::{self, MAX_CONFIG_BYTES};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// The file's name, in the models folder.
pub const FILE: &str = "profiles.json";

/// The version of the file this reads.
pub const VERSION: u64 = 1;

/// The most entries a file may have.
pub const MAX_PROFILES: usize = 256;

/// One entry of the file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProfile {
    /// A file name, or a pattern with `*`.
    #[serde(rename = "match")]
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_layers: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kv_cache: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_seconds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling: Option<SamplingSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl ModelProfile {
    /// The rules an entry must meet, as a sentence for the first it does not.
    pub fn check(&self) -> Result<(), String> {
        let p = self.pattern.trim();
        if p.is_empty() || p.chars().count() > 200 {
            return Err("match is empty or longer than 200 characters".into());
        }
        if !p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | ' ' | '(' | ')' | '*')) {
            return Err("match may hold only letters, digits, . _ - + ( ) spaces and *".into());
        }
        if let Some(f) = &self.family {
            if f.trim().is_empty() || f.chars().count() > 40 || !f.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) {
                return Err("family is empty, longer than 40 characters or has a character other than a letter, digit, _ - .".into());
            }
        }
        if self.gpu_layers.is_some_and(|n| n > 999) {
            return Err("gpu_layers is more than 999".into());
        }
        if self.window.is_some_and(|n| !(512..=262_144).contains(&n)) {
            return Err("window is not between 512 and 262144 tokens".into());
        }
        if let Some(k) = &self.kv_cache {
            if !["f16", "q8_0", "q4_0"].contains(&k.trim().to_ascii_lowercase().as_str()) {
                return Err(format!("kv_cache {k:?} is not one of f16, q8_0, q4_0"));
            }
        }
        if self.swap_seconds.is_some_and(|n| n > 3600) {
            return Err("swap_seconds is more than 3600".into());
        }
        if let Some(mode) = &self.tool_mode {
            if !["json", "native"].contains(&mode.trim().to_ascii_lowercase().as_str()) {
                return Err(format!("tool_mode {mode:?} is not json or native"));
            }
        }
        if let Some(s) = &self.sampling {
            s.checked().map_err(|e| format!("sampling: {e}"))?;
        }
        if self.notes.as_ref().is_some_and(|n| n.chars().count() > 500) {
            return Err("notes is longer than 500 characters".into());
        }
        Ok(())
    }

    /// The model's own sampling layer (empty when the entry has none).
    pub fn sampling(&self) -> Sampling {
        self.sampling.as_ref().and_then(|s| s.checked().ok()).unwrap_or_default()
    }

    /// The tool mode the entry names, when it names one (`json` is the constrained JSON, `native` the server's own tool calls).
    pub fn tool_mode(&self) -> Option<super::ToolMode> {
        match self.tool_mode.as_deref().map(|m| m.trim().to_ascii_lowercase()).as_deref() {
            Some("native") => Some(super::ToolMode::Native),
            Some("json") => Some(super::ToolMode::ConstrainedJson),
            _ => None,
        }
    }

    /// The family the entry names, when it names one.
    pub fn family(&self) -> Option<Family> {
        self.family.as_deref().map(parse_family)
    }
}

/// A family from the word a profile or a person writes.
pub fn parse_family(word: &str) -> Family {
    let w = word.trim().to_ascii_lowercase().replace('-', "_");
    match w.as_str() {
        "qwen3" => Family::Qwen3,
        "qwen3_coder" | "qwen3coder" => Family::Qwen3Coder,
        "llama3" | "llama_3" => Family::Llama3,
        "gemma3" | "gemma_3" => Family::Gemma3,
        "mistral" => Family::Mistral,
        "phi" => Family::Phi,
        "granite" => Family::Granite,
        other => Family::Other(other.to_string()),
    }
}

/// The word for a family, as a profile writes it.
pub fn family_word(family: &Family) -> String {
    match family {
        Family::Qwen3 => "qwen3".into(),
        Family::Qwen3Coder => "qwen3_coder".into(),
        Family::Llama3 => "llama3".into(),
        Family::Gemma3 => "gemma3".into(),
        Family::Mistral => "mistral".into(),
        Family::Phi => "phi".into(),
        Family::Granite => "granite".into(),
        Family::Other(w) => w.clone(),
    }
}

/// Does `name` (a file name) match `pattern` (`*` is any run of characters, the case is ignored)?
pub fn pattern_matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.trim().to_lowercase().chars().collect();
    let name: Vec<char> = name.trim().to_lowercase().chars().collect();
    let (mut p, mut n, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while n < name.len() {
        if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            mark = n;
            p += 1;
        } else if p < pattern.len() && pattern[p] == name[n] {
            p += 1;
            n += 1;
        } else if let Some(s) = star {
            p = s + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// What the file says, and what was wrong with it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Profiles {
    entries: Vec<ModelProfile>,
    /// The sampling the file gives each role.
    roles: BTreeMap<Role, Sampling>,
    /// Why the file or an entry of it was not used: one sentence each, with no path.
    pub problems: Vec<String>,
    /// Was there a file?
    pub present: bool,
}

impl Profiles {
    /// The entries in the order of the file.
    pub fn entries(&self) -> &[ModelProfile] {
        &self.entries
    }

    /// The first entry that matches the file name.
    pub fn for_file(&self, filename: &str) -> Option<&ModelProfile> {
        self.entries.iter().find(|e| pattern_matches(&e.pattern, filename))
    }

    /// The sampling the file gives a role (empty when it gives none).
    pub fn for_role(&self, role: Role) -> Sampling {
        self.roles.get(&role).cloned().unwrap_or_default()
    }

    /// Reads the text of a file. Never fails: what cannot be used is left out and said in `problems`.
    pub fn parse(text: &str) -> Profiles {
        let mut out = Profiles { present: true, ..Profiles::default() };
        let whole = |why: String| Profiles { present: true, problems: vec![format!("{FILE} cannot be used ({why}), so every model keeps the built-in settings")], ..Profiles::default() };
        if text.len() > MAX_CONFIG_BYTES {
            return whole(format!("it is larger than {} KiB", MAX_CONFIG_BYTES / 1024));
        }
        let prepared = match config_text::prepare(text) {
            Ok(p) => p,
            Err(e) => return whole(e.detail()),
        };
        let value: Value = match serde_json::from_str(&prepared) {
            Ok(v) => v,
            Err(e) => return whole(format!("it is not valid JSON: {e}")),
        };
        let Some(top) = value.as_object() else { return whole("it is not a JSON object".into()) };
        match top.get("version") {
            None => {}
            Some(v) if v.as_u64() == Some(VERSION) => {}
            Some(v) => return whole(format!("its version is {v}, and only version {VERSION} is read")),
        }
        for note in config_text::unknown_keys(&prepared, &["version", "profiles", "roles"]) {
            out.problems.push(format!("{FILE}: {note}"));
        }
        match top.get("roles") {
            None => {}
            Some(Value::Object(roles)) => {
                for (name, raw) in roles {
                    let Some(role) = Role::parse(name) else {
                        out.problems.push(format!("{FILE}: roles has \"{name}\", which is not a role (the roles are {})", Role::ALL.map(Role::name).join(", ")));
                        continue;
                    };
                    match serde_json::from_value::<SamplingSpec>(raw.clone()).map_err(|e| e.to_string()).and_then(|s| s.checked()) {
                        Ok(sampling) => {
                            out.roles.insert(role, sampling);
                        }
                        Err(why) => out.problems.push(format!("{FILE}: the sampling of the role {name} is left out ({why})")),
                    }
                }
            }
            Some(_) => out.problems.push(format!("{FILE}: roles is not an object, so no role has sampling of its own")),
        }
        let list = match top.get("profiles") {
            None => return out,
            Some(Value::Array(a)) => a,
            Some(_) => {
                out.problems.push(format!("{FILE}: profiles is not a list, so no model has a profile"));
                return out;
            }
        };
        for (i, raw) in list.iter().enumerate() {
            let number = i + 1;
            if out.entries.len() >= MAX_PROFILES {
                out.problems.push(format!("{FILE}: only the first {MAX_PROFILES} profiles are read"));
                break;
            }
            let entry: ModelProfile = match serde_json::from_value(raw.clone()) {
                Ok(e) => e,
                Err(e) => {
                    out.problems.push(format!("{FILE}: profile {number} is left out ({e})"));
                    continue;
                }
            };
            if let Err(why) = entry.check() {
                out.problems.push(format!("{FILE}: profile {number} ({}) is left out ({why})", entry.pattern));
                continue;
            }
            out.entries.push(entry);
        }
        out
    }

    /// Reads the file at `path`: not there is no profiles and no problem.
    pub fn load(path: &Path) -> Profiles {
        use std::io::Read;
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Profiles::default(),
            Err(e) => return Profiles { present: true, problems: vec![format!("{FILE} cannot be read ({}), so every model keeps the built-in settings", e.kind())], ..Profiles::default() },
        };
        let mut bytes = Vec::new();
        if file.take(MAX_CONFIG_BYTES as u64 + 1).read_to_end(&mut bytes).is_err() {
            return Profiles { present: true, problems: vec![format!("{FILE} cannot be read, so every model keeps the built-in settings")], ..Profiles::default() };
        }
        match String::from_utf8(bytes) {
            Ok(text) => Profiles::parse(&text),
            Err(_) => Profiles { present: true, problems: vec![format!("{FILE} is not valid UTF-8 text, so every model keeps the built-in settings")], ..Profiles::default() },
        }
    }
}

/// The family of a model: what the profile says, else what the header's architecture says for the architectures that name a family, else what the file name says.
pub fn family_for(filename: &str, header: Option<&GgufInfo>, profile: Option<&ModelProfile>) -> Family {
    if let Some(family) = profile.and_then(ModelProfile::family) {
        return family;
    }
    if let Some(family) = header.and_then(|h| gguf::family_of_architecture(&h.architecture)) {
        return family;
    }
    Family::from_filename(filename)
}

/// How many layers a card with `free_mib` of free memory can hold, and what is left over, for a model of `info` at a window of `window` tokens. `margin_mib` is kept free for the
/// desktop and the buffers the server makes (about 0.5 GiB in use).
///
/// `layers = min(block_count, floor((free - margin - cache of the layers) / (weight of a layer)))`, solved for the layers whose weight and whose cache both go on the card. The weight
/// of a layer is [`GgufInfo::bytes_per_layer`] (an estimate from the file's size), the cache at 16 bits a number. **This is arithmetic on estimates**: it is for showing what a model
/// would need, and for the plan a profile will carry once the speed gate has been run on it. It is not what the launch uses. For the 8B on the 4 GB card the measured answer is 22
/// layers at 6144 tokens; this arithmetic says 19 or 20 with the usual margin, which is the sort of difference that a measurement settles and a formula does not.
pub fn plan_layers(info: &GgufInfo, window: u32, free_mib: u64, margin_mib: u64) -> Option<u32> {
    let layers = info.block_count?;
    let weight = info.bytes_per_layer()?;
    let cache = info.kv_bytes(1, window, 2)?;
    let per_layer = weight.checked_add(cache)?.max(1);
    let free = free_mib.saturating_sub(margin_mib).checked_mul(1024 * 1024)?;
    Some(((free / per_layer).min(u64::from(layers))) as u32)
}

/// Everything one model's line in the list says besides its name and size: for `GET /api/v1/llama/models`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ModelFacts {
    pub family: String,
    /// "qwen3" or "generic": the recipe the model is spoken to by when `AQUA_MODEL_PROFILES` is on.
    pub adapter: &'static str,
    /// What the header says; absent when it could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<GgufInfo>,
    /// Why the header could not be read, in a sentence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header_problem: Option<String>,
    /// The tokens one request can use at the window the server runs with (the window over the slots).
    pub window: u32,
    /// Is that window longer than the model was trained for? (The answers get worse past it.)
    pub window_over_trained: bool,
    /// The bytes the cache of all the layers takes at that window, at 16 bits a number (an estimate).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_bytes_at_window: Option<u64>,
    /// The layers the launch puts on a graphics card when `AQUA_LLAMA_NGL` is not set, never more than the model has. (On the processor build it is 0.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layers_on_card_by_default: Option<u32>,
    /// The entry of `profiles.json` that matches, as it was written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<ModelProfile>,
    /// What a person should know about this model: a window past its training, a mixture of experts, a file that is one part of a split model, a header that could not be read.
    pub notes: Vec<String>,
}

impl ModelFacts {
    /// The facts of the file at `path`. `window_total` and `slots` are the server's window and its slot count; `default_layers` the layers its launch puts on a card.
    pub fn of(path: &Path, profiles: &Profiles, window_total: u32, slots: u32, default_layers: u32) -> ModelFacts {
        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let profile = profiles.for_file(filename);
        let (header, header_problem) = match gguf::read_cached(path) {
            Ok(h) => (Some(h), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let family = family_for(filename, header.as_ref(), profile);
        let window = window_total / slots.max(1);
        let trained = header.as_ref().and_then(|h| h.context_length);
        let window_over_trained = trained.is_some_and(|t| u64::from(window) > t);
        let cache_bytes_at_window = header.as_ref().and_then(|h| h.kv_bytes(h.block_count?, window, 2));
        let layers_on_card_by_default = header.as_ref().and_then(|h| h.block_count).map(|n| n.min(default_layers));
        let mut notes = Vec::new();
        if let Some(why) = &header_problem {
            notes.push(format!("its header could not be read: {why}"));
        }
        if let (true, Some(t)) = (window_over_trained, trained) {
            notes.push(format!("Aqua runs it at a window of {window} tokens and it was trained for {t}"));
        }
        if header.as_ref().is_some_and(GgufInfo::is_moe) {
            notes.push("a mixture of experts: the weight of a layer is not even, so the layers a card can hold are not worked out from the size".into());
        }
        if let Some((no, count)) = header.as_ref().and_then(|h| h.split) {
            notes.push(format!("part {} of {count} of a model split over several files; the others must be next to it, and the sizes shown are of this part", no + 1));
        }
        ModelFacts { family: family_word(&family), adapter: super::adapter::adapter_for(&family).name(), header, header_problem, window, window_over_trained, cache_bytes_at_window, layers_on_card_by_default, profile: profile.cloned(), notes }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> GgufInfo {
        // The numbers of the real Qwen3-8B Q4_K_M: 36 layers, 4096 wide, 8 cache heads of 128, a file of 5.03 GB.
        GgufInfo {
            version: 3,
            tensor_count: 399,
            pair_count: 30,
            file_bytes: 5_027_783_488,
            architecture: "qwen3".into(),
            name: Some("Qwen3 8B".into()),
            size_label: Some("8B".into()),
            file_type: Some(15),
            quant: Some("Q4_K_M".into()),
            context_length: Some(40960),
            block_count: Some(36),
            embedding_length: Some(4096),
            head_count: Some(32),
            head_count_kv: Some(8),
            key_length: Some(128),
            value_length: Some(128),
            expert_count: None,
            vocab_size: Some(151_936),
            has_chat_template: true,
            split: None,
        }
    }

    #[test]
    fn a_pattern_is_a_name_or_a_name_with_stars_and_the_case_does_not_matter() {
        assert!(pattern_matches("Qwen3-8B-Q4_K_M.gguf", "qwen3-8b-q4_k_m.GGUF"));
        assert!(pattern_matches("llama-3*", "Llama-3.1-8B-Instruct-Q4_K_M.gguf"));
        assert!(pattern_matches("*8b*", "Qwen3-8B-Q4_K_M.gguf"));
        assert!(pattern_matches("*", "anything.gguf"));
        assert!(pattern_matches("a*b*c", "aXXbYYc"));
        assert!(!pattern_matches("a*b*c", "aXXbYY"));
        assert!(!pattern_matches("llama-3*", "qwen3.gguf"));
        assert!(!pattern_matches("qwen3", "qwen3.gguf"), "no star, so the whole name has to be the same");
        assert!(pattern_matches("**x", "ax"));
        assert!(!pattern_matches("", "x.gguf"));
        assert!(pattern_matches("", ""));
    }

    #[test]
    fn a_good_file_is_read_and_the_first_matching_entry_wins() {
        let text = r#"{ "version": 1, "profiles": [
            { "match": "Qwen3-8B-Q4_K_M.gguf", "gpu_layers": 22, "window": 6144, "swap_seconds": 25, "kv_cache": "f16", "sampling": { "temperature": 0.3, "top_p": 0.9 }, "notes": "measured" },
            { "match": "qwen3*", "family": "qwen3_coder" },
            { "match": "llama-3*", "family": "llama3" } ] }"#;
        let p = Profiles::parse(text);
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        assert_eq!(p.entries().len(), 3);
        let e = p.for_file("qwen3-8b-q4_k_m.gguf").unwrap();
        assert_eq!((e.gpu_layers, e.window, e.swap_seconds), (Some(22), Some(6144), Some(25)));
        assert_eq!(e.sampling().temperature, Some(0.3));
        assert_eq!(p.for_file("Qwen3-4B.gguf").unwrap().family(), Some(Family::Qwen3Coder), "the first that matches wins, and this is the second entry");
        assert_eq!(p.for_file("Llama-3.2-3B.gguf").unwrap().family(), Some(Family::Llama3));
        assert!(p.for_file("gemma-3.gguf").is_none());
    }

    #[test]
    fn comments_and_a_trailing_comma_are_accepted_as_in_the_other_config_files() {
        let text = "// my models\n{ \"profiles\": [ { \"match\": \"a.gguf\", \"family\": \"phi\", }, ], }";
        let p = Profiles::parse(text);
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        assert_eq!(p.for_file("a.gguf").unwrap().family(), Some(Family::Phi));
    }

    #[test]
    fn a_file_that_cannot_be_used_is_not_used_at_all_and_says_why() {
        for (text, word) in [
            ("not json at all", "not valid JSON"),
            ("[1, 2]", "not a JSON object"),
            (r#"{"version": 2, "profiles": []}"#, "version"),
            ("", "not valid JSON"),
        ] {
            let p = Profiles::parse(text);
            assert!(p.entries().is_empty(), "{text}");
            assert_eq!(p.problems.len(), 1, "{text}: {:?}", p.problems);
            assert!(p.problems[0].contains(word), "{text}: {}", p.problems[0]);
            assert!(p.problems[0].contains("built-in settings"), "{}", p.problems[0]);
            assert!(p.present);
        }
        let huge = format!("{{\"profiles\": [], \"x\": \"{}\"}}", "a".repeat(MAX_CONFIG_BYTES));
        assert!(Profiles::parse(&huge).problems[0].contains("larger than"));
    }

    #[test]
    fn a_role_can_have_sampling_of_its_own_and_a_wrong_one_is_left_out_with_the_reason() {
        let text = r#"{ "roles": { "chat": { "temperature": 0.7, "top_p": 0.8 }, "agent": { "temperature": 7 }, "nobody": { "top_k": 3 }, "fast": { "tempo": 1 } }, "profiles": [ { "match": "a.gguf" } ] }"#;
        let p = Profiles::parse(text);
        assert_eq!(p.for_role(Role::Chat).temperature, Some(0.7));
        assert!(p.for_role(Role::Agent).is_empty(), "a temperature of 7 is not taken");
        assert!(p.for_role(Role::Fast).is_empty());
        assert_eq!(p.problems.len(), 3, "{:#?}", p.problems);
        assert!(p.problems.iter().any(|m| m.contains("agent") && m.contains("temperature")));
        assert!(p.problems.iter().any(|m| m.contains("nobody") && m.contains("not a role")));
        assert!(p.problems.iter().any(|m| m.contains("fast") && m.contains("tempo")));
        assert_eq!(p.entries().len(), 1, "the profiles are read all the same");
        let p = Profiles::parse(r#"{ "roles": [1], "profiles": 5 }"#);
        assert_eq!(p.problems.len(), 2);
        assert!(p.entries().is_empty());
    }

    #[test]
    fn an_entry_that_is_wrong_is_left_out_with_its_number_and_the_rest_is_used() {
        let text = r#"{ "profiles": [
            { "match": "a.gguf", "gpu_layers": 20 },
            { "match": "b.gguf", "gpu_layers": 5000 },
            { "match": "c.gguf", "sampling": { "temperature": 9 } },
            { "match": "d.gguf", "kv_cache": "q3" },
            { "match": "e.gguf", "windw": 4096 },
            { "match": "../evil/path.gguf" },
            { "match": "" },
            { "family": "phi" },
            { "match": "f.gguf", "window": 100 },
            { "match": "g.gguf", "swap_seconds": 99999 },
            { "match": "h.gguf", "family": "bad family!" },
            { "match": "ok.gguf" } ] }"#;
        let p = Profiles::parse(text);
        let kept: Vec<&str> = p.entries().iter().map(|e| e.pattern.as_str()).collect();
        assert_eq!(kept, vec!["a.gguf", "ok.gguf"]);
        assert_eq!(p.problems.len(), 10, "{:#?}", p.problems);
        assert!(p.problems.iter().any(|m| m.contains("profile 2 (b.gguf)") && m.contains("gpu_layers")), "{:?}", p.problems);
        assert!(p.problems.iter().any(|m| m.contains("profile 3 (c.gguf)") && m.contains("temperature")));
        assert!(p.problems.iter().any(|m| m.contains("profile 4 (d.gguf)") && m.contains("kv_cache")));
        assert!(p.problems.iter().any(|m| m.contains("profile 5") && m.contains("windw")), "a misspelled field is named");
        assert!(p.problems.iter().any(|m| m.contains("profile 6") && m.contains("match may hold only")));
        assert!(p.problems.iter().any(|m| m.contains("profile 8")), "an entry with no match");
        for m in &p.problems {
            assert!(!m.contains(":\\") && !m.contains(":/"), "no path in {m}");
        }
    }

    #[test]
    fn a_top_level_key_that_is_not_known_is_named_and_the_file_is_still_used() {
        let p = Profiles::parse(r#"{ "profile": [], "profiles": [ { "match": "a.gguf" } ] }"#);
        assert_eq!(p.entries().len(), 1);
        assert_eq!(p.problems.len(), 1);
        assert!(p.problems[0].contains("profile"), "{:?}", p.problems);
    }

    #[test]
    fn only_the_first_entries_up_to_the_limit_are_read() {
        let items: Vec<String> = (0..MAX_PROFILES + 5).map(|i| format!("{{\"match\": \"m{i}.gguf\"}}")).collect();
        let p = Profiles::parse(&format!("{{\"profiles\": [{}]}}", items.join(",")));
        assert_eq!(p.entries().len(), MAX_PROFILES);
        assert!(p.problems.iter().any(|m| m.contains("only the first")));
    }

    #[test]
    fn a_file_is_loaded_by_path_and_one_that_is_not_there_is_nothing_and_no_problem() {
        use crate::test_support::TempWorld;
        let world = TempWorld::new("profiles_load");
        let none = Profiles::load(&world.path("profiles.json"));
        assert!(!none.present && none.entries().is_empty() && none.problems.is_empty());
        let path = world.write("profiles.json", r#"{"profiles": [{"match": "a.gguf", "gpu_layers": 12}]}"#);
        let p = Profiles::load(&path);
        assert!(p.present);
        assert_eq!(p.for_file("a.gguf").unwrap().gpu_layers, Some(12));
        let bad = world.write("bad.json", [0xff, 0xfe, 0x00]);
        assert!(Profiles::load(&bad).problems[0].contains("UTF-8"));
        // A folder where the file should be is unreadable and says so.
        std::fs::create_dir_all(world.path("dir.json")).unwrap();
        let unreadable = Profiles::load(&world.path("dir.json"));
        assert!(unreadable.present && !unreadable.problems.is_empty());
    }

    #[test]
    fn the_family_is_the_profiles_then_the_headers_then_the_file_names() {
        let h = info();
        assert_eq!(family_for("model.gguf", Some(&h), None), Family::Qwen3, "a file with no telling name but a qwen3 header");
        assert_eq!(family_for("Llama-3-8B.gguf", None, None), Family::Llama3);
        let profile = ModelProfile { pattern: "model.gguf".into(), family: Some("granite".into()), ..ModelProfile::default() };
        assert_eq!(family_for("model.gguf", Some(&h), Some(&profile)), Family::Granite, "the person's word is first");
        let no_family = ModelProfile { pattern: "model.gguf".into(), ..ModelProfile::default() };
        assert_eq!(family_for("model.gguf", Some(&h), Some(&no_family)), Family::Qwen3);
        let llama = GgufInfo { architecture: "llama".into(), ..info() };
        assert_eq!(family_for("Mistral-7B.gguf", Some(&llama), None), Family::Mistral, "a llama architecture does not decide: the name does");
        assert_eq!(family_for("qwen-0.5b-instruct-q4_k_m.gguf", None, None), Family::Other("qwen".into()), "Qwen2.5 is not Qwen3");
        for word in ["qwen3", "qwen3_coder", "llama3", "gemma3", "mistral", "phi", "granite", "falcon"] {
            assert_eq!(family_word(&parse_family(word)), word);
        }
        assert_eq!(parse_family("Qwen3-Coder"), Family::Qwen3Coder);
    }

    #[test]
    fn the_layers_a_card_holds_follow_from_the_sizes_and_stop_at_the_models_own_count() {
        let i = info();
        // The weight of a layer: 5,027,783,488 / 37 = 135,886,040 bytes; the cache of one layer at 6144 tokens: 2048 x 6144 x 2 = 25,165,824. Together 161,051,864.
        // A card with 3,561 MiB free and 512 MiB kept back has 3,049 MiB = 3,197,108,224 bytes: 19 whole layers.
        assert_eq!(plan_layers(&i, 6144, 3561, 512), Some(19));
        // Plenty of room: all 36, and no more.
        assert_eq!(plan_layers(&i, 6144, 24_000, 512), Some(36));
        // No room: none.
        assert_eq!(plan_layers(&i, 6144, 400, 512), Some(0));
        // A smaller window leaves room for a layer or two more.
        assert!(plan_layers(&i, 2048, 3561, 512).unwrap() > 19);
        // What the header does not say is not guessed.
        let mut blank = i.clone();
        blank.block_count = None;
        assert_eq!(plan_layers(&blank, 6144, 3561, 512), None);
        blank = i.clone();
        blank.file_bytes = 0;
        assert_eq!(plan_layers(&blank, 6144, 3561, 512), None);
        blank = i.clone();
        blank.head_count_kv = None;
        blank.head_count = None;
        assert_eq!(plan_layers(&blank, 6144, 3561, 512), None);
    }

    #[test]
    fn the_example_file_in_the_documentation_is_read_without_a_problem_and_says_what_the_page_says() {
        // The file a person copies to data/models/profiles.json: it must be one the loader takes whole, comments and all.
        let p = Profiles::parse(include_str!("../../../documentation/model_profiles.example.json"));
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        assert_eq!(p.entries().len(), 2);
        let baseline = p.for_file("Qwen3-8B-Q4_K_M.gguf").unwrap();
        assert_eq!((baseline.gpu_layers, baseline.window, baseline.swap_seconds), (Some(22), Some(6144), Some(25)), "the entry for the baseline model says what the launch already does");
        assert_eq!((baseline.sampling().temperature, baseline.sampling().top_p), (Some(0.3), Some(0.9)), "and the numbers Aqua has always sent");
        assert_eq!(p.for_file("my-llama-finetune-Q4.gguf").unwrap().family(), Some(Family::Llama3));
        assert_eq!(p.for_role(Role::Chat).top_k, Some(20));
    }

    /// A header written the way a model file has it, for the facts of a file.
    fn write_model(world: &crate::test_support::TempWorld, name: &str, window_trained: u32, blocks: u32) -> std::path::PathBuf {
        fn s(v: &str) -> Vec<u8> {
            let mut o = (v.len() as u64).to_le_bytes().to_vec();
            o.extend(v.as_bytes());
            o
        }
        fn kv32(k: &str, v: u32) -> Vec<u8> {
            let mut o = s(k);
            o.extend(4u32.to_le_bytes());
            o.extend(v.to_le_bytes());
            o
        }
        let mut body = s("general.architecture");
        body.extend(8u32.to_le_bytes());
        body.extend(s("qwen3"));
        body.extend(kv32("qwen3.context_length", window_trained));
        body.extend(kv32("qwen3.block_count", blocks));
        body.extend(kv32("qwen3.embedding_length", 4096));
        body.extend(kv32("qwen3.attention.head_count", 32));
        body.extend(kv32("qwen3.attention.head_count_kv", 8));
        let mut bytes = b"GGUF".to_vec();
        bytes.extend(3u32.to_le_bytes());
        bytes.extend(0u64.to_le_bytes());
        bytes.extend(6u64.to_le_bytes());
        bytes.extend(body);
        bytes.extend(vec![0u8; 1_000_000]);
        world.write(name, bytes)
    }

    #[test]
    fn the_facts_of_a_model_come_from_its_header_its_profile_and_the_servers_window() {
        use crate::test_support::TempWorld;
        let world = TempWorld::new("model_facts");
        let path = write_model(&world, "Mystery-8B.gguf", 4096, 36);
        let profiles = Profiles::parse(r#"{"profiles": [{"match": "mystery*", "swap_seconds": 30, "notes": "mine"}]}"#);
        let facts = ModelFacts::of(&path, &profiles, 6144, 1, 22);
        assert_eq!(facts.family, "qwen3", "the header's architecture, not the name");
        assert_eq!(facts.adapter, "qwen3");
        assert_eq!(facts.window, 6144);
        assert!(facts.window_over_trained);
        assert!(facts.notes.iter().any(|n| n.contains("6144") && n.contains("4096")), "{:?}", facts.notes);
        assert_eq!(facts.layers_on_card_by_default, Some(22));
        assert_eq!(facts.cache_bytes_at_window, Some(905_969_664));
        assert_eq!(facts.profile.as_ref().and_then(|p| p.swap_seconds), Some(30));
        assert!(facts.header.as_ref().is_some_and(|h| h.block_count == Some(36)));
        // A model with fewer layers than the launch puts on the card has them all there; a window over the slots divides.
        let small = write_model(&world, "Small.gguf", 40960, 16);
        let facts = ModelFacts::of(&small, &Profiles::default(), 6144, 2, 22);
        assert_eq!((facts.window, facts.window_over_trained, facts.layers_on_card_by_default), (3072, false, Some(16)));
        // A file that is not a model: no header, and the reason is in the notes, with no path.
        let junk = world.write("junk.gguf", "this is only text that was given a model's name, and it is long enough");
        let facts = ModelFacts::of(&junk, &Profiles::default(), 6144, 1, 22);
        assert!(facts.header.is_none() && facts.header_problem.as_deref().is_some_and(|p| p.contains("not a GGUF")));
        assert!(facts.notes.iter().any(|n| n.starts_with("its header could not be read")));
        assert!(!facts.notes.iter().any(|n| n.contains(&world.root.display().to_string())));
        let json = serde_json::to_value(&facts).unwrap();
        assert!(json.get("header").is_none(), "an absent header is left out of the answer");
    }
}
