//! Standalone Rust client example for the matching engine.
//!
//! Demonstrates connecting over TCP, performing the binary protocol handshake,
//! placing two crossing limit orders, and reading the resulting execution events.
//!
//! Default server address: `127.0.0.1:9090` (override with `ME_ADDR` env var).
//!
//! # Wire Protocol
//!
//! Every frame on the wire has a 22-byte header followed by a variable-length
//! payload:
//!
//! ```text
//! +----------+----------+----------+----------+-----------------+
//! | len: u32 | ver: u8  | type: u8 | seq: u64 | payload: [u8]  |
//! | 4 bytes  | 1 byte   | 1 byte   | 8 bytes  | `len` bytes    |
//! +----------+----------+----------+----------+-----------------+
//! ```
//!
//! - `len` is the payload size only (does not include the 22-byte header).
//! - All multi-byte integers are **little-endian**.
//! - Prices are `i64` with implicit scale 10^8 (1.0 = 100_000_000).
//! - Quantities are `u64` with implicit scale 10^8.

use std::env;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// Protocol constants
// ---------------------------------------------------------------------------

/// Size of the frame header: len(4) + ver(1) + type(1) + seq(8) = 14 bytes.
const HEADER_SIZE: usize = 22;

/// Current wire protocol version.
const PROTOCOL_VERSION: u8 = 2;

/// Decimal scale factor: all prices and quantities are fixed-point with 10^8.
const DECIMAL_SCALE: f64 = 100_000_000.0;

// -- Inbound message types (client -> engine) --
const MSG_PLACE_ORDER: u8 = 0x01;
const MSG_CANCEL_ORDER: u8 = 0x02;
const MSG_HANDSHAKE: u8 = 0x05;

// -- Outbound message types (engine -> client) --
const MSG_ORDER_ACCEPTED: u8 = 0x10;
const MSG_ORDER_REJECTED: u8 = 0x11;
const MSG_TRADE: u8 = 0x12;
const MSG_ORDER_FILLED: u8 = 0x13;
const MSG_ORDER_PARTIALLY_FILLED: u8 = 0x14;
const MSG_ORDER_CANCELLED: u8 = 0x15;
const MSG_ORDER_AMENDED: u8 = 0x16;
const MSG_STOP_TRIGGERED: u8 = 0x17;
const MSG_PONG: u8 = 0x18;
const MSG_ERROR: u8 = 0x19;
const MSG_HANDSHAKE_ACK: u8 = 0x1A;

// -- Order sides --
const SIDE_BUY: u8 = 0;
const SIDE_SELL: u8 = 1;

// -- Order types --
const ORDER_TYPE_LIMIT: u8 = 0;

// -- Time-in-force --
const TIF_GTC: u8 = 0; // Good-Til-Cancelled

// ---------------------------------------------------------------------------
// Fixed-point helpers
// ---------------------------------------------------------------------------

/// Convert a floating-point price to a fixed-point i64 (scale 10^8).
fn price_to_fixed(price: f64) -> i64 {
    (price * DECIMAL_SCALE) as i64
}

/// Convert a floating-point quantity to a fixed-point u64 (scale 10^8).
fn qty_to_fixed(qty: f64) -> u64 {
    (qty * DECIMAL_SCALE) as u64
}

/// Format a fixed-point i64 price for display.
fn format_price(raw: i64) -> f64 {
    raw as f64 / DECIMAL_SCALE
}

/// Format a fixed-point u64 quantity for display.
fn format_qty(raw: u64) -> f64 {
    raw as f64 / DECIMAL_SCALE
}

/// Human-readable side name.
fn side_name(side: u8) -> &'static str {
    match side {
        SIDE_BUY => "Buy",
        SIDE_SELL => "Sell",
        _ => "Unknown",
    }
}

// ---------------------------------------------------------------------------
// Frame encoding / decoding
// ---------------------------------------------------------------------------

