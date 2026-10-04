// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

// Phase 4 fork tests.
// Tests will cover:
//   1. Profitable arbitrage — succeeds and returns profit to owner.
//   2. Unprofitable arbitrage — reverts because balance < borrowed + minProfit.
//   3. Unauthorized caller — reverts when receiveFlashLoan is called by non-vault.
//
// Run with: forge test --fork-url $ARBITRUM_RPC_URL -vv

contract ArbitrageTest {
    // Phase 4 implementation goes here.
}
