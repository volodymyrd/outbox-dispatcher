# Code Review — Phase 8 (PR #7)

**Date:** 2026-05-15T17:07:47Z (initial), 2026-05-15T20:45:36Z (follow-up: findings 9–10), 2026-05-15T21:18:00Z (follow-up: findings 12–15), 2026-05-15T21:42:51Z (follow-up: verify F1–F15, findings 16–19), 2026-05-15T22:24:35Z (follow-up: verify F1–F19, findings 20–23)
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

### Finding 12 — `ci.yml` guard `github.event_name != 'workflow_call'` does not skip the build job during a release

| Field | Value |
|-------|-------|
| **File:Line** | `.github/workflows/ci.yml:81-103`, `.github/workflows/release.yml:20-22` |
| **Severity** | High |
| **Category** | Correctness |

**Problem**

The fix for prior Finding 11 added `if: github.event_name != 'workflow_call'` to the `build` job in `ci.yml`, intending to skip the redundant release-binary upload when `ci.yml` is invoked via `workflow_call` from `release.yml`. However, in a reusable workflow `github.event_name` is **the caller's event** — i.e. `push` for a tag push from `release.yml`, never the literal string `workflow_call`. The guard therefore evaluates `'push' != 'workflow_call'` → `true`, the build job runs anyway, and `actions/upload-artifact@v4` will fail the second upload with HTTP 409 (`an artifact with this name already exists on the workflow run`). Net effect: the duplicate-artifact bug Finding 11 was meant to eliminate is still present on every tag push.

**Context** (surrounding code as it exists today)

```yaml
# .github/workflows/ci.yml lines 78-83
  # Skip when invoked via workflow_call from release.yml — the release workflow
  # builds its own per-platform binaries and uploading an artifact with the same
  # name would cause an HTTP 409 conflict with upload-artifact@v4.
  build:
    name: Build Release Binary
    if: github.event_name != 'workflow_call'      # ❌ never false in practice
```

```yaml
# .github/workflows/release.yml lines 20-22
  ci:
    name: Verify CI on tagged commit
    uses: ./.github/workflows/ci.yml
```

**Recommended fix**

Pass an explicit `inputs` flag through `workflow_call` and gate on `inputs.<name>` — the `inputs` context is the only reliable signal that a workflow is being run as a callee:

```yaml
# .github/workflows/ci.yml — replace the existing on: + build job header
on:
  push:
    branches: [main]
  pull_request:
    branches: [main]
  workflow_call:
    inputs:
      skip_release_build:
        description: "Skip the build/upload step (release.yml builds its own binaries)"
        type: boolean
        default: false

# …

  build:
    name: Build Release Binary
    if: ${{ !inputs.skip_release_build }}
    runs-on: ubuntu-latest
    needs: [test-unit, test-integration]
    # … existing steps …
```

```yaml
# .github/workflows/release.yml — pass the flag from the caller
  ci:
    name: Verify CI on tagged commit
    uses: ./.github/workflows/ci.yml
    with:
      skip_release_build: true
```

**Why this fix**

GitHub explicitly documents that contexts in a reusable workflow reflect the caller's run, so `github.event_name` will never be `'workflow_call'`. The `inputs` context, on the other hand, is unique to callees and unambiguous — and the explicit flag also self-documents the intent in `release.yml`.

---

### Finding 13 — Operations runbook documents SIGHUP hot-reload, but no SIGHUP handler exists in the binary

| Field | Value |
|-------|-------|
| **File:Line** | `docs/operations.md:254-269` |
| **Severity** | High |
| **Category** | Documentation / Correctness |

**Problem**

`docs/operations.md` contains an entire "SIGHUP (hot reload)" section instructing operators to `kill -HUP <dispatcher_pid>` (or `docker kill --signal=SIGHUP outbox-dispatcher`) to reload `[signing_keys]`, `[dispatch]`, and `[observability]` settings without a restart. The binary, however, only installs handlers for `tokio::signal::ctrl_c()` (`SIGINT`/`SIGTERM`) — there is no `SIGHUP` handler anywhere in `crates/bin/src/main.rs`. The Linux default disposition for `SIGHUP` on a process with no handler is **terminate** — so an operator who follows the runbook to rotate a signing key will instead **kill the dispatcher** mid-flight. This is operationally dangerous and contradicts the runbook's "without a restart" claim.

**Context** (surrounding code as it exists today)

```markdown
<!-- docs/operations.md lines 254-269 -->
## SIGHUP (hot reload)

Send SIGHUP to reload config without a restart:

```bash
kill -HUP <dispatcher_pid>
# or with Docker:
docker kill --signal=SIGHUP outbox-dispatcher
```

The following settings take effect immediately:
- `[signing_keys]` keyring
- `[dispatch]` defaults (poll interval, batch size, backoff, timeouts, etc.)
- `[observability]` settings

Database connection settings require a restart.
```

```rust
// crates/bin/src/main.rs lines 228-238 — the only signal handling that exists
tokio::spawn(async move {
    tokio::select! {
        _ = signal_token.cancelled() => {}
        _ = tokio::signal::ctrl_c() => {
            info!("received SIGINT/SIGTERM, requesting shutdown");
            signal_token.cancel();
        }
    }
});
```

