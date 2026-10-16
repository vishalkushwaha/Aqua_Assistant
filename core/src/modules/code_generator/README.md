# Autonomous Code Generation Module (`core/src/modules/code_generator/`)

> **Code status (2026-10-08): written, not wired.** `TddEngine` has tests, and nothing outside them calls it ([who calls what](../../../../documentation/competitive/refresh/10_DEAD_CODE_CALLERS.md)). The text below describes the design, not what runs in Aqua. It writes `assert!(true)` and reports a pass. Register: `sim.code_generator` (simulated).

Handles full-stack autonomous project creation from natural language prompts (e.g., *"Create a project for a shopping website"*).

## Components
- **Project Scaffolder**: Creates directory structures, configuration files (`package.json`, `Cargo.toml`), and source code files.
- **Full-Stack Generator**: Writes HTML/CSS/JS/TS frontend, backend APIs, and database schemas.
- **Verification Loop**: Invokes system build & test harnesses (`cargo check`, `npm run build`) to verify generated code compiles cleanly before completion.

---

## Step 4.3: CPU Execution & Memory-Managed Worker Pool (`worker_pool/`)

The **CPU Worker Pool** executes compute-heavy AI tasks (VLM, OCR, Code Gen LLM, Image Gen) under strict CPU resource limits:

### 1. Memory Lifecycle Manager (`model_lifecycle.rs`)
- Forcibly drops ONNX / GGML model handles from RAM immediately upon task completion.
- Triggers OS memory garbage collection (`malloc_trim` / `SetProcessWorkingSetSize`) to return peak memory (up to 1.5GB) back to the **<200MB background baseline within 2 seconds**.

### 2. Dynamic Thread Priority & Core Allocation
- Assigns worker threads `BELOW_NORMAL_PRIORITY_CLASS` (Windows) or `nice 10` (Linux/macOS) to ensure background AI processing never stutters mouse movement or desktop UI responsiveness.
- Reserves at least **1 CPU core** free from AI workload for real-time OS audio sampling and system responsiveness.

### 3. Hard Execution Watchdog & Timeout Guard
- Enforces a 60-second hard execution deadline per task step.
- Automatically terminates runaway threads or child processes that hang in infinite loops, freeing memory and reporting a clean diagnostic error.

### 4. Runtime CPU SIMD Feature Detection
- Inspects CPU flags at startup (`AVX-512`, `AVX2`, `ARM NEON`).
- Dynamically loads the optimal vectorized ONNX runtime execution provider without crashing on older processors.

---

## Autonomous 4-Stage Test-Driven Development (TDD) & Documentation Lifecycle (`code_generator/tdd_lifecycle.rs`)

When the user asks Aqua to write tests or build a feature for a project (e.g., *"Write test cases for this personal project"*), Aqua executes a **strict 4-stage step-by-step workflow**:

```
[ User Request: "Write tests for my project" ]
                     │
                     ▼
┌────────────────────────────────────────────────────────┐
│ Stage 1: Code Structure & Architecture Analysis        │
│   - Scans full project directory & dependencies        │
│   - Generates/updates initial `README.md` & Spec Docs  │
└────────────────────────────────────────────────────────┘
                     │
                     ▼
┌────────────────────────────────────────────────────────┐
│ Stage 2: Comprehensive Test Suite Generation           │
│   - Writes unit, integration & edge-case test files    │
│   - Ensures 100% path coverage for core functions     │
└────────────────────────────────────────────────────────┘
                     │
                     ▼
┌────────────────────────────────────────────────────────┐
│ Stage 3: Verification & Test Execution Loop            │
│   - Runs `cargo test` / `npm test` / `pytest`          │
│   - Auto-fixes code if tests fail (up to 3 attempts)   │
└────────────────────────────────────────────────────────┘
                     │
                     ▼
┌────────────────────────────────────────────────────────┐
│ Stage 4: Documentation Sync & Result Verification      │
│   - Updates `README.md` with exact test results & coverage│
│   - Speaks voice summary & presents visual diff report │
└────────────────────────────────────────────────────────┘
```

### Step-by-Step Execution Breakdown:

1. **Stage 1 (Structure & Spec Docs First)**:
   - Scans the entire project tree (`Cargo.toml`, `package.json`, source files).
   - Establishes the canonical code structure and creates/updates `README.md` and API specifications first.
2. **Stage 2 (Comprehensive Test Case Writing)**:
   - Writes unit tests, integration tests, and edge-case error tests (handling nulls, bounds, invalid inputs).
3. **Stage 3 (Test Execution & Auto-Repair Loop)**:
   - Executes native build/test harnesses (`cargo test`, `npm test`, `pytest`).
   - If any test fails, Aqua's Reflection Engine (Phase 6) intercepts the failure trace, repairs the underlying code, and re-runs the tests until **100% pass rate** is achieved.
4. **Stage 4 (Documentation Synchronization & Final Verification)**:
   - Synchronizes documentation based on test findings, adding code usage examples, test coverage badges, and edge-case notes to `README.md`.
   - Delivers a voice summary (*"Master, I've written 15 unit tests. All tests passed cleanly and documentation has been updated!"*) with visual diff reports on the Tauri UI.

---

## Live Production Project Testing & Deployment Safeguards (`code_generator/live_safety.rs`)

When running tests or generating features for a **Live Production Project** (running in a production environment or deployed server):

### 1. Isolated Staging & Mock Environment Sandbox
- **Zero Production Database Contamination**: Automatically mocks live database connections (`process.env.NODE_ENV = "test"` or SQLite memory DB) so tests **never touch live production data or real customer records**.
- **Network Socket Mocking**: Intercepts external payment gateways (Stripe), email APIs (SendGrid), or production webhooks with mock HTTP stubs (`msw` / `wiremock`) during test execution.

### 2. Live Canary Health Probes & Endpoint Monitoring
- After code passes local staging tests, Aqua executes **Canary Health Probes** (`GET /healthz`, latency metrics, TLS cert checks) against non-destructive staging endpoints.

### 3. Human Approval Gate for Live Code Deployment
- Before pushing changes to a live branch (`main` / `production`) or triggering a live deployment:
  - Aqua intercepts the action via **Phase 5 Privileged Safety Gate**.
  - Renders a complete visual diff & test report on the Tauri UI dashboard.
  - Requires explicit master **Voice Step-Up Authentication** (*"Aqua, approve deployment to live production"*).

### 4. Zero-Downtime Automatic Rollback (`blue_green_rollback.rs`)
- If post-deployment live probes detect a spike in 5xx HTTP errors or failed health checks:
  - Aqua automatically triggers a **Zero-Downtime Rollback** to the previous stable release hash within **<2 seconds**.
  - Speaks a voice alert: *"Live health probe detected errors post-deployment. Reverted production to stable commit successfully."*
