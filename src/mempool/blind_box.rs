// src/mempool/blind_box.rs
// Nakamoto DAG Mempool Architecture.
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, BTreeSet};
use indexmap::IndexMap; // CORE-V6 FIX: Required for DAG topological sequence preservation
use crate::transaction::Transaction;

// -----------------------------------------------------------------------------
// Data Structures
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MempoolEntry {
    pub tx: Transaction,
    pub fee: u64,
    pub weight: u64, // CORE-V3: Replaced vbytes with Weight Units
    pub timestamp: u64,
}

// O(log N) sorting index structure for instant eviction based on fee rates.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FeeRateKey {
    pub rate: u64, 
    pub tx_hash: [u8; 32],
}

impl Ord for FeeRateKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match self.rate.cmp(&other.rate) {
            std::cmp::Ordering::Equal => self.tx_hash.cmp(&other.tx_hash),
            other_ordering => other_ordering,
        }
    }
}

impl PartialOrd for FeeRateKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, PartialEq)]
pub enum MempoolError {
    AlreadyExists,
    DoubleSpendDetected,
    MempoolFull,      
    FeeTooLow,
    // Rejected by tombstone cache.
    Tombstoned,
    // Too large to ever fit in a block template.
    TooLarge,
}

// -----------------------------------------------------------------------------
// Core Mempool State Machine
// -----------------------------------------------------------------------------
pub struct QuantumMempool {
    // Upgraded to IndexMap to guarantee DAG topological snapshot order.
    pub tx_pool: IndexMap<[u8; 32], MempoolEntry>,
    pub fee_index: BTreeSet<FeeRateKey>,
    pub spent_outpoints: HashSet<([u8; 32], u32)>,
    pub child_index: HashMap<[u8; 32], Vec<[u8; 32]>>,
    pub orphan_pool: HashMap<[u8; 32], Transaction>,
    pub current_weight: u64, // CORE-V3: Track capacity via mathematical weight
    // L1 DEFENSE: Rolling Tombstone Graveyard (Bitcoin Core recentRejects).
    pub tombstones: HashSet<[u8; 32]>,
    pub tombstone_queue: std::collections::VecDeque<[u8; 32]>,
}

impl QuantumMempool {
    // CORE-V3: Capacity scaled mathematically. 1.2B WU ≈ 300MB physical equivalent.
    pub const MAX_MEMPOOL_WEIGHT: u64 = 1_200_000_000;
    // L1 DEFENSE: Max limits for physical memory bounds.
    pub const MAX_TOMBSTONES: usize = 8192;
    pub const MAX_ORPHANS: usize = 1024;

    pub fn new() -> Self {
        Self {
            tx_pool: IndexMap::new(),
            fee_index: BTreeSet::new(),
            spent_outpoints: HashSet::new(),
            child_index: HashMap::new(),
            orphan_pool: HashMap::new(),
            current_weight: 0,
            tombstones: HashSet::new(),
            tombstone_queue: std::collections::VecDeque::new(),
        }
    }

    // Calculates a scaled integer representation of Sats/WU.
    fn calculate_rate(fee: u64, weight: u64) -> u64 {
        ((fee as u128 * 1_000_000) / weight.max(1) as u128) as u64
    }

