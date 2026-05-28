/// Integration tests: Stop orders (G50-G57)
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

/// G50: Stop-limit buy triggers on price rise → converts to limit + matches
#[test]
fn test_g50_stop_limit_buy_trigger() {
    let mut e = setup();

    // Place stop-limit buy: trigger at 100, limit at 101
    let stop = Order::new_stop_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
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
    let has_trigger = events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::StopTriggered { order_id: 1, .. }));
    assert!(has_trigger, "Stop order should have been triggered");
}

/// G51: Stop-market sell triggers on price drop
#[test]
fn test_g51_stop_market_sell_trigger() {
    let mut e = setup();

    // Place stop-market sell: trigger at 95
    let stop = Order::new_stop_market(
        1,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(95.0),
        FixedQty::from_f64(1.0),
        1000,
    );
    e.process(Command::PlaceOrder(stop));

    // Place a bid at 95
    e.process(limit_buy(2, 95.0, 2.0));

    // Place a sell that trades at 95 — triggers the stop
    let events = e.process(limit_sell(3, 95.0, 1.0)).to_vec();

    let has_trigger = events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::StopTriggered { order_id: 1, .. }));
    assert!(has_trigger);
}

/// G52: Stop order triggers at exact stop_price
#[test]
fn test_g52_trigger_exact_price() {
    let mut e = setup();

    let stop = Order::new_stop_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedPrice::from_f64(100.0), // trigger at exactly 100
        FixedQty::from_f64(1.0),
        1000,
    );
    e.process(Command::PlaceOrder(stop));

    e.process(limit_sell(2, 100.0, 2.0));
    let events = e.process(limit_buy(3, 100.0, 1.0)).to_vec(); // trade at 100

    let has_trigger = events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::StopTriggered { order_id: 1, .. }));
    assert!(has_trigger);
}

/// G53: Stop order does NOT trigger when price doesn't reach stop_price
#[test]
fn test_g53_no_trigger() {
    let mut e = setup();

    let stop = Order::new_stop_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(105.0),
        FixedPrice::from_f64(100.0), // trigger at 100
        FixedQty::from_f64(1.0),
        1000,
    );
    e.process(Command::PlaceOrder(stop));

    e.process(limit_sell(2, 99.0, 1.0));
    let events = e.process(limit_buy(3, 99.0, 1.0)).to_vec(); // trade at 99

    // Stop should NOT trigger (last trade at 99, but this is a buy stop that triggers at >= 100)
    let has_trigger = events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::StopTriggered { .. }));
    assert!(!has_trigger);
}

/// G54: Cancel stop order before trigger
#[test]
fn test_g54_cancel_stop_before_trigger() {
    let mut e = setup();

    let stop = Order::new_stop_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(105.0),
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(1.0),
        1000,
    );
    e.process(Command::PlaceOrder(stop));

    // Cancel the stop
    let events = e
        .process(Command::CancelOrder {
            order_id: 1,
            symbol: SymbolId(0),
            timestamp: 2000,
        })
        .to_vec();
    assert!(events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::OrderCancelled { order_id: 1, .. })));

    // Now even if price crosses, no trigger
    e.process(limit_sell(2, 100.0, 1.0));
    let events = e.process(limit_buy(3, 100.0, 1.0)).to_vec();
    let has_trigger = events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::StopTriggered { .. }));
    assert!(!has_trigger);
}

/// G57: Stop-limit after trigger doesn't cross — rests on book
#[test]
fn test_g57_stop_limit_rests_after_trigger() {
    let mut e = setup();

    // Stop-limit buy: trigger at 100, limit at 90 (won't cross current asks)
    let stop = Order::new_stop_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(90.0),  // limit price very low
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
    let has_trigger = events
        .iter()
        .any(|ev| matches!(ev, MatchEvent::StopTriggered { order_id: 1, .. }));
    assert!(has_trigger);

    // Order 1 should now be on the book as a bid at 90
    // (It was placed through process_order after conversion)
    assert!(e.book().best_bid().is_some());
}

// ─── Trailing stops ──────────────────────────────────────────────────────────

fn trailing_sell_stop(id: OrderId, init_stop: f64, trail: f64, qty: f64) -> Command {
    Command::PlaceOrder(
        Order::new_stop_market(
            id,
            SymbolId(0),
            Side::Sell,
            TimeInForce::GTC,
            FixedPrice::from_f64(init_stop),
            FixedQty::from_f64(qty),
            id * 1000,
        )
        .with_trail_offset(FixedPrice::from_f64(trail)),
    )
}

#[test]
fn test_trailing_sell_stop_ratchets_and_fills() {
    let mut e = setup();

    // Trailing sell stop: initial stop 95, trail 5.
    e.process(trailing_sell_stop(1, 95.0, 5.0, 1.0));
    assert_eq!(e.stop_book().len(), 1);

    // Trade at 110 (resting ask 110, taker buy 110) → stop ratchets up to 105.
    e.process(limit_sell(2, 110.0, 1.0));
    e.process(limit_buy(3, 110.0, 1.0));
    assert_eq!(e.stop_book().len(), 1, "no trigger while market rises");

    // Build a bid at 105, then a sell hits it → trade at 105 (down move).
    e.process(limit_buy(4, 105.0, 2.0));
    let events = e.process(limit_sell(5, 105.0, 1.0)).to_vec();

    // Trade at 105 reaches the ratcheted stop (105) → trailing stop triggers,
    // converts to a market sell and fills against the remaining bid.
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::StopTriggered { order_id: 1, .. })),
        "trailing stop must trigger at the ratcheted level"
    );
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::Trade(t) if t.taker_order_id == 1)),
        "triggered trailing stop must trade"
    );
    assert_eq!(e.stop_book().len(), 0);
}

