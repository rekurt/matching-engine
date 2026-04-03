# me-io

Бинарный TCP протокол для matching-engine. Реализует length-prefixed framing с версионированием, sequence numbers и state machine подключения.

## Возможности

| Компонент | Описание |
|-----------|----------|
| Frame codec | Encode/decode бинарных фреймов с заголовком 14 байт |
| Handshake | Обязательный handshake перед отправкой команд |
| Sequence numbers | Монотонно возрастающие номера — защита от out-of-order |
| Inbound commands | PlaceOrder (0x01), CancelOrder (0x02), AmendOrder (0x03) |
| Outbound events | Trade, OrderAccepted, OrderFilled, StopTriggered и др. |
| Error frames | Протокольные ошибки с кодом и сообщением |

## Формат фрейма

```
┌──────────┬──────────┬──────────┬──────────┬─────────────────┐
│ len: u32 │ ver: u8  │ type: u8 │ seq: u64 │ payload: [u8]   │
│ 4 bytes  │ 1 byte   │ 1 byte   │ 8 bytes  │ variable        │
└──────────┴──────────┴──────────┴──────────┴─────────────────┘
```

- `len` — размер payload в байтах (не включает заголовок)
- `ver` — версия протокола (текущая: 1)
- `type` — тип сообщения (см. `MessageType`)
- `seq` — номер последовательности, строго возрастающий

## Пример подключения клиента (Go-подобная логика)

```rust
use bytes::BytesMut;
use me_io::codec::{ConnectionCodecState, decode_frame, encode_handshake_ack, encode_events};
use me_io::protocol::ConnectionState;

// 1. Инициализация состояния соединения
let mut conn = ConnectionCodecState::new();
// conn.state == ConnectionState::HandshakePending

// 2. Клиент отправляет Handshake (type=0x05, seq=0):
//    [00 00 00 01] [01] [05] [00 00 00 00 00 00 00 00] [01]
//    len=1         ver  type  seq=0                     ver=1

// 3. Декодирование входящего фрейма
let mut buf = incoming_bytes; // BytesMut из TCP сокета
match decode_frame(&buf, &mut conn) {
    Ok((DecodedFrame::Handshake { client_version, seq }, consumed)) => {
        conn.state = ConnectionState::Active;
        let mut out = BytesMut::new();
        encode_handshake_ack(seq, &mut out);
        // отправить out клиенту
    }
    Err(DecodeError::Incomplete) => { /* ждём больше байт */ }
    Err(e) => { /* обработка ошибки */ }
}

// 4. После handshake — команды PlaceOrder и события Trade
```

## Handshake sequence

```
Client                          Server
  │──── Handshake (seq=0) ──────▶│
  │◀─── HandshakeAck (ver=1) ────│
  │──── PlaceOrder (seq=1) ──────▶│
  │◀─── OrderAccepted ────────────│
  │◀─── Trade ────────────────────│
  │──── Ping (seq=2) ────────────▶│
  │◀─── Pong ─────────────────────│
```

Команды до handshake вернут `DecodeError::HandshakeRequired`.
