use std::sync::Arc;
#[tokio::test]
async fn a_turn_whose_caller_hung_up_is_told_to_stop() {
    let seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (s2, st2) = (seen.clone(), started.clone());
    let turn = super::run_turn(move || {
        st2.store(true, std::sync::atomic::Ordering::SeqCst);
        for _ in 0..200 {
            if crate::modules::browser_agent::turn_cancelled_for_test() {
                s2.store(true, std::sync::atomic::Ordering::SeqCst);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Ok::<_, String>(())
    });
    // the caller gives up shortly after the work has begun
    let _ = tokio::time::timeout(std::time::Duration::from_millis(150), turn).await;
    for _ in 0..100 {
        if seen.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(started.load(std::sync::atomic::Ordering::SeqCst));
    assert!(seen.load(std::sync::atomic::Ordering::SeqCst), "the work was never told to stop");
}

#[tokio::test]
async fn work_that_finishes_returns_its_result_and_an_error_is_passed_on() {
    assert_eq!(super::run_turn(|| Ok::<_, String>(7)).await.unwrap(), 7);
    assert_eq!(super::run_turn(|| Err::<u8, _>("boom".to_string())).await.unwrap_err(), "boom");
}

/// The stages a turn notes on its blocking thread reach the request that is waiting for it (they become `stages_ms`).
#[tokio::test]
async fn a_turn_hands_back_where_its_time_went() {
    let (value, stages) = super::run_turn_timed(|| {
        crate::turn_timing::stage("stt", || std::thread::sleep(std::time::Duration::from_millis(30)));
        crate::turn_timing::stage("tts", || ());
        Ok::<_, String>(5)
    })
    .await
    .unwrap();
    assert_eq!(value, 5);
    assert_eq!(stages.iter().map(|s| s.stage).collect::<Vec<_>>(), ["stt", "tts"]);
    assert!(stages[0].ms >= 25, "the sleep shows in the first stage: {stages:?}");
    // A turn that notes nothing hands back nothing (not the previous turn's), and an error still reaches the caller.
    let (_, none) = super::run_turn_timed(|| Ok::<_, String>(1)).await.unwrap();
    assert!(none.is_empty(), "{none:?}");
    assert_eq!(super::run_turn_timed(|| Err::<u8, _>("boom".to_string())).await.unwrap_err(), "boom");
}

/// OA4-03 (UX-09). The turn's own limit was a fixed 600 s, below the 900 s a typed run is allowed (and the 7,200 s `AQUA_AGENT_MAX_SECS` allows): it is the agent's clock and a short grace.
#[test]
fn turn_max_is_not_below_the_agent_clock() {
    assert_eq!(super::turn_max_for(std::time::Duration::from_secs(900)), std::time::Duration::from_secs(930));
    assert_eq!(super::turn_max_for(std::time::Duration::from_secs(1800)), std::time::Duration::from_secs(1830), "a typed run with AQUA_AGENT_MAX_SECS=1800 can run 30 minutes in the foreground");
    assert_eq!(super::turn_max_for(std::time::Duration::from_secs(7200)), std::time::Duration::from_secs(7230));
    // Whatever the environment says, the turn is held for at least the longest the loop lets a run go, and for longer than the 600 s that it was.
    let clock = crate::modules::browser_agent::longest_agent_run();
    assert!(super::turn_max() > clock);
    if std::env::var("AQUA_AGENT_MAX_SECS").is_err() {
        assert!(super::turn_max() > std::time::Duration::from_secs(600) && clock.as_secs() == 900);
    }
}

#[tokio::test]
async fn a_panic_in_a_turn_is_an_error_not_a_crash() {
    let err = super::run_turn(|| -> Result<u8, String> { panic!("inside the turn") }).await.unwrap_err();
    assert!(err.contains("stopped unexpectedly"), "{err}");
}
