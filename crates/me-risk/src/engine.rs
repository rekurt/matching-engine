use rustc_hash::FxHashMap;
use tracing::{debug, warn};

use me_core::order::{Order, OrderType};
use me_core::types::{FixedPrice, SymbolId, SCALE_U64};

use crate::config::RiskConfig;
use crate::limits::{RateLimiter, RiskRejectReason};

/// Pre-trade risk engine.
/// Performs all risk checks BEFORE an order reaches the matching engine.
/// Designed to be fast — all checks are O(1).
pub struct RiskEngine {
    configs: FxHashMap<SymbolId, RiskConfig>,
    open_order_counts: FxHashMap<SymbolId, u32>,
    rate_limiters: FxHashMap<SymbolId, RateLimiter>,
}

impl RiskEngine {
    pub fn new() -> Self {
        Self {
            configs: FxHashMap::default(),
            open_order_counts: FxHashMap::default(),
            rate_limiters: FxHashMap::default(),
        }
    }

    /// Register a symbol with its risk config.
    pub fn add_symbol(&mut self, config: RiskConfig) {
        let symbol = config.symbol;
        // `max_position` requires per-account net-position tracking, which does
        // not exist yet. Warn loudly rather than silently ignoring a control an
        // operator may believe is active.
        if !config.max_position.is_zero() {
            warn!(
                symbol = symbol.0,
                "max_position is configured but NOT enforced (no account/position model yet) — control is inactive"
            );
        }
        let rate = config.max_orders_per_second;
        self.rate_limiters.insert(symbol, RateLimiter::new(rate));
        self.open_order_counts.insert(symbol, 0);
        self.configs.insert(symbol, config);
    }

    /// Get mutable access to a symbol's risk config (for admin API).
    pub fn config_mut(&mut self, symbol: SymbolId) -> Option<&mut RiskConfig> {
        self.configs.get_mut(&symbol)
    }

    /// Get read access to a symbol's risk config.
    pub fn config(&self, symbol: SymbolId) -> Option<&RiskConfig> {
        self.configs.get(&symbol)
    }

    /// Update the reference price for a symbol (called after each trade).
    ///
    /// The per-update movement is clamped to one band-width so a burst of
    /// orders cannot "walk" the reference arbitrarily far: each trade can move
    /// the reference by at most `price_band_bps`. Without this, an attacker
    /// could chain orders just inside the band, each fill ratcheting the
    /// reference, to eventually price orders far from the true market.
    pub fn update_reference_price(&mut self, symbol: SymbolId, price: FixedPrice) {
        if let Some(cfg) = self.configs.get_mut(&symbol) {
            if cfg.price_band_bps > 0 && cfg.reference_price.is_positive() {
                let ref_raw = cfg.reference_price.raw();
                let band = ((ref_raw as i128) * (cfg.price_band_bps as i128) / 10_000)
                    .clamp(0, i64::MAX as i128) as i64;
                let lo = ref_raw.saturating_sub(band);
                let hi = ref_raw.saturating_add(band);
                cfg.reference_price = FixedPrice::from_raw(price.raw().clamp(lo, hi));
            } else {
                cfg.reference_price = price;
            }
        }
    }

    /// Notify the risk engine that an order was added to the book.
    pub fn on_order_added(&mut self, symbol: SymbolId) {
        *self.open_order_counts.entry(symbol).or_insert(0) += 1;
    }

    /// Notify the risk engine that an order was removed from the book.
    pub fn on_order_removed(&mut self, symbol: SymbolId) {
        if let Some(count) = self.open_order_counts.get_mut(&symbol) {
            *count = count.saturating_sub(1);
        }
    }

    /// Set the open order count directly (e.g. after recovery).
    pub fn set_open_order_count(&mut self, symbol: SymbolId, count: u32) {
        self.open_order_counts.insert(symbol, count);
    }

    /// Halt trading for a symbol.
    pub fn halt_trading(&mut self, symbol: SymbolId) -> bool {
        if let Some(cfg) = self.configs.get_mut(&symbol) {
            cfg.trading_enabled = false;
            warn!(symbol = symbol.0, "trading HALTED");
            true
        } else {
            false
        }
    }

