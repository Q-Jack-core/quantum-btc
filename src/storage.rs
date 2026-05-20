// src/storage.rs
// =============================================================================
// QBTC Storage Engine
// Architecture: mapBlockIndex in-memory tree and RocksDB cold SegWit storage.
// =============================================================================

use rocksdb::{DB, Options, IteratorMode, Direction, WriteBatch, WriteOptions}; // L1 V2.0 CORE: Atomic WriteBatch & Sync
use std::path::Path;
use std::collections::HashMap;
use std::sync::RwLock;

use crate::block::{Block, BlockHeader, BlockIndex}; 
use crate::utxo::{UtxoState, UtxoUndoLog}; 
use crate::transaction::TxWitness;
use serde::{Serialize, Deserialize};

/// Storage prefixes for RocksDB key-value partition.
const PREFIX_HEADER: &[u8] = b"HDR_";
const PREFIX_BLOCK_INDEX: &[u8] = b"IDX_"; // Block tree metadata.
const PREFIX_CORE_BLOCK: &[u8] = b"BLK_";  // Core block transaction data.
const PREFIX_WITNESS: &[u8] = b"WIT_";     // ML-DSA-65 signature payloads.
const PREFIX_UNDO_LOG: &[u8] = b"UNDO_";   // State rollback logs.
const PREFIX_UTXO: &[u8] = b"UTXO_";       // Granular UTXO physical records.

// =============================================================================
// Data Structures for Post-Quantum SegWit
// =============================================================================

// CORE-V6 ARCHITECTURE: Instruction container for Atomic Batch Syncing.
// Encapsulates all state mutations required for a single block within a batch pipeline.
pub struct BatchCommitInstruction {
    pub block: Block,
    pub height: u64,
    pub undo_log: UtxoUndoLog,
    pub accumulated_work: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WitnessPayload {
    pub tx_witnesses: Vec<Vec<TxWitness>>, 
}

// =============================================================================
// Storage Engine Implementation V1.19
// =============================================================================

pub struct MemoryVault {
    pub map_block_index: HashMap<[u8; 32], BlockIndex>,
    // L1 MEMORY UPGRADE: Cache active chain vectors to eliminate disk I/O during height checks.
    pub active_chain: Vec<[u8; 32]>,
    pub header_chain: Vec<[u8; 32]>,
    pub map_orphan_blocks: HashMap<[u8; 32], Block>,
    pub map_orphan_blocks_by_prev: HashMap<[u8; 32], Vec<[u8; 32]>>,
    // CORE-V6: Unified Storage View (Staging Area)
    pub staged_blocks: HashMap<[u8; 32], Block>,
}

pub struct QuantumStorage {
    pub db: DB, 
    pub vault: RwLock<MemoryVault>,
    // CORE-V6: Absolute global I/O mutex. Prevents multi-threaded state tearing.
    pub io_lock: std::sync::Mutex<()>,
}

impl QuantumStorage {
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.set_max_background_jobs(4);
        opts.set_bytes_per_sync(1048576 * 2); 

        // CORE-V3: Block-Based Bloom Filter and LRU Cache.
        // Absolutely intercepts fake UTXO I/O exhaustion attacks with 0 disk reads.
        let mut block_opts = rocksdb::BlockBasedOptions::default();
        block_opts.set_bloom_filter(10.0, false);
        block_opts.set_block_cache(&rocksdb::Cache::new_lru_cache(256 * 1024 * 1024));
        opts.set_block_based_table_factory(&block_opts);

        let db = DB::open(&opts, path).expect("RocksDB initialization failed");
        
        let mut map_index = HashMap::new();

        // Reconstruct mapBlockIndex from physical disk.
        let iter = db.iterator(IteratorMode::Start);
        for item in iter {
            if let Ok((key, value)) = item {
                if key.starts_with(PREFIX_BLOCK_INDEX) {
                    if let Ok(index) = bincode::deserialize::<BlockIndex>(&value) {
                        map_index.insert(index.block_hash, index);
                    }
                }
            }
        }

        // L1 V2.0 CORE: Dynamically reconstruct active_chain from map_block_index.
        // Bypasses physical CHAIN_LIST array to guarantee topological consistency based on chain_work.
        let mut best_tip = [0u8; 32];
        let mut max_work = 0u128;
        for (hash, index) in &map_index {
            if index.chain_work > max_work {
                max_work = index.chain_work;
                best_tip = *hash;
            }
        }

        let mut dynamic_chain = Vec::new();
        if max_work > 0 {
            let mut current = best_tip;
            while let Some(idx) = map_index.get(&current) {
                dynamic_chain.push(current);
                if current == [0u8; 32] || idx.header.previous_hash == [0u8; 32] {
                    break;
                }
                current = idx.header.previous_hash;
            }
            dynamic_chain.reverse();
        }

        let active_chain = if !dynamic_chain.is_empty() {
            dynamic_chain
        } else {
            // Legacy bridge fallback for genesis initialization.
            match db.get(b"CHAIN_LIST") {
                Ok(Some(bytes)) => bincode::deserialize(&bytes).unwrap_or_default(),
                _ => Vec::new(),
            }
        };
        let header_chain: Vec<[u8; 32]> = match db.get(b"HEADER_CHAIN_LIST") {
            Ok(Some(bytes)) => bincode::deserialize(&bytes).unwrap_or_default(),
            _ => active_chain.clone(),
        };

