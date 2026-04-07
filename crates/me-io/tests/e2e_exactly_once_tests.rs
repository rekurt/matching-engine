/// C3: End-to-end integration test — codec → session_store → router.
///
/// This exercises the FULL path that a client request takes through the
/// server, in-process and without any network I/O:
///
/// 1. Client builds a PlaceOrder wire frame (22-byte header + 37-byte payload)
/// 2. Server decode_frame -> Command
/// 3. Server dedup_lookup -> miss
/// 4. Server router.process(Command) -> Vec<MatchEvent>
/// 5. Server encode_events -> write_buf (frames with placeholder seq=request seq)
/// 6. Server push_frames_with_seq_rewrite -> assigns real server_seq values
/// 7. Server dedup_insert(request_id, write_buf)
///
/// Then the RETRY path:
/// 8. Client resends SAME request_id
/// 9. Server dedup_lookup -> HIT, returns cached bytes
/// 10. Router MUST NOT be invoked a second time
/// 11. No new frames pushed into replay buffer
///
/// Then the REPLAY path:
/// 12. Client simulates reconnect with last_recv_seq=0
/// 13. store.resume() returns all originally-pushed frames with their server_seqs
use std::time::Duration;

use bytes::BytesMut;
use me_clock::SystemClock;
use me_core::symbol::SymbolConfig;
use me_engine::command::Command;
use me_engine::router::Router;
use me_io::codec::{decode_frame, encode_events, ConnectionCodecState, DecodedFrame};
use me_io::protocol::{ConnectionState, MessageType, FRAME_HEADER_SIZE, PROTOCOL_VERSION};
use me_io::session_store::{ReplayResult, SessionStore};

static SYS_CLK: SystemClock = SystemClock;

