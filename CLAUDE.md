# CLAUDE.md — matching-engine

## Проект

Высокопроизводительный биржевой движок на Rust. Workspace с крейтами:
- `me-core` — типы, ордера, события
- `me-book` — стакан ордеров и матчинг
- `me-io` — TCP-протокол и кодек фреймов
- `me-engine` — оркестратор
- `me-risk` — риск-менеджмент
- `me-server` — точка входа

## Команды

```bash
cargo build --release            # сборка
cargo test --workspace           # все тесты
cargo clippy --workspace --all-targets -- -D warnings   # линтер (0 предупреждений)
cargo audit                      # проверка CVE
```

## Архитектура тестов

### Паттерн helper-функций в тестах

В `crates/me-book/tests/matching_tests.rs` и `crates/me-engine/tests/` используется паттерн:
- `make_order(id, side, price, qty, tif)` — создаёт Order без повторения полей
- `new_book()` — создаёт OrderBook с дефолтным конфигом BTC/USDT
- `make_config()` — возвращает `SymbolConfig::default_btc_usdt()`

Новые тесты должны следовать этому паттерну, а не создавать ордера inline.

### Паттерн тестов codec (me-io)

В `crates/me-io/tests/codec_tests.rs`:
- `make_frame(msg_type, seq, payload)` — строит бинарный фрейм с правильным заголовком
- `make_frame_with_version(msg_type, seq, version, payload)` — для тестов неверной версии
- Всегда проверять `DecodeError::Incomplete` на пустых/коротких буферах
- Тесты round-trip: encode → decode → сверить поля

### Интеграционные тесты

Тесты в `crates/me-engine/tests/` запускают движок целиком через `Engine::spawn()`.
Используют `MatchEvent` для проверки выходных событий.

## Конвенции кода

### Типы с фиксированной точкой

`FixedPrice` и `FixedQty` — i64 умноженные на scale. Всегда создавать через:
- `FixedPrice::from_f64(1.5)` — не вручную умножать на scale
- `FixedQty::from_f64(0.001)` — аналогично

Переполнение i64 не проверяется в арифметике — держать значения в разумных пределах.

### Обработка ошибок в codec

`decode_frame` возвращает `Result<Option<DecodedFrame>, DecodeError>`:
- `Ok(None)` — данных недостаточно (Incomplete)
- `Ok(Some(frame))` — успешный фрейм
- `Err(DecodeError::...)` — протокольная ошибка (закрыть соединение)

### ConnectionCodecState

Состояние кодека (`ConnectionCodecState`) содержит `ConnectionState` и `seq_counter`.
Команды до handshake отклоняются с `DecodeError::CommandBeforeHandshake`.

## Документация

mdBook в `docs/src/` с публикацией через GitHub Actions на GitHub Pages.
- Исходники: `docs/src/*.md`
- Сборка (не в git): `docs/book/`
- Workflow: `.github/workflows/docs.yml` — триггер на изменения в `docs/src/` или `book.toml`

Каждый крейт имеет `README.md` с кратким описанием и code snippet.

## Конфигурация торговых пар

Торговые пары задаются в `config.toml` как `[[symbols]]`:
- `price_scale` — десятичные знаки цены (BTC: 1, ETH: 2)
- `qty_scale` — десятичные знаки количества (обычно 3-8)
- `min_qty` — минимальное количество в fixed-point единицах
- `price_step` — шаг цены в fixed-point единицах
- `max_orders_per_second` — rate limit на символ

Уникальный `symbol_id` — не должен пересекаться между символами.
