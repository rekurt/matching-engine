/// Phase 2 tests: SessionStore v2 — sharded, with dedup table and cumulative ACK.
///
/// These tests verify:
/// - Dedup table: lookup/insert with FIFO eviction
/// - Cumulative ACK purges replay buffer entries up to ack_seq
/// - ACK never regresses (out-of-order ACKs are ignored)
/// - Sharded isolation: sessions on different shards don't block each other
/// - Session TTL expiration still works
use std::time::Duration;

use bytes::Bytes;
use me_io::session_store::{ReplayResult, SessionStore};

/// Extract Ok frames from a ReplayResult, panicking on Gap or SessionNotFound.
fn expect_ok(r: ReplayResult) -> Vec<Bytes> {
    match r {
        ReplayResult::Ok(frames) => frames,
        other => panic!("expected Ok, got {other:?}"),
    }
}

// ─── Dedup table ────────────────────────────────────────────────────────────

#[test]
fn test_dedup_lookup_miss_on_fresh_session() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();
    assert_eq!(store.dedup_lookup(&sid, 42), None);
}

#[test]
fn test_dedup_insert_and_lookup() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();

    let response = Bytes::from_static(b"cached_response_bytes");
    store.dedup_insert(&sid, 42, response.clone());

    assert_eq!(store.dedup_lookup(&sid, 42), Some(response));
}

#[test]
fn test_dedup_different_request_ids_isolated() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();

    store.dedup_insert(&sid, 1, Bytes::from_static(b"response_1"));
    store.dedup_insert(&sid, 2, Bytes::from_static(b"response_2"));
    store.dedup_insert(&sid, 3, Bytes::from_static(b"response_3"));

    assert_eq!(
        store.dedup_lookup(&sid, 1),
        Some(Bytes::from_static(b"response_1"))
    );
    assert_eq!(
        store.dedup_lookup(&sid, 2),
        Some(Bytes::from_static(b"response_2"))
    );
    assert_eq!(
        store.dedup_lookup(&sid, 3),
        Some(Bytes::from_static(b"response_3"))
    );
}

#[test]
fn test_dedup_sessions_isolated() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid_a = store.create();
    let sid_b = store.create();

    store.dedup_insert(&sid_a, 42, Bytes::from_static(b"a_42"));
    store.dedup_insert(&sid_b, 42, Bytes::from_static(b"b_42"));

    assert_eq!(
        store.dedup_lookup(&sid_a, 42),
        Some(Bytes::from_static(b"a_42"))
    );
    assert_eq!(
        store.dedup_lookup(&sid_b, 42),
        Some(Bytes::from_static(b"b_42"))
    );
}

#[test]
fn test_dedup_fifo_eviction() {
    // Small dedup max to trigger eviction quickly
    let store = SessionStore::new_with_dedup(1024, 3, Duration::from_secs(60));
    let sid = store.create();

    store.dedup_insert(&sid, 1, Bytes::from_static(b"r1"));
    store.dedup_insert(&sid, 2, Bytes::from_static(b"r2"));
    store.dedup_insert(&sid, 3, Bytes::from_static(b"r3"));
    assert_eq!(store.dedup_len(&sid), 3);

    // Fourth insert should evict req_id=1 (oldest)
    store.dedup_insert(&sid, 4, Bytes::from_static(b"r4"));
    assert_eq!(store.dedup_len(&sid), 3);
    assert_eq!(store.dedup_lookup(&sid, 1), None, "oldest should be evicted");
    assert_eq!(
        store.dedup_lookup(&sid, 4),
        Some(Bytes::from_static(b"r4"))
    );
}

#[test]
fn test_dedup_reinsert_same_key_no_eviction() {
    let store = SessionStore::new_with_dedup(1024, 2, Duration::from_secs(60));
    let sid = store.create();

    store.dedup_insert(&sid, 1, Bytes::from_static(b"v1"));
    store.dedup_insert(&sid, 2, Bytes::from_static(b"v2"));

    // Re-inserting req_id=1 should NOT evict req_id=2
    store.dedup_insert(&sid, 1, Bytes::from_static(b"v1_updated"));
    assert_eq!(store.dedup_len(&sid), 2);
    assert_eq!(
        store.dedup_lookup(&sid, 1),
        Some(Bytes::from_static(b"v1_updated"))
    );
    assert_eq!(
        store.dedup_lookup(&sid, 2),
        Some(Bytes::from_static(b"v2"))
    );
}

