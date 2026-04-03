# Deployment Guide / Руководство по развертыванию

---

## English

### Building

```bash
cargo build --release -p me-server
# Binary at target/release/me-server
```

The release build enables all compiler optimizations critical for low-latency matching. Debug builds are significantly slower and should not be used in production.

### Configuration

The server accepts a TOML config file via the `--config` flag. CLI arguments override values from the config file. If no config is provided, built-in defaults are used (BTC/USDT, ETH/USDT, SOL/USDT).

#### CLI Arguments

| Argument | Short | Description | Default |
|---|---|---|---|
| `--config` | `-c` | Path to TOML config file | None (uses built-in defaults) |
| `--tcp-addr` | | TCP listen address | `0.0.0.0:9090` |
| `--uds-path` | | Unix domain socket path | None (disabled) |
| `--shards` | | Number of engine shards (0 = auto) | `0` |
| `--metrics-addr` | | Metrics HTTP address | `0.0.0.0:9091` |

When `--shards 0` is specified (or omitted), the engine auto-detects the number of available CPU cores and uses that as the shard count.

#### TOML Config Reference

Full `config.toml` example with all fields documented:

```toml
[server]
# TCP address for the binary wire protocol (order submission).
tcp_addr = "0.0.0.0:9090"

# HTTP address for Prometheus metrics, health checks, and admin API.
metrics_addr = "0.0.0.0:9091"

# Number of engine shards. Each shard runs its own worker thread.
# 0 = auto-detect based on available CPU cores.
shards = 0

# Optional Unix domain socket path for local low-latency connections.
# Uncomment to enable.
# uds_path = "/tmp/me.sock"

[server.admin]
# API key for admin endpoints. Empty string = no authentication required.
# Set this in production to protect halt/resume endpoints.
api_key = ""

[server.shutdown]
# Seconds to wait for existing connections to finish during graceful shutdown.
drain_timeout_secs = 30

# ─── Trading Pairs ──────────────────────────────────────────────────────────
# Each [[symbols]] block defines a trading pair.

[[symbols]]
id = 0                  # Unique numeric identifier (u16)
base = "BTC"            # Base asset
quote = "USDT"          # Quote asset
price_scale = 2         # Decimal places for price display
qty_scale = 8           # Decimal places for quantity display
min_qty = "0.00001"     # Minimum order quantity
max_qty = "10000.0"     # Maximum order quantity
qty_step = "0.00001"    # Quantity increment step
price_step = "0.01"     # Price increment step (tick size)

[[symbols]]
id = 1
base = "ETH"
quote = "USDT"
price_scale = 2
qty_scale = 8
min_qty = "0.0001"
max_qty = "100000.0"
qty_step = "0.0001"
price_step = "0.01"

[[symbols]]
id = 2
base = "SOL"
quote = "USDT"
price_scale = 4
qty_scale = 8
min_qty = "0.001"
max_qty = "100000.0"
qty_step = "0.001"
price_step = "0.0001"
```

All decimal values use a fixed-point representation internally with 8 decimal places (10^8 scale). Maximum fractional precision is 8 digits.

### Running

```bash
# With built-in defaults (BTC/USDT, ETH/USDT, SOL/USDT)
./target/release/me-server

# With config file
./target/release/me-server --config config.toml

# Override specific settings via CLI
./target/release/me-server --config config.toml --tcp-addr 127.0.0.1:9090 --shards 8

# Enable UDS alongside TCP
./target/release/me-server --config config.toml --uds-path /tmp/me.sock

# Adjust log level via RUST_LOG
RUST_LOG=me_server=debug ./target/release/me-server --config config.toml
```

### Docker

#### Dockerfile

The project includes a multi-stage Dockerfile that produces a minimal runtime image:

