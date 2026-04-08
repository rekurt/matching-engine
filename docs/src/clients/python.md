# Клиент на Python

Zero-dependency реализация на Python 3.8+, работающая напрямую с бинарным
протоколом через raw socket и стандартный модуль `struct`. Без сторонних
библиотек — отлично подходит для понимания протокола изнутри.

---

## Предварительные требования

- Python 3.8+
- Запущенный экземпляр me-server

---

## Запуск примера

```bash
python3 examples/python/client.py [host] [port]
# по умолчанию: 127.0.0.1 9090
```

Ожидаемый вывод:

```
============================================================
Matching Engine -- Python Client Example
============================================================
[*] Connected to 127.0.0.1:9090
[*] Handshake OK -- server version 2
[>] PlaceOrder  id=1  symbol=1  Buy  price=50000.0  qty=1.0
  [<] OrderAccepted  order_id=1  ts=2024-01-01 12:00:00  seq=1
[>] PlaceOrder  id=2  symbol=1  Sell  price=49999.0  qty=0.5
  [<] OrderAccepted  order_id=2  ts=...  seq=2
  [<] Trade  id=1  symbol=1  price=50000.0  qty=0.5  maker=1  taker=2  ...
  [<] OrderPartiallyFilled  order_id=1  filled=0.5  remaining=0.5  ...
  [<] OrderFilled  order_id=2  filled_qty=0.5  price=50000.0  ...
[*] Demo complete.
[*] Connection closed
```

---

## Архитектура клиента

### Константы протокола

```python
PROTOCOL_VERSION = 2
HEADER_SIZE = 22        # len(4) + ver(1) + type(1) + seq(8) + req_id(8)
PRICE_SCALE = 10**8     # фиксированная точка

# Команды клиента → движок
MSG_PLACE_ORDER    = 0x01
MSG_CANCEL_ORDER   = 0x02
MSG_AMEND_ORDER    = 0x03
MSG_PING           = 0x04
MSG_HANDSHAKE      = 0x05
MSG_RESUME_SESSION = 0x06
MSG_GET_ORDER_STATUS = 0x07
MSG_ACKNOWLEDGE    = 0x08
MSG_GET_OPEN_ORDERS = 0x09

# События движок → клиент
MSG_ORDER_ACCEPTED        = 0x10
MSG_ORDER_REJECTED        = 0x11
MSG_TRADE                 = 0x12
MSG_ORDER_FILLED          = 0x13
MSG_ORDER_PARTIALLY_FILLED = 0x14
MSG_ORDER_CANCELLED       = 0x15
MSG_STOP_TRIGGERED        = 0x17
MSG_PONG                  = 0x18
MSG_ERROR                 = 0x19
MSG_HANDSHAKE_ACK         = 0x1A
MSG_SESSION_RESUMED       = 0x1B
```

### Фиксированная точка

```python
def price_to_raw(price: float) -> int:
    return int(round(price * 10**8))

def raw_to_price(raw: int) -> float:
    return raw / 10**8

# Примеры:
# price_to_raw(50000.50) → 5_000_050_000_000
# raw_to_price(150_000_000) → 1.5
```

---

## Шаг 1 — Подключение и Handshake

```python
import socket, struct

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
sock.connect(("127.0.0.1", 9090))

# Отправить Handshake (тип 0x05, payload: client_version=2)
def send_frame(sock, msg_type, payload, seq, req_id=0):
    header = struct.pack("<IBBQQ", len(payload), 2, msg_type, seq, req_id)
    sock.sendall(header + payload)

send_frame(sock, 0x05, struct.pack("B", 2), seq=0)

# Прочитать HandshakeAck (22-байт заголовок + 41-байт payload)
header = sock.recv(22)
payload_len, ver, msg_type, seq, req_id = struct.unpack("<IBBQQ", header)
payload = sock.recv(payload_len) if payload_len > 0 else b""
assert msg_type == 0x1A, "Expected HandshakeAck"
server_version = payload[0]
print(f"Handshake OK, server version {server_version}")
```

