/// Phase 1 tests for Protocol v2 — frame header with request_id.
///
/// These tests verify:
/// - Header size is exactly 22 bytes (v2 = v1 14 bytes + 8 bytes request_id)
/// - request_id is correctly encoded and decoded
/// - Commands with request_id=0 are rejected (InvalidRequestId)
/// - Session control messages (Handshake, Ping, ResumeSession) allow request_id=0
/// - Ping now carries piggyback ACK
/// - Pong now carries piggyback server ACK
/// - New encode functions (ReplayGap, OpenOrdersBegin/End, FlowControlWarning)
use bytes::BytesMut;
use me_io::codec::{
    decode_frame, encode_flow_control_warning, encode_open_orders_begin, encode_open_orders_end,
    encode_pong, encode_replay_gap, ConnectionCodecState, DecodeError, DecodedFrame,
};
use me_io::protocol::{ConnectionState, MessageType, FRAME_HEADER_SIZE, PROTOCOL_VERSION};

/// System clock for tests.
static SYS_CLK: me_clock::SystemClock = me_clock::SystemClock;

// ─── Header size ─────────────────────────────────────────────────────────────

#[test]
fn test_frame_header_size_is_22_bytes_v2() {
    assert_eq!(
        FRAME_HEADER_SIZE, 22,
        "v2 header = len(4)+ver(1)+type(1)+seq(8)+req_id(8)"
    );
}

#[test]
fn test_protocol_version_is_2() {
    assert_eq!(PROTOCOL_VERSION, 2);
}

// ─── Frame builder ──────────────────────────────────────────────────────────

fn build_frame_v2(msg_type: u8, seq: u64, request_id: u64, payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(FRAME_HEADER_SIZE + payload.len());
    buf.extend_from_slice(&len.to_le_bytes()); // 4 bytes
    buf.push(PROTOCOL_VERSION); // 1 byte
    buf.push(msg_type); // 1 byte
    buf.extend_from_slice(&seq.to_le_bytes()); // 8 bytes
    buf.extend_from_slice(&request_id.to_le_bytes()); // 8 bytes
    buf.extend_from_slice(payload);
    buf
}

fn active_conn() -> ConnectionCodecState {
    let mut conn = ConnectionCodecState::new();
    conn.state = ConnectionState::Active;
    conn
}

// ─── PlaceOrder request_id round-trip ───────────────────────────────────────

fn place_order_payload_v2(order_id: u64, symbol: u16, qty_raw: u64) -> Vec<u8> {
    // OrderId(8)+Symbol(2)+Side(1)+Type(1)+TIF(1)+Price(8)+StopPrice(8)+Qty(8) = 37
    let mut p = Vec::with_capacity(37);
    p.extend_from_slice(&order_id.to_le_bytes());
    p.extend_from_slice(&symbol.to_le_bytes());
    p.push(0); // side = Buy
    p.push(0); // order_type = Limit
    p.push(0); // TIF = GTC
    let price: i64 = 100_000_000; // 1.0 in 8dp
    p.extend_from_slice(&price.to_le_bytes());
    let stop_price: i64 = 0;
    p.extend_from_slice(&stop_price.to_le_bytes());
    p.extend_from_slice(&qty_raw.to_le_bytes());
    p
}

#[test]
fn test_place_order_request_id_round_trip() {
    let mut conn = active_conn();
    let payload = place_order_payload_v2(42, 0, 100_000_000);
    let frame = build_frame_v2(
        MessageType::PlaceOrder as u8,
        1,
        0xABCDEF1234567890,
        &payload,
    );

    let (decoded, consumed) =
        decode_frame(&frame, &mut conn, &SYS_CLK).expect("PlaceOrder should decode");
    assert_eq!(consumed, frame.len());

    match decoded {
        DecodedFrame::Command(_, seq, request_id) => {
            assert_eq!(seq, 1);
            assert_eq!(request_id, 0xABCDEF1234567890);
        }
        _ => panic!("expected Command variant"),
    }
}

