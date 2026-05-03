//! Axum router that fronts every backend route.
//!
//! Strategy: a single catch-all route reads the request, classifies it, and
//! either:
//! * **write path** — calls primary, returns its response, enqueues a copy of
//!   the request to the SQLite write queue for shadow replay. (Synchronous
//!   primary call so the client sees the same result they'd see talking to
//!   the primary directly.)
//! * **read path** — calls primary, returns; with `SHADOW_SAMPLE_RATE`
//!   probability also calls shadow async-after-response and records any diff.
//! * **observability** — `/metrics`, `/diffs.json`, `/_diffsplitter/healthz`
//!   are served locally without proxying.

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State as AxState};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use bytes::Bytes;
use http_body_util::BodyExt;
use rand::Rng;

use crate::{alerting, dashboard, db, diff, metrics, State};

pub fn router(state: Arc<State>) -> Router {
    Router::new()
        .route("/_diffsplitter/healthz", get(local_healthz))
        .route("/_diffsplitter/version", get(local_version))
        // We override /version locally so deploy.yml's post-deploy probe can
        // verify the diffsplitter's *own* git_sha rather than the primary's.
        // Anyone wanting the primary's /version can hit /version on the
        // primary directly; the proxied UI doesn't need it.
        .route("/version", get(local_version))
        .route("/metrics", get(metrics_handler))
        // Phase 2 dashboard: GET /diffs (HTML), /diffs/:id (HTML),
        // /diffs.json (JSON, supersedes the old handler in this file).
        .merge(dashboard::routes())
        .fallback(any(proxy_handler))
        .with_state(state)
}

async fn local_version() -> impl IntoResponse {
    // Mirrors the backends' /version: reads /etc/version.json baked in by
    // the Dockerfile at build time. On dev, falls back to a `dev` stub.
    let body = std::fs::read_to_string("/etc/version.json").unwrap_or_else(|_| {
        r#"{"git_sha":"dev","image_digest":"sha256:dev","snapshot_version":1}"#.to_string()
    });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
}

async fn local_healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn metrics_handler(AxState(state): AxState<Arc<State>>) -> impl IntoResponse {
    // Refresh gauges that aren't event-driven before encoding.
    if let Ok(n) = db::queue_depth(&state.pool) {
        metrics::QUEUE_DEPTH.set(n);
    }
    if let Ok(n) = db::failed_count(&state.pool) {
        metrics::FAILED_WRITES_TOTAL.set(n);
    }
    let body = metrics::render();
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Class {
    Write,
    Read,
    Admin,
    Other,
}

fn classify(method: &Method, path: &str) -> Class {
    // Local admin endpoints — pass through to primary verbatim if anyone
    // calls them. We intentionally do NOT replay /_admin/* to the shadow
    // because the resync worker owns that loop.
    if path.starts_with("/_admin/") {
        return Class::Admin;
    }
    match (method, path) {
        (&Method::POST, "/users") => Class::Write,
        (&Method::POST, "/tweets") => Class::Write,
        (&Method::POST, "/follow") => Class::Write,
        (&Method::DELETE, "/follow") => Class::Write,
        (&Method::GET, _) => Class::Read,
        _ => Class::Other,
    }
}

async fn proxy_handler(AxState(state): AxState<Arc<State>>, req: Request) -> Response {
    let started = Instant::now();
    let method = req.method().clone();
    let uri = req.uri().clone();
    let path_and_query = uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| uri.path().to_string());
    let path_only = uri.path().to_string();
    let class = classify(&method, &path_only);
    metrics::REQUESTS_TOTAL
        .with_label_values(&[class_label(class)])
        .inc();

    // Buffer the full body so we can replay it to the shadow.
    let (parts, body) = req.into_parts();
    let body_bytes: Bytes = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
            tracing::warn!(error=%e, "client body read failed");
            return (StatusCode::BAD_REQUEST, "request body").into_response();
        }
    };
    let req_headers = parts.headers.clone();
    let content_type = req_headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    // Always call primary first.
    let primary_res = call_upstream(
        &state.http,
        &state.cfg.backend_primary_url,
        &method,
        &path_and_query,
        &req_headers,
        body_bytes.clone(),
    )
    .await;

    // Build the response we'll return to the client (independent of what we
    // do with the shadow — never block on shadow IO).
    let (client_response, primary_status, primary_body_for_diff) = match primary_res {
        Ok(UpstreamResponse {
            status,
            headers,
            body,
        }) => {
            let status_u16 = status.as_u16();
            let body_clone = body.clone();
            let mut resp = Response::builder().status(status);
            for (k, v) in &headers {
                if !is_hop_by_hop(k) {
                    resp = resp.header(k, v);
                }
            }
            let resp = resp.body(Body::from(body)).unwrap_or_else(|_| {
                Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from("upstream"))
                    .unwrap()
            });
            (resp, Some(status_u16), Some(body_clone))
        }
        Err(e) => {
            tracing::warn!(target="primary", error=%e, "primary upstream failed");
            (
                (StatusCode::BAD_GATEWAY, format!("primary upstream: {e}")).into_response(),
                None,
                None,
            )
        }
    };

    // Now schedule shadow work asynchronously so client latency is not
    // affected. The brief's K22 budget is p99 ≤ 50ms additional overhead;
    // we want ZERO blocking on shadow.
    match class {
        Class::Write => {
            // Durable enqueue (synchronous SQLite insert; ~100us-1ms; well
            // under the latency budget). Even if the proxy crashes after the
            // primary returns 2xx but before the shadow ack, the next worker
            // tick replays the write.
            if let Err(e) = db::enqueue_write(
                &state.pool,
                method.as_str(),
                &path_and_query,
                &body_bytes,
                content_type.as_deref(),
            ) {
                tracing::error!(error=%e, "failed to enqueue shadow write");
            }
        }
        Class::Read => {
            if rand::thread_rng().gen::<f64>() < state.cfg.shadow_sample_rate {
                if let Some(p_status) = primary_status {
                    let p_body = primary_body_for_diff.clone().unwrap_or_default();
                    let st = state.clone();
                    let m = method.clone();
                    let pq = path_and_query.clone();
                    let h = req_headers.clone();
                    let p_only = path_only.clone();
                    tokio::spawn(async move {
                        diff_read(&st, m, pq, p_only, h, body_bytes, p_status, p_body).await;
                    });
                }
            }
        }
        Class::Admin | Class::Other => { /* no shadow work */ }
    }

    let elapsed = started.elapsed();
    tracing::debug!(method=%method, path=%path_only, ?elapsed, "proxied");
    client_response
}

