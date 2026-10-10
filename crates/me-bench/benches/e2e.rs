use criterion::{criterion_group, criterion_main, Criterion};

use me_core::order::{Order, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::*;
use me_engine::command::Command;
use me_engine::engine::Engine;
use me_engine::router::Router;

// Helpers

/// Price in raw units: 100.00 in 8-dp fixed point.
const BASE_PRICE: i64 = 10_000_000_000;
const TICK: i64 = 1_000_000; // 0.01
/// Quantity in raw units: 1.0 BTC in 8-dp fixed point.
const ONE_BTC: u64 = 100_000_000;

fn fresh_engine() -> Engine {
    Engine::new(SymbolConfig::default_btc_usdt())
}

/// Build a book with `n_levels` ask levels, each with `orders_per_level` orders
/// of 1 BTC at prices BASE_PRICE, BASE_PRICE+TICK, ...
fn engine_with_asks(n_levels: usize, orders_per_level: usize) -> (Engine, u64) {
    let mut engine = fresh_engine();
    let mut id: u64 = 1;
    for lvl in 0..n_levels {
        let price = FixedPrice::from_raw(BASE_PRICE + (lvl as i64) * TICK);
        for _ in 0..orders_per_level {
            let order = Order::new_limit(
                id,
                SymbolId(0),
                Side::Sell,
                TimeInForce::GTC,
                price,
                FixedQty::from_raw(ONE_BTC),
                0,
            );
            engine.process(Command::PlaceOrder(order));
            id += 1;
        }
    }
    (engine, id)
}

/// Build a book with `n_levels` bid levels below BASE_PRICE.
#[allow(dead_code)]
fn engine_with_bids(n_levels: usize, orders_per_level: usize) -> (Engine, u64) {
    let mut engine = fresh_engine();
    let mut id: u64 = 1;
    for lvl in 0..n_levels {
        let price = FixedPrice::from_raw(BASE_PRICE - (lvl as i64) * TICK);
        for _ in 0..orders_per_level {
            let order = Order::new_limit(
                id,
                SymbolId(0),
                Side::Buy,
                TimeInForce::GTC,
                price,
                FixedQty::from_raw(ONE_BTC),
                0,
            );
            engine.process(Command::PlaceOrder(order));
            id += 1;
        }
    }
    (engine, id)
}

// 1. Limit order placement (no match)

fn bench_limit_no_match(c: &mut Criterion) {
    c.bench_function("e2e_limit_no_match", |b| {
        b.iter_custom(|iters| {
            let (mut engine, _) = engine_with_asks(100, 1);

            let start = std::time::Instant::now();
            for i in 0..iters {
                let order = Order::new_limit(
                    101 + i,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE - 100 * TICK), // well below asks
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(order));
            }
            start.elapsed()
        });
    });
}

// 2. Limit order full fill

fn bench_limit_full_fill(c: &mut Criterion) {
    c.bench_function("e2e_limit_full_fill", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            // Pre-populate with asks that will be consumed
            for i in 0..iters {
                let ask = Order::new_limit(
                    i * 2 + 1,
                    SymbolId(0),
                    Side::Sell,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE),
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(ask));
            }

            let start = std::time::Instant::now();
            for i in 0..iters {
                let buy = Order::new_limit(
                    i * 2 + 2,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE), // crosses at same price
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(buy));
            }
            start.elapsed()
        });
    });
}

// 3. Limit order partial fill

fn bench_limit_partial_fill(c: &mut Criterion) {
    c.bench_function("e2e_limit_partial_fill", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            // Asks of 0.5 BTC each
            for i in 0..iters {
                let ask = Order::new_limit(
                    i * 2 + 1,
                    SymbolId(0),
                    Side::Sell,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE),
                    FixedQty::from_raw(ONE_BTC / 2),
                    0,
                );
                engine.process(Command::PlaceOrder(ask));
            }

            let start = std::time::Instant::now();
            for i in 0..iters {
                // Buy 1 BTC, only 0.5 available => partial
                let buy = Order::new_limit(
                    i * 2 + 2,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE),
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(buy));
            }
            start.elapsed()
        });
    });
}

// 4. Market order sweep single level

