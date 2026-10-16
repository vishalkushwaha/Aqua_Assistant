# Encrypted Backup & System Recovery Manager (`core/src/modules/backup_manager/`)

> **Code status:** `mod.rs` only defines `status() -> "Backup Manager Ready"`. Sections below describe the **target design**. See [`documentation/API_REFERENCE.md`](../../../../documentation/API_REFERENCE.md) and traceability domain #23 in [`REQUIREMENTS_TRACEABILITY.md`](../../../../documentation/REQUIREMENTS_TRACEABILITY.md).

---

## 1. Subsystem Architecture & Purpose

The **Backup Manager** ensures total data resiliency, disaster recovery, and cryptographic privacy for Project Aqua. It handles zero-downtime background snapshots of databases, vector indices, user settings, and solution memories.

---

## 2. Key Components & Pipeline

1. **Automated Snapshot Engine (`snapshot.rs`)**:
   - Executes daily scheduled backups or on-demand manual triggers before major system updates.
   - Performs atomic SQLite WAL checkpoints (`PRAGMA wal_checkpoint(FULL)`) prior to archive creation.

2. **AES-256-GCM Cryptographic Vault Archiver (`crypto_archiver.rs`)**:
   - Packages `aqua_core.db`, `solutions.db`, `secure_db/`, and user preferences into a compressed archive (`.aqua_bak`).
   - Encrypts the entire snapshot using AES-256-GCM with a key derived from the user's master passcode via Argon2id.

3. **System Recovery & Verification (`recovery.rs`)**:
   - Verifies SHA-256 integrity checksums and ED25519 digital signatures before restoring any backup.
   - Reverts state atomically; if restoration fails midway, the existing live database remains untouched.

---

## 3. Backup Locations & Retention Policy

- **Local Snapshot Storage**: `data/backups/aqua_snapshot_<YYYYMMDD_HHMMSS>.aqua_bak`.
- **Retention Rule**: Keeps the 7 most recent daily snapshots and 4 weekly archives; older archives are automatically pruned to prevent disk bloat.
