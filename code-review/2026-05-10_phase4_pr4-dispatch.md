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

## Second-Pass Findings (post-rework on commit 580b7f7)

All nine findings above were verified addressed by reading the current code. The
second pass uncovered the following additional issues, scoped to changes
introduced in PR #4.

---

### Finding 10 — `dispatch_concurrency` is not validated in `AppConfig::validate`

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/config.rs:412-558` (validate block) / `crates/core/src/dispatch.rs:68` |
| **Severity** | Medium |
| **Category** | Config |

**Problem**

The Finding 7 fix added a `dispatch_concurrency: usize` field to `DispatchSettings` /
`DispatchConfig` and applied `config.dispatch_concurrency.max(1)` in `dispatch_due`
to defend against zero. The recommended fix explicitly asked for *both* a `.max(1)` guard
*and* an `AppConfig::validate()` rule, but only the runtime guard landed. As a result:

1. An operator who sets `dispatch_concurrency = 0` in TOML gets silent coercion to 1
   at runtime rather than a startup-time error — directly contradicting the project's
   "fail fast at startup" convention used for every other dispatch field (look at
   `batch_size`, `schedule_batch_size`, `max_attempts`, etc., all of which reject 0).
2. There is also no upper-bound / relational check (`dispatch_concurrency <= batch_size`,
   `dispatch_concurrency <= database.max_connections`), which is the *exact* failure mode
   Finding 7 was about. A misconfigured `dispatch_concurrency` larger than
   `database.max_connections` re-introduces the pool-starvation cascade that Finding 7
   set out to prevent.

**Context** (surrounding code as it exists today)

```rust
// crates/core/src/dispatch.rs lines 63-71
let mut tasks = futures::stream::iter(
    due.into_iter()
        .map(|d| dispatch_one(repo, callback, config, d)),
)
.buffer_unordered(config.dispatch_concurrency.max(1));

while tasks.next().await.is_some() {}
Ok(())
```

```rust
// crates/core/src/config.rs validate() — currently no check for dispatch_concurrency
// (search the validate() body and you will find no occurrence of "dispatch_concurrency").
```

**Recommended fix**

Add to `AppConfig::validate`:

```rust
if self.dispatch.dispatch_concurrency == 0 {
    errors.push("dispatch.dispatch_concurrency must be > 0".to_string());
}
if (self.dispatch.dispatch_concurrency as u32) > self.database.max_connections {
    errors.push(format!(
        "dispatch.dispatch_concurrency ({}) must be <= database.max_connections ({}); \
         a slow callback can otherwise hold the last connection while concurrent \
         dispatchers stall on lock_delivery",
        self.dispatch.dispatch_concurrency, self.database.max_connections
    ));
}
```

Drop the `.max(1)` at `dispatch.rs:68` once the validate check is in place — defensive
coercion at runtime hides config bugs operators should see at startup.

**Why this fix**

Finding 7 was a *concurrency / pool-starvation* finding, not a *missing field*
finding. The field alone, without a relational check against `max_connections`,
does not fix the failure mode it was raised against.

---

### Finding 11 — `dispatch_concurrency` is missing from `envs/app_config.toml`

| Field | Value |
|-------|-------|
| **File:Line** | `envs/app_config.toml` (the `[dispatch]` section) |
| **Severity** | Low |
| **Category** | Config |

**Problem**

`envs/app_config.toml` is the canonical "every tunable knob lives here" file
(`poll_interval_secs`, `batch_size`, `schedule_batch_size`, `max_attempts`,
`backoff_secs`, `handler_timeout_secs`, `lock_buffer_secs`,
`external_timeout_sweep_interval_secs`, `max_completion_cycles`,
`payload_size_limit_bytes`, `notify_channel`, …, all explicitly listed). The new
`dispatch_concurrency` field is the *only* dispatch field missing from this file,
even though it now has a serde `#[serde(default = "default_dispatch_concurrency")]`
covering its absence.

The functional impact is zero (default = 16). The operator-experience impact is
real: someone tuning throughput from this file would not know the knob exists.

**Context** (surrounding code as it exists today)

```toml
# envs/app_config.toml — current [dispatch] section
[dispatch]
poll_interval_secs = 5
batch_size = 50
schedule_batch_size = 500
max_attempts = 6
# ... no dispatch_concurrency line
```

**Recommended fix**

Add a line in the `[dispatch]` block of `envs/app_config.toml`:

