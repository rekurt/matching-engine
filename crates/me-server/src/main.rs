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
use me_engine::router::ShardedRouterV2;
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
    router: Arc<ShardedRouterV2>,
    admin_api_key: Option<String>,
}

// ─── Admin authentication middleware ─────────────────────────────────────────

async fn admin_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Result<axum::response::Response, StatusCode> {
    if let Some(ref expected_key) = state.admin_api_key {
        match headers.get("x-admin-key").and_then(|v| v.to_str().ok()) {
            Some(key) if key == expected_key => {}
            _ => return Err(StatusCode::UNAUTHORIZED),
        }
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

    info!(shards = num_shards, "Starting matching engine");

    let registry = Registry::new();
    let metrics = Arc::new(EngineMetrics::new(&registry));

    let mut router = ShardedRouterV2::new(num_shards);

    if cfg.symbols.is_empty() {
        error!("No symbols configured");
        std::process::exit(1);
    }

    for sym_entry in &cfg.symbols {
        match sym_entry.to_symbol_config() {
            Ok(sym) => {
                info!(base = sym.base, quote = sym.quote, id = sym.id.0, "Registering symbol");
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

    // Admin API key (None = no auth required)
    let admin_api_key = if cfg.server.admin.api_key.is_empty() {
        warn!("Admin API key not set — admin endpoints are UNPROTECTED");
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
            ctrl_c.await.ok();
        }
        warn!(drain_timeout_secs = drain_timeout, "Received shutdown signal, draining connections");
        let _ = shutdown_tx.send(true);
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

        let listener = tokio::net::TcpListener::bind(&metrics_addr)
            .await
            .unwrap();
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
    let tcp_server = TcpServer::new_with_config(
        Arc::clone(&router),
        Some(Arc::clone(&metrics)),
        Arc::clone(&session_store),
        hb_interval_ms,
        hb_timeout_ms,
    );
    info!(addr = %cfg.server.tcp_addr, "TCP listening");
    if let Err(e) = tcp_server.listen(&cfg.server.tcp_addr, shutdown_rx).await {
        warn!(error = %e, "TCP server error");
        std::process::exit(1);
    }
}
