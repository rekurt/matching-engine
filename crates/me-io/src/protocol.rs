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
    // Inbound (client → engine)
    PlaceOrder = 0x01,
    CancelOrder = 0x02,
    AmendOrder = 0x03,

    // Connection management (bidirectional)
    Ping = 0x04,
    Handshake = 0x05,

    // Outbound (engine → client)
    OrderAccepted = 0x10,
    OrderRejected = 0x11,
    Trade = 0x12,
    OrderFilled = 0x13,
    OrderPartiallyFilled = 0x14,
    OrderCancelled = 0x15,
    OrderAmended = 0x16,
    StopTriggered = 0x17,

    // Connection management (outbound)
    Pong = 0x18,
    Error = 0x19,
    HandshakeAck = 0x1A,
}

impl MessageType {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0x01 => Some(Self::PlaceOrder),
            0x02 => Some(Self::CancelOrder),
            0x03 => Some(Self::AmendOrder),
            0x04 => Some(Self::Ping),
            0x05 => Some(Self::Handshake),
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
            _ => None,
        }
    }

    /// Whether this message type is an inbound command (client → engine).
    pub fn is_inbound_command(&self) -> bool {
        matches!(
            self,
            Self::PlaceOrder | Self::CancelOrder | Self::AmendOrder
        )
    }
}

/// Header size in bytes: len(4) + ver(1) + type(1) + seq(8) = 14
pub const FRAME_HEADER_SIZE: usize = 14;

/// Maximum payload size (prevent memory exhaustion from malicious frames).
pub const MAX_PAYLOAD_SIZE: u32 = 1024;

/// Maximum write buffer size for backpressure (256 KB).
pub const MAX_WRITE_BUFFER_SIZE: usize = 256 * 1024;

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
}

/// Connection state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// Waiting for a Handshake message from the client.
    HandshakePending,
    /// Handshake completed, connection is active.
    Active,
    /// Server or client initiated graceful close.
    Closing,
}
