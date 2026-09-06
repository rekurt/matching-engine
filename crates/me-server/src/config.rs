use me_core::symbol::SymbolConfig;
use me_core::types::{FixedPrice, FixedQty, SymbolId};
use me_risk::config::RiskConfig;
use serde::Deserialize;
use std::path::Path;

/// Top-level configuration loaded from TOML.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub symbols: Vec<SymbolEntry>,
    #[serde(default)]
    pub wal: WalConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_tcp_addr")]
    pub tcp_addr: String,
    #[serde(default)]
    pub allow_plaintext_remote: bool,
    #[serde(default = "default_metrics_addr")]
    pub metrics_addr: String,
    #[serde(default)]
    pub shards: usize,
    pub uds_path: Option<String>,
    /// Clock implementation: "system", "coarse" (default), "rdtsc"
    #[serde(default = "default_clock")]
    pub clock: String,
    /// Pin shard worker threads to CPU cores (NUMA awareness, Linux-only).
    #[serde(default)]
    pub pin_threads: bool,
    /// Set real-time scheduling priority for shard workers (requires root, Linux-only).
    #[serde(default)]
    pub realtime_priority: bool,
    #[serde(default)]
    pub admin: AdminConfig,
    #[serde(default)]
    pub shutdown: ShutdownConfig,
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,
    #[serde(default)]
    pub session: SessionConfig,
    #[serde(default)]
    #[cfg_attr(not(feature = "tls"), allow(dead_code))]
    pub tls: TlsConfig,
}

/// Heartbeat / keepalive configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatConfig {
    /// Interval (ms) the server advertises to clients via TLV_HEARTBEAT_MS.
    #[serde(default = "default_heartbeat_interval_ms")]
    pub interval_ms: u64,
    /// Server-side read timeout (ms). Connection dropped if no data received
    /// within this window (should be > interval_ms to allow for jitter).
    #[serde(default = "default_heartbeat_timeout_ms")]
    pub timeout_ms: u64,
}

/// Session recovery configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    /// Number of outbound frames buffered per session for replay after reconnect.
    #[serde(default = "default_replay_buffer_size")]
    pub replay_buffer_size: usize,
    /// Seconds an unused idle session is retained; durable activity is preserved.
    #[serde(default = "default_session_ttl_secs")]
    pub ttl_secs: u64,
}

/// Optional TLS configuration. If cert_path/key_path are absent, TLS is disabled.
/// Fields are consumed by `main.rs` only when built with `--features tls`.
#[derive(Debug, Deserialize, Default)]
#[cfg_attr(not(feature = "tls"), allow(dead_code))]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert_path: Option<String>,
    pub key_path: Option<String>,
    /// If set, enables mutual TLS and requires a client certificate signed by this CA.
    pub client_ca_path: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
    #[serde(default)]
    pub api_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShutdownConfig {
    #[serde(default = "default_drain_timeout")]
    pub drain_timeout_secs: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolEntry {
    pub id: u16,
    pub base: String,
    pub quote: String,
    #[serde(default = "default_price_scale")]
    pub price_scale: u8,
    #[serde(default = "default_qty_scale")]
    pub qty_scale: u8,
    #[serde(default = "default_min_qty")]
    pub min_qty: String,
    #[serde(default = "default_max_qty")]
    pub max_qty: String,
    #[serde(default = "default_qty_step")]
    pub qty_step: String,
    #[serde(default = "default_price_step")]
    pub price_step: String,
    /// Invariant-violation policy for this symbol. One of
    /// `"strict"`, `"halt_symbol"` (default), or `"continue"`.
    /// The server defaults to `halt_symbol` so an internal invariant violation
    /// isolates the affected symbol instead of panicking the whole process.
    /// See `me_core::invariant::InvariantPolicy` for semantics.
    #[serde(default = "default_invariant_policy")]
    pub invariant_policy: me_core::invariant::InvariantPolicy,
    /// Optional pre-trade risk controls for this symbol. When present, the
    /// engine enforces fat-finger qty, max notional, price band, rate limit,
    /// max open orders, trading-enabled, and self-trade prevention. When
    /// absent, NO risk checks run for the symbol (back-compat).
    #[serde(default)]
    pub risk: Option<RiskSettings>,
}

/// Per-symbol pre-trade risk controls (deserialized from `[[symbols.risk]]`).
/// Decimal fields are strings (parsed to fixed-point) to avoid float rounding,
/// consistent with the symbol qty/price fields.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct RiskSettings {
    /// Fat-finger guard: maximum single-order quantity.
    #[serde(default = "default_risk_max_order_qty")]
    pub max_order_qty: String,
    /// Maximum single-order notional (price * qty) in quote currency.
    #[serde(default = "default_risk_max_order_notional")]
    pub max_order_notional: String,
    /// Price band: maximum deviation from reference price, in basis points
    /// (1 = 0.01%). 0 disables the band.
    #[serde(default = "default_risk_price_band_bps")]
    pub price_band_bps: u32,
    #[serde(default = "default_reference_price")]
    pub reference_price: String,
    /// Maximum orders per second (token-bucket rate limit). 0 disables.
    #[serde(default = "default_risk_max_orders_per_second")]
    pub max_orders_per_second: u32,
    /// Maximum simultaneously open orders for this symbol. 0 disables.
    #[serde(default = "default_risk_max_open_orders")]
    pub max_open_orders: u32,
    /// Whether trading is enabled for this symbol.
    #[serde(default = "default_true")]
    pub trading_enabled: bool,
    /// Reject (cancel-newest) any order that would match a resting order from
    /// the same client session.
    #[serde(default)]
    pub self_trade_prevention: bool,
}

