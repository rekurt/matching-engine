# meclient - Go Client for Matching Engine Binary Protocol

Go client library for connecting to the HFT cryptocurrency spot matching engine via its binary wire protocol over TCP or Unix Domain Sockets.

## Installation

```bash
go get github.com/rekurt/matching-engine/clients/go/meclient
```

## Quick Start

```go
package main

import (
    "fmt"
    "log"
    "github.com/rekurt/matching-engine/clients/go/meclient"
)

func main() {
    // Connect via TCP
    client, err := meclient.Dial("tcp", "localhost:9090")
    if err != nil {
        log.Fatal(err)
    }
    defer client.Close()

    // Place a limit buy order
    events, err := client.PlaceOrder(meclient.PlaceOrderRequest{
        OrderID:     1,
        SymbolID:    0,
        Side:        meclient.SideBuy,
        OrderType:   meclient.OrderTypeLimit,
        TimeInForce: meclient.TIFGTC,
        Price:       meclient.NewDecimal(50000.00),   // BTC price
        StopPrice:   meclient.DecimalZero(),
        Qty:         meclient.NewDecimal(0.5),        // 0.5 BTC
    })
    if err != nil {
        log.Fatal(err)
    }

    for _, event := range events {
        fmt.Printf("Event: %+v\n", event)
    }

    // Cancel order
    events, err = client.CancelOrder(1, 0)
    // ...
}
```

## Wire Protocol

All integers are **little-endian**. Prices and quantities use **fixed-point** representation with scale factor 10^8.

### Frame Format

Every message is wrapped in a length-prefixed, versioned frame (protocol **v2**):

```
┌──────────┬──────────┬──────────┬──────────┬─────────────┬─────────────────┐
│ len: u32 │ ver: u8  │ type: u8 │ seq: u64 │ req_id: u64 │ payload: [u8]   │
│ 4 bytes  │ 1 byte   │ 1 byte   │ 8 bytes  │ 8 bytes     │ variable        │
└──────────┴──────────┴──────────┴──────────┴─────────────┴─────────────────┘
```

Header size: 22 bytes. Version byte: `0x02`. `len` covers the payload only.

`req_id` is the idempotency key. Inbound commands (`PlaceOrder`, `CancelOrder`,
`AmendOrder`) MUST carry a non-zero `req_id` — the server dedups on it and echoes
it in the matching response for correlation; a zero `req_id` on a command is
rejected with `InvalidRequestId`. Connection-management frames (`Handshake`,
`Ping`) use `req_id` 0. `PlaceOrder`/`CancelOrder`/`AmendOrder` return the
generated `req_id`, and `ReadEvent` returns the echoed `req_id` of each response.

### Connection Lifecycle

After connecting, the client must send a **Handshake** (0x05) frame before any commands. The server replies with **HandshakeAck** (0x1A). The `Dial` and `DialUDS` functions perform this automatically.

### Fixed-Point Decimal

All prices and quantities are 64-bit integers with an implicit scale of 10^8:

| Human Value | Raw Value |
|------------|-----------|
| 1.00000000 | 100000000 |
| 50000.50 | 5000050000000 |
| 0.00000001 | 1 |

Prices are **signed i64** (to support comparison arithmetic).
Quantities are **unsigned u64** (always non-negative).

### Connection Management Messages

#### Handshake (0x05) — 1 byte payload
`ClientVersion(u8)` — sent by client on connect

#### HandshakeAck (0x1A) — 1 byte payload
`ServerVersion(u8)` — server response to Handshake

#### Ping (0x04) — 0 bytes payload
Sent by client; server responds with Pong

#### Pong (0x18) — 0 bytes payload
Server response to Ping

#### Error (0x19) — variable payload
`ErrorCode(u16 LE) + MsgLen(u16 LE) + Message(bytes)`

### Inbound Messages (Client → Engine)

#### PlaceOrder (0x01) — 37 bytes payload

