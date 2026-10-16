//! Tests of `speaker.rs` and `text_turn.rs`: an answer spoken piece by piece, the order of the pieces, a listener that leaves, and the streamed and the text-only typed turn (move only: they were part of the inline module `streaming_tests` of `pipeline.rs`; the path of every test is now `pipeline::streaming_tests::<name>`).

use super::*;

/// Engines with no models: no speech can be made (every piece arrives with empty sound) and the model is never asked, so what these
/// check is how the pieces are cut, ordered and sent, and that a listener who leaves stops the turn.
fn engines() -> (LlamaRouterEngine, KokoroTtsEngine) {
    let nothing = std::env::temp_dir().join("aqua_no_models_for_this_test");
    (LlamaRouterEngine::new(&nothing).unwrap(), KokoroTtsEngine::new(&nothing).unwrap())
}

fn one_speech_test_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    TTS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn words_of(events: &[TurnEvent]) -> Vec<String> {
    events.iter().filter_map(|e| if let TurnEvent::Speech { text, .. } = e { Some(text.clone()) } else { None }).collect()
}

#[test]
fn an_answer_is_spoken_piece_by_piece_as_it_is_written() {
    // Every test that makes a Speaker shares the one gate and the one count of held jobs with the tests that hold the speech worker: they run one at a time.
    let _one = one_speech_test_at_a_time();
    let (_, tts) = engines();
    let mut events = Vec::new();
    {
        let mut listener = |e: TurnEvent| {
            events.push(e);
            true
        };
        let mut speaker = Speaker::new(&tts, "af_sarah", "en-us", &mut listener);
        assert!(speaker.push("The weather in Pune is sunny today. It is 34"));
        assert_eq!(speaker.pieces, 1, "the first sentence is spoken while the rest is still being written");
        assert!(speaker.push(".5 degrees, with 40% humidity and a light breeze from the west. Have a"));
        assert!(speaker.push(" nice day!"));
        assert!(speaker.finish(false));
        assert_eq!(speaker.pieces, 3);
    }
    assert_eq!(words_of(&events), ["The weather in Pune is sunny today.", "It is 34.5 degrees, with 40% humidity and a light breeze from the west.", "Have a nice day!"]);
    assert!(events.iter().all(|e| matches!(e, TurnEvent::Speech { wav, .. } if wav.is_empty())), "with no speech engine the words still arrive, without sound");
}

#[test]
fn a_listener_that_leaves_stops_the_answer_and_is_sent_nothing_more() {
    // Every test that makes a Speaker shares the one gate and the one count of held jobs with the tests that hold the speech worker: they run one at a time.
    let _one = one_speech_test_at_a_time();
    let (_, tts) = engines();
    let mut sent = 0;
    {
        let mut listener = |_: TurnEvent| {
            sent += 1;
            false
        };
        let mut speaker = Speaker::new(&tts, "af_sarah", "en-us", &mut listener);
        assert!(speaker.push("The first piece is long enough to be spoken. And a second one that follows it closely, for sure."), "queueing does not wait to learn that the listener has left");
        assert!(!speaker.finish(false), "the first sound tells the listener, who leaves, so the rest is dropped");
        assert!(speaker.gone);
        assert!(!speaker.push("More text that nobody will hear."));
        let stopped = speaker.finish_turn("The whole answer.").expect_err("nobody heard the end").to_string();
        assert!(stopped.starts_with("stopped"), "{stopped}");
    }
    assert_eq!(sent, 1, "nothing is sent to someone who has left");
}

#[test]
fn a_whole_answer_that_was_not_streamed_is_spoken_in_pieces_and_never_twice() {
    // Every test that makes a Speaker shares the one gate and the one count of held jobs with the tests that hold the speech worker: they run one at a time.
    let _one = one_speech_test_at_a_time();
    let (_, tts) = engines();
    let answer = "I found three files with that name. The newest is in your Documents folder, and the other two are old copies in the Downloads folder.";
    let mut events = Vec::new();
    {
        let mut listener = |e: TurnEvent| {
            events.push(e);
            true
        };
        let mut speaker = Speaker::new(&tts, "af_sarah", "en-us", &mut listener);
        speaker.finish_turn(answer).unwrap();
        speaker.finish_turn(answer).unwrap();
    }
    assert_eq!(words_of(&events), ["I found three files with that name.", "The newest is in your Documents folder, and the other two are old copies in the Downloads folder."]);
}

