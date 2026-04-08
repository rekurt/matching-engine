# Wire Protocol Specification / Спецификация сетевого протокола

**Version / Версия:** 2.0
**Protocol Version Byte / Байт версии протокола:** `0x02`
**Status / Статус:** Stable / Стабильный
**Encoding / Кодировка:** Little-Endian for all multi-byte integers / Little-Endian для всех многобайтовых целых

---

## Table of Contents / Оглавление

1. [Overview / Обзор](#1-overview--обзор)
2. [Constants / Константы](#2-constants--константы)
3. [Frame Format / Формат кадра](#3-frame-format--формат-кадра)
4. [Message Types / Типы сообщений](#4-message-types--типы-сообщений)
5. [Inbound Messages (Client to Engine) / Входящие сообщения (Клиент к Движку)](#5-inbound-messages-client--engine--входящие-сообщения-клиент--движок)
6. [Outbound Messages (Engine to Client) / Исходящие сообщения (Движок к Клиенту)](#6-outbound-messages-engine--client--исходящие-сообщения-движок--клиенту)
7. [Enumerations / Перечисления](#7-enumerations--перечисления)
8. [Fixed-Point Encoding / Кодирование с фиксированной точкой](#8-fixed-point-encoding--кодирование-с-фиксированной-точкой)
9. [Connection Lifecycle / Жизненный цикл соединения](#9-connection-lifecycle--жизненный-цикл-соединения)
10. [Sequence Numbers / Порядковые номера](#10-sequence-numbers--порядковые-номера)
11. [Backpressure / Обратное давление](#11-backpressure--обратное-давление)
12. [Hex Dump Examples / Примеры шестнадцатеричных дампов](#12-hex-dump-examples--примеры-шестнадцатеричных-дампов)
13. [Implementation Notes / Примечания по реализации](#13-implementation-notes--примечания-по-реализации)

---

## 1. Overview / Обзор

This document specifies the binary wire protocol used by the matching engine for
client-server communication over TCP. The protocol is designed for ultra-low-latency
trading with minimal overhead: no TLV wrappers, no JSON, no string parsing. Every
message is a length-prefixed binary frame with a fixed 22-byte header (v2) followed by
a type-specific payload.

> **Русский:**
> Данный документ описывает бинарный сетевой протокол, используемый биржевым
> движком для взаимодействия клиента и сервера по TCP. Протокол спроектирован
> для сверхнизкой задержки: без TLV-обёрток, без JSON, без разбора строк.
> Каждое сообщение представляет собой бинарный кадр с префиксом длины,
> состоящий из фиксированного 22-байтового заголовка (v2) и полезной нагрузки,
> специфичной для типа сообщения.

---

## 2. Constants / Константы

| Constant                | Value      | Description (EN)                        | Описание (RU)                                |
|-------------------------|------------|-----------------------------------------|----------------------------------------------|
| `PROTOCOL_VERSION`      | `2`        | Current protocol version                | Текущая версия протокола                     |
| `FRAME_HEADER_SIZE`     | `22` bytes | Fixed header preceding every payload    | Фиксированный заголовок перед каждой нагрузкой|
| `MAX_PAYLOAD_SIZE`      | `1024` bytes | Maximum allowed payload length (regular) | Макс. длина нагрузки (обычные фреймы)      |
| `MAX_PAYLOAD_SIZE_BATCH`| `4096` bytes | Maximum payload for batch frames      | Макс. нагрузка для batch-фреймов             |
| `MAX_PAYLOAD_SIZE_SNAPSHOT`| `8192` bytes | Maximum payload for snapshot frames | Макс. нагрузка для snapshot-фреймов          |
| `MAX_WRITE_BUFFER_SIZE` | `262144` bytes (256 KB) | Server write-buffer backpressure limit | Лимит буфера записи для обратного давления |
| `MAX_BATCH_PLACE`       | `20`       | Max orders in BatchPlaceOrder           | Макс. ордеров в BatchPlaceOrder              |
| `MAX_BATCH_CANCEL`      | `50`       | Max orders in BatchCancelOrder          | Макс. ордеров в BatchCancelOrder             |

---

## 3. Frame Format / Формат кадра

Every message on the wire is a **frame** consisting of a 22-byte header (v2) immediately
followed by the payload bytes. There is no padding and no alignment requirement.

> **Русский:**
> Каждое сообщение на проводе представляет собой **кадр**, состоящий из
> 22-байтового заголовка (v2), за которым непосредственно следуют байты полезной
> нагрузки. Выравнивание и заполнение не требуются.

### ASCII Diagram / ASCII-диаграмма

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                     Payload Length (u32 LE)                    |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|   Version(u8) |    Type(u8)   |                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+                               +
|                     Sequence Number (u64 LE)                  |
+                               +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                               |                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+                               +
|                      Request ID (u64 LE)                      |
+                               +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                               |                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+                               +
|                        Payload (variable)                     |
+                              ...                              +
```

### Byte-Level Layout / Побайтовая раскладка

```
Offset  Size   Field       Type      Description
------  ----   -----       ----      -----------
 0       4     len         u32 LE    Payload length in bytes (NOT including header)
 4       1     ver         u8        Protocol version (must be 0x02)
 5       1     type        u8        MessageType enum discriminant
 6       8     seq         u64 LE    Monotonically increasing sequence number
14       8     req_id      u64 LE    Idempotency key (mirrored in command replies, 0 for service frames)
22       N     payload     bytes     Type-specific payload (0..1024 bytes; batch: 0..4096; snapshot: 0..8192)
```

> **Русский:**
>
> | Смещение | Размер | Поле    | Тип     | Описание                                            |
> |----------|--------|---------|---------|-----------------------------------------------------|
> | 0        | 4      | len     | u32 LE  | Длина полезной нагрузки в байтах (без заголовка)    |
> | 4        | 1      | ver     | u8      | Версия протокола (должна быть 0x02)                 |
> | 5        | 1      | type    | u8      | Дискриминант перечисления MessageType                |
> | 6        | 8      | seq     | u64 LE  | Монотонно возрастающий порядковый номер              |
> | 14       | 8      | req_id  | u64 LE  | Ключ идемпотентности (зеркалится в ответах на команды) |
> | 22       | N      | payload | байты   | Нагрузка (0..1024; batch: 0..4096; snapshot: 0..8192) |

**Total frame size** = `FRAME_HEADER_SIZE + len` = `22 + len` bytes.

The `len` field covers **only** the payload bytes. It does NOT include the 22-byte (v2)
header itself.

> **Русский:**
> Поле `len` охватывает **только** байты полезной нагрузки. Оно НЕ включает
> 22-байтовый заголовок.

---

## 4. Message Types / Типы сообщений

### Summary Table / Сводная таблица

| Code   | Name                 | Direction       | Payload Size   |
|--------|----------------------|-----------------|----------------|
| `0x01` | PlaceOrder           | Client->Engine  | 37 bytes       |
| `0x02` | CancelOrder          | Client->Engine  | 10 bytes       |
| `0x03` | AmendOrder           | Client->Engine  | variable       |
| `0x04` | Ping                 | Client->Engine  | 0 bytes        |
| `0x05` | Handshake            | Client->Engine  | 0 or 1 byte    |
| `0x10` | OrderAccepted        | Engine->Client  | 16 bytes       |
| `0x11` | OrderRejected        | Engine->Client  | 17 bytes       |
| `0x12` | Trade                | Engine->Client  | 51 bytes       |
| `0x13` | OrderFilled          | Engine->Client  | 32 bytes       |
| `0x14` | OrderPartiallyFilled | Engine->Client  | 40 bytes       |
| `0x15` | OrderCancelled       | Engine->Client  | 24 bytes       |
| `0x16` | OrderAmended         | Engine->Client  | 32 bytes       |
| `0x17` | StopTriggered        | Engine->Client  | 24 bytes       |
| `0x18` | Pong                 | Engine->Client  | 0 bytes        |
| `0x19` | Error                | Engine->Client  | variable       |
| `0x1A` | HandshakeAck         | Engine->Client  | 1 byte         |

> **Русский:**
>
> | Код    | Имя                  | Направление     | Размер нагрузки |
> |--------|----------------------|-----------------|-----------------|
> | `0x01` | PlaceOrder           | Клиент->Движок  | 37 байт         |
> | `0x02` | CancelOrder          | Клиент->Движок  | 10 байт         |
> | `0x03` | AmendOrder           | Клиент->Движок  | переменный      |
> | `0x04` | Ping                 | Клиент->Движок  | 0 байт          |
> | `0x05` | Handshake            | Клиент->Движок  | 0 или 1 байт   |
> | `0x10` | OrderAccepted        | Движок->Клиент  | 16 байт         |
> | `0x11` | OrderRejected        | Движок->Клиент  | 17 байт         |
> | `0x12` | Trade                | Движок->Клиент  | 51 байт         |
> | `0x13` | OrderFilled          | Движок->Клиент  | 32 байт         |
> | `0x14` | OrderPartiallyFilled | Движок->Клиент  | 40 байт         |
> | `0x15` | OrderCancelled       | Движок->Клиент  | 24 байт         |
> | `0x16` | OrderAmended         | Движок->Клиент  | 32 байт         |
> | `0x17` | StopTriggered        | Движок->Клиент  | 24 байт         |
> | `0x18` | Pong                 | Движок->Клиент  | 0 байт          |
> | `0x19` | Error                | Движок->Клиент  | переменный      |
> | `0x1A` | HandshakeAck         | Движок->Клиент  | 1 байт          |

---

## 5. Inbound Messages (Client -> Engine) / Входящие сообщения (Клиент -> Движок)

### 5.1 PlaceOrder (0x01)

Places a new order in the order book.

> **Русский:** Размещает новый ордер в книге заявок.

**Payload: 37 bytes**

```
Offset  Size  Field        Type     Description
------  ----  -----        ----     -----------
 0       8    order_id     u64 LE   Unique client-assigned order identifier
 8       2    symbol_id    u16 LE   Instrument/symbol identifier
10       1    side         u8       0=Buy, 1=Sell
11       1    order_type   u8       0=Limit, 1=Market, 2=StopLimit, 3=StopMarket
12       1    tif          u8       0=GTC, 1=IOC, 2=FOK
13       8    price        i64 LE   Limit price (fixed-point, scale 10^8)
21       8    stop_price   i64 LE   Stop trigger price (fixed-point, scale 10^8)
29       8    qty          u64 LE   Order quantity (fixed-point, scale 10^8)
```

> **Русский:**
>
> | Смещение | Размер | Поле       | Тип     | Описание                                           |
> |----------|--------|------------|---------|-----------------------------------------------------|
> | 0        | 8      | order_id   | u64 LE  | Уникальный идентификатор ордера, назначенный клиентом|
> | 8        | 2      | symbol_id  | u16 LE  | Идентификатор инструмента/символа                   |
> | 10       | 1      | side       | u8      | 0=Покупка, 1=Продажа                               |
> | 11       | 1      | order_type | u8      | 0=Лимитный, 1=Рыночный, 2=Стоп-лимит, 3=Стоп-рынок|
> | 12       | 1      | tif        | u8      | 0=GTC, 1=IOC, 2=FOK                                |
> | 13       | 8      | price      | i64 LE  | Лимитная цена (фикс. точка, масштаб 10^8)          |
> | 21       | 8      | stop_price | i64 LE  | Стоп-цена срабатывания (фикс. точка, масштаб 10^8) |
> | 29       | 8      | qty        | u64 LE  | Количество (фикс. точка, масштаб 10^8)             |

**Byte layout diagram:**

```
+--------+--------+--------+--------+--------+--------+--------+--------+
|                         order_id (u64 LE)                              |
+--------+--------+--------+--------+--------+--------+--------+--------+
| symbol_id(u16)  |  side  |  type  |  tif   |                          |
+--------+--------+--------+--------+--------+                          +
|                          price (i64 LE)                                |
+                 +--------+--------+--------+--------+--------+--------+
|                 |                                                      |
+--------+--------+                stop_price (i64 LE)                  +
|                                                      +--------+------+
|                                                      |                |
+--------+--------+--------+--------+--------+--------+                +
|                          qty (u64 LE)                                  |
+--------+--------+--------+--------+--------+--------+--------+--------+
Total payload: 37 bytes
```

**Notes / Примечания:**
- For Market orders, `price` should be set to 0. / Для рыночных ордеров `price` устанавливается в 0.
- For non-stop orders, `stop_price` should be set to 0. / Для не-стоп ордеров `stop_price` устанавливается в 0.
- `order_id` must be unique per connection. Duplicates result in `DuplicateOrderId` rejection. / `order_id` должен быть уникальным в рамках соединения. Дубликаты приводят к отклонению `DuplicateOrderId`.

---

### 5.2 CancelOrder (0x02)

Cancels an existing resting order.

> **Русский:** Отменяет существующий ордер в книге заявок.

**Payload: 10 bytes**

```
Offset  Size  Field        Type     Description
------  ----  -----        ----     -----------
 0       8    order_id     u64 LE   Order to cancel
 8       2    symbol_id    u16 LE   Symbol the order belongs to
```

> **Русский:**
>
> | Смещение | Размер | Поле      | Тип     | Описание                           |
> |----------|--------|-----------|---------|------------------------------------|
> | 0        | 8      | order_id  | u64 LE  | Идентификатор ордера для отмены    |
> | 8        | 2      | symbol_id | u16 LE  | Символ, которому принадлежит ордер |

**Byte layout:**

```
+--------+--------+--------+--------+--------+--------+--------+--------+
|                         order_id (u64 LE)                              |
+--------+--------+--------+--------+--------+--------+--------+--------+
| symbol_id(u16)  |
+--------+--------+
Total payload: 10 bytes
```

---

### 5.3 AmendOrder (0x03)

Amends the price and/or quantity of an existing resting order. Uses optional fields
indicated by presence flags.

> **Русский:** Изменяет цену и/или количество существующего ордера в книге заявок.
> Использует необязательные поля, обозначенные флагами наличия.

**Payload: variable (minimum 12 bytes, maximum 28 bytes)**

```
Offset  Size  Field        Type     Description
------  ----  -----        ----     -----------
 0       8    order_id     u64 LE   Order to amend
 8       2    symbol_id    u16 LE   Symbol the order belongs to
10       1    has_price    u8       1 if new price follows, 0 otherwise
11       0|8  price        i64 LE   [conditional] New price (only if has_price=1)
 ?       1    has_qty      u8       1 if new quantity follows, 0 otherwise
 ?       0|8  qty          u64 LE   [conditional] New quantity (only if has_qty=1)
```

> **Русский:**
>
> | Смещение | Размер | Поле      | Тип     | Описание                                           |
> |----------|--------|-----------|---------|----------------------------------------------------|
> | 0        | 8      | order_id  | u64 LE  | Идентификатор ордера для изменения                 |
> | 8        | 2      | symbol_id | u16 LE  | Символ, которому принадлежит ордер                 |
> | 10       | 1      | has_price | u8      | 1 если далее следует новая цена, 0 иначе           |
> | 11       | 0\|8   | price     | i64 LE  | [условно] Новая цена (только при has_price=1)      |
> | ?        | 1      | has_qty   | u8      | 1 если далее следует новое количество, 0 иначе     |
> | ?        | 0\|8   | qty       | u64 LE  | [условно] Новое количество (только при has_qty=1)  |

**Variant layouts:**

```
Case 1: Neither price nor qty (12 bytes)
+-- order_id(8) --+- sym(2) -+- 0 -+- 0 -+

Case 2: Price only (21 bytes)
+-- order_id(8) --+- sym(2) -+- 1 -+-- price(8) --+- 0 -+

Case 3: Qty only (21 bytes)
+-- order_id(8) --+- sym(2) -+- 0 -+- 1 -+-- qty(8) --+

Case 4: Both price and qty (28 bytes)
+-- order_id(8) --+- sym(2) -+- 1 -+-- price(8) --+- 1 -+-- qty(8) --+
```

---

### 5.4 Ping (0x04)

Heartbeat/keepalive message. May be sent in any connection state, including
before handshake. The server responds with Pong (0x18).

> **Русский:** Сообщение для проверки связи (heartbeat). Может отправляться в любом
> состоянии соединения, включая до рукопожатия. Сервер отвечает Pong (0x18).

**Payload: 0 bytes**

```
(empty payload)
```

---

### 5.5 Handshake (0x05)

Initiates the connection. Must be the first non-Ping frame sent by the client.
Optionally includes the client protocol version.

> **Русский:** Инициирует соединение. Должен быть первым кадром (кроме Ping),
> отправленным клиентом. Опционально содержит версию протокола клиента.

**Payload: 0 or 1 byte**

```
Offset  Size  Field            Type   Description
------  ----  -----            ----   -----------
 0       0|1  client_version   u8     [optional] Client's protocol version
```

> **Русский:**
>
> | Смещение | Размер | Поле           | Тип | Описание                                    |
> |----------|--------|----------------|-----|---------------------------------------------|
> | 0        | 0\|1   | client_version | u8  | [опционально] Версия протокола клиента      |

If the payload is empty (len=0), the server assumes `client_version = 1`.
The server responds with HandshakeAck (0x1A) on success, or Error (0x19) if
the version is unsupported.

> **Русский:** Если нагрузка пуста (len=0), сервер считает `client_version = 1`.
> Сервер отвечает HandshakeAck (0x1A) при успехе или Error (0x19) при
> неподдерживаемой версии.

---

## 6. Outbound Messages (Engine -> Client) / Исходящие сообщения (Движок -> Клиенту)

### 6.1 OrderAccepted (0x10)

Confirms that an order has been accepted and entered into the order book.

> **Русский:** Подтверждает, что ордер принят и размещён в книге заявок.

**Payload: 16 bytes**

```
Offset  Size  Field       Type     Description
------  ----  -----       ----     -----------
 0       8    order_id    u64 LE   The accepted order's ID
 8       8    timestamp   u64 LE   Server timestamp (nanoseconds since epoch)
```

> **Русский:**
>
> | Смещение | Размер | Поле      | Тип     | Описание                                     |
> |----------|--------|-----------|---------|----------------------------------------------|
> | 0        | 8      | order_id  | u64 LE  | Идентификатор принятого ордера               |
> | 8        | 8      | timestamp | u64 LE  | Временная метка сервера (наносекунды от эпохи)|

---

### 6.2 OrderRejected (0x11)

The order was rejected. Contains the reason code.

> **Русский:** Ордер отклонён. Содержит код причины.

**Payload: 17 bytes**

```
Offset  Size  Field       Type     Description
------  ----  -----       ----     -----------
 0       8    order_id    u64 LE   The rejected order's ID
 8       1    reason      u8       RejectReason enum value (see section 7)
 9       8    timestamp   u64 LE   Server timestamp (nanoseconds since epoch)
```

> **Русский:**
>
> | Смещение | Размер | Поле      | Тип     | Описание                                      |
> |----------|--------|-----------|---------|-----------------------------------------------|
> | 0        | 8      | order_id  | u64 LE  | Идентификатор отклонённого ордера             |
> | 8        | 1      | reason    | u8      | Значение перечисления RejectReason (см. раздел 7)|
> | 9        | 8      | timestamp | u64 LE  | Временная метка сервера (наносекунды от эпохи)|

---

### 6.3 Trade (0x12)

A trade has been executed between a maker and a taker.

> **Русский:** Сделка исполнена между мейкером и тейкером.

**Payload: 51 bytes**

```
Offset  Size  Field           Type     Description
------  ----  -----           ----     -----------
 0       8    trade_id        u64 LE   Unique trade identifier
 8       2    symbol_id       u16 LE   Instrument identifier
10       8    price           i64 LE   Execution price (fixed-point, scale 10^8)
18       8    qty             u64 LE   Executed quantity (fixed-point, scale 10^8)
26       8    maker_order_id  u64 LE   Resting (maker) order ID
34       8    taker_order_id  u64 LE   Aggressive (taker) order ID
42       1    maker_side      u8       Side of the maker: 0=Buy, 1=Sell
43       8    timestamp       u64 LE   Server timestamp (nanoseconds since epoch)
```

> **Русский:**
>
> | Смещение | Размер | Поле           | Тип     | Описание                                       |
> |----------|--------|----------------|---------|-------------------------------------------------|
> | 0        | 8      | trade_id       | u64 LE  | Уникальный идентификатор сделки                |
> | 8        | 2      | symbol_id      | u16 LE  | Идентификатор инструмента                      |
> | 10       | 8      | price          | i64 LE  | Цена исполнения (фикс. точка, масштаб 10^8)   |
> | 18       | 8      | qty            | u64 LE  | Исполненное количество (фикс. точка, 10^8)    |
> | 26       | 8      | maker_order_id | u64 LE  | Идентификатор ордера мейкера (пассивная сторона)|
> | 34       | 8      | taker_order_id | u64 LE  | Идентификатор ордера тейкера (агрессивная сторона)|
> | 42       | 1      | maker_side     | u8      | Сторона мейкера: 0=Покупка, 1=Продажа          |
> | 43       | 8      | timestamp      | u64 LE  | Временная метка сервера (наносекунды от эпохи) |

**Byte layout:**

```
+--------+--------+--------+--------+--------+--------+--------+--------+
|                         trade_id (u64 LE)                              |
+--------+--------+--------+--------+--------+--------+--------+--------+
| symbol_id(u16)  |                                                      |
+--------+--------+                  price (i64 LE)                     +
|                                                      +--------+------+
|                                                      |                |
+--------+--------+--------+--------+--------+--------+                +
|                          qty (u64 LE)                                  |
+--------+--------+--------+--------+--------+--------+--------+--------+
|                       maker_order_id (u64 LE)                          |
+--------+--------+--------+--------+--------+--------+--------+--------+
|                       taker_order_id (u64 LE)                          |
+--------+--------+--------+--------+--------+--------+--------+--------+
|maker_sd|                                                               |
+--------+                  timestamp (u64 LE)                          +
|                                                      +--------+------+
|                                                      |
+--------+--------+--------+--------+--------+--------+
Total payload: 51 bytes
```

---

### 6.4 OrderFilled (0x13)

An order has been completely filled. This is the terminal status for the order.

> **Русский:** Ордер полностью исполнен. Это терминальный статус ордера.

**Payload: 32 bytes**

```
Offset  Size  Field        Type     Description
------  ----  -----        ----     -----------
 0       8    order_id     u64 LE   The filled order's ID
 8       8    filled_qty   u64 LE   Total filled quantity (fixed-point, scale 10^8)
16       8    price        i64 LE   Last fill price (fixed-point, scale 10^8)
24       8    timestamp    u64 LE   Server timestamp (nanoseconds since epoch)
```

> **Русский:**
>
> | Смещение | Размер | Поле       | Тип     | Описание                                          |
> |----------|--------|------------|---------|----------------------------------------------------|
> | 0        | 8      | order_id   | u64 LE  | Идентификатор исполненного ордера                  |
> | 8        | 8      | filled_qty | u64 LE  | Общее исполненное количество (фикс. точка, 10^8)  |
> | 16       | 8      | price      | i64 LE  | Цена последнего исполнения (фикс. точка, 10^8)    |
> | 24       | 8      | timestamp  | u64 LE  | Временная метка сервера (наносекунды от эпохи)     |

---

### 6.5 OrderPartiallyFilled (0x14)

An order has been partially filled. It remains resting in the order book.

> **Русский:** Ордер частично исполнен. Он остаётся в книге заявок.

**Payload: 40 bytes**

```
Offset  Size  Field           Type     Description
------  ----  -----           ----     -----------
 0       8    order_id        u64 LE   The partially filled order's ID
 8       8    filled_qty      u64 LE   Quantity filled in this event (fixed-point)
16       8    remaining_qty   u64 LE   Remaining quantity in the book (fixed-point)
24       8    price           i64 LE   Fill price (fixed-point, scale 10^8)
32       8    timestamp       u64 LE   Server timestamp (nanoseconds since epoch)
```

> **Русский:**
>
> | Смещение | Размер | Поле          | Тип     | Описание                                           |
> |----------|--------|---------------|---------|----------------------------------------------------|
> | 0        | 8      | order_id      | u64 LE  | Идентификатор частично исполненного ордера          |
> | 8        | 8      | filled_qty    | u64 LE  | Количество, исполненное в этом событии (фикс. точка)|
> | 16       | 8      | remaining_qty | u64 LE  | Оставшееся количество в книге (фикс. точка)        |
> | 24       | 8      | price         | i64 LE  | Цена исполнения (фикс. точка, масштаб 10^8)        |
> | 32       | 8      | timestamp     | u64 LE  | Временная метка сервера (наносекунды от эпохи)      |

---

### 6.6 OrderCancelled (0x15)

An order has been successfully cancelled.

> **Русский:** Ордер успешно отменён.

**Payload: 24 bytes**

```
Offset  Size  Field           Type     Description
------  ----  -----           ----     -----------
 0       8    order_id        u64 LE   Cancelled order's ID
 8       8    remaining_qty   u64 LE   Quantity remaining at cancellation (fixed-point)
16       8    timestamp       u64 LE   Server timestamp (nanoseconds since epoch)
```

> **Русский:**
>
> | Смещение | Размер | Поле          | Тип     | Описание                                              |
> |----------|--------|---------------|---------|-------------------------------------------------------|
> | 0        | 8      | order_id      | u64 LE  | Идентификатор отменённого ордера                      |
> | 8        | 8      | remaining_qty | u64 LE  | Оставшееся количество на момент отмены (фикс. точка) |
> | 16       | 8      | timestamp     | u64 LE  | Временная метка сервера (наносекунды от эпохи)        |

---

### 6.7 OrderAmended (0x16)

An order has been successfully amended with new price and/or quantity.

> **Русский:** Ордер успешно изменён с новой ценой и/или количеством.

**Payload: 32 bytes**

```
Offset  Size  Field       Type     Description
------  ----  -----       ----     -----------
 0       8    order_id    u64 LE   Amended order's ID
 8       8    new_price   i64 LE   New price after amendment (fixed-point, scale 10^8)
16       8    new_qty     u64 LE   New quantity after amendment (fixed-point, scale 10^8)
24       8    timestamp   u64 LE   Server timestamp (nanoseconds since epoch)
```

> **Русский:**
>
> | Смещение | Размер | Поле      | Тип     | Описание                                          |
> |----------|--------|-----------|---------|----------------------------------------------------|
> | 0        | 8      | order_id  | u64 LE  | Идентификатор изменённого ордера                   |
> | 8        | 8      | new_price | i64 LE  | Новая цена после изменения (фикс. точка, 10^8)    |
> | 16       | 8      | new_qty   | u64 LE  | Новое количество после изменения (фикс. точка, 10^8)|
> | 24       | 8      | timestamp | u64 LE  | Временная метка сервера (наносекунды от эпохи)     |

---

### 6.8 StopTriggered (0x17)

A stop order has been triggered and converted to a market/limit order.

> **Русский:** Стоп-ордер сработал и преобразован в рыночный/лимитный ордер.

**Payload: 24 bytes**

```
Offset  Size  Field          Type     Description
------  ----  -----          ----     -----------
 0       8    order_id       u64 LE   Triggered stop order's ID
 8       8    trigger_price  i64 LE   Price at which the stop was triggered (fixed-point)
16       8    timestamp      u64 LE   Server timestamp (nanoseconds since epoch)
```

> **Русский:**
>
> | Смещение | Размер | Поле          | Тип     | Описание                                                  |
> |----------|--------|---------------|---------|-----------------------------------------------------------|
> | 0        | 8      | order_id      | u64 LE  | Идентификатор сработавшего стоп-ордера                    |
> | 8        | 8      | trigger_price | i64 LE  | Цена, при которой сработал стоп (фикс. точка, 10^8)      |
> | 16       | 8      | timestamp     | u64 LE  | Временная метка сервера (наносекунды от эпохи)            |

---

### 6.9 Pong (0x18)

Response to a Ping message.

> **Русский:** Ответ на сообщение Ping.

**Payload: 0 bytes**

```
(empty payload)
```

---

### 6.10 Error (0x19)

An error occurred. Contains machine-readable error code and optional human-readable
text. Some errors (e.g., Backpressure) result in immediate disconnection after
the Error frame is sent.

> **Русский:** Произошла ошибка. Содержит машиночитаемый код ошибки и опциональный
> текст, читаемый человеком. Некоторые ошибки (например, Backpressure) приводят
> к немедленному разрыву соединения после отправки кадра Error.

**Payload: variable (minimum 4 bytes)**

```
Offset  Size  Field        Type     Description
------  ----  -----        ----     -----------
 0       2    error_code   u16 LE   ErrorCode enum value (see section 7)
 2       2    msg_len      u16 LE   Length of message text in bytes
 4       N    message      bytes    UTF-8 encoded error description
```

> **Русский:**
>
> | Смещение | Размер | Поле       | Тип     | Описание                                     |
> |----------|--------|------------|---------|----------------------------------------------|
> | 0        | 2      | error_code | u16 LE  | Значение перечисления ErrorCode (см. раздел 7)|
> | 2        | 2      | msg_len    | u16 LE  | Длина текста сообщения в байтах               |
> | 4        | N      | message    | байты   | Описание ошибки в кодировке UTF-8             |

**Byte layout:**

```
+--------+--------+--------+--------+--------+--- ... ---+
| error_code(u16) | msg_len (u16)   |  message text ...   |
+--------+--------+--------+--------+--------+--- ... ---+
```

---

### 6.11 HandshakeAck (0x1A)

Successful response to a Handshake. Transitions the connection to Active state.

> **Русский:** Успешный ответ на Handshake. Переводит соединение в активное состояние.

**Payload: 1 byte**

```
Offset  Size  Field           Type   Description
------  ----  -----           ----   -----------
 0       1    server_version  u8     Server's protocol version
```

> **Русский:**
>
> | Смещение | Размер | Поле           | Тип | Описание                       |
> |----------|--------|----------------|-----|--------------------------------|
> | 0        | 1      | server_version | u8  | Версия протокола сервера       |

---

## 7. Enumerations / Перечисления

### 7.1 Side / Сторона

| Value | Name | EN           | RU           |
|-------|------|--------------|--------------|
| `0`   | Buy  | Buy side     | Покупка      |
| `1`   | Sell | Sell side    | Продажа      |

### 7.2 OrderType / Тип ордера

| Value | Name        | EN              | RU                 |
|-------|-------------|-----------------|---------------------|
| `0`   | Limit       | Limit order     | Лимитный ордер     |
| `1`   | Market      | Market order    | Рыночный ордер     |
| `2`   | StopLimit   | Stop-limit order| Стоп-лимитный ордер|
| `3`   | StopMarket  | Stop-market order| Стоп-рыночный ордер|

### 7.3 TimeInForce / Время действия ордера

| Value | Name | EN                              | RU                                    |
|-------|------|---------------------------------|---------------------------------------|
| `0`   | GTC  | Good Till Cancelled             | Действителен до отмены                |
| `1`   | IOC  | Immediate Or Cancel             | Немедленно или отменить               |
| `2`   | FOK  | Fill Or Kill                    | Исполнить полностью или отменить      |

### 7.4 RejectReason / Причина отклонения

| Value | Name                   | EN                              | RU                                     |
|-------|------------------------|---------------------------------|----------------------------------------|
| `0`   | NoLiquidity            | No liquidity available          | Нет доступной ликвидности              |
| `1`   | InsufficientLiquidity  | Insufficient liquidity for FOK  | Недостаточная ликвидность для FOK      |
| `2`   | InvalidPrice           | Invalid price                   | Недопустимая цена                      |
| `3`   | InvalidQuantity        | Invalid quantity                | Недопустимое количество                |
| `4`   | InvalidLotSize         | Violates lot size constraint    | Нарушение ограничения размера лота     |
| `5`   | InvalidTickSize        | Violates tick size constraint   | Нарушение ограничения шага цены        |
| `6`   | OrderNotFound          | Order not found for cancel/amend| Ордер не найден для отмены/изменения   |
| `7`   | DuplicateOrderId       | Duplicate order ID              | Дублирующийся идентификатор ордера     |
| `8`   | SymbolNotFound         | Unknown symbol ID               | Неизвестный идентификатор символа      |
| `9`   | TradingHalted          | Trading is halted               | Торговля приостановлена                |
| `10`  | RiskCheckFailed        | Risk check failed               | Проверка риска не пройдена             |

### 7.5 ErrorCode / Код ошибки

| Value | Name                | EN                              | RU                                      |
|-------|---------------------|---------------------------------|-----------------------------------------|
| `1`   | UnsupportedVersion  | Unsupported protocol version    | Неподдерживаемая версия протокола       |
| `2`   | SequenceOutOfOrder  | Sequence number out of order    | Порядковый номер вне очереди            |
| `3`   | HandshakeRequired   | Handshake required before commands | Требуется рукопожатие перед командами |
| `4`   | InvalidPayload      | Payload is malformed            | Некорректная полезная нагрузка          |
| `5`   | ServerShuttingDown  | Server is shutting down         | Сервер завершает работу                 |
| `6`   | Backpressure        | Write buffer overflow           | Переполнение буфера записи              |

---

## 8. Fixed-Point Encoding / Кодирование с фиксированной точкой

All prices and quantities use **fixed-point integer encoding** with a scale factor
of **10^8** (100,000,000). This avoids floating-point rounding issues inherent in
financial calculations.

> **Русский:**
> Все цены и количества используют **целочисленное кодирование с фиксированной
> точкой** и масштабным коэффициентом **10^8** (100 000 000). Это позволяет
> избежать ошибок округления с плавающей точкой, свойственных финансовым
> вычислениям.

### Conversion Formulas / Формулы конвертации

```
wire_value = real_value * 100_000_000

real_value = wire_value / 100_000_000
```

### Examples / Примеры

| Real Value       | Wire Value (i64/u64) | Hex (LE)                   | Description              |
|------------------|----------------------|----------------------------|--------------------------|
| `50000.50`       | `5_000_050_000_000`  | `0x80B48ED48B040000`       | BTC price ~$50,000.50    |
| `0.5`            | `50_000_000`         | `0x80F0FA0200000000`       | 0.5 BTC quantity         |
| `1.00`           | `100_000_000`        | `0x00E1F50500000000`       | 1.0 unit                 |
| `0.00000001`     | `1`                  | `0x0100000000000000`       | Minimum representable    |
| `-100.25`        | `-10_025_000_000`    | signed i64                 | Negative price (unusual) |

> **Русский:**
>
> | Реальное значение | Значение на проводе (i64/u64) | Описание                   |
> |-------------------|-------------------------------|----------------------------|
> | `50000.50`        | `5_000_050_000_000`           | Цена BTC ~$50,000.50       |
> | `0.5`             | `50_000_000`                  | 0.5 BTC количество         |
> | `1.00`            | `100_000_000`                 | 1.0 единица                |
> | `0.00000001`      | `1`                           | Минимальное представимое    |

**Important:** Prices use `i64` (signed) to allow for potential negative values
or sentinel values. Quantities use `u64` (unsigned) and must always be positive.

> **Русский:** Цены используют `i64` (знаковый тип) для возможных отрицательных
> значений или сигнальных значений. Количества используют `u64` (беззнаковый)
> и всегда должны быть положительными.

---

## 9. Connection Lifecycle / Жизненный цикл соединения

### State Machine / Конечный автомат

```
                         +-------------------+
                         |                   |
          TCP connect    | HandshakePending  |
         +-------------->|                   |
                         +--------+----------+
                                  |
                    Handshake(0x05) sent by client
                    HandshakeAck(0x1A) received
                                  |
                                  v
                         +--------+----------+
                         |                   |
                         |      Active       |<---+
                         |                   |    |
                         +---+----------+----+    |
                             |          |         |
                    Error /  |          | Ping/   |
                    Timeout  |          | Pong    |
                             |          +---------+
                             v
                         +---+---------------+
                         |                   |
                         |     Closing       |
                         |                   |
                         +-------------------+
                                  |
                             TCP close
```

> **Русский:**
>
> ```
>                          +--------------------+
>                          |                    |
>           TCP соединение | Ожидание           |
>          +-------------->| рукопожатия        |
>                          +--------+-----------+
>                                   |
>                     Клиент отправляет Handshake(0x05)
>                     Получен HandshakeAck(0x1A)
>                                   |
>                                   v
>                          +--------+-----------+
>                          |                    |
>                          |     Активное       |<---+
>                          |                    |    |
>                          +---+-----------+----+    |
>                              |           |         |
>                     Ошибка / |           | Ping/   |
>                     Таймаут  |           | Pong    |
>                              |           +---------+
>                              v
>                          +---+----------------+
>                          |                    |
>                          |    Закрытие        |
>                          |                    |
>                          +--------------------+
>                                   |
>                              TCP закрытие
> ```

### Rules / Правила

1. **First frame must be Handshake (0x05):** The very first frame sent after TCP
   connection (excluding Ping) MUST be a Handshake. Sending any command
   (PlaceOrder, CancelOrder, AmendOrder) before handshake results in an Error
   frame with `HandshakeRequired` (code 3) followed by disconnection.

2. **Ping is always allowed:** Ping (0x04) may be sent in any state, including
   `HandshakePending`. The server always responds with Pong (0x18).

3. **Commands require Active state:** PlaceOrder (0x01), CancelOrder (0x02),
   and AmendOrder (0x03) may only be sent after a successful handshake
   (i.e., after receiving HandshakeAck).

4. **Closing state is terminal:** Once an Error frame is sent by the server that
   transitions to Closing, the connection will be terminated. No further
   messages are processed.

> **Русский:**
>
> 1. **Первый кадр -- Handshake (0x05):** Самый первый кадр после TCP-соединения
>    (за исключением Ping) ДОЛЖЕН быть Handshake. Отправка любой команды до
>    рукопожатия приведёт к кадру Error с кодом `HandshakeRequired` (3)
>    и разрыву соединения.
>
> 2. **Ping всегда разрешён:** Ping (0x04) может отправляться в любом состоянии,
>    включая `HandshakePending`. Сервер всегда отвечает Pong (0x18).
>
> 3. **Команды требуют активного состояния:** PlaceOrder (0x01), CancelOrder (0x02)
>    и AmendOrder (0x03) могут отправляться только после успешного рукопожатия
>    (т.е. после получения HandshakeAck).
>
> 4. **Состояние закрытия терминально:** Когда сервер отправляет кадр Error,
>    переводящий соединение в состояние закрытия, соединение будет разорвано.
>    Дальнейшие сообщения не обрабатываются.

---

## 10. Sequence Numbers / Порядковые номера

Each frame carries a `seq` field (u64 LE) in its header. Sequence numbers are
**per-connection** and must be **strictly monotonically increasing**.

> **Русский:** Каждый кадр содержит поле `seq` (u64 LE) в заголовке. Порядковые
> номера привязаны к **конкретному соединению** и должны быть **строго монотонно
> возрастающими**.

### Rules / Правила

| Rule                          | Description                                                        |
|-------------------------------|--------------------------------------------------------------------|
| First frame                   | `seq = 0` is allowed for the very first frame of a connection      |
| Subsequent frames             | `seq` must satisfy `seq > last_client_seq`                         |
| Violation                     | Error frame with `SequenceOutOfOrder` (code 2) and disconnect      |
| Server sequences              | Server also uses monotonically increasing seq in its responses     |

> **Русский:**
>
> | Правило                     | Описание                                                          |
> |-----------------------------|-------------------------------------------------------------------|
> | Первый кадр                 | `seq = 0` допустим для самого первого кадра соединения            |
> | Последующие кадры           | `seq` должен удовлетворять `seq > last_client_seq`                |
> | Нарушение                   | Кадр Error с кодом `SequenceOutOfOrder` (2) и разрыв соединения  |
> | Серверные последовательности| Сервер также использует монотонно возрастающий seq в своих ответах |

**Example / Пример:**

```
Client sends: seq=0 (Handshake)  -- OK
Client sends: seq=1 (PlaceOrder) -- OK
Client sends: seq=2 (PlaceOrder) -- OK
Client sends: seq=2 (CancelOrder)-- ERROR: SequenceOutOfOrder (must be > 2)
Client sends: seq=5 (CancelOrder)-- OK (gaps are allowed, just must be >2)
```

---

## 11. Backpressure / Обратное давление

If the server's per-connection write buffer exceeds `MAX_WRITE_BUFFER_SIZE`
(256 KB / 262,144 bytes), the server will:

1. Send an Error frame with `error_code = 6` (Backpressure)
2. Immediately close the TCP connection

This protects the server from slow consumers that cannot drain outbound data
fast enough.

> **Русский:**
> Если буфер записи сервера для конкретного соединения превышает
> `MAX_WRITE_BUFFER_SIZE` (256 КБ / 262 144 байта), сервер:
>
> 1. Отправляет кадр Error с `error_code = 6` (Backpressure)
> 2. Немедленно закрывает TCP-соединение
>
> Это защищает сервер от медленных потребителей, которые не успевают
> считывать исходящие данные.

**Client mitigation strategies / Стратегии смягчения для клиента:**

- Ensure the client reads from the socket promptly (dedicated reader thread/task).
- Avoid blocking the network read path with heavy computation.
- Consider increasing the client's TCP receive buffer size.

> **Русский:**
> - Обеспечьте своевременное чтение из сокета (выделенный поток/задача для чтения).
> - Не блокируйте путь чтения из сети тяжёлыми вычислениями.
> - Рассмотрите увеличение размера TCP-буфера приёма на стороне клиента.

---

## 12. Hex Dump Examples / Примеры шестнадцатеричных дампов

All examples show complete frames (header + payload) as they appear on the wire.

> **Русский:** Все примеры показывают полные кадры (заголовок + нагрузка) в том
> виде, в каком они передаются по сети.

### 12.1 Handshake (Client -> Engine)

A Handshake with `client_version=2`, `seq=0`, `req_id=0`:

```
Offset  Hex
------  -----------------------------------------------
0000    01 00 00 00                                       len = 1
0004    02                                                ver = 2
0005    05                                                type = Handshake (0x05)
0006    00 00 00 00 00 00 00 00                           seq = 0
000E    00 00 00 00 00 00 00 00                           req_id = 0

        --- payload (1 byte) ---
0016    02                                                client_version = 2

Total frame: 23 bytes (22 header + 1 payload)
```

### 12.2 HandshakeAck (Engine -> Client)

Response with `server_version=2`, `seq=0`:

```
Offset  Hex
------  -----------------------------------------------
0000    29 00 00 00                                       len = 41
0004    02                                                ver = 2
0005    1A                                                type = HandshakeAck (0x1A)
0006    00 00 00 00 00 00 00 00                           seq = 0
000E    00 00 00 00 00 00 00 00                           req_id = 0

        --- payload (41 bytes) ---
0016    02                                                server_version = 2
0017    01 00  08 00  xx xx xx xx xx xx xx xx             TLV: capabilities (tag=0x0001, len=8, value=u64)
0023    02 00  10 00  xx xx xx xx xx xx xx xx             TLV: session_id (tag=0x0002, len=16, value=[u8;16])
              xx xx xx xx xx xx xx xx
0037    03 00  04 00  xx xx xx xx                         TLV: heartbeat_ms (tag=0x0003, len=4, value=u32)

Total frame: 63 bytes (22 header + 41 payload)
```

### 12.3 Ping (Client -> Engine)

Ping with `seq=1`, piggyback `ack_seq=5` (client has processed server frames up to seq 5):

```
Offset  Hex
------  -----------------------------------------------
0000    08 00 00 00                                       len = 8
0004    02                                                ver = 2
0005    04                                                type = Ping (0x04)
0006    01 00 00 00 00 00 00 00                           seq = 1
000E    00 00 00 00 00 00 00 00                           req_id = 0

        --- payload (8 bytes) ---
0016    05 00 00 00 00 00 00 00                           ack_seq = 5 (last processed server seq)

Total frame: 30 bytes (22 header + 8 payload)
```

> **Note:** Ping payload is optional. A 0-byte payload (v1 compat) is accepted; `ack_seq` defaults to 0.

### 12.4 Pong (Engine -> Client)

Pong with `seq=1`, `last_processed_client_seq=1`:

```
Offset  Hex
------  -----------------------------------------------
0000    08 00 00 00                                       len = 8
0004    02                                                ver = 2
0005    18                                                type = Pong (0x18)
0006    01 00 00 00 00 00 00 00                           seq = 1
000E    00 00 00 00 00 00 00 00                           req_id = 0

        --- payload (8 bytes) ---
0016    01 00 00 00 00 00 00 00                           last_processed_client_seq = 1

Total frame: 30 bytes (22 header + 8 payload)
```

### 12.5 PlaceOrder -- Limit Buy 1.0 BTC @ 50,000.00

Place a Limit Buy order: `order_id=42`, `symbol_id=1` (e.g., BTCUSD),
`side=Buy(0)`, `type=Limit(0)`, `tif=GTC(0)`,
`price=50000.00` (wire: 5,000,000,000,000 = 0x0000048C27395000),
`stop_price=0`, `qty=1.0` (wire: 100,000,000 = 0x00000000_05F5E100),
`seq=1`:

```
Offset  Hex
------  -----------------------------------------------
0000    25 00 00 00                                       len = 37
0004    02                                                ver = 2
0005    01                                                type = PlaceOrder (0x01)
0006    01 00 00 00 00 00 00 00                           seq = 1
000E    01 00 00 00 00 00 00 00                           req_id = 1

        --- payload (37 bytes) ---
0016    2A 00 00 00 00 00 00 00                           order_id = 42
001E    01 00                                             symbol_id = 1
0020    00                                                side = Buy (0)
0021    00                                                order_type = Limit (0)
0022    00                                                tif = GTC (0)
0023    00 50 39 27 8C 04 00 00                           price = 5_000_000_000_000
002B    00 00 00 00 00 00 00 00                           stop_price = 0
0033    00 E1 F5 05 00 00 00 00                           qty = 100_000_000 (1.0)

Total frame: 59 bytes (22 header + 37 payload)
```

> **Русский:** Размещение лимитного ордера на покупку: `order_id=42`, `symbol_id=1`
> (напр. BTCUSD), `side=Buy(0)`, `type=Limit(0)`, `tif=GTC(0)`,
> `price=50000.00`, `stop_price=0`, `qty=1.0`, `seq=1`.

### 12.6 CancelOrder

Cancel order 42 on symbol 1, `seq=2`:

```
Offset  Hex
------  -----------------------------------------------
0000    0A 00 00 00                                       len = 10
0004    02                                                ver = 2
0005    02                                                type = CancelOrder (0x02)
0006    02 00 00 00 00 00 00 00                           seq = 2
000E    02 00 00 00 00 00 00 00                           req_id = 2

        --- payload (10 bytes) ---
0016    2A 00 00 00 00 00 00 00                           order_id = 42
001E    01 00                                             symbol_id = 1

Total frame: 32 bytes (22 header + 10 payload)
```

### 12.7 Trade Execution

Trade: `trade_id=1001`, `symbol_id=1`, `price=50000.00`,
`qty=0.5` (wire: 50,000,000), `maker_order_id=42`, `taker_order_id=99`,
`maker_side=Buy(0)`, `timestamp=1700000000000000000` (~2023-11-14), `seq=3`:

```
Offset  Hex
------  -----------------------------------------------
0000    3B 00 00 00                                       len = 59
0004    02                                                ver = 2
0005    12                                                type = Trade (0x12)
0006    03 00 00 00 00 00 00 00                           seq = 3
000E    01 00 00 00 00 00 00 00                           req_id = 1 (mirrored from triggering command)

        --- payload (59 bytes) ---
0016    E9 03 00 00 00 00 00 00                           trade_id = 1001
001E    01 00                                             symbol_id = 1
0020    00 50 39 27 8C 04 00 00                           price = 5_000_000_000_000
0028    00 65 CD 1D 00 00 00 00                           qty = 50_000_000 (0.5)
0030    2A 00 00 00 00 00 00 00                           maker_order_id = 42
0038    63 00 00 00 00 00 00 00                           taker_order_id = 99
0040    00                                                maker_side = Buy (0)
0041    00 00 1C E9 0A 68 99 17                           timestamp = 1700000000000000000
0049    01 00 00 00 00 00 00 00                           sequence = 1

Total frame: 81 bytes (22 header + 59 payload)
```

### 12.8 OrderRejected -- DuplicateOrderId

Reject order 42 with `DuplicateOrderId(7)`, `timestamp=1700000000000000000`, `seq=2`:

```
Offset  Hex
------  -----------------------------------------------
0000    11 00 00 00                                       len = 17
0004    02                                                ver = 2
0005    11                                                type = OrderRejected (0x11)
0006    02 00 00 00 00 00 00 00                           seq = 2
000E    01 00 00 00 00 00 00 00                           req_id = 1 (mirrored from client command)

        --- payload (17 bytes) ---
0016    2A 00 00 00 00 00 00 00                           order_id = 42
001E    07                                                reason = DuplicateOrderId (7)
001F    00 00 1C E9 0A 68 99 17                           timestamp = 1700000000000000000

Total frame: 39 bytes (22 header + 17 payload)
```

### 12.9 Error -- HandshakeRequired

Client sent a command before Handshake. Error with code 3, message "handshake required":

```
Offset  Hex
------  -----------------------------------------------
0000    16 00 00 00                                       len = 22
0004    02                                                ver = 2
0005    19                                                type = Error (0x19)
0006    00 00 00 00 00 00 00 00                           seq = 0
000E    00 00 00 00 00 00 00 00                           req_id = 0 (service frame)

        --- payload (22 bytes) ---
0016    03 00                                             error_code = 3 (HandshakeRequired)
0018    12 00                                             msg_len = 18
001A    68 61 6E 64 73 68 61 6B 65 20 72 65 71 75 69     "handshake requi"
0029    72 65 64                                          "red"

Total frame: 44 bytes (22 header + 22 payload)
```

### 12.10 AmendOrder -- Price and Quantity

Amend order 42 on symbol 1 with new price 51000.00 and new qty 0.75, `seq=3`:

```
Price = 51000.00 => 5_100_000_000_000 = 0x0000049D44A90000 (LE: 00 00 A9 44 9D 04 00 00)
Qty   = 0.75     =>        75_000_000 = 0x00000000047868C0 (LE: C0 68 78 04 00 00 00 00)

Offset  Hex
------  -----------------------------------------------
0000    1C 00 00 00                                       len = 28
0004    02                                                ver = 2
0005    03                                                type = AmendOrder (0x03)
0006    03 00 00 00 00 00 00 00                           seq = 3
000E    03 00 00 00 00 00 00 00                           req_id = 3

        --- payload (28 bytes) ---
0016    2A 00 00 00 00 00 00 00                           order_id = 42
001E    01 00                                             symbol_id = 1
0020    01                                                has_price = 1 (yes)
0021    00 00 A9 44 9D 04 00 00                           price = 5_100_000_000_000
0029    01                                                has_qty = 1 (yes)
002A    C0 68 78 04 00 00 00 00                           qty = 75_000_000 (0.75)

Total frame: 50 bytes (22 header + 28 payload)
```

---

## 13. Implementation Notes / Примечания по реализации

### 13.1 Reading Frames / Чтение кадров

Pseudocode for reading a single frame from a TCP stream:

> **Русский:** Псевдокод чтения одного кадра из TCP-потока:

```
function read_frame(stream):
    // 1. Read the 22-byte header (v2)
    header = stream.read_exact(22)

    // 2. Parse header fields (all little-endian)
    payload_len = u32_from_le(header[0..4])
    version     = header[4]
    msg_type    = header[5]
    seq         = u64_from_le(header[6..14])
    req_id      = u64_from_le(header[14..22])

    // 3. Validate
    if version != 2:
        error("unsupported protocol version")
    if payload_len > 1024:
        error("payload exceeds MAX_PAYLOAD_SIZE")

    // 4. Read payload
    payload = stream.read_exact(payload_len)

    return Frame { version, msg_type, seq, req_id, payload }
```

### 13.2 Writing Frames / Запись кадров

```
function write_frame(stream, msg_type, seq, req_id, payload):
    // 1. Build the 22-byte header (v2)
    header = []
    header.append(u32_to_le(len(payload)))   // 4 bytes: payload length
    header.append(0x02)                       // 1 byte:  version
    header.append(msg_type)                   // 1 byte:  type
    header.append(u64_to_le(seq))            // 8 bytes: sequence
    header.append(u64_to_le(req_id))         // 8 bytes: request id

    // 2. Write header + payload atomically
    stream.write(header + payload)
```

### 13.3 Typical Session Flow / Типичный поток сеанса

```
Client                                          Engine
  |                                               |
  |--- [Handshake, seq=0, ver=2] --------------->|
  |                                               |
  |<-- [HandshakeAck, seq=0, server_ver=2] ------|  (state -> Active)
  |                                               |
  |--- [PlaceOrder, seq=1, Buy 1.0 @ 50000] --->|
  |                                               |
  |<-- [OrderAccepted, seq=1, oid=42, ts=...] ---|
  |                                               |
  |--- [PlaceOrder, seq=2, Sell 0.5 @ 50000] --->|
  |                                               |
  |<-- [OrderAccepted, seq=2, oid=43, ts=...] ---|
  |<-- [Trade, seq=3, tid=1, 0.5@50000] ---------|
  |<-- [OrderPartiallyFilled, seq=4, oid=42] ----|
  |<-- [OrderFilled, seq=5, oid=43] -------------|
  |                                               |
  |--- [CancelOrder, seq=3, oid=42] ------------>|
  |                                               |
  |<-- [OrderCancelled, seq=6, oid=42, rem=0.5] -|
  |                                               |
  |--- [Ping, seq=4] --------------------------->|
  |                                               |
  |<-- [Pong, seq=7] ----------------------------|
  |                                               |
```

> **Русский:**
>
> ```
> Клиент                                         Движок
>   |                                               |
>   |--- [Handshake, seq=0, ver=2] --------------->|
>   |                                               |
>   |<-- [HandshakeAck, seq=0, server_ver=2] ------|  (состояние -> Активное)
>   |                                               |
>   |--- [PlaceOrder, seq=1, Buy 1.0 @ 50000] --->|
>   |                                               |
>   |<-- [OrderAccepted, seq=1, oid=42, ts=...] ---|
>   |                                               |
>   |--- [PlaceOrder, seq=2, Sell 0.5 @ 50000] --->|
>   |                                               |
>   |<-- [OrderAccepted, seq=2, oid=43, ts=...] ---|
>   |<-- [Trade, seq=3, tid=1, 0.5@50000] ---------|
>   |<-- [OrderPartiallyFilled, seq=4, oid=42] ----|
>   |<-- [OrderFilled, seq=5, oid=43] -------------|
>   |                                               |
>   |--- [CancelOrder, seq=3, oid=42] ------------>|
>   |                                               |
>   |<-- [OrderCancelled, seq=6, oid=42, rem=0.5] -|
>   |                                               |
>   |--- [Ping, seq=4] --------------------------->|
>   |                                               |
>   |<-- [Pong, seq=7] ----------------------------|
>   |                                               |
> ```

### 13.4 Endianness Checklist / Контрольный список порядка байтов

All multi-byte integers on the wire are **little-endian**. When implementing:

| Language    | LE Read Function                                      |
|-------------|-------------------------------------------------------|
| C           | On LE arch: direct cast. On BE: `le32toh()`, `le64toh()` |
| C++         | `std::bit_cast` or `memcpy` + byte swap if needed     |
| Rust        | `u32::from_le_bytes()`, `i64::from_le_bytes()`        |
| Python      | `struct.unpack('<I', ...)`, `struct.unpack('<q', ...)`  |
| Go          | `binary.LittleEndian.Uint32()`, `.Uint64()`           |
| Java        | `ByteBuffer.order(ByteOrder.LITTLE_ENDIAN)`           |
| C#          | `BinaryReader` (default LE), `BitConverter`           |
| JavaScript  | `DataView.getUint32(offset, true)` (true = LE)        |

> **Русский:**
> Все многобайтовые целые числа на проводе -- **little-endian**. При реализации
> используйте соответствующие функции вашего языка для чтения/записи в формате
> little-endian (см. таблицу выше).

### 13.5 Error Handling Strategy / Стратегия обработки ошибок

| Error Condition                    | Server Response                | Recoverable? |
|------------------------------------|---------------------------------|-------------|
| Unknown message type               | Error(InvalidPayload) + close  | No          |
| Payload too short for message type | Error(InvalidPayload) + close  | No          |
| Payload exceeds MAX_PAYLOAD_SIZE   | Error(InvalidPayload) + close  | No          |
| Unsupported protocol version       | Error(UnsupportedVersion) + close | No       |
| Sequence out of order              | Error(SequenceOutOfOrder) + close | No       |
| Command before handshake           | Error(HandshakeRequired) + close | No        |
| Write buffer overflow              | Error(Backpressure) + close    | No          |
| Server shutting down               | Error(ServerShuttingDown) + close | No       |
| Invalid order parameters           | OrderRejected (specific reason)| Yes         |
| Order not found for cancel/amend   | OrderRejected(OrderNotFound)   | Yes         |

> **Русский:**
>
> | Условие ошибки                    | Ответ сервера                   | Восстановимо? |
> |-----------------------------------|---------------------------------|---------------|
> | Неизвестный тип сообщения         | Error(InvalidPayload) + закрытие| Нет           |
> | Нагрузка слишком короткая         | Error(InvalidPayload) + закрытие| Нет           |
> | Нагрузка превышает максимум       | Error(InvalidPayload) + закрытие| Нет           |
> | Неподдерживаемая версия протокола | Error(UnsupportedVersion) + закрытие | Нет      |
> | Порядковый номер вне очереди      | Error(SequenceOutOfOrder) + закрытие | Нет      |
> | Команда до рукопожатия            | Error(HandshakeRequired) + закрытие | Нет       |
> | Переполнение буфера записи        | Error(Backpressure) + закрытие  | Нет           |
> | Сервер завершает работу           | Error(ServerShuttingDown) + закрытие | Нет      |
> | Некорректные параметры ордера     | OrderRejected (конкретная причина)| Да          |
> | Ордер не найден для отмены/правки | OrderRejected(OrderNotFound)    | Да            |

**Key distinction:** Protocol-level errors (Error frame) cause disconnection.
Business-level rejections (OrderRejected frame) do NOT disconnect -- the client
can continue operating.

> **Русский:** Ключевое различие: ошибки уровня протокола (кадр Error) приводят
> к разрыву соединения. Отклонения бизнес-уровня (кадр OrderRejected) НЕ
> разрывают соединение -- клиент может продолжать работу.

---

## Appendix A: Quick Reference Card / Краткая справочная карта

```
FRAME HEADER (22 bytes, always present):
+------+-----+------+----------+----------+
| len  | ver | type |   seq    |  req_id  |
| 4B   | 1B  | 1B   |   8B     |   8B     |
+------+-----+------+----------+----------+
  u32    u8    u8      u64       u64
  LE                   LE        LE

INBOUND MESSAGE TYPES:
  0x01 PlaceOrder       37-38B  order_id(8)+sym(2)+side(1)+type(1)+tif(1)+price(8)+stop(8)+qty(8)+[flags(1)]
  0x02 CancelOrder       10B   order_id(8)+sym(2)
  0x03 AmendOrder        var   order_id(8)+sym(2)+has_p(1)+[price(8)]+has_q(1)+[qty(8)]
  0x04 Ping             0|8B   [ack_seq(8)]  (piggyback cumulative ACK)
  0x05 Handshake        0-1B   [client_version(1)]
  0x06 ResumeSession     24B   session_id(16)+last_recv_seq(8)
  0x07 GetOrderStatus    10B   order_id(8)+sym(2)
  0x08 Acknowledge        8B   last_processed_server_seq(8)
  0x09 GetOpenOrders      0B   (empty)
  0x0B BatchPlaceOrder   var   count(1)+[PlaceOrder×N]  (max 20)
  0x0C BatchCancelOrder  var   count(1)+[CancelOrder×N] (max 50)

OUTBOUND MESSAGE TYPES:
  0x10 OrderAccepted     16B   order_id(8)+ts(8)
  0x11 OrderRejected     17B   order_id(8)+reason(1)+ts(8)
  0x12 Trade             59B   trade_id(8)+sym(2)+price(8)+qty(8)+maker(8)+taker(8)+side(1)+ts(8)+seq(8)
  0x13 OrderFilled       32B   order_id(8)+filled_qty(8)+price(8)+ts(8)
  0x14 OrderPartFilled   40B   order_id(8)+filled(8)+remaining(8)+price(8)+ts(8)
  0x15 OrderCancelled    24B   order_id(8)+remaining(8)+ts(8)
  0x16 OrderAmended      32B   order_id(8)+new_price(8)+new_qty(8)+ts(8)
  0x17 StopTriggered     24B   order_id(8)+trigger_price(8)+ts(8)
  0x18 Pong               8B   last_processed_client_seq(8)
  0x19 Error             var   error_code(2)+msg_len(2)+msg(N)
  0x1A HandshakeAck      41B   server_ver(1)+TLV(capabilities+session_id+heartbeat_ms)
  0x1B SessionResumed     4B   replayed_count(4)
  0x1D OrderStatus       41B   order_id(8)+status(1)+filled(8)+remaining(8)+price(8)+ts(8)
  0x1E OrderNotFound      8B   order_id(8)
  0x1F OpenOrdersBegin   12B   count(4)+snapshot_seq(8)
  0x20 ReplayGap         16B   missing_from(8)+missing_to(8)
  0x21 OpenOrder         60B   fixed wire format (see spec)
  0x22 OpenOrdersEnd      8B   snapshot_seq(8)
  0x23 FlowControlWarn    2B   buffer_pct(1)+is_hard(1)

FIXED-POINT: scale = 10^8, prices = i64, quantities = u64, all LE
STATES: HandshakePending -> Active -> Closing
BACKPRESSURE: write buffer > 256KB -> Error(6) -> disconnect
```

---

*End of Protocol Specification / Конец спецификации протокола*
