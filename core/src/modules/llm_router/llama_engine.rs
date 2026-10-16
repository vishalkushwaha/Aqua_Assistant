use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use tracing::{info, warn};

use crate::model::adapter::{self, ModelAdapter};
use crate::model::profile::{self, Profiles};
use crate::model::roles::{self, RoleAssignments, RoleChoice};
use crate::model::{bridge, gguf, Family, Role};

use super::llama_cli::{
    agent_llama_timeout, display_name_from_filename, generate_prompt_with_progress,
    import_gguf, list_gguf_models, model_bundle_valid, resolve_cli_binary, resolve_model_file,
    resolve_named_model, GgufModelInfo, MODEL_FILENAME,
};

pub struct LlamaRouterEngine {
    model_dir: PathBuf,
    /// Active GGUF path + display name (switchable at runtime).
    active: RwLock<ActiveModel>,
    /// `data/models/profiles.json`: what the person wrote about their models (card OA9-03). Read once, when the engine starts; empty when there is no file.
    profiles: Profiles,
    /// `data/models/router.json`: the model the person chose for each role (card OA9-03). Empty when there is no file.
    roles: RwLock<RoleAssignments>,
    /// Where `router.json` is written: next to the model folder. `None` when the model folder has no folder above it to keep it in.
    router_file: Option<PathBuf>,
}

#[derive(Clone)]
struct ActiveModel {
    model_path: PathBuf,
    active_model_name: String,
}

/// The planner prefix last read into the single cache slot, if that server has not since read something else.
static WARMED_AGENT_PREFIX: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Does reading `prompt` replace the planner prefix the cache slot holds? The slot keeps what it read last, so a prompt that does not start with the prefix is read over it.
fn replaces_warm_prefix(prefix: &str, prompt: &str) -> bool {
    !prompt.starts_with(prefix)
}

fn forget_warm_prefix_if_replaced(prompt: &str) {
    if let Ok(mut guard) = WARMED_AGENT_PREFIX.lock() {
        if guard.as_ref().is_some_and(|prefix| replaces_warm_prefix(prefix, prompt)) {
            *guard = None;
            // The slot no longer starts with the agent's prompt: it is not worth a saved file (card OA9-06; does nothing unless AQUA_LLAMA_SLOT_SAVE is on).
            super::kv_slots::note_slot_replaced();
        }
    }
}

impl LlamaRouterEngine {
    /// Llama.cpp router — GGUFs under `data/models/llm/` (see `data/models/llm/README.md`).
    pub fn new<P: AsRef<Path>>(model_dir: P) -> Result<Self> {
        let model_dir = model_dir.as_ref().to_path_buf();
        let model_path =
            resolve_model_file(&model_dir).unwrap_or_else(|| model_dir.join(MODEL_FILENAME));
        let active_model_name = model_path
            .file_name()
            .and_then(|n| n.to_str())
            .map(display_name_from_filename)
            .unwrap_or_else(|| "Qwen-0.5B-Instruct-Q4_K_M".to_string());

        info!("Initializing Llama.cpp router at {:?}", model_path);

        let is_model_loaded = model_bundle_valid(&model_dir);
        let cli_present = resolve_cli_binary().is_some();

        if !is_model_loaded {
            warn!(
                "Llama GGUF missing. Place *.gguf files under data/models/llm/ — see README.md"
            );
        } else if cli_present {
            info!("Llama.cpp GGUF + native llama-cli active ({active_model_name})");
        } else {
            info!("Llama.cpp GGUF loaded ({active_model_name}); llama-cli missing → reflex fallback");
        }

        // The two files of the person's choices sit in the folder above the model folder (data/models). A model folder with no folder above it has neither.
        let root = model_dir.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf);
        let profiles = root.as_ref().map(|r| Profiles::load(&r.join(profile::FILE))).unwrap_or_default();
        let assigned = root.as_ref().map(|r| RoleAssignments::load(&r.join(roles::FILE))).unwrap_or_default();
        for problem in profiles.problems.iter().chain(assigned.problems.iter()) {
            warn!("{problem}");
        }