**Recommended fix**

Pick one of two options — both are acceptable, but the runbook must match the binary:

Option A — remove the SIGHUP section from the runbook (preferred until hot-reload is actually implemented):

```markdown
<!-- docs/operations.md — replace the SIGHUP section -->
## Config changes

Config changes require a restart. There is currently no hot-reload mechanism;
rolling the dispatcher (one replica at a time on multi-replica deployments) is
the safe path. `[signing_keys]`, `[dispatch]`, and `[observability]` settings
all take effect after the restart.

For signing-key rotation specifics, see §Signing key rotation above.
```

Option B — implement SIGHUP reload in the binary (more work). Sketch:

```rust
// crates/bin/src/main.rs — add alongside the SIGINT handler
use tokio::signal::unix::{signal, SignalKind};

let reload_tx = reload_tx.clone();
tokio::spawn(async move {
    let mut sighup = signal(SignalKind::hangup()).expect("install SIGHUP handler");
    while sighup.recv().await.is_some() {
        info!("received SIGHUP, reloading config");
        if let Err(e) = reload_tx.send(()).await {
            warn!(error = %e, "config reload channel closed");
        }
    }
});
```

The reload path then has to re-read `AppConfig`, swap the keyring atomically, and republish the dispatch/observability settings — non-trivial; pick Option A unless the work is in scope.

**Why this fix**

The runbook is the operator's source of truth. A documented procedure that actually terminates the process is worse than no documentation at all — it actively misleads someone who is already in an incident-adjacent situation (rotating a key, tuning backoff). Either implement the behaviour or remove the claim.

---

### Finding 14 — Dockerfile dependency-warmup layer compiles nothing useful (cache benefit is illusory)

| Field | Value |
|-------|-------|
| **File:Line** | `docker/Dockerfile:19-31` |
| **Severity** | Medium |
| **Category** | Performance |

**Problem**

The dependency-warmup stage is meant to precompile all third-party crates in an early layer so source-only edits hit a warm cache on subsequent builds. The current implementation creates stub source files containing only `pub fn _stub() {}` / `fn main() {}` — none of which import any external crate. As a result, `cargo build --release --bin outbox-dispatcher` against the stubs compiles only the four empty workspace crates and **does not compile any of the heavy dependencies** (`sqlx`, `axum`, `reqwest`, `tokio`, OpenTelemetry, `opentelemetry-otlp`, etc.). `cargo fetch --locked` downloads sources to the registry but does not produce build artefacts. On every full image build (cache miss), every dependency is recompiled from scratch in the second `cargo build` step, undoing the entire point of the split.

**Context** (surrounding code as it exists today)

```dockerfile
# docker/Dockerfile lines 19-31
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
```

**Recommended fix**

Switch to `cargo-chef`, which is the standard idiom for this problem in Rust Docker builds. It computes a JSON recipe from the manifests + lockfile and compiles only the dependency graph, with no stubs to maintain:

```dockerfile
# docker/Dockerfile — replace the builder stage
FROM rust:1.87-bookworm AS chef
RUN cargo install --locked cargo-chef --version 0.1.68
WORKDIR /build

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ENV SQLX_OFFLINE=true
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY crates/        crates/
COPY migrations/    migrations/
COPY envs/          envs/
COPY .sqlx/         .sqlx/
RUN cargo build --release --bin outbox-dispatcher
```

If `cargo-chef` is not desirable, a less elegant alternative is to make each stub `lib.rs`/`main.rs` actually `use` the crate's external deps so that `cargo build` is forced to compile them. That requires keeping the stub list in sync with `Cargo.toml` for every dep change, which is exactly the maintenance trap `cargo-chef` eliminates.

**Why this fix**

`cargo-chef` is the canonical Rust-on-Docker layering pattern and the only one that actually achieves the layer-cache goal that the current Dockerfile attempts. Without it, the "split deps from source" structure is paying complexity for zero benefit — every CI release build (a cache miss on `cache-from: type=gha` whenever any source changes) recompiles the entire dependency tree.

---

### Finding 15 — `docker/docker-compose.example.yml` instructs operators to copy a non-existent `.env.example`

| Field | Value |
|-------|-------|
| **File:Line** | `docker/docker-compose.example.yml:4-6` |
| **Severity** | Low |
| **Category** | Documentation |

**Problem**

The usage comment block at the top of `docker/docker-compose.example.yml` reads:

```yaml
# Usage:
#   cp .env.example .env          # fill in secrets
#   docker compose -f docker/docker-compose.example.yml up -d
```

`.env.example` does not exist in the repository (only `.env.toml` does, and that file is gitignored, not a template). Operators following the comment will hit `cp: .env.example: No such file or directory` on first run and have to reverse-engineer which env vars the compose file actually consumes (`POSTGRES_PASSWORD`, `ADMIN_TOKEN`, the per-signing-key vars). The very next finding ("usability footgun") is that the dispatcher service references `./config.prod.toml` as a bind-mount which also has no template; one-stop documentation would help.

**Context** (surrounding code as it exists today)

