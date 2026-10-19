//! Pool operations shared by the CLI (`gray credpool …`) and the sidecar
//! `credpool_*` tools. Every op takes the pool file lock, mutates, saves.
//! All return human-readable text (the same text a tool result carries).

use std::path::Path;

use crate::pool::{self, PooledKey};
use crate::store::{self, PoolFile};

fn now() -> f64 {
    crate::now()
}

fn fmt_epoch(epoch: Option<f64>) -> String {
    match epoch {
        Some(t) => {
            let rem = t - now();
            if rem > 0.0 {
                format!("in {rem:.0}s")
            } else {
                "now".to_string()
            }
        }
        None => "—".to_string(),
    }
}

/// One-line entry state for `status`/`list`.
fn entry_line(p: &PoolFile, e: &PooledKey, now: f64) -> String {
    let sole = p
        .entries
        .iter()
        .filter(|x| x.status != pool::STATUS_DEAD)
        .count()
        <= 1;
    let state = match e.status.as_str() {
        s if s == pool::STATUS_DEAD => {
            format!("dead ({})", e.last_error_reason.as_deref().unwrap_or(""))
        }
        s if s == pool::STATUS_EXHAUSTED => match pool::exhausted_until(e, sole) {
            Some(until) if now < until => format!("hot until {}", fmt_epoch(Some(until))),
            _ => "cooling (expired)".to_string(),
        },
        _ => "cold".to_string(),
    };
    let err = match (e.last_error_code, &e.last_error_reason) {
        (Some(c), Some(r)) => format!(" last={c}/{r}"),
        (Some(c), None) => format!(" last={c}"),
        _ => String::new(),
    };
    let sel = if p.last_selected_id.as_deref() == Some(e.id.as_str()) {
        " →current"
    } else {
        ""
    };
    format!(
        "  {:>6}  {:<40} {:<22} req={}{}{}",
        e.id, e.label, state, e.request_count, err, sel
    )
}

fn status_of(name: &str, p: &PoolFile) -> String {
    let now = now();
    let cold = pool::cold_indices(p, now).len();
    let next = pool::next_available_at(p, now);
    let mut out = format!(
        "pool '{name}' — {} cold / {} keys · strategy {} · upstream {}{}\n",
        cold,
        p.entries.len(),
        p.strategy,
        if p.base_url.is_empty() { "(unset)".to_string() } else { p.base_url.clone() },
        match next {
            Some(t) => format!(" · next cold {}", fmt_epoch(Some(t))),
            None => String::new(),
        }
    );
    for e in &p.entries {
        out.push_str(&entry_line(p, e, now));
        out.push('\n');
    }
    if p.entries.is_empty() {
        out.push_str("  (empty — `gray credpool add <provider> <key-ref>`)\n");
    }
    out
}

pub fn status(path: &Path, pool: Option<&str>) -> Result<String, String> {
    let f = store::load(path).map_err(|e| e.to_string())?;
    let mut out = String::new();
    match pool {
        Some(name) => {
            let p = f.pools.get(name).ok_or_else(|| format!("unknown pool '{name}'"))?;
            out.push_str(&status_of(name, p));
        }
        None => {
            if f.pools.is_empty() {
                return Ok("no pools — `gray credpool add <provider> <key-ref>`".into());
            }
            for (name, p) in &f.pools {
                let marker = if f.default_pool.as_deref() == Some(name.as_str()) {
                    " (default)"
                } else {
                    ""
                };
                out.push_str(&status_of(&format!("{name}{marker}"), p));
            }
        }
    }
    Ok(out)
}

pub fn add(
    path: &Path,
    pool_name: &str,
    key_ref: &str,
    label: Option<&str>,
    base_url: Option<&str>,
) -> Result<String, String> {
    // Validate the ref parses before it lands in the file.
    crate::keys::parse(key_ref).map_err(|e| e.to_string())?;
    let (mut f, _lock) = store::load_locked(path).map_err(|e| e.to_string())?;
    let p = f.pools.entry(pool_name.to_string()).or_default();
    if let Some(u) = base_url {
        p.base_url = u.to_string();
    }
    let prio = p.entries.iter().map(|e| e.priority).max().map_or(0, |m| m + 1);
    let e = PooledKey::new(key_ref, label.map(str::to_string), prio);
    let id = e.id.clone();
    p.entries.push(e);
    store::save(path, &f).map_err(|e| e.to_string())?;
    Ok(format!("added {id} to pool '{pool_name}' ({key_ref})"))
}

