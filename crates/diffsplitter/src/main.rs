//! Diffsplitter binary entrypoint.
//!
//! Reads configuration from environment variables, opens the durable SQLite
//! queue, spins up the background workers (write replay + readiness/version
//! poll), and serves the axum proxy.

use std::sync::Arc;

use anyhow::Context;
use diffsplitter::{alerting, config::Config, db, metrics, proxy, worker};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Structured logs. RUST_LOG controls the filter; default to info.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .json()
        .flatten_event(true)
        .init();

    let cfg = Config::from_env().context("loading config from env")?;
    tracing::info!(
        primary = %cfg.backend_primary_url,
        shadow = %cfg.backend_shadow_url,
        sample_rate = cfg.shadow_sample_rate,
        sqlite_path = %cfg.sqlite_path.display(),
        port = cfg.port,
        "diffsplitter starting"
    );

    metrics::register();

    let pool = db::open(&cfg.sqlite_path).context("opening sqlite")?;
    db::migrate(&pool).context("running schema migrations")?;

    // Pre-build a single reqwest client; reused for all upstream calls so the
    // connection pool to fly.io is warm and we don't re-do TLS each request.
    let http = reqwest::Client::builder()
        .pool_max_idle_per_host(32)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .context("building reqwest client")?;

    // Phase 3: build the alerting fan-out from env vars. LogNotifier is
    // always present; webhook + GitHub PR comment are opt-in.
    let notifiers = Arc::new(alerting::NotifierSet::from_env());
    tracing::info!(
        notifier_count = notifiers.notifiers.len(),
        "alerting initialized"
    );

    let state = Arc::new(diffsplitter::State {
        cfg: cfg.clone(),
        http,
        pool: pool.clone(),
        notifiers,
    });

    // Background: drains write_queue and replays POSTs at the shadow.
    let worker_state = state.clone();
    tokio::spawn(async move { worker::run_write_replay(worker_state).await });

    // Background: polls /version on both backends every 5s; on uptime
    // regression triggers the K8/K12 resync flow (load-snapshot fallback
    // until /_admin/begin-resync is implemented backend-side).
    let poll_state = state.clone();
    tokio::spawn(async move { worker::run_version_poller(poll_state).await });

    let app = proxy::router(state.clone());

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], cfg.port));
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum serve")?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received");
}
