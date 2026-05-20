// src/transaction.rs
// Core transaction protocol implementation.
// Integrates PQ-SegWit and delayed recovery logic.
// Cryptographic standard: NIST FIPS 204 (ML-DSA) and SHA-256.

use serde::{Serialize, Deserialize};
use sha2::{Digest, Sha256};
use crate::crypto;

// Reserved instruction set for future VM upgrades.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Opcode {
    OpCheckSig = 0xAC,
    // Reserved for future cryptographic proof verification.
    // Maps to NOP (No Operation) in V1 consensus rules.
    OpReserved1 = 0x50,
    OpReserved2 = 0x51,
    OpReserved3 = 0x52,
    OpReserved4 = 0x53,
    OpReserved5 = 0x54,
    OpReserved6 = 0x55,
    OpReserved7 = 0x56,
    OpReserved8 = 0x57,
    OpReserved9 = 0x58,
    OpReserved10 = 0x59,
}

/// Uniquely identifies a specific output from a previous transaction.
/// Serves as the fundamental coordinate system for the UTXO state.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutPoint {
    pub tx_hash: [u8; 32],
    pub vout: u32,
}

/// Witness structure for Post-Quantum SegWit.
/// Isolates the large ML-DSA signatures (~3309 bytes) from the transaction core
/// to mitigate block bloat and ensure TXID stability (malleability protection).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxWitness {
    /// ML-DSA-65 Signature: The cryptographic proof of ownership.
    /// Approximately 3309 bytes.
    pub signature: Vec<u8>,
    /// ML-DSA-65 Public Key: Used to verify the signature.
    /// Approximately 1952 bytes.
    pub public_key: Vec<u8>,
}

/// Core transaction input.
/// Stripped of witness data to ensure the TXID remains deterministic 
/// and immutable regardless of signature mutations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct TxIn {
    /// The hash of the previous transaction containing the UTXO being spent.
    pub previous_output_hash: [u8; 32],
    /// The index of the specific output in the previous transaction.
    pub vout: u32,
}

/// Recovery attributes for delayed backup access.
/// Encapsulates the logic for inheritance or secondary key recovery.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecoveryInfo {
    /// Salted Hash: SHA256(Backup_PubKey_Hash + Secret_Salt).
    /// Keeps the recovery identity hidden until activation.
    pub recovery_hash: [u8; 32],
    /// Activation Delay: The number of blocks that must pass 
    /// without activity before the backup key can claim the funds.
    pub recovery_delay: u64,
}

/// Core transaction output.
/// Defines the recipient, value, and optional recovery constraints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TxOut {
    /// Value in Satoshis (1 QBTC = 10^8 Satoshis).
    pub value: u64,
    /// The primary recipient's public key hash.
    pub public_key_hash: [u8; 32],
    /// Optional backup recovery metadata.
    pub recovery: Option<RecoveryInfo>,
}

/// The primary data structure for value transfer across the network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    /// Vector of inputs (Core pointers).
    pub inputs: Vec<TxIn>,
    /// Vector of outputs (Recipients and constraints).
    pub outputs: Vec<TxOut>,
    /// Vector of witnesses (Signatures), isolated from the TXID calculation.
    /// One witness per input.
    pub witnesses: Vec<TxWitness>,
}

impl Transaction {
    // -------------------------------------------------------------------------
    // Identifier Calculations
    // -------------------------------------------------------------------------

    /// Computes the unique Transaction ID (TXID) for the ledger.
    /// Excludes witness data to ensure malleability resistance.
    pub fn calculate_id(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        
        // Serialize immutable core fields: inputs and outputs.
        let core_data = bincode::serialize(&(&self.inputs, &self.outputs))
            .expect("Serialization error: Core TX descriptor failure");
            
        hasher.update(&core_data);
        hasher.finalize().into()
    }

    /// Computes the Witness ID (WTXID) for the intent magazine.
    /// Includes signature data to ensure total transaction integrity.
    pub fn calculate_witness_hash(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        
        // Serialize complete data structure including signatures.
        let all_data = bincode::serialize(self)
            .expect("Serialization error: Extended TX descriptor failure");
            
        hasher.update(&all_data);
        hasher.finalize().into()
    }

    // -------------------------------------------------------------------------
    // Memory & Economic Profiling
    // -------------------------------------------------------------------------

    /// Calculates the absolute physical memory footprint in bytes.
    /// Utilizes deterministic in-memory serialization to prevent manual calculation drift.
    pub fn get_physical_size(&self) -> usize {
        bincode::serialized_size(self).unwrap_or(0) as usize
    }

    // Calculate strict Weight Units (WU) for block size limits.
    pub fn get_weight(&self) -> u64 {
        let core_size = (self.inputs.len() * 41 + self.outputs.len() * 40) as u64;
        let total_physical_size = self.get_physical_size() as u64;
        core_size * 3 + total_physical_size
    }

    // Calculate computational complexity (SigOps).
    pub fn get_sigops_count(&self) -> usize {
        self.inputs.len()
    }

    // -------------------------------------------------------------------------
    // Signature Verification
    // -------------------------------------------------------------------------

    /// Validates the ML-DSA signature for a specific input.
    /// @param input_idx: The index of the input to verify.
    /// @param message_hash: The message (TX Core) signed by the sender.
    pub fn verify_witness(&self, input_idx: usize, message_hash: &[u8]) -> bool {
        match self.witnesses.get(input_idx) {
            Some(witness) => {
                crypto::ml_dsa::verify_signature(
                    message_hash,
                    &witness.signature,
                    &witness.public_key
                )
            },
            None => {
                println!("[WARN] Security: Missing witness for input index {}", input_idx);
                false
            }
        }
    }

    /// Ensures that every input has a corresponding witness.
    pub fn is_well_formed(&self) -> bool {
        !self.inputs.is_empty() && self.inputs.len() == self.witnesses.len()
    }
}

