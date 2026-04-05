use serde::{Deserialize, Serialize};

use crate::types::{FixedPrice, FixedQty, OrderId, SequenceId, Side, SymbolId};

// ─── Order execution flags (bit mask, stored in Order.flags) ─────────────────

/// Reject the order if it would immediately cross the spread (maker-only).
pub const ORDER_FLAG_POST_ONLY: u8 = 0x01;
/// Reject if the order would not reduce an open position (futures semantics).
pub const ORDER_FLAG_REDUCE_ONLY: u8 = 0x02;
/// Like POST_ONLY but also applies to stop orders once triggered.
pub const ORDER_FLAG_MAKER_OR_CANCEL: u8 = 0x04;

// ─── OrderType ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[repr(u8)]
pub enum OrderType {
    Limit = 0,
    Market = 1,
    StopLimit = 2,
    StopMarket = 3,
}

// ─── TimeInForce ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[repr(u8)]
pub enum TimeInForce {
    /// Good Till Cancel — remains on book until filled or cancelled.
    GTC = 0,
    /// Immediate or Cancel — fill what you can, cancel the rest.
    IOC = 1,
    /// Fill or Kill — fill entirely or reject entirely.
    FOK = 2,
}

// ─── OrderStatus ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[repr(u8)]
pub enum OrderStatus {
    New,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
    /// Stop order has been triggered and converted to limit/market.
    Triggered,
}

// ─── Order ───────────────────────────────────────────────────────────────────

/// Core order struct. 88 bytes (72 + 16 client_session_id).
///
/// ## Layout rationale
/// Fields ordered to minimize padding. First 72 bytes match the matching
/// engine's hot-path access pattern (price/qty/status) and fit in the first
/// cache line on x86_64 (64 bytes) plus a small tail. The 16-byte
/// `client_session_id` lives at the end — it's only touched at submission
/// time (codec decode) and at snapshot time (OpenOrders scan), NEVER in the
/// matching fast path. Placing it last keeps the critical fields cache-hot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(C)]
pub struct Order {
    pub id: OrderId,
    pub price: FixedPrice,
    pub stop_price: FixedPrice,
    pub qty: FixedQty,
    pub remaining_qty: FixedQty,
    pub cumulative_qty: FixedQty,
    pub timestamp: u64,
    pub sequence: SequenceId,
    pub symbol: SymbolId,
    pub side: Side,
    pub order_type: OrderType,
    pub tif: TimeInForce,
    pub status: OrderStatus,
    /// Execution flags bit-mask (ORDER_FLAG_POST_ONLY, etc.). Former _pad[0].
    pub flags: u8,
    pub _pad: u8,
    /// Client session that submitted this order. Used for per-client
    /// OpenOrders snapshot queries (protocol v2 GetOpenOrders handler).
    /// All-zero bytes indicate "no session" (internal tests, fixtures).
    pub client_session_id: [u8; 16],
}

/// Compile-time size guard: if someone adds a field that changes Order layout,
/// the docs at line 54 say "88 bytes" — this fails the build loudly instead of
/// silently drifting. Update both this assert and the docstring together.
const _: () = assert!(
    std::mem::size_of::<Order>() == 88,
    "Order struct size changed — update docstring at line 54 and this assert"
);

impl Order {
    /// Create a new limit order.
    pub fn new_limit(
        id: OrderId,
        symbol: SymbolId,
        side: Side,
        tif: TimeInForce,
        price: FixedPrice,
        qty: FixedQty,
        timestamp: u64,
    ) -> Self {
        Self {
            id,
            price,
            stop_price: FixedPrice::ZERO,
            qty,
            remaining_qty: qty,
            cumulative_qty: FixedQty::ZERO,
            timestamp,
            sequence: 0,
            symbol,
            side,
            order_type: OrderType::Limit,
            tif,
            status: OrderStatus::New,
            flags: 0,
            _pad: 0,
            client_session_id: [0u8; 16],
        }
    }

    /// Create a new market order.
    pub fn new_market(
        id: OrderId,
        symbol: SymbolId,
        side: Side,
        tif: TimeInForce,
        qty: FixedQty,
        timestamp: u64,
    ) -> Self {
        Self {
            id,
            price: FixedPrice::ZERO,
            stop_price: FixedPrice::ZERO,
            qty,
            remaining_qty: qty,
            cumulative_qty: FixedQty::ZERO,
            timestamp,
            sequence: 0,
            symbol,
            side,
            order_type: OrderType::Market,
            tif,
            status: OrderStatus::New,
            flags: 0,
            _pad: 0,
            client_session_id: [0u8; 16],
        }
    }

