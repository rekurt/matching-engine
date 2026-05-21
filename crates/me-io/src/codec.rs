use bytes::{BufMut, Bytes, BytesMut};

use me_core::events::MatchEvent;
use me_core::order::{Order, OrderType, TimeInForce};
use me_core::types::*;

use me_engine::command::Command;

use crate::protocol::{
    ConnectionState, ErrorCode, MessageType, BATCH_PLACE_ENTRY_SIZE, FRAME_HEADER_SIZE,
    MAX_BATCH_CANCEL, MAX_BATCH_PLACE, MAX_PAYLOAD_SIZE, MAX_PAYLOAD_SIZE_BATCH, PROTOCOL_VERSION,
    TLV_CAPABILITIES, TLV_HEARTBEAT_MS, TLV_SESSION_ID,
};
use crate::session_store::SessionId;

// ─── Decode types ───────────────────────────────────────────────────────────

/// Decode error types.
#[derive(Debug)]
pub enum DecodeError {
    Incomplete,
    InvalidMessageType(u8),
    InvalidPayload(String),
    PayloadTooLarge(u32),
    UnsupportedVersion(u8),
    SequenceOutOfOrder {
        expected_min: u64,
        got: u64,
    },
    HandshakeRequired,
    /// Command frame had request_id = 0 (not allowed for dedup-tracked messages).
    InvalidRequestId,
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
                write!(
                    f,
                    "Sequence out of order: expected >= {expected_min}, got {got}"
                )
            }
            Self::HandshakeRequired => write!(f, "Handshake required before sending commands"),
            Self::InvalidRequestId => {
                write!(f, "request_id must be non-zero for this message type")
            }
        }
    }
}

/// Result of decoding a single frame. Each variant carries the `seq` and
/// `request_id` fields from the frame header:
/// - `seq`: per-connection monotonic sequence
/// - `request_id`: client-generated idempotency key (u64), non-zero for commands
pub enum DecodedFrame {
    /// A matching-engine command (PlaceOrder, CancelOrder, AmendOrder).
    Command(Command, u64, u64), // (cmd, seq, request_id)
    /// One or more commands from a batch frame. All batch entries share
    /// the same request_id (batch as a whole is deduplicated).
    Batch(Vec<Command>, u64, u64), // (cmds, seq, request_id)
    /// Client sent a Ping; server should reply with Pong.
    /// `ack_seq` is the client's cumulative ACK (last_processed_server_seq).
    Ping { seq: u64, ack_seq: u64 },
    /// Client sent a Handshake; server should reply with HandshakeAck.
    Handshake { client_version: u8, seq: u64 },
    /// Client is reconnecting and wants to resume a previous session.
    ResumeSession {
        session_id: SessionId,
        last_recv_seq: u64,
        seq: u64,
    },
    /// Client is querying the status of a resting order.
    QueryOrder {
        order_id: u64,
        symbol_id: u16,
        seq: u64,
        request_id: u64,
    },
    /// Client cumulative ACK — purge replay buffer up to `last_processed_server_seq`.
    Acknowledge {
        last_processed_server_seq: u64,
        seq: u64,
    },
    /// Client requests a snapshot of its open orders (for resync after ReplayGap).
    GetOpenOrders { seq: u64, request_id: u64 },
}

/// Per-connection state used during decoding and session management.
pub struct ConnectionCodecState {
    pub state: ConnectionState,
    /// The last sequence number we accepted from the client.
    pub last_client_seq: u64,
    /// Session ID assigned to this connection at handshake.
    pub session_id: SessionId,
    /// Monotonic seq counter for outbound frames in the REPLAY PIPELINE.
    /// Wraps `ServerFrameSeq` to make seq=0 structurally unrepresentable.
    ///
    /// Out-of-band error frames (ServerShuttingDown, HeartbeatTimeout,
    /// Backpressure, InvalidPayload) DO use seq=0 — they're deliberately
    /// outside the ordered stream and are never pushed to the replay buffer.
    pub server_frame_seq: crate::session_store::ServerFrameSeq,
}

