# Career & Resume Intelligence Subsystem (`core/src/career/`)

> **Code status (2026-10-08): written, not wired.** `CareerEngine` has tests, and nothing outside them calls it ([who calls what](../../../documentation/competitive/refresh/10_DEAD_CODE_CALLERS.md)). The text below describes the design, not what runs in Aqua. There is no register entry.

This directory contains the self-learning career assistant modules for Project Aqua.

## Subdirectories
- `resume_parser/`: PDF/DOCX resume parser extracting structured education, skills, work experience, projects, and certifications into local vector RAG memory (`core/src/memory/`).
- `preferences/`: Career preference engine managing job constraints (roles, minimum CTC, strict remote/onsite filtering, notice period, target locations, companies to avoid).
- `resume_adapter/`: Job-specific resume customization engine. Creates tailored resume variants for target jobs highlighting genuine matching experience. Hard-coded with a **Strict Zero Fabrication Policy** (never invents false skills or qualifications) and presents modifications for user approval.

---

## Step 5.1: Zero Fabrication Integrity Guard (`truth_checker/`)

The **Zero Fabrication Integrity Guard** mathematically guarantees that Aqua never fabricates, inflates, or distorts facts across resumes, job applications, or system outputs:

### 1. NLI Semantic Entailment Validator (`entailment_validator.rs`)
- Uses a lightweight Natural Language Inference (NLI) model on CPU to verify every tailored bullet point (hypothesis) against the original master profile (premise).
- Enforces a minimum entailment score of $\ge 0.92$. Rejects inflated claims (e.g. converting *"basic script writing"* to *"Senior Lead Architect"*).

### 2. Dynamic Profile Update Wizard
- If a target job requires a skill missing from `master_profile.json`, Aqua does not invent it or drop it.
- Prompts the master user via voice: *"This job requires Docker, which is not in your master profile. Do you have Docker experience I should add?"* If confirmed, updates `master_profile.json` safely.

### 3. Immutable Temporal & Title Boundaries
- Hardcodes job titles, company names, employment date ranges (month/year), and degree institution names as **IMMUTABLE**.
- Automatically blocks any resume variant that alters employment dates, inflates titles, or disguises employment gaps.

### 4. Universal Grounded Fact Validator
- Extends factual grounding across all modules: document readers must cite exact PDF page/line numbers, and system queries must return actual OS kernel telemetry.

---

## Step 5.2: Missing Data Pause Protocol (`pause_protocol/`)

The **Missing Data Pause Protocol** manages incomplete task executions safely without making assumptions or losing state:

### 1. Pre-Execution Slot Completeness Audit
- Audits all required parameter slots (target salary, location, URL, input files) **before** initiating execution.
- If essential slots are missing, enters `PAUSED_AWAITING_INPUT` upfront instead of failing midway through execution.

### 2. Persistent Task Draft Serialization (`data/drafts/`)
- Immediately serializes paused task states to disk (`data/drafts/task_{id}.json`) encrypted with AES-256-GCM.
- Automatically recovers paused drafts upon OS boot or app restart, allowing the user to resume seamlessly.

### 3. Dual-Channel Input Listener (Voice + UI Modal)
- Binds paused task listeners simultaneously to **both** the voice STT stream and a pop-up UI input modal in Tauri.
- Whichever channel receives input first satisfies the missing parameter and resumes task execution.

### 4. 60-Second Auto-Stash & Queue Unblocker
- If no user response is received after 60 seconds, speaks a short notification (*"Saved task draft to pending items"*), moves the draft to `PAUSED_STASHED` state on disk, and unblocks the worker queue for subsequent tasks.

---

## Step 5.3: Privileged Operation Safety Gate & Transactional Rollback (`privilege_gate/`)

The **Privileged Operation Safety Gate** manages security authorization, non-repudiation, and atomic failure recovery for high-stakes tasks:

### 1. Two-Phase Commit (2PC) Execution Protocol (`two_phase_commit.rs`)
- **Phase 1 (Prepare)**: Prepares local workspace files, builds code, and validates form inputs in isolation without producing external side-effects.
- **Phase 2 (Commit)**: Executes irreversible external operations (submitting job applications, posting web forms, deleting files) **last** as a single atomic step once all local checks pass 100%.

### 2. Deep Capability-Based Privilege Inspector (`capability_inspector.rs`)
- Inspects leaf-level OS process calls, network sockets, and file mutation targets rather than surface-level voice text.
- Prevents script injection or macro chaining from bypassing confirmation gates if a privileged capability is touched.

### 3. ED25519 Cryptographically Signed Audit Ledger (`audit_signer.rs`)
- Cryptographically signs every privileged action approval using an internal ED25519 key pair.
- Appends log records (timestamp, voice fingerprint similarity score, user ID, target action hash) to an append-only ledger (`data/logs/audit.ledger`) to guarantee non-repudiation.

### 4. Emergency Kill-Switch & Master Interrupt Protocol
- Listening continuously for `Ctrl + Shift + Esc` hotkey or spoken phrase *"Aqua, Emergency Stop"*.
- Instantly sends SIGKILL to all active worker processes, closes browser contexts, isolates network sockets, and restores workspace to the last clean snapshot within **<100ms**.

---

## Step 5.4: Transactional Rollback & Error Isolation (`rollback_manager/`)

The **Rollback Manager** provides atomic, zero-residue failure isolation for complex task executions:

### 1. Workspace Shadow Staging Area (`.aqua/staging/`)
- Conducts file creations and code modifications inside a isolated shadow staging directory (`.aqua/staging/{task_id}`).
- Merges staging changes to the user's main workspace **only upon 100% successful task completion**. Purges the staging directory in $< 10\text{ms}$ on failure.

### 2. OS Process Group Tree Termination (`process_tree.rs`)
- Assigns worker execution tasks to native OS Job Objects (Windows) or Process Groups (Linux/macOS).
- Recursively sends `SIGKILL` to all child sub-processes (dev servers, compilers, browser instances) on task failure to prevent orphaned process or port leaks.

### 3. SQLite WAL Transactional Savepoints
- Wraps all internal SQLite and Vector database updates within `SAVEPOINT task_execution` blocks.
- Issues `ROLLBACK TO SAVEPOINT` on task failure, ensuring database states stay 100% synchronized with the workspace file system.

### 4. Diagnostic Crash Snapshots for Self-Correction
- Captures terminal stdout/stderr, stack traces, and code diffs into `.aqua/crash_reports/{task_id}.json` before executing rollback.
- Automatically routes crash snapshots to **Phase 6 (Continuous Error Learning)** to prevent repeating identical failure patterns.
