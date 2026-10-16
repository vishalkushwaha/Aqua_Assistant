//! The encrypted vault (`data/secure_db/vault.json`) and master-password rotation.
//!
//! * Key: PBKDF2-HMAC-SHA256 (600,000 iterations by default) over the password and a random 16-byte salt.
//! * Encryption: AES-256-GCM with a fresh random 96-bit nonce for every payload. The payload id and key version
//!   are authenticated, so a payload cannot be renamed or moved to another version without being detected.
//! * A wrong password, a flipped byte or a swapped id all fail to open; nothing is returned in that case.
//!
//! (Argon2id would be preferable and is not available to this build; the iteration count is stored with each
//! payload so it can be raised later without breaking old ones.)

use aqua_tool_host::hex_lower;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::fs;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

/// PBKDF2 work factor for new payloads.
pub const DEFAULT_ITERATIONS: u32 = 600_000;
const SALT_LEN: usize = 16;

/// One encrypted item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultPayload {
    pub payload_id: String,
    pub ciphertext_hex: String,
    pub salt_hex: String,
    pub nonce_hex: String,
    pub key_version: u32,
    /// PBKDF2 iterations used for this payload's key.
    #[serde(default = "default_iterations")]
    pub iterations: u32,
}

fn default_iterations() -> u32 {
    DEFAULT_ITERATIONS
}

/// The vault file: every payload, all under the same password.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Vault {
    pub payloads: Vec<VaultPayload>,
}

/// Key Rotation Engine.
pub struct KeyRotationEngine {
    vault_path: PathBuf,
    iterations: u32,
}

