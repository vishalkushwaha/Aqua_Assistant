//! Which model answers which role, set by the person and kept in `data/models/router.json` (card OA9-03; task PR-05 of the OpenCode audit).
//!
//! **The bug this fixes (D-PR-2).** The model screen has a "Use" button, and pressing it said "Active model is X". But the agent's planner and the chat (which follows the planner so that
//! the server does not swap models between turns) were chosen by `prefer_agent_model_filename`: a Qwen3-8B Q4_K_M file, or any Qwen3 8B file, in the folder won over the model the
//! person had just chosen, unless the process had `AQUA_AGENT_MODEL` set. A person who imported a bigger or better model and pressed Use was told it was in use while Aqua went on
//! with the Qwen3-8B, and the results and benchmarks that followed were attributed to the wrong model.
//!
//! Now there is a table of roles ([`Role`]: agent, chat, edit, fast, decision, diagnosis, summary) and a rule for each role's model, in this order:
//!
//! 1. **`AQUA_AGENT_MODEL`** names the agent's model, as it always did (and what follows the agent follows it). It is the one thing that wins over the person's choice, because it is set
//!    on the process, and the answer says so when it hides a choice the person made.
//! 2. **Set by you**: the file the person gave this role (the Use button gives it to the agent and the chat; the route `PUT /api/v1/models/roles` to any role). It must be in the model
//!    folder: if it was deleted since, the role goes back to the default and says why.
//! 3. **The default rule**, for a role nobody set: the agent gets the Qwen3-8B preference (Qwen3-8B-Q4_K_M, else any Qwen3 8B file) and the active model when there is none; every other
//!    role (chat included) **follows the agent**, because a role on another model makes the server unload one and load the other.
//!
//! With no `router.json` every role resolves exactly as before this card. The file is written by the core (atomically) and holds only what the person chose; it is not a place for
//! comments (the sampling a person writes by hand is in `profiles.json`).
//!
//! Only a model on this machine can be given a role: a model of another provider needs the remote policy, which is not built, so `PUT` refuses it with that reason.

use super::types::Role;
use aqua_tool_host::config_text::{self, MAX_CONFIG_BYTES};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

/// The file's name, in the models folder.
pub const FILE: &str = "router.json";

/// The version of the file this reads and writes.
pub const VERSION: u64 = 1;

/// The provider of the models in the folder, as a [`super::ModelRef`] writes it.
pub const LOCAL_PROVIDER: &str = super::llama_server_provider::ID;

/// The roles that have a caller today, so that the warning about two models does not count a role nothing asks for: the planner and the flows (agent), chat, the decision router and
/// the utility rewrite (fast). Edit, diagnosis and summary follow the agent until a caller asks for them by role.
pub const CALLED_ROLES: [Role; 4] = [Role::Agent, Role::Chat, Role::Decision, Role::Fast];

/// What the person has chosen: a file name for a role.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RoleAssignments {
    models: BTreeMap<Role, String>,
    /// Why the file or a line of it was not used: one sentence each, with no path.
    pub problems: Vec<String>,
    /// Was there a file?
    pub present: bool,
}

/// The file name in a model reference: `llama.cpp/X.gguf` and `X.gguf` are the same model; one of another provider is not one this machine has.
fn local_file(reference: &str) -> Result<&str, String> {
    let r = reference.trim();
    match r.split_once('/') {
        None => Ok(r),
        Some((provider, file)) if provider.eq_ignore_ascii_case(LOCAL_PROVIDER) => Ok(file.trim()),
        Some((provider, _)) => Err(format!("{provider} is not a provider on this machine: only models in the model folder can be given a role, since a call to another machine needs the remote policy, which is not built")),
    }
}

impl RoleAssignments {
    /// The file name given to a role, if any.
    pub fn get(&self, role: Role) -> Option<&str> {
        self.models.get(&role).map(String::as_str)
    }

