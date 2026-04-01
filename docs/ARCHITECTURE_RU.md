# Архитектура Matching Engine

## Обзор системы

HFT matching engine для спотовой криптобиржи на Rust. Обрабатывает до 500 торговых пар при ~500K ордеров/сек.

## Структура crate-ов

```
me-core      Базовые типы: FixedPrice, FixedQty, Order, Trade, MatchEvent
me-book      Order book + matching algorithm (single-threaded, no sync)
me-engine    Multi-pair engine, sharded router, command dispatch
me-persist   WAL + Snapshots для персистентности
me-io        Binary wire protocol, TCP + UDS серверы
me-metrics   Prometheus метрики
me-server    Серверный бинарник
me-datagen   Генератор тестовых данных
me-bench     Criterion бенчмарки
```

## Поток данных

```
[Client] → TCP/UDS → [me-io: decode frame]
                           ↓
                    [ShardedRouter: route by SymbolId]
                           ↓
               [crossbeam MPSC channel (per shard)]
                           ↓
                [Shard Worker Thread: tight loop]
                           ↓
              [Engine.process(cmd)] ← СТРОГО ПОСЛЕДОВАТЕЛЬНО
                    ↓          ↓
            [OrderBook]   [StopBook]
                    ↓
             [matching.rs: match_limit / match_market]
                    ↓
              [Vec<MatchEvent>]
                    ↓
          [me-io: encode events → client]
```

## Принцип строгой последовательности

**Фундаментальный инвариант**: все операции в рамках одной торговой пары выполняются СТРОГО ПОСЛЕДОВАТЕЛЬНО. Один `Engine` = один `OrderBook` = один поток исполнения.

- Между парами: полная параллельность (разные shard-ы)
- Внутри пары: нулевой параллелизм
- Детерминизм: одна и та же последовательность команд всегда даёт один и тот же результат
- WAL replay гарантирует воспроизводимость

## Структура Order Book

```
OrderBook
├── bids: BTreeMap<Reverse<FixedPrice>, PriceLevel>   // best bid = first
├── asks: BTreeMap<FixedPrice, PriceLevel>             // best ask = first
├── order_index: FxHashMap<OrderId, OrderLocation>     // O(1) cancel
├── order_slab: Slab<OrderNode>                        // arena allocator
├── best_bid: Option<FixedPrice>                       // cached O(1)
└── best_ask: Option<FixedPrice>                       // cached O(1)
```

### PriceLevel

FIFO-очередь ордеров на одной цене. Реализована как intrusive doubly-linked list через Slab:
- O(1) insert в конец
- O(1) remove из любого места по SlabKey
- O(1) доступ к front (для matching)

### Slab Allocator

Pre-allocated arena: `Vec<Entry<T>>` + free list. После инициализации hot path не трогает глобальный аллокатор.

## Алгоритм matching

### Limit Order (Price-Time Priority)

1. FOK: проверить суммарный объём на встречной стороне. Если < qty → reject.
2. Walk opposite side (best price first, FIFO внутри уровня):
   - execute_fill(taker, maker) по цене maker-а
3. После matching:
   - IOC + remaining > 0 → cancel remainder
   - GTC + remaining > 0 → insert в book

### Market Order

Аналогично Limit, но без ограничения по цене. Никогда не rests on book — unfilled remainder всегда cancel.

### Stop Orders

Stop-limit/Stop-market хранятся в StopBook. Trigger при:
- Buy stops: last_trade_price >= stop_price
- Sell stops: last_trade_price <= stop_price

При trigger: конвертация StopLimit → Limit, StopMarket → Market, подача обратно в process_order. Каскадные triggers ограничены MAX_STOP_CASCADE = 100.

## Шардирование

```
ShardedRouterV2
├── shards: Vec<Shard>          // N = num CPU cores
├── symbol_to_shard: HashMap    // SymbolId → shard index
│
Shard
├── cmd_tx: crossbeam Sender
└── thread: JoinHandle

Worker loop:
  loop { rx.recv() → engine.process(cmd) → reply.send(events) }
```

Символы назначаются в shard-ы при регистрации и никогда не мигрируют. Hash(SymbolId) % num_shards.

## WAL + Snapshots

### Write-Ahead Log

Формат записи: `[len: u32][crc32: u32][sequence: u64][command_bytes]`

- Append-only файл
- CRC32 checksum каждой записи
- Configurable fsync interval
- Replay: читаем записи, проверяем CRC, подаём в Engine

### Snapshots

- Периодический snapshot всего состояния OrderBook
- Включает: все resting orders, stop orders, sequence counters
- При восстановлении: загружаем последний snapshot + replay WAL от его sequence
- Atomic save через temp file + rename

### Восстановление

```
1. Загрузить последний snapshot для символа
2. Создать Engine, re-insert все resting orders
3. Открыть WAL, найти записи после snapshot.sequence
4. Replay WAL entries → engine.process(cmd)
5. Состояние идентично до рестарта
```

## Binary Wire Protocol

Фреймы: `[len: u32][type: u8][seq: u64][payload]`

Все числа little-endian. Цены и количества — fixed-point i64/u64 с scale 10^8.

Минимальный overhead: 13 bytes header + 37 bytes PlaceOrder payload = 50 bytes.