        Ok(Self {
            model_dir,
            active: RwLock::new(ActiveModel {
                model_path,
                active_model_name,
            }),
            profiles,
            roles: RwLock::new(assigned),
            router_file: root.map(|r| r.join(roles::FILE)),
        })
    }

    pub fn list_models(&self) -> Vec<GgufModelInfo> {
        let active_filename = self
            .model_path()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let mut models = list_gguf_models(&self.model_dir);
        let (window, slots) = (super::llama_server::context_tokens(), super::llama_server::slots());
        for m in &mut models {
            m.active = m.filename == active_filename;
            // What the header and the profile say about it (the header is read once and remembered until the file changes).
            m.info = Some(profile::ModelFacts::of(&self.model_dir.join(&m.filename), &self.profiles, window, slots, super::llama_server::default_gpu_layers(window)));
        }
        models
    }

    /// Switch the default GGUF used by pipeline / generate (basename only).
    pub fn set_active_model(&self, filename: &str) -> Result<()> {
        let path = resolve_named_model(&self.model_dir, filename).ok_or_else(|| {
            anyhow::anyhow!("GGUF not found or invalid name: {filename}")
        })?;
        let display_name = display_name_from_filename(filename);
        {
            let mut guard = self.active.write().unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.model_path = path.clone();
            guard.active_model_name = display_name.clone();
        }
        info!("Switched active Llama GGUF to {filename} ({display_name})");
        Ok(())
    }

    /// Copy a `.gguf` into the model folder and make it the active model. The running server is stopped first so Windows
    /// can replace a file the server still has open.
    pub fn import_model(&self, source: &Path) -> Result<String> {
        super::llama_server::shutdown();
        let name = import_gguf(&self.model_dir, source)?;
        self.set_active_model(&name)?;
        Ok(name)
    }

    /// Delete one `.gguf` from the model folder by its file name. If it was the active model, another installed file
    /// becomes active.
    pub fn remove_model(&self, filename: &str) -> Result<()> {
        let path = resolve_named_model(&self.model_dir, filename)
            .ok_or_else(|| anyhow::anyhow!("GGUF not found or invalid name: {filename}"))?;
        super::llama_server::shutdown();
        std::fs::remove_file(&path).map_err(|e| anyhow::anyhow!("could not delete {}: {e}", path.display()))?;
        let active = self.active_model_filename();
        if active.eq_ignore_ascii_case(filename) {
            if let Some(next) = list_gguf_models(&self.model_dir).into_iter().next() {
                self.set_active_model(&next.filename)?;
            }
        }
        self.forget_role_choices_of(filename);
        info!("Removed Llama GGUF {filename}");
        Ok(())
    }

    fn resolve_model_override(&self, model_override: Option<&str>) -> PathBuf {
        if let Some(name) = model_override {
            if let Some(p) = resolve_named_model(&self.model_dir, name) {
                return p;
            }
            warn!("Requested Llama model '{name}' not found; using active model");
        }
        self.model_path()
    }

    pub fn generate_response(&self, prompt: &str) -> Result<String> {
        self.generate_response_with_model(prompt, None)
    }

    /// The probabilities the agent's model gives to the first word of its answer to `prompt` when it may write only one of `labels` (see `modules::decision`): one read of
    /// the prompt and one decode step, on the model the chat and the agent already use, so nothing is loaded or swapped for it. Fails with the reason when the model server
    /// cannot be used, or does not give numbers.
    pub fn first_token_probs(&self, prompt: &str, labels: &[String]) -> Result<(Vec<(String, f64)>, super::llama_server::Timings)> {
        use super::llama_server as server;
        let name = self.model_filename_for(Role::Decision);
        let model = self.resolve_model_override(Some(name.as_str()));
        if !server::enabled() || !model.is_file() {
            return Err(anyhow::anyhow!("the language model is not available for a decision"));
        }
        // With the fast tier on (card OA9-06) the decision goes to the small model's own server first, which is free while the main model is busy: it is only when that server is not
        // ready that the rule below applies, and a decision that falls back to the main model is the one the rule would have refused.
        let tiered = crate::model::fast_route::applies(Role::Decision);
        // A decision is a help and not a reason to hold up a turn: a model that is busy with another request (an agent's planner call takes a minute) is not waited for.
        if !server::is_free() && !(tiered && super::fast_server::is_ready()) {
            return Err(anyhow::anyhow!("the language model is busy with another request, so a decision is not waited for"));
        }
        // The decision is read into the chat slot, which on one slot is the planner's too: if it replaces the prefix that was warmed, the next warm-up must read it again. A decision the
        // small model answered did not touch the main model's slot, so then the prefix stays warm.
        let answered_by_fast_before = crate::model::fast_tier::answered_by_fast();
        if !tiered {
            forget_warm_prefix_if_replaced(prompt);
        }
        let asked = bridge::with_settings(self.settings_for(Role::Decision, &model), || bridge::complete_choice(&model, prompt, labels, server::slot_for(server::CHAT_SLOT), agent_llama_timeout()));
        if tiered && crate::model::fast_tier::answered_by_fast() == answered_by_fast_before {
            forget_warm_prefix_if_replaced(prompt);
        }
        match asked {
            Ok(r) if r.first_token_probs.is_empty() => Err(anyhow::anyhow!("the model server gave no probabilities for the decision")),
            Ok(r) => Ok((r.first_token_probs, r.timings)),
            Err(server::Failure::Timeout(m) | server::Failure::Rejected(m) | server::Failure::Unavailable(m)) => Err(anyhow::anyhow!(m)),
            Err(server::Failure::Cancelled) => Err(anyhow::anyhow!("stopped: the task was cancelled")),
        }
    }

    /// Plain chat: the model the agent uses (`agent_model_filename`), not whichever model is "active" (the small one on a fresh
    /// install). The model server holds one model; a chat turn on another one makes it unload the first and load the second (10 to
    /// 40 seconds, and both prompt caches lost), and again for the next agent turn.
    pub fn generate_chat_response(&self, prompt: &str) -> Result<String> {
        self.generate_response_with_model(prompt, Some(&self.model_filename_for(Role::Chat)))
    }

    /// [`Self::generate_chat_response`] with the room the answer is given: a rewrite is about as long as the text it rewrites, and a model that is not stopped can write a page.
    pub fn generate_chat_response_limited(&self, prompt: &str, max_tokens: u32) -> Result<String> {
        // With the fast tier on (card OA9-06) this call is the role fast, and the bridge may answer it with the small model; with it off the role the bridge is told is chat, as ever.
        let _role = crate::model::fast_route::applies(Role::Fast).then(|| ThisCallIs::new(Role::Fast));
        self.generate_response_with_budget(prompt, Some(&self.model_filename_for(Role::Fast)), max_tokens)
    }

    pub fn generate_response_with_model(
        &self,
        prompt: &str,
        model_override: Option<&str>,
    ) -> Result<String> {
        self.generate_response_with_budget(prompt, model_override, CHAT_REPLY_TOKENS)
    }

    /// [`Self::generate_chat_response`], with the answer also handed to `on_text` as it is written (a few characters at a time), so
    /// that its start can be spoken while the rest is still being made. `on_text` returns `false` to stop, because nobody is listening
    /// any more: this then ends with the error "stopped: the task was cancelled" and the model server, which is fine, keeps its model.
    /// The returned answer is the whole text, cleaned and cut at its last complete sentence when the model ran out of tokens, exactly as
    /// [`Self::generate_chat_response`] returns it. When the model server cannot be used the answer comes from `llama-cli` and is handed
    /// over in one piece at the end. A server that fails after some text was handed over is an error (the answer so far was already
    /// said; a second try would say it again).
    pub fn generate_chat_streaming(&self, prompt: &str, on_text: impl FnMut(&str) -> bool) -> Result<StreamedChat> {
        self.generate_chat_streaming_with_model(prompt, &self.model_filename_for(Role::Chat), on_text)
    }

    /// [`Self::generate_chat_streaming`] on a named model (the tests use the small one).
    pub(crate) fn generate_chat_streaming_with_model(&self, prompt: &str, model_name: &str, mut on_text: impl FnMut(&str) -> bool) -> Result<StreamedChat> {
        let trimmed = prompt.trim();
        if trimmed.is_empty() {
            return Ok(StreamedChat::default());
        }
        let model_path = self.resolve_model_override(Some(model_name));
        let model_adapter = self.adapter_of(&model_path);
        let prompt_for_model = model_adapter.adapt_prompt(trimmed);
        forget_warm_prefix_if_replaced(&prompt_for_model);
        if prompt_for_model.chars().count() > super::llama_server::prompt_chars_ceiling() {
            return Err(anyhow::anyhow!(super::llama_server::too_long(prompt_for_model.chars().count())));
        }
        if super::llama_server::enabled() && model_path.is_file() {
            let mut said = false;
            let asked = bridge::with_settings(self.settings_for(Role::Chat, &model_path), || {
                bridge::complete_streaming(
                    &model_path,
                    &prompt_for_model,
                    CHAT_REPLY_TOKENS,
                    super::llama_server::slot_for(super::llama_server::CHAT_SLOT),
                    agent_llama_timeout(),
                    |_| {},
                    |delta| {
                        said |= !delta.is_empty();
                        on_text(delta)
                    },
                )
            });
            match asked {
                Ok(reply) => {
                    let clean = clean_reply(model_adapter, reply.text.trim());
                    let text = if reply.cut_off {
                        let ended = end_at_last_sentence(&clean);
                        if ended.is_empty() { clean } else { ended }
                    } else {
                        clean
                    };
                    if !text.is_empty() {
                        return Ok(StreamedChat { text, cut_off: reply.cut_off });
                    }
                    warn!("llama-server returned an empty answer; trying llama-cli");
                }
                Err(super::llama_server::Failure::Timeout(msg)) | Err(super::llama_server::Failure::Rejected(msg)) => {
                    warn!("Streamed chat failed: {msg}");
                    return Err(anyhow::anyhow!(msg));
                }
                Err(super::llama_server::Failure::Cancelled) => return Err(anyhow::anyhow!("stopped: the task was cancelled")),
                Err(super::llama_server::Failure::Unavailable(why)) if said => {
                    warn!("llama-server failed in the middle of a streamed answer: {why}");
                    return Err(anyhow::anyhow!("the language model stopped in the middle of the answer"));
                }
                Err(super::llama_server::Failure::Unavailable(why)) => {
                    warn!("llama-server not used ({why}); falling back to llama-cli");
                }
            }
        }
        let text = self.answer_with_cli(trimmed, &prompt_for_model, &model_path, CHAT_REPLY_TOKENS, &mut |_| {}, true)?;
        if !text.is_empty() && !on_text(&text) {
            return Err(anyhow::anyhow!("stopped: the task was cancelled"));
        }
        Ok(StreamedChat { text, cut_off: false })
    }

    /// The default rule for the agent's model, for an agent nobody chose a model for: `AQUA_AGENT_MODEL`, else Qwen3-8B Q4_K_M, else any Qwen3 8B file, else nothing (the active
    /// model is then used). **Not what the agent uses once the person has chosen a model** (card OA9-03, finding D-PR-2): see [`Self::model_filename_for`], which asks the table of
    /// roles first and this rule only for a role nobody set.
    pub fn prefer_agent_model_filename(&self) -> Option<String> {
        if let Ok(name) = std::env::var("AQUA_AGENT_MODEL") {
            let name = name.trim().to_string();
            if !name.is_empty() && resolve_named_model(&self.model_dir, &name).is_some() {
                return Some(name);
            }
        }
        preferred_by_rule(&list_gguf_models(&self.model_dir))
    }

    /// Basename the agent planner will actually load: the model the person chose for the agent role, `AQUA_AGENT_MODEL`, the preferred Qwen3-8B, or the active model, in that
    /// order of strength (see `crate::model::roles`).
    pub fn agent_model_filename(&self) -> String {
        self.model_filename_for(Role::Agent)
    }

    /// One reply from the agent's model for a caller that has no better idea of the room it needs (the flows' completer).
    /// Uses Qwen3-8B-Q4_K_M when present (not the tiny 0.5B downgrade).
    pub fn generate_agent_response(&self, prompt: &str) -> Result<String> {
        self.generate_agent_response_with_progress(prompt, DEFAULT_REPLY_TOKENS, |_| {})
    }

    /// The same, with how many tokens the reply may take. A step that writes a post or a list of commands needs far more
    /// room than a tool call, and a reply that is cut off is asked for again once with twice the room.
    pub fn generate_agent_response_with_budget(&self, prompt: &str, max_tokens: u32) -> Result<String> {
        self.generate_agent_response_with_progress(prompt, max_tokens, |_| {})
    }

    /// The planner's call. `max_tokens` is chosen by the caller, which knows what it is asking for (see
    /// [`PLANNER_TOOL_TOKENS`]). It used to be guessed from the prompt text, with a string that never matched, so every
    /// reply was capped at 120 tokens and a long answer or a code edit was cut off in the middle.
    pub fn generate_agent_response_with_progress(
        &self,
        prompt: &str,
        max_tokens: u32,
        on_tick: impl FnMut(u64),
    ) -> Result<String> {
        self.generate_agent_response_constrained(prompt, max_tokens, None, on_tick)
    }

    /// [`Self::generate_agent_response_with_progress`], and the reply is held to `constraint` while it is written (the planner's
    /// JSON: only a shown tool's name, only its arguments). It applies to the model server; the `llama-cli` fallback cannot take one,
    /// and its reply is read with the tolerant parser as before.
    pub fn generate_agent_response_constrained(
        &self,
        prompt: &str,
        max_tokens: u32,
        constraint: Option<&super::llama_server::Constraint>,
        on_tick: impl FnMut(u64),
    ) -> Result<String> {
        self.generate_agent_response_shrinking(prompt, max_tokens, constraint, on_tick, None)
    }

    /// [`Self::generate_agent_response_constrained`], and with the model layer's router on (`AQUA_MODEL_ROUTER`) a prompt the server refuses as too long is asked again once with the
    /// shorter prompt `shrink` gives (card OA9-02, task PR-03): the router makes the second request itself and the caller sees one call. With the router off nothing is asked
    /// again here, and `shrink` is not called: the caller recovers from the refusal it is given, as it always did.
    pub fn generate_agent_response_shrinking(
        &self,
        prompt: &str,
        max_tokens: u32,
        constraint: Option<&super::llama_server::Constraint>,
        mut on_tick: impl FnMut(u64),
        mut shrink: Option<crate::modules::planner::PromptShrink<'_>>,
    ) -> Result<String> {
        let first = self.generate_agent_response_once(prompt, max_tokens, constraint, &mut on_tick, crate::modules::planner::reborrowed(&mut shrink));
        match first {
            // A server that does not take the constraint (an older build, a schema it cannot turn into a grammar) refuses the request. The
            // planner is then asked again without it: its reply is read with the tolerant parser, as it was before there was a constraint.
            // (With an offer of native tool calls the request had tools and no schema, so a refusal is not the schema's: the planner asks again the JSON way, card OA9-06.)
            Err(e) if refused_the_constraint(constraint, &e) && crate::model::native_tools::current_offer().is_none() => {
                warn!("the model server refused the constrained request ({e}); asking again without the constraint");
                self.generate_agent_response_once(prompt, max_tokens, None, &mut on_tick, crate::modules::planner::reborrowed(&mut shrink))
            }
            other => other,
        }
    }

    fn generate_agent_response_once(
        &self,
        prompt: &str,
        max_tokens: u32,
        constraint: Option<&super::llama_server::Constraint>,
        on_tick: &mut dyn FnMut(u64),
        shrink: Option<crate::modules::planner::PromptShrink<'_>>,
    ) -> Result<String> {
        let budget = max_tokens.clamp(32, MAX_REPLY_TOKENS);
        let active = self
            .model_path()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let override_name = Some(self.agent_model_filename()).filter(|name| {
            !name.eq_ignore_ascii_case(&active)
        });
        if let Some(ref name) = override_name {
            info!("Agent planning using {name} (active was {active})");
        } else {
            info!("Agent planning using active model {active}");
        }
        self.generate_response_with_budget_progress(
            prompt,
            override_name.as_deref(),
            budget,
            on_tick,
            true,
            OnCutOff::RetryThenFail,
            super::llama_server::slot_for(super::llama_server::AGENT_SLOT),
            constraint,
            shrink,
        )
    }

    fn generate_response_with_budget(
        &self,
        prompt: &str,
        model_override: Option<&str>,
        max_tokens: u32,
    ) -> Result<String> {
        // Chat uses the persistent llama-server too: the 8B model stays in memory between requests instead of being
        // loaded afresh each time (about 15 s), and requests take turns on it. It falls back to llama-cli if it cannot start.
        self.generate_response_with_budget_progress(prompt, model_override, max_tokens, &mut |_| {}, true, OnCutOff::EndAtSentence, super::llama_server::slot_for(super::llama_server::CHAT_SLOT), None, None)
    }

    fn generate_response_with_budget_progress(
        &self,
        prompt: &str,
        model_override: Option<&str>,
        max_tokens: u32,
        on_tick: &mut dyn FnMut(u64),
        // The agent's planner keeps a model server running so each step reads only what changed.
        use_server: bool,
        on_cut_off: OnCutOff,
        // The server's cache slot for this kind of request (chat and the agent's planner keep separate caches).
        slot: Option<u32>,
        // What the reply must look like, enforced by the server while it decodes (none for chat).
        constraint: Option<&super::llama_server::Constraint>,
        // What to do about a prompt the server refuses as too long: the router asks again with what this gives (see `generate_agent_response_shrinking`).
        mut shrink: Option<crate::modules::planner::PromptShrink<'_>>,
    ) -> Result<String> {
        let trimmed = prompt.trim();
        if trimmed.is_empty() {
            return Ok(String::new());
        }

        let model_path = self.resolve_model_override(model_override);
        let model_adapter = self.adapter_of(&model_path);
        let prompt_for_model = model_adapter.adapt_prompt(trimmed);
        // A call the fast tier may answer with the small model (card OA9-06; only then is the role of this call told) leaves the main model's slot alone when it does: whether it did is
        // known after the call, and the prefix that was warmed is forgotten then if it did not.
        let tiered = THIS_CALL_IS.with(std::cell::Cell::get).is_some();
        let answered_by_fast_before = crate::model::fast_tier::answered_by_fast();
        if !tiered {
            forget_warm_prefix_if_replaced(&prompt_for_model);
        }

        // Past this it is a runaway paste (the model's window is smaller), and `llama-cli` takes the prompt on its
        // command line, which Windows cuts off near 32 K characters.
        if prompt_for_model.chars().count() > super::llama_server::prompt_chars_ceiling() {
            return Err(anyhow::anyhow!(super::llama_server::too_long(prompt_for_model.chars().count())));
        }
        if use_server && super::llama_server::enabled() && model_path.is_file() {
            // The planner's calls (which retry a cut-off reply) are the agent's role; the others are chat, unless the caller said this call is another role (the fast tier's).
            let role = THIS_CALL_IS.with(std::cell::Cell::get).unwrap_or(if on_cut_off == OnCutOff::RetryThenFail { crate::model::Role::Agent } else { crate::model::Role::Chat });
            // How much of the window the prompt takes by the meter's count, and so how many tokens a longer reply could have (card OA4-02).
            let window = self.max_context_tokens();
            let estimated = crate::modules::context::meter::meter().tokens(&prompt_for_model);
            let settings = self.settings_for(role, &model_path);
            // The caller's shorter prompt is the engine's prompt again: trimmed, written for the model as any other prompt is, and not over the limit of a runaway paste (the router sends it as it is).
            let has_shrink = shrink.is_some();
            let mut shorter_prompt = |refusal: &crate::model::ProviderError| -> Option<String> {
                let shorter = shrink.as_mut()?(refusal)?;
                let trimmed = shorter.trim();
                let for_model = model_adapter.adapt_prompt(trimmed);
                if trimmed.is_empty() || for_model.chars().count() > super::llama_server::prompt_chars_ceiling() {
                    return None;
                }
                forget_warm_prefix_if_replaced(&for_model);
                Some(for_model)
            };
            let asked = ask_with_cut_off_handling(max_tokens, on_cut_off, Some(reply_headroom(window, estimated)), |room| {
                bridge::with_settings(settings.clone(), || bridge::complete_constrained_shrinking(role, &model_path, &prompt_for_model, room, slot, constraint, agent_llama_timeout(), |secs| on_tick(secs), if has_shrink { Some(&mut shorter_prompt) } else { None }))
            });
            if tiered && crate::model::fast_tier::answered_by_fast() == answered_by_fast_before {
                forget_warm_prefix_if_replaced(&prompt_for_model);
            }
            match asked {
                Ok(reply) => {
                    LAST_TIMINGS.with(|t| t.set(Some(reply.timings)));
                    let clean = clean_reply(model_adapter, reply.text.trim());
                    if reply.cut_off {
                        match on_cut_off {
                            // Half a tool call or half a JSON answer is worse than an error: it would be read as the answer.
                            OnCutOff::RetryThenFail => {
                                return Err(cut_off_error(estimated, window));
                            }
                            OnCutOff::EndAtSentence => {
                                let ended = end_at_last_sentence(&clean);
                                if !ended.is_empty() {
                                    return Ok(ended);
                                }
                            }
                        }
                    }
                    if !clean.is_empty() {
                        return Ok(clean);
                    }
                    warn!("llama-server returned an empty answer; trying llama-cli");
                }
                // Out of time: another attempt would spend the same time again.
                Err(super::llama_server::Failure::Timeout(msg)) => {
                    warn!("Agent llama generate failed: {msg}");
                    return Err(anyhow::anyhow!(msg));
                }
                // The model is fine and refused this one request; running it another way would be refused too.
                Err(super::llama_server::Failure::Rejected(msg)) => {
                    warn!("Agent llama request refused: {msg}");
                    return Err(refusal_error(msg));
                }
                // Stopped by the person: end the turn now, and do not try another way of running the same prompt.
                Err(super::llama_server::Failure::Cancelled) => {
                    return Err(anyhow::anyhow!("stopped: the task was cancelled"));
                }
                Err(super::llama_server::Failure::Unavailable(why)) => {
                    warn!("llama-server not used ({why}); falling back to llama-cli");
                    SERVER_DOWN.with(|c| c.set(true));
                }
            }
        }
        self.answer_with_cli(trimmed, &prompt_for_model, &model_path, max_tokens, on_tick, use_server)
    }

    /// The answer from `llama-cli` (one process per request), for when the model server cannot be used. With no model file or no
    /// `llama-cli` it is the reflex reply, or for the agent's planner (`agent_planner`) an error that says what is missing.
    fn answer_with_cli(&self, trimmed: &str, prompt_for_model: &str, model_path: &Path, max_tokens: u32, on_tick: &mut dyn FnMut(u64), agent_planner: bool) -> Result<String> {
        // No GGUF on disk: answer with the reflex reply instead of launching llama on a missing file.
        if let Some(cli) = resolve_cli_binary().filter(|_| model_path.is_file()) {
            match generate_prompt_with_progress(
                &cli,
                model_path,
                prompt_for_model,
                max_tokens,
                agent_llama_timeout(),
                |secs| on_tick(secs),
            ) {
                Ok(text) => {
                    let clean = clean_reply(self.adapter_of(model_path), text.trim());
                    if !clean.is_empty() {
                        return Ok(clean);
                    }
                }
                Err(e) => {
                    warn!("Agent llama generate failed: {e:#}");
                    return Err(e);
                }
            }
        }

        no_engine_answer(agent_planner, model_path.is_file(), resolve_cli_binary().is_some(), || self.reflex_response(trimmed))
    }

    /// Have the model read the start of its prompts now, so that the first real request of each kind finds them in its cache.
    /// The server is started if it is not running (the first read loads the model). Each prompt is read in pieces of
    /// `PREFILL_CHUNK_CHARS` characters, and a real request that is waiting for the model goes first between two pieces, so
    /// this never makes a person wait for more than one piece (about 15 s on this CPU). Returns the number of pieces read.
    pub fn warm_up_prompts(&self, model_name: &str, chat_prompt: &str, agent_prefix: &str) -> Result<usize> {
        let model_path = self.resolve_model_override(Some(model_name));
        if !model_path.is_file() || !super::llama_server::enabled() {
            anyhow::bail!("the language model is not available for a warm-up");
        }
        let mut read = 0usize;
        for (text, slot, role) in [(chat_prompt, super::llama_server::CHAT_SLOT, crate::model::Role::Chat), (agent_prefix, super::llama_server::AGENT_SLOT, crate::model::Role::Agent)] {
            for piece in prefix_pieces(text.trim(), PREFILL_CHUNK_CHARS) {
                // A person's request first: wait until nobody is waiting for the model.
                let mut waited = 0;
                while super::llama_server::others_waiting() && waited < 600 {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    waited += 1;
                }
                forget_warm_prefix_if_replaced(&piece);
                match bridge::with_settings(self.settings_for(role, &model_path), || bridge::complete_warmup(role, &model_path, &piece, super::llama_server::slot_for(slot), agent_llama_timeout())) {
                    Ok(_) => read += 1,
                    Err(e) => anyhow::bail!("the warm-up read failed: {e:?}"),
                }
            }
        }
        Ok(read)
    }

    /// Read `prefix` into the model server's cache (`cache_prompt`, one generated token, discarded).
    /// `prefix` must be the exact start of the agent request that follows. A second call with the same
    /// bytes does nothing while that server is still the one that read them.
    pub fn warm_agent_prefix(&self, model_name: &str, prefix: &str) -> Result<()> {
        if prefix.is_empty() {
            return Ok(());
        }
        if super::llama_server::is_running() {
            if let Ok(guard) = WARMED_AGENT_PREFIX.lock() {
                if guard.as_deref() == Some(prefix) {
                    return Ok(());
                }
            }
        }
        let model_path = self.resolve_model_override(Some(model_name));
        if !model_path.is_file() || !super::llama_server::enabled() {
            anyhow::bail!("the language model is not available for a warm-up");
        }
        info!("agent warmup: prefix_chars={} cache_prompt=true", prefix.chars().count());
        // With AQUA_LLAMA_SLOT_SAVE on, the start of the prompt is told (as a hash) before the read, so that a server this read has to start can have its saved slot put back first,
        // and again when the read is in the slot, so that a stop can save it (card OA9-06; both do nothing with the switch off).
        super::kv_slots::note_prefix_to_read(prefix);
        bridge::with_settings(self.settings_for(Role::Agent, &model_path), || {
            bridge::complete_warmup(Role::Agent, &model_path, prefix, super::llama_server::slot_for(super::llama_server::AGENT_SLOT), agent_llama_timeout())
        })
        .map_err(|e| anyhow::anyhow!("the agent-prefix warm-up failed: {e:?}"))?;
        super::kv_slots::note_prefix_read();
        if let Ok(mut guard) = WARMED_AGENT_PREFIX.lock() {
            *guard = Some(prefix.to_string());
        }
        info!("agent warmup completed");
        Ok(())
    }

    pub async fn generate_response_stream_async(
        &self,
        prompt: &str,
        tx: tokio::sync::mpsc::Sender<String>,
    ) -> Result<()> {
        self.generate_response_stream_with_model_async(prompt, None, tx)
            .await
    }

    pub async fn generate_response_stream_with_model_async(
        &self,
        prompt: &str,
        model_override: Option<&str>,
        tx: tokio::sync::mpsc::Sender<String>,
    ) -> Result<()> {
        let trimmed = prompt.trim();
        if trimmed.is_empty() {
            return Ok(());
        }

        let model = self.resolve_model_override(model_override);
        if let Some(cli) = resolve_cli_binary() {
            let prompt_string = trimmed.to_string();
            return super::llama_cli::generate_prompt_stream_async(
                &cli,
                &model,
                &prompt_string,
                256,
                tx,
            )
            .await;
        }

        let fallback = self.reflex_response(trimmed);
        let _ = tx.send(fallback).await;
        Ok(())
    }

    fn reflex_response(&self, trimmed: &str) -> String {
        let lower = trimmed.to_lowercase();
        let response = if lower.contains("who are you") || lower.contains("your name") {
            "I am Aqua AI, a privacy-first, CPU-optimized local desktop assistant.".to_string()
        } else if lower.contains("hello") || lower.contains("hi") {
            "Hello! I am Aqua AI. How can I assist you with your desktop tasks today?".to_string()
        } else if lower.contains("aqua") {
            "Project Aqua is an ultra-low-latency desktop assistant operating with <200MB execution RAM.".to_string()
        } else {
            format!("Aqua AI: Processed prompt about '{trimmed}'.")
        };

        let label = self.active_model_name();
        if self.is_model_loaded() {
            format!("Aqua AI ({label}): {response}")
        } else {
            format!("Aqua Reflex Engine: {response}")
        }
    }

    /// Is a GGUF model in the models folder right now? (Checked live, not once at start.)
    pub fn is_model_loaded(&self) -> bool {
        model_bundle_valid(&self.model_dir)
    }

    pub fn is_engine_ready(&self) -> bool {
        self.is_model_loaded()
    }

    pub fn model_dir(&self) -> &Path {
        &self.model_dir
    }

    pub fn model_path(&self) -> PathBuf {
        self.active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .model_path
            .clone()
    }

    pub fn active_model_name(&self) -> String {
        self.active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_model_name
            .clone()
    }

    pub fn active_model_filename(&self) -> String {
        self.model_path()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(MODEL_FILENAME)
            .to_string()
    }

    /// The context window one request can use: the server's total, divided between its cache slots.
    pub fn max_context_tokens(&self) -> u32 {
        super::llama_server::context_tokens() / super::llama_server::slots()
    }

    /// Prefer a persisted / requested filename when present on disk.
    pub fn prefer_model_if_present(&self, filename: &str) -> Result<()> {
        if filename.trim().is_empty() {
            bail!("empty model filename");
        }
        self.set_active_model(filename)
    }
}