fn bench_market_sweep_single_level(c: &mut Criterion) {
    c.bench_function("e2e_market_sweep_single_level", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            // Place asks all at one price
            for i in 0..iters {
                let ask = Order::new_limit(
                    i * 2 + 1,
                    SymbolId(0),
                    Side::Sell,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE),
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(ask));
            }

            let start = std::time::Instant::now();
            for i in 0..iters {
                let buy = Order::new_market(
                    i * 2 + 2,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(buy));
            }
            start.elapsed()
        });
    });
}

// 5. Market order sweep multiple levels (10+)

fn bench_market_sweep_multi_level(c: &mut Criterion) {
    c.bench_function("e2e_market_sweep_multi_level", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            let levels = 15;
            // Pre-populate 15 ask levels, 1 BTC each, enough for all iters
            for i in 0..iters {
                for lvl in 0..levels {
                    let ask = Order::new_limit(
                        i * (levels as u64) * 2 + (lvl as u64) * 2 + 1,
                        SymbolId(0),
                        Side::Sell,
                        TimeInForce::GTC,
                        FixedPrice::from_raw(BASE_PRICE + (lvl as i64) * TICK),
                        FixedQty::from_raw(ONE_BTC),
                        0,
                    );
                    engine.process(Command::PlaceOrder(ask));
                }
            }

            let start = std::time::Instant::now();
            for i in 0..iters {
                // Market buy sweeping across all 15 levels (15 BTC)
                let buy = Order::new_market(
                    i * (levels as u64) * 2 + (levels as u64) * 2,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedQty::from_raw(ONE_BTC * levels as u64),
                    0,
                );
                engine.process(Command::PlaceOrder(buy));
            }
            start.elapsed()
        });
    });
}

// 6. IOC order with match

fn bench_ioc_with_match(c: &mut Criterion) {
    c.bench_function("e2e_ioc_with_match", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            // Place asks (only 0.5 BTC each so IOC partially fills)
            for i in 0..iters {
                let ask = Order::new_limit(
                    i * 2 + 1,
                    SymbolId(0),
                    Side::Sell,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE),
                    FixedQty::from_raw(ONE_BTC / 2),
                    0,
                );
                engine.process(Command::PlaceOrder(ask));
            }

            let start = std::time::Instant::now();
            for i in 0..iters {
                let buy = Order::new_limit(
                    i * 2 + 2,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::IOC,
                    FixedPrice::from_raw(BASE_PRICE),
                    FixedQty::from_raw(ONE_BTC), // wants 1, only 0.5 avail
                    0,
                );
                engine.process(Command::PlaceOrder(buy));
            }
            start.elapsed()
        });
    });
}

// 7. IOC order no match

fn bench_ioc_no_match(c: &mut Criterion) {
    let (mut engine, mut next_id) = engine_with_asks(10, 1);

    c.bench_function("e2e_ioc_no_match", |b| {
        b.iter(|| {
            next_id += 1;
            let order = Order::new_limit(
                next_id,
                SymbolId(0),
                Side::Buy,
                TimeInForce::IOC,
                FixedPrice::from_raw(BASE_PRICE - 200 * TICK), // way below best ask
                FixedQty::from_raw(ONE_BTC),
                0,
            );
            engine.process(Command::PlaceOrder(order));
        });
    });
}

// 8. FOK order success

fn bench_fok_success(c: &mut Criterion) {
    c.bench_function("e2e_fok_success", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            for i in 0..iters {
                let ask = Order::new_limit(
                    i * 2 + 1,
                    SymbolId(0),
                    Side::Sell,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE),
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(ask));
            }

            let start = std::time::Instant::now();
            for i in 0..iters {
                let buy = Order::new_limit(
                    i * 2 + 2,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::FOK,
                    FixedPrice::from_raw(BASE_PRICE),
                    FixedQty::from_raw(ONE_BTC), // exact match
                    0,
                );
                engine.process(Command::PlaceOrder(buy));
            }
            start.elapsed()
        });
    });
}

// 9. FOK order reject