/// Build a wire frame: 22-byte header + payload.
fn build_frame(msg_type: u8, seq: u64, request_id: u64, payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(FRAME_HEADER_SIZE + payload.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.push(PROTOCOL_VERSION);
    buf.push(msg_type);
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.extend_from_slice(&request_id.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Build a PlaceOrder payload for a valid limit BUY at price=1.0, qty given.
/// Payload layout matches crates/me-io/src/codec.rs::decode_place_order.
fn place_order_payload(order_id: u64, symbol: u16, qty_raw: u64) -> Vec<u8> {
    let mut p = Vec::with_capacity(37);
    p.extend_from_slice(&order_id.to_le_bytes()); // 8
    p.extend_from_slice(&symbol.to_le_bytes()); // 2
    p.push(0); // side = Buy
    p.push(0); // order_type = Limit
    p.push(0); // TIF = GTC
    let price: i64 = 100_000_000; // price=1.0 in 8dp
    p.extend_from_slice(&price.to_le_bytes()); // 8
    let stop_price: i64 = 0;
    p.extend_from_slice(&stop_price.to_le_bytes()); // 8
    p.extend_from_slice(&qty_raw.to_le_bytes()); // 8
    p
}

/// Stand up an active ConnectionCodecState bound to a known session_id.
fn active_conn(session_id: me_io::session_store::SessionId) -> ConnectionCodecState {
    let mut conn = ConnectionCodecState::new();
    conn.state = ConnectionState::Active;
    conn.session_id = session_id;
    conn
}

/// Mirror of the server handler for PlaceOrder. Returns (bytes_written_count,
/// dedup_cache_hit) for assertions.
fn handle_place_order(
    router: &mut Router,
    store: &SessionStore,
    conn: &mut ConnectionCodecState,
    server_frame_seq: &mut u64,
    wire_frame: &[u8],
    write_buf: &mut BytesMut,
) -> bool {
    write_buf.clear();
    let (decoded, _consumed) = decode_frame(wire_frame, conn, &SYS_CLK).expect("frame must decode");

    let (mut cmd, seq, request_id) = match decoded {
        DecodedFrame::Command(c, s, r) => (c, s, r),
        _ => panic!("expected Command decoded frame"),
    };

    // dedup lookup
    if let Some(cached) = store.dedup_lookup(&conn.session_id, request_id) {
        write_buf.extend_from_slice(&cached);
        return true; // cache hit
    }

    // inject session_id into order
    if let Command::PlaceOrder(ref mut ord) = cmd {
        ord.client_session_id = conn.session_id;
    }

    let events = router.process(cmd);
    encode_events(&events, seq, request_id, write_buf);

    // rewrite each frame's seq with monotonic server_frame_seq
    let mut block: Vec<u8> = write_buf.to_vec();
    let count = store.push_frames_with_seq_rewrite(&conn.session_id, *server_frame_seq, &mut block);
    *server_frame_seq += count;
    // Copy the mutated bytes back to write_buf (headers now carry real seqs).
    write_buf.clear();
    write_buf.extend_from_slice(&block);

    // cache the finalized response
    store.dedup_insert(
        &conn.session_id,
        request_id,
        bytes::Bytes::copy_from_slice(write_buf),
    );
    false // cache miss (fresh execution)
}

#[test]
fn e2e_place_order_retry_is_cached_and_not_reprocessed() {
    // Setup: router with BTC/USDT symbol, session store, in-proc state.
    let mut router = Router::new(1);
    router.add_symbol(SymbolConfig::default_btc_usdt());
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let session_id = store.create();
    let mut conn = active_conn(session_id);
    let mut server_frame_seq = 1u64; // first outbound frame gets seq=1
    let mut write_buf = BytesMut::new();

    // Wire frame: PlaceOrder with client-seq=1, request_id=0xDEADBEEF.
    let payload = place_order_payload(100, 0, 100_000_000);
    let wire = build_frame(MessageType::PlaceOrder as u8, 1, 0xDEADBEEF, &payload);

    // First dispatch: fresh execution.
    let cached1 = handle_place_order(
        &mut router,
        &store,
        &mut conn,
        &mut server_frame_seq,
        &wire,
        &mut write_buf,
    );
    assert!(!cached1, "first call must be cache miss");
    let frames_after_first = server_frame_seq;
    assert!(frames_after_first >= 1, "at least one event frame emitted");
    let first_response_bytes = write_buf.clone().freeze();

    // Snapshot the book state: exactly one resting order.
    let orders_before = router.get_open_orders_by_session(session_id).unwrap();
    assert_eq!(
        orders_before.len(),
        1,
        "PlaceOrder created exactly 1 resting order"
    );
    assert_eq!(orders_before[0].id, 100);

    // Retry: SAME wire bytes, SAME request_id.
    // Client's conn.last_client_seq is now 1; the retry's seq=1 must still
    // pass sequence validation (seq needs to be > last_client_seq). So we
    // simulate a fresh conn state as if this were a new TCP connection after
    // reconnect — in real life the client would use seq=2 (monotonic). For the
    // dedup test, though, we just reset the conn's last_client_seq back.
    // OR: use a higher seq; request_id is what triggers dedup.
    let retry_wire = build_frame(
        MessageType::PlaceOrder as u8,
        2,          // higher seq (strictly monotonic — required by decoder)
        0xDEADBEEF, // SAME request_id
        &payload,
    );
    let cached2 = handle_place_order(
        &mut router,
        &store,
        &mut conn,
        &mut server_frame_seq,
        &retry_wire,
        &mut write_buf,
    );

    assert!(cached2, "retry with same request_id MUST hit dedup cache");
    assert_eq!(
        server_frame_seq, frames_after_first,
        "cache hit path MUST NOT advance server_frame_seq"
    );
    assert_eq!(
        &write_buf[..],
        &first_response_bytes[..],
        "cached bytes must match original response"
    );

    // Verify the book STILL has only 1 order — retry was NOT re-executed.
    let orders_after = router.get_open_orders_by_session(session_id).unwrap();
    assert_eq!(orders_after.len(), 1, "dedup must prevent double-placement");
}

#[test]
fn e2e_replay_after_disconnect_returns_stored_seqs() {
    // Exercise: submit 3 orders, then simulate reconnect with last_recv=0 —
    // all 3 OrderAccepted frames come back in order with monotonic server_seqs.
    let mut router = Router::new(1);
    router.add_symbol(SymbolConfig::default_btc_usdt());
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let session_id = store.create();
    let mut conn = active_conn(session_id);
    let mut server_frame_seq = 1u64; // first outbound frame gets seq=1
    let mut write_buf = BytesMut::new();

    for client_seq in 1..=3u64 {
        let payload = place_order_payload(client_seq, 0, 100_000_000);
        let wire = build_frame(
            MessageType::PlaceOrder as u8,
            client_seq,
            client_seq, // use client_seq as request_id (unique per call)
            &payload,
        );
        handle_place_order(
            &mut router,
            &store,
            &mut conn,
            &mut server_frame_seq,
            &wire,
            &mut write_buf,
        );
    }

    // server_frame_seq is "next to assign": after pushing 3 frames with seqs
    // 1,2,3 it points at 4.
    assert_eq!(
        server_frame_seq, 4,
        "3 PlaceOrders → seqs 1,2,3 assigned, next is 4"
    );

    // Simulate reconnect: "I've received nothing, give me all frames"
    match store.resume(&session_id, 0) {
        ReplayResult::Ok(frames) => {
            assert_eq!(frames.len(), 3);
            // Each frame's header.seq MUST equal 1, 2, 3 in order.
            for (i, f) in frames.iter().enumerate() {
                let seq = u64::from_le_bytes(f[6..14].try_into().unwrap());
                assert_eq!(seq, (i + 1) as u64, "replayed frame[{i}] has wrong seq");
            }
        }
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn e2e_ack_purges_replay_buffer_preventing_redelivery() {
    // After client ACKs the server's frames, those entries are removed from
    // the replay buffer. A subsequent resume(last_recv=ack_level) still works
    // because gap boundary only includes stuff that was ACKED AHEAD of what
    // the client claims to have.
    let mut router = Router::new(1);
    router.add_symbol(SymbolConfig::default_btc_usdt());
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let session_id = store.create();
    let mut conn = active_conn(session_id);
    let mut server_frame_seq = 1u64; // first outbound frame gets seq=1
    let mut write_buf = BytesMut::new();

    // Submit 5 orders.
    for i in 1..=5u64 {
        let payload = place_order_payload(i, 0, 100_000_000);
        let wire = build_frame(MessageType::PlaceOrder as u8, i, i, &payload);
        handle_place_order(
            &mut router,
            &store,
            &mut conn,
            &mut server_frame_seq,
            &wire,
            &mut write_buf,
        );
    }
    assert_eq!(server_frame_seq, 6, "5 frames assigned (1..=5), next is 6");
    assert_eq!(store.buffer_len(&session_id), 5);

    // Client ACKs up to server_seq=3 (received frames 1,2,3).
    let purged = store.apply_ack(&session_id, 3);
    assert_eq!(purged, 3);
    assert_eq!(store.buffer_len(&session_id), 2);

    // Resume claiming last_recv=3 should return frames 4,5 only.
    match store.resume(&session_id, 3) {
        ReplayResult::Ok(frames) => {
            assert_eq!(frames.len(), 2);
            let seq4 = u64::from_le_bytes(frames[0][6..14].try_into().unwrap());
            let seq5 = u64::from_le_bytes(frames[1][6..14].try_into().unwrap());
            assert_eq!(seq4, 4);
            assert_eq!(seq5, 5);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn e2e_get_open_orders_snapshot_sees_session_orders_only() {
    // Cross-session isolation: two sessions each place orders, GetOpenOrders
    // snapshots return only the calling session's orders.
    let mut router = Router::new(1);
    router.add_symbol(SymbolConfig::default_btc_usdt());
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let session_a = store.create();
    let session_b = store.create();

    let mut conn_a = active_conn(session_a);
    let mut conn_b = active_conn(session_b);
    let mut seq_a = 1u64;
    let mut seq_b = 1u64;
    let mut wbuf = BytesMut::new();

    // Payload layout: order_id(0..8) symbol(8..10) side(10) type(11) tif(12)
    // price(13..21) stop_price(21..29) qty(29..37)
    const PRICE_OFFSET: usize = 13;
    const SIDE_OFFSET: usize = 10;

    // Session A places 3 BUY orders at distinct prices.
    for oid in 1..=3u64 {
        let mut payload = place_order_payload(oid, 0, 100_000_000);
        let price: i64 = 100_000_000 + (oid as i64) * 1_000_000;
        payload[PRICE_OFFSET..PRICE_OFFSET + 8].copy_from_slice(&price.to_le_bytes());
        let wire = build_frame(MessageType::PlaceOrder as u8, oid, oid, &payload);
        handle_place_order(
            &mut router,
            &store,
            &mut conn_a,
            &mut seq_a,
            &wire,
            &mut wbuf,
        );
    }

    // Session B places 1 SELL order far above A's buys (no cross).
    let mut payload = place_order_payload(100, 0, 100_000_000);
    payload[SIDE_OFFSET] = 1; // SELL
    let price: i64 = 200_000_000; // 2.0
    payload[PRICE_OFFSET..PRICE_OFFSET + 8].copy_from_slice(&price.to_le_bytes());
    let wire = build_frame(MessageType::PlaceOrder as u8, 1, 1, &payload);
    handle_place_order(
        &mut router,
        &store,
        &mut conn_b,
        &mut seq_b,
        &wire,
        &mut wbuf,
    );

    // Snapshot each session.
    let snap_a = router.get_open_orders_by_session(session_a).unwrap();
    let snap_b = router.get_open_orders_by_session(session_b).unwrap();

    assert_eq!(snap_a.len(), 3, "session A has 3 resting orders");
    assert_eq!(snap_b.len(), 1, "session B has 1 resting order");

    for o in &snap_a {
        assert_eq!(o.client_session_id, session_a);
    }
    for o in &snap_b {
        assert_eq!(o.client_session_id, session_b);
        assert_eq!(o.id, 100);
    }

    // Different sessions' orders are on different shards/buckets, but the
    // scan correctly aggregates by session_id.
}

#[test]
fn e2e_cancel_order_after_place_with_dedup() {
    // Place then cancel — both are separate requests; dedup of each is
    // independent.
    let mut router = Router::new(1);
    router.add_symbol(SymbolConfig::default_btc_usdt());
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let session_id = store.create();
    let mut conn = active_conn(session_id);
    let mut server_frame_seq = 1u64; // first outbound frame gets seq=1
    let mut wbuf = BytesMut::new();

    // Place order id=7.
    let payload = place_order_payload(7, 0, 100_000_000);
    let wire = build_frame(MessageType::PlaceOrder as u8, 1, 0xAAAA, &payload);
    handle_place_order(
        &mut router,
        &store,
        &mut conn,
        &mut server_frame_seq,
        &wire,
        &mut wbuf,
    );
    assert_eq!(
        router.get_open_orders_by_session(session_id).unwrap().len(),
        1,
        "order placed"
    );

    // Send CancelOrder frame.
    let mut cancel_payload = Vec::with_capacity(10);
    cancel_payload.extend_from_slice(&7u64.to_le_bytes());
    cancel_payload.extend_from_slice(&0u16.to_le_bytes());
    let cancel_wire = build_frame(MessageType::CancelOrder as u8, 2, 0xBBBB, &cancel_payload);
    handle_place_order(
        &mut router,
        &store,
        &mut conn,
        &mut server_frame_seq,
        &cancel_wire,
        &mut wbuf,
    );

    // Snapshot should now be empty.
    assert_eq!(
        router.get_open_orders_by_session(session_id).unwrap().len(),
        0,
        "order cancelled"
    );

    // Retry the CANCEL (same request_id) — must NOT produce a second cancel
    // event, and must return the cached response.
    let retry_cancel = build_frame(
        MessageType::CancelOrder as u8,
        3,
        0xBBBB, // SAME request_id as cancel
        &cancel_payload,
    );
    let cached = handle_place_order(
        &mut router,
        &store,
        &mut conn,
        &mut server_frame_seq,
        &retry_cancel,
        &mut wbuf,
    );
    assert!(cached, "cancel retry must be dedup cache hit");
}

/// Handler for BatchPlaceOrder that mirrors tcp_server.rs:413 — the batch
/// path processes each command separately then caches the aggregated response.
fn handle_batch_place_order(
    router: &mut Router,
    store: &SessionStore,
    conn: &mut ConnectionCodecState,
    server_frame_seq: &mut u64,
    wire_frame: &[u8],
    write_buf: &mut BytesMut,
    router_calls: &mut u32,
) -> bool {
    write_buf.clear();
    let (decoded, _) = decode_frame(wire_frame, conn, &SYS_CLK).expect("batch frame must decode");

    let (cmds, seq, request_id) = match decoded {
        DecodedFrame::Batch(cmds, seq, rid) => (cmds, seq, rid),
        _ => panic!("expected Batch decoded frame"),
    };

    // dedup lookup
    if let Some(cached) = store.dedup_lookup(&conn.session_id, request_id) {
        write_buf.extend_from_slice(&cached);
        return true; // cache hit — no router calls made
    }

    // per-cmd processing
    for mut cmd in cmds {
        if let Command::PlaceOrder(ref mut ord) = cmd {
            ord.client_session_id = conn.session_id;
        }
        let events = router.process(cmd);
        *router_calls += 1;
        encode_events(&events, seq, request_id, write_buf);
    }

    // rewrite seqs
    let mut block: Vec<u8> = write_buf.to_vec();
    let count = store.push_frames_with_seq_rewrite(&conn.session_id, *server_frame_seq, &mut block);
    *server_frame_seq += count;
    write_buf.clear();
    write_buf.extend_from_slice(&block);

    // cache aggregated response
    store.dedup_insert(
        &conn.session_id,
        request_id,
        bytes::Bytes::copy_from_slice(write_buf),
    );
    false
}

#[test]
fn e2e_batch_place_order_retry_is_cached_and_not_reprocessed() {
    // Build a BatchPlaceOrder frame containing 3 orders.
    // Frame layout: header(22) + payload[count(1) + sub_payload_37*3]
    fn build_batch(seq: u64, request_id: u64, count: u8, sub_payloads: &[Vec<u8>]) -> Vec<u8> {
        let mut payload = vec![count];
        for p in sub_payloads {
            payload.extend_from_slice(p);
        }
        build_frame(
            MessageType::BatchPlaceOrder as u8,
            seq,
            request_id,
            &payload,
        )
    }

    let mut router = Router::new(1);
    router.add_symbol(SymbolConfig::default_btc_usdt());
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let session_id = store.create();
    let mut conn = active_conn(session_id);
    let mut server_frame_seq = 1u64;
    let mut wbuf = BytesMut::new();
    let mut router_calls = 0u32;

    // Build 3 distinct sub-orders. BatchPlaceOrder decoder reads 38 bytes
    // per sub when remaining ≥ 38, so we pad each sub with a trailing byte.
    fn batch_sub(order_id: u64, qty_raw: u64) -> Vec<u8> {
        let mut p = place_order_payload(order_id, 0, qty_raw);
        p.push(0); // 38-byte padded sub-payload (trailing flags byte)
        p
    }
    let subs = vec![
        batch_sub(10, 100_000_000),
        batch_sub(11, 100_000_000),
        batch_sub(12, 100_000_000),
    ];
    let wire = build_batch(1, 0xF001, 3, &subs);

    // First dispatch: fresh execution, 3 router calls.
    let cached1 = handle_batch_place_order(
        &mut router,
        &store,
        &mut conn,
        &mut server_frame_seq,
        &wire,
        &mut wbuf,
        &mut router_calls,
    );
    assert!(!cached1, "first batch call: cache miss");
    assert_eq!(router_calls, 3, "3 orders → 3 router.process() invocations");

    let orders_after_first = router.get_open_orders_by_session(session_id).unwrap();
    assert_eq!(orders_after_first.len(), 3, "all 3 orders placed");
    let first_response = wbuf.clone().freeze();

    // Retry with SAME request_id (seq must be strictly monotonic).
    let retry_wire = build_batch(2, 0xF001, 3, &subs);
    let cached2 = handle_batch_place_order(
        &mut router,
        &store,
        &mut conn,
        &mut server_frame_seq,
        &retry_wire,
        &mut wbuf,
        &mut router_calls,
    );

    assert!(cached2, "batch retry MUST hit dedup cache");
    assert_eq!(
        router_calls, 3,
        "retry: router.process() NOT invoked again (dedup prevents double-placement)"
    );
    assert_eq!(
        &wbuf[..],
        &first_response[..],
        "cached batch response bytes match original"
    );

    // Book STILL has only 3 orders — retry was NOT re-executed.
    assert_eq!(
        router.get_open_orders_by_session(session_id).unwrap().len(),
        3,
        "dedup prevents double-placement of batch"
    );
}

#[test]
fn e2e_dedup_cache_hit_does_not_grow_replay_buffer() {
    // Coverage gap #6: verify that a retry-via-cache-hit does NOT push
    // another copy of the response into the replay buffer. Otherwise a
    // misbehaving client could inflate the buffer without bound.
    let mut router = Router::new(1);
    router.add_symbol(SymbolConfig::default_btc_usdt());
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let session_id = store.create();
    let mut conn = active_conn(session_id);
    let mut server_frame_seq = 1u64;
    let mut wbuf = BytesMut::new();

    // Submit once.
    let payload = place_order_payload(1, 0, 100_000_000);
    let wire = build_frame(MessageType::PlaceOrder as u8, 1, 0xAAAA, &payload);
    handle_place_order(
        &mut router,
        &store,
        &mut conn,
        &mut server_frame_seq,
        &wire,
        &mut wbuf,
    );
    let buf_after_first = store.buffer_len(&session_id);
    assert!(buf_after_first >= 1);

    // Retry the SAME request_id 5 times — buffer MUST NOT grow.
    for i in 2..=6u64 {
        let retry = build_frame(MessageType::PlaceOrder as u8, i, 0xAAAA, &payload);
        handle_place_order(
            &mut router,
            &store,
            &mut conn,
            &mut server_frame_seq,
            &retry,
            &mut wbuf,
        );
        assert_eq!(
            store.buffer_len(&session_id),
            buf_after_first,
            "retry #{}: cache-hit path MUST NOT re-push frames",
            i - 1
        );
    }
}