impl ConnectionCodecState {
    pub fn new() -> Self {
        Self {
            state: ConnectionState::HandshakePending,
            last_client_seq: 0,
            session_id: SessionId::NONE,
            server_frame_seq: crate::session_store::ServerFrameSeq::FIRST,
        }
    }
}

impl Default for ConnectionCodecState {
    fn default() -> Self {
        Self::new()
    }
}

// ─── TLV helpers ─────────────────────────────────────────────────────────────

/// Write a TLV record: tag(2 LE) + len(2 LE) + value.
pub fn write_tlv(buf: &mut BytesMut, tag: u16, value: &[u8]) {
    buf.put_u16_le(tag);
    buf.put_u16_le(value.len() as u16);
    buf.put_slice(value);
}

/// TLV decoding error — surfaced by the `tlv_iter` iterator so callers can
/// distinguish "end of stream" from "malformed/truncated input".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlvError {
    /// Data ends mid-header (fewer than 4 bytes left for tag+len).
    TruncatedHeader { remaining: usize },
    /// Data ends before the declared value length is satisfied.
    TruncatedValue {
        tag: u16,
        expected: usize,
        actual: usize,
    },
}

/// Iterate over TLV records in a byte slice, yielding `Result<(tag, value)>`.
///
/// A clean end of stream yields `None`. A malformed/truncated buffer yields
/// `Some(Err(TlvError::...))` ONCE and then stops. Callers who only care
/// about well-known tags can `.filter_map(Result::ok)`.
pub fn tlv_iter(data: &[u8]) -> impl Iterator<Item = Result<(u16, &[u8]), TlvError>> {
    TlvIter { data }
}

struct TlvIter<'a> {
    data: &'a [u8],
}

