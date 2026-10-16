# Aqua Core (`aqua-core`)

Rust coordinator library and the daemon binary for Project Aqua: the voice pipeline, the HTTP API on `127.0.0.1:8767`, the agent loop, flows and workflows, memory and storage. Every file and its role is in [`documentation/SOURCE_INVENTORY.md`](../documentation/SOURCE_INVENTORY.md).

---

## Crate layout

```
core/
├── src/
│   ├── lib.rs              # Module declarations; the `learning` module is included from ../../learning/
│   ├── main.rs             # The program: takes the port, starts engines, tool host, gateway, watchdogs
│   ├── pipeline.rs         # The voice and text turn — doc: documentation/VOICE_PIPELINE.md
│   ├── api_gateway/        # Axum REST + WebSocket — see api_gateway/README.md; routes in documentation/openapi.json
│   ├── modules/            # The agent loop (browser_agent.rs), the planner, the model server (llm_router/), context, reader
│   ├── laya_validator/     # The gate before the model: refuse, chat or agent
│   ├── memory/             # solutions.db recipes, the experience system, remembered facts, embeddings — see memory/README.md
│   ├── agent_runs/         # The durable record of every agent run, its trace, resume
│   ├── flows/, workflows/  # AI Flows and workflow automation
│   ├── vault/, dictation/  # The offline vault; dictation by rules
│   ├── storage/            # aqua_core.db, the sealed voiceprint — see storage/README.md
│   ├── reflection/         # Crash reports and repair hints — see reflection/README.md
│   ├── startup_health/     # Start-up checks; bind-first start-up (off by default)
│   ├── crypto/             # Key rotation — written, not wired — see crypto/README.md
│   ├── hal/                # Session watcher — written, not wired — see hal/README.md
│   ├── career/             # Career intelligence — written, not wired — see career/README.md
│   ├── (many single files) # doctor, audit_key, promoted, skills, hooks, reminders, queue, sources, ...
│   └── learning → ../../learning/mod.rs
└── tests/                  # Integration tests (see documentation/TESTING.md)
```

---

## Feature modules (`src/modules/`)

The modules that run: `browser_agent` (the loop), `planner`, `context`, `reader`, `decision`, `stuck`, `code_verification`, `agent_experience`, `practice_runner` and `llm_router` (the model server; documented in [`src/modules/llm_router/README.md`](src/modules/llm_router/README.md)).

These have a README and are **written, not wired**: nothing outside their own tests calls them ([`documentation/competitive/refresh/10_DEAD_CODE_CALLERS.md`](../documentation/competitive/refresh/10_DEAD_CODE_CALLERS.md)). Their READMEs describe a design larger than the code.

| Module | README |
| :--- | :--- |
| `backup_manager` | [`src/modules/backup_manager/README.md`](src/modules/backup_manager/README.md) |
| `code_generator` | [`src/modules/code_generator/README.md`](src/modules/code_generator/README.md) |
| `dependency_integrator` | [`src/modules/dependency_integrator/README.md`](src/modules/dependency_integrator/README.md) |
| `document_reader` | [`src/modules/document_reader/README.md`](src/modules/document_reader/README.md) |
| `image_generator` | [`src/modules/image_generator/README.md`](src/modules/image_generator/README.md) |
| `scheduler` | [`src/modules/scheduler/README.md`](src/modules/scheduler/README.md) |
| `updater` | [`src/modules/updater/README.md`](src/modules/updater/README.md) |

Also written, not wired and without a README: `file_organizer/` (see [`documentation/IMPLEMENTATION_STATUS.md`](../documentation/IMPLEMENTATION_STATUS.md)).

---

## Public API surface

- `AquaEndToEndPipeline::new(models_base_dir)` — voice stack
- `aqua_core::version()` — crate version
- `api_gateway::run_api_gateway(port, state)` — HTTP server (blocks until shutdown)
- `doctor::run`, `audit_key`, `promoted`, `vault::cli` — used by `aqua-cli` ([`apps/aqua-cli/README.md`](../apps/aqua-cli/README.md))
- The full list of public modules is `src/lib.rs`; signatures of the older ones are in [`documentation/API_REFERENCE.md`](../documentation/API_REFERENCE.md)

---

## Build & test

Build and test one crate at a time: the test executables of this crate are large (the lib test alone is about 1.2 GB), and the disk is small. Never rebuild over the Aqua you are running.

```bash
cargo build -p aqua-core
cargo test -p aqua-core --lib
cargo test -p aqua-core --tests
cargo run -p aqua-core        # only with the port 8767 free, or AQUA_API_PORT=8790 and a data folder of its own
```

Dependencies: `aqua-audio-adapter`, `aqua-tool-host`, `aqua-native-tools`, `aqua-mcp-adapters`, `aqua-vision-adapter`, `aqua-device-monitor`, `axum`, `rusqlite`, `tokio` (see `Cargo.toml` and [`documentation/DEPENDENCIES.md`](../documentation/DEPENDENCIES.md)).

---

## HAL

See [`src/hal/README.md`](src/hal/README.md) for the session watcher and planned autostart/audio HAL files. The module is written, not wired.
