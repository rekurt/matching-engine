use me_core::events::MatchEvent;
use me_core::order::{Order, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::{FixedPrice, FixedQty, Side, SymbolId};
use me_engine::command::Command;
use me_engine::engine::Engine;

fn limit(id: u64, side: Side, price: f64, qty: f64) -> Command {
    Command::PlaceOrder(Order::new_limit(
        id,
        SymbolId(0),
        side,
        TimeInForce::GTC,
        FixedPrice::from_f64(price),
        FixedQty::from_f64(qty),
        id,
    ))
}

#[test]
fn monotonic_sweep_does_not_replay_old_prices_against_ratcheted_trailing_stop() {
    for side in [Side::Sell, Side::Buy] {
        let mut engine = Engine::new(SymbolConfig::default_btc_usdt());
        let (first, second, initial_stop, reversed, aggressor) = match side {
            Side::Sell => (100.0, 110.0, 95.0, 105.0, Side::Buy),
            Side::Buy => (110.0, 100.0, 115.0, 105.0, Side::Sell),
        };
        engine.process(limit(1, side, first, 1.0));
        engine.process(limit(2, side, second, 1.0));
        let stop = Order::new_stop_market(
            3,
            SymbolId(0),
            side,
            TimeInForce::IOC,
            FixedPrice::from_f64(initial_stop),
            FixedQty::from_f64(1.0),
            3,
        )
        .with_trail_offset(FixedPrice::from_f64(5.0));
        engine.process(Command::PlaceOrder(stop));
        let events = engine.process(limit(4, aggressor, second, 2.0));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, MatchEvent::StopTriggered { order_id: 3, .. })),
            "{side:?}: {events:?}"
        );
        assert_eq!(
            engine.stop_book().get(3).unwrap().stop_price,
            FixedPrice::from_f64(reversed)
        );
        engine.process(limit(5, side, reversed, 1.0));
        let events = engine.process(limit(6, aggressor, reversed, 1.0));
        assert!(events
            .iter()
            .any(|event| matches!(event, MatchEvent::StopTriggered { order_id: 3, .. })));
    }
}

#[test]
fn terminal_quantity_amend_removes_residual_from_level_aggregate() {
    for side in [Side::Buy, Side::Sell] {
        for new_qty in [6.0, 5.0] {
            let mut engine = Engine::new(SymbolConfig::default_btc_usdt());
            let opposite = match side {
                Side::Buy => Side::Sell,
                Side::Sell => Side::Buy,
            };
            engine.process(limit(1, side, 100.0, 10.0));
            engine.process(limit(2, side, 100.0, 2.0));
            engine.process(limit(3, opposite, 100.0, 6.0));
            engine.process(Command::AmendOrder {
                order_id: 1,
                symbol: SymbolId(0),
                new_price: None,
                new_qty: Some(FixedQty::from_f64(new_qty)),
                timestamp: 4,
            });
            assert!(engine.book().get_order(1).is_none());
            let level = match side {
                Side::Buy => engine.book().best_bid_level().unwrap().1,
                Side::Sell => engine.book().best_ask_level().unwrap().1,
            };
            assert_eq!(
                level.total_qty,
                FixedQty::from_f64(2.0),
                "{side:?} new_qty={new_qty}"
            );
        }
    }
}

#[test]
fn snapshot_restore_preserves_fifo_after_slab_slot_reuse() {
    let config = SymbolConfig::default_btc_usdt();
    let mut engine = Engine::new(config.clone());
    engine.process(limit(1, Side::Sell, 100.0, 1.0));
    engine.process(limit(2, Side::Sell, 100.0, 1.0));
    engine.process(Command::CancelOrder {
        order_id: 1,
        symbol: SymbolId(0),
        timestamp: 3,
    });
    engine.process(limit(3, Side::Sell, 100.0, 1.0));
    let mut restored = Engine::restore_from_snapshot(config, engine.snapshot());
    for id in [2, 3] {
        assert_eq!(
            restored.book().get_order(id).unwrap().sequence,
            engine.book().get_order(id).unwrap().sequence
        );
    }
    let events = restored.process(limit(4, Side::Buy, 100.0, 1.0));
    let maker = events.iter().find_map(|event| match event {
        MatchEvent::Trade(trade) => Some(trade.maker_order_id),
        _ => None,
    });
    assert_eq!(maker, Some(2));
}

#[test]
fn malformed_resting_sequences_reject_snapshot_without_changing_live_book() {
    let config = SymbolConfig::default_btc_usdt();
    let mut original = Engine::new(config.clone());
    original.process(limit(1, Side::Sell, 100.0, 1.0));
    original.process(limit(2, Side::Sell, 100.0, 1.0));
    for invalid in [0, 1, 3] {
        let mut snapshot = original.snapshot();
        snapshot.resting_orders[1].sequence = invalid;
        let mut target = Engine::new(config.clone());
        target.process(limit(99, Side::Buy, 90.0, 1.0));
        assert!(
            target.try_apply_snapshot(snapshot).is_err(),
            "invalid sequence={invalid}"
        );
        assert!(target.book().get_order(99).is_some());
        assert_eq!(target.book().order_count(), 1);
    }
}