impl<'a> Iterator for TlvIter<'a> {
    type Item = Result<(u16, &'a [u8]), TlvError>;

    fn next(&mut self) -> Option<Self::Item> {
        // Clean end of stream.
        if self.data.is_empty() {
            return None;
        }
        // Partial header — corruption, not clean end.
        if self.data.len() < 4 {
            let err = TlvError::TruncatedHeader {
                remaining: self.data.len(),
            };
            self.data = &[];
            return Some(Err(err));
        }
        let tag = u16::from_le_bytes([self.data[0], self.data[1]]);
        let len = u16::from_le_bytes([self.data[2], self.data[3]]) as usize;
        self.data = &self.data[4..];
        if self.data.len() < len {
            let err = TlvError::TruncatedValue {
                tag,
                expected: len,
                actual: self.data.len(),
            };
            self.data = &[];
            return Some(Err(err));
        }
        let value = &self.data[..len];
        self.data = &self.data[len..];
        Some(Ok((tag, value)))
    }
}

// ─── Decoding ───────────────────────────────────────────────────────────────

/// Try to decode a frame from a buffer, enforcing connection-level invariants.
/// Returns Ok((decoded_frame, bytes_consumed)) on success.
pub fn decode_frame(
    buf: &[u8],
    conn: &mut ConnectionCodecState,
    clock: &dyn me_clock::Clock,
) -> Result<(DecodedFrame, usize), DecodeError> {
    if buf.len() < FRAME_HEADER_SIZE {
        return Err(DecodeError::Incomplete);
    }

    let len = u32::from_le_bytes(
        buf[0..4]
            .try_into()
            .expect("frame len: buf checked >= FRAME_HEADER_SIZE"),
    );

    let msg_type_raw = buf[5];

    // Per-type payload size limit (batch frames are larger).
    let max_payload = if msg_type_raw == MessageType::BatchPlaceOrder as u8
        || msg_type_raw == MessageType::BatchCancelOrder as u8
    {
        MAX_PAYLOAD_SIZE_BATCH
    } else {
        MAX_PAYLOAD_SIZE
    };
    if len > max_payload {
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

    let seq = u64::from_le_bytes(
        buf[6..14]
            .try_into()
            .expect("frame seq: buf checked >= FRAME_HEADER_SIZE"),
    );
    let request_id = u64::from_le_bytes(
        buf[14..22]
            .try_into()
            .expect("frame request_id: buf checked >= FRAME_HEADER_SIZE"),
    );
    let payload = &buf[FRAME_HEADER_SIZE..total_size];

    let msg_type =
        MessageType::from_u8(msg_type_raw).ok_or(DecodeError::InvalidMessageType(msg_type_raw))?;

    // request_id validation: inbound commands MUST provide non-zero request_id for dedup.
    if msg_type.requires_request_id() && request_id == 0 {
        return Err(DecodeError::InvalidRequestId);
    }

    // Sequence validation: must be strictly increasing.
    // seq == 0 is only valid for the very first Handshake frame (while still in HandshakePending).
    let is_first_handshake = seq == 0
        && msg_type == MessageType::Handshake
        && conn.state == ConnectionState::HandshakePending;
    if seq <= conn.last_client_seq && !is_first_handshake {
        return Err(DecodeError::SequenceOutOfOrder {
            expected_min: conn.last_client_seq + 1,
            got: seq,
        });
    }

    // State machine dispatch.
    match msg_type {
        MessageType::Handshake => {
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

        MessageType::ResumeSession => {
            // Allowed from HandshakePending (reconnect path).
            if payload.len() < 24 {
                return Err(DecodeError::InvalidPayload(
                    "ResumeSession too short".into(),
                ));
            }
            conn.last_client_seq = seq;
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&payload[0..16]);
            let session_id = SessionId::from_bytes(bytes);
            let last_recv_seq = u64::from_le_bytes(
                payload[16..24]
                    .try_into()
                    .expect("ResumeSession: last_recv_seq"),
            );
            return Ok((
                DecodedFrame::ResumeSession {
                    session_id,
                    last_recv_seq,
                    seq,
                },
                total_size,
            ));
        }

        MessageType::Ping => {
            conn.last_client_seq = seq;
            // Ping v2 payload: last_processed_server_seq(8) = piggyback ACK.
            // If payload is empty (v1 clients), treat ack_seq as 0.
            let ack_seq = if payload.len() >= 8 {
                u64::from_le_bytes(payload[0..8].try_into().expect("Ping: ack_seq"))
            } else {
                0
            };
            return Ok((DecodedFrame::Ping { seq, ack_seq }, total_size));
        }

        MessageType::Acknowledge => {
            // Explicit cumulative ACK. Allowed in Active state (session control).
            if conn.state != ConnectionState::Active {
                return Err(DecodeError::HandshakeRequired);
            }
            if payload.len() < 8 {
                return Err(DecodeError::InvalidPayload("Acknowledge too short".into()));
            }
            conn.last_client_seq = seq;
            let last_processed_server_seq =
                u64::from_le_bytes(payload[0..8].try_into().expect("Acknowledge: ack_seq"));
            return Ok((
                DecodedFrame::Acknowledge {
                    last_processed_server_seq,
                    seq,
                },
                total_size,
            ));
        }

        MessageType::GetOpenOrders => {
            // Snapshot request. Requires active session and non-zero request_id
            // (validated above via requires_request_id).
            if conn.state != ConnectionState::Active {
                return Err(DecodeError::HandshakeRequired);
            }
            conn.last_client_seq = seq;
            return Ok((DecodedFrame::GetOpenOrders { seq, request_id }, total_size));
        }

        _ if msg_type.is_inbound_command() => {
            if conn.state != ConnectionState::Active {
                return Err(DecodeError::HandshakeRequired);
            }
        }

        _ => {
            return Err(DecodeError::InvalidPayload(
                "Client sent an outbound-only message type".into(),
            ));
        }
    }

    conn.last_client_seq = seq;

    // Decode active-state commands.
    let frame = match msg_type {
        MessageType::PlaceOrder => {
            let cmd = decode_place_order(payload, clock)?;
            DecodedFrame::Command(cmd, seq, request_id)
        }
        MessageType::CancelOrder => {
            let cmd = decode_cancel_order(payload, clock)?;
            DecodedFrame::Command(cmd, seq, request_id)
        }
        MessageType::AmendOrder => {
            let cmd = decode_amend_order(payload, clock)?;
            DecodedFrame::Command(cmd, seq, request_id)
        }
        MessageType::GetOrderStatus => {
            if payload.len() < 10 {
                return Err(DecodeError::InvalidPayload(
                    "GetOrderStatus too short".into(),
                ));
            }
            let order_id =
                u64::from_le_bytes(payload[0..8].try_into().expect("GetOrderStatus: order_id"));
            let symbol_id = u16::from_le_bytes(
                payload[8..10]
                    .try_into()
                    .expect("GetOrderStatus: symbol_id"),
            );
            DecodedFrame::QueryOrder {
                order_id,
                symbol_id,
                seq,
                request_id,
            }
        }
        MessageType::BatchPlaceOrder => {
            if payload.is_empty() {
                return Err(DecodeError::InvalidPayload("BatchPlaceOrder empty".into()));
            }
            let count = payload[0];
            if count == 0 || count > MAX_BATCH_PLACE {
                return Err(DecodeError::InvalidPayload(format!(
                    "BatchPlaceOrder count must be 1..{MAX_BATCH_PLACE}, got {count}"
                )));
            }
            let mut cmds = Vec::with_capacity(count as usize);
            let mut offset = 1usize;
            for _ in 0..count {
                // Batch entries are FIXED at 38 bytes (flags mandatory). The
                // single-order format allows an optional flags byte (37 or 38),
                // but inside a batch a variable size is unparseable: with no
                // per-entry length prefix the decoder cannot tell where one
                // entry ends. Guessing the size from the remaining buffer (the
                // previous behaviour) silently corrupted every entry after the
                // first whenever a 37-byte entry was followed by another entry.
                if payload.len() < offset + BATCH_PLACE_ENTRY_SIZE {
                    return Err(DecodeError::InvalidPayload(
                        "BatchPlaceOrder truncated".into(),
                    ));
                }
                cmds.push(decode_place_order(
                    &payload[offset..offset + BATCH_PLACE_ENTRY_SIZE],
                    clock,
                )?);
                offset += BATCH_PLACE_ENTRY_SIZE;
            }
            DecodedFrame::Batch(cmds, seq, request_id)
        }
        MessageType::BatchCancelOrder => {
            if payload.is_empty() {
                return Err(DecodeError::InvalidPayload("BatchCancelOrder empty".into()));
            }
            let count = payload[0];
            if count == 0 || count > MAX_BATCH_CANCEL {
                return Err(DecodeError::InvalidPayload(format!(
                    "BatchCancelOrder count must be 1..{MAX_BATCH_CANCEL}, got {count}"
                )));
            }
            let mut cmds = Vec::with_capacity(count as usize);
            let mut offset = 1usize;
            for _ in 0..count {
                if payload.len() < offset + 10 {
                    return Err(DecodeError::InvalidPayload(
                        "BatchCancelOrder truncated".into(),
                    ));
                }
                cmds.push(decode_cancel_order(&payload[offset..offset + 10], clock)?);
                offset += 10;
            }
            DecodedFrame::Batch(cmds, seq, request_id)
        }
        _ => unreachable!("handled above"),
    };

    Ok((frame, total_size))
}

/// PlaceOrder payload, backward-compatible by length:
/// OrderId(8) + SymbolId(2) + Side(1) + OrderType(1) + TIF(1) + Price(8) +
/// StopPrice(8) + Qty(8)  = 37 bytes (base)
///   [+ Flags(1)]         = 38 bytes
///   [+ ExpireAt(8)]      = 46 bytes (GTD expiry, unix nanoseconds)
/// Older clients send 37/38 bytes; the extra fields default to 0.
fn decode_place_order(payload: &[u8], clock: &dyn me_clock::Clock) -> Result<Command, DecodeError> {
    if payload.len() < 37 {
        return Err(DecodeError::InvalidPayload("PlaceOrder too short".into()));
    }
    let order_id = u64::from_le_bytes(payload[0..8].try_into().expect("PlaceOrder: order_id"));
    if order_id == 0 {
        return Err(DecodeError::InvalidPayload(
            "PlaceOrder order_id must be non-zero".into(),
        ));
    }
    let symbol = SymbolId(u16::from_le_bytes(
        payload[8..10].try_into().expect("PlaceOrder: symbol"),
    ));
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
        3 => TimeInForce::GTD,
        v => return Err(DecodeError::InvalidPayload(format!("Invalid TIF: {v}"))),
    };
    let price = FixedPrice::from_raw(i64::from_le_bytes(
        payload[13..21].try_into().expect("PlaceOrder: price"),
    ));
    let stop_price = FixedPrice::from_raw(i64::from_le_bytes(
        payload[21..29].try_into().expect("PlaceOrder: stop_price"),
    ));
    let qty_raw = u64::from_le_bytes(payload[29..37].try_into().expect("PlaceOrder: qty"));
    if qty_raw == 0 {
        return Err(DecodeError::InvalidPayload(
            "PlaceOrder qty must be non-zero".into(),
        ));
    }
    let qty = FixedQty::from_raw(qty_raw);

    // Backward-compatible flags: byte 37 if present, else 0.
    let flags = if payload.len() >= 38 {
        payload[37]
    } else {
        0u8
    };

    // Backward-compatible GTD expiry: bytes 38..46 if present, else 0.
    let expire_at = if payload.len() >= 46 {
        u64::from_le_bytes(payload[38..46].try_into().expect("PlaceOrder: expire_at"))
    } else {
        0u64
    };
    if matches!(tif, TimeInForce::GTD) && expire_at == 0 {
        return Err(DecodeError::InvalidPayload(
            "GTD order requires a non-zero expire_at".into(),
        ));
    }

    if matches!(order_type, OrderType::Limit | OrderType::StopLimit) && !price.is_positive() {
        return Err(DecodeError::InvalidPayload(
            "Limit/StopLimit order requires positive price".into(),
        ));
    }
    if matches!(order_type, OrderType::StopLimit | OrderType::StopMarket)
        && !stop_price.is_positive()
    {
        return Err(DecodeError::InvalidPayload(
            "Stop order requires positive stop_price".into(),
        ));
    }

    let timestamp = clock.now_ns();

    let order = Order {
        id: order_id,
        price,
        stop_price,
        qty,
        remaining_qty: qty,
        cumulative_qty: FixedQty::ZERO,
        expire_at,
        timestamp,
        sequence: 0,
        symbol,
        side,
        order_type,
        tif,
        status: me_core::order::OrderStatus::New,
        flags,
        _pad: 0,
        // Note: session_id is injected post-decode by tcp_server/uds_server
        // using with_session_id(), since it's a property of the connection
        // not the wire payload.
        client_session_id: SessionId::NONE,
    };

    Ok(Command::PlaceOrder(order))
}

/// CancelOrder payload (10 bytes): OrderId(8) + SymbolId(2)
fn decode_cancel_order(
    payload: &[u8],
    clock: &dyn me_clock::Clock,
) -> Result<Command, DecodeError> {
    if payload.len() < 10 {
        return Err(DecodeError::InvalidPayload("CancelOrder too short".into()));
    }
    let order_id = u64::from_le_bytes(payload[0..8].try_into().expect("CancelOrder: order_id"));
    if order_id == 0 {
        return Err(DecodeError::InvalidPayload(
            "CancelOrder order_id must be non-zero".into(),
        ));
    }
    let symbol = SymbolId(u16::from_le_bytes(
        payload[8..10].try_into().expect("CancelOrder: symbol"),
    ));

    let timestamp = clock.now_ns();

    Ok(Command::CancelOrder {
        order_id,
        symbol,
        timestamp,
    })
}

/// AmendOrder payload (variable): OrderId(8) + SymbolId(2) + has_price(1) + [price(8)] + has_qty(1) + [qty(8)]
fn decode_amend_order(payload: &[u8], clock: &dyn me_clock::Clock) -> Result<Command, DecodeError> {
    if payload.len() < 12 {
        return Err(DecodeError::InvalidPayload("AmendOrder too short".into()));
    }
    let order_id = u64::from_le_bytes(payload[0..8].try_into().expect("AmendOrder: order_id"));
    if order_id == 0 {
        return Err(DecodeError::InvalidPayload(
            "AmendOrder order_id must be non-zero".into(),
        ));
    }
    let symbol = SymbolId(u16::from_le_bytes(
        payload[8..10].try_into().expect("AmendOrder: symbol"),
    ));

    let mut offset = 10;
    let new_price = if payload[offset] == 1 {
        offset += 1;
        if offset + 8 > payload.len() {
            return Err(DecodeError::InvalidPayload(
                "AmendOrder price truncated".into(),
            ));
        }
        let p = i64::from_le_bytes(
            payload[offset..offset + 8]
                .try_into()
                .expect("AmendOrder: new_price"),
        );
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
        let q = u64::from_le_bytes(
            payload[offset..offset + 8]
                .try_into()
                .expect("AmendOrder: new_qty"),
        );
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

    let timestamp = clock.now_ns();

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
///
/// **Sequence semantics**: Each emitted wire frame carries its own distinct
/// `server_seq` in the header, starting from `starting_seq` and incrementing
/// monotonically. This makes per-frame ACK and replay possible.
///
/// `request_id` is mirrored from the triggering client command (for solicited
/// events) or 0 for unsolicited events (e.g. passive fills, stop triggers).
///
/// Returns the number of wire frames written (== number of sequence slots
/// consumed). The caller should advance its `server_frame_seq` counter by
/// this amount after the buffer is pushed to the replay store.
pub fn encode_events(
    events: &[MatchEvent],
    starting_seq: u64,
    request_id: u64,
    buf: &mut BytesMut,
) -> u64 {
    let mut consumed = 0u64;
    for event in events {
        encode_event(event, starting_seq + consumed, request_id, buf);
        consumed += 1;
    }
    consumed
}

fn encode_event(event: &MatchEvent, seq: u64, request_id: u64, buf: &mut BytesMut) {
    match event {
        MatchEvent::OrderAccepted {
            order_id,
            timestamp,
        } => {
            write_header(buf, 16, MessageType::OrderAccepted as u8, seq, request_id);
            buf.put_u64_le(*order_id);
            buf.put_u64_le(*timestamp);
        }
        MatchEvent::OrderRejected {
            order_id,
            reason,
            timestamp,
        } => {
            write_header(buf, 17, MessageType::OrderRejected as u8, seq, request_id);
            buf.put_u64_le(*order_id);
            buf.put_u8(reject_reason_to_u8(reason));
            buf.put_u64_le(*timestamp);
        }
        MatchEvent::Trade(trade) => {
            // trade_id(8)+symbol(2)+price(8)+qty(8)+maker_id(8)+taker_id(8)+side(1)+ts(8)+seq(8) = 59
            write_header(buf, 59, MessageType::Trade as u8, seq, request_id);
            buf.put_u64_le(trade.trade_id);
            buf.put_u16_le(trade.symbol.0);
            buf.put_i64_le(trade.price.raw());
            buf.put_u64_le(trade.qty.raw());
            buf.put_u64_le(trade.maker_order_id);
            buf.put_u64_le(trade.taker_order_id);
            buf.put_u8(trade.maker_side as u8);
            buf.put_u64_le(trade.timestamp);
            buf.put_u64_le(trade.sequence);
        }
        MatchEvent::OrderFilled {
            order_id,
            filled_qty,
            price,
            timestamp,
        } => {
            write_header(buf, 32, MessageType::OrderFilled as u8, seq, request_id);
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
            write_header(
                buf,
                40,
                MessageType::OrderPartiallyFilled as u8,
                seq,
                request_id,
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
            write_header(buf, 24, MessageType::OrderCancelled as u8, seq, request_id);
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
            write_header(buf, 32, MessageType::OrderAmended as u8, seq, request_id);
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
            write_header(buf, 24, MessageType::StopTriggered as u8, seq, request_id);
            buf.put_u64_le(*order_id);
            buf.put_i64_le(trigger_price.raw());
            buf.put_u64_le(*timestamp);
        }
        MatchEvent::OrderStatus {
            order_id,
            status,
            filled_qty,
            remaining_qty,
            price,
            timestamp,
        } => {
            // order_id(8) + status(1) + filled(8) + remaining(8) + price(8) + ts(8) = 41
            write_header(buf, 41, MessageType::OrderStatus as u8, seq, request_id);
            buf.put_u64_le(*order_id);
            buf.put_u8(*status);
            buf.put_u64_le(filled_qty.raw());
            buf.put_u64_le(remaining_qty.raw());
            buf.put_i64_le(price.raw());
            buf.put_u64_le(*timestamp);
        }
        MatchEvent::OrderNotFound { order_id } => {
            write_header(buf, 8, MessageType::OrderNotFound as u8, seq, request_id);
            buf.put_u64_le(*order_id);
        }
    }
}

/// Encode a Pong response frame with piggyback server ACK.
/// Payload: last_processed_client_seq(8) — tells client how far server has read.
pub fn encode_pong(seq: u64, last_processed_client_seq: u64, buf: &mut BytesMut) {
    write_header(buf, 8, MessageType::Pong as u8, seq, 0);
    buf.put_u64_le(last_processed_client_seq);
}

/// Encode a HandshakeAck response frame with TLV extensions.
///
/// Payload layout:
/// - server_version(1)
/// - TLV: TLV_CAPABILITIES (u64, 8 bytes)  = 12 bytes
/// - TLV: TLV_SESSION_ID  ([u8;16])         = 20 bytes
/// - TLV: TLV_HEARTBEAT_MS (u32, 4 bytes)   = 8 bytes
///
/// Total payload: 41 bytes
pub fn encode_handshake_ack(
    seq: u64,
    session_id: SessionId,
    capabilities: u64,
    heartbeat_interval_ms: u32,
    buf: &mut BytesMut,
) {
    // Pre-compute payload size to write correct header.
    // version(1) + cap_tlv(12) + sid_tlv(20) + hb_tlv(8) = 41
    let payload_len: u32 = 1 + (2 + 2 + 8) + (2 + 2 + 16) + (2 + 2 + 4);
    write_header(buf, payload_len, MessageType::HandshakeAck as u8, seq, 0);
    buf.put_u8(PROTOCOL_VERSION);
    write_tlv(buf, TLV_CAPABILITIES, &capabilities.to_le_bytes());
    write_tlv(buf, TLV_SESSION_ID, session_id.as_bytes());
    write_tlv(buf, TLV_HEARTBEAT_MS, &heartbeat_interval_ms.to_le_bytes());
}

/// Encode a SessionResumed frame.
/// Payload: replayed_count(4)
pub fn encode_session_resumed(replayed: u32, seq: u64, buf: &mut BytesMut) {
    write_header(buf, 4, MessageType::SessionResumed as u8, seq, 0);
    buf.put_u32_le(replayed);
}

/// Write raw replay frames directly into the write buffer.
pub fn encode_replay_frames(frames: &[Bytes], buf: &mut BytesMut) {
    for frame in frames {
        buf.extend_from_slice(frame);
    }
}

/// Encode a protocol-level Error frame.
/// Payload: error_code(2) + message_len(2) + message([u8])
pub fn encode_error(seq: u64, code: ErrorCode, message: &str, buf: &mut BytesMut) {
    let msg_bytes = message.as_bytes();
    let msg_len = msg_bytes.len().min(255) as u16;
    let payload_len = 2 + 2 + msg_len as u32;
    write_header(buf, payload_len, MessageType::Error as u8, seq, 0);
    buf.put_u16_le(code as u16);
    buf.put_u16_le(msg_len);
    buf.put_slice(&msg_bytes[..msg_len as usize]);
}

/// Encode a ReplayGap frame — client requested replay from a seq that was evicted.
/// Client MUST respond with GetOpenOrders for resync.
/// Payload: missing_from(8) + missing_to(8) = 16 bytes
pub fn encode_replay_gap(seq: u64, missing_from: u64, missing_to: u64, buf: &mut BytesMut) {
    write_header(buf, 16, MessageType::ReplayGap as u8, seq, 0);
    buf.put_u64_le(missing_from);
    buf.put_u64_le(missing_to);
}

/// Encode an OpenOrdersBegin frame — marks start of snapshot response.
/// Payload: count(4) + snapshot_seq(8) = 12 bytes
pub fn encode_open_orders_begin(
    seq: u64,
    request_id: u64,
    count: u32,
    snapshot_seq: u64,
    buf: &mut BytesMut,
) {
    write_header(buf, 12, MessageType::OpenOrdersBegin as u8, seq, request_id);
    buf.put_u32_le(count);
    buf.put_u64_le(snapshot_seq);
}

/// Encode an OpenOrdersEnd frame — marks end of snapshot response.
/// Payload: snapshot_seq(8) = 8 bytes
pub fn encode_open_orders_end(seq: u64, request_id: u64, snapshot_seq: u64, buf: &mut BytesMut) {
    write_header(buf, 8, MessageType::OpenOrdersEnd as u8, seq, request_id);
    buf.put_u64_le(snapshot_seq);
}

/// Encode a single OpenOrder record within a snapshot response.
///
/// Payload (60 bytes): order_id(8) + symbol(2) + side(1) + order_type(1) +
/// tif(1) + status(1) + price(8) + stop_price(8) + qty(8) + remaining_qty(8) +
/// timestamp(8) + flags(1) + _reserved(5) = 60 bytes.
///
/// `client_session_id` is NOT echoed back (client already knows its own ID).
pub fn encode_open_order(
    seq: u64,
    request_id: u64,
    order: &me_core::order::Order,
    buf: &mut BytesMut,
) {
    write_header(buf, 60, MessageType::OpenOrder as u8, seq, request_id);
    buf.put_u64_le(order.id);
    buf.put_u16_le(order.symbol.0);
    buf.put_u8(order.side as u8);
    buf.put_u8(order.order_type as u8);
    buf.put_u8(order.tif as u8);
    buf.put_u8(order.status as u8);
    buf.put_i64_le(order.price.raw());
    buf.put_i64_le(order.stop_price.raw());
    buf.put_u64_le(order.qty.raw());
    buf.put_u64_le(order.remaining_qty.raw());
    buf.put_u64_le(order.timestamp);
    buf.put_u8(order.flags);
    // Reserved padding to make payload aligned and future-proof
    buf.put_u8(0);
    buf.put_u8(0);
    buf.put_u8(0);
    buf.put_u8(0);
    buf.put_u8(0);
}

/// Encode a FlowControlWarning frame — slow consumer notification.
/// Payload: buffer_used_pct(1) + is_hard_limit(1) = 2 bytes
pub fn encode_flow_control_warning(
    seq: u64,
    buffer_used_pct: u8,
    is_hard_limit: bool,
    buf: &mut BytesMut,
) {
    write_header(buf, 2, MessageType::FlowControlWarning as u8, seq, 0);
    buf.put_u8(buffer_used_pct);
    buf.put_u8(is_hard_limit as u8);
}

/// Write a v2 frame header: len(4) + ver(1) + type(1) + seq(8) + request_id(8) = 22 bytes.
/// For server→client frames `request_id` is either:
/// - Mirrored from the triggering client command (for command responses), or
/// - 0 for unsolicited events (resting-order fills, stop triggers, session control).
fn write_header(buf: &mut BytesMut, payload_len: u32, msg_type: u8, seq: u64, request_id: u64) {
    buf.put_u32_le(payload_len);
    buf.put_u8(PROTOCOL_VERSION);
    buf.put_u8(msg_type);
    buf.put_u64_le(seq);
    buf.put_u64_le(request_id);
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
        RejectReason::WouldTakeMarket => 11,
        RejectReason::InternalError => 12,
        RejectReason::SelfTradePrevented => 13,
        RejectReason::InvalidExpiry => 14,
    }
}
