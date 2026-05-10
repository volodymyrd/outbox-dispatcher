# Code Review — PR #4 (Phase 4 dispatch loop + HTTP callback)

**Date:** 2026-05-10T14:50:36Z
**Branch:** phase4
**Reviewed by:** Claude (review command)
**Scope:** PR #4 — `crates/core/src/{dispatch,scheduler,timeout_sweep}.rs`, `crates/http-callback/src/client.rs`, `crates/bin/src/main.rs`

---

## Findings

### Finding 1 — `HttpCallback::new` returns `anyhow::Result` from a library crate

| Field | Value |
|-------|-------|
| **File:Line** | `crates/http-callback/src/client.rs:54` |
| **Severity** | High |
| **Category** | Idiom |

**Problem**

`CLAUDE.md` mandates `thiserror` for library-crate error types and `anyhow` only in `crates/bin`. `HttpCallback::new` is in a library crate (`outbox-dispatcher-http-callback`) yet exposes `anyhow::Result` and a string-built `anyhow::anyhow!`. This forces every caller (including future library consumers) to depend on `anyhow` and loses error typing. The project already has `thiserror`-based `core::Error`; the binary still gets nice context via `.with_context(...)` because `anyhow::Error: From<E> where E: std::error::Error`.

**Context** (surrounding code as it exists today)

```rust
// crates/http-callback/src/client.rs lines 52-72
impl HttpCallback {
    /// Build an `HttpCallback` from the HTTP client config and the loaded keyring.
    pub fn new(cfg: &HttpClientConfig, keyring: Arc<KeyRing>) -> anyhow::Result<Self> {
        let mut builder = ClientBuilder::new()
            .user_agent(&cfg.user_agent)
            .connect_timeout(Duration::from_secs(cfg.connect_timeout_secs))
            .redirect(reqwest::redirect::Policy::none()); // §6.3: redirects are failures

        if cfg.allow_insecure_tls {
            builder = builder.danger_accept_invalid_certs(true);
        }

        let client = builder
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build reqwest client: {e}"))?;
        Ok(Self {
            client,
            keyring: keyring.clone(),
        })
    }
}
```

**Recommended fix**

Define a `thiserror` error type local to the crate (or re-use one) and drop the `anyhow` dep:

```rust
// crates/http-callback/src/error.rs (new)
#[derive(Debug, thiserror::Error)]
pub enum HttpCallbackError {
    #[error("failed to build reqwest client: {0}")]
    ClientBuild(#[from] reqwest::Error),
}

// crates/http-callback/src/client.rs
impl HttpCallback {
    pub fn new(
        cfg: &HttpClientConfig,
        keyring: Arc<KeyRing>,
    ) -> Result<Self, HttpCallbackError> {
        let mut builder = ClientBuilder::new()
            .user_agent(&cfg.user_agent)
            .connect_timeout(Duration::from_secs(cfg.connect_timeout_secs))
            .redirect(reqwest::redirect::Policy::none());

        if cfg.allow_insecure_tls {
            builder = builder.danger_accept_invalid_certs(true);
        }

        let client = builder.build()?;
        Ok(Self { client, keyring })
    }
}
```

Then remove `anyhow = { workspace = true }` from `crates/http-callback/Cargo.toml`.

**Why this fix**

Library crates expose typed errors so callers can pattern-match; the binary still gets free `.context(...)` via the `?` → `anyhow::Error` conversion, satisfying both CLAUDE.md and ergonomic startup-error reporting in `bin/main.rs:122`.

---

### Finding 2 — Dead-code `keyring_with` helper trips `clippy --deny warnings`

| Field | Value |
|-------|-------|
| **File:Line** | `crates/http-callback/src/client.rs:262-280` |
| **Severity** | Medium |
| **Category** | Testing |

**Problem**

