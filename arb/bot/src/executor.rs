//! Transaction simulation and submission.
//!
//! Phase 1 — module declared.
//! Phase 5 will implement:
//!   - `simulate(provider, tx) -> Result<Bytes>` — dry-run via eth_call
//!   - `send(provider, wallet, tx) -> Result<TxHash>` — live send
//!
//! SECURITY: Private key is loaded here and only here, and only when
//! `Config::execution_enabled == true`. It is never logged or printed.
