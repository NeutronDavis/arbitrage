// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

// Phase 4 will implement:
//
//   contract Arbitrage is IFlashLoanRecipient {
//       IBalancerVault public immutable vault;
//
//       constructor(address _vault) { vault = IBalancerVault(_vault); }
//
//       /// @notice Owner-only entry point. Initiates a Balancer flash loan.
//       function execute(...) external onlyOwner { ... }
//
//       /// @notice Balancer vault callback. Performs the two-leg swap.
//       function receiveFlashLoan(...) external override { ... }
//
//       /// @notice Withdraw any tokens stuck in the contract.
//       function withdraw(address token, uint256 amount) external onlyOwner { ... }
//   }
