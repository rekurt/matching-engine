/**
 * Matching Engine -- TypeScript/Node.js Client Example
 * =====================================================
 *
 * A zero-dependency Node.js 18+ client that speaks the matching engine's
 * binary wire protocol over plain TCP.
 *
 * Protocol overview
 * -----------------
 * Every frame on the wire has a 22-byte header followed by a variable-length
 * payload:
 *
 *     +----------+--------+--------+----------+-----------------+
 *     | len: u32 | ver:u8 | type:u8| seq: u64 | payload: [u8]   |
 *     | 4 bytes  | 1 byte | 1 byte | 8 bytes  | `len` bytes     |
 *     +----------+--------+--------+----------+-----------------+
 *
 * - `len`  -- payload size only (does NOT include the 22-byte header).
 * - `ver`  -- protocol version, currently 2.
 * - `type` -- message type tag (see constants below).
 * - `seq`  -- monotonically increasing sequence number per connection.
 * - All multi-byte integers are little-endian.
 *
 * Fixed-point encoding
 * --------------------
 * Prices are signed 64-bit integers (i64) with scale 10^8.
 * Quantities are unsigned 64-bit integers (u64) with scale 10^8.
 * We use BigInt throughout to avoid floating-point loss on 64-bit values.
 *
 *     50000.50 USD  ->  5_000_050_000_000n  (i64 LE)
 *     0.5 BTC       ->  50_000_000n         (u64 LE)
 *
 * Usage
 * -----
 *     npx ts-node client.ts [host] [port]
 *
 * Defaults to 127.0.0.1:9090.
 */

import * as net from "net";

// ---------------------------------------------------------------------------
// Protocol constants
// ---------------------------------------------------------------------------

const PROTOCOL_VERSION = 2;
const HEADER_SIZE = 22; // len(4) + ver(1) + type(1) + seq(8)
const PRICE_SCALE = 100_000_000n; // 10^8

// -- Message types (client -> engine) --
const MSG_PLACE_ORDER = 0x01;
const MSG_CANCEL_ORDER = 0x02;
const MSG_PING = 0x04;
const MSG_HANDSHAKE = 0x05;

// -- Message types (engine -> client) --
const MSG_ORDER_ACCEPTED = 0x10;
const MSG_ORDER_REJECTED = 0x11;
const MSG_TRADE = 0x12;
const MSG_ORDER_FILLED = 0x13;
const MSG_ORDER_PARTIALLY_FILLED = 0x14;
const MSG_ORDER_CANCELLED = 0x15;
const MSG_ORDER_AMENDED = 0x16;
const MSG_STOP_TRIGGERED = 0x17;
const MSG_PONG = 0x18;
const MSG_ERROR = 0x19;
const MSG_HANDSHAKE_ACK = 0x1a;

const MSG_TYPE_NAMES: Record<number, string> = {
  [MSG_PLACE_ORDER]: "PlaceOrder",
  [MSG_CANCEL_ORDER]: "CancelOrder",
  [MSG_PING]: "Ping",
  [MSG_HANDSHAKE]: "Handshake",
  [MSG_ORDER_ACCEPTED]: "OrderAccepted",
  [MSG_ORDER_REJECTED]: "OrderRejected",
  [MSG_TRADE]: "Trade",
  [MSG_ORDER_FILLED]: "OrderFilled",
  [MSG_ORDER_PARTIALLY_FILLED]: "OrderPartiallyFilled",
  [MSG_ORDER_CANCELLED]: "OrderCancelled",
  [MSG_ORDER_AMENDED]: "OrderAmended",
  [MSG_STOP_TRIGGERED]: "StopTriggered",
  [MSG_PONG]: "Pong",
  [MSG_ERROR]: "Error",
  [MSG_HANDSHAKE_ACK]: "HandshakeAck",
};

// -- Side enum --
const SIDE_BUY = 0;
const SIDE_SELL = 1;
const SIDE_NAMES: Record<number, string> = { [SIDE_BUY]: "Buy", [SIDE_SELL]: "Sell" };

