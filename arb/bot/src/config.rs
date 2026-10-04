//! Environment loading and validated configuration.
//!
//! Phase 2c: PancakeSwap V3 integration. Optional variables with documented defaults:
//!   VENUES              — comma-separated venues (default "UniV3-500,UniV3-3000,Pancake-100,Pancake-500")
//!   LOG_EVERY_N_BLOCKS  — process every Nth block (default 40)
//!   TRADE_SIZES_WETH    — comma-separated WETH amounts to quote (default "0.05,0.1,0.25,0.5")
//!   OUTPUT_FILE         — path for JSONL opportunity log (default "data/opportunities.jsonl")
//!   WATCHDOG_SECS       — watchdog timeout in seconds (default 90)

use anyhow::{anyhow, Context, Result};
use crate::strategy::Venue;

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
    /// Active venues to quote and log.
    pub venues: Vec<Venue>,
    /// WETH amounts (in whole WETH, as f64) to quote per venue per block.
    pub trade_sizes_weth: Vec<f64>,
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
            .field("venues", &self.venues)
            .field("trade_sizes_weth", &self.trade_sizes_weth)
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

        let venues_str = std::env::var("VENUES")
            .unwrap_or_else(|_| "UniV3-500,UniV3-3000,Pancake-100,Pancake-500".into());

        let venues: Vec<Venue> = venues_str
            .split(',')
            .map(|s| s.trim().parse::<Venue>())
            .collect::<Result<Vec<_>>>()?;

        if venues.is_empty() {
            return Err(anyhow!("VENUES cannot be empty"));
        }

        let trade_sizes_weth: Vec<f64> = std::env::var("TRADE_SIZES_WETH")
            .unwrap_or_else(|_| "0.05,0.1,0.25,0.5".into())
            .split(',')
            .map(|s| {
                s.trim()
                    .parse::<f64>()
                    .context("TRADE_SIZES_WETH must be comma-separated floats, e.g. 0.05,0.1,0.25,0.5")
            })
            .collect::<Result<Vec<_>>>()?;

        if trade_sizes_weth.is_empty() || trade_sizes_weth.iter().any(|&v| v <= 0.0) {
            return Err(anyhow!("TRADE_SIZES_WETH values must be positive"));
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
            venues,
            trade_sizes_weth,
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
}
