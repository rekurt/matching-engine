use serde::Serialize;

/// Audit event types for regulatory compliance.
///
/// All fields are zero-alloc: no `String`, no heap pointers.
/// Enum discriminants (side, order_type, tif, reason) are stored as `u8`
/// and serialized as human-readable strings via custom helpers.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "event_type")]
pub enum AuditEvent {
    /// New order submitted.
    OrderSubmitted {
        timestamp_ns: u64,
        order_id: u64,
        symbol_id: u16,
        #[serde(serialize_with = "ser_side")]
        side: u8,
        #[serde(serialize_with = "ser_order_type")]
        order_type: u8,
        price_raw: i64,
        qty_raw: u64,
        #[serde(serialize_with = "ser_tif")]
        tif: u8,
    },
    /// Order accepted by matching engine.
    OrderAccepted { timestamp_ns: u64, order_id: u64 },
    /// Order rejected.
    OrderRejected {
        timestamp_ns: u64,
        order_id: u64,
        #[serde(serialize_with = "ser_reject_reason")]
        reason: u8,
    },
    /// Trade executed.
    TradeExecuted {
        timestamp_ns: u64,
        trade_id: u64,
        symbol_id: u16,
        price_raw: i64,
        qty_raw: u64,
        maker_order_id: u64,
        taker_order_id: u64,
        #[serde(serialize_with = "ser_side")]
        maker_side: u8,
    },
    /// Order filled (completely).
    OrderFilled {
        timestamp_ns: u64,
        order_id: u64,
        filled_qty_raw: u64,
        price_raw: i64,
    },
    /// Order partially filled.
    OrderPartiallyFilled {
        timestamp_ns: u64,
        order_id: u64,
        filled_qty_raw: u64,
        remaining_qty_raw: u64,
        price_raw: i64,
    },
    /// Order cancelled.
    OrderCancelled {
        timestamp_ns: u64,
        order_id: u64,
        remaining_qty_raw: u64,
    },
    /// Order amended.
    OrderAmended {
        timestamp_ns: u64,
        order_id: u64,
        new_price_raw: i64,
        new_qty_raw: u64,
    },
    /// Stop order triggered.
    StopTriggered {
        timestamp_ns: u64,
        order_id: u64,
        trigger_price_raw: i64,
    },
    /// Trading halted.
    TradingHalted {
        timestamp_ns: u64,
        symbol_id: u16,
        #[serde(serialize_with = "ser_halt_reason")]
        reason: u8,
    },
    /// Trading resumed.
    TradingResumed { timestamp_ns: u64, symbol_id: u16 },
}

// ─── Fixed-size entry for SPSC ring buffer (zero-alloc hot path) ────────────

/// Fixed-size audit entry for the async ring buffer.
/// No heap, no pointers, `Copy`. Matching thread writes this, audit thread reads.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct AuditEntry {
    pub timestamp_ns: u64,
    pub event_type: u8,
    pub order_id: u64,
    pub symbol_id: u16,
    pub side: u8,
    pub order_type: u8,
    pub tif: u8,
    pub price_raw: i64,
    pub qty_raw: u64,
    pub aux_i64: i64,
    pub aux_u64: u64,
    pub maker_order_id: u64,
    pub taker_order_id: u64,
    pub reason: u8,
}

/// Event type discriminants for `AuditEntry.event_type`.
pub mod event_type {
    pub const ORDER_SUBMITTED: u8 = 1;
    pub const ORDER_ACCEPTED: u8 = 2;
    pub const ORDER_REJECTED: u8 = 3;
    pub const TRADE_EXECUTED: u8 = 4;
    pub const ORDER_FILLED: u8 = 5;
    pub const ORDER_PARTIALLY_FILLED: u8 = 6;
    pub const ORDER_CANCELLED: u8 = 7;
    pub const ORDER_AMENDED: u8 = 8;
    pub const STOP_TRIGGERED: u8 = 9;
    pub const TRADING_HALTED: u8 = 10;
    pub const TRADING_RESUMED: u8 = 11;
}

/// Halt reason discriminants.
pub mod halt_reason {
    pub const MANUAL: u8 = 0;
    pub const CIRCUIT_BREAKER: u8 = 1;
    pub const MAINTENANCE: u8 = 2;
}

impl AuditEntry {
    /// Wire size of the explicit little-endian binary encoding produced by
    /// [`AuditEntry::to_bytes`]. This is the sum of the field sizes only —
    /// it deliberately excludes the struct's `repr(C)` alignment padding.
    /// 8+1+8+2+1+1+1+8+8+8+8+8+8+1 = 71.
    pub const ENCODED_LEN: usize = 71;

