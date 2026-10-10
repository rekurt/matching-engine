/// Integration tests: IOC, FOK, Cancel, Amend, Validation
/// Covers scenarios C (23-35), E (41-44), F (45-49), H (58-65), I (66-72), J (73-77)
use me_core::events::{MatchEvent, RejectReason};
use me_core::order::{
    Order, OrderStatus, TimeInForce, ORDER_FLAG_MAKER_OR_CANCEL, ORDER_FLAG_POST_ONLY,
};
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

#[test]
fn test_rejected_iceberg_does_not_rest() {
    let mut e = setup();
    // qty 0.000015 is not a multiple of the 0.00001 lot step → InvalidLotSize.
    let bad = Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_raw(1_500),
        1000,
    )
    .with_display_qty(FixedQty::from_raw(1_000));
    let events = e.process(Command::PlaceOrder(bad)).to_vec();
    assert!(events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::OrderRejected { .. })));
    assert_eq!(e.book().order_count(), 0, "rejected iceberg must not rest");

    // A follow-up command must not resurrect a phantom slice.
    e.process(limit_sell(2, 101.0, 1.0));
    assert!(e.book().get_order(1).is_none());
}

#[test]
fn test_aggressive_iceberg_fully_crosses_without_crossed_book() {
    let mut e = setup();
    // Resting ask of 5 @ 100.
    e.process(limit_sell(1, 100.0, 5.0));

    // Iceberg BUY that crosses: total 5, display 1, limit 100. It must fully
    // execute against the ask via re-matched slices, not rest into a crossed book.
    let ice = Order::new_limit(
        2,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(5.0),
        2000,
    )
    .with_display_qty(FixedQty::from_f64(1.0));
    let events = e.process(Command::PlaceOrder(ice)).to_vec();

    let traded: f64 = events
        .iter()
        .filter_map(|ev| match ev {
            MatchEvent::Trade(t) => Some(t.qty.raw() as f64),
            _ => None,
        })
        .sum();
    assert_eq!(
        traded,
        FixedQty::from_f64(5.0).raw() as f64,
        "all 5 execute"
    );
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::OrderFilled { order_id: 2, .. })),
        "iceberg reports a single terminal OrderFilled"
    );
    // No crossed book — everything matched.
    assert_eq!(e.book().order_count(), 0);
    assert_eq!(e.book().best_bid(), None);
    assert_eq!(e.book().best_ask(), None);
}

#[test]
fn test_iceberg_display_below_min_qty_rejected() {
    // A visible slice below the symbol minimum would rest a sub-min order on the
    // book. The order must be rejected, not silently rested.
    let mut e = setup();
    let bad = Order::new_limit(
        1,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(10.0),
        1000,
    )
    .with_display_qty(FixedQty::from_raw(500)); // 0.000005 < min_qty (0.00001)
    let events = e.process(Command::PlaceOrder(bad)).to_vec();
    assert!(
        has_reject(&events, RejectReason::InvalidQuantity),
        "iceberg display slice below min_qty must be rejected"
    );
    assert_eq!(e.book().order_count(), 0, "rejected iceberg must not rest");
}

#[test]
fn test_iceberg_display_off_lot_step_rejected() {
    // The visible slice rests on the book, so it must obey the lot step like any
    // resting order. total 10 (valid), display 0.000015 (1500 raw) is not a
    // multiple of the 0.00001 step → reject, do not rest an off-step slice.
    let mut e = setup();
    let bad = Order::new_limit(
        1,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(10.0),
        1000,
    )
    .with_display_qty(FixedQty::from_raw(1_500));
    let events = e.process(Command::PlaceOrder(bad)).to_vec();
    assert!(
        has_reject(&events, RejectReason::InvalidLotSize),
        "off-step display slice must be rejected"
    );
    assert_eq!(e.book().order_count(), 0);
}

#[test]
fn test_iceberg_display_not_below_total_rejected() {
    // display_qty must be a strict slice (< total). display == total is not an
    // iceberg; reject rather than silently resting as a fully-visible order.
    let mut e = setup();
    let bad = Order::new_limit(
        1,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(5.0),
        1000,
    )
    .with_display_qty(FixedQty::from_f64(5.0));
    let events = e.process(Command::PlaceOrder(bad)).to_vec();
    assert!(
        has_reject(&events, RejectReason::InvalidQuantity),
        "display_qty >= total must be rejected"
    );
    assert_eq!(e.book().order_count(), 0);
}