    /// Create a new stop-limit order.
    #[allow(clippy::too_many_arguments)]
    pub fn new_stop_limit(
        id: OrderId,
        symbol: SymbolId,
        side: Side,
        tif: TimeInForce,
        price: FixedPrice,
        stop_price: FixedPrice,
        qty: FixedQty,
        timestamp: u64,
    ) -> Self {
        Self {
            id,
            price,
            stop_price,
            qty,
            remaining_qty: qty,
            cumulative_qty: FixedQty::ZERO,
            timestamp,
            sequence: 0,
            symbol,
            side,
            order_type: OrderType::StopLimit,
            tif,
            status: OrderStatus::New,
            flags: 0,
            _pad: 0,
            client_session_id: [0u8; 16],
        }
    }

    /// Create a new stop-market order.
    pub fn new_stop_market(
        id: OrderId,
        symbol: SymbolId,
        side: Side,
        tif: TimeInForce,
        stop_price: FixedPrice,
        qty: FixedQty,
        timestamp: u64,
    ) -> Self {
        Self {
            id,
            price: FixedPrice::ZERO,
            stop_price,
            qty,
            remaining_qty: qty,
            cumulative_qty: FixedQty::ZERO,
            timestamp,
            sequence: 0,
            symbol,
            side,
            order_type: OrderType::StopMarket,
            tif,
            status: OrderStatus::New,
            flags: 0,
            _pad: 0,
            client_session_id: [0u8; 16],
        }
    }

    /// Builder: associate this order with a client session for later lookup
    /// by OpenOrders snapshot. Returns self for chaining.
    ///
    /// Used by the codec when decoding client-submitted orders.
    #[inline]
    pub fn with_session_id(mut self, session_id: [u8; 16]) -> Self {
        self.client_session_id = session_id;
        self
    }

    /// Whether this order is a stop order (not yet triggered).
    #[inline]
    pub fn is_stop(&self) -> bool {
        matches!(
            self.order_type,
            OrderType::StopLimit | OrderType::StopMarket
        )
    }

    /// Convert a triggered stop order to its active form.
    pub fn convert_stop(&mut self) {
        match self.order_type {
            OrderType::StopLimit => {
                self.order_type = OrderType::Limit;
                self.status = OrderStatus::Triggered;
            }
            OrderType::StopMarket => {
                self.order_type = OrderType::Market;
                self.status = OrderStatus::Triggered;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FixedPrice;

    #[test]
    fn test_new_limit_order() {
        let o = Order::new_limit(
            1,
            SymbolId(0),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_f64(100.0),
            FixedQty::from_f64(1.0),
            1000,
        );
        assert_eq!(o.id, 1);
        assert_eq!(o.order_type, OrderType::Limit);
        assert_eq!(o.remaining_qty, o.qty);
        assert_eq!(o.cumulative_qty, FixedQty::ZERO);
        assert_eq!(o.status, OrderStatus::New);
        assert!(o.stop_price.is_zero());
    }

    #[test]
    fn test_new_market_order() {
        let o = Order::new_market(
            2,
            SymbolId(0),
            Side::Sell,
            TimeInForce::IOC,
            FixedQty::from_f64(5.0),
            2000,
        );
        assert_eq!(o.order_type, OrderType::Market);
        assert!(o.price.is_zero());
    }

    #[test]
    fn test_stop_order_conversion() {
        let mut o = Order::new_stop_limit(
            3,
            SymbolId(0),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_f64(105.0),
            FixedPrice::from_f64(100.0),
            FixedQty::from_f64(1.0),
            3000,
        );
        assert!(o.is_stop());
        o.convert_stop();
        assert_eq!(o.order_type, OrderType::Limit);
        assert_eq!(o.status, OrderStatus::Triggered);
        assert!(!o.is_stop());
    }

    #[test]
    fn test_stop_market_conversion() {
        let mut o = Order::new_stop_market(
            4,
            SymbolId(0),
            Side::Sell,
            TimeInForce::GTC,
            FixedPrice::from_f64(95.0),
            FixedQty::from_f64(2.0),
            4000,
        );
        assert!(o.is_stop());
        o.convert_stop();
        assert_eq!(o.order_type, OrderType::Market);
        assert_eq!(o.status, OrderStatus::Triggered);
    }
}
