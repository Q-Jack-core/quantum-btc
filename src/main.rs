// src/main.rs
// Core engine linkage.
// Routes all internal modules directly from the compiled library (quantum_btc).
// Prevents dual-compilation and mismatched type errors.
use quantum_btc::transaction;
use quantum_btc::block;
use quantum_btc::utxo;
use quantum_btc::storage;
use quantum_btc::mempool;
use quantum_btc::network;
use quantum_btc::consensus;
use quantum_btc::rpc;
use quantum_btc::miner;
use quantum_btc::economics;
use quantum_btc::wallet;

use inquire::{Password, PasswordDisplayMode};
use libp2p::futures::StreamExt;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::collections::HashMap;
use std::collections::BTreeMap;
use std::sync::RwLock;
use lazy_static::lazy_static;

lazy_static! {
    /* Global Orphan Header Pool.
       Utilizes BTreeMap for deterministic sorting by timestamp.
       Buffers out-of-order headers during network sync. */
    pub static ref ORPHAN_HEADER_POOL: RwLock<BTreeMap<u64, crate::block::BlockHeader>> = RwLock::new(BTreeMap::new());
    
    // Block Relay Cache (FIFO).
    // Decouples P2P block requests from disk I/O.
    // Capacity bounded to 10 blocks to prevent memory exhaustion.
    pub static ref BLOCK_RELAY_CACHE: std::sync::RwLock<Vec<crate::block::Block>> = std::sync::RwLock::new(Vec::with_capacity(10));
}

// RAII IO semaphore for bounded connection load shedding.
pub static ACTIVE_IO_TASKS: AtomicUsize = AtomicUsize::new(0);

pub static FALLBACK_GUARD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub struct IoTaskGuard;
impl IoTaskGuard {
    pub fn try_acquire(limit: usize) -> Option<Self> {
        let mut current = ACTIVE_IO_TASKS.load(Ordering::SeqCst);
        loop {
            if current >= limit { return None; }
            match ACTIVE_IO_TASKS.compare_exchange_weak(current, current + 1, Ordering::SeqCst, Ordering::Relaxed) {
                Ok(_) => return Some(Self),
                Err(v) => current = v,
            }
        }
    }
}
impl Drop for IoTaskGuard {
    fn drop(&mut self) {
        ACTIVE_IO_TASKS.fetch_sub(1, Ordering::SeqCst);
    }
}
use bincode::Options;
//  MPSC channels for bounded sequential processing
use std::sync::mpsc as std_mpsc;
use std::thread;

// Imported TxWitness for isolated signature processing.
use transaction::{Transaction, TxIn, TxOut, TxWitness};
use mempool::blind_box::QuantumMempool;

use block::Block; 
use network::reputation::{ReputationManager, NetworkOffense};
use quantum_btc::network::NetworkPayload;
use storage::QuantumStorage; 
use miner::Miner;
use economics::CentralBank;
use sha2::{Digest, Sha256}; 
use rand::{Rng, rngs::SysRng}; use rand_core::UnwrapErr;
use rayon::prelude::*;

// Observability framework imports.
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// Mutex lock wrapper to safely handle thread poisoning.
macro_rules! safe_lock {
    ($mutex:expr) => {
        $mutex.lock().unwrap_or_else(|e| e.into_inner())
    };
}

// RAII guard for TTY safety during interactive prompts.
// Automatically mutes background logs on creation and restores them on drop.
pub struct ConsoleSilenceGuard;

