# Deploy

## First-time setup

```bash
# 1. Create the Fly app in the personal org.
flyctl apps create twitter-formal-diffsplitter --org personal

# 2. Create the SQLite volume (1GB, IAD region — single-machine app).
flyctl volumes create diffsplitter_data \
  --app twitter-formal-diffsplitter \
  --region iad \
  --size 1

# 3. Set admin tokens for the two backends. These must match what the
#    backends compare against (constant-time compared against ADMIN_TOKEN
#    env on each backend).
flyctl secrets set --app twitter-formal-diffsplitter \
  RUST_PRIMARY_ADMIN_TOKEN=<token> \
  GO_SHADOW_ADMIN_TOKEN=<token>

# 4. Create a deploy token and push it into the GitHub repo as the
#    FLY_API_TOKEN secret. Also set FLY_APP as a repo variable.
flyctl tokens create deploy --app twitter-formal-diffsplitter
gh secret set FLY_API_TOKEN --repo michaellady/twitter-formal-diffsplitter
gh variable set FLY_APP --body twitter-formal-diffsplitter \
  --repo michaellady/twitter-formal-diffsplitter
```

## Continuous deploy

Every push to `main` runs:

1. `verify.yml` — `cargo test`, then a two-pass image build pushed to
   `ghcr.io/michaellady/twitter-formal-diffsplitter:git-<sha>`. The image
   digest is uploaded as a workflow artifact.
2. `deploy.yml` (triggered by `verify` success on main) — downloads the
   digest artifact and runs `flyctl deploy --image @<digest>`. After
   deploy, polls `/version` for up to 60s and fails if `git_sha` doesn't
   match the deployed commit.

## Local smoke test against live backends

Required env (operators must supply admin tokens):

```bash
export BACKEND_PRIMARY_URL=https://twitter-formal-rust.fly.dev
export BACKEND_SHADOW_URL=https://twitter-formal-go.fly.dev
export RUST_PRIMARY_ADMIN_TOKEN=<from your stash>
export GO_SHADOW_ADMIN_TOKEN=<from your stash>
export SHADOW_SAMPLE_RATE=1.0
export SQLITE_PATH=$PWD/data/diffsplitter.db
mkdir -p data
cargo run --release
```

Then in another shell:

```bash
# 1. Write a tweet through the proxy
curl -fsS -X POST -H 'content-type: application/json' \
  -d '{"handle":"alice","content":"hello via proxy"}' \
  http://localhost:8080/tweets

# 2. Wait a moment for the write-replay worker to fire at the shadow
sleep 3

# 3. Both backends should now have the tweet
curl -fsS 'https://twitter-formal-rust.fly.dev/timeline?for=alice' | jq
curl -fsS 'https://twitter-formal-go.fly.dev/timeline?for=alice'   | jq

# 4. Inspect the diffsplitter
curl -fsS http://localhost:8080/diffs.json | jq .
curl -fsS http://localhost:8080/metrics    | grep diffsplitter
```

## Crash-recovery test

```bash
# 1. Start the diffsplitter
cargo run --release &
PID=$!

# 2. Fire a few writes
for i in {1..5}; do
  curl -fsS -X POST -H 'content-type: application/json' \
    -d "{\"handle\":\"alice\",\"content\":\"crash test $i\"}" \
    http://localhost:8080/tweets
done

# 3. Kill the proxy mid-flight (some writes may not have replayed yet)
kill -9 $PID

# 4. Restart and confirm the queue drains
cargo run --release &
sleep 5
curl -fsS http://localhost:8080/metrics | grep diffsplitter_queue_depth
# expect 0 once the worker drains
```

## Required Fly secrets summary

- `RUST_PRIMARY_ADMIN_TOKEN` — `X-Admin-Token` for primary's `/_admin/snapshot`
  and `/_admin/load-snapshot`. Must match the primary's `ADMIN_TOKEN` env.
- `GO_SHADOW_ADMIN_TOKEN` — same, for the Go shadow.

The diffsplitter does NOT require an admin token of its own — it has no
admin endpoints. Anyone with network access can query `/diffs.json` and
`/metrics`; that's intentional (operator visibility).