```toml
[dispatch]
poll_interval_secs = 5
batch_size = 50
# Maximum concurrent in-flight HTTP calls per dispatch cycle. Should be
# <= database.max_connections to avoid pool starvation under slow callbacks.
dispatch_concurrency = 16
schedule_batch_size = 500
# ...
```

**Why this fix**

Documents the new tunable so operators can find it; matches the precedent set by
every other dispatch field. Pure documentation change — no code impact.

---

### Finding 12 — `HttpCallback` reqwest client has no full-request timeout

| Field | Value |
|-------|-------|
| **File:Line** | `crates/http-callback/src/client.rs:55-67` |
| **Severity** | Low |
| **Category** | Correctness |

**Problem**

The reqwest `ClientBuilder` is configured only with `.connect_timeout(...)`. There is
no `.timeout(...)` (full request) or `.read_timeout(...)` on the client itself.
A receiver that completes the TCP handshake quickly but then drips response bytes
extremely slowly (or stalls mid-body) is bounded *only* by the outer
`tokio::time::timeout(due.target.timeout, …)` in `dispatch.rs:133`.

That outer timeout works — it drops the future, which causes reqwest to cancel
the in-flight request — but two minor concerns remain:

1. **Defense-in-depth**: a callback bug that swallows or extends the outer
   timeout (e.g. a future wrapper that ignores cancellation) leaves no timeout
   inside reqwest. Today there is no such bug, but the layered defence is
   essentially free.
2. **Connection lifetime under cancellation**: when reqwest's body-stream future
   is dropped, the underlying TCP connection is closed (it cannot be returned
   to the pool mid-body). Setting `.timeout(due.target.timeout + lock_buffer)`
   on the request via `.timeout(...)` on the request builder would let reqwest
   itself observe the boundary and abort cleanly.

**Context** (surrounding code as it exists today)

```rust
// crates/http-callback/src/client.rs lines 55-67
pub fn new(cfg: &HttpClientConfig, keyring: Arc<KeyRing>) -> Result<Self, HttpCallbackError> {
    let mut builder = ClientBuilder::new()
        .user_agent(&cfg.user_agent)
        .connect_timeout(Duration::from_secs(cfg.connect_timeout_secs))
        .redirect(reqwest::redirect::Policy::none()); // §6.3: redirects are failures

    if cfg.allow_insecure_tls {
        builder = builder.danger_accept_invalid_certs(true);
    }

    let client = builder.build()?;
    Ok(Self { client, keyring })
}
```

**Recommended fix**

Apply `.timeout(...)` on the per-request builder in `deliver`, using
`due.target.timeout`. This is preferable to setting it on the client because
each `CallbackTarget` has its own timeout:

```rust
// crates/http-callback/src/client.rs deliver() — request-build section
let mut req = self
    .client
    .post(&target.url)
    .timeout(target.timeout)  // <— new: full-request timeout matches per-callback config
    .header("Content-Type", "application/json")
    // ... rest unchanged
```

**Why this fix**

Layered timeouts are the standard pattern: connect, full request, and outer
tokio timeout. Today only connect + outer tokio are present; the middle layer
is essentially free defence and removes the need to rely on Tokio's cancellation
propagating cleanly into reqwest.

---

### Finding 13 — `extract_retry_after` doc claims "negative deltas collapse to zero" but code returns `None`

| Field | Value |
|-------|-------|
| **File:Line** | `crates/http-callback/src/client.rs:193-209` |
| **Severity** | Low |
| **Category** | Idiom |

**Problem**

The doc-comment on `extract_retry_after` (after the Finding 5 fix) says:

> Negative deltas (date already in the past) collapse to zero.

But the implementation is `target.duration_since(SystemTime::now()).ok()`, which
returns `Err` when `target < now` and is then `.ok()` → `None`. Behaviour-wise
this is fine (in `compute_next_available_at`, `Some(Duration::ZERO)` and `None`
both end up using the schedule base since `after.max(base) == base`), but the
doc and code disagree.

**Context** (surrounding code as it exists today)

```rust
// crates/http-callback/src/client.rs lines 193-209
/// Parse a `Retry-After` header value into a `Duration`.
///
/// Supports both integer seconds (most common) and the HTTP-date form
/// (RFC 7231 §7.1.3, e.g. `Wed, 21 Oct 2026 07:28:00 GMT`).
/// Negative deltas (date already in the past) collapse to zero.
/// Returns `None` if the header is absent or unparseable.
fn extract_retry_after(response: &reqwest::Response) -> Option<Duration> {
    let value = response.headers().get("Retry-After")?.to_str().ok()?;
    let trimmed = value.trim();
    // Try integer seconds first (most common form).
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    // RFC 7231 §7.1.3 HTTP-date form. Negative deltas collapse to zero.
    let target = httpdate::parse_http_date(trimmed).ok()?;
    target.duration_since(SystemTime::now()).ok()
}
```

