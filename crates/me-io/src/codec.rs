use bytes::{BufMut, BytesMut};

use me_core::events::MatchEvent;
use me_core::order::{Order, OrderType, TimeInForce};
use me_core::types::*;

use me_engine::command::Command;

use crate::protocol::{MessageType, FRAME_HEADER_SIZE, MAX_PAYLOAD_SIZE};

/// Decode error types.
#[derive(Debug)]
pub enum DecodeError {
    Incomplete,
    InvalidMessageType(u8),
    InvalidPayload(String),
    PayloadTooLarge(u32),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Incomplete => write!(f, "Incomplete frame"),
            Self::InvalidMessageType(t) => write!(f, "Invalid message type: 0x{t:02x}"),
            Self::InvalidPayload(msg) => write!(f, "Invalid payload: {msg}"),
            Self::PayloadTooLarge(sz) => write!(f, "Payload too large: {sz}"),
        }
    }
}

/// Try to decode a Command from a frame buffer.
/// Returns Ok((command, seq, bytes_consumed)) on success.
pub fn decode_frame(buf: &[u8]) -> Result<(Command, u64, usize), DecodeError> {
    if buf.len() < FRAME_HEADER_SIZE {
        return Err(DecodeError::Incomplete);
    }

    let len = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    if len > MAX_PAYLOAD_SIZE {
        return Err(DecodeError::PayloadTooLarge(len));
    }
    let total_size = FRAME_HEADER_SIZE + len as usize;
    if buf.len() < total_size {
        return Err(DecodeError::Incomplete);
    }

    let msg_type = buf[4];
    let seq = u64::from_le_bytes(buf[5..13].try_into().unwrap());
    let payload = &buf[FRAME_HEADER_SIZE..total_size];

    let cmd = match MessageType::from_u8(msg_type) {
        Some(MessageType::PlaceOrder) => decode_place_order(payload, seq)?,
        Some(MessageType::CancelOrder) => decode_cancel_order(payload, seq)?,
        Some(MessageType::AmendOrder) => decode_amend_order(payload, seq)?,
        Some(_) => return Err(DecodeError::InvalidPayload("Not an inbound message".into())),
        None => return Err(DecodeError::InvalidMessageType(msg_type)),
    };

    Ok((cmd, seq, total_size))
}

/// PlaceOrder payload (37 bytes):
/// OrderId(8) + SymbolId(2) + Side(1) + OrderType(1) + TIF(1) + Price(8) + StopPrice(8) + Qty(8)
fn decode_place_order(payload: &[u8], _seq: u64) -> Result<Command, DecodeError> {
    if payload.len() < 37 {
        return Err(DecodeError::InvalidPayload("PlaceOrder too short".into()));
    }
    let order_id = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let symbol = SymbolId(u16::from_le_bytes(payload[8..10].try_into().unwrap()));
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
    let price = FixedPrice::from_raw(i64::from_le_bytes(payload[13..21].try_into().unwrap()));
    let stop_price = FixedPrice::from_raw(i64::from_le_bytes(payload[21..29].try_into().unwrap()));
    let qty = FixedQty::from_raw(u64::from_le_bytes(payload[29..37].try_into().unwrap()));

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
fn decode_cancel_order(payload: &[u8], _seq: u64) -> Result<Command, DecodeError> {
    if payload.len() < 10 {
        return Err(DecodeError::InvalidPayload("CancelOrder too short".into()));
    }
    let order_id = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let symbol = SymbolId(u16::from_le_bytes(payload[8..10].try_into().unwrap()));

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
fn decode_amend_order(payload: &[u8], _seq: u64) -> Result<Command, DecodeError> {
    if payload.len() < 12 {
        return Err(DecodeError::InvalidPayload("AmendOrder too short".into()));
    }
    let order_id = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let symbol = SymbolId(u16::from_le_bytes(payload[8..10].try_into().unwrap()));

    let mut offset = 10;
    let new_price = if payload[offset] == 1 {
        offset += 1;
        let p = i64::from_le_bytes(payload[offset..offset + 8].try_into().unwrap());
        offset += 8;
        Some(FixedPrice::from_raw(p))
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
        let q = u64::from_le_bytes(payload[offset..offset + 8].try_into().unwrap());
        Some(FixedQty::from_raw(q))
    } else {
        None
    };

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

fn write_header(buf: &mut BytesMut, payload_len: u32, msg_type: u8, seq: u64) {
    buf.put_u32_le(payload_len);
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
    }
}
