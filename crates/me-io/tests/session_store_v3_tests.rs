/// Phase 3 tests: Gap detection, adaptive buffer, flow control.
///
/// These tests verify:
/// - resume() returns Gap when requested frames were evicted from buffer
/// - Adaptive buffer grows from buffer_min up to buffer_max
/// - Hard limit triggers slow_consumer flag + eviction
/// - apply_ack clears slow_consumer when buffer drops below soft limit
/// - check_flow_control_warning emits once per cycle for soft limit
/// - check_flow_control_warning emits repeatedly until ACK for hard limit
use std::time::Duration;

use bytes::Bytes;
use me_io::session_store::{ReplayResult, SessionId, SessionStore};

#[test]
fn test_resume_ok_when_no_eviction() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();

    for seq in 1..=10u64 {
        store.push(&sid, seq, Bytes::from_static(b"frame"));
    }

    // Client reports last_recv=5 → wants frames 6..=10, all in buffer
    let result = store.resume(&sid, 5);
    match result {
        ReplayResult::Ok(frames) => assert_eq!(frames.len(), 5),
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn test_resume_gap_when_frames_evicted_by_hard_limit() {
    // buffer_max=4 forces eviction after 5th push
    let store = SessionStore::new_adaptive(4, 4, 100, Duration::from_secs(60));
    let sid = store.create();

    // Push 7 frames; first 3 (seq 1, 2, 3) should be evicted (buffer holds 4)
    for seq in 1..=7u64 {
        store.push(&sid, seq, Bytes::from_static(b"frame"));
    }
    assert_eq!(store.buffer_len(&sid), 4);

    // Client says "I got up to seq 2" → needs frames 3..=7
    // But frame 3 is evicted (highest_evicted = 3)
    let result = store.resume(&sid, 2);
    match result {
        ReplayResult::Gap {
            missing_from,
            missing_to,
        } => {
            assert_eq!(missing_from, 3, "client needs from seq 3");
            assert_eq!(missing_to, 3, "seq 3 is highest that was evicted");
        }
        other => panic!("expected Gap, got {other:?}"),
    }
}

#[test]
fn test_resume_ok_when_client_caught_up_past_evictions() {
    let store = SessionStore::new_adaptive(4, 4, 100, Duration::from_secs(60));
    let sid = store.create();

    // Push 7 frames — frames 1..=3 evicted
    for seq in 1..=7u64 {
        store.push(&sid, seq, Bytes::from_static(b"f"));
    }

    // Client says "I got up to seq 4" → needs 5..=7, all in buffer
    let result = store.resume(&sid, 4);
    match result {
        ReplayResult::Ok(frames) => assert_eq!(frames.len(), 3),
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn test_resume_gap_boundary_exact_evicted_seq() {
    let store = SessionStore::new_adaptive(3, 3, 100, Duration::from_secs(60));
    let sid = store.create();

    // Push 5 frames — frames 1, 2 evicted (highest_evicted = 2)
    for seq in 1..=5u64 {
        store.push(&sid, seq, Bytes::from_static(b"f"));
    }
    assert_eq!(store.buffer_len(&sid), 3);

    // Client says last_recv=1 (which is < highest_evicted=2) → gap
    assert!(matches!(store.resume(&sid, 1), ReplayResult::Gap { .. }));
    // Client says last_recv=2 (equal to highest_evicted) → no gap, wants 3..=5
    assert!(matches!(store.resume(&sid, 2), ReplayResult::Ok(_)));
}

#[test]
fn test_resume_session_not_found() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let fake = SessionId::from_bytes([0xFFu8; 16]);
    assert_eq!(store.resume(&fake, 0), ReplayResult::SessionNotFound);
}

#[test]
fn test_buffer_grows_from_min_to_max() {
    let store = SessionStore::new_adaptive(2, 16, 100, Duration::from_secs(60));
    let sid = store.create();

    // Push enough frames to force growth
    for seq in 1..=10u64 {
        store.push(&sid, seq, Bytes::from_static(b"f"));
    }

    // With min=2, doublings: 2 -> 4 -> 8 -> 16 (max)
    // 10 frames should fit without eviction
    assert_eq!(store.buffer_len(&sid), 10);

    // No eviction → resume(0) returns all 10
    let result = store.resume(&sid, 0);
    match result {
        ReplayResult::Ok(frames) => assert_eq!(frames.len(), 10),
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn test_buffer_evicts_at_hard_limit() {
    let store = SessionStore::new_adaptive(2, 4, 100, Duration::from_secs(60));
    let sid = store.create();

    // Push 10 frames into max=4 buffer → 6 evictions
    for seq in 1..=10u64 {
        store.push(&sid, seq, Bytes::from_static(b"f"));
    }

    assert_eq!(store.buffer_len(&sid), 4);
    // Only frames 7, 8, 9, 10 remain
    let result = store.resume(&sid, 6);
    match result {
        ReplayResult::Ok(frames) => assert_eq!(frames.len(), 4),
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn test_slow_consumer_flag_set_on_eviction() {
    let store = SessionStore::new_adaptive(2, 3, 100, Duration::from_secs(60));
    let sid = store.create();

    // Before hard limit: not slow
    for seq in 1..=3u64 {
        store.push(&sid, seq, Bytes::from_static(b"f"));
    }
    let (_, is_hard) = store.buffer_pressure(&sid);
    assert!(!is_hard, "no eviction yet, should not be slow");

    // Push 4th frame → evicts 1 → slow_consumer = true
    store.push(&sid, 4, Bytes::from_static(b"f"));
    let (_, is_hard) = store.buffer_pressure(&sid);
    assert!(is_hard, "eviction triggered, should be slow");
}

#[test]
fn test_slow_consumer_cleared_on_ack() {
    let store = SessionStore::new_adaptive(2, 4, 100, Duration::from_secs(60));
    let sid = store.create();

    // Fill past max → eviction → slow_consumer
    for seq in 1..=6u64 {
        store.push(&sid, seq, Bytes::from_static(b"f"));
    }
    assert!(store.buffer_pressure(&sid).1, "should be slow");

    // Client ACKs everything → buffer drops below soft limit → cleared
    store.apply_ack(&sid, 10);
    assert!(!store.buffer_pressure(&sid).1, "should not be slow");
}

#[test]
fn test_flow_control_warning_soft_limit_once() {
    // max=10, soft limit triggers at 80% = 8 frames
    let store = SessionStore::new_adaptive(2, 10, 100, Duration::from_secs(60));
    let sid = store.create();

    // Push 7 frames — below soft limit (70%)
    for seq in 1..=7u64 {
        store.push(&sid, seq, Bytes::from_static(b"f"));
    }
    assert!(
        store.check_flow_control_warning(&sid).is_none(),
        "below soft limit, no warning"
    );

    // Push 1 more → 8 frames = 80% = soft limit
    store.push(&sid, 8, Bytes::from_static(b"f"));
    let warning = store.check_flow_control_warning(&sid);
    assert!(warning.is_some(), "soft limit should trigger warning");
    let (_pct, is_hard) = warning.unwrap();
    assert!(!is_hard, "should be soft warning");

    // Subsequent checks at same fill level → no repeated warning
    assert!(
        store.check_flow_control_warning(&sid).is_none(),
        "soft warning already sent, no repeat"
    );
}

#[test]
fn test_flow_control_warning_hard_limit_repeats() {
    let store = SessionStore::new_adaptive(2, 4, 100, Duration::from_secs(60));
    let sid = store.create();

    // Force eviction
    for seq in 1..=6u64 {
        store.push(&sid, seq, Bytes::from_static(b"f"));
    }

    // Hard limit triggered → warning repeats until ACK clears it
    let w1 = store.check_flow_control_warning(&sid);
    assert!(w1.is_some() && w1.unwrap().1, "hard warning expected");

    let w2 = store.check_flow_control_warning(&sid);
    assert!(
        w2.is_some() && w2.unwrap().1,
        "hard warning should repeat until ACK"
    );
}

#[test]
fn test_flow_control_warning_cycle_reset_on_ack() {
    let store = SessionStore::new_adaptive(2, 10, 100, Duration::from_secs(60));
    let sid = store.create();

    // Fill to soft limit
    for seq in 1..=8u64 {
        store.push(&sid, seq, Bytes::from_static(b"f"));
    }
    assert!(store.check_flow_control_warning(&sid).is_some());
    assert!(store.check_flow_control_warning(&sid).is_none()); // already sent

    // ACK drops buffer below soft limit
    store.apply_ack(&sid, 5); // removes 5 frames → 3 remain
    assert_eq!(store.buffer_len(&sid), 3);

    // Push more frames back up to soft limit → new cycle, warning again
    for seq in 9..=13u64 {
        store.push(&sid, seq, Bytes::from_static(b"f"));
    }
    // Now have 8 frames, at 80% again
    assert!(
        store.check_flow_control_warning(&sid).is_some(),
        "new cycle after ACK should allow new warning"
    );
}

#[test]
fn test_buffer_pressure_returns_percentage() {
    let store = SessionStore::new_adaptive(2, 10, 100, Duration::from_secs(60));
    let sid = store.create();

    for seq in 1..=5u64 {
        store.push(&sid, seq, Bytes::from_static(b"f"));
    }
    let (pct, _) = store.buffer_pressure(&sid);
    assert_eq!(pct, 50, "5/10 = 50%");

    store.push(&sid, 6, Bytes::from_static(b"f"));
    let (pct, _) = store.buffer_pressure(&sid);
    assert_eq!(pct, 60);
}

#[test]
fn test_full_resync_flow_after_overflow() {
    let store = SessionStore::new_adaptive(2, 4, 100, Duration::from_secs(60));
    let sid = store.create();

    // Server sends 10 frames while client is offline
    for seq in 1..=10u64 {
        store.push(&sid, seq, Bytes::from_static(b"frame"));
    }

    // Client reconnects saying "I last received seq=1"
    let result = store.resume(&sid, 1);
    match result {
        ReplayResult::Gap {
            missing_from,
            missing_to,
        } => {
            assert_eq!(missing_from, 2, "needs from 2");
            assert!(missing_to >= 6, "significant eviction happened");
        }
        other => panic!("expected Gap, got {other:?}"),
    }

    // Client would now issue GetOpenOrders (handled at server layer in Phase 4).
    // After resync, client acknowledges latest seq and resumes normal flow.
    store.apply_ack(&sid, 10);
    assert_eq!(store.buffer_len(&sid), 0);
    assert!(!store.buffer_pressure(&sid).1, "ACK clears slow consumer");
}