#[test]
fn what_was_heard_comes_before_the_answer() {
    // Every test that makes a Speaker shares the one gate and the one count of held jobs with the tests that hold the speech worker: they run one at a time.
    let _one = one_speech_test_at_a_time();
    let (_, tts) = engines();
    let mut events = Vec::new();
    {
        let mut listener = |e: TurnEvent| {
            events.push(e);
            true
        };
        let mut speaker = Speaker::new(&tts, "af_sarah", "en-us", &mut listener);
        speaker.heard("what is the date today");
        speaker.finish_turn("Today is Saturday.").unwrap();
    }
    assert_eq!(events.len(), 2);
    assert_eq!(events[0], TurnEvent::Heard("what is the date today".into()));
    assert!(matches!(&events[1], TurnEvent::Speech { text, .. } if text == "Today is Saturday."));
}

fn hold_tts() -> Arc<(Mutex<bool>, std::sync::Condvar)> {
    TTS_TEST_HELD.store(0, Ordering::SeqCst);
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    *TTS_TEST_GATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(gate.clone());
    gate
}

fn release_tts(gate: &Arc<(Mutex<bool>, std::sync::Condvar)>) {
    let (lock, cv) = &**gate;
    *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
    cv.notify_all();
    *TTS_TEST_GATE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[test]
fn speech_stays_in_order_when_the_queue_is_full_and_a_later_turn_does_not_hear_the_earlier_one() {
    let _one = TTS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_, tts) = engines();
    let gate = hold_tts();
    let pieces = [
        "Alpha has more than enough words to be the first spoken piece.",
        "Bravo has more than enough words to be its own spoken piece.",
        "Charlie has more than enough words to be its own spoken piece.",
        "Delta has more than enough words to be its own spoken piece.",
    ];
    let mut first = Vec::new();
    {
        let mut listener = |e: TurnEvent| {
            first.push(e);
            true
        };
        let mut speaker = Speaker::new(&tts, "af_sarah", "en-us", &mut listener);
        speaker.enqueue(pieces[0].to_string());
        let ready = Instant::now() + Duration::from_secs(2);
        while TTS_TEST_HELD.load(Ordering::SeqCst) == 0 && Instant::now() < ready {
            std::thread::sleep(Duration::from_millis(10));
        }
        for piece in &pieces[1..] {
            speaker.enqueue((*piece).to_string());
        }
        assert_eq!(speaker.dropped, 1, "one piece is in the worker, two sit in the queue, the newest is not queued");
        assert_eq!(speaker.submitted, 3);
        release_tts(&gate);
        assert!(speaker.finish(false));
    }
    assert_eq!(words_of(&first), pieces);
    let sounds: Vec<bool> = first.iter().map(|e| matches!(e, TurnEvent::Speech { wav, .. } if wav.is_empty())).collect();
    assert!(sounds.iter().filter(|empty| **empty).count() >= 1, "the piece that did not fit has no sound");

    let mut second = Vec::new();
    {
        let mut listener = |e: TurnEvent| {
            second.push(e);
            true
        };
        let mut speaker = Speaker::new(&tts, "af_sarah", "en-us", &mut listener);
        speaker.finish_turn("Only the second turn is spoken here today.").unwrap();
    }
    assert_eq!(words_of(&second), ["Only the second turn is spoken here today."]);
}

