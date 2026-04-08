# me-io

Бинарный TCP/UDS протокол v2 для matching-engine. Реализует length-prefixed framing с версионированием, sequence numbers, request_id для идемпотентности и session management.

## Возможности

| Компонент | Описание |
|-----------|----------|
| Frame codec | Encode/decode бинарных фреймов с заголовком **22 байт** (v2) |
| Handshake | Обязательный handshake с TLV-расширениями (capabilities, session_id, heartbeat) |
| Session recovery | ResumeSession + replay буфер для восстановления после разрыва |
| Sequence numbers | Монотонно возрастающие номера + request_id для дедупликации |
| Inbound commands | PlaceOrder, CancelOrder, AmendOrder, Ping, Handshake, ResumeSession, GetOrderStatus, Acknowledge, GetOpenOrders, BatchPlaceOrder, BatchCancelOrder |
| Outbound events | Trade, OrderAccepted, OrderFilled, StopTriggered, HandshakeAck, SessionResumed, OrderStatus, OpenOrders snapshot и др. |
| Error frames | 12 кодов ошибок (UnsupportedVersion..InternalError) |
| TLS | Опциональная поддержка TLS и mTLS (feature `tls`) |

## Формат фрейма (v2)

```
┌──────────┬──────────┬──────────┬──────────┬─────────────┬─────────────────┐
│ len: u32 │ ver: u8  │ type: u8 │ seq: u64 │ req_id: u64 │ payload: [u8]   │
│ 4 bytes  │ 1 byte   │ 1 byte   │ 8 bytes  │ 8 bytes     │ variable        │
└──────────┴──────────┴──────────┴──────────┴─────────────┴─────────────────┘
```

- `len` — размер payload в байтах (не включает 22-байт заголовок)
- `ver` — версия протокола (текущая: **2**)
- `type` — тип сообщения (см. `MessageType`)
- `seq` — номер последовательности, строго возрастающий
- `req_id` — ключ идемпотентности (обязателен для команд и запросов, 0 для служебных: Handshake, Ping, Acknowledge)

## Handshake sequence (v2)

```
Client                                   Server
  │──── Handshake (seq=0, ver=2) ────────▶│
  │◀─── HandshakeAck (41B TLV payload) ───│  (session_id, capabilities, heartbeat_ms)
  │──── PlaceOrder (seq=1, req_id=1) ────▶│
  │◀─── OrderAccepted ────────────────────│
  │◀─── Trade ─────────────────────────────│
  │──── Ping (seq=2, ack_seq=2) ──────────▶│
  │◀─── Pong ──────────────────────────────│
```

Session recovery:
```
Client                                   Server
  │──── ResumeSession (session_id, last_seq) ▶│
  │◀─── SessionResumed (replayed_count=3) ────│
  │◀─── [replay frame 1] ─────────────────────│
  │◀─── [replay frame 2] ─────────────────────│
  │◀─── [replay frame 3] ─────────────────────│
  │     (normal flow resumes)                  │
```

Команды до handshake вернут `DecodeError::HandshakeRequired`.
