// src/rpc.rs
use axum::{
    extract::{State, Request},
    routing::{get, post},
    Json, Router,
    middleware::{self, Next},
    response::Response,
};
use tower_http::cors::{Any, CorsLayer};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::net::SocketAddr;
use tokio::net::TcpListener;

//use crate::utxo::UtxoState;
use crate::mempool::blind_box::QuantumMempool;
use crate::block::Block;
use crate::storage::QuantumStorage;
use crate::network::NetworkPayload;
// Included TxWitness for isolated signature processing.
use crate::transaction::{Transaction, TxIn, TxOut, TxWitness};
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct RpcState {
    pub port: u16, 
    pub datadir: String, // Industrial standard isolated storage path.
    pub mempool: Arc<Mutex<QuantumMempool>>,
    pub latest_block: Arc<Mutex<Block>>,
    pub utxo_tx: tokio::sync::mpsc::Sender<crate::utxo::UtxoCommand>,
    pub p2p_tx: tokio::sync::mpsc::Sender<NetworkPayload>,
    // Relies on row-level UTXO mutexes to allow concurrent state operations.
    
    // Integrated QuantumStorage to query physical chain height.
    pub storage: Arc<QuantumStorage>,
    pub explorer_enabled: bool,
}

#[derive(Serialize)]
pub struct NodeInfoResponse {
    pub current_height: u64,
    pub mempool_size: usize,
    pub status: String,
}

#[derive(Serialize)]
pub struct ApiResponse {
    pub success: bool,
    pub message: String,
    pub tx_hash: Option<String>, 
}

#[derive(Deserialize)]
pub struct BalanceRequest {
    pub address: String,
}

#[derive(Serialize)]
pub struct BalanceResponse {
    pub address: String,
    pub confirmed_sats: u64, 
    pub unconfirmed_sats: u64, 
    pub pending_sats: u64,     
    pub locked_sats: u64,      
}

#[derive(Deserialize)]
pub struct TransferRequest {
    // Use string representation to prevent IEEE 754 precision loss.
    pub amount_qbtc: String,
    pub target_hex: String, 
    pub wallet_name: Option<String>, 
    pub password: Option<String>, 
}

#[derive(Deserialize)]
pub struct WalletActionReq {
    pub wallet_name: String, 
    pub mnemonic: Option<String>,
    pub password: Option<String>, 
}

#[derive(Serialize)]
pub struct WalletActionRes { 
    pub success: bool, 
    pub message: String, 
    pub address: Option<String>, 
    pub mnemonic: Option<String> 
}

#[derive(Deserialize)]
pub struct TxStatusRequest {
    pub tx_hash: String,
}

#[derive(Serialize)]
pub struct TxStatusResponse {
    pub status: String, 
}

#[derive(Deserialize)]
pub struct VerifyTargetRequest {
    pub target: String,
}

#[derive(Serialize)]
pub struct VerifyTargetResponse {
    pub is_valid: bool,
    pub exact_hex: Option<String>,
    pub message: String,
}

async fn token_auth(req: Request, next: Next) -> Result<Response, axum::http::StatusCode> {
    let expected = std::env::var("QBTC_RPC_TOKEN").unwrap_or_default();
    if expected.is_empty() { return Ok(next.run(req).await); }
    
    if let Some(auth) = req.headers().get(axum::http::header::AUTHORIZATION) {
        if let Ok(token) = auth.to_str() {
            if token == expected { return Ok(next.run(req).await); }
        }
    }
    Err(axum::http::StatusCode::UNAUTHORIZED)
}
pub async fn start_rpc_server(port: u16, state: RpcState) {
    let rpc_port = port + 4000;
    let addr = SocketAddr::from(([127, 0, 0, 1], rpc_port));
    
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);
    
    let app = Router::new()
        .route("/api/get_info", get(get_node_info))
        .route("/api/get_balance", post(get_tactical_balance)) 
        .route("/api/execute_transfer", post(execute_transfer)) 
        .route("/api/wallet_gen", post(api_wallet_gen))
        .route("/api/wallet_restore", post(api_wallet_restore))
        .route("/api/wallet_unlock", post(api_wallet_unlock))
        .route("/api/tx_status", post(get_tx_status))
        .route("/api/verify_target", post(verify_tactical_target))
        .route("/api/get_block_template", post(api_get_block_template))
        .route("/api/submit_block", post(api_submit_block))
        .route("/api/get_block", post(get_block_by_height));

    let app = app.route_layer(middleware::from_fn(token_auth)).layer(cors).with_state(state);

    println!("[INFO] RPC: Server listening on http://127.0.0.1:{}", rpc_port);
    let listener = TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
