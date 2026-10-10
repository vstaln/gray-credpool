//! One upstream request through the pool: select a cold key → send → on a
//! rotatable failure mark the key hot and send again with the next one.
//! Shared by the per-turn relay (streams `head.reader` to the host) and
//! the `gray credpool request` CLI (buffers it for inspection).
//!
//! Locking: `credpool.json` is flock-ed per step (select / mark), not held
//! across network I/O — concurrent relays and CLIs interleave instead of
//! queuing behind one turn's upstream call.

use std::io::Read;
use std::path::Path;

use crate::pool;
use crate::store;

/// A response head from one send attempt; `reader` yields the body.
pub struct UpstreamHead {
    pub status: u16,
    /// End-to-end headers only (hop-by-hop stripped).
    pub headers: Vec<(String, String)>,
    /// `Retry-After` parsed to a delay in seconds, when present.
    pub retry_after: Option<f64>,
    pub reader: Box<dyn Read + Send>,
}

impl std::fmt::Debug for UpstreamHead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamHead")
            .field("status", &self.status)
            .field("retry_after", &self.retry_after)
            .finish_non_exhaustive()
    }
}

/// One line of rotation history — `request` prints it, the relay logs it.
#[derive(Debug, Clone)]
pub struct Attempt {
    pub key_id: String,
    pub key_label: String,
    /// e.g. `200`, `429 → hot`, `resolve_error: env var X unset`.
    pub outcome: String,
}

/// The response the caller should surface. On success `buffered` is empty
/// and `head.reader` streams the body; on upstream failure the body was
/// already consumed into `buffered` (the reader is exhausted).
pub struct Forward {
    pub head: UpstreamHead,
    pub buffered: Vec<u8>,
    pub attempts: Vec<Attempt>,
    /// At least one key was benched during this request.
    pub rotated: bool,
}

/// Nothing was cold enough to send — the caller synthesizes a 503/429.
pub struct PoolDrained {
    pub message: String,
    pub next_available_at: Option<f64>,
    pub attempts: Vec<Attempt>,
}

/// Failure bodies worth keeping verbatim for the caller.
const ERROR_BODY_CAP: u64 = 256 * 1024;
/// Belt-and-braces bound; marks already bound attempts to pool size.
const MAX_ATTEMPTS: usize = 8;

fn read_capped(reader: &mut dyn Read, cap: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = reader.take(cap).read_to_end(&mut buf);
    buf
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<f64> {
    if let Some(ms) = headers
        .get("retry-after-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
    {
        return Some(ms / 1000.0);
    }
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
}

fn header_list(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    const HOP_BY_HOP: &[&str] = &[
        "connection",
        "keep-alive",
        "transfer-encoding",
        "upgrade",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "content-length", // recomputed by the caller for buffered bodies
    ];
    headers
        .iter()
        .filter(|(name, _)| !HOP_BY_HOP.contains(&name.as_str().to_ascii_lowercase().as_str()))
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_string(), v.to_string()))
        })
        .collect()
}

/// One real HTTP attempt: `{method} {base_url}{path}` with
/// `Authorization: Bearer <key>`; JSON body when present.
pub fn http_send(
    client: &reqwest::blocking::Client,
    base_url: &str,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    key: &str,
) -> Result<UpstreamHead, String> {
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let mut req = match method {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        other => return Err(format!("unsupported method {other}")),
    };
    req = req
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"))
        .header(reqwest::header::ACCEPT, "*/*");
    if let Some(body) = body {
        req = req
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_vec());
    }
    let resp = req.send().map_err(|e| format!("{e}"))?;
    let headers = resp.headers().clone();
    Ok(UpstreamHead {
        status: resp.status().as_u16(),
        retry_after: retry_after(&headers),
        headers: header_list(&headers),
        reader: Box::new(resp),
    })
}

fn synth_head(status: u16, body: Vec<u8>) -> UpstreamHead {
    UpstreamHead {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        retry_after: None,
        reader: Box::new(std::io::Cursor::new(body)),
    }
}

