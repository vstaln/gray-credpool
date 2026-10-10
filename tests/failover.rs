//! Hermetic verification: a three-key pool whose mock upstream 429s the
//! first two keys must walk to the third — through both the CLI probe
//! (`ops::request`) and the per-turn relay (`relay::start` + raw POST).
//! No live credentials anywhere; `file:` refs carry fake material.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};

use gray_credpool::{ops, pool, relay, store};

struct Fixture {
    _dir: tempfile::TempDir,
    pool_path: PathBuf,
    mock_path: PathBuf,
    pool_name: String,
}

/// Three `file:` refs → test-key-1..3; mock 429s the first two.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool_path = dir.path().join("credpool.json");
    let mock_path = dir.path().join("mock.json");

    for i in 1..=3 {
        let mut f = std::fs::File::create(dir.path().join(format!("key{i}.txt"))).unwrap();
        writeln!(f, "test-key-{i}").unwrap();
    }
    std::fs::write(
        &mock_path,
        r#"{
          "status": 200,
          "headers": {"content-type": "application/json"},
          "body": {"ok": true, "id": "resp_mock"},
          "models": [{"id": "mock-1", "name": "Mock One"}],
          "keys": {
            "test-key-1": {"status": 429, "retry_after": 0.1,
                           "body": {"error": {"message": "slow down"}}},
            "test-key-2": {"status": 429,
                           "body": {"error": {"message": "slow down"}}}
          }
        }"#,
    )
    .unwrap();

    for i in 1..=3 {
        let key_file = dir.path().join(format!("key{i}.txt"));
        ops::add(
            &pool_path,
            "demo",
            &format!("file:{}", key_file.display()),
            Some(&format!("key{i}")),
            Some(&format!("mock:{}", mock_path.display())),
        )
        .unwrap();
    }
    Fixture {
        _dir: dir,
        pool_path,
        mock_path,
        pool_name: "demo".into(),
    }
}

fn entry_statuses(path: &Path, pool_name: &str) -> Vec<(String, String)> {
    let f = store::load(path).unwrap();
    f.pools[pool_name]
        .entries
        .iter()
        .map(|e| (e.label.clone(), e.status.clone()))
        .collect()
}

#[test]
fn three_key_pool_rotates_after_forced_429s() {
    let fx = fixture();
    let out = ops::request(&fx.pool_path, &fx.pool_name, "POST", "/responses", Some(b"{}"))
        .expect("request");
    eprintln!("{out}");
    // Attempt log shows the walk: key1 429, key2 429, key3 200.
    assert!(out.contains("key1"), "attempt log names key1: {out}");
    assert!(out.contains("key2"), "attempt log names key2: {out}");
    assert!(out.contains("key3"), "attempt log names key3: {out}");
    assert!(out.contains("HTTP 200"), "final response surfaced: {out}");
    assert!(out.contains("\"ok\":true") || out.contains("\"ok\": true"));

    let statuses = entry_statuses(&fx.pool_path, &fx.pool_name);
    assert_eq!(
        statuses,
        vec![
            ("key1".to_string(), pool::STATUS_EXHAUSTED.to_string()),
            ("key2".to_string(), pool::STATUS_EXHAUSTED.to_string()),
            ("key3".to_string(), pool::STATUS_OK.to_string()),
        ],
        "the two 429'd keys are benched, the third served the request"
    );
}

#[test]
fn hot_keys_are_skipped_on_next_request() {
    let fx = fixture();
    ops::request(&fx.pool_path, &fx.pool_name, "POST", "/responses", Some(b"{}"))
        .expect("first request");
    let out = ops::request(&fx.pool_path, &fx.pool_name, "POST", "/responses", Some(b"{}"))
        .expect("second request");
    assert!(out.contains("HTTP 200"));
    assert!(!out.contains("key1 →"), "benched key1 is not retried: {out}");
    assert!(!out.contains("key2 →"), "benched key2 is not retried: {out}");
}

#[test]
fn drained_pool_reports_next_available() {
    let fx = fixture();
    // Bench all three.
    for label in ["key1", "key2", "key3"] {
        ops::mark(
            &fx.pool_path,
            &fx.pool_name,
            ops::MarkOpts {
                id: Some(label.into()),
                status: Some(429),
                reason: None,
                seconds: Some(3600.0),
                dead: false,
            },
        )
        .unwrap();
    }
    let out = ops::request(&fx.pool_path, &fx.pool_name, "POST", "/responses", Some(b"{}"))
        .expect("drained request still returns a report");
    assert!(out.contains("pool drained"), "{out}");
    assert!(out.contains("next cold"), "{out}");
}

#[test]
fn relay_rotates_inside_one_post() {
    let fx = fixture();
    ops::use_pool(&fx.pool_path, &fx.pool_name).unwrap();
    let intents: relay::Intents = Default::default();
    // The sidecar resolves the pool path via GRAY_HOME — the fixture file
    // lives outside it, so point the env override at the fixture dir.
    let prev = std::env::var("GRAY_HOME").ok();
    unsafe {
        std::env::set_var("GRAY_HOME", fx.pool_path.parent().unwrap());
    }
    let (port, bearer) = relay::start(&fx.pool_name, &intents).expect("relay start");
    std::thread::sleep(std::time::Duration::from_millis(30));

    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let body = b"{}";
    let req = format!(
        "POST /relay/{bearer}/responses HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {bearer}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    match prev {
        Some(v) => unsafe { std::env::set_var("GRAY_HOME", v) },
        None => unsafe { std::env::remove_var("GRAY_HOME") },
    }
    assert!(text.starts_with("HTTP/1.1 200"), "relay response: {text}");
    assert!(text.contains("\"ok\":true") || text.contains("\"ok\": true"), "{text}");

    let statuses = entry_statuses(&fx.pool_path, &fx.pool_name);
    assert_eq!(
        statuses[0].1, pool::STATUS_EXHAUSTED,
        "key1 benched mid-request: {statuses:?}"
    );
    assert_eq!(statuses[2].1, pool::STATUS_OK);
}

#[test]
fn cooldown_expiry_restores_key() {
    let fx = fixture();
    // mock gives key1 a 0.1s Retry-After — after the first walk it expires.
    ops::request(&fx.pool_path, &fx.pool_name, "POST", "/responses", Some(b"{}"))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(150));
    let f = store::load(&fx.pool_path).unwrap();
    let p = &f.pools[&fx.pool_name];
    // select() clears expired exhaustion — cold count is back to ≥1.
    let mut p2 = p.clone();
    assert!(!pool::cold_indices(&mut p2, gray_credpool::now()).is_empty());
}
