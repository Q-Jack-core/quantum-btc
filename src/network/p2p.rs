// src/network/p2p.rs
use libp2p::{
    gossipsub, identity, identify, mdns, noise, request_response, swarm::NetworkBehaviour,
    tcp, yamux, PeerId, SwarmBuilder, StreamProtocol, kad,
};
use std::time::Duration;
use std::{fs, path::Path}; 

// Defines the network behaviour combining multiple P2P protocols.
#[derive(NetworkBehaviour)]
pub struct QbtcBehaviour {
    pub gossipsub: gossipsub::Behaviour,
    pub mdns: mdns::tokio::Behaviour,
    pub identify: identify::Behaviour, // Protocol identification and metadata exchange.
    
    // Kademlia Distributed Hash Table (DHT) for global peer discovery.
    // Operates entirely in memory to prevent I/O bottlenecks during node bootstrap.
    pub kad: kad::Behaviour<kad::store::MemoryStore>,
    
    // Request-Response protocol for direct block synchronization.
    // Separates bulk historical syncing from real-time gossip broadcasts.
    pub req_resp: request_response::cbor::Behaviour<crate::network::SyncRequest, crate::network::SyncResponse>,

    // Native Ping heartbeat to teardown dead Yamux streams.
    pub ping: libp2p::ping::Behaviour,
}

// Enhanced builder with architecture-role awareness.
// Receives 'is_seed_node' flag to dynamically configure Kademlia DHT routing modes.
pub fn build_swarm(storage_path: &str, is_seed_node: bool) -> Result<libp2p::Swarm<QbtcBehaviour>, Box<dyn std::error::Error>> {
    // Persistent node identity initialization.
    // Maintains a stable PeerId across restarts to preserve network topology
    // and prevent connection collisions from identical IP origins.
    let identity_path = Path::new(storage_path).join("network_identity.bin");
    
    let local_key = if identity_path.exists() {
        // Load existing identity keypair from local storage
        let encoded = fs::read(&identity_path)?;
        identity::Keypair::from_protobuf_encoding(&encoded)
            .map_err(|e| format!("Failed to decode existing identity key: {}", e))?
    } else {
        // PeerId Proof-of-Work to mitigate Sybil network flooding.
        tracing::info!("[INFO] P2P: Generating PoW-secured network identity...");
        let mut generated_key;
        loop {
            generated_key = identity::Keypair::generate_ed25519();
            let peer_id = PeerId::from(generated_key.public());
            let peer_bytes = peer_id.to_bytes();
            // Requires 8-bit computational effort per identity creation.
            if peer_bytes.last().unwrap_or(&1) == &0 { 
                break;
            }
        }
        let encoded = generated_key.to_protobuf_encoding()?;
        fs::create_dir_all(storage_path)?;
        fs::write(&identity_path, encoded)?;
        generated_key
    };

    let local_peer_id = PeerId::from(local_key.public());
    tracing::info!("[INFO] P2P: Local node identity (PeerId): {}", local_peer_id);

    // 1. Gossipsub configuration for block and mempool propagation.
    let message_id_fn = |message: &gossipsub::Message| {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&message.data);
        let hash_hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        gossipsub::MessageId::from(hash_hex)
    };
    
    let gossipsub_config = gossipsub::ConfigBuilder::default()
        .heartbeat_interval(Duration::from_secs(1)) 
        .validation_mode(gossipsub::ValidationMode::Strict) 
        .message_id_fn(message_id_fn)
        // Limit Gossipsub transmit size to 2MB to prevent bandwidth amplification.
        .max_transmit_size(2 * 1024 * 1024) 
        .build()
        .expect("Valid gossipsub config");
        
    let gossipsub = gossipsub::Behaviour::new(
        gossipsub::MessageAuthenticity::Signed(local_key.clone()),
        gossipsub_config,
    ).expect("Correct configuration");

    // 2. mDNS for local peer discovery.
    let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), local_peer_id)?;

    // 3. Identify protocol for version and key exchange.
    let identify = identify::Behaviour::new(identify::Config::new(
        "/qbtc/1.0.0".to_string(),
        local_key.public(),
    ));

    // High-Speed Point-to-Point Pipeline.
    // Timeout extended to 120s to accommodate trans-pacific ML-DSA-65 validation.
    let req_resp_config = request_response::Config::default()
        .with_request_timeout(Duration::from_secs(120));
        
    // Initialize standard request-response protocol for block and mempool synchronization.
    let req_resp = request_response::cbor::Behaviour::<crate::network::SyncRequest, crate::network::SyncResponse>::new(
        [(StreamProtocol::new("/qbtc/sync/2.0.0"), request_response::ProtocolSupport::Full)],
        req_resp_config,
    );

    // 5. Kademlia DHT configuration for peer discovery.
    let mut kad_config = kad::Config::default();
    // Isolate Kademlia network strictly to the QBTC protocol namespace.
    kad_config.set_protocol_names(vec![StreamProtocol::new("/qbtc/kad/1.0.0")]);
    
    let kad_store = kad::store::MemoryStore::new(local_peer_id);
    let mut kad = kad::Behaviour::with_config(local_peer_id, kad_store, kad_config);

    // Asymmetric Bootstrap Architecture.
    // Mode must be set on the active behaviour, not the config, to allow future runtime transitions.
    // Seed nodes operate as Servers to anchor the DHT routing tables.
    // Standard nodes operate as Clients to prevent NAT-induced routing pollution.
    if is_seed_node {
        kad.set_mode(Some(kad::Mode::Server));
    } else {
        kad.set_mode(Some(kad::Mode::Client));
    }

    // libp2p 0.53+ enforcing strict drop mechanism against Slowloris attacks.
    let ping = libp2p::ping::Behaviour::new(
        libp2p::ping::Config::new()
            .with_interval(Duration::from_secs(30))
            .with_timeout(Duration::from_secs(20)),
    );

    let behaviour = QbtcBehaviour { gossipsub, mdns, identify, kad, req_resp, ping };

    let swarm = SwarmBuilder::with_existing_identity(local_key)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            // Wrap the Yamux configuration in a closure to satisfy the FnOnce trait bound.
            // This cleanly breaks the 256KB physical transmission ceiling.
            || {
                let mut cfg = yamux::Config::default();
                #[allow(deprecated)]
                cfg.set_max_buffer_size(8 * 1024 * 1024);
                #[allow(deprecated)]
                cfg.set_receive_window_size(8 * 1024 * 1024);
                cfg
            }
        )?
        // Wrap the TCP transport layer with a DNS resolver for seed domain resolution.
        .with_dns()?
        .with_behaviour(|_| behaviour).expect("Behaviour merged successfully")
        // Extend idle timeout to 60s to prevent disconnects during heavy cryptographic validation.
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    Ok(swarm)
}