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
mod executor;
mod logging;
mod multicall;
mod pricing;
mod provider;
mod strategy;
mod summary;
mod watchdog;

use config::Config;
use constants::UNI_V3_FEE_TIERS;
use pricing::{raw_to_weth, sushi_v2, uniswap_v3, usd_per_eth, weth_to_raw};
use provider::{build_http_provider, HttpProvider};
use strategy::{FirstLeg, MarketState, OpportunityRecord, Venue};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "arb-bot",
    about = "Arbitrum WETH/USDC arbitrage bot (Phase 2: read-only price logger)"
)]
struct Args {
    /// Fetch quotes for exactly one block, print a human-readable table, then exit.
    #[arg(long, default_value_t = false)]
    dry_run_once: bool,

    /// Summarise a JSONL opportunity log and exit.
    #[arg(long)]
    summarize: Option<Option<String>>,
}

/// Static context discovered once at startup.
struct Ctx {
    v3_pools: Vec<uniswap_v3::V3Pool>,
    sushi_meta: sushi_v2::PairMeta,
    trade_sizes: Vec<f64>,
    output_file: String,
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
        let file = std::fs::File::open(&path)
            .with_context(|| format!("Cannot open JSONL file: {path}"))?;
        let stats = summary::summarize_reader(std::io::BufReader::new(file))?;
        println!("File: {path}");
        summary::print_summary(&stats);
        return Ok(());
    }

    let cfg = Config::from_env().context("Failed to load configuration")?;

    // Safety check: never run transactions in Phase 2.
    if cfg.execution_enabled {
        warn!("EXECUTION_ENABLED=true is set but Phase 2 is read-only. Ignoring.");
    }

    let http = build_http_provider(&cfg.rpc_http)?;

    // Discover Uniswap V3 pools at startup.
    info!("Discovering Uniswap V3 pools…");
    let v3_pools = uniswap_v3::discover_pools(&http, &UNI_V3_FEE_TIERS)
        .await
        .context("Pool discovery failed")?;

    if v3_pools.is_empty() {
        anyhow::bail!("No UniV3 WETH/USDC pools found — check addresses in constants.rs");
    }
    for p in &v3_pools {
        info!(fee = p.fee, pool = %p.address, "Found UniV3 pool");
    }

    // Determine SushiSwap token ordering once.
    let sushi_meta = sushi_v2::get_pair_meta(&http)
        .await
        .context("Failed to read Sushi pair token ordering")?;
    info!(weth_is_token0 = sushi_meta.weth_is_token0, "Sushi pair meta loaded");

    // Ensure output directory exists.
    if let Some(parent) = Path::new(&cfg.output_file).parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create output dir {:?}", parent))?;
    }

    let calls = rpc_calls_per_block();
    info!(
        rpc_calls_per_processed_block = calls,
        log_every_n_blocks = cfg.log_every_n_blocks,
        "RPC budget (Multicall3 batched)"
    );

    let ctx = Ctx {
        v3_pools,
        sushi_meta,
        trade_sizes: cfg.trade_sizes_weth.clone(),
        output_file: cfg.output_file.clone(),
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
    //
    // Processing of a sampled block must finish before the next sampled block starts.
    // If block processing falls behind, the newly arriving sampled block is skipped
    // and a warning is logged with the lag in blocks.
    //
    // Watchdog: If no sampled block is processed for `cfg.watchdog_secs`, log an ERROR
    // and exit with code 1 so an external restart loop can restart the process.
    // The watchdog timer arms once the WebSocket subscription is established.
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

    tokio::select! {
        res = block_loop => {
            if let Err(e) = res {
                error!(
                    error = %provider::redact_urls(&format!("{e:#}")),
                    "WebSocket block loop terminated with error"
                );
                std::process::exit(1);
            }
            Ok(())
        }
        tripped = watchdog_runner.run() => {
            error!(
                timeout_secs = cfg.watchdog_secs,
                elapsed_secs = tripped.elapsed.as_secs(),
                "Watchdog tripped: no sampled block processed for {} seconds; exiting with non-zero code",
                cfg.watchdog_secs
            );
            std::process::exit(1);
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

    // All per-block reads and quotes executed in 2 aggregate3 calls pinned to `block`.
    let batched = strategy::process_block_batched(
        http,
        &ctx.v3_pools,
        ctx.sushi_meta,
        &ctx.trade_sizes,
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
    );

    let rpc_calls = rpc_calls_per_block();

    info!(
        block = block_num,
        rpc_calls = rpc_calls,
        records = records.len(),
        best_bps = records.first().map(|r| r.gross_spread_bps).unwrap_or(f64::NAN),
        elapsed_ms = t0.elapsed().as_millis(),
        "Block processed"
    );

    strategy::append_jsonl(&ctx.output_file, &records)?;

    if dry_run {
        print_prices(&batched.state, &batched.first);
        print_sanity(&batched.first);
        // Cross-check local Sushi math against the deployed router (+1 RPC call).
        if let Some(&size) = ctx.trade_sizes.first() {
            match sushi_v2::cross_check_router(http, &batched.state.sushi, weth_to_raw(size), block).await {
                Ok(c) => println!(
                    "Sushi CPF cross-check @ {} WETH: local={} router={} diff={}  {}",
                    raw_to_weth(c.amount_in),
                    c.local_out,
                    c.router_out,
                    c.local_out.abs_diff(c.router_out),
                    if c.local_out == c.router_out { "OK (exact)" } else { "MISMATCH" }
                ),
                Err(e) => println!(
                    "Sushi CPF cross-check failed: {}",
                    provider::redact_urls(&format!("{e:#}"))
                ),
            }
        }
        print_spreads(&records);
        println!(
            "\nWrote {} records to {} (block {}, {} ms)\n",
            records.len(),
            ctx.output_file,
            block_num,
            t0.elapsed().as_millis()
        );
    }
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// RPC calls per processed block (continuous mode):
/// Batch 1 (state + first legs) + Batch 2 (second legs) = 2 Multicall3 aggregate3 calls.
/// Block numbers arrive via the WS subscription (no extra call).
/// `--dry-run-once` adds 1 `eth_blockNumber` + 1 router cross-check.
fn rpc_calls_per_block() -> usize {
    2
}

/// Per-venue price and depth table.
fn print_prices(state: &MarketState, first: &[FirstLeg]) {
    println!("\n══ Venue snapshot (block-pinned) ══");
    println!(
        "{:<12} {:>8} {:>14} {:>24}",
        "Venue", "Size(W)", "USD/ETH", "Depth (V3 L units)"
    );
    println!("{:-<62}", "");
    for leg in first {
        println!(
            "{:<12} {:>8.3} {:>14.2} {:>24}",
            leg.venue.to_string(),
            leg.size_weth,
            usd_per_eth(leg.weth_in, leg.usdc_out),
            state.liquidity(leg.venue)
        );
    }
    println!(
        "SushiV2 reserves: {:.6} WETH / {:.2} USDC",
        raw_to_weth(state.sushi.reserve_weth),
        state.sushi.reserve_usdc as f64 / 1e6
    );
}

/// Decimals sanity check: smallest-size price per venue vs the deepest V3 tier.
fn print_sanity(first: &[FirstLeg]) {
    let Some(min_size) = first.iter().map(|l| l.size_weth).reduce(f64::min) else {
        return;
    };
    let small: Vec<&FirstLeg> = first.iter().filter(|l| l.size_weth == min_size).collect();
    let Some(reference) = small
        .iter()
        .find(|l| l.venue == Venue::UniswapV3 { fee: 500 })
        .or(small.first())
    else {
        return;
    };
    let ref_px = usd_per_eth(reference.weth_in, reference.usdc_out);
    println!("\n══ Decimals sanity @ {min_size} WETH (vs {}) ══", reference.venue);
    for l in &small {
        let px = usd_per_eth(l.weth_in, l.usdc_out);
        let dev = if ref_px > 0.0 { (px / ref_px - 1.0) * 100.0 } else { 0.0 };
        // Fees + impact explain a few %; a decimals bug shows up as ~10^12x.
        let verdict = if dev.abs() < 10.0 { "plausible" } else { "CHECK" };
        println!("{:<12} {:>10.2} USD/ETH  {:>+7.2}%  {verdict}", l.venue.to_string(), px, dev);
    }
}

/// Spread table, best first.
fn print_spreads(records: &[OpportunityRecord]) {
    println!("\n══ Round-trip gross spreads (before gas / flash fee; best first) ══");
    println!(
        "{:<12} {:<12} {:<8} {:>8} {:>12}",
        "Venue A", "Venue B", "Dir", "Size(W)", "Spread(bps)"
    );
    println!("{:-<56}", "");
    for r in records {
        println!(
            "{:<12} {:<12} {:<8} {:>8.3} {:>12.2}",
            r.venue_a, r.venue_b, r.direction, r.size_weth, r.gross_spread_bps
        );
    }
    println!("Dir: a_to_b = WETH->USDC on A, USDC->WETH on B; b_to_a = reverse.");
}

