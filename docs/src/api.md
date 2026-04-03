# API: Команды и события

Matching engine предоставляет два API:
1. **Бинарный TCP-протокол** — торговые операции (порт 9090)
2. **HTTP REST API** — мониторинг и администрирование (порт 9091)

---

## Бинарный протокол: Команды (Клиент → Движок)

Все команды отправляются в бинарных кадрах с 14-байтовым заголовком.
Подробный формат кадра см. в [Бинарный протокол](./protocol.md).

### PlaceOrder — Разместить ордер

Код: `0x01` | Нагрузка: 37 байт

```
Поле        Тип      Описание
---------   -------  --------
order_id    u64 LE   Уникальный ID ордера (назначает клиент)
symbol_id   u16 LE   ID торговой пары
side        u8       Buy=0, Sell=1
order_type  u8       Limit=0, Market=1, StopLimit=2, StopMarket=3
tif         u8       GTC=0, IOC=1, FOK=2
price       i64 LE   Лимитная цена (fixed-point ×10^8; 0 для Market)
stop_price  i64 LE   Стоп-цена (fixed-point ×10^8; 0 для не-стоп)
qty         u64 LE   Количество (fixed-point ×10^8)
```

Правила:
- `order_id` должен быть уникальным в рамках соединения.
- Для рыночных ордеров `price = 0`.
- Для не-стоп ордеров `stop_price = 0`.
- FOK: если недостаточно ликвидности — полный отказ без частичных исполнений.
- IOC: неисполненный остаток немедленно отменяется.
- GTC: неисполненный остаток остаётся в книге.

### CancelOrder — Отменить ордер

Код: `0x02` | Нагрузка: 10 байт

```
Поле        Тип      Описание
---------   -------  --------
order_id    u64 LE   ID ордера для отмены
symbol_id   u16 LE   ID торговой пары
```

- Работает для ордеров в основной книге и в стоп-книге.
- Отмена разрешена даже если торговля по инструменту приостановлена (halt).

### AmendOrder — Изменить ордер

Код: `0x03` | Нагрузка: 12–28 байт (переменная)

```
Поле        Тип      Описание
---------   -------  --------
order_id    u64 LE   ID ордера для изменения
symbol_id   u16 LE   ID торговой пары
has_price   u8       1 = далее следует новая цена, 0 = без изменения цены
[price]     i64 LE   [условно] Новая цена (только если has_price=1)
has_qty     u8       1 = далее следует новое количество, 0 = без изменения
[qty]       u64 LE   [условно] Новое количество (только если has_qty=1)
```

Варианты нагрузки:
- Ни цена, ни qty: 12 байт (order_id=8 + symbol_id=2 + has_price=1 + has_qty=1)
- Только цена: 21 байт
- Только qty: 21 байт
- Цена и qty: 28 байт

Важно: изменение **только qty вниз** — сохраняет приоритет по времени.
Изменение цены или увеличение qty — теряет приоритет по времени (cancel + re-insert).

### Ping — Heartbeat

Код: `0x04` | Нагрузка: 0 байт

Можно отправлять в любом состоянии соединения, включая до handshake.
Движок всегда отвечает Pong (0x18).

### Handshake — Инициализация соединения

Код: `0x05` | Нагрузка: 0 или 1 байт

```
Поле            Тип  Описание
-----------     ---  --------
client_version  u8   [необязательно] Версия протокола клиента (должна быть 1)
```

**Должен быть первым кадром** (кроме Ping) после TCP-соединения.
Ответ от движка: HandshakeAck (0x1A).

---

## Бинарный протокол: События (Движок → Клиент)

### OrderAccepted — Ордер принят

Код: `0x10` | Нагрузка: 16 байт

```
Поле       Тип      Описание
---------  -------  --------
order_id   u64 LE   ID принятого ордера
timestamp  u64 LE   Временная метка сервера (наносекунды от эпохи Unix)
```

Получение этого события означает, что ордер прошёл все проверки и принят в движок.
Ордер ещё не обязательно исполнен — он может ожидать в книге.