#[test]
fn a_cancelled_turn_drops_queued_speech_and_the_next_turn_is_clean() {
    let _one = TTS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_, tts) = engines();
    let flag = Arc::new(AtomicBool::new(false));
    let _bound = crate::modules::browser_agent::bind_turn_cancel(flag.clone());
    let gate = hold_tts();
    let mut heard = Vec::new();
    {
        let mut listener = |e: TurnEvent| {
            heard.push(e);
            true
        };
        let mut speaker = Speaker::new(&tts, "af_sarah", "en-us", &mut listener);
        assert!(speaker.push("Alpha has more than enough words to be the first spoken piece. Bravo has more than enough words to be its own spoken piece. "));
        flag.store(true, Ordering::SeqCst);
        release_tts(&gate);
        assert!(!speaker.finish(false));
    }
    assert!(heard.is_empty(), "a turn cancelled before its sound is delivered plays nothing: {heard:?}");
    drop(_bound);
    let mut next = Vec::new();
    {
        let mut listener = |e: TurnEvent| {
            next.push(e);
            true
        };
        let mut speaker = Speaker::new(&tts, "af_sarah", "en-us", &mut listener);
        speaker.finish_turn("The following turn speaks on its own.").unwrap();
    }
    assert_eq!(words_of(&next), ["The following turn speaks on its own."]);
}

#[test]
fn a_streamed_text_turn_sends_the_words_to_the_listener_and_has_no_sound_of_its_own() {
    // Every test that makes a Speaker shares the one gate and the one count of held jobs with the tests that hold the speech worker: they run one at a time.
    let _one = one_speech_test_at_a_time();
    let (llm, tts) = engines();
    let mut events = Vec::new();
    let streamed = run_text_to_speech_streaming(&llm, &tts, "What's the date today?", "af_sarah", "en-us", None, None, None, &mut |e| {
        events.push(e);
        true
    })
    .unwrap();
    assert_eq!(streamed.assistant_text, format!("Today is {}.", chrono::Local::now().format("%A, %B %-d, %Y")));
    assert_eq!(words_of(&events), [streamed.assistant_text.clone()]);
    assert!(streamed.tts_wav_base64.is_empty(), "the sound went to the listener");
    // The same turn without a listener gives the same words in one result.
    let whole = run_text_to_speech_with_mcp(&llm, &tts, "What's the date today?", "af_sarah", "en-us", None, None, None).unwrap();
    assert_eq!(whole.assistant_text, streamed.assistant_text);
    assert!(whole.tts_sample_count > 0 && streamed.tts_sample_count == 0, "no speech engine here: only the silent stand-in of the whole-answer path has samples");
}

/// A measurement, not a check (run it with `--ignored --nocapture`): the real 8B model on its server and the real speech engine (the
/// Kokoro daemon must be running), each question asked both ways, with the time of every piece from the start of the request, how
/// long each piece plays, and whether the next one was ready in time (a "stall" is a silence the listener would hear).
#[test]
#[ignore = "a measurement: needs the 8B model, the model server and the Kokoro daemon"]
fn measure_when_the_first_sound_comes_streamed_and_not() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../data/models");
    let llm = LlamaRouterEngine::new(root.join("llm")).unwrap();
    let tts = KokoroTtsEngine::new(root.join("kokoro")).unwrap();
    let loading = std::time::Instant::now();
    llm.warm_up_prompts(&llm.agent_model_filename(), &chat_warmup_prompt(), "").unwrap();
    eprintln!("MEASURE the model loaded and the chat prompt read: {} ms", loading.elapsed().as_millis());
    let _ = tts.synthesize_wav("Ready.", "af_sarah", "en-us");
    for question in ["Tell me about the planet Mars in three short sentences.", "What is the difference between weather and climate? Answer in a short paragraph.", "Why is the sky blue?"] {
        let started = std::time::Instant::now();
        let mut pieces: Vec<(u64, String, f32)> = Vec::new();
        let streamed = run_text_to_speech_streaming(&llm, &tts, question, "af_sarah", "en-us", None, None, None, &mut |event| {
            if let TurnEvent::Speech { text, wav } = event {
                let seconds = aqua_audio_adapter::kokoro_cli::wav_bytes_to_f32_mono(&wav).map(|(s, rate)| s.len() as f32 / rate as f32).unwrap_or(0.0);
                pieces.push((started.elapsed().as_millis() as u64, text, seconds));
            }
            true
        })
        .unwrap();
        let streamed_ms = started.elapsed().as_millis();
        eprintln!("MEASURE {question:?}");
        let (mut playing_until, mut stalls) = (0.0f32, 0.0f32);
        for (k, (at_ms, text, seconds)) in pieces.iter().enumerate() {
            let ready = *at_ms as f32 / 1000.0;
            if k > 0 && ready > playing_until {
                stalls += ready - playing_until;
            }
            let begins = if k == 0 { ready } else { ready.max(playing_until) };
            playing_until = begins + seconds;
            eprintln!("MEASURE   piece {k}: ready at {at_ms} ms, plays {seconds:.1} s, {} chars: {text:?}", text.chars().count());
        }
        eprintln!("MEASURE   streamed: first sound at {} ms, the turn ended at {streamed_ms} ms, the audio ends at {:.1} s, silence between pieces {stalls:.1} s", pieces.first().map_or(0, |p| p.0), playing_until);
        let plain_started = std::time::Instant::now();
        let whole = run_text_to_speech_with_mcp(&llm, &tts, question, "af_sarah", "en-us", None, None, None).unwrap();
        let whole_seconds = whole.tts_sample_count as f32 / whole.tts_sample_rate.max(1) as f32;
        eprintln!("MEASURE   not streamed: sound at {} ms, plays {whole_seconds:.1} s, so the audio ends at {:.1} s; streamed text {} chars, plain text {} chars", plain_started.elapsed().as_millis(), plain_started.elapsed().as_millis() as f32 / 1000.0 + whole_seconds, streamed.assistant_text.chars().count(), whole.assistant_text.chars().count());
    }
    crate::modules::llm_router::llama_server::shutdown();
}

