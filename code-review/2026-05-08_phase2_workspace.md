# Code Review — workspace

**Date:** 2026-05-08T10:30:00Z
**Branch:** phase2
**Reviewed by:** Gemini (using review command)
**Scope:** full workspace

---

## Findings

### Finding 1 — Poison Pill in `fetch_due_deliveries`

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/repo.rs:260` |
| **Severity** | High |
| **Category** | Correctness |

**Problem**

If a delivery row is associated with an event that has corrupt or structurally invalid JSON in its `callbacks` column, `extract_callback_target` will return an `Err`. This error bubbles up through `fetch_due_deliveries`, causing the entire dispatcher batch to fail. Since the dispatcher usually polls in order of `available_at`, a single "poisoned" row at the head of the queue will halt all delivery progress for the entire service.

**Context** (surrounding code as it exists today)

```rust
// crates/core/src/repo.rs lines 257-264
        let mut due = Vec::with_capacity(rows.len());
        for row in rows {
            let target =
                extract_callback_target(&row.callbacks, &row.callback_name, &self.defaults)?;
            due.push(DueDelivery {
                delivery_id: row.delivery_id,
```

**Recommended fix**

The dispatcher should be resilient to individual corrupt rows. `fetch_due_deliveries` (or the caller in the dispatch loop) should catch parsing errors for a single row, log the critical failure, and ideally move that specific delivery to a `dead_letter` state so it doesn't block the rest of the queue.

**Why this fix**

Upholds the availability of the service; a single invalid data point in the database should not be a single point of failure for the entire outbox pipeline.

---

### Finding 2 — Inefficient loop in `ensure_deliveries`

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/repo.rs:206` |
| **Severity** | Low |
| **Category** | Performance |

**Problem**

The implementation iterates over callbacks and executes an `INSERT` statement for each one. While wrapped in a transaction, this still results in N round-trips to the database (or at least N prepared statement executions) per event expansion.

**Context** (surrounding code as it exists today)

```rust
// crates/core/src/repo.rs lines 206-218
        let mut tx = self.pool.begin().await?;
        for cb in callbacks {
            sqlx::query!(
                r#"
                INSERT INTO outbox_deliveries
                    (event_id, callback_name, completion_mode, available_at)
                VALUES ($1, $2, $3, now())
                ON CONFLICT (event_id, callback_name) DO NOTHING
                "#,
                event_id,
                cb.name,
                cb.mode.as_str(),
            )
            .execute(&mut *tx)
            .await?;
        }
```

**Recommended fix**

Use a single multi-row `INSERT` statement or `UNNEST` to insert all deliveries in a single batch.

```rust
        sqlx::query!(
            r#"
            INSERT INTO outbox_deliveries (event_id, callback_name, completion_mode, available_at)
            SELECT $1, * FROM UNNEST($2::text[], $3::text[])
            ON CONFLICT (event_id, callback_name) DO NOTHING
            "#,
            event_id,
            &callback_names as &[String],
            &completion_modes as &[String],
        )
        .execute(&self.pool)
        .await?;
```

**Why this fix**

Reduces database round-trips and improves throughput, especially for events with many callback targets.

---

### Finding 3 — `CompletionMode` manual string matching

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/repo.rs:136` |
| **Severity** | Low |
| **Category** | Idiom |

**Problem**

`RawCallbackSpec` uses a `String` for the `mode` field and a manual `default_mode_str` function, followed by an explicit `if/else` to convert it to `CompletionMode`. This is less idiomatic than using `serde` to deserialize the enum directly.

**Context** (surrounding code as it exists today)

```rust
// crates/core/src/repo.rs lines 136-151
    #[serde(default = "default_mode_str")]
    mode: String,
// ...
impl RawCallbackSpec {
    fn into_target(self, defaults: &DispatchDefaults) -> CallbackTarget {
        let mode = if self.mode == "external" {
            CompletionMode::External
        } else {
            CompletionMode::Managed
        };
```

**Recommended fix**

Derive `Default` on `CompletionMode` and use it directly in `RawCallbackSpec`.

```rust
// crates/core/src/schema.rs
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompletionMode {
    #[default]
    Managed,
    External,
}

// crates/core/src/repo.rs
struct RawCallbackSpec {
    #[serde(default)]
    mode: CompletionMode,
    // ...
}
```

**Why this fix**

Leverages `serde` for type-safe deserialization and reduces boilerplate.

---

## Summary

| # | Title | File:Line | Severity | Category | Status | Notes |
|---|-------|-----------|----------|----------|--------|-------|
| 1 | Poison Pill in `fetch_due_deliveries` | `crates/core/src/repo.rs:260` | High | Correctness | TODO | |
| 2 | Inefficient loop in `ensure_deliveries` | `crates/core/src/repo.rs:206` | Low | Performance | TODO | |
| 3 | `CompletionMode` manual string matching | `crates/core/src/repo.rs:136` | Low | Idiom | TODO | |

> **Instructions for the implementing LLM:**
> - Change `TODO` to `DONE` once a finding is fully addressed.
> - If a finding is intentionally skipped or cannot be applied, change `TODO` to `SKIPPED` and add a short reason in the **Notes** column.
> - Do not delete rows — the table is the authoritative implementation log.