`keyring_with` is defined in `#[cfg(test)] mod tests` but never called by any test. `cargo clippy --workspace --all-targets` emits `warning: function keyring_with is never used`, which the project's mandatory post-change check (`cargo clippy --workspace -- -D warnings`) treats as a hard fail. The CI pipeline (Phase 8) will block on this. Also: `make_event` returns a `(EventForDelivery, CallbackTarget)` pair but no test exercises the `CallbackTarget` path through `deliver`, so the helper is half-wired.

**Context** (surrounding code as it exists today)

```rust
// crates/http-callback/src/client.rs lines 262-281
fn keyring_with(key_id: &str, secret_env: &str, secret_val: &str) -> Arc<KeyRing> {
    // Set env var, build keyring, unset env var.
    let env_key = secret_env.to_string();
    unsafe {
        std::env::set_var(&env_key, secret_val);
    }
    let mut keys = HashMap::new();
    keys.insert(
        key_id.to_string(),
        SigningKeyConfig {
            secret_env: secret_env.to_string(),
        },
    );
    let kr = KeyRing::load(&keys).unwrap();
    unsafe {
        std::env::remove_var(&env_key);
    }
    Arc::new(kr)
}
```

**Recommended fix**

Either delete the helper entirely, or wire it into a real `deliver()` test. The bigger gap is the missing HTTP test (see Finding 4); fixing that test would consume `keyring_with`. Minimum fix to unblock CI:

```rust
// Delete lines 262-280 of crates/http-callback/src/client.rs entirely,
// along with the unused `SigningKeyConfig` import in the tests module's
// `use outbox_dispatcher_core::{...};` line.
```

**Why this fix**

`-D warnings` mandates zero warnings; dead test helpers should be deleted unless they're about to be used.

---

### Finding 3 — Redundant `Arc::clone` in `HttpCallback::new`

| Field | Value |
|-------|-------|
| **File:Line** | `crates/http-callback/src/client.rs:69` |
| **Severity** | Low |
| **Category** | Idiom |

**Problem**

`keyring` is taken by-value as `Arc<KeyRing>` but stored via `keyring: keyring.clone()`, bumping the refcount unnecessarily. Direct field-shorthand move suffices.

**Context** (surrounding code as it exists today)

```rust
// crates/http-callback/src/client.rs lines 64-71
        let client = builder
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build reqwest client: {e}"))?;
        Ok(Self {
            client,
            keyring: keyring.clone(),
        })
    }
```

**Recommended fix**

```rust
        let client = builder.build()?;
        Ok(Self { client, keyring })
    }
```

**Why this fix**

`keyring` is owned by the function; no other reference is held, so the explicit `.clone()` does an avoidable atomic increment. Field-shorthand also reads cleaner.

---

### Finding 4 — No HTTP-level test of `Callback::deliver`

| Field | Value |
|-------|-------|
| **File:Line** | `crates/http-callback/src/client.rs:282-314` (test module) |
| **Severity** | High |
| **Category** | Testing |

**Problem**

The Phase 4 milestone in TDD §16 requires verifying:
- 2xx success path sets timestamps
- 503 retries with correct backoff
- `attempts ≥ max_attempts` dead-letters
- timeout path is exercised end-to-end
- mid-dispatch crash leaves a row recovered via `locked_until` expiry
- HMAC signature header correctness, redirect rejection (§6.3), and `Retry-After` parsing

The current tests cover only `build_body` and a string-parse stub of `Retry-After`. The actual `deliver()` function — including header emission (§6.1), HMAC signing on the wire, status interpretation (§6.3), redirect policy, and `Retry-After` extraction — has **zero** automated coverage. CLAUDE.md sets a >90 % per-module target; this module is at ~30 %.

**Context** (surrounding code as it exists today)

