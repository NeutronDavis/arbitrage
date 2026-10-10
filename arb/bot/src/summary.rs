//! JSONL opportunity log analysis, stats snapshotting, and statistical summarisation.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const HISTOGRAM_MIN: f64 = -100.0;
pub const HISTOGRAM_MAX: f64 = 10.0;
pub const HISTOGRAM_BUCKET_WIDTH: f64 = 0.5;
pub const HISTOGRAM_NUM_BUCKETS: usize = 220; // (-100 to +10) / 0.5 = 220

/// Histogram of gross spreads in 0.5 bps buckets from -100 to +10 bps,
/// with underflow (< -100 bps) and overflow (>= +10 bps) buckets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpreadHistogram {
    pub underflow: u64,
    pub buckets: Vec<u64>,
    pub overflow: u64,
}

impl Default for SpreadHistogram {
    fn default() -> Self {
        Self {
            underflow: 0,
            buckets: vec![0; HISTOGRAM_NUM_BUCKETS],
            overflow: 0,
        }
    }
}

impl SpreadHistogram {
    /// Record a spread value into the histogram.
    pub fn record(&mut self, spread: f64) {
        if spread < HISTOGRAM_MIN {
            self.underflow += 1;
        } else if spread >= HISTOGRAM_MAX {
            self.overflow += 1;
        } else {
            let idx = ((spread - HISTOGRAM_MIN) / HISTOGRAM_BUCKET_WIDTH).floor() as usize;
            if idx < self.buckets.len() {
                self.buckets[idx] += 1;
            } else {
                self.overflow += 1;
            }
        }
    }

    /// Merge another histogram into this one.
    #[allow(dead_code)]
    pub fn merge(&mut self, other: &Self) {
        self.underflow += other.underflow;
        self.overflow += other.overflow;
        if self.buckets.len() < other.buckets.len() {
            self.buckets.resize(other.buckets.len(), 0);
        }
        for (i, &cnt) in other.buckets.iter().enumerate() {
            if i < self.buckets.len() {
                self.buckets[i] += cnt;
            }
        }
    }

    /// Total number of samples recorded across underflow, buckets, and overflow.
    pub fn total_count(&self) -> u64 {
        self.underflow + self.buckets.iter().sum::<u64>() + self.overflow
    }

    /// Approximate percentile (0.0 to 1.0) using linear interpolation within the bucket.
    pub fn approximate_percentile(&self, p: f64) -> f64 {
        let total = self.total_count();
        if total == 0 {
            return 0.0;
        }
        let target_rank = (p * total as f64).max(0.5);
        let mut cum = 0.0;

        cum += self.underflow as f64;
        if cum >= target_rank {
            return HISTOGRAM_MIN;
        }

        for (i, &cnt) in self.buckets.iter().enumerate() {
            let bucket_low = HISTOGRAM_MIN + (i as f64) * HISTOGRAM_BUCKET_WIDTH;
            let cnt_f = cnt as f64;
            if cum + cnt_f >= target_rank {
                let fraction = if cnt > 0 {
                    (target_rank - cum) / cnt_f
                } else {
                    0.5
                };
                return bucket_low + fraction * HISTOGRAM_BUCKET_WIDTH;
            }
            cum += cnt_f;
        }

        HISTOGRAM_MAX
    }

    /// Approximate median spread in basis points.
    pub fn approximate_median(&self) -> f64 {
        self.approximate_percentile(0.50)
    }

    /// Approximate P95 spread in basis points.
    pub fn approximate_p95(&self) -> f64 {
        self.approximate_percentile(0.95)
    }
}

/// Accumulated statistics for a specific (pair, venues, direction, size) group.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GroupSpreadStats {
    pub pair: String,
    pub venue_a: String,
    pub venue_b: String,
    pub direction: String,
    pub size_weth: f64,
    pub sample_count: u64,
    pub max: f64,
    pub min: f64,
    pub sum: f64,
    pub histogram: SpreadHistogram,
}

/// Persistent stats snapshot across runs.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct StatsSnapshot {
    pub updated_at: u64,
    pub groups: Vec<GroupSpreadStats>,
}