async fn api_wallet_unlock(State(state): State<RpcState>, Json(req): Json<WalletActionReq>) -> Json<WalletActionRes> {
    let pwd = req.password.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "CLI_DEFAULT_LOCK".to_string());
    match crate::wallet::QuantumWallet::load_from_disk_secure(&state.datadir, &req.wallet_name, &pwd) {
        Ok(w) => Json(WalletActionRes {
            success: true,
            message: "Vault unlocked successfully.".to_string(),
            address: Some(w.qbtc_address),
            mnemonic: None
        }),
        Err(e) => Json(WalletActionRes {
            success: false,
            message: format!("Access Denied: {}", e),
            address: None,
            mnemonic: None
        })
    }
}

async fn api_wallet_gen(State(state): State<RpcState>, Json(req): Json<WalletActionReq>) -> Json<WalletActionRes> {
    use rand::rngs::SysRng; use rand::Rng; use rand_core::UnwrapErr; use bip39::Mnemonic;
    let mut entropy = [0u8; 16]; UnwrapErr(SysRng).fill_bytes(&mut entropy);
    let mnemonic = Mnemonic::from_entropy(&entropy).unwrap();
    let phrase = mnemonic.to_string();
    let pwd = req.password.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "CLI_DEFAULT_LOCK".to_string());
    
    match crate::wallet::QuantumWallet::restore_from_mnemonic(&phrase) {
        Ok(w) => {
            if let Err(e) = w.save_to_disk_secure(&state.datadir, &req.wallet_name, &pwd) {
                return Json(WalletActionRes { success: false, message: format!("Encryption Failed: {}", e), address: None, mnemonic: None });
            }
            Json(WalletActionRes { success: true, message: "Wallet initialized successfully.".to_string(), address: Some(w.qbtc_address), mnemonic: Some(phrase) })
        },
        Err(e) => Json(WalletActionRes { success: false, message: e, address: None, mnemonic: None })
    }
}

async fn api_wallet_restore(State(state): State<RpcState>, Json(req): Json<WalletActionReq>) -> Json<WalletActionRes> {
    if let Some(phrase) = req.mnemonic {
        let pwd = req.password.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "CLI_DEFAULT_LOCK".to_string());
        match crate::wallet::QuantumWallet::restore_from_mnemonic(&phrase) {
            Ok(w) => {
                if let Err(e) = w.save_to_disk_secure(&state.datadir, &req.wallet_name, &pwd) {
                    return Json(WalletActionRes { success: false, message: format!("Encryption Failed: {}", e), address: None, mnemonic: None });
                }
                Json(WalletActionRes { success: true, message: "Wallet restored successfully.".to_string(), address: Some(w.qbtc_address), mnemonic: None })
            },
            Err(e) => Json(WalletActionRes { success: false, message: e, address: None, mnemonic: None })
        }
    } else { Json(WalletActionRes { success: false, message: "Missing 12 words".to_string(), address: None, mnemonic: None }) }
}

async fn get_node_info(State(state): State<RpcState>) -> Json<NodeInfoResponse> {
    let mp = state.mempool.lock().unwrap();
    // Fetch physical chain length from storage instead of unix timestamp.
    let physical_height = state.storage.get_chain_list().len().saturating_sub(1) as u64;
    Json(NodeInfoResponse {
        current_height: physical_height, 
        mempool_size: mp.tx_pool.len(),
        status: "Node operational".to_string(),
    })
}

