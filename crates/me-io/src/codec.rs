use bytes::{BufMut, BytesMut};

use me_core::events::MatchEvent;
use me_core::order::{Order, OrderType, TimeInForce};
use me_core::types::*;

use me_engine::command::Command;

use crate::protocol::{
    ConnectionState, ErrorCode, MessageType, FRAME_HEADER_SIZE, MAX_PAYLOAD_SIZE, PROTOCOL_VERSION,
};

// ─── Decode types ───────────────────────────────────────────────────────────

/// Decode error types.
#[derive(Debug)]
pub enum DecodeError {
    Incomplete,
    InvalidMessageType(u8),
    InvalidPayload(String),
    PayloadTooLarge(u32),
    UnsupportedVersion(u8),
    SequenceOutOfOrder { expected_min: u64, got: u64 },
    HandshakeRequired,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Incomplete => write!(f, "Incomplete frame"),
            Self::InvalidMessageType(t) => write!(f, "Invalid message type: 0x{t:02x}"),
            Self::InvalidPayload(msg) => write!(f, "Invalid payload: {msg}"),
            Self::PayloadTooLarge(sz) => write!(f, "Payload too large: {sz}"),
            Self::UnsupportedVersion(v) => write!(f, "Unsupported protocol version: {v}"),
            Self::SequenceOutOfOrder { expected_min, got } => {
                write!(f, "Sequence out of order: expected >= {expected_min}, got {got}")
            }
            Self::HandshakeRequired => write!(f, "Handshake required before sending commands"),
        }
    }
}

/// Result of decoding a single frame.
pub enum DecodedFrame {
    /// A matching-engine command (PlaceOrder, CancelOrder, AmendOrder).
    Command(Command, u64),
    /// Client sent a Ping; server should reply with Pong.
    Ping(u64),
    /// Client sent a Handshake; server should reply with HandshakeAck.
    Handshake { client_version: u8, seq: u64 },
}

/// Per-connection state used during decoding to enforce protocol rules.
pub struct ConnectionCodecState {
    pub state: ConnectionState,
    /// The last sequence number we accepted from the client.
    pub last_client_seq: u64,
}

impl ConnectionCodecState {
    pub fn new() -> Self {
        Self {
            state: ConnectionState::HandshakePending,
            last_client_seq: 0,
        }
    }
}

