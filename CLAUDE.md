# Outbox Dispatcher

## Project

**outbox-dispatcher** — a standalone Rust microservice that turns a Postgres outbox table into a reliable HTTP delivery
service.

Design document: `../outbox/TDDs/04-outbox-dispatcher-tdd.md `

## Workspace layout

```
outbox-dispatcher/
├── Cargo.toml                    # workspace root
├── migrations/
│   └── 0001_initial_schema.sql   # V1 schema (both tables, indexes, pg_notify trigger)
├── crates/
│   ├── core/          (outbox-dispatcher-core)        — library: Repo trait, PgRepo, schema types, retry, config
│   ├── http-callback/ (outbox-dispatcher-http-callback) — HTTP webhook delivery + HMAC signing
│   ├── admin-api/     (outbox-dispatcher-admin-api)   — axum admin HTTP server
│   └── bin/           (outbox-dispatcher)             — binary: wires everything, migration runner
```

## Mandatory After Every Code Change

Run in this order after **every** edit — fix all issues before moving on:

```bash
cargo fmt --all
cargo check --workspace
cargo clippy --workspace
cargo test --workspace
```

If a `Cargo.toml` dependency was added or removed:

```bash
cargo sort --workspace
```

## Key source files

| File                                  | Purpose                                                                |
|---------------------------------------|------------------------------------------------------------------------|
| `crates/core/src/repo.rs`             | `Repo` trait definition + full `PgRepo` impl                           |
| `crates/core/src/schema.rs`           | All public data types (`RawEvent`, `CallbackTarget`, `DueDelivery`, …) |
| `crates/core/src/retry.rs`            | `compute_next_available_at` (backoff + jitter)                         |
| `crates/http-callback/src/signing.rs` | HMAC-SHA256 `sign` + constant-time `verify`                            |
| `crates/bin/src/main.rs`              | CLI entry point, migration runner, service bootstrap                   |
| `migrations/0001_initial_schema.sql`  | Complete V1 schema                                                     |

## Local database (Docker)

```bash
docker compose up -d                    # start Postgres on port 5434
docker compose down                     # stop
```

Credentials are in `.env` (gitignored). Copy from `.env.example` if needed:
`DATABASE_URL=postgres://outbox:outbox@localhost:5434/outbox_dispatcher`

## Running migrations

```bash
DATABASE_URL=postgres://outbox:outbox@localhost:5434/outbox_dispatcher cargo run -- migrate
DATABASE_URL=postgres://outbox:outbox@localhost:5434/outbox_dispatcher cargo run
DATABASE_URL=postgres://outbox:outbox@localhost:5434/outbox_dispatcher cargo run -- --skip-migrations
cargo run -- migrations dump            # print embedded SQL to stdout
RUST_LOG=debug cargo run               # verbose logging
```

## sqlx offline mode

The `.sqlx/` directory contains cached query metadata and is checked into version control.
Builds without `DATABASE_URL` use it automatically (`SQLX_OFFLINE=true`).

After adding or changing any sqlx query macro, regenerate the cache:

```bash
DATABASE_URL=postgres://outbox:outbox@localhost:5434/outbox_dispatcher cargo sqlx prepare --workspace
```

## Integration tests (Phases 2+)

Integration tests use `testcontainers` to spin up a real Postgres instance per test — no manual
setup needed, but Docker must be running.

```bash
# Docker must be running
cargo test --test '*'
```

## Implementation phases

| Phase | Status          | Description                                                                       |
|-------|-----------------|-----------------------------------------------------------------------------------|
| 1     | **In Progress** | Workspace scaffold, migration SQL, `Repo` trait, full `PgRepo`, migration runner  |
| 2     | Todo            | YAML config parsing, domain types, `parse_callbacks`, payload-size helper         |
| 3     | Todo            | `LISTEN/NOTIFY` wake source, cursor recovery, scheduler                           |
| 4     | Todo            | `Callback` trait, `HttpCallback`, managed dispatch, HMAC signing at dispatch time |
| 5     | Todo            | External mode, completion sweeper                                                 |
| 6     | Todo            | Admin HTTP API (axum)                                                             |
| 7     | Todo            | Prometheus metrics, structured logging, OpenTelemetry, retention worker           |
| 8     | Todo            | Docker, CI/CD, docs                                                               |

## Key design notes

- Migration filename format: `NNNN_description.sql` (sqlx integer prefix, **not** Flyway `V001__`).
- `Repo` trait is object-safe (`Arc<dyn Repo>`). All methods are `async` via `async-trait`.
- `CallbackError` has **only** the `Transient` variant — no permanent failures.
- `signing_key_id` is resolved at **dispatch time**, not schedule time (tolerates deploy skew).
- `locked_until` prevents duplicate dispatch across replicas; committed before the HTTP call.
- **sqlx offline mode**: `.sqlx/` cache is checked in; builds work without `DATABASE_URL`. Regenerate after any query change with `cargo sqlx prepare --workspace`.
- **Migration locking**: `MIGRATOR.run()` already takes its own Postgres advisory lock internally on its own connection. Do **not** wrap it manually with `pg_advisory_lock` via `.execute(pool)` — the pool hands out a different connection each call, so the lock is held on a connection that no longer participates in the migration.
- **Downgrade guard**: `validate_schema` queries `_sqlx_migrations`; treat Postgres SQLSTATE `42P01` (undefined_table) as "no migrations applied" so `--skip-migrations` works on a fresh DB.
- **HMAC body bytes**: feed the raw payload to HMAC via `mac.update(body)`. Never `String::from_utf8_lossy(body)` (mutates non-UTF-8 with U+FFFD) or `format!("{ts}.{body}")` (allocates the full payload). Stream `format!("{ts}.")` then `body`.
- **Retry index guard**: clamp with `attempt.max(1) as usize - 1` before indexing the backoff schedule — `(attempt - 1) as usize` silently wraps to `usize::MAX` if `attempt == 0` and pegs to the longest backoff.
- **Session-bound state**: advisory locks, `SET LOCAL`, and `LISTEN` are tied to the underlying `PgConnection`. Acquire from `pool.acquire()` and reuse the same `&mut conn` for the whole sequence — never `.execute(pool)` in pieces.

## Code Conventions

### Comments

- Use comments sparingly — only for complex or non-obvious logic; self-documenting code is preferred

### Errors

- Use `thiserror` for all error types in library crates (`core`, `http-callback`, `admin-api`)
- Use `anyhow` in `crates/bin` for startup-flow errors; attach context with `.with_context(|| ...)` rather than bare `?`

### Async / Tokio

- No blocking calls inside `async fn` — no sync file I/O, no `std::thread::sleep`, no blocking HTTP clients
- Never hold a lock across an `.await` point
- Use `tokio::sync` primitives in async code, not `std::sync::Mutex` / `RwLock`

### Database

- All queries use SQLx compile-time macros (`sqlx::query!`, `sqlx::query_as!`, `sqlx::query_scalar!`)

### Logging

- Use `tracing` macros: `debug!` for request details, `info!` for business events, `warn!` for permission denials,
  `error!` for failures

### Testing

- Unit tests use `mockall` (mock traits via `#[automock]`)
- Target >90% coverage per module
- Test both happy path AND all error branches
- No live DB in unit tests — mock the repo trait

