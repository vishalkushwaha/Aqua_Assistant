---
summary: "Every hard-coded default in first-party code (ports, URLs, paths, thresholds) plus a generated table of the AQUA_* environment variables."
read_when:
  - "looking for a port, path, threshold or default value"
  - "adding or changing an AQUA_* environment variable"
  - "finding where data or staging files live"
---

# Configuration, Constants & Runtime Paths

Every **hard-coded default** in first-party Aqua code (non-vendor). The code also reads over a hundred `AQUA_*` **environment variables** (ports, model files, switches such as `AQUA_LAYA`, `AQUA_ROUTER_V2`, `AQUA_LLAMA_CTX`); the last section of this page lists them, generated from the source. Paths are relative to the process current working directory (usually repo root) unless `AQUA_WORKSPACE_ROOT` says otherwise.

---

## 1. Network ports & URLs

| Service | Value | Source |
| :--- | :--- | :--- |
| API gateway (HTTP) | `127.0.0.1:8767` | `core/src/main.rs`, `api_gateway/server.rs` |
| REST base | `http://127.0.0.1:8767/api/v1/` | Axum routes |
| WebSocket | `ws://127.0.0.1:8767/ws` | Same Axum app |
| Vite dev server | `http://localhost:1420` | `apps/aqua-desktop/vite.config.ts`, `tauri.conf.json` |

The older table rows for MCP IPC, Android ADB, Browser DevTools, an IDE MCP socket, SSH and MQTT endpoints named a file (`mcp_suite.rs`) that is gone and addresses that nothing in the code uses; they were removed on 2026-10-08. The MCP servers Aqua really starts are described in [`BROWSER.md`](BROWSER.md) and [`TOOLS_CONFIG.md`](TOOLS_CONFIG.md).

**Production targets (not bound in code):** TLS `8766`, HMAC auth — see [`SECURITY_AND_PRIVACY.md`](SECURITY_AND_PRIVACY.md).

---

## 2. RAM & SLA display constants

| Constant | Value | Where |
| :--- | :--- | :--- |
| Active RAM SLA (UI/docs) | none: a 200 MB target was never measured and is not kept (the local model needs about 5 GB); `get_status` sends no `ram_sla_mb` and the UI shows "not measured" | `AdminHeader`, API `get_status` |

---

## 3. Voice & biometrics thresholds

| Parameter | Value | Source |
| :--- | :--- | :--- |
| Voice fingerprint size | 256 (learned from the model at start) | `biometrics_ecapa.rs` |
| Master voice match threshold | **0.55** (cosine; `AQUA_VOICE_MATCH_THRESHOLD` overrides, 0.20 to 0.95) | `VoiceBiometricsEngine::match_threshold` |
| Adaptive drift update threshold | **0.90** | `drift_update_threshold` |
| DeepFilterNet attenuation limit | **15.0 dB** | `NoiseSuppressor` |
| Kokoro TTS sample rate | **22050 Hz** | `KokoroTtsEngine` |
| Whisper language default | `en` | `WhisperSttEngine` |
| End-to-end test frame | 16000 samples | `end_to_end_test.rs` |

---

## 4. Career & NLI

| Parameter | Value | Source |
| :--- | :--- | :--- |
| Zero-fabrication entailment threshold | **≥ 0.92** | Architecture + `career_engine.rs` (fails at 0.65 when skills missing) |
| Audit ledger path | `.aqua/career/audit.ledger` | `CareerEngine::new` |

---

## 5. Solution memory & RL

| Parameter | Value | Source |
| :--- | :--- | :--- |
| Fast-path eligibility (similarity) | **≥ 0.92** | `solutions_db.rs` `SearchResult` |
| RL reward clamp | **[-1.0, 1.0]** | `rl_engine.rs` |
| Policy weight update step | **0.05 × reward** | `rl_engine.rs` |
| Low-confidence purge threshold | **< 0.30** (design in learning README) | `rl_engine.rs` comments |
| Trajectories file | `learning/experience/trajectories.json` | Created at runtime by `RlEngine` |
| Policy weights file | `learning/experience/policy_weights.json` | Same |

---

## 6. Social watchdog defaults

| Field | Default | Source |
| :--- | :--- | :--- |
| `check_interval_secs` | **600** (10 min) | `SocialWatchdogConfig::default` |
| `profile_dir` | `.aqua/browser_profile` | Same |
| `auto_login_enabled` | `true` | Same |
| `captcha_solver_enabled` | `true` | Same |

---

## 7. Model file paths (must match code)

Base for voice pipeline: **`data/models/`** (`AquaEndToEndPipeline::new`).

| Engine | Expected path (relative to model subdir) | Notes |
| :--- | :--- | :--- |
| DeepFilterNet | `deepfilternet/DeepFilterNet3_onnx/` | **Directory** must exist (`noise_cancellation.rs`) |
| Whisper | `whisper/ggml-medium-en.bin` | File |
| LLM | `llm/qwen-0.5b-instruct-q4_k_m.gguf` | File |
| Kokoro | `kokoro/kokoro-v1.0.onnx` + `kokoro/voices-v1.0.bin` | File + CLI |
| Speaker model (voice lock) | `biometrics/speaker_wespeaker_resnet34_lm.onnx` | File (`scripts/fetch_speaker_model.ps1`) |
| Vision OCR | `{model_dir}/pp_ocr.onnx` | `VisionEngine::new(model_dir)` — tests use **`data/models/vision/`** |
| YOLO | `{model_dir}/yolov8_nano.onnx` | Same directory as OCR |

**Note:** nothing in the program calls `VisionEngine::new` (the vision adapter is written, not wired: see [`KNOWN_LIMITS.md`](KNOWN_LIMITS.md)); its tests use `data/models/vision` ([`vision_adapter/README.md`](../adapters/vision_adapter/README.md)).

Manifests for CI metadata: `core/target/test_models/*/model_manifest.json` — see [`TEST_FIXTURES.md`](TEST_FIXTURES.md).

---

## 8. Staging & local state (`.aqua/`)

| Path | Writer | Purpose |
| :--- | :--- | :--- |
| `.aqua/staging/dynamic_imports/` | `AutoIntegrator` | Dependency integrator sandbox |
| `.aqua/browser_profile/` | `SocialWatchdog` | Playwright profile (design) |
| `.aqua/career/audit.ledger` | `CareerEngine` | Signed application audit |
| `.aqua/crash_reports/{task_id}.json` | `ReflectionEngine` | Crash + repair strategy |

Entire `.aqua/` is **gitignored**.

---

## 9. SQLite files

| File | Extra tables beyond `storage/db.rs` |
| :--- | :--- |
| `data/aqua_core.db` | `scheduled_tasks` added by `SchedulerEngine` |
| `data/solutions.db` | `solutions` table — see [`data/README.md`](../data/README.md) |

---

## 10. Desktop / Tauri

| Key | Value |
| :--- | :--- |
| Product name | Aqua Assistant |
| App ID | `com.viskush.aqua` |
| Window size | 1200 × 800 |
| Frontend dist | `apps/aqua-desktop/dist` (Tauri build) |
| CSP | `default-src 'self'; …` | `tauri.conf.json` |

Tauri **Rust backend** (`src-tauri/src`) is not in the repository yet — UI runs via `npm run dev` only.

---

## 11. Workspace metadata

| Field | Value |
| :--- | :--- |
| Version | `0.1.0` |
| Edition | 2021 |
| License | MIT (workspace `Cargo.toml`) |
| Authors | VisKush |
| Repository | `https://gitlab.com/VisKush/aqua_assistant.git` |

No root `LICENSE` file in tree; MIT is declared in crate manifests.

---

## 12. Git ignore (operational)

See [`.gitignore`](../.gitignore): `/target`, `node_modules/`, `dist/`, `.aqua/`, model binaries under `data/models/`, IDE folders.

---

## 12a. The log and the background jobs (card OA3-04)

The core builds its log in one place (`core/src/logging.rs`). Three variables decide how, and `aqua-cli logs` reads it back.