```yaml
# docker/docker-compose.example.yml lines 1-6
# Example: run outbox-dispatcher alongside a standalone Postgres instance.
# This is a minimal reference — copy and adjust for your environment.
#
# Usage:
#   cp .env.example .env          # fill in secrets
#   docker compose -f docker/docker-compose.example.yml up -d
```

**Recommended fix**

Ship a small `.env.example` at the repo root listing the variables the compose example expects, and keep the comment accurate:

```ini
# .env.example — copy to .env (gitignored) and fill in values before `docker compose up`.

# Postgres bootstrap
POSTGRES_PASSWORD=change-me

# Admin API bearer token — generate with: openssl rand -hex 32
ADMIN_TOKEN=change-me

# Per-signing-key secrets (one var per [signing_keys] entry). Examples:
# PAYMENTS_HMAC_SECRET=change-me
# NOTIFICATIONS_HMAC_SECRET=change-me
```

Then update the comment to point at it. If you'd rather not add the file, drop the `cp` line and instead inline the required exports:

```yaml
# Usage:
#   export POSTGRES_PASSWORD=...  ADMIN_TOKEN=$(openssl rand -hex 32)
#   docker compose -f docker/docker-compose.example.yml up -d
```

**Why this fix**

The example is the first thing operators run. A copy-pasteable command that fails on step one undermines confidence in the rest of the deployment docs. Either form (template file or inline exports) gets the user to a green compose run on the first try.

---

### Finding 16 — `docs/operations.md` still tells operators to send SIGHUP after Finding 13's fix

| Field | Value |
|-------|-------|
| **File:Line** | `docs/operations.md:179`, `docs/operations.md:194` |
| **Severity** | High |
| **Category** | Documentation / Correctness |

**Problem**

Finding 13 removed the dedicated "SIGHUP (hot reload)" section and replaced it with a §Config changes section that correctly states "Config changes require a restart. There is no hot-reload mechanism." However, two cross-references to SIGHUP remained in the file and were not updated. Both are still active operator-facing instructions:

- §Dead letters appearing for a callback → step 2: "the key is missing from `[signing_keys]`. Add it and send a SIGHUP or restart."
- §Signing key rotation → step 3: "Send SIGHUP or restart the dispatcher. The new key takes effect on the next delivery attempt."

As established in Finding 13, the binary has no SIGHUP handler — the default Linux disposition for SIGHUP on a handler-less process is **terminate**. An operator rotating a key by following step 3 literally will kill the dispatcher mid-flight. The container's `restart: unless-stopped` policy means the supervisor will restart it, so the end-state is "a brutal restart" — but the runbook explicitly frames SIGHUP as an alternative to a restart, which is the opposite of what actually happens. This contradicts the §Config changes section in the same file.

**Context** (surrounding code as it exists today)

```markdown
<!-- docs/operations.md line 179 (inside §Dead letters appearing) -->
   - `signing_key_id 'foo' not registered` — the key is missing from `[signing_keys]`. Add it and send a SIGHUP or restart.
```

```markdown
<!-- docs/operations.md lines 190-198 -->
### Signing key rotation

1. Generate a new secret and update the env var (or Kubernetes secret).
2. Add the new key id to `[signing_keys]` in config (or update the existing entry to point to the new env var).
3. Send SIGHUP or restart the dispatcher. The new key takes effect on the next delivery attempt.
4. Publishers can start writing the new key id immediately after the dispatcher config is updated.
5. Keep the old key id in `[signing_keys]` (pointing to the old secret) until all in-flight deliveries using it have completed.
```

**Recommended fix**

Drop "SIGHUP or" from both locations so they match the §Config changes guidance:

```markdown
<!-- docs/operations.md line 179 -->
   - `signing_key_id 'foo' not registered` — the key is missing from `[signing_keys]`. Add it and restart the dispatcher.
```

```markdown
<!-- docs/operations.md lines 190-198 -->
### Signing key rotation

1. Generate a new secret and update the env var (or Kubernetes secret).
2. Add the new key id to `[signing_keys]` in config (or update the existing entry to point to the new env var).
3. Restart the dispatcher. The new key takes effect on the next delivery attempt.
4. Publishers can start writing the new key id immediately after the dispatcher config is updated.
5. Keep the old key id in `[signing_keys]` (pointing to the old secret) until all in-flight deliveries using it have completed.
```

**Why this fix**

The runbook must be internally consistent. The §Config changes section says config requires a restart; these two earlier sections must not contradict it by suggesting SIGHUP as a parallel path that does not exist in the binary. Finding 13's fix is incomplete until these cross-references are also corrected.

---

### Finding 17 — Quick-start bind-mount path mismatch silently nullifies the prod config overlay

| Field | Value |
|-------|-------|
| **File:Line** | `docs/deployment.md:12-22`, `examples/docker-compose.with-postgres.yml:51`, `docker/docker-compose.example.yml:46` |
| **Severity** | High |
| **Category** | Correctness / Documentation |

**Problem**

Both `docs/deployment.md` and `README.md` walk first-time users through this quick-start:

```bash
# from the workspace root
cp examples/config.production.toml config.prod.toml
docker compose -f examples/docker-compose.with-postgres.yml up -d
```

