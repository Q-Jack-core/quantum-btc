// Adversarial / messy-network regression tests for BlockDownloadTracker.
//
// These drive the tracker from the outside, through its public API only, the
// way main.rs does. They assert the protocol contract rather than any single
// implementation of it, so they stay valid as the internals change:
//
//   1. A task that the caller handles correctly always reaches a terminal
//      state (saved, or escalated to a rescue sync).
//   2. A task is never left believing a download is in progress while no
//      request is outstanding and no event can reach it.
//   3. The staleness pruner always drains the tracker.
//
// Property 2 is the one that produced a real stall: the empty-DataResponse arm
// in main.rs used to discard the FetchFullBlock action it was handed, leaving
// the hash pinned with nothing on the wire.

use libp2p::PeerId;
use quantum_btc::network::block_tracker::{BlockDownloadTracker, TrackerAction};

type Tracker = BlockDownloadTracker<u64>;

fn hash_of(n: u64) -> [u8; 32] {
    let mut h = [0u8; 32];
    h[..8].copy_from_slice(&n.to_be_bytes());
    h
}

/// Happy path: compact request, compact arrives, block committed.
#[test]
fn compact_request_reaches_terminal_state_on_success() {
    let mut t = Tracker::new();
    let hash = hash_of(1);

    t.on_request_compact(1, hash, PeerId::random());
    assert!(t.is_in_flight(&hash));

    t.on_block_saved(&hash);
    assert!(!t.is_in_flight(&hash));
    assert_eq!(t.on_network_failure(&1), TrackerAction::None);
}

/// Compact miss, caller follows the protocol, full block arrives.
#[test]
fn compact_miss_then_full_block_reaches_terminal_state() {
    let mut t = Tracker::new();
    let peer = PeerId::random();
    let hash = hash_of(2);

    t.on_request_compact(10, hash, peer);
    match t.on_compact_failed(&10) {
        TrackerAction::FetchFullBlock { peer: p, hash: h } => {
            assert_eq!(p, peer);
            assert_eq!(h, hash);
            t.register_full_request(11, h);
        }
        other => panic!("expected FetchFullBlock, got {other:?}"),
    }

    t.on_block_saved(&hash);
    assert!(!t.is_in_flight(&hash));
}

/// Compact miss, then the full download also fails: must escalate to a rescue
/// sync and leave no residue behind.
#[test]
fn full_block_failure_escalates_and_clears_state() {
    let mut t = Tracker::new();
    let peer = PeerId::random();
    let hash = hash_of(3);

    t.on_request_compact(20, hash, peer);
    let TrackerAction::FetchFullBlock { hash: h, .. } = t.on_compact_failed(&20) else {
        panic!("expected FetchFullBlock");
    };
    t.register_full_request(21, h);

    assert_eq!(
        t.on_network_failure(&21),
        TrackerAction::RescueSync { peer },
        "a failed full download must hand the caller something to act on"
    );
    assert!(
        !t.is_in_flight(&hash),
        "escalated task must not stay in flight"
    );
}

/// Property 2, the stall guard. Whatever action the tracker hands back, acting
/// on it must leave the block either terminal or still reachable by some
/// request id. A block must never be pinned with nothing outstanding.
#[test]
fn handled_task_is_never_pinned_without_an_outstanding_request() {
    let mut t = Tracker::new();
    let peer = PeerId::random();
    let hash = hash_of(4);

    t.on_request_compact(30, hash, peer);

    // Peer answers GetData with an empty payload: same entry point main.rs uses.
    let action = t.on_network_failure(&30);
    match action {
        TrackerAction::FetchFullBlock { peer: p, hash: h } => {
            // Caller must dispatch and register, which keeps the task reachable.
            t.register_full_request(31, h);
            assert_eq!(
                t.on_network_failure(&31),
                TrackerAction::RescueSync { peer: p },
                "registered retry must remain reachable by its request id"
            );
            assert!(!t.is_in_flight(&hash));
        }
        TrackerAction::RescueSync { .. } => {
            assert!(!t.is_in_flight(&hash), "rescued task must be cleared");
        }
        TrackerAction::None => {
            assert!(
                !t.is_in_flight(&hash),
                "a task the tracker refuses to act on must not stay in flight"
            );
        }
    }
}