    // Phase 1: Bare-metal admission protocol.
    // Enforces First-Seen-Safe rule. RBF is strictly prohibited.
    pub fn add_transaction(&mut self, tx: Transaction, fee: u64) -> Result<(), MempoolError> {
        let tx_hash = tx.calculate_id();

        // L1 DEFENSE: Intercept zombie transactions to prevent memory rebounce.
        if self.tombstones.contains(&tx_hash) {
            return Err(MempoolError::Tombstoned);
        }

        if self.tx_pool.contains_key(&tx_hash) {
            return Err(MempoolError::AlreadyExists);
        }

        for input in &tx.inputs {
            if self.spent_outpoints.contains(&(input.previous_output_hash, input.vout)) {
                tracing::warn!("[WARN] Mempool: Double-spend collision detected. Transaction rejected.");
                return Err(MempoolError::DoubleSpendDetected);
            }
        }

        let tx_weight = tx.get_weight();
        let tx_sigops = tx.get_sigops_count();
        let current_rate = Self::calculate_rate(fee, tx_weight);

        // CORE-V3: Native SegWit & SigOps mathematical clamping.
        if tx_weight > crate::config::MAX_BLOCK_WEIGHT as u64 || tx_sigops > crate::config::MAX_BLOCK_SIGOPS as usize {
            return Err(MempoolError::MempoolFull); 
        }
        // A tx that can never fit in a template would sit here until it expires.
        let max_tx_size = crate::config::MAX_TEMPLATE_BLOCK_BYTES.saturating_sub(crate::config::BLOCK_TEMPLATE_RESERVE_BYTES);
        if tx.get_physical_size() > max_tx_size {
            return Err(MempoolError::TooLarge);
        }
        // calculate_rate() is scaled by 1_000_000, MIN_RELAY_FEE_RATE is in sats/WU.
        if current_rate < crate::config::MIN_RELAY_FEE_RATE.saturating_mul(1_000_000) {
            return Err(MempoolError::FeeTooLow);
        }

        // O(1) Dynamic Eviction Protocol using BTreeSet
        while self.current_weight + tx_weight > Self::MAX_MEMPOOL_WEIGHT {
            if let Some(lowest) = self.fee_index.first().cloned() {
                if current_rate <= lowest.rate {
                    return Err(MempoolError::FeeTooLow);
                }
                self.evict_transaction_and_descendants(&lowest.tx_hash);
                // Always make progress, even if the index had a stale entry.
                self.fee_index.remove(&lowest);
            } else {
                break;
            }
        }

        self.current_weight += tx_weight;
        self.fee_index.insert(FeeRateKey { rate: current_rate, tx_hash });
        
        // Register DAG dependencies for cascade eviction.
        for input in &tx.inputs {
            self.spent_outpoints.insert((input.previous_output_hash, input.vout));
            if self.tx_pool.contains_key(&input.previous_output_hash) {
                self.child_index.entry(input.previous_output_hash).or_default().push(tx_hash);
            }
        }

        self.tx_pool.insert(tx_hash, MempoolEntry {
            tx,
            fee,
            weight: tx_weight,
            timestamp: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
        });

        Ok(())
    }

    pub fn add_orphan(&mut self, tx: Transaction) {
        let tx_hash = tx.calculate_id();
        // Orphan Pool Hard Cap. O(1) random eviction.
        if self.orphan_pool.len() >= Self::MAX_ORPHANS {
            if let Some(key_to_remove) = self.orphan_pool.keys().next().cloned() {
                self.orphan_pool.remove(&key_to_remove);
            }
        }
        self.orphan_pool.insert(tx_hash, tx);
    }

    // Time-To-Live Purge.
    pub fn purge_stale_transactions(&mut self, max_age_seconds: u64) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let mut stale_hashes = Vec::new();

        for (hash, entry) in &self.tx_pool {
            if now.saturating_sub(entry.timestamp) > max_age_seconds {
                stale_hashes.push(*hash);
            }
        }

