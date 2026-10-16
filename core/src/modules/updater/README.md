# Cross-Platform OTA Updater & Model Synchronizer (`core/src/modules/updater/`)

> **Code status (2026-10-08): written, not wired.** This module has tests, and nothing outside them calls it ([who calls what](../../../../documentation/competitive/refresh/10_DEAD_CODE_CALLERS.md)). The text below describes the design, not what runs in Aqua. The app updates through `apps/aqua-desktop/src-tauri/src/updates.rs` and `update_policy.rs` (see `documentation/UPDATES.md`); models are fetched by `scripts/install_models.ps1`. There is no register entry.

Manages secure updates for Project Aqua's application binaries and AI models.

## Responsibilities
1. **Ed25519 signature check**: verifies a binary's signature against a public key the caller pins, then swaps it in beside the old one (kept as `.bak`). A made-up signature is refused. It does not download anything yet.
2. **Local Vault Preservation**: Ensures local SQLite databases (`data/secure_db/`), user voice fingerprints, custom resumes, and solution memory (`solutions.json`) are preserved without data loss during software updates.
3. **SHA-256 verification**: a listed model must exist and match its SHA-256 (computed in a stream); a missing or mismatching model refuses the update. Downloading models is done by `scripts/fetch_*.ps1`, which verify against pinned hashes.
