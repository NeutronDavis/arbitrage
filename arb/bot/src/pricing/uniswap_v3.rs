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

use crate::constants::{PANCAKE_V3_FACTORY, PANCAKE_V3_QUOTER_V2, UNI_V3_FACTORY, UNI_V3_QUOTER_V2, USDC, WETH};
use crate::strategy::Venue;

// ── sol! bindings ─────────────────────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    interface IUniswapV3Factory {
        function getPool(address tokenA, address tokenB, uint24 fee)
            external view returns (address pool);
        function feeAmountTickSpacing(uint24 fee)
            external view returns (int24);
    }
}

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
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
        function liquidity() external view returns (uint128 liquidity);
    }
}

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct V3Pool {
    pub venue: Venue,
    pub fee: u32,
    pub address: Address,
    pub quoter: Address,
}

/// Swap direction on the WETH/USDC pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    WethToUsdc,
    UsdcToWeth,
}

// ── Pool discovery ────────────────────────────────────────────────────────────

/// Discover Uniswap V3 pools at startup.
pub async fn discover_uniswap_pools<P: Provider>(
    http: &P,
    fee_tiers: &[u32],
) -> Result<Vec<V3Pool>> {
    let factory_addr: Address = UNI_V3_FACTORY.parse()?;
    let quoter_addr: Address = UNI_V3_QUOTER_V2.parse()?;
    let weth_addr: Address = WETH.parse()?;
    let usdc_addr: Address = USDC.parse()?;
    let factory = IUniswapV3Factory::new(factory_addr, http);

    let mut pools = Vec::new();
    for &fee in fee_tiers {
        let pool_addr = factory
            .getPool(weth_addr, usdc_addr, U24::from(fee))
            .call()
            .await
            .map_err(|e| anyhow!("UniV3 getPool(fee={fee}) failed: {e}"))?;

        if pool_addr == Address::ZERO {
            warn!(fee, "UniV3 pool does not exist — skipping tier");
            continue;
        }
        pools.push(V3Pool {
            venue: Venue::UniswapV3 { fee },
            fee,
            address: pool_addr,
            quoter: quoter_addr,
        });
    }
    Ok(pools)
}

/// Discover PancakeSwap V3 pools at startup from factory for fee tiers 100 and 500.
///
/// Reads enabled fee tiers dynamically from the factory (`feeAmountTickSpacing > 0`).
/// Checks WETH balance of each pool and skips with a warning if under 10 WETH.
pub async fn discover_pancake_pools<P: Provider>(
    http: &P,
    fee_tiers: &[u32],
) -> Result<Vec<V3Pool>> {
    let factory_addr: Address = PANCAKE_V3_FACTORY.parse()?;
    let quoter_addr: Address = PANCAKE_V3_QUOTER_V2.parse()?;
    let weth_addr: Address = WETH.parse()?;
    let usdc_addr: Address = USDC.parse()?;
    let factory = IUniswapV3Factory::new(factory_addr, http);
    let weth_token = IERC20::new(weth_addr, http);

    let min_balance = U256::from(10) * U256::from(10).pow(U256::from(18)); // 10 WETH

    let mut pools = Vec::new();
    for &fee in fee_tiers {
        // 1. Read enabled tiers dynamically from factory
        let tick_spacing = match factory.feeAmountTickSpacing(U24::from(fee)).call().await {
            Ok(ts) => ts,
            Err(e) => {
                let msg = crate::provider::redact_urls(&e.to_string());
                warn!(fee, error = %msg, "Pancake feeAmountTickSpacing check failed — skipping");
                continue;
            }
        };

        let ts_i32: i32 = tick_spacing.as_i32();
        if ts_i32 <= 0 {
            warn!(fee, tick_spacing = ts_i32, "Pancake fee tier is not enabled on factory — skipping");
            continue;
        }

        // 2. Query pool address
        let pool_addr = factory
            .getPool(weth_addr, usdc_addr, U24::from(fee))
            .call()
            .await
            .map_err(|e| anyhow!("Pancake getPool(fee={fee}) failed: {e}"))?;

        if pool_addr == Address::ZERO {
            warn!(fee, "Pancake pool does not exist — skipping tier");
            continue;
        }

        // 3. Skip with a warning any pool whose WETH balance is under 10 WETH
        let weth_balance = weth_token
            .balanceOf(pool_addr)
            .call()
            .await
            .map_err(|e| anyhow!("balanceOf on {pool_addr} failed: {e}"))?;

        if weth_balance < min_balance {
            let bal_f64 = weth_balance.to::<u128>() as f64 / 1e18;
            warn!(
                fee,
                pool = %pool_addr,
                balance_weth = bal_f64,
                "Pancake pool WETH balance under 10 WETH — skipping"
            );
            continue;
        }

        pools.push(V3Pool {
            venue: Venue::PancakeV3 { fee },
            fee,
            address: pool_addr,
            quoter: quoter_addr,
        });
    }
    Ok(pools)
}

