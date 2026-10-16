# Aqua Desktop: the app (Tauri shell, Vite + React page)

The window you use: the voice orb, the chat, and the admin pages for the voice pipeline, the agents and tools, flows and workflows, devices, models and diagnostics. The page is React; the shell around it is Tauri v2 (Rust). Neither holds the assistant: that is `aqua-core`, which the shell starts as its sidecar and the page talks to over HTTP on `127.0.0.1:8767`.

---

## Stack

| Layer | Technology |
| :--- | :--- |
| UI | React 18, TypeScript, Lucide icons |
| Build | Vite 5 (dev server on `127.0.0.1:1420`) |
| Styling | Vanilla CSS (`src/index.css`, glass-panel theme) |
| Shell | Tauri v2: `src-tauri/src/main.rs` opens the main window and the orb window, the tray icon and the orb menu, starts `aqua-core` as its sidecar (and starts it again up to three times if it stops by itself), hands the page the gateway address and the UI token, moves the orb out of the way (`orb_place/`), and checks for and installs updates (`updates.rs`, `update_policy.rs`) |

---

## Development

```bash
cd apps/aqua-desktop
npm install
npm run dev          # the Vite page on 127.0.0.1:1420; start aqua-core separately
npm test             # the TypeScript tests (node --test; Node 22.6 or newer)
npm run build        # tsc and vite build
```

`scripts/start-aqua.ps1` (the `npm run aqua` script) starts the page, the shell and a debug core together, in order. `scripts/start-aqua-release.ps1` is the release counterpart. Do not start or stop the Aqua you use from here; see `CLAUDE.md`.

In development the Vite proxy forwards `/api` and `/ws` to the core and adds the UI token (the API token if the UI token does not exist yet), and only to requests that come from this page (`vite.config.ts`). A packaged app has no proxy: the shell hands the token to its own page (`src/api/gateway.ts`).

---

## Pages (`src/nav.ts`, `src/App.tsx`)

The menu is built from `src/nav.ts`, and that file is the list to trust. This table names the long-standing pages.

| Group | Pages (component) |
| :--- | :--- |
| Assistant | Overview (`OverviewPage.tsx`); Voice pipeline test (`PipelineVerificationDashboard.tsx`) |
| Voice engines | Noise clean-up (`DeepFilterNetVerificationPanel.tsx`); Speech to text (`WhisperVerificationPanel.tsx`); Language model (`LlamaCppPlaygroundPanel.tsx`); Voices (`TtsVoiceSelectorPanel.tsx`) |
| Agents and automation | Browser and desktop agents (`BrowserMcpPanel.tsx`); Tools and approvals (`ToolsPanel.tsx`); Runs and proof (`RunsPanel.tsx`); Permissions and consents (`PermissionsPage.tsx`); Background runs (`QueuePanel.tsx`); Agent learning (`AgentLearningPanel.tsx`); Memory (`MemoryPage.tsx`); Skills inbox (`SkillsInbox.tsx`); Workflows (`WorkflowsPanel.tsx`); AI flows (`FlowsPanel.tsx`) |
| System | Models and pipeline (`ModelRegistryPanel.tsx`); File index (`FileIndexPanel.tsx`); Devices and battery (`DevicesPanel.tsx`); Orb position (`OrbPositionPanel.tsx`); System diagnostics (`SystemDiagnosticsPanel.tsx`); Voice lock and admin (`VoiceEnrollmentWizard.tsx`); Security and risk matrix (`SecurityMatrixPanel.tsx`); Market intelligence (`MarketIntelligencePanel.tsx`) |

The security-matrix and market-intelligence pages are placeholders: fixed text and switches that call nothing in the core. Shared chrome: `AdminHeader.tsx` (RAM meter, biometric status), `TaskBar.tsx` (what the assistant is doing), and the approval prompt that any task can raise (`BrowserConfirmationPrompt.tsx`). The orb is `VoiceOrb.tsx` and `NeuralOrb.tsx`.

Much of what the core can do has a route and no screen yet, because the interface is waiting for a redesign (decision D-7): `documentation/UI_CONTRACT.md` lists what the redesign is built from, and `documentation/KNOWN_LIMITS.md` lists what waits for it.

---

## Backend integration

1. **REST**: the typed clients are in `src/api/` (one file per area). Every route is in `documentation/openapi.json`.
2. **Streams**: a spoken or typed turn is streamed as server-sent events (`src/api/pipelineStream.ts`), so the first sentence is heard while the rest is still being written.
3. **Tauri commands** (the page calls the shell, not the core): `gateway_info`, `open_external`, the orb commands (`orb_menu`, `orb_resize`, `bring_to_front`, `orb_place_*`, `orb_hit_regions`) and the update commands (`update_status`, `check_for_update`, `install_update`, `update_progress`).
4. **WebSocket**: `ws://127.0.0.1:8767/ws` (or `/ws` through the dev proxy).

The page keeps no state library; `useState` and a small shared poller for the core's status (`src/hooks/coreStatusStore.ts`).

---

## Tests

- `tests/*.test.ts`: the TypeScript tests (`npm test`).
- `e2e/*.e2e.mjs`: end-to-end scripts for flows, workflows, learning and the packaged app; they need a running core and a browser, and are not part of the gate.
- `src-tauri` has Rust tests of its own (`cargo test` in that folder; it is not a workspace member), and the update policy is also run from `core/tests/update_policy_test.rs`.

Counts and how they are made: `documentation/TESTING.md`.

---

## Architecture doc note

The system design refers to `src/views/`; this app uses **`src/components/`** only.
