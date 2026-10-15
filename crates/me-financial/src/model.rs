use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Internal funded order contract. Public HTTP APIs convert decimal strings to these units.
/// Prices and quantities use 10^8 fixed-point units; expiry is Unix nanoseconds.
pub struct OrderSpec {
    pub symbol: u16,
    pub side: Side,
    /// 0=limit, 1=market, 2=stop-limit, 3=stop-market; a nonzero trail offset makes a stop trailing.
    pub order_type: u8,
    /// 0=GTC, 1=IOC, 2=FOK, 3=GTD; engine validation enforces legal combinations.
    pub time_in_force: u8,
    pub price_raw: i64,
    /// Required positive buy ceiling or sell floor for market and stop-market orders.
    pub protection_price_raw: i64,
    pub qty_raw: u64,
    #[serde(default)]
    pub stop_price_raw: i64,
    #[serde(default)]
    pub trail_offset_raw: i64,
    #[serde(default)]
    pub display_qty_raw: u64,
    #[serde(default)]
    pub expire_at: u64,
    #[serde(default)]
    /// Bit 0=post-only, bit 2=maker-or-cancel. Reduce-only and unknown bits are unsupported.
    pub flags: u8,
}
impl OrderSpec {
    pub fn validate(&self) -> Result<()> {
        if self.qty_raw == 0
            || self.order_type > 3
            || self.time_in_force > 3
            || self.protection_price_raw < 0
            || self.flags & !5 != 0
            || self.price_raw < 0
            || self.stop_price_raw < 0
            || self.trail_offset_raw < 0
            || self.display_qty_raw > self.qty_raw
        {
            return Err(Error::Invalid("invalid order parameters"));
        }
        if matches!(self.order_type, 1 | 3) && self.protection_price_raw == 0 {
            return Err(Error::Invalid("market protection required"));
        }
        if matches!(self.order_type, 0 | 2) && self.price_raw == 0 {
            return Err(Error::Invalid("limit price required"));
        }
        if self.order_type < 2 && (self.stop_price_raw != 0 || self.trail_offset_raw != 0) {
            return Err(Error::Invalid("stop fields on non-stop order"));
        }
        Ok(())
    }
    pub fn reserve_price(&self) -> i64 {
        if matches!(self.order_type, 0 | 2) {
            self.price_raw
        } else {
            self.protection_price_raw
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fees {
    pub id: Uuid,
    #[serde(default)]
    pub maker_bps: u16,
    #[serde(default)]
    pub taker_bps: u16,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fill {
    pub maker_order: i64,
    pub taker_order: i64,
    pub maker_version: i64,
    pub taker_version: i64,
    pub price_raw: i64,
    pub qty_raw: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrderOutcome {
    pub order_id: i64,
    pub version: i64,
    pub remaining: u64,
    pub status: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandOutcome {
    pub request_id: Uuid,
    pub accepted: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrderBinding {
    pub order_id: i64,
    pub version: i64,
    pub account_id: Uuid,
    pub reservation_id: Uuid,
    pub fee_schedule: Uuid,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Decision {
    pub engine_id: Uuid,
    pub shard: u32,
    pub sequence: u64,
    pub previous_digest: String,
    #[serde(default)]
    pub commands: Vec<CommandOutcome>,
    pub fills: Vec<Fill>,
    pub orders: Vec<OrderOutcome>,
    pub bindings: Vec<OrderBinding>,
}
impl Decision {
    pub fn digest(&self) -> Result<String> {
        fingerprint(self)
    }
}
pub fn fingerprint(value: &impl Serialize) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Place,
    Cancel,
    Amend,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinancialCommand {
    pub request_id: Uuid,
    pub operation: Operation,
    pub order_id: i64,
    pub account_id: Uuid,
    pub version: i64,
    #[serde(default)]
    pub expected_version: i64,
    pub reservation_id: Uuid,
    pub fee_schedule: Uuid,
    pub spec: OrderSpec,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EngineIdentity {
    pub storage_version: u32,
    pub engine_id: Uuid,
    pub definition_hash: String,
    pub capabilities: Vec<String>,
    pub watermarks: Vec<Watermark>,
    pub maintenance: bool,
    #[serde(default)]
    pub storage: Option<StorageStatus>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StorageStatus {
    pub available_bytes: u64,
    pub minimum_free_bytes: u64,
    pub journal_bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Watermark {
    pub shard: u32,
    pub sequence: u64,
    pub digest: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecisionPage {
    pub watermark: Watermark,
    pub decisions: Vec<Decision>,
}
pub const STORAGE_VERSION: u32 = 1;
pub const REQUIRED_CAPABILITIES: &[&str] = &[
    "financial_v1",
    "durable_decisions_v1",
    "protected_market_v1",
    "maintenance_barrier_v1",
];
