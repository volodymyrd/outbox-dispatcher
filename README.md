# outbox-dispatcher

[![CI](https://github.com/volodymyrd/outbox-dispatcher/actions/workflows/ci.yml/badge.svg)](https://github.com/volodymyrd/outbox-dispatcher/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/volodymyrd/outbox-dispatcher)](https://github.com/volodymyrd/outbox-dispatcher/releases)

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
  ±25% jittered exponential backoff.
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
                                                               ┌─────────────────┐
                                                               │outbox_deliveries│
                                                               └─────────────────┘
                                                                       │ FOR UPDATE
                                                                       │ SKIP LOCKED
                                                                       ▼
                                                               ┌─────────────────┐
                                                               │   Dispatcher    │── HTTPS ─► Receiver
                                                               └─────────────────┘   (signed)
```

## Quick start

```bash
# 1. Start Postgres + dispatcher (first run builds the image locally — ~3 min)
export ADMIN_TOKEN="$(openssl rand -hex 32)"
cp examples/config.production.toml examples/config.prod.toml
# Edit examples/config.prod.toml: set [signing_keys] entries, tune [dispatch], etc.
docker compose -f examples/docker-compose.with-postgres.yml up -d --build

# 2. Verify
curl http://localhost:9090/ready
```

To run from source:

```bash
docker compose up -d                               # local Postgres on :5434
export DATABASE_URL=postgres://outbox:outbox@localhost:5434/outbox_dispatcher
export ADMIN_TOKEN="$(openssl rand -hex 32)"
cargo run -- migrate                               # apply schema
cargo run                                          # start the dispatcher
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

## Contributing

Issues and pull requests are welcome. Please run the mandatory pre-commit
checks (above) before opening a PR. For substantive design changes, open an
issue first to discuss the approach.
