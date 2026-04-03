# Architecture: HFT Crypto Spot Matching Engine

Version 0.1.0 | Rust 2021 Edition

## Table of Contents

1. [Overview](#overview)
2. [Crate Dependency Graph](#crate-dependency-graph)
3. [Core Data Types](#core-data-types)
4. [Order Book Structure](#order-book-structure)
5. [Matching Algorithm](#matching-algorithm)
6. [Engine and Command Dispatch](#engine-and-command-dispatch)
7. [Threading Model](#threading-model)
8. [Wire Protocol](#wire-protocol)
9. [Network I/O Layer](#network-io-layer)
10. [Persistence Layer](#persistence-layer)
11. [Risk Management](#risk-management)
12. [Circuit Breaker](#circuit-breaker)
13. [Observability](#observability)
14. [Audit Trail](#audit-trail)
15. [Zero-Allocation Design](#zero-allocation-design)
16. [Performance Characteristics](#performance-characteristics)
17. [Data Flow](#data-flow)
18. [Recovery Procedure](#recovery-procedure)

---

## Overview

This is a high-frequency trading (HFT) cryptocurrency spot matching engine written
in Rust. It implements price-time priority matching with support for limit, market,
stop-limit, and stop-market orders. The engine is designed around a shared-nothing
architecture where each trading pair is owned by exactly one thread, eliminating all
lock contention in the matching hot path.

```
+-------------------------------------------------------------------+
|                         me-server                                  |
|  CLI (clap) | TOML config | REST admin (Axum :9091) | signals     |
+-------------------------------------------------------------------+
        |               |                |
        v               v                v
+-------------+  +-------------+  +---------------+
|   me-io     |  | me-metrics  |  |   me-risk     |
| TCP + UDS   |  | Prometheus  |  | Rate limits   |
| Binary wire |  | 50+ metrics |  | Price bands   |
+-------------+  +-------------+  +---------------+
        |                                |
        v                                v
+---------------------------------------------------+
|                    me-engine                       |
|  Engine (per-pair) | Router | ShardedRouterV2      |
+---------------------------------------------------+
        |
        v
+---------------------------------------------------+
|                    me-book                         |
|  OrderBook | PriceLevel | StopBook | Slab | Pool  |
+---------------------------------------------------+
        |
        v
+---------------------------------------------------+
|                    me-core                         |
|  FixedPrice | FixedQty | Order | Trade | Events   |
+---------------------------------------------------+
        |
        v
+---------------------------------------------------+
|                   me-persist                       |
|  WAL (CRC32) | Snapshots (atomic rename)          |
+---------------------------------------------------+
```

---

## Crate Dependency Graph

```
me-server ──> me-engine ──> me-book ──> me-core
    |             |             |
    |             |             +------> me-core
    |             |
    +-----------> me-io ──> me-engine ──> me-book ──> me-core
    |
    +-----------> me-metrics ──> me-book ──> me-core
    |                        |
    |                        +-> me-core
    |
    +-----------> me-risk ──> me-core
    |
    +-----------> me-audit ──> me-core

me-persist ──> me-core    (codec for WAL/snapshot serialization)
me-bench   ──> me-engine, me-book, me-core
me-datagen ──> me-core    (order flow generation)
```

**Summary of each crate:**

| Crate | Purpose |
|---|---|
| `me-core` | Foundational types: `FixedPrice(i64)`, `FixedQty(u64)`, `Order`, `Trade`, `SymbolConfig`, `MatchEvent`, error types. Scale factor = 10^8. |
| `me-book` | Order book data structure: BTreeMap price levels, slab allocator, intrusive linked lists, stop book. |
| `me-engine` | Per-pair `Engine` (command dispatch, validation, stop cascade). `Router` (single-thread). `ShardedRouterV2` (multi-thread). |
| `me-persist` | Write-ahead log and snapshot persistence with CRC32 integrity checks. |
| `me-io` | Binary wire protocol (14-byte header), frame codec, async TCP and UDS servers (tokio). |
| `me-server` | Binary entry point, CLI (clap), TOML config, REST admin API (Axum on port 9091), signal handling. |
| `me-metrics` | 50+ Prometheus metrics: orders, trades, book state, connections, WAL, shard queue depth. |
| `me-bench` | Criterion benchmarks: throughput, latency, e2e matching, stress testing (18 benchmarks). |
| `me-datagen` | Realistic order flow generator with configurable distribution parameters. |
| `me-risk` | Pre-trade risk management: token-bucket rate limiting, fat-finger checks, price bands, position limits. |
| `me-audit` | Structured JSON audit events for all matching operations, emitted via the `tracing` framework. |

---

## Core Data Types

### Fixed-Point Arithmetic

All prices and quantities use fixed-point integer representation with a scale
factor of 10^8 (100,000,000). This eliminates all floating-point arithmetic
from the matching hot path.

```
FixedPrice(i64)   -- signed, supports comparison arithmetic
                   -- Range: +/-92,233,720,368.54775807
                   -- Example: $100.50 = FixedPrice(10_050_000_000)

FixedQty(u64)     -- unsigned, quantities are never negative
                   -- Range: 0 .. 184,467,440,737.09551615
                   -- Example: 1.5 BTC = FixedQty(150_000_000)
```

Both types are `#[repr(transparent)]` wrappers around their inner integer,
meaning they have zero overhead compared to raw integer operations. All
arithmetic operators (`Add`, `Sub`, `AddAssign`, `SubAssign`) are implemented
with `#[inline]` and saturating variants are provided for safety.

### Order Structure

```rust
#[repr(C)]
pub struct Order {             // 72 bytes, cache-line friendly
    pub id: OrderId,           // u64
    pub price: FixedPrice,     // i64
    pub stop_price: FixedPrice,// i64
    pub qty: FixedQty,         // u64
    pub remaining_qty: FixedQty, // u64
    pub cumulative_qty: FixedQty, // u64
    pub timestamp: u64,        // nanosecond epoch
    pub sequence: SequenceId,  // u64  (book-local monotonic)
    pub symbol: SymbolId,      // u16
    pub side: Side,            // u8   (Buy=0, Sell=1)
    pub order_type: OrderType, // u8   (Limit=0, Market=1, StopLimit=2, StopMarket=3)
    pub tif: TimeInForce,      // u8   (GTC=0, IOC=1, FOK=2)
    pub status: OrderStatus,   // u8
    pub _pad: [u8; 2],         // alignment padding
}
```

Field ordering is chosen to minimize padding. The `#[repr(C)]` layout makes
the structure suitable for zero-copy network serialization and memory-mapped
access patterns.

### Trade Structure

```rust
#[repr(C)]
pub struct Trade {
    pub trade_id: TradeId,         // u64
    pub symbol: SymbolId,          // u16
    pub price: FixedPrice,         // i64
    pub qty: FixedQty,             // u64
    pub maker_order_id: OrderId,   // u64
    pub taker_order_id: OrderId,   // u64
    pub maker_side: Side,          // u8
    pub timestamp: u64,            // nanosecond epoch
    pub sequence: SequenceId,      // u64
}
```

### SymbolConfig

Each trading pair is configured with:

- `price_scale` / `qty_scale` -- decimal precision
- `min_qty` / `max_qty` -- quantity bounds
- `qty_step` (lot size) -- quantity must be a multiple
- `price_step` (tick size) -- price must be a multiple

Validation is performed at order submission time: tick size and lot size
compliance are checked before an order enters the matching pipeline.

---

## Order Book Structure

```
                         OrderBook
    +----------------------------------------------------+
    |  symbol: SymbolId                                  |
    |  config: SymbolConfig                              |
    |  sequence: u64          (monotonic counter)        |
    |  trade_id_gen: u64      (trade ID generator)       |
    |  best_bid: Option<FixedPrice>  (cached)            |
    |  best_ask: Option<FixedPrice>  (cached)            |
    +----------------------------------------------------+
    |                                                    |
    |  bids: BTreeMap<Reverse<FixedPrice>, PriceLevel>   |
    |        (descending: first() = highest = best bid)  |
    |                                                    |
    |  asks: BTreeMap<FixedPrice, PriceLevel>             |
    |        (ascending: first() = lowest = best ask)    |
    |                                                    |
    |  order_index: FxHashMap<OrderId, OrderLocation>     |
    |        (O(1) lookup for cancel/amend)              |
    |                                                    |
    |  order_slab: Slab<OrderNode>                        |
    |        (shared pool for all order nodes)           |
    +----------------------------------------------------+

          Bid Side (descending)          Ask Side (ascending)
    +---------+---------+------+   +---------+---------+------+
    | $100.02 | $100.01 |$99.98|   | $100.05 | $100.06 |$100.1|
    +---------+---------+------+   +---------+---------+------+
      best bid                       best ask
```

### PriceLevel (Intrusive Doubly-Linked List)

Each price level is a FIFO queue implemented as an intrusive doubly-linked list
backed by the shared slab allocator. This achieves O(1) insert and O(1) remove
for any order at any position.

```
PriceLevel {
    price: FixedPrice,
    total_qty: FixedQty,    // aggregate quantity at this level
    order_count: u32,
    head --> [OrderNode] <-> [OrderNode] <-> [OrderNode] <-- tail
}

OrderNode {
    order: Order,           // the full order data
    prev: Option<SlabKey>,  // intrusive link (backward)
    next: Option<SlabKey>,  // intrusive link (forward)
}
```

### OrderLocation (O(1) Cancel/Amend Index)

```
order_index: FxHashMap<OrderId, OrderLocation>

OrderLocation {
    price: FixedPrice,   // which price level
    side: Side,          // bid or ask
    slab_key: SlabKey,   // direct slab index for O(1) node access
}
```

Cancellation path: `OrderId -> FxHashMap lookup -> OrderLocation -> Slab[key]
-> unlink from doubly-linked list -> remove from BTreeMap if level empty`.
All steps are O(1) except the BTreeMap removal which is O(log N) in the number
of price levels (typically small).

### Slab Allocator

```
Slab<T> {
    entries: Vec<Entry<T>>,      // pre-allocated
    free_head: Option<SlabKey>,  // intrusive free list
    len: usize,
}

enum Entry<T> {
    Occupied(T),
    Vacant { next_free: Option<SlabKey> },
}
```

The slab allocator is a custom implementation that provides:

- **O(1) allocation**: reuses free slots via an intrusive free list, falls
  back to `Vec::push` only when all slots are occupied.
- **O(1) deallocation**: marks slot as vacant and prepends to free list.
- **Zero syscalls in steady state**: once the slab grows to its high-water
  mark, no further heap allocations occur. The slab key type is `u32`,
  supporting up to approximately 4 billion entries per book.
- **Stable indices**: slab keys remain valid across insertions/removals of
  other entries, which is essential for the intrusive linked list pointers.

Initial capacity: 4,096 entries per order book.

### StopBook

Stop orders are stored separately from the active order book:

```
StopBook {
    stop_buys:  BTreeMap<FixedPrice, Vec<Order>>           // ascending
    stop_sells: BTreeMap<Reverse<FixedPrice>, Vec<Order>>  // descending
    stop_index: FxHashMap<OrderId, StopLocation>           // O(1) cancel
}
```

- Stop-buy orders trigger when market price rises to or above `stop_price`.
- Stop-sell orders trigger when market price falls to or below `stop_price`.

The BTreeMap ordering ensures that trigger checking scans from the most likely
to trigger first and can break early when no more triggers are possible.

---

## Matching Algorithm

The engine uses **price-time priority** (also called FIFO matching):

1. Orders at the best price match first.
2. Within the same price level, the oldest order (earliest timestamp/sequence)
   matches first.

### Limit Order Matching

**Buy limit** -- iterates asks from best (lowest) price upward:

```
for each ask level where ask_price <= limit_price:
    for each order in level (FIFO, head to tail):
        fill_qty = min(taker.remaining, maker.remaining)
        execute trade at ask_price
        if taker fully filled: stop
    if level empty: remove level
    if taker fully filled: stop
```

**Sell limit** -- iterates bids from best (highest) price downward:

```
for each bid level where bid_price >= limit_price:
    for each order in level (FIFO, head to tail):
        fill_qty = min(taker.remaining, maker.remaining)
        execute trade at bid_price
        if taker fully filled: stop
    if level empty: remove level
    if taker fully filled: stop
```

### Time-in-Force Behavior

| TIF | Before Matching | After Matching |
|-----|-----------------|----------------|
| **GTC** | -- | Remainder rests on book |
| **IOC** | -- | Remainder cancelled immediately |
| **FOK** | Pre-check: total available qty >= order qty, reject if insufficient | Must be fully filled (checked before execution) |

FOK pre-checks available liquidity *before* executing any fills. This
guarantees atomicity: either the entire order fills or nothing happens.

### Market Order Matching

Market orders sweep the entire opposite side with no price limit:

- **Buy market**: iterates all ask levels from lowest to highest.
- **Sell market**: iterates all bid levels from highest to lowest.
- Market orders **never rest** on the book. Any unfilled remainder is cancelled.

### Cancel and Amend

**Cancel**: O(1) lookup via `order_index`, O(1) unlink from intrusive linked list.

**Amend** follows two paths:
- **Qty decrease only** (no price change): modify in-place, **retain time
  priority**. The order stays at its current position in the FIFO queue.
- **Price change or qty increase**: cancel the existing order and re-insert
  with new parameters. This **loses time priority** and the new order may
  immediately match if the amended price crosses the spread.

---

## Engine and Command Dispatch

### Command Types

```rust
enum Command {
    PlaceOrder(Order),
    CancelOrder { order_id, symbol, timestamp },
    AmendOrder  { order_id, symbol, new_price, new_qty, timestamp },
}
```

### Engine (Per-Pair)

Each `Engine` instance owns exactly one `OrderBook` and one `StopBook`. The
fundamental invariant: **all operations within one Engine are strictly
sequential. One Engine = one OrderBook = one thread. No locks, no shared
mutable state.**

```
Engine {
    book: OrderBook,
    stop_book: StopBook,
    events: EventBuffer,       // pre-allocated, reused across commands
    trading_state: TradingState,  // Active | Halted
}
```

Command processing flow:

```
Engine::process(cmd) -> &[MatchEvent]
    |
    +-- events.clear()        // reuse buffer, no allocation
    |
    +-- match cmd:
    |     PlaceOrder(order)
    |       |-- if halted: reject
    |       |-- if stop order: insert into stop_book
    |       |-- else: matching::process_order(book, order, events)
    |       +-- check_stop_triggers()
    |
    |     CancelOrder { order_id }
    |       |-- if in book: matching::cancel_order()
    |       |-- if in stop_book: stop_book.cancel()
    |       +-- else: reject (OrderNotFound)
    |
    |     AmendOrder { order_id, new_price, new_qty }
    |       |-- if halted: reject
    |       |-- matching::amend_order()
    |       +-- check_stop_triggers()
    |
    +-- return events.events()
```

### Stop Order Cascade

After every order placement or amendment that generates trades, the engine
checks for triggered stop orders:

```
check_stop_triggers():
    last_price = find last trade price in events buffer
    cascade_count = 0

    loop:
        if cascade_count >= 100: break   // safety bound
        triggered = stop_book.check_triggers(last_price)
        if triggered.is_empty(): break

        cascade_count += 1
        for order in triggered:
            order.convert_stop()          // StopLimit -> Limit, StopMarket -> Market
            matching::process_order(book, order, events)

        new_price = find last trade price
        if new_price == last_price: break // no new trades, no more triggers
        last_price = new_price
```

The cascade is bounded to **100 iterations** to prevent infinite loops where
stop triggers create trades that trigger more stops.

### EventBuffer

```rust
pub struct EventBuffer {
    events: Vec<MatchEvent>,   // pre-allocated, capacity = 256
}
```

The `EventBuffer` is cleared (not deallocated) at the start of each command.
The underlying `Vec` grows to its high-water mark and stays there, meaning
subsequent commands reuse the same memory. This eliminates per-event heap
allocation in the steady state.

---

## Threading Model

```
                    Async I/O (tokio runtime)
    +-----------------------------------------------+
    |  TCP Listener         UDS Listener            |
    |    |                    |                      |
    |    v                    v                      |
    |  [Task per conn]   [Task per conn]            |
    |    |                    |                      |
    |    +-----> decode frame + encode response      |
    +-----------------------------------------------+
                        |
               (synchronous RPC)
                        |
                        v
    +-----------------------------------------------+
    |            ShardedRouterV2                     |
    |                                               |
    |  symbol_to_shard: FxHashMap<SymbolId, usize>  |
    |  Routing: symbol_id % num_shards              |
    |                                               |
    |  Shard 0        Shard 1        Shard N-1      |
    |  +----------+  +----------+  +----------+    |
    |  | crossbeam|  | crossbeam|  | crossbeam|    |
    |  | bounded  |  | bounded  |  | bounded  |    |
    |  | chan 4096|  | chan 4096|  | chan 4096|    |
    |  +-----+----+  +-----+----+  +-----+----+    |
    +--------|-------------|-------------|----------+
             |             |             |
             v             v             v
    +-----------+  +-----------+  +-----------+
    |  Worker   |  |  Worker   |  |  Worker   |
    |  Thread 0 |  |  Thread 1 |  |  Thread N |
    |           |  |           |  |           |
    | Engine A  |  | Engine C  |  | Engine E  |
    | Engine B  |  | Engine D  |  | Engine F  |
    +-----------+  +-----------+  +-----------+

    Each worker thread:
    - Owns a FxHashMap<SymbolId, Engine>
    - Runs a tight recv() loop on its crossbeam channel
    - Processes commands synchronously
    - Sends reply via oneshot channel
    - ZERO locks, ZERO contention within the worker
```

### Synchronous RPC Pattern

Communication between the async I/O layer and the synchronous matching
engine uses a request/reply pattern:

```
Async task (tokio):
    1. Decode binary frame into Command
    2. Create crossbeam oneshot channel (reply_tx, reply_rx)
    3. Send CommandEnvelope { cmd, reply: reply_tx } to shard channel
    4. reply_rx.recv()  -- blocks the tokio task (acceptable: matching is fast)
    5. Encode response events into binary frames
    6. Write to socket

Worker thread:
    1. rx.recv()  -- blocks on crossbeam channel
    2. Route to Engine by SymbolId
    3. engine.process(cmd)  -- synchronous, no allocation
    4. reply.send(events)   -- wake up the async task
```

The `bounded(4096)` channel capacity provides backpressure: if a shard is
overloaded, senders block until the shard consumes messages.

### Shard Assignment

Symbols are assigned to shards using simple modular hashing:

```
shard_index = symbol_id.0 % num_shards
```

If `num_shards` is 0 (default), the system uses `std::thread::available_parallelism()`
to auto-detect the number of physical CPU cores, falling back to 4.

### Shard Message Types

The shard channel carries an enum with five variants:

```
enum ShardMsg {
    Command { cmd, reply },   // order/cancel/amend + response channel
    AddSymbol(SymbolConfig),  // register a new trading pair
    HaltSymbol(SymbolId),     // circuit breaker: halt
    ResumeSymbol(SymbolId),   // circuit breaker: resume
    Shutdown,                 // graceful termination
}
```

Administrative messages (halt, resume, add symbol) are sent through the same
channel as trading commands, ensuring they are processed in order relative to
commands for the same shard.

---

## Wire Protocol

### Frame Format

```
+----------+----------+----------+----------+-----------------+
| len: u32 | ver: u8  | type: u8 | seq: u64 | payload: [u8]   |
| 4 bytes  | 1 byte   | 1 byte   | 8 bytes  | variable        |
+----------+----------+----------+----------+-----------------+
      \_______________________________________________/
                   14-byte header

len:  payload size only (does NOT include the 14-byte header)
ver:  protocol version (currently 1)
type: message type enum (see below)
seq:  monotonically increasing sequence number per connection
```

### Message Types

| Code | Direction | Type | Payload Size |
|------|-----------|------|-------------|
| `0x01` | Client -> Engine | PlaceOrder | 37 bytes |
| `0x02` | Client -> Engine | CancelOrder | 10 bytes |
| `0x03` | Client -> Engine | AmendOrder | 12-28 bytes (variable) |
| `0x04` | Bidirectional | Ping | 0 bytes |
| `0x05` | Client -> Engine | Handshake | 0-1 bytes |
| `0x10` | Engine -> Client | OrderAccepted | 16 bytes |
| `0x11` | Engine -> Client | OrderRejected | 17 bytes |
| `0x12` | Engine -> Client | Trade | 51 bytes |
| `0x13` | Engine -> Client | OrderFilled | 32 bytes |
| `0x14` | Engine -> Client | OrderPartiallyFilled | 40 bytes |
| `0x15` | Engine -> Client | OrderCancelled | 24 bytes |
| `0x16` | Engine -> Client | OrderAmended | 32 bytes |
| `0x17` | Engine -> Client | StopTriggered | 24 bytes |
| `0x18` | Engine -> Client | Pong | 0 bytes |
| `0x19` | Engine -> Client | Error | variable |
| `0x1A` | Engine -> Client | HandshakeAck | 1 byte |

### PlaceOrder Payload (37 bytes)

```
+----------+-----------+------+-----------+-----+-------+-----------+------+
| OrderId  | SymbolId  | Side | OrderType | TIF | Price | StopPrice | Qty  |
| 8 bytes  | 2 bytes   | 1b   | 1 byte    | 1b  | 8b    | 8 bytes   | 8b   |
+----------+-----------+------+-----------+-----+-------+-----------+------+
   u64 LE     u16 LE    enum    enum       enum  i64 LE   i64 LE    u64 LE
```

### Connection State Machine

```
    HandshakePending ---[Handshake msg]---> Active ---[Shutdown]---> Closing
         |                                    |
         +-- any command msg ---> Error: HandshakeRequired
```

### Protocol Safety

- **Max payload size**: 1024 bytes (prevents memory exhaustion).
- **Sequence validation**: strictly increasing per connection, out-of-order
  frames cause disconnect.
- **Backpressure**: write buffer limit of 256 KB; slow consumers are
  disconnected with a Backpressure error frame.
- **Version check**: handshake must carry the correct protocol version.

---

## Network I/O Layer

### TCP Server

Built on tokio async runtime. Each accepted connection spawns a dedicated
tokio task:

```
TcpServer::listen(addr, shutdown_rx)
    loop {
        select! {
            biased;  // prioritize shutdown signal
            shutdown_rx.changed() => break
            listener.accept() => spawn handle_connection(stream)
        }
    }

handle_connection(stream):
    read_buf  = BytesMut::with_capacity(4096)
    write_buf = BytesMut::with_capacity(4096)
    conn_state = ConnectionCodecState::new()

    loop {
        select! {
            biased;
            shutdown => send Error(ServerShuttingDown), return
            stream.read_buf() => {
                loop {  // drain all complete frames
                    decode_frame(read_buf, conn_state)
                    match frame:
                        Handshake => encode_handshake_ack()
                        Ping      => encode_pong()
                        Command   => router.process(cmd) -> encode_events()
                    check backpressure (write_buf > 256KB => disconnect)
                    stream.write_all(write_buf)
                }
            }
        }
    }
```

### UDS Server

Identical architecture to TCP but listens on a Unix domain socket. UDS
provides lower latency for co-located clients by avoiding the TCP/IP stack.

### Graceful Shutdown

A `tokio::sync::watch` channel propagates the shutdown signal:

1. SIGTERM or SIGINT is caught by a dedicated signal handler task.
2. The handler sends `true` on the watch channel.
3. All listeners and connection tasks observe the signal via `select!` with
   `biased` priority for the shutdown branch.
4. Connections receive a `ServerShuttingDown` error frame before closing.

---

## Persistence Layer

### Write-Ahead Log (WAL)

Every command is logged to the WAL before being applied, ensuring durability.

```
WAL Entry Format:
+----------+----------+----------+--------------------+
| len: u32 | crc: u32 | seq: u64 | command_bytes: [u8] |
| 4 bytes  | 4 bytes  | 8 bytes  | variable            |
+----------+----------+----------+--------------------+

len:  size of command_bytes only
crc:  CRC32 checksum of command_bytes (crc32fast)
seq:  monotonically increasing WAL sequence number
```

**WalWriter**:
- Append-only file using `BufWriter<File>`.
- Configurable sync interval: flush to OS after every N writes.
- CRC32 computed per entry for corruption detection during recovery.

**WalReader**:
- Sequential scan with CRC32 verification per entry.
- Supports reading all entries or starting from a specific sequence.
- CRC mismatch returns an error (truncated/corrupt entries).

### Snapshots

Periodic snapshots capture the full state of an order book, allowing WAL
truncation and fast recovery.

```
Snapshot File Format:
+-------+---------+-----------+----------+-------------+
| Magic | Version | SymbolId  | Sequence | TradeIdGen  |
| u32   | u16     | u16       | u64      | u64         |
| "MESN"| 1       |           |          |             |
+-------+---------+-----------+----------+-------------+
| RestingCount: u32 | [Order; RestingCount]             |
+--------------------+----------------------------------+
| StopCount: u32     | [Order; StopCount]               |
+--------------------+----------------------------------+
| CRC32: u32  (covers everything above)                 |
+-------------------------------------------------------+
```

**Atomic writes**: snapshots are written to a temporary file first, then
atomically renamed to the final path. This guarantees that a crash during
snapshot creation never leaves a corrupt snapshot file.

**Snapshot management**:
- `save()`: serialize and atomically write.
- `load_latest()`: find the most recent snapshot by filename sorting.
- `cleanup()`: retain only the N most recent snapshots per symbol.

---

## Risk Management

The `me-risk` crate provides a pre-trade risk engine that performs all checks
**before** an order reaches the matching engine. All checks are O(1).

### Risk Checks (in order)

1. **Trading halt**: reject if the symbol is halted.
2. **Rate limiting**: token-bucket algorithm per symbol
   (`max_orders_per_second`). Rejects if the bucket is empty.
3. **Fat-finger max quantity**: reject if `order.qty > max_order_qty`.
4. **Max notional value**: for limit orders, reject if
   `price * qty > max_order_notional`.
5. **Price band**: for limit orders, reject if the price deviates more than
   `price_band_bps` basis points from the reference price (last trade price).
6. **Max open orders**: reject if the symbol already has
   `>= max_open_orders` resting on the book.

The risk engine maintains per-symbol state (open order counts, rate limiter
tokens, reference prices) and is updated after each matching cycle.

---

## Circuit Breaker

Each `Engine` has a `TradingState` that can be `Active` or `Halted`.

**When halted**:
- New orders (including stops) are **rejected** with `TradingHalted`.
- Amend requests are **rejected** with `TradingHalted`.
- Cancel requests are **always allowed** (to let participants reduce exposure).

**Control path**:

```
REST Admin API (Axum :9091)
    POST /admin/halt/{symbol_id}     (requires X-Admin-Key header)
    POST /admin/resume/{symbol_id}   (requires X-Admin-Key header)
         |
         v
    ShardedRouterV2::halt_symbol(symbol)
         |
         v
    ShardMsg::HaltSymbol sent through the shard channel
         |
         v
    Worker thread: engine.halt_trading()
```

Note: halt/resume is **eventually consistent**. Commands already queued in the
shard channel may be processed before the halt message arrives.

---

## Observability

### Prometheus Metrics (50+)

Exposed via `GET /metrics` on port 9091.

**Order Processing**:
- `orders_total{side, order_type, tif, symbol}` -- orders received
- `orders_rejected_total{reason, symbol}` -- rejections by reason
- `orders_accepted_total{side, order_type, symbol}` -- accepted orders
- `order_processing_duration_seconds{order_type}` -- latency histogram

**Matching**:
- `trades_total{symbol}` -- trade count
- `trades_volume_total{symbol}` -- cumulative volume
- `trade_price{symbol}` -- last trade price
- `fills_total{fill_type}` -- full vs. partial fills

**Order Book State**:
- `book_bid_levels{symbol}` / `book_ask_levels{symbol}` -- level counts
- `book_bid_orders{symbol}` / `book_ask_orders{symbol}` -- order counts
- `book_spread{symbol}` -- current bid-ask spread
- `book_best_bid{symbol}` / `book_best_ask{symbol}` -- top-of-book prices
- `book_depth_bid_qty{symbol}` / `book_depth_ask_qty{symbol}` -- total depth

**Cancel/Amend**:
- `cancels_total{symbol}` / `cancels_failed_total{symbol}`
- `amends_total{amend_type, symbol}`

**Stop Orders**:
- `stop_orders_pending{symbol}`
- `stop_triggers_total{symbol}`

**System**:
- `commands_total{command_type}` -- total commands by type
- `events_total{event_type}` -- total events emitted
- `active_symbols` -- number of registered trading pairs

**Network**:
- `connections_open{transport}` -- current open connections (TCP/UDS)
- `bytes_in_total{transport}` / `bytes_out_total{transport}`
- `frames_in_total{transport}` / `frames_out_total{transport}`
- `protocol_errors_total{error_type}`

**Shard**:
- `shard_commands_total{shard_id}` -- commands per shard
- `matching_duration_seconds{command_type}` -- matching latency histogram

### Tracing

Structured logging via the `tracing` crate with:
- `EnvFilter` for runtime log level control (`RUST_LOG`).
- Per-shard `info_span!("shard_worker", shard_id)`.
- Per-connection `info_span!("tcp_conn", peer)`.

### Admin API

```
GET  /health              -- "OK" (no auth)
GET  /metrics             -- Prometheus text format (no auth)
GET  /admin/status        -- JSON status (requires X-Admin-Key)
POST /admin/halt/{id}     -- halt trading (requires X-Admin-Key)
POST /admin/resume/{id}   -- resume trading (requires X-Admin-Key)
```

---

## Audit Trail

The `me-audit` crate emits structured JSON events for every matching operation
via the `tracing` framework with `target: "audit"`. In production, these events
would be routed to a dedicated sink (Kafka topic, append-only file, compliance
database).

Audit event types:
- `OrderSubmitted` -- before matching, captures full order parameters
- `OrderAccepted` / `OrderRejected` -- matching results
- `TradeExecuted` -- trade details with maker/taker IDs
- `OrderFilled` / `OrderPartiallyFilled` -- fill events
- `OrderCancelled` / `OrderAmended` -- lifecycle events
- `StopTriggered` -- stop activation events
- `TradingHalted` / `TradingResumed` -- circuit breaker events

All events include nanosecond timestamps and raw fixed-point values for
lossless reconstruction.

---

## Zero-Allocation Design

The matching engine is designed to minimize heap allocations in the hot path.
After warmup, steady-state order processing performs **zero allocations** in
the common case.

### Techniques Used

1. **Slab allocator**: `OrderNode` instances are allocated from a pre-allocated
   slab (`Slab<OrderNode>`, initial capacity 4096). Once the slab reaches its
   high-water mark, inserts and removes reuse existing slots via an intrusive
   free list. No calls to the global allocator.

2. **EventBuffer reuse**: the `EventBuffer` (capacity 256) is cleared at the
   start of each command but never deallocated. The underlying `Vec` retains
   its allocation across commands.

3. **Fixed-size types**: `Order` (72 bytes) and `Trade` are `Copy` types with
   `#[repr(C)]` layout. No heap-allocated fields (strings, Vecs) on the hot
   path.

4. **Pre-allocated read/write buffers**: TCP connection handlers allocate
   `BytesMut::with_capacity(4096)` once per connection. The `bytes` crate
   reuses internal buffers across read/write cycles.

5. **Crossbeam channels**: bounded channels with capacity 4096 pre-allocate
   their internal ring buffers. Message passing does not allocate.

6. **FxHashMap**: the `rustc-hash` crate provides a non-cryptographic hash
   map with lower overhead than the standard `HashMap`. The `order_index`
   map rehashes occasionally but this is amortized.

7. **BTreeMap levels**: price level entries in the BTreeMap are allocated
   individually by the standard library. However, in practice, the number of
   price levels is bounded by the market's tick structure, and BTreeMap nodes
   are reused after removal via the allocator's free list.

### Hot Path (No Allocation)

```
Command arrives on crossbeam channel
  -> Engine::process()
       -> events.clear()          // no alloc: Vec::clear retains capacity
       -> matching::process_order()
            -> validate_order()    // pure arithmetic, no alloc
            -> match_buy_limit()
                 -> slab.get()     // O(1) index lookup
                 -> fill tracking  // Vec<(OrderId, FixedQty, bool)> on stack
                 -> slab.remove()  // O(1), returns to free list
       -> events.push(Trade)       // no alloc if under capacity
  -> reply.send(events)            // crossbeam channel, no alloc
```

The one place where allocation MAY occur in the hot path is the `fills` Vec
inside the level matching function, which collects fill data before applying
mutations. In practice this Vec is small (typically 1-3 fills per level).

---

## Performance Characteristics

### Complexity Table

| Operation | Time Complexity | Notes |
|---|---|---|
| Place limit (no match) | O(log P) | BTreeMap insert at price level |
| Place limit (match, M fills) | O(M + log P) | M fills + possible level removal |
| Place market (M fills across L levels) | O(M + L log P) | Sweep + level removals |
| Cancel order | O(1) amortized | FxHashMap + slab unlink |
| Amend (qty decrease) | O(1) amortized | In-place modification |
| Amend (price change) | O(log P + match cost) | Cancel + re-insert |
| Best bid/ask query | O(1) | Cached in `best_bid`/`best_ask` |
| Order lookup by ID | O(1) amortized | FxHashMap lookup |
| FOK pre-check | O(L) | Scan affected price levels |
| Stop trigger check | O(T + log S) | T triggered orders, S stop price levels |

Where:
- P = number of distinct price levels on one side
- M = number of individual fills in a match
- L = number of price levels consumed
- T = number of triggered stop orders
- S = number of stop price levels

### Latency Profile

The system is optimized for single-digit microsecond matching latency:

- **Fixed-point arithmetic**: all price/quantity math uses integer operations.
  No floating-point conversion anywhere in the matching path.
- **Cache-friendly layout**: `Order` is 72 bytes (`#[repr(C)]`), and
  `OrderNode` instances within a price level are accessed sequentially via
  the intrusive linked list, promoting prefetch.
- **No locks**: each Engine is single-threaded. No mutex acquisition, no
  atomic operations, no CAS loops in the matching hot path.
- **No I/O on the critical path**: WAL writes and metric updates happen
  outside the matching function. The `Engine::process()` method does pure
  computation.

### Throughput

- **Per-shard**: bounded by the speed of the shard worker loop (crossbeam
  recv -> match -> reply). With N shards, the system scales nearly linearly
  for independent symbols.
- **Cross-shard**: no contention between shards. Each shard is a fully
  independent processing pipeline.
- **Backpressure**: the bounded channel (4096 capacity) provides natural
  backpressure. If a shard falls behind, producers block.

---

## Data Flow

### Order Placement (Happy Path)

```
Client                  TCP/UDS Server          ShardedRouterV2        Shard Worker
  |                         |                        |                      |
  |--- PlaceOrder frame --->|                        |                      |
  |                         |-- decode_frame() ----->|                      |
  |                         |                        |-- route by symbol -->|
  |                         |                        |   (crossbeam send)  |
  |                         |                        |                      |
  |                         |                        |                      +-- Engine::process()
  |                         |                        |                      |     |
  |                         |                        |                      |     +-- validate()
  |                         |                        |                      |     +-- match_limit()
  |                         |                        |                      |     +-- check_stops()
  |                         |                        |                      |     |
  |                         |                        |<--- reply events ----|<----+
  |                         |<-- encode_events() ----|                      |
  |<-- response frames -----|                        |                      |
  |                         |                        |                      |
```

### Stop Order Trigger Flow

```
1. Client places a stop-limit buy (stop=100, limit=101)
   -> Engine routes to StopBook::insert()
   -> MatchEvent::OrderAccepted

2. Later, a sell order matches at price 100
   -> Trade generated at price 100
   -> Engine::check_stop_triggers()
   -> StopBook::check_triggers(price=100)
   -> Stop-buy at 100 triggered
   -> MatchEvent::StopTriggered
   -> order.convert_stop()  (StopLimit -> Limit)
   -> matching::process_order() with the converted limit order
   -> May generate more trades -> may trigger more stops (cascade)
   -> Cascade bounded at 100 iterations
```

### Snapshot + WAL Recovery Flow

```
Startup:
  1. Load latest snapshot for each symbol
     -> BookSnapshot { resting_orders, stop_orders, sequence, trade_id_gen }
     -> Rebuild OrderBook by inserting each resting order
     -> Rebuild StopBook by inserting each stop order
     -> Set sequence counter and trade_id_gen

  2. Open WAL, read entries from snapshot.sequence onward
     -> For each WalEntry:
          Convert WalCommand -> engine Command
          Engine::process(cmd)
     -> This replays all commands since the snapshot

  3. Engine is now in the exact state it was before shutdown
```

---

## Recovery Procedure

1. **Find latest snapshot**: `SnapshotManager::load_latest(symbol)` scans the
   snapshot directory for the most recent file matching the symbol.

2. **Restore book state**: insert all resting orders into the `OrderBook` and
   all stop orders into the `StopBook`. Restore the monotonic sequence counter
   and trade ID generator.

3. **Replay WAL**: `WalReader::read_from(snapshot.sequence)` reads all entries
   after the snapshot checkpoint. Each entry is CRC32-verified and replayed
   through the engine.

4. **Truncate WAL** (optional): after a new snapshot is taken, old WAL entries
   before the snapshot sequence can be discarded.

The recovery process is deterministic: given the same snapshot and WAL, the
engine will always reconstruct the same state. This is guaranteed by the
fixed-point arithmetic (no floating-point nondeterminism) and the sequential
command processing model.

### Failure Modes

| Scenario | Behavior |
|---|---|
| WAL entry CRC mismatch | Error returned, recovery stops at corruption point |
| Snapshot CRC mismatch | Error returned, try previous snapshot |
| Snapshot missing | Start from empty book, replay entire WAL |
| Crash during snapshot write | Temp file left behind, atomic rename did not complete. Latest valid snapshot is used. |
| Crash during WAL write | Partial entry detected at EOF (len mismatch or CRC failure), truncated entries are skipped |
