//! Policy and diagnostics for matching-engine invariant violations.
//!
//! "Invariants" are internal data-structure contracts enforced by the
//! matching core — e.g. "a key yielded by `PriceLevelIter` must exist in
//! the slab". A violation at runtime indicates memory corruption or a
//! logic bug, never a user error.
//!
//! Because the correct response depends on deployment context, the
//! handling is configurable per symbol via [`InvariantPolicy`]:
//!
//! - **`Strict`**: panic immediately. Appropriate for dev/test/staging
//!   where bugs must surface loudly.
//! - **`HaltSymbol`**: log, emit a rejection, and halt only the affected
//!   symbol. Appropriate for production — isolates the blast radius so
//!   that other symbols sharing a shard stay available.
//! - **`Continue`**: log, emit a rejection, and keep processing. Only
//!   appropriate for recovery scenarios where partial service is better
//!   than no service.

use serde::{Deserialize, Serialize};

/// How the matching engine should react when an internal invariant is
/// violated. See module docs for full rationale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InvariantPolicy {
    /// Panic with a descriptive message. Dev/test default.
    #[default]
    Strict,
    /// Log, emit rejection, halt the symbol. Recommended for production.
    HaltSymbol,
    /// Log, emit rejection, keep processing. Use only for recovery.
    Continue,
}

/// Machine-readable identifier for each distinct matching-engine
/// invariant. Used in logs and Prometheus labels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvariantKind {
    /// A slab key yielded by `PriceLevelIter` did not resolve in the slab.
    SlabKeyFromIteratorMissing,
    /// `order_index` lookup failed for an order that was just accessed.
    OrderLocationMissing,
    /// A slab key taken from `OrderLocation` did not resolve in the slab.
    SlabKeyFromLocationMissing,
    /// `remove_order` failed for an order that was just verified to exist.
    OrderRemovalAfterVerification,
    /// `Slab::remove` was called on a vacant entry (caller bug).
    SlabRemoveVacant,
}

impl InvariantKind {
    /// Stable string identifier for metrics labels and structured logs.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::SlabKeyFromIteratorMissing => "slab_key_from_iterator_missing",
            Self::OrderLocationMissing => "order_location_missing",
            Self::SlabKeyFromLocationMissing => "slab_key_from_location_missing",
            Self::OrderRemovalAfterVerification => "order_removal_after_verification",
            Self::SlabRemoveVacant => "slab_remove_vacant",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_strict() {
        assert_eq!(InvariantPolicy::default(), InvariantPolicy::Strict);
    }

    #[test]
    fn kind_str_is_stable() {
        // These strings appear in Prometheus labels and dashboards —
        // changing them is a breaking change for operators.
        assert_eq!(
            InvariantKind::SlabKeyFromIteratorMissing.as_str(),
            "slab_key_from_iterator_missing"
        );
        assert_eq!(
            InvariantKind::OrderLocationMissing.as_str(),
            "order_location_missing"
        );
    }

    #[test]
    fn policy_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&InvariantPolicy::HaltSymbol).unwrap(),
            "\"halt_symbol\""
        );
        assert_eq!(
            serde_json::to_string(&InvariantPolicy::Continue).unwrap(),
            "\"continue\""
        );
        let p: InvariantPolicy = serde_json::from_str("\"strict\"").unwrap();
        assert_eq!(p, InvariantPolicy::Strict);
    }
}
