/// Persistence tests: WAL replay, snapshot save/restore, recovery scenarios.
/// Covers test scenarios K (78-84) and L.91 from the plan.

use std::fs;
use std::path::PathBuf;

use me_core::events::MatchEvent;
use me_core::order::{Order, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::*;
use me_engine::command::Command;
use me_engine::engine::Engine;
use me_persist::snapshot::{BookSnapshot, SnapshotManager};
use me_persist::wal::{WalCommand, WalReader, WalWriter};

fn config() -> SymbolConfig {
    SymbolConfig::default_btc_usdt()
}

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join("me_persist_tests").join(name);
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn limit_buy_order(id: OrderId, price_raw: i64, qty_raw: u64) -> Order {
    Order::new_limit(
        id, SymbolId(0), Side::Buy, TimeInForce::GTC,
        FixedPrice::from_raw(price_raw), FixedQty::from_raw(qty_raw), id * 1000,
    )
}

fn limit_sell_order(id: OrderId, price_raw: i64, qty_raw: u64) -> Order {
    Order::new_limit(
        id, SymbolId(0), Side::Sell, TimeInForce::GTC,
        FixedPrice::from_raw(price_raw), FixedQty::from_raw(qty_raw), id * 1000,
    )
}

fn to_wal_cmd(cmd: &Command) -> WalCommand {
    match cmd {
        Command::PlaceOrder(o) => WalCommand::PlaceOrder(*o),
        Command::CancelOrder { order_id, symbol, timestamp } => WalCommand::CancelOrder {
            order_id: *order_id, symbol: *symbol, timestamp: *timestamp,
        },
        Command::AmendOrder { order_id, symbol, new_price, new_qty, timestamp } => WalCommand::AmendOrder {
            order_id: *order_id, symbol: *symbol, new_price: *new_price, new_qty: *new_qty, timestamp: *timestamp,
        },
    }
}

/// Rebuild an engine by replaying WAL commands.
fn replay_commands(commands: &[Command]) -> Engine {
    let mut engine = Engine::new(config());
    for cmd in commands {
        engine.process(cmd.clone());
    }
    engine
}

/// Compare two engines' book state for equality.
fn books_match(a: &Engine, b: &Engine) -> bool {
    let a_orders = a.book().all_resting_orders();
    let b_orders = b.book().all_resting_orders();
    if a_orders.len() != b_orders.len() { return false; }
    // Compare by id sets
    let a_ids: std::collections::HashSet<_> = a_orders.iter().map(|o| o.id).collect();
    let b_ids: std::collections::HashSet<_> = b_orders.iter().map(|o| o.id).collect();
    a_ids == b_ids
}

// ─── K78: WAL replay empty log → empty book ─────────────────────────────────

#[test]
fn test_k78_wal_replay_empty() {
    let dir = temp_dir("k78");
    let wal_path = dir.join("test.wal");

    {
        let mut w = WalWriter::open(&wal_path, 1).unwrap();
        w.flush().unwrap();
    }

    let mut r = WalReader::open(&wal_path).unwrap();
    let entries = r.read_all().unwrap();
    assert!(entries.is_empty());

    // Replay empty → empty engine
    let engine = replay_commands(&[]);
    assert_eq!(engine.book().order_count(), 0);
    let _ = fs::remove_dir_all(&dir);
}

// ─── K79: WAL replay N commands → identical state ────────────────────────────

#[test]
fn test_k79_wal_replay_matches_direct() {
    let dir = temp_dir("k79");
    let wal_path = dir.join("test.wal");

    let commands = vec![
        Command::PlaceOrder(limit_buy_order(1, 10_000_000_000, 100_000_000)),
        Command::PlaceOrder(limit_sell_order(2, 10_100_000_000, 100_000_000)),
        Command::PlaceOrder(limit_buy_order(3, 9_900_000_000, 200_000_000)),
        Command::PlaceOrder(limit_sell_order(4, 10_200_000_000, 150_000_000)),
    ];

    // Write WAL
    {
        let mut w = WalWriter::open(&wal_path, 1).unwrap();
        for cmd in &commands {
            w.append(&to_wal_cmd(cmd)).unwrap();
        }
        w.flush().unwrap();
    }

    // Process directly
    let direct = replay_commands(&commands);

    // Read WAL and replay
    let mut r = WalReader::open(&wal_path).unwrap();
    let entries = r.read_all().unwrap();
    assert_eq!(entries.len(), 4);

    let mut replayed = Engine::new(config());
    for entry in &entries {
        match &entry.command {
            WalCommand::PlaceOrder(o) => { replayed.process(Command::PlaceOrder(*o)); }
            WalCommand::CancelOrder { order_id, symbol, timestamp } => {
                replayed.process(Command::CancelOrder { order_id: *order_id, symbol: *symbol, timestamp: *timestamp });
            }
            WalCommand::AmendOrder { order_id, symbol, new_price, new_qty, timestamp } => {
                replayed.process(Command::AmendOrder { order_id: *order_id, symbol: *symbol, new_price: *new_price, new_qty: *new_qty, timestamp: *timestamp });
            }
        }
    }

    assert!(books_match(&direct, &replayed));
    let _ = fs::remove_dir_all(&dir);
}

// ─── K79b: WAL replay with cancel ───────────────────────────────────────────

#[test]
fn test_k79b_wal_replay_with_cancel() {
    let dir = temp_dir("k79b");
    let wal_path = dir.join("test.wal");

    let commands = vec![
        Command::PlaceOrder(limit_buy_order(1, 10_000_000_000, 100_000_000)),
        Command::PlaceOrder(limit_buy_order(2, 9_900_000_000, 200_000_000)),
        Command::CancelOrder { order_id: 1, symbol: SymbolId(0), timestamp: 5000 },
    ];

    {
        let mut w = WalWriter::open(&wal_path, 1).unwrap();
        for cmd in &commands { w.append(&to_wal_cmd(cmd)).unwrap(); }
        w.flush().unwrap();
    }

    let direct = replay_commands(&commands);
    assert_eq!(direct.book().order_count(), 1);
    assert!(!direct.book().contains_order(1));
    assert!(direct.book().contains_order(2));

    let mut r = WalReader::open(&wal_path).unwrap();
    let entries = r.read_all().unwrap();
    let mut replayed = Engine::new(config());
    for entry in &entries {
        match &entry.command {
            WalCommand::PlaceOrder(o) => { replayed.process(Command::PlaceOrder(*o)); }
            WalCommand::CancelOrder { order_id, symbol, timestamp } => {
                replayed.process(Command::CancelOrder { order_id: *order_id, symbol: *symbol, timestamp: *timestamp });
            }
            _ => {}
        }
    }
    assert!(books_match(&direct, &replayed));
    let _ = fs::remove_dir_all(&dir);
}

// ─── K80: Snapshot save and restore ──────────────────────────────────────────

#[test]
fn test_k80_snapshot_save_restore() {
    let dir = temp_dir("k80");
    let mgr = SnapshotManager::new(&dir).unwrap();

    let mut engine = Engine::new(config());
    engine.process(Command::PlaceOrder(limit_buy_order(1, 10_000_000_000, 100_000_000)));
    engine.process(Command::PlaceOrder(limit_sell_order(2, 10_100_000_000, 200_000_000)));

    let resting = engine.book().all_resting_orders();
    let stops = engine.stop_book().all_stop_orders();

    let snapshot = BookSnapshot {
        symbol: SymbolId(0),
        sequence: engine.book().sequence,
        trade_id_gen: engine.book().trade_id_gen,
        resting_orders: resting.clone(),
        stop_orders: stops,
    };

    mgr.save(&snapshot).unwrap();

    let loaded = mgr.load_latest(SymbolId(0)).unwrap().unwrap();
    assert_eq!(loaded.resting_orders.len(), 2);
    assert_eq!(loaded.sequence, snapshot.sequence);
    assert_eq!(loaded.trade_id_gen, snapshot.trade_id_gen);

    // Verify order IDs match
    let loaded_ids: std::collections::HashSet<_> = loaded.resting_orders.iter().map(|o| o.id).collect();
    assert!(loaded_ids.contains(&1));
    assert!(loaded_ids.contains(&2));

    let _ = fs::remove_dir_all(&dir);
}

// ─── K81: Snapshot + WAL replay ──────────────────────────────────────────────

#[test]
fn test_k81_snapshot_plus_wal() {
    let dir = temp_dir("k81");
    let snap_dir = dir.join("snapshots");
    let wal_path = dir.join("test.wal");

    let mgr = SnapshotManager::new(&snap_dir).unwrap();

    // Phase 1: build book, take snapshot
    let mut engine = Engine::new(config());
    engine.process(Command::PlaceOrder(limit_buy_order(1, 10_000_000_000, 100_000_000)));
    engine.process(Command::PlaceOrder(limit_sell_order(2, 10_100_000_000, 200_000_000)));

    let snapshot = BookSnapshot {
        symbol: SymbolId(0),
        sequence: engine.book().sequence,
        trade_id_gen: engine.book().trade_id_gen,
        resting_orders: engine.book().all_resting_orders(),
        stop_orders: engine.stop_book().all_stop_orders(),
    };
    mgr.save(&snapshot).unwrap();

    // Phase 2: more commands after snapshot (written to WAL)
    let post_snap_commands = vec![
        Command::PlaceOrder(limit_buy_order(3, 9_900_000_000, 150_000_000)),
        Command::CancelOrder { order_id: 1, symbol: SymbolId(0), timestamp: 10000 },
    ];

    {
        let mut w = WalWriter::open(&wal_path, 1).unwrap();
        w.set_sequence(snapshot.sequence);
        for cmd in &post_snap_commands {
            w.append(&to_wal_cmd(cmd)).unwrap();
        }
        w.flush().unwrap();
    }

    // Continue processing on the original engine
    for cmd in &post_snap_commands {
        engine.process(cmd.clone());
    }

    // Phase 3: Recover from snapshot + WAL
    let loaded_snap = mgr.load_latest(SymbolId(0)).unwrap().unwrap();
    let mut recovered = Engine::new(config());
    // Re-insert resting orders from snapshot
    for order in &loaded_snap.resting_orders {
        recovered.process(Command::PlaceOrder(*order));
    }

    // Replay WAL entries after snapshot
    let mut reader = WalReader::open(&wal_path).unwrap();
    let entries = reader.read_all().unwrap();
    for entry in &entries {
        match &entry.command {
            WalCommand::PlaceOrder(o) => { recovered.process(Command::PlaceOrder(*o)); }
            WalCommand::CancelOrder { order_id, symbol, timestamp } => {
                recovered.process(Command::CancelOrder { order_id: *order_id, symbol: *symbol, timestamp: *timestamp });
            }
            _ => {}
        }
    }

    // Verify: same orders on book
    assert!(books_match(&engine, &recovered));
    let _ = fs::remove_dir_all(&dir);
}

// ─── K82: Corrupted WAL entry ────────────────────────────────────────────────

#[test]
fn test_k82_corrupted_wal() {
    let dir = temp_dir("k82");
    let wal_path = dir.join("test.wal");

    // Write valid entry then corrupt data
    {
        let mut w = WalWriter::open(&wal_path, 1).unwrap();
        w.append(&WalCommand::PlaceOrder(limit_buy_order(1, 10_000_000_000, 100_000_000))).unwrap();
        w.flush().unwrap();
    }

    // Append garbage bytes
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&wal_path).unwrap();
        f.write_all(&[0xFF; 20]).unwrap();
    }

    // Reading should get first entry but fail/stop on corruption
    let mut reader = WalReader::open(&wal_path).unwrap();
    let first = reader.next_entry().unwrap();
    assert!(first.is_some());

    // Second read should fail (corrupted)
    let second = reader.next_entry();
    assert!(second.is_err() || second.unwrap().is_none());

    let _ = fs::remove_dir_all(&dir);
}