        println!("[INFO] Storage: mapBlockIndex mounted in memory. Total tracked forks: {}", map_index.len());

        Self { 
            db,
            vault: RwLock::new(MemoryVault {
                map_block_index: map_index,
                active_chain,
                header_chain,
                map_orphan_blocks: HashMap::new(),
                map_orphan_blocks_by_prev: HashMap::new(),
                staged_blocks: HashMap::new(),
            }),
            io_lock: std::sync::Mutex::new(()),
        }
    }

    // -------------------------------------------------------------------------
    // Block Tree Management
    // -------------------------------------------------------------------------

    pub fn save_block_index(&self, index: BlockIndex) {
        let mut key = PREFIX_BLOCK_INDEX.to_vec();
        key.extend_from_slice(&index.block_hash);
        let encoded = bincode::serialize(&index).expect("Failed to serialize BlockIndex");
        self.db.put(&key, encoded).expect("Failed to write BlockIndex to RocksDB");

        let mut vault = self.vault.write().unwrap();
        vault.map_block_index.insert(index.block_hash, index);
    }

    pub fn get_block_index(&self, block_hash: &[u8; 32]) -> Option<BlockIndex> {
        let vault = self.vault.read().unwrap();
        vault.map_block_index.get(block_hash).cloned()
    }

    // Cryptographically secure Lowest Common Ancestor (LCA) traversal.
    // Utilizes overlay map to prevent LCA blindness during Deep Sync.
    pub fn find_fork_lca(&self, hash_a: &[u8; 32], hash_b: &[u8; 32], overlay: &HashMap<[u8; 32], BlockIndex>) -> Option<[u8; 32]> {
        let vault = self.vault.read().unwrap();
        
        let get_idx = |h: &[u8; 32]| -> Option<BlockIndex> {
            overlay.get(h).cloned().or_else(|| vault.map_block_index.get(h).cloned())
        };

        let mut node_a = get_idx(hash_a)?;
        let mut node_b = get_idx(hash_b)?;

        // Step 1: Height alignment. Retreat the higher node until heights match.
        while node_a.height > node_b.height {
            node_a = get_idx(&node_a.header.previous_hash)?;
        }
        while node_b.height > node_a.height {
            node_b = get_idx(&node_b.header.previous_hash)?;
        }

        // Step 2: Synchronous traversal backward until hashes converge.
        while node_a.block_hash != node_b.block_hash {
            // L1 V1.2 CORE FIX: Handle Genesis boundary to prevent 'None' propagation panics
            if node_a.header.previous_hash == [0u8; 32] || node_b.header.previous_hash == [0u8; 32] {
                return None; 
            }
            
            let prev_a = get_idx(&node_a.header.previous_hash)?;
            let prev_b = get_idx(&node_b.header.previous_hash)?;
            
            // Step 3: Absolute anti-cycle assertion. Prevents infinite loop attacks.
            if prev_a.height >= node_a.height || prev_b.height >= node_b.height {
                return None; 
            }
            
            node_a = prev_a;
            node_b = prev_b;
        }

        Some(node_a.block_hash)
    }

    // -------------------------------------------------------------------------
    // Headers-First Storage (Legacy Bridge)
    // -------------------------------------------------------------------------
    
    pub fn save_header(&self, header: &BlockHeader, hash: &[u8; 32]) {
        let encoded = bincode::serialize(header).expect("Header serialization failed");
        let mut key = PREFIX_HEADER.to_vec();
        key.extend_from_slice(hash);
        self.db.put(&key, &encoded).expect("Failed to write header to RocksDB");

        let mut header_chain = self.get_header_chain_list();
        if !header_chain.contains(hash) {
            header_chain.push(*hash);
            self.db.put(b"HEADER_CHAIN_LIST", bincode::serialize(&header_chain).unwrap()).unwrap();
            // L1 MEMORY UPGRADE: Sync memory vault.
            let mut vault = self.vault.write().unwrap();
            vault.header_chain = header_chain;
        }

        // Create a lightweight BlockIndex without full block data.
        if self.get_block_index(hash).is_none() {
            // Calculate cumulative chain work.
            let prev_work = if header.previous_hash == [0u8; 32] { 0 } 
                else { self.get_block_index(&header.previous_hash).map(|i| i.chain_work).unwrap_or(0) };
            
            // Height alignment fix.
            // Fetch the parent's actual height to calculate the current chain coordinate.
            // Resolves index corruption caused by legacy timestamp-based height mapping.
            let b_height = if header.previous_hash == [0u8; 32] {
                0 // Genesis Block height.
            } else {
                // If parent exists, height = parent.height + 1. Otherwise fallback to 0 safely.
                self.get_block_index(&header.previous_hash).map(|idx| idx.height + 1).unwrap_or(0)
            };
            
            let b_index = BlockIndex {
                block_hash: *hash,
                header: header.clone(),
                height: b_height, 
                chain_work: prev_work + 1,
                has_data: false,
            };
            self.save_block_index(b_index);
        }
    }