impl StatsSnapshot {
    /// Record a single opportunity observation into the snapshot.
    pub fn record_sample(
        &mut self,
        pair: &str,
        venue_a: &str,
        venue_b: &str,
        direction: &str,
        size_weth: f64,
        spread: f64,
    ) {
        let size_key = (size_weth * 1e9).round() as u64;
        let entry = self.groups.iter_mut().find(|g| {
            g.pair == pair
                && g.venue_a == venue_a
                && g.venue_b == venue_b
                && g.direction == direction
                && (g.size_weth * 1e9).round() as u64 == size_key
        });

        if let Some(g) = entry {
            g.sample_count += 1;
            if spread > g.max {
                g.max = spread;
            }
            if spread < g.min {
                g.min = spread;
            }
            g.sum += spread;
            g.histogram.record(spread);
        } else {
            let mut hist = SpreadHistogram::default();
            hist.record(spread);
            self.groups.push(GroupSpreadStats {
                pair: pair.to_string(),
                venue_a: venue_a.to_string(),
                venue_b: venue_b.to_string(),
                direction: direction.to_string(),
                size_weth,
                sample_count: 1,
                max: spread,
                min: spread,
                sum: spread,
                histogram: hist,
            });
        }
    }

    /// Merge another stats snapshot into this one.
    #[allow(dead_code)]
    pub fn merge(&mut self, other: &Self) {
        for og in &other.groups {
            let size_key = (og.size_weth * 1e9).round() as u64;
            if let Some(g) = self.groups.iter_mut().find(|g| {
                g.pair == og.pair
                    && g.venue_a == og.venue_a
                    && g.venue_b == og.venue_b
                    && g.direction == og.direction
                    && (g.size_weth * 1e9).round() as u64 == size_key
            }) {
                g.sample_count += og.sample_count;
                g.max = g.max.max(og.max);
                g.min = g.min.min(og.min);
                g.sum += og.sum;
                g.histogram.merge(&og.histogram);
            } else {
                self.groups.push(og.clone());
            }
        }
    }

    /// Atomically write snapshot to `<OUTPUT_FILE>.stats.json` via a temp file and rename.
    pub fn save_atomic<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Cannot create dir {:?}", parent))?;
        }
        let temp_path = parent.join(format!(
            ".{}.tmp.{}",
            path.file_name().and_then(|f| f.to_str()).unwrap_or("stats"),
            std::process::id()
        ));

        let mut to_write = self.clone();
        to_write.updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let json_bytes = serde_json::to_vec_pretty(&to_write)?;
        {
            let mut file = std::fs::File::create(&temp_path)
                .with_context(|| format!("Failed to create temp stats file {:?}", temp_path))?;
            file.write_all(&json_bytes)?;
            file.sync_all()?;
        }

        // Atomic rename / replace
        if let Err(_e) = std::fs::rename(&temp_path, path) {
            let _ = std::fs::remove_file(path);
            std::fs::rename(&temp_path, path)
                .with_context(|| format!("Failed to atomically rename {:?} to {:?}", temp_path, path))?;
        }
        Ok(())
    }

    /// Load existing stats file on startup if present, otherwise default to empty snapshot.
    pub fn load_or_default<P: AsRef<Path>>(path: P) -> Self {
        let path = path.as_ref();
        if !path.exists() {
            return Self::default();
        }
        match std::fs::File::open(path) {
            Ok(file) => {
                let reader = std::io::BufReader::new(file);
                match serde_json::from_reader(reader) {
                    Ok(snapshot) => {
                        tracing::info!(path = ?path, "Loaded existing stats snapshot");
                        snapshot
                    }
                    Err(e) => {
                        tracing::warn!(path = ?path, error = %e, "Failed to parse existing stats file, starting fresh");
                        Self::default()
                    }
                }
            }
            Err(e) => {
                tracing::warn!(path = ?path, error = %e, "Could not open existing stats file, starting fresh");
                Self::default()
            }
        }
    }
}

/// Summary statistics for a single (pair, venue_a, venue_b, direction, size) group.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupStats {
    pub pair: String,
    pub venue_a: String,
    pub venue_b: String,
    pub direction: String,
    pub size_weth: f64,
    pub blocks_seen: usize,
    pub row_count: usize,
    pub positive_count: usize,
    pub profit_min_usd_count: usize,
    pub above_minus_2bps_count: usize,
    pub max_bps: f64,
    pub median_bps: f64,
    pub p95_bps: f64,
    pub is_approximate: bool,
    pub max_profit_usd: f64,
    pub max_run_pos: usize,
    pub max_run_above_neg1: usize,
}

