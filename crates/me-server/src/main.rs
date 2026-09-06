mod config;

use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    routing::{get, post},
    Router as AxumRouter,
};
use clap::Parser;
use config::{default_config, load_config, Config};
use me_audit::async_logger::{AsyncAuditLogger, AuditFormat};
use me_engine::router::Router;
use me_io::session_store::SessionStore;
use me_io::tcp_server::TcpServer;
use me_io::uds_server::UdsServer;
use me_metrics::EngineMetrics;
use prometheus::Registry;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

/// High-performance matching engine server.
///
/// Высокопроизводительный сервер матчинг-движка.
#[derive(Parser, Debug)]
#[command(name = "me-server", about = "High-performance matching engine server")]
struct Cli {
    /// Path to TOML configuration file.
    /// If not provided, uses built-in defaults (BTC/USDT, ETH/USDT, SOL/USDT).
    #[arg(long, short)]
    config: Option<String>,

    /// TCP listen address (overrides config file).
    #[arg(long)]
    tcp_addr: Option<String>,

    /// Unix domain socket path (overrides config file).
    #[arg(long)]
    uds_path: Option<String>,

    /// Number of engine shards (overrides config file, 0 = auto).
    #[arg(long)]
    shards: Option<usize>,

    /// Metrics HTTP listen address (overrides config file).
    #[arg(long)]
    metrics_addr: Option<String>,
}

#[derive(Clone)]
struct AppState {
    metrics: Arc<EngineMetrics>,
    router: Arc<Router>,
    admin_api_key: Option<String>,
    shutdown: tokio::sync::watch::Receiver<bool>,
    listeners_ready: Vec<Arc<std::sync::atomic::AtomicBool>>,
    sessions: Arc<SessionStore>,
}

// ─── Admin authentication middleware ─────────────────────────────────────────

async fn admin_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Result<axum::response::Response, StatusCode> {
    let expected_key = state
        .admin_api_key
        .as_deref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    match headers.get("x-admin-key").and_then(|v| v.to_str().ok()) {
        Some(key) if constant_time_eq(key.as_bytes(), expected_key.as_bytes()) => {
            Ok(next.run(request).await)
        }
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Constant-time byte comparison to avoid leaking the admin key via response
/// timing. Length is intentionally folded in without early-out so the running
/// time depends only on the longer input, not on the match position.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0
}

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn metrics_handler(State(state): State<AppState>) -> String {
    let mut text = state.metrics.encode();
    if let Some(audit) = state.router.audit() {
        text.push_str(&format!("# TYPE me_audit_dropped_total counter\nme_audit_dropped_total {}\n# TYPE me_audit_disconnected_total counter\nme_audit_disconnected_total {}\n", audit.dropped_count(), audit.disconnected_count()));
    }
    text
}

async fn health_handler(State(state): State<AppState>) -> (StatusCode, String) {
    if *state.shutdown.borrow()
        || state
            .listeners_ready
            .iter()
            .any(|ready| !ready.load(std::sync::atomic::Ordering::Acquire))
        || state.sessions.ensure_healthy().is_err()
    {
        return (StatusCode::SERVICE_UNAVAILABLE, "draining".into());
    }
    match tokio::task::spawn_blocking(move || state.router.health()).await {
        Ok(Ok(())) => (StatusCode::OK, "OK".into()),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "engine unavailable".into()),
    }
}

async fn halt_handler(
    State(state): State<AppState>,
    axum::extract::Path(symbol_id): axum::extract::Path<u16>,
) -> impl axum::response::IntoResponse {
    let symbol = me_core::types::SymbolId(symbol_id);
    let operation = tokio::task::spawn_blocking(move || {
        let symbols = state.router.symbol_states()?;
        if !symbols.iter().any(|(id, _)| *id == symbol) {
            return Ok(None);
        }
        Ok::<_, String>(Some(state.router.halt_symbol(symbol)))
    })
    .await;
    let (success, status) = match operation {
        Ok(Ok(Some(true))) => (true, StatusCode::OK),
        Ok(Ok(None)) => (false, StatusCode::NOT_FOUND),
        _ => (false, StatusCode::SERVICE_UNAVAILABLE),
    };
    if success {
        info!(symbol_id, "Admin: trading HALTED");
    } else {
        warn!(symbol_id, "Admin: halt failed — symbol not found");
    }
    (
        status,
        axum::Json(serde_json::json!({
            "symbol_id": symbol_id,
            "action": "halt",
            "success": success
        })),
    )
}

