# Конфигурация

Конфигурация хранится в TOML-файле. По умолчанию движок ищет `config.toml` в рабочей директории. Путь задаётся флагом `--config`.

---

## Секция `[server]`

```toml
[server]
tcp_addr     = "0.0.0.0:9090"
metrics_addr = "0.0.0.0:9091"
shards       = 0
# uds_path   = "/tmp/me.sock"  # опционально
```

| Поле | Тип | По умолчанию | Описание |
|------|-----|-------------|----------|
| `tcp_addr` | string | `"0.0.0.0:9090"` | Адрес TCP-сервера для клиентских подключений |
| `metrics_addr` | string | `"0.0.0.0:9091"` | Адрес HTTP-сервера для Prometheus метрик и admin API |
| `shards` | u32 | `0` | Количество шардов движка. `0` = авто (по числу CPU) |
| `uds_path` | string | — | Путь к Unix-сокету (опционально, вместо или вместе с TCP) |

### Подсекция `[server.admin]`

```toml
[server.admin]
api_key = "secret-key"
```

| Поле | Тип | Описание |
|------|-----|----------|
| `api_key` | string | API-ключ для admin-эндпоинтов (`/admin/...`). Пустая строка — без аутентификации |

### Подсекция `[server.shutdown]`

```toml
[server.shutdown]
drain_timeout_secs = 30
```

| Поле | Тип | Описание |
|------|-----|----------|
| `drain_timeout_secs` | u64 | Секунды ожидания drain при graceful shutdown |

---

## Секция `[[symbols]]`

Каждая запись `[[symbols]]` — один торговый инструмент. Записей может быть несколько.

```toml
[[symbols]]
id          = 0
base        = "BTC"
quote       = "USDT"
price_scale = 2
qty_scale   = 8
min_qty     = "0.00001"
max_qty     = "10000.0"
qty_step    = "0.00001"
price_step  = "0.01"
max_orders_per_second = 100
```

### Поля символа

| Поле | Тип | Обязательно | Описание |
|------|-----|------------|----------|
| `id` | u32 | да | Уникальный числовой идентификатор символа. Используется в протоколе |
| `base` | string | да | Базовая валюта (покупаемый актив), например `"BTC"` |
| `quote` | string | да | Котируемая валюта (в чём измеряется цена), например `"USDT"` |
| `price_scale` | u8 | да | Количество знаков после запятой для цены (fixed-point) |
| `qty_scale` | u8 | да | Количество знаков после запятой для количества (fixed-point) |
| `min_qty` | string | да | Минимальное количество в ордере (в единицах базовой валюты) |
| `max_qty` | string | да | Максимальное количество в ордере |
| `qty_step` | string | да | Минимальный шаг изменения количества (лот) |
| `price_step` | string | да | Минимальный шаг изменения цены (тик) |
| `max_orders_per_second` | u32 | нет | Rate limit для одного клиента по этому символу. `0` = без ограничений |

---

## Fixed-Point кодирование

Движок хранит цены и количества как `i64` с фиксированной точкой. Реальное значение:

```
real_value = raw_i64 / 10^scale
```

**Примеры:**

| Актив | price_scale | Реальная цена | raw i64 |
|-------|-------------|---------------|---------|
| BTC/USDT | 2 | $50 000.00 | 5 000 000 |
| ETH/USDT | 2 | $3 200.50 | 320 050 |
| SOL/USDT | 4 | $145.2500 | 1 452 500 |
| USDC/USDT | 6 | $1.000100 | 1 000 100 |

| Актив | qty_scale | Реальное кол-во | raw i64 |
|-------|-----------|-----------------|---------|
| BTC | 8 | 1.50000000 BTC | 150 000 000 |
| ETH | 8 | 10.00000000 ETH | 1 000 000 000 |
| Мем-коин | 8 | 1 000 000.00000000 | 100 000 000 000 000 |

> Значения передаются в протоколе в little-endian. Подробности — в разделе [Бинарный протокол](./protocol.md).

---

## Полный пример конфига

```toml
[server]
tcp_addr     = "0.0.0.0:9090"
metrics_addr = "0.0.0.0:9091"
shards       = 4

[server.admin]
api_key = "my-admin-secret"

[server.shutdown]
drain_timeout_secs = 30

[[symbols]]
id          = 0
base        = "BTC"
quote       = "USDT"
price_scale = 2
qty_scale   = 8
min_qty     = "0.00001"
max_qty     = "10000.0"
qty_step    = "0.00001"
price_step  = "0.01"
max_orders_per_second = 200

[[symbols]]
id          = 1
base        = "ETH"
quote       = "USDT"
price_scale = 2
qty_scale   = 8
min_qty     = "0.0001"
max_qty     = "100000.0"
qty_step    = "0.0001"
price_step  = "0.01"
max_orders_per_second = 500
```
