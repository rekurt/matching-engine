//! Integration tests for `InvariantPolicy` behavior during matching.
//!
//! We can't easily corrupt the book's internal state from outside the
//! crate, so these tests verify that the policy is read from config and
//! that the public accessors (`is_halted_by_invariant`,
//! `invariant_violations_total`) behave correctly after a direct call
//! to `record_invariant_violation`.
//!
//! The matching hot-path code paths (unwraps replaced by match+violation)
//! are exercised by the existing `matching_tests.rs` and
//! `property_tests.rs` — any regression there would fail those suites.

use me_book::book::OrderBook;
use me_core::invariant::{InvariantKind, InvariantPolicy};
use me_core::symbol::SymbolConfig;

fn config_with(policy: InvariantPolicy) -> SymbolConfig {
    let mut cfg = SymbolConfig::default_btc_usdt();
    cfg.invariant_policy = policy;
    cfg
}

#[test]
fn halt_policy_isolates_one_symbol() {
    // Two books: one configured to halt, one to continue. Inject a
    // violation into the first — the second must remain healthy.
    let halting = OrderBook::new(config_with(InvariantPolicy::HaltSymbol));
    let continuing = OrderBook::new(config_with(InvariantPolicy::Continue));

    halting.record_invariant_violation(InvariantKind::SlabKeyFromIteratorMissing, "isolation_test");

    assert!(halting.is_halted_by_invariant());
    assert!(!continuing.is_halted_by_invariant());
}

#[test]
fn violation_counter_monotonically_increases() {
    let book = OrderBook::new(config_with(InvariantPolicy::Continue));
    for _ in 0..5 {
        book.record_invariant_violation(InvariantKind::OrderLocationMissing, "monotonic_test");
    }
    assert_eq!(book.invariant_violations_total(), 5);
}

#[test]
fn default_policy_is_strict() {
    // A fresh SymbolConfig without explicit policy must be Strict so
    // tests panic loudly on any invariant bug.
    let cfg = SymbolConfig::default_btc_usdt();
    assert_eq!(cfg.invariant_policy, InvariantPolicy::Strict);
}

#[test]
#[should_panic(expected = "BUG: matching invariant violated")]
fn strict_policy_aborts() {
    let book = OrderBook::new(config_with(InvariantPolicy::Strict));
    book.record_invariant_violation(InvariantKind::SlabRemoveVacant, "strict_test");
}
