//! NDJSON sidecar loop: host→stdin requests → stdout replies.
//!
//! Handled methods:
//! - `plugin/manifest` → the manifest
//! - `provider/chat`   → mint a per-turn relay, return {relay_url, relay_token}
//! - `provider/models` → the pool's model catalog
//! - `tool/call`       → credpool_* pool tools
//! - `command/run`     → `/credpool` → CLI verbs as {text}
//! - `prompt/context`  → pool status injected into the system context
//! - `tool/after`      → heuristic: bench the last-picked key when a tool
//!                       result carries an upstream-looking 4xx/5xx marker
//! - `plugin/shutdown` → exit (reader thread breaks, queue drains)

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex, mpsc};

use serde_json::{Value, json};

use crate::cli::Cli;
use crate::manifest;
use crate::ops;
use crate::relay::{self, Intents};

fn tool_ok(text: String) -> Value {
    json!({"content": text})
}

fn tool_err(e: impl std::fmt::Display) -> Value {
    json!({"content": format!("{e}"), "is_error": true})
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    arg_str(args, key).ok_or_else(|| format!("missing '{key}'"))
}

/// Which pool a provider request routes to: the declared default, else
/// the sole pool, else loud error.
fn pool_for_request(path: &std::path::Path) -> Result<String, String> {
    let f = crate::store::load(path).map_err(|e| e.to_string())?;
    if let Some(d) = &f.default_pool {
        if f.pools.contains_key(d) {
            return Ok(d.clone());
        }
    }
    match f.pools.len() {
        1 => Ok(f.pools.keys().next().expect("len 1").clone()),
        0 => Err("credpool: no pools configured".into()),
        _ => Err("credpool: multiple pools, set a default (`gray credpool use <name>`)".into()),
    }
}

fn provider_chat(path: &std::path::Path, intents: &Intents) -> Value {
    let pool = match pool_for_request(path) {
        Ok(p) => p,
        Err(e) => return tool_err(e),
    };
    match relay::start(&pool, intents) {
        Ok((port, bearer)) => json!({
            "relay_url": format!("http://127.0.0.1:{port}/relay/{bearer}/responses"),
            "relay_token": bearer,
        }),
        Err(e) => tool_err(e),
    }
}

fn provider_models(path: &std::path::Path) -> Value {
    let pool = match pool_for_request(path) {
        Ok(p) => p,
        Err(e) => return tool_err(e),
    };
    match ops::models(path, &pool) {
        Ok(models) => json!({"models": models}),
        Err(e) => tool_err(e),
    }
}

fn tool_call(path: &std::path::Path, params: &Value) -> Value {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("args").cloned().unwrap_or(Value::Null);
    let r = match name {
        "credpool_status" => ops::status(path, arg_str(&args, "provider")),
        "credpool_add" => (|| {
            ops::add(
                path,
                required(&args, "provider")?,
                required(&args, "key_ref")?,
                arg_str(&args, "label"),
                arg_str(&args, "base_url"),
            )
        })(),
        "credpool_remove" => (|| {
            ops::remove(path, required(&args, "provider")?, required(&args, "id")?)
        })(),
        "credpool_mark" => (|| {
            ops::mark(
                path,
                required(&args, "provider")?,
                ops::MarkOpts {
                    id: arg_str(&args, "id").map(str::to_string),
                    status: args.get("status").and_then(|v| v.as_u64()).map(|v| v as u16),
                    reason: arg_str(&args, "reason").map(str::to_string),
                    seconds: args.get("seconds").and_then(|v| v.as_f64()),
                    dead: args.get("dead").and_then(|v| v.as_bool()).unwrap_or(false),
                },
            )
        })(),
        "credpool_rotate" => (|| ops::rotate(path, required(&args, "provider")?))(),
        "credpool_clear" => (|| {
            ops::clear(path, required(&args, "provider")?, arg_str(&args, "id"))
        })(),
        "" => Err("tool/call: missing name".into()),
        other => Err(format!("unknown tool: {other}")),
    };
    match r {
        Ok(text) => tool_ok(text),
        Err(e) => tool_err(e),
    }
}

