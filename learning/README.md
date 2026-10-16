# Learning & Reinforcement Learning Directory

All AI, Machine Learning, and Reinforcement Learning models, environments, experience replay buffers, and training pipelines are isolated within this directory.

## Implementation note (repository state, 2026-10-06)

The **`aqua-core` crate embeds this folder** via `core/src/lib.rs` (`#[path = "../../learning/mod.rs"]`). Most of what follows on this page is a **design**. What exists:

| Artifact | Status |
| :--- | :--- |
| `rl_engine.rs`, `mod.rs` | **Present**: `RlEngine`, policy weights, trajectory steps, with their own tests. **Nothing reads the weights to choose a tool**, and tool calls no longer write to it |
| `experience/policy_weights.json`, `trajectories.json` | Data files of the RL engine. They are not the experience store |
| What actually learns | `core/src/memory/experience/` (verified runs are kept and reused for similar goals, with word-hash similarity), `core/src/memory/agent_learning.rs` (recipes for repeated goals), `core/src/reflection/` (repair hints after a failed call) |
| The trie, the INT8 vector search with a 0.88 threshold, the 30-day expiry, `reflection/log_parser.rs`, `algorithms/`, `training/`, `main_sidecar.py` | **Design only**: not in the tree |

The generated register of what works is [`documentation/CAPABILITIES.md`](../documentation/CAPABILITIES.md).

Solution memory persistence is implemented in **`core/src/memory/solutions_db.rs`** (`data/solutions.db`). See [`documentation/IMPLEMENTATION_STATUS.md`](../documentation/IMPLEMENTATION_STATUS.md).

---

## Directory Layout (target)
- `algorithms/`: Custom & Wrapped RL/ML Algorithms (PPO, SAC, DQN)
- `configs/`: Hyperparameters and Model Schedules
- `environments/`: Gymnasium Custom Task Environments
- `experience/`: Replay Buffers, Vector Memory & Trajectories
- `inference/`: High-speed Runtime Model Execution Engine
- `models/`: Saved Checkpoints, Weights & ONNX Exports
- `training/`: Offline & Online Training Pipelines
- `evaluation/`: Model Performance Metrics & Benchmarks
- `main_sidecar.py`: Python Async Worker Sidecar Entrypoint

---

## Step 4.4: Solution Memory & Experience Caching (`experience/solutions.json`)

The **Solution Memory Engine** indexes verified execution paths and solution patterns to accelerate future task performance:

### 1. Environment Hash Verification & TTL Invalidation
- Stores a SHA-256 hash of the target OS, compiler version, and dependency lockfile alongside each solution.
- Automatically invalidates stale cached entries if the environment changes or the 30-day Time-To-Live (TTL) expires.

### 2. Hybrid Exact Trie + Cosine Semantic Lookup
- **Fast Path (<1ms)**: Exact string/token matching via Rust Trie lookup.
- **Semantic Path (<15ms)**: 384-dimensional INT8 vector cosine similarity search ($\ge 0.88$ threshold) for rephrased queries.

### 3. Atomic File Persistence & Thread-Safe Synchronization
- Uses `Arc<RwLock<SolutionStore>>` in Rust to safely allow concurrent reader/writer access.
- Disk writes use atomic write-and-rename (`solutions.json.tmp` $\rightarrow$ `solutions.json`) to guarantee zero file corruption on system power loss.

### 4. Mandatory Verification Gate
- Only solutions that achieve a **100% clean verification score** (0 compiler error exit code, successful build, or verified output) are saved to `solutions.json`. Failed or unverified attempts are strictly rejected.

---

## Step 6.1: Failure Root-Cause Extractor & Log Parser (`reflection/log_parser.rs`)

The **Log Parser** extracts structured diagnostic data from raw terminal outputs, stack traces, and runtime crash reports:

### 1. Multi-Pass Un-Truncated Log Extractor
- Scans full, un-truncated stdout/stderr build streams (Rust `cargo`, TypeScript `tsc`, Python `pytest`, Playwright) to isolate true root-cause exceptions buried above build noise.

### 2. Universal Multi-Language Error Normalizer
- Normalizes language-specific error outputs (e.g., Rust `E0308`, TS `TS2322`, Python `TypeError`) into a standardized `ErrorSignature` struct containing domain, error code, file path, line number, and primary message snippet.

