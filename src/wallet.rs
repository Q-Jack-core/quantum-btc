// src/wallet.rs
// HD Keystore Engine (NIST FIPS 204 Compliant)
use serde::{Serialize, Deserialize};
use std::fs;
use rand::rngs::SysRng;
use rand::Rng;
use rand_core::UnwrapErr;
use bip39::Mnemonic; // Restored: Required for mnemonic parsing
// BIP-350 Bech32m Standard Library
use bech32::{self, FromBase32, ToBase32, Variant};

// -----------------------------------------------------------------------------
// Encryption Suite
// -----------------------------------------------------------------------------
use aes_gcm::{Aes256Gcm, Key, Nonce, aead::{Aead, KeyInit}};
use pbkdf2::pbkdf2_hmac;
use sha2::{Digest, Sha256};

// -----------------------------------------------------------------------------
// Post-Quantum Wallet Structure
// -----------------------------------------------------------------------------
#[derive(Serialize, Deserialize, Clone)]
pub struct QuantumWallet {
    pub seed: Vec<u8>,             // Stores the ML-DSA Secret Key (SK)
    pub mnemonic: String,          
    pub external_index: u32,       
    pub internal_index: u32,       
    pub qbtc_address: String,      
    pub public_key: Vec<u8>,       // Stores the 1952-byte ML-DSA Public Key (PK)
}

#[derive(Serialize, Deserialize)]
pub struct EncryptedKeystoreV2 {
    pub root_pubkey: Vec<u8>,
    pub primary_address: String,
    pub external_index: u32,
    pub internal_index: u32,
    pub salt: Vec<u8>,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,       
}

impl QuantumWallet {

    // -------------------------------------------------------------------------
    // BIP-350 Bech32m Address Encoder & Verifier
    // -------------------------------------------------------------------------
    // Adheres to the Bech32m address format.
    // Utilizes BCH error-correcting codes and restricts character set to prevent visual ambiguity.

    /// Encodes a raw physical public key hash into a secure Bech32m QBTC address.
    pub fn encode_qbtc_address(pubkey_hash: &[u8]) -> String {
        // 1. Convert 8-bit bytes into 5-bit words (Base32) as required by BIP-173/350.
        let base32_data = pubkey_hash.to_base32();
        
        // 2. Encode the address using the "qbtc" Human-Readable Part (HRP) and Bech32m Variant.
        bech32::encode("qbtc", base32_data, Variant::Bech32m)
            .expect("Bech32m encoding failed.")
    }

    /// Verifies a QBTC address and extracts the raw public key hash if mathematically valid.
    /// Returns None silently if formatting is corrupted to prevent console spam during alias resolution.
    pub fn decode_qbtc_address(address: &str) -> Option<Vec<u8>> {
        // 1. Attempt BIP-350 decoding.
        match bech32::decode(address) {
            Ok((hrp, base32_data, variant)) => {
                // 2. Verify network prefix ("qbtc") and Bech32m variant silently.
                if hrp != "qbtc" || variant != Variant::Bech32m {
                    return None;
                }
                
                // 3. Convert 5-bit words back to 8-bit bytes.
                if let Ok(hash_bytes) = Vec::<u8>::from_base32(&base32_data) {
                    return Some(hash_bytes);
                }
                None
            }
            Err(_) => {
                // Silently return None to allow fallback to alias-based keystore routing.
                None
            }
        }
    }

    // -------------------------------------------------------------------------
    // Core Cryptography Logic (ML-DSA)
    // -------------------------------------------------------------------------

    pub fn pk_to_address(pk: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(pk);
        let hash = hasher.finalize();
        // Generate Bech32m encoded address.
        Self::encode_qbtc_address(&hash)
    }

