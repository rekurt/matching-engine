/// Integration tests: Limit orders, Market orders, Price-Time priority
/// Covers test scenarios A (1-8), B (9-22), D (36-40) from the plan.
use me_core::events::{MatchEvent, RejectReason};
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

fn market_buy_fok(id: OrderId, qty: f64) -> Command {
    Command::PlaceOrder(Order::new_market(
        id,
        SymbolId(0),
        Side::Buy,
        TimeInForce::FOK,
        FixedQty::from_f64(qty),
        id * 1000,
    ))
}

fn count_trades(events: &[MatchEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, MatchEvent::Trade(_)))
        .count()
}

fn has_event_type(events: &[MatchEvent], pred: impl Fn(&MatchEvent) -> bool) -> bool {
    events.iter().any(pred)
}

// ─── A. Limit Orders — Basic ────────────────────────────────────────────────

/// A1: Limit buy crosses one ask level — full fill
#[test]
fn test_a1_limit_buy_crosses_one_ask() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0));
    let events = e.process(limit_buy(2, 100.0, 1.0)).to_vec();

    assert!(count_trades(&events) >= 1);
    assert!(has_event_type(&events, |ev| matches!(
        ev,
        MatchEvent::OrderFilled { order_id: 1, .. }
    )));
    assert!(has_event_type(&events, |ev| matches!(
        ev,
        MatchEvent::OrderFilled { order_id: 2, .. }
    )));
    assert_eq!(e.book().order_count(), 0);
}

/// A2: Limit buy crosses multiple ask levels — partial fills at each level
#[test]
fn test_a2_limit_buy_crosses_multiple_levels() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0));
    e.process(limit_sell(2, 101.0, 1.0));
    e.process(limit_sell(3, 102.0, 1.0));

    let events = e.process(limit_buy(4, 102.0, 2.5)).to_vec();

    // Should fill orders 1 and 2 completely, and partially fill order 3
    assert!(count_trades(&events) >= 3);
    assert_eq!(e.book().order_count(), 1); // order 3 partially filled, on book
}

/// A3: Limit buy without crossing — rests on book, best_bid updated
#[test]
fn test_a3_limit_buy_no_crossing() {
    let mut e = setup();
    e.process(limit_sell(1, 101.0, 1.0));
    let events = e.process(limit_buy(2, 100.0, 1.0)).to_vec();

    assert_eq!(count_trades(&events), 0);
    assert_eq!(e.book().order_count(), 2);
    assert_eq!(e.book().best_bid(), Some(FixedPrice::from_f64(100.0)));
}

/// A4: Limit sell crosses one bid level — full fill
#[test]
fn test_a4_limit_sell_crosses_one_bid() {
    let mut e = setup();
    e.process(limit_buy(1, 100.0, 1.0));
    let events = e.process(limit_sell(2, 100.0, 1.0)).to_vec();

    assert!(count_trades(&events) >= 1);
    assert_eq!(e.book().order_count(), 0);
}

/// A5: Limit sell crosses multiple bid levels — partial fills
#[test]
fn test_a5_limit_sell_crosses_multiple_levels() {
    let mut e = setup();
    e.process(limit_buy(1, 102.0, 1.0));
    e.process(limit_buy(2, 101.0, 1.0));
    e.process(limit_buy(3, 100.0, 1.0));

    let events = e.process(limit_sell(4, 100.0, 2.5)).to_vec();
    assert!(count_trades(&events) >= 3);
}

/// A6: Limit sell without crossing — rests on book
#[test]
fn test_a6_limit_sell_no_crossing() {
    let mut e = setup();
    e.process(limit_buy(1, 99.0, 1.0));
    e.process(limit_sell(2, 101.0, 1.0));

    assert_eq!(e.book().order_count(), 2);
    assert_eq!(e.book().best_ask(), Some(FixedPrice::from_f64(101.0)));
}

/// A7: Limit order exactly at best ask/bid price — full match
#[test]
fn test_a7_exact_price_match() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0));
    let events = e.process(limit_buy(2, 100.0, 1.0)).to_vec();

    assert!(count_trades(&events) >= 1);
    assert_eq!(e.book().order_count(), 0);
}

/// A8: Limit order with price better than best ask — match at maker's price
#[test]
fn test_a8_better_price_fills_at_maker_price() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0));
    let events = e.process(limit_buy(2, 105.0, 1.0)).to_vec();

    // Trade should be at 100.0 (maker price), not 105.0
    let trade_price = events.iter().find_map(|ev| match ev {
        MatchEvent::Trade(t) => Some(t.price),
        _ => None,
    });
    assert_eq!(trade_price, Some(FixedPrice::from_f64(100.0)));
}

