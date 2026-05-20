// src/config.rs

// Maximum allowable Block Weight in Weight Units (WU).
pub const MAX_BLOCK_WEIGHT: u32 = 32_000_000;

// Maximum allowable Signature Operations per block.
pub const MAX_BLOCK_SIGOPS: u32 = 80_000;

// Minimum fee rate in Sats/WU to relay a transaction.
pub const MIN_RELAY_FEE_RATE: u64 = 5;

// Coinbase UTXO maturity threshold.
pub const COINBASE_MATURITY: u64 = 100;