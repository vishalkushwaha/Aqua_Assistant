//! A Merkle tree over a batch of ledger lines, as in certificate transparency (RFC 6962): a leaf is hashed with a `0x00` prefix and an inner node with
//! `0x01`, so a leaf can never be taken for a node. A tree head (its root) says what the batch holds; an inclusion proof shows that one line is in it by
//! giving the hashes along the way up, a handful of them whatever the size of the batch.
//!
//! Hashes are lower-case hex of SHA-256 (what the ledger already writes).

use sha2::{Digest, Sha256};

pub type Hash = [u8; 32];

pub fn hex(h: &Hash) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(s: &str) -> Option<Hash> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// The hash of a leaf: the line's own hash (hex), as a leaf.
pub fn leaf(line_hash_hex: &str) -> Hash {
    let mut h = Sha256::new();
    h.update([0u8]);
    h.update(line_hash_hex.as_bytes());
    h.finalize().into()
}

fn node(left: &Hash, right: &Hash) -> Hash {
    let mut h = Sha256::new();
    h.update([1u8]);
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// The largest power of two below `n` (for `n >= 2`).
fn split(n: usize) -> usize {
    let mut k = 1;
    while k * 2 < n {
        k *= 2;
    }
    k
}

/// The root over `leaves` (already hashed with [`leaf`]). The root of nothing is the hash of the empty string.
pub fn root(leaves: &[Hash]) -> Hash {
    match leaves.len() {
        0 => Sha256::digest([]).into(),
        1 => leaves[0],
        n => {
            let k = split(n);
            node(&root(&leaves[..k]), &root(&leaves[k..]))
        }
    }
}

/// The hashes that, with the leaf at `index`, give the root: from the bottom up, each with whether it sits to the left.
pub fn inclusion_proof(leaves: &[Hash], index: usize) -> Option<Vec<(Hash, bool)>> {
    if index >= leaves.len() {
        return None;
    }
    fn walk(leaves: &[Hash], index: usize, out: &mut Vec<(Hash, bool)>) {
        if leaves.len() <= 1 {
            return;
        }
        let k = split(leaves.len());
        if index < k {
            walk(&leaves[..k], index, out);
            out.push((root(&leaves[k..]), false));
        } else {
            walk(&leaves[k..], index - k, out);
            out.push((root(&leaves[..k]), true));
        }
    }
    let mut out = Vec::new();
    walk(leaves, index, &mut out);
    Some(out)
}

/// Does `leaf_hash`, with `proof`, give `expected_root`?
pub fn verify_inclusion(leaf_hash: &Hash, proof: &[(Hash, bool)], expected_root: &Hash) -> bool {
    let mut acc = *leaf_hash;
    for (sibling, sibling_is_left) in proof {
        acc = if *sibling_is_left { node(sibling, &acc) } else { node(&acc, sibling) };
    }
    &acc == expected_root
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(n: usize) -> Vec<Hash> {
        (0..n).map(|i| leaf(&format!("{:064x}", i))).collect()
    }

    #[test]
    fn a_leaf_and_a_node_are_never_mistaken_for_each_other_and_the_root_depends_on_every_leaf_and_their_order() {
        let l = leaves(5);
        assert_ne!(hex(&leaf("aa")), format!("{:x}", Sha256::digest("aa".as_bytes())), "the prefix separates a leaf from a plain hash");
        let r = root(&l);
        assert_eq!(r, root(&l), "the same leaves give the same root");
        let mut changed = l.clone();
        changed[3] = leaf("tampered");
        assert_ne!(root(&changed), r, "one leaf changed");
        let mut swapped = l.clone();
        swapped.swap(1, 2);
        assert_ne!(root(&swapped), r, "two leaves swapped");
        assert_ne!(root(&l[..4]), r, "a leaf cut off");
        let mut longer = l.clone();
        longer.push(leaf("extra"));
        assert_ne!(root(&longer), r, "a leaf added");
        assert_eq!(root(&[]), <Hash>::from(Sha256::digest([])));
        assert_eq!(root(&l[..1]), l[0]);
    }

    #[test]
    fn every_leaf_of_every_size_of_tree_has_a_proof_that_checks_and_only_for_its_own_leaf_and_root() {
        for n in 1..=40 {
            let l = leaves(n);
            let r = root(&l);
            for i in 0..n {
                let proof = inclusion_proof(&l, i).expect("a proof");
                assert!(verify_inclusion(&l[i], &proof, &r), "n={n} i={i}");
                assert!(proof.len() <= 6, "a proof is a handful of hashes: {}", proof.len());
                if n > 1 {
                    assert!(!verify_inclusion(&l[(i + 1) % n], &proof, &r), "another leaf does not fit this proof (n={n} i={i})");
                }
                assert!(!verify_inclusion(&l[i], &proof, &root(&leaves(n + 1))), "nor does another root");
            }
        }
        assert!(inclusion_proof(&leaves(3), 3).is_none());
    }

    #[test]
    fn hex_round_trips_and_refuses_what_is_not_a_hash() {
        let h = leaf("x");
        assert_eq!(from_hex(&hex(&h)), Some(h));
        assert_eq!((from_hex("zz"), from_hex(&"g".repeat(64)), from_hex(&"é".repeat(32))), (None, None, None));
    }
}
