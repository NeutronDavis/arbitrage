// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

// Minimal ERC-20 interface sufficient for the arbitrage contract.
// Phase 4 will use OpenZeppelin's SafeERC20 wrapper instead of calling these directly.

interface IERC20 {
    function totalSupply() external view returns (uint256);
    function balanceOf(address account) external view returns (uint256);
    function transfer(address to, uint256 amount) external returns (bool);
    function allowance(address owner, address spender) external view returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
}