```dockerfile
# Stage 1: Build
FROM rust:latest AS builder
WORKDIR /usr/src/matching-engine
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo build --release -p me-server

# Stage 2: Runtime
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/src/matching-engine/target/release/me-server /usr/local/bin/me-server
EXPOSE 9090 9091
CMD ["me-server"]
```

Build and run:

```bash
docker build -t matching-engine .
docker run -p 9090:9090 -p 9091:9091 matching-engine
```

Mount a config file:

```bash
docker run -p 9090:9090 -p 9091:9091 \
  -v $(pwd)/config.toml:/etc/me/config.toml \
  matching-engine me-server --config /etc/me/config.toml
```

#### Docker Compose with Prometheus and Grafana

```yaml
# docker-compose.yml
version: "3.8"

services:
  matching-engine:
    build: .
    ports:
      - "9090:9090"   # Trading protocol
      - "9091:9091"   # Metrics + Admin API
    volumes:
      - ./config.toml:/etc/me/config.toml:ro
    command: ["me-server", "--config", "/etc/me/config.toml"]
    restart: unless-stopped

  prometheus:
    image: prom/prometheus:latest
    ports:
      - "9092:9090"
    volumes:
      - ./prometheus.yml:/etc/prometheus/prometheus.yml:ro
      - prometheus-data:/prometheus
    restart: unless-stopped

  grafana:
    image: grafana/grafana:latest
    ports:
      - "3000:3000"
    volumes:
      - grafana-data:/var/lib/grafana
    environment:
      - GF_SECURITY_ADMIN_PASSWORD=admin
    restart: unless-stopped

volumes:
  prometheus-data:
  grafana-data:
```

Prometheus configuration (`prometheus.yml`):

```yaml
global:
  scrape_interval: 5s

scrape_configs:
  - job_name: "matching-engine"
    static_configs:
      - targets: ["matching-engine:9091"]
    metrics_path: /metrics
```

### Monitoring

The engine exposes Prometheus-format metrics on the HTTP server (default port 9091).

**Endpoints:**

| Path | Description |
|---|---|
| `/health` | Health check, returns `OK` (200) |
| `/metrics` | Prometheus metrics (text format) |

**Key metrics to monitor:**

| Metric | Type | Description |
|---|---|---|
| `me_order_processing_duration_seconds` | Histogram | Order processing latency (by order_type) |
| `me_connections_active` | Gauge | Active client connections (by transport) |
| `me_book_spread` | Gauge | Current bid-ask spread (by symbol) |
| `me_trades_total` | Counter | Total executed trades (by symbol) |
| `me_orders_total` | Counter | Total orders received (by side, order_type, tif, symbol) |
| `me_orders_rejected_total` | Counter | Rejected orders (by reason, symbol) |
| `me_shard_queue_depth` | Gauge | Pending commands per shard |

**Recommended alert rules (Prometheus):**

```yaml
groups:
  - name: matching-engine
    rules:
      # P99 latency exceeds 1ms
      - alert: HighOrderLatency
        expr: histogram_quantile(0.99, rate(me_order_processing_duration_seconds_bucket[5m])) > 0.001
        for: 2m
        labels:
          severity: warning
        annotations:
          summary: "Order processing P99 latency above 1ms"

      # Error rate (rejections) above 5%
      - alert: HighRejectRate
        expr: rate(me_orders_rejected_total[5m]) / rate(me_orders_total[5m]) > 0.05
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "Order rejection rate exceeds 5%"

      # Shard queue depth growing
      - alert: ShardQueueBacklog
        expr: me_shard_queue_depth > 1000
        for: 1m
        labels:
          severity: critical
        annotations:
          summary: "Shard queue depth exceeds 1000, possible processing bottleneck"

      # No active connections
      - alert: NoConnections
        expr: sum(me_connections_active) == 0
        for: 5m
        labels:
          severity: info
        annotations:
          summary: "No active client connections"
```

### Graceful Shutdown

The server handles `SIGTERM` and `SIGINT` (Ctrl+C) for graceful shutdown:

