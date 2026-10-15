use me_ledger::{amend_buy_reserve, Amount};
use proptest::prelude::*;
use std::str::FromStr;

#[test]
fn exact_quote_and_fee_below_eight_decimals() {
    assert_eq!(
        Amount::quote(1, 1).unwrap().to_string(),
        "0.0000000000000001"
    );
    assert_eq!(
        Amount::fee(1, 1, 1).unwrap().to_string(),
        "0.00000000000000000001"
    );
    assert_eq!(
        Amount::purchase_reserve(1, 1, 1).unwrap().to_string(),
        "0.00000000000000010001"
    );
    assert!(!Amount::fee(1, 1, 1).unwrap().has_precision(8));
}
#[test]
fn malformed_and_numeric_json_are_rejected() {
    for invalid in [
        "",
        "+1",
        "1e2",
        "1.",
        ".1",
        "1.000000000000000000001",
        "NaN",
        " 1",
        "-",
        "1..0",
    ] {
        assert!(Amount::from_str(invalid).is_err(), "{invalid}");
    }
    assert!(serde_json::from_str::<Amount>("1.5").is_err());
    assert!(Amount::from_str(&"9".repeat(59)).is_err());
}
#[test]
fn amend_must_cover_mixed_old_and_new_execution() {
    assert_eq!(
        amend_buy_reserve(200_00000000, 10_00000000, 150_00000000, 20_00000000, 0)
            .unwrap()
            .to_string(),
        "3500"
    );
    assert_eq!(
        amend_buy_reserve(200_00000000, 10_00000000, 150_00000000, 20_00000000, 25)
            .unwrap()
            .to_string(),
        "3508.75"
    );
}
proptest! {
    #[test]
    fn decimal_roundtrip(raw in any::<i128>()) {
        let value=Amount::from_units(raw.into()).unwrap();
        prop_assert_eq!(value.to_string().parse::<Amount>().unwrap(),value);
    }
    #[test]
    fn fees_and_notional_conserve_exact_units(p in 1i64..i64::MAX,q in any::<u64>(),bps in 0u16..=10000) {
        let total=Amount::purchase_reserve(p,q,bps).unwrap();
        prop_assert_eq!(total.checked_sub(&Amount::fee(p,q,bps).unwrap()).unwrap(),Amount::quote(p,q).unwrap());
    }
    #[test]
    fn amend_bound_dominates_every_possible_old_fill(old_price in 1i64..1000,new_price in 1i64..1000,old_qty in 0u64..40,new_qty in 0u64..40,bps in 0u16..=10000) {
        let bound=amend_buy_reserve(old_price,old_qty,new_price,new_qty,bps).unwrap();
        for filled in 0..=old_qty {
            let possible=Amount::purchase_reserve(old_price,filled,bps).unwrap().checked_add(&Amount::purchase_reserve(new_price,new_qty.saturating_sub(filled),bps).unwrap()).unwrap();
            prop_assert!(possible<=bound);
        }
    }
}
