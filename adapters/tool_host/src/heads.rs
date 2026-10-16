//! Signed tree heads for the audit ledger.
//!
//! The ledger's lines are hash-chained, which shows that a line was edited, removed or reordered *if the rest of the file is left as it was*. Code that
//! runs as the same user can rewrite the whole file with a new, consistent chain. A **tree head** closes that: every [`HEAD_EVERY`] lines (and when the
//! ledger is sealed) the Merkle root of those lines' hashes (see [`crate::merkle`]) is written to `<ledger>.heads.jsonl`, chained to the head before it and
//! **signed with a key the agent cannot reach** (held by the program, outside the files the tools may touch). A rewritten ledger no longer matches the
//! signed roots, and a person can show one run's lines with their proofs to somebody else, who needs nothing but the public key to check them.
//!
//! This module has no idea where the key lives: the program gives the ledger a [`HeadSigner`]. Verification needs only the public key.

use crate::canon::hex_lower;
use crate::merkle::{self, Hash};
use ring::signature::{UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// How many ledger lines one head covers.
pub const HEAD_EVERY: usize = 64;
/// What the first head chains to.
pub const GENESIS_HEAD: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Signs tree heads. The program implements it with a key kept where the agent's tools cannot read it.
pub trait HeadSigner: Send + Sync {
    /// A short name for the public key (see [`key_id_of`]).
    fn key_id(&self) -> String;
    fn public_key(&self) -> Vec<u8>;
    fn sign(&self, message: &[u8]) -> Vec<u8>;
}

/// The short name of a public key: the first eight bytes of its SHA-256, in hex.
pub fn key_id_of(public_key: &[u8]) -> String {
    Sha256::digest(public_key).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// One signed tree head: a batch of ledger lines, their Merkle root, the head before it, and the signature over all of that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub index: u64,
    pub from_seq: u64,
    pub to_seq: u64,
    pub count: u64,
    pub root: String,
    pub prev_head: String,
    pub ts_unix_ms: u64,
    pub key_id: String,
    pub hash: String,
    pub signature: String,
}

pub fn head_hash(prev_head: &str, from_seq: u64, to_seq: u64, count: u64, root: &str, ts: u64, key_id: &str) -> String {
    let mut h = Sha256::new();
    for part in [prev_head, &from_seq.to_string(), &to_seq.to_string(), &count.to_string(), root, &ts.to_string(), key_id] {
        h.update(part.as_bytes());
        h.update(b"\n");
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()).collect()
}

/// The root over a batch of `(seq, line hash)`.
pub fn root_of(batch: &[(u64, String)]) -> Hash {
    merkle::root(&batch.iter().map(|(_, h)| merkle::leaf(h)).collect::<Vec<_>>())
}

/// A signed head over `batch` (consecutive lines, oldest first).
pub fn make_head(signer: &dyn HeadSigner, index: u64, prev_head: &str, batch: &[(u64, String)], ts: u64) -> Option<Head> {
    let (from_seq, to_seq) = (batch.first()?.0, batch.last()?.0);
    let root = merkle::hex(&root_of(batch));
    let key_id = signer.key_id();
    let count = batch.len() as u64;
    let hash = head_hash(prev_head, from_seq, to_seq, count, &root, ts, &key_id);
    let signature = hex_lower(&signer.sign(hash.as_bytes()));
    Some(Head { index, from_seq, to_seq, count, root, prev_head: prev_head.to_string(), ts_unix_ms: ts, key_id, hash, signature })
}

pub fn heads_path(ledger: &Path) -> PathBuf {
    ledger.with_extension("heads.jsonl")
}

/// Every head in the file, oldest first. A line that does not parse is a problem (reported by [`verify_chain`]), so it is kept as an empty slot.
pub fn read_heads(path: &Path) -> Vec<Result<Head, String>> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    text.lines().filter(|l| !l.trim().is_empty()).enumerate().map(|(i, l)| serde_json::from_str::<Head>(l).map_err(|e| format!("head line {}: {e}", i + 1))).collect()
}

