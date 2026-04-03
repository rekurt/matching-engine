# Деплой

---

## Сборка

```bash
cargo build --release -p me-server
# Бинарный файл: target/release/me-server
```

Release-сборка включает все оптимизации компилятора, критичные для низколатентного
матчинга. Debug-сборки значительно медленнее и **не должны использоваться в продакшене**.

---

## Конфигурация

Сервер принимает конфигурационный файл TOML через флаг `--config`.
Аргументы командной строки имеют приоритет над значениями из файла.
Без конфигурации используются встроенные значения по умолчанию (BTC/USDT, ETH/USDT, SOL/USDT).

### Аргументы командной строки

| Аргумент        | Сокр. | Описание                               | По умолчанию    |
|-----------------|-------|----------------------------------------|-----------------|
| `--config`      | `-c`  | Путь к TOML-файлу конфигурации         | встроенные знач. |
| `--tcp-addr`    |       | TCP-адрес для приёма соединений        | `0.0.0.0:9090`  |
| `--uds-path`    |       | Путь к Unix Domain Socket              | нет (отключено) |
| `--shards`      |       | Количество шардов движка (0 = авто)    | `0`             |
| `--metrics-addr`|       | HTTP-адрес для метрик и admin API      | `0.0.0.0:9091`  |

При `--shards 0` движок автоматически использует `std::thread::available_parallelism()`.

### Полный пример config.toml

```toml
[server]
# TCP-адрес для бинарного протокола (приём ордеров)
tcp_addr = "0.0.0.0:9090"

# HTTP-адрес для метрик Prometheus, health-check и admin API
metrics_addr = "0.0.0.0:9091"

# Количество шардов (0 = автоопределение по ядрам CPU)
shards = 0

# Unix Domain Socket для локальных низколатентных подключений
# Раскомментируйте для включения
# uds_path = "/tmp/me.sock"

[server.admin]
# API-ключ для admin эндпоинтов.
# Пустая строка = без аутентификации (только для разработки!)
api_key = ""

[server.shutdown]
# Время ожидания завершения соединений при остановке (секунды)
drain_timeout_secs = 30

# ─── Торговые пары ───────────────────────────────────────────────────────────
# Каждый блок [[symbols]] определяет торговую пару

[[symbols]]
id = 0                  # Уникальный ID (u16)
base = "BTC"
quote = "USDT"
price_scale = 2         # Знаков после запятой в цене
qty_scale = 8           # Знаков после запятой в количестве
min_qty = "0.00001"     # Минимальное количество в ордере
max_qty = "10000.0"     # Максимальное количество
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

Все числовые значения внутренне хранятся в fixed-point с 8 знаками (масштаб 10^8).

---

## Запуск

```bash
# Встроенные значения по умолчанию (BTC/USDT, ETH/USDT, SOL/USDT)
./target/release/me-server

# С файлом конфигурации
./target/release/me-server --config config.toml

# Переопределение отдельных параметров
./target/release/me-server --config config.toml --tcp-addr 127.0.0.1:9090 --shards 8

# Включение UDS параллельно с TCP
./target/release/me-server --config config.toml --uds-path /tmp/me.sock

# Настройка уровня логирования
RUST_LOG=me_server=debug ./target/release/me-server --config config.toml
```

---

## Docker

### Dockerfile (multi-stage)

```dockerfile
# Этап 1: Сборка
FROM rust:latest AS builder
WORKDIR /usr/src/matching-engine
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo build --release -p me-server

# Этап 2: Runtime (минимальный образ)
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/src/matching-engine/target/release/me-server /usr/local/bin/me-server
EXPOSE 9090 9091
CMD ["me-server"]
```

```bash
# Сборка образа
docker build -t matching-engine .

# Запуск
docker run -p 9090:9090 -p 9091:9091 matching-engine

# С файлом конфигурации
docker run -p 9090:9090 -p 9091:9091 \
  -v $(pwd)/config.toml:/etc/me/config.toml \
  matching-engine me-server --config /etc/me/config.toml
```

---

## Docker Compose с Prometheus и Grafana

Полный стек для разработки и продакшена:

```yaml
# docker-compose.yml
services:
  matching-engine:
    build: .
    ports:
      - "9090:9090"   # Торговый протокол (бинарный TCP)
      - "9091:9091"   # Метрики + Admin API (HTTP)
    volumes:
      - ./config.toml:/etc/me/config.toml:ro
    command: ["me-server", "--config", "/etc/me/config.toml"]
    restart: unless-stopped
    environment:
      - RUST_LOG=me_server=info

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
    depends_on:
      - prometheus

