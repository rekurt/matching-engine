use me_core::{MatchEvent, SessionId};
use me_engine::router::DurableRequest;
use me_io::session_store::{ReplayResult, SessionStore};
use std::time::Duration;

fn path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "me-session-{}-{}.json",
        std::process::id(),
        SessionId::random().log_prefix()
    ))
}
fn record(session_id: SessionId, request_id: u64) -> DurableRequest {
    DurableRequest {
        session_id,
        request_id,
        item_index: 0,
        events: vec![MatchEvent::OrderAccepted {
            order_id: 42,
            timestamp: 1,
        }],
        owners: vec![(42, session_id)],
        expiration_count: 0,
        expiration_owners: Vec::new(),
    }
}
#[test]
fn restart_recovers_identity_dedup_and_replay_without_resequencing() {
    let path = path();
    let store = SessionStore::open(&path, 32, Duration::ZERO).unwrap();
    let id = store.create();
    store.checkpoint().unwrap();
    let bytes = store.commit_request(record(id, 1)).unwrap();
    drop(store);
    let store = SessionStore::open(&path, 32, Duration::ZERO).unwrap();
    store.gc();
    assert_eq!(store.dedup_lookup(&id, 1), Some(bytes.clone()));
    assert_eq!(store.resume(&id, 0), ReplayResult::Ok(vec![bytes.clone()]));
    assert_eq!(store.commit_request(record(id, 1)).unwrap(), bytes);
    assert_eq!(store.next_outbound_seq(&id), 2);
    std::fs::remove_file(path).unwrap();
}
#[test]
fn engine_commit_gap_reconciles_exactly_once() {
    let path = path();
    let store = SessionStore::open(&path, 32, Duration::from_secs(60)).unwrap();
    let id = store.create();
    store.admit_request(id, 1, vec![1]).unwrap();
    store.checkpoint().unwrap();
    drop(store);
    let store = SessionStore::open(&path, 32, Duration::from_secs(60)).unwrap();
    store.reconcile(vec![record(id, 1)]).unwrap();
    store.reconcile(vec![record(id, 1)]).unwrap();
    assert_eq!(store.buffer_len(&id), 1);
    std::fs::remove_file(path).unwrap();
}
#[test]
fn lease_excludes_concurrent_resume_and_releases_on_drop() {
    let store = SessionStore::new(32, Duration::from_secs(60));
    let id = store.create();
    let lease = store.claim(id).unwrap();
    assert!(store.claim(id).is_err());
    drop(lease);
    assert!(store.claim(id).is_ok());
}
#[test]
fn missing_persistence_directory_fails_before_session_admission() {
    let path = path().join("missing").join("session.wal");
    assert!(SessionStore::open(&path, 32, Duration::from_secs(60)).is_err());
}

#[test]
fn empty_session_journal_cannot_reconstruct_acknowledged_identity() {
    let path = path();
    let store = SessionStore::open(&path, 32, Duration::from_secs(60)).unwrap();
    assert!(store
        .reconcile(vec![record(SessionId::random(), 1)])
        .is_err());
    let id = store.create();
    assert!(store.reconcile(vec![record(id, 1)]).is_err());
    std::fs::remove_file(path).unwrap();
}