pub fn remove(path: &Path, pool_name: &str, id_or_label: &str) -> Result<String, String> {
    let (mut f, _lock) = store::load_locked(path).map_err(|e| e.to_string())?;
    let p = f
        .pools
        .get_mut(pool_name)
        .ok_or_else(|| format!("unknown pool '{pool_name}'"))?;
    let before = p.entries.len();
    p.entries
        .retain(|e| e.id != id_or_label && e.label != id_or_label);
    if p.entries.len() == before {
        return Err(format!("no entry '{id_or_label}' in pool '{pool_name}'"));
    }
    store::save(path, &f).map_err(|e| e.to_string())?;
    Ok(format!("removed '{id_or_label}' from '{pool_name}'"))
}

pub struct MarkOpts {
    pub id: Option<String>,
    pub status: Option<u16>,
    pub reason: Option<String>,
    pub seconds: Option<f64>,
    pub dead: bool,
}

pub fn mark(path: &Path, pool_name: &str, opts: MarkOpts) -> Result<String, String> {
    let (mut f, _lock) = store::load_locked(path).map_err(|e| e.to_string())?;
    let p = f
        .pools
        .get_mut(pool_name)
        .ok_or_else(|| format!("unknown pool '{pool_name}'"))?;
    let Some(idx) = pool::find_failed(p, opts.id.as_deref(), None) else {
        return Err(format!("pool '{pool_name}': no entry to mark (empty selection)"));
    };
    let now = now();
    let reset_at = opts.seconds.map(|s| now + s);
    pool::mark(
        p,
        idx,
        opts.status,
        opts.reason.as_deref(),
        Some("marked via credpool".into()),
        reset_at,
        now,
        opts.dead,
    );
    let e = &p.entries[idx];
    let sole = p.entries.iter().filter(|x| x.status != pool::STATUS_DEAD).count() <= 1;
    let until = pool::exhausted_until(e, sole);
    let label = e.label.clone();
    store::save(path, &f).map_err(|e| e.to_string())?;
    let what = if opts.dead { "dead" } else { "exhausted" };
    Ok(format!(
        "marked {label} {what} — cold again {}",
        fmt_epoch(until)
    ))
}

pub fn rotate(path: &Path, pool_name: &str) -> Result<String, String> {
    let (mut f, _lock) = store::load_locked(path).map_err(|e| e.to_string())?;
    let p = f
        .pools
        .get_mut(pool_name)
        .ok_or_else(|| format!("unknown pool '{pool_name}'"))?;
    let Some(idx) = pool::rotate(p, now()) else {
        return Err(format!("pool '{pool_name}': no cold key to rotate to"));
    };
    let label = p.entries[idx].label.clone();
    store::save(path, &f).map_err(|e| e.to_string())?;
    Ok(format!("rotated '{pool_name}' → {label}"))
}

pub fn clear(path: &Path, pool_name: &str, id: Option<&str>) -> Result<String, String> {
    let (mut f, _lock) = store::load_locked(path).map_err(|e| e.to_string())?;
    let p = f
        .pools
        .get_mut(pool_name)
        .ok_or_else(|| format!("unknown pool '{pool_name}'"))?;
    let now = now();
    let mut cleared = 0;
    for e in &mut p.entries {
        if let Some(id) = id {
            if e.id != id && e.label != id {
                continue;
            }
        }
        if e.status != pool::STATUS_OK {
            e.status = pool::STATUS_OK.into();
            e.status_at = Some(now);
            e.reset_at = None;
            e.last_error_code = None;
            e.last_error_reason = None;
            cleared += 1;
        }
    }
    store::save(path, &f).map_err(|e| e.to_string())?;
    Ok(format!("cleared {cleared} entr{} in '{pool_name}'", if cleared == 1 { "y" } else { "ies" }))
}

pub fn use_pool(path: &Path, pool_name: &str) -> Result<String, String> {
    let (mut f, _lock) = store::load_locked(path).map_err(|e| e.to_string())?;
    if !f.pools.contains_key(pool_name) {
        return Err(format!("unknown pool '{pool_name}'"));
    }
    f.default_pool = Some(pool_name.to_string());
    store::save(path, &f).map_err(|e| e.to_string())?;
    Ok(format!("default pool → '{pool_name}'"))
}

