use crate::{
    config::{Permission, Principal},
    worker::State as App,
};
use axum::{
    extract::{DefaultBodyLimit, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use me_financial::{OrderSpec, Side};
use me_ledger::{Amend, Amount, Funding, OrderState};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;
#[derive(Clone)]
pub struct Caller {
    pub identity: String,
    pub principal: Principal,
}
struct ApiError(StatusCode, String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}
impl From<me_ledger::Error> for ApiError {
    fn from(e: me_ledger::Error) -> Self {
        let status = match &e {
            me_ledger::Error::Conflict => StatusCode::CONFLICT,
            me_ledger::Error::InsufficientFunds
            | me_ledger::Error::Invalid(_)
            | me_ledger::Error::Contract(_) => StatusCode::UNPROCESSABLE_ENTITY,
            me_ledger::Error::Database(_) => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::CONFLICT,
        };
        if matches!(&e, me_ledger::Error::Database(_)) {
            tracing::warn!(error=%e,"Ledger operation unavailable");
            Self(
                status,
                "ledger unavailable; query request state before retrying".into(),
            )
        } else {
            Self(status, e.to_string())
        }
    }
}
impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        Self(StatusCode::SERVICE_UNAVAILABLE, e.to_string())
    }
}
type Result<T> = std::result::Result<T, ApiError>;
fn invalid(message: &str) -> ApiError {
    ApiError(StatusCode::UNPROCESSABLE_ENTITY, message.into())
}
async fn authorize(
    state: &App,
    caller: &Caller,
    permission: Permission,
    account: Option<Uuid>,
    admission: bool,
) -> Result<()> {
    if !caller.principal.permits(permission, account) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "account/operation is not authorized".into(),
        ));
    }
    if admission && !state.ready() {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "financial admission is paused".into(),
        ));
    }
    state.verify().await?;
    Ok(())
}
async fn admission(state: &App, known: bool) -> Result<()> {
    if !known && !state.ready() {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "financial admission is paused".into(),
        ));
    }
    Ok(())
}
pub fn router(state: App) -> Router {
    Router::new()
        .route(
            "/health/live",
            get(|| async { Json(json!({"status":"running"})) }),
        )
        .route("/health/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/v1/admin/accounts", post(account))
        .route("/v1/admin/maintenance", post(maintenance))
        .route("/v1/admin/reconcile", post(reconcile))
        .route("/v1/admin/checkpoint", get(checkpoint))
        .route("/v1/accounts/{account}/balances", get(balances))
        .route("/v1/accounts/{account}/orders", post(place))
        .route("/v1/accounts/{account}/orders/{order}", get(order))
        .route("/v1/accounts/{account}/orders/{order}/cancel", post(cancel))
        .route("/v1/accounts/{account}/orders/{order}/amend", post(amend))
        .route("/v1/accounts/{account}/requests/{key}", get(request))
        .route("/v1/accounts/{account}/events", get(events))
        .route("/v1/accounts/{account}/funding", post(fund))
        .route("/v1/accounts/{account}/withdrawals", post(withdraw))
        .route("/v1/accounts/{account}/batch/place", post(batch_place))
        .route("/v1/accounts/{account}/batch/cancel", post(batch_cancel))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(state)
}
async fn ready(State(s): State<App>, Extension(c): Extension<Caller>) -> Result<Response> {
    authorize(&s, &c, Permission::Read, None, false).await?;
    let status = s.ledger.status().await?;
    let ready = s.ready() && !status.maintenance && status.reconciliation_reason.is_none();
    Ok((if ready {StatusCode::OK}else{StatusCode::SERVICE_UNAVAILABLE},Json(json!({"ready":ready,"settlement_lag":s.lag.load(std::sync::atomic::Ordering::Acquire),"ledger":status}))).into_response())
}
async fn metrics(State(s): State<App>, Extension(c): Extension<Caller>) -> Result<String> {
    authorize(&s, &c, Permission::Read, None, false).await?;
    let status = s.ledger.status().await?;
    let mut result = format!("# TYPE me_gateway_ready gauge\nme_gateway_ready {}\n# TYPE me_gateway_outbox_pending gauge\nme_gateway_outbox_pending {}\n# TYPE me_gateway_intents_pending gauge\nme_gateway_intents_pending {}\n# TYPE me_gateway_settlement_lag gauge\nme_gateway_settlement_lag {}\n# TYPE me_gateway_reconciliation_required gauge\nme_gateway_reconciliation_required {}\n",u8::from(s.ready()),status.pending_outbox,status.pending_requests,s.lag.load(std::sync::atomic::Ordering::Acquire),u8::from(status.reconciliation_reason.is_some()));
    use std::sync::atomic::Ordering;
    result.push_str(&format!("me_gateway_oldest_pending_seconds {}\nme_gateway_worker_errors_total {}\nme_matcher_available_disk_bytes {}\nme_matcher_minimum_free_bytes {}\nme_matcher_journal_bytes {}\n",status.oldest_pending_seconds,s.errors.load(Ordering::Relaxed),s.disk_available.load(Ordering::Acquire),s.disk_minimum.load(Ordering::Acquire),s.journal_bytes.load(Ordering::Acquire)));
    for (asset, amount) in s.ledger.reserved_totals().await? {
        result.push_str(&format!(
            "me_gateway_reserved{{asset=\"{asset}\"}} {amount}\n"
        ));
    }
    Ok(result)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Account {
    key: String,
    name: String,
}
async fn account(
    State(s): State<App>,
    Extension(c): Extension<Caller>,
    Json(input): Json<Account>,
) -> Result<Json<Value>> {
    authorize(&s, &c, Permission::Admin, None, false).await?;
    if input.key.is_empty() || input.key.len() > 128 || input.name.len() > 128 {
        return Err(invalid("account key/name"));
    }
    let id = s
        .ledger
        .provision_account(
            &format!("{}:{}", c.identity, input.key),
            &json!({"name":input.name}),
        )
        .await?;
    Ok(Json(json!({"account_id":id})))
}
async fn balances(
    State(s): State<App>,
    Extension(c): Extension<Caller>,
    Path(a): Path<Uuid>,
) -> Result<Json<Value>> {
    authorize(&s, &c, Permission::Read, Some(a), false).await?;
    Ok(Json(json!(s.ledger.balances(a).await?)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalOrder {
    symbol: u16,
    side: Side,
    order_type: String,
    time_in_force: String,
    quantity: String,
    #[serde(default)]
    price: Option<String>,
    #[serde(default)]
    protection_price: Option<String>,
    #[serde(default)]
    stop_price: Option<String>,
    #[serde(default)]
    trail_offset: Option<String>,
    #[serde(default)]
    display_quantity: Option<String>,
    #[serde(default)]
    expire_at: u64,
    #[serde(default)]
    flags: Vec<String>,
}
fn raw(value: &str) -> Result<u64> {
    let amount: Amount = value.parse().map_err(ApiError::from)?;
    if amount.is_negative() || !amount.has_precision(8) {
        return Err(invalid("prices and quantities require nonnegative decimal strings with at most eight fractional places"));
    }
    let units = amount.units();
    let end = units.len().saturating_sub(12);
    if end == 0 {
        Ok(0)
    } else {
        units[..end]
            .parse()
            .map_err(|_| invalid("price/quantity overflow"))
    }
}
fn price(value: &str) -> Result<i64> {
    raw(value)?
        .try_into()
        .map_err(|_| invalid("price overflow"))
}
impl ExternalOrder {
    fn spec(self) -> Result<OrderSpec> {
        let kind = match self.order_type.as_str() {
            "limit" => 0,
            "market" => 1,
            "stop_limit" => 2,
            "stop_market" => 3,
            _ => return Err(invalid("order_type")),
        };
        let tif = match self.time_in_force.as_str() {
            "GTC" => 0,
            "IOC" => 1,
            "FOK" => 2,
            "GTD" => 3,
            _ => return Err(invalid("time_in_force")),
        };
        let p = self.price.as_deref().map(price).transpose()?.unwrap_or(0);
        let protection = if matches!(kind, 1 | 3) {
            price(
                self.protection_price
                    .as_deref()
                    .ok_or_else(|| invalid("market protection_price required"))?,
            )?
        } else {
            p
        };
        let mut flags = 0;
        for flag in self.flags {
            flags |= match flag.as_str() {
                "post_only" => 1,
                "maker_or_cancel" => 4,
                _ => return Err(invalid("unsupported order flag")),
            };
        }
        let result = OrderSpec {
            symbol: self.symbol,
            side: self.side,
            order_type: kind,
            time_in_force: tif,
            price_raw: p,
            protection_price_raw: protection,
            qty_raw: raw(&self.quantity)?,
            stop_price_raw: self
                .stop_price
                .as_deref()
                .map(price)
                .transpose()?
                .unwrap_or(0),
            trail_offset_raw: self
                .trail_offset
                .as_deref()
                .map(price)
                .transpose()?
                .unwrap_or(0),
            display_qty_raw: self
                .display_quantity
                .as_deref()
                .map(raw)
                .transpose()?
                .unwrap_or(0),
            expire_at: self.expire_at,
            flags,
        };
        result.validate().map_err(|e| invalid(&e.to_string()))?;
        Ok(result)
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Place {
    key: String,
    order: ExternalOrder,
}
fn order_json(o: OrderState) -> Result<Value> {
    Ok(
        json!({"order_id":o.id,"account_id":o.account_id,"version":o.version,"status":o.status,"remaining":Amount::base(o.remaining),"reserved":o.held,"symbol":o.spec.symbol,"side":o.spec.side,"quantity":Amount::base(o.spec.qty_raw),"price":if o.spec.price_raw==0 {Amount::zero()}else{Amount::quote(o.spec.price_raw,100_000_000)?},"protection_price":if o.spec.protection_price_raw==0 {Amount::zero()}else{Amount::quote(o.spec.protection_price_raw,100_000_000)?}}),
    )
}
async fn place(
    State(s): State<App>,
    Extension(c): Extension<Caller>,
    Path(a): Path<Uuid>,
    Json(input): Json<Place>,
) -> Result<(StatusCode, Json<Value>)> {
    authorize(&s, &c, Permission::Trade, Some(a), false).await?;
    admission(&s, s.ledger.has_request(a, &input.key).await?).await?;
    let result = s.ledger.place(a, &input.key, &input.order.spec()?).await?;
    Ok((StatusCode::ACCEPTED, Json(order_json(result)?)))
}
async fn order(
    State(s): State<App>,
    Extension(c): Extension<Caller>,
    Path((a, o)): Path<(Uuid, i64)>,
) -> Result<Json<Value>> {
    authorize(&s, &c, Permission::Read, Some(a), false).await?;
    Ok(Json(order_json(s.ledger.order(a, o).await?)?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Key {
    key: String,
}
async fn cancel(
    State(s): State<App>,
    Extension(c): Extension<Caller>,
    Path((a, o)): Path<(Uuid, i64)>,
    Json(input): Json<Key>,
) -> Result<(StatusCode, Json<Value>)> {
    authorize(&s, &c, Permission::Trade, Some(a), false).await?;
    admission(&s, s.ledger.has_request(a, &input.key).await?).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"request_id":s.ledger.cancel(a,&input.key,o).await?})),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    key: String,
    price: Option<String>,
    quantity: Option<String>,
}
async fn amend(
    State(s): State<App>,
    Extension(c): Extension<Caller>,
    Path((a, o)): Path<(Uuid, i64)>,
    Json(input): Json<Change>,
) -> Result<(StatusCode, Json<Value>)> {
    authorize(&s, &c, Permission::Trade, Some(a), false).await?;
    admission(&s, s.ledger.has_request(a, &input.key).await?).await?;
    let change = Amend {
        price_raw: input.price.as_deref().map(price).transpose()?,
        qty_raw: input.quantity.as_deref().map(raw).transpose()?,
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"request_id":s.ledger.amend(a,&input.key,o,change).await?})),
    ))
}
async fn request(
    State(s): State<App>,
    Extension(c): Extension<Caller>,
    Path((a, k)): Path<(Uuid, String)>,
) -> Result<Json<Value>> {
    authorize(&s, &c, Permission::Read, Some(a), false).await?;
    let mut state = s.ledger.request(a, &k).await?;
    state.as_object_mut().unwrap().remove("payload");
    Ok(Json(state))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    #[serde(default)]
    after: u64,
    #[serde(default = "zero_digest")]
    digest: String,
    #[serde(default = "page_size")]
    limit: u16,
}
fn zero_digest() -> String {
    "0".repeat(64)
}
fn page_size() -> u16 {
    32
}
async fn events(
    State(s): State<App>,
    Extension(c): Extension<Caller>,
    Path(a): Path<Uuid>,
    Query(q): Query<Cursor>,
) -> Result<Json<Value>> {
    authorize(&s, &c, Permission::Read, Some(a), false).await?;
    Ok(Json(
        s.ledger
            .account_events(a, q.after, &q.digest, q.limit)
            .await?,
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Transfer {
    asset: String,
    amount: Amount,
    external_reference: String,
}
async fn transfer(
    s: App,
    c: Caller,
    a: Uuid,
    input: Transfer,
    withdrawal: bool,
) -> Result<Json<Value>> {
    authorize(&s, &c, Permission::Funding, Some(a), false).await?;
    admission(
        &s,
        s.ledger
            .has_external_reference(&input.external_reference)
            .await?,
    )
    .await?;
    let f = Funding {
        account_id: a,
        asset: input.asset,
        amount: input.amount,
        external_reference: input.external_reference,
    };
    let id = if withdrawal {
        s.ledger.withdraw(&f).await?
    } else {
        s.ledger.fund(&f).await?
    };
    Ok(Json(json!({"journal_id":id})))
}
async fn fund(
    State(s): State<App>,
    Extension(c): Extension<Caller>,
    Path(a): Path<Uuid>,
    Json(input): Json<Transfer>,
) -> Result<Json<Value>> {
    transfer(s, c, a, input, false).await
}
async fn withdraw(
    State(s): State<App>,
    Extension(c): Extension<Caller>,
    Path(a): Path<Uuid>,
    Json(input): Json<Transfer>,
) -> Result<Json<Value>> {
    transfer(s, c, a, input, true).await
}
async fn batch_place(
    State(s): State<App>,
    Extension(c): Extension<Caller>,
    Path(a): Path<Uuid>,
    Json(input): Json<Vec<Place>>,
) -> Result<(StatusCode, Json<Value>)> {
    authorize(&s, &c, Permission::Trade, Some(a), false).await?;
    if input.is_empty() || input.len() > 20 {
        return Err(invalid("batch place size must be 1..20"));
    }
    let mut results = Vec::new();
    for item in input {
        let allowed = admission(&s, s.ledger.has_request(a, &item.key).await?).await;
        let result = match allowed.and_then(|()| item.order.spec()) {
            Ok(spec) => s
                .ledger
                .place(a, &item.key, &spec)
                .await
                .map_err(ApiError::from)
                .and_then(order_json),
            Err(e) => Err(e),
        };
        results.push(match result {
            Ok(value) => json!({"key":item.key,"result":value}),
            Err(e) => json!({"key":item.key,"error":e.1}),
        });
    }
    Ok((StatusCode::ACCEPTED, Json(json!({"results":results}))))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cancel {
    key: String,
    order_id: i64,
}
async fn batch_cancel(
    State(s): State<App>,
    Extension(c): Extension<Caller>,
    Path(a): Path<Uuid>,
    Json(input): Json<Vec<Cancel>>,
) -> Result<(StatusCode, Json<Value>)> {
    authorize(&s, &c, Permission::Trade, Some(a), false).await?;
    if input.is_empty() || input.len() > 50 {
        return Err(invalid("batch cancel size must be 1..50"));
    }
    let mut results = Vec::new();
    for item in input {
        let result = match admission(&s, s.ledger.has_request(a, &item.key).await?).await {
            Ok(()) => s
                .ledger
                .cancel(a, &item.key, item.order_id)
                .await
                .map_err(ApiError::from),
            Err(e) => Err(e),
        };
        results.push(match result {
            Ok(id) => json!({"key":item.key,"request_id":id}),
            Err(e) => json!({"key":item.key,"error":e.1}),
        });
    }
    Ok((StatusCode::ACCEPTED, Json(json!({"results":results}))))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Maintenance {
    enabled: bool,
}
async fn maintenance(
    State(s): State<App>,
    Extension(c): Extension<Caller>,
    Json(input): Json<Maintenance>,
) -> Result<Json<Value>> {
    authorize(&s, &c, Permission::Admin, None, false).await?;
    Ok(Json(
        json!({"watermark":s.maintenance(input.enabled).await?}),
    ))
}
async fn reconcile(State(s): State<App>, Extension(c): Extension<Caller>) -> Result<Json<Value>> {
    authorize(&s, &c, Permission::Admin, None, false).await?;
    let identity = s.identity().await?;
    if !identity.maintenance {
        return Err(invalid(
            "matcher must be behind its durable maintenance barrier",
        ));
    }
    let w = &identity.watermarks[0];
    s.ledger
        .clear_reconciliation(identity.engine_id, w.sequence, &w.digest)
        .await?;
    Ok(Json(json!({"status":"verified","admission":"paused"})))
}

async fn checkpoint(State(s): State<App>, Extension(c): Extension<Caller>) -> Result<Json<Value>> {
    authorize(&s, &c, Permission::Admin, None, false).await?;
    let _guard = s.maintenance_lock.lock().await;
    let mut identity = s.identity().await?;
    identity.storage = None;
    let status = s.ledger.status().await?;
    let cursor = s.ledger.cursor(identity.engine_id, 0).await?;
    let watermark = &identity.watermarks[0];
    if !identity.maintenance
        || !status.maintenance
        || status.reconciliation_reason.is_some()
        || status.pending_outbox != 0
        || status.pending_requests != 0
        || cursor != (watermark.sequence, watermark.digest.clone())
    {
        return Err(invalid(
            "checkpoint requires a drained, reconciled maintenance barrier",
        ));
    }
    s.ledger.audit().await?;
    Ok(Json(
        json!({"identity":identity,"ledger":status,"watermark":watermark}),
    ))
}