#[test]
fn test_command_with_zero_request_id_is_rejected() {
    let mut conn = active_conn();
    let payload = place_order_payload_v2(42, 0, 100_000_000);
    // request_id = 0 for a command — MUST be rejected
    let frame = build_frame_v2(MessageType::PlaceOrder as u8, 1, 0, &payload);

    let result = decode_frame(&frame, &mut conn, &SYS_CLK);
    match result {
        Err(DecodeError::InvalidRequestId) => {} // expected
        Err(e) => panic!("expected InvalidRequestId, got {e:?}"),
        Ok(_) => panic!("expected error, got Ok"),
    }
}

#[test]
fn test_cancel_order_with_zero_request_id_is_rejected() {
    let mut conn = active_conn();
    // CancelOrder payload: order_id(8) + symbol(2)
    let mut payload = Vec::with_capacity(10);
    payload.extend_from_slice(&42u64.to_le_bytes());
    payload.extend_from_slice(&0u16.to_le_bytes());
    let frame = build_frame_v2(MessageType::CancelOrder as u8, 1, 0, &payload);

    let result = decode_frame(&frame, &mut conn, &SYS_CLK);
    assert!(matches!(result, Err(DecodeError::InvalidRequestId)));
}

#[test]
fn test_handshake_allows_zero_request_id() {
    let mut conn = ConnectionCodecState::new();
    // Handshake payload: server_version(1)
    let payload = vec![PROTOCOL_VERSION];
    // request_id=0 is OK for session control messages
    let frame = build_frame_v2(MessageType::Handshake as u8, 0, 0, &payload);

    let (decoded, _) =
        decode_frame(&frame, &mut conn, &SYS_CLK).expect("Handshake with req_id=0 should decode");
    assert!(matches!(decoded, DecodedFrame::Handshake { .. }));
}

// ─── Ping with piggyback ACK ─────────────────────────────────────────────────

#[test]
fn test_ping_with_piggyback_ack() {
    let mut conn = active_conn();
    // Ping payload: last_processed_server_seq(8)
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&12345u64.to_le_bytes());
    let frame = build_frame_v2(MessageType::Ping as u8, 1, 0, &payload);

    let (decoded, _) = decode_frame(&frame, &mut conn, &SYS_CLK).expect("Ping should decode");
    match decoded {
        DecodedFrame::Ping { seq, ack_seq } => {
            assert_eq!(seq, 1);
            assert_eq!(ack_seq, 12345, "Ping should carry piggyback ACK");
        }
        _ => panic!("expected Ping variant"),
    }
}

#[test]
fn test_ping_without_payload_defaults_ack_to_zero() {
    let mut conn = active_conn();
    // Ping with empty payload (for backward compat during tests) — ack_seq defaults to 0
    let frame = build_frame_v2(MessageType::Ping as u8, 1, 0, &[]);

    let (decoded, _) = decode_frame(&frame, &mut conn, &SYS_CLK).expect("Ping should decode");
    match decoded {
        DecodedFrame::Ping { ack_seq, .. } => {
            assert_eq!(ack_seq, 0);
        }
        _ => panic!("expected Ping variant"),
    }
}

// ─── Acknowledge message ────────────────────────────────────────────────────

#[test]
fn test_acknowledge_frame() {
    let mut conn = active_conn();
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&99999u64.to_le_bytes());
    let frame = build_frame_v2(MessageType::Acknowledge as u8, 1, 0, &payload);

    let (decoded, _) =
        decode_frame(&frame, &mut conn, &SYS_CLK).expect("Acknowledge should decode");
    match decoded {
        DecodedFrame::Acknowledge {
            last_processed_server_seq,
            seq,
        } => {
            assert_eq!(seq, 1);
            assert_eq!(last_processed_server_seq, 99999);
        }
        _ => panic!("expected Acknowledge variant"),
    }
}

// ─── GetOpenOrders ──────────────────────────────────────────────────────────

