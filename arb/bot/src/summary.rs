//! JSONL opportunity log analysis and statistical summarisation.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::BufRead;

/// Summary statistics for a single (pair, venue_a, venue_b, direction, size) group.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupStats {
    pub pair: String,
    pub venue_a: String,
    pub venue_b: String,
    pub direction: String,
    pub size_weth: f64,
    pub row_count: usize,
    pub positive_count: usize,
    pub max_bps: f64,
    pub median_bps: f64,
    pub p95_bps: f64,
}

fn default_pair() -> String {
    "WETH/USDC".to_string()
}

#[derive(Deserialize)]
struct OpportunityEntry {
    #[serde(default = "default_pair")]
    pub pair: String,
    pub venue_a: String,
    pub venue_b: String,
    pub direction: String,
    pub size_weth: f64,
    pub gross_spread_bps: f64,
}

/// Compute median of a sorted (ascending) slice. Returns 0.0 if empty.
pub fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// Compute 95th percentile using nearest rank method.
/// Rank: ceil(0.95 * n) - 1 (0-indexed). Returns 0.0 if empty.
pub fn percentile_95(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return 0.0;
    }
    if n == 1 {
        return sorted[0];
    }
    let rank = ((0.95 * n as f64).ceil() as usize).saturating_sub(1);
    sorted[rank.min(n - 1)]
}

type GroupKey = (String, String, String, String, u64); // (pair, venue_a, venue_b, direction, size_key)
type GroupVal = (f64, Vec<f64>);

/// Summarise records from any reader yielding JSONL lines.
pub fn summarize_reader<R: BufRead>(reader: R) -> Result<Vec<GroupStats>> {
    // Key: (pair, venue_a, venue_b, direction, size_key_nanoweth)
    let mut groups: BTreeMap<GroupKey, GroupVal> = BTreeMap::new();

    for (line_idx, line_res) in reader.lines().enumerate() {
        let line = line_res?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let rec: OpportunityEntry = serde_json::from_str(trimmed)
            .with_context(|| format!("Failed to parse JSON record at line {}", line_idx + 1))?;

        let size_key = (rec.size_weth * 1e9).round() as u64;
        let entry = groups
            .entry((rec.pair, rec.venue_a, rec.venue_b, rec.direction, size_key))
            .or_insert_with(|| (rec.size_weth, Vec::new()));
        entry.1.push(rec.gross_spread_bps);
    }

    let mut out = Vec::new();
    for ((pair, venue_a, venue_b, direction, _), (size_weth, mut spreads)) in groups {
        spreads.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let row_count = spreads.len();
        let positive_count = spreads.iter().filter(|&&s| s > 0.0).count();
        let max_bps = *spreads.last().unwrap_or(&0.0);
        let median_bps = median(&spreads);
        let p95_bps = percentile_95(&spreads);

        out.push(GroupStats {
            pair,
            venue_a,
            venue_b,
            direction,
            size_weth,
            row_count,
            positive_count,
            max_bps,
            median_bps,
            p95_bps,
        });
    }

    Ok(out)
}