/// A measurement, not a check (run it with `--ignored --nocapture`): how long the first spoken piece of a chat answer takes to be written
/// under different wording of the rules, on the real 8B model. `AQUA_RULE_VARIANTS` is a list of sentences separated by `|`, each added to
/// the rules as a variant (an empty one is the rules as they are). The speech engine is not involved (it adds about 0.8 s to each).
#[test]
#[ignore = "a measurement: needs the 8B model"]
fn measure_how_the_wording_of_the_rules_changes_the_first_piece() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../data/models");
    let llm = LlamaRouterEngine::new(root.join("llm")).unwrap();
    llm.warm_up_prompts(&llm.agent_model_filename(), &chat_warmup_prompt(), "").unwrap();
    let names = ["current", "variant 1", "variant 2", "variant 3"];
    let variants: Vec<(&str, String)> = match std::env::var("AQUA_RULE_VARIANTS") {
        Ok(list) => list.split('|').enumerate().map(|(i, extra)| (names[i.min(3)], if extra.trim().is_empty() { CHAT_RULES.to_string() } else { format!("{CHAT_RULES} {}", extra.trim()) })).collect(),
        Err(_) => vec![("current", CHAT_RULES.to_string())],
    };
    let questions = [
        "Tell me about the planet Mars in three short sentences.",
        "What is the difference between weather and climate?",
        "Why is the sky blue?",
        "How do I boil an egg?",
        "Who wrote Pride and Prejudice?",
        "What is a good name for a cat?",
        "Explain what a mortgage is.",
        "Is it safe to eat rice that was left out overnight?",
    ];
    let mut firsts: std::collections::BTreeMap<&str, Vec<u64>> = Default::default();
    for question in questions {
        for (name, rules) in &variants {
            let prompt = chat_prompt_with_rules(rules, question);
            let started = std::time::Instant::now();
            let mut splitter = crate::speech_stream::SentenceSplitter::new();
            let (mut first_ms, mut first_text) = (None, String::new());
            let chat = llm
                .generate_chat_streaming(&prompt, |delta| {
                    let pieces = splitter.push(delta);
                    if first_ms.is_none() && !pieces.is_empty() {
                        first_ms = Some(started.elapsed().as_millis() as u64);
                        first_text = pieces[0].clone();
                    }
                    true
                })
                .unwrap();
            let first_ms = first_ms.unwrap_or(started.elapsed().as_millis() as u64);
            firsts.entry(name).or_default().push(first_ms);
            eprintln!("MEASURE {name:10} first piece at {first_ms:5} ms ({:3} chars), whole answer {:3} chars in {:5} ms | {question} -> {first_text:?}", first_text.chars().count(), chat.text.chars().count(), started.elapsed().as_millis());
        }
    }
    for (name, times) in firsts {
        let mut sorted = times.clone();
        sorted.sort_unstable();
        eprintln!("MEASURE {name:10} first piece: median {} ms, mean {} ms, worst {} ms over {} questions", sorted[sorted.len() / 2], times.iter().sum::<u64>() / times.len() as u64, sorted[sorted.len() - 1], times.len());
    }
    crate::modules::llm_router::llama_server::shutdown();
}

