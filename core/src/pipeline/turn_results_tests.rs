//! Tests of `turn_results.rs`: the note for a refused turn (move only: they were part of the inline module `refusal_note_tests` of `pipeline.rs`; the path of every test is now `pipeline::turn_results_tests::<name>`).

use super::*;

#[test]
fn a_refused_turn_gets_a_short_note_and_a_turn_nobody_addressed_stays_silent() {
    // Someone spoke to Aqua and was refused: they are told why, in words they can act on.
    assert_eq!(refusal_note("ACCESS DENIED: Unauthorized speaker (41.2% match)."), Some("I did not recognise your voice."));
    assert_eq!(refusal_note("ACCESS DENIED: could not verify the speaker."), Some("I could not check your voice just now. Please try again."));
    // Nobody spoke to Aqua (no wake word, noise, a duplicate, a timeout): there is nothing to answer.
    for silent in ["IGNORED: Wake-word not detected.", NO_SPEECH, NOT_SPEECH, "IGNORED: duplicate request.", "Pipeline error: the model is not available", "Today is Saturday.", ""] {
        assert_eq!(refusal_note(silent), None, "{silent:?}");
    }
    // The notes carry no percentage or other detail about the voiceprint, and say nothing a bystander could use.
    for note in [refusal_note("ACCESS DENIED: Unauthorized speaker (41.2% match).").unwrap(), refusal_note("ACCESS DENIED: could not verify the speaker.").unwrap()] {
        assert!(!note.contains('%') && note.chars().count() < 80, "{note}");
    }
}