### OrderRejected — Ордер отклонён

Код: `0x11` | Нагрузка: 17 байт

```
Поле           Тип      Описание
-----------    -------  --------
order_id       u64 LE   ID отклонённого ордера
timestamp      u64 LE   Временная метка сервера (нс)
reject_reason  u8       Причина отклонения (см. RejectReason ниже)
```

| Значение | RejectReason          | Описание                               |
|----------|-----------------------|----------------------------------------|
| 0        | NoLiquidity           | Нет доступной ликвидности              |
| 1        | InsufficientLiquidity | Недостаточно ликвидности для FOK       |
| 2        | InvalidPrice          | Некорректная цена                      |
| 3        | InvalidQuantity       | Некорректное количество                |
| 4        | InvalidLotSize        | Нарушение шага лота                    |
| 5        | InvalidTickSize       | Нарушение шага цены                    |
| 6        | OrderNotFound         | Ордер не найден (для cancel/amend)     |
| 7        | DuplicateOrderId      | Дублирующийся ID ордера                |
| 8        | SymbolNotFound        | Неизвестный ID инструмента             |
| 9        | TradingHalted         | Торговля по инструменту приостановлена |
| 10       | RiskCheckFailed       | Проверка pre-trade риска не пройдена   |

### Trade — Сделка

Код: `0x12` | Нагрузка: 51 байт

```
Поле            Тип      Описание
-----------     -------  --------
trade_id        u64 LE   Уникальный ID сделки
symbol_id       u16 LE   ID инструмента
price           i64 LE   Цена сделки (fixed-point ×10^8)
qty             u64 LE   Объём сделки (fixed-point ×10^8)
maker_order_id  u64 LE   ID ордера мейкера
taker_order_id  u64 LE   ID ордера тейкера
maker_side      u8       Сторона мейкера (Buy=0, Sell=1)
timestamp       u64 LE   Временная метка сервера (нс)
```

Trade отправляется **обоим** участникам сделки (мейкеру и тейкеру).

### OrderFilled — Ордер полностью исполнен

Код: `0x13` | Нагрузка: 32 байта

```
Поле        Тип      Описание
---------   -------  --------
order_id    u64 LE   ID исполненного ордера
filled_qty  u64 LE   Суммарное исполненное количество (fixed-point ×10^8)
price       i64 LE   Цена последнего исполнения (fixed-point ×10^8)
timestamp   u64 LE   Временная метка сервера (нс)
```

Терминальное событие: после OrderFilled ордер не может получить других событий.

### OrderPartiallyFilled — Ордер частично исполнен

Код: `0x14` | Нагрузка: 40 байт

```
Поле           Тип      Описание
-----------    -------  --------
order_id       u64 LE   ID ордера
filled_qty     u64 LE   Исполнено в этом событии (fixed-point ×10^8)
remaining_qty  u64 LE   Остаток в книге (fixed-point ×10^8)
price          i64 LE   Цена исполнения (fixed-point ×10^8)
timestamp      u64 LE   Временная метка сервера (нс)
```

Ордер остаётся в книге и может получить последующие события.

### OrderCancelled — Ордер отменён

Код: `0x15` | Нагрузка: 24 байта

```
Поле           Тип      Описание
-----------    -------  --------
order_id       u64 LE   ID отменённого ордера
remaining_qty  u64 LE   Остаток на момент отмены (fixed-point ×10^8)
timestamp      u64 LE   Временная метка сервера (нс)
```

Отправляется как при явной отмене через CancelOrder, так и при автоматической
отмене остатка IOC/FOK ордера.

### OrderAmended — Ордер изменён

Код: `0x16` | Нагрузка: 32 байта

```
Поле       Тип      Описание
---------  -------  --------
order_id   u64 LE   ID изменённого ордера
new_price  i64 LE   Новая цена после изменения (fixed-point ×10^8)
new_qty    u64 LE   Новое количество после изменения (fixed-point ×10^8)
timestamp  u64 LE   Временная метка сервера (нс)
```

### StopTriggered — Стоп-ордер сработал