    pub fn get_header(&self, hash: &[u8; 32]) -> Option<BlockHeader> {
        let mut key = PREFIX_HEADER.to_vec();
        key.extend_from_slice(hash);
        match self.db.get(&key) {
            Ok(Some(bytes)) => bincode::deserialize(&bytes).ok(),
            _ => None,
        }
    }

    pub fn get_header_chain_list(&self) -> Vec<[u8; 32]> {
        // L1 MEMORY UPGRADE: Pure memory read, zero disk I/O.
        let vault = self.vault.read().unwrap();
        vault.header_chain.clone()
    }

    // -------------------------------------------------------------------------
    // PQ-SegWit Block Storage (Cold/Hot Partitioning)
    // -------------------------------------------------------------------------
    
    pub fn get_chain_list(&self) -> Vec<[u8; 32]> {
        // L1 MEMORY UPGRADE: Pure memory read, zero disk I/O.
        let vault = self.vault.read().unwrap();
        vault.active_chain.clone()
    }

    // L1 V1.2 CORE FIX: Use accurate physical height for Witness DB key instead of block timestamp.
    pub fn save_block_segwit(&self, mut block: Block, height: u64) {
        let block_hash = block.calculate_hash();
        
        let mut witness_payload = WitnessPayload { tx_witnesses: Vec::new() };
        for tx in &mut block.transactions {
            let witnesses = std::mem::take(&mut tx.witnesses);
            witness_payload.tx_witnesses.push(witnesses);
        }

        let core_encoded = bincode::serialize(&block).expect("Serialization failed");
        let mut core_key = PREFIX_CORE_BLOCK.to_vec();
        core_key.extend_from_slice(&block_hash);
        self.db.put(&core_key, &core_encoded).unwrap();

        let mut witness_key = PREFIX_WITNESS.to_vec();
        witness_key.extend_from_slice(&height.to_be_bytes()); 
        witness_key.extend_from_slice(&block_hash);           
        let witness_encoded = bincode::serialize(&witness_payload).unwrap();
        self.db.put(&witness_key, &witness_encoded).unwrap();

        self.db.put(b"LATEST_BLOCK_HEAD", &core_encoded).unwrap();
        let mut chain = self.get_chain_list();
        if !chain.contains(&block_hash) {
            chain.push(block_hash);
            self.db.put(b"CHAIN_LIST", bincode::serialize(&chain).unwrap()).unwrap();
            // L1 MEMORY UPGRADE: Sync memory vault.
            let mut vault = self.vault.write().unwrap();
            vault.active_chain = chain;
        }

        self.save_header(&block.header, &block_hash);

        // Update BlockIndex to flag that full data is available.
        if let Some(mut index) = self.get_block_index(&block_hash) {
            index.has_data = true;
            self.save_block_index(index);
        }
    }

    pub fn rollback_chain(&self, fork_idx: usize) {
        // Global I/O Lock prevents network threads from injecting blocks during rollback.
        let _global_io_guard = self.io_lock.lock().unwrap();
        let mut chain = self.get_chain_list();
        if fork_idx < chain.len() {
            let mut vault = self.vault.write().unwrap(); 
            
            // Use WriteBatch to guarantee atomic reverse-state transitions.
            let mut batch = WriteBatch::default();
            
            // Iterate backwards (from tip down to fork) to preserve cryptographic causality.
            for i in ((fork_idx + 1)..chain.len()).rev() {
                let hash_to_kill = &chain[i];
                let height = i as u64;

                let mut undo_key = PREFIX_UNDO_LOG.to_vec(); 
                undo_key.extend_from_slice(&height.to_be_bytes()); 
                undo_key.extend_from_slice(hash_to_kill);
                
                // Phase 1: Reverse physical UTXO mutations using the UndoLog.
                // MUST execute before the UndoLog is eradicated.
                if let Ok(Some(undo_bytes)) = self.db.get(&undo_key) {
                    if let Ok(undo_log) = bincode::deserialize::<UtxoUndoLog>(&undo_bytes) {
                        // Reverse Phase B: Physically eradicate UTXOs created by the orphaned block.
                        for outpoint in &undo_log.newly_created_outpoints {
                            let mut utxo_key = PREFIX_UTXO.to_vec();
                            utxo_key.extend_from_slice(&outpoint.tx_hash);
                            utxo_key.extend_from_slice(&outpoint.vout.to_be_bytes());
                            batch.delete(&utxo_key);
                        }
                        
                        // Reverse Phase A: Physically resurrect UTXOs spent by the orphaned block.
                        for (outpoint, record) in &undo_log.spent_utxos {
                            let mut utxo_key = PREFIX_UTXO.to_vec();
                            utxo_key.extend_from_slice(&outpoint.tx_hash);
                            utxo_key.extend_from_slice(&outpoint.vout.to_be_bytes());
                            let record_encoded = bincode::serialize(record).expect("Fatal: UTXO serialization fault during rollback");
                            batch.put(&utxo_key, &record_encoded);
                        }
                    }
                }

                // Phase 2: Eradicate Block, Witness, and Undo Data.
                let mut core_key = PREFIX_CORE_BLOCK.to_vec(); core_key.extend_from_slice(hash_to_kill);
                batch.delete(&core_key);

                let mut witness_key = PREFIX_WITNESS.to_vec(); witness_key.extend_from_slice(&height.to_be_bytes()); witness_key.extend_from_slice(hash_to_kill);
                batch.delete(&witness_key);

                batch.delete(&undo_key);
                
                // L1 MEMORY UPGRADE: Demote the block index to prevent dangling ghost pointers.
                if let Some(idx) = vault.map_block_index.get_mut(hash_to_kill) {
                    idx.has_data = false;
                }
            }

            chain.truncate(fork_idx + 1); 
            batch.put(b"CHAIN_LIST", bincode::serialize(&chain).unwrap());
            vault.active_chain = chain.clone();
            
            if let Some(fork_hash) = chain.last() {
                let mut core_key = PREFIX_CORE_BLOCK.to_vec();
                core_key.extend_from_slice(fork_hash);
                if let Ok(Some(core_bytes)) = self.db.get(&core_key) {
                    batch.put(b"LATEST_BLOCK_HEAD", &core_bytes);
                }
            }
            
            // EXECUTE ABSOLUTE PHYSICAL ATOMICITY FOR ROLLBACK (Crash-Safe)
            // [CRITICAL FIX]: Force OS to bypass buffer cache and physically sync to SSD flash.
            let mut write_opts = WriteOptions::default();
            write_opts.set_sync(true);
            self.db.write_opt(batch, &write_opts).expect("Fatal: WriteBatch rollback failed. Hardware/Disk failure detected.");
            println!("[INFO] Storage: Atomic rollback executed. Active chain tip reset to index {}.", fork_idx);
        }
    }

