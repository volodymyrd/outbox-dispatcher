use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::Result;
use crate::schema::{
    CallbackTarget, CompletionMode, DeadLetterRow, DeliveryRow, DueDelivery, EventWithDeliveries,
    ExternalPendingRow, PageParams, RawEvent, RawEventSerializable, SweepReport,
};

/// Defaults applied when a callback spec omits optional fields.
#[derive(Debug, Clone)]
pub struct DispatchDefaults {
    pub max_attempts: u32,
    pub backoff: Vec<Duration>,
    pub timeout: Duration,
    pub max_completion_cycles: u32,
}

impl Default for DispatchDefaults {
    fn default() -> Self {
        Self {
            max_attempts: 6,
            backoff: vec![
                Duration::from_secs(30),
                Duration::from_secs(120),
                Duration::from_secs(600),
                Duration::from_secs(3600),
                Duration::from_secs(21600),
                Duration::from_secs(86400),
            ],
            timeout: Duration::from_secs(30),
            max_completion_cycles: 20,
        }
    }
}

#[async_trait]
pub trait Repo: Send + Sync {
    // ── Scheduling ─────────────────────────────────────────────────────────

    /// Returns events with `id > after_id`, ordered by id, up to `limit`.
    async fn fetch_new_events(&self, after_id: i64, limit: i64) -> Result<Vec<RawEvent>>;

    /// Inserts one delivery row per callback target; idempotent via UNIQUE constraint.
    async fn ensure_deliveries(&self, event_id: Uuid, callbacks: &[CallbackTarget]) -> Result<()>;

    /// Inserts a dead-lettered delivery row for a structurally invalid callback.
    async fn create_invalid_delivery(
        &self,
        event_id: Uuid,
        callback_name: &str,
        reason: &str,
    ) -> Result<()>;

    // ── Dispatch ───────────────────────────────────────────────────────────

    /// Returns due deliveries (available_at ≤ now, not locked, not dispatched).
    async fn fetch_due_deliveries(&self, batch_size: i64) -> Result<Vec<DueDelivery>>;

    /// Atomically sets `locked_until` and increments `attempts`; returns false if
    /// someone else took the row concurrently.
    ///
    /// Note: the `DueDelivery.attempts` visible to the caller reflects the count
    /// *before* this call. The in-progress attempt is `attempts + 1`. Callers must
    /// account for this when checking whether the next failure should dead-letter
    /// (i.e. compare `attempts + 1 >= max_attempts`, not `attempts >= max_attempts`).
    async fn lock_delivery(&self, id: i64, until: DateTime<Utc>) -> Result<bool>;

    /// Managed-mode success: sets both `dispatched_at` and `processed_at`.
    async fn mark_dispatched_managed(&self, id: i64) -> Result<()>;

    /// External-mode success: sets only `dispatched_at`; receiver writes `processed_at`.
    async fn mark_dispatched_external(&self, id: i64) -> Result<()>;

    /// Records a transient failure, advances `available_at`, optionally dead-letters.
    async fn mark_failure(
        &self,
        id: i64,
        error: &str,
        available_at: DateTime<Utc>,
        dead_letter: bool,
    ) -> Result<()>;

    // ── External-completion timeout sweep ──────────────────────────────────

    /// Resets hung external-mode rows for redelivery or dead-letters exhausted ones.
    ///
    /// Only rows whose callback spec includes `external_completion_timeout_seconds` are
    /// evaluated; rows without that field are intentionally skipped (treated as
    /// "wait forever" for external completion).
    async fn reset_hung_external(
        &self,
        now: DateTime<Utc>,
        max_completion_cycles: i32,
    ) -> Result<SweepReport>;

    // ── Startup ────────────────────────────────────────────────────────────

    /// Returns the highest `outbox_events.id` that has a delivery row, or 0.
    async fn recover_cursor(&self) -> Result<i64>;

    // ── Admin ──────────────────────────────────────────────────────────────

    async fn list_dead_letters(&self, page: PageParams) -> Result<Vec<DeadLetterRow>>;

    /// `older_than`: only rows where `now() - dispatched_at > older_than`.
    async fn list_external_pending(
        &self,
        page: PageParams,
        older_than: Option<Duration>,
    ) -> Result<Vec<ExternalPendingRow>>;

