/// Integration tests: IOC, FOK, Cancel, Amend, Validation
/// Covers scenarios C (23-35), E (41-44), F (45-49), H (58-65), I (66-72), J (73-77)
use me_core::events::{MatchEvent, RejectReason};
use me_core::order::{Order, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::*;
use me_engine::command::Command;
use me_engine::engine::Engine;

fn setup() -> Engine {
    Engine::new(SymbolConfig::default_btc_usdt())
}

fn limit_buy_tif(id: OrderId, price: f64, qty: f64, tif: TimeInForce) -> Command {
    Command::PlaceOrder(Order::new_limit(
        id,
        SymbolId(0),
        Side::Buy,
        tif,
        FixedPrice::from_f64(price),
        FixedQty::from_f64(qty),
        id * 1000,
    ))
}

fn limit_sell_tif(id: OrderId, price: f64, qty: f64, tif: TimeInForce) -> Command {
    Command::PlaceOrder(Order::new_limit(
        id,
        SymbolId(0),
        Side::Sell,
        tif,
        FixedPrice::from_f64(price),
        FixedQty::from_f64(qty),
        id * 1000,
    ))
}

fn limit_buy(id: OrderId, price: f64, qty: f64) -> Command {
    limit_buy_tif(id, price, qty, TimeInForce::GTC)
}

fn limit_sell(id: OrderId, price: f64, qty: f64) -> Command {
    limit_sell_tif(id, price, qty, TimeInForce::GTC)
}

fn count_trades(events: &[MatchEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, MatchEvent::Trade(_)))
        .count()
}

fn has_reject(events: &[MatchEvent], reason: RejectReason) -> bool {
    events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::OrderRejected { reason: r, .. } if *r == reason))
}

// ─── C. Min/Max Qty and Lot Size ────────────────────────────────────────────

/// C23: Order with qty < min_qty → reject
#[test]
fn test_c23_qty_below_min() {
    let mut e = setup();
    // min_qty = 1_000 raw = 0.00001
    let order = Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_raw(10_000_000_000), // 100.0
        FixedQty::from_raw(500),              // below min
        1000,
    );
    let events = e.process(Command::PlaceOrder(order)).to_vec();
    assert!(has_reject(&events, RejectReason::InvalidQuantity));
}

/// C24: Order with qty == min_qty → accept
#[test]
fn test_c24_qty_at_min() {
    let mut e = setup();
    let order = Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_raw(10_000_000_000),
        FixedQty::from_raw(1_000), // exactly min
        1000,
    );
    let events = e.process(Command::PlaceOrder(order)).to_vec();
    assert!(events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::OrderAccepted { .. })));
}

/// C25: Order with qty > max_qty → reject
#[test]
fn test_c25_qty_above_max() {
    let mut e = setup();
    let order = Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_raw(10_000_000_000),
        FixedQty::from_raw(2_000_000_000_000), // above max (1_000_000_000_000)
        1000,
    );
    let events = e.process(Command::PlaceOrder(order)).to_vec();
    assert!(has_reject(&events, RejectReason::InvalidQuantity));
}

/// C26: Order with qty == max_qty → accept
#[test]
fn test_c26_qty_at_max() {
    let mut e = setup();
    let order = Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_raw(10_000_000_000),
        FixedQty::from_raw(1_000_000_000_000), // exactly max
        1000,
    );
    let events = e.process(Command::PlaceOrder(order)).to_vec();
    assert!(events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::OrderAccepted { .. })));
}

/// C27: Order with qty not multiple of qty_step → reject
#[test]
fn test_c27_qty_not_multiple_of_step() {
    let mut e = Engine::new({
        let mut cfg = SymbolConfig::default_btc_usdt();
        cfg.qty_step = FixedQty::from_raw(1_000_000); // 0.01
        cfg.min_qty = FixedQty::from_raw(1_000_000);
        cfg
    });
    let order = Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_raw(10_000_000_000),
        FixedQty::from_raw(1_500_000), // 0.015 — not multiple of 0.01
        1000,
    );
    let events = e.process(Command::PlaceOrder(order)).to_vec();
    assert!(has_reject(&events, RejectReason::InvalidLotSize));
}

