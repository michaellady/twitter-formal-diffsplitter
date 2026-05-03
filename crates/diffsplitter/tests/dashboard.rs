//! Integration tests for the Phase 2 dashboard routes.
//!
//! These spin up an in-memory-ish SQLite (a temp file — `:memory:` would work
//! too but a tempfile mirrors production semantics), seed a couple of diffs,
//! and exercise the axum router via `tower::ServiceExt::oneshot`.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use diffsplitter::config::Config;
use diffsplitter::{dashboard, db, State};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn tmp_db_path() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "diffsplitter-test-{}-{}.db",
        std::process::id(),
        rand_suffix()
    ));
    p
}

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n:x}")
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
        .merge(dashboard::routes())
        .with_state(state)
}

fn seed_diff(state: &Arc<State>, path: &str, severity: &str, descended_from: Option<i64>) -> i64 {
    db::record_diff(
        &state.pool,
        "GET",
        path,
        Some(200),
        Some(500),
        Some(r#"{"a":1}"#),
        Some(r#"{"a":2}"#),
        r#"{"kind":"json","primary":{"a":1},"shadow":{"a":2}}"#,
        severity,
        descended_from,
    )
    .expect("record_diff")
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

#[tokio::test]
async fn diffs_index_returns_200_with_expected_columns() {
    let state = fresh_state();
    let _ = seed_diff(&state, "/timeline", "high", None);
    let _ = seed_diff(&state, "/u/alice", "critical", Some(42));
    let app = router(state);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/diffs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_str(resp).await;
    // Column headers
    for col in [
        "timestamp",
        "method",
        "path",
        "primary",
        "shadow",
        "severity",
    ] {
        assert!(
            html.contains(&format!("<th>{col}</th>")),
            "missing column header `{col}` in body:\n{}",
            &html[..html.len().min(2000)]
        );
    }
    // Both rows present
    assert!(html.contains("/timeline"));
    assert!(html.contains("/u/alice"));
    // K13 degraded styling for the descended-from row
    assert!(
        html.contains("class=\"degraded\""),
        "expected greyed-out row for diff with descended_from_failed_write_id"
    );
    // Per-row drill-in link
    assert!(html.contains("/diffs/2") || html.contains("/diffs/1"));
    // Page weight budget — well under 50 KB.
    assert!(
        html.len() < 50 * 1024,
        "page exceeded 50KB budget: {} bytes",
        html.len()
    );
}

#[tokio::test]
async fn diffs_show_nonexistent_returns_404() {
    let state = fresh_state();
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/diffs/9999")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn diffs_show_existing_returns_html() {
    let state = fresh_state();
    let id = seed_diff(&state, "/timeline", "high", None);
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/diffs/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_str(resp).await;
    assert!(html.contains("primary body"));
    assert!(html.contains("shadow body"));
    assert!(html.contains("structured diff"));
}

#[tokio::test]
async fn diffs_json_round_trips_schema() {
    let state = fresh_state();
    let id = seed_diff(&state, "/timeline", "high", None);
    let app = router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/diffs.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_str(resp).await;
    let v: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    let diffs = v.get("diffs").and_then(|d| d.as_array()).expect("diffs[]");
    assert_eq!(diffs.len(), 1);
    let row = &diffs[0];
    assert_eq!(row["id"].as_i64(), Some(id));
    assert_eq!(row["method"].as_str(), Some("GET"));
    assert_eq!(row["path"].as_str(), Some("/timeline"));
    assert_eq!(row["severity"].as_str(), Some("high"));
    assert_eq!(row["primary_status"].as_i64(), Some(200));
    assert_eq!(row["shadow_status"].as_i64(), Some(500));
    assert!(row["observed_at_ns"].as_i64().unwrap_or(0) > 0);
}

#[tokio::test]
async fn diffs_index_severity_filter() {
    let state = fresh_state();
    let _ = seed_diff(&state, "/timeline", "high", None);
    let _ = seed_diff(&state, "/u/alice", "critical", None);
    let app = router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/diffs.json?severity=critical")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_str(resp).await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let diffs = v["diffs"].as_array().unwrap();
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0]["severity"].as_str(), Some("critical"));
}

#[tokio::test]
async fn diffs_index_rejects_bad_severity() {
    let state = fresh_state();
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/diffs?severity=bogus")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
