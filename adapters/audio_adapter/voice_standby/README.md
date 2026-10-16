# Voice Standby Subsystem Specification (`adapters/audio_adapter/voice_standby/`)

## 1. Subsystem Overview
The **Voice Standby Subsystem** is an ultra-low-power, local-only audio monitoring pipeline that runs continuously in the background of Project Aqua. It maintains zero network transmission, consumes under **1% CPU utilization**, and stays within a **15MB RAM baseline** during idle operation.

---

## 2. Technical Architecture & Data Flow

```
[ OS Boot / System Tray Autostart ] ──► [ OS Lock/Session Watcher (Pauses on Lock Screen) ]
          │
          ▼
[ Hardware Microphone / Headset ] ────► [ Acoustic Echo Cancellation (AEC `aec.rs`) ]
          │                                   └─► Subtracts speaker output from mic input
          ▼
[ Audio Device Hot-Plug Watcher (`device_watcher.rs`) ]
          │ (Continuous 16kHz 16-bit PCM Mono Stream)
          ▼
[ Audio Buffer & Ring Queue (`cpal` / Rust `ringbuf`) ]
          │
          ▼
[ DeepFilterNet CPU Noise Suppression (~5–15 ms) ]
          │
          ▼
[ ONNX Wake Word Engine ("Hey Aqua" / "Aqua") ]
          │
          │ (Keyword Pattern Matched!)
          ▼
[ Silero VAD (Voice Activity Detection ONNX CPU) ] ────► Isolates User Voice Utterance
          │
          ▼
[ Speaker Verification (ECAPA-TDNN ONNX Engine) ]
          │
          ├── Initial Boot (No Profile Exists) ──► Trigger First-Time Admin Enrollment Wizard
          ├── Match Authorized Master User (>= 0.82) ──► Emit Visual Wave Event & Trigger Greeting
          └── Unauthorized Voice (< 0.82) ───────────► Remain Silent & Inactive (Log to Audit)
```

---

## 3. Key Components & Specifications

### 3.1 Audio Capture, AEC & Device Hot-Plug Watcher
- **Sampling Rate**: 16,000 Hz, 16-bit Linear PCM, Mono.
- **Acoustic Echo Cancellation (`aec.rs`)**: Real-time echo subtraction allowing Aqua to hear user wake words even while the agent is speaking or playing music.
- **Audio Hot-Plug Manager (`device_watcher.rs`)**: Detects USB mic / Bluetooth headset connect/disconnect events in real time and automatically re-binds the audio stream without restarting Aqua.
- **OS Lock Screen & Session Watcher (`session_watcher.rs`)**: Detects Windows/macOS lock screen events and pauses audio sampling while system is locked, automatically resuming upon OS unlock.
- **Buffer Architecture**: High-efficiency lock-free Rust ring buffer (`ringbuf`) keeping a sliding 3-second audio window.

### 3.2 Noise Suppression & Cleaning (`DeepFilterNet`)
- **Latency Target**: 5–15 ms real-time frame processing.
- **CPU Footprint**: <0.5% CPU utilization running ONNX INT8 SIMD quantized weights.
- **Multi-Stage Spectral Filtering**:
  - *Stationary Noise Removal*: Eliminates background PC fan noise, air conditioner hum, and electronic mic hiss.
  - *Non-Stationary Noise Suppression*: Filters keyboard mechanical clicks, door slams, traffic noise, and background TV audio.
  - *Room Dereverberation*: Removes acoustic room echo and wall reflections in large or uncarpeted rooms, preserving clean vocal formants.
- **Dynamic Bypass & Power Saver**: Continuously measures input Signal-to-Noise Ratio (SNR). If room noise is below $-45\text{dB}$ (pristine quiet environment), DeepFilterNet automatically bypasses filtering to save CPU cycles.
- **Downstream Optimization**: Feeds pristine 16kHz mono audio directly into openWakeword ONNX, Silero VAD, and ECAPA-TDNN speaker verification.