---

## Шаг 2 — Буферизованное чтение фреймов

TCP — поточный протокол, данные могут приходить в произвольных кусках.
Правильный подход — накапливать данные в буфере и парсить только полные фреймы:

```python
buf = bytearray()

def read_frame(sock, timeout=5.0):
    """Блокирует до получения полного фрейма."""
    sock.settimeout(timeout)
    while True:
        # Проверяем есть ли полный заголовок (22 байта для v2)
        if len(buf) >= 22:
            payload_len, ver, msg_type, seq, req_id = struct.unpack_from("<IBBQQ", buf, 0)
            total = 22 + payload_len
            if len(buf) >= total:
                payload = bytes(buf[22:total])
                del buf[:total]          # потребить фрейм из буфера
                return msg_type, seq, req_id, payload
        # Нужно больше данных
        chunk = sock.recv(4096)
        if not chunk:
            raise ConnectionError("Connection closed")
        buf.extend(chunk)
```

---

## Шаг 3 — Отправка PlaceOrder

PlaceOrder payload — 37 байт, little-endian:

```
order_id   : u64  (8 байт)
symbol_id  : u16  (2 байта)
side       : u8   (1 байт)   0=Buy, 1=Sell
order_type : u8   (1 байт)   0=Limit, 1=Market, 2=StopLimit, 3=StopMarket
tif        : u8   (1 байт)   0=GTC, 1=IOC, 2=FOK
price      : i64  (8 байт)   fixed-point 10^8
stop_price : i64  (8 байт)   fixed-point 10^8 (0 если не стоп)
qty        : u64  (8 байт)   fixed-point 10^8
```

```python
seq = 2       # следующий порядковый номер
req_id = 1    # ненулевой req_id обязателен для торговых команд

payload = struct.pack(
    "<QHBBBqqQ",
    1001,                          # order_id
    0,                             # symbol_id (BTC/USDT)
    0,                             # side: Buy
    0,                             # order_type: Limit
    0,                             # tif: GTC
    price_to_raw(50_000.0),        # price
    0,                             # stop_price (нет)
    price_to_raw(1.5),             # qty: 1.5 BTC
)
assert len(payload) == 37

send_frame(sock, 0x01, payload, seq, req_id=req_id)
seq += 1
req_id += 1
```

---

## Шаг 4 — Отправка CancelOrder

CancelOrder payload — 10 байт:

```python
payload = struct.pack("<QH", 1001, 0)  # order_id, symbol_id
send_frame(sock, 0x02, payload, seq, req_id=req_id)
seq += 1
req_id += 1
```

---

## Шаг 5 — Декодирование событий

