# API Gateway (Axum REST + WebSocket)

HTTP surface for remote task dispatch, LLM playground APIs, and UI streaming hooks.

---

## Entry points

| Symbol | File | Role |
| :--- | :--- | :--- |
| `run_api_gateway(port, state)` | `server/router.rs` (re-exported by `server.rs`) | Binds `127.0.0.1:{port}` and runs until Ctrl+C |
| Binary | `core/src/main.rs` | Initializes DB + `AppState`, then `run_api_gateway(8767, …)` |

---

## Routes (implemented)

The table below is the original set of eleven, kept as a guide to the pipeline routes. The router in `server/router.rs` (`api_routes`), with the routers it merges (the families of `server/`, each a `routes()` next to its handlers: `status_api.rs`, `pipeline_api.rs`, `stream_api.rs`, `turn_control_api.rs`, `listen_api.rs`, `speech_api.rs`, `voice_settings_api.rs`, `llm_api.rs`, `mcp_api.rs`, `tools_api.rs`, `browser_api.rs`, `approvals_api.rs`, `agent_api.rs`, `agent_stream_api.rs`, `index_api.rs`, `ws.rs`; and `flow_api.rs`, `flow_sched_api.rs`, `flow_comments_api.rs`, `workflow_api.rs`, `learning_api.rs`, `devices_api.rs`, `route_api.rs`, `run_api.rs` (the events of a run), `routing_feedback_api.rs`), registers about **170 routes**: tools (`/tools`, `/tools/call`, `/tools/audit/*`, `/tools/trash/purge`), the agent (`/mcp/agent` and its stream), the browser and desktop (`/browser/*`, `/windows-mcp/*`), flows, workflows, learning, voice settings and more. `POST /api/v1/route/explain` says where a request would go without running a model, and `GET /api/v1/runs/{id}/trace` is a stub that answers an empty list. The complete list is `documentation/openapi.json` (generated from the router's source by a test, with the schemas of the bodies of the routes a client uses first); the error codes, `GET /api/v1/version` and the generated TypeScript client are described in `documentation/ERROR_CODES.md` (`contract.rs`, `contract_schemas.rs`). [`documentation/TOOLS_REFERENCE.md`](../../../documentation/TOOLS_REFERENCE.md) covers the tools, [`documentation/API_INTERCONNECT_AND_MODELS.md`](../../../documentation/API_INTERCONNECT_AND_MODELS.md) the pipeline routes (it predates most of the others), and the router code is the source: `grep -n "route(" core/src/api_gateway/*.rs`.

| Method | Path | Handler |
| :--- | :--- | :--- |
| `GET` | `/api/v1/status` | `get_status` |
| `POST` | `/api/v1/task/dispatch` | `dispatch_task` |
| `POST` | `/api/v1/llama/generate` | `llama_generate_endpoint` |
| `POST` | `/api/v1/kokoro/synthesize` | `kokoro_synthesize_endpoint` |
| `GET` | `/api/v1/pipeline/health` | `pipeline_health_endpoint` |
| `POST` | `/api/v1/pipeline/text` | `pipeline_text_endpoint` (Llama → Kokoro) |
| `POST` | `/api/v1/pipeline/voice-frame` | `pipeline_voice_frame_endpoint` |
| `POST` | `/api/v1/pipeline/wav` | `pipeline_wav_endpoint` (WAV base64) |
| `POST` | `/api/v1/pipeline/text/stream` | `pipeline_text_stream_endpoint` (server-sent events: `speech` pieces, `done`, `error`) |
| `POST` | `/api/v1/pipeline/wav/stream` | `pipeline_wav_stream_endpoint` (the same, with `heard` first) |
| `GET` | `/ws` | WebSocket upgrade → `CONNECTED` JSON |

Full request/response shapes: [`documentation/API_INTERCONNECT_AND_MODELS.md`](../../../documentation/API_INTERCONNECT_AND_MODELS.md) §3.0–§4.

The five `pipeline` routes that run a turn take part in the person's conversation (the last few turns, "the other one", a question the agent is waiting on and its `question` field in the answer) **only with the UI token**. A turn made with the API token runs as the first turn of an empty conversation: it sees none of what the person said, adds nothing to it, and can neither answer nor clear a waiting question (`conversation::with_client`).

---

## Production hardening (planned)

- TLS on separate port (`8766`)
- HMAC bearer authentication
- `POST /api/v1/security/approve` for step-up auth

See [`documentation/IMPLEMENTATION_STATUS.md`](../../../documentation/IMPLEMENTATION_STATUS.md) §7.

---

## Dependencies

- `axum` with `ws` feature
- Invokes `LlamaRouterEngine` directly for `/llama/generate`

---

## Lifecycle

The binary blocks on `axum::serve` with graceful shutdown on Ctrl+C. Run from the repository root so `data/` and `data/models/` paths resolve correctly.
