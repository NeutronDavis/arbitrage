//! Tracing / structured logging initialisation.
//!
//! Phase 1 — sets up a minimal subscriber so `tracing::info!` works in main.

use anyhow::Result;

/// Initialise the global tracing subscriber.
///
/// Reads `RUST_LOG` for filter directives (defaults to `info`).
/// Phase 2 will add JSON formatting and per-block fields.
pub fn init() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init()
        .map_err(|e| anyhow::anyhow!("tracing init failed: {e}"))?;

    Ok(())
}
