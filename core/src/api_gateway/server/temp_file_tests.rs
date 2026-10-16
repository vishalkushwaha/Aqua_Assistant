use super::RemoveOnDrop;

#[test]
fn a_recording_is_removed_even_when_the_work_panics() {
    let path = std::env::temp_dir().join(format!("aqua_rm_{}.wav", std::process::id()));
    std::fs::write(&path, b"voice").unwrap();
    let p2 = path.clone();
    let result = std::panic::catch_unwind(move || {
        let _cleanup = RemoveOnDrop(p2);
        panic!("inside the audio code");
    });
    assert!(result.is_err());
    assert!(!path.exists(), "the file must be gone after a panic");
}
