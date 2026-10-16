//! Tests of `turn_queue.rs`: the wait for the model's turn, the yield to a person and the count of those who wait (move only: this was the inline module `queue_tests` of `llama_server.rs`; the path of every test is now `modules::llm_router::llama_server::turn_queue_tests::<name>`).

use super::*;

/// A thread that waits in `lock_within` is counted in `WAITING_PERSON` while it waits, so the tests that wait (or that read the count) take turns.
static COUNT_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn a_request_waits_for_its_turn_but_not_forever() {
    let _turn = COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let m = Mutex::new(1u8);
    // free: taken at once
    assert!(lock_within(&m, Duration::from_millis(200), |_| {}).is_some());
    // held by someone else: gives up at the limit instead of waiting indefinitely
    let held = m.lock().unwrap();
    let started = Instant::now();
    assert!(lock_within(&m, Duration::from_millis(300), |_| {}).is_none());
    assert!(started.elapsed() >= Duration::from_millis(300) && started.elapsed() < Duration::from_secs(5));
    drop(held);
    assert!(lock_within(&m, Duration::from_millis(200), |_| {}).is_some(), "available again once released");
}

/// Card R6-03, T6-03.4: a background call gives the model up only when the switch is on, only when it is a background call, and only to a person who is waiting.
#[test]
fn a_background_call_gives_the_model_up_only_with_the_switch_on_and_only_to_a_person_who_is_waiting() {
    for (switch, background, person, expected) in [(true, true, true, true), (false, true, true, false), (true, false, true, false), (true, true, false, false), (false, false, false, false)] {
        assert_eq!(should_yield(switch, background, person), expected, "switch {switch}, background {background}, person waiting {person}");
    }
    // The switch is off unless it is set to on.
    let saved = std::env::var("AQUA_BACKGROUND_YIELD").ok();
    std::env::remove_var("AQUA_BACKGROUND_YIELD");
    assert!(!background_yield_enabled(), "off by default");
    for (value, on) in [("on", true), ("1", true), (" TRUE ", true), ("off", false), ("0", false), ("sometimes", false), ("", false)] {
        std::env::set_var("AQUA_BACKGROUND_YIELD", value);
        assert_eq!(background_yield_enabled(), on, "{value:?}");
    }
    match saved {
        Some(v) => std::env::set_var("AQUA_BACKGROUND_YIELD", v),
        None => std::env::remove_var("AQUA_BACKGROUND_YIELD"),
    }
}

/// The error of a call that yielded is told from every other error, and above all from the person's Stop (whose text ends the run).
#[test]
fn the_error_of_a_call_that_gave_the_model_up_is_not_taken_for_the_persons_stop_or_for_a_failure() {
    let yielded = anyhow::anyhow!(yielded_message());
    assert!(is_yield(&yielded) && is_yield(&yielded.context("the agent step failed")), "found through the context a caller adds");
    assert!(matches!(yielded_failure(), Failure::Timeout(m) if m == yielded_message()), "it travels as a time-out, which every caller hands on unchanged");
    for other in ["stopped: the task was cancelled", "llama timed out after 90s (model=x, prompt was 5 chars)", "the language model is busy with another request (waited 120s)", ""] {
        assert!(!is_yield(&anyhow::anyhow!(other.to_string())), "{other:?}");
    }
    assert!(!yielded_message().contains("the task was cancelled"), "the loop reads that text as the person's Stop and ends the run");
}