// ─── K83: Empty snapshot ─────────────────────────────────────────────────────

#[test]
fn test_k83_empty_snapshot() {
    let dir = temp_dir("k83");
    let mgr = SnapshotManager::new(&dir).unwrap();

    let snapshot = BookSnapshot {
        symbol: SymbolId(0),
        sequence: 0,
        trade_id_gen: 0,
        resting_orders: vec![],
        stop_orders: vec![],
    };

    mgr.save(&snapshot).unwrap();
    let loaded = mgr.load_latest(SymbolId(0)).unwrap().unwrap();
    assert!(loaded.resting_orders.is_empty());
    assert!(loaded.stop_orders.is_empty());
    assert_eq!(loaded.sequence, 0);

    let _ = fs::remove_dir_all(&dir);
}

// ─── K84: Snapshot with stop orders ──────────────────────────────────────────

#[test]
fn test_k84_snapshot_with_stops() {
    let dir = temp_dir("k84");
    let mgr = SnapshotManager::new(&dir).unwrap();

    let stop = Order::new_stop_limit(
        10, SymbolId(0), Side::Buy, TimeInForce::GTC,
        FixedPrice::from_raw(10_100_000_000),
        FixedPrice::from_raw(10_000_000_000),
        FixedQty::from_raw(100_000_000),
        1000,
    );

    let snapshot = BookSnapshot {
        symbol: SymbolId(0),
        sequence: 5,
        trade_id_gen: 3,
        resting_orders: vec![limit_buy_order(1, 9_900_000_000, 100_000_000)],
        stop_orders: vec![stop],
    };

    mgr.save(&snapshot).unwrap();
    let loaded = mgr.load_latest(SymbolId(0)).unwrap().unwrap();
    assert_eq!(loaded.resting_orders.len(), 1);
    assert_eq!(loaded.stop_orders.len(), 1);
    assert_eq!(loaded.stop_orders[0].id, 10);

    let _ = fs::remove_dir_all(&dir);
}

