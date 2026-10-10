/// Integration tests for WAL pipeline in the Router.
use me_core::events::MatchEvent;
use me_core::order::{Order, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::*;
use me_engine::command::Command;
use me_engine::router::{Router, WalConfig, WalMode};
use me_persist::wal::{WalCommand, WalEntry, WalReader};

fn commands(entries: Vec<WalEntry>) -> Vec<WalEntry> {
    let mut commands = Vec::new();
    for entry in entries {
        match entry.command {
            WalCommand::Decision {
                command,
                expirations,
                ..
            } => {
                for expiration in expirations {
                    if let MatchEvent::OrderCancelled {
                        order_id,
                        timestamp,
                        ..
                    } = expiration
                    {
                        commands.push(WalEntry {
                            sequence: entry.sequence,
                            command: WalCommand::CancelOrder {
                                order_id,
                                timestamp,
                                symbol: command.symbol(),
                            },
                        });
                    }
                }
                commands.push(WalEntry {
                    sequence: entry.sequence,
                    command: *command,
                });
            }
            WalCommand::SymbolConfig { .. } | WalCommand::TradingState { .. } => {}
            command => commands.push(WalEntry {
                sequence: entry.sequence,
                command,
            }),
        }
    }
    commands
}

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
        id,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(price),
        FixedQty::from_f64(qty),
        id * 1000,
    ))
}

