// src/consensus/mod.rs
pub mod asert;

use crate::block::Block;
use std::time::{SystemTime, UNIX_EPOCH};
use sha2::{Sha256, Digest};
use std::sync::atomic::{AtomicU64, Ordering};

// Max future time allowance (1 hour)
pub const MAX_FUTURE_TIME_SECS: u64 = 3600; 

pub const MAX_TARGET: u64 = 0x0000_00FF_FFFF_FFFF;

static LAST_PRINTED_HEIGHT: AtomicU64 = AtomicU64::new(0);

pub struct ConsensusEngine;

impl ConsensusEngine {
    /// ASERTi3-2d (Per-Block) Target Calculation
    pub fn calculate_next_target(
        anchor_timestamp: u64,
        anchor_target: u64,
        latest_block_timestamp: u64,
        chain_length: u64, 
    ) -> u64 {
        if chain_length <= 1 {
            return anchor_target; 
        }

        // CORE-V2: Genesis override for Block 1938 rescue operation.
        if chain_length == 1938 {
            return 0x0000_00FF_FFFF_FFFF;
        }

        let mut height_diff = chain_length.saturating_sub(0); 
        // CORE-V2: Offset attacker's block inflation to stabilize ASERT curve.
        if chain_length > 1938 {
            height_diff = chain_length.saturating_sub(1700);
        }

        let final_target = asert::calculate_asert_target(
            anchor_target,
            anchor_timestamp,
            latest_block_timestamp,
            height_diff
        );

        // Log target only once per block height
        let last_printed = LAST_PRINTED_HEIGHT.load(Ordering::Relaxed);
        if chain_length != last_printed {
            println!("[INFO] Block {}: Target 0x{:016x}", chain_length, final_target);
            LAST_PRINTED_HEIGHT.store(chain_length, Ordering::Relaxed);
        }

        final_target
    }

    /// Enforces MTP-11 (Median Time Past) and strict future time clamping.
    /// Prevents time-warp attacks and ensures monotonic block progression.
    pub fn verify_timestamp(incoming_block: &Block, past_timestamps: &[u64]) -> Result<(), &'static str> {
        let incoming_time = incoming_block.header.timestamp;
        let current_local_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // 1. Strict Future Time Clamp
        if incoming_time > current_local_time + MAX_FUTURE_TIME_SECS {
            return Err("Security: Block timestamp exceeds maximum future threshold.");
        }

        // 2. MTP-11 (Median Time Past) calculation
        let mtp = if past_timestamps.is_empty() {
            0 // Genesis fallback: Allows the very first blocks to pass safely
        } else {
            let len = past_timestamps.len();
            let effective_len = std::cmp::min(len, 11);
            
            // Zero-trust tail extraction ensures only the latest blocks are validated
            let tail_slice = &past_timestamps[len - effective_len..];

            // Zero-cost CPU stack allocation
            let mut temp_stack = [0u64; 11];
            let valid_slice = &mut temp_stack[..effective_len];
            
            valid_slice.copy_from_slice(tail_slice);

            // O(N) selection for median extraction without sorting overhead
            let mid_index = effective_len / 2;
            let (_, &mut median, _) = valid_slice.select_nth_unstable(mid_index);
            
            median
        };

        // Absolute Monotonic Enforcement
        if incoming_time <= mtp {
            return Err("Security: Block timestamp MUST be strictly greater than MTP-11 (Median Time Past).");
        }

        Ok(())
    }


    pub fn required_target(
        anchor_timestamp: u64,
        anchor_target: u64,
        parent_timestamp: u64,
        height: u64,
    ) -> u64 {
        Self::calculate_next_target(anchor_timestamp, anchor_target, parent_timestamp, height)
            .min(MAX_TARGET)
    }

    pub fn verify_block_target_and_pow(
        block: &Block,
        required: u64,
    ) -> Result<(), &'static str> {
        if block.header.target > MAX_TARGET {
            return Err("Consensus Violation: target above network maximum.");
        }
        if block.header.target != required {
            return Err("Consensus Violation: declared target does not match the required target.");
        }
        if !Self::verify_proof_of_work(block, required) {
            return Err("Consensus Violation: insufficient proof-of-work.");
        }
        Ok(())
    }

    /// Verifies the Proof-of-Work hash against the current target.
    pub fn verify_proof_of_work(block: &Block, target: u64) -> bool {
        let hash = block.calculate_hash();
        let mut prefix_bytes = [0u8; 8];
        prefix_bytes.copy_from_slice(&hash[0..8]);
        let hash_prefix = u64::from_be_bytes(prefix_bytes);
        
        hash_prefix <= target
    }

    /// Validates transaction integrity with O(N) time and minimal memory footprint.
    /// Checks commit_merkle_root against the block's witness hashes, binding
    /// the signatures to the block hash the same way merkle_root binds the txs.
    pub fn verify_witness_commitment(block: &Block) -> bool {
        let witness_hashes: Vec<[u8; 32]> = block.transactions.iter()
            .map(|tx| tx.calculate_witness_hash())
            .collect();
        crate::crypto::merkle::build_merkle_root(witness_hashes) == block.header.commit_merkle_root
    }

    pub fn verify_merkle_root(block: &Block) -> bool {
        if block.transactions.is_empty() { return false; }

        let mut current_level: Vec<[u8; 32]> = block.transactions.iter()
            .map(|tx| tx.calculate_id())
            .collect();

        // CVE-2012-2459: duplicated txids can produce the same root as a
        // different transaction list, so they are never valid.
        if crate::crypto::merkle::has_duplicate_txs(&current_level) {
            return false;
        }

        while current_level.len() > 1 {
            if current_level.len() % 2 != 0 {
                current_level.push(current_level.last().unwrap().clone());
            }
            let mut next_level = Vec::with_capacity(current_level.len() / 2);
            for chunk in current_level.chunks(2) {
                let mut hasher = Sha256::new();
                hasher.update(&chunk[0]);
                hasher.update(&chunk[1]);
                next_level.push(hasher.finalize().into());
            }
            current_level = next_level;
        }

        current_level[0] == block.header.merkle_root
    }
}