        for hash in stale_hashes {
            if self.tombstones.insert(hash) {
                self.tombstone_queue.push_back(hash);
                // FIFO Ring Buffer eviction.
                if self.tombstone_queue.len() > Self::MAX_TOMBSTONES {
                    if let Some(oldest) = self.tombstone_queue.pop_front() {
                        self.tombstones.remove(&oldest);
                    }
                }
            }
            self.evict_transaction_and_descendants(&hash);
        }
    }

    /// Empties the pool and every index that describes it. Tombstones are kept
    /// so the dropped txs aren't immediately re-admitted.
    pub fn clear_pool(&mut self) {
        self.tx_pool.clear();
        self.fee_index.clear();
        self.spent_outpoints.clear();
        self.child_index.clear();
        self.current_weight = 0;
    }

    /// Removes txs (and anything spending their outputs) while keeping every
    /// index in sync. Use this instead of touching tx_pool directly.
    pub fn remove_with_descendants(&mut self, hashes: &HashSet<[u8; 32]>) {
        for hash in hashes {
            self.evict_transaction_and_descendants(hash);
        }
    }

    // Recursive cascade eviction for sub-trees to prevent memory leaks.
    fn evict_transaction_and_descendants(&mut self, tx_hash: &[u8; 32]) {
        let mut stack = vec![*tx_hash];
        
        while let Some(current_hash) = stack.pop() {
            if let Some(children) = self.child_index.remove(&current_hash) {
                for child in children {
                    stack.push(child);
                }
            }

            // CORE-V6 FIX: Use shift_remove for single eviction to strictly preserve DAG order.
            if let Some(entry) = self.tx_pool.shift_remove(&current_hash) {
                self.current_weight = self.current_weight.saturating_sub(entry.tx.get_weight());
                self.fee_index.remove(&FeeRateKey { 
                    rate: Self::calculate_rate(entry.fee, entry.weight), 
                    tx_hash: current_hash 
                });
                
                for input in &entry.tx.inputs {
                    self.spent_outpoints.remove(&(input.previous_output_hash, input.vout));
                }
            }
        }
    }

    // Phase 2: Topological sorting for mining using DAG constraints.
    // CORE-V3: Dual-lock greedy knapsack algorithm (Weight + SigOps).
    pub fn get_txs_for_mining(&self) -> Vec<Transaction> {
        let mut valid_txs = Vec::new();
        let mut block_spent_utxos = HashSet::new();
        let mut current_block_weight: u64 = 480; // Block header reserved weight (120 * 4)
        let mut current_block_sigops: usize = 0;
        // Weight alone is not enough: ML-DSA witnesses are counted at 1 WU/byte,
        // so MAX_BLOCK_WEIGHT admits ~31MB of them while the firewall drops any
        // block over MAX_BLOCK_SIZE_BYTES.
        let mut current_block_size: usize = crate::config::BLOCK_TEMPLATE_RESERVE_BYTES;
        
        let mut in_degree: HashMap<[u8; 32], usize> = HashMap::new();

        for (tx_hash, entry) in &self.tx_pool {
            let mut pending_parents_count = 0;
            for input in &entry.tx.inputs {
                if self.tx_pool.contains_key(&input.previous_output_hash) {
                    pending_parents_count += 1;
                }
            }
            in_degree.insert(*tx_hash, pending_parents_count);
        }

        let mut ready_queue: Vec<[u8; 32]> = in_degree.iter()
            .filter(|&(_, &count)| count == 0) 
            .map(|(&hash, _)| hash)
            .collect();

        while !ready_queue.is_empty() {
            // Sort by fee rate descending.
            ready_queue.sort_by(|a, b| {
                let rate_a = self.tx_pool.get(a).map(|e| Self::calculate_rate(e.fee, e.weight)).unwrap_or(0);
                let rate_b = self.tx_pool.get(b).map(|e| Self::calculate_rate(e.fee, e.weight)).unwrap_or(0);
                rate_a.partial_cmp(&rate_b).unwrap_or(std::cmp::Ordering::Equal)
            });

            let current_hash = ready_queue.pop().unwrap();
            
            if let Some(entry) = self.tx_pool.get(&current_hash) {
                let tx_weight = entry.tx.get_weight();
                let tx_sigops = entry.tx.get_sigops_count();
                let tx_size = entry.tx.get_physical_size();

                // Enforce mathematical constraints. Skip if exceeds, but keep packing smaller txs.
                if current_block_weight + tx_weight > crate::config::MAX_BLOCK_WEIGHT as u64 
                    || current_block_sigops + tx_sigops > crate::config::MAX_BLOCK_SIGOPS as usize
                    || current_block_size + tx_size > crate::config::MAX_TEMPLATE_BLOCK_BYTES {
                    continue; 
                }

                let mut is_conflict = false;
                for input in &entry.tx.inputs {
                    if block_spent_utxos.contains(&(input.previous_output_hash, input.vout)) {
                        is_conflict = true;
                        break;
                    }
                }

                if is_conflict { continue; }

                current_block_weight += tx_weight;
                current_block_sigops += tx_sigops;
                current_block_size += tx_size;
                valid_txs.push(entry.tx.clone());
                
                for input in &entry.tx.inputs {
                    block_spent_utxos.insert((input.previous_output_hash, input.vout));
                }

                // Unlock dependent children in the DAG.
                if let Some(children) = self.child_index.get(&current_hash) {
                    for child_hash in children {
                        if let Some(count) = in_degree.get_mut(child_hash) {
                            *count -= 1;
                            if *count == 0 { ready_queue.push(*child_hash); }
                        }
                    }
                }
            }
        }
        
        valid_txs
    }

    // Phase 3: Atomic State Sweeping.
    pub fn atomic_sweep(&mut self, mined_txs: &[Transaction]) {
        let mut globally_spent_utxos = HashSet::new();
        
        for tx in mined_txs {
            for input in &tx.inputs {
                globally_spent_utxos.insert((input.previous_output_hash, input.vout));
            }
            // Clear confirmed transactions from orphan pool if they existed.
            self.orphan_pool.remove(&tx.calculate_id());
        }

        let mut hashes_to_remove = Vec::new();
        for (tx_hash, entry) in &self.tx_pool {
            let mut is_invalidated = false;
            for input in &entry.tx.inputs {
                if globally_spent_utxos.contains(&(input.previous_output_hash, input.vout)) {
                    is_invalidated = true; 
                    break;
                }
            }
            if is_invalidated { hashes_to_remove.push(*tx_hash); }
        }

        // A pool tx that conflicts with the block (spends the same input but was
        // not itself mined) can never confirm, and neither can anything built on
        // its outputs: evict it together with its descendants. Mined txs are
        // removed alone, since their children are now valid.
        let mined_ids: HashSet<[u8; 32]> = mined_txs.iter().map(|tx| tx.calculate_id()).collect();
        let (hashes_to_remove, conflicts): (Vec<_>, Vec<_>) =
            hashes_to_remove.into_iter().partition(|h| mined_ids.contains(h));
        for hash in &conflicts {
            self.evict_transaction_and_descendants(hash);
        }

        // Convert to HashSet for O(1) lookup to prevent CPU exhaustion.
        let remove_set: HashSet<_> = hashes_to_remove.into_iter().collect();

        for hash in &remove_set {
            if let Some(removed_entry) = self.tx_pool.get(hash) {
                self.current_weight = self.current_weight.saturating_sub(removed_entry.tx.get_weight());
                self.fee_index.remove(&FeeRateKey {
                    rate: Self::calculate_rate(removed_entry.fee, removed_entry.weight),
                    tx_hash: *hash 
                });
                self.child_index.remove(hash);
                for input in &removed_entry.tx.inputs {
                    self.spent_outpoints.remove(&(input.previous_output_hash, input.vout));
                }
            }
        }
        
        // Execute a single atomic memory sweep, preserving topological order at maximum speed.
        self.tx_pool.retain(|hash, _| !remove_set.contains(hash));
    }
}
#[cfg(test)]
mod hardening_tests {
    use super::*;
    use crate::transaction::{Transaction, TxIn, TxOut, TxWitness};