// -- OrderType enum --
const ORDER_LIMIT = 0;
const ORDER_MARKET = 1;
const ORDER_STOP_LIMIT = 2;
const ORDER_STOP_MARKET = 3;

// -- TimeInForce enum --
const TIF_GTC = 0;
const TIF_IOC = 1;
const TIF_FOK = 2;

// -- Reject reasons --
const REJECT_REASONS: Record<number, string> = {
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
};

// ---------------------------------------------------------------------------
// Fixed-point helpers
// ---------------------------------------------------------------------------

/** Convert a human-readable price (number) to the engine's i64 fixed-point. */
function priceToRaw(price: number): bigint {
  // Multiply in floating point, then round to nearest integer.
  return BigInt(Math.round(price * Number(PRICE_SCALE)));
}

/** Convert an i64 fixed-point price back to a human-readable number. */
function rawToPrice(raw: bigint): number {
  return Number(raw) / Number(PRICE_SCALE);
}

/** Convert a human-readable quantity to the engine's u64 fixed-point. */
function qtyToRaw(qty: number): bigint {
  return BigInt(Math.round(qty * Number(PRICE_SCALE)));
}

/** Convert a u64 fixed-point quantity back to a human-readable number. */
function rawToQty(raw: bigint): number {
  return Number(raw) / Number(PRICE_SCALE);
}

/** Format a nanosecond UNIX timestamp as a human-readable string. */
function tsToStr(nanos: bigint): string {
  if (nanos === 0n) return "0";
  const ms = Number(nanos / 1_000_000n);
  return new Date(ms).toISOString();
}

// ---------------------------------------------------------------------------
// Event type
// ---------------------------------------------------------------------------

interface MEEvent {
  type: string;
  seq: bigint;
  [key: string]: unknown;
}

// ---------------------------------------------------------------------------
// MEClient
// ---------------------------------------------------------------------------

class MEClient {
  private host: string;
  private port: number;
  private socket: net.Socket | null = null;
  private seq = 0n;
  private buf = Buffer.alloc(0);

  // Pending data listeners -- read_event resolves the oldest one.
  private pendingResolvers: Array<(evt: MEEvent) => void> = [];
  private eventQueue: MEEvent[] = [];

  constructor(host = "127.0.0.1", port = 9090) {
    this.host = host;
    this.port = port;
  }

  // -- Connection lifecycle ------------------------------------------------

  /** Open the TCP connection. */
  async connect(): Promise<void> {
    return new Promise((resolve, reject) => {
      this.socket = new net.Socket();
      this.socket.setNoDelay(true);

      this.socket.on("data", (chunk: Buffer) => {
        // Append new data to internal buffer.
        this.buf = Buffer.concat([this.buf, chunk]);
        // Try to decode as many frames as possible.
        this.drainFrames();
      });

      this.socket.on("error", (err) => {
        reject(err);
      });

      this.socket.on("close", () => {
        this.socket = null;
      });

      this.socket.connect(this.port, this.host, () => {
        console.log(`[*] Connected to ${this.host}:${this.port}`);
        resolve();
      });
    });
  }

  /** Close the TCP connection. */
  close(): void {
    if (this.socket) {
      this.socket.destroy();
      this.socket = null;
      console.log("[*] Connection closed");
    }
  }

  /** Perform the protocol handshake. */
  async handshake(): Promise<MEEvent> {
    this.sendFrame(MSG_HANDSHAKE, Buffer.alloc(0));
    const event = await this.readEvent();
    if (event.type !== "HandshakeAck") {
      throw new Error(`Expected HandshakeAck, got ${event.type}`);
    }
    console.log(`[*] Handshake OK -- server version ${event.serverVersion}`);
    return event;
  }

  // -- Commands ------------------------------------------------------------

