// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

// Source: https://github.com/Uniswap/v2-periphery/blob/master/contracts/interfaces/IUniswapV2Router02.sol
// SushiSwap V2 uses the same interface.

interface IUniswapV2Router {
    function getAmountsOut(uint256 amountIn, address[] calldata path)
        external
        view
        returns (uint256[] memory amounts);

    function swapExactTokensForTokens(
        uint256        amountIn,
        uint256        amountOutMin,
        address[] calldata path,
        address        to,
        uint256        deadline
    ) external returns (uint256[] memory amounts);
}
