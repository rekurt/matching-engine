use serde::Serialize;

/// Audit event types for regulatory compliance.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "event_type")]
pub enum AuditEvent {
    /// New order submitted.
    OrderSubmitted {
        timestamp_ns: u64,
        order_id: u64,
        symbol_id: u16,
        side: String,
        order_type: String,
        price_raw: i64,
        qty_raw: u64,
        tif: String,
    },
    /// Order accepted by matching engine.
    OrderAccepted {
        timestamp_ns: u64,
        order_id: u64,
    },
    /// Order rejected.
    OrderRejected {
        timestamp_ns: u64,
        order_id: u64,
        reason: String,
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
        maker_side: String,
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
        reason: String,
    },
    /// Trading resumed.
    TradingResumed {
        timestamp_ns: u64,
        symbol_id: u16,
    },
}
