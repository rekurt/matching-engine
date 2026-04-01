use std::sync::Arc;

use clap::Parser;
use me_core::symbol::SymbolConfig;
use me_core::types::{FixedPrice, FixedQty, SymbolId};
use me_engine::router::ShardedRouterV2;
use me_io::tcp_server::TcpServer;
use me_io::uds_server::UdsServer;

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

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let num_shards = cli
        .shards
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));

    eprintln!("Starting matching engine with {num_shards} shards");

    let mut router = ShardedRouterV2::new(num_shards);

    for sym in default_symbols() {
        eprintln!("  Registering {}/{} (id={})", sym.base, sym.quote, sym.id.0);
        router.add_symbol(sym);
    }

    let router = Arc::new(router);

    // Spawn optional UDS listener
    if let Some(ref uds_path) = cli.uds_path {
        let uds_server = UdsServer::new(Arc::clone(&router));
        let path = uds_path.clone();
        tokio::spawn(async move {
            eprintln!("UDS listening on {path}");
            if let Err(e) = uds_server.listen(&path).await {
                eprintln!("UDS server error: {e}");
            }
        });
    }

    // TCP listener (blocks)
    let tcp_server = TcpServer::new(Arc::clone(&router));
    eprintln!("TCP listening on {}", cli.tcp_addr);
    if let Err(e) = tcp_server.listen(&cli.tcp_addr).await {
        eprintln!("TCP server error: {e}");
        std::process::exit(1);
    }
}
