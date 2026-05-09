//! Scheduler — event discovery, callback expansion, and the LISTEN/NOTIFY wake loop.
//!
//! # Responsibilities
//!
//! 1. **Cursor recovery** — on startup, recover the high-water mark from the database
//!    so the first poll resumes from where the previous run left off.
//! 2. **Event scheduling** — for each new event, expand its `callbacks` array into
//!    `outbox_deliveries` rows, validating each callback structurally. Invalid callbacks
//!    are immediately dead-lettered; events whose payload exceeds the configured limit are
//!    dead-lettered in bulk. Poison-pill rows (corrupt JSONB that cannot be deserialized)
//!    are logged, metered, and skipped so one bad row cannot stall the entire queue.
//! 3. **Wake source** — a `PgListener` that listens on `outbox_events_new` wakes the loop
//!    with sub-second latency on every INSERT; a periodic poll-timer is the fallback if
//!    the LISTEN connection is lost or too quiet.
//! 4. **Graceful shutdown** — a `CancellationToken` lets the caller signal shutdown; the
//!    loop drains any in-progress scheduling step and returns cleanly.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sqlx::postgres::PgListener;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::callbacks::{parse_callbacks, payload_too_large_error};
use crate::config::DispatchConfig;
use crate::error::Result;
use crate::repo::Repo;

// ── Public API ─────────────────────────────────────────────────────────────────

/// A shared boolean that reflects whether the `PgListener` connection is up.
///
/// `true` means the LISTEN connection is established; `false` means it is
/// disconnected or has not yet connected. This is updated by the wake loop.
pub type ListenerStatus = Arc<AtomicBool>;

/// Run the scheduler loop until `shutdown` is cancelled.
///
/// - Recovers the event cursor on startup.
/// - Listens on `config.notify_channel` for low-latency wakeups.
/// - Also wakes on `config.poll_interval` so missed NOTIFYs are never permanently lost.
/// - On each wake, calls [`schedule_new_deliveries`] to expand new events into delivery rows.
///
/// The `listener_status` value is set to `true` while the LISTEN connection is up and
/// `false` when it drops. Callers (e.g. the admin `/ready` endpoint) can inspect it.
pub async fn run_scheduler(
    repo: Arc<dyn Repo>,
    config: DispatchConfig,
    mut listener: PgListener,
    listener_status: ListenerStatus,
    shutdown: CancellationToken,
) -> Result<()> {
    // Subscribe to the NOTIFY channel.
    listener
        .listen(&config.notify_channel)
        .await
        .inspect_err(|e| {
            warn!(channel = %config.notify_channel, error = %e, "failed to LISTEN on channel");
        })?;

    listener_status.store(true, Ordering::Release);
    info!(channel = %config.notify_channel, "LISTEN connection established");

    // Recover the event cursor so we don't re-process already-scheduled events.
    let mut cursor = repo.recover_cursor().await?;
    debug!(cursor, "recovered event cursor from database");

    let poll_interval = config.poll_interval;
    let mut next_poll = Instant::now() + poll_interval;

    loop {
        tokio::select! {
            biased;

            // Shutdown takes highest priority.
            _ = shutdown.cancelled() => {
                info!("scheduler received shutdown signal, stopping");
                listener_status.store(false, Ordering::Release);
                return Ok(());
            }

            // LISTEN notification from Postgres — lowest latency wake.
            result = listener.recv() => {
                match result {
                    Ok(notification) => {
                        debug!(
                            channel = %notification.channel(),
                            payload = notification.payload(),
                            "received NOTIFY",
                        );
                        listener_status.store(true, Ordering::Release);
                    }
                    Err(e) => {
                        // sqlx PgListener reconnects automatically; we update the status
                        // gauge and let the poll-timer keep us functional.
                        warn!(error = %e, "LISTEN connection lost; will rely on poll-timer until reconnected");
                        listener_status.store(false, Ordering::Release);
                    }
                }
            }

            // Poll-timer fallback: fires at most every poll_interval.
            _ = sleep_until(next_poll) => {
                next_poll = Instant::now() + poll_interval;
                debug!("scheduler poll-timer fired");
            }
        }

        // Run a scheduling cycle regardless of which wake source fired.
        match schedule_new_deliveries(repo.as_ref(), &config, cursor).await {
            Ok(new_cursor) => {
                cursor = new_cursor;
            }
            Err(e) => {
                // Log and continue — a transient DB error should not crash the loop.
                error!(error = %e, "error during schedule_new_deliveries; will retry on next wake");
            }
        }
    }
}

