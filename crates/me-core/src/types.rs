use std::fmt;
use std::ops::{Add, AddAssign, Sub, SubAssign};

use serde::{Deserialize, Serialize};

/// Scale factor: 10^8 = 100_000_000.
/// All prices and quantities are stored as integers multiplied by this factor.
/// Example: 1.50000000 BTC is stored as FixedQty(150_000_000).
pub const SCALE: i64 = 100_000_000;
pub const SCALE_U64: u64 = 100_000_000;

// ─── OrderId / TradeId / SequenceId ──────────────────────────────────────────

pub type OrderId = u64;
pub type TradeId = u64;
pub type SequenceId = u64;

// ─── SymbolId ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[repr(transparent)]
pub struct SymbolId(pub u16);

// ─── Side ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[repr(u8)]
pub enum Side {
    Buy = 0,
    Sell = 1,
}

impl Side {
    #[inline]
    pub fn opposite(self) -> Self {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

// ─── FixedPrice ──────────────────────────────────────────────────────────────

/// Fixed-point price with 8 decimal places.
/// Stored as i64 (signed) to allow comparison arithmetic.
/// Range: ±92_233_720_368.54775807
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[repr(transparent)]
pub struct FixedPrice(pub i64);

impl FixedPrice {
    pub const ZERO: Self = Self(0);
    pub const MAX: Self = Self(i64::MAX);

    #[inline]
    pub const fn from_raw(raw: i64) -> Self {
        Self(raw)
    }

    #[inline]
    pub const fn raw(self) -> i64 {
        self.0
    }

    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub const fn is_positive(self) -> bool {
        self.0 > 0
    }

    #[inline]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    #[inline]
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }

    /// Create from a floating point value (for tests/convenience only).
    pub fn from_f64(val: f64) -> Self {
        Self((val * SCALE as f64) as i64)
    }
}

impl Add for FixedPrice {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl Sub for FixedPrice {
    type Output = Self;
    #[inline]
    fn sub(self, rhs: Self) -> Self {
        Self(self.0 - rhs.0)
    }
}

impl fmt::Debug for FixedPrice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.0 / SCALE;
        let frac = (self.0 % SCALE).unsigned_abs();
        write!(f, "FixedPrice({whole}.{frac:08})")
    }
}

impl fmt::Display for FixedPrice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.0 / SCALE;
        let frac = (self.0 % SCALE).unsigned_abs();
        write!(f, "{whole}.{frac:08}")
    }
}

// ─── FixedQty ────────────────────────────────────────────────────────────────

/// Fixed-point quantity with 8 decimal places.
/// Stored as u64 (unsigned) since quantities are never negative.
/// Range: 0 .. 184_467_440_737.09551615
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[repr(transparent)]
pub struct FixedQty(pub u64);

impl FixedQty {
    pub const ZERO: Self = Self(0);
    pub const MAX: Self = Self(u64::MAX);

    #[inline]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    #[inline]
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }

    #[inline]
    pub const fn min(self, other: Self) -> Self {
        if self.0 < other.0 {
            self
        } else {
            other
        }
    }

    /// Create from a floating point value (for tests/convenience only).
    pub fn from_f64(val: f64) -> Self {
        Self((val * SCALE_U64 as f64) as u64)
    }

    /// Check if qty is a multiple of step.
    #[inline]
    pub const fn is_multiple_of(self, step: Self) -> bool {
        if step.0 == 0 {
            return true;
        }
        self.0.is_multiple_of(step.0)
    }
}

impl Add for FixedQty {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl AddAssign for FixedQty {
    #[inline]
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl Sub for FixedQty {
    type Output = Self;
    #[inline]
    fn sub(self, rhs: Self) -> Self {
        Self(self.0 - rhs.0)
    }
}

impl SubAssign for FixedQty {
    #[inline]
    fn sub_assign(&mut self, rhs: Self) {
        self.0 -= rhs.0;
    }
}

impl fmt::Debug for FixedQty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.0 / SCALE_U64;
        let frac = self.0 % SCALE_U64;
        write!(f, "FixedQty({whole}.{frac:08})")
    }
}

impl fmt::Display for FixedQty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.0 / SCALE_U64;
        let frac = self.0 % SCALE_U64;
        write!(f, "{whole}.{frac:08}")
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fixed_price_basic() {
        let p = FixedPrice::from_f64(100.50);
        assert_eq!(p.raw(), 10_050_000_000);
        assert!(p.is_positive());
        assert!(!p.is_zero());
    }

    #[test]
    fn test_fixed_price_zero() {
        assert!(FixedPrice::ZERO.is_zero());
        assert!(!FixedPrice::ZERO.is_positive());
    }

    #[test]
    fn test_fixed_price_ordering() {
        let a = FixedPrice::from_f64(100.0);
        let b = FixedPrice::from_f64(101.0);
        assert!(a < b);
        assert!(b > a);
    }

    #[test]
    fn test_fixed_price_arithmetic() {
        let a = FixedPrice::from_f64(100.0);
        let b = FixedPrice::from_f64(50.0);
        assert_eq!((a + b).raw(), FixedPrice::from_f64(150.0).raw());
        assert_eq!((a - b).raw(), FixedPrice::from_f64(50.0).raw());
    }

    #[test]
    fn test_fixed_qty_basic() {
        let q = FixedQty::from_f64(1.5);
        assert_eq!(q.raw(), 150_000_000);
        assert!(!q.is_zero());
    }

    #[test]
    fn test_fixed_qty_min() {
        let a = FixedQty::from_f64(3.0);
        let b = FixedQty::from_f64(5.0);
        assert_eq!(a.min(b), a);
        assert_eq!(b.min(a), a);
    }

    #[test]
    fn test_fixed_qty_is_multiple_of() {
        let step = FixedQty::from_f64(0.01);
        assert!(FixedQty::from_f64(0.03).is_multiple_of(step));
        assert!(FixedQty::from_f64(1.00).is_multiple_of(step));
        // 0.015 is not a multiple of 0.01
        let qty = FixedQty::from_raw(1_500_000); // 0.015
        assert!(!qty.is_multiple_of(step));
    }

    #[test]
    fn test_fixed_qty_saturating() {
        assert_eq!(
            FixedQty::MAX.saturating_add(FixedQty::from_raw(1)),
            FixedQty::MAX
        );
        assert_eq!(
            FixedQty::ZERO.saturating_sub(FixedQty::from_raw(1)),
            FixedQty::ZERO
        );
    }

    #[test]
    fn test_side_opposite() {
        assert_eq!(Side::Buy.opposite(), Side::Sell);
        assert_eq!(Side::Sell.opposite(), Side::Buy);
    }

    #[test]
    fn test_fixed_price_display() {
        let p = FixedPrice::from_raw(10_050_000_000);
        assert_eq!(format!("{p}"), "100.50000000");
    }

    #[test]
    fn test_fixed_qty_display() {
        let q = FixedQty::from_raw(150_000_000);
        assert_eq!(format!("{q}"), "1.50000000");
    }
}
