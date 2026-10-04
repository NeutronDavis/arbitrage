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
use std::io::Write;
use std::path::Path;
use tracing::{info, warn};

mod config;
mod constants;
mod executor;
mod logging;
mod pricing;
mod provider;
mod strategy;

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

    let calls = rpc_calls_per_block(v3_pools.len(), cfg.trade_sizes_weth.len());
    info!(
        rpc_calls_per_processed_block = calls,
        log_every_n_blocks = cfg.log_every_n_blocks,
        "RPC budget"
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
        info!(block = block_num, "dry-run-once: fetching quotes…");
        return process_block(&http, &ctx, block_num, true).await;
    }

    // Continuous block loop.
    //
    // Sampling uses the block number itself (`block % N == 0`) rather than a
    // counter: a counter captured into the per-block `async move` future is
    // copied, never incremented in the outer scope, and silently skips every
    // block. Block-number sampling is stateless and survives reconnects.
    let log_every = cfg.log_every_n_blocks;
    let ctx = &ctx;
    let http_ref = &http;
    provider::run_block_loop(&cfg.rpc_ws, &http, |_, block_num| async move {
        if !block_num.is_multiple_of(log_every) {
            return Ok(());
        }
        process_block(http_ref, ctx, block_num, false).await
    })
    .await
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

    // State reads, concurrently, pinned to `block`.
    let (liq, reserves) = tokio::try_join!(
        uniswap_v3::fetch_liquidity(http, &ctx.v3_pools, block),
        sushi_v2::get_reserves(http, ctx.sushi_meta, block),
    )?;
    let state = MarketState {
        block,
        v3: ctx.v3_pools.iter().cloned().zip(liq).collect(),
        sushi: reserves,
    };

    // Quotes for every venue pair, both directions, every size.
    let (first, trips) = strategy::collect_round_trips(http, &state, &ctx.trade_sizes).await?;

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let records = strategy::to_records(block_num, timestamp, &state, &trips);

    info!(
        block = block_num,
        rpc_calls = rpc_calls_per_block(ctx.v3_pools.len(), ctx.trade_sizes.len()),
        records = records.len(),
        best_bps = records.first().map(|r| r.gross_spread_bps).unwrap_or(f64::NAN),
        elapsed_ms = t0.elapsed().as_millis(),
        "Block processed"
    );

    append_jsonl(&ctx.output_file, &records)?;

    if dry_run {
        print_prices(&state, &first);
        print_sanity(&first);
        // Cross-check local Sushi math against the deployed router (+1 RPC call).
        if let Some(&size) = ctx.trade_sizes.first() {
            match sushi_v2::cross_check_router(http, &state.sushi, weth_to_raw(size), block).await {
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
///   V3 `liquidity()` per pool + 1 Sushi `getReserves` + QuoterV2 calls.
/// Block numbers arrive via the WS subscription (no extra call).
/// `--dry-run-once` adds 1 `eth_blockNumber` + 1 router cross-check.
fn rpc_calls_per_block(n_v3: usize, n_sizes: usize) -> usize {
    n_v3 + 1 + strategy::quoter_calls(n_v3, n_sizes)
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

/// Append a slice of records to a JSONL file (one JSON object per line).
fn append_jsonl(path: &str, records: &[OpportunityRecord]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("Cannot open output file: {path}"))?;

    for record in records {
        let line = serde_json::to_string(record)
            .context("Failed to serialise opportunity record")?;
        writeln!(file, "{line}").with_context(|| format!("Write failed: {path}"))?;
    }
    Ok(())
}
