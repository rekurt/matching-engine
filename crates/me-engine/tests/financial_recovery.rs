#![cfg(feature = "financial")]
use me_core::symbol::SymbolConfig;
use me_engine::financial::FinancialEngine;
use me_financial::*;
use uuid::Uuid;
struct Dir(std::path::PathBuf);
impl Dir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("me-financial-{}", Uuid::new_v4()));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn command(id: i64, account: Uuid, side: Side) -> FinancialCommand {
    let request = Uuid::new_v4();
    FinancialCommand {
        request_id: request,
        operation: Operation::Place,
        order_id: id,
        account_id: account,
        version: 1,
        expected_version: 0,
        reservation_id: request,
        fee_schedule: Uuid::from_u128(1),
        spec: OrderSpec {
            symbol: 0,
            side,
            order_type: 0,
            time_in_force: 0,
            price_raw: 100_00000000,
            protection_price_raw: 100_00000000,
            qty_raw: 1_00000000,
            stop_price_raw: 0,
            trail_offset_raw: 0,
            display_qty_raw: 0,
            expire_at: 0,
            flags: 0,
        },
    }
}
fn open(dir: &Dir) -> FinancialEngine {
    FinancialEngine::open(&dir.0, vec![SymbolConfig::default_btc_usdt()]).unwrap()
}
#[test]
fn durable_decisions_replay_and_retry_with_original_identity() {
    let dir = Dir::new();
    let buyer = Uuid::new_v4();
    let seller = Uuid::new_v4();
    let mut engine = open(&dir);
    let buy = command(1, buyer, Side::Buy);
    let sell = command(2, seller, Side::Sell);
    let first = engine.submit(buy.clone(), 1000).unwrap();
    let second = engine.submit(sell.clone(), 2000).unwrap();
    assert_eq!(second.fills.len(), 1);
    assert_eq!(
        second
            .orders
            .iter()
            .filter(|o| o.status == "filled")
            .count(),
        2
    );
    let identity = engine.identity();
    drop(engine);
    let mut recovered = open(&dir);
    assert_eq!(identity.engine_id, recovered.identity().engine_id);
    assert_eq!(identity.watermarks[0].digest, recovered.watermark().digest);
    assert_eq!(
        first.digest().unwrap(),
        recovered.submit(buy, 999999).unwrap().digest().unwrap()
    );
    assert_eq!(
        second.digest().unwrap(),
        recovered.submit(sell, 999999).unwrap().digest().unwrap()
    );
    assert_eq!(recovered.watermark().sequence, 2);
    assert_eq!(
        recovered
            .decisions(0, &"0".repeat(64), 1)
            .unwrap()
            .decisions
            .len(),
        1
    );
    assert!(recovered.decisions(3, &"0".repeat(64), 1).is_err());
    assert!(recovered.decisions(1, &"0".repeat(64), 1).is_err());
    assert!(FinancialEngine::open(&dir.0, vec![SymbolConfig::default_btc_usdt()]).is_err());
}
#[test]
fn account_self_trade_and_ownership_are_enforced() {
    let dir = Dir::new();
    let mut engine = open(&dir);
    let account = Uuid::new_v4();
    engine.submit(command(1, account, Side::Buy), 1).unwrap();
    let rejected = engine.submit(command(2, account, Side::Sell), 2).unwrap();
    assert!(!rejected.commands[0].accepted);
    assert!(rejected.fills.is_empty());
    let mut cancel = command(1, Uuid::new_v4(), Side::Buy);
    cancel.operation = Operation::Cancel;
    cancel.expected_version = 1;
    assert!(engine.submit(cancel, 3).is_err());
    assert!(engine.decisions(0, &"0".repeat(64), 10).is_err());
    drop(engine);
    assert_eq!(open(&dir).watermark().sequence, 2);
}
#[test]
fn maintenance_barrier_freezes_expiry_until_resume() {
    let dir = Dir::new();
    let mut engine = open(&dir);
    let mut buy = command(1, Uuid::new_v4(), Side::Buy);
    buy.spec.time_in_force = 3;
    buy.spec.expire_at = 100;
    engine.submit(buy.clone(), 1).unwrap();
    let watermark = engine.maintenance(true, 50).unwrap();
    assert!(engine.tick(101).unwrap().is_none());
    assert!(engine
        .submit(command(2, Uuid::new_v4(), Side::Sell), 101)
        .is_err());
    // Replaying an already committed request is permitted behind the admission barrier.
    assert_eq!(engine.submit(buy, 101).unwrap().sequence, 1);
    assert_eq!(engine.watermark().sequence, watermark.sequence);
    drop(engine);
    let mut engine = open(&dir);
    assert!(engine.identity().maintenance);
    engine.maintenance(false, 102).unwrap();
    let expiry = engine.tick(103).unwrap().unwrap();
    assert_eq!(expiry.orders[0].status, "cancelled");
    drop(engine);
    assert_eq!(open(&dir).watermark().sequence, 4);
}
#[test]
fn corrupted_header_is_not_mistaken_for_an_uncommitted_tail() {
    let dir = Dir::new();
    let mut engine = open(&dir);
    engine
        .submit(command(1, Uuid::new_v4(), Side::Buy), 1)
        .unwrap();
    drop(engine);
    let path = dir.0.join("active/financial.wal");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[8] ^= 1;
    std::fs::write(path, bytes).unwrap();
    assert!(FinancialEngine::open(&dir.0, vec![SymbolConfig::default_btc_usdt()]).is_err());
}
#[test]
fn stop_market_protection_survives_restart_and_activation() {
    let dir = Dir::new();
    let mut engine = open(&dir);
    let buyer = Uuid::new_v4();
    let mut stop = command(1, buyer, Side::Buy);
    stop.spec.order_type = 3;
    stop.spec.stop_price_raw = 100_00000000;
    engine.submit(stop, 1).unwrap();
    engine
        .submit(command(2, Uuid::new_v4(), Side::Sell), 2)
        .unwrap();
    let mut high = command(3, Uuid::new_v4(), Side::Sell);
    high.spec.price_raw = 101_00000000;
    engine.submit(high, 3).unwrap();
    drop(engine);
    let mut engine = open(&dir);
    let result = engine
        .submit(command(4, Uuid::new_v4(), Side::Buy), 4)
        .unwrap();
    assert_eq!(result.fills.len(), 1);
    assert!(result
        .orders
        .iter()
        .any(|o| o.order_id == 1 && o.status == "cancelled"));
    drop(engine);
    assert_eq!(open(&dir).watermark().sequence, 4);
}

