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
use anyhow::Result;
use futures_util::stream::{self, StreamExt, TryStreamExt};
use serde::Serialize;

use crate::pricing::sushi_v2::Reserves;
use crate::pricing::uniswap_v3::{self, Side, V3Pool};
use crate::pricing::weth_to_raw;

/// Upper bound on in-flight quoter `eth_call`s, to stay under provider rate limits.
const MAX_CONCURRENT_RPC: usize = 8;

// ── Types ─────────────────────────────────────────────────────────────────────

/// A pricing venue on the WETH/USDC pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Venue {
    UniswapV3 { fee: u32 },
    SushiV2,
}

impl std::fmt::Display for Venue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Venue::UniswapV3 { fee } => write!(f, "UniV3-{fee}"),
            Venue::SushiV2 => write!(f, "SushiV2"),
        }
    }
}

/// Everything read from chain for one block (besides the quoter calls).
#[derive(Debug, Clone)]
pub struct MarketState {
    /// Every read and quote for this block is pinned to this block.
    pub block: BlockId,
    /// Discovered V3 pools with their in-range liquidity `L` this block.
    pub v3: Vec<(V3Pool, u128)>,
    pub sushi: Reserves,
}

impl MarketState {
    /// All venues in a fixed order (V3 by fee tier, then Sushi).
    pub fn venues(&self) -> Vec<Venue> {
        let mut v: Vec<Venue> = self
            .v3
            .iter()
            .map(|(p, _)| Venue::UniswapV3 { fee: p.fee })
            .collect();
        v.push(Venue::SushiV2);
        v
    }

    /// Depth metric comparable across venues, in V3 `L` units: sqrt(raw_weth · raw_usdc).
    /// For V3 this is the pool's in-range `liquidity()`; for a V2 pair the
    /// equivalent is sqrt(reserve_weth · reserve_usdc) (V2 = full-range V3).
    pub fn liquidity(&self, venue: Venue) -> u128 {
        match venue {
            Venue::UniswapV3 { fee } => self
                .v3
                .iter()
                .find(|(p, _)| p.fee == fee)
                .map(|(_, l)| *l)
                .unwrap_or(0),
            Venue::SushiV2 => v2_liquidity(&self.sushi),
        }
    }
}

/// First leg: `weth_in` WETH -> `usdc_out` USDC on `venue`.
#[derive(Debug, Clone, Copy)]
pub struct FirstLeg {
    pub venue: Venue,
    pub size_weth: f64,
    pub weth_in: u128,
    pub usdc_out: u128,
}

/// Full round trip: WETH -> USDC on `venue_a`, then USDC -> WETH on `venue_b`.
#[derive(Debug, Clone, Copy)]
pub struct RoundTrip {
    pub venue_a: Venue,
    pub venue_b: Venue,
    pub size_weth: f64,
    pub weth_in: u128,
    pub usdc_mid: u128,
    pub weth_back: u128,
}

