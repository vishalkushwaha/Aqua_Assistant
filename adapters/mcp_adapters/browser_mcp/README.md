# Browser + Windows MCP for Aqua

Gives the **llama.cpp agent** control of your **Chrome tab** and **Windows desktop UI** through open-source MCP servers — no git clones required.

## Architecture

```
User goal (UI / API)
        │
        ▼
aqua-core  POST /api/v1/mcp/agent
        │
        ├─► LlamaRouterEngine  (proposes JSON tool calls)
        │
        ├─► BrowserMcpClient  →  npx @browsermcp/mcp@0.1.3  →  Chrome extension
        │
        └─► WindowsMcpClient  →  uvx windows-mcp serve       →  desktop UI automation
```

## Setup

### Browser MCP

1. Node.js installed (`npx` on PATH).
2. Run the idempotent setup (machine-readable JSON with `-Json`):

```powershell
./scripts/setup-browser-mcp.ps1 -Json
./scripts/check-browser-mcp.ps1 -Json
./scripts/start-browser-mcp.ps1 -Json   # requires aqua-core
```

3. Install & **Connect** the extension: [Set up extension](https://docs.browsermcp.io/setup-extension).
   Aqua never installs Chrome extensions silently and never stores browser passwords.

### Automatic recovery (agent)

Every browser-enabled agent run (`/api/v1/mcp/agent`) goes through:

`Detect → Setup (if needed) → Start MCP → Probe browser action → Continue task`

Contract (`GET /api/v1/browser-mcp/health`, `POST /api/v1/browser-mcp/ensure`):

| Flag | Meaning |
|------|---------|
| `installed` | Node + npx available / package resolvable |
| `configured` | Aqua Browser MCP config / setup state |
| `server_running` | stdio MCP process initialized |
| `browser_connected` | Chrome extension Connected |
| `browser_action_available` | Live `browser_snapshot` / `browser_navigate` probe OK |
| `ready` | All five true |

If the extension is offline, the agent opens the setup docs, polls briefly, falls back to Windows tools when available, and **does not claim success** without evidence. After Connect, a mid-task recovery can re-enable `browser_*` tools and continue the same goal.

### Windows-MCP

1. `./scripts/setup_windows_mcp.ps1` (installs UV if needed, prefetches `windows-mcp` from PyPI).
2. Prefer English Windows UI for best automation results.
3. Upstream: [CursorTouch/Windows-MCP](https://github.com/CursorTouch/Windows-MCP).

Then run `cargo run -p aqua-core` and open the **MCP Agents** tab.

## API

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/api/v1/browser-mcp/status` | Browser process / init status |
| GET | `/api/v1/browser-mcp/health` | Full ready contract (installed…action) |
| POST | `/api/v1/browser-mcp/setup` | Run idempotent setup script |
| POST | `/api/v1/browser-mcp/ensure` | Setup + start + connect poll + action probe |
| POST | `/api/v1/browser-mcp/start` | Spawn Browser MCP + initialize |
| POST | `/api/v1/browser-mcp/stop` | Kill Browser MCP |
| GET | `/api/v1/browser-mcp/tools` | Browser `tools/list` |
| POST | `/api/v1/browser-mcp/call` | `{ "tool", "arguments" }` |
| POST | `/api/v1/browser-mcp/agent` | Browser-only agent loop |
| GET | `/api/v1/windows-mcp/status` | Windows process / init status |
| POST | `/api/v1/windows-mcp/start` | Spawn Windows-MCP + initialize |
| POST | `/api/v1/windows-mcp/stop` | Kill Windows-MCP |
| GET | `/api/v1/windows-mcp/tools` | Windows `tools/list` |
| POST | `/api/v1/windows-mcp/call` | `{ "tool", "arguments" }` |
| POST | `/api/v1/mcp/agent` | Unified agent: `{ "goal", "max_steps?", "use_browser?", "use_windows?" }` |

## Env overrides

| Variable | Meaning |
|----------|---------|
| `AQUA_BROWSER_MCP_COMMAND` | Default `npx` |
| `AQUA_BROWSER_MCP_ARGS` | Default `-y @browsermcp/mcp@0.1.3` |
| `AQUA_WINDOWS_MCP_COMMAND` | Default resolved `uvx` (or `~/.local/bin/uvx.exe`) |
| `AQUA_WINDOWS_MCP_ARGS` | Default `windows-mcp serve` |

## Code

| Path | Role |
|------|------|
| `adapters/mcp_adapters/src/stdio_mcp.rs` | Shared NDJSON stdio MCP client |
| `adapters/mcp_adapters/src/browser_mcp/` | Browser defaults |
| `adapters/mcp_adapters/src/windows_mcp/` | Windows defaults |
| `core/src/modules/browser_agent.rs` | Llama ↔ tools loop (`run_mcp_agent`) |
| `apps/aqua-desktop/src/components/BrowserMcpPanel.tsx` | Verification UI |

## Wire protocol

Aqua speaks **newline-delimited JSON-RPC** on stdio (MCP SDK `StdioServerTransport` / FastMCP), not Content-Length framing.
