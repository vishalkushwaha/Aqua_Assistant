# Multi-Device Model Context Protocol (MCP) Suite (`adapters/mcp_adapters/`)

This directory houses the MCP server adapters for device control, web automation, social media interaction, and remote hardware access.

**What is in this crate today (2026-10-06):**

| Folder or file | What it is |
| :--- | :--- |
| `browser/` | The managed browser: a Playwright MCP backend, the site policy, confirmations, observation and extraction, the audit trail |
| `browser_mcp/` | The opt-in browser-extension backend (Browser MCP 0.1.3) and `social_watchdog.rs` (a stub that logs and returns true) |
| `windows_mcp/` | The desktop: a Windows-MCP client, the guard that allows 14 UI tools and refuses shells, and the tool provider |
| `stdio_mcp.rs`, `tool_lock.rs` | The MCP stdio client, and the record of what each server says its tools are (a change is reported) |
| `app_launch/`, `local_files.rs`, `file_index/`, `shell_open.rs` | Opening an app by name, finding and opening files and programs, opening a web page without a shell |
| `full_access.rs`, `job.rs` | The "full access" switch, and the Windows Job Object that ends a server with Aqua |

**Not in the tree** (the list below is the original design): `android_mcp/`, `mac_mcp/`, `ide_mcp/`, `ssh_device_mcp/`, `iot_mcp/` and `document_vision_mcp/` do not exist. SSH access is a tool of the tool host (`ssh.*`), not an MCP server. Upgrading the two servers that exist: [`documentation/MCP_UPGRADES.md`](../../documentation/MCP_UPGRADES.md).

## Subdirectories & Adapter Portfolio (design: only `windows_mcp/` and `browser_mcp/` exist)
- `windows_mcp/`: Windows native UI automation, process execution, sticky notes, and system controls.
- `android_mcp/`: Android phone and device management via ADB bridge and remote agent protocol.
- `mac_mcp/`: macOS automation via AppleScript, System Events, and native macOS shortcuts.
- `browser_mcp/`: Web search (Google), social media interaction, web scraping, Playwright automation, and LinkedIn career agent (`social_watchdog/`).
- `ide_mcp/`: IDE & coding orchestrator integration for VS Code, **Claude Code**, **OpenCode**, and terminal development tools.
- `ssh_device_mcp/`: Remote device access over SSH and TCP sockets (controlling remote laptops, IP cameras, connected hardware).
- `iot_mcp/`: Smart Home, lights, MQTT, HomeAssistant, and local smart device control.
- `document_vision_mcp/`: Local OCR (PP-OCR) and Vision VLM (Moondream2 GGUF) extraction for receipts, mobile PDFs, and screenshots.

---

## Step 4.1: Human-in-the-Loop Safety Gate & Security Evaluator (`security_gate/`)

The **Security Evaluator** acts as a strict firewall between Task Routing and Task Execution, enforcing the following 4 critical protection protocols:

### 1. Risk Matrix & Approval Protocols
- **Low Risk (Auto-Approved)**: Read-only operations (checking time, searching local documentation, reading sticky notes, viewing PDFs). Executed immediately.
- **Medium Risk (Soft Confirmation)**: Modifying local scratch files, starting dev servers, editing non-critical code. Requires a quick UI notification with a 5-second auto-proceed countdown.
- **High Risk (Mandatory Master Approval)**: Applying to jobs on LinkedIn, posting social media updates, running destructive terminal commands (`rm -rf`, system formatting), or making financial transactions. Pauses execution completely until explicit master voice (*"Approve"*) or UI button click is received.

### 2. Voice Re-Authentication on High-Stakes Actions ("Step-Up Auth")
- When a high-risk operation is triggered (e.g. submitting a job application), asking simple confirmation is not enough if someone else is nearby.
- **Fix**: Aqua requires **Voice Step-Up Re-Authentication**. The user must speak the explicit approval phrase (*"Aqua, confirm job submission"*), which is immediately re-verified against the ECAPA-TDNN master voice fingerprint.

### 3. Approval Timeout & Safe Degradation
- If a high-risk confirmation prompt receives no user response within **30 seconds**:
  - The task automatically times out and transitions to `CANCELLED_TIMED_OUT`.
  - Aqua emits a short voice notification (*"Action timed out without approval and was cancelled"*) and clears sensitive state from RAM.

### 4. Interactive Dry-Run / Diff Preview
- Before executing complex or destructive actions (such as overwriting files or submitting web forms):
  - Aqua generates a **Visual Dry-Run Preview** on the Tauri UI (showing exact diffs, target URLs, or form fields filled).
  - The user can inspect the exact payload before granting approval.

---

## Step 4.2: MCP Router & Core Module Adapter Architecture (`mcp_router/`)

The **MCP Router** manages bi-directional JSON-RPC protocol communication between Aqua's Rust core engine and internal/external capability providers:

