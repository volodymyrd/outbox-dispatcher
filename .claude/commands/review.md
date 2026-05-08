Review the code in $ARGUMENTS (or the most recently edited file if none specified) for quality, correctness, and production-readiness in this Rust + Tokio + sqlx + Axum (admin) outbox-dispatcher codebase.

## Checklist

### Rust Idioms
- [ ] No `unwrap()` / `expect()` in library code — only allowed in `crates/bin/src/main.rs` startup or where an invariant is documented inline (e.g. `HmacSha256::new_from_slice` accepts any key length)
- [ ] No `clone()` calls that could be avoided with references or `Arc`
- [ ] Use `?` operator instead of manual `match` on `Result` / `Option`
- [ ] Prefer `if let` / `while let` over explicit `match` for single-arm patterns
- [ ] Iterators preferred over manual `for` loops with `push`
- [ ] No unnecessary `collect()` before immediately iterating
- [ ] Library errors use `thiserror`; binary uses `anyhow` with `.with_context(...)`
- [ ] `#[derive(Debug, Clone)]` only where semantically correct; never derive `Debug` on types holding secrets
- [ ] Prefer `From` / `Into` impls over explicit conversion functions
- [ ] Comments only for non-obvious logic — self-documenting code preferred (per CLAUDE.md)

### Async / Tokio
- [ ] No blocking calls inside `async fn` (sync file I/O, `std::thread::sleep`, blocking HTTP clients)
- [ ] No locks held across `.await` points
- [ ] `tokio::sync` primitives in async code, not `std::sync::Mutex` / `RwLock`
- [ ] `Arc<T>` for shared immutable state; `tokio::sync::RwLock` only when readers dominate
- [ ] Long-running loops use `tokio::select!` with cancellation, not unbounded sleeps

### Database (sqlx + Postgres)
- [ ] All queries use sqlx compile-time macros (`sqlx::query!`, `query_as!`, `query_scalar!`) — no raw string queries except for one-off DDL such as `pg_advisory_lock`
- [ ] `fetch_optional` for 0-or-1 rows; `fetch_one` only when a row is guaranteed
- [ ] Multi-step mutations wrapped in explicit transactions (`pool.begin()` → `tx.commit()`)
- [ ] `.sqlx/` query cache regenerated after any query change (`cargo sqlx prepare --workspace`)
- [ ] Session-bound state (advisory locks, `SET LOCAL`, `LISTEN`) acquired and released on the **same** `PgConnection`, not via `&PgPool` (which checks out a random connection per call)
- [ ] No N+1 queries — related rows fetched in a single SQL with JOIN, CTE, or `IN (...)`

### Migrations
- [ ] Filename format `NNNN_description.sql` (sqlx integer prefix, **not** Flyway `V001__`)
- [ ] No manual `pg_advisory_lock` wrapping `MIGRATOR.run()` — sqlx serialises internally on its own connection
- [ ] Downgrade guard tolerates a missing `_sqlx_migrations` table (Postgres SQLSTATE `42P01`) by treating max version as `None`, so `--skip-migrations` works on a fresh DB
- [ ] DDL is idempotent where reasonable (`CREATE TABLE IF NOT EXISTS`, `CREATE INDEX IF NOT EXISTS`)

### Outbox Semantics
- [ ] `locked_until` is set and committed **before** the HTTP call, not after — prevents duplicate dispatch across replicas
- [ ] `signing_key_id` resolved at **dispatch time**, not schedule time — tolerates deploy skew
- [ ] `CallbackError` only has the `Transient` variant; permanent failure is encoded via `max_attempts` exhaustion → dead-letter
- [ ] Backoff retry index uses `attempt.max(1) as usize - 1` — never `(attempt - 1) as usize` directly (silent wrap to `usize::MAX` if `attempt == 0`)
- [ ] ±25 % jitter applied to every backoff delay
- [ ] `Retry-After` header treated as a **floor**, not an absolute override
- [ ] `available_at` / `locked_until` comparisons use `chrono::DateTime<Utc>`, not naive `SystemTime`

### HMAC Signing (`crates/http-callback/src/signing.rs`)
- [ ] HMAC fed the **raw body bytes** via `mac.update(body)` — never `String::from_utf8_lossy(body)` (mutates non-UTF-8 with U+FFFD) or `format!("{ts}.{body}")` (allocates the full payload)
- [ ] Verification uses `mac.verify_slice(...)` for constant-time comparison — never `==` on hex strings
- [ ] Secrets and signing keys never logged (no `Debug` derive on key types, or sensitive fields masked)