/// What checking the heads found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadsReport {
    pub heads: usize,
    /// How many ledger lines are under a head whose root matches what the lines say.
    pub covered_lines: u64,
    /// Heads whose lines are no longer all kept (the ledger rotated them away): the signature and the chain are checked, the root cannot be.
    pub unchecked_heads: usize,
    /// What is wrong, in words. Empty when everything that can be checked is right.
    pub problems: Vec<String>,
}

/// One head on its own: it is what it says (its hash), and carries a valid signature by a key in `public_keys`.
pub fn verify_head(head: &Head, public_keys: &[Vec<u8>]) -> Vec<String> {
    let label = format!("head {}", head.index);
    let mut problems = Vec::new();
    if head_hash(&head.prev_head, head.from_seq, head.to_seq, head.count, &head.root, head.ts_unix_ms, &head.key_id) != head.hash {
        problems.push(format!("{label}: its hash is not what it says (it was changed)"));
    }
    let key = public_keys.iter().find(|k| key_id_of(k) == head.key_id);
    match (key, unhex(&head.signature)) {
        (None, _) => problems.push(format!("{label}: signed by key {}, which is not one that is known", head.key_id)),
        (Some(_), None) => problems.push(format!("{label}: the signature cannot be read")),
        (Some(k), Some(sig)) => {
            if UnparsedPublicKey::new(&ED25519, k).verify(head.hash.as_bytes(), &sig).is_err() {
                problems.push(format!("{label}: the signature is not valid"));
            }
        }
    }
    problems
}

/// The chain of heads: each is what it says (its hash), follows the one before it, is numbered after it, and carries a valid signature by a key in
/// `public_keys`.
pub fn verify_chain(heads: &[Result<Head, String>], public_keys: &[Vec<u8>]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut prev: Option<&Head> = None;
    let mut expected_prev_hash = GENESIS_HEAD.to_string();
    for (i, h) in heads.iter().enumerate() {
        let head = match h {
            Ok(h) => h,
            Err(e) => {
                problems.push(format!("a head cannot be read ({e})"));
                prev = None;
                continue;
            }
        };
        let label = format!("head {}", head.index);
        problems.extend(verify_head(head, public_keys));
        // The first head the file holds may start anywhere (older ones may have been rotated away on purpose), unless it is number 1.
        if let Some(p) = prev {
            if head.prev_head != p.hash {
                problems.push(format!("{label}: it does not follow head {} (one was removed or changed)", p.index));
            }
            if head.index != p.index + 1 {
                problems.push(format!("{label}: it should be number {}", p.index + 1));
            }
            if head.from_seq != p.to_seq + 1 {
                problems.push(format!("{label}: it starts at line {} but the head before it ended at {}", head.from_seq, p.to_seq));
            }
        } else if i == 0 && head.index == 1 && head.prev_head != expected_prev_hash {
            problems.push(format!("{label}: the first head does not start from the beginning"));
        }
        expected_prev_hash = head.hash.clone();
        prev = Some(head);
    }
    problems
}

