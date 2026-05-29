/// Integration tests: Stop orders (G50-G57)
use me_core::events::{MatchEvent, RejectReason};
use me_core::order::{Order, TimeInForce, ORDER_FLAG_MAKER_OR_CANCEL};
use me_core::symbol::SymbolConfig;
use me_core::types::*;
use me_core::SessionId;
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

#[test]
fn test_two_trailing_stops_trigger_deterministically() {
    // Two identical trailing stops are hit by the same trade with only enough
    // liquidity for one. The earlier stop (by timestamp, then id) must get the
    // fill — trigger order must be canonical, not HashMap-iteration order, or a
    // replica / WAL replay could fill the other one.
    let mut e = setup();
    e.process(trailing_sell_stop(1, 95.0, 5.0, 1.0)); // ts 1000
    e.process(trailing_sell_stop(2, 95.0, 5.0, 1.0)); // ts 2000

    // Trade at 110 ratchets both stops up to 105 (no trigger yet).
    e.process(limit_sell(10, 110.0, 1.0));
    e.process(limit_buy(11, 110.0, 1.0));
    assert_eq!(e.stop_book().len(), 2);

    // Bid 2 @ 105; a sell of 1 trades at 105 (leaving 1 bid) and trips both
    // trailing stops. Their two market sells (2 lots) chase the single remaining
    // bid lot — only one fills.
    e.process(limit_buy(12, 105.0, 2.0));
    let events = e.process(limit_sell(13, 105.0, 1.0)).to_vec();

    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::Trade(t) if t.taker_order_id == 1)),
        "the earlier trailing stop (id 1) must deterministically get the liquidity"
    );
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::Trade(t) if t.taker_order_id == 2)),
        "the later trailing stop (id 2) must not be filled before id 1"
    );
}

#[test]
fn test_maker_or_cancel_on_stop_market_rejected() {
    // A maker-or-cancel (or post-only) flag on a market-type order is
    // contradictory: once triggered it converts to a market order, which always
    // takes liquidity. Reject at submission rather than silently taking.
    let mut e = setup();
    let mut o = Order::new_stop_market(
        1,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(95.0),
        FixedQty::from_f64(1.0),
        1000,
    );
    o.flags = ORDER_FLAG_MAKER_OR_CANCEL;
    let events = e.process(Command::PlaceOrder(o)).to_vec();
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            MatchEvent::OrderRejected {
                reason: RejectReason::WouldTakeMarket,
                ..
            }
        )),
        "maker-or-cancel on a stop-market must be rejected"
    );
    assert_eq!(
        e.stop_book().len(),
        0,
        "contradictory maker-or-cancel stop-market must not rest"
    );
}

#[test]
fn test_live_order_reusing_pending_stop_id_rejected() {
    // Symmetric to the stop-side check: a regular order whose id is already
    // pending in the stop book must be rejected (process_order only sees the
    // live book), so an id is unique across both books.
    let mut e = setup();
    e.process(Command::PlaceOrder(Order::new_stop_market(
        1,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(95.0),
        FixedQty::from_f64(1.0),
        1000,
    )));
    assert!(e.stop_book().contains(1));

    let events = e.process(limit_buy(1, 100.0, 1.0)).to_vec();
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            MatchEvent::OrderRejected {
                reason: RejectReason::DuplicateOrderId,
                ..
            }
        )),
        "a live order reusing a pending stop id must be rejected"
    );
    assert!(
        e.book().get_order(1).is_none(),
        "duplicate live order must not rest"
    );
}

