//! Environment loading and validated configuration.
//!
//! Phase 2c: PancakeSwap V3 integration. Optional variables with documented defaults:
//!   VENUES              — comma-separated venues (default "UniV3-500,UniV3-3000,Pancake-100,Pancake-500")
//!   LOG_EVERY_N_BLOCKS  — process every Nth block (default 40)
//!   TRADE_SIZES_WETH    — comma-separated WETH amounts to quote (default "0.05,0.1,0.25,0.5")
//!   OUTPUT_FILE         — path for JSONL opportunity log (default "data/opportunities.jsonl")
//!   WATCHDOG_SECS       — watchdog timeout in seconds (default 90)

use alloy::primitives::Address;
use anyhow::{anyhow, Context, Result};
use crate::constants::{USDC, USDT, WBTC};
use crate::strategy::Venue;

/// Configuration for a specific WETH/token market.
#[derive(Debug, Clone, PartialEq)]
pub struct MarketConfig {
    /// Token symbol, e.g. "USDC", "WBTC", "USDT".
    pub symbol: String,
    /// Canonical pair label, e.g. "WETH/USDC", "WETH/WBTC", "WETH/USDT".
    pub pair: String,
    /// Token contract address.
    pub quote_token: Address,
    /// Token decimals (e.g. 6 for USDC/USDT, 8 for WBTC).
    pub quote_decimals: u8,
    /// Active venues to quote and log for this market.
    pub venues: Vec<Venue>,
    /// WETH amounts to quote for this market.
    pub trade_sizes_weth: Vec<f64>,
}

/// All runtime configuration loaded from environment variables.
///
/// Deliberately not `Clone` — configuration should not be copied around freely.
/// `Debug` is implemented by hand below so the RPC URLs can never be printed.
pub struct Config {
    /// HTTP RPC URL for one-shot JSON-RPC calls.
    /// Never logged or printed — treat as a secret.
    pub rpc_http: String,
    /// WebSocket RPC URL for block subscriptions.
    /// Never logged or printed — treat as a secret.
    pub rpc_ws: String,
    /// Whether the bot may submit real transactions (Phase 5).
    pub execution_enabled: bool,
    /// Minimum net profit in wei before the bot acts.
    #[allow(dead_code)] // used in Phase 5 executor
    pub min_profit_wei: u128,
    /// Process every Nth block. Reduces RPC usage at the cost of latency.
    pub log_every_n_blocks: u64,
    /// Active markets configured for logging.
    pub markets: Vec<MarketConfig>,
    /// Path for the JSONL opportunity log file.
    pub output_file: String,
    /// Watchdog timeout in seconds. If no sampled block is processed for this long,
    /// the bot logs an ERROR and exits with a non-zero code. Default is 90 seconds.
    pub watchdog_secs: u64,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("rpc_http", &"<redacted>")
            .field("rpc_ws", &"<redacted>")
            .field("execution_enabled", &self.execution_enabled)
            .field("min_profit_wei", &self.min_profit_wei)
            .field("log_every_n_blocks", &self.log_every_n_blocks)
            .field("markets", &self.markets)
            .field("output_file", &self.output_file)
            .field("watchdog_secs", &self.watchdog_secs)
            .finish()
    }
}