  /**
   * Send a PlaceOrder command.
   *
   * PlaceOrder payload (37 bytes, all LE):
   *   order_id(u64) + symbol_id(u16) + side(u8) + order_type(u8) + tif(u8)
   *   + price(i64) + stop_price(i64) + qty(u64)
   */
  placeOrder(opts: {
    orderId: bigint;
    symbolId: number;
    side: number;
    orderType?: number;
    tif?: number;
    price?: number;
    stopPrice?: number;
    qty: number;
  }): void {
    const {
      orderId,
      symbolId,
      side,
      orderType = ORDER_LIMIT,
      tif = TIF_GTC,
      price = 0,
      stopPrice = 0,
      qty,
    } = opts;

    const payload = Buffer.alloc(37);
    payload.writeBigUInt64LE(orderId, 0);         // order_id: u64
    payload.writeUInt16LE(symbolId, 8);            // symbol_id: u16
    payload.writeUInt8(side, 10);                  // side: u8
    payload.writeUInt8(orderType, 11);             // order_type: u8
    payload.writeUInt8(tif, 12);                   // tif: u8
    payload.writeBigInt64LE(priceToRaw(price), 13);     // price: i64
    payload.writeBigInt64LE(priceToRaw(stopPrice), 21); // stop_price: i64
    payload.writeBigUInt64LE(qtyToRaw(qty), 29);        // qty: u64

    this.sendFrame(MSG_PLACE_ORDER, payload);
    const sideStr = SIDE_NAMES[side] ?? "?";
    console.log(
      `[>] PlaceOrder  id=${orderId}  symbol=${symbolId}  ${sideStr}  price=${price}  qty=${qty}`
    );
  }

  /**
   * Send a CancelOrder command.
   *
   * CancelOrder payload (10 bytes): order_id(u64) + symbol_id(u16)
   */
  cancelOrder(orderId: bigint, symbolId: number): void {
    const payload = Buffer.alloc(10);
    payload.writeBigUInt64LE(orderId, 0);
    payload.writeUInt16LE(symbolId, 8);
    this.sendFrame(MSG_CANCEL_ORDER, payload);
    console.log(`[>] CancelOrder  id=${orderId}  symbol=${symbolId}`);
  }

  /** Send a Ping (empty payload). */
  ping(): void {
    this.sendFrame(MSG_PING, Buffer.alloc(0));
    console.log("[>] Ping");
  }

  // -- Reading events ------------------------------------------------------

  /** Wait for the next event from the engine. */
  readEvent(timeoutMs = 5000): Promise<MEEvent> {
    // If we already have a queued event, resolve immediately.
    if (this.eventQueue.length > 0) {
      return Promise.resolve(this.eventQueue.shift()!);
    }

    return new Promise<MEEvent>((resolve, reject) => {
      const timer = setTimeout(() => {
        // Remove this resolver from the queue.
        const idx = this.pendingResolvers.indexOf(resolve);
        if (idx !== -1) this.pendingResolvers.splice(idx, 1);
        reject(new Error("Timed out waiting for server response"));
      }, timeoutMs);

      // Wrap resolver to also clear the timer.
      const wrappedResolve = (evt: MEEvent) => {
        clearTimeout(timer);
        resolve(evt);
      };

      this.pendingResolvers.push(wrappedResolve);
    });
  }

  /** Read events until the socket has nothing left (within timeout). */
  async readAllEvents(timeoutMs = 500): Promise<MEEvent[]> {
    const events: MEEvent[] = [];
    while (true) {
      try {
        events.push(await this.readEvent(timeoutMs));
      } catch {
        break; // Timeout -- no more events pending.
      }
    }
    return events;
  }

  // -- Internal helpers ----------------------------------------------------

  private nextSeq(): bigint {
    this.seq += 1n;
    return this.seq;
  }

  /** Build a frame (22-byte header + payload) and send it. */
  private sendFrame(msgType: number, payload: Buffer): void {
    if (!this.socket) throw new Error("Not connected");
    const header = Buffer.alloc(HEADER_SIZE);
    header.writeUInt32LE(payload.length, 0);     // len: u32
    header.writeUInt8(PROTOCOL_VERSION, 4);      // ver: u8
    header.writeUInt8(msgType, 5);               // type: u8
    const seq = this.nextSeq();
    header.writeBigUInt64LE(seq, 6);
    header.writeBigUInt64LE([1, 2, 3, 7, 9, 11, 12].includes(msgType) ? seq : 0n, 14);  // seq: u64
    this.socket.write(Buffer.concat([header, payload]));
  }

