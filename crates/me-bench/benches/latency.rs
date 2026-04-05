use criterion::{criterion_group, criterion_main, Criterion};

use me_core::order::{Order, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::*;
use me_engine::command::Command;
use me_engine::engine::Engine;

/// Price in raw units: 100.00 in 8-dp fixed point.
const BASE_PRICE: i64 = 10_000_000_000;
const TICK: i64 = 1_000_000; // 0.01
/// Quantity in raw units: 1.0 BTC in 8-dp fixed point.
const ONE_BTC: u64 = 100_000_000;

// ---------------------------------------------------------------------------
// 1. Single limit order latency (one-sided book)
// ---------------------------------------------------------------------------

fn bench_single_order_latency(c: &mut Criterion) {
    c.bench_function("single_limit_order_latency", |b| {
        b.iter_custom(|iters| {
            let config = SymbolConfig::default_btc_usdt();
            let mut engine = Engine::new(config);

            // Pre-populate book with 100 ask levels
            for i in 0..100u64 {
                let order = Order::new_limit(
                    i + 1,
                    SymbolId(0),
                    Side::Sell,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE + (i as i64) * TICK),
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(order));
            }

            let start = std::time::Instant::now();
            for i in 0..iters {
                let order = Order::new_limit(
                    1000 + i,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE - 100 * TICK), // won't cross
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(order));
            }
            start.elapsed()
        });
    });

    c.bench_function("single_cancel_latency", |b| {
        b.iter_custom(|iters| {
            let config = SymbolConfig::default_btc_usdt();
            let mut engine = Engine::new(config);

            // Insert orders to cancel
            for i in 0..iters {
                let order = Order::new_limit(
                    i + 1,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE),
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(order));
            }

            let start = std::time::Instant::now();
            for i in 0..iters {
                engine.process(Command::CancelOrder {
                    order_id: i + 1,
                    symbol: SymbolId(0),
                    timestamp: 0,
                });
            }
            start.elapsed()
        });
    });
}

// ---------------------------------------------------------------------------
// 2. Limit order latency on a two-sided book (100 bids + 100 asks)
// ---------------------------------------------------------------------------

fn bench_two_sided_book_latency(c: &mut Criterion) {
    c.bench_function("two_sided_book_limit_latency", |b| {
        b.iter_custom(|iters| {
            let config = SymbolConfig::default_btc_usdt();
            let mut engine = Engine::new(config);

            // 100 ask levels + 100 bid levels
            let mut id = 1u64;
            for i in 0..100u64 {
                let ask = Order::new_limit(
                    id,
                    SymbolId(0),
                    Side::Sell,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE + (i as i64 + 1) * TICK),
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
                    FixedPrice::from_raw(BASE_PRICE - (i as i64) * TICK),
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(bid));
                id += 1;
            }

            // Place non-crossing bids below all existing bids
            let start = std::time::Instant::now();
            for i in 0..iters {
                let order = Order::new_limit(
                    id + i,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE - 200 * TICK),
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(order));
            }
            start.elapsed()
        });
    });
}

criterion_group!(
    benches,
    bench_single_order_latency,
    bench_two_sided_book_latency
);
criterion_main!(benches);
