/// Wire protocol message types for the binary API transport.
///
/// Frame format (length-prefixed):
/// ┌──────────┬──────────┬──────────┬─────────────────┐
/// │ len: u32 │ type: u8 │ seq: u64 │ payload: [u8]   │
/// │ 4 bytes  │ 1 byte   │ 8 bytes  │ variable        │
/// └──────────┴──────────┴──────────┴─────────────────┘

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageType {
    // Inbound (client → engine)
    PlaceOrder = 0x01,
    CancelOrder = 0x02,
    AmendOrder = 0x03,

    // Outbound (engine → client)
    OrderAccepted = 0x10,
    OrderRejected = 0x11,
    Trade = 0x12,
    OrderFilled = 0x13,
    OrderPartiallyFilled = 0x14,
    OrderCancelled = 0x15,
    OrderAmended = 0x16,
    StopTriggered = 0x17,
}

impl MessageType {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0x01 => Some(Self::PlaceOrder),
            0x02 => Some(Self::CancelOrder),
            0x03 => Some(Self::AmendOrder),
            0x10 => Some(Self::OrderAccepted),
            0x11 => Some(Self::OrderRejected),
            0x12 => Some(Self::Trade),
            0x13 => Some(Self::OrderFilled),
            0x14 => Some(Self::OrderPartiallyFilled),
            0x15 => Some(Self::OrderCancelled),
            0x16 => Some(Self::OrderAmended),
            0x17 => Some(Self::StopTriggered),
            _ => None,
        }
    }
}

/// Header size in bytes: len(4) + type(1) + seq(8) = 13
pub const FRAME_HEADER_SIZE: usize = 13;

/// Maximum payload size (prevent memory exhaustion from malicious frames).
pub const MAX_PAYLOAD_SIZE: u32 = 1024;