/// Backwards-compatible pool discovery helper (discovers Uniswap V3 pools).
#[allow(dead_code)]
pub async fn discover_pools<P: Provider>(
    http: &P,
    fee_tiers: &[u32],
) -> Result<Vec<V3Pool>> {
    discover_uniswap_pools(http, fee_tiers).await
}

// ── Per-block reads (legacy / comparison) ────────────────────────────────────

/// Read in-range liquidity `L` for every pool, concurrently.
/// Returned in the same order as `pools`.
#[allow(dead_code)]
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

/// Exact-input single-hop quote from QuoterV2 (one `eth_call`), parameterized by quoter address.
///
/// Returns `Ok(None)` when the quoter reverts (e.g. not enough liquidity for
/// the size); the caller skips that combination rather than aborting the block.
#[allow(dead_code)]
pub async fn quote<P: Provider>(
    http: &P,
    quoter_addr: Address,
    block: BlockId,
    fee: u32,
    side: Side,
    amount_in: U256,
) -> Result<Option<U256>> {
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
            warn!(fee, ?side, quoter = %quoter_addr, error = %msg, "quoteExactInputSingle failed — skipping");
            Ok(None)
        }
    }
}

// ── Multicall3 helpers ────────────────────────────────────────────────────────

use alloy::sol_types::SolCall;
use crate::multicall::{Call3, MulticallResult};

/// Build a Multicall3 call for `IUniswapV3Pool.liquidity()`.
pub fn build_liquidity_call(pool: Address) -> Call3 {
    let call_data = IUniswapV3Pool::liquidityCall {}.abi_encode();
    Call3 {
        target: pool,
        allowFailure: true,
        callData: call_data.into(),
    }
}

/// Decode the result of an `IUniswapV3Pool.liquidity()` call from Multicall3.
pub fn decode_liquidity_result(res: &MulticallResult) -> Option<u128> {
    if !res.success {
        return None;
    }
    IUniswapV3Pool::liquidityCall::abi_decode_returns(&res.returnData)
        .ok()
}

/// Build a Multicall3 call for `IQuoterV2.quoteExactInputSingle()`, parameterized by quoter address.
pub fn build_quote_call(quoter_addr: Address, fee: u32, side: Side, amount_in: U256) -> Result<Call3> {
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
    let call_data = IQuoterV2::quoteExactInputSingleCall { params }.abi_encode();
    Ok(Call3 {
        target: quoter_addr,
        allowFailure: true,
        callData: call_data.into(),
    })
}

/// Decode the result of an `IQuoterV2.quoteExactInputSingle()` call from Multicall3.
pub fn decode_quote_result(res: &MulticallResult) -> Option<U256> {
    if !res.success {
        return None;
    }
    IQuoterV2::quoteExactInputSingleCall::abi_decode_returns(&res.returnData)
        .ok()
        .map(|r| r.amountOut)
}
