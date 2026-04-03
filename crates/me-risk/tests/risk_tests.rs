//! Integration tests for me-risk: rate limiting, position limits, counter reset.

use me_core::order::{Order, TimeInForce};
use me_core::types::{FixedPrice, FixedQty, Side, SymbolId};
use me_risk::config::RiskConfig;
use me_risk::engine::RiskEngine;
use me_risk::limits::RiskRejectReason;

fn sym(id: u16) -> SymbolId {
    SymbolId(id)
}

fn base_config(symbol: SymbolId) -> RiskConfig {
    let mut cfg = RiskConfig::default_for(symbol);
    cfg.reference_price = FixedPrice::from_f64(100.0);
    cfg.max_orders_per_second = 1000; // effectively unlimited for most tests
    cfg.price_band_bps = 0; // disabled unless test sets it
    cfg.max_open_orders = 0; // unlimited unless test sets it
    cfg
}

fn limit_buy(symbol: SymbolId, price: f64, qty: f64) -> Order {
    Order::new_limit(
        1,
        symbol,
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(price),
        FixedQty::from_f64(qty),
        1000,
    )
}

fn limit_sell(symbol: SymbolId, price: f64, qty: f64) -> Order {
    Order::new_limit(
        2,
        symbol,
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(price),
        FixedQty::from_f64(qty),
        1000,
    )
}

// ---------- Rate limiting tests ----------

#[test]
fn rate_limit_exceeded_returns_error() {
    let s = sym(1);
    let mut engine = RiskEngine::new();
    let mut cfg = base_config(s);
    // capacity = 2: first two calls succeed, third fails immediately
    cfg.max_orders_per_second = 2;
    engine.add_symbol(cfg);

    let order = limit_buy(s, 100.0, 1.0);
    assert!(engine.check_order(&order).is_ok(), "1st call should pass");
    assert!(engine.check_order(&order).is_ok(), "2nd call should pass");
    assert_eq!(
        engine.check_order(&order),
        Err(RiskRejectReason::RateLimitExceeded),
        "3rd call must be rejected"
    );
}

#[test]
fn rate_limit_counter_resets_after_window() {
    use std::time::Duration;

    let s = sym(2);
    let mut engine = RiskEngine::new();
    let mut cfg = base_config(s);
    // Use rate=1 so a single order drains the bucket immediately.
    cfg.max_orders_per_second = 1;
    engine.add_symbol(cfg);

    let order = limit_buy(s, 100.0, 1.0);
    assert!(engine.check_order(&order).is_ok(), "1st call should pass");
    assert_eq!(
        engine.check_order(&order),
        Err(RiskRejectReason::RateLimitExceeded),
        "2nd immediate call should fail"
    );

    // Sleep 2 seconds to let the token-bucket refill one token (2× buffer for CI reliability).
    std::thread::sleep(Duration::from_millis(2000));

    assert!(
        engine.check_order(&order).is_ok(),
        "after 1.1s window, one token must be available again"
    );
}

#[test]
fn rate_limit_different_symbols_are_independent() {
    let s1 = sym(10);
    let s2 = sym(11);
    let mut engine = RiskEngine::new();

    let mut cfg1 = base_config(s1);
    cfg1.max_orders_per_second = 1; // tight limit on s1
    engine.add_symbol(cfg1);

    let mut cfg2 = base_config(s2);
    cfg2.max_orders_per_second = 100; // generous limit on s2
    engine.add_symbol(cfg2);

    let o1 = limit_buy(s1, 100.0, 1.0);
    let o2 = limit_buy(s2, 100.0, 1.0);

    assert!(engine.check_order(&o1).is_ok());
    // s1 exhausted
    assert_eq!(engine.check_order(&o1), Err(RiskRejectReason::RateLimitExceeded));
    // s2 still fine
    assert!(engine.check_order(&o2).is_ok());
    assert!(engine.check_order(&o2).is_ok());
}

// ---------- Open order count / position limit tests ----------

#[test]
fn open_order_count_exceeds_limit_rejects_new_order() {
    let s = sym(3);
    let mut engine = RiskEngine::new();
    let mut cfg = base_config(s);
    cfg.max_open_orders = 3;
    engine.add_symbol(cfg);

    // Fill up to limit
    for _ in 0..3 {
        engine.on_order_added(s);
    }

    let order = limit_buy(s, 100.0, 1.0);
    assert_eq!(
        engine.check_order(&order),
        Err(RiskRejectReason::MaxOpenOrdersExceeded)
    );
}

#[test]
fn open_order_count_decrements_on_removal_allows_new_order() {
    let s = sym(4);
    let mut engine = RiskEngine::new();
    let mut cfg = base_config(s);
    cfg.max_open_orders = 2;
    engine.add_symbol(cfg);

    engine.on_order_added(s);
    engine.on_order_added(s);

    let order = limit_buy(s, 100.0, 1.0);
    // At limit — must fail
    assert_eq!(
        engine.check_order(&order),
        Err(RiskRejectReason::MaxOpenOrdersExceeded)
    );

    // Remove one order from the book
    engine.on_order_removed(s);

    // Now should succeed
    assert!(engine.check_order(&order).is_ok());
}

