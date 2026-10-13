// src/config.rs

// Maximum allowable Block Weight in Weight Units (WU).
pub const MAX_BLOCK_WEIGHT: u32 = 32_000_000;

// Maximum serialized block size, enforced by every node's block firewall.
pub const MAX_BLOCK_SIZE_BYTES: usize = 8 * 1024 * 1024;

// Space kept free for the header and coinbase when packing mempool txs.
pub const BLOCK_TEMPLATE_RESERVE_BYTES: usize = 64 * 1024;

// Largest block we build ourselves. Stock peers read at most 10 MiB per sync
// response and ML-DSA byte vectors encode at ~1.9x in CBOR, so blocks above
// ~5.3 MB raw cannot be downloaded by them and would be orphaned.
pub const MAX_TEMPLATE_BLOCK_BYTES: usize = 4_500_000;

// Largest transfer the wallet builds (~460 inputs), so it relays to stock
// peers and two still fit in one propagation-safe block.
pub const MAX_WALLET_TX_BYTES: u64 = 2_500_000;

// Height from which blocks must commit to their witnesses via
// commit_merkle_root. Enforced on every block-entry path: gossip, deep sync
// and /api/submit_block. Blocks below this height are not checked, so the
// rule needs no historical audit; it only has to activate far enough ahead
// that every node on the network is running code that enforces it on all
// three paths. Chain height was 24,650 on 2026-10-14, which is 5,350 blocks
// below this value: about 37 days at the 600 s target, 19 days if blocks
// come in at 300 s.
pub const WITNESS_COMMITMENT_ACTIVATION_HEIGHT: u64 = 30_000;

// Maximum allowable Signature Operations per block.
pub const MAX_BLOCK_SIGOPS: u32 = 80_000;

// Minimum fee rate in Sats/WU to relay a transaction.
pub const MIN_RELAY_FEE_RATE: u64 = 5;

// Coinbase UTXO maturity threshold.
pub const COINBASE_MATURITY: u64 = 100;

// Hard fork activation height for consensus security patch v2.3.0
pub const CONSENSUS_HARDFORK_V2_HEIGHT: u64 = 21_500;