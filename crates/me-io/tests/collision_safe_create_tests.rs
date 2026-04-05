/// RFC-0001 Task C1: Collision-safe SessionStore::create() tests.
///
/// In production, SessionId::random() collides with probability ~2^-128 —
/// astronomically unlikely but not impossible over long server lifetime.
/// The bug: `map.insert(id, entry)` silently overwrites on collision,
/// destroying an in-flight session's state.
///
/// Fix: `create_with_id_source()` takes a closure for id generation so
/// tests can force collisions. The public `create()` delegates to it.
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use me_io::session_store::{SessionId, SessionStore};

#[test]
fn create_retries_on_collision_with_different_id() {
    // Rigged id source: first call returns [0x42; 16], second call
    // returns [0x42; 16] (collision!), third returns [0x43; 16].
    // Store MUST detect the collision and retry, yielding [0x43; 16].
    let store = SessionStore::new(1024, Duration::from_secs(60));

    // Pre-populate [0x42; 16] by calling create_with_id_source.
    let first = store.create_with_id_source(|| SessionId::from_bytes([0x42u8; 16]));
    assert_eq!(first, SessionId::from_bytes([0x42u8; 16]));

    // Now call again with a source that first returns the SAME id (collision),
    // then a different one. Store must retry.
    let counter = Arc::new(AtomicU8::new(0));
    let counter_clone = Arc::clone(&counter);
    let second = store.create_with_id_source(move || {
        let n = counter_clone.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // First attempt: collides with `first`.
            SessionId::from_bytes([0x42u8; 16])
        } else {
            // Retry: fresh id.
            SessionId::from_bytes([0x43u8; 16])
        }
    });

    assert_eq!(second, SessionId::from_bytes([0x43u8; 16]));
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "id source called twice: 1 collision + 1 retry"
    );
    assert_eq!(store.len(), 2, "two sessions created without clobbering");
}

#[test]
fn create_does_not_overwrite_existing_session_on_collision() {
    // Assert the collision-safety property: pre-existing session's state
    // must NOT be replaced when a new create() collides with its id.
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let fixed_id = SessionId::from_bytes([0x7Fu8; 16]);

    // Session A: put a dedup entry so we can verify it survives.
    let a = store.create_with_id_source(|| fixed_id);
    assert_eq!(a, fixed_id);
    store.dedup_insert(&a, 42, bytes::Bytes::from_static(b"A's response"));
    assert!(store.dedup_lookup(&a, 42).is_some());

    // Session B tries to collide with A's id, then retries with a fresh one.
    let counter = Arc::new(AtomicU8::new(0));
    let counter_clone = Arc::clone(&counter);
    let b = store.create_with_id_source(move || {
        let n = counter_clone.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            fixed_id // collide with A
        } else {
            SessionId::from_bytes([0x80u8; 16])
        }
    });

    assert_ne!(b, a, "new id must differ from existing");

    // A's dedup entry MUST still be there — NOT clobbered by B.
    let a_entry = store.dedup_lookup(&a, 42);
    assert_eq!(
        a_entry.as_deref(),
        Some(b"A's response".as_ref()),
        "A's state must survive collision"
    );
}

#[test]
fn create_public_api_still_works_with_random_ids() {
    // Smoke-test: the public create() — which uses SessionId::random() —
    // still works and produces distinct sessions.
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let a = store.create();
    let b = store.create();
    assert_ne!(a, b);
    assert!(!a.is_none());
    assert!(!b.is_none());
    assert_eq!(store.len(), 2);
}
