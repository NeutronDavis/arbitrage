//! Multicall3 contract interface and batch execution helper.
//!
//! Canonical deployment on Arbitrum One: 0xcA11bde05977b3631167028862bE2a173976CA11.

use alloy::eips::BlockId;
use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::sol;
use anyhow::{anyhow, Result};

use crate::constants::MULTICALL3;

sol! {
    #[sol(rpc)]
    interface IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }

        struct Result {
            bool success;
            bytes returnData;
        }

        function aggregate3(Call3[] calldata calls)
            external
            payable
            returns (Result[] memory returnData);

        function getCurrentBlockTimestamp() external view returns (uint256 timestamp);
        function getBlockNumber() external view returns (uint256 blockNumber);
    }
}

pub use IMulticall3::{Call3, Result as MulticallResult};

/// Build a Multicall3 call to fetch the current block timestamp.
pub fn build_timestamp_call() -> Result<Call3> {
    use alloy::sol_types::SolCall;
    let multicall_addr: Address = MULTICALL3.parse()?;
    let call_data = IMulticall3::getCurrentBlockTimestampCall {}.abi_encode();
    Ok(Call3 {
        target: multicall_addr,
        allowFailure: true,
        callData: call_data.into(),
    })
}

/// Decode the timestamp result from Multicall3.
pub fn decode_timestamp_result(res: &MulticallResult) -> Option<u64> {
    use alloy::sol_types::SolCall;
    if !res.success {
        return None;
    }
    IMulticall3::getCurrentBlockTimestampCall::abi_decode_returns(&res.returnData)
        .ok()
        .map(|timestamp| timestamp.to::<u64>())
}

/// Execute a batch of calls using Multicall3 `aggregate3`, pinned to `block`.
/// If a call reverts and `allowFailure == true`, Multicall3 returns `success: false`
/// without aborting the batch.
pub async fn aggregate3<P: Provider>(
    provider: &P,
    calls: Vec<Call3>,
    block: BlockId,
) -> Result<Vec<MulticallResult>> {
    let multicall_addr: Address = MULTICALL3.parse()?;
    let contract = IMulticall3::new(multicall_addr, provider);
    let results = contract
        .aggregate3(calls)
        .block(block)
        .call()
        .await
        .map_err(|e| anyhow!("Multicall3.aggregate3 failed: {e}"))?;
    Ok(results)
}