| Variable | Default | What it does |
| :--- | :--- | :--- |
| `RUST_LOG` | the core's own crates and the tool host at `info`, everything else at `warn` (`logging::DEFAULT_FILTER`) | The usual `tracing` filter; when it is set (and is a filter) it replaces the default, it does not add to it. A mistake in it falls back to the default. |
| `AQUA_LOG_FORMAT` | `text` | `json` writes one JSON object per line: `ts`, `level`, `target`, `message`, `fields`, `spans` (each span the line was written in, from the outermost, with its fields) and `run_id`, `call_id` and `request_id` at the top when a span or the event has one. Any other value is text. |
| `AQUA_LOG_FILE` | not set | A path. The core then writes its log to that file itself, rotated at 5 MB with at most five files (`<name>.1` is the newest of the old ones), and also to the console when the output is a terminal. Without it the core prints to the standard output, which the desktop app catches into `logs/core.log` (and starts again at 5 MB). The app does not set this variable. A file that cannot be opened is said once and the console is used; lines lost while another program holds the file are counted and reported when writing works again. |

What is in a line: the text format is what it was, with the spans in front of the message: `run{run_id="run_..." route="agent" caller="api"}:tool{tool="fs.read" call_id="req_..."}: ... tool call ended ok=true decision="auto" ms=12`. A request has `request{request_id, method, path}` (never the query; the health probe has none). A tool call's closing line has the outcome and the error code and never the arguments or the result.

`aqua-cli logs [--run ID] [--level L] [--tail N] [--json] [--file PATH]` reads `AQUA_LOG_FILE`, else `logs/core.log` of the workspace, with the older files beside it: `--run` keeps the lines of one run (JSON lines that carry the id in a span or at the top, text lines that name it as a whole word), `--level` the lines at that level and above, `--tail` the last N (200 by default), `--json` prints each as a JSON object. It does not talk to the running core.

The background jobs have no setting. `GET /api/v1/jobs` lists them ([`API_REFERENCE.md`](API_REFERENCE.md)); when Aqua ends, the jobs are told to stop and waited for together for five seconds (one second when the desktop app has gone), and the ones still going are named in the log.

---

## Related

- Full public APIs: [`API_REFERENCE.md`](API_REFERENCE.md)
- Glossary: [`GLOSSARY.md`](GLOSSARY.md)

<!-- BEGIN GENERATED ENV -->

## 13. Environment variables (generated)

### The typed flags

Generated from the flag registry (`core/src/flags.rs`), where these switches are read in one place (`GET /api/v1/flags` lists them with what this process uses). Environment variables and signed promotions are applied at startup. Admin-panel overrides are typed, persisted in `data/config/settings.json`, and become active after the next restart.