fn bench_fok_reject(c: &mut Criterion) {
    c.bench_function("e2e_fok_reject", |b| {
        b.iter_custom(|iters| {
            // Book has 1 BTC ask, FOK wants 2 => reject.
            let mut engine = fresh_engine();
            let ask = Order::new_limit(
                1,
                SymbolId(0),
                Side::Sell,
                TimeInForce::GTC,
                FixedPrice::from_raw(BASE_PRICE),
                FixedQty::from_raw(ONE_BTC),
                0,
            );
            engine.process(Command::PlaceOrder(ask));

            let start = std::time::Instant::now();
            for i in 0..iters {
                let buy = Order::new_limit(
                    100 + i,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::FOK,
                    FixedPrice::from_raw(BASE_PRICE),
                    FixedQty::from_raw(ONE_BTC * 2), // insufficient liquidity
                    0,
                );
                engine.process(Command::PlaceOrder(buy));
            }
            start.elapsed()
        });
    });
}

// 10. Cancel order (middle of book)

fn bench_cancel_mid_book(c: &mut Criterion) {
    c.bench_function("e2e_cancel_mid_book", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            // Ensure enough orders so every cancel targets a unique existing order.
            let total = (iters * 2).max(200);
            for i in 0..total {
                let order = Order::new_limit(
                    i + 1,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE - (i as i64) * TICK),
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(order));
            }

            // Cancel from the middle range — each order exactly once
            let start_offset = total / 4;
            let start = std::time::Instant::now();
            for i in 0..iters {
                engine.process(Command::CancelOrder {
                    order_id: start_offset + i + 1,
                    symbol: SymbolId(0),
                    timestamp: 0,
                });
            }
            start.elapsed()
        });
    });
}

// 11. Cancel best bid

fn bench_cancel_best_bid(c: &mut Criterion) {
    c.bench_function("e2e_cancel_best_bid", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            // Insert bids: each at a unique best price so cancelling one shifts BBO.
            for i in 0..iters {
                let order = Order::new_limit(
                    i + 1,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE - (i as i64) * TICK),
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(order));
            }

            let start = std::time::Instant::now();
            // Cancel from the best (highest) bid downward
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

// 12. Amend order qty down (in-place)

fn bench_amend_qty_down(c: &mut Criterion) {
    let mut engine = fresh_engine();
    // Place 1000 bids
    for i in 0..1000u64 {
        let order = Order::new_limit(
            i + 1,
            SymbolId(0),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_raw(BASE_PRICE - (i as i64) * TICK),
            FixedQty::from_raw(ONE_BTC * 10),
            0,
        );
        engine.process(Command::PlaceOrder(order));
    }

    let mut counter = 0u64;

    c.bench_function("e2e_amend_qty_down", |b| {
        b.iter(|| {
            counter += 1;
            let target_id = (counter % 1000) + 1;
            engine.process(Command::AmendOrder {
                order_id: target_id,
                symbol: SymbolId(0),
                new_price: None,
                new_qty: Some(FixedQty::from_raw(ONE_BTC * 5)), // reduce qty
                timestamp: counter,
            });
        });
    });
}

// 13. Amend order price change (cancel + replace)

fn bench_amend_price_change(c: &mut Criterion) {
    let mut engine = fresh_engine();
    for i in 0..1000u64 {
        let order = Order::new_limit(
            i + 1,
            SymbolId(0),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_raw(BASE_PRICE - (i as i64) * TICK),
            FixedQty::from_raw(ONE_BTC),
            0,
        );
        engine.process(Command::PlaceOrder(order));
    }

    let mut counter = 0u64;

    c.bench_function("e2e_amend_price_change", |b| {
        b.iter(|| {
            counter += 1;
            let target_id = (counter % 1000) + 1;
            engine.process(Command::AmendOrder {
                order_id: target_id,
                symbol: SymbolId(0),
                new_price: Some(FixedPrice::from_raw(
                    BASE_PRICE - 2000 * TICK - (counter as i64 % 500) * TICK,
                )),
                new_qty: None,
                timestamp: counter,
            });
        });
    });
}

// 14. Stop order placement

fn bench_stop_order_placement(c: &mut Criterion) {
    c.bench_function("e2e_stop_order_placement", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();

            let start = std::time::Instant::now();
            for i in 0..iters {
                let order = Order::new_stop_limit(
                    i + 1,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE + 10 * TICK), // limit price
                    FixedPrice::from_raw(BASE_PRICE + 5 * TICK),  // stop trigger
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(order));
            }
            start.elapsed()
        });
    });
}