/// The model layer's part of the engine (card OA9-03): which model each role uses, what family a model is, and what it is spoken to by.
impl LlamaRouterEngine {
    /// Runs `f` with the table the person made and what the resolver needs to know of the folder and the environment.
    fn with_role_inputs<R>(&self, f: impl FnOnce(&RoleAssignments, &roles::Inputs<'_>) -> R) -> R {
        let models = list_gguf_models(&self.model_dir);
        let installed: Vec<String> = models.iter().map(|m| m.filename.clone()).collect();
        let env = std::env::var("AQUA_AGENT_MODEL").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let preferred = preferred_by_rule(&models);
        let active = self.active_model_filename();
        let assigned = self.roles.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&assigned, &roles::Inputs { installed: &installed, active: &active, env_agent: env.as_deref(), preferred: preferred.as_deref() })
    }

    /// The model of one role and why: what the person chose for it, else `AQUA_AGENT_MODEL` (for the agent), else the Qwen3-8B preference (for the agent), else the agent's model.
    pub fn role_choice(&self, role: Role) -> RoleChoice {
        self.with_role_inputs(|assigned, inputs| roles::resolve(role, assigned, inputs))
    }

    /// The file name of the model a role uses.
    pub fn model_filename_for(&self, role: Role) -> String {
        self.role_choice(role).model
    }

