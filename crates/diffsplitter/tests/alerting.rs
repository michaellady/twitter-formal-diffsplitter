//! Integration tests for Stream 2 Phase 3 alerting.
//!
//! Covers the WebhookNotifier wire format against an in-process httpmock,
//! and the DB-side `claim_for_notification` de-dup contract — the same
//! property that prevents double-firing across proxy restarts.

use std::path::PathBuf;
use std::sync::Arc;

use diffsplitter::alerting::{AlertDiff, Notifier, NotifierSet, WebhookNotifier};
use diffsplitter::db;
use httpmock::prelude::*;
use serde_json::Value;

fn tmp_db_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "diffsplitter-alerting-test-{}-{:?}-{}-{:x}.db",
        std::process::id(),
        std::thread::current().id(),
        seq,
        n
    ));
    p
}

fn sample_diff(id: i64) -> AlertDiff {
    AlertDiff {
        diff_id: id,
        severity: "critical".to_string(),
        request_path: "/tweets/abc".to_string(),
        method: "GET".to_string(),
        primary_status: Some(200),
        shadow_status: Some(500),
        observed_at_ns: 1_700_000_000_000_000_000,
    }
}

#[tokio::test]
async fn webhook_notifier_posts_expected_json_shape() {
    let server = MockServer::start_async().await;
    let mock = server.mock_async(|when, then| {
        when.method(POST)
            .path("/hook")
            .header("content-type", "application/json");
        then.status(200).body("ok");
    }).await;

    let url = server.url("/hook");
    let n = WebhookNotifier::new(url);
    let diff = sample_diff(99);
    n.notify(&diff).await.expect("webhook notify ok");

    mock.assert_async().await;

    // Verify payload shape from the recorded request.
    let received = &mock.hits_async().await;
    assert_eq!(*received, 1);

    // Re-fetch the request body via a second mock that captures it.
    let server2 = MockServer::start_async().await;
    let captured = server2.mock_async(|when, then| {
        when.method(POST).path("/cap");
        then.status(200);
    }).await;
    let n2 = WebhookNotifier::new(server2.url("/cap"));
    n2.notify(&diff).await.expect("second notify ok");
    let history = captured.hits_async().await;
    assert_eq!(history, 1);

    // httpmock 0.7's recorded-body access: pull through `received_requests`
    // off the server. The `last_hits` API differs across versions; instead
    // verify shape by spinning up a third mock that asserts on body fields.
    let server3 = MockServer::start_async().await;
    let body_mock = server3.mock_async(|when, then| {
        when.method(POST)
            .path("/shape")
            .json_body_partial(
                r#"{"diff_id":99,"severity":"critical","request_path":"/tweets/abc","method":"GET"}"#,
            );
        then.status(200);
    }).await;
    let n3 = WebhookNotifier::new(server3.url("/shape"));
    n3.notify(&diff).await.expect("third notify ok");
    body_mock.assert_async().await;
}

#[tokio::test]
async fn webhook_notifier_propagates_non_2xx_as_error() {
    let server = MockServer::start_async().await;
    let _mock = server.mock_async(|when, then| {
        when.method(POST).path("/fail");
        then.status(503).body("upstream busy");
    }).await;

    let n = WebhookNotifier::new(server.url("/fail"));
    let err = n.notify(&sample_diff(1)).await.expect_err("should error");
    let msg = format!("{err}");
    assert!(msg.contains("503"), "expected 503 in error: {msg}");
}

#[tokio::test]
async fn notifier_set_fan_out_does_not_propagate_individual_failures() {
    // Failing webhook + always-on log notifier — fire() must not panic
    // and must not return Result::Err (it returns ()).
    let server = MockServer::start_async().await;
    let _mock = server.mock_async(|when, then| {
        when.method(POST).path("/fail");
        then.status(500);
    }).await;

    let mut set = NotifierSet {
        notifiers: vec![Arc::new(diffsplitter::alerting::LogNotifier)],
    };
    set.notifiers
        .push(Arc::new(WebhookNotifier::new(server.url("/fail"))));

    set.fire(&sample_diff(7)).await;
}

#[tokio::test]
async fn claim_for_notification_is_single_shot() {
    let path = tmp_db_path();
    let pool = db::open(&path).expect("open");
    db::migrate(&pool).expect("migrate");

    // Insert a critical diff.
    let id = db::record_diff(
        &pool, "GET", "/x", Some(200), Some(500),
        Some("a"), Some("b"), "diff", "critical", None,
    ).expect("record");

    let first = db::claim_for_notification(&pool, id).expect("claim 1");
    let second = db::claim_for_notification(&pool, id).expect("claim 2");
    assert!(first, "first claim must succeed");
    assert!(!second, "second claim must be skipped (no double-fire)");
}

#[tokio::test]
async fn end_to_end_critical_diff_triggers_log_notifier() {
    // Mirrors the proxy hot-path: insert a critical row, claim it, fan out
    // through a NotifierSet that contains LogNotifier. We don't use a
    // tracing-test layer (would pull a heavy dep); instead we assert the
    // contract: claim returns true once, the fire() call completes without
    // error, and a second claim returns false.
    let path = tmp_db_path();
    let pool = db::open(&path).expect("open");
    db::migrate(&pool).expect("migrate");

    let id = db::record_diff(
        &pool,
        "GET",
        "/users/me",
        Some(200),
        Some(500),
        Some(r#"{"author":"alice"}"#),
        Some(r#"{"author":"bob"}"#),
        "diff: author differs",
        "critical",
        None,
    )
    .expect("record diff");

    assert!(db::claim_for_notification(&pool, id).expect("claim"));

    let set = NotifierSet {
        notifiers: vec![Arc::new(diffsplitter::alerting::LogNotifier)],
    };
    let alert = AlertDiff {
        diff_id: id,
        severity: "critical".to_string(),
        request_path: "/users/me".to_string(),
        method: "GET".to_string(),
        primary_status: Some(200),
        shadow_status: Some(500),
        observed_at_ns: 1_700_000_000_000_000_000,
    };
    set.fire(&alert).await;

    // Second claim must be a no-op (de-dup contract).
    assert!(!db::claim_for_notification(&pool, id).expect("claim 2"));
}

#[tokio::test]
async fn diffs_json_payload_shape_matches_alert_payload() {
    // Belt-and-suspenders: the JSON we send to webhooks must serialize the
    // same field names operators see in the dashboard. If a future refactor
    // renames a field on AlertDiff this test catches the wire-break.
    let diff = sample_diff(123);
    let v: Value = serde_json::to_value(&diff).expect("serialize");
    for field in [
        "diff_id",
        "severity",
        "request_path",
        "method",
        "primary_status",
        "shadow_status",
        "observed_at_ns",
    ] {
        assert!(v.get(field).is_some(), "missing field {field}");
    }
}