```rust
// crates/http-callback/src/client.rs lines 282-314
    #[test]
    fn build_body_includes_required_fields() {
        let (event, _) = make_event(42, None);
        let body = build_body(&event);
        assert_eq!(body["delivery_id"], 42);
        // ...
    }

    #[test]
    fn unknown_signing_key_returns_transient_error() {
        // We can't call deliver() in a sync test, but we can test the keyring lookup
        // directly: an unknown key_id should not be found in an empty keyring.
        let keyring = empty_keyring();
        assert!(keyring.get("nonexistent-key").is_none());
    }

    #[test]
    fn extract_retry_after_parses_integer_seconds() {
        // ... we verify the parsing logic directly.
        let secs_str = "120";
        let parsed: Option<u64> = secs_str.trim().parse().ok();
        assert_eq!(parsed, Some(120u64));
    }
```

**Recommended fix**

Add `wiremock = "0.6"` (or `mockito`) to `[dev-dependencies]` and add tokio tests that exercise `HttpCallback::deliver` against a fake HTTP server:

```rust
// crates/http-callback/src/client.rs (sketch)
#[tokio::test]
async fn deliver_sends_required_headers_and_signature() {
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/hook"))
        .and(matchers::header_exists("X-Outbox-Event-Id"))
        .and(matchers::header_exists("X-Outbox-Signature"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let cfg = HttpClientConfig::default();
    let kr = keyring_with("k1", "TEST_SECRET_ENV", "QUFB...padding...="); // 32-byte
    let cb = HttpCallback::new(&cfg, kr).unwrap();

    let (mut event, mut target) = make_event(1, Some("k1"));
    target.url = format!("{}/hook", server.uri());
    cb.deliver(&target, &event).await.expect("expected 2xx");
}

#[tokio::test]
async fn deliver_returns_transient_with_retry_after_on_429() { /* ... */ }

#[tokio::test]
async fn deliver_returns_transient_on_redirect() { /* ... */ }
```

**Why this fix**

Without HTTP-level tests we have no automated guarantee that the on-the-wire shape matches §6.1 / §6.3 — exactly the contract receivers depend on. `wiremock` runs entirely in-process and unblocks the Phase 4 E2E milestone listed in TDD §16.

---

### Finding 5 — `extract_retry_after` ignores HTTP-date form

| Field | Value |
|-------|-------|
| **File:Line** | `crates/http-callback/src/client.rs:201-209` |
| **Severity** | Low |
| **Category** | Correctness |

**Problem**

§8.5 / TDD says **`Retry-After` honoured as a floor**. The current parser only accepts integer seconds; for the HTTP-date form (RFC 7231 §7.1.3 — e.g. `Retry-After: Wed, 21 Oct 2026 07:28:00 GMT`), it returns `None`, which means the receiver's request is silently ignored and we fall through to the standard backoff. Receivers using gateways that emit HTTP-date Retry-After (CloudFront, Apache `mod_evasive`) won't have the floor honoured.

**Context** (surrounding code as it exists today)

```rust
// crates/http-callback/src/client.rs lines 197-209
/// Parse a `Retry-After` header value into a `Duration`.
///
/// Supports both integer seconds (most common) and the HTTP date form.
/// Returns `None` if the header is absent or unparseable.
fn extract_retry_after(response: &reqwest::Response) -> Option<Duration> {
    let value = response.headers().get("Retry-After")?.to_str().ok()?;
    // Try integer seconds first.
    if let Ok(secs) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    // HTTP date parsing is not implemented here; return None for date form.
    None
}
```

**Recommended fix**

Add a small RFC 7231 date parser via the `httpdate` crate (very thin, no transitive deps), and clamp negative deltas to zero:

```rust
fn extract_retry_after(response: &reqwest::Response) -> Option<Duration> {
    let value = response.headers().get("Retry-After")?.to_str().ok()?;
    let trimmed = value.trim();
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    // RFC 7231 §7.1.3 HTTP-date form. Negative deltas (date in the past) collapse to zero.
    let target = httpdate::parse_http_date(trimmed).ok()?;
    target.duration_since(SystemTime::now()).ok()
}
```