The compose file at `examples/docker-compose.with-postgres.yml` declares the bind mount as:

```yaml
- ./config.prod.toml:/app/envs/app_config_prod.toml:ro
```

Per Compose Spec, **relative source paths in `volumes` resolve relative to the Compose file's own directory**, not the current working directory. Compose therefore looks for `examples/config.prod.toml`, but the `cp` command put the file at the workspace root `./config.prod.toml`. The host path the bind mount resolves to does not exist. With the default Docker daemon behavior, this either fails the `up` command (`bind source path does not exist`) or — depending on the daemon — silently creates an empty directory at the host path and mounts it. Either way, the dispatcher runs without the prod overlay the user thought they were providing.

The same path-resolution rule applies to `docker/docker-compose.example.yml:46` (`./config.prod.toml` resolves to `docker/config.prod.toml`), which has no `cp` instruction at all — so users following its top-of-file usage block are also pointed at a host path that does not exist.

**Context** (surrounding code as it exists today)

```markdown
<!-- docs/deployment.md lines 12-26 -->
## Quick start with Docker Compose

```bash
# Generate secrets
export ADMIN_TOKEN="$(openssl rand -hex 32)"
export MY_HMAC_SECRET="$(openssl rand -base64 48)"

# Copy and edit the production config
cp examples/config.production.toml config.prod.toml
# Edit config.prod.toml: set [signing_keys] entries, tune [dispatch], etc.

# Start Postgres + dispatcher
docker compose -f examples/docker-compose.with-postgres.yml up -d
```
```

```yaml
# examples/docker-compose.with-postgres.yml lines 48-51
    volumes:
      # Production config overlay — overrides baked-in defaults.
      # The file must exist; copy and edit examples/config.production.toml.
      - ./config.prod.toml:/app/envs/app_config_prod.toml:ro
```

**Recommended fix**

Pick one of these two approaches and apply consistently.

Option A — fix the `cp` destination so it lands next to the compose file:

```markdown
<!-- docs/deployment.md and README.md quick-start -->
# Copy and edit the production config (must live next to the compose file).
cp examples/config.production.toml examples/config.prod.toml
# Edit examples/config.prod.toml: set [signing_keys] entries, tune [dispatch], etc.

docker compose -f examples/docker-compose.with-postgres.yml up -d
```

And update `docker/docker-compose.example.yml`'s usage comment to instruct copying to `docker/config.prod.toml`.

Option B — anchor the bind mount to the working directory using an env var so it does not depend on the compose file's location:

```yaml
# examples/docker-compose.with-postgres.yml
    volumes:
      - ${OUTBOX_CONFIG:-./config.prod.toml}:/app/envs/app_config_prod.toml:ro
```

…and document `export OUTBOX_CONFIG="$(pwd)/config.prod.toml"` in the quick-start.

Option A is the smaller change and keeps the example self-contained.

**Why this fix**

The "happy path" quick-start in the README is the first thing a new user runs. It must work as written. Today it silently drops the prod overlay (or fails outright), which is the worst-of-both-worlds: the user thinks they configured signing keys and dispatch settings, but the running container is using only baked-in defaults.

---

### Finding 18 — Alerting rule uses non-literal range selector, which is invalid PromQL

| Field | Value |
|-------|-------|
| **File:Line** | `docs/operations.md:165` |
| **Severity** | Low |
| **Category** | Documentation |

**Problem**

The §Alerting recommendations table includes:

```
| Scheduler stalled | `rate(outbox_cycle_duration_seconds_count[2 * poll_interval]) == 0` | Critical |
```

PromQL's range-vector selector `[…]` only accepts a **literal duration** (`30s`, `5m`, `1h`). Expressions like `2 * poll_interval` are a parse error — Prometheus will reject the rule. Operators copy-pasting this row into an Alertmanager config or a Grafana panel will hit `1:36: parse error: unexpected character inside braces: ' '`. The intent (use 2× the configured poll interval) is clear in prose, but the alert as written cannot be evaluated.

**Context** (surrounding code as it exists today)

```markdown
<!-- docs/operations.md lines 160-167 -->
| Alert | Condition | Severity |
|-------|-----------|----------|
| Dead letters accumulating | `rate(outbox_dead_letters_total[5m]) > 0` | Warning |
| High dispatch lag | `outbox_lag_seconds > 300` | Warning |
| Listener down | `outbox_listener_connection_status == 0 for 2m` | Critical |
| Scheduler stalled | `rate(outbox_cycle_duration_seconds_count[2 * poll_interval]) == 0` | Critical |
| Signing key drift | `rate(outbox_signing_key_resolution_failures_total[5m]) > 0` | Warning |
| Corrupted rows | `rate(outbox_corrupted_rows_total[5m]) > 0` | Warning |
```

**Recommended fix**

Either give a concrete duration (matching the default `poll_interval_secs = 5` ⇒ `10s` lower bound, but a larger window smooths short-lived stalls) or rephrase to show the formula outside the code block:

```markdown
| Scheduler stalled | `rate(outbox_cycle_duration_seconds_count[1m]) == 0` (set window ≥ 2 × `poll_interval_secs`) | Critical |
```

