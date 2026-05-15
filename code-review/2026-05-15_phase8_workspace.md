# Code Review — Phase 8 (PR #7)

**Date:** 2026-05-15T17:07:47Z (initial), 2026-05-15T20:45:36Z (follow-up: findings 9–10)
**Branch:** phase8
**Reviewed by:** Claude (review command)
**Scope:** PR #7 — Dockerfile, GitHub Actions CI/CD, example configs, deployment/ops/protocol docs
**Files reviewed:**
- `.github/workflows/ci.yml`
- `.github/workflows/release.yml`
- `docker/Dockerfile`
- `docker/docker-compose.example.yml`
- `examples/config.minimal.toml`
- `examples/config.production.toml`
- `examples/docker-compose.with-postgres.yml`
- `docs/deployment.md`
- `docs/operations.md`
- `docs/webhook-protocol.md`
- `CLAUDE.md`

> No Rust source changed in this PR — most of the checklist sections (Async/Tokio, sqlx, Repo trait, HMAC, etc.) are inapplicable. Findings focus on Docker/CI/docs.

---

## Findings

### Finding 1 — Compose healthchecks use `wget`, which is not in the runtime image

| Field | Value |
|-------|-------|
| **File:Line** | `docker/docker-compose.example.yml:48`, `examples/docker-compose.with-postgres.yml:53` |
| **Severity** | Critical |
| **Category** | Correctness |

**Problem**

Both example compose files run `wget -qO- http://localhost:9090/health || exit 1` as the dispatcher healthcheck, but the runtime stage in `docker/Dockerfile` builds `FROM debian:bookworm-slim` and only installs `ca-certificates`. `wget` is **not** present in `bookworm-slim`. Every healthcheck will exit with `command not found`, so Docker will report the dispatcher unhealthy and `depends_on: condition: service_healthy` chains downstream will never see it up. Both example configurations are unusable as written.

**Context** (surrounding code as it exists today)

```yaml
# docker/docker-compose.example.yml lines 47-52
healthcheck:
  test: ["CMD-SHELL", "wget -qO- http://localhost:9090/health || exit 1"]
  interval: 10s
  timeout: 5s
  retries: 5
  start_period: 15s
```

```dockerfile
# docker/Dockerfile lines 47-49
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*
```

**Recommended fix**

Pick one of these two approaches and apply it consistently:

Option A — add a static healthcheck binary to the image (preferred, smallest):

```dockerfile
# docker/Dockerfile — replace the apt install line
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates wget && \
    rm -rf /var/lib/apt/lists/*
```

Option B — add a dispatcher subcommand `healthcheck` that does `GET /health` in-process, and use it:

```yaml
# docker/docker-compose.example.yml lines 47-52
healthcheck:
  test: ["CMD", "/app/outbox-dispatcher", "healthcheck"]
  interval: 10s
  timeout: 5s
  retries: 5
  start_period: 15s
```

**Why this fix**

Option A is one extra apt package (~600 KB) and keeps the compose file as-is. Option B is hermetic — no external binary required — but needs a small Rust addition. Either way, the current configuration ships broken.

---

### Finding 2 — Missing `.dockerignore`; build context includes `target/`, `.git/`, `.idea/`

| Field | Value |
|-------|-------|
| **File:Line** | `docker/Dockerfile` (repo root has no `.dockerignore`) |
| **Severity** | High |
| **Category** | Performance / Correctness |

**Problem**

There is no `.dockerignore` at the workspace root. `docker build` therefore packages and sends `target/` (hundreds of MB of Rust build artefacts), `.git/`, `.idea/`, and any locally-mounted `.env.toml` to the Docker daemon as build context. This dramatically slows local and CI image builds, bloats Buildx cache layers, and risks shipping `.env.toml` into the image if it ever ends up tracked or copied by `COPY . .`-style instructions (the current Dockerfile is careful, but a future change could regress).

**Context**

The Dockerfile explicitly copies only the manifests, then `crates/`, `migrations/`, `envs/`, and `.sqlx/` — so today nothing leaks. The issue is purely build-time waste and a footgun for future edits. No `.dockerignore` is checked in:

```
$ ls -la /Users/vova/work/workspace/volmyr/github/outbox-dispatcher/.dockerignore
ls: …/.dockerignore: No such file or directory
```

**Recommended fix**

Add `.dockerignore` at the workspace root:

```gitignore
# Build artefacts
target/
**/target/

# VCS / IDE / OS
.git/
.gitignore
.idea/
.vscode/
.DS_Store

# Local secrets — never bake into images
.env
.env.toml

# Docs and examples are not needed inside the image
docs/
examples/
code-review/
GEMINI.md
README.md
CLAUDE.md

# Test fixtures from testcontainers / cargo
**/*.log
```

