//! A persistent `llama-server` for the agent's planner.
//!
//! `llama-cli` starts, loads the 5 GB model, reads the whole prompt and exits, once per planning
//! step. On a CPU that is most of a minute of reading the same instructions again and again. The
//! server keeps the model loaded and keeps the KV cache of the prompt it has already read, so a step
//! only reads what changed since the last one (measured here: 37 s for the first request, 3 s for
//! the next two with the same 4,500-character prefix).
//!
//! * Started on first use, never at boot. Stopped after `AQUA_LLAMA_IDLE_SECS` (default 7200)
//!   without a request, so the model does not sit in RAM all day.
//! * Bound to 127.0.0.1 on a random port and protected by a random API key, so nothing else on the
//!   machine can use it.
//! * Placed in a Job Object that is closed when Aqua exits, however it exits, so it can never be
//!   left running.
//! * Optional: `AQUA_LLAMA_SERVER=0` turns it off. If it cannot start or fails, the caller falls
//!   back to `llama-cli`, so the agent works either way.
//! * One request at a time (Aqua's own lock). The server's cache slots (`--parallel`, `AQUA_LLAMA_SLOTS`, default 1)
//!   split `-c`. One slot of 6144 tokens is the default (card R6-20, from the bake-off of R6-31: on this 4 GB card the planner was right
//!   77% of the time at 6144 and 62% at 2048, for 140 MiB more video memory); `AQUA_LLAMA_CTX=2048` is the old setting. A second slot is opt-in:
//!   raise `AQUA_LLAMA_SLOTS` and `AQUA_LLAMA_CTX` together, because `-c` is the total, not the size of each slot.
//!
//! The code is cut by what it does (move only: no line of code was changed, and every `llama_server::X` is where it was because this file re-exports each part):
//! `request_outcome` (what a request ends in: the failure, the answer with what the server measured, and the words of a request that was refused, was busy, gave the model up or went quiet),
//! `request_json` (the JSON of a request, the cache slots, the tools of an offer), `launch_settings` (what the server is started with and where it is found: `AQUA_LLAMA_SERVER`, `AQUA_LLAMA_SERVER_BIN`,
//! `AQUA_LLAMA_IDLE_SECS`, `AQUA_LLAMA_CTX`, `AQUA_LLAMA_SLOTS`, `AQUA_LLAMA_THREADS`, `AQUA_LLAMA_THREADS_BATCH`, the layers on the card, the port, the key, the log),
//! `server_process` (the one running server: its lock, the glance that never waits, stop, the idle reaper, shutdown), `server_start` (the launch on the card or on the processor, `AQUA_LLAMA_NGL` and the switches of the
//! speed experiments, the wait for the health check), `turn_queue` (the wait for the model's turn, `AQUA_LLAMA_QUEUE_SECS`, and the yield to a person, `AQUA_BACKGROUND_YIELD`), `http_calls` (the plain and the streamed POST,
//! and the watch on a quiet stream) and `complete_request` (the `complete_*` calls and the one request they are all cases of). This file keeps the imports that every part reaches through `use super::*`, the declarations and the
//! re-exports. The tests are the `*_tests` modules, each in a file of its own, and the inline module `tests` (the lock that the tests which use the real server share, and the two measurements of the 8B model).

use anyhow::{anyhow, Context, Result};
use aqua_native_tools::contain::{Contained, Limits};
// The chunked decoder and the event splitter moved to the model layer (card OA9-04), where the provider for an Ollama or an LM Studio reads the same bytes with the same code.
use crate::model::sse::{ChunkedDecoder, EventSplitter};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tracing::{info, warn};
// The sibling modules that the parts name as `super::gpu::...`, `super::kv_slots::...`: they are reached through this file, so that no line of any part had to change.
use super::{draft_model, fast_server, gpu, kv_slots, launch_args, llama_cli, server_options};

/// What a request ends in: the failure, the answer, what the server measured, and the sentences of a request that did not get one.
mod request_outcome;
pub use self::request_outcome::*;

/// The JSON of one request: the cache slots, the constraint on the reply, the tools of an offer, what the model layer adds.
mod request_json;
pub use self::request_json::*;

/// What the server is started with and where it is found: the switch, the program, the window, the layers, the threads, the slots, the port, the key, the log.
mod launch_settings;
pub use self::launch_settings::*;

/// The one running server: its lock, the glance that never waits, stop, the idle reaper, shutdown.
mod server_process;
pub use self::server_process::*;

/// Starting the server on the card or on the processor, and the check that a server a request was hung up on is well.
mod server_start;
use self::server_start::*;

/// The wait for the model's turn and the yield of a background call to a person.
mod turn_queue;
pub use self::turn_queue::*;

/// The plain and the streamed POST to the server, and the watch on a stream that went quiet.
mod http_calls;
pub(crate) use self::http_calls::*;

/// The `complete_*` calls and the one request they are all cases of.
mod complete_request;
pub use self::complete_request::*;

/// The lock of the tests that start, use or stop the one model server, and the two measurements of the 8B model (ignored). They stay in this file, in this module, because
/// `kv_slots::tests::no_part_of_aqua_calls_the_experiment_functions` reads this file for the module `tests` and for the measuring test that calls the two experiment functions of `kv_slots.rs`.
#[cfg(test)]
mod tests {
    use super::*;

