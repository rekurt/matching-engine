# Глубокий аудит проекта matching-engine (2026-04-10)

## Краткий вердикт

Проект в хорошем техническом состоянии по ядру (matching/risk/persistence/protocol):

- `cargo test` проходит по всему workspace.
- `cargo clippy --workspace --all-targets -- -D warnings` проходит без warning.
- Архитектура хорошо модульная и уже пригодна для production-пилота.

При этом есть несколько практических «пробелов» (особенно вокруг клиентов, документации и операционной устойчивости), которые ограничивают реальное использование.

## Что реально «не работает» или сломано сейчас

### 1) Broken quickstart в корневом README

В README указан путь `examples/go/place_order/main.go`, но такого файла нет. Из-за этого «5-minute quickstart» ломается буквально на первом пользовательском сценарии.

### 2) Непоследовательная Go-история: клиент v1 при сервере v2

Серверный протокол — v2 (header 22 байта, `req_id`, exactly-once механика), но Go клиент в `clients/go/meclient` всё ещё работает на v1 (header 14 байт, без `req_id`).

Фактически:
- библиотека сама объявляет, что v1 и что подключение к v2 серверу не взлетит;
- quickstart-примеры в документации местами выглядят как «рабочие», хотя они не совместимы с текущим сервером.

Это главный blocker для adoption (особенно если аудитория хочет low-latency интеграцию на Go).

### 3) HTTP-часть метрик/admin может падать через `unwrap`

Поднятие HTTP listener-а и `axum::serve` в background task используют `unwrap()`. Если порт занят или bind не удался, таск упадёт panic-ом.

Это не ломает matching loop напрямую, но:
- теряются метрики/админка,
- диагностика становится хуже,
- поведение не полностью controlled-fail.

### 4) Часть protocol-кодов зарезервирована как «future», но не реализована end-to-end

В `me-io` есть ошибки/сценарии, помеченные как зарезервированные на будущую фазу (например `SessionExpired`, `BufferExhausted`). Это нормально для roadmap, но для внешних интеграторов важно явно фиксировать «что уже прод» vs «что контрактно зарезервировано».

## Что не доделано (с точки зрения продукта)

1. **Полноценные официальные клиенты v2** (как минимум Go; желательно и Python/TS c parity).
2. **Runbook/operability слой**:
   - чёткий recovery runbook: как действовать при replay gap / wal corruption / dead shard;
   - алерты + SLO/SLA шаблоны (latency, reject rate, replay pressure).
3. **Failover/redundancy story**:
   - есть WAL/snapshot, но нужен «операционный сценарий» для active-standby с гарантией RPO/RTO.
4. **Удобный admin query API** (не только halt/resume):
   - health подробный,
   - shard/symbol state,
   - backlog/session pressure,
   - текущие risk limits по symbol.

## Какую функцию добавить первой (максимальный ROI)

## `Go client protocol v2 migration` (must-have)

Почему это самая выгодная функция:

- снимает главный friction для интеграции;
- открывает exactly-once (`req_id`) и recovery paths для внешних клиентов;
- превращает текущую сильную серверную часть в реально «подключаемый» продукт.

Минимальный scope:

1. v2 header + request_id plumbing.
2. Handshake/Resume/Ack/OpenOrders support.
3. FlowControlWarning + ReplayGap handling.
4. Обновить все примеры/README на единый рабочий happy-path.
5. Добавить e2e test: `go client ↔ me-server` на v2.

## Что можно улучшить в архитектуре (после client v2)

1. **Operational hardening:** убрать `unwrap` из сетевых bootstrap путей, перевод на управляемые ошибки + retry/backoff.
2. **Snapshot/WAL governance:** policy на размер/частоту/retention + tool для оффлайн-проверки/реплея.
3. **Symbol lifecycle API:** add/remove symbol динамически + безопасная миграция symbol->shard.
4. **Risk extensibility:** plugin-like pre-trade checks (per venue/strategy policy).
5. **Protocol compatibility matrix:** таблица server/client versions + capabilities bitmask examples.

## Неочевидные, но профитные применения проекта

1. **Детерминированный simulation core для research/ML-RL**
   - прогон стратегий на воспроизводимом order flow;
   - сравнение execution quality при изменениях matcher/risk.

2. **Exchange emulator для брокерских интеграций**
   - использовать как high-fidelity sandbox для FIX/REST-gateway обвязки;
   - проверять exactly-once и reconnect-сценарии до выхода на реальную биржу.

3. **Latency lab / regression gate в CI**
   - `me-bench + me-datagen` как performance budget gate;
   - стоп-релиз при деградации p99 latency или росте reject rate.

4. **Risk policy A/B стенд**
   - быстро проверять impact rate-limits/price-bands на fill-rate и toxicity.

## Приоритетный roadmap (коротко)

1. Починить корневой Quickstart + выровнять все примеры.
2. Довести Go client до protocol v2 и добавить e2e-контрактные тесты.
3. Усилить отказоустойчивость bootstrap/admin/metrics путей.
4. Добавить расширенный admin observability API.
5. Закрепить compatibility matrix + runbook для production операций.
