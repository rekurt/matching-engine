//! Realistic HFT pipeline benchmarks.
//!
//! Measures every layer of the production hot path individually and combined:
//!   Clock → Risk → Matching → WAL → Async Audit
//!
//! Results show true wire-to-wire latency (excluding network I/O).

use criterion::{criterion_group, criterion_main, Criterion};
use tracing_subscriber::layer::SubscriberExt;

use me_audit::async_logger::{AsyncAuditLogger, AuditFormat};
use me_audit::logger::{match_event_to_entry, order_to_entry, AuditLogger};
use me_clock::{Clock, CoarseClock, SystemClock};
use me_core::events::MatchEvent;
use me_core::order::{Order, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::*;
use me_engine::command::Command;
use me_engine::engine::Engine;
use me_persist::wal::{WalCommand, WalWriter};
use me_risk::config::RiskConfig;
use me_risk::engine::RiskEngine;

const BASE_PRICE: i64 = 10_000_000_000;
const TICK: i64 = 1_000_000;
const ONE_BTC: u64 = 100_000_000;

fn fresh_engine() -> Engine {
    Engine::new(SymbolConfig::default_btc_usdt())
}

fn risk_engine_for_bench() -> RiskEngine {
    let mut risk = RiskEngine::new();
    let mut cfg = RiskConfig::default_for(SymbolId(0));
    cfg.reference_price = FixedPrice::from_raw(BASE_PRICE);
    cfg.max_orders_per_second = 1_000_000;
    cfg.max_open_orders = 1_000_000;
    cfg.price_band_bps = 5000;
    risk.add_symbol(cfg);
    risk
}

fn install_noop_tracing() {
    use tracing_subscriber::fmt;
    let _ = tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(fmt::layer().with_writer(std::io::sink)),
    );
}

fn temp_wal_path(name: &str) -> String {
    let dir = std::env::temp_dir().join("me_bench_wal").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    format!("{}/bench.wal", dir.display())
}

// ═══════════════════════════════════════════════════════════════════════════
// 1. CLOCK COMPARISON — SystemClock vs CoarseClock vs RdtscClock
// ═══════════════════════════════════════════════════════════════════════════

fn bench_clock_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("clock_implementations");

    group.bench_function("system_clock", |b| {
        let clock = SystemClock;
        b.iter(|| clock.now_ns());
    });

    group.bench_function("coarse_clock", |b| {
        let clock = CoarseClock::new();
        b.iter(|| clock.now_ns());
    });

    group.bench_function("rdtsc_clock", |b| {
        let clock = me_clock::RdtscClock::new();
        b.iter(|| clock.now_ns());
    });

    group.finish();
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. RISK CHECK — isolated cost
// ═══════════════════════════════════════════════════════════════════════════

fn bench_risk_check(c: &mut Criterion) {
    let mut group = c.benchmark_group("risk_check");

    group.bench_function("pass_all_checks", |b| {
        b.iter_custom(|iters| {
            let mut risk = risk_engine_for_bench();
            let start = std::time::Instant::now();
            for i in 0..iters {
                let order = Order::new_limit(
                    i + 1, SymbolId(0), Side::Buy, TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE - TICK),
                    FixedQty::from_raw(ONE_BTC), i,
                );
                let _ = risk.check_order(&order);
            }
            start.elapsed()
        });
    });

    group.bench_function("reject_fat_finger", |b| {
        b.iter_custom(|iters| {
            let mut risk = RiskEngine::new();
            let mut cfg = RiskConfig::default_for(SymbolId(0));
            cfg.reference_price = FixedPrice::from_raw(BASE_PRICE);
            cfg.max_order_qty = FixedQty::from_raw(ONE_BTC * 10);
            cfg.max_orders_per_second = 1_000_000;
            risk.add_symbol(cfg);

            let start = std::time::Instant::now();
            for i in 0..iters {
                let order = Order::new_limit(
                    i + 1, SymbolId(0), Side::Buy, TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE),
                    FixedQty::from_raw(ONE_BTC * 100), i,
                );
                let _ = risk.check_order(&order);
            }
            start.elapsed()
        });
    });

    group.finish();
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. AUDIT — sync vs async comparison
// ═══════════════════════════════════════════════════════════════════════════