    fn tx(inputs: &[([u8; 32], u32)], outputs: usize, sig_len: usize) -> Transaction {
        Transaction {
            inputs: inputs.iter().map(|(h, v)| TxIn { previous_output_hash: *h, vout: *v }).collect(),
            outputs: (0..outputs).map(|_| TxOut { value: 1, public_key_hash: [7u8; 32], recovery: None }).collect(),
            witnesses: inputs.iter().map(|_| TxWitness { signature: vec![1u8; sig_len], public_key: vec![2u8; 64] }).collect(),
        }
    }
    fn fee_for(t: &Transaction, sats_per_wu: u64) -> u64 { t.get_weight() * sats_per_wu }

    #[test]
    fn rejects_fee_below_min_relay_rate() {
        let mut mp = QuantumMempool::new();
        let t = tx(&[([1u8; 32], 0)], 1, 3309);
        // 1 sat/WU is below MIN_RELAY_FEE_RATE (5 sats/WU).
        assert!(matches!(mp.add_transaction(t.clone(), fee_for(&t, 1)), Err(MempoolError::FeeTooLow)));
        assert!(mp.add_transaction(t.clone(), fee_for(&t, 10)).is_ok());
    }

    #[test]
    fn rejects_tx_too_large_for_any_template() {
        let mut mp = QuantumMempool::new();
        let t = tx(&[([2u8; 32], 0)], 1, 5_000_000);
        assert!(matches!(mp.add_transaction(t.clone(), fee_for(&t, 10)), Err(MempoolError::TooLarge)));
    }