**Why this fix**

Mirrors the `.gitignore` for non-source-of-truth files plus excludes anything the runtime image does not need. Shrinks the build context from ~hundreds-of-MB to single-digit MB, and removes the risk of any future `COPY . .` accidentally including secrets.

---

### Finding 3 — Dockerfile dependency-warmup layer swallows real errors with `|| true`

| Field | Value |
|-------|-------|
| **File:Line** | `docker/Dockerfile:29` |
| **Severity** | High |
| **Category** | Correctness |

**Problem**

The dependency-only build step uses `2>/dev/null || true`. If the workspace fails to resolve (e.g. a broken `Cargo.toml`, a missing dependency, an incompatible toolchain), the failure is silently discarded. The subsequent real build then succeeds-or-fails on its own, but you lose all error output from the warmup attempt and lose the entire point of separating dependency compilation from source compilation: when the cache is missing, both steps now compile everything from scratch with no signal that the warmup didn't actually warm anything. It also masks misconfigurations of the stub `main.rs` that should otherwise be caught immediately.

**Context** (surrounding code as it exists today)

```dockerfile
# docker/Dockerfile lines 19-29
RUN mkdir -p crates/core/src \
             crates/http-callback/src \
             crates/admin-api/src \
             crates/bin/src && \
    echo "pub fn _stub() {}" > crates/core/src/lib.rs && \
    echo "pub fn _stub() {}" > crates/http-callback/src/lib.rs && \
    echo "pub fn _stub() {}" > crates/admin-api/src/lib.rs && \
    echo "fn main() {}"      > crates/bin/src/main.rs

RUN cargo build --release --bin outbox-dispatcher 2>/dev/null || true
```

**Recommended fix**

Compile only the dependency graph (no error-suppression):

```dockerfile
# docker/Dockerfile — replace line 29
# Warm the dep graph. The stubs declare no extern crates, so we ask cargo to
# fetch and compile dependencies only (not the workspace binary itself).
RUN cargo fetch --locked && \
    cargo build --release --workspace --all-targets --offline || \
    (echo "warm-up build failed — investigate before relying on layer cache" && exit 1)
```

Or, more reliably, use `cargo chef` (a small first-stage that emits a dependency-only recipe), which is the idiomatic way to cache Rust workspace dependencies in Docker.

**Why this fix**

Silent `|| true` defeats the layer-cache optimisation and hides real problems. If the warmup genuinely cannot work for some workspace shape, the build should fail loudly so the operator can fix the Dockerfile, not paper over it.

---

### Finding 4 — Release workflow does not gate on CI

| Field | Value |
|-------|-------|
| **File:Line** | `.github/workflows/release.yml:1-19` |
| **Severity** | Medium |
| **Category** | Correctness |

**Problem**

`release.yml` runs on `push tags v*.*.*`. There is no dependency on the `CI` workflow having passed on the underlying commit. A maintainer can therefore tag a commit whose `cargo clippy -- -D warnings`, unit tests, or integration tests are red, and the workflow will happily publish broken binaries and a Docker image to GHCR.

**Context**

```yaml
# .github/workflows/release.yml lines 1-7
name: Release

on:
  push:
    tags:
      - "v[0-9]+.[0-9]+.[0-9]+"
```

**Recommended fix**

Add a CI gate that runs the same checks before any artefact is built:

```yaml
# .github/workflows/release.yml — insert a job that runs first
jobs:
  ci:
    name: Verify CI on tagged commit
    uses: ./.github/workflows/ci.yml

  build-binaries:
    name: Build ${{ matrix.target }}
    needs: ci
    runs-on: ${{ matrix.os }}
    # … existing matrix …

  docker:
    name: Docker Image
    needs: ci
    runs-on: ubuntu-latest
    # … existing steps …
```

To use `workflow_call`, also add this to `ci.yml`:

```yaml
# .github/workflows/ci.yml — replace the existing `on:` block
on:
  push:
    branches: [main]
  pull_request:
    branches: [main]
  workflow_call:
```

**Why this fix**

Tags are cheap to create but releases are visible to users. Forcing the same checks the PR ran prevents accidental regressions from shipping.

---

### Finding 5 — `cross` is installed from an unpinned git ref

| Field | Value |
|-------|-------|
| **File:Line** | `.github/workflows/release.yml:55` |
| **Severity** | Medium |
| **Category** | Correctness |

**Problem**

```yaml
- name: Install cross (for cross-compilation)
  if: matrix.cross
  run: cargo install cross --git https://github.com/cross-rs/cross
```

Installing from `main` is non-reproducible: a future cross release with a breaking change (Docker base image swap, env var rename) silently breaks the next release build, and the prior release's binaries cannot be rebuilt bit-for-bit.