fn trailing_buy_stop(id: OrderId, init_stop: f64, trail: f64, qty: f64) -> Command {
    Command::PlaceOrder(
        Order::new_stop_market(
            id,
            SymbolId(0),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_f64(init_stop),
            FixedQty::from_f64(qty),
            id * 1000,
        )
        .with_trail_offset(FixedPrice::from_f64(trail)),
    )
}

#[test]
fn test_trailing_buy_stop_ratchets_and_fills() {
    let mut e = setup();

    // Trailing buy stop (protective for a short): initial stop 105, trail 5.
    e.process(trailing_buy_stop(1, 105.0, 5.0, 1.0));
    assert_eq!(e.stop_book().len(), 1);

    // Trade at 90 → stop ratchets DOWN to min(105, 90+5) = 95.
    e.process(limit_buy(2, 90.0, 1.0));
    e.process(limit_sell(3, 90.0, 1.0));
    assert_eq!(e.stop_book().len(), 1, "no trigger while market falls");

    // Rest an ask at 95, then a buy lifts it → trade at 95 reaches the ratcheted
    // stop → trailing buy triggers, converts to a market buy, fills the rest.
    e.process(limit_sell(4, 95.0, 2.0));
    let events = e.process(limit_buy(5, 95.0, 1.0)).to_vec();
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::StopTriggered { order_id: 1, .. })),
        "trailing buy stop must trigger at the ratcheted level"
    );
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::Trade(t) if t.taker_order_id == 1)),
        "triggered trailing buy stop must trade"
    );
    assert_eq!(e.stop_book().len(), 0);
}

#[test]
fn test_trailing_stop_survives_snapshot_with_ratcheted_price() {
    let mut e = setup();
    e.process(trailing_sell_stop(1, 95.0, 5.0, 1.0));
    // Ratchet the stop up to 105 via a trade at 110.
    e.process(limit_sell(2, 110.0, 1.0));
    e.process(limit_buy(3, 110.0, 1.0));

    // Snapshot + restore into a fresh engine.
    let snap = e.snapshot();
    let mut e2 = Engine::restore_from_snapshot(SymbolConfig::default_btc_usdt(), snap);
    assert_eq!(e2.stop_book().len(), 1, "trailing stop restored");

    // The ratcheted level (105) must survive: a pullback to 106 does NOT trigger,
    // but a drop to 105 does — proving stop_price == 105, not the initial 95.
    e2.process(limit_buy(4, 106.0, 1.0));
    e2.process(limit_sell(5, 106.0, 1.0)); // trade at 106
    assert_eq!(
        e2.stop_book().len(),
        1,
        "106 > ratcheted stop 105 → no trigger"
    );

    e2.process(limit_buy(6, 105.0, 2.0));
    let events = e2.process(limit_sell(7, 105.0, 1.0)).to_vec(); // trade at 105
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::StopTriggered { order_id: 1, .. })),
        "restored trailing stop triggers at the ratcheted 105, proving the ratchet survived"
    );
    assert_eq!(e2.stop_book().len(), 0);
}

#[test]
fn test_reused_stop_id_not_cancelled_by_stale_gtd_entry() {
    let mut e = setup();
    // A GTD stop with an already-past expiry, cancelled before the sweep runs.
    // Its heap entry (expire=1) is left stale.
    let gtd_stop = Order::new_stop_market(
        1,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTD,
        FixedPrice::from_f64(90.0),
        FixedQty::from_f64(1.0),
        1000,
    )
    .with_expiry(1);
    e.process(Command::PlaceOrder(gtd_stop));
    e.process(Command::CancelOrder {
        order_id: 1,
        symbol: SymbolId(0),
        timestamp: 1100,
    });
    assert!(!e.stop_book().contains(1));

    // Reuse id 1 for a NEW non-GTD stop.
    let new_stop = Order::new_stop_market(
        1,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(80.0),
        FixedQty::from_f64(1.0),
        2000,
    );
    e.process(Command::PlaceOrder(new_stop));
    assert!(e.stop_book().contains(1));

    // The stale GTD heap entry must NOT cancel the reused non-GTD stop.
    let swept = e.expire_gtd_now();
    assert!(
        !swept
            .iter()
            .any(|ev| matches!(ev, MatchEvent::OrderCancelled { order_id: 1, .. })),
        "stale GTD entry must not cancel a reused non-GTD stop"
    );
    assert!(
        e.stop_book().contains(1),
        "reused non-GTD stop must survive the stale sweep"
    );
}

#[test]
fn test_iceberg_replenishment_trade_triggers_stop() {
    let mut e = setup();
    // Stop sell that should fire when the market trades at/below 99.
    e.process(Command::PlaceOrder(Order::new_stop_market(
        1,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(99.0),
        FixedQty::from_f64(1.0),
        1000,
    )));
    assert_eq!(e.stop_book().len(), 1);

    // Resting bids: 100 (qty 1) then 99 (qty 3).
    e.process(limit_buy(2, 100.0, 1.0));
    e.process(limit_buy(3, 99.0, 3.0));

    // Aggressive iceberg SELL (total 4, display 1, limit 99): the visible slice
    // trades at 100 (checked per-command → no trigger), and the replenishment
    // slices trade at 99. The stop is reached only by a replenishment trade, so
    // it must still fire after replenishment.
    let ice = Order::new_limit(
        5,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(99.0),
        FixedQty::from_f64(4.0),
        5000,
    )
    .with_display_qty(FixedQty::from_f64(1.0));
    let events = e.process(Command::PlaceOrder(ice)).to_vec();

    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::StopTriggered { order_id: 1, .. })),
        "a stop reached only by an iceberg replenishment trade must still fire"
    );
    assert_eq!(e.stop_book().len(), 0);
}
