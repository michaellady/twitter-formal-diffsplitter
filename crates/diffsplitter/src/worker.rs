//! Background workers.
//!
//! 1. `run_write_replay`: pops one entry from the SQLite write queue at a
//!    time, replays it against the shadow, and either deletes it (on 2xx),
//!    increments the attempt counter (on transient failure), or moves it to
//!    `failed_writes` (after `max_write_attempts`).
//!
//! 2. `run_version_poller`: polls `/version` on both backends every 5s.
//!    On uptime regression (a restart), kicks off the K8/K12 resync flow.
//!    Since `/_admin/begin-resync` is not yet implemented backend-side, the
//!    fallback path is `POST /_admin/snapshot` on the still-live peer →
//!    `POST /_admin/load-snapshot` on the restarted backend.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use serde::Deserialize;
use tokio::time::sleep;

use crate::{db, metrics, State};

const RETRY_BACKOFFS_MS: &[u64] = &[
    1_000,   // 1s
    5_000,   // 5s
    15_000,  // 15s
    60_000,  // 1m
    180_000, // 3m
];

pub async fn run_write_replay(state: Arc<State>) {
    tracing::info!("write-replay worker starting");
    loop {
        let popped = db::pop_one_ready(&state.pool);
        let Ok(Some(qw)) = popped else {
            // No work; record gauge and sleep briefly.
            if let Ok(n) = db::queue_depth(&state.pool) {
                metrics::QUEUE_DEPTH.set(n);
            }
            sleep(Duration::from_millis(200)).await;
            continue;
        };

        metrics::WRITE_REPLAY_ATTEMPTS.inc();

        let url = format!("{}{}", state.cfg.backend_shadow_url, qw.path);
        let method = match Method::from_bytes(qw.method.as_bytes()) {
            Ok(m) => m,
            Err(_) => {
                tracing::error!(id = qw.id, method = %qw.method, "invalid method in queue, dropping");
                let _ = db::move_to_failed(&state.pool, qw.id);
                continue;
            }
        };
        let mut req = state.http.request(method, &url);
        if let Some(ct) = qw.content_type.as_deref() {
            req = req.header(http::header::CONTENT_TYPE, ct);
        }
        if !qw.body.is_empty() {
            req = req.body(qw.body.clone());
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if (200..300).contains(&status) {
                    if let Err(e) = db::mark_succeeded(&state.pool, qw.id, status) {
                        tracing::error!(error=%e, id=qw.id, "mark_succeeded failed");
                    }
                } else {
                    handle_attempt_failure(&state, qw.id, qw.attempts, format!("HTTP {status}"))
                        .await;
                }
            }
            Err(e) => {
                handle_attempt_failure(&state, qw.id, qw.attempts, format!("transport: {e}")).await;
            }
        }
    }
}

async fn handle_attempt_failure(state: &Arc<State>, id: i64, attempts_so_far: u32, err: String) {
    let next_attempts = attempts_so_far + 1;
    if next_attempts >= state.cfg.max_write_attempts {
        tracing::warn!(id, %err, "write retries exhausted, moving to failed_writes");
        let _ = db::move_to_failed(&state.pool, id);
        // K13: enter shadow-degraded mode.
        let _ = db::upsert_backend_state(&state.pool, "shadow", -1, "", "degraded");
        metrics::SHADOW_DEGRADED
            .with_label_values(&["shadow"])
            .set(1);
        // Fire-and-forget resync; if it succeeds, the poller will flip
        // backend_state.shadow back to 'live'.
        let st = state.clone();
        tokio::spawn(async move {
            if let Err(e) = trigger_shadow_resync(&st).await {
                tracing::error!(error=%e, "auto-resnapshot of shadow failed");
            }
        });
        return;
    }
    let backoff_idx = (next_attempts as usize)
        .saturating_sub(1)
        .min(RETRY_BACKOFFS_MS.len() - 1);
    let backoff = RETRY_BACKOFFS_MS[backoff_idx];
    let _ = db::mark_failed_attempt(&state.pool, id, &err, backoff);
    tracing::debug!(id, attempts = next_attempts, %err, backoff_ms = backoff, "scheduled retry");
}

#[derive(Deserialize, Debug)]
struct VersionResp {
    git_sha: String,
    #[serde(default)]
    process_uptime_seconds: i64,
}

pub async fn run_version_poller(state: Arc<State>) {
    let interval = Duration::from_secs(state.cfg.version_poll_interval_secs.max(1));
    tracing::info!(secs = interval.as_secs(), "version poller starting");
    loop {
        if let Err(e) = poll_once(&state).await {
            tracing::debug!(error=%e, "version poll iteration failed");
        }
        sleep(interval).await;
    }
}

