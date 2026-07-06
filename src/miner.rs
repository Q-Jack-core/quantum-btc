// src/miner.rs
use crate::block::Block;
use crate::transaction::Transaction;
use crate::consensus::ConsensusEngine;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use crossbeam_channel::{Receiver, Sender};
use std::time::{Instant, Duration};
use num_cpus;

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
    
    // Initiates the dynamic multi-core Proof-of-Work daemon thread pool.
    // Utilizes a Coordinator-Worker architecture with absolute mathematical isolation.
    pub fn start_daemon(
        cmd_rx: Receiver<MinerCommand>,
        result_tx: Sender<Block>,
    ) {
        // CORE-V2: Shared memory blackboard for multi-core synchronization
        let shared_template = Arc::new(RwLock::new(None::<BlockTemplate>));
        let is_mining = Arc::new(AtomicBool::new(false));
        let template_version = Arc::new(AtomicU64::new(0)); 
        
        let template_writer = shared_template.clone();
        let mining_flag = is_mining.clone();
        let version_writer = template_version.clone();

        // 1. Coordinator Thread: Broadcasts network commands to all CPU cores
        std::thread::spawn(move || {
            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    MinerCommand::Mine(template) => {
                        miner_log!("[INFO] Miner [Coordinator]: 📡 Broadcasting real task for height {} to CPU cores!", template.current_height);
                        if let Ok(mut lock) = template_writer.write() {
                            *lock = Some(template);
                        }
                        version_writer.fetch_add(1, Ordering::SeqCst);
                        mining_flag.store(true, Ordering::Relaxed);
                    }
                    MinerCommand::Stop | MinerCommand::Shutdown => {
                        mining_flag.store(false, Ordering::Relaxed);
                        if let MinerCommand::Shutdown = cmd { break; }
                    }
                }
            }
        });

        // 2. Dynamic Auto-Scaling CPU Infantry Phalanx (Adapts to local hardware)
        let core_count = num_cpus::get() as u64;
        miner_log!("[INFO] Miner: Detected {} physical/logical cores. Deploying parallel hash grid...", core_count);

        for thread_id in 0..core_count {
            let template_reader = shared_template.clone();
            let mining_flag_reader = is_mining.clone();
            let version_reader = template_version.clone();
            let result_tx = result_tx.clone();

            std::thread::spawn(move || {
                let mut local_active_block: Option<Block> = None;
                let mut current_target: u64 = 0;
                
                // CORE-V2: Mathematical isolation using the optimal chunk offset algorithm (Pristine Original Logic)
                let offset: u64 = thread_id * 1_000_000_000_000_000;
                let mut nonce: u64 = offset; 
                let mut local_version: u64 = 0;
                let mut last_sleep_time = Instant::now();

                loop {
                    if !mining_flag_reader.load(Ordering::Relaxed) || IS_SIGNING.load(Ordering::Relaxed) {
                        local_active_block = None;
                        std::thread::sleep(Duration::from_millis(10));
                        last_sleep_time = Instant::now();
                        continue;
                    }

                    let current_global_version = version_reader.load(Ordering::Relaxed);
                    if current_global_version != local_version {
                        local_active_block = None; 
                        local_version = current_global_version;
                        nonce = offset; // Reset to the thread's dedicated mathematical sector
                    }

                    if local_active_block.is_none() {
                        if let Ok(guard) = template_reader.read() {
                            if let Some(template) = &*guard {
                                local_active_block = Some(Block::new(
                                    template.previous_hash,
                                    template.transactions.clone(),
                                    template.target,
                                    0,
                                ));
                                current_target = template.target;
                            }
                        }
                    }

                    if let Some(block) = local_active_block.as_mut() {
                        let mut local_nonce = nonce;
                        let mut puzzle_solved = false;
                        const CHUNK_SIZE: u64 = 65536;

                        block.header.timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

                        for _ in 0..CHUNK_SIZE {
                            block.header.nonce = local_nonce;
                            if ConsensusEngine::verify_proof_of_work(block, current_target) {
                                puzzle_solved = true;
                                break;
                            }
                            local_nonce = local_nonce.wrapping_add(1); // Linear sequential hashing within isolated sector
                        }

                        nonce = local_nonce;

                        if puzzle_solved {
                            mining_flag_reader.store(false, Ordering::Relaxed);
                            
                            let final_hash = block.calculate_hash();
                            let hex_hash: String = final_hash.iter().map(|b| format!("{:02x}", b)).collect();
                            
                            miner_log!("[INFO] Miner [Local Client Core-{}]: 💥 CPU block forged successfully!", thread_id);
                            miner_log!("[INFO] Miner: Absolute Block Hash: 0x{}", hex_hash);
                            
                            let _ = result_tx.send(block.clone());
                            local_active_block = None;
                            last_sleep_time = Instant::now();
                        } else {
                            if last_sleep_time.elapsed() >= Duration::from_millis(50) {
                                std::thread::sleep(Duration::from_millis(1));
                                last_sleep_time = Instant::now();
                            }
                        }

                        // Original pristine heartbeat logic: Only core 0 reports progress
                        if thread_id == 0 {
                            let pure_nonce = nonce - offset;
                            if pure_nonce % 10_000_000 < CHUNK_SIZE {
                                miner_log!("[INFO] Miner: {} million hashes calculated.", pure_nonce / 1_000_000);
                            }
                        }
                    }
                }
            });
        }
    }
}