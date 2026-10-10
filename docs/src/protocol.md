# Wire protocol v2

TCP and Unix domain sockets carry the same frames. TLS changes the transport, not the frame layout. All integers are little-endian. Prices use signed `i64` and quantities use unsigned `u64`, both scaled by `10^8`. Timestamps and expiry times use Unix nanoseconds.

## Frame header

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 4 | Payload length, excluding the header (`u32`) |
| 4 | 1 | Version: `2` |
| 5 | 1 | Message type |
| 6 | 8 | Sequence (`u64`) |
| 14 | 8 | Request ID (`u64`) |
| 22 | variable | Payload |

Read exactly the 22-byte header, then the declared payload. One socket read may contain part of a frame or several frames. Regular inbound payloads are limited to 1024 bytes; batch payloads to 4096 bytes.

Client sequences increase strictly within a connection. Sequence zero is accepted only for its first Handshake. Commands and queries require a nonzero request ID; Ping, Handshake, ResumeSession and Acknowledge do not. Reuse the original request ID and exact payload when retrying a mutation.

Positive server sequences identify the per-session replay stream. Replies passed through the active-session response path, including HandshakeAck, Pong, ReplayGap from ResumeSession and some Error frames, are assigned server sequences and journaled when WAL is enabled. A gap detected during pending delivery instead emits ReplayGap directly with sequence zero, outside the replay journal. Replayed frames retain their original sequence. The server rewrites these headers; a Pong header is not the echoed client sequence (its payload reports the last processed client sequence).

Sequence zero is out-of-band and never advances the receive cursor. SessionResumed is sent with zero before the replay frames; errors sent outside the active-session replay path also use zero. Process every positive-sequence frame in order, including control frames, and advance the cursor only after applying it. Solicited order events mirror the request ID; passive owner events use zero.

## Connection and recovery

Start with Handshake to create a session, or ResumeSession to recover one. Do not send order commands before acceptance. Persist the session ID and last applied positive server sequence outside the connection. A session can have only one active connection.

| Type | Name | Payload, in field order |
| --- | --- | --- |
| `0x04` | Ping | Empty, or last applied server sequence (`u64`) as cumulative ACK |
| `0x05` | Handshake | Empty, or client version (`u8`, use 2) |
| `0x06` | ResumeSession | Session ID (16 bytes), last received/applied server sequence (`u64`) |
| `0x08` | Acknowledge | Last applied server sequence (`u64`) |
| `0x18` | Pong | Last processed client sequence (`u64`): 8 bytes |
| `0x19` | Error | Code (`u16`), message length (`u16`), UTF-8 message |
| `0x1A` | HandshakeAck | Server version (`u8`), TLV extensions |
| `0x1B` | SessionResumed | Replayed frame count (`u32`) |
| `0x20` | ReplayGap | Missing sequence range: from (`u64`), to (`u64`) |
| `0x23` | FlowControlWarning | Buffer usage percent (`u8`), hard-limit flag (`u8`) |

HandshakeAck currently has 41 payload bytes. Each TLV is tag (`u16`), length (`u16`), value. Skip unknown tags:

| Tag | Value |
| --- | --- |
| `1` | Capabilities (`u64`) |
| `2` | Assigned session ID (16 bytes) |
| `3` | Heartbeat interval in milliseconds (`u32`) |

Capability bits 0–7 indicate session recovery, heartbeat, batch orders, order flags, order query, TLS, mutation deduplication and open-order snapshots respectively. Send heartbeats within the configured timeout.

SessionResumed precedes replay. On ReplayGap, obtain a complete GetOpenOrders snapshot; it restores open order state, not missed settlement events. Reconcile missing executions separately. Do not silently treat a fresh session as recovery of an old one. See [client integration](integration.md) and [deployment](deployment.md).

## Order commands

### PlaceOrder (`0x01`)

| Offset | Type | Field |
| --- | --- | --- |
| 0 | `u64` | Nonzero order ID |
| 8 | `u16` | Symbol ID |
| 10 | `u8` | Side: buy 0, sell 1 |
| 11 | `u8` | Order type |
| 12 | `u8` | Time in force |
| 13 | `i64` | Limit price; zero for market |
| 21 | `i64` | Stop price; zero for non-stop |
| 29 | `u64` | Positive total quantity |
| 37 | `u8` | Optional flags; absent means zero |
| 38 | `u64` | Optional GTD expiry; absent means zero |
| 46 | `i64` | Optional trailing distance; absent means zero |
| 54 | `u64` | Optional iceberg display quantity; absent means zero |

Base payload is 37 bytes; optional fields extend it to 38, 46, 54 or 62 bytes. Include all preceding optional fields when using a later one. GTD requires a nonzero future expiry. Trailing distance must be nonnegative and a positive distance is valid only on stop orders. Positive display quantity is valid only on limit orders. Stop orders require a positive stop price; limit/stop-limit require a positive limit price.

Order types: Limit 0, Market 1, StopLimit 2, StopMarket 3. Time in force: GTC 0, IOC 1, FOK 2, GTD 3. Flags: POST_ONLY `0x01`, REDUCE_ONLY `0x02`, MAKER_OR_CANCEL `0x04`. REDUCE_ONLY is a reserved futures flag; this spot engine does not enforce position reduction. Do not rely on it as an account risk control.

