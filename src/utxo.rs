// src/utxo.rs
// =============================================================================
// UTXO State Management
// Implementation: Ledger State with delayed recovery logic.
// Standards: NIST FIPS 204 (Lattice-Sig) & NIST SP 800-185 (KMAC256)
// =============================================================================

use std::collections::HashMap;
use serde::{Serialize, Deserialize};
use crate::transaction::{Transaction, TxIn, TxOut, OutPoint};
use crate::block::Block;

// Consensus Constants
// -----------------------------------------------------------------------------
// CORE-V6: Import global consensus parameter to ensure architectural consistency.
use crate::config::COINBASE_MATURITY;

// Dust Limit
/// Absolute minimum output value (546 units). 
/// Aligns with standard P2PKH dust limit to prevent state bloat.
pub const DUST_LIMIT: u64 = 546;

/// Recovery Challenge Window: 4320 blocks (~30 days) of waiting time
/// for inheritance claims to allow primary owner intervention.
pub const RECOVERY_CHALLENGE_WINDOW: u64 = 4320; 

/// Lock TTL: 300 seconds. Prevents freezing of assets during 
/// complex lattice computations.
pub const LOCK_TTL_SECONDS: u64 = 300;

// =============================================================================
// Undo Engine
// Port of Bitcoin Core's CBlockUndo and DisconnectBlock logic.
// =============================================================================

/// Records UTXO state changes caused by a single block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UtxoUndoLog {
    /// UTXOs that were spent by this block.
    pub spent_utxos: Vec<(OutPoint, UtxoRecord)>, 
    
    /// New UTXOs created by this block.
    pub newly_created_outpoints: Vec<OutPoint>,
}

impl UtxoUndoLog {
    pub fn new() -> Self {
        Self { spent_utxos: Vec::new(), newly_created_outpoints: Vec::new() }
    }
}

use std::collections::HashSet; //  Added for Q-SigCache

/// The atomic unit of value in the network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UtxoRecord {
    /// The core output data (Value, Public Key, Recovery Info).
    pub output: TxOut,
    /// The block height where this UTXO was confirmed or last spent.
    /// Serves as the inactivity counter for delayed recovery.
    pub height: u64,           
    pub is_coinbase: bool,
    //  Removed UtxoLockState. Concurrency handled by mempool overlay.
}

/// Manages the unspent transaction output set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UtxoState {
    /// Map of all spendable assets.
    pub unspent_outputs: HashMap<OutPoint, UtxoRecord>,
    //  Q-SigCache to prevent ML-DSA-65 CPU exhaustion attacks.
    pub verified_tx_cache: HashSet<[u8; 32]>,
}

impl UtxoState {
    /// Initializes an empty UTXO state.
    pub fn new() -> Self {
        Self { 
            unspent_outputs: HashMap::new(),
            verified_tx_cache: HashSet::new(),
        }
    }

    /// Disconnects a block and restores the previous state.
    pub fn disconnect_block(&mut self, undo_log: &UtxoUndoLog) -> Result<(), &'static str> {
        // Restore spent UTXOs before removing newly created ones to preserve dependency topological order.

        // 1. Un-spend: Restore previously spent UTXOs first.
        let mut resurrected_count = 0;
        for (outpoint, record) in &undo_log.spent_utxos {
            self.unspent_outputs.insert(outpoint.clone(), record.clone());
            resurrected_count += 1;
        }
        if resurrected_count > 0 {
            println!("[INFO] UTXO: Rollback restored {} previously spent UTXOs.", resurrected_count);
        }

        // 2. Un-create: Remove newly created UTXOs.
        let mut obliterated_count = 0;
        for outpoint in &undo_log.newly_created_outpoints {
            if self.unspent_outputs.remove(outpoint).is_some() {
                obliterated_count += 1;
            }
        }
        if obliterated_count > 0 {
            println!("[INFO] UTXO: Rollback removed {} newly created UTXOs.", obliterated_count);
        }

