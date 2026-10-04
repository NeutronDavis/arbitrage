//! CLI tool to summarise JSONL opportunity log files.
//!
//! # Usage
//! ```bash
//! cargo run --bin summarize
//! cargo run --bin summarize -- data/opportunities.jsonl
//! ```

use anyhow::{Context, Result};
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let file_path = if args.len() > 1 {
        PathBuf::from(&args[1])
    } else {
        PathBuf::from("data/opportunities.jsonl")
    };

    if !file_path.exists() {
        eprintln!("Error: file not found: {}", file_path.display());
        std::process::exit(1);
    }

    let file = File::open(&file_path)
        .with_context(|| format!("Cannot open file: {}", file_path.display()))?;
    let reader = BufReader::new(file);

    let stats = arb_bot::summary::summarize_reader(reader)
        .with_context(|| format!("Failed to summarise {}", file_path.display()))?;

    println!("File: {}", file_path.display());
    arb_bot::summary::print_summary(&stats);

    Ok(())
}