async fn resume_handler(
    State(state): State<AppState>,
    axum::extract::Path(symbol_id): axum::extract::Path<u16>,
) -> impl axum::response::IntoResponse {
    let symbol = me_core::types::SymbolId(symbol_id);
    let operation = tokio::task::spawn_blocking(move || {
        let symbols = state.router.symbol_states()?;
        if !symbols.iter().any(|(id, _)| *id == symbol) {
            return Ok(None);
        }
        Ok::<_, String>(Some(state.router.resume_symbol(symbol)))
    })
    .await;
    let (success, status) = match operation {
        Ok(Ok(Some(true))) => (true, StatusCode::OK),
        Ok(Ok(None)) => (false, StatusCode::NOT_FOUND),
        _ => (false, StatusCode::SERVICE_UNAVAILABLE),
    };
    if success {
        info!(symbol_id, "Admin: trading RESUMED");
    } else {
        warn!(symbol_id, "Admin: resume failed — symbol not found");
    }
    (
        status,
        axum::Json(serde_json::json!({
            "symbol_id": symbol_id,
            "action": "resume",
            "success": success
        })),
    )
}

async fn status_handler(State(state): State<AppState>) -> impl axum::response::IntoResponse {
    let draining = *state.shutdown.borrow()
        || state
            .listeners_ready
            .iter()
            .any(|ready| !ready.load(std::sync::atomic::Ordering::Acquire))
        || state.sessions.ensure_healthy().is_err();
    let result = tokio::task::spawn_blocking(move || state.router.symbol_states()).await;
    match result {
        Ok(Ok(symbols)) if !draining => (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "status": "ready",
                "symbols": symbols.into_iter().map(|(id, active)| serde_json::json!({"symbol_id": id.0, "trading_enabled": active})).collect::<Vec<_>>()
            })),
        ),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(
                serde_json::json!({"status": if draining {"draining"} else {"unavailable"}}),
            ),
        ),
    }
}

// ─── Main ────────────────────────────────────────────────────────────────────

