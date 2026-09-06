use bytes::BytesMut;
use me_core::MatchEvent;
use me_io::{
    codec,
    session_store::{ReplayResult, SessionStore},
};
use std::time::Duration;

fn frame(seq: u64) -> bytes::Bytes {
    let mut bytes = BytesMut::new();
    codec::encode_events(
        &[MatchEvent::OrderAccepted {
            order_id: seq,
            timestamp: 1,
        }],
        seq,
        seq,
        &mut bytes,
    );
    bytes.freeze()
}

#[test]
fn resume_cursor_does_not_skip_a_concurrent_maker_delivery() {
    let store = SessionStore::new(32, Duration::from_secs(60));
    let id = store.create();
    store.push(&id, 1, frame(1));
    let (replay, next) = store.delivery_snapshot(&id, 0).unwrap();
    store.push(&id, 2, frame(2));
    assert_eq!(replay, ReplayResult::Ok(vec![frame(1)]));
    assert_eq!(
        next, 2,
        "cursor must cover only the replay selected before the concurrent delivery"
    );
}

#[test]
fn resume_rejects_future_cursor_without_discarding_replay() {
    let store = SessionStore::new(32, Duration::from_secs(60));
    let id = store.create();
    store.push(&id, 1, frame(1));
    assert!(store.delivery_snapshot(&id, 2).is_err());
    assert_eq!(
        store.delivery_snapshot(&id, 0).unwrap(),
        (ReplayResult::Ok(vec![frame(1)]), 2)
    );
}

#[test]
fn claimed_idle_session_is_not_collected() {
    let store = SessionStore::new(32, Duration::ZERO);
    let id = store.create();
    let lease = store.claim(id).unwrap();
    store.gc();
    assert_eq!(store.len(), 1);
    drop(lease);
    store.gc();
    assert_eq!(store.len(), 0);
    assert!(store.claim(id).is_err());
}

#[test]
fn active_lease_survives_idle_ttl_in_both_replay_paths() {
    let store = SessionStore::new(32, Duration::from_millis(1));
    let id = store.create();
    let lease = store.claim(id).unwrap();
    store.push(&id, 1, frame(1));
    std::thread::sleep(Duration::from_millis(5));
    store.gc();
    assert_eq!(store.resume(&id, 0), ReplayResult::Ok(vec![frame(1)]));
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(
        store.delivery_snapshot(&id, 0).unwrap(),
        (ReplayResult::Ok(vec![frame(1)]), 2)
    );
    drop(lease);
    std::thread::sleep(Duration::from_millis(5));
    store.gc();
    assert_eq!(store.len(), 0);
}

#[test]
fn exhausted_client_sequence_returns_protocol_error_without_panicking() {
    let mut conn = codec::ConnectionCodecState::new();
    conn.state = me_io::protocol::ConnectionState::Active;
    conn.last_client_seq = u64::MAX;
    let mut bytes = vec![0, 0, 0, 0, 2, 4];
    bytes.extend_from_slice(&u64::MAX.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    assert!(matches!(
        codec::decode_frame(&bytes, &mut conn, &me_clock::SystemClock),
        Err(codec::DecodeError::InvalidPayload(_))
    ));
}
