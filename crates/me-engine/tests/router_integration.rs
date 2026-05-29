/// Integration tests for the unified Router with Risk + Audit pipeline.
use std::sync::Arc;

use me_audit::async_logger::{AsyncAuditLogger, AuditFormat};
use me_core::events::MatchEvent;
use me_core::order::{Order, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::*;
use me_engine::command::Command;
use me_engine::router::Router;
use me_risk::config::RiskConfig;

fn btc_config() -> SymbolConfig {
    SymbolConfig::default_btc_usdt()
}

fn risk_config_strict() -> RiskConfig {
    let mut cfg = RiskConfig::default_for(SymbolId(0));
    cfg.max_order_qty = FixedQty::from_f64(10.0); // max 10 BTC
    cfg.max_orders_per_second = 1_000_000;
    cfg.reference_price = FixedPrice::from_f64(100.0);
    cfg.price_band_bps = 5000; // ±50%
    cfg
}

fn limit_buy(id: u64, price: f64, qty: f64) -> Command {
    Command::PlaceOrder(Order::new_limit(
        id,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(price),
        FixedQty::from_f64(qty),
        id * 1000,
    ))
}

fn limit_sell(id: u64, price: f64, qty: f64) -> Command {
    Command::PlaceOrder(Order::new_limit(
        id,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(price),
        FixedQty::from_f64(qty),
        id * 1000,
    ))
}

// ─── Phase 3: Router + Risk integration ─────────────────────────────────────

#[test]
fn test_router_risk_rejects_fat_finger() {
    let audit = Arc::new(AsyncAuditLogger::new(1024, AuditFormat::Json));
    let mut router = Router::new_full(1, None, Some(Arc::clone(&audit)));

    let sym_cfg = btc_config();
    router.add_symbol_with_risk(sym_cfg, Some(risk_config_strict()));

    // Order exceeds max_order_qty (10 BTC limit)
    let big_order = Command::PlaceOrder(Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(50.0), // 50 BTC > max 10
        1000,
    ));

    let events = router.process(big_order);
    assert!(events.iter().any(|e| matches!(
        e,
        MatchEvent::OrderRejected {
            reason: me_core::events::RejectReason::RiskCheckFailed,
            ..
        }
    )));
}

#[test]
fn test_router_risk_passes_normal_order() {
    let audit = Arc::new(AsyncAuditLogger::new(1024, AuditFormat::Json));
    let mut router = Router::new_full(1, None, Some(Arc::clone(&audit)));

    let sym_cfg = btc_config();
    router.add_symbol_with_risk(sym_cfg, Some(risk_config_strict()));

    // Normal order within limits
    let events = router.process(limit_buy(1, 100.0, 1.0));
    assert!(events
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderAccepted { .. })));
}

#[test]
fn test_router_risk_and_matching_full_fill() {
    let audit = Arc::new(AsyncAuditLogger::new(1024, AuditFormat::Json));
    let mut router = Router::new_full(1, None, Some(Arc::clone(&audit)));

    router.add_symbol_with_risk(btc_config(), Some(risk_config_strict()));

    // Place a sell
    router.process(limit_sell(1, 100.0, 1.0));

    // Buy crosses → should match
    let events = router.process(limit_buy(2, 100.0, 1.0));
    let has_trade = events.iter().any(|e| matches!(e, MatchEvent::Trade(_)));
    assert!(has_trade, "Expected trade, got: {:?}", events);
}

#[test]
fn test_router_audit_receives_events() {
    let audit = Arc::new(AsyncAuditLogger::new(1024, AuditFormat::Json));
    let mut router = Router::new_full(1, None, Some(Arc::clone(&audit)));

    router.add_symbol_with_risk(btc_config(), Some(risk_config_strict()));

    // Place + fill
    router.process(limit_sell(1, 100.0, 1.0));
    router.process(limit_buy(2, 100.0, 1.0));

    // Give audit consumer thread time to process
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Audit should have received entries (0 dropped)
    assert_eq!(audit.dropped_count(), 0);
}