impl KeyRotationEngine {
    /// Creates a new KeyRotationEngine instance.
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        let vault_path = base_dir.as_ref().join("data").join("secure_db").join("vault.json");
        Self { vault_path, iterations: DEFAULT_ITERATIONS }
    }

    /// A lower work factor, for tests only (a production vault must use `DEFAULT_ITERATIONS` or more).
    pub fn with_iterations(mut self, iterations: u32) -> Self {
        self.iterations = iterations.max(1);
        self
    }

    pub fn vault_path(&self) -> &Path {
        &self.vault_path
    }

    /// Derive a 256-bit key from a password and salt with PBKDF2-HMAC-SHA256.
    pub fn derive_key(&self, password: &str, salt: &[u8], iterations: u32) -> Vec<u8> {
        let mut key = vec![0u8; 32];
        let rounds = NonZeroU32::new(iterations.max(1)).expect("at least one round");
        ring::pbkdf2::derive(ring::pbkdf2::PBKDF2_HMAC_SHA256, rounds, salt, password.as_bytes(), &mut key);
        key
    }

    /// Encrypt `plaintext` under `password` as a new payload (fresh random salt and nonce).
    pub fn seal(&self, password: &str, plaintext: &[u8], payload_id: &str, key_version: u32) -> Result<VaultPayload, String> {
        if password.is_empty() {
            return Err("the password cannot be empty".into());
        }
        let rng = SystemRandom::new();
        let mut salt = [0u8; SALT_LEN];
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rng.fill(&mut salt).map_err(|_| "no secure random source")?;
        rng.fill(&mut nonce_bytes).map_err(|_| "no secure random source")?;
        let key = self.derive_key(password, &salt, self.iterations);
        let sealing = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).map_err(|_| "bad key")?);
        let mut buf = plaintext.to_vec();
        sealing
            .seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce_bytes), Aad::from(aad(payload_id, key_version)), &mut buf)
            .map_err(|_| "encryption failed")?;
        Ok(VaultPayload {
            payload_id: payload_id.to_string(),
            ciphertext_hex: hex_lower(&buf),
            salt_hex: hex_lower(&salt),
            nonce_hex: hex_lower(&nonce_bytes),
            key_version,
            iterations: self.iterations,
        })
    }

    /// Decrypt a payload. A wrong password or any change to the stored data is an error.
    pub fn open(&self, password: &str, payload: &VaultPayload) -> Result<Vec<u8>, String> {
        let salt = from_hex(&payload.salt_hex)?;
        let nonce_bytes: [u8; NONCE_LEN] = from_hex(&payload.nonce_hex)?.try_into().map_err(|_| "bad nonce length".to_string())?;
        let mut buf = from_hex(&payload.ciphertext_hex)?;
        let key = self.derive_key(password, &salt, payload.iterations);
        let opening = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).map_err(|_| "bad key".to_string())?);
        let plain = opening
            .open_in_place(Nonce::assume_unique_for_key(nonce_bytes), Aad::from(aad(&payload.payload_id, payload.key_version)), &mut buf)
            .map_err(|_| "wrong password, or the vault data was changed".to_string())?;
        Ok(plain.to_vec())
    }

    pub fn load(&self) -> Result<Vault, String> {
        match fs::read_to_string(&self.vault_path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("the vault file is damaged: {e}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vault::default()),
            Err(e) => Err(format!("could not read the vault: {e}")),
        }
    }

    /// Write the vault atomically: a crash leaves either the old file or the new one, never half of one.
    pub fn save(&self, vault: &Vault) -> Result<(), String> {
        let json = serde_json::to_string_pretty(vault).map_err(|e| format!("could not serialise the vault: {e}"))?;
        // Flushed before it replaces the old file (`fsutil::write_atomic_durable`): a power cut must not leave the encrypted vault empty.
        aqua_tool_host::fsutil::write_atomic_durable(&self.vault_path, json.as_bytes()).map_err(|e| format!("could not write the vault: {e}"))
    }

    /// Add or replace one item in the vault.
    pub fn store(&self, password: &str, payload_id: &str, plaintext: &[u8]) -> Result<(), String> {
        let mut vault = self.load()?;
        let version = vault.payloads.iter().map(|p| p.key_version).max().unwrap_or(1);
        // Every payload shares one password, so a wrong one must not be able to add a payload beside the others.
        if let Some(existing) = vault.payloads.first() {
            self.open(password, existing)?;
        }
        let sealed = self.seal(password, plaintext, payload_id, version)?;
        vault.payloads.retain(|p| p.payload_id != payload_id);
        vault.payloads.push(sealed);
        self.save(&vault)
    }

    /// Read one item from the vault.
    pub fn fetch(&self, password: &str, payload_id: &str) -> Result<Vec<u8>, String> {
        let vault = self.load()?;
        let payload = vault.payloads.iter().find(|p| p.payload_id == payload_id).ok_or_else(|| format!("no item named {payload_id} in the vault"))?;
        self.open(password, payload)
    }

    /// Change the master password: every payload is opened with the old one and sealed again with the new one
    /// (key version + 1), and the file is replaced only if all of them succeeded. There is nothing to rotate in a
    /// vault that does not exist, and that is an error rather than something to make up.
    pub fn rotate_master_key(&self, old_password: &str, new_password: &str) -> Result<bool, String> {
        if old_password.is_empty() || new_password.is_empty() {
            return Err("Password strings cannot be empty".to_string());
        }
        let vault = self.load()?;
        if vault.payloads.is_empty() {
            return Err("there is no vault to rotate (nothing has been stored in it)".to_string());
        }
        let mut rekeyed = Vault::default();
        for payload in &vault.payloads {
            let plain = self.open(old_password, payload)?;
            rekeyed.payloads.push(self.seal(new_password, &plain, &payload.payload_id, payload.key_version + 1)?);
        }
        self.save(&rekeyed)?;
        tracing::info!("Vault master password rotated; {} item(s) re-keyed.", rekeyed.payloads.len());
        Ok(true)
    }
}

/// What is authenticated along with the ciphertext.
fn aad(payload_id: &str, key_version: u32) -> Vec<u8> {
    format!("aqua-vault|{payload_id}|{key_version}").into_bytes()
}