/// WAL (Write-Ahead Log) configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WalConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// WAL mode: "pre" (before matching), "post" (after matching)
    #[serde(default = "default_wal_mode")]
    pub mode: String,
    /// Path to WAL directory
    #[serde(default = "default_wal_path")]
    pub path: String,
    /// Flush WAL to disk every N commands (0 = every command)
    #[serde(default = "default_wal_sync_interval")]
    pub sync_interval: u64,
    /// Directory for periodic book snapshots. Empty string = derive from
    /// `{path}/snapshots`. Snapshots checkpoint book state; the decision journal is retained.
    #[serde(default)]
    pub snapshot_dir: String,
    /// Take a snapshot of all symbols on a shard every N processed commands,
    /// retaining that shard's WAL. 0 disables periodic snapshots (recovery
    /// then replays the entire WAL).
    #[serde(default = "default_snapshot_interval")]
    pub snapshot_interval: u64,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: default_wal_mode(),
            path: default_wal_path(),
            sync_interval: default_wal_sync_interval(),
            snapshot_dir: String::new(),
            snapshot_interval: default_snapshot_interval(),
        }
    }
}

fn default_wal_mode() -> String {
    "pre".to_string()
}
fn default_wal_path() -> String {
    "./data/wal".to_string()
}
fn default_wal_sync_interval() -> u64 {
    100
}
fn default_snapshot_interval() -> u64 {
    100_000
}

fn default_invariant_policy() -> me_core::invariant::InvariantPolicy {
    // Production default: isolate the affected symbol rather than panic.
    me_core::invariant::InvariantPolicy::HaltSymbol
}

fn default_true() -> bool {
    true
}
fn default_risk_max_order_qty() -> String {
    "10000.0".to_string()
}
fn default_risk_max_order_notional() -> String {
    "100000000.0".to_string() // $100M
}
fn default_reference_price() -> String {
    "0".into()
}

fn default_risk_price_band_bps() -> u32 {
    1000 // ±10%
}
fn default_risk_max_orders_per_second() -> u32 {
    1000
}
fn default_risk_max_open_orders() -> u32 {
    100_000
}