### 3.3 Wake Word Engine (`wake_word/`)
- **Model**: ONNX quantized INT8 neural keyword spotter trained specifically for trigger phrases:
  - `"Hey Aqua"`
  - `"Aqua"`
- **Idle CPU**: <0.8% on standard modern dual-core/quad-core CPUs.
- **Memory Footprint**: ~12–15 MB RAM.

### 3.4 Voice Activity Detector (`Silero VAD`)
- **Function**: Dynamically detects exact speech start (`speech_started`) and trailing silence (`speech_ended`).
- **Adaptive Silence Timeout**:
  - Fast queries (e.g., *"What time is it?"*): 500ms trailing silence cut-off.
  - Complex multi-clause queries or dictation: Automatically extends silence timeout to **1200ms–1500ms** if mid-sentence pauses occur.
- **Dynamic Energy Floor & SNR Calibration**: Continuously tracks RMS ambient noise floor to dynamically calibrate confidence thresholds for whispers vs. noisy environments.
- **Transient Noise Filtering**: Discards speech segments under **150ms** (coughs, pops, keyboard clicks).
- **Safety Cutoff Cap**: Enforces a strict **15-second maximum recording limit** to prevent memory buffer overflow during continuous background noise.

### 3.5 Speaker Verification & Admin Fingerprint Panel (`speaker_verification/`)
- **First-Time Setup**: On initial launch, if no master voice fingerprint exists in `data/secure_db/`, launches the **First-Time Admin Voice Calibration Wizard**.
- **Adaptive Voice Profile Calibration (`adaptive_profile.rs`)**: Seamlessly updates rolling average voice profile embeddings when high-confidence ($\ge 0.90$) authentications occur.
- **Admin Panel**: Allows the admin to record, calibrate, update, or replace the master voice fingerprint anytime. Stored encrypted (AES-256-GCM) in `data/secure_db/`.
- **Model & Checksum Verification**: Validates SHA-256 hashes of all ONNX/GGUF model files at startup to prevent model corruption.
- **Panic Hook & Self-Healing Watchdog (`watchdog.rs`)**: Catches audio driver exceptions or thread panics silently, dumps diagnostic traces to `data/logs/crash.log`, and automatically restarts the standby loop within 1 second.
- **Strict Verification Logic**:
  - Converts incoming voice chunk into a 192-dimensional vector.
  - Computes Cosine Distance against the stored Master Voice Profile.
  - **Threshold**: Similarity $\ge 0.82$ required to wake up.
  - **Mismatch**: Mismatched or unauthorized voices are strictly ignored; Aqua remains **100% silent and inactive**.

### 3.6 Local Speech-to-Text Transcription Engine (`whisper-rs`)

The **Speech-to-Text (STT) Subsystem** converts user speech into clean text using `whisper.cpp` C++ core bindings via Rust (`whisper-rs`):

#### 1. Zero-Copy Rust FFI Binding (`whisper.cpp`)
- Calls `whisper.cpp` C++ engine directly via Rust FFI without Python overhead, achieving sub-100ms transcription latency.

#### 2. Anti-Hallucination Decoding Guard
- Prevents small models from hallucinating repetitive strings during quiet audio:
  - `suppress_blank = true`
  - `no_speech_threshold = 0.6`
  - `entropy_threshold = 2.4`
  - `logprob_threshold = -1.0`

#### 3. High-Performance SIMD Resampler (`rubato`)
- Converts hardware audio streams (44.1kHz / 48kHz stereo) directly into 16kHz float32 mono in-memory using Rust `rubato` SIMD kernels, adding zero runtime resampling delay.

#### 4. Thread Pool Lock & CPU Core Protection
- Constrained strictly to **2 physical CPU threads** using AVX2 / AVX-512 / ARM NEON vectorization to prevent CPU spikes or system UI stutters.

#### 5. Adaptive Dual-Model Promotion & Token Confidence Scoring
- **Primary Fast Path**: `whisper-tiny.en` INT8 (<80ms execution speed).
- **Token Confidence Audit**: Evaluates average token log-probabilities. If confidence score $< 0.70$ (e.g. muffled voice, technical terms, accent), automatically re-promotes inference to `whisper-base.en` INT8 (<180ms execution speed) for 100% transcription accuracy.

