use me_core::{
    events::MatchEvent,
    order::{Order, TimeInForce},
    symbol::SymbolConfig,
    types::*,
    SessionId,
};
use me_engine::{
    command::Command,
    router::{Router, WalConfig, WalMode},
};
use me_risk::config::RiskConfig;
fn cfg(name: &str) -> WalConfig {
    let p = std::env::temp_dir().join(format!("me_review_{}_{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    WalConfig {
        mode: WalMode::Pre,
        path: p.to_str().unwrap().into(),
        sync_interval: 1,
        ..Default::default()
    }
}
fn risk() -> RiskConfig {
    let mut r = RiskConfig::default_for(SymbolId(0));
    r.max_order_qty = FixedQty::from_f64(1.0);
    r.max_orders_per_second = 100000;
    r
}
fn router(c: WalConfig) -> Router {
    let mut r = Router::new_with_wal(1, None, None, c);
    r.add_symbol_with_risk(SymbolConfig::default_btc_usdt(), Some(risk()));
    r
}
fn order(q: f64, sid: SessionId) -> Command {
    let mut o = Order::new_limit(
        1,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(100.0),
        FixedQty::from_f64(q),
        1000,
    );
    o.client_session_id = sid;
    Command::PlaceOrder(o)
}
#[test]
fn risk_rejected_order_must_not_reappear() {
    let c = cfg("risk");
    let s = SessionId::from_bytes([1; 16]);
    let r = router(c.clone());
    let e = r.process_with_session(order(2.0, s), s);
    assert!(e
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderRejected { .. })));
    assert!(r.get_open_orders_by_session(s).unwrap().is_empty());
    r.shutdown();
    let r = router(c);
    let n = r.get_open_orders_by_session(s).unwrap().len();
    r.shutdown();
    assert_eq!(n, 0, "risk-rejected order appeared after restart");
}
#[test]
fn unauthorized_cancel_must_not_apply_on_restart() {
    let c = cfg("auth");
    let s = SessionId::from_bytes([2; 16]);
    let r = router(c.clone());
    r.process_with_session(order(1.0, s), s);
    let e = r.process_with_session(
        Command::CancelOrder {
            order_id: 1,
            symbol: SymbolId(0),
            timestamp: 2000,
        },
        SessionId::from_bytes([3; 16]),
    );
    assert!(e
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderRejected { .. })));
    assert_eq!(r.get_open_orders_by_session(s).unwrap().len(), 1);
    r.shutdown();
    let r = router(c);
    let n = r.get_open_orders_by_session(s).unwrap().len();
    r.shutdown();
    assert_eq!(n, 1, "unauthorized cancel applied after restart");
}
#[test]
fn amend_must_enforce_risk_quantity() {
    let r = router(cfg("amend"));
    let s = SessionId::from_bytes([4; 16]);
    r.process_with_session(order(1.0, s), s);
    r.process_with_session(
        Command::AmendOrder {
            order_id: 1,
            symbol: SymbolId(0),
            new_price: None,
            new_qty: Some(FixedQty::from_f64(2.0)),
            timestamp: 2000,
        },
        s,
    );
    let owned = r.get_open_orders_by_session(s).unwrap();
    let unowned = r.get_open_orders_by_session(SessionId::NONE).unwrap();
    println!("amend owned={:?}, unowned={:?}", owned, unowned);
    let q = owned.iter().chain(unowned.iter()).next().unwrap().qty;
    r.shutdown();
    assert!(
        q <= FixedQty::from_f64(1.0),
        "amend bypassed max_order_qty: {:?}",
        q
    );
}
#[test]
fn corrupt_snapshot_must_not_silently_drop_orders() {
    let mut c = cfg("snapshot");
    c.snapshot_dir = Some(format!("{}/snap", c.path));
    c.snapshot_interval = 1;
    let s = SessionId::from_bytes([5; 16]);
    let r = router(c.clone());
    r.process_with_session(order(1.0, s), s);
    r.shutdown();
    for e in std::fs::read_dir(c.snapshot_dir.as_ref().unwrap()).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|e| e == "bin") {
            std::fs::write(p, b"corrupt").unwrap();
        }
    }
    let r = router(c);
    let n = r.get_open_orders_by_session(s).unwrap().len();
    r.shutdown();
    assert_eq!(
        n, 1,
        "server resumed with empty book after snapshot corruption"
    );
}
#[test]
fn wal_sequence_must_survive_snapshot_restart() {
    let mut c = cfg("watermark");
    c.snapshot_dir = Some(format!("{}/snap", c.path));
    c.snapshot_interval = 2;
    let sid = SessionId::from_bytes([6; 16]);
    let r = router(c.clone());
    r.process_with_session(order(1.0, sid), sid);
    r.process_with_session(
        Command::CancelOrder {
            order_id: 999,
            symbol: SymbolId(0),
            timestamp: 2000,
        },
        sid,
    );
    r.shutdown();
    let r = router(c.clone());
    let mut second = order(1.0, sid);
    if let Command::PlaceOrder(ref mut o) = second {
        o.id = 2;
    }
    let e = r.process_with_session(second, sid);
    assert!(e
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderAccepted { .. })));
    assert_eq!(r.get_open_orders_by_session(sid).unwrap().len(), 2);
    r.shutdown();
    let r = router(c);
    let n = r.get_open_orders_by_session(sid).unwrap().len();
    r.shutdown();
    assert_eq!(
        n, 2,
        "accepted post-restart order skipped below snapshot watermark"
    );
}

