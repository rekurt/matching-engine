use serde::{Deserialize, Serialize};

use crate::trade::Trade;
use crate::types::{FixedPrice, FixedQty, OrderId};

// ─── RejectReason ────────────────────────────────────────────────────────────

/// Explicit `#[repr(u8)]` discriminants ARE the wire/audit contract: the io
/// codec sends `reason as u8` and `me-audit` records `reason as u8`, so these
/// numbers must never drift. Pinning them here (rather than relying on
/// declaration order) makes the cast guaranteed-correct by the type system and
/// lets variants be reordered/inserted without silently changing the wire value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum RejectReason {
    /// No liquidity on opposite side (market/IOC/FOK).
    NoLiquidity = 0,
    /// FOK order cannot be fully filled.
    InsufficientLiquidity = 1,
    /// Price is invalid (zero for limit, negative, etc.).
    InvalidPrice = 2,
    /// Quantity is invalid (zero, below min, above max).
    InvalidQuantity = 3,
    /// Quantity does not satisfy lot size step.
    InvalidLotSize = 4,
    /// Price does not satisfy tick size step.
    InvalidTickSize = 5,
    /// Order ID not found (for cancel/amend).
    OrderNotFound = 6,
    /// Duplicate order ID.
    DuplicateOrderId = 7,
    /// Symbol not found.
    SymbolNotFound = 8,
    /// Trading is halted (circuit breaker).
    TradingHalted = 9,
    /// Order rejected by risk check.
    RiskCheckFailed = 10,
    /// Post-only order would have taken liquidity (crossed the spread).
    WouldTakeMarket = 11,
    /// Engine internal error (WAL write failure, shard dead, etc.).
    /// Distinct from business-logic rejections — indicates server-side fault.
    InternalError = 12,
    /// Order would match against the submitter's own resting order
    /// (self-trade prevention).
    SelfTradePrevented = 13,
    /// GTD order has a missing or already-past expiry timestamp.
    InvalidExpiry = 14,
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

#[cfg(test)]
mod tests {
    use super::RejectReason;

    #[test]
    fn reject_reason_wire_values_are_pinned() {
        // These numbers are the wire/audit contract: the io codec and me-audit
        // both emit `reason as u8`. #[repr(u8)] keeps them stable even if a
        // variant is reordered or inserted; this test fails loudly on any drift,
        // which would otherwise desync the server, Go client, and audit log.
        assert_eq!(RejectReason::NoLiquidity as u8, 0);
        assert_eq!(RejectReason::InsufficientLiquidity as u8, 1);
        assert_eq!(RejectReason::InvalidPrice as u8, 2);
        assert_eq!(RejectReason::InvalidQuantity as u8, 3);
        assert_eq!(RejectReason::InvalidLotSize as u8, 4);
        assert_eq!(RejectReason::InvalidTickSize as u8, 5);
        assert_eq!(RejectReason::OrderNotFound as u8, 6);
        assert_eq!(RejectReason::DuplicateOrderId as u8, 7);
        assert_eq!(RejectReason::SymbolNotFound as u8, 8);
        assert_eq!(RejectReason::TradingHalted as u8, 9);
        assert_eq!(RejectReason::RiskCheckFailed as u8, 10);
        assert_eq!(RejectReason::WouldTakeMarket as u8, 11);
        assert_eq!(RejectReason::InternalError as u8, 12);
        assert_eq!(RejectReason::SelfTradePrevented as u8, 13);
        assert_eq!(RejectReason::InvalidExpiry as u8, 14);
    }
}
