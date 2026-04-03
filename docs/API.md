# Admin HTTP API Reference / Справочник по HTTP API администрирования

---

## English

### Overview

The matching engine exposes an HTTP API on the metrics address (default `0.0.0.0:9091`) for health checks, Prometheus metrics, and administrative operations. The API is built with Axum and serves both public and authenticated endpoints.

### Endpoints

| Method | Path | Auth | Description |
|---|---|---|---|
| GET | `/health` | No | Health check, returns `OK` |
| GET | `/metrics` | No | Prometheus metrics (text format) |
| GET | `/admin/status` | Yes* | Engine status |
| POST | `/admin/halt/{symbol_id}` | Yes* | Halt trading for a symbol |
| POST | `/admin/resume/{symbol_id}` | Yes* | Resume trading for a symbol |

\* Authentication is required only when `server.admin.api_key` is set in the configuration. If the API key is empty or omitted, admin endpoints are open (intended for development only).

### Authentication

When `api_key` is configured in the TOML config, all `/admin/*` endpoints require the `X-Admin-Key` header:

```toml
[server.admin]
api_key = "your-secret-key"
```

```bash
curl -H "X-Admin-Key: your-secret-key" http://localhost:9091/admin/status
```

Without a configured API key, admin endpoints are accessible without authentication. The server logs a warning at startup:

```
WARN me_server: Admin API key not set — admin endpoints are UNPROTECTED
```

### Request/Response Examples

#### GET /health

Health check endpoint. No authentication required.

```bash
curl http://localhost:9091/health
```

**Response (200 OK):**

```
OK
```

---

#### GET /metrics

Prometheus metrics in text exposition format. No authentication required.

```bash
curl http://localhost:9091/metrics
```

**Response (200 OK):**

```
# HELP me_orders_total Total orders received
# TYPE me_orders_total counter
me_orders_total{side="buy",order_type="limit",tif="gtc",symbol="0"} 15234
me_orders_total{side="sell",order_type="limit",tif="gtc",symbol="0"} 14891
# HELP me_trades_total Total trades executed
# TYPE me_trades_total counter
me_trades_total{symbol="0"} 8721
# HELP me_connections_active Currently active connections
# TYPE me_connections_active gauge
me_connections_active{transport="tcp"} 3
...
```

---

#### GET /admin/status

Returns the current engine status.

```bash
curl -H "X-Admin-Key: your-secret-key" http://localhost:9091/admin/status
```

**Response (200 OK):**

```json
{
  "status": "running",
  "message": "Matching engine is operational"
}
```

**Error (401 Unauthorized) -- missing or invalid API key:**

```bash
curl http://localhost:9091/admin/status
```

```
(empty body, HTTP 401)
```

---

#### POST /admin/halt/{symbol_id}

Halt trading for a specific symbol. All new orders for this symbol will be rejected while halted. Existing resting orders remain in the book.

```bash
curl -X POST -H "X-Admin-Key: your-secret-key" http://localhost:9091/admin/halt/0
```

**Response (200 OK) -- symbol found and halted:**

```json
{
  "symbol_id": 0,
  "action": "halt",
  "success": true
}
```

**Response (404 Not Found) -- symbol does not exist:**

```bash
curl -X POST -H "X-Admin-Key: your-secret-key" http://localhost:9091/admin/halt/999
```

```json
{
  "symbol_id": 999,
  "action": "halt",
  "success": false
}
```

**Error (401 Unauthorized) -- missing or invalid API key:**

```bash
curl -X POST http://localhost:9091/admin/halt/0
```

```
(empty body, HTTP 401)
```

---

#### POST /admin/resume/{symbol_id}

Resume trading for a previously halted symbol.

```bash
curl -X POST -H "X-Admin-Key: your-secret-key" http://localhost:9091/admin/resume/0
```

**Response (200 OK) -- symbol found and resumed:**

```json
{
  "symbol_id": 0,
  "action": "resume",
  "success": true
}
```

**Response (404 Not Found) -- symbol does not exist:**

```bash
curl -X POST -H "X-Admin-Key: your-secret-key" http://localhost:9091/admin/resume/999
```

```json
{
  "symbol_id": 999,
  "action": "resume",
  "success": false
}
```

### Metrics Reference

All metrics use the `me_` prefix. Labels are shown in curly braces.

#### Order Processing

| Metric | Type | Labels | Description |
|---|---|---|---|
| `me_orders_total` | Counter | `side`, `order_type`, `tif`, `symbol` | Total orders received |
| `me_orders_rejected_total` | Counter | `reason`, `symbol` | Rejected orders |
| `me_orders_accepted_total` | Counter | `side`, `order_type`, `symbol` | Accepted orders |
| `me_order_processing_duration_seconds` | Histogram | `order_type` | Time to process a single order |

