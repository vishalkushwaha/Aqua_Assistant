//! Where secrets live: the operating system's credential store, not files and not the model.
//!
//! Tools refer to a secret by name in their configuration (`"secret": "github/token"`). The tool
//! provider reads the value at the moment of the call, attaches it to the outgoing request, hands
//! it to the host so every result and log line is masked, and drops it. On Windows the store is
//! Credential Manager (entries are named `Aqua/<name>` and encrypted for the current user).
//! Secrets are written by the user (`aqua-cli secret-set`), never by a tool call.

use std::collections::HashMap;
use std::sync::Mutex;

pub trait Secrets: Send + Sync {
    fn get(&self, name: &str) -> Result<Option<String>, String>;
    fn set(&self, name: &str, value: &str) -> Result<(), String>;
    fn delete(&self, name: &str) -> Result<bool, String>;
}

/// A secret name: letters, digits and `_ - / .`, at most 100 characters.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 100 && !name.starts_with('/') && !name.contains("..") && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/' | '.'))
}

/// In-memory secrets, for tests and for running without a credential store.
#[derive(Default)]
pub struct MemorySecrets(Mutex<HashMap<String, String>>);

impl MemorySecrets {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Secrets for MemorySecrets {
    fn get(&self, name: &str) -> Result<Option<String>, String> {
        Ok(self.0.lock().unwrap_or_else(|e| e.into_inner()).get(name).cloned())
    }
    fn set(&self, name: &str, value: &str) -> Result<(), String> {
        if !valid_name(name) {
            return Err(format!("'{name}' is not a valid secret name"));
        }
        self.0.lock().unwrap_or_else(|e| e.into_inner()).insert(name.into(), value.into());
        Ok(())
    }
    fn delete(&self, name: &str) -> Result<bool, String> {
        Ok(self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(name).is_some())
    }
}

/// Windows Credential Manager.
pub struct CredentialManager;

#[cfg(windows)]
mod win {
    use super::valid_name;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Security::Credentials::{CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC};

    const ERROR_NOT_FOUND: u32 = 1168;

    fn target(name: &str) -> Vec<u16> {
        format!("Aqua/{name}").encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn get(name: &str) -> Result<Option<String>, String> {
        if !valid_name(name) {
            return Err(format!("'{name}' is not a valid secret name"));
        }
        let t = target(name);
        let mut out: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: `t` is NUL-terminated; on success `out` points to a credential that CredFree releases.
        unsafe {
            if CredReadW(t.as_ptr(), CRED_TYPE_GENERIC, 0, &mut out) == 0 {
                let e = GetLastError();
                return if e == ERROR_NOT_FOUND { Ok(None) } else { Err(format!("Credential Manager error {e}")) };
            }
            let c = &*out;
            let bytes = std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec();
            CredFree(out as *const core::ffi::c_void);
            String::from_utf8(bytes).map(Some).map_err(|_| "the stored secret is not valid UTF-8".to_string())
        }
    }

    pub fn set(name: &str, value: &str) -> Result<(), String> {
        if !valid_name(name) {
            return Err(format!("'{name}' is not a valid secret name"));
        }
        let mut t = target(name);
        let mut blob = value.as_bytes().to_vec();
        // SAFETY: the struct is zeroed and then filled with pointers that outlive the call.
        unsafe {
            let mut c: CREDENTIALW = std::mem::zeroed();
            c.Type = CRED_TYPE_GENERIC;
            c.TargetName = t.as_mut_ptr();
            c.CredentialBlobSize = blob.len() as u32;
            c.CredentialBlob = blob.as_mut_ptr();
            c.Persist = CRED_PERSIST_LOCAL_MACHINE;
            if CredWriteW(&c, 0) == 0 {
                return Err(format!("Credential Manager error {}", GetLastError()));
            }
        }
        Ok(())
    }

    pub fn delete(name: &str) -> Result<bool, String> {
        if !valid_name(name) {
            return Err(format!("'{name}' is not a valid secret name"));
        }
        let t = target(name);
        // SAFETY: `t` is NUL-terminated.
        unsafe {
            if CredDeleteW(t.as_ptr(), CRED_TYPE_GENERIC, 0) == 0 {
                let e = GetLastError();
                return if e == ERROR_NOT_FOUND { Ok(false) } else { Err(format!("Credential Manager error {e}")) };
            }
        }
        Ok(true)
    }
}

#[cfg(windows)]
impl Secrets for CredentialManager {
    fn get(&self, name: &str) -> Result<Option<String>, String> {
        win::get(name)
    }
    fn set(&self, name: &str, value: &str) -> Result<(), String> {
        win::set(name, value)
    }
    fn delete(&self, name: &str) -> Result<bool, String> {
        win::delete(name)
    }
}

#[cfg(not(windows))]
impl Secrets for CredentialManager {
    fn get(&self, _name: &str) -> Result<Option<String>, String> {
        Err("no credential store is wired up on this platform yet".into())
    }
    fn set(&self, _name: &str, _value: &str) -> Result<(), String> {
        Err("no credential store is wired up on this platform yet".into())
    }
    fn delete(&self, _name: &str) -> Result<bool, String> {
        Err("no credential store is wired up on this platform yet".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_names_are_restricted() {
        for ok in ["github/token", "a", "svc.prod-1_key"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "/abs", "a/../b", "a b", "a;b", "a\0b", &"x".repeat(101)] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn memory_secrets_round_trip() {
        let s = MemorySecrets::new();
        assert_eq!(s.get("x").unwrap(), None);
        s.set("x", "v").unwrap();
        assert_eq!(s.get("x").unwrap().as_deref(), Some("v"));
        assert!(s.delete("x").unwrap() && !s.delete("x").unwrap());
        assert!(s.set("bad name", "v").is_err());
    }

    /// What the doctor writes and removes (`doctor/probe-<pid>-<nanos>`): eight of these were found left behind in a real Credential Manager.
    #[cfg(windows)]
    #[test]
    fn a_probe_written_like_the_doctors_is_removed_again_and_nothing_is_left() {
        let cm = CredentialManager;
        let name = format!("doctor/probe-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        cm.set(&name, "doctor-probe-value").unwrap();
        assert_eq!(cm.get(&name).unwrap().as_deref(), Some("doctor-probe-value"));
        assert!(cm.delete(&name).unwrap(), "the delete found it");
        assert_eq!(cm.get(&name).unwrap(), None, "and it is gone");
    }

    /// Uses the real Credential Manager with a throwaway name, and removes it afterwards.
    #[cfg(windows)]
    #[test]
    fn credential_manager_stores_reads_and_deletes_a_secret() {
        let name = format!("aqua-test/{}", std::process::id());
        let cm = CredentialManager;
        assert_eq!(cm.get(&name).unwrap(), None);
        cm.set(&name, "s3cret-value-ünïcode").unwrap();
        assert_eq!(cm.get(&name).unwrap().as_deref(), Some("s3cret-value-ünïcode"));
        cm.set(&name, "replaced").unwrap();
        assert_eq!(cm.get(&name).unwrap().as_deref(), Some("replaced"));
        assert!(cm.delete(&name).unwrap());
        assert_eq!(cm.get(&name).unwrap(), None);
        assert!(!cm.delete(&name).unwrap());
    }
}
