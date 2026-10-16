//! A spoken turn: a frame of audio or a recording through the speech gate, the noise remover, Whisper, the wake and voice checks and the session commands, then the reply and its speech (move only: this was part of `pipeline.rs`).

use super::*;

pub fn run_audio_frame_to_speech(
    suppressor: &NoiseSuppressor,
    stt: &WhisperSttEngine,
    llm: &LlamaRouterEngine,
    tts: &KokoroTtsEngine,
    bio: &mut VoiceBiometricsEngine,
    security_enabled: bool,
    wakeup_word: &str,
    active_session: bool,
    noisy_audio_frame: &[f32],
) -> Result<(String, String, Vec<f32>)> {
    run_audio_frame_to_speech_with_mcp(
        suppressor,
        stt,
        llm,
        tts,
        bio,
        security_enabled,
        wakeup_word,
        active_session,
        noisy_audio_frame,
        None,
        None,
        None,
    )
}

/// The sound of the note for a refused turn, or nothing when there is none to say or speech cannot be made: a refusal must never turn into
/// an error because the speech engine is down.
fn refusal_audio(tts: &KokoroTtsEngine, reply: &str) -> Vec<f32> {
    refusal_note(reply).and_then(|note| tts.synthesize(note).ok()).unwrap_or_default()
}

pub fn run_audio_frame_to_speech_with_mcp(
    suppressor: &NoiseSuppressor,
    stt: &WhisperSttEngine,
    llm: &LlamaRouterEngine,
    tts: &KokoroTtsEngine,
    bio: &mut VoiceBiometricsEngine,
    security_enabled: bool,
    wakeup_word: &str,
    active_session: bool,
    noisy_audio_frame: &[f32],
    browser: Option<Arc<BrowserBackend>>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    laya: Option<&Arc<LayaGate>>,
) -> Result<(String, String, Vec<f32>)> {
    if is_not_speech(stt, noisy_audio_frame) {
        return Ok((String::new(), NOT_SPEECH.to_string(), Vec::new()));
    }
    let clean_frame = crate::turn_timing::stage("denoise", || suppressor.process_frame(noisy_audio_frame));
    let transcribed_text = crate::turn_timing::stage("stt", || stt.transcribe(&clean_frame))?;
    if transcribed_text.trim().is_empty() || is_invented_by_whisper(&transcribed_text, active_session, loudest_second_rms(&clean_frame), wakeup_word) {
        return Ok((String::new(), NO_SPEECH.to_string(), Vec::new()));
    }

    if !passes_wake_gate(&transcribed_text, wakeup_word, active_session) {
        return Ok((transcribed_text, "IGNORED: Wake-word not detected.".to_string(), Vec::new()));
    }

    if security_enabled {
        // Fail closed: if the voiceprint cannot be computed the speaker is not verified.
        // The raw frame, as enrolment and the WAV route use: a voiceprint made from audio that went through the noise
        // gate does not look like one made from the recording the voice was enrolled with, so a right voice would be
        // refused on this route and not on the other.
        match crate::turn_timing::stage("voiceprint", || bio.extract_embedding(noisy_audio_frame)) {
            Ok(emb) => {
                let (authorized, sim) = bio.verify_speaker(&emb);
                if !authorized {
                    let reply = format!("ACCESS DENIED: Unauthorized speaker ({:.1}% match).", sim * 100.0);
                    let audio = refusal_audio(tts, &reply);
                    return Ok((transcribed_text, reply, audio));
                }
            }
            Err(e) => {
                warn!("Speaker verification failed: {e:#}");
                let reply = "ACCESS DENIED: could not verify the speaker.".to_string();
                let audio = refusal_audio(tts, &reply);
                return Ok((transcribed_text, reply, audio));
            }
        }
    }

    // Wake/sleep stay string-matched (free). Everything else goes through Laya in generate_assistant_reply.
    match resolve_session_command(&transcribed_text, wakeup_word, active_session) {
        CommandHandling::Speak { text, .. } => {
            info!("Local session command → TTS (skipping llama.cpp): {text}");
            let audio_speech_output = tts.synthesize(&text)?;
            return Ok((transcribed_text, text, audio_speech_output));
        }
        CommandHandling::AlreadyInState { command } => {
            info!(
                "Local session command {:?} ignored — already in that state",
                command
            );
            return Ok((transcribed_text, String::new(), Vec::new()));
        }
        CommandHandling::NotACommand => {}
    }

    let response_text = generate_assistant_reply(llm, browser, windows, &transcribed_text, Heard::Spoken { wake_phrase: wakeup_word }, laya)?;
    let audio_speech_output = crate::turn_timing::stage("tts", || tts.synthesize(&response_text))?;

    Ok((transcribed_text, response_text, audio_speech_output))
}