**Context**

`release.yml:53-55` (shown above).

**Recommended fix**

Pin to a known-good tag and use a prebuilt binary installer so the step is fast and deterministic:

```yaml
- name: Install cross (for cross-compilation)
  if: matrix.cross
  uses: taiki-e/install-action@v2
  with:
    tool: cross@0.2.5
```

**Why this fix**

`taiki-e/install-action` fetches a prebuilt cross binary in seconds instead of compiling from source (~minutes), and the pinned version means tomorrow's release builds the same way today's did.

---

### Finding 6 — Cloud Run docs suggest `PORT=8080`, but the dispatcher does not read `PORT`

| Field | Value |
|-------|-------|
| **File:Line** | `docs/deployment.md:149` |
| **Severity** | Medium |
| **Category** | Config / Documentation |

**Problem**

The Cloud Run section ends with:

> Set `PORT=8080` or adjust `admin.bind` to match what Cloud Run expects.

The dispatcher loads its admin listener from `admin.bind` (`crates/core/src/config.rs:425-445` plus `crates/bin/src/main.rs:247`). It does **not** consult a `PORT` env var. A Cloud Run deployer following the doc literally — exporting `PORT=8080` — will get a service that ignores the variable and binds to whatever `admin.bind` says (default `0.0.0.0:9090`), causing Cloud Run's health probe on `:8080` to fail and the revision to be marked unhealthy.

**Context**

```markdown
<!-- docs/deployment.md lines 137-149 -->
[admin]
bind = "0.0.0.0:8080"      # Cloud Run routes traffic to $PORT (default 8080)
```

```

Set `PORT=8080` or adjust `admin.bind` to match what Cloud Run expects.
```

**Recommended fix**

Drop the `PORT=` claim and document only the supported lever, or wire a `PORT` override into config loading. Doc-only fix:

```markdown
Cloud Run injects a `$PORT` env var (default `8080`). `outbox-dispatcher` does
not read `$PORT` directly — set `admin.bind = "0.0.0.0:8080"` (or pass
`APP__ADMIN__BIND=0.0.0.0:8080`) so the admin listener matches.
```

Code-only fix (if you want the doc claim to be true): in `AppConfig::load` add

```rust
.set_override_option(
    "admin.bind",
    std::env::var("PORT").ok().filter(|s| !s.trim().is_empty())
        .map(|p| format!("0.0.0.0:{p}")),
)?
```

**Why this fix**

Either the doc matches the code or the code matches the doc. As-is, the most prominent Cloud Run guidance is wrong.

---

### Finding 7 — Compose comment references `examples/config.production.yaml`, but the file is `.toml`

| Field | Value |
|-------|-------|
| **File:Line** | `docker/docker-compose.example.yml:44` |
| **Severity** | Low |
| **Category** | Documentation |

**Problem**

The mount comment names the wrong filename, sending users to a path that does not exist:

```yaml
# Mount your production config (see examples/config.production.yaml).
```

The actual file is `examples/config.production.toml`. The dispatcher only consumes TOML config — there is no YAML loader.

**Context**

```yaml
# docker/docker-compose.example.yml lines 43-46
volumes:
  # Mount your production config (see examples/config.production.yaml).
  # The base envs/ baked into the image are used as fallback defaults.
  - ./config.prod.toml:/app/envs/app_config_prod.toml:ro
```

**Recommended fix**

```yaml
volumes:
  # Mount your production config (see examples/config.production.toml).
  # The base envs/ baked into the image are used as fallback defaults.
  - ./config.prod.toml:/app/envs/app_config_prod.toml:ro
```

**Why this fix**

A user copying the file name out of the comment lands on a 404. One-character fix.

---

### Finding 8 — Dockerfile declares no `HEALTHCHECK`

| Field | Value |
|-------|-------|
| **File:Line** | `docker/Dockerfile:42-69` |
| **Severity** | Low |
| **Category** | Config |

**Problem**

The runtime stage exposes `9090` (admin) but does not include a `HEALTHCHECK` instruction. Users who run the image with `docker run` (no compose, no Kubernetes) get no liveness signal — the image always reports `running`, never `unhealthy`, even when the dispatcher is wedged. This pushes the responsibility entirely onto the orchestrator.

**Context**

```dockerfile
# docker/Dockerfile lines 60-69
USER dispatcher

# Admin API
EXPOSE 9090
# Prometheus metrics
EXPOSE 9091

ENV APP_ENV=prod

ENTRYPOINT ["/app/outbox-dispatcher"]
```

**Recommended fix**

Once Finding 1 is resolved (so the test command actually exists in the image):

