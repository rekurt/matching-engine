use criterion::{criterion_group, criterion_main, Criterion};

use me_core::order::{Order, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::*;
use me_engine::command::Command;
use me_engine::engine::Engine;

fn bench_single_order_latency(c: &mut Criterion) {
    let config = SymbolConfig::default_btc_usdt();
    let mut engine = Engine::new(config);

    // Pre-populate book with 100 ask levels
    for i in 0..100 {
        let order = Order::new_limit(
            i + 1,
            SymbolId(0),
            Side::Sell,
            TimeInForce::GTC,
            FixedPrice::from_raw(10_000_000_000 + (i as i64) * 1_000_000),
            FixedQty::from_raw(100_000_000),
            0,
        );
        engine.process(Command::PlaceOrder(order));
    }

    let mut next_id = 1000u64;

    c.bench_function("single_limit_order_latency", |b| {
        b.iter(|| {
            next_id += 1;
            let order = Order::new_limit(
                next_id,
                SymbolId(0),
                Side::Buy,
                TimeInForce::GTC,
                FixedPrice::from_raw(9_999_000_000),
                FixedQty::from_raw(100_000_000),
                0,
            );
            engine.process(Command::PlaceOrder(order));
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
                    FixedPrice::from_raw(10_000_000_000),
                    FixedQty::from_raw(100_000_000),
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

criterion_group!(benches, bench_single_order_latency);
criterion_main!(benches);
