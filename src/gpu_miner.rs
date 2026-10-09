use std::time::{SystemTime, UNIX_EPOCH};
use std::sync::atomic::Ordering;
use crate::block::{Block, BlockHeader};
use crate::crypto::merkle;
use crate::transaction::Transaction;

// Mirrors miner.rs's miner_log!: the Tier 0 isolation protocol mutes console
// output while the node is signing, and GPU progress lines have to honour
// that too.
macro_rules! gpu_log {
    ($($arg:tt)*) => {
        if !crate::miner::MUTE_CONSOLE_LOGS.load(Ordering::Relaxed) {
            tracing::info!($($arg)*);
        }
    };
}
macro_rules! gpu_warn {
    ($($arg:tt)*) => {
        if !crate::miner::MUTE_CONSOLE_LOGS.load(Ordering::Relaxed) {
            tracing::warn!($($arg)*);
        }
    };
}

extern "C" {
    fn qbtc_cuda_mine_on(device_id: i32, header_120: *const u8, target_u64: u64, start_nonce: u64, batch_size: u64, found_nonce_out: *mut u64, stop_flag_ptr: *mut i32) -> i32;
    fn qbtc_cuda_device_count(device_count_out: *mut i32) -> i32;
    fn qbtc_cuda_init_device(device_id: i32) -> i32;
    fn qbtc_cuda_device_name(device_id: i32) -> *const std::os::raw::c_char;
    fn qbtc_cuda_shutdown();
}
pub struct GpuStopFlag { inner: Box<i32> }
impl GpuStopFlag {
    pub fn new() -> Self { Self { inner: Box::new(0) } }
    pub fn as_ptr(&mut self) -> *mut i32 { self.inner.as_mut() as *mut i32 }
    pub fn stop(&mut self) { *self.inner = 1; }
    pub fn is_stopped(&self) -> bool { *self.inner != 0 }
}
pub fn device_name(device_id: i32) -> String {
    let name_ptr = unsafe { qbtc_cuda_device_name(device_id) };
    if name_ptr.is_null() {
        "Unknown".to_string()
    } else {
        unsafe { std::ffi::CStr::from_ptr(name_ptr) }.to_string_lossy().into_owned()
    }
}

// Prepares every visible GPU and returns the device ids that came up.
// QBTC_CUDA_DEVICES=0,2 restricts mining to a subset; an empty result means
// no usable GPU and the caller should fall back to CPU.
pub fn cuda_init_all() -> Vec<i32> {
    let mut count: i32 = 0;
    let ret = unsafe { qbtc_cuda_device_count(&mut count) };
    if ret != 0 || count <= 0 {
        gpu_warn!("[WARN] Miner: CUDA device enumeration failed (ret={}, devices={})", ret, count);
        return Vec::new();
    }

    let wanted: Option<Vec<i32>> = std::env::var("QBTC_CUDA_DEVICES").ok().map(|raw| {
        raw.split(',')
            .filter_map(|p| p.trim().parse::<i32>().ok())
            .filter(|d| *d >= 0 && *d < count)
            .collect()
    });
    let candidates: Vec<i32> = match wanted {
        Some(list) if !list.is_empty() => list,
        _ => (0..count).collect(),
    };

    let mut ready = Vec::new();
    for dev in candidates {
        let r = unsafe { qbtc_cuda_init_device(dev) };
        if r == 0 {
            gpu_log!("[INFO] Miner: CUDA device {} ready: {}", dev, device_name(dev));
            ready.push(dev);
        } else {
            gpu_warn!("[WARN] Miner: CUDA device {} init failed (ret={}), skipping", dev, r);
        }
    }
    if ready.is_empty() {
        gpu_warn!("[WARN] Miner: no CUDA device could be initialized");
    } else {
        gpu_log!("[INFO] Miner: mining on {} GPU(s): {:?}", ready.len(), ready);
    }
    ready
}