    /// Gives a role a file name, or takes the choice away (`None`: the role goes back to the default rule).
    pub fn set(&mut self, role: Role, file: Option<String>) {
        match file {
            Some(f) => {
                self.models.insert(role, f);
            }
            None => {
                self.models.remove(&role);
            }
        }
    }

    /// The roles that have a choice, in the order of [`Role::ALL`].
    pub fn assigned(&self) -> Vec<(Role, &str)> {
        Role::ALL.iter().filter_map(|r| self.models.get(r).map(|m| (*r, m.as_str()))).collect()
    }

    /// Reads the text of a file. Never fails: what cannot be used is left out and said in `problems`.
    pub fn parse(text: &str) -> RoleAssignments {
        let mut out = RoleAssignments { present: true, ..RoleAssignments::default() };
        let broken = |why: String| RoleAssignments { present: true, problems: vec![format!("{FILE} cannot be used ({why}), so every role has the default model")], ..RoleAssignments::default() };
        if text.len() > MAX_CONFIG_BYTES {
            return broken(format!("it is larger than {} KiB", MAX_CONFIG_BYTES / 1024));
        }
        let prepared = match config_text::prepare(text) {
            Ok(p) => p,
            Err(e) => return broken(e.detail()),
        };
        let value: Value = match serde_json::from_str(&prepared) {
            Ok(v) => v,
            Err(e) => return broken(format!("it is not valid JSON: {e}")),
        };
        let Some(top) = value.as_object() else { return broken("it is not a JSON object".into()) };
        match top.get("version") {
            None => {}
            Some(v) if v.as_u64() == Some(VERSION) => {}
            Some(v) => return broken(format!("its version is {v}, and only version {VERSION} is read")),
        }
        for note in config_text::unknown_keys(&prepared, &["version", "roles"]) {
            out.problems.push(format!("{FILE}: {note}"));
        }
        match top.get("roles") {
            None => {}
            Some(Value::Object(roles)) => {
                for (name, raw) in roles {
                    let Some(role) = Role::parse(name) else {
                        out.problems.push(format!("{FILE}: \"{name}\" is not a role (the roles are {})", Role::ALL.map(Role::name).join(", ")));
                        continue;
                    };
                    match raw.as_str().map(local_file) {
                        Some(Ok(file)) if is_plain_file_name(file) => {
                            out.models.insert(role, file.to_string());
                        }
                        Some(Ok(_)) => out.problems.push(format!("{FILE}: the model of the role {name} is not a file name of the model folder")),
                        Some(Err(why)) => out.problems.push(format!("{FILE}: the model of the role {name} is left out ({why})")),
                        None => out.problems.push(format!("{FILE}: the model of the role {name} is not text")),
                    }
                }
            }
            Some(_) => out.problems.push(format!("{FILE}: roles is not an object, so no role has a model of its own")),
        }
        out
    }

    /// Reads the file at `path`: not there is no choices and no problem.
    pub fn load(path: &Path) -> RoleAssignments {
        use std::io::Read;
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return RoleAssignments::default(),
            Err(e) => return RoleAssignments { present: true, problems: vec![format!("{FILE} cannot be read ({}), so every role has the default model", e.kind())], ..RoleAssignments::default() },
        };
        let mut bytes = Vec::new();
        if file.take(MAX_CONFIG_BYTES as u64 + 1).read_to_end(&mut bytes).is_err() {
            return RoleAssignments { present: true, problems: vec![format!("{FILE} cannot be read, so every role has the default model")], ..RoleAssignments::default() };
        }
        match String::from_utf8(bytes) {
            Ok(text) => RoleAssignments::parse(&text),
            Err(_) => RoleAssignments { present: true, problems: vec![format!("{FILE} is not valid UTF-8 text, so every role has the default model")], ..RoleAssignments::default() },
        }
    }

    /// The file's text: the version and the models, each as `llama.cpp/<file>`.
    pub fn to_json(&self) -> String {
        let roles: serde_json::Map<String, Value> = self.assigned().into_iter().map(|(r, m)| (r.name().to_string(), json!(format!("{LOCAL_PROVIDER}/{m}")))).collect();
        serde_json::to_string_pretty(&json!({ "version": VERSION, "roles": roles })).unwrap_or_default() + "\n"
    }

    /// Writes the file, whole or not at all.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        config_text::write_atomic(path, self.to_json().as_bytes())
    }
}