#[test]
fn test_stop_with_duplicate_id_rejected() {
    // Stops bypass process_order, so the engine must apply the same uniqueness
    // invariant: a stop whose id collides with an existing stop OR a live order
    // must be rejected as DuplicateOrderId, not silently inserted (which would
    // orphan a stop-book vector entry).
    let mut e = setup();
    e.process(Command::PlaceOrder(Order::new_stop_market(
        1,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(95.0),
        FixedQty::from_f64(1.0),
        1000,
    )));
    assert_eq!(e.stop_book().len(), 1);

    // Duplicate stop id → rejected.
    let dup = e
        .process(Command::PlaceOrder(Order::new_stop_market(
            1,
            SymbolId(0),
            Side::Sell,
            TimeInForce::GTC,
            FixedPrice::from_f64(90.0),
            FixedQty::from_f64(1.0),
            1100,
        )))
        .to_vec();
    assert!(
        dup.iter().any(|ev| matches!(
            ev,
            MatchEvent::OrderRejected {
                reason: RejectReason::DuplicateOrderId,
                ..
            }
        )),
        "duplicate stop id must be rejected"
    );
    assert_eq!(
        e.stop_book().len(),
        1,
        "duplicate stop must not be inserted"
    );

    // Stop id colliding with a live resting order → also rejected.
    e.process(limit_buy(2, 100.0, 1.0));
    let dup2 = e
        .process(Command::PlaceOrder(Order::new_stop_market(
            2,
            SymbolId(0),
            Side::Sell,
            TimeInForce::GTC,
            FixedPrice::from_f64(95.0),
            FixedQty::from_f64(1.0),
            1200,
        )))
        .to_vec();
    assert!(
        dup2.iter().any(|ev| matches!(
            ev,
            MatchEvent::OrderRejected {
                reason: RejectReason::DuplicateOrderId,
                ..
            }
        )),
        "stop id colliding with a live order must be rejected"
    );
}

#[test]
fn test_gtd_stop_without_expiry_rejected() {
    // A GTD stop carries no expiry here (expire_at = 0). The engine routes stops
    // straight to the stop book, so it must validate them first — otherwise the
    // stop is accepted, never enqueued for expiry (engine skips expire_at == 0),
    // and becomes a permanent stop instead of being rejected as InvalidExpiry.
    let mut e = setup();
    let stop = Order::new_stop_market(
        1,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTD,
        FixedPrice::from_f64(95.0),
        FixedQty::from_f64(1.0),
        1000,
    ); // new_stop_market leaves expire_at = 0
    let events = e.process(Command::PlaceOrder(stop)).to_vec();
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            MatchEvent::OrderRejected {
                reason: RejectReason::InvalidExpiry,
                ..
            }
        )),
        "a GTD stop with no expiry must be rejected as InvalidExpiry"
    );
    assert_eq!(e.stop_book().len(), 0, "invalid GTD stop must not rest");
}

fn stop_market_sell(id: OrderId, stop: f64, qty: f64) -> Command {
    Command::PlaceOrder(Order::new_stop_market(
        id,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(stop),
        FixedQty::from_f64(qty),
        id * 1000, // timestamp = id*1000, so lower id == earlier
    ))
}

#[test]
fn test_same_price_stops_keep_time_priority_after_cancel() {
    // Three stop-sells at the same stop price; cancelling the first uses
    // swap_remove, which moves the last (newest) into the freed slot. Trigger
    // order must still follow time priority (older first), not the reordered
    // vector — otherwise a newer stop jumps ahead under limited liquidity.
    let mut e = setup();
    e.process(stop_market_sell(1, 95.0, 1.0)); // ts 1000
    e.process(stop_market_sell(2, 95.0, 1.0)); // ts 2000
    e.process(stop_market_sell(3, 95.0, 1.0)); // ts 3000
    assert_eq!(e.stop_book().len(), 3);

    // Cancel the oldest (id 1); swap_remove moves id 3 into slot 0 → vector [3,2].
    e.process(Command::CancelOrder {
        order_id: 1,
        symbol: SymbolId(0),
        timestamp: 1500,
    });
    assert_eq!(e.stop_book().len(), 2);

    // Bid of 2 @ 95, then a sell of 1 trades at 95 (leaving one bid lot) and
    // trips both surviving stops. Their two market sells chase the single
    // remaining lot — the older survivor (id 2) must win, not id 3.
    e.process(limit_buy(10, 95.0, 2.0));
    let events = e.process(limit_sell(11, 95.0, 1.0)).to_vec();

    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::Trade(t) if t.taker_order_id == 2)),
        "older surviving stop (id 2) must trigger/fill ahead of the newer id 3"
    );
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::Trade(t) if t.taker_order_id == 3)),
        "newer stop (id 3) must not jump ahead after the cancel"
    );
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

// ─── Self-trade prevention for triggered stops ──────────────────────────────

const SESSION_A: SessionId = SessionId::from_bytes([0xA1u8; 16]);
const SESSION_B: SessionId = SessionId::from_bytes([0xB2u8; 16]);
const SESSION_C: SessionId = SessionId::from_bytes([0xC3u8; 16]);

fn limit_sell_session(id: OrderId, price: f64, qty: f64, session: SessionId) -> Command {
    let mut o = Order::new_limit(
        id,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(price),
        FixedQty::from_f64(qty),
        id * 1000,
    );
    o.client_session_id = session;
    Command::PlaceOrder(o)
}