#[test]
fn interrupted_initial_generation_does_not_publish_a_partial_identity() {
    let dir = Dir::new();
    let partial = dir.0.join(".initializing-interrupted");
    std::fs::create_dir(&partial).unwrap();
    std::fs::write(partial.join("financial.json"), b"partial").unwrap();
    let mut engine = open(&dir);
    engine
        .submit(command(1, Uuid::new_v4(), Side::Buy), 1)
        .unwrap();
    let id = engine.identity().engine_id;
    drop(engine);
    assert_eq!(open(&dir).identity().engine_id, id);
}
#[test]
fn expired_crossing_gtd_is_rejected_before_any_fill() {
    let dir = Dir::new();
    let mut engine = open(&dir);
    engine
        .submit(command(1, Uuid::new_v4(), Side::Sell), 1)
        .unwrap();
    let mut expired = command(2, Uuid::new_v4(), Side::Buy);
    expired.spec.time_in_force = 3;
    expired.spec.expire_at = 2;
    let result = engine.submit(expired, 3).unwrap();
    assert!(!result.commands[0].accepted);
    assert!(result.fills.is_empty());
    assert_eq!(result.orders[0].status, "rejected");
    drop(engine);
    assert_eq!(open(&dir).watermark().sequence, 2);
}

#[test]
fn maintenance_snapshot_preserves_financial_bindings_and_detects_tampering() {
    let dir = Dir::new();
    let account = Uuid::new_v4();
    let mut engine = open(&dir);
    let mut stop = command(1, account, Side::Buy);
    stop.spec.order_type = 3;
    stop.spec.stop_price_raw = 105_00000000;
    stop.spec.trail_offset_raw = 5_00000000;
    stop.spec.protection_price_raw = 110_00000000;
    engine.submit(stop.clone(), 1).unwrap();
    engine.maintenance(true, 2).unwrap();
    let path = dir.0.join("active/financial.snapshot.json");
    let mut snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        snapshot["payload"]["orders"]["1"]["account_id"],
        account.to_string()
    );
    assert_eq!(
        snapshot["payload"]["orders"]["1"]["reservation_id"],
        stop.reservation_id.to_string()
    );
    assert_eq!(
        snapshot["payload"]["orders"]["1"]["spec"]["protection_price_raw"],
        110_00000000_i64
    );
    drop(engine);
    let engine = open(&dir);
    assert!(engine.identity().maintenance);
    drop(engine);
    snapshot["payload"]["orders"]["1"]["version"] = serde_json::json!(2);
    snapshot["digest"] = serde_json::json!(fingerprint(&snapshot["payload"]).unwrap());
    std::fs::write(path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
    assert!(FinancialEngine::open(&dir.0, vec![SymbolConfig::default_btc_usdt()]).is_err());
}

#[test]
fn multi_market_expiry_is_bounded_durable_and_drained_before_matching() {
    let dir = Dir::new();
    let mut second = SymbolConfig::default_btc_usdt();
    second.id = me_core::types::SymbolId(1);
    second.base = "ETH".into();
    let symbols = vec![SymbolConfig::default_btc_usdt(), second];
    let mut engine = FinancialEngine::open(&dir.0, symbols.clone()).unwrap();
    let account = Uuid::new_v4();
    for id in 1..=600 {
        let mut order = command(id, account, Side::Buy);
        order.spec.symbol = if id <= 300 { 0 } else { 1 };
        order.spec.time_in_force = 3;
        order.spec.expire_at = 10000;
        engine.submit(order, id as u64).unwrap();
    }
    let first = engine.tick(10000).unwrap().unwrap();
    assert_eq!(first.orders.len(), 128);
    assert!(serde_json::to_vec(&first).unwrap().len() < 8 * 1024 * 1024);
    drop(engine);
    let mut engine = FinancialEngine::open(&dir.0, symbols.clone()).unwrap();
    let sell = command(601, Uuid::new_v4(), Side::Sell);
    let result = engine.submit(sell, 10000).unwrap();
    assert!(
        result.fills.is_empty(),
        "expired liquidity must not execute"
    );
    let mut after = 0;
    let mut digest = "0".repeat(64);
    let mut expired = 0;
    let mut batches = 0;
    loop {
        let page = engine.decisions(after, &digest, 256).unwrap();
        if page.decisions.is_empty() {
            break;
        }
        for decision in page.decisions {
            if decision.commands.is_empty() {
                assert!(decision.orders.len() <= 128);
                expired += decision.orders.len();
                batches += 1;
                assert!(decision.orders.iter().all(|o| o.status == "cancelled"));
            }
            after = decision.sequence;
            digest = decision.digest().unwrap();
        }
    }
    assert_eq!(expired, 600);
    assert_eq!(batches, 6);
    drop(engine);
    assert_eq!(
        FinancialEngine::open(&dir.0, symbols)
            .unwrap()
            .watermark()
            .sequence,
        607
    );
}