        Ok(())
    }

    // -------------------------------------------------------------------------
    // Lock Management
    // -------------------------------------------------------------------------
    //  Lock Management removed. Physical row-level locks are deprecated
    // in favor of the stateless Mempool Overlay architecture.

    // -------------------------------------------------------------------------
    // Balance Calculation
    // -------------------------------------------------------------------------

    /// Account for pending transactions in the mempool to provide accurate balance projection.
    pub fn get_balance(&self, pubkey_hash: &[u8; 32], current_height: u64, pending_txs: &[Transaction]) -> (u64, u64, u64) {
        let mut mature = 0;
        let mut locked = 0;
        let mut pending_change = 0;
        
        let mut projected_utxos = self.unspent_outputs.clone();
        
        // 1. Remove assets that are being spent in the current mempool.
        for tx in pending_txs {
            for input in &tx.inputs {
                projected_utxos.remove(&OutPoint { 
                    tx_hash: input.previous_output_hash, 
                    vout: input.vout 
                });
            }
            // 2. Isolate pending mempool funds for security verification.
            for output in &tx.outputs {
                if output.public_key_hash == *pubkey_hash {
                    pending_change += output.value;
                }
            }
        }
        
        for record in projected_utxos.values() {
            if record.output.public_key_hash == *pubkey_hash {
                if record.is_coinbase && current_height < record.height + COINBASE_MATURITY {
                    locked += record.output.value;
                } else {
                    mature += record.output.value;
                }
            }
        }
        // Decouple state: (Confirmed On-Chain, Pending Mempool, Locked Coinbase)
        (mature, pending_change, locked)
    }

    // -------------------------------------------------------------------------
    // UTXO Selection
    // -------------------------------------------------------------------------

    /// Selects spendable UTXOs prioritizing largest values first to minimize ML-DSA-65 signature bloat.
    /// Employs deterministic sorting to guarantee repeatable state transitions.
    pub fn get_spendable_utxos(
        &mut self, 
        pubkey_hash: &[u8; 32], 
        current_height: u64, 
        required_amount: u64,
        pending_txs: &[Transaction]
    ) -> Result<(Vec<(OutPoint, UtxoRecord)>, u64), &'static str> {
        
        //  Virtual projection (Mempool Overlay) eliminates need for physical locks.
        let mut available_utxos = self.unspent_outputs.clone();
        for tx in pending_txs {
            for input in &tx.inputs {
                available_utxos.remove(&OutPoint { tx_hash: input.previous_output_hash, vout: input.vout });
            }
        }

        // Extract all valid, mature UTXOs belonging to the requested pubkey.
        let mut valid_utxos: Vec<(OutPoint, UtxoRecord)> = available_utxos
            .into_iter()
            .filter(|(_, record)| {
                if record.output.public_key_hash != *pubkey_hash {
                    return false;
                }
                if record.is_coinbase && current_height < record.height + COINBASE_MATURITY {
                    return false;
                }
                true
            })
            .collect();

        // Apply Largest-First coin selection strategy.
        // Tie-breaker uses tx_hash to ensure absolute deterministic selection across all nodes.
        valid_utxos.sort_by(|a, b| {
            b.1.output.value.cmp(&a.1.output.value)
                .then_with(|| a.0.tx_hash.cmp(&b.0.tx_hash))
        });

        let mut gathered_amount = 0;
        let mut selected = Vec::new();

        for (outpoint, record) in valid_utxos {
            gathered_amount += record.output.value;
            selected.push((outpoint, record));

            if gathered_amount >= required_amount { 
                break; 
            }
        }

        if gathered_amount < required_amount {
            return Err("Insufficient funds.");
        }

        Ok((selected, gathered_amount))
    }

    // -------------------------------------------------------------------------
    // Consensus Validation
    // -------------------------------------------------------------------------

    /// Validates a transaction against the ledger.
    /// Implements witness separation and delayed recovery logic.
    pub fn validate_transaction(&self, tx: &Transaction, current_height: u64, is_historical: bool) -> Result<u64, &'static str> {
        // Differentiate between strict mempool validation and historical AssumeValid.
        // Reserved hook for Layer-2 validation routing.
        if tx.inputs.is_empty() && !tx.outputs.is_empty() && tx.outputs[0].value == 0 {
            println!("[INFO] Consensus: Layer-2 validation hook triggered.");
            return Err("Layer-2 verification requires upgraded node implementation.");
        }

        //  Dust Limit check completely stripped from Consensus Rule.
        // Relocated to network Relay Policy to prevent Hard Forks.

        let mut input_sum = 0;
        // Use the SegWit-safe TXID (Excludes Witnesses) for the message hash.
        let tx_core_hash = tx.calculate_id();
        
        //  Intra-transaction double-spend tracker for Mempool.
        let mut tx_spent_tracker = std::collections::HashSet::new();

        for (i, input) in tx.inputs.iter().enumerate() {
            let outpoint = OutPoint { tx_hash: input.previous_output_hash, vout: input.vout };
            
            if !tx_spent_tracker.insert(outpoint.clone()) {
                return Err("Consensus Violation: Duplicate inputs detected within the same transaction.");
            }

            let record = self.unspent_outputs.get(&outpoint)
                .ok_or("Attempted to spend non-existent UTXO.")?;

            if record.is_coinbase {
                let maturity_height = record.height + COINBASE_MATURITY;
                if current_height < maturity_height {
                    return Err("Consensus Violation: Attempted to spend immature coinbase UTXO.");
                }
            }

            //  Q-SigCache optimization. Bypasses ML-DSA-65 matrices if verified in mempool.
            let is_primary = if tx.witnesses.is_empty() {
                if is_historical { true } else { return Err("Consensus Violation: Missing witness."); }
            } else if self.verified_tx_cache.contains(&tx_core_hash) {
                true
            } else {
                tx.verify_witness(i, &tx_core_hash)
            };
            
            if !is_primary {
                // Check if delayed recovery logic is triggered.
                if let Some(recovery) = &record.output.recovery {
                    // Logic: Only activate if (Creation Height + Delay + 30-day Window) < Current.
                    let activation_height = record.height + recovery.recovery_delay + RECOVERY_CHALLENGE_WINDOW;
                    
                    if current_height < activation_height {
                        return Err("Recovery claim is still within the challenge window.");
                    }

                    // Verify recovery hash reveal in the witness.
                    if !self.verify_recovery_reveal(tx, i, &recovery.recovery_hash) {
                        return Err("Invalid recovery reveal.");
                    }
                    println!("[INFO] Consensus: Recovery claim verified for UTXO at height {}.", record.height);
                } else {
                    return Err("Invalid signature and no recovery protocol defined.");
                }
            }
            input_sum += record.output.value;
        }

        let output_sum: u64 = tx.outputs.iter().map(|o| o.value).sum();
        if input_sum < output_sum { return Err("Transaction outputs exceed inputs."); }
        // Calculate and return the implicit transaction fee.
        Ok(input_sum - output_sum)
    }

    /// Validates recovery hash reveal against the on-chain recovery data.
    fn verify_recovery_reveal(&self, tx: &Transaction, input_idx: usize, target_hash: &[u8; 32]) -> bool {
        // 1. Verify witness existence
        let witness = match tx.witnesses.get(input_idx) {
            Some(w) => w,
            None => {
                println!("[WARN] Security: Missing witness for recovery attempt.");
                return false;
            }
        };

        // 2. Block theft vector (Prevent public key substitution attack)
        // Hash the provided public key via SHA-256 and enforce absolute equality with the on-chain target_hash commitment.
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&witness.public_key);
        let computed_hash: [u8; 32] = hasher.finalize().into();

        if computed_hash != *target_hash {
            println!("[WARN] Security: Recovery hash mismatch! Blocked potential theft attempt.");
            return false;
        }

        // 3. Cryptographic proof of ownership (ML-DSA-65 signature verification)
        // Even if the attacker spoofs the public key hash, they must prove ownership via the backup post-quantum private key.
        let tx_core_hash = tx.calculate_id();
        crate::crypto::ml_dsa::verify_signature(&tx_core_hash, &witness.signature, &witness.public_key)
    }

    // -------------------------------------------------------------------------
    // State Mutation
    // -------------------------------------------------------------------------

    /// Mutates the UTXO state based on a validated block and generates an undo log.
    pub fn process_block(&mut self, block: &Block, height: u64, is_historical: bool) -> Result<UtxoUndoLog, &'static str> {
        if block.transactions.is_empty() {
            return Err("Consensus Violation: Block contains no transactions.");
        }
        let mut undo_log = UtxoUndoLog::new();
        let mut total_fees = 0u64;

        // Phase 1 - Pre-flight Resolution & Virtual UTXO Routing
        // Solves intra-block dependencies (Child-Pays-For-Parent) and prevents CPU exhaustion.
        let mut virtual_utxo_cache: HashMap<OutPoint, UtxoRecord> = HashMap::new();
        let mut signature_tasks = Vec::new();
        //  Intra-block double-spend tracker.
        let mut in_block_spent_tracker: std::collections::HashSet<OutPoint> = std::collections::HashSet::new();

        for (index, tx) in block.transactions.iter().enumerate() {
            let is_null_input = tx.inputs.len() == 1 && tx.inputs[0].previous_output_hash == [0u8; 32];
            let is_coinbase = index == 0;
            let tx_core_hash = tx.calculate_id();

            if is_coinbase {
                if !is_null_input { return Err("Block index 0 is not a valid coinbase transaction."); }
            } else {
                if is_null_input { return Err("Coinbase transaction detected at index > 0."); }
                
                if tx.inputs.is_empty() && !tx.outputs.is_empty() && tx.outputs[0].value == 0 {
                    return Err("Layer-2 verification requires upgraded node implementation.");
                }

                let mut input_sum = 0;
                for (i, input) in tx.inputs.iter().enumerate() {
                    let op = OutPoint { tx_hash: input.previous_output_hash, vout: input.vout };
                    
                    //  Prevent malicious intra-block double spending (Money Printer bug).
                    if !in_block_spent_tracker.insert(op.clone()) {
                        return Err("Consensus Violation: Double spend detected within the same block.");
                    }

                    // O(1) Virtual Routing: Check historical state FIRST, then check intra-block cache.
                    let record = if let Some(rec) = self.unspent_outputs.get(&op) {
                        rec.clone()
                    } else if let Some(rec) = virtual_utxo_cache.get(&op) {
                        rec.clone()
                    } else {
                        return Err("Attempted to spend non-existent UTXO.");
                    };

                    if record.is_coinbase && height < record.height + COINBASE_MATURITY {
                        return Err("Consensus Violation: Attempted to spend immature coinbase UTXO.");
                    }
                    
                    // Queue for Phase 2 parallel computation
                    signature_tasks.push((tx, i, tx_core_hash, record.clone()));
                    input_sum += record.output.value;
                }

                let output_sum: u64 = tx.outputs.iter().map(|o| o.value).sum();
                if input_sum < output_sum { return Err("Transaction outputs exceed inputs."); }
                total_fees += input_sum - output_sum;
            }

            // Populate virtual cache for subsequent transactions in the SAME block
            for (out_idx, output) in tx.outputs.iter().enumerate() {
                let new_op = OutPoint { tx_hash: tx_core_hash, vout: out_idx as u32 };
                virtual_utxo_cache.insert(new_op, UtxoRecord {
                    output: output.clone(),
                    height,
                    is_coinbase,
                });
            }
        }

        // Pre-flight coinbase validation.
        // Ensures atomic state mutation by rejecting invalid blocks prior to Phase 2/3 execution.
        let coinbase_output_sum: u64 = block.transactions[0].outputs.iter().map(|o| o.value).sum();
        let expected_max = crate::economics::CentralBank::get_block_reward(height) + total_fees;
        if coinbase_output_sum > expected_max {
            return Err("Consensus Violation: Coinbase output value exceeds block reward plus fees.");
        }

        // Phase 2 - Absolute Concurrency (Rayon Par-Iter)
        // Executes heavy ML-DSA-65 matrices on all CPU cores simultaneously.
        use rayon::prelude::*;
        let all_signatures_valid = signature_tasks.par_iter().all(|(tx, i, tx_core_hash, record)| {
            if tx.witnesses.is_empty() {
                is_historical
            } else if self.verified_tx_cache.contains(tx_core_hash) {
                true
            } else {
                let is_primary = tx.verify_witness(*i, tx_core_hash);
                if !is_primary {
                    if let Some(recovery) = &record.output.recovery {
                        let activation_height = record.height + recovery.recovery_delay + RECOVERY_CHALLENGE_WINDOW;
                        if height < activation_height { return false; }
                        return self.verify_recovery_reveal(tx, *i, &recovery.recovery_hash);
                    }
                    return false;
                }
                true
            }
        });

        if !all_signatures_valid {
            return Err("Consensus Violation: Invalid ML-DSA-65 signature detected in block.");
        }

        // Phase 3 - Atomic Commit
        // All cryptographic proofs passed. State is now deterministically mutated.
        for (index, tx) in block.transactions.iter().enumerate() {
            let tx_hash = tx.calculate_id();
            
            if index > 0 {
                for input in &tx.inputs {
                    let op = OutPoint { tx_hash: input.previous_output_hash, vout: input.vout };
                    if let Some(dead_utxo) = self.unspent_outputs.remove(&op) {
                        undo_log.spent_utxos.push((op, dead_utxo));
                    }
                }
            }

            for (out_idx, output) in tx.outputs.iter().enumerate() {
                let new_op = OutPoint { tx_hash, vout: out_idx as u32 };
                self.unspent_outputs.insert(new_op.clone(), UtxoRecord {
                    output: output.clone(),
                    height,
                    is_coinbase: index == 0,
                });
                undo_log.newly_created_outpoints.push(new_op);
            }
            
            self.verified_tx_cache.remove(&tx_hash);
        }

        Ok(undo_log)
    }
}

