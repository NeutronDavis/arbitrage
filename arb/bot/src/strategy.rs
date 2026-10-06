//! Spread calculation and opportunity logging.
//!
//! Phase 2: gross spread only (no gas, no flash loan fee — that is Phase 3).
//!
//! # Gross spread definition
//! For a round-trip arb of `weth_in` on venue A then venue B:
//!   gross_spread_bps = (weth_back − weth_in) / weth_in × 10_000
//!
//! Negative = loss before fees. Positive = potential opportunity.
//!
//! # How round trips are priced (exactly, no approximation)
//! 1. First leg, per venue and size: WETH -> USDC  (`usdc_mid`).
//! 2. Second leg, for every *other* venue B: quote USDC -> WETH using that
//!    exact `usdc_mid` as input. UniV3 legs use QuoterV2; SushiV2 legs use the
//!    on-chain-identical constant-product formula on the block's reserves.
//!
//! With V venues and S sizes that is V·(V−1)·S round trips, i.e. both
//! directions of every venue pair. Same-venue round trips are skipped (they
//! can only lose the fee twice).

use alloy::eips::BlockId;
use alloy::primitives::U256;
use alloy::providers::Provider;
use anyhow::{Context, Result};
use futures_util::stream::{self, StreamExt, TryStreamExt};
use serde::Serialize;

use crate::pricing::sushi_v2::Reserves;
use crate::pricing::uniswap_v3::{self, Side, V3Pool};
use crate::pricing::weth_to_raw;

/// Upper bound on in-flight quoter `eth_call`s, to stay under provider rate limits.
#[allow(dead_code)]
const MAX_CONCURRENT_RPC: usize = 8;

// ── Types ─────────────────────────────────────────────────────────────────────

/// A pricing venue on the WETH/USDC pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum Venue {
    UniswapV3 { fee: u32 },
    PancakeV3 { fee: u32 },
    SushiV2,
}

impl std::fmt::Display for Venue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Venue::UniswapV3 { fee } => write!(f, "UniV3-{fee}"),
            Venue::PancakeV3 { fee } => write!(f, "Pancake-{fee}"),
            Venue::SushiV2 => write!(f, "SushiV2"),
        }
    }
}

impl std::str::FromStr for Venue {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("SushiV2") {
            return Ok(Venue::SushiV2);
        }
        if let Some(rest) = s.strip_prefix("UniV3-") {
            let fee = rest.parse::<u32>().context("Invalid UniV3 fee tier")?;
            return Ok(Venue::UniswapV3 { fee });
        }
        if let Some(rest) = s.strip_prefix("Pancake-") {
            let fee = rest.parse::<u32>().context("Invalid Pancake fee tier")?;
            return Ok(Venue::PancakeV3 { fee });
        }
        anyhow::bail!("Unknown venue: '{s}'. Expected UniV3-<fee>, Pancake-<fee>, or SushiV2");
    }
}

use alloy::primitives::Address;
use crate::constants::WETH;

/// Everything read from chain for one block (besides the quoter calls).
#[derive(Debug, Clone)]
pub struct MarketState {
    /// Every read and quote for this block is pinned to this block.
    #[allow(dead_code)]
    pub block: BlockId,
    /// Discovered V3 pools with their in-range liquidity `L` this block.
    pub v3: Vec<(V3Pool, u128)>,
    pub sushi: Option<Reserves>,
}

impl MarketState {
    /// All venues in deterministic order across pools, plus Sushi if active.
    pub fn venues(&self) -> Vec<Venue> {
        let mut v: Vec<Venue> = self
            .v3
            .iter()
            .map(|(p, _)| p.venue)
            .collect();
        v.sort();
        v.dedup();
        if self.sushi.is_some() {
            v.push(Venue::SushiV2);
        }
        v
    }

    /// Depth metric for a specific pair and venue in V3 `L` units.
    pub fn pool_liquidity(&self, pair: &str, venue: Venue) -> u128 {
        self.v3
            .iter()
            .find(|(p, _)| p.pair == pair && p.venue == venue)
            .map(|(_, l)| *l)
            .unwrap_or_else(|| {
                if pair == "WETH/USDC" && venue == Venue::SushiV2 {
                    self.sushi.as_ref().map(v2_liquidity).unwrap_or(0)
                } else {
                    0
                }
            })
    }