```dockerfile
HEALTHCHECK --interval=15s --timeout=5s --start-period=20s --retries=3 \
    CMD wget -qO- http://localhost:9090/health || exit 1
```

Or, with a built-in subcommand:

```dockerfile
HEALTHCHECK --interval=15s --timeout=5s --start-period=20s --retries=3 \
    CMD ["/app/outbox-dispatcher", "healthcheck"]
```

**Why this fix**

A baseline image-level healthcheck means `docker ps` / orchestrator-agnostic deployments get accurate status without each consumer having to re-declare one.

---

### Finding 9 — Operations runbook lists Prometheus metrics that do not exist in the code

| Field | Value |
|-------|-------|
| **File:Line** | `docs/operations.md:138-153`, `docs/operations.md:160`, `docs/operations.md:247` |
| **Severity** | High |
| **Category** | Documentation |

**Problem**

The "Key metrics" table in `docs/operations.md` (and the alerting/retention sections that reference it) names many metrics that **don't exist** in the actual exporter (`crates/core/src/metrics.rs`). Dashboards, alerts, and queries built from this guide will silently return empty series. The drift includes both wrong metric names and wrong / missing labels:

| Doc says (operations.md) | Actual (metrics.rs) |
|---|---|
| `outbox_dispatched_total{callback,mode}` | Does not exist. Use `outbox_deliveries_total{callback,mode,result="ok"}` |
| `outbox_dispatch_failures_total{callback,reason}` | Does not exist. Use `outbox_deliveries_total{result="transient"\|"timeout"\|"invalid"\|"external_reset"}` |
| `outbox_dead_lettered_total{callback,reason}` | `outbox_dead_letters_total{callback}` (no `reason` label) |
| `outbox_dispatch_duration_seconds{callback}` | `outbox_dispatch_duration_seconds{callback,mode}` (missing `mode` in doc) |
| `outbox_lag_seconds{callback}` | `outbox_lag_seconds` (no labels) |
| `outbox_pending_deliveries{callback}` | `outbox_pending_deliveries{callback,mode}` (missing `mode` in doc) |
| `outbox_signing_key_resolution_failures_total{key_id,callback}` | `…{signing_key_id,callback}` (label name is `signing_key_id`, not `key_id`) |
| `outbox_scheduler_cycles_total` | Does not exist. Closest signals: `outbox_cycle_duration_seconds` (histogram → `_count` is the counter) and `outbox_listener_connection_status` (gauge, 1/0) |
| `outbox_retention_deleted_total{reason}` (also referenced in §Retention) | `outbox_retention_deletions_total{reason}` |
| `outbox_oldest_terminal_event_age_seconds` (§Retention) | `outbox_retention_oldest_event_age_seconds` |

The alerting recommendation `No outbox_scheduler_cycles_total increase for 2× poll_interval` is therefore unimplementable as written.

**Context** (surrounding code as it exists today)

```rust
// crates/core/src/metrics.rs lines 34-53 — authoritative metric names
pub const EVENTS_TOTAL: &str = "outbox_events_total";
pub const DELIVERIES_TOTAL: &str = "outbox_deliveries_total";
pub const DISPATCH_DURATION_SECONDS: &str = "outbox_dispatch_duration_seconds";
pub const LAG_SECONDS: &str = "outbox_lag_seconds";
pub const PENDING_DELIVERIES: &str = "outbox_pending_deliveries";
pub const EXTERNAL_PENDING_DELIVERIES: &str = "outbox_external_pending_deliveries";
pub const EXTERNAL_PENDING_SECONDS: &str = "outbox_external_pending_seconds";
pub const DEAD_LETTERS_TOTAL: &str = "outbox_dead_letters_total";
pub const EXTERNAL_TIMEOUT_RESETS_TOTAL: &str = "outbox_external_timeout_resets_total";
pub const COMPLETION_CYCLES_EXHAUSTED_TOTAL: &str = "outbox_completion_cycles_exhausted_total";
pub const SIGNING_KEY_RESOLUTION_FAILURES_TOTAL: &str =
    "outbox_signing_key_resolution_failures_total";
pub const INVALID_CALLBACKS_TOTAL: &str = "outbox_invalid_callbacks_total";
pub const PAYLOAD_SIZE_REJECTIONS_TOTAL: &str = "outbox_payload_size_rejections_total";
pub const RETENTION_DELETIONS_TOTAL: &str = "outbox_retention_deletions_total";
pub const RETENTION_CYCLE_DURATION_SECONDS: &str = "outbox_retention_cycle_duration_seconds";
pub const RETENTION_OLDEST_EVENT_AGE_SECONDS: &str = "outbox_retention_oldest_event_age_seconds";
pub const CORRUPTED_ROWS_TOTAL: &str = "outbox_corrupted_rows_total";
pub const CYCLE_DURATION_SECONDS: &str = "outbox_cycle_duration_seconds";
pub const LISTENER_CONNECTION_STATUS: &str = "outbox_listener_connection_status";
```

