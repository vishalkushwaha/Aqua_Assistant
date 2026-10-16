# Local language model (`llama.cpp`)

`LlamaRouterEngine` (`llama_engine.rs`) runs local GGUF models through llama.cpp, for chat and for the agent's planner. The name is older than the code: there is **no routing between models by intent** (chat, code, agent) and **no sliding window**; one model answers at a time, and a prompt that does not fit the model's window is shortened before it is sent (`prompt_fitted_to_context` in `modules/browser_agent.rs`).

---

## Which model

* **Chat** uses the active model: the one chosen in the app (`llm_active_gguf` setting), else the default file `data/models/llm/qwen-0.5b-instruct-q4_k_m.gguf` (`MODEL_FILENAME`).
* **The agent's planner** prefers `Qwen3-8B-Q4_K_M.gguf` when it is on disk, then any Qwen3 8B file, else the active model. `AQUA_AGENT_MODEL=<file name>` picks another one (`prefer_agent_model_filename`). That is the rule for an agent nobody chose a model for: the **Use** button, and `PUT /api/v1/models/roles`, record a choice in `data/models/router.json` that wins over it (`model_filename_for`; `AQUA_AGENT_MODEL` still wins over a choice). See `documentation/MODEL_PROFILES.md`.
* With no GGUF file or no llama.cpp program, the engine answers from a **reflex fallback** with the same API shape, so the rest of Aqua still starts (see `data/models/llm/README.md`).

---

## How it runs

* **A persistent `llama-server`** (`llama_server.rs`, and the folder `llama_server/` that holds its parts) is the normal way. It is started on first use, never at boot, keeps the model loaded and keeps the cache of the prompt it already read, so each planning step reads only what changed. It listens on 127.0.0.1 on a random port with a random key, runs in a Job Object that dies with Aqua, and stops after `AQUA_LLAMA_IDLE_SECS` (default 7200) without a request. `AQUA_LLAMA_SERVER=0` turns it off.
* **The parts of `llama_server.rs`** are in `llama_server/`, cut by what each does (move only: the root keeps the documentation, the imports and the re-exports, so `llama_server::X` is where it was): `request_outcome.rs` (what a request ends in: `Failure`, `Reply`, `Timings`, and the sentences of a request that was refused, was busy, gave the model up or went quiet), `request_json.rs` (the JSON of a request, the cache slots, the constraint on the reply), `launch_settings.rs` (the window, the layers on the card, the threads, the slots, the program, the port, the key), `server_process.rs` (the one running server, its lock, the idle reaper, shutdown), `server_start.rs` (the launch on the card or the processor and the health check), `turn_queue.rs` (the wait for the model's turn and the yield to a person), `http_calls.rs` (the plain and the streamed POST and the watch on a quiet stream) and `complete_request.rs` (the `complete_*` calls). The tests are the `*_tests.rs` files beside them.
* **`llama-cli`** (`llama_cli.rs`) is the fallback: one process per request, which reloads the model each time. It is used when the server cannot start or fails.
* **The window.** The server is started with `AQUA_LLAMA_CTX` tokens in total (default 6144, at least 2048, at most 32768) split over `AQUA_LLAMA_SLOTS` slots (default 1). `max_context_tokens()` is the size of one slot. 6144 is the default since card R6-20: on the 4 GB graphics card this was tuned on the planner was right 77% of the time at 6144 and 62% at 2048, for 140 MiB more video memory (progress doc 2l); `AQUA_LLAMA_CTX=2048` is the old setting. The layers on the card follow the window (24 at 2048, 22 at 6144, `default_gpu_layers`): with 24 at 6144 the card is full and prompt reading fell from 70 to 13 tokens a second, so the server gave up on the card. A cold read of the planner's real prompt is longer at 6144 because the prompt is no longer cut to fit (about 8 s, an estimate).
* **GPU.** `gpu.rs` decides whether the GPU build of the server is used (a card with too little free memory is left alone); the server itself fits how many layers go on the card to the free video memory (`-ngl auto`, with a margin for the desktop).

---

## Speed experiments (card OA9-06; all off, tried only against `aqua_fake_llama`)

* **The saved slot** (`kv_slots.rs`, `AQUA_LLAMA_SLOT_SAVE`): the slot of the agent's prompt is saved through the server's `/slots/N?action=save` when the server is stopped on purpose (`shutdown`) or goes idle, and put back through `restore` right after the next start, only for a file named for this model file and this prompt start inside `data/cache/kv`. The file is as private as the conversation; its name holds hashes, no text. Every failure is one line of the log and never delays or fails a request.
* **The second server** (`fast_server.rs`, `AQUA_FAST_TIER`, with `model/fast_route.rs` and `model/fast_tier.rs`): one extra `llama-server` for the owner's small `.gguf`, answering the roles decision and fast, with a fall back to the main model. Lazy, on the processor only (`AQUA_FAST_TIER_NGL`, 0), one slot, a small window (`AQUA_FAST_TIER_CTX`), loopback only, stopped when idle (`AQUA_FAST_TIER_IDLE_SECS`) and by `shutdown`, left alone for five minutes after a failure. Its state is in `GET /api/v1/models/roles` (`speed_experiments.fast_tier.state`).

Both are described, with what was and was not run, in [`documentation/SPEED_EXPERIMENTS.md`](../../../../documentation/SPEED_EXPERIMENTS.md).

---

## The model layer

The engine's five calls to the server (the decision router, streamed chat, the constrained reply of the planner and of chat, the two warm-ups) go through `crate::model::bridge`. With `AQUA_MODEL_ROUTER` off (the default) the bridge calls the function of `llama_server.rs` of the same name; with it `on` the call is described as a `ChatRequest` and goes through the model router to `LlamaServerProvider`, which calls the same `llama_server::complete_inner` with the body built by `llama_server::build_body`. The engine keeps choosing the file, writing the prompt, cleaning the reply and falling back to `llama-cli`. See [`documentation/MODEL_PROVIDER.md`](../../../../documentation/MODEL_PROVIDER.md).

---

## API gateway

`POST /api/v1/llama/generate` uses the shared engine in `AppState` (`core/src/api_gateway/server.rs`). Status: `llm_engine_ready`.

---

## Tests

`core/tests/llama_test.rs`, and the unit tests in `llama_engine.rs`, `llama_server/` (the `*_tests.rs` files) and `llama_cli.rs`. Measurements that load the 8B model (they take minutes) are marked `#[ignore]` and are run by hand.

---

## Related

* End-to-end voice pipeline: `core/src/pipeline.rs`
* What works and what does not: [`documentation/CAPABILITIES.md`](../../../../documentation/CAPABILITIES.md)
* Model inventory: [`documentation/API_INTERCONNECT_AND_MODELS.md`](../../../../documentation/API_INTERCONNECT_AND_MODELS.md)