/// A name that stays inside the model folder: one `.gguf` file name with no folder part.
pub fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && !name.contains(['/', '\\', ':']) && !name.contains("..") && name.to_ascii_lowercase().ends_with(".gguf") && !name.chars().any(char::is_control)
}

/// The words of a person's request for a model: `X.gguf` or `llama.cpp/X.gguf`, and a reason in a sentence when it cannot be a model of a role.
pub fn parse_reference(reference: &str) -> Result<String, String> {
    let file = local_file(reference)?;
    if !is_plain_file_name(file) {
        return Err(format!("{reference:?} is not the name of a .gguf file in the model folder"));
    }
    Ok(file.to_string())
}

/// Where a role's model came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `AQUA_AGENT_MODEL`.
    Environment,
    /// The person chose it.
    SetByYou,
    /// No one chose: the Qwen3-8B preference.
    DefaultRule,
    /// No one chose and there is no Qwen3 8B file: the active model.
    ActiveModel,
    /// No one chose: it is the agent's model.
    FollowsAgent,
}

impl Source {
    pub fn word(self) -> &'static str {
        match self {
            Source::Environment => "environment",
            Source::SetByYou => "set_by_you",
            Source::DefaultRule => "default_rule",
            Source::ActiveModel => "active_model",
            Source::FollowsAgent => "follows_agent",
        }
    }
}

/// What the resolver needs to know of the machine.
pub struct Inputs<'a> {
    /// The `.gguf` files in the model folder.
    pub installed: &'a [String],
    /// The file the Use button last chose as the active model.
    pub active: &'a str,
    /// `AQUA_AGENT_MODEL`, when it is set.
    pub env_agent: Option<&'a str>,
    /// The Qwen3-8B preference: the file it picks among the installed ones, if there is one.
    pub preferred: Option<&'a str>,
}

/// The model of one role and why.
#[derive(Clone, Debug, PartialEq)]
pub struct RoleChoice {
    pub role: Role,
    /// The file name.
    pub model: String,
    pub source: Source,
    /// One sentence for the person.
    pub why: String,
    /// What is wrong with a choice that was made (the file is gone; the environment hides it).
    pub note: Option<String>,
}

/// A role's choice as the routes tell it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoleView {
    pub role: String,
    /// Where the model runs: always the local server for now.
    pub provider: String,
    /// The file name.
    pub model: String,
    /// `environment`, `set_by_you`, `default_rule`, `active_model` or `follows_agent`.
    pub source: String,
    pub why: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl RoleChoice {
    pub fn view(&self) -> RoleView {
        RoleView { role: self.role.name().to_string(), provider: LOCAL_PROVIDER.to_string(), model: self.model.clone(), source: self.source.word().to_string(), why: self.why.clone(), note: self.note.clone() }
    }
}

fn installed_as<'a>(installed: &'a [String], name: &str) -> Option<&'a str> {
    installed.iter().map(String::as_str).find(|f| f.eq_ignore_ascii_case(name))
}