#### Matching

| Metric | Type | Labels | Description |
|---|---|---|---|
| `me_trades_total` | Counter | `symbol` | Total trades executed |
| `me_trades_volume_total` | Gauge | `symbol` | Total trade volume in quantity |
| `me_trade_price` | Gauge | `symbol` | Last trade price per symbol |
| `me_fills_total` | Counter | `fill_type` | Full vs partial fills |

#### Order Book

| Metric | Type | Labels | Description |
|---|---|---|---|
| `me_book_bid_levels` | Gauge | `symbol` | Number of bid price levels |
| `me_book_ask_levels` | Gauge | `symbol` | Number of ask price levels |
| `me_book_bid_orders` | Gauge | `symbol` | Total resting bid orders |
| `me_book_ask_orders` | Gauge | `symbol` | Total resting ask orders |
| `me_book_spread` | Gauge | `symbol` | Current spread (best_ask - best_bid) |
| `me_book_best_bid` | Gauge | `symbol` | Best bid price |
| `me_book_best_ask` | Gauge | `symbol` | Best ask price |
| `me_book_depth_bid_qty` | Gauge | `symbol` | Total bid quantity depth |
| `me_book_depth_ask_qty` | Gauge | `symbol` | Total ask quantity depth |

#### Cancel/Amend

| Metric | Type | Labels | Description |
|---|---|---|---|
| `me_cancels_total` | Counter | `symbol` | Successful cancellations |
| `me_cancels_failed_total` | Counter | `symbol` | Failed cancellations |
| `me_amends_total` | Counter | `amend_type`, `symbol` | Amends by type |

#### Stop Orders

| Metric | Type | Labels | Description |
|---|---|---|---|
| `me_stop_orders_pending` | Gauge | `symbol` | Pending stop orders |
| `me_stop_triggers_total` | Counter | `symbol` | Stop order triggers |

#### System

| Metric | Type | Labels | Description |
|---|---|---|---|
| `me_commands_total` | Counter | `command_type` | Total commands processed |
| `me_events_total` | Counter | `event_type` | Total events emitted |
| `me_active_symbols` | Gauge | -- | Number of active trading pairs |

#### WAL/Persistence

| Metric | Type | Labels | Description |
|---|---|---|---|
| `me_wal_writes_total` | Counter | -- | WAL write operations |
| `me_wal_bytes_written_total` | Counter | -- | WAL bytes written |
| `me_snapshot_saves_total` | Counter | -- | Snapshot saves |
| `me_wal_flush_duration_seconds` | Histogram | -- | WAL flush duration |
| `me_snapshot_duration_seconds` | Histogram | -- | Snapshot duration |
| `me_snapshot_size_bytes` | Gauge | -- | Snapshot size in bytes |

#### Connections

| Metric | Type | Labels | Description |
|---|---|---|---|
| `me_connections_total` | Counter | `transport` | Total connections opened |
| `me_connections_active` | Gauge | `transport` | Currently active connections |
| `me_connection_bytes_in_total` | Counter | `transport` | Total bytes received |
| `me_connection_bytes_out_total` | Counter | `transport` | Total bytes sent |
| `me_connection_frames_in_total` | Counter | `transport` | Total frames received |
| `me_connection_frames_out_total` | Counter | `transport` | Total frames sent |

#### Protocol Errors

| Metric | Type | Labels | Description |
|---|---|---|---|
| `me_protocol_errors_total` | Counter | `error_type` | Protocol errors by type |

#### Matching Detail

| Metric | Type | Labels | Description |
|---|---|---|---|
| `me_matching_duration_seconds` | Histogram | `command_type` | Matching duration by command type |
| `me_stop_cascade_depth` | Histogram | -- | Depth of cascading stop triggers |
| `me_stop_cascade_duration_seconds` | Histogram | -- | Duration of cascading stop triggers |

#### Shard Metrics

| Metric | Type | Labels | Description |
|---|---|---|---|
| `me_shard_queue_depth` | Gauge | `shard` | Queue depth per shard |
| `me_shard_commands_total` | Counter | `shard` | Total commands per shard |

---

## Русский

### Обзор

Матчинг-движок предоставляет HTTP API на адресе метрик (по умолчанию `0.0.0.0:9091`) для проверки здоровья, метрик Prometheus и административных операций. API построен на Axum и обслуживает как публичные, так и аутентифицированные эндпоинты.

### Эндпоинты

