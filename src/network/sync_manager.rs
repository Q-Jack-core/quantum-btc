// src/network/sync_manager.rs

use std::collections::HashMap;

/// Defines the processing result of block synchronization.
#[derive(Debug, PartialEq)]
pub enum SyncResult {
    /// Perfect sequential block, ready to be passed to the consensus engine.
    Accepted,
    /// Missing ancestry detected! Buffered in the orphan pool instead of purging.
    BufferedAsOrphan,
    /// Malformed or stale data, rejected safely.
    Rejected(String),
}

/// Synchronization manager handling the orphan block pool and sequence resolution.
pub struct SyncManager {
    /// The latest confirmed physical height of the local node.
    pub current_height: u64,
    /// The orphan pool: Height -> Block data (Using String for TDD simulation purposes).
    pub orphan_pool: HashMap<u64, String>, 
}

impl SyncManager {
    pub fn new(start_height: u64) -> Self {
        Self {
            current_height: start_height,
            orphan_pool: HashMap::new(),
        }
    }

    /// Core logic: Process incoming blocks pushed from the P2P network.
    pub fn process_incoming_block(&mut self, block_height: u64, block_data: String) -> SyncResult {
        // L1 DEFENSE: Reject stale blocks instantly to save CPU.
        if block_height <= self.current_height {
            return SyncResult::Rejected("Stale block".to_string());
        }

        if block_height == self.current_height + 1 {
            // Step 1: Accept the perfectly sequenced block.
            self.current_height = block_height;
            
            // Resolve sequential blocks from the orphan pool.
            let mut next_expected = self.current_height + 1;
            while self.orphan_pool.remove(&next_expected).is_some() {
                self.current_height = next_expected;
                next_expected += 1;
            }
            
            SyncResult::Accepted
        } else {
            // Buffer it in the orphan pool to survive out-of-order network storms.
            self.orphan_pool.insert(block_height, block_data);
            SyncResult::BufferedAsOrphan
        }
    }

    /// Batch processing logic for bulk Headers or Blocks.
    /// Specifically designed to handle bulk Headers or Blocks received from the network.
    pub fn process_batch(&mut self, batch: Vec<(u64, String)>) -> (usize, usize) {
        let mut accepted_count = 0;
        let mut buffered_count = 0;

        for (height, data) in batch {
            match self.process_incoming_block(height, data) {
                SyncResult::Accepted => accepted_count += 1,
                SyncResult::BufferedAsOrphan => buffered_count += 1,
                // Safely ignore rejected blocks in batch processing.
                SyncResult::Rejected(_) => {}, 
            }
        }
        (accepted_count, buffered_count)
    }
}



// =====================================================================
// Network Synchronization Debouncer & Circuit Breaker
// =====================================================================


/// Tracks the penalty state of a network peer.
#[derive(Debug, Default)]
pub struct PeerState {
    /// Number of consecutive zero-progress warnings.
    pub strikes: u32,             
    /// Virtual timestamp (ms) when the ban expires.
    pub banned_until_vtime: u64,  
}

/// Core Brain: In-Flight Tracker + Circuit Breaker.
pub struct SyncDebouncer {
    /// Records [Block Hash -> Virtual timestamp of request].
    pub in_flight: HashMap<[u8; 32], u64>,
    /// Records [Peer ID -> Peer State].
    pub peer_states: HashMap<String, PeerState>,
    /// Physical timeout TTL (in milliseconds).
    pub ttl_ms: u64,
}

impl SyncDebouncer {
    pub fn new(ttl_ms: u64) -> Self {
        Self {
            in_flight: HashMap::new(),
            peer_states: HashMap::new(),
            ttl_ms,
        }
    }

    /// Line of Defense 1 & 2: Hash-level Debounce Lock + TTL Forced Eviction.
    /// Returns true to allow request; false to intercept duplicate request.
    pub fn should_request(&mut self, hash: [u8; 32], current_vtime: u64) -> bool {
        if let Some(&req_time) = self.in_flight.get(&hash) {
            // If the request is still alive within TTL, block it strictly! 
            // Prevents infinite loop network flooding.
            if current_vtime < req_time + self.ttl_ms {
                return false; 
            }
            // TTL expired: The previous request hit a "network black hole" and was dropped.
            // Evict old record physically and allow the new request!
        }
        // Allow request and stamp with the latest virtual time.
        self.in_flight.insert(hash, current_vtime);
        true
    }

    /// Peer State Adjudication: Checks if the peer is currently banned.
    pub fn is_peer_banned(&self, peer_id: &str, current_vtime: u64) -> bool {
        if let Some(state) = self.peer_states.get(peer_id) {
            return current_vtime < state.banned_until_vtime;
        }
        false
    }

    /// Line of Defense 3: Smart Progress Detection & Exponential Backoff.
    pub fn report_peer_progress(&mut self, peer_id: &str, has_progress: bool, current_vtime: u64) {
        let state = self.peer_states.entry(peer_id.to_string()).or_insert(PeerState::default());
        
        if has_progress {
            // Real progress detected (height increased or orphan pool grew).
            // Wipe all strike records immediately!
            state.strikes = 0; 
        } else {
            // Zero progress, increment strike count.
            state.strikes += 1;
            
            // 3 consecutive strikes trip the circuit breaker!
            if state.strikes >= 3 {
                // Exponential backoff algorithm: 1s, 2s, 4s, 8s... (in ms).
                let exponent = state.strikes - 3;
                // Prevent overflow, cap max penalty at approx 1 hour.
                let penalty_ms = if exponent > 12 { 3_600_000 } else { (1 << exponent) * 1000 };
                
                state.banned_until_vtime = current_vtime + penalty_ms as u64;
            }
        }
    }
}
