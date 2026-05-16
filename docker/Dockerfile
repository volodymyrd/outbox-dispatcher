# syntax=docker/dockerfile:1

# ── Stage 1: install cargo-chef ───────────────────────────────────────────────
FROM rust:1.87-bookworm AS chef
RUN cargo install --locked cargo-chef --version 0.1.68
WORKDIR /build

# ── Stage 2: compute dependency recipe ───────────────────────────────────────
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo chef prepare --recipe-path recipe.json

# ── Stage 3: compile dependencies only ───────────────────────────────────────
FROM chef AS builder
ENV SQLX_OFFLINE=true
COPY --from=planner /build/recipe.json recipe.json
# This layer is cached as long as Cargo.toml / Cargo.lock are unchanged.
RUN cargo chef cook --release --recipe-path recipe.json

# Copy real sources and compile the binary.
COPY Cargo.toml Cargo.lock ./
COPY crates/        crates/
COPY migrations/    migrations/
COPY envs/          envs/
COPY .sqlx/         .sqlx/
RUN cargo build --release --bin outbox-dispatcher

# ── Stage 4: runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# Install TLS certificates (needed by reqwest's rustls backend to verify
# receiver TLS certificates) and wget for the healthcheck.
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