fn command_run(path: &std::path::Path, params: &Value) -> Value {
    let argv: Vec<String> = params
        .get("argv")
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let clap_args: Vec<String> = std::iter::once("credpool".to_string())
        .chain(argv.iter().cloned())
        .collect();
    let file_flag = ["--file".to_string(), path.display().to_string()];
    let mut full: Vec<String> = vec![clap_args[0].clone()];
    full.extend(file_flag);
    full.extend(clap_args[1..].iter().cloned());
    use clap::Parser;
    match Cli::try_parse_from(&full) {
        Ok(cli) => match crate::cli::run(cli) {
            Ok(text) => json!({"text": text}),
            Err(e) => json!({"text": format!("credpool: {e}")}),
        },
        Err(e) => json!({"text": format!("{e}")}),
    }
}

fn prompt_context(path: &std::path::Path) -> Value {
    let f = match crate::store::load(path) {
        Ok(f) => f,
        Err(_) => return json!({}),
    };
    if f.pools.is_empty() {
        return json!({});
    }
    let mut lines = vec!["Credential pool active — keys rotate on rate-limit.".to_string()];
    for (name, p) in &f.pools {
        let now = crate::now();
        let cold = crate::pool::cold_indices(p, now).len();
        lines.push(format!("  {name}: {cold}/{} keys cold", p.entries.len()));
    }
    json!({"text": lines.join("\n")})
}

/// Best-effort failure observation. Provider HTTP errors never reach
/// plugins as events, so this watches tool results for upstream-looking
/// markers — lossy by nature, documented in docs/INTEGRATION.md.
fn tool_after(path: &std::path::Path, params: &Value) -> Value {
    let is_error = params
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !is_error {
        return json!({});
    }
    let content = params
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("");
    let status = detect_upstream_status(content);
    let Some(status) = status else {
        return json!({});
    };
    let Ok(f) = crate::store::load(path) else {
        return json!({});
    };
    let Some(pool_name) = f
        .pools
        .iter()
        .find(|(_, p)| p.watch_tool_errors)
        .map(|(n, _)| n.clone())
        .or_else(|| f.default_pool.clone().filter(|d| f.pools.get(d).is_some_and(|p| p.watch_tool_errors)))
    else {
        return json!({});
    };
    let _ = ops::mark(
        path,
        &pool_name,
        ops::MarkOpts {
            id: None,
            status: Some(status),
            reason: None,
            seconds: None,
            dead: false,
        },
    );
    json!({})
}

/// A tool result "carries" an upstream failure when it looks like an HTTP
/// status the pool rotates on. Narrow on purpose — the false-positive cost
/// is a benched key.
fn detect_upstream_status(content: &str) -> Option<u16> {
    for needle in ["status 429", "HTTP 429", "429 Too Many", "rate_limit", "status 401", "HTTP 401"] {
        if content.contains(needle) {
            return Some(if needle.contains("401") { 401 } else { 429 });
        }
    }
    None
}

pub fn run() {
    let path = match crate::pool_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("credpool: {e}");
            std::process::exit(1);
        }
    };
    let intents: Intents = Arc::new(Mutex::new(HashMap::new()));
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let (work_tx, work_rx) = mpsc::channel::<Value>();

    let reader = std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v.get("method").and_then(|m| m.as_str()) == Some("plugin/shutdown") {
                break;
            }
            if work_tx.send(v).is_err() {
                break;
            }
        }
    });

    for req in work_rx {
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        // Notifications have no id — never reply.
        let Some(id) = req.get("id").cloned() else {
            continue;
        };
        let params = req.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "plugin/manifest" => manifest::manifest(),
            "provider/chat" => provider_chat(&path, &intents),
            "provider/models" => provider_models(&path),
            "tool/call" => tool_call(&path, &params),
            "command/run" => command_run(&path, &params),
            "prompt/context" => prompt_context(&path),
            "tool/after" => tool_after(&path, &params),
            _ => {
                let reply = json!({"id": id, "error": {"code": -32601, "message": format!("unknown method: {method}")}});
                let mut o = stdout.lock().expect("stdout");
                let _ = writeln!(o, "{reply}");
                let _ = o.flush();
                continue;
            }
        };
        let reply = json!({"id": id, "result": result});
        let mut o = stdout.lock().expect("stdout");
        let _ = writeln!(o, "{reply}");
        let _ = o.flush();
    }
    let _ = reader.join();
}
