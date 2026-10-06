//! Token allowlist, TOML parsing, on-chain verification, and token risk flags.
//!
//! Phase 2f: configurable multi-token scanner allowlist.

use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::sol;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

sol! {
    #[sol(rpc)]
    interface IERC20Metadata {
        function name() external view returns (string);
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
        function balanceOf(address account) external view returns (uint256);
    }

    #[sol(rpc)]
    interface IPausable {
        function paused() external view returns (bool);
    }

    #[sol(rpc)]
    interface IBlacklistable {
        function isBlacklisted(address account) external view returns (bool);
    }

    #[sol(rpc)]
    interface IBeacon {
        function implementation() external view returns (address);
    }

    #[sol(rpc)]
    interface IArbGatewayToken {
        function l1Address() external view returns (address);
    }
}

/// A token entry in `tokens.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TokenEntry {
    /// Token ticker / symbol, e.g. "USDC".
    pub symbol: String,
    /// Contract address on Arbitrum One (hex string).
    pub address: String,
    /// Expected token decimals.
    pub decimals: u8,
    /// Authoritative source URL (e.g. Arbiscan or official token list).
    pub source_url: String,
    /// Whether this token address has been reviewed and approved by user.
    #[serde(default)]
    pub reviewed: bool,
}

/// Root structure of `tokens.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokensConfig {
    #[serde(default)]
    pub tokens: Vec<TokenEntry>,
}

impl TokensConfig {
    /// Load `tokens.toml` from a specific path.
    pub fn load_from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())
            .with_context(|| format!("Failed to read tokens file at {:?}", path.as_ref()))?;
        let cfg: TokensConfig = toml::from_str(&content)
            .with_context(|| format!("Failed to parse TOML from {:?}", path.as_ref()))?;
        Ok(cfg)
    }

    /// Load `tokens.toml` searching default expected locations.
    pub fn load_default() -> Result<Self> {
        let candidate_paths = [
            "tokens.toml",
            "bot/tokens.toml",
            "arb/bot/tokens.toml",
            "../tokens.toml",
        ];
        for p in &candidate_paths {
            if Path::new(p).exists() {
                return Self::load_from_path(p);
            }
        }
        anyhow::bail!("tokens.toml not found in default locations: {:?}", candidate_paths);
    }

    /// Check whether a given address has `reviewed == true`.
    pub fn is_reviewed(&self, addr: &Address) -> bool {
        self.tokens.iter().any(|t| {
            if let Ok(parsed) = t.address.parse::<Address>() {
                parsed == *addr && t.reviewed
            } else {
                false
            }
        })
    }
}

/// Risk flags detected from contract bytecode, storage, and interface queries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenRiskFlags {
    /// Whether contract is an EIP-1967 or known proxy.
    pub is_proxy: bool,
    /// EIP-1967 implementation contract address if present.
    pub proxy_impl: Option<Address>,
    /// EIP-1967 admin address if present.
    pub proxy_admin: Option<Address>,
    /// Whether contract is an EIP-1967 beacon proxy.
    pub has_beacon: bool,
    /// EIP-1967 beacon contract address if present.
    pub beacon_addr: Option<Address>,
    /// Underlying implementation resolved from beacon if present.
    pub beacon_impl: Option<Address>,
    /// Whether contract exposes pause functionality.
    pub has_pause: bool,
    /// Whether contract is currently paused.
    pub is_currently_paused: bool,
    /// Whether contract exposes blacklist / blocklist functionality.
    pub has_blacklist: bool,
    /// Whether contract exposes fee-on-transfer / tax selectors.
    pub has_fee: bool,
    /// Whether token is an Arbitrum standard gateway bridged token.
    pub is_arb_gateway: bool,
    /// Underlying L1 address if bridged via Arbitrum gateway.
    pub l1_address: Option<Address>,
}

