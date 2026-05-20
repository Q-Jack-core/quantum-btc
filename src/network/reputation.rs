// src/network/reputation.rs
use libp2p::PeerId;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

// Constants for peer behavior evaluation.
pub const INITIAL_TRUST: i32 = 100;
pub const BAN_THRESHOLD: i32 = 0;

// Penalty point definitions.
pub const PENALTY_INVALID_HEADER: i32 = 50;  
pub const PENALTY_MALFORMED_DATA: i32 = 20;  
pub const PENALTY_TIMEOUT: i32 = 10;         
pub const PENALTY_INVALID_SIG: i32 = 100;    
pub const PENALTY_SLOW_PEER: i32 = 100;      
pub const REWARD_VALID_BLOCK: i32 = 5;       

// Physical memory constraint to prevent OOM and LRU Cache Poisoning.
const MAX_PROFILES: usize = 8192;

/// State-aware record for individual peer behavior.
pub struct PeerProfile {
    pub score: i32,
    pub consecutive_timeouts: u32,
    pub last_active: u64,
}

/// Strictly typed network offenses to ensure accurate judgment.
pub enum NetworkOffense {
    Timeout,
    InvalidSignature,
    MalformedData,
    InvalidHeader,
    SlowPeer,
}

/// Advanced peer scoring engine to mitigate DDoS, Sybil, and Eclipse attacks.
pub struct ReputationManager {
    profiles: HashMap<PeerId, PeerProfile>,
}

impl ReputationManager {
    pub fn new() -> Self {
        Self { profiles: HashMap::new() }
    }

    fn current_time() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    /// Garbage collection protecting honest nodes during mass connection attacks.
    fn enforce_capacity_limit(&mut self) {
        if self.profiles.len() <= MAX_PROFILES {
            return;
        }
        // Immunity rule: Retain nodes with good standing. Purge untrusted cache.
        self.profiles.retain(|_, profile| profile.score >= INITIAL_TRUST);
    }

    /// Retrieves or initializes a profile, updating the activity timestamp.
    fn update_activity(&mut self, peer_id: &PeerId) -> &mut PeerProfile {
        self.enforce_capacity_limit();
        let now = Self::current_time();
        let profile = self.profiles.entry(*peer_id).or_insert(PeerProfile {
            score: INITIAL_TRUST,
            consecutive_timeouts: 0,
            last_active: now,
        });
        // Only advance the activity clock for peers in good standing.
        if profile.score > BAN_THRESHOLD {
            profile.last_active = now;
        }
        profile
    }

    pub fn is_trusted(&mut self, peer_id: &PeerId) -> bool {
        let now = Self::current_time();
        
        if let Some(profile) = self.profiles.get_mut(peer_id) {
            if profile.score <= BAN_THRESHOLD {
                // Ban expiration evaluation: 2 Hours = 7200 seconds
                if now.saturating_sub(profile.last_active) >= 7200 {
                    tracing::info!("[INFO] Reputation: Ban expired for peer {}. Trust reset.", peer_id);
                    profile.score = INITIAL_TRUST;
                    profile.consecutive_timeouts = 0;
                    profile.last_active = now; 
                    return true;
                }
                return false; 
            }
        }
        // For honest peers or unrecorded peers, process normally.
        self.update_activity(peer_id).score > BAN_THRESHOLD
    }

    /// Processes offenses with context-aware memory.
    /// Returns true if the peer hits the ban threshold and requires physical disconnection.
    pub fn report_offense(&mut self, peer_id: &PeerId, offense: NetworkOffense) -> bool {
        let profile = self.update_activity(peer_id);
        
        match offense {
            NetworkOffense::Timeout => {
                profile.consecutive_timeouts += 1;
                // Performance tolerance for trans-oceanic ML-DSA-65 transmission.
                // Do not penalize score for network latency to prevent honest node blacklisting.
                // Underlying libp2p timeout will automatically handle the stream teardown.
            },
            NetworkOffense::InvalidSignature => profile.score -= PENALTY_INVALID_SIG,
            NetworkOffense::MalformedData => profile.score -= PENALTY_MALFORMED_DATA,
            NetworkOffense::InvalidHeader => profile.score -= PENALTY_INVALID_HEADER,
            NetworkOffense::SlowPeer => profile.score -= PENALTY_SLOW_PEER,
        }

        if profile.score <= BAN_THRESHOLD {
            tracing::warn!("[WARN] Reputation: Peer banned. Identity: {}", peer_id);
            true
        } else {
            tracing::info!("[INFO] Reputation: Penalty applied to {}. Current score: {}", peer_id, profile.score);
            false
        }
    }

    /// Standard reward for passive gossip participation. Does not reset timeouts.
    pub fn reward_gossip(&mut self, peer_id: &PeerId) {
        let profile = self.update_activity(peer_id);
        // Prevent score manipulation: Banned peers cannot earn passive rewards.
        if profile.score > BAN_THRESHOLD && profile.score < INITIAL_TRUST {
            profile.score = (profile.score + REWARD_VALID_BLOCK).min(INITIAL_TRUST);
            tracing::info!("[INFO] Reputation: Reward applied to {}. Current score: {}", peer_id, profile.score);
        }
    }

    /// Critical reward. Validates successful direct sync and resets timeout memory.
    pub fn reward_sync_success(&mut self, peer_id: &PeerId) {
        let profile = self.update_activity(peer_id);
        // L1 DEFENSE FIX: Prevent Zombie Resurrection.
        if profile.score > BAN_THRESHOLD {
            profile.consecutive_timeouts = 0; // Absolute reset on honest response
            if profile.score < INITIAL_TRUST {
                profile.score = (profile.score + REWARD_VALID_BLOCK).min(INITIAL_TRUST);
            }
        }
    }

    pub fn get_score(&mut self, peer_id: &PeerId) -> i32 {
        self.update_activity(peer_id).score
    }
}