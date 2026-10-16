# Speaker Verification & Admin Voice Fingerprint Panel (`adapters/audio_adapter/speaker_verification/`)

This subsystem manages voice biometrics verification and provides an **Admin Voice Fingerprint Management Panel** to securely register, calibrate, update, or replace authorized user voice profiles.

---

## 1. Admin Voice Fingerprint Management Panel

Located in the Tauri Desktop UI under **Settings $\rightarrow$ Admin Voice Security Panel**:

### Features & Capabilities:
1. **Enrollment Wizard**:
   - Prompts the user to speak 3 sample calibration phrases (e.g., *"Aqua, system activation test 1"*, *"Hey Aqua, confirm my voice profile"*, *"Aqua, security calibration complete"*).
   - Extracts 192-dimensional `ECAPA-TDNN` embedding vectors for each phrase and computes an averaged Master Voice Fingerprint.
2. **AES-256 Encrypted Vault Storage**:
   - The master voice fingerprint vector is encrypted with AES-256-GCM and stored in `data/secure_db/`.
3. **Adaptive Voice Profile Drift Calibration (`adaptive_profile.rs`)**:
   - When an authorized user successfully authenticates with high confidence ($\ge 0.90$), Aqua subtly updates the rolling average embedding in `data/secure_db/` so voice recognition adapts over time without compromising security.
4. **Re-calibration & Profile Replacement**:
   - Allows the Admin to recalibrate or replace the registered voice fingerprint anytime.
5. **Strict Similarity Threshold Tuning**:
   - Configurable strictness threshold ($\text{Cosine Similarity} \ge 0.82$).

---

## 2. Strict Single-User Voice Enforcement Pipeline

```
[ Wake Word "Aqua" / "Hey Aqua" Triggered ]
                     │
                     ▼
[ Acoustic Echo Cancellation (AEC) & Frame Extraction ] ──► Subtracts speaker output from mic stream
                     │
                     ▼
[ ECAPA-TDNN ONNX Embedding Extractor ] ──► Computes 192-dim Voice Vector
                     │
                     ▼
[ Cosine Similarity Comparison against Encrypted Master Fingerprint ]
                     │
     ┌───────────────┴───────────────┐
     ▼                               ▼
[ Match >= 0.82 ]             [ Match < 0.82 (Unauthorized Voice) ]
     │                               │
     ▼                               ▼
- Wake Up & Active State      - Remain 100% Silent & Inactive
- Emit Visual Pulse Event     - Reject Command / Zero Response
- Speak: "Yes, Master..."     - Log Security Attempt to `telemetry/`
- Unlock System Execution
```

---

## 3. Strict Rejection Rule
- **Other Users' Voices**: Strictly rejected. If an unauthorized person speaks *"Aqua"* or *"Hey Aqua"*, the system does **NOT** respond, does not speak, and remains completely inactive.