async fn get_tactical_balance(State(state): State<RpcState>, Json(req): Json<BalanceRequest>) -> Json<BalanceResponse> {
    let current_height = state.storage.get_chain_list().len() as u64;
    let pending_txs: Vec<Transaction> = state.mempool.lock().unwrap().get_txs_for_mining();

    let mut target_hash = [0u8; 32];
    let mut is_valid_target = false;

    if let Some(decoded) = crate::wallet::QuantumWallet::decode_qbtc_address(&req.address) {
        target_hash.copy_from_slice(&decoded[0..32]);
        is_valid_target = true;
    } else if let Some((pub_key, _)) = crate::wallet::QuantumWallet::get_public_info(&state.datadir, &req.address) {
        let mut h = Sha256::new(); h.update(&pub_key); target_hash = h.finalize().into();
        is_valid_target = true;
    }

    if !is_valid_target {
        return Json(BalanceResponse { 
            address: req.address, 
            confirmed_sats: 0, 
            unconfirmed_sats: 0,
            pending_sats: 0,      // Added missing field to satisfy Rust compiler
            locked_sats: 0        // Added missing field to satisfy Rust compiler
        });
    }

    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
    let _ = state.utxo_tx.send(crate::utxo::UtxoCommand::GetBalance { 
        pubkey_hash: target_hash, 
        current_height, 
        pending_txs,
        resp: resp_tx 
    }).await;
    
    // FIX: Removed the underscore from '_pending_sats' to actively receive the mempool pending balance.
    let (mature_sats, pending_sats, locked_sats) = resp_rx.await.unwrap_or((0, 0, 0));

    // FIX: Strictly decouple pending and locked states for accurate frontend display.
    Json(BalanceResponse { 
        address: req.address, 
        confirmed_sats: mature_sats, 
        unconfirmed_sats: pending_sats + locked_sats, // Fallback
        pending_sats: pending_sats,
        locked_sats: locked_sats,
    })
}

