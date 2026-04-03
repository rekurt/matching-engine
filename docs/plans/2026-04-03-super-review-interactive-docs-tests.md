# Super Review: Interactive Docs, Tests Expansion & Integration Guide

## Overview
Комплексное улучшение проекта matching-engine:
1. Код-ревью через cargo clippy/audit с фиксацией и исправлением найденных проблем
2. README.md для каждого крейта — кратко что умеет и как использовать
3. Исходники mdBook в `docs/src/` + GitHub Actions публикует на GitHub Pages
4. Расширение тестов — приоритет на me-io/codec (0 тестов) и me-book/matching
5. Руководство по интеграции и конфигурации торговых пар

## Context
- Files involved: `crates/me-io/src/codec.rs`, `crates/me-book/src/matching.rs`, `crates/me-book/src/book.rs`, `docs/`, `config.toml`, все существующие тесты в `crates/me-engine/tests/`
- Related patterns: существующие тесты в `crates/me-engine/tests/*.rs` — helper `make_order`, assert на `MatchEvent`
- Dependencies: без новых зависимостей в Cargo.toml; mdBook устанавливается только в CI

## Development Approach
- **Testing approach**: Regular (сначала код, потом тесты) кроме Task 3-4 где тесты — основной продукт
- Complete each task fully before moving to the next
- **CRITICAL: every task MUST include new/updated tests**
- **CRITICAL: all tests must pass before starting next task**

## Implementation Steps

### Task 1: Код-ревью — clippy, audit, анализ критических мест

**Files:**
- Modify: `crates/me-io/src/codec.rs` (если найдены ошибки)
- Modify: `crates/me-book/src/matching.rs` (если найдены ошибки)
- Modify: `crates/me-engine/src/engine.rs` (если найдены ошибки)
- Modify: `crates/me-core/src/types.rs` (если найдены ошибки)

- [x] Запустить `cargo clippy --workspace --all-targets -- -D warnings` и зафиксировать все предупреждения
- [x] Запустить `cargo audit` для проверки зависимостей на CVE
- [x] Проверить обработку ошибок в `me-io/src/codec.rs` — декодирование невалидных фреймов
- [x] Проверить integer overflow в `me-core/src/types.rs` (FixedPrice i64 арифметика)
- [x] Исправить все найденные clippy-предупреждения
- [x] Запустить `cargo test --workspace` — все тесты проходят

### Task 2a: README.md для me-core и me-io

**Files:**
- Create: `crates/me-core/README.md`
- Create: `crates/me-io/README.md`

- [x] `crates/me-core/README.md` — примитивы: типы ордеров, FixedPrice/Qty, структура Order, Side, TimeInForce; минимальный code snippet создания ордера
- [x] `crates/me-io/README.md` — бинарный протокол TCP: кодек фреймов, handshake, sequence numbers; пример подключения клиента
- [x] Каждый README содержит: 1-2 строки что это, краткую таблицу возможностей, минимальный code snippet
- [x] Запустить `cargo test --workspace` — все тесты проходят

### Task 2b: README.md для me-book, me-engine, me-risk, me-server

**Files:**
- Create: `crates/me-book/README.md`
- Create: `crates/me-engine/README.md`
- Create: `crates/me-risk/README.md`
- Create: `crates/me-server/README.md` (если существует)

- [x] `crates/me-book/README.md` — стакан ордеров: LIMIT/MARKET/STOP-LIMIT/STOP-MARKET, FOK/IOC/GTC; пример создания книги и отправки ордера
- [x] `crates/me-engine/README.md` — оркестратор: как запустить движок, lifecycle; пример конфига
- [x] `crates/me-risk/README.md` — управление рисками: rate limiting, position limits; пример риск-параметров
- [x] `crates/me-server/README.md` — TCP-сервер: как запустить, env vars, порты
- [x] Запустить `cargo test --workspace` — все тесты проходят

### Task 3: Тесты для me-io/codec (сейчас 0 тестов — критично)

**Files:**
- Create: `crates/me-io/tests/codec_tests.rs`
- Modify: `crates/me-io/Cargo.toml` (добавить [dev-dependencies] если нужно)

- [x] Тест roundtrip encode→decode для `PlaceOrder` (limit, market, stop-limit, stop-market)
- [x] Тест roundtrip для `CancelOrder` и `AmendOrder`
- [x] Тест decode всех outbound событий: OrderAccepted, Trade, OrderFilled, StopTriggered
- [x] Тест граничных случаев: пустой буфер (Incomplete), невалидный тип сообщения, слишком большой payload
- [x] Тест протокольных нарушений: команда без handshake, неверная версия протокола
- [x] Тест sequence number out-of-order
- [x] Запустить `cargo test -p me-io` — все должны пройти

### Task 4a: Тесты для me-book/matching

**Files:**
- Create: `crates/me-book/tests/matching_tests.rs`

- [x] Тест: FOK с недостаточной ликвидностью — полный отказ без частичного исполнения
- [x] Тест: IOC с частичным исполнением — остаток отменяется
- [x] Тест: рыночный ордер при пустом стакане
- [x] Тест: несколько уровней цен (sweep) — правильный порядок исполнения
- [x] Тест: self-trade (покупатель == продавец, если движок это проверяет)
- [x] Тест stop book: stop-limit не триггерится до достижения стоп-цены, триггерится после
- [x] Запустить `cargo test -p me-book` — все должны пройти

