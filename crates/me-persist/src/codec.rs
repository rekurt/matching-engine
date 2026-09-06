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
        WalCommand::Decision { .. }
        | WalCommand::TradingState { .. }
        | WalCommand::SymbolConfig { .. } => {
            buf.push(4);
            buf.extend(serde_json::to_vec(cmd).expect("serializable journal record"));
        }
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
        4 => {
            let command: WalCommand = serde_json::from_slice(rest).map_err(|e| e.to_string())?;
            if let WalCommand::Decision { command: inner, .. } = &command {
                if !matches!(
                    inner.as_ref(),
                    WalCommand::PlaceOrder(_)
                        | WalCommand::CancelOrder { .. }
                        | WalCommand::AmendOrder { .. }
                ) {
                    return Err("Decision must contain a single order command".into());
                }
            }
            Ok(command)
        }
        TAG_PLACE_ORDER => {
            if ![ORDER_ENCODED_SIZE_V4, ORDER_ENCODED_SIZE].contains(&rest.len()) {
                return Err("Invalid PlaceOrder length".into());
            }
            let order = decode_order(rest)?;
            Ok(WalCommand::PlaceOrder(order))
        }
        TAG_CANCEL_ORDER => {
            if rest.len() != 18 {
                return Err("Invalid CancelOrder length".to_string());
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
            let mut read_option = || -> Result<Option<[u8; 8]>, String> {
                let tag = *rest.get(offset).ok_or("AmendOrder missing option tag")?;
                offset += 1;
                match tag {
                    0 => Ok(None),
                    1 => {
                        let value = rest
                            .get(offset..offset + 8)
                            .ok_or("AmendOrder truncated option value")?;
                        offset += 8;
                        Ok(Some(value.try_into().unwrap()))
                    }
                    _ => Err("AmendOrder invalid option tag".into()),
                }
            };
            let new_price =
                read_option()?.map(|value| FixedPrice::from_raw(i64::from_le_bytes(value)));
            let new_qty = read_option()?.map(|value| FixedQty::from_raw(u64::from_le_bytes(value)));
            if offset != rest.len() {
                return Err("AmendOrder trailing bytes".into());
            }
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

/// Size of encoded order in bytes.
/// v2: +16 bytes for client_session_id. v3: +1 byte for `flags`.
/// v4: +8 bytes for `expire_at` (GTD). v5: +8 bytes for `trail_offset`.
/// v6: +8 bytes for `display_qty` (iceberg).
pub const ORDER_ENCODED_SIZE: usize = 111;

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
    buf.extend_from_slice(&order.expire_at.to_le_bytes());
    buf.extend_from_slice(&order.trail_offset.raw().to_le_bytes());
    buf.extend_from_slice(&order.display_qty.raw().to_le_bytes());
    buf.extend_from_slice(&order.timestamp.to_le_bytes());
    buf.extend_from_slice(&order.sequence.to_le_bytes());
    buf.extend_from_slice(&order.symbol.0.to_le_bytes());
    buf.push(order.side as u8);
    buf.push(order.order_type as u8);
    buf.push(order.tif as u8);
    buf.push(order.status as u8);
    buf.push(order.flags);
    buf.extend_from_slice(order.client_session_id.as_bytes());
}

/// Size of the pre-trailing/iceberg encoded order (v4): expire_at present, but
/// no trail_offset/display_qty. A WAL written by the previous release still
/// carries these shorter records; decoding them by length (defaulting the two
/// new fields to zero) lets recovery replay them instead of mistaking the
/// shorter record for a torn tail and truncating the WAL.
const ORDER_ENCODED_SIZE_V4: usize = 95;

fn decode_order(data: &[u8]) -> Result<Order, String> {
    use me_core::order::{OrderStatus, OrderType, TimeInForce};
    use me_core::types::Side;

    // The v4 and current layouts share the byte 0..56 prefix (id..expire_at).
    // The current layout then inserts trail_offset(8)+display_qty(8) before the
    // tail (timestamp..session), so the tail is shifted by exactly 16 bytes.
    // `tail` is where that common tail begins for the detected layout.
    // `extended` (>= 111) stays lenient on length because the snapshot reader
    // hands `decode_order` the remaining buffer (orders + trailing CRC), not a
    // single record — it reads the first 111 bytes and strides on. A
    // non-extended record, by contrast, must be EXACTLY the v4 size: a length
    // in (95, 111) is not produced by any released encoder, so it is a torn or
    // corrupt record (or the never-released intra-branch v5=103 layout) and is
    // rejected loudly rather than mis-decoded against the v4 offsets.
    let extended = data.len() >= ORDER_ENCODED_SIZE;
    if !extended && data.len() != ORDER_ENCODED_SIZE_V4 {
        return Err(format!(
            "Order data bad length {} (expected {ORDER_ENCODED_SIZE_V4} or >= {ORDER_ENCODED_SIZE})",
            data.len()
        ));
    }

    let id = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let price = FixedPrice::from_raw(i64::from_le_bytes(data[8..16].try_into().unwrap()));
    let stop_price = FixedPrice::from_raw(i64::from_le_bytes(data[16..24].try_into().unwrap()));
    let qty = FixedQty::from_raw(u64::from_le_bytes(data[24..32].try_into().unwrap()));
    let remaining_qty = FixedQty::from_raw(u64::from_le_bytes(data[32..40].try_into().unwrap()));
    let cumulative_qty = FixedQty::from_raw(u64::from_le_bytes(data[40..48].try_into().unwrap()));
    let expire_at = u64::from_le_bytes(data[48..56].try_into().unwrap());

    let (trail_offset, display_qty, tail) = if extended {
        (
            FixedPrice::from_raw(i64::from_le_bytes(data[56..64].try_into().unwrap())),
            FixedQty::from_raw(u64::from_le_bytes(data[64..72].try_into().unwrap())),
            72usize,
        )
    } else {
        (FixedPrice::ZERO, FixedQty::ZERO, 56usize)
    };

    let timestamp = u64::from_le_bytes(data[tail..tail + 8].try_into().unwrap());
    let sequence = u64::from_le_bytes(data[tail + 8..tail + 16].try_into().unwrap());
    let symbol = SymbolId(u16::from_le_bytes(
        data[tail + 16..tail + 18].try_into().unwrap(),
    ));
    let side = match data[tail + 18] {
        0 => Side::Buy,
        1 => Side::Sell,
        v => return Err(format!("Invalid side: {v}")),
    };
    let order_type = match data[tail + 19] {
        0 => OrderType::Limit,
        1 => OrderType::Market,
        2 => OrderType::StopLimit,
        3 => OrderType::StopMarket,
        v => return Err(format!("Invalid order type: {v}")),
    };
    let tif = match data[tail + 20] {
        0 => TimeInForce::GTC,
        1 => TimeInForce::IOC,
        2 => TimeInForce::FOK,
        3 => TimeInForce::GTD,
        v => return Err(format!("Invalid TIF: {v}")),
    };
    let status = match data[tail + 21] {
        0 => OrderStatus::New,
        1 => OrderStatus::PartiallyFilled,
        2 => OrderStatus::Filled,
        3 => OrderStatus::Cancelled,
        4 => OrderStatus::Rejected,
        5 => OrderStatus::Triggered,
        v => return Err(format!("Invalid status: {v}")),
    };
    let flags = data[tail + 22];
    let mut session_bytes = [0u8; 16];
    session_bytes.copy_from_slice(&data[tail + 23..tail + 39]);
    let client_session_id = me_core::SessionId::from_bytes(session_bytes);

    Ok(Order {
        id,
        price,
        stop_price,
        qty,
        remaining_qty,
        cumulative_qty,
        expire_at,
        trail_offset,
        display_qty,
        timestamp,
        sequence,
        symbol,
        side,
        order_type,
        tif,
        status,
        flags,
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
    fn test_decode_legacy_v4_order_defaults_trail_and_display() {
        // The pre-trailing/iceberg encoded order (v4) was 95 bytes: it carried
        // expire_at but no trail_offset/display_qty, so `timestamp` followed
        // expire_at directly. A WAL written by the previous release must still
        // replay after upgrade — otherwise recovery treats the shorter record
        // as a torn tail and silently truncates the WAL (data loss).
        const V4_SIZE: usize = 95;
        // v4 offsets: id 0, price 8, stop 16, qty 24, remaining 32, cumulative
        // 40, expire_at 48, timestamp 56, sequence 64, symbol 72, side 74,
        // order_type 75, tif 76, status 77, flags 78, session 79..95. Unset
        // fields (stop/cumulative/expire/sequence/type/tif/status/flags) stay 0.
        let mut data = vec![0u8; V4_SIZE];
        data[0..8].copy_from_slice(&7u64.to_le_bytes());
        data[8..16].copy_from_slice(&FixedPrice::from_f64(100.0).raw().to_le_bytes());
        data[24..32].copy_from_slice(&FixedQty::from_f64(2.0).raw().to_le_bytes());
        data[32..40].copy_from_slice(&FixedQty::from_f64(2.0).raw().to_le_bytes());
        data[56..64].copy_from_slice(&123u64.to_le_bytes()); // timestamp at the v4 offset
        data[72..74].copy_from_slice(&3u16.to_le_bytes());
        data[74] = 1; // side = Sell

        let order = decode_order_from(&data).expect("legacy v4 order must still decode");
        assert_eq!(order.id, 7);
        assert_eq!(order.qty, FixedQty::from_f64(2.0));
        assert_eq!(order.timestamp, 123);
        assert_eq!(order.symbol, SymbolId(3));
        assert_eq!(order.side, Side::Sell);
        assert_eq!(order.trail_offset, FixedPrice::ZERO);
        assert_eq!(order.display_qty, FixedQty::ZERO);
    }

    #[test]
    fn test_decode_gray_zone_length_rejected() {
        // A non-extended record must be EXACTLY the v4 size. A length in
        // (95, 111) — e.g. a torn WAL tail, or the never-released intra-branch
        // v5=103 layout — must be rejected loudly, not silently mis-decoded
        // against the v4 offsets (which would yield a garbage order).
        for len in [96usize, 103, 110] {
            let data = vec![0u8; len];
            assert!(
                decode_order_from(&data).is_err(),
                "length {len} is between v4 (95) and current (111); must be rejected"
            );
        }
    }

    #[test]
    fn test_roundtrip_preserves_flags() {
        use me_core::order::ORDER_FLAG_POST_ONLY;
        let mut order = Order::new_limit(
            7,
            SymbolId(1),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_f64(100.0),
            FixedQty::from_f64(1.0),
            123,
        );
        order.flags = ORDER_FLAG_POST_ONLY;
        let encoded = encode_wal_command(&WalCommand::PlaceOrder(order));
        match decode_wal_command(&encoded).unwrap() {
            WalCommand::PlaceOrder(o) => {
                assert_eq!(
                    o.flags, ORDER_FLAG_POST_ONLY,
                    "flags must survive roundtrip"
                );
            }
            _ => panic!("Expected PlaceOrder"),
        }
    }

    #[test]
    fn test_roundtrip_preserves_gtd_expiry() {
        let order = Order::new_limit(
            9,
            SymbolId(2),
            Side::Buy,
            TimeInForce::GTC,
            FixedPrice::from_f64(100.0),
            FixedQty::from_f64(1.0),
            123,
        )
        .with_expiry(1_700_000_000_000_000_000);
        let encoded = encode_wal_command(&WalCommand::PlaceOrder(order));
        match decode_wal_command(&encoded).unwrap() {
            WalCommand::PlaceOrder(o) => {
                assert_eq!(o.tif, TimeInForce::GTD);
                assert_eq!(o.expire_at, 1_700_000_000_000_000_000);
            }
            _ => panic!("Expected PlaceOrder"),
        }
    }

    #[test]
    fn test_roundtrip_preserves_iceberg_and_trail() {
        let order = Order::new_limit(
            11,
            SymbolId(3),
            Side::Sell,
            TimeInForce::GTC,
            FixedPrice::from_f64(100.0),
            FixedQty::from_f64(10.0),
            55,
        )
        .with_display_qty(FixedQty::from_f64(2.0));
        match decode_wal_command(&encode_wal_command(&WalCommand::PlaceOrder(order))).unwrap() {
            WalCommand::PlaceOrder(o) => {
                assert_eq!(o.display_qty, FixedQty::from_f64(2.0));
                assert!(o.is_iceberg());
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
    #[test]
    fn malformed_amends_return_errors_without_panicking() {
        for price in [None, Some(FixedPrice::from_raw(1))] {
            for qty in [None, Some(FixedQty::from_raw(1))] {
                let bytes = encode_wal_command(&WalCommand::AmendOrder {
                    order_id: 1,
                    symbol: SymbolId(0),
                    timestamp: 1,
                    new_price: price,
                    new_qty: qty,
                });
                for end in 0..bytes.len() {
                    assert!(decode_wal_command(&bytes[..end]).is_err(), "prefix {end}");
                }
                for offset in [19, if price.is_some() { 28 } else { 20 }] {
                    let mut invalid = bytes.clone();
                    invalid[offset] = 2;
                    assert!(
                        decode_wal_command(&invalid).is_err(),
                        "invalid option at {offset}"
                    );
                }
                let mut extra = bytes;
                extra.push(0);
                assert!(decode_wal_command(&extra).is_err());
            }
        }
    }

    #[test]
    fn wal_records_reject_trailing_bytes() {
        let commands = [
            WalCommand::PlaceOrder(Order::new_limit(
                1,
                SymbolId(0),
                Side::Buy,
                TimeInForce::GTC,
                FixedPrice::from_raw(1),
                FixedQty::from_raw(1),
                1,
            )),
            WalCommand::CancelOrder {
                order_id: 1,
                symbol: SymbolId(0),
                timestamp: 1,
            },
            WalCommand::TradingState {
                symbol: SymbolId(0),
                halted: true,
            },
            WalCommand::SymbolConfig {
                symbol: SymbolId(0),
                fingerprint: "test".into(),
            },
            WalCommand::Decision {
                command: Box::new(WalCommand::CancelOrder {
                    order_id: 1,
                    symbol: SymbolId(0),
                    timestamp: 1,
                }),
                session_id: me_core::SessionId::NONE,
                request_id: Some(1),
                item_index: 0,
                events: Vec::new(),
                expirations: Vec::new(),
                expiration_owners: Vec::new(),
                owners: Vec::new(),
                processing_timestamp: 1,
                apply: true,
            },
        ];
        for command in commands {
            let mut encoded = encode_wal_command(&command);
            assert!(decode_wal_command(&encoded).is_ok());
            encoded.push(0);
            assert!(decode_wal_command(&encoded).is_err());
        }
    }
    #[test]
    fn decision_journal_rejects_control_or_nested_commands() {
        let control = WalCommand::TradingState {
            symbol: SymbolId(0),
            halted: true,
        };
        let decision = WalCommand::Decision {
            command: Box::new(control),
            session_id: me_core::SessionId::NONE,
            request_id: None,
            item_index: 0,
            events: Vec::new(),
            expirations: Vec::new(),
            expiration_owners: Vec::new(),
            owners: Vec::new(),
            processing_timestamp: 1,
            apply: true,
        };
        assert!(decode_wal_command(&encode_wal_command(&decision)).is_err());
        let nested = WalCommand::Decision {
            command: Box::new(decision),
            session_id: me_core::SessionId::NONE,
            request_id: None,
            item_index: 0,
            events: Vec::new(),
            expirations: Vec::new(),
            expiration_owners: Vec::new(),
            owners: Vec::new(),
            processing_timestamp: 1,
            apply: true,
        };
        assert!(decode_wal_command(&encode_wal_command(&nested)).is_err());
    }
}