    /// Depth metric comparable across venues, in V3 `L` units.
    #[allow(dead_code)]
    pub fn liquidity(&self, venue: Venue) -> u128 {
        match venue {
            Venue::UniswapV3 { .. } | Venue::PancakeV3 { .. } => self
                .v3
                .iter()
                .find(|(p, _)| p.venue == venue)
                .map(|(_, l)| *l)
                .unwrap_or(0),
            Venue::SushiV2 => self.sushi.as_ref().map(v2_liquidity).unwrap_or(0),
        }
    }
}

/// First leg: `weth_in` WETH -> `quote_out` quote tokens on `venue`.
#[derive(Debug, Clone)]
pub struct FirstLeg {
    pub pair: String,
    pub venue: Venue,
    pub size_weth: f64,
    pub weth_in: u128,
    pub quote_out: u128,
    pub quote_token: Address,
    pub quote_decimals: u8,
}

/// Full round trip: WETH -> Quote on `venue_a`, then Quote -> WETH on `venue_b`.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RoundTrip {
    pub pair: String,
    pub venue_a: Venue,
    pub venue_b: Venue,
    pub size_weth: f64,
    pub weth_in: u128,
    pub mid_amount: u128,
    pub quote_token: Address,
    pub quote_decimals: u8,
    pub weth_back: u128,
}

fn default_pair() -> String {
    "WETH/USDC".to_string()
}

/// One JSONL output record per (block, pair, venue pair, direction, size).
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct OpportunityRecord {
    /// On-chain block timestamp (unix seconds).
    pub timestamp: u64,
    /// Local wall-clock unix seconds when the block was processed and logged.
    pub logged_at: u64,
    pub block_number: u64,
    /// Market pair (e.g. "WETH/USDC", "WETH/WBTC", "WETH/USDT").
    #[serde(default = "default_pair")]
    pub pair: String,
    /// The venue pair is unordered and canonical;
    /// `direction` says which way the WETH flowed.
    pub venue_a: String,
    pub venue_b: String,
    /// "a_to_b" = WETH->Quote on venue_a, then Quote->WETH on venue_b.
    /// "b_to_a" = WETH->Quote on venue_b, then Quote->WETH on venue_a.
    pub direction: String,
    pub size_weth: f64,
    /// Raw WETH (18 dec) returned after both legs, as a string for precision.
    pub weth_out: String,
    /// Raw USDC (6 dec) held between the two legs, filled for WETH/USDC rows only.
    #[serde(default)]
    pub usdc_mid: String,
    /// Raw quote token units held between the two legs (all pairs), as a string for precision.
    #[serde(default)]
    pub mid_amount: String,
    /// Gross spread in basis points (may be negative).
    pub gross_spread_bps: f64,
    /// Depth of venue_a in V3 `L` units, as a JSON string for precision.
    pub pool_liquidity_a: String,
    /// Depth of venue_b in V3 `L` units, as a JSON string for precision.
    pub pool_liquidity_b: String,
    /// SushiSwap WETH reserve at read time (18 dec), logged on every record as a JSON string.
    pub sushi_reserve_weth: String,
    /// SushiSwap USDC reserve at read time (6 dec), logged on every record as a JSON string.
    pub sushi_reserve_usdc: String,
}

// ── Pure math ─────────────────────────────────────────────────────────────────

/// Compute gross spread in basis points.
///
/// `gross_spread_bps = (weth_back − weth_in) / weth_in × 10_000`
///
/// Both amounts are in raw WETH units (18 dec, as u128 for precision).
/// Returns 0.0 if `weth_in` is zero.
pub fn gross_spread_bps(weth_in: u128, weth_back: u128) -> f64 {
    if weth_in == 0 {
        return 0.0;
    }
    let diff = weth_back as f64 - weth_in as f64;
    (diff / weth_in as f64) * 10_000.0
}