/// The model of `role`, by the rule in the module's header.
pub fn resolve(role: Role, assigned: &RoleAssignments, i: &Inputs<'_>) -> RoleChoice {
    let make = |model: &str, source: Source, why: String, note: Option<String>| RoleChoice { role, model: model.to_string(), source, why, note };
    let mine = assigned.get(role);
    if role == Role::Agent {
        if let Some(file) = i.env_agent.and_then(|e| installed_as(i.installed, e)) {
            let note = mine.filter(|m| !m.eq_ignore_ascii_case(file)).map(|m| format!("{m} was chosen for this role and is not used while AQUA_AGENT_MODEL is set"));
            return make(file, Source::Environment, "AQUA_AGENT_MODEL names it".to_string(), note);
        }
    }
    let mut note = None;
    if let Some(file) = mine {
        match installed_as(i.installed, file) {
            Some(found) => return make(found, Source::SetByYou, "you chose it".to_string(), None),
            None => note = Some(format!("{file} was chosen for this role and is no longer in the model folder, so the default is used")),
        }
    }
    if role == Role::Agent {
        return match i.preferred {
            Some(file) => make(file, Source::DefaultRule, "the Qwen3-8B file in the folder is preferred for the agent when nothing is chosen".to_string(), note),
            None => make(i.active, Source::ActiveModel, "no Qwen3 8B file is in the folder, so the active model is used".to_string(), note),
        };
    }
    let agent = resolve(Role::Agent, assigned, i);
    make(&agent.model, Source::FollowsAgent, "nothing was chosen for this role, so it uses the agent's model".to_string(), note)
}