    /// Resume trading for a symbol.
    pub fn resume_trading(&mut self, symbol: SymbolId) -> bool {
        if let Some(cfg) = self.configs.get_mut(&symbol) {
            cfg.trading_enabled = true;
            debug!(symbol = symbol.0, "trading RESUMED");
            true
        } else {
            false
        }
    }

    /// Check if trading is enabled for a symbol.
    pub fn is_trading_enabled(&self, symbol: SymbolId) -> bool {
        self.configs.get(&symbol).is_some_and(|c| c.trading_enabled)
    }

    /// Perform all pre-trade risk checks on an order.
    /// Returns Ok(()) if the order passes, Err(reason) if rejected.
    pub fn check_order(&mut self, order: &Order) -> Result<(), RiskRejectReason> {
        let symbol = order.symbol;
        let config = match self.configs.get(&symbol) {
            Some(c) => c,
            None => return Ok(()), // No risk config = allow
        };

        // 1. Trading halt check
        if !config.trading_enabled {
            return Err(RiskRejectReason::TradingHalted);
        }

        // 2. Rate limit check
        if let Some(limiter) = self.rate_limiters.get_mut(&symbol) {
            if !limiter.try_acquire() {
                return Err(RiskRejectReason::RateLimitExceeded);
            }
        }

        // 3. Fat finger: max order quantity
        if order.qty > config.max_order_qty {
            return Err(RiskRejectReason::ExceedsMaxOrderQty);
        }

        // 4. Max notional value (price * qty). Market/stop-market orders carry
        //    no price, so estimate their notional from the reference price —
        //    otherwise a fat-finger market order bypasses notional limits.
        let notional_price: Option<u64> = match order.order_type {
            OrderType::Limit | OrderType::StopLimit => Some(order.price.raw().unsigned_abs()),
            OrderType::Market | OrderType::StopMarket => {
                if config.reference_price.is_positive() {
                    Some(config.reference_price.raw().unsigned_abs())
                } else {
                    None
                }
            }
        };
        if let Some(px) = notional_price {
            let notional = (px as u128) * (order.qty.raw() as u128) / SCALE_U64 as u128;
            if notional > config.max_order_notional as u128 {
                return Err(RiskRejectReason::ExceedsMaxNotional);
            }
        }

        // 5. Price band check (limit-priced orders only).
        if config.price_band_bps > 0
            && config.reference_price.is_positive()
            && matches!(order.order_type, OrderType::Limit | OrderType::StopLimit)
        {
            let ref_price = config.reference_price.raw();
            // Clamp band to i64 range so an extreme bps cannot overflow the
            // cast and invert the bounds (which would let everything through).
            let band = ((ref_price as u128) * (config.price_band_bps as u128) / 10_000)
                .min(i64::MAX as u128) as i64;
            let lower = ref_price.saturating_sub(band);
            let upper = ref_price.saturating_add(band);

            if order.price.raw() < lower || order.price.raw() > upper {
                return Err(RiskRejectReason::PriceBandViolation);
            }
        }

        // 6. Max open orders
        let open = self.open_order_counts.get(&symbol).copied().unwrap_or(0);
        if config.max_open_orders > 0 && open >= config.max_open_orders {
            return Err(RiskRejectReason::MaxOpenOrdersExceeded);
        }

        Ok(())
    }

    /// Get all symbol configs (for admin API).
    pub fn all_configs(&self) -> &FxHashMap<SymbolId, RiskConfig> {
        &self.configs
    }
}

