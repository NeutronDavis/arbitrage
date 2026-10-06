//! Pricing sub-modules for each venue, plus shared unit helpers.
//!
//! Decimals: WETH = 18, USDC = 6 (Arbitrum native USDC).

pub mod sushi_v2;
pub mod uniswap_v3;

/// Whole WETH (f64) -> raw 18-decimal units.
pub fn weth_to_raw(amount: f64) -> u128 {
    (amount * 1e18).round() as u128
}

/// Raw 18-decimal WETH -> whole WETH.
#[allow(dead_code)]
pub fn raw_to_weth(raw: u128) -> f64 {
    raw as f64 / 1e18
}

/// USD per ETH implied by swapping `weth_in_raw` (18 dec) for `usdc_out_raw` (6 dec).
#[allow(dead_code)]
pub fn usd_per_eth(weth_in_raw: u128, usdc_out_raw: u128) -> f64 {
    if weth_in_raw == 0 {
        return 0.0;
    }
    (usdc_out_raw as f64 / 1e6) / (weth_in_raw as f64 / 1e18)
}

/// Implied rate of quote tokens per 1 WETH given raw inputs and decimals.
pub fn implied_quote_per_weth(weth_in_raw: u128, quote_out_raw: u128, quote_decimals: u8) -> f64 {
    if weth_in_raw == 0 {
        return 0.0;
    }
    (quote_out_raw as f64 / 10f64.powi(quote_decimals as i32)) / (weth_in_raw as f64 / 1e18)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_round_trip() {
        assert_eq!(weth_to_raw(0.01), 10_000_000_000_000_000);
        assert_eq!(weth_to_raw(0.1), 100_000_000_000_000_000);
        assert!((raw_to_weth(50_000_000_000_000_000) - 0.05).abs() < 1e-12);
    }

    #[test]
    fn usd_price_respects_decimals() {
        // 0.1 WETH -> 268.29 USDC  =>  2682.9 USD/ETH
        let p = usd_per_eth(100_000_000_000_000_000, 268_290_000);
        assert!((p - 2682.9).abs() < 1e-6, "p={p}");
        assert_eq!(usd_per_eth(0, 1), 0.0);
    }
}
