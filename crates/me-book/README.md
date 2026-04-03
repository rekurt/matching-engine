# me-book — Order Book & Matching Algorithm

Price-time priority order book with LIMIT, MARKET, STOP-LIMIT, and STOP-MARKET order support.

| Feature | Details |
|---------|---------|
| Order types | LIMIT, MARKET, STOP-LIMIT, STOP-MARKET |
| Time in force | GTC, IOC, FOK |
| Data structures | BTreeMap price levels + Slab allocator + intrusive FIFO list |
| Cancel/amend | O(1) via FxHashMap<OrderId, OrderLocation> |
| Stop orders | Separate StopBook; cascading triggers bounded to 100 iterations |

## Quick example

```rust
use me_book::book::OrderBook;
use me_book::matching;
use me_core::symbol::SymbolConfig;
use me_core::types::{Order, Side, OrderType, TimeInForce, FixedPrice, FixedQty, OrderId};
use me_core::events::EventBuffer;

let config = SymbolConfig::default(); // BTC/USDT defaults
let mut book = OrderBook::new(config);
let mut events = EventBuffer::with_capacity(64);

let buy = Order {
    id: OrderId(1),
    side: Side::Buy,
    order_type: OrderType::Limit,
    time_in_force: TimeInForce::Gtc,
    price: Some(FixedPrice(50_000_00)), // $50 000.00
    qty: FixedQty(1_0000_0000),         // 1 BTC
    ..Order::default()
};

matching::process_order(&mut book, buy, &mut events);
// events contains OrderAccepted (resting on bid side)
```

## Architecture

```
OrderBook
 ├── bids: BTreeMap<FixedPrice, PriceLevel>   — highest first
 ├── asks: BTreeMap<FixedPrice, PriceLevel>   — lowest first
 ├── nodes: Slab<OrderNode>                   — O(1) alloc/dealloc
 └── index: FxHashMap<OrderId, OrderLocation> — O(1) cancel lookup

StopBook (separate)
 ├── stop_bids: BTreeMap<FixedPrice, Vec<Order>>
 └── stop_asks: BTreeMap<FixedPrice, Vec<Order>>
```

## Matching rules

- **FOK** — checks full fill availability before touching the book; rejects entirely if insufficient liquidity
- **IOC** — fills what's available, cancels the remainder immediately
- **Market** — sweeps the opposite side at any price; rejected if book is empty
- **Stop** — rests in StopBook until last trade price crosses stop price, then inserts as LIMIT or MARKET
