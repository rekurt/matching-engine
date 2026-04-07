/// Phase 4 tests: OpenOrders snapshot via Router::get_open_orders_by_session.
///
/// These tests verify:
/// - Orders tagged with client_session_id are correctly tracked by the engine
/// - Router scans all shards (multi-shard fan-out)
/// - Snapshot returns only this client's orders, not other sessions
/// - After cancel, snapshot no longer includes that order
/// - Filled orders don't appear in snapshot (only resting)
use me_core::order::{Order, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::{FixedPrice, FixedQty, Side, SymbolId};
use me_engine::command::Command;
use me_engine::router::Router;

fn btc_config() -> SymbolConfig {
    SymbolConfig::default_btc_usdt()
}

fn eth_config() -> SymbolConfig {
    let mut c = SymbolConfig::default_btc_usdt();
    c.id = SymbolId(1);
    c.base = "ETH".to_string();
    c
}

fn limit_buy(id: u64, symbol: SymbolId, price: f64, qty: f64, session: me_core::SessionId) -> Command {
    Command::PlaceOrder(
        Order::new_limit(
            id,
            symbol,
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_f64(price),
            FixedQty::from_f64(qty),
            id * 1000,
        )
        .with_session_id(session),
    )
}

fn limit_sell(id: u64, symbol: SymbolId, price: f64, qty: f64, session: me_core::SessionId) -> Command {
    Command::PlaceOrder(
        Order::new_limit(
            id,
            symbol,
            Side::Sell,
            TimeInForce::GTC,
            FixedPrice::from_f64(price),
            FixedQty::from_f64(qty),
            id * 1000,
        )
        .with_session_id(session),
    )
}

// ─── Basic snapshot ──────────────────────────────────────────────────────────

#[test]
fn test_empty_snapshot_for_new_session() {
    let router = Router::new(1);
    let session = me_core::SessionId::from_bytes([0xAAu8; 16]);
    let orders = router.get_open_orders_by_session(session).unwrap();
    assert!(orders.is_empty());
}

#[test]
fn test_snapshot_returns_placed_orders() {
    let mut router = Router::new(1);
    router.add_symbol(btc_config());
    let session = me_core::SessionId::from_bytes([0xAAu8; 16]);

    // Place 3 orders on different price levels
    router.process(limit_buy(1, SymbolId(0), 50_000.0, 1.0, session));
    router.process(limit_buy(2, SymbolId(0), 49_000.0, 2.0, session));
    router.process(limit_buy(3, SymbolId(0), 48_000.0, 3.0, session));

    let snapshot = router.get_open_orders_by_session(session).unwrap();
    assert_eq!(snapshot.len(), 3);

    let ids: std::collections::HashSet<u64> = snapshot.iter().map(|o| o.id).collect();
    assert!(ids.contains(&1));
    assert!(ids.contains(&2));
    assert!(ids.contains(&3));
}

#[test]
fn test_snapshot_isolates_sessions() {
    let mut router = Router::new(1);
    router.add_symbol(btc_config());
    let session_a = me_core::SessionId::from_bytes([0xAAu8; 16]);
    let session_b = me_core::SessionId::from_bytes([0xBBu8; 16]);

    // Session A places 2 orders
    router.process(limit_buy(1, SymbolId(0), 50_000.0, 1.0, session_a));
    router.process(limit_buy(2, SymbolId(0), 49_000.0, 1.0, session_a));

    // Session B places 1 order
    router.process(limit_buy(3, SymbolId(0), 48_000.0, 1.0, session_b));

    let snap_a = router.get_open_orders_by_session(session_a).unwrap();
    let snap_b = router.get_open_orders_by_session(session_b).unwrap();

    assert_eq!(snap_a.len(), 2);
    assert_eq!(snap_b.len(), 1);

    // Verify cross-session leakage doesn't happen
    for o in &snap_a {
        assert_eq!(o.client_session_id, session_a);
    }
    for o in &snap_b {
        assert_eq!(o.client_session_id, session_b);
    }
}

#[test]
fn test_snapshot_after_cancel() {
    let mut router = Router::new(1);
    router.add_symbol(btc_config());
    let session = me_core::SessionId::from_bytes([0xCCu8; 16]);

    router.process(limit_buy(1, SymbolId(0), 50_000.0, 1.0, session));
    router.process(limit_buy(2, SymbolId(0), 49_000.0, 1.0, session));
    router.process(limit_buy(3, SymbolId(0), 48_000.0, 1.0, session));
    assert_eq!(router.get_open_orders_by_session(session).unwrap().len(), 3);

    // Cancel order 2
    router.process(Command::CancelOrder {
        order_id: 2,
        symbol: SymbolId(0),
        timestamp: 5_000,
    });

    let snapshot = router.get_open_orders_by_session(session).unwrap();
    assert_eq!(snapshot.len(), 2);
    let ids: std::collections::HashSet<u64> = snapshot.iter().map(|o| o.id).collect();
    assert!(ids.contains(&1));
    assert!(!ids.contains(&2), "cancelled order must not appear");
    assert!(ids.contains(&3));
}

#[test]
fn test_snapshot_excludes_filled_orders() {
    let mut router = Router::new(1);
    router.add_symbol(btc_config());
    let session = me_core::SessionId::from_bytes([0xDDu8; 16]);

    // Place a sell that will be fully consumed by incoming buy
    router.process(limit_sell(1, SymbolId(0), 50_000.0, 1.0, session));

    // Opposing buy from a different session consumes the full sell
    let other_session = me_core::SessionId::from_bytes([0xEEu8; 16]);
    router.process(limit_buy(2, SymbolId(0), 50_000.0, 1.0, other_session));

    // The resting sell (id=1) should now be filled → not in snapshot
    let snapshot = router.get_open_orders_by_session(session).unwrap();
    assert!(
        snapshot.iter().all(|o| o.id != 1),
        "filled order should not appear in snapshot"
    );
}

// ─── Multi-symbol / multi-shard fan-out ─────────────────────────────────────

#[test]
fn test_snapshot_aggregates_across_symbols() {
    let mut router = Router::new(2); // 2 shards
    router.add_symbol(btc_config());
    router.add_symbol(eth_config());
    let session = me_core::SessionId::from_bytes([0x11u8; 16]);

    // Place on BTC (symbol 0)
    router.process(limit_buy(1, SymbolId(0), 50_000.0, 1.0, session));
    router.process(limit_buy(2, SymbolId(0), 49_000.0, 1.0, session));

    // Place on ETH (symbol 1) — hashed to different shard
    router.process(limit_buy(10, SymbolId(1), 3_000.0, 2.0, session));
    router.process(limit_buy(11, SymbolId(1), 2_900.0, 2.0, session));

    let snapshot = router.get_open_orders_by_session(session).unwrap();
    assert_eq!(snapshot.len(), 4, "should aggregate across both shards");

    let btc_count = snapshot
        .iter()
        .filter(|o| o.symbol == SymbolId(0))
        .count();
    let eth_count = snapshot
        .iter()
        .filter(|o| o.symbol == SymbolId(1))
        .count();
    assert_eq!(btc_count, 2);
    assert_eq!(eth_count, 2);
}

#[test]
fn test_snapshot_partial_fill_shows_remaining() {
    let mut router = Router::new(1);
    router.add_symbol(btc_config());
    let session = me_core::SessionId::from_bytes([0x22u8; 16]);

    // Place sell of 10 BTC
    router.process(limit_sell(1, SymbolId(0), 50_000.0, 10.0, session));

    // Partial fill: buy 3 BTC
    let other = me_core::SessionId::from_bytes([0x33u8; 16]);
    router.process(limit_buy(2, SymbolId(0), 50_000.0, 3.0, other));

    let snapshot = router.get_open_orders_by_session(session).unwrap();
    assert_eq!(snapshot.len(), 1);
    let remaining = snapshot[0].remaining_qty;
    assert_eq!(
        remaining,
        FixedQty::from_f64(7.0),
        "should show 7 BTC remaining after 3 consumed"
    );
}

#[test]
fn test_snapshot_scales_to_many_orders() {
    let mut router = Router::new(1);
    router.add_symbol(btc_config());
    let session = me_core::SessionId::from_bytes([0x44u8; 16]);

    // Place 100 orders at distinct prices
    for i in 1..=100u64 {
        let price = 50_000.0 - (i as f64);
        router.process(limit_buy(i, SymbolId(0), price, 1.0, session));
    }

    let snapshot = router.get_open_orders_by_session(session).unwrap();
    assert_eq!(snapshot.len(), 100);

    // All should have the right session
    for o in &snapshot {
        assert_eq!(o.client_session_id, session);
    }
}

#[test]
fn test_nonexistent_session_returns_empty() {
    let mut router = Router::new(1);
    router.add_symbol(btc_config());

    // Place with one session
    let active = me_core::SessionId::from_bytes([0x55u8; 16]);
    router.process(limit_buy(1, SymbolId(0), 50_000.0, 1.0, active));

    // Query with a different session that has never placed anything
    let fake = me_core::SessionId::from_bytes([0xFFu8; 16]);
    let snapshot = router.get_open_orders_by_session(fake).unwrap();
    assert!(snapshot.is_empty());
}
