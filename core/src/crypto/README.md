# Cryptography & Vault Key Rotation

> **Code status (2026-10-08): written, not wired.** `KeyRotationEngine` has tests, and nothing outside them calls it ([who calls what](../../../documentation/competitive/refresh/10_DEAD_CODE_CALLERS.md)). The text below describes the design, not what runs in Aqua. The keys in use are `core/src/audit_key.rs` and `core/src/storage/voiceprint.rs`. There is no register entry.

Master key rotation and re-encryption utilities for the encrypted vault (`data/secure_db/`).

---

## Module

| File | Type |
| :--- | :--- |
| `key_rotation.rs` | `KeyRotationEngine`, `VaultPayload` |
| `mod.rs` | Re-exports + `status()` |

---

## Responsibilities (architecture)

- Derive keys with PBKDF2-HMAC-SHA256 (600,000 iterations, random 16-byte salt per payload; Argon2id is not available to this build)
- Encrypt each payload with AES-256-GCM (fresh random nonce; the payload id and key version are authenticated, so renaming or re-versioning a payload is detected)
- Rotate the master password: every payload is opened with the old one and sealed again with the new one; the file is replaced only if all succeed, and a vault that does not exist is an error
- A wrong password or a changed byte never opens a payload

Nothing stores real data in the vault yet. The voiceprint is sealed separately (`storage/voiceprint.rs`) with a key held in Windows Credential Manager.
- Integrate with OTA updater vault preservation ([`../modules/updater/README.md`](../modules/updater/README.md))

---

## Tests

`core/tests/key_rotation_test.rs`

---

## Related

- Architecture requirement #7 in [`documentation/REQUIREMENTS_TRACEABILITY.md`](../../../documentation/REQUIREMENTS_TRACEABILITY.md)
- Storage (non-encrypted): [`../storage/README.md`](../storage/README.md)