fn class_label(c: Class) -> &'static str {
    match c {
        Class::Write => "write",
        Class::Read => "read",
        Class::Admin => "admin",
        Class::Other => "other",
    }
}

#[allow(clippy::too_many_arguments)]
async fn diff_read(
    state: &Arc<State>,
    method: Method,
    path_and_query: String,
    path_only: String,
    headers: HeaderMap,
    body: Bytes,
    primary_status: u16,
    primary_body: Bytes,
) {
    // K13: if shadow is degraded, don't sample reads.
    if let Ok(Some(s)) = db::get_backend_state(&state.pool, "shadow") {
        if s.state == "degraded" {
            return;
        }
    }

    let shadow_res = call_upstream(
        &state.http,
        &state.cfg.backend_shadow_url,
        &method,
        &path_and_query,
        &headers,
        body,
    )
    .await;
    let (s_status, s_body) = match shadow_res {
        Ok(r) => (r.status.as_u16(), r.body),
        Err(e) => {
            tracing::debug!(error=%e, "shadow read failed");
            return;
        }
    };
    let p_text = String::from_utf8_lossy(&primary_body).to_string();
    let s_text = String::from_utf8_lossy(&s_body).to_string();
    let outcome = diff::compare(&path_only, primary_status, s_status, &p_text, &s_text);
    if !outcome.diverged {
        return;
    }
    metrics::DIFF_TOTAL
        .with_label_values(&[outcome.severity.as_str()])
        .inc();
    let p_truncated = truncate(&p_text, state.cfg.diff_body_max_bytes);
    let s_truncated = truncate(&s_text, state.cfg.diff_body_max_bytes);
    let diff_id = match db::record_diff(
        &state.pool,
        method.as_str(),
        &path_only,
        Some(primary_status),
        Some(s_status),
        Some(&p_truncated),
        Some(&s_truncated),
        &outcome.blob,
        outcome.severity.as_str(),
        None,
    ) {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::warn!(error=%e, "failed to persist diff");
            None
        }
    };

    // Phase 3: alerting. Only critical severity fires notifiers, and only
    // when we successfully claim the row's `notified_at_ns` (atomic single-
    // statement UPDATE WHERE notified_at_ns IS NULL — survives restart and
    // concurrent claimers). Notifier failures never affect the request path
    // because diff_read itself runs on a tokio::spawn detached from the
    // client response.
    if outcome.severity == diff::Severity::Critical {
        if let Some(id) = diff_id {
            match db::claim_for_notification(&state.pool, id) {
                Ok(true) => {
                    let alert = alerting::AlertDiff {
                        diff_id: id,
                        severity: outcome.severity.as_str().to_string(),
                        request_path: path_only.clone(),
                        method: method.as_str().to_string(),
                        primary_status: Some(primary_status),
                        shadow_status: Some(s_status),
                        observed_at_ns: db::now_ns(),
                    };
                    state.notifiers.fire(&alert).await;
                }
                Ok(false) => {
                    tracing::debug!(diff_id = id, "diff already notified; skipping");
                }
                Err(e) => {
                    tracing::warn!(error=%e, diff_id=id, "claim_for_notification failed");
                }
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut t = s[..max].to_string();
    t.push_str("...[truncated]");
    t
}

#[derive(Debug)]
pub(crate) struct UpstreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

async fn call_upstream(
    client: &reqwest::Client,
    base: &str,
    method: &Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> anyhow::Result<UpstreamResponse> {
    let url = format!("{base}{path_and_query}");
    let mut req = client.request(method.clone(), &url);
    for (k, v) in headers.iter() {
        if forward_header(k) {
            req = req.header(k.as_str(), v);
        }
    }
    if !body.is_empty() {
        req = req.body(body.to_vec());
    }
    let resp = req.send().await?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.bytes().await?;
    Ok(UpstreamResponse {
        status: StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
        headers,
        body,
    })
}

/// Conservative header passthrough: drop hop-by-hop, drop the inbound Host
/// (reqwest will set the right one), keep cookies / content-type / accept /
/// user-agent / x-* etc.
fn forward_header(name: &HeaderName) -> bool {
    if is_hop_by_hop(name) {
        return false;
    }
    matches!(
        name.as_str(),
        "accept"
            | "accept-encoding"
            | "accept-language"
            | "authorization"
            | "content-type"
            | "content-length"
            | "cookie"
            | "user-agent"
            | "x-admin-token"
    ) || name.as_str().starts_with("x-")
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host"
            | "connection"
            | "keep-alive"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "upgrade"
            | "proxy-authorization"
            | "proxy-authenticate"
    )
}

#[allow(dead_code)]
pub(crate) fn _unused_silencer(h: &HeaderValue, u: &Uri) {
    let _ = (h, u);
}
