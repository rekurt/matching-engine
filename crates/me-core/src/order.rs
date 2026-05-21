use serde::{Deserialize, Serialize};

use crate::session::SessionId;
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
    /// Good Till Date — rests like GTC but is auto-cancelled once
    /// `Order.expire_at` (unix nanoseconds) is reached.
    GTD = 3,
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

/// Core order struct. 104 bytes.
///
/// ## Layout rationale
/// Fields ordered to minimize padding. Hot-path fields (price/qty/status)
/// come first. The 16-byte `client_session_id` lives at the end — it's only
/// touched at submission time (codec decode) and at snapshot time (OpenOrders
/// scan), NEVER in the matching fast path. Placing it last keeps the critical
/// fields cache-hot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(C)]
pub struct Order {
    pub id: OrderId,
    pub price: FixedPrice,
    pub stop_price: FixedPrice,
    pub qty: FixedQty,
    pub remaining_qty: FixedQty,
    pub cumulative_qty: FixedQty,
    /// GTD expiry as unix nanoseconds. `0` means no expiry (GTC/IOC/FOK).
    pub expire_at: u64,
    /// Trailing-stop trail distance (price units). `0` means a fixed stop.
    /// When non-zero on a stop order, `stop_price` ratchets toward the market
    /// as it moves favorably, staying `trail_offset` away from the extreme.
    pub trail_offset: FixedPrice,
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
    /// `SessionId::NONE` indicates "no session" (internal tests, fixtures).
    pub client_session_id: SessionId,
}

/// Compile-time size guard: if someone adds a field that changes Order layout,
/// the docstring above says "104 bytes" — this fails the build loudly instead
/// of silently drifting. Update both this assert and the docstring together.
const _: () = assert!(
    std::mem::size_of::<Order>() == 104,
    "Order struct size changed — update docstring and this assert"
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
            expire_at: 0,
            trail_offset: FixedPrice::ZERO,
            timestamp,
            sequence: 0,
            symbol,
            side,
            order_type: OrderType::Limit,
            tif,
            status: OrderStatus::New,
            flags: 0,
            _pad: 0,
            client_session_id: SessionId::NONE,
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
            expire_at: 0,
            trail_offset: FixedPrice::ZERO,
            timestamp,
            sequence: 0,
            symbol,
            side,
            order_type: OrderType::Market,
            tif,
            status: OrderStatus::New,
            flags: 0,
            _pad: 0,
            client_session_id: SessionId::NONE,
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
            expire_at: 0,
            trail_offset: FixedPrice::ZERO,
            timestamp,
            sequence: 0,
            symbol,
            side,
            order_type: OrderType::StopLimit,
            tif,
            status: OrderStatus::New,
            flags: 0,
            _pad: 0,
            client_session_id: SessionId::NONE,
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
            expire_at: 0,
            trail_offset: FixedPrice::ZERO,
            timestamp,
            sequence: 0,
            symbol,
            side,
            order_type: OrderType::StopMarket,
            tif,
            status: OrderStatus::New,
            flags: 0,
            _pad: 0,
            client_session_id: SessionId::NONE,
        }
    }

    /// Builder: associate this order with a client session for later lookup
    /// by OpenOrders snapshot. Returns self for chaining.
    ///
    /// Used by the codec when decoding client-submitted orders.
    #[inline]
    pub fn with_session_id(mut self, session_id: SessionId) -> Self {
        self.client_session_id = session_id;
        self
    }

    /// Builder: set the GTD expiry (unix nanoseconds) and mark the order GTD.
    #[inline]
    pub fn with_expiry(mut self, expire_at: u64) -> Self {
        self.expire_at = expire_at;
        self.tif = TimeInForce::GTD;
        self
    }

    /// Builder: make this stop order a trailing stop with the given trail
    /// distance (price units). Only meaningful on stop orders.
    #[inline]
    pub fn with_trail_offset(mut self, trail_offset: FixedPrice) -> Self {
        self.trail_offset = trail_offset;
        self
    }

    /// Whether this is a GTD order carrying an expiry.
    #[inline]
    pub fn is_gtd(&self) -> bool {
        matches!(self.tif, TimeInForce::GTD)
    }

    /// Whether this is a trailing stop (a stop order with a non-zero trail).
    #[inline]
    pub fn is_trailing_stop(&self) -> bool {
        self.is_stop() && self.trail_offset.raw() > 0
    }

    /// Whether this order has expired at the given time (unix nanoseconds).
    /// Non-GTD orders and GTD orders with `expire_at == 0` never expire.
    #[inline]
    pub fn is_expired_at(&self, now_ns: u64) -> bool {
        self.is_gtd() && self.expire_at != 0 && now_ns >= self.expire_at
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
    fn test_gtd_expiry_helpers() {
        let o = Order::new_limit(
            1,
            SymbolId(0),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_f64(100.0),
            FixedQty::from_f64(1.0),
            1000,
        )
        .with_expiry(5_000);
        assert!(o.is_gtd());
        assert_eq!(o.tif, TimeInForce::GTD);
        assert_eq!(o.expire_at, 5_000);
        assert!(!o.is_expired_at(4_999));
        assert!(o.is_expired_at(5_000));
        assert!(o.is_expired_at(6_000));

        // Non-GTD orders never expire.
        let g = Order::new_limit(
            2,
            SymbolId(0),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_f64(100.0),
            FixedQty::from_f64(1.0),
            1000,
        );
        assert!(!g.is_gtd());
        assert!(!g.is_expired_at(u64::MAX));
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