impl Config {
    /// Load and validate configuration from the environment.
    ///
    /// Reads a `.env` file if present (via `dotenvy`).
    /// Returns an error if any required variable is missing or malformed.
    ///
    /// # Note
    /// `PRIVATE_KEY` is intentionally **not** loaded here — it is only read
    /// in `executor.rs` and only when `execution_enabled == true`.
    pub fn from_env() -> Result<Self> {
        // dotenvy silently ignores a missing .env — that is intentional.
        let _ = dotenvy::dotenv();

        let rpc_http = std::env::var("ARBITRUM_RPC_HTTP")
            .map_err(|_| anyhow!("ARBITRUM_RPC_HTTP not set"))?;

        let rpc_ws = std::env::var("ARBITRUM_RPC_WS")
            .map_err(|_| anyhow!("ARBITRUM_RPC_WS not set"))?;

        let execution_enabled = std::env::var("EXECUTION_ENABLED")
            .unwrap_or_else(|_| "false".into())
            .trim()
            .eq_ignore_ascii_case("true");

        let min_profit_wei: u128 = std::env::var("MIN_PROFIT_WEI")
            .unwrap_or_else(|_| "1000000000000000".into()) // 0.001 WETH default
            .trim()
            .parse()
            .map_err(|e| anyhow!("MIN_PROFIT_WEI parse error: {e}"))?;

        let log_every_n_blocks: u64 = std::env::var("LOG_EVERY_N_BLOCKS")
            .unwrap_or_else(|_| "40".into())
            .trim()
            .parse()
            .context("LOG_EVERY_N_BLOCKS must be a positive integer")?;

        if log_every_n_blocks == 0 {
            return Err(anyhow!("LOG_EVERY_N_BLOCKS must be > 0"));
        }

        // Optional VENUES and TRADE_SIZES_WETH overrides for WETH/USDC (backwards compatibility)
        let custom_usdc_venues: Option<Vec<Venue>> = match std::env::var("VENUES") {
            Ok(s) if !s.trim().is_empty() => {
                let v = s
                    .split(',')
                    .map(|x| x.trim().parse::<Venue>())
                    .collect::<Result<Vec<_>>>()?;
                Some(v)
            }
            _ => None,
        };

        let custom_usdc_sizes: Option<Vec<f64>> = match std::env::var("TRADE_SIZES_WETH") {
            Ok(s) if !s.trim().is_empty() => {
                let sz = s
                    .split(',')
                    .map(|x| x.trim().parse::<f64>().context("TRADE_SIZES_WETH parse error"))
                    .collect::<Result<Vec<_>>>()?;
                Some(sz)
            }
            _ => None,
        };

        // Active markets (default: "USDC,WBTC,USDT")
        let markets_str = std::env::var("MARKETS")
            .unwrap_or_else(|_| "USDC,WBTC,USDT".into());

        let mut markets = Vec::new();
        for m_sym in markets_str.split(',') {
            let sym = m_sym.trim().to_ascii_uppercase();
            if sym.is_empty() {
                continue;
            }
            match sym.as_str() {
                "USDC" => {
                    let venues = custom_usdc_venues.clone().unwrap_or_else(|| vec![
                        Venue::UniswapV3 { fee: 500 },
                        Venue::PancakeV3 { fee: 100 },
                        Venue::PancakeV3 { fee: 500 },
                    ]);
                    let trade_sizes_weth = custom_usdc_sizes.clone().unwrap_or_else(|| vec![0.05, 0.1, 0.25, 0.5]);
                    markets.push(MarketConfig {
                        symbol: "USDC".into(),
                        pair: "WETH/USDC".into(),
                        quote_token: USDC.parse()?,
                        quote_decimals: 6,
                        venues,
                        trade_sizes_weth,
                    });
                }
                "WBTC" => {
                    markets.push(MarketConfig {
                        symbol: "WBTC".into(),
                        pair: "WETH/WBTC".into(),
                        quote_token: WBTC.parse()?,
                        quote_decimals: 8,
                        venues: vec![
                            Venue::UniswapV3 { fee: 500 },
                            Venue::PancakeV3 { fee: 100 },
                        ],
                        trade_sizes_weth: vec![0.1, 0.5, 1.0, 2.0],
                    });
                }
                "USDT" => {
                    markets.push(MarketConfig {
                        symbol: "USDT".into(),
                        pair: "WETH/USDT".into(),
                        quote_token: USDT.parse()?,
                        quote_decimals: 6,
                        venues: vec![
                            Venue::UniswapV3 { fee: 500 },
                            Venue::PancakeV3 { fee: 100 },
                            Venue::PancakeV3 { fee: 500 },
                        ],
                        trade_sizes_weth: vec![0.05, 0.1, 0.25],
                    });
                }
                other => {
                    anyhow::bail!("Unsupported market '{other}'. Supported markets are USDC, WBTC, USDT");
                }
            }
        }

        if markets.is_empty() {
            return Err(anyhow!("MARKETS cannot be empty"));
        }

        let output_file = std::env::var("OUTPUT_FILE")
            .unwrap_or_else(|_| "data/opportunities.jsonl".into());

        let watchdog_secs: u64 = std::env::var("WATCHDOG_SECS")
            .unwrap_or_else(|_| "90".into())
            .trim()
            .parse()
            .context("WATCHDOG_SECS must be a positive integer")?;

        if watchdog_secs == 0 {
            return Err(anyhow!("WATCHDOG_SECS must be > 0"));
        }

        Ok(Self {
            rpc_http,
            rpc_ws,
            execution_enabled,
            min_profit_wei,
            log_every_n_blocks,
            markets,
            output_file,
            watchdog_secs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_default_venues() {
        let s = "UniV3-500,UniV3-3000,Pancake-100,Pancake-500";
        let venues: Vec<Venue> = s.split(',').map(|x| x.trim().parse().unwrap()).collect();
        assert_eq!(
            venues,
            vec![
                Venue::UniswapV3 { fee: 500 },
                Venue::UniswapV3 { fee: 3000 },
                Venue::PancakeV3 { fee: 100 },
                Venue::PancakeV3 { fee: 500 },
            ]
        );
    }

    #[test]
    fn test_market_config_defaults() {
        let wbtc_addr = WBTC.parse::<Address>().unwrap();
        let usdt_addr = USDT.parse::<Address>().unwrap();
        assert_ne!(wbtc_addr, Address::ZERO);
        assert_ne!(usdt_addr, Address::ZERO);
    }
}