/// What is worth telling the person about the choices as a whole: the roles that have a caller and use different models make the server unload one model and load the other, and
/// lose the prompt caches of both. `swap_seconds` is the time the model's profile gives for loading it.
pub fn warnings(choices: &[RoleChoice], swap_seconds: &dyn Fn(&str) -> Option<u32>) -> Vec<String> {
    let used: Vec<&RoleChoice> = choices.iter().filter(|c| CALLED_ROLES.contains(&c.role)).collect();
    let mut models: Vec<&str> = Vec::new();
    for c in &used {
        if !models.iter().any(|m| m.eq_ignore_ascii_case(&c.model)) {
            models.push(&c.model);
        }
    }
    if models.len() < 2 {
        return Vec::new();
    }
    let which: Vec<String> = used.iter().map(|c| format!("{} uses {}", c.role.name(), c.model)).collect();
    let cost = models
        .iter()
        .find_map(|m| swap_seconds(m).map(|s| format!("about {s} seconds for {m}, from its profile")))
        .unwrap_or_else(|| "10 to 40 seconds measured with the 8B on this machine".to_string());
    vec![format!("{}: the model server holds one model, so each turn that changes model makes it unload one and load the other ({cost}) and the prompt caches of both are lost", which.join(", "))]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<String> {
        vec!["Qwen3-8B-Q4_K_M.gguf".into(), "Llama-3.2-3B-Q4_K_M.gguf".into(), "qwen-0.5b-instruct-q4_k_m.gguf".into()]
    }

    fn inputs<'a>(installed: &'a [String], active: &'a str, env: Option<&'a str>, preferred: Option<&'a str>) -> Inputs<'a> {
        Inputs { installed, active, env_agent: env, preferred }
    }

    fn pick(role: Role, a: &RoleAssignments, i: &Inputs<'_>) -> (String, Source) {
        let c = resolve(role, a, i);
        (c.model, c.source)
    }

    #[test]
    fn with_nothing_chosen_the_agent_gets_the_preference_and_every_other_role_follows_it() {
        let installed = files();
        let i = inputs(&installed, "qwen-0.5b-instruct-q4_k_m.gguf", None, Some("Qwen3-8B-Q4_K_M.gguf"));
        let none = RoleAssignments::default();
        for role in Role::ALL {
            let (model, source) = pick(role, &none, &i);
            assert_eq!(model, "Qwen3-8B-Q4_K_M.gguf", "{role:?}");
            assert_eq!(source, if role == Role::Agent { Source::DefaultRule } else { Source::FollowsAgent }, "{role:?}");
        }
        // No Qwen3 8B in the folder: the active model.
        let i = inputs(&installed, "Llama-3.2-3B-Q4_K_M.gguf", None, None);
        assert_eq!(pick(Role::Agent, &none, &i), ("Llama-3.2-3B-Q4_K_M.gguf".to_string(), Source::ActiveModel));
        assert_eq!(pick(Role::Chat, &none, &i), ("Llama-3.2-3B-Q4_K_M.gguf".to_string(), Source::FollowsAgent));
    }

    #[test]
    fn the_regression_pressing_use_on_another_model_beats_the_qwen3_preference() {
        // The bug: Qwen3-8B in the folder, the person chose the Llama, and the agent went on with the Qwen3.
        let installed = files();
        let i = inputs(&installed, "Llama-3.2-3B-Q4_K_M.gguf", None, Some("Qwen3-8B-Q4_K_M.gguf"));
        let mut a = RoleAssignments::default();
        a.set(Role::Agent, Some("Llama-3.2-3B-Q4_K_M.gguf".into()));
        a.set(Role::Chat, Some("Llama-3.2-3B-Q4_K_M.gguf".into()));
        for role in [Role::Agent, Role::Chat, Role::Decision, Role::Fast, Role::Edit, Role::Diagnosis, Role::Summary] {
            assert_eq!(pick(role, &a, &i).0, "Llama-3.2-3B-Q4_K_M.gguf", "{role:?}");
        }
        assert_eq!(pick(Role::Agent, &a, &i).1, Source::SetByYou);
        assert_eq!(pick(Role::Decision, &a, &i).1, Source::FollowsAgent, "what was not chosen follows the agent that was");
        // The same table, the old way of choosing, would have given the Qwen3: that is what the default rule still does for a role nobody chose.
        assert_eq!(pick(Role::Agent, &RoleAssignments::default(), &i).0, "Qwen3-8B-Q4_K_M.gguf");
    }

    #[test]
    fn a_role_can_have_a_model_of_its_own_and_the_others_keep_the_agents() {
        let installed = files();
        let i = inputs(&installed, "Qwen3-8B-Q4_K_M.gguf", None, Some("Qwen3-8B-Q4_K_M.gguf"));
        let mut a = RoleAssignments::default();
        a.set(Role::Fast, Some("llama-3.2-3b-q4_k_m.GGUF".into()));
        let c = resolve(Role::Fast, &a, &i);
        assert_eq!((c.model.as_str(), c.source), ("Llama-3.2-3B-Q4_K_M.gguf", Source::SetByYou), "the file as it is spelled in the folder");
        assert_eq!(pick(Role::Agent, &a, &i).0, "Qwen3-8B-Q4_K_M.gguf");
        assert_eq!(pick(Role::Chat, &a, &i).0, "Qwen3-8B-Q4_K_M.gguf");
        // Taking the choice away puts the role back.
        a.set(Role::Fast, None);
        assert_eq!(pick(Role::Fast, &a, &i), ("Qwen3-8B-Q4_K_M.gguf".to_string(), Source::FollowsAgent));
        assert!(a.assigned().is_empty());
    }

    #[test]
    fn a_model_that_was_deleted_after_it_was_chosen_falls_back_and_says_so() {
        let installed = vec!["Qwen3-8B-Q4_K_M.gguf".to_string()];
        let i = inputs(&installed, "Qwen3-8B-Q4_K_M.gguf", None, Some("Qwen3-8B-Q4_K_M.gguf"));
        let mut a = RoleAssignments::default();
        a.set(Role::Agent, Some("Gone.gguf".into()));
        a.set(Role::Chat, Some("Gone.gguf".into()));
        let agent = resolve(Role::Agent, &a, &i);
        assert_eq!((agent.model.as_str(), agent.source), ("Qwen3-8B-Q4_K_M.gguf", Source::DefaultRule));
        assert!(agent.note.as_deref().is_some_and(|n| n.contains("Gone.gguf") && n.contains("no longer in the model folder")), "{:?}", agent.note);
        let chat = resolve(Role::Chat, &a, &i);
        assert_eq!(chat.source, Source::FollowsAgent);
        assert!(chat.note.is_some());
        assert!(resolve(Role::Fast, &a, &i).note.is_none(), "a role nobody chose for has nothing to say");
    }

    #[test]
    fn the_environment_names_the_agents_model_as_it_always_did_and_says_when_it_hides_a_choice() {
        let installed = files();
        let i = inputs(&installed, "Qwen3-8B-Q4_K_M.gguf", Some("llama-3.2-3b-q4_k_m.gguf"), Some("Qwen3-8B-Q4_K_M.gguf"));
        let none = RoleAssignments::default();
        assert_eq!(pick(Role::Agent, &none, &i), ("Llama-3.2-3B-Q4_K_M.gguf".to_string(), Source::Environment));
        assert_eq!(pick(Role::Chat, &none, &i), ("Llama-3.2-3B-Q4_K_M.gguf".to_string(), Source::FollowsAgent), "what follows the agent follows the variable");
        let mut a = RoleAssignments::default();
        a.set(Role::Agent, Some("Qwen3-8B-Q4_K_M.gguf".into()));
        a.set(Role::Chat, Some("Qwen3-8B-Q4_K_M.gguf".into()));
        let agent = resolve(Role::Agent, &a, &i);
        assert_eq!(agent.source, Source::Environment);
        assert!(agent.note.as_deref().is_some_and(|n| n.contains("Qwen3-8B-Q4_K_M.gguf") && n.contains("AQUA_AGENT_MODEL")));
        assert_eq!(pick(Role::Chat, &a, &i), ("Qwen3-8B-Q4_K_M.gguf".to_string(), Source::SetByYou), "a role chosen on its own is not the variable's");
        // A variable that names a file that is not there is ignored, as it was.
        let i = inputs(&installed, "Qwen3-8B-Q4_K_M.gguf", Some("nothing.gguf"), Some("Qwen3-8B-Q4_K_M.gguf"));
        assert_eq!(pick(Role::Agent, &none, &i).1, Source::DefaultRule);
    }

    #[test]
    fn two_roles_on_different_models_get_a_warning_and_the_same_model_does_not() {
        let installed = files();
        let i = inputs(&installed, "Qwen3-8B-Q4_K_M.gguf", None, Some("Qwen3-8B-Q4_K_M.gguf"));
        let choices = |a: &RoleAssignments| Role::ALL.map(|r| resolve(r, a, &i)).to_vec();
        let none = RoleAssignments::default();
        assert!(warnings(&choices(&none), &|_| None).is_empty());
        let mut a = RoleAssignments::default();
        a.set(Role::Chat, Some("Llama-3.2-3B-Q4_K_M.gguf".into()));
        let w = warnings(&choices(&a), &|_| None);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("agent uses Qwen3-8B-Q4_K_M.gguf") && w[0].contains("chat uses Llama-3.2-3B-Q4_K_M.gguf") && w[0].contains("10 to 40 seconds"), "{}", w[0]);
        let w = warnings(&choices(&a), &|m| (m == "Llama-3.2-3B-Q4_K_M.gguf").then_some(12));
        assert!(w[0].contains("about 12 seconds for Llama-3.2-3B-Q4_K_M.gguf, from its profile"), "{}", w[0]);
        // A role nothing asks for by role (summary) on its own model is not a swap that happens.
        let mut b = RoleAssignments::default();
        b.set(Role::Summary, Some("Llama-3.2-3B-Q4_K_M.gguf".into()));
        assert!(warnings(&choices(&b), &|_| None).is_empty());
    }

    #[test]
    fn the_file_is_read_with_a_reason_for_every_line_left_out_and_written_back_the_same() {
        let text = r#"{ "version": 1, "roles": {
            "agent": "llama.cpp/Qwen3-8B-Q4_K_M.gguf",
            "chat": "Llama-3.2-3B-Q4_K_M.gguf",
            "fast": "openai/gpt-4o",
            "decision": "C:\\evil.gguf",
            "edit": 5,
            "nobody": "x.gguf" } }"#;
        let a = RoleAssignments::parse(text);
        assert_eq!(a.get(Role::Agent), Some("Qwen3-8B-Q4_K_M.gguf"));
        assert_eq!(a.get(Role::Chat), Some("Llama-3.2-3B-Q4_K_M.gguf"));
        assert_eq!((a.get(Role::Fast), a.get(Role::Decision), a.get(Role::Edit)), (None, None, None));
        assert_eq!(a.problems.len(), 4, "{:#?}", a.problems);
        assert!(a.problems.iter().any(|m| m.contains("fast") && m.contains("not a provider on this machine")));
        assert!(a.problems.iter().any(|m| m.contains("decision") && m.contains("not a file name")));
        assert!(a.problems.iter().any(|m| m.contains("edit") && m.contains("not text")));
        assert!(a.problems.iter().any(|m| m.contains("nobody") && m.contains("not a role")));
        // Written back: the provider is spelled out, and reading it again gives the same choices.
        let again = RoleAssignments::parse(&a.to_json());
        assert!(again.problems.is_empty(), "{:?}", again.problems);
        assert_eq!(again.assigned(), a.assigned());
        assert!(a.to_json().contains("\"llama.cpp/Qwen3-8B-Q4_K_M.gguf\""));
    }

    #[test]
    fn a_file_that_cannot_be_used_leaves_every_role_on_its_default_and_says_why() {
        for (text, word) in [("nonsense", "not valid JSON"), ("[]", "not a JSON object"), (r#"{"version": 3}"#, "version"), ("", "not valid JSON")] {
            let a = RoleAssignments::parse(text);
            assert!(a.assigned().is_empty(), "{text}");
            assert_eq!(a.problems.len(), 1, "{text}");
            assert!(a.problems[0].contains(word) && a.problems[0].contains("default model"), "{}", a.problems[0]);
        }
        let a = RoleAssignments::parse(r#"{"roles": 7}"#);
        assert!(a.problems[0].contains("not an object"));
    }

    #[test]
    fn the_file_is_saved_whole_and_loaded_and_one_that_is_not_there_is_nothing() {
        use crate::test_support::TempWorld;
        let world = TempWorld::new("roles_file");
        let path = world.path("models/router.json");
        let none = RoleAssignments::load(&path);
        assert!(!none.present && none.assigned().is_empty() && none.problems.is_empty());
        let mut a = RoleAssignments::default();
        a.set(Role::Agent, Some("Qwen3-8B-Q4_K_M.gguf".into()));
        a.set(Role::Chat, Some("Llama-3.2-3B-Q4_K_M.gguf".into()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        a.save(&path).unwrap();
        let loaded = RoleAssignments::load(&path);
        assert!(loaded.present && loaded.problems.is_empty());
        assert_eq!(loaded.assigned(), a.assigned());
        assert!(!world.path("models").read_dir().unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains(".tmp")), "no temporary file is left");
        let bad = world.write("models/bad.json", [0xff, 0xfe]);
        assert!(RoleAssignments::load(&bad).problems[0].contains("UTF-8"));
    }

    #[test]
    fn a_request_names_a_file_of_the_folder_or_a_local_reference_and_nothing_else() {
        assert_eq!(parse_reference("X.gguf"), Ok("X.gguf".to_string()));
        assert_eq!(parse_reference(" llama.cpp/X.gguf "), Ok("X.gguf".to_string()));
        assert_eq!(parse_reference("LLAMA.CPP/X.GGUF"), Ok("X.GGUF".to_string()));
        for bad in ["", "x", "x.bin", "../x.gguf", "a/b/x.gguf", "C:\\x.gguf", "ollama/llama3", "openai/gpt-4o", "llama.cpp/"] {
            assert!(parse_reference(bad).is_err(), "{bad:?}");
        }
        assert!(parse_reference("openai/gpt-4o").unwrap_err().contains("remote policy"));
        assert!(is_plain_file_name("a-b_c.1.gguf") && !is_plain_file_name("a..gguf") && !is_plain_file_name("a/b.gguf"));
    }
}