| Variable | Kind | Default | Values | What it does |
|---|---|---|---|---|
| `AQUA_TOOL_SEARCH` | switch | `off` | on, 1, true turn it on | Shows the agent the tools.search tool, which finds a tool by what you want to do. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_DOOR` | choice | `on` | on (on, 1, true); shadow (shadow); off (off, 0, false) | Whether browser and desktop calls go through the tool host: on is the default, shadow is an explicit compatibility comparison, and off uses the legacy route without comparison. AQUA_ONE_DOOR=off also uses that compatibility route. |
| `AQUA_ONE_DOOR` | choice | not set | off (off, 0, false); on (on, 1, true) | The older name of AQUA_DOOR. Only off is read (it selects the legacy compatibility route). |
| `AQUA_ROUTER_V2` | switch | `on` | 0, false, off turn it off (exactly as written) | The second generation of the router (which tools a goal is shown). Set 0, false or off to bring back the earlier rules. |
| `AQUA_RESEARCH` | switch | `off` | on, 1, true, yes turn it on | Deep research: the research.run tool exists only when this is on. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_SKILL_PROPOSALS` | switch | `off` | on, 1, true, yes turn it on | Drafts a skill by itself after a run that went well. Nothing becomes a skill until you have read and approved it. |
| `AQUA_CLIPBOARD_TOOLS` | switch | `off` | on, 1, true, yes turn it on | Offers the clipboard.read and clipboard.write tools. clipboard.read always asks. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_SCREEN_TOOLS` | switch | `on` | 0, off, false, no turn it off | Offers the tools that read the text on the screen. Set 0, off, false or no to leave them out. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_RECIPES_V2` | switch | `off` | on, 1, true turn it on | Recipes version 2: how a learned recipe is made, kept and replayed, with placeholders and postconditions. |
| `AQUA_PLAN` | switch | `off` | on, 1, true turn it on | The agent writes a short plan before a goal with two steps or more. |
| `AQUA_READER` | switch | `off` | on, 1, true turn it on | A reader that takes facts, each with its quote, from a page before the agent answers. |
| `AQUA_TOOL_OUTPUT_SPILL` | switch | `on` | 0, off, false, no turn it off | Keeps the whole text of a tool result that is cut for the model, with its secrets masked, in the folder tool-output next to the data folder for a week, so the agent can search it with fs.grep. Set 0, off, false or no to keep nothing. |
| `AQUA_MODEL_ROUTER` | switch | `off` | on, 1, true turn it on | Sends the calls to the local language model through the model router (core/src/model) instead of straight to the server. The request and the answer are the same either way; this is for trying the router before it becomes the default. |
| `AQUA_REMOTE_MODEL` | switch | `off` | on, 1, true turn it on | Allows the optional remote-model provider to be constructed at startup. Its settings must also enable it, and every use still asks in the app. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_AGENT_TOOLS` | switch | `off` | on, 1, true turn it on | Offers a coding run the two tools its loop handles itself: todo.write (a checklist that is shown again every round) and agent.explore (one read-only search through the host, at most eight a run). Off until a benchmark on the model shows they help; with it off the coding prompt does not mention them. |
| `AQUA_CODING_PACKS` | switch | `off` | on, 1, true turn it on | Shows a coding goal up to two tools of one more pack when its words call for one (database, docker, kubernetes, monitoring, forge or web), so at most 14 tools instead of 12. Off until a benchmark on the model shows the longer list does not cost accuracy. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_PREFIX_STABLE` | switch | `off` | on, 1, true turn it on | Keeps the start of a coding goal's prompt the same from round to round and from goal to goal, so the model server reads less of it again: the coding record (what was read, changed and failed) moves to the end of the prompt, the rules use <file> and <folder> instead of the paths of the goal, and the goal's file, folder and check follow the list of tools. Off until the saving and the effect on coding goals have been measured. |
| `AQUA_PROJECT_PACK` | switch | `off` | on, 1, true turn it on | Looks for a .aqua folder in the project a goal names (skills, commands, agents, hooks, MCP servers, formatters, tools) and uses each kind of it only after you have trusted that kind by the hash of its files; a file that changes after that is untrusted again. Nothing in the folder is read, listed or run before you trust it. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_SKILLS_V2` | switch | `off` | on, 1, true turn it on | Offers skill.file, which lists and reads the other files in a skill's folder (a template, a checklist): files directly in the folder, text, 16 KiB at most, never a link or a path. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_LAST_STEP_FINAL` | switch | `off` | on, 1, true turn it on | On the last round the run's budget allows (a budget of two rounds or more), the reply is held to a final answer (a question to you stays possible where asking is on) and the prompt says to answer from what the run has and to say what is not done, so a run that used all its rounds ends with the model's own words and not a template; it is still recorded as having stopped at max_steps. Off until it has been tried on the model. |
| `AQUA_FS_OUTLINE` | switch | `off` | on, 1, true turn it on | Offers fs.outline, which lists the declarations of a source file (functions, structs, classes) with the lines each one spans, so that a long file can be read by the part that is needed. Read through the same folder checks as fs.read. Off until the rounds a coding task needs have been measured with it on; with it off the tool does not exist and the coding prompt does not mention it. |
| `AQUA_COLLAPSE_REPEATS` | switch | `off` | on, 1, true turn it on | Shows a tool result that a later one repeats, word for word, as one line that says which later result it is (the same file read again, the same check run again), so the prompt keeps the room for results that say something new. An error is never collapsed. Off until the rounds a coding task needs have been measured with it on. |
| `AQUA_MODEL_PROFILES` | switch | `off` | on, 1, true turn it on | Speaks to each language model as its family and its profile say (core/src/model/adapter.rs, data/models/profiles.json, data/models/router.json): a Qwen3 file gets /no_think and the thinking switch exactly as before, any other family gets neither, the thinking blocks are taken out of its reply by the rules of its family, and the sampling of its profile and of the role is sent. Off: every model is spoken to as Qwen3 is, with temperature 0.3 and top_p 0.9. |
| `AQUA_SAMPLING_PRESETS` | switch | `off` | on, 1, true turn it on | Adds the sampling the model's card recommends to the chat and summary roles of a Qwen3 model (temperature 0.7, top_p 0.8, top_k 20, from the published card and not measured here), under what the profile and the role set. Off until the bake-off has been run with it. |
| `AQUA_SEED` | number | `-1` | a whole number from -1 to 9223372036854775807 | A whole number from 0 up that is sent as the seed of every request to the local model, so that an evaluation can be run again with the same words. -1 or not set: no seed, and the model chooses by chance as it always has. |
| `AQUA_STREAM_IDLE_SECS` | number | `0` | a whole number from 0 to 3600 | How many seconds a streamed answer of the local model may go quiet, once its first piece has come, before the call is hung up on and ends with the message that the model went quiet (the model server is asked whether it is well, and keeps its model when it is). 0 is off, and is what it is until it has been tried with the model; 20 is the value the audit suggests. It does not apply to a call that is not streamed, which gets nothing until the whole answer is ready, and it does not run while the model is still reading the prompt. |
| `AQUA_STREAM_FIRST_EVENT_SECS` | number | `0` | a whole number from 0 to 3600 | How many seconds a streamed call may wait for the model to begin to answer (it is reading the prompt first: about 8 seconds for a cold one, longer for a long one on the processor) before the call is hung up on with the same message. 0 is off, and is what it is until it has been tried with the model. The call's own time limit (AQUA_LLAMA_TIMEOUT_SECS) always applies. |
| `AQUA_CHECK_TOOLS` | switch | `off` | on, 1, true turn it on | Offers project.profile (what the project is and the command that tests, builds, lints and type-checks it) and check.run (runs one of those commands through the terminal tools and answers with a short list of what failed and where) to a coding run, in place of the model choosing a command and a folder by itself. check.run asks as the same term.run would. Off until the rounds a coding task needs have been measured with it on; with it off neither tool is on offer (they are listed as unavailable) and the coding prompt does not mention them. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_NESTED_NOTES` | switch | `off` | on, 1, true turn it on | Attaches the note (AQUA.md, AGENTS.md or CLAUDE.md) of the folder a coding run reads a file from, once a run, to the result of the read, when you trusted that folder and the note is as it was when you did, and cuts a long project note at its headings and lines (the build, test and rule sections first) instead of at 2,048 characters wherever they fall. Off until it has been measured with the coding tasks. |
| `AQUA_DENY_FEEDBACK` | switch | `off` | on, 1, true turn it on | A "no" to a question can come with words (POST /api/v1/browser/confirm with decision deny and a reason, up to 300 characters). With this on, a coding run is shown the words as data from you ("They said: ...") and goes on instead of stopping, at most twice a run; a run that is not a coding run, a third "no" and a "no" with no words stop the run as they always did. The words never approve anything. Off until the app has a place to type them: then the words are not kept and every "no" stops the run. |
| `AQUA_STEER_CUES` | switch | `off` | on, 1, true turn it on | A typed request that starts like a continuation while a task you are waiting for is going on is added to that task ("also add tests", "wait, use b.rs instead") or kept to run after it ("then run clippy", "when you're done, ..."), instead of waiting its turn as a new request. Never for a stop, never for a yes or a no (nothing said this way answers a question), and only with the UI token. Off: such a sentence is a new request, as it always was. |
| `AQUA_ATTENTION` | switch | `off` | on, 1, true turn it on | Puts a short sentence in the queue of spoken announcements when a run is waiting for your yes ("I need your yes to edit main.rs"), has a question for you, or has ended ("Done: 3 files changed", "I stopped: I reached the limit of 200 tool calls"). The app's player says it when it asks for the next announcement and it is not busy; nothing is said by this flag alone if no player is running. Built from fixed sentences and the name of the file, never code or a path; never a yes by voice. Off: nothing is announced for a run. |
| `AQUA_COMMANDS` | switch | `off` | on, 1, true turn it on | Custom commands: a procedure you wrote once in data/commands/<name>.md (or in the commands of a repository whose .aqua you trusted) is listed by GET /api/v1/commands and started by name with a few words by POST /api/v1/commands/<name>/run. A line !`like this` in it is a step that is run through the tool host and asks like any command; the run starts with only the tools the command allows. Nothing is expanded or run until you start a command. |
| `AQUA_COMMAND_TOOLS` | switch | `off` | on, 1, true turn it on | Offers the command tools you declared: cmd.* from data/tools/custom.json and proj.* from the tools.json of a repository whose .aqua code you trusted. Each is a fixed program with typed blanks, run as the term.run call it stands for (the same rules, folder checks and question), never through a shell. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_ANSWER_EVIDENCE` | switch | `off` | on, 1, true turn it on | Ends the answer of a coding run with one sentence that Aqua writes from the checks it saw (cargo test passed, the last check failed, the code changed after it, no check was run), so that what the answer claims and what the run showed are side by side. Off: the answer is the model's words only, as it always was. |
| `AQUA_CODING_RECORD` | switch | `off` | on, 1, true turn it on | Builds the coding record that stands in front of the history of a coding run (what the goal is, what was read and changed, what the newest check said and which tests failed, whether the code changed since, what is in the way, what to do next) from what the checks said, so that it says to give the answer after a passing check and not to read the last error, and keeps it with the run (record of GET /api/v1/runs/{id}). Off: the record keeps the first error of the run for ever, as it always did. Turning it on changes the text of every coding prompt: write the golden transcripts of core/tests/fixtures/agent_scenarios again (python scripts/eval/record_scenario.py golden) when it becomes the default. |
| `AQUA_CHECK_STEPS` | switch | `off` | on, 1, true turn it on | Records the check the loop runs after an edit (python -m py_compile after a Python edit, cargo check or npm run typecheck with AQUA_CODING_CHECK=on) as a step of the run, a term.run marked automatic, so that the gate that asks for a passing check after the last change reads it and an answer that follows a passing check is not sent back to run one more. Off: the check is printed under the answer of the edit and is no step, as it always was. Turning it on changes the steps of a run: write the golden transcripts of core/tests/fixtures/agent_scenarios again (python scripts/eval/record_scenario.py golden) when it becomes the default. |
| `AQUA_SHORT_CHECK_OUTPUT` | switch | `off` | on, 1, true turn it on | Shows the model the output of the check that follows an edit (cargo check, npm run typecheck, python -m py_compile) as its exit code, at most eight of the compiler's complaints with their file and line, and how many more there were, instead of the whole text (the last lines of the traceback for a Python file that does not compile). The gate that asks for a passing check still reads the whole of it. Off: the model sees the whole text, as it always did. |
| `AQUA_SNAPSHOT` | switch | `off` | on, 1, true turn it on | Takes a checkpoint of the project folder of a coding run, when it is a trusted one (the workspace, or the project folder under full access), before the run's first command that may change files and again when the run ends, in a store of Aqua's own under data/snapshots (at most 512 MiB; a file over 2 MB, an ignored file, a secret-looking name and Aqua's own files are never kept). GET /api/v1/runs/{id}/diff then shows what the run changed, whatever changed it (the files each of the run's tools changed and the files a command changed), and Undo this run puts back what a command changed besides what the file tools changed, leaving every file you changed since. Off until it has been measured on the disk and in time: nothing is kept and a command's changes are not in the run diff. |
| `AQUA_CODING_SESSIONS` | switch | `off` | on, 1, true turn it on | Keeps a short record of what a coding run did (the goals, the names of the files it read and changed, what its last check said, its plan, a few notes of its own) per project, and shows it to the next coding run in the same project as one block of at most 600 characters of data, not instructions: 4 hours after the last run for a session found by its project, 24 hours for one you named with session_id. No file contents are kept. The run gets agent.note to write at most 12 notes of 160 characters. The records are deleted for good (the file is overwritten) after 30 days, when you delete one, and when you clear the conversation. Off: nothing is kept and every prompt is the same as before. Turning it on lengthens the system part of a coding prompt by up to 600 characters: run scripts/eval/speed_gate.py with Aqua closed before it becomes a default. |
| `AQUA_CONTINUE` | switch | `off` | on, 1, true turn it on | A coding run that is about to stop at its last round while the failing tests of its newest check have become fewer in the last three rounds is given 8 more rounds, once, and says so; a coding run that stops on its rounds or its time ends its answer with a Where I stopped line that POST /api/v1/runs/{id}/continue can go on from. A chain of runs is at most 48 rounds and 30 minutes. Off: a run ends at its round cap and the answer is the one it always was. Turning it on changes how long a run may take: measure the rounds of the coding tasks before it becomes a default. |
| `AQUA_SESSION_SUMMARY` | switch | `off` | on, 1, true turn it on | Experimental. When a coding session has been closed and Aqua has been idle for ten minutes, writes one short summary of it (the goal, the files changed, what the last check said, what is left) from the session record alone, with no model call and no file contents, and stores it as a project note that a later goal in the same project may be offered as an untrusted note. At most one summary per session. Off: no summary is written. |
| `AQUA_PREAPPROVALS` | switch | `off` | on, 1, true turn it on | Lets you, in the app (the UI token) or at a terminal (aqua-cli preapprove), allow ONE run to run the project's own checks (the tests, the build, the linter, the type check: only the command lines the terminal tools count as a project's checks) in ONE folder without asking each time, for a number of calls and a time you choose, and lets a run be started with that pre-approval (POST /api/v1/runs with preapproval_id, aqua-cli run --preapproval). It answers nothing else: every other command, every edit and everything on the network still asks, and a question that your hooks or your rules ask is still asked. Each use is written in the ledger as preapproved. Off: no pre-approval can be made, given to a run or used. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_EDIT_GRANT` | switch | `off` | on, 1, true turn it on | Lets you, in the app (the UI token), answer the question about the first edit of a coding run with "allow edits in this folder for this run" (POST /api/v1/browser/confirm with decision approve and scope run). Later edits of files in that folder and the folders inside it (fs.edit, fs.write and fs.patch that only change or create files) then go through with no question until the run ends or is stopped, and for at most two hours; the grant lives in memory only, is for that run and no other, is not given to a helper agent and cannot be widened. It never covers a delete, a move, a command, git, a secret file, a script or build file, or a name the file tools never write (.vscode, .cargo, .husky, .idea, .devcontainer...), and a question that your hooks, your rules or your autonomy level ask is still asked. It ends the moment the run reads anything from outside the project (a web page, a mail, a command's output, a file outside the project folder): every edit then asks again for the rest of the run. Reading files inside the project does not end it. Every grant, every edit it lets through and every end is written in the ledger. Off: no question offers the choice, no answer makes a grant, and every edit asks. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_CODE_TOOLS` | switch | `off` | on, 1, true turn it on | Offers code.outline (the symbols of one source file, each with the lines it spans and its first line, a method under its type), code.symbols (finds where a function, class or type is defined, by its name or some words of it, in the allowed folder, best match first), code.deps (what one file imports and which files import it, for Rust, TypeScript, JavaScript, Python and Go) and code.refs (the lines that use a name, tagged call, import, test or ref, by file) to a coding run, so that a long file is read by the part that is needed, a name is found without reading files and what a change reaches is seen before it is made. All four read through the same folder checks as fs.read and fs.grep, run nothing, and make the symbols and the imports from the shape of the lines (not a parser: Rust, Python, TypeScript, JavaScript and Go best, the rest by their common keywords). Off until the rounds a coding task needs have been measured with it on; with it off none of the four is on offer (they are listed as unavailable) and the coding prompt does not mention them. |
| `AQUA_REPO_MAP` | switch | `off` | on, 1, true turn it on | Offers code.map to a coding run: a map of the project in about 250 tokens (up to 1,000 when it asks), the files most worth looking at for a goal and the names in each, made from the import lines, the names of the symbols and the words of the goal, and the files the run says it works on. It is a guess from the shape of the lines, reads through the same folder checks as fs.read, runs nothing, and its file names and symbols are text from the project, shown as data. The map is a tool the run may call, and what it answers is put in no prompt: only with it on the tool is named in the coding prompt (one line), as every tool on offer is. Off until the rounds a coding task needs have been measured with it on; with it off code.map is not on offer (it is listed as unavailable) and the coding prompt does not mention it. It is separate from AQUA_CODE_TOOLS. |
| `AQUA_CODE_SPEECH` | switch | `off` | on, 1, true turn it on | Leaves code out of what is spoken: a fenced block (three or more backticks or tildes) and a unified diff written outside a fence are not read aloud, and "The code is on the screen." is said once in the place of the first. The written answer is not changed. Off: code is read aloud as it always was. Turning it on changes what is heard, not what the model is told. |
| `AQUA_PROJECT_TERMS` | switch | `off` | on, 1, true turn it on | Adds the words of the project you work in (file names, package and dependency names, the names the newest source files declare; never a secret-looking file, line or word; at most 200 words and 800 characters, your own words first) to the prompt that primes the speech model for a dictation clip. The voice path, whose prompt is the wake phrase, is not changed. Off until the effect on the words heard has been measured: nothing is added to the prompt. |
| `AQUA_CODING_VOICE` | switch | `off` | on, 1, true turn it on | Answers the spoken or typed phrases "read the diff", "next change", "previous change", "line by line" and "repeat" with no model, from the changes of the last run that changed files (the record of GET /api/v1/runs/{id}/diff, kept when AQUA_SNAPSHOT is on), and puts "show me the diff" and "undo the last run" on the screen as a card (GET /api/v1/coding/staged) that you press in the app: a voice never undoes anything and never approves anything. Only for your own client. Off: those phrases are requests like any other. |
| `AQUA_MODEL_PROVIDERS` | switch | `off` | on, 1, true turn it on | Reads data/models/providers.json, the list of model servers on this machine (an Ollama, an LM Studio, a vLLM, a llama-server of your own: http://127.0.0.1:port/v1 or localhost, in plain http, never another machine) that Aqua may ask, and lets GET /api/v1/models/providers show them and POST /api/v1/models/providers/test try one with a few words. Nothing in the agent, the chat or the flows asks such a server: the models Aqua runs are the ones it always ran. A server on another machine is read, shown with the reason and never asked. |
| `AQUA_SYNTAX_GATE` | choice | `off` | on (on, 1, true); warn (warn) | Looks at what fs.write (an overwrite), fs.edit and fs.patch leave behind. warn: a note after a change that leaves a file's brackets or quotes unbalanced (Rust, JavaScript, TypeScript, Go, C and C++, Java, C#, Kotlin, Python, TOML, CSS, JSON with comments) or breaks a JSON file; on: the same notes, and a change that turns a JSON file that parsed into one that does not is refused, nothing written, with the line. A file that was already broken is never blamed on the change, a text scan is a guess and only ever warns, and a file that is made or added to is not judged. Off: nothing is looked at, as it always was. |
| `AQUA_CODING_FORMAT` | switch | `off` | on, 1, true turn it on | Runs the project's own formatter on a file a coding run has just changed, when the project says it has one (rustfmt.toml for Rust, a ruff setting for Python, go.mod for Go, a Prettier or Biome setting with the program in node_modules for JavaScript and TypeScript), as a term.run through the tool host (so the terminal's rules ask as they ask for any program), once for each of the first three changed files, 20 seconds at most, before the check that follows the edit; says that the file was formatted and what its version is now. A formatter that is missing, refused or fails is told in one line and never fails the edit. Off: no formatter runs after an edit, as it always was. |
| `AQUA_DIAGNOSTICS` | switch | `off` | on, 1, true turn it on | Shows the output of the check that follows an edit (python -m py_compile after a Python edit, cargo check or npm run typecheck with AQUA_CODING_CHECK=on) as the errors it found, one block per file with the files the run changed first (ERROR [12:5] mismatched types (E0308)), at most 20 for a file and about 900 tokens in all, warnings left out, instead of the whole text; a check that failed in a way that has no error line, or that timed out, is shown as it printed. The gate that asks for a passing check still reads the whole of it. Off: the output is shown as it always was (or as AQUA_SHORT_CHECK_OUTPUT says). |
| `AQUA_AGENTS` | switch | `off` | on, 1, true turn it on | Agent definitions and helpers: the built-in agents (build, plan, explore, review) and the ones you wrote in data/agents/<name>.md (or a repository whose .aqua text you trusted ships in .aqua/agents) are listed by GET /api/v1/agents, a run can be asked to be one (agent in the request of POST /api/v1/mcp/agent, agent: in a command) and then has only the tools the agent names, its words as notes after the rules and its own limit of rounds and seconds, and a coding run is offered agent.task, which hands one question to a helper that works in a context of its own (at most 3 a run, one level deep, 6 rounds and 120 seconds by default), can only read and search, cannot ask anybody, and whose answer comes back as text from outside. A helper is served by the same one model, after the run has paused for it and never beside it; what that costs the run's prompt cache has not been measured on this build. Off until the rounds and the accuracy of the coding tasks have been measured with it: with it off nothing is listed, no agent can be named, the tool is not on offer and the coding prompt does not mention it. |
| `AQUA_MCP_REMOTE` | switch | `off` | on, 1, true turn it on | Lets a server in data/tools/mcp.json be a remote one ("url": "https://...") instead of a program on this computer. It is spoken to over HTTP through the tool host's own HTTP engine, so the allow-list of data/tools/http.json (the host must be on it, there is no question for an unlisted one), the guard against private addresses, no redirects and a cap of 16 MB on a response all apply; the headers it is sent are plain text or the name of a secret in the credential store (a header that carries a credential must be a secret); the server is trusted by its address and header names like any other, its tools ask every time and no answer is kept for the session. Not built: logging in with OAuth, and the stateless protocol of 2026-07-28 over HTTP. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_CONFIG_WATCH` | switch | `off` | on, 1, true turn it on | Looks every two seconds at data/hooks.json, data/tools/mcp.json and the SKILL.md files of data/skills, and reads one again when it changed, with the rules of POST /api/v1/config/reload: only what cannot loosen anything is applied while Aqua runs (a hook that was running and is gone, or changed, waits for a restart; a server that changed or is new is not trusted), and nothing is read again while a run is going on. |
| `AQUA_SHIPPED_SKILLS` | switch | `off` | on, 1, true turn it on | Offers the ten development skills that ship with Aqua (fix-failing-tests, debug-build, review-diff, write-tests, add-endpoint, db-migration, upgrade-deps, release-notes, incident-triage, init-project) next to the ones you wrote: a goal they fit lists them in the prompt (a name and one line each, marked built in) and skill.load reads them. Each one only narrows what the run may use, and none reaches the network or runs an installer; a skill of yours or of a trusted repository with the same name wins. Off until a benchmark on the model shows the lists cost no accuracy; with it off they are not in the prompt, skill.load does not know them, and aqua-cli doctor names them. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_WORKTREES` | switch | `off` | on, 1, true turn it on | Offers git.worktree_add, git.worktree_list and git.worktree_remove: a coding run can make a second working folder of a repository, on a branch of its own, under .aqua/worktrees inside the repository's own folder (a folder Aqua owns, with a .gitignore of its own, which must be in a folder Aqua may write), work there through the git tools (its repo alias), fs.* and the cwd of term.run, and leave your first folder alone. Making one always asks and says about how many files and megabytes it copies; removing one always asks, is by name, only for a worktree Aqua made and recorded (data/tools/git_worktrees.json), refuses one with changes unless force=true (changes to tracked files are kept in a backup reference, untracked files are lost), takes away a link or junction inside it on its own first, and never deletes a branch that is not merged. The worktree is not trusted: a commit there asks. Nothing is reset, cleaned, force-pushed or rewritten. Off: the tools are not on offer and nothing is read or made. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_GIT_REVIEW` | switch | `off` | on, 1, true turn it on | Offers git.conflicts (the files with merge conflicts, how each is conflicted, the hunks of each text file with what each side has, and whether a merge, cherry-pick, revert or rebase is in progress), git.resolve_conflict (replaces the hunks of one file by ours, theirs, both or text the model wrote: you are asked each time and shown the change as a diff, the file is kept in the undo store first, nothing is staged or committed, and a file that changed after you were shown the diff is not written) and git.pr_draft (a title and a description for a pull request, made from the local commit log and the names of the changed files; nothing is sent, no forge is asked). With it on a commit is also refused while a file of the index is unmerged or the lines it adds hold a whole conflict (<<<<<<<, =======, >>>>>>>), and a merge that stops on conflicts names git.conflicts. Off: the tools are not on offer and a commit is what it was. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_EDIT_LINES` | switch | `off` | on, 1, true turn it on | Offers fs.edit_lines to a coding run: it replaces the lines start to end of a file with new text (an empty text takes them out, end = start - 1 inserts before start) and is held to an anchor, the text of line start (expect_first_line): if the file has other text there, because the lines moved or the file changed, nothing is changed and the real lines are shown so the call can be repeated. It goes through the same door as fs.edit: the same folder checks, the same question and the same diff of the change, the same undo, and a file keeps its line endings and its last line break. It is not covered by the run-scoped edit grant (AQUA_EDIT_GRANT). Off until the edit-format bake-off (documentation/EDIT_BAKEOFF.md) has been run on the model: with it off the tool is not on offer (it is listed as unavailable) and the coding prompt does not mention it. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_LSP` | switch | `off` | on, 1, true turn it on | Offers code.definition, code.references, code.hover and code.document_symbols to a coding run: exact answers from a language server (rust-analyzer, gopls ...) that you named in data/tools/lsp.json with the absolute path of its program, its arguments and the SHA-256 of that program. A program is started only for a language the file names, in a folder marked trusted in fs.json, and only if its bytes have that hash (nothing is downloaded or installed, and a program with no hash is never started). The first call of a run for a language starts it and asks you, naming the program and its hash; it is put in a Job Object that ends the whole tree when the run ends, when it has been idle for five minutes or when it stops answering (the language is then unavailable for the rest of the run), and an answer larger than a cap and more requests than a limit are refused. What it answers is text from outside, shown as data, cut to a few lines, and a place outside the project is counted and never shown. It is read when Aqua starts: with it off (the default) the four tools are not registered at all, no list or prompt names them and no process can start. Tried only against a scripted fake server, never against a real one: see documentation/LSP_BRIDGE.md. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_LLAMA_CACHE_REUSE` | choice | `off` | 64; 128; 256; 512 | Starts the model server with --cache-reuse N: the smallest piece of a prompt, in tokens, that the server will take from its cache by shifting it, even when the text in front of it changed. Only these values are passed on; anything else is ignored with a line in the log. A speed experiment (card OA9-06): off until the speed gate and the planner bake-off have been run with it on, and a server that does not list the option is started as before. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_LLAMA_CACHE_RAM` | choice | `off` | 0; 512; 1024; 2048; 4096 | Starts the model server with --cache-ram N: how many MiB of the computer's memory the server may use to keep the prompts of idle slots (0 keeps none; the server's own default is 8,192). Only these values are passed on. A speed and memory experiment (card OA9-06): off until it has been measured, and a server that does not list the option is started as before. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_LLAMA_SLOT_SAVE` | switch | `off` | on, 1, true turn it on | Starts the model server with --slot-save-path data/cache/kv, a folder of Aqua's own under the data folder that holds nothing but the saved state of a slot, saves the slot of the agent's prompt when the server is stopped on purpose or goes idle (only when the start of the agent's prompt is in it) and puts it back right after the next start of the server (only when a file for this model file and this prompt start is there). A file there holds the model's state for that conversation start: the tokens of a conversation and what the model made of them, so it is as private as the conversation itself; its name holds hashes and no text. A build without the option, a refused request or a damaged file is one line of the log and never delays or fails a request. The folder is kept to 300 MB (the oldest file goes first), and a function that empties it exists but no command of Aqua calls it yet. A speed experiment (card OA9-06), tried only against a fake model server: off until the first request after a start has been measured with a restored slot. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_LLAMA_DRAFT` | choice | `off` | cpu (on, 1, true); gpu | Starts the model server with a small draft model for speculative decoding: the file named by AQUA_LLAMA_DRAFT_MODEL (a .gguf file name in the model folder, or a full path inside it or inside the one folder AQUA_LLAMA_DRAFT_FOLDER allows; Aqua never downloads one) with at most 5 tokens guessed at a time, at least 1 and none below a probability of 0.5. cpu (also on, 1, true) keeps the draft on the processor with --spec-draft-ngl 0, since the card is already full with the main model; gpu leaves it to the server. A file that is missing, is not a GGUF file, is the main model, has another vocabulary or is outside the folders leaves the launch as it was and the status says why. A speed experiment (card OA9-06): off until a measured gain in tokens a second over at least three runs, and the share of guesses the server kept (in the status) is 55 percent or more. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_PLANNER_TOOL_MODE` | choice | `json` | native | How the agent's planner gets its tool calls from the local model. json (the default, and what any other word means) holds the reply to a JSON schema, as it always was. native sends the tools of the round to the model server as its own tool list and turns the tool calls of the reply into the same call the JSON gives, which then goes through the same door with the same checks: a tool that was not offered, bad arguments or a call that is not a call are refused, never run and never guessed at. A model's entry in data/models/profiles.json may say tool_mode native or json for that model (read with AQUA_MODEL_PROFILES on, and it wins over this switch). The agent's planner only. A speed and accuracy experiment (card OA9-06): off until a bake-off says native calls are at least as accurate as the constrained JSON. **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_FAST_TIER` | switch | `off` | on, 1, true turn it on | Names a smaller local model (the .gguf file AQUA_FAST_TIER_MODEL, in the model folder; Aqua never downloads one) for the roles decision, fast and diagnosis, with a fall back to the main model when the small one is missing, cannot start, is not ready yet, takes longer than AQUA_FAST_TIER_SECS, answers with an error or has too small a window. The small model runs in a second model server of its own, started at the first call that needs it (never when Aqua starts), on the processor only unless AQUA_FAST_TIER_NGL says otherwise (so it never takes video memory from the main model), with a small window (AQUA_FAST_TIER_CTX), one slot, no draft model and no saved slots; it listens on 127.0.0.1 only, is stopped after AQUA_FAST_TIER_IDLE_SECS without a call and when Aqua stops, and a start that fails is not tried again for five minutes. Routed: the engine's decision calls and the rewrites of the dictation (role fast); no part of the engine asks for the role diagnosis yet. The status (GET /api/v1/models/roles, key speed_experiments) says whether it is off, waiting, starting, ready or failed, and why. A speed experiment (card OA9-06), tried only against a fake model server: off until a bake-off says the small model is accurate enough (scripts/eval/speed_experiments/run_experiment.py fast_tier). **It widens what the agent can reach, so a promotion may not set it.** |
| `AQUA_FAST_TIER_SECS` | number | `20` | a whole number from 1 to 600 | How many seconds the small model of the fast tier (AQUA_FAST_TIER) may take, of the call's own time, before the main model is asked instead; the main model then has what is left of the call's time. From 1 to 600; 20 is the default. It applies only while the fast tier is on. A call that finds the small model's server still loading is not waited for: it goes to the main model at once. |
| `AQUA_FAST_TIER_NGL` | number | `0` | a whole number from 0 to 99 | How many layers of the small model of the fast tier (AQUA_FAST_TIER) go to the graphics card. 0 is the default: the small model runs on the processor and never takes video memory from the main model, which on a 4 GB card is full with its 22 layers. A number above 0 starts the second server with the graphics-card build and that many layers, and is the person's choice and risk: it was not measured, and a card that holds both models is not assumed. From 0 to 99. |
| `AQUA_FAST_TIER_CTX` | number | `4096` | a whole number from 1024 to 16384 | The window, in tokens, of the second model server of the fast tier (AQUA_FAST_TIER): one slot of this size, so a small model keeps a small cache. A prompt that does not fit it is answered by the main model instead. From 1024 to 16384; 4096 is the default. |
| `AQUA_FAST_TIER_IDLE_SECS` | number | `300` | a whole number from 10 to 86400 | How many seconds the second model server of the fast tier (AQUA_FAST_TIER) may sit without a call before it is stopped to give its memory back; the next call that needs it starts it again. From 10 to 86400; 300 is the default. |