#[test]
fn durable_request_and_batch_item_replay_cached_events() {
    let c = cfg("requests");
    let sid = SessionId::from_bytes([7; 16]);
    let r = router(c.clone());
    let accepted = r.process_with_request_item(order(1.0, sid), sid, 99, 1);
    let mut second = order(1.0, sid);
    if let Command::PlaceOrder(ref mut o) = second {
        o.id = 2;
    }
    let accepted2 = r.process_with_request_item(second.clone(), sid, 99, 2);
    let rejected = r.process_with_request(order(2.0, sid), sid, 100);
    assert_eq!(r.recovered_requests().unwrap().len(), 3);
    r.shutdown();
    let r = router(c);
    assert_eq!(
        r.process_with_request_item(order(1.0, sid), sid, 99, 1),
        accepted
    );
    assert_eq!(r.process_with_request_item(second, sid, 99, 2), accepted2);
    assert_eq!(r.process_with_request(order(2.0, sid), sid, 100), rejected);
    assert_eq!(r.get_open_orders_by_session(sid).unwrap().len(), 2);
    assert_eq!(r.recovered_requests().unwrap().len(), 3);
    r.shutdown();
}
#[test]
fn torn_journal_fails_closed_without_destroying_evidence() {
    use std::io::Write;
    let c = cfg("torn");
    let sid = SessionId::from_bytes([8; 16]);
    let r = router(c.clone());
    r.process_with_session(order(1.0, sid), sid);
    r.shutdown();
    let path = std::path::Path::new(&c.path).join("shard_0.wal");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&[1, 2])
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let r = Router::new_with_wal(1, None, None, c);
    assert!(r.health().is_err());
    r.shutdown();
    assert_eq!(std::fs::read(path).unwrap(), before);
}
#[test]
fn trading_halt_survives_restart() {
    let c = cfg("halt");
    let r = router(c.clone());
    assert!(r.halt_symbol(SymbolId(0)));
    assert!(r.health().is_ok());
    r.shutdown();
    let r = router(c);
    assert_eq!(r.symbol_states().unwrap(), vec![(SymbolId(0), false)]);
    assert!(r
        .process(order(1.0, SessionId::NONE))
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderRejected { .. })));
    r.shutdown();
}
#[test]
fn symbol_config_change_refuses_recovery() {
    let c = cfg("config_drift");
    let r = router(c.clone());
    r.process(order(1.0, SessionId::NONE));
    r.shutdown();
    let mut r = Router::new_with_wal(1, None, None, c);
    let mut changed = risk();
    changed.max_order_qty = FixedQty::from_f64(2.0);
    r.add_symbol_with_risk(SymbolConfig::default_btc_usdt(), Some(changed));
    assert!(r.health().is_err());
    r.shutdown();
}
#[test]
fn topology_change_refuses_recovery() {
    let c = cfg("topology");
    let r = router(c.clone());
    r.shutdown();
    assert!(std::panic::catch_unwind(|| Router::new_with_wal(2, None, None, c)).is_err());
}
#[test]
fn duplicate_writer_refuses_health() {
    let c = cfg("locking");
    let r = router(c.clone());
    assert!(r.health().is_ok());
    let second = Router::new_with_wal(1, None, None, c);
    assert!(second.health().is_err());
    second.shutdown();
    r.shutdown();
}

