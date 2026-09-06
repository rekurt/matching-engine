#!/usr/bin/env python3
"""
Matching Engine -- Python Client Example
=========================================

A zero-dependency Python 3.8+ client that speaks the matching engine's binary
wire protocol over plain TCP.

Protocol overview
-----------------
Every frame on the wire looks like this (22-byte header + variable payload):

    +----------+--------+--------+----------+-----------------+
    | len: u32 | ver:u8 | type:u8| seq: u64 | payload: [u8]   |
    | 4 bytes  | 1 byte | 1 byte | 8 bytes  | `len` bytes     |
    +----------+--------+--------+----------+-----------------+

* ``len``  -- payload size in bytes (does **not** include the 22-byte header).
* ``ver``  -- protocol version, currently ``2``.
* ``type`` -- message type tag (see MSG_* constants below).
* ``seq``  -- monotonically increasing sequence number per connection.
* All multi-byte integers are **little-endian**.

Fixed-point encoding
--------------------
Prices are signed 64-bit integers (``i64``) with an implicit scale of 10**8.
Quantities are unsigned 64-bit integers (``u64``) with the same 10**8 scale.

    50000.50 USD  ->  5_000_050_000_000  (i64, LE)
    0.5 BTC       ->  50_000_000         (u64, LE)

Connection flow
---------------
1. Open a TCP connection to the engine.
2. Send a **Handshake** frame (type 0x05, empty payload).
3. Read a **HandshakeAck** frame (type 0x1A, 1-byte server version).
4. Now the connection is active -- send PlaceOrder / CancelOrder / Ping.

Usage
-----
    python3 client.py [host] [port]

Defaults to ``127.0.0.1:9090``.
"""

from __future__ import annotations

import socket
import struct
import sys
import time
from typing import Any, Dict, Optional, Tuple

# ---------------------------------------------------------------------------
# Protocol constants
# ---------------------------------------------------------------------------

PROTOCOL_VERSION = 2
HEADER_SIZE = 22  # len(4) + ver(1) + type(1) + seq(8) + request_id(8)
PRICE_SCALE = 10**8

# -- Message types (client -> engine) --
MSG_PLACE_ORDER = 0x01
MSG_CANCEL_ORDER = 0x02
MSG_PING = 0x04
MSG_HANDSHAKE = 0x05

# -- Message types (engine -> client) --
MSG_ORDER_ACCEPTED = 0x10
MSG_ORDER_REJECTED = 0x11
MSG_TRADE = 0x12
MSG_ORDER_FILLED = 0x13
MSG_ORDER_PARTIALLY_FILLED = 0x14
MSG_ORDER_CANCELLED = 0x15
MSG_ORDER_AMENDED = 0x16
MSG_STOP_TRIGGERED = 0x17
MSG_PONG = 0x18
MSG_ERROR = 0x19
MSG_HANDSHAKE_ACK = 0x1A

# Human-readable names for pretty-printing.
MSG_TYPE_NAMES = {
    MSG_PLACE_ORDER: "PlaceOrder",
    MSG_CANCEL_ORDER: "CancelOrder",
    MSG_PING: "Ping",
    MSG_HANDSHAKE: "Handshake",
    MSG_ORDER_ACCEPTED: "OrderAccepted",
    MSG_ORDER_REJECTED: "OrderRejected",
    MSG_TRADE: "Trade",
    MSG_ORDER_FILLED: "OrderFilled",
    MSG_ORDER_PARTIALLY_FILLED: "OrderPartiallyFilled",
    MSG_ORDER_CANCELLED: "OrderCancelled",
    MSG_ORDER_AMENDED: "OrderAmended",
    MSG_STOP_TRIGGERED: "StopTriggered",
    MSG_PONG: "Pong",
    MSG_ERROR: "Error",
    MSG_HANDSHAKE_ACK: "HandshakeAck",
}

# -- Side enum --
SIDE_BUY = 0
SIDE_SELL = 1
SIDE_NAMES = {SIDE_BUY: "Buy", SIDE_SELL: "Sell"}

# -- OrderType enum --
ORDER_LIMIT = 0
ORDER_MARKET = 1
ORDER_STOP_LIMIT = 2
ORDER_STOP_MARKET = 3

# -- TimeInForce enum --
TIF_GTC = 0  # Good-Til-Cancelled
TIF_IOC = 1  # Immediate-Or-Cancel
TIF_FOK = 2  # Fill-Or-Kill

