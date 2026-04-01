use std::fmt;

use crate::events::RejectReason;
use crate::types::OrderId;

/// Errors that can occur in the matching engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineError {
    OrderRejected {
        order_id: OrderId,
        reason: RejectReason,
    },
    OrderNotFound {
        order_id: OrderId,
    },
    SymbolNotFound {
        symbol: String,
    },
    SymbolAlreadyExists {
        symbol: String,
    },
    InvalidAmend {
        order_id: OrderId,
        reason: String,
    },
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OrderRejected { order_id, reason } => {
                write!(f, "Order {order_id} rejected: {reason:?}")
            }
            Self::OrderNotFound { order_id } => {
                write!(f, "Order {order_id} not found")
            }
            Self::SymbolNotFound { symbol } => {
                write!(f, "Symbol {symbol} not found")
            }
            Self::SymbolAlreadyExists { symbol } => {
                write!(f, "Symbol {symbol} already exists")
            }
            Self::InvalidAmend { order_id, reason } => {
                write!(f, "Invalid amend for order {order_id}: {reason}")
            }
        }
    }
}

impl std::error::Error for EngineError {}