/// The heads against the ledger's lines (`seq` -> line hash, for the lines still kept): the root of each head's batch is recomputed.
///
/// Lines may be missing **only from the front**, where the ledger rotated its oldest file away on purpose. A signed line that is missing anywhere else (a gap
/// in the middle, or the end of the ledger cut off) is a problem, and so is a ledger with none of the lines that were signed: otherwise deleting the lines
/// a head covers would look like a rotation.
pub fn verify_against_lines(heads: &[Result<Head, String>], lines: &BTreeMap<u64, String>) -> HeadsReport {
    let mut report = HeadsReport::default();
    let (oldest, newest) = (lines.keys().next().copied(), lines.keys().next_back().copied());
    // The kept lines are one unbroken run of sequence numbers.
    if let (Some(o), Some(n)) = (oldest, newest) {
        if lines.len() as u64 != n - o + 1 {
            let first_gap = (o..=n).find(|s| !lines.contains_key(s)).unwrap_or(o);
            report.problems.push(format!("the ledger is missing line {first_gap} (and the lines kept are not one unbroken run: lines were removed from the middle)"));
        }
    }
    for h in heads.iter().filter_map(|h| h.as_ref().ok()) {
        report.heads += 1;
        let batch: Vec<(u64, String)> = (h.from_seq..=h.to_seq).filter_map(|s| lines.get(&s).map(|x| (s, x.clone()))).collect();
        if batch.len() as u64 != h.to_seq - h.from_seq + 1 {
            // Missing lines are allowed to be those older than the oldest line kept (rotated away); nothing else may be.
            let removed = (h.from_seq..=h.to_seq).any(|s| !lines.contains_key(&s) && oldest.is_none_or(|o| s >= o));
            if removed {
                report.problems.push(format!("head {}: lines {} to {} were signed but some are no longer in the ledger (they were removed, or the end of the ledger was cut off)", h.index, h.from_seq, h.to_seq));
            } else {
                report.unchecked_heads += 1;
            }
            continue;
        }
        if batch.len() as u64 != h.count {
            report.problems.push(format!("head {}: it says {} lines but covers lines {} to {}", h.index, h.count, h.from_seq, h.to_seq));
        } else if merkle::hex(&root_of(&batch)) != h.root {
            report.problems.push(format!("head {}: lines {} to {} are not the lines that were signed (they were changed)", h.index, h.from_seq, h.to_seq));
        } else {
            report.covered_lines += h.count;
        }
    }
    report
}

/// The proof that line `seq` is under `head`: the hashes up to the root, each with whether it sits to the left. `None` when the head does not cover it
/// or a line of the batch is missing.
pub fn proof_for(head: &Head, lines: &BTreeMap<u64, String>, seq: u64) -> Option<Vec<(String, bool)>> {
    if seq < head.from_seq || seq > head.to_seq {
        return None;
    }
    let batch: Vec<(u64, String)> = (head.from_seq..=head.to_seq).filter_map(|s| lines.get(&s).map(|x| (s, x.clone()))).collect();
    if batch.len() as u64 != head.to_seq - head.from_seq + 1 {
        return None;
    }
    let leaves: Vec<Hash> = batch.iter().map(|(_, h)| merkle::leaf(h)).collect();
    let proof = merkle::inclusion_proof(&leaves, (seq - head.from_seq) as usize)?;
    Some(proof.into_iter().map(|(h, left)| (merkle::hex(&h), left)).collect())
}

/// Does a line with hash `line_hash` and `proof` belong under `head`? (What somebody who is shown one run checks, with the head and the public key.)
pub fn verify_proof(head: &Head, line_hash: &str, proof: &[(String, bool)]) -> bool {
    let Some(root) = merkle::from_hex(&head.root) else { return false };
    let mut steps = Vec::new();
    for (h, left) in proof {
        match merkle::from_hex(h) {
            Some(x) => steps.push((x, *left)),
            None => return false,
        }
    }
    merkle::verify_inclusion(&merkle::leaf(line_hash), &steps, &root)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    /// A signer with a key made for the test.
    pub struct TestSigner {
        pair: Ed25519KeyPair,
    }

    impl TestSigner {
        pub fn new() -> Self {
            let rng = ring::rand::SystemRandom::new();
            let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).expect("a key");
            Self { pair: Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("a key pair") }
        }
    }

    impl HeadSigner for TestSigner {
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
}

#[cfg(test)]
mod tests {
    use super::test_support::TestSigner;
    use super::*;

    fn batch(from: u64, n: u64) -> Vec<(u64, String)> {
        (from..from + n).map(|s| (s, format!("{s:064x}"))).collect()
    }

    fn lines_of(batches: &[Vec<(u64, String)>]) -> BTreeMap<u64, String> {
        batches.iter().flatten().cloned().collect()
    }

