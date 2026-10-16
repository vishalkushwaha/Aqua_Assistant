//! The voiceprint at rest.
//!
//! A voiceprint is biometric data, so the database never holds it as readable numbers. It is sealed with
//! AES-256-GCM under a random 32-byte key that lives in the operating system's credential store (Windows
//! Credential Manager, encrypted for the current user) and never in a file next to the database. Copying
//! `aqua_core.db` alone therefore does not reveal it.
//!
//! Stored form: `enc:v1:<nonce hex>:<ciphertext hex>`. A value that is still plain JSON (written by an older
//! version) is read as before and upgraded to this form the next time it is saved or loaded.

use aqua_native_tools::secrets::Secrets;
use aqua_tool_host::hex_lower;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};

/// Name of the key in the credential store.
pub const KEY_NAME: &str = "aqua/voiceprint-key";
const PREFIX: &str = "enc:v1:";
const AAD: &[u8] = b"aqua-voiceprint|v1";

/// True if `stored` is already in the sealed form.
pub fn is_sealed(stored: &str) -> bool {
    stored.starts_with(PREFIX)
}

fn key_bytes(secrets: &dyn Secrets) -> Result<[u8; 32], String> {
    if let Some(hex) = secrets.get(KEY_NAME)? {
        let bytes = from_hex(hex.trim())?;
        return bytes.try_into().map_err(|_| "the stored voiceprint key has the wrong length".to_string());
    }
    let mut key = [0u8; 32];
    SystemRandom::new().fill(&mut key).map_err(|_| "no secure random source".to_string())?;
    secrets.set(KEY_NAME, &hex_lower(&key))?;
    Ok(key)
}

/// Seal an embedding for storage.
pub fn protect(secrets: &dyn Secrets, embedding: &[f32]) -> Result<String, String> {
    let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key_bytes(secrets)?).map_err(|_| "bad key".to_string())?);
    let mut nonce = [0u8; NONCE_LEN];
    SystemRandom::new().fill(&mut nonce).map_err(|_| "no secure random source".to_string())?;
    let mut buf = serde_json::to_vec(embedding).map_err(|e| e.to_string())?;
    key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(AAD), &mut buf).map_err(|_| "encryption failed".to_string())?;
    Ok(format!("{PREFIX}{}:{}", hex_lower(&nonce), hex_lower(&buf)))
}

/// Read a stored embedding: sealed (decrypted) or legacy plain JSON.
pub fn reveal(secrets: &dyn Secrets, stored: &str) -> Result<Vec<f32>, String> {
    let Some(rest) = stored.strip_prefix(PREFIX) else {
        return serde_json::from_str(stored).map_err(|e| format!("the stored voiceprint is not readable: {e}"));
    };
    let (nonce_hex, cipher_hex) = rest.split_once(':').ok_or("the stored voiceprint is damaged")?;
    let nonce: [u8; NONCE_LEN] = from_hex(nonce_hex)?.try_into().map_err(|_| "the stored voiceprint is damaged".to_string())?;
    let mut buf = from_hex(cipher_hex)?;
    let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key_bytes_existing(secrets)?).map_err(|_| "bad key".to_string())?);
    let plain = key
        .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::from(AAD), &mut buf)
        .map_err(|_| "the stored voiceprint cannot be opened (wrong key, or it was changed)".to_string())?;
    serde_json::from_slice(plain).map_err(|e| format!("the stored voiceprint is not readable: {e}"))
}

/// Opening never creates a key: a missing key means the data cannot be read, not that a new one should be made.
fn key_bytes_existing(secrets: &dyn Secrets) -> Result<[u8; 32], String> {
    let hex = secrets.get(KEY_NAME)?.ok_or("the voiceprint key is missing from the credential store, so the voiceprint cannot be opened (enrol again)")?;
    from_hex(hex.trim())?.try_into().map_err(|_| "the stored voiceprint key has the wrong length".to_string())
}

fn from_hex(s: &str) -> Result<Vec<u8>, String> {
    // ASCII hex digits only: a damaged value with a multi-byte character would otherwise panic when sliced (and
    // `from_str_radix` also accepts a leading '+'). A bad value is an error, which means "enrol again", not a crash.
    if s.len() % 2 != 0 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid hex".into());
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_native_tools::secrets::MemorySecrets;

    #[test]
    fn damaged_hex_is_an_error_and_never_a_panic() {
        assert_eq!(from_hex("00ff10").unwrap(), vec![0, 255, 16]);
        for bad in ["aéb0", "é", "+f", "0g", "abc", "ééé"] {
            assert!(from_hex(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_voiceprint_round_trips_and_the_stored_text_shows_none_of_its_numbers() {
        let s = MemorySecrets::new();
        let emb = vec![0.123456_f32, -0.987654, 0.5];
        let sealed = protect(&s, &emb).unwrap();
        assert!(is_sealed(&sealed) && !sealed.contains("0.1234") && !sealed.contains("987654"), "{sealed}");
        assert_eq!(reveal(&s, &sealed).unwrap(), emb);
        assert_ne!(protect(&s, &emb).unwrap(), sealed, "a fresh nonce each time");
    }

    #[test]
    fn the_key_is_created_once_kept_in_the_credential_store_and_never_made_up_when_missing() {
        let s = MemorySecrets::new();
        assert!(s.get(KEY_NAME).unwrap().is_none());
        let sealed = protect(&s, &[1.0, 2.0]).unwrap();
        let key = s.get(KEY_NAME).unwrap().expect("created on first use");
        assert_eq!(key.len(), 64);
        protect(&s, &[3.0]).unwrap();
        assert_eq!(s.get(KEY_NAME).unwrap().unwrap(), key, "reused, not replaced");
        let elsewhere = MemorySecrets::new();
        assert!(reveal(&elsewhere, &sealed).unwrap_err().contains("key is missing"), "no key, no data, and no new key is invented");
        assert!(elsewhere.get(KEY_NAME).unwrap().is_none());
    }

    #[test]
    fn a_changed_or_wrong_key_value_never_opens_and_legacy_plain_json_still_reads() {
        let s = MemorySecrets::new();
        let sealed = protect(&s, &[1.0, 2.0]).unwrap();
        let mut bad = sealed.clone();
        let last = bad.pop().unwrap();
        bad.push(if last == '0' { '1' } else { '0' });
        assert!(reveal(&s, &bad).is_err());
        s.set(KEY_NAME, &"ab".repeat(32)).unwrap();
        assert!(reveal(&s, &sealed).is_err(), "another key");
        assert!(reveal(&s, "enc:v1:nonsense").is_err());
        assert_eq!(reveal(&s, "[0.25,0.5]").unwrap(), vec![0.25, 0.5], "older plain JSON");
        assert!(!is_sealed("[0.25,0.5]"));
    }
}
