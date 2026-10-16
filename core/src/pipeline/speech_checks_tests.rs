//! Tests of `speech_checks.rs`: what is Whisper's invention, how loud a recording is, and the trimmed copy of a recording (move only: they were part of the inline modules `honest_answer_tests`, `filler_tests`, `spoken_request_routing_tests` and `trim_tests` of `pipeline.rs`; the path of every test is now `pipeline::speech_checks_tests::<name>`).

use super::*;

#[test]
fn a_short_phrase_in_a_long_silent_recording_is_not_mistaken_for_a_quiet_recording() {
    let mut long = vec![0.0f32; 16_000 * 60];
    for (i, s) in long.iter_mut().skip(16_000 * 10).take(12_800).enumerate() {
        *s = (i as f32 * 0.05).sin() * 0.05; // 0.8 s of ordinary quiet-room speech, in a minute of silence
    }
    assert!(rms_of(&long) < QUIET_RMS, "averaged over the whole file it looks quiet: {}", rms_of(&long));
    assert!(loudest_second_rms(&long) > QUIET_RMS, "but its loudest second is clearly speech: {}", loudest_second_rms(&long));
    // so a "Thank you." heard in a session at that level is a real answer, not an invention
    assert!(!is_invented_by_whisper("Thank you.", true, loudest_second_rms(&long), "Hey Aqua"));
    assert!(is_invented_by_whisper("Thank you.", true, loudest_second_rms(&vec![0.001; 16_000 * 30]), "Hey Aqua"));
}

#[test]
fn a_filler_is_dropped_only_when_nobody_is_talking_to_aqua_or_it_was_too_quiet_to_be_speech() {
    // asleep: Whisper's "Thank you." for room noise is dropped, loud or not
    assert!(is_invented_by_whisper("Thank you.", false, 0.2, "Hey Aqua"));
    // in a session at a spoken volume: a real answer
    assert!(!is_invented_by_whisper("Okay.", true, 0.05, "Hey Aqua"));
    assert!(!is_invented_by_whisper("Thanks.", true, 0.05, "Hey Aqua"));
    // in a session but nearly silent: the invention
    assert!(is_invented_by_whisper("Thank you.", true, 0.002, "Hey Aqua"));
    // ordinary words are never dropped
    assert!(!is_invented_by_whisper("Stop.", false, 0.002, "Hey Aqua"));
    assert!(!is_invented_by_whisper("Yes.", true, 0.001, "Hey Aqua"));
}

#[test]
fn the_priming_text_on_a_quiet_recording_is_dropped_but_said_aloud_it_is_the_wake_call() {
    assert!(is_invented_by_whisper("Hey Aqua.", false, 0.003, "Hey Aqua"), "echo of the prompt on near-silence");
    assert!(!is_invented_by_whisper("Hey Aqua.", false, 0.08, "Hey Aqua"), "a person really said it");
}

#[test]
fn the_television_and_whispers_loops_are_dropped_but_not_a_person_repeating_a_command_to_aqua() {
    assert!(is_invented_by_whisper("Hello everyone. Hello everyone.", true, 0.05, "Hey Aqua"), "even in a conversation, at a normal volume");
    assert!(is_invented_by_whisper("Hi. It's the reason why I'm here today. It's the reason why I'm here today. The", true, 0.05, "Hey Aqua"));
    assert!(!is_invented_by_whisper("Hey Aqua, open the file. Hey Aqua, open the file.", false, 0.05, "Hey Aqua"), "said to Aqua by name");
    assert!(!is_invented_by_whisper("Open Notepad.", true, 0.05, "Hey Aqua"));
    assert!(!is_invented_by_whisper("Stop. Stop.", true, 0.05, "Hey Aqua"));
    // The rules that were there before are unchanged.
    assert!(is_invented_by_whisper("Thank you.", false, 0.05, "Hey Aqua"));
    assert!(!is_invented_by_whisper("Thank you.", true, 0.05, "Hey Aqua"), "a real answer inside a conversation");
}

/// The recording is read from the environment each time, so the tests that set it take turns with the ones that rely on it.
static ENV: Mutex<()> = Mutex::new(());

const SR: f32 = 16_000.0;

/// A crude voice: harmonics of a gliding pitch in syllable-long bursts.
fn voice(seconds: f32) -> Vec<f32> {
    (0..(seconds * SR) as usize)
        .map(|i| {
            let t = i as f32 / SR;
            let f = 130.0 * (1.0 + 0.08 * (2.0 * std::f32::consts::PI * 2.0 * t).sin());
            let phase = 2.0 * std::f32::consts::PI * f * t;
            let syllables = 0.5 + 0.5 * (2.0 * std::f32::consts::PI * 3.5 * t).sin();
            0.12 * syllables * (1..=8).map(|h| (phase * h as f32).sin() / h as f32).sum::<f32>()
        })
        .collect()
}