    /// Serialize to a fixed-size little-endian byte buffer.
    ///
    /// This replaces a previous `slice::from_raw_parts` over the raw struct,
    /// which read the `repr(C)` alignment padding bytes — constructing a
    /// `&[u8]` over uninitialized padding is undefined behaviour in Rust. The
    /// explicit field-by-field encoding is both UB-free and a stable,
    /// portable wire format (no dependency on the compiler's struct layout).
    pub fn to_bytes(&self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        let mut off = 0usize;
        macro_rules! put {
            ($val:expr) => {{
                let b = $val.to_le_bytes();
                out[off..off + b.len()].copy_from_slice(&b);
                off += b.len();
            }};
        }
        put!(self.timestamp_ns);
        put!(self.event_type);
        put!(self.order_id);
        put!(self.symbol_id);
        put!(self.side);
        put!(self.order_type);
        put!(self.tif);
        put!(self.price_raw);
        put!(self.qty_raw);
        put!(self.aux_i64);
        put!(self.aux_u64);
        put!(self.maker_order_id);
        put!(self.taker_order_id);
        put!(self.reason);
        debug_assert_eq!(off, Self::ENCODED_LEN);
        out
    }

    /// Inverse of [`AuditEntry::to_bytes`]. Returns `None` if the slice is
    /// shorter than [`AuditEntry::ENCODED_LEN`].
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::ENCODED_LEN {
            return None;
        }
        let mut off = 0usize;
        macro_rules! get {
            ($t:ty) => {{
                const N: usize = std::mem::size_of::<$t>();
                let v = <$t>::from_le_bytes(buf[off..off + N].try_into().unwrap());
                off += N;
                v
            }};
        }
        let e = AuditEntry {
            timestamp_ns: get!(u64),
            event_type: get!(u8),
            order_id: get!(u64),
            symbol_id: get!(u16),
            side: get!(u8),
            order_type: get!(u8),
            tif: get!(u8),
            price_raw: get!(i64),
            qty_raw: get!(u64),
            aux_i64: get!(i64),
            aux_u64: get!(u64),
            maker_order_id: get!(u64),
            taker_order_id: get!(u64),
            reason: get!(u8),
        };
        debug_assert_eq!(off, Self::ENCODED_LEN);
        Some(e)
    }

    /// Convert to a rich `AuditEvent` (for JSON serialization on the consumer thread).
    pub fn to_audit_event(&self) -> AuditEvent {
        match self.event_type {
            event_type::ORDER_SUBMITTED => AuditEvent::OrderSubmitted {
                timestamp_ns: self.timestamp_ns,
                order_id: self.order_id,
                symbol_id: self.symbol_id,
                side: self.side,
                order_type: self.order_type,
                price_raw: self.price_raw,
                qty_raw: self.qty_raw,
                tif: self.tif,
            },
            event_type::ORDER_ACCEPTED => AuditEvent::OrderAccepted {
                timestamp_ns: self.timestamp_ns,
                order_id: self.order_id,
            },
            event_type::ORDER_REJECTED => AuditEvent::OrderRejected {
                timestamp_ns: self.timestamp_ns,
                order_id: self.order_id,
                reason: self.reason,
            },
            event_type::TRADE_EXECUTED => AuditEvent::TradeExecuted {
                timestamp_ns: self.timestamp_ns,
                trade_id: self.aux_u64,
                symbol_id: self.symbol_id,
                price_raw: self.price_raw,
                qty_raw: self.qty_raw,
                maker_order_id: self.maker_order_id,
                taker_order_id: self.taker_order_id,
                maker_side: self.side,
            },
            event_type::ORDER_FILLED => AuditEvent::OrderFilled {
                timestamp_ns: self.timestamp_ns,
                order_id: self.order_id,
                filled_qty_raw: self.qty_raw,
                price_raw: self.price_raw,
            },
            event_type::ORDER_PARTIALLY_FILLED => AuditEvent::OrderPartiallyFilled {
                timestamp_ns: self.timestamp_ns,
                order_id: self.order_id,
                filled_qty_raw: self.qty_raw,
                remaining_qty_raw: self.aux_u64,
                price_raw: self.price_raw,
            },
            event_type::ORDER_CANCELLED => AuditEvent::OrderCancelled {
                timestamp_ns: self.timestamp_ns,
                order_id: self.order_id,
                remaining_qty_raw: self.qty_raw,
            },
            event_type::ORDER_AMENDED => AuditEvent::OrderAmended {
                timestamp_ns: self.timestamp_ns,
                order_id: self.order_id,
                new_price_raw: self.price_raw,
                new_qty_raw: self.qty_raw,
            },
            event_type::STOP_TRIGGERED => AuditEvent::StopTriggered {
                timestamp_ns: self.timestamp_ns,
                order_id: self.order_id,
                trigger_price_raw: self.price_raw,
            },
            event_type::TRADING_HALTED => AuditEvent::TradingHalted {
                timestamp_ns: self.timestamp_ns,
                symbol_id: self.symbol_id,
                reason: self.reason,
            },
            event_type::TRADING_RESUMED => AuditEvent::TradingResumed {
                timestamp_ns: self.timestamp_ns,
                symbol_id: self.symbol_id,
            },
            _ => AuditEvent::OrderAccepted {
                timestamp_ns: self.timestamp_ns,
                order_id: self.order_id,
            },
        }
    }
}

