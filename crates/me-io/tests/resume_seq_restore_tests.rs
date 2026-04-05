/// Regression tests for the server_frame_seq-on-resume bug.
///
/// Bug (pre-fix): on `ResumeSession::Ok`, the server handler would set
/// `conn.session_id` but leave `conn.server_frame_seq` at its initial value
/// of 1 (from `ConnectionCodecState::new()`). The next `push_frames_with_seq_rewrite`
/// would then assign seq=1 to a NEW frame, COLLIDING with the original seq=1
/// already stored in the replay buffer. This broke ACK semantics, gap
/// detection, and on-wire seq monotonicity.
///
/// Fix: `SessionStore::next_outbound_seq(id)` returns the correct seq to use
/// for the NEXT outbound frame on this session. Handlers MUST call it after
/// `ResumeSession::Ok` / `::Gap` and seed `conn.server_frame_seq`.
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use me_io::codec;
use me_io::session_store::{ReplayResult, SessionStore};

/// Extract seq (header offset 6..14 LE) from a wire frame.
fn seq_of(frame: &[u8]) -> u64 {
    u64::from_le_bytes(frame[6..14].try_into().unwrap())
}

#[test]
fn next_outbound_seq_starts_at_1_on_fresh_session() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();
    assert_eq!(
        store.next_outbound_seq(&sid),
        1,
        "fresh session: next seq must be 1 (seq=0 is reserved sentinel)"
    );
}

#[test]
fn next_outbound_seq_returns_1_for_missing_session() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    // sid was never created
    let fake_sid = [0xFFu8; 16];
    assert_eq!(
        store.next_outbound_seq(&fake_sid),
        1,
        "missing session: treat as fresh (seq=1)"
    );
}

#[test]
fn next_outbound_seq_advances_by_pushed_count() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();

    // Push 5 frames with seqs 1..=5
    let mut buf = BytesMut::new();
    for _ in 0..5 {
        codec::encode_pong(0, 0, &mut buf); // placeholder seq=0
    }
    let mut block: Vec<u8> = buf.to_vec();
    let count = store.push_frames_with_seq_rewrite(&sid, 1, &mut block);
    assert_eq!(count, 5);

    assert_eq!(
        store.next_outbound_seq(&sid),
        6,
        "after pushing 5 frames (seqs 1..=5), next is 6"
    );
}

#[test]
fn next_outbound_seq_survives_ack_purge() {
    // Frames are ACKed and purged from the buffer, but next_outbound_seq
    // MUST still track the highest seq ever pushed. Otherwise resume
    // would restart seq numbering on top of ACKed-but-known seqs.
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();

    // Push 10 frames with seqs 1..=10
    for seq in 1..=10u64 {
        store.push(&sid, seq, Bytes::from_static(b"x"));
    }
    assert_eq!(store.buffer_len(&sid), 10);

    // ACK all 10 → buffer drained
    let purged = store.apply_ack(&sid, 10);
    assert_eq!(purged, 10);
    assert_eq!(store.buffer_len(&sid), 0);

    // next_outbound_seq must STILL be 11, not 1.
    assert_eq!(
        store.next_outbound_seq(&sid),
        11,
        "after ACKing all 10 frames, next seq must be 11 (not reset to 1)"
    );
}

#[test]
fn next_outbound_seq_survives_hard_limit_eviction() {
    // Frames 1..=3 get hard-limit evicted, frames 4..=7 remain in buffer.
    // next_outbound_seq should reflect the true highest pushed = 7.
    let store = SessionStore::new_adaptive(4, 4, 100, Duration::from_secs(60));
    let sid = store.create();

    for seq in 1..=7u64 {
        store.push(&sid, seq, Bytes::from_static(b"f"));
    }
    assert_eq!(store.buffer_len(&sid), 4); // buffer holds 4..=7

    assert_eq!(
        store.next_outbound_seq(&sid),
        8,
        "after pushing 7 frames (some evicted), next is 8"
    );
}