/// V2 depth in V3 `L` units: floor(sqrt(reserve_weth · reserve_usdc)).
pub fn v2_liquidity(r: &Reserves) -> u128 {
    r.reserve_weth
        .checked_mul(r.reserve_usdc)
        .map(|k| k.isqrt())
        .unwrap_or(u128::MAX)
}

// ── Batched Quoting via Multicall3 (Target: 2 RPC calls per block) ───────────

/// Static configuration and active pools for one market.
#[derive(Debug, Clone)]
pub struct MarketSetup {
    pub config: crate::config::MarketConfig,
    pub pools: Vec<V3Pool>,
    pub sushi_meta: Option<crate::pricing::sushi_v2::PairMeta>,
}

/// Output of a batched per-block read across all markets (Batch 1 + Batch 2).
pub struct BatchedBlockResult {
    pub block_timestamp: u64,
    pub state: MarketState,
    pub first: Vec<FirstLeg>,
    pub trips: Vec<RoundTrip>,
    pub batch1_calls: usize,
    pub batch2_calls: usize,
    pub batch1_payload_bytes: usize,
    pub batch2_payload_bytes: usize,
    pub max_gas_estimate: u64,
}

/// Estimate Multicall3 calldata size in bytes.
fn estimate_multicall_payload_size(calls: &[crate::multicall::Call3]) -> usize {
    // 4 bytes selector + 32 bytes offset + 32 bytes length + calls array overhead
    let mut bytes = 4 + 32 + 32;
    for c in calls {
        // target (32) + allowFailure (32) + offset (32) + length (32) + data padded to 32
        let data_padded = c.callData.len().div_ceil(32) * 32;
        bytes += 32 + 32 + 32 + 32 + data_padded;
    }
    bytes
}