    /// Resets all mutable columns so the delivery re-enters the pending queue.
    ///
    /// Does not guard against an in-flight dispatcher that currently holds a lock on
    /// this row. Callers (admin API) should ensure the row is not actively locked
    /// before calling this. A `locked_until` guard will be added when the admin API
    /// is implemented in Phase 6.
    async fn retry_delivery(&self, id: i64) -> Result<bool>;

    /// Sets `processed_at = COALESCE(processed_at, now())`. Idempotent.
    async fn complete_delivery(&self, id: i64) -> Result<bool>;

    /// Operator escape hatch: sets `dead_letter = TRUE`.
    async fn abandon_delivery(&self, id: i64) -> Result<bool>;

    async fn fetch_event_with_deliveries(&self, event_id: Uuid) -> Result<EventWithDeliveries>;
}

// ── Postgres implementation ────────────────────────────────────────────────────

pub struct PgRepo {
    pool: PgPool,
    defaults: DispatchDefaults,
}

impl PgRepo {
    pub fn new(pool: PgPool, defaults: DispatchDefaults) -> Self {
        Self { pool, defaults }
    }
}

/// JSONB callback spec as written by the publisher.
#[derive(Debug, Deserialize)]
struct RawCallbackSpec {
    name: String,
    url: String,
    #[serde(default = "default_mode_str")]
    mode: String,
    signing_key_id: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
    max_attempts: Option<u32>,
    backoff_seconds: Option<Vec<u64>>,
    timeout_seconds: Option<u64>,
    external_completion_timeout_seconds: Option<u64>,
    max_completion_cycles: Option<u32>,
}

fn default_mode_str() -> String {
    "managed".to_string()
}

impl RawCallbackSpec {
    fn into_target(self, defaults: &DispatchDefaults) -> CallbackTarget {
        let mode = if self.mode == "external" {
            CompletionMode::External
        } else {
            CompletionMode::Managed
        };
        let timeout = self
            .timeout_seconds
            .map(Duration::from_secs)
            .unwrap_or(defaults.timeout);
        let max_attempts = self.max_attempts.unwrap_or(defaults.max_attempts);
        let backoff = self
            .backoff_seconds
            .map(|v| v.into_iter().map(Duration::from_secs).collect())
            .unwrap_or_else(|| defaults.backoff.clone());
        let max_completion_cycles = self
            .max_completion_cycles
            .unwrap_or(defaults.max_completion_cycles);
        let external_completion_timeout = self
            .external_completion_timeout_seconds
            .map(Duration::from_secs);

        CallbackTarget {
            name: self.name,
            url: self.url,
            mode,
            signing_key_id: self.signing_key_id,
            headers: self.headers,
            max_attempts,
            backoff,
            timeout,
            external_completion_timeout,
            max_completion_cycles,
        }
    }
}

/// Internal row type for `fetch_due_deliveries` — carries both delivery and event columns.
#[derive(sqlx::FromRow)]
struct DueDeliveryRow {
    delivery_id: i64,
    event_id: Uuid,
    attempts: i32,
    callback_name: String,
    kind: String,
    aggregate_type: String,
    aggregate_id: Uuid,
    payload: serde_json::Value,
    metadata: serde_json::Value,
    actor_id: Option<Uuid>,
    correlation_id: Option<Uuid>,
    causation_id: Option<Uuid>,
    created_at: DateTime<Utc>,
    callbacks: serde_json::Value,
}

