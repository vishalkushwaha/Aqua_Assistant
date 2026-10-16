# Runtime Data Directory (`data/`)

Local persistence, downloaded models, and generated artifacts for Project Aqua. Paths are relative to the **repository root** unless configured otherwise.

---

## Layout

```
data/
├── .aqua_api_token, .aqua_ui_token   # The two tokens (git-ignored; never print them)
├── aqua_core.db              # DatabaseManager: settings (the sealed voiceprint), task_history
├── solutions.db              # SolutionsDb: learned recipes + embeddings
├── agent_runs.db             # The record of every agent run (core/src/agent_runs/)
├── experience.db             # The Agent Experience system (core/src/memory/experience/)
├── user_memory.db            # What the person asked Aqua to remember
├── flows.db, workflows.db    # AI Flows and workflows, their runs and schedules
├── vault/vault.db            # The offline vault (off until a folder is added)
├── index/files.db            # The file index (adapters/mcp_adapters/src/file_index/)
├── tools/                    # Tool configuration (policy.json, fs.json, term.json, mcp.json, ...), the audit ledger, the undo store (trash/)
├── browser/                  # The browser policy, the Aqua-only profile, uploads, the audit trail
├── browser_mcp/              # State of the Browser MCP extension backend
├── learning/                 # Learned app-launch skills
├── devices/, orb/            # Device monitor and orb placement settings
├── engines/                  # llama.cpp builds fetched by scripts/fetch_llama_*.ps1
├── models/                   # GGUF/ONNX weights — see models/README.md
├── generated_images/         # Images that flows draw (gitignored)
├── cache/, logs/             # Caches; the model server's log
└── skills/, skill_proposals/ # The person's skills; skills Aqua proposed
```

Which files exist depends on what has been used: a folder appears the first time a feature writes to it. `secure_db/` (the vault of `core/src/crypto/`) is not created, because nothing calls that module.

---

## Git ignore rules

From [`.gitignore`](../.gitignore):

- `data/backups/`, `data/engines/`, `data/generated_images/`
- The databases (`data/*.db` and their `-wal`, `-shm` and `-journal` files, also in a data folder made in another directory) and the two token files
- `data/tools/*.jsonl` (the audit ledger) and `data/tools/artifacts/`
- Large model binaries under `data/models/` (`*.gguf`, `*.onnx`, `*.bin`, `*.pt`, `*.safetensors`), and the Laya cache
- `data/browser/*` except `policy.default.json`
- Staging `.aqua/` at repo root
- All other runtime state: every database under `data/` (`data/**/*.db`, the `-wal`, `-shm` and `-journal` files, `*.sqlite` and `*.sqlite3`), `data/vault/`, `data/index/`, `data/cache/`, `data/orb/`, `data/devices/`, `data/learning/`, `data/skill_proposals/`, `data/tool_output/`, `data/snapshots/`, `data/door_shadow.json`, the files that scripts fetch (`data/models/embedding/tokenizer.json`, `*.tar.gz` under `data/models/`, `data/models/deepfilternet/DeepFilterNet3_onnx/`), and at the repository root `logs/` and `founder/`

The vault database holds the person's documents and the file index lists their computer, so never stage `data/`; the rule is in `CLAUDE.md`, and the pre-commit hook (`scripts/install_hooks.ps1`) refuses such paths.

**Ignoring does not untrack.** A file that git already tracks stays tracked whatever `.gitignore` says. Commit 8d1be080 put runtime files on `main` (measured on 2026-10-09: 86 paths under `data/`, `logs/` and `founder/`, among them `data/vault/vault.db` and a 30 MB `data/index/files.db` with its `-wal` and `-shm`). `python scripts/check_tracked_data.py` lists them (`--list` prints just the paths); `scripts/tracked_data.allow` is the short list that may stay (this file, the model READMEs, `.gitkeep` files, `data/browser/policy.default.json`, `data/models/embedding/config.json`). The `tracked-data` step of `scripts/ci.ps1` only reports until the owner has run `git rm --cached -- <paths>` in their own clone (the working files stay on disk), after which `-StrictData` can make it fail the gate. The history still holds the files: anything secret in them must be revoked, and a history rewrite is the owner's decision.

---

## Schema reference

| Database | Manager | Tables / content |
| :--- | :--- | :--- |
| `aqua_core.db` | `core/src/storage/db.rs` | `settings(key, value, updated_at)`, `task_history(task_id, prompt, status, created_at)` — WAL enabled |
| `solutions.db` | `core/src/memory/solutions_db.rs` | `solutions(id, intent_pattern, payload_recipe_json, vector_embedding_blob, execution_count, success_rate, last_executed_timestamp)` — WAL enabled |

Full API/storage narrative: [`documentation/API_INTERCONNECT_AND_MODELS.md`](../documentation/API_INTERCONNECT_AND_MODELS.md) §7.

---

## Test fixtures

Integration tests may use:

- `core/target/test_models/**/model_manifest.json`
- Sample CSV under `core/target/test_sales_*.csv`

These are for **tests only**, not production data paths.

---

## Related docs

- Model downloads: [`models/README.md`](models/README.md)
- Getting started: [`documentation/GETTING_STARTED.md`](../documentation/GETTING_STARTED.md)
