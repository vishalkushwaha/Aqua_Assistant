//! A configuration that won: `data/promoted_config.json`, written by `scripts/eval/quality.py promote` only when the candidate's result passed the harness's
//! gates against a baseline. At start the core sets the flags in it **unless the person has set them already** (an environment variable the person set is
//! the person's choice and wins), and only flags in [`ALLOWED`] with the values listed there: a file with anything else in it is ignored for that entry and
//! said so. Deleting the file (`quality.py unpromote`) puts the defaults of the code back.
//!
//! **The file is signed** (card R6-13). `quality.py promote` writes `promoted_config.pending.json`, which holds the promotions as one text (`payload`) and nothing that
//! proves who wrote it; a person runs `aqua-cli sign-promoted` on it, which checks it again, shows what it holds, and signs it with the audit key (the key that signs the
//! tool host's ledger, kept in the Windows credential store, where the agent's tools cannot read it). At start the core uses a file only when its signature is by a key
//! this installation knows (`audit_key::known_public_keys`) and covers the text exactly as it is: a file that was edited afterwards, signed by a key nobody here knows,
//! or not signed at all (the first format of this file was not) is not used, and the reason is said. What is signed is [`SIGN_DOMAIN`] and the payload, so that a
//! signature made for this file is not one made for a tree head of the ledger, which the same key signs.
//!
//! The payload is `{ "schema": 2, "promoted_at", "flags": {the flags in effect}, "promotions": [{ "label", "candidate", "flags", "evidence", "replaced", "union_measured" }] }`.
//! A second promotion is added to the first only when the person asks for it (`quality.py promote --stack`), and then says `union_measured: false`: the two were each
//! measured against a baseline that had neither.

use aqua_tool_host::heads::{key_id_of, HeadSigner};
use aqua_tool_host::hex_lower;
use ring::signature::{UnparsedPublicKey, ED25519};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What a promotion may set, and to what. `scripts/eval/quality.py` has the same list and a test keeps the two equal.
///
/// Only flags that change how well the agent works. Never one that changes what is asked or what the agent may reach (`AQUA_CAPS`, `AQUA_TOOL_SEARCH`,
/// `AQUA_ONE_DOOR`, full access): the harness measures speed and correctness, and a policy that asks less can only look better to it. The person switches those.
pub const ALLOWED: &[(&str, &[&str])] = &[
    ("AQUA_CONTEXT", &["v2"]),
    ("AQUA_PLAN", &["on"]),
    ("AQUA_READER", &["on"]),
    ("AQUA_ASK", &["on"]),
    ("AQUA_PLANNER_BATCH", &["2", "3"]),
];

/// What is put before the payload in what is signed. The audit key also signs the ledger's tree heads; this keeps a signature made for one from being taken for the other.
pub const SIGN_DOMAIN: &str = "aqua/promoted-config/v2\n";

/// One promotion in the file: a candidate that passed the gate, with the flags it set.
#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub flags: BTreeMap<String, String>,
    /// Was this promotion measured together with the ones before it? False for a second promotion that was added to a first (`quality.py promote --stack`).
    #[serde(default = "yes")]
    pub union_measured: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Promoted {
    #[serde(default)]
    pub label: String,
    /// The flags in effect: what the promotions set, a later one replacing an earlier one.
    #[serde(default)]
    pub flags: BTreeMap<String, String>,
    #[serde(default)]
    pub promotions: Vec<Entry>,
}

impl Promoted {
    /// The label to log: the promotions' labels, and a note when two of them were not measured together.
    pub fn describe(&self) -> String {
        let labels: Vec<&str> = self.promotions.iter().map(|p| p.label.as_str()).filter(|l| !l.is_empty()).collect();
        let mut out = if labels.is_empty() { self.label.clone() } else { labels.join(" + ") };
        if self.promotions.iter().any(|p| !p.union_measured) {
            out.push_str(" (stacked: the promotions together were not measured)");
        }
        out
    }
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("promoted_config.json")
}

/// The bytes a signature covers: [`SIGN_DOMAIN`] and the payload, exactly as the file holds the text.
fn signed_bytes(payload: &str) -> Vec<u8> {
    let mut out = SIGN_DOMAIN.as_bytes().to_vec();
    out.extend_from_slice(payload.as_bytes());
    out
}

