//! Showing one run's records to somebody else.
//!
//! A [`RunProof`] holds the ledger lines of one run, for each the signed tree head it is under and the proof that it is in that head's batch, and the heads
//! themselves. [`verify_bundle`] checks it with nothing but the public key of the person's Aqua (given by the one who checks, not read out of the bundle:
//! a bundle that names its own key proves nothing): every line is what its hash says, every head is signed by that key, and every line is in the batch the
//! head signed. A line that is in no head yet (the newest ones, if the ledger was not sealed) is counted and said, not passed.

use crate::audit::{digest, kept_lines, AuditLedger, Line};
use crate::heads::{self, Head};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProofLine {
    pub seq: u64,
    /// The ledger line as it was written.
    pub line: Value,
    pub hash: String,
    /// The tree head this line is under, if any.
    pub head_index: Option<u64>,
    /// The hashes from the line up to that head's root, each with whether it sits to the left.
    pub proof: Vec<(String, bool)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunProof {
    pub run_id: String,
    pub lines: Vec<ProofLine>,
    /// The heads the lines are under (only those).
    pub heads: Vec<Head>,
    /// The key ids of the public keys that signed them (the keys themselves are given by whoever checks).
    pub key_ids: Vec<String>,
    /// Lines that are under a signed head whose root matches.
    pub covered: usize,
    /// Lines that are in no head yet.
    pub unsigned: usize,
    /// Whether the whole kept ledger is one unbroken chain.
    pub chain_ok: bool,
    /// What is wrong, in words: a broken chain, a head that is not what it says, a root that does not match.
    pub problems: Vec<String>,
}

/// What [`verify_bundle`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BundleReport {
    pub lines: usize,
    pub verified: usize,
    pub unsigned: usize,
    pub problems: Vec<String>,
}

impl BundleReport {
    /// Everything in the bundle that can be checked is right, and nothing was left unchecked.
    pub fn all_good(&self) -> bool {
        self.problems.is_empty() && self.unsigned == 0 && self.verified == self.lines && self.lines > 0
    }
}

/// The proof of one run's lines. The ledger is sealed first, so the newest lines are covered by a head too.
pub fn run_proof(ledger: &AuditLedger, run_id: &str, public_keys: &[Vec<u8>]) -> RunProof {
    let mut problems = Vec::new();
    if let Err(e) = ledger.seal() {
        problems.push(format!("the newest lines could not be sealed: {e}"));
    }
    let chain_ok = match ledger.verify_all() {
        Ok(_) => true,
        Err(e) => {
            problems.push(e.to_string());
            false
        }
    };
    let all = kept_lines(ledger.path());
    let hashes: BTreeMap<u64, String> = all.iter().map(|l| (l.seq, l.hash.clone())).collect();
    let head_results = heads::read_heads(&heads::heads_path(ledger.path()));
    // The ledger as a whole against what was signed: a line that was signed and is gone (not rotated away) is a problem of every run's proof, because the
    // proof of one run cannot say that a line of that run is among the ones that went.
    problems.extend(heads::verify_chain(&head_results, public_keys).into_iter().map(|p| format!("the ledger: {p}")));
    problems.extend(heads::verify_against_lines(&head_results, &hashes).problems.into_iter().map(|p| format!("the ledger: {p}")));
    let heads: Vec<Head> = head_results.into_iter().filter_map(Result::ok).collect();
    let mut lines = Vec::new();
    let mut used: Vec<Head> = Vec::new();
    let (mut covered, mut unsigned) = (0, 0);
    for l in all.iter().filter(|l| l.event.agent_run_id == run_id) {
        let head = heads.iter().find(|h| l.seq >= h.from_seq && l.seq <= h.to_seq);
        let mut entry = ProofLine { seq: l.seq, line: serde_json::to_value(l).unwrap_or(Value::Null), hash: l.hash.clone(), head_index: None, proof: Vec::new() };
        match head {
            None => unsigned += 1,
            Some(h) => {
                let head_problems = heads::verify_head(h, public_keys);
                match heads::proof_for(h, &hashes, l.seq) {
                    Some(proof) if head_problems.is_empty() && heads::verify_proof(h, &l.hash, &proof) => {
                        entry.head_index = Some(h.index);
                        entry.proof = proof;
                        covered += 1;
                        if !used.iter().any(|u| u.index == h.index) {
                            used.push(h.clone());
                        }
                    }
                    Some(_) => {
                        problems.extend(head_problems);
                        problems.push(format!("line {}: it does not match the batch that head {} signed", l.seq, h.index));
                    }
                    None => {
                        problems.push(format!("line {}: head {} covers it, but some of that batch is no longer kept, so it cannot be shown", l.seq, h.index));
                        unsigned += 1;
                    }
                }
            }
        }
        lines.push(entry);
    }
    problems.sort();
    problems.dedup();
    let mut key_ids: Vec<String> = used.iter().map(|h| h.key_id.clone()).collect();
    key_ids.sort();
    key_ids.dedup();
    RunProof { run_id: run_id.to_string(), lines, heads: used, key_ids, covered, unsigned, chain_ok, problems }
}

