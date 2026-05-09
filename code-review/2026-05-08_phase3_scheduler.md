# Code Review — Phase 3 (Scheduler & wake loop)

**Date:** 2026-05-08T21:01:50Z
**Branch:** phase3
**Reviewed by:** Gemini CLI (review command)
**Scope:** `crates/core/src/scheduler.rs`, `crates/core/src/repo.rs`, `crates/bin/src/main.rs`, `crates/core/src/config.rs`

---

## Findings

### Finding 1 — Redundant `catch_unwind` in scheduler loop

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/scheduler.rs:163` |
| **Severity** | Medium |
| **Category** | Idiom |

**Problem**

`catch_unwind` is used defensively around `parse_callbacks`. This is generally discouraged for internal library logic. If `parse_callbacks` panics, it's a bug that should be fixed at the source. Using `catch_unwind` hides potential logic errors and complicates the code with `AssertUnwindSafe`.

**Context**

```rust
// crates/core/src/scheduler.rs lines 163-173
        let parsed = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            parse_callbacks(&event.callbacks, config)
        })) {
            Ok(p) => p,
            Err(_) => {
                // Defensive: parse_callbacks should never panic, but guard anyway.
                error!(
                    event_id = %event.event_id,
                    "unexpected panic while parsing callbacks — skipping event (poison pill)"
                );
                continue;
            }
        };
```

**Recommended fix**

```rust
        let parsed = parse_callbacks(&event.callbacks, config);
```

**Why this fix**

`parse_callbacks` already returns a `ParsedCallbacks` struct that handles invalid data gracefully; internal bugs (panics) should be surfaced and fixed rather than swallowed by a "poison pill" guard.

---

### Finding 2 — Use of unstable `let_chains` syntax

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/scheduler.rs:180` |
| **Severity** | Medium |
| **Category** | Idiom |

**Problem**

The code uses the unstable `let_chains` feature (`if condition && let ... = ...`). While it may compile on specific toolchains, it is non-standard and should be avoided in production code unless the project explicitly targets nightly or a specific future stable version that includes it.

**Context**

```rust
// crates/core/src/scheduler.rs lines 180-188
        // Schedule valid callbacks.
        if !parsed.valid.is_empty()
            && let Err(e) = repo.ensure_deliveries(event.event_id, &parsed.valid).await
        {
            error!(
                event_id = %event.event_id,
                error = %e,
                "failed to ensure deliveries for event"
            );
        }
```

**Recommended fix**

```rust
        // Schedule valid callbacks.
        if !parsed.valid.is_empty() {
            if let Err(e) = repo.ensure_deliveries(event.event_id, &parsed.valid).await {
                error!(
                    event_id = %event.event_id,
                    error = %e,
                    "failed to ensure deliveries for event"
                );
            }
        }
```

**Why this fix**

Maintains compatibility with stable Rust and adheres to the project's "no hacks" mandate by avoiding unstable language features.

---