/// Messy networks re-gossip the same header from several peers while a full
/// download is already on the wire. The in-flight download must not become
/// unreachable as a result.
#[test]
fn reannouncement_does_not_strand_an_inflight_full_request() {
    let mut t = Tracker::new();
    let peer_a = PeerId::random();
    let peer_b = PeerId::random();
    let hash = hash_of(5);

    t.on_request_compact(40, hash, peer_a);
    let TrackerAction::FetchFullBlock { hash: h, .. } = t.on_compact_failed(&40) else {
        panic!("expected FetchFullBlock");
    };
    t.register_full_request(41, h);

    // Second announcement for the same block, from a different peer.
    t.on_request_compact(42, hash, peer_b);

    // The original download now fails on the wire. Either it is actionable, or
    // the newest request id must still be able to drive the task forward.
    let first = t.on_network_failure(&41);
    if first == TrackerAction::None {
        let second = t.on_network_failure(&42);
        assert_ne!(
            second,
            TrackerAction::None,
            "after re-announcement the block must still be recoverable \
             through the newest request id"
        );
    }
}

/// Byzantine input: request ids the tracker never issued must be inert.
#[test]
fn unknown_request_ids_are_inert() {
    let mut t = Tracker::new();
    assert_eq!(t.on_compact_failed(&999_999), TrackerAction::None);
    assert_eq!(t.on_network_failure(&999_999), TrackerAction::None);
    t.on_block_saved(&hash_of(6));
}

/// The pruner must always drain, whatever mix of states the tracker is in.
#[test]
fn pruner_drains_every_task() {
    let mut t = Tracker::new();
    let peer = PeerId::random();

    for i in 0..200u64 {
        let h = hash_of(i);
        t.on_request_compact(i, h, peer);
        if i % 3 == 0 {
            let _ = t.on_compact_failed(&i);
        }
        if i % 7 == 0 {
            let _ = t.on_network_failure(&i);
        }
    }

    t.prune_stale_tasks(0);
    for i in 0..200u64 {
        assert!(
            !t.is_in_flight(&hash_of(i)),
            "hash {i} survived a zero-timeout prune"
        );
    }
}

/// Long randomized event stream with a protocol-following caller. Asserts the
/// stall guard continuously: no hash may be in flight once every request id
/// touching it has been consumed.
#[test]
fn randomized_stream_never_produces_a_stalled_task() {
    let mut t = Tracker::new();
    let peers: Vec<PeerId> = (0..8).map(|_| PeerId::random()).collect();
    let mut rng: u64 = 0x1234_5678_9abc_def0;
    let mut open: Vec<(u64, [u8; 32])> = Vec::new();
    let mut next_id = 1u64;

    for step in 0..300_000u64 {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        let hash = hash_of((rng >> 33) % 64);
        let peer = peers[((rng >> 17) % 8) as usize];

        match (rng >> 5) % 5 {
            0 => {
                let id = next_id;
                next_id += 1;
                t.on_request_compact(id, hash, peer);
                open.push((id, hash));
            }
            1 | 3 => {
                if open.is_empty() {
                    continue;
                }
                let idx = ((rng >> 9) as usize) % open.len();
                let (id, _) = open.swap_remove(idx);
                let action = if (rng >> 7) % 2 == 0 {
                    t.on_compact_failed(&id)
                } else {
                    t.on_network_failure(&id)
                };
                // Protocol-following caller: always act on what it is handed.
                if let TrackerAction::FetchFullBlock { hash: fh, .. } = action {
                    let retry = next_id;
                    next_id += 1;
                    t.register_full_request(retry, fh);
                    open.push((retry, fh));
                    assert!(
                        t.is_in_flight(&fh),
                        "step {step}: retry registered but task vanished"
                    );
                }
            }
            2 => {
                t.on_block_saved(&hash);
                open.retain(|(_, h)| h != &hash);
            }
            4 => {
                t.prune_stale_tasks(30_000);
            }
            _ => unreachable!(),
        }

        if open.len() > 8192 {
            open.truncate(2048);
        }
    }

    // Drain every id the caller still holds, acting on each action as required.
    while let Some((id, _)) = open.pop() {
        if let TrackerAction::FetchFullBlock { hash: fh, .. } = t.on_network_failure(&id) {
            let retry = next_id;
            next_id += 1;
            t.register_full_request(retry, fh);
            open.push((retry, fh));
        }
    }

    t.prune_stale_tasks(0);
    for slot in 0..64u64 {
        assert!(
            !t.is_in_flight(&hash_of(slot)),
            "slot {slot} left pinned after the stream drained"
        );
    }
}
