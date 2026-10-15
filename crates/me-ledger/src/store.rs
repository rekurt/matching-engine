use crate::{
    model::{fingerprint, wallet},
    *,
};
use num_bigint::BigInt;
use sqlx::row::Row;
use sqlx_postgres::{PgConnection, PgPool, PgPoolOptions};
use std::{collections::BTreeMap, str::FromStr};
use uuid::Uuid;

#[derive(Clone)]
pub struct Ledger {
    pub(crate) pool: PgPool,
}
#[derive(Clone)]
pub(crate) struct Posting {
    account: Option<Uuid>,
    asset: String,
    bucket: &'static str,
    pub(crate) amount: Amount,
}
impl Posting {
    pub(crate) fn new(
        account: Option<Uuid>,
        asset: &str,
        bucket: &'static str,
        amount: Amount,
    ) -> Self {
        Self {
            account,
            asset: asset.into(),
            bucket,
            amount,
        }
    }
    fn key(&self) -> String {
        wallet(self.account, &self.asset, self.bucket)
    }
}
pub(crate) fn units(text: String) -> Result<Amount> {
    Amount::from_units(
        BigInt::from_str(&text)
            .map_err(|_| Error::Reconciliation("invalid stored amount".into()))?,
    )
}
pub(crate) fn retryable(error: &Error) -> bool {
    matches!(error, Error::Database(sqlx::error::Error::Database(e)) if matches!(e.code().as_deref(),Some("40001"|"40P01"|"23505")))
}
impl Ledger {
    pub async fn connect(url: &str) -> Result<Self> {
        Ok(Self {
            pool: PgPoolOptions::new()
                .max_connections(16)
                .connect(url)
                .await?,
        })
    }
    pub async fn migrate(&self) -> Result<()> {
        let exists: bool =
            sqlx::query_scalar::query_scalar("SELECT to_regclass('financial_schema') IS NOT NULL")
                .fetch_one(&self.pool)
                .await?;
        if exists {
            let version: i32 = sqlx::query_scalar::query_scalar(
                "SELECT version FROM financial_schema WHERE singleton=TRUE",
            )
            .fetch_one(&self.pool)
            .await?;
            if version != 1 {
                return Err(Error::Reconciliation(
                    "incompatible financial schema".into(),
                ));
            }
        }
        let migration = sqlx::migrate::Migration::new(
            1,
            "ledger".into(),
            sqlx::migrate::MigrationType::Simple,
            include_str!("../migrations/0001_ledger.sql").into(),
            false,
        );
        let migrator = sqlx::migrate::Migrator {
            migrations: std::borrow::Cow::Owned(vec![migration]),
            ..sqlx::migrate::Migrator::DEFAULT
        };
        migrator.run(&self.pool).await?;
        Ok(())
    }
    pub async fn add_asset(&self, code: &str, precision: u8) -> Result<()> {
        if precision > 20 {
            return Err(Error::Invalid("asset precision"));
        }
        sqlx::query::query(
            "INSERT INTO assets(code,funding_precision) VALUES($1,$2) ON CONFLICT(code) DO NOTHING",
        )
        .bind(code)
        .bind(i16::from(precision))
        .execute(&self.pool)
        .await?;
        let stored: i16 =
            sqlx::query_scalar::query_scalar("SELECT funding_precision FROM assets WHERE code=$1")
                .bind(code)
                .fetch_one(&self.pool)
                .await?;
        if stored != i16::from(precision) {
            return Err(Error::Reconciliation("asset precision changed".into()));
        }
        Ok(())
    }
    pub async fn add_market(
        &self,
        symbol: u16,
        base: &str,
        quote: &str,
        fees: &Fees,
        definition_hash: &str,
    ) -> Result<()> {
        if fees.id.is_nil() || fees.maker_bps > 10000 || fees.taker_bps > 10000 {
            return Err(Error::Invalid("fee schedule identity/rates"));
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query::query("INSERT INTO fee_schedules(id,maker_bps,taker_bps) VALUES($1,$2,$3) ON CONFLICT(id) DO NOTHING").bind(fees.id).bind(i32::from(fees.maker_bps)).bind(i32::from(fees.taker_bps)).execute(&mut *tx).await?;
        let row = sqlx::query::query("SELECT maker_bps,taker_bps FROM fee_schedules WHERE id=$1")
            .bind(fees.id)
            .fetch_one(&mut *tx)
            .await?;
        if row.get::<i32, _>("maker_bps") != i32::from(fees.maker_bps)
            || row.get::<i32, _>("taker_bps") != i32::from(fees.taker_bps)
        {
            return Err(Error::Conflict);
        }
        sqlx::query::query("INSERT INTO markets(symbol,base,quote,fee_schedule,definition_hash) VALUES($1,$2,$3,$4,$5) ON CONFLICT(symbol) DO NOTHING").bind(i32::from(symbol)).bind(base).bind(quote).bind(fees.id).bind(definition_hash).execute(&mut *tx).await?;
        let row = sqlx::query::query(
            "SELECT base,quote,fee_schedule,definition_hash FROM markets WHERE symbol=$1",
        )
        .bind(i32::from(symbol))
        .fetch_one(&mut *tx)
        .await?;
        if row.get::<String, _>("base") != base
            || row.get::<String, _>("quote") != quote
            || row.get::<String, _>("definition_hash") != definition_hash
        {
            return Err(Error::Reconciliation(
                "instrument/fee configuration changed".into(),
            ));
        }
        if row.get::<Uuid, _>("fee_schedule") != fees.id {
            let paused: Option<bool> = sqlx::query_scalar::query_scalar(
                "SELECT maintenance FROM engine_binding WHERE singleton=TRUE FOR UPDATE",
            )
            .fetch_optional(&mut *tx)
            .await?;
            if paused != Some(true) {
                return Err(Error::Invalid("fee changes require maintenance"));
            }
            sqlx::query::query("UPDATE markets SET fee_schedule=$2 WHERE symbol=$1")
                .bind(i32::from(symbol))
                .bind(fees.id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn balances(&self, account: Uuid) -> Result<Vec<WalletBalance>> {
        let rows=sqlx::query::query("SELECT a.code, COALESCE(v.balance,0)::TEXT AS available, COALESCE(r.balance,0)::TEXT AS reserved FROM assets a LEFT JOIN wallets v ON v.asset=a.code AND v.account_id=$1 AND v.bucket='available' LEFT JOIN wallets r ON r.asset=a.code AND r.account_id=$1 AND r.bucket='reserved' ORDER BY a.code").bind(account).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|r| {
                Ok(WalletBalance {
                    asset: r.get("code"),
                    available: units(r.get("available"))?,
                    reserved: units(r.get("reserved"))?,
                })
            })
            .collect()
    }
    pub async fn fund(&self, funding: &Funding) -> Result<Uuid> {
        self.external_transfer(funding, false).await
    }
    pub async fn withdraw(&self, withdrawal: &Funding) -> Result<Uuid> {
        self.external_transfer(withdrawal, true).await
    }
    async fn external_transfer(&self, funding: &Funding, withdrawal: bool) -> Result<Uuid> {
        if funding.amount.is_negative()
            || funding.amount.is_zero()
            || funding.external_reference.is_empty()
            || funding.external_reference.len() > 128
        {
            return Err(Error::Invalid("funding amount/reference"));
        }
        for attempt in 0..8 {
            match self.external_transfer_once(funding, withdrawal).await {
                Err(e) if retryable(&e) && attempt < 7 => {
                    tokio::time::sleep(std::time::Duration::from_millis(2 << attempt)).await
                }
                result => return result,
            }
        }
        unreachable!()
    }
    async fn external_transfer_once(&self, f: &Funding, withdrawal: bool) -> Result<Uuid> {
        let mut tx = self.pool.begin().await?;
        sqlx::query::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *tx)
            .await?;
        let amount = if withdrawal {
            f.amount.negated()
        } else {
            f.amount.clone()
        };
        let postings = vec![
            Posting::new(Some(f.account_id), &f.asset, "available", amount.clone()),
            Posting::new(None, &f.asset, "external", amount.negated()),
        ];
        let key = format!("external:{}", f.external_reference);
        let payload = serde_json::json!({"transfer":f,"withdrawal":withdrawal});
        if let Some(id) = journal_replay(&mut tx, &key, &journal_hash(&payload, &postings)?).await?
        {
            return Ok(id);
        }
        ensure_admission(&mut tx).await?;
        let active: bool =
            sqlx::query_scalar::query_scalar("SELECT active FROM accounts WHERE id=$1 FOR UPDATE")
                .bind(f.account_id)
                .fetch_one(&mut *tx)
                .await?;
        if !active {
            return Err(Error::Invalid("account inactive"));
        }
        let precision: i16 =
            sqlx::query_scalar::query_scalar("SELECT funding_precision FROM assets WHERE code=$1")
                .bind(&f.asset)
                .fetch_one(&mut *tx)
                .await?;
        if !f.amount.has_precision(precision as u8) {
            return Err(Error::Invalid("funding precision"));
        }
        let id = post(
            &mut tx,
            &key,
            if withdrawal { "withdrawal" } else { "funding" },
            &payload,
            postings,
        )
        .await?;
        tx.commit().await?;
        Ok(id)
    }
    pub async fn place(&self, account: Uuid, key: &str, spec: &OrderSpec) -> Result<OrderState> {
        spec.validate()?;
        if key.is_empty() || key.len() > 128 {
            return Err(Error::Invalid("idempotency key"));
        }
        for attempt in 0..8 {
            match self.place_once(account, key, spec).await {
                Err(e) if retryable(&e) && attempt < 7 => {
                    tokio::time::sleep(std::time::Duration::from_millis(2 << attempt)).await
                }
                result => return result,
            }
        }
        unreachable!()
    }
    async fn place_once(&self, account: Uuid, key: &str, spec: &OrderSpec) -> Result<OrderState> {
        let hash = fingerprint(&("place", account, spec))?;
        let mut tx = self.pool.begin().await?;
        sqlx::query::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *tx)
            .await?;
        if let Some(row) = sqlx::query::query(
            "SELECT payload_hash,initial_response FROM requests WHERE account_id=$1 AND idempotency_key=$2",
        )
        .bind(account)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?
        {
            if row.get::<String, _>("payload_hash") != hash {
                return Err(Error::Conflict);
            }
            let response: Option<serde_json::Value> = row.get("initial_response");
            return serde_json::from_value(response.ok_or_else(|| Error::Reconciliation("missing initial response".into()))?).map_err(Error::from);
        }
        ensure_admission(&mut tx).await?;
        let active: bool =
            sqlx::query_scalar::query_scalar("SELECT active FROM accounts WHERE id=$1 FOR UPDATE")
                .bind(account)
                .fetch_one(&mut *tx)
                .await?;
        if !active {
            return Err(Error::Invalid("account inactive"));
        }
        let market=sqlx::query::query("SELECT m.*,f.maker_bps,f.taker_bps FROM markets m JOIN fee_schedules f ON f.id=m.fee_schedule WHERE symbol=$1").bind(i32::from(spec.symbol)).fetch_one(&mut *tx).await?;
        let fees = Fees {
            id: market.get("fee_schedule"),
            maker_bps: market.get::<i32, _>("maker_bps") as u16,
            taker_bps: market.get::<i32, _>("taker_bps") as u16,
        };
        let amount = spec.reserve(spec.qty_raw, &fees)?;
        let asset: String = market.get(if spec.side == Side::Buy {
            "quote"
        } else {
            "base"
        });
        let request = Uuid::new_v4();
        let payload = serde_json::to_value(spec)?;
        sqlx::query::query("INSERT INTO requests(id,account_id,idempotency_key,payload_hash,operation,payload) VALUES($1,$2,$3,$4,'place',$5)").bind(request).bind(account).bind(key).bind(hash).bind(&payload).execute(&mut *tx).await?;
        post(
            &mut tx,
            &format!("reserve:{request}"),
            "reserve",
            &payload,
            vec![
                Posting::new(Some(account), &asset, "available", amount.negated()),
                Posting::new(Some(account), &asset, "reserved", amount.clone()),
            ],
        )
        .await?;
        let row=sqlx::query::query("INSERT INTO orders(account_id,symbol,fee_schedule,spec,remaining,held,pending_request) VALUES($1,$2,$3,$4,$5::TEXT::NUMERIC,$6::TEXT::NUMERIC,$7) RETURNING *,remaining::TEXT AS remaining_units,held::TEXT AS held_units").bind(account).bind(i32::from(spec.symbol)).bind(fees.id).bind(&payload).bind(spec.qty_raw.to_string()).bind(amount.units()).bind(request).fetch_one(&mut *tx).await?;
        let order = order_row(row)?;
        sqlx::query::query("UPDATE requests SET initial_response=$2 WHERE id=$1")
            .bind(request)
            .bind(serde_json::to_value(&order)?)
            .execute(&mut *tx)
            .await?;
        sqlx::query::query(
            "INSERT INTO order_versions(order_id,version,spec,request_id) VALUES($1,1,$2,$3)",
        )
        .bind(order.id)
        .bind(&payload)
        .bind(request)
        .execute(&mut *tx)
        .await?;
        let envelope = serde_json::json!({"operation":"place","order_id":order.id,"account_id":account,"version":1,"reservation_id":request,"fee_schedule":fees.id,"spec":spec});
        sqlx::query::query("INSERT INTO outbox(request_id,payload) VALUES($1,$2)")
            .bind(request)
            .bind(envelope)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(order)
    }
    pub async fn order(&self, account: Uuid, id: i64) -> Result<OrderState> {
        order_row(sqlx::query::query("SELECT *,remaining::TEXT AS remaining_units,held::TEXT AS held_units FROM orders WHERE id=$1 AND account_id=$2").bind(id).bind(account).fetch_one(&self.pool).await?)
    }
}
pub(crate) fn order_row(row: sqlx_postgres::PgRow) -> Result<OrderState> {
    Ok(OrderState {
        id: row.get("id"),
        account_id: row.get("account_id"),
        version: row.get("version"),
        spec: serde_json::from_value(row.get("spec"))?,
        remaining: row
            .get::<String, _>("remaining_units")
            .parse()
            .map_err(|_| Error::Reconciliation("quantity".into()))?,
        held: units(row.get("held_units"))?,
        status: row.get("status"),
    })
}
pub(crate) async fn post(
    conn: &mut PgConnection,
    key: &str,
    kind: &str,
    payload: &serde_json::Value,
    postings: Vec<Posting>,
) -> Result<Uuid> {
    let hash = journal_hash(payload, &postings)?;
    if let Some(id) = journal_replay(conn, key, &hash).await? {
        return Ok(id);
    }
    let postings: Vec<_> = postings
        .into_iter()
        .filter(|p| !p.amount.is_zero())
        .collect();
    if postings.len() < 2 {
        return Err(Error::Invalid("journal requires postings"));
    }
    let mut sums: BTreeMap<&str, Amount> = BTreeMap::new();
    let mut wallets: BTreeMap<String, Posting> = BTreeMap::new();
    for p in &postings {
        let next = sums
            .get(p.asset.as_str())
            .cloned()
            .unwrap_or_default()
            .checked_add(&p.amount)?;
        sums.insert(&p.asset, next);
        wallets.entry(p.key()).or_insert_with(|| p.clone());
    }
    if sums.values().any(|a| !a.is_zero()) {
        return Err(Error::Invalid("unbalanced journal"));
    }
    for (id, p) in &wallets {
        sqlx::query::query("INSERT INTO wallets(id,account_id,asset,bucket) VALUES($1,$2,$3,$4) ON CONFLICT(id) DO NOTHING").bind(id).bind(p.account).bind(&p.asset).bind(p.bucket).execute(&mut *conn).await?;
    }
    for id in wallets.keys() {
        sqlx::query::query("SELECT id FROM wallets WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *conn)
            .await?;
    }
    let id = Uuid::new_v4();
    sqlx::query::query(
        "INSERT INTO journals(id,dedup_key,payload_hash,kind,payload) VALUES($1,$2,$3,$4,$5)",
    )
    .bind(id)
    .bind(key)
    .bind(hash)
    .bind(kind)
    .bind(payload)
    .execute(&mut *conn)
    .await?;
    for (ordinal, p) in postings.iter().enumerate() {
        let result =
            sqlx::query::query("UPDATE wallets SET balance=balance+$2::TEXT::NUMERIC WHERE id=$1")
                .bind(p.key())
                .bind(p.amount.units())
                .execute(&mut *conn)
                .await;
        if let Err(sqlx::error::Error::Database(ref e)) = result {
            if e.code().as_deref() == Some("23514") {
                return Err(Error::InsufficientFunds);
            }
        }
        result?;
        sqlx::query::query("INSERT INTO postings(journal_id,ordinal,wallet_id,amount) VALUES($1,$2,$3,$4::TEXT::NUMERIC)").bind(id).bind(ordinal as i32).bind(p.key()).bind(p.amount.units()).execute(&mut *conn).await?;
    }
    Ok(id)
}

fn journal_hash(payload: &serde_json::Value, postings: &[Posting]) -> Result<String> {
    fingerprint(&(
        payload,
        postings
            .iter()
            .map(|p| (&p.account, &p.asset, &p.bucket, &p.amount))
            .collect::<Vec<_>>(),
    ))
    .map_err(Error::from)
}
async fn journal_replay(conn: &mut PgConnection, key: &str, hash: &str) -> Result<Option<Uuid>> {
    match sqlx::query::query("SELECT id,payload_hash FROM journals WHERE dedup_key=$1")
        .bind(key)
        .fetch_optional(conn)
        .await?
    {
        Some(row) if row.get::<String, _>("payload_hash") == hash => Ok(Some(row.get("id"))),
        Some(_) => Err(Error::Conflict),
        None => Ok(None),
    }
}

pub(crate) async fn ensure_admission(conn: &mut PgConnection) -> Result<()> {
    if let Some(row) = sqlx::query::query("SELECT maintenance,reconciliation_reason FROM engine_binding WHERE singleton=TRUE FOR SHARE").fetch_optional(conn).await? {
        if let Some(reason) = row.get::<Option<String>,_>("reconciliation_reason") {
            return Err(Error::Reconciliation(reason));
        }
        if row.get::<bool,_>("maintenance") { return Err(Error::Invalid("maintenance mode")); }
    }
    Ok(())
}
