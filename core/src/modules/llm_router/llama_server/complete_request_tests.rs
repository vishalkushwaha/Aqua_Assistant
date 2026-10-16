//! Tests of `complete_request.rs`, `server_start.rs` and `server_process.rs` against the real server and the small model, skipped when there is none (move only: they were part of the inline module `tests` of `llama_server.rs`; the two measurements of the 8B model stay in that module, in `llama_server.rs`; the path of every test is now `modules::llm_router::llama_server::complete_request_tests::<name>`).

use super::*;
use super::tests::THE_SERVER;

/// Against the real server on the real card (skipped without a GPU build or a card): the model answers, and it says it is on the
/// card. The small model, so that loading it does not take long.
#[test]
fn the_real_server_runs_on_the_graphics_card_when_there_is_one_and_answers() {
    let _only_one = THE_SERVER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _state = super::super::gpu::STATE_TESTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    super::super::gpu::forget_failures();
    let model = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../data/models/llm/qwen-0.5b-instruct-q4_k_m.gguf");
    let Some(plan) = super::super::gpu::plan() else {
        eprintln!("skipped: no GPU build of llama-server, or no graphics card with free memory");
        return;
    };
    if !model.is_file() {
        eprintln!("skipped: no small model");
        return;
    }
    shutdown();
    let answer = complete(&model, "Say hello in one short sentence.", 24, Duration::from_secs(300), |_| {}).unwrap_or_else(|e| panic!("on the card: {e:?}"));
    assert!(!answer.trim().is_empty());
    let device = gpu_in_use().expect("the model is on the graphics card");
    assert!(device.contains(&plan.device.name), "{device}");
    assert!(!super::super::gpu::is_off(), "nothing failed");
    shutdown();
    assert!(gpu_in_use().is_none(), "a stopped server is on no card");
}

/// A server on the card that dies while it is answering (a lost device, an out-of-memory error: here, the process is killed
/// under the request): the same request is answered by a server on the processor, and the card is left alone for a while.
#[test]
fn a_server_on_the_card_that_dies_under_a_request_is_replaced_by_one_on_the_processor_and_the_request_is_answered() {
    let _only_one = THE_SERVER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _state = super::super::gpu::STATE_TESTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    super::super::gpu::forget_failures();
    let model = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../data/models/llm/qwen-0.5b-instruct-q4_k_m.gguf");
    if super::super::gpu::plan().is_none() || server_binary().is_none() || !model.is_file() {
        eprintln!("skipped: no GPU build or card, no processor build, or no small model");
        return;
    }
    shutdown();
    assert!(complete(&model, "Say hi.", 4, Duration::from_secs(300), |_| {}).is_ok());
    assert!(gpu_in_use().is_some(), "the server is on the card");
    let pid = server_pid().expect("a running server");
    // Kill it while it writes a long answer.
    let killer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(700));
        let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    });
    let answer = complete(&model, "Write a long story about a dragon with many chapters.", 600, Duration::from_secs(300), |_| {});
    killer.join().unwrap();
    let answer = answer.unwrap_or_else(|e| panic!("the request was lost: {e:?}"));
    assert!(!answer.trim().is_empty());
    assert!(gpu_in_use().is_none(), "the answer came from a server on the processor");
    assert!(super::super::gpu::is_off(), "the card is left alone for a while");
    super::super::gpu::forget_failures();
    shutdown();
}

/// A card that cannot load the model (here: a device that does not exist, which the server refuses to start with) must not cost
/// the request: the same model starts on the processor, and the card is left alone for a while.
#[test]
fn a_card_that_cannot_load_the_model_falls_back_to_the_processor_and_the_request_is_not_lost() {
    let _only_one = THE_SERVER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _state = super::super::gpu::STATE_TESTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let model = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../data/models/llm/qwen-0.5b-instruct-q4_k_m.gguf");
    let Some(gpu_binary) = super::super::gpu::gpu_server_binary() else {
        eprintln!("skipped: no GPU build of llama-server");
        return;
    };
    if server_binary().is_none() || !model.is_file() {
        eprintln!("skipped: no llama-server or no small model");
        return;
    }
    super::super::gpu::forget_failures();
    shutdown();
    std::env::set_var("AQUA_LLAMA_GPU_SERVER_BIN", &gpu_binary);
    std::env::set_var("AQUA_LLAMA_DEVICE", "Vulkan9");
    let answer = complete(&model, "Say hello in one short sentence.", 24, Duration::from_secs(300), |_| {});
    std::env::remove_var("AQUA_LLAMA_DEVICE");
    std::env::remove_var("AQUA_LLAMA_GPU_SERVER_BIN");
    let answer = answer.unwrap_or_else(|e| panic!("the request was lost: {e:?}"));
    assert!(!answer.trim().is_empty());
    assert!(gpu_in_use().is_none(), "the answer came from the processor");
    assert!(super::super::gpu::is_off(), "and the card is left alone for a while");
    super::super::gpu::forget_failures();
    shutdown();
}