volumes:
  prometheus-data:
  grafana-data:
```

Конфигурация Prometheus (`prometheus.yml`):

```yaml
global:
  scrape_interval: 5s
  evaluation_interval: 5s

scrape_configs:
  - job_name: "matching-engine"
    static_configs:
      - targets: ["matching-engine:9091"]
    metrics_path: /metrics
```

Запуск стека:

```bash
docker compose up -d
# Grafana: http://localhost:3000 (admin/admin)
# Prometheus: http://localhost:9092
# Matching engine health: http://localhost:9091/health
```

---

## Мониторинг

Движок экспортирует метрики Prometheus на порту 9091.

### Ключевые метрики для мониторинга

| Метрика                              | Тип       | Описание                                   |
|--------------------------------------|-----------|--------------------------------------------|
| `me_order_processing_duration_seconds` | Histogram | Латентность обработки ордеров              |
| `me_connections_active`              | Gauge     | Активные клиентские соединения             |
| `me_book_spread`                     | Gauge     | Текущий спред по инструменту               |
| `me_trades_total`                    | Counter   | Количество сделок                          |
| `me_orders_total`                    | Counter   | Количество ордеров                         |
| `me_orders_rejected_total`           | Counter   | Отклонённые ордера                         |
| `me_shard_queue_depth`               | Gauge     | Глубина очереди команд на шард             |

### Рекомендуемые Prometheus-алерты

```yaml
groups:
  - name: matching-engine
    rules:
      # P99 латентность превышает 1 мс
      - alert: HighOrderLatency
        expr: histogram_quantile(0.99, rate(me_order_processing_duration_seconds_bucket[5m])) > 0.001
        for: 2m
        labels:
          severity: warning
        annotations:
          summary: "P99 латентность обработки ордеров выше 1 мс"

      # Частота отклонений выше 5%
      - alert: HighRejectRate
        expr: rate(me_orders_rejected_total[5m]) / rate(me_orders_total[5m]) > 0.05
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "Частота отклонения ордеров превышает 5%"

      # Очередь шарда растёт
      - alert: ShardQueueBacklog
        expr: me_shard_queue_depth > 1000
        for: 1m
        labels:
          severity: critical
        annotations:
          summary: "Глубина очереди шарда > 1000: возможное узкое место"

      # Нет активных соединений
      - alert: NoConnections
        expr: sum(me_connections_active) == 0
        for: 5m
        labels:
          severity: info
        annotations:
          summary: "Нет активных клиентских соединений"
```

---

## Корректная остановка (Graceful Shutdown)

Сервер обрабатывает сигналы `SIGTERM` и `SIGINT` (Ctrl+C):

1. Получен сигнал остановки.
2. Прекращается приём новых соединений.
3. Существующие соединения могут завершить работу в пределах `drain_timeout_secs`.
4. По истечении таймаута оставшиеся соединения закрываются.
5. Каждое соединение получает кадр `Error(ServerShuttingDown)` перед закрытием.

```toml
[server.shutdown]
drain_timeout_secs = 30
```

---

## Рекомендации для продакшена

- **Используйте `--release` сборку.** Debug-сборки имеют на порядки большую латентность.

- **Оптимальное количество шардов.** Ориентир: 1 шард на 2–3 торговые пары.
  При `shards = 0` движок использует все доступные ядра CPU.

- **Настройте API-ключ.** Без `server.admin.api_key` эндпоинты halt/resume
  доступны любому, кто достучится до порта метрик.

- **Запускайте за файрволом.** Движок не реализует TLS. Используйте обратный
  прокси или сетевую изоляцию.

- **Мониторьте через Prometheus + Grafana.** Отслеживайте P99 латентность и
  глубину очередей шардов.

- **Разделяйте порты.** Торговый протокол: TCP порт 9090. Метрики и admin API:
  HTTP порт 9091. Файрвол настраивайте раздельно.

- **Используйте UDS для локальных клиентов.** Если торговый шлюз на той же
  машине — `uds_path` даёт меньшую латентность, чем TCP loopback.

- **`RUST_LOG` для диагностики.** В продакшене: `me_server=info`.
  Для отладки временно: `me_server=debug`.

- **Отделите порт метрик от торгового.** Метрики-порт должен быть доступен
  только мониторинговым системам, торговый — только авторизованным клиентам.