  /** Try to decode complete frames from the internal buffer and dispatch. */
  private drainFrames(): void {
    while (true) {
      if (this.buf.length < HEADER_SIZE) return;

      const payloadLen = this.buf.readUInt32LE(0);
      const totalLen = HEADER_SIZE + payloadLen;
      if (this.buf.length < totalLen) return; // Incomplete frame.

      const ver = this.buf.readUInt8(4);
      const msgType = this.buf.readUInt8(5);
      const seq = this.buf.readBigUInt64LE(6);
      const payload = this.buf.subarray(HEADER_SIZE, totalLen);

      // Consume from buffer.
      this.buf = this.buf.subarray(totalLen);

      if (ver !== PROTOCOL_VERSION) {
        console.error(`[!] Unsupported protocol version: ${ver}`);
        continue;
      }

      const event = this.decodePayload(msgType, seq, payload);
      this.dispatchEvent(event);
    }
  }

  /** Push event to either a waiting resolver or the queue. */
  private dispatchEvent(event: MEEvent): void {
    if (this.pendingResolvers.length > 0) {
      const resolve = this.pendingResolvers.shift()!;
      resolve(event);
    } else {
      this.eventQueue.push(event);
    }
  }

  /** Decode a frame's payload into a typed event object. */
  private decodePayload(msgType: number, seq: bigint, payload: Buffer): MEEvent {
    const base: MEEvent = {
      type: MSG_TYPE_NAMES[msgType] ?? `Unknown(0x${msgType.toString(16)})`,
      seq,
    };

    switch (msgType) {
      case MSG_HANDSHAKE_ACK: {
        // Payload: server_version (1 byte)
        base.serverVersion = payload.length > 0 ? payload.readUInt8(0) : 0;
        break;
      }

      case MSG_ORDER_ACCEPTED: {
        // Payload: order_id(8) + timestamp(8) = 16 bytes
        base.orderId = payload.readBigUInt64LE(0);
        base.timestamp = payload.readBigUInt64LE(8);
        base.timestampStr = tsToStr(base.timestamp as bigint);
        break;
      }

      case MSG_ORDER_REJECTED: {
        // Payload: order_id(8) + reason(1) + timestamp(8) = 17 bytes
        base.orderId = payload.readBigUInt64LE(0);
        const reasonCode = payload.readUInt8(8);
        base.reason = REJECT_REASONS[reasonCode] ?? `Unknown(${reasonCode})`;
        base.reasonCode = reasonCode;
        base.timestamp = payload.readBigUInt64LE(9);
        base.timestampStr = tsToStr(base.timestamp as bigint);
        break;
      }

      case MSG_TRADE: {
        // Payload: trade_id(8) + symbol_id(2) + price(i64,8) + qty(u64,8)
        //        + maker_order_id(8) + taker_order_id(8) + maker_side(1)
        //        + timestamp(8) = 51 bytes
        let off = 0;
        base.tradeId = payload.readBigUInt64LE(off); off += 8;
        base.symbolId = payload.readUInt16LE(off); off += 2;
        const priceRaw = payload.readBigInt64LE(off); off += 8;
        base.price = rawToPrice(priceRaw);
        base.priceRaw = priceRaw;
        const qtyRaw = payload.readBigUInt64LE(off); off += 8;
        base.qty = rawToQty(qtyRaw);
        base.qtyRaw = qtyRaw;
        base.makerOrderId = payload.readBigUInt64LE(off); off += 8;
        base.takerOrderId = payload.readBigUInt64LE(off); off += 8;
        const makerSide = payload.readUInt8(off); off += 1;
        base.makerSide = SIDE_NAMES[makerSide] ?? `Unknown(${makerSide})`;
        base.timestamp = payload.readBigUInt64LE(off);
        base.timestampStr = tsToStr(base.timestamp as bigint);
        break;
      }

      case MSG_ORDER_FILLED: {
        // Payload: order_id(8) + filled_qty(u64,8) + price(i64,8)
        //        + timestamp(8) = 32 bytes
        base.orderId = payload.readBigUInt64LE(0);
        base.filledQty = rawToQty(payload.readBigUInt64LE(8));
        base.price = rawToPrice(payload.readBigInt64LE(16));
        base.timestamp = payload.readBigUInt64LE(24);
        base.timestampStr = tsToStr(base.timestamp as bigint);
        break;
      }

      case MSG_ORDER_PARTIALLY_FILLED: {
        // Payload: order_id(8) + filled_qty(u64,8) + remaining_qty(u64,8)
        //        + price(i64,8) + timestamp(8) = 40 bytes
        base.orderId = payload.readBigUInt64LE(0);
        base.filledQty = rawToQty(payload.readBigUInt64LE(8));
        base.remainingQty = rawToQty(payload.readBigUInt64LE(16));
        base.price = rawToPrice(payload.readBigInt64LE(24));
        base.timestamp = payload.readBigUInt64LE(32);
        base.timestampStr = tsToStr(base.timestamp as bigint);
        break;
      }

      case MSG_ORDER_CANCELLED: {
        // Payload: order_id(8) + remaining_qty(u64,8) + timestamp(8) = 24
        base.orderId = payload.readBigUInt64LE(0);
        base.remainingQty = rawToQty(payload.readBigUInt64LE(8));
        base.timestamp = payload.readBigUInt64LE(16);
        base.timestampStr = tsToStr(base.timestamp as bigint);
        break;
      }

      case MSG_ORDER_AMENDED: {
        // Payload: order_id(8) + new_price(i64,8) + new_qty(u64,8)
        //        + timestamp(8) = 32
        base.orderId = payload.readBigUInt64LE(0);
        base.newPrice = rawToPrice(payload.readBigInt64LE(8));
        base.newQty = rawToQty(payload.readBigUInt64LE(16));
        base.timestamp = payload.readBigUInt64LE(24);
        base.timestampStr = tsToStr(base.timestamp as bigint);
        break;
      }

      case MSG_STOP_TRIGGERED: {
        // Payload: order_id(8) + trigger_price(i64,8) + timestamp(8) = 24
        base.orderId = payload.readBigUInt64LE(0);
        base.triggerPrice = rawToPrice(payload.readBigInt64LE(8));
        base.timestamp = payload.readBigUInt64LE(16);
        base.timestampStr = tsToStr(base.timestamp as bigint);
        break;
      }

      case MSG_PONG: {
        // Empty payload.
        break;
      }

      case MSG_ERROR: {
        // Payload: error_code(u16) + msg_len(u16) + msg(bytes)
        base.errorCode = payload.readUInt16LE(0);
        const msgLen = payload.readUInt16LE(2);
        base.message = payload.subarray(4, 4 + msgLen).toString("utf8");
        break;
      }

      default: {
        base.rawPayload = payload.toString("hex");
        break;
      }
    }

    return base;
  }
}