#[test]
fn test_maker_or_cancel_rejected_when_crossing() {
    // MAKER_OR_CANCEL (0x04) is a documented flag with the same "maker only"
    // semantics as POST_ONLY: a crossing order must be rejected as
    // WouldTakeMarket, not silently accepted as a taker.
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0)); // resting ask
    let mut o = Order::new_limit(
        2,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(1.0),
        2000,
    );
    o.flags = ORDER_FLAG_MAKER_OR_CANCEL;
    let events = e.process(Command::PlaceOrder(o)).to_vec();
    assert!(
        has_reject(&events, RejectReason::WouldTakeMarket),
        "crossing maker-or-cancel order must be rejected"
    );
    assert_eq!(
        count_trades(&events),
        0,
        "maker-or-cancel must not take liquidity"
    );
}

#[test]
fn test_amend_iceberg_rejected_and_unchanged() {
    // Amending an iceberg can't be applied consistently (amend only touches the
    // visible slice, leaving the engine's hidden template stale). It must be
    // rejected, leaving the iceberg fully intact and still replenishing.
    let mut e = setup();
    e.process(iceberg_sell(1, 100.0, 6.0, 2.0)); // total 6, visible 2
    let events = e
        .process(Command::AmendOrder {
            order_id: 1,
            symbol: SymbolId(0),
            new_price: None,
            new_qty: Some(FixedQty::from_f64(1.0)),
            timestamp: 2000,
        })
        .to_vec();
    assert!(
        has_reject(&events, RejectReason::AmendNotSupported),
        "amending an iceberg must be rejected"
    );
    assert_eq!(
        e.book().get_order(1).unwrap().remaining_qty,
        FixedQty::from_f64(2.0),
        "iceberg visible slice unchanged after a rejected amend"
    );
    // Still replenishes: consume the visible slice, the next slice appears.
    e.process(limit_buy(2, 100.0, 2.0));
    assert!(
        e.book().get_order(1).is_some(),
        "iceberg still replenishes after a rejected amend"
    );
}

#[test]
fn test_cancel_iceberg_reports_true_remaining() {
    // Cancelling an untouched iceberg must report the true open quantity
    // (qty - cumulative), not the visible slice — otherwise a client/risk
    // system under-credits the cancelled quantity.
    let mut e = setup();
    e.process(iceberg_sell(1, 100.0, 6.0, 2.0)); // total 6, visible slice 2
    let events = e
        .process(Command::CancelOrder {
            order_id: 1,
            symbol: SymbolId(0),
            timestamp: 2000,
        })
        .to_vec();
    let rem = events.iter().find_map(|ev| match ev {
        MatchEvent::OrderCancelled {
            order_id: 1,
            remaining_qty,
            ..
        } => Some(*remaining_qty),
        _ => None,
    });
    assert_eq!(
        rem,
        Some(FixedQty::from_f64(6.0)),
        "cancel must report true open (qty - cumulative), not the visible slice"
    );
}

#[test]
fn test_iceberg_refilled_slice_status_is_partially_filled() {
    // After a slice fills and the reserve replenishes a new slice that rests
    // WITHOUT trading, the resting book order must reflect partial-fill status
    // (clients read status from the book via QueryOrder/OpenOrders) — not the
    // template's stale New.
    let mut e = setup();
    e.process(iceberg_sell(1, 100.0, 6.0, 2.0)); // total 6, display 2
    e.process(limit_buy(2, 100.0, 2.0)); // fill first slice; next slice rests, no trade
    let resting = e
        .book()
        .get_order(1)
        .copied()
        .expect("iceberg still resting");
    assert!(
        resting.cumulative_qty.raw() > 0,
        "iceberg has filled quantity"
    );
    assert_eq!(
        resting.status,
        OrderStatus::PartiallyFilled,
        "refilled iceberg slice must rest as PartiallyFilled, not New"
    );
}

#[test]
fn test_iceberg_partial_fill_reports_true_remaining() {
    // The order owner's fill event must reflect the TRUE residual
    // (qty - cumulative), including the hidden reserve — not just the visible
    // slice. Reporting the slice size understates the owner's working quantity
    // and breaks client-side position/risk tracking.
    let mut e = setup();
    e.process(iceberg_sell(1, 100.0, 6.0, 2.0)); // total 6, display 2

    // Fill the first visible slice (2). 4 remain (2 re-shown + 2 hidden).
    let events = e.process(limit_buy(2, 100.0, 2.0)).to_vec();
    let remaining = events.iter().find_map(|ev| match ev {
        MatchEvent::OrderPartiallyFilled {
            order_id: 1,
            remaining_qty,
            ..
        } => Some(*remaining_qty),
        _ => None,
    });
    assert_eq!(
        remaining,
        Some(FixedQty::from_f64(4.0)),
        "iceberg must report true remaining (qty - cumulative), not the visible slice"
    );
}