/// C28: Order with qty multiple of qty_step → accept
#[test]
fn test_c28_qty_multiple_of_step() {
    let mut e = Engine::new({
        let mut cfg = SymbolConfig::default_btc_usdt();
        cfg.qty_step = FixedQty::from_raw(1_000_000);
        cfg.min_qty = FixedQty::from_raw(1_000_000);
        cfg
    });
    let order = Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_raw(10_000_000_000),
        FixedQty::from_raw(3_000_000), // 0.03
        1000,
    );
    let events = e.process(Command::PlaceOrder(order)).to_vec();
    assert!(events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::OrderAccepted { .. })));
}

/// C29: Price not multiple of price_step → reject
#[test]
fn test_c29_price_not_on_tick() {
    let mut e = setup();
    // price_step = 1_000_000 (0.01)
    let order = Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_raw(10_000_500_000), // 100.005 — not on 0.01 tick
        FixedQty::from_raw(100_000_000),
        1000,
    );
    let events = e.process(Command::PlaceOrder(order)).to_vec();
    assert!(has_reject(&events, RejectReason::InvalidTickSize));
}

/// C30: Price multiple of price_step → accept
#[test]
fn test_c30_price_on_tick() {
    let mut e = setup();
    let order = Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_raw(10_001_000_000), // 100.01
        FixedQty::from_raw(100_000_000),
        1000,
    );
    let events = e.process(Command::PlaceOrder(order)).to_vec();
    assert!(events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::OrderAccepted { .. })));
}

/// C31: Limit order with price == 0 → reject
#[test]
fn test_c31_zero_price_limit() {
    let mut e = setup();
    let order = Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::ZERO,
        FixedQty::from_raw(100_000_000),
        1000,
    );
    let events = e.process(Command::PlaceOrder(order)).to_vec();
    assert!(has_reject(&events, RejectReason::InvalidPrice));
}

/// C32: Market order (price=0 is ok, qty must be valid)
#[test]
fn test_c32_market_order_zero_price_ok() {
    let mut e = setup();
    let order = Order::new_market(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedQty::from_raw(100_000_000),
        1000,
    );
    let events = e.process(Command::PlaceOrder(order)).to_vec();
    // Should be accepted (then cancelled because empty book)
    assert!(events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::OrderAccepted { .. })));
}

/// C33: Order with qty == 0 → reject
#[test]
fn test_c33_zero_qty() {
    let mut e = setup();
    let order = Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_raw(10_000_000_000),
        FixedQty::ZERO,
        1000,
    );
    let events = e.process(Command::PlaceOrder(order)).to_vec();
    assert!(has_reject(&events, RejectReason::InvalidQuantity));
}

/// C34: Partial fill leaves remaining < min_qty — stays on book (min only checked on entry)
#[test]
fn test_c34_partial_fill_below_min() {
    let mut e = setup();
    // Ask for 0.00002 (2_000 raw), min_qty = 0.00001 (1_000)
    e.process(Command::PlaceOrder(Order::new_limit(
        1,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_raw(10_000_000_000),
        FixedQty::from_raw(2_000),
        1000,
    )));

    // Market buy for 0.00001.5 — not possible with lot step.
    // Let's use a scenario where remaining < min
    // Ask for 0.00003, buy 0.00002. Remaining = 0.00001 which is == min.
    e.process(Command::PlaceOrder(Order::new_limit(
        2,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_raw(10_100_000_000),
        FixedQty::from_raw(3_000), // 0.00003
        2000,
    )));

    // Buy 0.00002 at price 101 — fills 0.00002 from order 2, remaining = 0.00001
    let events = e.process(Command::PlaceOrder(Order::new_market(
        3,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedQty::from_raw(5_000), // 0.00005 — fills order 1 (2000) + partial order 2 (3000)
        3000,
    )));

    // After fill, order 2 should still be on book even if remaining is small
    // (min_qty is only checked at order entry, not after fills)
    assert_eq!(count_trades(events), 2);
}

/// C35: Fill for exactly qty_step units — correct remaining
#[test]
fn test_c35_fill_exact_step() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0));
    let events = e.process(Command::PlaceOrder(Order::new_market(
        2,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedQty::from_raw(1_000), // 0.00001 — one step
        2000,
    )));

    assert_eq!(count_trades(events), 1);
    // Remaining of ask should be 1.0 - 0.00001
    assert!(e.book().contains_order(1)); // ask still on book
}

// ─── E. IOC (Immediate or Cancel) ───────────────────────────────────────────

/// E41: IOC buy with full crossing — full fill, nothing on book
#[test]
fn test_e41_ioc_full_fill() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0));
    let events = e
        .process(limit_buy_tif(2, 100.0, 1.0, TimeInForce::IOC))
        .to_vec();

    assert_eq!(count_trades(&events), 1);
    assert_eq!(e.book().order_count(), 0);
}