    #[test]
    fn a_chain_of_heads_checks_out_and_each_thing_that_can_be_changed_is_found() {
        let signer = TestSigner::new();
        let keys = vec![signer.public_key()];
        let (b1, b2, b3) = (batch(1, 64), batch(65, 64), batch(129, 10));
        let h1 = make_head(&signer, 1, GENESIS_HEAD, &b1, 1_000).unwrap();
        let h2 = make_head(&signer, 2, &h1.hash, &b2, 2_000).unwrap();
        let h3 = make_head(&signer, 3, &h2.hash, &b3, 3_000).unwrap();
        let ok = |hs: &[Head]| hs.iter().cloned().map(Ok).collect::<Vec<Result<Head, String>>>();
        assert!(verify_chain(&ok(&[h1.clone(), h2.clone(), h3.clone()]), &keys).is_empty());
        let lines = lines_of(&[b1.clone(), b2.clone(), b3.clone()]);
        let report = verify_against_lines(&ok(&[h1.clone(), h2.clone(), h3.clone()]), &lines);
        assert_eq!((report.heads, report.covered_lines, report.unchecked_heads, report.problems.is_empty()), (3, 138, 0, true), "{report:?}");
        // A head removed from the middle.
        assert!(verify_chain(&ok(&[h1.clone(), h3.clone()]), &keys).iter().any(|p| p.contains("does not follow head 1")));
        // Heads in the wrong order.
        assert!(!verify_chain(&ok(&[h2.clone(), h1.clone()]), &keys).is_empty());
        // A field changed after signing (the hash no longer says it).
        let mut forged = h2.clone();
        forged.root = merkle::hex(&root_of(&batch(1000, 3)));
        assert!(verify_chain(&ok(&[h1.clone(), forged, h3.clone()]), &keys).iter().any(|p| p.contains("it was changed")));
        // A head made again with a different key is signed, but not by a key that is known.
        let stranger = TestSigner::new();
        let forged = make_head(&stranger, 2, &h1.hash, &b2, 2_000).unwrap();
        assert!(verify_chain(&ok(&[h1.clone(), forged.clone()]), &keys).iter().any(|p| p.contains("not one that is known")));
        // The same head with a signature copied from another is not valid.
        let mut bad_sig = h2.clone();
        bad_sig.signature = h1.signature.clone();
        assert!(verify_chain(&ok(&[h1.clone(), bad_sig]), &keys).iter().any(|p| p.contains("signature is not valid")));
        // A line of the ledger changed, deleted or added: its batch no longer matches the signed root.
        let mut edited = lines.clone();
        edited.insert(70, format!("{:064x}", 99_999));
        let r = verify_against_lines(&ok(&[h1.clone(), h2.clone(), h3.clone()]), &edited);
        assert!(r.problems.iter().any(|p| p.starts_with("head 2:") && p.contains("changed")) && r.covered_lines == 74, "{r:?}");
        // A line deleted from the middle, the end of the ledger cut off, and a ledger with nothing left are all found: only the front may be missing.
        let mut cut = lines.clone();
        cut.remove(&134);
        let r = verify_against_lines(&ok(&[h1.clone(), h2.clone(), h3.clone()]), &cut);
        assert!(r.problems.iter().any(|p| p.contains("missing line 134")) && r.problems.iter().any(|p| p.starts_with("head 3:") && p.contains("removed")), "{r:?}");
        let truncated: BTreeMap<u64, String> = lines.iter().filter(|(s, _)| **s <= 130).map(|(s, h)| (*s, h.clone())).collect();
        let r = verify_against_lines(&ok(&[h1.clone(), h2.clone(), h3.clone()]), &truncated);
        assert!(r.problems.iter().any(|p| p.starts_with("head 3:") && p.contains("cut off")) && r.covered_lines == 128, "{r:?}");
        let r = verify_against_lines(&ok(&[h1.clone(), h2.clone(), h3.clone()]), &BTreeMap::new());
        assert_eq!(r.problems.len(), 3, "a ledger that holds none of the signed lines is not a rotation: {r:?}");
        // The oldest lines rotated away on purpose, in the middle of the first batch: that is allowed, and the rest still check.
        let rotated: BTreeMap<u64, String> = lines.iter().filter(|(s, _)| **s >= 40).map(|(s, h)| (*s, h.clone())).collect();
        let r = verify_against_lines(&ok(&[h1.clone(), h2.clone(), h3.clone()]), &rotated);
        assert_eq!((r.unchecked_heads, r.problems.len(), r.covered_lines), (1, 0, 74), "{r:?}");
    }

