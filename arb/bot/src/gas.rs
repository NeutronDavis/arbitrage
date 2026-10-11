//! Gas model and on-chain fee verification for Arbitrum One.
//!
//! Phase 3a: Profit-potential estimator.
//!
//! Models:
//! - L2 execution: gas_units_assumed * l2_gas_price * eth_usd_price
//! - L1 data fee: Arbitrum NodeInterface gasEstimateL1Component / ArbGasInfo
//! - Flash loan fee: Balancer Vault ProtocolFeesCollector on-chain percentage
//! - Net profit: best_profit_usd - gas_usd - flash_fee_usd

use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::Provider;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use crate::constants::{
    ARB_GAS_INFO, BALANCER_VAULT, NODE_INTERFACE, WETH,
};
use crate::provider::HttpProvider;

alloy::sol! {
    #[sol(rpc)]
    interface IBalancerVault {
        function getProtocolFeesCollector() external view returns (address);
        function getPausedState() external view returns (bool paused, uint32 pauseWindowEndTime, uint32 bufferPeriodEndTime);
    }

    #[sol(rpc)]
    interface IProtocolFeesCollector {
        function getFlashLoanFeePercentage() external view returns (uint256);
    }

    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
    }

    #[sol(rpc)]
    interface INodeInterface {
        function gasEstimateL1Component(address to, bool contractCreation, bytes calldata data)
            external
            returns (uint64 gasEstimateForL1, uint256 baseFee, uint256 l1BaseFeeEstimate);
    }

    #[sol(rpc)]
    interface IArbGasInfo {
        function getPricesInWei() external view returns (uint256, uint256, uint256, uint256, uint256, uint256);
        function getL1BaseFeeEstimate() external view returns (uint256);
    }
}

/// Breakdown of costs for an evaluated trade size.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GasCostBreakdown {
    /// L2 execution gas cost in USD.
    pub l2_gas_usd: f64,
    /// L1 calldata posting fee in USD.
    pub l1_data_fee_usd: f64,
    /// Total gas cost (L2 + L1) in USD.
    pub total_gas_usd: f64,
    /// Flash loan fee in USD.
    pub flash_fee_usd: f64,
    /// Net profit in USD = gross_profit_usd - total_gas_usd - flash_fee_usd.
    pub net_profit_usd: f64,
}

/// Configurable gas model holding active parameters and on-chain readings.
#[derive(Debug, Clone)]
pub struct GasModel {
    /// Assumed L2 execution units (default 500,000).
    pub gas_units_assumed: u64,
    /// Assumed calldata length in bytes (default 1,000).
    #[allow(dead_code)]
    pub calldata_bytes_assumed: usize,
    /// Flash loan fee in basis points (default 0.0).
    pub flash_loan_fee_bps: f64,
    /// Current L2 gas price in wei from node.
    pub l2_gas_price_wei: u128,
    /// Estimated L1 data fee in wei for calldata_bytes_assumed.
    pub l1_fee_wei: u128,
}

impl GasModel {
    /// Construct gas model with explicit values.
    pub fn new(
        gas_units_assumed: u64,
        calldata_bytes_assumed: usize,
        flash_loan_fee_bps: f64,
        l2_gas_price_wei: u128,
        l1_fee_wei: u128,
    ) -> Self {
        Self {
            gas_units_assumed,
            calldata_bytes_assumed,
            flash_loan_fee_bps,
            l2_gas_price_wei,
            l1_fee_wei,
        }
    }

    /// Compute detailed cost breakdown and net profit.
    pub fn compute_costs(
        &self,
        size_weth: f64,
        gross_profit_usd: f64,
        eth_usd_price: f64,
    ) -> GasCostBreakdown {
        // L2 execution fee in USD
        let l2_cost_wei = (self.gas_units_assumed as u128).saturating_mul(self.l2_gas_price_wei);
        let l2_gas_usd = (l2_cost_wei as f64 / 1e18) * eth_usd_price;

        // L1 data fee in USD
        let l1_data_fee_usd = (self.l1_fee_wei as f64 / 1e18) * eth_usd_price;
        let total_gas_usd = l2_gas_usd + l1_data_fee_usd;

        // Flash loan fee: size_weth * (fee_bps / 10,000) * eth_usd_price
        let flash_fee_usd = (self.flash_loan_fee_bps / 10_000.0) * size_weth * eth_usd_price;

        let net_profit_usd = gross_profit_usd - total_gas_usd - flash_fee_usd;

        GasCostBreakdown {
            l2_gas_usd,
            l1_data_fee_usd,
            total_gas_usd,
            flash_fee_usd,
            net_profit_usd,
        }
    }
}

/// Information retrieved from on-chain Balancer Vault check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalancerVaultStatus {
    pub vault_address: String,
    pub collector_address: String,
    pub flash_loan_fee_bps: f64,
    pub weth_balance_wei: U256,
    pub weth_balance_f64: f64,
    pub is_paused: bool,
}