/// Card OA10-06 (UX-08): `tts: false` is a turn that asks no speech engine: the same words, in the same pieces, with no sound and no worker, and nothing in the result that says there was sound.
#[test]
fn a_text_only_turn_has_the_same_words_and_no_sound_and_asks_no_speech_engine() {
    let _one = one_speech_test_at_a_time();
    let (llm, tts) = engines();
    let answer = "I found three files with that name. The newest is in your Documents folder, and the other two are old copies in the Downloads folder.";
    let mut spoken = Vec::new();
    let mut silent = Vec::new();
    {
        let mut listener = |e: TurnEvent| {
            spoken.push(e);
            true
        };
        let mut speaker = Speaker::new(&tts, "af_sarah", "en-us", &mut listener);
        speaker.finish_turn(answer).unwrap();
        assert!(speaker.worker.is_some() && speaker.submitted > 0, "a turn that speaks hands its pieces to the speech worker");
    }
    {
        let mut listener = |e: TurnEvent| {
            silent.push(e);
            true
        };
        let mut speaker = Speaker::text_only(&tts, "af_sarah", "en-us", &mut listener);
        speaker.finish_turn(answer).unwrap();
        assert!(speaker.worker.is_none(), "no speech worker is started for a text-only turn");
        assert_eq!((speaker.submitted, speaker.completed, speaker.sample_count), (0, 0, 0), "and no piece is handed to one");
    }
    assert_eq!(words_of(&silent), words_of(&spoken), "the same pieces, in the same order");
    assert!(words_of(&silent).len() >= 2);
    assert!(silent.iter().all(|e| matches!(e, TurnEvent::Speech { wav, .. } if wav.is_empty())));
    // The whole turn, both ways: with `speak` the whole-answer path makes its (here silent stand-in) sound, without it there is none.
    let ask = "What's the date today?";
    let mut pieces = Vec::new();
    let streamed = run_text_turn_with_sound(
        &llm,
        &tts,
        ask,
        "af_sarah",
        "en-us",
        None,
        None,
        None,
        Some(&mut |e: TurnEvent| {
            pieces.push(e);
            true
        }),
        false,
    )
    .unwrap();
    assert_eq!(streamed.assistant_text, format!("Today is {}.", chrono::Local::now().format("%A, %B %-d, %Y")));
    assert_eq!(words_of(&pieces), [streamed.assistant_text.clone()], "the words still arrive");
    assert!(streamed.tts_wav_base64.is_empty() && streamed.tts_sample_count == 0);
    let quiet = run_text_turn_with_sound(&llm, &tts, ask, "af_sarah", "en-us", None, None, None, None, false).unwrap();
    let loud = run_text_turn_with_sound(&llm, &tts, ask, "af_sarah", "en-us", None, None, None, None, true).unwrap();
    assert_eq!(quiet.assistant_text, loud.assistant_text, "the same text");
    assert!(quiet.tts_wav_base64.is_empty() && quiet.tts_sample_count == 0, "no sound for a text-only turn");
    assert!(loud.tts_sample_count > 0, "and there was sound for the other, so the check above can fail");
}