/// The resolution shim's read side: resolve the entry `select` would pick.
/// `reveal` prints the material; without it, proof it resolves.
pub fn resolve(path: &Path, pool_name: &str, reveal: bool) -> Result<String, String> {
    let (mut f, _lock) = store::load_locked(path).map_err(|e| e.to_string())?;
    let p = f
        .pools
        .get_mut(pool_name)
        .ok_or_else(|| format!("unknown pool '{pool_name}'"))?;
    let Some(idx) = pool::select(p, now()) else {
        let next = pool::next_available_at(p, now());
        return Err(format!(
            "pool '{pool_name}': no cold key (next {})",
            fmt_epoch(next)
        ));
    };
    let e = p.entries[idx].clone();
    store::save(path, &f).map_err(|e| e.to_string())?;
    match crate::keys::resolve(&e.key_ref) {
        Ok(key) if reveal => Ok(key),
        Ok(_) => Ok(format!("{} → resolves ({})", e.label, e.key_ref)),
        Err(err) => Err(format!("{} fails to resolve: {err}", e.label)),
    }
}

/// The write side of the shim for `credential_source != plugin` setups:
/// file the currently-selected key where gray's `api_key` path reads it —
/// `auth.json[pool]` and, with `activate`, `config.json.api_key`.
/// Lossy by design: an in-flight agent keeps its build-time key until the
/// next process start / provider switch (see docs/INTEGRATION.md).
pub fn apply(path: &Path, pool_name: &str, activate: bool) -> Result<String, String> {
    let key = resolve(path, pool_name, true)?;
    let home = crate::gray_home().map_err(|e| e.to_string())?;
    let auth_path = home.join("auth.json");
    let mut auth: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&auth_path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .map(|v: serde_json::Value| v.as_object().cloned().unwrap_or_default())
        .unwrap_or_default();
    auth.insert(pool_name.to_string(), serde_json::Value::String(key.clone()));
    let tmp = auth_path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&auth).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &auth_path).map_err(|e| e.to_string())?;
    let mut out = format!("wrote selected key to auth.json['{pool_name}']");
    if activate {
        let cfg_path = home.join("config.json");
        let mut cfg: serde_json::Value = std::fs::read_to_string(&cfg_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        cfg["api_key"] = serde_json::Value::String(key);
        cfg["credential_source"] = serde_json::Value::String(String::new());
        cfg["auth_mode"] = serde_json::Value::String("api_key".into());
        let tmp = cfg_path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &cfg_path).map_err(|e| e.to_string())?;
        out.push_str("; config.json api_key ← selected key (active on next agent start)");
    }
    Ok(out)
}

/// `env` shim for wrappers: shell line exporting the selected key.
pub fn env_line(path: &Path, pool_name: &str, var: &str) -> Result<String, String> {
    let key = resolve(path, pool_name, true)?;
    Ok(format!("export {var}='{}'", key.replace('\'', "'\\''")))
}

pub fn config_set(
    path: &Path,
    pool_name: &str,
    base_url: Option<&str>,
    strategy: Option<&str>,
    watch_tool_errors: Option<bool>,
    models: Option<Vec<String>>,
) -> Result<String, String> {
    let (mut f, _lock) = store::load_locked(path).map_err(|e| e.to_string())?;
    let p = f
        .pools
        .get_mut(pool_name)
        .ok_or_else(|| format!("unknown pool '{pool_name}'"))?;
    let mut changed = Vec::new();
    if let Some(u) = base_url {
        p.base_url = u.to_string();
        changed.push(format!("base_url={u}"));
    }
    if let Some(s) = strategy {
        if !pool::STRATEGIES.contains(&s) {
            return Err(format!("unknown strategy '{s}' ({})", pool::STRATEGIES.join("|")));
        }
        p.strategy = s.to_string();
        changed.push(format!("strategy={s}"));
    }
    if let Some(w) = watch_tool_errors {
        p.watch_tool_errors = w;
        changed.push(format!("watch_tool_errors={w}"));
    }
    if let Some(m) = models {
        p.models = m;
        changed.push("models updated".to_string());
    }
    if changed.is_empty() {
        return Err("nothing to set".into());
    }
    store::save(path, &f).map_err(|e| e.to_string())?;
    Ok(format!("{pool_name}: {}", changed.join(", ")))
}

