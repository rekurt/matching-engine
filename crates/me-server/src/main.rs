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
}

// ─── Admin authentication middleware ─────────────────────────────────────────

async fn admin_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Result<axum::response::Response, StatusCode> {
    let expected_key = match state.admin_api_key.as_ref() {
        Some(key) => key,
        None => return Err(StatusCode::FORBIDDEN),
    };

    match headers.get("x-admin-key").and_then(|v| v.to_str().ok()) {
        Some(key) if key == expected_key => {}
        _ => return Err(StatusCode::UNAUTHORIZED),
    }
    Ok(next.run(request).await)
}

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn metrics_handler(State(state): State<AppState>) -> String {
    state.metrics.encode()
}

async fn health_handler() -> &'static str {
    "OK"
}

async fn halt_handler(
    State(state): State<AppState>,
    axum::extract::Path(symbol_id): axum::extract::Path<u16>,
) -> impl axum::response::IntoResponse {
    let symbol = me_core::types::SymbolId(symbol_id);
    let success = state.router.halt_symbol(symbol);
    if success {
        info!(symbol_id, "Admin: trading HALTED");
    } else {
        warn!(symbol_id, "Admin: halt failed — symbol not found");
    }
    let status = if success {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
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
    let success = state.router.resume_symbol(symbol);
    if success {
        info!(symbol_id, "Admin: trading RESUMED");
    } else {
        warn!(symbol_id, "Admin: resume failed — symbol not found");
    }
    let status = if success {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
    (
        status,
        axum::Json(serde_json::json!({
            "symbol_id": symbol_id,
            "action": "resume",
            "success": success
        })),
    )
}

async fn status_handler() -> impl axum::response::IntoResponse {
    axum::Json(serde_json::json!({
        "status": "running",
        "message": "Matching engine is operational"
    }))
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

    cfg
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("me_server=info".parse().unwrap()),
        )
        .with_target(true)
        .init();

    let cli = Cli::parse();
    let cfg = resolve_config(&cli);

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
        info!(mode = %cfg.wal.mode, path = %cfg.wal.path, "WAL enabled");
        Some(EngineWalConfig {
            mode,
            path: cfg.wal.path.clone(),
            sync_interval: cfg.wal.sync_interval,
        })
    } else {
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

    for sym_entry in &cfg.symbols {
        match sym_entry.to_symbol_config() {
            Ok(sym) => {
                info!(
                    base = sym.base,
                    quote = sym.quote,
                    id = sym.id.0,
                    "Registering symbol"
                );
                router.add_symbol(sym);
            }
            Err(e) => {
                error!(error = %e, "Invalid symbol configuration");
                std::process::exit(1);
            }
        }
    }

    metrics.set_active_symbols(cfg.symbols.len() as i64);

    let router = Arc::new(router);

    // Admin API key (required for admin routes)
    let admin_api_key = if cfg.server.admin.api_key.is_empty() {
        warn!("Admin API key not set — admin endpoints are DISABLED");
        None
    } else {
        info!("Admin API key configured — admin endpoints require X-Admin-Key header");
        Some(cfg.server.admin.api_key.clone())
    };

    // Graceful shutdown channel
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Handle SIGTERM/SIGINT for graceful shutdown.
    let drain_timeout = cfg.server.shutdown.drain_timeout_secs;
    tokio::spawn(async move {
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
        if let Err(e) = shutdown_tx.send(true) {
            tracing::warn!(error = ?e, "shutdown_tx.send failed: all receivers dropped");
        }
    });

    // Spawn HTTP metrics + admin server
    let app_state = AppState {
        metrics: Arc::clone(&metrics),
        router: Arc::clone(&router),
        admin_api_key,
    };

    let metrics_addr = cfg.server.metrics_addr.clone();
    let app_state_clone = app_state.clone();
    tokio::spawn(async move {
        // Admin routes with authentication middleware
        let admin_routes = AxumRouter::new()
            .route("/admin/status", get(status_handler))
            .route("/admin/halt/{symbol_id}", post(halt_handler))
            .route("/admin/resume/{symbol_id}", post(resume_handler))
            .route_layer(middleware::from_fn_with_state(
                app_state_clone.clone(),
                admin_auth,
            ))
            .with_state(app_state_clone.clone());

        // Public routes (no auth)
        let public_routes = AxumRouter::new()
            .route("/metrics", get(metrics_handler))
            .route("/health", get(health_handler))
            .with_state(app_state_clone);

        let app = public_routes.merge(admin_routes);

        let listener = tokio::net::TcpListener::bind(&metrics_addr).await.unwrap();
        info!(addr = %metrics_addr, "Metrics HTTP server listening");
        axum::serve(listener, app).await.unwrap();
    });

    // Shared session store (TCP and UDS share the same store so reconnects across
    // transports are also supported).
    let session_store = Arc::new(SessionStore::new(
        cfg.server.session.replay_buffer_size,
        std::time::Duration::from_secs(cfg.server.session.ttl_secs),
    ));

    let hb_interval_ms = cfg.server.heartbeat.interval_ms as u32;
    let hb_timeout_ms = cfg.server.heartbeat.timeout_ms;

    // Background GC task: evict expired sessions every 30 s.
    {
        let gc_store = Arc::clone(&session_store);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                gc_store.gc();
            }
        });
    }

    // Spawn optional UDS listener
    if let Some(ref uds_path) = cfg.server.uds_path {
        let uds_server = UdsServer::new_with_config(
            Arc::clone(&router),
            Some(Arc::clone(&metrics)),
            Arc::clone(&session_store),
            hb_interval_ms,
            hb_timeout_ms,
            Arc::clone(&clock),
        );
        let path = uds_path.clone();
        let uds_shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            info!(path = %path, "UDS listening");
            if let Err(e) = uds_server.listen(&path, uds_shutdown).await {
                warn!(error = %e, "UDS server error");
            }
        });
    }

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

    #[cfg(feature = "tls")]
    {
        use tokio_rustls::TlsAcceptor;

        if let (Some(cert_path), Some(key_path)) =
            (&cfg.server.tls.cert_path, &cfg.server.tls.key_path)
        {
            match load_tls_config(
                cert_path,
                key_path,
                cfg.server.tls.client_ca_path.as_deref(),
            ) {
                Ok(rustls_cfg) => {
                    let acceptor = TlsAcceptor::from(Arc::new(rustls_cfg));
                    tcp_server = tcp_server.with_tls(acceptor);
                    info!("TLS enabled");
                }
                Err(e) => {
                    error!(error = %e, "Failed to load TLS certificates");
                    std::process::exit(1);
                }
            }
        }
    }

    info!(addr = %cfg.server.tcp_addr, "TCP listening");
    if let Err(e) = tcp_server.listen(&cfg.server.tcp_addr, shutdown_rx).await {
        warn!(error = %e, "TCP server error");
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
    use rustls_pemfile::{certs, private_key};
    use std::fs::File;
    use std::io::BufReader;
    use tokio_rustls::rustls;
    use tokio_rustls::rustls::server::WebPkiClientVerifier;
    use tokio_rustls::rustls::RootCertStore;

    let cert_file = File::open(cert_path).map_err(|e| format!("cert: {e}"))?;
    let cert_chain: Vec<_> = certs(&mut BufReader::new(cert_file))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("cert parse: {e}"))?;

    let key_file = File::open(key_path).map_err(|e| format!("key: {e}"))?;
    let key = private_key(&mut BufReader::new(key_file))
        .map_err(|e| format!("key parse: {e}"))?
        .ok_or_else(|| "no private key found".to_string())?;

    let builder = if let Some(ca_path) = client_ca_path {
        // mTLS: require client certificate
        let ca_file = File::open(ca_path).map_err(|e| format!("client CA: {e}"))?;
        let ca_certs: Vec<_> = certs(&mut BufReader::new(ca_file))
            .collect::<Result<_, _>>()
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