// =============================================================================
// Unified Transfer Protocol: Construct transaction, generate ML-DSA signatures,
// and broadcast directly to the network.
// =============================================================================
async fn execute_transfer(State(state): State<RpcState>, Json(req): Json<TransferRequest>) -> Json<ApiResponse> {
    let target_wallet = req.wallet_name.unwrap_or_else(|| "default".to_string());
    let pwd = req.password.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "CLI_DEFAULT_LOCK".to_string());
    
    let my_wallet = match crate::wallet::QuantumWallet::load_from_disk_secure(&state.datadir, &target_wallet, &pwd) {
        Ok(w) => w,
        Err(e) => return Json(ApiResponse { success: false, message: format!("AUTHORIZATION FAILED: {}", e), tx_hash: None }),
    };

    let mut target_hash = [0u8; 32]; 
    let mut is_valid_target = false;

    if let Some(decoded) = crate::wallet::QuantumWallet::decode_qbtc_address(&req.target_hex) {
        target_hash.copy_from_slice(&decoded[0..32]);
        is_valid_target = true;
    } else if let Some((pub_key, _)) = crate::wallet::QuantumWallet::get_public_info(&state.datadir, &req.target_hex) {
        let mut h = Sha256::new(); h.update(&pub_key); target_hash = h.finalize().into();
        is_valid_target = true;
    }

    if !is_valid_target {
        return Json(ApiResponse { success: false, message: "INVALID TARGET: Base58 Checksum Failed.".to_string(), tx_hash: None });
    }

    // Fetch physical height for UTXO maturity validation.
    let current_height = state.storage.get_chain_list().len() as u64;
    
    // High-precision string parsing for financial exactness.
    let amount_str = req.amount_qbtc.trim();
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

    let mut root_h = Sha256::new(); root_h.update(&my_wallet.public_key);
    let my_pk_hash: [u8; 32] = root_h.finalize().into();

    let pending_txs: Vec<Transaction> = state.mempool.lock().unwrap().get_txs_for_mining();
    
    let network_fee_rate: u64 = crate::config::MIN_RELAY_FEE_RATE * 5;
    const TX_BASE_BYTES: u64 = 30;
    const TX_IN_BYTES: u64 = 5350;
    const TX_OUT_BYTES: u64 = 50;

    let mut target_fee_atomic: u64 = (TX_BASE_BYTES + TX_IN_BYTES + (2 * TX_OUT_BYTES)) * network_fee_rate;
    let mut _utxo_query_result = Err("Insufficient deep liquidity to cover transaction.");

    for _iteration in 0..5 {
        let current_total_required = amount_atomic + target_fee_atomic;
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        
        let _ = state.utxo_tx.send(crate::utxo::UtxoCommand::GetSpendable {
            pubkey_hash: my_pk_hash,
            current_height,
            required_amount: current_total_required,
            pending_txs: pending_txs.clone(),
            resp: resp_tx
        }).await;

        match resp_rx.await.unwrap_or(Err("Actor Channel Closed")) {
            Ok((selected, gathered)) => {
                let input_count = selected.len() as u64;
                let projected_bytes = TX_BASE_BYTES + (input_count * TX_IN_BYTES) + (2 * TX_OUT_BYTES);
                
                if projected_bytes > 8_000_000 {
                    _utxo_query_result = Err("Transaction exceeds 8MB physical limit");
                    break;
                }
                
                let projected_fee = projected_bytes * network_fee_rate;
                
                if gathered >= amount_atomic + projected_fee {
                    target_fee_atomic = projected_fee;
                    _utxo_query_result = Ok((selected, gathered));
                    break;
                } else {
                    target_fee_atomic = projected_fee;
                }
            }
            Err(e) => {
                _utxo_query_result = Err(e);
                break;
            }
        }
    }

    let utxo_query_result = _utxo_query_result;
    let total_required = amount_atomic + target_fee_atomic;

    match utxo_query_result {
        Ok((gathered_utxos, total_gathered)) => {
            let mut inputs = Vec::new();
            for (outpoint, _) in &gathered_utxos {
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
            
            let mut witnesses = Vec::new(); 
            for _ in 0..inputs.len() {
                let signature = my_wallet.sign_transaction(&tx_core_hash, false, 0);
                witnesses.push(TxWitness {
                    signature,
                    public_key: my_wallet.public_key.clone(),
                });
            }
            
            let tx = Transaction { inputs, outputs, witnesses };
            let tx_hash = tx.calculate_id();

            let eval_height = state.storage.get_chain_list().len() as u64;
            let (val_tx, val_rx) = tokio::sync::oneshot::channel();
            let _ = state.utxo_tx.send(crate::utxo::UtxoCommand::ValidateMempoolTx { tx: tx.clone(), current_height: eval_height, crypto_pre_verified: false, resp: val_tx }).await;
            
            match val_rx.await.unwrap_or(Err("Actor Channel Closed")) {
                Ok(exact_fee) => {
                    if exact_fee < 1000 {
                        return Json(ApiResponse { success: false, message: "Fee too low.".to_string(), tx_hash: None });
                    }
                    
                    let admission_result = state.mempool.lock().unwrap().add_transaction(tx.clone(), exact_fee);
                    
                    if admission_result.is_ok() {
                        let _ = state.p2p_tx.send(NetworkPayload::TransactionInv(tx.calculate_id())).await;
                        let hash_hex: String = tx_hash.iter().map(|b| format!("{:02x}", b)).collect();
                        return Json(ApiResponse { success: true, message: "Transaction broadcasted.".to_string(), tx_hash: Some(hash_hex) });
                    } else {
                        return Json(ApiResponse { success: false, message: "Mempool rejected the transaction.".to_string(), tx_hash: None });
                    }
                }
                Err(_) => {
                    return Json(ApiResponse { success: false, message: "UTXO validation failed.".to_string(), tx_hash: None });
                }
            }
        }
        Err(e) => Json(ApiResponse { success: false, message: e.to_string(), tx_hash: None }),
    }
}

