//! Verify that Multicall3 batched results match direct RPC calls on the same block for multi-market pricing.

use alloy::eips::BlockId;
use alloy::primitives::aliases::U24;
use alloy::primitives::{Address, U160, U256};
use anyhow::Result;
use arb_bot::config::Config;
use arb_bot::constants::{
    PANCAKE_V3_QUOTER_V2, UNI_V3_QUOTER_V2, USDT, WBTC, WETH,
};
use arb_bot::pricing::uniswap_v3::{self, IQuoterV2, IUniswapV3Pool};
use arb_bot::pricing::weth_to_raw;
use arb_bot::provider::build_http_provider;
use arb_bot::strategy::{self, MarketSetup, Venue};

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::from_env()?;
    let http = build_http_provider(&cfg.rpc_http)?;

    let mut market_setups = Vec::new();
    for m_cfg in &cfg.markets {
        let pools = uniswap_v3::discover_market_pools(
            &http,
            &m_cfg.pair,
            m_cfg.quote_token,
            &m_cfg.venues,
        )
        .await?;

        market_setups.push(MarketSetup {
            config: m_cfg.clone(),
            pools,
            sushi_meta: None,
        });
    }

    let block_num = alloy::providers::Provider::get_block_number(&http).await?;
    let block = BlockId::number(block_num);
    println!("Testing Multicall3 parity on block {}\n", block_num);

    let uni_quoter: Address = UNI_V3_QUOTER_V2.parse()?;
    let pancake_quoter: Address = PANCAKE_V3_QUOTER_V2.parse()?;
    let weth_addr: Address = WETH.parse()?;
    let wbtc_addr: Address = WBTC.parse()?;
    let usdt_addr: Address = USDT.parse()?;

    // 1. Direct unbatched RPC calls pinned to block
    // (a) Pancake-100 WBTC pool liquidity
    let wbtc_market = market_setups
        .iter()
        .find(|m| m.config.pair == "WETH/WBTC")
        .expect("WETH/WBTC market not configured");
    let pancake_wbtc_100_pool = wbtc_market
        .pools
        .iter()
        .find(|p| p.venue == Venue::PancakeV3 { fee: 100 })
        .expect("Pancake-100 WBTC pool not discovered");

    let direct_liq_wbtc_100 = IUniswapV3Pool::new(pancake_wbtc_100_pool.address, &http)
        .liquidity()
        .block(block)
        .call()
        .await?;

    // (b) One WBTC quote: UniV3-500 WETH -> WBTC @ 0.1 WETH
    let wbtc_in_raw = U256::from(weth_to_raw(0.1));
    let direct_quote_wbtc = IQuoterV2::new(uni_quoter, &http)
        .quoteExactInputSingle(IQuoterV2::QuoteExactInputSingleParams {
            tokenIn: weth_addr,
            tokenOut: wbtc_addr,
            amountIn: wbtc_in_raw,
            fee: U24::from(500),
            sqrtPriceLimitX96: U160::ZERO,
        })
        .block(block)
        .call()
        .await?
        .amountOut;

    // (c) One USDT quote: Pancake-500 WETH -> USDT @ 0.05 WETH
    let usdt_in_raw = U256::from(weth_to_raw(0.05));
    let direct_quote_usdt = IQuoterV2::new(pancake_quoter, &http)
        .quoteExactInputSingle(IQuoterV2::QuoteExactInputSingleParams {
            tokenIn: weth_addr,
            tokenOut: usdt_addr,
            amountIn: usdt_in_raw,
            fee: U24::from(500),
            sqrtPriceLimitX96: U160::ZERO,
        })
        .block(block)
        .call()
        .await?
        .amountOut;

    // 2. Multicall3 batched calls (Batch 1 + Batch 2) across all markets
    let batched = strategy::process_block_batched(
        &http,
        &market_setups,
        block,
    )
    .await?;

    let mc_liq_wbtc_100 = batched.state.pool_liquidity("WETH/WBTC", Venue::PancakeV3 { fee: 100 });
    let mc_quote_wbtc = batched
        .first
        .iter()
        .find(|l| l.pair == "WETH/WBTC" && l.venue == Venue::UniswapV3 { fee: 500 } && (l.size_weth - 0.1).abs() < 1e-6)
        .map(|l| U256::from(l.quote_out))
        .unwrap_or(U256::ZERO);

    let mc_quote_usdt = batched
        .first
        .iter()
        .find(|l| l.pair == "WETH/USDT" && l.venue == Venue::PancakeV3 { fee: 500 } && (l.size_weth - 0.05).abs() < 1e-6)
        .map(|l| U256::from(l.quote_out))
        .unwrap_or(U256::ZERO);

    println!("{:<36} {:<24} {:<24} {:<10}", "Value Checked", "Direct Unbatched", "Multicall3 Batched", "Status");
    println!("{:-<96}", "");

    // Value 1: Pancake-100 WBTC pool liquidity
    println!(
        "{:<36} {:<24} {:<24} {:<10}",
        "1. Pancake-100 WBTC Liquidity",
        direct_liq_wbtc_100,
        mc_liq_wbtc_100,
        if direct_liq_wbtc_100 == mc_liq_wbtc_100 { "EXACT MATCH" } else { "MISMATCH" }
    );

    // Value 2: UniV3-500 WBTC quote @ 0.1 WETH
    println!(
        "{:<36} {:<24} {:<24} {:<10}",
        "2. UniV3-500 WBTC Quote (0.1W)",
        direct_quote_wbtc,
        mc_quote_wbtc,
        if direct_quote_wbtc == mc_quote_wbtc { "EXACT MATCH" } else { "MISMATCH" }
    );

    // Value 3: Pancake-500 USDT quote @ 0.05 WETH
    println!(
        "{:<36} {:<24} {:<24} {:<10}",
        "3. Pancake-500 USDT Quote (0.05W)",
        direct_quote_usdt,
        mc_quote_usdt,
        if direct_quote_usdt == mc_quote_usdt { "EXACT MATCH" } else { "MISMATCH" }
    );

    assert_eq!(direct_liq_wbtc_100, mc_liq_wbtc_100, "Pancake-100 WBTC liquidity mismatch");
    assert_eq!(direct_quote_wbtc, mc_quote_wbtc, "UniV3-500 WBTC quote mismatch");
    assert_eq!(direct_quote_usdt, mc_quote_usdt, "Pancake-500 USDT quote mismatch");

    println!("\nAll 3 values match exactly between direct and Multicall3 batched execution!\n");
    println!("Pancake-100 WBTC pool address: {}", pancake_wbtc_100_pool.address);
    println!("Direct cast call check commands:");
    println!("  cast call --block {} {} \"liquidity()(uint128)\"", block_num, pancake_wbtc_100_pool.address);
    println!("  cast call --block {} {} \"quoteExactInputSingle((address,address,uint256,uint24,uint160))((uint256,uint160,uint32,uint256))\" \"({},{},{},500,0)\"",
        block_num, uni_quoter, weth_addr, wbtc_addr, wbtc_in_raw);
    println!("  cast call --block {} {} \"quoteExactInputSingle((address,address,uint256,uint24,uint160))((uint256,uint160,uint32,uint256))\" \"({},{},{},500,0)\"",
        block_num, pancake_quoter, weth_addr, usdt_addr, usdt_in_raw);

    Ok(())
}
