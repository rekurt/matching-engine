use serde::{Deserialize, Serialize};

use crate::invariant::InvariantPolicy;
use crate::types::{FixedPrice, FixedQty, SymbolId};

/// Configuration for a trading pair.
/// Defines precision, limits, and step sizes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SymbolConfig {
    pub id: SymbolId,
    /// Base asset name (e.g. "BTC").
    pub base: String,
    /// Quote asset name (e.g. "USDT").
    pub quote: String,
    /// Number of decimal places for price (e.g. 2 means tick = 0.01).
    pub price_scale: u8,
    /// Number of decimal places for quantity (e.g. 8 means step = 0.00000001).
    pub qty_scale: u8,
    /// Minimum order quantity.
    pub min_qty: FixedQty,
    /// Maximum order quantity.
    pub max_qty: FixedQty,
    /// Lot size step — quantity must be a multiple of this.
    pub qty_step: FixedQty,
    /// Tick size — price must be a multiple of this.
    pub price_step: FixedPrice,
    /// How the matching engine reacts to internal invariant violations
    /// for this symbol. See [`InvariantPolicy`] for the full rationale.
    /// Defaults to `Strict` so that tests and dev builds panic loudly.
    #[serde(default)]
    pub invariant_policy: InvariantPolicy,
}

impl SymbolConfig {
    /// Create a default config suitable for testing (BTC/USDT like).
    pub fn default_btc_usdt() -> Self {
        Self {
            id: SymbolId(0),
            base: "BTC".to_string(),
            quote: "USDT".to_string(),
            price_scale: 2,
            qty_scale: 8,
            min_qty: FixedQty::from_raw(1_000), // 0.00001
            max_qty: FixedQty::from_raw(1_000_000_000_000), // 10000.0
            qty_step: FixedQty::from_raw(1_000), // 0.00001
            price_step: FixedPrice::from_raw(1_000_000), // 0.01
            invariant_policy: InvariantPolicy::default(),
        }
    }

    /// Validate a price against this symbol's tick size and range.
    pub fn validate_price(&self, price: FixedPrice) -> bool {
        if !price.is_positive() {
            return false;
        }
        if self.price_step.raw() == 0 {
            return true;
        }
        price.raw() % self.price_step.raw() == 0
    }

    /// Validate a quantity against this symbol's lot size, min, and max.
    pub fn validate_qty(&self, qty: FixedQty) -> bool {
        if qty.is_zero() {
            return false;
        }
        if qty < self.min_qty || qty > self.max_qty {
            return false;
        }
        qty.is_multiple_of(self.qty_step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_price() {
        let cfg = SymbolConfig::default_btc_usdt();
        // tick = 0.01 = 1_000_000 raw
        assert!(cfg.validate_price(FixedPrice::from_raw(10_000_000_000))); // 100.00
        assert!(cfg.validate_price(FixedPrice::from_raw(1_000_000))); // 0.01
        assert!(!cfg.validate_price(FixedPrice::ZERO));
        assert!(!cfg.validate_price(FixedPrice::from_raw(500_000))); // 0.005 not on tick
    }

    #[test]
    fn test_validate_qty() {
        let cfg = SymbolConfig::default_btc_usdt();
        assert!(cfg.validate_qty(FixedQty::from_raw(1_000))); // min qty
        assert!(cfg.validate_qty(FixedQty::from_raw(100_000_000))); // 1.0
        assert!(!cfg.validate_qty(FixedQty::ZERO));
        assert!(!cfg.validate_qty(FixedQty::from_raw(500))); // below min
    }

    #[test]
    fn test_validate_qty_max() {
        let cfg = SymbolConfig::default_btc_usdt();
        assert!(cfg.validate_qty(cfg.max_qty));
        assert!(!cfg.validate_qty(FixedQty::from_raw(cfg.max_qty.raw() + 1_000)));
    }

    #[test]
    fn test_validate_qty_lot_size() {
        let mut cfg = SymbolConfig::default_btc_usdt();
        cfg.qty_step = FixedQty::from_raw(1_000_000); // 0.01
        cfg.min_qty = FixedQty::from_raw(1_000_000);
        assert!(cfg.validate_qty(FixedQty::from_raw(3_000_000))); // 0.03 ok
        assert!(!cfg.validate_qty(FixedQty::from_raw(1_500_000))); // 0.015 not on lot
    }
}
