# Contributing — `twitter-formal-diffsplitter`

This repo is a Tier-4 trusted-infrastructure component. The two backends it
proxies (`twitter-formal-rust`, `twitter-formal-go`) carry the verification
weight; this repo's job is to *use* them in a way that produces real
cross-implementation conformance evidence.

## Tier-4 PR checklist (merge-blocking)

Every PR labeled or scoped as Tier-4 work MUST:

- [ ] Touch `CHANGELOG-tier4.md` — at least one new line under the appropriate section
- [ ] Touch `TCB.md` if the PR expands, shrinks, or modifies the trust surface (new admin endpoint, new trusted shim, discharged proof obligation, etc.) — `Trust-Boundary` section in the changelog as well
- [ ] Pass the `tier4-bootstrap-check` workflow (CI gate)

Skip rule: dependency-bump-only PRs (Renovate, Dependabot) may pass with the
changelog auto-appended via the bot, no `TCB.md` change required.

## Local dev

```bash
# requires Rust 1.95.0 (rustup will pick this up via rust-toolchain.toml)
cargo build --release
cargo test
```

## Smoke test against live backends

```bash
export BACKEND_PRIMARY_URL=https://twitter-formal-rust.fly.dev
export BACKEND_SHADOW_URL=https://twitter-formal-go.fly.dev
export RUST_PRIMARY_ADMIN_TOKEN=...   # from your local stash
export GO_SHADOW_ADMIN_TOKEN=...
export SHADOW_SAMPLE_RATE=1.0          # diff every read
export SQLITE_PATH=./data/diffsplitter.db
mkdir -p data
cargo run --release
```

Then in another shell:

```bash
# write through the proxy
curl -fsS -X POST -H 'content-type: application/json' \
  -d '{"handle":"alice","content":"hello via proxy"}' \
  http://localhost:8080/tweets

# verify both backends got it
curl -fsS 'https://twitter-formal-rust.fly.dev/timeline?for=alice' | jq .
curl -fsS 'https://twitter-formal-go.fly.dev/timeline?for=alice' | jq .

# inspect what the diffsplitter saw
curl -fsS http://localhost:8080/diffs.json | jq .
curl -fsS http://localhost:8080/metrics | grep diffsplitter
```

## Trust-boundary policy

If your PR adds a new dependency, new HTTP route, new SQLite table, new
admin endpoint, or anything that touches the comparator/resync flow, it
expands the trust surface. Add a row to `TCB.md` and note it in
`CHANGELOG-tier4.md` under `Trust-Boundary`.

If your PR discharges a piece of trust (e.g., extracts the comparator into
a property-tested standalone crate with shrinking inputs), remove the row.
