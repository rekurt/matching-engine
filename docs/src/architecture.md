# Архитектура

Matching Engine — это HFT-движок для спотовой криптоторговли на Rust. Реализует
приоритет цена-время с поддержкой лимитных, рыночных, стоп-лимитных и стоп-рыночных
ордеров. Архитектура построена по принципу shared-nothing: каждая торговая пара
обрабатывается ровно одним потоком без блокировок в горячем пути матчинга.

---

## Обзор компонентов

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

## Граф зависимостей крейтов

```
me-server ──> me-engine ──> me-book ──> me-core
    |             |
    +-----------> me-io ──> me-engine ──> me-book ──> me-core
    |
    +-----------> me-metrics ──> me-book ──> me-core
    |
    +-----------> me-risk ──> me-core
    |
    +-----------> me-audit ──> me-core

me-persist ──> me-core    (кодек для WAL/snapshot сериализации)
me-bench   ──> me-engine, me-book, me-core
me-datagen ──> me-core    (генератор потока ордеров)
```

| Крейт        | Назначение                                                                 |
|--------------|----------------------------------------------------------------------------|
| `me-core`    | Базовые типы: FixedPrice(i64), FixedQty(u64), Order, Trade, MatchEvent    |
| `me-book`    | Стакан ордеров: BTreeMap по ценовым уровням, slab allocator, StopBook     |
| `me-engine`  | Engine на торговую пару, Router, ShardedRouterV2                           |
| `me-persist` | WAL с CRC32 и снапшоты с атомарным переименованием                         |
| `me-io`      | Бинарный протокол (14-байт заголовок), async TCP и UDS серверы (tokio)    |
| `me-server`  | Точка входа, CLI (clap), TOML конфиг, REST admin API (Axum, порт 9091)    |
| `me-metrics` | 50+ метрик Prometheus: ордера, сделки, книга, соединения, WAL              |
| `me-risk`    | Pre-trade риск: token-bucket rate limiting, price bands, position limits   |
| `me-audit`   | Структурированные JSON-события аудита через tracing                         |
| `me-bench`   | Criterion-бенчмарки: 18 сценариев по throughput, latency, e2e матчинг     |

---

## Основные типы данных

### Fixed-Point арифметика

Все цены и количества используют fixed-point с масштабом 10^8:

```
FixedPrice(i64)   — знаковый, диапазон: ±92_233_720_368.54775807
                   Пример: $100.50 = FixedPrice(10_050_000_000)

FixedQty(u64)     — беззнаковый (количество всегда >= 0)
                   Пример: 1.5 BTC = FixedQty(150_000_000)
```

Оба типа — `#[repr(transparent)]` обёртки, нулевые накладные расходы.
Все арифметические операторы реализованы с `#[inline]`.

### Структура Order

```rust
#[repr(C)]
pub struct Order {             // 72 байта, cache-line friendly
    pub id: OrderId,           // u64
    pub price: FixedPrice,     // i64
    pub stop_price: FixedPrice,// i64
    pub qty: FixedQty,         // u64
    pub remaining_qty: FixedQty,
    pub cumulative_qty: FixedQty,
    pub timestamp: u64,        // наносекунды от эпохи
    pub sequence: SequenceId,  // u64 (монотонный счётчик книги)
    pub symbol: SymbolId,      // u16
    pub side: Side,            // u8
    pub order_type: OrderType, // u8
    pub tif: TimeInForce,      // u8
    pub status: OrderStatus,   // u8
    pub _pad: [u8; 2],
}
```

---

## Структура стакана ордеров

```
                         OrderBook
    +----------------------------------------------------+
    |  symbol: SymbolId                                  |
    |  config: SymbolConfig                              |
    |  sequence: u64          (монотонный счётчик)       |
    |  best_bid: Option<FixedPrice>  (кэшировано)        |
    |  best_ask: Option<FixedPrice>  (кэшировано)        |
    +----------------------------------------------------+
    |                                                    |
    |  bids: BTreeMap<Reverse<FixedPrice>, PriceLevel>   |
    |        (по убыванию: first() = лучший bid)         |
    |                                                    |
    |  asks: BTreeMap<FixedPrice, PriceLevel>             |
    |        (по возрастанию: first() = лучший ask)      |
    |                                                    |
    |  order_index: FxHashMap<OrderId, OrderLocation>     |
    |        (O(1) lookup для отмены/изменения)          |
    |                                                    |
    |  order_slab: Slab<OrderNode>                        |
    |        (общий пул для всех узлов ордеров)          |
    +----------------------------------------------------+

       Bid (по убыванию)          Ask (по возрастанию)
    +--------+--------+------+ +--------+--------+------+
    |$100.02 |$100.01 |$99.98| |$100.05 |$100.06 |$100.1|
    +--------+--------+------+ +--------+--------+------+
      best bid                   best ask
```

### PriceLevel — интрузивный двусвязный список