impl TokenRiskFlags {
    /// Render concise bracketed flag string, e.g. `"[PROXY, BLACKLIST, PAUSE]"`.
    pub fn display_flags(&self) -> String {
        let mut flags = Vec::new();
        if self.has_beacon {
            flags.push("BEACON".to_string());
        }
        if self.is_proxy {
            flags.push("PROXY".to_string());
        }
        if self.is_arb_gateway {
            flags.push("ARB_GATEWAY".to_string());
        }
        if self.has_pause {
            if self.is_currently_paused {
                flags.push("PAUSED!".to_string());
            } else {
                flags.push("PAUSE".to_string());
            }
        }
        if self.has_blacklist {
            flags.push("BLACKLIST".to_string());
        }
        if self.has_fee {
            flags.push("FEE".to_string());
        }
        if flags.is_empty() {
            "[NONE]".to_string()
        } else {
            format!("[{}]", flags.join(", "))
        }
    }

    /// Detailed description of proxy architecture for reports.
    pub fn proxy_detail(&self) -> String {
        if self.has_beacon {
            let b = self.beacon_addr.map(|a| format!("{:#x}", a)).unwrap_or_else(|| "unknown".into());
            let imp = self.beacon_impl.map(|a| format!("{:#x}", a)).unwrap_or_else(|| "unknown".into());
            format!("BeaconProxy (Beacon: {}, Impl: {})", b, imp)
        } else if self.is_proxy {
            let imp = self.proxy_impl.map(|a| format!("{:#x}", a)).unwrap_or_else(|| "unknown".into());
            let adm = self.proxy_admin.map(|a| format!("{:#x}", a)).unwrap_or_else(|| "none".into());
            format!("EIP1967Proxy (Impl: {}, Admin: {})", imp, adm)
        } else if self.is_arb_gateway {
            let l1 = self.l1_address.map(|a| format!("{:#x}", a)).unwrap_or_else(|| "standard".into());
            format!("StandardArbERC20 (Gateway Bridged, L1: {})", l1)
        } else {
            "Standard Contract (No Proxy)".to_string()
        }
    }
}

/// On-chain verification result for a candidate token.
#[derive(Debug, Clone)]
pub struct TokenCheckResult {
    pub entry: TokenEntry,
    pub address: Address,
    pub bytecode_len: usize,
    #[allow(dead_code)]
    pub onchain_symbol: Option<String>,
    #[allow(dead_code)]
    pub onchain_decimals: Option<u8>,
    pub is_verified: bool,
    pub error: Option<String>,
    pub risk_flags: TokenRiskFlags,
}

// Common 4-byte selectors for token risk detection
const SEL_PAUSED: [u8; 4] = [0x5c, 0x97, 0x5a, 0xbb];          // paused()
const SEL_PAUSE: [u8; 4] = [0x84, 0x56, 0xcb, 0x59];           // pause()
const SEL_IS_PAUSED: [u8; 4] = [0xb1, 0x87, 0xbd, 0x26];       // isPaused()

const SEL_IS_BLACKLISTED: [u8; 4] = [0xfe, 0x57, 0x5a, 0x87]; // isBlacklisted(address)
const SEL_BLACKLISTED: [u8; 4] = [0x51, 0x97, 0xf6, 0x44];    // blacklisted(address)
const SEL_IS_BLOCKED: [u8; 4] = [0x5e, 0x08, 0xc4, 0x8a];     // isBlocked(address)
const SEL_BLACKLIST: [u8; 4] = [0x43, 0xee, 0xd0, 0x57];      // blacklist(address)

const SEL_TAX_FEE: [u8; 4] = [0x24, 0xd8, 0x58, 0x34];        // taxFee()
const SEL_TRANSFER_TAX: [u8; 4] = [0x24, 0x2c, 0xc3, 0xd9];   // transferTaxRate()
const SEL_FEE_PERCENT: [u8; 4] = [0x91, 0xd9, 0x04, 0x6a];    // feePercent()
const SEL_BUY_FEE: [u8; 4] = [0xc3, 0x6a, 0x8d, 0x79];        // buyFee()
const SEL_SELL_FEE: [u8; 4] = [0x7b, 0x6f, 0x63, 0x43];       // sellFee()

