//! The key that signs the audit ledger's tree heads (see `aqua_tool_host::heads`).
//!
//! The private key is held by Windows Credential Manager (under the program's own account, where no tool of the agent reads it: the agent has no tool
//! that returns a stored secret). The **public** keys (the current one and every earlier one, so that old heads still check) are listed in
//! `data/tools/audit_keys.json`. That list is a convenience: whoever checks a bundle should hold the key id they were told by the person (`aqua-cli
//! audit-key`), because a file on the same disk is only as safe as the disk.

use aqua_native_tools::secrets::Secrets;
use aqua_tool_host::fsutil;
use aqua_tool_host::hex_lower;
use aqua_tool_host::heads::{key_id_of, HeadSigner};
use base64::Engine as _;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const SECRET_NAME: &str = "aqua/audit-signing-key";

/// Signs with the key pair held in memory (read from the credential store once).
pub struct KeySigner {
    pair: Ed25519KeyPair,
}

impl HeadSigner for KeySigner {
    fn key_id(&self) -> String {
        key_id_of(self.pair.public_key().as_ref())
    }
    fn public_key(&self) -> Vec<u8> {
        self.pair.public_key().as_ref().to_vec()
    }
    fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.pair.sign(message).as_ref().to_vec()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownKey {
    pub key_id: String,
    pub public_key_hex: String,
    pub created_at_ms: i64,
}

fn keys_path(data_dir: &Path) -> PathBuf {
    data_dir.join("tools").join("audit_keys.json")
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()).collect()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Every public key that has signed heads here, oldest first. **For showing and for checking only**: a list that cannot be read is an empty list here (the checks then say
/// that nothing can be proven). Whatever *changes* the list reads it with [`known_keys_checked`], which does not.
pub fn known_keys(data_dir: &Path) -> Vec<KnownKey> {
    std::fs::read_to_string(keys_path(data_dir)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// The list, read strictly: no file is an empty list; a file that cannot be read, or is not a list, is an error that says which and what to do. The error is the reason nothing
/// is signed and nothing is written: this list is the only thing that lets a head signed by an earlier key be checked, and a write over a list that was read as empty would
/// erase every earlier key (card OA3-02; the old reader turned any failure, a virus scanner's lock included, into an empty list, and the next write replaced the file).
pub fn known_keys_checked(data_dir: &Path) -> Result<Vec<KnownKey>, String> {
    let path = keys_path(data_dir);
    match fsutil::read_json_strict::<Vec<KnownKey>>(&path) {
        Ok(keys) => Ok(keys.unwrap_or_default()),
        Err(fsutil::ReadError::Unreadable(e)) => Err(format!(
            "{} could not be read ({e}). Nothing was signed and the file was not touched: the keys listed in it are what proves earlier signed heads. Close whatever holds the file and start Aqua again",
            path.display()
        )),
        Err(fsutil::ReadError::Corrupt(e)) => Err(format!(
            "{} is damaged ({e}). Nothing was signed and the file was not touched, because the keys listed in it are what proves earlier signed heads. Repair it, or restore it from a copy; to start a new list on purpose (heads signed before can then no longer be checked) move the file aside and start Aqua again",
            path.display()
        )),
    }
}

/// The public keys as bytes (what `verify_head` takes).
pub fn known_public_keys(data_dir: &Path) -> Vec<Vec<u8>> {
    known_keys(data_dir).iter().filter_map(|k| unhex(&k.public_key_hex)).collect()
}

/// Adds a public key to the list, read strictly (see [`known_keys_checked`]) and written whole and flushed (`fsutil::write_atomic_durable`).
fn remember(data_dir: &Path, public: &[u8]) -> Result<(), String> {
    let mut keys = known_keys_checked(data_dir)?;
    if keys.iter().any(|k| unhex(&k.public_key_hex).as_deref() == Some(public)) {
        return Ok(());
    }
    keys.push(KnownKey { key_id: key_id_of(public), public_key_hex: hex_lower(public), created_at_ms: now_ms() });
    let path = keys_path(data_dir);
    fsutil::write_atomic_durable(&path, &serde_json::to_vec_pretty(&keys).map_err(|e| e.to_string())?).map_err(|e| format!("{}: {e}", path.display()))
}

fn make(secrets: &dyn Secrets, data_dir: &Path) -> Result<KeySigner, String> {
    // The list comes first: a key put into the credential store whose public half cannot then be listed is a key nobody can check heads against.
    known_keys_checked(data_dir)?;
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| "could not make a signing key".to_string())?;
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|_| "the new key is not valid".to_string())?;
    secrets.set(SECRET_NAME, &base64::engine::general_purpose::STANDARD.encode(pkcs8.as_ref()))?;
    remember(data_dir, pair.public_key().as_ref())?;
    Ok(KeySigner { pair })
}

/// The signing key: read from the credential store, or made and stored on the first start. Its public half is added to the list of known keys.
pub fn load_or_create(secrets: &dyn Secrets, data_dir: &Path) -> Result<KeySigner, String> {
    if let Some(text) = secrets.get(SECRET_NAME)? {
        let bytes = base64::engine::general_purpose::STANDARD.decode(text.trim()).map_err(|_| "the stored signing key cannot be read".to_string())?;
        let pair = Ed25519KeyPair::from_pkcs8(&bytes).map_err(|_| "the stored signing key is not valid".to_string())?;
        remember(data_dir, pair.public_key().as_ref())?;
        return Ok(KeySigner { pair });
    }
    make(secrets, data_dir)
}

/// The signing key that is already held, or `None` when no key has been made yet. Nothing is made and nothing is written: `aqua-cli sign-promoted` must not start a key
/// of its own (card R6-13). A stored key that is damaged is an error, as in [`load_or_create`].
pub fn existing_signer(secrets: &dyn Secrets) -> Result<Option<KeySigner>, String> {
    let Some(text) = secrets.get(SECRET_NAME)? else { return Ok(None) };
    let bytes = base64::engine::general_purpose::STANDARD.decode(text.trim()).map_err(|_| "the stored signing key cannot be read".to_string())?;
    let pair = Ed25519KeyPair::from_pkcs8(&bytes).map_err(|_| "the stored signing key is not valid".to_string())?;
    Ok(Some(KeySigner { pair }))
}

const TRUST_KEY_NAME: &str = "aqua/mcp-trust-key";

/// The key that makes the records of what the person trusted (`mcp_trusted.json` of the MCP servers, `trusted_packs.json` of the project packs) something that whoever can only write a file
/// cannot forge: 32 random bytes in the credential store (card R6-16). A program of the same user that can read the credential store can read it too; the record is for the one that can only
/// write a file.
///
/// The answer says whether the key was made now: only at that start is an MCP record in the first format converted (see `ServerManager::set_trust_key`). With `create` false nothing is made
/// (a look at what is trusted must not start a key); with no key, and no store that can say, the answer is `None` and nothing is trusted.
pub fn trust_key(secrets: &dyn Secrets, create: bool) -> Option<(Vec<u8>, bool)> {
    match secrets.get(TRUST_KEY_NAME) {
        Ok(Some(text)) => base64::engine::general_purpose::STANDARD.decode(text.trim()).ok().filter(|k| k.len() >= 32).map(|k| (k, false)),
        Ok(None) if create => {
            let mut key = [0u8; 32];
            ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut key).ok()?;
            secrets.set(TRUST_KEY_NAME, &base64::engine::general_purpose::STANDARD.encode(key)).ok()?;
            Some((key.to_vec(), true))
        }
        Ok(None) | Err(_) => None,
    }
}

/// A new signing key from now on. The old public key stays in the list, so the heads it signed still check.
pub fn rotate(secrets: &dyn Secrets, data_dir: &Path) -> Result<KeySigner, String> {
    make(secrets, data_dir)
}

/// What a person is told so that they can write the key id down somewhere else (a note, another machine) and check a bundle against it later.
pub fn describe(data_dir: &Path) -> String {
    let keys = known_keys(data_dir);
    match keys.last() {
        None => "No signing key has been made yet (it is made the first time Aqua starts with the tool host).".into(),
        Some(k) => format!(
            "Key id: {}\nPublic key: {}\n{} key(s) have signed heads here. Keep the key id and the public key somewhere Aqua cannot reach: a bundle is only as trustworthy as the key it is checked against.",
            k.key_id,
            k.public_key_hex,
            keys.len()
        ),
    }
}

/// `aqua-cli verify-bundle`: checks a bundle file against a public key given in hex. Returns the lines to print and whether everything is good.
pub fn verify_bundle_file(path: &Path, public_key_hex: &str) -> Result<(String, bool), String> {
    let key = unhex(public_key_hex).ok_or("the key must be the public key in hex")?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("the bundle is not valid JSON: {e}"))?;
    // The bundle may be the whole answer of the proof route (it nests the proof) or the proof itself.
    let proof = value.get("proof").cloned().unwrap_or(value);
    let bundle: aqua_tool_host::RunProof = serde_json::from_value(proof).map_err(|e| format!("this is not a run proof: {e}"))?;
    let report = aqua_tool_host::verify_bundle(&bundle, &[key.clone()]);
    let mut out = format!("Run {}: {} line(s), {} verified under a head signed by key {}, {} not yet under any head.\n", bundle.run_id, report.lines, report.verified, key_id_of(&key), report.unsigned);
    for p in &report.problems {
        out.push_str(&format!("PROBLEM: {p}\n"));
    }
    let good = report.all_good();
    out.push_str(if good { "Everything in this bundle checks out.\n" } else { "This bundle does NOT check out completely.\n" });
    Ok((out, good))
}