async fn poll_once(state: &Arc<State>) -> anyhow::Result<()> {
    let primary = fetch_version(&state.http, &state.cfg.backend_primary_url).await;
    let shadow = fetch_version(&state.http, &state.cfg.backend_shadow_url).await;

    if let Ok(p) = &primary {
        let prior = db::get_backend_state(&state.pool, "primary")?;
        let regressed = prior
            .as_ref()
            .and_then(|x| x.last_observed_uptime_seconds)
            .map(|prev_uptime| p.process_uptime_seconds < prev_uptime)
            .unwrap_or(false);
        db::upsert_backend_state(
            &state.pool,
            "primary",
            p.process_uptime_seconds,
            &p.git_sha,
            "live",
        )?;
        if regressed {
            tracing::warn!("primary uptime regressed; running primary <- shadow resync");
            let st = state.clone();
            tokio::spawn(async move {
                if let Err(e) = resync_from_peer(&st, "shadow", "primary").await {
                    tracing::error!(error=%e, "primary resync failed");
                }
            });
        }
    }

    if let Ok(s) = &shadow {
        let prior = db::get_backend_state(&state.pool, "shadow")?;
        let regressed = prior
            .as_ref()
            .and_then(|x| x.last_observed_uptime_seconds)
            .map(|prev_uptime| s.process_uptime_seconds < prev_uptime)
            .unwrap_or(false);
        // If we previously marked shadow degraded but it's responding to
        // /version, we'll only flip back to 'live' once the resync flow
        // completes. So preserve degraded state here.
        let prior_state = prior
            .as_ref()
            .map(|x| x.state.clone())
            .unwrap_or_else(|| "live".into());
        let new_state = if prior_state == "degraded" {
            "degraded"
        } else {
            "live"
        };
        db::upsert_backend_state(
            &state.pool,
            "shadow",
            s.process_uptime_seconds,
            &s.git_sha,
            new_state,
        )?;
        if regressed {
            tracing::warn!("shadow uptime regressed; running shadow <- primary resync");
            let st = state.clone();
            tokio::spawn(async move {
                if let Err(e) = resync_from_peer(&st, "primary", "shadow").await {
                    tracing::error!(error=%e, "shadow resync failed");
                }
            });
        }
    }

    Ok(())
}

async fn fetch_version(client: &reqwest::Client, base: &str) -> anyhow::Result<VersionResp> {
    let url = format!("{base}/version");
    let resp = client
        .get(&url)
        .timeout(Duration::from_secs(5))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("status {}", resp.status());
    }
    let v: VersionResp = resp.json().await.context("decoding /version")?;
    Ok(v)
}

/// Snapshot the live `from` backend, then load it into `to`. This is the
/// fallback for K8/K12 since `/_admin/begin-resync` and `/_admin/mark-live`
/// are not yet implemented backend-side. Once those land, this becomes:
///   begin-resync(to) -> snapshot(from) -> load-snapshot(to) -> mark-live(to).
async fn resync_from_peer(state: &Arc<State>, from: &str, to: &str) -> anyhow::Result<()> {
    let (from_url, from_token) = backend_endpoint(state, from);
    let (to_url, to_token) = backend_endpoint(state, to);

    // 1. snapshot the live peer
    let snap_url = format!("{from_url}/_admin/snapshot");
    let mut headers = HeaderMap::new();
    if let Some(t) = from_token.as_deref() {
        headers.insert(
            HeaderName::from_static("x-admin-token"),
            HeaderValue::from_str(t)?,
        );
    }
    let resp = state
        .http
        .post(&snap_url)
        .headers(headers.clone())
        .timeout(Duration::from_secs(30))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("snapshot {} -> {}", snap_url, resp.status());
    }
    let snapshot_bytes = resp.bytes().await?;

    // 2. load it into the recovering backend
    let load_url = format!("{to_url}/_admin/load-snapshot");
    let mut headers = HeaderMap::new();
    if let Some(t) = to_token.as_deref() {
        headers.insert(
            HeaderName::from_static("x-admin-token"),
            HeaderValue::from_str(t)?,
        );
    }
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let resp = state
        .http
        .post(&load_url)
        .headers(headers)
        .body(snapshot_bytes.to_vec())
        .timeout(Duration::from_secs(30))
        .send()
        .await?;
    if !resp.status().is_success() {
        let s = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("load-snapshot {} -> {} {}", load_url, s, body);
    }

    // 3. flip backend_state for the recovered peer back to 'live'
    let _ = db::upsert_backend_state(&state.pool, to, 0, "", "live");
    metrics::SHADOW_DEGRADED.with_label_values(&[to]).set(0);
    tracing::info!(from = %from, to = %to, "resync complete");
    Ok(())
}

async fn trigger_shadow_resync(state: &Arc<State>) -> anyhow::Result<()> {
    resync_from_peer(state, "primary", "shadow").await
}

fn backend_endpoint(state: &Arc<State>, which: &str) -> (String, Option<String>) {
    match which {
        "primary" => (
            state.cfg.backend_primary_url.clone(),
            state.cfg.primary_admin_token.clone(),
        ),
        _ => (
            state.cfg.backend_shadow_url.clone(),
            state.cfg.shadow_admin_token.clone(),
        ),
    }
}
