# Tier-4 Changelog

Tracks Tier-4 changes (UI, deploy, shadow/diff-test, proof-discharge progress)
for this repo. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Every Tier-4 PR MUST append at least one line under the appropriate section.
The `tier4-bootstrap-check` CI gate enforces this.

## [Unreleased]

### Added

- Stream 2 Phase 3: critical-diff alerting. New module
  `crates/diffsplitter/src/alerting.rs` defines a `Notifier` trait with
  three impls: `LogNotifier` (always on; structured `tracing::warn!` to
  stderr — the no-op fallback so operators see critical diffs even with
  no external sink), `WebhookNotifier` (generic JSON POST — works with
  Slack incoming webhooks, Discord, ntfy.sh, custom bridges), and
  `GitHubPRCommentNotifier` (POSTs a markdown summary to the GitHub
  Issues comments API for a tracked PR). `NotifierSet::fire` fans out
  concurrently via `futures_util::join_all`; individual notifier failures
  are logged but never block the request path or the diff record itself.
  The proxy fires only on `severity == "critical"` after successfully
  claiming the row via `db::claim_for_notification`, an atomic
  single-statement `UPDATE diffs SET notified_at_ns = now WHERE id = ?
  AND notified_at_ns IS NULL` so two proxy instances or a
  crash-and-restart cannot double-fire. Configuration via env vars:
  `ALERT_WEBHOOK_URL` enables the webhook; `ALERT_GH_REPO` +
  `ALERT_GH_PR` + `ALERT_GH_TOKEN` (all three required) enable the PR
  commenter. Schema migration adds `diffs.notified_at_ns INTEGER`
  idempotently via a `PRAGMA table_info` sniff (no DROP, no rebuild).
  New runtime deps: `async-trait`, `futures-util` (no-default-features);
  `httpmock` as a dev-dep only. Six new tests in `tests/alerting.rs`
  cover the webhook JSON shape (verified against an in-process httpmock
  with `json_body_partial`), non-2xx error propagation, fan-out failure
  isolation, the single-shot claim contract, the end-to-end
  insert-then-fire path, and a payload-shape regression guard.
- Stream 2 Phase 4: cross-impl conformance scoreboard. New module
  `crates/diffsplitter/src/scoreboard.rs` mounts two routes on the existing
  axum router via the same `merge` helper used by the dashboard:
  `GET /scoreboard` (server-rendered HTML) and `GET /scoreboard.json`
  (same data, JSON shape). The page answers the topline question "are the
  two impls staying converged or diverging?" with: a counts panel (total
  requests, total diffs, divergence rate over 1h / 24h / 7d), an inline-SVG
  severity bar chart over the 24h window (no JS, no external assets), a
  top-10 diverging-paths leaderboard sorted by diff count, and a pointer
  to the last critical diff (with a link back to the dashboard). When the
  shadow is currently `degraded` (K13) the page renders a yellow banner
  and greys out the rate cards — the rate is meaningless while writes are
  being silently dropped. The "requests" denominator is best-effort: there
  is no per-request audit row (proxy only emits Prometheus counters, which
  reset on restart), so we approximate it as
  `diffs + queued-writes + failed-writes`. This undercounts non-diverging
  reads and slightly overstates the rate; the tradeoff is documented in
  `db::requests_in_window` and called out in the page footer. Conservative
  by design (overstates rather than hides). New helpers in `db.rs` keep
  the scoreboard out of the proxy hot path: `diffs_in_window`,
  `requests_in_window`, `diffs_total`, `severity_breakdown`,
  `top_diverging_paths`, `last_critical_diff`, `shadow_degraded_now`.
  Pure server-rendered HTML; typical render ≈ 4-10KB, well under the 50KB
  budget enforced by an integration test (`tests/scoreboard.rs`). Five
  integration tests cover: HTML renders 200 with all required sections,
  JSON round-trips through SQLite, rolling-window math buckets diffs into
  the right windows on a seeded DB, an empty DB returns sensible zeros,
  and the K13 degraded banner appears when shadow state is `degraded`.
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
