//! Tests of `server_process.rs`: asking about the server does not wait for a request (move only: it was part of the inline module `tests` of `llama_server.rs`; the path of the test is now `modules::llm_router::llama_server::server_process_tests::<name>`).

use super::*;
use super::tests::THE_SERVER;

/// Found in a review: the status page asks for the card every 5 seconds, and the answer waited on the lock that every request holds for
/// its whole time, so a thread of the web server was lost for as long as an answer took (a minute or more for an agent turn).
#[test]
fn asking_about_the_server_does_not_wait_for_a_request_that_is_being_answered() {
    let _only_one = THE_SERVER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    shutdown();
    *GLANCE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Some("Test card (Vulkan0, 4000 MiB free)".to_string()));
    let in_use = SERVER.lock().unwrap_or_else(|e| e.into_inner()); // what a request in progress holds
    // The premise: asking the lock itself, as the two functions did, waits until the request is over.
    let plain = std::thread::spawn(|| drop(SERVER.lock().unwrap_or_else(|e| e.into_inner())));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!plain.is_finished(), "a plain lock waits for the request, which is what the status page used to do");
    let asker = std::thread::spawn(|| {
        let started = Instant::now();
        let answers = (is_running(), gpu_in_use());
        (answers, started.elapsed())
    });
    let answered_meanwhile = (0..100).any(|_| {
        std::thread::sleep(Duration::from_millis(50));
        asker.is_finished()
    });
    drop(in_use);
    plain.join().unwrap();
    let ((running, card), took) = asker.join().unwrap();
    assert!(answered_meanwhile && took < Duration::from_secs(1), "asking waited for the request to end ({took:?})");
    assert!(running, "in use means running, or about to be");
    assert_eq!(card.as_deref(), Some("Test card (Vulkan0, 4000 MiB free)"), "while a request is answered it says where the model was last noted");
    // Free again, it looks for itself, and the note follows what is there.
    assert_eq!(gpu_in_use(), None);
    assert!(!is_running());
    show(None);
    assert_eq!(*GLANCE.lock().unwrap(), None);
}
