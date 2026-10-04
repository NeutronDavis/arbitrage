//! HTTP and WebSocket provider construction with auto-reconnect.
//!
//! Phase 2 implementation.
//!
//! In alloy 2.5, `RootProvider<N: Network = Ethereum>` handles both HTTP and WS;
//! the transport is embedded in the internal `RpcClient`, not the type parameter.
//! RPC URLs are never printed or logged (they contain API keys).

use anyhow::{Context, Result};
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use futures_util::StreamExt;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};

/// Replace every `http(s)://` or `ws(s)://` URL in `s` with `<redacted-url>`.
///
/// Transport errors (e.g. reqwest's "error sending request for url (...)") embed
/// the full RPC URL, which contains the API key. Run every error string through
/// this before it reaches a log line or stderr.
pub fn redact_urls(s: &str) -> String {
    const SCHEMES: [&str; 4] = ["https://", "http://", "wss://", "ws://"];
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        // Find the earliest scheme occurrence in the remaining text.
        let next = SCHEMES
            .iter()
            .filter_map(|p| rest.find(p))
            .min();
        match next {
            None => {
                out.push_str(rest);
                return out;
            }
            Some(i) => {
                out.push_str(&rest[..i]);
                out.push_str("<redacted-url>");
                let tail = &rest[i..];
                let end = tail
                    .find(|c: char| c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '>' | ','))
                    .unwrap_or(tail.len());
                rest = &tail[end..];
            }
        }
    }
}

/// Concrete provider type (HTTP). `RootProvider` defaults to `RootProvider<Ethereum>`.
/// Read-only calls do not need gas/nonce fillers; use `ProviderBuilder::default()`.
pub type HttpProvider = alloy::providers::RootProvider;

/// Build an HTTP provider from a URL string. The URL is never logged.
pub fn build_http_provider(url: &str) -> Result<HttpProvider> {
    let url = url
        .parse()
        .context("ARBITRUM_RPC_HTTP is not a valid URL (details redacted)")?;
    Ok(ProviderBuilder::default().connect_http(url))
}

/// Build a WebSocket `RootProvider` (same concrete type, different underlying transport).
async fn build_ws_provider(url: &str) -> Result<alloy::providers::RootProvider> {
    let ws = WsConnect::new(url);
    ProviderBuilder::default()
        .connect_ws(ws)
        .await
        .context("Failed to connect WebSocket provider (URL redacted)")
}

/// Subscribe to new blocks over WebSocket, calling `on_block` for each block number.
///
/// On connection drop, reconnects with exponential backoff (1 s → 30 s).
pub async fn run_block_loop<F, Fut>(
    ws_url: &str,
    http: &HttpProvider,
    mut on_block: F,
) -> Result<()>
where
    F: FnMut(&HttpProvider, u64) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut backoff_secs: u64 = 1;

    loop {
        match build_ws_provider(ws_url).await {
            Err(e) => {
                error!(error = %redact_urls(&format!("{e:#}")), "WebSocket connect failed; retry in {backoff_secs}s");
                sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(30);
                continue;
            }
            Ok(ws_provider) => {
                info!("WebSocket connected; subscribing to blocks");
                backoff_secs = 1;

                match ws_provider.subscribe_blocks().await {
                    Err(e) => {
                        error!(error = %redact_urls(&format!("{e:#}")), "subscribe_blocks failed; reconnecting");
                        sleep(Duration::from_secs(backoff_secs)).await;
                        backoff_secs = (backoff_secs * 2).min(30);
                        continue;
                    }
                    Ok(sub) => {
                        let mut stream = sub.into_stream();
                        while let Some(header) = stream.next().await {
                            let block_num = header.number;
                            if let Err(e) = on_block(http, block_num).await {
                                warn!(block = block_num, error = %redact_urls(&format!("{e:#}")), "per-block handler error");
                            }
                        }
                        warn!("Block stream ended; reconnecting in {backoff_secs}s");
                        sleep(Duration::from_secs(backoff_secs)).await;
                        backoff_secs = (backoff_secs * 2).min(30);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::redact_urls;

    #[test]
    fn redacts_http_and_ws_urls() {
        let s = "error sending request for url (https://arb-mainnet.g.alchemy.com/v2/SECRETKEY): timeout; ws wss://x.io/ws/KEY2 down";
        let r = redact_urls(s);
        assert!(!r.contains("SECRETKEY"));
        assert!(!r.contains("KEY2"));
        assert_eq!(
            r,
            "error sending request for url (<redacted-url>): timeout; ws <redacted-url> down"
        );
    }

    #[test]
    fn leaves_plain_text_untouched() {
        assert_eq!(redact_urls("no urls here"), "no urls here");
    }
}