    pub fn restore_from_mnemonic(phrase: &str) -> Result<Self, String> {
        let parsed_mnemonic = Mnemonic::parse(phrase).map_err(|_| "Invalid Mnemonic Phrase")?; 
        
        // -------------------------------------------------------------------------
        // Deterministic ML-DSA Seed Derivation
        // -------------------------------------------------------------------------
        // 1. Extract the raw 64-byte seed from the mnemonic phrase.
        let raw_seed = parsed_mnemonic.to_seed(""); 
        
        // 2. Hash to 32 bytes to satisfy the ML-DSA PRNG requirement.
        let mut hasher = Sha256::new();
        hasher.update(&raw_seed);
        let deterministic_seed_32: [u8; 32] = hasher.finalize().into();

        println!("[INFO] Wallet: Generating ML-DSA-65 keypair from mnemonic entropy.");
        
        // 3. Generate deterministic ML-DSA keypair.
        let (pk_der, sk_der) = crate::crypto::ml_dsa::generate_deterministic_keypair(&deterministic_seed_32);

        let mut wallet = Self {
            seed: sk_der, 
            mnemonic: phrase.to_string(),
            external_index: 0,
            internal_index: 0,
            qbtc_address: String::new(),
            public_key: pk_der, 
        };

        wallet.qbtc_address = Self::pk_to_address(&wallet.public_key);
        Ok(wallet)
    }

    // Persists keystore payload to isolated datadir.
    pub fn save_to_disk_secure(&self, datadir: &str, name: &str, password: &str) -> Result<(), String> {
        let mut salt = [0u8; 16];
        UnwrapErr(SysRng).fill_bytes(&mut salt);
        
        let mut encryption_key = [0u8; 32];
        pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, 100_000, &mut encryption_key);
        
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&encryption_key));
        let mut nonce_bytes = [0u8; 12];
        UnwrapErr(SysRng).fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        
        let plaintext = bincode::serialize(self).map_err(|e| e.to_string())?;
        let ciphertext = cipher.encrypt(nonce, plaintext.as_ref())
            .map_err(|e| format!("Encryption Error: {}", e))?;

        let keystore = EncryptedKeystoreV2 {
            root_pubkey: self.public_key.clone(),
            primary_address: self.qbtc_address.clone(),
            external_index: self.external_index,
            internal_index: self.internal_index,
            salt: salt.to_vec(),
            nonce: nonce_bytes.to_vec(),
            ciphertext,
        };

        let dir_path = format!("{}/keystores", datadir);
        let _ = fs::create_dir_all(&dir_path);
        let file_path = format!("{}/{}.dat", dir_path, name);
        
        let final_data = bincode::serialize(&keystore).map_err(|e| e.to_string())?;
        fs::write(file_path, final_data).map_err(|e| e.to_string())?;
        
        Ok(())
    }

    // Loads keystore using secure datadir decoupling.
    pub fn load_from_disk_secure(datadir: &str, name: &str, password: &str) -> Result<Self, String> {
        let file_path = format!("{}/keystores/{}.dat", datadir, name);
        let data = fs::read(&file_path).map_err(|_| format!("Keystore file '{}.dat' not found in datadir.", name))?;
        
        if let Ok(keystore) = bincode::deserialize::<EncryptedKeystoreV2>(&data) {
            let mut encryption_key = [0u8; 32];
            pbkdf2_hmac::<Sha256>(password.as_bytes(), &keystore.salt, 100_000, &mut encryption_key);
            let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&encryption_key));
            let nonce = Nonce::from_slice(&keystore.nonce);
            let plaintext = cipher.decrypt(nonce, keystore.ciphertext.as_slice())
                .map_err(|_| "Authentication failed: Invalid password.")?;
            let wallet: Self = bincode::deserialize(&plaintext).map_err(|e| e.to_string())?;
            return Ok(wallet);
        }
        Err("Unsupported wallet format.".to_string())
    }

    // Retrieves public info using decoupled datadir path.
    pub fn get_public_info(datadir: &str, name: &str) -> Option<(Vec<u8>, String)> {
        let file_path = format!("{}/keystores/{}.dat", datadir, name);
        if let Ok(data) = fs::read(&file_path) {
            if let Ok(keystore) = bincode::deserialize::<EncryptedKeystoreV2>(&data) {
                return Some((keystore.root_pubkey, keystore.primary_address));
            }
        }
        None
    }

    // -------------------------------------------------------------------------
    // ML-DSA-65 Signature Execution
    // -------------------------------------------------------------------------
    pub fn sign_transaction(&self, message_hash: &[u8], _is_change: bool, _index: u32) -> Vec<u8> {
        tracing::debug!("[DEBUG] Wallet: Initiating ML-DSA-65 signature generation.");
        
        // Generate the post-quantum cryptographic signature.
        let signature = crate::crypto::ml_dsa::sign_message(message_hash, &self.seed);
        
        tracing::debug!("[DEBUG] Wallet: ML-DSA-65 signature generated successfully.");
        signature
    }
}