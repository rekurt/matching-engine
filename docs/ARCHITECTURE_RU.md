# Архитектура Matching Engine

## Обзор системы

HFT matching engine для спотовой криптобиржи на Rust. Обрабатывает до 500 торговых пар при ~500K ордеров/сек.

## Структура crate-ов

```
me-core      Базовые типы: FixedPrice, FixedQty, Order, Trade, MatchEvent, SymbolConfig
me-book      Order book + matching algorithm (single-threaded, no sync)
me-engine    Multi-pair engine, sharded router, command dispatch
me-persist   WAL + Snapshots для персистентности
me-io        Binary wire protocol (14-byte header), TCP + UDS серверы
me-server    Серверный бинарник, TOML конфигурация, REST admin API
me-metrics   Prometheus метрики (50+ метрик)
me-risk      Риск-менеджмент: rate limiting, лимиты позиций
me-audit     Аудит-логирование: JSON-события всех операций
me-datagen   Генератор тестовых данных
me-bench     Criterion бенчмарки (throughput, latency, e2e, stress)
```

### Граф зависимостей

```
me-server ─── me-io ──── me-engine ─── me-book ─── me-core
    │            │            │
    │            │            ├── me-persist ─── me-core
    │            │            │
    ├── me-metrics            ├── me-risk ────── me-core
    │                         │
    └── me-audit              └── me-audit ───── me-core
```

## Поток данных

```
[Client] → TCP/UDS → [me-io: decode frame]
                           ↓
                    [ShardedRouter: route by SymbolId]
                           ↓
               [crossbeam MPSC channel (per shard, capacity 4096)]
                           ↓
                [Shard Worker Thread: tight loop]
                           ↓
              [Engine.process(cmd)] ← СТРОГО ПОСЛЕДОВАТЕЛЬНО
                    ↓          ↓
            [OrderBook]   [StopBook]
                    ↓
             [matching.rs: match_limit / match_market]
                    ↓
              [EventBuffer (pre-allocated, capacity 256)]
                    ↓
          [me-io: encode events → client]
```

## Принцип строгой последовательности

**Фундаментальный инвариант**: все операции в рамках одной торговой пары выполняются СТРОГО ПОСЛЕДОВАТЕЛЬНО. Один `Engine` = один `OrderBook` = один поток исполнения.

- Между парами: полная параллельность (разные shard-ы)
- Внутри пары: нулевой параллелизм, нулевая синхронизация
- Детерминизм: одна и та же последовательность команд всегда даёт один и тот же результат
- WAL replay гарантирует воспроизводимость

## Структура Order Book

```
OrderBook
├── bids: BTreeMap<Reverse<FixedPrice>, PriceLevel>   // best bid = first
├── asks: BTreeMap<FixedPrice, PriceLevel>             // best ask = first
├── order_index: FxHashMap<OrderId, OrderLocation>     // O(1) cancel/amend
├── order_slab: Slab<OrderNode>                        // arena allocator
├── best_bid: Option<FixedPrice>                       // cached O(1)
├── best_ask: Option<FixedPrice>                       // cached O(1)
├── sequence: SequenceId                               // monotonic
└── trade_id_gen: TradeId                              // monotonic
```

### PriceLevel

FIFO-очередь ордеров на одной цене. Реализована как intrusive doubly-linked list через Slab:
- O(1) insert в конец
- O(1) remove из любого места по SlabKey
- O(1) доступ к front (для matching)
- total_qty — агрегированный объём на уровне (обновляется при каждой операции)
- order_count — количество ордеров на уровне

### Slab Allocator

Pre-allocated arena: `Vec<Entry<T>>` + free list. После инициализации hot path не трогает глобальный аллокатор. `OrderNode` содержит `Order` + prev/next указатели (slab keys) для интрузивного связанного списка.

## Алгоритм matching

### Limit Order (Price-Time Priority)

1. FOK: проверить суммарный объём на встречной стороне. Если < qty → reject.
2. Walk opposite side (best price first, FIFO внутри уровня):
   - execute_fill(taker, maker) по цене maker-а
   - Emit: Trade + OrderPartiallyFilled/OrderFilled для обеих сторон
