/// C4: Concurrent same-session stress tests.
///
/// These tests verify that SessionStore survives hot concurrent access from
/// multiple threads on the SAME session — the realistic case where:
/// - a TCP connection handler thread pushes outbound frames
/// - a background ACK arrives and triggers purge
/// - a dedup lookup runs for an in-flight retry
///
/// Without proper locking, these would deadlock, panic, or corrupt state.
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use bytes::Bytes;
use me_io::session_store::SessionStore;

#[test]
fn test_concurrent_pushes_on_same_session_are_serialized() {
    let store = Arc::new(SessionStore::new(100_000, Duration::from_secs(60)));
    let sid = store.create();

    let num_threads = 8;
    let pushes_per_thread = 500u64;
    let mut handles = Vec::new();

    for t in 0..num_threads {
        let store = Arc::clone(&store);
        let sid_copy = sid;
        handles.push(thread::spawn(move || {
            for i in 0..pushes_per_thread {
                // Ensure globally unique seq: t * pushes_per_thread + i.
                let seq = t * pushes_per_thread + i;
                store.push(&sid_copy, seq, Bytes::from_static(b"x"));
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    // All frames accounted for (no panics, no lost writes).
    let expected = (num_threads * pushes_per_thread) as usize;
    assert_eq!(
        store.buffer_len(&sid),
        expected,
        "all concurrent pushes must land in buffer"
    );
}

#[test]
fn test_concurrent_dedup_inserts_same_session_no_corruption() {
    let store = Arc::new(SessionStore::new_with_dedup(
        1024,
        10_000,
        Duration::from_secs(60),
    ));
    let sid = store.create();

    let num_threads = 8;
    let inserts_per_thread = 500u64;
    let mut handles = Vec::new();

    for t in 0..num_threads {
        let store = Arc::clone(&store);
        let sid_copy = sid;
        handles.push(thread::spawn(move || {
            for i in 0..inserts_per_thread {
                // Unique request_id: t * inserts_per_thread + i.
                let rid = t * inserts_per_thread + i;
                let payload = format!("thread_{t}_req_{i}");
                store.dedup_insert(&sid_copy, rid, Bytes::from(payload));
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    // All inserts must be findable (no lost entries — dedup_max=10000 > total=4000).
    let expected = (num_threads * inserts_per_thread) as usize;
    assert_eq!(store.dedup_len(&sid), expected);

    // Spot-check: specific thread's specific request is still present.
    let check = store.dedup_lookup(&sid, 3 * 500 + 17).unwrap();
    assert_eq!(&check[..], b"thread_3_req_17");
}

#[test]
fn test_concurrent_push_ack_lookup_no_deadlock() {
    // Simulate the realistic server hot path: writer pushes, ACK purges,
    // retry does dedup_lookup — all concurrent on the same session.
    let store = Arc::new(SessionStore::new_with_dedup(
        10_000,
        1024,
        Duration::from_secs(60),
    ));
    let sid = store.create();

    let store_pusher = Arc::clone(&store);
    let sid_pusher = sid;
    let pusher = thread::spawn(move || {
        for seq in 1..=1000u64 {
            store_pusher.push(&sid_pusher, seq, Bytes::from_static(b"frame"));
            // Also cache a dedup response for the same "request".
            store_pusher.dedup_insert(&sid_pusher, seq, Bytes::from_static(b"resp"));
        }
    });

    let store_acker = Arc::clone(&store);
    let sid_acker = sid;
    let acker = thread::spawn(move || {
        for ack_to in (100..=1000).step_by(100) {
            store_acker.apply_ack(&sid_acker, ack_to);
            thread::sleep(Duration::from_micros(50));
        }
    });

    let store_lookup = Arc::clone(&store);
    let sid_lookup = sid;
    let lookup = thread::spawn(move || {
        let mut hits = 0;
        for rid in 1..=1000u64 {
            if store_lookup.dedup_lookup(&sid_lookup, rid).is_some() {
                hits += 1;
            }
        }
        hits
    });

    pusher.join().unwrap();
    acker.join().unwrap();
    let _hit_count = lookup.join().unwrap();

    // Final state consistency: dedup table has some entries (at least some
    // survived eviction); buffer has been partially purged by ACKs.
    // We don't assert exact numbers (racy), just that state is coherent.
    let dedup_size = store.dedup_len(&sid);
    let buf_size = store.buffer_len(&sid);
    assert!(dedup_size <= 1024, "dedup respects max: {dedup_size}");
    assert!(buf_size <= 1000, "buffer consistent: {buf_size}");
}

#[test]
fn test_concurrent_gc_and_ops_no_deadlock() {
    // GC runs across all shards; meanwhile other threads are doing per-session
    // ops. Shouldn't deadlock (gc locks shards one at a time).
    let store = Arc::new(SessionStore::new(1024, Duration::from_millis(50)));

    // Create many sessions that will expire.
    let mut sids = Vec::new();
    for _ in 0..100 {
        sids.push(store.create());
    }

    let store_worker = Arc::clone(&store);
    let sids_worker = sids.clone();
    let worker = thread::spawn(move || {
        for _ in 0..50 {
            for sid in &sids_worker {
                store_worker.push(sid, 1, Bytes::from_static(b"x"));
            }
        }
    });

    let store_gc = Arc::clone(&store);
    let gc = thread::spawn(move || {
        for _ in 0..20 {
            store_gc.gc();
            thread::sleep(Duration::from_micros(200));
        }
    });

    worker.join().unwrap();
    gc.join().unwrap();

    // Sanity: store is in a consistent state.
    let len = store.len();
    assert!(len <= 100, "session count sane: {len}");
}

#[test]
fn test_push_frames_with_seq_rewrite_thread_safe() {
    // The seq-rewrite path is the hot-path outbound frame writer. Multiple
    // threads calling it on the SAME session must all assign distinct seqs
    // AND push each frame into the replay buffer.
    // FRAME_HEADER_SIZE = 22 bytes, zero payload.
    let store = Arc::new(SessionStore::new(100_000, Duration::from_secs(60)));
    let sid = store.create();

    let num_threads = 4;
    let frames_per_thread = 200u64;

    // Each thread builds its own block of N identical headers with seq=0 placeholder,
    // then calls push_frames_with_seq_rewrite starting at a thread-unique base seq.
    let mut handles = Vec::new();
    for t in 0..num_threads {
        let store = Arc::clone(&store);
        let sid_copy = sid;
        handles.push(thread::spawn(move || {
            let base_seq = t * frames_per_thread;
            let mut block = vec![0u8; 22 * frames_per_thread as usize];
            // payload_len=0 in header for each frame: bytes 0..4 = 0.
            // version=2 at offset 4, msg_type=0x10 (OrderAccepted) at offset 5.
            for i in 0..frames_per_thread as usize {
                let off = i * 22;
                block[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
                block[off + 4] = 2;
                block[off + 5] = 0x10;
            }
            let count = store.push_frames_with_seq_rewrite(&sid_copy, base_seq, &mut block);
            assert_eq!(count, frames_per_thread);
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let total_frames = (num_threads * frames_per_thread) as usize;
    assert_eq!(store.buffer_len(&sid), total_frames);
}