    /// The tests that start, use or stop the one model server of this process must not run at the same time (a `shutdown()` in
    /// one would kill the server in the middle of a request of another).
    pub(super) static THE_SERVER: Mutex<()> = Mutex::new(());

    /// A measurement, not a check (run it with `--ignored --nocapture`): the agent's real prompts, about 1,500 tokens at the start and a
    /// few hundred more each round, through Aqua's own server code with the real 8B model. Where the model runs depends on the
    /// environment: `AQUA_LLAMA_GPU=off` for the processor, `AQUA_LLAMA_SERVER_BIN` for a particular processor build.
    #[test]
    #[ignore = "a measurement: loads the 8B model and takes minutes"]
    fn measure_the_agents_rounds_on_the_8b_model() {
        let _only_one = THE_SERVER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _state = super::super::gpu::STATE_TESTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let model = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../data/models/llm/Qwen3-8B-Q4_K_M.gguf");
        if !model.is_file() {
            eprintln!("skipped: no 8B model");
            return;
        }
        shutdown();
        let rounds = crate::modules::browser_agent::cache_probe_prompts("What is the weather in Pune right now", 3);
        let loading = Instant::now();
        complete(&model, "Say hi.", 1, Duration::from_secs(600), |_| {}).unwrap_or_else(|e| panic!("load: {e:?}"));
        eprintln!("MEASURE the model is loaded and has answered after {:.1} s; on {}", loading.elapsed().as_secs_f32(), gpu_in_use().unwrap_or_else(|| "the processor".into()));
        for (i, prompt) in rounds.iter().enumerate() {
            let started = Instant::now();
            let reply = complete_checked(&model, prompt, 40, slot_for(AGENT_SLOT), Duration::from_secs(600), |_| {}).unwrap_or_else(|e| panic!("round {}: {e:?}", i + 1));
            let seconds = started.elapsed().as_secs_f32();
            eprintln!(
                "MEASURE round {}: {seconds:.1} s, read {} tokens (reused {}), {:.0} tokens a second, wrote {} characters",
                i + 1,
                reply.prompt_tokens_read,
                reply.prompt_tokens_cached,
                reply.prompt_tokens_read as f32 / seconds.max(0.001),
                reply.text.len()
            );
        }
        shutdown();
    }

    /// A measurement, not a check (card OA9-06, task PR-13; run it with `--ignored --nocapture`, Aqua closed, `AQUA_LLAMA_SLOT_SAVE=on`): what a saved slot buys. The agent's first
    /// prompt is read cold (the number the speed baseline calls round 1), the slot is saved, the server is stopped and started again, the slot is put back, and the same prompt is read
    /// again. It is the only caller of the two experiment functions of `kv_slots.rs`. Whatever it prints is the first measurement; nothing is decided from this file.
    #[test]
    #[ignore = "a measurement: loads the 8B model twice and takes minutes"]
    fn measure_slot_persistence_on_the_8b_model() {
        let _only_one = THE_SERVER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _state = super::super::gpu::STATE_TESTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let model = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../data/models/llm/Qwen3-8B-Q4_K_M.gguf");
        if !model.is_file() {
            eprintln!("skipped: no 8B model");
            return;
        }
        if !crate::flags::is_on(&crate::flags::LLAMA_SLOT_SAVE) {
            eprintln!("skipped: set AQUA_LLAMA_SLOT_SAVE=on, so that the server is started with a folder for saved slots");
            return;
        }
        shutdown();
        let prompt = crate::modules::browser_agent::cache_probe_prompts("What is the weather in Pune right now", 1).remove(0);
        let timed = |label: &str| {
            let started = Instant::now();
            let reply = complete_checked(&model, &prompt, 40, slot_for(AGENT_SLOT), Duration::from_secs(600), |_| {}).unwrap_or_else(|e| panic!("{label}: {e:?}"));
            eprintln!("MEASURE {label}: {:.1} s, read {} tokens (reused {}), on {}", started.elapsed().as_secs_f32(), reply.prompt_tokens_read, reply.prompt_tokens_cached, gpu_in_use().unwrap_or_else(|| "the processor".into()));
        };
        timed("slot cold read");
        match super::super::kv_slots::experiment_save_slot(&prompt) {
            Ok(a) => eprintln!("MEASURE slot saved: {} tokens, {} bytes, {} ms", a.tokens, a.bytes, a.ms),
            Err(e) => {
                eprintln!("MEASURE slot not saved: {e}");
                shutdown();
                return;
            }
        }
        shutdown();
        // The server is started again by a request that is not the prompt, so that its slot holds something else.
        complete(&model, "Say hi.", 1, Duration::from_secs(600), |_| {}).unwrap_or_else(|e| panic!("load: {e:?}"));
        match super::super::kv_slots::experiment_restore_slot(&prompt) {
            Ok(a) => eprintln!("MEASURE slot restored: {} tokens, {} bytes, {} ms", a.tokens, a.bytes, a.ms),
            Err(e) => eprintln!("MEASURE slot not restored: {e}"),
        }
        timed("slot read after a restore");
        shutdown();
    }
}

#[cfg(test)]
mod launch_settings_tests;

#[cfg(test)]
mod launch_golden_tests;

#[cfg(test)]
mod request_json_tests;

#[cfg(test)]
mod request_outcome_tests;

#[cfg(test)]
mod server_process_tests;

#[cfg(test)]
mod turn_queue_tests;

#[cfg(test)]
mod stream_tests;

#[cfg(test)]
mod complete_request_tests;