/// A turn that ends here, saying nothing.
fn ignored_result(stt: &WhisperSttEngine, llm: &LlamaRouterEngine, tts: &KokoroTtsEngine, text: &str) -> PipelineWavResult {
    PipelineWavResult {
        denoise_applied: false,
        transcribed_text: String::new(),
        assistant_text: text.to_string(),
        session_action: None,
        local_command: false,
        tts_sample_count: 0,
        tts_wav_base64: String::new(),
        tts_sample_rate: tts.sample_rate(),
        whisper_engine_ready: stt.is_engine_ready(),
        llm_engine_ready: llm.is_engine_ready(),
        tts_engine_ready: tts.is_engine_ready(),
    }
}

/// Removes a scratch folder when it goes out of scope, whichever way the function leaves.
struct ScratchDir(PathBuf);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn run_wav_file_to_speech(
    suppressor: &NoiseSuppressor,
    stt: &WhisperSttEngine,
    llm: &LlamaRouterEngine,
    tts: &KokoroTtsEngine,
    bio: &mut VoiceBiometricsEngine,
    security_enabled: bool,
    wakeup_word: &str,
    active_session: bool,
    deepfilter_model_dir: &Path,
    input_wav: &Path,
    voice: &str,
    lang: &str,
) -> Result<PipelineWavResult> {
    run_wav_file_to_speech_with_mcp(
        suppressor,
        stt,
        llm,
        tts,
        bio,
        security_enabled,
        wakeup_word,
        active_session,
        deepfilter_model_dir,
        input_wav,
        voice,
        lang,
        None,
        None,
        None,
    )
}

pub fn run_wav_file_to_speech_with_mcp(
    suppressor: &NoiseSuppressor,
    stt: &WhisperSttEngine,
    llm: &LlamaRouterEngine,
    tts: &KokoroTtsEngine,
    bio: &mut VoiceBiometricsEngine,
    security_enabled: bool,
    wakeup_word: &str,
    active_session: bool,
    deepfilter_model_dir: &Path,
    input_wav: &Path,
    voice: &str,
    lang: &str,
    browser: Option<Arc<BrowserBackend>>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    laya: Option<&Arc<LayaGate>>,
) -> Result<PipelineWavResult> {
    run_wav_turn(suppressor, stt, llm, tts, bio, security_enabled, wakeup_word, active_session, deepfilter_model_dir, input_wav, voice, lang, browser, windows, laya, None)
}

/// [`run_wav_file_to_speech_with_mcp`] for a client that plays the answer as it arrives (see [`run_text_to_speech_streaming`]). The
/// listener also hears what was recognised as soon as the speaker is verified and the words are known to be a request, which is
/// seconds before the answer.
pub fn run_wav_file_to_speech_streaming(
    suppressor: &NoiseSuppressor,
    stt: &WhisperSttEngine,
    llm: &LlamaRouterEngine,
    tts: &KokoroTtsEngine,
    bio: &mut VoiceBiometricsEngine,
    security_enabled: bool,
    wakeup_word: &str,
    active_session: bool,
    deepfilter_model_dir: &Path,
    input_wav: &Path,
    voice: &str,
    lang: &str,
    browser: Option<Arc<BrowserBackend>>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    laya: Option<&Arc<LayaGate>>,
    listener: Listener<'_>,
) -> Result<PipelineWavResult> {
    run_wav_turn(suppressor, stt, llm, tts, bio, security_enabled, wakeup_word, active_session, deepfilter_model_dir, input_wav, voice, lang, browser, windows, laya, Some(listener))
}