/// Reads the payload (the promotions as text): its schema must be 2, and the flags in effect are worked out from the promotions when it does not list them.
pub fn parse_payload(payload: &str) -> Result<Promoted, String> {
    let value: serde_json::Value = serde_json::from_str(payload).map_err(|e| format!("the promotions cannot be read: {e}"))?;
    if value.get("schema").and_then(|s| s.as_u64()) != Some(2) {
        return Err("the promotions are not in the format this version reads (schema 2)".into());
    }
    let mut promoted: Promoted = serde_json::from_value(value).map_err(|e| format!("the promotions cannot be read: {e}"))?;
    if promoted.flags.is_empty() {
        for p in &promoted.promotions {
            promoted.flags.extend(p.flags.clone());
        }
    }
    Ok(promoted)
}

/// A file of the kind `quality.py promote` writes (`schema`, `payload`) with a signature added: used when it is signed by a key in `public_keys` and the text is as it was.
/// Why a file is not used is the error: unsigned (the first format of this file was), signed by a key nobody here knows, or changed after it was signed.
pub fn parse_signed(text: &str, public_keys: &[Vec<u8>]) -> Result<Promoted, String> {
    let file: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| format!("promoted_config.json: {e}"))?;
    let field = |name: &str| file.get(name).and_then(|v| v.as_str()).filter(|s| !s.is_empty());
    let (Some(payload), Some(signature), Some(key_id)) = (field("payload"), field("signature"), field("key_id")) else {
        return Err("promoted_config.json is not signed, so it is not used (a person signs a promotion with `aqua-cli sign-promoted`)".into());
    };
    let Some(public) = public_keys.iter().find(|k| key_id_of(k) == key_id) else {
        return Err(format!("promoted_config.json is signed by key {key_id}, which is not one this installation knows, so it is not used"));
    };
    let Some(sig) = crate::audit_key::unhex(signature) else {
        return Err("promoted_config.json: the signature cannot be read, so the file is not used".into());
    };
    if UnparsedPublicKey::new(&ED25519, public).verify(&signed_bytes(payload), &sig).is_err() {
        return Err("promoted_config.json: the signature does not match what the file says (it was changed after it was signed), so it is not used".into());
    }
    parse_payload(payload).map_err(|e| format!("promoted_config.json: {e}"))
}

pub fn load(data_dir: &Path) -> Result<Option<Promoted>, String> {
    let text = match std::fs::read_to_string(path(data_dir)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("promoted_config.json: {e}")),
    };
    parse_signed(&text, &crate::audit_key::known_public_keys(data_dir)).map(Some)
}

/// What a person is shown before signing: every promotion with the flags it sets, and the flags that result.
pub fn summary(p: &Promoted) -> String {
    let mut out = String::new();
    for (i, e) in p.promotions.iter().enumerate() {
        let flags: Vec<String> = e.flags.iter().map(|(k, v)| format!("{k}={v}")).collect();
        out.push_str(&format!("  {}. {} sets {}{}\n", i + 1, if e.label.is_empty() { "(no label)" } else { &e.label }, flags.join(", "), if e.union_measured { "" } else { " (not measured together with the ones before it)" }));
    }
    let all: Vec<String> = p.flags.iter().map(|(k, v)| format!("{k}={v}")).collect();
    out.push_str(&format!("Aqua will start with: {}\n", all.join(", ")));
    out
}