### Every name the source mentions

Generated from the source: every `"AQUA_*"` name the first-party Rust code mentions as a string (test files are not searched). It lists where to read what a variable does; `cargo test -p aqua-core --lib docs_gen` fails when this table is out of date, and `AQUA_UPDATE_DOCS=1` rewrites it.

208 variables.

| Variable | First file | Files |
|---|---|---|
| `AQUA_AGENTS` | `core/src/flags.rs` | 1 |
| `AQUA_AGENT_MAX_SECS` | `core/src/modules/browser_agent/guards.rs` | 2 |
| `AQUA_AGENT_MODEL` | `core/src/model/roles.rs` | 2 |
| `AQUA_AGENT_TOOLS` | `core/src/flags.rs` | 1 |
| `AQUA_ALLOWED_ORIGINS` | `core/src/api_gateway/auth.rs` | 1 |
| `AQUA_ALLOW_OPEN_MCP_LISTENER` | `adapters/mcp_adapters/src/listening.rs` | 1 |
| `AQUA_ANSWER_EVIDENCE` | `core/src/flags.rs` | 1 |
| `AQUA_API_PORT` | `core/src/cli_tools/config.rs` | 6 |
| `AQUA_API_TOKEN` | `core/src/api_gateway/auth.rs` | 7 |
| `AQUA_ASK` | `core/src/modules/coding_session.rs` | 4 |
| `AQUA_ATTACHMENTS_DIR` | `core/src/api_gateway/attachments_api.rs` | 1 |
| `AQUA_ATTACHMENT_MAX_MB` | `core/src/api_gateway/attachments_api.rs` | 1 |
| `AQUA_ATTACHMENT_STORE_MB` | `core/src/api_gateway/attachments_api.rs` | 1 |
| `AQUA_ATTENTION` | `core/src/api_gateway/attention.rs` | 2 |
| `AQUA_BACKGROUND_YIELD` | `core/src/modules/llm_router/llama_server/turn_queue.rs` | 1 |
| `AQUA_BIND_FIRST` | `core/src/startup_health/early.rs` | 1 |
| `AQUA_BROWSER_AUDIT_FULL` | `adapters/mcp_adapters/src/browser/audit.rs` | 1 |
| `AQUA_BROWSER_BACKEND` | `adapters/mcp_adapters/src/browser/backend.rs` | 1 |
| `AQUA_BROWSER_MCP_ARGS` | `adapters/mcp_adapters/src/browser_mcp/client.rs` | 1 |
| `AQUA_BROWSER_MCP_COMMAND` | `adapters/mcp_adapters/src/browser_mcp/client.rs` | 2 |
| `AQUA_BROWSER_MCP_EXTENSION_ID` | `adapters/mcp_adapters/src/browser_mcp/client.rs` | 1 |
| `AQUA_BROWSER_MCP_ONE_CONNECTION` | `adapters/mcp_adapters/src/browser_mcp/client.rs` | 1 |
| `AQUA_CAPS` | `core/src/promoted.rs` | 2 |
| `AQUA_CHECK_STEPS` | `core/src/flags.rs` | 1 |
| `AQUA_CHECK_TOOLS` | `core/src/flags.rs` | 2 |
| `AQUA_CLIPBOARD_TOOLS` | `core/src/api_gateway/route_api.rs` | 3 |
| `AQUA_CODE_SPEECH` | `core/src/coding_voice.rs` | 2 |
| `AQUA_CODE_TOOLS` | `core/src/flags.rs` | 2 |
| `AQUA_CODING_CHECK` | `core/src/modules/coding_runner.rs` | 1 |
| `AQUA_CODING_FORMAT` | `core/src/flags.rs` | 1 |
| `AQUA_CODING_PACKS` | `core/src/flags.rs` | 1 |
| `AQUA_CODING_PROMPT` | `core/src/modules/browser_agent/prompt.rs` | 1 |
| `AQUA_CODING_RECORD` | `core/src/flags.rs` | 1 |
| `AQUA_CODING_SESSIONS` | `core/src/flags.rs` | 1 |
| `AQUA_CODING_VOICE` | `core/src/api_gateway/attention.rs` | 3 |
| `AQUA_COLLAPSE_REPEATS` | `core/src/flags.rs` | 2 |
| `AQUA_COMMANDS` | `core/src/flags.rs` | 1 |
| `AQUA_COMMAND_TOOLS` | `core/src/flags.rs` | 2 |
| `AQUA_CONFIG_WATCH` | `core/src/flags.rs` | 1 |
| `AQUA_CONTEXT` | `core/src/modules/coding_session.rs` | 3 |
| `AQUA_CONTINUE` | `core/src/flags.rs` | 1 |
| `AQUA_CONVERSATION` | `core/src/conversation.rs` | 1 |
| `AQUA_DECISION_REFUSE` | `core/src/laya_validator/decision_route.rs` | 1 |
| `AQUA_DECISION_ROUTER` | `core/src/laya_validator/decision_route.rs` | 1 |
| `AQUA_DEEP_FILTER_BIN` | `adapters/audio_adapter/src/deepfilter_cli.rs` | 1 |
| `AQUA_DENY_FEEDBACK` | `core/src/flags.rs` | 1 |
| `AQUA_DESKTOP_STOP_KEY` | `core/src/main.rs` | 1 |
| `AQUA_DESKTOP_TIERS` | `adapters/mcp_adapters/src/windows_mcp/tiers.rs` | 1 |
| `AQUA_DEVICE_SIM` | `core/src/api_gateway/devices_api.rs` | 1 |
| `AQUA_DEV_UNSAFE` | `adapters/mcp_adapters/src/windows_mcp/mod.rs` | 1 |
| `AQUA_DIAGNOSE` | `core/src/triage/diagnose.rs` | 1 |
| `AQUA_DIAGNOSTICS` | `core/src/flags.rs` | 1 |
| `AQUA_DICTATION_MEDIUM` | `core/src/dictation/model_tier.rs` | 1 |
| `AQUA_DICTATION_MEDIUM_MODEL` | `core/src/dictation/model_tier.rs` | 1 |
| `AQUA_DICTATION_REWRITE` | `core/src/dictation/command.rs` | 1 |
| `AQUA_DOOR` | `core/src/flags.rs` | 3 |
| `AQUA_EDIT_GRANT` | `core/src/flags.rs` | 2 |
| `AQUA_EDIT_LINES` | `core/src/flags.rs` | 1 |
| `AQUA_EMBED_DIR` | `core/src/memory/embedding.rs` | 1 |
| `AQUA_ENGINES_DIR` | `adapters/audio_adapter/src/engines.rs` | 1 |
| `AQUA_FACT_RANK` | `core/src/memory/embedding.rs` | 1 |
| `AQUA_FAST_TIER` | `core/src/api_gateway/models_api.rs` | 4 |
| `AQUA_FAST_TIER_CTX` | `core/src/flags.rs` | 2 |
| `AQUA_FAST_TIER_IDLE_SECS` | `core/src/flags.rs` | 2 |
| `AQUA_FAST_TIER_MODEL` | `core/src/model/fast_route.rs` | 3 |
| `AQUA_FAST_TIER_NGL` | `core/src/flags.rs` | 2 |
| `AQUA_FAST_TIER_SECS` | `core/src/flags.rs` | 1 |
| `AQUA_FENCE_ALL` | `core/src/flags/dynamic_config.rs` | 2 |
| `AQUA_FLOWS_DB` | `core/src/flows/services.rs` | 1 |
| `AQUA_FLOWS_MODEL` | `core/src/flows/planner/llm.rs` | 1 |
| `AQUA_FONT` | `core/src/flows/image/card.rs` | 1 |
| `AQUA_FS_OUTLINE` | `core/src/flags.rs` | 1 |
| `AQUA_GIT_REVIEW` | `core/src/flags.rs` | 1 |
| `AQUA_IMAGE_DIR` | `core/src/flows/image/mod.rs` | 1 |
| `AQUA_IMAGE_ENDPOINT` | `core/src/flows/image/diffusion.rs` | 1 |
| `AQUA_KOKORO_CLI_BIN` | `adapters/audio_adapter/src/kokoro_cli.rs` | 1 |
| `AQUA_KOKORO_MODELS` | `core/src/startup_health.rs` | 1 |
| `AQUA_KOKORO_ROOT` | `adapters/audio_adapter/src/kokoro_cli.rs` | 1 |
| `AQUA_LAST_STEP_FINAL` | `core/src/flags.rs` | 1 |
| `AQUA_LAYA` | `core/src/laya_validator/engine.rs` | 1 |
| `AQUA_LAYA_FAST` | `core/src/laya_validator/engine.rs` | 1 |
| `AQUA_LAYA_TIMEOUT_SECS` | `core/src/laya_validator/engine.rs` | 1 |
| `AQUA_LEARNING_ROOT` | `core/src/memory/experience/service.rs` | 1 |
| `AQUA_LLAMA_CACHE_RAM` | `core/src/api_gateway/models_api.rs` | 3 |
| `AQUA_LLAMA_CACHE_REUSE` | `core/src/api_gateway/models_api.rs` | 3 |
| `AQUA_LLAMA_CLI_BIN` | `core/src/modules/llm_router/llama_cli.rs` | 1 |
| `AQUA_LLAMA_CTX` | `core/src/modules/llm_router/llama_server/launch_settings.rs` | 1 |
| `AQUA_LLAMA_DEVICE` | `core/src/modules/llm_router/gpu.rs` | 1 |
| `AQUA_LLAMA_DRAFT` | `core/src/api_gateway/models_api.rs` | 3 |
| `AQUA_LLAMA_DRAFT_FOLDER` | `core/src/modules/llm_router/server_options.rs` | 1 |
| `AQUA_LLAMA_DRAFT_MODEL` | `core/src/modules/llm_router/server_options.rs` | 1 |
| `AQUA_LLAMA_FA` | `core/src/modules/llm_router/launch_args.rs` | 2 |
| `AQUA_LLAMA_GPU` | `core/src/modules/llm_router/gpu.rs` | 1 |
| `AQUA_LLAMA_GPU_SERVER_BIN` | `core/src/modules/llm_router/gpu.rs` | 1 |
| `AQUA_LLAMA_IDLE_SECS` | `core/src/modules/llm_router/llama_server/launch_settings.rs` | 1 |
| `AQUA_LLAMA_KV` | `core/src/modules/llm_router/launch_args.rs` | 2 |
| `AQUA_LLAMA_MODEL` | `core/src/modules/llm_router/llama_cli.rs` | 1 |
| `AQUA_LLAMA_NGL` | `core/src/modules/llm_router/llama_cli.rs` | 2 |
| `AQUA_LLAMA_QUEUE_SECS` | `core/src/modules/llm_router/llama_server/turn_queue.rs` | 1 |
| `AQUA_LLAMA_SERVER` | `core/src/modules/llm_router/llama_server/launch_settings.rs` | 1 |
| `AQUA_LLAMA_SERVER_BIN` | `core/src/modules/llm_router/llama_server/launch_settings.rs` | 1 |
| `AQUA_LLAMA_SLOTS` | `core/src/modules/llm_router/llama_server/launch_settings.rs` | 1 |
| `AQUA_LLAMA_SLOT_SAVE` | `core/src/api_gateway/models_api.rs` | 4 |
| `AQUA_LLAMA_SPEC` | `core/src/modules/llm_router/launch_args.rs` | 2 |
| `AQUA_LLAMA_THINK_BUDGET` | `core/src/modules/llm_router/launch_args.rs` | 2 |
| `AQUA_LLAMA_THREADS` | `core/src/modules/llm_router/llama_cli.rs` | 2 |
| `AQUA_LLAMA_THREADS_BATCH` | `core/src/modules/llm_router/llama_server/launch_settings.rs` | 1 |
| `AQUA_LLAMA_TIMEOUT_SECS` | `core/src/modules/llm_router/llama_cli.rs` | 1 |
| `AQUA_LOG_FILE` | `core/src/logging.rs` | 1 |
| `AQUA_LOG_FORMAT` | `core/src/logging.rs` | 1 |
| `AQUA_LOOPBACK_ONLY_PORT` | `adapters/mcp_adapters/src/browser_mcp/client.rs` | 1 |
| `AQUA_LSP` | `core/src/flags.rs` | 2 |
| `AQUA_MCP_PROTOCOL_NEGOTIATION` | `adapters/mcp_adapters/src/stdio_mcp.rs` | 1 |
| `AQUA_MCP_REMOTE` | `core/src/flags.rs` | 1 |
| `AQUA_MODEL_PROFILES` | `core/src/flags.rs` | 2 |
| `AQUA_MODEL_PROVIDERS` | `core/src/api_gateway/models_api.rs` | 4 |
| `AQUA_MODEL_ROUTER` | `core/src/api_gateway/route_api.rs` | 3 |
| `AQUA_MXC_EXE` | `adapters/native_tools/src/sandbox.rs` | 1 |
| `AQUA_N8N_URL` | `core/src/flows/n8n.rs` | 1 |
| `AQUA_NESTED_NOTES` | `core/src/flags.rs` | 1 |
| `AQUA_NODE_BIN` | `adapters/mcp_adapters/src/browser/managed.rs` | 1 |
| `AQUA_NOT_REGISTERED` | `core/src/api_gateway/route_api.rs` | 3 |
| `AQUA_OA11_PROBE` | `core/src/hooks.rs` | 1 |
| `AQUA_ONE_DOOR` | `core/src/flags.rs` | 3 |
| `AQUA_ORB_NO_HOOKS` | `apps/aqua-desktop/src-tauri/src/orb_place/mod.rs` | 1 |
| `AQUA_PARENT_PID` | `core/src/main.rs` | 3 |
| `AQUA_PLAN` | `core/src/api_gateway/route_api.rs` | 4 |
| `AQUA_PLANNER_BATCH` | `core/src/modules/browser_agent/recipe_replay.rs` | 4 |
| `AQUA_PLANNER_CONSTRAINT` | `core/src/modules/browser_agent/recipe_replay.rs` | 3 |
| `AQUA_PLANNER_THOUGHT` | `core/src/modules/planner/schema.rs` | 1 |
| `AQUA_PLANNER_TOOL_MODE` | `core/src/api_gateway/models_api.rs` | 3 |
| `AQUA_PLAN_MODE` | `core/src/modules/browser_agent/guards.rs` | 2 |
| `AQUA_PLATFORM_FACEBOOK` | `core/src/flows/platform.rs` | 1 |
| `AQUA_PREAPPROVALS` | `core/src/api_gateway/preapproval_api.rs` | 4 |
| `AQUA_PREFIX_STABLE` | `core/src/flags.rs` | 1 |
| `AQUA_PROBE_OUT` | `core/src/modules/planner/schema.rs` | 1 |
| `AQUA_PROJECT_PACK` | `core/src/flags/dynamic_config.rs` | 3 |
| `AQUA_PROJECT_TERMS` | `core/src/coding_voice.rs` | 2 |
| `AQUA_READER` | `core/src/flags.rs` | 3 |
| `AQUA_RECIPES_V2` | `core/src/flags.rs` | 2 |
| `AQUA_RECIPE_EXPLORE` | `core/src/flows/recipe_builder/start.rs` | 1 |
| `AQUA_REDTEAM_ENV_SECRET_TOKEN` | `core/src/redteam.rs` | 1 |
| `AQUA_REMOTE_CODING` | `core/src/doctor.rs` | 3 |
| `AQUA_REMOTE_CODING_SEND` | `core/src/modules/coding_runner.rs` | 2 |
| `AQUA_REMOTE_CODING_URL` | `core/src/doctor.rs` | 3 |
| `AQUA_REMOTE_MODEL` | `core/src/flags/dynamic_config.rs` | 3 |
| `AQUA_REPO_MAP` | `core/src/flags.rs` | 2 |
| `AQUA_REQUEST_MAX_SECS` | `core/src/api_gateway/server/middleware.rs` | 1 |
| `AQUA_RESEARCH` | `core/src/api_gateway/route_api.rs` | 4 |
| `AQUA_ROUTER_V2` | `core/src/api_gateway/route_api.rs` | 3 |
| `AQUA_RUN_CAPTURE` | `core/src/agent_runs/mod.rs` | 1 |
| `AQUA_RUN_EVENTS` | `core/src/agent_runs/events.rs` | 1 |
| `AQUA_SAMPLING_PRESETS` | `core/src/flags.rs` | 2 |
| `AQUA_SCREEN_TOOLS` | `core/src/flags.rs` | 2 |
| `AQUA_SEARCH_DUCKDUCKGO` | `adapters/native_tools/src/http/mod.rs` | 1 |
| `AQUA_SEED` | `core/src/flags.rs` | 3 |
| `AQUA_SELF_REPAIR_DB` | `core/src/self_repair/mod.rs` | 1 |
| `AQUA_SELF_REPAIR_WATCH` | `core/src/self_repair/watch.rs` | 1 |
| `AQUA_SELF_REPAIR_WATCH_SECS` | `core/src/self_repair/watch.rs` | 1 |
| `AQUA_SESSION_SUMMARY` | `core/src/flags.rs` | 1 |
| `AQUA_SHIPPED_SKILLS` | `core/src/flags.rs` | 2 |
| `AQUA_SHORT_CHECK_OUTPUT` | `core/src/flags.rs` | 1 |
| `AQUA_SKILLS_V2` | `core/src/flags.rs` | 1 |
| `AQUA_SKILL_PROPOSALS` | `core/src/flags.rs` | 1 |
| `AQUA_SNAPSHOT` | `core/src/flags.rs` | 1 |
| `AQUA_SOCIAL_FACEBOOK_URL` | `core/src/flows/social/facebook.rs` | 1 |
| `AQUA_SOCIAL_LINKEDIN_URL` | `core/src/flows/social/linkedin.rs` | 1 |
| `AQUA_SPEECH_GATE` | `adapters/audio_adapter/src/speech_gate.rs` | 1 |
| `AQUA_START_MCP_ON_BOOT` | `core/src/main.rs` | 1 |
| `AQUA_STEER_CUES` | `core/src/flags.rs` | 1 |
| `AQUA_STREAM_FIRST_EVENT_SECS` | `core/src/flags.rs` | 4 |
| `AQUA_STREAM_IDLE_SECS` | `core/src/flags.rs` | 4 |
| `AQUA_SWEEP_PROFILE` | `adapters/mcp_adapters/src/browser/backend.rs` | 1 |
| `AQUA_SYNTAX_GATE` | `core/src/flags.rs` | 1 |
| `AQUA_TERM_AUTO_BACKGROUND` | `adapters/native_tools/src/term/mod.rs` | 1 |
| `AQUA_TERM_CHECK_GRANTS` | `adapters/native_tools/src/term/mod.rs` | 1 |
| `AQUA_TERM_JOURNAL` | `adapters/native_tools/src/term/journalled.rs` | 1 |
| `AQUA_TERM_LIVE_OUTPUT` | `adapters/native_tools/src/term/mod.rs` | 1 |
| `AQUA_TERM_PROJECT_CHECKS` | `adapters/native_tools/src/term/mod.rs` | 1 |
| `AQUA_TEST_BATCH` | `core/src/flags.rs` | 1 |
| `AQUA_TEST_SECRET_THING` | `adapters/native_tools/src/sys.rs` | 1 |
| `AQUA_TEST_STRICT_DATA` | `core/src/runtime/data_tripwire.rs` | 1 |
| `AQUA_TOOL_MODE` | `core/src/doctor.rs` | 2 |
| `AQUA_TOOL_OUTPUT_SPILL` | `core/src/flags.rs` | 2 |
| `AQUA_TOOL_SEARCH` | `core/src/flags.rs` | 3 |
| `AQUA_TRACE_CONTENT` | `core/src/agent_runs/export.rs` | 1 |
| `AQUA_TRACE_EXPORT` | `core/src/agent_runs/export.rs` | 1 |
| `AQUA_TRIM_SILENCE` | `core/src/pipeline/speech_checks.rs` | 1 |
| `AQUA_UI_TOKEN` | `core/src/api_gateway/auth.rs` | 2 |
| `AQUA_UNATTENDED_MAY_CHANGE` | `adapters/tool_host/src/host.rs` | 1 |
| `AQUA_UPDATE_DOCS` | `core/src/api_gateway/openapi.rs` | 2 |
| `AQUA_UPDATE_URL` | `apps/aqua-desktop/src-tauri/src/updates.rs` | 1 |
| `AQUA_USER_MEMORY` | `core/src/memory/user_facts/mod.rs` | 1 |
| `AQUA_VOICE_MATCH_THRESHOLD` | `adapters/audio_adapter/src/biometrics_ecapa.rs` | 1 |
| `AQUA_VOTE` | `core/src/triage/diagnose.rs` | 1 |
| `AQUA_WAKE_MODEL` | `core/src/api_gateway/server/listen_api.rs` | 1 |
| `AQUA_WARMUP` | `core/src/api_gateway/server/warmup.rs` | 1 |
| `AQUA_WEATHER_DIRECT` | `core/src/live_weather.rs` | 1 |
| `AQUA_WHISPER_CLI_BIN` | `adapters/audio_adapter/src/whisper_cli.rs` | 1 |
| `AQUA_WHISPER_FULL_CONTEXT` | `adapters/audio_adapter/src/whisper_cli.rs` | 1 |
| `AQUA_WHISPER_LISTEN_THREADS` | `adapters/audio_adapter/src/whisper_cli.rs` | 1 |
| `AQUA_WHISPER_THREADS` | `adapters/audio_adapter/src/whisper_cli.rs` | 1 |
| `AQUA_WHISPER_VAD` | `adapters/audio_adapter/src/whisper_cli.rs` | 1 |
| `AQUA_WINDOWS_MCP_ARGS` | `adapters/mcp_adapters/src/windows_mcp/mod.rs` | 1 |
| `AQUA_WINDOWS_MCP_COMMAND` | `adapters/mcp_adapters/src/windows_mcp/mod.rs` | 1 |
| `AQUA_WORKFLOWS_DB` | `core/src/workflows/runner.rs` | 1 |
| `AQUA_WORKSPACE_ROOT` | `adapters/mcp_adapters/src/browser_mcp/health.rs` | 2 |
| `AQUA_WORKTREES` | `core/src/flags.rs` | 1 |