async fn get_tx_status(State(state): State<RpcState>, Json(req): Json<TxStatusRequest>) -> Json<TxStatusResponse> {
    let mut hash_bytes = [0u8; 32];
    if req.tx_hash.len() == 64 {
        for i in 0..32 { hash_bytes[i] = u8::from_str_radix(&req.tx_hash[i*2..i*2+2], 16).unwrap_or(0); }
    } else { return Json(TxStatusResponse { status: "INVALID_HASH".to_string() }); }

    let mp = state.mempool.lock().unwrap();
    if mp.tx_pool.contains_key(&hash_bytes) { return Json(TxStatusResponse { status: "PENDING".to_string() }); }
    Json(TxStatusResponse { status: "UNKNOWN_OR_MINED".to_string() })
}

async fn verify_tactical_target(State(state): State<RpcState>, Json(req): Json<VerifyTargetRequest>) -> Json<VerifyTargetResponse> {
    let target = req.target.trim();
    let mut exact_hex = String::new();
    let mut is_valid = false;

    if let Some(_) = crate::wallet::QuantumWallet::decode_qbtc_address(target) {
        exact_hex = target.to_string(); 
        is_valid = true;
    } else if let Some((pub_key, _)) = crate::wallet::QuantumWallet::get_public_info(&state.datadir, target) {
        let mut hasher = Sha256::new(); hasher.update(&pub_key);
        let hash: [u8; 32] = hasher.finalize().into();
        exact_hex = crate::wallet::QuantumWallet::encode_qbtc_address(&hash); 
        is_valid = true;
    }

    if is_valid {
        Json(VerifyTargetResponse { is_valid: true, exact_hex: Some(exact_hex), message: "Target verified.".to_string() })
    } else {
        Json(VerifyTargetResponse { is_valid: false, exact_hex: None, message: "Invalid target format.".to_string() })
    }
}


#[derive(Deserialize)]
pub struct BlockRequest {
    pub height: u64,
}

#[derive(Serialize)]
pub struct TxInputDetail {
    pub prev_txid: String,
    pub vout: u32,
}

#[derive(Serialize)]
pub struct TxOutputDetail {
    pub value_sats: u64,
    pub address_hash: String,
}

#[derive(Serialize)]
pub struct TxDetailResponse {
    pub txid: String,
    pub inputs: Vec<TxInputDetail>,
    pub outputs: Vec<TxOutputDetail>,
    pub is_quantum_secured: bool,
    pub witness_size_bytes: usize,
}

#[derive(Serialize)]
pub struct BlockResponse {
    pub height: u64,
    pub timestamp: u64,
    pub previous_hash: String,
    pub merkle_root: String,
    pub commit_merkle_root: String,
    pub nonce: u64,
    pub target: u64,
    pub tx_count: usize,
    pub transactions: Vec<TxDetailResponse>,
}