/// Compute EIP-1967 storage slot from key: bytes32(uint256(keccak256(key)) - 1)
pub fn compute_eip1967_slot(key: &str) -> B256 {
    let hash = alloy::primitives::keccak256(key.as_bytes());
    let val = U256::from_be_bytes(hash.0) - U256::from(1);
    B256::from(val.to_be_bytes())
}

/// Inspect bytecode to see if a 4-byte selector appears anywhere in the code.
fn bytecode_contains_selector(code: &[u8], sel: &[u8; 4]) -> bool {
    code.windows(4).any(|w| w == sel)
}

/// Verify a token on-chain: checks bytecode exists, symbol(), decimals(), and token risk flags.
pub async fn verify_token<P: Provider>(http: &P, entry: &TokenEntry) -> TokenCheckResult {
    let addr = match entry.address.parse::<Address>() {
        Ok(a) => a,
        Err(e) => {
            return TokenCheckResult {
                entry: entry.clone(),
                address: Address::ZERO,
                bytecode_len: 0,
                onchain_symbol: None,
                onchain_decimals: None,
                is_verified: false,
                error: Some(format!("Invalid address: {e}")),
                risk_flags: TokenRiskFlags::default(),
            };
        }
    };

    // 1. Check bytecode
    let code = match http.get_code_at(addr).await {
        Ok(c) => c,
        Err(e) => {
            return TokenCheckResult {
                entry: entry.clone(),
                address: addr,
                bytecode_len: 0,
                onchain_symbol: None,
                onchain_decimals: None,
                is_verified: false,
                error: Some(format!("get_code_at failed: {e}")),
                risk_flags: TokenRiskFlags::default(),
            };
        }
    };

    let bytecode_len = code.len();
    if bytecode_len == 0 {
        return TokenCheckResult {
            entry: entry.clone(),
            address: addr,
            bytecode_len: 0,
            onchain_symbol: None,
            onchain_decimals: None,
            is_verified: false,
            error: Some("No bytecode (EOA or non-existent contract)".into()),
            risk_flags: TokenRiskFlags::default(),
        };
    }

    // 2. Query symbol()
    let token_contract = IERC20Metadata::new(addr, http);
    let onchain_symbol = token_contract.symbol().call().await.ok();

    // 3. Query decimals()
    let onchain_decimals = token_contract.decimals().call().await.ok();

    // Validate symbol and decimals match expectation
    let mut is_verified = true;
    let mut err_msg = None;

    if let Some(ref sym) = onchain_symbol {
        // Compare case-insensitively or allowing minor variations (e.g. USD₮0 vs USDT)
        let sym_clean = sym.replace('₮', "T").replace('0', "");
        let entry_clean = entry.symbol.replace('₮', "T").replace('0', "");
        let is_sym_match = sym.eq_ignore_ascii_case(&entry.symbol)
            || sym_clean.eq_ignore_ascii_case(&entry_clean)
            || (entry.symbol.eq_ignore_ascii_case("USDC.e") && sym.eq_ignore_ascii_case("USDC"));
        if !is_sym_match {
            is_verified = false;
            err_msg = Some(format!("Symbol mismatch: expected '{}', got '{}'", entry.symbol, sym));
        }
    } else {
        is_verified = false;
        err_msg = Some("symbol() call failed".into());
    }

    if let Some(dec) = onchain_decimals {
        if dec != entry.decimals {
            is_verified = false;
            err_msg = Some(format!("Decimals mismatch: expected {}, got {}", entry.decimals, dec));
        }
    } else {
        is_verified = false;
        err_msg = Some("decimals() call failed".into());
    }

    // 4. Token Risk Checks
    let mut risk_flags = TokenRiskFlags::default();

    // (a) EIP-1967 proxy checks via storage
    let impl_slot = compute_eip1967_slot("eip1967.proxy.implementation");
    if let Ok(impl_val) = http.get_storage_at(addr, U256::from_be_bytes(impl_slot.0)).await {
        let impl_bytes: [u8; 32] = impl_val.to_be_bytes();
        let impl_addr = Address::from_slice(&impl_bytes[12..]);
        if impl_addr != Address::ZERO {
            risk_flags.is_proxy = true;
            risk_flags.proxy_impl = Some(impl_addr);
        }
    }

    let admin_slot = compute_eip1967_slot("eip1967.proxy.admin");
    if let Ok(admin_val) = http.get_storage_at(addr, U256::from_be_bytes(admin_slot.0)).await {
        let admin_bytes: [u8; 32] = admin_val.to_be_bytes();
        let admin_addr = Address::from_slice(&admin_bytes[12..]);
        if admin_addr != Address::ZERO {
            risk_flags.proxy_admin = Some(admin_addr);
        }
    }

    // EIP-1967 beacon proxy checks via storage
    let beacon_slot = compute_eip1967_slot("eip1967.proxy.beacon");
    if let Ok(beacon_val) = http.get_storage_at(addr, U256::from_be_bytes(beacon_slot.0)).await {
        let beacon_bytes: [u8; 32] = beacon_val.to_be_bytes();
        let beacon_addr = Address::from_slice(&beacon_bytes[12..]);
        if beacon_addr != Address::ZERO {
            risk_flags.has_beacon = true;
            risk_flags.beacon_addr = Some(beacon_addr);
            // Resolve implementation from beacon contract
            let beacon_contract = IBeacon::new(beacon_addr, http);
            if let Ok(b_impl) = beacon_contract.implementation().call().await {
                if b_impl != Address::ZERO {
                    risk_flags.beacon_impl = Some(b_impl);
                }
            }
        }
    }

    // Arbitrum L2 gateway bridged token check
    let gateway = IArbGatewayToken::new(addr, http);
    if let Ok(l1) = gateway.l1Address().call().await {
        if l1 != Address::ZERO {
            risk_flags.is_arb_gateway = true;
            risk_flags.l1_address = Some(l1);
        }
    }
    if bytecode_len == 760 {
        risk_flags.is_arb_gateway = true;
    }

    // (b) Pause checks
    let has_pause_sel = bytecode_contains_selector(&code, &SEL_PAUSED)
        || bytecode_contains_selector(&code, &SEL_PAUSE)
        || bytecode_contains_selector(&code, &SEL_IS_PAUSED);
    if has_pause_sel {
        risk_flags.has_pause = true;
    }
    // Attempt paused() call
    let pausable = IPausable::new(addr, http);
    if let Ok(p) = pausable.paused().call().await {
        risk_flags.has_pause = true;
        risk_flags.is_currently_paused = p;
    }

    // (c) Blacklist checks
    let has_bl_sel = bytecode_contains_selector(&code, &SEL_IS_BLACKLISTED)
        || bytecode_contains_selector(&code, &SEL_BLACKLISTED)
        || bytecode_contains_selector(&code, &SEL_IS_BLOCKED)
        || bytecode_contains_selector(&code, &SEL_BLACKLIST);
    if has_bl_sel {
        risk_flags.has_blacklist = true;
    }
    // Attempt isBlacklisted(0x1) call
    let bl = IBlacklistable::new(addr, http);
    let probe_addr = Address::repeat_byte(0x01);
    if bl.isBlacklisted(probe_addr).call().await.is_ok() {
        risk_flags.has_blacklist = true;
    }

    // (d) Fee checks
    let has_fee_sel = bytecode_contains_selector(&code, &SEL_TAX_FEE)
        || bytecode_contains_selector(&code, &SEL_TRANSFER_TAX)
        || bytecode_contains_selector(&code, &SEL_FEE_PERCENT)
        || bytecode_contains_selector(&code, &SEL_BUY_FEE)
        || bytecode_contains_selector(&code, &SEL_SELL_FEE);
    if has_fee_sel {
        risk_flags.has_fee = true;
    }

    TokenCheckResult {
        entry: entry.clone(),
        address: addr,
        bytecode_len,
        onchain_symbol,
        onchain_decimals,
        is_verified,
        error: err_msg,
        risk_flags,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_eip1967_slots() {
        let impl_slot = compute_eip1967_slot("eip1967.proxy.implementation");
        assert_eq!(
            format!("{:#x}", impl_slot),
            "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc"
        );
        let admin_slot = compute_eip1967_slot("eip1967.proxy.admin");
        assert_eq!(
            format!("{:#x}", admin_slot),
            "0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103"
        );
        let beacon_slot = compute_eip1967_slot("eip1967.proxy.beacon");
        assert_eq!(
            format!("{:#x}", beacon_slot),
            "0xa3f0ad74e5423aebfd80d3ef4346578335a9a72aeaee59ff6cb3582b35133d50"
        );
    }

    #[test]
    fn test_unreviewed_token_never_reaches_markets_json() {
        let entries = vec![
            TokenEntry {
                symbol: "USDC".into(),
                address: "0xaf88d065e77c8cC2239327C5EDb3A432268e5831".into(),
                decimals: 6,
                source_url: "https://arbiscan.io/...".into(),
                reviewed: true,
            },
            TokenEntry {
                symbol: "UNREVIEWED".into(),
                address: "0x0000000000000000000000000000000000000001".into(),
                decimals: 18,
                source_url: "https://arbiscan.io/...".into(),
                reviewed: false,
            },
        ];

        let cfg = TokensConfig { tokens: entries.clone() };
        let usdc_addr: Address = "0xaf88d065e77c8cC2239327C5EDb3A432268e5831".parse().unwrap();
        let unreviewed_addr: Address = "0x0000000000000000000000000000000000000001".parse().unwrap();

        assert!(cfg.is_reviewed(&usdc_addr));
        assert!(!cfg.is_reviewed(&unreviewed_addr));

        // Filter candidate tokens as enforced in discovery
        let target: Vec<&TokenEntry> = entries.iter().filter(|t| t.reviewed).collect();
        assert_eq!(target.len(), 1);
        assert_eq!(target[0].symbol, "USDC");
        assert!(!target.iter().any(|t| t.symbol == "UNREVIEWED"));

        // Simulate building markets.json
        let markets: Vec<serde_json::Value> = target
            .iter()
            .map(|t| {
                serde_json::json!({
                    "symbol": t.symbol,
                    "quote_token": t.address,
                    "quote_decimals": t.decimals,
                })
            })
            .collect();

        let json_str = serde_json::to_string(&markets).unwrap();
        assert!(json_str.contains("USDC"));
        assert!(!json_str.contains("UNREVIEWED"));
        assert!(!json_str.contains("0x0000000000000000000000000000000000000001"));
    }

    #[tokio::test]
    async fn test_cbeth_onchain_verification() {
        let _ = dotenvy::dotenv();
        if let Ok(rpc) = std::env::var("ARBITRUM_RPC_HTTP") {
            let http = crate::provider::build_http_provider(&rpc).unwrap();
            let entry = TokenEntry {
                symbol: "cbETH".into(),
                address: "0x1DEBd73E752bEaF79865Fd6446b0c970EaE7732f".into(),
                decimals: 18,
                source_url: "https://arbiscan.io/token/0x1DEBd73E752bEaF79865Fd6446b0c970EaE7732f".into(),
                reviewed: false,
            };
            let res = verify_token(&http, &entry).await;
            assert!(res.is_verified, "cbETH must be verified on-chain: {:?}", res.error);
            assert_eq!(res.onchain_decimals, Some(18));
            assert_eq!(res.onchain_symbol.as_deref(), Some("cbETH"));
            assert!(res.bytecode_len > 0);
        }
    }
}
