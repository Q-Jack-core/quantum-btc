// src/network/mod.rs
// Core P2P network module.
// Architecture: Asynchronous event-driven subsystem.
// Purpose: Handles protocol definitions and routing for Gossipsub and RPC.

pub mod p2p;
pub mod reputation;
pub mod sync_manager;

use serde::{Serialize, Deserialize};
use crate::block::{Block, BlockHeader};
use crate::transaction::Transaction;

// Dedicated structures for the request-response data pipeline.
// Bypasses Gossipsub to prevent network flooding and MTU overflow during historical sync.

/// Synchronization mode definition for AssumeValid architecture.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum SyncMode {
    /// Full payload including ML-DSA-65 signatures.
    /// Required for recent blocks to ensure immediate consensus security.
    Full,
    /// Core transaction structure only, stripping witness data to conserve bandwidth.
    /// Utilized for historical blocks validated by cumulative PoW trust.
    CoreOnly,
}

// Point-to-Point Direct Protocol Pipeline.
// Supports decoupled Header and Data fetching.

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum SyncRequest {
    GetHeaders { locator_hashes: Vec<[u8; 32]>, requester: String },
    GetData { hashes: Vec<[u8; 32]>, requester: String, mode: SyncMode },
    // Request the lightweight compact block skeleton.
    GetCompactBlock { block_hash: [u8; 32], requester: String },
    // Fallback request for missing transactions during local reconstruction.
    GetBlockTxn { block_hash: [u8; 32], indexes: Vec<usize>, requester: String },
    // Directed pull request for individual mempool transactions.
    GetMempoolTx { tx_hash: [u8; 32], requester: String },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum SyncResponse {
    Headers { headers: Vec<BlockHeader>, responder: String },
    DataResponse { blocks: Vec<Block>, responder: String },
    // Delivers the compact block skeleton.
    CompactBlockResponse { compact_block: Option<crate::block::CompactBlock>, responder: String },
    // Delivers specifically requested missing transactions.
    BlockTxnResponse { block_hash: [u8; 32], transactions: Vec<Transaction>, responder: String },
    // Delivery of specific transaction payload.
    MempoolTxResponse { tx: Option<Transaction>, responder: String },
}

// Strict Gossipsub Protocol Definition.
// Defines the serializable data structures permitted on the P2P mesh network.
#[derive(Serialize, Deserialize, Clone)]
pub enum NetworkPayload {
    // Broadcast a lightweight header to the mesh when a new block is mined.
    BlockAnnouncement(crate::block::BlockHeader),
    
    // CORE-V4: Transformed to INV-only broadcast to prevent ML-DSA-65 bandwidth exhaustion.
    // Replaces full transaction broadcast with 32-byte identifier.
    TransactionInv([u8; 32]),
}