/// The failover walk. `send` runs once per attempt with the entry that
/// `select` picked and its resolved key material.
///
/// `Ok(Forward)` always carries a surfaceable response — success, a
/// non-rotatable upstream error passed through verbatim, or the last
/// upstream error when the pool drained mid-walk (the host sees the real
/// 401/429 rather than a synthetic pool error). `Err(PoolDrained)` only
/// when no key was cold enough to attempt at all.
pub fn forward(
    store_path: &Path,
    pool_name: &str,
    mut send: impl FnMut(&pool::PooledKey, &str) -> Result<UpstreamHead, String>,
) -> Result<Forward, PoolDrained> {
    let mut attempts: Vec<Attempt> = Vec::new();
    let mut rotated = false;
    for _ in 0..MAX_ATTEMPTS {
        let now = crate::now();
        // Step 1 — under the lock: pick a cold entry.
        let entry = {
            let (mut file, _lock) = store::load_locked(store_path).map_err(|e| PoolDrained {
                message: e.to_string(),
                next_available_at: None,
                attempts: attempts.clone(),
            })?;
            let Some(pool_file) = file.pools.get_mut(pool_name) else {
                return Err(PoolDrained {
                    message: format!("unknown pool '{pool_name}'"),
                    next_available_at: None,
                    attempts,
                });
            };
            let Some(idx) = pool::select(pool_file, now) else {
                return Err(PoolDrained {
                    message: format!("pool '{pool_name}': every key is cooling or dead"),
                    next_available_at: pool::next_available_at(pool_file, now),
                    attempts,
                });
            };
            let entry = pool_file.entries[idx].clone();
            // Persist the request_count/last_selected bump (cheap, small file).
            let _ = store::save(store_path, &file);
            entry
        };

        // Step 2 — unlocked: resolve the key ref.
        let key = match crate::keys::resolve(&entry.key_ref) {
            Ok(key) => key,
            Err(e) => {
                attempts.push(Attempt {
                    key_id: entry.id.clone(),
                    key_label: entry.label.clone(),
                    outcome: format!("resolve_error: {e}"),
                });
                rotated |= mark_and_report(
                    store_path,
                    pool_name,
                    &entry.id,
                    None,
                    Some(pool::FAILURE_RESOLVE),
                    Some(e),
                    None,
                    now,
                );
                continue;
            }
        };

        // Step 3 — unlocked: one upstream attempt.
        let mut head = match send(&entry, &key) {
            Ok(head) => head,
            Err(transport) => {
                attempts.push(Attempt {
                    key_id: entry.id.clone(),
                    key_label: entry.label.clone(),
                    outcome: format!("transport: {transport}"),
                });
                let more = mark_and_report(
                    store_path,
                    pool_name,
                    &entry.id,
                    None,
                    Some("transport"),
                    Some(transport.clone()),
                    None,
                    now,
                );
                rotated = true;
                if more {
                    continue;
                }
                // Drained on a transport failure: synthesize the response
                // the host would have seen.
                let body = serde_json::to_vec(&serde_json::json!({
                    "error": {"message": format!("upstream unreachable: {transport}"),
                              "type": "server_error"}
                }))
                .unwrap_or_default();
                return Ok(Forward {
                    head: synth_head(502, body.clone()),
                    buffered: body,
                    attempts,
                    rotated,
                });
            }
        };

        let status = head.status;
        if status < 400 {
            attempts.push(Attempt {
                key_id: entry.id,
                key_label: entry.label,
                outcome: format!("{status}"),
            });
            return Ok(Forward {
                head,
                buffered: Vec::new(),
                attempts,
                rotated,
            });
        }

        // Failure: buffer the body so the mark decision and (if the pool
        // is drained) the verbatim surface both see it.
        let body = read_capped(&mut *head.reader, ERROR_BODY_CAP);
        let snippet = String::from_utf8_lossy(&body).chars().take(300).collect::<String>();
        attempts.push(Attempt {
            key_id: entry.id.clone(),
            key_label: entry.label.clone(),
            outcome: format!("{status} {snippet}"),
        });
        let reset_at = head.retry_after.map(|secs| now + secs);
        if pool::is_rotatable_status(Some(status)) {
            let more = mark_and_report(
                store_path,
                pool_name,
                &entry.id,
                Some(status),
                None,
                Some(snippet),
                reset_at,
                now,
            );
            rotated = true;
            if more {
                continue;
            }
        }
        // Non-rotatable error, or pool drained: surface verbatim.
        return Ok(Forward {
            head: UpstreamHead {
                status,
                headers: head.headers,
                retry_after: head.retry_after,
                reader: Box::new(std::io::Cursor::new(body.clone())),
            },
            buffered: body,
            attempts,
            rotated,
        });
    }
    Err(PoolDrained {
        message: format!("pool '{pool_name}': attempt cap reached"),
        next_available_at: None,
        attempts,
    })
}

/// Mark `entry_id` hot under the lock; returns true when another cold
/// entry remains (rotation is possible). Errors are logged, never fatal —
/// a failed mark must not wedge the request.
fn mark_and_report(
    store_path: &Path,
    pool_name: &str,
    entry_id: &str,
    code: Option<u16>,
    reason: Option<&str>,
    message: Option<String>,
    reset_at: Option<f64>,
    now: f64,
) -> bool {
    let result = (|| -> anyhow::Result<bool> {
        let (mut file, _lock) = store::load_locked(store_path)?;
        let Some(pool_file) = file.pools.get_mut(pool_name) else {
            anyhow::bail!("unknown pool '{pool_name}'");
        };
        let Some(idx) = pool_file.entries.iter().position(|e| e.id == entry_id) else {
            anyhow::bail!("pool '{pool_name}': entry {entry_id} vanished");
        };
        pool::mark(pool_file, idx, code, reason, message, reset_at, now, false);
        let has_cold = !pool::cold_indices(pool_file, crate::now()).is_empty();
        store::save(store_path, &file)?;
        Ok(has_cold)
    })();
    match result {
        Ok(more) => more,
        Err(e) => {
            eprintln!("credpool: failed to mark {entry_id}: {e}");
            false
        }
    }
}