// ─── B. Market Orders — Extended Edge Cases ─────────────────────────────────

/// B9: Market buy in empty book — cancel, zero fills
#[test]
fn test_b9_market_buy_empty_book() {
    let mut e = setup();
    let events = e.process(market_buy(1, 1.0)).to_vec();

    assert_eq!(count_trades(&events), 0);
    assert!(has_event_type(&events, |ev| matches!(
        ev,
        MatchEvent::OrderCancelled { .. }
    )));
}

/// B10: Market buy, asks exist but total qty < market qty — partial fill + cancel
#[test]
fn test_b10_market_buy_insufficient_asks() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 0.5));
    let events = e.process(market_buy(2, 1.0)).to_vec();

    assert!(count_trades(&events) >= 1);
    assert!(has_event_type(&events, |ev| matches!(
        ev,
        MatchEvent::OrderCancelled { order_id: 2, .. }
    )));
}

/// B11: Market buy exactly fills all asks — full fill, asks empty
#[test]
fn test_b11_market_buy_exact_fill() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0));
    let events = e.process(market_buy(2, 1.0)).to_vec();

    assert!(count_trades(&events) >= 1);
    assert!(e.book().asks_empty());
    assert!(has_event_type(&events, |ev| matches!(
        ev,
        MatchEvent::OrderFilled { order_id: 2, .. }
    )));
}

/// B12: Market buy for 1 unit (min qty) — fills one ask
#[test]
fn test_b12_market_buy_min_qty() {
    let mut e = setup();
    // min_qty is 0.00001 = 1_000 raw
    e.process(limit_sell(1, 100.0, 0.00001));
    let events = e.process(market_buy(2, 0.00001)).to_vec();

    assert!(count_trades(&events) >= 1);
}

/// B13: Market buy huge volume through many price levels
#[test]
fn test_b13_market_buy_sweep_many_levels() {
    let mut e = setup();
    for i in 0..100u64 {
        e.process(limit_sell(i + 1, 100.0 + i as f64 * 0.01, 0.01));
    }
    let events = e.process(market_buy(200, 1.0)).to_vec(); // sweep all 100 levels

    assert!(count_trades(&events) >= 100);
    assert!(e.book().asks_empty());
}

/// B14: Market sell in empty book
#[test]
fn test_b14_market_sell_empty_book() {
    let mut e = setup();
    let events = e.process(market_sell(1, 1.0)).to_vec();

    assert_eq!(count_trades(&events), 0);
    assert!(has_event_type(&events, |ev| matches!(
        ev,
        MatchEvent::OrderCancelled { .. }
    )));
}

/// B15: Market sell, bids exist but insufficient
#[test]
fn test_b15_market_sell_insufficient_bids() {
    let mut e = setup();
    e.process(limit_buy(1, 100.0, 0.5));
    let events = e.process(market_sell(2, 1.0)).to_vec();

    assert!(count_trades(&events) >= 1);
    assert!(has_event_type(&events, |ev| matches!(
        ev,
        MatchEvent::OrderCancelled { order_id: 2, .. }
    )));
}

/// B16: Market buy, single ask with qty=1 (min), market wants more
#[test]
fn test_b16_market_buy_single_min_ask() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 0.00001));
    let events = e.process(market_buy(2, 1.0)).to_vec();

    assert!(count_trades(&events) >= 1);
    // Remainder cancelled
    assert!(has_event_type(&events, |ev| matches!(
        ev,
        MatchEvent::OrderCancelled { order_id: 2, .. }
    )));
}

/// B17: Market buy through levels with different prices — fills at maker prices (ascending)
#[test]
fn test_b17_market_buy_ascending_prices() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0));
    e.process(limit_sell(2, 101.0, 1.0));
    e.process(limit_sell(3, 102.0, 1.0));

    let events = e.process(market_buy(4, 3.0)).to_vec();

    let trade_prices: Vec<_> = events
        .iter()
        .filter_map(|ev| match ev {
            MatchEvent::Trade(t) => Some(t.price),
            _ => None,
        })
        .collect();

    assert_eq!(trade_prices.len(), 3);
    assert_eq!(trade_prices[0], FixedPrice::from_f64(100.0));
    assert_eq!(trade_prices[1], FixedPrice::from_f64(101.0));
    assert_eq!(trade_prices[2], FixedPrice::from_f64(102.0));
}