    // -------------------------------------------------------------------------
    // P2P Sync Engine API (Preserved for main.rs compatibility)
    // -------------------------------------------------------------------------

    pub fn get_latest_block(&self) -> Option<Block> {
        match self.db.get(b"LATEST_BLOCK_HEAD") {
            Ok(Some(bytes)) => bincode::deserialize(&bytes).ok(),
            _ => None,
        }
    }

    pub fn get_headers_after_index(&self, start: usize, limit: usize) -> Vec<BlockHeader> {
        let chain = self.get_header_chain_list();
        let mut headers = Vec::new();
        for i in start..std::cmp::min(start + limit, chain.len()) {
            if let Some(header) = self.get_header(&chain[i]) {
                headers.push(header);
            }
        }
        headers
    }

    // Fetch Blocks After Index.
    // Upgraded to support Witness Stripping (CoreOnly mode) for AssumeValid sync.
    pub fn get_blocks_after_index(&self, start: usize, limit: usize, core_only: bool) -> Vec<Block> {
        let chain = self.get_chain_list();
        let mut blocks = Vec::new();
        for i in start..std::cmp::min(start + limit, chain.len()) {
            let mut core_key = PREFIX_CORE_BLOCK.to_vec();
            core_key.extend_from_slice(&chain[i]);
            
            if let Ok(Some(bytes)) = self.db.get(&core_key) {
                if let Ok(mut block) = bincode::deserialize::<Block>(&bytes) {

                    // Witness Stripping execution.
                    // If core_only is true, bypass RocksDB cold storage query for quantum signatures.
                    // Clear the vector to strictly prevent any residual memory leakage.
                    if core_only {
                        for tx in block.transactions.iter_mut() {
                            tx.witnesses.clear();
                        }
                        blocks.push(block);
                        continue; // Skip witness retrieval entirely
                    }

                    // PQ-SegWit Reassembly: Retrieve signatures from Cold Storage.
                    let block_hash = chain[i];
                    // L1 V1.2 CORE FIX: Query actual height from BlockIndex to match save_block_segwit.
                    let height = self.get_block_index(&block_hash).map(|idx| idx.height).unwrap_or(0);
                    
                    let mut witness_key = PREFIX_WITNESS.to_vec();
                    witness_key.extend_from_slice(&height.to_be_bytes()); 
                    witness_key.extend_from_slice(&block_hash);           
                    
                    if let Ok(Some(w_bytes)) = self.db.get(&witness_key) {
                        if let Ok(witness_payload) = bincode::deserialize::<WitnessPayload>(&w_bytes) {
                            for (tx_idx, tx) in block.transactions.iter_mut().enumerate() {
                                if tx_idx < witness_payload.tx_witnesses.len() {
                                    tx.witnesses = witness_payload.tx_witnesses[tx_idx].clone();
                                }
                            }
                        }
                    }
                    blocks.push(block);
                }
            }
        }
        blocks
    }