# -- Reject reasons --
REJECT_REASONS = {
    0: "NoLiquidity",
    1: "InsufficientLiquidity",
    2: "InvalidPrice",
    3: "InvalidQuantity",
    4: "InvalidLotSize",
    5: "InvalidTickSize",
    6: "OrderNotFound",
    7: "DuplicateOrderId",
    8: "SymbolNotFound",
    9: "TradingHalted",
    10: "RiskCheckFailed",
}


# ---------------------------------------------------------------------------
# Fixed-point helpers
# ---------------------------------------------------------------------------

def price_to_raw(price: float) -> int:
    """Convert a human-readable price (float) to the engine's i64 fixed-point
    representation.  Example: 50000.50 -> 5_000_050_000_000."""
    return int(round(price * PRICE_SCALE))


def raw_to_price(raw: int) -> float:
    """Convert an i64 fixed-point value back to a human-readable float."""
    return raw / PRICE_SCALE


def qty_to_raw(qty: float) -> int:
    """Convert a human-readable quantity (float) to the engine's u64
    fixed-point representation.  Example: 0.5 -> 50_000_000."""
    return int(round(qty * PRICE_SCALE))


def raw_to_qty(raw: int) -> float:
    """Convert a u64 fixed-point quantity back to a human-readable float."""
    return raw / PRICE_SCALE


def _ts_to_str(nanos: int) -> str:
    """Format a nanosecond UNIX timestamp as a human-readable string."""
    if nanos == 0:
        return "0"
    secs = nanos / 1e9
    return time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime(secs))


# ---------------------------------------------------------------------------
# MEClient
# ---------------------------------------------------------------------------