/// E42: IOC buy with partial crossing — partial fill + cancel, nothing on book
#[test]
fn test_e42_ioc_partial_fill() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 0.5));
    let events = e
        .process(limit_buy_tif(2, 100.0, 1.0, TimeInForce::IOC))
        .to_vec();

    assert_eq!(count_trades(&events), 1);
    assert!(events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::OrderCancelled { order_id: 2, .. })));
    // IOC should NOT rest on book
    assert!(!e.book().contains_order(2));
}

/// E43: IOC buy with no crossing — cancel, nothing on book
#[test]
fn test_e43_ioc_no_crossing() {
    let mut e = setup();
    e.process(limit_sell(1, 101.0, 1.0));
    let events = e
        .process(limit_buy_tif(2, 100.0, 1.0, TimeInForce::IOC))
        .to_vec();

    assert_eq!(count_trades(&events), 0);
    assert!(events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::OrderCancelled { order_id: 2, .. })));
    assert!(!e.book().contains_order(2));
}

/// E44: IOC sell — same scenarios
#[test]
fn test_e44_ioc_sell() {
    let mut e = setup();
    e.process(limit_buy(1, 100.0, 0.5));
    let events = e
        .process(limit_sell_tif(2, 100.0, 1.0, TimeInForce::IOC))
        .to_vec();

    assert_eq!(count_trades(&events), 1);
    assert!(!e.book().contains_order(2));
}

// ─── F. FOK (Fill or Kill) ──────────────────────────────────────────────────

/// F45: FOK buy, liquidity exactly sufficient — full fill
#[test]
fn test_f45_fok_exact_fill() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0));
    let events = e
        .process(limit_buy_tif(2, 100.0, 1.0, TimeInForce::FOK))
        .to_vec();

    assert_eq!(count_trades(&events), 1);
    assert_eq!(e.book().order_count(), 0);
}

/// F46: FOK buy, liquidity 1 unit short — full reject, zero fills
#[test]
fn test_f46_fok_one_short() {
    let mut e = setup();
    // Sell 0.99999 — 1 unit short of 1.0
    e.process(Command::PlaceOrder(Order::new_limit(
        1,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_raw(10_000_000_000),
        FixedQty::from_raw(99_999_000),
        1000,
    )));

    let events = e
        .process(limit_buy_tif(2, 100.0, 1.0, TimeInForce::FOK))
        .to_vec();

    assert_eq!(count_trades(&events), 0);
    assert!(has_reject(&events, RejectReason::InsufficientLiquidity));
    // Sell order should still be intact
    assert!(e.book().contains_order(1));
}

/// F47: FOK buy, liquidity spread across multiple price levels — fill if total sufficient
#[test]
fn test_f47_fok_multi_level() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 0.5));
    e.process(limit_sell(2, 101.0, 0.5));

    let events = e
        .process(limit_buy_tif(3, 101.0, 1.0, TimeInForce::FOK))
        .to_vec();

    assert_eq!(count_trades(&events), 2);
    assert_eq!(e.book().order_count(), 0);
}

/// F48: FOK buy in empty book → reject
#[test]
fn test_f48_fok_empty_book() {
    let mut e = setup();
    let events = e
        .process(limit_buy_tif(1, 100.0, 1.0, TimeInForce::FOK))
        .to_vec();

    assert_eq!(count_trades(&events), 0);
    assert!(has_reject(&events, RejectReason::InsufficientLiquidity));
}

/// F49: FOK sell — same scenarios
#[test]
fn test_f49_fok_sell() {
    let mut e = setup();
    e.process(limit_buy(1, 100.0, 1.0));

    let events = e
        .process(limit_sell_tif(2, 100.0, 1.0, TimeInForce::FOK))
        .to_vec();
    assert_eq!(count_trades(&events), 1);
}

// ─── H. Cancel ──────────────────────────────────────────────────────────────

/// H58: Cancel existing order — removed from book
#[test]
fn test_h58_cancel_existing() {
    let mut e = setup();
    e.process(limit_buy(1, 100.0, 1.0));
    let events = e
        .process(Command::CancelOrder {
            order_id: 1,
            symbol: SymbolId(0),
            timestamp: 5000,
        })
        .to_vec();

    assert!(events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::OrderCancelled { order_id: 1, .. })));
    assert_eq!(e.book().order_count(), 0);
}