#[test]
fn unknown_symbol_rejection_is_a_durable_request() {
    let c = cfg("unknown_request");
    let r = router(c.clone());
    let sid = SessionId::from_bytes([9; 16]);
    let cmd = Command::CancelOrder {
        order_id: 999,
        symbol: SymbolId(99),
        timestamp: 123,
    };
    let rejected = r.process_with_request(cmd.clone(), sid, 1);
    assert!(matches!(
        rejected[0],
        MatchEvent::OrderRejected {
            reason: me_core::RejectReason::SymbolNotFound,
            order_id: 999,
            ..
        }
    ));
    assert!(r.request_record(sid, 1).unwrap().is_some());
    r.shutdown();
    let r = router(c);
    assert!(r.health().is_ok());
    assert_eq!(r.process_with_request(cmd, sid, 1), rejected);
    r.shutdown();
}

#[test]
fn same_shard_multiple_symbols_recover_and_owners_are_isolated() {
    let mut c = cfg("multiple_symbols");
    c.snapshot_interval = 1;
    c.snapshot_dir = Some(format!("{}/snap", c.path));
    let first = SessionId::from_bytes([10; 16]);
    let second = SessionId::from_bytes([11; 16]);
    let build = || {
        let mut r = router(c.clone());
        let mut config = SymbolConfig::default_btc_usdt();
        config.id = SymbolId(1);
        config.base = "ETH".into();
        r.add_symbol(config);
        r
    };
    let r = build();
    r.process_with_request(order(1.0, first), first, 1);
    let mut cmd = order(1.0, second);
    if let Command::PlaceOrder(ref mut o) = cmd {
        o.symbol = SymbolId(1);
    }
    r.process_with_request(cmd, second, 2);
    let record = r.request_record(first, 1).unwrap().unwrap();
    assert_eq!(record.owners, vec![(1, first)]);
    r.shutdown();
    let r = build();
    assert!(r.health().is_ok());
    assert_eq!(r.get_open_orders_by_session(first).unwrap().len(), 1);
    assert_eq!(r.get_open_orders_by_session(second).unwrap().len(), 1);
    r.shutdown();
}

#[test]
fn executed_trade_and_both_owners_survive_restart() {
    let c = cfg("trade_history");
    let maker = SessionId::from_bytes([12; 16]);
    let taker = SessionId::from_bytes([13; 16]);
    let r = router(c.clone());
    r.process_with_request(order(1.0, maker), maker, 1);
    let mut cmd = order(1.0, taker);
    if let Command::PlaceOrder(ref mut o) = cmd {
        o.id = 2;
        o.side = Side::Sell;
    }
    let events = r.process_with_request(cmd.clone(), taker, 2);
    assert!(events
        .iter()
        .any(|event| matches!(event, MatchEvent::Trade(_))));
    let record = r.request_record(taker, 2).unwrap().unwrap();
    assert!(record.owners.contains(&(1, maker)));
    assert!(record.owners.contains(&(2, taker)));
    r.shutdown();
    let r = router(c);
    assert_eq!(r.process_with_request(cmd, taker, 2), events);
    assert!(r.get_open_orders_by_session(maker).unwrap().is_empty());
    assert!(r.get_open_orders_by_session(taker).unwrap().is_empty());
    r.shutdown();
}