class MEClient:
    """Matching engine TCP client.

    Uses blocking sockets for simplicity.  For production use you would want
    an async implementation (``asyncio``, ``trio``, etc.) with proper
    back-pressure handling.
    """

    def __init__(self, host: str = "127.0.0.1", port: int = 9090):
        self.host = host
        self.port = port
        self._sock: Optional[socket.socket] = None
        self._seq: int = 0  # Next outbound sequence number.
        self._buf: bytearray = bytearray()  # Receive buffer for framing.

    # -- Connection lifecycle ------------------------------------------------

    def connect(self) -> None:
        """Open the TCP connection."""
        self._sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self._sock.connect((self.host, self.port))
        print(f"[*] Connected to {self.host}:{self.port}")

    def close(self) -> None:
        """Close the TCP connection."""
        if self._sock:
            self._sock.close()
            self._sock = None
            print("[*] Connection closed")

    def handshake(self) -> Dict[str, Any]:
        """Perform the protocol handshake.

        Sends a Handshake frame (type 0x05) with an empty payload and waits
        for a HandshakeAck (type 0x1A) whose single-byte payload carries the
        server's protocol version.
        """
        self._send_frame(MSG_HANDSHAKE, b"")
        event = self.read_event()
        if event["type"] != "HandshakeAck":
            raise RuntimeError(f"Expected HandshakeAck, got {event['type']}")
        print(f"[*] Handshake OK -- server version {event['server_version']}")
        return event

    # -- Commands ------------------------------------------------------------

    def place_order(
        self,
        order_id: int,
        symbol_id: int,
        side: int,
        order_type: int = ORDER_LIMIT,
        tif: int = TIF_GTC,
        price: float = 0.0,
        stop_price: float = 0.0,
        qty: float = 0.0,
    ) -> None:
        """Send a PlaceOrder command.

        PlaceOrder payload layout (37 bytes, all little-endian):
            order_id   : u64  (8 bytes)
            symbol_id  : u16  (2 bytes)
            side       : u8   (1 byte)  -- 0=Buy, 1=Sell
            order_type : u8   (1 byte)  -- 0=Limit, 1=Market, ...
            tif        : u8   (1 byte)  -- 0=GTC, 1=IOC, 2=FOK
            price      : i64  (8 bytes) -- fixed-point, scale 10^8
            stop_price : i64  (8 bytes)
            qty        : u64  (8 bytes) -- fixed-point, scale 10^8
        """
        payload = struct.pack(
            "<QHBBBqqQ",
            order_id,
            symbol_id,
            side,
            order_type,
            tif,
            price_to_raw(price),
            price_to_raw(stop_price),
            qty_to_raw(qty),
        )
        assert len(payload) == 37, f"PlaceOrder payload must be 37 bytes, got {len(payload)}"
        self._send_frame(MSG_PLACE_ORDER, payload)
        side_str = SIDE_NAMES.get(side, "?")
        print(
            f"[>] PlaceOrder  id={order_id}  symbol={symbol_id}  "
            f"{side_str}  price={price}  qty={qty}"
        )

    def cancel_order(self, order_id: int, symbol_id: int) -> None:
        """Send a CancelOrder command.

        CancelOrder payload layout (10 bytes):
            order_id  : u64  (8 bytes)
            symbol_id : u16  (2 bytes)
        """
        payload = struct.pack("<QH", order_id, symbol_id)
        assert len(payload) == 10
        self._send_frame(MSG_CANCEL_ORDER, payload)
        print(f"[>] CancelOrder  id={order_id}  symbol={symbol_id}")

    def ping(self) -> None:
        """Send a Ping and wait for the Pong response."""
        self._send_frame(MSG_PING, b"")
        print("[>] Ping")

    # -- Reading events ------------------------------------------------------

    def read_event(self, timeout: float = 5.0) -> Dict[str, Any]:
        """Block until a complete frame arrives, then decode and return it as a
        dictionary.

        Raises ``TimeoutError`` if no complete frame is received within
        *timeout* seconds.
        """
        assert self._sock is not None, "Not connected"
        self._sock.settimeout(timeout)
        while True:
            # Try to parse a frame from what we already have.
            frame = self._try_decode()
            if frame is not None:
                return frame
            # Need more data.
            try:
                chunk = self._sock.recv(4096)
            except socket.timeout:
                raise TimeoutError("Timed out waiting for server response")
            if not chunk:
                raise ConnectionError("Connection closed by server")
            self._buf.extend(chunk)

    def read_all_events(self, timeout: float = 0.5) -> list:
        """Read events until the socket has nothing left (within *timeout*).

        Useful after placing crossing orders to drain all resulting events.
        """
        events = []
        while True:
            try:
                events.append(self.read_event(timeout=timeout))
            except (TimeoutError, socket.timeout):
                break
        return events

    # -- Internal helpers ----------------------------------------------------

    def _next_seq(self) -> int:
        """Return the next sequence number (monotonically increasing)."""
        self._seq += 1
        return self._seq

    def _send_frame(self, msg_type: int, payload: bytes) -> None:
        """Build a frame (22-byte header + payload) and send it."""
        assert self._sock is not None, "Not connected"
        seq = self._next_seq()
        request_id = seq if msg_type in (0x01, 0x02, 0x03, 0x07, 0x09, 0x0B, 0x0C) else 0
        header = struct.pack("<IBBQQ", len(payload), PROTOCOL_VERSION, msg_type, seq, request_id)
        assert len(header) == HEADER_SIZE
        self._sock.sendall(header + payload)

    def _try_decode(self) -> Optional[Dict[str, Any]]:
        """Try to decode a single frame from ``self._buf``.

        Returns ``None`` if there is not enough data yet.
        """
        if len(self._buf) < HEADER_SIZE:
            return None

        # Parse header.
        payload_len, ver, msg_type, seq, request_id = struct.unpack_from("<IBBQQ", self._buf, 0)
        if payload_len > 1024 * 1024:
            raise RuntimeError("Response payload exceeds 1 MiB")
        total = HEADER_SIZE + payload_len
        if len(self._buf) < total:
            return None  # Incomplete frame.

        payload = bytes(self._buf[HEADER_SIZE:total])
        # Consume the frame from the buffer.
        self._buf = self._buf[total:]

        if ver != PROTOCOL_VERSION:
            raise RuntimeError(f"Unsupported protocol version: {ver}")

        event = self._decode_payload(msg_type, seq, payload)
        event["request_id"] = request_id
        return event

    def _decode_payload(self, msg_type: int, seq: int, payload: bytes) -> Dict[str, Any]:
        """Decode a frame payload into a human-friendly dictionary."""
        base: Dict[str, Any] = {
            "type": MSG_TYPE_NAMES.get(msg_type, f"Unknown(0x{msg_type:02x})"),
            "seq": seq,
        }

        if msg_type == MSG_HANDSHAKE_ACK:
            # Payload: server_version (1 byte)
            base["server_version"] = payload[0] if payload else 0

        elif msg_type == MSG_ORDER_ACCEPTED:
            # Payload: order_id(8) + timestamp(8)  = 16 bytes
            oid, ts = struct.unpack_from("<QQ", payload)
            base.update(order_id=oid, timestamp=ts, timestamp_str=_ts_to_str(ts))

        elif msg_type == MSG_ORDER_REJECTED:
            # Payload: order_id(8) + reason(1) + timestamp(8)  = 17 bytes
            oid, reason, ts = struct.unpack_from("<QBQ", payload)
            base.update(
                order_id=oid,
                reason=REJECT_REASONS.get(reason, f"Unknown({reason})"),
                reason_code=reason,
                timestamp=ts,
                timestamp_str=_ts_to_str(ts),
            )

        elif msg_type == MSG_TRADE:
            # Payload: trade_id(8) + symbol_id(2) + price(i64,8) + qty(u64,8)
            #        + maker_order_id(8) + taker_order_id(8) + maker_side(1)
            #        + timestamp(8)  = 51 bytes
            (
                trade_id, symbol_id, price_raw, qty_raw,
                maker_oid, taker_oid, maker_side, ts,
            ) = struct.unpack_from("<QHqQQQBQ", payload)
            base.update(
                trade_id=trade_id,
                symbol_id=symbol_id,
                price=raw_to_price(price_raw),
                price_raw=price_raw,
                qty=raw_to_qty(qty_raw),
                qty_raw=qty_raw,
                maker_order_id=maker_oid,
                taker_order_id=taker_oid,
                maker_side=SIDE_NAMES.get(maker_side, f"Unknown({maker_side})"),
                timestamp=ts,
                timestamp_str=_ts_to_str(ts),
            )

        elif msg_type == MSG_ORDER_FILLED:
            # Payload: order_id(8) + filled_qty(u64,8) + price(i64,8)
            #        + timestamp(8)  = 32 bytes
            oid, filled_raw, price_raw, ts = struct.unpack_from("<QQqQ", payload)
            base.update(
                order_id=oid,
                filled_qty=raw_to_qty(filled_raw),
                filled_qty_raw=filled_raw,
                price=raw_to_price(price_raw),
                price_raw=price_raw,
                timestamp=ts,
                timestamp_str=_ts_to_str(ts),
            )

        elif msg_type == MSG_ORDER_PARTIALLY_FILLED:
            # Payload: order_id(8) + filled_qty(u64,8) + remaining_qty(u64,8)
            #        + price(i64,8) + timestamp(8)  = 40 bytes
            oid, filled_raw, remaining_raw, price_raw, ts = struct.unpack_from(
                "<QQQqQ", payload
            )
            base.update(
                order_id=oid,
                filled_qty=raw_to_qty(filled_raw),
                remaining_qty=raw_to_qty(remaining_raw),
                price=raw_to_price(price_raw),
                timestamp=ts,
                timestamp_str=_ts_to_str(ts),
            )

        elif msg_type == MSG_ORDER_CANCELLED:
            # Payload: order_id(8) + remaining_qty(u64,8) + timestamp(8) = 24
            oid, remaining_raw, ts = struct.unpack_from("<QQQ", payload)
            base.update(
                order_id=oid,
                remaining_qty=raw_to_qty(remaining_raw),
                timestamp=ts,
                timestamp_str=_ts_to_str(ts),
            )

        elif msg_type == MSG_ORDER_AMENDED:
            # Payload: order_id(8) + new_price(i64,8) + new_qty(u64,8)
            #        + timestamp(8) = 32
            oid, price_raw, qty_raw, ts = struct.unpack_from("<QqQQ", payload)
            base.update(
                order_id=oid,
                new_price=raw_to_price(price_raw),
                new_qty=raw_to_qty(qty_raw),
                timestamp=ts,
                timestamp_str=_ts_to_str(ts),
            )

        elif msg_type == MSG_STOP_TRIGGERED:
            # Payload: order_id(8) + trigger_price(i64,8) + timestamp(8) = 24
            oid, trigger_raw, ts = struct.unpack_from("<QqQ", payload)
            base.update(
                order_id=oid,
                trigger_price=raw_to_price(trigger_raw),
                timestamp=ts,
                timestamp_str=_ts_to_str(ts),
            )

        elif msg_type == MSG_PONG:
            # Empty payload.
            pass

        elif msg_type == MSG_ERROR:
            # Payload: error_code(u16,2) + msg_len(u16,2) + msg(variable)
            error_code, msg_len = struct.unpack_from("<HH", payload)
            msg_text = payload[4 : 4 + msg_len].decode("utf-8", errors="replace")
            base.update(error_code=error_code, message=msg_text)

        else:
            base["raw_payload"] = payload.hex()

        return base