// ── Scheduling ─────────────────────────────────────────────────────────────────

/// Fetch new events since `cursor`, expand their callbacks into delivery rows,
/// and return the updated cursor (the highest event id processed).
///
/// Each event is processed independently so one bad event does not block the rest.
/// Poison-pill rows (JSONB that fails deserialization) are counted, logged at `error`,
/// and skipped; the cursor still advances past them.
///
/// This function is intentionally `pub` so integration tests can drive it directly
/// without starting the full wake loop.
pub async fn schedule_new_deliveries(
    repo: &dyn Repo,
    config: &DispatchConfig,
    cursor: i64,
) -> Result<i64> {
    let new_events = repo
        .fetch_new_events(cursor, config.schedule_batch_size)
        .await?;
    if new_events.is_empty() {
        return Ok(cursor);
    }

    let mut new_cursor = cursor;

    for event in &new_events {
        // Always advance the cursor so a poison pill does not stall the loop.
        new_cursor = event.id;

        // ── Payload-size guard ────────────────────────────────────────────────
        // Events that exceed the limit are dead-lettered immediately for every
        // callback in the list. This is a documented carve-out from the normal
        // "dead_letter only via attempts >= max_attempts" rule (see §8.2 / §2.3).
        if event.payload_size_bytes > config.payload_size_limit_bytes {
            let reason =
                payload_too_large_error(event.payload_size_bytes, config.payload_size_limit_bytes);
            warn!(
                event_id = %event.event_id,
                payload_bytes = event.payload_size_bytes,
                limit_bytes = config.payload_size_limit_bytes,
                "payload too large — dead-lettering all callbacks"
            );

            // Extract callback names from the JSONB array for dead-lettering.
            // We don't fully parse here; a name or synthetic fallback is enough.
            let names = extract_callback_names(&event.callbacks);
            for name in names {
                if let Err(e) = repo
                    .create_invalid_delivery(event.event_id, &name, &reason)
                    .await
                {
                    error!(
                        event_id = %event.event_id,
                        callback_name = %name,
                        error = %e,
                        "failed to dead-letter oversized-payload callback"
                    );
                }
            }
            continue;
        }

        // ── Structural validation & callback expansion ────────────────────────
        // `parse_callbacks` returns a split of valid and invalid entries.
        // Invalid entries are dead-lettered immediately; valid entries are expanded
        // into delivery rows via `ensure_deliveries` (idempotent via UNIQUE constraint).
        let parsed = parse_callbacks(&event.callbacks, config);

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

        // Dead-letter structurally invalid callbacks.
        for (name, reason) in &parsed.invalid {
            if let Err(e) = repo
                .create_invalid_delivery(event.event_id, name, reason)
                .await
            {
                error!(
                    event_id = %event.event_id,
                    callback_name = %name,
                    error = %e,
                    "failed to dead-letter invalid callback"
                );
            }
        }
    }

    Ok(new_cursor)
}

// ── Helpers ────────────────────────────────────────────────────────────────────