/// `aqua-cli verify-run`: proves the ledger lines of a run in this install's own ledger.
pub fn verify_run_local(data_dir: &Path, run_id: &str) -> (String, bool) {
    let ledger = aqua_tool_host::AuditLedger::open(data_dir.join("tools").join("host.audit.jsonl"));
    let keys = known_public_keys(data_dir);
    if keys.is_empty() {
        return ("No signing key is known here, so nothing can be proven.\n".into(), false);
    }
    let proof = aqua_tool_host::proof::run_proof(&ledger, run_id, &keys);
    let report = aqua_tool_host::verify_bundle(&proof, &keys);
    let mut out = format!(
        "Run {run_id}: {} ledger line(s); the chain of the whole ledger is {}; {} verified under signed heads, {} not under any head.\n",
        proof.lines.len(),
        if proof.chain_ok { "unbroken" } else { "BROKEN" },
        report.verified,
        report.unsigned
    );
    for p in proof.problems.iter().chain(report.problems.iter()) {
        out.push_str(&format!("PROBLEM: {p}\n"));
    }
    let good = proof.chain_ok && proof.problems.is_empty() && report.all_good();
    out.push_str(if good { "Everything checks out.\n" } else { "This run's record does NOT check out completely.\n" });
    (out, good)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_native_tools::secrets::MemorySecrets;
    use aqua_tool_host::{AuditEvent, AuditLedger};
    use std::sync::Arc;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_audit_key_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("tools")).unwrap();
        d
    }

    fn event(run: &str, tool: &str) -> AuditEvent {
        AuditEvent {
            request_id: "r".into(),
            session_id: "s".into(),
            agent_run_id: run.into(),
            caller: "agent".into(),
            tool: tool.into(),
            args: serde_json::json!({"path": "a.txt"}),
            decision: "auto".into(),
            classes: vec!["READ".into()],
            ok: true,
            error_code: None,
            duration_ms: 1,
            shadow: None,
            approval: None,
        }
    }

    #[test]
    fn the_key_is_made_once_kept_in_the_credential_store_and_only_its_public_half_is_written_to_disk() {
        let d = dir("make");
        let secrets = MemorySecrets::default();
        let a = load_or_create(&secrets, &d).unwrap();
        let b = load_or_create(&secrets, &d).unwrap();
        assert_eq!(a.public_key(), b.public_key(), "the same key the second time");
        assert_eq!(known_keys(&d).len(), 1);
        let file = std::fs::read_to_string(keys_path(&d)).unwrap();
        assert!(file.contains(&a.key_id()) && !file.contains("pkcs8") && !file.contains(&secrets.get(SECRET_NAME).unwrap().unwrap()), "the private key is not in the file");
        assert!(describe(&d).contains(&a.key_id()) && describe(&dir("none")).contains("No signing key"));
        // A stored key that is damaged is an error, not a new key (a new one would make the old heads unverifiable without a word).
        secrets.set(SECRET_NAME, "not a key").unwrap();
        assert!(load_or_create(&secrets, &d).is_err());
    }

    #[test]
    fn a_damaged_list_of_keys_is_never_rewritten_so_the_key_history_is_not_lost() {
        // Regression: any failure to read the list was taken for "no keys yet", and the next write replaced the file with the current key alone.
        let d = dir("damaged");
        let secrets = MemorySecrets::default();
        let first = load_or_create(&secrets, &d).unwrap();
        let second = rotate(&secrets, &d).unwrap();
        assert_eq!(known_keys(&d).len(), 2);
        let good = std::fs::read(keys_path(&d)).unwrap();
        // A truncated file (a crash during an old non-atomic write, or a damaged disk).
        let cut = &good[..good.len() / 2];
        std::fs::write(keys_path(&d), cut).unwrap();
        let key_in_store = secrets.get(SECRET_NAME).unwrap();
        let err = load_or_create(&secrets, &d).err().expect("refused");
        assert!(err.contains("damaged") && err.contains("not touched") && err.contains("move the file aside"), "{err}");
        assert_eq!(std::fs::read(keys_path(&d)).unwrap(), cut, "the file is exactly as it was: both public keys are still in it, and it can be repaired by hand");
        // A rotation does not put a new key into the credential store when its public half cannot be listed.
        assert!(rotate(&secrets, &d).is_err());
        assert_eq!(secrets.get(SECRET_NAME).unwrap(), key_in_store, "no key was made");
        // Repaired, it works again and the history is whole.
        std::fs::write(keys_path(&d), &good).unwrap();
        let again = load_or_create(&secrets, &d).unwrap();
        assert_eq!(again.key_id(), second.key_id());
        assert_eq!(known_keys(&d).iter().map(|k| k.key_id.clone()).collect::<Vec<_>>(), vec![first.key_id(), second.key_id()]);
    }

    #[test]
    fn a_list_that_cannot_be_read_is_not_replaced_either() {
        let d = dir("unreadable");
        let secrets = MemorySecrets::default();
        // A folder where the file should be: it is there, and it cannot be read.
        std::fs::create_dir_all(keys_path(&d)).unwrap();
        let err = load_or_create(&secrets, &d).err().expect("refused");
        assert!(err.contains("could not be read") && err.contains("not touched"), "{err}");
        assert!(keys_path(&d).is_dir(), "left as it was");
        assert!(secrets.get(SECRET_NAME).unwrap().is_none(), "no key was made");
    }

    #[test]
    fn the_list_is_written_whole_and_leaves_no_temporary_file_beside_it() {
        let d = dir("durable");
        let secrets = MemorySecrets::default();
        load_or_create(&secrets, &d).unwrap();
        rotate(&secrets, &d).unwrap();
        let names: Vec<String> = std::fs::read_dir(d.join("tools")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["audit_keys.json".to_string()], "{names:?}");
        assert_eq!(known_keys_checked(&d).unwrap().len(), 2);
        assert_eq!(known_keys_checked(&dir("nothing")).unwrap().len(), 0, "no file is an empty list, not an error");
    }

    #[test]
    fn after_a_rotation_the_heads_of_the_old_key_and_of_the_new_one_both_check() {
        let d = dir("rotate");
        let secrets = MemorySecrets::default();
        let ledger = AuditLedger::open(d.join("tools").join("host.audit.jsonl"));
        let first = load_or_create(&secrets, &d).unwrap();
        ledger.set_head_signer(Arc::new(KeySigner { pair: Ed25519KeyPair::from_pkcs8(&base64::engine::general_purpose::STANDARD.decode(secrets.get(SECRET_NAME).unwrap().unwrap()).unwrap()).unwrap() }));
        for i in 0..5 {
            ledger.append(&event("runA", &format!("t{i}"))).unwrap();
        }
        ledger.seal().unwrap();
        let second = rotate(&secrets, &d).unwrap();
        assert_ne!(first.key_id(), second.key_id());
        ledger.set_head_signer(Arc::new(KeySigner { pair: Ed25519KeyPair::from_pkcs8(&base64::engine::general_purpose::STANDARD.decode(secrets.get(SECRET_NAME).unwrap().unwrap()).unwrap()).unwrap() }));
        for i in 0..5 {
            ledger.append(&event("runA", &format!("u{i}"))).unwrap();
        }
        ledger.seal().unwrap();
        assert_eq!(known_keys(&d).len(), 2);
        let (text, good) = verify_run_local(&d, "runA");
        assert!(good, "{text}");
        assert!(text.contains("10 ledger line(s)") && text.contains("10 verified"), "{text}");
    }

    #[test]
    fn a_bundle_written_to_a_file_is_checked_against_the_key_the_checker_holds() {
        let d = dir("bundle");
        let secrets = MemorySecrets::default();
        let signer = load_or_create(&secrets, &d).unwrap();
        let ledger = AuditLedger::open(d.join("tools").join("host.audit.jsonl"));
        ledger.set_head_signer(Arc::new(KeySigner { pair: Ed25519KeyPair::from_pkcs8(&base64::engine::general_purpose::STANDARD.decode(secrets.get(SECRET_NAME).unwrap().unwrap()).unwrap()).unwrap() }));
        for i in 0..8 {
            ledger.append(&event(if i % 2 == 0 { "runA" } else { "runB" }, &format!("t{i}"))).unwrap();
        }
        let proof = aqua_tool_host::proof::run_proof(&ledger, "runA", &known_public_keys(&d));
        let file = d.join("bundle.json");
        std::fs::write(&file, serde_json::json!({ "proof": proof }).to_string()).unwrap();
        let (text, good) = verify_bundle_file(&file, &hex_lower(&signer.public_key())).unwrap();
        assert!(good && text.contains("4 line(s), 4 verified") && text.contains("Everything in this bundle checks out"), "{text}");
        // Somebody else's key: not good.
        let stranger = KeySigner { pair: Ed25519KeyPair::from_pkcs8(Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap().as_ref()).unwrap() };
        let (text, good) = verify_bundle_file(&file, &hex_lower(&stranger.public_key())).unwrap();
        assert!(!good && text.contains("PROBLEM") && text.contains("NOT check out"), "{text}");
        assert!(verify_bundle_file(&file, "zz").is_err() && verify_bundle_file(&d.join("missing.json"), "00").is_err());
        std::fs::write(&file, "{ \"nope\": 1 }").unwrap();
        assert!(verify_bundle_file(&file, &hex_lower(&signer.public_key())).unwrap_err().contains("not a run proof"));
        let (text, good) = verify_run_local(&dir("nokey"), "runA");
        assert!(!good && text.contains("No signing key"));
    }
}
