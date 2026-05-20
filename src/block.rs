// src/block.rs
use serde::{Serialize, Deserialize};
use crate::transaction::Transaction;
use sha2::{Sha256, Digest};
use std::time::{SystemTime, UNIX_EPOCH};

// Core data structures for block and consensus representation.

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct BlockHeader {
    pub timestamp: u64,
    pub previous_hash: [u8; 32],
    // Root hash of all physical transactions.
    pub merkle_root: [u8; 32],  
    // Root hash of all Mempool commitments. Zero footprint on UTXO.
    pub commit_merkle_root: [u8; 32], 
    pub nonce: u64,     
    pub target: u64,    
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Block {
    pub header: BlockHeader,
    pub transactions: Vec<Transaction>,
}

// Q-BIP-152: Prefilled transaction to guarantee Coinbase and core structural TXs 
// are always included without requiring secondary round-trip requests.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PrefilledTransaction {
    pub index: usize,
    pub tx: Transaction,
}

// Q-BIP-152: Quantum-Adaptive Compact Block structure.
// Reduces broadcast footprint from megabytes to kilobytes.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CompactBlock {
    pub header: BlockHeader,
    // Collision-resistant salt for this specific compact block.
    pub nonce: u64,
    // 64-bit truncated hashes of the transactions.
    pub short_ids: Vec<u64>,
    pub prefilled_txs: Vec<PrefilledTransaction>,
}

impl CompactBlock {
    // Generates a 64-bit short ID using SHA-256 and the block-specific nonce.
    // Emulates BIP-152 SipHash-2-4 behavior without external dependencies.
    pub fn calculate_short_id(tx_hash: &[u8; 32], nonce: u64) -> u64 {
        let mut hasher = Sha256::new();
        hasher.update(tx_hash);
        hasher.update(nonce.to_be_bytes());
        let result = hasher.finalize();
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&result[0..8]);
        u64::from_le_bytes(bytes)
    }
}

/// Represents a node in the in-memory block tree (mapBlockIndex).
/// Contains only consensus metadata, allowing the node to evaluate 
/// chain reorganizations efficiently without loading full block payloads.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BlockIndex {
    /// The unique SHA-256 identifier of this block.
    pub block_hash: [u8; 32],
    /// A copy of the lightweight header.
    pub header: BlockHeader,
    /// Distance from the Genesis block.
    pub height: u64,
    /// Cumulative Proof-of-Work from Genesis up to this block.
    /// Uses u128 to prevent overflow during long-term network operation.
    pub chain_work: u128,
    /// Flag indicating if the full block data (transactions and signatures) is stored on disk.
    pub has_data: bool, 
}

impl BlockHeader {
    // Calculate expected hash operations for this block's target.
    pub fn get_block_proof(&self) -> u128 {
        let target = self.target as u128;
        if target == 0 {
            return 0;
        }
        (u64::MAX as u128) / target
    }
}

impl BlockIndex {
    // Constructor for secure tree node initialization.
    pub fn new(block_hash: [u8; 32], header: BlockHeader, height: u64, parent_work: u128, has_data: bool) -> Self {
        let block_work = header.get_block_proof();
        Self {
            block_hash,
            header,
            height,
            chain_work: parent_work + block_work,
            has_data,
        }
    }
}

impl Block {
    // Calculates total block weight in Weight Units (WU). Base header size is 120 bytes.
    pub fn get_block_weight(&self) -> u64 {
        let header_weight = 120 * 4;
        let txs_weight: u64 = self.transactions.iter().map(|tx| tx.get_weight()).sum();
        header_weight + txs_weight
    }

    // Calculates total SigOps to prevent CPU exhaustion.
    pub fn get_block_sigops(&self) -> usize {
        self.transactions.iter().map(|tx| tx.get_sigops_count()).sum()
    }