// 15. Stop order trigger

fn bench_stop_order_trigger(c: &mut Criterion) {
    c.bench_function("e2e_stop_order_trigger", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            let mut id: u64 = 1;

            // For each iteration: place a stop-buy above market, then trigger
            // it by executing a trade at or above the stop price.
            // We need a sell resting at the stop limit price so the triggered
            // order can fill.
            for i in 0..iters {
                let base = i * 4;
                let trigger_price = BASE_PRICE + ((i as i64) % 100 + 1) * TICK;
                let limit_price = trigger_price + TICK;

                // 1. Place a sell limit at the trigger level (provides liquidity
                //    for the triggered stop order).
                let sell_for_trigger = Order::new_limit(
                    base + 1,
                    SymbolId(0),
                    Side::Sell,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(limit_price),
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(sell_for_trigger));

                // 2. Place stop-buy that triggers at trigger_price.
                let stop = Order::new_stop_limit(
                    base + 2,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(limit_price),
                    FixedPrice::from_raw(trigger_price),
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(stop));

                // 3. Place a sell that will match (creating a trade at trigger_price)
                //    to trigger the stop. We need a resting buy and a crossing sell.
                let resting_buy = Order::new_limit(
                    base + 3,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(trigger_price),
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(resting_buy));

                id = base + 4;
            }

            // Now trigger the stops by selling into the resting buys
            let start = std::time::Instant::now();
            for i in 0..iters {
                let trigger_price = BASE_PRICE + ((i as i64) % 100 + 1) * TICK;
                let sell = Order::new_limit(
                    id,
                    SymbolId(0),
                    Side::Sell,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(trigger_price),
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(sell));
                id += 1;
            }
            start.elapsed()
        });
    });
}

// 16. Deep book matching (1000 price levels)

fn bench_deep_book_match(c: &mut Criterion) {
    c.bench_function("e2e_deep_book_match_1000_levels", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            let levels: u64 = 1000;
            let mut id: u64 = 1;

            // Initial population: 1000 ask levels
            for lvl in 0..levels {
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

            // Measure ONLY sweep time, replenish untimed
            let mut total = std::time::Duration::ZERO;
            for _ in 0..iters {
                let start = std::time::Instant::now();
                let sweep = Order::new_market(
                    id,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedQty::from_raw(ONE_BTC * levels),
                    0,
                );
                engine.process(Command::PlaceOrder(sweep));
                total += start.elapsed();
                id += 1;

                // Untimed: replenish asks for next iteration
                for lvl in 0..levels {
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
    });
}

// 17. Multi-pair routing

fn bench_multi_pair_routing(c: &mut Criterion) {
    c.bench_function("e2e_multi_pair_routing", |b| {
        b.iter_custom(|iters| {
            let mut router = Router::new(1);

            // Register 5 symbols
            for sym in 0..5u16 {
                let mut config = SymbolConfig::default_btc_usdt();
                config.id = SymbolId(sym);
                config.base = format!("SYM{}", sym);
                router.add_symbol(config);
            }

            // Pre-populate each symbol with asks
            for sym in 0..5u16 {
                for i in 0..100u64 {
                    let ask = Order::new_limit(
                        (sym as u64) * 100_000 + i + 1,
                        SymbolId(sym),
                        Side::Sell,
                        TimeInForce::GTC,
                        FixedPrice::from_raw(BASE_PRICE),
                        FixedQty::from_raw(ONE_BTC),
                        0,
                    );
                    router.process(Command::PlaceOrder(ask));
                }
            }

            let start = std::time::Instant::now();
            for i in 0..iters {
                let sym = (i % 5) as u16;
                let order = Order::new_limit(
                    500_000 + i,
                    SymbolId(sym),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE - TICK), // no match, just route + place
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                router.process(Command::PlaceOrder(order));
            }
            start.elapsed()
        });
    });
}

// 18. Full order lifecycle: place -> partial fill -> amend -> cancel

fn bench_full_lifecycle(c: &mut Criterion) {
    c.bench_function("e2e_full_order_lifecycle", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            let mut id: u64 = 1;

            // Pre-populate some asks so we can partial-fill buys
            for _i in 0..iters {
                let ask = Order::new_limit(
                    id,
                    SymbolId(0),
                    Side::Sell,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE),
                    FixedQty::from_raw(ONE_BTC / 2), // 0.5 BTC each
                    0,
                );
                engine.process(Command::PlaceOrder(ask));
                id += 1;
            }

            let start = std::time::Instant::now();
            for _ in 0..iters {
                // Step 1: Place a buy for 1 BTC at BASE_PRICE (partial fill: 0.5 filled, 0.5 resting)
                let buy_id = id;
                let buy = Order::new_limit(
                    buy_id,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE),
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(buy));
                id += 1;

                // Step 2: Amend remaining qty down
                engine.process(Command::AmendOrder {
                    order_id: buy_id,
                    symbol: SymbolId(0),
                    new_price: None,
                    new_qty: Some(FixedQty::from_raw(ONE_BTC / 4)),
                    timestamp: 1,
                });

                // Step 3: Cancel the order
                engine.process(Command::CancelOrder {
                    order_id: buy_id,
                    symbol: SymbolId(0),
                    timestamp: 2,
                });
            }
            start.elapsed()
        });
    });
}

