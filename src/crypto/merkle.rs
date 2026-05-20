// src/crypto/merkle.rs
use sha2::{Sha256, Digest};

/// Computes the Merkle Root using the Satoshi Nakamoto standard.
/// Logic: Pairwise hashing -> Tail duplication for odd elements -> Recursive reduction.
pub fn build_merkle_root(mut hashes: Vec<[u8; 32]>) -> [u8; 32] {
    if hashes.is_empty() {
        return [0u8; 32];
    }

    while hashes.len() > 1 {
        // Satoshi standard: If number of elements is odd, duplicate the last element.
        if hashes.len() % 2 != 0 {
            let last = *hashes.last().unwrap();
            hashes.push(last);
        }

        let mut next_layer = Vec::with_capacity(hashes.len() / 2);
        for i in (0..hashes.len()).step_by(2) {
            let mut hasher = Sha256::new();
            hasher.update(hashes[i]);
            hasher.update(hashes[i+1]);
            let result = hasher.finalize();
            
            let mut next_hash = [0u8; 32];
            next_hash.copy_from_slice(&result);
            next_layer.push(next_hash);
        }
        hashes = next_layer;
    }

    hashes[0]
}

/// Mitigation for CVE-2012-2459.
/// Validates that no duplicate transaction IDs exist within a single block.
/// Optimized to O(N) using HashSet to prevent computational DoS attacks.
pub fn has_duplicate_txs(hashes: &[[u8; 32]]) -> bool {
    use std::collections::HashSet;
    let mut seen = HashSet::with_capacity(hashes.len());
    for hash in hashes {
        if !seen.insert(*hash) {
            return true;
        }
    }
    false
}