#[test]
fn test_get_open_orders_frame() {
    let mut conn = active_conn();
    // Empty payload, non-zero request_id required
    let frame = build_frame_v2(MessageType::GetOpenOrders as u8, 1, 42, &[]);

    let (decoded, _) =
        decode_frame(&frame, &mut conn, &SYS_CLK).expect("GetOpenOrders should decode");
    match decoded {
        DecodedFrame::GetOpenOrders { seq, request_id } => {
            assert_eq!(seq, 1);
            assert_eq!(request_id, 42);
        }
        _ => panic!("expected GetOpenOrders variant"),
    }
}

#[test]
fn test_get_open_orders_requires_non_zero_request_id() {
    let mut conn = active_conn();
    let frame = build_frame_v2(MessageType::GetOpenOrders as u8, 1, 0, &[]);

    let result = decode_frame(&frame, &mut conn, &SYS_CLK);
    assert!(matches!(result, Err(DecodeError::InvalidRequestId)));
}

// ─── Pong with piggyback ACK (encode) ───────────────────────────────────────

#[test]
fn test_encode_pong_v2_carries_ack() {
    let mut buf = BytesMut::new();
    encode_pong(5, 12345, &mut buf);

    // Header 22 + payload 8 (ack_seq) = 30
    assert_eq!(buf.len(), 30);

    // Payload len
    let payload_len = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    assert_eq!(payload_len, 8);

    // Version + type
    assert_eq!(buf[4], PROTOCOL_VERSION);
    assert_eq!(buf[5], MessageType::Pong as u8);

    // seq
    let seq = u64::from_le_bytes(buf[6..14].try_into().unwrap());
    assert_eq!(seq, 5);

    // request_id = 0 for Pong
    let req_id = u64::from_le_bytes(buf[14..22].try_into().unwrap());
    assert_eq!(req_id, 0);

    // Piggyback ACK value
    let ack = u64::from_le_bytes(buf[22..30].try_into().unwrap());
    assert_eq!(ack, 12345);
}

// ─── Server→client frames have request_id=0 in header (or mirrored) ─────────

#[test]
fn test_encode_events_mirrors_request_id() {
    use me_core::events::MatchEvent;
    use me_io::codec::encode_events;

    let mut buf = BytesMut::new();
    let events = vec![MatchEvent::OrderAccepted {
        order_id: 100,
        timestamp: 5000,
    }];
    // Mirror request_id=777 from triggering command
    encode_events(&events, 1, 777, &mut buf);

    // Verify request_id in header matches the mirrored value
    let req_id = u64::from_le_bytes(buf[14..22].try_into().unwrap());
    assert_eq!(
        req_id, 777,
        "Server response should mirror client's request_id"
    );
}

#[test]
fn test_encode_events_unsolicited_zero_request_id() {
    use me_core::events::MatchEvent;
    use me_io::codec::encode_events;

    let mut buf = BytesMut::new();
    let events = vec![MatchEvent::StopTriggered {
        order_id: 200,
        trigger_price: me_core::types::FixedPrice::from_raw(100_000_000),
        timestamp: 5000,
    }];
    // Unsolicited event (e.g., stop triggered by market move) → request_id=0
    encode_events(&events, 1, 0, &mut buf);

    let req_id = u64::from_le_bytes(buf[14..22].try_into().unwrap());
    assert_eq!(req_id, 0, "Unsolicited event should have request_id=0");
}

// ─── New server→client message encoders ─────────────────────────────────────

#[test]
fn test_encode_replay_gap() {
    let mut buf = BytesMut::new();
    encode_replay_gap(10, 100, 500, &mut buf);

    // Header 22 + payload 16 (missing_from + missing_to) = 38
    assert_eq!(buf.len(), 38);
    assert_eq!(buf[5], MessageType::ReplayGap as u8);

    let payload_len = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    assert_eq!(payload_len, 16);

    let missing_from = u64::from_le_bytes(buf[22..30].try_into().unwrap());
    let missing_to = u64::from_le_bytes(buf[30..38].try_into().unwrap());
    assert_eq!(missing_from, 100);
    assert_eq!(missing_to, 500);
}