#[test]
fn open_order_count_does_not_underflow_below_zero() {
    let s = sym(5);
    let mut engine = RiskEngine::new();
    engine.add_symbol(base_config(s));

    // Remove when count is already 0 — must not panic or wrap
    engine.on_order_removed(s);
    engine.on_order_removed(s);

    let order = limit_buy(s, 100.0, 1.0);
    assert!(engine.check_order(&order).is_ok());
}

#[test]
fn set_open_order_count_sets_counter_directly() {
    let s = sym(6);
    let mut engine = RiskEngine::new();
    let mut cfg = base_config(s);
    cfg.max_open_orders = 5;
    engine.add_symbol(cfg);

    // Simulate recovery: jump counter to limit
    engine.set_open_order_count(s, 5);

    let order = limit_buy(s, 100.0, 1.0);
    assert_eq!(
        engine.check_order(&order),
        Err(RiskRejectReason::MaxOpenOrdersExceeded)
    );

    // Reduce counter through recovery and verify orders pass
    engine.set_open_order_count(s, 4);
    assert!(engine.check_order(&order).is_ok());
}

// ---------- Max order quantity (fat finger) ----------

#[test]
fn exceeds_max_order_qty_returns_error() {
    let s = sym(7);
    let mut engine = RiskEngine::new();
    let mut cfg = base_config(s);
    cfg.max_order_qty = FixedQty::from_f64(10.0);
    engine.add_symbol(cfg);

    assert_eq!(
        engine.check_order(&limit_buy(s, 100.0, 11.0)),
        Err(RiskRejectReason::ExceedsMaxOrderQty)
    );
    assert!(engine.check_order(&limit_buy(s, 100.0, 10.0)).is_ok());
}

// ---------- Max notional value ----------

#[test]
fn exceeds_max_notional_returns_error() {
    let s = sym(8);
    let mut engine = RiskEngine::new();
    let mut cfg = base_config(s);
    // SCALE = 100_000_000 (8 decimals)
    // price=100.0  → raw = 10_000_000_000
    // qty=6.0      → raw =    600_000_000
    // notional = (10_000_000_000 * 600_000_000) / 100_000_000 = 60_000_000_000  ($600)
    // qty=4.0      → raw =    400_000_000
    // notional = (10_000_000_000 * 400_000_000) / 100_000_000 = 40_000_000_000  ($400)
    // Cap between $400 and $600 → use $500 = 50_000_000_000 raw
    cfg.max_order_qty = FixedQty::from_f64(1000.0);
    cfg.max_order_notional = 50_000_000_000; // $500 in raw units
    engine.add_symbol(cfg);

    // 100 * 6 = $600 notional > $500 cap
    assert_eq!(
        engine.check_order(&limit_buy(s, 100.0, 6.0)),
        Err(RiskRejectReason::ExceedsMaxNotional)
    );
    // 100 * 4 = $400 notional <= $500 cap
    assert!(engine.check_order(&limit_buy(s, 100.0, 4.0)).is_ok());
}

// ---------- Price band ----------

#[test]
fn price_band_violation_outside_band_returns_error() {
    let s = sym(9);
    let mut engine = RiskEngine::new();
    let mut cfg = base_config(s);
    cfg.reference_price = FixedPrice::from_f64(100.0);
    cfg.price_band_bps = 500; // ±5%
    engine.add_symbol(cfg);

    // 110 is +10% from 100 → outside 5% band
    assert_eq!(
        engine.check_order(&limit_buy(s, 110.0, 1.0)),
        Err(RiskRejectReason::PriceBandViolation)
    );
    // 90 is -10% from 100 → outside 5% band
    assert_eq!(
        engine.check_order(&limit_sell(s, 90.0, 1.0)),
        Err(RiskRejectReason::PriceBandViolation)
    );
    // 104 is +4% from 100 → inside band
    assert!(engine.check_order(&limit_buy(s, 104.0, 1.0)).is_ok());
}

#[test]
fn price_band_disabled_when_bps_zero() {
    let s = sym(12);
    let mut engine = RiskEngine::new();
    let mut cfg = base_config(s);
    cfg.reference_price = FixedPrice::from_f64(100.0);
    cfg.price_band_bps = 0; // disabled
    engine.add_symbol(cfg);

    // Any price should pass when band is disabled
    assert!(engine.check_order(&limit_buy(s, 999.0, 1.0)).is_ok());
    assert!(engine.check_order(&limit_buy(s, 1.0, 1.0)).is_ok());
}

#[test]
fn reference_price_update_shifts_band_center() {
    let s = sym(13);
    let mut engine = RiskEngine::new();
    let mut cfg = base_config(s);
    cfg.reference_price = FixedPrice::from_f64(100.0);
    cfg.price_band_bps = 500; // ±5%
    engine.add_symbol(cfg);

    // 200 is way outside band around 100
    assert_eq!(
        engine.check_order(&limit_buy(s, 200.0, 1.0)),
        Err(RiskRejectReason::PriceBandViolation)
    );

    // After a trade at 200, reference moves to 200
    engine.update_reference_price(s, FixedPrice::from_f64(200.0));

    // Now 200 is within ±5% of 200
    assert!(engine.check_order(&limit_buy(s, 200.0, 1.0)).is_ok());
}

// ---------- Unknown symbol ----------

#[test]
fn unknown_symbol_passes_all_checks() {
    let mut engine = RiskEngine::new();
    // Don't add any symbol config — per design, no config = allow
    let order = limit_buy(sym(99), 100.0, 1.0);
    assert!(engine.check_order(&order).is_ok());
}
