use me_core::{
    events::MatchEvent,
    order::{Order, TimeInForce},
    symbol::SymbolConfig,
    types::{FixedPrice, FixedQty, Side, SymbolId},
};
use me_engine::{command::Command, engine::Engine};

fn limit(id: u64, side: Side, price: i64, qty: u64) -> Order {
    Order::new_limit(
        id,
        SymbolId(0),
        side,
        TimeInForce::GTC,
        FixedPrice(price * 100_000_000),
        FixedQty(qty * 100_000_000),
        id * 1000,
    )
}
fn market(id: u64, side: Side, qty: u64, tif: TimeInForce) -> Order {
    Order::new_market(
        id,
        SymbolId(0),
        side,
        tif,
        FixedQty(qty * 100_000_000),
        id * 1000,
    )
}
#[test]
fn protection_bounds_both_sides_and_never_rests_remainder() {
    for side in [Side::Buy, Side::Sell] {
        let mut engine = Engine::new(SymbolConfig::default_btc_usdt());
        let (near, far) = if side == Side::Buy {
            (100, 101)
        } else {
            (100, 99)
        };
        engine.process(Command::PlaceOrder(limit(1, side.opposite(), near, 1)));
        engine.process(Command::PlaceOrder(limit(2, side.opposite(), far, 3)));
        engine
            .book_mut()
            .set_market_protection(3, FixedPrice(100 * 100_000_000))
            .unwrap();
        let events = engine
            .process(Command::PlaceOrder(market(3, side, 3, TimeInForce::IOC)))
            .to_vec();
        let trades: Vec<_> = events
            .iter()
            .filter_map(|e| {
                if let MatchEvent::Trade(t) = e {
                    Some(t)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].price, FixedPrice(100 * 100_000_000));
        assert!(events
            .iter()
            .any(|e| matches!(e, MatchEvent::OrderCancelled { order_id: 3, .. })));
        assert!(engine.book().get_order(3).is_none());
        assert_eq!(
            engine.book().get_order(2).unwrap().remaining_qty,
            FixedQty(3 * 100_000_000)
        );
    }
}
#[test]
fn fok_excludes_liquidity_outside_protection() {
    let mut engine = Engine::new(SymbolConfig::default_btc_usdt());
    engine.process(Command::PlaceOrder(limit(1, Side::Sell, 100, 1)));
    engine.process(Command::PlaceOrder(limit(2, Side::Sell, 101, 3)));
    engine
        .book_mut()
        .set_market_protection(3, FixedPrice(100 * 100_000_000))
        .unwrap();
    let events = engine
        .process(Command::PlaceOrder(market(
            3,
            Side::Buy,
            2,
            TimeInForce::FOK,
        )))
        .to_vec();
    assert!(!events.iter().any(|e| matches!(e, MatchEvent::Trade(_))));
    assert!(events
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderRejected { order_id: 3, .. })));
    assert_eq!(
        engine.book().get_order(1).unwrap().remaining_qty,
        FixedQty(100_000_000)
    );
}
#[test]
fn stop_market_keeps_protection_on_activation() {
    let mut engine = Engine::new(SymbolConfig::default_btc_usdt());
    engine
        .book_mut()
        .set_market_protection(1, FixedPrice(100 * 100_000_000))
        .unwrap();
    engine.process(Command::PlaceOrder(Order::new_stop_market(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice(100 * 100_000_000),
        FixedQty(100_000_000),
        1000,
    )));
    engine.process(Command::PlaceOrder(limit(2, Side::Sell, 100, 1)));
    engine.process(Command::PlaceOrder(limit(3, Side::Sell, 101, 5)));
    let events = engine
        .process(Command::PlaceOrder(limit(4, Side::Buy, 100, 1)))
        .to_vec();
    assert!(events
        .iter()
        .any(|e| matches!(e, MatchEvent::StopTriggered { order_id: 1, .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderCancelled { order_id: 1, .. })));
    let trades: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, MatchEvent::Trade(_)))
        .collect();
    assert_eq!(trades.len(), 1);
    assert!(engine.book().get_order(1).is_none());
}
