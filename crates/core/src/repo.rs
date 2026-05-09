use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::config::DispatchConfig;
use crate::error::Result;
use crate::schema::{
    CallbackTarget, DeadLetterRow, DeliveryRow, DueDelivery, EventWithDeliveries,
    ExternalPendingRow, PageParams, RawEvent, RawEventSerializable, SweepReport,
};

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
    ///
    /// Rows whose callback spec cannot be parsed are dead-lettered in-place and
    /// excluded from the returned batch so a single corrupt row cannot stall the
    /// queue ("poison pill" guard).
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
    config: DispatchConfig,
}

impl PgRepo {
    pub fn new(pool: PgPool, config: DispatchConfig) -> Self {
        Self { pool, config }
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
        // Single batched INSERT via UNNEST: one round-trip regardless of fan-out.
        let names: Vec<String> = callbacks.iter().map(|cb| cb.name.clone()).collect();
        let modes: Vec<String> = callbacks
            .iter()
            .map(|cb| cb.mode.as_str().to_string())
            .collect();
        sqlx::query!(
            r#"
            INSERT INTO outbox_deliveries
                (event_id, callback_name, completion_mode, available_at)
            SELECT $1, name, mode, now()
            FROM UNNEST($2::text[], $3::text[]) AS t(name, mode)
            ON CONFLICT (event_id, callback_name) DO NOTHING
            "#,
            event_id,
            &names as &[String],
            &modes as &[String],
        )
        .execute(&self.pool)
        .await?;
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
            match extract_callback_target(&row.callbacks, &row.callback_name, &self.config) {
                Ok(target) => due.push(DueDelivery {
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
                }),
                Err(err) => {
                    // Poison-pill guard: a structurally invalid callback spec would otherwise
                    // halt the entire queue (this row keeps its `available_at` at the head).
                    // Dead-letter it so the rest of the batch can proceed.
                    tracing::error!(
                        delivery_id = row.delivery_id,
                        event_id = %row.event_id,
                        callback_name = %row.callback_name,
                        error = %err,
                        "dead-lettering delivery with invalid callback spec",
                    );
                    let reason_owned = format!("invalid callback spec: {err}");
                    let reason = truncate_at_char_boundary(&reason_owned, 4096);
                    if let Err(db_err) = sqlx::query!(
                        r#"
                        UPDATE outbox_deliveries
                        SET dead_letter  = TRUE,
                            last_error   = $2,
                            locked_until = NULL
                        WHERE id = $1
                        "#,
                        row.delivery_id,
                        reason,
                    )
                    .execute(&self.pool)
                    .await
                    {
                        tracing::error!(
                            delivery_id = row.delivery_id,
                            error = %db_err,
                            "failed to dead-letter poison-pill delivery",
                        );
                    }
                }
            }
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
        // Find the most recently inserted delivery row (O(1) via the BIGSERIAL PK),
        // then join once to resolve its event's sequential id.
        // This avoids scanning outbox_events in reverse when there is a large gap of
        // events without delivery rows (e.g. after a downtime or publisher burst).
        let cursor = sqlx::query_scalar!(
            r#"
            SELECT COALESCE(
                (SELECT e.id
                 FROM outbox_events e
                 WHERE e.event_id = (
                     SELECT d.event_id
                     FROM outbox_deliveries d
                     ORDER BY d.id DESC
                     LIMIT 1
                 )),
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
///
/// Re-runs the full structural validation from [`crate::callbacks::parse_callbacks`]
/// at dispatch time as a defense-in-depth check: if the row was mutated after schedule
/// (admin tool, replication anomaly, or schema drift) to contain a reserved header,
/// private-IP target, or `http://` URL, the dispatcher refuses to honor it.
///
/// Uses [`crate::callbacks::validate_named_callback`] so only the target entry is
/// fully parsed — O(N) scan per delivery rather than O(N²) re-materialisation of
/// every sibling callback's headers/backoff.
fn extract_callback_target(
    callbacks: &serde_json::Value,
    callback_name: &str,
    config: &DispatchConfig,
) -> Result<CallbackTarget> {
    match crate::callbacks::validate_named_callback(callbacks, callback_name, config) {
        Ok(target) => Ok(target),
        Err(crate::callbacks::NamedCallbackError::Invalid(reason)) => {
            Err(crate::error::Error::InvalidData(format!(
                "callback '{callback_name}' failed structural revalidation at dispatch time: {reason}"
            )))
        }
        Err(crate::callbacks::NamedCallbackError::NotFound) => {
            Err(crate::error::Error::CallbackTargetMissing(format!(
                "callback '{callback_name}' not found in event callbacks array"
            )))
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_config() -> DispatchConfig {
        // Allow unsigned callbacks so the minimal specs in these tests pass
        // schedule-time validation (they omit signing_key_id).
        DispatchConfig {
            allow_unsigned_callbacks: true,
            ..Default::default()
        }
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
        let target = extract_callback_target(&callbacks, "notify", &test_config()).unwrap();
        assert_eq!(target.name, "notify");
        assert_eq!(target.url, "https://example.com/hook");
        assert_eq!(target.mode, crate::schema::CompletionMode::Managed);
        assert_eq!(target.max_attempts, test_config().max_attempts);
    }

    #[test]
    fn extract_callback_target_not_an_array() {
        let callbacks = json!({"name": "notify", "url": "https://example.com"});
        let err = extract_callback_target(&callbacks, "notify", &test_config()).unwrap_err();
        assert!(matches!(err, crate::error::Error::InvalidData(_)));
    }

    #[test]
    fn extract_callback_target_too_many_callbacks_surfaces_real_reason() {
        // Regression for 2026-05-08 code review Finding 3: an operator who lowered
        // `max_callbacks_per_event` below an already-scheduled event's fan-out would
        // previously see a misleading "not found" error. Array-level rejections must
        // now surface as InvalidData with the actual reason.
        let config = DispatchConfig {
            max_callbacks_per_event: 1,
            allow_unsigned_callbacks: true,
            ..Default::default()
        };
        let callbacks = json!([
            {"name": "first",  "url": "https://a.example.com/"},
            {"name": "second", "url": "https://b.example.com/"}
        ]);
        let err = extract_callback_target(&callbacks, "first", &config).unwrap_err();
        let crate::error::Error::InvalidData(msg) = err else {
            panic!("expected InvalidData, got {err:?}");
        };
        assert!(msg.contains("too many callbacks"), "got: {msg}");
        assert!(msg.contains("revalidation"), "got: {msg}");
    }

    #[test]
    fn extract_callback_target_missing_name() {
        let callbacks = json!([
            {"name": "other", "url": "https://example.com/other"}
        ]);
        let err = extract_callback_target(&callbacks, "notify", &test_config()).unwrap_err();
        assert!(matches!(err, crate::error::Error::CallbackTargetMissing(_)));
    }

    #[test]
    fn extract_callback_target_malformed_spec() {
        // Missing required "url" field — parse_callbacks flags this in its `invalid` list,
        // so dispatch-time revalidation returns `InvalidData`.
        let callbacks = json!([{"name": "notify"}]);
        let err = extract_callback_target(&callbacks, "notify", &test_config()).unwrap_err();
        assert!(matches!(err, crate::error::Error::InvalidData(_)));
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
            "external_completion_timeout_seconds": 3600,
            "max_completion_cycles": 7
        }]);
        let target = extract_callback_target(&callbacks, "notify", &test_config()).unwrap();
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
        let target = extract_callback_target(&callbacks, "notify", &test_config()).unwrap();
        assert_eq!(target.signing_key_id.as_deref(), Some("key-abc"));
    }

    #[test]
    fn extract_callback_target_picks_correct_entry_from_multi() {
        let callbacks = json!([
            {"name": "alpha", "url": "https://alpha.example.com"},
            {"name": "beta",  "url": "https://beta.example.com"}
        ]);
        let target = extract_callback_target(&callbacks, "beta", &test_config()).unwrap();
        assert_eq!(target.url, "https://beta.example.com");
    }

    // ── Dispatch-time revalidation: security regression tests ────────────────
    //
    // These lock in the defense-in-depth guarantee that mutating the callbacks JSON
    // between schedule and dispatch cannot resurrect a structurally invalid spec.

    #[test]
    fn extract_callback_target_rejects_mutated_reserved_header() {
        // An attacker-controlled (or admin-mutated) row carrying an Authorization header
        // must be refused at dispatch time even though it was "approved" at schedule time.
        let callbacks = json!([{
            "name": "notify",
            "url": "https://example.com/hook",
            "headers": { "Authorization": "Bearer secret" }
        }]);
        let err = extract_callback_target(&callbacks, "notify", &test_config()).unwrap_err();
        let crate::error::Error::InvalidData(msg) = err else {
            panic!("expected InvalidData, got {err:?}");
        };
        assert!(msg.contains("revalidation"), "got: {msg}");
        assert!(msg.contains("Authorization"), "got: {msg}");
    }

    #[test]
    fn extract_callback_target_rejects_mutated_http_url() {
        // Flipping https:// to http:// after schedule time must be caught.
        let callbacks = json!([{
            "name": "notify",
            "url": "http://example.com/hook"
        }]);
        let err = extract_callback_target(&callbacks, "notify", &test_config()).unwrap_err();
        assert!(matches!(err, crate::error::Error::InvalidData(_)));
    }

    #[test]
    fn extract_callback_target_rejects_mutated_private_host() {
        let callbacks = json!([{
            "name": "notify",
            "url": "https://metadata.google.internal/hook"
        }]);
        let err = extract_callback_target(&callbacks, "notify", &test_config()).unwrap_err();
        assert!(matches!(err, crate::error::Error::InvalidData(_)));
    }

    // ── Parser contract: schedule and dispatch agree on valid callbacks ──────

    #[test]
    fn dispatch_parser_matches_schedule_parser_for_valid_callbacks() {
        // A callback that passes parse_callbacks must produce a structurally equivalent
        // target via extract_callback_target. Locks the contract between the two parsers.
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "headers": { "X-Service": "svc" },
            "max_attempts": 3,
        });
        let arr = json!([cb]);
        let cfg = DispatchConfig::default();
        let mut parsed = crate::callbacks::parse_callbacks(&arr, &cfg);
        assert_eq!(parsed.valid.len(), 1);
        let from_schedule = parsed.valid.remove(0);
        let from_dispatch = extract_callback_target(&arr, "abc", &cfg).unwrap();
        assert_eq!(from_schedule.name, from_dispatch.name);
        assert_eq!(from_schedule.url, from_dispatch.url);
        assert_eq!(from_schedule.headers, from_dispatch.headers);
        assert_eq!(from_schedule.max_attempts, from_dispatch.max_attempts);
        assert_eq!(from_schedule.mode, from_dispatch.mode);
        assert_eq!(from_schedule.signing_key_id, from_dispatch.signing_key_id);
        assert_eq!(from_schedule.backoff, from_dispatch.backoff);
        assert_eq!(from_schedule.timeout, from_dispatch.timeout);
        assert_eq!(
            from_schedule.external_completion_timeout,
            from_dispatch.external_completion_timeout
        );
        assert_eq!(
            from_schedule.max_completion_cycles,
            from_dispatch.max_completion_cycles
        );
    }
}