#[test]
fn test_dedup_lookup_on_nonexistent_session() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let fake_sid = [0xDEu8; 16];
    assert_eq!(store.dedup_lookup(&fake_sid, 42), None);
}

// ─── Cumulative ACK ──────────────────────────────────────────────────────────

#[test]
fn test_apply_ack_purges_replay_buffer() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();

    // Push 10 frames with server_seq 1..=10
    for seq in 1..=10u64 {
        store.push(&sid, seq, Bytes::from_static(b"frame"));
    }
    assert_eq!(store.buffer_len(&sid), 10);

    // ACK up to seq 5 should remove frames 1..=5
    let purged = store.apply_ack(&sid, 5);
    assert_eq!(purged, 5);
    assert_eq!(store.buffer_len(&sid), 5);

    // Client reconnects claiming `last_recv_seq=5` (consistent with prior ACK).
    // Gap boundary is max(evicted, last_acked) = 5, so asking for frames > 5
    // is within-bounds — server replays frames 6..=10.
    let replayed = expect_ok(store.resume(&sid, 5));
    assert_eq!(replayed.len(), 5);
}

#[test]
fn test_resume_below_ack_watermark_returns_gap() {
    // State regression detection: client claims a last_recv_seq LOWER than
    // what it previously ACKed. This is a client bug (state loss), and the
    // server must surface it as a Gap rather than silently replaying.
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();

    for seq in 1..=10u64 {
        store.push(&sid, seq, Bytes::from_static(b"f"));
    }
    // Client ACKs up to 5 — server purges 1..=5.
    store.apply_ack(&sid, 5);

    // Then client reconnects saying "I only have up to 3" (state regression).
    // Server should refuse to silently replay 4,5 — those were ACKed, gone.
    match store.resume(&sid, 3) {
        ReplayResult::Gap { missing_from, missing_to } => {
            assert_eq!(missing_from, 4);
            assert_eq!(missing_to, 5);
        }
        other => panic!("expected Gap, got {other:?}"),
    }
}

#[test]
fn test_apply_ack_all() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();

    for seq in 1..=5u64 {
        store.push(&sid, seq, Bytes::from_static(b"f"));
    }
    let purged = store.apply_ack(&sid, 100);
    assert_eq!(purged, 5);
    assert_eq!(store.buffer_len(&sid), 0);
}

#[test]
fn test_apply_ack_zero_is_noop() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();

    for seq in 1..=3u64 {
        store.push(&sid, seq, Bytes::from_static(b"f"));
    }
    // ACK of 0 (initial state, client hasn't processed anything) — nothing evicted
    let purged = store.apply_ack(&sid, 0);
    assert_eq!(purged, 0);
    assert_eq!(store.buffer_len(&sid), 3);
}

#[test]
fn test_apply_ack_does_not_regress() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();

    for seq in 1..=10u64 {
        store.push(&sid, seq, Bytes::from_static(b"f"));
    }

    // First ACK advances to 7
    store.apply_ack(&sid, 7);
    assert_eq!(store.buffer_len(&sid), 3);

    // Out-of-order ACK with lower seq should be ignored (no regression)
    let purged = store.apply_ack(&sid, 3);
    assert_eq!(purged, 0, "older ACK must not regress");
    assert_eq!(store.buffer_len(&sid), 3);
}

#[test]
fn test_apply_ack_on_nonexistent_session() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let fake_sid = [0xFFu8; 16];
    let purged = store.apply_ack(&fake_sid, 100);
    assert_eq!(purged, 0, "ACK on missing session returns 0");
}

// ─── Integration: ACK + resume ──────────────────────────────────────────────

#[test]
fn test_resume_after_partial_ack() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();

    // Produce 10 frames
    for seq in 1..=10u64 {
        let data = format!("frame_{seq}").into_bytes();
        store.push(&sid, seq, Bytes::from(data));
    }

    // Client ACKs up to seq 4
    store.apply_ack(&sid, 4);

    // Client reconnects with last_recv_seq=4 — should get frames 5..=10
    let replayed = expect_ok(store.resume(&sid, 4));
    assert_eq!(replayed.len(), 6);
}