### 3.7 Text Sanitization & Entity/Slot Extractor (`sanitizer/`)
- **Fast Disfluency & Prefix Stripping**: Removes remaining wake words (*"hey aqua"*, *"aqua"*) and verbal filler stutters (*"um"*, *"uh"*, *"like"*) using compiled Rust regex trie trees in $< 1\text{ms}$.
- **Phonetic Entity Restoration**: Fixes homophone transcription errors for technical terms (e.g. *"sea plus plus"* $\rightarrow$ *"C++"*, *"react js"* $\rightarrow$ *"React.js"*, *"dot pdf"* $\rightarrow$ *".pdf"*).
- **Relative Temporal Resolver**: Converts spoken relative terms (*"tomorrow at 5"*, *"in 15 minutes"*, *"next Monday"*) into absolute ISO-8601 timestamps anchored to the local OS system clock.
- **Context-Aware Entity & File Path Matcher**: Matches spoken file names against recent workspace LRU cache (e.g. *"my resume"* $\rightarrow$ `e:\Personal_AI\Aqua\career\resume.pdf`).
- **Empty / Garbage Output Guard**: Discards transcripts containing zero semantic tokens post-sanitization without waking up intent classification or LLM modules.

### 3.8 Dual-Engine Intent Router (`intent_router/`)
- **Tier 1: Fast-Path Pattern Matcher (< 2ms)**:
  - Uses compiled Regex Trie lookup structures in Rust.
  - Matches 100% deterministic voice commands directly without calling AI models:
    - *System Clock / Time Check*: *"What time is it?"*, *"Tell me the time"* $\rightarrow$ Immediate local system clock response.
    - *Windows Sticky Notes MCP*: *"Take a note..."*, *"Read my notes"* $\rightarrow$ Direct `windows_notes_mcp` invocation.
    - *Browser Media MCP*: *"Play music on YouTube"*, *"Pause song"* $\rightarrow$ Direct `browser_mcp` invocation.
- **Tier 2: Laya_Rust CPU Intent Classifier (< 15ms)**:
  - Uses a lightweight 5MB INT8 vector embedding space + Cosine SVM inside Rust core.
  - Classifies complex queries into target module execution categories: `CAREER_INTELLIGENCE`, `CODE_GENERATION`, `DOCUMENT_READER`, `BROWSER_WATCHDOG`, `IMAGE_GENERATOR`, `SYSTEM_HARDWARE`.
- **Multi-Intent Sentence Chaining**: Detects compound sentences connected by conjunctions (e.g. *"Take a note about the meeting AND set a reminder for 5 PM"*) and splits them into ordered sequential tasks in a single pipeline run.
- **Active UI / Application Context Bias**: Dynamically boosts intent probabilities based on the user's currently focused desktop application (e.g., if VS Code is focused, boosts `CODE_GENERATION`; if Chrome/LinkedIn is focused, boosts `CAREER_INTELLIGENCE`).
- **Destructive Operation Safety Gate**: Intercepts high-risk intents (e.g., system deletion, posting to social media, applying for jobs) and forces a `REQUIRES_HUMAN_CONFIRMATION` gate flag in the payload before dispatching to Phase 4.
- **Dynamic Hot-Reloadable Intent Patterns**: Pattern trie dictionaries in `data/intent_patterns.json` can be edited by the user or updated by plugins without recompiling or restarting Aqua.
- **Ambiguity Clarification Protocol**: If classification confidence $< 0.65$, triggers an explicit voice clarification prompt (*"Did you mean update your resume or start code generation?"*) instead of guessing.

