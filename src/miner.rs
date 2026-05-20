// src/miner.rs
use crate::block::Block;
use crate::transaction::Transaction;
use crate::consensus::ConsensusEngine;
use std::sync::atomic::{AtomicBool, Ordering};
use crossbeam_channel::{Receiver, Sender};
use std::time::{Instant, Duration};

// Global operational flags for Tier 0 isolation protocol.
pub static MUTE_CONSOLE_LOGS: AtomicBool = AtomicBool::new(false);
pub static IS_SIGNING: AtomicBool = AtomicBool::new(false);
// Lock-free command pending flag.
pub static PENDING_CMD: AtomicBool = AtomicBool::new(false);

// Data packet encapsulating all necessary parameters for mining a block.
pub struct BlockTemplate {
    pub previous_hash: [u8; 32],
    pub transactions: Vec<Transaction>,
    pub target: u64,
    pub current_height: u64,
}

// Communication protocol between Tokio main thread and Daemon.
pub enum MinerCommand {
    Mine(BlockTemplate),
    Stop,
    // CORE-V2: Absolute termination signal for the isolated thread.
    Shutdown,
}

macro_rules! miner_log {
    ($($arg:tt)*) => {
        if !MUTE_CONSOLE_LOGS.load(Ordering::Relaxed) {
            tracing::info!($($arg)*);
        }
    };
}

pub struct Miner;

impl Miner {
    
    // Initiates the isolated Proof-of-Work daemon thread.
    // Utilizes sentinel flags and channel passing for 0-latency interruptions.
    pub fn start_daemon(
        cmd_rx: Receiver<MinerCommand>,
        result_tx: Sender<Block>,
    ) {
        std::thread::spawn(move || {
            let mut active_block: Option<Block> = None;
            let mut current_target: u64 = 0;
            let mut nonce: u64 = 0;

            // Time anchor for network heartbeat (Satoshi solo-mining architecture).
            let mut last_sleep_time = Instant::now();

            loop {
                // Lock-free channel polling.
                // Bypasses the deprecated PENDING_CMD for nanosecond response times.
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        MinerCommand::Mine(template) => {
                            // Delegate dual-magazine Merkle root calculations to the Block constructor.
                            // Ensures absolute consensus alignment and eliminates redundant miner-side hashing.
                            let new_block = Block::new(
                                template.previous_hash,
                                template.transactions.clone(),
                                template.target,
                                0,
                            );
                            
                            active_block = Some(new_block);
                            current_target = template.target;
                            nonce = 0;
                            
                            miner_log!("[INFO] Miner: Starting Proof-of-Work for block height {}.", template.current_height);
                            miner_log!("[INFO] Miner: Target threshold set to {}.", template.target);
                            miner_log!("[INFO] Miner: Engine started.");
                        }
                        MinerCommand::Stop => {
                            active_block = None;
                            miner_log!("[INFO] Miner: Interrupt signal received. Engine suspended.");
                        }
                        MinerCommand::Shutdown => {
                            miner_log!("[INFO] Miner: Shutdown signal received. Breaking compute loop.");
                            return; // CORE-V2: Instantly kills the OS thread and yields resources.
                        }
                    }
                }

                // Pause compute if wallet is signing.
                if IS_SIGNING.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(10));
                    last_sleep_time = Instant::now();
                    continue;
                }

                // Execute strided hashing if an active block exists.
                if let Some(block) = active_block.as_mut() {
                    let mut local_nonce = nonce;
                    let mut puzzle_solved = false;
                    
                    // Stride size: Optimizes CPU pipeline while bounding execution time.
                    const CHUNK_SIZE: u64 = 65536;

                    // FIX: Relocate syscall outside the hot loop. Step timestamp per chunk to prevent CPU starvation.
                    block.header.timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

                    for _ in 0..CHUNK_SIZE {
                        block.header.nonce = local_nonce;

                        if ConsensusEngine::verify_proof_of_work(block, current_target) {
                            puzzle_solved = true;
                            break;
                        }
                        // Cryptographic safety: Prevent u64 overflow panics.
                        local_nonce = local_nonce.wrapping_add(1);
                    }

                    nonce = local_nonce;

                    if puzzle_solved {
                        let hash = block.calculate_hash();
                        let mut prefix_bytes = [0u8; 8];
                        prefix_bytes.copy_from_slice(&hash[0..8]);
                        let hash_prefix = u64::from_be_bytes(prefix_bytes);

                        miner_log!("[INFO] Miner: Proof-of-Work puzzle solved.");
                        miner_log!("[INFO] Miner: Winning nonce: {}", nonce);
                        miner_log!("[INFO] Miner: Hash prefix {} is less than or equal to target {}.", hash_prefix, current_target);
                        
                        // Output absolute cryptographic hex hash for mainnet checkpoints.
                        let hex_hash: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
                        miner_log!("[INFO] Miner: Absolute Block Hash: 0x{}", hex_hash);
                        
                        let _ = result_tx.send(block.clone());
                        active_block = None;
                        last_sleep_time = Instant::now();
                    } else {
                        // Deterministic OS Yield (50ms compute / 1ms sleep).
                        // Guarantees P2P network threads are never starved by the hashing loop.
                        if last_sleep_time.elapsed() >= Duration::from_millis(50) {
                            std::thread::sleep(Duration::from_millis(1));
                            last_sleep_time = Instant::now();
                        }
                    }

                    if nonce % 5_000_000 < CHUNK_SIZE {
                        miner_log!("[INFO] Miner: {} million hashes calculated.", nonce / 1_000_000);
                    }
                } else {
                    // Deep sleep when idle to conserve system resources.
                    std::thread::sleep(Duration::from_millis(10));
                    last_sleep_time = Instant::now();
                }
            }
        });
    }
}