/// Keeps the model on the processor for as long as it lives (the variable is removed even when the test fails).
struct GpuOff;

impl GpuOff {
    fn new() -> Self {
        std::env::set_var("AQUA_LLAMA_GPU", "off");
        GpuOff
    }
}

impl Drop for GpuOff {
    fn drop(&mut self) {
        std::env::remove_var("AQUA_LLAMA_GPU");
    }
}

/// A request that runs past its time limit is hung up on, and a server on the processor that was only slow keeps its model: the
/// next request does not wait for a reload. (The small model writes 400 tokens in a few seconds; the limit is one. The processor
/// is chosen here: on a card a time-out means trouble, see the next test.)
#[test]
fn a_slow_request_that_times_out_does_not_cost_a_server_on_the_processor_its_loaded_model() {
    let _only_one = THE_SERVER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _state = super::super::gpu::STATE_TESTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let model = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../data/models/llm/qwen-0.5b-instruct-q4_k_m.gguf");
    if server_binary().is_none() || !model.is_file() {
        eprintln!("skipped: no llama-server or no small model");
        return;
    }
    let _cpu_only = GpuOff::new();
    shutdown();
    // Load it first with a request that cannot time out, so the limit below is about the answer and not the load.
    assert!(complete(&model, "Say hi.", 4, Duration::from_secs(300), |_| {}).is_ok());
    let started = Instant::now();
    let slow = complete(&model, "Write a very long story about a dragon, with many chapters.", 400, Duration::from_secs(1), |_| {});
    assert!(matches!(slow, Err(Failure::Timeout(_))), "{slow:?}");
    assert!(is_running(), "the server was only slow, so it is still loaded (it took {:?} to hang up)", started.elapsed());
    let again = Instant::now();
    assert!(complete(&model, "Say hi.", 4, Duration::from_secs(300), |_| {}).is_ok());
    assert!(again.elapsed() < Duration::from_secs(10), "no reload: {:?}", again.elapsed());
    shutdown();
}

/// On the card a request that outlasts its limit means the card is in trouble (its memory has overflowed, for one): the model
/// moves to the processor, which answers at a steady speed. Skipped without a GPU build or a card.
#[test]
fn a_request_that_times_out_on_the_graphics_card_moves_the_model_to_the_processor() {
    let _only_one = THE_SERVER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _state = super::super::gpu::STATE_TESTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    super::super::gpu::forget_failures();
    let model = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../data/models/llm/qwen-0.5b-instruct-q4_k_m.gguf");
    if super::super::gpu::plan().is_none() || server_binary().is_none() || !model.is_file() {
        eprintln!("skipped: no GPU build or card, no processor build, or no small model");
        return;
    }
    shutdown();
    assert!(complete(&model, "Say hi.", 4, Duration::from_secs(300), |_| {}).is_ok());
    assert!(gpu_in_use().is_some(), "the server is on the card");
    let slow = complete(&model, "Write a very long story about a dragon, with many chapters.", 800, Duration::from_secs(1), |_| {});
    assert!(matches!(slow, Err(Failure::Timeout(_))), "{slow:?}");
    assert!(!is_running(), "the server on the card was stopped");
    assert!(super::super::gpu::is_off(), "and the card is left alone for a while");
    assert!(complete(&model, "Say hi.", 4, Duration::from_secs(300), |_| {}).is_ok(), "the next request is answered on the processor");
    assert!(gpu_in_use().is_none());
    super::super::gpu::forget_failures();
    shutdown();
}