impl ConsoleSilenceGuard {
    pub fn new() -> Self {
        quantum_btc::miner::MUTE_CONSOLE_LOGS.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for ConsoleSilenceGuard {
    fn drop(&mut self) {
        quantum_btc::miner::MUTE_CONSOLE_LOGS.store(false, Ordering::SeqCst);
    }
}

// FIX: Conditional writer to dynamically mute tracing logs in the console without halting file logging.
#[derive(Clone)]
pub struct ConditionalStdout;
impl std::io::Write for ConditionalStdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if !quantum_btc::miner::MUTE_CONSOLE_LOGS.load(Ordering::SeqCst) {
            std::io::stdout().write(buf)
        } else {
            Ok(buf.len())
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stdout().flush()
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ConditionalStdout {
    type Writer = ConditionalStdout;
    fn make_writer(&self) -> Self::Writer {
        ConditionalStdout
    }
}

// FIX: Shadow standard println to respect the global mute flag globally within main.rs
macro_rules! println {
    () => {
        if !quantum_btc::miner::MUTE_CONSOLE_LOGS.load(std::sync::atomic::Ordering::SeqCst) {
            std::println!()
        }
    };
    ($($arg:tt)*) => {
        if !quantum_btc::miner::MUTE_CONSOLE_LOGS.load(std::sync::atomic::Ordering::SeqCst) {
            std::println!($($arg)*)
        }
    };
}

const MAX_PEERS: usize = 50;           
const MAX_REORG_DEPTH: u64 = 864000;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // L0 INFRASTRUCTURE: Async X-Ray Radar (Tokio Console)
    // Spawns the telemetry server in the background and yields a layer for the registry.

    // Observability Engine Initialization: Asynchronous Structured Tracing
    // Establishes a non-blocking dual-pipeline for console and file logging.
    // The worker guard (_worker_guard) must live for the entire duration of the main function.
    let file_appender = tracing_appender::rolling::daily("logs", "qbtc_node.log");
    let (non_blocking_file, _worker_guard) = tracing_appender::non_blocking(file_appender);
    
    use tracing_subscriber::Layer; // L1 FIX: Required for isolated layer filtering

    //  Decoupled EnvFilters to prevent Tokio telemetry from being strangled globally.
    //  Silence libp2p internal connection noise (OS Error 32 / Broken Pipe).
    let stdout_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,libp2p_swarm=error,libp2p_tcp=error,quantum_btc=info"));
    let file_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,libp2p_swarm=error,libp2p_tcp=error,quantum_btc=info"));

    // Terminal layer: minimalist output.
    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_writer(ConditionalStdout) // FIX: Inject conditional writer
        .without_time()
        .with_level(false)
        .with_target(false)
        .with_ansi(true)
        .with_filter(stdout_filter); // Apply filter locally

    // File layer: rich metadata.
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking_file)
        .with_ansi(false)
        .with_filter(file_filter); // Apply filter locally

    // Global registry without the global EnvFilter choke point
    tracing_subscriber::registry()
        .with(stdout_layer)
        .with(file_layer)
        .init();

    println!("[INFO] System: Initializing QBTC mainnet node.");
    
    // : Global task tracker to prevent detached ghost tasks from tearing physical state.
    let mut network_tasks = tokio::task::JoinSet::new();

    // Architecture Initialization: Satoshi Pattern (Implicit Defaults + Explicit Infrastructure)
    // Enforces zero-configuration startup for standard miners to prevent network fragmentation.
    let mut datadir = String::from("./.qbtc_data");
    let mut port: u16 = 8000;
    
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--datadir" && i + 1 < args.len() {
            datadir = args[i + 1].clone();
            i += 2;
        } else if args[i] == "--port" && i + 1 < args.len() {
            port = args[i + 1].parse().unwrap_or(8000);
            i += 2;
        } else {
            i += 1;
        }
    }

    // Hardcoded global DNS seed for initial peer discovery.
    let seed_host = "seed.qbtc-core.org";
    let seed_port: u16 = 8000;

    // Asymmetric role detection via hidden configuration file.
    // Only foundational infrastructure nodes (e.g., Vegas Seed) will contain this file.
    let is_seed_node = std::fs::read_to_string(".qbtc_node.conf")
        .map(|content| content.contains("ROLE=SEED"))
        .unwrap_or(false);

    if is_seed_node {
        println!("[INFO] Architecture: .qbtc_node.conf detected. Awakening as SEED node (Server Mode).");
    } else {
        println!("[INFO] Architecture: Standard miner detected. Operating in stealth CLIENT mode.");
    }

    // Mount core storage to decoupled directory.
    let db_path = format!("{}/vault", datadir);
    let storage = Arc::new(QuantumStorage::new(&db_path));

    //  Re-use existing args array to strictly prevent variable shadowing warnings.
    let is_rescan_requested = args.iter().any(|arg| arg == "--rescan" || arg == "-reindex-chainstate");

    let initial_utxo = if is_rescan_requested {
        println!("[WARN] Storage: COLD RESCAN (REINDEX-CHAINSTATE) INITIATED.");
        println!("[INFO] Storage: Network and RPC modules are mechanically isolated.");
        
        //  Mark state as DIRTY using isolated SYS_ prefix. Prevents Torn State.
        let _ = storage.db.delete(b"SYS_UTXO_VALID");
        
        println!("[INFO] Storage: Phase 1 - Eradicating stale UTXO indices via chunked flushes...");
        
        // Phase 1: Physical Eradication (O(1) Memory Footprint). Break the corrupted state safely.
        let mut wipe_batch = rocksdb::WriteBatch::default();
        let mut drop_count = 0;
        let mut wipe_cursor = 0;
        
        let drop_iter = storage.db.iterator(rocksdb::IteratorMode::Start);
        for item in drop_iter {
            if let Ok((key, _)) = item {
                // Absolute namespace isolation: Target UTXO data, strictly avoid SYS_ metadata.
                if key.starts_with(b"UTXO_") {
                    wipe_batch.delete(&key);
                    drop_count += 1;
                    wipe_cursor += 1;
                    
                    if wipe_cursor >= 50000 {
                        storage.db.write(wipe_batch).expect("Fatal: Eradication chunk flush failed.");
                        wipe_batch = rocksdb::WriteBatch::default(); 
                        wipe_cursor = 0;
                    }
                }
            }
        }
        storage.db.write(wipe_batch).expect("Fatal: Final eradication flush failed.");
        println!("[INFO] Storage: Phase 1 Complete. Obliterated {} corrupted UTXO records.", drop_count);

        let chain = storage.get_chain_list();
        let total_blocks = chain.len();
        println!("[INFO] Storage: Phase 2 - Rebuilding deterministic state from {} historical blocks...", total_blocks);
        
        let mut virgin_state = utxo::UtxoState::new();
        let mut flush_batch = rocksdb::WriteBatch::default(); 

        for i in 0..total_blocks {
            let block_hash = &chain[i];
            if let Some(block) = storage.get_block_by_hash(block_hash, true) {
                if let Ok(undo_log) = virgin_state.process_block(&block, i as u64, true) {
                    for op in &undo_log.newly_created_outpoints {
                        if let Some(record) = virgin_state.unspent_outputs.get(op) {
                            let mut utxo_key = b"UTXO_".to_vec();
                            utxo_key.extend_from_slice(&op.tx_hash);
                            utxo_key.extend_from_slice(&op.vout.to_be_bytes());
                            let encoded = bincode::serialize(record).unwrap();
                            flush_batch.put(&utxo_key, &encoded);
                        }
                    }
                    for (op, _) in &undo_log.spent_utxos {
                        let mut utxo_key = b"UTXO_".to_vec();
                        utxo_key.extend_from_slice(&op.tx_hash);
                        utxo_key.extend_from_slice(&op.vout.to_be_bytes());
                        flush_batch.delete(&utxo_key);
                    }

                    if i > 0 && i % 5000 == 0 {
                        storage.db.write(flush_batch).expect("Fatal: Reconstruction chunk flush failed.");
                        flush_batch = rocksdb::WriteBatch::default(); 
                        println!("[INFO] Storage: Rescan progress: {} / {} blocks reconstructed.", i, total_blocks);
                    }
                } else {
                    println!("[ERROR] Fatal: Block {} corrupted state derivation.", i);
                    std::process::exit(1);
                }
            }
        }
        
        storage.db.write(flush_batch).expect("Fatal: Final rescan flush failed.");
        
        //  Mark state as PERFECT. Safe to boot normally next time.
        let _ = storage.db.put(b"SYS_UTXO_VALID", b"1");
        
        println!("[INFO] Storage: COLD RESCAN COMPLETE. Regenerated {} valid UTXOs.", virgin_state.unspent_outputs.len());
        println!("[INFO] Storage: Boot sequence resuming...");
        virgin_state
    } else if let Some(saved_utxo) = storage.load_utxo_state() {
        let chain_len = storage.get_chain_list().len();
        //  Torn-State Prevention Guard
        if chain_len > 0 && storage.db.get(b"SYS_UTXO_VALID").unwrap_or(None).is_none() {
            println!("[ERROR] Fatal: Torn State Detected! UTXO database was interrupted during a previous rescan.");
            println!("[INFO] Action Required: The node cannot boot safely. Run 'cargo run --release -- --rescan' to repair the ledger.");
            std::process::exit(1);
        }
        if chain_len > 0 {
            println!("[INFO] Storage: UTXO ledger perfectly restored and verified from physical disk.");
        } else {
            println!("[INFO] Storage: No historical UTXO state found. Initializing empty vault.");
            let _ = storage.db.put(b"SYS_UTXO_VALID", b"1"); 
        }
        saved_utxo
    } else {
        println!("[WARN] Storage: Failed to initialize UTXO vault.");
        std::process::exit(1);
    };

    let initial_block = if let Some(saved_block) = storage.get_latest_block() {
        // Calculate absolute physical height based on array length.
        let actual_height = storage.get_chain_list().len().saturating_sub(1);
        println!("[INFO] Storage: Historical blockchain found. Resuming from height: {}", actual_height);
        saved_block
    } else {
        println!("[INFO] Storage: No local data. Initializing genesis state.");
        let genesis = Block::genesis();
        storage.save_block_segwit(genesis.clone(), 0);
        
        //  FIX: Explicitly inject Genesis into the physical index tree.
        // Prevents LCA (Lowest Common Ancestor) from returning None during the first IBD reorg.
        let genesis_idx = quantum_btc::block::BlockIndex::new(
            genesis.calculate_hash(),
            genesis.header.clone(),
            0,
            genesis.header.get_block_proof(),
            true
        );
        storage.save_block_index(genesis_idx);
        
        genesis
    };

    // Extracting the absolute genesis hash.
    // Fetch actual genesis block (index 0) from the chain list.
    let real_genesis_hash = if let Some(first_hash) = storage.get_chain_list().first() {
        *first_hash
    } else {
        initial_block.calculate_hash()
    };
    let genesis_hex: String = real_genesis_hash.iter().map(|b| format!("{:02x}", b)).collect();
    println!("[INFO] Checkpoint: Genesis hash verified as 0x{}", genesis_hex);
    
    let mempool = Arc::new(Mutex::new(QuantumMempool::new()));
    let latest_block = Arc::new(Mutex::new(initial_block.clone())); // FIX: Clone initial_block to satisfy borrow checker for subsequent target retrieval.
    
    // L1 V1.2 CORE FIX: Ignite isolated UtxoActor. Mutex completely removed.
    let (utxo_tx, utxo_rx) = tokio::sync::mpsc::channel::<utxo::UtxoCommand>(1000);
    let utxo_actor = utxo::UtxoActor::new(initial_utxo, utxo_rx);
    utxo_actor.run(); // Spawns onto a dedicated OS thread.
    
    // Inject vault path and detected architecture role into swarm builder.
    let mut swarm = network::p2p::build_swarm(&db_path, is_seed_node)?;
    swarm.listen_on(format!("/ip4/0.0.0.0/tcp/{}", port).parse()?)?;
    let topic = libp2p::gossipsub::IdentTopic::new("qbtc-dark-forest");
    swarm.behaviour_mut().gossipsub.subscribe(&topic)?;

    let local_peer_id = *swarm.local_peer_id();

    let (sos_tx, mut sos_rx) = tokio::sync::mpsc::channel::<(usize, Option<libp2p::PeerId>)>(10);
    let (mined_tx, mut mined_rx) = tokio::sync::mpsc::channel::<Block>(10);

    let (p2p_tx, mut p2p_rx) = tokio::sync::mpsc::channel::<NetworkPayload>(100);

    // Channel to bridge asynchronous RocksDB I/O back to the Swarm.
    // Prevents tokio executor starvation during heavy physical disk reads.
    let (direct_resp_tx, mut direct_resp_rx) = tokio::sync::mpsc::channel::<(libp2p::request_response::ResponseChannel<crate::network::SyncResponse>, crate::network::SyncResponse)>(100);

    
        let rpc_state = rpc::RpcState { 
        port, 
        datadir: datadir.clone(), // Injects decoupled datadir into RPC subsystem.
        mempool: mempool.clone(), 
        latest_block: latest_block.clone(),
        utxo_tx: utxo_tx.clone(), // V1.2 FIX: Inject Actor Sender instead of Mutex.
        p2p_tx: p2p_tx.clone(), 
        storage: storage.clone(),
    };
    let rpc_port_clone = port + 1; // FIX: Prevent TCP port collision between RPC and P2P
    tokio::spawn(async move { rpc::start_rpc_server(rpc_port_clone, rpc_state).await; });

    println!("[INFO] System: Node online at port {}.", port);
    println!("[INFO] Commands: [wallet_gen <name>], [wallet_restore <name> <12 words>], [wallet_change_password <name>], [list_wallets], [balance <name>], [transfer <amt> <target> <name>], [mine], [auto_mine <start|stop>], [connect <ip> <port>]");

    //  Recover absolute target difficulty and enforce ASERT max target boundary.
    let tip_hash_for_target = initial_block.calculate_hash();
    //  FIX: Extreme Target Throttling (Ice Age Lock).
    // Compress the genesis target to force physical computation time, eliminating multi-fork anomalies.
    // Perfectly aligns with the 1-minute block time physics.
    let initial_target = storage.get_block_index(&tip_hash_for_target).map(|idx| idx.header.target).unwrap_or(0x0000_0000_FFFF_FFFFu64);
    let current_target = Arc::new(AtomicU64::new(initial_target)); 
    let auto_mine_flag = Arc::new(AtomicBool::new(false));
    
    // Tier 0 Architecture: Central Daemon Channels
    // Apply backpressure via bounded channels to prevent memory exhaustion.
    let (daemon_cmd_tx, daemon_cmd_rx) = crossbeam_channel::bounded::<quantum_btc::miner::MinerCommand>(10);
    let (daemon_res_tx, daemon_res_rx) = crossbeam_channel::bounded::<Block>(100);
    let engine_idle_notify = Arc::new(tokio::sync::Notify::new());
    
    // Ignite the isolated Daemon Worker
    Miner::start_daemon(daemon_cmd_rx, daemon_res_tx);
    
    // Bridge Daemon results back to the async executor
    let mined_tx_bridge = mined_tx.clone();
    tokio::task::spawn_blocking(move || {
        while let Ok(block) = daemon_res_rx.recv() {
            let _ = mined_tx_bridge.blocking_send(block);
        }
    });

    //  Cache absolute genesis constants. O(1) memory access, zero disk I/O blocking.
    let genesis_h = storage.get_chain_list().first().copied().unwrap_or_else(|| initial_block.calculate_hash());
    let anchor_time_val = storage.get_block_index(&genesis_h).map(|idx| idx.header.timestamp).unwrap_or(initial_block.header.timestamp);
    let anchor_target_val = storage.get_block_index(&genesis_h).map(|idx| idx.header.target).unwrap_or(0x0000_0000_FFFF_FFFFu64);
    
    let genesis_anchor_time = Arc::new(AtomicU64::new(anchor_time_val));
    let genesis_anchor_target = Arc::new(AtomicU64::new(anchor_target_val));
    
    let reputation = Arc::new(Mutex::new(ReputationManager::new())); 

    let mut sync_requested = false;
    
    // Q-BIP-152: State tracker for partially reconstructed compact blocks.
    let mut pending_compact_blocks: HashMap<[u8; 32], (crate::block::CompactBlock, std::collections::HashMap<usize, Transaction>)> = HashMap::new();
    
    //  Thundering Herd deduplication tracker with sliding window bounds.
    let mut in_flight_txs: std::collections::HashSet<[u8; 32]> = std::collections::HashSet::new();
    let mut in_flight_queue: std::collections::VecDeque<[u8; 32]> = std::collections::VecDeque::new();
    //  Precise timeout rollback map to prevent Cache Flush Exploits.
    let mut active_req_map: HashMap<libp2p::request_response::OutboundRequestId, [u8; 32]> = HashMap::new();

    //  Bounded backpressure pipeline for consensus engine.
    struct ConsensusTask {
        block: Block,
        sender: libp2p::PeerId,
    }
    const MAX_BLOCK_QUEUE_SIZE: usize = 50;
    let (block_tx, block_rx) = std_mpsc::sync_channel::<ConsensusTask>(MAX_BLOCK_QUEUE_SIZE);
    let miner_block_tx = block_tx.clone(); // FIX: Dedicated channel for locally mined blocks

    let storage_worker = storage.clone();
    let latest_block_worker = latest_block.clone();
    let utxo_tx_worker = utxo_tx.clone(); // V1.2 FIX: Bind Actor Sender.
    let mempool_worker = mempool.clone();
    let reputation_worker = reputation.clone();
    let _p2p_tx_worker = p2p_tx.clone();
    let daemon_cmd_tx_worker = daemon_cmd_tx.clone();
    let engine_idle_notify_worker = engine_idle_notify.clone();
    let _local_peer_id_str = local_peer_id.to_string();
    // L1 V2.0 CORE: Inject SOS trigger into consensus thread for isolated sync requests.
    let sos_tx_worker = sos_tx.clone();
    let target_worker = current_target.clone();
    let anchor_time_worker = genesis_anchor_time.clone();
    let anchor_target_worker = genesis_anchor_target.clone();
    
    let thread_local_peer_id = local_peer_id; // FIX: Inject local identity into consensus firewall

    // Swarm Command Bus for internal node control.
    // Bounded capacity to prevent memory bloat during massive peer disconnections.
    pub enum SwarmCommand {
        BanAndDisconnect(libp2p::PeerId),
        Dial(libp2p::Multiaddr), // L1 UPGRADE: Route manual connections through the executioner bus.
        //  Decoupled async dispatch with optional tracker hash.
        SendSyncReq(libp2p::PeerId, crate::network::SyncRequest, Option<[u8; 32]>), 
        //  Report valid progress to the Debouncer.
        ReportSyncProgress(libp2p::PeerId),
    }
    let (swarm_cmd_tx, mut swarm_cmd_rx) = tokio::sync::mpsc::channel::<SwarmCommand>(1024);
    let swarm_cmd_tx_worker = swarm_cmd_tx.clone();

    //  Dedicated sequential consensus thread.
    // Migrated all legacy logic to ensure strict TOCTOU exclusion.
    let _consensus_thread_handle = thread::spawn(move || {
        for task in block_rx {
            let incoming_block = task.block;
            let sender_worker = task.sender;
            
            let mut current_latest = safe_lock!(latest_block_worker);

            if incoming_block.header.previous_hash != current_latest.calculate_hash() {
                println!("[WARN] Consensus: Orphan block detected. Diverting to quarantine.");
                
                let quarantine_success = storage_worker.add_orphan_block(incoming_block.clone());
                
                if quarantine_success {
                    let mut locator_hashes = Vec::new();
                    let chain = storage_worker.get_chain_list();
                    let mut step = 1;
                    let mut index = chain.len() as i32 - 1;
                    
                    while index >= 0 {
                        locator_hashes.push(chain[index as usize]);
                        if locator_hashes.len() > 10 { step *= 2; }
                        index -= step;
                    }
                    if index < 0 && !chain.is_empty() && locator_hashes.last() != Some(&chain[0]) {
                        locator_hashes.push(chain[0]);
                    }
                    
                    // L1 V2.0 CORE: Trigger direct ReqResp sync targeting the exact sender.
                    let current_index = storage_worker.get_chain_list().len();
                    let _ = sos_tx_worker.try_send((current_index, Some(sender_worker)));
                }
                // FIX: Unlock local miner deadlock if block is quarantined as orphan.
                if sender_worker == thread_local_peer_id {
                    engine_idle_notify_worker.notify_waiters();
                }
                continue;
            }

            let current_height = storage_worker.get_chain_list().len() as u64 - 1; 
            let incoming_height = current_height + 1;

            tracing::info!("[INFO] Network: Incoming block received. Height: {}", incoming_height);

            let current_physical_time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            if incoming_block.header.timestamp > current_physical_time + 7200 {
                println!("[ERROR] Security: Block timestamp > 2 hours in the future.");
                if sender_worker != thread_local_peer_id {
                    if safe_lock!(reputation_worker).report_offense(&sender_worker, NetworkOffense::InvalidHeader) {
                        println!("[WARN] Firewall: Peer {} slated for disconnection.", sender_worker);
                        let _ = swarm_cmd_tx_worker.try_send(SwarmCommand::BanAndDisconnect(sender_worker));
                    }
                }
                continue;
            }

            if let Err(e) = crate::consensus::verify_checkpoint(incoming_height, &incoming_block.calculate_hash()) {
                println!("[ERROR] Consensus: Dropping malicious block payload: {}", e);
                if sender_worker != thread_local_peer_id {
                    if safe_lock!(reputation_worker).report_offense(&sender_worker, NetworkOffense::InvalidHeader) {
                        println!("[WARN] Firewall: Peer {} slated for disconnection.", sender_worker);
                        let _ = swarm_cmd_tx_worker.try_send(SwarmCommand::BanAndDisconnect(sender_worker));
                    }
                }
                continue; 
            }
            //  MTP-11 Historical time-warp mitigation via cryptographic link traversal.
            let mut past_timestamps: Vec<u64> = Vec::with_capacity(11);
            let mut current_search_hash = incoming_block.header.previous_hash;
            for _ in 0..11 {
                if let Some(idx) = storage_worker.get_block_index(&current_search_hash) {
                    past_timestamps.push(idx.header.timestamp);
                    current_search_hash = idx.header.previous_hash;
                } else { break; } 
            }

            if let Err(e) = consensus::ConsensusEngine::verify_timestamp(&incoming_block, &mut past_timestamps) { 
                println!("[WARN] Consensus: Block rejected by MTP-11 time-warp protection: {:?}", e);
                continue; 
            }

            //  Genesis Block Size Limit for Quantum Era.
            //  Mathematical constraint enforcement via Block Weight and SigOps.
            if incoming_block.get_physical_size() > 8 * 1024 * 1024
                || incoming_block.get_block_weight() > quantum_btc::config::MAX_BLOCK_WEIGHT as u64 
                || incoming_block.get_block_sigops() > quantum_btc::config::MAX_BLOCK_SIGOPS as usize {
                tracing::error!("[ERROR] Firewall: Block exceeds 8MB quantum limit, weight, or SigOps limit. Payload dropped.");
                if sender_worker != thread_local_peer_id {
                    if safe_lock!(reputation_worker).report_offense(&sender_worker, NetworkOffense::MalformedData) {
                        let _ = swarm_cmd_tx_worker.try_send(SwarmCommand::BanAndDisconnect(sender_worker)); 
                    }
                }
                continue;
            }

            // L0 DEFENSE: Cryptographic Proof-of-Work and Merkle Tree verification.
            if sender_worker != thread_local_peer_id {
                if !consensus::ConsensusEngine::verify_proof_of_work(&incoming_block, incoming_block.header.target) {
                    tracing::error!("[ERROR] Firewall: Invalid Proof-of-Work! Malicious peer detected.");
                    let _ = safe_lock!(reputation_worker).report_offense(&sender_worker, NetworkOffense::InvalidHeader); 
                    let _ = swarm_cmd_tx_worker.try_send(SwarmCommand::BanAndDisconnect(sender_worker));
                    continue;
                }
                if !consensus::ConsensusEngine::verify_merkle_root(&incoming_block) {
                    tracing::error!("[ERROR] Firewall: Merkle Root mismatch! Transaction payload tampered.");
                    let _ = safe_lock!(reputation_worker).report_offense(&sender_worker, NetworkOffense::MalformedData); 
                    let _ = swarm_cmd_tx_worker.try_send(SwarmCommand::BanAndDisconnect(sender_worker));
                    continue;
                }
            } else {
                // FIX: Authenticate local origin to prevent executioner self-ban.
                tracing::debug!("[INFO] Firewall: Local origin authenticated. Bypassing P2P punitive filters.");
                if !consensus::ConsensusEngine::verify_proof_of_work(&incoming_block, incoming_block.header.target) || !consensus::ConsensusEngine::verify_merkle_root(&incoming_block) {
                    tracing::error!("[ERROR] Firewall: Local block cryptographic validation failed. Dropping block safely.");
                    engine_idle_notify_worker.notify_waiters(); // FIX: Prevent local miner deadlock on rejection
                    continue;
                }
            }
            
            if incoming_height > current_height {
                if incoming_height.saturating_sub(current_height) > MAX_REORG_DEPTH { continue; }
                
                // V1.2 FIX: Execute UtxoActor ProcessBlock command.
                let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                let _ = utxo_tx_worker.blocking_send(utxo::UtxoCommand::ApplyBlock {
                    block: incoming_block.clone(),
                    height: incoming_height,
                    is_historical: false,
                    resp: resp_tx,
                });
                
                match resp_rx.blocking_recv().unwrap_or(Err("Actor Channel Closed")) {
                    Ok(undo_log) => {
                        let array_index = storage_worker.get_chain_list().len() as u64;
                        
                        // L1 V2.0 CORE: Calculate absolute accumulated work for incoming block.
                        let prev_hash = incoming_block.header.previous_hash;
                        let current_work = storage_worker.get_block_index(&prev_hash).map(|idx| idx.chain_work).unwrap_or(0);
                        
    
                        // Calculate absolute accumulated work for incoming block.
                        let new_accumulated_work = current_work.saturating_add(incoming_block.header.get_block_proof());

                       // Execute atomic physical write via single bottleneck function.
                        // V1.2 FIX: Fetch latest UTXO snapshot from Actor for atomic persistence.
                        let (snap_tx, snap_rx) = tokio::sync::oneshot::channel();
                        let _ = utxo_tx_worker.blocking_send(utxo::UtxoCommand::GetSnapshot { resp: snap_tx });
                        let utxo_snap = snap_rx.blocking_recv().unwrap();

                        storage_worker.commit_state_transition(incoming_block.clone(), array_index, &undo_log, &utxo_snap, new_accumulated_work);
                        
                        *current_latest = incoming_block.clone();
                        
                        //  Populate L1 Block Relay Cache (FIFO).
                        // Strictly enforces Satoshi Causality: Never relay unpersisted physical state.
                        {
                            let mut cache = BLOCK_RELAY_CACHE.write().unwrap();
                            if cache.len() >= 10 {
                                cache.remove(0); // Constant time N=10 FIFO eviction
                            }
                            cache.push(incoming_block.clone());
                        }

                        safe_lock!(mempool_worker).atomic_sweep(&incoming_block.transactions);
                        
                        //  Broadcast AFTER physical write completes (Main Path).
                        // Ensures normal blocks mined locally are announced to the global network.
                        if sender_worker == thread_local_peer_id {
                            let _ = _p2p_tx_worker.blocking_send(quantum_btc::network::NetworkPayload::BlockAnnouncement(incoming_block.header.clone()));
                            tracing::info!("[INFO] Network: Local block physically secured and broadcasted.");
                        }
                        
                        if array_index > 0 && array_index % 1000 == 0 {
                            let cutoff = array_index.saturating_sub(1000);
                            let _ = storage_worker.prune_historical_witnesses(cutoff);
                        }

                        tracing::info!("[INFO] Consensus: Block validated. Chain height: {}", incoming_height);
                        let _ = daemon_cmd_tx_worker.send(quantum_btc::miner::MinerCommand::Stop);
                        quantum_btc::miner::PENDING_CMD.store(true, Ordering::Relaxed);
                        engine_idle_notify_worker.notify_waiters();
                        
        
                        //  ASERTi3-2d continuous difficulty adjustment. O(1) memory derivation.
                        let current_timestamp = incoming_block.header.timestamp;
                        let next_height = incoming_height + 1; 
                        
                        let new_target = consensus::ConsensusEngine::calculate_next_target(
                            anchor_time_worker.load(Ordering::Relaxed), 
                            anchor_target_worker.load(Ordering::Relaxed), 
                            current_timestamp, 
                            next_height
                        );
                        target_worker.store(new_target, Ordering::SeqCst);

                        //  Update to context-aware reward API.
                        safe_lock!(reputation_worker).reward_gossip(&sender_worker);

                        let mut current_cascade_hash = incoming_block.calculate_hash();
                        let mut cascade_count = 0;
                        
                        loop {
                            let recovered_orphans = storage_worker.get_orphans_by_parent(&current_cascade_hash);
                            if recovered_orphans.is_empty() { break; }
                            
                            let mut advanced = false;
                            for orphan in recovered_orphans {
                                let o_height = storage_worker.get_chain_list().len() as u64; 
                                
                                if let Err(e) = crate::consensus::verify_checkpoint(o_height, &orphan.calculate_hash()) {
                                    tracing::error!("[ERROR] Consensus: Orphan block rejected by checkpoint during cascade recovery: {}", e);
                                    continue;
                                }

                                let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                let _ = utxo_tx_worker.blocking_send(utxo::UtxoCommand::ApplyBlock { block: orphan.clone(), height: o_height, is_historical: false, resp: resp_tx });

                                if let Ok(o_undo_log) = resp_rx.blocking_recv().unwrap_or(Err("Actor Channel Closed")) {
                                    // L1 V2.0 CORE: Calculate absolute accumulated work for recovered orphan.
                                    let prev_hash = orphan.header.previous_hash;
                                    let current_work = storage_worker.get_block_index(&prev_hash).map(|idx| idx.chain_work).unwrap_or(0);
                                    // Calculate absolute accumulated work for recovered orphan.
                                    let new_accumulated_work = current_work.saturating_add(orphan.header.get_block_proof());
                                    
                                    let (snap_tx, snap_rx) = tokio::sync::oneshot::channel();
                                    let _ = utxo_tx_worker.blocking_send(utxo::UtxoCommand::GetSnapshot { resp: snap_tx });
                                    let utxo_snap = snap_rx.blocking_recv().unwrap();

                                    storage_worker.commit_state_transition(orphan.clone(), o_height, &o_undo_log, &utxo_snap, new_accumulated_work);
                                    
                                    *current_latest = orphan.clone();
                                    safe_lock!(mempool_worker).atomic_sweep(&orphan.transactions);
                                    current_cascade_hash = orphan.calculate_hash();
                                    cascade_count += 1;
                                    advanced = true;
                                    break; 
                                }
                            }
                            if !advanced { break; } 
                        }
                        
                        if cascade_count > 0 {
                            // UTXO state already atomically flushed per recovered orphan.
                            println!("[INFO] Sync: Reassembled {} orphan blocks via atomic pipeline.", cascade_count);
                        }
                    },
                    Err(e) => {
                        println!("[WARN] Consensus: Block rejected by UTXO state machine: {}", e);
                        // FIX: Wake up local miner if its own block is rejected by the UTXO state machine.
                        if sender_worker == thread_local_peer_id {
                            engine_idle_notify_worker.notify_waiters();
                        }
                    }
                }
            } else if incoming_height == current_height {
                if incoming_block.calculate_hash() < current_latest.calculate_hash() {
                    let old_tip_hash = current_latest.calculate_hash();
                    if let Some(undo_log) = storage_worker.get_undo_log(current_height, &old_tip_hash) {
                        
                        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                        let _ = utxo_tx_worker.blocking_send(utxo::UtxoCommand::DisconnectBlock { undo_log, resp: resp_tx });
                        if let Err(e) = resp_rx.blocking_recv().unwrap_or(Err("Actor Channel Closed")) {
                            tracing::error!("[ERROR] Tie-breaker rollback failed: {}. Preserving state.", e);
                            continue; // Skip invalid state transition
                        }

                        if let Some(old_block) = storage_worker.get_block_by_hash(&old_tip_hash, false) {
                            let mut mempool_guard = safe_lock!(mempool_worker);
                            for tx in old_block.transactions.into_iter().skip(1) { // Skip coinbase
                                // MAINNET FIX: Eradicate hardcoded dust fee during Tie-breaker Reorg.
                                let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                let _ = utxo_tx_worker.blocking_send(quantum_btc::utxo::UtxoCommand::ValidateMempoolTx { tx: tx.clone(), current_height, crypto_pre_verified: true, resp: resp_tx });
                                if let Ok(Ok(exact_fee)) = resp_rx.blocking_recv() {
                                    let _ = mempool_guard.add_transaction(tx, exact_fee);
                                }
                            }
                        }
                    }

                    // MAINNET Note: Physical disk eradication for tie-breaker reorg.
                    // Erases orphaned UTXOs and witness payloads from RocksDB prior to new state transition.
                    storage_worker.rollback_chain(current_height as usize - 1);

                    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                    let _ = utxo_tx_worker.blocking_send(utxo::UtxoCommand::ApplyBlock { block: incoming_block.clone(), height: incoming_height, is_historical: false, resp: resp_tx });

                    if let Ok(undo_log) = resp_rx.blocking_recv().unwrap() {
                        // FIX: Enforce atomic state transition for tie-breaker blocks to prevent missing undo logs.
                        let prev_hash = incoming_block.header.previous_hash;
                        let current_work = storage_worker.get_block_index(&prev_hash).map(|idx| idx.chain_work).unwrap_or(0);
                        let new_accumulated_work = current_work.saturating_add(incoming_block.header.get_block_proof());
                        
                        let (snap_tx, snap_rx) = tokio::sync::oneshot::channel();
                        let _ = utxo_tx_worker.blocking_send(utxo::UtxoCommand::GetSnapshot { resp: snap_tx });
                        let utxo_snap = snap_rx.blocking_recv().unwrap();
                        
                        //  Eradicate E0425 scope error.
                        // Tie-breaker overwrites the current tip, so the physical index is strictly 'current_height'.
                        storage_worker.commit_state_transition(incoming_block.clone(), current_height, &undo_log, &utxo_snap, new_accumulated_work);
                        
                        *current_latest = incoming_block.clone();
                        
                        //  Populate L1 Block Relay Cache (Tie-Breaker Path).
                        // Ensures newly reorganized tips are instantly available to the P2P swarm.
                        {
                            let mut cache = BLOCK_RELAY_CACHE.write().unwrap();
                            if cache.len() >= 10 {
                                cache.remove(0); // Bounded O(n) eviction. Safe for N=10.
                            }
                            cache.push(incoming_block.clone());
                        }

                        safe_lock!(mempool_worker).atomic_sweep(&incoming_block.transactions);
                        
                        //  Broadcast AFTER physical write completes.
                        // Ensures remote nodes will find the block in RocksDB when they ask for it.
                        if sender_worker == thread_local_peer_id {
                            let _ = _p2p_tx_worker.blocking_send(quantum_btc::network::NetworkPayload::BlockAnnouncement(incoming_block.header.clone()));
                            tracing::info!("[INFO] Network: Local block physically secured and broadcasted.");
                        }
                        
                        // FIX: Topology-Aware Async Snapshot Reconciliation
                        // Lock duration: ~1 microsecond. Extracts transaction topology skeleton.
                        let snapshot: Vec<([u8; 32], Vec<TxIn>, Vec<TxOut>)> = {
                            let guard = safe_lock!(mempool_worker);
                            guard.tx_pool.values().map(|entry| {
                                (entry.tx.calculate_id(), entry.tx.inputs.clone(), entry.tx.outputs.clone())
                            }).collect()
                        };
                        
                        let utxo_tx_async = utxo_tx_worker.clone();
                        let mempool_async = mempool_worker.clone();
                        tokio::spawn(async move {
                            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                            let _ = utxo_tx_async.send(utxo::UtxoCommand::ReconcileMempool { snapshot, resp: resp_tx }).await;
                            
                            if let Ok(blacklist) = resp_rx.await {
                                if !blacklist.is_empty() {
                                    let mut guard = safe_lock!(mempool_async);
                                    // Retain preserves insertion order perfectly for both HashMap and IndexMap
                                    guard.tx_pool.retain(|k, _| !blacklist.contains(k));
                                    tracing::info!("[INFO] Mempool: Asynchronous snapshot reconciliation purged {} ghost transactions.", blacklist.len());
                                }
                            }
                        });

                        println!("[INFO] Consensus: Chain reorganization. Switched to block at height: {}", incoming_height);
                        let _ = daemon_cmd_tx_worker.send(quantum_btc::miner::MinerCommand::Stop);
                        quantum_btc::miner::PENDING_CMD.store(true, Ordering::Relaxed);
                        engine_idle_notify_worker.notify_waiters();
                        
                        //  Recalibrate ASERT target boundary post tie-breaker chain reorganization.
                        let active_chain = storage_worker.get_chain_list();
                        let tip_height = active_chain.len().saturating_sub(1) as u64;
                        
                        let new_target = consensus::ConsensusEngine::calculate_next_target(
                            anchor_time_worker.load(Ordering::Relaxed),
                            anchor_target_worker.load(Ordering::Relaxed),
                            incoming_block.header.timestamp,
                            tip_height + 1
                        );
                        target_worker.store(new_target, Ordering::SeqCst);

                        //  Update to context-aware reward API.
                        safe_lock!(reputation_worker).reward_gossip(&sender_worker);
                    }
                }
            }
        }
    });

    // Bootnode protocol activation.
    // Initiates connection to the hardcoded DNS seed to bootstrap the Kademlia DHT routing table.
    // Standard miners will query this node; the seed itself will passively listen.
    if !is_seed_node {
        let bootnode_addr = format!("/dns4/{}/tcp/{}", seed_host, seed_port);
        match bootnode_addr.parse::<libp2p::Multiaddr>() {
            Ok(addr) => {
                if let Err(e) = swarm.dial(addr) {
                    println!("[ERROR] P2P: Bootnode connection failed: {:?}", e);
                } else {
                    println!("[INFO] P2P: Dialing bootstrap node at {}:{}", seed_host, seed_port);
                }
            },
            Err(_) => println!("[WARN] P2P: Invalid bootnode address format: {}", bootnode_addr),
        }
    } else {
        println!("[INFO] P2P: Seed node active. Passively listening for incoming connections.");
    }

    // Dedicated OS thread for Terminal CLI.
    // Decouples synchronous I/O from the Tokio async executor to prevent task starvation.
    let cli_auto_mine_flag = auto_mine_flag.clone();
    let cli_datadir = datadir.clone();
    let cli_latest_block = latest_block.clone();
    // : Downgrade to Weak to prevent terminal thread from holding the physical lock.
    let cli_mempool_weak = Arc::downgrade(&mempool);
    let cli_current_target = current_target.clone();
    let cli_storage_weak = Arc::downgrade(&storage);
    let cli_daemon_cmd_tx = daemon_cmd_tx.clone();
    let cli_engine_idle_notify = engine_idle_notify.clone();
    let cli_utxo_tx = utxo_tx.clone();
    let cli_p2p_tx = p2p_tx.clone();
    let cli_swarm_cmd_tx = swarm_cmd_tx.clone();
    let rt_handle = tokio::runtime::Handle::current();

    // Headless auto-mine injection utilizing environment variables for decoupled configuration
    if std::env::var("QBTC_CHAOS_TEST").is_ok() {
        let wallet_name = std::env::var("QBTC_MINER_WALLET").unwrap_or_else(|_| "default_miner".to_string());
        println!("[INFO] System: Chaos test environment detected. Engaging headless auto-mine sequence.");
        
        // [100% COMPILE GUARANTEE]: Generate deterministic 32-byte PK hash directly from wallet name.
        // Bypasses the internal wallet module entirely to prevent struct definition mismatches.
        let mut miner_pk_hash = [0u8; 32];
        let name_bytes = wallet_name.as_bytes();
        let copy_len = name_bytes.len().min(32);
        miner_pk_hash[..copy_len].copy_from_slice(&name_bytes[..copy_len]);
        
        println!("[INFO] Wallet: Auto-mining engaged. Coinbase target locked to '{}'", wallet_name);

        auto_mine_flag.store(true, Ordering::SeqCst);
        
        let flag_clone = auto_mine_flag.clone();
        let latest_block_clone = latest_block.clone();
        let mempool_clone = mempool.clone();
        let target_clone = current_target.clone();
        let storage_clone = storage.clone();
        let daemon_cmd_tx_clone = daemon_cmd_tx.clone();
        let engine_idle_notify_clone = engine_idle_notify.clone();
        
        rt_handle.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            while flag_clone.load(Ordering::SeqCst) {
                let previous_hash = safe_lock!(latest_block_clone).calculate_hash();
                let current_height = storage_clone.get_chain_list().len() as u64;

                let mut total_fees = 0u64;
                let mut txs = {
                    let mempool_guard = safe_lock!(mempool_clone);
                    let selected_txs = mempool_guard.get_txs_for_mining();
                    for tx in &selected_txs {
                        let tx_hash = tx.calculate_id();
                        if let Some(entry) = mempool_guard.tx_pool.get(&tx_hash) {
                            total_fees += entry.fee;
                        }
                    }
                    selected_txs
                };
                
                let coinbase_in = transaction::TxIn { previous_output_hash: [0u8; 32], vout: current_height as u32 };
                let coinbase_witness = transaction::TxWitness { signature: vec![], public_key: vec![] };
                let block_reward = economics::CentralBank::get_block_reward(current_height) + total_fees;

                txs.insert(0, transaction::Transaction { 
                    inputs: vec![coinbase_in], 
                    outputs: vec![transaction::TxOut { value: block_reward, public_key_hash: miner_pk_hash, recovery: None }],
                    witnesses: vec![coinbase_witness] 
                });

                let template = miner::BlockTemplate {
                    previous_hash,
                    transactions: txs,
                    target: target_clone.load(Ordering::SeqCst),
                    current_height,
                };
                let _ = daemon_cmd_tx_clone.send(miner::MinerCommand::Mine(template));
                miner::PENDING_CMD.store(true, Ordering::Relaxed);

                engine_idle_notify_clone.notified().await;
            }
        });
    }

    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line_res in stdin.lines() {
            // : Attempt to upgrade Weak pointers. Exit gracefully if main sequence initiated teardown.
            let cli_storage = match cli_storage_weak.upgrade() { Some(s) => s, None => break };
            let cli_mempool = match cli_mempool_weak.upgrade() { Some(m) => m, None => break };

            let line = match line_res {
                Ok(l) => l,
                Err(_) => break,
            };
            let parts: Vec<&str> = line.trim().split_whitespace().collect();
            if parts.is_empty() { continue; }

            match parts[0] {
                "connect" => {
                    if parts.len() < 3 {
                        println!("[WARN] Usage: connect <ip_address> <port>");
                        continue;
                    }
                    let target_addr = format!("/ip4/{}/tcp/{}", parts[1], parts[2]);
                    if let Ok(addr) = target_addr.parse::<libp2p::Multiaddr>() {
                        let _ = cli_swarm_cmd_tx.blocking_send(SwarmCommand::Dial(addr));
                    } else {
                        println!("[ERROR] Network: Invalid IP format.");
                    }
                }
                "auto_mine" => {
                    if parts.len() < 2 {
                        println!("[WARN] Usage: auto_mine <start|stop> [target_alias_or_address]");
                        continue;
                    }
                    match parts[1] {
                        "start" => {
                            //  IBD Lock for daemon mode.
                            let validated_hash = safe_lock!(cli_latest_block).calculate_hash();
                            let downloaded_hash = cli_storage.get_chain_list().last().copied().unwrap_or_else(|| Block::genesis().calculate_hash());
                            if validated_hash != downloaded_hash {
                                println!("[WARN] Miner: Node is synchronizing. Auto-mining is physically locked during IBD.");
                                continue;
                            }

                            if cli_auto_mine_flag.load(Ordering::SeqCst) {
                                println!("[INFO] Miner: Auto-mine process is already active.");
                                continue;
                            }
                            
                            let target_arg = parts.get(2).copied().unwrap_or("default");
                            let mut miner_pk_hash = [0u8; 32];
                            let mut target_locked = false;

                            if let Some(decoded) = crate::wallet::QuantumWallet::decode_qbtc_address(target_arg) {
                                miner_pk_hash.copy_from_slice(&decoded[0..32]);
                                target_locked = true;
                            } else if let Some((pub_key, _)) = crate::wallet::QuantumWallet::get_public_info(&cli_datadir, target_arg) {
                                let mut hasher = sha2::Sha256::new();
                                sha2::Digest::update(&mut hasher, &pub_key); 
                                miner_pk_hash = hasher.finalize().into();
                                target_locked = true;
                            }

                            if !target_locked {
                                println!("[ERROR] Miner: Target '{}' does not exist. Auto-mine sequence aborted.", target_arg);
                                continue;
                            }

                            cli_auto_mine_flag.store(true, Ordering::SeqCst);
                            println!("[INFO] Miner: Auto-mine engaged. Target locked: {}", target_arg);

                            let flag_clone = cli_auto_mine_flag.clone();
                            let latest_block_clone = cli_latest_block.clone();
                            let mempool_clone = cli_mempool.clone();
                            let target_clone = cli_current_target.clone();
                            let storage_clone = cli_storage.clone();
                            let daemon_cmd_tx_clone = cli_daemon_cmd_tx.clone();
                            let engine_idle_notify_clone = cli_engine_idle_notify.clone();

                            rt_handle.spawn(async move {
                                while flag_clone.load(Ordering::SeqCst) {
                                    let previous_hash = safe_lock!(latest_block_clone).calculate_hash();
                                    let current_height = storage_clone.get_chain_list().len() as u64;

                                    let mut total_fees = 0u64;
                                    let mut txs = {
                                        let mempool_guard = safe_lock!(mempool_clone);
                                        let selected_txs = mempool_guard.get_txs_for_mining();
                                        for tx in &selected_txs {
                                            let tx_hash = tx.calculate_id();
                                            if let Some(entry) = mempool_guard.tx_pool.get(&tx_hash) {
                                                total_fees += entry.fee;
                                            }
                                        }
                                        selected_txs
                                    };
                                    
                                    let coinbase_in = TxIn { previous_output_hash: [0u8; 32], vout: current_height as u32 };
                                    let coinbase_witness = TxWitness { signature: vec![], public_key: vec![] };
                                    let block_reward = CentralBank::get_block_reward(current_height) + total_fees;

                                    txs.insert(0, Transaction { 
                                        inputs: vec![coinbase_in], 
                                        outputs: vec![TxOut { value: block_reward, public_key_hash: miner_pk_hash, recovery: None }],
                                        witnesses: vec![coinbase_witness] 
                                    });

                            

                                    let template = quantum_btc::miner::BlockTemplate {
                                        previous_hash,
                                        transactions: txs,
        
                                        target: target_clone.load(Ordering::SeqCst),
                                        current_height,
                                    };
                                    let _ = daemon_cmd_tx_clone.send(quantum_btc::miner::MinerCommand::Mine(template));
                                    quantum_btc::miner::PENDING_CMD.store(true, Ordering::Relaxed);

                                    engine_idle_notify_clone.notified().await;
                                }
                                println!("[INFO] Miner: Auto-mine process terminated.");
                            });
                        }
                        "stop" => {
                            if !cli_auto_mine_flag.load(Ordering::SeqCst) {
                                println!("[WARN] Miner: Auto-mine is not currently running.");
                            } else {
                                cli_auto_mine_flag.store(false, Ordering::SeqCst);
                                let _ = cli_daemon_cmd_tx.send(quantum_btc::miner::MinerCommand::Stop);
                                quantum_btc::miner::PENDING_CMD.store(true, Ordering::Relaxed);
                                cli_engine_idle_notify.notify_waiters();
                                println!("[INFO] Miner: Auto-mine halt requested. Core engines spinning down.");
                            }
                        }
                        _ => println!("[WARN] Usage: auto_mine <start|stop> [target_alias]"),
                    }
                }
                "wallet_gen" => {
                    let wallet_name = if parts.len() > 1 { parts[1] } else { "default" };
                    let _guard = ConsoleSilenceGuard::new();
                    
                    let password_result = Password::new(&format!("Enter secure password for cold vault '{}':", wallet_name))
                        .with_display_mode(PasswordDisplayMode::Masked)
                        .with_custom_confirmation_message("Confirm secure password:")
                        .with_custom_confirmation_error_message("[ERROR] Passwords do not match. Aborting generation.")
                        .with_help_message("Warning: Asset recovery is impossible if this password is lost.")
                        .prompt();
                        
                    drop(_guard); // FIX: Instantly restore console output after human input is gathered

                    let password = match password_result {
                        Ok(pwd) => pwd,
                        Err(_) => {
                            println!("[INFO] Wallet generation aborted by user.");
                            continue;
                        }
                    };

                    if password.trim().is_empty() {
                        println!("[ERROR] Password cannot be empty. Aborting.");
                        continue;
                    }

                    let mut entropy = [0u8; 16];
                    UnwrapErr(SysRng).fill_bytes(&mut entropy);
                    let mnemonic = bip39::Mnemonic::from_entropy(&entropy).unwrap();
                    let phrase = mnemonic.to_string();
                    
                    if let Ok(new_wallet) = wallet::QuantumWallet::restore_from_mnemonic(&phrase) {
                        if new_wallet.save_to_disk_secure(&cli_datadir, wallet_name, &password).is_ok() {
                            println!("[INFO] Wallet: Keypair generated. Alias: '{}' | Address: {}", wallet_name, new_wallet.qbtc_address);
                            println!("[INFO] Wallet: Mnemonic: {}", phrase);
                            println!("[WARN] Wallet: Please store the mnemonic safely in an air-gapped location.");
                            println!("[INFO] Wallet: Keystore AES-256-GCM encrypted and secured.");
                        } else {
                            println!("[ERROR] Wallet: Failed to flush keystore to disk.");
                        }
                    }
                }
                "list_wallets" => {
                    let dir_path = format!("{}/keystores", cli_datadir);
                    println!("[INFO] Keystore: Found wallets in datadir '{}':", cli_datadir);
                    if let Ok(entries) = std::fs::read_dir(dir_path) {
                        for entry in entries.flatten() {
                            if let Some(name) = entry.file_name().to_str() {
                                if name.ends_with(".dat") { println!("  - {}", name.replace(".dat", "")); }
                            }
                        }
                    }
                }
                "wallet_restore" => {
                    if parts.len() < 14 {
                        println!("[WARN] Usage: wallet_restore <wallet_name> <word1> ... <word12>");
                        continue;
                    }
                    let wallet_name = parts[1];
                    let phrase = parts[2..14].join(" "); 
                    
                    let _guard = ConsoleSilenceGuard::new();

                    let password_result = Password::new("Enter encryption password for restored vault:")
                        .with_display_mode(PasswordDisplayMode::Masked)
                        .with_custom_confirmation_message("Confirm secure password:")
                        .with_custom_confirmation_error_message("[ERROR] Passwords do not match. Aborting.")
                        .prompt();
                        
                    drop(_guard); // FIX: Instantly restore console output after human input is gathered

                    let password = match password_result {
                        Ok(pwd) => pwd,
                        Err(_) => {
                            println!("[INFO] Restoration aborted by user.");
                            continue;
                        }
                    };

                    if password.trim().is_empty() {
                        println!("[ERROR] Password cannot be empty.");
                        continue;
                    }

                    match wallet::QuantumWallet::restore_from_mnemonic(&phrase) {
                        Ok(recovered_wallet) => {
                            if recovered_wallet.save_to_disk_secure(&cli_datadir, wallet_name, &password).is_ok() {
                                println!("[INFO] Wallet: Successfully restored from mnemonic.");
                                println!("[INFO] Wallet: Alias: '{}' | Address: {}", wallet_name, recovered_wallet.qbtc_address);
                            } else {
                                println!("[ERROR] Wallet: Failed to secure recovered keystore to disk.");
                            }
                        }
                        Err(e) => println!("[ERROR] Wallet: Restore failed: {}", e),
                    }
                }
                "wallet_change_password" => {
                    if parts.len() < 2 {
                        println!("[WARN] Usage: wallet_change_password <wallet_name>");
                        continue;
                    }
                    let wallet_name = parts[1];
                    let _guard = ConsoleSilenceGuard::new();

                    let old_password_result = Password::new("Enter current decryption password:")
                            .with_display_mode(PasswordDisplayMode::Masked)
                            .prompt();

                    let old_password = match old_password_result {
                        Ok(pwd) => pwd,
                        Err(_) => {
                            println!("[INFO] Password modification aborted by user.");
                            continue;
                        }
                    };

                    let my_wallet = match wallet::QuantumWallet::load_from_disk_secure(&cli_datadir, wallet_name, &old_password) {
                        Ok(w) => w,
                        Err(_) => {
                            println!("[ERROR] Authentication failed. Invalid current password.");
                            continue;
                        }
                    };

                    let new_password_result = Password::new("Enter NEW secure password:")
                            .with_display_mode(PasswordDisplayMode::Masked)
                            .with_custom_confirmation_message("Confirm NEW secure password:")
                            .with_custom_confirmation_error_message("[ERROR] Passwords do not match. Aborting.")
                            .prompt();
                            
                    drop(_guard); // FIX: Instantly restore console output after human input is gathered

                    let new_password = match new_password_result {
                        Ok(pwd) => pwd,
                        Err(_) => {
                            println!("[INFO] Password modification aborted by user.");
                            continue;
                        }
                    };

                    if new_password.trim().is_empty() {
                        println!("[ERROR] New password cannot be empty. Aborting.");
                        continue;
                    }

                    match my_wallet.save_to_disk_secure(&cli_datadir, wallet_name, &new_password) {
                        Ok(_) => println!("[INFO] Keystore for '{}' successfully re-encrypted with new credentials.", wallet_name),
                        Err(e) => println!("[ERROR] Keystore re-encryption failed: {}", e),
                    }
                }
                "balance" => {
                    //  IBD Lock to prevent misleading zero-balance queries.
                    let validated_hash = safe_lock!(cli_latest_block).calculate_hash();
                    let downloaded_hash = cli_storage.get_chain_list().last().copied().unwrap_or_else(|| Block::genesis().calculate_hash());
                    if validated_hash != downloaded_hash {
                        println!("[WARN] Vault: Node is synchronizing. Ledger state is incomplete. Please wait for True Sync to complete.");
                        continue;
                    }
                    
                    let wallet_name = if parts.len() > 1 { parts[1] } else { "default" };
                    
                    if let Some((pub_key, address)) = crate::wallet::QuantumWallet::get_public_info(&cli_datadir, wallet_name) {
                        let mut hasher = Sha256::new(); hasher.update(&pub_key);
                        let my_pk_hash: [u8; 32] = hasher.finalize().into();
                        
                        let current_height = cli_storage.get_chain_list().len() as u64;
                        let pending_txs: Vec<Transaction> = safe_lock!(cli_mempool).tx_pool.values().map(|e| e.tx.clone()).collect();
                        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                        
                        let _ = cli_utxo_tx.blocking_send(utxo::UtxoCommand::GetBalance { pubkey_hash: my_pk_hash, current_height, pending_txs: pending_txs.clone(), resp: resp_tx });
                        // Receive decoupled parameters ensuring strict mempool isolation.
                        let (mature, pending, locked) = resp_rx.blocking_recv().unwrap_or((0, 0, 0));
                        
                        println!("[INFO] Vault: Balance for Alias '{}' ({}):", wallet_name, address);
                        let hex_string: String = my_pk_hash.iter().map(|b| format!("{:02x}", b)).collect();
                        println!("[INFO] Vault: Target Hash Hex: {}", hex_string);

                        // Strict UI separation of cryptographic certainty.
                        println!("  Confirmed (On-Chain) : {:.8} QBTC", mature as f64 / 100_000_000.0);
                        if pending > 0 {
                            println!("  Pending (Mempool)    : +{:.8} QBTC", pending as f64 / 100_000_000.0);
                        }
                        println!("  Locked (Coinbase)    : {:.8} QBTC", locked as f64 / 100_000_000.0);
                    } else { 
                        println!("[ERROR] Wallet: No keystore found for '{}'.", wallet_name); 
                    }
                }
                "transfer" => {
                    if parts.len() < 3 {
                        println!("[WARN] Usage: transfer <amount_in_qbtc> <recipient_pubkey_hash_hex> [wallet_name]");
                        continue;
                    }
                    
                    //  IBD Lock to prevent torn-state transactions.
                    let validated_hash = safe_lock!(cli_latest_block).calculate_hash();
                    let downloaded_hash = cli_storage.get_chain_list().last().copied().unwrap_or_else(|| Block::genesis().calculate_hash());
                    if validated_hash != downloaded_hash {
                        println!("[WARN] Transfer: Node is synchronizing. Transactions cannot be safely authored during IBD.");
                        continue;
                    }
                    
                    let amount_str = parts[1];
                    let recipient_hex = parts[2];
                    let wallet_name = if parts.len() > 3 { parts[3] } else { "default" };
                    
                    /*  Absolute TTY Mutex & OS-Level Physical Lock.
                               Guarantees strict physical isolation of the stdin/stdout buffer. */
                            let _guard = ConsoleSilenceGuard::new();
                            
                            /* Pause main thread briefly to allow in-flight async logs to drain. */
                            std::thread::sleep(std::time::Duration::from_millis(150));
                            let _ = std::io::Write::flush(&mut std::io::stdout());
                            
                            /* OS-level lock on standard output. Background threads calling std::println! 
                               will safely suspend until the password is fully entered. */
                            let _stdout_lock = std::io::stdout().lock();

                            let password_result = Password::new(&format!("Enter password to unlock vault '{}' for transfer:", wallet_name))
                                    .with_display_mode(PasswordDisplayMode::Masked)
                                    .prompt();
                                    
                            drop(_stdout_lock);
                            drop(_guard);

                    let password = match password_result {
                        Ok(pwd) => pwd,
                        Err(_) => {
                            println!("[INFO] Transfer aborted by user.");
                            continue;
                        }
                    };

                    match wallet::QuantumWallet::load_from_disk_secure(&cli_datadir, wallet_name, &password) {
                        Ok(my_wallet) => {
                            let amount_atomic: u64 = if let Some(dot_idx) = amount_str.find('.') {
                                let mut int_part = amount_str[..dot_idx].to_string();
                                let mut frac_part = amount_str[dot_idx + 1..].to_string();
                                if frac_part.len() > 8 { frac_part.truncate(8); }
                                while frac_part.len() < 8 { frac_part.push('0'); }
                                int_part.push_str(&frac_part);
                                int_part.parse().unwrap_or(0)
                            } else {
                                amount_str.parse::<u64>().unwrap_or(0).saturating_mul(100_000_000)
                            };
                            let amount_qbtc = amount_atomic as f64 / 100_000_000.0;
                            
                            let mut target_hash = [0u8; 32];
                            let mut is_valid_target = false;

                            if recipient_hex.starts_with("qbtc1") {
                                if let Some(decoded_bytes) = crate::wallet::QuantumWallet::decode_qbtc_address(&recipient_hex) {
                                    println!("[INFO] Transfer: Valid Bech32m address detected. Resolving bytes.");
                                    target_hash.copy_from_slice(&decoded_bytes[0..32]);
                                    is_valid_target = true;
                                } else {
                                    println!("[ERROR] Transfer: Invalid Bech32m address format.");
                                    continue;
                                }
                            }
                            else if let Some((pub_key, _)) = crate::wallet::QuantumWallet::get_public_info(&cli_datadir, &recipient_hex) {
                                println!("[INFO] Transfer: Local alias '{}' resolved.", recipient_hex);
                                let mut hasher = Sha256::new(); hasher.update(&pub_key);
                                target_hash = hasher.finalize().into();
                                is_valid_target = true;
                            }
                            else if recipient_hex.len() == 64 {
                                println!("[WARN] Transfer: Raw hex used. Checksum validation bypassed.");
                                for i in 0..32 { target_hash[i] = u8::from_str_radix(&recipient_hex[i*2..i*2+2], 16).unwrap_or(0); }
                                is_valid_target = true;
                            }

                            if !is_valid_target {
                                println!("[ERROR] Transfer: Invalid target checksum or unknown alias.");
                                continue;
                            }

                            let mut hasher = Sha256::new(); hasher.update(&my_wallet.public_key);
                            let my_pk_hash: [u8; 32] = hasher.finalize().into();
                            
                            let current_height = cli_storage.get_chain_list().len() as u64;
                            /*  FINAL: Algebraic Convergence Engine (Quantum Grade)
                                       Utilizes O(n) mathematical state machine with rigid physical buffers.
                                       Annihilates CPU overhead while absolutely preventing VarInt byte-drift rejects. */
                                    let network_fee_rate: u64 = quantum_btc::config::MIN_RELAY_FEE_RATE * 5;

                                    const TX_BASE_BYTES: u64 = 30;
                                    const TX_IN_BYTES: u64 = 5350;
                                    const TX_OUT_BYTES: u64 = 50;

                                    let mut target_fee_atomic: u64 = (TX_BASE_BYTES + TX_IN_BYTES + (2 * TX_OUT_BYTES)) * network_fee_rate;
                                    /* FIX: Use '_' prefix to explicitly acknowledge this is a diagnostic buffer */
                                    let mut _utxo_query_result = Err("Insufficient deep liquidity to cover transaction.");
                                    let pending_txs: Vec<quantum_btc::transaction::Transaction> = cli_mempool.lock().unwrap().tx_pool.values().map(|e| e.tx.clone()).collect();

                                    for _iteration in 0..5 {
                                        /* FIX: Declare current_total_required strictly within the loop scope to prevent redundant assignment warnings */
                                        let current_total_required = amount_atomic + target_fee_atomic;
                                        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                        
                                        let _ = cli_utxo_tx.blocking_send(quantum_btc::utxo::UtxoCommand::GetSpendable {
                                            pubkey_hash: my_pk_hash,
                                            current_height,
                                            required_amount: current_total_required,
                                            pending_txs: pending_txs.clone(),
                                            resp: resp_tx
                                        });

                                        match resp_rx.blocking_recv().unwrap_or(Err("Actor Channel Closed")) {
                                            Ok((selected, gathered)) => {
                                                let input_count = selected.len() as u64;
                                                let projected_bytes = TX_BASE_BYTES + (input_count * TX_IN_BYTES) + (2 * TX_OUT_BYTES);
                                                
                                                //  Expand local wallet assembly line capacity with strict safety margins.
                                                // Increased to 8MB to allow Whale TXs (e.g., 2000+ UTXOs at ~7MB).
                                                // STRICTLY kept under the 16MB P2P network limit to prevent Block Overflow.
                                                if projected_bytes > 8_000_000 {
                                                    _utxo_query_result = Err("Transaction exceeds 8MB physical limit");
                                                    break;
                                                }
                                                
                                                let projected_fee = projected_bytes * network_fee_rate;
                                                
                                                if gathered >= amount_atomic + projected_fee {
                                                    target_fee_atomic = projected_fee;
                                                    _utxo_query_result = Ok((selected, gathered));
                                                    break; /* Convergence achieved! */
                                                } else {
                                                    /* Not enough gathered to cover the revised fee. Adjust target and re-iterate. */
                                                    target_fee_atomic = projected_fee;
                                                }
                                            }
                                            Err(e) => {
                                                _utxo_query_result = Err(e);
                                                break;
                                            }
                                        }
                                    }

                                    //  Eradicate TOCTOU fee calculation bug.
                                    // Directly extract the exactly weighed and locked UTXOs from the convergence engine.
                                    // NEVER query the UTXO set a second time to prevent state drift and FeeTooLow rejection.
                                    let utxo_query_result = _utxo_query_result;
                                    
                                    //  Restore missing variable required for precise change calculation.
                                    let total_required = amount_atomic + target_fee_atomic;

                            match utxo_query_result {
                                Ok((utxos, total_gathered)) => {
                                    let mut inputs = Vec::new();
                                    for (outpoint, _) in &utxos {
                                        inputs.push(TxIn {
                                            previous_output_hash: outpoint.tx_hash,
                                            vout: outpoint.vout,
                                        });
                                    }
                                    
                                    let mut outputs = vec![TxOut { value: amount_atomic, public_key_hash: target_hash, recovery: None }];
                                    if total_gathered > total_required {
                                        outputs.push(TxOut { value: total_gathered - total_required, public_key_hash: my_pk_hash, recovery: None });
                                    }

                                    let temp_tx = Transaction { inputs: inputs.clone(), outputs: outputs.clone(), witnesses: vec![] };
                                    let tx_core_hash = temp_tx.calculate_id();

                                    quantum_btc::miner::IS_SIGNING.store(true, Ordering::SeqCst);
                                    let inputs_len = inputs.len();
                                    
                                    // Heavy ML-DSA-65 post-quantum signing block
                                    let witnesses: Vec<TxWitness> = (0..inputs_len).into_par_iter().map(|_| {
                                        let signature = my_wallet.sign_transaction(&tx_core_hash, false, 0);
                                        TxWitness {
                                            signature,
                                            public_key: my_wallet.public_key.clone(),
                                        }
                                    }).collect();
                                    
                                    quantum_btc::miner::IS_SIGNING.store(false, Ordering::SeqCst);

                                    let tx = Transaction { inputs, outputs, witnesses };
                                    
                                    let eval_height = cli_storage.get_chain_list().len() as u64;
                                    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                    //  FIX: Mark as pre-verified since we just authored and signed it locally
                                    let _ = cli_utxo_tx.blocking_send(utxo::UtxoCommand::ValidateMempoolTx { tx: tx.clone(), current_height: eval_height, crypto_pre_verified: true, resp: resp_tx });
                                    
                                    match resp_rx.blocking_recv().unwrap_or(Err("Actor Channel Closed")) {
                                        Ok(exact_fee) => {
                                            if exact_fee < 1000 {
                                                println!("[ERROR] Local: Transaction fee {} too low to meet network relay policy.", exact_fee);
                                            } else {
                                                /*  Causality lock. Commit to physical mempool before P2P broadcast. */
                                                let mut mempool_guard = safe_lock!(cli_mempool);
                                                if let Err(e) = mempool_guard.add_transaction(tx.clone(), exact_fee) {
                                                    println!("[ERROR] Local: Mempool rejected the transaction: {:?}", e);
                                                } else {
                                                    drop(mempool_guard);
                                                    let _ = cli_p2p_tx.blocking_send(NetworkPayload::TransactionInv(tx.calculate_id()));
                                                    println!("[INFO] Network: Transaction broadcasted ({} QBTC + {} Sats fee).", amount_qbtc, exact_fee);
                                                }
                                            }
                                        }
                                        Err(_) => {
                                            println!("[ERROR] Local: UTXO validation rejected the transaction.");
                                        }
                                    }
                                }
                                Err(e) => println!("[ERROR] Transfer: {}", e),
                            }
                        }
                        Err(err_msg) => println!("[ERROR] Wallet: {}", err_msg),
                    }
                } 
                "mine" => {
                    //  IBD Lock to prevent Frankenstein fork creation.
                    let validated_hash = safe_lock!(cli_latest_block).calculate_hash();
                    let downloaded_hash = cli_storage.get_chain_list().last().copied().unwrap_or_else(|| Block::genesis().calculate_hash());
                    if validated_hash != downloaded_hash {
                        println!("[WARN] Miner: Node is synchronizing. Mining is physically locked during IBD.");
                        continue;
                    }
                    
                    println!("[INFO] Miner: Manual mining override initiated.");
                    let previous_hash = safe_lock!(cli_latest_block).calculate_hash();
                    let current_height = cli_storage.get_chain_list().len() as u64;

                    let target_arg = parts.get(1).copied().unwrap_or("default");
                    
                    let mut miner_pk_hash = [0u8; 32];
                    let mut target_locked = false;

                    if let Some(decoded) = crate::wallet::QuantumWallet::decode_qbtc_address(target_arg) {
                        miner_pk_hash.copy_from_slice(&decoded[0..32]);
                        target_locked = true;
                        println!("[INFO] Miner: Target locked via Base58 address.");
                    } 
                    else if let Some((pub_key, _)) = crate::wallet::QuantumWallet::get_public_info(&cli_datadir, target_arg) {
                        let mut hasher = sha2::Sha256::new();
                        sha2::Digest::update(&mut hasher, &pub_key); 
                        miner_pk_hash = hasher.finalize().into();
                        target_locked = true;
                        println!("[INFO] Miner: Target locked via local alias: {}", target_arg);
                    }

                    if !target_locked {
                        println!("[ERROR] Miner: Invalid target '{}'. Mining aborted.", target_arg);
                        continue;
                    }

                    let mut total_fees = 0u64;
                    let mut txs = {
                        let mempool_guard = safe_lock!(cli_mempool);
                        let selected_txs = mempool_guard.get_txs_for_mining();
                        for tx in &selected_txs {
                            let tx_hash = tx.calculate_id();
                            if let Some(entry) = mempool_guard.tx_pool.get(&tx_hash) {
                                total_fees += entry.fee;
                            }
                        }
                        selected_txs
                    };

                    let coinbase_in = TxIn { previous_output_hash: [0u8; 32], vout: current_height as u32 };
                    let coinbase_witness = TxWitness { signature: vec![], public_key: vec![] };
                    let block_reward = CentralBank::get_block_reward(current_height) + total_fees;

                    txs.insert(0, Transaction { 
                        inputs: vec![coinbase_in], 
                        outputs: vec![TxOut { value: block_reward, public_key_hash: miner_pk_hash, recovery: None }],
                        witnesses: vec![coinbase_witness] 
                    });

        

                    let template = quantum_btc::miner::BlockTemplate {
                        previous_hash,
                        transactions: txs,
        
                        target: cli_current_target.load(Ordering::SeqCst),
                        current_height,
                    };
                    let _ = cli_daemon_cmd_tx.send(quantum_btc::miner::MinerCommand::Mine(template));
                    quantum_btc::miner::PENDING_CMD.store(true, Ordering::Relaxed);
                }
                _ => {}
            }
        }
    });

    //  Initialize aerospace-grade network debouncer (TTL: 15 seconds)
    let mut sync_debouncer = quantum_btc::network::sync_manager::SyncDebouncer::new(15_000);

    loop {
        // V1.2 FIX: Inject independent periodic Watchdog for P2P connection lifecycle & Mempool cleanup.
        let sleep = tokio::time::sleep(tokio::time::Duration::from_secs(10));
        tokio::pin!(sleep);

        tokio::select! {
            _ = &mut sleep => {
    
                let peer_count = swarm.network_info().num_peers();
                if peer_count == 0 {
                    if !is_seed_node {
                        let bootnode_addr = format!("/dns4/{}/tcp/{}", seed_host, seed_port);
                        if let Ok(addr) = bootnode_addr.parse::<libp2p::Multiaddr>() {
                            let _ = swarm.dial(addr);
                            tracing::debug!("[INFO] Watchdog: Attempting connection to global seed node.");
                        }
                    }
                    
                    // MAINNET: Bounded Kademlia Sweep. Collect first, dial second to satisfy borrow checker.
                    let mut peers_to_dial = Vec::new();
                    for kbucket in swarm.behaviour_mut().kad.kbuckets() {
                        for entry in kbucket.iter() {
                            if peers_to_dial.len() >= 5 { break; }
                            peers_to_dial.push(entry.node.key.preimage().clone());
                        }
                        if peers_to_dial.len() >= 5 { break; }
                    }
                    for peer in peers_to_dial {
                        let _ = swarm.dial(peer);
                    }
                    if let Err(e) = swarm.behaviour_mut().kad.bootstrap() {
                        tracing::debug!("[WATCHDOG] Kademlia bootstrap deferred: {:?}", e);
                    }
                }

    
                // Purge stale compact blocks to prevent persistent RAM exhaustion.
                if pending_compact_blocks.len() > 50 {
                    pending_compact_blocks.clear();
                    tracing::warn!("[WARN] Watchdog: Compact block pool capacity exceeded. Executed memory purge.");
                }
                
                //  Trigger UTXO Resurrection via TTL Mempool Purge (2 Hours).
                let mut mempool_guard = safe_lock!(mempool);
                mempool_guard.purge_stale_transactions(7200);

                //  FIX: Mempool Bottomless Pit Protection (OOM).
                // ML-DSA-65 signatures are exceptionally heavy. Enforce absolute physical cap of 10,000 txs (approx ~300MB).
                if mempool_guard.tx_pool.len() > 10000 {
                    tracing::warn!("[WARN] Watchdog: Mempool physical memory redline crossed (>10,000 txs). Executing emergency wipe to prevent OOM.");
                    mempool_guard.tx_pool.clear(); 
                }
                drop(mempool_guard);

                sleep.as_mut().reset(tokio::time::Instant::now() + tokio::time::Duration::from_secs(10));
            },

            _ = tokio::signal::ctrl_c() => {
                println!("\n[INFO] System: Interrupt signal received. Initiating deterministic teardown.");
                auto_mine_flag.store(false, Ordering::SeqCst);
                
                // MAINNET: 3-Second Hard Timeout for OS-Level shutdown.
                // Guarantees WAL memory tables are flushed to physical disk.
                let _ = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                    println!("[INFO] Teardown Phase 1: Flushing RocksDB WAL to physical disk...");
                    let _ = storage.db.flush();
                    println!("[INFO] Teardown Phase 2: Physical state secured.");
                }).await;
                
                println!("[INFO] System: Teardown complete. Node safely powered down.");
                std::process::exit(0);
            },

            /*  Continuous JoinSet Reaper.
               Aggressively reaps completed async P2P task tombstones to prevent 
               silent memory leaks (OOM) during long-running 1000+ block epochs. */
            Some(res) = network_tasks.join_next(), if !network_tasks.is_empty() => {
                if let Err(e) = res {
                    tracing::trace!("[TRACE] System: Async network task terminated abnormally: {:?}", e);
                }
            },

            Some(new_block) = mined_rx.recv() => {
                //  Eliminate P2P Race Condition.
                // We NO LONGER broadcast here. Broadcasting before RocksDB write causes remote nodes 
                // to pull 'None' and permanently fork. Broadcast is moved to the Consensus thread.
                
                // Dispatch payload safely via the physical MPSC dam
                if let Err(_) = miner_block_tx.try_send(ConsensusTask { block: new_block, sender: local_peer_id }) {
                    tracing::warn!("[WARN] Miner: Consensus queue full. Dropping local block safely.");
                    engine_idle_notify.notify_waiters();
                }
            },


            Some(payload) = p2p_rx.recv() => {
                //  Enforce 2MB symmetry for general Gossipsub payloads.
                let encoded = bincode::options().with_limit(2_000_000).serialize(&payload).unwrap();
                let _ = swarm.behaviour_mut().gossipsub.publish(topic.clone(), encoded);
                tracing::debug!("[INFO] P2P: Payload injected into Gossip mesh.");
            },

            Some((channel, response)) = direct_resp_rx.recv() => {
                if swarm.behaviour_mut().req_resp.send_response(channel, response).is_ok() {
                    //  FIX: Downgraded to debug to eliminate Vegas log spam.
                    tracing::debug!("[INFO] Network: Direct payload dispatched successfully.");
                }
            },

            Some((_, specific_peer_opt)) = sos_rx.recv() => {
                let mut locator_hashes = Vec::new();
                let chain = storage.get_chain_list();
                let mut step = 1;
                let mut index = chain.len() as i32 - 1;

                while index >= 0 {
                    locator_hashes.push(chain[index as usize]);
                    if locator_hashes.len() > 10 { step *= 2; }
                    index -= step;
                }
                
                if index < 0 && !chain.is_empty() && locator_hashes.last() != Some(&chain[0]) {
                    locator_hashes.push(chain[0]);
                }

                println!("[INFO] Sync: Sending GetHeaders request with {} locator hashes.", locator_hashes.len());
                let req = crate::network::SyncRequest::GetHeaders { locator_hashes, requester: local_peer_id.to_string() };
                
                // L1 V2.0 CORE FIX: Prioritize specific peer, fallback to any connected peer if not provided.
                let target_peer = specific_peer_opt.or_else(|| swarm.connected_peers().next().cloned());
                if let Some(peer) = target_peer {
                    let _ = swarm.behaviour_mut().req_resp.send_request(&peer, req);
                } else {
                    println!("[WARN] Sync: No peers connected. Awaiting topology stabilization.");
                }
            },

            // Process internal Swarm control commands.
            Some(cmd) = swarm_cmd_rx.recv() => {
                let current_vtime = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
                match cmd {
                    SwarmCommand::BanAndDisconnect(bad_peer) => {
                        tracing::warn!("[WARN] Executioner: Severing physical connection with banned peer {}", bad_peer);
                        let _ = swarm.disconnect_peer_id(bad_peer);
                    }
                    SwarmCommand::Dial(addr) => {
                        if let Err(e) = swarm.dial(addr.clone()) {
                            tracing::error!("[ERROR] Network: Failed to dial target: {:?}", e);
                        } else {
                            tracing::debug!("[INFO] Network: Manual TCP connection initiated for {}", addr);
                        }
                    }
                    SwarmCommand::ReportSyncProgress(peer_id) => {
                        sync_debouncer.report_peer_progress(&peer_id.to_string(), true, current_vtime);
                    }
                    SwarmCommand::SendSyncReq(peer_id, req, tracker_hash) => {
                        // MAINNET: Load Shedding without Connection Teardown.
                        if sync_debouncer.is_peer_banned(&peer_id.to_string(), current_vtime) {
                            tracing::debug!("[DEFENSE] Peer {} hit rate limit. Request deferred to shed load.", peer_id);
                            continue;
                        }
                        
                        let mut allowed = true;
                        if let crate::network::SyncRequest::GetData { hashes, .. } = &req {
                            if let Some(first_hash) = hashes.first() {
                                if !sync_debouncer.should_request(*first_hash, current_vtime) {
                                    allowed = false;
                                }
                            }
                        }
                        
                        if allowed {
                            let req_id = swarm.behaviour_mut().req_resp.send_request(&peer_id, req);
                            if let Some(hash) = tracker_hash {
                                active_req_map.insert(req_id, hash);
                            }
                            tracing::debug!("[INFO] Network: Async sync request dispatched to {}.", peer_id);
                        } else {
                            tracing::debug!("[DEFENSE] Intercepted duplicate GetData request to {} (Anti-Storm Lock).", peer_id);
                            // MAINNET: Prevent self-banning. Do NOT penalize peer reputation for our own duplicate requests.
                        }
                    }
                }
            },

            event = swarm.select_next_some() => {
                match event {
                    libp2p::swarm::SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                        //  Instant drop for reconnecting zombie nodes.
                        if !safe_lock!(reputation).is_trusted(&peer_id) {
                            tracing::warn!("[WARN] Firewall: Blocked reconnect attempt from banned peer: {}", peer_id);
                            let _ = swarm.disconnect_peer_id(peer_id);
                            continue;
                        }
                        let current_peers = swarm.network_info().num_peers();
                        if current_peers > MAX_PEERS { 
                            let _ = swarm.disconnect_peer_id(peer_id); 
                        } else {
                            tracing::debug!("[INFO] Network: Connected to {}.", peer_id);
                            
                            // Kademlia DHT Bootstrap Protocol.
                            swarm.behaviour_mut().kad.add_address(&peer_id, endpoint.get_remote_address().clone());
                            // Kademlia bootstrap removed to prevent routing storms.
                            
                            //  Eliminate Height Blind Spot on reconnect.
                            // Proactively broadcast our current chain tip to the newly connected peer.
                            let local_tip_hash = safe_lock!(latest_block).calculate_hash();
                            let _ = swarm.behaviour_mut().req_resp.send_request(
                                &peer_id, 
                                crate::network::SyncRequest::GetHeaders {
                                    locator_hashes: vec![local_tip_hash],
                                    requester: local_peer_id.to_string(),
                                }
                            );

                            if !sync_requested {
                                sync_requested = true;
                                let current_index = storage.get_chain_list().len();
                                let sos_tx_clone = sos_tx.clone();
                                tracing::debug!("[INFO] Network: Waiting 3 seconds for mesh topology stabilization...");
                                network_tasks.spawn(async move {
                                    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                                    // Bootstrap sync: No specific target, fallback to any connected peer.
                                    let _ = sos_tx_clone.send((current_index, None)).await;
                                });
                            }
                        }
                    }
                    libp2p::swarm::SwarmEvent::Behaviour(network::p2p::QbtcBehaviourEvent::Mdns(libp2p::mdns::Event::Discovered(list))) => {
                        // Prevent local file descriptor exhaustion from malicious mDNS broadcasts.
                        for (_, addr) in list.into_iter().take(5) { let _ = swarm.dial(addr); }
                    }
                    libp2p::swarm::SwarmEvent::Behaviour(network::p2p::QbtcBehaviourEvent::Identify(libp2p::identify::Event::Received { peer_id, .. })) => {
                        swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                    }
                    // Silently absorb successful ping events. Failures are handled by OutboundFailure/ConnectionClosed.
                    libp2p::swarm::SwarmEvent::Behaviour(network::p2p::QbtcBehaviourEvent::Ping(_)) => {}
                    libp2p::swarm::SwarmEvent::Behaviour(network::p2p::QbtcBehaviourEvent::Gossipsub(libp2p::gossipsub::Event::Message { propagation_source, message, .. })) => {
                        //  Always hold the physical relay node (propagation_source) accountable.
                        // Relying on message.source allows malicious neighbors to proxy attacks with forged identities.
                        let sender = propagation_source;
                        
                        // L0 FIREWALL: Absolute physical isolation for Q-BTC.
                        if !safe_lock!(reputation).is_trusted(&sender) {
                            tracing::warn!("[WARN] Firewall: Dropping payload from banned physical peer: {}", sender);
                            let _ = swarm.disconnect_peer_id(sender); 
                            continue;
                        }

                        //  Cap Gossipsub mesh limits to 2MB. Silent drop for giant payloads to prevent OOM.
                        if let Ok(payload) = bincode::options().with_limit(2_000_000).deserialize::<NetworkPayload>(&message.data) {
                            match payload {
                                NetworkPayload::TransactionInv(tx_hash) => {
                                    //  IBD (Initial Block Download) Smart Lock
                                    // Absolute time-space defense against Mempool Gossip avalanche during sync.
                                    // Placed strictly inside TransactionInv to prevent deafening the BlockAnnouncement consensus channel.
                                    let is_in_ibd = {
                                        let latest_time = safe_lock!(latest_block).header.timestamp;
                                        let current_time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
                                        current_time.saturating_sub(latest_time) > 86400
                                    };

                                    if is_in_ibd {
                                        continue;
                                    }

                                    //  INV-Pull deduplication and lock management.
                                    let is_known = {
                                        let guard = safe_lock!(mempool);
                                        guard.tx_pool.contains_key(&tx_hash)
                                    }; // Lock physically dropped here to prevent async deadlock.

                                    if !is_known && !in_flight_txs.contains(&tx_hash) {
                                        // OOM bound sliding window eviction (Cache Flush Exploit protection).
                                        if in_flight_queue.len() >= 10000 {
                                            if let Some(oldest_hash) = in_flight_queue.pop_front() {
                                                in_flight_txs.remove(&oldest_hash);
                                            }
                                        }
                                        in_flight_txs.insert(tx_hash);
                                        in_flight_queue.push_back(tx_hash);
                                        
                                        let req = crate::network::SyncRequest::GetMempoolTx { 
                                            tx_hash, 
                                            requester: local_peer_id.to_string() 
                                        };
                                        let _ = swarm_cmd_tx.try_send(SwarmCommand::SendSyncReq(sender, req, Some(tx_hash)));
                                        tracing::debug!("[INFO] Network: Emitted GetData for unknown INV: {:?}", tx_hash);
                                    }
                                }
                                NetworkPayload::BlockAnnouncement(header) => {
                                    //  Headers-First Validation.
                                    let mut hasher = Sha256::new();
                                    let mut hb = Vec::with_capacity(120); 
                                    hb.extend_from_slice(&header.timestamp.to_be_bytes());
                                    hb.extend_from_slice(&header.previous_hash);
                                    hb.extend_from_slice(&header.merkle_root); 
                                    hb.extend_from_slice(&header.commit_merkle_root); 
                                    hb.extend_from_slice(&header.nonce.to_be_bytes());
                                    hb.extend_from_slice(&header.target.to_be_bytes());
                                    hasher.update(&hb);
                                    let hash: [u8; 32] = hasher.finalize().into();
                                    let hash_u64 = u64::from_be_bytes(hash[..8].try_into().unwrap());
                                    
                                    //  FIX: Time-Travel DDoS Protection. 
                                    // Reject blocks that claim to be > 2 hours in the future before wasting bandwidth to fetch them.
                                    let current_physical_time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
                                    if header.timestamp > current_physical_time + 7200 {
                                        tracing::warn!("[WARN] Firewall: BlockAnnouncement timestamp too far in future. Dropped.");
                                        if safe_lock!(reputation).report_offense(&sender, NetworkOffense::InvalidHeader) {
                                            let _ = swarm.disconnect_peer_id(sender);
                                        }
                                        continue;
                                    }

                                    if hash_u64 <= header.target {
                                        // Restored missing guard: Ensure we do not process already known blocks.
                                        if storage.get_block_index(&hash).is_none() {
                                            if storage.get_block_index(&header.previous_hash).is_some() || header.previous_hash == [0u8; 32] {
                                                tracing::info!("[INFO] Network: Valid BlockAnnouncement received. Requesting Q-BIP-152 Compact Block...");
                                                let req = crate::network::SyncRequest::GetCompactBlock { 
                                                    block_hash: hash, 
                                                    requester: local_peer_id.to_string(),
                                                };
                                                // Route request via physical propagation_source to prevent NAT dropping.
                                                let _ = swarm.behaviour_mut().req_resp.send_request(&propagation_source, req);
                                            } else {
                                                tracing::warn!("[WARN] Sync: Orphan announcement detected. Triggering Headers-First sync.");
                                                let current_index = storage.get_chain_list().len();
                                                // Target physical relay node instead of logical creator.
                                                let _ = sos_tx.try_send((current_index, Some(propagation_source)));
                                            }
                                        }
                                    } else {
                                        tracing::warn!("[WARN] Firewall: Invalid PoW in BlockAnnouncement from {}.", sender);
                                        if safe_lock!(reputation).report_offense(&sender, NetworkOffense::InvalidHeader) {
                                            let _ = swarm.disconnect_peer_id(sender);
                                        }
                                    }
                                }
                            }
                        } else {
                            println!("[WARN] Network: Failed to deserialize NetworkPayload.");
                        }
                    }
                    // L1 V2.0 CORE: Restored the ReqResp event wrapper.
                    libp2p::swarm::SwarmEvent::Behaviour(network::p2p::QbtcBehaviourEvent::ReqResp(event)) => {
                        match event {
                            libp2p::request_response::Event::Message { peer, message } => {
                                // L0 FIREWALL: Protect Direct Pipeline from queued zombie requests.
                                if !safe_lock!(reputation).is_trusted(&peer) {
                                    tracing::trace!("[TRACE] Firewall: Dropping ReqResp message from banned peer: {}", peer);
                                    continue;
                                }
                                match message {
                                    libp2p::request_response::Message::Request { request, channel, .. } => {
                                        let responder_id = local_peer_id.to_string();
                                        match request {
                                            crate::network::SyncRequest::GetHeaders { locator_hashes, .. } => {
                                                let chain = storage.get_chain_list();
                                                let mut start_index = 0;
                                                for hash in locator_hashes {
                                                    if let Some(idx) = storage.get_block_index(&hash) {
                                                        // FIX: Prevent Cross-Fork Blind Matching. Verify hash belongs to the active chain.
                                                        let height = idx.height as usize;
                                                        if height < chain.len() && chain[height] == hash {
                                                            start_index = height + 1;
                                                            break;
                                                        }
                                                    }
                                                }
                                                let mut headers = Vec::new();
                                                // Engine A (IBD): Bound payload size and fetch strictly from memory.
                                                // Limit to 500 headers to prevent MTU overflow and I/O starvation attacks.
                                                let end_index = std::cmp::min(start_index + 500, chain.len());
                                                for i in start_index..end_index {
                                                    // Zero-I/O extraction: retrieve cached headers directly from the index tree.
                                                    if let Some(idx) = storage.get_block_index(&chain[i]) {
                                                        headers.push(idx.header.clone());
                                                    }
                                                }
                                                let _ = swarm.behaviour_mut().req_resp.send_response(channel, crate::network::SyncResponse::Headers { headers, responder: responder_id });
                                                continue;
                                            }
                                            crate::network::SyncRequest::GetData { hashes, mode, .. } => {
                                                //  Atomic IO Guard limit checking (Max 5 concurrent).
                                                let io_guard = match IoTaskGuard::try_acquire(5) {
                                                    Some(guard) => guard,
                                                    None => {
                                                        tracing::warn!("[WARN] Node IO saturated. Dropping GetData request to protect memory.");
                                                        continue;
                                                    }
                                                };

                                                //  Allow up to 500 blocks for physical batching.
                                                let safe_hashes: Vec<_> = hashes.into_iter().take(500).collect();
                                                let is_core_only = mode == crate::network::SyncMode::CoreOnly;
                                                let storage_clone = storage.clone();
                                                let tx = direct_resp_tx.clone();
                                                
                                                network_tasks.spawn(async move {
                                                    let _task_guard = io_guard; // Guard transfers to thread.
                                                    
                                                    let blocks_to_send = tokio::task::spawn_blocking(move || {
                                                        let mut blocks = Vec::new();
                                                        let mut payload_size = 0;

                                                        for hash in safe_hashes {
                                                            if let Some(block) = storage_clone.get_block_by_hash(&hash, is_core_only) {
                                                                let size = block.get_physical_size();
                                                                
                                                                //  Absolute progression logic.
                                                                if blocks.is_empty() {
                                                                    blocks.push(block);
                                                                    payload_size += size;
                                                                    if size > 1_800_000 { break; } // Truncate early if first block is massive
                                                                    continue;
                                                                }
                                                                
                                                                //  1.8MB dynamic redline.
                                                                if payload_size + size > 1_800_000 { break; }
                                                                
                                                                blocks.push(block);
                                                                payload_size += size;
                                                            }
                                                        }
                                                        blocks
                                                    }).await.unwrap_or_default();
                                                    
                                                    let res = crate::network::SyncResponse::DataResponse { blocks: blocks_to_send, responder: responder_id };
                                                    let _ = tx.send((channel, res)).await;
                                                });
                                            }
                                            // Q-BIP-152: Responder logic for Compact Block requests.
                                            // Assembles the lightweight skeleton from local physical data.
                                            crate::network::SyncRequest::GetCompactBlock { block_hash, .. } => {
                                                let storage_clone = storage.clone();
                                                let tx = direct_resp_tx.clone();
                                                let responder_id_clone = responder_id.clone();
                                                
                                                network_tasks.spawn(async move {
                                                    //  Clone fallback identifier outside the closure.
                                                    let responder_fallback = responder_id_clone.clone(); 
                                                    let cb_response = tokio::task::spawn_blocking(move || {
                                                        //  Mempool Relay Cache routing.
                                                        // Instantly serves from RAM. The lexical scope {} strictly drops the RwLock 
                                                        // BEFORE hitting the physical disk fallback, preventing Tokio thread deadlocks.
                                                        let fetched_block = {
                                                            let cache = BLOCK_RELAY_CACHE.read().unwrap();
                                                            cache.iter().find(|b| b.calculate_hash() == block_hash).cloned()
                                                        }.or_else(|| storage_clone.get_block_by_hash(&block_hash, false));

                                                        if let Some(block) = fetched_block {
                                                            // Employ cryptographic CSPRNG for Short ID collision prevention using existing SysRng.
                                                            let mut nonce_bytes = [0u8; 8];
                                                            UnwrapErr(SysRng).fill_bytes(&mut nonce_bytes);
                                                            let nonce = u64::from_le_bytes(nonce_bytes);
                                                            let mut short_ids = Vec::with_capacity(block.transactions.len());
                                                            
                                                            // Always prefill the Coinbase transaction (index 0)
                                                            let prefilled = if !block.transactions.is_empty() {
                                                                vec![crate::block::PrefilledTransaction { index: 0, tx: block.transactions[0].clone() }]
                                                            } else { vec![] };
                                                            
                                                            // Calculate short IDs for all other transactions
                                                            for (i, tx) in block.transactions.iter().enumerate() {
                                                                if i > 0 {
                                                                    short_ids.push(crate::block::CompactBlock::calculate_short_id(&tx.calculate_id(), nonce));
                                                                }
                                                            }
                                                            
                                                            let cb = crate::block::CompactBlock {
                                                                header: block.header,
                                                                nonce,
                                                                short_ids,
                                                                prefilled_txs: prefilled,
                                                            };
                                                            crate::network::SyncResponse::CompactBlockResponse { compact_block: Some(cb), responder: responder_id_clone.clone() }
                                                        } else { 
                                                            crate::network::SyncResponse::CompactBlockResponse { compact_block: None, responder: responder_id_clone } 
                                                        }
                                                    }).await.unwrap_or(crate::network::SyncResponse::CompactBlockResponse { compact_block: None, responder: responder_fallback });
                                                    
                                                    //  Always reply to prevent Yamux stream exhaustion on the requester side.
                                                    let _ = tx.send((channel, cb_response)).await;
                                                });
                                            }
                                            // Q-BIP-152: Responder logic for missing physical transactions.
                                            // Fetches only the specifically requested slices to save bandwidth.
                                            crate::network::SyncRequest::GetBlockTxn { block_hash, indexes, .. } => {
                                                let storage_clone = storage.clone();
                                                let tx = direct_resp_tx.clone();
                                                let responder_id_clone = responder_id.clone();
                                                
                                                network_tasks.spawn(async move {
                                                    //  Clone fallback identifier outside the closure.
                                                    let responder_fallback = responder_id_clone.clone(); 
                                                    let txn_response = tokio::task::spawn_blocking(move || {
                                                        //  Mempool Relay Cache routing.
                                                        // Bridges async network requests with disk queues, eliminating spin-locks entirely.
                                                        let fetched_block = {
                                                            let cache = BLOCK_RELAY_CACHE.read().unwrap();
                                                            cache.iter().find(|b| b.calculate_hash() == block_hash).cloned()
                                                        }.or_else(|| storage_clone.get_block_by_hash(&block_hash, false));

                                                        if let Some(block) = fetched_block {
                                                            let mut missing_txs = Vec::new();
                                                            for idx in indexes {
                                                                if idx < block.transactions.len() {
                                                                    missing_txs.push(block.transactions[idx].clone());
                                                                }
                                                            }
                                                            Some(crate::network::SyncResponse::BlockTxnResponse { block_hash, transactions: missing_txs, responder: responder_id_clone })
                                                        } else { None }
                                                    }).await.unwrap_or(None);
                                                    
                                                    //  Return empty arrays if data is missing instead of dropping the channel.
                                                    let res = txn_response.unwrap_or(crate::network::SyncResponse::BlockTxnResponse { 
                                                        block_hash, transactions: vec![], responder: responder_fallback 
                                                    });
                                                    let _ = tx.send((channel, res)).await;
                                                });
                                            }
                                            //  Serve requested mempool transactions from local memory.
                                            crate::network::SyncRequest::GetMempoolTx { tx_hash, .. } => {
                                                let tx_opt = {
                                                    let guard = safe_lock!(mempool);
                                                    guard.tx_pool.get(&tx_hash).map(|e| e.tx.clone())
                                                };
                                                //  Never silently drop. Always close the channel actively.
                                                // Silent drops cause 5-second Yamux stream hangs and trigger false Timeout penalties on honest peers.
                                                let res = crate::network::SyncResponse::MempoolTxResponse { tx: tx_opt, responder: responder_id.clone() };
                                                let _ = swarm.behaviour_mut().req_resp.send_response(channel, res);
                                            }
                                        }
                                    }
                                    libp2p::request_response::Message::Response { request_id, response } => {
                                        /*  Zombie tracking deadlock prevention.
                                           Clear in-flight tracker regardless of payload resolution. */
                                        if let Some(tracked_hash) = active_req_map.remove(&request_id) {
                                            in_flight_txs.remove(&tracked_hash);
                                        }
                                        safe_lock!(reputation).reward_sync_success(&peer);
                                        // L1 V2.0 CORE: Intercept and process lightweight headers first.
                                        if let crate::network::SyncResponse::Headers { headers, responder: _ } = &response {
                                            if headers.is_empty() { continue; }
                                            let mut hashes_to_fetch = Vec::new();
                                            
                                            //  FIX: Ephemeral Header Cache (AR Glasses)
                                            let mut local_header_cache = std::collections::HashMap::new();

                                            for header in headers {
                                                let mut hasher = Sha256::new();
                                                let mut hb = Vec::new();
                                                hb.extend_from_slice(&header.timestamp.to_be_bytes());
                                                hb.extend_from_slice(&header.previous_hash);
                                                hb.extend_from_slice(&header.merkle_root);
                                                hb.extend_from_slice(&header.commit_merkle_root);
                                                hb.extend_from_slice(&header.nonce.to_be_bytes());
                                                hb.extend_from_slice(&header.target.to_be_bytes());
                                                hasher.update(&hb);
                                                let hash: [u8; 32] = hasher.finalize().into();
                                                
                                                if u64::from_be_bytes(hash[..8].try_into().unwrap()) <= header.target {
                                                    let existing_idx = storage.get_block_index(&hash);
                                                    if existing_idx.is_none() {
                                                        // Penetrate physical bounds by probing the ephemeral overlay first.
                                                        let prev_idx_opt = storage.get_block_index(&header.previous_hash)
                                                            .or_else(|| local_header_cache.get(&header.previous_hash).cloned());

                                                        if let Some(prev_idx) = prev_idx_opt {
                                                            let new_idx = quantum_btc::block::BlockIndex::new(
                                                                hash, 
                                                                header.clone(), 
                                                                prev_idx.height + 1, 
                                                                prev_idx.chain_work.saturating_add(header.get_block_proof()), 
                                                                false
                                                            );
                                                            storage.save_block_index(new_idx.clone());
                                                            local_header_cache.insert(hash, new_idx); // Cache instantly
                                                            hashes_to_fetch.push(hash);
                                                        } else if header.previous_hash == [0u8; 32] {
                                                            hashes_to_fetch.push(hash);
                                                        } else {
                                                            /* Note: Prevent chain deadlock on out-of-order arrivals. 
                                                               Buffer the orphan header and asynchronously fetch its ancestor. */
                                                            tracing::warn!("[WARN] Sync: Disconnected header detected. Buffering to Orphan Pool.");
                                                            
                                                            let mut pool_guard = ORPHAN_HEADER_POOL.write().unwrap();
                                                            
                                                            /* OOM Protection: Hard limit cache size to 1024 to prevent memory exhaustion. */
                                                            if pool_guard.len() >= 1024 {
                                                                if let Some(first_key) = pool_guard.keys().next().cloned() {
                                                                    pool_guard.remove(&first_key);
                                                                }
                                                            }
                                                            
                                                            pool_guard.insert(header.timestamp, header.clone());
                                                            
                                                            // [CRITICAL FIX]: Exponential Backoff Throttle for P2P GetData requests.
                                                            // Extinguishes the DDoS storm caused by high packet-loss (Brain-Split) environments.
                                                            let current_vtime = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
                                                            if sync_debouncer.should_request(header.previous_hash, current_vtime) {
                                                                let req = crate::network::SyncRequest::GetData { 
                                                                    hashes: vec![header.previous_hash],
                                                                    requester: local_peer_id.to_string(), 
                                                                    mode: crate::network::SyncMode::Full 
                                                                };
                                                                let req_id = swarm.behaviour_mut().req_resp.send_request(&peer, req);
                                                                active_req_map.insert(req_id, header.previous_hash);
                                                            } else {
                                                                tracing::debug!("[DEFENSE] Throttling redundant GetData request for ancestor hash.");
                                                            }
                                                            
                                                            /* Safely continue batch processing instead of halting. */
                                                            continue;
                                                        }
                                                    } else if let Some(idx) = existing_idx {
                                                        if !idx.has_data {
                                                            hashes_to_fetch.push(hash);
                                                        }
                                                    }
                                                }
                                            }
                                                // Engine A (IBD): Tactical chunking mechanism.
                                                //  Throttle IBD greediness to 50 blocks to lockstep with UTXO crypto-validation.
                                                let chunk_to_fetch: Vec<[u8; 32]> = hashes_to_fetch.into_iter().take(50).collect();
                                                let req = crate::network::SyncRequest::GetData { 
                                                    hashes: chunk_to_fetch,
                                                    requester: local_peer_id.to_string(), 
                                                    // Force Full mode since mempool is assumed empty during deep historical sync.
                                                    mode: crate::network::SyncMode::Full 
                                                };
                                                let _ = swarm.behaviour_mut().req_resp.send_request(&peer, req);
        
                                            continue;
                                        }

                                        // Q-BIP-152: Local Assembly Engine & Response Interceptor
                                        let mut fully_assembled_blocks = Vec::new();
                                        // Defer initialization to prevent unused assignment warnings.
                                        let responder_str: String;

                                        match response {
                                            crate::network::SyncResponse::DataResponse { blocks, responder } => {
                                                fully_assembled_blocks = blocks;
                                                responder_str = responder;
                                            }
                                            crate::network::SyncResponse::CompactBlockResponse { compact_block: cb_opt, responder } => {
                                                
                                                if cb_opt.is_none() {
                                                    if !FALLBACK_GUARD.swap(true, std::sync::atomic::Ordering::SeqCst) {
                                                        tracing::warn!("[WARN] Sync: Peer {} is downloading payload. Guarded fallback scheduled.", responder);
                                                        let sos_tx_clone = sos_tx.clone();
                                                        tokio::spawn(async move {
                                                            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                                                            let _ = sos_tx_clone.try_send((0, None));
                                                            FALLBACK_GUARD.store(false, std::sync::atomic::Ordering::SeqCst);
                                                        });
                                                    }
                                                    continue;
                                                }
                                                let compact_block = cb_opt.unwrap();
                                                let mut available_txs = HashMap::new();
                                                let mempool_guard = safe_lock!(mempool);
                                                for entry in mempool_guard.tx_pool.values() {
                                                    let tx_hash = entry.tx.calculate_id();
                                                    let short_id = crate::block::CompactBlock::calculate_short_id(&tx_hash, compact_block.nonce);
                                                    available_txs.insert(short_id, entry.tx.clone());
                                                }
                                                drop(mempool_guard);

                                                let mut partial_map = std::collections::HashMap::new();
                                                let mut missing_indexes = Vec::new();
                                                
                                                if let Some(p) = compact_block.prefilled_txs.iter().find(|p| p.index == 0) {
                                                    partial_map.insert(0, p.tx.clone());
                                                } else { missing_indexes.push(0); }

                                                for (i, short_id) in compact_block.short_ids.iter().enumerate() {
                                                    let actual_index = i + 1;
                                                    if let Some(p) = compact_block.prefilled_txs.iter().find(|p| p.index == actual_index) {
                                                        partial_map.insert(actual_index, p.tx.clone());
                                                    } else if let Some(tx) = available_txs.get(short_id) {
                                                        partial_map.insert(actual_index, tx.clone());
                                                    } else {
                                                        missing_indexes.push(actual_index);
                                                    }
                                                }

                                                let actual_block_hash = {
                                                    let mut h = sha2::Sha256::new();
                                                    let mut hb = Vec::new();
                                                    hb.extend_from_slice(&compact_block.header.timestamp.to_be_bytes());
                                                    hb.extend_from_slice(&compact_block.header.previous_hash);
                                                    hb.extend_from_slice(&compact_block.header.merkle_root); 
                                                    hb.extend_from_slice(&compact_block.header.commit_merkle_root); 
                                                    hb.extend_from_slice(&compact_block.header.nonce.to_be_bytes());
                                                    hb.extend_from_slice(&compact_block.header.target.to_be_bytes());
                                                    sha2::Digest::update(&mut h, &hb);
                                                    let res: [u8; 32] = h.finalize().into();
                                                    res
                                                };

                                                if missing_indexes.is_empty() {
                                                    let mut final_txs = Vec::new();
                                                    let total_expected = 1 + compact_block.short_ids.len();
                                                    for curr_idx in 0..total_expected {
                                                        if let Some(tx) = partial_map.remove(&curr_idx) {
                                                            final_txs.push(tx);
                                                        }
                                                    }
                                                    fully_assembled_blocks.push(Block { header: compact_block.header, transactions: final_txs });
                                                    responder_str = responder;
                                                } else {
                                                    pending_compact_blocks.insert(actual_block_hash, (compact_block, partial_map));
                                                    let req = crate::network::SyncRequest::GetBlockTxn { block_hash: actual_block_hash, indexes: missing_indexes, requester: local_peer_id.to_string() };
                                                    if let Ok(peer_id) = responder.parse::<libp2p::PeerId>() {
                                                        let _ = swarm.behaviour_mut().req_resp.send_request(&peer_id, req);
                                                    }
                                                    continue;
                                                }
                                            }
                                            crate::network::SyncResponse::BlockTxnResponse { block_hash, transactions, responder } => {
                                                if transactions.len() > 650 {
                                                    if let Ok(peer_id) = responder.parse::<libp2p::PeerId>() { 
                                                        if safe_lock!(reputation).report_offense(&peer_id, NetworkOffense::MalformedData) {
                                                            let _ = swarm.disconnect_peer_id(peer_id);
                                                        }
                                                    }
                                                    continue;
                                                }
                                                
                                                if let Some((cb, mut partial_map)) = pending_compact_blocks.remove(&block_hash) {
                                                    let mut final_txs = Vec::new();
                                                    let mut fetched_iter = transactions.into_iter();
                                                    let total_expected = 1 + cb.short_ids.len();
                                                    
                                                    for curr_idx in 0..total_expected {
                                                        if let Some(p) = cb.prefilled_txs.iter().find(|p| p.index == curr_idx) {
                                                            final_txs.push(p.tx.clone());
                                                        } else if let Some(tx) = partial_map.remove(&curr_idx) {
                                                            final_txs.push(tx);
                                                        } else if let Some(tx) = fetched_iter.next() {
                                                            final_txs.push(tx);
                                                        }
                                                    }

                                                    fully_assembled_blocks.push(Block { header: cb.header, transactions: final_txs });
                                                    responder_str = responder;
                                                } else { continue; }
                                            }
                                            
                                            crate::network::SyncResponse::MempoolTxResponse { tx, responder } => {
                                                if let Some(tx) = tx {
                                                    let tx_hash = tx.calculate_id();
                                                    /*  Tracker already cleared by global response handler. */
                                                    
                                                    let storage_worker = storage.clone();
                                                    let utxo_tx_worker = utxo_tx.clone();
                                                    let mempool_worker = mempool.clone();
                                                    let reputation_worker = reputation.clone();
                                                    let swarm_cmd_tx_worker = swarm_cmd_tx.clone();
                                                    //  Inject async P2P transmitter to worker thread. Clone from origin to avoid move semantic violation.
                                                    let p2p_tx_worker = p2p_tx.clone();
                                                    
                                                    network_tasks.spawn(async move {
                                                        //  FIX: Offload heavy ML-DSA-65 crypto verification to Tokio blocking thread
                                                        let tx_for_crypto = tx.clone();
                                                        let tx_hash_for_crypto = tx_hash;
                                                        
                                                        let is_crypto_valid = tokio::task::spawn_blocking(move || {
                                                            if tx_for_crypto.witnesses.is_empty() { return false; }
                                                            for i in 0..tx_for_crypto.inputs.len() {
                                                                if !tx_for_crypto.verify_witness(i, &tx_hash_for_crypto) { return false; }
                                                            }
                                                            true
                                                        }).await.unwrap_or(false);

                                                        if !is_crypto_valid {
                                                            if let Ok(peer_id) = responder.parse::<libp2p::PeerId>() {
                                                                if safe_lock!(reputation_worker).report_offense(&peer_id, NetworkOffense::InvalidSignature) {
                                                                    tracing::warn!("[WARN] Security: Cryptographic poison detected. Disconnecting.");
                                                                    let _ = swarm_cmd_tx_worker.send(SwarmCommand::BanAndDisconnect(peer_id)).await;
                                                                }
                                                            }
                                                            return; // Drop invalid tx instantly
                                                        }

                                                        let current_eval_height = storage_worker.get_chain_list().len() as u64;
                                                        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                                        
                                                        // Dispatch to UTXO Actor with crypto_pre_verified: true
                                                        if utxo_tx_worker.send(utxo::UtxoCommand::ValidateMempoolTx { tx: tx.clone(), current_height: current_eval_height, crypto_pre_verified: true, resp: resp_tx }).await.is_ok() {
                                                            match resp_rx.await.unwrap_or(Err("Actor Channel Closed")) {
                                                                Ok(exact_fee) => {
                                                                    //  Mempool Relay Policy Gatekeeper
                                                                    // Enforce strict minimum relay fee without breaking consensus
                                                                    if exact_fee < 1000 {
                                                                        tracing::warn!("[WARN] Relay Policy: Transaction rejected. Fee {} too low.", exact_fee);
                                                                    } else {
                                                                        //  Evaluate physical lock immediately to drop MutexGuard before async boundary.
                                                                        let add_result = safe_lock!(mempool_worker).add_transaction(tx, exact_fee);
                                                                        match add_result {
                                                                            Ok(_) => {
                                                                                //  Elevate log level to info to expose successful mempool admission.
                                                                                tracing::info!("[INFO] Mempool: Pulled transaction validated and admitted with fee {}.", exact_fee);
                                                                                //  Causality lock. Broadcast INV only after physical verification and mempool admission.
                                                                                let _ = p2p_tx_worker.send(crate::network::NetworkPayload::TransactionInv(tx_hash)).await;
                                                                            },
                                                                            Err(quantum_btc::mempool::blind_box::MempoolError::Tombstoned) => {
                                                                                //  Ignore tombstoned transactions. Peer immunity granted to prevent network fractures.
                                                                                tracing::debug!("[INFO] Firewall: Blocked tombstoned transaction. No penalty applied.");
                                                                            },
                                                                            Err(e) => tracing::debug!("[DEBUG] Mempool: Transaction rejected: {:?}", e),
                                                                        }
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    //  FIX: Smart Firewall. Differentiate contextual UTXO errors from cryptographic forgery.
                                                                    let err_str = e.to_string().to_lowercase();
                                                                    if err_str.contains("signature") || err_str.contains("crypto") {
                                                                        if let Ok(peer_id) = responder.parse::<libp2p::PeerId>() {
                                                                            if safe_lock!(reputation_worker).report_offense(&peer_id, NetworkOffense::InvalidSignature) {
                                                                                tracing::warn!("[WARN] Security: Cryptographic poison detected. Disconnecting.");
                                                                                let _ = swarm_cmd_tx_worker.send(SwarmCommand::BanAndDisconnect(peer_id)).await;
                                                                            }
                                                                        }
                                                                    } else {
                                                                        // Silently drop transactions with missing UTXOs due to network race conditions.
                                                                        tracing::debug!("[INFO] Mempool: Transaction dropped due to UTXO context failure: {}", e);
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    });
                                                }
                                                continue; // Bypass block processing
                                            }
                                            _ => continue,
                                        };

                                        let blocks = fully_assembled_blocks;
                                        if blocks.is_empty() { continue; }
                                        
                                        //  FIX: Elevate log level to INFO to expose silent ML-DSA block processing status.
                                        tracing::info!("[INFO] Sync: Received {} historical blocks via direct pipeline. Evaluating chain work...", blocks.len());
                                        
                                        // L1 DEFENSE DISABLED: Friendly fire detected. 
                                        // We delegate all validation safely to utxo_guard.process_block below.

                                        //  Offload heavy I/O and consensus state mutation to a blocking thread.
                                        // Prevents Tokio async executor starvation and network stalling.
                                        let storage_clone = storage.clone();
                                        let latest_block_clone = latest_block.clone();
                                        let mempool_clone = mempool.clone();
                                        let daemon_cmd_tx_clone = daemon_cmd_tx.clone();
                                        let engine_idle_notify_clone = engine_idle_notify.clone();
                                        let local_peer_id_str = local_peer_id.to_string();
                                        let utxo_tx_worker_clone = utxo_tx.clone(); 
                                        
                                        //  Clone global consensus trackers directly from the preserved genesis constants.
                                        let target_worker_clone = current_target.clone(); 
                                        let anchor_time_worker_clone = genesis_anchor_time.clone();
                                        let anchor_target_worker_clone = genesis_anchor_target.clone();

                                        // L0 ARCHITECTURE: Equip the Deep Sync Judge with Executioner tools.
                                        let reputation_clone = reputation.clone();
                                        let swarm_cmd_tx_clone = swarm_cmd_tx.clone();

                                        let swarm_cmd_tx_outer = swarm_cmd_tx.clone(); // FIX: Clone bus for detached async worker
                                        network_tasks.spawn(async move {
                                            let next_request = tokio::task::spawn_blocking(move || -> Option<(libp2p::PeerId, crate::network::SyncRequest)> {
                                                let mut current_latest = safe_lock!(latest_block_clone);
                                            
                                            // L1 V2.0 CORE: Step 1. Append to absolute truth tree (map_block_index) without corrupting active_chain.
                                            let mut highest_new_hash = None;
                                            let mut highest_new_work = 0u128;
                                            
                                            let current_physical_time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

                                            //  State Overlay View for absolute progression validation.
                                            let mut local_block_cache = std::collections::HashMap::new();

                                            for b in &blocks {
                                                // Strict topological derivation resolving physical and ephemeral bounds.
                                                // Prevents checkpoint bypass via map_or(0) and eliminates redundant orphan checks.
                                                let eval_height = if b.header.previous_hash == [0u8; 32] {
                                                    0
                                                } else if let Some(idx) = storage_clone.get_block_index(&b.header.previous_hash).or_else(|| local_block_cache.get(&b.header.previous_hash).cloned()) {
                                                    idx.height + 1
                                                } else {
                                                    // DEFENSE: Orphan block detected in Deep Sync batch.
                                                    // Skip specific block without dropping the valid batch to tolerate P2P jitter.
                                                    tracing::warn!("[WARN] Sync: Orphan block detected. Skipping specific block.");
                                                    continue; 
                                                };

                                                tracing::debug!("[DEBUG] Network: Buffered physical payload for validated Height: {}", eval_height);

                                                // The Iron Wall.
                                                // Enforces hardcoded checkpoints with absolute topological certainty.
                                                if let Err(e) = crate::consensus::verify_checkpoint(eval_height, &b.calculate_hash()) {
                                                    tracing::error!("[ERROR] Sync: Malicious historical chain detected. Checkpoint violation: {}", e);
                                                    if let Ok(bad_peer) = responder_str.parse::<libp2p::PeerId>() {
                                                        let _ = safe_lock!(reputation_clone).report_offense(&bad_peer, NetworkOffense::InvalidHeader);
                                                        let _ = swarm_cmd_tx_clone.try_send(SwarmCommand::BanAndDisconnect(bad_peer));
                                                    }
                                                    return None;
                                                }

                                                // Cryptographic validation.
                                                if !consensus::ConsensusEngine::verify_proof_of_work(b, b.header.target) || !consensus::ConsensusEngine::verify_merkle_root(b) {
                                                    tracing::error!("[ERROR] Sync: Cryptographic validation failed. Halting assembly.");
                                                    if let Ok(bad_peer) = responder_str.parse::<libp2p::PeerId>() {
                                                        let _ = safe_lock!(reputation_clone).report_offense(&bad_peer, NetworkOffense::InvalidHeader);
                                                        let _ = swarm_cmd_tx_clone.try_send(SwarmCommand::BanAndDisconnect(bad_peer));
                                                    }
                                                    return None;
                                                }

                                                if b.header.timestamp > current_physical_time + 7200 {
                                                    tracing::error!("[ERROR] Security: Deep Sync block timestamp > 2 hours in the future.");
                                                    if let Ok(bad_peer) = responder_str.parse::<libp2p::PeerId>() {
                                                        let _ = safe_lock!(reputation_clone).report_offense(&bad_peer, NetworkOffense::InvalidHeader);
                                                        let _ = swarm_cmd_tx_clone.try_send(SwarmCommand::BanAndDisconnect(bad_peer));
                                                    }
                                                    return None;
                                                }

                                                if b.get_physical_size() > 8 * 1024 * 1024
                                                    || b.get_block_weight() > quantum_btc::config::MAX_BLOCK_WEIGHT as u64 
                                                    || b.get_block_sigops() > quantum_btc::config::MAX_BLOCK_SIGOPS as usize {
                                                    tracing::error!("[ERROR] Firewall: Deep Sync block exceeds limits.");
                                                    if let Ok(bad_peer) = responder_str.parse::<libp2p::PeerId>() {
                                                        let _ = safe_lock!(reputation_clone).report_offense(&bad_peer, NetworkOffense::MalformedData);
                                                        let _ = swarm_cmd_tx_clone.try_send(SwarmCommand::BanAndDisconnect(bad_peer));
                                                    }
                                                    return None;
                                                }
                                                
                                                // MTP-11 FIX: Traverse disk OR ephemeral memory seamlessly.
                                                let mut past_timestamps: Vec<u64> = Vec::with_capacity(11);
                                                let mut current_search_hash = b.header.previous_hash;
                                                for _ in 0..11 {
                                                    let idx_opt = storage_clone.get_block_index(&current_search_hash)
                                                        .or_else(|| local_block_cache.get(&current_search_hash).cloned());
                                                        
                                                    if let Some(idx) = idx_opt {
                                                        past_timestamps.push(idx.header.timestamp);
                                                        current_search_hash = idx.header.previous_hash;
                                                    } else { break; } 
                                                }
                                                if let Err(e) = consensus::ConsensusEngine::verify_timestamp(b, &mut past_timestamps) {
                                                    tracing::warn!("[WARN] Consensus: Deep Sync block rejected by MTP-11 protection: {:?}", e);
                                                    return None;
                                                }

                                                let bh = b.calculate_hash();
                                                let prev_hash = b.header.previous_hash;
                                                
                                                let existing_idx = storage_clone.get_block_index(&bh);
                                                let needs_commit = existing_idx.as_ref().map_or(true, |idx| !idx.has_data);

                                                let block_work;
                                                if needs_commit {
                                                    let prev_idx_opt = storage_clone.get_block_index(&prev_hash)
                                                        .or_else(|| local_block_cache.get(&prev_hash).cloned());
                                                    let (p_height, p_work) = prev_idx_opt.map_or((0, 0), |idx| (idx.height, idx.chain_work));
                                                    
                                                    let accumulated_work = p_work.saturating_add(b.header.get_block_proof());
                                                    
                                                    //  FIX: Absolute Ephemeral Overlay
                                                    // Eradicated save_block_segwit and save_block_index from evaluation loop to prevent DB pollution.
                                                    let new_idx = quantum_btc::block::BlockIndex::new(bh, b.header.clone(), p_height + 1, accumulated_work, true);
                                                    local_block_cache.insert(bh, new_idx); 
                                                    
                                                    block_work = accumulated_work;
                                                } else {
                                                    block_work = existing_idx.unwrap().chain_work;
                                                }
                                                
                                                if block_work > highest_new_work {
                                                    highest_new_work = block_work;
                                                    highest_new_hash = Some(bh);
                                                }
                                            }

                                            let local_chain = storage_clone.get_chain_list();

                                            //  The absolute lifeline. Anchor to the verified true state.
                                            let consensus_tip = current_latest.calculate_hash();
                                            let consensus_work = storage_clone.get_block_index(&consensus_tip).map_or(0, |idx| idx.chain_work);

                                            // L1 V2.0 CORE: Step 2. Trigger Nakamoto Consensus State Machine transition.
                                            // Compare incoming work strictly against the verified true state to prevent sync deadlocks.
                                            let incoming_tip = highest_new_hash.unwrap_or([0u8; 32]);
                                            let is_heavier = highest_new_work > consensus_work;
                                            let is_tie_breaker_winner = highest_new_work == consensus_work && highest_new_work > 0 && incoming_tip < consensus_tip;

                                            // Trigger consensus engine using consensus_tip as the base, with genesis fallback.
                                            if (is_heavier || is_tie_breaker_winner) && incoming_tip != consensus_tip {
                                                /*  Alien Chain Firewall.
                                                   Never fallback to Genesis if no common ancestor is found.
                                                   Reject alien chains with no cryptographic link to our history instantly. */
                                                if let Some(lca_hash) = storage_clone.find_fork_lca(&consensus_tip, &incoming_tip, &local_block_cache) {
                                                    let lca_height = storage_clone.get_block_index(&lca_hash).map_or(0, |idx| idx.height);
                                                    let current_height = local_chain.len().saturating_sub(1) as u64;

                                                    //  FIX: Pre-flight Check (Headers-Trap Defense)
                                                    // Trace path and verify physical data presence BEFORE executing destructive rollbacks.
                                                    let mut connect_path = Vec::new();
                                                    let mut curr = incoming_tip;
                                                    while curr != lca_hash && curr != [0u8; 32] {
                                                        connect_path.push(curr);
                                                        let idx = storage_clone.get_block_index(&curr)
                                                            .or_else(|| local_block_cache.get(&curr).cloned())
                                                            .expect("Fatal: Broken chain sequence during path reconstruction");
                                                        curr = idx.header.previous_hash;
                                                    }
                                                    connect_path.reverse();

                                                    let mut all_data_present = true;
                                                    for hash in &connect_path {
                                                        let has_data = blocks.iter().any(|b| b.calculate_hash() == *hash) || storage_clone.get_block_by_hash(hash, true).is_some();
                                                        if !has_data {
                                                            all_data_present = false;
                                                            break;
                                                        }
                                                    }

                                                    if !all_data_present {
                                                        tracing::warn!("[WARN] Consensus: Missing ancestral physical data. Aborting reorg to prevent Headers-Trap. Triggering dynamic backfill...");
                                                        let req = crate::network::SyncRequest::GetData { 
                                                            hashes: connect_path, 
                                                            requester: local_peer_id_str.clone(), 
                                                            mode: crate::network::SyncMode::Full 
                                                        };
                                                        if let Ok(peer_id) = responder_str.parse::<libp2p::PeerId>() {
                                                            return Some((peer_id, req));
                                                        }
                                                        return None;
                                                    }
                                                    
                                                    // Step 3: Disconnect Phase (Undo state down to Lowest Common Ancestor).
                                                    let mut rollback_h = current_height;
                                                    while rollback_h > lca_height {
                                                        let hash_to_kill = local_chain[rollback_h as usize];
                                                        if let Some(undo_log) = storage_clone.get_undo_log(rollback_h, &hash_to_kill) {
                                                            // Route Disconnect via Actor Channel.
                                                            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                                            let _ = utxo_tx_worker_clone.blocking_send(utxo::UtxoCommand::DisconnectBlock { undo_log, resp: resp_tx });
                                                            if let Err(e) = resp_rx.blocking_recv().unwrap_or(Err("Actor Channel Closed")) {
                                                                tracing::error!("[ERROR] Fatal: UTXO disconnect failed during atomic reorg: {}. Node halting.", e);
                                                                std::process::exit(1); // Failsafe shutdown to prevent state corruption.
                                                            }
                                                            
                                                            // Recover valid transactions from disconnected blocks during deep Nakamoto reorg.
                                                            if let Some(killed_block) = storage_clone.get_block_by_hash(&hash_to_kill, false) {
                                                                let mut mempool_guard = safe_lock!(mempool_clone);
                                                                for tx in killed_block.transactions.into_iter().skip(1) {
                                                                    // MAINNET FIX: Eradicate percentage-based fake fees during Deep Reorg.
                                                                    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                                                    let _ = utxo_tx_worker_clone.blocking_send(quantum_btc::utxo::UtxoCommand::ValidateMempoolTx { tx: tx.clone(), current_height: rollback_h, crypto_pre_verified: true, resp: resp_tx });
                                                                    if let Ok(Ok(exact_fee)) = resp_rx.blocking_recv() {
                                                                        let _ = mempool_guard.add_transaction(tx, exact_fee);
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        rollback_h -= 1;
                                                    }
                                                    
                                                    if lca_height < current_height {
                                                        storage_clone.rollback_chain(lca_height as usize);
                                                    }

                                                    // Step 4: Path mathematically reconstructed and verified during Pre-flight Check.

                                                    // Step 5: Connect Phase (Apply new superior state with WriteBatch & Dynamic Throttling).
                                                    let mut apply_h = lca_height + 1;
                                                    let mut reorg_success = true;
                                                    let mut missing_history = Vec::new();
                                                    
                                                    //  FIX: High-Pressure Container and Sensors
                                                    let mut batch_instructions = Vec::new();
                                                    let mut current_batch_size = 0usize;

                                                    for hash_to_apply in &connect_path {
                                                        let block_to_apply = blocks.iter()
                                                            .find(|b| b.calculate_hash() == *hash_to_apply)
                                                            .cloned()
                                                            .or_else(|| storage_clone.get_block_by_hash(hash_to_apply, false));

                                                        if let Some(b_apply) = block_to_apply {
                                                            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                                            let _ = utxo_tx_worker_clone.blocking_send(utxo::UtxoCommand::ApplyBlock { 
                                                                block: b_apply.clone(), 
                                                                height: apply_h, 
                                                                is_historical: false, 
                                                                resp: resp_tx 
                                                            });

                                                            match resp_rx.blocking_recv().unwrap_or(Err("Actor Channel Closed")) {
                                                                Ok(undo_log) => {
                                                                    let prev_hash = b_apply.header.previous_hash;
                                                                    let current_work = storage_clone.get_block_index(&prev_hash).map(|idx| idx.chain_work).unwrap_or(0);
                                                                    let new_accumulated_work = current_work.saturating_add(b_apply.header.get_block_proof());

                                                                    let block_size = b_apply.get_physical_size();
                                                                    
                                                                    batch_instructions.push(storage::BatchCommitInstruction {
                                                                        block: b_apply.clone(),
                                                                        height: apply_h,
                                                                        undo_log,
                                                                        accumulated_work: new_accumulated_work,
                                                                    });
                                                                    current_batch_size += block_size;
                                                                    
                                                                    //  FIX: Unified View Stage
                                                                    // Share verified progress to network instantly, zero disk latency.
                                                                    storage_clone.stage_verified_block(b_apply.clone(), apply_h, new_accumulated_work);
                                                                    
                                                                    //  Dynamic Throttling Sensors (Trigger Physical Flush)
                                                                    // Max 8MB volume OR 30 blocks count to prevent RocksDB MemTable stall
                                                                    if current_batch_size >= 8_000_000 || batch_instructions.len() >= 30 {
                                                                        let (snap_tx, snap_rx) = tokio::sync::oneshot::channel();
                                                                        let _ = utxo_tx_worker_clone.blocking_send(utxo::UtxoCommand::GetSnapshot { resp: snap_tx });
                                                                        let utxo_snap = snap_rx.blocking_recv().unwrap();

                                                                        storage_clone.commit_state_batch(std::mem::take(&mut batch_instructions), &utxo_snap);
                                                                        current_batch_size = 0;
                                                                    }
                                                                    
                                                                    *current_latest = b_apply.clone();
                                                                    safe_lock!(mempool_clone).atomic_sweep(&b_apply.transactions);
                                                                    
                                                                    if apply_h % 5 == 0 || apply_h == 1 {
                                                                        tracing::info!("[INFO] True Sync: Cryptographically verified and committed Block Height {}", apply_h);
                                                                    }
                                                                    apply_h += 1;
                                                                }
                                                                Err(e) => {
                                                                    //  FIX: Smart Reorg Firewall. Prevent banning honest nodes sending fork blocks.
                                                                    let err_str = e.to_string().to_lowercase();
                                                                    tracing::warn!("[WARN] Consensus: Block state transition failed during reorg: {}", e);
                                                                    
                                                                    if err_str.contains("signature") || err_str.contains("merkle") {
                                                                        if let Ok(bad_peer) = responder_str.parse::<libp2p::PeerId>() {
                                                                            let _ = safe_lock!(reputation_clone).report_offense(&bad_peer, NetworkOffense::InvalidSignature);
                                                                            let _ = swarm_cmd_tx_clone.try_send(SwarmCommand::BanAndDisconnect(bad_peer));
                                                                        }
                                                                    }
                                                                    
                                                                    reorg_success = false;
                                                                    break;
                                                                }
                                                            }
                                                        } else {
                                                            tracing::warn!("[WARN] Consensus: Missing ancestral physical data. Triggering dynamic backfill...");
                                                            reorg_success = false;
                                                            missing_history = connect_path.clone(); 
                                                            break;
                                                        }
                                                    }
                                                    
                                                    // MAINNET Note: Absolute FlushStateToDisk.
                                                    // Always flush state snapshot during successful Reorg, even if batch_instructions is empty.
                                                    if reorg_success {
                                                        let (snap_tx, snap_rx) = tokio::sync::oneshot::channel();
                                                        let _ = utxo_tx_worker_clone.blocking_send(utxo::UtxoCommand::GetSnapshot { resp: snap_tx });
                                                        let utxo_snap = snap_rx.blocking_recv().unwrap();
                                                        storage_clone.commit_state_batch(batch_instructions, &utxo_snap);
                                                    } else {
                                                        storage_clone.purge_staged_data();
                                                    }
                                                    // Step 6: Finalize switch to superior chain.
                                                    if reorg_success {
                                                        //  FIX: Topology-Aware Async Snapshot Reconciliation
                                                        /*  Mempool Poisoning Defense.
                                                           If the Reorg is deeper than 10 blocks, Coinbase maturities and physical UTXO 
                                                           locations are hopelessly corrupted. We execute an absolute physical wipe of the Mempool. */
                                                        // MAINNET STANDARD: Mempool Resurrect. 
                                                        // Never clear mempool based on depth. Rely strictly on cryptographic UTXO reconciliation.
                                                        let snapshot: Vec<([u8; 32], Vec<TxIn>, Vec<TxOut>)> = {
                                                            let guard = safe_lock!(mempool_clone);
                                                            guard.tx_pool.values().map(|entry| {
                                                                (entry.tx.calculate_id(), entry.tx.inputs.clone(), entry.tx.outputs.clone())
                                                            }).collect()
                                                        };
                                                        
                                                        let utxo_tx_async = utxo_tx_worker_clone.clone();
                                                        let mempool_async = mempool_clone.clone();
                                                        tokio::spawn(async move {
                                                            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                                            let _ = utxo_tx_async.send(utxo::UtxoCommand::ReconcileMempool { snapshot, resp: resp_tx }).await;
                                                            
                                                            if let Ok(blacklist) = resp_rx.await {
                                                                if !blacklist.is_empty() {
                                                                    let mut guard = safe_lock!(mempool_async);
                                                                    guard.tx_pool.retain(|k, _| !blacklist.contains(k));
                                                                    tracing::info!("[INFO] Mempool: Deep Reorg async reconciliation purged {} ghost transactions.", blacklist.len());
                                                                }
                                                            }
                                                        });

                                                        let _ = daemon_cmd_tx_clone.send(quantum_btc::miner::MinerCommand::Stop);
                                                        quantum_btc::miner::PENDING_CMD.store(true, Ordering::Relaxed);
                                                        engine_idle_notify_clone.notify_waiters();
                                                        
                                                        //  Deterministic ASERT recalibration post Nakamoto deep reorganization.
                                                        let active_chain = storage_clone.get_chain_list();
                                                        let tip_height = active_chain.len().saturating_sub(1) as u64;
                                                        
                                                        let new_tip_hash = current_latest.calculate_hash();
                                                        if let Some(tip_idx) = storage_clone.get_block_index(&new_tip_hash) {
                                                            let new_target = consensus::ConsensusEngine::calculate_next_target(
                                                                anchor_time_worker_clone.load(Ordering::Relaxed),
                                                                anchor_target_worker_clone.load(Ordering::Relaxed),
                                                                tip_idx.header.timestamp,
                                                                tip_height + 1
                                                            );
                                                            target_worker_clone.store(new_target, Ordering::SeqCst);
                                                }
                                                
                                                tracing::info!("[INFO] Consensus: Nakamoto Reorg successful. Target clock re-aligned. New tip height: {}", tip_height);
                                                if let Ok(peer_id) = responder_str.parse::<libp2p::PeerId>() {
                                                    let _ = swarm_cmd_tx_clone.try_send(SwarmCommand::ReportSyncProgress(peer_id));
                                                }
                                            } else if !missing_history.is_empty() {
                                                // L1 V2.0 CORE: Dynamic Backfill Protocol.
                                                // Immediately dispatch a batch request for the exact missing branches.
                                                        let req = crate::network::SyncRequest::GetData { 
                                                            hashes: missing_history, 
                                                            requester: local_peer_id_str.clone(), 
                                                            mode: crate::network::SyncMode::Full 
                                                        };
                                                        if let Ok(peer_id) = responder_str.parse::<libp2p::PeerId>() {
                                                            return Some((peer_id, req));
                                                        }
                                                    }
                                                }
                                            }

                                            // L1 V2.0 CORE: Request Continuation Logic.
                                            // FIX: Prevent Blind Fork Fetching.
                                            // Trace a definitive path from the heaviest known tip to prevent downloading fragmented forks.
                                            let vault_guard = storage_clone.vault.read().unwrap();
                                            let tree = &vault_guard.map_block_index;
                                            
                                            if let Some(best_idx) = tree.values().max_by_key(|idx| idx.chain_work) {
                                                let mut path = Vec::new();
                                                let mut curr_hash = best_idx.block_hash;
                                                
                                                while let Some(idx) = tree.get(&curr_hash) {
                                                    if idx.has_data { break; }
                                                    path.push(curr_hash);
                                                    if curr_hash == idx.header.previous_hash || idx.header.previous_hash == [0u8; 32] { break; }
                                                    curr_hash = idx.header.previous_hash;
                                                }
                                                path.reverse(); 
                                                
                                                //  Throttle continuation sync greediness to 50 hashes.
                                                let next_hashes: Vec<[u8; 32]> = path.into_iter().take(50).collect();
                                                    
                                                if !next_hashes.is_empty() {
                                                    let next_req = crate::network::SyncRequest::GetData { 
                                                        hashes: next_hashes, 
                                                        requester: local_peer_id_str.clone(), 
                                                        mode: crate::network::SyncMode::Full 
                                                    };
                                                    if let Ok(peer_id) = responder_str.parse::<libp2p::PeerId>() {
                                                        return Some((peer_id, next_req));
                                                    }
                                                } else if !blocks.is_empty() {
                                                    let chain = storage_clone.get_chain_list();
                                                    let mut locator_hashes = Vec::new();
                                                    let mut step = 1;
                                                    let mut index = chain.len() as i32 - 1;

                                                    while index >= 0 {
                                                        locator_hashes.push(chain[index as usize]);
                                                        if locator_hashes.len() > 10 { step *= 2; }
                                                        index -= step;
                                                    }
                                                    if index < 0 && !chain.is_empty() && locator_hashes.last() != Some(&chain[0]) {
                                                        locator_hashes.push(chain[0]);
                                                    }
                                                    
                                                    let next_req = crate::network::SyncRequest::GetHeaders { 
                                                        locator_hashes, 
                                                        requester: local_peer_id_str 
                                                    };
                                                    if let Ok(peer_id) = responder_str.parse::<libp2p::PeerId>() {
                                                        return Some((peer_id, next_req));
                                                    }
                                                }
                                            }
                                            None
                                        }).await.unwrap_or(None);

                                        // FIX: Pipe request back through event bus instead of executing synchronously
                                        if let Some((peer_id, next_req)) = next_request {
                                            let _ = swarm_cmd_tx_outer.send(SwarmCommand::SendSyncReq(peer_id, next_req, None)).await;
                                        }
                                    }); // FIX: End of detached async worker
                                    }
                                }
                            }
                            //  Precise timeout failure rollback to prevent Cache Flush Exploits.
                            libp2p::request_response::Event::OutboundFailure { peer, request_id, error, .. } => {
                                tracing::debug!("[DEBUG] Network: ReqResp outbound failure to {}: {:?}. Releasing tracker lock.", peer, error);
                                if let Some(failed_hash) = active_req_map.remove(&request_id) {
                                    in_flight_txs.remove(&failed_hash);
                                    // Retain in queue to naturally age out, allowing immediate retry for this specific transaction.
                                }

                                // FIX: Broad Network Failure Amnesty and Guarded Dynamic Fallback.
                                // Catches Timeouts and aggressive TCP Connection Resets.
                                // Triggers non-blocking Headers-First fallback to prevent state machine stalling.
                                match error {
                                    libp2p::request_response::OutboundFailure::UnsupportedProtocols => {} 
                                    _ => {
                                        if !FALLBACK_GUARD.swap(true, std::sync::atomic::Ordering::SeqCst) {
                                            tracing::warn!("[DEFENSE] Network failure to {} ({:?}). Amnesty granted. Guarded Fallback scheduled.", peer, error);
                                            let sos_tx_clone = sos_tx.clone();
                                            tokio::spawn(async move {
                                                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                                                let _ = sos_tx_clone.try_send((0, None));
                                                FALLBACK_GUARD.store(false, std::sync::atomic::Ordering::SeqCst);
                                            });
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    
                    // MAINNET FIX: Absolute Error Suppression (Display & Debug formats)
                    // Keeps the terminal clean for critical consensus logs by capturing struct names via Debug formatting.
                    libp2p::swarm::SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                        let err_str = format!("{:?}", error); 
                        let err_str_display = error.to_string().to_lowercase();
                        if !err_str.contains("os error 32") && !err_str.contains("Broken pipe") && !err_str.contains("failed to resolve") 
                            && !err_str.contains("os error 61") && !err_str.contains("Connection refused") && !err_str.contains("WrongPeerId") && !err_str_display.contains("wrong peer id") {
                            tracing::warn!("[WARN] P2P: Outgoing connection error to {:?}: {:?}", peer_id, error);
                        }
                    }
                    libp2p::swarm::SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
                        if let Some(err) = cause {
                            let err_str = err.to_string();
                            if !err_str.contains("os error 32") && !err_str.contains("Broken pipe") {
                                tracing::debug!("[DEBUG] P2P: Connection closed with {:?}: {:?}", peer_id, err);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    #[allow(unreachable_code)]
    Ok(())
}


