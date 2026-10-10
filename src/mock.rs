//! `mock:` upstreams — a canned-response file for hermetic verification
//! (`gray credpool add demo env:K` + `config demo --base-url mock:m.json`,
//! then `request` walks rotation without a real API).
//!
//! ```json
//! {
//!   "status": 200,
//!   "headers": {"content-type": "application/json"},
//!   "body": {"ok": true},
//!   "models": [{"id": "m1", "name": "Mock One"}],
//!   "keys": {
//!     "sk-first":  {"status": 429, "body": {"error": {"message": "slow down"}},
//!                  "retry_after": 30}
//!   }
//! }
//! ```
//!
//! `keys` matches the *resolved* key material exactly, or `"*"` for every
//! key without its own rule; `default`/`status` is the fallback. `body`
//! may be a string (sent raw — SSE text works) or any JSON value
//! (serialized). `retry_after` becomes a `Retry-After` header AND the
//! pool's `reset_at`, so a short mock window un-cools the key on schedule.

use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MockRule {
    #[serde(default = "default_status")]
    pub status: u16,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<serde_json::Value>,
    /// Seconds → Retry-After header + pool `reset_at`.
    #[serde(default)]
    pub retry_after: Option<f64>,
}

fn default_status() -> u16 {
    200
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MockFile {
    #[serde(flatten)]
    pub default_rule: MockRuleFields,
    #[serde(default)]
    pub keys: BTreeMap<String, MockRule>,
    #[serde(default)]
    pub models: Vec<serde_json::Value>,
}

/// The default response fields sit at the top level of the mock file.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MockRuleFields {
    #[serde(default = "default_status")]
    pub status: u16,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<serde_json::Value>,
    #[serde(default)]
    pub retry_after: Option<f64>,
}

pub struct MockEndpoint {
    file: MockFile,
}

pub struct MockResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl MockEndpoint {
    /// `mock:PATH` (or bare PATH) → endpoint. Relative paths resolve
    /// against cwd; the pool stores the string verbatim.
    pub fn load(spec: &str) -> Result<Self, String> {
        let path = spec.strip_prefix("mock:").unwrap_or(spec);
        let path = if let Some(rest) = path.strip_prefix("~/") {
            std::env::var("HOME")
                .map(|h| Path::new(&h).join(rest))
                .unwrap_or_else(|_| Path::new(path).to_path_buf())
        } else {
            Path::new(path).to_path_buf()
        };
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read mock file {}: {e}", path.display()))?;
        let file: MockFile =
            serde_json::from_str(&raw).map_err(|e| format!("bad mock file {}: {e}", path.display()))?;
        Ok(Self { file })
    }

    pub fn models(&self) -> &[serde_json::Value] {
        &self.file.models
    }

    /// The canned response for one presented key.
    pub fn respond(&self, key: &str) -> MockResponse {
        let rule = self
            .file
            .keys
            .get(key)
            .cloned()
            .or_else(|| self.file.keys.get("*").cloned())
            .map(|r| MockRuleFields {
                status: r.status,
                headers: r.headers,
                body: r.body,
                retry_after: r.retry_after,
            })
            .unwrap_or_else(|| self.file.default_rule.clone());
        let mut headers = rule.headers;
        if let Some(secs) = rule.retry_after {
            headers.insert("retry-after".to_string(), format!("{secs}"));
        }
        let body = match rule.body {
            Some(serde_json::Value::String(s)) => s.into_bytes(),
            Some(v) => serde_json::to_vec(&v).unwrap_or_default(),
            None => Vec::new(),
        };
        MockResponse {
            status: rule.status,
            headers,
            body,
        }
    }
}

/// Is this base_url a mock spec?
pub fn is_mock(base_url: &str) -> bool {
    base_url.starts_with("mock:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn per_key_rules_and_default() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("m.json");
        let mut f = std::fs::File::create(&p).unwrap();
        write!(
            f,
            r#"{{"status": 200, "body": {{"ok": true}},
              "keys": {{"k1": {{"status": 429, "retry_after": 5}}}}}}"#
        )
        .unwrap();
        let mock = MockEndpoint::load(&format!("mock:{}", p.display())).unwrap();
        assert_eq!(mock.respond("k1").status, 429);
        assert_eq!(mock.respond("k1").headers["retry-after"], "5");
        let ok = mock.respond("other");
        assert_eq!(ok.status, 200);
        assert_eq!(String::from_utf8(ok.body).unwrap(), "{\"ok\":true}");
    }
}