Код: `0x17` | Нагрузка: 24 байта

```
Поле           Тип      Описание
-----------    -------  --------
order_id       u64 LE   ID сработавшего стоп-ордера
trigger_price  i64 LE   Цена срабатывания (fixed-point ×10^8)
timestamp      u64 LE   Временная метка сервера (нс)
```

После StopTriggered ордер конвертируется (StopLimit → Limit, StopMarket → Market)
и немедленно обрабатывается матчинговым движком.

### HandshakeAck, Pong, Error

- **HandshakeAck (0x1A)**: 1 байт нагрузки — `server_version: u8`.
- **Pong (0x18)**: 0 байт нагрузки — ответ на Ping.
- **Error (0x19)**: переменная нагрузка — `error_code: u8` + опциональное сообщение.

| Значение | ErrorCode            | Описание                                |
|----------|----------------------|-----------------------------------------|
| 1        | UnsupportedVersion   | Неподдерживаемая версия протокола       |
| 2        | SequenceOutOfOrder   | Порядковый номер нарушен               |
| 3        | HandshakeRequired    | Требуется handshake перед командами     |
| 4        | InvalidPayload       | Некорректная нагрузка кадра            |
| 5        | ServerShuttingDown   | Сервер завершает работу                 |
| 6        | Backpressure         | Переполнение буфера записи (256 КБ)    |

После Error-кадра с кодами 1–4 соединение закрывается.

---

## HTTP REST API

HTTP API доступен на порту 9091 (по умолчанию `0.0.0.0:9091`).

### Публичные эндпоинты

#### GET /health

Проверка работоспособности. Аутентификация не требуется.

```bash
curl http://localhost:9091/health
```

Ответ `200 OK`:
```
OK
```

#### GET /metrics

Метрики Prometheus в текстовом формате. Аутентификация не требуется.

```bash
curl http://localhost:9091/metrics
```

Ответ `200 OK`:
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

### Административные эндпоинты

Требуют заголовок `X-Admin-Key`, если `server.admin.api_key` задан в конфиге.
Без API-ключа в конфиге эндпоинты открыты (только для разработки).

```toml
[server.admin]
api_key = "your-secret-key"
```

#### GET /admin/status

```bash
curl -H "X-Admin-Key: your-secret-key" http://localhost:9091/admin/status
```

Ответ `200 OK`:
```json
{"status": "running", "message": "Matching engine is operational"}
```

#### POST /admin/halt/{symbol_id}

Приостановить торги по инструменту. Новые ордера отклоняются (кроме отмен).
Существующие ордера остаются в книге.

```bash
curl -X POST -H "X-Admin-Key: your-secret-key" http://localhost:9091/admin/halt/0
```

Ответ `200 OK` (инструмент найден):
```json
{"symbol_id": 0, "action": "halt", "success": true}
```

Ответ `404 Not Found`:
```json
{"symbol_id": 999, "action": "halt", "success": false}
```

#### POST /admin/resume/{symbol_id}

Возобновить торги по ранее приостановленному инструменту.

```bash
curl -X POST -H "X-Admin-Key: your-secret-key" http://localhost:9091/admin/resume/0
```

Ответ `200 OK`:
```json
{"symbol_id": 0, "action": "resume", "success": true}
```

---

## Справочник метрик Prometheus

Все метрики используют префикс `me_`.

### Обработка ордеров

| Метрика                              | Тип       | Метки                            |
|--------------------------------------|-----------|----------------------------------|
| `me_orders_total`                    | Counter   | side, order_type, tif, symbol   |
| `me_orders_rejected_total`           | Counter   | reason, symbol                   |
| `me_orders_accepted_total`           | Counter   | side, order_type, symbol         |
| `me_order_processing_duration_seconds` | Histogram | order_type                     |

### Матчинг

| Метрика                     | Тип     | Метки   |
|-----------------------------|---------|---------|
| `me_trades_total`           | Counter | symbol  |
| `me_trades_volume_total`    | Gauge   | symbol  |
| `me_trade_price`            | Gauge   | symbol  |
| `me_fills_total`            | Counter | fill_type |