### 1. Heartbeat Monitor & Self-Healing Child Process Supervisor
- **Ping/Pong Heartbeat**: Maintains a 5-second `ping/pong` check over IPC stdout/stdin for all external MCP child processes (`browser_mcp`, `ide_mcp`, `android_mcp`).
- **Auto-Restart**: If an MCP process crashes or hangs, the supervisor terminates the dead process and restarts it within **500ms** before re-submitting the task payload.

### 2. Multi-MCP Directed Acyclic Graph (DAG) Workflow Engine & Real-Time Event Bus
- Orchestrates complex multi-step tasks requiring multiple MCP servers (e.g. *"Read receipt with VLM, extract amounts, log to Excel via Windows MCP, and post report via Browser MCP"*).
- **Real-Time Inter-MCP Event Bus (`mcp_event_bus.rs`)**: Enables real-time pub/sub communication between independent MCP servers. For example, `browser_mcp` streams live scraping events directly to `windows_mcp` or `ide_mcp` over shared memory Unix domain sockets / named pipes without routing delays.
- Builds a sequential DAG dependency graph, automatically feeding the output payload of Step N as the input JSON schema to Step N+1.

### 3. Modular & Reusable Interface Contract (`mcp_schema/`)
- Every MCP adapter adheres to a standardized **JSON-RPC 2.0 Interface Specification**.
- New adapters (e.g., custom hardware drivers, smart home devices, cloud CLI tools) can be plugged in without modifying core system logic.

### 4. Alternate Adapter Fallback & Graceful Degradation
- If a target MCP server is unreachable (e.g., Android phone disconnected via ADB), the router dynamically shifts execution to an alternative adapter (e.g., Desktop `browser_mcp`) or provides a clear voice diagnostic (*"Android device unavailable, falling back to desktop browser"*).

### 5. Cross-Platform Canonical Path Resolver
- Canonicalizes all file paths and URIs across Windows (`E:\...`), macOS (`/Users/...`), Linux, and Android (`/sdcard/...`) before sending JSON-RPC messages, ensuring zero path mismatch errors across operating systems.

---

## Master MCP Selection & Output Rules Matrix (`mcp_router/rules_matrix.json`)

The **MCP Router** follows a deterministic decision rulebook to select the right MCP tool, construct JSON-RPC payloads, and parse output results for every user trigger:

| Trigger Scenario / Voice Command | Target MCP Server | Specific MCP Tools Called | Input Payload Construction | Expected Output Format & Post-Processing |
| :--- | :--- | :--- | :--- | :--- |
| *"Take a note..."* / *"Read my notes"* | `windows_mcp` | `create_sticky_note`, `read_sticky_notes` | `{ "content": "..." }` | Text string logged to OS Sticky Notes; returns status `SUCCESS` to TTS. |
| *"Play music on YouTube"* / *"Pause song"* | `browser_mcp` | `navigate_url`, `click_element` | `{ "url": "youtube.com/results?search=...", "action": "play" }` | Playwright DOM handle; streams playback state event `MEDIA_PLAYING` to UI. |
| *"Tailor my resume for Google job posting"* | `browser_mcp` + `career_adapter` | `scrape_page_text`, `tailor_resume` | `{ "job_url": "...", "master_profile": "data/secure_db/master_profile.enc" }` | Extracted Job Description JSON $\rightarrow$ NLI Entailment Validator $\rightarrow$ Tailored PDF in `data/resumes/`. |
| *"Apply for Senior Rust Developer on LinkedIn"* | `browser_mcp/career_agent` | `fill_form_fields`, `upload_file`, `submit_form` | `{ "resume_pdf": "...", "form_mapping": "{...}" }` | Requires **Voice Step-Up Auth** $\rightarrow$ Fills DOM fields $\rightarrow$ Returns `APPLICATION_SUBMITTED` log. |
| *"Create a React + Rust shopping website"* | `ide_mcp` + `code_generator` | `scaffold_project`, `write_file`, `run_terminal_cmd` | `{ "prompt": "...", "target_dir": "..." }` | Creates workspace files $\rightarrow$ Runs `cargo check` & `npm run build` $\rightarrow$ Verified 0 exit code. |
| *"Summarize this invoice receipt PDF/image"* | `document_reader` + `vision_adapter` | `extract_ocr_text`, `parse_vlm_fields` | `{ "file_path": "receipt.pdf" }` | PP-OCR + Moondream2 VLM $\rightarrow$ Extracted JSON `{ vendor, total, line_items }` $\rightarrow$ Rendered UI table. |
| *"Check social feed for AI updates"* | `browser_mcp/social_watchdog` | `poll_feed_items`, `extract_notifications` | `{ "interval_mins": 15 }` | Background Playwright daemon $\rightarrow$ Filters new posts $\rightarrow$ Pushes proactive voice interrupt if high priority. |
| *"Backup system settings and databases"* | `windows_mcp` / `mac_mcp` | `create_encrypted_snapshot` | `{ "destination": "data/backups/" }` | AES-256 encrypted snapshot ZIP $\rightarrow$ SHA-256 verification hash logged to audit ledger. |