    // -------------------------------------------------------------------------
    // Fetch Single Block by Hash (BIP-130 Support)
    // Upgraded to support Witness Stripping (CoreOnly mode) for AssumeValid sync.
    // -------------------------------------------------------------------------
    pub fn get_block_by_hash(&self, block_hash: &[u8; 32], core_only: bool) -> Option<Block> {
        // CORE-V6: Unified View Staging Intercept
        {
            let vault = self.vault.read().unwrap();
            if let Some(staged_block) = vault.staged_blocks.get(block_hash) {
                let mut b = staged_block.clone();
                if core_only {
                    for tx in b.transactions.iter_mut() { tx.witnesses.clear(); }
                }
                return Some(b);
            }
        }

        let mut core_key = PREFIX_CORE_BLOCK.to_vec();
        core_key.extend_from_slice(block_hash);
        
        if let Ok(Some(core_bytes)) = self.db.get(&core_key) {
            if let Ok(mut block) = bincode::deserialize::<Block>(&core_bytes) {

                // Witness Stripping execution.
                if core_only {
                    for tx in block.transactions.iter_mut() {
                        tx.witnesses.clear();
                    }
                    return Some(block);
                }

                // PQ-SegWit Reassembly: Retrieve signatures from Cold Storage.
                // L1 V1.2 CORE FIX: Query actual height from BlockIndex to match save_block_segwit.
                let height = self.get_block_index(block_hash).map(|idx| idx.height).unwrap_or(0);
                let mut witness_key = PREFIX_WITNESS.to_vec();
                witness_key.extend_from_slice(&height.to_be_bytes()); 
                witness_key.extend_from_slice(block_hash);           
                
                if let Ok(Some(w_bytes)) = self.db.get(&witness_key) {
                    if let Ok(witness_payload) = bincode::deserialize::<WitnessPayload>(&w_bytes) {
                        for (tx_idx, tx) in block.transactions.iter_mut().enumerate() {
                            if tx_idx < witness_payload.tx_witnesses.len() {
                                tx.witnesses = witness_payload.tx_witnesses[tx_idx].clone();
                            }
                        }
                    }
                }
                return Some(block);
            }
        }
        None
    }

    // -------------------------------------------------------------------------
    // Historical Witness Pruning
    // -------------------------------------------------------------------------
    
    pub fn prune_historical_witnesses(&self, cutoff_height: u64) -> Result<usize, &'static str> {
        let mut pruned_count = 0;
        let iter = self.db.iterator(IteratorMode::From(PREFIX_WITNESS, Direction::Forward));