**Recommended fix**

Either match the doc by clamping to zero:

```rust
let target = httpdate::parse_http_date(trimmed).ok()?;
Some(
    target
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO),
)
```

…or correct the doc-comment to match the current behaviour:

```rust
/// Past HTTP-date values are returned as `None` (treated as "no floor"), which
/// is behaviourally equivalent to a zero floor under `compute_next_available_at`.
```

The first option is preferable because it makes the function self-consistent: a
parseable `Retry-After` header always yields `Some(_)`; only an *unparseable*
or *absent* header yields `None`. Tests would then be easier to write
(`assert_eq!(extract_retry_after(...), Some(Duration::ZERO))` for a past date).

**Why this fix**

Doc-comments that misdescribe behaviour are the cheapest comment failure mode to
fix and the most expensive one to leave in: future maintainers reasoning about
Retry-After semantics will trust the comment.

---

### Finding 14 — `extract_retry_after` HTTP-date branch has no unit test

| Field | Value |
|-------|-------|
| **File:Line** | `crates/http-callback/src/client.rs:281-294` (test module) |
| **Severity** | Low |
| **Category** | Testing |

**Problem**

The Finding 5 fix added `httpdate::parse_http_date` for RFC 7231 §7.1.3 dates.
The unit test `extract_retry_after_parses_integer_seconds` only exercises the
integer-seconds path; there is no test that:

- A valid future HTTP-date returns a `Some(Duration)`.
- A past HTTP-date returns `None` (or `Some(ZERO)` depending on the resolution of Finding 13).
- A malformed string returns `None`.

The HTTP-level test `deliver_returns_transient_with_retry_after_on_429` uses
`"Retry-After: 60"` — also the integer path. So the new `httpdate` dependency
has zero code coverage.

**Context** (surrounding code as it exists today)

```rust
// crates/http-callback/src/client.rs lines 281-294 (test for the integer path only)
#[test]
fn extract_retry_after_parses_integer_seconds() {
    let secs_str = "120";
    let parsed: Option<u64> = secs_str.trim().parse().ok();
    assert_eq!(parsed, Some(120u64));
    assert_eq!(
        parsed.map(Duration::from_secs),
        Some(Duration::from_secs(120))
    );
}
```

Note: this test does not actually invoke `extract_retry_after` — it
re-implements the parse inline. The function as written can only be exercised
via the public `deliver` path (it takes a `&reqwest::Response`), so a more useful
test factors the parsing into a helper accepting `&str`.

**Recommended fix**

Refactor `extract_retry_after` to delegate to a thin `parse_retry_after(&str) -> Option<Duration>`
helper that does not depend on `reqwest::Response`, then test the helper directly:

```rust
fn parse_retry_after(value: &str) -> Option<Duration> {
    let trimmed = value.trim();
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let target = httpdate::parse_http_date(trimmed).ok()?;
    target.duration_since(SystemTime::now()).ok()
}

fn extract_retry_after(response: &reqwest::Response) -> Option<Duration> {
    parse_retry_after(response.headers().get("Retry-After")?.to_str().ok()?)
}

#[test]
fn parse_retry_after_integer_seconds() {
    assert_eq!(parse_retry_after("60"), Some(Duration::from_secs(60)));
}

#[test]
fn parse_retry_after_future_http_date_returns_some() {
    // Build an HTTP-date ~10 minutes in the future
    let future = SystemTime::now() + Duration::from_secs(600);
    let header = httpdate::fmt_http_date(future);
    let d = parse_retry_after(&header).expect("future date must parse");
    assert!(d > Duration::from_secs(500) && d < Duration::from_secs(700));
}

#[test]
fn parse_retry_after_garbage_returns_none() {
    assert_eq!(parse_retry_after("not-a-date"), None);
}
```

**Why this fix**

CLAUDE.md sets a >90 % per-module coverage target. The HTTP-date branch is
new code with no test; `httpdate` is a new dependency with zero exercise.

---

### Finding 15 — `keyring_with` test helper leaks env-var state via `unsafe { set_var }`

| Field | Value |
|-------|-------|
| **File:Line** | `crates/http-callback/src/client.rs:298-317` |
| **Severity** | Low |
| **Category** | Testing |

**Problem**

