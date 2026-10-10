use tracing::info;

use me_core::events::MatchEvent;
use me_core::order::Order;
use me_core::types::SymbolId;

use crate::types::{AuditEntry, AuditEvent};

/// Synchronous audit trail logger.
/// Emits structured JSON audit events via the `tracing` framework.
///
/// For the async zero-alloc version, see [`crate::async_logger::AsyncAuditLogger`].
pub struct AuditLogger {
    enabled: bool,
}

impl AuditLogger {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Log an order submission (before matching). Zero-alloc: no format!().
    pub fn log_order_submitted(&self, order: &Order) {
        if !self.enabled {
            return;
        }
        let event = AuditEvent::OrderSubmitted {
            timestamp_ns: order.timestamp,
            order_id: order.id,
            symbol_id: order.symbol.0,
            side: order.side as u8,
            order_type: order.order_type as u8,
            price_raw: order.price.raw(),
            qty_raw: order.qty.raw(),
            tif: order.tif as u8,
        };
        self.emit(event);
    }

    /// Log matching engine events (after matching). Zero-alloc conversion.
    pub fn log_match_events(&self, events: &[MatchEvent], _symbol: SymbolId) {
        if !self.enabled {
            return;
        }
        for event in events {
            let audit = match event {
                MatchEvent::OrderAccepted {
                    order_id,
                    timestamp,
                } => AuditEvent::OrderAccepted {
                    timestamp_ns: *timestamp,
                    order_id: *order_id,
                },
                MatchEvent::OrderRejected {
                    order_id,
                    reason,
                    timestamp,
                } => AuditEvent::OrderRejected {
                    timestamp_ns: *timestamp,
                    order_id: *order_id,
                    reason: *reason as u8,
                },
                MatchEvent::Trade(trade) => AuditEvent::TradeExecuted {
                    timestamp_ns: trade.timestamp,
                    trade_id: trade.trade_id,
                    symbol_id: trade.symbol.0,
                    price_raw: trade.price.raw(),
                    qty_raw: trade.qty.raw(),
                    maker_order_id: trade.maker_order_id,
                    taker_order_id: trade.taker_order_id,
                    maker_side: trade.maker_side as u8,
                },
                MatchEvent::OrderFilled {
                    order_id,
                    filled_qty,
                    price,
                    timestamp,
                } => AuditEvent::OrderFilled {
                    timestamp_ns: *timestamp,
                    order_id: *order_id,
                    filled_qty_raw: filled_qty.raw(),
                    price_raw: price.raw(),
                },
                MatchEvent::OrderPartiallyFilled {
                    order_id,
                    filled_qty,
                    remaining_qty,
                    price,
                    timestamp,
                } => AuditEvent::OrderPartiallyFilled {
                    timestamp_ns: *timestamp,
                    order_id: *order_id,
                    filled_qty_raw: filled_qty.raw(),
                    remaining_qty_raw: remaining_qty.raw(),
                    price_raw: price.raw(),
                },
                MatchEvent::OrderCancelled {
                    order_id,
                    remaining_qty,
                    timestamp,
                } => AuditEvent::OrderCancelled {
                    timestamp_ns: *timestamp,
                    order_id: *order_id,
                    remaining_qty_raw: remaining_qty.raw(),
                },
                MatchEvent::OrderAmended {
                    order_id,
                    new_price,
                    new_qty,
                    timestamp,
                } => AuditEvent::OrderAmended {
                    timestamp_ns: *timestamp,
                    order_id: *order_id,
                    new_price_raw: new_price.raw(),
                    new_qty_raw: new_qty.raw(),
                },
                MatchEvent::StopTriggered {
                    order_id,
                    trigger_price,
                    timestamp,
                } => AuditEvent::StopTriggered {
                    timestamp_ns: *timestamp,
                    order_id: *order_id,
                    trigger_price_raw: trigger_price.raw(),
                },
                // Query-response events are not audit-loggable trading events.
                MatchEvent::OrderStatus { .. } | MatchEvent::OrderNotFound { .. } => continue,
            };
            self.emit(audit);
        }
    }

