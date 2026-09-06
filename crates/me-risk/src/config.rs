use me_core::types::{FixedPrice, FixedQty, SymbolId};
use serde::{Deserialize, Serialize};

/// Risk configuration for a single trading pair.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RiskConfig {
    pub symbol: SymbolId,

    /// Maximum single order quantity (fat finger protection).
    pub max_order_qty: FixedQty,

    /// Maximum single order notional value (price * qty) in quote currency.
    /// Prevents accidentally large orders.
    pub max_order_notional: u64, // raw fixed-point

    /// Maximum net position per account. 0 = unlimited.
    /// Unsupported until an account ledger exists; nonzero configuration is rejected.
    pub max_position: FixedQty,

    /// Price band: maximum % deviation from reference price.
    /// Stored as basis points (1 = 0.01%). 0 = disabled.
    pub price_band_bps: u32,

    /// Reference price for price band checks. Updated on each trade.
    pub reference_price: FixedPrice,

    /// Maximum orders and amendments per second per symbol. 0 = disabled.
    pub max_orders_per_second: u32,

    /// Maximum open orders per symbol.
    pub max_open_orders: u32,

    /// Whether trading is enabled for this symbol.
    pub trading_enabled: bool,

    /// Self-trade prevention: if true, reject (cancel-newest) any incoming
    /// order that would match against a resting order from the same client
    /// session. Enforced by the engine via `Engine::set_self_trade_prevention`.
    pub self_trade_prevention: bool,
}

impl RiskConfig {
    /// Create a default permissive config for testing.
    pub fn default_for(symbol: SymbolId) -> Self {
        Self {
            symbol,
            max_order_qty: FixedQty::from_raw(1_000_000_000_000), // 10000.0
            max_order_notional: 10_000_000_000_000_000,           // $100M
            max_position: FixedQty::ZERO,                         // unlimited
            price_band_bps: 1000,                                 // ±10%
            reference_price: FixedPrice::ZERO,
            max_orders_per_second: 100,
            max_open_orders: 10_000,
            trading_enabled: true,
            self_trade_prevention: false,
        }
    }
}
