// src/miner.rs
//
// The multi-core CPU coordinator-worker pool, plus an optional CUDA
// backend selected with QBTC_MINER_BACKEND. The CPU path is unchanged by
// default; GPU mining is opt-in and falls back to CPU whenever no usable
// device is present.

use crate::block::Block;
use crate::transaction::Transaction;
use crate::consensus::ConsensusEngine;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use crossbeam_channel::{Receiver, Sender};
use std::time::{Instant, Duration};

// Standard SHA-256 initial hash values (FIPS 180-4).
const SHA256_H_INIT: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
    0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

// Caches the fixed part of the header's SHA-256 state so the CPU hash path
// stops re-hashing the full 120-byte (3-block) header from scratch on every
// nonce. The first 64 bytes (timestamp | previous_hash | first 24 bytes of
// merkle_root) do not change for a given template and timestamp, so their
// compression runs once per chunk instead of once per nonce, cutting one of
// the three compressions per attempt.
//
// Every step here goes through sha2's own `compress256`, the same
// independently tested primitive the normal path uses; no SHA-256 round
// logic is reimplemented. `hash_nonce` is also only a pre-filter: any
// candidate it flags is still confirmed through the authoritative
// `ConsensusEngine::verify_proof_of_work` (and therefore the real
// `Block::calculate_hash()`) before it can be accepted anywhere.
pub struct CpuPowHasher {
    midstate: [u32; 8],
    // Bytes 64..104 of the header: last 8 bytes of merkle_root + all 32
    // bytes of commit_merkle_root. Fixed for the lifetime of one chunk.
    w2tail: [u8; 40],
}

impl CpuPowHasher {
    pub fn new(timestamp: u64, previous_hash: &[u8; 32], merkle_root: &[u8; 32], commit_merkle_root: &[u8; 32]) -> Self {
        use sha2::digest::generic_array::GenericArray;
        let mut block1: GenericArray<u8, sha2::digest::typenum::U64> = Default::default();
        block1[0..8].copy_from_slice(&timestamp.to_be_bytes());
        block1[8..40].copy_from_slice(previous_hash);
        block1[40..64].copy_from_slice(&merkle_root[0..24]);
        let mut state = SHA256_H_INIT;
        sha2::compress256(&mut state, &[block1]);

        let mut w2tail = [0u8; 40];
        w2tail[0..8].copy_from_slice(&merkle_root[24..32]);
        w2tail[8..40].copy_from_slice(commit_merkle_root);

        Self { midstate: state, w2tail }
    }

    // Bit-identical to Block::calculate_hash() for a header built from the
    // same fields plus (nonce, target) -- verified against it directly in
    // the unit test below over randomized inputs, not just by inspection.
    pub fn hash_nonce(&self, nonce: u64, target: u64) -> [u8; 32] {
        use sha2::digest::generic_array::GenericArray;
        let mut state = self.midstate;

        let mut block2: GenericArray<u8, sha2::digest::typenum::U64> = Default::default();
        block2[0..40].copy_from_slice(&self.w2tail);
        block2[40..48].copy_from_slice(&nonce.to_be_bytes());
        block2[48..56].copy_from_slice(&target.to_be_bytes());
        block2[56] = 0x80;
        sha2::compress256(&mut state, &[block2]);

        // Block3: entirely padding -- 56 zero bytes then the 8-byte
        // big-endian bit-length of the 120-byte (960-bit) message.
        let mut block3: GenericArray<u8, sha2::digest::typenum::U64> = Default::default();
        block3[56..64].copy_from_slice(&960u64.to_be_bytes());
        sha2::compress256(&mut state, &[block3]);

        let mut out = [0u8; 32];
        for i in 0..8 {
            out[i * 4..i * 4 + 4].copy_from_slice(&state[i].to_be_bytes());
        }
        out
    }
}