#[test]
fn reconnect_workflow_preserves_seq_monotonicity() {
    // End-to-end simulation of the ResumeSession flow:
    //   1. Session created, frames 1..=5 pushed via connection A
    //   2. Connection A drops; connection B reconnects with ResumeSession
    //   3. Handler MUST call next_outbound_seq to seed B's server_frame_seq
    //   4. B pushes a new frame — it gets seq=6, NOT seq=1
    //   5. Replay returns all 6 frames in strict monotonic order 1..=6
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();

    // Conn A: push 5 frames.
    let mut buf_a = BytesMut::new();
    for _ in 0..5 {
        codec::encode_pong(0, 0, &mut buf_a);
    }
    let mut block_a: Vec<u8> = buf_a.to_vec();
    store.push_frames_with_seq_rewrite(&sid, 1, &mut block_a);

    // "Conn A drops, conn B arrives and sends ResumeSession":
    //   - fresh ConnectionCodecState: server_frame_seq = 1 (initial)
    // Handler handles ResumeSession::Ok:
    let result = store.resume(&sid, 0);
    assert!(matches!(result, ReplayResult::Ok(_)));
    // ── THE FIX: handler must call next_outbound_seq ──
    let b_server_frame_seq = store.next_outbound_seq(&sid);
    assert_eq!(b_server_frame_seq, 6, "after 5 frames, next is 6");

    // Conn B pushes 1 new frame.
    let mut buf_b = BytesMut::new();
    codec::encode_pong(0, 0, &mut buf_b);
    let mut block_b: Vec<u8> = buf_b.to_vec();
    store.push_frames_with_seq_rewrite(&sid, b_server_frame_seq, &mut block_b);

    // Verify: full replay yields frames 1..=6 in strict monotonic order.
    let all_frames = match store.resume(&sid, 0) {
        ReplayResult::Ok(f) => f,
        other => panic!("expected Ok, got {other:?}"),
    };
    assert_eq!(all_frames.len(), 6, "5 original + 1 new");

    let seqs: Vec<u64> = all_frames.iter().map(|f| seq_of(f)).collect();
    assert_eq!(
        seqs,
        vec![1, 2, 3, 4, 5, 6],
        "seqs must be strictly monotonic — NO duplicates, NO reset to 1"
    );
}

#[test]
fn dedup_lookup_refreshes_last_access() {
    // A client retrying with the same request_id must keep its session alive.
    // Before the fix: dedup_lookup took an immutable lock, so a retry-heavy
    // client (silent otherwise) could TTL-expire mid-retry.
    use std::thread::sleep;
    let ttl = Duration::from_millis(100);
    let store = SessionStore::new(1024, ttl);
    let sid = store.create();
    store.dedup_insert(&sid, 42, Bytes::from_static(b"cached_response"));

    // Sleep 2/3 of TTL → session still alive.
    sleep(Duration::from_millis(65));
    assert!(
        store.dedup_lookup(&sid, 42).is_some(),
        "session must still be alive before TTL"
    );

    // Sleep another 2/3 of TTL → would be 130ms total if last_access wasn't
    // refreshed. With the fix, the lookup reset the clock → still alive.
    sleep(Duration::from_millis(65));
    assert!(
        store.dedup_lookup(&sid, 42).is_some(),
        "dedup_lookup must refresh last_access; session should still be alive"
    );

    // Now go idle past the full TTL from last lookup.
    sleep(Duration::from_millis(150));
    store.gc();
    // Post-GC: session expired (no activity for >TTL).
    assert_eq!(store.len(), 0, "session GC'd after genuine idle");
}

#[test]
fn resume_after_gap_also_restores_seq() {
    // Even in the Gap branch (client lost state, must resync), the handler
    // still continues pushing on the SAME session — so server_frame_seq
    // MUST still be restored to avoid collisions after snapshot.
    let store = SessionStore::new_adaptive(4, 4, 100, Duration::from_secs(60));
    let sid = store.create();

    // Fill + overflow: seqs 1..=7, eviction to 4..=7.
    for seq in 1..=7u64 {
        store.push(&sid, seq, Bytes::from_static(b"x"));
    }

    // Client reconnects claiming last_recv=2 — the gap (3..=3) is detected.
    let result = store.resume(&sid, 2);
    assert!(matches!(result, ReplayResult::Gap { .. }));

    // Handler must still call next_outbound_seq for post-gap pushes.
    assert_eq!(
        store.next_outbound_seq(&sid),
        8,
        "even after Gap, next seq continues from 8"
    );
}
