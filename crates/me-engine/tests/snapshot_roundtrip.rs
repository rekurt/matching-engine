//! Round-trip tests for `Engine::snapshot()` + `restore_from_snapshot()`.
//!
//! The critical invariant these tests defend is: snapshots MUST persist
//! both resting orders AND pending stop orders. Historically the latter
//! were forgotten, silently dropping client orders after a restart.

use me_core::order::{Order, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::{FixedPrice, FixedQty, Side, SymbolId};

use me_engine::command::Command;
use me_engine::engine::Engine;

fn config() -> SymbolConfig {
    SymbolConfig::default_btc_usdt()
}

fn place_limit(engine: &mut Engine, id: u64, side: Side, price: f64, qty: f64, ts: u64) {
    let order = Order::new_limit(
        id,
        SymbolId(0),
        side,
        TimeInForce::GTC,
        FixedPrice::from_f64(price),
        FixedQty::from_f64(qty),
        ts,
    );
    engine.process(Command::PlaceOrder(order));
}

fn place_stop_limit(
    engine: &mut Engine,
    id: u64,
    side: Side,
    stop: f64,
    limit: f64,
    qty: f64,
    ts: u64,
) {
    let order = Order::new_stop_limit(
        id,
        SymbolId(0),
        side,
        TimeInForce::GTC,
        FixedPrice::from_f64(limit),
        FixedPrice::from_f64(stop),
        FixedQty::from_f64(qty),
        ts,
    );
    engine.process(Command::PlaceOrder(order));
}

#[test]
fn snapshot_captures_resting_and_stop_orders() {
    let mut engine = Engine::new(config());
    place_limit(&mut engine, 1, Side::Buy, 100.0, 1.0, 1_000);
    place_limit(&mut engine, 2, Side::Sell, 105.0, 2.0, 2_000);
    // Stop-buy triggers when price rises to 110.
    place_stop_limit(&mut engine, 3, Side::Buy, 110.0, 111.0, 0.5, 3_000);
    // Stop-sell triggers when price drops to 95.
    place_stop_limit(&mut engine, 4, Side::Sell, 95.0, 94.0, 0.75, 4_000);

    let snap = engine.snapshot();
    assert_eq!(snap.resting_orders.len(), 2, "2 resting limits expected");
    assert_eq!(snap.stop_orders.len(), 2, "2 pending stops expected");
    assert!(snap.stop_orders.iter().any(|o| o.id == 3));
    assert!(snap.stop_orders.iter().any(|o| o.id == 4));
}

#[test]
fn restore_yields_identical_snapshot() {
    let mut original = Engine::new(config());
    place_limit(&mut original, 1, Side::Buy, 100.0, 1.0, 1_000);
    place_limit(&mut original, 2, Side::Buy, 99.0, 2.0, 2_000);
    place_limit(&mut original, 3, Side::Sell, 110.0, 1.5, 3_000);
    place_stop_limit(&mut original, 10, Side::Buy, 115.0, 116.0, 0.5, 4_000);
    place_stop_limit(&mut original, 11, Side::Sell, 90.0, 89.0, 0.25, 5_000);

    let snap = original.snapshot();
    let restored = Engine::restore_from_snapshot(config(), snap.clone());
    let snap_after_restore = restored.snapshot();

    // Counters must match exactly.
    assert_eq!(snap.sequence, snap_after_restore.sequence);
    assert_eq!(snap.trade_id_gen, snap_after_restore.trade_id_gen);

    // Compare set of order IDs (insertion order within Vec is not part
    // of the public contract).
    let mut orig_resting: Vec<u64> = snap.resting_orders.iter().map(|o| o.id).collect();
    let mut rest_resting: Vec<u64> = snap_after_restore
        .resting_orders
        .iter()
        .map(|o| o.id)
        .collect();
    orig_resting.sort_unstable();
    rest_resting.sort_unstable();
    assert_eq!(orig_resting, rest_resting);

    let mut orig_stops: Vec<u64> = snap.stop_orders.iter().map(|o| o.id).collect();
    let mut rest_stops: Vec<u64> = snap_after_restore
        .stop_orders
        .iter()
        .map(|o| o.id)
        .collect();
    orig_stops.sort_unstable();
    rest_stops.sort_unstable();
    assert_eq!(orig_stops, rest_stops);
}

#[test]
fn restored_engine_continues_processing_stop_triggers() {
    // Regression guard: a stop order taken from a snapshot must still
    // fire when its trigger price is reached. If restore_from_snapshot
    // quietly dropped stop orders, this test would fail with no trade.
    let mut original = Engine::new(config());
    place_limit(&mut original, 1, Side::Buy, 100.0, 1.0, 1_000);
    // Stop-sell that fires when trade prints at or below 100.
    place_stop_limit(&mut original, 50, Side::Sell, 100.0, 99.0, 1.0, 2_000);

    let snap = original.snapshot();
    let mut restored = Engine::restore_from_snapshot(config(), snap);

    assert_eq!(
        restored.stop_book().len(),
        1,
        "restored engine must still know about the pending stop order"
    );

    // Fire a trade at 100 — crosses into the stop trigger region.
    let taker = Order::new_limit(
        99,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(1.0),
        3_000,
    );
    let events = restored.process(Command::PlaceOrder(taker));
    let trade_happened = events
        .iter()
        .any(|e| matches!(e, me_core::events::MatchEvent::Trade(_)));
    assert!(trade_happened, "taker should match the resting bid at 100");

    assert_eq!(
        restored.stop_book().len(),
        0,
        "stop order should have been triggered by the 100 trade print"
    );
}

#[test]
fn restore_into_empty_engine_via_apply_snapshot() {
    let mut src = Engine::new(config());
    place_limit(&mut src, 1, Side::Buy, 100.0, 1.0, 1_000);
    place_stop_limit(&mut src, 2, Side::Buy, 110.0, 111.0, 0.5, 2_000);
    let snap = src.snapshot();

    // Pre-populate a destination engine with a different state, then
    // apply the snapshot — it must completely overwrite.
    let mut dst = Engine::new(config());
    place_limit(&mut dst, 999, Side::Sell, 200.0, 5.0, 0);
    dst.apply_snapshot(snap);

    let after = dst.snapshot();
    assert_eq!(after.resting_orders.len(), 1);
    assert_eq!(after.resting_orders[0].id, 1);
    assert_eq!(after.stop_orders.len(), 1);
    assert_eq!(after.stop_orders[0].id, 2);
}

#[test]
fn empty_engine_snapshot_roundtrip() {
    let empty = Engine::new(config());
    let snap = empty.snapshot();
    assert_eq!(snap.sequence, 0);
    assert!(snap.resting_orders.is_empty());
    assert!(snap.stop_orders.is_empty());

    let restored = Engine::restore_from_snapshot(config(), snap);
    let again = restored.snapshot();
    assert_eq!(again.sequence, 0);
    assert!(again.resting_orders.is_empty());
    assert!(again.stop_orders.is_empty());
}