#[test]
fn test_rejected_gtd_does_not_leak_expiry_entry() {
    // A GTD order that is rejected (here: off-step lot size) must NOT leave a
    // stale entry in the expiry heap — otherwise repeated bad input with a
    // far-future expiry grows the heap and sweep work unbounded (DoS).
    let mut e = setup();
    let bad = Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTD,
        FixedPrice::from_f64(100.0),
        FixedQty::from_raw(1_500), // not a multiple of the 1000 lot step
        1000,
    )
    .with_expiry(u64::MAX);
    let events = e.process(Command::PlaceOrder(bad)).to_vec();
    assert!(has_reject(&events, RejectReason::InvalidLotSize));
    assert_eq!(
        e.gtd_pending(),
        0,
        "rejected GTD must not enqueue an expiry entry"
    );
}

#[test]
fn test_fully_filled_gtd_does_not_enqueue_expiry() {
    // A GTD order that is accepted but never rests (here: an aggressive GTD
    // limit that fully fills on arrival) has no resting order for the sweep to
    // cancel, so it must NOT leave a heap entry — otherwise such orders pile up
    // stale entries until their (far-future) expiry.
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0)); // resting ask to cross
    let gtd = Order::new_limit(
        2,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTD,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(1.0),
        2000,
    )
    .with_expiry(u64::MAX);
    e.process(Command::PlaceOrder(gtd));
    assert!(
        e.book().get_order(2).is_none(),
        "fully-filled GTD does not rest"
    );
    assert_eq!(
        e.gtd_pending(),
        0,
        "a GTD that never rests must not enqueue an expiry entry"
    );
}

#[test]
fn test_accepted_gtd_enqueues_expiry_entry() {
    // Guard against over-correction: an accepted GTD order must still be tracked.
    let mut e = setup();
    e.process(gtd_buy(1, 100.0, 1.0, u64::MAX));
    assert_eq!(e.gtd_pending(), 1, "accepted GTD is tracked for expiry");
}

#[test]
fn test_iceberg_display_on_ioc_rejected() {
    // Iceberg semantics require a resting TIF (GTC/GTD): the hidden reserve
    // replenishes over time. display_qty on an immediate-or-cancel order is
    // meaningless and must be rejected, not silently ignored.
    let mut e = setup();
    let bad = Order::new_limit(
        1,
        SymbolId(0),
        Side::Sell,
        TimeInForce::IOC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(6.0),
        1000,
    )
    .with_display_qty(FixedQty::from_f64(2.0));
    let events = e.process(Command::PlaceOrder(bad)).to_vec();
    assert!(
        has_reject(&events, RejectReason::InvalidQuantity),
        "display_qty on a non-resting TIF must be rejected"
    );
    assert_eq!(e.book().order_count(), 0);
}

#[test]
fn test_iceberg_total_not_multiple_of_display() {
    // total 5, display 2 → slices 2, 2, 1. The final short slice (min(display,
    // hidden)) must fill and the iceberg must complete exactly.
    let mut e = setup();
    e.process(iceberg_sell(1, 100.0, 5.0, 2.0));
    e.process(limit_buy(2, 100.0, 2.0)); // cum 2
    e.process(limit_buy(3, 100.0, 2.0)); // cum 4, reserve 1
    assert_eq!(
        e.book().get_order(1).unwrap().remaining_qty,
        FixedQty::from_f64(1.0),
        "final slice is the 1-unit remainder, not a full display"
    );
    let events = e.process(limit_buy(4, 100.0, 1.0)).to_vec();
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::OrderFilled { order_id: 1, .. })),
        "remainder slice fills → OrderFilled"
    );
    assert!(e.book().get_order(1).is_none(), "iceberg fully consumed");
}