/// Query on-chain Balancer Vault status, ProtocolFeesCollector, and WETH balance.
pub async fn query_balancer_status(provider: &HttpProvider) -> Result<BalancerVaultStatus> {
    let vault_addr: Address = BALANCER_VAULT.parse()?;
    let weth_addr: Address = WETH.parse()?;

    // 1. Get ProtocolFeesCollector
    let vault = IBalancerVault::new(vault_addr, provider);
    let collector_addr = vault
        .getProtocolFeesCollector()
        .call()
        .await
        .context("IBalancerVault.getProtocolFeesCollector call failed")?;

    // 2. Get flash loan fee percentage from collector
    let collector = IProtocolFeesCollector::new(collector_addr, provider);
    let fee_pct_raw = collector
        .getFlashLoanFeePercentage()
        .call()
        .await
        .context("IProtocolFeesCollector.getFlashLoanFeePercentage call failed")?;

    // Balancer returns 18-decimal fraction (e.g. 0 = 0 bps, 1e16 = 1% = 100 bps)
    let fee_bps = (fee_pct_raw.to::<u128>() as f64 / 1e18) * 10_000.0;

    // 3. Check paused state
    let paused_state = vault
        .getPausedState()
        .call()
        .await
        .context("IBalancerVault.getPausedState call failed")?;
    let is_paused = paused_state.paused;

    // 4. Check Vault WETH balance
    let weth = IERC20::new(weth_addr, provider);
    let balance = weth
        .balanceOf(vault_addr)
        .call()
        .await
        .context("IERC20.balanceOf(BALANCER_VAULT) call failed")?;

    let balance_f64 = balance.to::<u128>() as f64 / 1e18;

    Ok(BalancerVaultStatus {
        vault_address: vault_addr.to_string(),
        collector_address: collector_addr.to_string(),
        flash_loan_fee_bps: fee_bps,
        weth_balance_wei: balance,
        weth_balance_f64: balance_f64,
        is_paused,
    })
}

/// Query L1 calldata fee using NodeInterface with fallback to ArbGasInfo.
pub async fn query_l1_data_fee_estimate(
    provider: &HttpProvider,
    calldata_bytes: usize,
) -> Result<u128> {
    let node_iface_addr: Address = NODE_INTERFACE.parse()?;
    let node_iface = INodeInterface::new(node_iface_addr, provider);

    let dummy_data = Bytes::from(vec![0u8; calldata_bytes]);
    match node_iface
        .gasEstimateL1Component(Address::ZERO, false, dummy_data)
        .call()
        .await
    {
        Ok(res) => {
            let gas_l1 = res.gasEstimateForL1 as u128;
            let base_fee = res.baseFee.to::<u128>();
            Ok(gas_l1.saturating_mul(base_fee))
        }
        Err(_) => {
            // Fallback to ArbGasInfo precompile
            let arb_gas_addr: Address = ARB_GAS_INFO.parse()?;
            let arb_gas = IArbGasInfo::new(arb_gas_addr, provider);
            let prices = arb_gas
                .getPricesInWei()
                .call()
                .await
                .context("IArbGasInfo.getPricesInWei call failed")?;

            let per_l2_tx = prices._0.to::<u128>();
            let per_l1_byte = prices._1.to::<u128>();
            let fee = per_l2_tx.saturating_add((calldata_bytes as u128).saturating_mul(per_l1_byte));
            Ok(fee)
        }
    }
}

/// Query current L2 gas price from node.
pub async fn query_l2_gas_price(provider: &HttpProvider) -> Result<u128> {
    let gas_price = provider
        .get_gas_price()
        .await
        .context("get_gas_price failed")?;
    Ok(gas_price)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn test_gas_arithmetic_fixed_inputs() {
        let model = GasModel::new(
            500_000,       // gas_units_assumed
            1_000,         // calldata_bytes_assumed
            0.0,           // flash_loan_fee_bps
            20_000_000,    // 0.02 gwei L2 gas price
            7_240_000_000, // L1 data fee in wei
        );

        let eth_usd_price = 2500.0;
        let size_weth = 0.01;
        let gross_profit_usd = 0.25;

        let costs = model.compute_costs(size_weth, gross_profit_usd, eth_usd_price);

        // L2 cost: 500,000 * 20,000,000 = 10_000_000_000_000 wei = 1e13 wei = 1e-5 ETH.
        // 1e-5 * 2500 = $0.025
        assert!((costs.l2_gas_usd - 0.025).abs() < 1e-6);

        // L1 data cost: 7_240_000_000 / 1e18 * 2500 = 7.24e-9 * 2500 = 0.0000181
        assert!((costs.l1_data_fee_usd - 0.0000181).abs() < 1e-6);

        // Total gas: 0.025 + 0.0000181 = 0.0250181
        assert!((costs.total_gas_usd - 0.0250181).abs() < 1e-6);

        // Flash fee: 0.0 bps = $0.0
        assert_eq!(costs.flash_fee_usd, 0.0);

        // Net profit: 0.25 - 0.0250181 - 0.0 = 0.2249819
        assert!((costs.net_profit_usd - 0.2249819).abs() < 1e-6);
    }

    #[test]
    fn test_gas_arithmetic_with_flash_loan_fee() {
        let model = GasModel::new(
            500_000,
            1_000,
            5.0, // 5.0 bps flash fee
            20_000_000,
            0,
        );

        let eth_usd_price = 2000.0;
        let size_weth = 1.0;
        let gross_profit_usd = 5.0;

        let costs = model.compute_costs(size_weth, gross_profit_usd, eth_usd_price);

        // Flash fee: 1.0 * (5.0 / 10,000) * 2000 = 1.0 USD
        assert!((costs.flash_fee_usd - 1.0).abs() < 1e-6);

        // L2 gas: 500,000 * 20,000,000 / 1e18 * 2000 = 0.02 USD
        assert!((costs.l2_gas_usd - 0.02).abs() < 1e-6);

        // Net profit: 5.0 - 0.02 - 1.0 = 3.98 USD
        assert!((costs.net_profit_usd - 3.98).abs() < 1e-6);
    }
}
