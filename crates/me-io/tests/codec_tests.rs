use bytes::BytesMut;
use me_core::events::{MatchEvent, RejectReason};
use me_core::trade::Trade;
use me_core::types::{FixedPrice, FixedQty, Side, SymbolId};
use me_engine::command::Command;
use me_io::codec::{
    decode_frame, encode_error, encode_events, encode_handshake_ack, encode_pong,
    ConnectionCodecState, DecodeError, DecodedFrame,
};
use me_io::protocol::{
    ConnectionState, ErrorCode, FRAME_HEADER_SIZE, MAX_PAYLOAD_SIZE, PROTOCOL_VERSION,
};

// ─── Frame builder helpers ───────────────────────────────────────────────────

fn make_frame(msg_type: u8, seq: u64, payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(FRAME_HEADER_SIZE + payload.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.push(PROTOCOL_VERSION);
    buf.push(msg_type);
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

fn make_frame_with_version(msg_type: u8, seq: u64, version: u8, payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(FRAME_HEADER_SIZE + payload.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.push(version);
    buf.push(msg_type);
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Build a PlaceOrder payload (37 bytes).
#[allow(clippy::too_many_arguments)]
fn place_order_payload(
    order_id: u64,
    symbol: u16,
    side: u8,
    order_type: u8,
    tif: u8,
    price: i64,
    stop_price: i64,
    qty: u64,
) -> Vec<u8> {
    let mut p = Vec::with_capacity(37);
    p.extend_from_slice(&order_id.to_le_bytes());
    p.extend_from_slice(&symbol.to_le_bytes());
    p.push(side);
    p.push(order_type);
    p.push(tif);
    p.extend_from_slice(&price.to_le_bytes());
    p.extend_from_slice(&stop_price.to_le_bytes());
    p.extend_from_slice(&qty.to_le_bytes());
    p
}

/// Build a CancelOrder payload (10 bytes).
fn cancel_order_payload(order_id: u64, symbol: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(10);
    p.extend_from_slice(&order_id.to_le_bytes());
    p.extend_from_slice(&symbol.to_le_bytes());
    p
}

/// Build an AmendOrder payload.
fn amend_order_payload(
    order_id: u64,
    symbol: u16,
    new_price: Option<i64>,
    new_qty: Option<u64>,
) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&order_id.to_le_bytes());
    p.extend_from_slice(&symbol.to_le_bytes());
    if let Some(price) = new_price {
        p.push(1);
        p.extend_from_slice(&price.to_le_bytes());
    } else {
        p.push(0);
    }
    if let Some(qty) = new_qty {
        p.push(1);
        p.extend_from_slice(&qty.to_le_bytes());
    } else {
        p.push(0);
    }
    p
}

/// Create a state that has completed handshake.
fn active_conn() -> ConnectionCodecState {
    let mut conn = ConnectionCodecState::new();
    let handshake = make_frame(0x05, 1, &[PROTOCOL_VERSION]);
    let (frame, _) = decode_frame(&handshake, &mut conn).expect("handshake must succeed");
    assert!(matches!(frame, DecodedFrame::Handshake { .. }));
    conn.state = ConnectionState::Active;
    conn
}

// ─── Handshake ───────────────────────────────────────────────────────────────

#[test]
fn test_decode_handshake() {
    let mut conn = ConnectionCodecState::new();
    let frame = make_frame(0x05, 0, &[PROTOCOL_VERSION]);
    let (decoded, consumed) = decode_frame(&frame, &mut conn).expect("handshake");
    assert_eq!(consumed, frame.len());
    assert!(matches!(
        decoded,
        DecodedFrame::Handshake {
            client_version: PROTOCOL_VERSION,
            seq: 0
        }
    ));
}

#[test]
fn test_handshake_ack_encode() {
    let mut buf = BytesMut::new();
    let session_id = [0u8; 16];
    let caps = 0u64;
    let hb_ms = 5_000u32;
    encode_handshake_ack(0, session_id, caps, hb_ms, &mut buf);
    // Header (14) + payload: version(1) + cap_tlv(12) + sid_tlv(20) + hb_tlv(8) = 41
    assert_eq!(buf.len(), 14 + 41);
    // Verify version byte in payload
    assert_eq!(buf[14], PROTOCOL_VERSION);
}

// ─── PlaceOrder roundtrip ────────────────────────────────────────────────────

#[test]
fn test_decode_place_order_limit() {
    let mut conn = active_conn();
    // Limit buy BTC/USDT: price 30_000.0, qty 0.5
    let price_raw = FixedPrice::from_f64(30_000.0).raw();
    let qty_raw = FixedQty::from_f64(0.5).raw();
    let payload = place_order_payload(
        42,   // order_id
        1,    // symbol
        0,    // side = Buy
        0,    // order_type = Limit
        0,    // tif = GTC
        price_raw,
        0,    // stop_price (unused for Limit)
        qty_raw,
    );
    let frame = make_frame(0x01, 2, &payload);
    let (decoded, consumed) = decode_frame(&frame, &mut conn).expect("place_order limit");
    assert_eq!(consumed, frame.len());
    match decoded {
        DecodedFrame::Command(Command::PlaceOrder(order), seq) => {
            assert_eq!(seq, 2);
            assert_eq!(order.id, 42);
            assert_eq!(order.symbol, SymbolId(1));
            assert_eq!(order.side, Side::Buy);
            assert_eq!(order.price, FixedPrice::from_f64(30_000.0));
            assert_eq!(order.qty, FixedQty::from_f64(0.5));
        }
        _ => panic!("expected PlaceOrder command"),
    }
}

#[test]
fn test_decode_place_order_market() {
    let mut conn = active_conn();
    // Market sell: price=0, stop_price=0 are OK for market orders
    let qty_raw = FixedQty::from_f64(1.0).raw();
    let payload = place_order_payload(
        100,  // order_id
        2,    // symbol
        1,    // side = Sell
        1,    // order_type = Market
        1,    // tif = IOC
        0,    // price (irrelevant for market)
        0,    // stop_price
        qty_raw,
    );
    let frame = make_frame(0x01, 2, &payload);
    let (decoded, _) = decode_frame(&frame, &mut conn).expect("place_order market");
    match decoded {
        DecodedFrame::Command(Command::PlaceOrder(order), _) => {
            assert_eq!(order.id, 100);
            assert_eq!(order.side, Side::Sell);
            assert_eq!(order.qty, FixedQty::from_f64(1.0));
        }
        _ => panic!("expected PlaceOrder command"),
    }
}

#[test]
fn test_decode_place_order_stop_limit() {
    let mut conn = active_conn();
    let price_raw = FixedPrice::from_f64(29_000.0).raw();
    let stop_price_raw = FixedPrice::from_f64(29_500.0).raw();
    let qty_raw = FixedQty::from_f64(0.1).raw();
    let payload = place_order_payload(
        200,          // order_id
        1,            // symbol
        1,            // side = Sell
        2,            // order_type = StopLimit
        0,            // tif = GTC
        price_raw,
        stop_price_raw,
        qty_raw,
    );
    let frame = make_frame(0x01, 2, &payload);
    let (decoded, _) = decode_frame(&frame, &mut conn).expect("stop_limit");
    match decoded {
        DecodedFrame::Command(Command::PlaceOrder(order), _) => {
            assert_eq!(order.id, 200);
            assert_eq!(order.price, FixedPrice::from_f64(29_000.0));
            assert_eq!(order.stop_price, FixedPrice::from_f64(29_500.0));
        }
        _ => panic!("expected PlaceOrder command"),
    }
}

#[test]
fn test_decode_place_order_stop_market() {
    let mut conn = active_conn();
    let stop_price_raw = FixedPrice::from_f64(28_000.0).raw();
    let qty_raw = FixedQty::from_f64(0.25).raw();
    let payload = place_order_payload(
        300,          // order_id
        1,            // symbol
        1,            // side = Sell
        3,            // order_type = StopMarket
        1,            // tif = IOC
        0,            // price (irrelevant)
        stop_price_raw,
        qty_raw,
    );
    let frame = make_frame(0x01, 2, &payload);
    let (decoded, _) = decode_frame(&frame, &mut conn).expect("stop_market");
    match decoded {
        DecodedFrame::Command(Command::PlaceOrder(order), _) => {
            assert_eq!(order.id, 300);
            assert_eq!(order.stop_price, FixedPrice::from_f64(28_000.0));
        }
        _ => panic!("expected PlaceOrder command"),
    }
}

// ─── CancelOrder roundtrip ───────────────────────────────────────────────────

#[test]
fn test_decode_cancel_order() {
    let mut conn = active_conn();
    let payload = cancel_order_payload(999, 5);
    let frame = make_frame(0x02, 2, &payload);
    let (decoded, consumed) = decode_frame(&frame, &mut conn).expect("cancel_order");
    assert_eq!(consumed, frame.len());
    match decoded {
        DecodedFrame::Command(Command::CancelOrder { order_id, symbol, .. }, seq) => {
            assert_eq!(seq, 2);
            assert_eq!(order_id, 999);
            assert_eq!(symbol, SymbolId(5));
        }
        _ => panic!("expected CancelOrder command"),
    }
}

// ─── AmendOrder roundtrip ────────────────────────────────────────────────────

#[test]
fn test_decode_amend_order_price_only() {
    let mut conn = active_conn();
    let new_price = FixedPrice::from_f64(31_000.0).raw();
    let payload = amend_order_payload(42, 1, Some(new_price), None);
    let frame = make_frame(0x03, 2, &payload);
    let (decoded, _) = decode_frame(&frame, &mut conn).expect("amend price");
    match decoded {
        DecodedFrame::Command(Command::AmendOrder { order_id, new_price, new_qty, .. }, _) => {
            assert_eq!(order_id, 42);
            assert_eq!(new_price, Some(FixedPrice::from_f64(31_000.0)));
            assert_eq!(new_qty, None);
        }
        _ => panic!("expected AmendOrder command"),
    }
}

#[test]
fn test_decode_amend_order_qty_only() {
    let mut conn = active_conn();
    let new_qty = FixedQty::from_f64(2.0).raw();
    let payload = amend_order_payload(42, 1, None, Some(new_qty));
    let frame = make_frame(0x03, 2, &payload);
    let (decoded, _) = decode_frame(&frame, &mut conn).expect("amend qty");
    match decoded {
        DecodedFrame::Command(Command::AmendOrder { new_price, new_qty, .. }, _) => {
            assert_eq!(new_price, None);
            assert_eq!(new_qty, Some(FixedQty::from_f64(2.0)));
        }
        _ => panic!("expected AmendOrder command"),
    }
}

#[test]
fn test_decode_amend_order_both() {
    let mut conn = active_conn();
    let new_price = FixedPrice::from_f64(32_000.0).raw();
    let new_qty = FixedQty::from_f64(0.75).raw();
    let payload = amend_order_payload(42, 1, Some(new_price), Some(new_qty));
    let frame = make_frame(0x03, 2, &payload);
    let (decoded, _) = decode_frame(&frame, &mut conn).expect("amend both");
    match decoded {
        DecodedFrame::Command(Command::AmendOrder { new_price, new_qty, .. }, _) => {
            assert_eq!(new_price, Some(FixedPrice::from_f64(32_000.0)));
            assert_eq!(new_qty, Some(FixedQty::from_f64(0.75)));
        }
        _ => panic!("expected AmendOrder command"),
    }
}

// ─── Outbound event encoding ─────────────────────────────────────────────────

#[test]
fn test_encode_order_accepted() {
    let event = MatchEvent::OrderAccepted {
        order_id: 77,
        timestamp: 12345678,
    };
    let mut buf = BytesMut::new();
    encode_events(&[event], 1, &mut buf);
    // Header (14) + payload (16): order_id(8) + timestamp(8)
    assert_eq!(buf.len(), 30);
    // Payload len = 16 in LE
    let payload_len = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    assert_eq!(payload_len, 16);
    // order_id in payload
    let order_id = u64::from_le_bytes(buf[14..22].try_into().unwrap());
    assert_eq!(order_id, 77);
}

#[test]
fn test_encode_trade() {
    let trade = Trade {
        trade_id: 1,
        symbol: SymbolId(1),
        price: FixedPrice::from_f64(30_000.0),
        qty: FixedQty::from_f64(0.5),
        maker_order_id: 10,
        taker_order_id: 20,
        maker_side: Side::Buy,
        timestamp: 99999,
        sequence: 1,
    };
    let event = MatchEvent::Trade(trade);
    let mut buf = BytesMut::new();
    encode_events(&[event], 1, &mut buf);
    // Header (14) + payload (59): trade_id(8) + symbol(2) + price(8) + qty(8) + maker_order_id(8) + taker_order_id(8) + maker_side(1) + timestamp(8) + sequence(8)
    assert_eq!(buf.len(), 73);
    let payload_len = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    assert_eq!(payload_len, 59);
    // Verify field values at known offsets (payload starts at byte 14)
    let trade_id = u64::from_le_bytes(buf[14..22].try_into().unwrap());
    assert_eq!(trade_id, 1);
    let symbol = u16::from_le_bytes(buf[22..24].try_into().unwrap());
    assert_eq!(symbol, 1);
    let price = i64::from_le_bytes(buf[24..32].try_into().unwrap());
    assert_eq!(price, FixedPrice::from_f64(30_000.0).raw());
    let qty = u64::from_le_bytes(buf[32..40].try_into().unwrap());
    assert_eq!(qty, FixedQty::from_f64(0.5).raw());
    let maker_order_id = u64::from_le_bytes(buf[40..48].try_into().unwrap());
    assert_eq!(maker_order_id, 10);
    let taker_order_id = u64::from_le_bytes(buf[48..56].try_into().unwrap());
    assert_eq!(taker_order_id, 20);
    assert_eq!(buf[56], Side::Buy as u8);
    let timestamp = u64::from_le_bytes(buf[57..65].try_into().unwrap());
    assert_eq!(timestamp, 99999);
    let sequence = u64::from_le_bytes(buf[65..73].try_into().unwrap());
    assert_eq!(sequence, 1);
}

#[test]
fn test_encode_order_filled() {
    let event = MatchEvent::OrderFilled {
        order_id: 55,
        filled_qty: FixedQty::from_f64(1.0),
        price: FixedPrice::from_f64(30_000.0),
        timestamp: 11111,
    };
    let mut buf = BytesMut::new();
    encode_events(&[event], 1, &mut buf);
    // Header (14) + payload (32): order_id(8) + filled_qty(8) + price(8) + timestamp(8)
    assert_eq!(buf.len(), 46);
    let order_id = u64::from_le_bytes(buf[14..22].try_into().unwrap());
    assert_eq!(order_id, 55);
    let filled_qty = u64::from_le_bytes(buf[22..30].try_into().unwrap());
    assert_eq!(filled_qty, FixedQty::from_f64(1.0).raw());
    let price = i64::from_le_bytes(buf[30..38].try_into().unwrap());
    assert_eq!(price, FixedPrice::from_f64(30_000.0).raw());
    let timestamp = u64::from_le_bytes(buf[38..46].try_into().unwrap());
    assert_eq!(timestamp, 11111);
}

#[test]
fn test_encode_stop_triggered() {
    let event = MatchEvent::StopTriggered {
        order_id: 88,
        trigger_price: FixedPrice::from_f64(28_500.0),
        timestamp: 22222,
    };
    let mut buf = BytesMut::new();
    encode_events(&[event], 1, &mut buf);
    // Header (14) + payload (24): order_id(8) + trigger_price(8) + timestamp(8)
    assert_eq!(buf.len(), 38);
    let order_id = u64::from_le_bytes(buf[14..22].try_into().unwrap());
    assert_eq!(order_id, 88);
    let trigger_price = i64::from_le_bytes(buf[22..30].try_into().unwrap());
    assert_eq!(trigger_price, FixedPrice::from_f64(28_500.0).raw());
    let timestamp = u64::from_le_bytes(buf[30..38].try_into().unwrap());
    assert_eq!(timestamp, 22222);
}

#[test]
fn test_encode_multiple_events() {
    let events = vec![
        MatchEvent::OrderAccepted { order_id: 1, timestamp: 100 },
        MatchEvent::OrderFilled {
            order_id: 1,
            filled_qty: FixedQty::from_f64(1.0),
            price: FixedPrice::from_f64(100.0),
            timestamp: 200,
        },
    ];
    let mut buf = BytesMut::new();
    encode_events(&events, 1, &mut buf);
    // OrderAccepted: 14 + 16 = 30; OrderFilled: 14 + 32 = 46
    assert_eq!(buf.len(), 76);
}

#[test]
fn test_encode_order_rejected() {
    let event = MatchEvent::OrderRejected {
        order_id: 42,
        reason: RejectReason::NoLiquidity,
        timestamp: 55555,
    };
    let mut buf = BytesMut::new();
    encode_events(&[event], 1, &mut buf);
    // Header (14) + payload (17): order_id(8) + reason(1) + timestamp(8)
    assert_eq!(buf.len(), 31);
    let payload_len = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    assert_eq!(payload_len, 17);
    let order_id = u64::from_le_bytes(buf[14..22].try_into().unwrap());
    assert_eq!(order_id, 42);
    assert_eq!(buf[22], 0); // NoLiquidity = 0
    let timestamp = u64::from_le_bytes(buf[23..31].try_into().unwrap());
    assert_eq!(timestamp, 55555);
}

#[test]
fn test_encode_order_partially_filled() {
    let event = MatchEvent::OrderPartiallyFilled {
        order_id: 33,
        filled_qty: FixedQty::from_f64(0.5),
        remaining_qty: FixedQty::from_f64(0.5),
        price: FixedPrice::from_f64(50_000.0),
        timestamp: 77777,
    };
    let mut buf = BytesMut::new();
    encode_events(&[event], 1, &mut buf);
    // Header (14) + payload (40): order_id(8) + filled_qty(8) + remaining_qty(8) + price(8) + timestamp(8)
    assert_eq!(buf.len(), 54);
    let payload_len = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    assert_eq!(payload_len, 40);
    let order_id = u64::from_le_bytes(buf[14..22].try_into().unwrap());
    assert_eq!(order_id, 33);
    let filled_qty = u64::from_le_bytes(buf[22..30].try_into().unwrap());
    assert_eq!(filled_qty, FixedQty::from_f64(0.5).raw());
    let remaining_qty = u64::from_le_bytes(buf[30..38].try_into().unwrap());
    assert_eq!(remaining_qty, FixedQty::from_f64(0.5).raw());
    let price = i64::from_le_bytes(buf[38..46].try_into().unwrap());
    assert_eq!(price, FixedPrice::from_f64(50_000.0).raw());
    let timestamp = u64::from_le_bytes(buf[46..54].try_into().unwrap());
    assert_eq!(timestamp, 77777);
}

#[test]
fn test_encode_order_cancelled() {
    let event = MatchEvent::OrderCancelled {
        order_id: 99,
        remaining_qty: FixedQty::from_f64(2.0),
        timestamp: 33333,
    };
    let mut buf = BytesMut::new();
    encode_events(&[event], 1, &mut buf);
    // Header (14) + payload (24): order_id(8) + remaining_qty(8) + timestamp(8)
    assert_eq!(buf.len(), 38);
    let payload_len = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    assert_eq!(payload_len, 24);
    let order_id = u64::from_le_bytes(buf[14..22].try_into().unwrap());
    assert_eq!(order_id, 99);
    let remaining_qty = u64::from_le_bytes(buf[22..30].try_into().unwrap());
    assert_eq!(remaining_qty, FixedQty::from_f64(2.0).raw());
    let timestamp = u64::from_le_bytes(buf[30..38].try_into().unwrap());
    assert_eq!(timestamp, 33333);
}

#[test]
fn test_encode_order_amended() {
    let event = MatchEvent::OrderAmended {
        order_id: 11,
        new_price: FixedPrice::from_f64(45_000.0),
        new_qty: FixedQty::from_f64(0.25),
        timestamp: 44444,
    };
    let mut buf = BytesMut::new();
    encode_events(&[event], 1, &mut buf);
    // Header (14) + payload (32): order_id(8) + new_price(8) + new_qty(8) + timestamp(8)
    assert_eq!(buf.len(), 46);
    let payload_len = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    assert_eq!(payload_len, 32);
    let order_id = u64::from_le_bytes(buf[14..22].try_into().unwrap());
    assert_eq!(order_id, 11);
    let new_price = i64::from_le_bytes(buf[22..30].try_into().unwrap());
    assert_eq!(new_price, FixedPrice::from_f64(45_000.0).raw());
    let new_qty = u64::from_le_bytes(buf[30..38].try_into().unwrap());
    assert_eq!(new_qty, FixedQty::from_f64(0.25).raw());
    let timestamp = u64::from_le_bytes(buf[38..46].try_into().unwrap());
    assert_eq!(timestamp, 44444);
}

// ─── Error frame encoding ─────────────────────────────────────────────────────

#[test]
fn test_encode_error_normal() {
    let mut buf = BytesMut::new();
    encode_error(3, ErrorCode::SequenceOutOfOrder, "seq out of order", &mut buf);
    // Header (14) + payload: code(2) + msg_len(2) + msg(16) = 20
    assert_eq!(buf.len(), 34);
    let payload_len = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    assert_eq!(payload_len, 20);
    let seq = u64::from_le_bytes(buf[6..14].try_into().unwrap());
    assert_eq!(seq, 3);
    let code = u16::from_le_bytes(buf[14..16].try_into().unwrap());
    assert_eq!(code, ErrorCode::SequenceOutOfOrder as u16);
    let msg_len = u16::from_le_bytes(buf[16..18].try_into().unwrap());
    assert_eq!(msg_len, 16);
    assert_eq!(&buf[18..34], b"seq out of order");
}

#[test]
fn test_encode_error_empty_message() {
    let mut buf = BytesMut::new();
    encode_error(0, ErrorCode::HandshakeRequired, "", &mut buf);
    // Header (14) + payload: code(2) + msg_len(2) = 4
    assert_eq!(buf.len(), 18);
    let msg_len = u16::from_le_bytes(buf[16..18].try_into().unwrap());
    assert_eq!(msg_len, 0);
}

#[test]
fn test_encode_error_long_message_truncated() {
    let long_msg = "x".repeat(300);
    let mut buf = BytesMut::new();
    encode_error(0, ErrorCode::InvalidPayload, &long_msg, &mut buf);
    // Message must be truncated to 255 bytes
    let msg_len = u16::from_le_bytes(buf[16..18].try_into().unwrap());
    assert_eq!(msg_len, 255);
    // Total: header(14) + code(2) + msg_len(2) + 255 = 273
    assert_eq!(buf.len(), 273);
}

// ─── Pong encoding ───────────────────────────────────────────────────────────

#[test]
fn test_encode_pong() {
    let mut buf = BytesMut::new();
    encode_pong(5, &mut buf);
    // Header only, payload len = 0
    assert_eq!(buf.len(), 14);
    let payload_len = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    assert_eq!(payload_len, 0);
    let seq = u64::from_le_bytes(buf[6..14].try_into().unwrap());
    assert_eq!(seq, 5);
}

// ─── Boundary cases ──────────────────────────────────────────────────────────

#[test]
fn test_decode_empty_buffer() {
    let mut conn = ConnectionCodecState::new();
    let result = decode_frame(&[], &mut conn);
    assert!(matches!(result, Err(DecodeError::Incomplete)));
}

#[test]
fn test_decode_partial_header() {
    let mut conn = ConnectionCodecState::new();
    // Only 5 bytes — less than FRAME_HEADER_SIZE (14)
    let partial = vec![0u8; 5];
    let result = decode_frame(&partial, &mut conn);
    assert!(matches!(result, Err(DecodeError::Incomplete)));
}

#[test]
fn test_decode_incomplete_payload() {
    let mut conn = ConnectionCodecState::new();
    // Header says payload is 37 bytes but we only have 10
    let mut frame = make_frame(0x01, 1, &[0u8; 37]);
    frame.truncate(14 + 10); // cut payload short
    // Also update the len field to still claim 37 bytes
    frame[0..4].copy_from_slice(&37u32.to_le_bytes());
    let result = decode_frame(&frame, &mut conn);
    assert!(matches!(result, Err(DecodeError::Incomplete)));
}

#[test]
fn test_decode_payload_too_large() {
    let mut conn = ConnectionCodecState::new();
    // Craft a header claiming payload > MAX_PAYLOAD_SIZE
    let too_large = MAX_PAYLOAD_SIZE + 1;
    let mut buf = vec![0u8; FRAME_HEADER_SIZE];
    buf[0..4].copy_from_slice(&too_large.to_le_bytes());
    buf[4] = PROTOCOL_VERSION;
    buf[5] = 0x01; // PlaceOrder
    let result = decode_frame(&buf, &mut conn);
    assert!(matches!(result, Err(DecodeError::PayloadTooLarge(_))));
}

#[test]
fn test_decode_invalid_message_type() {
    let mut conn = ConnectionCodecState::new();
    // Use unknown message type 0xFF
    let frame = make_frame(0xFF, 0, &[]);
    let result = decode_frame(&frame, &mut conn);
    assert!(matches!(result, Err(DecodeError::InvalidMessageType(0xFF))));
}

// ─── Protocol violations ─────────────────────────────────────────────────────

#[test]
fn test_command_without_handshake() {
    let mut conn = ConnectionCodecState::new();
    // State is HandshakePending; sending a PlaceOrder must fail
    let price_raw = FixedPrice::from_f64(100.0).raw();
    let qty_raw = FixedQty::from_f64(1.0).raw();
    let payload = place_order_payload(1, 1, 0, 0, 0, price_raw, 0, qty_raw);
    let frame = make_frame(0x01, 1, &payload);
    let result = decode_frame(&frame, &mut conn);
    assert!(matches!(result, Err(DecodeError::HandshakeRequired)));
}

#[test]
fn test_wrong_protocol_version() {
    let mut conn = ConnectionCodecState::new();
    let frame = make_frame_with_version(0x05, 0, 99, &[99]);
    let result = decode_frame(&frame, &mut conn);
    assert!(matches!(result, Err(DecodeError::UnsupportedVersion(99))));
}

#[test]
fn test_outbound_type_sent_by_client_is_rejected() {
    let mut conn = active_conn();
    // Client sends 0x10 (OrderAccepted) — outbound-only type
    let frame = make_frame(0x10, 2, &[0u8; 16]);
    let result = decode_frame(&frame, &mut conn);
    assert!(matches!(result, Err(DecodeError::InvalidPayload(_))));
}

// ─── Sequence number validation ──────────────────────────────────────────────

#[test]
fn test_sequence_strictly_increasing() {
    let mut conn = active_conn();
    let price_raw = FixedPrice::from_f64(100.0).raw();
    let qty_raw = FixedQty::from_f64(1.0).raw();

    for seq in 2u64..=5 {
        let payload = place_order_payload(seq, 1, 0, 0, 0, price_raw, 0, qty_raw);
        let frame = make_frame(0x01, seq, &payload);
        decode_frame(&frame, &mut conn).expect("sequential seqs must succeed");
    }
}

#[test]
fn test_sequence_out_of_order_rejected() {
    let mut conn = active_conn();
    let price_raw = FixedPrice::from_f64(100.0).raw();
    let qty_raw = FixedQty::from_f64(1.0).raw();

    // First command seq=5
    let payload = place_order_payload(1, 1, 0, 0, 0, price_raw, 0, qty_raw);
    let frame = make_frame(0x01, 5, &payload);
    decode_frame(&frame, &mut conn).expect("seq 5 ok");

    // Replay with seq=3 (out of order)
    let payload2 = place_order_payload(2, 1, 0, 0, 0, price_raw, 0, qty_raw);
    let frame2 = make_frame(0x01, 3, &payload2);
    let result = decode_frame(&frame2, &mut conn);
    assert!(matches!(
        result,
        Err(DecodeError::SequenceOutOfOrder { expected_min: 6, got: 3 })
    ));
}

#[test]
fn test_sequence_duplicate_rejected() {
    let mut conn = active_conn();
    let price_raw = FixedPrice::from_f64(100.0).raw();
    let qty_raw = FixedQty::from_f64(1.0).raw();

    let payload = place_order_payload(1, 1, 0, 0, 0, price_raw, 0, qty_raw);
    let frame = make_frame(0x01, 2, &payload);
    decode_frame(&frame, &mut conn).expect("first ok");

    // Same seq again = out of order
    let payload2 = place_order_payload(2, 1, 0, 0, 0, price_raw, 0, qty_raw);
    let frame2 = make_frame(0x01, 2, &payload2);
    let result = decode_frame(&frame2, &mut conn);
    assert!(matches!(result, Err(DecodeError::SequenceOutOfOrder { .. })));
}

// ─── Payload validation ──────────────────────────────────────────────────────

#[test]
fn test_place_order_zero_id_rejected() {
    let mut conn = active_conn();
    let price_raw = FixedPrice::from_f64(100.0).raw();
    let qty_raw = FixedQty::from_f64(1.0).raw();
    let payload = place_order_payload(0, 1, 0, 0, 0, price_raw, 0, qty_raw); // order_id=0
    let frame = make_frame(0x01, 2, &payload);
    let result = decode_frame(&frame, &mut conn);
    assert!(matches!(result, Err(DecodeError::InvalidPayload(_))));
}

#[test]
fn test_place_order_zero_qty_rejected() {
    let mut conn = active_conn();
    let price_raw = FixedPrice::from_f64(100.0).raw();
    let payload = place_order_payload(1, 1, 0, 0, 0, price_raw, 0, 0); // qty=0
    let frame = make_frame(0x01, 2, &payload);
    let result = decode_frame(&frame, &mut conn);
    assert!(matches!(result, Err(DecodeError::InvalidPayload(_))));
}

#[test]
fn test_place_order_limit_zero_price_rejected() {
    let mut conn = active_conn();
    let qty_raw = FixedQty::from_f64(1.0).raw();
    let payload = place_order_payload(1, 1, 0, 0, 0, 0, 0, qty_raw); // price=0 for limit
    let frame = make_frame(0x01, 2, &payload);
    let result = decode_frame(&frame, &mut conn);
    assert!(matches!(result, Err(DecodeError::InvalidPayload(_))));
}

#[test]
fn test_place_order_invalid_side_rejected() {
    let mut conn = active_conn();
    let price_raw = FixedPrice::from_f64(100.0).raw();
    let qty_raw = FixedQty::from_f64(1.0).raw();
    let payload = place_order_payload(1, 1, 99, 0, 0, price_raw, 0, qty_raw); // side=99
    let frame = make_frame(0x01, 2, &payload);
    let result = decode_frame(&frame, &mut conn);
    assert!(matches!(result, Err(DecodeError::InvalidPayload(_))));
}

#[test]
fn test_place_order_invalid_order_type_rejected() {
    let mut conn = active_conn();
    let price_raw = FixedPrice::from_f64(100.0).raw();
    let qty_raw = FixedQty::from_f64(1.0).raw();
    let payload = place_order_payload(1, 1, 0, 99, 0, price_raw, 0, qty_raw); // type=99
    let frame = make_frame(0x01, 2, &payload);
    let result = decode_frame(&frame, &mut conn);
    assert!(matches!(result, Err(DecodeError::InvalidPayload(_))));
}

#[test]
fn test_cancel_order_zero_id_rejected() {
    let mut conn = active_conn();
    let payload = cancel_order_payload(0, 1); // order_id=0
    let frame = make_frame(0x02, 2, &payload);
    let result = decode_frame(&frame, &mut conn);
    assert!(matches!(result, Err(DecodeError::InvalidPayload(_))));
}

#[test]
fn test_amend_order_no_fields_rejected() {
    let mut conn = active_conn();
    let payload = amend_order_payload(42, 1, None, None);
    let frame = make_frame(0x03, 2, &payload);
    let result = decode_frame(&frame, &mut conn);
    assert!(matches!(result, Err(DecodeError::InvalidPayload(_))));
}

// ─── Ping handling ───────────────────────────────────────────────────────────

#[test]
fn test_ping_before_handshake_allowed() {
    let mut conn = ConnectionCodecState::new();
    // Ping should be allowed even in HandshakePending state
    let frame = make_frame(0x04, 1, &[]);
    let (decoded, _) = decode_frame(&frame, &mut conn).expect("ping allowed");
    assert!(matches!(decoded, DecodedFrame::Ping(1)));
}

#[test]
fn test_ping_after_handshake_allowed() {
    let mut conn = active_conn();
    let frame = make_frame(0x04, 2, &[]);
    let (decoded, _) = decode_frame(&frame, &mut conn).expect("ping after handshake");
    assert!(matches!(decoded, DecodedFrame::Ping(2)));
}

// ─── Stop order validation ───────────────────────────────────────────────────

#[test]
fn test_stop_limit_zero_stop_price_rejected() {
    let mut conn = active_conn();
    let price_raw = FixedPrice::from_f64(100.0).raw();
    let qty_raw = FixedQty::from_f64(1.0).raw();
    // StopLimit with stop_price=0 should be rejected
    let payload = place_order_payload(1, 1, 0, 2, 0, price_raw, 0, qty_raw);
    let frame = make_frame(0x01, 2, &payload);
    let result = decode_frame(&frame, &mut conn);
    assert!(matches!(result, Err(DecodeError::InvalidPayload(_))));
}

#[test]
fn test_stop_market_zero_stop_price_rejected() {
    let mut conn = active_conn();
    let qty_raw = FixedQty::from_f64(1.0).raw();
    // StopMarket with stop_price=0 should be rejected
    let payload = place_order_payload(1, 1, 0, 3, 0, 0, 0, qty_raw);
    let frame = make_frame(0x01, 2, &payload);
    let result = decode_frame(&frame, &mut conn);
    assert!(matches!(result, Err(DecodeError::InvalidPayload(_))));
}

// ─── Bytes consumed ──────────────────────────────────────────────────────────

#[test]
fn test_consumed_bytes_exact() {
    let mut conn = active_conn();
    let price_raw = FixedPrice::from_f64(100.0).raw();
    let qty_raw = FixedQty::from_f64(1.0).raw();
    let payload = place_order_payload(1, 1, 0, 0, 0, price_raw, 0, qty_raw);
    let frame = make_frame(0x01, 2, &payload);
    let frame_len = frame.len();
    // Append extra junk to simulate pipelined frames
    let mut buf = frame;
    buf.extend_from_slice(&[0xFF; 20]);
    let (_, consumed) = decode_frame(&buf, &mut conn).expect("decode");
    assert_eq!(consumed, frame_len);
    // Should NOT consume the trailing junk
    assert!(consumed < buf.len());
}