### 3.9 Task Payload Construction & Dispatch (`task_dispatcher/`)
- **Event Bus Contract**: Serializes clean transcript, extracted slots, intent category, voice hash token, and safety confirmation status into a strongly typed `AquaTask` struct.
- **Priority Queue Architecture**:
  - *HIGH Priority Queue*: Instant audio/media controls (*"pause"*, *"mute"*, *"stop alarm"*), bypassing standard task backlog.
  - *NORMAL Priority Queue*: Interactive career, code generation, and document queries.
  - *LOW Priority Queue*: Background system backups, social media watchdog sync, and log maintenance.
- **Backpressure & Bounded Channel Safety**: Bounded Tokio MPSC queues with a hard limit of **100 tasks**. If queue fills up, rejects low-priority tasks with a voice warning (*"System busy, task queued"*) to prevent out-of-memory worker crashes.
- **Task Preemption & Cancellation Token**: Attaches a `CancellationToken` to every dispatched task payload, allowing the user to say *"Aqua, cancel that"* to instantly abort any running execution in Phase 4.
- **Payload Data Privacy & Encryption**: Sensitive payload slots (such as user credentials, API keys, or resume metadata) are encrypted in RAM using AES-128-GCM before queue dispatch to prevent memory inspection attacks.
- **UI Visual State Synchronization**: Emits an instant WebSocket event to the Tauri UI frontend (`TASK_DISPATCHED`) to render an active visual pulse indicator before execution starts.
- **Audit Telemetry & RL Memory**: Asynchronously appends every dispatched intent (timestamp, confidence score, target module) to `data/logs/intent_audit.jsonl` for offline reinforcement learning feedback.

### 3.10 Output Streaming, Voice TTS Engine & Audio Synthesizer (`tts_engine/`)

The **TTS Subsystem** provides natural, ultra-low-latency voice responses running strictly on CPU:

#### 1. Streaming Clause-Boundary Phoneme Synthesizer (`piper_onnx.rs`)
- **Engine**: Local **Piper TTS ONNX CPU** engine (<15MB RAM baseline).
- **Sub-150ms Speech Start**: Buffers incoming streaming LLM/Agent text tokens until a natural sentence or clause boundary (`.`, `?`, `!`, `,`, `;`) is detected. Instantly synthesizes and streams the first clause while subsequent text is still generating, achieving sub-150ms initial speech latency without robotic cadence breaks.

#### 2. Code vs. Voice Dual-Channel Splitter (`markdown_cleaner.rs`)
- **UI Stream Channel**: Sends raw Markdown, syntax-highlighted code blocks, diffs, tables, and visual artifacts to the Tauri frontend.
- **Voice Stream Channel**: Filters out code fences, raw file paths, URLs, and stack traces, converting them into natural spoken summaries (e.g. converting ```` ```rust fn main() ... ``` ```` to *"Generated the Rust component and verified the build cleanly"*).

#### 3. Acoustic Echo Cancellation (AEC) Barge-In & Voice Interruption
- Keeps WebRTC AEC3 echo cancellation active during speaker output.
- If the user speaks (*"Aqua, stop"*, *"Cancel"*), the audio buffer player immediately stops (`tts_player.stop()`), flushes the audio queue, and switches to Standby VAD mode in $<50\text{ms}$.

#### 4. Expressive Emotion & SSML Tag Parser
- Parses pitch, speed, and emotion tags (e.g. `<speak speed="1.1" pitch="+5%">`) to deliver natural human cadence variations for alerts vs. regular conversation.

#### 5. Audio Output Device Hot-Swapper
- Continuously monitors default system audio output devices (speakers, Bluetooth headphones, USB headsets).
- If headphones are unplugged during active speech, seamlessly shifts the audio stream to the new output device without stuttering or crashing the TTS engine.

#### 6. Multi-Tier Dynamic TTS Fallback
- **Tier 1 (Primary)**: High-quality local **Piper TTS ONNX CPU** engine.
- **Tier 2 (System Native)**: Native OS Speech Synthesis APIs (Windows SAPI5, macOS AVFoundation, Linux Speech Dispatcher).
- **Tier 3 (Silent Mode)**: Renders output directly as a visual UI text toast notification if audio devices are unavailable.
