# me-engine — Matching Engine Orchestrator

One-engine-per-symbol orchestrator with lock-free single-thread-per-symbol guarantee and multi-shard routing.

| Feature | Details |
|---------|---------|
| Concurrency model | One thread per symbol — no locks, deterministic replay |
| Routing | `Router` (single-thread) or `ShardedRouterV2` (multi-thread, N shards) |
| Circuit breaker | `TradingState::Halted` — cancels allowed, new orders rejected |
| Stop cascade | Bounded to 100 iterations to prevent infinite loops |
| Command types | PlaceOrder, CancelOrder, AmendOrder, Halt/Resume |

## Quick example

```rust
use me_engine::engine::Engine;
use me_engine::command::Command;
use me_core::symbol::SymbolConfig;
use me_core::types::{Order, Side, OrderType, TimeInForce, FixedPrice, FixedQty, OrderId};

let config = SymbolConfig::default();
let mut engine = Engine::new(config);

let order = Order {
    id: OrderId(1),
    side: Side::Buy,
    order_type: OrderType::Limit,
    time_in_force: TimeInForce::Gtc,
    price: Some(FixedPrice(50_000_00)),
    qty: FixedQty(1_0000_0000),
    ..Order::default()
};

let events = engine.process(Command::PlaceOrder(order));
// events: [OrderAccepted { order_id: 1, .. }]
```

## Command flow

```
Client → TCP Frame → Codec → Command
                               │
                          Router/ShardedRouterV2
                               │
                     Engine (per symbol, single thread)
                               │
                     MatchEvents → Codec → TCP Frame → Client
```

## Sharded routing

`ShardedRouterV2` distributes symbols across N worker threads using `symbol_id % num_shards`.
Each shard owns a bounded crossbeam channel (capacity 4096). Set `shards = 0` in config for auto-detection (one shard per CPU core).

```toml
[server]
shards = 4  # 0 = auto (number of CPU cores)
```

## Configuration example

```toml
[[symbols]]
id     = 1
base   = "BTC"
quote  = "USDT"
price_scale = 2   # $50000.00 stored as 5000000
qty_scale   = 8   # 1 BTC stored as 100000000
```
