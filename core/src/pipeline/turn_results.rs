//! What a turn returns and what a streamed turn tells its listener, and the note for a turn that was refused (move only: this was part of `pipeline.rs`).

use super::*;

#[derive(Debug, Clone, Serialize)]
pub struct PipelineWavResult {
    pub denoise_applied: bool,
    pub transcribed_text: String,
    pub assistant_text: String,
    /// Local session command handled without LLM: `"wake"`, `"sleep"`, or `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_action: Option<String>,
    /// True when assistant_text came from a local predefined reply (no llama.cpp).
    pub local_command: bool,
    pub tts_sample_count: usize,
    pub tts_wav_base64: String,
    pub tts_sample_rate: u32,
    pub whisper_engine_ready: bool,
    pub llm_engine_ready: bool,
    pub tts_engine_ready: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PipelineTextResult {
    pub input_text: String,
    pub assistant_text: String,
    pub tts_sample_count: usize,
    pub tts_wav_base64: String,
    pub tts_sample_rate: u32,
    pub llm_engine_ready: bool,
    pub tts_engine_ready: bool,
}

/// What a streamed turn tells its listener while it runs (see `run_text_to_speech_streaming`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEvent {
    /// What was heard, as soon as it is known to be the speaker's own request (voice turns only).
    Heard(String),
    /// One spoken piece of the answer: its words and its sound as a WAV file. The sound is empty when speech could not be made, so that
    /// the words still arrive.
    Speech { text: String, wav: Vec<u8> },
    /// What the turn is doing now ("Plan with llama.cpp", "Calling App"), so a client can show more than "thinking". `status` is
    /// `running`, `completed` or `failed`.
    Step { title: String, status: String },
}

/// The turn-level step that one step of an agent run comes to (card R6-22): its title and whether it is `running`, `completed` or `failed`, and nothing else. A step that
/// is not a phase of the run (a tool call with its arguments, a result) has no title and no status, and is `None`: what a call was given or returned is not told to a client
/// that only wants to show what Aqua is doing, because it can quote the request or a page. The stream sends what this builds as a `step` event.
pub fn step_event(step: &crate::modules::browser_agent::AgentStep) -> Option<TurnEvent> {
    // The same reduction of a step as the run's log and the API token's agent stream make theirs from (card OA10-01).
    let view = crate::agent_runs::events::PhaseView::of(step);
    Some(TurnEvent::Step { title: view.title?, status: view.status? })
}

/// The short sentence for a voice turn that was addressed to Aqua and then refused, in words a person can act on. `None` for the
/// outcomes that stay silent: nobody spoke to Aqua (the wake word was missing, or it was only noise), so there is nothing to answer.
pub fn refusal_note(assistant_text: &str) -> Option<&'static str> {
    if assistant_text.starts_with("ACCESS DENIED: Unauthorized speaker") {
        Some("I did not recognise your voice.")
    } else if assistant_text.starts_with("ACCESS DENIED") {
        Some("I could not check your voice just now. Please try again.")
    } else {
        None
    }
}

/// Receives the events of a streamed turn. It returns `false` when nobody is listening any more: the turn then stops at its next step.
pub type Listener<'a> = &'a mut dyn FnMut(TurnEvent) -> bool;
