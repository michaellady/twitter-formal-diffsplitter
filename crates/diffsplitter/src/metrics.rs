//! Prometheus metrics. Cardinality-bounded — labels are fixed string sets only.

use once_cell::sync::Lazy;
use prometheus::{
    register_int_counter, register_int_counter_vec, register_int_gauge, register_int_gauge_vec,
    Encoder, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, TextEncoder,
};

pub static DIFF_TOTAL: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "diffsplitter_diff_total",
        "Diffs observed between primary and shadow, by severity.",
        &["severity"]
    )
    .expect("register diff_total")
});

pub static QUEUE_DEPTH: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!(
        "diffsplitter_queue_depth",
        "Number of write_queue entries pending shadow replay."
    )
    .expect("register queue_depth")
});

pub static FAILED_WRITES_TOTAL: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!(
        "diffsplitter_failed_writes_total",
        "Cumulative writes that exhausted retries and moved to failed_writes."
    )
    .expect("register failed_writes_total")
});

pub static WRITE_REPLAY_ATTEMPTS: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "diffsplitter_write_replay_attempts_total",
        "Total HTTP attempts the write-replay worker has made against the shadow."
    )
    .expect("register write_replay_attempts_total")
});

pub static REQUESTS_TOTAL: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "diffsplitter_requests_total",
        "Requests proxied, labeled by class (write|read|admin|other).",
        &["class"]
    )
    .expect("register requests_total")
});

pub static SHADOW_DEGRADED: Lazy<IntGaugeVec> = Lazy::new(|| {
    register_int_gauge_vec!(
        "diffsplitter_shadow_degraded",
        "1 if the shadow is in degraded mode (read-sample diffs paused).",
        &["backend"]
    )
    .expect("register shadow_degraded")
});

pub fn register() {
    // Touch each Lazy so /metrics shows the families on first scrape even if
    // no traffic has happened yet.
    Lazy::force(&DIFF_TOTAL);
    Lazy::force(&QUEUE_DEPTH);
    Lazy::force(&FAILED_WRITES_TOTAL);
    Lazy::force(&WRITE_REPLAY_ATTEMPTS);
    Lazy::force(&REQUESTS_TOTAL);
    Lazy::force(&SHADOW_DEGRADED);
}

pub fn render() -> String {
    let metric_families = prometheus::gather();
    let mut buf = Vec::new();
    let encoder = TextEncoder::new();
    encoder.encode(&metric_families, &mut buf).ok();
    String::from_utf8(buf).unwrap_or_default()
}