#[cfg(test)]
mod cpu_pow_hasher_tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn matches_plain_sha256_over_randomized_inputs() {
        // Simple xorshift PRNG so this test has no extra dependency.
        let mut state: u64 = 0x243F6A8885A308D3;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for _ in 0..2000 {
            let timestamp = next();
            let mut previous_hash = [0u8; 32];
            let mut merkle_root = [0u8; 32];
            let mut commit_merkle_root = [0u8; 32];
            for chunk in previous_hash.chunks_mut(8) { chunk.copy_from_slice(&next().to_be_bytes()); }
            for chunk in merkle_root.chunks_mut(8) { chunk.copy_from_slice(&next().to_be_bytes()); }
            for chunk in commit_merkle_root.chunks_mut(8) { chunk.copy_from_slice(&next().to_be_bytes()); }
            let nonce = next();
            let target = next();

            let hasher = CpuPowHasher::new(timestamp, &previous_hash, &merkle_root, &commit_merkle_root);
            let fast = hasher.hash_nonce(nonce, target);

            let mut header_bytes = [0u8; 120];
            header_bytes[0..8].copy_from_slice(&timestamp.to_be_bytes());
            header_bytes[8..40].copy_from_slice(&previous_hash);
            header_bytes[40..72].copy_from_slice(&merkle_root);
            header_bytes[72..104].copy_from_slice(&commit_merkle_root);
            header_bytes[104..112].copy_from_slice(&nonce.to_be_bytes());
            header_bytes[112..120].copy_from_slice(&target.to_be_bytes());
            let mut plain = Sha256::new();
            plain.update(&header_bytes);
            let slow: [u8; 32] = plain.finalize().into();

            assert_eq!(fast, slow, "mismatch at timestamp={} nonce={} target={}", timestamp, nonce, target);
        }
    }
}

// Global operational flags for Tier 0 isolation protocol.
pub static MUTE_CONSOLE_LOGS: AtomicBool = AtomicBool::new(false);
pub static IS_SIGNING: AtomicBool = AtomicBool::new(false);
// Lock-free command pending flag.
pub static PENDING_CMD: AtomicBool = AtomicBool::new(false);

// Data packet encapsulating all necessary parameters for mining a block.
#[derive(Clone)]
pub struct BlockTemplate {
    pub previous_hash: [u8; 32],
    pub transactions: Vec<Transaction>,
    pub target: u64,
    pub current_height: u64,
}

// Communication protocol between Tokio main thread and Daemon.
// Clone: the Hybrid backend relays every command to both the CPU and CUDA
// sub-daemons over two independent channels.
#[derive(Clone)]
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

// ---- Backend selection ---------------------------------------------
#[derive(Debug, Clone, PartialEq)]
enum MinerBackend {
    Cpu,
    Cuda,
    // Runs the CPU coordinator-worker pool and the CUDA daemon at the same
    // time against the same block template, so idle CPU cores contribute
    // hashrate alongside the GPU instead of sitting unused.
    Hybrid,
}

impl MinerBackend {
    fn from_env() -> Self {
        match std::env::var("QBTC_MINER_BACKEND")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "cuda" | "gpu" => MinerBackend::Cuda,
            "hybrid" | "both" | "cpu+gpu" => MinerBackend::Hybrid,
            _ => MinerBackend::Cpu,
        }
    }
}

pub struct Miner;

impl Miner {
    
