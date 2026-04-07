/// C1: Exactly-once retry behavior integration tests.
///
/// These tests verify the dedup contract end-to-end:
/// - A request with `request_id=X` processed once and cached
/// - The SAME `request_id=X` arriving again returns the cached response
///   WITHOUT invoking the engine a second time
/// - Different `request_id`s on the same session are processed independently
/// - Different sessions with same `request_id` are isolated
///
/// This is the core contract protocol v2 promises: mid-flight retries are safe.
use std::time::Duration;

use bytes::Bytes;
use me_io::session_store::{SessionId, SessionStore};

/// Simulate a "process + cache" step: lookup request_id; if hit, return cached;
/// otherwise invoke a fresh "handler" closure, cache the response, return it.
/// This mirrors the hot path in tcp_server.rs / uds_server.rs.
fn process_or_replay<F>(
    store: &SessionStore,
    sid: &SessionId,
    request_id: u64,
    handler: F,
) -> Bytes
where
    F: FnOnce() -> Bytes,
{
    if let Some(cached) = store.dedup_lookup(sid, request_id) {
        return cached;
    }
    let response = handler();
    store.dedup_insert(sid, request_id, response.clone());
    response
}

#[test]
fn test_retry_same_request_id_returns_cached_response_without_reprocessing() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();

    let mut handler_call_count = 0u32;
    let mut call_once = |bytes: &'static [u8]| -> Bytes {
        handler_call_count += 1;
        Bytes::from_static(bytes)
    };

    // First call: handler runs.
    let r1 = process_or_replay(&store, &sid, 42, || call_once(b"OrderAccepted#1"));
    assert_eq!(&r1[..], b"OrderAccepted#1");
    assert_eq!(handler_call_count, 1);

    // Retry with same request_id: cached response returned, handler NOT called.
    let r2 = process_or_replay(&store, &sid, 42, || {
        panic!("handler MUST NOT be called on duplicate request_id");
    });
    assert_eq!(&r2[..], b"OrderAccepted#1");
    assert_eq!(handler_call_count, 1, "handler called exactly once");
}

#[test]
fn test_different_request_ids_on_same_session_processed_independently() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();

    let mut calls = Vec::new();
    let mut record = |tag: &'static str| -> Bytes {
        calls.push(tag);
        Bytes::from_static(tag.as_bytes())
    };

    let r1 = process_or_replay(&store, &sid, 1, || record("req_1"));
    let r2 = process_or_replay(&store, &sid, 2, || record("req_2"));
    let r3 = process_or_replay(&store, &sid, 3, || record("req_3"));

    assert_eq!(&r1[..], b"req_1");
    assert_eq!(&r2[..], b"req_2");
    assert_eq!(&r3[..], b"req_3");
    assert_eq!(calls, vec!["req_1", "req_2", "req_3"]);

    // Retries of each: all cached.
    assert_eq!(&process_or_replay(&store, &sid, 1, || panic!("cached"))[..], b"req_1");
    assert_eq!(&process_or_replay(&store, &sid, 2, || panic!("cached"))[..], b"req_2");
    assert_eq!(&process_or_replay(&store, &sid, 3, || panic!("cached"))[..], b"req_3");
}

#[test]
fn test_same_request_id_different_sessions_are_isolated() {
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid_a = store.create();
    let sid_b = store.create();

    // Both sessions use request_id=1 — they must NOT collide.
    let ra = process_or_replay(&store, &sid_a, 1, || Bytes::from_static(b"from_A"));
    let rb = process_or_replay(&store, &sid_b, 1, || Bytes::from_static(b"from_B"));

    assert_eq!(&ra[..], b"from_A");
    assert_eq!(&rb[..], b"from_B");

    // Retry on A gets A's response, retry on B gets B's.
    let ra2 = process_or_replay(&store, &sid_a, 1, || panic!("A cached"));
    let rb2 = process_or_replay(&store, &sid_b, 1, || panic!("B cached"));
    assert_eq!(&ra2[..], b"from_A");
    assert_eq!(&rb2[..], b"from_B");
}

#[test]
fn test_retry_after_many_other_requests_still_returns_cached() {
    // dedup table has FIFO eviction — if dedup_max is large enough,
    // an early request's response survives many subsequent requests.
    let store = SessionStore::new_with_dedup(1024, 100, Duration::from_secs(60));
    let sid = store.create();

    // Cache request_id=1.
    process_or_replay(&store, &sid, 1, || Bytes::from_static(b"first"));

    // Process 50 other requests (well under 100-entry dedup limit).
    for i in 2..=51u64 {
        process_or_replay(&store, &sid, i, || Bytes::from_static(b"other"));
    }

    // Original is still cached.
    let replay = process_or_replay(&store, &sid, 1, || panic!("should be cached"));
    assert_eq!(&replay[..], b"first");
}

#[test]
fn test_retry_fails_only_if_dedup_evicted_by_capacity_pressure() {
    // With dedup_max=3, inserting a 4th request evicts request_id=1.
    // A retry of request_id=1 will then re-invoke the handler.
    let store = SessionStore::new_with_dedup(1024, 3, Duration::from_secs(60));
    let sid = store.create();

    let mut second_call_for_1 = false;
    process_or_replay(&store, &sid, 1, || Bytes::from_static(b"first"));
    process_or_replay(&store, &sid, 2, || Bytes::from_static(b"two"));
    process_or_replay(&store, &sid, 3, || Bytes::from_static(b"three"));
    // This 4th insert should evict request_id=1 (FIFO).
    process_or_replay(&store, &sid, 4, || Bytes::from_static(b"four"));

    // Retry request_id=1 — evicted, so handler IS called again.
    let replay = process_or_replay(&store, &sid, 1, || {
        second_call_for_1 = true;
        Bytes::from_static(b"first_AGAIN")
    });

    assert!(
        second_call_for_1,
        "request_id=1 was evicted, handler must be re-invoked"
    );
    assert_eq!(&replay[..], b"first_AGAIN");
}

#[test]
fn test_dedup_insert_is_idempotent_for_same_request_id() {
    // Inserting the SAME request_id twice must not consume two dedup slots.
    let store = SessionStore::new_with_dedup(1024, 2, Duration::from_secs(60));
    let sid = store.create();

    store.dedup_insert(&sid, 1, Bytes::from_static(b"v1"));
    store.dedup_insert(&sid, 1, Bytes::from_static(b"v1_update")); // same key
    store.dedup_insert(&sid, 2, Bytes::from_static(b"v2"));

    // dedup_max=2: if re-insert consumed a slot, entry 1 would be evicted.
    // Correct behavior: both 1 and 2 still present.
    assert!(store.dedup_lookup(&sid, 1).is_some(), "key 1 must remain");
    assert!(store.dedup_lookup(&sid, 2).is_some(), "key 2 must remain");
}