3. После matching:
   - IOC + remaining > 0 → cancel remainder
   - GTC + remaining > 0 → insert в book
   - FOK: если не полностью заполнен → reject (never happens — checked pre-execution)

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
├── shards: Vec<Shard>          // N = num CPU cores (по умолчанию)
├── symbol_to_shard: HashMap    // SymbolId → shard index
│
Shard
├── cmd_tx: crossbeam::Sender<ShardMsg>   // bounded, capacity 4096
└── thread: JoinHandle                     // dedicated worker

Worker loop:
  loop { rx.recv() → engine.process(cmd) → reply.send(events) }
```

Символы назначаются в shard-ы при регистрации: `symbol_id % num_shards`. Никогда не мигрируют.

Synchronous RPC: каждая команда содержит oneshot reply канал. I/O поток блокируется до получения ответа — это обеспечивает порядок response-ов.

## Конфигурация

Сервер поддерживает два уровня конфигурации:
1. **TOML файл** (`--config config.toml`) — символы, адреса, шарды, API ключ
2. **CLI аргументы** — перезаписывают значения из конфига

Без `--config` используются встроенные defaults: BTC/USDT, ETH/USDT, SOL/USDT.

## Admin API

HTTP REST API на отдельном порту (по умолчанию :9091) через Axum:

| Эндпоинт | Метод | Аутентификация | Описание |
|-----------|-------|----------------|----------|
| `/health` | GET | Нет | Проверка состояния |
| `/metrics` | GET | Нет | Prometheus метрики |
| `/admin/status` | GET | X-Admin-Key | Статус движка |
| `/admin/halt/{symbol_id}` | POST | X-Admin-Key | Остановить торги |
| `/admin/resume/{symbol_id}` | POST | X-Admin-Key | Возобновить торги |

Аутентификация через заголовок `X-Admin-Key` — включается при установке `server.admin.api_key` в конфиге.

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
- Atomic save через temp file + rename (crash-safe)
- Magic number: `MESN`, версия: 1

### Восстановление

```
1. Загрузить последний snapshot для символа
2. Создать Engine, re-insert все resting orders
3. Открыть WAL, найти записи после snapshot.sequence
4. Replay WAL entries → engine.process(cmd)
5. Состояние идентично до рестарта
```

## Binary Wire Protocol

14-байтный заголовок:
```
┌──────────┬──────────┬──────────┬──────────┬─────────────────┐
│ len: u32 │ ver: u8  │ type: u8 │ seq: u64 │ payload: [u8]   │
│ 4 bytes  │ 1 byte   │ 1 byte   │ 8 bytes  │ variable        │
└──────────┴──────────┴──────────┴──────────┴─────────────────┘
```

- `len` — размер payload (не включает заголовок)
- `ver` — версия протокола (сейчас 1)
- Все числа little-endian
- Цены и количества — fixed-point i64/u64 с scale 10^8

Подробная спецификация: [docs/PROTOCOL.md](PROTOCOL.md)

## Метрики и наблюдаемость

50+ Prometheus метрик:
- **Ордера**: orders_total, orders_rejected_total, order_processing_duration_seconds
- **Сделки**: trades_total, trades_volume_total, trade_price
- **Стакан**: bid/ask levels, spread, best_bid/best_ask, depth
- **Соединения**: connections_active, bytes_in/out, protocol_errors_total
- **WAL**: wal_writes_total, wal_flush_duration_seconds
- **Шарды**: shard_queue_depth, shard_commands_total

## Характеристики производительности

- **Размещение ордера (без мэтча)**: менее микросекунды
- **Одиночный мэтч (1 fill)**: единицы микросекунд
- **Market sweep (15 уровней)**: десятки микросекунд
- **Отмена ордера**: менее микросекунды (O(1) через HashMap + Slab)
- **Уменьшение количества (in-place amend)**: менее микросекунды

## Нулевые аллокации на горячем пути

1. **EventBuffer** — pre-allocated (capacity 256), reused across commands
2. **Slab<OrderNode>** — arena allocator с intrusive free list
3. **BTreeMap** — не аллоцирует в common case (переиспользует узлы)
4. **BytesMut** — pre-allocated write buffer для encoding
5. **Fixed-point** — никаких heap-аллокаций для арифметики