Каждый ценовой уровень — FIFO-очередь на интрузивном двусвязном списке через
общий slab allocator. O(1) вставка и O(1) удаление для любой позиции.

### Slab allocator

```
Slab<T> {
    entries: Vec<Entry<T>>,      // предвыделен
    free_head: Option<SlabKey>,  // интрузивный список свободных слотов
    len: usize,
}
```

- O(1) аллокация: переиспользует свободные слоты без обращения к ОС.
- O(1) освобождение.
- Начальная ёмкость: 4096 записей на стакан.
- Ключ типа `u32` — до ~4 млрд записей на книгу.

### StopBook

```
StopBook {
    stop_buys:  BTreeMap<FixedPrice, Vec<Order>>           // по возрастанию
    stop_sells: BTreeMap<Reverse<FixedPrice>, Vec<Order>>  // по убыванию
    stop_index: FxHashMap<OrderId, StopLocation>           // O(1) отмена
}
```

Stop-buy срабатывает когда цена рынка >= stop_price.
Stop-sell срабатывает когда цена рынка <= stop_price.

---

## Алгоритм матчинга

Движок использует **приоритет цена-время** (price-time priority, FIFO):

1. Ордера с лучшей ценой исполняются первыми.
2. В пределах одного ценового уровня — по времени (самый старый первым).

### Лимитный ордер Buy

```
для каждого уровня ask где ask_price <= limit_price:
    для каждого ордера на уровне (FIFO):
        fill_qty = min(taker.remaining, maker.remaining)
        выполнить сделку по ask_price
        если taker полностью исполнен: выход
    если уровень пуст: удалить
    если taker полностью исполнен: выход
```

### Time-in-Force

| TIF | До матчинга | После матчинга |
|-----|-------------|----------------|
| GTC | — | Остаток остаётся в книге |
| IOC | — | Остаток немедленно отменяется |
| FOK | Проверка: суммарная доступная qty >= qty ордера | Должен исполниться полностью или отклониться |

FOK проверяет ликвидность **до** выполнения сделок — гарантирует атомарность.

### Рыночный ордер

Рыночные ордера проходят по всему противоположному стакану без ограничения цены.
Неисполненный остаток всегда отменяется (рыночные ордера **никогда** не остаются в книге).

### Amend (изменение ордера)

- **Только уменьшение qty** (без изменения цены): изменение на месте, сохраняет
  приоритет по времени.
- **Изменение цены или увеличение qty**: отмена существующего + новая вставка.
  Приоритет по времени **теряется**.

---

## Обработка команд в Engine

```
Engine::process(cmd) -> &[MatchEvent]
    |
    +-- events.clear()        // переиспользование буфера без аллокаций
    |
    +-- match cmd:
    |     PlaceOrder(order)
    |       |-- если halted: отклонить
    |       |-- если стоп-ордер: вставить в stop_book
    |       |-- иначе: matching::process_order(book, order, events)
    |       +-- check_stop_triggers()
    |
    |     CancelOrder { order_id }
    |       |-- если в book: matching::cancel_order()
    |       |-- если в stop_book: stop_book.cancel()
    |       +-- иначе: отклонить (OrderNotFound)
    |
    |     AmendOrder { order_id, new_price, new_qty }
    |       |-- если halted: отклонить
    |       |-- matching::amend_order()
    |       +-- check_stop_triggers()
    |
    +-- return events.events()
```

### Каскад стоп-ордеров

После каждой сделки движок проверяет triggered stop-ордера:

```
check_stop_triggers():
    last_price = последняя цена сделки из буфера событий
    cascade_count = 0

    loop:
        если cascade_count >= 100: break   // защита от бесконечного цикла
        triggered = stop_book.check_triggers(last_price)
        если triggered пуст: break

        cascade_count += 1
        для каждого ордера в triggered:
            order.convert_stop()          // StopLimit -> Limit, StopMarket -> Market
            matching::process_order(book, order, events)

        new_price = последняя цена
        если new_price == last_price: break
        last_price = new_price
```

Каскад ограничен **100 итерациями**.

---

## Модель потоков и маршрутизация

### Поток данных

```
                Async I/O (tokio runtime)
+-----------------------------------------------+
|  TCP Listener         UDS Listener            |
|    |                    |                      |
|    v                    v                      |
|  [Task per conn]   [Task per conn]            |
|    |                                           |
|    +-----> decode frame                       |
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
|  Shard 0          Shard 1         Shard N-1  |
|  +----------+    +----------+    +----------+ |
|  | crossbeam|    | crossbeam|    | crossbeam| |
|  | bounded  |    | bounded  |    | bounded  | |
|  | chan 4096|    | chan 4096|    | chan 4096| |
|  +-----+----+    +-----+----+    +-----+----+ |
+--------|----------------|-------------|---------+
         |                |             |
         v                v             v
+------------+  +------------+  +------------+
|  Worker 0  |  |  Worker 1  |  |  Worker N  |
|            |  |            |  |            |
| Engine A   |  | Engine C   |  | Engine E   |
| Engine B   |  | Engine D   |  | Engine F   |
+------------+  +------------+  +------------+
```