fn bench_audit_comparison(c: &mut Criterion) {
    install_noop_tracing();
    let mut group = c.benchmark_group("audit_sync_vs_async");

    // Sync audit (old path): serialize JSON on hot thread
    group.bench_function("sync_json_3events", |b| {
        b.iter_custom(|iters| {
            let logger = AuditLogger::new(true);
            let events = vec![
                MatchEvent::OrderAccepted { order_id: 1, timestamp: 1000 },
                MatchEvent::Trade(me_core::trade::Trade {
                    trade_id: 1, symbol: SymbolId(0),
                    price: FixedPrice::from_raw(BASE_PRICE),
                    qty: FixedQty::from_raw(ONE_BTC),
                    maker_order_id: 1, taker_order_id: 2,
                    maker_side: Side::Sell, timestamp: 1000, sequence: 0,
                }),
                MatchEvent::OrderFilled {
                    order_id: 2, filled_qty: FixedQty::from_raw(ONE_BTC),
                    price: FixedPrice::from_raw(BASE_PRICE), timestamp: 1000,
                },
            ];
            let start = std::time::Instant::now();
            for _ in 0..iters {
                logger.log_match_events(&events, SymbolId(0));
            }
            start.elapsed()
        });
    });

    // Async audit (new path): SPSC send only
    group.bench_function("async_spsc_3events", |b| {
        b.iter_custom(|iters| {
            let logger = AsyncAuditLogger::new(65536, AuditFormat::Json);
            let events = vec![
                MatchEvent::OrderAccepted { order_id: 1, timestamp: 1000 },
                MatchEvent::Trade(me_core::trade::Trade {
                    trade_id: 1, symbol: SymbolId(0),
                    price: FixedPrice::from_raw(BASE_PRICE),
                    qty: FixedQty::from_raw(ONE_BTC),
                    maker_order_id: 1, taker_order_id: 2,
                    maker_side: Side::Sell, timestamp: 1000, sequence: 0,
                }),
                MatchEvent::OrderFilled {
                    order_id: 2, filled_qty: FixedQty::from_raw(ONE_BTC),
                    price: FixedPrice::from_raw(BASE_PRICE), timestamp: 1000,
                },
            ];
            let start = std::time::Instant::now();
            for _ in 0..iters {
                for event in &events {
                    if let Some(entry) = match_event_to_entry(event, SymbolId(0)) {
                        logger.log_entry(entry);
                    }
                }
            }
            start.elapsed()
        });
    });

    // Async audit disabled (baseline)
    group.bench_function("disabled", |b| {
        b.iter_custom(|iters| {
            let logger = AuditLogger::new(false);
            let events = vec![MatchEvent::OrderAccepted { order_id: 1, timestamp: 1000 }];
            let start = std::time::Instant::now();
            for _ in 0..iters {
                logger.log_match_events(&events, SymbolId(0));
            }
            start.elapsed()
        });
    });

    group.finish();
}

// ═══════════════════════════════════════════════════════════════════════════
// 4. WAL — write cost
// ═══════════════════════════════════════════════════════════════════════════

fn bench_wal_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_write");

    // WAL write per command (buffered, no fsync)
    group.bench_function("buffered_append", |b| {
        b.iter_custom(|iters| {
            let path = temp_wal_path("bench_buffered");
            let mut wal = WalWriter::open(&path, u64::MAX).unwrap(); // never auto-flush
            let order = Order::new_limit(
                1, SymbolId(0), Side::Buy, TimeInForce::GTC,
                FixedPrice::from_raw(BASE_PRICE), FixedQty::from_raw(ONE_BTC), 0,
            );
            let cmd = WalCommand::PlaceOrder(order);

            let start = std::time::Instant::now();
            for _ in 0..iters {
                let _ = wal.append(&cmd);
            }
            start.elapsed()
        });
    });

    // WAL write with fsync every command
    group.bench_function("fsync_every_command", |b| {
        b.iter_custom(|iters| {
            let path = temp_wal_path("bench_fsync");
            let mut wal = WalWriter::open(&path, 1).unwrap(); // fsync every write
            let order = Order::new_limit(
                1, SymbolId(0), Side::Buy, TimeInForce::GTC,
                FixedPrice::from_raw(BASE_PRICE), FixedQty::from_raw(ONE_BTC), 0,
            );
            let cmd = WalCommand::PlaceOrder(order);

            let start = std::time::Instant::now();
            for _ in 0..iters {
                let _ = wal.append(&cmd);
            }
            start.elapsed()
        });
    });

    // WAL write with batch fsync (every 100 commands)
    group.bench_function("batch_fsync_100", |b| {
        b.iter_custom(|iters| {
            let path = temp_wal_path("bench_batch");
            let mut wal = WalWriter::open(&path, 100).unwrap();
            let order = Order::new_limit(
                1, SymbolId(0), Side::Buy, TimeInForce::GTC,
                FixedPrice::from_raw(BASE_PRICE), FixedQty::from_raw(ONE_BTC), 0,
            );
            let cmd = WalCommand::PlaceOrder(order);

            let start = std::time::Instant::now();
            for _ in 0..iters {
                let _ = wal.append(&cmd);
            }
            start.elapsed()
        });
    });

    group.finish();
}