// ─── Recovery preserves partial fill state ───────────────────────────────────

#[test]
fn test_recovery_partial_fills() {
    let dir = temp_dir("recovery_partial");
    let wal_path = dir.join("test.wal");

    // Build book: ask at 100 with qty 10, buy 3 (partial fill)
    let commands = vec![
        Command::PlaceOrder(limit_sell_order(1, 10_000_000_000, 1_000_000_000)),  // 10.0
        Command::PlaceOrder(Order::new_market(
            2, SymbolId(0), Side::Buy, TimeInForce::GTC,
            FixedQty::from_raw(300_000_000), 2000, // 3.0
        )),
    ];

    let mut engine = Engine::new(config());
    {
        let mut w = WalWriter::open(&wal_path, 1).unwrap();
        for cmd in &commands {
            engine.process(cmd.clone());
            w.append(&to_wal_cmd(cmd)).unwrap();
        }
        w.flush().unwrap();
    }

    // Original: order 1 should have remaining = 7.0
    let order = engine.book().get_order(1).unwrap();
    assert_eq!(order.remaining_qty, FixedQty::from_raw(700_000_000));

    // Replay from WAL
    let mut reader = WalReader::open(&wal_path).unwrap();
    let entries = reader.read_all().unwrap();
    let mut recovered = Engine::new(config());
    for entry in entries {
        match entry.command {
            WalCommand::PlaceOrder(o) => { recovered.process(Command::PlaceOrder(o)); }
            WalCommand::CancelOrder { order_id, symbol, timestamp } => {
                recovered.process(Command::CancelOrder { order_id, symbol, timestamp });
            }
            _ => {}
        }
    }

    // Recovered: same partial fill state
    let recovered_order = recovered.book().get_order(1).unwrap();
    assert_eq!(recovered_order.remaining_qty, FixedQty::from_raw(700_000_000));

    let _ = fs::remove_dir_all(&dir);
}

