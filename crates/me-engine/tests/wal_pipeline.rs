/// Integration tests for WAL pipeline in the Router.
use me_core::events::MatchEvent;
use me_core::order::{Order, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::*;
use me_core::RejectReason;
use me_engine::command::Command;
use me_engine::router::{Router, WalConfig, WalMode};
use me_persist::wal::{WalReader, WalCommand};

fn btc_config() -> SymbolConfig {
    SymbolConfig::default_btc_usdt()
}

fn temp_wal_dir(name: &str) -> String {
    let dir = std::env::temp_dir()
        .join("me_wal_pipeline_tests")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.to_str().unwrap().to_string()
}

fn limit_buy(id: u64, price: f64, qty: f64) -> Command {
    Command::PlaceOrder(Order::new_limit(
        id, SymbolId(0), Side::Buy, TimeInForce::GTC,
        FixedPrice::from_f64(price), FixedQty::from_f64(qty), id * 1000,
    ))
}

fn limit_sell(id: u64, price: f64, qty: f64) -> Command {
    Command::PlaceOrder(Order::new_limit(
        id, SymbolId(0), Side::Sell, TimeInForce::GTC,
        FixedPrice::from_f64(price), FixedQty::from_f64(qty), id * 1000,
    ))
}

// ─── WAL Pre-mode tests ─────────────────────────────────────────────────────

#[test]
fn test_wal_pre_mode_writes_before_matching() {
    let wal_dir = temp_wal_dir("pre_mode");
    let wal_cfg = WalConfig {
        mode: WalMode::Pre,
        path: wal_dir.clone(),
        sync_interval: 1, // flush every command
    };

    {
        let mut router = Router::new_with_wal(1, None, None, wal_cfg);
        router.add_symbol(btc_config());

        // Place a sell, then a buy that crosses → trade
        router.process(limit_sell(1, 100.0, 1.0));
        router.process(limit_buy(2, 100.0, 1.0));
        router.process(Command::CancelOrder {
            order_id: 999, // non-existent, still journaled in pre-mode
            symbol: SymbolId(0),
            timestamp: 3000,
        });

        router.shutdown();
    }

    // Read WAL and verify entries
    let wal_path = format!("{}/shard_0.wal", wal_dir);
    let mut reader = WalReader::open(&wal_path).unwrap();
    let entries = reader.read_all().unwrap();

    assert_eq!(entries.len(), 3, "All 3 commands should be in WAL");
    assert_eq!(entries[0].sequence, 1);
    assert_eq!(entries[1].sequence, 2);
    assert_eq!(entries[2].sequence, 3);

    // First entry should be PlaceOrder (sell)
    match &entries[0].command {
        WalCommand::PlaceOrder(o) => assert_eq!(o.id, 1),
        _ => panic!("Expected PlaceOrder"),
    }
    // Second entry should be PlaceOrder (buy)
    match &entries[1].command {
        WalCommand::PlaceOrder(o) => assert_eq!(o.id, 2),
        _ => panic!("Expected PlaceOrder"),
    }
    // Third should be CancelOrder
    match &entries[2].command {
        WalCommand::CancelOrder { order_id, .. } => assert_eq!(*order_id, 999),
        _ => panic!("Expected CancelOrder"),
    }
}

#[test]
fn test_wal_pre_mode_open_failure_rejects_and_preserves_state() {
    let base_dir = temp_wal_dir("pre_mode_open_failure");
    let blocked_parent = std::path::Path::new(&base_dir).join("blocked_parent");
    std::fs::write(&blocked_parent, b"not-a-directory").unwrap();

    let wal_cfg = WalConfig {
        mode: WalMode::Pre,
        path: blocked_parent.to_string_lossy().into_owned(),
        sync_interval: 1,
    };

    let mut router = Router::new_with_wal(1, None, None, wal_cfg);
    router.add_symbol(btc_config());

    let events = router.process(limit_buy(1, 100.0, 1.0));
    assert!(
        events.iter().any(|e| matches!(
            e,
            MatchEvent::OrderRejected {
                order_id: 1,
                reason: RejectReason::InternalError,
                ..
            }
        )),
        "command must be rejected when pre-mode WAL writer is unavailable"
    );

    let order_status = router.query_order(1, SymbolId(0));
    assert!(
        order_status.iter().any(|e| matches!(
            e,
            MatchEvent::OrderNotFound { order_id: 1 }
        )),
        "rejected order must not be present in the order book"
    );

    router.shutdown();
}

#[test]
fn test_wal_post_mode_skips_risk_rejected() {
    let wal_dir = temp_wal_dir("post_mode");
    let wal_cfg = WalConfig {
        mode: WalMode::Post,
        path: wal_dir.clone(),
        sync_interval: 1,
    };

    {
        let mut router = Router::new_with_wal(1, None, None, wal_cfg);

        let mut risk_cfg = me_risk::config::RiskConfig::default_for(SymbolId(0));
        risk_cfg.max_order_qty = FixedQty::from_f64(5.0); // max 5 BTC
        risk_cfg.max_orders_per_second = 1_000_000;
        router.add_symbol_with_risk(btc_config(), Some(risk_cfg));

        // Normal order (should be journaled)
        let events = router.process(limit_buy(1, 100.0, 1.0));
        assert!(events.iter().any(|e| matches!(e, MatchEvent::OrderAccepted { .. })));

        // Fat finger order (should be risk-rejected, NOT journaled in post-mode)
        let events = router.process(limit_buy(2, 100.0, 50.0));
        assert!(events.iter().any(|e| matches!(e, MatchEvent::OrderRejected { .. })));

        // Another normal order
        let events = router.process(limit_sell(3, 100.0, 1.0));
        assert!(events.iter().any(|e| matches!(e, MatchEvent::OrderAccepted { .. })));

        router.shutdown();
    }

    // In post-mode, only accepted commands are journaled
    let wal_path = format!("{}/shard_0.wal", wal_dir);
    let mut reader = WalReader::open(&wal_path).unwrap();
    let entries = reader.read_all().unwrap();

    // Risk-rejected order should NOT be in WAL
    assert_eq!(entries.len(), 2, "Only 2 accepted commands should be in WAL (risk-rejected excluded)");
    match &entries[0].command {
        WalCommand::PlaceOrder(o) => assert_eq!(o.id, 1),
        _ => panic!("Expected PlaceOrder id=1"),
    }
    match &entries[1].command {
        WalCommand::PlaceOrder(o) => assert_eq!(o.id, 3),
        _ => panic!("Expected PlaceOrder id=3"),
    }
}

#[test]
fn test_wal_disabled_no_file() {
    let wal_dir = temp_wal_dir("disabled");
    let wal_cfg = WalConfig {
        mode: WalMode::Disabled,
        path: wal_dir.clone(),
        sync_interval: 1,
    };

    {
        let mut router = Router::new_with_wal(1, None, None, wal_cfg);
        router.add_symbol(btc_config());
        router.process(limit_buy(1, 100.0, 1.0));
        router.shutdown();
    }

    // No WAL file should exist
    let wal_path = format!("{}/shard_0.wal", wal_dir);
    assert!(!std::path::Path::new(&wal_path).exists(), "WAL file should not exist when disabled");
}

#[test]
fn test_wal_batch_flush() {
    let wal_dir = temp_wal_dir("batch_flush");
    let wal_cfg = WalConfig {
        mode: WalMode::Pre,
        path: wal_dir.clone(),
        sync_interval: 50, // flush every 50 commands
    };

    {
        let mut router = Router::new_with_wal(1, None, None, wal_cfg);
        router.add_symbol(btc_config());

        // Send 100 commands
        for i in 1..=100u64 {
            router.process(limit_buy(i, 100.0 - (i as f64) * 0.01, 0.1));
        }

        router.shutdown(); // flush on shutdown
    }

    let wal_path = format!("{}/shard_0.wal", wal_dir);
    let mut reader = WalReader::open(&wal_path).unwrap();
    let entries = reader.read_all().unwrap();

    assert_eq!(entries.len(), 100);
    assert_eq!(entries.last().unwrap().sequence, 100);
}

#[test]
fn test_wal_recovery_restores_book_state() {
    let wal_dir = temp_wal_dir("recovery");
    let wal_cfg = WalConfig {
        mode: WalMode::Pre,
        path: wal_dir.clone(),
        sync_interval: 1,
    };

    // Phase 1: Process commands and write WAL
    {
        let mut router = Router::new_with_wal(1, None, None, wal_cfg);
        router.add_symbol(btc_config());

        // Place 5 bids at different prices
        for i in 1..=5u64 {
            router.process(limit_buy(i, 99.0 + (i as f64) * 0.01, 1.0));
        }
        // Cancel order 3
        router.process(Command::CancelOrder {
            order_id: 3,
            symbol: SymbolId(0),
            timestamp: 10000,
        });

        router.shutdown();
    }

    // Phase 2: Read WAL and replay on a fresh engine
    let wal_path = format!("{}/shard_0.wal", wal_dir);
    let mut reader = WalReader::open(&wal_path).unwrap();
    let entries = reader.read_all().unwrap();
    assert_eq!(entries.len(), 6); // 5 place + 1 cancel

    // Replay on a fresh engine
    let mut engine = me_engine::engine::Engine::new(btc_config());
    for entry in &entries {
        let cmd = match &entry.command {
            WalCommand::PlaceOrder(order) => Command::PlaceOrder(*order),
            WalCommand::CancelOrder { order_id, symbol, timestamp } => Command::CancelOrder {
                order_id: *order_id,
                symbol: *symbol,
                timestamp: *timestamp,
            },
            WalCommand::AmendOrder { order_id, symbol, new_price, new_qty, timestamp } => {
                Command::AmendOrder {
                    order_id: *order_id,
                    symbol: *symbol,
                    new_price: *new_price,
                    new_qty: *new_qty,
                    timestamp: *timestamp,
                }
            }
        };
        engine.process(cmd);
    }

    // Verify: 4 orders remaining (5 placed - 1 cancelled)
    assert_eq!(engine.book().order_count(), 4);
    assert!(engine.book().contains_order(1));
    assert!(engine.book().contains_order(2));
    assert!(!engine.book().contains_order(3)); // cancelled
    assert!(engine.book().contains_order(4));
    assert!(engine.book().contains_order(5));
}

#[test]
fn test_wal_multi_shard() {
    let wal_dir = temp_wal_dir("multi_shard");
    let wal_cfg = WalConfig {
        mode: WalMode::Pre,
        path: wal_dir.clone(),
        sync_interval: 1,
    };

    {
        let mut router = Router::new_with_wal(2, None, None, wal_cfg); // 2 shards

        let mut cfg0 = btc_config();
        cfg0.id = SymbolId(0);
        router.add_symbol(cfg0);

        let mut cfg1 = btc_config();
        cfg1.id = SymbolId(1);
        cfg1.base = "ETH".to_string();
        router.add_symbol(cfg1);

        // Symbol 0 → shard 0, Symbol 1 → shard 1
        router.process(Command::PlaceOrder(Order::new_limit(
            1, SymbolId(0), Side::Buy, TimeInForce::GTC,
            FixedPrice::from_f64(100.0), FixedQty::from_f64(1.0), 1000,
        )));
        router.process(Command::PlaceOrder(Order::new_limit(
            2, SymbolId(1), Side::Sell, TimeInForce::GTC,
            FixedPrice::from_f64(50.0), FixedQty::from_f64(2.0), 2000,
        )));

        router.shutdown();
    }

    // Each shard should have its own WAL file
    let wal0 = format!("{}/shard_0.wal", wal_dir);
    let wal1 = format!("{}/shard_1.wal", wal_dir);

    let entries0 = WalReader::open(&wal0).unwrap().read_all().unwrap();
    let entries1 = WalReader::open(&wal1).unwrap().read_all().unwrap();

    assert_eq!(entries0.len(), 1);
    assert_eq!(entries1.len(), 1);

    match &entries0[0].command {
        WalCommand::PlaceOrder(o) => assert_eq!(o.symbol, SymbolId(0)),
        _ => panic!("Expected PlaceOrder on shard 0"),
    }
    match &entries1[0].command {
        WalCommand::PlaceOrder(o) => assert_eq!(o.symbol, SymbolId(1)),
        _ => panic!("Expected PlaceOrder on shard 1"),
    }
}