    /// Log trading halt.
    pub fn log_trading_halted(&self, symbol: SymbolId, reason: u8) {
        if !self.enabled {
            return;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        self.emit(AuditEvent::TradingHalted {
            timestamp_ns: now,
            symbol_id: symbol.0,
            reason,
        });
    }

    /// Log trading resume.
    pub fn log_trading_resumed(&self, symbol: SymbolId) {
        if !self.enabled {
            return;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        self.emit(AuditEvent::TradingResumed {
            timestamp_ns: now,
            symbol_id: symbol.0,
        });
    }

    fn emit(&self, event: AuditEvent) {
        match serde_json::to_string(&event) {
            Ok(json) => info!(target: "audit", "{}", json),
            Err(e) => {
                // Serialization failure — emit a warning with the event
                // identity so the missing record can be reconstructed from
                // context (NEVER a silent empty line). Mirrors the async
                // logger pattern (async_logger.rs:207).
                tracing::warn!(
                    target: "audit",
                    error = %e,
                    event_debug = ?event,
                    "FAILED TO SERIALIZE AUDIT ENTRY — RECORD LOST"
                );
            }
        }
    }
}

impl Default for AuditLogger {
    fn default() -> Self {
        Self::new(true)
    }
}

/// Convert a `MatchEvent` into a fixed-size `AuditEntry` for the async SPSC path.
/// Zero-alloc, no heap.
pub fn match_event_to_entry(event: &MatchEvent, symbol: SymbolId) -> Option<AuditEntry> {
    use crate::types::event_type;

    let mut e = AuditEntry {
        symbol_id: symbol.0,
        ..AuditEntry::default()
    };

    match event {
        MatchEvent::OrderAccepted {
            order_id,
            timestamp,
        } => {
            e.event_type = event_type::ORDER_ACCEPTED;
            e.order_id = *order_id;
            e.timestamp_ns = *timestamp;
        }
        MatchEvent::OrderRejected {
            order_id,
            reason,
            timestamp,
        } => {
            e.event_type = event_type::ORDER_REJECTED;
            e.order_id = *order_id;
            e.timestamp_ns = *timestamp;
            e.reason = *reason as u8;
        }
        MatchEvent::Trade(trade) => {
            e.event_type = event_type::TRADE_EXECUTED;
            e.timestamp_ns = trade.timestamp;
            e.order_id = trade.taker_order_id;
            e.symbol_id = trade.symbol.0;
            e.price_raw = trade.price.raw();
            e.qty_raw = trade.qty.raw();
            e.maker_order_id = trade.maker_order_id;
            e.taker_order_id = trade.taker_order_id;
            e.side = trade.maker_side as u8;
            e.aux_u64 = trade.trade_id;
        }
        MatchEvent::OrderFilled {
            order_id,
            filled_qty,
            price,
            timestamp,
        } => {
            e.event_type = event_type::ORDER_FILLED;
            e.order_id = *order_id;
            e.timestamp_ns = *timestamp;
            e.qty_raw = filled_qty.raw();
            e.price_raw = price.raw();
        }
        MatchEvent::OrderPartiallyFilled {
            order_id,
            filled_qty,
            remaining_qty,
            price,
            timestamp,
        } => {
            e.event_type = event_type::ORDER_PARTIALLY_FILLED;
            e.order_id = *order_id;
            e.timestamp_ns = *timestamp;
            e.qty_raw = filled_qty.raw();
            e.aux_u64 = remaining_qty.raw();
            e.price_raw = price.raw();
        }
        MatchEvent::OrderCancelled {
            order_id,
            remaining_qty,
            timestamp,
        } => {
            e.event_type = event_type::ORDER_CANCELLED;
            e.order_id = *order_id;
            e.timestamp_ns = *timestamp;
            e.qty_raw = remaining_qty.raw();
        }
        MatchEvent::OrderAmended {
            order_id,
            new_price,
            new_qty,
            timestamp,
        } => {
            e.event_type = event_type::ORDER_AMENDED;
            e.order_id = *order_id;
            e.timestamp_ns = *timestamp;
            e.price_raw = new_price.raw();
            e.qty_raw = new_qty.raw();
        }
        MatchEvent::StopTriggered {
            order_id,
            trigger_price,
            timestamp,
        } => {
            e.event_type = event_type::STOP_TRIGGERED;
            e.order_id = *order_id;
            e.timestamp_ns = *timestamp;
            e.price_raw = trigger_price.raw();
        }
        MatchEvent::OrderStatus { .. } | MatchEvent::OrderNotFound { .. } => return None,
    }
    Some(e)
}

/// Convert an `Order` to an `AuditEntry` for submission logging.
pub fn order_to_entry(order: &Order) -> AuditEntry {
    use crate::types::event_type;
    AuditEntry {
        event_type: event_type::ORDER_SUBMITTED,
        timestamp_ns: order.timestamp,
        order_id: order.id,
        symbol_id: order.symbol.0,
        side: order.side as u8,
        order_type: order.order_type as u8,
        tif: order.tif as u8,
        price_raw: order.price.raw(),
        qty_raw: order.qty.raw(),
        ..AuditEntry::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_core::events::RejectReason;

    #[test]
    fn test_audit_logger_disabled() {
        let logger = AuditLogger::new(false);
        let events = vec![MatchEvent::OrderAccepted {
            order_id: 1,
            timestamp: 1000,
        }];
        logger.log_match_events(&events, SymbolId(0));
    }

    #[test]
    fn test_audit_event_serialization() {
        let event = AuditEvent::OrderSubmitted {
            timestamp_ns: 1000,
            order_id: 42,
            symbol_id: 0,
            side: 0,       // Buy
            order_type: 0, // Limit
            price_raw: 5000000000000,
            qty_raw: 100000000,
            tif: 0, // GTC
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event_type\":\"OrderSubmitted\""));
        assert!(json.contains("\"order_id\":42"));
        assert!(json.contains("\"side\":\"Buy\""));
        assert!(json.contains("\"order_type\":\"Limit\""));
        assert!(json.contains("\"tif\":\"GTC\""));
    }

    #[test]
    fn test_audit_all_event_types() {
        let logger = AuditLogger::new(true);
        let events = vec![
            MatchEvent::OrderAccepted {
                order_id: 1,
                timestamp: 100,
            },
            MatchEvent::OrderRejected {
                order_id: 2,
                reason: RejectReason::InvalidPrice,
                timestamp: 200,
            },
            MatchEvent::OrderCancelled {
                order_id: 3,
                remaining_qty: me_core::types::FixedQty::from_raw(1000),
                timestamp: 300,
            },
        ];
        logger.log_match_events(&events, SymbolId(0));
    }

    #[test]
    fn test_audit_entry_roundtrip() {
        let entry = AuditEntry {
            event_type: crate::types::event_type::ORDER_SUBMITTED,
            timestamp_ns: 12345,
            order_id: 99,
            symbol_id: 0,
            side: 0,
            order_type: 0,
            tif: 0,
            price_raw: 5000000000000,
            qty_raw: 100000000,
            ..AuditEntry::default()
        };
        let event = entry.to_audit_event();
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"order_id\":99"));
        assert!(json.contains("\"side\":\"Buy\""));
    }
}