### Repo Trait
- [ ] `Repo` stays object-safe (`Arc<dyn Repo>` must compile) — no generic methods, all `async` via `#[async_trait]`
- [ ] Mocks (`#[automock]` / `mockall`) used in unit tests — no live Postgres
- [ ] Integration tests use `testcontainers` for a real ephemeral Postgres

### Testing
- [ ] Every public function has at least one test
- [ ] Error branches (NotFound, transient failure, lock contention, schema mismatch, signature mismatch) tested
- [ ] Test names follow `<function>_<scenario>` convention
- [ ] Target >90 % coverage per module (per CLAUDE.md)

### Configuration & Validation
- [ ] No magic numbers / hardcoded strings that belong in config
- [ ] `AppConfig::validate()` covers new fields (empty strings, zero counts, payload-size minimums)
- [ ] Sensitive config (DSNs, secrets) read from env, never committed in YAML

## Output Format

For each issue found:
1. **File:Line** — exact location
2. **Severity** — `Critical` / `High` / `Medium` / `Low`
3. **Category** — (Security | Correctness | Concurrency | Performance | Idiom | Migration | Testing | Config)
4. **Finding** — what the problem is
5. **Fix** — the idiomatic Rust solution with a code snippet

End with a summary table of findings by severity.

## Report File

After completing the review, **always** write a report file, even if there are no findings.

### Gather context first

Before writing, run:
```bash
git branch --show-current
date -u +"%Y-%m-%dT%H:%M:%SZ"
```

### File path

Create the `code-review/` directory at the workspace root if it does not exist, then write:

```
code-review/YYYY-MM-DD_<branch>_<target-slug>.md
```

- `YYYY-MM-DD` — today's UTC date
- `<branch>` — current git branch name with `/` replaced by `-`
- `<target-slug>` — the reviewed file or scope: base filename without extension, or `workspace` when reviewing the full workspace

Example: `code-review/2025-05-08_phase2_callbacks.md`

### Report structure

Use **exactly** this template:

```markdown
# Code Review — <target>

**Date:** <ISO-8601 UTC datetime>
**Branch:** <branch>
**Reviewed by:** Claude (review command)
**Scope:** <file path(s) or "full workspace">

---

## Findings

<!-- One section per finding. Omit section entirely if no findings. -->

### Finding <N> — <short title>

| Field | Value |
|-------|-------|
| **File:Line** | `path/to/file.rs:42` |
| **Severity** | Critical / High / Medium / Low |
| **Category** | Security / Correctness / Concurrency / Performance / Idiom / Migration / Testing / Config |

**Problem**

<One or two sentences describing what is wrong and why it matters.>

**Context** (surrounding code as it exists today)

```rust
// file.rs lines 38-48
<exact existing code excerpt — enough for an LLM to locate and understand the problem>
```

**Recommended fix**

```rust
<complete corrected replacement — not a diff, the full new form of the changed lines>
```

**Why this fix**

<One sentence explaining the Rust/project reasoning behind the recommendation.>

---

<!-- repeat for each finding -->

## Summary

| # | Title | File:Line | Severity | Category | Status | Notes |
|---|-------|-----------|----------|----------|--------|-------|
| 1 | <short title> | `path/file.rs:42` | Critical | Correctness | TODO | |
| 2 | <short title> | `path/file.rs:88` | High | Idiom | TODO | |

> **Instructions for the implementing LLM:**
> - Change `TODO` to `DONE` once a finding is fully addressed.
> - If a finding is intentionally skipped or cannot be applied, change `TODO` to `SKIPPED` and add a short reason in the **Notes** column.
> - Do not delete rows — the table is the authoritative implementation log.
```

### When there are no findings

Still write the file. Use an empty `## Findings` section with a note:

```markdown
## Findings

No issues found.
```

And a summary table with a single row:

```markdown
| # | Title | File:Line | Severity | Category | Status | Notes |
|---|-------|-----------|----------|----------|--------|-------|
| — | No findings | — | — | — | DONE | All checklist items passed |
```

## Mandatory Post-Change Steps

After applying **every** fix, run these commands in order and resolve all issues before finishing:

```bash
cargo fmt --all
cargo check --workspace
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

If any `Cargo.toml` dependency was added or removed:
```bash
cargo sort --workspace
```

If any sqlx query macro was added or changed:
```bash
DATABASE_URL=postgres://outbox:outbox@localhost:5434/outbox_dispatcher cargo sqlx prepare --workspace
```

Do not report the module as done until all commands exit cleanly and all tests pass.
