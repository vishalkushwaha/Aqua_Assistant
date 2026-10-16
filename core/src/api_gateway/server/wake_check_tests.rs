use super::*;

#[test]
fn the_response_says_why_a_clip_was_not_checked_and_hides_the_field_when_it_was() {
    let skipped = serde_json::to_value(WakeCheckResponse::skipped("busy")).unwrap();
    assert_eq!(skipped, serde_json::json!({"wake": false, "heard": "", "skipped": "busy"}));
    let heard = serde_json::to_value(WakeCheckResponse { wake: true, heard: "Hey Aqua".into(), skipped: None }).unwrap();
    assert_eq!(heard, serde_json::json!({"wake": true, "heard": "Hey Aqua"}), "no `skipped` key when it was checked");
}

#[test]
fn the_wake_lane_is_separate_from_the_turn_queue_and_takes_one_check_at_a_time() {
    assert!(!Arc::ptr_eq(&wake_slot(), &turn_gate()), "room chatter must never share the queue real requests wait in");
    let first = wake_slot().try_acquire_owned().expect("a free lane");
    assert!(wake_slot().try_acquire_owned().is_err(), "a second check is skipped, not queued");
    drop(first);
    assert!(wake_slot().try_acquire_owned().is_ok());
    assert_eq!(WAKE_MODEL, "ggml-base.en.bin");
}