fn run_wav_turn(
    suppressor: &NoiseSuppressor,
    stt: &WhisperSttEngine,
    llm: &LlamaRouterEngine,
    tts: &KokoroTtsEngine,
    bio: &mut VoiceBiometricsEngine,
    security_enabled: bool,
    wakeup_word: &str,
    active_session: bool,
    deepfilter_model_dir: &Path,
    input_wav: &Path,
    voice: &str,
    lang: &str,
    browser: Option<Arc<BrowserBackend>>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    laya: Option<&Arc<LayaGate>>,
    listener: Option<Listener<'_>>,
) -> Result<PipelineWavResult> {
    // No speech-to-text means no transcript: say so, rather than inventing one and answering it.
    if !stt.is_engine_ready() {
        anyhow::bail!("speech-to-text is not available: {}", stt.unavailable_reason());
    }
    // Digital silence has nothing to denoise or transcribe: skip both rather than spend seconds of CPU on it.
    let mut input_level = f32::MAX;
    if let Some(pcm) = std::fs::read(input_wav).ok().and_then(|b| aqua_audio_adapter::extract_pcm16_samples(&b).ok()) {
        let level = rms_of(&pcm);
        input_level = loudest_second_rms(&pcm);
        if level < SILENCE_RMS {
            return Ok(PipelineWavResult {
                denoise_applied: false,
                transcribed_text: String::new(),
                assistant_text: NO_SPEECH.to_string(),
                session_action: None,
                local_command: false,
                tts_sample_count: 0,
                tts_wav_base64: String::new(),
                tts_sample_rate: tts.sample_rate(),
                whisper_engine_ready: stt.is_engine_ready(),
                llm_engine_ready: llm.is_engine_ready(),
                tts_engine_ready: tts.is_engine_ready(),
            });
        }
    }
    // Loud but not speech (wind, typing, a fan): neither denoised nor transcribed, and nothing is said back.
    let recording16k = std::fs::read(input_wav).ok().and_then(|b| aqua_audio_adapter::resample::wav_to_16k_mono(&b).ok());
    if let Some(pcm16k) = &recording16k {
        if is_not_speech(stt, pcm16k) {
            return Ok(ignored_result(stt, llm, tts, NOT_SPEECH));
        }
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let scratch = std::env::temp_dir().join(format!("aqua_pipeline_{stamp}"));
    std::fs::create_dir_all(&scratch)?;
    // The folder holds the person's denoised voice, so it goes away on every exit, errors included.
    let _scratch_guard = ScratchDir(scratch.clone());

    // The recording is mostly silence: the orb starts it before the person speaks and ends it 1.5 s after the last voiced sound. The
    // noise remover and Whisper take time in proportion to the length of what they are given (and Whisper's shortened window only
    // applies up to 12 s), so they are given the speech with a little room around it. The voiceprint check keeps the whole recording.
    let heard_wav: PathBuf = crate::turn_timing::stage("trim", || recording16k.as_deref().and_then(|pcm| trimmed_recording(pcm, &scratch)))
        .unwrap_or_else(|| input_wav.to_path_buf());

    let mut denoise_applied = false;
    let stt_wav: PathBuf = if suppressor.is_engine_ready() {
        if let Some(cli) = resolve_cli_binary() {
            let tar = model_tarball(deepfilter_model_dir);
            if tar.is_file() {
                let df_out = scratch.join("df");
                // A denoiser that fails must not cost the whole turn: carry on with the original audio.
                match crate::turn_timing::stage("denoise", || enhance_wav(&cli, &tar, &heard_wav, &df_out)) {
                    Ok(_) => {
                        let enhanced = enhanced_output_wav(&df_out, &heard_wav);
                        if enhanced.is_file() {
                            denoise_applied = true;
                            enhanced
                        } else {
                            heard_wav.clone()
                        }
                    }
                    Err(e) => {
                        warn!("Noise suppression failed, using the original audio: {e:#}");
                        heard_wav.clone()
                    }
                }
            } else {
                heard_wav.clone()
            }
        } else {
            heard_wav.clone()
        }
    } else {
        heard_wav.clone()
    };

    let transcribed_text = crate::turn_timing::stage("stt", || stt.transcribe_wav_file(&stt_wav))
        .with_context(|| format!("whisper transcribe {:?}", stt_wav))?;

    let empty_result = |assistant_text: String,
                        session_action: Option<String>,
                        local_command: bool|
     -> PipelineWavResult {
        PipelineWavResult {
            denoise_applied,
            transcribed_text: transcribed_text.clone(),
            assistant_text,
            session_action,
            local_command,
            tts_sample_count: 0,
            tts_wav_base64: String::new(),
            tts_sample_rate: tts.sample_rate(),
            whisper_engine_ready: stt.is_engine_ready(),
            llm_engine_ready: llm.is_engine_ready(),
            tts_engine_ready: tts.is_engine_ready(),
        }
    };

    if transcribed_text.trim().is_empty() || is_invented_by_whisper(&transcribed_text, active_session, input_level, wakeup_word) {
        // With the voice-activity model an empty answer means it heard no speech: that is noise, not a person who was not understood.
        let why = if transcribed_text.trim().is_empty() && stt.vad_active() { NOT_SPEECH } else { NO_SPEECH };
        return Ok(empty_result(why.to_string(), None, false));
    }

    if !passes_wake_gate(&transcribed_text, wakeup_word, active_session) {
        let _ = std::fs::remove_dir_all(&scratch);
        return Ok(empty_result(
            "IGNORED: Wake-word not detected.".to_string(),
            None,
            false,
        ));
    }

    if security_enabled {
        // Fail closed: an unreadable file or a failed voiceprint means the speaker is not verified.
        let verified = crate::turn_timing::stage("voiceprint", || {
            std::fs::read(input_wav)
                .ok()
                .and_then(|bytes| aqua_audio_adapter::extract_pcm16_samples(&bytes).ok())
                .and_then(|pcm| bio.extract_embedding(&pcm).ok())
                .map(|emb| bio.verify_speaker(&emb))
        });
        match verified {
            Some((true, _)) => {}
            other => {
                let _ = std::fs::remove_dir_all(&scratch);
                let message = match other {
                    Some((_, sim)) => format!("ACCESS DENIED: Unauthorized speaker ({:.1}% match).", sim * 100.0),
                    None => "ACCESS DENIED: could not verify the speaker.".to_string(),
                };
                // Someone spoke to Aqua and was refused: say why, in a few words, instead of leaving them in silence. (A turn that was
                // never addressed to Aqua returned above, and stays silent.)
                let note = refusal_note(&message);
                let mut result = empty_result(message, None, false);
                if let Some(note) = note {
                    let mut speaker = listener.map(|listener| Speaker::new(tts, voice, lang, listener));
                    if let Ok((wav_base64, samples, rate)) = speech_for_result(tts, note, voice, lang, speaker.as_mut()) {
                        result.tts_wav_base64 = wav_base64;
                        result.tts_sample_count = samples;
                        result.tts_sample_rate = rate;
                    }
                }
                return Ok(result);
            }
        }
    }

    // A streamed turn: the listener is told what was heard now, while the answer is still to come.
    let mut speaker = listener.map(|listener| Speaker::new(tts, voice, lang, listener));
    if let Some(speaker) = speaker.as_mut() {
        speaker.heard(&transcribed_text);
    }

    // Local wake / sleep commands take priority — never call llama.cpp.
    match resolve_session_command(&transcribed_text, wakeup_word, active_session) {
        CommandHandling::Speak { command, text } => {
            info!("Local session command → TTS (skipping llama.cpp): {text}");
            let (tts_wav_base64, tts_sample_count, tts_sample_rate) = speech_for_result(tts, &text, voice, lang, speaker.as_mut())?;
            let _ = std::fs::remove_dir_all(&scratch);
            return Ok(PipelineWavResult {
                denoise_applied,
                transcribed_text,
                assistant_text: text,
                session_action: Some(session_action_label(command).to_string()),
                local_command: true,
                tts_sample_count,
                tts_wav_base64,
                tts_sample_rate,
                whisper_engine_ready: stt.is_engine_ready(),
                llm_engine_ready: llm.is_engine_ready(),
                tts_engine_ready: tts.is_engine_ready(),
            });
        }
        CommandHandling::AlreadyInState { command } => {
            info!(
                "Local session command {:?} ignored — already in that state",
                command
            );
            let _ = std::fs::remove_dir_all(&scratch);
            return Ok(empty_result(
                String::new(),
                Some(session_action_label(command).to_string()),
                true,
            ));
        }
        CommandHandling::NotACommand => {}
    }

    let prompt = transcribed_text.clone();

    // After wake/sleep, Laya gate routes chat vs MCP (same gate as text path).
    let assistant_text = generate_assistant_reply_speaking(llm, browser, windows, &prompt, Heard::Spoken { wake_phrase: wakeup_word }, laya, speaker.as_mut())?;
    let (tts_wav_base64, tts_sample_count, tts_sample_rate) = speech_for_result(tts, &assistant_text, voice, lang, speaker.as_mut())?;

    let _ = std::fs::remove_dir_all(&scratch);

    Ok(PipelineWavResult {
        denoise_applied,
        transcribed_text,
        assistant_text,
        session_action: None,
        local_command: false,
        tts_sample_count,
        tts_wav_base64,
        tts_sample_rate,
        whisper_engine_ready: stt.is_engine_ready(),
        llm_engine_ready: llm.is_engine_ready(),
        tts_engine_ready: tts.is_engine_ready(),
    })
}
