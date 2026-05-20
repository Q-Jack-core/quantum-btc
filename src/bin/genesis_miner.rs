// src/bin/genesis_miner.rs
use quantum_btc::block::Block;
use std::time::Instant;

fn main() {
    println!("=========================================================");
    println!("[INFO] System: Q-BTC mainnet genesis block generator initialized.");
    println!("=========================================================");

    // Fetch the incomplete genesis block (nonce is currently 0).
    let mut genesis_block = Block::genesis();
    let target = genesis_block.header.target;

    println!("[INFO] Genesis Motto: \"17/May/2026: The quantum age dawns. The 21,000,000 truth shines eternal.\"");
    println!("[INFO] Timestamp: {}", genesis_block.header.timestamp);
    println!("[INFO] Target Difficulty: {}", target);
    println!("[INFO] Engine started. Calculating nonce...");
    println!("");

    let mut nonce: u64 = 0; // Starting iteration from 0.
    let start_time = Instant::now();

    // Proof-of-work iteration.
    loop {
        genesis_block.header.nonce = nonce;
        let hash = genesis_block.calculate_hash();

        // Extract the first 8 bytes for difficulty comparison.
        let mut prefix_bytes = [0u8; 8];
        prefix_bytes.copy_from_slice(&hash[0..8]);
        let hash_prefix = u64::from_be_bytes(prefix_bytes);

        // Check difficulty target.
        if hash_prefix <= target {
            let duration = start_time.elapsed();
            println!("\n=========================================================");
            println!("[INFO] Valid nonce discovered.");
            println!("=========================================================");
            println!("[INFO] Nonce: {}", nonce);
            let hex_hash: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
            println!("[INFO] Genesis Hash: 0x{}", hex_hash);
            println!("[INFO] Time elapsed: {:.2} seconds.", duration.as_secs_f64());
            println!("=========================================================");
            println!("[INFO] Action required: Update src/block.rs with the discovered nonce.");
            break;
        }

        nonce += 1;

        if nonce % 2_000_000 == 0 {
            let elapsed = start_time.elapsed().as_secs_f64();
            let hr = (nonce as f64 / 1_000_000.0) / elapsed;
            println!("[INFO] Progress: {} million hashes computed. Rate: {:.2} MH/s.", nonce / 1_000_000, hr);
        }
    }
}