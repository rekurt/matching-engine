use std::time::Instant;

use clap::Parser;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use me_core::events::MatchEvent;
use me_core::order::{Order, TimeInForce};
use me_core::symbol::SymbolConfig;
use me_core::types::{FixedPrice, FixedQty, OrderId, Side, SymbolId};
use me_engine::command::Command;
use me_engine::router::Router;

// ─── CLI ────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "me-datagen", about = "Realistic order flow generator for the matching engine")]
struct Cli {
    /// Number of orders to generate.
    #[arg(long, default_value_t = 100_000)]
    orders: u64,

    /// Number of trading pairs.
    #[arg(long, default_value_t = 1)]
    pairs: u16,

    /// Starting mid-price for each pair.
    #[arg(long, default_value_t = 50_000.0)]
    base_price: f64,

    /// Per-step price volatility (0.0 - 1.0). Controls the standard deviation
    /// of each random-walk step as a fraction of the current mid-price.
    #[arg(long, default_value_t = 0.001)]
    volatility: f64,

    /// Fraction of orders that are market orders (0.0 - 1.0).
    #[arg(long, default_value_t = 0.15)]
    market_ratio: f64,

    /// Fraction of commands that are cancels (0.0 - 1.0).
    #[arg(long, default_value_t = 0.20)]
    cancel_ratio: f64,

    /// Fraction of commands that are amends (0.0 - 1.0).
    /// The remainder after market + cancel + amend + stop is limit orders.
    #[arg(long, default_value_t = 0.05)]
    amend_ratio: f64,

    /// Fraction of limit orders placed as stop-limit or stop-market.
    #[arg(long, default_value_t = 0.05)]
    stop_ratio: f64,

    /// Buy-side probability (0.0 - 1.0). Default 0.5 = balanced book.
    #[arg(long, default_value_t = 0.5)]
    buy_ratio: f64,

    /// Probability that a limit order uses IOC time-in-force.
    #[arg(long, default_value_t = 0.05)]
    ioc_ratio: f64,

    /// Probability that a limit order uses FOK time-in-force.
    #[arg(long, default_value_t = 0.02)]
    fok_ratio: f64,

    /// Random seed for reproducibility. Omit for a random seed.
    #[arg(long)]
    seed: Option<u64>,
}

// ─── Per-pair state ─────────────────────────────────────────────────────────

/// Tracks the evolving mid-price and live order IDs for one trading pair.
struct PairState {
    symbol: SymbolId,
    mid_price: f64,
    /// Order IDs currently resting on this pair's book.
    live_orders: Vec<OrderId>,
}

// ─── Statistics ─────────────────────────────────────────────────────────────

