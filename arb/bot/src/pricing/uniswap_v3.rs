//! Uniswap V3 price fetching via QuoterV2 and pool state reads.
//!
//! Phase 2 implementation.
//!
//! Per block this module performs:
//!   - one `liquidity()` read per discovered pool (concurrently), and
//!   - `quoteExactInputSingle` calls requested by the strategy layer.
//!
//! `slot0()` is intentionally not read per block: nothing in Phase 2 consumes
//! it, and dropping it saves one RPC call per pool per processed block.

use alloy::eips::BlockId;
use alloy::primitives::{Address, U160, U256};
use alloy::primitives::aliases::U24;
use alloy::providers::Provider;
use alloy::sol;
use anyhow::{anyhow, Result};
use futures_util::future::try_join_all;
use tracing::warn;

use crate::constants::{UNI_V3_FACTORY, UNI_V3_QUOTER_V2, USDC, WETH};

// ── sol! bindings ─────────────────────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    interface IUniswapV3Factory {
        function getPool(address tokenA, address tokenB, uint24 fee)
            external view returns (address pool);
    }
}

sol! {
    #[sol(rpc)]
    interface IQuoterV2 {
        struct QuoteExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint256 amountIn;
            uint24  fee;
            uint160 sqrtPriceLimitX96;
        }
        function quoteExactInputSingle(QuoteExactInputSingleParams memory params)
            external
            returns (
                uint256 amountOut,
                uint160 sqrtPriceX96After,
                uint32  initializedTicksCrossed,
                uint256 gasEstimate
            );
    }
}

sol! {
    #[sol(rpc)]
    interface IUniswapV3Pool {
        function liquidity() external view returns (uint128);
    }
}

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct V3Pool {
    pub fee: u32,
    pub address: Address,
}

/// Swap direction on the WETH/USDC pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    WethToUsdc,
    UsdcToWeth,
}

// ── Pool discovery ────────────────────────────────────────────────────────────

pub async fn discover_pools<P: Provider>(
    http: &P,
    fee_tiers: &[u32],
) -> Result<Vec<V3Pool>> {
    let factory_addr: Address = UNI_V3_FACTORY.parse()?;
    let weth_addr: Address = WETH.parse()?;
    let usdc_addr: Address = USDC.parse()?;
    let factory = IUniswapV3Factory::new(factory_addr, http);

    let mut pools = Vec::new();
    for &fee in fee_tiers {
        let pool_addr = factory
            .getPool(weth_addr, usdc_addr, U24::from(fee))
            .call()
            .await
            .map_err(|e| anyhow!("getPool(fee={fee}) failed: {e}"))?;

        if pool_addr == Address::ZERO {
            warn!(fee, "UniV3 pool does not exist — skipping tier");
            continue;
        }
        pools.push(V3Pool { fee, address: pool_addr });
    }
    Ok(pools)
}

// ── Per-block reads ───────────────────────────────────────────────────────────

/// Read in-range liquidity `L` for every pool, concurrently.
/// Returned in the same order as `pools`.
pub async fn fetch_liquidity<P: Provider>(
    http: &P,
    pools: &[V3Pool],
    block: BlockId,
) -> Result<Vec<u128>> {
    try_join_all(pools.iter().map(|pool| async move {
        IUniswapV3Pool::new(pool.address, http)
            .liquidity()
            .block(block)
            .call()
            .await
            .map_err(|e| anyhow!("liquidity() on {}: {e}", pool.address))
    }))
    .await
}

/// Exact-input single-hop quote from QuoterV2 (one `eth_call`).
///
/// Returns `Ok(None)` when the quoter reverts (e.g. not enough liquidity for
/// the size); the caller skips that combination rather than aborting the block.
pub async fn quote<P: Provider>(
    http: &P,
    block: BlockId,
    fee: u32,
    side: Side,
    amount_in: U256,
) -> Result<Option<U256>> {
    let quoter_addr: Address = UNI_V3_QUOTER_V2.parse()?;
    let weth_addr: Address = WETH.parse()?;
    let usdc_addr: Address = USDC.parse()?;
    let (token_in, token_out) = match side {
        Side::WethToUsdc => (weth_addr, usdc_addr),
        Side::UsdcToWeth => (usdc_addr, weth_addr),
    };
    let params = IQuoterV2::QuoteExactInputSingleParams {
        tokenIn: token_in,
        tokenOut: token_out,
        amountIn: amount_in,
        fee: U24::from(fee),
        sqrtPriceLimitX96: U160::ZERO,
    };
    match IQuoterV2::new(quoter_addr, http)
        .quoteExactInputSingle(params)
        .block(block)
        .call()
        .await
    {
        Ok(out) => Ok(Some(out.amountOut)),
        Err(e) => {
            // Error text may embed the transport URL; redact before logging.
            let msg = crate::provider::redact_urls(&e.to_string());
            warn!(fee, ?side, error = %msg, "quoteExactInputSingle failed — skipping");
            Ok(None)
        }
    }
}