#[test]
fn a_missing_binary_is_reported_as_unavailable_so_the_caller_can_fall_back() {
    let _only_one = THE_SERVER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var("AQUA_LLAMA_SERVER_BIN", "C:/definitely/not/llama-server.exe");
    // The environment override is ignored when it points nowhere; the sibling binary may still exist,
    // so only assert the failure shape when nothing can be found.
    if server_binary().is_none() {
        let r = complete(Path::new("C:/no/model.gguf"), "hi", 8, Duration::from_secs(5), |_| {});
        assert!(matches!(r, Err(Failure::Unavailable(_))));
    }
    std::env::remove_var("AQUA_LLAMA_SERVER_BIN");
    shutdown();
    assert!(!is_running());
}

/// Against the real server and a small real model: the second request with the same start must
/// come from the cache and be much faster. Skipped when the binary or model is missing.
#[test]
fn the_real_server_reuses_the_prompt_it_has_already_read() {
    let _only_one = THE_SERVER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let model = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../data/models/llm/qwen-0.5b-instruct-q4_k_m.gguf");
    if server_binary().is_none() || !model.is_file() {
        eprintln!("skipped: no llama-server or no small model");
        return;
    }
    shutdown();
    let prefix = "You are Aqua, a local PC agent. Tools: browser_navigate opens a URL; browser_snapshot reads the page. Reply with ONLY JSON. ".repeat(40);
    let t0 = Instant::now();
    let first = complete(&model, &format!("{prefix}\nGoal: say hi."), 16, Duration::from_secs(300), |_| {}).unwrap_or_else(|e| panic!("first request: {e:?}"));
    let first_time = t0.elapsed();
    assert!(is_running(), "the server stays up between requests");
    let t1 = Instant::now();
    let second = complete(&model, &format!("{prefix}\nGoal: say bye."), 16, Duration::from_secs(300), |_| {}).unwrap_or_else(|e| panic!("second request: {e:?}"));
    let second_time = t1.elapsed();
    assert!(!first.trim().is_empty() && !second.trim().is_empty());
    eprintln!("first {first_time:?} (includes loading the model), second {second_time:?}");
    assert!(second_time < first_time, "the cached request is faster: {first_time:?} vs {second_time:?}");
    // It stops when asked, and starts again on demand.
    shutdown();
    assert!(!is_running());
    assert!(complete(&model, "Goal: say hi.", 8, Duration::from_secs(300), |_| {}).is_ok());
    shutdown();
}

/// Against the real server and the agent's real prompts: what two consecutive rounds share (the same start; only the end
/// changes) is what the server's cache holds, token for token. So a round reads only what is new.
#[test]
fn the_real_server_caches_what_consecutive_agent_prompts_share() {
    let _only_one = THE_SERVER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let model = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../data/models/llm/qwen-0.5b-instruct-q4_k_m.gguf");
    if server_binary().is_none() || !model.is_file() {
        eprintln!("skipped: no llama-server or no small model");
        return;
    }
    let prompts = crate::modules::browser_agent::cache_probe_prompts("What is the weather in Pune right now", 4);
    shutdown();
    let replies: Vec<Reply> = prompts
        .iter()
        .map(|p| complete_checked(&model, p, 2, slot_for(AGENT_SLOT), Duration::from_secs(300), |_| {}).unwrap_or_else(|e| panic!("request: {e:?}")))
        .collect();
    let count_tokens = |text: &str| -> usize {
        let guard = SERVER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let running = guard.as_ref().expect("the server is running");
        let v = post_json(running.port, &running.key, "/tokenize", &json!({ "content": text }), Duration::from_secs(30), &std::sync::atomic::AtomicBool::new(false)).expect("tokenize");
        v["tokens"].as_array().map_or(0, |a| a.len())
    };
    for k in 1..prompts.len() {
        let shared = prompts[k - 1].chars().zip(prompts[k].chars()).take_while(|(a, b)| a == b).count();
        let stable: String = prompts[k].chars().take(shared).collect();
        let stable_tokens = count_tokens(&stable);
        assert!(stable_tokens > 1_000, "round {}: the two prompts share a long start ({stable_tokens} tokens)", k + 1);
        // The server's count includes the few tokens of the chat template in front of the prompt, hence the slack.
        assert!(
            replies[k].prompt_tokens_cached + 8 >= stable_tokens as u64,
            "round {}: the server cached {} tokens but the prompts share {stable_tokens}: {:?}",
            k + 1,
            replies[k].prompt_tokens_cached,
            replies[k]
        );
        eprintln!("round {}: shared start {stable_tokens} tokens, cached {}, read {}", k + 1, replies[k].prompt_tokens_cached, replies[k].prompt_tokens_read);
    }
    shutdown();
}