fn from_hex(hex_str: &str) -> Result<Vec<u8>, String> {
    if hex_str.len() % 2 != 0 || !hex_str.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid hex string".to_string());
    }
    (0..hex_str.len()).step_by(2).map(|i| u8::from_str_radix(&hex_str[i..i + 2], 16).map_err(|e| e.to_string())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(name: &str) -> KeyRotationEngine {
        let dir = std::env::temp_dir().join(format!("aqua_vault_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        KeyRotationEngine::new(&dir).with_iterations(1_000)
    }

    #[test]
    fn what_goes_in_comes_out_and_nothing_readable_is_stored() {
        let e = engine("roundtrip");
        let secret = b"voiceprint 0.12 0.88 -0.4 and a passphrase";
        e.store("correct horse", "voice", secret).unwrap();
        assert_eq!(e.fetch("correct horse", "voice").unwrap(), secret);
        let file = fs::read_to_string(e.vault_path()).unwrap();
        assert!(!file.contains("voiceprint") && !file.contains(&hex_lower(secret)), "the file must not hold the plaintext");
    }

    #[test]
    fn a_wrong_password_a_flipped_byte_or_a_renamed_item_never_opens() {
        let e = engine("tamper");
        let p = e.seal("pw", b"data", "voice", 1).unwrap();
        assert_eq!(e.open("pw", &p).unwrap(), b"data");
        assert!(e.open("pw2", &p).is_err());
        let mut flipped = p.clone();
        let mut bytes = from_hex(&flipped.ciphertext_hex).unwrap();
        bytes[0] ^= 1;
        flipped.ciphertext_hex = hex_lower(&bytes);
        assert!(e.open("pw", &flipped).is_err());
        let mut renamed = p.clone();
        renamed.payload_id = "other".into();
        assert!(e.open("pw", &renamed).is_err(), "the id is authenticated");
        let mut newer = p.clone();
        newer.key_version = 9;
        assert!(e.open("pw", &newer).is_err(), "the key version is authenticated");
    }

    #[test]
    fn every_payload_gets_its_own_salt_and_nonce() {
        let e = engine("fresh");
        let (a, b) = (e.seal("pw", b"same", "x", 1).unwrap(), e.seal("pw", b"same", "x", 1).unwrap());
        assert_ne!((a.salt_hex.clone(), a.nonce_hex.clone()), (b.salt_hex.clone(), b.nonce_hex.clone()));
        assert_ne!(a.ciphertext_hex, b.ciphertext_hex);
        assert_eq!(e.derive_key("pw", &from_hex(&a.salt_hex).unwrap(), 1_000).len(), 32);
        assert!(e.seal("", b"x", "x", 1).is_err());
    }

    #[test]
    fn rotating_the_password_rekeys_every_item_and_the_old_password_stops_working() {
        let e = engine("rotate");
        e.store("old", "voice", b"voiceprint").unwrap();
        e.store("old", "token", b"api-token").unwrap();
        assert!(e.rotate_master_key("old", "new").unwrap());
        assert_eq!(e.fetch("new", "voice").unwrap(), b"voiceprint");
        assert_eq!(e.fetch("new", "token").unwrap(), b"api-token");
        assert!(e.fetch("old", "voice").is_err());
        assert!(e.load().unwrap().payloads.iter().all(|p| p.key_version == 2));
    }

    #[test]
    fn a_failed_rotation_changes_nothing() {
        let e = engine("rotate_fail");
        assert!(e.rotate_master_key("a", "b").unwrap_err().contains("no vault to rotate"), "no vault: say so, do not invent data");
        e.store("old", "voice", b"voiceprint").unwrap();
        let before = fs::read_to_string(e.vault_path()).unwrap();
        assert!(e.rotate_master_key("WRONG", "new").is_err());
        assert_eq!(fs::read_to_string(e.vault_path()).unwrap(), before);
        assert!(e.rotate_master_key("", "new").is_err());
        assert!(e.store("WRONG", "other", b"x").is_err(), "a wrong password cannot add to an existing vault");
    }
}
