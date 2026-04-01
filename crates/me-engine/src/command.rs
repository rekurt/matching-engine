use me_core::order::Order;
use me_core::types::{FixedPrice, FixedQty, OrderId, SymbolId};

/// Commands that can be sent to the matching engine.
#[derive(Clone, Debug)]
pub enum Command {
    /// Place a new order (any type: limit, market, stop-limit, stop-market).
    PlaceOrder(Order),

    /// Cancel an existing order by ID.
    CancelOrder {
        order_id: OrderId,
        symbol: SymbolId,
        timestamp: u64,
    },

    /// Amend an existing order's price and/or quantity.
    AmendOrder {
        order_id: OrderId,
        symbol: SymbolId,
        new_price: Option<FixedPrice>,
        new_qty: Option<FixedQty>,
        timestamp: u64,
    },
}

impl Command {
    /// Get the symbol ID for routing.
    pub fn symbol(&self) -> SymbolId {
        match self {
            Command::PlaceOrder(order) => order.symbol,
            Command::CancelOrder { symbol, .. } => *symbol,
            Command::AmendOrder { symbol, .. } => *symbol,
        }
    }

    /// Get the timestamp.
    pub fn timestamp(&self) -> u64 {
        match self {
            Command::PlaceOrder(order) => order.timestamp,
            Command::CancelOrder { timestamp, .. } => *timestamp,
            Command::AmendOrder { timestamp, .. } => *timestamp,
        }
    }
}
