---
name: aqua-run
description: Start, stop and smoke-test the Aqua stack locally — aqua-core API gateway (127.0.0.1:8767) and the Vite admin UI (127.0.0.1:1420) — including the API token. Use when asked to run/launch/restart Aqua, check the gateway is up, call an /api/v1 endpoint, or verify a change end to end.
---

# Run Aqua locally

## Topology
- `aqua-core` (`core/src/main.rs`) binds **127.0.0.1:8767** (hardcoded in `run_api_gateway(8767, …)`). Some docs still say 8765 — 8767 is correct.
- Vite UI (`apps/aqua-desktop`) on **127.0.0.1:1420**; proxies `/api` and `/ws` to 8767 and injects the API token.
- Kokoro TTS daemon (Python) on 127.0.0.1:8766, started by `core/src/startup_health.rs`.
- Managed browser (Playwright MCP + Edge, Aqua profile): starts on the first browser task, closes after 10 idle min. Setup: `scripts/setup-managed-browser.ps1`. Policy/approvals: `documentation/BROWSER.md`.

## API token (required)
Every route except `GET /api/v1/health` needs `Authorization: Bearer <token>`; requests with a non-allow-listed `Origin` or non-loopback `Host` get 403 (`core/src/api_gateway/auth.rs`).
- Token: `AQUA_API_TOKEN` env, else `data/.aqua_api_token` (created on first core start, git-ignored). Never print it into chat or commit it.
- Extra UI origins: `AQUA_ALLOWED_ORIGINS=http://127.0.0.1:5173,...`

## Start
1. Check port first — the user may already run an instance. Do **not** kill a process you didn't start:
   `netstat -ano | grep ':8767' | grep LISTEN`
2. Core (from repo root so `data/` resolves):
   - `cargo run -p aqua-core --release` (release: Laya gate is much faster than debug)
   - `AQUA_START_MCP_ON_BOOT=1` starts Browser/Windows MCP at boot. The default is off: the agent starts each server the first time it needs it.
   - Run it in the background and poll `GET /api/v1/health` until 200.
3. UI: `cd apps/aqua-desktop && npm run dev` → http://127.0.0.1:1420

## Smoke test
```bash
T="${AQUA_API_TOKEN:-$(tr -d '[:space:]' < data/.aqua_api_token)}"
curl -s http://127.0.0.1:8767/api/v1/health
curl -s -H "Authorization: Bearer $T" http://127.0.0.1:8767/api/v1/pipeline/health
curl -s -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
  -d '{"task_id":"t1","prompt":"what is 2+2"}' http://127.0.0.1:8767/api/v1/task/dispatch
```
Or on Windows: `powershell -File scripts/verify_local_stack.ps1` (text → LLM → TTS, and jfk.wav → full voice pipeline).

Endpoints that act on the machine (`/windows-mcp/call`, `/browser/call`, `/browser-mcp/call`, `/mcp/agent*`) — only call them when the user asked for that action. Browser actions may pause for approval (`GET /api/v1/browser/confirmations`); never approve on the user's behalf.

## Tests
- Fast unit tests: `cargo test --offline -p aqua-core --lib` and `-p aqua-mcp-adapters --lib`.
- `core/tests/*` and `adapters/*/tests/*` integration tests need model files / built binaries (see `aqua-build`); report skips honestly.

## Useful env
`AQUA_LLAMA_MODEL` (GGUF basename or path), `AQUA_LLAMA_TIMEOUT_SECS` (default 180), `AQUA_LLAMA_THREADS`, `AQUA_LLAMA_NGL`, `AQUA_LAYA_TIMEOUT_SECS` (default 15), `AQUA_WORKSPACE_ROOT`.