**Recommended fix**

Rewrite the §Key metrics table in `docs/operations.md` to mirror the constants and labels declared in `crates/core/src/metrics.rs`. A correct replacement is:

```markdown
| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `outbox_events_total` | Counter | `kind` | Events observed by the scheduler |
| `outbox_deliveries_total` | Counter | `callback`, `mode`, `result` | All delivery outcomes (`result` ∈ `ok`/`transient`/`timeout`/`invalid`/`external_reset`) |
| `outbox_dispatch_duration_seconds` | Histogram | `callback`, `mode` | HTTP round-trip duration |
| `outbox_lag_seconds` | Gauge | — | Age of the oldest pending delivery |
| `outbox_pending_deliveries` | Gauge | `callback`, `mode` | Current pending delivery count |
| `outbox_external_pending_deliveries` | Gauge | `callback` | External deliveries awaiting completion |
| `outbox_external_pending_seconds` | Histogram | `callback` | Age distribution of external-pending rows |
| `outbox_dead_letters_total` | Counter | `callback` | Deliveries reaching dead-letter state |
| `outbox_external_timeout_resets_total` | Counter | `callback` | External completions that timed out and were reset |
| `outbox_completion_cycles_exhausted_total` | Counter | `callback` | External deliveries dead-lettered after max cycles |
| `outbox_signing_key_resolution_failures_total` | Counter | `signing_key_id`, `callback` | Unknown signing key at dispatch time |
| `outbox_invalid_callbacks_total` | Counter | `reason` | Schedule-time invalid callbacks |
| `outbox_payload_size_rejections_total` | Counter | `kind` | Events rejected for exceeding `payload_size_limit_bytes` |
| `outbox_retention_deletions_total` | Counter | `reason` | Rows deleted by the retention worker (`reason` ∈ `processed`/`dead_letter`) |
| `outbox_retention_cycle_duration_seconds` | Histogram | — | Retention worker cycle duration |
| `outbox_retention_oldest_event_age_seconds` | Gauge | — | Age of oldest terminal event still on disk (NaN when none) |
| `outbox_corrupted_rows_total` | Counter | `stage` | Poison-pill rows skipped (`stage` ∈ `schedule`/`dispatch`/`sweep`/`retention`) |
| `outbox_cycle_duration_seconds` | Histogram | — | Scheduler loop duration (use `_count` as a liveness counter) |
| `outbox_listener_connection_status` | Gauge | — | LISTEN connection up (1) or down (0) |
```

And in the alerting list, replace the scheduler-cycle alert with one based on the listener gauge:

```markdown
| Listener down | `outbox_listener_connection_status == 0 for 2m` | Critical |
| Scheduler stalled | `rate(outbox_cycle_duration_seconds_count[2 * poll_interval]) == 0` | Critical |
```

And in §Retention:

```markdown
Monitor via `outbox_retention_deletions_total{reason}` and `outbox_retention_oldest_event_age_seconds`.
```

**Why this fix**

Every alert and dashboard built from a runbook needs the metric names to match the exporter byte-for-byte. The current drift turns the entire alerting section into silently-failing config — worse than having no alerts at all, because it implies coverage that does not exist.

---

### Finding 10 — `README.md` is a two-line stub with no project overview

| Field | Value |
|-------|-------|
| **File:Line** | `README.md:1-3` |
| **Severity** | High |
| **Category** | Documentation |

**Problem**

The repository root `README.md` currently contains only:

```markdown
# outbox-dispatcher
Outbox Dispatcher
```

This is the first page anyone (users, contributors, GHCR consumers, CI scanners, anyone landing from a search result) sees. For an open-source project shipping in Phase 8 with a Docker image, a release workflow, deployment/operations/protocol guides, and a Cloud Run story, the README needs to actually introduce the service, show how to run it, and link out to the deeper docs. The TDD (`../outbox/TDDs/04-outbox-dispatcher-tdd.md`) already contains every fact required — none of this needs to be invented, just summarised.

**Context** (surrounding code as it exists today)

```markdown
<!-- README.md — entire current contents -->
# outbox-dispatcher
Outbox Dispatcher
```

**Recommended fix**

Replace `README.md` with a structured, open-source-idiomatic README. Distill from the TDD and the existing `docs/deployment.md` / `docs/operations.md` / `docs/webhook-protocol.md`. Suggested skeleton (adapt content; do not invent claims that the code does not back):