// =============================================================================
// UTXO Actor Model (V1.2 Architecture)
// Isolated state machine to prevent concurrency deadlocks.
// Communicates strictly via channel messages. No shared mutex memory.
// =============================================================================

use tokio::sync::{mpsc, oneshot};

/// Commands sent to the UtxoActor.
pub enum UtxoCommand {
    /// Validates a real-time transaction for the Mempool. Strict signature check.
    ValidateMempoolTx {
        tx: Transaction,
        current_height: u64,
        crypto_pre_verified: bool, //  Offload heavy crypto verification
        //  Returns exact fee to enable strict relay policy in main.rs
        resp: oneshot::Sender<Result<u64, &'static str>>,
    },
    /// Validates and applies a block to the UTXO state.
    ApplyBlock {
        block: Block,
        height: u64,
        is_historical: bool,
        resp: oneshot::Sender<Result<UtxoUndoLog, &'static str>>,
    },
    /// Reverts a block's effects using the provided undo log.
    DisconnectBlock {
        undo_log: UtxoUndoLog,
        resp: oneshot::Sender<Result<(), &'static str>>,
    },
    /// Selects spendable UTXOs and locks them.
    GetSpendable {
        pubkey_hash: [u8; 32],
        current_height: u64,
        required_amount: u64,
        pending_txs: Vec<Transaction>,
        resp: oneshot::Sender<Result<(Vec<(OutPoint, UtxoRecord)>, u64), &'static str>>,
    },
    /// Calculates current balance including pending mempool transactions.
    GetBalance {
        pubkey_hash: [u8; 32],
        current_height: u64,
        pending_txs: Vec<Transaction>,
        resp: oneshot::Sender<(u64, u64, u64)>,
    },
    /// Extracts a deep clone of the current state snapshot for disk persistence.
    GetSnapshot {
        resp: oneshot::Sender<UtxoState>,
    },
    //  Topology-Aware Async Snapshot Reconciliation
    ReconcileMempool {
        snapshot: Vec<([u8; 32], Vec<TxIn>, Vec<TxOut>)>,
        resp: oneshot::Sender<Vec<[u8; 32]>>,
    },
}