use std::collections::HashMap;
use lazy_static::lazy_static;

lazy_static! {
    // MAINNET CHECKPOINTS
    // Immutable trust anchors mapped by absolute block height.
    pub static ref CHECKPOINTS: HashMap<u64, [u8; 32]> = {
        let mut m = HashMap::new();
        
        // Block 0: Genesis.
        m.insert(0, parse_hex("00000039ed259365a978de048e4c3d22a75e07573035e00ef4d0f1ea1b934402"));
        
        // Block 100：First hardcoded stability checkpoint.
        m.insert(100, parse_hex("00000000f7848b67987d7b4dcbc5dfc541cc98ef7ec108eb3380154df294c330"));

        // Block 200: Second hardcoded stability checkpoint (Mainnet Launch Anchor).
        m.insert(200, parse_hex("00000000e78797980fc697b63da596ac20d14528c2b001ad794688f3ad3b062d"));
        
        // CORE-V2: Immutable trust anchor for post-rescue chain structure.
        // Local rescue executed successfully. Hash sealed.
        m.insert(1938, parse_hex("0000006873ebe098064205450bb0453a600d98b8f31966dd5f226ac658a2b425"));

        // CORE-V3: Consensus checkpoint for post-inflation state stabilization.
        m.insert(3620, parse_hex("000000004cad8998d3f507e98f264ee8b2f5aa211cddf869eca0b50201e87c77"));

        // =================================================================
        // GENESIS LOCK: THE 10000 BLOCK MILESTONE
        // =================================================================
        m.insert(10000, parse_hex("000000000089f7e3ca2260477c830545abd6e02fc514703e39f2c25fd30486d4"));

        m
    };
}

// Converts hex string to raw bytes once during node initialization.
// Prevents dynamic memory allocation in the consensus hot loop.
fn parse_hex(hex: &str) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    for i in 0..32 {
        bytes[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
    }
    bytes
}

// Validates block hash against hardcoded physical checkpoints via O(1) memory lookup.
pub fn verify_checkpoint(height: u64, block_hash: &[u8; 32]) -> Result<(), &'static str> {
    if let Some(expected_hash) = CHECKPOINTS.get(&height) {
        if block_hash != expected_hash {
            println!("[ERROR] Consensus: Checkpoint mismatch at height {}.", height);
            return Err("Checkpoint violation: Hardcoded hash mismatch.");
        }
    }
    Ok(())
}



#[cfg(test)]
mod validation_tests {
    use super::*;
    use crate::transaction::{Transaction, TxIn, TxOut, TxWitness};

    fn tx(seed: u8) -> Transaction {
        Transaction {
            inputs: vec![TxIn { previous_output_hash: [seed; 32], vout: 0 }],
            outputs: vec![TxOut { value: 1, public_key_hash: [seed; 32], recovery: None }],
            witnesses: vec![TxWitness { signature: vec![seed; 8], public_key: vec![seed; 8] }],
        }
    }

    #[test]
    fn duplicate_txids_never_pass_merkle_check() {
        let (a, b, c) = (tx(1), tx(2), tx(3));
        let good = Block::new([0u8; 32], vec![a.clone(), b.clone(), c.clone()], 1, 0);
        assert!(ConsensusEngine::verify_merkle_root(&good));
        // [a, b, c, c] hashes to the same root as [a, b, c] (odd leaf duplication).
        let mut dup = good.clone();
        dup.transactions = vec![a, b, c.clone(), c];
        assert_eq!(crate::crypto::merkle::build_merkle_root(dup.transactions.iter().map(|t| t.calculate_id()).collect()), good.header.merkle_root);
        assert!(!ConsensusEngine::verify_merkle_root(&dup));
    }

    #[test]
    fn witness_commitment_binds_signatures() {
        let block = Block::new([0u8; 32], vec![tx(1), tx(2)], 1, 0);
        assert!(ConsensusEngine::verify_witness_commitment(&block));
        let mut tampered = block.clone();
        tampered.transactions[1].witnesses[0].signature = vec![9u8; 8];
        assert!(ConsensusEngine::verify_merkle_root(&tampered), "txids unchanged");
        assert!(!ConsensusEngine::verify_witness_commitment(&tampered));
    }
}