```markdown
# outbox-dispatcher

[![CI](https://github.com/volodymyrd/outbox-dispatcher/actions/workflows/ci.yml/badge.svg)](https://github.com/volodymyrd/outbox-dispatcher/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/volodymyrd/outbox-dispatcher)](https://github.com/volodymyrd/outbox-dispatcher/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
<!-- Add badges only for things that actually exist. Drop the License badge until LICENSE is added. -->

A standalone Rust microservice that turns a Postgres outbox table into a reliable,
at-least-once HTTP webhook delivery service.

`outbox-dispatcher` watches `outbox_events` via `LISTEN/NOTIFY`, schedules one
`outbox_deliveries` row per callback, signs each request with HMAC-SHA256, and
delivers it with bounded retries and jittered exponential backoff. Multiple
replicas can run safely against the same database — row-level `locked_until`
prevents duplicate dispatch.

## Why

Implementing the transactional outbox pattern correctly is fiddly: you need a
durable cursor, idempotent retries, signed webhooks, dead-letter handling,
SSRF guards, and observability. `outbox-dispatcher` packages all of that as a
single binary so publishing services only need to `INSERT` into one table.

## Features

- **Reliable delivery** — at-least-once webhooks with bounded retries and
  ±25 % jittered exponential backoff.
- **Multi-replica safe** — row-level `locked_until` and `FOR UPDATE SKIP LOCKED`
  prevent duplicate dispatch across replicas.
- **Two completion modes** — `managed` (dispatcher marks the row done on 2xx)
  and `external` (receiver writes `processed_at` asynchronously).
- **HMAC-SHA256 signing** — keys rotated via config + env-var indirection;
  resolved at dispatch time, not schedule time, so deploys can be skewed.
- **SSRF guard** — denylist of private CIDRs; opt-out for trusted internal
  networks via `allow_private_ip_targets`.
- **Admin HTTP API** — bearer-token-protected endpoints for stats,
  dead-letter inspection, retry / complete / abandon, and event detail.
- **Prometheus metrics + OpenTelemetry traces** — first-class observability
  out of the box.
- **Retention worker** — optional auto-pruning of terminal events.

## Architecture at a glance

```
┌──────────────┐   INSERT    ┌────────────────┐   pg_notify   ┌────────────────┐
│  Publisher   │ ──────────► │ outbox_events  │ ─────────────►│   Scheduler    │
└──────────────┘             └────────────────┘               └───────┬────────┘
                                                                      │ INSERT
                                                                      ▼
                                                              ┌────────────────┐
                                                              │outbox_deliveries│
                                                              └───────┬────────┘
                                                                      │ FOR UPDATE
                                                                      │ SKIP LOCKED
                                                                      ▼
                                                              ┌────────────────┐
                                                              │   Dispatcher   │── HTTPS ─► Receiver
                                                              └────────────────┘  (signed)
```

See `../outbox/TDDs/04-outbox-dispatcher-tdd.md` for the full design.

## Quick start

```bash
# 1. Start Postgres + dispatcher with a single compose command
export ADMIN_TOKEN="$(openssl rand -hex 32)"
docker compose -f examples/docker-compose.with-postgres.yml up -d

