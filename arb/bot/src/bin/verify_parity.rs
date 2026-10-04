//! Verify that Multicall3 batched results match direct RPC calls on the same block for PancakeSwap V3.

use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use anyhow::Result;
use arb_bot::config::Config;
use arb_bot::constants::PANCAKE_V3_QUOTER_V2;
use arb_bot::pricing::uniswap_v3::Side;
use arb_bot::pricing::{uniswap_v3, weth_to_raw};
use arb_bot::provider::build_http_provider;
use arb_bot::strategy::{self, Venue};

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::from_env()?;
    let http = build_http_provider(&cfg.rpc_http)?;

    let mut uni_tiers = Vec::new();
    let mut pancake_tiers = Vec::new();
    for v in &cfg.venues {
        match v {
            Venue::UniswapV3 { fee } => uni_tiers.push(*fee),
            Venue::PancakeV3 { fee } => pancake_tiers.push(*fee),
            _ => {}
        }
    }

    let mut v3_pools = Vec::new();
    if !uni_tiers.is_empty() {
        v3_pools.extend(uniswap_v3::discover_uniswap_pools(&http, &uni_tiers).await?);
    }
    if !pancake_tiers.is_empty() {
        v3_pools.extend(uniswap_v3::discover_pancake_pools(&http, &pancake_tiers).await?);
    }

    let block_num = alloy::providers::Provider::get_block_number(&http).await?;
    let block = BlockId::number(block_num);
    println!("Testing Multicall3 parity on block {}\n", block_num);

    let pancake_quoter: Address = PANCAKE_V3_QUOTER_V2.parse()?;
    let pancake_500_pool = v3_pools
        .iter()
        .find(|p| p.venue == Venue::PancakeV3 { fee: 500 })
        .expect("Pancake-500 pool not discovered");

    // 1. Direct unbatched RPC calls pinned to block
    let direct_liq_500 = uniswap_v3::fetch_liquidity(&http, std::slice::from_ref(pancake_500_pool), block).await?[0];
    let direct_quote_100 = uniswap_v3::quote(
        &http,
        pancake_quoter,
        block,
        100,
        Side::WethToUsdc,
        U256::from(weth_to_raw(0.05)),
    )
    .await?
    .map(|v| v.to::<u128>())
    .unwrap_or(0);

    let direct_quote_500 = uniswap_v3::quote(
        &http,
        pancake_quoter,
        block,
        500,
        Side::WethToUsdc,
        U256::from(weth_to_raw(0.05)),
    )
    .await?
    .map(|v| v.to::<u128>())
    .unwrap_or(0);

    // 2. Multicall3 batched calls (Batch 1 + Batch 2)
    let batched = strategy::process_block_batched(
        &http,
        &v3_pools,
        None,
        &[0.05, 0.1, 0.25, 0.5],
        block,
    )
    .await?;

    let mc_liq_500 = batched.state.liquidity(Venue::PancakeV3 { fee: 500 });
    let mc_quote_100 = batched
        .first
        .iter()
        .find(|l| l.venue == Venue::PancakeV3 { fee: 100 } && l.size_weth == 0.05)
        .map(|l| l.usdc_out)
        .unwrap_or(0);

    let mc_quote_500 = batched
        .first
        .iter()
        .find(|l| l.venue == Venue::PancakeV3 { fee: 500 } && l.size_weth == 0.05)
        .map(|l| l.usdc_out)
        .unwrap_or(0);

    println!("{:<32} {:<24} {:<24} {:<10}", "Value Checked", "Direct Unbatched", "Multicall3 Batched", "Status");
    println!("{:-<92}", "");

    // Value 1: Pancake-500 pool liquidity
    println!(
        "{:<32} {:<24} {:<24} {:<10}",
        "1. Pancake-500 Liquidity",
        direct_liq_500,
        mc_liq_500,
        if direct_liq_500 == mc_liq_500 { "EXACT MATCH" } else { "MISMATCH" }
    );

    // Value 2: Pancake-100 quote @ 0.05 WETH
    println!(
        "{:<32} {:<24} {:<24} {:<10}",
        "2. Pancake-100 0.05W Quote",
        direct_quote_100,
        mc_quote_100,
        if direct_quote_100 == mc_quote_100 { "EXACT MATCH" } else { "MISMATCH" }
    );

    // Value 3: Pancake-500 quote @ 0.05 WETH
    println!(
        "{:<32} {:<24} {:<24} {:<10}",
        "3. Pancake-500 0.05W Quote",
        direct_quote_500,
        mc_quote_500,
        if direct_quote_500 == mc_quote_500 { "EXACT MATCH" } else { "MISMATCH" }
    );

    assert_eq!(direct_liq_500, mc_liq_500, "Pancake-500 liquidity mismatch");
    assert_eq!(direct_quote_100, mc_quote_100, "Pancake-100 quote mismatch");
    assert_eq!(direct_quote_500, mc_quote_500, "Pancake-500 quote mismatch");

    println!("\nAll 3 values match exactly between direct and Multicall3 batched execution!\n");
    Ok(())
}
