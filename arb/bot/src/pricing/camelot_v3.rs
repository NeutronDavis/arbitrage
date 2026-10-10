//! Camelot V3 (Algebra) price fetching via Algebra Quoter and pool state reads.
//!
//! Phase 2h: Algebra V1.9 concentrated liquidity integration on Arbitrum One.
//! Features dynamic pool fees queried off-chain via `IAlgebraQuoter.quoteExactInputSingle`.

use alloy::eips::BlockId;
use alloy::primitives::{Address, U160, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{anyhow, Result};

use crate::constants::{CAMELOT_V3_FACTORY, CAMELOT_V3_QUOTER, WETH};
use crate::multicall::{Call3, MulticallResult};
use crate::pricing::uniswap_v3::Side;
use crate::strategy::Venue;

// ── sol! bindings ─────────────────────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    interface IAlgebraFactory {
        function poolByPair(address tokenA, address tokenB)
            external view returns (address pool);
    }

    #[sol(rpc)]
    interface IAlgebraQuoter {
        function quoteExactInputSingle(
            address tokenIn,
            address tokenOut,
            uint256 amountIn,
            uint160 limitSqrtPrice
        ) external returns (uint256 amountOut, uint16 fee);
    }

    #[sol(rpc)]
    interface IAlgebraPool {
        function liquidity() external view returns (uint128 liquidity);
    }
}

/// A discovered and verified Camelot V3 pool on Arbitrum One.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CamelotPool {
    pub pair: String,
    pub venue: Venue,
    pub address: Address,
    pub quoter: Address,
    pub quote_token: Address,
}