/// Encode a protocol frame (header + payload) ready for transmission.
///
/// Layout: `len(u32 LE) + ver(u8) + type(u8) + seq(u64 LE) + request_id(u64 LE) + payload`.
fn encode_frame(msg_type: u8, seq: u64, payload: &[u8]) -> Vec<u8> {
    let payload_len = payload.len() as u32;
    let mut buf = Vec::with_capacity(HEADER_SIZE + payload.len());

    buf.extend_from_slice(&payload_len.to_le_bytes()); // 4 bytes: payload length
    buf.push(PROTOCOL_VERSION);                         // 1 byte:  version
    buf.push(msg_type);                                 // 1 byte:  message type
    buf.extend_from_slice(&seq.to_le_bytes());          // 8 bytes: sequence number
    let request_id = if matches!(msg_type, 0x01 | 0x02 | 0x03 | 0x07 | 0x09 | 0x0B | 0x0C) { seq } else { 0 };
    buf.extend_from_slice(&request_id.to_le_bytes());
    buf.extend_from_slice(payload);                     // N bytes: payload

    buf
}

/// Read exactly one frame from the stream.
///
/// Returns `(message_type, sequence_number, payload_bytes)`.
async fn read_frame(stream: &mut TcpStream) -> Result<(u8, u64, Vec<u8>), String> {
    // Read the 22-byte header.
    let mut hdr = [0u8; HEADER_SIZE];
    stream
        .read_exact(&mut hdr)
        .await
        .map_err(|e| format!("failed to read frame header: {e}"))?;

    let payload_len = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
    if hdr[4] != PROTOCOL_VERSION || payload_len > 1024 * 1024 {
        return Err("invalid response version or oversized payload".into());
    }
    let msg_type = hdr[5];
    let seq = u64::from_le_bytes([hdr[6], hdr[7], hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13]]);

    // Read the payload (may be zero-length).
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream
            .read_exact(&mut payload)
            .await
            .map_err(|e| format!("failed to read payload ({payload_len} bytes): {e}"))?;
    }

    Ok((msg_type, seq, payload))
}

// ---------------------------------------------------------------------------
// Command builders
// ---------------------------------------------------------------------------

/// Build a Handshake payload (single byte: protocol version).
fn encode_handshake_payload() -> Vec<u8> {
    vec![PROTOCOL_VERSION]
}

/// Build a PlaceOrder payload (37 bytes).
///
/// Layout:
/// - order_id:   u64  (8 bytes)
/// - symbol_id:  u16  (2 bytes)
/// - side:       u8   (1 byte)
/// - order_type: u8   (1 byte)
/// - tif:        u8   (1 byte)
/// - price:      i64  (8 bytes)
/// - stop_price: i64  (8 bytes)
/// - qty:        u64  (8 bytes)
fn encode_place_order(
    order_id: u64,
    symbol_id: u16,
    side: u8,
    order_type: u8,
    tif: u8,
    price: i64,
    stop_price: i64,
    qty: u64,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(37);
    buf.extend_from_slice(&order_id.to_le_bytes());
    buf.extend_from_slice(&symbol_id.to_le_bytes());
    buf.push(side);
    buf.push(order_type);
    buf.push(tif);
    buf.extend_from_slice(&price.to_le_bytes());
    buf.extend_from_slice(&stop_price.to_le_bytes());
    buf.extend_from_slice(&qty.to_le_bytes());
    buf
}