# ---------------------------------------------------------------------------
# Pretty-printer
# ---------------------------------------------------------------------------

def print_event(event: Dict[str, Any]) -> None:
    """Pretty-print a decoded engine event to stdout."""
    etype = event["type"]
    seq = event.get("seq", "?")

    if etype == "HandshakeAck":
        print(f"  [<] HandshakeAck  server_version={event['server_version']}  seq={seq}")

    elif etype == "OrderAccepted":
        print(
            f"  [<] OrderAccepted  order_id={event['order_id']}  "
            f"ts={event['timestamp_str']}  seq={seq}"
        )

    elif etype == "OrderRejected":
        print(
            f"  [<] OrderRejected  order_id={event['order_id']}  "
            f"reason={event['reason']}  ts={event['timestamp_str']}  seq={seq}"
        )

    elif etype == "Trade":
        print(
            f"  [<] Trade  id={event['trade_id']}  symbol={event['symbol_id']}  "
            f"price={event['price']}  qty={event['qty']}  "
            f"maker={event['maker_order_id']}  taker={event['taker_order_id']}  "
            f"maker_side={event['maker_side']}  ts={event['timestamp_str']}  seq={seq}"
        )

    elif etype == "OrderFilled":
        print(
            f"  [<] OrderFilled  order_id={event['order_id']}  "
            f"filled_qty={event['filled_qty']}  price={event['price']}  "
            f"ts={event['timestamp_str']}  seq={seq}"
        )

    elif etype == "OrderPartiallyFilled":
        print(
            f"  [<] OrderPartiallyFilled  order_id={event['order_id']}  "
            f"filled={event['filled_qty']}  remaining={event['remaining_qty']}  "
            f"price={event['price']}  ts={event['timestamp_str']}  seq={seq}"
        )

    elif etype == "OrderCancelled":
        print(
            f"  [<] OrderCancelled  order_id={event['order_id']}  "
            f"remaining={event['remaining_qty']}  "
            f"ts={event['timestamp_str']}  seq={seq}"
        )

    elif etype == "OrderAmended":
        print(
            f"  [<] OrderAmended  order_id={event['order_id']}  "
            f"new_price={event['new_price']}  new_qty={event['new_qty']}  "
            f"ts={event['timestamp_str']}  seq={seq}"
        )

    elif etype == "StopTriggered":
        print(
            f"  [<] StopTriggered  order_id={event['order_id']}  "
            f"trigger_price={event['trigger_price']}  "
            f"ts={event['timestamp_str']}  seq={seq}"
        )

    elif etype == "Pong":
        print(f"  [<] Pong  seq={seq}")

    elif etype == "Error":
        print(
            f"  [<] Error  code={event['error_code']}  "
            f"message={event['message']}  seq={seq}"
        )

    else:
        print(f"  [<] {etype}  {event}")


