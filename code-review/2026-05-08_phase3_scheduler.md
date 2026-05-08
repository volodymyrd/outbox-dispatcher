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

## Summary

| # | Title | File:Line | Severity | Category | Status | Notes |
|---|-------|-----------|----------|----------|--------|-------|
| 1 | Redundant `catch_unwind` | `crates/core/src/scheduler.rs:163` | Medium | Idiom | TODO | |
| 2 | Unstable `let_chains` | `crates/core/src/scheduler.rs:180` | Medium | Idiom | TODO | |
| 3 | Hardcoded `FETCH_LIMIT` | `crates/core/src/scheduler.rs:141` | Low | Config | TODO | |
| 4 | `max_callbacks_per_event` upper bound | `crates/core/src/config.rs:469` | Medium | Config | TODO | |
| 5 | `KeyRing::load` result discarded | `crates/bin/src/main.rs:64` | Low | Idiom | TODO | |

> **Instructions for the implementing LLM:**
> - Change `TODO` to `DONE` once a finding is fully addressed.
> - If a finding is intentionally skipped or cannot be applied, change `TODO` to `SKIPPED` and add a short reason in the **Notes** column.
> - Do not delete rows — the table is the authoritative implementation log.
