use me_core::symbol::SymbolConfig;
use me_core::{
    types::{FixedPrice, FixedQty, Side, SymbolId},
    MatchEvent, Order, TimeInForce,
};
use me_engine::{command::Command, engine::Engine};
fn maker(id: u64, qty: f64, display: f64) -> Order {
    Order::new_limit(
        id,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(qty),
        id,
    )
    .with_display_qty(FixedQty::from_f64(display))
}
#[test]
fn immediate_orders_consume_hidden_reserve() {
    for (market, tif) in [
        (true, TimeInForce::IOC),
        (true, TimeInForce::FOK),
        (false, TimeInForce::IOC),
        (false, TimeInForce::FOK),
    ] {
        let mut e = Engine::new(SymbolConfig::default_btc_usdt());
        e.process(Command::PlaceOrder(maker(1, 6.0, 2.0)));
        let taker = if market {
            Order::new_market(2, SymbolId(0), Side::Buy, tif, FixedQty::from_f64(6.0), 2)
        } else {
            Order::new_limit(
                2,
                SymbolId(0),
                Side::Buy,
                tif,
                FixedPrice::from_f64(100.0),
                FixedQty::from_f64(6.0),
                2,
            )
        };
        let events = e.process(Command::PlaceOrder(taker)).to_vec();
        let filled: u64 = events
            .iter()
            .filter_map(|ev| {
                if let MatchEvent::Trade(t) = ev {
                    Some(t.qty.raw())
                } else {
                    None
                }
            })
            .sum();
        assert_eq!(
            filled,
            FixedQty::from_f64(6.0).raw(),
            "market={market} tif={tif:?}: {events:?}"
        );
        assert_eq!(e.book().order_count(), 0);
    }
}
#[test]
fn replenished_slice_loses_fifo_priority_to_other_visible_makers() {
    let mut e = Engine::new(SymbolConfig::default_btc_usdt());
    e.process(Command::PlaceOrder(maker(1, 4.0, 1.0)));
    e.process(Command::PlaceOrder(maker(2, 1.0, 0.0)));
    let events = e
        .process(Command::PlaceOrder(Order::new_market(
            3,
            SymbolId(0),
            Side::Buy,
            TimeInForce::IOC,
            FixedQty::from_f64(3.0),
            3,
        )))
        .to_vec();
    let ids: Vec<_> = events
        .iter()
        .filter_map(|ev| {
            if let MatchEvent::Trade(t) = ev {
                Some(t.maker_order_id)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(ids, vec![1, 2, 1]);
}

#[test]
fn hidden_liquidity_precedes_worse_price_and_fok_shortage_is_atomic() {
    let mut e = Engine::new(SymbolConfig::default_btc_usdt());
    e.process(Command::PlaceOrder(maker(1, 4.0, 1.0)));
    let mut worse = maker(2, 1.0, 0.0);
    worse.price = FixedPrice::from_f64(101.0);
    e.process(Command::PlaceOrder(worse));
    let rejected = e
        .process(Command::PlaceOrder(Order::new_market(
            3,
            SymbolId(0),
            Side::Buy,
            TimeInForce::FOK,
            FixedQty::from_f64(6.0),
            3,
        )))
        .to_vec();
    assert!(!rejected
        .iter()
        .any(|event| matches!(event, MatchEvent::Trade(_))));
    assert_eq!(
        e.book().get_order(1).unwrap().open_qty(),
        FixedQty::from_f64(4.0)
    );
    let events = e
        .process(Command::PlaceOrder(Order::new_market(
            4,
            SymbolId(0),
            Side::Buy,
            TimeInForce::FOK,
            FixedQty::from_f64(5.0),
            4,
        )))
        .to_vec();
    let ids: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            MatchEvent::Trade(trade) => Some(trade.maker_order_id),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec![1, 1, 1, 1, 2]);
    assert_eq!(e.book().order_count(), 0);
}
