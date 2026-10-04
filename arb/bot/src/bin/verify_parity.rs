//! Verify that Multicall3 batched results match direct unbatched RPC calls for the same block.

use alloy::eips::BlockId;
use alloy::primitives::U256;
use anyhow::Result;
use arb_bot::config::Config;
use arb_bot::constants::UNI_V3_FEE_TIERS;
use arb_bot::pricing::uniswap_v3::Side;
use arb_bot::pricing::{sushi_v2, uniswap_v3, weth_to_raw};
use arb_bot::provider::build_http_provider;
use arb_bot::strategy;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::from_env()?;
    let http = build_http_provider(&cfg.rpc_http)?;

    let v3_pools = uniswap_v3::discover_pools(&http, &UNI_V3_FEE_TIERS).await?;
    let sushi_meta = sushi_v2::get_pair_meta(&http).await?;

    let block_num = alloy::providers::Provider::get_block_number(&http).await?;
    let block = BlockId::number(block_num);
    println!("Testing Multicall3 parity on block {}\n", block_num);

    // 1. Direct unbatched RPC calls
    let direct_liq = uniswap_v3::fetch_liquidity(&http, &v3_pools, block).await?;
    let direct_reserves = sushi_v2::get_reserves(&http, sushi_meta, block).await?;
    let direct_quote = uniswap_v3::quote(
        &http,
        block,
        500,
        Side::WethToUsdc,
        U256::from(weth_to_raw(0.01)),
    )
    .await?
    .map(|v| v.to::<u128>())
    .unwrap_or(0);

    // 2. Multicall3 batched calls (Batch 1 + Batch 2)
    let batched = strategy::process_block_batched(
        &http,
        &v3_pools,
        sushi_meta,
        &[0.01, 0.05, 0.1],
        block,
    )
    .await?;

    let mc_liq_500 = batched.state.liquidity(strategy::Venue::UniswapV3 { fee: 500 });
    let mc_reserves = batched.state.sushi;
    let mc_quote_500 = batched
        .first
        .iter()
        .find(|l| l.venue == strategy::Venue::UniswapV3 { fee: 500 } && l.size_weth == 0.01)
        .map(|l| l.usdc_out)
        .unwrap_or(0);

    println!("{:<32} {:<24} {:<24} {:<10}", "Value Checked", "Direct Unbatched", "Multicall3 Batched", "Status");
    println!("{:-<92}", "");

    // Value 1: UniV3 500 pool liquidity
    let v1_direct = direct_liq[0];
    let v1_mc = mc_liq_500;
    println!(
        "{:<32} {:<24} {:<24} {:<10}",
        "1. UniV3-500 Liquidity",
        v1_direct,
        v1_mc,
        if v1_direct == v1_mc { "EXACT MATCH" } else { "MISMATCH" }
    );

    // Value 2: SushiSwap WETH & USDC reserves
    let v2_direct = format!("{}/{}", direct_reserves.reserve_weth, direct_reserves.reserve_usdc);
    let v2_mc = format!("{}/{}", mc_reserves.reserve_weth, mc_reserves.reserve_usdc);
    println!(
        "{:<32} {:<24} {:<24} {:<10}",
        "2. Sushi WETH/USDC Reserves",
        v2_direct,
        v2_mc,
        if v2_direct == v2_mc { "EXACT MATCH" } else { "MISMATCH" }
    );

    // Value 3: QuoterV2 0.01 WETH quote on UniV3-500
    let v3_direct = direct_quote;
    let v3_mc = mc_quote_500;
    println!(
        "{:<32} {:<24} {:<24} {:<10}",
        "3. QuoterV2 0.01WETH->USDC (500)",
        v3_direct,
        v3_mc,
        if v3_direct == v3_mc { "EXACT MATCH" } else { "MISMATCH" }
    );

    assert_eq!(v1_direct, v1_mc, "Liquidity mismatch");
    assert_eq!(direct_reserves.reserve_weth, mc_reserves.reserve_weth, "Sushi WETH reserve mismatch");
    assert_eq!(direct_reserves.reserve_usdc, mc_reserves.reserve_usdc, "Sushi USDC reserve mismatch");
    assert_eq!(v3_direct, v3_mc, "Quote mismatch");

    println!("\nAll 3 values match exactly between direct and Multicall3 batched execution!\n");
    Ok(())
}