/// Process all per-block reads and quotes across ALL markets in exactly 2 Multicall3 `aggregate3` calls:
/// - Batch 1: Timestamp + all pool liquidities + optional Sushi reserves + all V3 first leg quotes (WETH -> Quote).
/// - Batch 2: All V3 second leg quotes (Quote -> WETH) using the exact mid amounts.
pub async fn process_block_batched<P: Provider>(
    http: &P,
    markets: &[MarketSetup],
    block: BlockId,
) -> Result<BatchedBlockResult> {
    use crate::multicall;

    let weth_addr: Address = WETH.parse()?;
    let mut max_gas_estimate: u64 = 0;

    // Collect all unique pools across all markets
    let mut all_pools: Vec<V3Pool> = Vec::new();
    for m in markets {
        for p in &m.pools {
            if !all_pools.iter().any(|existing| existing.address == p.address) {
                all_pools.push(p.clone());
            }
        }
    }

    // Check if any market has Sushi configured
    let sushi_meta = markets.iter().find_map(|m| m.sushi_meta);

    // ── Batch 1: Timestamp + State Reads + First Legs ─────────────────────────
    let mut batch1 = Vec::new();

    // Call 0: Block timestamp
    batch1.push(multicall::build_timestamp_call()?);

    // Calls 1..=n_pools: V3 pool liquidity
    for p in &all_pools {
        batch1.push(uniswap_v3::build_liquidity_call(p.address));
    }

    // Optional Sushi getReserves() call
    let sushi_included = sushi_meta.is_some();
    if sushi_included {
        batch1.push(crate::pricing::sushi_v2::build_reserves_call()?);
    }

    // Calls for first leg V3 quotes across all markets
    let mut batch1_quote_meta = Vec::new();
    for m in markets {
        for p in &m.pools {
            for &size in &m.config.trade_sizes_weth {
                let weth_in = weth_to_raw(size);
                let call = uniswap_v3::build_quote_call(
                    p.quoter,
                    p.fee,
                    weth_addr,
                    p.quote_token,
                    U256::from(weth_in),
                )?;
                batch1.push(call);
                batch1_quote_meta.push((m.config.clone(), p.clone(), size, weth_in));
            }
        }
    }

    let batch1_calls = batch1.len();
    let batch1_payload_bytes = estimate_multicall_payload_size(&batch1);

    // Execute Batch 1 (RPC Call 1)
    let res1 = multicall::aggregate3(http, batch1, block).await?;

    let block_timestamp = multicall::decode_timestamp_result(&res1[0]).unwrap_or(0);

    let n_pools = all_pools.len();
    let mut v3_liq = Vec::with_capacity(n_pools);
    for (i, p) in all_pools.iter().enumerate() {
        let liq = uniswap_v3::decode_liquidity_result(&res1[1 + i]).unwrap_or(0);
        v3_liq.push((p.clone(), liq));
    }

    let (sushi_reserves, quote_start_idx) = if let Some(meta) = sushi_meta {
        let reserves = crate::pricing::sushi_v2::decode_reserves_result(&res1[1 + n_pools], meta);
        (reserves, 2 + n_pools)
    } else {
        (None, 1 + n_pools)
    };

    let state = MarketState {
        block,
        v3: v3_liq,
        sushi: sushi_reserves,
    };

    // Unpack first legs
    let mut first = Vec::new();

    // 1. V3 first legs from Batch 1
    for (i, (m_cfg, p, size, weth_in)) in batch1_quote_meta.into_iter().enumerate() {
        let res_idx = quote_start_idx + i;
        if let Some((quote_out, gas_est)) = uniswap_v3::decode_quote_result(&res1[res_idx]) {
            max_gas_estimate = max_gas_estimate.max(gas_est);
            first.push(FirstLeg {
                pair: m_cfg.pair,
                venue: p.venue,
                size_weth: size,
                weth_in,
                quote_out: quote_out.to::<u128>(),
                quote_token: m_cfg.quote_token,
                quote_decimals: m_cfg.quote_decimals,
            });
        }
    }

    // 2. Sushi first legs (calculated locally off reserves for WETH/USDC, 0 RPC calls)
    if let Some(ref sushi) = state.sushi {
        if let Some(m_usdc) = markets.iter().find(|m| m.config.symbol == "USDC") {
            for &size in &m_usdc.config.trade_sizes_weth {
                let weth_in = weth_to_raw(size);
                let usdc_out = sushi.weth_to_usdc(weth_in);
                if usdc_out > 0 {
                    first.push(FirstLeg {
                        pair: "WETH/USDC".into(),
                        venue: Venue::SushiV2,
                        size_weth: size,
                        weth_in,
                        quote_out: usdc_out,
                        quote_token: m_usdc.config.quote_token,
                        quote_decimals: 6,
                    });
                }
            }
        }
    }

    // ── Batch 2: Second Legs (across each market) ─────────────────────────────
    let mut batch2 = Vec::new();
    let mut batch2_meta = Vec::new();
    let mut trips = Vec::new();

    for m in markets {
        let pair_name = &m.config.pair;
        let mut market_venues: Vec<Venue> = m.pools.iter().map(|p| p.venue).collect();
        if m.sushi_meta.is_some() {
            market_venues.push(Venue::SushiV2);
        }

        let market_first: Vec<&FirstLeg> = first.iter().filter(|l| &l.pair == pair_name).collect();

        for leg in market_first {
            for &venue_b in &market_venues {
                if venue_b == leg.venue {
                    continue;
                }
                match venue_b {
                    Venue::SushiV2 => {
                        // Local Sushi calculation (0 RPC calls)
                        if let Some(ref sushi) = state.sushi {
                            let weth_back = sushi.usdc_to_weth(leg.quote_out);
                            if weth_back > 0 {
                                trips.push(RoundTrip {
                                    pair: leg.pair.clone(),
                                    venue_a: leg.venue,
                                    venue_b,
                                    size_weth: leg.size_weth,
                                    weth_in: leg.weth_in,
                                    mid_amount: leg.quote_out,
                                    quote_token: leg.quote_token,
                                    quote_decimals: leg.quote_decimals,
                                    weth_back,
                                });
                            }
                        }
                    }
                    Venue::UniswapV3 { .. } | Venue::PancakeV3 { .. } => {
                        if let Some(p) = m.pools.iter().find(|p| p.venue == venue_b) {
                            let call = uniswap_v3::build_quote_call(
                                p.quoter,
                                p.fee,
                                p.quote_token,
                                weth_addr,
                                U256::from(leg.quote_out),
                            )?;
                            batch2.push(call);
                            batch2_meta.push((leg.clone(), venue_b));
                        }
                    }
                }
            }
        }
    }

    let batch2_calls = batch2.len();
    let batch2_payload_bytes = estimate_multicall_payload_size(&batch2);

    if !batch2.is_empty() {
        // Execute Batch 2 (RPC Call 2)
        let res2 = multicall::aggregate3(http, batch2, block).await?;
        for (res, (leg, venue_b)) in res2.iter().zip(batch2_meta) {
            if let Some((weth_back, gas_est)) = uniswap_v3::decode_quote_result(res) {
                max_gas_estimate = max_gas_estimate.max(gas_est);
                trips.push(RoundTrip {
                    pair: leg.pair,
                    venue_a: leg.venue,
                    venue_b,
                    size_weth: leg.size_weth,
                    weth_in: leg.weth_in,
                    mid_amount: leg.quote_out,
                    quote_token: leg.quote_token,
                    quote_decimals: leg.quote_decimals,
                    weth_back: weth_back.to::<u128>(),
                });
            }
        }
    }

    Ok(BatchedBlockResult {
        block_timestamp,
        state,
        first,
        trips,
        batch1_calls,
        batch2_calls,
        batch1_payload_bytes,
        batch2_payload_bytes,
        max_gas_estimate,
    })
}