// ─── Serde helpers: u8 → human-readable string in JSON ─────────────────────

fn ser_side<S: serde::Serializer>(v: &u8, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(match v {
        0 => "Buy",
        1 => "Sell",
        _ => "Unknown",
    })
}

fn ser_order_type<S: serde::Serializer>(v: &u8, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(match v {
        0 => "Limit",
        1 => "Market",
        2 => "StopLimit",
        3 => "StopMarket",
        _ => "Unknown",
    })
}

fn ser_tif<S: serde::Serializer>(v: &u8, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(match v {
        0 => "GTC",
        1 => "IOC",
        2 => "FOK",
        _ => "Unknown",
    })
}

fn ser_reject_reason<S: serde::Serializer>(v: &u8, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(match v {
        0 => "NoLiquidity",
        1 => "InsufficientLiquidity",
        2 => "InvalidPrice",
        3 => "InvalidQuantity",
        4 => "InvalidLotSize",
        5 => "InvalidTickSize",
        6 => "OrderNotFound",
        7 => "DuplicateOrderId",
        8 => "SymbolNotFound",
        9 => "TradingHalted",
        10 => "RiskCheckFailed",
        11 => "WouldTakeMarket",
        12 => "InternalError",
        _ => "Unknown",
    })
}

fn ser_halt_reason<S: serde::Serializer>(v: &u8, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(match v {
        0 => "Manual",
        1 => "CircuitBreaker",
        2 => "Maintenance",
        _ => "Unknown",
    })
}

#[cfg(test)]
mod entry_codec_tests {
    use super::*;

    fn sample() -> AuditEntry {
        AuditEntry {
            timestamp_ns: 0x0102_0304_0506_0708,
            event_type: event_type::TRADE_EXECUTED,
            order_id: 0xDEAD_BEEF,
            symbol_id: 0x1234,
            side: 1,
            order_type: 2,
            tif: 3,
            price_raw: -123_456_789,
            qty_raw: 987_654_321,
            aux_i64: -42,
            aux_u64: 42,
            maker_order_id: 111,
            taker_order_id: 222,
            reason: 9,
        }
    }

    #[test]
    fn to_bytes_has_fixed_field_only_length() {
        // The explicit encoding excludes the struct's repr(C) padding; the size
        // is the exact sum of the field widths.
        assert_eq!(AuditEntry::ENCODED_LEN, 71);
        assert_eq!(sample().to_bytes().len(), AuditEntry::ENCODED_LEN);
    }

    #[test]
    fn to_bytes_from_bytes_roundtrip() {
        let e = sample();
        let bytes = e.to_bytes();
        let back = AuditEntry::from_bytes(&bytes).expect("decode");
        assert_eq!(e.timestamp_ns, back.timestamp_ns);
        assert_eq!(e.event_type, back.event_type);
        assert_eq!(e.order_id, back.order_id);
        assert_eq!(e.symbol_id, back.symbol_id);
        assert_eq!(e.side, back.side);
        assert_eq!(e.order_type, back.order_type);
        assert_eq!(e.tif, back.tif);
        assert_eq!(e.price_raw, back.price_raw);
        assert_eq!(e.qty_raw, back.qty_raw);
        assert_eq!(e.aux_i64, back.aux_i64);
        assert_eq!(e.aux_u64, back.aux_u64);
        assert_eq!(e.maker_order_id, back.maker_order_id);
        assert_eq!(e.taker_order_id, back.taker_order_id);
        assert_eq!(e.reason, back.reason);
    }

    #[test]
    fn from_bytes_rejects_short_input() {
        let short = vec![0u8; AuditEntry::ENCODED_LEN - 1];
        assert!(AuditEntry::from_bytes(&short).is_none());
    }
}
