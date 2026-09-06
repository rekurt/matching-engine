# Быстрый старт

Пять шагов от нулевой установки до первого исполненного ордера.

---

## Шаг 1 — Сборка

Требуется [Rust stable](https://rustup.rs) (1.96+).

```bash
git clone https://github.com/rekurt/matching-engine.git
cd matching-engine
cargo build --release
```

Бинарный файл появится в `target/release/me-server`.

---

## Шаг 2 — Конфигурация

Скопируйте пример конфига и при необходимости отредактируйте:

```bash
cp config.toml my-config.toml
```

Минимальная конфигурация для одного символа BTC/USDT:

```toml
[server]
tcp_addr   = "127.0.0.1:9090"
metrics_addr = "127.0.0.1:9091"

[[symbols]]
id          = 0
base        = "BTC"
quote       = "USDT"
price_scale = 2       # цена с точностью до $0.01
qty_scale   = 8       # количество с точностью до 0.00000001 BTC
min_qty     = "0.00001"
max_qty     = "10000.0"
qty_step    = "0.00001"
price_step  = "0.01"
```

Подробное описание всех полей — в разделе [Конфигурация](./configuration.md).

---

## Шаг 3 — Запуск

```bash
./target/release/me-server --config my-config.toml
```

Сервер выводит в stdout:

```
INFO  me_server: tcp listening on 127.0.0.1:9090
INFO  me_server: metrics listening on 127.0.0.1:9091
INFO  me_server: registered symbol BTC/USDT (id=0)
```

Проверить что сервер жив:

```bash
curl http://localhost:9091/health
# OK
```

---

## Шаг 4 — Подключение

Используйте готовый клиент из `examples/go/`:

```bash
cd examples/go
go run main.go 127.0.0.1:9090
```

Ожидаемый вывод:

```
=== Matching Engine Go Client Example ===

Connecting to 127.0.0.1:9090 ...
Connected and handshake completed.

Sending Ping ...
Pong received — connection is healthy.
```

Для подключения вручную через `nc` (netcat) используйте бинарный режим — протокол не текстовый.

---

## Шаг 5 — Первый ордер

Фрагмент для чистого тестового стакана: размещение двух пересекающихся лимитных ордеров. На wire цена и количество всегда имеют масштаб 10⁸; `price_scale` и `qty_scale` не меняют wire encoding. В рабочей интеграции проверяйте ошибки каждого вызова и задавайте deadlines.

```go
client, _ := meclient.Dial("127.0.0.1:9090")
defer client.Close()

// Покупка: 1.5 BTC @ $50 000
client.PlaceOrder(&meclient.PlaceOrderRequest{
    OrderId:     1001,
    SymbolId:    0,
    Side:        meclient.SideBuy,
    OrderType:   meclient.OrderTypeLimit,
    TimeInForce: meclient.TimeInForceGTC,
    Price:       meclient.NewDecimal(50000),
    Qty:         meclient.NewDecimal(1.5),
})

// Продажа: 1.0 BTC @ $50 000 (пересекается с покупкой)
client.PlaceOrder(&meclient.PlaceOrderRequest{
    OrderId:     1002,
    SymbolId:    0,
    Side:        meclient.SideSell,
    OrderType:   meclient.OrderTypeLimit,
    TimeInForce: meclient.TimeInForceGTC,
    Price:       meclient.NewDecimal(50000),
    Qty:         meclient.NewDecimal(1),
})

// Читаем входящие события (Trade, OrderAccepted, …)
for {
    event, err := client.ReadEvent()
    if err != nil { break }
    fmt.Println(event)
}
```

Вы увидите событие `Trade` с количеством `1.0 BTC` по цене `$50 000`.

> Полный рабочий пример: [`examples/go/main.go`](../../../examples/go/main.go)

---

## Что дальше

- [Конфигурация](./configuration.md) — все поля конфигурационного файла
- [Торговые пары](./trading-pairs.md) — готовые конфиги для популярных активов
- [Бинарный протокол](./protocol.md) — спецификация протокола с hex-примерами
- [Клиент на Go](./clients/go.md) — полный пример интеграции