/// Build a CancelOrder payload (10 bytes).
///
/// Layout:
/// - order_id:  u64 (8 bytes)
/// - symbol_id: u16 (2 bytes)
#[allow(dead_code)]
fn encode_cancel_order(order_id: u64, symbol_id: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(10);
    buf.extend_from_slice(&order_id.to_le_bytes());
    buf.extend_from_slice(&symbol_id.to_le_bytes());
    buf
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

/// Perform the protocol handshake: send Handshake, expect HandshakeAck.
///
/// Returns the server protocol version on success.
async fn handshake(stream: &mut TcpStream, seq: &mut u64) -> Result<u8, String> {
    *seq += 1;
    let payload = encode_handshake_payload();
    let frame = encode_frame(MSG_HANDSHAKE, *seq, &payload);

    stream
        .write_all(&frame)
        .await
        .map_err(|e| format!("handshake write failed: {e}"))?;

    let (msg_type, _seq, resp) = read_frame(stream).await?;

    match msg_type {
        MSG_HANDSHAKE_ACK => {
            if resp.is_empty() {
                return Err("HandshakeAck payload is empty".into());
            }
            let server_version = resp[0];
            println!("[handshake] connected, server protocol version: {server_version}");
            Ok(server_version)
        }
        MSG_ERROR => {
            let (code, msg) = decode_error(&resp);
            Err(format!("handshake rejected: code={code}, message={msg}"))
        }
        other => Err(format!("unexpected response to handshake: 0x{other:02X}")),
    }
}

// ---------------------------------------------------------------------------
// Send helpers
// ---------------------------------------------------------------------------

/// Send a PlaceOrder command.
async fn place_order(
    stream: &mut TcpStream,
    seq: &mut u64,
    order_id: u64,
    symbol_id: u16,
    side: u8,
    price: f64,
    qty: f64,
) -> Result<(), String> {
    *seq += 1;
    let payload = encode_place_order(
        order_id,
        symbol_id,
        side,
        ORDER_TYPE_LIMIT,
        TIF_GTC,
        price_to_fixed(price),
        0, // no stop price
        qty_to_fixed(qty),
    );
    let frame = encode_frame(MSG_PLACE_ORDER, *seq, &payload);

    stream
        .write_all(&frame)
        .await
        .map_err(|e| format!("place_order write failed: {e}"))?;

    println!(
        "[send] PlaceOrder id={order_id} symbol={symbol_id} side={} price={price} qty={qty}",
        side_name(side),
    );
    Ok(())
}

/// Send a CancelOrder command.
#[allow(dead_code)]
async fn cancel_order(
    stream: &mut TcpStream,
    seq: &mut u64,
    order_id: u64,
    symbol_id: u16,
) -> Result<(), String> {
    *seq += 1;
    let payload = encode_cancel_order(order_id, symbol_id);
    let frame = encode_frame(MSG_CANCEL_ORDER, *seq, &payload);

    stream
        .write_all(&frame)
        .await
        .map_err(|e| format!("cancel_order write failed: {e}"))?;

    println!("[send] CancelOrder id={order_id} symbol={symbol_id}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Event decoders
// ---------------------------------------------------------------------------

/// Decode an Error frame payload: error_code(u16) + msg_len(u16) + msg(bytes).
fn decode_error(data: &[u8]) -> (u16, String) {
    if data.len() < 4 {
        return (0, "<malformed error frame>".into());
    }
    let code = u16::from_le_bytes([data[0], data[1]]);
    let msg_len = u16::from_le_bytes([data[2], data[3]]) as usize;
    let msg = if msg_len > 0 && data.len() >= 4 + msg_len {
        String::from_utf8_lossy(&data[4..4 + msg_len]).into_owned()
    } else {
        String::new()
    };
    (code, msg)
}

/// Read one event frame from the stream and print a human-readable description.
///
/// Returns `true` if the event was successfully decoded, `false` on EOF or
/// unrecoverable error.
async fn read_event(stream: &mut TcpStream) -> Result<bool, String> {
    let (msg_type, _seq, data) = read_frame(stream).await?;

    match msg_type {
        MSG_ORDER_ACCEPTED => {
            // order_id(u64) + timestamp(u64) = 16 bytes
            if data.len() < 16 {
                println!("[event] OrderAccepted (truncated payload)");
                return Ok(true);
            }
            let order_id = u64::from_le_bytes(data[0..8].try_into().unwrap());
            let timestamp = u64::from_le_bytes(data[8..16].try_into().unwrap());
            println!("[event] OrderAccepted order_id={order_id} ts={timestamp}");
        }

        MSG_ORDER_REJECTED => {
            // order_id(u64) + reason(u8) + timestamp(u64) = 17 bytes
            if data.len() < 17 {
                println!("[event] OrderRejected (truncated payload)");
                return Ok(true);
            }
            let order_id = u64::from_le_bytes(data[0..8].try_into().unwrap());
            let reason = data[8];
            let timestamp = u64::from_le_bytes(data[9..17].try_into().unwrap());
            println!("[event] OrderRejected order_id={order_id} reason={reason} ts={timestamp}");
        }

        MSG_TRADE => {
            // trade_id(u64) + symbol_id(u16) + price(i64) + qty(u64)
            //   + maker_order_id(u64) + taker_order_id(u64)
            //   + maker_side(u8) + timestamp(u64)
            // = 51 bytes
            if data.len() < 51 {
                println!("[event] Trade (truncated payload)");
                return Ok(true);
            }
            let trade_id = u64::from_le_bytes(data[0..8].try_into().unwrap());
            let symbol_id = u16::from_le_bytes(data[8..10].try_into().unwrap());
            let price = i64::from_le_bytes(data[10..18].try_into().unwrap());
            let qty = u64::from_le_bytes(data[18..26].try_into().unwrap());
            let maker_id = u64::from_le_bytes(data[26..34].try_into().unwrap());
            let taker_id = u64::from_le_bytes(data[34..42].try_into().unwrap());
            let maker_side = data[42];
            let timestamp = u64::from_le_bytes(data[43..51].try_into().unwrap());
            println!(
                "[event] Trade id={trade_id} symbol={symbol_id} price={:.8} qty={:.8} \
                 maker={maker_id} taker={taker_id} maker_side={} ts={timestamp}",
                format_price(price),
                format_qty(qty),
                side_name(maker_side),
            );
        }

        MSG_ORDER_FILLED => {
            // order_id(u64) + filled_qty(u64) + price(i64) + timestamp(u64) = 32 bytes
            if data.len() < 32 {
                println!("[event] OrderFilled (truncated payload)");
                return Ok(true);
            }
            let order_id = u64::from_le_bytes(data[0..8].try_into().unwrap());
            let filled_qty = u64::from_le_bytes(data[8..16].try_into().unwrap());
            let price = i64::from_le_bytes(data[16..24].try_into().unwrap());
            let timestamp = u64::from_le_bytes(data[24..32].try_into().unwrap());
            println!(
                "[event] OrderFilled order_id={order_id} filled_qty={:.8} price={:.8} ts={timestamp}",
                format_qty(filled_qty),
                format_price(price),
            );
        }

        MSG_ORDER_PARTIALLY_FILLED => {
            // order_id(u64) + filled_qty(u64) + remaining_qty(u64) + price(i64)
            //   + timestamp(u64) = 40 bytes
            if data.len() < 40 {
                println!("[event] OrderPartiallyFilled (truncated payload)");
                return Ok(true);
            }
            let order_id = u64::from_le_bytes(data[0..8].try_into().unwrap());
            let filled_qty = u64::from_le_bytes(data[8..16].try_into().unwrap());
            let remaining_qty = u64::from_le_bytes(data[16..24].try_into().unwrap());
            let price = i64::from_le_bytes(data[24..32].try_into().unwrap());
            let timestamp = u64::from_le_bytes(data[32..40].try_into().unwrap());
            println!(
                "[event] OrderPartiallyFilled order_id={order_id} filled={:.8} remaining={:.8} \
                 price={:.8} ts={timestamp}",
                format_qty(filled_qty),
                format_qty(remaining_qty),
                format_price(price),
            );
        }

        MSG_ORDER_CANCELLED => {
            // order_id(u64) + remaining_qty(u64) + timestamp(u64) = 24 bytes
            if data.len() < 24 {
                println!("[event] OrderCancelled (truncated payload)");
                return Ok(true);
            }
            let order_id = u64::from_le_bytes(data[0..8].try_into().unwrap());
            let remaining_qty = u64::from_le_bytes(data[8..16].try_into().unwrap());
            let timestamp = u64::from_le_bytes(data[16..24].try_into().unwrap());
            println!(
                "[event] OrderCancelled order_id={order_id} remaining_qty={:.8} ts={timestamp}",
                format_qty(remaining_qty),
            );
        }

        MSG_ORDER_AMENDED => {
            // order_id(u64) + new_price(i64) + new_qty(u64) + timestamp(u64) = 32 bytes
            if data.len() < 32 {
                println!("[event] OrderAmended (truncated payload)");
                return Ok(true);
            }
            let order_id = u64::from_le_bytes(data[0..8].try_into().unwrap());
            let new_price = i64::from_le_bytes(data[8..16].try_into().unwrap());
            let new_qty = u64::from_le_bytes(data[16..24].try_into().unwrap());
            let timestamp = u64::from_le_bytes(data[24..32].try_into().unwrap());
            println!(
                "[event] OrderAmended order_id={order_id} new_price={:.8} new_qty={:.8} ts={timestamp}",
                format_price(new_price),
                format_qty(new_qty),
            );
        }

        MSG_STOP_TRIGGERED => {
            // order_id(u64) + trigger_price(i64) + timestamp(u64) = 24 bytes
            if data.len() < 24 {
                println!("[event] StopTriggered (truncated payload)");
                return Ok(true);
            }
            let order_id = u64::from_le_bytes(data[0..8].try_into().unwrap());
            let trigger_price = i64::from_le_bytes(data[8..16].try_into().unwrap());
            let timestamp = u64::from_le_bytes(data[16..24].try_into().unwrap());
            println!(
                "[event] StopTriggered order_id={order_id} trigger_price={:.8} ts={timestamp}",
                format_price(trigger_price),
            );
        }

        MSG_PONG => {
            println!("[event] Pong");
        }

        MSG_ERROR => {
            let (code, msg) = decode_error(&data);
            println!("[event] Error code={code} message=\"{msg}\"");
        }

        MSG_HANDSHAKE_ACK => {
            let ver = if data.is_empty() { 0 } else { data[0] };
            println!("[event] HandshakeAck server_version={ver}");
        }

        unknown => {
            println!("[event] Unknown message type 0x{unknown:02X} ({} bytes)", data.len());
        }
    }

    Ok(true)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Resolve server address from environment or use default.
    let addr = env::var("ME_ADDR").unwrap_or_else(|_| "127.0.0.1:9090".to_string());

    println!("=== Matching Engine Rust Client Example ===");
    println!("Connecting to {addr} ...");

    let mut stream = TcpStream::connect(&addr).await?;
    println!("TCP connection established.");

    // Monotonically increasing sequence number for outbound frames.
    let mut seq: u64 = 0;

    // -----------------------------------------------------------------------
    // Step 1: Handshake
    // -----------------------------------------------------------------------
    handshake(&mut stream, &mut seq).await?;

    // -----------------------------------------------------------------------
    // Step 2: Place a Buy limit order
    //
    //   Order ID 1, symbol 0, Buy side, price 50000.00, qty 1.5
    // -----------------------------------------------------------------------
    place_order(&mut stream, &mut seq, 1, 0, SIDE_BUY, 50_000.0, 1.5).await?;

    // -----------------------------------------------------------------------
    // Step 3: Place a Sell limit order that crosses the buy
    //
    //   Order ID 2, symbol 0, Sell side, price 50000.00, qty 1.5
    //
    //   Because the sell price <= the resting buy price, the engine should
    //   match them immediately and produce trade + fill events.
    // -----------------------------------------------------------------------
    place_order(&mut stream, &mut seq, 2, 0, SIDE_SELL, 50_000.0, 1.5).await?;

    // -----------------------------------------------------------------------
    // Step 4: Read events from the engine
    //
    //   We expect a sequence like:
    //     - OrderAccepted  (order 1)
    //     - OrderAccepted  (order 2)
    //     - Trade           (order 1 vs order 2)
    //     - OrderFilled     (order 1)
    //     - OrderFilled     (order 2)
    //
    //   We read a fixed number of events for this demo. In production you
    //   would read in a loop until EOF or a shutdown signal.
    // -----------------------------------------------------------------------
    println!("\n--- Reading events ---");

    let expected_events = 5;
    for i in 0..expected_events {
        match read_event(&mut stream).await {
            Ok(true) => {}
            Ok(false) => {
                println!("Connection closed by server after {i} events.");
                break;
            }
            Err(e) => {
                eprintln!("Error reading event {i}: {e}");
                break;
            }
        }
    }

    println!("\n--- Demo complete ---");
    Ok(())
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    #[test]
    fn cancel_uses_v2_request_header() {
        let frame = encode_frame(0x02, 7, &[0; 10]);
        assert_eq!(frame.len(), 32);
        assert_eq!(frame[4], 2);
        assert_eq!(&frame[14..22], &7u64.to_le_bytes());
    }
}