/// Per-market summary metrics extracted from heartbeat file.
#[derive(Debug, Clone, PartialEq)]
pub struct MarketHeartbeatSummary {
    pub pair: String,
    pub blocks: usize,
    pub min_bps: f64,
    pub median_bps: f64,
    pub max_bps: f64,
    pub count_above_neg2: usize,
    pub count_above_neg1: usize,
    pub count_above_zero: usize,
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
            blocks_seen: row_count,
            row_count,
            positive_count,
            profit_min_usd_count,
            above_minus_2bps_count,
            max_bps,
            median_bps,
            p95_bps,
            is_approximate: false,
            max_profit_usd,
            max_run_pos,
            max_run_above_neg1,
        });
    }

    Ok(out)
}

/// Summarise heartbeat records into per-market statistics.
pub fn summarize_heartbeat_records(
    heartbeats: &[crate::strategy::HeartbeatRecord],
) -> Vec<MarketHeartbeatSummary> {
    let mut by_pair: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for hb in heartbeats {
        by_pair.entry(hb.pair.clone()).or_default().push(hb.best_spread_bps);
    }

    let mut out = Vec::new();
    for (pair, mut spreads) in by_pair {
        spreads.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let blocks = spreads.len();
        let min_bps = *spreads.first().unwrap_or(&0.0);
        let max_bps = *spreads.last().unwrap_or(&0.0);
        let median_bps = median(&spreads);
        let count_above_neg2 = spreads.iter().filter(|&&s| s > -2.0).count();
        let count_above_neg1 = spreads.iter().filter(|&&s| s > -1.0).count();
        let count_above_zero = spreads.iter().filter(|&&s| s > 0.0).count();

        out.push(MarketHeartbeatSummary {
            pair,
            blocks,
            min_bps,
            median_bps,
            max_bps,
            count_above_neg2,
            count_above_neg1,
            count_above_zero,
        });
    }
    out
}

