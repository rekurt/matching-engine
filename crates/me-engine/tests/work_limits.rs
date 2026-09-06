use me_core::symbol::SymbolConfig;
use me_core::{
    types::{FixedPrice, FixedQty, Side, SymbolId},
    MatchEvent, Order, TimeInForce,
};
use me_engine::{
    command::Command,
    engine::{Engine, MAX_LIVE_GTD, MAX_PENDING_STOPS},
};
fn limit(id: u64, side: Side, qty: FixedQty) -> Order {
    Order::new_limit(
        id,
        SymbolId(0),
        side,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        qty,
        id,
    )
}
#[test]
fn fifo_visible_makers_cannot_hide_work_behind_first_iceberg() {
    let mut e = Engine::new(SymbolConfig::default_btc_usdt());
    e.process(Command::PlaceOrder(
        limit(1, Side::Sell, FixedQty::from_f64(1000.0)).with_display_qty(FixedQty::from_f64(0.25)),
    ));
    for id in 2..=10001 {
        e.process(Command::PlaceOrder(limit(
            id,
            Side::Sell,
            FixedQty::from_raw(1000),
        )));
    }
    for tif in [TimeInForce::IOC, TimeInForce::FOK, TimeInForce::GTC] {
        let mut taker = limit(20000, Side::Buy, FixedQty::from_f64(1000.0));
        taker.tif = tif;
        let events = e.process(Command::PlaceOrder(taker)).to_vec();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, MatchEvent::OrderRejected { .. })),
            "{events:?}"
        );
        assert!(!events
            .iter()
            .any(|event| matches!(event, MatchEvent::Trade(_))));
        assert_eq!(e.book().order_count(), 10001);
    }
}
#[test]
fn too_many_iceberg_slices_are_rejected_at_admission() {
    let mut e = Engine::new(SymbolConfig::default_btc_usdt());
    let events = e
        .process(Command::PlaceOrder(
            limit(1, Side::Sell, FixedQty::from_f64(10000.0))
                .with_display_qty(FixedQty::from_raw(1000)),
        ))
        .to_vec();
    assert!(events
        .iter()
        .any(|event| matches!(event, MatchEvent::OrderRejected { .. })));
    assert_eq!(e.book().order_count(), 0);
}
#[test]
fn pending_stop_capacity_releases_on_cancel_and_rejects_oversized_snapshot() {
    let mut e = Engine::new(SymbolConfig::default_btc_usdt());
    let stop = |id| {
        Order::new_stop_limit(
            id,
            SymbolId(0),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_f64(100.0),
            FixedPrice::from_f64(101.0),
            FixedQty::from_f64(1.0),
            id,
        )
    };
    for id in 1..=MAX_PENDING_STOPS as u64 {
        e.process(Command::PlaceOrder(stop(id)));
    }
    let next = MAX_PENDING_STOPS as u64 + 1;
    assert!(e
        .process(Command::PlaceOrder(stop(next)))
        .iter()
        .any(|event| matches!(event, MatchEvent::OrderRejected { .. })));
    e.process(Command::CancelOrder {
        order_id: 1,
        symbol: SymbolId(0),
        timestamp: 10,
    });
    assert!(e
        .process(Command::PlaceOrder(stop(next)))
        .iter()
        .any(|event| matches!(event, MatchEvent::OrderAccepted { .. })));
    let mut snapshot = e.snapshot();
    snapshot.stop_orders.push(stop(next + 1));
    assert!(e.try_apply_snapshot(snapshot).is_err());
    assert_eq!(e.stop_book().len(), MAX_PENDING_STOPS);
}
#[test]
fn gtd_capacity_counts_live_unique_ids_and_releases_on_cancel() {
    let mut e = Engine::new(SymbolConfig::default_btc_usdt());
    let gtd = |id| {
        let mut order = limit(id, Side::Buy, FixedQty::from_f64(1.0));
        order.tif = TimeInForce::GTD;
        order.expire_at = u64::MAX - 1;
        order
    };
    for id in 1..=MAX_LIVE_GTD as u64 {
        e.process(Command::PlaceOrder(gtd(id)));
    }
    assert!(e
        .process(Command::PlaceOrder(gtd(99999)))
        .iter()
        .any(|event| matches!(event, MatchEvent::OrderRejected { .. })));
    for _ in 0..10 {
        e.process(Command::CancelOrder {
            order_id: 1,
            symbol: SymbolId(0),
            timestamp: 10,
        });
        assert!(e
            .process(Command::PlaceOrder(gtd(1)))
            .iter()
            .any(|event| matches!(event, MatchEvent::OrderAccepted { .. })));
        assert_eq!(e.gtd_pending(), MAX_LIVE_GTD);
    }
    let mut snapshot = e.snapshot();
    snapshot.resting_orders.push(gtd(99999));
    assert!(e.try_apply_snapshot(snapshot).is_err());
}
#[test]
fn aggressive_gtd_iceberg_tracks_expiry_after_first_slice_fills() {
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };
    struct Clock(AtomicU64);
    impl me_clock::Clock for Clock {
        fn now_ns(&self) -> u64 {
            self.0.load(Ordering::Relaxed)
        }
    }
    let clock = Arc::new(Clock(AtomicU64::new(1)));
    let mut e = Engine::with_clock(SymbolConfig::default_btc_usdt(), clock.clone());
    e.process(Command::PlaceOrder(limit(
        1,
        Side::Sell,
        FixedQty::from_f64(0.1),
    )));
    let mut ice =
        limit(2, Side::Buy, FixedQty::from_f64(1.0)).with_display_qty(FixedQty::from_f64(0.1));
    ice.tif = TimeInForce::GTD;
    ice.expire_at = 100;
    e.process(Command::PlaceOrder(ice));
    assert_eq!(e.gtd_pending(), 1);
    clock.0.store(100, Ordering::Relaxed);
    let events = e.expire_gtd_now();
    assert!(events.iter().any(|event|matches!(event,MatchEvent::OrderCancelled{order_id:2,remaining_qty,..} if *remaining_qty==FixedQty::from_f64(0.9))));
    assert!(!e.book().contains_order(2));
}
