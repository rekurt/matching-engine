use std::sync::Arc;

use axum::{extract::State, routing::{get, post}, Router as AxumRouter};
use clap::Parser;
use me_core::symbol::SymbolConfig;
use me_core::types::{FixedPrice, FixedQty, SymbolId};
use me_engine::router::ShardedRouterV2;
use me_io::tcp_server::TcpServer;
use me_io::uds_server::UdsServer;
use me_metrics::EngineMetrics;
use prometheus::Registry;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Matching engine server.
#[derive(Parser, Debug)]
#[command(name = "me-server", about = "High-performance matching engine server")]
struct Cli {
    /// TCP listen address.
    #[arg(long, default_value = "0.0.0.0:9090")]
    tcp_addr: String,

    /// Unix domain socket path (optional).
    #[arg(long)]
    uds_path: Option<String>,

    /// Number of engine shards (defaults to available parallelism).
    #[arg(long)]
    shards: Option<usize>,

    /// Metrics HTTP listen address.
    #[arg(long, default_value = "0.0.0.0:9091")]
    metrics_addr: String,
}

fn default_symbols() -> Vec<SymbolConfig> {
    vec![
        SymbolConfig {
            id: SymbolId(0),
            base: "BTC".to_string(),
            quote: "USDT".to_string(),
            price_scale: 2,
            qty_scale: 8,
            min_qty: FixedQty::from_raw(1_000),
            max_qty: FixedQty::from_raw(1_000_000_000_000),
            qty_step: FixedQty::from_raw(1_000),
            price_step: FixedPrice::from_raw(1_000_000),
        },
        SymbolConfig {
            id: SymbolId(1),
            base: "ETH".to_string(),
            quote: "USDT".to_string(),
            price_scale: 2,
            qty_scale: 8,
            min_qty: FixedQty::from_raw(10_000),
            max_qty: FixedQty::from_raw(1_000_000_000_000),
            qty_step: FixedQty::from_raw(10_000),
            price_step: FixedPrice::from_raw(100_000),
        },
        SymbolConfig {
            id: SymbolId(2),
            base: "SOL".to_string(),
            quote: "USDT".to_string(),
            price_scale: 4,
            qty_scale: 8,
            min_qty: FixedQty::from_raw(100_000),
            max_qty: FixedQty::from_raw(1_000_000_000_000),
            qty_step: FixedQty::from_raw(100_000),
            price_step: FixedPrice::from_raw(10_000),
        },
    ]
}

#[derive(Clone)]
struct AppState {
    metrics: Arc<EngineMetrics>,
    router: Arc<ShardedRouterV2>,
}

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
    let symbol = SymbolId(symbol_id);
    let success = state.router.halt_symbol(symbol);
    if success {
        info!(symbol_id, "Admin: trading HALTED");
    } else {
        warn!(symbol_id, "Admin: halt failed — symbol not found");
    }
    let status = if success {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::NOT_FOUND
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
    let symbol = SymbolId(symbol_id);
    let success = state.router.resume_symbol(symbol);
    if success {
        info!(symbol_id, "Admin: trading RESUMED");
    } else {
        warn!(symbol_id, "Admin: resume failed — symbol not found");
    }
    let status = if success {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::NOT_FOUND
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

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("me_server=info".parse().unwrap()),
        )
        .with_target(true)
        .init();

    let cli = Cli::parse();

    let num_shards = cli
        .shards
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));

    info!(shards = num_shards, "Starting matching engine");

    let registry = Registry::new();
    let metrics = Arc::new(EngineMetrics::new(&registry));

    let mut router = ShardedRouterV2::new(num_shards);

    for sym in default_symbols() {
        info!(base = sym.base, quote = sym.quote, id = sym.id.0, "Registering symbol");
        router.add_symbol(sym);
    }

    metrics.set_active_symbols(default_symbols().len() as i64);

    let router = Arc::new(router);

    // Graceful shutdown channel
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Handle SIGTERM/SIGINT for graceful shutdown.
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
        warn!("Received shutdown signal");
        let _ = shutdown_tx.send(true);
    });

    // Spawn HTTP metrics + admin server
    let app_state = AppState {
        metrics: Arc::clone(&metrics),
        router: Arc::clone(&router),
    };
    let metrics_addr = cli.metrics_addr.clone();
    tokio::spawn(async move {
        let app = AxumRouter::new()
            .route("/metrics", get(metrics_handler))
            .route("/health", get(health_handler))
            .route("/admin/status", get(status_handler))
            .route("/admin/halt/{symbol_id}", post(halt_handler))
            .route("/admin/resume/{symbol_id}", post(resume_handler))
            .with_state(app_state);
        let listener = tokio::net::TcpListener::bind(&metrics_addr)
            .await
            .unwrap();
        info!(addr = %metrics_addr, "Metrics HTTP server listening");
        axum::serve(listener, app).await.unwrap();
    });

    // Spawn optional UDS listener
    if let Some(ref uds_path) = cli.uds_path {
        let uds_server =
            UdsServer::new_with_metrics(Arc::clone(&router), Some(Arc::clone(&metrics)));
        let path = uds_path.clone();
        let uds_shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            info!(path = %path, "UDS listening");
            if let Err(e) = uds_server.listen(&path, uds_shutdown).await {
                warn!(error = %e, "UDS server error");
            }
        });
    }

    // TCP listener (blocks)
    let tcp_server =
        TcpServer::new_with_metrics(Arc::clone(&router), Some(Arc::clone(&metrics)));
    info!(addr = %cli.tcp_addr, "TCP listening");
    if let Err(e) = tcp_server.listen(&cli.tcp_addr, shutdown_rx).await {
        warn!(error = %e, "TCP server error");
        std::process::exit(1);
    }
}