    // Generates the hardcoded Genesis block.
    pub fn genesis() -> Self {
        println!("[INFO] Genesis: Initializing genesis block.");
        println!("[INFO] Genesis: Motto: \"17/May/2026: The quantum age dawns. The 21,000,000 truth shines eternal.\"");
        println!("[INFO] Genesis: Identity: qbtc1tpv0e2s92eurft7z5v3l592l2m8a8cy0hdg967mcr3pk3pwh026qpkn68x");

        // Embed the updated genesis motto into the coinbase transaction.
        let motto = b"17/May/2026: The quantum age dawns. The 21,000,000 truth shines eternal.";
        // Genesis creator signature. Outputs are sent to an unspendable address.
        let founder_address = b"qbtc1tpv0e2s92eurft7z5v3l592l2m8a8cy0hdg967mcr3pk3pwh026qpkn68x";

        let genesis_tx = crate::transaction::Transaction {
            inputs: vec![crate::transaction::TxIn {
                previous_output_hash: [0u8; 32],
                vout: 0xFFFFFFFF, // Standard Coinbase vout
            }],
            outputs: vec![crate::transaction::TxOut {
                value: 50_0000_0000, // 50 QBTC Genesis Reward
                public_key_hash: [0u8; 32], // Unspendable address
                recovery: None,
            }],
            witnesses: vec![crate::transaction::TxWitness {
                signature: motto.to_vec(), 
                public_key: founder_address.to_vec(), 
            }],
        };

        // Compute dual-magazine roots using pure binary Merkle logic.
        let tx_id = genesis_tx.calculate_id();
        let witness_hash = genesis_tx.calculate_witness_hash();

        let merkle_root = crate::crypto::merkle::build_merkle_root(vec![tx_id]);
        let commit_merkle_root = crate::crypto::merkle::build_merkle_root(vec![witness_hash]);

        Block {
            header: BlockHeader {
                timestamp: 1778976000, // May 17 2026 00:00:00 GMT
                previous_hash: [0u8; 32],
                merkle_root, 
                commit_merkle_root,
                // NOTE: This nonce must be recalculated via genesis_miner.rs
                nonce: 6759977,
                target: 1099511627775,
            },
            transactions: vec![genesis_tx],
        }
    }

    // Initializes a new block template.
    pub fn new(previous_hash: [u8; 32], transactions: Vec<Transaction>, target: u64, nonce: u64) -> Self {
        // Extract transaction IDs (Magazine 1) and Witness hashes (Magazine 2).
        let tx_ids: Vec<[u8; 32]> = transactions.iter().map(|tx| tx.calculate_id()).collect();
        let witness_hashes: Vec<[u8; 32]> = transactions.iter().map(|tx| tx.calculate_witness_hash()).collect();

        // Enforce CVE-2012-2459 mitigation at the consensus constructor level.
        if crate::crypto::merkle::has_duplicate_txs(&tx_ids) {
            panic!("Consensus failure: Duplicate transactions detected in block construction.");
        }

        let merkle_root = crate::crypto::merkle::build_merkle_root(tx_ids);
        let commit_merkle_root = crate::crypto::merkle::build_merkle_root(witness_hashes);

        Block {
            header: BlockHeader {
                timestamp: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
                previous_hash,
                merkle_root, 
                commit_merkle_root, 
                nonce,
                target,
            },
            transactions,
        }
    }

    // Computes the SHA-256 hash of the block header.
    pub fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        
        // Zero-allocation stack buffer for 120-byte header.
        // Format: timestamp(8) | prev_hash(32) | merkle_root(32) | commit_root(32) | nonce(8) | target(8)
        let mut header_bytes = [0u8; 120]; 
        header_bytes[0..8].copy_from_slice(&self.header.timestamp.to_be_bytes());
        header_bytes[8..40].copy_from_slice(&self.header.previous_hash);
        header_bytes[40..72].copy_from_slice(&self.header.merkle_root); 
        header_bytes[72..104].copy_from_slice(&self.header.commit_merkle_root); 
        header_bytes[104..112].copy_from_slice(&self.header.nonce.to_be_bytes());
        header_bytes[112..120].copy_from_slice(&self.header.target.to_be_bytes());

        hasher.update(&header_bytes);
        hasher.finalize().into()
    }

    // Calculates estimated physical memory footprint in bytes.
    pub fn get_physical_size(&self) -> usize {
        // Size = Header (approx 120 bytes) + sum of physical sizes of all transactions
        let tx_size: usize = self.transactions.iter().map(|tx| tx.get_physical_size()).sum();
        120 + tx_size
    }
}


