---
name: aqua-mcp-debug
description: Diagnose Aqua's MCP agent stack — Browser MCP (npx @browsermcp/mcp + Chrome extension), Windows-MCP (uvx windows-mcp serve), the stdio JSON-RPC client, the Laya gate and the llama.cpp agent loop. Use when MCP tools hang, time out, report "not started", the agent loops or claims success falsely, or when changing adapters/mcp_adapters or core/src/modules/browser_agent.rs.
---

# Debug the MCP agent stack

## Map
- `adapters/mcp_adapters/src/stdio_mcp.rs` — generic newline-delimited JSON-RPC client. stdout is read on a thread and every request has a deadline (`StdioMcpConfig.timeout`: browser 60 s, windows 120 s). On timeout the server is killed (process tree via `taskkill /T` on Windows) and restarted on the next call. stderr is drained and logged at `debug`.
- `browser/` — **the browser layer** (`documentation/BROWSER.md`). `backend.rs` `BrowserBackend::call` is the only path to a browser: tool allowlist → arg checks → URL policy (ask/allow/deny) → tab/JS/upload rules → confirmation broker → dispatch → post-action URL re-check → audit (`data/browser/audit.jsonl`). `policy.rs` (rules, classifiers), `observation.rs` (Playwright section parser, snapshot compaction, error codes), `managed.rs` (Playwright MCP launch flags), `extract.rs` (audited extractor scripts), `confirm.rs`.
- Managed backend = pinned Playwright MCP in `tools/browser-mcp` (installed by `scripts/setup-managed-browser.ps1`) driving Edge in `data/browser/profile`. Extension backend = Browser MCP below.
- `browser_mcp/{client,health}.rs` — Browser MCP (extension backend) config + readiness (installed → configured → server → extension connected → action probe).
- `windows_mcp/mod.rs` — Windows-MCP config.
- `core/src/modules/browser_agent.rs` — plan → act → observe loop driving llama.cpp; `local_open_url` only accepts http(s) URLs (`is_safe_web_url`).
- `core/src/laya_validator/` — refuse / chat / mcp_agent gate; falls back to heuristics after `AQUA_LAYA_TIMEOUT_SECS`.
- Gateway routes in `core/src/api_gateway/server.rs` (`/browser-mcp/*`, `/windows-mcp/*`, `/mcp/agent`, `/mcp/agent/stream`).

## Triage steps
0. Which backend? `GET /api/v1/browser/status` (`managed` or `extension`), pending approvals at `/api/v1/browser/confirmations`, real-browser tests: `cargo test -p aqua-mcp-adapters --test browser_backend_test -- --ignored --test-threads=1`.
1. Extension prereqs: `powershell -File scripts/check-browser-mcp.ps1 -Json` (reports `stage`: install / configure / server_start / browser_connect / action_probe / none). Windows-MCP: `scripts/setup_windows_mcp.ps1`.
2. See server stderr: run core with `RUST_LOG=aqua_mcp_adapters=debug`.
3. Status without side effects: `GET /api/v1/browser-mcp/status`, `/windows-mcp/status`, `/api/v1/laya/status` (token: see `aqua-run`).
4. Hangs: a request blocked longer than the config timeout is a bug — check nothing reintroduced a blocking read on the child's stdout, or piped stderr without draining.
5. Agent misbehaviour: stream `/api/v1/mcp/agent/stream` to see every plan/act/observe step; wrong route → `POST /api/v1/laya/feedback {"message","correct_route"}`.

## Safety invariants (keep them when editing)
- The gateway must stay behind `auth::require_local_auth` + allow-listed CORS; never reintroduce `CorsLayer::permissive()` or unauthenticated routes that reach MCP/PowerShell.
- Windows-MCP is pinned (0.8.5, `--offline`, `--tools` = 14 UI tools) and every call goes through `WindowsGuard::call` (`adapters/mcp_adapters/src/windows_mcp/guard.rs`): PowerShell, FileSystem, Registry, Process, Scrape and Clipboard are never exposed, `App` cannot take `executable`/`args`/`cwd` or launch shells, voice can only look. `tests/tool_routes.rs` fails if `call_tool(` appears outside the guarded files; `tests/windows_mcp_live.rs` (`--ignored`) checks the real server. Child processes get a scrubbed environment (no `AQUA_*`, tokens, cloud keys).
- Browser calls must go through `BrowserBackend::call` — never call the managed/extension `StdioMcpClient` directly from core. After `CONFIRMATION_DENIED`/`POLICY_*`, the agent must not retry another way (no local_open_url / PowerShell fallback).
- Anything the model outputs that becomes a process/URL/path must be validated (web page content can prompt-inject the agent).
- Truncate strings with `chars()`, never byte slices, on tool descriptions/outputs.
- Don't `.expect()` on shared `Mutex`es in handlers — use `unwrap_or_else(PoisonError::into_inner)`.
- Only call tools that act on the desktop/browser when the user asked for that action.
