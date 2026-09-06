use me_core::{
    events::MatchEvent,
    order::{Order, TimeInForce},
    symbol::SymbolConfig,
    types::*,
};
use me_engine::{
    command::Command,
    router::{Router, WalConfig, WalMode},
};
use me_metrics::EngineMetrics;
use std::sync::Arc;

fn limit_buy() -> Command {
    Command::PlaceOrder(Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(1.0),
        1,
    ))
}

#[test]
fn duplicate_registration_preserves_live_state_and_halt() {
    let mut router = Router::new(1);
    router.add_symbol(SymbolConfig::default_btc_usdt());
    router.process(limit_buy());
    assert!(router.halt_symbol(SymbolId(0)));
    let before = router.query_order(1, SymbolId(0));
    assert!(matches!(
        before.as_slice(),
        [MatchEvent::OrderStatus { .. }]
    ));
    let duplicate = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        router.add_symbol(SymbolConfig::default_btc_usdt())
    }));
    assert!(
        duplicate.is_err(),
        "duplicate symbol must be rejected before mutation"
    );
    assert_eq!(router.query_order(1, SymbolId(0)), before);
    assert_eq!(router.symbol_states().unwrap(), vec![(SymbolId(0), false)]);
    router.shutdown();
}

#[test]
fn router_records_real_wal_flushes_and_queue_depth() {
    let path = std::env::temp_dir().join(format!("me_router_metrics_{}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    let metrics = Arc::new(EngineMetrics::new(&Default::default()));
    let mut router = Router::new_with_wal(
        1,
        Some(metrics.clone()),
        None,
        WalConfig {
            mode: WalMode::Pre,
            path: path.to_string_lossy().into_owned(),
            sync_interval: 1,
            ..Default::default()
        },
    );
    router.add_symbol(SymbolConfig::default_btc_usdt());
    router.health().unwrap();
    let before = metrics.wal_flush_duration_seconds.get_sample_count();
    assert!(before > 0, "symbol config fsync must be observed");
    router.process(limit_buy());
    assert_eq!(
        metrics.wal_flush_duration_seconds.get_sample_count(),
        before + 1
    );
    router.health().unwrap();
    assert_eq!(metrics.shard_queue_depth.with_label_values(&["0"]).get(), 0);
    router.shutdown();
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn duplicate_registration_does_not_change_durable_state() {
    let path = std::env::temp_dir().join(format!("me_router_duplicate_{}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    let cfg = WalConfig {
        mode: WalMode::Pre,
        path: path.to_string_lossy().into_owned(),
        sync_interval: 1,
        ..Default::default()
    };
    let mut router = Router::new_with_wal(1, None, None, cfg.clone());
    router.add_symbol(SymbolConfig::default_btc_usdt());
    router.process(limit_buy());
    assert!(router.halt_symbol(SymbolId(0)));
    let before = router.query_order(1, SymbolId(0));
    let mut changed = SymbolConfig::default_btc_usdt();
    changed.base = "OTHER".into();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| router.add_symbol(changed)))
            .is_err()
    );
    assert_eq!(router.query_order(1, SymbolId(0)), before);
    router.shutdown();
    let mut recovered = Router::new_with_wal(1, None, None, cfg);
    recovered.add_symbol(SymbolConfig::default_btc_usdt());
    recovered.health().unwrap();
    assert_eq!(recovered.query_order(1, SymbolId(0)), before);
    assert_eq!(
        recovered.symbol_states().unwrap(),
        vec![(SymbolId(0), false)]
    );
    recovered.shutdown();
    std::fs::remove_dir_all(path).unwrap();
}
