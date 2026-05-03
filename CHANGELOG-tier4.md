# Tier-4 Changelog

Tracks Tier-4 changes (UI, deploy, shadow/diff-test, proof-discharge progress)
for this repo. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Every Tier-4 PR MUST append at least one line under the appropriate section.
The `tier4-bootstrap-check` CI gate enforces this.

## [Unreleased]

### Added

- Stream 2 Phase 2: server-rendered diffs dashboard. New module
  `crates/diffsplitter/src/dashboard.rs` mounts three routes on the existing
  axum router: `GET /diffs` (HTML — last 100 diffs sorted by
  `observed_at_ns DESC`, columns: timestamp/method/path/primary status/shadow
  status/severity/view), `GET /diffs/:id` (HTML drill-in with side-by-side
  primary vs shadow body and the structured diff blob), and `GET /diffs.json`
  (supersedes the placeholder Phase 1 handler — same data, JSON shape
  `{"diffs":[...]}`). All three accept `?severity=critical|high|medium|low|noise`
  and `?since=YYYY-MM-DD` (UTC) filters; bad filter input returns 400. Diffs
  whose `descended_from_failed_write_id` is non-null are visually greyed out
  with a "(degraded)" tag per K13 — they are caused by a known dropped write,
  not a real cross-impl divergence. Pure server-rendered HTML (`format!`
  strings + inline `<style>`); no JS framework, no external assets; typical
  page weight ≈ 6-15KB, well under the 50KB budget enforced by an integration
  test. No auth (single-developer demo posture, same as the backend UIs).
- Stream 2 Phase 1: initial diffsplitter axum proxy. Single binary in
  `crates/diffsplitter`. Routes writes (POST /users, POST /tweets,
  POST /follow, DELETE /follow) through the primary, returns the primary's
  response, and durably enqueues the same request to a SQLite write queue
  for the shadow. Samples reads (`SHADOW_SAMPLE_RATE`, default 0.1) and
  records primary↔shadow diffs in the `diffs` table.
- Background workers: write-replay (5-attempt retry with exponential backoff)
  and `/version` poller (every 5s, drives K8/K12 uptime-regression resync).
- K13 shadow-degraded mode: after retry exhaustion the comparator pauses
  read sampling on the shadow and triggers an auto-resnapshot from the
  primary.
- `GET /diffs.json` (raw JSON dashboard backend; UI is Phase 2).
- `GET /metrics` Prometheus exposition: `diffsplitter_diff_total`,
  `diffsplitter_queue_depth`, `diffsplitter_failed_writes_total`,
  `diffsplitter_requests_total`, `diffsplitter_write_replay_attempts_total`,
  `diffsplitter_shadow_degraded`.
- Fly.io deployment scaffolding: `Dockerfile` (Rust 1.95.0 builder, distroless
  final stage), `fly.toml` (app `twitter-formal-diffsplitter`, 1GB volume at
  `/data`), and `.github/workflows/{ci,verify,deploy}.yml` matching the
  backends' two-pass image-digest promotion pattern.

### Changed

### Deprecated

### Removed

### Fixed

### Trust-Boundary

- Initial TCB enumeration. Every file in this repo is in the trust surface
  by construction; see `TCB.md` for the row-by-row breakdown. Trust
  categories: HTTP routing & body buffering, durable SQLite queue, diff
  comparator, resync orchestrator, Prometheus metrics, deploy stack.

---

_For Tier 1–3 history (the verified core), see the two backend repos:_
_`twitter_rust_formal_verification` and `twitter_golang_formal_verification`._
