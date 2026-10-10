//! Market discovery across Uniswap V3 and PancakeSwap V3 on Arbitrum One.
//!
//! Phase 2f: Automated pool discovery, WETH liquidity depth checks,
//! single-leg price impact measurement, and trade size selection.

use alloy::primitives::aliases::U24;
use alloy::primitives::{Address, U160, U256};
use alloy::providers::Provider;
use anyhow::{Context, Result};
use std::path::Path;
use tracing::info;

use crate::config::MarketConfig;
use crate::constants::{
    CAMELOT_V3_FACTORY, CAMELOT_V3_QUOTER, PANCAKE_V3_FACTORY, PANCAKE_V3_QUOTER_V2,
    UNI_V3_FACTORY, UNI_V3_QUOTER_V2, WETH,
};
use crate::pricing::camelot_v3::IAlgebraFactory;
use crate::pricing::uniswap_v3::{IQuoterV2, IUniswapV3Factory, IERC20};
use crate::pricing::weth_to_raw;
use crate::strategy::Venue;
use crate::tokens::{verify_token, TokenCheckResult, TokensConfig};

/// Details of a discovered pool during `--discover`.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DiscoveredPool {
    pub venue: Venue,
    pub fee: u32,
    pub pool_address: Address,
    pub quoter_address: Address,
    pub weth_balance_raw: U256,
    pub weth_balance_f64: f64,
    /// Price impact (in bps) at 0.05, 0.25, and 1.0 WETH.
    pub impact_0_05_bps: f64,
    pub impact_0_25_bps: Option<f64>,
    pub impact_1_00_bps: Option<f64>,
    pub quote_0_05: Option<U256>,
    pub quote_0_25: Option<U256>,
    pub quote_1_00: Option<U256>,
}

/// Discovered market containing multiple viable pools.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DiscoveredMarket {
    pub symbol: String,
    pub pair: String,
    pub token_address: Address,
    pub decimals: u8,
    pub reviewed: bool,
    pub pools: Vec<DiscoveredPool>,
    pub chosen_sizes_weth: Vec<f64>,
}