impl Default for RiskEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_core::order::{Order, TimeInForce};
    use me_core::types::{FixedPrice, FixedQty, Side, SymbolId};

    fn btc_symbol() -> SymbolId {
        SymbolId(0)
    }

    fn risk_config() -> RiskConfig {
        let mut cfg = RiskConfig::default_for(btc_symbol());
        cfg.reference_price = FixedPrice::from_f64(50000.0);
        cfg.max_order_qty = FixedQty::from_f64(100.0);
        cfg.max_orders_per_second = 1000; // high for tests
        cfg.price_band_bps = 500; // ±5%
        cfg
    }

    fn limit_buy(price: f64, qty: f64) -> Order {
        Order::new_limit(
            1,
            btc_symbol(),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_f64(price),
            FixedQty::from_f64(qty),
            1000,
        )
    }

    #[test]
    fn test_order_passes_all_checks() {
        let mut engine = RiskEngine::new();
        engine.add_symbol(risk_config());
        let order = limit_buy(50000.0, 1.0);
        assert!(engine.check_order(&order).is_ok());
    }

    #[test]
    fn test_trading_halted() {
        let mut engine = RiskEngine::new();
        engine.add_symbol(risk_config());
        engine.halt_trading(btc_symbol());
        let order = limit_buy(50000.0, 1.0);
        assert_eq!(
            engine.check_order(&order),
            Err(RiskRejectReason::TradingHalted)
        );
    }

    #[test]
    fn test_fat_finger_qty() {
        let mut engine = RiskEngine::new();
        engine.add_symbol(risk_config());
        let order = limit_buy(50000.0, 200.0); // exceeds max 100
        assert_eq!(
            engine.check_order(&order),
            Err(RiskRejectReason::ExceedsMaxOrderQty)
        );
    }

    #[test]
    fn test_price_band_violation() {
        let mut engine = RiskEngine::new();
        engine.add_symbol(risk_config()); // ref=50000, band=5%
                                          // Price at 60000 = 20% above reference, should fail
        let order = limit_buy(60000.0, 1.0);
        assert_eq!(
            engine.check_order(&order),
            Err(RiskRejectReason::PriceBandViolation)
        );
    }

    #[test]
    fn test_price_within_band() {
        let mut engine = RiskEngine::new();
        engine.add_symbol(risk_config()); // ref=50000, band=5%
                                          // Price at 52000 = 4% above reference, should pass
        let order = limit_buy(52000.0, 1.0);
        assert!(engine.check_order(&order).is_ok());
    }

    #[test]
    fn test_max_open_orders() {
        let mut engine = RiskEngine::new();
        let mut cfg = risk_config();
        cfg.max_open_orders = 5;
        engine.add_symbol(cfg);

        for _ in 0..5 {
            engine.on_order_added(btc_symbol());
        }

        let order = limit_buy(50000.0, 1.0);
        assert_eq!(
            engine.check_order(&order),
            Err(RiskRejectReason::MaxOpenOrdersExceeded)
        );

        engine.on_order_removed(btc_symbol());
        assert!(engine.check_order(&order).is_ok());
    }

    #[test]
    fn test_market_order_notional_enforced() {
        let mut engine = RiskEngine::new();
        let mut cfg = risk_config(); // ref=50000
        cfg.max_order_notional = 100_000 * SCALE_U64; // $100k cap
        engine.add_symbol(cfg);
        // Market buy of 10 BTC ≈ $500k at ref 50000 → exceeds notional.
        let order = Order::new_market(
            2,
            btc_symbol(),
            Side::Buy,
            TimeInForce::IOC,
            FixedQty::from_f64(10.0),
            1000,
        );
        assert_eq!(
            engine.check_order(&order),
            Err(RiskRejectReason::ExceedsMaxNotional)
        );
    }

    #[test]
    fn test_reference_price_clamped_to_one_band() {
        let mut engine = RiskEngine::new();
        engine.add_symbol(risk_config()); // ref=50000, band=5%
                                          // A trade far above the band should only move the reference by one
                                          // band-width (5% → 52500), not jump to 90000.
        engine.update_reference_price(btc_symbol(), FixedPrice::from_f64(90000.0));
        let ref_after = engine.config(btc_symbol()).unwrap().reference_price;
        assert_eq!(ref_after, FixedPrice::from_f64(52500.0));
    }

    #[test]
    fn test_resume_trading() {
        let mut engine = RiskEngine::new();
        engine.add_symbol(risk_config());
        engine.halt_trading(btc_symbol());
        assert!(!engine.is_trading_enabled(btc_symbol()));
        engine.resume_trading(btc_symbol());
        assert!(engine.is_trading_enabled(btc_symbol()));
        let order = limit_buy(50000.0, 1.0);
        assert!(engine.check_order(&order).is_ok());
    }
}