fn default_clock() -> String {
    "coarse".to_string()
}
fn default_tcp_addr() -> String {
    "127.0.0.1:9090".to_string()
}
fn default_metrics_addr() -> String {
    "127.0.0.1:9091".to_string()
}
fn default_drain_timeout() -> u64 {
    30
}
fn default_heartbeat_interval_ms() -> u64 {
    5_000
}
fn default_heartbeat_timeout_ms() -> u64 {
    15_000
}
fn default_replay_buffer_size() -> usize {
    1_000
}
fn default_session_ttl_secs() -> u64 {
    60
}
fn default_price_scale() -> u8 {
    2
}
fn default_qty_scale() -> u8 {
    8
}
fn default_min_qty() -> String {
    "0.00001".to_string()
}
fn default_max_qty() -> String {
    "10000.0".to_string()
}
fn default_qty_step() -> String {
    "0.00001".to_string()
}
fn default_price_step() -> String {
    "0.01".to_string()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            tcp_addr: default_tcp_addr(),
            allow_plaintext_remote: false,
            metrics_addr: default_metrics_addr(),
            shards: 0,
            uds_path: None,
            clock: default_clock(),
            pin_threads: false,
            realtime_priority: false,
            admin: AdminConfig::default(),
            shutdown: ShutdownConfig::default(),
            heartbeat: HeartbeatConfig::default(),
            session: SessionConfig::default(),
            tls: TlsConfig::default(),
        }
    }
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            interval_ms: default_heartbeat_interval_ms(),
            timeout_ms: default_heartbeat_timeout_ms(),
        }
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            replay_buffer_size: default_replay_buffer_size(),
            ttl_secs: default_session_ttl_secs(),
        }
    }
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            drain_timeout_secs: default_drain_timeout(),
        }
    }
}

const FIXED_SCALE: u64 = 100_000_000; // 10^8

fn parse_decimal_to_raw(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return Err("expected an unsigned decimal".into());
    }
    let oob = || "value out of range for fixed-point u64".to_string();
    if let Some(dot_pos) = s.find('.') {
        let int_part: u64 = s[..dot_pos]
            .parse()
            .map_err(|e| format!("invalid integer part: {e}"))?;
        let frac_str = &s[dot_pos + 1..];
        let frac_digits = frac_str.len();
        if frac_digits > 8 {
            return Err(format!("too many decimal places: {frac_digits} (max 8)"));
        }
        let frac_part: u64 = if frac_str.is_empty() {
            0
        } else {
            frac_str
                .parse()
                .map_err(|e| format!("invalid fractional part: {e}"))?
        };
        let scale_factor = 10u64.pow(8 - frac_digits as u32);
        // Checked arithmetic: a large integer part must not silently wrap.
        int_part
            .checked_mul(FIXED_SCALE)
            .zip(frac_part.checked_mul(scale_factor))
            .and_then(|(hi, lo)| hi.checked_add(lo))
            .ok_or_else(oob)
    } else {
        let int_part: u64 = s.parse().map_err(|e| format!("invalid number: {e}"))?;
        int_part.checked_mul(FIXED_SCALE).ok_or_else(oob)
    }
}

impl SymbolEntry {
    pub fn to_symbol_config(&self) -> Result<SymbolConfig, String> {
        let id = self.id;
        if self.price_scale > 8 {
            return Err(format!("symbol {id}: price_scale {} > 8", self.price_scale));
        }
        if self.qty_scale > 8 {
            return Err(format!("symbol {id}: qty_scale {} > 8", self.qty_scale));
        }
        let min_qty = FixedQty::from_raw(
            parse_decimal_to_raw(&self.min_qty)
                .map_err(|e| format!("symbol {id}: min_qty: {e}"))?,
        );
        let max_qty = FixedQty::from_raw(
            parse_decimal_to_raw(&self.max_qty)
                .map_err(|e| format!("symbol {id}: max_qty: {e}"))?,
        );
        let qty_step = FixedQty::from_raw(
            parse_decimal_to_raw(&self.qty_step)
                .map_err(|e| format!("symbol {id}: qty_step: {e}"))?,
        );
        let price_step = FixedPrice::from_raw(
            parse_decimal_to_raw(&self.price_step)
                .map_err(|e| format!("symbol {id}: price_step: {e}"))? as i64,
        );
        if min_qty.raw() > max_qty.raw() {
            return Err(format!("symbol {id}: min_qty > max_qty"));
        }
        if qty_step.is_zero() {
            return Err(format!("symbol {id}: qty_step must be > 0"));
        }
        if price_step.raw() <= 0 {
            return Err(format!("symbol {id}: price_step must be > 0"));
        }
        Ok(SymbolConfig {
            id: SymbolId(self.id),
            base: self.base.clone(),
            quote: self.quote.clone(),
            price_scale: self.price_scale,
            qty_scale: self.qty_scale,
            min_qty,
            max_qty,
            qty_step,
            price_step,
            invariant_policy: self.invariant_policy,
        })
    }

