//! Per-turn loopback relay — the interception point that exists today.
//!
//! `provider/chat` mints a bearer + opens a listener; the host POSTs its
//! standard Responses body to `…/relay/<bearer>/<path>` with
//! `Authorization: Bearer <bearer>`. The relay swaps in the pool-selected
//! key, forwards to the pool's `base_url`, and — on a rotatable upstream
//! status — benches that key and resends with the next cold one, all
//! inside the admitted request. The host sees one ordinary HTTP
//! response; rotation happens invisibly underneath it.
//!
//! Success bodies stream (`io::copy` chunks), failures buffer for the
//! mark decision. Relays self-expire after [`RELAY_TTL`].

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

/// A relay lives for one turn; 15 min is generous for one POST + stream.
const RELAY_TTL: Duration = Duration::from_secs(900);
const MAX_HEAD: usize = 1024 * 1024;
const MAX_BODY: usize = 256 << 20;
const READ_TIMEOUT: Duration = Duration::from_secs(330);
const IDLE_POLL: Duration = Duration::from_millis(50);

/// What `provider/chat` parked for one admitted request.
pub struct RelayIntent {
    pub pool: String,
}

pub type Intents = Arc<Mutex<HashMap<String, RelayIntent>>>;

/// Mint a per-turn bearer and open the listener. Returns `(port, bearer)`;
/// the caller embeds both in `relay_url`.
pub fn start(pool_name: &str, intents: &Intents) -> Result<(u16, String), String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("relay bind: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("relay nonblocking: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("relay addr: {e}"))?
        .port();
    let bearer = format!("cp-{}", crate::hex_id());
    intents
        .lock()
        .map_err(|_| "relay intents poisoned".to_string())?
        .insert(
            bearer.clone(),
            RelayIntent {
                pool: pool_name.to_string(),
            },
        );
    let intents2 = intents.clone();
    std::thread::spawn(move || serve(listener, intents2));
    Ok((port, bearer))
}

fn serve(listener: TcpListener, intents: Intents) {
    let deadline = Instant::now() + RELAY_TTL;
    loop {
        match listener.accept() {
            Ok((mut s, _)) => handle_conn(&mut s, &intents),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return;
                }
                std::thread::sleep(IDLE_POLL);
            }
            Err(_) => return,
        }
    }
}

fn write_resp(s: &mut TcpStream, status: u16, headers: &[(String, String)], body: &[u8]) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Response",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    let mut has_ct = false;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("content-length") || k.eq_ignore_ascii_case("connection") {
            continue;
        }
        has_ct |= k.eq_ignore_ascii_case("content-type");
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if !has_ct {
        head.push_str("Content-Type: application/json\r\n");
    }
    head.push_str("\r\n");
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body);
    let _ = s.flush();
}

/// Stream an accepted upstream response: headers first, then body chunks.
fn write_stream(
    s: &mut TcpStream,
    status: u16,
    headers: &[(String, String)],
    reader: &mut dyn Read,
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} OK\r\nConnection: close\r\n");
    let mut has_ct = false;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("connection") || k.eq_ignore_ascii_case("content-length") {
            continue;
        }
        has_ct |= k.eq_ignore_ascii_case("content-type");
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if !has_ct {
        head.push_str("Content-Type: application/octet-stream\r\n");
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes())?;
    std::io::copy(reader, s)?;
    s.flush()
}

struct ParsedRequest {
    method: String,
    path: String,
    auth: String,
    body: Vec<u8>,
}

