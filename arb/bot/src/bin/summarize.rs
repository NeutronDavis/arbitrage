//! CLI tool to summarise JSONL opportunity log files.
//!
//! # Usage
//! ```bash
//! cargo run --bin summarize
//! cargo run --bin summarize -- data/opportunities.jsonl
//! cargo run --bin summarize -- data/opportunities.jsonl --top 10
//! ```

use anyhow::{Context, Result};
use clap::Parser;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "summarize", about = "Summarise JSONL opportunity log files")]
struct Args {
    /// Path to JSONL opportunity log file (default: data/opportunities.jsonl).
    #[arg(default_value = "data/opportunities.jsonl")]
    file: PathBuf,

    /// Show top N ranked groups by max spread, rows above -2 bps, and persistence.
    #[arg(long)]
    top: Option<usize>,

    /// Minimum estimated gross profit in USD for screening (default: 0.01).
    #[arg(long, default_value_t = 0.01)]
    usd_min: f64,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if !args.file.exists() {
        eprintln!("Error: file not found: {}", args.file.display());
        std::process::exit(1);
    }

    let file = File::open(&args.file)
        .with_context(|| format!("Cannot open file: {}", args.file.display()))?;
    let reader = BufReader::new(file);

    let stats = arb_bot::summary::summarize_reader_with_usd_min(reader, args.usd_min)
        .with_context(|| format!("Failed to summarise {}", args.file.display()))?;

    println!("File: {}", args.file.display());
    arb_bot::summary::print_summary(&stats, args.top, args.usd_min);

    Ok(())
}