Same comment applies to "Listener down": the operator-facing trailing `for 2m` belongs in the alert rule's `for:` field, not in the PromQL expression — though Prometheus will treat the trailing word as part of the expression body and reject it too. Suggest:

```markdown
| Listener down | `outbox_listener_connection_status == 0` (with `for: 2m` in the rule) | Critical |
```

**Why this fix**

An alerting table in a runbook is implicitly a copy-paste source. Rules that are syntactically invalid waste operator time during the on-call onboarding moment when the value should be highest.

---

### Finding 19 — README quick-start "from source" missing `DATABASE_URL`

| Field | Value |
|-------|-------|
| **File:Line** | `README.md:70-76` |
| **Severity** | Low |
| **Category** | Documentation |

**Problem**

The "To run from source" snippet in the README is:

```bash
docker compose up -d    # local Postgres on :5434
cargo run -- migrate    # apply schema
cargo run               # start the dispatcher
```

Both `cargo run` invocations require `database.url` to be set. In the project's local-dev layout, `.env.toml` is gitignored and not present after a fresh clone — so on a clean checkout `cargo run -- migrate` exits with a configuration error about a missing database URL. The CLAUDE.md project notes consistently prefix every example with `DATABASE_URL=postgres://outbox:outbox@localhost:5434/outbox_dispatcher`; the README should match.

**Context** (surrounding code as it exists today)

```markdown
<!-- README.md lines 70-76 -->
To run from source:

```bash
docker compose up -d    # local Postgres on :5434
cargo run -- migrate    # apply schema
cargo run               # start the dispatcher
```
```

**Recommended fix**

```markdown
To run from source:

```bash
docker compose up -d                               # local Postgres on :5434
export DATABASE_URL=postgres://outbox:outbox@localhost:5434/outbox_dispatcher
export ADMIN_TOKEN="$(openssl rand -hex 32)"
cargo run -- migrate                               # apply schema
cargo run                                          # start the dispatcher
```
```

**Why this fix**

A first-time contributor copying these three lines hits a config error within seconds. Adding the two missing exports makes the snippet self-contained and matches the conventions used everywhere else in the project's docs.

---

### Finding 20 — Dispatcher does not catch SIGTERM; production `docker stop` and Kubernetes pod-termination skip graceful shutdown

| Field | Value |
|-------|-------|
| **File:Line** | `crates/bin/src/main.rs:233-243`, `docker/Dockerfile:59` |
| **Severity** | High |
| **Category** | Correctness / Concurrency |

**Problem**

Phase 8's whole purpose is to package the dispatcher as a production-ready container image, and both `docker/Dockerfile` and `docs/deployment.md` set up Docker + Kubernetes deployment paths. Those paths rely on **SIGTERM** for graceful shutdown:

- `docker stop` sends `SIGTERM` to PID 1 by default, then `SIGKILL` after the grace period (default 10 s).
- The Kubernetes kubelet sends `SIGTERM` at pod termination, then `SIGKILL` after `terminationGracePeriodSeconds` (default 30 s).

The signal-handling task installed in `crates/bin/src/main.rs` only listens via `tokio::signal::ctrl_c()`, which on Unix is documented to register a handler for **`SIGINT` only** — `SIGTERM` is not caught. The default Linux disposition for an uncaught `SIGTERM` is "terminate," so the kernel kills the process without running the shutdown token, the JoinSet drain, or the tracer flush. Net effect under Docker/Kubernetes:

- In-flight HTTP webhook deliveries are aborted mid-call; their rows stay locked until `locked_until` expires, delaying retry by `handler_timeout + lock_buffer` seconds.
- The LISTEN connection is dropped without an unsubscribe.
- Buffered OpenTelemetry spans for the final cycle are discarded (`tracer_provider.shutdown()` never runs).
- The reassuring log line `"received SIGINT/SIGTERM, requesting shutdown"` is itself misleading — only SIGINT can fire it.

The Rust source for this lives outside the PR's diff, so this is technically a pre-existing bug, but Phase 8 is what makes it operationally relevant (no one was running this as PID 1 before).

**Context** (surrounding code as it exists today)

```rust
// crates/bin/src/main.rs lines 228-244
// Signal handler: cancel the shutdown token on SIGINT/Ctrl-C. Also exits
// cleanly when the token is cancelled by any other path (e.g. admin bind
// failure) so the JoinSet can drain uniformly.
{
    let shutdown_clone = shutdown.clone();
    workers.spawn(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("received SIGINT/SIGTERM, requesting shutdown");
                shutdown_clone.cancel();
            }
            _ = shutdown_clone.cancelled() => {
                // Another path triggered shutdown — exit cleanly.
            }
        }
    });
}
```

```dockerfile
# docker/Dockerfile lines 56-59 — no STOPSIGNAL override, defaults to SIGTERM
HEALTHCHECK --interval=15s --timeout=5s --start-period=20s --retries=3 \
    CMD wget -qO- http://localhost:9090/health || exit 1

ENTRYPOINT ["/app/outbox-dispatcher"]
```

**Recommended fix**

Add an explicit SIGTERM handler alongside the existing SIGINT one. Use `tokio::select!` to race them so whichever arrives first cancels the shutdown token:

```rust
// crates/bin/src/main.rs — replace the signal-handler block
{
    let shutdown_clone = shutdown.clone();
    workers.spawn(async move {
        #[cfg(unix)]
        let mut sigterm = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )
        .expect("install SIGTERM handler");

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("received SIGINT, requesting shutdown");
                shutdown_clone.cancel();
            }
            #[cfg(unix)]
            _ = sigterm.recv() => {
                info!("received SIGTERM, requesting shutdown");
                shutdown_clone.cancel();
            }
            _ = shutdown_clone.cancelled() => {
                // Another path triggered shutdown — exit cleanly.
            }
        }
    });
}
```

The `#[cfg(unix)]` guard keeps the Windows build working (Windows uses CTRL-BREAK instead). As a much smaller but partial alternative, the Dockerfile can declare `STOPSIGNAL SIGINT` so `docker stop` sends SIGINT instead of SIGTERM — that addresses Docker but not Kubernetes (kubelet always sends SIGTERM regardless of `STOPSIGNAL`).

**Why this fix**

Linux production environments deliver `SIGTERM` for orderly shutdown; not catching it means every replica replacement, rolling restart, or `docker stop` leaves a few in-flight deliveries locked for the full `handler_timeout + lock_buffer` window and silently drops the final batch of telemetry spans. This is exactly the failure mode the rest of Phase 7/8 was built to prevent.

---

### Finding 21 — Quick-start `docker compose up` references a published image that does not exist before the first release

| Field | Value |
|-------|-------|
| **File:Line** | `examples/docker-compose.with-postgres.yml:30`, `README.md:61-66`, `docs/deployment.md:10-22` |
| **Severity** | Medium |
| **Category** | Documentation / Correctness |

**Problem**

The headline quick-start in both `README.md` and `docs/deployment.md` is:

```bash
docker compose -f examples/docker-compose.with-postgres.yml up -d
```

`examples/docker-compose.with-postgres.yml` sets `image: ghcr.io/volodymyrd/outbox-dispatcher:latest` (with the `build:` stanza commented out). `release.yml` publishes that image only when a tag `v*.*.*` is pushed — until the first release exists, `:latest` is a 404. Every new user who runs the quick-start before the first GHCR release sees a `manifest unknown` / `pull access denied` error on `docker compose up`. The two-line workaround ("Or build locally: build: context: ..") is commented out in the compose file but never mentioned in the README/deployment quick-start prose, so users have to read the YAML to discover it.

This compounds with the project being open-source and pre-1.0: the **first** thing a curious visitor tries is the quick-start, and on a fresh clone the most likely outcome is a failure that has nothing to do with the dispatcher itself.

**Context** (surrounding code as it exists today)

```yaml
# examples/docker-compose.with-postgres.yml lines 29-34
  dispatcher:
    image: ghcr.io/volodymyrd/outbox-dispatcher:latest
    # Or build locally:
    # build:
    #   context: ..
    #   dockerfile: docker/Dockerfile
```

```markdown
<!-- README.md lines 61-66 -->
```bash
# 1. Start Postgres + dispatcher with a single compose command
export ADMIN_TOKEN="$(openssl rand -hex 32)"
cp examples/config.production.toml examples/config.prod.toml
# Edit examples/config.prod.toml: set [signing_keys] entries, tune [dispatch], etc.
docker compose -f examples/docker-compose.with-postgres.yml up -d
```
```

**Recommended fix**

Either pin a known-existing image tag in the compose file and document publishing as a prerequisite, or default to the local-build path so the quick-start always works. The latter has zero failure modes pre-release:

```yaml
# examples/docker-compose.with-postgres.yml lines 29-34
  dispatcher:
    build:
      context: ..
      dockerfile: docker/Dockerfile
    # Or, to consume the published image instead:
    # image: ghcr.io/volodymyrd/outbox-dispatcher:1.0.0
```

And update the README quick-start to note the first-run build:

```markdown
# 1. Start Postgres + dispatcher (first run builds the image locally — ~3 min)
export ADMIN_TOKEN="$(openssl rand -hex 32)"
cp examples/config.production.toml examples/config.prod.toml
# Edit examples/config.prod.toml: set [signing_keys] entries, tune [dispatch], etc.
docker compose -f examples/docker-compose.with-postgres.yml up -d --build
```

Once a real release exists, flip the comment so `image:` is the default and the local build is the fallback.

**Why this fix**

The README quick-start is the project's first impression. A copy-paste-able command that fails for everyone for the entire pre-release window is worse than no quick-start at all — and `cargo-chef` plus the new `.dockerignore` make the local build genuinely fast.

---

### Finding 22 — README architecture diagram has a one-character misalignment on the `outbox_deliveries` box

| Field | Value |
|-------|-------|
| **File:Line** | `README.md:42-57` |
| **Severity** | Low |
| **Category** | Documentation |

**Problem**

The ASCII architecture diagram uses 16 `─` glyphs for the box borders, but the `outbox_deliveries` text inside one box is 17 characters wide, so the right-hand border sticks out by one cell in any monospace renderer:

```
┌────────────────┐    ← 16 chars
│outbox_deliveries│   ← 17 chars
└────────────────┘    ← 16 chars
```

Purely cosmetic, but a misaligned diagram on the README's "Architecture at a glance" section subtly undermines the visual polish the rest of the README invests in.

**Context** (surrounding code as it exists today)

```markdown
<!-- README.md lines 42-57 -->
                                                              ┌────────────────┐
                                                              │outbox_deliveries│
                                                              └────────────────┘
```

**Recommended fix**

Widen the box borders to 17 cells (or shorten the label). Widening keeps the table layout consistent with the other boxes which have small interior padding:

```markdown
                                                              ┌─────────────────┐
                                                              │outbox_deliveries│
                                                              └─────────────────┘
```

**Why this fix**

A one-character extension fixes the alignment without re-flowing the rest of the diagram.

---

### Finding 23 — `docker/docker-compose.example.yml` silently accepts an empty `ADMIN_TOKEN`

| Field | Value |
|-------|-------|
| **File:Line** | `docker/docker-compose.example.yml:39` |
| **Severity** | Low |
| **Category** | Security / Config |

**Problem**

`examples/docker-compose.with-postgres.yml:42` correctly fails fast if `ADMIN_TOKEN` is unset:

```yaml
ADMIN_TOKEN: ${ADMIN_TOKEN:?ADMIN_TOKEN is required}
```

But the sibling example at `docker/docker-compose.example.yml:39` uses the unchecked form:

```yaml
ADMIN_TOKEN: ${ADMIN_TOKEN}
```

If the operator forgets to `export ADMIN_TOKEN=…` before `docker compose up`, the variable expands to an empty string, the dispatcher starts with an empty admin token, and the admin API becomes effectively unauthenticated (depending on how `AdminConfig::validate()` handles empty tokens — even if startup is refused, the diagnostic is "admin token is empty" not "ADMIN_TOKEN env var not set", which is harder to debug). The two compose files should be consistent, and the safer behavior is to fail at compose-time with a clear error.

**Context** (surrounding code as it exists today)

```yaml
# docker/docker-compose.example.yml lines 36-41
    environment:
      APP_ENV: prod
      DATABASE_URL: postgres://outbox:${POSTGRES_PASSWORD:-outbox}@postgres:5432/outbox_dispatcher
      ADMIN_TOKEN: ${ADMIN_TOKEN}
      # Add your signing key secrets here, e.g.:
      # MY_SERVICE_HMAC_SECRET: ${MY_SERVICE_HMAC_SECRET}
```

**Recommended fix**

Mirror the sibling example's validation:

```yaml
    environment:
      APP_ENV: prod
      DATABASE_URL: postgres://outbox:${POSTGRES_PASSWORD:-outbox}@postgres:5432/outbox_dispatcher
      ADMIN_TOKEN: ${ADMIN_TOKEN:?ADMIN_TOKEN is required}
      # Add your signing key secrets here, e.g.:
      # MY_SERVICE_HMAC_SECRET: ${MY_SERVICE_HMAC_SECRET}
```

**Why this fix**