#[derive(Default)]
struct Stats {
    commands_processed: u64,
    limit_orders: u64,
    market_orders: u64,
    stop_orders: u64,
    cancels: u64,
    amends: u64,
    trades: u64,
    orders_accepted: u64,
    orders_rejected: u64,
    orders_cancelled: u64,
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Sample a standard-normal value using the Box-Muller transform so we avoid
/// pulling in an extra crate dependency for distributions.
fn sample_normal(rng: &mut StdRng) -> f64 {
    let u1: f64 = rng.random::<f64>().max(1e-15); // avoid ln(0)
    let u2: f64 = rng.random::<f64>();
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

/// Snap a floating-point price to the symbol tick grid (0.01).
fn snap_price(price: f64) -> f64 {
    (price * 100.0).round() / 100.0
}

/// Generate a random order quantity in a realistic range (0.001 .. 2.0).
fn random_qty(rng: &mut StdRng) -> FixedQty {
    let base: f64 = rng.random::<f64>() * 1.999 + 0.001;
    // Snap to lot size 0.00001
    let snapped = (base * 100_000.0).round() / 100_000.0;
    FixedQty::from_f64(snapped)
}

/// Pick a time-in-force based on configured ratios.
fn pick_tif(rng: &mut StdRng, cli: &Cli) -> TimeInForce {
    let r: f64 = rng.random();
    if r < cli.ioc_ratio {
        TimeInForce::IOC
    } else if r < cli.ioc_ratio + cli.fok_ratio {
        TimeInForce::FOK
    } else {
        TimeInForce::GTC
    }
}

/// Pick buy or sell based on the configured buy ratio.
fn pick_side(rng: &mut StdRng, cli: &Cli) -> Side {
    if rng.random::<f64>() < cli.buy_ratio {
        Side::Buy
    } else {
        Side::Sell
    }
}

/// Advance the mid-price by one random-walk step.
fn step_price(mid: f64, volatility: f64, rng: &mut StdRng) -> f64 {
    let change = mid * volatility * sample_normal(rng);
    let new = mid + change;
    // Clamp to a sane floor (tick size) so we never go to zero or negative.
    snap_price(new.max(0.01))
}

/// Generate a limit-order price distributed around the mid.
/// Bids are placed *below* mid, asks *above* mid, spread over a realistic
/// number of levels (up to ~100 ticks from mid).
fn limit_price(mid: f64, side: Side, rng: &mut StdRng) -> FixedPrice {
    // Distance in ticks (0.01 each). Use an exponential distribution so
    // most orders cluster near the top of book.
    let ticks = (-rng.random::<f64>().max(1e-15).ln() * 20.0).min(200.0);
    let offset = ticks * 0.01;
    let price = match side {
        Side::Buy => snap_price(mid - offset),
        Side::Sell => snap_price(mid + offset),
    };
    FixedPrice::from_f64(price.max(0.01))
}

/// Generate a stop price that sits on the "wrong" side of mid so the stop
/// will trigger if the market moves through it.
fn stop_price(mid: f64, side: Side, rng: &mut StdRng) -> FixedPrice {
    let ticks = rng.random::<f64>() * 50.0 + 5.0;
    let offset = ticks * 0.01;
    let price = match side {
        // Buy stop triggers when price rises above stop_price.
        Side::Buy => snap_price(mid + offset),
        // Sell stop triggers when price falls below stop_price.
        Side::Sell => snap_price(mid - offset),
    };
    FixedPrice::from_f64(price.max(0.01))
}

// ─── Command generation ─────────────────────────────────────────────────────

/// Decide which command type to generate and build it.
fn generate_command(
    pair: &mut PairState,
    next_id: &mut OrderId,
    ts: &mut u64,
    rng: &mut StdRng,
    cli: &Cli,
    stats: &mut Stats,
) -> Command {
    *ts += 1;
    let roll: f64 = rng.random();

    // Cancel
    if roll < cli.cancel_ratio && !pair.live_orders.is_empty() {
        let idx = rng.random_range(0..pair.live_orders.len());
        let oid = pair.live_orders.swap_remove(idx);
        stats.cancels += 1;
        return Command::CancelOrder {
            order_id: oid,
            symbol: pair.symbol,
            timestamp: *ts,
        };
    }

    // Amend
    let amend_start = cli.cancel_ratio;
    if roll < amend_start + cli.amend_ratio && !pair.live_orders.is_empty() {
        let idx = rng.random_range(0..pair.live_orders.len());
        let oid = pair.live_orders[idx];
        // Randomly amend price, qty, or both.
        let new_price = if rng.random::<bool>() {
            Some(limit_price(pair.mid_price, pick_side(rng, cli), rng))
        } else {
            None
        };
        let new_qty = if new_price.is_none() || rng.random::<bool>() {
            Some(random_qty(rng))
        } else {
            None
        };
        stats.amends += 1;
        return Command::AmendOrder {
            order_id: oid,
            symbol: pair.symbol,
            new_price,
            new_qty,
            timestamp: *ts,
        };
    }

    // Market order
    let market_start = amend_start + cli.amend_ratio;
    if roll < market_start + cli.market_ratio {
        let id = *next_id;
        *next_id += 1;
        let side = pick_side(rng, cli);
        let qty = random_qty(rng);
        stats.market_orders += 1;
        return Command::PlaceOrder(Order::new_market(
            id,
            pair.symbol,
            side,
            TimeInForce::IOC,
            qty,
            *ts,
        ));
    }

    // ── Limit (possibly stop) order ─────────────────────────────────────

    let id = *next_id;
    *next_id += 1;
    let side = pick_side(rng, cli);
    let qty = random_qty(rng);
    let tif = pick_tif(rng, cli);

    // Decide whether this is a stop order.
    let is_stop = rng.random::<f64>() < cli.stop_ratio;

    let order = if is_stop {
        let sp = stop_price(pair.mid_price, side, rng);
        // 50/50 stop-limit vs stop-market among stop orders.
        if rng.random::<bool>() {
            let lp = limit_price(pair.mid_price, side, rng);
            stats.stop_orders += 1;
            Order::new_stop_limit(id, pair.symbol, side, tif, lp, sp, qty, *ts)
        } else {
            stats.stop_orders += 1;
            Order::new_stop_market(id, pair.symbol, side, tif, sp, qty, *ts)
        }
    } else {
        let price = limit_price(pair.mid_price, side, rng);
        stats.limit_orders += 1;
        Order::new_limit(id, pair.symbol, side, tif, price, qty, *ts)
    };

    // Track GTC limit orders as potentially cancellable later.
    if tif == TimeInForce::GTC {
        pair.live_orders.push(id);
    }

    Command::PlaceOrder(order)
}

// ─── Event accounting ───────────────────────────────────────────────────────

fn account_events(events: &[MatchEvent], pair: &mut PairState, stats: &mut Stats) {
    for event in events {
        match event {
            MatchEvent::Trade(_) => {
                stats.trades += 1;
            }
            MatchEvent::OrderAccepted { .. } => {
                stats.orders_accepted += 1;
            }
            MatchEvent::OrderRejected { .. } => {
                stats.orders_rejected += 1;
            }
            MatchEvent::OrderCancelled { order_id, .. } => {
                stats.orders_cancelled += 1;
                // Remove from live set so we don't try to cancel again.
                if let Some(pos) = pair.live_orders.iter().position(|&id| id == *order_id) {
                    pair.live_orders.swap_remove(pos);
                }
            }
            MatchEvent::OrderFilled { order_id, .. } => {
                // Filled orders leave the book.
                if let Some(pos) = pair.live_orders.iter().position(|&id| id == *order_id) {
                    pair.live_orders.swap_remove(pos);
                }
            }
            _ => {}
        }
    }
}

// ─── Main ───────────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();

    // Validate ratio sum.
    let ratio_sum = cli.market_ratio + cli.cancel_ratio + cli.amend_ratio;
    if ratio_sum > 1.0 {
        eprintln!(
            "error: market_ratio + cancel_ratio + amend_ratio = {ratio_sum:.3} exceeds 1.0"
        );
        std::process::exit(1);
    }

    // Seed the RNG.
    let seed = cli.seed.unwrap_or_else(|| {
        let s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        eprintln!("using random seed: {s}");
        s
    });
    let mut rng = StdRng::seed_from_u64(seed);

    // Build router and register pairs.
    let mut router = Router::new();
    let mut pairs: Vec<PairState> = Vec::with_capacity(cli.pairs as usize);

    for i in 0..cli.pairs {
        let mut config = SymbolConfig::default_btc_usdt();
        config.id = SymbolId(i);
        config.base = format!("BASE{i}");
        config.quote = "USDT".into();
        router.add_symbol(config);
        pairs.push(PairState {
            symbol: SymbolId(i),
            mid_price: cli.base_price,
            live_orders: Vec::with_capacity(1024),
        });
    }

    let mut stats = Stats::default();
    let mut next_id: OrderId = 1;
    let mut ts: u64 = 0;

    // ── Seed the book with initial depth ────────────────────────────────
    // Place a burst of limit orders on both sides to build realistic depth
    // before we start the timed run.
    let seed_count = 200_u64 * cli.pairs as u64;
    for _ in 0..seed_count {
        let pair_idx = rng.random_range(0..pairs.len());
        let pair = &mut pairs[pair_idx];
        let side = pick_side(&mut rng, &cli);
        let price = limit_price(pair.mid_price, side, &mut rng);
        let qty = random_qty(&mut rng);
        ts += 1;
        let id = next_id;
        next_id += 1;
        let order = Order::new_limit(id, pair.symbol, side, TimeInForce::GTC, price, qty, ts);
        pair.live_orders.push(id);
        let events = router.process(Command::PlaceOrder(order));
        account_events(&events, pair, &mut stats);
    }

    // Reset stats after seeding so they reflect the timed run only.
    stats = Stats::default();

    // ── Timed run ───────────────────────────────────────────────────────
    let start = Instant::now();

    for _ in 0..cli.orders {
        // Pick a random pair.
        let pair_idx = rng.random_range(0..pairs.len());
        let pair = &mut pairs[pair_idx];

        // Evolve the mid-price.
        pair.mid_price = step_price(pair.mid_price, cli.volatility, &mut rng);

        let cmd = generate_command(pair, &mut next_id, &mut ts, &mut rng, &cli, &mut stats);
        let events = router.process(cmd);
        stats.commands_processed += 1;
        account_events(&events, pair, &mut stats);
    }

    let elapsed = start.elapsed();
    let secs = elapsed.as_secs_f64();
    let orders_per_sec = if secs > 0.0 {
        cli.orders as f64 / secs
    } else {
        0.0
    };

    // ── Collect book depth across all pairs ─────────────────────────────
    let mut total_bid_levels: usize = 0;
    let mut total_ask_levels: usize = 0;
    let mut total_resting: usize = 0;

    for pair in &pairs {
        if let Some(engine) = router.get_engine(pair.symbol) {
            let book = engine.book();
            total_bid_levels += book.bid_level_count();
            total_ask_levels += book.ask_level_count();
            total_resting += book.order_count();
        }
    }

    let avg_bid_depth = total_bid_levels as f64 / cli.pairs.max(1) as f64;
    let avg_ask_depth = total_ask_levels as f64 / cli.pairs.max(1) as f64;

    // ── Print summary ───────────────────────────────────────────────────
    println!("=== me-datagen results ===");
    println!("seed:                {seed}");
    println!("pairs:               {}", cli.pairs);
    println!("commands processed:  {}", stats.commands_processed);
    println!("  limit orders:      {}", stats.limit_orders);
    println!("  market orders:     {}", stats.market_orders);
    println!("  stop orders:       {}", stats.stop_orders);
    println!("  cancels:           {}", stats.cancels);
    println!("  amends:            {}", stats.amends);
    println!("trades executed:     {}", stats.trades);
    println!("orders accepted:     {}", stats.orders_accepted);
    println!("orders rejected:     {}", stats.orders_rejected);
    println!("orders cancelled:    {}", stats.orders_cancelled);
    println!("resting orders:      {total_resting}");
    println!("avg bid depth:       {avg_bid_depth:.1} levels");
    println!("avg ask depth:       {avg_ask_depth:.1} levels");
    println!("elapsed:             {secs:.3}s");
    println!("throughput:          {orders_per_sec:.0} commands/sec");
}