#[test]
fn test_router_halt_rejects_via_engine() {
    let mut router = Router::new(1);
    router.add_symbol(btc_config());

    // Place an order first (should work)
    let events = router.process(limit_buy(1, 100.0, 1.0));
    assert!(events
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderAccepted { .. })));

    // Halt trading
    assert!(router.halt_symbol(SymbolId(0)));

    // Wait for halt message to be processed by shard
    std::thread::sleep(std::time::Duration::from_millis(10));

    // New order should be rejected with TradingHalted
    let events = router.process(limit_buy(2, 100.0, 1.0));
    assert!(events.iter().any(|e| matches!(
        e,
        MatchEvent::OrderRejected {
            reason: me_core::events::RejectReason::TradingHalted,
            ..
        }
    )));

    // Cancel should still work
    let events = router.process(Command::CancelOrder {
        order_id: 1,
        symbol: SymbolId(0),
        timestamp: 3000,
    });
    assert!(events
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderCancelled { .. })));
}

#[test]
fn test_router_multi_shard_isolation() {
    let mut router = Router::new(4); // 4 shards

    let mut cfg0 = btc_config();
    cfg0.id = SymbolId(0);
    router.add_symbol(cfg0);

    let mut cfg1 = btc_config();
    cfg1.id = SymbolId(1);
    cfg1.base = "ETH".to_string();
    router.add_symbol(cfg1);

    // Place buy on symbol 0
    let o1 = Command::PlaceOrder(Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(1.0),
        1000,
    ));
    let events = router.process(o1);
    assert!(events
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderAccepted { .. })));

    // Place sell on symbol 1 — should NOT cross with symbol 0 buy
    let o2 = Command::PlaceOrder(Order::new_limit(
        2,
        SymbolId(1),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(1.0),
        2000,
    ));
    let events = router.process(o2);
    assert!(events
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderAccepted { .. })));
    assert!(
        !events.iter().any(|e| matches!(e, MatchEvent::Trade(_))),
        "Orders on different symbols should not match"
    );

    // Cancel on symbol 0 should not affect symbol 1
    router.process(Command::CancelOrder {
        order_id: 1,
        symbol: SymbolId(0),
        timestamp: 3000,
    });

    // Symbol 1 order should still exist
    let q = router.query_order(2, SymbolId(1));
    assert!(q
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderStatus { .. })));
}

#[test]
fn test_router_unknown_symbol_rejected() {
    let router = Router::new(1);
    // No symbols registered

    let events = router.process(limit_buy(1, 100.0, 1.0));
    assert!(events.iter().any(|e| matches!(
        e,
        MatchEvent::OrderRejected {
            reason: me_core::events::RejectReason::SymbolNotFound,
            ..
        }
    )));
}

#[test]
fn test_router_cancel_nonexistent_order() {
    let mut router = Router::new(1);
    router.add_symbol(btc_config());

    let events = router.process(Command::CancelOrder {
        order_id: 999,
        symbol: SymbolId(0),
        timestamp: 1000,
    });
    assert!(events.iter().any(|e| matches!(
        e,
        MatchEvent::OrderRejected {
            reason: me_core::events::RejectReason::OrderNotFound,
            ..
        }
    )));
}

#[test]
fn test_router_without_risk_or_audit() {
    // Router works fine without risk config or audit
    let mut router = Router::new(1);
    router.add_symbol(btc_config());

    let events = router.process(limit_buy(1, 100.0, 1.0));
    assert!(events
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderAccepted { .. })));
}

#[test]
fn test_router_query_order() {
    let mut router = Router::new(1);
    router.add_symbol(btc_config());

    router.process(limit_buy(1, 100.0, 1.0));

    let q = router.query_order(1, SymbolId(0));
    assert!(q
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderStatus { .. })));

    // Query non-existent
    let q = router.query_order(999, SymbolId(0));
    assert!(q
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderNotFound { .. })));
}

#[test]
fn test_router_query_iceberg_reports_true_residual() {
    let mut router = Router::new(1);
    router.add_symbol(btc_config());

    // Untouched iceberg sell: total 6, visible slice 2.
    router.process(Command::PlaceOrder(
        Order::new_limit(
            1,
            SymbolId(0),
            Side::Sell,
            TimeInForce::GTC,
            FixedPrice::from_f64(100.0),
            FixedQty::from_f64(6.0),
            1000,
        )
        .with_display_qty(FixedQty::from_f64(2.0)),
    ));

    let q = router.query_order(1, SymbolId(0));
    let status = q.iter().find_map(|ev| match ev {
        MatchEvent::OrderStatus {
            filled_qty,
            remaining_qty,
            ..
        } => Some((*filled_qty, *remaining_qty)),
        _ => None,
    });
    assert_eq!(
        status,
        Some((FixedQty::ZERO, FixedQty::from_f64(6.0))),
        "untouched iceberg must report filled 0 / open 6, not 4 filled / 2 remaining"
    );
}

