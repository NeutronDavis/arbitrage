# Arbitrum WETH Arbitrage Bot

Flash-loan arbitrage bot targeting WETH/USDC price discrepancies between
Uniswap V3 (multiple fee tiers) and SushiSwap V2 on Arbitrum One (chain ID 42161).

## Architecture

```
arb/
├── contract/   Solidity smart contract (Foundry)
└── bot/        Off-chain detection and execution engine (Rust + Alloy)
```

## Quick-start

### Prerequisites
- Rust (stable ≥ 1.75) — <https://rustup.rs>
- Foundry — <https://getfoundry.sh>
- An Arbitrum RPC endpoint (Alchemy / Infura / Ankr)

### Setup

```bash
cp bot/.env.example bot/.env
# Edit bot/.env and fill in real values
```

### Run the bot (Phase 2+)

```bash
cd bot
cargo run
```

### Run Solidity tests (Phase 4+)

```bash
cd contract
forge test --fork-url $ARBITRUM_RPC_URL -vv
```

## Contract addresses (Arbitrum One, chain ID 42161)

See `bot/src/constants.rs` for a full annotated list.

| Protocol | Contract | Address | Status |
|---|---|---|---|
| WETH | ERC-20 | `0x82aF49447D8a07e3bd95BD0d56f35241523fBab1` | ✅ VERIFIED |
| USDC (native) | ERC-20 | `0xaf88d065e77c8cC2239327C5EDb3A432268e5831` | ✅ VERIFIED |
| Balancer V2 Vault | Flash loans | `0xBA12222222228d8Ba445958a75a0704d566BF2C8` | ✅ VERIFIED |
| Uniswap V3 QuoterV2 | Price quotes | `0x61fFE014bA17989E743c5F6cB21bF9697530B21e` | ✅ VERIFIED |
| Uniswap V3 SwapRouter (V1) | Swap execution | `0xE592427A0AEce92De3Edee1F18E0157C05861564` | ✅ VERIFIED |
| Uniswap V3 SwapRouter02 | Swap execution (alt) | `0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45` | ✅ VERIFIED |
| SushiSwap V2 Router | Swap execution | `0x1b02dA8Cb0d097eB8D57A175b88c7D8b47997506` | ✅ VERIFIED |
| SushiSwap V2 Factory | Pair lookup | `0xc35DADB65012eC5796536bD9864eD8773aBc74C4` | ✅ VERIFIED |
| SushiSwap V2 WETH/USDC Pair | Pool | `0x57b85FEf094e10b5eeCDF350Af688299E9553378` | ✅ VERIFIED |

## Security

- Private keys are **never** hardcoded. Load from `.env` only.
- `.env` is in `.gitignore`. Only `.env.example` (with placeholders) is committed.
- `EXECUTION_ENABLED=false` by default — no transactions are sent unless explicitly enabled.
- The Solidity contract reverts atomically if net profit < `minProfit`.

## Phases

| Phase | Status | Description |
|---|---|---|
| 1 | ✅ Scaffold | Folder structure, deps, empty modules |
| 2 | 🔜 | Read-only price logger |
| 3 | 🔜 | Profit model (log-only) |
| 4 | 🔜 | Solidity contract + fork tests |
| 5 | 🔜 | Execution (disabled by default) |

## License

MIT