/// Against the real server: after the warm-up has read the start of the agent's prompt (in pieces, as `warm_up_prompts`
/// does), the first task reads only what is its own (the goal), not the rules and the list of tools.
#[test]
fn the_real_server_has_the_start_of_the_agents_prompt_ready_after_a_warm_up() {
    let _only_one = THE_SERVER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let model = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../data/models/llm/qwen-0.5b-instruct-q4_k_m.gguf");
    if server_binary().is_none() || !model.is_file() {
        eprintln!("skipped: no llama-server or no small model");
        return;
    }
    let first_round = crate::modules::browser_agent::cache_probe_prompts("What is the weather in Pune right now", 1).remove(0);
    let start = first_round.find("User goal:").map(|i| &first_round[..i]).expect("the prompt has a goal part").trim().to_string();
    let ask = |p: &str, max: u32| complete_checked(&model, p, max, slot_for(AGENT_SLOT), Duration::from_secs(300), |_| {}).unwrap_or_else(|e| panic!("request: {e:?}"));

    // Without a warm-up: the first task reads everything.
    shutdown();
    let cold = ask(&first_round, 2);
    // With one: the start is read in pieces beforehand, and the task reads only the rest.
    shutdown();
    let pieces = crate::modules::llm_router::llama_engine::prefix_pieces(&start, crate::modules::llm_router::llama_engine::PREFILL_CHUNK_CHARS);
    assert!(pieces.len() >= 2, "the start of the prompt is long enough to be read in pieces: {} characters", start.len());
    for piece in &pieces {
        ask(piece, 1);
    }
    let warm = ask(&first_round, 2);
    eprintln!("first task, no warm-up: read {} tokens; after a warm-up of {} pieces: read {} tokens (cached {})", cold.prompt_tokens_read, pieces.len(), warm.prompt_tokens_read, warm.prompt_tokens_cached);
    assert!(cold.prompt_tokens_read > 1_000, "{cold:?}");
    assert!(warm.prompt_tokens_read * 5 < cold.prompt_tokens_read, "after the warm-up only the goal is read: {warm:?} against {cold:?}");
    shutdown();
}

/// Against the real server: a request that names a cache slot goes to that slot, so a chat turn cannot take over the
/// slot that holds the agent's prompt. (Left to itself the server gives a request the slot that shares most with it.)
#[test]
fn the_real_server_keeps_the_agents_prompt_in_its_own_slot() {
    let _only_one = THE_SERVER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let model = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../data/models/llm/qwen-0.5b-instruct-q4_k_m.gguf");
    if server_binary().is_none() || !model.is_file() {
        eprintln!("skipped: no llama-server or no small model");
        return;
    }
    if slots() < 2 {
        eprintln!("skipped: the server is set to one slot");
        return;
    }
    shutdown();
    let agent_prefix = "You are Aqua, a local PC agent. Tools: browser_navigate opens a URL; browser_snapshot reads the page. Reply with ONLY JSON. ".repeat(30);
    let chat_prefix = "You are Aqua, a private assistant. Answer briefly, in plain sentences that sound natural when spoken. ".repeat(30);
    let ask = |prompt: String, slot: u32| complete_checked(&model, &prompt, 4, Some(slot), Duration::from_secs(300), |_| {}).unwrap_or_else(|e| panic!("request: {e:?}"));
    let first = ask(format!("{agent_prefix}\nGoal: one."), AGENT_SLOT);
    assert!(first.prompt_tokens_read > 200, "the first request reads everything: {first:?}");
    // A chat turn in between, in the chat slot.
    let _ = ask(format!("{chat_prefix}\nThe user said: hello."), CHAT_SLOT);
    // The agent's next request still finds its prompt where it left it.
    let next = ask(format!("{agent_prefix}\nGoal: two."), AGENT_SLOT);
    assert!(next.prompt_tokens_cached * 10 >= first.prompt_tokens_read * 8, "agent prompt reused after a chat turn: {first:?} then {next:?}");
    shutdown();
}
