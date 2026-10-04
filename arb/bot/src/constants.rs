//! Verified contract addresses for Arbitrum One (chain ID 42161).
//!
//! Every address is annotated with its authoritative source.
//! Addresses marked TODO: VERIFY must be cross-checked on Arbiscan before use
//! in any live transaction.
#![allow(dead_code)] // Phase 1: addresses declared but not yet called. Removed in Phase 2+.


// ── Tokens ────────────────────────────────────────────────────────────────────

/// Wrapped Ether (WETH) on Arbitrum One.
/// Source: https://arbiscan.io/token/0x82aF49447D8a07e3bd95BD0d56f35241523fBab1
pub const WETH: &str = "0x82aF49447D8a07e3bd95BD0d56f35241523fBab1";

/// Native USDC (Circle-issued) on Arbitrum One.
/// NOT USDC.e (bridged). Source: https://arbiscan.io/token/0xaf88d065e77c8cC2239327C5EDb3A432268e5831
/// and https://www.circle.com/blog/usdc-now-available-natively-on-arbitrum
pub const USDC: &str = "0xaf88d065e77c8cC2239327C5EDb3A432268e5831";

// ── Multicall3 ────────────────────────────────────────────────────────────────

/// Multicall3 on Arbitrum One (chain ID 42161).
/// Canonical address deployed across EVM chains via CREATE2.
/// Source: https://github.com/mds1/multicall3/blob/main/deployments.json
/// On-chain verified:
///   Bytecode size: 3808 bytes
///   Codehash: 0xd5c15df687b16f2ff992fc8d767b4216323184a2bbc6ee2f9c398c318e770891 (matches Ethereum mainnet)
///   Multicall3.getChainId() -> 42161
pub const MULTICALL3: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";

// ── Balancer V2 ───────────────────────────────────────────────────────────────

/// Balancer V2 Vault — the flash loan entry-point.
/// Source: https://docs.balancer.fi/reference/contracts/deployment-addresses/arbitrum.html
/// Flash loan fee: currently 0 bps (governance can raise it; verify before production use).
pub const BALANCER_VAULT: &str = "0xBA12222222228d8Ba445958a75a0704d566BF2C8";

// ── Uniswap V3 ────────────────────────────────────────────────────────────────

/// Uniswap V3 Factory on Arbitrum One — used to look up pool addresses via `getPool`.
/// Source: https://developers.uniswap.org/docs/protocols/v3/deployments/v3-arbitrum-deployments
/// On-chain verified 2026-10-01: QuoterV2.factory() and SwapRouter.factory() both return
/// this address, confirming it is the canonical UniswapV3Factory on Arbitrum One.
pub const UNI_V3_FACTORY: &str = "0x1F98431c8aD98523631AE4a59f267346ea31F984";

/// Uniswap V3 QuoterV2 on Arbitrum One — used for off-chain price quotes.
/// Source: https://docs.uniswap.org/contracts/v3/reference/deployments/arbitrum-deployments
pub const UNI_V3_QUOTER_V2: &str = "0x61fFE014bA17989E743c5F6cB21bF9697530B21e";

/// Uniswap V3 SwapRouter (V1) on Arbitrum One — used for on-chain swaps (Phase 4).
/// Source: https://developers.uniswap.org/docs/protocols/v3/deployments/v3-arbitrum-deployments
/// Verified: factory() returns 0x1F98431c8aD98523631AE4a59f267346ea31F984 (UniswapV3Factory). 2026-10-01.
/// Note: SwapRouter02 (0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45) is the newer entrypoint;
///       V1 SwapRouter used here for simpler single-hop calldata in Phase 4.
pub const UNI_V3_SWAP_ROUTER: &str = "0xE592427A0AEce92De3Edee1F18E0157C05861564";

/// Uniswap V3 SwapRouter02 on Arbitrum One — newer universal entrypoint (alternative to V1 above).
/// Source: https://developers.uniswap.org/docs/protocols/v3/deployments/v3-arbitrum-deployments
/// TODO: decide in Phase 4 whether to switch to SwapRouter02 for multi-hop flexibility.
pub const UNI_V3_SWAP_ROUTER_02: &str = "0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45";

/// Uniswap V3 pool fee tiers to check (in hundredths of a bip).
/// 500 = 0.05 %, 3000 = 0.30 %, 10000 = 1.00 %
pub const UNI_V3_FEE_TIERS: [u32; 3] = [500, 3000, 10_000];