async fn get_block_by_height(State(state): State<RpcState>, Json(req): Json<BlockRequest>) -> Json<Option<BlockResponse>> {
    let chain = state.storage.get_chain_list();
    if req.height as usize >= chain.len() {
        return Json(None);
    }
    let target_hash = chain[req.height as usize];

    if let Some(block) = state.storage.get_block_by_hash(&target_hash, false) {
        let prev_hex: String = block.header.previous_hash.iter().map(|b| format!("{:02x}", b)).collect();
        let merkle_hex: String = block.header.merkle_root.iter().map(|b| format!("{:02x}", b)).collect();
        let commit_merkle_hex: String = block.header.commit_merkle_root.iter().map(|b| format!("{:02x}", b)).collect();

        let tx_details: Vec<TxDetailResponse> = block.transactions.iter().map(|tx| {
            let txid_hex = tx.calculate_id().iter().map(|b| format!("{:02x}", b)).collect();
            
            let inputs_detail = tx.inputs.iter().map(|vin| TxInputDetail {
                prev_txid: vin.previous_output_hash.iter().map(|b| format!("{:02x}", b)).collect(),
                vout: vin.vout,
            }).collect();

            let outputs_detail = tx.outputs.iter().map(|vout| TxOutputDetail {
                value_sats: vout.value,
                address_hash: vout.public_key_hash.iter().map(|b| format!("{:02x}", b)).collect(),
            }).collect();

            let witness_size = tx.witnesses.iter().map(|w| w.signature.len() + w.public_key.len()).sum();

            TxDetailResponse {
                txid: txid_hex,
                inputs: inputs_detail,
                outputs: outputs_detail,
                is_quantum_secured: !tx.witnesses.is_empty(),
                witness_size_bytes: witness_size,
            }
        }).collect();

        return Json(Some(BlockResponse {
            height: req.height,
            timestamp: block.header.timestamp,
            previous_hash: prev_hex,
            merkle_root: merkle_hex,
            commit_merkle_root: commit_merkle_hex,
            nonce: block.header.nonce,
            target: block.header.target,
            tx_count: block.transactions.len(),
            transactions: tx_details,
        }));
    }
    Json(None)
}


// =============================================================================
// STRATUM POOL GATEWAY API (LAYER 2 INTEGRATION)
// =============================================================================

#[derive(Deserialize)]
pub struct GetBlockTemplateReq {
    pub miner_address: String,
}

#[derive(Serialize)]
pub struct GetBlockTemplateRes {
    pub previous_hash: String,
    pub current_height: u64,
    pub target: u64,
    pub transactions: Vec<Transaction>,
}

#[derive(Deserialize)]
pub struct SubmitBlockReq {
    pub block: Block,
}

async fn api_get_block_template(State(state): State<RpcState>, Json(req): Json<GetBlockTemplateReq>) -> Json<Option<GetBlockTemplateRes>> {
    let mut miner_pk_hash = [0u8; 32];
    if let Some(decoded) = crate::wallet::QuantumWallet::decode_qbtc_address(&req.miner_address) {
        miner_pk_hash.copy_from_slice(&decoded[0..32]);
    } else {
        return Json(None); 
    }

    let chain = state.storage.get_chain_list();
    let current_height = chain.len() as u64;
    
    let tip_hash = chain.last().copied().unwrap_or([0u8; 32]);
    let genesis_hash = chain.first().copied().unwrap_or([0u8; 32]);
    
    let tip_idx = state.storage.get_block_index(&tip_hash).unwrap();
    let genesis_idx = state.storage.get_block_index(&genesis_hash).unwrap();
    
    // Dynamic ASERTi3-2d target recalculation for external Stratum nodes.
    let target = crate::consensus::ConsensusEngine::required_target(
        genesis_idx.header.timestamp,
        genesis_idx.header.target,
        tip_idx.header.timestamp,
        current_height
    );

    let mut total_fees = 0u64;
    let mut txs = {
        let mempool_guard = state.mempool.lock().unwrap();
        let selected = mempool_guard.get_txs_for_mining();
        for tx in &selected {
            let tx_hash = tx.calculate_id();
            if let Some(entry) = mempool_guard.tx_pool.get(&tx_hash) {
                total_fees += entry.fee;
            }
        }
        selected
    };

    let coinbase_in = TxIn { previous_output_hash: [0u8; 32], vout: current_height as u32 };
    let coinbase_witness = TxWitness { signature: vec![], public_key: vec![] };
    let block_reward = crate::economics::CentralBank::get_block_reward(current_height) + total_fees;

    txs.insert(0, Transaction {
        inputs: vec![coinbase_in],
        outputs: vec![TxOut { value: block_reward, public_key_hash: miner_pk_hash, recovery: None }],
        witnesses: vec![coinbase_witness]
    });

    let previous_hash: String = tip_hash.iter().map(|b| format!("{:02x}", b)).collect();

    Json(Some(GetBlockTemplateRes {
        previous_hash,
        current_height,
        target,
        transactions: txs,
    }))
}

