use super::voice_security_status as status;

#[test]
fn a_missing_voiceprint_model_or_enrolment_is_reported_not_hidden() {
    assert!(!status(true, Some(false), Some(false)).ready && status(true, Some(false), Some(true)).note.contains("model is missing"));
    assert!(!status(true, Some(true), Some(false)).ready && status(true, Some(true), Some(false)).note.contains("no voice is enrolled"));
    assert!(status(true, Some(true), Some(true)).ready);
    assert!(status(false, Some(false), Some(false)).ready, "with security off nothing is refused");
    assert!(status(true, None, None).ready, "a busy engine is unknown, not a failure");
}

#[test]
fn a_voice_that_is_recorded_but_cannot_be_read_is_reported_as_unreadable_not_as_never_enrolled() {
    use super::voice_security_status_with_stored as with_stored;
    let lost = with_stored(true, Some(true), Some(false), Some(true));
    assert!(!lost.ready && lost.note.contains("could not be read") && lost.note.contains("Enrol your voice again"), "{}", lost.note);
    let never = with_stored(true, Some(true), Some(false), Some(false));
    assert!(!never.ready && never.note.contains("no voice is enrolled"));
    assert!(with_stored(true, Some(true), Some(true), Some(true)).ready);
    assert!(with_stored(false, Some(true), Some(false), Some(true)).ready, "security off: nothing is refused");
}