`keyring_with` uses `unsafe { std::env::set_var(...) }` and
`unsafe { std::env::remove_var(...) }` to bridge a string secret into
`KeyRing::load` (which only reads from env). The pattern is correct *for a
single-threaded setup*, but `cargo test` runs tests in parallel by default. If
a future test calls `keyring_with` concurrently with another test that reads
the same env var (or with another call that uses the same `secret_env` name),
the second `set_var` will race the first `remove_var`. Today only one test
(`deliver_sends_hmac_signature_header_when_key_present`) uses the helper with
the unique name `TEST_HMAC_SECRET_DELIVER`, so the race is latent — but it is
a foot-gun for the next person to add a wiremock test.

The `SAFETY` comment claims "test-only, single-threaded setup before the async
runtime starts" — but `#[tokio::test]` *does* start an async runtime, and
multiple `#[tokio::test]` functions in the same binary do run concurrently
across threads.

**Context** (surrounding code as it exists today)

```rust
// crates/http-callback/src/client.rs lines 298-317
fn keyring_with(key_id: &str, secret_env: &str, secret_val: &str) -> Arc<KeyRing> {
    use outbox_dispatcher_core::SigningKeyConfig;
    let env_key = secret_env.to_string();
    // SAFETY: test-only, single-threaded setup before the async runtime starts.
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

Two options, in order of preference:

1. **Build the `KeyRing` directly in tests** without going through env. Expose
   a `KeyRing::from_secrets(&HashMap<String, Vec<u8>>)` or
   `KeyRing::with_key(&str, &[u8])` constructor for tests (behind
   `#[cfg(test)]` or as a public test helper crate), so the helper becomes:

   ```rust
   fn keyring_with(key_id: &str, secret: &[u8]) -> Arc<KeyRing> {
       Arc::new(KeyRing::with_key(key_id, secret))
   }
   ```