// ═══════════════════════════════════════════════════════════════════════════
// 5. FULL PIPELINE — layer-by-layer breakdown for limit full fill
// ═══════════════════════════════════════════════════════════════════════════

fn bench_pipeline_breakdown(c: &mut Criterion) {
    install_noop_tracing();
    let mut group = c.benchmark_group("pipeline_breakdown_full_fill");

    // A. Matching ONLY
    group.bench_function("1_matching_only", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            for i in 0..iters {
                let ask = Order::new_limit(
                    i * 2 + 1, SymbolId(0), Side::Sell, TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE), FixedQty::from_raw(ONE_BTC), 0,
                );
                engine.process(Command::PlaceOrder(ask));
            }
            let start = std::time::Instant::now();
            for i in 0..iters {
                let buy = Order::new_limit(
                    i * 2 + 2, SymbolId(0), Side::Buy, TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE), FixedQty::from_raw(ONE_BTC), 0,
                );
                engine.process(Command::PlaceOrder(buy));
            }
            start.elapsed()
        });
    });

    // B. Risk + Matching
    group.bench_function("2_risk_plus_matching", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            let mut risk = risk_engine_for_bench();
            for i in 0..iters {
                let ask = Order::new_limit(
                    i * 2 + 1, SymbolId(0), Side::Sell, TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE), FixedQty::from_raw(ONE_BTC), 0,
                );
                engine.process(Command::PlaceOrder(ask));
            }
            let start = std::time::Instant::now();
            for i in 0..iters {
                let order = Order::new_limit(
                    i * 2 + 2, SymbolId(0), Side::Buy, TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE), FixedQty::from_raw(ONE_BTC), i,
                );
                let _ = risk.check_order(&order);
                engine.process(Command::PlaceOrder(order));
            }
            start.elapsed()
        });
    });

    // C. Risk + Matching + Async Audit
    group.bench_function("3_risk_match_async_audit", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            let mut risk = risk_engine_for_bench();
            let audit = AsyncAuditLogger::new(65536, AuditFormat::Json);
            for i in 0..iters {
                let ask = Order::new_limit(
                    i * 2 + 1, SymbolId(0), Side::Sell, TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE), FixedQty::from_raw(ONE_BTC), 0,
                );
                engine.process(Command::PlaceOrder(ask));
            }
            let start = std::time::Instant::now();
            for i in 0..iters {
                let order = Order::new_limit(
                    i * 2 + 2, SymbolId(0), Side::Buy, TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE), FixedQty::from_raw(ONE_BTC), i,
                );
                audit.log_entry(order_to_entry(&order));
                let _ = risk.check_order(&order);
                let events = engine.process(Command::PlaceOrder(order));
                for event in events {
                    if let Some(entry) = match_event_to_entry(event, SymbolId(0)) {
                        audit.log_entry(entry);
                    }
                }
            }
            start.elapsed()
        });
    });

    // D. Risk + Matching + Async Audit + WAL (buffered)
    group.bench_function("4_full_pipeline_with_wal", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            let mut risk = risk_engine_for_bench();
            let audit = AsyncAuditLogger::new(65536, AuditFormat::Json);
            let path = temp_wal_path("pipeline_wal");
            let mut wal = WalWriter::open(&path, 100).unwrap();

            for i in 0..iters {
                let ask = Order::new_limit(
                    i * 2 + 1, SymbolId(0), Side::Sell, TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE), FixedQty::from_raw(ONE_BTC), 0,
                );
                engine.process(Command::PlaceOrder(ask));
            }
            let start = std::time::Instant::now();
            for i in 0..iters {
                let order = Order::new_limit(
                    i * 2 + 2, SymbolId(0), Side::Buy, TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE), FixedQty::from_raw(ONE_BTC), i,
                );
                // WAL pre-write
                let _ = wal.append(&WalCommand::PlaceOrder(order));
                // Risk
                let _ = risk.check_order(&order);
                // Audit submission
                audit.log_entry(order_to_entry(&order));
                // Match
                let events = engine.process(Command::PlaceOrder(order));
                // Audit events
                for event in events {
                    if let Some(entry) = match_event_to_entry(event, SymbolId(0)) {
                        audit.log_entry(entry);
                    }
                }
            }
            start.elapsed()
        });
    });

    // E. Full pipeline with sync audit (OLD path — for comparison)
    group.bench_function("5_old_sync_audit_pipeline", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            let mut risk = risk_engine_for_bench();
            let logger = AuditLogger::new(true);
            for i in 0..iters {
                let ask = Order::new_limit(
                    i * 2 + 1, SymbolId(0), Side::Sell, TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE), FixedQty::from_raw(ONE_BTC), 0,
                );
                engine.process(Command::PlaceOrder(ask));
            }
            let start = std::time::Instant::now();
            for i in 0..iters {
                let order = Order::new_limit(
                    i * 2 + 2, SymbolId(0), Side::Buy, TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE), FixedQty::from_raw(ONE_BTC), i,
                );
                let _ = risk.check_order(&order);
                logger.log_order_submitted(&order);
                let events = engine.process(Command::PlaceOrder(order));
                logger.log_match_events(events, SymbolId(0));
            }
            start.elapsed()
        });
    });

    group.finish();
}