    #[test]
    fn the_oldest_heads_may_have_been_rotated_away_and_the_rest_still_check_but_a_changed_first_head_is_found() {
        let signer = TestSigner::new();
        let keys = vec![signer.public_key()];
        let (b1, b2) = (batch(1, 64), batch(65, 64));
        let h1 = make_head(&signer, 1, GENESIS_HEAD, &b1, 1).unwrap();
        let h2 = make_head(&signer, 2, &h1.hash, &b2, 2).unwrap();
        // The file starts at head 2 (head 1 was rotated away): that is allowed.
        assert!(verify_chain(&[Ok(h2.clone())], &keys).is_empty());
        // Lines 1 to 64 are gone too: head 2 still checks against its own lines.
        let r = verify_against_lines(&[Ok(h2.clone())], &lines_of(&[b2.clone()]));
        assert_eq!((r.covered_lines, r.problems.len()), (64, 0));
        // But a first head that says it is number 1 has to start from the beginning.
        let mut lie = h1.clone();
        lie.prev_head = h2.hash.clone();
        lie.hash = head_hash(&lie.prev_head, lie.from_seq, lie.to_seq, lie.count, &lie.root, lie.ts_unix_ms, &lie.key_id);
        lie.signature = hex_lower(&signer.sign(lie.hash.as_bytes()));
        assert!(verify_chain(&[Ok(lie)], &keys).iter().any(|p| p.contains("start from the beginning")));
    }

    #[test]
    fn a_key_can_be_replaced_and_the_chain_goes_on_across_the_change() {
        let (old, new) = (TestSigner::new(), TestSigner::new());
        let h1 = make_head(&old, 1, GENESIS_HEAD, &batch(1, 64), 1).unwrap();
        let h2 = make_head(&new, 2, &h1.hash, &batch(65, 64), 2).unwrap();
        let heads = vec![Ok(h1), Ok(h2)];
        assert!(verify_chain(&heads, &[old.public_key(), new.public_key()]).is_empty());
        assert!(!verify_chain(&heads, &[new.public_key()]).is_empty(), "without the old key the old head is not checkable");
    }

    #[test]
    fn one_line_is_shown_with_its_proof_and_somebody_with_only_the_head_checks_it() {
        let signer = TestSigner::new();
        let b = batch(1, 37);
        let head = make_head(&signer, 1, GENESIS_HEAD, &b, 5).unwrap();
        let lines = lines_of(&[b.clone()]);
        let proof = proof_for(&head, &lines, 20).expect("a proof");
        assert!(proof.len() <= 6);
        assert!(verify_proof(&head, &b[19].1, &proof));
        assert!(!verify_proof(&head, &b[20].1, &proof), "another line does not fit it");
        let mut other = head.clone();
        other.root = merkle::hex(&root_of(&batch(500, 37)));
        assert!(!verify_proof(&other, &b[19].1, &proof), "nor another root");
        assert!(proof_for(&head, &lines, 99).is_none() && proof_for(&head, &BTreeMap::new(), 20).is_none());
        assert!(!verify_proof(&head, &b[19].1, &[("zz".into(), true)]));
    }

    #[test]
    fn a_key_has_a_short_name_and_the_signed_head_is_what_the_signer_signed() {
        let s = TestSigner::new();
        assert_eq!(key_id_of(&s.public_key()), s.key_id());
        assert_eq!(s.key_id().len(), 16);
        assert!(make_head(&s, 1, GENESIS_HEAD, &[], 1).is_none(), "nothing to sign over");
    }
}
