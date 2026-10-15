use num_bigint::BigInt;
use num_traits::{Signed, Zero};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::{fmt, str::FromStr};

use crate::{Error, Result};

/// Integer accounting units, 10^-20 of an asset. Every operation is checked
/// against PostgreSQL NUMERIC(78,0), including intermediate products.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Amount(BigInt);

impl Amount {
    pub const SCALE: u32 = 20;
    pub fn zero() -> Self {
        Self(BigInt::zero())
    }
    pub fn from_units(value: BigInt) -> Result<Self> {
        if value.abs().to_str_radix(10).len() > 78 {
            return Err(Error::Overflow);
        }
        Ok(Self(value))
    }
    pub fn units(&self) -> String {
        self.0.to_string()
    }
    pub fn is_zero(&self) -> bool {
        self.0.is_zero()
    }
    pub fn is_negative(&self) -> bool {
        self.0.is_negative()
    }
    pub fn checked_add(&self, rhs: &Self) -> Result<Self> {
        Self::from_units(&self.0 + &rhs.0)
    }
    pub fn checked_sub(&self, rhs: &Self) -> Result<Self> {
        Self::from_units(&self.0 - &rhs.0)
    }
    pub fn negated(&self) -> Self {
        Self(-&self.0)
    }
    pub fn base(qty_raw: u64) -> Self {
        Self(BigInt::from(qty_raw) * BigInt::from(10u64).pow(12))
    }
    pub fn quote(price_raw: i64, qty_raw: u64) -> Result<Self> {
        if price_raw <= 0 {
            return Err(Error::Invalid("price must be positive"));
        }
        Self::from_units(BigInt::from(price_raw) * BigInt::from(qty_raw) * 10_000u32)
    }
    pub fn fee(price_raw: i64, qty_raw: u64, bps: u16) -> Result<Self> {
        if price_raw <= 0 || bps > 10_000 {
            return Err(Error::Invalid("invalid price or fee rate"));
        }
        Self::from_units(BigInt::from(price_raw) * BigInt::from(qty_raw) * u32::from(bps))
    }
    pub fn purchase_reserve(price_raw: i64, qty_raw: u64, bps: u16) -> Result<Self> {
        Self::quote(price_raw, qty_raw)?.checked_add(&Self::fee(price_raw, qty_raw, bps)?)
    }
    pub fn has_precision(&self, decimals: u8) -> bool {
        decimals <= 20 && (&self.0 % BigInt::from(10u32).pow(20 - u32::from(decimals))).is_zero()
    }
}

impl FromStr for Amount {
    type Err = Error;
    fn from_str(text: &str) -> Result<Self> {
        if text.is_empty() || text.len() > 100 || text.starts_with('+') {
            return Err(Error::Invalid("invalid amount"));
        }
        let (negative, digits) = text.strip_prefix('-').map_or((false, text), |s| (true, s));
        let mut parts = digits.split('.');
        let whole = parts.next().unwrap_or("");
        let fraction = parts.next().unwrap_or("");
        if parts.next().is_some()
            || whole.is_empty()
            || !whole.bytes().all(|b| b.is_ascii_digit())
            || fraction.len() > 20
            || !fraction.bytes().all(|b| b.is_ascii_digit())
            || (digits.ends_with('.') && fraction.is_empty())
        {
            return Err(Error::Invalid("invalid decimal amount"));
        }
        let raw = format!("{whole}{fraction}{}", "0".repeat(20 - fraction.len()));
        let mut number = BigInt::from_str(&raw).map_err(|_| Error::Invalid("invalid amount"))?;
        if negative {
            number = -number;
        }
        Self::from_units(number)
    }
}
impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits = self.0.abs().to_str_radix(10);
        let padded = format!("{:0>21}", digits);
        let (whole, fraction) = padded.split_at(padded.len() - 20);
        let fraction = fraction.trim_end_matches('0');
        if self.is_negative() {
            write!(f, "-")?;
        }
        write!(f, "{whole}")?;
        if !fraction.is_empty() {
            write!(f, ".{fraction}")?;
        }
        Ok(())
    }
}
impl Serialize for Amount {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for Amount {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(de::Error::custom)
    }
}

/// Upper bound over every possible old fill quantity before an amendment.
/// Values of the piecewise-linear cost function need checking only at its ends
/// and at the new remaining-quantity breakpoint.
pub fn amend_buy_reserve(
    old_price: i64,
    old_remaining: u64,
    new_price: i64,
    new_remaining: u64,
    fee_bps: u16,
) -> Result<Amount> {
    let breakpoint = old_remaining.min(new_remaining);
    let old = Amount::purchase_reserve(old_price, old_remaining, fee_bps)?;
    let new = Amount::purchase_reserve(new_price, new_remaining, fee_bps)?;
    let mixed = Amount::purchase_reserve(old_price, breakpoint, fee_bps)?.checked_add(
        &Amount::purchase_reserve(new_price, new_remaining - breakpoint, fee_bps)?,
    )?;
    Ok(old.max(new).max(mixed))
}