/// Run full discovery across reviewed tokens.
///
/// Enforces the allowlist: only tokens with `reviewed == true` are evaluated for pool discovery.
pub async fn run_discovery<P: Provider>(
    http: &P,
    min_pool_weth: f64,
    output_path: &str,
) -> Result<(Vec<TokenCheckResult>, Vec<DiscoveredMarket>)> {
    let tokens_cfg = TokensConfig::load_default()
        .context("Failed to load tokens.toml for discovery")?;

    println!("\n═══════════════════════════════════════════════════════════════════════════════════════════════════════════");
    println!("                                   PHASE 2F: TOKEN ALLOWLIST & SECURITY AUDIT                              ");
    println!("═══════════════════════════════════════════════════════════════════════════════════════════════════════════");

    let mut check_results = Vec::new();
    for entry in &tokens_cfg.tokens {
        let res = verify_token(http, entry).await;
        check_results.push(res);
    }

    // Print Token Verification Table
    println!(
        "{:<8} {:<42} {:<5} {:<10} {:<24} {:<10} {:<12}",
        "Symbol", "Contract Address", "Dec", "Status", "Risk Flags", "Reviewed", "Bytecode"
    );
    println!("{:-<115}", "");

    for c in &check_results {
        let status = if c.is_verified { "VERIFIED" } else { "UNVERIFIED" };
        let reviewed_str = if c.entry.reviewed { "YES" } else { "NO (pending)" };
        let bc_str = format!("{} bytes", c.bytecode_len);
        let flags_str = c.risk_flags.display_flags();

        println!(
            "{:<8} {:<42} {:<5} {:<10} {:<24} {:<10} {:<12}",
            c.entry.symbol,
            c.entry.address,
            c.entry.decimals,
            status,
            flags_str,
            reviewed_str,
            bc_str
        );
        if c.risk_flags.is_proxy || c.risk_flags.has_beacon || c.risk_flags.is_arb_gateway {
            println!("   └── Architecture: {}", c.risk_flags.proxy_detail());
        }
        if let Some(ref err) = c.error {
            println!("   └── Error: {}", err);
        }
    }
    println!("{:-<115}", "");
    println!("Total tokens in allowlist: {}", check_results.len());
    let verified_count = check_results.iter().filter(|c| c.is_verified).count();
    let reviewed_count = check_results.iter().filter(|c| c.entry.reviewed).count();
    println!("Verified on-chain: {}, Reviewed by user: {}\n", verified_count, reviewed_count);

    // Print tokens skipped because they are unreviewed
    let unreviewed_tokens: Vec<&TokenCheckResult> = check_results
        .iter()
        .filter(|c| !c.entry.reviewed)
        .collect();
    if !unreviewed_tokens.is_empty() {
        println!("Skipped unreviewed tokens (reviewed = false):");
        for t in &unreviewed_tokens {
            println!(
                "  - {:<8} ({}) [on-chain verified: {}]",
                t.entry.symbol, t.entry.address, t.is_verified
            );
        }
        println!();
    }

    // Filter tokens for pool discovery: ONLY verified AND reviewed = true
    let target_tokens: Vec<&TokenCheckResult> = check_results
        .iter()
        .filter(|c| c.is_verified && c.entry.reviewed)
        .collect();

    info!(
        target_tokens = target_tokens.len(),
        min_pool_weth,
        "Initiating pool discovery on Uniswap V3 and PancakeSwap V3 factories for reviewed tokens…"
    );

    let uni_factory_addr: Address = UNI_V3_FACTORY.parse()?;
    let uni_quoter_addr: Address = UNI_V3_QUOTER_V2.parse()?;
    let pancake_factory_addr: Address = PANCAKE_V3_FACTORY.parse()?;
    let pancake_quoter_addr: Address = PANCAKE_V3_QUOTER_V2.parse()?;
    let camelot_factory_addr: Address = CAMELOT_V3_FACTORY.parse()?;
    let camelot_quoter_addr: Address = CAMELOT_V3_QUOTER.parse()?;
    let weth_addr: Address = WETH.parse()?;

    let uni_factory = IUniswapV3Factory::new(uni_factory_addr, http);
    let pancake_factory = IUniswapV3Factory::new(pancake_factory_addr, http);
    let camelot_factory = IAlgebraFactory::new(camelot_factory_addr, http);
    let weth_token = IERC20::new(weth_addr, http);

    // Enabled fee tiers: UniV3 [100, 500, 3000, 10000]; PancakeV3 [100, 500, 2500, 10000]
    let uni_candidate_fees = [100u32, 500, 3000, 10000];
    let pancake_candidate_fees = [100u32, 500, 2500, 10000];

    let min_balance_raw = U256::from((min_pool_weth * 1e18) as u128);

    println!("═══════════════════════════════════════════════════════════════════════════════════════════════════════════");
    println!("                                   POOL DISCOVERY & PRICE IMPACT ANALYSIS                                  ");
    println!("═══════════════════════════════════════════════════════════════════════════════════════════════════════════");

    let mut discovered_markets = Vec::new();

    for tok in target_tokens {
        let token_addr = tok.address;
        let symbol = &tok.entry.symbol;
        let pair = format!("WETH/{symbol}");

        println!("\n── Candidate: {} ({}) ──", pair, token_addr);
        println!(
            "{:<12} {:<42} {:>12} {:>14} {:>14} {:>10}",
            "Venue", "Pool Address", "WETH Depth", "Imp@0.25W", "Imp@1.00W", "Status"
        );
        println!("{:-<100}", "");

        let mut candidate_pools = Vec::new();

        // 1. Check Uniswap V3 fee tiers
        for &fee in &uni_candidate_fees {
            let ts = match uni_factory.feeAmountTickSpacing(U24::from(fee)).call().await {
                Ok(ts) => ts.as_i32(),
                Err(_) => 0,
            };
            if ts <= 0 {
                continue;
            }
            if let Ok(pool_addr) = uni_factory.getPool(weth_addr, token_addr, U24::from(fee)).call().await {
                if pool_addr != Address::ZERO {
                    if let Ok(bal) = weth_token.balanceOf(pool_addr).call().await {
                        let bal_f64 = bal.to::<u128>() as f64 / 1e18;
                        if bal >= min_balance_raw {
                            candidate_pools.push((Venue::UniswapV3 { fee }, fee, pool_addr, uni_quoter_addr, bal, bal_f64));
                        } else {
                            println!(
                                "{:<12} {:<42} {:>12.2} {:>14} {:>14} {:>10}",
                                format!("UniV3-{fee}"), pool_addr, bal_f64, "-", "-", "SHALLOW"
                            );
                        }
                    }
                }
            }
        }

        // 2. Check PancakeSwap V3 fee tiers
        for &fee in &pancake_candidate_fees {
            let ts = match pancake_factory.feeAmountTickSpacing(U24::from(fee)).call().await {
                Ok(ts) => ts.as_i32(),
                Err(_) => 0,
            };
            if ts <= 0 {
                continue;
            }
            if let Ok(pool_addr) = pancake_factory.getPool(weth_addr, token_addr, U24::from(fee)).call().await {
                if pool_addr != Address::ZERO {
                    if let Ok(bal) = weth_token.balanceOf(pool_addr).call().await {
                        let bal_f64 = bal.to::<u128>() as f64 / 1e18;
                        if bal >= min_balance_raw {
                            candidate_pools.push((Venue::PancakeV3 { fee }, fee, pool_addr, pancake_quoter_addr, bal, bal_f64));
                        } else {
                            println!(
                                "{:<12} {:<42} {:>12.2} {:>14} {:>14} {:>10}",
                                format!("Pancake-{fee}"), pool_addr, bal_f64, "-", "-", "SHALLOW"
                            );
                        }
                    }
                }
            }
        }

        // 3. Check Camelot V3 (Algebra)
        if let Ok(pool_addr) = camelot_factory.poolByPair(weth_addr, token_addr).call().await {
            if pool_addr != Address::ZERO {
                if let Ok(bal) = weth_token.balanceOf(pool_addr).call().await {
                    let bal_f64 = bal.to::<u128>() as f64 / 1e18;
                    if bal >= min_balance_raw {
                        candidate_pools.push((Venue::CamelotV3, 0, pool_addr, camelot_quoter_addr, bal, bal_f64));
                    } else {
                        println!(
                            "{:<12} {:<42} {:>12.2} {:>14} {:>14} {:>10}",
                            "Camelot-dyn", pool_addr, bal_f64, "-", "-", "SHALLOW"
                        );
                    }
                }
            }
        }

        // Measure price impact on candidate pools
        let mut measured_pools = Vec::new();
        for (venue, fee, pool_addr, quoter_addr, bal, bal_f64) in candidate_pools {
            let (imp_05, q05) = (0.0, quote_pool_single(http, quoter_addr, venue, fee, weth_addr, token_addr, 0.05).await);
            let q25 = quote_pool_single(http, quoter_addr, venue, fee, weth_addr, token_addr, 0.25).await;
            let q100 = quote_pool_single(http, quoter_addr, venue, fee, weth_addr, token_addr, 1.0).await;

            let imp_25 = match (q05, q25) {
                (Some(out05), Some(out25)) => {
                    let rate05 = out05.to::<u128>() as f64 / 0.05;
                    let rate25 = out25.to::<u128>() as f64 / 0.25;
                    Some(((rate25 - rate05) / rate05) * 10_000.0)
                }
                _ => None,
            };

            let imp_100 = match (q05, q100) {
                (Some(out05), Some(out100)) => {
                    let rate05 = out05.to::<u128>() as f64 / 0.05;
                    let rate100 = out100.to::<u128>() as f64 / 1.0;
                    Some(((rate100 - rate05) / rate05) * 10_000.0)
                }
                _ => None,
            };

            let imp_25_str = imp_25.map(|v| format!("{:+.2} bps", v)).unwrap_or_else(|| "N/A".into());
            let imp_100_str = imp_100.map(|v| format!("{:+.2} bps", v)).unwrap_or_else(|| "N/A".into());

            println!(
                "{:<12} {:<42} {:>12.2} {:>14} {:>14} {:>10}",
                venue.to_string(), pool_addr, bal_f64, imp_25_str, imp_100_str, "KEPT"
            );

            measured_pools.push(DiscoveredPool {
                venue,
                fee,
                pool_address: pool_addr,
                quoter_address: quoter_addr,
                weth_balance_raw: bal,
                weth_balance_f64: bal_f64,
                impact_0_05_bps: imp_05,
                impact_0_25_bps: imp_25,
                impact_1_00_bps: imp_100,
                quote_0_05: q05,
                quote_0_25: q25,
                quote_1_00: q100,
            });
        }

        // Rule: Keep tokens that have pools on at least 2 distinct venue/tier combinations
        if measured_pools.len() >= 2 {
            // Choose 3 trade sizes per market where single-leg impact across kept pools <= 3 bps, capped at 5 WETH.
            let chosen_sizes = select_trade_sizes_for_market(http, weth_addr, token_addr, &measured_pools).await;
            println!("  └── Viable: {} distinct pools! Chosen trade sizes: {:?} WETH", measured_pools.len(), chosen_sizes);

            discovered_markets.push(DiscoveredMarket {
                symbol: symbol.clone(),
                pair,
                token_address: token_addr,
                decimals: tok.entry.decimals,
                reviewed: tok.entry.reviewed,
                pools: measured_pools,
                chosen_sizes_weth: chosen_sizes,
            });
        } else if measured_pools.len() == 1 {
            println!("  └── Excluded: only 1 pool found (requires >= 2 distinct venue/tier combinations)");
        } else {
            println!("  └── Excluded: 0 viable pools with >= {:.1} WETH", min_pool_weth);
        }
    }

    // Assert invariant: only reviewed tokens can ever reach discovered_markets
    for m in &discovered_markets {
        assert!(
            m.reviewed,
            "Invariant violation: unreviewed token '{}' reached discovered_markets",
            m.symbol
        );
    }

    // Write data/markets.json
    let market_configs: Vec<MarketConfig> = discovered_markets
        .iter()
        .map(|m| MarketConfig {
            symbol: m.symbol.clone(),
            pair: m.pair.clone(),
            quote_token: m.token_address,
            quote_decimals: m.decimals,
            venues: m.pools.iter().map(|p| p.venue).collect(),
            trade_sizes_weth: m.chosen_sizes_weth.clone(),
        })
        .collect();

    if let Some(parent) = Path::new(output_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json_bytes = serde_json::to_string_pretty(&market_configs)?;
    std::fs::write(output_path, json_bytes)
        .with_context(|| format!("Failed to write markets file to {}", output_path))?;

    println!("\n═══════════════════════════════════════════════════════════════════════════════════════════════════════════");
    println!("                                   FINAL DISCOVERED MARKETS (Wrote {})", output_path);
    println!("═══════════════════════════════════════════════════════════════════════════════════════════════════════════");
    println!(
        "{:<12} {:<42} {:<32} {:<24}",
        "Pair", "Quote Token", "Venues", "Trade Sizes (WETH)"
    );
    println!("{:-<110}", "");
    for m in &market_configs {
        let venues_str = m.venues.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ");
        let sizes_str = format!("{:?}", m.trade_sizes_weth);
        println!(
            "{:<12} {:<42} {:<32} {:<24}",
            m.pair, m.quote_token, venues_str, sizes_str
        );
    }
    println!("{:-<110}\n", "");

    Ok((check_results, discovered_markets))
}

/// Helper to quote single leg across Uniswap V3, PancakeSwap V3, or Camelot V3.
async fn quote_pool_single<P: Provider>(
    http: &P,
    quoter: Address,
    venue: Venue,
    fee: u32,
    token_in: Address,
    token_out: Address,
    size_weth: f64,
) -> Option<U256> {
    match venue {
        Venue::CamelotV3 => {
            use alloy::primitives::U160;
            use crate::pricing::camelot_v3::IAlgebraQuoter;
            let q = IAlgebraQuoter::new(quoter, http);
            let amount_in = U256::from(weth_to_raw(size_weth));
            q.quoteExactInputSingle(token_in, token_out, amount_in, U160::ZERO)
                .call()
                .await
                .ok()
                .map(|r| r.amountOut)
        }
        _ => quote_single(http, quoter, token_in, token_out, fee, size_weth).await,
    }
}

/// Helper to quote single leg from QuoterV2.
async fn quote_single<P: Provider>(
    http: &P,
    quoter: Address,
    token_in: Address,
    token_out: Address,
    fee: u32,
    size_weth: f64,
) -> Option<U256> {
    let amount_in = U256::from(weth_to_raw(size_weth));
    let q = IQuoterV2::new(quoter, http);
    let params = IQuoterV2::QuoteExactInputSingleParams {
        tokenIn: token_in,
        tokenOut: token_out,
        amountIn: amount_in,
        fee: U24::from(fee),
        sqrtPriceLimitX96: U160::ZERO,
    };
    q.quoteExactInputSingle(params).call().await.ok().map(|r| r.amountOut)
}

/// Select 3 trade sizes per market where single-leg price impact across kept pools is at most 3 bps, capped at 5 WETH.
async fn select_trade_sizes_for_market<P: Provider>(
    http: &P,
    weth: Address,
    token: Address,
    pools: &[DiscoveredPool],
) -> Vec<f64> {
    // Candidate ladder of trade sizes up to 5 WETH
    let candidates = [0.01, 0.02, 0.05, 0.1, 0.25, 0.5, 1.0, 1.5, 2.0, 3.0, 5.0];

    // Measure impact at each candidate size across all pools
    let mut qualifying_sizes = Vec::new();

    for &size in &candidates {
        let mut all_pass = true;
        for p in pools {
            let base_opt = quote_pool_single(http, p.quoter_address, p.venue, p.fee, weth, token, 0.05).await;
            let current_opt = quote_pool_single(http, p.quoter_address, p.venue, p.fee, weth, token, size).await;
            match (base_opt, current_opt) {
                (Some(b), Some(c)) => {
                    let rate_base = b.to::<u128>() as f64 / 0.05;
                    let rate_cur = c.to::<u128>() as f64 / size;
                    let impact_bps = ((rate_cur - rate_base) / rate_base).abs() * 10_000.0;
                    if impact_bps > 3.0 {
                        all_pass = false;
                        break;
                    }
                }
                _ => {
                    all_pass = false;
                    break;
                }
            }
        }
        if all_pass {
            qualifying_sizes.push(size);
        }
    }

    // Pick 3 representative sizes:
    if qualifying_sizes.len() >= 3 {
        // Pick small, middle, and max qualifying size
        let s1 = qualifying_sizes[0];
        let s2 = qualifying_sizes[qualifying_sizes.len() / 2];
        let s3 = *qualifying_sizes.last().unwrap();
        let mut out = vec![s1, s2, s3];
        out.dedup();
        while out.len() < 3 && qualifying_sizes.len() >= 3 {
            out = qualifying_sizes[..3].to_vec();
        }
        out
    } else if !qualifying_sizes.is_empty() {
        // If 1 or 2 sizes qualify, pad with lowest candidates
        let mut out = qualifying_sizes;
        for &c in &[0.02, 0.05, 0.1] {
            if !out.contains(&c) {
                out.push(c);
            }
            if out.len() == 3 {
                break;
            }
        }
        out.sort_by(|a, b| a.partial_cmp(b).unwrap());
        out
    } else {
        // Fallback default safe small sizes
        vec![0.02, 0.05, 0.1]
    }
}