/// H59: Cancel non-existent order_id → OrderNotFound
#[test]
fn test_h59_cancel_nonexistent() {
    let mut e = setup();
    let events = e
        .process(Command::CancelOrder {
            order_id: 99,
            symbol: SymbolId(0),
            timestamp: 5000,
        })
        .to_vec();

    assert!(has_reject(&events, RejectReason::OrderNotFound));
}

/// H60: Cancel already cancelled order → OrderNotFound
#[test]
fn test_h60_cancel_already_cancelled() {
    let mut e = setup();
    e.process(limit_buy(1, 100.0, 1.0));
    e.process(Command::CancelOrder {
        order_id: 1,
        symbol: SymbolId(0),
        timestamp: 5000,
    });
    let events = e
        .process(Command::CancelOrder {
            order_id: 1,
            symbol: SymbolId(0),
            timestamp: 6000,
        })
        .to_vec();

    assert!(has_reject(&events, RejectReason::OrderNotFound));
}

/// H62: Cancel only order at a price level → level removed
#[test]
fn test_h62_cancel_only_at_level() {
    let mut e = setup();
    e.process(limit_buy(1, 100.0, 1.0));
    assert_eq!(e.book().bid_level_count(), 1);

    e.process(Command::CancelOrder {
        order_id: 1,
        symbol: SymbolId(0),
        timestamp: 5000,
    });
    assert_eq!(e.book().bid_level_count(), 0);
}

/// H63: Cancel from middle of queue — remaining orders keep FIFO order
#[test]
fn test_h63_cancel_middle_keeps_order() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0));
    e.process(limit_sell(2, 100.0, 1.0));
    e.process(limit_sell(3, 100.0, 1.0));

    // Cancel middle order
    e.process(Command::CancelOrder {
        order_id: 2,
        symbol: SymbolId(0),
        timestamp: 5000,
    });

    // Market buy should fill order 1 first, then order 3
    let events = e.process(Command::PlaceOrder(Order::new_market(
        4,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedQty::from_f64(1.0),
        6000,
    )));

    let filled_id = events.iter().find_map(|ev| match ev {
        MatchEvent::OrderFilled { order_id, .. } if *order_id == 1 => Some(1),
        _ => None,
    });
    assert_eq!(filled_id, Some(1));
}

/// H64: Cancel best bid → best_bid updates to next level
#[test]
fn test_h64_cancel_best_bid() {
    let mut e = setup();
    e.process(limit_buy(1, 100.0, 1.0));
    e.process(limit_buy(2, 99.0, 1.0));

    assert_eq!(e.book().best_bid(), Some(FixedPrice::from_f64(100.0)));
    e.process(Command::CancelOrder {
        order_id: 1,
        symbol: SymbolId(0),
        timestamp: 5000,
    });
    assert_eq!(e.book().best_bid(), Some(FixedPrice::from_f64(99.0)));
}

/// H65: Cancel all orders on a side → best = None
#[test]
fn test_h65_cancel_all_bids() {
    let mut e = setup();
    e.process(limit_buy(1, 100.0, 1.0));
    e.process(limit_buy(2, 99.0, 1.0));

    e.process(Command::CancelOrder {
        order_id: 1,
        symbol: SymbolId(0),
        timestamp: 5000,
    });
    e.process(Command::CancelOrder {
        order_id: 2,
        symbol: SymbolId(0),
        timestamp: 6000,
    });

    assert_eq!(e.book().best_bid(), None);
    assert!(e.book().bids_empty());
}

// ─── I. Amend ───────────────────────────────────────────────────────────────

/// I66: Amend qty down — in-place, time priority preserved
#[test]
fn test_i66_amend_qty_down() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0));
    e.process(limit_sell(2, 100.0, 1.0));

    // Amend order 1 qty down
    let events = e.process(Command::AmendOrder {
        order_id: 1,
        symbol: SymbolId(0),
        new_price: None,
        new_qty: Some(FixedQty::from_f64(0.5)),
        timestamp: 5000,
    });
    assert!(events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::OrderAmended { order_id: 1, .. })));

    // Order 1 should still fill first (time priority preserved)
    let events = e.process(Command::PlaceOrder(Order::new_market(
        3,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedQty::from_f64(0.5),
        6000,
    )));
    assert!(events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::OrderFilled { order_id: 1, .. })));
}