#[test]
fn test_resume_requests_evicted_frames_returns_partial() {
    // After Phase 3 + H1 fix: evicted-via-ACK frames now trigger a Gap.
    // This used to silently return an empty Vec.
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();

    // Push 10 frames then ACK them all (purges replay buffer entirely)
    for seq in 1..=10u64 {
        store.push(&sid, seq, Bytes::from_static(b"f"));
    }
    store.apply_ack(&sid, 10);
    assert_eq!(store.buffer_len(&sid), 0);

    // Client asks for seq > 2, but frames 3..=10 were ACKed and purged.
    // gap boundary = max(evicted=0, last_acked=10) = 10, so asking for 2
    // yields Gap(3..=10) — client must resync via GetOpenOrders snapshot.
    match store.resume(&sid, 2) {
        ReplayResult::Gap { missing_from, missing_to } => {
            assert_eq!(missing_from, 3);
            assert_eq!(missing_to, 10);
        }
        other => panic!("expected Gap, got {other:?}"),
    }
}

// ─── Sharded isolation ──────────────────────────────────────────────────────

#[test]
fn test_many_sessions_distributed_across_shards() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let mut sessions = Vec::new();
    for _ in 0..1000 {
        sessions.push(store.create());
    }
    assert_eq!(store.len(), 1000);

    // All should be retrievable
    for sid in &sessions {
        let replayed = expect_ok(store.resume(sid, 0));
        assert_eq!(replayed.len(), 0);
    }
}

#[test]
fn test_concurrent_dedup_inserts_different_sessions() {
    use std::sync::Arc;
    use std::thread;

    let store = Arc::new(SessionStore::new(1024, Duration::from_secs(60)));
    let mut handles = Vec::new();

    // Spawn 8 threads, each creates a session and does 100 dedup inserts
    for _ in 0..8 {
        let store = Arc::clone(&store);
        handles.push(thread::spawn(move || {
            let sid = store.create();
            for i in 0..100u64 {
                store.dedup_insert(&sid, i, Bytes::from_static(b"response"));
            }
            assert_eq!(store.dedup_len(&sid), 100);
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(store.len(), 8);
}

// ─── TTL expiration ─────────────────────────────────────────────────────────

#[test]
fn test_session_expires_after_ttl() {
    // Very short TTL for quick test
    let store = SessionStore::new(1024, Duration::from_millis(50));
    let sid = store.create();

    // Sleep past TTL
    std::thread::sleep(Duration::from_millis(100));

    // resume() should detect expiration and return SessionNotFound
    let result = store.resume(&sid, 0);
    assert_eq!(
        result,
        ReplayResult::SessionNotFound,
        "expired session should return SessionNotFound"
    );
}

#[test]
fn test_activity_resets_ttl() {
    let store = SessionStore::new(1024, Duration::from_millis(100));
    let sid = store.create();

    // Sleep half the TTL
    std::thread::sleep(Duration::from_millis(50));

    // Push a frame (updates last_access)
    store.push(&sid, 1, Bytes::from_static(b"f"));

    // Sleep another 50ms — total 100ms but activity reset should keep it alive
    std::thread::sleep(Duration::from_millis(75));

    // Still valid
    let result = store.resume(&sid, 0);
    assert!(
        matches!(result, ReplayResult::Ok(_)),
        "activity should extend TTL, got {result:?}"
    );
}

#[test]
fn test_gc_removes_expired_sessions() {
    let store = SessionStore::new(1024, Duration::from_millis(30));
    for _ in 0..10 {
        store.create();
    }
    assert_eq!(store.len(), 10);

    std::thread::sleep(Duration::from_millis(50));
    store.gc();
    assert_eq!(store.len(), 0, "GC should clean up expired sessions");
}

// ─── Metrics accessors ──────────────────────────────────────────────────────

#[test]
fn test_buffer_len_on_nonexistent_session() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let fake = [0u8; 16];
    assert_eq!(store.buffer_len(&fake), 0);
}

#[test]
fn test_dedup_len_on_nonexistent_session() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let fake = [0u8; 16];
    assert_eq!(store.dedup_len(&fake), 0);
}

#[test]
fn test_store_is_empty_initially() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    assert!(store.is_empty());
    assert_eq!(store.len(), 0);
}