fn read_request(s: &mut TcpStream) -> Option<ParsedRequest> {
    let _ = s.set_read_timeout(Some(READ_TIMEOUT));
    let mut buf = vec![0u8; 65536];
    let mut head = Vec::new();
    loop {
        let n = s.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        head.extend_from_slice(&buf[..n]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if head.len() > MAX_HEAD {
            return None;
        }
    }
    let head_str = String::from_utf8_lossy(&head).into_owned();
    let mut lines = head_str.lines();
    let request_line = lines.next().unwrap_or("").to_string();
    let mut len = 0usize;
    let mut auth = String::new();
    for line in lines {
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let Some((name, v)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            len = v.trim().parse().unwrap_or(0);
        } else if name.eq_ignore_ascii_case("authorization") {
            auth = v.trim().to_string();
        }
    }
    if len > MAX_BODY {
        return None;
    }
    let header_end = head
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(head.len());

    let mut body = head[header_end..].to_vec();
    while body.len() < len {
        let n = s.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    let mut parts = request_line.split_whitespace();
    Some(ParsedRequest {
        method: parts.next().unwrap_or("").to_string(),
        path: parts.next().unwrap_or("").to_string(),
        auth,
        body,
    })
}

fn handle_conn(s: &mut TcpStream, intents: &Intents) {
    let Some(req) = read_request(s) else {
        return;
    };
    // /relay/<bearer>/<upstream path>
    let Some(stripped) = req.path.strip_prefix("/relay/") else {
        write_resp(s, 404, &[], b"not found");
        return;
    };
    let (bearer, upstream_path) = match stripped.split_once('/') {
        Some((b, p)) => (b.to_string(), format!("/{p}")),
        None => (stripped.to_string(), "/responses".to_string()),
    };
    if req.auth != format!("Bearer {bearer}") {
        write_resp(s, 404, &[], b"not found");
        return;
    }
    let pool_name = match intents.lock().ok().and_then(|m| m.get(bearer.as_str()).map(|i| i.pool.clone())) {
        Some(p) => p,
        None => {
            write_resp(s, 404, &[], b"not found");
            return;
        }
    };
    serve_turn(s, &pool_name, &req.method, &upstream_path, &req.body);
}

fn serve_turn(s: &mut TcpStream, pool_name: &str, method: &str, upstream_path: &str, body: &[u8]) {
    let fail = |s: &mut TcpStream, e: String| {
        let b = json!({"error": {"type": "server_error", "message": format!("credpool: {e}")}});
        write_resp(s, 500, &[], b.to_string().as_bytes());
    };
    let store_path = match crate::pool_path() {
        Ok(p) => p,
        Err(e) => return fail(s, e.to_string()),
    };
    let base_url = match crate::store::load(&store_path)
        .ok()
        .and_then(|f| f.pools.get(pool_name).map(|p| p.base_url.clone()))
    {
        Some(b) if !b.is_empty() => b,
        _ => return fail(s, format!("unknown pool '{pool_name}'")),
    };
    if crate::mock::is_mock(&base_url) {
        let endpoint = match crate::mock::MockEndpoint::load(&base_url) {
            Ok(m) => m,
            Err(e) => return fail(s, e),
        };
        let fwd = crate::upstream::forward(&store_path, pool_name, |_entry, key| {
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
        finish(s, fwd);
        return;
    }
    let client = match reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(600))
        .build()
    {
        Ok(c) => c,
        Err(e) => return fail(s, format!("http client: {e}")),
    };
    let fwd = crate::upstream::forward(&store_path, pool_name, |_entry, key| {
        crate::upstream::http_send(&client, &base_url, method, upstream_path, Some(body), key)
    });
    finish(s, fwd);
}

fn finish(s: &mut TcpStream, fwd: Result<crate::upstream::Forward, crate::upstream::PoolDrained>) {
    match fwd {
        Ok(forward) => {
            for a in &forward.attempts {
                eprintln!("credpool relay: {} ({}) → {}", a.key_label, a.key_id, a.outcome);
            }
            if forward.head.status >= 400 || !forward.buffered.is_empty() {
                write_resp(s, forward.head.status, &forward.head.headers, &forward.buffered);
            } else {
                let mut head = forward.head;
                let _ = write_stream(s, head.status, &head.headers, &mut *head.reader);
            }
        }
        Err(drained) => {
            let body = json!({
                "error": {
                    "type": "rate_limit_error",
                    "message": drained.message,
                    "credpool_next_available_at": drained.next_available_at,
                }
            });
            write_resp(s, 503, &[], body.to_string().as_bytes());
        }
    }
}

/// For tests: make one admitted POST against a live relay.
#[cfg(test)]
pub fn post(port: u16, bearer: &str, path: &str, body: &[u8]) -> std::io::Result<(u16, Vec<u8>)> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    let req = format!(
        "POST /relay/{bearer}{path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {bearer}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(req.as_bytes())?;
    s.write_all(body)?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let body_start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    Ok((status, raw[body_start..].to_vec()))
}