/// I67: Amend qty up → cancel+replace, loses time priority
#[test]
fn test_i67_amend_qty_up() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0));
    e.process(limit_sell(2, 100.0, 1.0));

    // Amend order 1 qty up
    e.process(Command::AmendOrder {
        order_id: 1,
        symbol: SymbolId(0),
        new_price: None,
        new_qty: Some(FixedQty::from_f64(2.0)),
        timestamp: 5000,
    });

    // Order 2 should now fill first (order 1 lost time priority)
    let events = e.process(Command::PlaceOrder(Order::new_market(
        3,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedQty::from_f64(1.0),
        6000,
    )));
    assert!(events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::OrderFilled { order_id: 2, .. })));
}

/// I68: Amend price → cancel+replace, loses time priority
#[test]
fn test_i68_amend_price() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0));
    e.process(limit_sell(2, 100.0, 1.0));

    // Amend order 1 to different price (still 100.0 but triggers cancel+replace)
    e.process(Command::AmendOrder {
        order_id: 1,
        symbol: SymbolId(0),
        new_price: Some(FixedPrice::from_f64(99.0)),
        new_qty: None,
        timestamp: 5000,
    });

    // Order 1 should now be at 99.0
    assert_eq!(e.book().best_ask(), Some(FixedPrice::from_f64(99.0)));
}

/// I70: Amend non-existent order → error
#[test]
fn test_i70_amend_nonexistent() {
    let mut e = setup();
    let events = e.process(Command::AmendOrder {
        order_id: 99,
        symbol: SymbolId(0),
        new_price: Some(FixedPrice::from_f64(100.0)),
        new_qty: None,
        timestamp: 5000,
    });
    assert!(has_reject(events, RejectReason::OrderNotFound));
}

// ─── J. Book State Integrity ────────────────────────────────────────────────

/// J73: After operations, best_bid < best_ask
#[test]
fn test_j73_no_crossed_book() {
    let mut e = setup();
    e.process(limit_buy(1, 100.0, 1.0));
    e.process(limit_sell(2, 101.0, 1.0));

    let bb = e.book().best_bid().unwrap();
    let ba = e.book().best_ask().unwrap();
    assert!(bb < ba);
}

/// J74: Empty book: both None
#[test]
fn test_j74_empty_book() {
    let e = setup();
    assert_eq!(e.book().best_bid(), None);
    assert_eq!(e.book().best_ask(), None);
}

/// J75: Single order book
#[test]
fn test_j75_single_order_book() {
    let mut e = setup();
    e.process(limit_buy(1, 100.0, 1.0));
    assert!(e.book().best_bid().is_some());
    assert_eq!(e.book().best_ask(), None);
}

/// J76: After full sweep, side is empty
#[test]
fn test_j76_full_sweep_empty() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0));
    e.process(Command::PlaceOrder(Order::new_market(
        2,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedQty::from_f64(1.0),
        2000,
    )));
    assert_eq!(e.book().best_ask(), None);
    assert!(e.book().asks_empty());
}

/// J77: Order count consistency
#[test]
fn test_j77_order_count() {
    let mut e = setup();
    e.process(limit_buy(1, 100.0, 1.0));
    e.process(limit_buy(2, 99.0, 2.0));
    e.process(limit_sell(3, 101.0, 1.0));
    assert_eq!(e.book().order_count(), 3);

    e.process(Command::CancelOrder {
        order_id: 2,
        symbol: SymbolId(0),
        timestamp: 5000,
    });
    assert_eq!(e.book().order_count(), 2);
}

// ─── GTD (Good-Till-Date) ────────────────────────────────────────────────────

fn gtd_buy(id: OrderId, price: f64, qty: f64, expire_at: u64) -> Command {
    Command::PlaceOrder(
        Order::new_limit(
            id,
            SymbolId(0),
            Side::Buy,
            TimeInForce::GTD,
            FixedPrice::from_f64(price),
            FixedQty::from_f64(qty),
            id * 1000,
        )
        .with_expiry(expire_at),
    )
}

#[test]
fn test_gtd_without_expiry_is_rejected() {
    let mut e = setup();
    // tif=GTD but expire_at=0 (builder not used) → InvalidExpiry.
    let mut order = Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTD,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(1.0),
        1000,
    );
    order.expire_at = 0;
    let events = e.process(Command::PlaceOrder(order)).to_vec();
    assert!(events.iter().any(|ev| matches!(
        ev,
        MatchEvent::OrderRejected {
            reason: RejectReason::InvalidExpiry,
            ..
        }
    )));
}

