# Клиент на Go

Полный пример интеграции с matching engine на Go, используя высокоуровневую
библиотеку `meclient` из `clients/go/meclient/`. Библиотека скрывает работу с
бинарным протоколом: framing, handshake, sequence numbers.

---

## Предварительные требования

- Go 1.21+
- Запущенный экземпляр me-server с хотя бы одним символом (symbol_id=0)

---

## Запуск примера

```bash
cd examples/go
go run main.go [addr]
# по умолчанию: 127.0.0.1:9090
```

Ожидаемый вывод при пересечении двух ордеров:

```
=== Matching Engine Go Client Example ===

Connecting to 127.0.0.1:9090 ...
Connected and handshake completed.

Sending Ping ...
Pong received — connection is healthy.

Placing BUY LIMIT: order_id=1001, price=50000, qty=1.5
  → sent (seq=2)
  [seq=1] OrderAccepted: order_id=1001, ts=...

Placing SELL LIMIT: order_id=1002, price=49999, qty=0.5
  → sent (seq=3)
  [seq=2] OrderAccepted: order_id=1002, ts=...
  [seq=3] Trade: trade_id=1, symbol=0, price=50000, qty=0.5, maker=1001, taker=1002, maker_side=Buy
  [seq=4] OrderPartiallyFilled: order_id=1001, filled=0.5, remaining=1, price=50000
  [seq=5] OrderFilled: order_id=1002, filled_qty=0.5, price=50000
```

---

## Шаг 1 — Подключение

`meclient.Dial` открывает TCP-соединение и автоматически выполняет handshake:

```go
import "github.com/rekurt/matching-engine/clients/go/meclient"

client, err := meclient.Dial("127.0.0.1:9090")
if err != nil {
    log.Fatal(err)
}
defer client.Close()
```

---

## Шаг 2 — Ping / Pong

Проверка живости соединения перед отправкой ордеров:

```go
if err := client.Ping(); err != nil {
    log.Fatalf("ping failed: %v", err)
}
// Pong получен — соединение здорово
```

---

## Шаг 3 — Размещение ордера

### Buy Limit: 1.5 BTC @ $50 000

```go
buyOrder := &meclient.PlaceOrderRequest{
    OrderId:     1001,
    SymbolId:    0,                         // BTC/USDT
    Side:        meclient.SideBuy,
    OrderType:   meclient.OrderTypeLimit,
    TimeInForce: meclient.TimeInForceGTC,   // Good-Til-Cancelled
    Price:       meclient.NewDecimal(50_000.00),
    StopPrice:   meclient.Decimal{},        // не стоп-ордер
    Qty:         meclient.NewDecimal(1.5),
}

seq, err := client.PlaceOrder(buyOrder)
// seq — номер исходящего фрейма для отладки
```

### Sell Limit пересекается с Buy: 0.5 BTC @ $49 999

```go
sellOrder := &meclient.PlaceOrderRequest{
    OrderId:     1002,
    SymbolId:    0,
    Side:        meclient.SideSell,
    OrderType:   meclient.OrderTypeLimit,
    TimeInForce: meclient.TimeInForceGTC,
    Price:       meclient.NewDecimal(49_999.00),
    StopPrice:   meclient.Decimal{},
    Qty:         meclient.NewDecimal(0.5),
}

_, err = client.PlaceOrder(sellOrder)
```

Sell ордер пересекает рестинг Buy — движок генерирует `Trade` по цене маркет-мейкера
($50 000), затем `OrderFilled` (sell) и `OrderPartiallyFilled` (buy).

---

## Шаг 4 — Чтение событий

### Одиночное событие

```go
event, seq, err := client.ReadEvent()
if err != nil {
    log.Fatal(err)
}

switch {
case event.OrderAccepted != nil:
    fmt.Printf("Accepted: order_id=%d\n", event.OrderAccepted.OrderId)

case event.Trade != nil:
    t := event.Trade
    fmt.Printf("Trade: price=%s qty=%s maker=%d taker=%d\n",
        t.Price, t.Qty, t.MakerOrderId, t.TakerOrderId)

case event.OrderFilled != nil:
    f := event.OrderFilled
    fmt.Printf("Filled: order_id=%d qty=%s\n", f.OrderId, f.FilledQty)

case event.OrderRejected != nil:
    fmt.Printf("Rejected: order_id=%d reason=%s\n",
        event.OrderRejected.OrderId, event.OrderRejected.Reason)
}
```

### Дренаж всех событий (с таймаутом)

После размещения пересекающегося ордера движок может прислать несколько событий
последовательно. Читаем до тайм-аута:

```go
for {
    ch := make(chan struct{ event *meclient.Event; seq uint64; err error }, 1)
    go func() {
        event, seq, err := client.ReadEvent()
        ch <- struct{ ... }{event, seq, err}
    }()

    select {
    case r := <-ch:
        if r.err != nil {
            return
        }
        // обработать r.event
    case <-time.After(500 * time.Millisecond):
        return // нет новых событий
    }
}
```

---

## Шаг 5 — Отмена ордера

```go
cancelReq := &meclient.CancelOrderRequest{
    OrderId:  1001,
    SymbolId: 0,
}

_, err = client.CancelOrder(cancelReq)

// Ждём OrderCancelled
event, _, err := client.ReadEvent()
if event.OrderCancelled != nil {
    fmt.Printf("Cancelled: order_id=%d remaining=%s\n",
        event.OrderCancelled.OrderId,
        event.OrderCancelled.RemainingQty)
}
```

---

## Шаг 6 — Market ордер (IOC)

Market ордер немедленно исполняется по лучшим ценам стакана. IOC отменяет
неисполненный остаток. При пустом стакане — `OrderRejected` с причиной `NoLiquidity`.

```go
marketOrder := &meclient.PlaceOrderRequest{
    OrderId:     1003,
    SymbolId:    0,
    Side:        meclient.SideBuy,
    OrderType:   meclient.OrderTypeMarket,
    TimeInForce: meclient.TimeInForceIOC,
    Price:       meclient.Decimal{}, // цена не задаётся для маркет-ордеров
    StopPrice:   meclient.Decimal{},
    Qty:         meclient.NewDecimal(1.0),
}

_, err = client.PlaceOrder(marketOrder)
```

---

## Типы данных meclient

| Тип/Константа              | Значение         | Описание                   |
|----------------------------|------------------|----------------------------|
| `meclient.SideBuy`         | `0`              | Покупка                    |
| `meclient.SideSell`        | `1`              | Продажа                    |
| `meclient.OrderTypeLimit`  | `0`              | Лимитный ордер             |
| `meclient.OrderTypeMarket` | `1`              | Рыночный ордер             |
| `meclient.OrderTypeStopLimit`  | `2`          | Стоп-лимит                 |
| `meclient.OrderTypeStopMarket` | `3`          | Стоп-маркет                |
| `meclient.TimeInForceGTC`  | `0`              | Good-Til-Cancelled         |
| `meclient.TimeInForceIOC`  | `1`              | Immediate-Or-Cancel        |
| `meclient.TimeInForceFOK`  | `2`              | Fill-Or-Kill               |

---

## Полный рабочий пример

Файл [`examples/go/main.go`](../../../../examples/go/main.go) содержит полный
рабочий пример: connect → ping → buy limit → sell limit (trade) → cancel → market IOC.

---

## Что дальше

- [Клиент на Python](./python.md) — zero-dependency реализация на raw socket
- [Обзор протокола](../integration.md) — детали framing, sequence numbers
- [Бинарный протокол](../protocol.md) — hex-дампы и полная спецификация