Either drop the `Supports both integer seconds (most common) and the HTTP date form.` claim from the doc comment, or add `httpdate = "1"` and implement.

**Why this fix**

The current doc-comment contradicts the implementation; either align the implementation with the documented behaviour or align the doc to the actual behaviour. Honouring HTTP-date form costs nothing for receivers that emit integer seconds and protects against misbehaving with the small fraction that emit dates.

---

### Finding 6 — Dead `if … { return Ok(()) } Ok(())` branch in `sweep_hung_external`

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/timeout_sweep.rs:42-46` |
| **Severity** | Low |
| **Category** | Idiom |

**Problem**

Both branches return `Ok(())`. The `if report.reset == 0 && report.exhausted == 0 { return Ok(()); }` is dead control flow that just makes the function harder to read. There is no early-out side effect.

**Context** (surrounding code as it exists today)

```rust
// crates/core/src/timeout_sweep.rs lines 23-47
pub async fn sweep_hung_external(repo: &dyn Repo, config: &DispatchConfig) -> Result<()> {
    let report = repo
        .reset_hung_external(Utc::now(), config.max_completion_cycles as i32)
        .await?;

    if report.reset > 0 {
        info!(
            reset = report.reset,
            "external timeout sweep reset deliveries for redelivery"
        );
    }

    if report.exhausted > 0 {
        warn!(
            exhausted = report.exhausted,
            "external timeout sweep dead-lettered rows after max_completion_cycles"
        );
    }

    if report.reset == 0 && report.exhausted == 0 {
        return Ok(());
    }

    Ok(())
}
```

**Recommended fix**

```rust
pub async fn sweep_hung_external(repo: &dyn Repo, config: &DispatchConfig) -> Result<()> {
    let report = repo
        .reset_hung_external(Utc::now(), config.max_completion_cycles as i32)
        .await?;

    if report.reset > 0 {
        info!(
            reset = report.reset,
            "external timeout sweep reset deliveries for redelivery"
        );
    }
    if report.exhausted > 0 {
        warn!(
            exhausted = report.exhausted,
            "external timeout sweep dead-lettered rows after max_completion_cycles"
        );
    }
    Ok(())
}
```

**Why this fix**

Dead control flow violates the CLAUDE.md guidance: "Don't add features, refactor, or introduce abstractions beyond what the task requires." Returning `Ok(())` once at the end matches the rest of the codebase's style.

---

### Finding 7 — `dispatch_due` has no concurrency cap; can starve the pg pool

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/dispatch.rs:61-67` |
| **Severity** | Medium |
| **Category** | Concurrency |

**Problem**

`futures::future::join_all` drives all `batch_size` deliveries concurrently. Each `dispatch_one` makes 2-3 DB round trips (`lock_delivery`, then `mark_*` or `mark_failure`), each acquired from the pool. With the default `batch_size = 50` and `database.max_connections` typically 10-25, a slow callback that holds the pool's last connection while another 49 dispatches all hit `lock_delivery` simultaneously can exhaust the pool and trip `acquire_timeout_secs` (default 10 s), turning into a cascade of false `lock_delivery` failures even though the rows themselves are fine.

There is also no documented invariant in the config (`AppConfig::validate`) that `database.max_connections >= batch_size`, so an operator can configure a pathological combination at startup and only see the problem under load.

**Context** (surrounding code as it exists today)

```rust
// crates/core/src/dispatch.rs lines 49-68
pub async fn dispatch_due(
    repo: &dyn Repo,
    callback: &dyn Callback,
    config: &DispatchConfig,
) -> Result<()> {
    let due = repo.fetch_due_deliveries(config.batch_size).await?;
    if due.is_empty() {
        return Ok(());
    }

    debug!(count = due.len(), "dispatching due deliveries");

    let futures: Vec<_> = due
        .into_iter()
        .map(|d| dispatch_one(repo, callback, config, d))
        .collect();

    futures::future::join_all(futures).await;
    Ok(())
}
```