    /// Build the per-symbol [`RiskConfig`], or `None` when no `risk` table is
    /// configured for this symbol. Decimal fields are parsed via the same
    /// fixed-point parser as qty/price to avoid float rounding.
    pub fn to_risk_config(&self) -> Result<Option<RiskConfig>, String> {
        let Some(r) = &self.risk else {
            return Ok(None);
        };
        let id = self.id;
        let max_order_qty = FixedQty::from_raw(
            parse_decimal_to_raw(&r.max_order_qty)
                .map_err(|e| format!("symbol {id}: risk.max_order_qty: {e}"))?,
        );
        let max_order_notional = parse_decimal_to_raw(&r.max_order_notional)
            .map_err(|e| format!("symbol {id}: risk.max_order_notional: {e}"))?;
        Ok(Some(RiskConfig {
            symbol: SymbolId(id),
            max_order_qty,
            max_order_notional,
            // RESERVED until an account/position model exists. Keep unlimited.
            max_position: FixedQty::ZERO,
            price_band_bps: r.price_band_bps,
            reference_price: FixedPrice::from_raw(
                i64::try_from(parse_decimal_to_raw(&r.reference_price)?)
                    .map_err(|_| "reference price exceeds i64")?,
            ),
            max_orders_per_second: r.max_orders_per_second,
            max_open_orders: r.max_open_orders,
            trading_enabled: r.trading_enabled,
            self_trade_prevention: r.self_trade_prevention,
        }))
    }
}

/// Permissive-but-real default risk controls applied to the built-in symbols
/// when the server runs without a config file. Enabling risk by default closes
/// the "risk engine never activates in production" gap.
fn default_risk_settings() -> RiskSettings {
    RiskSettings {
        max_order_qty: default_risk_max_order_qty(),
        max_order_notional: default_risk_max_order_notional(),
        price_band_bps: default_risk_price_band_bps(),
        reference_price: default_reference_price(),
        max_orders_per_second: default_risk_max_orders_per_second(),
        max_open_orders: default_risk_max_open_orders(),
        trading_enabled: true,
        self_trade_prevention: false,
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        let tcp: std::net::SocketAddr = self
            .server
            .tcp_addr
            .parse()
            .map_err(|_| "invalid tcp_addr")?;
        let _: std::net::SocketAddr = self
            .server
            .metrics_addr
            .parse()
            .map_err(|_| "invalid metrics_addr")?;
        let tls = &self.server.tls;
        if tls.cert_path.is_some() != tls.key_path.is_some()
            || (tls.client_ca_path.is_some() && tls.cert_path.is_none())
        {
            return Err("TLS requires both cert_path and key_path".into());
        }
        if !cfg!(feature = "tls") && tls.cert_path.is_some() {
            return Err("TLS configured but binary was built without --features tls".into());
        }
        if !tcp.ip().is_loopback() && tls.cert_path.is_none() && !self.server.allow_plaintext_remote
        {
            return Err("remote plaintext TCP requires explicit server.allow_plaintext_remote=true behind a trusted gateway".into());
        }
        if self.server.shards > 1024 {
            return Err("server.shards must be <= 1024".into());
        }
        if !matches!(self.server.clock.as_str(), "system" | "coarse" | "rdtsc") {
            return Err("unsupported server.clock".into());
        }
        let hb = &self.server.heartbeat;
        if hb.interval_ms == 0
            || hb.interval_ms > u32::MAX as u64
            || hb.timeout_ms <= hb.interval_ms
        {
            return Err("heartbeat timeout must exceed a positive u32 interval".into());
        }
        if self.server.shutdown.drain_timeout_secs == 0
            || self.server.shutdown.drain_timeout_secs > 3600
        {
            return Err("drain_timeout_secs must be in 1..=3600".into());
        }
        if self.server.session.replay_buffer_size == 0
            || self.server.session.replay_buffer_size > 1_000_000
            || self.server.session.ttl_secs == 0
        {
            return Err("invalid session replay capacity or TTL".into());
        }
        if !self.server.admin.api_key.is_empty()
            && (self.server.admin.api_key.len() < 16
                || !self
                    .server
                    .admin
                    .api_key
                    .bytes()
                    .all(|b| (33..=126).contains(&b)))
        {
            return Err(
                "admin key must contain at least 16 printable non-space ASCII bytes".into(),
            );
        }
        if !matches!(self.wal.mode.as_str(), "pre" | "post") {
            return Err("wal.mode must be pre or post".into());
        }
        if self.wal.enabled && self.wal.path.trim().is_empty() {
            return Err("wal.path must not be empty".into());
        }
        if self.symbols.is_empty() {
            return Err("at least one symbol is required".into());
        }
        let mut seen = std::collections::HashSet::new();
        for entry in &self.symbols {
            if !seen.insert(entry.id) {
                return Err(format!("duplicate symbol {}", entry.id));
            }
            if entry.base.is_empty() || entry.quote.is_empty() {
                return Err("symbol currencies must not be empty".into());
            }
            entry.to_symbol_config()?;
            entry.to_risk_config()?;
        }
        Ok(())
    }
}

