# syntax=docker/dockerfile:1.7
#
# Multi-stage Rust build for the diffsplitter. Mirrors the two backends'
# pattern: a Rust 1.95.0 builder produces a release binary AND writes
# /etc/version.json (since distroless final stage has no /bin/sh to run
# printf at container start time). Stage 2 ships only the binary +
# version.json on a distroless base.
#
# Image-digest provenance: verify.yml does a two-pass build — pass 1 is
# pushed to GHCR with IMAGE_DIGEST=sha256:pending, pass 2 re-bakes pass 1's
# digest into /etc/version.json. deploy.yml then `flyctl deploy --image
# ghcr.io/...@<digest>` pulls pass 2's image by digest. /version reports
# pass 1's digest (pass 2 cannot bake its own digest — chicken/egg).
# git_sha IS deterministic across both passes; deploy.yml verifies that.

FROM rust:1.95.0-bookworm AS builder
WORKDIR /build

# Copy manifests first to leverage Docker layer cache for cargo fetch.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

ARG GIT_SHA=dev
ARG IMAGE_DIGEST=sha256:dev

RUN cargo build --release -p diffsplitter
RUN printf '{"git_sha":"%s","image_digest":"%s"}\n' "$GIT_SHA" "$IMAGE_DIGEST" > /tmp/version.json

# rusqlite is built with the `bundled` feature (see Cargo.toml), so the
# final image does NOT need libsqlite3 installed.
FROM gcr.io/distroless/cc-debian12:nonroot
WORKDIR /app
COPY --from=builder /build/target/release/diffsplitter /app/diffsplitter
COPY --from=builder /tmp/version.json /etc/version.json

EXPOSE 8080
ENV PORT=8080 \
    RUST_LOG=info \
    SQLITE_PATH=/data/diffsplitter.db

ENTRYPOINT ["/app/diffsplitter"]
