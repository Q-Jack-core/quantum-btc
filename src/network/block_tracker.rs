// src/network/block_tracker.rs
use libp2p::PeerId;
use std::collections::HashMap;
use std::hash::Hash;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    RequestedCompact { peer: PeerId, request_time: u64 },
    RequestedFull { peer: PeerId, request_time: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrackerAction {
    None,
    FetchFullBlock { peer: PeerId, hash: [u8; 32] },
    RescueSync { peer: PeerId },
}

pub struct BlockDownloadTracker<ReqId = libp2p::request_response::OutboundRequestId> {
    in_flight: HashMap<[u8; 32], TaskStatus>,
    req_map: HashMap<ReqId, [u8; 32]>,
}

impl<ReqId: Copy + Eq + Hash> BlockDownloadTracker<ReqId> {
    pub fn new() -> Self {
        Self {
            in_flight: HashMap::new(),
            req_map: HashMap::new(),
        }
    }

    fn current_time_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    pub fn on_request_compact(&mut self, req_id: ReqId, hash: [u8; 32], peer: PeerId) {
        if let Some(status) = self.in_flight.get_mut(&hash) {
            match status {
                TaskStatus::RequestedFull { .. } => return,
                TaskStatus::RequestedCompact { peer: p, .. } => {
                    *p = peer;
                    self.req_map.retain(|_, h| h != &hash);
                    self.req_map.insert(req_id, hash);
                    return;
                }
            }
        }
        self.req_map.retain(|_, h| h != &hash);
        self.req_map.insert(req_id, hash);
        self.in_flight.insert(
            hash,
            TaskStatus::RequestedCompact {
                peer,
                request_time: Self::current_time_ms(),
            },
        );
    }

    pub fn on_compact_failed(&mut self, req_id: &ReqId) -> TrackerAction {
        if let Some(hash) = self.req_map.remove(req_id) {
            if let Some(status) = self.in_flight.get_mut(&hash) {
                if let TaskStatus::RequestedCompact { peer, .. } = status {
                    let target_peer = *peer;
                    *status = TaskStatus::RequestedFull {
                        peer: target_peer,
                        request_time: Self::current_time_ms(),
                    };
                    return TrackerAction::FetchFullBlock {
                        peer: target_peer,
                        hash,
                    };
                }
            }
        }
        TrackerAction::None
    }

    pub fn register_full_request(&mut self, req_id: ReqId, hash: [u8; 32]) {
        if self.in_flight.contains_key(&hash) {
            self.req_map.retain(|_, h| h != &hash);
            self.req_map.insert(req_id, hash);
        }
    }

    pub fn on_block_saved(&mut self, hash: &[u8; 32]) {
        self.in_flight.remove(hash);
        self.req_map.retain(|_, h| h != hash);
    }

    pub fn on_network_failure(&mut self, req_id: &ReqId) -> TrackerAction {
        if let Some(hash) = self.req_map.remove(req_id) {
            if let Some(status) = self.in_flight.get_mut(&hash) {
                match status {
                    TaskStatus::RequestedCompact { peer, .. } => {
                        let target_peer = *peer;
                        *status = TaskStatus::RequestedFull {
                            peer: target_peer,
                            request_time: Self::current_time_ms(),
                        };
                        return TrackerAction::FetchFullBlock {
                            peer: target_peer,
                            hash,
                        };
                    }
                    TaskStatus::RequestedFull { peer, .. } => {
                        let target_peer = *peer;
                        self.in_flight.remove(&hash);
                        self.req_map.retain(|_, h| h != &hash);
                        return TrackerAction::RescueSync { peer: target_peer };
                    }
                }
            }
        }
        TrackerAction::None
    }

    pub fn prune_stale_tasks(&mut self, timeout_ms: u64) -> Vec<[u8; 32]> {
        let now = Self::current_time_ms();
        let stale_hashes: Vec<[u8; 32]> = self
            .in_flight
            .iter()
            .filter_map(|(hash, status)| {
                let req_time = match status {
                    TaskStatus::RequestedCompact { request_time, .. } => *request_time,
                    TaskStatus::RequestedFull { request_time, .. } => *request_time,
                };
                if now.saturating_sub(req_time) >= timeout_ms {
                    Some(*hash)
                } else {
                    None
                }
            })
            .collect();

        for h in &stale_hashes {
            self.in_flight.remove(h);
        }
        self.req_map.retain(|_, h| self.in_flight.contains_key(h));
        stale_hashes
    }

    pub fn is_in_flight(&self, hash: &[u8; 32]) -> bool {
        self.in_flight.contains_key(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[test]
    fn test_compact_lifecycle_success() {
        let mut tracker: BlockDownloadTracker<u64> = BlockDownloadTracker::new();
        let peer = PeerId::random();
        let hash = [1u8; 32];
        let req_id = 1001u64;

        tracker.on_request_compact(req_id, hash, peer);
        assert!(tracker.is_in_flight(&hash));

        tracker.on_block_saved(&hash);
        assert!(!tracker.is_in_flight(&hash));
    }

    #[test]
    fn test_compact_miss_transitions_to_full_block() {
        let mut tracker: BlockDownloadTracker<u64> = BlockDownloadTracker::new();
        let peer = PeerId::random();
        let hash = [2u8; 32];
        let req_id = 1002u64;

        tracker.on_request_compact(req_id, hash, peer);
        let action = tracker.on_compact_failed(&req_id);

        assert_eq!(action, TrackerAction::FetchFullBlock { peer, hash });
        match tracker.in_flight.get(&hash).unwrap() {
            TaskStatus::RequestedFull { peer: p, .. } => assert_eq!(*p, peer),
            _ => panic!("Expected RequestedFull status"),
        }
    }

    #[test]
    fn test_concurrent_tasks_isolation() {
        let mut tracker: BlockDownloadTracker<u64> = BlockDownloadTracker::new();
        let peer1 = PeerId::random();
        let peer2 = PeerId::random();
        let hash1 = [10u8; 32];
        let hash2 = [20u8; 32];

        tracker.on_request_compact(1u64, hash1, peer1);
        tracker.on_request_compact(2u64, hash2, peer2);

        let action1 = tracker.on_compact_failed(&1u64);
        assert_eq!(
            action1,
            TrackerAction::FetchFullBlock {
                peer: peer1,
                hash: hash1
            }
        );

        tracker.on_block_saved(&hash2);
        assert!(!tracker.is_in_flight(&hash2));
        assert!(tracker.is_in_flight(&hash1));
    }

    #[test]
    fn test_network_failure_and_stale_pruning() {
        let mut tracker: BlockDownloadTracker<u64> = BlockDownloadTracker::new();
        let peer = PeerId::random();
        let hash = [99u8; 32];

        tracker.on_request_compact(10u64, hash, peer);
        let action = tracker.on_network_failure(&10u64);
        assert_eq!(action, TrackerAction::FetchFullBlock { peer, hash });

        tracker.prune_stale_tasks(0);
        assert_eq!(tracker.in_flight.len(), 0);
        assert_eq!(tracker.req_map.len(), 0);
    }

    #[test]
    fn test_duplicate_announcement_idempotency() {
        let mut tracker: BlockDownloadTracker<u64> = BlockDownloadTracker::new();
        let peer1 = PeerId::random();
        let peer2 = PeerId::random();
        let hash = [77u8; 32];

        tracker.on_request_compact(1u64, hash, peer1);
        assert!(tracker.is_in_flight(&hash));

        tracker.on_request_compact(2u64, hash, peer2);
        assert!(tracker.is_in_flight(&hash));

        let action_stale = tracker.on_compact_failed(&1u64);
        assert_eq!(action_stale, TrackerAction::None);

        let action_active = tracker.on_compact_failed(&2u64);
        assert_eq!(
            action_active,
            TrackerAction::FetchFullBlock { peer: peer2, hash }
        );
    }

    #[test]
    fn test_byzantine_unmapped_request_resilience() {
        let mut tracker: BlockDownloadTracker<u64> = BlockDownloadTracker::new();
        let bogus_req_id = 99999u64;

        assert_eq!(
            tracker.on_compact_failed(&bogus_req_id),
            TrackerAction::None
        );
        assert_eq!(
            tracker.on_network_failure(&bogus_req_id),
            TrackerAction::None
        );
    }

    #[test]
    fn test_out_of_order_block_cascade() {
        let mut tracker: BlockDownloadTracker<u64> = BlockDownloadTracker::new();
        let peer = PeerId::random();
        let hash_n = [100u8; 32];
        let hash_n_plus_1 = [101u8; 32];

        tracker.on_request_compact(10u64, hash_n, peer);
        tracker.on_request_compact(11u64, hash_n_plus_1, peer);

        let action_n = tracker.on_compact_failed(&10u64);
        assert_eq!(
            action_n,
            TrackerAction::FetchFullBlock { peer, hash: hash_n }
        );
        tracker.register_full_request(12u64, hash_n);

        tracker.on_block_saved(&hash_n_plus_1);
        assert!(!tracker.is_in_flight(&hash_n_plus_1));
        assert!(tracker.is_in_flight(&hash_n));

        tracker.on_block_saved(&hash_n);
        assert!(!tracker.is_in_flight(&hash_n));
        assert_eq!(tracker.req_map.len(), 0);
    }

    #[test]
    fn test_full_failure_escalates_to_rescue_sync() {
        let mut tracker: BlockDownloadTracker<u64> = BlockDownloadTracker::new();
        let peer = PeerId::random();
        let hash = [55u8; 32];

        tracker.on_request_compact(1u64, hash, peer);
        let action_compact = tracker.on_compact_failed(&1u64);
        assert_eq!(action_compact, TrackerAction::FetchFullBlock { peer, hash });

        tracker.register_full_request(2u64, hash);

        let action_full = tracker.on_network_failure(&2u64);
        assert_eq!(action_full, TrackerAction::RescueSync { peer });
        assert!(!tracker.is_in_flight(&hash));
    }

    #[test]
    fn test_high_concurrency_chaos_fuzzing() {
        let mut tracker: BlockDownloadTracker<u64> = BlockDownloadTracker::new();
        let peers: Vec<PeerId> = (0..10).map(|_| PeerId::random()).collect();
        let mut req_counter = 1u64;

        for i in 0..5000u64 {
            let mut hash = [0u8; 32];
            hash[..8].copy_from_slice(&(i % 50).to_be_bytes());
            let peer = peers[(i % 10) as usize];
            let req_id = req_counter;
            req_counter += 1;

            match i % 4 {
                0 => tracker.on_request_compact(req_id, hash, peer),
                1 => {
                    let _ = tracker.on_compact_failed(&req_id);
                }
                2 => tracker.on_block_saved(&hash),
                3 => {
                    let _ = tracker.on_network_failure(&req_id);
                }
                _ => unreachable!(),
            }
        }

        let active_hashes: HashSet<[u8; 32]> = tracker.in_flight.keys().copied().collect();
        for target_hash in tracker.req_map.values() {
            assert!(
                active_hashes.contains(target_hash),
                "Invariant Broken: Ghost request in req_map"
            );
        }

        tracker.prune_stale_tasks(0);
        assert_eq!(tracker.in_flight.len(), 0);
        assert_eq!(tracker.req_map.len(), 0);
    }

    #[test]
    fn test_million_scale_fuzzing_invariants() {
        let mut tracker: BlockDownloadTracker<u64> = BlockDownloadTracker::new();
        let peers: Vec<PeerId> = (0..16).map(|_| PeerId::random()).collect();
        let mut pseudo_rng: u64 = 0xdeadbeef12345678;
        let mut live_req_ids: Vec<u64> = Vec::with_capacity(1024);

        for step in 0..1_000_000u64 {
            pseudo_rng = pseudo_rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let block_slot = (pseudo_rng >> 32) % 128;
            let mut hash = [0u8; 32];
            hash[0..8].copy_from_slice(&block_slot.to_be_bytes());

            let peer = peers[((pseudo_rng >> 24) % 16) as usize];
            let action_type = (pseudo_rng >> 16) % 6;

            match action_type {
                0 | 1 => {
                    let req_id = step + 1;
                    tracker.on_request_compact(req_id, hash, peer);
                    live_req_ids.push(req_id);
                }
                2 => {
                    let req_id = if !live_req_ids.is_empty() && (pseudo_rng & 1 == 0) {
                        let idx = ((pseudo_rng >> 8) as usize) % live_req_ids.len();
                        live_req_ids.swap_remove(idx)
                    } else {
                        step + 1
                    };
                    if let TrackerAction::FetchFullBlock { hash, .. } =
                        tracker.on_compact_failed(&req_id)
                    {
                        let full_req_id = step + 1_000_000_000;
                        tracker.register_full_request(full_req_id, hash);
                        live_req_ids.push(full_req_id);
                    }
                }
                3 => {
                    tracker.on_block_saved(&hash);
                }
                4 => {
                    let req_id = if !live_req_ids.is_empty() && (pseudo_rng & 1 == 0) {
                        let idx = ((pseudo_rng >> 8) as usize) % live_req_ids.len();
                        live_req_ids.swap_remove(idx)
                    } else {
                        step + 1
                    };
                    let _ = tracker.on_network_failure(&req_id);
                }
                5 => {
                    tracker.prune_stale_tasks(30_000);
                }
                _ => unreachable!(),
            }

            if live_req_ids.len() > 4096 {
                live_req_ids.truncate(1024);
            }

            if step % 100_000 == 0 {
                let active_hashes: HashSet<[u8; 32]> = tracker.in_flight.keys().copied().collect();
                for target_hash in tracker.req_map.values() {
                    assert!(
                        active_hashes.contains(target_hash),
                        "Critical Invariant Broken at step {}: Ghost request target in req_map",
                        step
                    );
                }
            }
        }

        tracker.prune_stale_tasks(0);
        assert_eq!(tracker.in_flight.len(), 0);
        assert_eq!(tracker.req_map.len(), 0);
    }

    #[test]
    fn test_deep_reorg_abandoned_branch_pruning() {
        let mut tracker: BlockDownloadTracker<u64> = BlockDownloadTracker::new();
        let peer = PeerId::random();

        for i in 0..100u64 {
            let mut hash_a = [0u8; 32];
            hash_a[0..8].copy_from_slice(&i.to_be_bytes());
            hash_a[31] = 0xAA;
            tracker.on_request_compact(i, hash_a, peer);
        }
        assert_eq!(tracker.in_flight.len(), 100);

        for i in 0..100u64 {
            let mut hash_b = [0u8; 32];
            hash_b[0..8].copy_from_slice(&i.to_be_bytes());
            hash_b[31] = 0xBB;
            tracker.on_block_saved(&hash_b);
        }

        tracker.prune_stale_tasks(0);
        assert_eq!(tracker.in_flight.len(), 0);
        assert_eq!(tracker.req_map.len(), 0);
    }

    #[test]
    fn test_byzantine_sybil_flooding_and_stale_cleanup() {
        let mut tracker: BlockDownloadTracker<u64> = BlockDownloadTracker::new();
        let hash = [0xEE; 32];
        let sybil_peers: Vec<PeerId> = (0..50).map(|_| PeerId::random()).collect();

        for (i, sybil) in sybil_peers.iter().enumerate() {
            tracker.on_request_compact(i as u64, hash, *sybil);
        }

        assert_eq!(tracker.in_flight.len(), 1);
        assert_eq!(tracker.req_map.len(), 1);

        for i in 0..49u64 {
            assert_eq!(tracker.on_compact_failed(&i), TrackerAction::None);
        }

        let action = tracker.on_compact_failed(&49u64);
        assert_eq!(
            action,
            TrackerAction::FetchFullBlock {
                peer: sybil_peers[49],
                hash
            }
        );
    }

    #[test]
    fn test_illegal_state_transition_matrix() {
        let mut tracker: BlockDownloadTracker<u64> = BlockDownloadTracker::new();
        let peer = PeerId::random();
        let hash = [0x5A; 32];

        assert_eq!(tracker.on_compact_failed(&404), TrackerAction::None);
        assert_eq!(tracker.on_network_failure(&404), TrackerAction::None);
        tracker.on_block_saved(&hash);

        tracker.on_request_compact(100, hash, peer);
        tracker.on_block_saved(&hash);
        assert_eq!(tracker.on_compact_failed(&100), TrackerAction::None);
        assert_eq!(tracker.on_network_failure(&100), TrackerAction::None);

        tracker.on_request_compact(200, hash, peer);
        let action = tracker.on_compact_failed(&200);
        assert_eq!(action, TrackerAction::FetchFullBlock { peer, hash });
        tracker.register_full_request(201, hash);
        tracker.on_block_saved(&hash);

        assert_eq!(tracker.on_network_failure(&201), TrackerAction::None);
        assert_eq!(tracker.in_flight.len(), 0);
        assert_eq!(tracker.req_map.len(), 0);
    }

    #[test]
    fn test_edge_case_hashes() {
        let mut tracker: BlockDownloadTracker<u64> = BlockDownloadTracker::new();
        let peer = PeerId::random();
        let null_hash = [0x00; 32];
        let max_hash = [0xFF; 32];

        tracker.on_request_compact(1, null_hash, peer);
        tracker.on_request_compact(2, max_hash, peer);

        assert_eq!(
            tracker.on_compact_failed(&1),
            TrackerAction::FetchFullBlock {
                peer,
                hash: null_hash,
            }
        );
        assert_eq!(
            tracker.on_compact_failed(&2),
            TrackerAction::FetchFullBlock {
                peer,
                hash: max_hash,
            }
        );

        tracker.on_block_saved(&null_hash);
        tracker.on_block_saved(&max_hash);

        assert_eq!(tracker.in_flight.len(), 0);
        assert_eq!(tracker.req_map.len(), 0);
    }

    #[test]
    fn test_multithreaded_pipeline_simulation() {
        let tracker = Arc::new(Mutex::new(BlockDownloadTracker::<u64>::new()));
        let mut handles = Vec::new();

        for t in 0..8u64 {
            let tracker_clone = Arc::clone(&tracker);
            let handle = thread::spawn(move || {
                let peer = PeerId::random();
                for i in 0..500u64 {
                    let req_id = t * 10_000 + i;
                    let mut hash = [0u8; 32];
                    hash[0..8].copy_from_slice(&req_id.to_be_bytes());

                    {
                        let mut trk = tracker_clone.lock().unwrap();
                        trk.on_request_compact(req_id, hash, peer);
                    }

                    if i % 2 == 0 {
                        let mut trk = tracker_clone.lock().unwrap();
                        let _ = trk.on_compact_failed(&req_id);
                    } else {
                        let mut trk = tracker_clone.lock().unwrap();
                        trk.on_block_saved(&hash);
                    }
                }
            });
            handles.push(handle);
        }

        for h in handles {
            h.join().unwrap();
        }

        let mut trk = tracker.lock().unwrap();
        trk.prune_stale_tasks(0);
        assert_eq!(trk.in_flight.len(), 0);
        assert_eq!(trk.req_map.len(), 0);
    }

    #[test]
    fn test_large_capacity_memory_profile() {
        let mut tracker: BlockDownloadTracker<u64> = BlockDownloadTracker::new();
        let peer = PeerId::random();

        for i in 0..20_000u64 {
            let mut hash = [0u8; 32];
            hash[0..8].copy_from_slice(&i.to_be_bytes());
            tracker.on_request_compact(i, hash, peer);
        }
        assert_eq!(tracker.in_flight.len(), 20_000);
        assert_eq!(tracker.req_map.len(), 20_000);

        for i in 0..10_000u64 {
            let mut hash = [0u8; 32];
            hash[0..8].copy_from_slice(&i.to_be_bytes());
            tracker.on_block_saved(&hash);
        }
        assert_eq!(tracker.in_flight.len(), 10_000);
        assert_eq!(tracker.req_map.len(), 10_000);

        tracker.prune_stale_tasks(0);
        assert_eq!(tracker.in_flight.len(), 0);
        assert_eq!(tracker.req_map.len(), 0);
    }
}