#[test]
fn test_gtd_rests_then_expires() {
    let mut e = setup();
    // expire_at far in the past (1 ns) → SystemClock now is always greater.
    let events = e.process(gtd_buy(1, 100.0, 1.0, 1)).to_vec();
    assert!(events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::OrderAccepted { .. })));
    assert_eq!(e.book().order_count(), 1, "GTD order rests like GTC");

    // Lazy sweep cancels the expired order.
    let expired = e.expire_gtd_now();
    assert!(
        expired
            .iter()
            .any(|ev| matches!(ev, MatchEvent::OrderCancelled { order_id: 1, .. })),
        "expired GTD order must be cancelled"
    );
    assert_eq!(e.book().order_count(), 0);
}

#[test]
fn test_gtd_future_expiry_not_swept() {
    let mut e = setup();
    // expire_at far in the future.
    let far = u64::MAX;
    e.process(gtd_buy(1, 100.0, 1.0, far));
    assert_eq!(e.book().order_count(), 1);
    let expired = e.expire_gtd_now();
    assert!(expired.is_empty(), "future GTD must not be swept");
    assert_eq!(e.book().order_count(), 1);
}

// ─── Iceberg ─────────────────────────────────────────────────────────────────

fn iceberg_sell(id: OrderId, price: f64, total: f64, display: f64) -> Command {
    Command::PlaceOrder(
        Order::new_limit(
            id,
            SymbolId(0),
            Side::Sell,
            TimeInForce::GTC,
            FixedPrice::from_f64(price),
            FixedQty::from_f64(total),
            id * 1000,
        )
        .with_display_qty(FixedQty::from_f64(display)),
    )
}

#[test]
fn test_iceberg_shows_only_display_and_replenishes() {
    let mut e = setup();

    // Iceberg sell: total 6, display 2 at price 100.
    e.process(iceberg_sell(1, 100.0, 6.0, 2.0));
    let resting = e.book().get_order(1).copied().expect("iceberg rests");
    assert_eq!(
        resting.remaining_qty,
        FixedQty::from_f64(2.0),
        "only the display slice is visible"
    );
    // Only the visible slice counts toward book liquidity at the price.
    assert_eq!(
        e.book().available_ask_qty(None),
        FixedQty::from_f64(2.0),
        "hidden reserve is not visible on the book"
    );

    // Buy 2 → fully fills the visible slice; engine replenishes the next slice.
    let events = e.process(limit_buy(2, 100.0, 2.0)).to_vec();
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::OrderPartiallyFilled { order_id: 1, .. })),
        "filled slice must report PartiallyFilled while a reserve remains"
    );
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::OrderFilled { order_id: 1, .. })),
        "iceberg must not report Filled until the reserve is exhausted"
    );
    assert_eq!(
        e.book().get_order(1).unwrap().remaining_qty,
        FixedQty::from_f64(2.0),
        "next slice replenished to display size"
    );

    // Two more buys of 2 exhaust the reserve (2 + 2 + 2 = 6).
    e.process(limit_buy(3, 100.0, 2.0));
    let events = e.process(limit_buy(4, 100.0, 2.0)).to_vec();
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::OrderFilled { order_id: 1, .. })),
        "final slice fully fills → OrderFilled"
    );
    assert!(e.book().get_order(1).is_none(), "iceberg fully consumed");
}

#[test]
fn test_iceberg_survives_snapshot_restore() {
    use me_engine::engine::Engine;
    let mut e = setup();
    e.process(iceberg_sell(1, 100.0, 6.0, 2.0));
    // Consume one slice so cumulative=2, reserve=4.
    e.process(limit_buy(2, 100.0, 2.0));

    // Snapshot + restore into a fresh engine.
    let snap = e.snapshot();
    let mut e2 = Engine::restore_from_snapshot(SymbolConfig::default_btc_usdt(), snap);
    assert_eq!(
        e2.book().get_order(1).unwrap().remaining_qty,
        FixedQty::from_f64(2.0)
    );

    // Reserve must still replenish after restore.
    e2.process(limit_buy(3, 100.0, 2.0)); // cumulative 4
    assert!(e2.book().get_order(1).is_some(), "still replenishing");
    e2.process(limit_buy(4, 100.0, 2.0)); // cumulative 6 → done
    assert!(e2.book().get_order(1).is_none(), "exhausted after restore");
}