    /// The file names of the models in the model folder (no header is read: for a list of names).
    pub fn installed_filenames(&self) -> Vec<String> {
        list_gguf_models(&self.model_dir).into_iter().map(|m| m.filename).collect()
    }

    /// The model of every role, in the order of [`Role::ALL`].
    pub fn role_choices(&self) -> Vec<RoleChoice> {
        self.with_role_inputs(|assigned, inputs| Role::ALL.iter().map(|r| roles::resolve(*r, assigned, inputs)).collect())
    }

    /// What is worth saying about the roles as a whole (two roles on two models make the server swap them).
    pub fn role_warnings(&self) -> Vec<String> {
        roles::warnings(&self.role_choices(), &|file| self.profiles.for_file(file).and_then(|p| p.swap_seconds))
    }

    /// Why `profiles.json` or `router.json` was not used, in sentences (empty when both were fine or are not there).
    pub fn model_file_problems(&self) -> Vec<String> {
        let assigned = self.roles.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        self.profiles.problems.iter().chain(assigned.problems.iter()).cloned().collect()
    }

    /// Gives roles a model of the model folder, or takes a choice away (`None`: the role goes back to the default rule). Every name is checked before anything changes, and the file
    /// `router.json` is written whole or not at all: on an error no role has changed.
    pub fn assign_roles(&self, changes: &[(Role, Option<String>)]) -> Result<Vec<RoleChoice>> {
        let installed: Vec<String> = list_gguf_models(&self.model_dir).into_iter().map(|m| m.filename).collect();
        let mut next = self.roles.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        for (role, model) in changes {
            match model {
                None => next.set(*role, None),
                Some(reference) => {
                    let name = roles::parse_reference(reference).map_err(|why| anyhow::anyhow!(why))?;
                    let found = installed.iter().find(|f| f.eq_ignore_ascii_case(&name)).ok_or_else(|| anyhow::anyhow!("there is no model called {name} in the model folder"))?;
                    next.set(*role, Some(found.clone()));
                }
            }
        }
        if let Some(path) = &self.router_file {
            next.save(path).map_err(|e| anyhow::anyhow!("the choice could not be saved to {} ({}), so nothing was changed", roles::FILE, e.kind()))?;
        }
        *self.roles.write().unwrap_or_else(std::sync::PoisonError::into_inner) = next;
        Ok(self.role_choices())
    }