// ═══════════════════════════════════════════════════════════════════════════
// 6. HFT SCENARIOS — realistic trading patterns with full pipeline
// ═══════════════════════════════════════════════════════════════════════════

fn bench_hft_scenarios(c: &mut Criterion) {
    install_noop_tracing();
    let mut group = c.benchmark_group("hft_scenarios");

    // Market maker: cancel+replace cycle with risk+audit
    group.bench_function("cancel_replace_with_pipeline", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            let mut risk = risk_engine_for_bench();
            let audit = AsyncAuditLogger::new(65536, AuditFormat::Json);

            // Populate asks
            for i in 0..100u64 {
                let ask = Order::new_limit(
                    i + 1, SymbolId(0), Side::Sell, TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE + (i as i64 + 1) * TICK),
                    FixedQty::from_raw(ONE_BTC), 0,
                );
                engine.process(Command::PlaceOrder(ask));
            }

            // Place initial bid
            let mut bid_id = 10_000u64;
            let initial = Order::new_limit(
                bid_id, SymbolId(0), Side::Buy, TimeInForce::GTC,
                FixedPrice::from_raw(BASE_PRICE - TICK),
                FixedQty::from_raw(ONE_BTC), 0,
            );
            engine.process(Command::PlaceOrder(initial));

            let start = std::time::Instant::now();
            for i in 0..iters {
                // Cancel
                let cancel_events = engine.process(Command::CancelOrder {
                    order_id: bid_id, symbol: SymbolId(0), timestamp: i,
                });
                for event in cancel_events {
                    if let Some(entry) = match_event_to_entry(event, SymbolId(0)) {
                        audit.log_entry(entry);
                    }
                }

                // Place new bid
                bid_id += 1;
                let order = Order::new_limit(
                    bid_id, SymbolId(0), Side::Buy, TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE - ((i % 5) as i64 + 1) * TICK),
                    FixedQty::from_raw(ONE_BTC), i,
                );
                let _ = risk.check_order(&order);
                audit.log_entry(order_to_entry(&order));
                let place_events = engine.process(Command::PlaceOrder(order));
                for event in place_events {
                    if let Some(entry) = match_event_to_entry(event, SymbolId(0)) {
                        audit.log_entry(entry);
                    }
                }
            }
            start.elapsed()
        });
    });

    // Aggressive taker: market orders hitting the book
    group.bench_function("taker_market_sweep_with_pipeline", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            let mut risk = risk_engine_for_bench();
            let audit = AsyncAuditLogger::new(65536, AuditFormat::Json);

            for i in 0..iters {
                let ask = Order::new_limit(
                    i * 2 + 1, SymbolId(0), Side::Sell, TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE), FixedQty::from_raw(ONE_BTC), 0,
                );
                engine.process(Command::PlaceOrder(ask));
            }

            let start = std::time::Instant::now();
            for i in 0..iters {
                let order = Order::new_market(
                    i * 2 + 2, SymbolId(0), Side::Buy, TimeInForce::GTC,
                    FixedQty::from_raw(ONE_BTC), i,
                );
                let _ = risk.check_order(&order);
                let events = engine.process(Command::PlaceOrder(order));
                for event in events {
                    if let Some(entry) = match_event_to_entry(event, SymbolId(0)) {
                        audit.log_entry(entry);
                    }
                }
            }
            start.elapsed()
        });
    });

    // IOC snipe: immediate-or-cancel with partial fill
    group.bench_function("ioc_snipe_with_pipeline", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            let mut risk = risk_engine_for_bench();
            let audit = AsyncAuditLogger::new(65536, AuditFormat::Json);

            for i in 0..iters {
                let ask = Order::new_limit(
                    i * 2 + 1, SymbolId(0), Side::Sell, TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE),
                    FixedQty::from_raw(ONE_BTC / 2), 0, // 0.5 BTC
                );
                engine.process(Command::PlaceOrder(ask));
            }

            let start = std::time::Instant::now();
            for i in 0..iters {
                let order = Order::new_limit(
                    i * 2 + 2, SymbolId(0), Side::Buy, TimeInForce::IOC,
                    FixedPrice::from_raw(BASE_PRICE),
                    FixedQty::from_raw(ONE_BTC), i, // wants 1, gets 0.5
                );
                let _ = risk.check_order(&order);
                let events = engine.process(Command::PlaceOrder(order));
                for event in events {
                    if let Some(entry) = match_event_to_entry(event, SymbolId(0)) {
                        audit.log_entry(entry);
                    }
                }
            }
            start.elapsed()
        });
    });

    group.finish();
}