// ---------------------------------------------------------------------------
// Pretty-printer
// ---------------------------------------------------------------------------

function printEvent(event: MEEvent): void {
  const { type: etype, seq } = event;

  switch (etype) {
    case "HandshakeAck":
      console.log(`  [<] HandshakeAck  server_version=${event.serverVersion}  seq=${seq}`);
      break;
    case "OrderAccepted":
      console.log(
        `  [<] OrderAccepted  order_id=${event.orderId}  ts=${event.timestampStr}  seq=${seq}`
      );
      break;
    case "OrderRejected":
      console.log(
        `  [<] OrderRejected  order_id=${event.orderId}  reason=${event.reason}  ts=${event.timestampStr}  seq=${seq}`
      );
      break;
    case "Trade":
      console.log(
        `  [<] Trade  id=${event.tradeId}  symbol=${event.symbolId}  ` +
          `price=${event.price}  qty=${event.qty}  ` +
          `maker=${event.makerOrderId}  taker=${event.takerOrderId}  ` +
          `maker_side=${event.makerSide}  ts=${event.timestampStr}  seq=${seq}`
      );
      break;
    case "OrderFilled":
      console.log(
        `  [<] OrderFilled  order_id=${event.orderId}  filled_qty=${event.filledQty}  ` +
          `price=${event.price}  ts=${event.timestampStr}  seq=${seq}`
      );
      break;
    case "OrderPartiallyFilled":
      console.log(
        `  [<] OrderPartiallyFilled  order_id=${event.orderId}  filled=${event.filledQty}  ` +
          `remaining=${event.remainingQty}  price=${event.price}  ts=${event.timestampStr}  seq=${seq}`
      );
      break;
    case "OrderCancelled":
      console.log(
        `  [<] OrderCancelled  order_id=${event.orderId}  remaining=${event.remainingQty}  ` +
          `ts=${event.timestampStr}  seq=${seq}`
      );
      break;
    case "OrderAmended":
      console.log(
        `  [<] OrderAmended  order_id=${event.orderId}  new_price=${event.newPrice}  ` +
          `new_qty=${event.newQty}  ts=${event.timestampStr}  seq=${seq}`
      );
      break;
    case "StopTriggered":
      console.log(
        `  [<] StopTriggered  order_id=${event.orderId}  trigger_price=${event.triggerPrice}  ` +
          `ts=${event.timestampStr}  seq=${seq}`
      );
      break;
    case "Pong":
      console.log(`  [<] Pong  seq=${seq}`);
      break;
    case "Error":
      console.log(`  [<] Error  code=${event.errorCode}  message=${event.message}  seq=${seq}`);
      break;
    default:
      console.log(`  [<] ${etype}  ${JSON.stringify(event)}`);
  }
}

