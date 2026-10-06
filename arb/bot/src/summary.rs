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
    pub profit_min_usd_count: usize,
    pub above_minus_2bps_count: usize,
    pub max_bps: f64,
    pub median_bps: f64,
    pub p95_bps: f64,
    pub max_profit_usd: f64,
    pub max_run_pos: usize,
    pub max_run_above_neg1: usize,
}

fn default_pair() -> String {
    "WETH/USDC".to_string()
}

#[derive(Deserialize)]
struct OpportunityEntry {
    #[serde(default = "default_pair")]
    pub pair: String,
    #[serde(default)]
    pub block_number: u64,
    pub venue_a: String,
    pub venue_b: String,
    pub direction: String,
    pub size_weth: f64,
    pub gross_spread_bps: f64,
    #[serde(default)]
    pub est_gross_profit_usd: f64,
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
type GroupVal = (f64, Vec<(u64, f64, f64)>); // (size_weth, vec of (block_number, spread_bps, est_gross_profit_usd))

/// Summarise records with default usd_min = 0.01.
#[allow(dead_code)]
pub fn summarize_reader<R: BufRead>(reader: R) -> Result<Vec<GroupStats>> {
    summarize_reader_with_usd_min(reader, 0.01)
}

/// Summarise records from any reader yielding JSONL lines with configurable minimum USD profit threshold.
pub fn summarize_reader_with_usd_min<R: BufRead>(reader: R, usd_min: f64) -> Result<Vec<GroupStats>> {
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
        entry.1.push((rec.block_number, rec.gross_spread_bps, rec.est_gross_profit_usd));
    }

    let mut out = Vec::new();
    for ((pair, venue_a, venue_b, direction, _), (size_weth, mut rows)) in groups {
        // Sort chronologically by block number for persistence runs
        rows.sort_by_key(|r| r.0);

        let mut cur_run_pos = 0;
        let mut max_run_pos = 0;
        let mut cur_run_above_neg1 = 0;
        let mut max_run_above_neg1 = 0;

        for &(_, spread, _) in &rows {
            if spread > 0.0 {
                cur_run_pos += 1;
                if cur_run_pos > max_run_pos {
                    max_run_pos = cur_run_pos;
                }
            } else {
                cur_run_pos = 0;
            }

            if spread > -1.0 {
                cur_run_above_neg1 += 1;
                if cur_run_above_neg1 > max_run_above_neg1 {
                    max_run_above_neg1 = cur_run_above_neg1;
                }
            } else {
                cur_run_above_neg1 = 0;
            }
        }

        let mut spreads: Vec<f64> = rows.iter().map(|(_, s, _)| *s).collect();
        spreads.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let row_count = spreads.len();
        let positive_count = spreads.iter().filter(|&&s| s > 0.0).count();
        let profit_min_usd_count = rows.iter().filter(|(_, _, p)| *p >= usd_min).count();
        let above_minus_2bps_count = spreads.iter().filter(|&&s| s > -2.0).count();
        let max_bps = *spreads.last().unwrap_or(&0.0);
        let median_bps = median(&spreads);
        let p95_bps = percentile_95(&spreads);
        let max_profit_usd = rows.iter().map(|(_, _, p)| *p).fold(f64::NEG_INFINITY, f64::max);
        let max_profit_usd = if max_profit_usd.is_finite() { max_profit_usd } else { 0.0 };

        out.push(GroupStats {
            pair,
            venue_a,
            venue_b,
            direction,
            size_weth,
            row_count,
            positive_count,
            profit_min_usd_count,
            above_minus_2bps_count,
            max_bps,
            median_bps,
            p95_bps,
            max_profit_usd,
            max_run_pos,
            max_run_above_neg1,
        });
    }

    Ok(out)
}

