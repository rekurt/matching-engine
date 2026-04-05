use me_core::symbol::SymbolConfig;
use me_core::types::{FixedPrice, FixedQty, SymbolId};
use serde::Deserialize;
use std::path::Path;

/// Top-level configuration loaded from TOML.
#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub symbols: Vec<SymbolEntry>,
    #[serde(default)]
    pub wal: WalConfig,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_tcp_addr")]
    pub tcp_addr: String,
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
pub struct SessionConfig {
    /// Number of outbound frames buffered per session for replay after reconnect.
    #[serde(default = "default_replay_buffer_size")]
    pub replay_buffer_size: usize,
    /// Seconds an idle session is retained before GC evicts it.
    #[serde(default = "default_session_ttl_secs")]
    pub ttl_secs: u64,
}

/// Optional TLS configuration. If cert_path/key_path are absent, TLS is disabled.
/// Fields are consumed by `main.rs` only when built with `--features tls`.
#[derive(Debug, Deserialize, Default)]
#[cfg_attr(not(feature = "tls"), allow(dead_code))]
pub struct TlsConfig {
    pub cert_path: Option<String>,
    pub key_path: Option<String>,
    /// If set, enables mutual TLS and requires a client certificate signed by this CA.
    pub client_ca_path: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct AdminConfig {
    #[serde(default)]
    pub api_key: String,
}

#[derive(Debug, Deserialize)]
pub struct ShutdownConfig {
    #[serde(default = "default_drain_timeout")]
    pub drain_timeout_secs: u64,
}

#[derive(Debug, Deserialize)]
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
    /// `"strict"` (default), `"halt_symbol"`, or `"continue"`.
    /// See `me_core::invariant::InvariantPolicy` for semantics.
    #[serde(default)]
    pub invariant_policy: me_core::invariant::InvariantPolicy,
}

/// WAL (Write-Ahead Log) configuration.
#[derive(Debug, Deserialize)]
pub struct WalConfig {
    #[serde(default)]
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
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: default_wal_mode(),
            path: default_wal_path(),
            sync_interval: default_wal_sync_interval(),
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

fn default_clock() -> String {
    "coarse".to_string()
}
fn default_tcp_addr() -> String {
    "0.0.0.0:9090".to_string()
}
fn default_metrics_addr() -> String {
    "0.0.0.0:9091".to_string()
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
    if let Some(dot_pos) = s.find('.') {
        let int_part: u64 = s[..dot_pos]
            .parse()
            .map_err(|e| format!("invalid integer part: {e}"))?;
        let frac_str = &s[dot_pos + 1..];
        let frac_digits = frac_str.len();
        if frac_digits > 8 {
            return Err(format!("too many decimal places: {frac_digits} (max 8)"));
        }
        let frac_part: u64 = frac_str
            .parse()
            .map_err(|e| format!("invalid fractional part: {e}"))?;
        let scale_factor = 10u64.pow(8 - frac_digits as u32);
        Ok(int_part * FIXED_SCALE + frac_part * scale_factor)
    } else {
        let int_part: u64 = s.parse().map_err(|e| format!("invalid number: {e}"))?;
        Ok(int_part * FIXED_SCALE)
    }
}

impl SymbolEntry {
    pub fn to_symbol_config(&self) -> Result<SymbolConfig, String> {
        Ok(SymbolConfig {
            id: SymbolId(self.id),
            base: self.base.clone(),
            quote: self.quote.clone(),
            price_scale: self.price_scale,
            qty_scale: self.qty_scale,
            min_qty: FixedQty::from_raw(
                parse_decimal_to_raw(&self.min_qty)
                    .map_err(|e| format!("symbol {}: min_qty: {e}", self.id))?,
            ),
            max_qty: FixedQty::from_raw(
                parse_decimal_to_raw(&self.max_qty)
                    .map_err(|e| format!("symbol {}: max_qty: {e}", self.id))?,
            ),
            qty_step: FixedQty::from_raw(
                parse_decimal_to_raw(&self.qty_step)
                    .map_err(|e| format!("symbol {}: qty_step: {e}", self.id))?,
            ),
            price_step: FixedPrice::from_raw(
                parse_decimal_to_raw(&self.price_step)
                    .map_err(|e| format!("symbol {}: price_step: {e}", self.id))?
                    as i64,
            ),
            invariant_policy: self.invariant_policy,
        })
    }
}

/// Load configuration from a TOML file. Returns default config on any error
/// if `required` is false.
pub fn load_config(path: &Path) -> Result<Config, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    toml::from_str(&content).map_err(|e| format!("parse error in {}: {e}", path.display()))
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
                invariant_policy: me_core::invariant::InvariantPolicy::default(),
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
                invariant_policy: me_core::invariant::InvariantPolicy::default(),
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
                invariant_policy: me_core::invariant::InvariantPolicy::default(),
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