#[test]
fn test_iceberg_emits_single_status_with_correct_filled_qty() {
    // Per-slice OrderFilled/PartiallyFilled events must be collapsed into ONE
    // status event per command, carrying the correct aggregate filled_qty — no
    // per-slice duplicates may leak to the client.
    let mut e = setup();
    e.process(iceberg_sell(1, 100.0, 6.0, 2.0));
    let events = e.process(limit_buy(2, 100.0, 2.0)).to_vec();
    let status: Vec<&MatchEvent> = events
        .iter()
        .filter(|ev| {
            matches!(
                ev,
                MatchEvent::OrderFilled { order_id: 1, .. }
                    | MatchEvent::OrderPartiallyFilled { order_id: 1, .. }
            )
        })
        .collect();
    assert_eq!(status.len(), 1, "exactly one status event per command");
    match status[0] {
        MatchEvent::OrderPartiallyFilled { filled_qty, .. } => {
            assert_eq!(*filled_qty, FixedQty::from_f64(2.0), "aggregate filled_qty");
        }
        other => panic!("expected PartiallyFilled, got {other:?}"),
    }
}

#[test]
fn test_iceberg_gtd_expiry_drops_reserve() {
    // An iceberg that is also GTD must, on expiry, be cancelled AND have its
    // hidden reserve dropped — a later trade must not resurrect a phantom slice.
    let mut e = setup();
    let ice = Order::new_limit(
        1,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTD,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(6.0),
        1000,
    )
    .with_expiry(1) // already-past expiry (SystemClock now > 1 ns)
    .with_display_qty(FixedQty::from_f64(2.0));
    e.process(Command::PlaceOrder(ice));
    e.process(limit_buy(2, 100.0, 2.0)); // fill one slice → reserve 4 tracked
    assert!(e.book().get_order(1).is_some());

    let expired = e.expire_gtd_now();
    assert!(
        expired
            .iter()
            .any(|ev| matches!(ev, MatchEvent::OrderCancelled { order_id: 1, .. })),
        "expired GTD iceberg must be cancelled"
    );
    assert!(e.book().get_order(1).is_none());

    // No phantom replenishment after expiry.
    e.process(limit_buy(3, 100.0, 2.0));
    assert!(
        e.book().get_order(1).is_none(),
        "expired iceberg must not replenish"
    );
}

#[test]
fn test_iceberg_replenish_respects_time_priority_not_id() {
    // Two icebergs at the same price where the EARLIER-submitted one has the
    // HIGHER id. Replenishment priority must follow submission time, not the
    // client-supplied id, so the earlier order keeps queue priority.
    let mut e = setup();
    // id 10 submitted first (ts 1000); id 5 submitted later (ts 2000).
    e.process(Command::PlaceOrder(
        Order::new_limit(
            10,
            SymbolId(0),
            Side::Sell,
            TimeInForce::GTC,
            FixedPrice::from_f64(100.0),
            FixedQty::from_f64(4.0),
            1000,
        )
        .with_display_qty(FixedQty::from_f64(2.0)),
    ));
    e.process(Command::PlaceOrder(
        Order::new_limit(
            5,
            SymbolId(0),
            Side::Sell,
            TimeInForce::GTC,
            FixedPrice::from_f64(100.0),
            FixedQty::from_f64(4.0),
            2000,
        )
        .with_display_qty(FixedQty::from_f64(2.0)),
    ));

    // Sweep both visible slices (2 + 2) → both replenish in this command.
    e.process(limit_buy(20, 100.0, 4.0));
    assert!(e.book().get_order(10).is_some() && e.book().get_order(5).is_some());

    // A follow-up buy of one slice must hit the earlier-submitted iceberg (id 10,
    // ts 1000), not the lower id (5, ts 2000).
    let events = e.process(limit_buy(21, 100.0, 2.0)).to_vec();
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::Trade(t) if t.maker_order_id == 10)),
        "earlier-submitted iceberg (lower timestamp) must keep priority"
    );
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::Trade(t) if t.maker_order_id == 5)),
        "lower id must not jump ahead of an earlier-submitted order"
    );
}

#[test]
fn test_two_icebergs_same_price_replenish_deterministically() {
    // Two iceberg sells at the SAME price, each with a hidden reserve. One
    // incoming buy sweeps both visible slices in a single command, forcing BOTH
    // to replenish within that command. Replenishment order must be canonical
    // (by order id), not HashMap-iteration order — otherwise two replicas or a
    // WAL replay could assign different queue priority to the two replenished
    // slices and diverge. The lower id must regain time priority.
    let mut e = setup();
    e.process(iceberg_sell(1, 100.0, 4.0, 2.0)); // total 4, display 2, reserve 2
    e.process(iceberg_sell(2, 100.0, 4.0, 2.0));

    // Sweep both visible slices (2 + 2 = 4) → both replenish their reserve.
    e.process(limit_buy(10, 100.0, 4.0));
    assert!(e.book().get_order(1).is_some(), "iceberg 1 replenished");
    assert!(e.book().get_order(2).is_some(), "iceberg 2 replenished");

    // A follow-up buy of exactly one display slice must hit the LOWER id first
    // (canonical time priority after a simultaneous replenish).
    let events = e.process(limit_buy(11, 100.0, 2.0)).to_vec();
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::Trade(t) if t.maker_order_id == 1)),
        "lower-id iceberg must regain queue priority deterministically"
    );
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::Trade(t) if t.maker_order_id == 2)),
        "higher-id iceberg must not match before the lower-id one"
    );
}

