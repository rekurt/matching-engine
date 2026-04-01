/// Integration tests: Stop orders (G50-G57)

use me_core::events::{MatchEvent, RejectReason};
use me_core::order::{Order, OrderType, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::*;
use me_engine::command::Command;
use me_engine::engine::Engine;

fn setup() -> Engine {
    Engine::new(SymbolConfig::default_btc_usdt())
}

fn limit_buy(id: OrderId, price: f64, qty: f64) -> Command {
    Command::PlaceOrder(Order::new_limit(
        id, SymbolId(0), Side::Buy, TimeInForce::GTC,
        FixedPrice::from_f64(price), FixedQty::from_f64(qty), id * 1000,
    ))
}

fn limit_sell(id: OrderId, price: f64, qty: f64) -> Command {
    Command::PlaceOrder(Order::new_limit(
        id, SymbolId(0), Side::Sell, TimeInForce::GTC,
        FixedPrice::from_f64(price), FixedQty::from_f64(qty), id * 1000,
    ))
}

fn count_trades(events: &[MatchEvent]) -> usize {
    events.iter().filter(|e| matches!(e, MatchEvent::Trade(_))).count()
}

/// G50: Stop-limit buy triggers on price rise → converts to limit + matches
#[test]
fn test_g50_stop_limit_buy_trigger() {
    let mut e = setup();

    // Place stop-limit buy: trigger at 100, limit at 101
    let stop = Order::new_stop_limit(
        1, SymbolId(0), Side::Buy, TimeInForce::GTC,
        FixedPrice::from_f64(101.0),
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(1.0),
        1000,
    );
    e.process(Command::PlaceOrder(stop));

    // Place an ask at 100
    e.process(limit_sell(2, 100.0, 2.0));

    // Place a buy that trades at 100 — this should trigger the stop
    let events = e.process(limit_buy(3, 100.0, 1.0)).to_vec();

    // Check for stop triggered event
    let has_trigger = events.iter().any(|ev| matches!(ev, MatchEvent::StopTriggered { order_id: 1, .. }));
    assert!(has_trigger, "Stop order should have been triggered");
}

/// G51: Stop-market sell triggers on price drop
#[test]
fn test_g51_stop_market_sell_trigger() {
    let mut e = setup();

    // Place stop-market sell: trigger at 95
    let stop = Order::new_stop_market(
        1, SymbolId(0), Side::Sell, TimeInForce::GTC,
        FixedPrice::from_f64(95.0),
        FixedQty::from_f64(1.0),
        1000,
    );
    e.process(Command::PlaceOrder(stop));

    // Place a bid at 95
    e.process(limit_buy(2, 95.0, 2.0));

    // Place a sell that trades at 95 — triggers the stop
    let events = e.process(limit_sell(3, 95.0, 1.0)).to_vec();

    let has_trigger = events.iter().any(|ev| matches!(ev, MatchEvent::StopTriggered { order_id: 1, .. }));
    assert!(has_trigger);
}

/// G52: Stop order triggers at exact stop_price
#[test]
fn test_g52_trigger_exact_price() {
    let mut e = setup();

    let stop = Order::new_stop_limit(
        1, SymbolId(0), Side::Buy, TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedPrice::from_f64(100.0), // trigger at exactly 100
        FixedQty::from_f64(1.0),
        1000,
    );
    e.process(Command::PlaceOrder(stop));

    e.process(limit_sell(2, 100.0, 2.0));
    let events = e.process(limit_buy(3, 100.0, 1.0)).to_vec(); // trade at 100

    let has_trigger = events.iter().any(|ev| matches!(ev, MatchEvent::StopTriggered { order_id: 1, .. }));
    assert!(has_trigger);
}

/// G53: Stop order does NOT trigger when price doesn't reach stop_price
#[test]
fn test_g53_no_trigger() {
    let mut e = setup();

    let stop = Order::new_stop_limit(
        1, SymbolId(0), Side::Buy, TimeInForce::GTC,
        FixedPrice::from_f64(105.0),
        FixedPrice::from_f64(100.0), // trigger at 100
        FixedQty::from_f64(1.0),
        1000,
    );
    e.process(Command::PlaceOrder(stop));

    e.process(limit_sell(2, 99.0, 1.0));
    let events = e.process(limit_buy(3, 99.0, 1.0)).to_vec(); // trade at 99

    // Stop should NOT trigger (last trade at 99, but this is a buy stop that triggers at >= 100)
    let has_trigger = events.iter().any(|ev| matches!(ev, MatchEvent::StopTriggered { .. }));
    assert!(!has_trigger);
}

/// G54: Cancel stop order before trigger
#[test]
fn test_g54_cancel_stop_before_trigger() {
    let mut e = setup();

    let stop = Order::new_stop_limit(
        1, SymbolId(0), Side::Buy, TimeInForce::GTC,
        FixedPrice::from_f64(105.0),
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(1.0),
        1000,
    );
    e.process(Command::PlaceOrder(stop));

    // Cancel the stop
    let events = e.process(Command::CancelOrder { order_id: 1, symbol: SymbolId(0), timestamp: 2000 }).to_vec();
    assert!(events.iter().any(|ev| matches!(ev, MatchEvent::OrderCancelled { order_id: 1, .. })));

    // Now even if price crosses, no trigger
    e.process(limit_sell(2, 100.0, 1.0));
    let events = e.process(limit_buy(3, 100.0, 1.0)).to_vec();
    let has_trigger = events.iter().any(|ev| matches!(ev, MatchEvent::StopTriggered { .. }));
    assert!(!has_trigger);
}

/// G57: Stop-limit after trigger doesn't cross — rests on book
#[test]
fn test_g57_stop_limit_rests_after_trigger() {
    let mut e = setup();

    // Stop-limit buy: trigger at 100, limit at 90 (won't cross current asks)
    let stop = Order::new_stop_limit(
        1, SymbolId(0), Side::Buy, TimeInForce::GTC,
        FixedPrice::from_f64(90.0), // limit price very low
        FixedPrice::from_f64(100.0), // trigger at 100
        FixedQty::from_f64(1.0),
        1000,
    );
    e.process(Command::PlaceOrder(stop));

    // Create a trade at 100 to trigger the stop
    e.process(limit_sell(2, 100.0, 2.0));
    let events = e.process(limit_buy(3, 100.0, 1.0)).to_vec();

    // Stop triggered, but limit price 90 doesn't cross remaining ask at 100
    // The triggered order (now a limit at 90) should rest on the book as a bid
    let has_trigger = events.iter().any(|ev| matches!(ev, MatchEvent::StopTriggered { order_id: 1, .. }));
    assert!(has_trigger);

    // Order 1 should now be on the book as a bid at 90
    // (It was placed through process_order after conversion)
    assert!(e.book().best_bid().is_some());
}