fn limit_buy_session(id: OrderId, price: f64, qty: f64, session: SessionId) -> Command {
    let mut o = Order::new_limit(
        id,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(price),
        FixedQty::from_f64(qty),
        id * 1000,
    );
    o.client_session_id = session;
    Command::PlaceOrder(o)
}

/// A triggered stop that would cross its OWN session's resting order must be
/// rejected by self-trade prevention — not executed. The trigger path
/// (`matching::process_order`) must honor STP just like the normal place path.
#[test]
fn test_triggered_stop_blocked_by_self_trade_prevention() {
    let mut e = setup();
    e.set_self_trade_prevention(true);

    // Session B sell @ 100 — fuel for the trigger trade (consumed first, FIFO).
    e.process(limit_sell_session(1, 100.0, 1.0, SESSION_B));
    // Session A sell @ 101 — the self-cross target; untouched by the 100 trade.
    e.process(limit_sell_session(2, 101.0, 1.0, SESSION_A));

    // Session A stop-limit buy: trigger=100, limit=101. Rests in the stop book.
    let mut stop = Order::new_stop_limit(
        3,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(101.0), // limit
        FixedPrice::from_f64(100.0), // stop/trigger
        FixedQty::from_f64(1.0),
        3000,
    );
    stop.client_session_id = SESSION_A;
    e.process(Command::PlaceOrder(stop));

    // Session C buys @ 100 → trades against B's sell → last trade=100 → triggers
    // A's stop. The triggered buy @ 101 would cross A's own resting sell @ 101.
    let events = e
        .process(limit_buy_session(4, 100.0, 1.0, SESSION_C))
        .to_vec();

    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::StopTriggered { order_id: 3, .. })),
        "stop #3 must trigger on the 100 trade: {events:?}"
    );
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            MatchEvent::OrderRejected {
                order_id: 3,
                reason: RejectReason::SelfTradePrevented,
                ..
            }
        )),
        "triggered stop crossing same-session liquidity must be STP-rejected: {events:?}"
    );
    // No trade between the triggered stop (#3) and A's resting sell (#2).
    assert!(
        e.book().contains_order(2),
        "session A's resting sell #2 must remain — no self-trade executed"
    );
}

/// The STP guard on the trigger path must NOT over-block: a triggered stop that
/// crosses a DIFFERENT session's liquidity still executes normally.
#[test]
fn test_triggered_stop_crossing_other_session_still_trades() {
    let mut e = setup();
    e.set_self_trade_prevention(true);

    // Session B sell @ 100 — trigger fuel.
    e.process(limit_sell_session(1, 100.0, 1.0, SESSION_B));
    // Session B sell @ 101 — cross target owned by a DIFFERENT session than the stop.
    e.process(limit_sell_session(2, 101.0, 1.0, SESSION_B));

    // Session A stop-limit buy: trigger=100, limit=101.
    let mut stop = Order::new_stop_limit(
        3,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(101.0),
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(1.0),
        3000,
    );
    stop.client_session_id = SESSION_A;
    e.process(Command::PlaceOrder(stop));

    // Session C buys @ 100 → trades against B's sell #1 → triggers A's stop #3,
    // which then crosses B's sell #2 (different session) → must trade.
    let events = e
        .process(limit_buy_session(4, 100.0, 1.0, SESSION_C))
        .to_vec();

    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::StopTriggered { order_id: 3, .. })),
        "stop #3 must trigger: {events:?}"
    );
    assert!(
        !events.iter().any(|ev| matches!(
            ev,
            MatchEvent::OrderRejected {
                reason: RejectReason::SelfTradePrevented,
                ..
            }
        )),
        "a different-session triggered stop must NOT be STP-rejected: {events:?}"
    );
    // The triggered stop crossed B's sell #2 → that resting order is consumed.
    assert!(
        !e.book().contains_order(2),
        "B's resting sell #2 must be filled by the triggered stop: {events:?}"
    );
}

// ─── Intra-command trade path feeds stop triggers ───────────────────────────