/// Extract callback names from a JSONB array without full validation.
///
/// Used only in the payload-size rejection path where we need names to dead-letter
/// each callback without running full structural validation (which would be redundant
/// since we're rejecting everything unconditionally).
///
/// Falls back to a synthetic name when an element lacks a valid string `"name"` field.
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

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── extract_callback_names ────────────────────────────────────────────────

    #[test]
    fn extract_names_from_array_of_named_objects() {
        let callbacks = json!([
            {"name": "send_email", "url": "https://a.example.com/"},
            {"name": "push_notif", "url": "https://b.example.com/"}
        ]);
        let names = extract_callback_names(&callbacks);
        assert_eq!(names, vec!["send_email", "push_notif"]);
    }

    #[test]
    fn extract_names_falls_back_for_missing_name_field() {
        let callbacks = json!([
            {"url": "https://a.example.com/"},
            {"name": "push_notif"}
        ]);
        let names = extract_callback_names(&callbacks);
        assert_eq!(names, vec!["[0]", "push_notif"]);
    }

    #[test]
    fn extract_names_from_non_array_returns_unknown() {
        let callbacks = json!({"name": "not_array"});
        let names = extract_callback_names(&callbacks);
        assert_eq!(names, vec!["<unknown>"]);
    }

    #[test]
    fn extract_names_from_empty_array_returns_unknown() {
        let names = extract_callback_names(&json!([]));
        assert_eq!(names, vec!["<unknown>"]);
    }

    #[test]
    fn extract_names_non_string_name_field_falls_back() {
        let callbacks = json!([{"name": 42, "url": "https://a.example.com/"}]);
        let names = extract_callback_names(&callbacks);
        assert_eq!(names, vec!["[0]"]);
    }

    // ── schedule_new_deliveries (unit tests using MockRepo) ───────────────────
    //
    // These tests mock the Repo trait so they run without a database.

    use std::collections::VecDeque;
    use std::sync::Mutex;

    use crate::schema::{
        CallbackTarget, DeadLetterRow, DueDelivery, EventWithDeliveries, ExternalPendingRow,
        PageParams, RawEvent, SweepReport,
    };
    use async_trait::async_trait;
    use chrono::Utc;
    use std::time::Duration;
    use uuid::Uuid;

    /// A minimal mock Repo that records calls to `ensure_deliveries` and
    /// `create_invalid_delivery`, and returns pre-loaded events from a queue.
    struct MockRepo {
        events: Mutex<VecDeque<RawEvent>>,
        ensured: Mutex<Vec<(Uuid, Vec<String>)>>,
        invalids: Mutex<Vec<(Uuid, String, String)>>,
        cursor: Mutex<i64>,
    }

    impl MockRepo {
        fn new(events: Vec<RawEvent>, initial_cursor: i64) -> Self {
            Self {
                events: Mutex::new(events.into()),
                ensured: Mutex::new(vec![]),
                invalids: Mutex::new(vec![]),
                cursor: Mutex::new(initial_cursor),
            }
        }

        fn ensured_deliveries(&self) -> Vec<(Uuid, Vec<String>)> {
            self.ensured.lock().unwrap().clone()
        }

        fn invalid_deliveries(&self) -> Vec<(Uuid, String, String)> {
            self.invalids.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Repo for MockRepo {
        async fn fetch_new_events(
            &self,
            after_id: i64,
            _limit: i64,
        ) -> crate::error::Result<Vec<RawEvent>> {
            let events: Vec<RawEvent> = self
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| e.id > after_id)
                .cloned()
                .collect();
            Ok(events)
        }

        async fn ensure_deliveries(
            &self,
            event_id: Uuid,
            callbacks: &[CallbackTarget],
        ) -> crate::error::Result<()> {
            let names = callbacks.iter().map(|c| c.name.clone()).collect();
            self.ensured.lock().unwrap().push((event_id, names));
            Ok(())
        }

        async fn create_invalid_delivery(
            &self,
            event_id: Uuid,
            callback_name: &str,
            reason: &str,
        ) -> crate::error::Result<()> {
            self.invalids.lock().unwrap().push((
                event_id,
                callback_name.to_string(),
                reason.to_string(),
            ));
            Ok(())
        }

        async fn recover_cursor(&self) -> crate::error::Result<i64> {
            Ok(*self.cursor.lock().unwrap())
        }

        // ── unused in scheduler tests ─────────────────────────────────────────

        async fn fetch_due_deliveries(
            &self,
            _batch_size: i64,
        ) -> crate::error::Result<Vec<DueDelivery>> {
            Ok(vec![])
        }

        async fn lock_delivery(
            &self,
            _id: i64,
            _until: chrono::DateTime<Utc>,
        ) -> crate::error::Result<bool> {
            Ok(false)
        }

        async fn mark_dispatched_managed(&self, _id: i64) -> crate::error::Result<()> {
            Ok(())
        }

        async fn mark_dispatched_external(&self, _id: i64) -> crate::error::Result<()> {
            Ok(())
        }

        async fn mark_failure(
            &self,
            _id: i64,
            _error: &str,
            _available_at: chrono::DateTime<Utc>,
            _dead_letter: bool,
        ) -> crate::error::Result<()> {
            Ok(())
        }

        async fn reset_hung_external(
            &self,
            _now: chrono::DateTime<Utc>,
            _max_completion_cycles: i32,
        ) -> crate::error::Result<SweepReport> {
            Ok(SweepReport::default())
        }

        async fn list_dead_letters(
            &self,
            _page: PageParams,
        ) -> crate::error::Result<Vec<DeadLetterRow>> {
            Ok(vec![])
        }

        async fn list_external_pending(
            &self,
            _page: PageParams,
            _older_than: Option<Duration>,
        ) -> crate::error::Result<Vec<ExternalPendingRow>> {
            Ok(vec![])
        }

        async fn retry_delivery(&self, _id: i64) -> crate::error::Result<bool> {
            Ok(false)
        }

        async fn complete_delivery(&self, _id: i64) -> crate::error::Result<bool> {
            Ok(false)
        }

        async fn abandon_delivery(&self, _id: i64) -> crate::error::Result<bool> {
            Ok(false)
        }

        async fn fetch_event_with_deliveries(
            &self,
            event_id: Uuid,
        ) -> crate::error::Result<EventWithDeliveries> {
            Err(crate::error::Error::InvalidData(format!(
                "event {event_id} not found"
            )))
        }
    }

    fn make_event(id: i64, callbacks: serde_json::Value, payload_bytes: i64) -> RawEvent {
        RawEvent {
            id,
            event_id: Uuid::new_v4(),
            kind: "test.event@v1".to_string(),
            aggregate_type: "test".to_string(),
            aggregate_id: Uuid::new_v4(),
            payload: json!({"test": "data"}),
            metadata: json!({}),
            callbacks,
            actor_id: None,
            correlation_id: None,
            causation_id: None,
            created_at: Utc::now(),
            payload_size_bytes: payload_bytes,
        }
    }

    fn config() -> DispatchConfig {
        DispatchConfig {
            allow_unsigned_callbacks: true,
            allow_private_ip_targets: true,
            ..Default::default()
        }
    }

    // ── Happy-path: valid callbacks are expanded into delivery rows ───────────

    #[tokio::test]
    async fn valid_callbacks_are_scheduled() {
        let event_id = Uuid::new_v4();
        let event = RawEvent {
            id: 1,
            event_id,
            kind: "test@v1".to_string(),
            aggregate_type: "order".to_string(),
            aggregate_id: Uuid::new_v4(),
            payload: json!({}),
            metadata: json!({}),
            callbacks: json!([
                {"name": "notify", "url": "https://example.com/hook"},
                {"name": "log_cb", "url": "https://log.example.com/hook"}
            ]),
            actor_id: None,
            correlation_id: None,
            causation_id: None,
            created_at: Utc::now(),
            payload_size_bytes: 10,
        };

        let repo = MockRepo::new(vec![event], 0);
        let new_cursor = schedule_new_deliveries(&repo, &config(), 0).await.unwrap();

        assert_eq!(new_cursor, 1);
        let ensured = repo.ensured_deliveries();
        assert_eq!(ensured.len(), 1);
        assert_eq!(ensured[0].0, event_id);
        let mut names = ensured[0].1.clone();
        names.sort();
        assert_eq!(names, vec!["log_cb", "notify"]);
        assert!(repo.invalid_deliveries().is_empty());
    }

    // ── Cursor advances correctly ─────────────────────────────────────────────

    #[tokio::test]
    async fn cursor_advances_to_last_event_id() {
        let events = vec![
            make_event(
                10,
                json!([{"name": "abc", "url": "https://a.example.com/"}]),
                10,
            ),
            make_event(
                20,
                json!([{"name": "def", "url": "https://b.example.com/"}]),
                10,
            ),
        ];
        let repo = MockRepo::new(events, 0);
        let new_cursor = schedule_new_deliveries(&repo, &config(), 0).await.unwrap();
        assert_eq!(new_cursor, 20);
    }

    #[tokio::test]
    async fn no_new_events_returns_same_cursor() {
        let repo = MockRepo::new(vec![], 5);
        let new_cursor = schedule_new_deliveries(&repo, &config(), 5).await.unwrap();
        assert_eq!(new_cursor, 5);
    }

    // ── Payload-size guard ────────────────────────────────────────────────────

    #[tokio::test]
    async fn oversized_payload_dead_letters_all_callbacks() {
        let event = make_event(
            1,
            json!([
                {"name": "notify", "url": "https://a.example.com/"},
                {"name": "log_cb", "url": "https://b.example.com/"}
            ]),
            2_000_000, // 2 MB > 1 MB limit
        );
        let event_id = event.event_id;

        let repo = MockRepo::new(vec![event], 0);
        let new_cursor = schedule_new_deliveries(&repo, &config(), 0).await.unwrap();

        // Cursor still advances past the oversized event.
        assert_eq!(new_cursor, 1);

        // No valid deliveries created.
        assert!(repo.ensured_deliveries().is_empty());

        // Both callbacks dead-lettered with the source_payload_too_large prefix.
        let invalids = repo.invalid_deliveries();
        assert_eq!(invalids.len(), 2, "expected 2 dead-lettered callbacks");
        for (eid, _name, reason) in &invalids {
            assert_eq!(*eid, event_id);
            assert!(
                reason.starts_with("source_payload_too_large:"),
                "got: {reason}"
            );
        }
        let mut names: Vec<_> = invalids.iter().map(|(_, n, _)| n.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["log_cb", "notify"]);
    }

    // ── Invalid callbacks dead-lettered at schedule time ─────────────────────

    #[tokio::test]
    async fn invalid_callback_is_dead_lettered() {
        let cfg = DispatchConfig::default(); // allow_unsigned_callbacks = false
        let event_id = Uuid::new_v4();
        let event = RawEvent {
            id: 1,
            event_id,
            kind: "test@v1".to_string(),
            aggregate_type: "order".to_string(),
            aggregate_id: Uuid::new_v4(),
            payload: json!({}),
            metadata: json!({}),
            // Missing required signing_key_id — structurally invalid with default config.
            callbacks: json!([{"name": "bad", "url": "https://a.example.com/"}]),
            actor_id: None,
            correlation_id: None,
            causation_id: None,
            created_at: Utc::now(),
            payload_size_bytes: 10,
        };

        let repo = MockRepo::new(vec![event], 0);
        let new_cursor = schedule_new_deliveries(&repo, &cfg, 0).await.unwrap();

        assert_eq!(new_cursor, 1);
        assert!(repo.ensured_deliveries().is_empty());
        let invalids = repo.invalid_deliveries();
        assert_eq!(invalids.len(), 1);
        assert_eq!(invalids[0].0, event_id);
        assert_eq!(invalids[0].1, "bad");
        assert!(invalids[0].2.starts_with("invalid_callback:"));
    }

    // ── Mixed valid and invalid callbacks in the same event ───────────────────

    #[tokio::test]
    async fn mixed_callbacks_split_correctly() {
        let event_id = Uuid::new_v4();
        let event = RawEvent {
            id: 1,
            event_id,
            kind: "test@v1".to_string(),
            aggregate_type: "order".to_string(),
            aggregate_id: Uuid::new_v4(),
            payload: json!({}),
            metadata: json!({}),
            callbacks: json!([
                {"name": "good", "url": "https://example.com/hook"},
                {"name": "1bad", "url": "https://example.com/other"}  // invalid name
            ]),
            actor_id: None,
            correlation_id: None,
            causation_id: None,
            created_at: Utc::now(),
            payload_size_bytes: 10,
        };

        let repo = MockRepo::new(vec![event], 0);
        let new_cursor = schedule_new_deliveries(&repo, &config(), 0).await.unwrap();

        assert_eq!(new_cursor, 1);
        let ensured = repo.ensured_deliveries();
        assert_eq!(ensured.len(), 1);
        assert_eq!(ensured[0].1, vec!["good"]);
        let invalids = repo.invalid_deliveries();
        assert_eq!(invalids.len(), 1);
        assert!(invalids[0].2.starts_with("invalid_callback:"));
    }

    // ── Poison-pill: non-array JSONB is skipped without crashing ─────────────

    #[tokio::test]
    async fn non_array_callbacks_is_poison_pill_skipped() {
        // `parse_callbacks` returns only a "<root>" invalid entry for non-array JSONB.
        // The scheduler must skip it as a poison pill without dead-lettering under
        // the event_id, since we have no callback names to associate.
        let event = make_event(1, json!({"not": "an_array"}), 10);

        let repo = MockRepo::new(vec![event], 0);
        let new_cursor = schedule_new_deliveries(&repo, &config(), 0).await.unwrap();

        // Cursor still advances.
        assert_eq!(new_cursor, 1);
        // No deliveries created — poison pill skipped.
        assert!(repo.ensured_deliveries().is_empty());
        assert!(repo.invalid_deliveries().is_empty());
    }

    // ── Cursor does not go backwards ─────────────────────────────────────────

    #[tokio::test]
    async fn events_before_cursor_are_not_returned() {
        let events = vec![
            make_event(
                3,
                json!([{"name": "abc", "url": "https://a.example.com/"}]),
                10,
            ),
            make_event(
                7,
                json!([{"name": "def", "url": "https://b.example.com/"}]),
                10,
            ),
        ];
        let repo = MockRepo::new(events, 0);

        // Call with cursor=5 — only event id=7 should be returned.
        let new_cursor = schedule_new_deliveries(&repo, &config(), 5).await.unwrap();
        assert_eq!(new_cursor, 7);
        let ensured = repo.ensured_deliveries();
        assert_eq!(ensured.len(), 1);
        assert_eq!(ensured[0].1, vec!["def"]);
    }
}