// ── Legacy Quoting (fallback / comparison) ───────────────────────────────────

/// Quote `amount_in` on `venue` for the given side. Sushi is local (0 RPC calls);
/// V3 is one QuoterV2 call. `None` = quote unavailable (quoter reverted).
#[allow(dead_code)]
pub async fn quote_on<P: Provider>(
    http: &P,
    state: &MarketState,
    venue: Venue,
    side: Side,
    amount_in: u128,
) -> Result<Option<u128>> {
    match venue {
        Venue::SushiV2 => Ok(state.sushi.as_ref().map(|s| match side {
            Side::WethToUsdc | Side::WethToQuote => s.weth_to_usdc(amount_in),
            Side::UsdcToWeth | Side::QuoteToWeth => s.usdc_to_weth(amount_in),
        })),
        Venue::UniswapV3 { fee } | Venue::PancakeV3 { fee } => {
            let pool = state.v3.iter().find(|(p, _)| p.venue == venue).map(|(p, _)| p);
            let Some(p) = pool else { return Ok(None); };
            Ok(uniswap_v3::quote(http, p.quoter, state.block, fee, side, U256::from(amount_in))
                .await?
                .map(|v| v.to::<u128>()))
        }
    }
}

/// Number of RPC calls `collect_round_trips` makes (QuoterV2 calls only).
#[allow(dead_code)]
pub fn quoter_calls(n_v3: usize, n_sizes: usize) -> usize {
    // First legs: one per V3 venue per size.
    // Second legs: V3 venue B, for each other venue A (n_v3 - 1 V3 + 1 Sushi), per size.
    n_v3 * n_sizes + n_v3 * n_v3 * n_sizes
}

