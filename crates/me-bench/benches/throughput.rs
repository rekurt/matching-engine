use criterion::{criterion_group, criterion_main, Criterion};

use me_core::order::{Order, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::*;
use me_engine::command::Command;
use me_engine::engine::Engine;

fn bench_limit_order_throughput(c: &mut Criterion) {
    c.bench_function("limit_order_place_and_match", |b| {
        b.iter_custom(|iters| {
            let config = SymbolConfig::default_btc_usdt();
            let mut engine = Engine::new(config);

            // Pre-populate asks
            for i in 0..1000 {
                let order = Order::new_limit(
                    i + 100_000,
                    SymbolId(0),
                    Side::Sell,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(10_000_000_000 + (i as i64) * 1_000_000),
                    FixedQty::from_raw(100_000_000),
                    0,
                );
                engine.process(Command::PlaceOrder(order));
            }

            let start = std::time::Instant::now();
            for i in 0..iters {
                let order = Order::new_limit(
                    i + 1_000_000,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(9_999_000_000), // won't cross
                    FixedQty::from_raw(100_000_000),
                    0,
                );
                engine.process(Command::PlaceOrder(order));
            }
            start.elapsed()
        });
    });
}

fn bench_market_order_throughput(c: &mut Criterion) {
    c.bench_function("market_order_sweep", |b| {
        b.iter_custom(|iters| {
            let config = SymbolConfig::default_btc_usdt();
            let mut engine = Engine::new(config);

            let start = std::time::Instant::now();
            for i in 0..iters {
                // Place an ask, then sweep it with a market buy
                let ask = Order::new_limit(
                    i * 2 + 1,
                    SymbolId(0),
                    Side::Sell,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(10_000_000_000),
                    FixedQty::from_raw(100_000_000),
                    0,
                );
                engine.process(Command::PlaceOrder(ask));

                let buy = Order::new_market(
                    i * 2 + 2,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedQty::from_raw(100_000_000),
                    0,
                );
                engine.process(Command::PlaceOrder(buy));
            }
            start.elapsed()
        });
    });
}

criterion_group!(
    benches,
    bench_limit_order_throughput,
    bench_market_order_throughput
);
criterion_main!(benches);
