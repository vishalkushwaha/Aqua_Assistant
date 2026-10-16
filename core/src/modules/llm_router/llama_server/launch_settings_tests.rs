//! Tests of `launch_settings.rs`: the defaults and what changes them, the end of the server log, the threads, the switch and the idle time, the key and the port (move only: they were the inline module `voice_defaults` and part of the inline module `tests` of `llama_server.rs`; the path of every test is now `modules::llm_router::llama_server::launch_settings_tests::<name>`).

use super::*;

#[test]
fn the_defaults_are_one_slot_of_6144_and_the_measured_thread_count_and_the_overrides_change_them() {
    let saved: Vec<_> = ["AQUA_LLAMA_SLOTS", "AQUA_LLAMA_CTX", "AQUA_LLAMA_THREADS", "AQUA_LLAMA_THREADS_BATCH"]
        .into_iter()
        .map(|k| (k, std::env::var(k).ok()))
        .collect();
    for (k, _) in &saved {
        std::env::remove_var(k);
    }
    assert_eq!(slots(), 1);
    assert_eq!(context_tokens(), 6144, "the window the bake-off measured as the better one");
    // The old setting is one word away, and the window is never below what the server was started with before or above what it takes.
    std::env::set_var("AQUA_LLAMA_CTX", "2048");
    assert_eq!(context_tokens(), 2048);
    std::env::set_var("AQUA_LLAMA_CTX", "512");
    assert_eq!(context_tokens(), 2048);
    std::env::set_var("AQUA_LLAMA_CTX", "999999");
    assert_eq!(context_tokens(), 32768);
    std::env::set_var("AQUA_LLAMA_CTX", "large");
    assert_eq!(context_tokens(), 6144, "words that are not a number change nothing");
    std::env::remove_var("AQUA_LLAMA_CTX");
    // The layers on the card follow the window: the two points that were run (24 at 2048, 22 at 6144), the slope between and beyond them, and a floor.
    assert_eq!((default_gpu_layers(2048), default_gpu_layers(4096), default_gpu_layers(6144), default_gpu_layers(8192)), (24, 23, 22, 21));
    assert_eq!((default_gpu_layers(0), default_gpu_layers(1000), default_gpu_layers(32768), default_gpu_layers(u32::MAX)), (24, 24, 12, 12), "never more than 24 and never fewer than 12");
    assert_eq!(default_gpu_layers(context_tokens()), 22, "the default window and its layers are the pair that was measured fast");
    assert_eq!(threads(), default_threads().to_string());
    assert_eq!(threads_batch(), threads(), "reading the prompt takes the same count as writing, unless it is told otherwise");
    // Where the machine has the processors the measurement was made with, the default is the measured count.
    if aqua_audio_adapter::cpu::logical_processors() >= MEASURED_THREADS {
        assert_eq!(threads(), MEASURED_THREADS.to_string());
    }
    // The overrides: one for both, a second for the prompt alone, and words that are not a number change nothing.
    std::env::set_var("AQUA_LLAMA_THREADS", "3");
    assert_eq!((threads(), threads_batch()), ("3".to_string(), "3".to_string()));
    std::env::set_var("AQUA_LLAMA_THREADS_BATCH", "5");
    assert_eq!((threads(), threads_batch()), ("3".to_string(), "5".to_string()));
    std::env::set_var("AQUA_LLAMA_THREADS", "many");
    std::env::set_var("AQUA_LLAMA_THREADS_BATCH", "all");
    assert_eq!((threads(), threads_batch()), (default_threads().to_string(), default_threads().to_string()));
    for (k, v) in saved {
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    }
}

#[test]
fn the_end_of_the_server_log_becomes_one_line_for_an_error_message() {
    let dir = std::env::temp_dir().join(format!("aqua_llog_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("llama-server.log");
    assert_eq!(log_tail(&f), "", "no log, nothing to add");
    std::fs::write(&f, "line one
E failed to load model: out of memory

").unwrap();
    assert_eq!(log_tail(&f), ": line one E failed to load model: out of memory");
    std::fs::write(&f, format!("{}tail-marker", "x".repeat(2000))).unwrap();
    let t = log_tail(&f);
    assert!(t.len() <= 305 && t.ends_with("tail-marker"), "{} chars", t.len());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The thread count used to be one per physical core; it is now the count measured to be fastest (6, on a laptop with 4 physical cores and 8 logical processors),
/// and this test was left asking for the old rule. What it should hold is the rule that is there: the measured count, never more than the machine has,
/// never fewer than two. The rule is tried for machines other than this one, because this one only has one count.
#[test]
fn the_model_gets_the_measured_thread_count_but_never_more_threads_than_the_machine_has() {
    for (logical, want) in [(1, 2), (2, 2), (3, 3), (4, 4), (6, 6), (8, 6), (16, 6), (64, 6)] {
        assert_eq!(threads_for(logical), want, "{logical} logical processors");
    }
    let logical = aqua_audio_adapter::cpu::logical_processors();
    assert_eq!(default_threads(), threads_for(logical), "this machine has {logical}");
    assert!((2..=MEASURED_THREADS).contains(&default_threads()), "{}", default_threads());
    assert!(default_threads() <= logical.max(2), "never more threads than the machine has");
}

#[test]
fn it_can_be_turned_off_and_its_idle_time_is_bounded() {
    std::env::set_var("AQUA_LLAMA_SERVER", "0");
    assert!(!enabled());
    std::env::set_var("AQUA_LLAMA_SERVER", "off");
    assert!(!enabled());
    std::env::set_var("AQUA_LLAMA_SERVER", "1");
    assert!(enabled());
    std::env::remove_var("AQUA_LLAMA_SERVER");
    assert!(enabled(), "on by default");
    std::env::set_var("AQUA_LLAMA_IDLE_SECS", "1");
    assert_eq!(idle_timeout(), Duration::from_secs(30), "never shorter than 30 s");
    std::env::set_var("AQUA_LLAMA_IDLE_SECS", "99999999");
    assert_eq!(idle_timeout(), Duration::from_secs(86_400));
    std::env::remove_var("AQUA_LLAMA_IDLE_SECS");
    assert_eq!(idle_timeout(), Duration::from_secs(7_200), "two hours: a model that is unloaded after ten minutes costs 15 to 85 s to load and 50 to 200 s to read the agent prompt again");
}

#[test]
fn keys_are_long_and_different_and_ports_are_local_and_free() {
    let (a, b) = (random_key(), random_key());
    assert!(a.len() >= 32 && a != b && a.chars().all(|c| c.is_ascii_hexdigit()));
    let p = free_port().unwrap();
    assert!(p > 1023);
}
