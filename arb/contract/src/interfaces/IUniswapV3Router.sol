// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

// Source: https://github.com/Uniswap/v3-periphery/blob/main/contracts/interfaces/IQuoterV2.sol
// Phase 4 will expand this to the full interface as needed.

interface IUniswapV3Router {
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24  fee;
        address recipient;
        uint256 deadline;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    function exactInputSingle(ExactInputSingleParams calldata params)
        external
        payable
        returns (uint256 amountOut);
}