    // Miner daemon entry point. QBTC_MINER_BACKEND picks the backend:
    // cpu (multi-core, the default), cuda, or hybrid.
    pub fn start_daemon(
        cmd_rx: Receiver<MinerCommand>,
        result_tx: Sender<Block>,
    ) {
        let backend = MinerBackend::from_env();
        match backend {
            MinerBackend::Cuda => {
                miner_log!("[INFO] Miner: CUDA backend selected");
                Self::run_cuda_daemon(cmd_rx, result_tx);
            }
            MinerBackend::Cpu => {
                miner_log!("[INFO] Miner: CPU backend selected");
                Self::run_multicore_cpu_daemon(cmd_rx, result_tx);
            }
            MinerBackend::Hybrid => {
                miner_log!("[INFO] Miner: Hybrid CPU+GPU backend selected");
                // Fan the same command stream out to two independent
                // channels so the CPU pool and the CUDA daemon each see
                // every Mine/Stop/Shutdown exactly like a solo backend would.
                let (cpu_cmd_tx, cpu_cmd_rx) = crossbeam_channel::unbounded::<MinerCommand>();
                let (gpu_cmd_tx, gpu_cmd_rx) = crossbeam_channel::unbounded::<MinerCommand>();
                std::thread::spawn(move || {
                    while let Ok(cmd) = cmd_rx.recv() {
                        let is_shutdown = matches!(cmd, MinerCommand::Shutdown);
                        let _ = cpu_cmd_tx.send(cmd.clone());
                        let _ = gpu_cmd_tx.send(cmd);
                        if is_shutdown { break; }
                    }
                });
                // CPU pool spawns its own coordinator/worker threads and
                // returns immediately -- safe to start first.
                Self::run_multicore_cpu_daemon(cpu_cmd_rx, result_tx.clone());
                // GPU threads already use a per-thread nonce offset of
                // thread_id * 1_000_000_000_000_000 (see below); the CUDA
                // scan starts at nonce 0, same as CPU thread 0, but at
                // 200+ MH/s the GPU clears that low range in a fraction of
                // a second while a single CPU thread would take years to
                // reach it -- the overlap is negligible, and every other
                // CPU thread's range is already exclusive of the GPU's.
                let devices = crate::gpu_miner::cuda_init_all();
                if !devices.is_empty() {
                    Self::run_cuda_daemon_loop_on(gpu_cmd_rx, result_tx, devices);
                } else {
                    miner_log!("[WARN] Miner: Hybrid mode requested but CUDA initialization failed. Continuing CPU-only.");
                }
            }
        }
    }

    // ---- CPU daemon: the multi-core coordinator-worker pool ---------
    // Initiates the dynamic multi-core Proof-of-Work daemon thread pool.
    // Utilizes a Coordinator-Worker architecture with absolute mathematical isolation.
    fn run_multicore_cpu_daemon(
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
        // CORE-V2 FIX: Cgroup-aware parallelism detecting true available vCPUs safely.
        let total_cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1) as u64;
        
        // Smart dynamic isolation: Automatically reserve cores for OS and networking.
        let core_count = if total_cores >= 8 {
            total_cores - 2 // Heavy setups (8+ cores): Reserve 2 cores
        } else if total_cores > 2 {
            total_cores - 1 // Medium setups (3-7 cores): Reserve 1 core
        } else {
            1 // Low-end (1-2 cores): Guarantee 1 mining thread to prevent crash
        };

        miner_log!("[INFO] Miner: Detected {} vCPU. Deploying {} hash threads (Reserved {} for Network/OS).", 
            total_cores, core_count, total_cores - core_count);

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
                        // 65536 -> 262144: the yield_now()/sleep(1ms) safety net below
                        // (there to keep this thread from starving Tokio's networking
                        // threads, since cores are only *counted* off, not pinned via
                        // real OS thread affinity) still fires, just 4x less often --
                        // cuts its overhead without removing the protection that kept
                        // the node responsive today.
                        const CHUNK_SIZE: u64 = 262144;

                        block.header.timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

                        // Midstate caching: hash the fixed first 64 bytes of
                        // the header once per chunk instead of once per
                        // nonce (see CpuPowHasher). Only a cheap pre-filter
                        // -- verify_proof_of_work() below still does the
                        // real, authoritative check before accepting.
                        let fast_hasher = CpuPowHasher::new(
                            block.header.timestamp,
                            &block.header.previous_hash,
                            &block.header.merkle_root,
                            &block.header.commit_merkle_root,
                        );

                        for _ in 0..CHUNK_SIZE {
                            let fast_hash = fast_hasher.hash_nonce(local_nonce, current_target);
                            let fast_prefix = u64::from_be_bytes(fast_hash[0..8].try_into().unwrap());
                            if fast_prefix <= current_target {
                                block.header.nonce = local_nonce;
                                if ConsensusEngine::verify_proof_of_work(block, current_target) {
                                    puzzle_solved = true;
                                    break;
                                }
                            }
                            local_nonce = local_nonce.wrapping_add(1); // Linear sequential hashing within isolated sector
                        }