fn resolve_config(cli: &Cli) -> Config {
    let mut cfg = match &cli.config {
        Some(path) => match load_config(std::path::Path::new(path)) {
            Ok(c) => {
                info!(path, "Loaded configuration file");
                c
            }
            Err(e) => {
                error!(path, error = %e, "Failed to load config file");
                std::process::exit(1);
            }
        },
        None => {
            info!("No config file specified, using built-in defaults");
            default_config()
        }
    };

    // CLI overrides
    if let Some(ref addr) = cli.tcp_addr {
        cfg.server.tcp_addr = addr.clone();
    }
    if let Some(ref addr) = cli.metrics_addr {
        cfg.server.metrics_addr = addr.clone();
    }
    if let Some(shards) = cli.shards {
        cfg.server.shards = shards;
    }
    if cli.uds_path.is_some() {
        cfg.server.uds_path = cli.uds_path.clone();
    }

    if let Err(e) = cfg.validate() {
        error!(error = %e, "Invalid configuration");
        std::process::exit(1);
    }
    cfg
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(true)
        .init();

    let cli = Cli::parse();
    let cfg = resolve_config(&cli);
    #[cfg(feature = "tls")]
    let tls_acceptor = match (&cfg.server.tls.cert_path, &cfg.server.tls.key_path) {
        (Some(cert), Some(key)) => {
            let config = load_tls_config(cert, key, cfg.server.tls.client_ca_path.as_deref())
                .unwrap_or_else(|e| {
                    error!(error = %e, "Cannot initialize TLS");
                    std::process::exit(1)
                });
            Some(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
        }
        _ => None,
    };

    let num_shards = if cfg.server.shards == 0 {
        std::thread::available_parallelism().map_or(4, |n| n.get())
    } else {
        cfg.server.shards
    };

    // Create high-performance clock from config
    let clock = me_clock::create_clock(&cfg.server.clock);
    info!(clock = %cfg.server.clock, shards = num_shards, "Starting matching engine");

    let registry = Registry::new();
    let metrics = Arc::new(EngineMetrics::new(&registry));

    // Create async audit logger (zero-alloc hot path, serializes on separate thread)
    let audit = Arc::new(AsyncAuditLogger::new(65536, AuditFormat::Json));

    // Build router with optional WAL
    let wal_config = if cfg.wal.enabled {
        use me_engine::router::{WalConfig as EngineWalConfig, WalMode};
        let mode = match cfg.wal.mode.as_str() {
            "post" => WalMode::Post,
            "disabled" => WalMode::Disabled,
            _ => WalMode::Pre,
        };
        let snapshot_dir = if cfg.wal.snapshot_dir.is_empty() {
            format!("{}/snapshots", cfg.wal.path)
        } else {
            cfg.wal.snapshot_dir.clone()
        };
        info!(
            mode = %cfg.wal.mode,
            path = %cfg.wal.path,
            snapshot_dir = %snapshot_dir,
            snapshot_interval = cfg.wal.snapshot_interval,
            "WAL enabled (crash recovery active)"
        );
        Some(EngineWalConfig {
            mode,
            path: cfg.wal.path.clone(),
            sync_interval: cfg.wal.sync_interval,
            snapshot_dir: Some(snapshot_dir),
            snapshot_interval: cfg.wal.snapshot_interval,
        })
    } else {
        warn!("WAL disabled: volatile test mode, restart loses orders");
        None
    };

    // Thread affinity config
    let affinity = me_engine::router::AffinityConfig {
        pin_threads: cfg.server.pin_threads,
        realtime_priority: cfg.server.realtime_priority,
    };
    if cfg.server.pin_threads {
        info!("Thread pinning enabled (NUMA-aware)");
    }

    let mut router = Router::new_with_affinity(
        num_shards,
        Some(Arc::clone(&metrics)),
        Some(Arc::clone(&audit)),
        wal_config,
        affinity,
    );

    if cfg.symbols.is_empty() {
        error!("No symbols configured");
        std::process::exit(1);
    }

    let mut seen_ids = std::collections::HashSet::new();
    for sym_entry in &cfg.symbols {
        if !seen_ids.insert(sym_entry.id) {
            error!(
                id = sym_entry.id,
                "Duplicate symbol id in configuration — ids must be unique"
            );
            std::process::exit(1);
        }
        match sym_entry.to_symbol_config() {
            Ok(sym) => {
                let risk_config = match sym_entry.to_risk_config() {
                    Ok(rc) => rc,
                    Err(e) => {
                        error!(error = %e, "Invalid risk configuration");
                        std::process::exit(1);
                    }
                };
                info!(
                    base = sym.base,
                    quote = sym.quote,
                    id = sym.id.0,
                    risk_enabled = risk_config.is_some(),
                    "Registering symbol"
                );
                router.add_symbol_with_risk(sym, risk_config);
            }
            Err(e) => {
                error!(error = %e, "Invalid symbol configuration");
                std::process::exit(1);
            }
        }
    }

    metrics.set_active_symbols(cfg.symbols.len() as i64);

    if let Err(e) = router.health() {
        error!(error = %e, "Engine recovery failed");
        router.shutdown();
        std::process::exit(1);
    }
    let router = Arc::new(router);

    // Admin API key (None = mutating admin endpoints disabled)
    let admin_api_key = if cfg.server.admin.api_key.is_empty() {
        warn!("Admin API key not set — mutating admin endpoints are DISABLED");
        None
    } else {
        info!("Admin API key configured — admin endpoints require X-Admin-Key header");
        Some(cfg.server.admin.api_key.clone())
    };

    // Graceful shutdown channel
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Handle SIGTERM/SIGINT for graceful shutdown.
    let drain_timeout = cfg.server.shutdown.drain_timeout_secs;
    let signal_shutdown = shutdown_tx.clone();
    let signal_task = tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm =
                signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
            tokio::select! {
                _ = ctrl_c => {}
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            if let Err(e) = ctrl_c.await {
                tracing::error!(
                    error = ?e,
                    "ctrl-c signal registration failed — graceful shutdown may not work"
                );
            }
        }
        warn!(
            drain_timeout_secs = drain_timeout,
            "Received shutdown signal, draining connections"
        );
        if let Err(e) = signal_shutdown.send(true) {
            tracing::warn!(error = ?e, "shutdown_tx.send failed: all receivers dropped");
        }
    });

    // Shared session store (TCP and UDS share the same store so reconnects across
    // transports are also supported).
    let session_path = std::path::Path::new(&cfg.wal.path).join("sessions.wal");
    let session_file_existed = session_path.exists();
    let recovered = router.recovered_requests().unwrap_or_else(|e| {
        error!(error = %e, "Cannot recover request ledger");
        std::process::exit(1)
    });
    if cfg.wal.enabled && !session_file_existed && !recovered.is_empty() {
        error!("Session journal missing for existing requests; restore a consistent backup");
        std::process::exit(1);
    }
    let session_store = if cfg.wal.enabled {
        SessionStore::open(
            &session_path,
            cfg.server.session.replay_buffer_size,
            std::time::Duration::from_secs(cfg.server.session.ttl_secs),
        )
        .unwrap_or_else(|e| {
            error!(error = %e, "Cannot recover sessions");
            std::process::exit(1)
        })
    } else {
        SessionStore::new(
            cfg.server.session.replay_buffer_size,
            std::time::Duration::from_secs(cfg.server.session.ttl_secs),
        )
    };
    if let Err(e) = session_store
        .reconcile(recovered)
        .and_then(|()| session_store.recover_batches(&router))
    {
        error!(error = %e, "Session/request recovery failed");
        std::process::exit(1);
    }
    if let Err(e) = session_store.checkpoint() {
        error!(error = %e, "Cannot initialize session journal");
        std::process::exit(1);
    }
    let session_store = Arc::new(session_store);
    let tcp_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let uds_ready = Arc::new(std::sync::atomic::AtomicBool::new(
        cfg.server.uds_path.is_none(),
    ));

    // Spawn HTTP metrics + admin server
    let app_state = AppState {
        metrics: Arc::clone(&metrics),
        router: Arc::clone(&router),
        admin_api_key,
        shutdown: shutdown_rx.clone(),
        listeners_ready: vec![Arc::clone(&tcp_ready), Arc::clone(&uds_ready)],
        sessions: Arc::clone(&session_store),
    };

    let metrics_addr = cfg.server.metrics_addr.clone();
    let app_state_clone = app_state;
    let listener = match tokio::net::TcpListener::bind(&metrics_addr).await {
        Ok(listener) => listener,
        Err(e) => {
            error!(error = %e, "Cannot bind metrics/admin listener");
            std::process::exit(1);
        }
    };
    let mut http_shutdown = shutdown_rx.clone();
    let http_shutdown_tx = shutdown_tx.clone();
    let mut http_task = tokio::spawn(async move {
        // Public admin status route
        let mut app = AxumRouter::new()
            .route("/admin/status", get(status_handler))
            .with_state(app_state_clone.clone());

        // Mutating admin routes are only exposed when an API key is configured.
        if app_state_clone.admin_api_key.is_some() {
            let protected_admin = AxumRouter::new()
                .route("/admin/halt/{symbol_id}", post(halt_handler))
                .route("/admin/resume/{symbol_id}", post(resume_handler))
                .route_layer(middleware::from_fn_with_state(
                    app_state_clone.clone(),
                    admin_auth,
                ))
                .with_state(app_state_clone.clone());
            app = app.merge(protected_admin);
        } else {
            warn!("Admin halt/resume routes are not registered because no admin API key is set");
        }

        // Public routes (no auth)
        let public_routes = AxumRouter::new()
            .route("/metrics", get(metrics_handler))
            .route("/health", get(health_handler))
            .route("/ready", get(health_handler))
            .with_state(app_state_clone);

        let app = public_routes.merge(app);

        info!(addr = %metrics_addr, "Metrics HTTP server listening");
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                while !*http_shutdown.borrow() {
                    if http_shutdown.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
        if result.is_err() {
            let _ = http_shutdown_tx.send(true);
        }
        result
    });

    let hb_interval_ms = cfg.server.heartbeat.interval_ms as u32;
    let hb_timeout_ms = cfg.server.heartbeat.timeout_ms;

    // Background GC task: evict expired sessions every 30 s.
    let gc_task = {
        let gc_store = Arc::clone(&session_store);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                gc_store.gc();
            }
        })
    };

    // Spawn optional UDS listener
    let uds_task = if let Some(ref uds_path) = cfg.server.uds_path {
        let uds_server = UdsServer::new_with_config(
            Arc::clone(&router),
            Some(Arc::clone(&metrics)),
            Arc::clone(&session_store),
            hb_interval_ms,
            hb_timeout_ms,
            Arc::clone(&clock),
        );
        let uds_server = uds_server
            .with_drain_timeout(std::time::Duration::from_secs(drain_timeout))
            .with_readiness(uds_ready);
        let path = uds_path.clone();
        let uds_shutdown = shutdown_rx.clone();
        let uds_shutdown_tx = shutdown_tx.clone();
        Some(tokio::spawn(async move {
            let result = uds_server.listen(&path, uds_shutdown).await;
            if result.is_err() {
                let _ = uds_shutdown_tx.send(true);
            }
            result
        }))
    } else {
        None
    };

    // TCP listener (blocks until shutdown)
    #[allow(unused_mut)]
    let mut tcp_server = TcpServer::new_with_config(
        Arc::clone(&router),
        Some(Arc::clone(&metrics)),
        Arc::clone(&session_store),
        hb_interval_ms,
        hb_timeout_ms,
        Arc::clone(&clock),
    );

    tcp_server = tcp_server
        .with_drain_timeout(std::time::Duration::from_secs(drain_timeout))
        .with_readiness(tcp_ready);

    #[cfg(feature = "tls")]
    if let Some(acceptor) = tls_acceptor {
        tcp_server = tcp_server.with_tls(acceptor);
    }

    info!(addr = %cfg.server.tcp_addr, "TCP listening");
    let tcp_result = tcp_server.listen(&cfg.server.tcp_addr, shutdown_rx).await;
    let _ = shutdown_tx.send(true);
    signal_task.abort();
    gc_task.abort();
    drop(tcp_server);
    let mut failed = tcp_result.is_err();
    if let Err(e) = tcp_result {
        error!(error = %e, "TCP listener failed");
    }
    if let Some(task) = uds_task {
        failed |= !matches!(task.await, Ok(Ok(())));
    }
    match tokio::time::timeout(
        std::time::Duration::from_secs(drain_timeout),
        &mut http_task,
    )
    .await
    {
        Ok(Ok(Ok(()))) => {}
        Ok(error) => {
            failed = true;
            error!(?error, "HTTP shutdown failed");
        }
        Err(_) => {
            http_task.abort();
            let _ = http_task.await;
            failed = true;
            error!("HTTP shutdown timed out");
        }
    }
    if let Err(e) = session_store.checkpoint() {
        error!(error = %e, "Session checkpoint failed");
        failed = true;
    }
    match Arc::try_unwrap(router) {
        Ok(router) => router.shutdown(),
        Err(_) => {
            error!("Engine still has active request owners after drain");
            failed = true;
        }
    }
    if failed {
        std::process::exit(1);
    }
}

