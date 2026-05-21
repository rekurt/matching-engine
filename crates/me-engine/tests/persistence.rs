//! Comprehensive tests for snapshot/WAL recovery of the matching engine.
//!
//! Validates that the engine can be fully reconstructed from WAL replay,
//! snapshots, or a combination of both, with identical state to the original.

use std::fs;
use std::path::PathBuf;

use me_core::events::MatchEvent;
use me_core::order::{Order, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::{FixedPrice, FixedQty, OrderId, Side, SymbolId};

use me_engine::command::Command;
use me_engine::engine::Engine;

use me_persist::snapshot::{BookSnapshot, SnapshotManager};
use me_persist::wal::{WalCommand, WalEntry, WalReader, WalWriter};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Generate a unique temporary directory for a test.
fn temp_dir(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("me_persistence_tests")
        .join(test_name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn config() -> SymbolConfig {
    SymbolConfig::default_btc_usdt()
}

/// Convert a WalCommand into an engine Command.
fn wal_to_command(wal: &WalCommand) -> Command {
    match wal {
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
    }
}

/// Convert an engine Command into a WalCommand.
fn command_to_wal(cmd: &Command) -> WalCommand {
    match cmd {
        Command::PlaceOrder(order) => WalCommand::PlaceOrder(*order),
        Command::CancelOrder {
            order_id,
            symbol,
            timestamp,
        } => WalCommand::CancelOrder {
            order_id: *order_id,
            symbol: *symbol,
            timestamp: *timestamp,
        },
        Command::AmendOrder {
            order_id,
            symbol,
            new_price,
            new_qty,
            timestamp,
        } => WalCommand::AmendOrder {
            order_id: *order_id,
            symbol: *symbol,
            new_price: *new_price,
            new_qty: *new_qty,
            timestamp: *timestamp,
        },
    }
}

/// Take a snapshot of the current engine state.
fn snapshot_engine(engine: &Engine) -> BookSnapshot {
    BookSnapshot {
        symbol: engine.config().id,
        sequence: engine.book().sequence,
        trade_id_gen: engine.book().trade_id_gen,
        wal_sequence: 0,
        resting_orders: engine.book().all_resting_orders(),
        stop_orders: engine.stop_book().all_stop_orders(),
    }
}

/// Rebuild an engine from a snapshot and a series of WAL entries.
///
/// Creates a fresh engine, re-inserts all resting and stop orders from the
/// snapshot (directly into the book to avoid re-matching), then replays any
/// WAL entries whose sequence number is greater than the snapshot's sequence.
fn rebuild_engine_from_snapshot(snapshot: BookSnapshot, wal_entries: Vec<WalEntry>) -> Engine {
    let mut engine = Engine::new(config());

    // Re-insert resting orders directly into the book (bypassing matching).
    for order in &snapshot.resting_orders {
        engine.book_mut().insert_order(*order);
    }

    // Re-insert stop orders.
    for order in &snapshot.stop_orders {
        engine
            .stop_book_mut()
            .insert(*order, &mut me_core::events::EventBuffer::default());
    }

    // Restore sequence and trade_id_gen counters.
    engine.book_mut().sequence = snapshot.sequence;
    engine.book_mut().trade_id_gen = snapshot.trade_id_gen;

    // Replay WAL entries that come after the snapshot.
    for entry in &wal_entries {
        if entry.sequence > snapshot.sequence {
            engine.process(wal_to_command(&entry.command));
        }
    }

    engine
}

/// Rebuild an engine purely from WAL entries (no snapshot).
fn rebuild_engine_from_wal(wal_entries: Vec<WalEntry>) -> Engine {
    let mut engine = Engine::new(config());
    for entry in &wal_entries {
        engine.process(wal_to_command(&entry.command));
    }
    engine
}

/// Create a limit buy order.
fn limit_buy(id: OrderId, price: f64, qty: f64, ts: u64) -> Order {
    Order::new_limit(
        id,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(price),
        FixedQty::from_f64(qty),
        ts,
    )
}

/// Create a limit sell order.
fn limit_sell(id: OrderId, price: f64, qty: f64, ts: u64) -> Order {
    Order::new_limit(
        id,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(price),
        FixedQty::from_f64(qty),
        ts,
    )
}

/// Create a stop-limit buy order.
fn stop_limit_buy(id: OrderId, stop_price: f64, limit_price: f64, qty: f64, ts: u64) -> Order {
    Order::new_stop_limit(
        id,
        SymbolId(0),
        Side::Buy,
        TimeInForce::GTC,
        FixedPrice::from_f64(limit_price),
        FixedPrice::from_f64(stop_price),
        FixedQty::from_f64(qty),
        ts,
    )
}

/// Create a stop-limit sell order.
fn stop_limit_sell(id: OrderId, stop_price: f64, limit_price: f64, qty: f64, ts: u64) -> Order {
    Order::new_stop_limit(
        id,
        SymbolId(0),
        Side::Sell,
        TimeInForce::GTC,
        FixedPrice::from_f64(limit_price),
        FixedPrice::from_f64(stop_price),
        FixedQty::from_f64(qty),
        ts,
    )
}

/// Assert two engines have identical book state (resting orders, stop orders,
/// best bid/ask).
fn assert_engines_equal(a: &Engine, b: &Engine) {
    // Compare resting orders (sorted by id for stable comparison).
    let mut a_orders = a.book().all_resting_orders();
    let mut b_orders = b.book().all_resting_orders();
    a_orders.sort_by_key(|o| o.id);
    b_orders.sort_by_key(|o| o.id);

    assert_eq!(
        a_orders.len(),
        b_orders.len(),
        "Resting order count mismatch: {} vs {}",
        a_orders.len(),
        b_orders.len()
    );

    for (ao, bo) in a_orders.iter().zip(b_orders.iter()) {
        assert_eq!(ao.id, bo.id, "Order ID mismatch");
        assert_eq!(ao.price, bo.price, "Price mismatch for order {}", ao.id);
        assert_eq!(ao.side, bo.side, "Side mismatch for order {}", ao.id);
        assert_eq!(
            ao.remaining_qty, bo.remaining_qty,
            "Remaining qty mismatch for order {}",
            ao.id
        );
        assert_eq!(
            ao.cumulative_qty, bo.cumulative_qty,
            "Cumulative qty mismatch for order {}",
            ao.id
        );
    }

    // Compare stop orders.
    let mut a_stops = a.stop_book().all_stop_orders();
    let mut b_stops = b.stop_book().all_stop_orders();
    a_stops.sort_by_key(|o| o.id);
    b_stops.sort_by_key(|o| o.id);

    assert_eq!(
        a_stops.len(),
        b_stops.len(),
        "Stop order count mismatch: {} vs {}",
        a_stops.len(),
        b_stops.len()
    );

    for (ao, bo) in a_stops.iter().zip(b_stops.iter()) {
        assert_eq!(ao.id, bo.id, "Stop order ID mismatch");
        assert_eq!(
            ao.stop_price, bo.stop_price,
            "Stop price mismatch for order {}",
            ao.id
        );
    }

    // Compare best bid/ask.
    assert_eq!(
        a.book().best_bid(),
        b.book().best_bid(),
        "Best bid mismatch"
    );
    assert_eq!(
        a.book().best_ask(),
        b.book().best_ask(),
        "Best ask mismatch"
    );
}

/// Write a series of commands to a WAL file and also process them on an engine.
/// Returns the engine after processing all commands.
fn write_wal_and_process(
    wal_path: &std::path::Path,
    commands: &[Command],
    start_seq: u64,
) -> Engine {
    let mut engine = Engine::new(config());
    let mut writer = WalWriter::open(wal_path, 1).unwrap();
    writer.set_sequence(start_seq);
    for cmd in commands {
        engine.process(cmd.clone());
        writer.append(&command_to_wal(cmd)).unwrap();
    }
    writer.flush().unwrap();
    engine
}

// ─── Test 1: WAL replay empty log -> empty book ─────────────────────────────

#[test]
fn test_wal_replay_empty_log() {
    let dir = temp_dir("wal_replay_empty");
    let wal_path = dir.join("empty.wal");

    {
        let mut writer = WalWriter::open(&wal_path, 1).unwrap();
        writer.flush().unwrap();
    }

    let mut reader = WalReader::open(&wal_path).unwrap();
    let entries = reader.read_all().unwrap();
    assert!(entries.is_empty());

    let engine = rebuild_engine_from_wal(entries);
    assert_eq!(engine.book().order_count(), 0);
    assert!(engine.book().best_bid().is_none());
    assert!(engine.book().best_ask().is_none());
    assert!(engine.stop_book().is_empty());

    let _ = fs::remove_dir_all(&dir);
}

// ─── Test 2: WAL replay N place commands -> identical state ─────────────────

#[test]
fn test_wal_replay_place_commands() {
    let dir = temp_dir("wal_replay_place");
    let wal_path = dir.join("orders.wal");

    let commands: Vec<Command> = vec![
        Command::PlaceOrder(limit_buy(1, 100.0, 1.0, 1000)),
        Command::PlaceOrder(limit_buy(2, 99.0, 2.0, 2000)),
        Command::PlaceOrder(limit_sell(3, 105.0, 1.5, 3000)),
        Command::PlaceOrder(limit_sell(4, 106.0, 3.0, 4000)),
        Command::PlaceOrder(limit_buy(5, 98.0, 0.5, 5000)),
    ];

    // Build reference engine and write WAL.
    let reference = write_wal_and_process(&wal_path, &commands, 0);

    // Replay from WAL.
    let mut reader = WalReader::open(&wal_path).unwrap();
    let entries = reader.read_all().unwrap();
    assert_eq!(entries.len(), 5);

    let recovered = rebuild_engine_from_wal(entries);
    assert_engines_equal(&reference, &recovered);

    let _ = fs::remove_dir_all(&dir);
}

// ─── Test 3: WAL replay with cancel -> state matches ────────────────────────

#[test]
fn test_wal_replay_with_cancel() {
    let dir = temp_dir("wal_replay_cancel");
    let wal_path = dir.join("cancel.wal");

    let commands: Vec<Command> = vec![
        Command::PlaceOrder(limit_buy(1, 100.0, 1.0, 1000)),
        Command::PlaceOrder(limit_buy(2, 99.0, 2.0, 2000)),
        Command::PlaceOrder(limit_sell(3, 105.0, 1.5, 3000)),
        Command::CancelOrder {
            order_id: 1,
            symbol: SymbolId(0),
            timestamp: 4000,
        },
    ];

    let reference = write_wal_and_process(&wal_path, &commands, 0);

    let mut reader = WalReader::open(&wal_path).unwrap();
    let entries = reader.read_all().unwrap();
    let recovered = rebuild_engine_from_wal(entries);

    assert_engines_equal(&reference, &recovered);
    assert!(!recovered.book().contains_order(1));
    assert!(recovered.book().contains_order(2));
    assert!(recovered.book().contains_order(3));

    let _ = fs::remove_dir_all(&dir);
}

// ─── Test 4: WAL replay with amend -> state matches ─────────────────────────

#[test]
fn test_wal_replay_with_amend() {
    let dir = temp_dir("wal_replay_amend");
    let wal_path = dir.join("amend.wal");

    let commands: Vec<Command> = vec![
        Command::PlaceOrder(limit_buy(1, 100.0, 2.0, 1000)),
        Command::PlaceOrder(limit_sell(2, 110.0, 3.0, 2000)),
        Command::AmendOrder {
            order_id: 1,
            symbol: SymbolId(0),
            new_price: Some(FixedPrice::from_f64(101.0)),
            new_qty: Some(FixedQty::from_f64(1.5)),
            timestamp: 3000,
        },
        Command::AmendOrder {
            order_id: 2,
            symbol: SymbolId(0),
            new_price: None,
            new_qty: Some(FixedQty::from_f64(2.0)),
            timestamp: 4000,
        },
    ];

    let reference = write_wal_and_process(&wal_path, &commands, 0);

    let mut reader = WalReader::open(&wal_path).unwrap();
    let entries = reader.read_all().unwrap();
    let recovered = rebuild_engine_from_wal(entries);

    assert_engines_equal(&reference, &recovered);

    let _ = fs::remove_dir_all(&dir);
}

// ─── Test 5: WAL replay determinism ─────────────────────────────────────────

#[test]
fn test_wal_replay_determinism() {
    let dir = temp_dir("wal_determinism");
    let wal_path = dir.join("determinism.wal");

    let commands: Vec<Command> = vec![
        Command::PlaceOrder(limit_buy(1, 100.0, 1.0, 1000)),
        Command::PlaceOrder(limit_sell(2, 100.0, 0.5, 2000)), // partial fill
        Command::PlaceOrder(limit_buy(3, 99.0, 2.0, 3000)),
        Command::CancelOrder {
            order_id: 3,
            symbol: SymbolId(0),
            timestamp: 4000,
        },
        Command::PlaceOrder(limit_sell(4, 105.0, 1.0, 5000)),
    ];

    {
        let mut writer = WalWriter::open(&wal_path, 1).unwrap();
        for cmd in &commands {
            writer.append(&command_to_wal(cmd)).unwrap();
        }
        writer.flush().unwrap();
    }

    // Replay the same WAL twice.
    let mut reader1 = WalReader::open(&wal_path).unwrap();
    let engine1 = rebuild_engine_from_wal(reader1.read_all().unwrap());

    let mut reader2 = WalReader::open(&wal_path).unwrap();
    let engine2 = rebuild_engine_from_wal(reader2.read_all().unwrap());

    assert_engines_equal(&engine1, &engine2);

    let _ = fs::remove_dir_all(&dir);
}

// ─── Test 6: Snapshot save and restore -> identical book state ───────────────

#[test]
fn test_snapshot_save_and_restore() {
    let dir = temp_dir("snapshot_save_restore");

    let mut engine = Engine::new(config());
    engine.process(Command::PlaceOrder(limit_buy(1, 100.0, 1.0, 1000)));
    engine.process(Command::PlaceOrder(limit_buy(2, 99.0, 2.0, 2000)));
    engine.process(Command::PlaceOrder(limit_sell(3, 105.0, 1.5, 3000)));
    engine.process(Command::PlaceOrder(limit_sell(4, 106.0, 3.0, 4000)));

    let snap = snapshot_engine(&engine);
    let mgr = SnapshotManager::new(&dir).unwrap();
    mgr.save(&snap).unwrap();

    let loaded = mgr.load_latest(SymbolId(0)).unwrap().unwrap();
    assert_eq!(loaded.sequence, snap.sequence);
    assert_eq!(loaded.trade_id_gen, snap.trade_id_gen);
    assert_eq!(loaded.resting_orders.len(), snap.resting_orders.len());

    let recovered = rebuild_engine_from_snapshot(loaded, vec![]);
    assert_engines_equal(&engine, &recovered);

    let _ = fs::remove_dir_all(&dir);
}

// ─── Test 7: Snapshot + WAL replay ──────────────────────────────────────────

#[test]
fn test_snapshot_plus_wal_replay() {
    let dir = temp_dir("snapshot_plus_wal");
    let wal_path = dir.join("after_snap.wal");

    // Phase 1: build initial state and snapshot.
    let mut engine = Engine::new(config());
    let phase1_cmds: Vec<Command> = vec![
        Command::PlaceOrder(limit_buy(1, 100.0, 1.0, 1000)),
        Command::PlaceOrder(limit_buy(2, 99.0, 2.0, 2000)),
        Command::PlaceOrder(limit_sell(3, 110.0, 1.0, 3000)),
    ];
    for cmd in &phase1_cmds {
        engine.process(cmd.clone());
    }

    let snap = snapshot_engine(&engine);
    let snap_seq = snap.sequence;
    let mgr = SnapshotManager::new(&dir).unwrap();
    mgr.save(&snap).unwrap();

    // Phase 2: more commands after the snapshot (written to WAL).
    let phase2_cmds: Vec<Command> = vec![
        Command::PlaceOrder(limit_sell(4, 108.0, 0.5, 4000)),
        Command::PlaceOrder(limit_buy(5, 97.0, 1.0, 5000)),
    ];
    {
        let mut writer = WalWriter::open(&wal_path, 1).unwrap();
        writer.set_sequence(snap_seq);
        for cmd in &phase2_cmds {
            engine.process(cmd.clone());
            writer.append(&command_to_wal(cmd)).unwrap();
        }
        writer.flush().unwrap();
    }

    // Recovery: load snapshot + replay WAL.
    let loaded_snap = mgr.load_latest(SymbolId(0)).unwrap().unwrap();
    let mut reader = WalReader::open(&wal_path).unwrap();
    let wal_entries = reader.read_all().unwrap();

    let recovered = rebuild_engine_from_snapshot(loaded_snap, wal_entries);
    assert_engines_equal(&engine, &recovered);

    let _ = fs::remove_dir_all(&dir);
}

// ─── Test 8: Full recovery cycle ────────────────────────────────────────────

#[test]
fn test_full_recovery_cycle() {
    let dir = temp_dir("full_recovery");
    let snap_dir = dir.join("snapshots");
    let wal_path = dir.join("commands.wal");

    // Phase 1: build up the book with several orders, including a trade.
    let mut engine = Engine::new(config());

    let phase1_cmds: Vec<Command> = vec![
        Command::PlaceOrder(limit_buy(1, 100.0, 5.0, 1000)),
        Command::PlaceOrder(limit_buy(2, 99.0, 3.0, 2000)),
        Command::PlaceOrder(limit_sell(3, 105.0, 2.0, 3000)),
        // This sell matches buy#1 partially.
        Command::PlaceOrder(limit_sell(4, 100.0, 2.0, 4000)),
    ];

    for cmd in &phase1_cmds {
        engine.process(cmd.clone());
    }

    // Snapshot after phase 1.
    let snap = snapshot_engine(&engine);
    let snap_seq = snap.sequence;
    let mgr = SnapshotManager::new(&snap_dir).unwrap();
    mgr.save(&snap).unwrap();

    // Phase 2: more commands written to WAL.
    let phase2_cmds: Vec<Command> = vec![
        Command::PlaceOrder(limit_sell(5, 99.0, 1.0, 5000)), // matches buy#2
        Command::CancelOrder {
            order_id: 3,
            symbol: SymbolId(0),
            timestamp: 6000,
        },
        Command::PlaceOrder(limit_buy(6, 98.0, 4.0, 7000)),
        Command::AmendOrder {
            order_id: 6,
            symbol: SymbolId(0),
            new_price: Some(FixedPrice::from_f64(97.0)),
            new_qty: None,
            timestamp: 8000,
        },
    ];

    {
        let mut writer = WalWriter::open(&wal_path, 1).unwrap();
        writer.set_sequence(snap_seq);
        for cmd in &phase2_cmds {
            engine.process(cmd.clone());
            writer.append(&command_to_wal(cmd)).unwrap();
        }
        writer.flush().unwrap();
    }

    // Full recovery: snapshot + WAL.
    let loaded = mgr.load_latest(SymbolId(0)).unwrap().unwrap();
    let mut reader = WalReader::open(&wal_path).unwrap();
    let entries = reader.read_all().unwrap();

    let recovered = rebuild_engine_from_snapshot(loaded, entries);
    assert_engines_equal(&engine, &recovered);

    let _ = fs::remove_dir_all(&dir);
}

// ─── Test 9: Corrupted WAL entry -> error, no crash ─────────────────────────

#[test]
fn test_corrupted_wal_entry() {
    let dir = temp_dir("corrupted_wal");
    let wal_path = dir.join("corrupt.wal");

    // Write two valid entries.
    {
        let mut writer = WalWriter::open(&wal_path, 1).unwrap();
        writer
            .append(&WalCommand::PlaceOrder(limit_buy(1, 100.0, 1.0, 1000)))
            .unwrap();
        writer
            .append(&WalCommand::PlaceOrder(limit_sell(2, 105.0, 2.0, 2000)))
            .unwrap();
        writer.flush().unwrap();
    }

    // Corrupt bytes in the second entry's payload area.
    {
        let mut data = fs::read(&wal_path).unwrap();
        assert!(data.len() > 30, "WAL file too short to corrupt");
        let mid = data.len() / 2 + 5;
        data[mid] ^= 0xFF;
        data[mid + 1] ^= 0xFF;
        data[mid + 2] ^= 0xFF;
        fs::write(&wal_path, &data).unwrap();
    }

    // Reading should produce an error (CRC mismatch), not a panic.
    let mut reader = WalReader::open(&wal_path).unwrap();

    // First entry might still be valid.
    let first = reader.next_entry();
    // We don't assert first is Ok because corruption might hit the first entry
    // depending on size. The key assertion is: no panic.

    // Attempt to read all remaining; at least one must fail.
    let result = reader.read_all();
    // Either read_all itself returned Err, or first already failed.
    let had_error = first.is_err() || result.is_err();
    assert!(had_error, "Expected at least one error from corrupted WAL");

    let _ = fs::remove_dir_all(&dir);
}

// ─── Test 10: WAL compaction ────────────────────────────────────────────────

#[test]
fn test_wal_compaction() {
    let dir = temp_dir("wal_compaction");
    let wal_path = dir.join("full.wal");
    let snap_dir = dir.join("snapshots");

    // Write 10 commands to WAL.
    let mut engine = Engine::new(config());
    let all_cmds: Vec<Command> = (1..=10)
        .map(|i| Command::PlaceOrder(limit_buy(i, 100.0 - i as f64, 1.0, i * 1000)))
        .collect();

    {
        let mut writer = WalWriter::open(&wal_path, 1).unwrap();
        for cmd in &all_cmds {
            engine.process(cmd.clone());
            writer.append(&command_to_wal(cmd)).unwrap();
        }
        writer.flush().unwrap();
    }

    // Take snapshot at current state (after all 10 commands).
    let snap = snapshot_engine(&engine);
    let mgr = SnapshotManager::new(&snap_dir).unwrap();
    mgr.save(&snap).unwrap();

    // "Compaction": filter WAL entries to only those after snapshot sequence.
    let mut reader = WalReader::open(&wal_path).unwrap();
    let all_entries = reader.read_all().unwrap();
    assert_eq!(all_entries.len(), 10);

    let entries_after_snap: Vec<_> = all_entries
        .into_iter()
        .filter(|e| e.sequence > snap.sequence)
        .collect();
    // All entries are at or before the snapshot, so none should remain.
    assert!(
        entries_after_snap.is_empty(),
        "Expected no entries after snapshot, got {}",
        entries_after_snap.len()
    );

    // Recovery from snapshot alone should produce correct state.
    let loaded = mgr.load_latest(SymbolId(0)).unwrap().unwrap();
    let recovered = rebuild_engine_from_snapshot(loaded, vec![]);
    assert_engines_equal(&engine, &recovered);

    let _ = fs::remove_dir_all(&dir);
}

// ─── Test 11: Empty snapshot works correctly ────────────────────────────────

#[test]
fn test_empty_snapshot() {
    let dir = temp_dir("empty_snapshot");

    let engine = Engine::new(config());
    let snap = snapshot_engine(&engine);

    let mgr = SnapshotManager::new(&dir).unwrap();
    mgr.save(&snap).unwrap();

    let loaded = mgr.load_latest(SymbolId(0)).unwrap().unwrap();
    assert!(loaded.resting_orders.is_empty());
    assert!(loaded.stop_orders.is_empty());
    assert_eq!(loaded.sequence, 0);
    assert_eq!(loaded.trade_id_gen, 0);

    let recovered = rebuild_engine_from_snapshot(loaded, vec![]);
    assert_engines_equal(&engine, &recovered);

    let _ = fs::remove_dir_all(&dir);
}

// ─── Test 12: Snapshot with stop orders -> stop orders preserved ────────────

#[test]
fn test_snapshot_with_stop_orders() {
    let dir = temp_dir("snapshot_stop_orders");

    let mut engine = Engine::new(config());

    // Place regular orders.
    engine.process(Command::PlaceOrder(limit_buy(1, 100.0, 1.0, 1000)));
    engine.process(Command::PlaceOrder(limit_sell(2, 110.0, 1.0, 2000)));

    // Place stop orders (routed to the stop book).
    engine.process(Command::PlaceOrder(stop_limit_buy(
        3, 115.0, 116.0, 1.0, 3000,
    )));
    engine.process(Command::PlaceOrder(stop_limit_sell(
        4, 95.0, 94.0, 2.0, 4000,
    )));

    assert_eq!(engine.book().order_count(), 2);
    assert_eq!(engine.stop_book().len(), 2);

    let snap = snapshot_engine(&engine);
    assert_eq!(snap.resting_orders.len(), 2);
    assert_eq!(snap.stop_orders.len(), 2);

    let mgr = SnapshotManager::new(&dir).unwrap();
    mgr.save(&snap).unwrap();

    let loaded = mgr.load_latest(SymbolId(0)).unwrap().unwrap();
    assert_eq!(loaded.stop_orders.len(), 2);

    // Verify stop order fields are preserved.
    let mut loaded_stops = loaded.stop_orders.clone();
    loaded_stops.sort_by_key(|o| o.id);
    assert_eq!(loaded_stops[0].id, 3);
    assert_eq!(loaded_stops[0].stop_price, FixedPrice::from_f64(115.0));
    assert_eq!(loaded_stops[1].id, 4);
    assert_eq!(loaded_stops[1].stop_price, FixedPrice::from_f64(95.0));

    let recovered = rebuild_engine_from_snapshot(loaded, vec![]);
    assert_engines_equal(&engine, &recovered);

    let _ = fs::remove_dir_all(&dir);
}

// ─── Test 13: Recovery after partial fills -> remaining quantities correct ──

#[test]
fn test_recovery_after_partial_fills() {
    let dir = temp_dir("partial_fills");
    let wal_path = dir.join("partial.wal");

    let commands: Vec<Command> = vec![
        // Buy 5.0 @ 100
        Command::PlaceOrder(limit_buy(1, 100.0, 5.0, 1000)),
        // Sell 2.0 @ 100 -> fills 2.0 from buy#1, leaving 3.0 remaining.
        Command::PlaceOrder(limit_sell(2, 100.0, 2.0, 2000)),
        // Sell 1.0 @ 100 -> fills 1.0 from buy#1, leaving 2.0 remaining.
        Command::PlaceOrder(limit_sell(3, 100.0, 1.0, 3000)),
    ];

    let reference = write_wal_and_process(&wal_path, &commands, 0);

    // Verify reference state.
    let order1 = reference.book().get_order(1).unwrap();
    assert_eq!(order1.remaining_qty, FixedQty::from_f64(2.0));
    assert_eq!(order1.cumulative_qty, FixedQty::from_f64(3.0));

    // Replay from WAL.
    let mut reader = WalReader::open(&wal_path).unwrap();
    let entries = reader.read_all().unwrap();
    let recovered = rebuild_engine_from_wal(entries);

    assert_engines_equal(&reference, &recovered);

    let recovered_order1 = recovered.book().get_order(1).unwrap();
    assert_eq!(recovered_order1.remaining_qty, FixedQty::from_f64(2.0));
    assert_eq!(recovered_order1.cumulative_qty, FixedQty::from_f64(3.0));

    // Also test snapshot recovery of partially filled order.
    let snap = snapshot_engine(&reference);
    let snap_dir = dir.join("snapshots");
    let mgr = SnapshotManager::new(&snap_dir).unwrap();
    mgr.save(&snap).unwrap();

    let loaded = mgr.load_latest(SymbolId(0)).unwrap().unwrap();
    let snap_recovered = rebuild_engine_from_snapshot(loaded, vec![]);

    let snap_order1 = snap_recovered.book().get_order(1).unwrap();
    assert_eq!(snap_order1.remaining_qty, FixedQty::from_f64(2.0));
    assert_eq!(snap_order1.cumulative_qty, FixedQty::from_f64(3.0));

    let _ = fs::remove_dir_all(&dir);
}

// ─── Test 14: Recovery preserves price-time priority (FIFO) ─────────────────

#[test]
fn test_recovery_preserves_price_time_priority() {
    let dir = temp_dir("price_time_priority");
    let wal_path = dir.join("priority.wal");

    // Place multiple bids at the same price; the first should be filled first.
    let commands: Vec<Command> = vec![
        Command::PlaceOrder(limit_buy(1, 100.0, 1.0, 1000)), // first at 100
        Command::PlaceOrder(limit_buy(2, 100.0, 1.0, 2000)), // second at 100
        Command::PlaceOrder(limit_buy(3, 100.0, 1.0, 3000)), // third at 100
    ];

    // Build reference engine and WAL.
    let mut reference = write_wal_and_process(&wal_path, &commands, 0);

    // Replay from WAL.
    let mut reader = WalReader::open(&wal_path).unwrap();
    let entries = reader.read_all().unwrap();
    let mut recovered = rebuild_engine_from_wal(entries);

    // Submit a sell that matches one bid. It should fill order#1 (FIFO).
    let sell_cmd = Command::PlaceOrder(limit_sell(10, 100.0, 1.0, 10000));

    let ref_events: Vec<_> = reference.process(sell_cmd.clone()).to_vec();
    let rec_events: Vec<_> = recovered.process(sell_cmd).to_vec();

    // Both should produce trades against order#1.
    let ref_trade = ref_events.iter().find_map(|e| {
        if let MatchEvent::Trade(t) = e {
            Some(t)
        } else {
            None
        }
    });
    let rec_trade = rec_events.iter().find_map(|e| {
        if let MatchEvent::Trade(t) = e {
            Some(t)
        } else {
            None
        }
    });

    assert!(
        ref_trade.is_some(),
        "Reference engine should produce a trade"
    );
    assert!(
        rec_trade.is_some(),
        "Recovered engine should produce a trade"
    );

    // The maker (resting order) should be order#1 in both cases (FIFO).
    assert_eq!(
        ref_trade.unwrap().maker_order_id,
        rec_trade.unwrap().maker_order_id,
        "FIFO priority not preserved: reference matched order {} but recovered matched order {}",
        ref_trade.unwrap().maker_order_id,
        rec_trade.unwrap().maker_order_id
    );

    // After the fill, orders 2 and 3 should remain.
    assert!(!reference.book().contains_order(1));
    assert!(reference.book().contains_order(2));
    assert!(reference.book().contains_order(3));

    let _ = fs::remove_dir_all(&dir);
}
