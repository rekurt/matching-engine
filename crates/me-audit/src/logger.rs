use tracing::info;

use me_core::events::MatchEvent;
use me_core::order::Order;
use me_core::types::SymbolId;

use crate::types::AuditEvent;

/// Audit trail logger.
/// Emits structured JSON audit events via the `tracing` framework.
/// In production, these would be routed to a dedicated audit log sink
/// (e.g., Kafka topic, append-only file, or compliance database).
pub struct AuditLogger {
    enabled: bool,
}

impl AuditLogger {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Log an order submission (before matching).
    pub fn log_order_submitted(&self, order: &Order) {
        if !self.enabled {
            return;
        }
        let event = AuditEvent::OrderSubmitted {
            timestamp_ns: order.timestamp,
            order_id: order.id,
            symbol_id: order.symbol.0,
            side: format!("{:?}", order.side),
            order_type: format!("{:?}", order.order_type),
            price_raw: order.price.raw(),
            qty_raw: order.qty.raw(),
            tif: format!("{:?}", order.tif),
        };
        self.emit(event);
    }

    /// Log matching engine events (after matching).
    pub fn log_match_events(&self, events: &[MatchEvent], _symbol: SymbolId) {
        if !self.enabled {
            return;
        }
        for event in events {
            let audit = match event {
                MatchEvent::OrderAccepted { order_id, timestamp } => {
                    AuditEvent::OrderAccepted {
                        timestamp_ns: *timestamp,
                        order_id: *order_id,
                    }
                }
                MatchEvent::OrderRejected { order_id, reason, timestamp } => {
                    AuditEvent::OrderRejected {
                        timestamp_ns: *timestamp,
                        order_id: *order_id,
                        reason: format!("{:?}", reason),
                    }
                }
                MatchEvent::Trade(trade) => {
                    AuditEvent::TradeExecuted {
                        timestamp_ns: trade.timestamp,
                        trade_id: trade.trade_id,
                        symbol_id: trade.symbol.0,
                        price_raw: trade.price.raw(),
                        qty_raw: trade.qty.raw(),
                        maker_order_id: trade.maker_order_id,
                        taker_order_id: trade.taker_order_id,
                        maker_side: format!("{:?}", trade.maker_side),
                    }
                }
                MatchEvent::OrderFilled { order_id, filled_qty, price, timestamp } => {
                    AuditEvent::OrderFilled {
                        timestamp_ns: *timestamp,
                        order_id: *order_id,
                        filled_qty_raw: filled_qty.raw(),
                        price_raw: price.raw(),
                    }
                }
                MatchEvent::OrderPartiallyFilled { order_id, filled_qty, remaining_qty, price, timestamp } => {
                    AuditEvent::OrderPartiallyFilled {
                        timestamp_ns: *timestamp,
                        order_id: *order_id,
                        filled_qty_raw: filled_qty.raw(),
                        remaining_qty_raw: remaining_qty.raw(),
                        price_raw: price.raw(),
                    }
                }
                MatchEvent::OrderCancelled { order_id, remaining_qty, timestamp } => {
                    AuditEvent::OrderCancelled {
                        timestamp_ns: *timestamp,
                        order_id: *order_id,
                        remaining_qty_raw: remaining_qty.raw(),
                    }
                }
                MatchEvent::OrderAmended { order_id, new_price, new_qty, timestamp } => {
                    AuditEvent::OrderAmended {
                        timestamp_ns: *timestamp,
                        order_id: *order_id,
                        new_price_raw: new_price.raw(),
                        new_qty_raw: new_qty.raw(),
                    }
                }
                MatchEvent::StopTriggered { order_id, trigger_price, timestamp } => {
                    AuditEvent::StopTriggered {
                        timestamp_ns: *timestamp,
                        order_id: *order_id,
                        trigger_price_raw: trigger_price.raw(),
                    }
                }
            };
            self.emit(audit);
        }
    }

    /// Log trading halt.
    pub fn log_trading_halted(&self, symbol: SymbolId, reason: &str) {
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
            reason: reason.to_string(),
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
        // Use tracing with target "audit" so it can be routed to a dedicated sink
        let json = serde_json::to_string(&event).unwrap_or_default();
        info!(target: "audit", "{}", json);
    }
}

impl Default for AuditLogger {
    fn default() -> Self {
        Self::new(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_core::events::RejectReason;

    #[test]
    fn test_audit_logger_disabled() {
        let logger = AuditLogger::new(false);
        // Should not panic when disabled
        let events = vec![
            MatchEvent::OrderAccepted { order_id: 1, timestamp: 1000 },
        ];
        logger.log_match_events(&events, SymbolId(0));
    }

    #[test]
    fn test_audit_event_serialization() {
        let event = AuditEvent::OrderSubmitted {
            timestamp_ns: 1000,
            order_id: 42,
            symbol_id: 0,
            side: "Buy".to_string(),
            order_type: "Limit".to_string(),
            price_raw: 5000000000000,
            qty_raw: 100000000,
            tif: "GTC".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event_type\":\"OrderSubmitted\""));
        assert!(json.contains("\"order_id\":42"));
    }

    #[test]
    fn test_audit_all_event_types() {
        let logger = AuditLogger::new(true);
        let events = vec![
            MatchEvent::OrderAccepted { order_id: 1, timestamp: 100 },
            MatchEvent::OrderRejected { order_id: 2, reason: RejectReason::InvalidPrice, timestamp: 200 },
            MatchEvent::OrderCancelled { order_id: 3, remaining_qty: me_core::types::FixedQty::from_raw(1000), timestamp: 300 },
        ];
        // Just ensure it doesn't panic
        logger.log_match_events(&events, SymbolId(0));
    }
}