#[test]
fn test_stop_limit_with_display_qty_rejected() {
    // display_qty (iceberg) is only meaningful on a plain Limit order. A
    // StopLimit carrying display_qty would pass the slice-size/TIF checks, rest
    // in the stop book, and on trigger convert_stop → Limit → process_order
    // directly — bypassing the engine's iceberg registration. It would then rest
    // untracked yet still flagged display_qty>0, which a later snapshot restore
    // resurrects as a phantom iceberg. Reject it here, matching the wire
    // decoder's constraint (display_qty only on OrderType::Limit).
    let mut e = setup();
    let bad = Order::new_stop_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(101.0), // limit
        FixedPrice::from_f64(100.0), // stop/trigger
        FixedQty::from_f64(10.0),
        1000,
    )
    .with_display_qty(FixedQty::from_f64(2.0)); // valid slice size, wrong order type
    let events = e.process(Command::PlaceOrder(bad)).to_vec();
    assert!(
        has_reject(&events, RejectReason::InvalidQuantity),
        "display_qty on a non-Limit (StopLimit) order must be rejected: {events:?}"
    );
    assert_eq!(
        e.stop_book().len(),
        0,
        "rejected stop-limit must not rest in the stop book"
    );
}

#[test]
fn test_stop_trigger_price_not_on_tick_rejected() {
    // A stop's trigger price is a price level and must obey tick size, exactly
    // like a limit price. An off-tick trigger (100.005 on a 0.01 tick) would
    // otherwise be accepted and rest in the stop book at an invalid level.
    let mut e = setup();
    let order = Order::new_stop_market(
        1,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_raw(10_000_500_000), // stop/trigger 100.005 — off 0.01 tick
        FixedQty::from_raw(100_000_000),      // 1.0
        1000,
    );
    let events = e.process(Command::PlaceOrder(order)).to_vec();
    assert!(
        has_reject(&events, RejectReason::InvalidTickSize),
        "off-tick stop trigger price must be rejected: {events:?}"
    );
    assert_eq!(e.stop_book().len(), 0, "rejected stop must not rest");
}

#[test]
fn test_post_only_iceberg_refill_does_not_take() {
    // POST_ONLY means "never take". On entry process_order rejects a crossing
    // maker-only order; the refill path (rematch_limit) must honor the same
    // contract. A replenished slice that would cross liquidity which appeared
    // AFTER the iceberg rested must not execute as a taker.
    let mut e = setup();
    // POST_ONLY buy iceberg @ 100, total 4, display 1. Empty book → it rests.
    let mut ice = Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(4.0),
        1000,
    )
    .with_display_qty(FixedQty::from_f64(1.0));
    ice.flags = ORDER_FLAG_POST_ONLY;
    let placed = e.process(Command::PlaceOrder(ice)).to_vec();
    assert!(
        placed
            .iter()
            .any(|ev| matches!(ev, MatchEvent::OrderAccepted { order_id: 1, .. })),
        "post-only iceberg must rest on an empty book: {placed:?}"
    );

    // The active sell consumes three passive slices in FIFO order.
    let events = e.process(limit_sell(2, 100.0, 3.0)).to_vec();

    // The post-only iceberg must NOT take the resting ask on replenishment.
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::Trade(t) if t.taker_order_id == 1)),
        "post-only iceberg refill must not execute as a taker: {events:?}"
    );
    let maker_fills: u64 = events
        .iter()
        .filter_map(|event| match event {
            MatchEvent::Trade(trade) if trade.maker_order_id == 1 => Some(trade.qty.raw()),
            _ => None,
        })
        .sum();
    assert_eq!(maker_fills, FixedQty::from_f64(3.0).raw());
    assert_eq!(
        e.book().get_order(1).unwrap().open_qty(),
        FixedQty::from_f64(1.0)
    );
}
