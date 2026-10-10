//! Credential references — the pool stores *pointers* to key material,
//! never the keys themselves (task contract: `env:VAR` / `file:` / `cmd:`).
//!
//!   env:NAME        — `std::env::var(NAME)`, trimmed
//!   file:PATH       — file contents, trimmed (`~` expands)
//!   cmd:PROG ARGS…  — `PROG ARGS…` stdout, first line, trimmed
//!   auth:NAME       — string entry `NAME` in `$GRAY_HOME/auth.json`
//!
//! Anything else is refused so a raw key can never land in the pool file
//! by accident.

use std::path::PathBuf;

/// A parsed key reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyRef {
    Env(String),
    File(String),
    Cmd(Vec<String>),
    /// `auth:NAME` — a `AuthEntry::Key` row in gray's own auth.json.
    Auth(String),
}

pub fn parse(raw: &str) -> anyhow::Result<KeyRef> {
    let raw = raw.trim();
    if let Some(var) = raw.strip_prefix("env:") {
        let var = var.trim();
        anyhow::ensure!(!var.is_empty(), "env: reference needs a variable name");
        return Ok(KeyRef::Env(var.to_string()));
    }
    if let Some(path) = raw.strip_prefix("file:") {
        let path = path.trim();
        anyhow::ensure!(!path.is_empty(), "file: reference needs a path");
        return Ok(KeyRef::File(path.to_string()));
    }
    if let Some(cmd) = raw.strip_prefix("cmd:") {
        let words: Vec<String> = cmd
            .split_whitespace()
            .map(str::to_string)
            .collect();
        anyhow::ensure!(!words.is_empty(), "cmd: reference needs a command");
        return Ok(KeyRef::Cmd(words));
    }
    if let Some(name) = raw.strip_prefix("auth:") {
        let name = name.trim();
        anyhow::ensure!(!name.is_empty(), "auth: reference needs a name");
        return Ok(KeyRef::Auth(name.to_string()));
    }
    anyhow::bail!(
        "unrecognized key reference {raw:?} — use env:VAR, file:PATH, cmd:PROG…, or auth:NAME (raw keys are never stored)"
    )
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

/// Resolve a reference to key material. Never logged, never persisted.
pub fn resolve(key_ref: &str) -> Result<String, String> {
    let parsed = parse(key_ref).map_err(|e| e.to_string())?;
    let value = match parsed {
        KeyRef::Env(var) => std::env::var(&var)
            .map_err(|_| format!("env var {var} is not set"))?,
        KeyRef::File(path) => {
            let path = expand_tilde(&path);
            std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?
        }
        KeyRef::Cmd(words) => {
            let (prog, args) = words.split_first().unwrap();
            let out = std::process::Command::new(prog)
                .args(args)
                .output()
                .map_err(|e| format!("cannot run {prog}: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "{prog} exited {}",
                    out.status.code().unwrap_or(-1)
                ));
            }
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .to_string()
        }
        KeyRef::Auth(name) => {
            let path = crate::gray_home()
                .map_err(|e| e.to_string())?
                .join("auth.json");
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let store: serde_json::Value =
                serde_json::from_str(&raw).map_err(|e| format!("bad auth.json: {e}"))?;
            match store.get(&name) {
                Some(serde_json::Value::String(key)) => key.clone(),
                _ => return Err(format!("no string credential '{name}' in auth.json")),
            }
        }
    };
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(format!("reference {key_ref} resolved empty"));
    }
    Ok(value)
}

/// Does the string look like a reference we can resolve?
pub fn is_ref(raw: &str) -> bool {
    parse(raw).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_refs() {
        assert_eq!(parse("env:X").unwrap(), KeyRef::Env("X".into()));
        assert_eq!(parse("file:~/k").unwrap(), KeyRef::File("~/k".into()));
        assert_eq!(
            parse("cmd:gh auth token").unwrap(),
            KeyRef::Cmd(vec!["gh".into(), "auth".into(), "token".into()])
        );
        assert_eq!(parse("auth:openrouter").unwrap(), KeyRef::Auth("openrouter".into()));
        assert!(parse("sk-literal-key").is_err());
    }

    #[test]
    fn resolves_env() {
        unsafe { std::env::set_var("CREDPOOL_TEST_KEY", "sk-test-1") };
        assert_eq!(resolve("env:CREDPOOL_TEST_KEY").unwrap(), "sk-test-1");
        unsafe { std::env::remove_var("CREDPOOL_TEST_KEY") };
        assert!(resolve("env:CREDPOOL_TEST_KEY").is_err());
    }

    #[test]
    fn resolves_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("k");
        std::fs::write(&p, "  sk-file-2\n").unwrap();
        assert_eq!(
            resolve(&format!("file:{}", p.display())).unwrap(),
            "sk-file-2"
        );
    }
}