### 3. Transient Infrastructure Error Filter
- Classifies failures into `PERMANENT_CODE_BUG` vs. `TRANSIENT_INFRASTRUCTURE_FLAKE` (network timeouts, locked file handles, registry rate limits).
- Retries transient errors automatically (up to 3 exponential backoff attempts) before logging to crash reports.

### 4. PII Redactor & Secret Sanitizer
- Automatically scrubs sensitive environment variables, secret tokens, IP addresses, and personal user directory paths from diagnostic crash reports before passing data to reflection engines or RL memory.

---

## Step 6.2: Reflection & Counterfactual Reasoning Engine (`reflection/reflection_engine.rs`)

The **Reflection Engine** formulates verified fixes by analyzing errors and evaluating candidate patches:

### 1. Regression-Aware Test Harness (`reflection_harness.rs`)
- Ensures candidate patches pass **both** the targeted error fix AND the full project unit test suite (`cargo test` / `npm test`), preventing fix-induced code regressions.

### 2. 3-Attempt Max Reflection Budget
- Limits automated self-healing to a strict ceiling of **3 reflection iterations per failure**.
- If 3 attempts fail to yield a clean build, halts reflection, releases RAM, and prompts the user for guidance.

### 3. Abstract Pattern Generalizer
- Converts machine-specific paths, environment variables, and absolute URLs into abstract template variables (`${WORKSPACE_ROOT}`, `${OS_TEMP_DIR}`) before storing fix patterns.

### 4. Counterfactual Validation Pass
- Proves patch causality by confirming the error reproduces when the patch is temporarily un-applied, and resolves when re-applied.

---

## Step 6.3: Solution Memory Indexer & Experience Storage (`experience/solutions.db`)

The **Solution Memory Indexer** manages long-term indexing, weight decay, and retrieval of verified execution patterns:

### 1. SQLite HNSW Vector Indexing & Memory Cap
- Migrates solution storage to embedded **SQLite + HNSW Vector Index (`data/solutions.db`)**.
- Keeps top 100 active patterns in an LRU RAM cache, querying disk for vector search to maintain **<200MB RAM baseline**.

### 2. Multi-Version Solution Branching
- Keys solution patterns by composite index `(Error Signature + Framework/OS Version Hash)`.
- Maintains distinct fix branches so framework-specific syntax changes do not overwrite each other.

### 3. Reinforcement Confidence Weight Decay
- Dynamically assigns confidence score $C \in [0.0, 1.0]$ to every recipe.
- Successful reuse increases $C$ by $+0.05$; execution failure decreases $C$ by $-0.20$. Entries dropping below $C < 0.30$ are automatically purged.

### 4. Static AST Security Scanner
- Audits cached code fix snippets with a static AST security parser prior to execution to ensure zero dangerous system calls or payload injections exist.

---

## Step 6.5: First-Time Auto-Discovery & Instant Re-Use Accelerator (`auto_indexer.rs`)

When the user requests a **brand-new task for the first time**:

### 1. First-Time Search & Solution Discovery Protocol
- Aqua searches available local documentation, workspace code, and registered MCP tools to synthesize the optimal execution plan.
- Runs the task execution through the Verification Harness (`cargo check`, `npm test`, or visual DOM validation).

### 2. Automatic Plan Compilation & Fast-Path Indexing
- As soon as the new task completes successfully with a 100% clean verification score:
  1. Compiles the exact step-by-step execution graph into a lightweight binary macro recipe.
  2. Generates a 384-dimensional vector embedding of the user's intent.
  3. Saves the compiled recipe directly into **`data/solutions.db`** and Tier-1 Fast-Path Regex Trie (`data/intent_patterns.json`).

### 3. Subsequent Request Instant Execution (< 2ms)
- **Next Time the Task is Triggered**:
  - Step 3.4 (Fast-Path Pattern Matcher) instantly matches the intent in **$<2\text{ms}$**.
  - Reuses the pre-compiled binary macro directly without searching, model reasoning, or re-synthesizing steps.
  - Reduces task execution latency by **up to 10x–50x** with zero processing wait time for the user.

---

## Step 6.4: Local RL Agent Policy Update & Telemetry (`learning/training/`)

The **Local RL Policy Trainer** fine-tunes task routing and decision weights offline based on verified user interactions:

