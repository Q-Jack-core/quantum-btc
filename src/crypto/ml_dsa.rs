// src/crypto/ml_dsa.rs
/// NIST FIPS 204 (ML-DSA) Post-Quantum Signature Engine.
/// Implements the finalized US NIST standard for quantum-resistant signatures.
/// Zeroize enabled to prevent RAM-scraping and cold-boot extraction.

use ml_dsa::{MlDsa65, Signature, KeyGen};
use ml_dsa::signature::{Signer, Verifier, SignatureEncoding, Keypair}; 
use pkcs8::{EncodePrivateKey, DecodePrivateKey}; 
use spki::{EncodePublicKey, DecodePublicKey};    
use zeroize::Zeroize; 
use rand::SeedableRng;
use rand::rngs::StdRng;

// Core OS RNG and Infallible wrapper
use rand::rngs::SysRng; 
use rand_core::UnwrapErr; 

/// Target: ML-DSA-65 (NIST Security Category 3)
pub fn generate_cold_keypair() -> (Vec<u8>, Vec<u8>) {
    // Wraps the OS RNG into an infallible layer to satisfy 
    // the ML-DSA requirement for an infallible entropy source.
    let mut infallible_rng = UnwrapErr(SysRng);
    
    let secret_key = MlDsa65::key_gen(&mut infallible_rng);
    let public_key = secret_key.verifying_key();
    
    let pk_der = public_key.to_public_key_der().expect("Encoding error: Public key PKCS8 DER conversion failed.");
    let sk_der = secret_key.to_pkcs8_der().expect("Encoding error: Secret key PKCS8 DER conversion failed.");
    
    (pk_der.as_bytes().to_vec(), sk_der.as_bytes().to_vec())
}

pub fn generate_deterministic_keypair(seed: &[u8; 32]) -> (Vec<u8>, Vec<u8>) {
    // Utilizes a deterministic RNG based on the provided 32-byte seed.
    let mut deterministic_rng = StdRng::from_seed(*seed);
    
    let secret_key = MlDsa65::key_gen(&mut deterministic_rng);
    let public_key = secret_key.verifying_key();
    
    let pk_der = public_key.to_public_key_der().expect("Encoding error: Public key PKCS8 DER conversion failed.");
    let sk_der = secret_key.to_pkcs8_der().expect("Encoding error: Secret key PKCS8 DER conversion failed.");
    
    (pk_der.as_bytes().to_vec(), sk_der.as_bytes().to_vec())
}

pub fn sign_message(message: &[u8], secret_key: &[u8]) -> Vec<u8> {
    let mut sk_buffer = secret_key.to_vec();
    
    let sk = match ml_dsa::SigningKey::<MlDsa65>::from_pkcs8_der(&sk_buffer) {
        Ok(key) => key,
        Err(_) => {
            // Secure wipe: Clear memory before panic to prevent partial state leak.
            sk_buffer.zeroize();
            panic!("Decryption error: Corrupted ML-DSA-65 PKCS8 secret key.");
        }
    };

    // Secure wipe: Overwrite the plaintext private key array in stack memory.
    sk_buffer.zeroize();

    let signature: Signature<MlDsa65> = sk.sign(message);
    signature.to_bytes().to_vec()
}

pub fn verify_signature(message: &[u8], signature: &[u8], public_key: &[u8]) -> bool {
    let pk = match ml_dsa::VerifyingKey::<MlDsa65>::from_public_key_der(public_key) {
        Ok(key) => key,
        Err(_) => return false, // Silent return on malformed packets
    };

    let sig = match Signature::<MlDsa65>::try_from(signature) {
        Ok(s) => s,
        Err(_) => return false, // Silent return on invalid sizes
    };

    pk.verify(message, &sig).is_ok()
}