#[test]
fn test_self_trade_prevention_rejects_same_session_cross() {
    use me_core::events::RejectReason;
    use me_core::SessionId;

    let mut router = Router::new(1);
    let mut risk = RiskConfig::default_for(SymbolId(0));
    risk.max_orders_per_second = 1_000_000;
    risk.self_trade_prevention = true;
    router.add_symbol_with_risk(btc_config(), Some(risk));

    let session_a = SessionId::from_bytes([0xA1u8; 16]);
    let session_b = SessionId::from_bytes([0xB2u8; 16]);

    // Session A rests a sell at 100.
    let mut sell = Order::new_limit(
        1,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(1.0),
        1000,
    );
    sell.client_session_id = session_a;
    router.process(Command::PlaceOrder(sell));

    // Session A buy that crosses its own resting sell → rejected by STP.
    let mut self_buy = Order::new_limit(
        2,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(1.0),
        2000,
    );
    self_buy.client_session_id = session_a;
    let events = router.process(Command::PlaceOrder(self_buy));
    assert!(
        events.iter().any(|e| matches!(
            e,
            MatchEvent::OrderRejected {
                reason: RejectReason::SelfTradePrevented,
                ..
            }
        )),
        "same-session crossing order must be rejected by STP"
    );
    assert!(
        !events.iter().any(|e| matches!(e, MatchEvent::Trade(_))),
        "no trade should occur on self-trade"
    );

    // Session B buy crossing the same resting sell → trades normally.
    let mut other_buy = Order::new_limit(
        3,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(1.0),
        3000,
    );
    other_buy.client_session_id = session_b;
    let events = router.process(Command::PlaceOrder(other_buy));
    assert!(
        events.iter().any(|e| matches!(e, MatchEvent::Trade(_))),
        "different-session order must trade"
    );
}

#[test]
fn test_expired_gtd_swept_before_open_order_risk_check() {
    // With max_open_orders reached by an already-expired GTD order, the lazy
    // expiry sweep must run BEFORE the pre-trade risk check, freeing the slot,
    // so the next order is not spuriously rejected by stale (expired) liquidity.
    let mut rcfg = RiskConfig::default_for(SymbolId(0));
    rcfg.max_open_orders = 1;
    rcfg.max_orders_per_second = 1_000_000;
    rcfg.max_order_qty = FixedQty::from_f64(10.0);
    rcfg.reference_price = FixedPrice::from_f64(50.0);
    rcfg.price_band_bps = 5000; // ±50%

    let mut router = Router::new(1);
    router.add_symbol_with_risk(btc_config(), Some(rcfg));

    // GTD buy with an already-past expiry → rests; open-order count = 1 (= max).
    let gtd = Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTD,
        FixedPrice::from_f64(50.0),
        FixedQty::from_f64(1.0),
        1000,
    )
    .with_expiry(1);
    let e1 = router.process(Command::PlaceOrder(gtd));
    assert!(e1
        .iter()
        .any(|ev| matches!(ev, MatchEvent::OrderAccepted { order_id: 1, .. })));

    // The slot is occupied by an expired order; placing another must sweep it
    // first and then accept, not reject for max_open_orders.
    let e2 = router.process(limit_buy(2, 49.0, 1.0));
    assert!(
        e2.iter()
            .any(|ev| matches!(ev, MatchEvent::OrderAccepted { order_id: 2, .. })),
        "expired GTD must be swept before the open-order risk check"
    );
    assert!(
        !e2.iter().any(|ev| matches!(
            ev,
            MatchEvent::OrderRejected {
                reason: me_core::events::RejectReason::RiskCheckFailed,
                ..
            }
        )),
        "must not be rejected by an already-expired order's stale count"
    );
}