/// Build a Multicall3 `Call3` for `IAlgebraQuoter.quoteExactInputSingle`.
pub fn build_quote_call(
    quoter: Address,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Result<Call3> {
    let call_data = IAlgebraQuoter::quoteExactInputSingleCall {
        tokenIn: token_in,
        tokenOut: token_out,
        amountIn: amount_in,
        limitSqrtPrice: U160::ZERO,
    }
    .abi_encode();

    Ok(Call3 {
        target: quoter,
        allowFailure: true,
        callData: call_data.into(),
    })
}

/// Decode the return data of an `IAlgebraQuoter.quoteExactInputSingle` Multicall3 result.
/// Returns `(amount_out, fee_bps)`.
pub fn decode_quote_result(res: &MulticallResult) -> Option<(U256, f64)> {
    if !res.success {
        return None;
    }
    let decoded = IAlgebraQuoter::quoteExactInputSingleCall::abi_decode_returns(&res.returnData).ok()?;
    let fee_bps = (decoded.fee as f64) / 100.0;
    Some((decoded.amountOut, fee_bps))
}

/// Quote a single swap via Camelot's Algebra Quoter.
/// Returns `(amount_out, fee_bps)` where `fee_bps` is the dynamic fee in basis points.
pub async fn quote<P: Provider>(
    http: &P,
    quoter_addr: Address,
    block: BlockId,
    side: Side,
    amount_in: U256,
    quote_token: Address,
) -> Result<Option<(U256, f64)>> {
    let weth_addr: Address = WETH.parse()?;
    let (token_in, token_out) = match side {
        Side::WethToUsdc | Side::WethToQuote => (weth_addr, quote_token),
        Side::UsdcToWeth | Side::QuoteToWeth => (quote_token, weth_addr),
    };

    let quoter = IAlgebraQuoter::new(quoter_addr, http);
    let call = quoter
        .quoteExactInputSingle(token_in, token_out, amount_in, U160::ZERO)
        .block(block);

    match call.call().await {
        Ok(return_vals) => {
            let fee_bps = (return_vals.fee as f64) / 100.0;
            Ok(Some((return_vals.amountOut, fee_bps)))
        }
        Err(e) => {
            tracing::debug!(
                error = %crate::provider::redact_urls(&e.to_string()),
                "Camelot Algebra quoteExactInputSingle reverted"
            );
            Ok(None)
        }
    }
}

/// Discover Camelot V3 pool for a given market if it exists and has >= 10 WETH balance.
pub async fn discover_camelot_pool<P: Provider>(
    http: &P,
    pair: &str,
    quote_token: Address,
) -> Result<Option<CamelotPool>> {
    let factory_addr: Address = CAMELOT_V3_FACTORY.parse()?;
    let quoter_addr: Address = CAMELOT_V3_QUOTER.parse()?;
    let weth_addr: Address = WETH.parse()?;

    let factory = IAlgebraFactory::new(factory_addr, http);
    let pool_addr = factory
        .poolByPair(weth_addr, quote_token)
        .call()
        .await
        .map_err(|e| anyhow!("Camelot poolByPair failed: {e}"))?;

    if pool_addr == Address::ZERO {
        tracing::info!(pair, "Camelot V3 pool does not exist for market — skipping");
        return Ok(None);
    }

    // Check WETH balance >= 10 WETH
    let weth_token = crate::pricing::uniswap_v3::IERC20::new(weth_addr, http);
    let weth_balance = weth_token
        .balanceOf(pool_addr)
        .call()
        .await
        .map_err(|e| anyhow!("balanceOf on Camelot pool {pool_addr} failed: {e}"))?;

    if !has_sufficient_weth_depth(weth_balance) {
        let bal_f64 = weth_balance.to::<u128>() as f64 / 1e18;
        tracing::warn!(
            pair,
            venue = "Camelot-dyn",
            pool = %pool_addr,
            balance_weth = bal_f64,
            "Camelot pool WETH balance under 10 WETH — skipping"
        );
        return Ok(None);
    }

    Ok(Some(CamelotPool {
        pair: pair.to_string(),
        venue: Venue::CamelotV3,
        address: pool_addr,
        quoter: quoter_addr,
        quote_token,
    }))
}

pub const MIN_WETH_BALANCE_RAW: u128 = 10 * 1_000_000_000_000_000_000;

pub fn has_sufficient_weth_depth(balance: U256) -> bool {
    balance >= U256::from(MIN_WETH_BALANCE_RAW)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_camelot_quote_fixed_fixture() {
        // Return data fixture: uint256 amountOut = 25066581 (25.066581 USDC), uint16 fee = 100 (1 bps)
        // ABI encoding of (uint256, uint16) is two 32-byte words:
        // word 0: 25066581 = 0x017e7c55
        // word 1: 100 = 0x64
        let return_data = alloy::primitives::hex::decode(
            "00000000000000000000000000000000000000000000000000000000017e7c55\
             0000000000000000000000000000000000000000000000000000000000000064",
        )
        .expect("valid hex");

        let res = MulticallResult {
            success: true,
            returnData: return_data.into(),
        };

        let decoded = decode_quote_result(&res).expect("decoding succeeded");
        assert_eq!(decoded.0, U256::from(25066581));
        assert!((decoded.1 - 1.0).abs() < 1e-6); // 100 / 100 = 1.0 bps
    }

    #[test]
    fn test_build_quote_call() {
        let quoter: Address = CAMELOT_V3_QUOTER.parse().unwrap();
        let weth: Address = WETH.parse().unwrap();
        let usdc: Address = crate::constants::USDC.parse().unwrap();
        let amount = U256::from(10_000_000_000_000_000u128); // 0.01 WETH

        let call = build_quote_call(quoter, weth, usdc, amount).expect("build call");
        assert_eq!(call.target, quoter);
        assert!(call.allowFailure);
        // Call data must start with function selector and be 4 + 4*32 = 132 bytes long
        assert_eq!(call.callData.len(), 4 + 32 * 4);
    }

    #[test]
    fn test_discovery_depth_filter() {
        // Less than 10 WETH -> rejected
        assert!(!has_sufficient_weth_depth(U256::from(9_999_999_999_999_999_999u128)));
        // Exactly 10 WETH -> accepted
        assert!(has_sufficient_weth_depth(U256::from(10_000_000_000_000_000_000u128)));
        // More than 10 WETH -> accepted
        assert!(has_sufficient_weth_depth(U256::from(100_000_000_000_000_000_000u128)));
    }
}
