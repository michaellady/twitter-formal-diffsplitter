# Trusted Computing Base (TCB) — `twitter-formal-diffsplitter`

This repo is **entirely** in the Tier-4 trust surface. Unlike the two backends
(`twitter-formal-rust`, `twitter-formal-go`), the diffsplitter has no verified
core to point at. It is a piece of trusted infrastructure whose job is to
establish *cross-implementation conformance evidence* for the two backends.

The narrower this list, the stronger the conformance claim. Every PR that
expands the trust surface MUST add a row here.

| File | Item | Why trusted | Validated by |
|---|---|---|---|
| `crates/diffsplitter/src/proxy.rs` | HTTP routing, request body buffering, primary/shadow fan-out | Live request mediator: incorrect routing breaks every client and corrupts the conformance signal | `cargo test`; smoke against live backends documented in DEPLOY.md |
| `crates/diffsplitter/src/db.rs` | SQLite schema + `enqueue_write`, `pop_one_ready`, `mark_succeeded`, `move_to_failed` | Loss of a queued write means the shadow silently misses a mutation and every later diff becomes a false positive | WAL + crash-recovery test; queue rows are durable across process restart |
| `crates/diffsplitter/src/diff.rs` | Comparator (status + JSON-aware body diff, severity classifier, `/version` field masks) | Decides whether primary≠shadow is reported as a divergence; a bad mask hides real bugs, an over-strict diff cries wolf | Unit tests in `diff.rs`; deliberate-divergence smoke from DEPLOY.md |
| `crates/diffsplitter/src/worker.rs` | Write-replay worker, version poller, K8/K12 resync orchestrator | Re-issues writes against the shadow with retry+backoff; on uptime regression drives the snapshot/load-snapshot dance | Crash-recovery: kill the proxy mid-replay, restart, observe queue drains |
| `crates/diffsplitter/src/metrics.rs` | Prometheus counters (`diff_total`, `queue_depth`, `failed_writes_total`, `requests_total`) | Operators trust these to alert on shadow divergence; wrong counters hide regressions | Stable label cardinality (no per-path / per-user labels) |
| `crates/diffsplitter/src/dashboard.rs` | `GET /diffs` (HTML), `GET /diffs/:id` (HTML drill-in), `GET /diffs.json` — server-rendered evidence dashboard with severity/since filters and K13 degraded-row styling | Read-only renderer over the existing `diffs` table; the only page humans use to triage divergences. A bug here can hide a real divergence (false negative) or misclassify a noise diff as critical (false positive). No auth; no `<script>`; HTML is escaped at render time. | `cargo test` (`tests/dashboard.rs`): index returns 200 with the expected columns, `/diffs/:nonexistent` returns 404, `/diffs.json` round-trips through SQLite, severity filter applied, bad input rejected with 400 |
| `crates/diffsplitter/src/alerting.rs` | `Notifier` trait + `LogNotifier` (always on), `WebhookNotifier` (generic POST — Slack/Discord/ntfy/custom), optional `GitHubPRCommentNotifier`; `NotifierSet::fire` concurrent fan-out; `db::claim_for_notification` atomic single-shot claim on `diffs.notified_at_ns` | Notification dispatch is trusted but **not formally verified** — the webhook payload schema is just a serde-derived JSON object. Failure modes (wrong severity threshold, malformed payload, double-fire across restart) are operational rather than safety-critical: missed alerts mean operators are paged late, but the diff record itself is independent and the proxy hot path never blocks on notifier IO. | `cargo test` (`tests/alerting.rs`): `WebhookNotifier` posts the documented JSON shape against an `httpmock` server; non-2xx propagates as an error; `NotifierSet::fire` swallows individual failures; `claim_for_notification` returns `true` exactly once per `diff_id`; end-to-end critical-row insertion + claim + fire path |
| `Dockerfile` | Builder + distroless final stage, image-digest provenance | Same K3 promotion pattern as the two backends; verify pushes a digest, deploy pulls by digest | `verify.yml` + `deploy.yml` workflows |
| `fly.toml` | App config + 1GB persistent volume mount at `/data` | The SQLite write queue must be durable across machine restarts; the volume is the durability boundary | Fly volume snapshots; crash-recovery smoke |
| `.github/workflows/{ci,verify,deploy}.yml` | CI gate, GHCR push, Fly deploy by digest | Image-digest provenance — only verified images can deploy | Workflow definitions reviewed in PR |
| Snapshot interop | The diffsplitter's resync flow assumes the byte-equivalent snapshot contract held by both backends (sorted users/follows/tweets, `snapshot_version` ∈ {N, N+1}) | If the two backends drift on the snapshot wire format, the resync flow corrupts state | Backend-side schema tests (in `twitter_formal_spec/.github/template-tier4-bootstrap`); diffsplitter never *parses* the snapshot, only forwards bytes |

## Trust surface categories (this repo)

- **HTTP routing + body buffering:** `crates/diffsplitter/src/proxy.rs`
- **Durable queue:** `crates/diffsplitter/src/db.rs` (rusqlite, WAL)
- **Diff comparator:** `crates/diffsplitter/src/diff.rs`
- **Resync orchestrator:** `crates/diffsplitter/src/worker.rs`
- **Observability:** `crates/diffsplitter/src/metrics.rs`
- **Evidence dashboard (read-only HTML):** `crates/diffsplitter/src/dashboard.rs`
- **Deploy stack:** `Dockerfile`, `fly.toml`, `.github/workflows/*`

## What is *not* in this repo's TCB

- The verified core of either backend (`twitter_rust_formal_verification`,
  `twitter_golang_formal_verification`) — those have their own `TCB.md`.
- The snapshot serializer / parser — owned by the backends; the diffsplitter
  forwards bytes verbatim.
- HMAC cookie minting — the primary mints, the diffsplitter passes cookies
  through unchanged. Browsers see the primary's cookies via the proxy.

## How rows get removed

A row is removed when the underlying responsibility is shifted out of this
repo (e.g., snapshot interop becomes a typed shared crate consumed by both
backends and this proxy stops handling raw bytes). The PR that removes the
row updates `CHANGELOG-tier4.md` under `Trust-Boundary`.
