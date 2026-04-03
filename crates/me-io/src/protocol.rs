// Wire protocol message types for the binary API transport.
//
// Frame format (length-prefixed, versioned):
// ┌──────────┬──────────┬──────────┬──────────┬─────────────────┐
// │ len: u32 │ ver: u8  │ type: u8 │ seq: u64 │ payload: [u8]   │
// │ 4 bytes  │ 1 byte   │ 1 byte   │ 8 bytes  │ variable        │
// └──────────┴──────────┴──────────┴──────────┴─────────────────┘
//
// `len` covers only the payload bytes (not the header).
// `ver` is the protocol version (currently 1).
// `type` is one of the MessageType variants.
// `seq` is a monotonically increasing sequence number per connection.

/// Current wire protocol version.
pub const PROTOCOL_VERSION: u8 = 1;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageType {
    // ── Inbound (client → engine) ────────────────────────────────────────
    PlaceOrder = 0x01,
    CancelOrder = 0x02,
    AmendOrder = 0x03,

    // ── Connection management (bidirectional / inbound) ───────────────────
    Ping = 0x04,
    Handshake = 0x05,
    /// Resume a previous session after reconnect.
    /// Payload: session_id(16) + last_recv_seq(8) = 24 bytes
    ResumeSession = 0x06,
    /// Query the current state of a resting order.
    /// Payload: order_id(8) + symbol_id(2) = 10 bytes
    GetOrderStatus = 0x07,
    /// Place multiple orders in a single frame (max 20).
    /// Payload: count(1) + [PlaceOrder payload(37-38 bytes) × count]
    BatchPlaceOrder = 0x0B,
    /// Cancel multiple orders in a single frame (max 50).
    /// Payload: count(1) + [CancelOrder payload(10 bytes) × count]
    BatchCancelOrder = 0x0C,

    // ── Outbound (engine → client) ────────────────────────────────────────
    OrderAccepted = 0x10,
    OrderRejected = 0x11,
    Trade = 0x12,
    OrderFilled = 0x13,
    OrderPartiallyFilled = 0x14,
    OrderCancelled = 0x15,
    OrderAmended = 0x16,
    StopTriggered = 0x17,

    // ── Connection management (outbound) ─────────────────────────────────
    Pong = 0x18,
    Error = 0x19,
    /// Handshake accepted. Payload: server_version(1) + TLV extensions.
    HandshakeAck = 0x1A,
    /// Session resumed after reconnect. Payload: replayed_count(4).
    SessionResumed = 0x1B,
    /// Response to GetOrderStatus — order is resting.
    /// Payload: order_id(8)+status(1)+filled(8)+remaining(8)+price(8)+ts(8) = 41 bytes
    OrderStatus = 0x1D,
    /// Response to GetOrderStatus — order not found in book.
    /// Payload: order_id(8) = 8 bytes
    OrderNotFound = 0x1E,
}

impl MessageType {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0x01 => Some(Self::PlaceOrder),
            0x02 => Some(Self::CancelOrder),
            0x03 => Some(Self::AmendOrder),
            0x04 => Some(Self::Ping),
            0x05 => Some(Self::Handshake),
            0x06 => Some(Self::ResumeSession),
            0x07 => Some(Self::GetOrderStatus),
            0x0B => Some(Self::BatchPlaceOrder),
            0x0C => Some(Self::BatchCancelOrder),
            0x10 => Some(Self::OrderAccepted),
            0x11 => Some(Self::OrderRejected),
            0x12 => Some(Self::Trade),
            0x13 => Some(Self::OrderFilled),
            0x14 => Some(Self::OrderPartiallyFilled),
            0x15 => Some(Self::OrderCancelled),
            0x16 => Some(Self::OrderAmended),
            0x17 => Some(Self::StopTriggered),
            0x18 => Some(Self::Pong),
            0x19 => Some(Self::Error),
            0x1A => Some(Self::HandshakeAck),
            0x1B => Some(Self::SessionResumed),
            0x1D => Some(Self::OrderStatus),
            0x1E => Some(Self::OrderNotFound),
            _ => None,
        }
    }

    /// Whether this message type is an inbound command (client → engine) that
    /// requires an active session.
    pub fn is_inbound_command(&self) -> bool {
        matches!(
            self,
            Self::PlaceOrder
                | Self::CancelOrder
                | Self::AmendOrder
                | Self::GetOrderStatus
                | Self::BatchPlaceOrder
                | Self::BatchCancelOrder
        )
    }
}

/// Header size in bytes: len(4) + ver(1) + type(1) + seq(8) = 14
pub const FRAME_HEADER_SIZE: usize = 14;

/// Maximum payload size for regular frames (prevent memory exhaustion).
pub const MAX_PAYLOAD_SIZE: u32 = 1024;

/// Maximum payload size for batch order frames.
pub const MAX_PAYLOAD_SIZE_BATCH: u32 = 4096;

/// Maximum write buffer size for backpressure (256 KB).
pub const MAX_WRITE_BUFFER_SIZE: usize = 256 * 1024;

/// Maximum orders in a BatchPlaceOrder frame.
pub const MAX_BATCH_PLACE: u8 = 20;

/// Maximum orders in a BatchCancelOrder frame.
pub const MAX_BATCH_CANCEL: u8 = 50;

// ─── TLV extension tags (used in HandshakeAck and future messages) ────────────

/// TLV tag: server capabilities bitmask (u64 LE, 8 bytes).
pub const TLV_CAPABILITIES: u16 = 0x0001;
/// TLV tag: session ID assigned to this connection ([u8; 16]).
pub const TLV_SESSION_ID: u16 = 0x0002;
/// TLV tag: server heartbeat interval the client should respect (u32 LE, ms).
pub const TLV_HEARTBEAT_MS: u16 = 0x0003;

// ─── Server capability bits (TLV_CAPABILITIES bitmask) ───────────────────────

pub const CAP_SESSION_RECOVERY: u64 = 1 << 0;
pub const CAP_HEARTBEAT: u64 = 1 << 1;
pub const CAP_BATCH_ORDERS: u64 = 1 << 2;
pub const CAP_ORDER_FLAGS: u64 = 1 << 3;
pub const CAP_ORDER_QUERY: u64 = 1 << 4;
pub const CAP_TLS: u64 = 1 << 5;

/// Protocol-level error codes sent in Error (0x19) frames.
#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    /// Client sent an unknown or unsupported protocol version.
    UnsupportedVersion = 1,
    /// Sequence number was not monotonically increasing.
    SequenceOutOfOrder = 2,
    /// Message arrived before handshake completed.
    HandshakeRequired = 3,
    /// Payload failed validation (bad enum value, zero ID, etc.).
    InvalidPayload = 4,
    /// Server is shutting down.
    ServerShuttingDown = 5,
    /// Write buffer backpressure — client is consuming too slowly.
    Backpressure = 6,
    /// Client did not send Ping within the expected heartbeat window.
    HeartbeatTimeout = 7,
    /// TLS handshake failed (only sent when TLS feature is enabled).
    TlsError = 8,
}

/// Connection state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// Waiting for a Handshake or ResumeSession message from the client.
    HandshakePending,
    /// Handshake completed, connection is active.
    Active,
    /// Server or client initiated graceful close.
    Closing,
}