1. Signal is received.
2. The server stops accepting new connections.
3. Existing connections are allowed to finish within the drain timeout.
4. After the drain timeout (default: 30 seconds), remaining connections are closed.
5. The process exits.

The drain timeout is configurable:

```toml
[server.shutdown]
drain_timeout_secs = 30
```

### Production Recommendations

- **Always use `--release` builds.** Debug builds have orders-of-magnitude higher latency.
- **Set appropriate shard count.** Guideline: 1 shard per 2-3 trading pairs. With `shards = 0`, the engine uses all available CPU cores.
- **Configure an admin API key.** Without it, halt/resume endpoints are open to anyone who can reach the metrics port.
- **Run behind a firewall.** The engine does not implement TLS. Use a reverse proxy or network-level isolation to secure connections.
- **Monitor with Prometheus + Grafana.** Use the alert rules above as a starting point. Track P99 latency and shard queue depth closely.
- **Separate the metrics port from the trading port.** The trading protocol runs on TCP port 9090; metrics and admin API run on HTTP port 9091. Firewall these independently.
- **Use UDS for co-located clients.** If your trading gateway runs on the same machine, configure `uds_path` for lower latency than TCP loopback.
- **Set `RUST_LOG` for diagnostics.** In production, `me_server=info` (default) is appropriate. Use `me_server=debug` temporarily to troubleshoot issues.

---

## Русский

### Сборка

```bash
cargo build --release -p me-server
# Бинарный файл: target/release/me-server
```

Релизная сборка включает все оптимизации компилятора, критичные для низколатентного матчинга. Debug-сборки значительно медленнее и не должны использоваться в продакшене.

### Конфигурация

Сервер принимает конфигурационный файл TOML через флаг `--config`. Аргументы командной строки имеют приоритет над значениями из файла конфигурации. Если конфигурация не указана, используются встроенные значения по умолчанию (BTC/USDT, ETH/USDT, SOL/USDT).

#### Аргументы командной строки

| Аргумент | Сокр. | Описание | По умолчанию |
|---|---|---|---|
| `--config` | `-c` | Путь к TOML-файлу конфигурации | Нет (встроенные значения) |
| `--tcp-addr` | | Адрес TCP для приема соединений | `0.0.0.0:9090` |
| `--uds-path` | | Путь к Unix domain socket | Нет (отключено) |
| `--shards` | | Количество шардов движка (0 = авто) | `0` |
| `--metrics-addr` | | HTTP-адрес для метрик | `0.0.0.0:9091` |

При `--shards 0` (или если параметр опущен) движок автоматически определяет количество доступных ядер CPU и использует его как количество шардов.

#### Описание TOML-конфигурации

Полный пример `config.toml` с описанием всех полей:

```toml
[server]
# TCP-адрес для бинарного протокола (прием ордеров).
tcp_addr = "0.0.0.0:9090"

# HTTP-адрес для метрик Prometheus, проверки здоровья и админ API.
metrics_addr = "0.0.0.0:9091"

# Количество шардов движка. Каждый шард работает в своем потоке.
# 0 = автоопределение по количеству ядер CPU.
shards = 0

# Путь к Unix domain socket для локальных низколатентных подключений.
# Раскомментируйте для включения.
# uds_path = "/tmp/me.sock"

[server.admin]
# API-ключ для админ-эндпоинтов. Пустая строка = аутентификация не требуется.
# Установите в продакшене для защиты эндпоинтов halt/resume.
api_key = ""

[server.shutdown]
# Время ожидания (сек.) завершения существующих соединений при остановке.
drain_timeout_secs = 30

# ─── Торговые пары ──────────────────────────────────────────────────────────
# Каждый блок [[symbols]] определяет торговую пару.

[[symbols]]
id = 0                  # Уникальный числовой идентификатор (u16)
base = "BTC"            # Базовый актив
quote = "USDT"          # Котировочный актив
price_scale = 2         # Знаков после запятой для цены
qty_scale = 8           # Знаков после запятой для количества
min_qty = "0.00001"     # Минимальное количество в ордере
max_qty = "10000.0"     # Максимальное количество в ордере
qty_step = "0.00001"    # Шаг изменения количества
price_step = "0.01"     # Шаг цены (тик)

[[symbols]]
id = 1
base = "ETH"
quote = "USDT"
price_scale = 2
qty_scale = 8
min_qty = "0.0001"
max_qty = "100000.0"
qty_step = "0.0001"
price_step = "0.01"

[[symbols]]
id = 2
base = "SOL"
quote = "USDT"
price_scale = 4
qty_scale = 8
min_qty = "0.001"
max_qty = "100000.0"
qty_step = "0.001"
price_step = "0.0001"
```