#[test]
fn test_encode_open_orders_begin() {
    let mut buf = BytesMut::new();
    encode_open_orders_begin(5, 42, 7, 999, &mut buf);

    // Header 22 + payload 12 (count(4) + snapshot_seq(8)) = 34
    assert_eq!(buf.len(), 34);
    assert_eq!(buf[5], MessageType::OpenOrdersBegin as u8);

    // request_id mirrored
    let req_id = u64::from_le_bytes(buf[14..22].try_into().unwrap());
    assert_eq!(req_id, 42);

    let count = u32::from_le_bytes(buf[22..26].try_into().unwrap());
    let snapshot_seq = u64::from_le_bytes(buf[26..34].try_into().unwrap());
    assert_eq!(count, 7);
    assert_eq!(snapshot_seq, 999);
}

#[test]
fn test_encode_open_orders_end() {
    let mut buf = BytesMut::new();
    encode_open_orders_end(5, 42, 999, &mut buf);

    // Header 22 + payload 8 (snapshot_seq) = 30
    assert_eq!(buf.len(), 30);
    assert_eq!(buf[5], MessageType::OpenOrdersEnd as u8);

    let snapshot_seq = u64::from_le_bytes(buf[22..30].try_into().unwrap());
    assert_eq!(snapshot_seq, 999);
}

#[test]
fn test_encode_flow_control_warning_soft() {
    let mut buf = BytesMut::new();
    encode_flow_control_warning(5, 80, false, &mut buf);

    // Header 22 + payload 2 = 24
    assert_eq!(buf.len(), 24);
    assert_eq!(buf[5], MessageType::FlowControlWarning as u8);
    assert_eq!(buf[22], 80); // buffer_used_pct
    assert_eq!(buf[23], 0); // is_hard_limit = false
}

#[test]
fn test_encode_flow_control_warning_hard() {
    let mut buf = BytesMut::new();
    encode_flow_control_warning(5, 100, true, &mut buf);

    assert_eq!(buf.len(), 24);
    assert_eq!(buf[22], 100);
    assert_eq!(buf[23], 1); // is_hard_limit = true
}

// ─── Sequence validation still works in v2 ─────────────────────────────────

#[test]
fn test_sequence_out_of_order_still_rejected() {
    let mut conn = active_conn();
    conn.last_client_seq = 5;

    let payload = place_order_payload_v2(42, 0, 100_000_000);
    // seq=3 < last_client_seq=5 should be rejected
    let frame = build_frame_v2(MessageType::PlaceOrder as u8, 3, 1, &payload);

    let result = decode_frame(&frame, &mut conn, &SYS_CLK);
    assert!(matches!(
        result,
        Err(DecodeError::SequenceOutOfOrder { .. })
    ));
}

// ─── Version mismatch rejection ─────────────────────────────────────────────

#[test]
fn test_v1_frame_rejected_by_v2_server() {
    let mut conn = ConnectionCodecState::new();
    // Simulate a v1 client sending a Handshake (14-byte header, version=1)
    // This should be rejected because our server is v2.
    let mut frame = Vec::new();
    frame.extend_from_slice(&0u32.to_le_bytes()); // len
    frame.push(1); // version = 1 (old)
    frame.push(MessageType::Handshake as u8);
    frame.extend_from_slice(&0u64.to_le_bytes()); // seq
                                                  // v1 frame ends here — 14 bytes total (no request_id)

    // Pad to FRAME_HEADER_SIZE so the decoder reads a "full" header
    // (it will interpret the padding as request_id=0, but version check will fail first)
    while frame.len() < FRAME_HEADER_SIZE {
        frame.push(0);
    }

    let result = decode_frame(&frame, &mut conn, &SYS_CLK);
    assert!(matches!(result, Err(DecodeError::UnsupportedVersion(1))));
}