// ═══════════════════════════════════════════════════════════════════════════
// 7. THROUGHPUT — sustained operations per second
// ═══════════════════════════════════════════════════════════════════════════

fn bench_throughput_full_pipeline(c: &mut Criterion) {
    install_noop_tracing();
    let mut group = c.benchmark_group("throughput_full_pipeline");

    // Place non-crossing orders with full pipeline (risk + async audit)
    group.bench_function("place_no_match", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            let mut risk = risk_engine_for_bench();
            let audit = AsyncAuditLogger::new(65536, AuditFormat::Json);

            // Pre-populate asks
            for i in 0..100u64 {
                let ask = Order::new_limit(
                    i + 1, SymbolId(0), Side::Sell, TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE + (i as i64 + 1) * TICK),
                    FixedQty::from_raw(ONE_BTC), 0,
                );
                engine.process(Command::PlaceOrder(ask));
            }

            let start = std::time::Instant::now();
            for i in 0..iters {
                let order = Order::new_limit(
                    1000 + i, SymbolId(0), Side::Buy, TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE - 100 * TICK),
                    FixedQty::from_raw(ONE_BTC), i,
                );
                let _ = risk.check_order(&order);
                audit.log_entry(order_to_entry(&order));
                let events = engine.process(Command::PlaceOrder(order));
                for event in events {
                    if let Some(entry) = match_event_to_entry(event, SymbolId(0)) {
                        audit.log_entry(entry);
                    }
                }
            }
            start.elapsed()
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_clock_comparison,
    bench_risk_check,
    bench_audit_comparison,
    bench_wal_write,
    bench_pipeline_breakdown,
    bench_hft_scenarios,
    bench_throughput_full_pipeline,
);
criterion_main!(benches);