/// Print formatted table of summary statistics.
pub fn print_summary(stats: &[GroupStats]) {
    println!("\n══ Opportunity Summary by Pair, Venue Pair, Direction & Size ══");
    println!(
        "{:<11} {:<12} {:<12} {:<8} {:>8} {:>8} {:>14} {:>12} {:>12} {:>12}",
        "Pair", "Venue A", "Venue B", "Dir", "Size(W)", "Rows", "Spread > 0", "Max(bps)", "Median(bps)", "P95(bps)"
    );
    println!("{:-<106}", "");

    let mut total_rows = 0;
    let mut total_positive = 0;

    for s in stats {
        total_rows += s.row_count;
        total_positive += s.positive_count;
        let pct_pos = if s.row_count > 0 {
            (s.positive_count as f64 / s.row_count as f64) * 100.0
        } else {
            0.0
        };
        let pos_str = format!("{} ({:>5.1}%)", s.positive_count, pct_pos);
        println!(
            "{:<11} {:<12} {:<12} {:<8} {:>8.3} {:>8} {:>14} {:>12.2} {:>12.2} {:>12.2}",
            s.pair,
            s.venue_a,
            s.venue_b,
            s.direction,
            s.size_weth,
            s.row_count,
            pos_str,
            s.max_bps,
            s.median_bps,
            s.p95_bps
        );
    }
    println!("{:-<106}", "");
    println!(
        "Total rows: {}, Groups: {}, Rows with spread > 0: {}\n",
        total_rows,
        stats.len(),
        total_positive
    );
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_median_odd_and_even() {
        assert_eq!(median(&[]), 0.0);
        assert_eq!(median(&[10.0]), 10.0);
        assert_eq!(median(&[1.0, 3.0, 5.0]), 3.0);
        assert_eq!(median(&[1.0, 3.0, 5.0, 7.0]), 4.0);
    }

    #[test]
    fn test_percentile_95() {
        assert_eq!(percentile_95(&[]), 0.0);
        assert_eq!(percentile_95(&[42.0]), 42.0);

        // 20 elements: 1.0 ..= 20.0
        // ceil(0.95 * 20) - 1 = 19 - 1 = 18 -> index 18 is 19.0
        let vals: Vec<f64> = (1..=20).map(|i| i as f64).collect();
        assert_eq!(percentile_95(&vals), 19.0);

        // 100 elements: 1.0 ..= 100.0
        // ceil(0.95 * 100) - 1 = 95 - 1 = index 94 -> 95.0
        let vals100: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        assert_eq!(percentile_95(&vals100), 95.0);
    }

    #[test]
    fn test_summarize_fixture() {
        // Fixture with multiple groups, positive and negative spreads, and various sizes
        let fixture = r#"
{"venue_a":"UniV3-500","venue_b":"SushiV2","direction":"a_to_b","size_weth":0.01,"gross_spread_bps":-25.0}
{"venue_a":"UniV3-500","venue_b":"SushiV2","direction":"a_to_b","size_weth":0.01,"gross_spread_bps":-10.0}
{"venue_a":"UniV3-500","venue_b":"SushiV2","direction":"a_to_b","size_weth":0.01,"gross_spread_bps":5.0}
{"venue_a":"UniV3-500","venue_b":"SushiV2","direction":"a_to_b","size_weth":0.01,"gross_spread_bps":12.0}
{"venue_a":"UniV3-500","venue_b":"SushiV2","direction":"b_to_a","size_weth":0.05,"gross_spread_bps":-50.0}
"#;
        let stats = summarize_reader(Cursor::new(fixture)).expect("summarize fixture");
        assert_eq!(stats.len(), 2);

        // Group 1: UniV3-500 <-> SushiV2, a_to_b, 0.01 WETH (defaults to WETH/USDC)
        let g1 = &stats[0];
        assert_eq!(g1.pair, "WETH/USDC");
        assert_eq!(g1.venue_a, "UniV3-500");
        assert_eq!(g1.venue_b, "SushiV2");
        assert_eq!(g1.direction, "a_to_b");
        assert_eq!(g1.size_weth, 0.01);
        assert_eq!(g1.row_count, 4);
        assert_eq!(g1.positive_count, 2); // 5.0 and 12.0
        assert_eq!(g1.max_bps, 12.0);
        // Spreads: [-25.0, -10.0, 5.0, 12.0] -> median is (-10.0 + 5.0) / 2 = -2.5
        assert!((g1.median_bps - (-2.5)).abs() < 1e-6);
        // p95 of 4 elements: ceil(0.95 * 4) - 1 = 4 - 1 = 3 -> 12.0
        assert_eq!(g1.p95_bps, 12.0);

        // Group 2: UniV3-500 <-> SushiV2, b_to_a, 0.05 WETH (defaults to WETH/USDC)
        let g2 = &stats[1];
        assert_eq!(g2.pair, "WETH/USDC");
        assert_eq!(g2.direction, "b_to_a");
        assert_eq!(g2.size_weth, 0.05);
        assert_eq!(g2.row_count, 1);
        assert_eq!(g2.positive_count, 0);
        assert_eq!(g2.max_bps, -50.0);
        assert_eq!(g2.median_bps, -50.0);
        assert_eq!(g2.p95_bps, -50.0);
    }

    #[test]
    fn test_summarize_multi_pair_and_legacy_rows() {
        // Line 1: legacy row without pair (should default to WETH/USDC)
        // Line 2: modern row with WETH/WBTC
        // Line 3: modern row with WETH/USDT
        let fixture = r#"
{"venue_a":"UniV3-500","venue_b":"Pancake-100","direction":"a_to_b","size_weth":0.05,"gross_spread_bps":-12.0}
{"pair":"WETH/WBTC","venue_a":"UniV3-500","venue_b":"Pancake-100","direction":"a_to_b","size_weth":0.1,"gross_spread_bps":-2.5}
{"pair":"WETH/USDT","venue_a":"UniV3-500","venue_b":"Pancake-500","direction":"b_to_a","size_weth":0.05,"gross_spread_bps":-8.0}
"#;
        let stats = summarize_reader(Cursor::new(fixture)).expect("summarize multi pair");
        assert_eq!(stats.len(), 3);

        // Verify all 3 pairs were parsed and grouped
        let pairs: Vec<&str> = stats.iter().map(|s| s.pair.as_str()).collect();
        assert!(pairs.contains(&"WETH/USDC"));
        assert!(pairs.contains(&"WETH/WBTC"));
        assert!(pairs.contains(&"WETH/USDT"));
    }
}
