# twitter-formal-diffsplitter

Cross-implementation conformance proxy for the formally-verified Twitter clone:

- **Primary**: `twitter-formal-rust` — Verus + TLA⁺ verified, Rust 1.95.0
  ([repo](https://github.com/michaellady/twitter_rust_formal_verification))
- **Shadow**: `twitter-formal-go` — Gobra + TLA⁺ verified, Go
  ([repo](https://github.com/michaellady/twitter_golang_formal_verification))

The diffsplitter sits in front of both and produces *evidence* that two
independently-verified implementations of the same spec agree at runtime.

## What it does

1. **Proxy writes** — `POST /users`, `POST /tweets`, `POST /follow`,
   `DELETE /follow` go to the primary. The primary's response is returned
   to the client unchanged. The same request is durably enqueued in a
   SQLite write queue and replayed against the shadow by a background
   worker (5 attempts with exponential backoff; on exhaustion the entry
   moves to `failed_writes` and the shadow enters `degraded` mode per K13).
2. **Sample reads** — `GET /timeline`, `GET /version`, `GET /healthz`,
   `GET /u/:handle`, etc. go to the primary. With probability
   `SHADOW_SAMPLE_RATE` (default `0.1`) the same read is fired at the
   shadow asynchronously after the response is sent, the bodies are
   compared by the JSON-aware diff comparator, and any divergence is
   recorded in the `diffs` table.
3. **Resync** — A background poller hits `/version` on both backends every
   5s. On uptime regression (a restart), the diffsplitter snapshots the
   live peer (`POST /_admin/snapshot`) and loads it into the recovering
   peer (`POST /_admin/load-snapshot`). Resync from snapshot is the
   K8/K12 fallback until backend-side `/_admin/begin-resync` and
   `/_admin/mark-live` land.
4. **Expose evidence** — `GET /diffs.json` lists recent divergences.
   `GET /metrics` exposes Prometheus counters (`diffsplitter_diff_total`
   by severity, `diffsplitter_queue_depth`, `diffsplitter_failed_writes_total`,
   `diffsplitter_requests_total` by class, etc.).

## Why this matters

Each backend has its own end-to-end formal verification chain. Both target
the same TLA⁺ specification at the top. **The diffsplitter is the only
piece of evidence that the two specifications-as-implemented actually
behave the same way under live traffic.** When the diff counter is zero
across millions of read samples, the spec is faithful. When it is nonzero,
exactly one of the two implementations (or the spec itself) has a bug.

## Trust framing

The diffsplitter is **entirely in the Tier-4 trust surface**. There is no
verified core in this repo — see `TCB.md` for the row-by-row enumeration.
The verification weight lives in the two backend repos. This proxy's job
is to *use* them in a way that produces real conformance evidence.

## Deploy

See `DEPLOY.md`. Same image-digest provenance pipeline as the backends:
GitHub Actions runs `verify.yml` (test + two-pass build push to GHCR), then
`deploy.yml` does `flyctl deploy --image @sha256:<digest>` to the Fly app
`twitter-formal-diffsplitter`.

## License

MIT (matches the two backends).