Все десятичные значения внутренне используют фиксированную точку с 8 десятичными знаками (масштаб 10^8). Максимальная точность дробной части - 8 цифр.

### Запуск

```bash
# Со встроенными значениями по умолчанию (BTC/USDT, ETH/USDT, SOL/USDT)
./target/release/me-server

# С файлом конфигурации
./target/release/me-server --config config.toml

# Переопределение отдельных параметров через CLI
./target/release/me-server --config config.toml --tcp-addr 127.0.0.1:9090 --shards 8

# Включение UDS параллельно с TCP
./target/release/me-server --config config.toml --uds-path /tmp/me.sock

# Настройка уровня логирования через RUST_LOG
RUST_LOG=me_server=debug ./target/release/me-server --config config.toml
```

### Docker

#### Dockerfile

Проект включает многоэтапный Dockerfile, создающий минимальный runtime-образ:

```dockerfile
# Этап 1: Сборка
FROM rust:latest AS builder
WORKDIR /usr/src/matching-engine
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo build --release -p me-server

# Этап 2: Runtime
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/src/matching-engine/target/release/me-server /usr/local/bin/me-server
EXPOSE 9090 9091
CMD ["me-server"]
```

Сборка и запуск:

```bash
docker build -t matching-engine .
docker run -p 9090:9090 -p 9091:9091 matching-engine
```

Монтирование файла конфигурации:

```bash
docker run -p 9090:9090 -p 9091:9091 \
  -v $(pwd)/config.toml:/etc/me/config.toml \
  matching-engine me-server --config /etc/me/config.toml
```

#### Docker Compose с Prometheus и Grafana

```yaml
# docker-compose.yml
version: "3.8"

services:
  matching-engine:
    build: .
    ports:
      - "9090:9090"   # Торговый протокол
      - "9091:9091"   # Метрики + Админ API
    volumes:
      - ./config.toml:/etc/me/config.toml:ro
    command: ["me-server", "--config", "/etc/me/config.toml"]
    restart: unless-stopped

  prometheus:
    image: prom/prometheus:latest
    ports:
      - "9092:9090"
    volumes:
      - ./prometheus.yml:/etc/prometheus/prometheus.yml:ro
      - prometheus-data:/prometheus
    restart: unless-stopped

  grafana:
    image: grafana/grafana:latest
    ports:
      - "3000:3000"
    volumes:
      - grafana-data:/var/lib/grafana
    environment:
      - GF_SECURITY_ADMIN_PASSWORD=admin
    restart: unless-stopped

volumes:
  prometheus-data:
  grafana-data:
```

Конфигурация Prometheus (`prometheus.yml`):

```yaml
global:
  scrape_interval: 5s

scrape_configs:
  - job_name: "matching-engine"
    static_configs:
      - targets: ["matching-engine:9091"]
    metrics_path: /metrics
```

### Мониторинг

Движок экспортирует метрики в формате Prometheus на HTTP-сервере (порт 9091 по умолчанию).

**Эндпоинты:**