// ─── WAL determinism: same commands twice → same events ──────────────────────

#[test]
fn test_wal_determinism() {
    let commands = vec![
        Command::PlaceOrder(limit_sell_order(1, 10_000_000_000, 100_000_000)),
        Command::PlaceOrder(limit_buy_order(2, 10_000_000_000, 50_000_000)),
        Command::PlaceOrder(limit_buy_order(3, 9_900_000_000, 200_000_000)),
        Command::CancelOrder { order_id: 3, symbol: SymbolId(0), timestamp: 5000 },
    ];

    // Run 1
    let mut e1 = Engine::new(config());
    let mut events1 = Vec::new();
    for cmd in &commands {
        events1.extend(e1.process(cmd.clone()).to_vec());
    }

    // Run 2
    let mut e2 = Engine::new(config());
    let mut events2 = Vec::new();
    for cmd in &commands {
        events2.extend(e2.process(cmd.clone()).to_vec());
    }

    // Same number of events
    assert_eq!(events1.len(), events2.len());
    // Same book state
    assert!(books_match(&e1, &e2));
}

// ─── No snapshot returns None ────────────────────────────────────────────────

#[test]
fn test_no_snapshot_returns_none() {
    let dir = temp_dir("no_snap");
    let mgr = SnapshotManager::new(&dir).unwrap();
    let result = mgr.load_latest(SymbolId(0)).unwrap();
    assert!(result.is_none());
    let _ = fs::remove_dir_all(&dir);
}
