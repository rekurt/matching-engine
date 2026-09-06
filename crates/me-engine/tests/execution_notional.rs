use me_core::symbol::SymbolConfig;
use me_core::{
    types::{FixedPrice, FixedQty, Side, SymbolId},
    MatchEvent, Order, RejectReason, TimeInForce,
};
use me_engine::{command::Command, engine::Engine};
use me_risk::config::RiskConfig;

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
fn cap(engine: &mut Engine) {
    let mut config = RiskConfig::default_for(SymbolId(0));
    config.reference_price = FixedPrice::from_f64(100.0);
    config.max_order_notional = 100 * me_core::types::SCALE_U64;
    config.price_band_bps = 0;
    engine.set_risk_config(Some(config));
}
#[test]
fn market_execution_cannot_exceed_cap_when_reference_is_stale() {
    let mut engine = Engine::new(SymbolConfig::default_btc_usdt());
    engine.process(limit(1, Side::Sell, 1000.0, 1.0));
    cap(&mut engine);
    let events = engine
        .process(Command::PlaceOrder(Order::new_market(
            2,
            SymbolId(0),
            Side::Buy,
            TimeInForce::IOC,
            FixedQty::from_f64(1.0),
            2,
        )))
        .to_vec();
    assert!(
        events.iter().any(|e| matches!(
            e,
            MatchEvent::OrderRejected {
                reason: RejectReason::RiskCheckFailed,
                ..
            }
        )),
        "{events:?}"
    );
    assert!(engine.book().contains_order(1));
}
#[test]
fn sell_limit_better_execution_price_cannot_bypass_cap() {
    let mut engine = Engine::new(SymbolConfig::default_btc_usdt());
    engine.process(limit(1, Side::Buy, 1000.0, 1.0));
    cap(&mut engine);
    let events = engine.process(limit(2, Side::Sell, 100.0, 1.0)).to_vec();
    assert!(
        events.iter().any(|e| matches!(
            e,
            MatchEvent::OrderRejected {
                reason: RejectReason::RiskCheckFailed,
                ..
            }
        )),
        "{events:?}"
    );
    assert!(engine.book().contains_order(1));
}
#[test]
fn amend_execution_cap_preserves_original_order() {
    let mut engine = Engine::new(SymbolConfig::default_btc_usdt());
    engine.process(limit(1, Side::Buy, 1000.0, 1.0));
    engine.process(limit(2, Side::Sell, 1100.0, 1.0));
    cap(&mut engine);
    let events = engine
        .process(Command::AmendOrder {
            order_id: 2,
            symbol: SymbolId(0),
            new_price: Some(FixedPrice::from_f64(100.0)),
            new_qty: None,
            timestamp: 3,
        })
        .to_vec();
    assert!(
        events.iter().any(|e| matches!(
            e,
            MatchEvent::OrderRejected {
                reason: RejectReason::RiskCheckFailed,
                ..
            }
        )),
        "{events:?}"
    );
    assert_eq!(
        engine.book().get_order(2).unwrap().price,
        FixedPrice::from_f64(1100.0)
    );
}

#[test]
fn triggered_market_stop_uses_execution_price_instead_of_reference() {
    let mut engine = Engine::new(SymbolConfig::default_btc_usdt());
    engine.process(limit(1, Side::Sell, 1000.0, 1.0));
    engine.process(Command::PlaceOrder(Order::new_stop_market(
        3,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(1.0),
        3,
    )));
    engine.process(limit(2, Side::Sell, 100.0, 1.0));
    cap(&mut engine);
    let events = engine.process(limit(4, Side::Buy, 100.0, 1.0)).to_vec();
    assert!(
        events.iter().any(|e| matches!(
            e,
            MatchEvent::OrderRejected {
                order_id: 3,
                reason: RejectReason::RiskCheckFailed,
                ..
            }
        )),
        "{events:?}"
    );
    assert!(engine.book().contains_order(1));
    assert!(!engine.stop_book().contains(3));
}

#[test]
fn filled_order_amendments_must_decrease_monotonically_under_cap() {
    let mut engine = Engine::new(SymbolConfig::default_btc_usdt());
    cap(&mut engine);
    engine.process(limit(1, Side::Sell, 100.0, 1.0));
    engine.process(limit(2, Side::Buy, 100.0, 0.5));
    let events = engine
        .process(Command::AmendOrder {
            order_id: 1,
            symbol: SymbolId(0),
            new_price: Some(FixedPrice::from_f64(99.0)),
            new_qty: Some(FixedQty::from_f64(0.75)),
            timestamp: 3,
        })
        .to_vec();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, MatchEvent::OrderAmended { .. })),
        "{events:?}"
    );
    for (new_price, new_qty) in [
        (Some(FixedPrice::from_f64(100.0)), None),
        (None, Some(FixedQty::from_f64(0.8))),
    ] {
        let events = engine
            .process(Command::AmendOrder {
                order_id: 1,
                symbol: SymbolId(0),
                new_price,
                new_qty,
                timestamp: 4,
            })
            .to_vec();
        assert!(
            events.iter().any(|e| matches!(
                e,
                MatchEvent::OrderRejected {
                    reason: RejectReason::AmendNotSupported,
                    ..
                }
            )),
            "{events:?}"
        );
    }
    let resting = engine.book().get_order(1).unwrap();
    assert_eq!(resting.qty, FixedQty::from_f64(0.75));
    assert_eq!(resting.remaining_qty, FixedQty::from_f64(0.25));
}

#[test]
fn iceberg_quantity_and_ioc_remainder_are_conservatively_bounded() {
    let mut engine = Engine::new(SymbolConfig::default_btc_usdt());
    let mut maker = Order::new_limit(
        1,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(1000.0),
        FixedQty::from_f64(1.0),
        1,
    );
    maker.display_qty = FixedQty::from_f64(0.01);
    engine.process(Command::PlaceOrder(maker));
    cap(&mut engine);
    let events = engine
        .process(Command::PlaceOrder(Order::new_market(
            2,
            SymbolId(0),
            Side::Buy,
            TimeInForce::IOC,
            FixedQty::from_f64(1.0),
            2,
        )))
        .to_vec();
    assert!(events.iter().all(|e| !matches!(e, MatchEvent::Trade(_))));
    assert_eq!(
        engine.book().get_order(1).unwrap().remaining_qty,
        FixedQty::from_f64(0.01)
    );
}
