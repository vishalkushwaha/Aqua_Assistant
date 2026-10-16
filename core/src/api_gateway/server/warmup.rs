//! Loading the language model before it is asked for (card OA3-06, moved out of `server.rs`): the warm-up at start and the pre-warm when the wake phrase is heard.

use super::*;

fn warmup_enabled() -> bool {
    !matches!(std::env::var("AQUA_WARMUP").ok().as_deref(), Some("0" | "off" | "false" | "no"))
}

/// The first request after a start used to take about 70 seconds: the Laya gate needed 42 s to warm up (past its own
/// 15 s limit, so the first request got the cautious fallback) and the language model needed 33 s to load. Both are
/// done in the background, so the first real request finds them ready. It is started as soon as the engines exist, in
/// parallel with the startup checks (which used to finish first: the model load began only after the gateway had bound,
/// plus a 5 second wait), and the second call, from the gateway, does nothing.
/// `AQUA_WARMUP=0` turns it off (it keeps the 8B model in memory for the idle time, 2 hours by default).
pub fn start_warmup(laya: Arc<crate::laya_validator::LayaGate>, llm: Arc<crate::modules::llm_router::LlamaRouterEngine>) {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !warmup_enabled() || STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new().name("aqua-warmup".into()).spawn(move || {
        // A model that goes on the graphics card, and the speech engine, which also uses the card, each size themselves from the
        // video memory that is free when they start. Started together they would count the same memory twice, so the speech
        // engine goes first (it is ready in about 8 seconds). Nothing waits when the model runs on the processor.
        if crate::modules::llm_router::gpu::enabled() && crate::modules::llm_router::gpu::gpu_server_binary().is_some() {
            crate::startup_health::wait_for_kokoro_daemon(std::time::Duration::from_secs(25));
        }
        let started = std::time::Instant::now();
        let _ = laya.decide("What is two plus two?", false);
        info!("Warm-up: the Laya gate is ready ({} ms)", started.elapsed().as_millis());
        let started = std::time::Instant::now();
        // The model chat and the agent really use (the 8B), not whichever model is "active" (the small one by default):
        // warming the wrong one made the first real turn unload it and load the 8B, about 30 seconds.
        let agent_model = llm.agent_model_filename();
        // It also reads the start of the chat prompt and of the agent's prompt now, in the background (a real request goes
        // first between two pieces). The first request of each kind used to read them itself, at about 25 tokens a second:
        // 50 to 200 seconds for the agent's.
        // Chat only. The agent prefix is warmed later, from the planner block of the request that will use it.
        // Warming data/cache/agent_prompt_start.txt here used a tool list from another run.
        match llm.warm_up_prompts(&agent_model, &crate::pipeline::chat_warmup_prompt(), "") {
            Ok(pieces) => info!("Warm-up: the language model is loaded and has read the start of its prompts ({pieces} pieces, {} ms)", started.elapsed().as_millis()),
            Err(e) => tracing::warn!("Warm-up: the language model could not be loaded: {e:#}"),
        }
    });
}

/// The wake word was heard, so a request is on its way (a person takes 3 to 10 seconds to say it): if the language model was stopped
/// for being idle, load it now and read the start of its prompts, so that the request finds it loaded and not still loading
/// (15 to 40 seconds). Nothing happens when the model is already loaded, or while a warm-up is under way.
pub fn prewarm_for_request(llm: Arc<crate::modules::llm_router::LlamaRouterEngine>) {
    static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !warmup_enabled() || crate::modules::llm_router::llama_server::is_running() || BUSY.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new().name("aqua-prewarm".into()).spawn(move || {
        let started = std::time::Instant::now();
        let agent_model = llm.agent_model_filename();
        match llm.warm_up_prompts(&agent_model, &crate::pipeline::chat_warmup_prompt(), "") {
            Ok(pieces) => info!("Pre-warm on wake: the language model is loaded and has read the start of its prompts ({pieces} pieces, {} ms)", started.elapsed().as_millis()),
            Err(e) => tracing::warn!("Pre-warm on wake: the language model could not be loaded: {e:#}"),
        }
        BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
    });
}