### Synchronous RPC

```
Async task (tokio):
  1. Декодировать бинарный кадр в Command
  2. Создать crossbeam oneshot channel (reply_tx, reply_rx)
  3. Отправить CommandEnvelope { cmd, reply: reply_tx } в канал шарда
  4. reply_rx.recv()  — блокирует tokio task (приемлемо: матчинг быстрый)
  5. Закодировать события ответа в бинарные кадры
  6. Записать в сокет

Worker thread:
  1. rx.recv()  — блокирует на crossbeam-канале
  2. Маршрутизировать к Engine по SymbolId
  3. engine.process(cmd)  — синхронно, без аллокаций
  4. reply.send(events)   — пробудить async task
```

### Назначение шардов

```
shard_index = symbol_id % num_shards
```

При `shards = 0` используется `std::thread::available_parallelism()` (все ядра CPU).

---

## Уровень персистентности

### Write-Ahead Log (WAL)

Каждая команда записывается в WAL **до** применения:

```
Формат записи WAL:
+----------+----------+----------+--------------------+
| len: u32 | crc: u32 | seq: u64 | command_bytes: [u8] |
| 4 байта  | 4 байта  | 8 байт   | переменный          |
+----------+----------+----------+--------------------+
```

- Append-only файл через `BufWriter<File>`.
- CRC32 на запись — обнаружение битых данных при восстановлении.

### Снапшоты

```
Формат файла снапшота:
+-------+---------+-----------+----------+-------------+
| Magic | Version | SymbolId  | Sequence | TradeIdGen  |
| "MESN"| u16=1   | u16       | u64      | u64         |
+-------+---------+-----------+----------+-------------+
| RestingCount: u32 | [Order; RestingCount]             |
+--------------------+----------------------------------+
| StopCount: u32     | [Order; StopCount]               |
+--------------------+----------------------------------+
| CRC32: u32                                            |
+-------------------------------------------------------+
```

Атомарная запись: сначала во временный файл, затем атомарное переименование.
Краш во время создания снапшота **никогда** не оставляет битый файл.

---

## Управление рисками (pre-trade)

Все проверки выполняются **до** достижения матчинг-движка. Все — O(1).

1. **Halt**: отклонить, если инструмент остановлен.
2. **Rate limiting**: token-bucket алгоритм на инструмент (`max_orders_per_second`).
3. **Fat-finger qty**: отклонить, если `order.qty > max_order_qty`.
4. **Max notional**: отклонить, если `price × qty > max_order_notional`.
5. **Price band**: отклонить, если цена отклоняется на `price_band_bps` базисных пунктов
   от референсной цены (последняя цена сделки).
6. **Max open orders**: отклонить, если инструмент уже имеет `>= max_open_orders`.

---

## Circuit Breaker

Каждый Engine имеет `TradingState`: `Active` или `Halted`.

**В состоянии Halted:**
- Новые ордера (включая стопы) — **отклоняются** с `TradingHalted`.
- Изменения ордеров — **отклоняются** с `TradingHalted`.
- Отмены — **всегда разрешены** (позволяют снизить экспозицию).

**Управление:**
```
REST: POST /admin/halt/{symbol_id}
         → ShardedRouterV2::halt_symbol(symbol)
         → ShardMsg::HaltSymbol в канал шарда
         → Worker: engine.halt_trading()
```

Halt **eventually consistent**: команды, уже стоящие в очереди шарда,
могут быть обработаны до прибытия сообщения halt.

---

## Наблюдаемость

50+ метрик Prometheus, доступных через `GET /metrics` на порту 9091:

- **Ордера**: `me_orders_total`, `me_orders_rejected_total`, `me_orders_accepted_total`
- **Матчинг**: `me_trades_total`, `me_trades_volume_total`, `me_trade_price`
- **Книга**: `me_book_bid_levels`, `me_book_ask_levels`, `me_book_spread`
- **Стопы**: `me_stop_orders_pending`, `me_stop_triggers_total`
- **Соединения**: `me_connections_active`, `me_connection_bytes_in_total`
- **Производительность**: `me_order_processing_duration_seconds`, `me_shard_queue_depth`
- **WAL**: `me_wal_writes_total`, `me_snapshot_saves_total`

---

## Характеристики производительности

- Аллокации кучи в горячем пути: **0** (slab + EventBuffer)
- Блокировки в потоке матчинга: **0** (shared-nothing)
- Задержка отмены: **O(1)** (FxHashMap lookup + интрузивный список)
- Задержка вставки: **O(log P)** где P = количество ценовых уровней
- Каскад стопов: ограничен 100 итерациями
- WAL запись: одна `BufWriter` append операция на команду
