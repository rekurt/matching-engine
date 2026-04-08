# Быстрый старт

Пять шагов от нулевой установки до первого исполненного ордера.

---

## Шаг 1 — Сборка

Требуется [Rust stable](https://rustup.rs) (1.75+).

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
tcp_addr   = "0.0.0.0:9090"
metrics_addr = "0.0.0.0:9091"

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
INFO  me_server: tcp listening on 0.0.0.0:9090
INFO  me_server: metrics listening on 0.0.0.0:9091
INFO  me_server: registered symbol BTC/USDT (id=0)
```

Проверить что сервер жив:

```bash
curl http://localhost:9091/health
# {"status":"ok"}
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

Пример размещения двух пересекающихся лимитных ордеров и чтения события сделки:

```go
client, _ := meclient.Dial("127.0.0.1:9090")
defer client.Close()

// Покупка: 1.5 BTC @ $50 000
client.PlaceOrder(&meclient.PlaceOrderRequest{
    OrderId:     1001,
    SymbolId:    0,
    Side:        meclient.SideBuy,
    OrderType:   meclient.OrderTypeLimit,
    TimeInForce: meclient.TIFGtc,
    Price:       5000000,  // $50 000.00 (price_scale=2 → *100)
    Quantity:    150000000, // 1.5 BTC   (qty_scale=8  → *1e8)
})

// Продажа: 1.0 BTC @ $50 000 (пересекается с покупкой)
client.PlaceOrder(&meclient.PlaceOrderRequest{
    OrderId:     1002,
    SymbolId:    0,
    Side:        meclient.SideSell,
    OrderType:   meclient.OrderTypeLimit,
    TimeInForce: meclient.TIFGtc,
    Price:       5000000,
    Quantity:    100000000,
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