| Offset | Field | Type | Description |
|--------|-------|------|-------------|
| 0 | OrderID | u64 LE | Unique order identifier |
| 8 | SymbolID | u16 LE | Trading pair ID |
| 10 | Side | u8 | 0=Buy, 1=Sell |
| 11 | OrderType | u8 | 0=Limit, 1=Market, 2=StopLimit, 3=StopMarket |
| 12 | TimeInForce | u8 | 0=GTC, 1=IOC, 2=FOK |
| 13 | Price | i64 LE | Limit price (fixed-point, 0 for market orders) |
| 21 | StopPrice | i64 LE | Stop trigger price (0 if not a stop order) |
| 29 | Qty | u64 LE | Order quantity (fixed-point) |

#### CancelOrder (0x02) — 10 bytes payload

| Offset | Field | Type | Description |
|--------|-------|------|-------------|
| 0 | OrderID | u64 LE | Order to cancel |
| 8 | SymbolID | u16 LE | Trading pair ID |

#### AmendOrder (0x03) — variable payload (12-28 bytes)

| Offset | Field | Type | Description |
|--------|-------|------|-------------|
| 0 | OrderID | u64 LE | Order to amend |
| 8 | SymbolID | u16 LE | Trading pair ID |
| 10 | HasPrice | u8 | 1 if new price follows, 0 if not |
| 11 | Price | i64 LE | *(only if HasPrice=1)* New limit price |
| 11/19 | HasQty | u8 | 1 if new qty follows, 0 if not |
| 12/20 | Qty | u64 LE | *(only if HasQty=1)* New quantity |

### Outbound Messages (Engine → Client)

#### OrderAccepted (0x10) — 16 bytes
`OrderID(u64) + Timestamp(u64)`

#### OrderRejected (0x11) — 17 bytes
`OrderID(u64) + Reason(u8) + Timestamp(u64)`

Reject reasons:
| Value | Reason |
|-------|--------|
| 0 | NoLiquidity |
| 1 | InsufficientLiquidity |
| 2 | InvalidPrice |
| 3 | InvalidQuantity |
| 4 | InvalidLotSize |
| 5 | InvalidTickSize |
| 6 | OrderNotFound |
| 7 | DuplicateOrderId |
| 8 | SymbolNotFound |

#### Trade (0x12) — 51 bytes
`TradeID(u64) + SymbolID(u16) + Price(i64) + Qty(u64) + MakerOrderID(u64) + TakerOrderID(u64) + MakerSide(u8) + Timestamp(u64)`

#### OrderFilled (0x13) — 32 bytes
`OrderID(u64) + FilledQty(u64) + Price(i64) + Timestamp(u64)`

#### OrderPartiallyFilled (0x14) — 40 bytes
`OrderID(u64) + FilledQty(u64) + RemainingQty(u64) + Price(i64) + Timestamp(u64)`

#### OrderCancelled (0x15) — 24 bytes
`OrderID(u64) + RemainingQty(u64) + Timestamp(u64)`

#### OrderAmended (0x16) — 32 bytes
`OrderID(u64) + NewPrice(i64) + NewQty(u64) + Timestamp(u64)`

#### StopTriggered (0x17) — 24 bytes
`OrderID(u64) + TriggerPrice(i64) + Timestamp(u64)`

## Transport Options

### TCP
```go
client, err := meclient.Dial("tcp", "host:port")
```

### Unix Domain Socket
```go
client, err := meclient.Dial("unix", "/var/run/matching-engine.sock")
```

UDS provides ~30% lower latency than TCP for co-located processes.

## Thread Safety

The client is thread-safe. All methods use internal synchronization via mutex.
Multiple goroutines can safely call PlaceOrder, CancelOrder, etc. concurrently.

## Error Handling

Network errors are returned as Go errors. Protocol-level rejections are returned as events (OrderRejected) — not errors.
