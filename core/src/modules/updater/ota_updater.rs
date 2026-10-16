//! Update verification and rolling update.
//!
//! * A binary is applied only if its Ed25519 signature verifies against a public key the caller pins (compile it
//!   into the program; never take it from the same place as the update).
//! * A model file must match its SHA-256, computed in a stream (models are gigabytes).
//! * The new binary is written beside the old one and swapped in by a rename; the old one is kept as `.bak` and put
//!   back if the swap fails.
//!
//! Nothing here is wired to a network download yet: it verifies bytes it is handed.

use ring::signature::{UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// SHA-256 Checksum record for an AI model weight.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelChecksum {
    pub model_name: String,
    /// 64 hex characters.
    pub expected_sha256: String,
    pub relative_path: PathBuf,
}

/// OTA Release Manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateManifest {
    pub version: String,
    pub binary_url: String,
    /// Ed25519 signature over the binary, hex (128 characters) or base64.
    pub ed25519_signature: String,
    pub models: Vec<ModelChecksum>,
}

/// OTA updater.
pub struct OtaUpdater {
    models_dir: PathBuf,
}

impl OtaUpdater {
    /// Creates a new OtaUpdater instance.
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        let models_dir = base_dir.as_ref().join("data").join("models");
        Self { models_dir }
    }

    /// True only if `signature` is a valid Ed25519 signature of `data` under `public_key` (hex or base64, 32 bytes).
    /// Any malformed input, an empty payload or a wrong key is false.
    pub fn verify_ed25519_signature(&self, data: &[u8], signature: &str, public_key: &str) -> bool {
        if data.is_empty() {
            return false;
        }
        let (Some(sig), Some(key)) = (decode_bytes(signature, 64), decode_bytes(public_key, 32)) else {
            return false;
        };
        UnparsedPublicKey::new(&ED25519, key).verify(data, &sig).is_ok()
    }

    /// Verify the SHA-256 of a local file against `expected_sha256` (hex, any case).
    pub fn verify_model_sha256(&self, model_path: &Path, expected_sha256: &str) -> Result<bool, String> {
        let expected = expected_sha256.trim().to_ascii_lowercase();
        if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("not a SHA-256 value (needs 64 hex characters): {expected_sha256}"));
        }
        let computed = sha256_file(model_path)?;
        if computed == expected {
            Ok(true)
        } else {
            Err(format!("Checksum mismatch for {}: expected {expected}, found {computed}", model_path.display()))
        }
    }

    /// Verify, then swap in the new binary (keeping a `.bak`). Every listed model must exist and match.
    pub fn apply_rolling_update(&self, update_binary_bytes: &[u8], manifest: &UpdateManifest, public_key: &str) -> Result<bool, String> {
        // Step 1: the signature, against the key the caller pinned.
        if !self.verify_ed25519_signature(update_binary_bytes, &manifest.ed25519_signature, public_key) {
            return Err("Signature verification failed. Update refused.".to_string());
        }

        // Step 2: every model the manifest lists. One that is missing cannot be vouched for, so it is an error.
        for model in &manifest.models {
            let full_path = self.models_dir.join(&model.relative_path);
            if !full_path.is_file() {
                return Err(format!("{} is listed in the update but is not on disk: {}", model.model_name, full_path.display()));
            }
            self.verify_model_sha256(&full_path, &model.expected_sha256)?;
        }

        // Step 3: write beside, then rename; keep the old one.
        let target = self.models_dir.join("aqua_binary.exe");
        let backup = self.models_dir.join("aqua_binary.exe.bak");
        let staged = self.models_dir.join("aqua_binary.exe.new");
        fs::create_dir_all(&self.models_dir).map_err(|e| format!("could not create {}: {e}", self.models_dir.display()))?;
        fs::write(&staged, update_binary_bytes).map_err(|e| format!("Failed to stage the update: {e}"))?;
        let had_old = target.exists();
        if had_old {
            fs::copy(&target, &backup).map_err(|e| {
                let _ = fs::remove_file(&staged);
                format!("Could not back up the current binary, so nothing was changed: {e}")
            })?;
        }
        if let Err(e) = fs::rename(&staged, &target) {
            let _ = fs::remove_file(&staged);
            if had_old {
                let _ = fs::copy(&backup, &target);
            }
            return Err(format!("Failed to install the update (the previous version was restored): {e}"));
        }
        tracing::info!("OTA update applied (version {})", manifest.version);
        Ok(true)
    }
}

/// SHA-256 of a file, hex, read in blocks.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|e| format!("Model file {} cannot be opened: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Hex or base64 text to exactly `len` bytes.
fn decode_bytes(text: &str, len: usize) -> Option<Vec<u8>> {
    use base64::Engine;
    let t = text.trim();
    if t.len() == len * 2 && t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return (0..t.len()).step_by(2).map(|i| u8::from_str_radix(&t[i..i + 2], 16).ok()).collect();
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(t).ok()?;
    (bytes.len() == len).then_some(bytes)
}