### Finding 3 — Hardcoded `FETCH_LIMIT` in scheduler

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/scheduler.rs:141` |
| **Severity** | Low |
| **Category** | Config |

**Problem**

The number of events to fetch per scheduling cycle is hardcoded to 500. This limit should be configurable to allow tuning based on database performance and event volume.

**Context**

```rust
// crates/core/src/scheduler.rs lines 141-143
pub async fn schedule_new_deliveries(
    repo: &dyn Repo,
    config: &DispatchConfig,
    cursor: i64,
) -> Result<i64> {
    const FETCH_LIMIT: i64 = 500;
```

**Recommended fix**

Add `schedule_batch_size` to `DispatchConfig` and use it here.

```rust
// crates/core/src/scheduler.rs
    let new_events = repo.fetch_new_events(cursor, config.schedule_batch_size as i64).await?;
```

**Why this fix**

Centralizes magic numbers into configuration, allowing operators to tune the system without code changes.

---

### Finding 4 — Missing upper-bound validation for `max_callbacks_per_event`

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/config.rs:469` |
| **Severity** | Medium |
| **Category** | Config |

**Problem**

`max_callbacks_per_event` only validates that the value is `> 0`. It lacks a reasonable upper bound (e.g., 1000). A very large value could lead to high memory usage or N+1 query patterns if not carefully handled.

**Context**

```rust
// crates/core/src/config.rs lines 469-471
        if self.dispatch.max_callbacks_per_event == 0 {
            errors.push("dispatch.max_callbacks_per_event must be > 0".to_string());
        }
```

**Recommended fix**

```rust
        if self.dispatch.max_callbacks_per_event == 0 || self.dispatch.max_callbacks_per_event > 1000 {
            errors.push("dispatch.max_callbacks_per_event must be between 1 and 1000".to_string());
        }
```

**Why this fix**

Prevents potential resource exhaustion from excessively large fan-out configurations.

---

### Finding 5 — `KeyRing::load` result discarded in `main.rs`

| Field | Value |
|-------|-------|
| **File:Line** | `crates/bin/src/main.rs:64` |
| **Severity** | Low |
| **Category** | Idiom |

**Problem**

`KeyRing::load` is called to validate signing keys, but the resulting `KeyRing` is discarded. While this works for validation, it means the keys might be loaded again later, and it's a missed opportunity to initialize the keyring once at startup.

**Context**

```rust
// crates/bin/src/main.rs lines 64-70
    // Fail fast if any configured signing key cannot be resolved from its env var.
    if let Err(errors) = KeyRing::load(&config.signing_keys) {
        for e in &errors.0 {
            eprintln!("signing key error: {e}");
        }
        anyhow::bail!("invalid signing keys ({} error(s))", errors.0.len());
    }
```

**Recommended fix**

Store the `KeyRing` and pass it to components that need it (though in Phase 3 the scheduler doesn't need it yet, the dispatcher will in Phase 4).

**Why this fix**

Avoids redundant work and prepares for Phase 4 where the `KeyRing` will be required by the dispatcher.

---

## Findings — second pass (PR #3 review, 2026-05-09)

### Finding 6 — Cursor advances past events whose DB writes fail, causing permanent skip

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/scheduler.rs:147-228` |
| **Severity** | High |
| **Category** | Correctness |

**Problem**

`new_cursor = event.id` is written unconditionally at the top of every loop iteration, *before* `ensure_deliveries` and `create_invalid_delivery` run. The intent — documented in the comment "Always advance the cursor so a poison pill does not stall the loop" — is to tolerate parse-time failures, but the code advances the cursor for *every* failure mode, including transient DB errors. Both `ensure_deliveries` and `create_invalid_delivery` only `error!`-log on failure and continue; the function then returns `Ok(new_cursor)`. After a transient DB blip on a single event:

1. The cursor is persisted in memory (and recoverable from `recover_cursor` once *later* events succeed and create delivery rows).
2. The failed event has no delivery rows.
3. On the next scheduling cycle (or restart, since `recover_cursor` returns the highest event id with a delivery row), the failed event is `id < cursor` and is never re-fetched.

This is silent, permanent data loss for any event that hits a transient DB error during scheduling — exactly the failure mode the cursor was supposed to make recoverable.

**Context** (surrounding code as it exists today)

```rust
// crates/core/src/scheduler.rs lines 149-228
for event in &new_events {
    // Always advance the cursor so a poison pill does not stall the loop.
    new_cursor = event.id;

    // ── Payload-size guard ────────────────────────────────────────────────
    if event.payload_size_bytes > config.payload_size_limit_bytes {
        ...
        let names = extract_callback_names(&event.callbacks);
        for name in names {
            if let Err(e) = repo
                .create_invalid_delivery(event.event_id, &name, &reason)
                .await
            {
                error!(...);   // logged but cursor already advanced
            }
        }
        continue;
    }

    let parsed = parse_callbacks(&event.callbacks, config);

    if parsed.valid.is_empty() && parsed.invalid.iter().all(|(n, _)| n == "<root>") {
        error!(...);
        continue;
    }

    if !parsed.valid.is_empty()
        && let Err(e) = repo.ensure_deliveries(event.event_id, &parsed.valid).await
    {
        error!(...);   // logged but cursor already advanced — event lost
    }

    for (name, reason) in &parsed.invalid {
        if let Err(e) = repo
            .create_invalid_delivery(event.event_id, name, reason)
            .await
        {
            error!(...);   // logged but cursor already advanced — invalid row lost
        }
    }
}

Ok(new_cursor)
```

**Recommended fix**

Distinguish parse-time advances (poison pills — safe to skip forever) from DB-time failures (must not advance the cursor). Track whether all DB writes for the current event succeeded, and only advance the persisted cursor for events that fully succeeded *or* were poison pills. On the first DB failure, propagate so the wake loop logs and retries on the next cycle:

```rust
for event in &new_events {
    // ── Payload-size guard ────────────────────────────────────────────────
    if event.payload_size_bytes > config.payload_size_limit_bytes {
        let reason =
            payload_too_large_error(event.payload_size_bytes, config.payload_size_limit_bytes);
        warn!(
            event_id = %event.event_id,
            payload_bytes = event.payload_size_bytes,
            limit_bytes = config.payload_size_limit_bytes,
            "payload too large — dead-lettering all callbacks"
        );
        let names = extract_callback_names(&event.callbacks);
        for name in names {
            // Propagate: a transient DB error here would otherwise lose the dead-letter
            // record for an oversized payload (the event is also lost from the cursor).
            repo.create_invalid_delivery(event.event_id, &name, &reason)
                .await?;
        }
        new_cursor = event.id;
        continue;
    }

    // ── Structural validation & callback expansion ────────────────────────
    let parsed = parse_callbacks(&event.callbacks, config);

    // Poison pill (non-array or array-level rejection): no callback names to record;
    // safe to advance the cursor and skip — repeated work on restart would be a no-op.
    if parsed.valid.is_empty() && parsed.invalid.iter().all(|(n, _)| n == "<root>") {
        error!(
            event_id = %event.event_id,
            callbacks_json = %event.callbacks,
            "callbacks JSONB is structurally corrupt — skipping event (poison pill)"
        );
        new_cursor = event.id;
        continue;
    }

    // Schedule valid callbacks. Propagate DB errors so the cursor stays at the last
    // fully-processed event and the wake loop retries this event on the next cycle.
    if !parsed.valid.is_empty() {
        repo.ensure_deliveries(event.event_id, &parsed.valid).await?;
    }

    // Dead-letter structurally invalid callbacks. Same rule: propagate DB errors.
    for (name, reason) in &parsed.invalid {
        repo.create_invalid_delivery(event.event_id, name, reason)
            .await?;
    }

    new_cursor = event.id;
}
```

**Why this fix**

Cursor advance is the commit point for "this event has been scheduled". Advancing on a DB failure breaks the at-least-once delivery guarantee. The wake loop already handles `Err` from `schedule_new_deliveries` by logging and retrying on the next wake (see `run_scheduler`), so propagating is the right shape. Idempotency of `ensure_deliveries`/`create_invalid_delivery` (both use `ON CONFLICT DO NOTHING`) means re-running on the next cycle is safe.

---

### Finding 7 — N+1 inserts when dead-lettering invalid or oversized callbacks

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/scheduler.rs:170-182, 216-228` & `crates/core/src/repo.rs:205-226` |
| **Severity** | Medium |
| **Category** | Performance |

**Problem**

`schedule_new_deliveries` calls `create_invalid_delivery` once per callback in two paths:

1. Oversized-payload events (one row per name returned by `extract_callback_names`).
2. Structurally invalid callbacks within a parsed event.

Each call is a separate round-trip insert. With Finding 4 from this review now setting `MAX_CALLBACKS_PER_EVENT_LIMIT = 1_000`, a single oversized event can trigger up to 1000 sequential round-trips inside the scheduler loop — pinning a pool connection per call and stalling other events behind it. `ensure_deliveries` already uses a single `INSERT … FROM UNNEST` for the valid case; the dead-letter path has no equivalent.

**Context** (surrounding code as it exists today)

```rust
// crates/core/src/scheduler.rs lines 169-182 (oversized-payload path)
let names = extract_callback_names(&event.callbacks);
for name in names {
    if let Err(e) = repo
        .create_invalid_delivery(event.event_id, &name, &reason)
        .await
    {
        error!(...);
    }
}

// crates/core/src/scheduler.rs lines 216-228 (invalid-callback path)
for (name, reason) in &parsed.invalid {
    if let Err(e) = repo
        .create_invalid_delivery(event.event_id, name, reason)
        .await
    {
        error!(...);
    }
}

// crates/core/src/repo.rs lines 205-226 (single-row insert)
async fn create_invalid_delivery(
    &self,
    event_id: Uuid,
    callback_name: &str,
    reason: &str,
) -> Result<()> {
    let reason = truncate_at_char_boundary(reason, 4096);
    sqlx::query!(
        r#"
        INSERT INTO outbox_deliveries
            (event_id, callback_name, completion_mode, dead_letter, last_error, available_at)
        VALUES ($1, $2, 'managed', TRUE, $3, now())
        ON CONFLICT (event_id, callback_name) DO NOTHING
        "#,
        event_id,
        callback_name,
        reason,
    )
    .execute(&self.pool)
    .await?;
    Ok(())
}
```

**Recommended fix**

Add a batched method to `Repo` mirroring the `UNNEST` pattern of `ensure_deliveries` and call it once per event:

```rust
// crates/core/src/repo.rs — new trait method
async fn create_invalid_deliveries(
    &self,
    event_id: Uuid,
    entries: &[(String, String)], // (callback_name, reason)
) -> Result<()>;

// crates/core/src/repo.rs — PgRepo impl
async fn create_invalid_deliveries(
    &self,
    event_id: Uuid,
    entries: &[(String, String)],
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = entries.iter().map(|(n, _)| n.clone()).collect();
    let reasons: Vec<String> = entries
        .iter()
        .map(|(_, r)| truncate_at_char_boundary(r, 4096).to_string())
        .collect();
    sqlx::query!(
        r#"
        INSERT INTO outbox_deliveries
            (event_id, callback_name, completion_mode, dead_letter, last_error, available_at)
        SELECT $1, name, 'managed', TRUE, reason, now()
        FROM UNNEST($2::text[], $3::text[]) AS t(name, reason)
        ON CONFLICT (event_id, callback_name) DO NOTHING
        "#,
        event_id,
        &names as &[String],
        &reasons as &[String],
    )
    .execute(&self.pool)
    .await?;
    Ok(())
}
```

```rust
// crates/core/src/scheduler.rs — oversized-payload path
let names = extract_callback_names(&event.callbacks);
let entries: Vec<(String, String)> =
    names.into_iter().map(|n| (n, reason.clone())).collect();
repo.create_invalid_deliveries(event.event_id, &entries).await?;

// crates/core/src/scheduler.rs — invalid-callback path
repo.create_invalid_deliveries(event.event_id, &parsed.invalid).await?;
```

**Why this fix**

One round-trip per event for the dead-letter path matches the existing `ensure_deliveries` pattern, removes the N+1 stall on fan-out events, and keeps the dead-letter rows transactionally co-grouped so a partial DB failure cannot leave half the callbacks dead-lettered and half not.

---

### Finding 8 — Array-level rejections (too-many-callbacks) skip the event silently

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/scheduler.rs:194-202` & `crates/core/src/callbacks.rs:95-105` |
| **Severity** | Medium |
| **Category** | Correctness |

**Problem**

`parse_callbacks` returns a single `<root>` invalid entry for two distinct cases: (a) the JSONB is not an array, (b) the array exceeds `max_callbacks_per_event`. The scheduler treats both identically — skip the event with an `error!` log and no delivery rows. Case (a) is unreachable in practice (the DB CHECK rejects non-array `callbacks` at INSERT time), but case (b) is reachable: an operator who lowers `max_callbacks_per_event` to a tighter ceiling after events are already in the table will have those events disappear into log files, with no entry in `outbox_deliveries` for the admin API to surface. The dead-letter list (Phase 6) cannot show what was never written.

**Context** (surrounding code as it exists today)

```rust
// crates/core/src/scheduler.rs lines 194-202
// Poison-pill guard: if the entire callbacks value is unparseable at the
// array level (not an array, completely corrupt), `parse_callbacks` returns
// only `invalid` entries with the synthetic "<root>" label. Log and skip.
if parsed.valid.is_empty() && parsed.invalid.iter().all(|(n, _)| n == "<root>") {
    error!(
        event_id = %event.event_id,
        callbacks_json = %event.callbacks,
        "callbacks JSONB is structurally corrupt — skipping event (poison pill)"
    );
    continue;
}
```

```rust
// crates/core/src/callbacks.rs lines 95-105 — the too-many-callbacks return path
if array.len() > config.max_callbacks_per_event as usize {
    invalid.push((
        "<root>".to_string(),
        format!(
            "invalid_callback: too many callbacks in event ({}); max is {}",
            array.len(),
            config.max_callbacks_per_event
        ),
    ));
    return ParsedCallbacks { valid, invalid };
}
```

**Recommended fix**

Distinguish "array-level reject" (callback names are still extractable) from "JSONB corrupt" (no names extractable). For array-level rejections, dead-letter every callback in the array under the array-level reason so operators see the rejection in the dead-letter list. Two viable shapes:

1. Have `parse_callbacks` return a structured reason instead of a `<root>` sentinel name (e.g. an enum `ArrayLevelRejection` separate from per-callback errors), then in the scheduler iterate `event.callbacks` (still a parseable array in case b) to materialise dead-letter rows for each entry's name.
2. Keep the sentinel but, when the cause is too-many-callbacks (i.e. the value *is* an array), reuse `extract_callback_names` to dead-letter each entry with the `<root>` reason as `last_error`:

```rust
// crates/core/src/scheduler.rs
if parsed.valid.is_empty() && parsed.invalid.iter().all(|(n, _)| n == "<root>") {
    if event.callbacks.is_array() {
        // Array exists but failed an array-level check (e.g. too many callbacks).
        // Dead-letter every entry so the rejection is visible in the admin dead-letter list.
        let reason = parsed
            .invalid
            .first()
            .map(|(_, r)| r.clone())
            .unwrap_or_else(|| "invalid_callback: unspecified array-level rejection".to_string());
        let entries: Vec<(String, String)> = extract_callback_names(&event.callbacks)
            .into_iter()
            .map(|n| (n, reason.clone()))
            .collect();
        repo.create_invalid_deliveries(event.event_id, &entries).await?;
    } else {
        // Truly unparseable JSONB (currently unreachable due to DB CHECK).
        error!(
            event_id = %event.event_id,
            callbacks_json = %event.callbacks,
            "callbacks JSONB is structurally corrupt — skipping event (poison pill)"
        );
    }
    new_cursor = event.id;
    continue;
}
```

**Why this fix**

Schedule-time rejections are operationally significant — they are precisely the events the dead-letter UI needs to expose. Burying them in a `tracing::error!` line means the only way to discover lost events is to grep logs by event id, which doesn't scale and isn't what the rest of the dead-letter design assumes.

---

### Finding 9 — Wake loop reads NOTIFYs one-at-a-time and runs a full schedule cycle per notification

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/scheduler.rs:73-122` |
| **Severity** | Low |
| **Category** | Performance |

**Problem**

The `tokio::select!` consumes one notification per loop iteration and then runs `schedule_new_deliveries`. During a publisher burst (e.g. 1000 events inserted in quick succession), the listener delivers 1000 separate notifications and the scheduler runs 1000 separate cycles — each starting with a `fetch_new_events` round-trip. The first cycle picks up most/all of the burst (up to `schedule_batch_size`); the next 999 cycles do redundant work. There is also no `next_poll` reset after a NOTIFY-driven cycle, so a NOTIFY arriving close to `next_poll` causes the timer to fire a redundant second cycle on the very next iteration.

**Context** (surrounding code as it exists today)

```rust
// crates/core/src/scheduler.rs lines 73-122
loop {
    tokio::select! {
        biased;

        _ = shutdown.cancelled() => { ... return Ok(()); }

        result = listener.recv() => {
            match result {
                Ok(notification) => { ... }
                Err(e) => { ... }
            }
        }

        _ = sleep_until(next_poll) => {
            next_poll = Instant::now() + poll_interval;
            debug!("scheduler poll-timer fired");
        }
    }

    match schedule_new_deliveries(repo.as_ref(), &config, cursor).await {
        Ok(new_cursor) => { cursor = new_cursor; }
        Err(e) => { error!(...); }
    }
}
```

**Recommended fix**

Drain pending notifications between cycles and reset the poll timer after every cycle so each scheduling pass amortises across all signals received since the last pass:

```rust
loop {
    tokio::select! {
        biased;

        _ = shutdown.cancelled() => { ... return Ok(()); }

        result = listener.recv() => {
            match result {
                Ok(_) => listener_status.store(true, Ordering::Release),
                Err(e) => {
                    warn!(error = %e, "LISTEN connection lost; will rely on poll-timer until reconnected");
                    listener_status.store(false, Ordering::Release);
                }
            }
        }

        _ = sleep_until(next_poll) => {
            debug!("scheduler poll-timer fired");
        }
    }

    // Drain any further notifications that have already arrived in the buffer
    // so we run one scheduling pass per *batch* of signals, not per signal.
    while let Ok(Some(_)) = listener.try_recv().await {
        // intentionally discard — we just want to flush the buffer
    }

    match schedule_new_deliveries(repo.as_ref(), &config, cursor).await {
        Ok(new_cursor) => cursor = new_cursor,
        Err(e) => error!(error = %e, "error during schedule_new_deliveries; will retry on next wake"),
    }

    // Reset the poll timer after every cycle (NOTIFY or timer driven) so the
    // next timer fire is a full poll_interval away from the last completed cycle.
    next_poll = Instant::now() + poll_interval;
}
```

**Why this fix**

Coalescing notifications matches the actual workload (one cycle picks up the entire batch via `schedule_batch_size`), eliminates the redundant `fetch_new_events` round-trips, and makes the poll-timer's cadence predictable instead of bursting two cycles in a row when a NOTIFY lands just before the timer.

---

### Finding 10 — Wake loop swallows non-`fetch_new_events` errors after Finding 6 fix

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/scheduler.rs:111-121` |
| **Severity** | Low |
| **Category** | Observability |

**Problem**

The `match schedule_new_deliveries(...)` `Err` branch only emits a single `error!` line and continues looping. There is no metric, no health-check signal, and no rate-limit; if Postgres is in a sustained failure mode (e.g. PgBouncer flapping, `recover_cursor` won't even apply here because that only runs once at startup), the loop log-spams at full speed. Finding 6's recommended fix amplifies this by routing every transient DB failure through this branch instead of swallowing it inside the inner loop.

**Context** (surrounding code as it exists today)

```rust
// crates/core/src/scheduler.rs lines 111-121
match schedule_new_deliveries(repo.as_ref(), &config, cursor).await {
    Ok(new_cursor) => {
        cursor = new_cursor;
    }
    Err(e) => {
        // Log and continue — a transient DB error should not crash the loop.
        error!(error = %e, "error during schedule_new_deliveries; will retry on next wake");
    }
}
```

**Recommended fix**

On consecutive errors, apply a short capped backoff before the next cycle so a sustained DB outage doesn't generate an `error!` line per NOTIFY. (When metrics land in Phase 7, a counter goes here too.)

```rust
let mut consecutive_errors: u32 = 0;
loop {
    tokio::select! { ... }

    match schedule_new_deliveries(repo.as_ref(), &config, cursor).await {
        Ok(new_cursor) => {
            cursor = new_cursor;
            consecutive_errors = 0;
        }
        Err(e) => {
            consecutive_errors = consecutive_errors.saturating_add(1);
            error!(
                error = %e,
                consecutive_errors,
                "error during schedule_new_deliveries; will retry on next wake"
            );
            // Cap at ~poll_interval so we never delay longer than a regular poll.
            let backoff = poll_interval.min(Duration::from_millis(
                100u64.saturating_mul(1u64 << consecutive_errors.min(6)),
            ));
            tokio::time::sleep(backoff).await;
        }
    }

    next_poll = Instant::now() + poll_interval;
}
```

**Why this fix**

A capped exponential backoff on consecutive failures keeps the log volume bounded during a real outage and gives Postgres breathing room without changing the steady-state behaviour. `consecutive_errors` is also a natural attachment point for the future Phase 7 metric.

---

### Finding 11 — `extract_callback_names` returns `<unknown>` for non-array input that the DB CHECK forbids

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/scheduler.rs:243-259` |
| **Severity** | Low |
| **Category** | Idiom |

**Problem**

`extract_callback_names` is only called from the oversized-payload path *after* `event.payload_size_bytes > limit`. The DB schema CHECK constraint requires `callbacks` to be a non-empty array, so the `as_array()` and `is_empty()` fallbacks (`["<unknown>"]`) are unreachable in production. The dead branch obscures intent and complicates testing — three of the five unit tests for this helper exercise impossible inputs.

**Context** (surrounding code as it exists today)

```rust
// crates/core/src/scheduler.rs lines 243-259
fn extract_callback_names(callbacks: &serde_json::Value) -> Vec<String> {
    let Some(arr) = callbacks.as_array() else {
        return vec!["<unknown>".to_string()];
    };
    if arr.is_empty() {
        return vec!["<unknown>".to_string()];
    }
    arr.iter()
        .enumerate()
        .map(|(i, v)| {
            v.get("name")
                .and_then(|n| n.as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("[{i}]"))
        })
        .collect()
}
```

**Recommended fix**

Drop the unreachable fallbacks and let the empty/non-array case produce an empty `Vec<String>` (the caller already iterates and would correctly emit zero dead-letter rows). If defence-in-depth is wanted, `debug_assert!` the invariants instead of generating fake rows:

```rust
fn extract_callback_names(callbacks: &serde_json::Value) -> Vec<String> {
    debug_assert!(
        callbacks.as_array().is_some_and(|a| !a.is_empty()),
        "callbacks JSONB must be a non-empty array (DB CHECK constraint)"
    );
    callbacks
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, v)| {
            v.get("name")
                .and_then(|n| n.as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("[{i}]"))
        })
        .collect()
}
```

**Why this fix**

Removing dead branches makes the function's contract match the schema's invariants. The remaining tests then exercise only reachable behaviour, and a CHECK-constraint regression in a future migration would be caught by the `debug_assert!` in tests rather than silently emitting `<unknown>` rows.

---

## Summary

| # | Title | File:Line | Severity | Category | Status | Notes |
|---|-------|-----------|----------|----------|--------|-------|
| 1 | Redundant `catch_unwind` | `crates/core/src/scheduler.rs:163` | Medium | Idiom | DONE | |
| 2 | Unstable `let_chains` | `crates/core/src/scheduler.rs:180` | Medium | Idiom | SKIPPED | `let_chains` is stable in Rust edition 2024 (this project); Rust 1.91 + edition = "2024"; clippy prefers the collapsed form |
| 3 | Hardcoded `FETCH_LIMIT` | `crates/core/src/scheduler.rs:141` | Low | Config | DONE | Added `schedule_batch_size` to `DispatchConfig`/`DispatchSettings` with default 500 |
| 4 | `max_callbacks_per_event` upper bound | `crates/core/src/config.rs:469` | Medium | Config | DONE | |
| 5 | `KeyRing::load` result discarded | `crates/bin/src/main.rs:64` | Low | Idiom | DONE | |
| 6 | Cursor advances past events whose DB writes fail (data loss) | `crates/core/src/scheduler.rs:147-228` | High | Correctness | DONE | Cursor only advances after all DB writes for an event succeed; DB errors propagate so the wake loop retries |
| 7 | N+1 inserts when dead-lettering invalid/oversized callbacks | `crates/core/src/scheduler.rs:170-228` | Medium | Performance | DONE | Added `create_invalid_deliveries` (UNNEST batch) to `Repo` + `PgRepo`; both dead-letter paths now use one round-trip per event |
| 8 | Array-level rejections (too-many-callbacks) skip event silently | `crates/core/src/scheduler.rs:194-202` | Medium | Correctness | DONE | Array-level rejections now dead-letter each named entry via `create_invalid_deliveries`; truly corrupt non-array JSONB is still silently skipped |
| 9 | Wake loop runs a full cycle per NOTIFY; no notification draining or `next_poll` reset | `crates/core/src/scheduler.rs:73-122` | Low | Performance | DONE | Added `try_recv` drain after select; `next_poll` reset after every cycle |
| 10 | Wake-loop error branch has no backoff or rate-limit on sustained DB failure | `crates/core/src/scheduler.rs:111-121` | Low | Observability | DONE | Added `consecutive_errors` counter + capped exponential backoff (100 ms × 2^n, capped at poll_interval) |
| 11 | `extract_callback_names` carries unreachable fallbacks for non-array / empty inputs | `crates/core/src/scheduler.rs:243-259` | Low | Idiom | DONE | Replaced `<unknown>` fallback branches with `debug_assert!` + `into_iter().flatten()` |

> **Instructions for the implementing LLM:**
> - Change `TODO` to `DONE` once a finding is fully addressed.
> - If a finding is intentionally skipped or cannot be applied, change `TODO` to `SKIPPED` and add a short reason in the **Notes** column.
> - Do not delete rows — the table is the authoritative implementation log.