/// Comprehensive summariser reading the rows file, heartbeat file, and stats snapshot file.
pub fn summarize_from_paths<P: AsRef<Path>>(
    rows_path: P,
    usd_min: f64,
) -> Result<(Vec<GroupStats>, Vec<MarketHeartbeatSummary>)> {
    let rows_path = rows_path.as_ref();
    let path_str = rows_path.to_string_lossy().to_string();

    // 1. Locate heartbeat file
    let hb_candidates = [
        format!("{}.heartbeat.jsonl", path_str),
        path_str
            .strip_suffix(".jsonl")
            .map(|b| format!("{}.heartbeat.jsonl", b))
            .unwrap_or_default(),
    ];
    let hb_path = hb_candidates
        .into_iter()
        .find(|p| !p.is_empty() && Path::new(p).exists());

    // 2. Locate stats file
    let stats_candidates = [
        format!("{}.stats.json", path_str),
        path_str
            .strip_suffix(".jsonl")
            .map(|b| format!("{}.stats.json", b))
            .unwrap_or_default(),
    ];
    let stats_path = stats_candidates
        .into_iter()
        .find(|p| !p.is_empty() && Path::new(p).exists());

    // 3. Load heartbeat records
    let mut heartbeat_records = Vec::new();
    let mut processed_blocks = BTreeSet::new();

    if let Some(ref hp) = hb_path {
        if let Ok(file) = std::fs::File::open(hp) {
            let reader = std::io::BufReader::new(file);
            for l in reader.lines().map_while(Result::ok) {
                let trimmed = l.trim();
                if !trimmed.is_empty() {
                    if let Ok(hb) = serde_json::from_str::<crate::strategy::HeartbeatRecord>(trimmed) {
                        processed_blocks.insert(hb.block_number);
                        heartbeat_records.push(hb);
                    }
                }
            }
        }
    }
    let market_summaries = summarize_heartbeat_records(&heartbeat_records);

    // 4. Load stats snapshot if available
    let stats_snapshot = stats_path.as_ref().map(StatsSnapshot::load_or_default);

    // 5. Read rows file
    let file = std::fs::File::open(rows_path)
        .with_context(|| format!("Cannot open JSONL file: {:?}", rows_path))?;
    let reader = std::io::BufReader::new(file);

    let mut groups: BTreeMap<GroupKey, GroupVal> = BTreeMap::new();
    for (line_idx, line_res) in reader.lines().enumerate() {
        let line = line_res?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let rec: OpportunityEntry = serde_json::from_str(trimmed)
            .with_context(|| format!("Failed to parse JSON record at line {}", line_idx + 1))?;

        if hb_path.is_none() && rec.block_number > 0 {
            processed_blocks.insert(rec.block_number);
        }

        let size_key = (rec.size_weth * 1e9).round() as u64;
        let entry = groups
            .entry((rec.pair, rec.venue_a, rec.venue_b, rec.direction, size_key))
            .or_insert_with(|| (rec.size_weth, Vec::new()));
        entry.1.push((rec.block_number, rec.gross_spread_bps, rec.est_gross_profit_usd));
    }

    // Include groups present in stats snapshot even if 0 rows met the logging threshold
    if let Some(ref snap) = stats_snapshot {
        for sg in &snap.groups {
            let size_key = (sg.size_weth * 1e9).round() as u64;
            let key = (
                sg.pair.clone(),
                sg.venue_a.clone(),
                sg.venue_b.clone(),
                sg.direction.clone(),
                size_key,
            );
            groups.entry(key).or_insert_with(|| (sg.size_weth, Vec::new()));
        }
    }

    let all_blocks: Vec<u64> = processed_blocks.into_iter().collect();
    let mut out = Vec::new();

    for ((pair, venue_a, venue_b, direction, size_key), (size_weth, mut rows)) in groups {
        rows.sort_by_key(|r| r.0);

        let mut block_spread_map = std::collections::HashMap::new();
        for &(blk, spread, _) in &rows {
            block_spread_map.insert(blk, spread);
        }

        // Persistence runs: treating a block with no logged row for that group as below threshold
        let mut cur_run_pos = 0;
        let mut max_run_pos = 0;
        let mut cur_run_above_neg1 = 0;
        let mut max_run_above_neg1 = 0;

        if !all_blocks.is_empty() {
            for &blk in &all_blocks {
                let spread = block_spread_map.get(&blk).copied().unwrap_or(-999.0);
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
        } else {
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
        }

        let mut spreads: Vec<f64> = rows.iter().map(|(_, s, _)| *s).collect();
        spreads.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let row_count = spreads.len();
        let positive_count = spreads.iter().filter(|&&s| s > 0.0).count();
        let profit_min_usd_count = rows.iter().filter(|(_, _, p)| *p >= usd_min).count();
        let above_minus_2bps_count = spreads.iter().filter(|&&s| s > -2.0).count();
        let max_profit_usd = rows.iter().map(|(_, _, p)| *p).fold(f64::NEG_INFINITY, f64::max);
        let max_profit_usd = if max_profit_usd.is_finite() { max_profit_usd } else { 0.0 };

        // Match with stats snapshot entry if available
        let stats_entry = stats_snapshot.as_ref().and_then(|snap| {
            snap.groups.iter().find(|sg| {
                sg.pair == pair
                    && sg.venue_a == venue_a
                    && sg.venue_b == venue_b
                    && sg.direction == direction
                    && (sg.size_weth * 1e9).round() as u64 == size_key
            })
        });

        let row_max = spreads.last().copied();
        let (blocks_seen, max_bps, median_bps, p95_bps, is_approximate) = if let Some(se) = stats_entry {
            let true_max = match row_max {
                Some(rm) => se.max.max(rm),
                None => se.max,
            };
            let true_blocks = if !all_blocks.is_empty() {
                all_blocks.len().max(se.sample_count as usize)
            } else {
                se.sample_count as usize
            };
            (
                true_blocks,
                true_max,
                se.histogram.approximate_median(),
                se.histogram.approximate_p95(),
                true,
            )
        } else {
            (
                if !all_blocks.is_empty() { all_blocks.len() } else { row_count },
                row_max.unwrap_or(0.0),
                median(&spreads),
                percentile_95(&spreads),
                false,
            )
        };

        out.push(GroupStats {
            pair,
            venue_a,
            venue_b,
            direction,
            size_weth,
            blocks_seen,
            row_count,
            positive_count,
            profit_min_usd_count,
            above_minus_2bps_count,
            max_bps,
            median_bps,
            p95_bps,
            is_approximate,
            max_profit_usd,
            max_run_pos,
            max_run_above_neg1,
        });
    }

    Ok((out, market_summaries))
}

/// Print formatted per-market table from the heartbeat log.
pub fn print_market_heartbeat_table(summaries: &[MarketHeartbeatSummary]) {
    if summaries.is_empty() {
        return;
    }
    println!("\n══ Per-Market Best Spread Summary (from Heartbeat) ══");
    println!(
        "{:<12} {:>8} {:>11} {:>11} {:>11} {:>17} {:>17} {:>16}",
        "Pair", "Blocks", "Min(bps)", "Median(bps)", "Max(bps)", "Blocks > -2 bps", "Blocks > -1 bps", "Blocks > 0 bps"
    );
    println!("{:-<107}", "");
    for s in summaries {
        let neg2_str = format!(
            "{} ({:>5.1}%)",
            s.count_above_neg2,
            if s.blocks > 0 { (s.count_above_neg2 as f64 / s.blocks as f64) * 100.0 } else { 0.0 }
        );
        let neg1_str = format!(
            "{} ({:>5.1}%)",
            s.count_above_neg1,
            if s.blocks > 0 { (s.count_above_neg1 as f64 / s.blocks as f64) * 100.0 } else { 0.0 }
        );
        let zero_str = format!(
            "{} ({:>5.1}%)",
            s.count_above_zero,
            if s.blocks > 0 { (s.count_above_zero as f64 / s.blocks as f64) * 100.0 } else { 0.0 }
        );
        println!(
            "{:<12} {:>8} {:>11.2} {:>11.2} {:>11.2} {:>17} {:>17} {:>16}",
            s.pair, s.blocks, s.min_bps, s.median_bps, s.max_bps, neg2_str, neg1_str, zero_str
        );
    }
    println!();
}

/// Print formatted table of summary statistics, persistence view, and optional `--top N` views.
pub fn print_summary(stats: &[GroupStats], top_n: Option<usize>, usd_min: f64) {
    println!("\n══ Opportunity Summary by Pair, Venue Pair, Direction & Size ══");
    println!("   (Note: est_gross_profit_usd is computed at fixed trade sizes)\n");
    let usd_header = format!("USD >= ${:.2}", usd_min);
    println!(
        "{:<11} {:<12} {:<12} {:<8} {:>8} {:>7} {:>7} {:>14} {:>13} {:>11} {:>13} {:>13}",
        "Pair", "Venue A", "Venue B", "Dir", "Size(W)", "Blocks", "Rows", "Spread > 0", usd_header, "Max(bps)", "Median(bps)*", "P95(bps)*"
    );
    println!("{:-<135}", "");

    let total_blocks = stats.iter().map(|s| s.blocks_seen).max().unwrap_or(0);
    let mut total_rows = 0;
    let mut total_positive = 0;
    let mut total_profit_min_usd = 0;
    let mut any_approximate = false;

    for s in stats {
        total_rows += s.row_count;
        total_positive += s.positive_count;
        total_profit_min_usd += s.profit_min_usd_count;
        if s.is_approximate {
            any_approximate = true;
        }
        let pct_pos = if s.blocks_seen > 0 {
            (s.positive_count as f64 / s.blocks_seen as f64) * 100.0
        } else if s.row_count > 0 {
            (s.positive_count as f64 / s.row_count as f64) * 100.0
        } else {
            0.0
        };
        let pos_str = format!("{} ({:>5.1}%)", s.positive_count, pct_pos);
        println!(
            "{:<11} {:<12} {:<12} {:<8} {:>8.3} {:>7} {:>7} {:>14} {:>13} {:>11.2} {:>13.2} {:>13.2}",
            s.pair,
            s.venue_a,
            s.venue_b,
            s.direction,
            s.size_weth,
            s.blocks_seen,
            s.row_count,
            pos_str,
            s.profit_min_usd_count,
            s.max_bps,
            s.median_bps,
            s.p95_bps
        );
    }
    println!("{:-<135}", "");
    println!(
        "Total blocks seen: {}, Total rows logged: {}, Groups: {}, Rows with spread > 0: {}, Rows with {}: {}\n",
        total_blocks,
        total_rows,
        stats.len(),
        total_positive,
        usd_header,
        total_profit_min_usd
    );
    if any_approximate {
        println!("* Note: Median and P95 values are APPROXIMATE, calculated from the 0.5 bps bucket histogram.\n");
    }

    // Optional --top N rankings
    if let Some(n) = top_n {
        let n = n.min(stats.len());
        println!("══ Top {} Groups Ranked by Max Spread ══", n);
        println!(
            "{:<4} {:<11} {:<12} {:<12} {:<8} {:>8} {:>11} {:>13} {:>14} {:>12}",
            "Rank", "Pair", "Venue A", "Venue B", "Dir", "Size(W)", "Max(bps)", "Median(bps)*", "Spread > 0", usd_header
        );
        println!("{:-<112}", "");
        let mut by_max = stats.to_vec();
        by_max.sort_by(|a, b| b.max_bps.partial_cmp(&a.max_bps).unwrap_or(std::cmp::Ordering::Equal));
        for (i, s) in by_max.iter().take(n).enumerate() {
            let pct = if s.blocks_seen > 0 {
                (s.positive_count as f64 / s.blocks_seen as f64) * 100.0
            } else if s.row_count > 0 {
                (s.positive_count as f64 / s.row_count as f64) * 100.0
            } else {
                0.0
            };
            let pos_str = format!("{} ({:.1}%)", s.positive_count, pct);
            println!(
                "{:<4} {:<11} {:<12} {:<12} {:<8} {:>8.3} {:>11.2} {:>13.2} {:>14} {:>12}",
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
            "{:<4} {:<11} {:<12} {:<12} {:<8} {:>8} {:>18} {:>16} {:>11} {:>8} {:>7}",
            "Rank", "Pair", "Venue A", "Venue B", "Dir", "Size(W)", "Max Run (>-1 bps)", "Max Run (>0 bps)", "Max(bps)", "Blocks", "Rows"
        );
        println!("{:-<125}", "");
        let mut by_persistence = stats.to_vec();
        by_persistence.sort_by(|a, b| {
            b.max_run_above_neg1
                .cmp(&a.max_run_above_neg1)
                .then_with(|| b.max_run_pos.cmp(&a.max_run_pos))
                .then_with(|| b.max_bps.partial_cmp(&a.max_bps).unwrap_or(std::cmp::Ordering::Equal))
        });
        for (i, s) in by_persistence.iter().take(n).enumerate() {
            println!(
                "{:<4} {:<11} {:<12} {:<12} {:<8} {:>8.3} {:>18} {:>16} {:>11.2} {:>8} {:>7}",
                i + 1, s.pair, s.venue_a, s.venue_b, s.direction, s.size_weth, s.max_run_above_neg1, s.max_run_pos, s.max_bps, s.blocks_seen, s.row_count
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
        assert_eq!(g1.profit_min_usd_count, 2);
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
        assert_eq!(g.max_run_pos, 2);
        assert_eq!(g.max_run_above_neg1, 4);
    }

    #[test]
    fn test_histogram_accumulation_and_reload_restart() {
        let mut snap = StatsSnapshot::default();
        snap.record_sample("WETH/USDC", "UniV3-500", "Pancake-100", "a_to_b", 0.05, -120.0); // underflow
        snap.record_sample("WETH/USDC", "UniV3-500", "Pancake-100", "a_to_b", 0.05, -1.2);
        snap.record_sample("WETH/USDC", "UniV3-500", "Pancake-100", "a_to_b", 0.05, 0.4);
        snap.record_sample("WETH/USDC", "UniV3-500", "Pancake-100", "a_to_b", 0.05, 15.0); // overflow

        assert_eq!(snap.groups.len(), 1);
        let g = &snap.groups[0];
        assert_eq!(g.sample_count, 4);
        assert_eq!(g.histogram.underflow, 1);
        assert_eq!(g.histogram.overflow, 1);
        assert_eq!(g.histogram.total_count(), 4);

        // Atomic write to temp file
        let temp_dir = std::env::temp_dir();
        let stats_path = temp_dir.join(format!(
            "arb_test_stats_{}.stats.json",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));

        snap.save_atomic(&stats_path).expect("save atomic stats");

        // Simulate restart: reload from file
        let mut reloaded = StatsSnapshot::load_or_default(&stats_path);
        assert_eq!(reloaded.groups.len(), 1);
        assert_eq!(reloaded.groups[0].sample_count, 4);
        assert_eq!(reloaded.groups[0].max, 15.0);
        assert_eq!(reloaded.groups[0].min, -120.0);

        // Continue accumulating after restart
        reloaded.record_sample("WETH/USDC", "UniV3-500", "Pancake-100", "a_to_b", 0.05, -0.6);
        reloaded.record_sample("WETH/USDC", "UniV3-500", "Pancake-100", "a_to_b", 0.05, 2.0);

        let g_acc = &reloaded.groups[0];
        assert_eq!(g_acc.sample_count, 6);
        assert_eq!(g_acc.histogram.total_count(), 6);

        // Approximate percentiles
        let med = g_acc.histogram.approximate_median();
        let p95 = g_acc.histogram.approximate_p95();
        assert!(med < 2.0 && med > -5.0, "approx median={med}");
        assert!(p95 > med, "p95 must be greater than median");

        let _ = std::fs::remove_file(&stats_path);
    }

    #[test]
    fn test_summarizer_filtered_fixture_and_persistence() {
        let temp_dir = std::env::temp_dir();
        let id = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let rows_path = temp_dir.join(format!("test_filtered_{}.jsonl", id));
        let hb_path = temp_dir.join(format!("test_filtered_{}.jsonl.heartbeat.jsonl", id));
        let stats_path = temp_dir.join(format!("test_filtered_{}.jsonl.stats.json", id));

        // 4 processed blocks: 100, 140, 180, 220
        // Block 100: group has spread +0.5 bps (logged in rows)
        // Block 140: group has spread -5.0 bps (< -2.0 threshold -> NOT in rows!)
        // Block 180: group has spread +1.0 bps (logged in rows)
        // Block 220: group has spread +0.8 bps (logged in rows)
        let rows_content = r#"
{"timestamp":1000,"logged_at":1001,"block_number":100,"pair":"WETH/USDC","venue_a":"UniV3-500","venue_b":"Pancake-100","direction":"a_to_b","size_weth":0.05,"gross_spread_bps":0.5,"est_gross_profit_usd":0.02}
{"timestamp":1080,"logged_at":1081,"block_number":180,"pair":"WETH/USDC","venue_a":"UniV3-500","venue_b":"Pancake-100","direction":"a_to_b","size_weth":0.05,"gross_spread_bps":1.0,"est_gross_profit_usd":0.05}
{"timestamp":1120,"logged_at":1121,"block_number":220,"pair":"WETH/USDC","venue_a":"UniV3-500","venue_b":"Pancake-100","direction":"a_to_b","size_weth":0.05,"gross_spread_bps":0.8,"est_gross_profit_usd":0.04}
"#;
        std::fs::write(&rows_path, rows_content.trim()).expect("write rows");

        // Heartbeat records all 4 blocks
        let hb_content = r#"
{"timestamp":1000,"block_number":100,"pair":"WETH/USDC","best_spread_bps":0.5,"best_venue_a":"UniV3-500","best_venue_b":"Pancake-100","best_direction":"a_to_b","best_size_weth":0.05}
{"timestamp":1040,"block_number":140,"pair":"WETH/USDC","best_spread_bps":-5.0,"best_venue_a":"UniV3-500","best_venue_b":"Pancake-100","best_direction":"a_to_b","best_size_weth":0.05}
{"timestamp":1080,"block_number":180,"pair":"WETH/USDC","best_spread_bps":1.0,"best_venue_a":"UniV3-500","best_venue_b":"Pancake-100","best_direction":"a_to_b","best_size_weth":0.05}
{"timestamp":1120,"block_number":220,"pair":"WETH/USDC","best_spread_bps":0.8,"best_venue_a":"UniV3-500","best_venue_b":"Pancake-100","best_direction":"a_to_b","best_size_weth":0.05}
"#;
        std::fs::write(&hb_path, hb_content.trim()).expect("write heartbeat");

        // Stats records all 4 samples
        let mut snap = StatsSnapshot::default();
        snap.record_sample("WETH/USDC", "UniV3-500", "Pancake-100", "a_to_b", 0.05, 0.5);
        snap.record_sample("WETH/USDC", "UniV3-500", "Pancake-100", "a_to_b", 0.05, -5.0);
        snap.record_sample("WETH/USDC", "UniV3-500", "Pancake-100", "a_to_b", 0.05, 1.0);
        snap.record_sample("WETH/USDC", "UniV3-500", "Pancake-100", "a_to_b", 0.05, 0.8);
        snap.save_atomic(&stats_path).expect("write stats");

        let (stats, hb_summaries) = summarize_from_paths(&rows_path, 0.01).expect("summarize from paths");
        assert_eq!(stats.len(), 1);
        let g = &stats[0];
        assert_eq!(g.blocks_seen, 4);
        assert_eq!(g.row_count, 3); // 3 rows logged
        assert_eq!(g.positive_count, 3);
        assert!(g.is_approximate);

        // Persistence run:
        // Block 100: positive (run = 1)
        // Block 140: NO ROW in rows file -> treated as below threshold (< 0 and < -1) -> run resets to 0!
        // Block 180: positive (run = 1)
        // Block 220: positive (run = 2)
        // Max run = 2 (NOT 3, because block 140 broke the run!)
        assert_eq!(g.max_run_pos, 2);
        assert_eq!(g.max_run_above_neg1, 2);

        // Heartbeat summary check
        assert_eq!(hb_summaries.len(), 1);
        let hb_s = &hb_summaries[0];
        assert_eq!(hb_s.blocks, 4);
        assert_eq!(hb_s.min_bps, -5.0);
        assert_eq!(hb_s.max_bps, 1.0);
        assert_eq!(hb_s.count_above_zero, 3); // blocks 100, 180, 220

        let _ = std::fs::remove_file(&rows_path);
        let _ = std::fs::remove_file(&hb_path);
        let _ = std::fs::remove_file(&stats_path);
    }

    #[test]
    fn test_summarizer_reads_old_format_file() {
        let temp_dir = std::env::temp_dir();
        let rows_path = temp_dir.join(format!(
            "test_old_format_{}.jsonl",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));

        // Legacy format without companion heartbeat or stats files
        let legacy_content = r#"
{"block_number":100,"pair":"WETH/USDC","venue_a":"UniV3-500","venue_b":"SushiV2","direction":"a_to_b","size_weth":0.01,"gross_spread_bps":-2.0}
{"block_number":140,"pair":"WETH/USDC","venue_a":"UniV3-500","venue_b":"SushiV2","direction":"a_to_b","size_weth":0.01,"gross_spread_bps":1.5}
"#;
        std::fs::write(&rows_path, legacy_content.trim()).expect("write legacy");

        let (stats, hb_summaries) = summarize_from_paths(&rows_path, 0.01).expect("summarize old format");
        assert_eq!(stats.len(), 1);
        let g = &stats[0];
        assert_eq!(g.blocks_seen, 2);
        assert_eq!(g.row_count, 2);
        assert_eq!(g.positive_count, 1);
        assert!(!g.is_approximate); // exact from rows
        assert_eq!(hb_summaries.len(), 0); // no heartbeat file

        let _ = std::fs::remove_file(&rows_path);
    }

    #[test]
    fn test_reconciliation_exact_max_overrides_stale_stats() {
        let temp_dir = std::env::temp_dir();
        let id = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let rows_path = temp_dir.join(format!("test_reconcile_{}.jsonl", id));
        let stats_path = temp_dir.join(format!("test_reconcile_{}.jsonl.stats.json", id));
        let hb_path = temp_dir.join(format!("test_reconcile_{}.jsonl.heartbeat.jsonl", id));

        // 1. Heartbeat has 14 blocks
        let mut hb_content = String::new();
        for b in 1..=14 {
            let spread = if b == 10 { -1.31 } else { -2.5 };
            hb_content.push_str(&format!(
                "{{\"timestamp\":{},\"block_number\":{},\"pair\":\"WETH/USDC\",\"best_spread_bps\":{},\"best_venue_a\":\"UniV3-100\",\"best_venue_b\":\"Pancake-100\",\"best_direction\":\"a_to_b\",\"best_size_weth\":0.01}}\n",
                1000 + b * 10, b, spread
            ));
        }
        std::fs::write(&hb_path, hb_content.trim()).expect("write heartbeat");

        // 2. Stats snapshot written early on block 1 with max = -2.72
        let mut snap = StatsSnapshot::default();
        snap.record_sample("WETH/USDC", "UniV3-100", "Pancake-100", "a_to_b", 0.01, -2.72);
        snap.save_atomic(&stats_path).expect("write stale stats");

        // 3. Rows file has 3 rows that met threshold >= -2.0, with max = -1.31
        let rows_content = r#"
{"block_number":8,"pair":"WETH/USDC","venue_a":"UniV3-100","venue_b":"Pancake-100","direction":"a_to_b","size_weth":0.01,"gross_spread_bps":-1.9}
{"block_number":10,"pair":"WETH/USDC","venue_a":"UniV3-100","venue_b":"Pancake-100","direction":"a_to_b","size_weth":0.01,"gross_spread_bps":-1.31}
{"block_number":12,"pair":"WETH/USDC","venue_a":"UniV3-100","venue_b":"Pancake-100","direction":"a_to_b","size_weth":0.01,"gross_spread_bps":-1.7}
"#;
        std::fs::write(&rows_path, rows_content.trim()).expect("write rows");

        let (stats, hb_summaries) = summarize_from_paths(&rows_path, 0.01).expect("summarize from paths");
        assert_eq!(stats.len(), 1);
        let g = &stats[0];

        // CRITICAL: Max(bps) must be exactly -1.31 (from the logged rows), NOT the stale -2.72!
        assert!((g.max_bps - (-1.31)).abs() < 1e-6, "Expected max -1.31 but got {}", g.max_bps);

        // Blocks seen for market in heartbeat must be 14
        assert_eq!(hb_summaries.len(), 1);
        assert_eq!(hb_summaries[0].blocks, 14);

        let _ = std::fs::remove_file(&rows_path);
        let _ = std::fs::remove_file(&hb_path);
        let _ = std::fs::remove_file(&stats_path);
    }
}