#[async_trait]
impl Repo for PgRepo {
    async fn fetch_new_events(&self, after_id: i64, limit: i64) -> Result<Vec<RawEvent>> {
        let rows = sqlx::query_as!(
            RawEvent,
            r#"
            SELECT
                id,
                event_id,
                kind,
                aggregate_type,
                aggregate_id,
                payload,
                metadata,
                callbacks,
                actor_id,
                correlation_id,
                causation_id,
                created_at,
                octet_length(payload::text)::bigint AS "payload_size_bytes!"
            FROM outbox_events
            WHERE id > $1
            ORDER BY id
            LIMIT $2
            "#,
            after_id,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    async fn ensure_deliveries(&self, event_id: Uuid, callbacks: &[CallbackTarget]) -> Result<()> {
        if callbacks.is_empty() {
            return Ok(());
        }
        // Transaction ensures all-or-nothing insertion across the callback list.
        // For the typical single-callback case this adds one BEGIN/COMMIT round-trip;
        // acceptable given the small callback fan-out and correctness benefit.
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
        tx.commit().await?;
        Ok(())
    }

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

    async fn fetch_due_deliveries(&self, batch_size: i64) -> Result<Vec<DueDelivery>> {
        let rows = sqlx::query_as!(
            DueDeliveryRow,
            r#"
            SELECT
                d.id           AS delivery_id,
                d.event_id,
                d.attempts,
                d.callback_name,
                e.kind,
                e.aggregate_type,
                e.aggregate_id,
                e.payload,
                e.metadata,
                e.actor_id,
                e.correlation_id,
                e.causation_id,
                e.created_at,
                e.callbacks
            FROM outbox_deliveries d
            JOIN outbox_events e USING (event_id)
            WHERE d.dispatched_at IS NULL
              AND d.processed_at  IS NULL
              AND d.dead_letter   = FALSE
              AND d.available_at  <= now()
              AND (d.locked_until IS NULL OR d.locked_until < now())
            ORDER BY d.available_at, d.id
            LIMIT $1
            "#,
            batch_size,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut due = Vec::with_capacity(rows.len());
        for row in rows {
            let target =
                extract_callback_target(&row.callbacks, &row.callback_name, &self.defaults)?;
            due.push(DueDelivery {
                delivery_id: row.delivery_id,
                event_id: row.event_id,
                attempts: row.attempts,
                target,
                kind: row.kind,
                aggregate_type: row.aggregate_type,
                aggregate_id: row.aggregate_id,
                payload: row.payload,
                metadata: row.metadata,
                actor_id: row.actor_id,
                correlation_id: row.correlation_id,
                causation_id: row.causation_id,
                created_at: row.created_at,
            });
        }
        Ok(due)
    }

    async fn lock_delivery(&self, id: i64, until: DateTime<Utc>) -> Result<bool> {
        let result = sqlx::query!(
            r#"
            UPDATE outbox_deliveries
            SET locked_until    = $1,
                attempts        = attempts + 1,
                last_attempt_at = now()
            WHERE id            = $2
              AND (locked_until IS NULL OR locked_until < now())
              AND dispatched_at IS NULL
              AND dead_letter   = FALSE
            "#,
            until,
            id,
        )
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() == 1)
    }

    async fn mark_dispatched_managed(&self, id: i64) -> Result<()> {
        sqlx::query!(
            r#"
            UPDATE outbox_deliveries
            SET dispatched_at = now(),
                processed_at  = now(),
                locked_until  = NULL
            WHERE id = $1
            "#,
            id,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn mark_dispatched_external(&self, id: i64) -> Result<()> {
        sqlx::query!(
            r#"
            UPDATE outbox_deliveries
            SET dispatched_at = now(),
                locked_until  = NULL
            WHERE id = $1
            "#,
            id,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn mark_failure(
        &self,
        id: i64,
        error: &str,
        available_at: DateTime<Utc>,
        dead_letter: bool,
    ) -> Result<()> {
        let error = truncate_at_char_boundary(error, 4096);
        sqlx::query!(
            r#"
            UPDATE outbox_deliveries
            SET last_error   = $2,
                available_at = $3,
                dead_letter  = $4,
                locked_until = NULL
            WHERE id = $1
            "#,
            id,
            error,
            available_at,
            dead_letter,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn reset_hung_external(
        &self,
        now: DateTime<Utc>,
        max_completion_cycles: i32,
    ) -> Result<SweepReport> {
        // LATERAL inside UPDATE...FROM cannot reference the update target table.
        // A CTE resolves this: LATERAL runs inside the SELECT where both d and e are FROM items.
        let rows = sqlx::query!(
            r#"
            WITH to_update AS (
                SELECT
                    d.id,
                    d.completion_cycles,
                    d.dispatched_at,
                    d.available_at,
                    d.attempts,
                    cb_match.max_cycles
                FROM outbox_deliveries d
                JOIN outbox_events e ON e.event_id = d.event_id
                JOIN LATERAL (
                    SELECT
                        (cb->>'external_completion_timeout_seconds')::int AS timeout_s,
                        COALESCE((cb->>'max_completion_cycles')::int, $2) AS max_cycles
                    FROM jsonb_array_elements(e.callbacks) cb
                    WHERE cb->>'name' = d.callback_name
                    LIMIT 1
                ) cb_match ON cb_match.timeout_s IS NOT NULL
                WHERE d.completion_mode = 'external'
                  AND d.dispatched_at   IS NOT NULL
                  AND d.processed_at    IS NULL
                  AND d.dead_letter     = FALSE
                  AND d.dispatched_at + (cb_match.timeout_s || ' seconds')::interval < $1
            )
            UPDATE outbox_deliveries
            SET completion_cycles = to_update.completion_cycles + 1,
                dispatched_at = CASE
                    WHEN to_update.completion_cycles + 1 >= to_update.max_cycles
                    THEN to_update.dispatched_at
                    ELSE NULL
                END,
                available_at = CASE
                    WHEN to_update.completion_cycles + 1 >= to_update.max_cycles
                    THEN to_update.available_at
                    ELSE $1
                END,
                attempts = CASE
                    WHEN to_update.completion_cycles + 1 >= to_update.max_cycles
                    THEN to_update.attempts
                    ELSE 0
                END,
                last_error = CASE
                    WHEN to_update.completion_cycles + 1 >= to_update.max_cycles
                    THEN 'external_completion_cycles_exhausted'
                    ELSE NULL
                END,
                dead_letter  = (to_update.completion_cycles + 1 >= to_update.max_cycles),
                locked_until = NULL
            FROM to_update
            WHERE outbox_deliveries.id = to_update.id
            RETURNING outbox_deliveries.id AS "id!", outbox_deliveries.dead_letter AS "dead_letter!"
            "#,
            now,
            max_completion_cycles,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut report = SweepReport::default();
        for row in rows {
            if row.dead_letter {
                report.exhausted += 1;
            } else {
                report.reset += 1;
            }
        }
        Ok(report)
    }

    async fn recover_cursor(&self) -> Result<i64> {
        let cursor = sqlx::query_scalar!(
            r#"
            SELECT COALESCE(
                (SELECT e.id
                 FROM outbox_deliveries d
                 JOIN outbox_events e ON e.event_id = d.event_id
                 ORDER BY e.id DESC
                 LIMIT 1),
                0
            ) AS "cursor!"
            "#,
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(cursor)
    }

    async fn list_dead_letters(&self, page: PageParams) -> Result<Vec<DeadLetterRow>> {
        let rows = sqlx::query_as!(
            DeadLetterRow,
            r#"
            SELECT
                d.id           AS delivery_id,
                d.event_id,
                e.kind         AS event_kind,
                d.callback_name,
                d.completion_mode,
                d.attempts,
                d.last_error,
                d.last_attempt_at,
                d.created_at
            FROM outbox_deliveries d
            JOIN outbox_events e USING (event_id)
            WHERE d.dead_letter = TRUE
              AND ($1::bigint IS NULL OR d.id < $1)
              AND ($2::text   IS NULL OR d.callback_name = $2)
            ORDER BY d.id DESC
            LIMIT $3
            "#,
            page.cursor,
            page.callback_name,
            page.limit,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    async fn list_external_pending(
        &self,
        page: PageParams,
        older_than: Option<Duration>,
    ) -> Result<Vec<ExternalPendingRow>> {
        let older_than_secs = older_than.map(|d| d.as_secs_f64());
        let rows = sqlx::query_as!(
            ExternalPendingRow,
            r#"
            SELECT
                d.id           AS delivery_id,
                d.event_id,
                e.kind         AS event_kind,
                d.callback_name,
                d.attempts,
                d.dispatched_at AS "dispatched_at!",
                d.created_at
            FROM outbox_deliveries d
            JOIN outbox_events e USING (event_id)
            WHERE d.completion_mode = 'external'
              AND d.dispatched_at  IS NOT NULL
              AND d.processed_at   IS NULL
              AND d.dead_letter    = FALSE
              AND ($1::bigint IS NULL OR d.id < $1)
              AND ($2::text   IS NULL OR d.callback_name = $2)
              AND ($3::float8 IS NULL
                   OR EXTRACT(EPOCH FROM (now() - d.dispatched_at)) > $3)
            ORDER BY d.dispatched_at
            LIMIT $4
            "#,
            page.cursor,
            page.callback_name,
            older_than_secs,
            page.limit,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    async fn retry_delivery(&self, id: i64) -> Result<bool> {
        let result = sqlx::query!(
            r#"
            UPDATE outbox_deliveries
            SET attempts          = 0,
                last_error        = NULL,
                available_at      = now(),
                dispatched_at     = NULL,
                processed_at      = NULL,
                completion_cycles = 0,
                dead_letter       = FALSE,
                locked_until      = NULL
            WHERE id = $1
            "#,
            id,
        )
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() == 1)
    }

    async fn complete_delivery(&self, id: i64) -> Result<bool> {
        let result = sqlx::query!(
            r#"
            UPDATE outbox_deliveries
            SET processed_at = COALESCE(processed_at, now())
            WHERE id = $1
            "#,
            id,
        )
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() == 1)
    }

    async fn abandon_delivery(&self, id: i64) -> Result<bool> {
        let result = sqlx::query!(
            r#"
            UPDATE outbox_deliveries
            SET dead_letter = TRUE,
                last_error  = 'abandoned by operator'
            WHERE id = $1
            "#,
            id,
        )
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() == 1)
    }

    async fn fetch_event_with_deliveries(&self, event_id: Uuid) -> Result<EventWithDeliveries> {
        let event = sqlx::query_as!(
            RawEventSerializable,
            r#"
            SELECT
                id,
                event_id,
                kind,
                aggregate_type,
                aggregate_id,
                payload,
                metadata,
                callbacks,
                actor_id,
                correlation_id,
                causation_id,
                created_at
            FROM outbox_events
            WHERE event_id = $1
            "#,
            event_id,
        )
        .fetch_one(&self.pool)
        .await?;

        let deliveries = sqlx::query_as!(
            DeliveryRow,
            r#"
            SELECT
                id,
                event_id,
                callback_name,
                completion_mode,
                attempts,
                last_error,
                last_attempt_at,
                available_at,
                locked_until,
                dispatched_at,
                processed_at,
                completion_cycles,
                dead_letter,
                created_at
            FROM outbox_deliveries
            WHERE event_id = $1
            ORDER BY id
            "#,
            event_id,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(EventWithDeliveries { event, deliveries })
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

/// Returns the longest prefix of `s` whose byte length does not exceed `max_bytes`,
/// always cutting on a UTF-8 character boundary to avoid a panic.
fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Finds a callback by name in the event's callbacks JSONB array and parses it.
fn extract_callback_target(
    callbacks: &serde_json::Value,
    callback_name: &str,
    defaults: &DispatchDefaults,
) -> Result<CallbackTarget> {
    let arr = callbacks
        .as_array()
        .ok_or_else(|| crate::error::Error::InvalidData("callbacks is not an array".to_string()))?;

    let spec_value = arr
        .iter()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some(callback_name))
        .ok_or_else(|| {
            crate::error::Error::CallbackTargetMissing(format!(
                "callback '{callback_name}' not found in event callbacks array"
            ))
        })?;

    let spec: RawCallbackSpec = serde_json::from_value(spec_value.clone())?;
    Ok(spec.into_target(defaults))
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn defaults() -> DispatchDefaults {
        DispatchDefaults::default()
    }

    // ── truncate_at_char_boundary ─────────────────────────────────────────────

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate_at_char_boundary("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact_boundary() {
        assert_eq!(truncate_at_char_boundary("hello", 5), "hello");
    }

    #[test]
    fn truncate_ascii_at_byte_limit() {
        assert_eq!(truncate_at_char_boundary("hello world", 5), "hello");
    }

    #[test]
    fn truncate_does_not_split_multibyte_char() {
        // "é" is U+00E9, encoded as two bytes [0xC3, 0xA9].
        // Truncating at byte 1 would split the codepoint; the helper must back up to 0.
        let s = "é";
        assert_eq!(s.len(), 2);
        let result = truncate_at_char_boundary(s, 1);
        assert!(s.is_char_boundary(result.len()));
        assert_eq!(result, "");
    }

    #[test]
    fn truncate_multibyte_prefix_preserved() {
        // "héllo" — 'é' is 2 bytes; total 6 bytes.
        let s = "héllo";
        let result = truncate_at_char_boundary(s, 4);
        // bytes 0..4 cover 'h','é'(2b),'l' — but 'é' at byte 1-2, 'l' at byte 3 → 4 is OK
        assert!(s.is_char_boundary(result.len()));
        assert!(result.len() <= 4);
    }

    #[test]
    fn truncate_zero_max_bytes_returns_empty() {
        assert_eq!(truncate_at_char_boundary("hello", 0), "");
    }

    #[test]
    fn truncate_does_not_split_4byte_emoji() {
        // '🦀' is U+1F980, encoded as 4 bytes [0xF0, 0x9F, 0xA6, 0x80].
        // Any limit 1-3 must walk back to 0; limit 4 yields the whole char.
        let s = "🦀";
        assert_eq!(s.len(), 4);
        assert_eq!(truncate_at_char_boundary(s, 3), "");
        assert_eq!(truncate_at_char_boundary(s, 4), "🦀");
    }

    // ── extract_callback_target ───────────────────────────────────────────────

    #[test]
    fn extract_callback_target_minimal_spec() {
        let callbacks = json!([
            {"name": "notify", "url": "https://example.com/hook"}
        ]);
        let target = extract_callback_target(&callbacks, "notify", &defaults()).unwrap();
        assert_eq!(target.name, "notify");
        assert_eq!(target.url, "https://example.com/hook");
        assert_eq!(target.mode, crate::schema::CompletionMode::Managed);
        assert_eq!(target.max_attempts, defaults().max_attempts);
    }

    #[test]
    fn extract_callback_target_not_an_array() {
        let callbacks = json!({"name": "notify", "url": "https://example.com"});
        let err = extract_callback_target(&callbacks, "notify", &defaults()).unwrap_err();
        assert!(matches!(err, crate::error::Error::InvalidData(_)));
    }

    #[test]
    fn extract_callback_target_missing_name() {
        let callbacks = json!([
            {"name": "other", "url": "https://example.com/other"}
        ]);
        let err = extract_callback_target(&callbacks, "notify", &defaults()).unwrap_err();
        assert!(matches!(err, crate::error::Error::CallbackTargetMissing(_)));
    }

    #[test]
    fn extract_callback_target_malformed_spec() {
        // missing required "url" field
        let callbacks = json!([{"name": "notify"}]);
        let err = extract_callback_target(&callbacks, "notify", &defaults()).unwrap_err();
        assert!(matches!(err, crate::error::Error::Serialization(_)));
    }

    #[test]
    fn extract_callback_target_overrides_defaults() {
        let callbacks = json!([{
            "name": "notify",
            "url": "https://example.com/hook",
            "mode": "external",
            "max_attempts": 3,
            "backoff_seconds": [10, 20],
            "timeout_seconds": 5,
            "max_completion_cycles": 7
        }]);
        let target = extract_callback_target(&callbacks, "notify", &defaults()).unwrap();
        assert_eq!(target.mode, crate::schema::CompletionMode::External);
        assert_eq!(target.max_attempts, 3);
        assert_eq!(
            target.backoff,
            vec![Duration::from_secs(10), Duration::from_secs(20)]
        );
        assert_eq!(target.timeout, Duration::from_secs(5));
        assert_eq!(target.max_completion_cycles, 7);
    }

    #[test]
    fn extract_callback_target_signing_key_id() {
        let callbacks = json!([{
            "name": "notify",
            "url": "https://example.com/hook",
            "signing_key_id": "key-abc"
        }]);
        let target = extract_callback_target(&callbacks, "notify", &defaults()).unwrap();
        assert_eq!(target.signing_key_id.as_deref(), Some("key-abc"));
    }

    #[test]
    fn extract_callback_target_picks_correct_entry_from_multi() {
        let callbacks = json!([
            {"name": "alpha", "url": "https://alpha.example.com"},
            {"name": "beta",  "url": "https://beta.example.com"}
        ]);
        let target = extract_callback_target(&callbacks, "beta", &defaults()).unwrap();
        assert_eq!(target.url, "https://beta.example.com");
    }
}