```python
MSG_NAMES = {
    0x10: "OrderAccepted", 0x11: "OrderRejected",
    0x12: "Trade",          0x13: "OrderFilled",
    0x14: "OrderPartiallyFilled", 0x15: "OrderCancelled",
    0x17: "StopTriggered",  0x18: "Pong",
    0x1A: "HandshakeAck",
}

REJECT_REASONS = {
    0: "NoLiquidity", 1: "InsufficientLiquidity",
    2: "InvalidPrice", 3: "InvalidQuantity",
    6: "OrderNotFound", 7: "DuplicateOrderId",
    10: "RiskCheckFailed",
}

def decode_event(msg_type, seq, payload):
    name = MSG_NAMES.get(msg_type, f"Unknown(0x{msg_type:02x})")

    if msg_type == 0x10:  # OrderAccepted
        oid, ts = struct.unpack_from("<QQ", payload)
        return {"type": name, "seq": seq, "order_id": oid, "ts": ts}

    elif msg_type == 0x11:  # OrderRejected
        oid, reason, ts = struct.unpack_from("<QBQ", payload)
        return {"type": name, "seq": seq, "order_id": oid,
                "reason": REJECT_REASONS.get(reason, reason)}

    elif msg_type == 0x12:  # Trade
        (trade_id, sym, price_raw, qty_raw,
         maker_oid, taker_oid, maker_side, ts) = struct.unpack_from("<QHqQQQBQ", payload)
        return {"type": name, "seq": seq, "trade_id": trade_id,
                "price": raw_to_price(price_raw), "qty": raw_to_price(qty_raw),
                "maker_order_id": maker_oid, "taker_order_id": taker_oid}

    elif msg_type == 0x13:  # OrderFilled
        oid, filled_raw, price_raw, ts = struct.unpack_from("<QQqQ", payload)
        return {"type": name, "seq": seq, "order_id": oid,
                "filled_qty": raw_to_price(filled_raw),
                "price": raw_to_price(price_raw)}

    elif msg_type == 0x14:  # OrderPartiallyFilled
        oid, filled_raw, rem_raw, price_raw, ts = struct.unpack_from("<QQQqQ", payload)
        return {"type": name, "seq": seq, "order_id": oid,
                "filled_qty": raw_to_price(filled_raw),
                "remaining_qty": raw_to_price(rem_raw),
                "price": raw_to_price(price_raw)}

    elif msg_type == 0x15:  # OrderCancelled
        oid, remaining_raw, ts = struct.unpack_from("<QQQ", payload)
        return {"type": name, "seq": seq, "order_id": oid,
                "remaining_qty": raw_to_price(remaining_raw)}

    elif msg_type == 0x17:  # StopTriggered
        oid, trigger_raw, ts = struct.unpack_from("<QqQ", payload)
        return {"type": name, "seq": seq, "order_id": oid,
                "trigger_price": raw_to_price(trigger_raw)}

    else:
        return {"type": name, "seq": seq}
```

---

## Полный минимальный пример

```python
#!/usr/bin/env python3
import socket, struct

SCALE = 10**8
seq = 0
req_counter = 0

def to_raw(val): return int(round(val * SCALE))

def send(sock, t, payload, req_id=0):
    """Отправить v2-фрейм (22-байт заголовок + payload)."""
    global seq; seq += 1
    sock.sendall(struct.pack("<IBBQQ", len(payload), 2, t, seq, req_id) + payload)

def recv_frame(sock):
    """Прочитать один v2-фрейм (22-байт заголовок)."""
    hdr = sock.recv(22)
    pl_len, ver, msg_type, s, rid = struct.unpack("<IBBQQ", hdr)
    payload = sock.recv(pl_len) if pl_len else b""
    return msg_type, s, rid, payload

sock = socket.socket()
sock.connect(("127.0.0.1", 9090))

# Handshake (req_id=0 для служебных сообщений)
send(sock, 0x05, struct.pack("B", 2))
msg_type, _, _, _ = recv_frame(sock)
assert msg_type == 0x1A, "Expected HandshakeAck"

# Buy 1 BTC @ $50k (req_id=1 для торговых команд)
req_counter += 1
send(sock, 0x01, struct.pack("<QHBBBqqQ", 1, 0, 0, 0, 0, to_raw(50000), 0, to_raw(1.0)), req_id=req_counter)
msg_type, _, rid, _ = recv_frame(sock)
print("OrderAccepted" if msg_type == 0x10 else f"Unexpected: 0x{msg_type:02x}")
assert rid == req_counter, "req_id must mirror the command"

sock.close()
```

---

## Полный рабочий пример

Файл [`examples/python/client.py`](../../../../examples/python/client.py) содержит
полную реализацию класса `MEClient` с правильной буферизацией, декодированием
всех типов событий и демо-сценарием.

---

## Рекомендации для продакшена

- Используйте `asyncio` или `trio` вместо blocking сокетов для высокой нагрузки
- Добавьте reconnect-логику с экспоненциальным backoff
- Логируйте все входящие sequence numbers для обнаружения пропусков

---

## Что дальше

- [Клиент на Go](./go.md) — высокоуровневая библиотека meclient
- [Обзор протокола](../integration.md) — полное описание жизненного цикла
- [Бинарный протокол](../protocol.md) — hex-дампы, все payload-структуры