#[test]
fn expiry_and_triggering_request_share_one_durable_decision() {
    let c = cfg("expiry_delivery");
    let owner = SessionId::from_bytes([14; 16]);
    let caller = SessionId::from_bytes([15; 16]);
    let r = router(c.clone());
    let mut gtd = order(1.0, owner);
    if let Command::PlaceOrder(ref mut o) = gtd {
        o.tif = TimeInForce::GTD;
        o.expire_at = 1;
    }
    r.process_with_request(gtd, owner, 1);
    let mut trigger = order(1.0, caller);
    if let Command::PlaceOrder(ref mut o) = trigger {
        o.id = 2;
    }
    let events = r.process_with_request(trigger.clone(), caller, 2);
    assert!(events
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderCancelled { order_id: 1, .. })));
    let record = r.request_record(caller, 2).unwrap().unwrap();
    assert!(record.owners.contains(&(1, owner)));
    r.shutdown();
    let entries = me_persist::wal::WalReader::open(format!("{}/shard_0.wal", c.path))
        .unwrap()
        .read_all()
        .unwrap();
    assert_eq!(entries.len(), 3);
    assert!(
        matches!(&entries[2].command, me_persist::wal::WalCommand::Decision { expirations, .. } if expirations.len() == 1)
    );
    let r = router(c);
    assert_eq!(r.process_with_request(trigger, caller, 2), events);
    assert!(r.get_open_orders_by_session(owner).unwrap().is_empty());
    r.shutdown();
}

#[test]
fn damaged_snapshot_and_missing_history_remain_failed_after_repeated_startup() {
    let mut c = cfg("missing_history");
    c.snapshot_interval = 1;
    c.snapshot_dir = Some(format!("{}/snap", c.path));
    let r = router(c.clone());
    r.process(order(1.0, SessionId::NONE));
    r.shutdown();
    for entry in std::fs::read_dir(c.snapshot_dir.as_ref().unwrap()).unwrap() {
        std::fs::write(entry.unwrap().path(), b"corrupt").unwrap();
    }
    let wal_path = format!("{}/shard_0.wal", c.path);
    std::fs::write(&wal_path, []).unwrap();
    for _ in 0..2 {
        let r = router(c.clone());
        assert!(r.health().is_err());
        r.shutdown();
        assert!(std::fs::read(&wal_path).unwrap().is_empty());
    }
}

#[test]
fn rejected_fok_does_not_consume_open_order_capacity() {
    let mut r = Router::new(1);
    let mut limits = risk();
    limits.max_open_orders = 1;
    r.add_symbol_with_risk(SymbolConfig::default_btc_usdt(), Some(limits));
    let mut fok = order(1.0, SessionId::NONE);
    if let Command::PlaceOrder(ref mut order) = fok {
        order.tif = TimeInForce::FOK;
    }
    assert!(r
        .process(fok)
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderRejected { .. })));
    assert!(r
        .process(order(1.0, SessionId::NONE))
        .iter()
        .any(|e| matches!(e, MatchEvent::OrderAccepted { .. })));
    r.shutdown();
}

#[cfg(unix)]
#[test]
fn bearer_credentials_in_journal_and_snapshots_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let mut c = cfg("storage_permissions");
    c.snapshot_interval = 1;
    c.snapshot_dir = Some(format!("{}/snap", c.path));
    let r = router(c.clone());
    r.process(order(1.0, SessionId::from_bytes([17; 16])));
    r.shutdown();
    let mode = std::fs::metadata(format!("{}/shard_0.wal", c.path))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    for entry in std::fs::read_dir(c.snapshot_dir.unwrap()).unwrap() {
        assert_eq!(
            entry.unwrap().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