/// Print formatted table of summary statistics, persistence view, and optional `--top N` views.
pub fn print_summary(stats: &[GroupStats], top_n: Option<usize>, usd_min: f64) {
    println!("\n══ Opportunity Summary by Pair, Venue Pair, Direction & Size ══");
    println!("   (Note: est_gross_profit_usd is computed at fixed trade sizes)\n");
    let usd_header = format!("USD >= ${:.2}", usd_min);
    println!(
        "{:<11} {:<12} {:<12} {:<8} {:>8} {:>7} {:>14} {:>13} {:>11} {:>11} {:>11}",
        "Pair", "Venue A", "Venue B", "Dir", "Size(W)", "Rows", "Spread > 0", usd_header, "Max(bps)", "Median(bps)", "P95(bps)"
    );
    println!("{:-<118}", "");

    let mut total_rows = 0;
    let mut total_positive = 0;
    let mut total_profit_min_usd = 0;

    for s in stats {
        total_rows += s.row_count;
        total_positive += s.positive_count;
        total_profit_min_usd += s.profit_min_usd_count;
        let pct_pos = if s.row_count > 0 {
            (s.positive_count as f64 / s.row_count as f64) * 100.0
        } else {
            0.0
        };
        let pos_str = format!("{} ({:>5.1}%)", s.positive_count, pct_pos);
        println!(
            "{:<11} {:<12} {:<12} {:<8} {:>8.3} {:>7} {:>14} {:>13} {:>11.2} {:>11.2} {:>11.2}",
            s.pair,
            s.venue_a,
            s.venue_b,
            s.direction,
            s.size_weth,
            s.row_count,
            pos_str,
            s.profit_min_usd_count,
            s.max_bps,
            s.median_bps,
            s.p95_bps
        );
    }
    println!("{:-<118}", "");
    println!(
        "Total rows: {}, Groups: {}, Rows with spread > 0: {}, Rows with {}: {}\n",
        total_rows,
        stats.len(),
        total_positive,
        usd_header,
        total_profit_min_usd
    );

    // Optional --top N rankings
    if let Some(n) = top_n {
        let n = n.min(stats.len());
        println!("══ Top {} Groups Ranked by Max Spread ══", n);
        println!(
            "{:<4} {:<11} {:<12} {:<12} {:<8} {:>8} {:>11} {:>11} {:>14} {:>12}",
            "Rank", "Pair", "Venue A", "Venue B", "Dir", "Size(W)", "Max(bps)", "Median(bps)", "Spread > 0", usd_header
        );
        println!("{:-<110}", "");
        let mut by_max = stats.to_vec();
        by_max.sort_by(|a, b| b.max_bps.partial_cmp(&a.max_bps).unwrap_or(std::cmp::Ordering::Equal));
        for (i, s) in by_max.iter().take(n).enumerate() {
            let pos_str = format!("{} ({:.1}%)", s.positive_count, if s.row_count > 0 { (s.positive_count as f64 / s.row_count as f64) * 100.0 } else { 0.0 });
            println!(
                "{:<4} {:<11} {:<12} {:<12} {:<8} {:>8.3} {:>11.2} {:>11.2} {:>14} {:>12}",
                i + 1, s.pair, s.venue_a, s.venue_b, s.direction, s.size_weth, s.max_bps, s.median_bps, pos_str, s.profit_min_usd_count
            );
        }

        println!("\n══ Top {} Groups Ranked by Count of Rows Above -2 bps ══", n);
        println!(
            "{:<4} {:<11} {:<12} {:<12} {:<8} {:>8} {:>16} {:>11} {:>14} {:>12}",
            "Rank", "Pair", "Venue A", "Venue B", "Dir", "Size(W)", "Rows > -2 bps", "Max(bps)", "Spread > 0", usd_header
        );
        println!("{:-<115}", "");
        let mut by_above_neg2 = stats.to_vec();
        by_above_neg2.sort_by(|a, b| {
            b.above_minus_2bps_count
                .cmp(&a.above_minus_2bps_count)
                .then_with(|| b.max_bps.partial_cmp(&a.max_bps).unwrap_or(std::cmp::Ordering::Equal))
        });
        for (i, s) in by_above_neg2.iter().take(n).enumerate() {
            let neg2_str = format!("{} ({:.1}%)", s.above_minus_2bps_count, if s.row_count > 0 { (s.above_minus_2bps_count as f64 / s.row_count as f64) * 100.0 } else { 0.0 });
            let pos_str = format!("{} ({:.1}%)", s.positive_count, if s.row_count > 0 { (s.positive_count as f64 / s.row_count as f64) * 100.0 } else { 0.0 });
            println!(
                "{:<4} {:<11} {:<12} {:<12} {:<8} {:>8.3} {:>16} {:>11.2} {:>14} {:>12}",
                i + 1, s.pair, s.venue_a, s.venue_b, s.direction, s.size_weth, neg2_str, s.max_bps, pos_str, s.profit_min_usd_count
            );
        }

        println!("\n══ Top {} Groups Ranked by Persistence (Consecutive Blocks > -1 bps & > 0 bps) ══", n);
        println!(
            "{:<4} {:<11} {:<12} {:<12} {:<8} {:>8} {:>18} {:>16} {:>11} {:>7}",
            "Rank", "Pair", "Venue A", "Venue B", "Dir", "Size(W)", "Max Run (>-1 bps)", "Max Run (>0 bps)", "Max(bps)", "Rows"
        );
        println!("{:-<115}", "");
        let mut by_persistence = stats.to_vec();
        by_persistence.sort_by(|a, b| {
            b.max_run_above_neg1
                .cmp(&a.max_run_above_neg1)
                .then_with(|| b.max_run_pos.cmp(&a.max_run_pos))
                .then_with(|| b.max_bps.partial_cmp(&a.max_bps).unwrap_or(std::cmp::Ordering::Equal))
        });
        for (i, s) in by_persistence.iter().take(n).enumerate() {
            println!(
                "{:<4} {:<11} {:<12} {:<12} {:<8} {:>8.3} {:>18} {:>16} {:>11.2} {:>7}",
                i + 1, s.pair, s.venue_a, s.venue_b, s.direction, s.size_weth, s.max_run_above_neg1, s.max_run_pos, s.max_bps, s.row_count
            );
        }
        println!();
    }
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

        let vals: Vec<f64> = (1..=20).map(|i| i as f64).collect();
        assert_eq!(percentile_95(&vals), 19.0);

        let vals100: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        assert_eq!(percentile_95(&vals100), 95.0);
    }

    #[test]
    fn test_summarize_fixture() {
        let fixture = r#"
{"venue_a":"UniV3-500","venue_b":"SushiV2","direction":"a_to_b","size_weth":0.01,"gross_spread_bps":-25.0,"est_gross_profit_usd":-0.5}
{"venue_a":"UniV3-500","venue_b":"SushiV2","direction":"a_to_b","size_weth":0.01,"gross_spread_bps":-10.0,"est_gross_profit_usd":-0.2}
{"venue_a":"UniV3-500","venue_b":"SushiV2","direction":"a_to_b","size_weth":0.01,"gross_spread_bps":5.0,"est_gross_profit_usd":1.5}
{"venue_a":"UniV3-500","venue_b":"SushiV2","direction":"a_to_b","size_weth":0.01,"gross_spread_bps":12.0,"est_gross_profit_usd":3.2}
{"venue_a":"UniV3-500","venue_b":"SushiV2","direction":"b_to_a","size_weth":0.05,"gross_spread_bps":-50.0,"est_gross_profit_usd":-5.0}
"#;
        let stats = summarize_reader_with_usd_min(Cursor::new(fixture), 1.0).expect("summarize fixture");
        assert_eq!(stats.len(), 2);

        let g1 = &stats[0];
        assert_eq!(g1.pair, "WETH/USDC");
        assert_eq!(g1.venue_a, "UniV3-500");
        assert_eq!(g1.venue_b, "SushiV2");
        assert_eq!(g1.direction, "a_to_b");
        assert_eq!(g1.size_weth, 0.01);
        assert_eq!(g1.row_count, 4);
        assert_eq!(g1.positive_count, 2);
        assert_eq!(g1.profit_min_usd_count, 2); // 1.5 and 3.2
        assert_eq!(g1.max_bps, 12.0);
        assert!((g1.median_bps - (-2.5)).abs() < 1e-6);
        assert_eq!(g1.p95_bps, 12.0);

        let g2 = &stats[1];
        assert_eq!(g2.pair, "WETH/USDC");
        assert_eq!(g2.direction, "b_to_a");
        assert_eq!(g2.size_weth, 0.05);
        assert_eq!(g2.row_count, 1);
        assert_eq!(g2.positive_count, 0);
        assert_eq!(g2.profit_min_usd_count, 0);
        assert_eq!(g2.max_bps, -50.0);
    }

    #[test]
    fn test_summarize_multi_pair_and_legacy_rows() {
        let fixture = r#"
{"venue_a":"UniV3-500","venue_b":"Pancake-100","direction":"a_to_b","size_weth":0.05,"gross_spread_bps":-12.0}
{"pair":"WETH/WBTC","venue_a":"UniV3-500","venue_b":"Pancake-100","direction":"a_to_b","size_weth":0.1,"gross_spread_bps":-1.5,"est_gross_profit_usd":-0.4}
{"pair":"WETH/USDT","venue_a":"UniV3-500","venue_b":"Pancake-500","direction":"b_to_a","size_weth":0.05,"gross_spread_bps":-8.0,"est_gross_profit_usd":-1.1}
"#;
        let stats = summarize_reader(Cursor::new(fixture)).expect("summarize multi pair");
        assert_eq!(stats.len(), 3);

        let pairs: Vec<&str> = stats.iter().map(|s| s.pair.as_str()).collect();
        assert!(pairs.contains(&"WETH/USDC"));
        assert!(pairs.contains(&"WETH/WBTC"));
        assert!(pairs.contains(&"WETH/USDT"));

        // WBTC row spread is -1.5 bps which is above -2.0 bps
        let wbtc = stats.iter().find(|s| s.pair == "WETH/WBTC").unwrap();
        assert_eq!(wbtc.above_minus_2bps_count, 1);
    }

    #[test]
    fn test_persistence_runs_with_fixture() {
        let fixture = r#"
{"block_number":100,"venue_a":"UniV3-500","venue_b":"SushiV2","direction":"a_to_b","size_weth":0.01,"gross_spread_bps":-2.0,"est_gross_profit_usd":-0.05}
{"block_number":140,"venue_a":"UniV3-500","venue_b":"SushiV2","direction":"a_to_b","size_weth":0.01,"gross_spread_bps":-0.5,"est_gross_profit_usd":-0.01}
{"block_number":180,"venue_a":"UniV3-500","venue_b":"SushiV2","direction":"a_to_b","size_weth":0.01,"gross_spread_bps":1.2,"est_gross_profit_usd":0.03}
{"block_number":220,"venue_a":"UniV3-500","venue_b":"SushiV2","direction":"a_to_b","size_weth":0.01,"gross_spread_bps":0.5,"est_gross_profit_usd":0.01}
{"block_number":260,"venue_a":"UniV3-500","venue_b":"SushiV2","direction":"a_to_b","size_weth":0.01,"gross_spread_bps":-0.8,"est_gross_profit_usd":-0.02}
{"block_number":300,"venue_a":"UniV3-500","venue_b":"SushiV2","direction":"a_to_b","size_weth":0.01,"gross_spread_bps":-3.0,"est_gross_profit_usd":-0.10}
"#;
        let stats = summarize_reader(Cursor::new(fixture)).expect("summarize fixture");
        assert_eq!(stats.len(), 1);
        let g = &stats[0];
        // Runs:
        // spread > 0: blocks 180 (1.2) and 220 (0.5) => length 2
        assert_eq!(g.max_run_pos, 2);
        // spread > -1 bps: blocks 140 (-0.5), 180 (1.2), 220 (0.5), 260 (-0.8) => length 4
        assert_eq!(g.max_run_above_neg1, 4);
    }
}