# 2. Verify
curl http://localhost:9090/ready
```

To run from source:

```bash
docker compose up -d    # local Postgres on :5434
cargo run -- migrate    # apply schema
cargo run               # start the dispatcher
```

## Configuration

Config is layered (later wins):

1. `envs/app_config.toml` — baked-in defaults
2. `envs/app_config_${APP_ENV}.toml` — per-env overrides
3. `.env.toml` — local secrets (gitignored)
4. `APP__*` env vars — highest priority

Required at runtime:

| Setting | How to set |
|---------|-----------|
| `database.url` | `DATABASE_URL` env var |
| `admin.auth_token` | `ADMIN_TOKEN` env var |
| Each `[signing_keys]` entry | One env var per key |

Two example configs ship with the project:

- `examples/config.minimal.toml` — dev / low-traffic, no signing.
- `examples/config.production.toml` — fully populated production template.

## Webhook protocol

The dispatcher POSTs JSON to each callback URL with these reserved headers:

```
X-Outbox-Event-Id:        <uuid>
X-Outbox-Delivery-Id:     <i64>
X-Outbox-Callback-Name:   <string>
X-Outbox-Kind:            <event kind>
X-Outbox-Mode:            managed | external
X-Outbox-Attempt:         <1-based integer>
X-Outbox-Signing-Key-Id:  <id>                 (omitted when unsigned)
X-Outbox-Signature:       t=<unix_ts>,v1=<hex> (omitted when unsigned)
```

The signature is `HMAC-SHA256(secret, "{ts}.{raw_body}")`. Verify with a
constant-time comparison; tolerate `|now - ts| ≤ 300s` by default.

Full protocol and edge cases: [`docs/webhook-protocol.md`](docs/webhook-protocol.md).

## Operations

- **Liveness:** `GET /health` (no auth)
- **Readiness:** `GET /ready` (no auth) — checks DB, LISTEN, scheduler heartbeat
- **Admin API:** `/v1/stats`, `/v1/dead-letters`, `/v1/external-pending`,
  `/v1/events/<id>`, `POST /v1/deliveries/<id>/{retry,complete,abandon}`
- **Metrics:** Prometheus scrape on `:9091/metrics` (no auth)

See [`docs/operations.md`](docs/operations.md) for the runbook and
[`docs/deployment.md`](docs/deployment.md) for Docker / Cloud Run / Kubernetes
deployment recipes.

## Documentation

| Doc | Audience |
|-----|----------|
| [`docs/deployment.md`](docs/deployment.md) | Operators deploying the service |
| [`docs/operations.md`](docs/operations.md) | Day-2 runbook, metrics, common issues |
| [`docs/webhook-protocol.md`](docs/webhook-protocol.md) | Webhook receiver implementers |
| [`CLAUDE.md`](CLAUDE.md) | Project conventions and key design notes |
| `../outbox/TDDs/04-outbox-dispatcher-tdd.md` | Full design (out-of-tree) |

## Development

Required tooling: stable Rust (1.87+), Docker.

```bash
cargo fmt --all
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`.sqlx/` is a checked-in query cache so builds work without a live database.
After changing any sqlx query macro:

```bash
DATABASE_URL=postgres://outbox:outbox@localhost:5434/outbox_dispatcher \
  cargo sqlx prepare --workspace
```

Integration tests use `testcontainers` and spin up an ephemeral Postgres per
test — Docker must be running locally.

## Roadmap

The implementation phases tracked in [`CLAUDE.md`](CLAUDE.md) are all complete
(phases 1–8). Future work (v2+):

- Per-callback rate limiting
- Pluggable signing schemes (ed25519, JWT)
- Schema v2 migration tooling

## Contributing

Issues and pull requests are welcome. Please run the mandatory pre-commit
checks (above) before opening a PR. For substantive design changes, open an
issue first to discuss the approach.

## License

<!-- Add a LICENSE file (MIT or Apache-2.0 are conventional for Rust) and
     reference it here. The current repo has no LICENSE; that should be
     addressed before promoting the README. -->
```

The exact wording can vary, but the README must at minimum cover: a one-paragraph elevator pitch, a feature list, a quick-start, a configuration summary, the webhook contract, a link map into `docs/`, dev-tooling commands, and a license note. Drop any badge whose target does not exist.

**Why this fix**

A README is the cover of the project. Open-source consumers form a first impression of trustworthiness in seconds based on whether the README looks maintained. The current two-line stub signals "abandoned / WIP" even though the project is feature-complete through Phase 8 — that mismatch costs adoption. Every fact needed for the new README already exists in the TDD and the in-tree `docs/`; this is purely a packaging change.

---

### Finding 11 — Release workflow produces duplicate artifact name `outbox-dispatcher-linux-x86_64`

| Field | Value |
|-------|-------|
| **File:Line** | `.github/workflows/ci.yml:96-99`, `.github/workflows/release.yml:30-34, 78-82` |
| **Severity** | Medium |
| **Category** | Correctness |

**Problem**

`release.yml` calls `ci.yml` via `workflow_call` (the gate added for Finding 4). `ci.yml`'s `build` job uploads an artifact named **`outbox-dispatcher-linux-x86_64`**. The `build-binaries` matrix in `release.yml` *also* uploads an artifact named **`outbox-dispatcher-linux-x86_64`** (the first matrix entry). With `actions/upload-artifact@v4`, two uploads with the same name in the same workflow run fail the second one with HTTP 409 (`an artifact with this name already exists on the workflow run`). Every tag push will therefore fail in the `build-binaries (x86_64-unknown-linux-gnu)` job, blocking releases entirely.

**Context** (surrounding code as it exists today)

```yaml
# .github/workflows/ci.yml lines 94-99 — runs whenever ci.yml is invoked,
# including via workflow_call from release.yml
- name: Upload binary artifact
  uses: actions/upload-artifact@v4
  with:
    name: outbox-dispatcher-linux-x86_64
    path: target/release/outbox-dispatcher
    retention-days: 7
```

```yaml
# .github/workflows/release.yml lines 30-34 (matrix entry) and 78-82 (upload step)
- os: ubuntu-latest
  target: x86_64-unknown-linux-gnu
  asset_name: outbox-dispatcher-linux-x86_64

