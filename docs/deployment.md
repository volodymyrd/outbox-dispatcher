# Deployment Guide

This guide covers running `outbox-dispatcher` in production.

## Prerequisites

- Postgres 14 or later
- Docker (recommended) **or** a pre-built binary from the [releases page](https://github.com/volodymyrd/outbox-dispatcher/releases)

## Quick start with Docker Compose

```bash
# Generate secrets
export ADMIN_TOKEN="$(openssl rand -hex 32)"
export MY_HMAC_SECRET="$(openssl rand -base64 48)"

# Copy and edit the production config (must live next to the compose file)
cp examples/config.production.toml examples/config.prod.toml
# Edit examples/config.prod.toml: set [signing_keys] entries, tune [dispatch], etc.

# Start Postgres + dispatcher
docker compose -f examples/docker-compose.with-postgres.yml up -d

# Check readiness
curl http://localhost:9090/ready
```

## Configuration layers

Config is loaded in this order — later entries win:

| Source | Path | Required |
|--------|------|----------|
| Base defaults | `envs/app_config.toml` (baked into image) | Yes |
| Env override | `envs/app_config_${APP_ENV}.toml` | No |
| Local secrets | `.env.toml` | No |
| Env vars | `APP__*` / `DATABASE_URL` / `ADMIN_TOKEN` | No |

`APP_ENV` defaults to `local`; set to `prod` in production.

### Required settings

| Setting | How to set |
|---------|-----------|
| `database.url` | `DATABASE_URL` env var |
| `admin.auth_token` | `ADMIN_TOKEN` env var |
| Signing secrets | One env var per key, named in `[signing_keys]` |

### Signing keys

Add a stanza per key id that publishers reference in their `callbacks` JSONB:

```toml
# envs/app_config_prod.toml
[signing_keys]
"payments-v1"       = { secret_env = "PAYMENTS_HMAC_SECRET" }
"notifications-v1"  = { secret_env = "NOTIFICATIONS_HMAC_SECRET" }
```

Generate a secret (must decode to ≥ 32 bytes):

```bash
openssl rand -base64 48   # 48 random bytes → 64 base64 chars
```

Export it before starting the process:

```bash
export PAYMENTS_HMAC_SECRET="<base64 value>"
```

## Database setup

The dispatcher runs migrations automatically on startup. To run them manually (e.g. pre-deploy verification):

```bash
DATABASE_URL=postgres://... outbox-dispatcher migrate
```

To skip migrations and start directly (useful when migrations are managed externally):

```bash
outbox-dispatcher --skip-migrations
```

## Running the binary directly

```bash
export DATABASE_URL="postgres://outbox:outbox@localhost:5432/outbox_dispatcher"
export ADMIN_TOKEN="$(openssl rand -hex 32)"
export APP_ENV=prod

outbox-dispatcher
```

Verbose logging:

```bash
RUST_LOG=debug outbox-dispatcher
```

## Exposed ports

| Port | Purpose |
|------|---------|
| `9090` | Admin HTTP API (requires `Authorization: Bearer <ADMIN_TOKEN>`) |
| `9091` | Prometheus `/metrics` scrape endpoint (unauthenticated) |

## Health and readiness

| Endpoint | Auth | Purpose |
|----------|------|---------|
| `GET /health` | None | Liveness: always returns `200 "ok"` if the process is alive |
| `GET /ready` | None | Readiness: checks DB connectivity, LISTEN status, and scheduler heartbeat |

Readiness returns `503` if:
- The database ping fails
- The `LISTEN` connection to Postgres is down
- The scheduler has not completed a cycle within `2 × poll_interval`

## Scaling and multi-replica deployment

Multiple replicas can run against the same database safely. The `locked_until` mechanism prevents duplicate delivery:

- Each dispatcher locks a delivery row before invoking the webhook.
- Concurrent dispatchers skip rows already locked.
- If a replica crashes mid-call, the lock expires after `handler_timeout + lock_buffer` and the row becomes eligible for retry.

Recommendations:
- Keep `dispatch_concurrency` ≤ `database.max_connections` per replica to avoid pool starvation.
- Set `database.max_connections` to a value that fits within Postgres's `max_connections` budget, divided by replica count.

## Cloud Run

outbox-dispatcher is Cloud Run free-tier compatible. Key settings for Cloud Run:

```toml
[database]
max_connections = 5        # Cloud Run scales to zero; keep the pool small
acquire_timeout_secs = 5

[dispatch]
dispatch_concurrency = 4   # stay comfortably below max_connections

[admin]
bind = "0.0.0.0:8080"      # Cloud Run routes traffic to $PORT (default 8080)
```

Cloud Run injects a `$PORT` env var (default `8080`). `outbox-dispatcher` does
not read `$PORT` directly — set `admin.bind = "0.0.0.0:8080"` (or pass
`APP__ADMIN__BIND=0.0.0.0:8080`) so the admin listener matches.

## Kubernetes

A minimal Deployment:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: outbox-dispatcher
spec:
  replicas: 2
  selector:
    matchLabels:
      app: outbox-dispatcher
  template:
    metadata:
      labels:
        app: outbox-dispatcher
      annotations:
        prometheus.io/scrape: "true"
        prometheus.io/port: "9091"
    spec:
      containers:
        - name: dispatcher
          image: ghcr.io/volodymyrd/outbox-dispatcher:latest
          env:
            - name: APP_ENV
              value: prod
            - name: DATABASE_URL
              valueFrom:
                secretKeyRef:
                  name: outbox-dispatcher-secrets
                  key: database-url
            - name: ADMIN_TOKEN
              valueFrom:
                secretKeyRef:
                  name: outbox-dispatcher-secrets
                  key: admin-token
          ports:
            - containerPort: 9090
              name: admin
            - containerPort: 9091
              name: metrics
          livenessProbe:
            httpGet:
              path: /health
              port: admin
            initialDelaySeconds: 5
            periodSeconds: 10
          readinessProbe:
            httpGet:
              path: /ready
              port: admin
            initialDelaySeconds: 10
            periodSeconds: 10
          volumeMounts:
            - name: config
              mountPath: /app/envs/app_config_prod.toml
              subPath: app_config_prod.toml
              readOnly: true
      volumes:
        - name: config
          configMap:
            name: outbox-dispatcher-config
```

## Upgrading

1. Pull the new image (or binary).
2. The dispatcher validates that the database schema version is not newer than the binary. If it is, startup is refused — upgrade the binary first.
3. Deploy the new binary/image. Migrations run automatically on startup.
4. For zero-downtime: run old and new replicas concurrently during rollout. The schema is forward-compatible within a major version.