/// Load configuration from a TOML file. Returns default config on any error
/// if `required` is false.
pub fn load_config(path: &Path) -> Result<Config, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let cfg: Config =
        toml::from_str(&content).map_err(|e| format!("parse error in {}: {e}", path.display()))?;
    cfg.validate()?;
    Ok(cfg)
}

/// Build the default config (same as the hardcoded symbols that existed before).
pub fn default_config() -> Config {
    Config {
        server: ServerConfig::default(),
        wal: WalConfig::default(),
        symbols: vec![
            SymbolEntry {
                id: 0,
                base: "BTC".into(),
                quote: "USDT".into(),
                price_scale: 2,
                qty_scale: 8,
                min_qty: "0.00001".into(),
                max_qty: "10000.0".into(),
                qty_step: "0.00001".into(),
                price_step: "0.01".into(),
                invariant_policy: default_invariant_policy(),
                risk: Some(default_risk_settings()),
            },
            SymbolEntry {
                id: 1,
                base: "ETH".into(),
                quote: "USDT".into(),
                price_scale: 2,
                qty_scale: 8,
                min_qty: "0.0001".into(),
                max_qty: "100000.0".into(),
                qty_step: "0.0001".into(),
                price_step: "0.01".into(),
                invariant_policy: default_invariant_policy(),
                risk: Some(default_risk_settings()),
            },
            SymbolEntry {
                id: 2,
                base: "SOL".into(),
                quote: "USDT".into(),
                price_scale: 4,
                qty_scale: 8,
                min_qty: "0.001".into(),
                max_qty: "100000.0".into(),
                qty_step: "0.001".into(),
                price_step: "0.0001".into(),
                invariant_policy: default_invariant_policy(),
                risk: Some(default_risk_settings()),
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_decimal_integer() {
        assert_eq!(parse_decimal_to_raw("100").unwrap(), 10_000_000_000);
    }

    #[test]
    fn parse_decimal_fractional() {
        assert_eq!(parse_decimal_to_raw("0.01").unwrap(), 1_000_000);
        assert_eq!(parse_decimal_to_raw("50000.50").unwrap(), 5_000_050_000_000);
        assert_eq!(parse_decimal_to_raw("0.00001").unwrap(), 1_000);
    }

    #[test]
    fn parse_decimal_max_precision() {
        assert_eq!(parse_decimal_to_raw("0.00000001").unwrap(), 1);
    }

    #[test]
    fn parse_decimal_too_many_digits() {
        assert!(parse_decimal_to_raw("0.000000001").is_err());
    }

    #[test]
    fn parse_decimal_overflow_is_error_not_wrap() {
        // ~1.8e11 * 1e8 overflows u64 — must error, not silently wrap.
        assert!(parse_decimal_to_raw("200000000000").is_err());
    }

    #[test]
    fn invalid_scale_rejected() {
        let mut e = default_config().symbols.remove(0);
        e.price_scale = 9;
        assert!(e.to_symbol_config().is_err());
    }

    #[test]
    fn min_greater_than_max_rejected() {
        let mut e = default_config().symbols.remove(0);
        e.min_qty = "10.0".into();
        e.max_qty = "1.0".into();
        assert!(e.to_symbol_config().is_err());
    }

    #[test]
    fn server_default_invariant_policy_is_halt_symbol() {
        use me_core::invariant::InvariantPolicy;
        let cfg: Config = toml::from_str(
            r#"
[[symbols]]
id = 0
base = "BTC"
quote = "USDT"
"#,
        )
        .unwrap();
        assert_eq!(cfg.symbols[0].invariant_policy, InvariantPolicy::HaltSymbol);
    }

    #[test]
    fn default_config_symbols_convert() {
        let cfg = default_config();
        for sym in &cfg.symbols {
            sym.to_symbol_config().unwrap();
        }
    }

    #[test]
    fn toml_roundtrip() {
        let toml_str = r#"
[server]
tcp_addr = "127.0.0.1:9090"
shards = 4

[server.admin]
api_key = "secret123"

[[symbols]]
id = 0
base = "BTC"
quote = "USDT"
min_qty = "0.001"
max_qty = "1000.0"
qty_step = "0.001"
price_step = "0.01"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.server.tcp_addr, "127.0.0.1:9090");
        assert_eq!(cfg.server.shards, 4);
        assert_eq!(cfg.server.admin.api_key, "secret123");
        assert_eq!(cfg.symbols.len(), 1);
        assert_eq!(cfg.symbols[0].base, "BTC");
        let sc = cfg.symbols[0].to_symbol_config().unwrap();
        assert_eq!(sc.id, SymbolId(0));
    }
}

#[cfg(test)]
mod startup_validation_tests {
    use super::*;

    fn load(text: &str) -> Result<Config, String> {
        let path = std::env::temp_dir().join(format!(
            "me-config-{}-{}.toml",
            std::process::id(),
            me_core::SessionId::random().log_prefix()
        ));
        std::fs::write(&path, text).unwrap();
        let result = load_config(&path);
        std::fs::remove_file(path).unwrap();
        result
    }

    #[test]
    fn invalid_durability_mode_is_rejected_before_startup() {
        assert!(load(
            "[wal]\nenabled=true\nmode='prre'\n[[symbols]]\nid=0\nbase='BTC'\nquote='USD'"
        )
        .is_err());
    }

    #[test]
    fn partial_tls_configuration_is_rejected() {
        assert!(load(
            "[server.tls]\ncert_path='cert.pem'\n[[symbols]]\nid=0\nbase='BTC'\nquote='USD'"
        )
        .is_err());
    }

    #[test]
    fn unsupported_position_control_is_not_silently_ignored() {
        assert!(load(
            "[[symbols]]\nid=0\nbase='BTC'\nquote='USD'\n[symbols.risk]\nmax_position='1'"
        )
        .is_err());
    }

    #[test]
    fn heartbeat_timeout_must_exceed_interval() {
        assert!(load("[server.heartbeat]\ninterval_ms=5000\ntimeout_ms=10\n[[symbols]]\nid=0\nbase='BTC'\nquote='USD'").is_err());
    }

    #[test]
    fn shipped_defaults_preserve_orders_and_limit_network_exposure() {
        let cfg = default_config();
        assert!(cfg.wal.enabled);
        assert!(cfg
            .server
            .tcp_addr
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .ip()
            .is_loopback());
    }

    #[test]
    fn decimal_parser_rejects_embedded_sign() {
        assert!(parse_decimal_to_raw("1.+2").is_err());
    }
}
