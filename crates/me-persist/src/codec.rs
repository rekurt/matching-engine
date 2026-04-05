use me_core::order::Order;
use me_core::types::{FixedPrice, FixedQty, SymbolId};

use crate::wal::WalCommand;

/// Simple binary encoding for WAL commands.
/// Format: [tag: u8][fields...]
/// All integers are little-endian.
const TAG_PLACE_ORDER: u8 = 1;
const TAG_CANCEL_ORDER: u8 = 2;
const TAG_AMEND_ORDER: u8 = 3;

pub fn encode_wal_command(cmd: &WalCommand) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128);
    match cmd {
        WalCommand::PlaceOrder(order) => {
            buf.push(TAG_PLACE_ORDER);
            encode_order(order, &mut buf);
        }
        WalCommand::CancelOrder {
            order_id,
            symbol,
            timestamp,
        } => {
            buf.push(TAG_CANCEL_ORDER);
            buf.extend_from_slice(&order_id.to_le_bytes());
            buf.extend_from_slice(&symbol.0.to_le_bytes());
            buf.extend_from_slice(&timestamp.to_le_bytes());
        }
        WalCommand::AmendOrder {
            order_id,
            symbol,
            new_price,
            new_qty,
            timestamp,
        } => {
            buf.push(TAG_AMEND_ORDER);
            buf.extend_from_slice(&order_id.to_le_bytes());
            buf.extend_from_slice(&symbol.0.to_le_bytes());
            buf.extend_from_slice(&timestamp.to_le_bytes());
            // Encode Option<FixedPrice>
            match new_price {
                Some(p) => {
                    buf.push(1);
                    buf.extend_from_slice(&p.raw().to_le_bytes());
                }
                None => buf.push(0),
            }
            // Encode Option<FixedQty>
            match new_qty {
                Some(q) => {
                    buf.push(1);
                    buf.extend_from_slice(&q.raw().to_le_bytes());
                }
                None => buf.push(0),
            }
        }
    }
    buf
}

pub fn decode_wal_command(data: &[u8]) -> Result<WalCommand, String> {
    if data.is_empty() {
        return Err("Empty WAL command data".to_string());
    }

    let tag = data[0];
    let rest = &data[1..];

    match tag {
        TAG_PLACE_ORDER => {
            let order = decode_order(rest)?;
            Ok(WalCommand::PlaceOrder(order))
        }
        TAG_CANCEL_ORDER => {
            if rest.len() < 18 {
                return Err("CancelOrder too short".to_string());
            }
            let order_id = u64::from_le_bytes(rest[0..8].try_into().unwrap());
            let symbol = SymbolId(u16::from_le_bytes(rest[8..10].try_into().unwrap()));
            let timestamp = u64::from_le_bytes(rest[10..18].try_into().unwrap());
            Ok(WalCommand::CancelOrder {
                order_id,
                symbol,
                timestamp,
            })
        }
        TAG_AMEND_ORDER => {
            if rest.len() < 19 {
                return Err("AmendOrder too short".to_string());
            }
            let order_id = u64::from_le_bytes(rest[0..8].try_into().unwrap());
            let symbol = SymbolId(u16::from_le_bytes(rest[8..10].try_into().unwrap()));
            let timestamp = u64::from_le_bytes(rest[10..18].try_into().unwrap());
            let mut offset = 18;
            let new_price = if rest[offset] == 1 {
                offset += 1;
                let p = i64::from_le_bytes(rest[offset..offset + 8].try_into().unwrap());
                offset += 8;
                Some(FixedPrice::from_raw(p))
            } else {
                offset += 1;
                None
            };
            let new_qty = if rest[offset] == 1 {
                offset += 1;
                let q = u64::from_le_bytes(rest[offset..offset + 8].try_into().unwrap());
                Some(FixedQty::from_raw(q))
            } else {
                None
            };
            Ok(WalCommand::AmendOrder {
                order_id,
                symbol,
                new_price,
                new_qty,
                timestamp,
            })
        }
        _ => Err(format!("Unknown WAL command tag: {tag}")),
    }
}

/// Size of encoded order in bytes (v2: +16 bytes for client_session_id).
pub const ORDER_ENCODED_SIZE: usize = 86;

/// Encode an order into a buffer (public, used by snapshot).
pub fn encode_order_to(order: &Order, buf: &mut Vec<u8>) {
    encode_order(order, buf);
}

/// Decode an order from a byte slice (public, used by snapshot).
pub fn decode_order_from(data: &[u8]) -> Result<Order, String> {
    decode_order(data)
}

fn encode_order(order: &Order, buf: &mut Vec<u8>) {
    buf.extend_from_slice(&order.id.to_le_bytes());
    buf.extend_from_slice(&order.price.raw().to_le_bytes());
    buf.extend_from_slice(&order.stop_price.raw().to_le_bytes());
    buf.extend_from_slice(&order.qty.raw().to_le_bytes());
    buf.extend_from_slice(&order.remaining_qty.raw().to_le_bytes());
    buf.extend_from_slice(&order.cumulative_qty.raw().to_le_bytes());
    buf.extend_from_slice(&order.timestamp.to_le_bytes());
    buf.extend_from_slice(&order.sequence.to_le_bytes());
    buf.extend_from_slice(&order.symbol.0.to_le_bytes());
    buf.push(order.side as u8);
    buf.push(order.order_type as u8);
    buf.push(order.tif as u8);
    buf.push(order.status as u8);
    buf.extend_from_slice(&order.client_session_id);
}