/// One JSONL output record per (block, venue pair, direction, size).
#[derive(Debug, Clone, Serialize)]
pub struct OpportunityRecord {
    /// Local wall-clock unix seconds when the block was processed.
    pub timestamp: u64,
    pub block_number: u64,
    /// The venue pair is unordered and canonical (V3 tiers ascending, then Sushi);
    /// `direction` says which way the WETH flowed.
    pub venue_a: String,
    pub venue_b: String,
    /// "a_to_b" = WETH->USDC on venue_a, then USDC->WETH on venue_b.
    /// "b_to_a" = WETH->USDC on venue_b, then USDC->WETH on venue_a.
    pub direction: String,
    pub size_weth: f64,
    /// Raw WETH (18 dec) returned after both legs, as a string for precision.
    pub weth_out: String,
    /// Raw USDC (6 dec) held between the two legs.
    pub usdc_mid: String,
    /// Gross spread in basis points (may be negative).
    pub gross_spread_bps: f64,
    /// Depth of venue_a in V3 `L` units (V2 pair: sqrt(reserve_weth·reserve_usdc)).
    pub pool_liquidity_a: u128,
    /// Depth of venue_b in V3 `L` units.
    pub pool_liquidity_b: u128,
    /// SushiSwap WETH reserve at read time (18 dec), logged on every record.
    pub sushi_reserve_weth: u128,
    /// SushiSwap USDC reserve at read time (6 dec), logged on every record.
    pub sushi_reserve_usdc: u128,
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

// ── Quoting (RPC) ─────────────────────────────────────────────────────────────

/// Quote `amount_in` on `venue` for the given side. Sushi is local (0 RPC calls);
/// V3 is one QuoterV2 call. `None` = quote unavailable (quoter reverted).
async fn quote_on<P: Provider>(
    http: &P,
    state: &MarketState,
    venue: Venue,
    side: Side,
    amount_in: u128,
) -> Result<Option<u128>> {
    match venue {
        Venue::SushiV2 => Ok(Some(match side {
            Side::WethToUsdc => state.sushi.weth_to_usdc(amount_in),
            Side::UsdcToWeth => state.sushi.usdc_to_weth(amount_in),
        })),
        Venue::UniswapV3 { fee } => Ok(uniswap_v3::quote(http, state.block, fee, side, U256::from(amount_in))
            .await?
            .map(|v| v.to::<u128>())),
    }
}

/// Number of RPC calls `collect_round_trips` makes (QuoterV2 calls only).
pub fn quoter_calls(n_v3: usize, n_sizes: usize) -> usize {
    // First legs: one per V3 venue per size.
    // Second legs: V3 venue B, for each other venue A (n_v3 - 1 V3 + 1 Sushi), per size.
    n_v3 * n_sizes + n_v3 * n_v3 * n_sizes
}

/// Price every first leg and every cross-venue round trip for one block.
pub async fn collect_round_trips<P: Provider>(
    http: &P,
    state: &MarketState,
    sizes_weth: &[f64],
) -> Result<(Vec<FirstLeg>, Vec<RoundTrip>)> {
    let venues = state.venues();

    // 1. First legs (WETH -> USDC on every venue, every size).
    let first_jobs: Vec<(Venue, f64)> = venues
        .iter()
        .flat_map(|&v| sizes_weth.iter().map(move |&s| (v, s)))
        .collect();
    let first: Vec<FirstLeg> = stream::iter(first_jobs)
        .map(|(venue, size_weth)| async move {
            let weth_in = weth_to_raw(size_weth);
            let out = quote_on(http, state, venue, Side::WethToUsdc, weth_in).await?;
            Ok::<_, anyhow::Error>(out.map(|usdc_out| FirstLeg { venue, size_weth, weth_in, usdc_out }))
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
                .map(move |&b| (*leg, b))
        })
        .collect();
    let trips: Vec<RoundTrip> = stream::iter(second_jobs)
        .map(|(leg, venue_b)| async move {
            let back = quote_on(http, state, venue_b, Side::UsdcToWeth, leg.usdc_out).await?;
            Ok::<_, anyhow::Error>(back.map(|weth_back| RoundTrip {
                venue_a: leg.venue,
                venue_b,
                size_weth: leg.size_weth,
                weth_in: leg.weth_in,
                usdc_mid: leg.usdc_out,
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
    state: &MarketState,
    trips: &[RoundTrip],
) -> Vec<OpportunityRecord> {
    let order = state.venues();
    let rank = |v: Venue| order.iter().position(|&x| x == v).unwrap_or(usize::MAX);

    let mut records: Vec<OpportunityRecord> = trips
        .iter()
        .map(|t| {
            // Canonical unordered pair + direction.
            let (a, b, direction) = if rank(t.venue_a) <= rank(t.venue_b) {
                (t.venue_a, t.venue_b, "a_to_b")
            } else {
                (t.venue_b, t.venue_a, "b_to_a")
            };
            OpportunityRecord {
                timestamp,
                block_number,
                venue_a: a.to_string(),
                venue_b: b.to_string(),
                direction: direction.into(),
                size_weth: t.size_weth,
                weth_out: t.weth_back.to_string(),
                usdc_mid: t.usdc_mid.to_string(),
                gross_spread_bps: gross_spread_bps(t.weth_in, t.weth_back),
                pool_liquidity_a: state.liquidity(a),
                pool_liquidity_b: state.liquidity(b),
                sushi_reserve_weth: state.sushi.reserve_weth,
                sushi_reserve_usdc: state.sushi.reserve_usdc,
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
            v3: vec![(V3Pool { fee: 500, address: Address::ZERO }, 42)],
            sushi: Reserves { reserve_weth: 4 * 10u128.pow(18), reserve_usdc: 9 * 10u128.pow(6) },
        };
        let v3 = Venue::UniswapV3 { fee: 500 };
        let w = 10u128.pow(16);
        let trips = [
            // Sushi -> V3 (reverse of canonical order) : +10 bps
            RoundTrip { venue_a: Venue::SushiV2, venue_b: v3, size_weth: 0.01, weth_in: w, usdc_mid: 1, weth_back: w + w / 1000 },
            // V3 -> Sushi : -100 bps
            RoundTrip { venue_a: v3, venue_b: Venue::SushiV2, size_weth: 0.01, weth_in: w, usdc_mid: 1, weth_back: w - w / 100 },
        ];
        let recs = to_records(1, 2, &state, &trips);
        assert_eq!(recs.len(), 2);
        // Sorted best first.
        assert!(recs[0].gross_spread_bps > recs[1].gross_spread_bps);
        // Both records use the same canonical pair.
        for r in &recs {
            assert_eq!(r.venue_a, "UniV3-500");
            assert_eq!(r.venue_b, "SushiV2");
            assert_eq!(r.pool_liquidity_a, 42);
            assert_eq!(r.pool_liquidity_b, 6 * 10u128.pow(12));
        }
        assert_eq!(recs[0].direction, "b_to_a");
        assert_eq!(recs[1].direction, "a_to_b");
    }
}
