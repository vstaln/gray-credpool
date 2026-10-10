//! gray-credpool — a multi-key credential pool for gray providers.
//!
//! Port of the hermes-agent credential pool (`agent/credential_pool.py`,
//! `fallback_cooldown.py`, `rate_limit_credits.py`, `credential_sources.py`):
//! a per-provider pool of API-key *references* (never raw keys on disk),
//! per-key cooldown on 401/402/403/429/5xx, and rotation strategies that
//! pick the next cold key. State lives in `$GRAY_HOME/credpool.json`.
//!
//! Two surfaces consume the pool:
//! - the CLI (`gray credpool …`) and `credpool_*` tools — inspect, add,
//!   mark and rotate entries;
//! - the `credpool` provider declaration — a per-request loopback relay
//!   that injects the selected key and fails over mid-turn on upstream
//!   errors (see `docs/INTEGRATION.md`).

pub mod cli;
pub mod keys;
pub mod manifest;
pub mod mock;
pub mod ops;
pub mod pool;
pub mod relay;
pub mod sidecar;
pub mod store;
pub mod upstream;

/// Plugin identity on the wire and in `~/.gray/plugins/`.
pub const PLUGIN_NAME: &str = "credpool";
/// Provider id this sidecar declares (the relay entry point).
pub const PROVIDER_ID: &str = "credpool";
/// Auth method id: the pool itself is the credential.
pub const AUTH_METHOD_ID: &str = "pool";
/// Sidecar protocol: 1.2 carries provider declarations.
pub const PROTOCOL: &str = "1.2";

/// `$GRAY_HOME` when set, else `~/.gray`.
pub fn gray_home() -> anyhow::Result<std::path::PathBuf> {
    if let Ok(home) = std::env::var("GRAY_HOME") {
        let home = home.trim();
        if !home.is_empty() {
            return Ok(std::path::PathBuf::from(home));
        }
    }
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"))?;
    Ok(std::path::PathBuf::from(home).join(".gray"))
}

/// The pool file: `$GRAY_HOME/credpool.json` (task-specified location).
pub fn pool_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(gray_home()?.join("credpool.json"))
}

/// Short random-ish hex id (no rand dep): time nanos ^ pid.
pub fn hex_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{:06x}", (t ^ ((std::process::id() as u64) << 32)) & 0xFF_FFFF)
}

/// Wall-clock epoch seconds.
pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