/// Checks a bundle against the public keys the checker trusts. See the module's note.
pub fn verify_bundle(bundle: &RunProof, trusted_keys: &[Vec<u8>]) -> BundleReport {
    let mut report = BundleReport { lines: bundle.lines.len(), ..Default::default() };
    let mut checked_heads: BTreeMap<u64, bool> = BTreeMap::new();
    for entry in &bundle.lines {
        let Ok(line) = serde_json::from_value::<Line>(entry.line.clone()) else {
            report.problems.push(format!("line {}: it cannot be read", entry.seq));
            continue;
        };
        if line.seq != entry.seq || line.hash != entry.hash || digest(&line.prev, line.seq, line.ts_unix_ms, &line.event) != line.hash {
            report.problems.push(format!("line {}: its content is not what its hash says (it was changed)", entry.seq));
            continue;
        }
        // What was checked is the record as this program reads it. A line that carries more than that (a field the hash does not cover, or a repeated key
        // that two readers would take differently) could show a reader something that was never signed, so it is refused: the line as shown has to be exactly
        // the record that was read.
        if serde_json::to_value(&line).ok().as_ref() != Some(&entry.line) {
            report.problems.push(format!("line {}: it has fields that are not part of the signed record, so what is shown is not only what was signed", entry.seq));
            continue;
        }
        if line.event.agent_run_id != bundle.run_id {
            report.problems.push(format!("line {}: it belongs to another run", entry.seq));
            continue;
        }
        let Some(index) = entry.head_index else {
            report.unsigned += 1;
            continue;
        };
        let Some(head) = bundle.heads.iter().find(|h| h.index == index) else {
            report.problems.push(format!("line {}: the head {index} it is under is not in the bundle", entry.seq));
            continue;
        };
        let head_ok = *checked_heads.entry(index).or_insert_with(|| {
            let problems = heads::verify_head(head, trusted_keys);
            let ok = problems.is_empty();
            report.problems.extend(problems);
            ok
        });
        if !head_ok {
            continue;
        }
        if entry.seq < head.from_seq || entry.seq > head.to_seq || !heads::verify_proof(head, &entry.hash, &entry.proof) {
            report.problems.push(format!("line {}: it is not in the batch that head {index} signed", entry.seq));
            continue;
        }
        report.verified += 1;
    }
    // A bundle that has no line says nothing about the run.
    if bundle.lines.is_empty() {
        report.problems.push("the bundle holds no line of this run".into());
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditEvent;
    use crate::heads::test_support::TestSigner;
    use crate::heads::HeadSigner;
    use serde_json::json;
    use std::sync::Arc;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_proof_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn event(run: &str, tool: &str) -> AuditEvent {
        AuditEvent {
            request_id: "r".into(),
            session_id: "s".into(),
            agent_run_id: run.into(),
            caller: "agent".into(),
            tool: tool.into(),
            args: json!({"path": "a.txt"}),
            decision: "auto".into(),
            classes: vec!["READ".into()],
            ok: true,
            error_code: None,
            duration_ms: 3,
            shadow: None,
            approval: None,
        }
    }

    /// A ledger with a signer and `n` lines, run "runA" on every third line.
    fn ledger(name: &str, n: usize) -> (AuditLedger, Arc<TestSigner>, std::path::PathBuf) {
        let d = dir(name);
        let path = d.join("audit.jsonl");
        let l = AuditLedger::open(&path);
        let signer = Arc::new(TestSigner::new());
        l.set_head_signer(signer.clone());
        for i in 0..n {
            l.append(&event(if i % 3 == 0 { "runA" } else { "runB" }, &format!("fs.read{i}"))).unwrap();
        }
        (l, signer, path)
    }

    #[test]
    fn a_head_is_written_every_sixty_four_lines_and_when_sealed_and_the_chain_of_heads_checks() {
        let (l, signer, path) = ledger("heads", 130);
        let heads: Vec<Head> = heads::read_heads(&heads::heads_path(&path)).into_iter().map(Result::unwrap).collect();
        assert_eq!(heads.len(), 2, "two full batches so far");
        assert_eq!((heads[0].from_seq, heads[0].to_seq, heads[1].from_seq, heads[1].to_seq), (1, 64, 65, 128));
        let sealed = l.seal().unwrap().expect("a head for the last two lines");
        assert_eq!((sealed.from_seq, sealed.to_seq, sealed.index), (129, 130, 3));
        assert!(l.seal().unwrap().is_none(), "nothing new to seal");
        let all: Vec<_> = heads::read_heads(&heads::heads_path(&path));
        assert!(heads::verify_chain(&all, &[signer.public_key()]).is_empty());
        let lines: BTreeMap<u64, String> = kept_lines(&path).into_iter().map(|x| (x.seq, x.hash)).collect();
        let r = heads::verify_against_lines(&all, &lines);
        assert_eq!((r.heads, r.covered_lines, r.problems.len()), (3, 130, 0), "{r:?}");
        // A ledger with no signer writes no head.
        let d2 = dir("nosigner");
        let plain = AuditLedger::open(d2.join("a.jsonl"));
        for i in 0..70 {
            plain.append(&event("x", &format!("t{i}"))).unwrap();
        }
        assert!(!heads::heads_path(plain.path()).exists());
    }

    /// Card R6-12: how a call was allowed is part of the line, so it is part of what a bundle signs and shows.
    #[test]
    fn an_approval_note_is_part_of_what_a_bundle_signs_and_shows() {
        let (l, signer, _) = ledger("note", 3);
        let keys = vec![signer.public_key()];
        let mut e = event("runN", "fs.write");
        e.approval = Some(crate::audit::ApprovalNote { kind: "once".into(), by: "ui".into(), reason: "the person said yes".into() });
        l.append(&e).unwrap();
        let proof = run_proof(&l, "runN", &keys);
        assert!(verify_bundle(&proof, &keys).all_good());
        assert_eq!(proof.lines[0].line["approval"]["kind"], "once");
        let mut edited = proof.clone();
        edited.lines[0].line["approval"]["kind"] = json!("session");
        let r = verify_bundle(&edited, &keys);
        assert!(!r.all_good() && r.problems.iter().any(|p| p.contains("it was changed")), "{r:?}");
    }

    #[test]
    fn a_ledger_that_is_opened_again_goes_on_with_its_heads_and_covers_the_lines_written_before() {
        let (l, signer, path) = ledger("reopen", 70);
        assert_eq!(heads::read_heads(&heads::heads_path(&path)).len(), 1);
        drop(l);
        let again = AuditLedger::open(&path);
        again.set_head_signer(signer.clone());
        for i in 0..60 {
            again.append(&event("runC", &format!("later{i}"))).unwrap();
        }
        // 6 lines were waiting from before (65 to 70) and 58 more made the batch of 64.
        let all = heads::read_heads(&heads::heads_path(&path));
        assert_eq!(all.len(), 2);
        let second = all[1].as_ref().unwrap();
        assert_eq!((second.index, second.from_seq, second.to_seq), (2, 65, 128));
        assert!(heads::verify_chain(&all, &[signer.public_key()]).is_empty());
    }

    #[test]
    fn one_runs_lines_are_shown_with_proofs_and_somebody_with_only_the_public_key_checks_them() {
        let (l, signer, _) = ledger("bundle", 100);
        let keys = vec![signer.public_key()];
        let proof = run_proof(&l, "runA", &keys);
        assert!(proof.problems.is_empty(), "{:?}", proof.problems);
        assert!(proof.chain_ok && proof.unsigned == 0 && proof.covered == proof.lines.len() && proof.lines.len() == 34, "{} lines", proof.lines.len());
        assert_eq!(proof.key_ids, vec![signer.key_id()]);
        // The bundle goes to somebody else as text.
        let text = serde_json::to_string(&proof).unwrap();
        let received: RunProof = serde_json::from_str(&text).unwrap();
        let report = verify_bundle(&received, &keys);
        assert!(report.all_good(), "{report:?}");
        // Their own key is what they check against: a bundle checked against another key is not good.
        let other = TestSigner::new();
        let wrong = verify_bundle(&received, &[other.public_key()]);
        assert!(!wrong.all_good() && wrong.problems.iter().any(|p| p.contains("not one that is known")), "{wrong:?}");
        // Changing a line (what a tool did, or whether it worked) is found.
        let mut edited = received.clone();
        edited.lines[2].line["tool"] = json!("fs.delete");
        assert!(verify_bundle(&edited, &keys).problems.iter().any(|p| p.contains("changed")));
        let mut edited = received.clone();
        edited.lines[5].line["ok"] = json!(false);
        assert!(!verify_bundle(&edited, &keys).all_good());
        // Taking a line out is not found by the lines themselves, but a line that was in the proof and is dropped leaves a shorter bundle: the
        // person who shows it can leave lines out, which is why the count is stated; a line cannot be swapped for another run's.
        let mut foreign = received.clone();
        let other_run = run_proof(&l, "runB", &keys);
        foreign.lines[0] = other_run.lines[0].clone();
        foreign.heads.extend(other_run.heads.clone());
        assert!(verify_bundle(&foreign, &keys).problems.iter().any(|p| p.contains("belongs to another run")));
        // A field added to a line that is otherwise untouched (the hash does not cover it) is refused: a reader of the bundle could take it for part of the record.
        let mut padded = received.clone();
        padded.lines[3].line["verdict"] = json!("approved");
        let r = verify_bundle(&padded, &keys);
        assert!(r.problems.iter().any(|p| p.contains("not part of the signed record")) && !r.all_good(), "{r:?}");
        // A proof moved onto another line does not fit.
        let mut moved = received.clone();
        moved.lines[1].proof = moved.lines[0].proof.clone();
        assert!(!verify_bundle(&moved, &keys).all_good());
        // A head that was changed is found.
        let mut forged = received.clone();
        forged.heads[0].root = "0".repeat(64);
        assert!(verify_bundle(&forged, &keys).problems.iter().any(|p| p.contains("it was changed")));
        assert!(!verify_bundle(&RunProof { lines: vec![], ..received.clone() }, &keys).all_good(), "a bundle with no line says nothing");
    }

    #[test]
    fn a_ledger_rewritten_as_a_whole_no_longer_matches_what_was_signed() {
        let (l, signer, path) = ledger("rewrite", 70);
        let keys = vec![signer.public_key()];
        l.seal().unwrap();
        drop(l);
        // Somebody rewrites the file with a new, consistent chain (every line's tool changed, hashes recomputed).
        let old = kept_lines(&path);
        let mut prev = "0".repeat(64);
        let mut text = String::new();
        for line in &old {
            let mut ev = line.event.clone();
            ev.tool = "fs.nothing".into();
            let hash = digest(&prev, line.seq, line.ts_unix_ms, &ev);
            let new = Line { seq: line.seq, ts_unix_ms: line.ts_unix_ms, prev: prev.clone(), event: ev, hash: hash.clone() };
            text.push_str(&serde_json::to_string(&new).unwrap());
            text.push('\n');
            prev = hash;
        }
        std::fs::write(&path, text).unwrap();
        let reopened = AuditLedger::open(&path);
        assert!(reopened.verify_all().is_ok(), "the chain alone cannot tell: it was rewritten consistently");
        let all = heads::read_heads(&heads::heads_path(&path));
        let lines: BTreeMap<u64, String> = kept_lines(&path).into_iter().map(|x| (x.seq, x.hash)).collect();
        let report = heads::verify_against_lines(&all, &lines);
        assert!(!report.problems.is_empty() && report.covered_lines == 0, "the signed roots do not match the rewritten lines: {report:?}");
        let proof = run_proof(&reopened, "runA", &keys);
        assert!(!proof.problems.is_empty() && proof.covered == 0, "{proof:?}");
    }

    #[test]
    fn lines_cut_off_the_end_of_the_ledger_are_a_problem_of_every_runs_proof_and_are_not_mistaken_for_rotation() {
        let (l, signer, path) = ledger("truncate", 130);
        let keys = vec![signer.public_key()];
        l.seal().unwrap();
        drop(l);
        // The last ten lines (which hold lines of runs) are deleted from the file.
        let text = std::fs::read_to_string(&path).unwrap();
        let kept: Vec<&str> = text.lines().take(120).collect();
        std::fs::write(&path, kept.join("\n") + "\n").unwrap();
        let reopened = AuditLedger::open(&path);
        let proof = run_proof(&reopened, "runA", &keys);
        assert!(proof.problems.iter().any(|p| p.starts_with("the ledger: head") && p.contains("cut off")), "{:?}", proof.problems);
        assert!(!verify_bundle(&proof, &keys).all_good() || !proof.problems.is_empty());
    }
}
