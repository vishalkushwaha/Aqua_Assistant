//! The plain chat answer: the rules the chat model is always given, the prompt (rules, date, what was asked to be remembered, the last turns), and the reply (move only: this was part of `pipeline.rs`).

use super::*;

/// Rules the chat model is always given. Without them it answers as if it could see the world, and made up a
/// weather report when nothing had looked anything up.
pub(super) const CHAT_RULES: &str = "You are Aqua, a private assistant that runs on the user's own computer. In this reply you have no tools: \
you cannot look anything up and you do not know the time of day, the weather, news, prices, files or the screen. The date is given below. \
If the question needs live or personal information, say plainly that you cannot check it from here and suggest asking Aqua to look it up. \
Never invent current facts, and never say you did something you did not do. Answer briefly, in plain sentences that sound natural when spoken.";

pub(super) fn chat_prompt(text: &str) -> String {
    chat_prompt_with_rules(CHAT_RULES, text)
}

pub(super) fn chat_prompt_with_rules(rules: &str, text: &str) -> String {
    // The date comes after the rules, which are the same every day, so the model's cache of them survives midnight. The last few turns
    // (when there are any) come after the date, so the start of the prompt is the same with and without them.
    // What the person asked Aqua to remember comes before the turns, and neither changes the start of the prompt.
    let remembered = crate::memory::user_facts::context_block(text);
    chat_prompt_with_context(rules, text, remembered.as_deref())
}

fn chat_prompt_with_context(rules: &str, text: &str, remembered: Option<&str>) -> String {
    let remembered = remembered.map(|b| format!("{b}\n")).unwrap_or_default();
    format!("{rules}\n\nToday is {}.\n\n{remembered}{}The user said: {text}", chrono::Local::now().format("%A, %B %-d, %Y"), crate::conversation::context_for_chat())
}

/// The start of the chat prompt, for the model to read ahead of the first question (see `spawn_warmup` in the gateway).
pub fn chat_warmup_prompt() -> String {
    chat_prompt("Hello")
}

/// Plain chat. If the language model is not available this is an error that says so, never a canned greeting
/// presented as an answer.
pub(super) fn chat_reply(llm: &LlamaRouterEngine, prompt: &str, speaker: Option<&mut Speaker<'_>>) -> Result<String> {
    if !llm.is_engine_ready() {
        anyhow::bail!("the local language model is not available (model file or llama-cli missing; see data/models/llm and scripts/build_llama_cli.ps1)");
    }
    // The same model the agent uses: with two models the one server restarts every time a chat turn follows an agent
    // turn (and the other way round), losing its cache and about 30 seconds each time.
    let answer = match speaker {
        None => strip_reasoning_tags(&crate::turn_timing::stage("chat", || llm.generate_chat_response(&chat_prompt(prompt)))?),
        // Spoken as it is written. The speech is made on this thread between two bits of text, so its time is a stage of its own and
        // the chat stage is what is left.
        Some(speaker) => {
            let started = std::time::Instant::now();
            let speech_before = speaker.tts_ms;
            let streamed = llm.generate_chat_streaming(&chat_prompt(prompt), |delta| speaker.push(delta));
            crate::turn_timing::note("chat", (started.elapsed().as_millis() as u64).saturating_sub(speaker.tts_ms - speech_before));
            let chat = streamed?;
            if !speaker.finish(chat.cut_off) {
                anyhow::bail!("stopped: the task was cancelled");
            }
            chat.text
        }
    };
    if answer.trim().is_empty() {
        anyhow::bail!("the language model returned no answer");
    }
    Ok(answer)
}
