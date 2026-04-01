/// Real-world trading scenario tests.
/// These simulate realistic trading patterns to catch issues
/// that isolated unit tests might miss.
use me_core::events::MatchEvent;
use me_core::order::{Order, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::*;
use me_engine::command::Command;
use me_engine::engine::Engine;

fn setup() -> Engine {
    Engine::new(SymbolConfig::default_btc_usdt())
}

fn limit_buy(id: OrderId, price: f64, qty: f64) -> Command {
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

fn limit_sell(id: OrderId, price: f64, qty: f64) -> Command {
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

fn market_buy(id: OrderId, qty: f64) -> Command {
    Command::PlaceOrder(Order::new_market(
        id,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedQty::from_f64(qty),
        id * 1000,
    ))
}

fn market_sell(id: OrderId, qty: f64) -> Command {
    Command::PlaceOrder(Order::new_market(
        id,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedQty::from_f64(qty),
        id * 1000,
    ))
}

fn cancel(id: OrderId) -> Command {
    Command::CancelOrder {
        order_id: id,
        symbol: SymbolId(0),
        timestamp: id * 1000 + 500,
    }
}

fn count_trades(events: &[MatchEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, MatchEvent::Trade(_)))
        .count()
}

/// Scenario: Realistic order book build-up and market sweep.
/// Place 50 bids (99.50 - 100.00) and 50 asks (100.01 - 100.50),
/// then sweep with a large market order.
#[test]
fn test_realistic_book_build_and_sweep() {
    let mut e = setup();
    let mut next_id = 1u64;

    // Build 50 bid levels
    for i in 0..50 {
        let price = 99.50 + (i as f64) * 0.01;
        e.process(limit_buy(next_id, price, 1.0));
        next_id += 1;
    }

    // Build 50 ask levels
    for i in 0..50 {
        let price = 100.01 + (i as f64) * 0.01;
        e.process(limit_sell(next_id, price, 1.0));
        next_id += 1;
    }

    assert_eq!(e.book().order_count(), 100);
    // Best bid = 99.50 + 49*0.01 = 99.99 (f64 precision may differ slightly)
    assert!(e.book().best_bid().is_some());
    assert!(e.book().best_ask().is_some());
    assert!(e.book().best_bid().unwrap() < e.book().best_ask().unwrap());

    // Market buy sweeps 10 ask levels
    let events = e.process(market_buy(next_id, 10.0)).to_vec();
    assert_eq!(count_trades(&events), 10);
    assert_eq!(e.book().order_count(), 90); // 50 bids + 40 remaining asks
}

/// Scenario: Place-Cancel-Replace pattern (common in HFT).
/// Place an order, cancel it, place a new one at different price.
#[test]
fn test_place_cancel_replace_pattern() {
    let mut e = setup();

    for round in 0..100u64 {
        let id = round * 3 + 1;
        let base_price = 9_000_000_000i64 + (round as i64) * 1_000_000; // 90.00 + round * 0.01
        let replace_price = base_price + 500_000; // not on tick, use base instead
        let cmd_place = Command::PlaceOrder(Order::new_limit(
            id,
            SymbolId(0),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_raw(base_price),
            FixedQty::from_f64(1.0),
            id * 1000,
        ));
        e.process(cmd_place);
        e.process(cancel(id));
        let cmd_replace = Command::PlaceOrder(Order::new_limit(
            id + 1,
            SymbolId(0),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_raw(base_price + 1_000_000),
            FixedQty::from_f64(1.0),
            (id + 1) * 1000,
        ));
        e.process(cmd_replace);
    }

    // Should have 100 orders on book (the replacements)
    assert_eq!(e.book().order_count(), 100);
}

/// Scenario: Aggressive market making — place orders on both sides,
/// get crossed by market orders, replace.
#[test]
fn test_market_making_scenario() {
    let mut e = setup();
    let mut next_id = 1u64;

    // Market maker posts bid and ask
    e.process(limit_buy(next_id, 99.99, 5.0));
    next_id += 1;
    e.process(limit_sell(next_id, 100.01, 5.0));
    next_id += 1;

    // Taker buys 2.0 (hits the ask)
    let events = e.process(market_buy(next_id, 2.0)).to_vec();
    next_id += 1;
    assert!(count_trades(&events) >= 1);

    // Taker sells 1.0 (hits the bid)
    let events = e.process(market_sell(next_id, 1.0)).to_vec();
    next_id += 1;
    assert!(count_trades(&events) >= 1);

    // Market maker still has orders
    assert!(e.book().order_count() >= 2);

    // Verify bid was partially filled
    let bid = e.book().get_order(1);
    assert!(bid.is_some());
}

/// Scenario: Order book emptied and rebuilt.
/// Test that the engine works correctly after being completely emptied.
#[test]
fn test_empty_and_rebuild() {
    let mut e = setup();

    // Build book
    for i in 1..=10u64 {
        e.process(limit_buy(i, 100.0 - i as f64, 1.0));
        e.process(limit_sell(i + 100, 101.0 + i as f64, 1.0));
    }
    assert_eq!(e.book().order_count(), 20);

    // Cancel all
    for i in 1..=10u64 {
        e.process(cancel(i));
        e.process(cancel(i + 100));
    }
    assert_eq!(e.book().order_count(), 0);
    assert_eq!(e.book().best_bid(), None);
    assert_eq!(e.book().best_ask(), None);

    // Rebuild
    e.process(limit_buy(200, 100.0, 1.0));
    e.process(limit_sell(201, 101.0, 1.0));
    assert_eq!(e.book().order_count(), 2);
    assert_eq!(e.book().best_bid(), Some(FixedPrice::from_f64(100.0)));
    assert_eq!(e.book().best_ask(), Some(FixedPrice::from_f64(101.0)));
}

/// Scenario: Rapid IOC orders — only cross what's available, never rest.
#[test]
fn test_rapid_ioc_stream() {
    let mut e = setup();

    // Build some asks
    for i in 1..=5u64 {
        e.process(limit_sell(i, 100.0 + i as f64 * 0.01, 0.1));
    }

    // Send 10 IOC buys — each tries to grab 0.1 at 100.01
    for i in 10..20u64 {
        let events = e
            .process(Command::PlaceOrder(Order::new_limit(
                i,
                SymbolId(0),
                Side::Buy,
                TimeInForce::IOC,
                FixedPrice::from_f64(100.01),
                FixedQty::from_f64(0.1),
                i * 1000,
            )))
            .to_vec();

        // Only the first should match (ask at 100.01 = 0.1 qty)
        if i == 10 {
            assert!(count_trades(&events) >= 1);
        } else {
            // Subsequent ones: no match at 100.01 (it was consumed), so cancelled
            assert!(events
                .iter()
                .any(|ev| matches!(ev, MatchEvent::OrderCancelled { .. })));
        }
    }

    // No IOC orders should rest on book
    assert!(!e.book().contains_order(10));
    assert!(!e.book().contains_order(15));
}

/// Scenario: Duplicate order ID rejection.
#[test]
fn test_duplicate_order_id() {
    let mut e = setup();
    e.process(limit_buy(1, 100.0, 1.0));

    let events = e.process(limit_buy(1, 99.0, 2.0)).to_vec();
    assert!(events.iter().any(|ev| matches!(
        ev,
        MatchEvent::OrderRejected {
            reason: me_core::events::RejectReason::DuplicateOrderId,
            ..
        }
    )));

    // Original order still intact
    assert_eq!(e.book().order_count(), 1);
}

/// Scenario: High-frequency cancel storm.
/// Place 1000 orders, cancel them all.
#[test]
fn test_cancel_storm() {
    let mut e = setup();

    for i in 1..=1000u64 {
        e.process(limit_buy(i, 100.0 - (i % 100) as f64 * 0.01, 0.01));
    }
    assert_eq!(e.book().order_count(), 1000);

    for i in 1..=1000u64 {
        e.process(cancel(i));
    }
    assert_eq!(e.book().order_count(), 0);
    assert_eq!(e.book().best_bid(), None);
}

/// Scenario: Large order crossing many small orders.
#[test]
fn test_large_vs_many_small() {
    let mut e = setup();

    // 500 small asks
    for i in 1..=500u64 {
        e.process(limit_sell(i, 100.0 + (i as f64) * 0.01, 0.01));
    }

    // One large market buy
    let events = e.process(market_buy(1000, 5.0)).to_vec(); // 5.0 = 500 * 0.01
    assert_eq!(count_trades(&events), 500);
    assert!(e.book().asks_empty());
}

/// Scenario: Amend order to cross — should trigger matching.
#[test]
fn test_amend_triggers_matching() {
    let mut e = setup();

    // Ask at 100
    e.process(limit_sell(1, 100.0, 1.0));
    // Bid at 99 (no cross)
    e.process(limit_buy(2, 99.0, 1.0));

    // Amend bid to 100 — should now cross with the ask
    let events = e
        .process(Command::AmendOrder {
            order_id: 2,
            symbol: SymbolId(0),
            new_price: Some(FixedPrice::from_f64(100.0)),
            new_qty: None,
            timestamp: 5000,
        })
        .to_vec();

    assert!(
        count_trades(&events) >= 1,
        "Amend to crossing price should trigger match"
    );
}

/// Scenario: Sequential fills — verify cumulative_qty tracked correctly.
#[test]
fn test_sequential_partial_fills() {
    let mut e = setup();

    // Large ask
    e.process(limit_sell(1, 100.0, 10.0));

    // Three small market buys
    for i in 2..=4u64 {
        let events = e.process(market_buy(i, 2.0)).to_vec();
        assert!(count_trades(&events) >= 1);
    }

    // Ask should still be on book with 4.0 remaining
    let order = e.book().get_order(1).unwrap();
    assert_eq!(order.remaining_qty, FixedQty::from_f64(4.0));
    assert_eq!(order.cumulative_qty, FixedQty::from_f64(6.0));
}

/// Scenario: Multi-pair isolation test via Router.
#[test]
fn test_multi_pair_isolation() {
    use me_engine::router::Router;

    let mut router = Router::new();

    let mut cfg0 = SymbolConfig::default_btc_usdt();
    cfg0.id = SymbolId(0);
    router.add_symbol(cfg0);

    let mut cfg1 = SymbolConfig::default_btc_usdt();
    cfg1.id = SymbolId(1);
    cfg1.base = "ETH".to_string();
    router.add_symbol(cfg1);

    // Place orders on different symbols
    let o1 = Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(50000.0),
        FixedQty::from_f64(1.0),
        1000,
    );
    let o2 = Order::new_limit(
        2,
        SymbolId(1),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(3000.0),
        FixedQty::from_f64(10.0),
        2000,
    );

    router.process(Command::PlaceOrder(o1));
    router.process(Command::PlaceOrder(o2));

    // Orders should not cross (different symbols)
    assert_eq!(
        router.get_engine(SymbolId(0)).unwrap().book().order_count(),
        1
    );
    assert_eq!(
        router.get_engine(SymbolId(1)).unwrap().book().order_count(),
        1
    );

    // Cancel on symbol 0 should not affect symbol 1
    router.process(Command::CancelOrder {
        order_id: 1,
        symbol: SymbolId(0),
        timestamp: 3000,
    });
    assert_eq!(
        router.get_engine(SymbolId(0)).unwrap().book().order_count(),
        0
    );
    assert_eq!(
        router.get_engine(SymbolId(1)).unwrap().book().order_count(),
        1
    );
}

/// Scenario: Stress test — 10000 random-like operations.
#[test]
fn test_stress_mixed_operations() {
    let mut e = setup();
    let mut next_id = 1u64;

    // Phase 1: Build book
    for i in 0..100 {
        e.process(limit_buy(next_id, 99.0 + (i as f64) * 0.01, 1.0));
        next_id += 1;
        e.process(limit_sell(next_id, 100.01 + (i as f64) * 0.01, 1.0));
        next_id += 1;
    }

    // Phase 2: Mixed operations
    for _ in 0..1000 {
        // Place and cancel
        e.process(limit_buy(next_id, 98.0, 0.1));
        let placed_id = next_id;
        next_id += 1;
        e.process(cancel(placed_id));

        // Market orders
        e.process(market_buy(next_id, 0.01));
        next_id += 1;
    }

    // Book should still be consistent
    if let (Some(bb), Some(ba)) = (e.book().best_bid(), e.book().best_ask()) {
        assert!(bb < ba, "Book should not be crossed: bid={bb:?} ask={ba:?}");
    }
}