/// Price every first leg and every cross-venue round trip for one block (unbatched).
#[allow(dead_code)]
pub async fn collect_round_trips<P: Provider>(
    http: &P,
    state: &MarketState,
    sizes_weth: &[f64],
) -> Result<(Vec<FirstLeg>, Vec<RoundTrip>)> {
    let venues = state.venues();
    let usdc_addr: Address = crate::constants::USDC.parse()?;

    // 1. First legs (WETH -> USDC on every venue, every size).
    let first_jobs: Vec<(Venue, f64)> = venues
        .iter()
        .flat_map(|&v| sizes_weth.iter().map(move |&s| (v, s)))
        .collect();
    let first: Vec<FirstLeg> = stream::iter(first_jobs)
        .map(|(venue, size_weth)| async move {
            let weth_in = weth_to_raw(size_weth);
            let out = quote_on(http, state, venue, Side::WethToUsdc, weth_in).await?;
            Ok::<_, anyhow::Error>(out.map(|usdc_out| FirstLeg {
                pair: "WETH/USDC".into(),
                venue,
                size_weth,
                weth_in,
                quote_out: usdc_out,
                quote_token: usdc_addr,
                quote_decimals: 6,
            }))
        })
        .buffered(MAX_CONCURRENT_RPC)
        .try_filter_map(|x| async move { Ok(x) })
        .try_collect()
        .await?;

    // 2. Second legs: USDC -> WETH on every other venue, using the exact usdc_mid.
    let second_jobs: Vec<(FirstLeg, Venue)> = first
        .iter()
        .flat_map(|leg| {
            venues
                .iter()
                .filter(move |&&b| b != leg.venue)
                .map(move |&b| (leg.clone(), b))
        })
        .collect();
    let trips: Vec<RoundTrip> = stream::iter(second_jobs)
        .map(|(leg, venue_b)| async move {
            let back = quote_on(http, state, venue_b, Side::UsdcToWeth, leg.quote_out).await?;
            Ok::<_, anyhow::Error>(back.map(|weth_back| RoundTrip {
                pair: leg.pair.clone(),
                venue_a: leg.venue,
                venue_b,
                size_weth: leg.size_weth,
                weth_in: leg.weth_in,
                mid_amount: leg.quote_out,
                quote_token: leg.quote_token,
                quote_decimals: leg.quote_decimals,
                weth_back,
            }))
        })
        .buffered(MAX_CONCURRENT_RPC)
        .try_filter_map(|x| async move { Ok(x) })
        .try_collect()
        .await?;

    Ok((first, trips))
}

// ── Records (pure) ────────────────────────────────────────────────────────────