// ── PancakeSwap V3 ────────────────────────────────────────────────────────────

/// PancakeSwap V3 Factory on Arbitrum One — used to discover pool addresses via `getPool`.
/// Source: https://docs.pancakeswap.finance/developers/smart-contracts/pancakeswap-exchange/v3-contracts/arbitrum-deployments
/// On-chain verified 2026-10-04:
///   Bytecode size: 10,452 bytes
///   feeAmountTickSpacing(100) -> 1 (enabled)
///   feeAmountTickSpacing(500) -> 10 (enabled)
///   getPool(WETH, USDC, 100) -> 0x7fCDC35463E3770c2fB992716Cd070B63540b947 (holds >100 WETH)
///   getPool(WETH, USDC, 500) -> 0xd9e2a1a61B6E61b275cEc326465d417e52C1b95c (holds >100 WETH)
pub const PANCAKE_V3_FACTORY: &str = "0x0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865";

/// PancakeSwap V3 QuoterV2 on Arbitrum One — used for off-chain price quotes.
/// Source: https://docs.pancakeswap.finance/developers/smart-contracts/pancakeswap-exchange/v3-contracts/arbitrum-deployments
/// On-chain verified 2026-10-04:
///   Bytecode size: 17,292 bytes
///   factory() -> 0x0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865 (matches PANCAKE_V3_FACTORY)
///   WETH9()   -> 0x82aF49447D8a07e3bd95BD0d56f35241523fBab1 (matches WETH constant)
///   ABI matches Uniswap QuoterV2 quoteExactInputSingle exactly.
pub const PANCAKE_V3_QUOTER_V2: &str = "0xB048Bbc1Ee6b733FFfCFb9e9CeF7375518e25997";

/// Candidate PancakeSwap V3 fee tiers to check (100 = 0.01%, 500 = 0.05%).
pub const PANCAKE_V3_FEE_TIERS: [u32; 2] = [100, 500];

// ── SushiSwap V2 ─────────────────────────────────────────────────────────────

/// SushiSwap V2 UniswapV2Router02-compatible router on Arbitrum One.
/// Source: docs.sushi.com, SushiSwap V2 → Router → Arbitrum One.
/// On-chain verified 2026-10-01:
///   cast code       → non-empty (contract exists)
///   factory()       → 0xc35DADB65012eC5796536bD9864eD8773aBc74C4 (matches SUSHI_V2_FACTORY below)
///   WETH()          → 0x82aF49447D8a07e3bd95BD0d56f35241523fBab1 (matches WETH constant)
///   getAmountsOut(1e18 WETH→USDC) → 660140047 (~$660 USDC, shallow pool, price reasonable)
pub const SUSHI_V2_ROUTER: &str = "0x1b02dA8Cb0d097eB8D57A175b88c7D8b47997506";

/// SushiSwap V2 Factory on Arbitrum One.
/// Source: https://github.com/sushiswap/v2-core/blob/master/deployments/arbitrum/UniswapV2Factory.json
/// On-chain verified 2026-10-01:
///   router.factory() → this address
///   getPair(WETH, native USDC) → 0x57b85FEf094e10b5eeCDF350Af688299E9553378 (non-zero ✅)
pub const SUSHI_V2_FACTORY: &str = "0xc35DADB65012eC5796536bD9864eD8773aBc74C4";

/// SushiSwap V2 pair init code hash on Arbitrum One.
/// Used off-chain to compute pair addresses deterministically (pairFor).
/// Source: retrieved via `cast call SUSHI_V2_FACTORY "pairCodeHash()(bytes32)"` on 2026-10-01.
pub const SUSHI_V2_PAIR_INIT_CODE_HASH: &str =
    "0xe18a34eb0e04b04f7a0ac29a6e80748dca96319b42c54d679cb821dca90c6303";

/// SushiSwap V2 WETH/native-USDC pair on Arbitrum One.
/// Source: factory.getPair(WETH, USDC) called on 2026-10-01.
/// Reserves at query time: ~0.324 WETH / ~$874 USDC (shallow pool — factor in when sizing trades).
pub const SUSHI_V2_WETH_USDC_PAIR: &str = "0x57b85FEf094e10b5eeCDF350Af688299E9553378";

// ── Chain ─────────────────────────────────────────────────────────────────────

/// Arbitrum One chain ID.
/// Source: https://chainlist.org/chain/42161
pub const ARBITRUM_CHAIN_ID: u64 = 42161;