fn decode_order(data: &[u8]) -> Result<Order, String> {
    use me_core::order::{OrderStatus, OrderType, TimeInForce};
    use me_core::types::Side;

    if data.len() < ORDER_ENCODED_SIZE {
        return Err(format!(
            "Order data too short: {} < {ORDER_ENCODED_SIZE}",
            data.len()
        ));
    }

    let id = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let price = FixedPrice::from_raw(i64::from_le_bytes(data[8..16].try_into().unwrap()));
    let stop_price = FixedPrice::from_raw(i64::from_le_bytes(data[16..24].try_into().unwrap()));
    let qty = FixedQty::from_raw(u64::from_le_bytes(data[24..32].try_into().unwrap()));
    let remaining_qty = FixedQty::from_raw(u64::from_le_bytes(data[32..40].try_into().unwrap()));
    let cumulative_qty = FixedQty::from_raw(u64::from_le_bytes(data[40..48].try_into().unwrap()));
    let timestamp = u64::from_le_bytes(data[48..56].try_into().unwrap());
    let sequence = u64::from_le_bytes(data[56..64].try_into().unwrap());
    let symbol = SymbolId(u16::from_le_bytes(data[64..66].try_into().unwrap()));
    let side = match data[66] {
        0 => Side::Buy,
        1 => Side::Sell,
        v => return Err(format!("Invalid side: {v}")),
    };
    let order_type = match data[67] {
        0 => OrderType::Limit,
        1 => OrderType::Market,
        2 => OrderType::StopLimit,
        3 => OrderType::StopMarket,
        v => return Err(format!("Invalid order type: {v}")),
    };
    let tif = match data[68] {
        0 => TimeInForce::GTC,
        1 => TimeInForce::IOC,
        2 => TimeInForce::FOK,
        v => return Err(format!("Invalid TIF: {v}")),
    };
    let status = match data[69] {
        0 => OrderStatus::New,
        1 => OrderStatus::PartiallyFilled,
        2 => OrderStatus::Filled,
        3 => OrderStatus::Cancelled,
        4 => OrderStatus::Rejected,
        5 => OrderStatus::Triggered,
        v => return Err(format!("Invalid status: {v}")),
    };
    let mut client_session_id = [0u8; 16];
    client_session_id.copy_from_slice(&data[70..86]);

    Ok(Order {
        id,
        price,
        stop_price,
        qty,
        remaining_qty,
        cumulative_qty,
        timestamp,
        sequence,
        symbol,
        side,
        order_type,
        tif,
        status,
        flags: 0,
        _pad: 0,
        client_session_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_core::order::{Order, TimeInForce};
    use me_core::types::*;

    #[test]
    fn test_roundtrip_place_order() {
        let order = Order::new_limit(
            42,
            SymbolId(5),
            Side::Sell,
            TimeInForce::IOC,
            FixedPrice::from_f64(50.25),
            FixedQty::from_f64(3.5),
            999,
        );
        let cmd = WalCommand::PlaceOrder(order);
        let encoded = encode_wal_command(&cmd);
        let decoded = decode_wal_command(&encoded).unwrap();

        match decoded {
            WalCommand::PlaceOrder(o) => {
                assert_eq!(o.id, 42);
                assert_eq!(o.symbol, SymbolId(5));
                assert_eq!(o.side, Side::Sell);
                assert_eq!(o.tif, TimeInForce::IOC);
            }
            _ => panic!("Expected PlaceOrder"),
        }
    }

    #[test]
    fn test_roundtrip_cancel() {
        let cmd = WalCommand::CancelOrder {
            order_id: 10,
            symbol: SymbolId(2),
            timestamp: 5000,
        };
        let encoded = encode_wal_command(&cmd);
        let decoded = decode_wal_command(&encoded).unwrap();

        match decoded {
            WalCommand::CancelOrder {
                order_id,
                symbol,
                timestamp,
            } => {
                assert_eq!(order_id, 10);
                assert_eq!(symbol, SymbolId(2));
                assert_eq!(timestamp, 5000);
            }
            _ => panic!("Expected CancelOrder"),
        }
    }

    #[test]
    fn test_roundtrip_amend() {
        let cmd = WalCommand::AmendOrder {
            order_id: 7,
            symbol: SymbolId(1),
            new_price: Some(FixedPrice::from_f64(200.0)),
            new_qty: None,
            timestamp: 8000,
        };
        let encoded = encode_wal_command(&cmd);
        let decoded = decode_wal_command(&encoded).unwrap();

        match decoded {
            WalCommand::AmendOrder {
                order_id,
                new_price,
                new_qty,
                ..
            } => {
                assert_eq!(order_id, 7);
                assert!(new_price.is_some());
                assert!(new_qty.is_none());
            }
            _ => panic!("Expected AmendOrder"),
        }
    }
}