    /// The Use button: this model is the active one and is chosen for the agent and for chat (the other roles follow the agent), so that the Qwen3-8B preference no longer hides it
    /// (finding D-PR-2).
    pub fn use_model(&self, filename: &str) -> Result<()> {
        if resolve_named_model(&self.model_dir, filename).is_none() {
            bail!("GGUF not found or invalid name: {filename}");
        }
        self.assign_roles(&[(Role::Agent, Some(filename.to_string())), (Role::Chat, Some(filename.to_string()))])?;
        self.set_active_model(filename)
    }

    /// A model was deleted: no role keeps naming it. (A role that does is read as unset, with a note, so this only keeps the file tidy; a failure to write it is not an error.)
    fn forget_role_choices_of(&self, filename: &str) {
        let mut next = self.roles.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        let named: Vec<Role> = next.assigned().into_iter().filter(|(_, m)| m.eq_ignore_ascii_case(filename)).map(|(r, _)| r).collect();
        if named.is_empty() {
            return;
        }
        for role in named {
            next.set(role, None);
        }
        if let Some(path) = &self.router_file {
            if let Err(e) = next.save(path) {
                warn!("{} could not be written after {filename} was deleted ({})", roles::FILE, e.kind());
            }
        }
        *self.roles.write().unwrap_or_else(std::sync::PoisonError::into_inner) = next;
    }

    /// The family of the model file: its profile's word, else its header's architecture, else its name.
    fn family_of(&self, model_path: &Path) -> Family {
        let name = model_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let header = gguf::read_cached(model_path).ok();
        profile::family_for(name, header.as_ref(), self.profiles.for_file(name))
    }

    /// What speaks to this model: its family's adapter with `AQUA_MODEL_PROFILES` on, the one old recipe with it off (nothing is read from the file then).
    fn adapter_of(&self, model_path: &Path) -> &'static dyn ModelAdapter {
        if adapter::profiles_on() {
            adapter::adapter_for(&self.family_of(model_path))
        } else {
            adapter::legacy()
        }
    }

    /// The sampling and thinking settings of a call, or `None` when they add nothing to the request the engine has always sent (the usual case: nothing is read from the file then).
    fn settings_for(&self, role: Role, model_path: &Path) -> Option<crate::model::sampling::ModelSettings> {
        if !adapter::settings_wanted() {
            return None;
        }
        let family = self.family_of(model_path);
        let name = model_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let profile_sampling = self.profiles.for_file(name).map(profile::ModelProfile::sampling).unwrap_or_default();
        let role_sampling = self.profiles.for_role(role);
        adapter::settings_for(&adapter::CallInputs { role, family: &family, profile_sampling: &profile_sampling, role_sampling: &role_sampling })
    }

    /// The profile file as it was read (for the status of the models).
    pub fn profiles(&self) -> &Profiles {
        &self.profiles
    }
}

/// How many tokens a chat answer may take: a few sentences, said aloud.
const CHAT_REPLY_TOKENS: u32 = 256;

thread_local! {
    static LAST_TIMINGS: std::cell::Cell<Option<super::llama_server::Timings>> = const { std::cell::Cell::new(None) };
    /// The role the bridge is told for the next call this thread makes, when it is not the one the call's shape says (see [`ThisCallIs`]). `None` in every call that is not the fast tier's.
    static THIS_CALL_IS: std::cell::Cell<Option<crate::model::Role>> = const { std::cell::Cell::new(None) };
}

/// Says, until it is dropped, which role the calls of this thread are (card OA9-06: a rewrite of the dictation is the role fast, which the fast tier may answer with the small model).
struct ThisCallIs(Option<crate::model::Role>);

impl ThisCallIs {
    fn new(role: crate::model::Role) -> ThisCallIs {
        ThisCallIs(THIS_CALL_IS.with(|c| c.replace(Some(role))))
    }
}

impl Drop for ThisCallIs {
    fn drop(&mut self) {
        THIS_CALL_IS.with(|c| c.set(self.0.take()));
    }
}

/// What the model server measured for the last planner request this thread made (once; `None` if it made none, or the answer came from
/// `llama-cli`, which measures nothing).
pub fn take_last_timings() -> Option<super::llama_server::Timings> {
    LAST_TIMINGS.with(|t| t.take())
}

thread_local! {
    /// Set when the model server could not be used for a call of this thread (it would not start, it crashed, it did not answer) and the call went on to `llama-cli`.
    static SERVER_DOWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the model server could not be used since this was last asked (once: the flag is cleared). The planner asks it after a call that failed, because the error that
/// reaches it is the one of `llama-cli` (or of the missing one), which does not say that the server was what failed.
pub fn take_server_down() -> bool {
    SERVER_DOWN.with(|c| c.replace(false))
}

/// The tokens a reply may have in a window of `window` tokens that a prompt of `estimated` tokens already takes, less the margin that is kept for what the estimate gets wrong.
fn reply_headroom(window: u32, estimated: usize) -> u32 {
    (window as usize).saturating_sub(estimated + crate::modules::context::budget::margin_tokens(window)).min(u32::MAX as usize) as u32
}

/// The error for a reply that was cut off at the token limit twice, or once with no room to ask for more. When the prompt takes 85 % of the window or more, the window ended the reply
/// and not the limit of tokens, so it is a prompt that does not fit (and the planner shortens it once); otherwise the reply was too long for its limit.
fn cut_off_error(estimated_prompt: usize, window: u32) -> anyhow::Error {
    let plain = "the model's reply was cut off at the token limit, so it cannot be used";
    if estimated_prompt * 100 >= window as usize * 85 {
        anyhow::Error::new(crate::model::ProviderError::ContextOverflow {
            limit: Some(window),
            detail: format!("{plain} (the prompt takes about {estimated_prompt} of the {window} tokens of the window)"),
        })
    } else {
        anyhow::Error::new(ReplyCutOff)
    }
}

/// The error for a request the server refused: a prompt that did not fit the window is a [`crate::model::ProviderError::ContextOverflow`] (the planner shortens it and asks again; it is
/// not asked again without its constraint, which would be refused for the same reason), any other refusal is a [`RequestRefused`]. The message is the server layer's, word for word.
fn refusal_error(message: String) -> anyhow::Error {
    match crate::model::error::context_overflow(&message) {
        Some(limit) => anyhow::Error::new(crate::model::ProviderError::ContextOverflow { limit, detail: message }),
        None => anyhow::Error::new(RequestRefused(message)),
    }
}

/// Did the server refuse a request because of the constraint it carried? (Then the request is asked again without it.)
fn refused_the_constraint(constraint: Option<&super::llama_server::Constraint>, e: &anyhow::Error) -> bool {
    constraint.is_some() && e.downcast_ref::<RequestRefused>().is_some()
}

/// The model's reply to the planner was cut off at the token limit twice over (it was asked for again with twice the room), so what there is of it cannot be used: half a tool call is not
/// one. The coding loop tells the model to write less per call (card OA6-01, ED-14); any other run ends with this message.
#[derive(Debug)]
pub struct ReplyCutOff;

impl std::fmt::Display for ReplyCutOff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the model's reply was cut off at the token limit, so it cannot be used")
    }
}