**Recommended fix**

Cap concurrency with `FuturesUnordered` or `buffer_unordered`, and add a config validation guard. The simplest patch:

```rust
use futures::stream::{StreamExt, FuturesUnordered};

pub async fn dispatch_due(
    repo: &dyn Repo,
    callback: &dyn Callback,
    config: &DispatchConfig,
) -> Result<()> {
    let due = repo.fetch_due_deliveries(config.batch_size).await?;
    if due.is_empty() {
        return Ok(());
    }

    debug!(count = due.len(), "dispatching due deliveries");

    let mut tasks = futures::stream::iter(
        due.into_iter()
            .map(|d| dispatch_one(repo, callback, config, d)),
    )
    .buffer_unordered(config.dispatch_concurrency.max(1));

    while tasks.next().await.is_some() {}
    Ok(())
}
```

Then add `dispatch_concurrency: usize` to `DispatchConfig` (default = `min(batch_size, max_connections / 2)`) and a `validate()` rule pinning the relationship. As an interim fix, document in `CLAUDE.md` and `envs/app_config.toml` that `database.max_connections` should be `>= batch_size + headroom`.

**Why this fix**

Decoupling dispatch parallelism from `batch_size` lets operators tune throughput without enlarging fetch granularity, and avoids the failure mode where a single slow webhook starves all subsequent dispatches of DB connections.

---

### Finding 8 — Non-dead transient failures are written to DB without a log line

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/dispatch.rs:155-185` |
| **Severity** | Low |
| **Category** | Idiom |

**Problem**

When a callback returns `Transient` and we are *not* dead-lettering (i.e. `next_attempt < max_attempts`), the code writes the failure to the DB but emits no log. Operators tailing logs see only the dead-letter `warn!` and the `error!` from `mark_failure` write failures — there is no trace of the underlying webhook failure that triggered the retry. This makes triaging "why is my callback retrying constantly?" require a DB query.

**Context** (surrounding code as it exists today)

```rust
// crates/core/src/dispatch.rs lines 156-185
        Ok(Err(CallbackError::Transient {
            reason,
            retry_after,
        })) => {
            let next_attempt = due.attempts + 1;
            let next_available =
                compute_next_available_at(next_attempt, retry_after, &due.target.backoff);
            let dead = next_attempt >= due.target.max_attempts as i32;

            if dead {
                warn!(
                    delivery_id = due.delivery_id,
                    attempts = next_attempt,
                    max_attempts = due.target.max_attempts,
                    reason = %reason,
                    "delivery exhausted retries — dead-lettering"
                );
            }

            if let Err(e) = repo
                .mark_failure(due.delivery_id, &reason, next_available, dead)
                .await
            {
                error!(
                    delivery_id = due.delivery_id,
                    error = %e,
                    "failed to record delivery failure"
                );
            }
        }
```

**Recommended fix**

Always log at `debug!` (or `info!`) for transient failures, keeping the `warn!` dead-letter line as-is:

```rust
        Ok(Err(CallbackError::Transient {
            reason,
            retry_after,
        })) => {
            let next_attempt = due.attempts + 1;
            let next_available =
                compute_next_available_at(next_attempt, retry_after, &due.target.backoff);
            let dead = next_attempt >= due.target.max_attempts as i32;

            if dead {
                warn!(
                    delivery_id = due.delivery_id,
                    attempts = next_attempt,
                    max_attempts = due.target.max_attempts,
                    reason = %reason,
                    "delivery exhausted retries — dead-lettering"
                );
            } else {
                debug!(
                    delivery_id = due.delivery_id,
                    attempts = next_attempt,
                    reason = %reason,
                    "delivery transient failure; will retry"
                );
            }

            if let Err(e) = repo
                .mark_failure(due.delivery_id, &reason, next_available, dead)
                .await
            {
                error!(
                    delivery_id = due.delivery_id,
                    error = %e,
                    "failed to record delivery failure"
                );
            }
        }