pub fn cuda_shutdown() { unsafe { qbtc_cuda_shutdown() }; }
pub fn build_header_120(timestamp: u64, previous_hash: &[u8; 32], merkle_root: &[u8; 32], commit_merkle_root: &[u8; 32], nonce: u64, target: u64) -> [u8; 120] {
    let mut buf = [0u8; 120];
    buf[0..8].copy_from_slice(&timestamp.to_be_bytes());
    buf[8..40].copy_from_slice(previous_hash);
    buf[40..72].copy_from_slice(merkle_root);
    buf[72..104].copy_from_slice(commit_merkle_root);
    buf[104..112].copy_from_slice(&nonce.to_be_bytes());
    buf[112..120].copy_from_slice(&target.to_be_bytes());
    buf
}
#[derive(Clone)]
pub struct GpuBlockData {
    pub previous_hash: [u8; 32],
    pub merkle_root: [u8; 32],
    pub commit_merkle_root: [u8; 32],
    pub target: u64,
    pub transactions: Vec<Transaction>,
}
impl GpuBlockData {
    pub fn from_template(previous_hash: [u8; 32], transactions: Vec<Transaction>, target: u64) -> Self {
        let tx_ids: Vec<[u8; 32]> = transactions.iter().map(|tx| tx.calculate_id()).collect();
        let witness_hashes: Vec<[u8; 32]> = transactions.iter().map(|tx| tx.calculate_witness_hash()).collect();
        let merkle_root = merkle::build_merkle_root(tx_ids);
        let commit_merkle_root = merkle::build_merkle_root(witness_hashes);
        Self { previous_hash, merkle_root, commit_merkle_root, target, transactions }
    }
    pub fn to_block_with_nonce(&self, nonce: u64, timestamp: u64) -> Block {
        Block {
            header: BlockHeader { timestamp, previous_hash: self.previous_hash, merkle_root: self.merkle_root, commit_merkle_root: self.commit_merkle_root, nonce, target: self.target },
            transactions: self.transactions.clone(),
        }
    }
}
// qbtc_cuda_mine returns 0 (found), 1 (batch exhausted, nothing found -- the
// normal/expected outcome most of the time), or a negative code identifying
// which specific stage failed (see qbtc_sha256_miner.cu): -1 kernel
// launch/sync, -2/-3/-4 one of the setup H2D memcpys, -5 the D2H result
// copy. The old `Option<u64>` return collapsed every failure into the same
// `None`, so a real CUDA error silently got counted as a completed
// batch_size worth of hashing in the caller's rate display -- hashrate kept
// reporting normally while the GPU wasn't actually computing anything.
pub enum GpuScanResult {
    Found(u64),
    Exhausted,
    // Distinct from Exhausted: no batch actually ran (stop was already
    // requested before qbtc_cuda_mine was even called), so the caller must
    // not credit batch_size hashes for it.
    Cancelled,
    Failed(i32),
}

fn describe_cuda_failure(code: i32) -> &'static str {
    match code {
        -1 => "kernel launch or cudaDeviceSynchronize failed",
        -2 => "H2D memcpy of block header failed",
        -3 => "H2D memcpy of found-sentinel failed",
        -4 => "H2D memcpy of stop-flag failed",
        -5 => "D2H memcpy of result failed",
        _ => "unknown failure",
    }
}