async fn api_submit_block(State(state): State<RpcState>, Json(req): Json<SubmitBlockReq>) -> Json<ApiResponse> {
    let block = req.block;
    let hash = block.calculate_hash();

    // 1. Tip validation to prevent stale submissions.
    let tip_hash = state.latest_block.lock().unwrap().calculate_hash();
    if block.header.previous_hash != tip_hash {
        return Json(ApiResponse { success: false, message: "Rejected: Orphan block or invalid tip".to_string(), tx_hash: None });
    }

    let current_height = state.storage.get_chain_list().len() as u64;

    // 2. PoW Validation with Hard Fork Gating
    if current_height >= crate::config::CONSENSUS_HARDFORK_V2_HEIGHT {
        let chain = state.storage.get_chain_list();
        let genesis_hash = chain.first().copied().unwrap_or([0u8; 32]);
        let genesis_idx = match state.storage.get_block_index(&genesis_hash) {
            Some(idx) => idx,
            None => return Json(ApiResponse { success: false, message: "Storage error: missing genesis".to_string(), tx_hash: None }),
        };
        let tip_idx = match state.storage.get_block_index(&tip_hash) {
            Some(idx) => idx,
            None => return Json(ApiResponse { success: false, message: "Storage error: missing tip".to_string(), tx_hash: None }),
        };

        let required = crate::consensus::ConsensusEngine::required_target(
            genesis_idx.header.timestamp,
            genesis_idx.header.target,
            tip_idx.header.timestamp,
            current_height,
        );
        if let Err(e) = crate::consensus::ConsensusEngine::verify_block_target_and_pow(&block, required) {
            return Json(ApiResponse { success: false, message: format!("Rejected: {e}"), tx_hash: None });
        }
    } else {
        let hash_u64 = u64::from_be_bytes(hash[..8].try_into().unwrap());
        if hash_u64 > block.header.target {
            return Json(ApiResponse { success: false, message: "Rejected: Invalid PoW".to_string(), tx_hash: None });
        }
    }

    // 3. Dispatch to isolated UtxoActor for ML-DSA-65 validation.
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
    let _ = state.utxo_tx.send(crate::utxo::UtxoCommand::ApplyBlock {
        block: block.clone(),
        height: current_height,
        is_historical: false,
        resp: resp_tx,
    }).await;

    match resp_rx.await.unwrap_or(Err("Actor Channel Closed")) {
        Ok(undo_log) => {
            // 4. Physical Commit Pipeline.
            let prev_hash = block.header.previous_hash;
            let current_work = state.storage.get_block_index(&prev_hash).map(|idx| idx.chain_work).unwrap_or(0);
            let new_work = current_work.saturating_add(block.header.get_block_proof());

            let (snap_tx, snap_rx) = tokio::sync::oneshot::channel();
            let _ = state.utxo_tx.send(crate::utxo::UtxoCommand::GetSnapshot { resp: snap_tx }).await;
            let utxo_snap = snap_rx.await.unwrap();

            state.storage.commit_state_transition(block.clone(), current_height, &undo_log, &utxo_snap, new_work);

            // 5. Memory Pointer Updates.
            *state.latest_block.lock().unwrap() = block.clone();
            state.mempool.lock().unwrap().atomic_sweep(&block.transactions);

            // 6. Network Broadcast via Gossipsub.
            let _ = state.p2p_tx.send(NetworkPayload::BlockAnnouncement(block.header.clone())).await;

            let hash_hex: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
            Json(ApiResponse { success: true, message: "Block accepted and broadcasted".to_string(), tx_hash: Some(hash_hex) })
        }
        Err(e) => {
            Json(ApiResponse { success: false, message: format!("Rejected by Consensus: {}", e), tx_hash: None })
        }
    }
}