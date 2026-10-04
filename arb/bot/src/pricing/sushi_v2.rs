//! SushiSwap V2 price fetching via getReserves + constant-product formula.
//!
//! Phase 2 implementation.

use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::sol;
use anyhow::{anyhow, Result};

use crate::constants::{SUSHI_V2_ROUTER, SUSHI_V2_WETH_USDC_PAIR, USDC, WETH};

// ── sol! bindings ─────────────────────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    interface IUniswapV2Pair {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function getReserves() external view
            returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    }
}

sol! {
    #[sol(rpc)]
    interface IUniswapV2Router {
        function getAmountsOut(uint256 amountIn, address[] calldata path)
            external view returns (uint256[] memory amounts);
    }
}

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct PairMeta {
    pub weth_is_token0: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Reserves {
    pub reserve_weth: u128,
    pub reserve_usdc: u128,
}

impl Reserves {
    /// Local quote, WETH in -> USDC out (identical to UniswapV2Library.getAmountOut).
    pub fn weth_to_usdc(&self, weth_in: u128) -> u128 {
        cpf_amount_out(weth_in, self.reserve_weth, self.reserve_usdc)
    }
    /// Local quote, USDC in -> WETH out.
    pub fn usdc_to_weth(&self, usdc_in: u128) -> u128 {
        cpf_amount_out(usdc_in, self.reserve_usdc, self.reserve_weth)
    }
}

/// Result of comparing the local CPF quote with `router.getAmountsOut`.
#[derive(Debug, Clone, Copy)]
pub struct CrossCheck {
    pub amount_in: u128,
    pub local_out: u128,
    pub router_out: u128,
}

// ── Startup: determine token order ────────────────────────────────────────────

pub async fn get_pair_meta<P: Provider>(http: &P) -> Result<PairMeta> {
    let pair_addr: Address = SUSHI_V2_WETH_USDC_PAIR.parse()?;
    let weth_addr: Address = WETH.parse()?;
    let pair = IUniswapV2Pair::new(pair_addr, http);
    let token0 = pair
        .token0()
        .call()
        .await
        .map_err(|e| anyhow!("pair.token0() failed: {e}"))?;
    Ok(PairMeta { weth_is_token0: token0 == weth_addr })
}

// ── Per-block reads ───────────────────────────────────────────────────────────

pub async fn get_reserves<P: Provider>(
    http: &P,
    meta: PairMeta,
    block: BlockId,
) -> Result<Reserves> {
    let pair_addr: Address = SUSHI_V2_WETH_USDC_PAIR.parse()?;
    let pair = IUniswapV2Pair::new(pair_addr, http);
    let res = pair
        .getReserves()
        .block(block)
        .call()
        .await
        .map_err(|e| anyhow!("getReserves() failed: {e}"))?;

    let (reserve_weth, reserve_usdc) = if meta.weth_is_token0 {
        (res.reserve0.to::<u128>(), res.reserve1.to::<u128>())
    } else {
        (res.reserve1.to::<u128>(), res.reserve0.to::<u128>())
    };
    Ok(Reserves { reserve_weth, reserve_usdc })
}

// ── Constant-product formula ──────────────────────────────────────────────────

/// Uniswap V2 / SushiSwap constant-product formula with 0.3% fee (997/1000).
pub fn cpf_amount_out(amount_in: u128, reserve_in: u128, reserve_out: u128) -> u128 {
    if reserve_in == 0 || reserve_out == 0 || amount_in == 0 {
        return 0;
    }
    // Use u256-width arithmetic via u128; values fit well within u128 for typical pool sizes.
    // For large amounts, use checked arithmetic to detect overflow.
    let num = amount_in
        .checked_mul(997)
        .and_then(|v| v.checked_mul(reserve_out));
    let den = reserve_in
        .checked_mul(1000)
        .and_then(|v| v.checked_add(amount_in.checked_mul(997)?));

    match (num, den) {
        (Some(n), Some(d)) if d > 0 => n / d,
        _ => 0,
    }
}

// ── Router cross-check ────────────────────────────────────────────────────────

/// Compare the local CPF quote with the deployed router's `getAmountsOut`
/// (WETH -> USDC). Dry-run only: costs one extra RPC call.
///
/// Pinned to the same block as `reserves`, so the two must match exactly.
pub async fn cross_check_router<P: Provider>(
    http: &P,
    reserves: &Reserves,
    weth_in: u128,
    block: BlockId,
) -> Result<CrossCheck> {
    let router_addr: Address = SUSHI_V2_ROUTER.parse()?;
    let weth_addr: Address = WETH.parse()?;
    let usdc_addr: Address = USDC.parse()?;
    let router = IUniswapV2Router::new(router_addr, http);
    let router_amounts = router
        .getAmountsOut(U256::from(weth_in), vec![weth_addr, usdc_addr])
        .block(block)
        .call()
        .await
        .map_err(|e| anyhow!("router.getAmountsOut cross-check failed: {e}"))?;

    let router_out = router_amounts
        .get(1)
        .ok_or_else(|| anyhow!("router.getAmountsOut returned fewer than 2 amounts"))?
        .to::<u128>();

    Ok(CrossCheck {
        amount_in: weth_in,
        local_out: reserves.weth_to_usdc(weth_in),
        router_out,
    })
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpf_weth_to_usdc_small_trade() {
        let reserve_weth: u128 = 323_981_445_350_387_315;
        let reserve_usdc: u128 = 874_656_724;
        let amount_in: u128 = 10_000_000_000_000_000; // 0.01 WETH
        let out = cpf_amount_out(amount_in, reserve_weth, reserve_usdc);
        assert!(out > 0, "amount out must be positive");
        assert!(out < reserve_usdc, "cannot drain more than the reserve");
    }

    #[test]
    fn cpf_zero_inputs_return_zero() {
        assert_eq!(cpf_amount_out(0, 1_000, 1_000), 0);
        assert_eq!(cpf_amount_out(1_000, 0, 1_000), 0);
        assert_eq!(cpf_amount_out(1_000, 1_000, 0), 0);
    }

    #[test]
    fn cpf_symmetric_sanity() {
        let reserve: u128 = 1_000_000;
        let amount_in: u128 = 1_000_000;
        let out = cpf_amount_out(amount_in, reserve, reserve);
        // out = 1000000 * 997 * 1000000 / (1000000 * 1000 + 1000000 * 997)
        //     = 997_000_000_000_000 / 1_997_000_000 = 499_248.87 -> floor = 499_248
        // (integer floor division, identical to UniswapV2Library.getAmountOut)
        assert_eq!(out, 499_248);
    }
}