/// Card OA10-06 (UX-08): with `AQUA_CODE_SPEECH` on a streamed answer does not speak its code and says the cue once; off, it speaks everything as it always did.
#[test]
fn a_speaker_leaves_code_out_only_when_the_switch_is_on() {
    let _one = one_speech_test_at_a_time();
    let (_, tts) = engines();
    let answer = "Here is how to parse it, it is short and clear.\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\nThat prints a greeting, which is all it does.";
    let say = |switch: Option<&str>| {
        let mut events = Vec::new();
        crate::flags::with_overrides(&[("AQUA_CODE_SPEECH", switch)], || {
            let mut listener = |e: TurnEvent| {
                events.push(e);
                true
            };
            let mut speaker = Speaker::new(&tts, "af_sarah", "en-us", &mut listener);
            // Written in pieces, as a model writes it.
            for piece in ["Here is how to parse it, it is short and clear.\n``", "`rust\nfn main() {\n    println!(\"hel", "lo\");\n}\n```\nThat prints a greeting, ", "which is all it does."] {
                assert!(speaker.push(piece));
            }
            assert!(speaker.finish(false));
        });
        words_of(&events).join(" ")
    };
    let off = say(None);
    assert!(off.contains("println") && !off.contains("on the screen"), "off: spoken as it always was: {off}");
    let on = say(Some("on"));
    assert!(!on.contains("println") && !on.contains("fn main") && on.matches(crate::speech_stream::CODE_CUE).count() == 1, "on: {on}");
    assert!(on.contains("Here is how to parse it") && on.contains("That prints a greeting"), "{on}");
    // The same for an answer that was not written piece by piece (the agent's): the whole text is spoken through the same splitter.
    let mut events = Vec::new();
    crate::flags::with_overrides(&[("AQUA_CODE_SPEECH", Some("on"))], || {
        let mut listener = |e: TurnEvent| {
            events.push(e);
            true
        };
        let mut speaker = Speaker::new(&tts, "af_sarah", "en-us", &mut listener);
        speaker.finish_turn(answer).unwrap();
    });
    assert!(!words_of(&events).join(" ").contains("println") && words_of(&events).join(" ").contains(crate::speech_stream::CODE_CUE));
}

/// Card OA10-06 (UX-16): "undo the last run" said to Aqua is answered by the program when `AQUA_CODING_VOICE` is on, with no model and no change: it is a card on the screen.
#[test]
fn a_coding_phrase_is_answered_by_the_program_only_when_the_switch_is_on() {
    let _one = one_speech_test_at_a_time();
    let (llm, tts) = engines();
    let on = crate::flags::with_overrides(&[("AQUA_CODING_VOICE", Some("on"))], || {
        run_text_to_speech_with_mcp(&llm, &tts, "undo the last run", "af_sarah", "en-us", None, None, None).map(|r| r.assistant_text)
    })
    .unwrap();
    assert!(on.starts_with("I have nothing to undo") || on.starts_with("I put the undo on your screen"), "{on}");
    assert!(!on.contains("I undid") && !on.contains("I have put back"), "{on}");
    // (Off, the phrase is a request like any other: `coding_voice::handle_utterance` says nothing to it, which its own test checks; the model is not run here.)
}

#[test]
fn a_streamed_text_turn_whose_listener_left_ends_as_a_stopped_task() {
    // Every test that makes a Speaker shares the one gate and the one count of held jobs with the tests that hold the speech worker: they run one at a time.
    let _one = one_speech_test_at_a_time();
    let (llm, tts) = engines();
    let stopped = run_text_to_speech_streaming(&llm, &tts, "What's the date today?", "af_sarah", "en-us", None, None, None, &mut |_| false);
    let message = format!("{:#}", stopped.expect_err("nobody was listening"));
    assert!(message.starts_with("stopped"), "{message}");
    // Empty text is nothing: no events, no work.
    let mut events = 0;
    let empty = run_text_to_speech_streaming(&llm, &tts, "   ", "af_sarah", "en-us", None, None, None, &mut |_| {
        events += 1;
        true
    })
    .unwrap();
    assert!(empty.assistant_text.is_empty() && events == 0);
}