fn limit_sell(id: u64, price: f64, qty: f64) -> Command {
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

/// A GTD buy whose `expire_at` is in the past (1 ns) so the next lazy sweep
/// (SystemClock now ≫ 1) cancels it. The order still rests when first placed —
/// the sweep runs before each *subsequent* command.
fn gtd_buy_expired(id: u64, price: f64, qty: f64) -> Command {
    Command::PlaceOrder(
        Order::new_limit(
            id,
            SymbolId(0),
            Side::Buy,
            TimeInForce::GTD,
            FixedPrice::from_f64(price),
            FixedQty::from_f64(qty),
            id * 1000,
        )
        .with_expiry(1),
    )
}

#[test]
fn test_wal_pre_mode_writes_before_matching() {
    let wal_dir = temp_wal_dir("pre_mode");
    let wal_cfg = WalConfig {
        mode: WalMode::Pre,
        path: wal_dir.clone(),
        sync_interval: 1, // flush every command
        ..Default::default()
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
    let entries = commands(reader.read_all().unwrap());

    assert_eq!(entries.len(), 3, "All 3 commands should be in WAL");
    assert_eq!(entries[0].sequence, 2);
    assert_eq!(entries[1].sequence, 3);
    assert_eq!(entries[2].sequence, 4);

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
        ..Default::default()
    };

    assert!(std::panic::catch_unwind(|| Router::new_with_wal(1, None, None, wal_cfg)).is_err());
}

#[test]
fn test_wal_post_mode_persists_risk_decisions() {
    let wal_dir = temp_wal_dir("post_mode");
    let wal_cfg = WalConfig {
        mode: WalMode::Post,
        path: wal_dir.clone(),
        sync_interval: 1,
        ..Default::default()
    };

    {
        let mut router = Router::new_with_wal(1, None, None, wal_cfg);

        let mut risk_cfg = me_risk::config::RiskConfig::default_for(SymbolId(0));
        risk_cfg.max_order_qty = FixedQty::from_f64(5.0); // max 5 BTC
        risk_cfg.max_orders_per_second = 1_000_000;
        router.add_symbol_with_risk(btc_config(), Some(risk_cfg));

        // Normal order (should be journaled)
        let events = router.process(limit_buy(1, 100.0, 1.0));
        assert!(events
            .iter()
            .any(|e| matches!(e, MatchEvent::OrderAccepted { .. })));

        // Fat finger order (should be risk-rejected, NOT journaled in post-mode)
        let events = router.process(limit_buy(2, 100.0, 50.0));
        assert!(events
            .iter()
            .any(|e| matches!(e, MatchEvent::OrderRejected { .. })));

        // Another normal order
        let events = router.process(limit_sell(3, 100.0, 1.0));
        assert!(events
            .iter()
            .any(|e| matches!(e, MatchEvent::OrderAccepted { .. })));

        router.shutdown();
    }

    // In post-mode, only accepted commands are journaled
    let wal_path = format!("{}/shard_0.wal", wal_dir);
    let mut reader = WalReader::open(&wal_path).unwrap();
    let entries = commands(reader.read_all().unwrap());

    // Risk-rejected order should NOT be in WAL
    assert_eq!(
        entries.len(),
        3,
        "Accepted and rejected decisions must both be durable"
    );
    match &entries[0].command {
        WalCommand::PlaceOrder(o) => assert_eq!(o.id, 1),
        _ => panic!("Expected PlaceOrder id=1"),
    }
    match &entries[2].command {
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
        ..Default::default()
    };

    {
        let mut router = Router::new_with_wal(1, None, None, wal_cfg);
        router.add_symbol(btc_config());
        router.process(limit_buy(1, 100.0, 1.0));
        router.shutdown();
    }

    // No WAL file should exist
    let wal_path = format!("{}/shard_0.wal", wal_dir);
    assert!(
        !std::path::Path::new(&wal_path).exists(),
        "WAL file should not exist when disabled"
    );
}

#[test]
fn test_wal_batch_flush() {
    let wal_dir = temp_wal_dir("batch_flush");
    let wal_cfg = WalConfig {
        mode: WalMode::Pre,
        path: wal_dir.clone(),
        sync_interval: 50, // flush every 50 commands
        ..Default::default()
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
    let entries = commands(reader.read_all().unwrap());

    assert_eq!(entries.len(), 100);
    assert_eq!(entries.last().unwrap().sequence, 101);
}

#[test]
fn test_wal_recovery_restores_book_state() {
    let wal_dir = temp_wal_dir("recovery");
    let wal_cfg = WalConfig {
        mode: WalMode::Pre,
        path: wal_dir.clone(),
        sync_interval: 1,
        ..Default::default()
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
    let entries = commands(reader.read_all().unwrap());
    assert_eq!(entries.len(), 6); // 5 place + 1 cancel

    // Replay on a fresh engine
    let mut engine = me_engine::engine::Engine::new(btc_config());
    for entry in &entries {
        let cmd = match &entry.command {
            WalCommand::Decision { .. }
            | WalCommand::SymbolConfig { .. }
            | WalCommand::TradingState { .. } => panic!("unwrapped commands only"),
            WalCommand::PlaceOrder(order) => Command::PlaceOrder(*order),
            WalCommand::CancelOrder {
                order_id,
                symbol,
                timestamp,
            } => Command::CancelOrder {
                order_id: *order_id,
                symbol: *symbol,
                timestamp: *timestamp,
            },
            WalCommand::AmendOrder {
                order_id,
                symbol,
                new_price,
                new_qty,
                timestamp,
            } => Command::AmendOrder {
                order_id: *order_id,
                symbol: *symbol,
                new_price: *new_price,
                new_qty: *new_qty,
                timestamp: *timestamp,
            },
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
fn test_router_recovers_book_after_restart() {
    // End-to-end: a fresh Router pointed at the same WAL dir must rebuild the
    // resting book on add_symbol, with no snapshots configured (full replay).
    let wal_dir = temp_wal_dir("router_recovery_replay");

    let mk_cfg = || WalConfig {
        mode: WalMode::Pre,
        path: wal_dir.clone(),
        sync_interval: 0, // fsync every write
        ..Default::default()
    };

    // Phase 1: place orders, leave 4 resting, then shut down.
    {
        let mut router = Router::new_with_wal(1, None, None, mk_cfg());
        router.add_symbol(btc_config());
        for i in 1..=5u64 {
            router.process(limit_buy(i, 99.0 + (i as f64) * 0.01, 1.0));
        }
        router.process(Command::CancelOrder {
            order_id: 3,
            symbol: SymbolId(0),
            timestamp: 10_000,
        });
        router.shutdown();
    }

    // Phase 2: brand-new router on the same dir recovers state via WAL replay.
    {
        let mut router = Router::new_with_wal(1, None, None, mk_cfg());
        router.add_symbol(btc_config());

        for id in [1u64, 2, 4, 5] {
            let st = router.query_order(id, SymbolId(0));
            assert!(
                st.iter().any(
                    |e| matches!(e, MatchEvent::OrderStatus { order_id, .. } if *order_id == id)
                ),
                "order {id} must be restored after restart"
            );
        }
        let cancelled = router.query_order(3, SymbolId(0));
        assert!(
            cancelled
                .iter()
                .any(|e| matches!(e, MatchEvent::OrderNotFound { order_id: 3 })),
            "cancelled order 3 must NOT reappear"
        );
        router.shutdown();
    }
}

#[test]
fn test_router_recovers_with_snapshot_and_wal_rotation() {
    // With periodic snapshots enabled, a snapshot is taken (and the WAL
    // truncated) mid-stream; post-snapshot commands live only in the rotated
    // WAL. Recovery must combine snapshot + post-snapshot WAL correctly.
    let wal_dir = temp_wal_dir("router_recovery_snapshot");
    let snap_dir = format!("{wal_dir}/snapshots");

    let mk_cfg = || WalConfig {
        mode: WalMode::Pre,
        path: wal_dir.clone(),
        sync_interval: 0,
        snapshot_dir: Some(snap_dir.clone()),
        snapshot_interval: 3, // snapshot after every 3 commands
    };

    {
        let mut router = Router::new_with_wal(1, None, None, mk_cfg());
        router.add_symbol(btc_config());
        // 4 commands: snapshot fires after #3 (truncating WAL), #4 goes to the
        // fresh WAL only.
        for i in 1..=4u64 {
            router.process(limit_buy(i, 99.0 + (i as f64) * 0.01, 1.0));
        }
        router.shutdown();
    }

    {
        let mut router = Router::new_with_wal(1, None, None, mk_cfg());
        router.add_symbol(btc_config());
        for id in 1..=4u64 {
            let st = router.query_order(id, SymbolId(0));
            assert!(
                st.iter().any(
                    |e| matches!(e, MatchEvent::OrderStatus { order_id, .. } if *order_id == id)
                ),
                "order {id} must be restored from snapshot+WAL"
            );
        }
        router.shutdown();
    }
}

#[test]
fn test_wal_multi_shard() {
    let wal_dir = temp_wal_dir("multi_shard");
    let wal_cfg = WalConfig {
        mode: WalMode::Pre,
        path: wal_dir.clone(),
        sync_interval: 1,
        ..Default::default()
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
            1,
            SymbolId(0),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_f64(100.0),
            FixedQty::from_f64(1.0),
            1000,
        )));
        router.process(Command::PlaceOrder(Order::new_limit(
            2,
            SymbolId(1),
            Side::Sell,
            TimeInForce::GTC,
            FixedPrice::from_f64(50.0),
            FixedQty::from_f64(2.0),
            2000,
        )));

        router.shutdown();
    }

    // Each shard should have its own WAL file
    let wal0 = format!("{}/shard_0.wal", wal_dir);
    let wal1 = format!("{}/shard_1.wal", wal_dir);

    let entries0 = commands(WalReader::open(&wal0).unwrap().read_all().unwrap());
    let entries1 = commands(WalReader::open(&wal1).unwrap().read_all().unwrap());

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

#[test]
fn test_gtd_expiry_is_journaled_before_triggering_command() {
    // A GTD order that expires during the lazy pre-command sweep MUST be
    // journaled as a durable CancelOrder BEFORE the command that triggered the
    // sweep. Otherwise crash recovery replays only PlaceOrder#1, PlaceOrder#2 —
    // resurrecting the expired GTD so it phantom-matches the later order.
    let wal_dir = temp_wal_dir("gtd_expiry_journaled");
    let wal_cfg = WalConfig {
        mode: WalMode::Pre,
        path: wal_dir.clone(),
        sync_interval: 1,
        ..Default::default()
    };

    {
        let mut router = Router::new_with_wal(1, None, None, wal_cfg);
        router.add_symbol(btc_config());
        // #1 rests now (already-past expiry is only swept on the next command).
        router.process(gtd_buy_expired(1, 100.0, 1.0));
        // #2 WOULD cross #1, but the pre-command sweep expires #1 first, so #2
        // just rests. The expiry cancel must be journaled before #2.
        router.process(limit_sell(2, 100.0, 1.0));
        router.shutdown();
    }

    let wal_path = format!("{}/shard_0.wal", wal_dir);
    let mut reader = WalReader::open(&wal_path).unwrap();
    let entries = commands(reader.read_all().unwrap());

    assert_eq!(
        entries.len(),
        3,
        "expected PlaceOrder#1, CancelOrder#1 (expiry), PlaceOrder#2; got {:?}",
        entries.iter().map(|e| &e.command).collect::<Vec<_>>()
    );
    match &entries[0].command {
        WalCommand::PlaceOrder(o) => assert_eq!(o.id, 1),
        other => panic!("entry 0 should be PlaceOrder#1, got {other:?}"),
    }
    match &entries[1].command {
        WalCommand::CancelOrder { order_id, .. } => {
            assert_eq!(
                *order_id, 1,
                "the expired GTD must be journaled as a cancel"
            );
        }
        other => panic!("entry 1 should be CancelOrder#1 (expiry), got {other:?}"),
    }
    match &entries[2].command {
        WalCommand::PlaceOrder(o) => assert_eq!(o.id, 2),
        other => panic!("entry 2 should be PlaceOrder#2, got {other:?}"),
    }
}

#[test]
fn test_recovery_does_not_resurrect_expired_gtd() {
    // End-to-end determinism: after a GTD expires live (freeing its slot) and a
    // later order rests against the now-empty side, recovery must NOT resurrect
    // the expired GTD and let it phantom-match the later order.
    let wal_dir = temp_wal_dir("gtd_recovery_no_resurrect");
    let mk_cfg = || WalConfig {
        mode: WalMode::Pre,
        path: wal_dir.clone(),
        sync_interval: 0, // fsync every write
        ..Default::default()
    };

    // Phase 1: live run.
    {
        let mut router = Router::new_with_wal(1, None, None, mk_cfg());
        router.add_symbol(btc_config());
        router.process(gtd_buy_expired(1, 100.0, 1.0)); // rests
        router.process(limit_sell(2, 100.0, 1.0)); // sweep expires #1 → #2 rests
        router.shutdown();
    }

    // Phase 2: brand-new router recovers from the same WAL dir.
    {
        let mut router = Router::new_with_wal(1, None, None, mk_cfg());
        router.add_symbol(btc_config());

        let q1 = router.query_order(1, SymbolId(0));
        assert!(
            q1.iter()
                .any(|e| matches!(e, MatchEvent::OrderNotFound { order_id: 1 })),
            "expired GTD #1 must NOT be resurrected after recovery, got {q1:?}"
        );

        let q2 = router.query_order(2, SymbolId(0));
        assert!(
            q2.iter()
                .any(|e| matches!(e, MatchEvent::OrderStatus { order_id: 2, .. })),
            "order #2 must still rest after recovery (no phantom match), got {q2:?}"
        );
        router.shutdown();
    }
}