/// Turns the unsigned file `quality.py promote` writes (`{ "schema": 2, "payload": "..." }`) into a signed one, with the key that `signer` holds. The promotions are read
/// again first and refused if any flag in them, or its value, is not one in [`ALLOWED`]: the audit key is never put to a promotion that could set what a score may not.
pub fn sign_pending(pending: &str, signer: &dyn HeadSigner) -> Result<String, String> {
    let file: serde_json::Value = serde_json::from_str(pending.trim_start_matches('\u{feff}')).map_err(|e| format!("the pending promotion cannot be read: {e}"))?;
    let payload = file.get("payload").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).ok_or("the pending promotion has no payload")?;
    let promoted = parse_payload(payload)?;
    if promoted.promotions.is_empty() {
        return Err("the pending promotion promotes nothing".into());
    }
    let mut refused = Vec::new();
    let mut all: Vec<(&String, &String)> = promoted.flags.iter().collect();
    all.extend(promoted.promotions.iter().flat_map(|e| e.flags.iter()));
    for (name, value) in all {
        match ALLOWED.iter().find(|(n, _)| n == name) {
            None => refused.push(format!("{name}: not a flag a promotion may set")),
            Some((_, values)) if !values.contains(&value.as_str()) => refused.push(format!("{name}={value}: not an allowed value ({})", values.join(" or "))),
            Some(_) => {}
        }
    }
    if !refused.is_empty() {
        refused.sort();
        refused.dedup();
        return Err(format!("not signed: {}", refused.join("; ")));
    }
    let signature = signer.sign(&signed_bytes(payload));
    let signed = serde_json::json!({ "schema": 2, "payload": payload, "key_id": signer.key_id(), "signature": hex_lower(&signature) });
    serde_json::to_string_pretty(&signed).map(|s| s + "\n").map_err(|e| e.to_string())
}

/// What applying the file did: the flags it set, and what it left alone and why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    pub set: Vec<String>,
    pub left: Vec<String>,
}

/// Sets the promoted flags through `set` (the environment, or a stand-in in tests), skipping what `is_set` says the person already set.
pub fn apply_with(promoted: &Promoted, is_set: &dyn Fn(&str) -> bool, set: &mut dyn FnMut(&str, &str)) -> Applied {
    let mut out = Applied::default();
    for (name, value) in &promoted.flags {
        let Some((_, values)) = ALLOWED.iter().find(|(n, _)| n == name) else {
            out.left.push(format!("{name}: not a flag a promotion may set"));
            continue;
        };
        if !values.contains(&value.as_str()) {
            out.left.push(format!("{name}={value}: not an allowed value ({})", values.join(" or ")));
            continue;
        }
        if is_set(name) {
            out.left.push(format!("{name}: you set it yourself, so it stays as you set it"));
            continue;
        }
        set(name, value);
        out.set.push(format!("{name}={value}"));
    }
    out
}

/// At start: reads the file and sets the flags in the environment. Returns what was done (for the log), or the reason the file could not be used.
pub fn apply(data_dir: &Path) -> Result<Option<(String, Applied)>, String> {
    let Some(p) = load(data_dir)? else { return Ok(None) };
    let applied = apply_with(&p, &|n| std::env::var_os(n).is_some(), &mut |n, v| std::env::set_var(n, v));
    Ok(Some((p.describe(), applied)))
}

