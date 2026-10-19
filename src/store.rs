//! `~/.gray/credpool.json` — the durable pool file.
//!
//! Layout:
//! ```json
//! {"schema":1,"default_pool":"openrouter","pools":{"openrouter":{
//!   "base_url":"https://openrouter.ai/api/v1","strategy":"fill_first",
//!   "watch_tool_errors":false,"models":[],"entries":[{PooledKey}]}}}
//! ```
//!
//! Writes are atomic (tmp + rename) under a `credpool.lock` advisory file
//! lock so a CLI mark and an in-flight relay rotation can't clobber each other.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::pool::{PooledKey, STRATEGY_FILL_FIRST};

pub const SCHEMA: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PoolFile {
    /// Upstream base URL this pool's keys authenticate against
    /// (`https://…/v1`) or `mock:<path>` for a canned-response file.
    #[serde(default)]
    pub base_url: String,
    /// Selection strategy: fill_first | round_robin | random | least_used.
    #[serde(default = "default_strategy")]
    pub strategy: String,
    /// When true, a `tool/after` rate-limit-looking error marks the
    /// last-selected key of this pool hot (opt-in surface).
    #[serde(default)]
    pub watch_tool_errors: bool,
    /// Optional static model list served by `provider/models` when the
    /// upstream does not answer GET /models.
    #[serde(default)]
    pub models: Vec<String>,
    /// Last entry `select()` handed out — the identity `mark`/`rotate`
    /// fall back to when the caller names no key.
    #[serde(default)]
    pub last_selected_id: Option<String>,
    #[serde(default)]
    pub entries: Vec<PooledKey>,
}

fn default_strategy() -> String {
    STRATEGY_FILL_FIRST.to_string()
}

impl Default for PoolFile {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            strategy: default_strategy(),
            watch_tool_errors: false,
            models: Vec::new(),
            last_selected_id: None,
            entries: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CredpoolFile {
    #[serde(default)]
    pub schema: u32,
    /// The pool the `credpool` provider declaration serves and bare CLI
    /// verbs default to.
    #[serde(default)]
    pub default_pool: Option<String>,
    #[serde(default)]
    pub pools: BTreeMap<String, PoolFile>,
}

/// Advisory lock while a `credpool.json` read-modify-write runs.
pub struct PoolLock {
    _file: File,
}

/// Take the inter-process lock then load. Pair with [`save_locked`].
pub fn load_locked(path: &Path) -> anyhow::Result<(CredpoolFile, PoolLock)> {
    let lock = lock_for(path)?;
    Ok((load(path)?, lock))
}

fn lock_for(path: &Path) -> anyhow::Result<PoolLock> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(lock_path(path))?;
    // Blocking lock: pool mutations are sub-millisecond, waiting is fine.
    file.lock()?;
    Ok(PoolLock { _file: file })
}

fn lock_path(path: &Path) -> PathBuf {
    path.with_extension("lock")
}

pub fn load(path: &Path) -> anyhow::Result<CredpoolFile> {
    let mut raw = String::new();
    match File::open(path) {
        Ok(mut f) => {
            f.read_to_string(&mut raw)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CredpoolFile {
                schema: SCHEMA,
                ..Default::default()
            });
        }
        Err(e) => return Err(e.into()),
    }
    let file: CredpoolFile = serde_json::from_str(&raw)?;
    Ok(file)
}

/// Atomic write; callers should hold [`PoolLock`].
pub fn save(path: &Path, file: &CredpoolFile) -> anyhow::Result<()> {
    let mut file = file.clone();
    file.schema = SCHEMA;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(serde_json::to_string_pretty(&file)?.as_bytes())?;
        f.sync_all().ok();
    }
    // Owner-only: refs are not secrets but the file steers authentication.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

impl CredpoolFile {
    /// The pool a provider request serves: `default_pool` when set, else
    /// the only pool when unambiguous, else an error naming the choice.
    pub fn active_pool_name(&self) -> anyhow::Result<String> {
        if let Some(name) = &self.default_pool {
            anyhow::ensure!(
                self.pools.contains_key(name),
                "default pool '{name}' has no entries — `gray credpool add {name} <key-ref>`"
            );
            return Ok(name.clone());
        }
        match self.pools.len() {
            0 => anyhow::bail!(
                "no credential pools — `gray credpool add <provider> <key-ref>` first"
            ),
            1 => Ok(self.pools.keys().next().unwrap().clone()),
            _ => anyhow::bail!(
                "several pools ({}) — pick one with `gray credpool use <pool>`",
                self.pools.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credpool.json");
        let mut f = CredpoolFile::default();
        f.pools.insert(
            "test".into(),
            PoolFile {
                base_url: "mock:./m.json".into(),
                ..Default::default()
            },
        );
        f.default_pool = Some("test".into());
        save(&path, &f).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back.schema, SCHEMA);
        assert_eq!(back.active_pool_name().unwrap(), "test");
        assert_eq!(back.pools["test"].base_url, "mock:./m.json");
    }
}