// 19. Stop-market order placement

fn bench_stop_market_placement(c: &mut Criterion) {
    c.bench_function("e2e_stop_market_placement", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();

            let start = std::time::Instant::now();
            for i in 0..iters {
                let order = Order::new_stop_market(
                    i + 1,
                    SymbolId(0),
                    Side::Sell,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE - 5 * TICK), // stop trigger below
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(order));
            }
            start.elapsed()
        });
    });
}

// 20. Amend qty UP (cancel+replace path, loses time priority)

fn bench_amend_qty_up(c: &mut Criterion) {
    c.bench_function("e2e_amend_qty_up", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            let count = 1000u64;
            for i in 0..count {
                let order = Order::new_limit(
                    i + 1,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE - (i as i64 + 100) * TICK),
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(order));
            }

            let start = std::time::Instant::now();
            for i in 0..iters {
                let target_id = (i % count) + 1;
                // Vary qty upward to always trigger cancel+replace path
                let new_qty = ONE_BTC * 2 + (i % 8) * ONE_BTC;
                engine.process(Command::AmendOrder {
                    order_id: target_id,
                    symbol: SymbolId(0),
                    new_price: None,
                    new_qty: Some(FixedQty::from_raw(new_qty)),
                    timestamp: i,
                });
            }
            start.elapsed()
        });
    });
}

// 21. Amend triggers match (price change crosses spread)

fn bench_amend_triggers_match(c: &mut Criterion) {
    c.bench_function("e2e_amend_triggers_match", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            let mut id = 1u64;

            // For each iteration: place an ask + a low bid
            for _ in 0..iters {
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

                let bid = Order::new_limit(
                    id,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE - 100 * TICK), // won't cross
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(bid));
                id += 1;
            }

            // Amend each bid's price UP to cross the spread → triggers match
            let start = std::time::Instant::now();
            for i in 0..iters {
                let bid_id = (i * 2) + 2; // bid IDs: 2, 4, 6, ...
                engine.process(Command::AmendOrder {
                    order_id: bid_id,
                    symbol: SymbolId(0),
                    new_price: Some(FixedPrice::from_raw(BASE_PRICE)),
                    new_qty: None,
                    timestamp: i,
                });
            }
            start.elapsed()
        });
    });
}

// 22. Market order on empty book (rejection fast-path)

fn bench_market_order_empty_book(c: &mut Criterion) {
    c.bench_function("e2e_market_order_empty_book", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();

            let start = std::time::Instant::now();
            for i in 0..iters {
                let buy = Order::new_market(
                    i + 1,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(buy));
            }
            start.elapsed()
        });
    });
}

// 23. Trading halt rejection (circuit breaker)

