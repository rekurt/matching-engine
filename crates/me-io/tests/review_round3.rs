use me_core::symbol::SymbolConfig;
use me_engine::router::Router;
use me_io::session_store::SessionStore;
use std::time::Duration;

#[test]
fn expired_reused_order_id_keeps_original_notification_owner() {
    use me_core::{
        types::{FixedPrice, FixedQty, Side, SymbolId},
        Order, TimeInForce,
    };
    use me_engine::command::Command;
    use me_io::session_store::ReplayResult;
    let mut router = Router::new(1);
    router.add_symbol(SymbolConfig::default_btc_usdt());
    let store = SessionStore::new(32, Duration::from_secs(60));
    let owner = store.create();
    let requester = store.create();
    let now = me_clock::Clock::now_ns(&me_clock::SystemClock);
    let mut order = Order::new_limit(
        42,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTD,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(1.0),
        now,
    );
    order.client_session_id = owner;
    order.expire_at = now + 5_000_000;
    router.process_with_request(Command::PlaceOrder(order), owner, 1);
    store
        .commit_request(router.request_record(owner, 1).unwrap().unwrap())
        .unwrap();
    std::thread::sleep(Duration::from_millis(10));
    order.id = 42;
    order.client_session_id = requester;
    order.tif = TimeInForce::GTC;
    order.expire_at = 0;
    router.process_with_request(Command::PlaceOrder(order), requester, 1);
    store
        .commit_request(router.request_record(requester, 1).unwrap().unwrap())
        .unwrap();
    let ReplayResult::Ok(frames) = store.resume(&owner, 0) else {
        panic!("owner replay unavailable");
    };
    router.shutdown();
    assert_eq!(
        frames.len(),
        2,
        "expired GTD owner must receive terminal cancellation after accepted"
    );
}