/// Turn round trips into JSONL records, sorted by `gross_spread_bps` descending.
pub fn to_records(
    block_number: u64,
    timestamp: u64,
    logged_at: u64,
    state: &MarketState,
    trips: &[RoundTrip],
) -> Vec<OpportunityRecord> {
    let order = state.venues();
    let rank = |v: Venue| order.iter().position(|&x| x == v).unwrap_or(usize::MAX);

    let (sushi_reserve_weth, sushi_reserve_usdc) = match &state.sushi {
        Some(s) => (s.reserve_weth.to_string(), s.reserve_usdc.to_string()),
        None => ("0".to_string(), "0".to_string()),
    };

    let mut records: Vec<OpportunityRecord> = trips
        .iter()
        .map(|t| {
            // Canonical unordered pair + direction.
            let (a, b, direction) = if rank(t.venue_a) <= rank(t.venue_b) {
                (t.venue_a, t.venue_b, "a_to_b")
            } else {
                (t.venue_b, t.venue_a, "b_to_a")
            };
            let mid_str = t.mid_amount.to_string();
            let usdc_mid = if t.pair == "WETH/USDC" {
                mid_str.clone()
            } else {
                String::new()
            };

            OpportunityRecord {
                timestamp,
                logged_at,
                block_number,
                pair: t.pair.clone(),
                venue_a: a.to_string(),
                venue_b: b.to_string(),
                direction: direction.into(),
                size_weth: t.size_weth,
                weth_out: t.weth_back.to_string(),
                usdc_mid,
                mid_amount: mid_str,
                gross_spread_bps: gross_spread_bps(t.weth_in, t.weth_back),
                pool_liquidity_a: state.pool_liquidity(&t.pair, a).to_string(),
                pool_liquidity_b: state.pool_liquidity(&t.pair, b).to_string(),
                sushi_reserve_weth: if t.pair == "WETH/USDC" { sushi_reserve_weth.clone() } else { "0".into() },
                sushi_reserve_usdc: if t.pair == "WETH/USDC" { sushi_reserve_usdc.clone() } else { "0".into() },
            }
        })
        .collect();

    records.sort_by(|x, y| {
        y.gross_spread_bps
            .partial_cmp(&x.gross_spread_bps)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    records
}

/// Append a slice of records to a JSONL file (one JSON object per line).
///
/// Opens the file with `create(true)` and `append(true)` so the file is created
/// if missing, and appended to without truncation if it already exists.
pub fn append_jsonl(path: &str, records: &[OpportunityRecord]) -> Result<()> {
    use std::io::Write;
    if records.is_empty() {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("Cannot open output file: {path}"))?;

    for record in records {
        let line = serde_json::to_string(record)
            .context("Failed to serialise opportunity record")?;
        writeln!(file, "{line}").with_context(|| format!("Write failed: {path}"))?;
    }
    Ok(())
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;

    #[test]
    fn spread_zero_when_equal() {
        let spread = gross_spread_bps(1_000_000_000_000_000_000, 1_000_000_000_000_000_000);
        assert!((spread - 0.0).abs() < 1e-9);
    }

    #[test]
    fn spread_positive_when_profit() {
        // weth_back > weth_in → positive spread
        let weth_in:   u128 = 1_000_000_000_000_000_000; // 1 WETH
        let weth_back: u128 = 1_001_000_000_000_000_000; // 1.001 WETH
        let spread = gross_spread_bps(weth_in, weth_back);
        // (0.001 / 1) * 10000 = 10 bps
        assert!((spread - 10.0).abs() < 0.001, "spread={spread}");
    }

    #[test]
    fn spread_negative_when_loss() {
        let weth_in:   u128 = 1_000_000_000_000_000_000;
        let weth_back: u128 =   990_000_000_000_000_000; // 0.99 WETH
        let spread = gross_spread_bps(weth_in, weth_back);
        // (-0.01 / 1) * 10000 = -100 bps
        assert!((spread - (-100.0)).abs() < 0.001, "spread={spread}");
    }

    #[test]
    fn spread_zero_when_weth_in_is_zero() {
        assert_eq!(gross_spread_bps(0, 1_000), 0.0);
    }

    #[test]
    fn sushi_round_trip_loses_about_two_fees() {
        // Deep symmetric-ish pool: a tiny round trip on the same CPF pool must
        // lose ~2 × 0.3% = ~60 bps (and never gain).
        let r = Reserves {
            reserve_weth: 1_000 * 10u128.pow(18),
            reserve_usdc: 2_600_000 * 10u128.pow(6),
        };
        let weth_in = 10u128.pow(16); // 0.01 WETH
        let usdc = r.weth_to_usdc(weth_in);
        let back = r.usdc_to_weth(usdc);
        let bps = gross_spread_bps(weth_in, back);
        assert!(bps < -59.0 && bps > -61.0, "bps={bps}");
    }

    #[test]
    fn v2_liquidity_is_sqrt_k() {
        let r = Reserves { reserve_weth: 4 * 10u128.pow(18), reserve_usdc: 9 * 10u128.pow(6) };
        // sqrt(4e18 * 9e6) = sqrt(36e24) = 6e12
        assert_eq!(v2_liquidity(&r), 6 * 10u128.pow(12));
    }

    #[test]
    fn quoter_call_count_formula() {
        // 3 V3 tiers, 3 sizes: 9 first legs + 27 second legs = 36.
        assert_eq!(quoter_calls(3, 3), 36);
    }

    #[test]
    fn records_are_canonical_and_sorted() {
        let state = MarketState {
            block: BlockId::number(1),
            v3: vec![(
                V3Pool {
                    pair: "WETH/USDC".into(),
                    venue: Venue::UniswapV3 { fee: 500 },
                    fee: 500,
                    address: Address::ZERO,
                    quoter: Address::ZERO,
                    quote_token: Address::ZERO,
                },
                42,
            )],
            sushi: Some(Reserves {
                reserve_weth: 4 * 10u128.pow(18),
                reserve_usdc: 9 * 10u128.pow(6),
            }),
        };
        let v3 = Venue::UniswapV3 { fee: 500 };
        let w = 10u128.pow(16);
        let trips = [
            // Sushi -> V3 (reverse of canonical order) : +10 bps
            RoundTrip {
                pair: "WETH/USDC".into(),
                venue_a: Venue::SushiV2,
                venue_b: v3,
                size_weth: 0.01,
                weth_in: w,
                mid_amount: 1,
                quote_token: Address::ZERO,
                quote_decimals: 6,
                weth_back: w + w / 1000,
            },
            // V3 -> Sushi : -100 bps
            RoundTrip {
                pair: "WETH/USDC".into(),
                venue_a: v3,
                venue_b: Venue::SushiV2,
                size_weth: 0.01,
                weth_in: w,
                mid_amount: 1,
                quote_token: Address::ZERO,
                quote_decimals: 6,
                weth_back: w - w / 100,
            },
        ];
        let recs = to_records(1, 2, 3, &state, &trips);
        assert_eq!(recs.len(), 2);
        // Sorted best first.
        assert!(recs[0].gross_spread_bps > recs[1].gross_spread_bps);
        // Both records use the same canonical pair.
        for r in &recs {
            assert_eq!(r.timestamp, 2);
            assert_eq!(r.logged_at, 3);
            assert_eq!(r.venue_a, "UniV3-500");
            assert_eq!(r.venue_b, "SushiV2");
            assert_eq!(r.pool_liquidity_a, "42");
            assert_eq!(r.pool_liquidity_b, (6 * 10u128.pow(12)).to_string());
            assert_eq!(r.sushi_reserve_weth, (4 * 10u128.pow(18)).to_string());
            assert_eq!(r.sushi_reserve_usdc, (9 * 10u128.pow(6)).to_string());
        }
        assert_eq!(recs[0].direction, "b_to_a");
        assert_eq!(recs[1].direction, "a_to_b");
    }

    #[test]
    fn venue_from_str_and_display() {
        assert_eq!("UniV3-500".parse::<Venue>().unwrap(), Venue::UniswapV3 { fee: 500 });
        assert_eq!("UniV3-3000".parse::<Venue>().unwrap(), Venue::UniswapV3 { fee: 3000 });
        assert_eq!("Pancake-100".parse::<Venue>().unwrap(), Venue::PancakeV3 { fee: 100 });
        assert_eq!("Pancake-500".parse::<Venue>().unwrap(), Venue::PancakeV3 { fee: 500 });
        assert_eq!("SushiV2".parse::<Venue>().unwrap(), Venue::SushiV2);
        assert_eq!(Venue::PancakeV3 { fee: 100 }.to_string(), "Pancake-100");
        assert_eq!(Venue::PancakeV3 { fee: 500 }.to_string(), "Pancake-500");
    }

    #[test]
    fn test_reopening_output_file_appends() {
        let dummy = |block: u64| OpportunityRecord {
            timestamp: 1000 + block,
            logged_at: 1001 + block,
            block_number: block,
            pair: "WETH/USDC".into(),
            venue_a: "UniV3-500".into(),
            venue_b: "SushiV2".into(),
            direction: "a_to_b".into(),
            size_weth: 0.01,
            weth_out: "9966649150947404".into(),
            usdc_mid: "26912378".into(),
            mid_amount: "26912378".into(),
            gross_spread_bps: -33.5,
            pool_liquidity_a: "1000".into(),
            pool_liquidity_b: "2000".into(),
            sushi_reserve_weth: "3000".into(),
            sushi_reserve_usdc: "4000".into(),
        };

        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join(format!(
            "arb_test_append_{}.jsonl",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path_str = test_file.to_str().unwrap();

        // Ensure file does not exist initially
        let _ = std::fs::remove_file(&test_file);

        // First write: batch of 2 records (creates file)
        let batch1 = vec![dummy(100), dummy(101)];
        append_jsonl(path_str, &batch1).expect("first append must succeed");

        let content1 = std::fs::read_to_string(&test_file).expect("must read file after first write");
        let lines1: Vec<&str> = content1.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines1.len(), 2, "initial write must create file with 2 records");

        // Reopen / second write (simulating subsequent block or process restart)
        let batch2 = vec![dummy(102), dummy(103), dummy(104)];
        append_jsonl(path_str, &batch2).expect("second append must succeed");

        let content2 = std::fs::read_to_string(&test_file).expect("must read file after second write");
        let lines2: Vec<&str> = content2.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines2.len(), 5, "reopening must append all 3 new records without truncating first 2");

        // Verify content order preserved
        let parsed_first: OpportunityRecord = serde_json::from_str(lines2[0]).unwrap();
        let parsed_last: OpportunityRecord = serde_json::from_str(lines2[4]).unwrap();
        assert_eq!(parsed_first.block_number, 100);
        assert_eq!(parsed_last.block_number, 104);

        // Clean up
        let _ = std::fs::remove_file(&test_file);
    }
}