fn bench_trading_halt_rejection(c: &mut Criterion) {
    c.bench_function("e2e_trading_halt_rejection", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            engine.halt_trading();

            let start = std::time::Instant::now();
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
            start.elapsed()
        });
    });
}

// 24. Deep queue at single price level (10k orders FIFO)

fn bench_deep_queue_single_level(c: &mut Criterion) {
    c.bench_function("e2e_deep_queue_single_level", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            let depth = 10_000u64;
            let mut id: u64 = 1;

            // 10,000 asks all at the same price
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

            // Measure sweep only, replenish untimed
            let mut total = std::time::Duration::ZERO;
            for _ in 0..iters {
                let start = std::time::Instant::now();
                let sweep = Order::new_market(
                    id,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedQty::from_raw(ONE_BTC * depth),
                    0,
                );
                engine.process(Command::PlaceOrder(sweep));
                total += start.elapsed();
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
    });
}

// 25. Cancel non-existent order (OrderNotFound fast-path)

fn bench_cancel_nonexistent(c: &mut Criterion) {
    c.bench_function("e2e_cancel_nonexistent", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            // Populate some orders so the hash map isn't empty
            for i in 0..100u64 {
                let order = Order::new_limit(
                    i + 1,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE - (i as i64) * TICK),
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(order));
            }

            let start = std::time::Instant::now();
            for i in 0..iters {
                engine.process(Command::CancelOrder {
                    order_id: 1_000_000 + i, // IDs that don't exist
                    symbol: SymbolId(0),
                    timestamp: 0,
                });
            }
            start.elapsed()
        });
    });
}

// 26. HFT cancel-replace pattern (cancel + place on every tick)

fn bench_hft_cancel_replace(c: &mut Criterion) {
    c.bench_function("e2e_hft_cancel_replace", |b| {
        b.iter_custom(|iters| {
            let mut engine = fresh_engine();
            // Seed ask side for realistic spread
            for i in 0..100u64 {
                let ask = Order::new_limit(
                    i + 1,
                    SymbolId(0),
                    Side::Sell,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE + (i as i64 + 1) * TICK),
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(ask));
            }

            // Place initial bid
            let mut bid_id = 10_000u64;
            let initial = Order::new_limit(
                bid_id,
                SymbolId(0),
                Side::Buy,
                TimeInForce::GTC,
                FixedPrice::from_raw(BASE_PRICE - TICK),
                FixedQty::from_raw(ONE_BTC),
                0,
            );
            engine.process(Command::PlaceOrder(initial));

            let start = std::time::Instant::now();
            for i in 0..iters {
                // Cancel current bid
                engine.process(Command::CancelOrder {
                    order_id: bid_id,
                    symbol: SymbolId(0),
                    timestamp: i,
                });
                // Place new bid at slightly different price
                bid_id += 1;
                let price_offset = (i % 10) as i64 + 1;
                let new_bid = Order::new_limit(
                    bid_id,
                    SymbolId(0),
                    Side::Buy,
                    TimeInForce::GTC,
                    FixedPrice::from_raw(BASE_PRICE - price_offset * TICK),
                    FixedQty::from_raw(ONE_BTC),
                    0,
                );
                engine.process(Command::PlaceOrder(new_bid));
            }
            start.elapsed()
        });
    });
}

// Criterion groups

criterion_group!(
    benches,
    bench_limit_no_match,
    bench_limit_full_fill,
    bench_limit_partial_fill,
    bench_market_sweep_single_level,
    bench_market_sweep_multi_level,
    bench_ioc_with_match,
    bench_ioc_no_match,
    bench_fok_success,
    bench_fok_reject,
    bench_cancel_mid_book,
    bench_cancel_best_bid,
    bench_amend_qty_down,
    bench_amend_price_change,
    bench_stop_order_placement,
    bench_stop_order_trigger,
    bench_deep_book_match,
    bench_multi_pair_routing,
    bench_full_lifecycle,
    // New benchmarks
    bench_stop_market_placement,
    bench_amend_qty_up,
    bench_amend_triggers_match,
    bench_market_order_empty_book,
    bench_trading_halt_rejection,
    bench_deep_queue_single_level,
    bench_cancel_nonexistent,
    bench_hft_cancel_replace,
);
criterion_main!(benches);
