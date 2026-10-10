//! Entry point: wires up config, logging, providers, and the main event loop.
//!
//! Phase 2 — read-only price logger.
//!
//! # Usage
//! ```
//! cargo run                    # continuous block loop
//! cargo run -- --dry-run-once  # fetch one block, print table, exit
//! ```

use alloy::eips::BlockId;
use alloy::providers::Provider;
use anyhow::{Context, Result};
use clap::Parser;
use std::path::Path;
use tracing::{error, info, warn};

mod config;
mod constants;
mod discover;
mod executor;
mod logging;
mod multicall;
mod pricing;
mod provider;
mod strategy;
mod summary;
mod tokens;
mod watchdog;

use config::Config;
use pricing::{implied_quote_per_weth, sushi_v2, uniswap_v3};
use provider::{build_http_provider, HttpProvider};
use strategy::{FirstLeg, MarketSetup, MarketState, OpportunityRecord, Venue};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "arb-bot",
    about = "Arbitrum WETH multi-market arbitrage bot (Phase 2f: read-only price logger)"
)]
struct Args {
    /// Fetch quotes for exactly one block, print a human-readable table, then exit.
    #[arg(long, default_value_t = false)]
    dry_run_once: bool,

    /// Discover pools and markets across UniV3 and PancakeV3 from tokens.toml.
    #[arg(long, default_value_t = false)]
    discover: bool,

    /// Output path for discovered markets file (default: data/markets.json).
    #[arg(long, default_value = "data/markets.json")]
    markets_out: String,

    /// Minimum WETH balance required for a pool to be kept (default: 10.0 WETH).
    #[arg(long)]
    min_pool_weth: Option<f64>,

    /// Summarise a JSONL opportunity log and exit.
    #[arg(long)]
    summarize: Option<Option<String>>,

    /// Show top N ranked groups in the summary (by max spread, rows above -2 bps, and persistence).
    #[arg(long)]
    top: Option<usize>,

    /// Minimum gross profit in USD for the profit threshold column in summary (default: 0.01).
    #[arg(long, default_value_t = 0.01)]
    usd_min: f64,
}

