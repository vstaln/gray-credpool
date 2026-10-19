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

/// Random hex id: 128 bits of /dev/urandom (time ^ pid hash mix as fallback).
pub fn hex_id() -> String {
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let mut b = [0u8; 16];
            if f.read_exact(&mut b).is_ok() {
                return b.iter().map(|x| format!("{x:02x}")).collect();
            }
        }
    }
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mix = |salt: u64| {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(t ^ ((std::process::id() as u128) << 64) ^ salt as u128);
        h.finish()
    };
    format!("{:016x}{:016x}", mix(0), mix(1))
}

/// Wall-clock epoch seconds.
pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
