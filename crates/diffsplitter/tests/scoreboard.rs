//! Integration tests for the Phase 4 scoreboard routes.
//!
//! Mirrors the dashboard test setup: temp-file SQLite, seed a few diffs,
//! exercise via `tower::ServiceExt::oneshot`. We deliberately seed rows with
//! explicit `observed_at_ns` timestamps (via direct INSERT) so we can test
//! the rolling-window math without sleeping.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use diffsplitter::config::Config;
use diffsplitter::{db, scoreboard, State};
use http_body_util::BodyExt;
use rusqlite::params;
use tower::ServiceExt;

fn tmp_db_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "diffsplitter-scoreboard-test-{}-{:?}-{}.db",
        std::process::id(),
        std::thread::current().id(),
        n,
    ));
    p
}

fn fresh_state() -> Arc<State> {
    let path = tmp_db_path();
    let pool = db::open(&path).expect("open sqlite");
    db::migrate(&pool).expect("migrate");
    let cfg = Config {
        backend_primary_url: "http://primary.invalid".to_string(),
        backend_shadow_url: "http://shadow.invalid".to_string(),
        primary_admin_token: None,
        shadow_admin_token: None,
        shadow_sample_rate: 0.0,
        sqlite_path: path,
        port: 0,
        max_write_attempts: 1,
        version_poll_interval_secs: 60,
        diff_body_max_bytes: 4096,
    };
    let http = reqwest::Client::builder().build().expect("reqwest client");
    Arc::new(State { cfg, http, pool })
}

fn router(state: Arc<State>) -> axum::Router {
    axum::Router::new()
        .merge(scoreboard::routes())
        .with_state(state)
}

/// Insert a diff row with an explicit observed_at_ns so we can pin where in
/// the rolling window it lands. Bypasses `db::record_diff` (which always uses
/// `now_ns`).
fn seed_diff_at(state: &Arc<State>, observed_at_ns: i64, path: &str, severity: &str) {
    let conn = state.pool.lock();
    conn.execute(
        "INSERT INTO diffs (observed_at_ns, method, path, primary_status, shadow_status,
            primary_body, shadow_body, diff_blob, severity, descended_from_failed_write_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,NULL)",
        params![
            observed_at_ns,
            "GET",
            path,
            200i64,
            500i64,
            r#"{"a":1}"#,
            r#"{"a":2}"#,
            r#"{"kind":"json"}"#,
            severity,
        ],
    )
    .expect("insert diff");
}

/// Insert a write-queue row at a specific time (so it counts toward the
/// requests denominator).
fn seed_write_at(state: &Arc<State>, enqueued_at_ns: i64, path: &str) {
    let conn = state.pool.lock();
    conn.execute(
        "INSERT INTO write_queue (enqueued_at_ns, method, path, body, content_type, next_attempt_at_ns)
            VALUES (?1, ?2, ?3, NULL, NULL, ?1)",
        params![enqueued_at_ns, "POST", path],
    )
    .expect("insert write");
}

async fn body_str(resp: axum::response::Response) -> String {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("utf8 body")
}

const NS_SEC: i64 = 1_000_000_000;

#[tokio::test]
async fn scoreboard_html_returns_200_with_all_sections() {
    let state = fresh_state();
    let now = db::now_ns();
    seed_diff_at(&state, now - 60 * NS_SEC, "/timeline", "critical");
    seed_diff_at(&state, now - 5 * 60 * NS_SEC, "/u/alice", "high");
    seed_write_at(&state, now - 60 * NS_SEC, "/users");

    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/scoreboard")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_str(resp).await;

    // All required sections present
    for needle in [
        "conformance scoreboard",
        "at a glance",
        "divergence rate · 1h",
        "divergence rate · 24h",
        "divergence rate · 7d",
        "severity breakdown",
        "<svg",
        "top diverging paths",
        "last critical diff",
        "/timeline",
        "/u/alice",
    ] {
        assert!(
            html.contains(needle),
            "missing `{needle}` in scoreboard HTML"
        );
    }
    // Page weight budget
    assert!(
        html.len() < 50 * 1024,
        "scoreboard exceeded 50KB budget: {} bytes",
        html.len()
    );
}