// ─── TLS helpers (compiled only with `tls` feature) ──────────────────────────

#[cfg(feature = "tls")]
fn load_tls_config(
    cert_path: &str,
    key_path: &str,
    client_ca_path: Option<&str>,
) -> Result<tokio_rustls::rustls::ServerConfig, String> {
    use tokio_rustls::rustls;
    use tokio_rustls::rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
    use tokio_rustls::rustls::server::WebPkiClientVerifier;
    use tokio_rustls::rustls::RootCertStore;

    let cert_chain = CertificateDer::pem_file_iter(cert_path)
        .map_err(|e| format!("cert: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("cert parse: {e}"))?;
    let key = PrivateKeyDer::from_pem_file(key_path).map_err(|e| format!("key parse: {e}"))?;

    let builder = if let Some(ca_path) = client_ca_path {
        // mTLS: require client certificate
        let ca_certs = CertificateDer::pem_file_iter(ca_path)
            .map_err(|e| format!("client CA: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("CA parse: {e}"))?;
        let mut root_store = RootCertStore::empty();
        for cert in ca_certs {
            root_store.add(cert).map_err(|e| format!("CA add: {e}"))?;
        }
        let verifier = WebPkiClientVerifier::builder(Arc::new(root_store))
            .build()
            .map_err(|e| format!("verifier: {e}"))?;
        rustls::ServerConfig::builder().with_client_cert_verifier(verifier)
    } else {
        rustls::ServerConfig::builder().with_no_client_auth()
    };

    builder
        .with_single_cert(cert_chain, key)
        .map_err(|e| format!("TLS config: {e}"))
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn constant_time_eq_matches() {
        assert!(constant_time_eq(b"secret-key", b"secret-key"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn constant_time_eq_rejects() {
        assert!(!constant_time_eq(
            b"secret-key",
            &[b"secret-key".as_slice(), &[0; 256]].concat()
        ));
        assert!(!constant_time_eq(b"secret-key", b"secret-keyX")); // length differs
        assert!(!constant_time_eq(b"secret-key", b"Secret-key")); // first byte differs
        assert!(!constant_time_eq(b"secret-key", b"secret-keX")); // last byte differs
        assert!(!constant_time_eq(b"abc", b""));
    }
}