### Стакан ордеров

| Метрика                   | Тип   | Описание                     |
|---------------------------|-------|------------------------------|
| `me_book_bid_levels`      | Gauge | Количество ценовых уровней bid |
| `me_book_ask_levels`      | Gauge | Количество ценовых уровней ask |
| `me_book_bid_orders`      | Gauge | Покоящихся ордеров bid        |
| `me_book_ask_orders`      | Gauge | Покоящихся ордеров ask        |
| `me_book_spread`          | Gauge | Спред (best_ask - best_bid)   |
| `me_book_best_bid`        | Gauge | Лучшая цена bid               |
| `me_book_best_ask`        | Gauge | Лучшая цена ask               |
| `me_book_depth_bid_qty`   | Gauge | Суммарный объём bid           |
| `me_book_depth_ask_qty`   | Gauge | Суммарный объём ask           |

### Отмены и изменения

| Метрика                  | Тип     | Метки             |
|--------------------------|---------|-------------------|
| `me_cancels_total`       | Counter | symbol            |
| `me_cancels_failed_total`| Counter | symbol            |
| `me_amends_total`        | Counter | amend_type, symbol |

### Стоп-ордера

| Метрика                    | Тип     | Метки  |
|----------------------------|---------|--------|
| `me_stop_orders_pending`   | Gauge   | symbol |
| `me_stop_triggers_total`   | Counter | symbol |

### Соединения

| Метрика                       | Тип     | Метки     |
|-------------------------------|---------|-----------|
| `me_connections_total`        | Counter | transport |
| `me_connections_active`       | Gauge   | transport |
| `me_connection_bytes_in_total`| Counter | transport |
| `me_connection_bytes_out_total`| Counter | transport |
| `me_connection_frames_in_total`| Counter | transport |
| `me_connection_frames_out_total`| Counter | transport |
| `me_protocol_errors_total`    | Counter | error_type |

### Производительность и шарды

| Метрика                             | Тип       | Метки        |
|-------------------------------------|-----------|--------------|
| `me_matching_duration_seconds`      | Histogram | command_type |
| `me_shard_queue_depth`              | Gauge     | shard        |
| `me_shard_commands_total`           | Counter   | shard        |
| `me_stop_cascade_depth`             | Histogram | —            |

### WAL и снапшоты

| Метрика                        | Тип       | Описание                 |
|--------------------------------|-----------|--------------------------|
| `me_wal_writes_total`          | Counter   | Операции записи WAL      |
| `me_wal_bytes_written_total`   | Counter   | Байт записано в WAL      |
| `me_wal_flush_duration_seconds`| Histogram | Длительность flush WAL   |
| `me_snapshot_saves_total`      | Counter   | Сохранения снапшотов     |
| `me_snapshot_duration_seconds` | Histogram | Длительность снапшота    |
| `me_snapshot_size_bytes`       | Gauge     | Размер снапшота в байтах |

---

## Жизненный цикл ордера — диаграмма состояний

```
                     PlaceOrder
                         |
                         v
                  +-------------+
                  | OrderAccepted|
                  +------+------+
                         |
          +--------------+--------------+
          |              |              |
          v              v              v
   +-----------+  +----------+  +-------------+
   |OrderFilled|  |StopBook  |  |Resting in   |
   |(terminal) |  |(если стоп)|  |OrderBook    |
   +-----------+  +-----+----+  +------+------+
                        |              |
                        | trigger      | новый ордер
                        v              | приходит и
                 StopTriggered         | matches
                        |              |
                        v              v
                  процессируется   OrderPartiallyFilled
                  как обычный      (остаток в книге)
                  ордер                  |
                                         | ещё матчинг
                                         v
                                    OrderFilled
                                    (terminal)
                                         |
                                 или CancelOrder
                                         v
                                   OrderCancelled
                                    (terminal)
```

Терминальные события: **OrderFilled**, **OrderRejected**, **OrderCancelled**.
После терминального события ордер больше не может получать события.
