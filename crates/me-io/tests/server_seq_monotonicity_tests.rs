/// CRITICAL invariant tests for Group A (server_seq architecture).
///
/// These tests verify that every outbound wire frame has a distinct,
/// monotonically-increasing `server_seq` in its header, and that the
/// replay buffer uses the SAME sequence space as the wire protocol.
///
/// Without these invariants, ACK/resume cannot work correctly.
use bytes::BytesMut;
use me_core::events::MatchEvent;
use me_core::order::OrderStatus;
use me_core::types::{FixedPrice, FixedQty, Side, SymbolId};
use me_io::codec::encode_events;
use me_io::protocol::FRAME_HEADER_SIZE;

/// Read server_seq (header offset 6..14, u64 LE) from a frame at `start`.
fn read_header_seq(buf: &[u8], start: usize) -> u64 {
    u64::from_le_bytes(buf[start + 6..start + 14].try_into().unwrap())
}

/// Read payload length (header offset 0..4, u32 LE) from a frame at `start`.
fn read_header_len(buf: &[u8], start: usize) -> usize {
    u32::from_le_bytes(buf[start..start + 4].try_into().unwrap()) as usize
}

/// Iterate wire frames in a buffer. Returns Vec of (frame_start_offset, header_seq).
fn iterate_frames(buf: &[u8]) -> Vec<(usize, u64)> {
    let mut result = Vec::new();
    let mut off = 0;
    while off + FRAME_HEADER_SIZE <= buf.len() {
        let seq = read_header_seq(buf, off);
        let payload_len = read_header_len(buf, off);
        result.push((off, seq));
        off += FRAME_HEADER_SIZE + payload_len;
    }
    result
}

#[test]
fn test_multi_event_response_each_frame_has_distinct_server_seq() {
    // When server responds to ONE command with MULTIPLE events (e.g.
    // OrderAccepted + Trade + OrderFilled), each wire frame MUST carry a
    // distinct, monotonically-increasing server_seq in its header.
    //
    // Otherwise the client cannot ACK/resume at frame granularity.
    let events = vec![
        MatchEvent::OrderAccepted {
            order_id: 1,
            timestamp: 1000,
        },
        MatchEvent::Trade(me_core::trade::Trade {
            trade_id: 1,
            symbol: SymbolId(0),
            price: FixedPrice::from_raw(100_000),
            qty: FixedQty::from_raw(1_000),
            maker_order_id: 2,
            taker_order_id: 1,
            maker_side: Side::Sell,
            timestamp: 1000,
            sequence: 0,
        }),
        MatchEvent::OrderFilled {
            order_id: 1,
            filled_qty: FixedQty::from_raw(1_000),
            price: FixedPrice::from_raw(100_000),
            timestamp: 1000,
        },
    ];

    let mut buf = BytesMut::new();
    // Server assigns sequential seqs starting at 100. request_id=42 mirrored.
    let starting_seq = 100u64;
    encode_events(&events, starting_seq, 42, &mut buf);

    let frames = iterate_frames(&buf);
    assert_eq!(frames.len(), 3, "expected 3 wire frames");

    // Each frame should carry its own monotonic seq.
    assert_eq!(
        frames[0].1, 100,
        "first frame should have seq=starting_seq"
    );
    assert_eq!(
        frames[1].1, 101,
        "second frame should have seq=starting_seq+1"
    );
    assert_eq!(
        frames[2].1, 102,
        "third frame should have seq=starting_seq+2"
    );
}

#[test]
fn test_single_event_response_uses_starting_seq() {
    // Degenerate case: single event => single frame => seq == starting_seq.
    let events = vec![MatchEvent::OrderAccepted {
        order_id: 1,
        timestamp: 1000,
    }];

    let mut buf = BytesMut::new();
    encode_events(&events, 50, 7, &mut buf);

    let frames = iterate_frames(&buf);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].1, 50);
}

#[test]
fn test_empty_events_produces_no_frames() {
    let events: Vec<MatchEvent> = vec![];
    let mut buf = BytesMut::new();
    encode_events(&events, 42, 1, &mut buf);
    assert!(buf.is_empty());
}

#[test]
fn test_server_seq_consumed_count_equals_event_count() {
    // encode_events must tell caller how many seqs were consumed so the
    // caller can advance their server_frame_seq counter correctly.
    //
    // This test currently uses a workaround (counting frames in buf);
    // after fix, encode_events should RETURN the count.
    let events = vec![
        MatchEvent::OrderCancelled {
            order_id: 1,
            remaining_qty: FixedQty::from_raw(0),
            timestamp: 1000,
        },
        MatchEvent::OrderCancelled {
            order_id: 2,
            remaining_qty: FixedQty::from_raw(0),
            timestamp: 1001,
        },
    ];
    let mut buf = BytesMut::new();
    encode_events(&events, 200, 99, &mut buf);

    let frames = iterate_frames(&buf);
    assert_eq!(frames.len(), 2);
    // Each frame consumes one seq slot
    assert_eq!(frames[1].1 - frames[0].1, 1);
}

#[test]
fn test_query_order_status_response_single_frame() {
    // QueryOrder returns one of: OrderStatus (1 frame) or OrderNotFound (1 frame).
    let events = vec![MatchEvent::OrderStatus {
        order_id: 42,
        status: OrderStatus::PartiallyFilled as u8,
        filled_qty: FixedQty::from_raw(500),
        remaining_qty: FixedQty::from_raw(500),
        price: FixedPrice::from_raw(100_000),
        timestamp: 5000,
    }];
    let mut buf = BytesMut::new();
    encode_events(&events, 77, 3, &mut buf);

    let frames = iterate_frames(&buf);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].1, 77);
}