/// Static context discovered once at startup.
struct Ctx {
    markets: Vec<MarketSetup>,
    output_file: String,
    heartbeat_file: String,
    stats_file: String,
    log_min_spread_bps: f64,
    stats: std::sync::Arc<tokio::sync::Mutex<summary::StatsSnapshot>>,
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    // Never let `main` return the error itself: the default `Termination`
    // impl prints the full Debug chain, which can contain the RPC URL.
    if let Err(e) = run().await {
        eprintln!("Error: {}", provider::redact_urls(&format!("{e:#}")));
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    logging::init()?;
    let args = Args::parse();

    // If --summarize is requested, run analysis and exit immediately without RPC connection.
    if let Some(path_opt) = args.summarize {
        let path = path_opt.unwrap_or_else(|| "data/opportunities.jsonl".into());
        let (stats, hb_summaries) = summary::summarize_from_paths(Path::new(&path), args.usd_min)
            .with_context(|| format!("Cannot summarise file: {path}"))?;
        println!("File: {path}");
        summary::print_summary(&stats, args.top, args.usd_min);
        summary::print_market_heartbeat_table(&hb_summaries);
        return Ok(());
    }

    let cfg = Config::from_env().context("Failed to load configuration")?;

    let http = build_http_provider(&cfg.rpc_http)?;

    // If --discover is requested, run pool discovery and write discovered markets file
    if args.discover {
        let min_weth = args.min_pool_weth.unwrap_or(10.0);
        discover::run_discovery(&http, min_weth, &args.markets_out).await?;
        return Ok(());
    }

    // Safety check: never run transactions in Phase 2.
    if cfg.execution_enabled {
        warn!("EXECUTION_ENABLED=true is set but Phase 2 is read-only. Ignoring.");
    }

    // Discover pools for each configured market
    let mut market_setups = Vec::new();
    for m_cfg in &cfg.markets {
        info!(pair = %m_cfg.pair, venues = ?m_cfg.venues, "Discovering pools for market…");
        let pools = uniswap_v3::discover_market_pools(
            &http,
            &m_cfg.pair,
            m_cfg.quote_token,
            &m_cfg.venues,
        )
        .await
        .with_context(|| format!("Pool discovery failed for market {}", m_cfg.pair))?;

        let sushi_meta = if m_cfg.symbol == "USDC" && m_cfg.venues.contains(&Venue::SushiV2) {
            info!("Loading SushiSwap V2 pair metadata for WETH/USDC…");
            let meta = sushi_v2::get_pair_meta(&http)
                .await
                .context("Failed to read Sushi pair token ordering")?;
            info!(weth_is_token0 = meta.weth_is_token0, "Sushi pair meta loaded");
            Some(meta)
        } else {
            None
        };

        if pools.is_empty() && sushi_meta.is_none() {
            warn!(pair = %m_cfg.pair, "No active pools discovered with >=10 WETH for market");
        }

        for p in &pools {
            info!(pair = %p.pair, venue = %p.venue, pool = %p.address, "Active pool configured");
        }

        market_setups.push(MarketSetup {
            config: m_cfg.clone(),
            pools,
            sushi_meta,
        });
    }

    let total_active_pools: usize = market_setups.iter().map(|m| m.pools.len()).sum();
    if total_active_pools == 0 && !market_setups.iter().any(|m| m.sushi_meta.is_some()) {
        anyhow::bail!("No active pools discovered across any market — check MARKETS/VENUES configuration");
    }

    // Ensure output directories exist.
    for path in [&cfg.output_file, &cfg.heartbeat_file, &cfg.stats_file] {
        if let Some(parent) = Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("Cannot create output dir {:?}", parent))?;
            }
        }
    }

    let calls = rpc_calls_per_block();
    info!(
        rpc_calls_per_processed_block = calls,
        log_every_n_blocks = cfg.log_every_n_blocks,
        markets = cfg.markets.len(),
        log_min_spread_bps = cfg.log_min_spread_bps,
        heartbeat_file = %cfg.heartbeat_file,
        stats_file = %cfg.stats_file,
        "RPC budget and logging thresholds (Multicall3 batched)"
    );

    // Load existing stats snapshot on startup if present
    let initial_stats = summary::StatsSnapshot::load_or_default(&cfg.stats_file);
    let stats = std::sync::Arc::new(tokio::sync::Mutex::new(initial_stats));

    let ctx = Ctx {
        markets: market_setups,
        output_file: cfg.output_file.clone(),
        heartbeat_file: cfg.heartbeat_file.clone(),
        stats_file: cfg.stats_file.clone(),
        log_min_spread_bps: cfg.log_min_spread_bps,
        stats,
    };

    if args.dry_run_once {
        let block_num = http
            .get_block_number()
            .await
            .context("eth_blockNumber failed")?;
        info!(block = block_num, "dry-run-once: fetching quotes via Multicall3…");
        return process_block(&http, &ctx, block_num, true).await;
    }

    // Continuous block loop with lag detection and watchdog.
    let log_every = cfg.log_every_n_blocks;
    let ctx = std::sync::Arc::new(ctx);
    let http = std::sync::Arc::new(http);
    let active_block = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

    let watchdog = watchdog::Watchdog::new(std::time::Duration::from_secs(cfg.watchdog_secs));
    let watchdog_sub = watchdog.clone();
    let watchdog_block = watchdog.clone();
    let watchdog_runner = watchdog.clone();

    info!(
        watchdog_secs = cfg.watchdog_secs,
        "Watchdog configured (arms on WebSocket subscription)"
    );

    let active_block_clone = std::sync::Arc::clone(&active_block);
    let ctx_clone = std::sync::Arc::clone(&ctx);
    let http_clone = std::sync::Arc::clone(&http);

    let block_loop = provider::run_block_loop(
        &cfg.rpc_ws,
        &http,
        move || {
            watchdog_sub.arm();
        },
        move |_, block_num| {
            let active_block = std::sync::Arc::clone(&active_block_clone);
            let ctx = std::sync::Arc::clone(&ctx_clone);
            let http = std::sync::Arc::clone(&http_clone);
            let watchdog_ref = watchdog_block.clone();
            async move {
                if !block_num.is_multiple_of(log_every) {
                    return Ok(());
                }
                let active = active_block.load(std::sync::atomic::Ordering::SeqCst);
                if active != 0 {
                    let lag = block_num.saturating_sub(active);
                    warn!(
                        block = block_num,
                        active_block = active,
                        lag_blocks = lag,
                        "Block processing fell behind active block — skipping sampled block"
                    );
                    return Ok(());
                }
                active_block.store(block_num, std::sync::atomic::Ordering::SeqCst);
                let active_ref = std::sync::Arc::clone(&active_block);
                tokio::spawn(async move {
                    let res = process_block(&http, &ctx, block_num, false).await;
                    active_ref.store(0, std::sync::atomic::Ordering::SeqCst);
                    match res {
                        Ok(()) => {
                            watchdog_ref.notify_processed_block();
                        }
                        Err(e) => {
                            warn!(
                                block = block_num,
                                error = %provider::redact_urls(&format!("{e:#}")),
                                "Block processing error"
                            );
                        }
                    }
                });
                Ok(())
            }
        },
    );

    // Periodic 10-minute stats snapshot saver
    let stats_timer_ctx = std::sync::Arc::clone(&ctx.stats);
    let stats_timer_file = ctx.stats_file.clone();
    let stats_timer = async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(600));
        interval.tick().await; // consume initial tick
        loop {
            interval.tick().await;
            let st = stats_timer_ctx.lock().await;
            if let Err(e) = st.save_atomic(&stats_timer_file) {
                warn!(error = %e, "Failed to save periodic 10-minute stats snapshot");
            } else {
                info!(path = %stats_timer_file, "Saved periodic 10-minute stats snapshot");
            }
        }
    };

    let shutdown_stats = std::sync::Arc::clone(&ctx.stats);
    let shutdown_stats_file = ctx.stats_file.clone();

    tokio::select! {
        res = block_loop => {
            if let Err(e) = res {
                error!(
                    error = %provider::redact_urls(&format!("{e:#}")),
                    "WebSocket block loop terminated with error"
                );
                let st = shutdown_stats.lock().await;
                let _ = st.save_atomic(&shutdown_stats_file);
                std::process::exit(1);
            }
            let st = shutdown_stats.lock().await;
            let _ = st.save_atomic(&shutdown_stats_file);
            Ok(())
        }
        tripped = watchdog_runner.run() => {
            error!(
                timeout_secs = cfg.watchdog_secs,
                elapsed_secs = tripped.elapsed.as_secs(),
                "Watchdog tripped: no sampled block processed for {} seconds; exiting with non-zero code",
                cfg.watchdog_secs
            );
            let st = shutdown_stats.lock().await;
            let _ = st.save_atomic(&shutdown_stats_file);
            std::process::exit(1);
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C shutdown signal; saving stats snapshot atomically...");
            let st = shutdown_stats.lock().await;
            if let Err(e) = st.save_atomic(&shutdown_stats_file) {
                error!(error = %e, "Failed to save stats snapshot on shutdown");
            } else {
                info!(path = %shutdown_stats_file, "Clean shutdown: stats snapshot saved successfully");
            }
            std::process::exit(0);
        }
        _ = stats_timer => {
            Ok(())
        }
    }
}