2. **Serialise env access** with a `static MUTEX: Mutex<()>` held across
   `set_var` … `KeyRing::load` … `remove_var`, and fix the `SAFETY` comment to
   describe the actual invariant (no parallel callers, not "before the async
   runtime starts").

Option 1 is strictly safer and matches the convention used in `config.rs` where
`LOAD_MUTEX` already serialises tests that touch `APP_CONFIG_DIR`.

**Why this fix**

Rust 2024 made `std::env::set_var` `unsafe` precisely because it is a
process-global mutation that races every other thread's `getenv`. The `SAFETY`
comment claims an invariant that does not hold for `#[tokio::test]` binaries;
that mismatch *will* bite the next person to copy this helper.

---

### Finding 16 — Pre-existing `clippy::type_complexity` in `scheduler.rs` blocks `--all-targets` lint

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/scheduler.rs:378` |
| **Severity** | Low |
| **Category** | Idiom |

**Problem**

`cargo clippy --workspace --all-targets -- -D warnings` fails with:

```
error: very complex type used. Consider factoring parts into `type` definitions
   --> crates/core/src/scheduler.rs:378:19
    |
378 |         invalids: Mutex<Vec<(Uuid, Vec<(String, String)>)>>,
    |                   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
```

This is in the Phase 3 mock-repo test code and not strictly a PR #4 issue —
`cargo clippy --workspace -- -D warnings` (without `--all-targets`) passes. But
the project's stated mandatory check (`cargo clippy --workspace -- -D warnings`)
omits `--all-targets`, so test-only clippy regressions are invisible. CI in
Phase 8 will almost certainly run `--all-targets`.

PR #4 is the right place to catch this because PR #4 added the `[dev-dependencies]`
wiremock paths and adjusted the same module; future PRs that *also* run
`--all-targets` will inherit the failure and blame the wrong PR.

**Context** (surrounding code as it exists today)

```rust
// crates/core/src/scheduler.rs lines 375-385 (the offending struct)
struct MockRepo {
    events: Mutex<VecDeque<RawEvent>>,
    ensured: Mutex<Vec<(Uuid, Vec<String>)>>,
    invalids: Mutex<Vec<(Uuid, Vec<(String, String)>)>>,
    cursor: Mutex<i64>,
    /// If set, `ensure_deliveries` returns this error once then clears it.
    ensure_error: Mutex<Option<crate::error::Error>>,
    /// If set, `create_invalid_deliveries` returns this error once then clears it.
    invalid_error: Mutex<Option<crate::error::Error>>,
}
```

**Recommended fix**

Introduce two type aliases at the top of the test module:

```rust
// crates/core/src/scheduler.rs — test module
type EnsuredCalls = Mutex<Vec<(Uuid, Vec<String>)>>;
type InvalidCalls = Mutex<Vec<(Uuid, Vec<(String, String)>)>>;

struct MockRepo {
    events: Mutex<VecDeque<RawEvent>>,
    ensured: EnsuredCalls,
    invalids: InvalidCalls,
    cursor: Mutex<i64>,
    ensure_error: Mutex<Option<crate::error::Error>>,
    invalid_error: Mutex<Option<crate::error::Error>>,
}
```

Also update CLAUDE.md's "Mandatory After Every Code Change" block to use
`cargo clippy --workspace --all-targets -- -D warnings` so test-only regressions
are caught locally before CI.

**Why this fix**

The fix is mechanical and clears a real CI-blocker the moment Phase 8 starts
running `--all-targets`. The CLAUDE.md change prevents the same gap from
re-opening on future PRs.

---

## Summary

| # | Title | File:Line | Severity | Category | Status | Notes |
|---|-------|-----------|----------|----------|--------|-------|
| 1 | `HttpCallback::new` returns `anyhow` from a library crate | `crates/http-callback/src/client.rs:54` | High | Idiom | DONE | Verified — now returns `Result<Self, HttpCallbackError>` |
| 2 | Dead-code `keyring_with` test helper trips `-D warnings` | `crates/http-callback/src/client.rs:262-280` | Medium | Testing | DONE | Verified — helper is now exercised by `deliver_sends_hmac_signature_header_when_key_present` |
| 3 | Redundant `Arc::clone` in `HttpCallback::new` | `crates/http-callback/src/client.rs:69` | Low | Idiom | DONE | Verified — field-shorthand move in place |
| 4 | No HTTP-level test of `Callback::deliver` | `crates/http-callback/src/client.rs:282-314` | High | Testing | DONE | Verified — 7 wiremock-backed tokio tests added |
| 5 | `extract_retry_after` ignores HTTP-date form | `crates/http-callback/src/client.rs:201-209` | Low | Correctness | DONE | Verified — `httpdate::parse_http_date` integrated. See Finding 13/14 for follow-ups |
| 6 | Dead `if … Ok(())` branch in `sweep_hung_external` | `crates/core/src/timeout_sweep.rs:42-46` | Low | Idiom | DONE | Verified — dead branch removed |
| 7 | `dispatch_due` has no concurrency cap; can starve pg pool | `crates/core/src/dispatch.rs:61-67` | Medium | Concurrency | DONE | Verified — `buffer_unordered(dispatch_concurrency)` in place. See Finding 10 — validation rule still missing |
| 8 | Non-dead transient failures logged silently | `crates/core/src/dispatch.rs:155-185` | Low | Idiom | DONE | Verified — `debug!` added in the non-dead branch |
| 9 | Scheduler `consecutive_errors` comment is now misleading | `crates/core/src/scheduler.rs:83-85` | Low | Idiom | DONE | Verified — comment tightened |
| 10 | `dispatch_concurrency` not validated in `AppConfig::validate` | `crates/core/src/config.rs:412-558` / `crates/core/src/dispatch.rs:68` | Medium | Config | TODO | Follow-up to Finding 7 — validation rule omitted |
| 11 | `dispatch_concurrency` missing from `envs/app_config.toml` | `envs/app_config.toml` `[dispatch]` | Low | Config | TODO | |
| 12 | `HttpCallback` reqwest client has no full-request timeout | `crates/http-callback/src/client.rs:55-67` | Low | Correctness | TODO | Defence-in-depth — outer `tokio::time::timeout` covers it today |
| 13 | `extract_retry_after` doc-comment disagrees with code for past dates | `crates/http-callback/src/client.rs:193-209` | Low | Idiom | TODO | |
| 14 | `extract_retry_after` HTTP-date branch has no unit test | `crates/http-callback/src/client.rs:281-294` | Low | Testing | TODO | |
| 15 | `keyring_with` test helper leaks env-var state via `unsafe { set_var }` | `crates/http-callback/src/client.rs:298-317` | Low | Testing | TODO | `SAFETY` comment claims an invariant that doesn't hold for `#[tokio::test]` |
| 16 | Pre-existing `clippy::type_complexity` blocks `--all-targets` lint | `crates/core/src/scheduler.rs:378` | Low | Idiom | TODO | Phase-3 leftover; surface before Phase 8 CI |

> **Instructions for the implementing LLM:**
> - Change `TODO` to `DONE` once a finding is fully addressed.
> - If a finding is intentionally skipped or cannot be applied, change `TODO` to `SKIPPED` and add a short reason in the **Notes** column.
> - Do not delete rows — the table is the authoritative implementation log.