/// One failover walk without a host: used by `gray credpool request` and
/// the verification flow. `send` decides the upstream per attempt.
pub fn request(
    path: &Path,
    pool_name: &str,
    method: &str,
    upstream_path: &str,
    body: Option<&[u8]>,
) -> Result<String, String> {
    let base_url = {
        let f = store::load(path).map_err(|e| e.to_string())?;
        f.pools
            .get(pool_name)
            .map(|p| p.base_url.clone())
            .ok_or_else(|| format!("unknown pool '{pool_name}'"))?
    };
    if base_url.is_empty() {
        return Err(format!(
            "pool '{pool_name}' has no base_url — `gray credpool config {pool_name} --base-url <url>`"
        ));
    }
    let mut out = String::new();
    if crate::mock::is_mock(&base_url) {
        let endpoint = crate::mock::MockEndpoint::load(&base_url).map_err(|e| e)?;
        let fwd = crate::upstream::forward(path, pool_name, |_entry, key| {
            let r = endpoint.respond(key);
            Ok(crate::upstream::UpstreamHead {
                status: r.status,
                retry_after: r
                    .headers
                    .get("retry-after")
                    .and_then(|v| v.parse::<f64>().ok()),
                headers: r.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                reader: Box::new(std::io::Cursor::new(r.body)),
            })
        });
        return Ok(render_forward(fwd));
    }
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(20))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?;
    let fwd = crate::upstream::forward(path, pool_name, |_entry, key| {
        crate::upstream::http_send(&client, &base_url, method, upstream_path, body, key)
    });
    out.push_str(&render_forward(fwd));
    Ok(out)
}

fn render_forward(
    fwd: Result<crate::upstream::Forward, crate::upstream::PoolDrained>,
) -> String {
    let mut out = String::new();
    match fwd {
        Ok(f) => {
            for a in &f.attempts {
                out.push_str(&format!("  [{}] {} → {}\n", a.key_id, a.key_label, a.outcome));
            }
            out.push_str(&format!("final: HTTP {}\n", f.head.status));
            if !f.buffered.is_empty() {
                let text = String::from_utf8_lossy(&f.buffered);
                out.push_str(text.chars().take(2000).collect::<String>().as_str());
                out.push('\n');
            } else {
                use std::io::Read;
                let mut buf = Vec::new();
                let _ = f
                    .head
                    .reader
                    .take(64 * 1024)
                    .read_to_end(&mut buf);
                out.push_str(
                    &String::from_utf8_lossy(&buf).chars().take(2000).collect::<String>(),
                );
                out.push('\n');
            }
            if f.rotated {
                out.push_str("rotation: keys were benched during this request\n");
            }
        }
        Err(d) => {
            for a in &d.attempts {
                out.push_str(&format!("  [{}] {} → {}\n", a.key_id, a.key_label, a.outcome));
            }
            out.push_str(&format!(
                "pool drained: {} (next cold {})\n",
                d.message,
                fmt_epoch(d.next_available_at)
            ));
        }
    }
    out
}

/// `provider/models`: static list, mock file, or live `GET {base}/models`.
pub fn models(path: &Path, pool_name: &str) -> Result<Vec<serde_json::Value>, String> {
    let f = store::load(path).map_err(|e| e.to_string())?;
    let p = f
        .pools
        .get(pool_name)
        .ok_or_else(|| format!("unknown pool '{pool_name}'"))?;
    if !p.models.is_empty() {
        return Ok(p
            .models
            .iter()
            .map(|id| serde_json::json!({"id": id, "name": id}))
            .collect());
    }
    if p.base_url.is_empty() {
        return Err(format!("pool '{pool_name}' has no base_url and no static models"));
    }
    if crate::mock::is_mock(&p.base_url) {
        let m = crate::mock::MockEndpoint::load(&p.base_url)?;
        return Ok(m.models().to_vec());
    }
    // Live: GET {base}/models through one failover walk.
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let fwd = crate::upstream::forward(path, pool_name, |_e, key| {
        crate::upstream::http_send(&client, &p.base_url, "GET", "/models", None, key)
    })
    .map_err(|d| d.message)?;
    let mut body = fwd.buffered.clone();
    if body.is_empty() {
        use std::io::Read;
        let _ = fwd.head.reader.take(2 << 20).read_to_end(&mut body);
    }
    let v: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| format!("bad /models payload: {e}"))?;
    let rows = v
        .get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(rows
        .iter()
        .filter_map(|r| r.get("id").and_then(|i| i.as_str()))
        .map(|id| serde_json::json!({"id": id, "name": id}))
        .collect())
}