```

**Why this fix**

Symmetry with the timeout branch (which already logs `"callback timed out"`) and a tracing trail per attempt is essential for production debugging once Phase 7 ships OpenTelemetry spans and `dispatch.{callback_name}` per §12.3.

---

### Finding 9 — Scheduler `consecutive_errors` doc-comment is now misleading

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/scheduler.rs:83-85` |
| **Severity** | Low |
| **Category** | Idiom |

**Problem**

The comment claims `consecutive_errors` tracks "consecutive scheduling/dispatch failures", but in the current code it is only mutated inside the `schedule_new_deliveries` match arms. Errors from `dispatch_due` and `sweep_hung_external` are logged and dropped without affecting the counter or the backoff. Either align the comment with the actual scope, or extend the counter to cover dispatch/sweep failures too.

**Context** (surrounding code as it exists today)

```rust
// crates/core/src/scheduler.rs lines 82-85, 137-159
    // Tracks consecutive scheduling/dispatch failures so we can apply a capped backoff
    // and avoid log-spamming at full speed during a sustained DB outage.
    let mut consecutive_errors: u32 = 0;
    // ... only mutated by schedule_new_deliveries below ...
```

**Recommended fix**

Either tighten the comment:

```rust
    // Tracks consecutive **scheduling** failures so we can apply a capped backoff
    // and avoid log-spamming at full speed during a sustained DB outage.
    // Dispatch / sweep errors are logged but do not contribute to the backoff because
    // dispatch_one already swallows per-row failures and the sweeper is rate-limited.
    let mut consecutive_errors: u32 = 0;
```

…or extend `consecutive_errors` to also track dispatch/sweep failures (a heavier change). The narrower comment fix matches the current behaviour and is the minimum acceptable patch.

**Why this fix**

Comments that misdescribe behaviour rot fastest and confuse future maintainers reasoning about the wake loop's failure semantics.

---

## Summary

| # | Title | File:Line | Severity | Category | Status | Notes |
|---|-------|-----------|----------|----------|--------|-------|
| 1 | `HttpCallback::new` returns `anyhow` from a library crate | `crates/http-callback/src/client.rs:54` | High | Idiom | TODO | |
| 2 | Dead-code `keyring_with` test helper trips `-D warnings` | `crates/http-callback/src/client.rs:262-280` | Medium | Testing | TODO | |
| 3 | Redundant `Arc::clone` in `HttpCallback::new` | `crates/http-callback/src/client.rs:69` | Low | Idiom | TODO | |
| 4 | No HTTP-level test of `Callback::deliver` | `crates/http-callback/src/client.rs:282-314` | High | Testing | TODO | |
| 5 | `extract_retry_after` ignores HTTP-date form | `crates/http-callback/src/client.rs:201-209` | Low | Correctness | TODO | |
| 6 | Dead `if … Ok(())` branch in `sweep_hung_external` | `crates/core/src/timeout_sweep.rs:42-46` | Low | Idiom | TODO | |
| 7 | `dispatch_due` has no concurrency cap; can starve pg pool | `crates/core/src/dispatch.rs:61-67` | Medium | Concurrency | TODO | |
| 8 | Non-dead transient failures logged silently | `crates/core/src/dispatch.rs:155-185` | Low | Idiom | TODO | |
| 9 | Scheduler `consecutive_errors` comment is now misleading | `crates/core/src/scheduler.rs:83-85` | Low | Idiom | TODO | |

> **Instructions for the implementing LLM:**
> - Change `TODO` to `DONE` once a finding is fully addressed.
> - If a finding is intentionally skipped or cannot be applied, change `TODO` to `SKIPPED` and add a short reason in the **Notes** column.
> - Do not delete rows — the table is the authoritative implementation log.
