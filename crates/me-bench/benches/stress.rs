//! Stress benchmarks: operations on very large order books.
//!
//! Measures how engine performance scales with book depth (number of levels)
//! and queue depth (orders per level). Tests placement, cancellation,
//! amendment, and matching against books with 10k–100k orders.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

use me_core::order::{Order, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::*;
use me_engine::command::Command;
use me_engine::engine::Engine;

const BASE_PRICE: i64 = 10_000_000_000;
const TICK: i64 = 1_000_000;
const ONE_BTC: u64 = 100_000_000;

/// Engine with relaxed max_qty for stress tests (allows up to 100k BTC orders).
fn stress_config() -> SymbolConfig {
    let mut cfg = SymbolConfig::default_btc_usdt();
    cfg.max_qty = FixedQty::from_raw(100_000 * ONE_BTC); // 100,000 BTC
    cfg
}

fn fresh_engine() -> Engine {
    Engine::new(stress_config())
}

/// Build a two-sided book: `n_levels` on each side, `orders_per_level` at each.
/// Asks: BASE_PRICE+TICK .. BASE_PRICE+n_levels*TICK
/// Bids: BASE_PRICE-TICK .. BASE_PRICE-n_levels*TICK
fn engine_two_sided(n_levels: usize, orders_per_level: usize) -> (Engine, u64) {
    let mut engine = fresh_engine();
    let mut id: u64 = 1;
    for lvl in 0..n_levels {
        let ask_price = FixedPrice::from_raw(BASE_PRICE + (lvl as i64 + 1) * TICK);
        let bid_price = FixedPrice::from_raw(BASE_PRICE - (lvl as i64 + 1) * TICK);
        for _ in 0..orders_per_level {
            let ask = Order::new_limit(
                id,
                SymbolId(0),
                Side::Sell,
                TimeInForce::GTC,
                ask_price,
                FixedQty::from_raw(ONE_BTC),
                0,
            );
            engine.process(Command::PlaceOrder(ask));
            id += 1;
            let bid = Order::new_limit(
                id,
                SymbolId(0),
                Side::Buy,
                TimeInForce::GTC,
                bid_price,
                FixedQty::from_raw(ONE_BTC),
                0,
            );
            engine.process(Command::PlaceOrder(bid));
            id += 1;
        }
    }
    (engine, id)
}

// ---------------------------------------------------------------------------
// 1. Limit order placement scaling: how does insert cost grow with book size?
//    Parameterized: 100, 1_000, 10_000, 50_000 levels
// ---------------------------------------------------------------------------

fn bench_place_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("place_into_large_book");
    group.sample_size(50);

    for &n_levels in &[100, 1_000, 10_000, 50_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{n_levels}_levels")),
            &n_levels,
            |b, &n_levels| {
                b.iter_custom(|iters| {
                    let (mut engine, next_id) = engine_two_sided(n_levels, 1);

                    // Place non-crossing bids at a price BELOW all existing bids.
                    // Single price level — no BTreeMap growth during measurement.
                    let far_price =
                        FixedPrice::from_raw(BASE_PRICE - (n_levels as i64 + 100) * TICK);
                    let start = std::time::Instant::now();
                    for i in 0..iters {
                        let order = Order::new_limit(
                            next_id + i,
                            SymbolId(0),
                            Side::Buy,
                            TimeInForce::GTC,
                            far_price,
                            FixedQty::from_raw(ONE_BTC),
                            0,
                        );
                        engine.process(Command::PlaceOrder(order));
                    }
                    start.elapsed()
                });
            },
        );
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// 2. Cancel from a large book: how does cancel cost scale?
//    Cancel from the MIDDLE of the book (not best bid/ask).
// ---------------------------------------------------------------------------

fn bench_cancel_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("cancel_from_large_book");
    group.sample_size(50);

    for &n_levels in &[100, 1_000, 10_000, 50_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{n_levels}_levels")),
            &n_levels,
            |b, &n_levels| {
                b.iter_custom(|iters| {
                    let (mut engine, next_id) = engine_two_sided(n_levels, 1);

                    // Insert `iters` extra orders at a mid-book price to cancel them.
                    let cancel_price =
                        FixedPrice::from_raw(BASE_PRICE - (n_levels as i64 / 2) * TICK);
                    for i in 0..iters {
                        let order = Order::new_limit(
                            next_id + i,
                            SymbolId(0),
                            Side::Buy,
                            TimeInForce::GTC,
                            cancel_price,
                            FixedQty::from_raw(ONE_BTC),
                            0,
                        );
                        engine.process(Command::PlaceOrder(order));
                    }

                    // Cancel exactly `iters` orders
                    let start = std::time::Instant::now();
                    for i in 0..iters {
                        engine.process(Command::CancelOrder {
                            order_id: next_id + i,
                            symbol: SymbolId(0),
                            timestamp: 0,
                        });
                    }
                    start.elapsed()
                });
            },
        );
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// 3. Market sweep scaling: sweep N levels in one shot.
//    Parameterized: 100, 500, 2_000, 5_000 levels
// ---------------------------------------------------------------------------

fn bench_sweep_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("market_sweep_depth");
    group.sample_size(20);

    for &n_levels in &[100, 500, 2_000, 5_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{n_levels}_levels")),
            &n_levels,
            |b, &n_levels| {
                b.iter_custom(|iters| {
                    let mut engine = fresh_engine();
                    let mut id: u64 = 1;

                    // Initial population
                    for lvl in 0..(n_levels as u64) {
                        let ask = Order::new_limit(
                            id,
                            SymbolId(0),
                            Side::Sell,
                            TimeInForce::GTC,
                            FixedPrice::from_raw(BASE_PRICE + (lvl as i64) * TICK),
                            FixedQty::from_raw(ONE_BTC),
                            0,
                        );
                        engine.process(Command::PlaceOrder(ask));
                        id += 1;
                    }

                    let mut total = std::time::Duration::ZERO;
                    for _ in 0..iters {
                        let t = std::time::Instant::now();
                        let sweep = Order::new_market(
                            id,
                            SymbolId(0),
                            Side::Buy,
                            TimeInForce::GTC,
                            FixedQty::from_raw(ONE_BTC * n_levels as u64),
                            0,
                        );
                        engine.process(Command::PlaceOrder(sweep));
                        total += t.elapsed();
                        id += 1;

                        // Untimed: replenish
                        for lvl in 0..(n_levels as u64) {
                            let ask = Order::new_limit(
                                id,
                                SymbolId(0),
                                Side::Sell,
                                TimeInForce::GTC,
                                FixedPrice::from_raw(BASE_PRICE + (lvl as i64) * TICK),
                                FixedQty::from_raw(ONE_BTC),
                                0,
                            );
                            engine.process(Command::PlaceOrder(ask));
                            id += 1;
                        }
                    }
                    total
                });
            },
        );
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// 4. Deep FIFO queue: many orders at the SAME price, then sweep all.
//    Parameterized: 1_000, 5_000, 10_000, 50_000 orders
// ---------------------------------------------------------------------------

fn bench_fifo_queue_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("fifo_queue_sweep");
    group.sample_size(20);

    for &depth in &[1_000, 5_000, 10_000, 50_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{depth}_orders")),
            &depth,
            |b, &depth| {
                b.iter_custom(|iters| {
                    let mut engine = fresh_engine();
                    let mut id: u64 = 1;

                    // All orders at the same price
                    for _ in 0..depth {
                        let ask = Order::new_limit(
                            id,
                            SymbolId(0),
                            Side::Sell,
                            TimeInForce::GTC,
                            FixedPrice::from_raw(BASE_PRICE),
                            FixedQty::from_raw(ONE_BTC),
                            0,
                        );
                        engine.process(Command::PlaceOrder(ask));
                        id += 1;
                    }

                    let mut total = std::time::Duration::ZERO;
                    for _ in 0..iters {
                        let t = std::time::Instant::now();
                        let sweep = Order::new_market(
                            id,
                            SymbolId(0),
                            Side::Buy,
                            TimeInForce::GTC,
                            FixedQty::from_raw(ONE_BTC * depth as u64),
                            0,
                        );
                        engine.process(Command::PlaceOrder(sweep));
                        total += t.elapsed();
                        id += 1;

                        // Untimed: replenish
                        for _ in 0..depth {
                            let ask = Order::new_limit(
                                id,
                                SymbolId(0),
                                Side::Sell,
                                TimeInForce::GTC,
                                FixedPrice::from_raw(BASE_PRICE),
                                FixedQty::from_raw(ONE_BTC),
                                0,
                            );
                            engine.process(Command::PlaceOrder(ask));
                            id += 1;
                        }
                    }
                    total
                });
            },
        );
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// 5. Amend in a large book: price change triggers cancel+re-insert.
//    How does BTreeMap re-insert cost scale with tree size?
// ---------------------------------------------------------------------------

fn bench_amend_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("amend_in_large_book");
    group.sample_size(50);

    for &n_levels in &[100, 1_000, 10_000, 50_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{n_levels}_levels")),
            &n_levels,
            |b, &n_levels| {
                b.iter_custom(|iters| {
                    let (mut engine, _) = engine_two_sided(n_levels, 1);

                    // Amend bid orders: change price (cancel + re-insert into BTreeMap)
                    let start = std::time::Instant::now();
                    for i in 0..iters {
                        // Cycle through bid order IDs: 2, 4, 6, ...
                        let bid_id = ((i % n_levels as u64) * 2) + 2;
                        let new_price_offset = (i % 10) as i64 + 1;
                        engine.process(Command::AmendOrder {
                            order_id: bid_id,
                            symbol: SymbolId(0),
                            new_price: Some(FixedPrice::from_raw(
                                BASE_PRICE - (n_levels as i64 + new_price_offset) * TICK,
                            )),
                            new_qty: None,
                            timestamp: i,
                        });
                    }
                    start.elapsed()
                });
            },
        );
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// 6. HFT cancel-replace on a large book.
//    Realistic: 50k-level two-sided book, rapid cancel+place at best bid.
// ---------------------------------------------------------------------------

fn bench_hft_large_book(c: &mut Criterion) {
    let mut group = c.benchmark_group("hft_cancel_replace_large_book");
    group.sample_size(50);

    for &n_levels in &[1_000, 10_000, 50_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{n_levels}_levels")),
            &n_levels,
            |b, &n_levels| {
                b.iter_custom(|iters| {
                    let (mut engine, mut next_id) = engine_two_sided(n_levels, 1);

                    // Place a "maker" bid at best bid (just below spread)
                    let maker_price = FixedPrice::from_raw(BASE_PRICE - TICK);
                    let mut maker_id = next_id;
                    let maker = Order::new_limit(
                        maker_id,
                        SymbolId(0),
                        Side::Buy,
                        TimeInForce::GTC,
                        maker_price,
                        FixedQty::from_raw(ONE_BTC),
                        0,
                    );
                    engine.process(Command::PlaceOrder(maker));
                    next_id += 1;

                    let start = std::time::Instant::now();
                    for i in 0..iters {
                        // Cancel current maker
                        engine.process(Command::CancelOrder {
                            order_id: maker_id,
                            symbol: SymbolId(0),
                            timestamp: i,
                        });
                        // Place new maker at slightly different price
                        maker_id = next_id;
                        next_id += 1;
                        let price_jitter = (i % 5) as i64 + 1;
                        let new_bid = Order::new_limit(
                            maker_id,
                            SymbolId(0),
                            Side::Buy,
                            TimeInForce::GTC,
                            FixedPrice::from_raw(BASE_PRICE - price_jitter * TICK),
                            FixedQty::from_raw(ONE_BTC),
                            0,
                        );
                        engine.process(Command::PlaceOrder(new_bid));
                    }
                    start.elapsed()
                });
            },
        );
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// 7. Stop book scaling: place many stops, then trigger cascade.
// ---------------------------------------------------------------------------

fn bench_stop_trigger_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("stop_trigger_cascade");
    group.sample_size(20);

    for &n_stops in &[10, 50, 100] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{n_stops}_stops")),
            &n_stops,
            |b, &n_stops| {
                b.iter_custom(|iters| {
                    let mut total = std::time::Duration::ZERO;

                    for _ in 0..iters {
                        let mut engine = fresh_engine();
                        let mut id: u64 = 1;
                        let trigger_price = FixedPrice::from_raw(BASE_PRICE + 10 * TICK);

                        // Place N stop-buy orders that all trigger at the same price.
                        // Each needs a corresponding sell to fill against.
                        for j in 0..n_stops {
                            // Sell resting at limit_price (liquidity for triggered stop)
                            let limit_price = BASE_PRICE + (11 + j as i64) * TICK;
                            let sell = Order::new_limit(
                                id,
                                SymbolId(0),
                                Side::Sell,
                                TimeInForce::GTC,
                                FixedPrice::from_raw(limit_price),
                                FixedQty::from_raw(ONE_BTC),
                                0,
                            );
                            engine.process(Command::PlaceOrder(sell));
                            id += 1;

                            // Stop-buy triggers at trigger_price
                            let stop = Order::new_stop_limit(
                                id,
                                SymbolId(0),
                                Side::Buy,
                                TimeInForce::GTC,
                                FixedPrice::from_raw(limit_price),
                                trigger_price,
                                FixedQty::from_raw(ONE_BTC),
                                0,
                            );
                            engine.process(Command::PlaceOrder(stop));
                            id += 1;
                        }

                        // Place a resting buy at trigger_price
                        let resting = Order::new_limit(
                            id,
                            SymbolId(0),
                            Side::Buy,
                            TimeInForce::GTC,
                            trigger_price,
                            FixedQty::from_raw(ONE_BTC),
                            0,
                        );
                        engine.process(Command::PlaceOrder(resting));
                        id += 1;

                        // TIMED: sell into the resting buy → trade → triggers all stops
                        let t = std::time::Instant::now();
                        let sell = Order::new_limit(
                            id,
                            SymbolId(0),
                            Side::Sell,
                            TimeInForce::GTC,
                            trigger_price,
                            FixedQty::from_raw(ONE_BTC),
                            0,
                        );
                        engine.process(Command::PlaceOrder(sell));
                        total += t.elapsed();
                    }
                    total
                });
            },
        );
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// 8. Book build throughput: how fast can we build a book from scratch?
//    Measures sustained insert into a growing BTreeMap.
// ---------------------------------------------------------------------------

fn bench_book_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("book_build_from_scratch");
    group.sample_size(20);

    for &n_orders in &[1_000, 10_000, 50_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{n_orders}_orders")),
            &n_orders,
            |b, &n_orders| {
                b.iter_custom(|iters| {
                    let mut total = std::time::Duration::ZERO;
                    for _ in 0..iters {
                        let mut engine = fresh_engine();

                        let t = std::time::Instant::now();
                        for i in 0..(n_orders as u64) {
                            // Alternate bid/ask, each at unique price
                            if i % 2 == 0 {
                                let ask = Order::new_limit(
                                    i + 1,
                                    SymbolId(0),
                                    Side::Sell,
                                    TimeInForce::GTC,
                                    FixedPrice::from_raw(BASE_PRICE + (i as i64 / 2 + 1) * TICK),
                                    FixedQty::from_raw(ONE_BTC),
                                    0,
                                );
                                engine.process(Command::PlaceOrder(ask));
                            } else {
                                let bid = Order::new_limit(
                                    i + 1,
                                    SymbolId(0),
                                    Side::Buy,
                                    TimeInForce::GTC,
                                    FixedPrice::from_raw(BASE_PRICE - (i as i64 / 2 + 1) * TICK),
                                    FixedQty::from_raw(ONE_BTC),
                                    0,
                                );
                                engine.process(Command::PlaceOrder(bid));
                            }
                        }
                        total += t.elapsed();
                    }
                    total
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_place_scaling,
    bench_cancel_scaling,
    bench_sweep_scaling,
    bench_fifo_queue_scaling,
    bench_amend_scaling,
    bench_hft_large_book,
    bench_stop_trigger_scaling,
    bench_book_build,
);
criterion_main!(benches);
