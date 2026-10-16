# Social Watchdog Background Daemon (`browser_mcp/social_watchdog/`)

> **Code status (2026-10-08): written, not wired.** `social_watchdog.rs` logs and returns true, and nothing outside its tests calls it ([who calls what](../../../../documentation/competitive/refresh/10_DEAD_CODE_CALLERS.md)). The text below is the design. Register: `sim.social_watchdog` (simulated). Reading the comments under a post and drafting replies is built as flow steps in `core/src/flows/social/` and `core/src/flows/comments_inbox.rs`.

Runs an asynchronous Playwright background watcher every 10–15 minutes to monitor social media posts.

## Features
- **Passive Monitoring**: Periodically checks published posts for new comments, likes, and direct messages without disturbing the user.
- **Proactive Voice Interrupt**: When a comment is detected, triggers a gentle voice prompt: *"Hey! You got a new comment on your post. Want to see it or can I read it for you?"*.
- **Interactive Voice Reply**: Supports detail filtering (Usernames Only, Complete Details, Summary), captures user dictation, drafts response via `llama.cpp`, and posts reply via Playwright upon voice confirmation ("Send it").

---

## Autonomous First-Time Task Learning & Re-Use Case Study: YouTube Video Upload Workflow

### Scenario: First-Time Command ("Aqua, upload my video from the project folder to YouTube")

```
[ User Command: "Upload video from my folder to YouTube" ]
                           │
                           ▼
┌────────────────────────────────────────────────────────────────────────┐
│ Step 1: Web Search & Strategy Synthesis                                │
│   - `browser_mcp` searches Google ("How to upload video on YouTube")   │
│   - Extracts DOM steps: Studio Login -> Create -> Upload -> Title/Desc  │
└────────────────────────────────────────────────────────────────────────┘
                           │
                           ▼
┌────────────────────────────────────────────────────────────────────────┐
│ Step 2: Multi-MCP Pipeline Execution                                   │
│   - `windows_mcp`: Scans local folder for video (.mp4) & thumbnail (.png)│
│   - `browser_mcp` (Playwright): Opens YouTube Studio & clicks Upload    │
│   - `laya_rust` + Local LLM: Generates Title & SEO Description        │
└────────────────────────────────────────────────────────────────────────┘
                           │
                           ▼
┌────────────────────────────────────────────────────────────────────────┐
│ Step 3: Missing Variable Pause Protocol (If thumbnail/title missing)   │
│   - Pauses & asks master user: "What title & thumbnail should I use?"  │
│   - Receives voice/UI input & continues execution                      │
└────────────────────────────────────────────────────────────────────────┘
                           │
                           ▼
┌────────────────────────────────────────────────────────────────────────┐
│ Step 4: Verification & Fast-Path Macro Caching                         │
│   - Detects final "Video Uploaded & Published" DOM indicator           │
│   - Compiles recipe to `data/solutions.db` & `data/intent_patterns.json`│
│   - Speaks TTS: "Your video 'Cyberpunk AI' is successfully uploaded!"  │
└────────────────────────────────────────────────────────────────────────┘
                           │
                           ▼
[ SUBSEQUENT RUNS: Instant Fast-Path Re-Use (<2ms) - Zero Web Search Wait! ]
```

### How Subsequent Requests Run Without Delay:
1. **Next Time You Say**: *"Upload the new video in my folder to YouTube"*.
2. **Fast-Path Match (<2ms)**: `intent_router` recognizes the compiled macro in `solutions.db`.
3. **Zero Web Search**: Skips Google searching entirely. Directly launches Playwright, grabs the video file, applies auto-generated metadata, requests step-up voice approval, and publishes.
4. **Final Confirmation Output**: Speaks voice confirmation (*"Master, your video has been successfully published to YouTube!"*) with direct URL link rendered in the Tauri UI.

---

## Persistent Browser Sessions, Auto-Login & CAPTCHA Solver Protocols (`browser_mcp/auth/`)

To prevent repeated logins and handle web security challenges during browser automation:

### 1. Persistent Browser Profile & Cookie Vault (`user_data_dir/`)
- Playwright uses a persistent browser profile stored in `data/browser_profile/`.
- Once logged into Google, YouTube, LinkedIn, or GitHub, cookies, OAuth tokens, and session states stay logged in permanently across app restarts.

### 2. Automated Google Login & Credential Injection Handler (`auto_login.rs`)
- If Playwright detects a logged-out state (`"Sign in"` DOM button present):
  - Fetches encrypted credentials from `data/secure_db/` (stored via AES-256-GCM).
  - Automatically types username and password into Google login fields with human-like typing delays (50–120ms key interval).

### 3. Dual-Layer CAPTCHA Solver Protocol (`captcha_solver.rs`)
- **Layer 1: Automated Local Vision/OCR CAPTCHA Solver**:
  - Uses `vision_adapter` (PP-OCR + Moondream2 VLM CPU) to solve text image CAPTCHAs and puzzle slider offsets automatically.
- **Layer 2: Interactive Human-in-the-Loop CAPTCHA Gate (reCAPTCHA / hCaptcha / 2FA)**:
  - If a 2FA prompt (Google Authenticator prompt on phone) or complex reCAPTCHA appears:
    - Aqua pops up a interactive browser viewport window on the Tauri UI.
    - Speaks a voice prompt: *"Google requires 2FA approval on your phone or CAPTCHA verification. Please complete it on the screen."*
    - Automatically detects when CAPTCHA/2FA passes, closes the pop-up, saves updated cookies to `data/browser_profile/`, and seamlessly resumes task execution.

---

## Future-Proof Master Edge-Case & Web Resiliency Matrix (`browser_mcp/resiliency.rs`)

To ensure Project Aqua handles **all real-world web variations** without requiring future code rewrites:

| Real-World Failure / Edge Case | Cause / Problem | Automated Solution / Future-Proof Protection |
| :--- | :--- | :--- |
| **1. UI Layout & Class Name Changes** | Website changes button IDs or CSS selectors (e.g., YouTube updates `.upload-btn` to `#create-video`). | **VLM Vision-Based DOM Locator**: If CSS selectors fail, uses Moondream2 VLM CPU to find buttons visually by screen coordinate bounding boxes. |
| **2. Dynamic Network Disconnects** | Internet drops mid-upload on step 3 of 5. | **Resumable Chunk Staging & Network Retry**: Retries connection with exponential backoff (1s, 2s, 4s, 8s); resumes upload from chunk index without starting over. |
| **3. Anti-Bot Cloudflare / Akamai Blocks** | Cloudflare JS challenges or TLS fingerprinting blocks Playwright. | **Stealth Profile & CDP Fingerprint Masking**: Uses `playwright-stealth` + real Chrome User Agent, WebGL spoofing, and realistic mouse trajectory curves. |
| **4. Account Security Suspensions** | Too many automated requests trigger temporary account flags. | **Adaptive Rate Limiter & Cooldown Engine**: Enforces human-like delay intervals (2–5 seconds between clicks) and daily task quotas per account. |
| **5. Pop-Up / Modal Interruption** | Unexpected website pop-up (e.g., *"Agree to new Terms"*, *"Try YouTube Premium"*). | **Universal Pop-Up Dismissal Parser**: Intercepts modal overlays, auto-dismisses non-essential pop-ups, or prompts user via voice if agreement is required. |
| **6. Slow File Processing / Upload Stall** | YouTube takes 10+ minutes to process HD/4K videos. | **Async Background Task Polling**: Unbinds active UI thread; polls processing status every 30 seconds in background and notifies user via voice upon completion. |