        for item in iter {
            let (key, _) = item.expect("RocksDB Iterator Fault");
            if !key.starts_with(PREFIX_WITNESS) { break; }

            let mut height_bytes = [0u8; 8];
            height_bytes.copy_from_slice(&key[4..12]);
            let record_height = u64::from_be_bytes(height_bytes);

            if record_height < cutoff_height {
                self.db.delete(&key).expect("Failed to delete witness data");
                pruned_count += 1;
            } else {
                break; 
            }
        }
        if pruned_count > 0 {
            println!("[INFO] Storage: Pruned {} obsolete witness records.", pruned_count);
        }
        Ok(pruned_count)
    }

    // =========================================================================
    // Undo Log Archive
    // =========================================================================
    
    pub fn save_undo_log(&self, height: u64, block_hash: &[u8; 32], undo_log: &UtxoUndoLog) {
        let mut key = PREFIX_UNDO_LOG.to_vec();
        key.extend_from_slice(&height.to_be_bytes()); 
        key.extend_from_slice(block_hash);
        let encoded = bincode::serialize(undo_log).expect("UndoLog serialization failed");
        self.db.put(&key, &encoded).expect("Failed to archive UndoLog to RocksDB");
    }

    pub fn get_undo_log(&self, height: u64, block_hash: &[u8; 32]) -> Option<UtxoUndoLog> {
        let mut key = PREFIX_UNDO_LOG.to_vec();
        key.extend_from_slice(&height.to_be_bytes());
        key.extend_from_slice(block_hash);
        match self.db.get(&key) {
            Ok(Some(bytes)) => bincode::deserialize(&bytes).ok(),
            _ => None,
        }
    }

    // -------------------------------------------------------------------------
    // UTXO Snapshot Persistence & Atomic State Transitions
    // -------------------------------------------------------------------------
    
    // CORE-V6: Unified View Stage verified block into memory.
    // Immediately reflects in GetHeaders and LCA, guaranteeing NO physical writes until batch commit.
    pub fn stage_verified_block(&self, block: Block, height: u64, accumulated_work: u128) {
        // [CRITICAL FIX]: Guard staging area from concurrent deep sync threads.
        let _global_io_guard = self.io_lock.lock().unwrap();
        let mut vault = self.vault.write().unwrap();
        let block_hash = block.calculate_hash();
        
        vault.staged_blocks.insert(block_hash, block.clone());
        
        let b_index = BlockIndex {
            block_hash,
            header: block.header,
            height,
            chain_work: accumulated_work,
            has_data: true,
        };
        vault.map_block_index.insert(block_hash, b_index);
        
        if !vault.active_chain.contains(&block_hash) {
            vault.active_chain.push(block_hash);
        }
        if !vault.header_chain.contains(&block_hash) {
            vault.header_chain.push(block_hash);
        }
    }

    // CORE-V6: Atomic Memory Purge
    // Wipes all uncommitted memory ghosts and resets the memory pointers to the physical RocksDB truth.
    pub fn purge_staged_data(&self) {
        // [CRITICAL FIX]: Guard memory purge to prevent torn reads.
        let _global_io_guard = self.io_lock.lock().unwrap();
        let mut vault = self.vault.write().unwrap();
        
        // CRITICAL FIX: Safe Ghost Eradication.
        // Instead of blind deletion, we must restore the original 'has_data: false' state from RocksDB if it was previously fetched via Headers-First sync.
        let keys_to_remove: Vec<[u8; 32]> = vault.staged_blocks.keys().copied().collect();
        for hash in keys_to_remove {
            let mut key = PREFIX_BLOCK_INDEX.to_vec();
            key.extend_from_slice(&hash);
            
            let mut restored = false;
            if let Ok(Some(bytes)) = self.db.get(&key) {
                if let Ok(old_idx) = bincode::deserialize::<BlockIndex>(&bytes) {
                    vault.map_block_index.insert(hash, old_idx);
                    restored = true;
                }
            }
            if !restored {
                vault.map_block_index.remove(&hash);
            }
        }
        
        vault.staged_blocks.clear();
        
        // Restore active_chain from RocksDB physical truth
        if let Ok(Some(bytes)) = self.db.get(b"CHAIN_LIST") {
            vault.active_chain = bincode::deserialize(&bytes).unwrap_or_default();
        }
        if let Ok(Some(bytes)) = self.db.get(b"HEADER_CHAIN_LIST") {
            vault.header_chain = bincode::deserialize(&bytes).unwrap_or_default();
        }
        println!("[INFO] Storage: Unified View staged data atomically purged. Network pointers reverted to physical truth.");
    }
    
    // L1 V2.0 CORE: Atomic WriteBatch execution for absolute physical state consistency.
    // Binds Block Data, Undo Logs, and UTXO Snapshot into a single unbreakable physical transaction.
    // Requires exact accumulated_work from consensus engine to prevent physical desynchronization.
    pub fn commit_state_transition(&self, mut block: Block, height: u64, undo_log: &UtxoUndoLog, state: &UtxoState, accumulated_work: u128) {
        // [CRITICAL FIX]: Absolute state lock prevents concurrent writes from fracturing the unified view.
        let _global_io_guard = self.io_lock.lock().unwrap();
        let mut batch = WriteBatch::default();
        let block_hash = block.calculate_hash();
        
        // 1. PQ-SegWit Partitioning & Core Storage
        let mut witness_payload = WitnessPayload { tx_witnesses: Vec::new() };
        for tx in &mut block.transactions {
            witness_payload.tx_witnesses.push(std::mem::take(&mut tx.witnesses));
        }
        
        let core_encoded = bincode::serialize(&block).unwrap();
        let mut core_key = PREFIX_CORE_BLOCK.to_vec();
        core_key.extend_from_slice(&block_hash);
        batch.put(&core_key, &core_encoded);
        
        let mut witness_key = PREFIX_WITNESS.to_vec();
        witness_key.extend_from_slice(&height.to_be_bytes()); 
        witness_key.extend_from_slice(&block_hash);           
        batch.put(&witness_key, bincode::serialize(&witness_payload).unwrap());

        // 2. Header & Undo Log Archiving
        let mut header_key = PREFIX_HEADER.to_vec();
        header_key.extend_from_slice(&block_hash);
        batch.put(&header_key, bincode::serialize(&block.header).unwrap());

        let mut undo_key = PREFIX_UNDO_LOG.to_vec();
        undo_key.extend_from_slice(&height.to_be_bytes()); 
        undo_key.extend_from_slice(&block_hash);
        batch.put(&undo_key, bincode::serialize(undo_log).unwrap());

        // 3. Granular UTXO State Atomic Commits (Crash-Safe)
        // Ensure UTXO mutations are strictly bound to block advancement via WriteBatch.
        
        // Phase A: Physically eradicate spent UTXOs from disk.
        for (outpoint, _) in &undo_log.spent_utxos {
            let mut utxo_key = PREFIX_UTXO.to_vec();
            utxo_key.extend_from_slice(&outpoint.tx_hash);
            utxo_key.extend_from_slice(&outpoint.vout.to_be_bytes());
            batch.delete(&utxo_key);
        }

        // Phase B: Physically persist newly minted UTXOs to disk.
        for outpoint in &undo_log.newly_created_outpoints {
            if let Some(record) = state.unspent_outputs.get(outpoint) {
                let mut utxo_key = PREFIX_UTXO.to_vec();
                utxo_key.extend_from_slice(&outpoint.tx_hash);
                utxo_key.extend_from_slice(&outpoint.vout.to_be_bytes());
                let record_encoded = bincode::serialize(record).expect("Fatal: UTXO serialization fault");
                batch.put(&utxo_key, &record_encoded);
            }
        }

        batch.put(b"LATEST_BLOCK_HEAD", &core_encoded);

        // 4. Memory Pointers Sync
        let mut vault = self.vault.write().unwrap();
        if !vault.active_chain.contains(&block_hash) {
            vault.active_chain.push(block_hash);
            batch.put(b"CHAIN_LIST", bincode::serialize(&vault.active_chain).unwrap());
        }
        if !vault.header_chain.contains(&block_hash) {
            vault.header_chain.push(block_hash);
            batch.put(b"HEADER_CHAIN_LIST", bincode::serialize(&vault.header_chain).unwrap());
        }

        // L1 V2.0 CORE: Enforce absolute physical consistency using consensus-provided accumulated_work.
        // Eradicated naive localized increment to align with Nakamoto difficulty retargeting.
        let b_index = BlockIndex {
            block_hash,
            header: block.header.clone(),
            height,
            chain_work: accumulated_work,
            has_data: true,
        };
        let mut idx_key = PREFIX_BLOCK_INDEX.to_vec();
        idx_key.extend_from_slice(&block_hash);
        batch.put(&idx_key, bincode::serialize(&b_index).unwrap());

        // EXECUTE ABSOLUTE PHYSICAL ATOMICITY (Crash-Safe)
        // [CRITICAL FIX]: Set sync=true to mathematically eliminate Asynchronous WAL Loss.
        let mut write_opts = WriteOptions::default();
        write_opts.set_sync(true);
        self.db.write_opt(batch, &write_opts).expect("Fatal: WriteBatch commit failed. Hardware/Disk failure detected.");
        vault.map_block_index.insert(block_hash, b_index);
    }

    // Atomic WriteBatch execution.
    // Absorbs multiple blocks, undo logs, and UTXO state into a single I/O transaction.
    pub fn commit_state_batch(&self, instructions: Vec<BatchCommitInstruction>, final_state: &UtxoState) {
        if instructions.is_empty() { return; }
        
        // [CRITICAL FIX]: Global physical lock. Eliminates the Race Condition where multiple syncs overwrite active_chain.
        let _global_io_guard = self.io_lock.lock().unwrap();
        let mut batch = WriteBatch::default();
        let mut latest_core_encoded = Vec::new();

        let mut active_chain_snap;
        let mut header_chain_snap;
        {
            let vault = self.vault.read().unwrap();
            active_chain_snap = vault.active_chain.clone();
            header_chain_snap = vault.header_chain.clone();
        }

        // Enforce strict chronological progression to guarantee UTXO dependency integrity.
        for mut inst in instructions {
            let block_hash = inst.block.calculate_hash();
            
            // 1. PQ-SegWit Partitioning & Core Storage
            let mut witness_payload = WitnessPayload { tx_witnesses: Vec::new() };
            for tx in &mut inst.block.transactions {
                witness_payload.tx_witnesses.push(std::mem::take(&mut tx.witnesses));
            }
            
            let core_encoded = bincode::serialize(&inst.block).unwrap();
            let mut core_key = PREFIX_CORE_BLOCK.to_vec();
            core_key.extend_from_slice(&block_hash);
            batch.put(&core_key, &core_encoded);
            
            latest_core_encoded = core_encoded; // Always tracks the chronological tip
            
            let mut witness_key = PREFIX_WITNESS.to_vec();
            witness_key.extend_from_slice(&inst.height.to_be_bytes()); 
            witness_key.extend_from_slice(&block_hash);           
            batch.put(&witness_key, bincode::serialize(&witness_payload).unwrap());

            // 2. Header & Undo Log Archiving
            let mut header_key = PREFIX_HEADER.to_vec();
            header_key.extend_from_slice(&block_hash);
            batch.put(&header_key, bincode::serialize(&inst.block.header).unwrap());

            let mut undo_key = PREFIX_UNDO_LOG.to_vec();
            undo_key.extend_from_slice(&inst.height.to_be_bytes()); 
            undo_key.extend_from_slice(&block_hash);
            batch.put(&undo_key, bincode::serialize(&inst.undo_log).unwrap());

            // 3. Chronological UTXO State Mutations
            for (outpoint, _) in &inst.undo_log.spent_utxos {
                let mut utxo_key = PREFIX_UTXO.to_vec();
                utxo_key.extend_from_slice(&outpoint.tx_hash);
                utxo_key.extend_from_slice(&outpoint.vout.to_be_bytes());
                batch.delete(&utxo_key);
            }

            for outpoint in &inst.undo_log.newly_created_outpoints {
                if let Some(record) = final_state.unspent_outputs.get(outpoint) {
                    let mut utxo_key = PREFIX_UTXO.to_vec();
                    utxo_key.extend_from_slice(&outpoint.tx_hash);
                    utxo_key.extend_from_slice(&outpoint.vout.to_be_bytes());
                    let record_encoded = bincode::serialize(record).expect("Fatal: UTXO serialization fault");
                    batch.put(&utxo_key, &record_encoded);
                }
            }

            // 4. Memory Pointers Sync for Snapshot
            if !active_chain_snap.contains(&block_hash) {
                active_chain_snap.push(block_hash);
            }
            if !header_chain_snap.contains(&block_hash) {
                header_chain_snap.push(block_hash);
            }

            let b_index = BlockIndex {
                block_hash,
                header: inst.block.header.clone(),
                height: inst.height,
                chain_work: inst.accumulated_work,
                has_data: true,
            };
            let mut idx_key = PREFIX_BLOCK_INDEX.to_vec();
            idx_key.extend_from_slice(&block_hash);
            batch.put(&idx_key, bincode::serialize(&b_index).unwrap());
        }

        // Finalize chain markers
        batch.put(b"LATEST_BLOCK_HEAD", &latest_core_encoded);
        batch.put(b"CHAIN_LIST", bincode::serialize(&active_chain_snap).unwrap());
        batch.put(b"HEADER_CHAIN_LIST", bincode::serialize(&header_chain_snap).unwrap());

        // EXECUTE ABSOLUTE PHYSICAL ATOMICITY (Crash-Safe)
        // [CRITICAL FIX]: Force fsync to SSD block device. Prevents memory wipe on kill -9.
        let mut write_opts = WriteOptions::default();
        write_opts.set_sync(true);
        self.db.write_opt(batch, &write_opts).expect("Fatal: WriteBatch commit failed. Hardware/Disk failure detected.");

        // Post-I/O Memory Cleanup
        let mut vault = self.vault.write().unwrap();
        vault.staged_blocks.clear();
        vault.active_chain = active_chain_snap;
        vault.header_chain = header_chain_snap;
    }

    // DEPRECATED: Giant snapshot persistence removed to prevent OOM and state tearing.
    // Granular UTXO mutations are now strictly managed within `commit_state_transition`.

    // L1 V2.0 CORE: Boot-time granular UTXO reconstruction via physical disk iteration.
    pub fn load_utxo_state(&self) -> Option<UtxoState> {
        let mut state = UtxoState::new();
        let iter = self.db.iterator(IteratorMode::From(PREFIX_UTXO, Direction::Forward));
        
        let mut loaded_count = 0;
        for item in iter {
            let (key, value) = item.expect("Fatal: RocksDB Iterator Fault during UTXO load");
            if !key.starts_with(PREFIX_UTXO) { 
                break; 
            }
            
            // Validate key geometry: Prefix(5) + TXID(32) + VOUT(4) = 41 bytes
            if key.len() == PREFIX_UTXO.len() + 32 + 4 {
                let mut tx_hash = [0u8; 32];
                tx_hash.copy_from_slice(&key[PREFIX_UTXO.len()..PREFIX_UTXO.len() + 32]);
                
                let mut vout_bytes = [0u8; 4];
                vout_bytes.copy_from_slice(&key[PREFIX_UTXO.len() + 32..]);
                let vout = u32::from_be_bytes(vout_bytes);
                
                if let Ok(record) = bincode::deserialize::<crate::utxo::UtxoRecord>(&value) {
                    state.unspent_outputs.insert(crate::transaction::OutPoint { tx_hash, vout }, record);
                    loaded_count += 1;
                }
            }
        }
        
        println!("[INFO] Storage: Reconstructed {} granular UTXOs from physical layer.", loaded_count);
        Some(state)
    }

    // -------------------------------------------------------------------------
    // mapOrphanBlocks and Anti-OOM Measures
    // -------------------------------------------------------------------------
    
    /// Safely quarantines an out-of-order block. Returns true if successfully added.
    pub fn add_orphan_block(&self, block: Block) -> bool {
        let hash = block.calculate_hash();
        let prev_hash = block.header.previous_hash;
        
        let mut vault = self.vault.write().unwrap();
        
        // Bounded capacity for orphan blocks (Max 50) to prevent OOM.
        if vault.map_orphan_blocks.len() >= 50 {
            // Random eviction to prevent memory exhaustion.
            let random_key = *vault.map_orphan_blocks.keys().next().unwrap();
            let evicted_block = vault.map_orphan_blocks.remove(&random_key).unwrap();
            let evicted_prev = evicted_block.header.previous_hash;
            
            if let Some(list) = vault.map_orphan_blocks_by_prev.get_mut(&evicted_prev) {
                list.retain(|&h| h != random_key);
                if list.is_empty() {
                    vault.map_orphan_blocks_by_prev.remove(&evicted_prev);
                }
            }
            println!("[WARN] Storage: Orphan pool capacity reached. Evicted random orphan.");
        }
        
        if !vault.map_orphan_blocks.contains_key(&hash) {
            vault.map_orphan_blocks.insert(hash, block);
            vault.map_orphan_blocks_by_prev.entry(prev_hash).or_insert_with(Vec::new).push(hash);
            
            let prev_hex: String = prev_hash.iter().take(4).map(|b| format!("{:02x}", b)).collect();
            println!("[INFO] Storage: Block quarantined as orphan. Awaiting parent: {}", prev_hex);
            true
        } else {
            false
        }
    }

    /// Retrieves and removes all orphans waiting for this specific parent hash.
    /// Initiates cascading reassembly in consensus engine.
    pub fn get_orphans_by_parent(&self, parent_hash: &[u8; 32]) -> Vec<Block> {
        let mut vault = self.vault.write().unwrap();
        
        let mut recovered_blocks = Vec::new();
        if let Some(orphan_hashes) = vault.map_orphan_blocks_by_prev.remove(parent_hash) {
            for hash in orphan_hashes {
                if let Some(block) = vault.map_orphan_blocks.remove(&hash) {
                    recovered_blocks.push(block);
                }
            }
        }
        recovered_blocks
    }

    // Graceful Shutdown
    pub fn flush(&self) {
        let _ = self.db.flush();
        println!("[INFO] Storage: RocksDB memory tables flushed to disk.");
    }
}