#[test]
fn test_trailing_stop_ratchets_on_intra_command_high() {
    // A single sell order that sweeps bids at 105 then 95 must ratchet a sell
    // trailing stop (stop 90 / trail 10) up to 95 on the 105 print, then trigger
    // it on the 95 print — even though both trades happen inside one command.
    // Feeding only the final price (95) would leave the stop at 90 and miss it.
    let mut e = setup();
    e.process(trailing_sell_stop(1, 90.0, 10.0, 1.0));
    assert_eq!(e.stop_book().len(), 1);

    // Two resting bids at different prices (no trade yet → no ratchet).
    e.process(limit_buy(2, 105.0, 1.0));
    e.process(limit_buy(3, 95.0, 1.0));
    assert_eq!(e.stop_book().len(), 1, "no trade yet → stop still pending");

    // One sell sweeps both bids → trades at 105 then 95 within this command.
    let events = e.process(limit_sell(4, 95.0, 2.0)).to_vec();

    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::StopTriggered { order_id: 1, .. })),
        "trailing stop must ratchet on the 105 print and trigger on the 95 print \
         within one command: {events:?}"
    );
    assert_eq!(
        e.stop_book().len(),
        0,
        "triggered trailing stop must leave the book"
    );
}

#[test]
fn test_fixed_stop_triggers_on_intra_command_extreme() {
    // Same root cause for fixed stops: a sell sweep trading 105 then 100 must
    // trigger a fixed stop-buy at 102 (price reached 105 ≥ 102), even though the
    // final trade (100) is below the trigger.
    let mut e = setup();
    let stop = Order::new_stop_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(102.0), // limit
        FixedPrice::from_f64(102.0), // stop/trigger
        FixedQty::from_f64(1.0),
        1000,
    );
    e.process(Command::PlaceOrder(stop));
    assert_eq!(e.stop_book().len(), 1);

    e.process(limit_buy(2, 105.0, 1.0));
    e.process(limit_buy(3, 100.0, 1.0));

    // Sell sweeps both bids → trades 105 then 100 in one command.
    let events = e.process(limit_sell(4, 100.0, 2.0)).to_vec();

    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::StopTriggered { order_id: 1, .. })),
        "fixed stop-buy at 102 must trigger on the 105 print within the sweep: {events:?}"
    );
    assert_eq!(e.stop_book().len(), 0);
}

#[test]
fn test_iceberg_refill_does_not_self_trade_a_triggered_same_session_stop() {
    // Self-trade prevention must also cover iceberg replenishment. The initial
    // placement STP check cannot see liquidity that appears AFTER the iceberg
    // rests — e.g. a same-session sell stop triggered by the iceberg's own fill.
    // The replenished buy slice must NOT match that same-session resting sell.
    let mut e = setup();
    e.set_self_trade_prevention(true);

    // #1 session-A stop-limit SELL: trigger 100, limit 100. Rests in stop book.
    let mut stop = Order::new_stop_limit(
        1,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0), // limit
        FixedPrice::from_f64(100.0), // stop/trigger
        FixedQty::from_f64(1.0),
        1000,
    );
    stop.client_session_id = SESSION_A;
    e.process(Command::PlaceOrder(stop));

    // #2 session-A buy iceberg @ 100, total 4, display 1. Visible slice rests.
    let mut iceberg = Order::new_limit(
        2,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(4.0),
        2000,
    )
    .with_display_qty(FixedQty::from_f64(1.0));
    iceberg.client_session_id = SESSION_A;
    e.process(Command::PlaceOrder(iceberg));

    // #3 session-B sell @ 100, qty 1 → trades against the iceberg's visible
    // slice (cross-session) → last trade 100 → triggers A's sell stop, which
    // rests @ 100. The iceberg then replenishes a buy slice @ 100 that would
    // cross A's own resting sell.
    let events = e
        .process(limit_sell_session(3, 100.0, 1.0, SESSION_B))
        .to_vec();

    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, MatchEvent::StopTriggered { order_id: 1, .. })),
        "A's sell stop must trigger on the cross-session fill: {events:?}"
    );
    // The replenished buy slice (#2) must NOT self-match A's resting sell (#1).
    assert!(
        !events.iter().any(|ev| matches!(
            ev,
            MatchEvent::Trade(t)
                if (t.maker_order_id == 1 && t.taker_order_id == 2)
                    || (t.maker_order_id == 2 && t.taker_order_id == 1)
        )),
        "iceberg refill must not self-trade the same-session stop: {events:?}"
    );
    assert!(
        e.book().get_order(1).is_some(),
        "A's triggered sell #1 must still rest (not consumed by a self-trade): {events:?}"
    );
}