# …

- name: Upload binary artifact
  uses: actions/upload-artifact@v4
  with:
    name: ${{ matrix.asset_name }}    # == outbox-dispatcher-linux-x86_64
    path: ${{ matrix.asset_name }}
```

**Recommended fix**

Skip the CI workflow's `build` job when invoked via `workflow_call` — it is redundant with `release.yml`'s `build-binaries`, which produces the artefact the release actually consumes. Either:

Option A — gate the `build` job in `ci.yml` so it only runs on push/PR, not via `workflow_call`:

```yaml
# .github/workflows/ci.yml — adjust the `build` job
  build:
    name: Build Release Binary
    if: github.event_name != 'workflow_call'
    runs-on: ubuntu-latest
    needs: [test-unit, test-integration]
    # … existing steps …
```

Option B — rename the CI artifact so it doesn't collide with the release matrix entry:

```yaml
# .github/workflows/ci.yml lines 96-99
- name: Upload binary artifact
  uses: actions/upload-artifact@v4
  with:
    name: outbox-dispatcher-linux-x86_64-ci
    path: target/release/outbox-dispatcher
    retention-days: 7
```

Option A is preferred — duplicating the build wastes ~2 min of runner time on every tag push for no benefit.

**Why this fix**

Reusable workflows share the artifact namespace with the caller, and `upload-artifact@v4` no longer allows duplicate names. Either de-duplicate (Option A) or namespace (Option B); silently failing release builds is the worst outcome.

---

## Summary

| # | Title | File:Line | Severity | Category | Status | Notes |
|---|-------|-----------|----------|----------|--------|-------|
| 1 | Compose healthchecks use `wget`, missing from `bookworm-slim` | `docker/docker-compose.example.yml:48`, `examples/docker-compose.with-postgres.yml:53` | Critical | Correctness | DONE | Added `wget` to apt install in Dockerfile runtime stage |
| 2 | Missing `.dockerignore`; build context includes `target/`, `.git/` | repo root | High | Performance | DONE | Added `.dockerignore` at workspace root |
| 3 | Dockerfile warmup swallows errors with `\|\| true` | `docker/Dockerfile:29` | High | Correctness | DONE | Replaced with `cargo fetch --locked && cargo build ... || (echo ... && exit 1)` |
| 4 | Release workflow does not gate on CI | `.github/workflows/release.yml:1-19` | Medium | Correctness | DONE | Added `ci` job using `workflow_call`; `build-binaries` and `docker` now `needs: ci` |
| 5 | `cross` installed from unpinned git ref | `.github/workflows/release.yml:55` | Medium | Correctness | DONE | Replaced with `taiki-e/install-action@v2` pinned to `cross@0.2.5` |
| 6 | Cloud Run docs claim `PORT=8080` is respected, but the binary ignores it | `docs/deployment.md:149` | Medium | Config | DONE | Replaced misleading `Set PORT=8080` sentence with accurate guidance |
| 7 | Compose comment names `.yaml` file that doesn't exist | `docker/docker-compose.example.yml:44` | Low | Documentation | DONE | Fixed `.yaml` → `.toml` in comment |
| 8 | Dockerfile declares no `HEALTHCHECK` | `docker/Dockerfile:42-69` | Low | Config | DONE | Added `HEALTHCHECK` instruction using `wget` (now present in image) |
| 9 | Operations runbook lists Prometheus metrics that do not exist in code | `docs/operations.md:138-153, 160, 247` | High | Documentation | TODO | Doc drift vs `crates/core/src/metrics.rs` — many wrong names and labels |
| 10 | `README.md` is a two-line stub with no project overview | `README.md:1-3` | High | Documentation | TODO | Replace with a proper open-source README distilled from the TDD — pitch, features, quick-start, config, webhook protocol, doc map, license |
| 11 | Release workflow uploads duplicate artifact name `outbox-dispatcher-linux-x86_64` | `.github/workflows/ci.yml:96-99`, `.github/workflows/release.yml:78-82` | Medium | Correctness | TODO | `upload-artifact@v4` will 409 on second upload — blocks releases |

> **Instructions for the implementing LLM:**
> - Change `TODO` to `DONE` once a finding is fully addressed.
> - If a finding is intentionally skipped or cannot be applied, change `TODO` to `SKIPPED` and add a short reason in the **Notes** column.
> - Do not delete rows — the table is the authoritative implementation log.
> - After applying fixes, run the mandatory post-change steps from CLAUDE.md (`cargo fmt`, `cargo check`, `cargo clippy -- -D warnings`, `cargo test`). For doc/CI/Docker-only fixes that do not touch Rust source, the Rust commands are no-ops but should still pass cleanly.
