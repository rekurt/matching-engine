# Matching Engine

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Docs](https://img.shields.io/badge/docs-GitHub%20Pages-blue)](https://rekurt.github.io/matching-engine/)

**HFT Cryptocurrency Spot Matching Engine** written in Rust.

A high-performance, deterministic order matching engine designed for cryptocurrency spot trading. Built as a Rust workspace with clear separation of concerns, zero-allocation hot paths, and a binary wire protocol for sub-microsecond network overhead.

---

**HFT Криптовалютный Спот Матчинг-Движок** на Rust.

Высокопроизводительный детерминированный движок сведения ордеров, спроектированный для спотовой торговли криптовалютами. Построен как Rust workspace с четким разделением ответственности, нулевыми аллокациями на горячем пути и бинарным сетевым протоколом для минимальных сетевых задержек.

---

## 5-Minute Quickstart

```bash
# 1. Build
cargo build --release

# 2. Run (BTC/USDT, ETH/USDT, SOL/USDT by default)
./target/release/me-server

# 3. Place your first order (Go)
go run examples/go/place_order/main.go
```

```go
// 4. Connect and place an order
client, _ := meclient.Dial("localhost:9090")
defer client.Close()
client.PlaceOrder(&meclient.PlaceOrderMsg{
    OrderID: 1, SymbolID: 0, Side: meclient.SideBuy,
    OrderType: meclient.OrderTypeLimit, TimeInForce: meclient.TimeInForceGTC,
    Price: meclient.NewDecimal(50000.0), Qty: meclient.NewDecimal(0.5),
})

// 5. Read the response
event, _ := client.ReadEvent()
fmt.Printf("event: %+v\n", event)
```

Full walkthrough: [rekurt.github.io/matching-engine/](https://rekurt.github.io/matching-engine/quickstart.html)

---

## Architecture / Архитектура

```
                       +-----------+
                       |  TCP/UDS  |    Binary wire protocol v2
                       |  Clients  |    Бинарный протокол v2
                       +-----+-----+
                             |
                    +--------v--------+
                    |    me-io        |   Frame decode/encode (22-byte header)
                    |  codec + server |   Session management, TLS
                    +--------+--------+
                             |
                  +----------+----------+
                  |                     |
         +--------v--------+   +--------v--------+
         |    me-risk      |   |   me-engine     |
         | Rate limit,     |   |  Router (shard) |   Command dispatch per SymbolId
         | price bands     |   +--------+--------+   Маршрутизация команд по SymbolId
         +-----------------+            |
               +------------------------+-------------+
               |                        |             |
          +----v----+              +----v----+   +----v----+
          | Engine  |              | Engine  |   | Engine  |   1 Engine = 1 pair
          | BTC/USD |              | ETH/USD |   | SOL/USD |   1 движок = 1 пара
          +----+----+              +---------+   +---------+
               |
       +-------+-------+
       |               |
  +----v-----+   +-----v------+
  | OrderBook |  |  StopBook  |   BTreeMap + Slab + linked list
  |  me-book  |  |  me-book   |
  +----+------+  +------------+
       |                              +------------------+
       +---> me-core                  |   me-persist     |  WAL + Snapshots
             FixedPrice, Order, Trade |  wal + snapshot  |
                                      +------------------+
  Observability: me-metrics (Prometheus 50+) | me-audit (JSON events)
```

## Crate Structure / Структура крейтов

| Crate | Description | Описание |
|-------|-------------|----------|
| [**me-core**](crates/me-core/README.md) | Foundational types: `FixedPrice`, `FixedQty`, `Order`, `Trade`, `SymbolConfig`, `MatchEvent`, error types | Базовые типы: цены, количества, ордера, сделки, события, ошибки |
| [**me-book**](crates/me-book/README.md) | Order book implementation: `OrderBook` (BTreeMap + Slab + intrusive linked list), `PriceLevel`, `StopBook`, matching algorithm | Стакан ордеров: BTreeMap + Slab + интрузивный связанный список, уровни цен, стоп-книга, алгоритм мэтчинга |
| [**me-engine**](crates/me-engine/README.md) | Engine per trading pair, `Command` dispatch, `Router` (single-thread), `ShardedRouterV2` (multi-thread) | Движок на пару, диспетчеризация команд, маршрутизатор (однопоточный и шардированный) |
| **me-persist** | Write-Ahead Log (WAL) with CRC32 checksums, snapshot save/load with atomic writes | WAL с CRC32 контрольными суммами, сохранение/загрузка снепшотов с атомарной записью |
| [**me-io**](crates/me-io/README.md) | Binary wire protocol definition, frame codec (encode/decode), TCP and Unix Domain Socket async servers (tokio) | Определение бинарного протокола, кодек фреймов, TCP и UDS серверы (tokio) |
| **me-bench** | Criterion benchmarks: throughput, latency, end-to-end scenarios (18 benchmarks) | Бенчмарки Criterion: пропускная способность, задержка, сквозные сценарии (18 бенчмарков) |
| [**me-server**](crates/me-server/README.md) | Server binary: TCP + UDS listeners, TOML config, admin REST API (Axum) | Серверный бинарник: TCP + UDS, TOML конфигурация, REST API администрирования |
| **me-metrics** | Prometheus metrics (50+ counters, gauges, histograms) | Метрики Prometheus (50+ счётчиков, гейджей, гистограмм) |
| [**me-risk**](crates/me-risk/README.md) | Risk management: rate limiting, position limits | Риск-менеджмент: ограничение частоты, лимиты позиций |
| **me-audit** | Audit logging: JSON events for all matching operations | Аудит: JSON-события всех операций мэтчинга |
| **me-datagen** | Realistic order flow generator with configurable parameters: volatility, market/limit/stop ratios, cancel/amend ratios | Генератор реалистичного потока ордеров с настраиваемыми параметрами |
| **me-clock** | Clock abstraction: `SystemClock`, `CoarseClock`, `RdtscClock` for deterministic testing and low-overhead timestamping | Абстракция часов для тестирования и low-overhead таймстемпинга |

## Features / Возможности

### Order Types / Типы ордеров

- **Limit** -- rests on book at specified price / размещается в стакане по указанной цене
- **Market** -- executes immediately against available liquidity / исполняется немедленно по доступной ликвидности
- **Stop-Limit** -- triggers as limit order when stop price is reached / активируется как лимитный при достижении стоп-цены
- **Stop-Market** -- triggers as market order when stop price is reached / активируется как рыночный при достижении стоп-цены

### Time-in-Force / Время действия

- **GTC** (Good-Til-Cancel) -- remains on book until filled or cancelled / остается в стакане до исполнения или отмены
- **IOC** (Immediate-or-Cancel) -- fill what you can, cancel the rest / исполнить что можно, остальное отменить
- **FOK** (Fill-or-Kill) -- fill entirely or reject entirely / исполнить полностью или отклонить полностью

### Other Capabilities / Прочие возможности

- Configurable precision per symbol: tick size, lot size, min/max quantity
- Настраиваемая точность по символу: шаг цены, шаг лота, мин/макс количество
- WAL (Write-Ahead Log) with CRC32 checksums for crash recovery
- WAL с CRC32 контрольными суммами для восстановления после сбоев
- Atomic snapshots of order book state for fast startup
- Атомарные снепшоты состояния стакана для быстрого запуска
- Order amend with time-priority preservation on qty-down
- Изменение ордера с сохранением приоритета времени при уменьшении количества

## Key Design Decisions / Ключевые проектные решения

1. **Price-Time Priority** -- orders are matched first by price (best price wins), then by arrival time (FIFO within a price level). / Ордера сводятся сначала по цене (лучшая цена побеждает), затем по времени прибытия (FIFO внутри уровня цены).

2. **Strict Sequential Processing Per Pair** -- each trading pair is processed by exactly one `Engine` instance. No locks, no shared mutable state within a pair. Deterministic replay guaranteed. / Каждая торговая пара обрабатывается ровно одним экземпляром `Engine`. Без блокировок, без разделяемого мутабельного состояния. Гарантия детерминированного воспроизведения.

3. **Sharded Router** -- multiple pairs are distributed across worker threads (`ShardedRouterV2`). Each shard owns a subset of engines and runs its own command loop. Crossbeam bounded channels for backpressure. / Несколько пар распределяются по рабочим потокам. Каждый шард владеет подмножеством движков и выполняет свой цикл обработки команд.

4. **Zero-Allocation Hot Path** -- the `EventBuffer` is pre-allocated and reused across commands. The `Slab` allocator recycles order node slots via an intrusive free list. `BTreeMap` handles price-level indexing without per-operation heap allocation in the common case. / `EventBuffer` предварительно аллоцирован и переиспользуется. Slab-аллокатор переиспользует слоты через интрузивный свободный список.

5. **Fixed-Point Arithmetic** -- all prices (i64) and quantities (u64) use 8-decimal fixed-point representation (scale = 10^8). No floating-point in the matching path. / Все цены (i64) и количества (u64) используют 8-десятичное представление с фиксированной точкой. Без чисел с плавающей точкой на пути мэтчинга.

## Build & Test / Сборка и тестирование

```bash
# Build the entire workspace / Сборка всего workspace
cargo build

# Build in release mode / Сборка в режиме release
cargo build --release

# Run all tests / Запуск всех тестов
cargo test

# Run tests for a specific crate / Тесты для конкретного крейта
cargo test -p me-core
cargo test -p me-book
cargo test -p me-engine
cargo test -p me-persist

# Run benchmarks / Запуск бенчмарков
cargo bench -p me-bench

# Run a specific benchmark suite / Запуск конкретного набора бенчмарков
cargo bench -p me-bench --bench throughput
cargo bench -p me-bench --bench latency
cargo bench -p me-bench --bench e2e
```

## Data Generator / Генератор данных

The `me-datagen` crate provides a realistic order flow generator for stress testing and performance analysis.

Крейт `me-datagen` предоставляет генератор реалистичного потока ордеров для стресс-тестирования и анализа производительности.

```bash
# Run with default settings (100k orders, 1 pair)
# Запуск с настройками по умолчанию (100к ордеров, 1 пара)
cargo run -p me-datagen --release

# View all options / Просмотр всех опций
cargo run -p me-datagen -- --help

# Custom scenario / Пользовательский сценарий
cargo run -p me-datagen --release -- \
    --orders 1000000 \
    --pairs 5 \
    --base-price 50000.0 \
    --volatility 0.002 \
    --market-ratio 0.15 \
    --cancel-ratio 0.20 \
    --amend-ratio 0.05 \
    --stop-ratio 0.05 \
    --seed 42
```

### Parameters / Параметры

| Parameter | Default | Description / Описание |
|-----------|---------|------------------------|
| `--orders` | 100000 | Number of commands to generate / Количество команд |
| `--pairs` | 1 | Number of trading pairs / Количество торговых пар |
| `--base-price` | 50000.0 | Starting mid-price / Начальная средняя цена |
| `--volatility` | 0.001 | Per-step price volatility / Волатильность цены за шаг |
| `--market-ratio` | 0.15 | Fraction of market orders / Доля рыночных ордеров |
| `--cancel-ratio` | 0.20 | Fraction of cancel commands / Доля команд отмены |
| `--amend-ratio` | 0.05 | Fraction of amend commands / Доля команд изменения |
| `--stop-ratio` | 0.05 | Fraction of stop orders / Доля стоп-ордеров |
| `--buy-ratio` | 0.5 | Buy-side probability / Вероятность покупки |
| `--ioc-ratio` | 0.05 | IOC time-in-force probability / Вероятность IOC |
| `--fok-ratio` | 0.02 | FOK time-in-force probability / Вероятность FOK |
| `--seed` | random | RNG seed for reproducibility / Сид ГСЧ для воспроизводимости |

## Go Client / Go клиент

A Go client library is provided at `clients/go/meclient/`. It supports both TCP and Unix Domain Socket connections and implements the full binary wire protocol.

Go клиентская библиотека находится в `clients/go/meclient/`. Поддерживает TCP и Unix Domain Socket соединения, реализует полный бинарный протокол.

```go
client, err := meclient.Dial("localhost:9090")
defer client.Close()

seq, err := client.PlaceOrder(&meclient.PlaceOrderMsg{
    OrderID:   1,
    SymbolID:  0,
    Side:      meclient.SideBuy,
    OrderType: meclient.OrderTypeLimit,
    TimeInForce: meclient.TimeInForceGTC,
    Price:     meclient.NewDecimal(50000.00),
    StopPrice: meclient.DecimalZero(),
    Qty:       meclient.NewDecimal(0.5),
})

event, err := client.ReadEvent()
```

See `clients/go/meclient/README.md` for the full wire protocol specification.

Полная спецификация протокола описана в `clients/go/meclient/README.md`.

## Performance Characteristics / Характеристики производительности

The engine is designed for low-latency, high-throughput order processing. Key performance properties:

Движок спроектирован для обработки ордеров с низкой задержкой и высокой пропускной способностью.

- **Single order placement (no match)**: sub-microsecond / Размещение ордера без мэтча: менее микросекунды
- **Single order match (1 fill)**: low single-digit microseconds / Одиночный мэтч: единицы микросекунд
- **Market order sweep (15 levels)**: tens of microseconds / Рыночный ордер через 15 уровней: десятки микросекунд
- **Cancel order**: sub-microsecond (O(1) via hash map + slab) / Отмена ордера: менее микросекунды
- **Amend qty-down (in-place)**: sub-microsecond / Уменьшение количества: менее микросекунды

Run `cargo bench -p me-bench` for precise measurements on your hardware.

Запустите `cargo bench -p me-bench` для точных измерений на вашем оборудовании.

### Benchmark Suites / Наборы бенчмарков

| Suite | Focus / Фокус |
|-------|----------------|
| `throughput` | Sustained throughput for limit and market orders / Пропускная способность |
| `latency` | Single-operation latency (place + cancel) / Задержка одиночных операций |
| `e2e` | Full scenarios: all order types, TIFs, amend, cancel, stop triggers, deep book, multi-pair routing / Полные сценарии |
| `stress` | High-load stress testing / Стресс-тестирование под нагрузкой |
| `realistic` | Realistic trading patterns / Реалистичные торговые паттерны |

## Wire Protocol Overview / Обзор сетевого протокола

The engine communicates via a custom binary protocol over TCP or Unix Domain Sockets. All integers are little-endian. Prices and quantities use fixed-point representation with scale 10^8.

Движок общается через бинарный протокол по TCP или Unix Domain Socket. Все целые числа в little-endian. Цены и количества используют представление с фиксированной точкой с масштабом 10^8.

**Frame format / Формат фрейма (protocol v2):**

```
+----------+---------+----------+----------+-------------+-----------------+
| len: u32 | ver: u8 | type: u8 | seq: u64 | req_id: u64 | payload: [u8]   |
| 4 bytes  | 1 byte  | 1 byte   | 8 bytes  | 8 bytes     | variable        |
+----------+---------+----------+----------+-------------+-----------------+
```

- `len` -- payload length (does not include the 22-byte header) / длина payload (не включает 22-байтный заголовок)
- `ver` -- protocol version, must be `0x02` / версия протокола, должна быть `0x02`
- `type` -- message type identifier / идентификатор типа сообщения
- `seq` -- monotonically increasing sequence number / монотонно возрастающий порядковый номер
- `req_id` -- client idempotency key (0 for server→client) / ключ идемпотентности клиента

**Inbound messages / Входящие сообщения:** `PlaceOrder` (0x01), `CancelOrder` (0x02), `AmendOrder` (0x03), `Ping` (0x04), `Handshake` (0x05), `ResumeSession` (0x06), `GetOrderStatus` (0x07), `Acknowledge` (0x08), `GetOpenOrders` (0x09), `BatchPlaceOrder` (0x0B), `BatchCancelOrder` (0x0C)

**Outbound messages / Исходящие сообщения:** `OrderAccepted` (0x10), `OrderRejected` (0x11), `Trade` (0x12), `OrderFilled` (0x13), `OrderPartiallyFilled` (0x14), `OrderCancelled` (0x15), `OrderAmended` (0x16), `StopTriggered` (0x17), `Pong` (0x18), `Error` (0x19), `HandshakeAck` (0x1A), `SessionResumed` (0x1B), `OrderStatus` (0x1D), `OrderNotFound` (0x1E), `OpenOrdersBegin` (0x1F), `ReplayGap` (0x20), `OpenOrder` (0x21), `OpenOrdersEnd` (0x22), `FlowControlWarning` (0x23)

For detailed byte layouts, see [docs/PROTOCOL.md](docs/PROTOCOL.md) and `clients/go/meclient/README.md`.

Подробные схемы байтов описаны в [docs/PROTOCOL.md](docs/PROTOCOL.md) и `clients/go/meclient/README.md`.

## Quick Start / Быстрый старт

```bash
# Build and run with defaults (BTC/USDT, ETH/USDT, SOL/USDT)
cargo build --release
./target/release/me-server

# Run with custom config
./target/release/me-server --config config.toml

# Run with CLI overrides
./target/release/me-server --config config.toml --tcp-addr 127.0.0.1:9090 --shards 8
```

## Client Examples / Примеры клиентов

Client examples are available in multiple languages:

Примеры клиентов доступны на нескольких языках:

| Language | Path | Dependencies |
|----------|------|--------------|
| **Go** | [`examples/go/`](examples/go/) | meclient library |
| **Python** | [`examples/python/`](examples/python/) | stdlib only (socket + struct) |
| **TypeScript** | [`examples/typescript/`](examples/typescript/) | Node.js Buffer API |
| **Rust** | [`examples/rust/`](examples/rust/) | tokio |

The Go client library is available at [`clients/go/meclient/`](clients/go/meclient/).

Go клиентская библиотека находится в [`clients/go/meclient/`](clients/go/meclient/).

## Documentation / Документация

**Online (GitHub Pages):** [https://rekurt.github.io/matching-engine/](https://rekurt.github.io/matching-engine/)

- [Architecture (English)](docs/ARCHITECTURE.md) / [Архитектура (русский)](docs/ARCHITECTURE_RU.md)
- [Wire Protocol Specification](docs/PROTOCOL.md) / [Спецификация протокола](docs/PROTOCOL.md)
- [Deployment & Configuration](docs/DEPLOYMENT.md) / [Развёртывание и конфигурация](docs/DEPLOYMENT.md)
- [Admin HTTP API Reference](docs/API.md) / [Справочник Admin API](docs/API.md)
- [Go Client](clients/go/meclient/README.md) / [Go клиент](clients/go/meclient/README.md)
- [Contributing](CONTRIBUTING.md) / [Участие в разработке](CONTRIBUTING.md)
- [Changelog](CHANGELOG.md)

### Cargo Documentation / Документация Cargo

```bash
# Generate and open API documentation
cargo doc --workspace --no-deps --open
```
