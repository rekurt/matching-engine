use crate::{Amount, Result};
pub use me_financial::{
    fingerprint, CommandOutcome, Decision, Fees, Fill, OrderBinding, OrderOutcome, OrderSpec, Side,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Quote reserve includes the larger maker/taker fee; sell reserve covers the full base quantity.
pub trait ReserveRequirement {
    fn reserve(&self, qty: u64, fees: &Fees) -> Result<Amount>;
}
impl ReserveRequirement for OrderSpec {
    fn reserve(&self, qty: u64, fees: &Fees) -> Result<Amount> {
        match self.side {
            Side::Buy => Amount::purchase_reserve(
                self.reserve_price(),
                qty,
                fees.maker_bps.max(fees.taker_bps),
            ),
            Side::Sell => Ok(Amount::base(qty)),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Funding {
    pub account_id: Uuid,
    pub asset: String,
    pub amount: Amount,
    pub external_reference: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrderState {
    pub id: i64,
    pub account_id: Uuid,
    pub version: i64,
    pub spec: OrderSpec,
    pub remaining: u64,
    pub held: Amount,
    pub status: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalletBalance {
    pub asset: String,
    pub available: Amount,
    pub reserved: Amount,
}
pub(crate) fn wallet(account: Option<Uuid>, asset: &str, bucket: &str) -> String {
    format!(
        "{}:{asset}:{bucket}",
        account.map_or_else(|| "system".to_owned(), |id| id.to_string())
    )
}