// ── Block processing ──────────────────────────────────────────────────────────

async fn process_block(
    http: &HttpProvider,
    ctx: &Ctx,
    block_num: u64,
    dry_run: bool,
) -> Result<()> {
    let t0 = std::time::Instant::now();
    let block = BlockId::number(block_num);

    // All per-block reads and quotes executed via chunked Multicall3 pinned to `block`.
    let batched = strategy::process_block_batched(
        http,
        &ctx.markets,
        block,
    )
    .await?;

    let logged_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let records = strategy::to_records(
        block_num,
        batched.block_timestamp,
        logged_at,
        &batched.state,
        &batched.trips,
        batched.eth_usd_price,
    );

    // 1. Extract and write heartbeat records (one line per market per block, flushed immediately)
    let heartbeats = strategy::extract_heartbeat_records(&records);
    strategy::append_heartbeat_jsonl(&ctx.heartbeat_file, &heartbeats)?;

    // 2. Accumulate all evaluated records into stats snapshot
    {
        let mut st = ctx.stats.lock().await;
        for r in &records {
            st.record_sample(
                &r.pair,
                &r.venue_a,
                &r.venue_b,
                &r.direction,
                r.size_weth,
                r.gross_spread_bps,
            );
        }
        if !std::path::Path::new(&ctx.stats_file).exists() {
            let _ = st.save_atomic(&ctx.stats_file);
        }
    }

    // 3. Filter records according to LOG_MIN_SPREAD_BPS (positives always written)
    let filtered_records = strategy::filter_records_by_threshold(&records, ctx.log_min_spread_bps);
    strategy::append_jsonl(&ctx.output_file, &filtered_records)?;

    let rpc_calls = batched.total_rpc_calls;

    info!(
        block = block_num,
        rpc_calls = rpc_calls,
        batch1_rpc = batched.batch1_rpc_calls,
        batch2_rpc = batched.batch2_rpc_calls,
        batch1_calls = batched.batch1_calls,
        batch2_calls = batched.batch2_calls,
        batch1_payload = batched.batch1_payload_bytes,
        batch2_payload = batched.batch2_payload_bytes,
        max_gas_estimate = batched.max_gas_estimate,
        eth_usd_price = batched.eth_usd_price,
        evaluated = records.len(),
        logged = filtered_records.len(),
        heartbeats = heartbeats.len(),
        best_bps = records.first().map(|r| r.gross_spread_bps).unwrap_or(f64::NAN),
        elapsed_ms = t0.elapsed().as_millis(),
        "Block processed"
    );

    if dry_run {
        {
            let st = ctx.stats.lock().await;
            if let Err(e) = st.save_atomic(&ctx.stats_file) {
                warn!(error = %e, "Failed to save stats snapshot in dry run");
            } else {
                info!(path = %ctx.stats_file, "Saved stats snapshot in dry run");
            }
        }

        print_market_snapshot(&ctx.markets, &batched.state, &batched.first);
        print_spreads_per_market(&records);

        println!(
            "\n══ RPC Multicall Metrics (Block {}) ══",
            block_num
        );
        println!(
            "  Total eth_call RPCs: {} (Batch 1: {} chunks, Batch 2: {} chunks)",
            rpc_calls, batched.batch1_rpc_calls, batched.batch2_rpc_calls
        );
        println!(
            "  Batch 1 (State + 1st legs): {} calls in {} chunk(s), {} bytes calldata",
            batched.batch1_calls, batched.batch1_rpc_calls, batched.batch1_payload_bytes
        );
        println!(
            "  Batch 2 (2nd legs):        {} calls in {} chunk(s), {} bytes calldata",
            batched.batch2_calls, batched.batch2_rpc_calls, batched.batch2_payload_bytes
        );
        println!(
            "  Largest eth_call gas estimate: {}",
            batched.max_gas_estimate
        );
        println!(
            "  ETH/USD Reference Price: ${:.2}",
            batched.eth_usd_price
        );
        println!(
            "\nWrote {} filtered records (evaluated {}) to {}, and {} heartbeats to {} ({} ms)\n",
            filtered_records.len(),
            records.len(),
            ctx.output_file,
            heartbeats.len(),
            ctx.heartbeat_file,
            t0.elapsed().as_millis()
        );
    }
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn rpc_calls_per_block() -> usize {
    2
}

/// Print implied price of the quote token per venue and verify agreement within ~10 bps.
fn print_market_snapshot(
    markets: &[MarketSetup],
    state: &MarketState,
    first: &[FirstLeg],
) {
    println!("\n══ Market snapshots & implied prices (block-pinned) ══");
    for m in markets {
        println!("\n── Market: {} (Quote Decimals: {}) ──", m.config.pair, m.config.quote_decimals);
        println!(
            "{:<12} {:>8} {:>18} {:>12} {:>24}",
            "Venue", "Size(W)", format!("{}/WETH", m.config.symbol), "Fee(bps)", "Depth (V3 L units)"
        );
        println!("{:-<78}", "");
        let market_first: Vec<&FirstLeg> = first.iter().filter(|l| l.pair == m.config.pair).collect();
        for leg in &market_first {
            let rate = implied_quote_per_weth(leg.weth_in, leg.quote_out, leg.quote_decimals);
            let fee_str = if let Some(f) = leg.fee_bps {
                format!("{:.2} (dyn)", f)
            } else {
                match leg.venue {
                    Venue::UniswapV3 { fee } | Venue::PancakeV3 { fee } => format!("{:.2}", fee as f64 / 100.0),
                    Venue::SushiV2 => "30.00".to_string(),
                    Venue::CamelotV3 => "-".to_string(),
                }
            };
            println!(
                "{:<12} {:>8.3} {:>18.4} {:>12} {:>24}",
                leg.venue.to_string(),
                leg.size_weth,
                rate,
                fee_str,
                state.pool_liquidity(&leg.pair, leg.venue)
            );
        }

        // Check agreement across venues at the smallest quoted trade size
        if let Some(min_size) = market_first.iter().map(|l| l.size_weth).reduce(f64::min) {
            let small_legs: Vec<&&FirstLeg> = market_first.iter().filter(|l| l.size_weth == min_size).collect();
            if let Some(ref_leg) = small_legs.first() {
                let ref_rate = implied_quote_per_weth(ref_leg.weth_in, ref_leg.quote_out, ref_leg.quote_decimals);
                println!("  Implied price agreement @ {:.3} WETH (ref: {}):", min_size, ref_leg.venue);
                for l in &small_legs {
                    let rate = implied_quote_per_weth(l.weth_in, l.quote_out, l.quote_decimals);
                    let diff_bps = if ref_rate > 0.0 { ((rate - ref_rate) / ref_rate) * 10_000.0 } else { 0.0 };
                    let agree = if diff_bps.abs() <= 10.0 {
                        "AGREES (within 10 bps)"
                    } else {
                        "SLIGHT DIVERGENCE (>10 bps)"
                    };
                    println!(
                        "    {:<12}: {:>14.4} (diff: {:>+6.2} bps)  {}",
                        l.venue.to_string(),
                        rate,
                        diff_bps,
                        agree
                    );
                }
            }
        }
    }
}

/// Print gross spreads per market exactly as computed, without labeling negative results as good.
fn print_spreads_per_market(records: &[OpportunityRecord]) {
    println!("\n══ Top gross spreads per market (fees & slippage included, before gas) ══");
    let mut pairs: Vec<String> = records.iter().map(|r| r.pair.clone()).collect();
    pairs.sort();
    pairs.dedup();

    for pair in pairs {
        println!("\n── Market: {} ──", pair);
        println!(
            "{:<12} {:<12} {:<8} {:>8} {:>14} {:>12} {:>12} {:>14}",
            "Venue A", "Venue B", "Dir", "Size(W)", "Spread(bps)", "Est USD", "DynFee(bps)", "Result"
        );
        println!("{:-<96}", "");
        let market_recs: Vec<&OpportunityRecord> = records.iter().filter(|r| r.pair == pair).collect();
        for r in market_recs {
            let verdict = if r.gross_spread_bps >= 0.0 {
                "PROFITABLE"
            } else {
                "NET LOSS"
            };
            let dyn_fee_str = r.fee_bps_used.map(|f| format!("{:.2}", f)).unwrap_or_else(|| "-".into());
            println!(
                "{:<12} {:<12} {:<8} {:>8.3} {:>14.2} {:>12.2} {:>12} {:>14}",
                r.venue_a, r.venue_b, r.direction, r.size_weth, r.gross_spread_bps, r.est_gross_profit_usd, dyn_fee_str, verdict
            );
        }
    }
    println!("\nNote: Spreads are reported exactly as computed: negative indicates a loss.");
}

