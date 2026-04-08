# CLAUDE.md — matching-engine

## Проект

Высокопроизводительный биржевой движок на Rust. Workspace с 12 крейтами:

| Крейт | Назначение |
|-------|-----------|
| `me-core` | Базовые типы: `FixedPrice`(i64), `FixedQty`(u64), `Order`, `Trade`, `MatchEvent`, `SymbolConfig` |
| `me-book` | Стакан ордеров (`OrderBook`, `StopBook`) и алгоритм матчинга |
| `me-engine` | Оркестратор: `Engine` (1 на пару), `Router`, `ShardedRouterV2` |
| `me-io` | Бинарный TCP/UDS-протокол v2, кодек фреймов (22-байт заголовок), session management |
| `me-risk` | Pre-trade риск-менеджмент: rate limiting, price bands, position limits |
| `me-persist` | WAL с CRC32 контрольными суммами, атомарные снапшоты |
| `me-server` | Точка входа: TCP + UDS, TOML-конфиг, REST admin API (Axum) |
| `me-metrics` | 50+ метрик Prometheus |
| `me-audit` | Структурированные JSON-события аудита |
| `me-bench` | Criterion-бенчмарки (throughput, latency, e2e, stress, realistic) |
| `me-datagen` | Генератор реалистичного потока ордеров |
| `me-clock` | Абстракция часов (SystemClock, CoarseClock, RdtscClock) |

## Команды

```bash
make setup                       # первый раз после клона — активирует pre-commit hook
cargo build --release            # сборка
cargo test --workspace           # все тесты
cargo clippy --workspace --all-targets -- -D warnings   # линтер (0 предупреждений)
cargo bench -p me-bench          # бенчмарки
cargo audit                      # проверка CVE
```

## Pre-commit hook

Расположен в `.githooks/pre-commit`, активируется через `make setup` (или `git config core.hooksPath .githooks`).

Проверяет (fast-fail):
1. Отсутствие merge conflict markers (`<<<<<<<`) в staged файлах
2. `cargo fmt --all -- --check` — форматирование
3. `cargo clippy --workspace --all-targets -- -D warnings` — линтер

Для WIP-коммитов: `git commit --no-verify`.

## Архитектура тестов

### Паттерн helper-функций в тестах

В `crates/me-book/tests/matching_tests.rs` и `crates/me-engine/tests/` используется паттерн:
- `limit(id, side, price, qty, tif)` — создаёт лимитный Order
- `market(id, side, qty)` — создаёт рыночный Order
- `stop_limit(id, side, stop, lim, qty)` — создаёт стоп-лимитный Order
- `new_book()` — создаёт OrderBook с дефолтным конфигом BTC/USDT
- `make_config()` — возвращает `SymbolConfig::default_btc_usdt()`
- `count_trades(events)` — подсчитывает Trade-события
- `has_accepted(events)` / `has_rejected(events)` / `has_filled(events)` / `has_partially_filled(events)` / `has_cancelled(events)` — проверка наличия событий

В `crates/me-engine/tests/` дополнительно:
- `setup()` — создаёт `Engine::new(config)`
- `limit_buy(id, price, qty)` / `limit_sell(id, price, qty)` — упрощённые хелперы
- `market_buy(id, qty)` / `market_sell(id, qty)` / `market_buy_fok(id, qty)`

Новые тесты должны следовать этому паттерну, а не создавать ордера inline.

### Паттерн тестов codec (me-io)

В `crates/me-io/tests/codec_tests.rs`:
- `make_frame(msg_type, seq, payload)` — строит бинарный фрейм с 22-байт заголовком
- `make_frame_with_req_id(msg_type, seq, req_id, payload)` — фрейм с request_id
- `make_frame_with_version(msg_type, seq, version, payload)` — для тестов неверной версии
- `place_order_payload(...)` — формирует payload PlaceOrder
- Всегда проверять `DecodeError::Incomplete` на пустых/коротких буферах
- Тесты round-trip: encode → decode → сверить поля

### Интеграционные тесты

Тесты в `crates/me-engine/tests/` создают движок через `Engine::new(config)`.
Обрабатывают команды через `engine.process(Command::PlaceOrder(order))`.
Проверяют выходные `MatchEvent` через хелпер-функции.

Property-based тесты в `crates/me-engine/tests/property_tests.rs` — 500 случайных
последовательностей команд, проверяют 5 инвариантов стакана (proptest).