/// A person who waits for the model is counted; a job in the background, a flow or a practice run that waits is not (it has no one to hurry for), and the count goes down again.
#[test]
fn only_the_request_of_a_person_is_counted_as_waiting_for_the_model() {
    let _turn = COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let m = std::sync::Arc::new(Mutex::new(0u8));
    let held = m.lock().unwrap();
    let before = WAITING_PERSON.load(std::sync::atomic::Ordering::SeqCst);
    // A thread marked unattended (a flow, a workflow, a practice run, or a background job) waits: the count of persons does not move.
    let unattended_waiter = {
        let m = m.clone();
        std::thread::spawn(move || {
            let _unattended = aqua_mcp_adapters::browser::unattended();
            lock_within(&m, Duration::from_millis(800), |_| {}).is_none()
        })
    };
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(WAITING_PERSON.load(std::sync::atomic::Ordering::SeqCst), before, "an unattended thread that waits is not a person");
    // An ordinary thread (a typed or spoken turn) waits: it is counted while it waits.
    let person_waiter = {
        let m = m.clone();
        std::thread::spawn(move || lock_within(&m, Duration::from_millis(600), |_| {}).is_none())
    };
    std::thread::sleep(Duration::from_millis(250));
    assert!(person_waiting() && WAITING_PERSON.load(std::sync::atomic::Ordering::SeqCst) > before, "a person who waits is counted");
    assert!(person_waiter.join().unwrap() && unattended_waiter.join().unwrap(), "both gave up at their limits while the lock was held");
    drop(held);
    assert_eq!(WAITING_PERSON.load(std::sync::atomic::Ordering::SeqCst), before, "and the count goes back down");
    // The wait after a yield ends when nobody waits any more, and at its limit otherwise.
    let started = Instant::now();
    wait_until_no_person_waits(Duration::from_secs(5), || false);
    if !person_waiting() {
        assert!(started.elapsed() < Duration::from_secs(1), "nobody waits: no wait");
    }
    let _w = Waiting::begin(true);
    let started = Instant::now();
    wait_until_no_person_waits(Duration::from_millis(200), || false);
    assert!(started.elapsed() >= Duration::from_millis(200) && started.elapsed() < Duration::from_secs(3), "a person who waits holds the call back, up to the limit");
    let started = Instant::now();
    wait_until_no_person_waits(Duration::from_secs(5), || true);
    assert!(started.elapsed() < Duration::from_secs(1), "and a Stop ends the wait");
}

/// A call that is told not to yield (a run that has already yielded often enough) is not asked to again, and the mark is gone afterwards.
#[test]
fn a_call_made_without_yielding_is_marked_for_its_length_and_the_mark_is_restored() {
    assert!(!NO_YIELD.with(|c| c.get()));
    let inside = without_yielding(|| NO_YIELD.with(|c| c.get()));
    assert!(inside && !NO_YIELD.with(|c| c.get()));
    let nested = without_yielding(|| without_yielding(|| 1) + NO_YIELD.with(|c| c.get() as i32));
    assert_eq!(nested, 2, "still marked after an inner call ended");
}

#[test]
fn a_wait_that_is_cut_short_by_the_holder_finishing_succeeds_and_a_poisoned_lock_is_recovered() {
    let _turn = COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let m = std::sync::Arc::new(Mutex::new(0u8));
    let holder = m.clone();
    let t = std::thread::spawn(move || {
        let _g = holder.lock().unwrap();
        std::thread::sleep(Duration::from_millis(250));
    });
    std::thread::sleep(Duration::from_millis(50));
    assert!(lock_within(&m, Duration::from_secs(5), |_| {}).is_some(), "the holder finished while we waited");
    t.join().unwrap();
    let poisoned = std::sync::Arc::new(Mutex::new(0u8));
    let p2 = poisoned.clone();
    let _ = std::thread::spawn(move || {
        let _g = p2.lock().unwrap();
        panic!("a panic while holding the lock");
    })
    .join();
    assert!(lock_within(&poisoned, Duration::from_millis(200), |_| {}).is_some(), "poison does not lock everyone out");
}

#[test]
fn the_queue_limit_has_a_sane_default_and_can_be_changed_within_bounds() {
    std::env::remove_var("AQUA_LLAMA_QUEUE_SECS");
    assert_eq!(queue_wait(), Duration::from_secs(120));
}