// ---------------------------------------------------------------------------
// Main demo
// ---------------------------------------------------------------------------

async function main(): Promise<void> {
  const host = process.argv[2] ?? "127.0.0.1";
  const port = parseInt(process.argv[3] ?? "9090", 10);

  console.log("=".repeat(60));
  console.log("Matching Engine -- TypeScript Client Example");
  console.log("=".repeat(60));

  const client = new MEClient(host, port);

  try {
    // Step 1: Connect and handshake.
    await client.connect();
    await client.handshake();

    // Step 2: Place a BUY limit order on symbol 1.
    //   Buy 1.0 BTC at 50,000.00 USD
    client.placeOrder({
      orderId: 1n,
      symbolId: 1,
      side: SIDE_BUY,
      orderType: ORDER_LIMIT,
      tif: TIF_GTC,
      price: 50_000.0,
      qty: 1.0,
    });

    // Read OrderAccepted.
    let events = await client.readAllEvents(1000);
    for (const e of events) printEvent(e);

    // Step 3: Place a SELL limit order that crosses the buy.
    //   Sell 0.5 BTC at 49,999.00 USD  (crosses the resting buy at 50k)
    //   This should trigger a trade.
    client.placeOrder({
      orderId: 2n,
      symbolId: 1,
      side: SIDE_SELL,
      orderType: ORDER_LIMIT,
      tif: TIF_GTC,
      price: 49_999.0,
      qty: 0.5,
    });

    // Read Trade + fill events.
    events = await client.readAllEvents(1000);
    for (const e of events) printEvent(e);

    // Step 4: Ping / Pong.
    client.ping();
    events = await client.readAllEvents(1000);
    for (const e of events) printEvent(e);

    // Step 5: Cancel the remaining portion of order 1.
    client.cancelOrder(1n, 1);
    events = await client.readAllEvents(1000);
    for (const e of events) printEvent(e);

    console.log();
    console.log("[*] Demo complete.");
  } catch (err: unknown) {
    if (err instanceof Error && (err as NodeJS.ErrnoException).code === "ECONNREFUSED") {
      console.error(
        `[!] Could not connect to ${host}:${port}. Make sure the matching engine is running.`
      );
    } else {
      console.error(`[!] Error: ${err}`);
    }
  } finally {
    client.close();
  }
}

main();