/// The isolated UTXO processor.
pub struct UtxoActor {
    state: UtxoState,
    receiver: mpsc::Receiver<UtxoCommand>,
}

impl UtxoActor {
    pub fn new(state: UtxoState, receiver: mpsc::Receiver<UtxoCommand>) -> Self {
        Self { state, receiver }
    }

    /// Spawns the Actor loop on a dedicated blocking thread to prevent Tokio starvation.
    pub fn run(mut self) {
        std::thread::spawn(move || {
            while let Some(cmd) = self.receiver.blocking_recv() {
                match cmd {
                    UtxoCommand::ValidateMempoolTx { tx, current_height, crypto_pre_verified, resp } => {
                        let tx_hash = tx.calculate_id();
                        let mut temporary_cache = false;
                        
                        //  Inject into cache if pre-verified to bypass ML-DSA-65 in state validation
                        if crypto_pre_verified && !self.state.verified_tx_cache.contains(&tx_hash) {
                            self.state.verified_tx_cache.insert(tx_hash);
                            temporary_cache = true;
                        }

                        let validation_result = self.state.validate_transaction(&tx, current_height, false);
                        
                        if validation_result.is_ok() {
                            //  Q-SigCache stores validated TXID to bypass future block-level ML-DSA-65 checks.
                            if self.state.verified_tx_cache.len() > 100_000 {
                                self.state.verified_tx_cache.clear(); // OOM Protection
                            }
                            self.state.verified_tx_cache.insert(tx_hash);
                        } else if temporary_cache {
                            self.state.verified_tx_cache.remove(&tx_hash); // Rollback cache on UTXO failure
                        }
                        //  Pass the exact fee directly back to main.rs
                        let _ = resp.send(validation_result);
                    }
                    UtxoCommand::ApplyBlock { block, height, is_historical, resp } => {
                        let _ = resp.send(self.state.process_block(&block, height, is_historical));
                    }
                    UtxoCommand::DisconnectBlock { undo_log, resp } => {
                        let _ = resp.send(self.state.disconnect_block(&undo_log));
                    }
                    UtxoCommand::GetSpendable { pubkey_hash, current_height, required_amount, pending_txs, resp } => {
                        let _ = resp.send(self.state.get_spendable_utxos(&pubkey_hash, current_height, required_amount, &pending_txs));
                    }
                    UtxoCommand::GetBalance { pubkey_hash, current_height, pending_txs, resp } => {
                        let _ = resp.send(self.state.get_balance(&pubkey_hash, current_height, &pending_txs));
                    }
                    UtxoCommand::GetSnapshot { resp } => {
                        let _ = resp.send(self.state.clone());
                    }
                    UtxoCommand::ReconcileMempool { snapshot, resp } => {
                        // Memory sandbox for topological inference
                        let mut virtual_utxo_cache: HashSet<OutPoint> = HashSet::new();
                        let mut blacklist = Vec::new();

                        for (tx_id, inputs, outputs) in snapshot {
                            let mut is_ghost = false;
                            
                            for input in &inputs {
                                let op = OutPoint { tx_hash: input.previous_output_hash, vout: input.vout };
                                // Validate against physical state and ephemeral topological state
                                if !self.state.unspent_outputs.contains_key(&op) && !virtual_utxo_cache.contains(&op) {
                                    is_ghost = true;
                                    break;
                                }
                            }

                            if is_ghost {
                                blacklist.push(tx_id);
                            } else {
                                // Transaction is contextually valid. Project outputs into sandbox for chained tx resolution.
                                for (vout, _) in outputs.iter().enumerate() {
                                    virtual_utxo_cache.insert(OutPoint { tx_hash: tx_id, vout: vout as u32 });
                                }
                            }
                        }
                        let _ = resp.send(blacklist);
                    }
                }
            }
        });
    }
}