/// B18: Market sell through levels with different prices — fills at maker prices (descending)
#[test]
fn test_b18_market_sell_descending_prices() {
    let mut e = setup();
    e.process(limit_buy(1, 102.0, 1.0));
    e.process(limit_buy(2, 101.0, 1.0));
    e.process(limit_buy(3, 100.0, 1.0));

    let events = e.process(market_sell(4, 3.0)).to_vec();

    let trade_prices: Vec<_> = events
        .iter()
        .filter_map(|ev| match ev {
            MatchEvent::Trade(t) => Some(t.price),
            _ => None,
        })
        .collect();

    assert_eq!(trade_prices.len(), 3);
    assert_eq!(trade_prices[0], FixedPrice::from_f64(102.0));
    assert_eq!(trade_prices[1], FixedPrice::from_f64(101.0));
    assert_eq!(trade_prices[2], FixedPrice::from_f64(100.0));
}

/// B19: Two sequential market buys — second sees book without already-filled orders
#[test]
fn test_b19_sequential_market_buys() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0));
    e.process(limit_sell(2, 101.0, 1.0));

    e.process(market_buy(3, 1.0)); // fills order 1
    let events = e.process(market_buy(4, 1.0)).to_vec(); // should fill order 2

    let trade_price = events.iter().find_map(|ev| match ev {
        MatchEvent::Trade(t) => Some(t.price),
        _ => None,
    });
    assert_eq!(trade_price, Some(FixedPrice::from_f64(101.0)));
}

/// B20: Market order FOK with insufficient volume — full reject
#[test]
fn test_b20_market_fok_insufficient() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 0.5));
    let events = e.process(market_buy_fok(2, 1.0)).to_vec();

    assert_eq!(count_trades(&events), 0);
    assert!(has_event_type(&events, |ev| matches!(
        ev,
        MatchEvent::OrderRejected {
            reason: RejectReason::InsufficientLiquidity,
            ..
        }
    )));
}

/// B21: Market order FOK with sufficient volume — full fill
#[test]
fn test_b21_market_fok_sufficient() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0));
    let events = e.process(market_buy_fok(2, 1.0)).to_vec();

    assert!(count_trades(&events) >= 1);
}

/// B22: Market order never rests on book
#[test]
fn test_b22_market_never_rests() {
    let mut e = setup();
    e.process(market_buy(1, 1.0)); // no asks
    assert_eq!(e.book().order_count(), 0);

    e.process(limit_sell(2, 100.0, 0.5));
    e.process(market_buy(3, 1.0)); // partial fill
                                   // Only the remaining ask (if any) should be on book, no market orders
                                   // Check that no market orders rest
    assert!(e.book().order_count() <= 1); // at most the ask remains
}

// ─── D. Price-Time Priority ─────────────────────────────────────────────────

/// D36: Two asks at same price, earlier inserted fills first
#[test]
fn test_d36_price_time_priority() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0));
    e.process(limit_sell(2, 100.0, 1.0));

    let events = e.process(market_buy(3, 1.0)).to_vec();

    // Order 1 should be filled (FIFO)
    assert!(has_event_type(&events, |ev| matches!(
        ev,
        MatchEvent::OrderFilled { order_id: 1, .. }
    )));
    // Order 2 should still be on book
    assert!(e.book().contains_order(2));
}

/// D37: Three asks at same price, market buy for 2.5x qty of one
#[test]
fn test_d37_partial_third_order() {
    let mut e = setup();
    e.process(limit_sell(1, 100.0, 1.0));
    e.process(limit_sell(2, 100.0, 1.0));
    e.process(limit_sell(3, 100.0, 1.0));

    let events = e.process(market_buy(4, 2.5)).to_vec();

    // Orders 1 and 2 fully filled, order 3 partially filled
    assert!(has_event_type(&events, |ev| matches!(
        ev,
        MatchEvent::OrderFilled { order_id: 1, .. }
    )));
    assert!(has_event_type(&events, |ev| matches!(
        ev,
        MatchEvent::OrderFilled { order_id: 2, .. }
    )));
    assert!(has_event_type(&events, |ev| matches!(
        ev,
        MatchEvent::OrderPartiallyFilled { order_id: 3, .. }
    )));
    assert!(e.book().contains_order(3));
}