### 1. Idle-Only Background Policy Trainer (`idle_trainer.py`)
- Executes PyTorch RL policy training **only** when system idle watcher reports zero user activity for $> 5\text{ minutes}$.
- Constrains PyTorch thread allocation (`torch.set_num_threads(1)`) to ensure zero CPU spikes or fan noise while the user is active.

### 2. Reward Clipping & Anti-Degeneracy Bounds
- Enforces strict reward normalization ($R \in [-1.0, +1.0]$) with PPO advantage clipping.
- Requires a batch size of $\ge 50$ trajectory transitions before updating policy weights, preventing model skew from isolated noisy events.

### 3. SHA-256 Merkle Hash-Chained Audit Ledger (`audit_ledger.rs`)
- Chains each telemetry record with the SHA-256 hash of the previous record to form an immutable append-only ledger (`data/logs/audit.ledger`).
- Prevents log tampering or history modification.

### 4. Zero Cloud Data Leakage (Local-Only Network Sandbox)
- Isolates the Python RL training sidecar inside an OS firewall loopback sandbox (`127.0.0.1` only).
- Strictly guarantees zero telemetry, model weights, or user data ever leave the local machine.

---

## Step 6.6: Dynamic Multi-Model `llama.cpp` Task Engine & Model Router (`core/src/model_manager/`)

The **`llama.cpp` Engine** provides high-speed, 100% offline local LLM inference running strictly on CPU:

### 1. Multi-Model Intent Matrix
```
[ Aqua Task Intent ]
          │
          ├── Chat / Greetings ("Hi", "Hello", Time) ────► 1. Micro Conversational Model (Qwen-0.5B / Phi-3 INT4) [<50MB RAM, <30ms]
          ├── Code Generation ("Create React app") ──────► 2. Coding Specialist Model (DeepSeek-Coder-1.3B INT4) [<400MB RAM]
          ├── Multi-Step Agent Task ("Upload video") ───► 3. Agentic Planner Model (Qwen-2.5-Coder-3B INT4) [<700MB RAM]
          └── Document Vision ("Read receipt") ──────────► 4. Vision Language Model (Moondream2 GGUF) [<300MB RAM]
```

### 2. Low-Memory & SIMD Hardware Optimization
- **`mmap` Memory Mapping**: Loads GGUF model files instantly from disk via memory mapping without duplicating byte buffers in heap memory.
- **K-Quants (Q4_K_M / Q8_0)**: Uses INT4/INT8 K-Quants quantization to reduce model footprint by **up to 75%** while retaining >98% accuracy.
- **AVX2 / AVX-512 / ARM NEON Vectorization**: Uses CPU SIMD matrix kernels to achieve **30–60 tokens per second** execution speed on standard dual-core/quad-core CPUs.

### 3. Dynamic RAM Model Unloader (`model_lifecycle.rs`)
- Enforces an active **Single-Model Load Policy**: Only **ONE** model is loaded into CPU RAM at any instant.
- The moment a task completes, the model handle is dropped and `malloc_trim` is called, restoring RAM to the **<200MB background baseline within 2 seconds**.

### 4. Token-by-Token Real-Time Output Streaming
- Streams generated text tokens in real time directly to the TTS engine (Phase 4.5 sentence-boundary synthesizer) and Tauri desktop UI over Tokio channels.

### 5. Context Shift & Sliding Window Memory
- Uses a sliding context window (4,096 tokens) with KV-cache reuse, ensuring long conversational turns or multi-step agent plans never run out of context.

---

## Step 6.7: Domain-Isolated Learning Storage Layout (`learning/experience/`)

Learning memories, fix recipes, and trajectory buffers are stored in **strictly separated directories** based on domain category:

```
learning/experience/
├── chat_memory/          # Conversational context, user preferences, greetings
├── coding_memory/        # Verified code fixes, compiler patch recipes (`cargo`/`npm`)
├── agent_memory/         # Multi-step MCP execution DAG graphs (YouTube, LinkedIn)
├── vision_memory/        # OCR field mapping rules and receipt parsing schemas
├── integrator_memory/    # Autonomous open-source library search, build recipes, & FFI wrappers
└── solutions.db          # Embedded SQLite + HNSW Vector Index across all domains
```

- **Zero Memory Contamination**: Code fix recipes never pollute conversational chat memory, and browser automation macros stay isolated in `agent_memory/`.