| Метод | Путь | Аутентификация | Описание |
|---|---|---|---|
| GET | `/health` | Нет | Проверка здоровья, возвращает `OK` |
| GET | `/metrics` | Нет | Метрики Prometheus (текстовый формат) |
| GET | `/admin/status` | Да* | Статус движка |
| POST | `/admin/halt/{symbol_id}` | Да* | Приостановить торги по инструменту |
| POST | `/admin/resume/{symbol_id}` | Да* | Возобновить торги по инструменту |

\* Аутентификация требуется только при установленном `server.admin.api_key` в конфигурации. Если API-ключ пустой или не указан, админ-эндпоинты открыты (предназначено только для разработки).

### Аутентификация

Когда `api_key` настроен в TOML-конфигурации, все эндпоинты `/admin/*` требуют заголовок `X-Admin-Key`:

```toml
[server.admin]
api_key = "your-secret-key"
```

```bash
curl -H "X-Admin-Key: your-secret-key" http://localhost:9091/admin/status
```

Без настроенного API-ключа админ-эндпоинты доступны без аутентификации. Сервер выводит предупреждение при запуске:

```
WARN me_server: Admin API key not set — admin endpoints are UNPROTECTED
```

### Примеры запросов и ответов

#### GET /health

Эндпоинт проверки здоровья. Аутентификация не требуется.

```bash
curl http://localhost:9091/health
```

**Ответ (200 OK):**

```
OK
```

---

#### GET /metrics

Метрики Prometheus в текстовом формате. Аутентификация не требуется.

```bash
curl http://localhost:9091/metrics
```

**Ответ (200 OK):**

```
# HELP me_orders_total Total orders received
# TYPE me_orders_total counter
me_orders_total{side="buy",order_type="limit",tif="gtc",symbol="0"} 15234
me_orders_total{side="sell",order_type="limit",tif="gtc",symbol="0"} 14891
# HELP me_trades_total Total trades executed
# TYPE me_trades_total counter
me_trades_total{symbol="0"} 8721
# HELP me_connections_active Currently active connections
# TYPE me_connections_active gauge
me_connections_active{transport="tcp"} 3
...
```

---

#### GET /admin/status

Возвращает текущий статус движка.

```bash
curl -H "X-Admin-Key: your-secret-key" http://localhost:9091/admin/status
```

**Ответ (200 OK):**

```json
{
  "status": "running",
  "message": "Matching engine is operational"
}
```

**Ошибка (401 Unauthorized) -- отсутствующий или неверный API-ключ:**

```bash
curl http://localhost:9091/admin/status
```

```
(пустое тело, HTTP 401)
```

---

#### POST /admin/halt/{symbol_id}

Приостановить торги по конкретному инструменту. Все новые ордера по данному инструменту будут отклоняться во время приостановки. Существующие ордера остаются в стакане.

```bash
curl -X POST -H "X-Admin-Key: your-secret-key" http://localhost:9091/admin/halt/0
```

**Ответ (200 OK) -- инструмент найден и торги приостановлены:**

```json
{
  "symbol_id": 0,
  "action": "halt",
  "success": true
}
```

**Ответ (404 Not Found) -- инструмент не существует:**

```bash
curl -X POST -H "X-Admin-Key: your-secret-key" http://localhost:9091/admin/halt/999
```

```json
{
  "symbol_id": 999,
  "action": "halt",
  "success": false
}
```

**Ошибка (401 Unauthorized) -- отсутствующий или неверный API-ключ:**

```bash
curl -X POST http://localhost:9091/admin/halt/0
```

```
(пустое тело, HTTP 401)
```

---

#### POST /admin/resume/{symbol_id}

Возобновить торги по ранее приостановленному инструменту.

```bash
curl -X POST -H "X-Admin-Key: your-secret-key" http://localhost:9091/admin/resume/0
```

**Ответ (200 OK) -- инструмент найден и торги возобновлены:**

```json
{
  "symbol_id": 0,
  "action": "resume",
  "success": true
}
```

**Ответ (404 Not Found) -- инструмент не существует:**

```bash
curl -X POST -H "X-Admin-Key: your-secret-key" http://localhost:9091/admin/resume/999
```

```json
{
  "symbol_id": 999,
  "action": "resume",
  "success": false
}
```

### Справочник по метрикам

Все метрики используют префикс `me_`. Метки (labels) указаны в фигурных скобках.

#### Обработка ордеров

| Метрика | Тип | Метки | Описание |
|---|---|---|---|
| `me_orders_total` | Counter | `side`, `order_type`, `tif`, `symbol` | Общее количество полученных ордеров |
| `me_orders_rejected_total` | Counter | `reason`, `symbol` | Отклоненные ордера |
| `me_orders_accepted_total` | Counter | `side`, `order_type`, `symbol` | Принятые ордера |
| `me_order_processing_duration_seconds` | Histogram | `order_type` | Время обработки одного ордера |