/// `aqua-cli sign-promoted`: reads the pending promotion at `pending`, asks `confirm` (given what the file holds) whether to sign it, and writes the signed file as
/// `promoted_config.json` in `data_dir`. The key is the audit key already held in `secrets`: none is made here. Returns what was done, for the person.
pub fn sign_file(secrets: &dyn aqua_native_tools::secrets::Secrets, data_dir: &Path, pending: &Path, confirm: &dyn Fn(&str) -> bool) -> Result<String, String> {
    let text = std::fs::read_to_string(pending).map_err(|e| format!("{}: {e}", pending.display()))?;
    let Some(signer) = crate::audit_key::existing_signer(secrets)? else {
        return Err("there is no audit signing key yet (it is made the first time Aqua starts with the tool host), so nothing can be signed".into());
    };
    // Read once to show what it holds, and again, inside `sign_pending`, to refuse what may not be signed.
    let payload = serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}')).ok().and_then(|v| v.get("payload").and_then(|p| p.as_str()).map(String::from)).unwrap_or_default();
    let shown = parse_payload(&payload)?;
    if !confirm(&format!("This promotion would be signed with key {}:\n{}", signer.key_id(), summary(&shown))) {
        return Err("not signed: it was not confirmed".into());
    }
    let signed = sign_pending(&text, &signer)?;
    let out = path(data_dir);
    aqua_tool_host::fsutil::write_atomic_durable(&out, signed.as_bytes()).map_err(|e| e.to_string())?;
    Ok(format!("Signed with key {}: {}\nAqua uses it the next time it starts.\n{}", signer.key_id(), out.display(), summary(&shown)))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::audit_key;
    use aqua_native_tools::secrets::{MemorySecrets, Secrets};

    fn promoted(flags: &[(&str, &str)]) -> Promoted {
        Promoted { label: "test".into(), flags: flags.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), promotions: vec![] }
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_promoted_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("tools")).unwrap();
        d
    }

    /// A pending file as `quality.py promote` writes it: the promotions as one text.
    fn pending(promotions: &[(&str, &[(&str, &str)], bool)]) -> String {
        let mut flags = serde_json::Map::new();
        let list: Vec<serde_json::Value> = promotions
            .iter()
            .map(|(label, fl, measured)| {
                for (k, v) in fl.iter() {
                    flags.insert(k.to_string(), serde_json::json!(v));
                }
                serde_json::json!({ "label": label, "candidate": 1, "promoted_at": "2026-10-08T10:00:00", "flags": fl.iter().map(|(k, v)| (k.to_string(), serde_json::json!(v))).collect::<serde_json::Map<_, _>>(), "evidence": { "result": 3, "tasks_compared": { "AQUA_CONTEXT": ["AB-T1", "AB-T2", "AB-T3"] } }, "replaced": [], "union_measured": measured })
            })
            .collect();
        let payload = serde_json::json!({ "schema": 2, "promoted_at": "2026-10-08T10:00:00", "flags": flags, "promotions": list }).to_string();
        serde_json::json!({ "schema": 2, "payload": payload }).to_string()
    }

    /// A signed file in `d`, by the key in `secrets` (made if there is none; it is listed among the keys this installation knows).
    fn signed_in(d: &Path, secrets: &MemorySecrets, pending_text: &str) -> String {
        let signer = audit_key::load_or_create(secrets, d).unwrap();
        let text = sign_pending(pending_text, &signer).unwrap();
        std::fs::write(path(d), &text).unwrap();
        text
    }

    #[test]
    fn a_flag_that_changes_what_is_asked_or_allowed_is_never_one_a_measurement_may_set() {
        for flag in ["AQUA_CAPS", "AQUA_TOOL_SEARCH", "AQUA_ONE_DOOR", "AQUA_DOOR", "AQUA_ROUTER_V2"] {
            assert!(!ALLOWED.iter().any(|(n, _)| *n == flag), "{flag} changes what is asked or what the agent may reach: the person switches it, not a score");
        }
        let p = promoted(&[("AQUA_CAPS", "on"), ("AQUA_TOOL_SEARCH", "on")]);
        let mut set: Vec<(String, String)> = Vec::new();
        let out = apply_with(&p, &|_| false, &mut |n, v| set.push((n.to_string(), v.to_string())));
        assert!(set.is_empty() && out.left.len() == 2, "{out:?}");
    }

    #[test]
    fn only_the_listed_flags_with_the_listed_values_are_set_and_what_the_person_set_stays() {
        let p = promoted(&[("AQUA_CONTEXT", "v2"), ("AQUA_PLAN", "on"), ("AQUA_PARENT_PID", "1"), ("AQUA_PLANNER_BATCH", "9"), ("PATH", "x")]);
        let mut set: Vec<(String, String)> = Vec::new();
        let out = apply_with(&p, &|n| n == "AQUA_PLAN", &mut |n, v| set.push((n.to_string(), v.to_string())));
        assert_eq!(set, vec![("AQUA_CONTEXT".to_string(), "v2".to_string())]);
        assert_eq!(out.set, vec!["AQUA_CONTEXT=v2".to_string()]);
        assert_eq!(out.left.len(), 4, "{:?}", out.left);
        assert!(out.left.iter().any(|l| l.contains("AQUA_PLAN") && l.contains("you set it yourself")));
        assert!(out.left.iter().any(|l| l.contains("AQUA_PARENT_PID") && l.contains("not a flag")));
        assert!(out.left.iter().any(|l| l.contains("AQUA_PLANNER_BATCH=9") && l.contains("2 or 3")));
        assert!(out.left.iter().any(|l| l.starts_with("PATH")));
    }

    #[test]
    fn a_missing_file_is_nothing_and_a_wrong_one_is_an_error_that_names_it() {
        let d = dir("missing");
        std::fs::remove_dir_all(&d).unwrap();
        assert!(load(&d).unwrap().is_none());
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(path(&d), "not json").unwrap();
        assert!(load(&d).unwrap_err().contains("promoted_config.json"));
    }

    /// Card R6-13 (T6-13.5): a promotion a person signed is used, with its evidence and its labels.
    #[test]
    fn a_promotion_signed_with_the_audit_key_is_read_back_with_what_it_holds() {
        let d = dir("signed");
        let secrets = MemorySecrets::default();
        signed_in(&d, &secrets, &pending(&[("context v2", &[("AQUA_CONTEXT", "v2")], true)]));
        let p = load(&d).unwrap().expect("a signed file is used");
        assert_eq!((p.flags.get("AQUA_CONTEXT").map(String::as_str), p.promotions.len(), p.describe().as_str()), (Some("v2"), 1, "context v2"));
        let mut set: Vec<(String, String)> = Vec::new();
        let out = apply_with(&p, &|_| false, &mut |n, v| set.push((n.to_string(), v.to_string())));
        assert_eq!((set, out.set), (vec![("AQUA_CONTEXT".to_string(), "v2".to_string())], vec!["AQUA_CONTEXT=v2".to_string()]));
    }

    #[test]
    fn a_signed_file_whose_text_was_changed_afterwards_is_not_used_and_the_error_says_why() {
        let d = dir("edited");
        let secrets = MemorySecrets::default();
        let text = signed_in(&d, &secrets, &pending(&[("context v2", &[("AQUA_CONTEXT", "v2")], true)]));
        // The flag is swapped for another that is allowed: still not what was signed.
        // (In the file the payload is a text inside a text, so its quotes are escaped.)
        let edited = text.replace("AQUA_CONTEXT", "AQUA_PLAN").replace(r#"\"v2\""#, r#"\"on\""#);
        assert_ne!(edited, text);
        std::fs::write(path(&d), &edited).unwrap();
        assert!(load(&d).unwrap_err().contains("signature does not match"), "{:?}", load(&d));
        // One more flag added to the text, the signature left as it was.
        let wider = text.replace(r#"\"flags\":{\"AQUA_CONTEXT\":\"v2\"}"#, r#"\"flags\":{\"AQUA_CONTEXT\":\"v2\",\"AQUA_READER\":\"on\"}"#);
        assert_ne!(wider, text, "{text}");
        std::fs::write(path(&d), &wider).unwrap();
        assert!(load(&d).unwrap_err().contains("signature does not match"), "{:?}", load(&d));
        // And a signature cut short or not hex.
        let bad: serde_json::Value = serde_json::from_str(&text).unwrap();
        for sig in ["zz", "abcd", ""] {
            let mut v = bad.clone();
            v["signature"] = serde_json::json!(sig);
            std::fs::write(path(&d), v.to_string()).unwrap();
            assert!(load(&d).is_err(), "{sig:?}");
        }
        // Put back as it was, it is used again.
        std::fs::write(path(&d), &text).unwrap();
        assert!(load(&d).unwrap().is_some());
    }

    #[test]
    fn a_file_signed_by_a_key_this_installation_does_not_know_is_not_used() {
        let mine = dir("stranger_mine");
        let theirs = dir("stranger_theirs");
        let _ = audit_key::load_or_create(&MemorySecrets::default(), &mine).unwrap();
        let text = signed_in(&theirs, &MemorySecrets::default(), &pending(&[("context v2", &[("AQUA_CONTEXT", "v2")], true)]));
        std::fs::write(path(&mine), &text).unwrap();
        let e = load(&mine).unwrap_err();
        assert!(e.contains("not one this installation knows"), "{e}");
        // A key that was signed with and is in the list of known keys is used, even when it is no longer the current one (after a rotation).
        let secrets = MemorySecrets::default();
        let text = signed_in(&mine, &secrets, &pending(&[("plan", &[("AQUA_PLAN", "on")], true)]));
        audit_key::rotate(&secrets, &mine).unwrap();
        std::fs::write(path(&mine), &text).unwrap();
        assert!(load(&mine).unwrap().is_some(), "the earlier key is still in the list");
    }

    #[test]
    fn a_file_that_is_not_signed_is_not_used_whatever_format_it_is_in() {
        let d = dir("unsigned");
        let _ = audit_key::load_or_create(&MemorySecrets::default(), &d).unwrap();
        for text in [
            pending(&[("context v2", &[("AQUA_CONTEXT", "v2")], true)]),
            "\u{feff}{\"schema\":1,\"label\":\"ctx\",\"flags\":{\"AQUA_CONTEXT\":\"v2\"},\"evidence\":{}}".to_string(),
            "{\"flags\":{\"AQUA_CONTEXT\":\"v2\"}}".to_string(),
            "{}".to_string(),
        ] {
            std::fs::write(path(&d), &text).unwrap();
            let e = load(&d).unwrap_err();
            assert!(e.contains("not signed"), "{text}: {e}");
        }
        // Not even when it names a key and a signature that are empty.
        std::fs::write(path(&d), "{\"schema\":2,\"payload\":\"{}\",\"key_id\":\"\",\"signature\":\"\"}").unwrap();
        assert!(load(&d).unwrap_err().contains("not signed"));
    }

    #[test]
    fn a_signature_made_without_the_domain_prefix_is_not_one_for_this_file() {
        let d = dir("domain");
        let secrets = MemorySecrets::default();
        let signer = audit_key::load_or_create(&secrets, &d).unwrap();
        let text = pending(&[("context v2", &[("AQUA_CONTEXT", "v2")], true)]);
        let payload = serde_json::from_str::<serde_json::Value>(&text).unwrap()["payload"].as_str().unwrap().to_string();
        // The key signs tree heads of the ledger: a signature over bare text, as a head's hash would be, is not accepted for the file.
        let bare = signer.sign(payload.as_bytes());
        let file = serde_json::json!({ "schema": 2, "payload": payload, "key_id": signer.key_id(), "signature": hex_lower(&bare) });
        std::fs::write(path(&d), file.to_string()).unwrap();
        assert!(load(&d).unwrap_err().contains("signature does not match"));
        // With the domain it is the same call that `sign_pending` makes.
        let right = signer.sign(&signed_bytes(&payload));
        let file = serde_json::json!({ "schema": 2, "payload": payload, "key_id": signer.key_id(), "signature": hex_lower(&right) });
        std::fs::write(path(&d), file.to_string()).unwrap();
        assert!(load(&d).unwrap().is_some());
    }

    #[test]
    fn the_audit_key_is_never_put_to_a_promotion_that_sets_what_a_score_may_not_or_one_that_is_empty() {
        let d = dir("refuse_sign");
        let signer = audit_key::load_or_create(&MemorySecrets::default(), &d).unwrap();
        for (flags, why) in [
            (&[("AQUA_CAPS", "on")][..], "AQUA_CAPS: not a flag a promotion may set"),
            (&[("AQUA_TOOL_SEARCH", "on")][..], "not a flag"),
            (&[("PATH", "x")][..], "PATH: not a flag"),
            (&[("AQUA_PLANNER_BATCH", "9")][..], "AQUA_PLANNER_BATCH=9: not an allowed value (2 or 3)"),
            (&[("AQUA_CONTEXT", "v2"), ("AQUA_ONE_DOOR", "on")][..], "AQUA_ONE_DOOR"),
        ] {
            let e = sign_pending(&pending(&[("x", flags, true)]), &signer).unwrap_err();
            assert!(e.starts_with("not signed") && e.contains(why), "{flags:?}: {e}");
        }
        assert!(sign_pending(&pending(&[]), &signer).unwrap_err().contains("promotes nothing"));
        assert!(sign_pending("not json", &signer).is_err());
        assert!(sign_pending("{\"schema\":2}", &signer).unwrap_err().contains("no payload"));
        assert!(sign_pending("{\"schema\":2,\"payload\":\"{\\\"schema\\\":1}\"}", &signer).unwrap_err().contains("schema 2"));
    }

    /// The payload as `scripts/eval/quality.py promote` writes it (its keys sorted, no spaces) is read as it is meant.
    #[test]
    fn the_payload_the_promotion_script_writes_is_read_with_its_stacking_note() {
        let payload = r#"{"flags":{"AQUA_CONTEXT":"v2","AQUA_PLAN":"on"},"promoted_at":"2026-10-08T10:00:00","promotions":[{"candidate":1,"evidence":{"baseline_digest":"ab","min_runs":3,"min_tasks":3,"pass_at_1":1.0,"result":4,"suite":"smoke","tasks_compared":{"AQUA_CONTEXT":["AB-T1","AB-T2","AB-T3"]},"ttc_p50":1.0},"flags":{"AQUA_CONTEXT":"v2"},"label":"context","promoted_at":"2026-10-08T10:00:00","replaced":[],"union_measured":true},{"candidate":2,"evidence":{},"flags":{"AQUA_PLAN":"on"},"label":"plan","promoted_at":"2026-10-08T10:05:00","replaced":[],"union_measured":false}],"schema":2}"#;
        let p = parse_payload(payload).unwrap();
        assert_eq!(p.promotions.len(), 2);
        assert!(p.promotions[0].union_measured && !p.promotions[1].union_measured);
        assert_eq!(p.describe(), "context + plan (stacked: the promotions together were not measured)");
        assert!(summary(&p).contains("plan sets AQUA_PLAN=on (not measured together with the ones before it)") && summary(&p).contains("Aqua will start with: AQUA_CONTEXT=v2, AQUA_PLAN=on"));
        // A payload without the list of flags in effect has them worked out from the promotions, the later replacing the earlier.
        let bare = r#"{"schema":2,"promotions":[{"label":"a","flags":{"AQUA_PLANNER_BATCH":"2"}},{"label":"b","flags":{"AQUA_PLANNER_BATCH":"3"}}]}"#;
        assert_eq!(parse_payload(bare).unwrap().flags.get("AQUA_PLANNER_BATCH").map(String::as_str), Some("3"));
        assert!(parse_payload("{\"schema\":1}").is_err() && parse_payload("[]").is_err() && parse_payload("nope").is_err());
    }

    #[test]
    fn signing_a_file_needs_the_key_and_a_yes_and_writes_the_promoted_file_only_then() {
        let d = dir("sign_file");
        let pending_file = d.join("promoted_config.pending.json");
        std::fs::write(&pending_file, pending(&[("context v2", &[("AQUA_CONTEXT", "v2")], true)])).unwrap();
        // No key has been made: nothing is signed, and no key is made here.
        let secrets = MemorySecrets::default();
        assert!(sign_file(&secrets, &d, &pending_file, &|_| true).unwrap_err().contains("no audit signing key"));
        assert!(secrets.get("aqua/audit-signing-key").unwrap().is_none() && !path(&d).exists());
        let _ = audit_key::load_or_create(&secrets, &d).unwrap();
        // The person is shown what is held and says no.
        let shown = std::sync::Mutex::new(String::new());
        let e = sign_file(&secrets, &d, &pending_file, &|s| {
            *shown.lock().unwrap() = s.to_string();
            false
        })
        .unwrap_err();
        assert!(e.contains("not confirmed") && !path(&d).exists());
        let seen = shown.lock().unwrap().clone();
        assert!(seen.contains("context v2 sets AQUA_CONTEXT=v2") && seen.contains("Aqua will start with: AQUA_CONTEXT=v2"), "{seen}");
        // And yes: the file is there, and the core reads it.
        let done = sign_file(&secrets, &d, &pending_file, &|_| true).unwrap();
        assert!(done.contains("Signed with key") && path(&d).exists(), "{done}");
        assert_eq!(load(&d).unwrap().unwrap().flags.get("AQUA_CONTEXT").map(String::as_str), Some("v2"));
        assert!(sign_file(&secrets, &d, &d.join("missing.json"), &|_| true).is_err());
    }

    #[test]
    fn the_list_is_the_one_the_promotion_script_checks_against() {
        let script = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("scripts").join("eval").join("quality.py")).expect("quality.py");
        let block = script.split("ALLOWED_FLAGS: Dict").nth(1).and_then(|r| r.split('}').next()).expect("the list");
        let re = regex::Regex::new(r#""(AQUA_[A-Z_]+)":\s*\(([^)]*)\)"#).unwrap();
        let from_script: Vec<(String, Vec<String>)> = re
            .captures_iter(block)
            .map(|c| (c[1].to_string(), c[2].split(',').map(|v| v.trim().trim_matches('"').to_string()).filter(|v| !v.is_empty()).collect()))
            .collect();
        let ours: Vec<(String, Vec<String>)> = ALLOWED.iter().map(|(n, vs)| (n.to_string(), vs.iter().map(|v| v.to_string()).collect())).collect();
        assert_eq!(from_script, ours);
    }
}