impl std::error::Error for ReplyCutOff {}

/// Is this error the one of a reply cut off at the token limit ([`ReplyCutOff`])?
pub fn is_cut_off(error: &anyhow::Error) -> bool {
    // A prompt that took the window (the planner shortens it and asks again) is not a reply that was too long.
    if error.downcast_ref::<crate::model::ProviderError>().is_some_and(|e| matches!(e, crate::model::ProviderError::ContextOverflow { .. })) {
        return false;
    }
    error.downcast_ref::<ReplyCutOff>().is_some() || format!("{error:#}").contains("reply was cut off at the token limit")
}

/// The model server answered and refused the request (a prompt past its window, a field it does not take). It is not a failure of the server.
#[derive(Debug)]
pub struct RequestRefused(pub String);

impl std::fmt::Display for RequestRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RequestRefused {}

/// A chat answer that was handed over as it was written (see [`LlamaRouterEngine::generate_chat_streaming`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StreamedChat {
    /// The whole answer, cleaned, and cut at its last complete sentence when the model ran out of tokens.
    pub text: String,
    /// The model ran out of tokens before it finished (the end of what it was saying was not heard).
    pub cut_off: bool,
}

/// The reply with the model's thinking taken out by the rules of the family that wrote it, and that family remembered for the places that clean a reply a second time.
fn clean_reply(model_adapter: &dyn ModelAdapter, text: &str) -> String {
    adapter::note_answered_by(model_adapter);
    model_adapter.clean_output(text)
}

/// The file the Qwen3-8B preference picks among `models`: Qwen3-8B-Q4_K_M (in its three spellings), else any Qwen3 8B file, else none.
fn preferred_by_rule(models: &[GgufModelInfo]) -> Option<String> {
    // Exact / preferred quality model for MCP agent planning.
    const PREFERRED: &[&str] = &[
        "Qwen3-8B-Q4_K_M.gguf",
        "Qwen3-8B-Q4-K-M.gguf",
        "qwen3-8b-q4_k_m.gguf",
    ];
    for pref in PREFERRED {
        if let Some(m) = models.iter().find(|m| m.filename.eq_ignore_ascii_case(pref)) {
            return Some(m.filename.clone());
        }
    }
    // Any Qwen3 8B on disk.
    models
        .iter()
        .find(|m| {
            let n = m.filename.to_ascii_lowercase();
            n.contains("qwen3") && n.contains("8b")
        })
        .map(|m| m.filename.clone())
}

/// Remove Qwen/OpenAI-style chain-of-thought blocks so that they are neither shown nor spoken. Only the blocks go (tags in any case): the
/// answer's own line breaks, the blank line between paragraphs and the indentation of code and lists stay, a run of blank lines left by a
/// removed block becomes one, and the ends are trimmed. A block that is never closed takes the rest of the text with it, and a closing tag
/// with no opening one (the model's thinking began in the prompt) takes the text before it. (It used to trim every line and drop every
/// blank line, which flattened Python and nested lists; speech makes its own plain text, see `speech_stream::for_speech`.)
///
/// **By which rules** is the model layer's say (`crate::model::adapter`): with `AQUA_MODEL_PROFILES` off, which is the default, always the ones described here; with it on, the rules
/// of the family that answered last (a family that does not think in `<reasoning>` tags keeps them when they are its own words).
pub fn strip_reasoning_tags(text: &str) -> String {
    match adapter::current_policy() {
        adapter::CleanPolicy::Legacy => strip_legacy(text),
        policy => adapter::clean_with(policy, text),
    }
}

/// [`strip_reasoning_tags`] by the rules it has always had.
pub(crate) fn strip_legacy(text: &str) -> String {
    let mut out = text.to_string();
    for (open, close) in [("<think>", "</think>"), ("<thinking>", "</thinking>"), ("<reason>", "</reason>"), ("<reasoning>", "</reasoning>")] {
        loop {
            // The tags are ASCII, so lower-casing ASCII keeps every position the same.
            let lower = out.to_ascii_lowercase();
            match (lower.find(open), lower.find(close)) {
                (Some(o), Some(c)) if c < o => out.replace_range(..c + close.len(), ""),
                (Some(o), _) => match lower[o..].find(close) {
                    Some(rel) => out.replace_range(o..o + rel + close.len(), ""),
                    None => {
                        out.truncate(o);
                        break;
                    }
                },
                (None, Some(c)) => out.replace_range(..c + close.len(), ""),
                (None, None) => break,
            }
        }
    }
    tidy_lines(&out)
}