#### Матчинг (сопоставление)

| Метрика | Тип | Метки | Описание |
|---|---|---|---|
| `me_trades_total` | Counter | `symbol` | Общее количество совершенных сделок |
| `me_trades_volume_total` | Gauge | `symbol` | Общий объем сделок в количестве |
| `me_trade_price` | Gauge | `symbol` | Последняя цена сделки по инструменту |
| `me_fills_total` | Counter | `fill_type` | Полное и частичное исполнение |

#### Стакан ордеров (Order Book)

| Метрика | Тип | Метки | Описание |
|---|---|---|---|
| `me_book_bid_levels` | Gauge | `symbol` | Количество ценовых уровней bid |
| `me_book_ask_levels` | Gauge | `symbol` | Количество ценовых уровней ask |
| `me_book_bid_orders` | Gauge | `symbol` | Всего покоящихся ордеров bid |
| `me_book_ask_orders` | Gauge | `symbol` | Всего покоящихся ордеров ask |
| `me_book_spread` | Gauge | `symbol` | Текущий спред (best_ask - best_bid) |
| `me_book_best_bid` | Gauge | `symbol` | Лучшая цена bid |
| `me_book_best_ask` | Gauge | `symbol` | Лучшая цена ask |
| `me_book_depth_bid_qty` | Gauge | `symbol` | Суммарный объем глубины bid |
| `me_book_depth_ask_qty` | Gauge | `symbol` | Суммарный объем глубины ask |

#### Отмена / Изменение ордеров

| Метрика | Тип | Метки | Описание |
|---|---|---|---|
| `me_cancels_total` | Counter | `symbol` | Успешные отмены |
| `me_cancels_failed_total` | Counter | `symbol` | Неудачные отмены |
| `me_amends_total` | Counter | `amend_type`, `symbol` | Изменения ордеров по типу |

#### Стоп-ордера

| Метрика | Тип | Метки | Описание |
|---|---|---|---|
| `me_stop_orders_pending` | Gauge | `symbol` | Ожидающие стоп-ордера |
| `me_stop_triggers_total` | Counter | `symbol` | Срабатывания стоп-ордеров |

#### Система

| Метрика | Тип | Метки | Описание |
|---|---|---|---|
| `me_commands_total` | Counter | `command_type` | Всего обработанных команд |
| `me_events_total` | Counter | `event_type` | Всего сгенерированных событий |
| `me_active_symbols` | Gauge | -- | Количество активных торговых пар |

#### WAL / Персистентность

| Метрика | Тип | Метки | Описание |
|---|---|---|---|
| `me_wal_writes_total` | Counter | -- | Операции записи в WAL |
| `me_wal_bytes_written_total` | Counter | -- | Байт записано в WAL |
| `me_snapshot_saves_total` | Counter | -- | Сохранения снимков |
| `me_wal_flush_duration_seconds` | Histogram | -- | Длительность flush WAL |
| `me_snapshot_duration_seconds` | Histogram | -- | Длительность создания снимка |
| `me_snapshot_size_bytes` | Gauge | -- | Размер снимка в байтах |

#### Соединения

| Метрика | Тип | Метки | Описание |
|---|---|---|---|
| `me_connections_total` | Counter | `transport` | Всего открытых соединений |
| `me_connections_active` | Gauge | `transport` | Текущие активные соединения |
| `me_connection_bytes_in_total` | Counter | `transport` | Всего получено байт |
| `me_connection_bytes_out_total` | Counter | `transport` | Всего отправлено байт |
| `me_connection_frames_in_total` | Counter | `transport` | Всего получено фреймов |
| `me_connection_frames_out_total` | Counter | `transport` | Всего отправлено фреймов |

#### Ошибки протокола

| Метрика | Тип | Метки | Описание |
|---|---|---|---|
| `me_protocol_errors_total` | Counter | `error_type` | Ошибки протокола по типу |

#### Детали матчинга

| Метрика | Тип | Метки | Описание |
|---|---|---|---|
| `me_matching_duration_seconds` | Histogram | `command_type` | Длительность матчинга по типу команды |
| `me_stop_cascade_depth` | Histogram | -- | Глубина каскадных срабатываний стоп-ордеров |
| `me_stop_cascade_duration_seconds` | Histogram | -- | Длительность каскадных срабатываний |

#### Метрики шардов

| Метрика | Тип | Метки | Описание |
|---|---|---|---|
| `me_shard_queue_depth` | Gauge | `shard` | Глубина очереди на шард |
| `me_shard_commands_total` | Counter | `shard` | Всего команд на шард |
