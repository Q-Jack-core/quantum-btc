// src/crypto/kmac256.rs
use tiny_keccak::{Hasher, Kmac};

/// Hardcoded Genesis Key for network-specific hashing.
/// Injects a constant string into the hashing process to differentiate
/// the network PoW from standard SHA-256 ASIC operations.
const QBTC_GENESIS_KEY: &[u8] = b"QBTC-GENESIS-PROTOCOL-V1";

/// Generates a KMAC256 hash using the network-specific key.
/// 
/// Future implementation note:
/// The current fallback uses `tiny-keccak` for compatibility.
/// Optimized versions may bypass this for AVX-512 inline assembly 
/// to leverage 512-bit data paths for parallel hash state computation.
pub fn hash_with_genesis_key(data: &[u8]) -> [u8; 32] {
    // Initialize KMAC256 with the genesis key and an empty customization string.
    let mut kmac = Kmac::v256(QBTC_GENESIS_KEY, b"");
    kmac.update(data);
    
    // Finalize and extract the 32-byte (256-bit) digest.
    let mut output = [0u8; 32];
    kmac.finalize(&mut output);
    
    output
}