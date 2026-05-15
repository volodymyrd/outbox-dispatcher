# syntax=docker/dockerfile:1

# ── Stage 1: build ────────────────────────────────────────────────────────────
FROM rust:1.87-bookworm AS builder

WORKDIR /build

# Accept build-time sqlx offline mode (no live DB needed during Docker build).
ENV SQLX_OFFLINE=true

# Cache dependency compilation separately from source compilation.
# Copy manifests first so the layer is reused when only source changes.
COPY Cargo.toml Cargo.lock ./
COPY crates/core/Cargo.toml           crates/core/Cargo.toml
COPY crates/http-callback/Cargo.toml  crates/http-callback/Cargo.toml
COPY crates/admin-api/Cargo.toml      crates/admin-api/Cargo.toml
COPY crates/bin/Cargo.toml            crates/bin/Cargo.toml

# Create stub lib/main files so Cargo can resolve the workspace and compile deps.
RUN mkdir -p crates/core/src \
             crates/http-callback/src \
             crates/admin-api/src \
             crates/bin/src && \
    echo "pub fn _stub() {}" > crates/core/src/lib.rs && \
    echo "pub fn _stub() {}" > crates/http-callback/src/lib.rs && \
    echo "pub fn _stub() {}" > crates/admin-api/src/lib.rs && \
    echo "fn main() {}"      > crates/bin/src/main.rs

RUN cargo fetch --locked && \
    cargo build --release --bin outbox-dispatcher || \
    (echo "warm-up build failed — check Cargo.toml / toolchain before relying on layer cache" && exit 1)

# Now copy the real source, sqlx offline cache, config, and migrations.
COPY crates/              crates/
COPY migrations/          migrations/
COPY envs/                envs/
COPY .sqlx/               .sqlx/

# Touch source files to force recompilation (stubs were built above).
RUN find crates -name "*.rs" | xargs touch

RUN cargo build --release --bin outbox-dispatcher

# ── Stage 2: runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# Install TLS certificates (needed by reqwest's rustls backend to verify
# receiver TLS certificates) and a minimal set of runtime libs.
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates wget && \
    rm -rf /var/lib/apt/lists/*

# Non-root user for least-privilege operation.
RUN useradd --uid 10001 --no-create-home --shell /sbin/nologin dispatcher

WORKDIR /app

COPY --from=builder /build/target/release/outbox-dispatcher /app/outbox-dispatcher
COPY --from=builder /build/envs/                             /app/envs/
COPY --from=builder /build/migrations/                       /app/migrations/

USER dispatcher

# Admin API
EXPOSE 9090
# Prometheus metrics
EXPOSE 9091

ENV APP_ENV=prod

HEALTHCHECK --interval=15s --timeout=5s --start-period=20s --retries=3 \
    CMD wget -qO- http://localhost:9090/health || exit 1

ENTRYPOINT ["/app/outbox-dispatcher"]