impl Default for ConnectionCodecState {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Decoding ───────────────────────────────────────────────────────────────

/// Try to decode a frame from a buffer, enforcing connection-level invariants.
/// Returns Ok((decoded_frame, bytes_consumed)) on success.
pub fn decode_frame(
    buf: &[u8],
    conn: &mut ConnectionCodecState,
) -> Result<(DecodedFrame, usize), DecodeError> {
    if buf.len() < FRAME_HEADER_SIZE {
        return Err(DecodeError::Incomplete);
    }

    let len = u32::from_le_bytes(
        buf[0..4]
            .try_into()
            .expect("frame len: buf checked >= FRAME_HEADER_SIZE"),
    );
    if len > MAX_PAYLOAD_SIZE {
        return Err(DecodeError::PayloadTooLarge(len));
    }
    let total_size = FRAME_HEADER_SIZE + len as usize;
    if buf.len() < total_size {
        return Err(DecodeError::Incomplete);
    }

    // Version byte
    let version = buf[4];
    if version != PROTOCOL_VERSION {
        return Err(DecodeError::UnsupportedVersion(version));
    }

    let msg_type_raw = buf[5];
    let seq = u64::from_le_bytes(
        buf[6..14]
            .try_into()
            .expect("frame seq: buf checked >= FRAME_HEADER_SIZE"),
    );
    let payload = &buf[FRAME_HEADER_SIZE..total_size];

    let msg_type = MessageType::from_u8(msg_type_raw)
        .ok_or(DecodeError::InvalidMessageType(msg_type_raw))?;

    // Sequence validation: must be strictly increasing.
    // seq == 0 is only valid for the very first Handshake frame.
    let is_first_handshake = seq == 0 && msg_type == MessageType::Handshake;
    if seq <= conn.last_client_seq && !is_first_handshake {
        return Err(DecodeError::SequenceOutOfOrder {
            expected_min: conn.last_client_seq + 1,
            got: seq,
        });
    }

    // State machine: handshake must come first.
    match msg_type {
        MessageType::Handshake => {
            // Handshake is always allowed (even for re-handshake).
            conn.last_client_seq = seq;
            let client_version = if payload.is_empty() {
                PROTOCOL_VERSION
            } else {
                payload[0]
            };
            return Ok((
                DecodedFrame::Handshake {
                    client_version,
                    seq,
                },
                total_size,
            ));
        }
        MessageType::Ping => {
            // Ping is allowed in any state after handshake, or even during
            // HandshakePending as a keepalive.
            conn.last_client_seq = seq;
            return Ok((DecodedFrame::Ping(seq), total_size));
        }
        _ if msg_type.is_inbound_command() => {
            if conn.state != ConnectionState::Active {
                return Err(DecodeError::HandshakeRequired);
            }
        }
        _ => {
            // Outbound message types from a client are invalid.
            return Err(DecodeError::InvalidPayload(
                "Client sent an outbound-only message type".into(),
            ));
        }
    }

    conn.last_client_seq = seq;

    let cmd = match msg_type {
        MessageType::PlaceOrder => decode_place_order(payload)?,
        MessageType::CancelOrder => decode_cancel_order(payload)?,
        MessageType::AmendOrder => decode_amend_order(payload)?,
        _ => unreachable!(), // handled above
    };

    Ok((DecodedFrame::Command(cmd, seq), total_size))
}

/// PlaceOrder payload (37 bytes):
/// OrderId(8) + SymbolId(2) + Side(1) + OrderType(1) + TIF(1) + Price(8) + StopPrice(8) + Qty(8)
fn decode_place_order(payload: &[u8]) -> Result<Command, DecodeError> {
    if payload.len() < 37 {
        return Err(DecodeError::InvalidPayload("PlaceOrder too short".into()));
    }
    // Safety: payload.len() >= 37 checked above.
    let order_id = u64::from_le_bytes(payload[0..8].try_into().expect("PlaceOrder: order_id"));
    if order_id == 0 {
        return Err(DecodeError::InvalidPayload(
            "PlaceOrder order_id must be non-zero".into(),
        ));
    }
    let symbol = SymbolId(u16::from_le_bytes(payload[8..10].try_into().expect("PlaceOrder: symbol")));
    let side = match payload[10] {
        0 => Side::Buy,
        1 => Side::Sell,
        v => return Err(DecodeError::InvalidPayload(format!("Invalid side: {v}"))),
    };
    let order_type = match payload[11] {
        0 => OrderType::Limit,
        1 => OrderType::Market,
        2 => OrderType::StopLimit,
        3 => OrderType::StopMarket,
        v => {
            return Err(DecodeError::InvalidPayload(format!(
                "Invalid order type: {v}"
            )))
        }
    };
    let tif = match payload[12] {
        0 => TimeInForce::GTC,
        1 => TimeInForce::IOC,
        2 => TimeInForce::FOK,
        v => return Err(DecodeError::InvalidPayload(format!("Invalid TIF: {v}"))),
    };
    let price = FixedPrice::from_raw(i64::from_le_bytes(payload[13..21].try_into().expect("PlaceOrder: price")));
    let stop_price = FixedPrice::from_raw(i64::from_le_bytes(payload[21..29].try_into().expect("PlaceOrder: stop_price")));
    let qty_raw = u64::from_le_bytes(payload[29..37].try_into().expect("PlaceOrder: qty"));
    if qty_raw == 0 {
        return Err(DecodeError::InvalidPayload(
            "PlaceOrder qty must be non-zero".into(),
        ));
    }
    let qty = FixedQty::from_raw(qty_raw);

    // Validate: limit orders need a positive price.
    match order_type {
        OrderType::Limit | OrderType::StopLimit => {
            if !price.is_positive() {
                return Err(DecodeError::InvalidPayload(
                    "Limit/StopLimit order requires positive price".into(),
                ));
            }
        }
        _ => {}
    }

    // Validate: stop orders need a positive stop_price.
    match order_type {
        OrderType::StopLimit | OrderType::StopMarket => {
            if !stop_price.is_positive() {
                return Err(DecodeError::InvalidPayload(
                    "Stop order requires positive stop_price".into(),
                ));
            }
        }
        _ => {}
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    let order = Order {
        id: order_id,
        price,
        stop_price,
        qty,
        remaining_qty: qty,
        cumulative_qty: FixedQty::ZERO,
        timestamp,
        sequence: 0,
        symbol,
        side,
        order_type,
        tif,
        status: me_core::order::OrderStatus::New,
        _pad: [0; 2],
    };

    Ok(Command::PlaceOrder(order))
}

/// CancelOrder payload (10 bytes): OrderId(8) + SymbolId(2)
fn decode_cancel_order(payload: &[u8]) -> Result<Command, DecodeError> {
    if payload.len() < 10 {
        return Err(DecodeError::InvalidPayload("CancelOrder too short".into()));
    }
    // Safety: payload.len() >= 10 checked above.
    let order_id = u64::from_le_bytes(payload[0..8].try_into().expect("CancelOrder: order_id"));
    if order_id == 0 {
        return Err(DecodeError::InvalidPayload(
            "CancelOrder order_id must be non-zero".into(),
        ));
    }
    let symbol = SymbolId(u16::from_le_bytes(payload[8..10].try_into().expect("CancelOrder: symbol")));

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    Ok(Command::CancelOrder {
        order_id,
        symbol,
        timestamp,
    })
}

/// AmendOrder payload (variable): OrderId(8) + SymbolId(2) + has_price(1) + [price(8)] + has_qty(1) + [qty(8)]
fn decode_amend_order(payload: &[u8]) -> Result<Command, DecodeError> {
    if payload.len() < 12 {
        return Err(DecodeError::InvalidPayload("AmendOrder too short".into()));
    }
    // Safety: payload.len() >= 12 checked above.
    let order_id = u64::from_le_bytes(payload[0..8].try_into().expect("AmendOrder: order_id"));
    if order_id == 0 {
        return Err(DecodeError::InvalidPayload(
            "AmendOrder order_id must be non-zero".into(),
        ));
    }
    let symbol = SymbolId(u16::from_le_bytes(payload[8..10].try_into().expect("AmendOrder: symbol")));

    let mut offset = 10;
    let new_price = if payload[offset] == 1 {
        offset += 1;
        if offset + 8 > payload.len() {
            return Err(DecodeError::InvalidPayload(
                "AmendOrder price truncated".into(),
            ));
        }
        let p = i64::from_le_bytes(payload[offset..offset + 8].try_into().expect("AmendOrder: new_price"));
        offset += 8;
        let price = FixedPrice::from_raw(p);
        if !price.is_positive() {
            return Err(DecodeError::InvalidPayload(
                "AmendOrder new_price must be positive".into(),
            ));
        }
        Some(price)
    } else {
        offset += 1;
        None
    };

    let new_qty = if offset < payload.len() && payload[offset] == 1 {
        offset += 1;
        if offset + 8 > payload.len() {
            return Err(DecodeError::InvalidPayload(
                "AmendOrder qty truncated".into(),
            ));
        }
        let q = u64::from_le_bytes(payload[offset..offset + 8].try_into().expect("AmendOrder: new_qty"));
        if q == 0 {
            return Err(DecodeError::InvalidPayload(
                "AmendOrder new_qty must be non-zero".into(),
            ));
        }
        Some(FixedQty::from_raw(q))
    } else {
        None
    };

    if new_price.is_none() && new_qty.is_none() {
        return Err(DecodeError::InvalidPayload(
            "AmendOrder must change at least one field".into(),
        ));
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    Ok(Command::AmendOrder {
        order_id,
        symbol,
        new_price,
        new_qty,
        timestamp,
    })
}

// ─── Encoding ────────────────────────────────────────────────────────────────

/// Encode a list of match events into a BytesMut buffer as frames.
pub fn encode_events(events: &[MatchEvent], seq: u64, buf: &mut BytesMut) {
    for event in events {
        encode_event(event, seq, buf);
    }
}

fn encode_event(event: &MatchEvent, seq: u64, buf: &mut BytesMut) {
    match event {
        MatchEvent::OrderAccepted {
            order_id,
            timestamp,
        } => {
            let payload_len = 16u32; // order_id(8) + timestamp(8)
            write_header(buf, payload_len, MessageType::OrderAccepted as u8, seq);
            buf.put_u64_le(*order_id);
            buf.put_u64_le(*timestamp);
        }
        MatchEvent::OrderRejected {
            order_id,
            reason,
            timestamp,
        } => {
            let payload_len = 17u32;
            write_header(buf, payload_len, MessageType::OrderRejected as u8, seq);
            buf.put_u64_le(*order_id);
            buf.put_u8(reject_reason_to_u8(reason));
            buf.put_u64_le(*timestamp);
        }
        MatchEvent::Trade(trade) => {
            let payload_len = 51u32;
            write_header(buf, payload_len, MessageType::Trade as u8, seq);
            buf.put_u64_le(trade.trade_id);
            buf.put_u16_le(trade.symbol.0);
            buf.put_i64_le(trade.price.raw());
            buf.put_u64_le(trade.qty.raw());
            buf.put_u64_le(trade.maker_order_id);
            buf.put_u64_le(trade.taker_order_id);
            buf.put_u8(trade.maker_side as u8);
            buf.put_u64_le(trade.timestamp);
        }
        MatchEvent::OrderFilled {
            order_id,
            filled_qty,
            price,
            timestamp,
        } => {
            let payload_len = 32u32;
            write_header(buf, payload_len, MessageType::OrderFilled as u8, seq);
            buf.put_u64_le(*order_id);
            buf.put_u64_le(filled_qty.raw());
            buf.put_i64_le(price.raw());
            buf.put_u64_le(*timestamp);
        }
        MatchEvent::OrderPartiallyFilled {
            order_id,
            filled_qty,
            remaining_qty,
            price,
            timestamp,
        } => {
            let payload_len = 40u32;
            write_header(
                buf,
                payload_len,
                MessageType::OrderPartiallyFilled as u8,
                seq,
            );
            buf.put_u64_le(*order_id);
            buf.put_u64_le(filled_qty.raw());
            buf.put_u64_le(remaining_qty.raw());
            buf.put_i64_le(price.raw());
            buf.put_u64_le(*timestamp);
        }
        MatchEvent::OrderCancelled {
            order_id,
            remaining_qty,
            timestamp,
        } => {
            let payload_len = 24u32;
            write_header(buf, payload_len, MessageType::OrderCancelled as u8, seq);
            buf.put_u64_le(*order_id);
            buf.put_u64_le(remaining_qty.raw());
            buf.put_u64_le(*timestamp);
        }
        MatchEvent::OrderAmended {
            order_id,
            new_price,
            new_qty,
            timestamp,
        } => {
            let payload_len = 32u32;
            write_header(buf, payload_len, MessageType::OrderAmended as u8, seq);
            buf.put_u64_le(*order_id);
            buf.put_i64_le(new_price.raw());
            buf.put_u64_le(new_qty.raw());
            buf.put_u64_le(*timestamp);
        }
        MatchEvent::StopTriggered {
            order_id,
            trigger_price,
            timestamp,
        } => {
            let payload_len = 24u32;
            write_header(buf, payload_len, MessageType::StopTriggered as u8, seq);
            buf.put_u64_le(*order_id);
            buf.put_i64_le(trigger_price.raw());
            buf.put_u64_le(*timestamp);
        }
    }
}

/// Encode a Pong response frame.
pub fn encode_pong(seq: u64, buf: &mut BytesMut) {
    write_header(buf, 0, MessageType::Pong as u8, seq);
}

/// Encode a HandshakeAck response frame.
/// Payload: server_version(1)
pub fn encode_handshake_ack(seq: u64, buf: &mut BytesMut) {
    write_header(buf, 1, MessageType::HandshakeAck as u8, seq);
    buf.put_u8(PROTOCOL_VERSION);
}

/// Encode a protocol-level Error frame.
/// Payload: error_code(2) + message_len(2) + message([u8])
pub fn encode_error(seq: u64, code: ErrorCode, message: &str, buf: &mut BytesMut) {
    let msg_bytes = message.as_bytes();
    let msg_len = msg_bytes.len().min(255) as u16;
    let payload_len = 2 + 2 + msg_len as u32;
    write_header(buf, payload_len, MessageType::Error as u8, seq);
    buf.put_u16_le(code as u16);
    buf.put_u16_le(msg_len);
    buf.put_slice(&msg_bytes[..msg_len as usize]);
}

fn write_header(buf: &mut BytesMut, payload_len: u32, msg_type: u8, seq: u64) {
    buf.put_u32_le(payload_len);
    buf.put_u8(PROTOCOL_VERSION);
    buf.put_u8(msg_type);
    buf.put_u64_le(seq);
}

fn reject_reason_to_u8(reason: &me_core::events::RejectReason) -> u8 {
    use me_core::events::RejectReason;
    match reason {
        RejectReason::NoLiquidity => 0,
        RejectReason::InsufficientLiquidity => 1,
        RejectReason::InvalidPrice => 2,
        RejectReason::InvalidQuantity => 3,
        RejectReason::InvalidLotSize => 4,
        RejectReason::InvalidTickSize => 5,
        RejectReason::OrderNotFound => 6,
        RejectReason::DuplicateOrderId => 7,
        RejectReason::SymbolNotFound => 8,
        RejectReason::TradingHalted => 9,
        RejectReason::RiskCheckFailed => 10,
    }
}