/// The text with the end of every line trimmed, a run of blank lines made one and the blank lines at both ends dropped (what is left when a block of thinking is taken out).
pub(crate) fn tidy_lines(out: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in out.lines().map(str::trim_end) {
        if line.is_empty() && lines.last().is_none_or(|last| last.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    while lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// What to return when neither llama-server nor llama-cli produced an answer.
///
/// How many tokens the planner may write in one reply. A tool call is a short JSON object (a long typed text can reach 150),
/// an answer is a few sentences, and a code edit carries the code. The numbers are caps, not targets: a model that is done
/// stops on its own, so a larger cap costs nothing except when it rambles (about 4 to 5 tokens a second on this CPU).
pub const PLANNER_TOOL_TOKENS: u32 = 200;
pub const PLANNER_ANSWER_TOKENS: u32 = 320;
pub const PLANNER_CODE_TOKENS: u32 = 640;
/// For a caller that does not say (the flows' completer): room for a short JSON answer or a short text.
const DEFAULT_REPLY_TOKENS: u32 = 512;
/// No reply is ever allowed more than this, even when a cut-off reply is asked for again with twice the room.
pub(crate) const MAX_REPLY_TOKENS: u32 = 1200;

/// How much text the warm-up has the model read at a time (about 400 tokens: 15 s on this CPU, 2 s on a GPU).
pub(crate) const PREFILL_CHUNK_CHARS: usize = 1_600;

/// The pieces in which a long prompt start is read ahead of time: each piece is the whole text up to a point, so the model's
/// cache extends from one piece to the next, and the last piece is the whole text. Pieces end on a character boundary, and
/// preferably on a line break so that the text read so far is a natural start of the real prompt.
pub(crate) fn prefix_pieces(text: &str, chunk_chars: usize) -> Vec<String> {
    let total = text.chars().count();
    if total == 0 {
        return vec![];
    }
    let mut ends: Vec<usize> = Vec::new();
    let mut at = chunk_chars.max(1);
    while at < total {
        // Back to the last line break within the last quarter of the piece, if there is one.
        let start = at.saturating_sub(chunk_chars / 4);
        let cut = text.chars().enumerate().skip(start).take(at - start + 1).filter(|(_, c)| *c == '\n').map(|(i, _)| i + 1).last().unwrap_or(at);
        ends.push(cut);
        at = cut + chunk_chars.max(1);
    }
    ends.push(total);
    ends.dedup();
    ends.into_iter().map(|n| text.chars().take(n).collect()).collect()
}

/// What to do when the model ran out of tokens in the middle of a reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OnCutOff {
    /// A tool call or a JSON answer: a cut-off one is useless. Ask once more with twice the room; fail if that is cut off too.
    RetryThenFail,
    /// Spoken text: keep what was said, up to the last complete sentence.
    EndAtSentence,
}

/// Ask the model server (`ask` takes the token room) and deal with a reply that was cut off according to `on_cut_off`.
/// For [`OnCutOff::RetryThenFail`] the question is asked again once, with twice the room (never more than the cap), and never with more room than the window has: `headroom` is the
/// tokens the window has left for a reply after the prompt and the margin (`None`: not known), and a retry that would not have at least a quarter more room than the first is not made
/// (the prompt is what fills the window, and asking again on the same prompt would fail the same way, after minutes on a CPU).
fn ask_with_cut_off_handling(
    max_tokens: u32,
    on_cut_off: OnCutOff,
    headroom: Option<u32>,
    mut ask: impl FnMut(u32) -> Result<super::llama_server::Reply, super::llama_server::Failure>,
) -> Result<super::llama_server::Reply, super::llama_server::Failure> {
    let reply = ask(max_tokens)?;
    if reply.cut_off && on_cut_off == OnCutOff::RetryThenFail {
        let wanted = max_tokens.saturating_mul(2).min(MAX_REPLY_TOKENS);
        let more = headroom.map_or(wanted, |h| wanted.min(h));
        if more >= max_tokens + max_tokens / 4 {
            warn!("The model's reply was cut off at {max_tokens} tokens; asking again with room for {more}");
            return ask(more);
        }
        if more < wanted {
            warn!("The model's reply was cut off at {max_tokens} tokens; the window has room for {more} and not for more, so it is not asked again");
        }
    }
    Ok(reply)
}

/// Text that was cut off at the token limit, ended at its last complete sentence (a ".", "!" or "?" followed by a space or
/// the end). A number such as "3.5" is not an end. With no complete sentence the text is returned as it is.
fn end_at_last_sentence(text: &str) -> String {
    let t = text.trim_end();
    let chars: Vec<(usize, char)> = t.char_indices().collect();
    let mut cut = None;
    for (k, (i, c)) in chars.iter().enumerate() {
        if matches!(c, '.' | '!' | '?') && chars.get(k + 1).is_none_or(|(_, next)| next.is_whitespace()) {
            cut = Some(i + c.len_utf8());
        }
    }
    match cut {
        // A sentence of a few letters ("Hi.", "Dr.") is more likely an abbreviation than the answer.
        Some(end) if end >= 5 => t[..end].to_string(),
        _ => t.to_string(),
    }
}

/// Chat may fall back to the built-in reflex reply (a canned greeting), because a friendly stub is
/// better than nothing there. The agent's planner may not: it read that greeting as the model's
/// plan, asked again five times, and finally showed "Hello! I am Aqua AI..." to the user as if it
/// were an answer to their task. For the planner a missing engine is an error that says what is missing.
fn no_engine_answer(agent_planner: bool, model_present: bool, cli_present: bool, reflex: impl FnOnce() -> String) -> Result<String> {
    if !agent_planner {
        return Ok(reflex());
    }
    let why = if !model_present {
        "the model file is missing (see data/models/llm)"
    } else if !cli_present {
        "llama-cli was not found (run scripts/build_llama_cli.ps1)"
    } else {
        "it returned no text"
    };
    Err(anyhow::anyhow!("The local language model could not run: {why}. The agent cannot plan without it."))
}

#[cfg(test)]
mod budget_tests {
    use crate::modules::llm_router::llama_server::{Failure, Reply};
    use super::{ask_with_cut_off_handling, cut_off_error, end_at_last_sentence, prefix_pieces, refusal_error, reply_headroom, take_server_down, OnCutOff, RequestRefused, MAX_REPLY_TOKENS, PLANNER_ANSWER_TOKENS, PLANNER_CODE_TOKENS, PLANNER_TOOL_TOKENS, SERVER_DOWN};

    fn reply(text: &str, cut_off: bool) -> Result<Reply, Failure> {
        Ok(Reply { text: text.into(), cut_off, ..Default::default() })
    }

    #[test]
    fn a_long_prompt_start_is_read_in_pieces_that_each_extend_the_last() {
        let text: String = (0..120).map(|i| format!("line {i} of the rules of the agent\n")).collect();
        let pieces = prefix_pieces(&text, 1_000);
        assert!(pieces.len() >= 4, "{}", pieces.len());
        for pair in pieces.windows(2) {
            assert!(pair[1].starts_with(&pair[0]) && pair[1].len() > pair[0].len(), "each piece continues the one before");
        }
        assert_eq!(pieces.last().unwrap(), &text, "the last piece is the whole text");
        // Pieces end after a line break when there is one close to the target.
        assert!(pieces[..pieces.len() - 1].iter().all(|p| p.ends_with('\n')), "{:?}", pieces.iter().map(|p| p.chars().last()).collect::<Vec<_>>());
        // Short and awkward texts.
        assert_eq!(prefix_pieces("short", 1_000), ["short"]);
        assert!(prefix_pieces("", 1_000).is_empty());
        let accents = "é".repeat(2_500);
        let p = prefix_pieces(&accents, 1_000);
        assert_eq!((p.len(), p.last().unwrap().chars().count()), (3, 2_500), "cut on characters, never inside one");
    }

    #[test]
    fn a_planner_reply_that_was_cut_off_is_asked_for_again_once_with_twice_the_room() {
        let mut asked = Vec::new();
        let got = ask_with_cut_off_handling(200, OnCutOff::RetryThenFail, None, |room| {
            asked.push(room);
            if room < 400 { reply("{\"tool\": \"App\", \"argum", true) } else { reply("{\"tool\": \"App\", \"arguments\": {}}", false) }
        })
        .unwrap();
        assert_eq!(asked, [200, 400]);
        assert!(!got.cut_off && got.text.ends_with("{}}"));
    }

    #[test]
    fn a_planner_reply_that_is_cut_off_again_is_reported_not_retried_forever() {
        let mut asked = Vec::new();
        let got = ask_with_cut_off_handling(200, OnCutOff::RetryThenFail, None, |room| {
            asked.push(room);
            reply("{\"final\": \"a very long", true)
        })
        .unwrap();
        assert_eq!(asked, [200, 400], "one retry only");
        assert!(got.cut_off, "the caller sees that it is still cut off");
        // The cap: a reply that already has the most room is not asked again.
        let mut asked = Vec::new();
        let _ = ask_with_cut_off_handling(MAX_REPLY_TOKENS, OnCutOff::RetryThenFail, None, |room| {
            asked.push(room);
            reply("{", true)
        });
        assert_eq!(asked, [MAX_REPLY_TOKENS]);
    }

    /// Card OA4-02 (CX-03): a reply that was cut off is asked for again with twice the room only when the window has the room; on a full window the second request would fail the
    /// same way after minutes of reading the same prompt.
    #[test]
    fn a_cut_reply_is_not_retried_at_double_when_the_window_is_full() {
        let asked_with = |headroom: Option<u32>| {
            let mut asked = Vec::new();
            let _ = ask_with_cut_off_handling(200, OnCutOff::RetryThenFail, headroom, |room| {
                asked.push(room);
                reply("{\"final\": \"a very long", true)
            });
            asked
        };
        assert_eq!(asked_with(None), [200, 400], "room not known: as before");
        assert_eq!(asked_with(Some(1000)), [200, 400], "room for double");
        assert_eq!(asked_with(Some(250)), [200, 250], "room for a quarter more and not for double: what the window has");
        assert_eq!(asked_with(Some(249)), [200], "less than a quarter more is not worth another read of the prompt");
        assert_eq!(asked_with(Some(0)), [200], "a full window");
        // The cap still holds.
        let mut asked = Vec::new();
        let _ = ask_with_cut_off_handling(800, OnCutOff::RetryThenFail, Some(5000), |room| {
            asked.push(room);
            reply("{", true)
        });
        assert_eq!(asked, [800, MAX_REPLY_TOKENS]);
        // And spoken text is never asked again whatever the room.
        let mut asked = Vec::new();
        let _ = ask_with_cut_off_handling(256, OnCutOff::EndAtSentence, Some(5000), |room| {
            asked.push(room);
            reply("One. Two", true)
        });
        assert_eq!(asked, [256]);
    }

    /// Card OA4-02 (CX-03): what the window has left for a reply, and which errors are a prompt that does not fit.
    #[test]
    fn the_room_for_a_reply_is_the_window_less_the_prompt_and_the_margin_and_a_prompt_that_fills_the_window_is_an_overflow() {
        assert_eq!(reply_headroom(6144, 4890), 640, "the largest prompt the planner fits for a code edit leaves exactly its reply");
        assert_eq!(reply_headroom(6144, 1400), 6144 - 1400 - 614);
        assert_eq!(reply_headroom(6144, 6000), 0, "never negative");
        // A reply cut off with the prompt at 85 % of the window or more is the window's end.
        let near = cut_off_error(5300, 6144);
        let typed = near.downcast_ref::<crate::model::ProviderError>().expect("a typed error");
        assert_eq!(typed.class(), "context_overflow");
        assert!(near.to_string().starts_with("the model's reply was cut off at the token limit, so it cannot be used (the prompt takes about 5300 of the 6144 tokens"), "{near}");
        // With room to spare it is the reply that was too long for its limit, in the words it has always had.
        let far = cut_off_error(1500, 6144);
        assert!(far.downcast_ref::<crate::model::ProviderError>().is_none());
        assert_eq!(far.to_string(), "the model's reply was cut off at the token limit, so it cannot be used");
        // A refusal for a prompt that did not fit is an overflow; any other refusal is a refusal, and both keep the server layer's words.
        let said = "the language model refused this request (HTTP 400: exceeds the available context size (6144 tokens; the prompt has 6234))";
        let overflow = refusal_error(said.to_string());
        assert_eq!(overflow.to_string(), said);
        assert!(matches!(overflow.downcast_ref::<crate::model::ProviderError>(), Some(crate::model::ProviderError::ContextOverflow { limit: Some(6144), .. })));
        assert!(overflow.downcast_ref::<RequestRefused>().is_none(), "so the request is not asked again without its constraint, which would be refused for the same reason");
        let other = refusal_error("the language model refused this request (HTTP 400: unsupported response_format)".to_string());
        assert!(other.downcast_ref::<RequestRefused>().is_some());
    }

    #[test]
    fn the_flag_that_says_the_server_was_down_is_read_once() {
        let _ = take_server_down();
        assert!(!take_server_down());
        SERVER_DOWN.with(|c| c.set(true));
        assert!(take_server_down());
        assert!(!take_server_down(), "and then it is gone");
    }

    #[test]
    fn spoken_text_is_not_asked_again_and_a_finished_reply_is_never_retried() {
        let mut asked = Vec::new();
        let _ = ask_with_cut_off_handling(256, OnCutOff::EndAtSentence, None, |room| {
            asked.push(room);
            reply("One. Two", true)
        });
        assert_eq!(asked, [256]);
        let mut asked = Vec::new();
        let _ = ask_with_cut_off_handling(200, OnCutOff::RetryThenFail, None, |room| {
            asked.push(room);
            reply("{}", false)
        });
        assert_eq!(asked, [200]);
        // A failure of the server passes through untouched.
        assert!(matches!(ask_with_cut_off_handling(200, OnCutOff::RetryThenFail, None, |_| Err(Failure::Cancelled)), Err(Failure::Cancelled)));
    }

    #[test]
    fn cut_off_speech_ends_at_its_last_complete_sentence() {
        assert_eq!(end_at_last_sentence("It is sunny today. The high is 3.5 degrees and the wind is"), "It is sunny today.");
        assert_eq!(end_at_last_sentence("Done! Next we will go to the sho"), "Done!");
        assert_eq!(end_at_last_sentence("A full answer."), "A full answer.");
        // No complete sentence, or only a very short one: nothing better to offer than what there is.
        assert_eq!(end_at_last_sentence("no end in sight and then"), "no end in sight and then");
        assert_eq!(end_at_last_sentence("Hi. and then more words here"), "Hi. and then more words here");
        // A number is not the end of a sentence.
        assert_eq!(end_at_last_sentence("The value is 3.5 and"), "The value is 3.5 and");
    }

    #[test]
    fn the_budgets_leave_room_for_what_each_kind_of_reply_needs() {
        // The old guess gave every reply 120 tokens: not enough for five headlines, a typed email, or a code edit.
        assert!(PLANNER_TOOL_TOKENS > 120 && PLANNER_TOOL_TOKENS < PLANNER_ANSWER_TOKENS && PLANNER_ANSWER_TOKENS < PLANNER_CODE_TOKENS);
        assert!(PLANNER_CODE_TOKENS < MAX_REPLY_TOKENS, "a cut-off code edit can still be asked for again with more room");
    }
}

#[cfg(test)]
mod strip_tests {
    use super::strip_reasoning_tags;

    #[test]
    fn strips_think_block() {
        let raw = "<think>\nplan weather\n</think>\n\nIt is sunny today.";
        assert_eq!(strip_reasoning_tags(raw), "It is sunny today.");
    }

    #[test]
    fn code_and_lists_keep_their_shape_and_paragraphs_stay_apart() {
        let code = "def total(items):\n    result = 0\n    for item in items:\n        if item.ok:\n            result += item.price\n\n    return result";
        assert_eq!(strip_reasoning_tags(&format!("<think>plan</think>\n\n{code}\n")), code, "six lines of indented Python come back as they went in");
        assert_eq!(strip_reasoning_tags("- one\n  - nested\n    - deeper\n- two"), "- one\n  - nested\n    - deeper\n- two");
        assert_eq!(strip_reasoning_tags("First paragraph.\n\nSecond paragraph.\n\n\n\nThird."), "First paragraph.\n\nSecond paragraph.\n\nThird.", "a run of blank lines is one");
        assert_eq!(strip_reasoning_tags("Before.\n\n<think>hmm</think>\n\nAfter."), "Before.\n\nAfter.", "a removed block does not leave a gap");
        assert_eq!(strip_reasoning_tags("a  \nb \t\r\n\r\nc"), "a\nb\n\nc", "trailing spaces and Windows line ends go");
    }

    #[test]
    fn tags_in_any_case_and_unfinished_or_stray_ones_are_handled() {
        assert_eq!(strip_reasoning_tags("<THINK>x</Think>Hi"), "Hi");
        assert_eq!(strip_reasoning_tags("<reasoning>why</reasoning>\nThe answer."), "The answer.");
        assert_eq!(strip_reasoning_tags("<thinking>a</thinking><think>b</think>Done."), "Done.");
        assert_eq!(strip_reasoning_tags("Answer first.\n<think>and then it never ends"), "Answer first.", "an unclosed block takes the rest");
        assert_eq!(strip_reasoning_tags("it began thinking in the prompt...</think>\n\nThe answer."), "The answer.", "a stray closing tag takes what is before it");
        assert_eq!(strip_reasoning_tags("<think>only thinking</think>"), "");
        assert_eq!(strip_reasoning_tags("  \n\n"), "");
        assert_eq!(strip_reasoning_tags("Caf\u{e9} <think>\u{e9}</think>cr\u{e8}me"), "Caf\u{e9} cr\u{e8}me", "accents do not move the cut");
    }

    #[test]
    fn the_agent_planner_gets_an_error_naming_what_is_missing_but_chat_keeps_its_reflex_reply() {
        let reflex = || "Hello! I am Aqua AI.".to_string();
        // Chat: a friendly stub is fine.
        assert_eq!(super::no_engine_answer(false, false, false, reflex).unwrap(), "Hello! I am Aqua AI.");
        // The planner: never the greeting, always a reason.
        let no_cli = super::no_engine_answer(true, true, false, reflex).unwrap_err().to_string();
        assert!(no_cli.contains("llama-cli was not found") && no_cli.contains("build_llama_cli.ps1") && !no_cli.contains("Hello"), "{no_cli}");
        let no_model = super::no_engine_answer(true, false, true, reflex).unwrap_err().to_string();
        assert!(no_model.contains("model file is missing"), "{no_model}");
        let empty = super::no_engine_answer(true, true, true, reflex).unwrap_err().to_string();
        assert!(empty.contains("returned no text") && empty.contains("cannot plan"), "{empty}");
    }
}

#[cfg(test)]
mod warm_prefix_tests {
    use super::replaces_warm_prefix;

    #[test]
    fn a_prompt_that_starts_with_the_warmed_prefix_keeps_it_and_any_other_prompt_replaces_it() {
        let prefix = "You are Aqua, a local PC agent.\nTools:\n- fs.read";
        assert!(!replaces_warm_prefix(prefix, &format!("{prefix}\nGoal: read a.txt")), "the planner's own request extends the cached start");
        assert!(!replaces_warm_prefix(prefix, prefix));
        assert!(replaces_warm_prefix(prefix, "Which of these answers fits: yes, no or unsure?\nThe user said: open it"), "a decision read into the same slot replaces the cached start");
        assert!(replaces_warm_prefix(prefix, "You are Aqua, a local PC agent.\nTools:\n- fs.write"), "a prefix that differs by one tool is another prefix");
        assert!(replaces_warm_prefix(prefix, ""), "an empty prompt does not start with it");
    }
}

#[cfg(test)]
mod cut_off_tests {
    use super::*;

    #[test]
    fn is_cut_off_recognises_the_error() {
        let cut = anyhow::Error::new(ReplyCutOff);
        assert!(is_cut_off(&cut));
        assert_eq!(cut.to_string(), "the model's reply was cut off at the token limit, so it cannot be used", "the words the person saw before are the same");
        // Through a layer of context, and as plain text (an error that was turned into a message on its way).
        assert!(is_cut_off(&anyhow::Error::new(ReplyCutOff).context("LLM generate for MCP agent")));
        assert!(is_cut_off(&anyhow::anyhow!("the model's reply was cut off at the token limit, so it cannot be used")));
        // Other failures are not.
        for other in [anyhow::anyhow!("llama timed out after 90s"), anyhow::anyhow!("stopped: the task was cancelled"), anyhow::Error::new(RequestRefused("refused".into()))] {
            assert!(!is_cut_off(&other), "{other}");
        }
    }
}

#[cfg(test)]
mod constraint_retry_tests {
    use super::*;
    use super::super::llama_server::Constraint;

    #[test]
    fn only_a_refusal_of_a_constrained_request_is_asked_again_without_the_constraint() {
        let schema = Constraint::JsonSchema(serde_json::json!({"type": "object"}));
        let refused = anyhow::Error::new(RequestRefused("the language model refused this request (unsupported response_format)".into()));
        assert!(refused_the_constraint(Some(&schema), &refused), "a server that does not take the schema is asked again without it");
        assert!(!refused_the_constraint(None, &refused), "a request that had no constraint has nothing to drop");
        for other in [anyhow::anyhow!("llama timed out after 90s"), anyhow::anyhow!("stopped: the task was cancelled"), anyhow::anyhow!("the model's reply was cut off at the token limit, so it cannot be used")] {
            assert!(!refused_the_constraint(Some(&schema), &other), "{other}: not a refusal, so not asked a second time");
        }
        assert_eq!(refused.to_string(), "the language model refused this request (unsupported response_format)", "the message is the one the person would have seen");
    }
}
