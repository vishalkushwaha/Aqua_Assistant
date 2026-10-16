//! A typed turn: the words go to the reply, and the answer is made into speech (the whole answer in one result, or piece by piece to a listener) or left as words (move only: this was part of `pipeline.rs`).

use super::*;

pub fn run_text_to_speech(
    llm: &LlamaRouterEngine,
    tts: &KokoroTtsEngine,
    text: &str,
    voice: &str,
    lang: &str,
) -> Result<PipelineTextResult> {
    run_text_to_speech_with_mcp(llm, tts, text, voice, lang, None, None, None)
}

pub fn run_text_to_speech_with_mcp(
    llm: &LlamaRouterEngine,
    tts: &KokoroTtsEngine,
    text: &str,
    voice: &str,
    lang: &str,
    browser: Option<Arc<BrowserBackend>>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    laya: Option<&Arc<LayaGate>>,
) -> Result<PipelineTextResult> {
    run_text_turn(llm, tts, text, voice, lang, browser, windows, laya, None, true)
}

/// [`run_text_to_speech_with_mcp`] for a client that plays the answer as it arrives. The spoken pieces of the answer go to `listener` as
/// each is ready (the first one while a chat answer is still being written), and the result carries no sound of its own: its
/// `tts_wav_base64` is empty and `tts_sample_count` counts the samples of all the pieces. When the listener leaves, the turn stops.
pub fn run_text_to_speech_streaming(
    llm: &LlamaRouterEngine,
    tts: &KokoroTtsEngine,
    text: &str,
    voice: &str,
    lang: &str,
    browser: Option<Arc<BrowserBackend>>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    laya: Option<&Arc<LayaGate>>,
    listener: Listener<'_>,
) -> Result<PipelineTextResult> {
    run_text_turn(llm, tts, text, voice, lang, browser, windows, laya, Some(listener), true)
}

/// [`run_text_to_speech_with_mcp`] and [`run_text_to_speech_streaming`] with the one choice they do not give: `speak` false is a text-only turn (card OA10-06, UX-08, `"tts": false`). Nothing
/// is synthesized and no speech engine is asked: the answer is the same text, `tts_wav_base64` is empty and `tts_sample_count` is 0, and a streamed turn still sends each piece of the answer as
/// a `Speech` event, with its words and no sound, in order. `listener` is the streamed turn's; `None` is the answer in one piece.
#[allow(clippy::too_many_arguments)]
pub fn run_text_turn_with_sound(
    llm: &LlamaRouterEngine,
    tts: &KokoroTtsEngine,
    text: &str,
    voice: &str,
    lang: &str,
    browser: Option<Arc<BrowserBackend>>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    laya: Option<&Arc<LayaGate>>,
    listener: Option<Listener<'_>>,
    speak: bool,
) -> Result<PipelineTextResult> {
    run_text_turn(llm, tts, text, voice, lang, browser, windows, laya, listener, speak)
}

#[allow(clippy::too_many_arguments)]
fn run_text_turn(
    llm: &LlamaRouterEngine,
    tts: &KokoroTtsEngine,
    text: &str,
    voice: &str,
    lang: &str,
    browser: Option<Arc<BrowserBackend>>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    laya: Option<&Arc<LayaGate>>,
    listener: Option<Listener<'_>>,
    speak: bool,
) -> Result<PipelineTextResult> {
    let input_text = text.trim().to_string();
    if input_text.is_empty() {
        return Ok(PipelineTextResult {
            input_text,
            assistant_text: String::new(),
            tts_sample_count: 0,
            tts_wav_base64: String::new(),
            tts_sample_rate: tts.sample_rate(),
            llm_engine_ready: llm.is_engine_ready(),
            tts_engine_ready: tts.is_engine_ready(),
        });
    }

    let mut speaker = listener.map(|listener| if speak { Speaker::new(tts, voice, lang, listener) } else { Speaker::text_only(tts, voice, lang, listener) });
    let assistant_text = generate_assistant_reply_speaking(llm, browser, windows, &input_text, Heard::Typed, laya, speaker.as_mut())?;
    // A text-only answer in one piece makes no sound at all; a streamed one still hands its pieces to the listener (by the text-only speaker), so it goes through the same end.
    let (tts_wav_base64, tts_sample_count, tts_sample_rate) =
        if !speak && speaker.is_none() { (String::new(), 0, tts.sample_rate()) } else { speech_for_result(tts, &assistant_text, voice, lang, speaker.as_mut())? };

    Ok(PipelineTextResult {
        input_text,
        assistant_text,
        tts_sample_count,
        tts_wav_base64,
        tts_sample_rate,
        llm_engine_ready: llm.is_engine_ready(),
        tts_engine_ready: tts.is_engine_ready(),
    })
}