## Конвенции кода

### Типы с фиксированной точкой

`FixedPrice`(i64) и `FixedQty`(u64) — fixed-point с масштабом 10^8. Создавать через:
- `FixedPrice::from_f64(1.5)` — не вручную умножать на scale
- `FixedQty::from_f64(0.001)` — аналогично
- `FixedPrice::from_raw(150_000_000)` — из raw i64

`FixedPrice` — знаковый (i64), допускает отрицательные значения для spread-арифметики.
`FixedQty` — беззнаковый (u64), количество всегда >= 0, вычитание через `saturating_sub`.

Переполнение не проверяется в арифметике — держать значения в разумных пределах.

### Структура Order (88 байт, #[repr(C)])

```
id: OrderId(u64), price: FixedPrice(i64), stop_price: FixedPrice(i64),
qty: FixedQty(u64), remaining_qty: FixedQty(u64), cumulative_qty: FixedQty(u64),
timestamp: u64, sequence: SequenceId(u64), symbol: SymbolId(u16),
side: Side(u8), order_type: OrderType(u8), tif: TimeInForce(u8),
status: OrderStatus(u8), flags: u8, _pad: u8, client_session_id: SessionId([u8;16])
```

Флаги ордера (битовая маска в `order.flags`):
- `ORDER_FLAG_POST_ONLY` (0x01) — отклонить, если пересекает спред
- `ORDER_FLAG_REDUCE_ONLY` (0x02) — отклонить, если не уменьшает позицию
- `ORDER_FLAG_MAKER_OR_CANCEL` (0x04) — как POST_ONLY, но для triggered stops

### Обработка ошибок в codec

Протокол v2: заголовок фрейма 22 байта = `len(u32) + ver(u8) + type(u8) + seq(u64) + req_id(u64)`.

`decode_frame` возвращает `Result<(DecodedFrame, usize), DecodeError>`:
- `Ok((frame, n))` — успешный фрейм, `n` байт потреблено
- `Err(DecodeError::Incomplete)` — данных недостаточно
- `Err(DecodeError::...)` — протокольная ошибка (закрыть соединение)

### ConnectionCodecState

Состояние кодека содержит:
- `state: ConnectionState` — `HandshakePending` → `Active` → `Closing`
- `last_client_seq: u64` — монотонность seq
- `session_id: SessionId` — 128-бит идентификатор сессии (назначается при handshake)
- `server_frame_seq: ServerFrameSeq` — монотонный счётчик исходящих фреймов

Команды до handshake отклоняются с `DecodeError::HandshakeRequired`.

### Инварианты стакана

Каждый символ имеет `invariant_policy` (из config.toml):
- `strict` — panic немедленно (dev/staging)
- `halt_symbol` — log + halt только затронутый символ (prod)
- `continue` — log + продолжить (emergency recovery)

Нарушения инкрементируют метрику `me_invariant_violations_total{kind,symbol}`.

## Документация

mdBook в `docs/src/` с публикацией через GitHub Actions на GitHub Pages.
- Исходники: `docs/src/*.md`, навигация в `docs/src/SUMMARY.md`
- Сборка (не в git): `docs/book/`
- Workflow: `.github/workflows/docs.yml` — триггер на изменения в `docs/src/` или `book.toml`

Каждый крейт имеет `README.md` с кратким описанием и code snippet.

## Конфигурация торговых пар

Торговые пары задаются в `config.toml` как `[[symbols]]`:
- `id` — уникальный `symbol_id: u16` (не должен пересекаться)
- `base` / `quote` — тикеры (напр. "BTC", "USDT")
- `price_scale` — десятичные знаки цены (BTC/USDT: 2, SOL/USDT: 4)
- `qty_scale` — десятичные знаки количества (обычно 8)
- `min_qty` / `max_qty` — лимиты количества (строки, напр. "0.00001")
- `qty_step` / `price_step` — шаги (строки, напр. "0.01")
- `invariant_policy` — политика инвариантов (опционально, по умолчанию `strict`)

Серверные настройки в `[server]`:
- `tcp_addr` — TCP (по умолчанию `0.0.0.0:9090`)
- `metrics_addr` — HTTP метрики/admin (по умолчанию `127.0.0.1:9091`)
- `shards` — количество шардов (0 = авто по числу ядер)

Risk-параметры (`max_orders_per_second` и т.п.) настраиваются в `RiskConfig` через код.