# ---------------------------------------------------------------------------
# Main demo
# ---------------------------------------------------------------------------

def main() -> None:
    host = sys.argv[1] if len(sys.argv) > 1 else "127.0.0.1"
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 9090

    print("=" * 60)
    print("Matching Engine -- Python Client Example")
    print("=" * 60)

    client = MEClient(host, port)

    try:
        # Step 1: Connect and handshake.
        client.connect()
        client.handshake()

        # Step 2: Place a BUY limit order on symbol 1.
        #   Buy 1.0 BTC at 50,000.00 USD
        client.place_order(
            order_id=1,
            symbol_id=1,
            side=SIDE_BUY,
            order_type=ORDER_LIMIT,
            tif=TIF_GTC,
            price=50_000.0,
            qty=1.0,
        )

        # Read OrderAccepted.
        events = client.read_all_events(timeout=1.0)
        for e in events:
            print_event(e)

        # Step 3: Place a SELL limit order that crosses the buy.
        #   Sell 0.5 BTC at 49,999.00 USD  (crosses the resting buy at 50k)
        #   This should trigger a trade.
        client.place_order(
            order_id=2,
            symbol_id=1,
            side=SIDE_SELL,
            order_type=ORDER_LIMIT,
            tif=TIF_GTC,
            price=49_999.0,
            qty=0.5,
        )

        # Read Trade + fill events.
        events = client.read_all_events(timeout=1.0)
        for e in events:
            print_event(e)

        # Step 4: Ping / Pong.
        client.ping()
        events = client.read_all_events(timeout=1.0)
        for e in events:
            print_event(e)

        # Step 5: Cancel the remaining portion of order 1.
        client.cancel_order(order_id=1, symbol_id=1)
        events = client.read_all_events(timeout=1.0)
        for e in events:
            print_event(e)

        print()
        print("[*] Demo complete.")

    except ConnectionRefusedError:
        print(
            f"[!] Could not connect to {host}:{port}. "
            "Make sure the matching engine is running."
        )
    except Exception as exc:
        print(f"[!] Error: {exc}")
    finally:
        client.close()


if __name__ == "__main__":
    main()
