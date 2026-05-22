// src/rpc.rs
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
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

pub async fn start_rpc_server(port: u16, state: RpcState) {
    let rpc_port = port + 4000;
    let addr = SocketAddr::from(([127, 0, 0, 1], rpc_port));
    
    let cors = CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any);
    let app = Router::new()
        .route("/api/get_info", get(get_node_info))
        .route("/api/get_balance", post(get_tactical_balance)) 
        .route("/api/execute_transfer", post(execute_transfer)) 
        .route("/api/wallet_gen", post(api_wallet_gen))
        .route("/api/wallet_restore", post(api_wallet_restore))
        .route("/api/tx_status", post(get_tx_status))
        .route("/api/verify_target", post(verify_tactical_target))
        .layer(cors)
        .with_state(state); 

    println!("[INFO] RPC: Server listening on http://127.0.0.1:{}", rpc_port);
    let listener = TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
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
        return Json(BalanceResponse { address: req.address, confirmed_sats: 0, unconfirmed_sats: 0 });
    }

    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
    let _ = state.utxo_tx.send(crate::utxo::UtxoCommand::GetBalance { 
        pubkey_hash: target_hash, 
        current_height, 
        pending_txs,
        resp: resp_tx 
    }).await;
    let (mature_sats, _pending_sats, locked_sats) = resp_rx.await.unwrap_or((0, 0, 0));

    Json(BalanceResponse { address: req.address, confirmed_sats: mature_sats, unconfirmed_sats: locked_sats })
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

    let fee_atomic: u64 = 10000;
    let total_required = amount_atomic + fee_atomic;

    let mut root_h = Sha256::new(); root_h.update(&my_wallet.public_key);
    let my_pk_hash: [u8; 32] = root_h.finalize().into();

    let pending_txs: Vec<Transaction> = state.mempool.lock().unwrap().get_txs_for_mining();
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
    let _ = state.utxo_tx.send(crate::utxo::UtxoCommand::GetSpendable { 
        pubkey_hash: my_pk_hash, 
        current_height, 
        required_amount: total_required, 
        pending_txs,
        resp: resp_tx 
    }).await;
    let utxo_query_result = resp_rx.await.unwrap_or(Err("INSUFFICIENT FUNDS OR UTXOS LOCKED BY OTHER THREADS."));

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
            // Require strict crypto verification for external RPC injections.
            let _ = state.utxo_tx.send(crate::utxo::UtxoCommand::ValidateMempoolTx { tx: tx.clone(), current_height: eval_height, crypto_pre_verified: false, resp: val_tx }).await;
            
            if val_rx.await.unwrap_or(Err("Actor Channel Closed")).is_ok() {
                // Execute admission synchronously and drop the MutexGuard immediately to preserve Send trait.
                let admission_result = state.mempool.lock().unwrap().add_transaction(tx.clone(), fee_atomic);
                
                if admission_result.is_ok() {
                    let _ = state.p2p_tx.send(NetworkPayload::TransactionInv(tx.calculate_id())).await;
                    let hash_hex: String = tx_hash.iter().map(|b| format!("{:02x}", b)).collect();
                    return Json(ApiResponse { success: true, message: "Transaction broadcasted.".to_string(), tx_hash: Some(hash_hex) });
                } else {
                    return Json(ApiResponse { success: false, message: "Mempool rejected the transaction.".to_string(), tx_hash: None });
                }
            } else {
                return Json(ApiResponse { success: false, message: "UTXO validation failed.".to_string(), tx_hash: None });
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