    #[test]
    fn conflicting_tx_is_evicted_with_its_descendants() {
        let mut mp = QuantumMempool::new();
        let parent = tx(&[([3u8; 32], 0)], 1, 100);
        let pid = parent.calculate_id();
        let child = tx(&[(pid, 0)], 1, 100);
        mp.add_transaction(parent.clone(), fee_for(&parent, 10)).unwrap();
        mp.add_transaction(child.clone(), fee_for(&child, 10)).unwrap();

        // A different tx spending the same input as `parent` gets mined.
        let mined = tx(&[([3u8; 32], 0)], 2, 100);
        mp.atomic_sweep(&[mined]);

        assert!(mp.tx_pool.is_empty(), "parent and its child must both be gone");
        assert_eq!(mp.current_weight, 0);
        assert!(mp.spent_outpoints.is_empty());
        assert!(mp.fee_index.is_empty());
    }

    #[test]
    fn mined_parent_keeps_valid_child() {
        let mut mp = QuantumMempool::new();
        let parent = tx(&[([4u8; 32], 0)], 1, 100);
        let pid = parent.calculate_id();
        let child = tx(&[(pid, 0)], 1, 100);
        let cid = child.calculate_id();
        mp.add_transaction(parent.clone(), fee_for(&parent, 10)).unwrap();
        mp.add_transaction(child.clone(), fee_for(&child, 10)).unwrap();

        mp.atomic_sweep(&[parent]);

        assert_eq!(mp.tx_pool.len(), 1);
        assert!(mp.tx_pool.contains_key(&cid));
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    use crate::transaction::{TxIn, TxOut, TxWitness};

    fn tx(seed: u8, prev: [u8; 32]) -> Transaction {
        Transaction {
            inputs: vec![TxIn { previous_output_hash: prev, vout: 0 }],
            outputs: vec![TxOut { value: seed as u64, public_key_hash: [seed; 32], recovery: None }],
            witnesses: vec![TxWitness { signature: vec![seed; 64], public_key: vec![seed; 64] }],
        }
    }

    #[test]
    fn remove_with_descendants_keeps_indexes_in_sync() {
        let mut mp = QuantumMempool::new();
        let p = tx(1, [1u8; 32]);
        let c = tx(2, p.calculate_id());
        mp.add_transaction(p.clone(), p.get_weight() * 10).unwrap();
        mp.add_transaction(c.clone(), c.get_weight() * 10).unwrap();
        mp.remove_with_descendants(&[p.calculate_id()].into_iter().collect());
        assert!(mp.tx_pool.is_empty());
        assert!(mp.fee_index.is_empty());
        assert!(mp.spent_outpoints.is_empty());
        assert_eq!(mp.current_weight, 0);
    }

    #[test]
    fn eviction_makes_progress_past_stale_fee_entries() {
        let mut mp = QuantumMempool::new();
        // Simulate drift: weight says the pool is full and the fee index points
        // at a tx that is no longer in tx_pool.
        mp.current_weight = QuantumMempool::MAX_MEMPOOL_WEIGHT;
        mp.fee_index.insert(FeeRateKey { rate: 1, tx_hash: [0xEE; 32] });
        let t = tx(3, [3u8; 32]);
        // Must return instead of spinning; whether it is admitted depends on the weight left.
        let _ = mp.add_transaction(t.clone(), t.get_weight() * 50);
        assert!(!mp.fee_index.contains(&FeeRateKey { rate: 1, tx_hash: [0xEE; 32] }));
    }

    #[test]
    fn clear_pool_resets_every_index() {
        let mut mp = QuantumMempool::new();
        let t = tx(4, [4u8; 32]);
        mp.add_transaction(t.clone(), t.get_weight() * 10).unwrap();
        mp.clear_pool();
        assert!(mp.tx_pool.is_empty() && mp.fee_index.is_empty() && mp.spent_outpoints.is_empty());
        assert_eq!(mp.current_weight, 0);
    }
}
