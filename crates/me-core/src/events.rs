use serde::{Deserialize, Serialize};

use crate::trade::Trade;
use crate::types::{FixedPrice, FixedQty, OrderId};

// ─── RejectReason ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    /// No liquidity on opposite side (market/IOC/FOK).
    NoLiquidity,
    /// FOK order cannot be fully filled.
    InsufficientLiquidity,
    /// Price is invalid (zero for limit, negative, etc.).
    InvalidPrice,
    /// Quantity is invalid (zero, below min, above max).
    InvalidQuantity,
    /// Quantity does not satisfy lot size step.
    InvalidLotSize,
    /// Price does not satisfy tick size step.
    InvalidTickSize,
    /// Order ID not found (for cancel/amend).
    OrderNotFound,
    /// Duplicate order ID.
    DuplicateOrderId,
    /// Symbol not found.
    SymbolNotFound,
    /// Trading is halted (circuit breaker).
    TradingHalted,
    /// Order rejected by risk check.
    RiskCheckFailed,
    /// Post-only order would have taken liquidity (crossed the spread).
    WouldTakeMarket,
    /// Order would match against the submitter's own resting order
    /// (self-trade prevention).
    SelfTradePrevented,
    /// GTD order has a missing or already-past expiry timestamp.
    InvalidExpiry,
    /// Engine internal error (WAL write failure, shard dead, etc.).
    /// Distinct from business-logic rejections — indicates server-side fault.
    InternalError,
}

// ─── MatchEvent ──────────────────────────────────────────────────────────────

/// Events produced by the matching engine during order processing.
/// Collected in a pre-allocated buffer to avoid per-event allocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchEvent {
    OrderAccepted {
        order_id: OrderId,
        timestamp: u64,
    },
    OrderRejected {
        order_id: OrderId,
        reason: RejectReason,
        timestamp: u64,
    },
    Trade(Trade),
    OrderPartiallyFilled {
        order_id: OrderId,
        filled_qty: FixedQty,
        remaining_qty: FixedQty,
        price: FixedPrice,
        timestamp: u64,
    },
    OrderFilled {
        order_id: OrderId,
        filled_qty: FixedQty,
        price: FixedPrice,
        timestamp: u64,
    },
    OrderCancelled {
        order_id: OrderId,
        remaining_qty: FixedQty,
        timestamp: u64,
    },
    OrderAmended {
        order_id: OrderId,
        new_price: FixedPrice,
        new_qty: FixedQty,
        timestamp: u64,
    },
    StopTriggered {
        order_id: OrderId,
        trigger_price: FixedPrice,
        timestamp: u64,
    },
    /// Response to GetOrderStatus — order is resting in the book.
    /// `status` maps to `OrderStatus as u8`: 0=New, 1=PartiallyFilled.
    OrderStatus {
        order_id: OrderId,
        status: u8,
        filled_qty: FixedQty,
        remaining_qty: FixedQty,
        price: FixedPrice,
        timestamp: u64,
    },
    /// Response to GetOrderStatus — order not found in the book.
    OrderNotFound {
        order_id: OrderId,
    },
}

// ─── EventBuffer ─────────────────────────────────────────────────────────────

/// Pre-allocated, reusable buffer for match events.
/// The buffer grows to its high-water mark and stays there.
pub struct EventBuffer {
    events: Vec<MatchEvent>,
}

impl EventBuffer {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            events: Vec::with_capacity(cap),
        }
    }

    #[inline]
    pub fn clear(&mut self) {
        self.events.clear();
    }

    #[inline]
    pub fn push(&mut self, event: MatchEvent) {
        self.events.push(event);
    }

    #[inline]
    pub fn events(&self) -> &[MatchEvent] {
        &self.events
    }

    /// Mutable view of the buffered events. Used by the engine to rewrite a
    /// fully-filled iceberg slice's `OrderFilled` into `OrderPartiallyFilled`
    /// when a hidden reserve remains.
    #[inline]
    pub fn events_mut(&mut self) -> &mut [MatchEvent] {
        &mut self.events
    }

    /// Mutable access to the underlying vector — lets the engine remove and
    /// re-summarize per-slice iceberg status events into a single event.
    #[inline]
    pub fn vec_mut(&mut self) -> &mut Vec<MatchEvent> {
        &mut self.events
    }

    #[inline]
    pub fn drain(&mut self) -> Vec<MatchEvent> {
        std::mem::take(&mut self.events)
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

impl Default for EventBuffer {
    fn default() -> Self {
        Self::with_capacity(256)
    }
}
