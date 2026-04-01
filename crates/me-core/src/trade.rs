use serde::{Deserialize, Serialize};

use crate::types::{FixedPrice, FixedQty, OrderId, SequenceId, Side, SymbolId, TradeId};

/// A completed trade between two orders.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[repr(C)]
pub struct Trade {
    pub trade_id: TradeId,
    pub symbol: SymbolId,
    pub price: FixedPrice,
    pub qty: FixedQty,
    pub maker_order_id: OrderId,
    pub taker_order_id: OrderId,
    pub maker_side: Side,
    pub timestamp: u64,
    pub sequence: SequenceId,
}