#[tokio::test]
async fn scoreboard_json_round_trips() {
    let state = fresh_state();
    let now = db::now_ns();
    seed_diff_at(&state, now - 60 * NS_SEC, "/timeline", "critical");
    seed_diff_at(&state, now - 60 * NS_SEC, "/timeline", "high");
    seed_diff_at(&state, now - 60 * NS_SEC, "/timeline", "high");
    seed_write_at(&state, now - 60 * NS_SEC, "/users");

    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/scoreboard.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_str(resp).await;
    let v: serde_json::Value = serde_json::from_str(&body).expect("valid json");

    // Top-level shape
    assert!(v["now_ns"].as_i64().unwrap_or(0) > 0);
    assert_eq!(v["diffs_total"].as_i64(), Some(3));
    assert_eq!(v["shadow_degraded"].as_bool(), Some(false));
    let windows = v["windows"].as_array().expect("windows[]");
    assert_eq!(windows.len(), 3);
    let labels: Vec<&str> = windows.iter().filter_map(|w| w["label"].as_str()).collect();
    assert_eq!(labels, vec!["1h", "24h", "7d"]);

    // 1h window has 3 diffs and 4 requests (3 diffs + 1 write).
    let h1 = &windows[0];
    assert_eq!(h1["diffs"].as_i64(), Some(3));
    assert_eq!(h1["requests"].as_i64(), Some(4));
    let rate = h1["rate"].as_f64().unwrap();
    assert!((rate - 0.75).abs() < 1e-9, "got rate {rate}");

    // Severity breakdown — canonical order, contains expected counts
    let sevs = v["severity_24h"].as_array().expect("sev[]");
    assert_eq!(sevs.len(), 5);
    let map: std::collections::HashMap<&str, i64> = sevs
        .iter()
        .map(|e| {
            (
                e["severity"].as_str().unwrap(),
                e["count"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(map.get("critical"), Some(&1));
    assert_eq!(map.get("high"), Some(&2));
    assert_eq!(map.get("medium"), Some(&0));

    // Top paths
    let top = v["top_paths_24h"].as_array().expect("top[]");
    assert_eq!(top.len(), 1);
    assert_eq!(top[0]["path"].as_str(), Some("/timeline"));
    assert_eq!(top[0]["diffs"].as_i64(), Some(3));

    // Last critical
    assert!(!v["last_critical"].is_null());
    assert!(v["last_critical"]["id"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn scoreboard_empty_db_returns_zeros() {
    let state = fresh_state();
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/scoreboard.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_str(resp).await;
    let v: serde_json::Value = serde_json::from_str(&body).expect("valid json");

    assert_eq!(v["diffs_total"].as_i64(), Some(0));
    assert_eq!(v["requests_total_approx"].as_i64(), Some(0));
    assert!(v["last_critical"].is_null());
    let windows = v["windows"].as_array().unwrap();
    for w in windows {
        assert_eq!(w["diffs"].as_i64(), Some(0));
        assert_eq!(w["requests"].as_i64(), Some(0));
        assert_eq!(w["rate"].as_f64(), Some(0.0));
    }
    let top = v["top_paths_24h"].as_array().unwrap();
    assert!(top.is_empty());
}

#[tokio::test]
async fn divergence_rate_calculation_buckets_correctly() {
    // Seed events at 30min, 12h, and 4d ago. The 1h window should see only
    // the 30min event; the 24h window should see the first two; the 7d
    // window should see all three.
    let state = fresh_state();
    let now = db::now_ns();
    let min30 = now - 30 * 60 * NS_SEC;
    let h12 = now - 12 * 3600 * NS_SEC;
    let d4 = now - 4 * 86400 * NS_SEC;
    seed_diff_at(&state, min30, "/p", "high");
    seed_diff_at(&state, h12, "/p", "high");
    seed_diff_at(&state, d4, "/p", "high");

    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/scoreboard.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");
    let body = body_str(resp).await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let windows = v["windows"].as_array().unwrap();

    assert_eq!(windows[0]["label"].as_str(), Some("1h"));
    assert_eq!(windows[0]["diffs"].as_i64(), Some(1));

    assert_eq!(windows[1]["label"].as_str(), Some("24h"));
    assert_eq!(windows[1]["diffs"].as_i64(), Some(2));

    assert_eq!(windows[2]["label"].as_str(), Some("7d"));
    assert_eq!(windows[2]["diffs"].as_i64(), Some(3));
}

#[tokio::test]
async fn scoreboard_shows_degraded_banner_when_shadow_degraded() {
    let state = fresh_state();
    db::upsert_backend_state(&state.pool, "shadow", 0, "abc", "degraded")
        .expect("upsert shadow degraded");

    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/scoreboard")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");
    let html = body_str(resp).await;
    assert!(html.contains("degraded"));
    assert!(html.contains("class=\"banner\""));
}