| Путь | Описание |
|---|---|
| `/health` | Проверка здоровья, возвращает `OK` (200) |
| `/metrics` | Метрики Prometheus (текстовый формат) |

**Ключевые метрики для мониторинга:**

| Метрика | Тип | Описание |
|---|---|---|
| `me_order_processing_duration_seconds` | Histogram | Латентность обработки ордеров (по order_type) |
| `me_connections_active` | Gauge | Активные клиентские соединения (по transport) |
| `me_book_spread` | Gauge | Текущий спред (по symbol) |
| `me_trades_total` | Counter | Общее количество сделок (по symbol) |
| `me_orders_total` | Counter | Общее количество ордеров (по side, order_type, tif, symbol) |
| `me_orders_rejected_total` | Counter | Отклоненные ордера (по reason, symbol) |
| `me_shard_queue_depth` | Gauge | Глубина очереди команд на шард |

**Рекомендуемые правила оповещений (Prometheus):**

```yaml
groups:
  - name: matching-engine
    rules:
      # P99 латентность превышает 1мс
      - alert: HighOrderLatency
        expr: histogram_quantile(0.99, rate(me_order_processing_duration_seconds_bucket[5m])) > 0.001
        for: 2m
        labels:
          severity: warning
        annotations:
          summary: "P99 латентность обработки ордеров выше 1мс"

      # Частота ошибок (отклонений) выше 5%
      - alert: HighRejectRate
        expr: rate(me_orders_rejected_total[5m]) / rate(me_orders_total[5m]) > 0.05
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "Частота отклонения ордеров превышает 5%"

      # Рост глубины очереди шарда
      - alert: ShardQueueBacklog
        expr: me_shard_queue_depth > 1000
        for: 1m
        labels:
          severity: critical
        annotations:
          summary: "Глубина очереди шарда превышает 1000, возможное узкое место"

      # Нет активных соединений
      - alert: NoConnections
        expr: sum(me_connections_active) == 0
        for: 5m
        labels:
          severity: info
        annotations:
          summary: "Нет активных клиентских соединений"
```

### Корректная остановка (Graceful Shutdown)

Сервер обрабатывает сигналы `SIGTERM` и `SIGINT` (Ctrl+C) для корректной остановки:

1. Получен сигнал остановки.
2. Сервер прекращает прием новых соединений.
3. Существующие соединения могут завершить работу в пределах таймаута.
4. По истечении таймаута (по умолчанию 30 секунд) оставшиеся соединения закрываются.
5. Процесс завершается.

Таймаут ожидания настраивается:

```toml
[server.shutdown]
drain_timeout_secs = 30
```

### Рекомендации для продакшена

- **Всегда используйте `--release` сборку.** Debug-сборки имеют на порядки большую латентность.
- **Установите оптимальное количество шардов.** Ориентир: 1 шард на 2-3 торговые пары. При `shards = 0` движок использует все доступные ядра CPU.
- **Настройте API-ключ админки.** Без него эндпоинты halt/resume доступны любому, кто может достучаться до порта метрик.
- **Запускайте за файрволом.** Движок не реализует TLS. Используйте обратный прокси или сетевую изоляцию для защиты соединений.
- **Мониторьте через Prometheus + Grafana.** Используйте правила оповещений выше как отправную точку. Отслеживайте P99 латентность и глубину очередей шардов.
- **Разделяйте порт метрик и торговый порт.** Торговый протокол работает на TCP порту 9090; метрики и админ API - на HTTP порту 9091. Настраивайте файрвол для них раздельно.
- **Используйте UDS для локальных клиентов.** Если торговый шлюз работает на той же машине, настройте `uds_path` для более низкой латентности по сравнению с TCP loopback.
- **Настройте `RUST_LOG` для диагностики.** В продакшене уровень `me_server=info` (по умолчанию) подходит. Используйте `me_server=debug` временно для устранения проблем.