/// A quiet room: faint, deterministic noise.
fn quiet(seconds: f32) -> Vec<f32> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..(seconds * SR) as usize)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (((state >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0) * 0.0035
        })
        .collect()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aqua_trim_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_recording_that_is_mostly_silence_is_cut_down_to_the_speech_in_a_copy() {
    let _env = ENV.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut clip = quiet(4.0);
    clip.extend(voice(1.5));
    clip.extend(quiet(3.0));
    let dir = scratch("cut");
    let path = trimmed_recording(&clip, &dir).expect("most of this clip is silence");
    let kept = aqua_audio_adapter::resample::wav_to_16k_mono(&std::fs::read(&path).unwrap()).unwrap();
    assert!(kept.len() as f32 / SR < 3.0, "1.5 s of voice plus its margins, not {:.1} s", kept.len() as f32 / SR);
    assert!(kept.len() as f32 / SR > 2.0, "the margins are kept: {:.1} s", kept.len() as f32 / SR);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_recording_with_little_to_cut_is_left_alone() {
    let _env = ENV.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut clip = quiet(0.4);
    clip.extend(voice(3.0));
    clip.extend(quiet(0.6));
    let dir = scratch("keep");
    assert!(trimmed_recording(&clip, &dir).is_none(), "under a second and a half to save is not worth a copy");
    assert!(trimmed_recording(&quiet(5.0), &dir).is_none(), "no voice to anchor on");
    assert!(!dir.join("speech.wav").exists());
    let _ = std::fs::remove_dir_all(dir);
}

/// A measurement, not a check (run it with `--ignored --nocapture` and `AQUA_TRIM_CHECK_DIR` set to an empty folder): real speech (the
/// first 3.5 seconds of the recording that comes with whisper.cpp) with the room noise a recording has before and after it, as the
/// orb makes them, written out with and without `trimmed_recording`, so that the speech tools can be timed on both.
#[test]
#[ignore = "a measurement: writes files for scripts/bench"]
fn write_clips_for_the_trimming_measurement() {
    let Some(dir) = std::env::var_os("AQUA_TRIM_CHECK_DIR").map(PathBuf::from) else {
        eprintln!("set AQUA_TRIM_CHECK_DIR to try this");
        return;
    };
    let sample = Path::new(env!("CARGO_MANIFEST_DIR")).join("../vendor/whisper.cpp/samples/jfk.wav");
    let jfk = aqua_audio_adapter::resample::wav_to_16k_mono(&std::fs::read(sample).unwrap()).unwrap();
    let speech = &jfk[..(3.5 * SR) as usize];
    std::fs::create_dir_all(&dir).unwrap();
    for (name, lead, trail) in [("typical", 0.8, 1.5), ("waited", 3.0, 3.0), ("long_wait", 6.0, 3.0)] {
        let mut clip = quiet(lead);
        clip.extend_from_slice(speech);
        clip.extend(quiet(trail));
        std::fs::write(dir.join(format!("{name}.wav")), aqua_audio_adapter::resample::mono_wav_bytes(&clip, 16_000).unwrap()).unwrap();
        let scratch = dir.join(format!("scratch_{name}"));
        std::fs::create_dir_all(&scratch).unwrap();
        match trimmed_recording(&clip, &scratch) {
            Some(path) => {
                std::fs::copy(&path, dir.join(format!("{name}_trimmed.wav"))).unwrap();
                eprintln!("MEASURE {name}: {:.1} s trimmed from the whole, see {name}_trimmed.wav", clip.len() as f32 / SR);
            }
            None => eprintln!("MEASURE {name}: {:.1} s, nothing worth cutting", clip.len() as f32 / SR),
        }
        let _ = std::fs::remove_dir_all(scratch);
    }
}

#[test]
fn it_can_be_switched_off() {
    let _env = ENV.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut clip = quiet(4.0);
    clip.extend(voice(1.5));
    clip.extend(quiet(3.0));
    let dir = scratch("off");
    std::env::set_var("AQUA_TRIM_SILENCE", "off");
    let off = trimmed_recording(&clip, &dir);
    std::env::remove_var("AQUA_TRIM_SILENCE");
    assert!(off.is_none());
    assert!(trimmed_recording(&clip, &dir).is_some(), "and back on");
    let _ = std::fs::remove_dir_all(dir);
}