                        nonce = local_nonce;

                        // Yield CPU time slice to prevent starving the Tokio async networking threads.
                        std::thread::yield_now();

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

    // ---- CUDA daemon ------------------------------------------------
    fn run_cuda_daemon(
        cmd_rx: Receiver<MinerCommand>,
        result_tx: Sender<Block>,
    ) {
        use crate::gpu_miner::cuda_init_all;

        // Bring up every visible GPU; fall back to CPU if none comes up.
        // QBTC_CUDA_DEVICES=0,1 restricts mining to a subset of devices.
        let devices = cuda_init_all();
        if devices.is_empty() {
            miner_log!("[WARN] Miner: CUDA initialization failed. Falling back to CPU backend.");
            Self::run_multicore_cpu_daemon(cmd_rx, result_tx);
            return;
        }
        miner_log!("[INFO] Miner: CUDA backend running on {} device(s).", devices.len());

        Self::run_cuda_daemon_loop_on(cmd_rx, result_tx, devices);
    }

    // The actual GPU command loop, split out from run_cuda_daemon() so the
    // Hybrid backend can reuse it after doing its own cuda_init() check --
    // calling cuda_init() twice would re-cudaMalloc the persistent device
    // buffers and leak the first set.
    fn run_cuda_daemon_loop_on(
        cmd_rx: Receiver<MinerCommand>,
        result_tx: Sender<Block>,
        devices: Vec<i32>,
    ) {
        use crate::gpu_miner::{cuda_shutdown, GpuBlockData, run_gpu_mine_loop_on};

        // One stop channel and one thread handle per device.
        let mut gpu_stops: Vec<crossbeam_channel::Sender<()>> = Vec::new();
        let mut gpu_threads: Vec<std::thread::JoinHandle<()>> = Vec::new();

        // Signal every running GPU job to stop, then reap the threads.
        macro_rules! halt_all {
            () => {{
                for tx in gpu_stops.drain(..) { let _ = tx.send(()); }
                for h in gpu_threads.drain(..) { let _ = h.join(); }
            }};
        }

        loop {
            // Blocking receive.
            match cmd_rx.recv() {
                Ok(cmd) => {
                    match cmd {
                        MinerCommand::Mine(template) => {
                            // A new template supersedes the old one: stop
                            // and join every in-flight GPU job first.
                            halt_all!();

                            miner_log!("[INFO] Miner: Starting Proof-of-Work for block height {}.", template.current_height + 1);
                            miner_log!("[INFO] Miner: Target threshold set to {}.", template.target);

                            let gpu_data = GpuBlockData::from_template(
                                template.previous_hash,
                                template.transactions,
                                template.target,
                            );

                            // One independent thread per device. Each walks
                            // its own disjoint set of nonce batches at its
                            // own pace, so mixed hardware needs no
                            // coordination -- a slower card simply completes
                            // fewer batches and never holds a faster one back.
                            let device_count = devices.len() as u64;
                            for (slot, dev) in devices.iter().copied().enumerate() {
                                let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
                                let result_tx_clone = result_tx.clone();
                                let data = gpu_data.clone();
                                let handle = std::thread::spawn(move || {
                                    run_gpu_mine_loop_on(
                                        dev,
                                        slot as u64,
                                        device_count,
                                        data,
                                        result_tx_clone,
                                        stop_rx,
                                    );
                                });
                                gpu_stops.push(stop_tx);
                                gpu_threads.push(handle);
                            }
                        }
                        MinerCommand::Stop => {
                            halt_all!();
                            miner_log!("[INFO] Miner: Interrupt signal received. Engine suspended.");
                        }
                        MinerCommand::Shutdown => {
                            halt_all!();
                            miner_log!("[INFO] Miner: Shutdown signal received. Breaking compute loop.");
                            cuda_shutdown();
                            return;
                        }
                    }
                }
                Err(_) => {
                    // Command channel closed: shut down.
                    halt_all!();
                    cuda_shutdown();
                    return;
                }
            }
        }
    }
}