Two examples that target the same audience should share their safety rails. The `:?…` form turns a silent footgun into a one-line compose error that names the variable to set.

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
| 9 | Operations runbook lists Prometheus metrics that do not exist in code | `docs/operations.md:138-153, 160, 247` | High | Documentation | DONE | Rewrote §Key metrics table and §Alerting to match `crates/core/src/metrics.rs`; fixed retention monitoring line |
| 10 | `README.md` is a two-line stub with no project overview | `README.md:1-3` | High | Documentation | DONE | Replaced stub with full README: pitch, features, architecture diagram, quick-start, config, webhook protocol, operations links, dev commands |
| 11 | Release workflow uploads duplicate artifact name `outbox-dispatcher-linux-x86_64` | `.github/workflows/ci.yml:96-99`, `.github/workflows/release.yml:78-82` | Medium | Correctness | DONE | Guard added (`if: github.event_name != 'workflow_call'`) but does not skip the job — see Finding 12 |
| 12 | `ci.yml` build-job guard `github.event_name != 'workflow_call'` never evaluates to false | `.github/workflows/ci.yml:81-83`, `.github/workflows/release.yml:20-22` | High | Correctness | DONE | Added `inputs.skip_release_build` to `workflow_call` inputs; guard changed to `if: ${{ !inputs.skip_release_build }}`; `release.yml` passes `skip_release_build: true` |
| 13 | Operations runbook documents SIGHUP hot-reload, but the binary has no SIGHUP handler — sending SIGHUP terminates the process | `docs/operations.md:254-269` | High | Documentation | DONE | Replaced SIGHUP section with "Config changes require a restart" guidance |
| 14 | Dockerfile dependency-warmup layer compiles only the empty stubs — no third-party crates are precompiled, so the layer cache provides no benefit | `docker/Dockerfile:19-31` | Medium | Performance | DONE | Replaced stub-based warmup with `cargo-chef` (planner + cook stages) |
| 15 | `docker/docker-compose.example.yml` tells operators to `cp .env.example .env`, but `.env.example` does not exist | `docker/docker-compose.example.yml:4-6` | Low | Documentation | DONE | Added `.env.example` at repo root with required variables; updated compose comment to reference it |
| 16 | SIGHUP references still present in operations.md (Dead letters §, Signing key rotation §) after Finding 13's fix — runbook still suggests an action that kills the process | `docs/operations.md:179, 194` | High | Documentation | DONE | Removed "send a SIGHUP or" from both locations; both now say "restart the dispatcher" |
| 17 | Quick-start `cp examples/config.production.toml config.prod.toml` puts the file at the workspace root, but the compose bind mount `./config.prod.toml` resolves relative to the compose file (`examples/`) — the prod overlay is silently absent | `docs/deployment.md:18`, `examples/docker-compose.with-postgres.yml:51`, `docker/docker-compose.example.yml:46` | High | Correctness | DONE | Changed `cp` destination to `examples/config.prod.toml` in docs/deployment.md and README.md; updated volume comment in examples/docker-compose.with-postgres.yml; updated docker/docker-compose.example.yml usage comment and volume comment to instruct `docker/config.prod.toml` |
| 18 | Alerting table uses non-literal PromQL range selector (`[2 * poll_interval]`) and `for 2m` inside the expression — invalid PromQL | `docs/operations.md:164-165` | Low | Documentation | DONE | Fixed `[2 * poll_interval]` → `[1m]` with explanatory note; moved `for 2m` out of the PromQL expression into a prose annotation |
| 19 | README "run from source" quick-start omits `DATABASE_URL` / `ADMIN_TOKEN` — both `cargo run` invocations will fail on a fresh clone | `README.md:70-76` | Low | Documentation | DONE | Added `export DATABASE_URL=...` and `export ADMIN_TOKEN=...` before the `cargo run` commands |
| 20 | Dispatcher does not catch SIGTERM — `docker stop` and Kubernetes pod-termination skip graceful shutdown, leaving rows locked and dropping the final telemetry batch | `crates/bin/src/main.rs:233-243`, `docker/Dockerfile:59` | High | Correctness / Concurrency | TODO | Pre-existing in `main.rs` (not in PR diff) but becomes operationally relevant with Phase 8's Docker/K8s deployment story. Either add a `tokio::signal::unix::SignalKind::terminate()` handler in `main.rs`, or as a partial Docker-only mitigation add `STOPSIGNAL SIGINT` to the Dockerfile (does not help Kubernetes) |
| 21 | Quick-start `docker compose up` references `ghcr.io/volodymyrd/outbox-dispatcher:latest`, which does not exist until the first release tag is pushed | `examples/docker-compose.with-postgres.yml:30`, `README.md:61-66`, `docs/deployment.md:10-22` | Medium | Documentation / Correctness | TODO | Default the compose file to `build:` and add `--build` to the README/deployment-quick-start command; flip to `image:` once a real release exists |
| 22 | README architecture diagram `outbox_deliveries` box is one cell wider than its borders | `README.md:42-57` | Low | Documentation | TODO | Widen the top/bottom borders from 16 to 17 `─` glyphs |
| 23 | `docker/docker-compose.example.yml` silently accepts an empty `ADMIN_TOKEN` (sibling `examples/docker-compose.with-postgres.yml` uses `${ADMIN_TOKEN:?…}`) | `docker/docker-compose.example.yml:39` | Low | Security / Config | TODO | Replace `${ADMIN_TOKEN}` with `${ADMIN_TOKEN:?ADMIN_TOKEN is required}` |

## Merge readiness

**Status: READY TO MERGE — with one follow-up issue strongly recommended.**

Findings F1–F19 are all confirmed DONE in the current branch state. The four new findings from this follow-up pass (F20–F23) are not strict blockers for Phase 8's scope (Docker/CI/docs):

- **F20 (High)** is a pre-existing bug in `crates/bin/src/main.rs` that this PR does not touch but makes operationally relevant. Strongly recommended as a follow-up issue/PR — without SIGTERM handling, every Kubernetes rollout or `docker stop` will leak `locked_until` time and drop the final telemetry batch. Phase 8 ships a working image, but not one that shuts down cleanly under the most common production termination signal.
- **F21 (Medium)** breaks the README quick-start for every user before the first GHCR release — high visibility, easy fix.
- **F22, F23 (Low)** are polish items.

None of F20–F23 invalidates the work in this PR; F1–F19's fixes stand correctly. Recommend merging Phase 8 and filing F20 as an immediate follow-up (touches Rust source and a fresh `.sqlx`-free path, so it belongs in its own PR). F21–F23 can be folded into the same follow-up or fixed in-flight before merge if quick to land.

> **Instructions for the implementing LLM:**
> - Change `TODO` to `DONE` once a finding is fully addressed.
> - If a finding is intentionally skipped or cannot be applied, change `TODO` to `SKIPPED` and add a short reason in the **Notes** column.
> - Do not delete rows — the table is the authoritative implementation log.
> - After applying fixes, run the mandatory post-change steps from CLAUDE.md (`cargo fmt`, `cargo check`, `cargo clippy -- -D warnings`, `cargo test`). For doc/CI/Docker-only fixes that do not touch Rust source, the Rust commands are no-ops but should still pass cleanly.