### Other commands

| Type | Name | Payload |
| --- | --- | --- |
| `0x02` | CancelOrder | Order ID (`u64`), symbol ID (`u16`): 10 bytes |
| `0x03` | AmendOrder | Order ID (`u64`), symbol ID (`u16`), has-price (`u8`), optional price (`i64`), has-qty (`u8`), optional qty (`u64`): 12–28 bytes |
| `0x07` | GetOrderStatus | Order ID (`u64`), symbol ID (`u16`): 10 bytes |
| `0x09` | GetOpenOrders | Empty |
| `0x0B` | BatchPlaceOrder | Count (`u8`, 1–20), then fixed 38-byte PlaceOrder entries |
| `0x0C` | BatchCancelOrder | Count (`u8`, 1–50), then 10-byte CancelOrder entries |

Amend presence flags are 0 or 1; include at least one changed field. Batch placement requires the flags byte on every entry and cannot carry GTD expiry, trailing distance or iceberg extensions. Batches execute individual items and are not atomic across symbols.

Cancel, amend and status requests are scoped to the requesting session. An order belonging to another session is reported as not found. Durable mutation retries return the original outcome; a retained request ID with a different payload is rejected. Query responses may be regenerated after bounded-cache eviction.

## Order events

Fields below are serialized in the listed order, without alignment padding. `timestamp` is `u64`, price is `i64`, quantity is `u64`, and order/trade IDs are `u64`.

| Type | Name | Bytes | Payload |
| --- | --- | --- | --- |
| `0x10` | OrderAccepted | 16 | Order ID, timestamp |
| `0x11` | OrderRejected | 17 | Order ID, reason (`u8`), timestamp |
| `0x12` | Trade | 59 | Trade ID, symbol (`u16`), price, qty, maker ID, taker ID, maker side (`u8`), timestamp, trade sequence (`u64`) |
| `0x13` | OrderFilled | 32 | Order ID, filled qty, price, timestamp |
| `0x14` | OrderPartiallyFilled | 40 | Order ID, filled qty, remaining qty, price, timestamp |
| `0x15` | OrderCancelled | 24 | Order ID, remaining qty, timestamp |
| `0x16` | OrderAmended | 32 | Order ID, new price, new qty, timestamp |
| `0x17` | StopTriggered | 24 | Order ID, trigger price, timestamp |
| `0x1D` | OrderStatus | 41 | Order ID, status (`u8`), filled qty, remaining qty, price, timestamp |
| `0x1E` | OrderNotFound | 8 | Order ID |

One command can emit several events. Continue consuming unsolicited owner events even when no local command is pending. The trade sequence inside Trade is distinct from the frame's server sequence.

## Open-order snapshots

| Type | Name | Bytes | Payload |
| --- | --- | --- | --- |
| `0x1F` | OpenOrdersBegin | 12 | Count (`u32`), snapshot sequence (`u64`) |
| `0x21` | OpenOrder | 60 | Layout below |
| `0x22` | OpenOrdersEnd | 8 | Same snapshot sequence (`u64`) |

Wait for End with the matching request ID. Validate Begin/End sequence equality and the declared order count before replacing local state. A read timeout is not a valid empty snapshot.

OpenOrder offsets: ID 0 (`u64`), symbol 8 (`u16`), side 10, type 11, TIF 12, status 13 (`u8` each), price 14 (`i64`), stop price 22 (`i64`), total qty 30 (`u64`), open qty 38 (`u64`), timestamp 46 (`u64`), flags 54 (`u8`), reserved zero bytes 55–59. For an iceberg, open qty includes the hidden remainder. This fixed layout does not include expiry, trailing distance or display size.

Order status values: New 0, PartiallyFilled 1, Filled 2, Cancelled 3, Rejected 4, Triggered 5.

## Rejection and error codes

OrderRejected reasons (`u8`):

| Code | Reason | Code | Reason |
| --- | --- | --- | --- |
| 0 | NoLiquidity | 8 | SymbolNotFound |
| 1 | InsufficientLiquidity | 9 | TradingHalted |
| 2 | InvalidPrice | 10 | RiskCheckFailed |
| 3 | InvalidQuantity | 11 | WouldTakeMarket |
| 4 | InvalidLotSize | 12 | InternalError |
| 5 | InvalidTickSize | 13 | SelfTradePrevented |
| 6 | OrderNotFound | 14 | InvalidExpiry |
| 7 | DuplicateOrderId | 15 | AmendNotSupported |

Protocol Error codes (`u16`): 1 UnsupportedVersion, 2 SequenceOutOfOrder, 3 HandshakeRequired, 4 InvalidPayload, 5 ServerShuttingDown, 6 Backpressure, 7 HeartbeatTimeout, 8 TlsError, 9 InvalidRequestId, 10 SessionExpired, 11 BufferExhausted, 12 InternalError. Some codes are reserved; their assignment remains part of the wire contract. Handle unknown codes without corrupting framing.

An InternalError is not a successful execution acknowledgment. Reconnect/retry according to the retained mutation identity and recovery contract; do not generate a replacement order merely because delivery failed.