pub fn gpu_scan_on(device_id: i32, header: &[u8; 120], target: u64, start_nonce: u64, batch_size: u64, stop: &mut GpuStopFlag) -> GpuScanResult {
    if stop.is_stopped() { return GpuScanResult::Cancelled; }
    let mut found: u64 = 0;
    let ret = unsafe { qbtc_cuda_mine_on(device_id, header.as_ptr(), target, start_nonce, batch_size, &mut found, stop.as_ptr()) };
    match ret {
        0 => { gpu_log!("[INFO] Miner: CUDA found nonce: {}", found); GpuScanResult::Found(found) }
        1 => GpuScanResult::Exhausted,
        code => {
            gpu_warn!("[WARN] Miner: CUDA batch failed (code={}, {}). Not counting this batch toward hashrate.", code, describe_cuda_failure(code));
            GpuScanResult::Failed(code)
        }
    }
}
// One mining loop per GPU.
//
// `device_id` is the CUDA device this loop drives; `slot` and `device_count`
// carve the nonce space so two GPUs never test the same nonce: device in slot
// s walks batches s, s+N, s+2N, ... Each loop advances its own cursor at its
// own pace, so a fast card simply completes more batches than a slow one and
// mixed hardware needs no coordination.
pub fn run_gpu_mine_loop_on(
    device_id: i32,
    slot: u64,
    device_count: u64,
    gpu_data: GpuBlockData,
    result_tx: crossbeam_channel::Sender<Block>,
    stop_rx: crossbeam_channel::Receiver<()>,
) {
    let fixed_timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let header = build_header_120(fixed_timestamp, &gpu_data.previous_hash, &gpu_data.merkle_root, &gpu_data.commit_merkle_root, 0, gpu_data.target);
    let batch_size: u64 = std::env::var("QBTC_CUDA_BATCH_SIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(100_000_000u64);
    let mut stop = GpuStopFlag::new();
    // Interleaved start so each device owns a disjoint set of batches.
    let devices = device_count.max(1);
    let stride = batch_size.saturating_mul(devices);
    let mut start_nonce: u64 = batch_size.saturating_mul(slot % devices);
    let mut hash_count: u64 = 0;
    // Real-time progress ticker, gated purely on wall-clock time rather
    // than on hash_count crossing the coarse 500M threshold below --
    // that way the operator sees a live rate within ~2s of starting no
    // matter how fast/slow the GPU or how big QBTC_CUDA_BATCH_SIZE is,
    // instead of possibly waiting a long time for the first log line.
    let mining_started = std::time::Instant::now();
    let mut last_progress_print = mining_started;
    let mut hashes_since_print: u64 = 0;
    const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
    gpu_log!(
        "[INFO] Miner: GPU{} scan started (batch_size={}, start_nonce={}, stride={}, timestamp={})",
        device_id, batch_size, start_nonce, stride, fixed_timestamp
    );
    if slot == 0 && std::env::var_os("QBTC_CUDA_DEBUG").is_some() {
        print_reference_hash_for_debug(&header);
    }
    // Tracks repeated GpuScanResult::Failed in a row. A single transient
    // failure just backs off and retries, but a run of them (driver in a
    // bad state, GPU fallen off the bus, etc.) is worth surfacing loudly
    // instead of warn!-spamming forever at the same log level.
    let mut consecutive_failures: u32 = 0;
    loop {
        if stop_rx.try_recv().is_ok() { stop.stop(); break; }
        let batch_started = std::time::Instant::now();
        let scan_result = gpu_scan_on(device_id, &header, gpu_data.target, start_nonce, batch_size, &mut stop);
        let batch_elapsed = batch_started.elapsed();
        match scan_result {
            GpuScanResult::Found(winning_nonce) => {
                consecutive_failures = 0;
                let candidate = gpu_data.to_block_with_nonce(winning_nonce, fixed_timestamp);
                if crate::consensus::ConsensusEngine::verify_proof_of_work(&candidate, gpu_data.target) {
                    gpu_log!("[INFO] Miner [GPU{}]: Proof-of-Work puzzle solved nonce={}", device_id, winning_nonce);
                    let hash = candidate.calculate_hash();
                    let hash_hex: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
                    gpu_log!("[INFO] Miner: Absolute Block Hash: 0x{}", hash_hex);
                    let _ = result_tx.send(candidate);
                    loop { if stop_rx.try_recv().is_ok() { return; } std::thread::sleep(std::time::Duration::from_millis(100)); }
                } else {
                    let dbg_hash = candidate.calculate_hash();
                    let dbg_prefix = u64::from_be_bytes(dbg_hash[0..8].try_into().unwrap());
                    gpu_warn!(
                        "[WARN] Miner: GPU{} reported a nonce that failed CPU re-verification \
                         (nonce={} hash_prefix={} target={} ts={}); discarding and continuing",
                        device_id, winning_nonce, dbg_prefix, gpu_data.target, fixed_timestamp
                    );
                    start_nonce = start_nonce.wrapping_add(stride);
                    continue;
                }
            }
            GpuScanResult::Exhausted => {
                consecutive_failures = 0;
                hash_count += batch_size;
                hashes_since_print += batch_size;
                let since_print = last_progress_print.elapsed();
                if since_print >= PROGRESS_INTERVAL {
                    let mh_s = (hashes_since_print as f64 / since_print.as_secs_f64()) / 1_000_000.0;
                    let total_elapsed = mining_started.elapsed().as_secs_f64();
                    // batch_elapsed here is this one call's wall time (includes
                    // the cudaDeviceSynchronize wait, not pure kernel time) --
                    // a baseline for judging whether async streams would ever
                    // be worth it: waste-per-transition scales with this number.
                    gpu_log!(
                        "[MINING] GPU{} progress: {} hashes total | {:.1} MH/s | elapsed {:.0}s | nonce={} | last_batch={:.3}s",
                        device_id, hash_count, mh_s, total_elapsed, start_nonce, batch_elapsed.as_secs_f64()
                    );
                    last_progress_print = std::time::Instant::now();
                    hashes_since_print = 0;
                }
                if hash_count % 500_000_000 < batch_size { gpu_log!("[INFO] Miner: GPU{} {} million hashes calculated.", device_id, hash_count / 1_000_000); }
                start_nonce = start_nonce.wrapping_add(stride);
                if start_nonce == 0 { gpu_warn!("[WARN] Miner: GPU{} exhausted the entire u64 nonce space.", device_id); break; }
                if stop.is_stopped() || stop_rx.try_recv().is_ok() { break; }
            }
            GpuScanResult::Cancelled => {
                // No batch ran, nothing to credit or log -- just exit the loop.
                break;
            }
            GpuScanResult::Failed(code) => {
                // Deliberately NOT added to hash_count -- no real hashing happened.
                // start_nonce is also NOT advanced, so the next attempt retries
                // this exact same range instead of silently skipping it.
                consecutive_failures += 1;
                if consecutive_failures >= 5 {
                    tracing::error!("[ERROR] Miner: GPU{} has failed {} batches in a row (last code={}). Check GPU/driver health -- CUDA mining is not making progress.", device_id, consecutive_failures, code);
                }
                // Brief backoff so a persistent CUDA failure (e.g. driver hiccup)
                // doesn't spin this thread in a tight retry loop.
                std::thread::sleep(std::time::Duration::from_millis(500));
                if stop.is_stopped() || stop_rx.try_recv().is_ok() { break; }
            }
        }
    }
}

// Prints the CPU-side reference hash of this job's header, for comparing by
// hand against the kernel when investigating a suspected GPU problem. This
// does NOT verify anything by itself -- every nonce the GPU reports is
// re-checked through ConsensusEngine::verify_proof_of_work() above before it
// can be accepted. Opt in with QBTC_CUDA_DEBUG=1.
pub fn print_reference_hash_for_debug(header: &[u8; 120]) {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(header);
    let rust_hash: [u8; 32] = hasher.finalize().into();
    let rust_prefix = u64::from_be_bytes(rust_hash[0..8].try_into().unwrap());
    gpu_log!("[DEBUG] Miner: reference hash prefix={}", rust_prefix);
    gpu_log!("[DEBUG] Miner: header_hex={}", header.iter().map(|b| format!("{:02x}",b)).collect::<String>());
}