### Task 4b: Тесты для me-risk

**Files:**
- Create: `crates/me-risk/tests/risk_tests.rs`

- [x] Тест: rate limiting — превышение лимита возвращает ошибку
- [x] Тест: position limit — превышение отклоняет ордер
- [x] Тест: корректный сброс rate limit counter после истечения окна
- [x] Запустить `cargo test -p me-risk` — все должны пройти

### Task 5a: mdBook — структура проекта в docs/

**Files:**
- Create: `book.toml` (в корне репозитория, с `src = "docs/src"` и `build.build-dir = "docs/book"`)
- Create: `docs/src/SUMMARY.md`
- Create: `docs/src/introduction.md`

- [x] Создать `book.toml` с настройками: `title`, `authors`, `src = "docs/src"`, `build-dir = "docs/book"`; встроенный поиск включён по умолчанию
- [x] Создать `docs/src/SUMMARY.md` — полная структура навигации со всеми главами
- [x] Написать `docs/src/introduction.md` — что такое matching-engine, для кого, архитектура в 10 строк
- [x] Добавить `docs/book/` в `.gitignore` (build output), `docs/src/` остаётся в git

### Task 5b: mdBook — quickstart и конфигурация торговых пар

**Files:**
- Create: `docs/src/quickstart.md`
- Create: `docs/src/configuration.md`
- Create: `docs/src/trading-pairs.md`

- [ ] `docs/src/quickstart.md` — 5 шагов от установки до первого ордера (build, config, run, connect, place order)
- [ ] `docs/src/configuration.md` — все поля `[[symbols]]` с объяснениями: price_scale, qty_scale, min_qty, price_step, max_orders_per_second
- [ ] `docs/src/trading-pairs.md` — готовые конфиги для: BTC/USDT, ETH/USDT, SOL/USDT, мем-коин (8 decimal), USDC/USDT
- [ ] Добавить раздел "Частые ошибки" — неправильный scale, overlapping symbol ID

### Task 5c: mdBook — руководство по интеграции (Go и Python)

**Files:**
- Create: `docs/src/integration.md`
- Create: `docs/src/clients/go.md`
- Create: `docs/src/clients/python.md`

- [ ] `docs/src/integration.md` — общий обзор: TCP протокол, handshake, отправка команд, получение событий
- [ ] `docs/src/clients/go.md` — полный пример на Go: подключение, place order, обработка ответа (переработать из `examples/go/`)
- [ ] `docs/src/clients/python.md` — аналогичный пример на Python (raw socket + struct)

### Task 5d: mdBook — протокол, архитектура, API, деплой

**Files:**
- Create: `docs/src/protocol.md`
- Create: `docs/src/architecture.md`
- Create: `docs/src/api.md`
- Create: `docs/src/deployment.md`

- [ ] `docs/src/protocol.md` — перенести из `docs/PROTOCOL.md`, добавить hex-примеры фреймов и объяснения каждого поля
- [ ] `docs/src/architecture.md` — перенести из `docs/ARCHITECTURE.md`, добавить диаграммы потоков данных
- [ ] `docs/src/api.md` — перенести из `docs/API.md`, структурировать по типам команд и событий
- [ ] `docs/src/deployment.md` — перенести из `docs/DEPLOYMENT.md`, добавить Docker Compose пример

### Task 5e: GitHub Actions для публикации mdBook на GitHub Pages

**Files:**
- Create: `.github/workflows/docs.yml`

- [ ] Создать workflow: trigger on push to master/main при изменениях в `docs/src/` или `book.toml`
- [ ] Шаги: checkout → install mdbook → `mdbook build` → deploy to GitHub Pages (actions/deploy-pages@v4)
- [ ] Добавить permissions: `contents: read`, `pages: write`, `id-token: write`
- [ ] Использовать `actions/configure-pages@v4` + `actions/upload-pages-artifact@v3` с path `docs/book`

### Task 6: Обновление README.md проекта

**Files:**
- Modify: `README.md`

- [ ] Добавить секцию "5-minute quickstart" (build, run, place first order — 5 строк кода)
- [ ] Добавить badge и ссылку на GitHub Pages документацию
- [ ] Добавить таблицу крейтов с однострочным описанием и ссылками на crate README.md
- [ ] Запустить `cargo test --workspace` — финальная проверка

### Task 7: Verify acceptance criteria

- [ ] manual test: `cargo build --release && ./target/release/me-server` запускается без ошибок
- [ ] run full test suite: `cargo test --workspace` — все тесты проходят
- [ ] run linter: `cargo clippy --workspace -- -D warnings` — 0 предупреждений
- [ ] verify test coverage: новых тестов >= 40 (me-io: ~15, me-book: ~12, me-risk: ~5, интеграционные: ~5)
- [ ] каждый крейт имеет README.md с code snippet
- [ ] GitHub Actions workflow файл создан и валиден (yaml lint)

### Task 8: Update documentation

- [ ] Обновить `CLAUDE.md` если есть паттерны, которые стоит зафиксировать
- [ ] Переместить этот план в `docs/plans/completed/`
